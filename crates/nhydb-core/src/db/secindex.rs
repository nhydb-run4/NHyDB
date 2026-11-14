//! Persistent SECONDARY key directory (0.28.2): a sorted tree of pages on
//! disk that is searched page by page.
//!
//! Up to 0.28.2-dev a SECONDARY block held the directory of *all* keys of an
//! index; the first query decoded it completely into a hash map (about
//! 150 bytes of RAM per distinct key, plus a sorted copy for range
//! lookups). Since 0.28.2 an index (per lane) is a bulk-loaded B+ tree:
//!
//! * leaf pages (block type 15, tag `SL28`): sorted keys with their posting
//!   count, codec and location – small postings inline, larger ones
//!   zlib-compressed in posting chunk blocks (type 15, tag `SC28`);
//! * inner pages (type 15, tag `SN28`): first key, offset and block id of
//!   each child;
//! * the root (type 7, tag `SI28`): index id, source block, key count,
//!   height and the top page.
//!
//! A lookup reads one page per level; decoded pages are kept in a byte
//! bounded LRU cache (`[cache] index_page_cache_bytes`). Range lookups walk
//! the leaves in key order (no key copy). Every page is a block of its own:
//! its payload CRC is checked when it is read, and the parent entry binds it
//! (offset, block id, tag, index, level).
//!
//! Key order (also the order of the writer): numbers (no NaN) by value,
//! then `str` (code point order = UTF-8 bytes), then `bytes`, then all other
//! keys (None, NaN, lists, dicts) by subclass and encoding. Equal numbers of
//! different types (`1`, `1.0`, `True`) are one key, as in a Python dict.


use std::cmp::Ordering;
use std::sync::Arc;

use num_bigint::BigInt;

use crate::address::{lane_flags, lane_from_flags, Lane};
use crate::error::{Error, Result};
use crate::format::*;
use crate::nat::{get_vu_u64, put_vu64, Nat};
use crate::val::{Num, Val};

use super::index::{decode_big_posting, json_key_decode, json_key_encode, SecondaryMeta};
use super::{Db, PostingRef, SecondaryDir};

pub const LEAF_TAG: &[u8; 4] = b"SL28";
pub const INNER_TAG: &[u8; 4] = b"SN28";
pub const CHUNK_TAG: &[u8; 4] = b"SC28";
/// Root tag of the key tree.
pub const ROOT_TAG: &[u8; 4] = SEC_TAG;

/// Target size of an uncompressed page body (a single large key may exceed it).
const PAGE_BYTES: usize = 4 << 10;

#[cfg(test)]
thread_local! {
    /// Tests lower the page size (in their own thread) to get deep trees
    /// from few keys.
    static TEST_PAGE_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(PAGE_BYTES) };
}

#[cfg(test)]
pub(crate) fn page_bytes() -> usize {
    TEST_PAGE_BYTES.with(|c| c.get())
}

#[cfg(not(test))]
#[inline]
pub(crate) fn page_bytes() -> usize {
    PAGE_BYTES
}

#[cfg(all(test, nhydb_internal_tests))]
pub(crate) fn set_page_bytes_for_tests(n: usize) {
    TEST_PAGE_BYTES.with(|c| c.set(n));
}

#[cfg(test)]
thread_local! {
    /// Tests simulate a writer error: fences of leaves after the first are
    /// written one byte too large (RA2-05, deep verify must notice).
    static TEST_BAD_FENCE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
/// Highest tree level accepted by reader and writer.
const MAX_HEIGHT: u64 = 64;
/// Postings up to this many raw bytes are stored inline in the leaf.
const INLINE_MAX: usize = 256;
/// Target size of a posting chunk block (a larger posting gets its own block).
const CHUNK_BYTES: usize = 1 << 20;

// ----------------------------------------------------------------------
// Key encoding and order
// ----------------------------------------------------------------------

const K_NONE: u8 = 0x00;
const K_FALSE: u8 = 0x01;
const K_TRUE: u8 = 0x02;
const K_INT: u8 = 0x03;
const K_BIG: u8 = 0x04;
const K_FLOAT: u8 = 0x05;
const K_STR: u8 = 0x06;
const K_BYTES: u8 = 0x07;
const K_OTHER: u8 = 0x0F;

/// Binary key encoding (tag byte + value): scalars compact, everything else
/// as JSON (`json_key_encode`).
pub fn encode_key(v: &Val, out: &mut Vec<u8>) -> Result<()> {
    match v {
        Val::None => out.push(K_NONE),
        Val::Bool(false) => out.push(K_FALSE),
        Val::Bool(true) => out.push(K_TRUE),
        Val::Int(i) => {
            out.push(K_INT);
            put_vu64(out, ((*i << 1) ^ (*i >> 63)) as u64);
        }
        Val::Big(b) => {
            out.push(K_BIG);
            out.extend_from_slice(&b.to_signed_bytes_le());
        }
        Val::Float(f) => {
            out.push(K_FLOAT);
            out.extend_from_slice(&f.to_bits().to_le_bytes());
        }
        Val::Str(s) => {
            out.push(K_STR);
            out.extend_from_slice(s.as_bytes());
        }
        Val::Bytes(b) => {
            out.push(K_BYTES);
            out.extend_from_slice(b);
        }
        other => {
            out.push(K_OTHER);
            out.extend_from_slice(json_key_encode(other)?.as_bytes());
        }
    }
    Ok(())
}

pub fn encode_key_vec(v: &Val) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    encode_key(v, &mut out)?;
    Ok(out)
}

pub fn decode_key(raw: &[u8]) -> Result<Val> {
    let bad = || Error::Corruption("SECONDARY key encoding invalid".into());
    let (&tag, rest) = raw.split_first().ok_or_else(bad)?;
    Ok(match tag {
        K_NONE if rest.is_empty() => Val::None,
        K_FALSE if rest.is_empty() => Val::Bool(false),
        K_TRUE if rest.is_empty() => Val::Bool(true),
        K_INT => {
            let mut p = 0;
            let z = get_vu_u64(rest, &mut p, "SECONDARY key")?;
            if p != rest.len() {
                return Err(bad());
            }
            Val::Int(((z >> 1) as i64) ^ -((z & 1) as i64))
        }
        K_BIG if !rest.is_empty() => Val::Big(BigInt::from_signed_bytes_le(rest)),
        K_FLOAT if rest.len() == 8 => Val::Float(f64::from_bits(u64::from_le_bytes(rest.try_into().unwrap()))),
        K_STR => Val::Str(std::str::from_utf8(rest).map_err(|_| bad())?.to_string()),
        K_BYTES => Val::Bytes(rest.to_vec()),
        K_OTHER => json_key_decode(rest)?,
        _ => return Err(bad()),
    })
}

/// (class, subclass) of an encoded key: 0 numbers, 1 `str`, 2 `bytes`,
/// 3 others (subclass 0 None, 1 NaN, 2 rest).
fn class_of(raw: &[u8]) -> (u8, u8) {
    match raw.first() {
        Some(&(K_FALSE | K_TRUE | K_INT | K_BIG)) => (0, 0),
        Some(&K_FLOAT) => {
            let nan = raw.get(1..9).map(|b| f64::from_bits(u64::from_le_bytes(b.try_into().unwrap())).is_nan()).unwrap_or(true);
            if nan { (3, 1) } else { (0, 0) }
        }
        Some(&K_STR) => (1, 0),
        Some(&K_BYTES) => (2, 0),
        Some(&K_NONE) => (3, 0),
        _ => (3, 2),
    }
}

fn num_of(raw: &[u8]) -> Option<Num> {
    match decode_key(raw).ok()? {
        v @ (Val::Bool(_) | Val::Int(_) | Val::Big(_) | Val::Float(_)) => v.as_number(),
        _ => None,
    }
}

fn int_of(raw: &[u8]) -> Option<i64> {
    match raw.first() {
        Some(&K_INT) => {
            let mut p = 0;
            let z = get_vu_u64(&raw[1..], &mut p, "").ok()?;
            Some(((z >> 1) as i64) ^ -((z & 1) as i64))
        }
        Some(&K_FALSE) => Some(0),
        Some(&K_TRUE) => Some(1),
        _ => None,
    }
}

/// Total order of encoded keys (see the module documentation).
pub fn cmp_keys(a: &[u8], b: &[u8]) -> Ordering {
    let (ca, cb) = (class_of(a), class_of(b));
    if ca != cb {
        return ca.cmp(&cb);
    }
    match ca.0 {
        0 => {
            if let (Some(x), Some(y)) = (int_of(a), int_of(b)) {
                return x.cmp(&y);
            }
            match (num_of(a), num_of(b)) {
                (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
                _ => a.cmp(b),
            }
        }
        _ => a[1.min(a.len())..].cmp(&b[1.min(b.len())..]),
    }
}

/// The same order on values (sorting the build without encoding every key).
pub fn cmp_vals(a: &Val, b: &Val) -> Ordering {
    fn cls(v: &Val) -> (u8, u8) {
        match v {
            Val::Float(f) if f.is_nan() => (3, 1),
            Val::Bool(_) | Val::Int(_) | Val::Big(_) | Val::Float(_) => (0, 0),
            Val::Str(_) => (1, 0),
            Val::Bytes(_) => (2, 0),
            Val::None => (3, 0),
            _ => (3, 2),
        }
    }
    let (ca, cb) = (cls(a), cls(b));
    if ca != cb {
        return ca.cmp(&cb);
    }
    match (a, b) {
        (Val::Int(x), Val::Int(y)) => x.cmp(y),
        (Val::Str(x), Val::Str(y)) => x.as_bytes().cmp(y.as_bytes()),
        (Val::Bytes(x), Val::Bytes(y)) => x.cmp(y),
        _ if ca.0 == 0 => match (a.as_number(), b.as_number()) {
            (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
            _ => Ordering::Equal,
        },
        _ => match (encode_key_vec(a), encode_key_vec(b)) {
            (Ok(x), Ok(y)) => x[1..].cmp(&y[1..]),
            _ => Ordering::Equal,
        },
    }
}

/// Range class of an encoded key (0 numbers, 1 `str`, 2 `bytes`, 3 rest).
pub fn range_class_of(raw: &[u8]) -> u8 {
    class_of(raw).0
}

pub fn is_container_key(raw: &[u8]) -> bool {
    class_of(raw) == (3, 2)
}

// ----------------------------------------------------------------------
// Pages
// ----------------------------------------------------------------------

/// Location of a posting list.
#[derive(Clone, Debug)]
pub enum Loc {
    /// Raw posting bytes stored in the leaf.
    Inline(Vec<u8>),
    /// zlib-compressed bytes in a chunk block: (block offset, block id,
    /// payload-relative offset, compressed length).
    Chunk { off: u64, bid: u64, rel: u64, clen: u64 },
}

/// One leaf entry (owned copy).
#[derive(Clone, Debug)]
pub struct SecEntry {
    pub key: Vec<u8>,
    pub raw_count: u64,
    pub width: u8,
    pub loc: Loc,
}

impl SecEntry {
    pub fn value(&self) -> Result<Val> {
        decode_key(&self.key)
    }
}

/// Decoded page: the uncompressed body plus entry offsets.
pub(crate) struct SecPage {
    level: u64,
    body: Vec<u8>,
    /// start of each entry in `body`
    offs: Vec<u32>,
    /// leaf: posting chunk blocks (offset, block id) referenced by index
    chunks: Vec<(u64, u64)>,
}

impl SecPage {
    pub(crate) fn cost(&self) -> usize {
        self.body.capacity() + self.offs.capacity() * 4 + self.chunks.capacity() * 16 + 96
    }

    fn len(&self) -> usize {
        self.offs.len()
    }

    /// Key bytes of entry `i` and the position behind them.
    fn key_at(&self, i: usize) -> Result<(&[u8], usize)> {
        let mut p = self.offs[i] as usize;
        let klen = get_vu_u64(&self.body, &mut p, "SECONDARY key")? as usize;
        let k = self.body.get(p..p + klen).ok_or_else(|| Error::Corruption("SECONDARY page key outside the page".into()))?;
        Ok((k, p + klen))
    }

    /// Index of the first entry with key >= q (`len()` if none).
    fn lower_bound(&self, q: &[u8]) -> Result<usize> {
        let (mut lo, mut hi) = (0usize, self.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if cmp_keys(self.key_at(mid)?.0, q) == Ordering::Less {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Ok(lo)
    }

    /// Inner page: index of the child that may hold `q` (last fence <= q).
    fn child_for(&self, q: &[u8]) -> Result<usize> {
        let (mut lo, mut hi) = (0usize, self.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if cmp_keys(self.key_at(mid)?.0, q) != Ordering::Greater {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Ok(lo.saturating_sub(1))
    }

    fn child(&self, i: usize) -> Result<(u64, u64)> {
        let (_, mut p) = self.key_at(i)?;
        let off = get_vu_u64(&self.body, &mut p, "SECONDARY child")?;
        let bid = get_vu_u64(&self.body, &mut p, "SECONDARY child")?;
        Ok((off, bid))
    }

    fn entry(&self, i: usize) -> Result<SecEntry> {
        let (k, mut p) = self.key_at(i)?;
        let key = k.to_vec();
        let b = &self.body;
        let raw_count = get_vu_u64(b, &mut p, "SECONDARY count")?;
        let width = *b.get(p).ok_or_else(|| Error::Corruption("SECONDARY width missing".into()))?;
        let kind = *b.get(p + 1).ok_or_else(|| Error::Corruption("SECONDARY location missing".into()))?;
        p += 2;
        if !matches!(
            width,
            0 | 4 | 8
                | crate::textindex::PAYLOAD_FULLTEXT_POSITIONS
                | crate::textindex::PAYLOAD_FULLTEXT_DOCLENS
                | crate::textindex::PAYLOAD_FULLTEXT_DOCLEN_SUMMARY
                | crate::textindex::PAYLOAD_FULLTEXT_DOCLEN_SEGMENT
        ) {
            return Err(Error::Corruption("SECONDARY payload codec invalid".into()));
        }
        let loc = match kind {
            0 => {
                let n = get_vu_u64(b, &mut p, "SECONDARY inline")? as usize;
                Loc::Inline(b.get(p..p + n).ok_or_else(|| Error::Corruption("SECONDARY inline posting outside the page".into()))?.to_vec())
            }
            1 => {
                let ci = get_vu_u64(b, &mut p, "SECONDARY chunk")? as usize;
                let rel = get_vu_u64(b, &mut p, "SECONDARY rel")?;
                let clen = get_vu_u64(b, &mut p, "SECONDARY clen")?;
                let &(off, bid) = self.chunks.get(ci).ok_or_else(|| Error::Corruption("SECONDARY chunk index invalid".into()))?;
                Loc::Chunk { off, bid, rel, clen }
            }
            _ => return Err(Error::Corruption("SECONDARY location kind invalid".into())),
        };
        Ok(SecEntry { key, raw_count, width, loc })
    }
}

/// Parses a page body (`level`, entry offsets; leaf: chunk table first).
fn parse_body(level: u64, body: Vec<u8>) -> Result<SecPage> {
    let mut p = 0usize;
    let mut chunks = Vec::new();
    if level == 0 {
        let m = get_vu_u64(&body, &mut p, "SECONDARY chunks")?;
        for _ in 0..m {
            let off = get_vu_u64(&body, &mut p, "SECONDARY chunk")?;
            let bid = get_vu_u64(&body, &mut p, "SECONDARY chunk")?;
            chunks.push((off, bid));
        }
    }
    let n = get_vu_u64(&body, &mut p, "SECONDARY entries")?;
    let mut offs = Vec::with_capacity((n as usize).min(body.len()));
    for _ in 0..n {
        offs.push(u32::try_from(p).map_err(|_| Error::Corruption("SECONDARY page too large".into()))?);
        let klen = get_vu_u64(&body, &mut p, "SECONDARY key")? as usize;
        p = p.checked_add(klen).filter(|e| *e <= body.len()).ok_or_else(|| Error::Corruption("SECONDARY key outside the page".into()))?;
        if level == 0 {
            get_vu_u64(&body, &mut p, "SECONDARY count")?;
            let kind = *body.get(p + 1).ok_or_else(|| Error::Corruption("SECONDARY entry incomplete".into()))?;
            p += 2;
            if kind == 0 {
                let len = get_vu_u64(&body, &mut p, "SECONDARY inline")? as usize;
                p = p.checked_add(len).filter(|e| *e <= body.len()).ok_or_else(|| Error::Corruption("SECONDARY inline posting outside the page".into()))?;
            } else {
                for _ in 0..3 {
                    get_vu_u64(&body, &mut p, "SECONDARY chunk ref")?;
                }
            }
        } else {
            get_vu_u64(&body, &mut p, "SECONDARY child")?;
            get_vu_u64(&body, &mut p, "SECONDARY child")?;
        }
    }
    if p != body.len() {
        return Err(Error::Corruption("SECONDARY page trailing bytes".into()));
    }
    Ok(SecPage { level, body, offs, chunks })
}

fn inflate(comp: &[u8], raw_len: u64) -> Result<Vec<u8>> {
    let raw = crate::zlib::decompress_with_limit(comp, usize::try_from(raw_len).unwrap_or(usize::MAX))
        .map_err(|e| Error::Corruption(format!("SECONDARY page zlib-corrupt: {e}")))?;
    if raw.len() as u64 != raw_len {
        return Err(Error::Corruption("SECONDARY page length mismatch".into()));
    }
    Ok(raw)
}

/// Root fields after the tag: index id, source block, key count, height.
pub(crate) struct RootHead {
    pub index_id: u64,
    pub source_bid: u64,
    pub entry_count: u64,
    pub height: u64,
    /// position behind the head (raw length + compressed top page follow)
    pub pos: usize,
}

pub(crate) fn parse_root_head(raw: &[u8]) -> Result<RootHead> {
    let mut p = 4;
    let index_id = get_vu_u64(raw, &mut p, "SECONDARY index")?;
    let source_bid = get_vu_u64(raw, &mut p, "SECONDARY source")?;
    let entry_count = get_vu_u64(raw, &mut p, "SECONDARY entries")?;
    let height = get_vu_u64(raw, &mut p, "SECONDARY height")?;
    if height > MAX_HEIGHT {
        return Err(Error::Corruption("SECONDARY tree height invalid".into()));
    }
    Ok(RootHead { index_id, source_bid, entry_count, height, pos: p })
}

// ----------------------------------------------------------------------
// Writer
// ----------------------------------------------------------------------

/// One level of the bulk-loaded tree: the page being filled and a finished
/// page held back until the next one (the last page of the top level is
/// embedded in the root instead of being written).
#[derive(Default)]
struct LevelW {
    body: Vec<u8>,
    n: u64,
    first: Option<Vec<u8>>,
    /// finished page (first key, encoded body incl. count) not written yet
    held: Option<(Vec<u8>, Vec<u8>)>,
    /// pages of this level written so far
    written: u64,
}

struct PendingLeafEntry {
    key: Vec<u8>,
    count: u64,
    width: u8,
    inline: Option<Vec<u8>>,
    /// (chunk index in the leaf, rel, clen)
    chunk: (usize, u64, u64),
}

/// Streaming writer of one index tree (keys must arrive in `cmp_keys` order).
pub(crate) struct SecWriter {
    index_id: u64,
    lane: Lane,
    levels: Vec<LevelW>,
    leaf: Vec<PendingLeafEntry>,
    leaf_bytes: usize,
    leaf_chunks: Vec<(u64, u64)>,
    chunk_buf: Vec<u8>,
    chunk_head: usize,
    entries: u64,
    last_key: Option<Vec<u8>>,
    pub bytes_written: u64,
    pub blocks_written: u64,
}

impl SecWriter {
    pub(crate) fn new(index_id: u64, lane: Lane) -> SecWriter {
        let mut head = CHUNK_TAG.to_vec();
        put_vu64(&mut head, index_id);
        SecWriter {
            index_id,
            lane,
            levels: vec![LevelW::default()],
            leaf: Vec::new(),
            leaf_bytes: 0,
            leaf_chunks: Vec::new(),
            chunk_head: head.len(),
            chunk_buf: head,
            entries: 0,
            last_key: None,
            bytes_written: 0,
            blocks_written: 0,
        }
    }

    fn page_payload(&self, tag: &[u8; 4], level: u64, body: &[u8]) -> Vec<u8> {
        let mut out = tag.to_vec();
        put_vu64(&mut out, self.index_id);
        put_vu64(&mut out, level);
        put_vu64(&mut out, body.len() as u64);
        out.extend_from_slice(&crate::zlib::compress(body, 1));
        out
    }

    fn append(&mut self, db: &mut Db, payload: &[u8]) -> Result<(u64, u64)> {
        let r = db.append_block(BT_SECONDARY_PAGE, payload, lane_flags(self.lane))?;
        self.bytes_written += payload.len() as u64;
        self.blocks_written += 1;
        Ok(r)
    }

    fn flush_chunk(&mut self, db: &mut Db) -> Result<()> {
        if self.chunk_buf.len() > self.chunk_head {
            let buf = std::mem::take(&mut self.chunk_buf);
            let r = self.append(db, &buf)?;
            self.leaf_chunks.push(r);
            self.chunk_buf = buf;
            self.chunk_buf.truncate(self.chunk_head);
        }
        Ok(())
    }

    /// Adds a key with its raw posting bytes.
    pub(crate) fn add(&mut self, db: &mut Db, key: Vec<u8>, count: u64, width: u8, raw: &[u8]) -> Result<()> {
        if let Some(prev) = &self.last_key {
            let o = cmp_keys(prev, &key);
            if o == Ordering::Greater || (o == Ordering::Equal && !is_container_key(&key)) {
                return Err(Error::Value("SECONDARY keys must arrive in ascending order".into()));
            }
        }
        let (inline, chunk) = if raw.len() <= INLINE_MAX {
            (Some(raw.to_vec()), (0, 0, 0))
        } else {
            let comp = crate::zlib::compress(raw, 1);
            if self.chunk_buf.len() > self.chunk_head && self.chunk_buf.len() + comp.len() > CHUNK_BYTES {
                self.flush_chunk(db)?;
            }
            let rel = self.chunk_buf.len() as u64;
            self.chunk_buf.extend_from_slice(&comp);
            ((None), (self.leaf_chunks.len(), rel, comp.len() as u64))
        };
        self.leaf_bytes += key.len() + 24 + inline.as_ref().map(|v| v.len()).unwrap_or(0);
        self.last_key = Some(key.clone());
        self.leaf.push(PendingLeafEntry { key, count, width, inline, chunk });
        self.entries += 1;
        if self.leaf_bytes >= page_bytes() {
            self.finish_leaf(db)?;
        }
        Ok(())
    }

    fn finish_leaf(&mut self, db: &mut Db) -> Result<()> {
        if self.leaf.is_empty() {
            return Ok(());
        }
        self.flush_chunk(db)?;
        let mut body = Vec::with_capacity(self.leaf_bytes + 16);
        put_vu64(&mut body, self.leaf_chunks.len() as u64);
        for (off, bid) in &self.leaf_chunks {
            put_vu64(&mut body, *off);
            put_vu64(&mut body, *bid);
        }
        put_vu64(&mut body, self.leaf.len() as u64);
        for e in &self.leaf {
            put_vu64(&mut body, e.key.len() as u64);
            body.extend_from_slice(&e.key);
            put_vu64(&mut body, e.count);
            body.push(e.width);
            match &e.inline {
                Some(raw) => {
                    body.push(0);
                    put_vu64(&mut body, raw.len() as u64);
                    body.extend_from_slice(raw);
                }
                None => {
                    body.push(1);
                    put_vu64(&mut body, e.chunk.0 as u64);
                    put_vu64(&mut body, e.chunk.1);
                    put_vu64(&mut body, e.chunk.2);
                }
            }
        }
        let first = self.leaf[0].key.clone();
        self.leaf.clear();
        self.leaf_chunks.clear();
        self.leaf_bytes = 0;
        self.push_page(db, 0, first, body)
    }

    /// A finished page of `level`: the held page before it is written and
    /// its fence goes up one level.
    fn push_page(&mut self, db: &mut Db, level: usize, first: Vec<u8>, body: Vec<u8>) -> Result<()> {
        if let Some((hf, hb)) = self.levels[level].held.take() {
            self.write_page(db, level, hf, hb)?;
        }
        self.levels[level].held = Some((first, body));
        Ok(())
    }

    fn write_page(&mut self, db: &mut Db, level: usize, first: Vec<u8>, body: Vec<u8>) -> Result<()> {
        if level + 1 >= MAX_HEIGHT as usize {
            return Err(Error::Value("SECONDARY tree would exceed the maximum height".into()));
        }
        let tag = if level == 0 { LEAF_TAG } else { INNER_TAG };
        let payload = self.page_payload(tag, level as u64, &body);
        let (off, bid) = self.append(db, &payload)?;
        self.levels[level].written += 1;
        if self.levels.len() == level + 1 {
            self.levels.push(LevelW::default());
        }
        #[cfg(test)]
        let first = {
            let mut first = first;
            if level == 0 && self.levels[0].written > 1 && TEST_BAD_FENCE.with(|c| c.get()) {
                *first.last_mut().unwrap() += 1;
            }
            first
        };
        let up = &mut self.levels[level + 1];
        if up.first.is_none() {
            up.first = Some(first.clone());
        }
        put_vu64(&mut up.body, first.len() as u64);
        up.body.extend_from_slice(&first);
        put_vu64(&mut up.body, off);
        put_vu64(&mut up.body, bid);
        up.n += 1;
        // An inner page closes only with at least two children: every level
        // then has at most half the pages of the one below and the tree
        // terminates even when fence keys exceed the page size (RA2-01).
        if up.n >= 2 && up.body.len() >= page_bytes() {
            self.finish_inner(db, level + 1)?;
        }
        Ok(())
    }

    fn finish_inner(&mut self, db: &mut Db, level: usize) -> Result<()> {
        let l = &mut self.levels[level];
        if l.n == 0 {
            return Ok(());
        }
        let mut body = Vec::with_capacity(l.body.len() + 8);
        put_vu64(&mut body, l.n);
        body.extend_from_slice(&l.body);
        let first = l.first.take().unwrap_or_default();
        l.body.clear();
        l.n = 0;
        self.push_page(db, level, first, body)
    }

    /// Writes the remaining pages and returns the root payload (written by
    /// the caller as the SECONDARY block of the index) with the page bytes
    /// and blocks written.
    pub(crate) fn finish(mut self, db: &mut Db, source_bid: u64) -> Result<(Vec<u8>, u64, u64)> {
        let root = self.finish_tree(db, source_bid)?;
        Ok((root, self.bytes_written, self.blocks_written))
    }

    fn finish_tree(&mut self, db: &mut Db, source_bid: u64) -> Result<Vec<u8>> {
        self.finish_leaf(db)?;
        let mut level = 0usize;
        let (height, top) = loop {
            if level > 0 {
                self.finish_inner(db, level)?;
            }
            let is_top = self.levels[level].written == 0 && self.levels.len() == level + 1;
            let held = self.levels[level].held.take();
            match held {
                Some((_, body)) if is_top => break (level as u64, body),
                Some((first, body)) => self.write_page(db, level, first, body)?,
                None if is_top => {
                    // empty index: an empty leaf
                    let mut body = Vec::new();
                    put_vu64(&mut body, 0);
                    put_vu64(&mut body, 0);
                    break (0, body);
                }
                None => {}
            }
            level += 1;
        };
        let mut root = ROOT_TAG.to_vec();
        put_vu64(&mut root, self.index_id);
        put_vu64(&mut root, source_bid);
        put_vu64(&mut root, self.entries);
        put_vu64(&mut root, height);
        put_vu64(&mut root, top.len() as u64);
        root.extend_from_slice(&crate::zlib::compress(&top, 1));
        Ok(root)
    }
}

// ----------------------------------------------------------------------
// Reading
// ----------------------------------------------------------------------

impl Db {
    fn sec_page_cached(&self, off: u64) -> Option<Arc<SecPage>> {
        self.cache().sec_pages.get(&off)
    }

    fn sec_page_store(&self, off: u64, page: Arc<SecPage>) {
        let cost = page.cost();
        self.cache().sec_pages.insert(off, page, cost, self.config.index_page_cache_bytes);
    }

    /// Top page of the tree (embedded in the root block).
    fn sec_top(&self, meta: &SecondaryMeta, keep: bool) -> Result<Arc<SecPage>> {
        if let Some(p) = self.sec_page_cached(meta.block_offset) {
            return Ok(p);
        }
        let (info, payload) = self.read_block(meta.block_offset)?;
        if info.block_type != BT_SECONDARY || info.block_id != meta.block_id {
            return Err(Error::Corruption(format!("SECONDARY block {}: header changed", meta.block_id)));
        }
        let h = parse_root_head(&payload)?;
        // RA2-05: every root field read without CRC on open is compared
        // on first use (freshness depends on the source block)
        if !payload.starts_with(ROOT_TAG)
            || h.index_id != meta.index_id
            || h.entry_count != meta.entry_count
            || h.source_bid != meta.source_block_id
            || h.height != meta.height
        {
            return Err(Error::Corruption(format!("SECONDARY block {}: root changed", meta.block_id)));
        }
        let mut p = h.pos;
        let raw_len = get_vu_u64(&payload, &mut p, "SECONDARY top")?;
        let page = Arc::new(parse_body(h.height, inflate(&payload[p..], raw_len)?)?);
        self.cache().sec_reads += 1;
        if keep {
            self.sec_page_store(meta.block_offset, page.clone());
        }
        Ok(page)
    }

    /// Child page `(off, bid)` of an inner page at `level`.
    fn sec_child(&self, meta: &SecondaryMeta, off: u64, bid: u64, level: u64, keep: bool) -> Result<Arc<SecPage>> {
        if let Some(p) = self.sec_page_cached(off) {
            if p.level == level {
                return Ok(p);
            }
        }
        let bad = |what: &str| Error::Corruption(format!("SECONDARY page at byte {off} (index {}): {what}", meta.index_id));
        if off < DATA_START || off >= meta.block_offset {
            return Err(bad("outside the index"));
        }
        let (info, payload) = self.read_block(off)?;
        let tag = if level == 0 { LEAF_TAG } else { INNER_TAG };
        if info.block_type != BT_SECONDARY_PAGE || info.block_id != bid || lane_from_flags(info.flags) != meta.lane || !payload.starts_with(tag) {
            return Err(bad("does not match its parent entry"));
        }
        let mut p = 4;
        let iid = get_vu_u64(&payload, &mut p, "SECONDARY page")?;
        let lvl = get_vu_u64(&payload, &mut p, "SECONDARY page")?;
        let raw_len = get_vu_u64(&payload, &mut p, "SECONDARY page")?;
        if iid != meta.index_id || lvl != level {
            return Err(bad("belongs to another index or level"));
        }
        let page = Arc::new(parse_body(level, inflate(&payload[p..], raw_len)?)?);
        self.cache().sec_reads += 1;
        if keep {
            self.sec_page_store(off, page.clone());
        }
        Ok(page)
    }

    /// Path from the top page to the leaf that may contain `q` (`None`:
    /// the leftmost leaf). Each element: (page, index in the page).
    fn sec_descend(&self, meta: &SecondaryMeta, q: Option<&[u8]>, keep: bool) -> Result<Vec<(Arc<SecPage>, usize)>> {
        let mut path = Vec::new();
        let mut page = self.sec_top(meta, keep)?;
        while page.level > 0 {
            if page.len() == 0 {
                return Err(Error::Corruption("SECONDARY inner page empty".into()));
            }
            let i = match q {
                Some(q) => page.child_for(q)?,
                None => 0,
            };
            let (off, bid) = page.child(i)?;
            let next = self.sec_child(meta, off, bid, page.level - 1, keep)?;
            path.push((page, i));
            page = next;
        }
        let i = match q {
            Some(q) => page.lower_bound(q)?,
            None => 0,
        };
        path.push((page, i));
        Ok(path)
    }

    /// Entry with key equal to `q` (under the key order).
    pub(crate) fn sec_lookup(&self, index_id: u64, lane: Lane, q: &[u8]) -> Result<Option<SecEntry>> {
        let Some(meta) = self.secondary_blocks.get(&(index_id, lane)) else {
            return Ok(None);
        };
        let mut found = None;
        self.sec_scan_meta(meta, Some(q), true, &mut |e| {
            if cmp_keys(&e.key, q) == Ordering::Equal {
                found = Some(e.clone());
            }
            Ok(false)
        })?;
        Ok(found)
    }

    /// [`Db::sec_lookup`] without putting pages into the page cache.
    pub(crate) fn sec_lookup_uncached(&self, index_id: u64, lane: Lane, q: &[u8]) -> Result<Option<SecEntry>> {
        let Some(meta) = self.secondary_blocks.get(&(index_id, lane)) else {
            return Ok(None);
        };
        let mut found = None;
        self.sec_scan_meta(meta, Some(q), false, &mut |e| {
            if cmp_keys(&e.key, q) == Ordering::Equal {
                found = Some(e.clone());
            }
            Ok(false)
        })?;
        Ok(found)
    }

    /// Visits the entries with key >= `from` (all with `None`) in key order
    /// until `f` returns `false`. Memory: one page per tree level.
    pub(crate) fn sec_scan(&self, index_id: u64, lane: Lane, from: Option<&[u8]>, f: &mut dyn FnMut(&SecEntry) -> Result<bool>) -> Result<()> {
        match self.secondary_blocks.get(&(index_id, lane)) {
            Some(meta) => self.sec_scan_meta(meta, from, true, f),
            None => Ok(()),
        }
    }

    /// `keep`: decoded pages go into the page cache (false for one-time
    /// walks such as `verify(deep=True)`, which must not evict the pages of
    /// regular reads).
    fn sec_scan_meta(&self, meta: &SecondaryMeta, from: Option<&[u8]>, keep: bool, f: &mut dyn FnMut(&SecEntry) -> Result<bool>) -> Result<()> {
        let mut path = self.sec_descend(meta, from, keep)?;
        loop {
            {
                let (leaf, i) = path.last_mut().unwrap();
                while *i < leaf.len() {
                    let e = leaf.entry(*i)?;
                    *i += 1;
                    if !f(&e)? {
                        return Ok(());
                    }
                }
            }
            // next leaf: climb to the first ancestor with a further child
            path.pop();
            loop {
                let Some((page, i)) = path.last_mut() else { return Ok(()) };
                *i += 1;
                if *i < page.len() {
                    break;
                }
                path.pop();
            }
            // descend along the leftmost children
            loop {
                let (page, i) = path.last().unwrap();
                let (off, bid) = page.child(*i)?;
                let next = self.sec_child(meta, off, bid, page.level - 1, keep)?;
                let leaf = next.level == 0;
                path.push((next, 0));
                if leaf {
                    break;
                }
            }
        }
    }

    /// Raw (uncompressed) posting bytes of an entry.
    pub(crate) fn sec_posting_raw(&self, _index_id: u64, lane: Lane, e: &SecEntry) -> Result<Vec<u8>> {
        match &e.loc {
            Loc::Inline(raw) => Ok(raw.clone()),
            Loc::Chunk { off, bid, rel, clen } => {
                let rel = usize::try_from(*rel).map_err(|_| Error::Corruption("SECONDARY posting offset".into()))?;
                let clen = usize::try_from(*clen).map_err(|_| Error::Corruption("SECONDARY posting length".into()))?;
                let comp = self.block_range(*off, rel, clen)?;
                let (bt, b, flags, _) = self.block_header_of(*off)?;
                if bt != BT_SECONDARY_PAGE || b != *bid || lane_from_flags(flags) != lane {
                    return Err(Error::Corruption(format!("SECONDARY posting chunk at byte {off}: does not match its leaf entry")));
                }
                // R010: `decompress` is bounded by deflate's maximum ratio
                crate::zlib::decompress(&comp).map_err(|e| Error::Corruption(format!("Index payload zlib-corrupt: {e}")))
            }
        }
    }

    /// Posting of a document-u entry (codec 0/4/8) as `Nat` list.
    pub(crate) fn sec_posting_us(&self, index_id: u64, lane: Lane, e: &SecEntry) -> Result<Vec<Nat>> {
        if matches!(
            e.width,
            crate::textindex::PAYLOAD_FULLTEXT_POSITIONS
                | crate::textindex::PAYLOAD_FULLTEXT_DOCLENS
                | crate::textindex::PAYLOAD_FULLTEXT_DOCLEN_SUMMARY
                | crate::textindex::PAYLOAD_FULLTEXT_DOCLEN_SEGMENT
        ) {
            return Err(Error::Schema("typed FULLTEXT payload is not a plain document ID posting list".into()));
        }
        let raw = self.sec_posting_raw(index_id, lane, e)?;
        let list: Vec<Nat> = match e.width {
            0 => decode_big_posting(&raw, e.raw_count)?,
            4 => raw.as_chunks::<4>().0.iter().map(|c| Nat::S(u32::from_le_bytes(*c) as u64)).collect(),
            _ => raw.as_chunks::<8>().0.iter().map(|c| Nat::S(u64::from_le_bytes(*c))).collect(),
        };
        if list.len() as u64 != e.raw_count || (e.width != 0 && raw.len() != list.len() * e.width as usize) {
            return Err(Error::Corruption("Posting count mismatch".into()));
        }
        Ok(list)
    }

    /// Whether the top page of an index is in the page cache.
    pub(crate) fn sec_top_cached(&self, index_id: u64, lane: Lane) -> bool {
        match self.secondary_blocks.get(&(index_id, lane)) {
            Some(m) => self.cache().sec_pages.contains(&m.block_offset),
            None => false,
        }
    }

    /// All entries as a directory (`_ensure_secondary_directory`, a debug
    /// view: memory proportional to the keys of the index).
    pub(crate) fn sec_full_directory(&self, index_id: u64, lane: Lane) -> Result<SecondaryDir> {
        let mut dir = SecondaryDir::default();
        self.sec_scan(index_id, lane, None, &mut |e| {
            if e.width == crate::textindex::PAYLOAD_FULLTEXT_DOCLEN_SEGMENT {
                return Ok(true); // document length segments are no keys
            }
            let (abs_offset, compressed_len) = match &e.loc {
                Loc::Inline(r) => (0, r.len() as u64),
                Loc::Chunk { off, rel, clen, .. } => (off + BLOCK_HEADER_SIZE as u64 + rel, *clen),
            };
            let value = e.value()?;
            dir.by_key.insert(crate::val::ValKey(value.clone()), dir.entries.len());
            // the summary of the paged document lengths appears as the
            // single document length entry of 0.28.0 (same key and count)
            let width = if e.width == crate::textindex::PAYLOAD_FULLTEXT_DOCLEN_SUMMARY { crate::textindex::PAYLOAD_FULLTEXT_DOCLENS } else { e.width };
            dir.entries.push(PostingRef { value, abs_offset, compressed_len, raw_count: e.raw_count, width });
            Ok(true)
        })?;
        Ok(dir)
    }

    /// Deep check of the tree of an index: binding of every page, key order
    /// across all leaves, key count, posting counts. Streaming (one page per
    /// level).
    /// RA2-05: every fence key of an inner page equals the first key of its
    /// child, and the children of a page are in key order.
    fn sec_verify_fences(&self, meta: &SecondaryMeta, page: &SecPage) -> Result<Option<String>> {
        if page.level == 0 {
            return Ok(None);
        }
        for i in 0..page.len() {
            let (fence, _) = page.key_at(i)?;
            let (off, bid) = page.child(i)?;
            let child = self.sec_child(meta, off, bid, page.level - 1, false)?;
            if child.len() == 0 {
                return Ok(Some(format!("empty page at byte {off}")));
            }
            if child.key_at(0)?.0 != fence {
                return Ok(Some(format!("fence key of the page at byte {off} differs from its first key")));
            }
            if i + 1 < page.len() {
                let next = page.key_at(i + 1)?.0;
                let last = child.key_at(child.len() - 1)?.0;
                if cmp_keys(last, next) == Ordering::Greater {
                    return Ok(Some(format!("page at byte {off} holds keys behind the next fence")));
                }
            }
            if let Some(e) = self.sec_verify_fences(meta, &child)? {
                return Ok(Some(e));
            }
        }
        Ok(None)
    }

    pub(crate) fn sec_verify_tree(&self, index_id: u64, lane: Lane) -> Result<u64> {
        let Some(meta) = self.secondary_blocks.get(&(index_id, lane)) else {
            return Ok(0);
        };
        let top = self.sec_top(meta, false)?;
        if let Some(e) = self.sec_verify_fences(meta, &top)? {
            return Err(Error::Corruption(format!("SECONDARY index {index_id}: {e}")));
        }
        let mut prev: Option<Vec<u8>> = None;
        let mut n = 0u64;
        let mut err: Option<String> = None;
        // FULLTEXT document lengths: summary vs. the sum of the segments
        let mut summary: Option<(u64, u64, u64)> = None;
        let mut seg_sum = (0u64, 0u64, 0u64);
        let mut seg_last: Option<Nat> = None;
        self.sec_scan_meta(meta, None, false, &mut |e| {
            if let Some(p) = &prev {
                let o = cmp_keys(p, &e.key);
                if o == Ordering::Greater || (o == Ordering::Equal && !is_container_key(&e.key)) {
                    err = Some("keys out of order".into());
                    return Ok(false);
                }
            }
            decode_key(&e.key)?;
            match e.width {
                crate::textindex::PAYLOAD_FULLTEXT_POSITIONS => {
                    crate::textindex::decode_fulltext_posting(&self.sec_posting_raw(index_id, lane, e)?, Some(e.raw_count))
                        .map_err(|x| Error::Corruption(format!("FULLTEXT term payload corrupt: {x}")))?;
                }
                crate::textindex::PAYLOAD_FULLTEXT_DOCLENS => {
                    crate::textindex::decode_fulltext_doc_lengths(&self.sec_posting_raw(index_id, lane, e)?, Some(e.raw_count))
                        .map_err(|x| Error::Corruption(format!("FULLTEXT document lengths corrupt: {x}")))?;
                }
                crate::textindex::PAYLOAD_FULLTEXT_DOCLEN_SUMMARY => {
                    let s = crate::textindex::decode_doclen_summary(&self.sec_posting_raw(index_id, lane, e)?, Some(e.raw_count))
                        .map_err(|x| Error::Corruption(format!("FULLTEXT document length summary corrupt: {x}")))?;
                    if summary.replace(s).is_some() {
                        err = Some("two document length summaries".into());
                        return Ok(false);
                    }
                }
                crate::textindex::PAYLOAD_FULLTEXT_DOCLEN_SEGMENT => {
                    let key = match decode_key(&e.key)? {
                        Val::Str(k) => k,
                        _ => String::new(),
                    };
                    let Some(last) = crate::textindex::doclen_segment_last(&key) else {
                        err = Some("document length segment key invalid".into());
                        return Ok(false);
                    };
                    let seg = crate::textindex::decode_doclen_segment(&self.sec_posting_raw(index_id, lane, e)?, Some(e.raw_count))
                        .map_err(|x| Error::Corruption(format!("FULLTEXT document lengths corrupt: {x}")))?;
                    let first_ok = match (seg.us.first(), &seg_last) {
                        (Some(f), Some(p)) => f > p,
                        (Some(_), None) => true,
                        (None, _) => false,
                    };
                    if !first_ok || seg.last() != Some(&last) {
                        err = Some("document length segments out of order or not matching their keys".into());
                        return Ok(false);
                    }
                    seg_sum.0 += seg.us.len() as u64;
                    seg_sum.1 += seg.tokens;
                    seg_sum.2 += 1;
                    seg_last = Some(last);
                }
                0 => {
                    self.sec_posting_us(index_id, lane, e)?;
                }
                w => {
                    // u32/u64 arrays: the length is the check (no list of
                    // all u in memory for a key with millions of documents)
                    let n = self.sec_posting_raw(index_id, lane, e)?.len() as u64;
                    if n != e.raw_count.saturating_mul(w as u64) {
                        return Err(Error::Corruption("Posting count mismatch".into()));
                    }
                }
            }
            prev = Some(e.key.clone());
            n += 1;
            Ok(true)
        })?;
        if err.is_none() {
            match summary {
                Some(s) if s != seg_sum => err = Some(format!("document length summary {s:?} does not match its segments {seg_sum:?}")),
                None if seg_sum.2 > 0 => err = Some("document length segments without summary".into()),
                _ => {}
            }
        }
        if let Some(e) = err {
            return Err(Error::Corruption(format!("SECONDARY index {index_id}: {e}")));
        }
        if n != meta.entry_count {
            return Err(Error::Corruption(format!("SECONDARY index {index_id}: {n} keys, root says {}", meta.entry_count)));
        }
        Ok(n)
    }
}

#[cfg(all(test, nhydb_internal_tests))]
#[path = "secindex_tests.rs"]
mod tests;
