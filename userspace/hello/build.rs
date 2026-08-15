fn main() {
    println!("cargo::rustc-check-cfg=cfg(target_os, values(\"molt\"))");
    println!("cargo::rerun-if-changed=../../targets/x86_64-unknown-molt.ld");
    println!("cargo::rerun-if-changed=../../targets/riscv64gc-unknown-molt.ld");
}
