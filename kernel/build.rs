use std::path::PathBuf;

fn main() {
    // Cargo sets CARGO_MANIFEST_DIR to the crate root (kernel/).
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let linker_script = manifest_dir.join("link.ld");

    // Re-run if the linker script or any assembly file changes.
    println!("cargo:rerun-if-changed={}", linker_script.display());
    println!("cargo:rerun-if-changed=src/sched/switch.s");
    println!("cargo:rerun-if-changed=src/arch/aarch64/vectors.s");

    // Pass the full path to the linker script so it works regardless of
    // the working directory the linker is invoked from.
    println!("cargo:rustc-link-arg=-T{}", linker_script.display());

    // Keep the entry symbol intact.
    println!("cargo:rustc-link-arg=--entry=_start");

    // Prevent the linker from inserting default libraries.
    println!("cargo:rustc-link-arg=--no-dynamic-linker");
}
