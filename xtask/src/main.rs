use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::time::{Duration, Instant};
use std::{env, fs, thread};

use bootloader::{BiosBoot, UefiBoot};

/// Bare-metal target triple for each supported architecture.
const X86_64_TARGET: &str = "x86_64-unknown-none";
const RISCV64_TARGET: &str = "riscv64gc-unknown-none-elf";

/// Serial markers every architecture must emit, in order, for a healthy boot.
const BOOT_MARKERS: &[&str] = &[
    "MOLT_EXCEPTION_OK",
    "MOLT_MAPPING_OK",
    "MOLT_TIMER_OK",
    "MOLT_CANCELLATION_OK",
    "MOLT_STALE_COMPLETION_OK",
    "MOLT_RESTART_OK",
    "MOLT_BOOT_OK",
];
/// isa-debug-exit status the x86_64 kernel reports for a successful run.
const QEMU_X86_64_SUCCESS: i32 = (0x10 << 1) | 1;
const SMOKE_TIMEOUT: Duration = Duration::from_secs(20);

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
        Some("smoke") => {
            let selection = args.next();
            if args.next().is_some() {
                return Err(usage());
            }
            smoke(selection.as_deref())
        }
        _ => Err(usage()),
    }
}

fn usage() -> String {
    "usage: cargo xtask <image|boot|smoke [x86_64|riscv64|all]>".into()
}

/// The architectures exercised by the smoke suite.
#[derive(Clone, Copy)]
enum Arch {
    X86_64,
    Riscv64,
}

/// Runs the smoke test for the selected architecture, or all of them.
fn smoke(selection: Option<&str>) -> Result<(), String> {
    match selection {
        None | Some("all") => {
            smoke_arch(Arch::X86_64)?;
            smoke_arch(Arch::Riscv64)
        }
        Some("x86_64") => smoke_arch(Arch::X86_64),
        Some("riscv64") => smoke_arch(Arch::Riscv64),
        Some(other) => Err(format!("unknown smoke target {other:?}; {}", usage())),
    }
}

/// Boots one architecture under QEMU and checks every serial marker appears.
fn smoke_arch(arch: Arch) -> Result<(), String> {
    let name = match arch {
        Arch::X86_64 => "x86_64",
        Arch::Riscv64 => "riscv64",
    };
    println!("== smoke: {name} ==");

    let mut child = match arch {
        Arch::X86_64 => {
            let images = build_images()?;
            qemu_x86_64_command(&images.bios)
                .arg("-serial")
                .arg("stdio")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|error| format!("failed to start qemu-system-x86_64: {error}"))?
        }
        Arch::Riscv64 => {
            let kernel = build_kernel(RISCV64_TARGET)?;
            qemu_riscv64_command(&kernel)
                .arg("-serial")
                .arg("stdio")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|error| format!("failed to start qemu-system-riscv64: {error}"))?
        }
    };

    let status = wait_with_timeout(&mut child, SMOKE_TIMEOUT)?;
    let output = child
        .wait_with_output()
        .map_err(|error| format!("failed to collect QEMU output: {error}"))?;
    let serial = String::from_utf8_lossy(&output.stdout);
    print!("{serial}");
    if !output.stderr.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
    }

    check_exit_status(arch, status)?;
    for marker in BOOT_MARKERS {
        if !serial.contains(marker) {
            return Err(format!("{name} QEMU exited without the {marker} serial marker"));
        }
    }
    Ok(())
}

struct Images {
    bios: PathBuf,
    uefi: PathBuf,
}

fn build_images() -> Result<Images, String> {
    let kernel = build_kernel(X86_64_TARGET)?;

    let target_dir = target_dir(&workspace_root());
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

/// Builds the release kernel for `target` and returns the ELF path.
fn build_kernel(target: &str) -> Result<PathBuf, String> {
    let root = workspace_root();
    let status = Command::new(cargo())
        .current_dir(&root)
        .args(["build", "--package", "molt-kernel", "--target", target, "--release"])
        .status()
        .map_err(|error| format!("failed to start cargo: {error}"))?;
    require_success(status, "kernel build")?;

    let kernel = target_dir(&root).join(target).join("release/molt-kernel");
    if !kernel.is_file() {
        return Err(format!("kernel binary was not created at {}", kernel.display()));
    }
    Ok(kernel)
}

fn run_qemu_interactive(image: &Path) -> Result<(), String> {
    let status = qemu_x86_64_command(image)
        .arg("-serial")
        .arg("mon:stdio")
        .status()
        .map_err(|error| format!("failed to start qemu-system-x86_64: {error}"))?;
    check_exit_status(Arch::X86_64, status)
}

fn qemu_x86_64_command(image: &Path) -> Command {
    let qemu = env::var_os("MOLT_QEMU").unwrap_or_else(|| OsString::from("qemu-system-x86_64"));
    let mut command = Command::new(qemu);
    if let Some(firmware) = env::var_os("MOLT_QEMU_FIRMWARE") {
        command.arg("-L").arg(firmware);
    }
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

fn qemu_riscv64_command(kernel: &Path) -> Command {
    let qemu =
        env::var_os("MOLT_QEMU_RISCV64").unwrap_or_else(|| OsString::from("qemu-system-riscv64"));
    let mut command = Command::new(qemu);
    // OpenSBI (`-bios default`) loads the ELF at its S-mode payload address and
    // an orderly SBI shutdown exits QEMU through the `virt` board's test device.
    command.args(["-machine", "virt", "-display", "none", "-no-reboot", "-bios", "default"]);
    command.arg("-kernel").arg(kernel);
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

/// Checks QEMU's process exit against the architecture's success convention.
fn check_exit_status(arch: Arch, status: ExitStatus) -> Result<(), String> {
    match arch {
        // The isa-debug-exit device reports the kernel's status in its exit code.
        Arch::X86_64 if status.code() == Some(QEMU_X86_64_SUCCESS) => Ok(()),
        Arch::X86_64 => Err(format!(
            "QEMU reported {:?}; expected debug-exit status {QEMU_X86_64_SUCCESS}",
            status.code()
        )),
        // An SBI shutdown exits QEMU cleanly; the marker check proves it reached
        // the terminal state rather than shutting down early on a panic.
        Arch::Riscv64 if status.code() == Some(0) => Ok(()),
        Arch::Riscv64 => Err(format!("QEMU reported {:?}; expected clean exit 0", status.code())),
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
