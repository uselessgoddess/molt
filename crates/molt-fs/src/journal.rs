//! Writable view of a COW-tree checkpoint and its payload log.
//!
//! A transaction appends typed records in the active bank while it has room and
//! path-copies metadata nodes. When the bank fills, live payload slices stream
//! into the bank not held by either durable checkpoint. [`Journal::sync`]
//! flushes those writes before it publishes their log and tree root, then
//! flushes the superblock.

use molt_block::SECTOR;

use crate::btree::{Key, MetadataTree, TreeStats, TreeTransaction, Value};
use crate::crc::{Crc, crc32c};
use crate::layout::{Area, BLOCK, Kind, Object, Region};
use crate::log::{ALIGN, HEADER, Record};
use crate::volume::Blocks;
use crate::{FsError, Name, Volume};

const AHEAD: u64 = 4;
const VERIFIED: usize = 8;

#[derive(Clone, Copy)]
struct VerifiedRecord {
    bank: u64,
    cursor: u64,
    chunk: u32,
    crc: u32,
}

#[derive(Clone, Copy)]
struct ExtentHint {
    file: u32,
    start: u64,
    end: u64,
    at: u64,
    skip: u32,
    len: u32,
}

/// The unpublished half of a checkpoint: where the new log bank is, how far it
/// has been filled, and the tree generation indexing it.
struct Transaction {
    at: u64,
    bytes: u64,
    tree: TreeTransaction,
}

impl Transaction {
    /// A copy to roll back to, if the heap has room for the tree's arena maps.
    fn try_clone(&self) -> Result<Self, FsError> {
        Ok(Self { tree: self.tree.try_clone()?, ..*self })
    }
}

/// A mounted writable filesystem.
pub struct Journal {
    volume: Volume,
    transaction: Option<Transaction>,
    tree: MetadataTree,
    next_object: u32,
    verified: [Option<VerifiedRecord>; VERIFIED],
    verify_hand: usize,
    extent_hint: Option<ExtentHint>,
}

impl Journal {
    /// Mounts the newest valid checkpoint and replays its mutation log.
    pub async fn mount(blocks: Blocks) -> Result<Self, FsError> {
        let mut journal = Self {
            volume: Volume::mount(blocks).await?,
            transaction: None,
            tree: MetadataTree::new()?,
            next_object: 0,
            verified: [None; VERIFIED],
            verify_hand: 0,
            extent_hint: None,
        };
        journal.replay().await?;
        Ok(journal)
    }

    /// Remounts the volume, dropping every uncommitted change.
    ///
    /// What was never synced was never a checkpoint, so it does not come back:
    /// this is the state a power cut would have left, reached deliberately.
    pub async fn remount(&mut self) -> Result<(), FsError> {
        self.volume.remount().await?;
        self.transaction = None;
        self.tree = MetadataTree::new()?;
        self.verified.fill(None);
        self.verify_hand = 0;
        self.extent_hint = None;
        self.replay().await
    }

    /// Sizes and validates object space from the mounted checkpoint.
    async fn replay(&mut self) -> Result<(), FsError> {
        let root = self.tree_root();
        if root == 0 {
            return Err(FsError::Corrupt);
        }
        let mut next = 0u32;
        loop {
            let found = self.tree.next(&mut self.volume, root, &Key::object(next), true).await?;
            let Some((key, value)) = found else { break };
            let Some(object) = key.as_object() else { break };
            if object != next {
                return Err(FsError::Corrupt);
            }
            value.as_object()?;
            next = next.checked_add(1).ok_or(FsError::Corrupt)?;
        }
        if next == 0 || self.volume.root() >= next {
            return Err(FsError::Corrupt);
        }
        self.next_object = next;
        if self.object(self.volume.root()).await?.kind != Kind::Dir {
            return Err(FsError::Corrupt);
        }
        self.validate_objects().await?;
        self.validate_log().await
    }

    /// The object id of the root directory.
    pub const fn root(&self) -> u32 {
        self.volume.root()
    }

    /// The generation of the active durable checkpoint.
    pub const fn generation(&self) -> u64 {
        self.volume.generation()
    }

    /// Returns the current COW tree shape and cache counters.
    pub async fn tree_stats(&mut self) -> Result<TreeStats, FsError> {
        let root = self.tree_root();
        self.tree.stats(&mut self.volume, root).await
    }

    /// Returns the current object state after replaying every mutation.
    pub async fn object(&mut self, id: u32) -> Result<Object, FsError> {
        if id >= self.next_object {
            return Err(FsError::Missing);
        }
        let root = self.tree_root();
        self.tree
            .get(&mut self.volume, root, &Key::object(id))
            .await?
            .ok_or(FsError::Corrupt)?
            .as_object()
    }

    /// Finds `name` in a directory, including objects created since mkfs.
    pub async fn lookup(&mut self, dir: u32, name: &Name) -> Result<u32, FsError> {
        let object = self.object(dir).await?;
        if object.kind != Kind::Dir {
            return Err(FsError::Kind);
        }
        let root = self.tree_root();
        let key = Key::dirent(dir, name);
        if let Some(value) = self.tree.get(&mut self.volume, root, &key).await? {
            return Ok(value.as_dirent());
        }
        Err(FsError::Missing)
    }

    /// Reads `index` in bytewise name order.
    pub async fn entry(&mut self, dir: u32, index: u32) -> Result<(Name, u32), FsError> {
        let object = self.object(dir).await?;
        if object.kind != Kind::Dir {
            return Err(FsError::Kind);
        }
        if index >= object.count {
            return Err(FsError::Missing);
        }
        let mut key = Key::dirent_start(dir);
        let mut inclusive = true;
        for position in 0..=index {
            let root = self.tree_root();
            let found = self.tree.next(&mut self.volume, root, &key, inclusive).await?;
            let Some((next, value)) = found else { return Err(FsError::Corrupt) };
            if !next.is_dirent(dir) {
                return Err(FsError::Corrupt);
            }
            if !inclusive && next <= key {
                return Err(FsError::Corrupt);
            }
            key = next;
            inclusive = false;
            if position == index {
                return Ok((next.name()?, value.as_dirent()));
            }
        }
        Err(FsError::Corrupt)
    }

    /// Reads current file contents from indexed payload records.
    pub async fn read(&mut self, file: u32, offset: u64, buf: &mut [u8]) -> Result<usize, FsError> {
        let object = self.object(file).await?;
        if object.kind != Kind::File {
            return Err(FsError::Kind);
        }
        if offset > object.size {
            return Err(FsError::Range);
        }
        let want = (object.size - offset).min(buf.len() as u64) as usize;
        buf[..want].fill(0);

        let read_end = offset.checked_add(want as u64).ok_or(FsError::Corrupt)?;
        let root = self.tree_root();
        // One descent lands on the first extent whose end is past `offset`, and
        // extents of a file never overlap, so the walk from there is in file
        // order and the first one starting past the window ends it.
        let (mut key, mut inclusive) = if let Some(hint) = self
            .extent_hint
            .filter(|hint| hint.file == file && hint.start <= offset && offset < hint.end)
        {
            let end = read_end.min(hint.end);
            self.copy_payload(
                hint.at,
                u64::from(hint.skip) + (offset - hint.start),
                u64::from(hint.skip) + u64::from(hint.len),
                &mut buf[..(end - offset) as usize],
            )
            .await?;
            if end == read_end {
                return Ok(want);
            }
            (Key::extent(file, hint.end), false)
        } else {
            (Key::extent(file, offset.checked_add(1).ok_or(FsError::Corrupt)?), true)
        };
        loop {
            let next = self.tree.next(&mut self.volume, root, &key, inclusive).await?;
            let Some((found, value)) = next else { break };
            if !found.is_extent(file) {
                break;
            }
            key = found;
            inclusive = false;
            let (at, skip, len) = value.as_extent();
            if len == 0 {
                continue;
            }
            let held_end = found.end();
            let held = held_end.checked_sub(u64::from(len)).ok_or(FsError::Corrupt)?;
            self.extent_hint = Some(ExtentHint { file, start: held, end: held_end, at, skip, len });
            if held >= read_end {
                break;
            }
            let start = offset.max(held);
            let end = read_end.min(held_end);
            if start < end {
                let target = (start - offset) as usize;
                self.copy_payload(
                    at,
                    u64::from(skip) + (start - held),
                    u64::from(skip) + u64::from(len),
                    &mut buf[target..target + (end - start) as usize],
                )
                .await?;
            }
        }
        Ok(want)
    }

    /// Creates and opens one empty object below `parent`.
    pub async fn create(&mut self, parent: u32, name: Name, kind: Kind) -> Result<u32, FsError> {
        let mut parent_object = self.object(parent).await?;
        if parent_object.kind != Kind::Dir {
            return Err(FsError::Kind);
        }
        match self.lookup(parent, &name).await {
            Ok(_) => return Err(FsError::Exists),
            Err(FsError::Missing) => {}
            Err(error) => return Err(error),
        }
        let object = self.next_object;
        let next = object.checked_add(1).ok_or(FsError::Full)?;
        parent_object.count = parent_object.count.checked_add(1).ok_or(FsError::Full)?;
        let before = self.snapshot(0).await?;
        let linked = async {
            self.index(Key::object(parent), Value::object(parent_object)).await?;
            let empty = Object { kind, count: 0, size: 0 };
            self.index(Key::object(object), Value::object(empty)).await?;
            self.index(Key::dirent(parent, &name), Value::dirent(object)).await
        }
        .await;
        if let Err(error) = linked {
            self.transaction = Some(before);
            return Err(error);
        }
        self.next_object = next;
        Ok(object)
    }

    /// Appends a file write and returns the number of accepted bytes.
    pub async fn write(&mut self, file: u32, offset: u64, bytes: &[u8]) -> Result<usize, FsError> {
        let mut object = self.object(file).await?;
        if object.kind != Kind::File {
            return Err(FsError::Kind);
        }
        let end = offset.checked_add(bytes.len() as u64).ok_or(FsError::Range)?;
        if bytes.is_empty() {
            return Ok(0);
        }
        self.extent_hint = None;
        let record = Record::write(file, offset, bytes.len())?;
        let before = self.snapshot(record.span()?).await?;
        let cursor = match self.append(record, bytes).await {
            Ok(cursor) => cursor,
            Err(error) => {
                self.transaction = Some(before);
                return Err(error);
            }
        };
        object.size = object.size.max(end);
        let indexed = async {
            self.index(Key::object(file), Value::object(object)).await?;
            self.trim(file, offset, end).await?;
            self.index(Key::extent(file, end), Value::extent(cursor, 0, bytes.len() as u32)).await
        }
        .await;
        if let Err(error) = indexed {
            self.transaction = Some(before);
            return Err(error);
        }
        Ok(bytes.len())
    }

    /// Cuts `[offset, end)` out of the extents already covering it.
    ///
    /// What a read relies on is that extents never overlap, and keeping that
    /// true is the write's job. A piece left to the right of the new range
    /// keeps its key, a piece left to the left takes a new one, and an extent
    /// swallowed whole leaves a whiteout unless the new key is its own.
    async fn trim(&mut self, file: u32, offset: u64, end: u64) -> Result<(), FsError> {
        let mut key = Key::extent(file, offset.checked_add(1).ok_or(FsError::Range)?);
        let mut inclusive = true;
        loop {
            let root = self.tree_root();
            let next = self.tree.next(&mut self.volume, root, &key, inclusive).await?;
            let Some((found, value)) = next else { break };
            if !found.is_extent(file) {
                break;
            }
            key = found;
            inclusive = false;
            let (at, skip, len) = value.as_extent();
            if len == 0 {
                continue;
            }
            let held_end = found.end();
            let held = held_end.checked_sub(u64::from(len)).ok_or(FsError::Corrupt)?;
            if held >= end {
                break;
            }
            if held < offset {
                let left = (offset - held) as u32;
                self.index(Key::extent(file, offset), Value::extent(at, skip, left)).await?;
            }
            if held_end > end {
                let cut = (end - held) as u32;
                let skip = skip.checked_add(cut).ok_or(FsError::Corrupt)?;
                self.index(found, Value::extent(at, skip, len - cut)).await?;
            } else if held_end != end {
                self.index(found, Value::whiteout()).await?;
            }
        }
        Ok(())
    }

    /// Makes every pending record durable and publishes a new generation.
    pub async fn sync(&mut self) -> Result<u64, FsError> {
        let Some(transaction) = self.transaction.as_ref() else {
            self.volume.flush().await?;
            return Ok(self.volume.generation());
        };
        let (at, bytes, root) = (transaction.at, transaction.bytes, transaction.tree.root);

        let crc = self.volume.log_checksum(Region { at, bytes, crc: 0 }).await?;
        let mut checkpoint = self.volume.checkpoint();
        checkpoint.generation = checkpoint.generation.checked_add(1).ok_or(FsError::Full)?;
        checkpoint.tree_root = root;
        checkpoint.set_region(Area::Log, Region { at, bytes, crc });
        let copy = 1 - self.volume.active_copy();

        // The log must survive before any durable superblock is allowed to
        // name it. The second flush is the commit point.
        self.volume.flush().await?;
        self.volume.write_checkpoint(copy, checkpoint).await?;
        self.volume.flush().await?;
        self.volume.commit(copy, checkpoint).await;
        self.transaction = None;
        Ok(checkpoint.generation)
    }

    async fn validate_log(&mut self) -> Result<(), FsError> {
        let mut cursor = 0;
        while cursor < self.log_region().bytes {
            let record = self.record(cursor).await?;
            if record.object >= self.next_object
                || self.object(record.object).await?.kind != Kind::File
                || record.offset.checked_add(u64::from(record.bytes)).is_none()
            {
                return Err(FsError::Corrupt);
            }
            cursor = cursor
                .checked_add(record.span().map_err(|_| FsError::Corrupt)?)
                .ok_or(FsError::Corrupt)?;
        }
        if cursor != self.log_region().bytes {
            return Err(FsError::Corrupt);
        }
        self.validate_index().await
    }

    async fn validate_index(&mut self) -> Result<(), FsError> {
        let region = self.log_region();
        let root = self.tree_root();
        if region.bytes > 0 && root == 0 {
            return Err(FsError::Corrupt);
        }
        self.validate_extents().await
    }

    async fn validate_objects(&mut self) -> Result<(), FsError> {
        for id in 0..self.next_object {
            let object = self.object(id).await?;
            if object.kind == Kind::File {
                if object.count != 0 {
                    return Err(FsError::Corrupt);
                }
                continue;
            }
            if object.size != 0 {
                return Err(FsError::Corrupt);
            }
            let mut key = Key::dirent_start(id);
            let mut inclusive = true;
            let mut count = 0u32;
            loop {
                let root = self.tree_root();
                let found = self.tree.next(&mut self.volume, root, &key, inclusive).await?;
                let Some((next, value)) = found else { break };
                if next.dirent_parent() != Some(id) {
                    break;
                }
                if value.as_dirent() >= self.next_object {
                    return Err(FsError::Corrupt);
                }
                next.name()?;
                count = count.checked_add(1).ok_or(FsError::Corrupt)?;
                key = next;
                inclusive = false;
            }
            if count != object.count {
                return Err(FsError::Corrupt);
            }
        }
        Ok(())
    }

    /// Checks that extents of each file are ordered, non-overlapping, and point
    /// to valid write records. Whiteouts are skipped.
    async fn validate_extents(&mut self) -> Result<(), FsError> {
        let root = self.tree_root();
        let mut key = Key::extent(0, 0);
        let mut inclusive = true;
        let mut last = None;
        loop {
            let next = self.tree.next(&mut self.volume, root, &key, inclusive).await?;
            let Some((found, value)) = next else { break };
            let Some(object) = found.extent_object() else { break };
            key = found;
            inclusive = false;
            let (at, skip, len) = value.as_extent();
            if len == 0 {
                continue;
            }
            let end = found.end();
            let start = end.checked_sub(u64::from(len)).ok_or(FsError::Corrupt)?;
            if last.is_some_and(|(held, held_end)| held == object && held_end > start) {
                return Err(FsError::Corrupt);
            }
            let record = self.record(at).await?;
            if record.object != object
                || record.offset.checked_add(u64::from(skip)) != Some(start)
                || u64::from(skip) + u64::from(len) > u64::from(record.bytes)
            {
                return Err(FsError::Corrupt);
            }
            last = Some((object, end));
        }
        Ok(())
    }

    /// Opens a transaction with room for `reserve`, reclaiming only when full.
    async fn begin(&mut self, reserve: u64) -> Result<(), FsError> {
        let checkpoint = self.volume.checkpoint();
        let capacity =
            u64::from(checkpoint.log_blocks).checked_mul(BLOCK as u64).ok_or(FsError::Corrupt)?;
        if reserve > capacity {
            return Err(FsError::Full);
        }

        if let Some(transaction) = self.transaction.as_ref() {
            if transaction.bytes.checked_add(reserve).is_some_and(|end| end <= capacity) {
                return Ok(());
            }
            let before = transaction.try_clone()?;
            let source = Region { at: transaction.at, bytes: transaction.bytes, crc: 0 };
            let root = transaction.tree.root;
            let target = self.free_bank(source.at)?;
            self.forget_bank(target);
            let transaction = self.transaction.as_mut().ok_or(FsError::Corrupt)?;
            transaction.at = target;
            transaction.bytes = 0;
            if let Err(error) = self.compact(source, root).await {
                self.transaction = Some(before);
                return Err(error);
            }
            if self
                .transaction
                .as_ref()
                .and_then(|transaction| transaction.bytes.checked_add(reserve))
                .is_some_and(|end| end <= capacity)
            {
                return Ok(());
            }
            self.transaction = Some(before);
            return Err(FsError::Full);
        }

        let active = checkpoint.region(Area::Log);
        let tree = self.tree.begin(&mut self.volume).await?;
        if active.bytes.checked_add(reserve).is_some_and(|end| end <= capacity) {
            self.transaction = Some(Transaction { at: active.at, bytes: active.bytes, tree });
            return Ok(());
        }

        let target = self.free_bank(active.at)?;
        self.forget_bank(target);
        self.transaction = Some(Transaction { at: target, bytes: 0, tree });
        if let Err(error) = self.compact(active, checkpoint.tree_root).await {
            self.transaction = None;
            return Err(error);
        }
        if !self
            .transaction
            .as_ref()
            .and_then(|transaction| transaction.bytes.checked_add(reserve))
            .is_some_and(|end| end <= capacity)
        {
            self.transaction = None;
            return Err(FsError::Full);
        }
        Ok(())
    }

    /// Chooses the one bank that no durable root and no compaction source uses.
    fn free_bank(&self, source: u64) -> Result<u64, FsError> {
        let checkpoint = self.volume.checkpoint();
        let active = checkpoint.region(Area::Log).at;
        (0..crate::layout::LOG_BANKS)
            .filter_map(|bank| checkpoint.log_bank(bank).ok())
            .find(|at| *at != source && *at != active && Some(*at) != self.volume.previous_log())
            .ok_or(FsError::Full)
    }

    /// Copies only live extent slices into a fresh bank and retargets their keys.
    async fn compact(&mut self, source: Region, root: u64) -> Result<(), FsError> {
        let mut key = Key::extent(0, 0);
        let mut inclusive = true;
        loop {
            let next = self.tree.next(&mut self.volume, root, &key, inclusive).await?;
            let Some((found, value)) = next else { break };
            let Some(object) = found.extent_object() else { break };
            key = found;
            inclusive = false;
            let (cursor, skip, len) = value.as_extent();
            if len == 0 {
                continue;
            }
            let end = found.end();
            let start = end.checked_sub(u64::from(len)).ok_or(FsError::Corrupt)?;
            let source_record = self.record_in(source, cursor).await?;
            if source_record.object != object
                || source_record.offset.checked_add(u64::from(skip)) != Some(start)
                || u64::from(skip) + u64::from(len) > u64::from(source_record.bytes)
            {
                return Err(FsError::Corrupt);
            }
            self.verify_range(source, cursor, u64::from(skip), u64::from(len)).await?;
            let payload = cursor
                .checked_add(source_record.payload_at().map_err(|_| FsError::Corrupt)?)
                .and_then(|at| at.checked_add(u64::from(skip)))
                .ok_or(FsError::Corrupt)?;
            let record = Record::checked(object, start, len)?;
            let at = self.append_from(source, payload, record).await?;
            self.index(found, Value::extent(at, 0, len)).await?;
        }
        Ok(())
    }

    async fn append(&mut self, record: Record, payload: &[u8]) -> Result<u64, FsError> {
        if payload.len() != record.payload() as usize {
            return Err(FsError::Corrupt);
        }
        let (at, cursor) = self.bank()?;
        self.forget_record(at, cursor);
        let span = record.span()?;
        let end = cursor.checked_add(span).ok_or(FsError::Full)?;
        let capacity = u64::from(self.volume.checkpoint().log_blocks)
            .checked_mul(BLOCK as u64)
            .ok_or(FsError::Corrupt)?;
        if end > capacity {
            return Err(FsError::Full);
        }

        let mut header = [0; HEADER];
        record.encode(&mut header);
        let payload_start = record.payload_at()?;
        let mut written = 0;
        while written < span {
            let mut sector = [0u8; SECTOR];
            if written == 0 {
                sector[..HEADER].copy_from_slice(&header);
            }
            let sector_end = written + SECTOR as u64;
            let checksums_start = written.max(HEADER as u64);
            let checksums_end = sector_end.min(payload_start);
            if checksums_start < checksums_end {
                let first = (checksums_start - HEADER as u64) / 4;
                let last = (checksums_end - HEADER as u64) / 4;
                for chunk in first..last {
                    let from = chunk as usize * BLOCK;
                    let to = (from + BLOCK).min(payload.len());
                    let checksum = crc32c(&payload[from..to]).to_le_bytes();
                    let target = (HEADER as u64 + chunk * 4 - written) as usize;
                    sector[target..target + 4].copy_from_slice(&checksum);
                }
            }
            let payload_end = payload_start + payload.len() as u64;
            let start = written.max(payload_start);
            let end = sector_end.min(payload_end);
            if start < end {
                let source = (start - payload_start) as usize;
                let target = (start - written) as usize;
                sector[target..target + (end - start) as usize]
                    .copy_from_slice(&payload[source..source + (end - start) as usize]);
            }
            self.volume.write_aligned(at, cursor + written, &sector).await?;
            written += ALIGN;
        }
        self.open()?.bytes = end;
        for (chunk, bytes) in payload.chunks(BLOCK).enumerate() {
            self.mark_verified(at, cursor, chunk as u32, crc32c(bytes));
        }
        Ok(cursor)
    }

    /// Appends one record while streaming its payload from another log bank.
    async fn append_from(
        &mut self,
        source: Region,
        source_at: u64,
        record: Record,
    ) -> Result<u64, FsError> {
        let (at, cursor) = self.bank()?;
        self.forget_record(at, cursor);
        let span = record.span()?;
        let end = cursor.checked_add(span).ok_or(FsError::Full)?;
        let capacity = u64::from(self.volume.checkpoint().log_blocks)
            .checked_mul(BLOCK as u64)
            .ok_or(FsError::Corrupt)?;
        let source_end =
            source_at.checked_add(u64::from(record.payload())).ok_or(FsError::Corrupt)?;
        if end > capacity {
            return Err(FsError::Full);
        }
        if source_end > source.bytes {
            return Err(FsError::Corrupt);
        }

        let mut header = [0; HEADER];
        record.encode(&mut header);
        let payload_start = record.payload_at()?;
        let mut written = 0;
        while written < span {
            let mut sector = [0u8; SECTOR];
            if written == 0 {
                sector[..HEADER].copy_from_slice(&header);
            }
            let sector_end = written + SECTOR as u64;
            let checksums_start = written.max(HEADER as u64);
            let checksums_end = sector_end.min(payload_start);
            if checksums_start < checksums_end {
                let first = (checksums_start - HEADER as u64) / 4;
                let last = (checksums_end - HEADER as u64) / 4;
                for chunk in first..last {
                    let (offset, bytes) = record.chunk(chunk as u32)?;
                    let at = source_at.checked_add(offset).ok_or(FsError::Corrupt)?;
                    let crc = self.region_checksum(source, at, u64::from(bytes)).await?;
                    let target = (HEADER as u64 + chunk * 4 - written) as usize;
                    sector[target..target + 4].copy_from_slice(&crc.to_le_bytes());
                }
            }
            let payload_end = payload_start + u64::from(record.payload());
            let start = written.max(payload_start);
            let finish = sector_end.min(payload_end);
            if start < finish {
                let target = (start - written) as usize;
                let from = source_at + start - payload_start;
                self.copy_region(
                    source,
                    from,
                    &mut sector[target..target + (finish - start) as usize],
                )
                .await?;
            }
            self.volume.write_aligned(at, cursor + written, &sector).await?;
            written += ALIGN;
        }
        self.open()?.bytes = end;
        Ok(cursor)
    }

    async fn copy_region(
        &mut self,
        region: Region,
        mut source: u64,
        target: &mut [u8],
    ) -> Result<(), FsError> {
        let end = source.checked_add(target.len() as u64).ok_or(FsError::Corrupt)?;
        if end > region.bytes {
            return Err(FsError::Corrupt);
        }
        let mut done = 0;
        while done < target.len() {
            let within = (source % BLOCK as u64) as usize;
            let take = (target.len() - done).min(BLOCK - within);
            let block = self.volume.block(region.at + source / BLOCK as u64).await?;
            target[done..done + take].copy_from_slice(&block[within..within + take]);
            done += take;
            source += take as u64;
        }
        Ok(())
    }

    async fn region_checksum(
        &mut self,
        region: Region,
        mut source: u64,
        bytes: u64,
    ) -> Result<u32, FsError> {
        let end = source.checked_add(bytes).ok_or(FsError::Corrupt)?;
        if end > region.bytes {
            return Err(FsError::Corrupt);
        }
        let mut crc = Crc::new();
        while source < end {
            let within = (source % BLOCK as u64) as usize;
            let take = (end - source).min((BLOCK - within) as u64) as usize;
            let block = self.volume.block(region.at + source / BLOCK as u64).await?;
            crc.update(&block[within..within + take]);
            source += take as u64;
        }
        Ok(crc.finish())
    }

    async fn index(&mut self, key: Key, value: Value) -> Result<(), FsError> {
        // Taken out and put back: the tree needs the volume the journal owns,
        // and a failed insert still leaves a transaction its caller rolls back.
        let mut transaction = self.transaction.take().ok_or(FsError::Corrupt)?;
        let inserted = self.tree.insert(&mut self.volume, &mut transaction.tree, &key, value).await;
        self.transaction = Some(transaction);
        inserted
    }

    /// Opens a transaction and copies it aside for rollback.
    ///
    /// A mutation is several appends and inserts; if a later one fails, the
    /// caller puts this copy back so the transaction keeps only what it had
    /// before. Blocks the abandoned half wrote stay unreferenced in the arena
    /// until the next generation reuses them.
    async fn snapshot(&mut self, reserve: u64) -> Result<Transaction, FsError> {
        self.begin(reserve).await?;
        self.transaction.as_ref().ok_or(FsError::Corrupt)?.try_clone()
    }

    /// The open transaction, or [`FsError::Corrupt`] if there is none.
    fn open(&mut self) -> Result<&mut Transaction, FsError> {
        self.transaction.as_mut().ok_or(FsError::Corrupt)
    }

    /// Where the pending log bank sits and how far it is filled.
    fn bank(&self) -> Result<(u64, u64), FsError> {
        let transaction = self.transaction.as_ref().ok_or(FsError::Corrupt)?;
        Ok((transaction.at, transaction.bytes))
    }

    fn tree_root(&self) -> u64 {
        self.transaction
            .as_ref()
            .map_or(self.volume.checkpoint().tree_root, |transaction| transaction.tree.root)
    }

    fn log_region(&self) -> Region {
        match self.transaction.as_ref() {
            Some(transaction) => Region { at: transaction.at, bytes: transaction.bytes, crc: 0 },
            None => self.volume.checkpoint().region(Area::Log),
        }
    }

    async fn record(&mut self, cursor: u64) -> Result<Record, FsError> {
        let log = self.log_region();
        self.record_in(log, cursor).await
    }

    async fn record_in(&mut self, log: Region, cursor: u64) -> Result<Record, FsError> {
        if cursor % ALIGN != 0
            || cursor.checked_add(HEADER as u64).ok_or(FsError::Corrupt)? > log.bytes
        {
            return Err(FsError::Corrupt);
        }
        let within = (cursor % BLOCK as u64) as usize;
        let block = self.volume.block(log.at + cursor / BLOCK as u64).await?;
        let record = Record::parse(&block[within..within + HEADER])?;
        let end = cursor
            .checked_add(record.span().map_err(|_| FsError::Corrupt)?)
            .ok_or(FsError::Corrupt)?;
        if end > log.bytes {
            return Err(FsError::Corrupt);
        }
        Ok(record)
    }

    async fn verify_range(
        &mut self,
        log: Region,
        cursor: u64,
        offset: u64,
        bytes: u64,
    ) -> Result<(), FsError> {
        let record = self.record_in(log, cursor).await?;
        let end = offset.checked_add(bytes).ok_or(FsError::Corrupt)?;
        if bytes == 0 || end > u64::from(record.bytes) {
            return Err(FsError::Corrupt);
        }
        let first = offset / BLOCK as u64;
        let last = (end - 1) / BLOCK as u64;
        for chunk in first..=last {
            self.verify_chunk(log, cursor, record, chunk as u32).await?;
        }
        Ok(())
    }

    async fn verify_chunk(
        &mut self,
        log: Region,
        cursor: u64,
        record: Record,
        chunk: u32,
    ) -> Result<(), FsError> {
        let checksum = cursor
            .checked_add(record.checksum_at(chunk).map_err(|_| FsError::Corrupt)?)
            .ok_or(FsError::Corrupt)?;
        let mut encoded = [0; 4];
        self.copy_region(log, checksum, &mut encoded).await?;
        let crc = u32::from_le_bytes(encoded);
        if self.verified.iter().flatten().any(|verified| {
            verified.bank == log.at
                && verified.cursor == cursor
                && verified.chunk == chunk
                && verified.crc == crc
        }) {
            return Ok(());
        }
        let (offset, bytes) = record.chunk(chunk).map_err(|_| FsError::Corrupt)?;
        let payload = cursor
            .checked_add(record.payload_at().map_err(|_| FsError::Corrupt)?)
            .and_then(|at| at.checked_add(offset))
            .ok_or(FsError::Corrupt)?;
        if self.region_checksum(log, payload, u64::from(bytes)).await? != crc {
            return Err(FsError::Checksum);
        }
        self.mark_verified(log.at, cursor, chunk, crc);
        Ok(())
    }

    fn mark_verified(&mut self, bank: u64, cursor: u64, chunk: u32, crc: u32) {
        if self.verified.iter().flatten().any(|verified| {
            verified.bank == bank
                && verified.cursor == cursor
                && verified.chunk == chunk
                && verified.crc == crc
        }) {
            return;
        }
        self.verified[self.verify_hand] = Some(VerifiedRecord { bank, cursor, chunk, crc });
        self.verify_hand = (self.verify_hand + 1) % VERIFIED;
    }

    fn forget_record(&mut self, bank: u64, cursor: u64) {
        for verified in &mut self.verified {
            if verified.is_some_and(|record| record.bank == bank && record.cursor == cursor) {
                *verified = None;
            }
        }
    }

    fn forget_bank(&mut self, bank: u64) {
        for verified in &mut self.verified {
            if verified.is_some_and(|record| record.bank == bank) {
                *verified = None;
            }
        }
    }

    async fn copy_payload(
        &mut self,
        cursor: u64,
        payload_offset: u64,
        payload_end: u64,
        target: &mut [u8],
    ) -> Result<(), FsError> {
        let log = self.log_region();
        let record = self.record_in(log, cursor).await?;
        let read_end = payload_offset.checked_add(target.len() as u64).ok_or(FsError::Corrupt)?;
        if read_end > payload_end || payload_end > u64::from(record.bytes) {
            return Err(FsError::Corrupt);
        }
        let payload_at = record.payload_at().map_err(|_| FsError::Corrupt)?;
        let mut source = cursor
            .checked_add(payload_at)
            .and_then(|at| at.checked_add(payload_offset))
            .ok_or(FsError::Corrupt)?;
        let end = source.checked_add(target.len() as u64).ok_or(FsError::Corrupt)?;
        let ahead_end = cursor
            .checked_add(payload_at)
            .and_then(|at| at.checked_add(payload_end))
            .ok_or(FsError::Corrupt)?;
        if end > ahead_end || ahead_end > log.bytes {
            return Err(FsError::Corrupt);
        }
        let first = source / BLOCK as u64;
        self.volume.prefetch(log.at + first).await?;
        for step in 1..=AHEAD {
            let next = first + step;
            if next * BLOCK as u64 >= ahead_end {
                break;
            }
            self.volume.prefetch(log.at + next).await?;
        }
        self.verify_range(log, cursor, payload_offset, target.len() as u64).await?;
        let mut done = 0;
        while done < target.len() {
            let within = (source % BLOCK as u64) as usize;
            let take = (target.len() - done).min(BLOCK - within);
            let block = log.at + source / BLOCK as u64;
            self.volume.prefetch(block).await?;
            for step in 1..=AHEAD {
                let next = source / BLOCK as u64 + step;
                if next * BLOCK as u64 >= ahead_end {
                    break;
                }
                self.volume.prefetch(log.at + next).await?;
            }
            let block = self.volume.block(block).await?;
            target[done..done + take].copy_from_slice(&block[within..within + take]);
            done += take;
            source += take as u64;
        }
        Ok(())
    }
}

const _: () = assert!(ALIGN == SECTOR as u64);
const _: () = assert!(BLOCK % SECTOR == 0);

#[cfg(all(test, feature = "format"))]
mod tests {
    use molt_block::{Backing, BlockError, Disk, Fault, Loopback, Serial};

    use super::Journal;
    use crate::format::{Tree, build, build_with_log};
    use crate::layout::{Area, Super};
    use crate::log::{ALIGN, Record};
    use crate::volume::DEPTH;
    use crate::{BLOCK, FsError, Kind, Name, attach};

    fn name(text: &str) -> Name {
        Name::try_from(text).unwrap()
    }

    fn image() -> alloc::vec::Vec<u8> {
        let mut tree = Tree::new();
        tree.file("base", b"checkpoint".to_vec()).unwrap();
        build(&tree, 1).unwrap()
    }

    fn mount<D: Disk>(device: D) -> Result<(Journal, Backing<Serial<D>, DEPTH>), FsError> {
        let (blocks, mut backing) = attach(Serial::new(device))?;
        let journal = backing.run(Journal::mount(blocks))?;
        Ok((journal, backing))
    }

    fn commit_file(bytes: &mut [u8], file: &str, contents: &[u8]) -> u64 {
        let (mut journal, mut backing) = mount(Loopback::write(bytes).unwrap()).unwrap();
        let root = journal.root();
        backing
            .run(async {
                let object = journal.create(root, name(file), Kind::File).await?;
                journal.write(object, 0, contents).await?;
                journal.sync().await
            })
            .unwrap()
    }

    fn newest(bytes: &[u8]) -> Result<Super, FsError> {
        let left = Super::parse(&bytes[..BLOCK])?;
        let right = Super::parse(&bytes[BLOCK..2 * BLOCK])?;
        Ok(if left.generation >= right.generation { left } else { right })
    }

    fn assert_checkpoint(bytes: &[u8], generation: u64) {
        let (mut journal, mut backing) = mount(Loopback::read(bytes).unwrap()).unwrap();
        assert_eq!(journal.generation(), generation);
        let root = journal.root();

        backing.run(async {
            let first = journal.lookup(root, &name("first")).await.unwrap();
            let mut contents = [0; 8];
            assert_eq!(journal.read(first, 0, &mut contents).await, Ok(5));
            assert_eq!(&contents[..5], b"first");

            match generation {
                2 => {
                    assert_eq!(journal.lookup(root, &name("second")).await, Err(FsError::Missing));
                }
                3 => {
                    let second = journal.lookup(root, &name("second")).await.unwrap();
                    assert_eq!(journal.read(second, 0, &mut contents).await, Ok(6));
                    assert_eq!(&contents[..6], b"second");
                }
                _ => panic!("unexpected generation {generation}"),
            }
        });
    }

    #[test]
    fn power_loss_mounts_old_or_new() -> Result<(), FsError> {
        let mut baseline = image();
        assert_eq!(commit_file(&mut baseline, "first", b"first"), 2);

        let mut first_success = None;
        for cut in 0..64 {
            let mut stable = baseline.clone();
            let mut volatile = alloc::vec![0; stable.len()];
            let outcome = {
                let device = Fault::new(&mut stable, &mut volatile)?.cut_after(cut);
                let (mut journal, mut backing) = mount(device)?;
                let root = journal.root();
                backing.run(async {
                    let object = journal.create(root, name("second"), Kind::File).await?;
                    journal.write(object, 0, b"second").await?;
                    journal.sync().await
                })
            };

            match outcome {
                Ok(3) => {
                    assert_checkpoint(&stable, 3);
                    first_success = Some(cut);
                    break;
                }
                Err(FsError::Device(BlockError::PowerLoss)) => {
                    assert_checkpoint(&stable, 2);
                }
                other => panic!("cut {cut} produced {other:?}"),
            }
        }

        assert_eq!(
            first_success,
            Some(9),
            "payload, COW paths, log/tree flush, root swing, checkpoint flush"
        );
        Ok(())
    }

    #[test]
    fn bad_log_falls_back() -> Result<(), FsError> {
        let mut bytes = image();
        assert_eq!(commit_file(&mut bytes, "first", b"first"), 2);

        let active = crate::layout::Super::parse(&bytes[BLOCK..2 * BLOCK])?;
        let log = active.region(crate::layout::Area::Log);
        bytes[log.at as usize * BLOCK + (log.bytes - ALIGN) as usize] ^= 1;
        let (mut journal, mut backing) = mount(Loopback::read(&bytes)?)?;
        let root = journal.root();

        assert_eq!(journal.generation(), 1);
        assert_eq!(backing.run(journal.lookup(root, &name("first"))), Err(FsError::Missing));
        Ok(())
    }

    #[test]
    fn small_checkpoint_appends_in_active_bank() -> Result<(), FsError> {
        let mut bytes = image();
        let initial = newest(&bytes)?.region(Area::Log);
        {
            let (mut journal, mut backing) = mount(Loopback::write(&mut bytes)?)?;
            let root = journal.root();
            backing.run(async {
                let file = journal.create(root, name("next"), Kind::File).await?;
                journal.write(file, 0, b"next").await?;
                journal.sync().await
            })?;
        }

        let active = newest(&bytes)?.region(Area::Log);
        assert_eq!(active.at, initial.at);
        assert_eq!(active.bytes, initial.bytes + ALIGN);
        Ok(())
    }

    #[test]
    fn corrupt_payload_is_refused_when_read() -> Result<(), FsError> {
        let mut bytes = image();
        let log = newest(&bytes)?.region(Area::Log);
        let record = Record::parse(&bytes[log.at as usize * BLOCK..])?;
        bytes[log.at as usize * BLOCK + record.payload_at()? as usize] ^= 1;
        let (mut journal, mut backing) = mount(Loopback::read(&bytes)?)?;
        let root = journal.root();
        let mut contents = [0; 16];

        let read = backing.run(async {
            let file = journal.lookup(root, &name("base")).await?;
            journal.read(file, 0, &mut contents).await
        });

        assert_eq!(read, Err(FsError::Checksum));
        Ok(())
    }

    #[test]
    fn bad_tree_falls_back() -> Result<(), FsError> {
        let mut bytes = image();
        assert_eq!(commit_file(&mut bytes, "first", b"first"), 2);
        assert_eq!(commit_file(&mut bytes, "second", b"second"), 3);

        let left = crate::layout::Super::parse(&bytes[..BLOCK])?;
        let right = crate::layout::Super::parse(&bytes[BLOCK..2 * BLOCK])?;
        let active = if left.generation > right.generation { left } else { right };
        bytes[active.tree_root as usize * BLOCK] ^= 1;

        assert_checkpoint(&bytes, 2);
        Ok(())
    }

    #[test]
    fn arena_reuses_old_generations() -> Result<(), FsError> {
        let mut bytes = image();
        {
            let (mut journal, mut backing) = mount(Loopback::write(&mut bytes)?)?;
            let root = journal.root();
            backing.run(async {
                let file = journal.create(root, name("rolling"), Kind::File).await?;
                for byte in 0..200u8 {
                    journal.write(file, 0, &[byte]).await?;
                    journal.sync().await?;
                }
                Ok::<_, FsError>(())
            })?;
        }
        let (mut journal, mut backing) = mount(Loopback::read(&bytes)?)?;
        let root = journal.root();
        let mut byte = [0];

        backing.run(async {
            let file = journal.lookup(root, &name("rolling")).await?;
            assert_eq!(journal.read(file, 0, &mut byte).await, Ok(1));
            Ok::<_, FsError>(())
        })?;
        assert_eq!(byte, [199]);
        Ok(())
    }

    #[test]
    fn writes_overlay_checkpoint_and_extend() -> Result<(), FsError> {
        let mut bytes = image();
        let (mut journal, mut backing) = mount(Loopback::write(&mut bytes)?)?;
        let root = journal.root();
        let mut contents = [0xa5; 20];

        let read = backing.run(async {
            let base = journal.lookup(root, &name("base")).await?;
            journal.write(base, 2, b"WRITE").await?;
            journal.write(base, 12, b"tail").await?;
            journal.read(base, 0, &mut contents).await
        })?;

        assert_eq!(read, 16);
        assert_eq!(&contents[..16], b"chWRITEint\0\0tail");
        Ok(())
    }

    #[test]
    fn checkpoints_reclaim_stale_payloads() -> Result<(), FsError> {
        let mut bytes = build_with_log(&Tree::new(), 1, 4)?;
        let (mut journal, mut backing) = mount(Loopback::write(&mut bytes)?)?;
        let root = journal.root();
        let mut read = [0];

        backing.run(async {
            let file = journal.create(root, name("hot"), Kind::File).await?;
            for byte in 0..64u8 {
                journal.write(file, 0, &[byte]).await?;
                journal.sync().await?;
            }
            journal.remount().await?;
            let file = journal.lookup(root, &name("hot")).await?;
            journal.read(file, 0, &mut read).await
        })?;

        assert_eq!(read, [63]);
        Ok(())
    }

    #[test]
    fn power_loss_during_reclaim_keeps_checkpoint() -> Result<(), FsError> {
        let mut baseline = build_with_log(&Tree::new(), 1, 4)?;
        // Thirty-two sector-aligned writes exactly fill this 16 KiB bank, so
        // the attempted checkpoint must compact into the third bank.
        let old = 31u8;
        {
            let (mut journal, mut backing) = mount(Loopback::write(&mut baseline)?)?;
            let root = journal.root();
            backing.run(async {
                let file = journal.create(root, name("hot"), Kind::File).await?;
                for byte in 0..=old {
                    journal.write(file, 0, &[byte]).await?;
                    journal.sync().await?;
                }
                Ok::<_, FsError>(())
            })?;
        }

        let mut succeeded = false;
        for cut in 0..64 {
            let mut stable = baseline.clone();
            let mut volatile = alloc::vec![0; stable.len()];
            let outcome = {
                let device = Fault::new(&mut stable, &mut volatile)?.cut_after(cut);
                let (mut journal, mut backing) = mount(device)?;
                let root = journal.root();
                backing.run(async {
                    let file = journal.lookup(root, &name("hot")).await?;
                    journal.write(file, 0, &[99]).await?;
                    journal.sync().await
                })
            };

            let (mut journal, mut backing) = mount(Loopback::read(&stable)?)?;
            let root = journal.root();
            let mut byte = [0];
            backing.run(async {
                let file = journal.lookup(root, &name("hot")).await?;
                journal.read(file, 0, &mut byte).await
            })?;
            match outcome {
                Ok(_) => {
                    assert_eq!(byte, [99]);
                    succeeded = true;
                    break;
                }
                Err(FsError::Device(BlockError::PowerLoss)) => assert_eq!(byte, [old]),
                other => panic!("cut {cut} produced {other:?}"),
            }
        }

        assert!(succeeded);
        Ok(())
    }

    #[test]
    fn write_splits_what_it_lands_in() -> Result<(), FsError> {
        let mut bytes = image();
        let (mut journal, mut backing) = mount(Loopback::write(&mut bytes)?)?;
        let root = journal.root();
        let mut contents = [0; 16];

        let read = backing.run(async {
            let file = journal.create(root, name("split"), Kind::File).await?;
            journal.write(file, 0, b"aaaaaaaaaaaaaaaa").await?;
            journal.write(file, 4, b"BBBB").await?;
            journal.write(file, 2, b"cc").await?;
            journal.read(file, 0, &mut contents).await
        })?;

        assert_eq!(read, 16);
        assert_eq!(&contents, b"aaccBBBBaaaaaaaa");
        Ok(())
    }

    #[test]
    fn cover_hides_what_it_swallowed() -> Result<(), FsError> {
        let mut bytes = image();
        let (mut journal, mut backing) = mount(Loopback::write(&mut bytes)?)?;
        let root = journal.root();
        let mut contents = [0; 4];

        // The whiteout the second write leaves at four must not end the walk
        // before the extent covering it.
        let read = backing.run(async {
            let file = journal.create(root, name("cover"), Kind::File).await?;
            journal.write(file, 0, b"aaaa").await?;
            journal.write(file, 0, b"bbbbbbbb").await?;
            journal.read(file, 0, &mut contents).await
        })?;

        assert_eq!(read, 4);
        assert_eq!(&contents, b"bbbb");
        Ok(())
    }

    #[test]
    fn overwrite_leaves_one_extent() -> Result<(), FsError> {
        let mut bytes = image();
        let (mut journal, mut backing) = mount(Loopback::write(&mut bytes)?)?;
        let root = journal.root();

        let stats = backing.run(async {
            let file = journal.create(root, name("hot"), Kind::File).await?;
            for byte in 0..64u8 {
                journal.write(file, 0, &[byte; 8]).await?;
            }
            journal.tree_stats().await
        })?;

        // Root object, file object, dirent, one extent: a single leaf. Keyed
        // on the log cursor instead, the same writes left sixty-four keys.
        assert_eq!(stats.nodes, 1);
        Ok(())
    }
}
