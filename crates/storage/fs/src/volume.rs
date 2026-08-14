//! Block I/O and checkpoint selection through a ring of buffers.
//!
//! Nothing here calls a device. [`Volume`] submits reads on a [`BlockClient`]
//! and awaits them where the bytes are wanted. The slots hold what came back so
//! tree walks and payload reads can reuse blocks without calling a device.

use alloc::vec::Vec;
use core::ops::Range;

use molt_block::{Backing, BlockClient, BlockOp, Buffer, Queue, SECTOR, channel};
use molt_core::ring::RequestId;
use molt_core::task;

use crate::FsError;
use crate::crc::Crc;
use crate::layout::{Area, BLOCK, Region, SUPERS, Super, buffer};
use crate::log::{HEADER, Record};

/// Sectors per block.
const SECTORS: u64 = (BLOCK / SECTOR) as u64;

const _: () = assert!(BLOCK == molt_block::BLOCK);

/// Blocks a volume keeps a buffer for.
///
/// Small enough that finding one stays a linear scan, big enough that a
/// streaming read holds its tree path, checksum table, current payload block,
/// and the blocks it asked for ahead, all at once.
const SLOTS: usize = 8;

/// How deep the ring under a volume is.
///
/// Every slot may be at the device at once, and a write travelling on the
/// scratch buffer is one more, so the ring is never the reason a read waits.
pub const DEPTH: usize = 2 * SLOTS;

/// Where one slot's block is.
enum Slot {
    /// Here, holding the block it names — or nothing worth keeping.
    Here { block: Option<u64>, buffer: Buffer },
    /// At the device, filling for the block it names.
    Flight { block: u64, id: RequestId },
    /// Handed to the ring, in a submission not taken back yet.
    Lent,
}

/// The end of a block ring a volume mounts on, and the geometry it cannot ask
/// the ring for.
pub struct Blocks {
    client: BlockClient<DEPTH>,
    sectors: u64,
}

/// Puts `queue` behind a ring, returning the end a volume reads through and
/// the end somebody has to pump for those reads to land.
pub fn attach<Q: Queue>(queue: Q) -> Result<(Blocks, Backing<Q, DEPTH>), FsError> {
    let (client, driver) = channel()?;
    let sectors = queue.sectors();
    Ok((Blocks { client, sectors }, Backing::new(driver, queue)))
}

/// A mounted volume.
pub struct Volume {
    client: BlockClient<DEPTH>,
    sectors: u64,
    slots: Vec<Slot>,
    hand: usize,
    scratch: Option<Buffer>,
    superblock: Super,
    active_copy: u64,
    previous_log: Option<u64>,
    previous_tree: Option<u64>,
}

impl Volume {
    /// Mounts at the newest checkpoint that verifies.
    pub async fn mount(blocks: Blocks) -> Result<Self, FsError> {
        let Blocks { client, sectors } = blocks;
        let mut volume = Self {
            client,
            sectors,
            slots: Vec::new(),
            hand: 0,
            scratch: Some(buffer()?),
            superblock: unmounted(sectors),
            active_copy: 0,
            previous_log: None,
            previous_tree: None,
        };
        let checkpoint = volume.survey().await?;
        volume.adopt(checkpoint);
        Ok(volume)
    }

    /// Re-reads the volume as a fresh mount would.
    ///
    /// Everything the mount held about the disk is dropped for what the disk
    /// now says, so a service restarting on top of this volume comes back at
    /// the last checkpoint that was made durable.
    pub async fn remount(&mut self) -> Result<(), FsError> {
        self.stale(0..u64::MAX).await;
        self.superblock = unmounted(self.sectors);
        let checkpoint = self.survey().await?;
        self.adopt(checkpoint);
        Ok(())
    }

    /// Takes the checkpoint a survey settled on as the mounted one.
    fn adopt(&mut self, checkpoint: Checkpoint) {
        self.superblock = checkpoint.superblock;
        self.active_copy = checkpoint.active_copy;
        self.previous_log = checkpoint.previous_log;
        self.previous_tree = checkpoint.previous_tree;
    }

    /// Takes the newest superblock copy that verifies.
    ///
    /// The log structure and every reachable metadata node are checked before
    /// a candidate is adopted. Payload chunks carry their own lazy checksums.
    async fn survey(&mut self) -> Result<Checkpoint, FsError> {
        let mut copies = [None; SUPERS as usize];
        let mut last_error = FsError::Magic;
        for copy in 0..SUPERS {
            match Super::parse(self.raw(copy).await?) {
                Ok(parsed) => copies[copy as usize] = Some(parsed),
                Err(error) => last_error = error,
            }
        }

        // Newest generation first, each candidate dropped if it fails to
        // verify. Loops rather than iterator chains: every frame of one carries
        // a superblock, and mount runs on whatever stack its caller has.
        let mut rejected = [false; SUPERS as usize];
        for _ in 0..SUPERS {
            let Some(active_copy) = newest(&copies, &rejected) else {
                break;
            };
            rejected[active_copy] = true;
            let Some(superblock) = copies[active_copy] else {
                break;
            };
            if superblock.blocks.saturating_mul(SECTORS) > self.sectors {
                last_error = FsError::Corrupt;
                continue;
            }
            if let Err(error) = self.verify_checkpoint(&superblock).await {
                last_error = error;
                continue;
            }

            let mut previous = None;
            for (copy, candidate) in copies.iter().enumerate() {
                if copy == active_copy {
                    continue;
                }
                if let Some(parsed) = *candidate
                    && parsed.blocks.saturating_mul(SECTORS) <= self.sectors
                    && self.verify_checkpoint(&parsed).await.is_ok()
                {
                    previous = Some(parsed);
                    break;
                }
            }
            return Ok(Checkpoint {
                superblock,
                active_copy: active_copy as u64,
                previous_log: previous.map(|parsed| parsed.region(Area::Log).at),
                previous_tree: previous.map(|parsed| parsed.tree_root).filter(|root| *root != 0),
            });
        }
        Err(last_error)
    }

    async fn verify_checkpoint(&mut self, superblock: &Super) -> Result<(), FsError> {
        let log = superblock.region(Area::Log);
        if self.log_checksum(log).await? != log.crc {
            return Err(FsError::Checksum);
        }
        crate::btree::verify(self, superblock).await
    }

    /// The object id of the root directory.
    pub const fn root(&self) -> u32 {
        self.superblock.root
    }

    /// The generation the mounted checkpoint carries.
    pub const fn generation(&self) -> u64 {
        self.superblock.generation
    }

    pub(crate) const fn checkpoint(&self) -> Super {
        self.superblock
    }

    pub(crate) const fn active_copy(&self) -> u64 {
        self.active_copy
    }

    pub(crate) const fn previous_log(&self) -> Option<u64> {
        self.previous_log
    }

    pub(crate) const fn previous_tree(&self) -> Option<u64> {
        self.previous_tree
    }

    pub(crate) async fn commit(&mut self, copy: u64, checkpoint: Super) {
        self.stale(0..u64::MAX).await;
        self.previous_log = Some(self.superblock.region(Area::Log).at);
        self.previous_tree = (self.superblock.tree_root != 0).then_some(self.superblock.tree_root);
        self.superblock = checkpoint;
        self.active_copy = copy;
    }

    /// Reads a block, or hands back the slot already holding it.
    pub(crate) async fn block(&mut self, index: u64) -> Result<&[u8; BLOCK], FsError> {
        if index >= self.superblock.blocks {
            return Err(FsError::Corrupt);
        }
        let at = match self.find(index) {
            Some(at) => at,
            None => {
                let at = self.spare().await?;
                self.start(at, index).await?;
                at
            }
        };
        self.land(at).await?;
        match &self.slots[at] {
            Slot::Here { buffer, .. } => Ok(buffer),
            _ => Err(FsError::Corrupt),
        }
    }

    /// Starts fetching a block if one is not already resident or in flight.
    pub(crate) async fn prefetch(&mut self, index: u64) -> Result<(), FsError> {
        if index >= self.superblock.blocks || self.find(index).is_some() {
            return Ok(());
        }
        let Some(at) = self.free() else { return Ok(()) };
        self.start(at, index).await
    }

    /// Reads a block without keeping it, on the buffer kept aside.
    ///
    /// This is the way around the slots, for the walks that would sweep every
    /// one of them out for blocks nobody reads twice.
    pub(crate) async fn raw(&mut self, index: u64) -> Result<&[u8; BLOCK], FsError> {
        let sector = index.checked_mul(SECTORS).ok_or(FsError::Corrupt)?;
        let buffer = self.scratch.take().ok_or(FsError::Corrupt)?;
        let done = self.client.once(BlockOp::Read { sector, bytes: BLOCK, buffer }).await;
        self.scratch = done.buffer;
        done.result.map_err(FsError::Device)?;
        self.scratch.as_deref().ok_or(FsError::Corrupt)
    }

    /// The slot holding or fetching `index`.
    fn find(&self, index: u64) -> Option<usize> {
        self.slots.iter().position(|slot| match slot {
            Slot::Here { block, .. } => *block == Some(index),
            Slot::Flight { block, .. } => *block == index,
            Slot::Lent => false,
        })
    }

    /// A slot whose buffer is here, without waiting for one to come back.
    ///
    /// An empty one first, then a new one while the pool may still hold it: a
    /// buffer costs less than the read that fetches back what it would have
    /// evicted. Only when neither is there does the hand come round and take a
    /// block somebody may still want.
    fn free(&mut self) -> Option<usize> {
        let empty = |slot: &Slot| matches!(slot, Slot::Here { block: None, .. });
        if let Some(at) = self.slots.iter().position(empty) {
            return Some(at);
        }
        if let Some(at) = self.grow() {
            return Some(at);
        }
        for _ in 0..self.slots.len() {
            let at = self.hand;
            self.hand = (self.hand + 1) % self.slots.len();
            if matches!(self.slots[at], Slot::Here { .. }) {
                return Some(at);
            }
        }
        None
    }

    /// Adds a slot, while the pool may hold one and the heap agrees.
    ///
    /// The pool fills as reads ask for it: a mount touches a handful of blocks,
    /// and a volume nobody streams from has no use for eight buffers. A refused
    /// buffer is not an error here — the caller takes a resident one instead.
    fn grow(&mut self) -> Option<usize> {
        if self.slots.len() == SLOTS || self.slots.try_reserve(1).is_err() {
            return None;
        }
        self.slots.push(Slot::Here { block: None, buffer: buffer().ok()? });
        Some(self.slots.len() - 1)
    }

    /// A slot to read into, waiting for one if every buffer is at the device.
    async fn spare(&mut self) -> Result<usize, FsError> {
        if let Some(at) = self.free() {
            return Ok(at);
        }
        if self.slots.is_empty() {
            return Err(FsError::Memory);
        }
        // The hand points at the oldest read outstanding. Whether it landed is
        // the next reader's problem: an empty slot is what is wanted here.
        let at = self.hand;
        let _ = self.land(at).await;
        Ok(at)
    }

    /// Submits a read for `index` into `at`, whose buffer must be here.
    async fn start(&mut self, at: usize, index: u64) -> Result<(), FsError> {
        let sector = index.checked_mul(SECTORS).ok_or(FsError::Corrupt)?;
        let Slot::Here { buffer, .. } = core::mem::replace(&mut self.slots[at], Slot::Lent) else {
            return Err(FsError::Corrupt);
        };
        let mut op = BlockOp::Read { sector, bytes: BLOCK, buffer };
        let id = loop {
            match self.client.submit(op) {
                Ok(id) => break id,
                Err(refused) => op = refused,
            }
            task::defer().await;
        };
        self.slots[at] = Slot::Flight { block: index, id };
        Ok(())
    }

    /// Waits for a slot's read, if it has one outstanding.
    async fn land(&mut self, at: usize) -> Result<(), FsError> {
        let &Slot::Flight { block, id } = &self.slots[at] else { return Ok(()) };
        let done = self.client.settle(id).await;
        let buffer = done.buffer.ok_or(FsError::Corrupt)?;
        // A refused read leaves the slot holding nothing rather than half a
        // block, so the next reader asks again instead of believing it.
        let landed = done.result.is_ok();
        self.slots[at] = Slot::Here { block: landed.then_some(block), buffer };
        done.result.map_err(FsError::Device)
    }

    /// Drops what a write is about to change under the slots.
    ///
    /// Only the blocks it covers: a transaction appends records and rewrites
    /// tree nodes between reads of both, and emptying every slot at each write
    /// would fetch them all again.
    async fn stale(&mut self, blocks: Range<u64>) {
        for at in 0..self.slots.len() {
            let covers = match &self.slots[at] {
                Slot::Here { block, .. } => block.is_some_and(|block| blocks.contains(&block)),
                Slot::Flight { block, .. } => blocks.contains(block),
                Slot::Lent => false,
            };
            if !covers {
                continue;
            }
            let _ = self.land(at).await;
            if let Slot::Here { block, .. } = &mut self.slots[at] {
                *block = None;
            }
        }
    }

    /// Runs one operation on the buffer kept aside, which comes back either
    /// way. Nothing a write carries belongs in a slot.
    async fn aside(&mut self, make: impl FnOnce(Buffer) -> BlockOp) -> Result<(), FsError> {
        let buffer = self.scratch.take().ok_or(FsError::Corrupt)?;
        let done = self.client.once(make(buffer)).await;
        self.scratch = done.buffer;
        done.result.map_err(FsError::Device)
    }

    /// Fills the buffer kept aside and borrows it.
    fn fill(&mut self, write: impl FnOnce(&mut [u8; BLOCK])) -> Result<(), FsError> {
        let mut buffer = self.scratch.take().ok_or(FsError::Corrupt)?;
        write(&mut buffer);
        self.scratch = Some(buffer);
        Ok(())
    }

    pub(crate) async fn write_aligned(
        &mut self,
        block: u64,
        offset: u64,
        bytes: &[u8],
    ) -> Result<(), FsError> {
        if offset % SECTOR as u64 != 0 || bytes.len() % SECTOR != 0 {
            return Err(FsError::Corrupt);
        }
        let sector = block
            .checked_mul(SECTORS)
            .and_then(|sector| sector.checked_add(offset / SECTOR as u64))
            .ok_or(FsError::Corrupt)?;
        let first = block + offset / BLOCK as u64;
        let span = ((offset % BLOCK as u64) as usize + bytes.len()).div_ceil(BLOCK) as u64;
        self.stale(first..first + span).await;
        let mut filled = Err(FsError::Corrupt);
        self.fill(|buffer| {
            filled = match buffer.get_mut(..bytes.len()) {
                Some(head) => {
                    head.copy_from_slice(bytes);
                    Ok(())
                }
                None => Err(FsError::Corrupt),
            };
        })?;
        filled?;
        self.aside(|buffer| BlockOp::Write { sector, bytes: bytes.len(), buffer }).await
    }

    /// Writes one tree arena block, which `encode` fills in place.
    pub(crate) async fn write_tree_block(
        &mut self,
        at: u64,
        encode: impl FnOnce(&mut [u8; BLOCK]),
    ) -> Result<(), FsError> {
        let checkpoint = self.checkpoint();
        let end = checkpoint
            .tree_at
            .checked_add(u64::from(checkpoint.tree_blocks))
            .ok_or(FsError::Corrupt)?;
        if at < checkpoint.tree_at || at >= end {
            return Err(FsError::Corrupt);
        }
        let sector = at.checked_mul(SECTORS).ok_or(FsError::Corrupt)?;
        self.stale(at..at + 1).await;
        self.fill(encode)?;
        self.aside(|buffer| BlockOp::Write { sector, bytes: BLOCK, buffer }).await
    }

    /// Hashes only the headers of a payload-log region.
    ///
    /// Their per-record checksums protect file bytes when those bytes are read;
    /// mount only needs this compact structural commitment to select a root.
    pub(crate) async fn log_checksum(&mut self, region: Region) -> Result<u32, FsError> {
        let mut crc = Crc::new();
        let mut cursor = 0;
        while cursor < region.bytes {
            let within = (cursor % BLOCK as u64) as usize;
            let end = within.checked_add(HEADER).ok_or(FsError::Corrupt)?;
            if end > BLOCK {
                return Err(FsError::Corrupt);
            }
            let mut header = [0; HEADER];
            header.copy_from_slice(
                &self.block(region.at + cursor / BLOCK as u64).await?[within..end],
            );
            let record = Record::parse(&header)?;
            crc.update(&header);
            cursor = cursor
                .checked_add(record.span().map_err(|_| FsError::Corrupt)?)
                .ok_or(FsError::Corrupt)?;
        }
        if cursor != region.bytes {
            return Err(FsError::Corrupt);
        }
        Ok(crc.finish())
    }

    pub(crate) async fn write_checkpoint(
        &mut self,
        copy: u64,
        value: Super,
    ) -> Result<(), FsError> {
        if copy >= SUPERS {
            return Err(FsError::Corrupt);
        }
        self.stale(copy..copy + 1).await;
        self.fill(|buffer| {
            buffer.fill(0);
            value.encode(&mut buffer[..]);
        })?;
        self.aside(|buffer| BlockOp::Write { sector: copy * SECTORS, bytes: SECTOR, buffer }).await
    }

    pub(crate) async fn flush(&mut self) -> Result<(), FsError> {
        self.client.once(BlockOp::Flush).await.result.map_err(FsError::Device)
    }
}

/// What is known of a volume before a survey: how far the device goes, which
/// is all a read of a superblock copy has to stay inside.
fn unmounted(sectors: u64) -> Super {
    Super { blocks: sectors / SECTORS, ..Super::default() }
}

/// Which checkpoint a mount settled on, and what the one before it held.
struct Checkpoint {
    superblock: Super,
    active_copy: u64,
    previous_log: Option<u64>,
    previous_tree: Option<u64>,
}

/// The copy holding the newest generation, ties going to the lower copy.
fn newest(copies: &[Option<Super>], rejected: &[bool]) -> Option<usize> {
    let mut best: Option<(usize, u64)> = None;
    for (copy, parsed) in copies.iter().enumerate() {
        if let Some(parsed) = parsed
            && !rejected[copy]
            && best.is_none_or(|(_, generation)| parsed.generation > generation)
        {
            best = Some((copy, parsed.generation));
        }
    }
    best.map(|(copy, _)| copy)
}

#[cfg(all(test, feature = "format"))]
mod tests {
    use molt_block::{Loopback, Serial};

    use crate::format::{Tree, build};
    use crate::layout::{BLOCK, Super};
    use crate::{FsError, Journal, MAX_NAME, Name};

    fn image() -> alloc::vec::Vec<u8> {
        let mut tree = Tree::new();
        tree.file("hello.txt", b"hello, molt".to_vec()).unwrap();
        tree.file("big.bin", alloc::vec![0xa5; 3 * BLOCK + 7]).unwrap();
        tree.dir("docs").unwrap().file("readme", b"read me".to_vec()).unwrap();
        build(&tree, 1).unwrap()
    }

    #[test]
    fn corrupt_tree_refused_at_mount() -> Result<(), FsError> {
        let mut bytes = image();
        let superblock = Super::parse(&bytes[..BLOCK])?;
        bytes[superblock.tree_root as usize * BLOCK] ^= 0xff;
        let (blocks, mut backing) = super::attach(Serial::new(Loopback::read(&bytes)?))?;

        let mounted = backing.run(Journal::mount(blocks));

        assert_eq!(mounted.err(), Some(FsError::Corrupt));
        Ok(())
    }

    #[test]
    fn torn_superblock_falls_back() -> Result<(), FsError> {
        let mut bytes = image();
        bytes[0] ^= 0xff;
        let (blocks, mut backing) = super::attach(Serial::new(Loopback::read(&bytes)?))?;
        let mut journal = backing.run(Journal::mount(blocks))?;
        let root = journal.root();

        let found = backing.run(journal.lookup(root, &Name::try_from("hello.txt")?));

        assert!(found.is_ok());
        Ok(())
    }

    #[test]
    fn overlong_name_never_stored() {
        let mut tree = Tree::new();

        assert_eq!(tree.file(&"a".repeat(MAX_NAME + 1), alloc::vec![]), Err(FsError::Name));
    }
}
