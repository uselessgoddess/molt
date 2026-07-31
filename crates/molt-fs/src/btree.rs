//! Copy-on-write metadata tree, kept on the heap.
//!
//! Leaves hold typed filesystem keys and internal nodes hold separators. Every
//! mutation writes a new root-to-leaf path; only a later superblock swing makes
//! that root durable. Nodes are checksummed independently, so mount can reject
//! a torn tree and fall back to the older checkpoint.
//!
//! A node is a block's worth of keys, so none of them is ever a local: nodes
//! are allocated zeroed, shared out of the cache behind [`Rc`], and split by
//! reading the merged sequence position by position rather than by staging it
//! in a scratch node. What is left on the stack of a mutation is one path of
//! block numbers.

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cmp::Ordering;

use crate::bitmap::Bitmap;
use crate::crc::Crc;
use crate::layout::{BLOCK, Kind, MAX_NAME, Object, Super};
use crate::mem::{Unique, Zeroed};
use crate::{FsError, Name, Volume, mem};

const MAGIC: [u8; 8] = *b"MOLTBTR4";
const HEADER: usize = 64;
const KEY_BYTES: usize = 272;
const VALUE_BYTES: usize = 32;
const CAPACITY: usize = 12;
const MAX_HEIGHT: usize = 8;

/// Nodes the cache keeps. A path is at most [`MAX_HEIGHT`] deep, so this holds
/// the working set of a mutation plus the levels a neighbouring one reuses.
const CACHE_SLOTS: usize = 16;

const OBJECT: u8 = 1;
const DIRENT: u8 = 2;
const EXTENT: u8 = 3;

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

    /// Keyed on the byte past the extent's last, so one descent finds the first
    /// extent that can reach into a read and the walk from there is in order.
    pub fn extent(object: u32, end: u64) -> Self {
        let mut key = Self::default();
        key.bytes[0] = EXTENT;
        key.bytes[1..5].copy_from_slice(&object.to_le_bytes());
        key.bytes[8..16].copy_from_slice(&end.to_le_bytes());
        key
    }

    fn tag(&self) -> u8 {
        self.bytes[0]
    }

    fn object_id(&self) -> u32 {
        u32::from_le_bytes(self.bytes[1..5].try_into().expect("fixed key field"))
    }

    pub fn is_dirent(&self, parent: u32) -> bool {
        self.tag() == DIRENT && self.object_id() == parent
    }

    /// The file an extent key belongs to, if it is one.
    pub fn extent_object(&self) -> Option<u32> {
        (self.tag() == EXTENT).then(|| self.object_id())
    }

    pub fn is_extent(&self, object: u32) -> bool {
        self.extent_object() == Some(object)
    }

    /// One past the last byte an extent key covers.
    pub fn end(&self) -> u64 {
        u64::from_le_bytes(self.bytes[8..16].try_into().expect("fixed key field"))
    }

    pub fn name(&self) -> Result<Name, FsError> {
        if self.tag() != DIRENT {
            return Err(FsError::Corrupt);
        }
        let len = self.bytes[5] as usize;
        Name::new(&self.bytes[6..6 + len]).map_err(|_| FsError::Corrupt)
    }

    fn check(&self) -> Result<(), FsError> {
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
            EXTENT
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
            EXTENT => {
                self.object_id().cmp(&other.object_id()).then_with(|| self.end().cmp(&other.end()))
            }
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

    /// A run of one write record's payload: where the record sits in the log,
    /// how far into its payload the run starts, and how long it is.
    pub fn extent(at: u64, skip: u32, len: u32) -> Self {
        let mut value = Self::default();
        value.bytes[..8].copy_from_slice(&at.to_le_bytes());
        value.bytes[8..12].copy_from_slice(&skip.to_le_bytes());
        value.bytes[12..16].copy_from_slice(&len.to_le_bytes());
        value
    }

    /// An extent covering nothing, left behind where a later write swallowed
    /// one whole and did not land on its key.
    pub fn whiteout() -> Self {
        Self::default()
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

    pub fn as_extent(self) -> (u64, u32, u32) {
        (
            u64::from_le_bytes(self.bytes[..8].try_into().expect("fixed value field")),
            u32::from_le_bytes(self.bytes[8..12].try_into().expect("fixed value field")),
            u32::from_le_bytes(self.bytes[12..16].try_into().expect("fixed value field")),
        )
    }
}

struct Node {
    level: u8,
    len: u8,
    generation: u64,
    keys: [Key; CAPACITY],
    values: [Value; CAPACITY],
    children: [u64; CAPACITY + 1],
}

// SAFETY: every field is pod, all-zero bytes are an empty leaf of generation zero.
unsafe impl Zeroed for Node {}

impl Node {
    /// An empty node, allocated zeroed.
    fn empty() -> Result<Unique<Self>, FsError> {
        Unique::zeroed()
    }

    fn leaf(generation: u64) -> Result<Unique<Self>, FsError> {
        let mut node = Self::empty()?;
        node.generation = generation;
        Ok(node)
    }

    fn inner(level: u8, generation: u64) -> Result<Unique<Self>, FsError> {
        let mut node = Self::leaf(generation)?;
        node.level = level;
        Ok(node)
    }

    /// A heap copy of a cached node, ready to be rewritten in place.
    fn copy(&self) -> Result<Unique<Self>, FsError> {
        let mut node = Self::inner(self.level, self.generation)?;
        node.len = self.len;
        node.keys.copy_from_slice(&self.keys);
        node.values.copy_from_slice(&self.values);
        node.children.copy_from_slice(&self.children);
        Ok(node)
    }

    fn lower(&self, key: &Key) -> usize {
        self.search(|held| held < key)
    }

    fn upper(&self, key: &Key) -> usize {
        self.search(|held| held <= key)
    }

    /// First index whose key fails `before`, by binary search.
    fn search(&self, before: impl Fn(&Key) -> bool) -> usize {
        let mut low = 0;
        let mut high = self.len as usize;
        while low < high {
            let middle = low + (high - low) / 2;
            if before(&self.keys[middle]) {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        low
    }

    fn parse(block: &[u8; BLOCK]) -> Result<Unique<Self>, FsError> {
        if block[..MAGIC.len()] != MAGIC || u32_at(block, 8) != 4 {
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
        let mut node = Self::inner(level, u64_at(block, 16))?;
        node.len = len as u8;
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

    fn encode(&self, block: &mut [u8; BLOCK]) {
        block.fill(0);
        block[..MAGIC.len()].copy_from_slice(&MAGIC);
        block[8..12].copy_from_slice(&4u32.to_le_bytes());
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

struct Entry {
    visited: bool,
    block: u64,
    node: Rc<Node>,
}

/// Nodes kept parsed, so a path walked twice costs one device read.
///
/// Slots are handed out as [`Rc`]: a mutation holds the node it is rewriting
/// while the cache still holds the same node for the next lookup, and neither
/// has to copy four kilobytes to do it.
struct Cache {
    entries: Vec<Entry>,
    hand: usize,
    stats: CacheStats,
}

impl Cache {
    fn new() -> Result<Self, FsError> {
        let mut entries = Vec::new();
        entries.try_reserve_exact(CACHE_SLOTS)?;
        Ok(Self { entries, hand: 0, stats: CacheStats { hits: 0, misses: 0, evictions: 0 } })
    }

    fn get(&mut self, block: u64) -> Option<Rc<Node>> {
        for entry in &mut self.entries {
            if entry.block == block {
                entry.visited = true;
                self.stats.hits += 1;
                return Some(Rc::clone(&entry.node));
            }
        }
        self.stats.misses += 1;
        None
    }

    fn put(&mut self, block: u64, node: Rc<Node>) {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.block == block) {
            entry.node = node;
            entry.visited = true;
            return;
        }
        if self.entries.len() < CACHE_SLOTS {
            self.entries.push(Entry { visited: false, block, node });
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
            self.entries[at] = Entry { visited: false, block, node };
            self.stats.evictions += 1;
            return;
        }
    }
}

/// What a rewritten level hands its parent: one new node, or two and the
/// separator between them.
///
/// The separator is boxed. It is the widest thing on the path by far, only a
/// split ever carries one, and this value is copied up every level.
enum Branch {
    One(u64),
    Split { left: u64, separator: Box<Key>, right: u64 },
}

/// The mutable state of one unpublished tree generation.
pub(crate) struct TreeTransaction {
    pub root: u64,
    generation: u64,
    tree_at: u64,
    /// Arena blocks the newest and previous checkpoints still reach.
    protected: Bitmap,
    /// Those plus the blocks this generation has written.
    used: Bitmap,
}

/// One bounded node cache shared by lookups and tree mutations.
pub(crate) struct MetadataTree {
    cache: Cache,
}

impl TreeTransaction {
    /// A copy of the transaction, if the heap has room for its two maps.
    pub fn try_clone(&self) -> Result<Self, FsError> {
        Ok(Self { protected: self.protected.try_clone()?, used: self.used.try_clone()?, ..*self })
    }
}

impl MetadataTree {
    pub fn new() -> Result<Self, FsError> {
        Ok(Self { cache: Cache::new()? })
    }

    pub async fn begin(&mut self, volume: &mut Volume) -> Result<TreeTransaction, FsError> {
        let checkpoint = volume.checkpoint();
        let mut transaction = TreeTransaction {
            root: checkpoint.tree_root,
            generation: checkpoint.generation.checked_add(1).ok_or(FsError::Full)?,
            tree_at: checkpoint.tree_at,
            protected: Bitmap::new(checkpoint.tree_blocks)?,
            used: Bitmap::new(checkpoint.tree_blocks)?,
        };
        self.mark(volume, checkpoint.tree_root, &mut transaction.protected).await?;
        if let Some(root) = volume.previous_tree() {
            self.mark(volume, root, &mut transaction.protected).await?;
        }
        transaction.used = transaction.protected.try_clone()?;
        Ok(transaction)
    }

    pub async fn get(
        &mut self,
        volume: &mut Volume,
        root: u64,
        key: &Key,
    ) -> Result<Option<Value>, FsError> {
        if root == 0 {
            return Ok(None);
        }
        let mut at = root;
        for _ in 0..MAX_HEIGHT {
            let node = self.read(volume, at).await?;
            if node.level == 0 {
                let index = node.lower(key);
                if index < node.len as usize && node.keys[index] == *key {
                    return Ok(Some(node.values[index]));
                }
                return Ok(None);
            }
            at = node.children[node.upper(key)];
        }
        Err(FsError::Corrupt)
    }

    pub async fn next(
        &mut self,
        volume: &mut Volume,
        root: u64,
        key: &Key,
        inclusive: bool,
    ) -> Result<Option<(Key, Value)>, FsError> {
        if root == 0 {
            return Ok(None);
        }
        let mut path = Path::default();
        let mut at = root;
        loop {
            let node = self.read(volume, at).await?;
            if node.level == 0 {
                let index = if inclusive { node.lower(key) } else { node.upper(key) };
                if index < node.len as usize {
                    return Ok(Some((node.keys[index], node.values[index])));
                }
                break;
            }
            let child = node.upper(key);
            path.push(at, child)?;
            at = node.children[child];
        }

        while let Some((parent_at, child)) = path.pop() {
            let parent = self.read(volume, parent_at).await?;
            if child >= parent.len as usize {
                continue;
            }
            at = parent.children[child + 1];
            loop {
                let node = self.read(volume, at).await?;
                if node.level == 0 {
                    return Ok((node.len > 0).then_some((node.keys[0], node.values[0])));
                }
                at = node.children[0];
            }
        }
        Ok(None)
    }

    pub async fn insert(
        &mut self,
        volume: &mut Volume,
        transaction: &mut TreeTransaction,
        key: &Key,
        value: Value,
    ) -> Result<(), FsError> {
        if transaction.root == 0 {
            let mut leaf = Node::leaf(transaction.generation)?;
            leaf.keys[0] = *key;
            leaf.values[0] = value;
            leaf.len = 1;
            transaction.root = self.write_new(volume, transaction, leaf).await?;
            return Ok(());
        }

        let mut path = Path::default();
        let mut leaf_at = transaction.root;
        let leaf = loop {
            let node = self.read(volume, leaf_at).await?;
            if node.level == 0 {
                break node;
            }
            // A split at the leaf may add a level, so the path stops one short.
            if path.depth + 1 >= MAX_HEIGHT {
                return Err(FsError::Full);
            }
            let child = node.upper(key);
            path.push(leaf_at, child)?;
            leaf_at = node.children[child];
        };

        let mut branch = self.insert_leaf(volume, transaction, &leaf, key, value).await?;
        drop(leaf);
        self.release(transaction, leaf_at);

        while let Some((parent_at, child)) = path.pop() {
            let parent = self.read(volume, parent_at).await?;
            branch = self.rewrite_parent(volume, transaction, &parent, child, branch).await?;
            drop(parent);
            self.release(transaction, parent_at);
        }

        transaction.root = match branch {
            Branch::One(at) => at,
            Branch::Split { left, separator, right } => {
                let level = self.read(volume, left).await?.level + 1;
                let mut root = Node::inner(level, transaction.generation)?;
                root.len = 1;
                root.keys[0] = *separator;
                root.children[0] = left;
                root.children[1] = right;
                self.write_new(volume, transaction, root).await?
            }
        };
        Ok(())
    }

    pub async fn stats(&mut self, volume: &mut Volume, root: u64) -> Result<TreeStats, FsError> {
        if root == 0 {
            return Ok(TreeStats { cache: self.cache.stats, ..TreeStats::default() });
        }
        let blocks = volume.checkpoint().tree_blocks as usize;
        let height = self.read(volume, root).await?.level + 1;
        let mut pending = Vec::new();
        mem::push(&mut pending, root)?;
        let mut nodes = 0u32;
        while let Some(at) = pending.pop() {
            let node = self.read(volume, at).await?;
            nodes = nodes.checked_add(1).ok_or(FsError::Corrupt)?;
            if node.level > 0 {
                // An arena block can appear once in a well-formed tree, so a
                // walk that outgrows the arena is walking a cycle.
                if pending.len() + node.len as usize + 1 > blocks {
                    return Err(FsError::Corrupt);
                }
                mem::extend(&mut pending, &node.children[..=node.len as usize])?;
            }
        }
        Ok(TreeStats { root, height, nodes, cache: self.cache.stats })
    }

    async fn insert_leaf(
        &mut self,
        volume: &mut Volume,
        transaction: &mut TreeTransaction,
        leaf: &Node,
        key: &Key,
        value: Value,
    ) -> Result<Branch, FsError> {
        let index = leaf.lower(key);
        if index < leaf.len as usize && leaf.keys[index] == *key {
            let mut node = leaf.copy()?;
            node.generation = transaction.generation;
            node.values[index] = value;
            return Ok(Branch::One(self.write_new(volume, transaction, node).await?));
        }

        // The insert read by position: `leaf` with `key` spliced in at `index`.
        // Staging it in a scratch node first would put another node's worth of
        // keys on the stack for a sequence only ever read in order.
        let total = leaf.len as usize + 1;
        let item = |slot: usize| match slot.cmp(&index) {
            Ordering::Less => (&leaf.keys[slot], leaf.values[slot]),
            Ordering::Equal => (key, value),
            Ordering::Greater => (&leaf.keys[slot - 1], leaf.values[slot - 1]),
        };
        let fill = |node: &mut Node, from: usize, to: usize| {
            node.len = (to - from) as u8;
            for slot in from..to {
                let (key, value) = item(slot);
                node.keys[slot - from] = *key;
                node.values[slot - from] = value;
            }
        };

        if total <= CAPACITY {
            let mut node = Node::leaf(transaction.generation)?;
            fill(&mut node, 0, total);
            return Ok(Branch::One(self.write_new(volume, transaction, node).await?));
        }

        let middle = total / 2;
        let mut left = Node::leaf(transaction.generation)?;
        fill(&mut left, 0, middle);
        let mut right = Node::leaf(transaction.generation)?;
        fill(&mut right, middle, total);
        let separator = Box::try_new(right.keys[0])?;
        let left_at = self.write_new(volume, transaction, left).await?;
        let right_at = self.write_new(volume, transaction, right).await?;
        Ok(Branch::Split { left: left_at, separator, right: right_at })
    }

    async fn rewrite_parent(
        &mut self,
        volume: &mut Volume,
        transaction: &mut TreeTransaction,
        parent: &Node,
        child: usize,
        branch: Branch,
    ) -> Result<Branch, FsError> {
        let (left, separator, right) = match branch {
            Branch::One(at) => {
                let mut node = parent.copy()?;
                node.generation = transaction.generation;
                node.children[child] = at;
                return Ok(Branch::One(self.write_new(volume, transaction, node).await?));
            }
            Branch::Split { left, separator, right } => (left, separator, right),
        };

        // As at the leaf, read by position: the split child's separator and its
        // second half spliced into the parent's keys and links.
        let total = parent.len as usize + 1;
        let key_at = |slot: usize| match slot.cmp(&child) {
            Ordering::Less => &parent.keys[slot],
            Ordering::Equal => &separator,
            Ordering::Greater => &parent.keys[slot - 1],
        };
        let child_at = |slot: usize| match slot {
            slot if slot < child => parent.children[slot],
            slot if slot == child => left,
            slot if slot == child + 1 => right,
            slot => parent.children[slot - 1],
        };
        let fill = |node: &mut Node, from: usize, to: usize| {
            node.len = (to - from) as u8;
            for slot in from..to {
                node.keys[slot - from] = *key_at(slot);
            }
            for slot in from..=to {
                node.children[slot - from] = child_at(slot);
            }
        };

        if total <= CAPACITY {
            let mut node = Node::inner(parent.level, transaction.generation)?;
            fill(&mut node, 0, total);
            return Ok(Branch::One(self.write_new(volume, transaction, node).await?));
        }

        // The middle key rises to the parent instead of staying in either half.
        let middle = total / 2;
        let mut left_node = Node::inner(parent.level, transaction.generation)?;
        fill(&mut left_node, 0, middle);
        let mut right_node = Node::inner(parent.level, transaction.generation)?;
        fill(&mut right_node, middle + 1, total);
        let rising = Box::try_new(*key_at(middle))?;
        let left_at = self.write_new(volume, transaction, left_node).await?;
        let right_at = self.write_new(volume, transaction, right_node).await?;
        Ok(Branch::Split { left: left_at, separator: rising, right: right_at })
    }

    async fn read(&mut self, volume: &mut Volume, at: u64) -> Result<Rc<Node>, FsError> {
        check_block(&volume.checkpoint(), at)?;
        if let Some(node) = self.cache.get(at) {
            return Ok(node);
        }
        let node = Node::parse(volume.block(at).await?)?.into_shared();
        self.cache.put(at, Rc::clone(&node));
        Ok(node)
    }

    async fn write_new(
        &mut self,
        volume: &mut Volume,
        transaction: &mut TreeTransaction,
        node: Unique<Node>,
    ) -> Result<u64, FsError> {
        let checkpoint = volume.checkpoint();
        let offset = transaction.used.first_clear().ok_or(FsError::Full)?;
        transaction.used.set(offset);
        let at = checkpoint.tree_at + u64::from(offset);
        // Encoded straight into the buffer the write leaves on, so a node
        // reaches the device without a block of it being copied twice.
        volume.write_tree_block(at, |block| node.encode(block)).await?;
        self.cache.put(at, node.into_shared());
        Ok(at)
    }

    /// Returns a node this generation wrote to the arena, unless an older
    /// checkpoint still reaches it.
    fn release(&mut self, transaction: &mut TreeTransaction, at: u64) {
        let Some(relative) = at.checked_sub(transaction.tree_at) else {
            return;
        };
        let Ok(offset) = u32::try_from(relative) else {
            return;
        };
        if !transaction.protected.get(offset) && transaction.used.get(offset) {
            transaction.used.clear(offset);
        }
    }

    async fn mark(
        &mut self,
        volume: &mut Volume,
        root: u64,
        bits: &mut Bitmap,
    ) -> Result<(), FsError> {
        if root == 0 {
            return Ok(());
        }
        let checkpoint = volume.checkpoint();
        let mut pending = Vec::new();
        mem::push(&mut pending, root)?;
        while let Some(at) = pending.pop() {
            let offset = tree_offset(&checkpoint, at)?;
            if bits.get(offset) {
                continue;
            }
            bits.set(offset);
            let node = self.read(volume, at).await?;
            if node.level > 0 {
                if pending.len() + node.len as usize + 1 > checkpoint.tree_blocks as usize {
                    return Err(FsError::Corrupt);
                }
                mem::extend(&mut pending, &node.children[..=node.len as usize])?;
            }
        }
        Ok(())
    }
}

/// The blocks a descent came through, and which link it took out of each.
///
/// This is all a mutation keeps on the stack: [`MAX_HEIGHT`] block numbers and
/// as many indices, rather than the nodes they name.
#[derive(Default)]
struct Path {
    blocks: [u64; MAX_HEIGHT],
    children: [usize; MAX_HEIGHT],
    depth: usize,
}

impl Path {
    fn push(&mut self, block: u64, child: usize) -> Result<(), FsError> {
        if self.depth >= MAX_HEIGHT {
            return Err(FsError::Corrupt);
        }
        self.blocks[self.depth] = block;
        self.children[self.depth] = child;
        self.depth += 1;
        Ok(())
    }

    fn pop(&mut self) -> Option<(u64, usize)> {
        self.depth = self.depth.checked_sub(1)?;
        Some((self.blocks[self.depth], self.children[self.depth]))
    }
}

/// Checks every reachable node before a superblock can become the mounted one.
pub(crate) async fn verify(volume: &mut Volume, checkpoint: &Super) -> Result<(), FsError> {
    if checkpoint.tree_root == 0 {
        return Ok(());
    }
    let mut used = Bitmap::new(checkpoint.tree_blocks)?;
    let mut pending = Vec::new();
    // No level is expected of the root; every child is expected one below it.
    mem::push(&mut pending, (checkpoint.tree_root, None))?;
    while let Some((at, expected)) = pending.pop() {
        let offset = tree_offset(checkpoint, at)?;
        if used.get(offset) {
            return Err(FsError::Corrupt);
        }
        used.set(offset);
        let node = Node::parse(volume.raw(at).await?)?;
        if node.generation > checkpoint.generation
            || (at == checkpoint.tree_root && node.generation != checkpoint.generation)
        {
            return Err(FsError::Corrupt);
        }
        if expected.is_some_and(|level| level != node.level) {
            return Err(FsError::Corrupt);
        }
        if node.level > 0 {
            if pending.len() + node.len as usize + 1 > checkpoint.tree_blocks as usize {
                return Err(FsError::Corrupt);
            }
            for &child in &node.children[..=node.len as usize] {
                mem::push(&mut pending, (child, Some(node.level - 1)))?;
            }
        }
    }
    Ok(())
}

fn check_block(checkpoint: &Super, at: u64) -> Result<(), FsError> {
    tree_offset(checkpoint, at).map(|_| ())
}

fn tree_offset(checkpoint: &Super, at: u64) -> Result<u32, FsError> {
    let offset = at.checked_sub(checkpoint.tree_at).ok_or(FsError::Corrupt)?;
    if offset >= u64::from(checkpoint.tree_blocks) {
        return Err(FsError::Corrupt);
    }
    u32::try_from(offset).map_err(|_| FsError::Corrupt)
}

/// The node checksum: the block with its own checksum field read as zero.
fn node_crc(block: &[u8; BLOCK]) -> u32 {
    let mut crc = Crc::new();
    crc.update(&block[..32]);
    crc.update(&[0; 4]);
    crc.update(&block[36..]);
    crc.finish()
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("fixed node field"))
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("fixed node field"))
}

const _: () = assert!(HEADER + CAPACITY * KEY_BYTES + (CAPACITY + 1) * VALUE_BYTES <= BLOCK);
