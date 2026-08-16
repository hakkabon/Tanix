use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let linker_script = manifest_dir.join("../link.ld");

    println!("cargo:rerun-if-changed={}", linker_script.display());
    println!("cargo:rustc-link-arg=-T{}", linker_script.display());
    println!("cargo:rustc-link-arg=--entry=_start");
    println!("cargo:rustc-link-arg=--no-dynamic-linker");
    // Link the server at its final runtime address (see kernel server.rs).
    // Phase 16: TANIX_LINK_SHIFT (hex) moves the fixed virt link base onto
    // another machine's RAM window (the sbsa-ref build sets it to
    // 0xFFC0000000 = its 1 TiB RAM base minus virt's 1 GiB base).
    let shift = std::env::var("TANIX_LINK_SHIFT")
        .ok()
        .map(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).unwrap_or(0))
        .unwrap_or(0);
    println!("cargo:rerun-if-env-changed=TANIX_LINK_SHIFT");
    println!("cargo:rustc-link-arg=--defsym=LINK_BASE=0x{:x}", 0x41200000u64 + shift);
}
