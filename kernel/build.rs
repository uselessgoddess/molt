//! Build-time wiring for freestanding kernel targets.
//!
//! The x86_64 image is laid out by the `bootloader` crate at image-build time,
//! so it needs no linker script here. The RISC-V target is jumped to directly
//! by OpenSBI at a fixed load address, so it must be linked against a script
//! that places `_start` first at that address. The script is emitted into
//! `OUT_DIR` and referenced by filename through a `-L` search path, which keeps
//! the wiring independent of the linker's working directory. It pairs with the
//! `_start` shim and `molt_riscv::entry_point!` macro in the `molt-riscv` crate.

use std::path::PathBuf;
use std::{env, fs};

/// Places the kernel where OpenSBI hands control to it on the QEMU `virt` board.
const RISCV64_LINKER_SCRIPT: &str = include_str!("riscv64.ld");

fn main() {
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if arch == "riscv64" {
        let out_dir = PathBuf::from(env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
        let script = out_dir.join("molt-riscv64.ld");
        fs::write(&script, RISCV64_LINKER_SCRIPT).expect("write RISC-V linker script");
        println!("cargo:rustc-link-search={}", out_dir.display());
        println!("cargo:rustc-link-arg=-Tmolt-riscv64.ld");
        println!("cargo:rerun-if-changed=riscv64.ld");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
