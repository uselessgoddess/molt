//! Writable view of an immutable image plus its committed mutation log.
//!
//! A transaction copies the active log into a third bank and appends typed,
//! sector-aligned records there. [`Journal::sync`] flushes that bank before it
//! publishes a new superblock, then flushes the superblock. The newest and
//! previous banks are never overwritten while a checkpoint can name them.

use molt_block::{SECTOR, Writable};

use crate::layout::{Area, BLOCK, Kind, OBJECT_BYTES, Object, Region};
use crate::log::{ALIGN, HEADER, Record};
use crate::{FsError, Name, Volume};

#[derive(Clone, Copy)]
struct Transaction {
    at: u64,
    bytes: u64,
}

/// A mounted writable filesystem.
pub struct Journal<'buf, D> {
    volume: Volume<'buf, D>,
    transaction: Option<Transaction>,
    base_objects: u32,
    next_object: u32,
}

impl<'buf, D: Writable> Journal<'buf, D> {
    /// Mounts the newest valid checkpoint and replays its mutation log.
    pub fn mount(device: D, block: &'buf mut [u8; BLOCK]) -> Result<Self, FsError> {
        let volume = Volume::mount(device, block)?;
        let object_bytes = volume.checkpoint().region(Area::Objects).bytes;
        if object_bytes % OBJECT_BYTES as u64 != 0 {
            return Err(FsError::Corrupt);
        }
        let base_objects =
            u32::try_from(object_bytes / OBJECT_BYTES as u64).map_err(|_| FsError::Corrupt)?;
        let mut journal =
            Self { volume, transaction: None, base_objects, next_object: base_objects };
        journal.validate_log()?;
        Ok(journal)
    }

    /// The object id of the root directory.
    pub const fn root(&self) -> u32 {
        self.volume.root()
    }

    /// The generation of the active durable checkpoint.
    pub const fn generation(&self) -> u64 {
        self.volume.generation()
    }

    /// Returns the current object state after replaying every mutation.
    pub fn object(&mut self, id: u32) -> Result<Object, FsError> {
        if id >= self.next_object {
            return Err(FsError::Missing);
        }
        let mut object = if id < self.base_objects {
            self.volume.object(id)?
        } else {
            let mut found = None;
            let mut cursor = 0;
            while cursor < self.log_region().bytes {
                let record = self.record(cursor)?;
                if let Record::Create { object, kind, .. } = record
                    && object == id
                {
                    found = Some(Object { kind, start: 0, count: 0, size: 0 });
                    break;
                }
                cursor += record.span().map_err(|_| FsError::Corrupt)?;
            }
            found.ok_or(FsError::Corrupt)?
        };

        let mut cursor = 0;
        while cursor < self.log_region().bytes {
            let record = self.record(cursor)?;
            match record {
                Record::Create { parent, .. } if parent == id => {
                    if object.kind != Kind::Dir {
                        return Err(FsError::Corrupt);
                    }
                    object.count = object.count.checked_add(1).ok_or(FsError::Corrupt)?;
                }
                Record::Write { object: written, offset, bytes } if written == id => {
                    if object.kind != Kind::File {
                        return Err(FsError::Corrupt);
                    }
                    let end = offset.checked_add(u64::from(bytes)).ok_or(FsError::Corrupt)?;
                    object.size = object.size.max(end);
                }
                _ => {}
            }
            cursor += record.span().map_err(|_| FsError::Corrupt)?;
        }
        Ok(object)
    }

    /// Finds `name` in a directory, including objects created since mkfs.
    pub fn lookup(&mut self, dir: u32, name: &Name) -> Result<u32, FsError> {
        let object = self.object(dir)?;
        if object.kind != Kind::Dir {
            return Err(FsError::Kind);
        }
        if dir < self.base_objects {
            let base = self.volume.object(dir)?;
            match self.volume.lookup(&base, name.as_bytes()) {
                Ok(object) => return Ok(object),
                Err(FsError::Missing) => {}
                Err(error) => return Err(error),
            }
        }

        let mut cursor = 0;
        while cursor < self.log_region().bytes {
            let record = self.record(cursor)?;
            if let Record::Create { object, parent, .. } = record
                && parent == dir
                && self.record_name(cursor, record)? == *name
            {
                return Ok(object);
            }
            cursor += record.span().map_err(|_| FsError::Corrupt)?;
        }
        Err(FsError::Missing)
    }

    /// Reads `index` in bytewise name order.
    pub fn entry(&mut self, dir: u32, index: u32) -> Result<(Name, u32), FsError> {
        let object = self.object(dir)?;
        if object.kind != Kind::Dir {
            return Err(FsError::Kind);
        }
        if index >= object.count {
            return Err(FsError::Missing);
        }
        let base = if dir < self.base_objects { Some(self.volume.object(dir)?) } else { None };
        let mut previous = None;
        let mut selected = None;

        for _ in 0..=index {
            let mut candidate = None;
            if let Some(base) = base {
                for at in 0..base.count {
                    let (name, object) = self.volume.entry(&base, at)?;
                    choose(&mut candidate, previous, name, object);
                }
            }
            let mut cursor = 0;
            while cursor < self.log_region().bytes {
                let record = self.record(cursor)?;
                if let Record::Create { object, parent, .. } = record
                    && parent == dir
                {
                    let name = self.record_name(cursor, record)?;
                    choose(&mut candidate, previous, name, object);
                }
                cursor += record.span().map_err(|_| FsError::Corrupt)?;
            }
            selected = candidate;
            previous = Some(selected.ok_or(FsError::Corrupt)?.0);
        }
        selected.ok_or(FsError::Corrupt)
    }

    /// Reads the current file contents, overlaying later writes over earlier
    /// ones and the immutable image.
    pub fn read(&mut self, file: u32, offset: u64, buf: &mut [u8]) -> Result<usize, FsError> {
        let object = self.object(file)?;
        if object.kind != Kind::File {
            return Err(FsError::Kind);
        }
        if offset > object.size {
            return Err(FsError::Range);
        }
        let want = (object.size - offset).min(buf.len() as u64) as usize;
        buf[..want].fill(0);

        if file < self.base_objects {
            let base = self.volume.object(file)?;
            if offset <= base.size {
                let take = (base.size - offset).min(want as u64) as usize;
                self.volume.read(&base, offset, &mut buf[..take])?;
            }
        }

        let read_end = offset.checked_add(want as u64).ok_or(FsError::Corrupt)?;
        let mut cursor = 0;
        while cursor < self.log_region().bytes {
            let record = self.record(cursor)?;
            if let Record::Write { object, offset: written_at, bytes } = record
                && object == file
            {
                let written_end =
                    written_at.checked_add(u64::from(bytes)).ok_or(FsError::Corrupt)?;
                let start = offset.max(written_at);
                let end = read_end.min(written_end);
                if start < end {
                    let target = (start - offset) as usize;
                    self.copy_payload(
                        cursor,
                        start - written_at,
                        &mut buf[target..target + (end - start) as usize],
                    )?;
                }
            }
            cursor += record.span().map_err(|_| FsError::Corrupt)?;
        }
        Ok(want)
    }

    /// Creates and opens one empty object below `parent`.
    pub fn create(&mut self, parent: u32, name: Name, kind: Kind) -> Result<u32, FsError> {
        if self.object(parent)?.kind != Kind::Dir {
            return Err(FsError::Kind);
        }
        match self.lookup(parent, &name) {
            Ok(_) => return Err(FsError::Exists),
            Err(FsError::Missing) => {}
            Err(error) => return Err(error),
        }
        let object = self.next_object;
        let next = object.checked_add(1).ok_or(FsError::Full)?;
        self.append(Record::create(object, parent, kind, name), name.as_bytes())?;
        self.next_object = next;
        Ok(object)
    }

    /// Appends a file write and returns the number of accepted bytes.
    pub fn write(&mut self, file: u32, offset: u64, bytes: &[u8]) -> Result<usize, FsError> {
        if self.object(file)?.kind != Kind::File {
            return Err(FsError::Kind);
        }
        offset.checked_add(bytes.len() as u64).ok_or(FsError::Range)?;
        if bytes.is_empty() {
            return Ok(0);
        }
        self.append(Record::write(file, offset, bytes.len())?, bytes)?;
        Ok(bytes.len())
    }

    /// Makes every pending record durable and publishes a new generation.
    pub fn sync(&mut self) -> Result<u64, FsError> {
        let Some(transaction) = self.transaction else {
            self.volume.flush()?;
            return Ok(self.volume.generation());
        };

        let crc = self.volume.checksum(transaction.at, transaction.bytes)?;
        let mut checkpoint = self.volume.checkpoint();
        checkpoint.generation = checkpoint.generation.checked_add(1).ok_or(FsError::Full)?;
        checkpoint
            .set_region(Area::Log, Region { at: transaction.at, bytes: transaction.bytes, crc });
        let copy = 1 - self.volume.active_copy();

        // The log must survive before any durable superblock is allowed to
        // name it. The second flush is the commit point.
        self.volume.flush()?;
        self.volume.write_checkpoint(copy, checkpoint)?;
        self.volume.flush()?;
        self.volume.commit(copy, checkpoint);
        self.transaction = None;
        Ok(checkpoint.generation)
    }

    fn validate_log(&mut self) -> Result<(), FsError> {
        let mut next = self.base_objects;
        let mut cursor = 0;
        while cursor < self.log_region().bytes {
            let record = self.record(cursor)?;
            match record {
                Record::Create { object, parent, .. } => {
                    if object != next || parent >= next {
                        return Err(FsError::Corrupt);
                    }
                    let name = self.record_name(cursor, record).map_err(|_| FsError::Corrupt)?;
                    if self.kind_before(parent, cursor)? != Kind::Dir
                        || self.name_exists_before(parent, &name, cursor)?
                    {
                        return Err(FsError::Corrupt);
                    }
                    next = next.checked_add(1).ok_or(FsError::Corrupt)?;
                }
                Record::Write { object, offset, bytes } => {
                    if object >= next
                        || self.kind_before(object, cursor)? != Kind::File
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
        Ok(())
    }

    fn kind_before(&mut self, object: u32, limit: u64) -> Result<Kind, FsError> {
        if object < self.base_objects {
            return Ok(self.volume.object(object)?.kind);
        }
        let mut cursor = 0;
        while cursor < limit {
            let record = self.record(cursor)?;
            if let Record::Create { object: created, kind, .. } = record
                && created == object
            {
                return Ok(kind);
            }
            cursor += record.span().map_err(|_| FsError::Corrupt)?;
        }
        Err(FsError::Corrupt)
    }

    fn name_exists_before(
        &mut self,
        parent: u32,
        name: &Name,
        limit: u64,
    ) -> Result<bool, FsError> {
        if parent < self.base_objects {
            let base = self.volume.object(parent)?;
            match self.volume.lookup(&base, name.as_bytes()) {
                Ok(_) => return Ok(true),
                Err(FsError::Missing) => {}
                Err(error) => return Err(error),
            }
        }
        let mut cursor = 0;
        while cursor < limit {
            let record = self.record(cursor)?;
            if let Record::Create { parent: held, .. } = record
                && held == parent
                && self.record_name(cursor, record)? == *name
            {
                return Ok(true);
            }
            cursor += record.span().map_err(|_| FsError::Corrupt)?;
        }
        Ok(false)
    }

    fn begin(&mut self) -> Result<Transaction, FsError> {
        if let Some(transaction) = self.transaction {
            return Ok(transaction);
        }
        let checkpoint = self.volume.checkpoint();
        let active = checkpoint.region(Area::Log);
        let target = (0..crate::layout::LOG_BANKS)
            .filter_map(|bank| checkpoint.log_bank(bank).ok())
            .find(|at| *at != active.at && Some(*at) != self.volume.previous_log())
            .ok_or(FsError::Corrupt)?;
        self.volume.copy_aligned(active.at, target, active.bytes)?;
        let transaction = Transaction { at: target, bytes: active.bytes };
        self.transaction = Some(transaction);
        Ok(transaction)
    }

    fn append(&mut self, record: Record, payload: &[u8]) -> Result<(), FsError> {
        if payload.len() != record.payload() as usize {
            return Err(FsError::Corrupt);
        }
        let transaction = self.begin()?;
        let span = record.span()?;
        let end = transaction.bytes.checked_add(span).ok_or(FsError::Full)?;
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
            self.volume.write_aligned(transaction.at, transaction.bytes + written, &sector)?;
            written += ALIGN;
        }
        self.transaction = Some(Transaction { bytes: end, ..transaction });
        Ok(())
    }

    fn log_region(&self) -> Region {
        match self.transaction {
            Some(transaction) => Region { at: transaction.at, bytes: transaction.bytes, crc: 0 },
            None => self.volume.checkpoint().region(Area::Log),
        }
    }

    fn record(&mut self, cursor: u64) -> Result<Record, FsError> {
        let log = self.log_region();
        if cursor % ALIGN != 0
            || cursor.checked_add(HEADER as u64).ok_or(FsError::Corrupt)? > log.bytes
        {
            return Err(FsError::Corrupt);
        }
        let within = (cursor % BLOCK as u64) as usize;
        let block = self.volume.block(log.at + cursor / BLOCK as u64)?;
        let record = Record::parse(&block[within..within + HEADER])?;
        let end = cursor
            .checked_add(record.span().map_err(|_| FsError::Corrupt)?)
            .ok_or(FsError::Corrupt)?;
        if end > log.bytes {
            return Err(FsError::Corrupt);
        }
        Ok(record)
    }

    fn record_name(&mut self, cursor: u64, record: Record) -> Result<Name, FsError> {
        let Record::Create { name_len, .. } = record else {
            return Err(FsError::Corrupt);
        };
        let mut bytes = [0; crate::MAX_NAME];
        self.copy_payload(cursor, 0, &mut bytes[..name_len as usize])?;
        Name::new(&bytes[..name_len as usize])
    }

    fn copy_payload(
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
            let block = self.volume.block(log.at + source / BLOCK as u64)?;
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
    use molt_block::{BlockError, Fault, Loopback};

    use super::Journal;
    use crate::format::{Tree, build};
    use crate::{BLOCK, FsError, Kind, Name};

    fn name(text: &str) -> Name {
        Name::try_from(text).expect("legal name")
    }

    fn image() -> alloc::vec::Vec<u8> {
        let mut tree = Tree::new();
        tree.file("base", b"immutable".to_vec()).expect("legal name");
        build(&tree, 1).expect("image")
    }

    fn commit_file(bytes: &mut [u8], file: &str, contents: &[u8]) -> u64 {
        let mut block = [0; BLOCK];
        let mut journal =
            Journal::mount(Loopback::writable(bytes).expect("writable image"), &mut block)
                .expect("mount");
        let object = journal.create(journal.root(), name(file), Kind::File).expect("create");
        journal.write(object, 0, contents).expect("write");
        journal.sync().expect("sync")
    }

    fn assert_checkpoint(bytes: &[u8], generation: u64) {
        let mut block = [0; BLOCK];
        let mut journal =
            Journal::mount(Loopback::new(bytes).expect("image"), &mut block).expect("mount");
        assert_eq!(journal.generation(), generation);

        let first = journal.lookup(journal.root(), &name("first")).expect("first survives");
        let mut contents = [0; 8];
        assert_eq!(journal.read(first, 0, &mut contents), Ok(5));
        assert_eq!(&contents[..5], b"first");

        match generation {
            2 => assert_eq!(journal.lookup(journal.root(), &name("second")), Err(FsError::Missing)),
            3 => {
                let second =
                    journal.lookup(journal.root(), &name("second")).expect("second committed");
                assert_eq!(journal.read(second, 0, &mut contents), Ok(6));
                assert_eq!(&contents[..6], b"second");
            }
            _ => panic!("unexpected generation {generation}"),
        }
    }

    #[test]
    fn power_loss_at_every_checkpoint_action_mounts_old_or_new_generation() {
        let mut baseline = image();
        assert_eq!(commit_file(&mut baseline, "first", b"first"), 2);

        let mut first_success = None;
        for cut in 0..64 {
            let mut stable = baseline.clone();
            let mut volatile = alloc::vec![0; stable.len()];
            let outcome = {
                let device = Fault::new(&mut stable, &mut volatile)
                    .expect("matching storage")
                    .cut_after(cut);
                let mut block = [0; BLOCK];
                let mut journal =
                    Journal::mount(device, &mut block).expect("old checkpoint mounts");
                (|| {
                    let object = journal.create(journal.root(), name("second"), Kind::File)?;
                    journal.write(object, 0, b"second")?;
                    journal.sync()
                })()
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
            Some(6),
            "copy, create, write, log flush, super write, super flush"
        );
    }

    #[test]
    fn newest_checkpoint_with_bad_log_falls_back_to_previous_generation() {
        let mut bytes = image();
        assert_eq!(commit_file(&mut bytes, "first", b"first"), 2);

        let active = crate::layout::Super::parse(&bytes[BLOCK..2 * BLOCK]).expect("generation two");
        let log = active.region(crate::layout::Area::Log);
        bytes[log.at as usize * BLOCK] ^= 1;

        let mut block = [0; BLOCK];
        let mut journal =
            Journal::mount(Loopback::new(&bytes).expect("image"), &mut block).expect("fallback");

        assert_eq!(journal.generation(), 1);
        assert_eq!(journal.lookup(journal.root(), &name("first")), Err(FsError::Missing));
    }

    #[test]
    fn later_writes_overlay_base_data_and_can_extend_sparsely() {
        let mut bytes = image();
        let mut block = [0; BLOCK];
        let mut journal =
            Journal::mount(Loopback::writable(&mut bytes).expect("image"), &mut block)
                .expect("mount");
        let base = journal.lookup(journal.root(), &name("base")).expect("base file");

        journal.write(base, 2, b"WRITE").expect("overwrite");
        journal.write(base, 12, b"tail").expect("sparse extension");
        let mut contents = [0xa5; 20];
        let read = journal.read(base, 0, &mut contents).expect("overlay read");

        assert_eq!(read, 16);
        assert_eq!(&contents[..16], b"imWRITEle\0\0\0tail");
    }
}
