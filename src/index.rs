// Shared index format and quantization helpers.

pub const DIM: usize = 14;
pub const STORE_DIM: usize = 16;
pub const SCALE: f64 = 10_000.0;
pub const PARTITIONS: usize = 256;

pub const PARTITIONS_MAGIC: &[u8; 8] = b"RINHIDX3";
pub const INDEX_VERSION: u32 = 3;
pub const INDEX_FILE_MAGIC: &[u8; 8] = b"RINHIF01";
pub const INDEX_FILE_HEADER_LEN: usize = 80;

pub type QVec = [i16; STORE_DIM];

#[derive(Clone, Copy)]
pub struct PartitionMeta {
    pub start: u32,
    pub count: u32,
    pub root: i32,
    pub bbox_min: QVec,
    pub bbox_max: QVec,
}

impl PartitionMeta {
    pub const fn empty() -> Self {
        Self {
            start: 0,
            count: 0,
            root: -1,
            bbox_min: [0; STORE_DIM],
            bbox_max: [0; STORE_DIM],
        }
    }
}

#[derive(Clone, Copy)]
pub struct KdNode {
    pub bbox_min: QVec,
    pub bbox_max: QVec,
    pub left: i32,
    pub right: i32,
    pub start: u32,
    pub count: u32,
}

#[inline(always)]
pub fn quantize(val: f64) -> i16 {
    (val * SCALE).round() as i16
}

#[inline]
pub fn quantize_vec(vals: &[f64; DIM]) -> QVec {
    let mut qv = [0i16; STORE_DIM];
    let mut i = 0;
    while i < DIM {
        qv[i] = quantize(vals[i]);
        i += 1;
    }
    qv
}

#[inline(always)]
pub fn partition_key(v: &QVec) -> u8 {
    let mut key = 0u8;
    if v[9] != 0 {
        key |= 1;
    }
    if v[10] != 0 {
        key |= 2;
    }
    if v[11] != 0 {
        key |= 4;
    }
    if v[5] != -10_000 {
        key |= 8;
    }
    key |= (bin4(v[12]) & 0x03) << 4;
    key |= (bin4(v[3]) & 0x03) << 6;
    key
}

#[inline(always)]
fn bin4(v: i16) -> u8 {
    if v < 2_500 {
        0
    } else if v < 5_000 {
        1
    } else if v < 7_500 {
        2
    } else {
        3
    }
}
