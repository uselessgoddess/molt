//! The filesystem as a cell a supervisor starts, restarts, and outlives.
//!
//! Other cells depend on the filesystem, so it cannot be a library each of them
//! links a copy of: it is one service, started once with the device it owns,
//! reached over a ring. [`FsCell`] is that service's lifecycle — mount, serve,
//! and a restart that puts the volume back at its last durable checkpoint —
//! while [`Fs`] stays the protocol underneath it.

use molt_block::Disk;
use molt_core::cell::{Cell, Health};

use crate::FsError;
use crate::service::Fs;

/// A mounted filesystem with a restart of its own.
pub struct FsCell<D, const N: usize> {
    fs: Fs<D, N>,
    health: Health,
}

impl<D: Disk, const N: usize> FsCell<D, N> {
    /// The service other cells talk to, while there is one.
    ///
    /// A cell whose restart failed answers [`FsError::Failed`] from here on:
    /// see [`restart`](Cell::restart) for what is left behind by then.
    pub fn fs(&mut self) -> Result<&mut Fs<D, N>, FsError> {
        match self.health {
            Health::Running => Ok(&mut self.fs),
            Health::Failed => Err(FsError::Failed),
        }
    }

    /// Whether the service is still serving.
    pub const fn health(&self) -> Health {
        self.health
    }

    /// The checkpoint the mounted volume carries, which counts syncs rather
    /// than restarts.
    pub fn checkpoint(&self) -> u64 {
        self.fs.generation()
    }
}

impl<D: Disk, const N: usize> Cell for FsCell<D, N> {
    /// The device to mount, which the cell then owns for its life: a restart
    /// remounts it rather than asking for another.
    type State = D;
    type Error = FsError;

    fn spawn(device: D) -> Result<Self, FsError> {
        Ok(Self { fs: Fs::mount(device)?, health: Health::Running })
    }

    /// Brings the volume back at its last durable checkpoint.
    ///
    /// The supervisor has stopped submissions, cancelled in-flight requests,
    /// and revoked capabilities by now, so the remount cannot answer a request
    /// from a filesystem that is half gone. What was synced survives; what was
    /// not is what a power cut would have taken.
    ///
    /// A remount that fails leaves the cell [`Health::Failed`] and keeps it
    /// there: the hooks have run by then, so handles are already revoked and
    /// the tree already dropped, and there is nothing left to serve a request
    /// from. Every later call answers [`FsError::Failed`] — including another
    /// restart, because the disk that would not mount is still the disk.
    fn restart(&mut self) -> Result<(), FsError> {
        self.fs()?;
        self.fs.restart().inspect_err(|_| self.health = Health::Failed)
    }
}

#[cfg(all(test, feature = "format"))]
mod tests {
    use alloc::vec::Vec;
    use core::cell::Cell as Flag;

    use molt_block::{BlockError, Device, Disk, Loopback};
    use molt_core::buffer::{BufferOperation, BufferRegistry};
    use molt_core::capability::{CapabilityError, CellId, Read};
    use molt_core::cell::{Cell, Health, RestartHooks, Supervisor};

    use super::FsCell;
    use crate::format::{Tree, build};
    use crate::layout::Kind;
    use crate::op::{FsDone, FsOp, Handle};
    use crate::{FsError, Name};

    const OWNER: CellId = CellId::new(7);

    struct Unplug<'a> {
        disk: Loopback<'a>,
        gone: &'a Flag<bool>,
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
        let mut cell = FsCell::<_, 4>::spawn(Loopback::writable(&mut bytes)?)?;

        write(&mut cell, &mut buffers, window, "kept.txt", true)?;
        cell.restart()?;

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
        let mut cell = FsCell::<_, 4>::spawn(Loopback::writable(&mut bytes)?)?;

        write(&mut cell, &mut buffers, window, "lost.txt", false)?;
        cell.restart()?;

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
        let mut cell = FsCell::<_, 4>::spawn(Loopback::writable(&mut bytes)?)?;
        let stale = cell.fs()?.root(OWNER)?;
        cell.fs()?.seal();

        cell.restart()?;

        let name = Name::try_from("nothing")?;
        let opened = cell.fs()?.apply(OWNER, FsOp::Open { dir: stale, name }, &mut buffers);

        assert_eq!(opened, Err(FsError::Handle(CapabilityError::Stale)));
        assert!(cell.fs()?.root(OWNER).is_ok(), "seal outlived epoch that set it");
        Ok(())
    }

    /// A borrowed disk is what makes this the test of the `'static`-free bound:
    /// the supervisor takes the cell whether or not its device outlives it.
    #[test]
    fn supervised_restart_is_ordered() -> Result<(), FsError> {
        let mut bytes = image();
        let mut supervisor = Supervisor::<FsCell<_, 4>>::new(Loopback::writable(&mut bytes)?)?;
        let mut hooks = Order::default();

        supervisor.restart(&mut hooks)?;
        supervisor.restart(&mut hooks)?;

        assert_eq!(hooks.0, ["stop", "cancel", "revoke", "stop", "cancel", "revoke"]);
        assert_eq!(supervisor.generation(), 2);
        Ok(())
    }

    #[test]
    fn failed_remount_stays_failed() -> Result<(), FsError> {
        let mut bytes = image();
        let gone = Flag::new(false);
        let disk = Unplug { disk: Loopback::writable(&mut bytes)?, gone: &gone };
        let mut cell = FsCell::<_, 4>::spawn(disk)?;

        gone.set(true);
        let restarted = cell.restart();

        assert_eq!(restarted, Err(FsError::Device(BlockError::Device)));
        assert_eq!(cell.health(), Health::Failed);
        assert_eq!(cell.fs().err(), Some(FsError::Failed));

        // Even with the disk back: the hooks have run, so the epoch that held
        // handles is over and there is nothing left to remount into.
        gone.set(false);
        assert_eq!(cell.restart(), Err(FsError::Failed));
        Ok(())
    }
}
