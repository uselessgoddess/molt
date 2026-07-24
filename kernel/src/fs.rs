//! Mounting the disk, committing a write, and reading it as a cell would.
//!
//! Nothing here reaches into the volume: the shell holds one root capability
//! and talks over a ring, the filesystem answers on the same loop, and the only
//! thing the kernel adds is the serial port both print through.

use core::cell::RefCell;

use molt_arch::{Platform, SerialPort, SerialWriter};
use molt_block::Disk;
use molt_core::buffer::{BufferOperation, BufferRegistry};
use molt_core::capability::CellId;
use molt_core::ring::IoRing;
use molt_fs::{BLOCK, Fs, FsDone, FsError, FsOp, Handle, Kind, Name};
use molt_kernel::report;
use molt_shell::{Console, Session, Shell, drive};

const OWNER: CellId = CellId::new(2);
/// Root, one entry open at a time, and room to be wrong about that.
const HANDLES: usize = 4;
const RING: usize = 4;
/// Deliberately smaller than the files on the disk, so `cat` has to loop.
const WINDOW: usize = 64;
const WRITTEN: &[u8] = b"written through virtio";

const SCRIPT: &[u8] = b"help\nls\nls docs\ncat hello.txt\nls nowhere\n";

pub fn smoke<P: Platform>(platform: &mut P, device: impl Disk) {
    let mut block = [0u8; BLOCK];
    let mut fs = match Fs::<_, HANDLES>::mount(device, &mut block) {
        Ok(mounted) => mounted,
        Err(error) => {
            report!(platform, "MOLT_FS_FAILED: {error:?}");
            return;
        }
    };
    report!(platform, "MOLT_FS_OK: generation {}", fs.generation());

    let root = match fs.root(OWNER) {
        Ok(root) => root,
        Err(error) => {
            report!(platform, "MOLT_FS_FAILED: {error:?}");
            return;
        }
    };
    fs.seal();

    let mut bytes = [0u8; WINDOW];
    bytes[..WRITTEN.len()].copy_from_slice(WRITTEN);
    let mut registry = BufferRegistry::<1>::new();
    let scratch = registry.register_read_write(OWNER, &mut bytes).expect("free buffer slot");
    let write_result = (|| {
        let source = registry.read_capability(scratch)?;
        let target = registry.write_capability(scratch)?;
        let name = Name::try_from("runtime.txt")?;
        let created =
            fs.apply(OWNER, FsOp::Create { dir: root, name, kind: Kind::File }, &mut registry)?;
        let Some(Handle::File(file)) = created.handle() else {
            return Err(FsError::Kind);
        };
        let written = fs.apply(
            OWNER,
            FsOp::Write { file, buffer: BufferOperation::new(source, 0, WRITTEN.len()), offset: 0 },
            &mut registry,
        )?;
        if written != FsDone::Written(WRITTEN.len()) {
            return Err(FsError::Corrupt);
        }
        let generation = match fs.apply(OWNER, FsOp::Sync, &mut registry)? {
            FsDone::Synced(generation) => generation,
            _ => return Err(FsError::Corrupt),
        };
        let window = BufferOperation::new(target, 0, WRITTEN.len());
        if fs.apply(OWNER, FsOp::Read { file, buffer: window, offset: 0 }, &mut registry)?
            != FsDone::Read(WRITTEN.len())
            || registry.resolve_write(window)? != WRITTEN
        {
            return Err(FsError::Corrupt);
        }
        fs.apply(OWNER, FsOp::Close(Handle::File(file)), &mut registry)?;
        Ok(generation)
    })();
    match write_result {
        Ok(generation) => report!(platform, "MOLT_FS_WRITE_OK: generation {generation}"),
        Err(error) => {
            report!(platform, "MOLT_FS_FAILED: {error:?}");
            return;
        }
    }
    let buffers = RefCell::new(registry);
    let mut ring = IoRing::<FsOp, Result<FsDone, FsError>, RING>::new();
    let (client, mut driver) = ring.split();
    let session = Session::new(client, &buffers, scratch, WINDOW).expect("registered scratch");

    // The console borrows the serial port for as long as the shell runs, so the
    // marker below waits until it is given back.
    let ran = {
        let mut out = Serial(platform.serial());
        drive(
            async {
                let mut shell = Shell::new(session, root);
                shell.script(SCRIPT, &mut out).await
            },
            || {
                fs.serve(OWNER, &mut driver, &mut buffers.borrow_mut());
            },
        )
    };
    ran.expect("shell that meets only errors it can print");
    report!(platform, "MOLT_SHELL_OK: script ran to the end");
}

/// The shell's console, which is the port the kernel reports on.
struct Serial<'s, S: SerialPort + ?Sized>(&'s mut S);

impl<S: SerialPort + ?Sized> Console for Serial<'_, S> {
    fn write(&mut self, bytes: &[u8]) {
        self.0.write_bytes(bytes);
    }
}
