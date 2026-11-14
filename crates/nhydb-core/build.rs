//! Unit tests that are not part of the published sources live in separate
//! `*_tests.rs` files; they are compiled only where those files exist.

use std::path::PathBuf;

fn main() {
    println!("cargo::rustc-check-cfg=cfg(nhydb_internal_tests)");
    let marker = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("src/db/directory_tests.rs");
    println!("cargo:rerun-if-changed={}", marker.display());
    println!("cargo:rerun-if-changed=build.rs");
    if marker.is_file() {
        println!("cargo:rustc-cfg=nhydb_internal_tests");
    }
}
