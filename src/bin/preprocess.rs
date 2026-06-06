// preprocess.rs - Build-time index builder
//
// Reads the uncompressed references.json (3M labeled vectors), quantizes each
// vector directly to i16 fixed-point, partitions it, builds exact KD/BBox trees,
// and writes index.bin (one-file runtime index with hot4/mid4/cold8 stages).
// Set WRITE_LEGACY_INDEX=1 to also emit vectors/labels/partitions/nodes bins.
//
// Usage:
//   preprocess [input_json] [output_dir]
//
// Defaults:
//   input_json = /app/resources/references.json
//   output_dir = /app/data

#[allow(dead_code)]
#[path = "../index.rs"]
mod index;

use index::*;
use std::fs;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

const LEAF_SIZE: usize = 64;
const FIXED_SCALE: i32 = 10_000;
const SPLIT_SAMPLE: usize = 512;
const SECTION_WRITE_BUF: usize = 1024 * 1024;

#[derive(Clone, Copy)]
struct BuildRecord {
    qvec: [i16; STORE_DIM],
    label: u8,
    key: u8,
}

const _: () = assert!(std::mem::size_of::<BuildRecord>() <= STORE_DIM * 2 + 4);

// ── Hand-rolled streaming JSON parser ────────────────────────────────────────
//
// The file is a JSON array of objects:
//   [{"vector":[0.01,...,-1,...,0.0416],"label":"legit"}, ...]
//
// Strategy: read into a large buffer, scan byte-by-byte. When the buffer runs
// low, shift unconsumed data to the front and refill. This keeps peak memory
// around BUF_SIZE + output vectors.

const BUF_SIZE: usize = 4 * 1024 * 1024; // 4 MB read buffer

struct StreamParser<R: Read> {
    reader: R,
    buf: Vec<u8>,
    pos: usize,
    len: usize,
    eof: bool,
}

impl<R: Read> StreamParser<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            buf: vec![0u8; BUF_SIZE],
            pos: 0,
            len: 0,
            eof: false,
        }
    }

    /// Ensure at least `need` bytes are available in buf[pos..len].
    /// Shifts and refills if necessary. Returns false only at true EOF.
    fn ensure(&mut self, need: usize) -> bool {
        if self.pos + need <= self.len {
            return true;
        }
        if self.eof {
            return self.pos < self.len;
        }
        // Shift unconsumed data to front
        let remaining = self.len - self.pos;
        if remaining > 0 {
            self.buf.copy_within(self.pos..self.len, 0);
        }
        self.pos = 0;
        self.len = remaining;
        // Read more data
        while self.len < self.buf.len() {
            match self.reader.read(&mut self.buf[self.len..]) {
                Ok(0) => {
                    self.eof = true;
                    break;
                }
                Ok(n) => self.len += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => panic!("read error: {}", e),
            }
        }
        self.pos + need <= self.len || self.pos < self.len
    }

    #[inline(always)]
    fn peek(&self) -> u8 {
        self.buf[self.pos]
    }

    #[inline(always)]
    fn advance(&mut self) {
        self.pos += 1;
    }

    #[inline(always)]
    fn has_data(&mut self) -> bool {
        self.ensure(1)
    }

    /// Skip whitespace characters.
    fn skip_ws(&mut self) {
        while self.has_data() {
            let b = self.peek();
            if b == b' ' || b == b'\n' || b == b'\r' || b == b'\t' {
                self.advance();
            } else {
                break;
            }
        }
    }

    /// Expect and consume a specific byte.
    fn expect(&mut self, expected: u8) {
        self.skip_ws();
        assert!(
            self.has_data(),
            "unexpected EOF, expected '{}'",
            expected as char
        );
        let b = self.peek();
        assert_eq!(
            b, expected,
            "expected '{}', got '{}' at buffer pos {}",
            expected as char, b as char, self.pos
        );
        self.advance();
    }

    /// Parse a JSON number as f64. Handles: -1, 0, 1, 0.0833, -0.5, etc.
    fn parse_number(&mut self) -> f64 {
        self.skip_ws();
        // Collect number bytes into a small stack buffer
        let mut num_buf = [0u8; 32];
        let mut n = 0usize;

        // Ensure we have some data
        self.ensure(32);

        while self.pos < self.len && n < 32 {
            let b = self.buf[self.pos];
            match b {
                b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E' => {
                    num_buf[n] = b;
                    n += 1;
                    self.pos += 1;
                }
                _ => break,
            }
        }

        // Parse the collected bytes as f64
        let s = unsafe { std::str::from_utf8_unchecked(&num_buf[..n]) };
        s.parse::<f64>().unwrap_or_else(|_| panic!("bad number: '{}'", s))
    }

    /// Parse a JSON number known to have at most 4 decimal places directly as i16 fixed-point.
    fn parse_fixed4_i16(&mut self) -> i16 {
        self.skip_ws();
        assert!(self.has_data(), "unexpected EOF while parsing fixed-point number");

        let mut sign = 1i32;
        if self.peek() == b'-' {
            sign = -1;
            self.advance();
        } else if self.peek() == b'+' {
            self.advance();
        }

        let mut int_part = 0i32;
        let mut saw_digit = false;
        while self.has_data() {
            let b = self.peek();
            if !b.is_ascii_digit() {
                break;
            }
            saw_digit = true;
            int_part = int_part * 10 + (b - b'0') as i32;
            self.advance();
        }

        let mut frac = 0i32;
        let mut digits = 0usize;
        if self.has_data() && self.peek() == b'.' {
            self.advance();
            while self.has_data() {
                let b = self.peek();
                if !b.is_ascii_digit() {
                    break;
                }
                saw_digit = true;
                assert!(digits < 4, "fixed-point number has more than 4 decimal places");
                frac = frac * 10 + (b - b'0') as i32;
                digits += 1;
                self.advance();
            }
        }

        assert!(saw_digit, "expected digit while parsing fixed-point number");
        while digits < 4 {
            frac *= 10;
            digits += 1;
        }

        let fixed = sign * (int_part * FIXED_SCALE + frac);
        assert!(
            (i16::MIN as i32..=i16::MAX as i32).contains(&fixed),
            "fixed-point value out of i16 range: {}",
            fixed
        );
        fixed as i16
    }

    /// Parse a JSON string (without the surrounding quotes). Returns bytes.
    /// Only handles simple unescaped strings (sufficient for "fraud"/"legit").
    fn parse_string_into(&mut self, out: &mut [u8; 8]) -> usize {
        self.skip_ws();
        self.expect(b'"');
        let mut n = 0usize;
        while self.has_data() {
            let b = self.peek();
            self.advance();
            if b == b'"' {
                return n;
            }
            if n < 8 {
                out[n] = b;
            }
            n += 1;
        }
        panic!("unterminated string");
    }

    /// Skip a JSON string value (with quotes).
    fn skip_string(&mut self) {
        self.skip_ws();
        self.expect(b'"');
        while self.has_data() {
            let b = self.peek();
            self.advance();
            if b == b'"' {
                return;
            }
            if b == b'\\' {
                // Skip escaped char
                if self.has_data() {
                    self.advance();
                }
            }
        }
        panic!("unterminated string");
    }

    /// Skip any JSON value (number, string, array, object, bool, null).
    fn skip_value(&mut self) {
        self.skip_ws();
        if !self.has_data() {
            return;
        }
        match self.peek() {
            b'"' => self.skip_string(),
            b'[' => {
                self.advance();
                self.skip_ws();
                if self.has_data() && self.peek() == b']' {
                    self.advance();
                    return;
                }
                loop {
                    self.skip_value();
                    self.skip_ws();
                    if !self.has_data() {
                        break;
                    }
                    if self.peek() == b']' {
                        self.advance();
                        return;
                    }
                    self.expect(b',');
                }
            }
            b'{' => {
                self.advance();
                self.skip_ws();
                if self.has_data() && self.peek() == b'}' {
                    self.advance();
                    return;
                }
                loop {
                    self.skip_string();
                    self.expect(b':');
                    self.skip_value();
                    self.skip_ws();
                    if !self.has_data() {
                        break;
                    }
                    if self.peek() == b'}' {
                        self.advance();
                        return;
                    }
                    self.expect(b',');
                }
            }
            b't' => {
                // true
                for _ in 0..4 {
                    self.advance();
                }
            }
            b'f' => {
                // false
                for _ in 0..5 {
                    self.advance();
                }
            }
            b'n' => {
                // null
                for _ in 0..4 {
                    self.advance();
                }
            }
            _ => {
                // number
                let _ = self.parse_number();
            }
        }
    }
}

// ── Entry parsing ────────────────────────────────────────────────────────────

/// Parse one reference object: {"vector":[...14 floats...],"label":"fraud"|"legit"}
/// Fields may appear in any order.
fn parse_entry<R: Read>(parser: &mut StreamParser<R>) -> (QVec, bool) {
    parser.expect(b'{');

    let mut qvec = QVec([0; STORE_DIM]);
    let mut is_fraud = false;
    let mut got_vector = false;
    let mut got_label = false;
    let mut first_field = true;

    loop {
        parser.skip_ws();
        if !parser.has_data() {
            break;
        }
        if parser.peek() == b'}' {
            parser.advance();
            break;
        }

        if !first_field {
            parser.expect(b',');
        }
        first_field = false;

        // Parse key
        let mut key_buf = [0u8; 8];
        let key_len = parser.parse_string_into(&mut key_buf);
        parser.expect(b':');

        if key_len == 6 && &key_buf[..6] == b"vector" {
            // Parse array of DIM floats
            parser.expect(b'[');
            for i in 0..DIM {
                if i > 0 {
                    parser.expect(b',');
                }
                qvec[i] = parser.parse_fixed4_i16();
            }
            parser.expect(b']');
            got_vector = true;
        } else if key_len == 5 && &key_buf[..5] == b"label" {
            let mut label_buf = [0u8; 8];
            let label_len = parser.parse_string_into(&mut label_buf);
            is_fraud = label_len == 5 && &label_buf[..5] == b"fraud";
            got_label = true;
        } else {
            // Unknown key — skip value
            parser.skip_value();
        }
    }

    assert!(got_vector, "missing 'vector' field");
    assert!(got_label, "missing 'label' field");
    assert_qvec_domain(&qvec);

    (qvec, is_fraud)
}

fn assert_qvec_domain(qvec: &QVec) {
    let mut i = 0usize;
    while i < DIM {
        let v = qvec[i] as i32;
        let ok = if i == 5 || i == 6 {
            (-FIXED_SCALE..=FIXED_SCALE).contains(&v)
        } else {
            (0..=FIXED_SCALE).contains(&v)
        };
        assert!(ok, "vector dimension {} out of safe distance domain: {}", i, v);
        i += 1;
    }
}

// ── Main ─────────────────────────────────────────────────────────────────────

fn main() {
    assert_dist_safe();

    let args: Vec<String> = std::env::args().collect();
    let input_path = args
        .get(1)
        .map(|s| s.as_str())
        .unwrap_or("/app/resources/references.json");
    let output_dir = args
        .get(2)
        .map(|s| s.as_str())
        .unwrap_or("/app/data");

    eprintln!("[preprocess] input:  {}", input_path);
    eprintln!("[preprocess] output: {}", output_dir);

    // Ensure output directory exists
    fs::create_dir_all(output_dir).expect("create output dir");

    let file = fs::File::open(input_path).expect("open input file");
    let reader = BufReader::with_capacity(4 * 1024 * 1024, file);
    let mut parser = StreamParser::new(reader);

    // Partition keys are u8, so bucket directly instead of doing a comparison sort.
    let estimated_count: usize = 3_000_000;
    let mut buckets: [Vec<BuildRecord>; PARTITIONS] =
        std::array::from_fn(|_| Vec::with_capacity(estimated_count / PARTITIONS));

    // Parse outer array
    parser.expect(b'[');
    parser.skip_ws();

    let mut count: usize = 0;

    // Check for empty array
    if parser.has_data() && parser.peek() == b']' {
        parser.advance();
    } else {
        loop {
            let (qvec, is_fraud) = parse_entry(&mut parser);
            let key = partition_key(&qvec);
            buckets[key as usize].push(BuildRecord {
                qvec: qvec.0,
                label: if is_fraud { 1 } else { 0 },
                key,
            });
            count += 1;

            if count % 500_000 == 0 {
                eprintln!("[preprocess] parsed {} vectors...", count);
            }

            parser.skip_ws();
            if !parser.has_data() {
                break;
            }
            if parser.peek() == b']' {
                parser.advance();
                break;
            }
            parser.expect(b',');
        }
    }

    eprintln!("[preprocess] total vectors: {}", count);

    eprintln!("[preprocess] flattening partition buckets...");
    let mut records: Vec<BuildRecord> = Vec::with_capacity(count);
    for mut bucket in buckets {
        records.append(&mut bucket);
    }

    eprintln!("[preprocess] building KD/BBox nodes...");
    let mut partitions = [PartitionMeta::empty(); PARTITIONS];
    let mut partition_node_counts = [0u32; PARTITIONS];
    let mut nodes: Vec<KdNode> = Vec::with_capacity((count / LEAF_SIZE).saturating_mul(2).max(1));
    let mut start = 0usize;
    while start < records.len() {
        let key = records[start].key as usize;
        let mut end = start + 1;
        while end < records.len() && records[end].key as usize == key {
            end += 1;
        }

        let node_start = nodes.len();
        let (root, bbox_min, bbox_max) = build_kd(&mut records[start..end], start, &mut nodes);
        partitions[key] = PartitionMeta {
            bbox_min,
            bbox_max,
            start: start as u32,
            count: (end - start) as u32,
            root: root as i32,
            _pad: 0,
        };
        partition_node_counts[key] = (nodes.len() - node_start) as u32;
        start = end;
    }

    log_distribution(&partitions);

    // Write output files
    let write_legacy = std::env::var("WRITE_LEGACY_INDEX")
        .map(|v| v == "1")
        .unwrap_or(false);
    let ipath = Path::new(output_dir).join("index.bin");

    if write_legacy {
        let vpath = Path::new(output_dir).join("vectors.bin");
        let lpath = Path::new(output_dir).join("labels.bin");
        let ppath = Path::new(output_dir).join("partitions.bin");
        let npath = Path::new(output_dir).join("nodes.bin");

        {
            let mut vfile = BufWriter::new(fs::File::create(&vpath).expect("create vectors.bin"));
            for r in &records {
                write_qvec_raw(&mut vfile, &r.qvec);
            }
            vfile.flush().expect("flush vectors.bin");
        }
        {
            let mut lfile = BufWriter::new(fs::File::create(&lpath).expect("create labels.bin"));
            write_labels_section(&mut lfile, &records);
            lfile.flush().expect("flush labels.bin");
        }
        {
            let mut pfile = BufWriter::new(fs::File::create(&ppath).expect("create partitions.bin"));
            write_partitions(
                &mut pfile,
                count as u64,
                nodes.len() as u64,
                &partitions,
                &partition_node_counts,
            );
            pfile.flush().expect("flush partitions.bin");
        }
        {
            let mut nfile = BufWriter::new(fs::File::create(&npath).expect("create nodes.bin"));
            for node in &nodes {
                write_node(&mut nfile, node);
            }
            nfile.flush().expect("flush nodes.bin");
        }
    }
    {
        let mut ifile = BufWriter::new(fs::File::create(&ipath).expect("create index.bin"));
        write_index_file(
            &mut ifile,
            &records,
            &partitions,
            &partition_node_counts,
            &nodes,
        );
        ifile.flush().expect("flush index.bin");
    }

    let vsize = count * STORE_DIM * std::mem::size_of::<i16>();
    let lsize = count;
    if write_legacy {
        eprintln!(
            "[preprocess] wrote vectors.bin ({} bytes, {} vectors x {} bytes each)",
            vsize,
            count,
            STORE_DIM * std::mem::size_of::<i16>()
        );
        eprintln!("[preprocess] wrote labels.bin  ({} bytes)", lsize);
        eprintln!("[preprocess] wrote partitions.bin ({} partitions)", PARTITIONS);
        eprintln!("[preprocess] wrote nodes.bin ({} nodes)", nodes.len());
    }
    eprintln!("[preprocess] wrote index.bin");
    eprintln!("[preprocess] done.");
}

fn write_index_file<W: Write>(
    w: &mut W,
    records: &[BuildRecord],
    partitions: &[PartitionMeta; PARTITIONS],
    partition_node_counts: &[u32; PARTITIONS],
    nodes: &[KdNode],
) {
    let partitions_len = partition_bytes_len();
    let nodes_len = nodes.len() * node_bytes_len();
    let hot4_len = records.len() * HOT_DIMS.len() * std::mem::size_of::<i16>();
    let mid4_len = records.len() * MID_DIMS.len() * std::mem::size_of::<i16>();
    let cold8_len = records.len() * COLD_SIMD_DIMS.len() * std::mem::size_of::<i16>();
    let labels_len = records.len();

    let partitions_off = INDEX_FILE_HEADER_LEN;
    let nodes_off = align_up(partitions_off + partitions_len, CACHELINE);
    let hot4_off = align_up(nodes_off + nodes_len, CACHELINE);
    let mid4_off = align_up(hot4_off + hot4_len, CACHELINE);
    let cold8_off = align_up(mid4_off + mid4_len, CACHELINE);
    let vectors_off = 0u64;
    let labels_off = align_up(cold8_off + cold8_len, CACHELINE);
    let total_len = labels_off + labels_len;

    w.write_all(INDEX_FILE_MAGIC).expect("write index magic");
    write_u32(w, INDEX_VERSION);
    write_u32(w, DIM as u32);
    write_u32(w, STORE_DIM as u32);
    write_u32(w, SCALE as u32);
    write_u64(w, records.len() as u64);
    write_u64(w, nodes.len() as u64);
    write_u64(w, partitions_off as u64);
    write_u64(w, nodes_off as u64);
    write_u64(w, hot4_off as u64);
    write_u64(w, mid4_off as u64);
    write_u64(w, cold8_off as u64);
    write_u64(w, vectors_off);
    write_u64(w, labels_off as u64);
    write_u64(w, total_len as u64);

    let mut written = RAW_INDEX_FILE_HEADER_LEN;
    write_padding_to(w, &mut written, partitions_off);
    write_partitions(
        w,
        records.len() as u64,
        nodes.len() as u64,
        partitions,
        partition_node_counts,
    );
    written += partitions_len;
    write_padding_to(w, &mut written, nodes_off);
    for node in nodes {
        write_node(w, node);
    }
    written += nodes_len;
    write_padding_to(w, &mut written, hot4_off);
    write_stage_section(w, records, &HOT_DIMS);
    written += hot4_len;
    write_padding_to(w, &mut written, mid4_off);
    write_stage_section(w, records, &MID_DIMS);
    written += mid4_len;
    write_padding_to(w, &mut written, cold8_off);
    write_stage_section(w, records, &COLD_SIMD_DIMS);
    written += cold8_len;
    write_padding_to(w, &mut written, labels_off);
    write_labels_section(w, records);
}

fn write_padding_to<W: Write>(w: &mut W, written: &mut usize, target: usize) {
    const ZEROES: [u8; CACHELINE] = [0; CACHELINE];
    while *written < target {
        let n = (target - *written).min(ZEROES.len());
        w.write_all(&ZEROES[..n]).expect("write index padding");
        *written += n;
    }
}

fn build_kd(
    records: &mut [BuildRecord],
    global_start: usize,
    nodes: &mut Vec<KdNode>,
) -> (u32, QVec, QVec) {
    let (bbox_min, bbox_max) = bbox_records(records);
    let root = build_kd_with_bbox(records, global_start, nodes, bbox_min, bbox_max);
    (root, bbox_min, bbox_max)
}

fn build_kd_with_bbox(
    records: &mut [BuildRecord],
    global_start: usize,
    nodes: &mut Vec<KdNode>,
    bbox_min: QVec,
    bbox_max: QVec,
) -> u32 {
    if records.len() <= LEAF_SIZE {
        let idx = nodes.len() as u32;
        nodes.push(KdNode {
            bbox_min,
            bbox_max,
            left: -1,
            right: -1,
            start: global_start as u32,
            count: records.len() as u32,
        });
        return idx;
    }

    let split_dim = choose_split_dim(records, &bbox_min, &bbox_max);
    let mid = records.len() / 2;
    records.select_nth_unstable_by(mid, |a, b| a.qvec[split_dim].cmp(&b.qvec[split_dim]));

    let idx = nodes.len() as u32;
    nodes.push(KdNode {
        bbox_min,
        bbox_max,
        left: -1,
        right: -1,
        start: 0,
        count: 0,
    });

    let (left_slice, right_slice) = records.split_at_mut(mid);
    let (left_min, left_max) = bbox_records(left_slice);
    let (right_min, right_max) = bbox_records(right_slice);
    let left = build_kd_with_bbox(left_slice, global_start, nodes, left_min, left_max);
    let right = build_kd_with_bbox(
        right_slice,
        global_start + mid,
        nodes,
        right_min,
        right_max,
    );
    let node = &mut nodes[idx as usize];
    node.left = left as i32;
    node.right = right as i32;
    idx
}

fn bbox_records(records: &[BuildRecord]) -> (QVec, QVec) {
    if records.is_empty() {
        return (QVec([0; STORE_DIM]), QVec([0; STORE_DIM]));
    }
    let mut min = [i16::MAX; STORE_DIM];
    let mut max = [i16::MIN; STORE_DIM];
    for r in records {
        let mut d = 0usize;
        while d < STORE_DIM {
            let v = r.qvec[d];
            if v < min[d] {
                min[d] = v;
            }
            if v > max[d] {
                max[d] = v;
            }
            d += 1;
        }
    }
    (QVec(min), QVec(max))
}

fn widest_dim(bbox_min: &QVec, bbox_max: &QVec) -> usize {
    let mut best_dim = 0usize;
    let mut best_width = i32::MIN;
    let mut d = 0usize;
    while d < DIM {
        let width = bbox_max[d] as i32 - bbox_min[d] as i32;
        if width > best_width {
            best_width = width;
            best_dim = d;
        }
        d += 1;
    }
    best_dim
}

fn choose_split_dim(records: &[BuildRecord], bbox_min: &QVec, bbox_max: &QVec) -> usize {
    let samples = records.len().min(SPLIT_SAMPLE);
    if samples < 2 {
        return widest_dim(bbox_min, bbox_max);
    }

    let mut sums = [0i64; DIM];
    let mut sums_sq = [0i64; DIM];
    let mut s = 0usize;
    while s < samples {
        let idx = s * records.len() / samples;
        let qvec = &records[idx].qvec;
        let mut d = 0usize;
        while d < DIM {
            let v = qvec[d] as i64;
            sums[d] += v;
            sums_sq[d] += v * v;
            d += 1;
        }
        s += 1;
    }

    let n = samples as i64;
    let mut best_dim = 0usize;
    let mut best_score = i64::MIN;
    let mut best_width = i32::MIN;
    let mut d = 0usize;
    while d < DIM {
        let score = n * sums_sq[d] - sums[d] * sums[d];
        let width = bbox_max[d] as i32 - bbox_min[d] as i32;
        if score > best_score || (score == best_score && width > best_width) {
            best_score = score;
            best_width = width;
            best_dim = d;
        }
        d += 1;
    }

    if best_score <= 0 {
        widest_dim(bbox_min, bbox_max)
    } else {
        best_dim
    }
}

fn log_distribution(partitions: &[PartitionMeta; PARTITIONS]) {
    let mut counts: Vec<u32> = partitions
        .iter()
        .map(|p| p.count)
        .filter(|&count| count != 0)
        .collect();
    counts.sort_unstable();

    let total: u64 = counts.iter().map(|&count| count as u64).sum();
    let ideal = if total == 0 {
        0.0
    } else {
        total as f64 / PARTITIONS as f64
    };
    let pct = |p: usize, counts: &[u32]| -> u32 {
        if counts.is_empty() {
            0
        } else {
            counts[(counts.len() - 1) * p / 100]
        }
    };
    let max = counts.last().copied().unwrap_or(0);
    let skew = if ideal == 0.0 { 0.0 } else { max as f64 / ideal };

    eprintln!(
        "[preprocess] partitions: non_empty={}, empty={}, min={}, p50={}, p90={}, p95={}, p99={}, max={}, skew={:.2}",
        counts.len(),
        PARTITIONS - counts.len(),
        counts.first().copied().unwrap_or(0),
        pct(50, &counts),
        pct(90, &counts),
        pct(95, &counts),
        pct(99, &counts),
        max,
        skew
    );

    let mut top: Vec<(usize, u32)> = partitions
        .iter()
        .enumerate()
        .filter_map(|(idx, p)| (p.count != 0).then_some((idx, p.count)))
        .collect();
    top.sort_unstable_by(|a, b| b.1.cmp(&a.1));
    let mut line = String::from("[preprocess] top partitions:");
    for (idx, count) in top.into_iter().take(10) {
        line.push_str(" ");
        line.push_str(&idx.to_string());
        line.push_str("=");
        line.push_str(&count.to_string());
    }
    eprintln!("{}", line);
}

fn write_partitions<W: Write>(
    w: &mut W,
    count: u64,
    nodes_count: u64,
    partitions: &[PartitionMeta; PARTITIONS],
    partition_node_counts: &[u32; PARTITIONS],
) {
    w.write_all(PARTITIONS_MAGIC).expect("write magic");
    write_u32(w, INDEX_VERSION);
    write_u64(w, count);
    write_u64(w, nodes_count);
    for (i, p) in partitions.iter().enumerate() {
        write_u32(w, p.start);
        write_u32(w, p.count);
        write_i32(w, p.root);
        write_u32(w, partition_node_counts[i]);
        write_qvec(w, &p.bbox_min);
        write_qvec(w, &p.bbox_max);
    }
}

const fn partition_bytes_len() -> usize {
    8 + 4 + 8 + 8 + PARTITIONS * (4 + 4 + 4 + 4 + STORE_DIM * 2 + STORE_DIM * 2)
}

const fn node_bytes_len() -> usize {
    STORE_DIM * 2 + STORE_DIM * 2 + 4 + 4 + 4 + 4
}

fn write_node<W: Write>(w: &mut W, node: &KdNode) {
    write_qvec(w, &node.bbox_min);
    write_qvec(w, &node.bbox_max);
    write_i32(w, node.left);
    write_i32(w, node.right);
    write_u32(w, node.start);
    write_u32(w, node.count);
}

fn write_qvec<W: Write>(w: &mut W, qvec: &QVec) {
    write_qvec_raw(w, &qvec.0);
}

fn write_qvec_raw<W: Write>(w: &mut W, qvec: &[i16; STORE_DIM]) {
    for v in qvec.iter() {
        w.write_all(&v.to_le_bytes()).expect("write qvec");
    }
}

fn write_stage_section<W: Write>(w: &mut W, records: &[BuildRecord], dims: &[usize]) {
    let mut buf = Vec::with_capacity(SECTION_WRITE_BUF);
    for r in records {
        for &dim in dims {
            if buf.len() + 2 > SECTION_WRITE_BUF {
                w.write_all(&buf).expect("write index stage");
                buf.clear();
            }
            buf.extend_from_slice(&r.qvec[dim].to_le_bytes());
        }
    }
    if !buf.is_empty() {
        w.write_all(&buf).expect("write index stage");
    }
}

fn write_labels_section<W: Write>(w: &mut W, records: &[BuildRecord]) {
    let mut buf = Vec::with_capacity(SECTION_WRITE_BUF);
    for r in records {
        if buf.len() == SECTION_WRITE_BUF {
            w.write_all(&buf).expect("write labels");
            buf.clear();
        }
        buf.push(r.label);
    }
    if !buf.is_empty() {
        w.write_all(&buf).expect("write labels");
    }
}

fn write_u32<W: Write>(w: &mut W, v: u32) {
    w.write_all(&v.to_le_bytes()).expect("write u32");
}

fn write_i32<W: Write>(w: &mut W, v: i32) {
    w.write_all(&v.to_le_bytes()).expect("write i32");
}

fn write_u64<W: Write>(w: &mut W, v: u64) {
    w.write_all(&v.to_le_bytes()).expect("write u64");
}
