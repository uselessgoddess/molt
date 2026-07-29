//! Starting the system: two cells, one namespace, and the tick between them.
//!
//! Init owns the device and the memory, and nothing else. It starts the
//! filesystem service, lets it publish a mount under [`Storage`], starts the
//! shell as a cell of its own, and then does the one thing a system with two
//! cells can do that a system with one cannot: it counts. A line the shell
//! finishes is a heartbeat; a line it does not finish is a missed one, and the
//! supervisor restarts what stops reporting. Nothing here hands the shell a
//! capability — it asks the namespace for one — which is why the service can
//! restart under it and why the shell can be restarted at all.

use core::cell::RefCell;

use molt_arch::{Platform, SerialPort, SerialWriter};
use molt_block::Disk;
use molt_core::buffer::{BufferOperation, BufferRegistry};
use molt_core::capability::{Capability, CellId, ReadWrite};
use molt_core::cell::Supervisor;
use molt_core::cpu::CpuId;
use molt_core::registry::{Registry, Scheme};
use molt_core::ring::{IoDriver, IoRing};
use molt_fs::{
    Disconnect, Fs, FsCell, FsDone, FsError, FsOp, Handle, Kind, Name, Storage, Teardown,
};
use molt_kernel::report;
use molt_shell::{Console, PROMPT, Session, Shell, ShellError, drive_bounded};

/// Init, which owns the memory both sides of the ring look at.
const INIT: CellId = CellId::new(1);
/// The filesystem service.
const FS: CellId = CellId::new(2);
/// The shell, which is a client and nothing more.
const SHELL: CellId = CellId::new(3);

/// The bootstrap root, the published one, one entry the shell has open, and
/// room to be wrong about that.
const HANDLES: usize = 4;
const RING: usize = 4;
/// One scheme is published today, and it is storage.
const NAMES: usize = 1;
/// Deliberately smaller than the files on the disk, so `cat` has to loop.
const WINDOW: usize = 64;
/// More polls than the longest line needs, so that only a line nothing answers
/// runs out of them.
const ROUNDS: usize = 32;
/// How many ticks a cell may go without reporting before it is restarted.
const DEADLINE: u64 = 1;
const WRITTEN: &[u8] = b"written through virtio";

/// What happens to the system while a line runs.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Event {
    /// Nothing but the command.
    Quiet,
    /// The service restarts first, so the lease in the shell's hand names a
    /// mount that is gone and the line has to ask the namespace again.
    Republish,
    /// Nobody serves the ring, so the line cannot finish and the tick it would
    /// have reported never happens.
    Starve,
}

/// The lines the shell runs, and what it meets while running them.
///
/// The last line is the interesting one: it prints a file, which it can only do
/// if it re-acquired storage after the service restarted under it and if it
/// came back from the restart its own supervisor gave it.
const SCRIPT: &[(&[u8], Event)] = &[
    (b"help", Event::Quiet),
    (b"ls", Event::Quiet),
    (b"ls docs", Event::Quiet),
    (b"ls nowhere", Event::Quiet),
    (b"ls", Event::Republish),
    (b"ls", Event::Starve),
    (b"ls docs", Event::Starve),
    (b"cat hello.txt", Event::Quiet),
];

/// Whatever stopped init, from either side of the ring.
#[derive(Debug)]
enum Trouble {
    /// The service could not do something init asked of it directly.
    Fs(FsError),
    /// The shell met an error it could not print, which is a shell bug.
    Shell(ShellError),
}

impl From<FsError> for Trouble {
    fn from(error: FsError) -> Self {
        Self::Fs(error)
    }
}

impl From<ShellError> for Trouble {
    fn from(error: ShellError) -> Self {
        Self::Shell(error)
    }
}

pub fn smoke<P: Platform>(platform: &mut P, device: impl Disk) {
    let mut service = match Supervisor::<FsCell<_, HANDLES>>::new(device) {
        Ok(started) => started,
        Err(error) => {
            report!(platform, "MOLT_FS_FAILED: {error:?}");
            return;
        }
    };
    report!(platform, "MOLT_FS_OK: generation {}", service.cell().checkpoint());
    match run(platform, &mut service) {
        Ok(()) => {}
        Err(Trouble::Fs(error)) => report!(platform, "MOLT_FS_FAILED: {error:?}"),
        Err(Trouble::Shell(error)) => report!(platform, "MOLT_SHELL_FAILED: {error:?}"),
    }
}

/// Runs the write cycle, the script, and the restart that outlives both.
fn run<P: Platform, D: Disk>(
    platform: &mut P,
    service: &mut Supervisor<FsCell<D, HANDLES>>,
) -> Result<(), Trouble> {
    let mut bytes = [0u8; WINDOW];
    bytes[..WRITTEN.len()].copy_from_slice(WRITTEN);
    let mut registry = BufferRegistry::<1>::new();
    let scratch = registry.register_read_write(INIT, &mut bytes).map_err(|_| FsError::Handles)?;
    let generation = commit(service.cell_mut().fs()?, &mut registry, scratch)?;
    report!(platform, "MOLT_FS_WRITE_OK: generation {generation}");

    let buffers = RefCell::new(registry);
    let names = RefCell::new(Registry::<Storage, NAMES>::new());
    let mut ring = IoRing::<FsOp, Result<FsDone, FsError>, RING>::new();
    let (client, mut driver) = ring.split();
    service.cell_mut().fs()?.publish(&mut names.borrow_mut(), FS, CpuId::BOOT)?;

    let session = Session::new(client, &buffers, &names, scratch, WINDOW)?;
    let mut shell = Supervisor::<Shell<'_, '_, '_, RING, 1, NAMES>>::new(session)?;
    script(platform, service, &mut shell, &mut driver, &buffers, &names)?;
    report!(platform, "MOLT_SHELL_OK: script ran to the end");

    // The script is over and the ring is empty, but the hooks are the ones a
    // restart under load runs, so the epoch ends the same way either way.
    service.restart(&mut Teardown::new(&mut driver, &names, FS))?;
    let published = service.cell_mut().fs()?.publish(&mut names.borrow_mut(), FS, CpuId::BOOT)?;
    let root = names.borrow().endpoint(published).map_err(FsError::Handle)?.root();
    let open = FsOp::Open { dir: root, name: Name::try_from("runtime.txt")? };
    let opened = service.cell_mut().fs()?.apply(FS, open, &mut buffers.borrow_mut())?;
    let Some(Handle::File(_)) = opened.handle() else {
        return Err(FsError::Kind.into());
    };
    report!(
        platform,
        "MOLT_FS_RESTART_OK: restart {} at generation {}",
        service.generation(),
        service.cell().checkpoint()
    );
    Ok(())
}

/// Runs every line of the script, watching the shell between them.
///
/// The tick is a line rather than a timer because a line is what this system
/// counts as progress: nothing preempts a cell here, so the only place a
/// supervisor can look at the clock is between the pieces of work it hands out.
fn script<P: Platform, D: Disk>(
    platform: &mut P,
    service: &mut Supervisor<FsCell<D, HANDLES>>,
    shell: &mut Supervisor<Shell<'_, '_, '_, RING, 1, NAMES>>,
    driver: &mut IoDriver<'_, FsOp, Result<FsDone, FsError>, RING>,
    buffers: &RefCell<BufferRegistry<'_, 1>>,
    names: &RefCell<Registry<Storage, NAMES>>,
) -> Result<(), Trouble> {
    let mut tick = 0;
    for &(line, event) in SCRIPT {
        tick += 1;
        if event == Event::Republish {
            let stale = names.borrow().acquire().map_err(FsError::from)?;
            service.restart(&mut Teardown::new(driver, names, FS))?;
            let published =
                service.cell_mut().fs()?.publish(&mut names.borrow_mut(), FS, CpuId::BOOT)?;
            if names.borrow().endpoint(stale).is_ok() {
                // A lease that outlived its publication would make every later
                // restart invisible to whoever holds one.
                return Err(FsError::Corrupt.into());
            }
            // Reported from the new publication rather than from the cell, so
            // the number is the one a client acquiring now would find.
            let mount = names.borrow().endpoint(published).map_err(FsError::Handle)?.checkpoint();
            report!(
                platform,
                "MOLT_REGISTRY_OK: {} republished at generation {mount}, older leases stale",
                Storage::NAME
            );
        }

        // The console holds the serial port for as long as the line runs, so
        // anything reported about the line waits until it is given back.
        let finished = {
            let mut out = Serial(platform.serial());
            out.write(PROMPT);
            out.line(line);
            let running = shell.cell_mut().run(line, &mut out);
            match event {
                Event::Starve => drive_bounded(running, ROUNDS, || {}),
                _ => drive_bounded(running, ROUNDS, || {
                    if let Ok(fs) = service.cell_mut().fs() {
                        fs.serve(SHELL, driver, &mut buffers.borrow_mut());
                    }
                }),
            }
        };
        // A line that ran out of polls reports nothing, and whether that is a
        // fault is the deadline's to say rather than this loop's.
        if let Some(result) = finished {
            result?;
            shell.record_heartbeat(tick);
        }

        let hooks = &mut Disconnect::new(driver, service.cell_mut().fs()?, SHELL);
        if let Some(restarted) = shell.watch(tick, DEADLINE, hooks) {
            restarted?;
            report!(
                platform,
                "MOLT_WATCHDOG_OK: shell restart {} at tick {tick}, {} request(s) cancelled",
                shell.generation(),
                hooks.cancelled()
            );
        }
    }
    Ok(())
}

/// Creates a file, writes it through the device, and makes it durable.
///
/// Init does this with the mount in hand rather than over the ring: it is the
/// one write the smoke test has to see land on a real disk, and no command the
/// shell offers would make one. The bootstrap root is closed again afterwards,
/// so the only root left when the script starts is the published one.
fn commit<D: Disk>(
    fs: &mut Fs<D, HANDLES>,
    buffers: &mut BufferRegistry<'_, 1>,
    scratch: Capability<ReadWrite>,
) -> Result<u64, FsError> {
    let source = buffers.read_capability(scratch)?;
    let target = buffers.write_capability(scratch)?;
    let root = fs.root(INIT)?;
    let name = Name::try_from("runtime.txt")?;
    let created = fs.apply(INIT, FsOp::Create { dir: root, name, kind: Kind::File }, buffers)?;
    let Some(Handle::File(file)) = created.handle() else {
        return Err(FsError::Kind);
    };
    let window = BufferOperation::new(source, 0, WRITTEN.len());
    if fs.apply(INIT, FsOp::Write { file, buffer: window, offset: 0 }, buffers)?
        != FsDone::Written(WRITTEN.len())
    {
        return Err(FsError::Corrupt);
    }
    let FsDone::Synced(generation) = fs.apply(INIT, FsOp::Sync, buffers)? else {
        return Err(FsError::Corrupt);
    };
    let window = BufferOperation::new(target, 0, WRITTEN.len());
    if fs.apply(INIT, FsOp::Read { file, buffer: window, offset: 0 }, buffers)?
        != FsDone::Read(WRITTEN.len())
        || buffers.resolve_write(window)? != WRITTEN
    {
        return Err(FsError::Corrupt);
    }
    fs.apply(INIT, FsOp::Close(Handle::File(file)), buffers)?;
    fs.apply(INIT, FsOp::Close(Handle::Dir(root)), buffers)?;
    Ok(generation)
}

/// The shell's console, which is the port the kernel reports on.
struct Serial<'s, S: SerialPort + ?Sized>(&'s mut S);

impl<S: SerialPort + ?Sized> Console for Serial<'_, S> {
    fn write(&mut self, bytes: &[u8]) {
        self.0.write_bytes(bytes);
    }
}
