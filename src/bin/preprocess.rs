// preprocess.rs - Build-time index builder
//
// Reads the uncompressed references.json (3M labeled vectors),
// quantizes each vector to i16 fixed-point, partitions it, builds exact
// KD/BBox trees per partition, and writes:
//   - vectors.bin      (count x STORE_DIM x 2 bytes, contiguous little-endian i16)
//   - labels.bin       (count bytes, 0=legit 1=fraud)
//   - partitions.bin   (Index v3 metadata)
//   - nodes.bin        (KD/BBox nodes)
//
// Usage:
//   preprocess [input_json] [output_dir]
//
// Defaults:
//   input_json = /app/resources/references.json
//   output_dir = /app/data

#[path = "../index.rs"]
mod index;

use index::*;
use std::fs;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

const LEAF_SIZE: usize = 64;

#[derive(Clone, Copy)]
struct Record {
    qvec: QVec,
    label: u8,
    key: u8,
}

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

    let mut vector: [f64; DIM] = [0.0; DIM];
    let mut is_fraud = false;
    let mut got_vector = false;
    let mut got_label = false;

    loop {
        parser.skip_ws();
        if !parser.has_data() {
            break;
        }
        if parser.peek() == b'}' {
            parser.advance();
            break;
        }
        if got_vector || got_label {
            // After the first field, expect comma
            if parser.peek() == b',' {
                parser.advance();
            }
        }

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
                vector[i] = parser.parse_number();
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

    (quantize_vec(&vector), is_fraud)
}

// ── Main ─────────────────────────────────────────────────────────────────────

fn main() {
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

    // Pre-allocate for expected 3M vectors (will grow if needed)
    let estimated_count: usize = 3_000_000;
    let mut records: Vec<Record> = Vec::with_capacity(estimated_count);

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
            records.push(Record {
                qvec,
                label: if is_fraud { 1 } else { 0 },
                key: partition_key(&qvec),
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

    eprintln!("[preprocess] sorting by partition key...");
    records.sort_unstable_by_key(|r| r.key);

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
        let root = build_kd(&mut records[start..end], start, &mut nodes) as i32;
        let (bbox_min, bbox_max) = bbox_records(&records[start..end]);
        partitions[key] = PartitionMeta {
            start: start as u32,
            count: (end - start) as u32,
            root,
            bbox_min,
            bbox_max,
        };
        partition_node_counts[key] = (nodes.len() - node_start) as u32;
        start = end;
    }

    log_distribution(&partitions);

    // Write output files
    let vpath = Path::new(output_dir).join("vectors.bin");
    let lpath = Path::new(output_dir).join("labels.bin");
    let ppath = Path::new(output_dir).join("partitions.bin");
    let npath = Path::new(output_dir).join("nodes.bin");

    {
        let mut vfile = BufWriter::new(fs::File::create(&vpath).expect("create vectors.bin"));
        for r in &records {
            write_qvec(&mut vfile, &r.qvec);
        }
        vfile.flush().expect("flush vectors.bin");
    }
    {
        let mut lfile = BufWriter::new(fs::File::create(&lpath).expect("create labels.bin"));
        for r in &records {
            lfile.write_all(&[r.label]).expect("write label");
        }
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

    let vsize = count * STORE_DIM * std::mem::size_of::<i16>();
    let lsize = count;
    eprintln!(
        "[preprocess] wrote vectors.bin ({} bytes, {} vectors × {} bytes each)",
        vsize,
        count,
        STORE_DIM * std::mem::size_of::<i16>()
    );
    eprintln!("[preprocess] wrote labels.bin  ({} bytes)", lsize);
    eprintln!("[preprocess] wrote partitions.bin ({} partitions)", PARTITIONS);
    eprintln!("[preprocess] wrote nodes.bin ({} nodes)", nodes.len());
    eprintln!("[preprocess] done.");
}

fn build_kd(records: &mut [Record], global_start: usize, nodes: &mut Vec<KdNode>) -> u32 {
    let (bbox_min, bbox_max) = bbox_records(records);
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

    let split_dim = widest_dim(&bbox_min, &bbox_max);
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
    let left = build_kd(left_slice, global_start, nodes);
    let right = build_kd(right_slice, global_start + mid, nodes);
    let node = &mut nodes[idx as usize];
    node.left = left as i32;
    node.right = right as i32;
    idx
}

fn bbox_records(records: &[Record]) -> (QVec, QVec) {
    if records.is_empty() {
        return ([0; STORE_DIM], [0; STORE_DIM]);
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
    (min, max)
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

fn log_distribution(partitions: &[PartitionMeta; PARTITIONS]) {
    let mut non_empty = 0usize;
    let mut max_count = 0u32;
    let mut total = 0usize;
    for p in partitions {
        if p.count != 0 {
            non_empty += 1;
            total += p.count as usize;
            if p.count > max_count {
                max_count = p.count;
            }
        }
    }
    eprintln!(
        "[preprocess] partitions: non_empty={}, avg_non_empty={}, max={}",
        non_empty,
        if non_empty == 0 { 0 } else { total / non_empty },
        max_count
    );
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

fn write_node<W: Write>(w: &mut W, node: &KdNode) {
    write_qvec(w, &node.bbox_min);
    write_qvec(w, &node.bbox_max);
    write_i32(w, node.left);
    write_i32(w, node.right);
    write_u32(w, node.start);
    write_u32(w, node.count);
}

fn write_qvec<W: Write>(w: &mut W, qvec: &QVec) {
    for v in qvec {
        w.write_all(&v.to_le_bytes()).expect("write qvec");
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
