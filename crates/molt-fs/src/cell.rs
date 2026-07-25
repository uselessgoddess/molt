//! The filesystem as a cell a supervisor starts, restarts, and outlives.
//!
//! Other cells depend on the filesystem, so it cannot be a library each of them
//! links a copy of: it is one service, started once with the device it owns,
//! reached over a ring. [`FsCell`] is that service's lifecycle — mount, serve,
//! and a restart that puts the volume back at its last durable checkpoint —
//! while [`Fs`] stays the protocol underneath it.

use molt_block::Disk;
use molt_core::cell::RestartHooks;

use crate::FsError;
use crate::service::Fs;

/// Whether the service still has a filesystem behind it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FsState {
    /// Mounted, answering requests.
    Running,
    /// A restart got past its hooks and could not remount.
    Failed,
}

/// A mounted filesystem with a restart of its own.
pub struct FsCell<D, const N: usize> {
    fs: Fs<D, N>,
    generation: u64,
    state: FsState,
}

impl<D: Disk, const N: usize> FsCell<D, N> {
    /// Mounts `device` as the system's filesystem service.
    pub fn start(device: D) -> Result<Self, FsError> {
        Ok(Self { fs: Fs::mount(device)?, generation: 0, state: FsState::Running })
    }

    /// The service other cells talk to, while there is one.
    ///
    /// A cell whose restart failed answers [`FsError::Failed`] from here on:
    /// see [`restart`](Self::restart) for what is left behind by then.
    pub fn fs(&mut self) -> Result<&mut Fs<D, N>, FsError> {
        match self.state {
            FsState::Running => Ok(&mut self.fs),
            FsState::Failed => Err(FsError::Failed),
        }
    }

    /// Whether the service is still serving.
    pub const fn state(&self) -> FsState {
        self.state
    }

    /// How many times this service has restarted.
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// The checkpoint the mounted volume carries.
    pub fn checkpoint(&self) -> u64 {
        self.fs.generation()
    }

    /// Stops the service and brings it back on the same device.
    ///
    /// Submissions stop, in-flight requests are cancelled, and capabilities are
    /// revoked before the volume is remounted, in that order: a restart that
    /// let one more submission in would answer it from a filesystem that is
    /// half gone. What was synced survives; what was not is what a power cut
    /// would have taken.
    ///
    /// A remount that fails leaves the cell [`FsState::Failed`] and keeps it
    /// there: the hooks have run by then, so handles are already revoked and the
    /// tree already dropped, and there is nothing left to serve a request from.
    /// Every later call answers [`FsError::Failed`] — including another restart,
    /// because retrying would revoke a second time and still find the same
    /// unreadable disk. Recovery is the supervisor's, by tearing the cell down
    /// and starting one on a device that mounts.
    pub fn restart(&mut self, hooks: &mut impl RestartHooks) -> Result<(), FsError> {
        self.fs()?;
        hooks.stop_submissions();
        hooks.cancel_requests();
        hooks.revoke_capabilities();
        self.fs.restart().inspect_err(|_| self.state = FsState::Failed)?;
        self.generation = self.generation.wrapping_add(1);
        Ok(())
    }
}

#[cfg(all(test, feature = "format"))]
mod tests {
    use alloc::vec::Vec;
    use core::cell::Cell;

    use molt_block::{BlockError, Device, Disk, Loopback};
    use molt_core::buffer::{BufferOperation, BufferRegistry};
    use molt_core::capability::{CapabilityError, CellId, Read};
    use molt_core::cell::RestartHooks;

    use super::{FsCell, FsState};
    use crate::format::{Tree, build};
    use crate::layout::Kind;
    use crate::op::{FsDone, FsOp, Handle};
    use crate::{FsError, Name};

    const OWNER: CellId = CellId::new(7);

    /// A disk that stops answering the moment `gone` is set.
    ///
    /// [`molt_block::Fault`] only injects into writes and flushes, and a remount
    /// is all reads, so a device that can lose its reads has to be local.
    struct Unplug<'a> {
        disk: Loopback<'a>,
        gone: &'a Cell<bool>,
    }

    impl Device for Unplug<'_> {
        fn sectors(&self) -> u64 {
            self.disk.sectors()
        }

        fn read(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), BlockError> {
            match self.gone.get() {
                true => Err(BlockError::Device),
                false => self.disk.read(sector, buf),
            }
        }
    }

    impl Disk for Unplug<'_> {
        fn write(&mut self, sector: u64, buf: &[u8]) -> Result<(), BlockError> {
            self.disk.write(sector, buf)
        }

        fn flush(&mut self) -> Result<(), BlockError> {
            self.disk.flush()
        }
    }

    #[derive(Default)]
    struct Order(Vec<&'static str>);

    impl RestartHooks for Order {
        fn stop_submissions(&mut self) {
            self.0.push("stop");
        }

        fn cancel_requests(&mut self) {
            self.0.push("cancel");
        }

        fn revoke_capabilities(&mut self) {
            self.0.push("revoke");
        }
    }

    fn image() -> Vec<u8> {
        build(&Tree::new(), 1).unwrap()
    }

    /// Creates `name` holding `source`, syncing only if asked.
    fn write(
        cell: &mut FsCell<Loopback<'_>, 4>,
        buffers: &mut BufferRegistry<'_, 1>,
        buffer: BufferOperation<Read>,
        name: &str,
        sync: bool,
    ) -> Result<(), FsError> {
        let root = cell.fs()?.root(OWNER)?;
        let name = Name::try_from(name)?;
        let create = FsOp::Create { dir: root, name, kind: Kind::File };
        let created = cell.fs()?.apply(OWNER, create, buffers)?;
        let Some(Handle::File(file)) = created.handle() else {
            return Err(FsError::Kind);
        };
        cell.fs()?.apply(OWNER, FsOp::Write { file, buffer, offset: 0 }, buffers)?;
        if sync {
            cell.fs()?.apply(OWNER, FsOp::Sync, buffers)?;
        }
        Ok(())
    }

    #[test]
    fn restart_keeps_synced() -> Result<(), FsError> {
        let mut bytes = image();
        let mut source = *b"durable molt";
        let mut buffers = BufferRegistry::<1>::new();
        let buffer = buffers.register_read(OWNER, &mut source).unwrap();
        let window = BufferOperation::new(buffer, 0, 12);
        let mut cell = FsCell::<_, 4>::start(Loopback::writable(&mut bytes)?)?;

        write(&mut cell, &mut buffers, window, "kept.txt", true)?;
        cell.restart(&mut Order::default())?;

        let root = cell.fs()?.root(OWNER)?;
        let name = Name::try_from("kept.txt")?;
        let opened = cell.fs()?.apply(OWNER, FsOp::Open { dir: root, name }, &mut buffers)?;

        assert!(matches!(opened, FsDone::Opened(Handle::File(_))), "{opened:?}");
        assert_eq!(cell.checkpoint(), 2, "sync did not move the checkpoint");
        Ok(())
    }

    #[test]
    fn restart_drops_unsynced() -> Result<(), FsError> {
        let mut bytes = image();
        let mut source = *b"durable molt";
        let mut buffers = BufferRegistry::<1>::new();
        let buffer = buffers.register_read(OWNER, &mut source).unwrap();
        let window = BufferOperation::new(buffer, 0, 12);
        let mut cell = FsCell::<_, 4>::start(Loopback::writable(&mut bytes)?)?;

        write(&mut cell, &mut buffers, window, "lost.txt", false)?;
        cell.restart(&mut Order::default())?;

        let root = cell.fs()?.root(OWNER)?;
        let name = Name::try_from("lost.txt")?;
        let opened = cell.fs()?.apply(OWNER, FsOp::Open { dir: root, name }, &mut buffers);

        assert_eq!(opened, Err(FsError::Missing));
        assert_eq!(cell.checkpoint(), 1, "an unsynced write reached the disk");
        Ok(())
    }

    #[test]
    fn restart_revokes_handles() -> Result<(), FsError> {
        let mut bytes = image();
        let mut buffers = BufferRegistry::<1>::new();
        let mut cell = FsCell::<_, 4>::start(Loopback::writable(&mut bytes)?)?;
        let stale = cell.fs()?.root(OWNER)?;
        cell.fs()?.seal();

        cell.restart(&mut Order::default())?;

        let name = Name::try_from("nothing")?;
        let opened = cell.fs()?.apply(OWNER, FsOp::Open { dir: stale, name }, &mut buffers);

        assert_eq!(opened, Err(FsError::Handle(CapabilityError::Stale)));
        assert!(cell.fs()?.root(OWNER).is_ok(), "seal outlived epoch that set it");
        Ok(())
    }

    #[test]
    fn restart_stops_before_remount() -> Result<(), FsError> {
        let mut bytes = image();
        let mut cell = FsCell::<_, 4>::start(Loopback::writable(&mut bytes)?)?;
        let mut hooks = Order::default();

        cell.restart(&mut hooks)?;
        cell.restart(&mut hooks)?;

        assert_eq!(hooks.0, ["stop", "cancel", "revoke", "stop", "cancel", "revoke"]);
        assert_eq!(cell.generation(), 2);
        Ok(())
    }

    #[test]
    fn failed_remount_stays_failed() -> Result<(), FsError> {
        let mut bytes = image();
        let gone = Cell::new(false);
        let disk = Unplug { disk: Loopback::writable(&mut bytes)?, gone: &gone };
        let mut cell = FsCell::<_, 4>::start(disk)?;
        let mut hooks = Order::default();

        gone.set(true);
        let restarted = cell.restart(&mut hooks);

        assert_eq!(restarted, Err(FsError::Device(BlockError::Device)));
        assert_eq!(cell.state(), FsState::Failed);
        assert_eq!(cell.fs().err(), Some(FsError::Failed));
        assert_eq!(cell.generation(), 0, "a failed remount counted as a restart");

        // Even with the disk back: the hooks of the first attempt already
        // revoked, and a second pass would revoke what a live cell handed out.
        gone.set(false);
        assert_eq!(cell.restart(&mut hooks), Err(FsError::Failed));
        assert_eq!(hooks.0, ["stop", "cancel", "revoke"], "a failed cell ran hooks again");
        Ok(())
    }
}
