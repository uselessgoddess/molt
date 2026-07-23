//! The filesystem as something other cells can talk to.
//!
//! [`Fs`] is the only holder of the [`Volume`]: everyone else names objects
//! through a capability table it owns and reaches it over a ring. The table
//! caches the object record behind each handle, which a read-only volume makes
//! free — nothing can change under a handle once it is open.

use molt_block::Device;
use molt_core::buffer::BufferRegistry;
use molt_core::capability::{Capability, CapabilityTable, CellId};
use molt_core::ring::{Completion, IoDriver};

use crate::FsError;
use crate::layout::{BLOCK, Kind, Object};
use crate::op::{Dir, File, FsDone, FsOp, Handle, Stat};
use crate::volume::Volume;

/// A mounted volume behind a capability table.
pub struct Fs<'buf, D, const N: usize> {
    volume: Volume<'buf, D>,
    open: CapabilityTable<Object, N>,
    pending: Option<Completion<Result<FsDone, FsError>>>,
}

impl<'buf, D: Device, const N: usize> Fs<'buf, D, N> {
    /// Mounts `device`, using `block` as the volume's only buffer.
    pub fn mount(device: D, block: &'buf mut [u8; BLOCK]) -> Result<Self, FsError> {
        Ok(Self {
            volume: Volume::mount(device, block)?,
            open: CapabilityTable::new(),
            pending: None,
        })
    }

    /// The checkpoint the mounted volume carries.
    pub fn generation(&self) -> u64 {
        self.volume.generation()
    }

    /// Hands `owner` a handle to the root directory.
    ///
    /// This is the one handle that comes from nowhere; every other one is
    /// opened through a directory somebody already holds.
    pub fn root(&mut self, owner: CellId) -> Result<Capability<Dir>, FsError> {
        let root = self.volume.root();
        let object = self.volume.object(root)?;
        if object.kind != Kind::Dir {
            return Err(FsError::Kind);
        }
        self.open.insert::<Dir>(owner, object).map_err(|_| FsError::Handles)
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
            FsOp::Root => self.root(owner).map(|dir| FsDone::Opened(Handle::Dir(dir))),
            FsOp::Open { dir, name } => {
                let parent = *self.open.get(dir)?;
                let id = self.volume.lookup(&parent, name.as_bytes())?;
                let object = self.volume.object(id)?;
                self.hold(owner, object).map(FsDone::Opened)
            }
            FsOp::Entry { dir, index } => {
                let parent = *self.open.get(dir)?;
                let (name, object) = self.volume.entry(&parent, index)?;
                Ok(FsDone::Entry { name, stat: stat(&self.volume.object(object)?) })
            }
            FsOp::Read { file, buffer, offset } => {
                let object = *self.open.get(file)?;
                let target = buffers.resolve_write(buffer)?;
                self.volume.read(&object, offset, target).map(FsDone::Read)
            }
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

    fn hold(&mut self, owner: CellId, object: Object) -> Result<Handle, FsError> {
        match object.kind {
            Kind::Dir => self.open.insert::<Dir>(owner, object).map(Handle::Dir),
            Kind::File => self.open.insert::<File>(owner, object).map(Handle::File),
        }
        .map_err(|_| FsError::Handles)
    }

    fn object(&self, handle: Handle) -> Result<Object, FsError> {
        Ok(match handle {
            Handle::Dir(dir) => *self.open.get(dir)?,
            Handle::File(file) => *self.open.get(file)?,
        })
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
