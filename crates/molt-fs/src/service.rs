//! The filesystem as something other cells can talk to.
//!
//! [`Fs`] is the only holder of the mounted volume: everyone else names objects
//! through a capability table it owns and reaches it over a ring. The table
//! caches only an object id and kind; size and entry count are replayed when
//! asked because writes can change them under an open handle.

use molt_block::Writable;
use molt_core::buffer::BufferRegistry;
use molt_core::capability::{Capability, CapabilityTable, CellId};
use molt_core::ring::{Completion, IoDriver};

use crate::layout::{BLOCK, Kind, Object};
use crate::op::{Dir, File, FsDone, FsOp, Handle, Stat};
use crate::{FsError, Journal};

#[derive(Clone, Copy)]
struct OpenObject {
    id: u32,
    kind: Kind,
}

/// A mounted volume behind a capability table.
pub struct Fs<'buf, D, const N: usize> {
    journal: Journal<'buf, D>,
    open: CapabilityTable<OpenObject, N>,
    pending: Option<Completion<Result<FsDone, FsError>>>,
    sealed: bool,
}

impl<'buf, D: Writable, const N: usize> Fs<'buf, D, N> {
    /// Mounts `device`, using `block` as the volume's only buffer.
    pub fn mount(device: D, block: &'buf mut [u8; BLOCK]) -> Result<Self, FsError> {
        Ok(Self {
            journal: Journal::mount(device, block)?,
            open: CapabilityTable::new(),
            pending: None,
            sealed: false,
        })
    }

    /// The checkpoint the mounted volume carries.
    pub fn generation(&self) -> u64 {
        self.journal.generation()
    }

    /// Hands `owner` a handle to the root directory.
    ///
    /// This is the one handle that comes from nowhere: every other is opened
    /// through a directory somebody already holds, and no [`FsOp`] mints it, so
    /// only code with the mounted `Fs` in hand — init at bootstrap — can bless
    /// the first holder. [`seal`](Self::seal) shuts the door afterwards, and
    /// then this returns [`FsError::Sealed`].
    pub fn root(&mut self, owner: CellId) -> Result<Capability<Dir>, FsError> {
        if self.sealed {
            return Err(FsError::Sealed);
        }
        let root = self.journal.root();
        let object = self.journal.object(root)?;
        if object.kind != Kind::Dir {
            return Err(FsError::Kind);
        }
        self.open
            .insert::<Dir>(owner, OpenObject { id: root, kind: object.kind })
            .map_err(|_| FsError::Handles)
    }

    /// Closes the root bootstrap for good, so no later caller can grant one.
    ///
    /// One-way by design: init hands out the roots the system starts with, then
    /// seals, and the authority to mint another is gone for the mount's life.
    pub fn seal(&mut self) {
        self.sealed = true;
    }

    /// Performs one operation, handing any new handle to `owner`.
    ///
    /// Ownership decides only who loses a handle when a cell restarts. Holding
    /// the capability is the authority to use it, so `owner` is not checked
    /// against the handles an operation names.
    pub fn apply<const M: usize>(
        &mut self,
        owner: CellId,
        op: FsOp,
        buffers: &mut BufferRegistry<'_, M>,
    ) -> Result<FsDone, FsError> {
        match op {
            FsOp::Open { dir, name } => {
                let parent = *self.open.get(dir)?;
                let id = self.journal.lookup(parent.id, &name)?;
                let object = self.journal.object(id)?;
                self.hold(owner, id, object.kind).map(FsDone::Opened)
            }
            FsOp::Entry { dir, index } => {
                let parent = *self.open.get(dir)?;
                let (name, object) = self.journal.entry(parent.id, index)?;
                Ok(FsDone::Entry { name, stat: stat(&self.journal.object(object)?) })
            }
            FsOp::Read { file, buffer, offset } => {
                let object = *self.open.get(file)?;
                let target = buffers.resolve_write(buffer)?;
                self.journal.read(object.id, offset, target).map(FsDone::Read)
            }
            FsOp::Create { dir, name, kind } => {
                let parent = *self.open.get(dir)?;
                let object = self.journal.create(parent.id, name, kind)?;
                self.hold(owner, object, kind).map(FsDone::Opened)
            }
            FsOp::Write { file, buffer, offset } => {
                let object = *self.open.get(file)?;
                let source = buffers.resolve_read(buffer)?;
                self.journal.write(object.id, offset, source).map(FsDone::Written)
            }
            FsOp::Sync => self.journal.sync().map(FsDone::Synced),
            FsOp::Stat(handle) => Ok(FsDone::Stat(stat(&self.object(handle)?))),
            FsOp::Close(handle) => {
                match handle {
                    Handle::Dir(dir) => self.open.revoke(dir)?,
                    Handle::File(file) => self.open.revoke(file)?,
                };
                Ok(FsDone::Closed)
            }
        }
    }

    /// Drains the submission queue, answering every operation it held.
    ///
    /// Returns how many completions were published, so a caller can tell a
    /// round that did work from one blocked behind a full completion queue.
    pub fn serve<const M: usize, const R: usize>(
        &mut self,
        owner: CellId,
        driver: &mut IoDriver<'_, FsOp, Result<FsDone, FsError>, R>,
        buffers: &mut BufferRegistry<'_, M>,
    ) -> usize {
        let mut served = 0;
        if let Some(completion) = self.pending.take() {
            match driver.try_complete(completion) {
                Ok(()) => served += 1,
                Err(completion) => {
                    self.pending = Some(completion);
                    return served;
                }
            }
        }
        while let Some(submission) = driver.try_next() {
            let id = submission.id();
            let result = self.apply(owner, submission.into_operation(), buffers);
            match driver.try_complete(Completion::new(id, result)) {
                Ok(()) => served += 1,
                Err(completion) => {
                    self.pending = Some(completion);
                    break;
                }
            }
        }
        served
    }

    /// Drops every handle `owner` holds, as restarting that cell must.
    pub fn revoke(&mut self, owner: CellId) -> usize {
        self.open.revoke_owner(owner)
    }

    fn hold(&mut self, owner: CellId, id: u32, kind: Kind) -> Result<Handle, FsError> {
        let object = OpenObject { id, kind };
        match kind {
            Kind::Dir => self.open.insert::<Dir>(owner, object).map(Handle::Dir),
            Kind::File => self.open.insert::<File>(owner, object).map(Handle::File),
        }
        .map_err(|_| FsError::Handles)
    }

    fn object(&mut self, handle: Handle) -> Result<Object, FsError> {
        let object = match handle {
            Handle::Dir(dir) => *self.open.get(dir)?,
            Handle::File(file) => *self.open.get(file)?,
        };
        let current = self.journal.object(object.id)?;
        if object.kind != current.kind {
            return Err(FsError::Corrupt);
        }
        Ok(current)
    }
}

/// A directory counts entries and has no length; a file is the reverse.
fn stat(object: &Object) -> Stat {
    match object.kind {
        Kind::Dir => Stat { kind: Kind::Dir, size: 0, entries: object.count },
        Kind::File => Stat { kind: Kind::File, size: object.size, entries: 0 },
    }
}

#[cfg(all(test, feature = "format"))]
mod tests {
    use molt_block::Loopback;
    use molt_core::buffer::{BufferOperation, BufferRegistry};
    use molt_core::capability::{CapabilityError, CellId};
    use molt_core::ring::{IoRing, RequestId, Submission};

    use super::Fs;
    use crate::format::{Tree, build};
    use crate::layout::{BLOCK, Kind};
    use crate::op::{FsDone, FsOp, Handle, Stat};
    use crate::{FsError, Name};

    const OWNER: CellId = CellId::new(4);

    fn image() -> alloc::vec::Vec<u8> {
        let mut tree = Tree::new();
        tree.file("hello.txt", b"hello, molt".to_vec()).expect("legal name");
        tree.dir("docs").expect("legal name").file("readme", b"read me".to_vec()).unwrap();
        build(&tree, 1).expect("image that fits")
    }

    fn name(text: &str) -> Name {
        Name::try_from(text).expect("legal name")
    }

    #[test]
    fn open_walks_from_root_handle() {
        let bytes = image();
        let mut block = [0u8; BLOCK];
        let mut fs = Fs::<_, 4>::mount(Loopback::new(&bytes).unwrap(), &mut block).expect("mount");
        let mut buffers = BufferRegistry::<1>::new();

        let root = fs.root(OWNER).expect("root handle");
        let opened = fs
            .apply(OWNER, FsOp::Open { dir: root, name: name("docs") }, &mut buffers)
            .expect("open directory");

        let Some(Handle::Dir(docs)) = opened.handle() else {
            panic!("the root's subdirectory opened as a file: {opened:?}");
        };
        let readme = fs.apply(OWNER, FsOp::Open { dir: docs, name: name("readme") }, &mut buffers);

        assert!(matches!(readme, Ok(FsDone::Opened(Handle::File(_)))), "{readme:?}");
    }

    #[test]
    fn read_lands_in_registered_buffer() {
        let bytes = image();
        let mut block = [0u8; BLOCK];
        let mut target = [0u8; 32];
        let mut fs = Fs::<_, 4>::mount(Loopback::new(&bytes).unwrap(), &mut block).expect("mount");
        let mut buffers = BufferRegistry::<1>::new();
        let buffer = buffers.register_write(OWNER, &mut target).expect("free slot");

        let root = fs.root(OWNER).expect("root handle");
        let opened = fs
            .apply(OWNER, FsOp::Open { dir: root, name: name("hello.txt") }, &mut buffers)
            .expect("open file");
        let Some(Handle::File(file)) = opened.handle() else {
            panic!("a file opened as a directory: {opened:?}");
        };
        let window = BufferOperation::new(buffer, 4, 11);
        let read = fs.apply(OWNER, FsOp::Read { file, buffer: window, offset: 0 }, &mut buffers);

        assert_eq!(read, Ok(FsDone::Read(11)));
        assert_eq!(buffers.resolve_write(window).expect("same buffer"), b"hello, molt");
    }

    #[test]
    fn create_write_sync_survives_remount() {
        let mut bytes = image();
        let mut source = *b"durable molt";
        let source_len = source.len();
        {
            let mut block = [0u8; BLOCK];
            let mut fs = Fs::<_, 4>::mount(Loopback::writable(&mut bytes).unwrap(), &mut block)
                .expect("mount");
            let mut buffers = BufferRegistry::<1>::new();
            let buffer = buffers.register_read(OWNER, &mut source).expect("free slot");
            let root = fs.root(OWNER).expect("root handle");

            let created = fs
                .apply(
                    OWNER,
                    FsOp::Create { dir: root, name: name("written.txt"), kind: Kind::File },
                    &mut buffers,
                )
                .expect("create");
            let Some(Handle::File(file)) = created.handle() else {
                panic!("new file opened as a directory: {created:?}");
            };
            let write = FsOp::Write {
                file,
                buffer: BufferOperation::new(buffer, 0, source_len),
                offset: 0,
            };

            assert_eq!(fs.apply(OWNER, write, &mut buffers), Ok(FsDone::Written(source_len)));
            assert_eq!(
                fs.apply(OWNER, FsOp::Stat(Handle::File(file)), &mut buffers),
                Ok(FsDone::Stat(Stat { kind: Kind::File, size: source_len as u64, entries: 0 }))
            );
            assert_eq!(fs.apply(OWNER, FsOp::Sync, &mut buffers), Ok(FsDone::Synced(2)));
        }

        let mut block = [0u8; BLOCK];
        let mut target = [0u8; 16];
        let target_len = target.len();
        let mut fs =
            Fs::<_, 4>::mount(Loopback::new(&bytes).unwrap(), &mut block).expect("remount");
        let mut buffers = BufferRegistry::<1>::new();
        let buffer = buffers.register_write(OWNER, &mut target).expect("free slot");
        let root = fs.root(OWNER).expect("root handle");
        let opened = fs
            .apply(OWNER, FsOp::Open { dir: root, name: name("written.txt") }, &mut buffers)
            .expect("open durable file");
        let Some(Handle::File(file)) = opened.handle() else {
            panic!("durable file opened as a directory: {opened:?}");
        };
        let read =
            FsOp::Read { file, buffer: BufferOperation::new(buffer, 0, target_len), offset: 0 };

        assert_eq!(fs.generation(), 2);
        assert_eq!(fs.apply(OWNER, read, &mut buffers), Ok(FsDone::Read(source_len)));
        assert_eq!(&target[..source_len], &source);
    }

    #[test]
    fn stat_counts_entries_of_directory() {
        let bytes = image();
        let mut block = [0u8; BLOCK];
        let mut fs = Fs::<_, 4>::mount(Loopback::new(&bytes).unwrap(), &mut block).expect("mount");
        let mut buffers = BufferRegistry::<1>::new();

        let root = fs.root(OWNER).expect("root handle");
        let stat = fs.apply(OWNER, FsOp::Stat(Handle::Dir(root)), &mut buffers);

        assert_eq!(stat, Ok(FsDone::Stat(Stat { kind: Kind::Dir, size: 0, entries: 2 })));
    }

    #[test]
    fn closed_handle_goes_stale() {
        let bytes = image();
        let mut block = [0u8; BLOCK];
        let mut fs = Fs::<_, 4>::mount(Loopback::new(&bytes).unwrap(), &mut block).expect("mount");
        let mut buffers = BufferRegistry::<1>::new();

        let root = fs.root(OWNER).expect("root handle");
        let closed = fs.apply(OWNER, FsOp::Close(Handle::Dir(root)), &mut buffers);
        let after = fs.apply(OWNER, FsOp::Entry { dir: root, index: 0 }, &mut buffers);

        assert_eq!(closed, Ok(FsDone::Closed));
        assert_eq!(after, Err(FsError::Handle(CapabilityError::Stale)));
    }

    #[test]
    fn revoked_owner_loses_every_handle() {
        let bytes = image();
        let mut block = [0u8; BLOCK];
        let mut fs = Fs::<_, 4>::mount(Loopback::new(&bytes).unwrap(), &mut block).expect("mount");
        let mut buffers = BufferRegistry::<1>::new();

        let root = fs.root(OWNER).expect("root handle");
        fs.apply(OWNER, FsOp::Open { dir: root, name: name("docs") }, &mut buffers)
            .expect("open directory");

        assert_eq!(fs.revoke(OWNER), 2);
    }

    #[test]
    fn revoked_owner_cannot_write_through_stale_file_handle() {
        let mut bytes = image();
        let mut source = [0xa5; 8];
        let source_len = source.len();
        let mut block = [0u8; BLOCK];
        let mut fs =
            Fs::<_, 4>::mount(Loopback::writable(&mut bytes).unwrap(), &mut block).expect("mount");
        let mut buffers = BufferRegistry::<1>::new();
        let buffer = buffers.register_read(OWNER, &mut source).expect("free slot");
        let root = fs.root(OWNER).expect("root handle");
        let created = fs
            .apply(
                OWNER,
                FsOp::Create { dir: root, name: name("revoked"), kind: Kind::File },
                &mut buffers,
            )
            .expect("create");
        let Some(Handle::File(file)) = created.handle() else {
            panic!("new file opened as a directory: {created:?}");
        };

        assert_eq!(fs.revoke(OWNER), 2);
        let write =
            FsOp::Write { file, buffer: BufferOperation::new(buffer, 0, source_len), offset: 0 };
        assert_eq!(
            fs.apply(OWNER, write, &mut buffers),
            Err(FsError::Handle(CapabilityError::Stale))
        );
    }

    #[test]
    fn handles_run_out_rather_than_overwrite() {
        let bytes = image();
        let mut block = [0u8; BLOCK];
        let mut fs = Fs::<_, 1>::mount(Loopback::new(&bytes).unwrap(), &mut block).expect("mount");
        let mut buffers = BufferRegistry::<1>::new();

        let root = fs.root(OWNER).expect("root handle");
        let opened = fs.apply(OWNER, FsOp::Open { dir: root, name: name("docs") }, &mut buffers);

        assert_eq!(opened, Err(FsError::Handles));
    }

    #[test]
    fn seal_refuses_later_root() {
        let bytes = image();
        let mut block = [0u8; BLOCK];
        let mut fs = Fs::<_, 4>::mount(Loopback::new(&bytes).unwrap(), &mut block).expect("mount");

        let first = fs.root(OWNER);
        fs.seal();
        let second = fs.root(OWNER);

        assert!(first.is_ok(), "{first:?}");
        assert_eq!(second, Err(FsError::Sealed));
    }

    #[test]
    fn ring_answers_in_order_submitted() {
        let bytes = image();
        let mut block = [0u8; BLOCK];
        let mut fs = Fs::<_, 4>::mount(Loopback::new(&bytes).unwrap(), &mut block).expect("mount");
        let mut buffers = BufferRegistry::<1>::new();
        let mut ring = IoRing::<FsOp, Result<FsDone, FsError>, 4>::new();
        let (mut client, mut driver) = ring.split();

        let root = fs.root(OWNER).expect("root handle");
        for (id, index) in [(1, 0), (2, 1)] {
            let op = FsOp::Entry { dir: root, index };
            client.try_submit(Submission::new(RequestId::new(id), op)).expect("free slot");
        }
        let served = fs.serve(OWNER, &mut driver, &mut buffers);

        assert_eq!(served, 2);
        let first = client.try_completion().expect("completion");
        assert_eq!(first.id(), RequestId::new(1));
        assert_eq!(
            first.into_result(),
            Ok(FsDone::Entry {
                name: name("docs"),
                stat: Stat { kind: Kind::Dir, size: 0, entries: 1 },
            })
        );
        let second = client.try_completion().expect("completion");
        assert_eq!(
            second.into_result(),
            Ok(FsDone::Entry {
                name: name("hello.txt"),
                stat: Stat { kind: Kind::File, size: 11, entries: 0 },
            })
        );
    }

    #[test]
    fn full_completion_queue_preserves_next_result() {
        let bytes = image();
        let mut block = [0u8; BLOCK];
        let mut fs = Fs::<_, 4>::mount(Loopback::new(&bytes).unwrap(), &mut block).expect("mount");
        let mut buffers = BufferRegistry::<1>::new();
        let mut ring = IoRing::<FsOp, Result<FsDone, FsError>, 1>::new();
        let (mut client, mut driver) = ring.split();
        let root = fs.root(OWNER).expect("root handle");

        let first = FsOp::Entry { dir: root, index: 0 };
        client.try_submit(Submission::new(RequestId::new(1), first)).expect("free slot");
        assert_eq!(fs.serve(OWNER, &mut driver, &mut buffers), 1);

        let second = FsOp::Entry { dir: root, index: 1 };
        client.try_submit(Submission::new(RequestId::new(2), second)).expect("free slot");
        assert_eq!(fs.serve(OWNER, &mut driver, &mut buffers), 0);
        assert_eq!(client.try_completion().expect("first completion").id(), RequestId::new(1));

        assert_eq!(fs.serve(OWNER, &mut driver, &mut buffers), 1);
        assert_eq!(client.try_completion().expect("second completion").id(), RequestId::new(2));
    }
}
