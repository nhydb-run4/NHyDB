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

pub fn decompress(data: &[u8]) -> Result<Vec<u8>, String> {
    miniz_oxide::inflate::decompress_to_vec_zlib(data).map_err(|e| format!("{e:?}"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn roundtrip() {
        let d: Vec<u8> = (0..10000u32).flat_map(|x| x.to_le_bytes()).collect();
        let c = super::compress(&d, 1);
        assert!(c.len() < d.len());
        assert_eq!(super::decompress(&c).unwrap(), d);
    }
}
