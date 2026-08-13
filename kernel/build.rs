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
    // Phase 16: the `sbsa-ref` machine links at its own DDR base.
    let linker_script = if std::env::var_os("CARGO_FEATURE_SBSA_REF").is_some() {
        manifest_dir.join("link-sbsa.ld")
    } else {
        manifest_dir.join("link.ld")
    };
    println!("cargo:rerun-if-changed={}", linker_script.display());
    println!("cargo:rustc-link-arg=-T{}", linker_script.display());
    println!("cargo:rustc-link-arg=--entry=_start");
    println!("cargo:rustc-link-arg=--no-dynamic-linker");

    // Rebuild if the guest stub binary changes (or appears).
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".into());
    // Phase 16: the sbsa-ref build embeds server binaries linked for the
    // sbsa RAM window (built with TANIX_LINK_SHIFT into a separate target
    // dir, e.g. target-sbsa); TANIX_SERVER_TARGET_DIR points there.
    let server_dir =
        std::env::var("TANIX_SERVER_TARGET_DIR").unwrap_or_else(|_| "target".into());
    println!("cargo:rerun-if-env-changed=TANIX_SERVER_TARGET_DIR");
    println!("cargo:rerun-if-env-changed=PROFILE");
    let stub_path = format!(
        "../../{}/aarch64-unknown-none/{}/tanix-zephyr-stub",
        server_dir, profile
    );
    println!("cargo:rerun-if-changed={}", stub_path);
    println!("cargo:rustc-env=TANIX_STUB_BIN_PATH={}", stub_path);

    // Same for the Phase-4/5/7/8/9/10/12/17 server binaries (init, pm,
    // mem, dev, worker, display, ui-demo, hog, wm, counter, clock, ramfs,
    // shell, net, ping, pong, sec).
    for name in [
        "init", "pm", "mem", "dev", "worker", "display", "ui-demo", "hog",
        "wm", "counter", "clock", "ramfs", "shell", "net", "ping", "pong",
        "sec",
    ] {
        let path = format!(
            "../../{}/aarch64-unknown-none/{}/tanix-{}",
            server_dir, profile, name
        );
        println!("cargo:rerun-if-changed={}", path);
        println!(
            "cargo:rustc-env=TANIX_{}_BIN_PATH={}",
            name.to_uppercase().replace('-', "_"),
            path
        );
    }
}
