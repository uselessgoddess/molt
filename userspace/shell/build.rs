fn main() {
    println!("cargo::rustc-check-cfg=cfg(target_os, values(\"molt\"))");
    let architecture = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let base = match architecture.as_str() {
        "x86_64" => "0x0000600000100000",
        "riscv64" => "0x00c0000000100000",
        _ => return,
    };
    println!("cargo:rustc-link-arg=--defsym=__molt_image_base_override={base}");
}
