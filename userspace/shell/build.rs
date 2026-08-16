fn main() {
    println!("cargo::rustc-check-cfg=cfg(target_os, values(\"molt\"))");
    let Some((script, base)) = molt_layout() else {
        return;
    };
    println!("cargo::rerun-if-changed={script}");
    println!("cargo::rustc-link-arg=-T{script}");
    println!("cargo::rustc-link-arg=--defsym=__molt_image_base_override={base}");
}

/// The absolute path of the layout this image links against and where it loads.
///
/// The target spec carries no path of its own: one written there would be
/// relative to whatever directory the linker started in, which is the working
/// directory today and an assumption the spec cannot state. Resolving it from
/// the manifest instead leaves the spec self-contained, which is what a tier-3
/// proposal would need it to be.
fn molt_layout() -> Option<(String, &'static str)> {
    if std::env::var("CARGO_CFG_TARGET_OS").ok()? != "molt" {
        return None;
    }
    let (script, base) = match std::env::var("CARGO_CFG_TARGET_ARCH").ok()?.as_str() {
        "x86_64" => ("x86_64-unknown-molt.ld", "0x0000600000100000"),
        "riscv64" => ("riscv64gc-unknown-molt.ld", "0x00c0000000100000"),
        _ => return None,
    };
    let manifest = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    Some((format!("{manifest}/../../targets/{script}"), base))
}
