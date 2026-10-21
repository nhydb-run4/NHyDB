//! Build fingerprint (T024): CRC-32 over the Rust sources of the workspace,
//! exported as `nhydb._native.BUILD_SOURCE_HASH`. `tests/python/conftest.py`
//! computes the same value from the checkout and warns if the installed
//! native module was built from a different source state.

use std::fs;
use std::path::{Path, PathBuf};

fn crc32_update(mut crc: u32, data: &[u8]) -> u32 {
    crc = !crc;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// Relative paths (with `/`) of the fingerprinted files; must match
/// `_source_files()` in `tests/python/conftest.py`.
fn source_files(root: &Path) -> Vec<(String, PathBuf)> {
    let mut files: Vec<PathBuf> = ["Cargo.toml", "Cargo.lock", "crates/nhydb-core/Cargo.toml", "crates/nhydb-py/Cargo.toml"]
        .iter()
        .map(|r| root.join(r))
        .collect();
    for d in ["crates/nhydb-core/src", "crates/nhydb-py/src"] {
        collect(&root.join(d), &mut files);
    }
    let mut out: Vec<(String, PathBuf)> = files
        .into_iter()
        .filter(|p| p.is_file())
        .map(|p| {
            let rel = p.strip_prefix(root).unwrap().components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/");
            (rel, p)
        })
        .collect();
    out.sort();
    out
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let root = manifest.join("../..");
    let hash = if root.join("crates/nhydb-core/src").is_dir() {
        let mut crc = 0u32;
        for (rel, p) in source_files(&root) {
            let data = fs::read(&p).unwrap_or_default();
            crc = crc32_update(crc, rel.as_bytes());
            crc = crc32_update(crc, b"\0");
            crc = crc32_update(crc, &data);
            crc = crc32_update(crc, b"\0");
        }
        format!("crc32:{crc:08x}")
    } else {
        "unknown".to_string()
    };
    println!("cargo:rustc-env=NHYDB_BUILD_SOURCE_HASH={hash}");
    for r in ["Cargo.toml", "Cargo.lock", "crates/nhydb-core/Cargo.toml", "crates/nhydb-core/src", "crates/nhydb-py/src"] {
        println!("cargo:rerun-if-changed={}", root.join(r).display());
    }
    println!("cargo:rerun-if-changed=build.rs");
}
