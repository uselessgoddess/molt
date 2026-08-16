fn main() {
    println!("cargo::rustc-check-cfg=cfg(target_os, values(\"molt\"))");
    if let Some(script) = molt_layout() {
        println!("cargo::rerun-if-changed={script}");
        println!("cargo::rustc-link-arg=-T{script}");
    }
}

/// The absolute path of the layout this image links against, if it is a Molt one.
///
/// The target spec carries no path of its own: one written there would be
/// relative to whatever directory the linker started in, which is the working
/// directory today and an assumption the spec cannot state. Resolving it from
/// the manifest instead leaves the spec self-contained, which is what a tier-3
/// proposal would need it to be.
fn molt_layout() -> Option<String> {
    if std::env::var("CARGO_CFG_TARGET_OS").ok()? != "molt" {
        return None;
    }
    let script = match std::env::var("CARGO_CFG_TARGET_ARCH").ok()?.as_str() {
        "x86_64" => "x86_64-unknown-molt.ld",
        "riscv64" => "riscv64gc-unknown-molt.ld",
        _ => return None,
    };
    let manifest = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    Some(format!("{manifest}/../../targets/{script}"))
}
