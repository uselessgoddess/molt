//! Reading a mounted volume through a ring of block buffers.
//!
//! Nothing here calls a device. [`Volume`] submits reads on a [`BlockClient`]
//! and awaits them where the bytes are wanted, which is what lets a sequential
//! read ask for the blocks after the one it is waiting on. The slots hold what
//! came back: a binary search over a directory re-reads a block only when it
//! moves off every slot, and the sums block covering a file stays resident
//! while the file streams past it.

use alloc::vec::Vec;
use core::cmp::Ordering;
use core::ops::Range;

use molt_block::{Backing, BlockClient, BlockOp, Buffer, Disk, SECTOR, channel};
use molt_core::ring::RequestId;
use molt_core::task;

use crate::FsError;
use crate::crc::{Crc, crc32c};
use crate::layout::{
    Area, BLOCK, ENTRY_BYTES, EXTENT_BYTES, Entry, Extent, Kind, MAX_NAME, OBJECT_BYTES, Object,
    SUPERS, Super, buffer, u32_at,
};
use crate::name::Name;

/// Sectors per block.
const SECTORS: u64 = (BLOCK / SECTOR) as u64;

const _: () = assert!(BLOCK == molt_block::BLOCK);

/// Blocks a volume keeps a buffer for.
///
/// Small enough that finding one stays a linear scan, big enough that a
/// streaming read holds its extent record, its sums block, the block it is on
/// and the ones it asked for ahead, all at once.
const SLOTS: usize = 8;

/// How deep the ring under a volume is.
///
/// Every slot may be at the device at once, and a write travelling on the
/// scratch buffer is one more, so the ring is never the reason a read waits.
pub const DEPTH: usize = 2 * SLOTS;

/// Blocks a sequential read asks for beyond the one it needs.
const AHEAD: u32 = 3;

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

/// Puts `device` behind a ring, returning the end a volume reads through and
/// the end somebody has to pump for those reads to land.
pub fn attach<D: Disk>(device: D) -> Result<(Blocks, Backing<D, DEPTH>), FsError> {
    let (client, driver) = channel()?;
    let sectors = device.sectors();
    Ok((Blocks { client, sectors }, Backing::new(driver, device)))
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
    /// Every metadata region is checked against the checksum the superblock
    /// records, so a corrupt volume fails here rather than at the first lookup
    /// that happens to touch the damaged block.
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
        for area in Area::ALL {
            let region = superblock.region(area);
            if self.checksum(region.at, region.bytes).await? != region.crc {
                return Err(FsError::Checksum);
            }
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

    /// Reads one object record.
    pub async fn object(&mut self, id: u32) -> Result<Object, FsError> {
        Object::parse(self.record(Area::Objects, id as u64, OBJECT_BYTES).await?)
    }

    /// Reads `dir`'s entry at `index`, in name order.
    pub async fn entry(&mut self, dir: &Object, index: u32) -> Result<(Name, u32), FsError> {
        let entry = self.at(dir, index).await?;
        Ok((self.name(entry).await?, entry.object))
    }

    /// Finds `name` in `dir`, returning the object it names.
    ///
    /// Entries are sorted, so this is a binary search: a directory of a
    /// thousand names costs ten block reads, not a thousand.
    pub async fn lookup(&mut self, dir: &Object, name: &[u8]) -> Result<u32, FsError> {
        if dir.kind != Kind::Dir {
            return Err(FsError::Kind);
        }
        let (mut low, mut high) = (0, dir.count);
        while low < high {
            let middle = low + (high - low) / 2;
            let entry = self.at(dir, middle).await?;
            match self.name(entry).await?.as_bytes().cmp(name) {
                Ordering::Less => low = middle + 1,
                Ordering::Greater => high = middle,
                Ordering::Equal => return Ok(entry.object),
            }
        }
        Err(FsError::Missing)
    }

    /// Reads `file` from `offset` into `buf`, returning how many bytes landed.
    ///
    /// A read is short only at the end of the file. A logical block no extent
    /// covers is a hole and reads as zeros, which is how an image elides the
    /// all-zero blocks of a sparse file.
    pub async fn read(
        &mut self,
        file: &Object,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, FsError> {
        if file.kind != Kind::File {
            return Err(FsError::Kind);
        }
        if offset > file.size {
            return Err(FsError::Range);
        }

        let want = (file.size - offset).min(buf.len() as u64) as usize;
        let mut done = 0;
        // One run covers many logical blocks, so a read that stays inside it
        // searches for it once and afterwards only checks.
        let mut run = None;
        while done < want {
            let at = offset + done as u64;
            let logical = u32::try_from(at / BLOCK as u64).map_err(|_| FsError::Corrupt)?;
            let within = (at % BLOCK as u64) as usize;
            let take = (want - done).min(BLOCK - within);
            match self.follow(file, &mut run, logical).await? {
                Some(block) => {
                    self.ahead(run, logical).await?;
                    let source = self.data(block).await?;
                    buf[done..done + take].copy_from_slice(&source[within..within + take]);
                }
                None => buf[done..done + take].fill(0),
            }
            done += take;
        }
        Ok(want)
    }

    /// The block holding `file`'s `logical` one, reusing `run` while it covers.
    async fn follow(
        &mut self,
        file: &Object,
        run: &mut Option<Extent>,
        logical: u32,
    ) -> Result<Option<u64>, FsError> {
        if let Some(extent) = *run
            && let Some(block) = extent.covers(logical)?
        {
            return Ok(Some(block));
        }
        *run = self.locate(file, logical).await?;
        match *run {
            Some(extent) => extent.covers(logical),
            None => Ok(None),
        }
    }

    /// Asks for the blocks after `logical` in the same run.
    ///
    /// A sequential reader comes back for them, and asking now overlaps them
    /// with the read it is about to wait on. The run is already known, so this
    /// costs no metadata read — only a guess about where the caller goes next.
    /// It is a guess, so it declines rather than waiting for a free slot.
    async fn ahead(&mut self, run: Option<Extent>, logical: u32) -> Result<(), FsError> {
        let Some(run) = run else { return Ok(()) };
        for step in 1..=AHEAD {
            let Some(next) = logical.checked_add(step) else { break };
            let Some(block) = run.covers(next)? else { break };
            if block >= self.superblock.blocks || self.find(block).is_some() {
                continue;
            }
            let Some(at) = self.free() else { break };
            self.start(at, block).await?;
        }
        Ok(())
    }

    /// The extent covering `file`'s `logical` block, if it is not a hole.
    async fn locate(&mut self, file: &Object, logical: u32) -> Result<Option<Extent>, FsError> {
        let (mut low, mut high) = (0, file.count);
        while low < high {
            let middle = low + (high - low) / 2;
            let index = file.start.checked_add(middle).ok_or(FsError::Corrupt)?;
            let record = self.record(Area::Extents, index as u64, EXTENT_BYTES).await?;
            let extent = Extent::parse(record)?;
            if extent.covers(logical)?.is_some() {
                return Ok(Some(extent));
            }
            if extent.logical < logical {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        Ok(None)
    }

    async fn at(&mut self, dir: &Object, index: u32) -> Result<Entry, FsError> {
        if dir.kind != Kind::Dir {
            return Err(FsError::Kind);
        }
        if index >= dir.count {
            return Err(FsError::Missing);
        }
        let index = dir.start.checked_add(index).ok_or(FsError::Corrupt)?;
        Entry::parse(self.record(Area::Entries, index as u64, ENTRY_BYTES).await?)
    }

    /// Copies an entry's name out of the name region, which it may straddle
    /// blocks of.
    async fn name(&mut self, entry: Entry) -> Result<Name, FsError> {
        let region = self.superblock.region(Area::Names);
        let len = entry.name_len as usize;
        let end = (entry.name_at as u64).checked_add(len as u64).ok_or(FsError::Corrupt)?;
        if end > region.bytes {
            return Err(FsError::Corrupt);
        }

        let mut bytes = [0u8; MAX_NAME];
        let mut done = 0;
        while done < len {
            let at = entry.name_at as u64 + done as u64;
            let within = (at % BLOCK as u64) as usize;
            let take = (len - done).min(BLOCK - within);
            let source = self.block(region.at + at / BLOCK as u64).await?;
            bytes[done..done + take].copy_from_slice(&source[within..within + take]);
            done += take;
        }
        Name::new(&bytes[..len])
    }

    /// Borrows one fixed-size record out of a metadata region.
    async fn record(&mut self, area: Area, index: u64, size: usize) -> Result<&[u8], FsError> {
        let region = self.superblock.region(area);
        let at = index.checked_mul(size as u64).ok_or(FsError::Corrupt)?;
        if at + size as u64 > region.bytes {
            return Err(FsError::Missing);
        }
        let within = (at % BLOCK as u64) as usize;
        let block = self.block(region.at + at / BLOCK as u64).await?;
        Ok(&block[within..within + size])
    }

    /// Reads a data block and checks it against the sum recorded for it.
    async fn data(&mut self, index: u64) -> Result<&[u8; BLOCK], FsError> {
        let offset = index.checked_sub(self.superblock.data_at).ok_or(FsError::Corrupt)?;
        if offset >= self.superblock.data_blocks {
            return Err(FsError::Corrupt);
        }

        let sums = self.superblock.region(Area::Sums);
        let at = offset * 4;
        let within = (at % BLOCK as u64) as usize;
        let expected = u32_at(self.block(sums.at + at / BLOCK as u64).await?, within);

        let block = self.block(index).await?;
        if crc32c(block) != expected {
            return Err(FsError::Checksum);
        }
        Ok(block)
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

    /// Reads `blocks` in order, giving each to `take`.
    ///
    /// Every slot it can get is spent on the blocks ahead of the one being
    /// handed over, so a sweep waits for the device once rather than once per
    /// block. Each block is released as it is taken: a sweep reads a region
    /// through, and nothing behind it is coming back.
    async fn sweep(
        &mut self,
        blocks: Range<u64>,
        mut take: impl FnMut(&[u8; BLOCK]),
    ) -> Result<(), FsError> {
        let mut next = blocks.start;
        for index in blocks.clone() {
            while next < blocks.end
                && let Some(at) = self.free()
            {
                self.start(at, next).await?;
                next += 1;
            }
            take(self.block(index).await?);
            self.drop_block(index);
        }
        Ok(())
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

    /// Forgets `index`, whose slot is worth more than what it holds.
    fn drop_block(&mut self, index: u64) {
        if let Some(at) = self.find(index)
            && let Slot::Here { block, .. } = &mut self.slots[at]
        {
            *block = None;
        }
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

    pub(crate) async fn copy_aligned(
        &mut self,
        source: u64,
        target: u64,
        bytes: u64,
    ) -> Result<(), FsError> {
        if bytes % SECTOR as u64 != 0 {
            return Err(FsError::Corrupt);
        }
        self.stale(target..target.saturating_add(bytes.div_ceil(BLOCK as u64))).await;
        let mut done = 0;
        while done < bytes {
            let take = (bytes - done).min(BLOCK as u64) as usize;
            let at = |block: u64| {
                block
                    .checked_mul(SECTORS)
                    .and_then(|sector| sector.checked_add(done / SECTOR as u64))
                    .ok_or(FsError::Corrupt)
            };
            let (from, to) = (at(source)?, at(target)?);
            self.aside(|buffer| BlockOp::Read { sector: from, bytes: take, buffer }).await?;
            self.aside(|buffer| BlockOp::Write { sector: to, bytes: take, buffer }).await?;
            done += take as u64;
        }
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

    pub(crate) async fn checksum(&mut self, block: u64, bytes: u64) -> Result<u32, FsError> {
        let mut crc = Crc::new();
        let mut left = bytes;
        self.sweep(block..block + bytes.div_ceil(BLOCK as u64), |whole| {
            let take = left.min(BLOCK as u64) as usize;
            crc.update(&whole[..take]);
            left -= take as u64;
        })
        .await?;
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
    use molt_block::{Backing, Loopback};

    use super::{Volume, attach};
    use crate::crc::crc32c;
    use crate::format::{Tree, build};
    use crate::layout::{Area, BLOCK, Kind, Super};
    use crate::{DEPTH, FsError, MAX_NAME};

    fn image() -> alloc::vec::Vec<u8> {
        let mut tree = Tree::new();
        tree.file("hello.txt", b"hello, molt".to_vec()).unwrap();
        tree.file("big.bin", alloc::vec![0xa5; 3 * BLOCK + 7]).unwrap();
        tree.dir("docs").unwrap().file("readme", b"read me".to_vec()).unwrap();
        build(&tree, 1).unwrap()
    }

    fn mount(bytes: &[u8]) -> (Volume, Backing<Loopback<'_>, DEPTH>) {
        let (blocks, mut backing) = attach(Loopback::new(bytes).unwrap()).unwrap();
        let volume = backing.run(Volume::mount(blocks)).unwrap();
        (volume, backing)
    }

    #[test]
    fn file_reads_back_what_was_written() -> Result<(), FsError> {
        let bytes = image();
        let (mut volume, mut backing) = mount(&bytes);

        let mut text = [0u8; 16];
        let read = backing.run(async {
            let root = volume.object(volume.root()).await?;
            let id = volume.lookup(&root, b"hello.txt").await?;
            let file = volume.object(id).await?;
            volume.read(&file, 0, &mut text).await
        })?;

        assert_eq!(&text[..read], b"hello, molt");
        Ok(())
    }

    #[test]
    fn read_crossing_blocks_stays_contiguous() -> Result<(), FsError> {
        let bytes = image();
        let (mut volume, mut backing) = mount(&bytes);

        let mut window = [0u8; 8];
        let read = backing.run(async {
            let root = volume.object(volume.root()).await?;
            let id = volume.lookup(&root, b"big.bin").await?;
            let file = volume.object(id).await?;
            volume.read(&file, BLOCK as u64 - 4, &mut window).await
        })?;

        assert_eq!(read, 8);
        assert_eq!(window, [0xa5; 8], "block boundary lost bytes");
        Ok(())
    }

    #[test]
    fn short_read_stops_at_end() -> Result<(), FsError> {
        let bytes = image();
        let (mut volume, mut backing) = mount(&bytes);

        let read = backing.run(async {
            let root = volume.object(volume.root()).await?;
            let id = volume.lookup(&root, b"hello.txt").await?;
            let file = volume.object(id).await?;
            volume.read(&file, 6, &mut [0; 64]).await
        });

        assert_eq!(read, Ok(5));
        Ok(())
    }

    #[test]
    fn missing_name_reported() -> Result<(), FsError> {
        let bytes = image();
        let (mut volume, mut backing) = mount(&bytes);

        let found = backing.run(async {
            let root = volume.object(volume.root()).await?;
            volume.lookup(&root, b"nothing").await
        });

        assert_eq!(found, Err(FsError::Missing));
        Ok(())
    }

    #[test]
    fn entries_come_back_sorted() -> Result<(), FsError> {
        let bytes = image();
        let (mut volume, mut backing) = mount(&bytes);

        let (first, second) = backing.run(async {
            let root = volume.object(volume.root()).await?;
            let first = volume.entry(&root, 0).await?.0;
            let second = volume.entry(&root, 1).await?.0;
            Ok::<_, FsError>((first, second))
        })?;

        assert_eq!(first.as_str(), Some("big.bin"));
        assert_eq!(second.as_str(), Some("docs"));
        Ok(())
    }

    #[test]
    fn nested_directory_reachable() -> Result<(), FsError> {
        let bytes = image();
        let (mut volume, mut backing) = mount(&bytes);

        let kind = backing.run(async {
            let root = volume.object(volume.root()).await?;
            let docs = volume.lookup(&root, b"docs").await?;
            let docs = volume.object(docs).await?;
            let id = volume.lookup(&docs, b"readme").await?;
            Ok::<_, FsError>(volume.object(id).await?.kind)
        })?;

        assert_eq!(kind, Kind::File);
        Ok(())
    }

    #[test]
    fn entry_past_end_reported() -> Result<(), FsError> {
        let bytes = image();
        let (mut volume, mut backing) = mount(&bytes);

        let entry = backing.run(async {
            let root = volume.object(volume.root()).await?;
            volume.entry(&root, 3).await.map(|entry| entry.0)
        });

        assert_eq!(entry, Err(FsError::Missing));
        Ok(())
    }

    #[test]
    fn corrupt_data_block_refused() -> Result<(), FsError> {
        let mut bytes = image();
        let data = Super::parse(&bytes[..BLOCK])?.data_at;
        bytes[data as usize * BLOCK] ^= 0xff;
        let (mut volume, mut backing) = mount(&bytes);

        let read = backing.run(async {
            let root = volume.object(volume.root()).await?;
            let id = volume.lookup(&root, b"big.bin").await?;
            let file = volume.object(id).await?;
            volume.read(&file, 0, &mut [0; 8]).await
        });

        assert_eq!(read, Err(FsError::Checksum));
        Ok(())
    }

    #[test]
    fn extent_physical_overflow_refused() -> Result<(), FsError> {
        let mut bytes = image();
        let mut superblock = Super::parse(&bytes[..BLOCK])?;
        let mut extents = superblock.region(Area::Extents);
        let at = extents.at as usize * BLOCK;
        bytes[at + 8..at + 16].copy_from_slice(&u64::MAX.to_le_bytes());
        extents.crc = crc32c(&bytes[at..at + extents.bytes as usize]);
        superblock.set_region(Area::Extents, extents);
        for copy in 0..super::SUPERS {
            superblock.encode(&mut bytes[copy as usize * BLOCK..]);
        }
        let (mut volume, mut backing) = mount(&bytes);

        let read = backing.run(async {
            let root = volume.object(volume.root()).await?;
            let id = volume.lookup(&root, b"big.bin").await?;
            let file = volume.object(id).await?;
            volume.read(&file, BLOCK as u64, &mut [0; 8]).await
        });

        assert_eq!(read, Err(FsError::Corrupt));
        Ok(())
    }

    #[test]
    fn corrupt_metadata_refused_at_mount() -> Result<(), FsError> {
        let mut bytes = image();
        let superblock = Super::parse(&bytes[..BLOCK])?;
        let at = superblock.region(Area::Objects).at as usize * BLOCK;
        bytes[at] ^= 0xff;
        let (blocks, mut backing) = attach(Loopback::new(&bytes)?)?;

        let mounted = backing.run(Volume::mount(blocks));

        assert_eq!(mounted.err(), Some(FsError::Checksum), "damaged object region mounted");
        Ok(())
    }

    #[test]
    fn torn_superblock_falls_back() -> Result<(), FsError> {
        let mut bytes = image();
        bytes[0] ^= 0xff;
        let (mut volume, mut backing) = mount(&bytes);

        let found = backing.run(async {
            let root = volume.object(volume.root()).await?;
            volume.lookup(&root, b"hello.txt").await
        });

        assert!(found.is_ok(), "older copy did not serve");
        Ok(())
    }

    #[test]
    fn overlong_name_never_stored() {
        let mut tree = Tree::new();

        assert_eq!(tree.file(&"a".repeat(MAX_NAME + 1), alloc::vec![]), Err(FsError::Name));
    }
}
