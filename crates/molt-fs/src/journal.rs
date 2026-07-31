//! Writable view of an immutable image plus its COW tree and payload log.
//!
//! A transaction copies the active log into a third bank, appends typed records,
//! and path-copies metadata nodes. [`Journal::sync`] flushes those writes before
//! it publishes their log and tree root, then flushes the superblock. Blocks
//! reachable from the newest and previous checkpoints are never overwritten.

use molt_block::SECTOR;

use crate::btree::{Key, MetadataTree, TreeStats, TreeTransaction, Value};
use crate::layout::{Area, BLOCK, Kind, OBJECT_BYTES, Object, Region};
use crate::log::{ALIGN, HEADER, Record};
use crate::volume::Blocks;
use crate::{FsError, Name, Volume};

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
    base_objects: u32,
    next_object: u32,
}

impl Journal {
    /// Mounts the newest valid checkpoint and replays its mutation log.
    pub async fn mount(blocks: Blocks) -> Result<Self, FsError> {
        let mut journal = Self {
            volume: Volume::mount(blocks).await?,
            transaction: None,
            tree: MetadataTree::new()?,
            base_objects: 0,
            next_object: 0,
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
        self.replay().await
    }

    /// Sizes the object space from the mounted checkpoint and replays its log.
    async fn replay(&mut self) -> Result<(), FsError> {
        let object_bytes = self.volume.checkpoint().region(Area::Objects).bytes;
        if object_bytes % OBJECT_BYTES as u64 != 0 {
            return Err(FsError::Corrupt);
        }
        self.base_objects =
            u32::try_from(object_bytes / OBJECT_BYTES as u64).map_err(|_| FsError::Corrupt)?;
        self.next_object = self.base_objects;
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
        if let Some(value) = self.tree.get(&mut self.volume, root, &Key::object(id)).await? {
            return value.as_object();
        }
        if id < self.base_objects {
            return self.volume.object(id).await;
        }
        Err(FsError::Corrupt)
    }

    /// Finds `name` in a directory, including objects created since mkfs.
    pub async fn lookup(&mut self, dir: u32, name: &Name) -> Result<u32, FsError> {
        let object = self.object(dir).await?;
        if object.kind != Kind::Dir {
            return Err(FsError::Kind);
        }
        if dir < self.base_objects {
            let base = self.volume.object(dir).await?;
            match self.volume.lookup(&base, name.as_bytes()).await {
                Ok(object) => return Ok(object),
                Err(FsError::Missing) => {}
                Err(error) => return Err(error),
            }
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
        let base = match dir < self.base_objects {
            true => Some(self.volume.object(dir).await?),
            false => None,
        };
        let mut previous = None;
        let mut selected = None;

        for _ in 0..=index {
            let mut candidate = None;
            if let Some(base) = base {
                for at in 0..base.count {
                    let (name, object) = self.volume.entry(&base, at).await?;
                    choose(&mut candidate, previous, name, object);
                }
            }
            let root = self.tree_root();
            let key = match previous {
                Some(name) => Key::dirent(dir, &name),
                None => Key::dirent_start(dir),
            };
            let found = self.tree.next(&mut self.volume, root, &key, previous.is_none()).await?;
            if let Some((key, value)) = found
                && key.is_dirent(dir)
            {
                choose(&mut candidate, previous, key.name()?, value.as_dirent());
            }
            selected = candidate;
            previous = Some(selected.ok_or(FsError::Corrupt)?.0);
        }
        selected.ok_or(FsError::Corrupt)
    }

    /// Reads the current file contents, overlaying later writes over earlier
    /// ones and the immutable image.
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

        if file < self.base_objects {
            let base = self.volume.object(file).await?;
            if offset <= base.size {
                let take = (base.size - offset).min(want as u64) as usize;
                self.volume.read(&base, offset, &mut buf[..take]).await?;
            }
        }

        let read_end = offset.checked_add(want as u64).ok_or(FsError::Corrupt)?;
        let root = self.tree_root();
        let mut key = Key::write_start(file);
        let mut inclusive = true;
        loop {
            let next = self.tree.next(&mut self.volume, root, &key, inclusive).await?;
            let Some((found, value)) = next else { break };
            if !found.is_write(file) {
                break;
            }
            let (written_at, bytes) = value.as_write();
            let cursor = found.cursor();
            let written_end = written_at.checked_add(u64::from(bytes)).ok_or(FsError::Corrupt)?;
            let start = offset.max(written_at);
            let end = read_end.min(written_end);
            if start < end {
                let target = (start - offset) as usize;
                self.copy_payload(
                    cursor,
                    start - written_at,
                    &mut buf[target..target + (end - start) as usize],
                )
                .await?;
            }
            key = found;
            inclusive = false;
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
        let before = self.snapshot().await?;
        let linked = async {
            self.append(Record::create(object, parent, kind, name), name.as_bytes()).await?;
            self.index(Key::object(parent), Value::object(parent_object)).await?;
            let empty = Object { kind, start: 0, count: 0, size: 0 };
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
        offset.checked_add(bytes.len() as u64).ok_or(FsError::Range)?;
        if bytes.is_empty() {
            return Ok(0);
        }
        let record = Record::write(file, offset, bytes.len())?;
        let before = self.snapshot().await?;
        let cursor = match self.append(record, bytes).await {
            Ok(cursor) => cursor,
            Err(error) => {
                self.transaction = Some(before);
                return Err(error);
            }
        };
        object.size = object.size.max(offset + bytes.len() as u64);
        let indexed = async {
            self.index(Key::object(file), Value::object(object)).await?;
            self.index(Key::write(file, cursor), Value::write(offset, bytes.len() as u32)).await
        }
        .await;
        if let Err(error) = indexed {
            self.transaction = Some(before);
            return Err(error);
        }
        Ok(bytes.len())
    }

    /// Makes every pending record durable and publishes a new generation.
    pub async fn sync(&mut self) -> Result<u64, FsError> {
        let Some(transaction) = self.transaction.as_ref() else {
            self.volume.flush().await?;
            return Ok(self.volume.generation());
        };
        let (at, bytes, root) = (transaction.at, transaction.bytes, transaction.tree.root);

        let crc = self.volume.checksum(at, bytes).await?;
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
        let mut next = self.base_objects;
        let mut cursor = 0;
        while cursor < self.log_region().bytes {
            let record = self.record(cursor).await?;
            match record {
                Record::Create { object, parent, .. } => {
                    if object != next || parent >= next {
                        return Err(FsError::Corrupt);
                    }
                    let name =
                        self.record_name(cursor, record).await.map_err(|_| FsError::Corrupt)?;
                    if self.kind_before(parent, cursor).await? != Kind::Dir
                        || self.name_exists_before(parent, &name, cursor).await?
                    {
                        return Err(FsError::Corrupt);
                    }
                    next = next.checked_add(1).ok_or(FsError::Corrupt)?;
                }
                Record::Write { object, offset, bytes } => {
                    if object >= next
                        || self.kind_before(object, cursor).await? != Kind::File
                        || offset.checked_add(u64::from(bytes)).is_none()
                    {
                        return Err(FsError::Corrupt);
                    }
                }
            }
            cursor = cursor
                .checked_add(record.span().map_err(|_| FsError::Corrupt)?)
                .ok_or(FsError::Corrupt)?;
        }
        if cursor != self.log_region().bytes {
            return Err(FsError::Corrupt);
        }
        self.next_object = next;
        self.validate_index().await
    }

    async fn validate_index(&mut self) -> Result<(), FsError> {
        let region = self.log_region();
        let root = self.tree_root();
        if region.bytes > 0 && root == 0 {
            return Err(FsError::Corrupt);
        }
        let mut cursor = 0;
        while cursor < region.bytes {
            let record = self.record(cursor).await?;
            match record {
                Record::Create { object, parent, kind, .. } => {
                    let name = self.record_name(cursor, record).await?;
                    let state = self
                        .tree
                        .get(&mut self.volume, root, &Key::object(object))
                        .await?
                        .ok_or(FsError::Corrupt)?
                        .as_object()?;
                    let linked = self
                        .tree
                        .get(&mut self.volume, root, &Key::dirent(parent, &name))
                        .await?
                        .ok_or(FsError::Corrupt)?
                        .as_dirent();
                    if state.kind != kind || linked != object {
                        return Err(FsError::Corrupt);
                    }
                }
                Record::Write { object, offset, bytes } => {
                    let indexed = self
                        .tree
                        .get(&mut self.volume, root, &Key::write(object, cursor))
                        .await?
                        .ok_or(FsError::Corrupt)?
                        .as_write();
                    if indexed != (offset, bytes) {
                        return Err(FsError::Corrupt);
                    }
                }
            }
            cursor += record.span().map_err(|_| FsError::Corrupt)?;
        }
        Ok(())
    }

    async fn kind_before(&mut self, object: u32, limit: u64) -> Result<Kind, FsError> {
        if object < self.base_objects {
            return Ok(self.volume.object(object).await?.kind);
        }
        let mut cursor = 0;
        while cursor < limit {
            let record = self.record(cursor).await?;
            if let Record::Create { object: created, kind, .. } = record
                && created == object
            {
                return Ok(kind);
            }
            cursor += record.span().map_err(|_| FsError::Corrupt)?;
        }
        Err(FsError::Corrupt)
    }

    async fn name_exists_before(
        &mut self,
        parent: u32,
        name: &Name,
        limit: u64,
    ) -> Result<bool, FsError> {
        if parent < self.base_objects {
            let base = self.volume.object(parent).await?;
            match self.volume.lookup(&base, name.as_bytes()).await {
                Ok(_) => return Ok(true),
                Err(FsError::Missing) => {}
                Err(error) => return Err(error),
            }
        }
        let mut cursor = 0;
        while cursor < limit {
            let record = self.record(cursor).await?;
            if let Record::Create { parent: held, .. } = record
                && held == parent
                && self.record_name(cursor, record).await? == *name
            {
                return Ok(true);
            }
            cursor += record.span().map_err(|_| FsError::Corrupt)?;
        }
        Ok(false)
    }

    /// Opens a transaction unless one is already open.
    async fn begin(&mut self) -> Result<(), FsError> {
        if self.transaction.is_some() {
            return Ok(());
        }
        let checkpoint = self.volume.checkpoint();
        let active = checkpoint.region(Area::Log);
        let target = (0..crate::layout::LOG_BANKS)
            .filter_map(|bank| checkpoint.log_bank(bank).ok())
            .find(|at| *at != active.at && Some(*at) != self.volume.previous_log())
            .ok_or(FsError::Corrupt)?;
        let tree = self.tree.begin(&mut self.volume).await?;
        self.volume.copy_aligned(active.at, target, active.bytes).await?;
        self.transaction = Some(Transaction { at: target, bytes: active.bytes, tree });
        Ok(())
    }

    async fn append(&mut self, record: Record, payload: &[u8]) -> Result<u64, FsError> {
        if payload.len() != record.payload() as usize {
            return Err(FsError::Corrupt);
        }
        self.begin().await?;
        let (at, cursor) = self.bank()?;
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
        let mut written = 0;
        while written < span {
            let mut sector = [0u8; SECTOR];
            if written == 0 {
                sector[..HEADER].copy_from_slice(&header);
            }
            let sector_end = written + SECTOR as u64;
            let payload_start = HEADER as u64;
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
        Ok(cursor)
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
    async fn snapshot(&mut self) -> Result<Transaction, FsError> {
        self.begin().await?;
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

    async fn record_name(&mut self, cursor: u64, record: Record) -> Result<Name, FsError> {
        let Record::Create { name_len, .. } = record else {
            return Err(FsError::Corrupt);
        };
        let mut bytes = [0; crate::MAX_NAME];
        self.copy_payload(cursor, 0, &mut bytes[..name_len as usize]).await?;
        Name::new(&bytes[..name_len as usize])
    }

    async fn copy_payload(
        &mut self,
        cursor: u64,
        payload_offset: u64,
        target: &mut [u8],
    ) -> Result<(), FsError> {
        let log = self.log_region();
        let mut source = cursor
            .checked_add(HEADER as u64)
            .and_then(|at| at.checked_add(payload_offset))
            .ok_or(FsError::Corrupt)?;
        let end = source.checked_add(target.len() as u64).ok_or(FsError::Corrupt)?;
        if end > log.bytes {
            return Err(FsError::Corrupt);
        }
        let mut done = 0;
        while done < target.len() {
            let within = (source % BLOCK as u64) as usize;
            let take = (target.len() - done).min(BLOCK - within);
            let block = self.volume.block(log.at + source / BLOCK as u64).await?;
            target[done..done + take].copy_from_slice(&block[within..within + take]);
            done += take;
            source += take as u64;
        }
        Ok(())
    }
}

fn choose(candidate: &mut Option<(Name, u32)>, previous: Option<Name>, name: Name, object: u32) {
    if previous.is_some_and(|previous| name.as_bytes() <= previous.as_bytes()) {
        return;
    }
    if candidate.is_none_or(|(held, _)| name.as_bytes() < held.as_bytes()) {
        *candidate = Some((name, object));
    }
}

const _: () = assert!(ALIGN == SECTOR as u64);
const _: () = assert!(BLOCK % SECTOR == 0);

#[cfg(all(test, feature = "format"))]
mod tests {
    use molt_block::{Backing, BlockError, Disk, Fault, Loopback, Serial};

    use super::Journal;
    use crate::format::{Tree, build};
    use crate::volume::DEPTH;
    use crate::{BLOCK, FsError, Kind, Name, attach};

    fn name(text: &str) -> Name {
        Name::try_from(text).unwrap()
    }

    fn image() -> alloc::vec::Vec<u8> {
        let mut tree = Tree::new();
        tree.file("base", b"immutable".to_vec()).unwrap();
        build(&tree, 1).unwrap()
    }

    fn mount<D: Disk>(device: D) -> Result<(Journal, Backing<Serial<D>, DEPTH>), FsError> {
        let (blocks, mut backing) = attach(Serial::new(device))?;
        let journal = backing.run(Journal::mount(blocks))?;
        Ok((journal, backing))
    }

    fn commit_file(bytes: &mut [u8], file: &str, contents: &[u8]) -> u64 {
        let (mut journal, mut backing) = mount(Loopback::writable(bytes).unwrap()).unwrap();
        let root = journal.root();
        backing
            .run(async {
                let object = journal.create(root, name(file), Kind::File).await?;
                journal.write(object, 0, contents).await?;
                journal.sync().await
            })
            .unwrap()
    }

    fn assert_checkpoint(bytes: &[u8], generation: u64) {
        let (mut journal, mut backing) = mount(Loopback::new(bytes).unwrap()).unwrap();
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
            Some(11),
            "copy, records, COW paths, log/tree flush, root swing, checkpoint flush"
        );
        Ok(())
    }

    #[test]
    fn bad_log_falls_back() -> Result<(), FsError> {
        let mut bytes = image();
        assert_eq!(commit_file(&mut bytes, "first", b"first"), 2);

        let active = crate::layout::Super::parse(&bytes[BLOCK..2 * BLOCK])?;
        let log = active.region(crate::layout::Area::Log);
        bytes[log.at as usize * BLOCK] ^= 1;
        let (mut journal, mut backing) = mount(Loopback::new(&bytes)?)?;
        let root = journal.root();

        assert_eq!(journal.generation(), 1);
        assert_eq!(backing.run(journal.lookup(root, &name("first"))), Err(FsError::Missing));
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
            let (mut journal, mut backing) = mount(Loopback::writable(&mut bytes)?)?;
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
        let (mut journal, mut backing) = mount(Loopback::new(&bytes)?)?;
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
    fn writes_overlay_base_and_extend() -> Result<(), FsError> {
        let mut bytes = image();
        let (mut journal, mut backing) = mount(Loopback::writable(&mut bytes)?)?;
        let root = journal.root();
        let mut contents = [0xa5; 20];

        let read = backing.run(async {
            let base = journal.lookup(root, &name("base")).await?;
            journal.write(base, 2, b"WRITE").await?;
            journal.write(base, 12, b"tail").await?;
            journal.read(base, 0, &mut contents).await
        })?;

        assert_eq!(read, 16);
        assert_eq!(&contents[..16], b"imWRITEle\0\0\0tail");
        Ok(())
    }
}
