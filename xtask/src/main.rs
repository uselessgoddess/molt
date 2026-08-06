use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::time::{Duration, Instant};
use std::{env, fs, thread};

use bootloader::{BiosBoot, UefiBoot};
use molt_fs::format::{Tree, build};

mod bench;
mod site;

const X86_64_TARGET: &str = "x86_64-unknown-none";
const RISCV64_TARGET: &str = "riscv64gc-unknown-none-elf";

/// The tree the smoke disk is built from, relative to the workspace root.
const DISK_TREE: &str = "disk";

const BOOT_MARKERS: &[&str] = &[
    "MOLT_EXCEPTION_OK",
    "MOLT_RAM_OK",
    "MOLT_HEAP_OK",
    "MOLT_MAPPING_OK",
    "MOLT_VA_OK",
    "MOLT_ASID_OK",
    "MOLT_REFCOUNT_OK",
    "MOLT_WX_OK",
    "MOLT_HUGE_MAP_OK",
    "MOLT_DEVICE_WINDOW_OK",
    "MOLT_EXEC_OK",
    "MOLT_SMP_OK",
    "MOLT_SHOOTDOWN_OK",
    "MOLT_TIMER_OK",
    "MOLT_CANCELLATION_OK",
    "MOLT_STALE_COMPLETION_OK",
    "MOLT_RESTART_OK",
    "MOLT_PHYSMAP_OK",
    "MOLT_FRAME_OWNER_OK",
    "MOLT_PCI_OK",
    "MOLT_BOOT_OK",
];

const PANIC_MARKER: &str = "MOLT_PANIC:";

/// Cores QEMU is given, which is what the SMP marker counts.
const CORES: &str = "4";

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
        Some("mkfs") => match (args.next(), args.next(), args.next()) {
            (Some(tree), Some(image), None) => mkfs(Path::new(&tree), Path::new(&image)),
            _ => Err(usage()),
        },
        Some("bench") => bench::dispatch(&args.collect::<Vec<_>>()),
        _ => Err(usage()),
    }
}

fn usage() -> String {
    "usage: cargo xtask <image|boot|smoke [x86_64|riscv64|all]|mkfs <tree> <image>\
     |bench [compare <base> [head]|record <dir>|site <data> <out>]>"
        .into()
}

/// Writes the tree at `tree` out as a mountable MoltFS image.
fn mkfs(tree: &Path, image: &Path) -> Result<(), String> {
    let bytes = lay_out(tree)?;
    fs::write(image, &bytes)
        .map_err(|error| format!("failed to write {}: {error}", image.display()))?;
    println!("{}: {} bytes from {}", image.display(), bytes.len(), tree.display());
    Ok(())
}

fn lay_out(tree: &Path) -> Result<Vec<u8>, String> {
    let mut root = Tree::new();
    read_tree(&mut root, tree)?;
    build(&root, 1).map_err(|error| format!("failed to lay out {}: {error:?}", tree.display()))
}

/// Reads `dir` into `tree`, in name order so the image is reproducible.
fn read_tree(tree: &mut Tree, dir: &Path) -> Result<(), String> {
    let read = |error| format!("failed to read {}: {error}", dir.display());
    let mut entries: Vec<fs::DirEntry> =
        fs::read_dir(dir).map_err(read)?.collect::<Result<_, _>>().map_err(read)?;
    entries.sort_by_key(fs::DirEntry::file_name);

    for entry in entries {
        let path = entry.path();
        let name = entry
            .file_name()
            .into_string()
            .map_err(|name| format!("{name:?} is not a UTF-8 name"))?;
        let kind = entry.file_type().map_err(&read)?;
        let named = |error| format!("{} cannot go into an image: {error:?}", path.display());
        if kind.is_dir() {
            read_tree(tree.dir(&name).map_err(named)?, &path)?;
        } else if kind.is_file() {
            let bytes = fs::read(&path)
                .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
            tree.file(&name, bytes).map_err(named)?;
        } else {
            return Err(format!("{} is neither file nor directory", path.display()));
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Arch {
    X86_64,
    Riscv64,
}

#[derive(Clone, Copy)]
enum Case {
    Boot,
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

fn arch_markers(arch: Arch, case: Case) -> &'static [&'static str] {
    match (arch, case) {
        // `sv57` is an assertion, not a report: the QEMU `virt` board's default
        // rv64 CPU implements every mode, so anything narrower means the probe
        // in `paging::enable` stopped early and the address space Molt plans to
        // spend is not there.
        // The RAM top is likewise an assertion: QEMU was given 2 GiB above
        // `0x8000_0000`, so a kernel that read the device tree prints exactly
        // this, and one that fell back to its own constant prints `0x88000000`.
        // The leaf size is per-arch because the mappers differ: the RISC-V port
        // reaches for a gigapage and the 2 GiB QEMU is given leaves room for
        // exactly one, while the x86_64 direct map stops at 2 MiB. Both are
        // pinned so a port that silently fell back to 4 KiB pages fails here
        // instead of only showing up as TLB misses years later.
        (Arch::Riscv64, Case::Boot) => &[
            "MOLT_SBI_CONSOLE:",
            "MOLT_SATP_MODE: sv57",
            "MOLT_RAM_OK: top 0x100000000",
            "MOLT_HUGE_MAP_OK: 1 GiB leaf",
            "MOLT_UART_WINDOW:",
        ],
        (Arch::X86_64, Case::Boot) => &[
            "MOLT_HUGE_MAP_OK: 2 MiB leaf",
            "MOLT_BAR_OK:",
            "MOLT_MSI_OK:",
            "MOLT_INTERRUPT_OK:",
            "MOLT_AFFINITY_OK:",
            "MOLT_VIRTIO_OK:",
            "MOLT_IOMMU_OK:",
            "MOLT_IOMMU_MAP_OK:",
            "MOLT_BLOCK_OK:",
            "MOLT_BLOCK_DEPTH_OK:",
            "MOLT_BLK_IRQ_OK:",
            "MOLT_FS_OK:",
            "MOLT_FS_WRITE_OK:",
            "MOLT_REGISTRY_OK:",
            "MOLT_WATCHDOG_OK:",
            "molt> cat hello.txt",
            "hello, molt",
            "MOLT_SHELL_OK:",
            "MOLT_FS_RESTART_OK:",
            "MOLT_VIRTIO_RESET_OK:",
            "MOLT_IOMMU_FAULT_OK:",
            "MOLT_NVME_IOMMU_OK:",
            "MOLT_NVME_DEPTH_OK:",
            "MOLT_NVME_OK:",
            "MOLT_NVME_RESET_OK:",
            "MOLT_NET_IOMMU_OK:",
            "MOLT_NET_OK:",
            "MOLT_UDP_OK:",
            "MOLT_NDP_OK:",
            "MOLT_TCP_OK:",
        ],
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
            (qemu_x86_64_command(&images.bios)?, "qemu-system-x86_64")
        }
        Arch::Riscv64 => {
            let kernel = build_kernel(RISCV64_TARGET, case)?;
            (qemu_riscv64_command(&kernel), "qemu-system-riscv64")
        }
    };

    let run = run_qemu_captured(command, binary, smoke_timeout())?;
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

struct QemuRun {
    status: Option<ExitStatus>,
    serial: String,
    diagnostics: String,
}

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

fn drain(mut source: impl Read + Send + 'static) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = source.read_to_end(&mut bytes);
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

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
    let status = qemu_x86_64_command(image)?
        .arg("-no-shutdown")
        .arg("-serial")
        .arg("mon:stdio")
        .status()
        .map_err(|error| format!("failed to start qemu-system-x86_64: {error}"))?;
    check_exit_status(Arch::X86_64, Case::Boot, status)
}

fn qemu_x86_64_command(image: &Path) -> Result<Command, String> {
    let disk = smoke_disk("molt-disk.img")?;
    let nvme = smoke_disk("molt-nvme.img")?;
    let qemu = env::var_os("MOLT_QEMU").unwrap_or_else(|| OsString::from("qemu-system-x86_64"));
    let mut command = Command::new(qemu);
    if let Some(firmware) = env::var_os("MOLT_QEMU_FIRMWARE") {
        command.arg("-L").arg(firmware);
    }
    command.args([
        "-machine",
        "q35",
        // More cores than the smoke needs to prove one: a crossing that only
        // ever runs on two is a crossing that never had to pick a target.
        "-smp",
        CORES,
        "-device",
        "edu",
        "-display",
        "none",
        "-no-reboot",
        "-device",
        "isa-debug-exit,iobase=0xf4,iosize=0x04",
    ]);
    command.arg("-drive").arg(format!("format=raw,file={}", image.display()));
    command
        .arg("-drive")
        .arg(format!("if=none,id=molt-disk,format=raw,cache=none,file={}", disk.display()));
    command
        .arg("-drive")
        .arg(format!("if=none,id=molt-nvme,format=raw,cache=none,file={}", nvme.display()));
    command.arg("-device").arg("virtio-iommu-pci");
    command
        .arg("-device")
        .arg("virtio-blk-pci,drive=molt-disk,disable-legacy=on,iommu_platform=on");
    command.arg("-device").arg("nvme,serial=molt,drive=molt-nvme");
    // `cat` behind the forwarder is the TCP smoke's peer: slirp runs one per
    // connection and pipes the stream through it, so it answers with the bytes
    // it was sent without the host having to listen anywhere.
    command.args(["-netdev", "user,id=molt-net,guestfwd=tcp:10.0.2.100:80-cmd:cat"]);
    command.arg("-device").arg(
        "virtio-net-pci,netdev=molt-net,disable-legacy=on,iommu_platform=on,mac=52:54:00:12:34:56",
    );
    Ok(command)
}

/// Builds the disk the smoke test hands to QEMU.
///
/// It is a real MoltFS image rather than a signed pattern, so block I/O, a
/// durable runtime write, and the shell's `cat` prove the same disk end to end.
fn smoke_disk(name: &str) -> Result<PathBuf, String> {
    let root = workspace_root();
    let image_dir = target_dir(&root).join("molt");
    fs::create_dir_all(&image_dir)
        .map_err(|error| format!("failed to create {}: {error}", image_dir.display()))?;
    let path = image_dir.join(name);

    let image = lay_out(&root.join(DISK_TREE))?;
    fs::write(&path, &image)
        .map_err(|error| format!("failed to write {}: {error}", path.display()))?;
    Ok(path)
}

fn qemu_riscv64_command(kernel: &Path) -> Command {
    let qemu =
        env::var_os("MOLT_QEMU_RISCV64").unwrap_or_else(|| OsString::from("qemu-system-riscv64"));
    let mut command = Command::new(qemu);
    // OpenSBI loads the ELF at its S-mode payload address.
    // More than the 128 MiB default, and not a round power of the fallback the
    // kernel used to carry: `MOLT_RAM_OK` can only print this if the device
    // tree was read. It is also the least that lets a 1 GiB leaf exist.
    command.args(["-machine", "virt", "-smp", CORES, "-m", "2G", "-display", "none"]);
    command.args(["-no-reboot", "-bios", "default"]);
    command.arg("-kernel").arg(kernel);
    command
}

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
        // Serial markers distinguish success and panic because SBI exits both with zero.
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use molt_block::{Loopback, Serial};
    use molt_core::CellId;
    use molt_core::buffer::{BufferOperation, BufferRegistry};
    use molt_fs::{Fs, FsDone, FsOp, Handle, Kind, Name};

    use super::{DISK_TREE, lay_out, qemu_x86_64_command, workspace_root};

    const OWNER: CellId = CellId::new(1);
    const WINDOW: usize = 64;

    #[test]
    fn x86_devices_isolated() {
        let command = qemu_x86_64_command(Path::new("boot.img")).unwrap();
        let args: Vec<_> = command.get_args().map(|arg| arg.to_string_lossy()).collect();

        assert!(args.iter().any(|arg| arg.as_ref() == "nvme,serial=molt,drive=molt-nvme"));
        assert!(
            args.iter().any(|arg| arg.contains("id=molt-disk") && arg.contains("molt-disk.img"))
        );
        assert!(
            args.iter().any(|arg| arg.contains("id=molt-nvme") && arg.contains("molt-nvme.img"))
        );
        assert!(args.iter().any(|arg| {
            arg.as_ref()
                == "virtio-net-pci,netdev=molt-net,disable-legacy=on,iommu_platform=on,mac=52:54:00:12:34:56"
        }));
    }

    #[test]
    fn smoke_disk_mounts_reads_and_writes() {
        let tree = workspace_root().join(DISK_TREE);
        let on_disk = fs::read(tree.join("hello.txt")).unwrap();
        let mut image = lay_out(&tree).unwrap();
        {
            let mut fs =
                Fs::<_, 4>::mount(Serial::new(Loopback::write(&mut image).unwrap())).unwrap();

            let mut bytes = [0u8; WINDOW];
            let mut buffers = BufferRegistry::<1>::new();
            let buffer = buffers.register_read_write(OWNER, &mut bytes).unwrap();
            let target = buffers.write_capability(buffer).unwrap();
            let root = fs.root(OWNER).unwrap();
            let name = Name::try_from("hello.txt").unwrap();
            let opened = fs.apply(OWNER, FsOp::Open { dir: root, name }, &mut buffers).unwrap();
            let Some(Handle::File(file)) = opened.handle() else {
                panic!("hello.txt opened as a directory: {opened:?}");
            };

            let window = BufferOperation::new(target, 0, WINDOW);
            let read =
                fs.apply(OWNER, FsOp::Read { file, buffer: window, offset: 0 }, &mut buffers);
            assert_eq!(read, Ok(FsDone::Read(on_disk.len())));
            assert_eq!(&buffers.resolve_write(window).unwrap()[..on_disk.len()], &on_disk);

            let source = buffers.read_capability(buffer).unwrap();
            let created = fs
                .apply(
                    OWNER,
                    FsOp::Create {
                        dir: root,
                        name: Name::try_from("runtime.txt").unwrap(),
                        kind: Kind::File,
                    },
                    &mut buffers,
                )
                .unwrap();
            let Some(Handle::File(runtime)) = created.handle() else {
                panic!("runtime.txt opened as a directory: {created:?}");
            };
            let write = FsOp::Write {
                file: runtime,
                buffer: BufferOperation::new(source, 0, on_disk.len()),
                offset: 0,
            };
            assert_eq!(fs.apply(OWNER, write, &mut buffers), Ok(FsDone::Written(on_disk.len())));
            assert_eq!(fs.apply(OWNER, FsOp::Sync, &mut buffers), Ok(FsDone::Synced(2)));
        }
        let mut bytes = [0u8; WINDOW];
        let mut fs = Fs::<_, 4>::mount(Serial::new(Loopback::read(&image).unwrap())).unwrap();
        let mut buffers = BufferRegistry::<1>::new();
        let buffer = buffers.register_write(OWNER, &mut bytes).unwrap();
        let root = fs.root(OWNER).unwrap();
        let opened = fs
            .apply(
                OWNER,
                FsOp::Open { dir: root, name: Name::try_from("runtime.txt").unwrap() },
                &mut buffers,
            )
            .unwrap();
        let Some(Handle::File(file)) = opened.handle() else {
            panic!("runtime.txt opened as a directory: {opened:?}");
        };
        let window = BufferOperation::new(buffer, 0, WINDOW);

        assert_eq!(
            fs.apply(OWNER, FsOp::Read { file, buffer: window, offset: 0 }, &mut buffers),
            Ok(FsDone::Read(on_disk.len()))
        );
        assert_eq!(&buffers.resolve_write(window).unwrap()[..on_disk.len()], &on_disk);
    }
}
