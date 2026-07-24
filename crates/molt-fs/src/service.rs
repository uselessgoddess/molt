//! The filesystem as something other cells can talk to.
//!
//! [`Fs`] is the only holder of the volume: everyone else names objects through
//! a capability table it owns and reaches it over a ring. The table caches the
//! stable id behind each handle and re-resolves the record per operation, so a
//! handle survives every write around it — the same id keeps naming the same
//! object whether or not the store beneath grew.
//!
//! The volume itself is any [`Backend`]: the read-only [`Volume`] or the
//! writable [`Store`](crate::Store). The read path is shared; a write a
//! read-only backend refuses with [`FsError::ReadOnly`], so one `Fs` serves
//! both.

use molt_block::Device;
use molt_core::buffer::BufferRegistry;
use molt_core::capability::{Capability, CapabilityTable, CellId};
use molt_core::ring::{Completion, IoDriver};

use crate::FsError;
use crate::layout::{BLOCK, Kind, Object};
use crate::name::Name;
use crate::op::{Dir, File, FsDone, FsOp, Handle, Stat};
use crate::volume::Volume;

/// A volume [`Fs`] can serve, read-only or writable.
///
/// Objects are named by a stable id: a directory or a file the backend hands
/// back an id for keeps it across every later change, so the capability table
/// can cache the id and re-resolve the record each operation. The read half is
/// shared; the write half defaults to [`FsError::ReadOnly`], which is the whole
/// of a read-only backend's write path.
pub trait Backend {
    /// The id of the root directory.
    fn root(&self) -> u32;
    /// The generation the mounted checkpoint carries.
    fn generation(&self) -> u64;
    /// What object `id` is.
    fn kind(&mut self, id: u32) -> Result<Kind, FsError>;
    /// Object `id`'s length or entry count, whichever it has.
    fn stat(&mut self, id: u32) -> Result<Stat, FsError>;
    /// Finds `name` in directory `dir`, returning the id it names.
    fn lookup(&mut self, dir: u32, name: &[u8]) -> Result<u32, FsError>;
    /// Reads `dir`'s entry at `index`, in name order.
    fn entry(&mut self, dir: u32, index: u32) -> Result<(Name, u32), FsError>;
    /// Reads `file` from `offset` into `buf`, returning how many bytes landed.
    fn read(&mut self, file: u32, offset: u64, buf: &mut [u8]) -> Result<usize, FsError>;

    /// Creates a `kind` object named `name` in `dir`, returning its id.
    fn create(&mut self, _dir: u32, _name: Name, _kind: Kind) -> Result<u32, FsError> {
        Err(FsError::ReadOnly)
    }

    /// Writes `bytes` into `file` at `offset`, returning how many it took.
    fn write(&mut self, _file: u32, _offset: u64, _bytes: &[u8]) -> Result<usize, FsError> {
        Err(FsError::ReadOnly)
    }

    /// Commits everything written since the last sync as a durable checkpoint.
    fn sync(&mut self) -> Result<(), FsError> {
        Err(FsError::ReadOnly)
    }
}

/// A mounted volume behind a capability table.
pub struct Fs<B, const N: usize> {
    backend: B,
    open: CapabilityTable<u32, N>,
    pending: Option<Completion<Result<FsDone, FsError>>>,
    sealed: bool,
}

impl<'buf, D: Device, const N: usize> Fs<Volume<'buf, D>, N> {
    /// Mounts `device` read-only, using `block` as the volume's only buffer.
    pub fn mount(device: D, block: &'buf mut [u8; BLOCK]) -> Result<Self, FsError> {
        Ok(Self::new(Volume::mount(device, block)?))
    }
}

impl<B: Backend, const N: usize> Fs<B, N> {
    /// Wraps `backend` in the capability table and ring protocol.
    pub fn new(backend: B) -> Self {
        Self { backend, open: CapabilityTable::new(), pending: None, sealed: false }
    }

    /// The checkpoint the mounted volume carries.
    pub fn generation(&self) -> u64 {
        self.backend.generation()
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
        let root = self.backend.root();
        if self.backend.kind(root)? != Kind::Dir {
            return Err(FsError::Kind);
        }
        self.open.insert::<Dir>(owner, root).map_err(|_| FsError::Handles)
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
                let id = self.backend.lookup(parent, name.as_bytes())?;
                self.hold(owner, id).map(FsDone::Opened)
            }
            FsOp::Entry { dir, index } => {
                let parent = *self.open.get(dir)?;
                let (name, id) = self.backend.entry(parent, index)?;
                Ok(FsDone::Entry { name, stat: self.backend.stat(id)? })
            }
            FsOp::Read { file, buffer, offset } => {
                let id = *self.open.get(file)?;
                let target = buffers.resolve_write(buffer)?;
                self.backend.read(id, offset, target).map(FsDone::Read)
            }
            FsOp::Create { dir, name, kind } => {
                let parent = *self.open.get(dir)?;
                let id = self.backend.create(parent, name, kind)?;
                self.hold(owner, id).map(FsDone::Created)
            }
            FsOp::Write { file, buffer, offset } => {
                let id = *self.open.get(file)?;
                let source = buffers.resolve_read(buffer)?;
                self.backend.write(id, offset, source).map(FsDone::Wrote)
            }
            FsOp::Sync => self.backend.sync().map(|()| FsDone::Synced),
            FsOp::Stat(handle) => Ok(FsDone::Stat(self.backend.stat(self.id(handle)?)?)),
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

    fn hold(&mut self, owner: CellId, id: u32) -> Result<Handle, FsError> {
        match self.backend.kind(id)? {
            Kind::Dir => self.open.insert::<Dir>(owner, id).map(Handle::Dir),
            Kind::File => self.open.insert::<File>(owner, id).map(Handle::File),
        }
        .map_err(|_| FsError::Handles)
    }

    fn id(&self, handle: Handle) -> Result<u32, FsError> {
        Ok(match handle {
            Handle::Dir(dir) => *self.open.get(dir)?,
            Handle::File(file) => *self.open.get(file)?,
        })
    }
}

/// A directory counts entries and has no length; a file is the reverse.
pub(crate) fn stat(object: &Object) -> Stat {
    match object.kind {
        Kind::Dir => Stat { kind: Kind::Dir, size: 0, entries: object.count },
        Kind::File => Stat { kind: Kind::File, size: object.size, entries: 0 },
    }
}

#[cfg(all(test, feature = "format"))]
mod tests {
    use molt_block::Loopback;
    use molt_core::buffer::{BufferOperation, BufferRegistry};
    use molt_core::capability::{Capability, CapabilityError, CellId};
    use molt_core::ring::{IoRing, RequestId, Submission};

    use super::Fs;
    use crate::format::{Tree, build};
    use crate::layout::{BLOCK, Kind};
    use crate::op::{FsDone, FsOp, Handle, Stat};
    use crate::{FsError, Name, Store};

    const OWNER: CellId = CellId::new(4);

    /// A device large enough for two arenas of a few small files.
    const BLOCKS: usize = 64;

    fn image() -> alloc::vec::Vec<u8> {
        let mut tree = Tree::new();
        tree.file("hello.txt", b"hello, molt".to_vec()).expect("legal name");
        tree.dir("docs").expect("legal name").file("readme", b"read me".to_vec()).unwrap();
        build(&tree, 1).expect("image that fits")
    }

    fn name(text: &str) -> Name {
        Name::try_from(text).expect("legal name")
    }

    /// A freshly formatted writable store behind a capability table.
    fn store<B: AsRef<[u8]> + AsMut<[u8]>>(bytes: B) -> Fs<Store<Loopback<B>>, 4> {
        Fs::new(Store::format(Loopback::new(bytes).expect("whole sectors")).expect("formatted"))
    }

    /// The file capability an open or a create handed back.
    fn file(done: FsDone) -> Capability<crate::op::File> {
        match done.handle() {
            Some(Handle::File(file)) => file,
            other => panic!("expected a file handle: {other:?}"),
        }
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

    #[test]
    fn create_then_write_reads_back() {
        let mut fs = store(alloc::vec![0u8; BLOCKS * BLOCK]);
        let mut source = *b"hello, molt";
        let mut sink = [0u8; 16];
        let mut buffers = BufferRegistry::<2>::new();
        let input = buffers.register_read(OWNER, &mut source).expect("free slot");
        let output = buffers.register_write(OWNER, &mut sink).expect("free slot");

        let root = fs.root(OWNER).expect("root handle");
        let created = fs
            .apply(
                OWNER,
                FsOp::Create { dir: root, name: name("greeting"), kind: Kind::File },
                &mut buffers,
            )
            .expect("create file");
        let file = file(created);
        let wrote = fs.apply(
            OWNER,
            FsOp::Write { file, buffer: BufferOperation::new(input, 0, 11), offset: 0 },
            &mut buffers,
        );
        let read = fs.apply(
            OWNER,
            FsOp::Read { file, buffer: BufferOperation::new(output, 0, 11), offset: 0 },
            &mut buffers,
        );

        assert_eq!(wrote, Ok(FsDone::Wrote(11)));
        assert_eq!(read, Ok(FsDone::Read(11)));
        assert_eq!(
            buffers.resolve_write(BufferOperation::new(output, 0, 11)).unwrap(),
            b"hello, molt"
        );
    }

    #[test]
    fn create_over_name_refused() {
        let mut fs = store(alloc::vec![0u8; BLOCKS * BLOCK]);
        let mut buffers = BufferRegistry::<1>::new();
        let root = fs.root(OWNER).expect("root handle");
        let make = |kind| FsOp::Create { dir: root, name: name("dup"), kind };
        fs.apply(OWNER, make(Kind::File), &mut buffers).expect("first create");

        assert_eq!(fs.apply(OWNER, make(Kind::Dir), &mut buffers), Err(FsError::Exists));
    }

    #[test]
    fn sync_makes_created_file_durable() {
        let mut bytes = alloc::vec![0u8; BLOCKS * BLOCK];
        {
            let mut fs = store(bytes.as_mut_slice());
            let mut source = *b"durable";
            let mut buffers = BufferRegistry::<1>::new();
            let input = buffers.register_read(OWNER, &mut source).expect("free slot");

            let root = fs.root(OWNER).expect("root handle");
            let created = fs
                .apply(
                    OWNER,
                    FsOp::Create { dir: root, name: name("keep"), kind: Kind::File },
                    &mut buffers,
                )
                .expect("create file");
            let file = file(created);
            fs.apply(
                OWNER,
                FsOp::Write { file, buffer: BufferOperation::new(input, 0, 7), offset: 0 },
                &mut buffers,
            )
            .expect("write file");

            assert_eq!(fs.generation(), 1, "an uncommitted write moved the generation");
            assert_eq!(fs.apply(OWNER, FsOp::Sync, &mut buffers), Ok(FsDone::Synced));
            assert_eq!(fs.generation(), 2);
        }

        // Remount the durable bytes read-only and read the file back over ops.
        let mut block = [0u8; BLOCK];
        let mut fs =
            Fs::<_, 4>::mount(Loopback::new(bytes.as_slice()).unwrap(), &mut block).expect("mount");
        let mut sink = [0u8; 16];
        let mut buffers = BufferRegistry::<1>::new();
        let output = buffers.register_write(OWNER, &mut sink).expect("free slot");

        let root = fs.root(OWNER).expect("root handle");
        let opened = fs
            .apply(OWNER, FsOp::Open { dir: root, name: name("keep") }, &mut buffers)
            .expect("open file");
        let file = file(opened);
        let read = fs.apply(
            OWNER,
            FsOp::Read { file, buffer: BufferOperation::new(output, 0, 7), offset: 0 },
            &mut buffers,
        );

        assert_eq!(read, Ok(FsDone::Read(7)));
        assert_eq!(buffers.resolve_write(BufferOperation::new(output, 0, 7)).unwrap(), b"durable");
    }

    #[test]
    fn read_only_backend_refuses_writes() {
        let bytes = image();
        let mut block = [0u8; BLOCK];
        let mut fs = Fs::<_, 4>::mount(Loopback::new(&bytes).unwrap(), &mut block).expect("mount");
        let mut source = *b"x";
        let mut buffers = BufferRegistry::<1>::new();
        let input = buffers.register_read(OWNER, &mut source).expect("free slot");

        let root = fs.root(OWNER).expect("root handle");
        let opened = fs
            .apply(OWNER, FsOp::Open { dir: root, name: name("hello.txt") }, &mut buffers)
            .expect("open file");
        let file = file(opened);

        let create = FsOp::Create { dir: root, name: name("new"), kind: Kind::File };
        let write = FsOp::Write { file, buffer: BufferOperation::new(input, 0, 1), offset: 0 };
        assert_eq!(fs.apply(OWNER, create, &mut buffers), Err(FsError::ReadOnly));
        assert_eq!(fs.apply(OWNER, write, &mut buffers), Err(FsError::ReadOnly));
        assert_eq!(fs.apply(OWNER, FsOp::Sync, &mut buffers), Err(FsError::ReadOnly));
    }

    #[test]
    fn revoked_owner_loses_created_handle() {
        let mut fs = store(alloc::vec![0u8; BLOCKS * BLOCK]);
        let mut buffers = BufferRegistry::<1>::new();
        let root = fs.root(OWNER).expect("root handle");
        fs.apply(
            OWNER,
            FsOp::Create { dir: root, name: name("f"), kind: Kind::File },
            &mut buffers,
        )
        .expect("create file");

        assert_eq!(fs.revoke(OWNER), 2, "root and the created file both outlive the owner");
    }
}
