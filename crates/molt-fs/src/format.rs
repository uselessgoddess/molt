//! Building an image, the other half of the format.
//!
//! It lives beside the reader rather than in `xtask` so both halves share one
//! definition of the layout and a test can round-trip through them. Nothing in
//! the kernel needs it, so it hides behind the `format` feature and is the only
//! part of the crate that allocates.

use alloc::vec;
use alloc::vec::Vec;

use crate::FsError;
use crate::crc::crc32c;
use crate::layout::{
    Area, BLOCK, DEFAULT_LOG_BLOCKS, DEFAULT_TREE_BLOCKS, ENTRY_BYTES, EXTENT_BYTES, Entry, Extent,
    Kind, OBJECT_BYTES, Object, Region, SUPERS, Super,
};
use crate::name::Name;

/// A directory being assembled for an image.
#[derive(Debug, Default)]
pub struct Tree {
    nodes: Vec<(Name, Node)>,
}

#[derive(Debug)]
enum Node {
    Dir(Tree),
    File(Vec<u8>),
}

impl Tree {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a file, replacing any file of the same name.
    pub fn file(&mut self, name: &str, bytes: Vec<u8>) -> Result<(), FsError> {
        let name = Name::try_from(name)?;
        match self.find(&name) {
            Some(at) => self.nodes[at].1 = Node::File(bytes),
            None => self.nodes.push((name, Node::File(bytes))),
        }
        Ok(())
    }

    /// Adds a directory, or borrows the one already under that name.
    pub fn dir(&mut self, name: &str) -> Result<&mut Self, FsError> {
        let name = Name::try_from(name)?;
        let at = match self.find(&name) {
            Some(at) => at,
            None => {
                self.nodes.push((name, Node::Dir(Self::new())));
                self.nodes.len() - 1
            }
        };
        match &mut self.nodes[at].1 {
            Node::Dir(tree) => Ok(tree),
            Node::File(_) => Err(FsError::Kind),
        }
    }

    fn find(&self, name: &Name) -> Option<usize> {
        self.nodes.iter().position(|(held, _)| held == name)
    }
}

/// Lays `tree` out as a mountable image stamped with `generation`.
pub fn build(tree: &Tree, generation: u64) -> Result<Vec<u8>, FsError> {
    build_with_log(tree, generation, DEFAULT_LOG_BLOCKS)
}

/// Lays `tree` out with `log_blocks` in each of three rotating log banks.
pub fn build_with_log(tree: &Tree, generation: u64, log_blocks: u32) -> Result<Vec<u8>, FsError> {
    build_with_capacity(tree, generation, log_blocks, DEFAULT_TREE_BLOCKS)
}

/// Lays `tree` out with explicit log and COW metadata capacity.
pub fn build_with_capacity(
    tree: &Tree,
    generation: u64,
    log_blocks: u32,
    tree_blocks: u32,
) -> Result<Vec<u8>, FsError> {
    if log_blocks == 0 || tree_blocks == 0 || tree_blocks > crate::layout::MAX_TREE_BLOCKS {
        return Err(FsError::Range);
    }
    let mut image = Image::default();
    let root = image.dir(tree)?;
    image.finish(root, generation, log_blocks, tree_blocks)
}

#[derive(Default)]
struct Image {
    objects: Vec<Object>,
    extents: Vec<Extent>,
    entries: Vec<Entry>,
    names: Vec<u8>,
    /// Data blocks in the order they were laid down, addressed from zero until
    /// [`Image::finish`] learns where the data region starts.
    data: Vec<u8>,
}

impl Image {
    /// Lays out a directory and everything under it, returning its object id.
    ///
    /// Entries are written sorted so a reader can binary search them, and the
    /// range is reserved before the children are laid out so a directory's
    /// entries stay contiguous however deep its subtrees go.
    fn dir(&mut self, tree: &Tree) -> Result<u32, FsError> {
        let id = self.reserve()?;
        let mut nodes: Vec<&(Name, Node)> = tree.nodes.iter().collect();
        nodes.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));

        let start = index(self.entries.len())?;
        self.entries.resize(self.entries.len() + nodes.len(), Entry::default());
        for (at, (name, node)) in nodes.iter().enumerate() {
            let object = match node {
                Node::Dir(tree) => self.dir(tree)?,
                Node::File(bytes) => self.file(bytes)?,
            };
            let name_at = index(self.names.len())?;
            self.names.extend_from_slice(name.as_bytes());
            self.entries[start as usize + at] =
                Entry { name_at, name_len: name.len() as u16, object };
        }

        self.objects[id as usize] =
            Object { kind: Kind::Dir, start, count: index(nodes.len())?, size: 0 };
        Ok(id)
    }

    /// Lays out a file's blocks, returning its object id.
    ///
    /// An all-zero block is left out entirely: it becomes a hole the reader
    /// fills in, which is what keeps a sparse file from costing its length.
    fn file(&mut self, bytes: &[u8]) -> Result<u32, FsError> {
        let start = index(self.extents.len())?;
        let mut count = 0;
        for (logical, chunk) in bytes.chunks(BLOCK).enumerate() {
            if chunk.iter().all(|&byte| byte == 0) {
                continue;
            }
            let logical = index(logical)?;
            let block = (self.data.len() / BLOCK) as u64;
            self.data.extend_from_slice(chunk);
            self.data.resize(self.data.len().next_multiple_of(BLOCK), 0);
            match self.extents.last_mut() {
                // Only extend a run this file started.
                Some(last) if count > 0 && last.logical + last.blocks == logical => {
                    last.blocks += 1
                }
                _ => {
                    self.extents.push(Extent { logical, blocks: 1, block });
                    count += 1;
                }
            }
        }

        let id = self.reserve()?;
        self.objects[id as usize] =
            Object { kind: Kind::File, start, count, size: bytes.len() as u64 };
        Ok(id)
    }

    /// Takes the next object id, to be filled in once its contents are laid out.
    fn reserve(&mut self) -> Result<u32, FsError> {
        let id = index(self.objects.len())?;
        self.objects.push(Object { kind: Kind::Dir, start: 0, count: 0, size: 0 });
        Ok(id)
    }

    /// Places the regions, checksums them, and writes both superblock copies.
    fn finish(
        mut self,
        root: u32,
        generation: u64,
        log_blocks: u32,
        tree_blocks: u32,
    ) -> Result<Vec<u8>, FsError> {
        let data_blocks = (self.data.len() / BLOCK) as u64;
        let sizes = [
            self.objects.len() * OBJECT_BYTES,
            self.extents.len() * EXTENT_BYTES,
            self.entries.len() * ENTRY_BYTES,
            self.names.len(),
            data_blocks as usize * 4,
        ];

        let mut superblock =
            Super { generation, root, data_blocks, log_blocks, tree_blocks, ..Super::default() };
        let mut at = SUPERS;
        for (area, bytes) in Area::BASE.into_iter().zip(sizes) {
            let region = Region { at, bytes: bytes as u64, crc: 0 };
            superblock.set_region(area, region);
            at += region.blocks();
        }
        superblock.data_at = at;
        superblock.tree_at = at.checked_add(data_blocks).ok_or(FsError::Range)?;
        let log_at =
            superblock.tree_at.checked_add(u64::from(tree_blocks)).ok_or(FsError::Range)?;
        let log_span =
            u64::from(log_blocks).checked_mul(crate::layout::LOG_BANKS).ok_or(FsError::Range)?;
        superblock.blocks = log_at.checked_add(log_span).ok_or(FsError::Range)?;
        superblock.set_region(Area::Log, Region { at: log_at, bytes: 0, crc: crc32c(&[]) });

        // Extents were addressed from the start of the data region.
        for extent in &mut self.extents {
            extent.block += superblock.data_at;
        }

        let mut sums = Vec::with_capacity(sizes[4]);
        for block in self.data.chunks(BLOCK) {
            sums.extend_from_slice(&crc32c(block).to_le_bytes());
        }

        let regions = [
            records(&self.objects, OBJECT_BYTES, Object::encode),
            records(&self.extents, EXTENT_BYTES, Extent::encode),
            records(&self.entries, ENTRY_BYTES, Entry::encode),
            self.names,
            sums,
        ];

        let mut image = vec![0; superblock.blocks as usize * BLOCK];
        for (area, bytes) in Area::BASE.into_iter().zip(regions) {
            let mut region = superblock.region(area);
            region.crc = crc32c(&bytes);
            superblock.set_region(area, region);
            let at = region.at as usize * BLOCK;
            image[at..at + bytes.len()].copy_from_slice(&bytes);
        }
        image[superblock.data_at as usize * BLOCK..][..self.data.len()].copy_from_slice(&self.data);

        for copy in 0..SUPERS {
            superblock.encode(&mut image[copy as usize * BLOCK..]);
        }
        Ok(image)
    }
}

fn records<T>(values: &[T], size: usize, encode: fn(&T, &mut [u8])) -> Vec<u8> {
    let mut bytes = vec![0; values.len() * size];
    for (at, value) in values.iter().enumerate() {
        encode(value, &mut bytes[at * size..]);
    }
    bytes
}

fn index(value: usize) -> Result<u32, FsError> {
    u32::try_from(value).map_err(|_| FsError::Range)
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::{Tree, build, build_with_capacity};
    use crate::FsError;
    use crate::layout::{BLOCK, MAX_TREE_BLOCKS, Super};

    #[test]
    fn empty_tree_still_mounts_as_volume() {
        let image = build(&Tree::new(), 1).expect("image that fits");
        let superblock = Super::parse(&image).expect("superblock");

        assert_eq!(superblock.generation, 1);
        assert_eq!(superblock.data_blocks, 0);
        assert_eq!(superblock.tree_blocks, MAX_TREE_BLOCKS);
    }

    #[test]
    fn both_superblock_copies_written() {
        let image = build(&Tree::new(), 3).expect("image that fits");

        assert_eq!(Super::parse(&image), Super::parse(&image[BLOCK..]));
    }

    #[test]
    fn image_covers_whole_blocks() {
        let mut tree = Tree::new();
        tree.file("a", vec![1; BLOCK + 1]).expect("legal name");

        let image = build(&tree, 1).expect("image that fits");

        assert_eq!(image.len() % BLOCK, 0);
    }

    #[test]
    fn hole_costs_no_data_block() {
        let mut tree = Tree::new();
        tree.file("sparse", vec![0; 4 * BLOCK]).expect("legal name");

        let image = build(&tree, 1).expect("image that fits");

        assert_eq!(Super::parse(&image).expect("superblock").data_blocks, 0);
    }

    #[test]
    fn invalid_tree_capacity_refused() {
        assert_eq!(build_with_capacity(&Tree::new(), 1, 1, 0), Err(FsError::Range));
        assert_eq!(
            build_with_capacity(&Tree::new(), 1, 1, MAX_TREE_BLOCKS + 1),
            Err(FsError::Range)
        );
    }

    #[test]
    fn directory_reopens_instead_of_duplicating() {
        let mut tree = Tree::new();
        tree.dir("docs").expect("legal name").file("one", vec![]).expect("legal name");
        tree.dir("docs").expect("legal name").file("two", vec![]).expect("legal name");

        assert_eq!(tree.nodes.len(), 1);
    }

    #[test]
    fn directory_over_file_refused() {
        let mut tree = Tree::new();
        tree.file("name", vec![]).expect("legal name");

        assert_eq!(tree.dir("name").err(), Some(FsError::Kind));
    }
}
