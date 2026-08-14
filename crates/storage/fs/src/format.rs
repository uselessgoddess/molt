//! Building an image, the other half of the format.
//!
//! It lives beside the reader rather than in `xtask` so both halves share one
//! definition of the layout and a test can round-trip through them. Nothing in
//! the kernel needs it, so it hides behind the `format` feature.

use alloc::vec;
use alloc::vec::Vec;

use crate::FsError;
use crate::btree::{self, Key, Value};
use crate::crc::crc32c;
use crate::layout::{
    Area, BLOCK, DEFAULT_LOG_BLOCKS, DEFAULT_TREE_BLOCKS, Kind, Object, Region, SUPERS, Super,
};
use crate::log::{HEADER, Record, headers_crc};
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
    entries: Vec<(Key, Value)>,
    log: Vec<u8>,
    next_object: u32,
}

impl Image {
    /// Lays out a directory and everything under it, returning its object id.
    fn dir(&mut self, tree: &Tree) -> Result<u32, FsError> {
        let id = self.reserve()?;
        let mut nodes: Vec<&(Name, Node)> = tree.nodes.iter().collect();
        nodes.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));

        for (name, node) in &nodes {
            let object = match node {
                Node::Dir(tree) => self.dir(tree)?,
                Node::File(bytes) => self.file(bytes)?,
            };
            self.entries.push((Key::dirent(id, name), Value::dirent(object)));
        }

        let object = Object { kind: Kind::Dir, count: index(nodes.len())?, size: 0 };
        self.entries.push((Key::object(id), Value::object(object)));
        Ok(id)
    }

    fn file(&mut self, bytes: &[u8]) -> Result<u32, FsError> {
        let id = self.reserve()?;
        let mut start = 0;
        while start < bytes.len() {
            let block_end = (start + BLOCK).min(bytes.len());
            let chunk = &bytes[start..block_end];
            if chunk.iter().all(|&byte| byte == 0) {
                start = block_end;
                continue;
            }
            let run = start;
            start = block_end;
            while start < bytes.len() {
                let next = (start + BLOCK).min(bytes.len());
                if bytes[start..next].iter().all(|&byte| byte == 0) {
                    break;
                }
                start = next;
            }
            let payload = &bytes[run..start];
            let offset = u64::try_from(run).map_err(|_| FsError::Range)?;
            let record = Record::write(id, offset, payload.len())?;
            let cursor = self.append(record, payload)?;
            let end = offset.checked_add(payload.len() as u64).ok_or(FsError::Range)?;
            self.entries
                .push((Key::extent(id, end), Value::extent(cursor, 0, payload.len() as u32)));
        }

        let object = Object { kind: Kind::File, count: 0, size: bytes.len() as u64 };
        self.entries.push((Key::object(id), Value::object(object)));
        Ok(id)
    }

    fn reserve(&mut self) -> Result<u32, FsError> {
        let id = self.next_object;
        self.next_object = id.checked_add(1).ok_or(FsError::Range)?;
        Ok(id)
    }

    fn append(&mut self, record: Record, payload: &[u8]) -> Result<u64, FsError> {
        let cursor = u64::try_from(self.log.len()).map_err(|_| FsError::Range)?;
        let span = usize::try_from(record.span()?).map_err(|_| FsError::Range)?;
        let end = self.log.len().checked_add(span).ok_or(FsError::Range)?;
        self.log.resize(end, 0);
        let header: &mut [u8; HEADER] =
            (&mut self.log[cursor as usize..][..HEADER]).try_into().map_err(|_| FsError::Range)?;
        record.encode(header);
        for (chunk, bytes) in payload.chunks(BLOCK).enumerate() {
            let at = cursor as usize + HEADER + chunk * 4;
            self.log[at..at + 4].copy_from_slice(&crc32c(bytes).to_le_bytes());
        }
        let payload_at = usize::try_from(record.payload_at()?).map_err(|_| FsError::Range)?;
        self.log[cursor as usize + payload_at..][..payload.len()].copy_from_slice(payload);
        Ok(cursor)
    }

    /// Places the regions, checksums them, and writes both superblock copies.
    fn finish(
        mut self,
        root: u32,
        generation: u64,
        log_blocks: u32,
        tree_blocks: u32,
    ) -> Result<Vec<u8>, FsError> {
        let log_capacity = usize::try_from(log_blocks)
            .ok()
            .and_then(|blocks| blocks.checked_mul(BLOCK))
            .ok_or(FsError::Range)?;
        if self.log.len() > log_capacity {
            return Err(FsError::Full);
        }
        let mut superblock = Super {
            generation,
            root,
            log_blocks,
            tree_at: SUPERS,
            tree_blocks,
            ..Super::default()
        };
        let log_at =
            superblock.tree_at.checked_add(u64::from(tree_blocks)).ok_or(FsError::Range)?;
        let log_span =
            u64::from(log_blocks).checked_mul(crate::layout::LOG_BANKS).ok_or(FsError::Range)?;
        superblock.blocks = log_at.checked_add(log_span).ok_or(FsError::Range)?;
        superblock.set_region(
            Area::Log,
            Region { at: log_at, bytes: self.log.len() as u64, crc: headers_crc(&self.log)? },
        );

        let mut image = vec![0; superblock.blocks as usize * BLOCK];
        let tree_start = superblock.tree_at as usize * BLOCK;
        let tree_end = tree_start + tree_blocks as usize * BLOCK;
        superblock.tree_root =
            btree::format(&mut self.entries, &mut image[tree_start..tree_end], generation, SUPERS)?;
        image[log_at as usize * BLOCK..][..self.log.len()].copy_from_slice(&self.log);

        for copy in 0..SUPERS {
            superblock.encode(&mut image[copy as usize * BLOCK..]);
        }
        Ok(image)
    }
}

fn index(value: usize) -> Result<u32, FsError> {
    u32::try_from(value).map_err(|_| FsError::Range)
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::{Tree, build, build_with_capacity};
    use crate::FsError;
    use crate::layout::{Area, BLOCK, DEFAULT_TREE_BLOCKS, MAX_TREE_BLOCKS, Super};

    #[test]
    fn empty_tree_mounts() -> Result<(), FsError> {
        let image = build(&Tree::new(), 1)?;
        let superblock = Super::parse(&image)?;

        assert_eq!(superblock.generation, 1);
        assert_ne!(superblock.tree_root, 0);
        assert_eq!(superblock.tree_blocks, DEFAULT_TREE_BLOCKS);
        Ok(())
    }

    #[test]
    fn mkfs_starts_from_tree_checkpoint() -> Result<(), FsError> {
        let mut tree = Tree::new();
        tree.file("seed", vec![1; BLOCK])?;

        let image = build(&tree, 1)?;
        let superblock = Super::parse(&image)?;

        assert_ne!(superblock.tree_root, 0);
        assert_eq!(superblock.tree_at, crate::layout::SUPERS);
        assert_eq!(superblock.region(Area::Log).bytes, BLOCK as u64 + crate::log::ALIGN);
        Ok(())
    }

    #[test]
    fn both_superblock_copies_written() -> Result<(), FsError> {
        let image = build(&Tree::new(), 3)?;

        assert_eq!(Super::parse(&image), Super::parse(&image[BLOCK..]));
        Ok(())
    }

    #[test]
    fn image_covers_whole_blocks() -> Result<(), FsError> {
        let mut tree = Tree::new();
        tree.file("a", vec![1; BLOCK + 1])?;

        let image = build(&tree, 1)?;

        assert_eq!(image.len() % BLOCK, 0);
        Ok(())
    }

    #[test]
    fn hole_costs_no_payload() -> Result<(), FsError> {
        let mut tree = Tree::new();
        tree.file("sparse", vec![0; 4 * BLOCK])?;

        let image = build(&tree, 1)?;

        assert_eq!(Super::parse(&image)?.region(Area::Log).bytes, 0);
        Ok(())
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
    fn directory_reopens_instead_of_duplicating() -> Result<(), FsError> {
        let mut tree = Tree::new();
        tree.dir("docs")?.file("one", vec![])?;
        tree.dir("docs")?.file("two", vec![])?;

        assert_eq!(tree.nodes.len(), 1);
        Ok(())
    }

    #[test]
    fn directory_over_file_refused() -> Result<(), FsError> {
        let mut tree = Tree::new();
        tree.file("name", vec![])?;

        assert_eq!(tree.dir("name").err(), Some(FsError::Kind));
        Ok(())
    }
}
