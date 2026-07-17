use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::time::{Duration, Instant};
use std::{env, fs, thread};

use bootloader::{BiosBoot, UefiBoot};

const TARGET: &str = "x86_64-unknown-none";
const BOOT_MARKER: &str = "MOLT_BOOT_OK";
const QEMU_SUCCESS: i32 = (0x10 << 1) | 1;
const SMOKE_TIMEOUT: Duration = Duration::from_secs(10);

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("image") if args.next().is_none() => {
            let images = build_images()?;
            println!("BIOS image: {}", images.bios.display());
            println!("UEFI image: {}", images.uefi.display());
            Ok(())
        }
        Some("boot") if args.next().is_none() => {
            let images = build_images()?;
            run_qemu_interactive(&images.bios)
        }
        Some("smoke") if args.next().is_none() => smoke_test(),
        _ => Err("usage: cargo xtask <image|boot|smoke>".into()),
    }
}

struct Images {
    bios: PathBuf,
    uefi: PathBuf,
}

fn build_images() -> Result<Images, String> {
    let root = workspace_root();
    let status = Command::new(cargo())
        .current_dir(&root)
        .args(["build", "--package", "molt-kernel", "--target", TARGET, "--release"])
        .status()
        .map_err(|error| format!("failed to start cargo: {error}"))?;
    require_success(status, "kernel build")?;

    let target_dir = target_dir(&root);
    let kernel = target_dir.join(TARGET).join("release/molt-kernel");
    if !kernel.is_file() {
        return Err(format!("kernel binary was not created at {}", kernel.display()));
    }

    let image_dir = target_dir.join("molt");
    fs::create_dir_all(&image_dir)
        .map_err(|error| format!("failed to create {}: {error}", image_dir.display()))?;
    let images =
        Images { bios: image_dir.join("molt-bios.img"), uefi: image_dir.join("molt-uefi.img") };

    BiosBoot::new(&kernel)
        .create_disk_image(&images.bios)
        .map_err(|error| format!("failed to build BIOS image: {error}"))?;
    UefiBoot::new(&kernel)
        .create_disk_image(&images.uefi)
        .map_err(|error| format!("failed to build UEFI image: {error}"))?;
    Ok(images)
}

fn run_qemu_interactive(image: &Path) -> Result<(), String> {
    let status = qemu_command(image)
        .arg("-serial")
        .arg("mon:stdio")
        .status()
        .map_err(|error| format!("failed to start qemu-system-x86_64: {error}"))?;
    require_qemu_success(status)
}

fn smoke_test() -> Result<(), String> {
    let images = build_images()?;
    let mut child = qemu_command(&images.bios)
        .arg("-serial")
        .arg("stdio")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to start qemu-system-x86_64: {error}"))?;

    let status = wait_with_timeout(&mut child, SMOKE_TIMEOUT)?;
    let output = child
        .wait_with_output()
        .map_err(|error| format!("failed to collect QEMU output: {error}"))?;
    let serial = String::from_utf8_lossy(&output.stdout);
    print!("{serial}");
    if !output.stderr.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
    }

    require_qemu_success(status)?;
    if !serial.contains(BOOT_MARKER) {
        return Err(format!("QEMU exited without the {BOOT_MARKER} serial marker"));
    }
    Ok(())
}

fn qemu_command(image: &Path) -> Command {
    let mut command = Command::new("qemu-system-x86_64");
    command.args([
        "-display",
        "none",
        "-no-reboot",
        "-no-shutdown",
        "-device",
        "isa-debug-exit,iobase=0xf4,iosize=0x04",
        "-drive",
    ]);
    command.arg(format!("format=raw,file={}", image.display()));
    command
}

fn wait_with_timeout(child: &mut Child, timeout: Duration) -> Result<ExitStatus, String> {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if started.elapsed() < timeout => thread::sleep(Duration::from_millis(25)),
            Ok(None) => {
                child
                    .kill()
                    .map_err(|error| format!("QEMU timed out and could not be killed: {error}"))?;
                let _ = child.wait();
                return Err(format!("QEMU did not exit within {}s", timeout.as_secs()));
            }
            Err(error) => {
                return Err(format!("failed while waiting for QEMU: {error}"));
            }
        }
    }
}

fn require_qemu_success(status: ExitStatus) -> Result<(), String> {
    if status.code() == Some(QEMU_SUCCESS) {
        Ok(())
    } else {
        Err(format!("QEMU reported {:?}; expected debug-exit status {QEMU_SUCCESS}", status.code()))
    }
}

fn require_success(status: ExitStatus, operation: &str) -> Result<(), String> {
    if status.success() { Ok(()) } else { Err(format!("{operation} failed with {status}")) }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives directly below the workspace root")
        .to_path_buf()
}

fn target_dir(root: &Path) -> PathBuf {
    env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .map(|path| if path.is_absolute() { path } else { root.join(path) })
        .unwrap_or_else(|| root.join("target"))
}

fn cargo() -> OsString {
    env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"))
}
