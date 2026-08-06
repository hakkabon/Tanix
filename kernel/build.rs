//! Build script for the Tanix kernel.
//!
//! Exposes the profile-aware path of the pre-built Zephyr-stub guest binary
//! via `cargo:rustc-env=TANIX_STUB_BIN_PATH`, consumed by
//! `include_bytes!(env!(...))` in main.rs when the `embed-zephyr-stub`
//! feature is enabled.

use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());

    // Link with the kernel linker script (entry point, sections, stack).
    let linker_script = manifest_dir.join("link.ld");
    println!("cargo:rerun-if-changed={}", linker_script.display());
    println!("cargo:rustc-link-arg=-T{}", linker_script.display());
    println!("cargo:rustc-link-arg=--entry=_start");
    println!("cargo:rustc-link-arg=--no-dynamic-linker");

    // Rebuild if the guest stub binary changes (or appears).
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".into());
    let stub_path = format!(
        "../../target/aarch64-unknown-none/{}/tanix-zephyr-stub",
        profile
    );
    println!("cargo:rerun-if-changed={}", stub_path);
    println!("cargo:rustc-env=TANIX_STUB_BIN_PATH={}", stub_path);
}
