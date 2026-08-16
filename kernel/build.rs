use std::path::PathBuf;
use std::{env, fs};

const RISCV64_LINKER_SCRIPT: &str = include_str!("riscv64.ld");

fn main() {
    println!("cargo:rustc-check-cfg=cfg(molt_user_image)");
    println!("cargo:rerun-if-env-changed=MOLT_USER_IMAGE");
    println!("cargo:rerun-if-env-changed=MOLT_SHELL_IMAGE");
    println!("cargo:rerun-if-env-changed=MOLT_DOMAIN_DISK_IMAGE");
    if env::var_os("MOLT_USER_IMAGE").is_some() {
        println!("cargo:rustc-cfg=molt_user_image");
    }
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
