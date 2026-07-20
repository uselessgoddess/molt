use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::time::{Duration, Instant};
use std::{env, fs, thread};

use bootloader::{BiosBoot, UefiBoot};

const X86_64_TARGET: &str = "x86_64-unknown-none";
const RISCV64_TARGET: &str = "riscv64gc-unknown-none-elf";

const BOOT_MARKERS: &[&str] = &[
    "MOLT_EXCEPTION_OK",
    "MOLT_MAPPING_OK",
    "MOLT_WX_OK",
    "MOLT_TIMER_OK",
    "MOLT_CANCELLATION_OK",
    "MOLT_STALE_COMPLETION_OK",
    "MOLT_RESTART_OK",
    "MOLT_BOOT_OK",
];

/// Marker the platform panic handlers print before terminating.
const PANIC_MARKER: &str = "MOLT_PANIC:";

const QEMU_X86_64_SUCCESS: i32 = (0x10 << 1) | 1;
const QEMU_X86_64_FAILURE: i32 = (0x11 << 1) | 1;
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
            let images = build_images(Case::Boot)?;
            println!("BIOS image: {}", images.bios.display());
            println!("UEFI image: {}", images.uefi.display());
            Ok(())
        }
        Some("boot") if args.next().is_none() => {
            let images = build_images(Case::Boot)?;
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

#[derive(Clone, Copy)]
enum Arch {
    X86_64,
    Riscv64,
}

/// What a smoke run boots and what it expects to see.
#[derive(Clone, Copy)]
enum Case {
    /// The shipped kernel: every Stage 1 marker, then a success exit.
    Boot,
    /// The `panic-smoke` build: a panic report, then a failure exit.
    Panic,
}

impl Case {
    fn features(self) -> &'static [&'static str] {
        match self {
            Case::Boot => &[],
            Case::Panic => &["panic-smoke"],
        }
    }

    fn markers(self) -> &'static [&'static str] {
        match self {
            Case::Boot => BOOT_MARKERS,
            Case::Panic => &[PANIC_MARKER],
        }
    }
}

/// Markers only one architecture can produce.
///
/// The RISC-V console backend is chosen by an SBI probe at boot. Requiring the
/// line, rather than a particular backend, proves the probe ran without
/// demanding firmware new enough to offer the debug console extension.
fn arch_markers(arch: Arch, case: Case) -> &'static [&'static str] {
    match (arch, case) {
        (Arch::Riscv64, Case::Boot) => &["MOLT_SBI_CONSOLE:"],
        _ => &[],
    }
}

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

fn smoke_arch(arch: Arch) -> Result<(), String> {
    smoke_case(arch, Case::Boot)?;
    smoke_case(arch, Case::Panic)
}

fn smoke_case(arch: Arch, case: Case) -> Result<(), String> {
    let name = match arch {
        Arch::X86_64 => "x86_64",
        Arch::Riscv64 => "riscv64",
    };
    let label = match case {
        Case::Boot => "boot",
        Case::Panic => "panic",
    };
    println!("== smoke: {name} {label} ==");

    let (command, binary) = match arch {
        Arch::X86_64 => {
            let images = build_images(case)?;
            (qemu_x86_64_command(&images.bios), "qemu-system-x86_64")
        }
        Arch::Riscv64 => {
            let kernel = build_kernel(RISCV64_TARGET, case)?;
            (qemu_riscv64_command(&kernel), "qemu-system-riscv64")
        }
    };

    let run = run_qemu_captured(command, binary, smoke_timeout())?;
    // Print what the guest managed to say before anything is judged: a run that
    // times out is the one whose serial log matters most.
    print!("{}", run.serial);
    if !run.diagnostics.is_empty() {
        eprint!("{}", run.diagnostics);
    }

    let status = run.status.ok_or_else(|| {
        format!(
            "{name} {label} QEMU did not exit within {}s; \
             set MOLT_SMOKE_TIMEOUT to raise the limit",
            smoke_timeout().as_secs()
        )
    })?;
    check_exit_status(arch, case, status)?;
    for marker in case.markers().iter().chain(arch_markers(arch, case)) {
        if !run.serial.contains(marker) {
            return Err(format!("{name} {label} QEMU exited without the {marker} serial marker"));
        }
    }
    Ok(())
}

/// Everything one bounded QEMU run produced. `status` is `None` on timeout.
struct QemuRun {
    status: Option<ExitStatus>,
    serial: String,
    diagnostics: String,
}

/// Runs QEMU with its serial log captured and a hard time bound.
///
/// Both pipes are drained by their own thread. A guest that outruns the 64 KiB
/// pipe buffer would otherwise block on its own serial writes and be reported
/// as a hang, and killing the child closes the pipes so the readers finish.
fn run_qemu_captured(
    mut command: Command,
    binary: &str,
    timeout: Duration,
) -> Result<QemuRun, String> {
    let mut child = command
        .arg("-serial")
        .arg("stdio")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to start {binary}: {error}"))?;
    let serial = drain(child.stdout.take().expect("stdout was piped"));
    let diagnostics = drain(child.stderr.take().expect("stderr was piped"));

    let status = wait_with_timeout(&mut child, timeout)?;
    let serial = serial.join().map_err(|_| "serial reader thread panicked".to_string())?;
    let diagnostics =
        diagnostics.join().map_err(|_| "diagnostic reader thread panicked".to_string())?;
    Ok(QemuRun { status, serial, diagnostics })
}

/// Reads one child pipe to end-of-file on its own thread.
fn drain(mut source: impl Read + Send + 'static) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = source.read_to_end(&mut bytes);
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

/// The per-run QEMU time bound, overridable for slow emulated hosts.
fn smoke_timeout() -> Duration {
    env::var("MOLT_SMOKE_TIMEOUT")
        .ok()
        .and_then(|value| value.parse().ok())
        .map_or(SMOKE_TIMEOUT, Duration::from_secs)
}

struct Images {
    bios: PathBuf,
    uefi: PathBuf,
}

fn build_images(case: Case) -> Result<Images, String> {
    let kernel = build_kernel(X86_64_TARGET, case)?;

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

fn build_kernel(target: &str, case: Case) -> Result<PathBuf, String> {
    let root = workspace_root();
    let mut command = Command::new(cargo());
    command.current_dir(&root).args([
        "build",
        "--package",
        "molt-kernel",
        "--target",
        target,
        "--release",
    ]);
    for feature in case.features() {
        command.args(["--features", feature]);
    }
    let status = command.status().map_err(|error| format!("failed to start cargo: {error}"))?;
    require_success(status, "kernel build")?;

    let kernel = target_dir(&root).join(target).join("release/molt-kernel");
    if !kernel.is_file() {
        return Err(format!("kernel binary was not created at {}", kernel.display()));
    }
    Ok(kernel)
}

fn run_qemu_interactive(image: &Path) -> Result<(), String> {
    let status = qemu_x86_64_command(image)
        // An interactive session keeps the machine alive after a guest reset so
        // the monitor can still inspect it; a smoke run must exit instead.
        .arg("-no-shutdown")
        .arg("-serial")
        .arg("mon:stdio")
        .status()
        .map_err(|error| format!("failed to start qemu-system-x86_64: {error}"))?;
    check_exit_status(Arch::X86_64, Case::Boot, status)
}

fn qemu_x86_64_command(image: &Path) -> Command {
    let qemu = env::var_os("MOLT_QEMU").unwrap_or_else(|| OsString::from("qemu-system-x86_64"));
    let mut command = Command::new(qemu);
    if let Some(firmware) = env::var_os("MOLT_QEMU_FIRMWARE") {
        command.arg("-L").arg(firmware);
    }
    // `-no-shutdown` is deliberately absent: it makes QEMU *stop* rather than
    // exit when the guest requests a reset, so a triple fault during boot turns
    // into an unkillable hang with no serial log instead of a reported failure.
    // The interactive `boot` path adds it back, where a monitor can use it.
    command.args([
        "-display",
        "none",
        "-no-reboot",
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

/// Waits for `child`, killing it and reporting `None` once `timeout` elapses.
fn wait_with_timeout(child: &mut Child, timeout: Duration) -> Result<Option<ExitStatus>, String> {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(Some(status)),
            Ok(None) if started.elapsed() < timeout => thread::sleep(Duration::from_millis(25)),
            Ok(None) => {
                child
                    .kill()
                    .map_err(|error| format!("QEMU timed out and could not be killed: {error}"))?;
                let _ = child.wait();
                return Ok(None);
            }
            Err(error) => {
                return Err(format!("failed while waiting for QEMU: {error}"));
            }
        }
    }
}

fn check_exit_status(arch: Arch, case: Case, status: ExitStatus) -> Result<(), String> {
    let expected = match (arch, case) {
        (Arch::X86_64, Case::Boot) => QEMU_X86_64_SUCCESS,
        (Arch::X86_64, Case::Panic) => QEMU_X86_64_FAILURE,
        // An SBI shutdown exits QEMU cleanly whatever the reason, so the marker
        // check is what distinguishes a terminal state from an early panic.
        (Arch::Riscv64, _) => 0,
    };
    if status.code() == Some(expected) {
        Ok(())
    } else {
        Err(format!("QEMU reported {:?}; expected exit status {expected}", status.code()))
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
