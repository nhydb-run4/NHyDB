//! zlib compression via the system `libz` (identical to Python's
//! `zlib.compress(data, level)`, so index blocks are byte-identical to those
//! produced by CPython). Decompression uses miniz_oxide (standard zlib format).
//!
//! Platforms without a system `libz` (Windows) compress with miniz_oxide.
//! The result is a valid zlib stream (readable everywhere), but the
//! compressed bytes may differ from CPython; the file format does not
//! require byte identity of the compressed postings.

#[cfg(unix)]
use std::os::raw::{c_int, c_ulong};

#[cfg(unix)]
#[link(name = "z")]
unsafe extern "C" {
    fn compressBound(source_len: c_ulong) -> c_ulong;
    fn compress2(dest: *mut u8, dest_len: *mut c_ulong, source: *const u8, source_len: c_ulong, level: c_int) -> c_int;
}

#[cfg(not(unix))]
pub fn compress(data: &[u8], level: i32) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec_zlib(data, level.clamp(0, 10) as u8)
}

#[cfg(unix)]
pub fn compress(data: &[u8], level: i32) -> Vec<u8> {
    unsafe {
        let mut cap = compressBound(data.len() as c_ulong);
        let mut out = vec![0u8; cap as usize];
        let rc = compress2(out.as_mut_ptr(), &mut cap, data.as_ptr(), data.len() as c_ulong, level);
        if rc == 0 {
            out.truncate(cap as usize);
            out
        } else {
            miniz_oxide::deflate::compress_to_vec_zlib(data, level.clamp(0, 10) as u8)
        }
    }
}

/// Largest output deflate can produce from `n` input bytes (ratio < 1032:1).
fn deflate_bound(n: usize) -> usize {
    n.saturating_mul(1032).saturating_add(64)
}

/// Decompression bounded by deflate's theoretical maximum ratio (R010).
pub fn decompress(data: &[u8]) -> Result<Vec<u8>, String> {
    decompress_with_limit(data, deflate_bound(data.len()))
}

/// Decompression with an explicit output limit (e.g. derived from the
/// directory entry); a larger stream is reported as an error instead of
/// allocating without bound (R010).
pub fn decompress_with_limit(data: &[u8], max_out: usize) -> Result<Vec<u8>, String> {
    let max_out = max_out.min(deflate_bound(data.len()));
    miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(data, max_out).map_err(|e| {
        if matches!(e.status, miniz_oxide::inflate::TINFLStatus::HasMoreOutput) {
            format!("decompressed data exceeds the limit of {max_out} bytes")
        } else {
            format!("{e:?}")
        }
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn roundtrip() {
        let d: Vec<u8> = (0..10000u32).flat_map(|x| x.to_le_bytes()).collect();
        let c = super::compress(&d, 1);
        assert!(c.len() < d.len());
        assert_eq!(super::decompress(&c).unwrap(), d);
        assert_eq!(super::decompress_with_limit(&c, d.len()).unwrap(), d);
        assert!(super::decompress_with_limit(&c, d.len() - 1).is_err());
        // "zip bomb": 64 MiB of zeros, limited to 1 MiB → error, no large allocation
        let bomb = super::compress(&vec![0u8; 64 << 20], 9);
        assert!(super::decompress_with_limit(&bomb, 1 << 20).unwrap_err().contains("exceeds"));
        assert!(super::decompress(b"not zlib").is_err());
    }
}
