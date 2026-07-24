//! A writable volume, held in memory and checkpointed to a [`Disk`].
//!
//! The reader mounts an image somebody else wrote; a [`Store`] is the somebody.
//! It keeps the whole tree in a flat arena of nodes — a stable id per node, so
//! a handle survives every mutation around it — and [`Store::sync`] lays that
//! arena out as an image and writes it down. Nothing here runs in the kernel,
//! which has no allocator; it is the `format` half of the crate, exercised on a
//! host and over a [`Loopback`](molt_block::Loopback) or a fault-injecting disk.
//!
//! Crash consistency is a double-buffered checkpoint. The volume holds two
//! superblock copies and two data arenas of equal size. A checkpoint writes the
//! whole image into the arena that is *not* live, flushes, and only then writes
//! the superblock into the copy that pairs with it — so the live checkpoint is
//! never touched, and a power cut anywhere leaves the previous one intact for
//! [`Volume::mount`](crate::Volume) to find. There is no fsck: a mount steps
//! down a generation until one verifies, and one always does.

use alloc::vec::Vec;

use molt_block::{Disk, SECTOR};

use crate::FsError;
use crate::format::Image;
use crate::layout::{BLOCK, Kind, SUPERS};
use crate::name::Name;
use crate::op::Stat;

/// Sectors per block.
const SECTORS: u64 = (BLOCK / SECTOR) as u64;

/// A node in the tree: a directory's children, or a file's bytes.
#[derive(Clone)]
enum Node {
    /// Children as `(name, id)`, kept sorted by name for a binary search.
    Dir(Vec<(Name, u32)>),
    File(Vec<u8>),
}

/// A writable volume over a [`Disk`], checkpointed by [`Store::sync`].
#[derive(Clone)]
pub struct Store<D> {
    disk: D,
    /// Every node, indexed by its stable id; the root is id zero.
    nodes: Vec<Node>,
    root: u32,
    /// The generation of the last committed checkpoint.
    generation: u64,
    /// Blocks in each of the two data arenas.
    arena: u64,
}

impl<D: Disk> Store<D> {
    /// Formats `disk` as an empty volume and commits its first checkpoint.
    ///
    /// The device is split into two superblock copies and two arenas; a device
    /// too small to hold both arenas is [`FsError::Full`].
    pub fn format(disk: D) -> Result<Self, FsError> {
        let blocks = disk.sectors() / SECTORS;
        let arena = blocks.saturating_sub(SUPERS) / 2;
        if arena == 0 {
            return Err(FsError::Full);
        }

        let mut store =
            Self { disk, nodes: alloc::vec![Node::Dir(Vec::new())], root: 0, generation: 0, arena };
        store.checkpoint()?;
        Ok(store)
    }

    /// The stable id of the root directory.
    pub const fn root(&self) -> u32 {
        self.root
    }

    /// The generation the last committed checkpoint carries.
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// What a node is.
    pub fn kind(&self, id: u32) -> Result<Kind, FsError> {
        Ok(match self.node(id)? {
            Node::Dir(_) => Kind::Dir,
            Node::File(_) => Kind::File,
        })
    }

    /// A node's length or entry count, whichever it has.
    pub fn stat(&self, id: u32) -> Result<Stat, FsError> {
        Ok(match self.node(id)? {
            Node::Dir(children) => {
                Stat { kind: Kind::Dir, size: 0, entries: children.len() as u32 }
            }
            Node::File(data) => Stat { kind: Kind::File, size: data.len() as u64, entries: 0 },
        })
    }

    /// Finds `name` in directory `dir`, returning the id it names.
    pub fn lookup(&self, dir: u32, name: &[u8]) -> Result<u32, FsError> {
        let children = self.children(dir)?;
        match children.binary_search_by(|(held, _)| held.as_bytes().cmp(name)) {
            Ok(at) => Ok(children[at].1),
            Err(_) => Err(FsError::Missing),
        }
    }

    /// Reads `dir`'s entry at `index`, in name order.
    pub fn entry(&self, dir: u32, index: u32) -> Result<(Name, u32), FsError> {
        self.children(dir)?.get(index as usize).copied().ok_or(FsError::Missing)
    }

    /// Reads `file` from `offset` into `buf`, returning how many bytes landed.
    ///
    /// A read is short only at the end of the file.
    pub fn read(&self, file: u32, offset: u64, buf: &mut [u8]) -> Result<usize, FsError> {
        let data = self.file(file)?;
        if offset > data.len() as u64 {
            return Err(FsError::Range);
        }
        let offset = offset as usize;
        let want = (data.len() - offset).min(buf.len());
        buf[..want].copy_from_slice(&data[offset..offset + want]);
        Ok(want)
    }

    /// Creates a `kind` node named `name` in directory `dir`.
    ///
    /// The change lives in memory until [`Store::sync`] writes it down.
    pub fn create(&mut self, dir: u32, name: Name, kind: Kind) -> Result<u32, FsError> {
        let at = match self
            .children(dir)?
            .binary_search_by(|(held, _)| held.as_bytes().cmp(name.as_bytes()))
        {
            Ok(_) => return Err(FsError::Exists),
            Err(at) => at,
        };

        let id = index(self.nodes.len())?;
        self.nodes.push(match kind {
            Kind::Dir => Node::Dir(Vec::new()),
            Kind::File => Node::File(Vec::new()),
        });
        let Node::Dir(children) = &mut self.nodes[dir as usize] else { unreachable!() };
        children.insert(at, (name, id));
        Ok(id)
    }

    /// Writes `bytes` into `file` at `offset`, extending it if need be.
    ///
    /// A write past the end grows the file with zeros between, which the next
    /// checkpoint elides as a hole. The change lives in memory until a sync.
    pub fn write(&mut self, file: u32, offset: u64, bytes: &[u8]) -> Result<usize, FsError> {
        let offset = usize::try_from(offset).map_err(|_| FsError::Range)?;
        let end = offset.checked_add(bytes.len()).ok_or(FsError::Range)?;
        let Node::File(data) = self.nodes.get_mut(file as usize).ok_or(FsError::Missing)? else {
            return Err(FsError::Kind);
        };
        if data.len() < end {
            data.resize(end, 0);
        }
        data[offset..end].copy_from_slice(bytes);
        Ok(bytes.len())
    }

    /// Commits everything written since the last sync as a new checkpoint.
    pub fn sync(&mut self) -> Result<(), FsError> {
        self.checkpoint()
    }

    /// Borrows the disk below, as a crash test does to inspect its bytes.
    pub fn disk(&self) -> &D {
        &self.disk
    }

    /// Hands the disk back, as a crash test does to remount it.
    pub fn into_disk(self) -> D {
        self.disk
    }

    fn node(&self, id: u32) -> Result<&Node, FsError> {
        self.nodes.get(id as usize).ok_or(FsError::Missing)
    }

    fn children(&self, dir: u32) -> Result<&[(Name, u32)], FsError> {
        match self.node(dir)? {
            Node::Dir(children) => Ok(children),
            Node::File(_) => Err(FsError::Kind),
        }
    }

    fn file(&self, file: u32) -> Result<&[u8], FsError> {
        match self.node(file)? {
            Node::File(data) => Ok(data),
            Node::Dir(_) => Err(FsError::Kind),
        }
    }

    /// Lays the tree out into `image` depth first, returning the root's id.
    fn lay(&self, image: &mut Image, id: u32) -> Result<u32, FsError> {
        match self.node(id)? {
            Node::File(data) => image.file(data),
            Node::Dir(children) => {
                let mut entries = Vec::with_capacity(children.len());
                for (name, child) in children {
                    let object = self.lay(image, *child)?;
                    entries.push((*name, object));
                }
                image.push_dir(&entries)
            }
        }
    }

    /// Writes a full image into the idle arena, then the superblock that names
    /// it — the live checkpoint untouched until the last write lands.
    fn checkpoint(&mut self) -> Result<(), FsError> {
        let generation = self.generation + 1;
        // The idle copy and its arena share a parity, and it flips each time.
        let copy = self.generation % SUPERS;
        let base = SUPERS + copy * self.arena;

        let mut image = Image::default();
        let root = self.lay(&mut image, self.root)?;
        let (superblock, arena) = image.compose(root, generation, base)?;
        if superblock.blocks - base > self.arena {
            return Err(FsError::Full);
        }

        write(&mut self.disk, base, &arena)?;
        self.disk.flush()?;

        let mut block = [0u8; BLOCK];
        superblock.encode(&mut block);
        write(&mut self.disk, copy, &block)?;
        self.disk.flush()?;

        self.generation = generation;
        Ok(())
    }
}

fn write<D: Disk>(disk: &mut D, block: u64, bytes: &[u8]) -> Result<(), FsError> {
    disk.write(block * SECTORS, bytes).map_err(FsError::Device)
}

fn index(value: usize) -> Result<u32, FsError> {
    u32::try_from(value).map_err(|_| FsError::Range)
}

#[cfg(test)]
mod tests {
    use molt_block::{Fault, Loopback};

    use super::Store;
    use crate::layout::{BLOCK, Kind};
    use crate::name::Name;
    use crate::volume::Volume;
    use crate::{FsError, SUPERS};

    /// A device large enough for two arenas of a few files.
    const BLOCKS: usize = 64;

    fn name(text: &str) -> Name {
        Name::try_from(text).expect("legal name")
    }

    fn image() -> Loopback<alloc::vec::Vec<u8>> {
        Loopback::new(alloc::vec![0u8; BLOCKS * BLOCK]).expect("whole sectors")
    }

    /// Reads the file `name` back out of the root of a freshly mounted volume.
    fn read_root_file(bytes: &[u8], name: &[u8]) -> Result<alloc::vec::Vec<u8>, FsError> {
        let mut buffer = [0u8; BLOCK];
        let mut volume = Volume::mount(Loopback::new(bytes).expect("whole sectors"), &mut buffer)?;
        let root = volume.object(volume.root())?;
        let id = volume.lookup(&root, name)?;
        let file = volume.object(id)?;
        let mut data = alloc::vec![0u8; file.size as usize];
        let read = volume.read(&file, 0, &mut data)?;
        data.truncate(read);
        Ok(data)
    }

    #[test]
    fn written_file_reads_back() {
        let mut store = Store::format(image()).expect("formatted");
        let file = store.create(store.root(), name("greeting"), Kind::File).expect("created");
        store.write(file, 0, b"hello, molt").expect("written");

        let mut buf = [0u8; 32];
        let read = store.read(file, 0, &mut buf).expect("read");

        assert_eq!(&buf[..read], b"hello, molt");
    }

    #[test]
    fn create_over_name_refused() {
        let mut store = Store::format(image()).expect("formatted");
        store.create(store.root(), name("dup"), Kind::File).expect("created");

        assert_eq!(store.create(store.root(), name("dup"), Kind::Dir), Err(FsError::Exists));
    }

    #[test]
    fn entries_stay_sorted_through_creates() {
        let mut store = Store::format(image()).expect("formatted");
        for text in ["gamma", "alpha", "beta"] {
            store.create(store.root(), name(text), Kind::File).expect("created");
        }

        let names: alloc::vec::Vec<_> = (0..3)
            .map(|at| store.entry(store.root(), at).expect("entry").0)
            .map(|name| name.as_str().expect("utf8").into())
            .collect::<alloc::vec::Vec<alloc::string::String>>();

        assert_eq!(names, ["alpha", "beta", "gamma"]);
    }

    #[test]
    fn write_past_end_zero_fills() {
        let mut store = Store::format(image()).expect("formatted");
        let file = store.create(store.root(), name("sparse"), Kind::File).expect("created");
        store.write(file, 4, b"z").expect("written");

        let mut buf = [0xffu8; 5];
        let read = store.read(file, 0, &mut buf).expect("read");

        assert_eq!(&buf[..read], b"\0\0\0\0z");
    }

    #[test]
    fn sync_survives_remount() {
        let mut store = Store::format(image()).expect("formatted");
        let file = store.create(store.root(), name("keep"), Kind::File).expect("created");
        store.write(file, 0, b"durable").expect("written");
        store.sync().expect("synced");

        let bytes = store.into_disk().into_inner();

        assert_eq!(read_root_file(&bytes, b"keep").as_deref(), Ok(b"durable".as_slice()));
    }

    #[test]
    fn generation_rises_only_on_sync() {
        let mut store = Store::format(image()).expect("formatted");
        assert_eq!(store.generation(), 1);

        store.create(store.root(), name("later"), Kind::File).expect("created");
        assert_eq!(store.generation(), 1, "an uncommitted change moved the generation");

        store.sync().expect("synced");
        assert_eq!(store.generation(), 2);
    }

    /// A power cut at every checkpoint op leaves either the old volume or the
    /// new one — never a torn in-between — since the live arena is never the one
    /// a checkpoint writes into.
    #[test]
    fn power_cut_at_every_checkpoint_op_stays_consistent() {
        // Commit generation one, then stage generation two in memory.
        let mut staged = Store::format(image()).expect("formatted");
        let file = staged.create(staged.root(), name("f"), Kind::File).expect("created");
        staged.write(file, 0, b"one").expect("written");
        staged.sync().expect("synced");
        staged.write(file, 0, b"two").expect("restaged");

        // A clean run counts the crash points the staged sync has.
        let mut clean = staged.clone().replace_disk(Fault::healthy);
        clean.sync().expect("clean sync");
        let attempts = clean.disk().attempts();
        assert!(attempts > 0);

        for budget in 0..=attempts {
            let mut cut = staged.clone().replace_disk(|disk| Fault::after(disk, budget));
            let _ = cut.sync();
            let bytes = cut.into_disk().into_inner().into_inner();

            let content = read_root_file(&bytes, b"f").expect("mounts at some generation");
            assert!(
                content == b"one" || content == b"two",
                "budget {budget} mounted a torn checkpoint: {content:?}",
            );
        }
    }

    /// A torn newer superblock falls back to the older copy, so the volume that
    /// mounts is the previous checkpoint rather than none at all.
    #[test]
    fn torn_newer_super_falls_back() {
        let mut store = Store::format(image()).expect("formatted");
        let file = store.create(store.root(), name("f"), Kind::File).expect("created");
        store.write(file, 0, b"one").expect("written");
        store.sync().expect("first sync");
        store.write(file, 0, b"two").expect("restaged");
        store.sync().expect("second sync");

        // Format took copy zero, so the two syncs land in copies one then zero:
        // the newest generation is in copy zero. Tear it and copy one serves.
        let mut bytes = store.into_disk().into_inner();
        bytes[0] ^= 0xff;

        assert_eq!(read_root_file(&bytes, b"f").as_deref(), Ok(b"one".as_slice()));
    }

    impl<D> Store<D> {
        /// Rewraps the disk, carrying the staged tree onto a new backing.
        fn replace_disk<E: molt_block::Disk>(self, wrap: impl FnOnce(D) -> E) -> Store<E> {
            Store {
                disk: wrap(self.disk),
                nodes: self.nodes,
                root: self.root,
                generation: self.generation,
                arena: self.arena,
            }
        }
    }

    // SUPERS is the pair of copies the checkpoint alternates between.
    const _: () = assert!(SUPERS == 2);
}
