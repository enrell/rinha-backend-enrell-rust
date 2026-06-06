// Shared index format and quantization helpers.

use std::ops::{Deref, DerefMut};

pub const DIM: usize = 14;
pub const STORE_DIM: usize = 16;
pub const SCALE: f64 = 10_000.0;
pub const PARTITIONS: usize = 256;

pub const PARTITIONS_MAGIC: &[u8; 8] = b"RINHIDX5";
pub const INDEX_VERSION: u32 = 5;
pub const INDEX_FILE_MAGIC: &[u8; 8] = b"RINHIF03";
pub const CACHELINE: usize = 64;
pub const RAW_INDEX_FILE_HEADER_LEN: usize = 104;
pub const INDEX_FILE_HEADER_LEN: usize = align_up(RAW_INDEX_FILE_HEADER_LEN, CACHELINE);

pub const HOT_DIMS: [usize; 4] = [0, 1, 2, 8];
pub const MID_DIMS: [usize; 4] = [4, 6, 7, 13];
pub const COLD_REAL_DIMS: [usize; 6] = [3, 5, 9, 10, 11, 12];
pub const PAD_DIMS: [usize; 2] = [14, 15];
pub const COLD_SIMD_DIMS: [usize; 8] = [
    COLD_REAL_DIMS[0],
    COLD_REAL_DIMS[1],
    COLD_REAL_DIMS[2],
    COLD_REAL_DIMS[3],
    COLD_REAL_DIMS[4],
    COLD_REAL_DIMS[5],
    PAD_DIMS[0],
    PAD_DIMS[1],
];

#[repr(C, align(32))]
#[derive(Clone, Copy)]
pub struct QVec(pub [i16; STORE_DIM]);

impl Deref for QVec {
    type Target = [i16; STORE_DIM];

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for QVec {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

#[repr(C, align(32))]
#[derive(Clone, Copy)]
pub struct PartitionMeta {
    pub bbox_min: QVec,
    pub bbox_max: QVec,
    pub start: u32,
    pub count: u32,
    pub root: i32,
    pub _pad: u32,
}

impl PartitionMeta {
    pub const fn empty() -> Self {
        Self {
            bbox_min: QVec([0; STORE_DIM]),
            bbox_max: QVec([0; STORE_DIM]),
            start: 0,
            count: 0,
            root: -1,
            _pad: 0,
        }
    }
}

#[repr(C)]
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
    debug_assert!(val.is_finite());
    debug_assert!((-3.2768..=3.2767).contains(&val));
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
    QVec(qv)
}

pub const fn align_up(v: usize, align: usize) -> usize {
    (v + align - 1) & !(align - 1)
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
