//! Physical container format.
//!
//! File format 0.16 (`NHYDB016`): two alternating 4 KiB superblocks with
//! CRC32, append-only blocks with a 32-byte header, 8-byte alignment, commit
//! boundary `last_valid_end`; block types 1–13. Schema/layout metadata
//! (types 1, 2, 3, 8, 9, 11) is not stored in the DB but encrypted in the
//! external layout plan.

use crate::error::{Error, Result};

pub const SUPERBLOCK_SIZE: usize = 4096;
pub const SUPERBLOCK_COUNT: usize = 2;
pub const DATA_START: u64 = (SUPERBLOCK_SIZE * SUPERBLOCK_COUNT) as u64;
pub const SUPER_MAGIC: &[u8; 8] = b"NHYDB016";
pub const FORMAT_MAJOR: u16 = 0;
pub const FORMAT_MINOR: u16 = 16;
pub const BLOCK_MAGIC: &[u8; 4] = b"NHBL";
pub const BLOCK_VERSION: u8 = 16;
/// `<8sHHIQQQQQQI` = 8+2+2+4+6*8+4
pub const SUPER_STRUCT_SIZE: usize = 68;
pub const SUPER_CRC_OFFSET: usize = SUPERBLOCK_SIZE - 4;
/// `<4sBBHQQII`
pub const BLOCK_HEADER_SIZE: usize = 32;

pub const BT_REALM: u8 = 1;
pub const BT_SHAPE: u8 = 2;
pub const BT_INDEX_DEF: u8 = 3;
pub const BT_DATA_DENSE: u8 = 4;
pub const BT_DELETE: u8 = 5;
pub const BT_CHECKPOINT: u8 = 6;
pub const BT_SECONDARY: u8 = 7;
pub const BT_EXTENSION_DEF: u8 = 8;
pub const BT_LAYOUT_DEF: u8 = 9;
pub const BT_SPARSE_CELLS: u8 = 10;
pub const BT_MATH_LAYOUT_STATE: u8 = 11;
pub const BT_DB_CONFIG: u8 = 12;
/// R0,u1: DB-internal bootstrap/generation records.
pub const BT_SYSTEM: u8 = 13;

/// Block types stored exclusively in the external layout plan.
pub const LAYOUT_META_TYPES: [u8; 6] = [BT_REALM, BT_SHAPE, BT_EXTENSION_DEF, BT_LAYOUT_DEF, BT_INDEX_DEF, BT_MATH_LAYOUT_STATE];

#[inline]
pub fn is_layout_meta(bt: u8) -> bool {
    LAYOUT_META_TYPES.contains(&bt)
}

pub const DATA_TAG: &[u8; 4] = b"D10D";
pub const SEC_TAG: &[u8; 4] = b"SI10";
pub const CHECKPOINT_TAG: &[u8; 4] = b"CP10";
pub const SPARSE_TAG: &[u8; 4] = b"SP10";
pub const EXT_TAG: u8 = 0xEA;

pub const ENGINE_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const FILE_FORMAT_VERSION: &str = "0.16";
pub const DEFAULT_BATCH_RECORDS: usize = 10_000;

pub fn block_name(bt: u8) -> Option<&'static str> {
    Some(match bt {
        BT_REALM => "REALM",
        BT_SHAPE => "SHAPE",
        BT_INDEX_DEF => "INDEX_DEF",
        BT_DATA_DENSE => "DATA_DENSE",
        BT_DELETE => "DELETE",
        BT_CHECKPOINT => "CHECKPOINT",
        BT_SECONDARY => "SECONDARY_INDEX",
        BT_EXTENSION_DEF => "EXTENSION_DEF",
        BT_LAYOUT_DEF => "LAYOUT_DEF",
        BT_SPARSE_CELLS => "SPARSE_CELLS",
        BT_MATH_LAYOUT_STATE => "MATH_LAYOUT_STATE",
        BT_DB_CONFIG => "DB_CONFIG",
        BT_SYSTEM => "SYSTEM_R0_U1",
        _ => return None,
    })
}

/// Rounds up to a multiple of 8. Never wraps (F056): values that cannot be
/// rounded up within u64 yield `u64::MAX`, which is still >= `n`, so every
/// size/limit check fails closed.
#[inline]
pub fn align8(n: u64) -> u64 {
    match n.checked_add(7) {
        Some(x) => x & !7,
        None => u64::MAX,
    }
}

#[inline]
pub fn crc32(data: &[u8]) -> u32 {
    crc32fast::hash(data)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct SuperState {
    pub generation: u64,
    pub checkpoint_offset: u64,
    pub last_valid_end: u64,
    pub next_block_id: u64,
    pub created_ns: u64,
    pub updated_ns: u64,
    pub flags: u32,
}

pub fn pack_super(s: &SuperState) -> Vec<u8> {
    let mut raw = vec![0u8; SUPERBLOCK_SIZE];
    raw[0..8].copy_from_slice(SUPER_MAGIC);
    raw[8..10].copy_from_slice(&FORMAT_MAJOR.to_le_bytes());
    raw[10..12].copy_from_slice(&FORMAT_MINOR.to_le_bytes());
    raw[12..16].copy_from_slice(&(SUPERBLOCK_SIZE as u32).to_le_bytes());
    let vals = [s.generation, s.checkpoint_offset, s.last_valid_end, s.next_block_id, s.created_ns, s.updated_ns];
    for (i, v) in vals.iter().enumerate() {
        raw[16 + i * 8..24 + i * 8].copy_from_slice(&v.to_le_bytes());
    }
    raw[64..68].copy_from_slice(&s.flags.to_le_bytes());
    let crc = crc32(&raw[..SUPER_CRC_OFFSET]);
    raw[SUPER_CRC_OFFSET..].copy_from_slice(&crc.to_le_bytes());
    raw
}

pub fn unpack_super(raw: &[u8]) -> Result<SuperState> {
    if raw.len() != SUPERBLOCK_SIZE {
        return Err(Error::Corruption("superblock incomplete".into()));
    }
    let got = u32::from_le_bytes(raw[SUPER_CRC_OFFSET..].try_into().unwrap());
    if got != crc32(&raw[..SUPER_CRC_OFFSET]) {
        return Err(Error::Corruption("superblock CRC mismatch".into()));
    }
    let major = u16::from_le_bytes([raw[8], raw[9]]);
    let minor = u16::from_le_bytes([raw[10], raw[11]]);
    if &raw[0..8] != SUPER_MAGIC || major != FORMAT_MAJOR || minor != FORMAT_MINOR {
        return Err(Error::Corruption("wrong NHyDB format, expected 0.16".into()));
    }
    let sbsize = u32::from_le_bytes(raw[12..16].try_into().unwrap());
    if sbsize as usize != SUPERBLOCK_SIZE {
        return Err(Error::Corruption("unknown superblock size".into()));
    }
    let q = |i: usize| u64::from_le_bytes(raw[16 + i * 8..24 + i * 8].try_into().unwrap());
    Ok(SuperState {
        generation: q(0),
        checkpoint_offset: q(1),
        last_valid_end: q(2),
        next_block_id: q(3),
        created_ns: q(4),
        updated_ns: q(5),
        flags: u32::from_le_bytes(raw[64..68].try_into().unwrap()),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockHeader {
    pub block_type: u8,
    pub flags: u16,
    pub block_id: u64,
    pub payload_len: u64,
    pub payload_crc: u32,
}

fn header_bytes(h: &BlockHeader, hcrc: u32) -> [u8; BLOCK_HEADER_SIZE] {
    let mut b = [0u8; BLOCK_HEADER_SIZE];
    b[0..4].copy_from_slice(BLOCK_MAGIC);
    b[4] = BLOCK_VERSION;
    b[5] = h.block_type;
    b[6..8].copy_from_slice(&h.flags.to_le_bytes());
    b[8..16].copy_from_slice(&h.block_id.to_le_bytes());
    b[16..24].copy_from_slice(&h.payload_len.to_le_bytes());
    b[24..28].copy_from_slice(&h.payload_crc.to_le_bytes());
    b[28..32].copy_from_slice(&hcrc.to_le_bytes());
    b
}

pub fn pack_block_header(bt: u8, bid: u64, payload: &[u8], flags: u16) -> [u8; BLOCK_HEADER_SIZE] {
    let h = BlockHeader { block_type: bt, flags, block_id: bid, payload_len: payload.len() as u64, payload_crc: crc32(payload) };
    let tmp = header_bytes(&h, 0);
    header_bytes(&h, crc32(&tmp))
}

pub fn unpack_block_header(raw: &[u8]) -> Result<BlockHeader> {
    if raw.len() != BLOCK_HEADER_SIZE {
        return Err(Error::Corruption("block header incomplete".into()));
    }
    let bt = raw[5];
    if &raw[0..4] != BLOCK_MAGIC || raw[4] != BLOCK_VERSION || block_name(bt).is_none() {
        return Err(Error::Corruption("block header magic/version/type invalid".into()));
    }
    let h = BlockHeader {
        block_type: bt,
        flags: u16::from_le_bytes([raw[6], raw[7]]),
        block_id: u64::from_le_bytes(raw[8..16].try_into().unwrap()),
        payload_len: u64::from_le_bytes(raw[16..24].try_into().unwrap()),
        payload_crc: u32::from_le_bytes(raw[24..28].try_into().unwrap()),
    };
    let hcrc = u32::from_le_bytes(raw[28..32].try_into().unwrap());
    if crc32(&header_bytes(&h, 0)) != hcrc {
        return Err(Error::Corruption("block header CRC mismatch".into()));
    }
    Ok(h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn super_roundtrip() {
        let s = SuperState { generation: 7, checkpoint_offset: 9, last_valid_end: 8192, next_block_id: 3, created_ns: 1, updated_ns: 2, flags: 0 };
        let raw = pack_super(&s);
        assert_eq!(unpack_super(&raw).unwrap(), s);
        let mut bad = raw.clone();
        bad[20] ^= 1;
        assert!(unpack_super(&bad).is_err());
    }

    #[test]
    fn header_roundtrip() {
        let h = pack_block_header(BT_SHAPE, 5, b"hello", 1);
        let p = unpack_block_header(&h).unwrap();
        assert_eq!((p.block_type, p.block_id, p.payload_len, p.flags), (BT_SHAPE, 5, 5, 1));
        assert_eq!(p.payload_crc, crc32(b"hello"));
        let mut bad = h;
        bad[9] ^= 1;
        assert!(unpack_block_header(&bad).is_err());
    }
}
