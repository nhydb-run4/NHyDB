//! Physical container format.
//!
//! File format 0.28.2 (signature `NHYDB` + version bytes 0, 28, 2): two
//! alternating 4 KiB superblocks with CRC32, append-only blocks with a
//! 32-byte header, 8-byte alignment, commit boundary `last_valid_end`;
//! block types 1–14. Schema/layout metadata (types 1, 2, 3, 8, 9, 11) is not
//! stored in the DB but encrypted in the external layout plan. The format
//! version is the engine release that defined it; this one adds the
//! persistent block directory (type 14, `db/directory.rs`) and the
//! superblock field `dir_root`. Files of older formats are not read
//! (no backward compatibility in the development phase).

use crate::error::{Error, Result};

pub const SUPERBLOCK_SIZE: usize = 4096;
pub const SUPERBLOCK_COUNT: usize = 2;
pub const DATA_START: u64 = (SUPERBLOCK_SIZE * SUPERBLOCK_COUNT) as u64;
/// Format version (major, minor, patch).
pub const FORMAT_VERSION: [u8; 3] = [0, 28, 2];
/// Superblock signature: `NHYDB` followed by the three version bytes.
pub const SUPER_MAGIC: &[u8; 8] = &[b'N', b'H', b'Y', b'D', b'B', FORMAT_VERSION[0], FORMAT_VERSION[1], FORMAT_VERSION[2]];
/// Superblock version fields (u16 each); the patch level is in the signature.
pub const FORMAT_MAJOR: u16 = FORMAT_VERSION[0] as u16;
pub const FORMAT_MINOR: u16 = FORMAT_VERSION[1] as u16;
pub const BLOCK_MAGIC: &[u8; 4] = b"NHBL";
pub const BLOCK_VERSION: u8 = 16;
/// `<8sHHIQQQQQQIQ` = 8+2+2+4+6*8+4+8
pub const SUPER_STRUCT_SIZE: usize = 76;
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
/// Persistent block directory (page, run or root, see `db/directory.rs`).
pub const BT_DIRECTORY: u8 = 14;
/// Pages and posting chunks of the SECONDARY key trees (0.28.2).
pub const BT_SECONDARY_PAGE: u8 = 15;

/// Block types stored exclusively in the external layout plan.
pub const LAYOUT_META_TYPES: [u8; 6] = [BT_REALM, BT_SHAPE, BT_EXTENSION_DEF, BT_LAYOUT_DEF, BT_INDEX_DEF, BT_MATH_LAYOUT_STATE];

#[inline]
pub fn is_layout_meta(bt: u8) -> bool {
    LAYOUT_META_TYPES.contains(&bt)
}

/// DATA payload with prefix checksum (0.28.2): `D28D vu(realm) vu(shape)
/// vu(u_start) vu(count) u32(prefix_crc of the bytes before it)` …
pub const DATA_TAG: &[u8; 4] = b"D28D";
/// Root of a SECONDARY key tree (0.28.2, `db/secindex.rs`).
pub const SEC_TAG: &[u8; 4] = b"SI28";
pub const CHECKPOINT_TAG: &[u8; 4] = b"CP10";
/// SPARSE payload with prefix checksum (0.28.2): `SP28 vu(realm)
/// vu(u_start) vu(u_end) vu(groups) vu(cells) u32(prefix_crc of the bytes before it)` …
pub const SPARSE_TAG: &[u8; 4] = b"SP28";

/// CRC-32C (Castagnoli, reflected polynomial 0x82F63B78) of a block prefix.
/// Deliberately not the CRC32 of the payload: a CRC32 appended to the bytes
/// it covers makes the CRC32 over both a constant, so the payload CRC would
/// no longer depend on the prefix. With a different polynomial the payload
/// CRC keeps checking the prefix independently: a block whose prefix and
/// prefix checksum were rewritten still fails the payload CRC when the block
/// is read. Both are unkeyed checksums against accidental damage; a
/// deliberately forged block with all checksums recomputed is not detected
/// (RA2-04, and a moved range may never be read for its real u).
pub fn prefix_crc(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = (c >> 1) ^ (0x82F6_3B78 & (c & 1).wrapping_neg());
        }
    }
    !c
}

/// Checks the prefix checksum behind the prefix fields that end at `*p`
/// (`prefix_crc` of `raw[..*p]`, little endian) and moves `*p` behind it.
/// RD-08: the routing prefix of a block is protected on open without
/// reading the whole payload.
pub fn check_prefix_crc(raw: &[u8], p: &mut usize) -> Result<()> {
    let got = raw.get(*p..*p + 4).ok_or_else(|| Error::Corruption("block prefix checksum incomplete".into()))?;
    if u32::from_le_bytes(got.try_into().unwrap()) != prefix_crc(&raw[..*p]) {
        return Err(Error::Corruption("block prefix checksum mismatch".into()));
    }
    *p += 4;
    Ok(())
}

/// Appends the prefix checksum to a prefix being written.
pub fn seal_prefix(prefix: &mut Vec<u8>) {
    let c = prefix_crc(prefix);
    prefix.extend_from_slice(&c.to_le_bytes());
}
pub const EXT_TAG: u8 = 0xEA;

pub const ENGINE_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const FILE_FORMAT_VERSION: &str = "0.28.2";
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
        BT_DIRECTORY => "DIRECTORY",
        BT_SECONDARY_PAGE => "SECONDARY_PAGE",
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
    /// Offset of the committed block directory root (0 = none).
    pub dir_root: u64,
}

/// Readable name of a superblock signature (`NHYDB 0.28.2`, `NHYDB016`, …).
pub fn format_name(sig: &[u8]) -> String {
    match sig {
        [b'N', b'H', b'Y', b'D', b'B', a, b, c] if !(a.is_ascii_digit() && b.is_ascii_digit() && c.is_ascii_digit()) => format!("NHYDB {a}.{b}.{c}"),
        _ => String::from_utf8_lossy(sig).into_owned(),
    }
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
    raw[68..76].copy_from_slice(&s.dir_root.to_le_bytes());
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
    if !(&raw[0..8] == SUPER_MAGIC && major == FORMAT_MAJOR && minor == FORMAT_MINOR) {
        return Err(Error::Corruption(format!("wrong NHyDB format {}, expected {FILE_FORMAT_VERSION}", format_name(&raw[0..8]))));
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
        dir_root: u64::from_le_bytes(raw[68..76].try_into().unwrap()),
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
        let s = SuperState { generation: 7, checkpoint_offset: 9, last_valid_end: 8192, next_block_id: 3, created_ns: 1, updated_ns: 2, flags: 0, dir_root: 8192 };
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

#[cfg(test)]
mod prefix_crc_tests {
    use super::*;

    #[test]
    fn prefix_crc_is_crc32c_and_independent_of_the_payload_crc() {
        assert_eq!(prefix_crc(b"123456789"), 0xE306_9283); // CRC-32C check value
        let mut a = b"D28D\x01\x01\x0b\x0d".to_vec();
        let mut b = b"D28D\x01\x01\x29\x0d".to_vec();
        seal_prefix(&mut a);
        seal_prefix(&mut b);
        a.extend_from_slice(b"records");
        b.extend_from_slice(b"records");
        assert_ne!(crc32(&a), crc32(&b), "the payload CRC must still see a forged prefix");
        let mut p = 8;
        check_prefix_crc(&a, &mut p).unwrap();
        assert_eq!(p, 12);
    }
}
