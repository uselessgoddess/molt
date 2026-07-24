//! Fixed-memory copy-on-write metadata tree.
//!
//! Leaves hold typed filesystem keys and internal nodes hold separators. Every
//! mutation writes a new root-to-leaf path; only a later superblock swing makes
//! that root durable. Nodes are checksummed independently, so mount can reject
//! a torn tree and fall back to the older checkpoint.

use core::cmp::Ordering;

use molt_block::{Device, Disk};

use crate::crc::crc32c;
use crate::layout::{BLOCK, Kind, MAX_NAME, MAX_TREE_BLOCKS, Object, Super};
use crate::{FsError, Name, Volume};

const MAGIC: [u8; 8] = *b"MOLTBTR3";
const HEADER: usize = 64;
const KEY_BYTES: usize = 272;
const VALUE_BYTES: usize = 32;
const CAPACITY: usize = 12;
const MAX_HEIGHT: usize = 8;
const CACHE_SLOTS: usize = 4;
const WORDS: usize = MAX_TREE_BLOCKS as usize / 64;

const OBJECT: u8 = 1;
const DIRENT: u8 = 2;
const WRITE: u8 = 3;

/// Activity counters for the bounded metadata-node cache.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CacheStats {
    /// Node lookups served without device I/O.
    pub hits: u64,
    /// Node lookups that fetched and parsed a block.
    pub misses: u64,
    /// Valid nodes replaced after the cache filled.
    pub evictions: u64,
}

/// Shape and cache activity of the current metadata tree.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TreeStats {
    /// Physical block of the pending or durable root, zero for an empty tree.
    pub root: u64,
    /// Root-to-leaf levels, zero for an empty tree.
    pub height: u8,
    /// Nodes reachable from `root`.
    pub nodes: u32,
    /// Cache counters at the time of the walk.
    pub cache: CacheStats,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) struct Key {
    bytes: [u8; KEY_BYTES],
}

impl Key {
    pub fn object(object: u32) -> Self {
        let mut key = Self::default();
        key.bytes[0] = OBJECT;
        key.bytes[1..5].copy_from_slice(&object.to_le_bytes());
        key
    }

    pub fn dirent(parent: u32, name: &Name) -> Self {
        let mut key = Self::dirent_start(parent);
        key.bytes[5] = name.len() as u8;
        key.bytes[6..6 + name.len()].copy_from_slice(name.as_bytes());
        key
    }

    pub fn dirent_start(parent: u32) -> Self {
        let mut key = Self::default();
        key.bytes[0] = DIRENT;
        key.bytes[1..5].copy_from_slice(&parent.to_le_bytes());
        key
    }

    pub fn write(object: u32, cursor: u64) -> Self {
        let mut key = Self::default();
        key.bytes[0] = WRITE;
        key.bytes[1..5].copy_from_slice(&object.to_le_bytes());
        key.bytes[8..16].copy_from_slice(&cursor.to_le_bytes());
        key
    }

    pub fn write_start(object: u32) -> Self {
        Self::write(object, 0)
    }

    fn tag(self) -> u8 {
        self.bytes[0]
    }

    fn object_id(self) -> u32 {
        u32::from_le_bytes(self.bytes[1..5].try_into().expect("fixed key field"))
    }

    pub fn is_dirent(self, parent: u32) -> bool {
        self.tag() == DIRENT && self.object_id() == parent
    }

    pub fn is_write(self, object: u32) -> bool {
        self.tag() == WRITE && self.object_id() == object
    }

    pub fn cursor(self) -> u64 {
        u64::from_le_bytes(self.bytes[8..16].try_into().expect("fixed key field"))
    }

    pub fn name(self) -> Result<Name, FsError> {
        if self.tag() != DIRENT {
            return Err(FsError::Corrupt);
        }
        let len = self.bytes[5] as usize;
        Name::new(&self.bytes[6..6 + len]).map_err(|_| FsError::Corrupt)
    }

    fn check(self) -> Result<(), FsError> {
        match self.tag() {
            OBJECT if self.bytes[5..].iter().all(|byte| *byte == 0) => Ok(()),
            DIRENT => {
                let len = self.bytes[5] as usize;
                if len == 0 || len > MAX_NAME || self.bytes[6 + len..].iter().any(|byte| *byte != 0)
                {
                    return Err(FsError::Corrupt);
                }
                self.name().map(|_| ())
            }
            WRITE
                if self.bytes[5..8].iter().all(|byte| *byte == 0)
                    && self.bytes[16..].iter().all(|byte| *byte == 0) =>
            {
                Ok(())
            }
            _ => Err(FsError::Corrupt),
        }
    }

    fn cmp_dirent(&self, other: &Self) -> Ordering {
        self.object_id().cmp(&other.object_id()).then_with(|| {
            let left = self.bytes[5] as usize;
            let right = other.bytes[5] as usize;
            self.bytes[6..6 + left].cmp(&other.bytes[6..6 + right])
        })
    }
}

impl Default for Key {
    fn default() -> Self {
        Self { bytes: [0; KEY_BYTES] }
    }
}

impl Ord for Key {
    fn cmp(&self, other: &Self) -> Ordering {
        self.tag().cmp(&other.tag()).then_with(|| match self.tag() {
            OBJECT => self.object_id().cmp(&other.object_id()),
            DIRENT => self.cmp_dirent(other),
            WRITE => self
                .object_id()
                .cmp(&other.object_id())
                .then_with(|| self.cursor().cmp(&other.cursor())),
            _ => self.bytes.cmp(&other.bytes),
        })
    }
}

impl PartialOrd for Key {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Copy, Default)]
pub(crate) struct Value {
    bytes: [u8; VALUE_BYTES],
}

impl Value {
    pub fn object(object: Object) -> Self {
        let mut value = Self::default();
        value.bytes[0] = object.kind.byte();
        value.bytes[4..8].copy_from_slice(&object.count.to_le_bytes());
        value.bytes[8..16].copy_from_slice(&object.size.to_le_bytes());
        value
    }

    pub fn dirent(object: u32) -> Self {
        let mut value = Self::default();
        value.bytes[..4].copy_from_slice(&object.to_le_bytes());
        value
    }

    pub fn write(offset: u64, bytes: u32) -> Self {
        let mut value = Self::default();
        value.bytes[..8].copy_from_slice(&offset.to_le_bytes());
        value.bytes[8..12].copy_from_slice(&bytes.to_le_bytes());
        value
    }

    pub fn as_object(self) -> Result<Object, FsError> {
        let kind = match self.bytes[0] {
            0 => Kind::Dir,
            1 => Kind::File,
            _ => return Err(FsError::Corrupt),
        };
        Ok(Object {
            kind,
            start: 0,
            count: u32::from_le_bytes(self.bytes[4..8].try_into().expect("fixed value field")),
            size: u64::from_le_bytes(self.bytes[8..16].try_into().expect("fixed value field")),
        })
    }

    pub fn as_dirent(self) -> u32 {
        u32::from_le_bytes(self.bytes[..4].try_into().expect("fixed value field"))
    }

    pub fn as_write(self) -> (u64, u32) {
        (
            u64::from_le_bytes(self.bytes[..8].try_into().expect("fixed value field")),
            u32::from_le_bytes(self.bytes[8..12].try_into().expect("fixed value field")),
        )
    }
}

#[derive(Clone, Copy)]
struct Node {
    level: u8,
    len: u8,
    generation: u64,
    keys: [Key; CAPACITY],
    values: [Value; CAPACITY],
    children: [u64; CAPACITY + 1],
}

impl Node {
    const EMPTY: Self = Self {
        level: 0,
        len: 0,
        generation: 0,
        keys: [Key { bytes: [0; KEY_BYTES] }; CAPACITY],
        values: [Value { bytes: [0; VALUE_BYTES] }; CAPACITY],
        children: [0; CAPACITY + 1],
    };

    fn leaf(generation: u64) -> Self {
        Self { generation, ..Self::EMPTY }
    }

    fn lower(&self, key: Key) -> usize {
        let mut low = 0;
        let mut high = self.len as usize;
        while low < high {
            let middle = low + (high - low) / 2;
            if self.keys[middle] < key {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        low
    }

    fn upper(&self, key: Key) -> usize {
        let mut low = 0;
        let mut high = self.len as usize;
        while low < high {
            let middle = low + (high - low) / 2;
            if self.keys[middle] <= key {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        low
    }

    fn parse(block: &[u8; BLOCK]) -> Result<Self, FsError> {
        if block[..MAGIC.len()] != MAGIC || u32_at(block, 8) != 3 {
            return Err(FsError::Corrupt);
        }
        if node_crc(block) != u32_at(block, 32) {
            return Err(FsError::Checksum);
        }
        let level = block[12];
        let len = block[13] as usize;
        if level as usize >= MAX_HEIGHT || len == 0 || len > CAPACITY {
            return Err(FsError::Corrupt);
        }
        let mut node = Self { level, len: len as u8, generation: u64_at(block, 16), ..Self::EMPTY };
        for at in 0..len {
            let start = HEADER + at * KEY_BYTES;
            node.keys[at].bytes.copy_from_slice(&block[start..start + KEY_BYTES]);
            node.keys[at].check()?;
            if at > 0 && node.keys[at - 1] >= node.keys[at] {
                return Err(FsError::Corrupt);
            }
        }
        let values = HEADER + CAPACITY * KEY_BYTES;
        if level == 0 {
            for at in 0..len {
                let start = values + at * VALUE_BYTES;
                node.values[at].bytes.copy_from_slice(&block[start..start + VALUE_BYTES]);
            }
        } else {
            for at in 0..=len {
                node.children[at] = u64_at(block, values + at * 8);
                if node.children[at] == 0 {
                    return Err(FsError::Corrupt);
                }
            }
        }
        Ok(node)
    }

    fn encode(self, block: &mut [u8; BLOCK]) {
        block.fill(0);
        block[..MAGIC.len()].copy_from_slice(&MAGIC);
        block[8..12].copy_from_slice(&3u32.to_le_bytes());
        block[12] = self.level;
        block[13] = self.len;
        block[16..24].copy_from_slice(&self.generation.to_le_bytes());
        for at in 0..self.len as usize {
            let start = HEADER + at * KEY_BYTES;
            block[start..start + KEY_BYTES].copy_from_slice(&self.keys[at].bytes);
        }
        let values = HEADER + CAPACITY * KEY_BYTES;
        if self.level == 0 {
            for at in 0..self.len as usize {
                let start = values + at * VALUE_BYTES;
                block[start..start + VALUE_BYTES].copy_from_slice(&self.values[at].bytes);
            }
        } else {
            for at in 0..=self.len as usize {
                block[values + at * 8..values + at * 8 + 8]
                    .copy_from_slice(&self.children[at].to_le_bytes());
            }
        }
        let checksum = node_crc(block);
        block[32..36].copy_from_slice(&checksum.to_le_bytes());
    }
}

#[derive(Clone, Copy)]
struct CacheEntry {
    valid: bool,
    visited: bool,
    block: u64,
    node: Node,
}

impl CacheEntry {
    const EMPTY: Self = Self { valid: false, visited: false, block: 0, node: Node::EMPTY };
}

struct Cache {
    entries: [CacheEntry; CACHE_SLOTS],
    hand: usize,
    stats: CacheStats,
}

impl Cache {
    const fn new() -> Self {
        Self {
            entries: [CacheEntry::EMPTY; CACHE_SLOTS],
            hand: 0,
            stats: CacheStats { hits: 0, misses: 0, evictions: 0 },
        }
    }

    fn get(&mut self, block: u64) -> Option<Node> {
        for entry in &mut self.entries {
            if entry.valid && entry.block == block {
                entry.visited = true;
                self.stats.hits += 1;
                return Some(entry.node);
            }
        }
        self.stats.misses += 1;
        None
    }

    fn put(&mut self, block: u64, node: Node) {
        if let Some(entry) =
            self.entries.iter_mut().find(|entry| entry.valid && entry.block == block)
        {
            entry.node = node;
            entry.visited = true;
            return;
        }
        if let Some(at) = self.entries.iter().position(|entry| !entry.valid) {
            self.entries[at] = CacheEntry { valid: true, visited: false, block, node };
            return;
        }

        // SIEVE's useful core: hits only set a bit, while eviction's hand
        // clears visited candidates and lazily promotes them past this round.
        loop {
            let at = self.hand;
            self.hand = (self.hand + 1) % CACHE_SLOTS;
            if self.entries[at].visited {
                self.entries[at].visited = false;
                continue;
            }
            self.entries[at] = CacheEntry { valid: true, visited: false, block, node };
            self.stats.evictions += 1;
            return;
        }
    }
}

#[derive(Clone, Copy)]
struct Branch {
    left: u64,
    separator: Option<Key>,
    right: Option<u64>,
}

/// The mutable state of one unpublished tree generation.
#[derive(Clone, Copy)]
pub(crate) struct TreeTransaction {
    pub root: u64,
    generation: u64,
    tree_at: u64,
    protected: [u64; WORDS],
    used: [u64; WORDS],
}

/// One bounded node cache shared by lookups and tree mutations.
pub(crate) struct MetadataTree {
    cache: Cache,
}

impl MetadataTree {
    pub const fn new() -> Self {
        Self { cache: Cache::new() }
    }

    pub fn begin<D: Disk>(
        &mut self,
        volume: &mut Volume<'_, D>,
    ) -> Result<TreeTransaction, FsError> {
        let checkpoint = volume.checkpoint();
        let mut transaction = TreeTransaction {
            root: checkpoint.tree_root,
            generation: checkpoint.generation.checked_add(1).ok_or(FsError::Full)?,
            tree_at: checkpoint.tree_at,
            protected: [0; WORDS],
            used: [0; WORDS],
        };
        self.mark(volume, checkpoint.tree_root, &mut transaction.protected)?;
        if let Some(root) = volume.previous_tree() {
            self.mark(volume, root, &mut transaction.protected)?;
        }
        transaction.used = transaction.protected;
        Ok(transaction)
    }

    pub fn get<D: Device>(
        &mut self,
        volume: &mut Volume<'_, D>,
        root: u64,
        key: Key,
    ) -> Result<Option<Value>, FsError> {
        if root == 0 {
            return Ok(None);
        }
        let mut at = root;
        loop {
            let node = self.read(volume, at)?;
            if node.level == 0 {
                let index = node.lower(key);
                if index < node.len as usize && node.keys[index] == key {
                    return Ok(Some(node.values[index]));
                }
                return Ok(None);
            }
            at = node.children[node.upper(key)];
        }
    }

    pub fn next<D: Device>(
        &mut self,
        volume: &mut Volume<'_, D>,
        root: u64,
        key: Key,
        inclusive: bool,
    ) -> Result<Option<(Key, Value)>, FsError> {
        if root == 0 {
            return Ok(None);
        }
        let mut path_blocks = [0; MAX_HEIGHT];
        let mut path_children = [0usize; MAX_HEIGHT];
        let mut depth = 0;
        let mut at = root;
        loop {
            let node = self.read(volume, at)?;
            if node.level == 0 {
                let index = if inclusive { node.lower(key) } else { node.upper(key) };
                if index < node.len as usize {
                    return Ok(Some((node.keys[index], node.values[index])));
                }
                break;
            }
            if depth >= MAX_HEIGHT {
                return Err(FsError::Corrupt);
            }
            let child = node.upper(key);
            path_blocks[depth] = at;
            path_children[depth] = child;
            depth += 1;
            at = node.children[child];
        }

        while depth > 0 {
            depth -= 1;
            let parent = self.read(volume, path_blocks[depth])?;
            let child = path_children[depth];
            if child >= parent.len as usize {
                continue;
            }
            at = parent.children[child + 1];
            loop {
                let node = self.read(volume, at)?;
                if node.level == 0 {
                    return Ok((node.len > 0).then_some((node.keys[0], node.values[0])));
                }
                at = node.children[0];
            }
        }
        Ok(None)
    }

    pub fn insert<D: Disk>(
        &mut self,
        volume: &mut Volume<'_, D>,
        transaction: &mut TreeTransaction,
        key: Key,
        value: Value,
    ) -> Result<(), FsError> {
        if transaction.root == 0 {
            let mut leaf = Node::leaf(transaction.generation);
            leaf.keys[0] = key;
            leaf.values[0] = value;
            leaf.len = 1;
            transaction.root = self.write_new(volume, transaction, leaf)?;
            return Ok(());
        }

        let mut path_blocks = [0; MAX_HEIGHT];
        let mut path_children = [0usize; MAX_HEIGHT];
        let mut depth = 0;
        let mut leaf_at = transaction.root;
        let leaf = loop {
            let node = self.read(volume, leaf_at)?;
            if node.level == 0 {
                break node;
            }
            if depth >= MAX_HEIGHT - 1 {
                return Err(FsError::Full);
            }
            let child = node.upper(key);
            path_blocks[depth] = leaf_at;
            path_children[depth] = child;
            depth += 1;
            leaf_at = node.children[child];
        };

        let mut branch = self.insert_leaf(volume, transaction, leaf, key, value)?;
        self.release(transaction, leaf_at);

        while depth > 0 {
            depth -= 1;
            let parent_at = path_blocks[depth];
            let parent = self.read(volume, parent_at)?;
            let child = path_children[depth];
            branch = self.rewrite_parent(volume, transaction, parent, child, branch)?;
            self.release(transaction, parent_at);
        }

        transaction.root = if let (Some(separator), Some(right)) = (branch.separator, branch.right)
        {
            let child = self.read(volume, branch.left)?;
            let mut root =
                Node { level: child.level + 1, generation: transaction.generation, ..Node::EMPTY };
            root.len = 1;
            root.keys[0] = separator;
            root.children[0] = branch.left;
            root.children[1] = right;
            self.write_new(volume, transaction, root)?
        } else {
            branch.left
        };
        Ok(())
    }

    pub fn stats<D: Device>(
        &mut self,
        volume: &mut Volume<'_, D>,
        root: u64,
    ) -> Result<TreeStats, FsError> {
        if root == 0 {
            return Ok(TreeStats { cache: self.cache.stats, ..TreeStats::default() });
        }
        let root_node = self.read(volume, root)?;
        let mut stack = [0; MAX_TREE_BLOCKS as usize];
        stack[0] = root;
        let mut depth = 1;
        let mut nodes = 0u32;
        while depth > 0 {
            depth -= 1;
            let node = self.read(volume, stack[depth])?;
            nodes = nodes.checked_add(1).ok_or(FsError::Corrupt)?;
            if node.level > 0 {
                for &child in &node.children[..=node.len as usize] {
                    if depth >= stack.len() {
                        return Err(FsError::Corrupt);
                    }
                    stack[depth] = child;
                    depth += 1;
                }
            }
        }
        Ok(TreeStats { root, height: root_node.level + 1, nodes, cache: self.cache.stats })
    }

    fn insert_leaf<D: Disk>(
        &mut self,
        volume: &mut Volume<'_, D>,
        transaction: &mut TreeTransaction,
        leaf: Node,
        key: Key,
        value: Value,
    ) -> Result<Branch, FsError> {
        let index = leaf.lower(key);
        if index < leaf.len as usize && leaf.keys[index] == key {
            let mut node = leaf;
            node.generation = transaction.generation;
            node.values[index] = value;
            return Ok(Branch {
                left: self.write_new(volume, transaction, node)?,
                separator: None,
                right: None,
            });
        }

        let total = leaf.len as usize + 1;
        let mut keys = [Key::default(); CAPACITY + 1];
        let mut values = [Value::default(); CAPACITY + 1];
        keys[..index].copy_from_slice(&leaf.keys[..index]);
        values[..index].copy_from_slice(&leaf.values[..index]);
        keys[index] = key;
        values[index] = value;
        let len = leaf.len as usize;
        keys[index + 1..=len].copy_from_slice(&leaf.keys[index..len]);
        values[index + 1..=len].copy_from_slice(&leaf.values[index..len]);
        if total <= CAPACITY {
            let mut node = Node::leaf(transaction.generation);
            node.len = total as u8;
            node.keys[..total].copy_from_slice(&keys[..total]);
            node.values[..total].copy_from_slice(&values[..total]);
            return Ok(Branch {
                left: self.write_new(volume, transaction, node)?,
                separator: None,
                right: None,
            });
        }

        let middle = total / 2;
        let mut left = Node::leaf(transaction.generation);
        left.len = middle as u8;
        left.keys[..middle].copy_from_slice(&keys[..middle]);
        left.values[..middle].copy_from_slice(&values[..middle]);
        let right_len = total - middle;
        let mut right = Node::leaf(transaction.generation);
        right.len = right_len as u8;
        right.keys[..right_len].copy_from_slice(&keys[middle..total]);
        right.values[..right_len].copy_from_slice(&values[middle..total]);
        let separator = right.keys[0];
        let left_at = self.write_new(volume, transaction, left)?;
        let right_at = self.write_new(volume, transaction, right)?;
        Ok(Branch { left: left_at, separator: Some(separator), right: Some(right_at) })
    }

    fn rewrite_parent<D: Disk>(
        &mut self,
        volume: &mut Volume<'_, D>,
        transaction: &mut TreeTransaction,
        parent: Node,
        child: usize,
        branch: Branch,
    ) -> Result<Branch, FsError> {
        let Some(separator) = branch.separator else {
            let mut node = parent;
            node.generation = transaction.generation;
            node.children[child] = branch.left;
            return Ok(Branch {
                left: self.write_new(volume, transaction, node)?,
                separator: None,
                right: None,
            });
        };
        let right = branch.right.ok_or(FsError::Corrupt)?;
        let total = parent.len as usize + 1;
        let mut keys = [Key::default(); CAPACITY + 1];
        let mut children = [0u64; CAPACITY + 2];
        keys[..child].copy_from_slice(&parent.keys[..child]);
        keys[child] = separator;
        keys[child + 1..total].copy_from_slice(&parent.keys[child..parent.len as usize]);
        children[..=child].copy_from_slice(&parent.children[..=child]);
        children[child] = branch.left;
        children[child + 1] = right;
        children[child + 2..=total]
            .copy_from_slice(&parent.children[child + 1..=parent.len as usize]);

        if total <= CAPACITY {
            let mut node =
                Node { level: parent.level, generation: transaction.generation, ..Node::EMPTY };
            node.len = total as u8;
            node.keys[..total].copy_from_slice(&keys[..total]);
            node.children[..=total].copy_from_slice(&children[..=total]);
            return Ok(Branch {
                left: self.write_new(volume, transaction, node)?,
                separator: None,
                right: None,
            });
        }

        let middle = total / 2;
        let mut left_node =
            Node { level: parent.level, generation: transaction.generation, ..Node::EMPTY };
        left_node.len = middle as u8;
        left_node.keys[..middle].copy_from_slice(&keys[..middle]);
        left_node.children[..=middle].copy_from_slice(&children[..=middle]);
        let right_len = total - middle - 1;
        let mut right_node =
            Node { level: parent.level, generation: transaction.generation, ..Node::EMPTY };
        right_node.len = right_len as u8;
        right_node.keys[..right_len].copy_from_slice(&keys[middle + 1..total]);
        right_node.children[..=right_len].copy_from_slice(&children[middle + 1..=total]);
        let left_at = self.write_new(volume, transaction, left_node)?;
        let right_at = self.write_new(volume, transaction, right_node)?;
        Ok(Branch { left: left_at, separator: Some(keys[middle]), right: Some(right_at) })
    }

    fn read<D: Device>(&mut self, volume: &mut Volume<'_, D>, at: u64) -> Result<Node, FsError> {
        check_block(volume.checkpoint(), at)?;
        if let Some(node) = self.cache.get(at) {
            return Ok(node);
        }
        let node = Node::parse(volume.block(at)?)?;
        self.cache.put(at, node);
        Ok(node)
    }

    fn write_new<D: Disk>(
        &mut self,
        volume: &mut Volume<'_, D>,
        transaction: &mut TreeTransaction,
        node: Node,
    ) -> Result<u64, FsError> {
        let checkpoint = volume.checkpoint();
        let mut found = None;
        for offset in 0..checkpoint.tree_blocks {
            if !bit(transaction.used, offset) {
                found = Some(offset);
                break;
            }
        }
        let offset = found.ok_or(FsError::Full)?;
        set(&mut transaction.used, offset);
        let at = checkpoint.tree_at + u64::from(offset);
        let mut block = [0; BLOCK];
        node.encode(&mut block);
        volume.write_tree_block(at, &block)?;
        self.cache.put(at, node);
        Ok(at)
    }

    fn release(&mut self, transaction: &mut TreeTransaction, at: u64) {
        let Some(relative) = at.checked_sub(transaction.tree_at) else {
            return;
        };
        let Ok(offset) = u32::try_from(relative) else {
            return;
        };
        if offset < MAX_TREE_BLOCKS
            && !bit(transaction.protected, offset)
            && bit(transaction.used, offset)
        {
            clear(&mut transaction.used, offset);
        }
    }

    fn mark<D: Device>(
        &mut self,
        volume: &mut Volume<'_, D>,
        root: u64,
        bits: &mut [u64; WORDS],
    ) -> Result<(), FsError> {
        if root == 0 {
            return Ok(());
        }
        let checkpoint = volume.checkpoint();
        let mut stack = [0; MAX_TREE_BLOCKS as usize];
        stack[0] = root;
        let mut depth = 1;
        while depth > 0 {
            depth -= 1;
            let at = stack[depth];
            let offset = tree_offset(checkpoint, at)?;
            if bit(*bits, offset) {
                continue;
            }
            set(bits, offset);
            let node = self.read(volume, at)?;
            if node.level > 0 {
                for &child in &node.children[..=node.len as usize] {
                    if depth >= stack.len() {
                        return Err(FsError::Corrupt);
                    }
                    stack[depth] = child;
                    depth += 1;
                }
            }
        }
        Ok(())
    }
}

/// Checks every reachable node before a superblock can become the mounted one.
pub(crate) fn verify<D: Device>(
    device: &mut D,
    scratch: &mut [u8; BLOCK],
    checkpoint: Super,
) -> Result<(), FsError> {
    if checkpoint.tree_root == 0 {
        return Ok(());
    }
    let mut used = [0; WORDS];
    let mut stack = [(0u64, 0xffu8); MAX_TREE_BLOCKS as usize];
    stack[0] = (checkpoint.tree_root, 0xff);
    let mut depth = 1;
    while depth > 0 {
        depth -= 1;
        let (at, expected) = stack[depth];
        let offset = tree_offset(checkpoint, at)?;
        if bit(used, offset) {
            return Err(FsError::Corrupt);
        }
        set(&mut used, offset);
        volume_read(device, scratch, at)?;
        let node = Node::parse(scratch)?;
        if node.generation > checkpoint.generation
            || (at == checkpoint.tree_root && node.generation != checkpoint.generation)
        {
            return Err(FsError::Corrupt);
        }
        if expected != 0xff && node.level != expected {
            return Err(FsError::Corrupt);
        }
        if node.level > 0 {
            for &child in &node.children[..=node.len as usize] {
                if depth >= stack.len() {
                    return Err(FsError::Corrupt);
                }
                stack[depth] = (child, node.level - 1);
                depth += 1;
            }
        }
    }
    Ok(())
}

fn check_block(checkpoint: Super, at: u64) -> Result<(), FsError> {
    tree_offset(checkpoint, at).map(|_| ())
}

fn tree_offset(checkpoint: Super, at: u64) -> Result<u32, FsError> {
    let offset = at.checked_sub(checkpoint.tree_at).ok_or(FsError::Corrupt)?;
    if offset >= u64::from(checkpoint.tree_blocks) {
        return Err(FsError::Corrupt);
    }
    u32::try_from(offset).map_err(|_| FsError::Corrupt)
}

fn bit(bits: [u64; WORDS], at: u32) -> bool {
    bits[at as usize / 64] & (1 << (at % 64)) != 0
}

fn set(bits: &mut [u64; WORDS], at: u32) {
    bits[at as usize / 64] |= 1 << (at % 64);
}

fn clear(bits: &mut [u64; WORDS], at: u32) {
    bits[at as usize / 64] &= !(1 << (at % 64));
}

fn node_crc(block: &[u8; BLOCK]) -> u32 {
    let mut copy = *block;
    copy[32..36].fill(0);
    crc32c(&copy)
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("fixed node field"))
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("fixed node field"))
}

fn volume_read<D: Device>(device: &mut D, block: &mut [u8; BLOCK], at: u64) -> Result<(), FsError> {
    let sectors = (BLOCK / molt_block::SECTOR) as u64;
    device.read(at.checked_mul(sectors).ok_or(FsError::Corrupt)?, block).map_err(FsError::Device)
}

const _: () = assert!(HEADER + CAPACITY * KEY_BYTES + (CAPACITY + 1) * VALUE_BYTES <= BLOCK);
