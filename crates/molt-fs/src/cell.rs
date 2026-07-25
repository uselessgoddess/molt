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

/// A mounted filesystem with a restart of its own.
pub struct FsCell<D, const N: usize> {
    fs: Fs<D, N>,
    generation: u64,
}

impl<D: Disk, const N: usize> FsCell<D, N> {
    /// Mounts `device` as the system's filesystem service.
    pub fn start(device: D) -> Result<Self, FsError> {
        Ok(Self { fs: Fs::mount(device)?, generation: 0 })
    }

    /// The service other cells talk to.
    pub fn fs(&mut self) -> &mut Fs<D, N> {
        &mut self.fs
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
    /// A remount that fails leaves no service to talk to — the disk is gone or
    /// no checkpoint on it verifies — and the supervisor tears the cell down
    /// rather than restarting it again.
    pub fn restart(&mut self, hooks: &mut impl RestartHooks) -> Result<(), FsError> {
        hooks.stop_submissions();
        hooks.cancel_requests();
        hooks.revoke_capabilities();
        self.fs.restart()?;
        self.generation = self.generation.wrapping_add(1);
        Ok(())
    }
}

#[cfg(all(test, feature = "format"))]
mod tests {
    use alloc::vec::Vec;

    use molt_block::Loopback;
    use molt_core::buffer::{BufferOperation, BufferRegistry};
    use molt_core::capability::{CapabilityError, CellId, Read};
    use molt_core::cell::RestartHooks;

    use super::FsCell;
    use crate::format::{Tree, build};
    use crate::layout::Kind;
    use crate::op::{FsDone, FsOp, Handle};
    use crate::{FsError, Name};

    const OWNER: CellId = CellId::new(7);

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
        let root = cell.fs().root(OWNER)?;
        let name = Name::try_from(name)?;
        let create = FsOp::Create { dir: root, name, kind: Kind::File };
        let created = cell.fs().apply(OWNER, create, buffers)?;
        let Some(Handle::File(file)) = created.handle() else {
            return Err(FsError::Kind);
        };
        cell.fs().apply(OWNER, FsOp::Write { file, buffer, offset: 0 }, buffers)?;
        if sync {
            cell.fs().apply(OWNER, FsOp::Sync, buffers)?;
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

        let root = cell.fs().root(OWNER)?;
        let name = Name::try_from("kept.txt")?;
        let opened = cell.fs().apply(OWNER, FsOp::Open { dir: root, name }, &mut buffers)?;

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

        let root = cell.fs().root(OWNER)?;
        let name = Name::try_from("lost.txt")?;
        let opened = cell.fs().apply(OWNER, FsOp::Open { dir: root, name }, &mut buffers);

        assert_eq!(opened, Err(FsError::Missing));
        assert_eq!(cell.checkpoint(), 1, "an unsynced write reached the disk");
        Ok(())
    }

    #[test]
    fn restart_revokes_handles() -> Result<(), FsError> {
        let mut bytes = image();
        let mut buffers = BufferRegistry::<1>::new();
        let mut cell = FsCell::<_, 4>::start(Loopback::writable(&mut bytes)?)?;
        let stale = cell.fs().root(OWNER)?;
        cell.fs().seal();

        cell.restart(&mut Order::default())?;

        let name = Name::try_from("nothing")?;
        let opened = cell.fs().apply(OWNER, FsOp::Open { dir: stale, name }, &mut buffers);

        assert_eq!(opened, Err(FsError::Handle(CapabilityError::Stale)));
        assert!(cell.fs().root(OWNER).is_ok(), "seal outlived epoch that set it");
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
}
