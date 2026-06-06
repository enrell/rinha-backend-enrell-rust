// Runtime exact k-NN search over the serialized v3 index.

use crate::index::*;

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

const K: usize = 5;

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn dist_sq(a: *const i16, b: *const i16) -> i32 {
    unsafe {
        let va = _mm256_loadu_si256(a as *const __m256i);
        let vb = _mm256_loadu_si256(b as *const __m256i);
        let diff = _mm256_sub_epi16(va, vb);
        let squared = _mm256_madd_epi16(diff, diff);
        let hi = _mm256_extracti128_si256(squared, 1);
        let lo = _mm256_castsi256_si128(squared);
        let sum4 = _mm_add_epi32(lo, hi);
        let sum2 = _mm_add_epi32(sum4, _mm_srli_si128(sum4, 8));
        let sum1 = _mm_add_epi32(sum2, _mm_srli_si128(sum2, 4));
        _mm_cvtsi128_si32(sum1)
    }
}

#[cfg(not(target_arch = "x86_64"))]
unsafe fn dist_sq(a: *const i16, b: *const i16) -> i32 {
    let mut sum = 0i32;
    let mut i = 0;
    while i < STORE_DIM {
        let d = unsafe { *a.add(i) as i32 - *b.add(i) as i32 };
        sum += d * d;
        i += 1;
    }
    sum
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn lower_bound(q: *const i16, bmin: *const i16, bmax: *const i16) -> i32 {
    unsafe {
        let vq = _mm256_loadu_si256(q as *const __m256i);
        let vmin = _mm256_loadu_si256(bmin as *const __m256i);
        let vmax = _mm256_loadu_si256(bmax as *const __m256i);
        let below = _mm256_max_epi16(_mm256_subs_epi16(vmin, vq), _mm256_setzero_si256());
        let above = _mm256_max_epi16(_mm256_subs_epi16(vq, vmax), _mm256_setzero_si256());
        let diff = _mm256_or_si256(below, above);
        let squared = _mm256_madd_epi16(diff, diff);
        let hi = _mm256_extracti128_si256(squared, 1);
        let lo = _mm256_castsi256_si128(squared);
        let sum4 = _mm_add_epi32(lo, hi);
        let sum2 = _mm_add_epi32(sum4, _mm_srli_si128(sum4, 8));
        let sum1 = _mm_add_epi32(sum2, _mm_srli_si128(sum2, 4));
        _mm_cvtsi128_si32(sum1)
    }
}

#[cfg(not(target_arch = "x86_64"))]
unsafe fn lower_bound(q: *const i16, bmin: *const i16, bmax: *const i16) -> i32 {
    let mut sum = 0i32;
    let mut i = 0;
    while i < STORE_DIM {
        let qv = unsafe { *q.add(i) as i32 };
        let lo = unsafe { *bmin.add(i) as i32 };
        let hi = unsafe { *bmax.add(i) as i32 };
        if qv < lo {
            let d = lo - qv;
            sum += d * d;
        } else if qv > hi {
            let d = qv - hi;
            sum += d * d;
        }
        i += 1;
    }
    sum
}

struct Top5 {
    dists: [i32; K],
    fraud: [bool; K],
}

impl Top5 {
    fn new() -> Self {
        Self {
            dists: [i32::MAX; K],
            fraud: [false; K],
        }
    }

    #[inline(always)]
    fn worst(&self) -> i32 {
        self.dists[K - 1]
    }

    #[inline(always)]
    fn try_insert(&mut self, dist: i32, is_fraud: bool) {
        if dist >= self.dists[K - 1] {
            return;
        }
        let mut i = K - 1;
        while i > 0 && self.dists[i - 1] > dist {
            self.dists[i] = self.dists[i - 1];
            self.fraud[i] = self.fraud[i - 1];
            i -= 1;
        }
        self.dists[i] = dist;
        self.fraud[i] = is_fraud;
    }

    fn fraud_count(&self) -> u32 {
        let mut c = 0;
        let mut i = 0;
        while i < K {
            if self.fraud[i] {
                c += 1;
            }
            i += 1;
        }
        c
    }
}

#[derive(Default)]
pub struct SearchStats {
    pub partition_key: u8,
    pub partitions_considered: u32,
    pub partitions_searched: u32,
    pub partitions_pruned: u32,
    pub empty_partitions: u32,
    pub nodes_visited: u32,
    pub nodes_pruned: u32,
    pub leaves_scanned: u32,
    pub vectors_scanned: u32,
}

pub struct Index {
    pub vectors: *const i16,
    pub labels: *const u8,
    pub count: usize,
    partitions: [PartitionMeta; PARTITIONS],
    nodes: Box<[KdNode]>,
}

impl Index {
    pub fn load(data_dir: &str) -> Self {
        use std::path::Path;

        let vpath = Path::new(data_dir).join("vectors.bin");
        let lpath = Path::new(data_dir).join("labels.bin");
        let ppath = Path::new(data_dir).join("partitions.bin");
        let npath = Path::new(data_dir).join("nodes.bin");

        let (vptr_u8, vlen) = load_data_file(&vpath);
        let (lptr, llen) = load_data_file(&lpath);
        let count = llen;
        assert_eq!(
            vlen,
            count * STORE_DIM * 2,
            "vectors.bin size mismatch: expected {} bytes for {} vectors, got {}",
            count * STORE_DIM * 2,
            count,
            vlen
        );

        let (partitions, nodes) = if ppath.exists() && npath.exists() {
            let pdata = std::fs::read(&ppath).expect("read partitions.bin");
            let ndata = std::fs::read(&npath).expect("read nodes.bin");
            parse_partition_files(&pdata, &ndata, count)
        } else {
            let mut p = [PartitionMeta::empty(); PARTITIONS];
            let bbox = bbox_from_ptr(vptr_u8, count);
            p[0] = PartitionMeta {
                start: 0,
                count: count as u32,
                root: -1,
                bbox_min: bbox.0,
                bbox_max: bbox.1,
            };
            (p, Vec::new().into_boxed_slice())
        };

        Self {
            vectors: vptr_u8 as *const i16,
            labels: lptr,
            count,
            partitions,
            nodes,
        }
    }

    pub fn search_exact(&self, query: &QVec) -> u32 {
        self.search_exact_inner(query, std::ptr::null_mut())
    }

    pub fn search_exact_with_stats(&self, query: &QVec, stats: &mut SearchStats) -> u32 {
        *stats = SearchStats::default();
        self.search_exact_inner(query, stats as *mut SearchStats)
    }

    fn search_exact_inner(&self, query: &QVec, stats: *mut SearchStats) -> u32 {
        let mut top5 = Top5::new();
        let qptr = query.as_ptr();

        if !self.nodes.is_empty() {
            self.search_partitioned(query, qptr, &mut top5, stats);
            return top5.fraud_count();
        }

        self.scan_range(0, self.count, qptr, &mut top5, stats);
        top5.fraud_count()
    }

    fn search_partitioned(&self, query: &QVec, qptr: *const i16, top5: &mut Top5, stats: *mut SearchStats) {
        let key = partition_key(query) as usize;
        stats_set_key(stats, key as u8);
        stats_partition_considered(stats);
        self.search_one_partition(&self.partitions[key], qptr, top5, stats);

        let mut k = 0;
        while k < PARTITIONS {
            if k != key {
                let p = &self.partitions[k];
                if p.count != 0 {
                    stats_partition_considered(stats);
                    let lb = unsafe { lower_bound(qptr, p.bbox_min.as_ptr(), p.bbox_max.as_ptr()) };
                    if lb < top5.worst() {
                        self.search_one_partition(p, qptr, top5, stats);
                    } else {
                        stats_partition_pruned(stats);
                    }
                } else {
                    stats_empty_partition(stats);
                }
            }
            k += 1;
        }
    }

    fn search_one_partition(&self, part: &PartitionMeta, qptr: *const i16, top5: &mut Top5, stats: *mut SearchStats) {
        if part.count == 0 {
            return;
        }
        stats_partition_searched(stats);
        if part.root < 0 {
            self.scan_range(part.start as usize, part.count as usize, qptr, top5, stats);
            return;
        }

        let mut stack = [0u32; 128];
        let mut sp = 1usize;
        stack[0] = part.root as u32;

        while sp != 0 {
            sp -= 1;
            let node = &self.nodes[stack[sp] as usize];
            stats_node_visited(stats);
            let lb = unsafe { lower_bound(qptr, node.bbox_min.as_ptr(), node.bbox_max.as_ptr()) };
            if lb >= top5.worst() {
                stats_node_pruned(stats);
                continue;
            }
            if node.left < 0 {
                stats_leaf_scanned(stats);
                self.scan_range(node.start as usize, node.count as usize, qptr, top5, stats);
                continue;
            }

            let left = node.left as usize;
            let right = node.right as usize;
            let left_lb = self.node_lower_bound(qptr, left);
            let right_lb = self.node_lower_bound(qptr, right);

            if left_lb < right_lb {
                push_if_promising(&mut stack, &mut sp, right as u32, right_lb, top5.worst());
                push_if_promising(&mut stack, &mut sp, left as u32, left_lb, top5.worst());
            } else {
                push_if_promising(&mut stack, &mut sp, left as u32, left_lb, top5.worst());
                push_if_promising(&mut stack, &mut sp, right as u32, right_lb, top5.worst());
            }
        }
    }

    #[inline(always)]
    fn node_lower_bound(&self, qptr: *const i16, idx: usize) -> i32 {
        let n = &self.nodes[idx];
        unsafe { lower_bound(qptr, n.bbox_min.as_ptr(), n.bbox_max.as_ptr()) }
    }

    #[inline(always)]
    fn scan_range(&self, start: usize, count: usize, qptr: *const i16, top5: &mut Top5, stats: *mut SearchStats) {
        stats_vectors_scanned(stats, count as u32);
        let end = start + count;
        let mut i = start;
        while i < end {
            let vptr = unsafe { self.vectors.add(i * STORE_DIM) };
            let d = unsafe { dist_sq(qptr, vptr) };
            if d < top5.worst() {
                let is_fraud = unsafe { *self.labels.add(i) } != 0;
                top5.try_insert(d, is_fraud);
            }
            i += 1;
        }
    }
}

#[inline(always)]
fn stats_set_key(stats: *mut SearchStats, key: u8) {
    if !stats.is_null() {
        unsafe { (*stats).partition_key = key };
    }
}

#[inline(always)]
fn stats_partition_considered(stats: *mut SearchStats) {
    if !stats.is_null() {
        unsafe { (*stats).partitions_considered += 1 };
    }
}

#[inline(always)]
fn stats_partition_searched(stats: *mut SearchStats) {
    if !stats.is_null() {
        unsafe { (*stats).partitions_searched += 1 };
    }
}

#[inline(always)]
fn stats_partition_pruned(stats: *mut SearchStats) {
    if !stats.is_null() {
        unsafe { (*stats).partitions_pruned += 1 };
    }
}

#[inline(always)]
fn stats_empty_partition(stats: *mut SearchStats) {
    if !stats.is_null() {
        unsafe { (*stats).empty_partitions += 1 };
    }
}

#[inline(always)]
fn stats_node_visited(stats: *mut SearchStats) {
    if !stats.is_null() {
        unsafe { (*stats).nodes_visited += 1 };
    }
}

#[inline(always)]
fn stats_node_pruned(stats: *mut SearchStats) {
    if !stats.is_null() {
        unsafe { (*stats).nodes_pruned += 1 };
    }
}

#[inline(always)]
fn stats_leaf_scanned(stats: *mut SearchStats) {
    if !stats.is_null() {
        unsafe { (*stats).leaves_scanned += 1 };
    }
}

#[inline(always)]
fn stats_vectors_scanned(stats: *mut SearchStats, count: u32) {
    if !stats.is_null() {
        unsafe { (*stats).vectors_scanned += count };
    }
}

fn push_if_promising(stack: &mut [u32; 128], sp: &mut usize, idx: u32, lb: i32, worst: i32) {
    if lb < worst && *sp < stack.len() {
        stack[*sp] = idx;
        *sp += 1;
    }
}

fn load_data_file(path: &std::path::Path) -> (*const u8, usize) {
    match mmap_file(path) {
        Some((ptr, len)) => {
            warmup_pages(ptr, len);
            (ptr, len)
        }
        None => {
            let data = std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));
            let len = data.len();
            let ptr = data.leak().as_ptr();
            warmup_pages(ptr, len);
            (ptr, len)
        }
    }
}

#[cfg(target_os = "linux")]
fn mmap_file(path: &std::path::Path) -> Option<(*const u8, usize)> {
    use std::ffi::c_void;
    use std::fs::File;
    use std::os::fd::AsRawFd;

    const PROT_READ: i32 = 0x1;
    const MAP_PRIVATE: i32 = 0x02;
    const MAP_POPULATE: i32 = 0x8000;
    const MAP_HUGETLB: i32 = 0x40000;
    const MAP_FAILED: *mut c_void = !0usize as *mut c_void;

    unsafe extern "C" {
        fn mmap(addr: *mut c_void, len: usize, prot: i32, flags: i32, fd: i32, offset: isize) -> *mut c_void;
        fn mlock(addr: *const c_void, len: usize) -> i32;
    }

    let file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len() as usize;
    if len == 0 {
        return None;
    }

    let base_flags = MAP_PRIVATE | MAP_POPULATE;
    let wants_huge = env_true("INDEX_HUGE");
    let mut ptr = MAP_FAILED;
    if wants_huge {
        ptr = unsafe { mmap(std::ptr::null_mut(), len, PROT_READ, base_flags | MAP_HUGETLB, file.as_raw_fd(), 0) };
    }
    if ptr == MAP_FAILED {
        ptr = unsafe { mmap(std::ptr::null_mut(), len, PROT_READ, base_flags, file.as_raw_fd(), 0) };
    }
    if ptr == MAP_FAILED {
        return None;
    }
    if env_true("INDEX_MLOCK") {
        let _ = unsafe { mlock(ptr.cast_const(), len) };
    }
    Some((ptr as *const u8, len))
}

#[cfg(not(target_os = "linux"))]
fn mmap_file(_path: &std::path::Path) -> Option<(*const u8, usize)> {
    None
}

fn env_true(name: &str) -> bool {
    match std::env::var(name) {
        Ok(v) => v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes"),
        Err(_) => false,
    }
}

fn warmup_pages(ptr: *const u8, len: usize) {
    let mut off = 0;
    let mut acc = 0u8;
    while off < len {
        acc ^= unsafe { std::ptr::read_volatile(ptr.add(off)) };
        off += 4096;
    }
    if len != 0 {
        acc ^= unsafe { std::ptr::read_volatile(ptr.add(len - 1)) };
    }
    std::hint::black_box(acc);
}

fn parse_partition_files(pdata: &[u8], ndata: &[u8], count: usize) -> ([PartitionMeta; PARTITIONS], Box<[KdNode]>) {
    let mut pos = 0usize;
    assert!(pdata.len() >= 24, "partitions.bin too small");
    assert_eq!(&pdata[..8], PARTITIONS_MAGIC, "bad partitions.bin magic");
    pos += 8;
    assert_eq!(read_u32(pdata, &mut pos), INDEX_VERSION, "unsupported index version");
    assert_eq!(read_u64(pdata, &mut pos) as usize, count, "index count mismatch");
    let stored_nodes = read_u64(pdata, &mut pos) as usize;

    let mut partitions = [PartitionMeta::empty(); PARTITIONS];
    let mut total = 0usize;
    let mut i = 0;
    while i < PARTITIONS {
        let start = read_u32(pdata, &mut pos);
        let part_count = read_u32(pdata, &mut pos);
        let root = read_i32(pdata, &mut pos);
        let _nodes_count = read_u32(pdata, &mut pos);
        let bbox_min = read_qvec(pdata, &mut pos);
        let bbox_max = read_qvec(pdata, &mut pos);
        partitions[i] = PartitionMeta {
            start,
            count: part_count,
            root,
            bbox_min,
            bbox_max,
        };
        total += part_count as usize;
        i += 1;
    }
    assert_eq!(total, count, "partition count mismatch");

    let node_size = STORE_DIM * 2 * 2 + 16;
    assert_eq!(ndata.len(), stored_nodes * node_size, "nodes.bin size mismatch");
    let mut nodes = Vec::with_capacity(stored_nodes);
    let mut npos = 0usize;
    while npos < ndata.len() {
        nodes.push(KdNode {
            bbox_min: read_qvec(ndata, &mut npos),
            bbox_max: read_qvec(ndata, &mut npos),
            left: read_i32(ndata, &mut npos),
            right: read_i32(ndata, &mut npos),
            start: read_u32(ndata, &mut npos),
            count: read_u32(ndata, &mut npos),
        });
    }
    (partitions, nodes.into_boxed_slice())
}

fn bbox_from_ptr(vdata: *const u8, count: usize) -> (QVec, QVec) {
    if count == 0 {
        return ([0; STORE_DIM], [0; STORE_DIM]);
    }
    let mut min = [i16::MAX; STORE_DIM];
    let mut max = [i16::MIN; STORE_DIM];
    let mut i = 0;
    while i < count {
        let base = i * STORE_DIM * 2;
        let mut d = 0;
        while d < STORE_DIM {
            let p = base + d * 2;
            let v = unsafe { i16::from_le_bytes([*vdata.add(p), *vdata.add(p + 1)]) };
            if v < min[d] {
                min[d] = v;
            }
            if v > max[d] {
                max[d] = v;
            }
            d += 1;
        }
        i += 1;
    }
    (min, max)
}

fn read_u32(data: &[u8], pos: &mut usize) -> u32 {
    let v = u32::from_le_bytes(data[*pos..*pos + 4].try_into().unwrap());
    *pos += 4;
    v
}

fn read_i32(data: &[u8], pos: &mut usize) -> i32 {
    read_u32(data, pos) as i32
}

fn read_u64(data: &[u8], pos: &mut usize) -> u64 {
    let v = u64::from_le_bytes(data[*pos..*pos + 8].try_into().unwrap());
    *pos += 8;
    v
}

fn read_qvec(data: &[u8], pos: &mut usize) -> QVec {
    let mut v = [0i16; STORE_DIM];
    let mut i = 0;
    while i < STORE_DIM {
        v[i] = i16::from_le_bytes([data[*pos], data[*pos + 1]]);
        *pos += 2;
        i += 1;
    }
    v
}

unsafe impl Send for Index {}
unsafe impl Sync for Index {}
