//! Reading a mounted volume through one block of buffer.
//!
//! Every lookup and every read goes through [`Volume::block`], which holds the
//! last block it read. There is no page cache and no allocation: a binary
//! search over a directory re-reads a block only when it moves to another one,
//! and a data block costs a second read for the checksum that covers it.

use core::cmp::Ordering;

use molt_block::{Device, SECTOR, Writable};

use crate::FsError;
use crate::crc::{Crc, crc32c};
use crate::layout::{
    Area, BLOCK, ENTRY_BYTES, EXTENT_BYTES, Entry, Extent, Kind, MAX_NAME, OBJECT_BYTES, Object,
    SUPERS, Super, u32_at,
};
use crate::name::Name;

/// Sectors per block.
const SECTORS: u64 = (BLOCK / SECTOR) as u64;

/// A mounted, read-only volume.
pub struct Volume<'buf, D> {
    device: D,
    block: &'buf mut [u8; BLOCK],
    cached: Option<u64>,
    superblock: Super,
    active_copy: u64,
    previous_log: Option<u64>,
}

impl<'buf, D: Device> Volume<'buf, D> {
    /// Mounts `device`, using `block` as its only buffer.
    ///
    /// Takes the newest superblock copy that verifies, then checks every
    /// metadata region against the checksum the superblock records, so a
    /// corrupt volume fails at mount rather than at the first lookup that
    /// happens to touch the damaged block.
    pub fn mount(mut device: D, block: &'buf mut [u8; BLOCK]) -> Result<Self, FsError> {
        let mut copies = [None; SUPERS as usize];
        let mut last_error = FsError::Magic;
        for copy in 0..SUPERS {
            read(&mut device, block, copy)?;
            match Super::parse(block) {
                Ok(parsed) => copies[copy as usize] = Some(parsed),
                Err(error) => last_error = error,
            }
        }

        let mut rejected = [false; SUPERS as usize];
        for _ in 0..SUPERS {
            let Some((active_copy, superblock)) = copies
                .iter()
                .enumerate()
                .filter(|(copy, _)| !rejected[*copy])
                .filter_map(|(copy, parsed)| parsed.map(|parsed| (copy as u64, parsed)))
                .max_by_key(|(copy, parsed)| (parsed.generation, core::cmp::Reverse(*copy)))
            else {
                break;
            };
            rejected[active_copy as usize] = true;
            if superblock.blocks.saturating_mul(SECTORS) > device.sectors() {
                last_error = FsError::Corrupt;
                continue;
            }
            if let Err(error) = verify_checkpoint(&mut device, block, superblock) {
                last_error = error;
                continue;
            }

            let previous_log = copies
                .iter()
                .enumerate()
                .find(|(copy, _)| *copy != active_copy as usize)
                .and_then(|(_, parsed)| parsed.map(|parsed| parsed.region(Area::Log).at));
            return Ok(Self { device, block, cached: None, superblock, active_copy, previous_log });
        }
        Err(last_error)
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

    pub(crate) fn commit(&mut self, copy: u64, checkpoint: Super) {
        self.previous_log = Some(self.superblock.region(Area::Log).at);
        self.superblock = checkpoint;
        self.active_copy = copy;
        self.cached = None;
    }

    /// Reads one object record.
    pub fn object(&mut self, id: u32) -> Result<Object, FsError> {
        Object::parse(self.record(Area::Objects, id as u64, OBJECT_BYTES)?)
    }

    /// Reads `dir`'s entry at `index`, in name order.
    pub fn entry(&mut self, dir: &Object, index: u32) -> Result<(Name, u32), FsError> {
        let entry = self.at(dir, index)?;
        Ok((self.name(entry)?, entry.object))
    }

    /// Finds `name` in `dir`, returning the object it names.
    ///
    /// Entries are sorted, so this is a binary search: a directory of a
    /// thousand names costs ten block reads, not a thousand.
    pub fn lookup(&mut self, dir: &Object, name: &[u8]) -> Result<u32, FsError> {
        if dir.kind != Kind::Dir {
            return Err(FsError::Kind);
        }
        let (mut low, mut high) = (0, dir.count);
        while low < high {
            let middle = low + (high - low) / 2;
            let entry = self.at(dir, middle)?;
            match self.name(entry)?.as_bytes().cmp(name) {
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
    pub fn read(&mut self, file: &Object, offset: u64, buf: &mut [u8]) -> Result<usize, FsError> {
        if file.kind != Kind::File {
            return Err(FsError::Kind);
        }
        if offset > file.size {
            return Err(FsError::Range);
        }

        let want = (file.size - offset).min(buf.len() as u64) as usize;
        let mut done = 0;
        while done < want {
            let at = offset + done as u64;
            let logical = u32::try_from(at / BLOCK as u64).map_err(|_| FsError::Corrupt)?;
            let within = (at % BLOCK as u64) as usize;
            let take = (want - done).min(BLOCK - within);
            match self.locate(file, logical)? {
                Some(block) => {
                    let source = self.data(block)?;
                    buf[done..done + take].copy_from_slice(&source[within..within + take]);
                }
                None => buf[done..done + take].fill(0),
            }
            done += take;
        }
        Ok(want)
    }

    /// The physical block holding `file`'s `logical` block, if it is not a hole.
    fn locate(&mut self, file: &Object, logical: u32) -> Result<Option<u64>, FsError> {
        let (mut low, mut high) = (0, file.count);
        while low < high {
            let middle = low + (high - low) / 2;
            let index = file.start.checked_add(middle).ok_or(FsError::Corrupt)?;
            let extent = Extent::parse(self.record(Area::Extents, index as u64, EXTENT_BYTES)?)?;
            if let Some(block) = extent.covers(logical)? {
                return Ok(Some(block));
            }
            if extent.logical < logical {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        Ok(None)
    }

    fn at(&mut self, dir: &Object, index: u32) -> Result<Entry, FsError> {
        if dir.kind != Kind::Dir {
            return Err(FsError::Kind);
        }
        if index >= dir.count {
            return Err(FsError::Missing);
        }
        let index = dir.start.checked_add(index).ok_or(FsError::Corrupt)?;
        Entry::parse(self.record(Area::Entries, index as u64, ENTRY_BYTES)?)
    }

    /// Copies an entry's name out of the name region, which it may straddle
    /// blocks of.
    fn name(&mut self, entry: Entry) -> Result<Name, FsError> {
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
            let source = self.block(region.at + at / BLOCK as u64)?;
            bytes[done..done + take].copy_from_slice(&source[within..within + take]);
            done += take;
        }
        Name::new(&bytes[..len])
    }

    /// Borrows one fixed-size record out of a metadata region.
    fn record(&mut self, area: Area, index: u64, size: usize) -> Result<&[u8], FsError> {
        let region = self.superblock.region(area);
        let at = index.checked_mul(size as u64).ok_or(FsError::Corrupt)?;
        if at + size as u64 > region.bytes {
            return Err(FsError::Missing);
        }
        let within = (at % BLOCK as u64) as usize;
        let block = self.block(region.at + at / BLOCK as u64)?;
        Ok(&block[within..within + size])
    }

    /// Reads a data block and checks it against the sum recorded for it.
    fn data(&mut self, index: u64) -> Result<&[u8; BLOCK], FsError> {
        let offset = index.checked_sub(self.superblock.data_at).ok_or(FsError::Corrupt)?;
        if offset >= self.superblock.data_blocks {
            return Err(FsError::Corrupt);
        }

        let sums = self.superblock.region(Area::Sums);
        let at = offset * 4;
        let within = (at % BLOCK as u64) as usize;
        let expected = u32_at(self.block(sums.at + at / BLOCK as u64)?, within);

        let block = self.block(index)?;
        if crc32c(block) != expected {
            return Err(FsError::Checksum);
        }
        Ok(block)
    }

    /// Reads a block, or hands back the one already in the buffer.
    pub(crate) fn block(&mut self, index: u64) -> Result<&[u8; BLOCK], FsError> {
        if index >= self.superblock.blocks {
            return Err(FsError::Corrupt);
        }
        if self.cached != Some(index) {
            // The buffer holds a partial block until the read lands.
            self.cached = None;
            read(&mut self.device, self.block, index)?;
            self.cached = Some(index);
        }
        Ok(self.block)
    }
}

impl<D: Writable> Volume<'_, D> {
    pub(crate) fn copy_aligned(
        &mut self,
        source: u64,
        target: u64,
        bytes: u64,
    ) -> Result<(), FsError> {
        if bytes % SECTOR as u64 != 0 {
            return Err(FsError::Corrupt);
        }
        let mut done = 0;
        while done < bytes {
            let take = (bytes - done).min(BLOCK as u64) as usize;
            let source_sector = source
                .checked_mul(SECTORS)
                .and_then(|sector| sector.checked_add(done / SECTOR as u64))
                .ok_or(FsError::Corrupt)?;
            self.device.read(source_sector, &mut self.block[..take]).map_err(FsError::Device)?;
            let target_sector = target
                .checked_mul(SECTORS)
                .and_then(|sector| sector.checked_add(done / SECTOR as u64))
                .ok_or(FsError::Corrupt)?;
            self.device.write(target_sector, &self.block[..take]).map_err(FsError::Device)?;
            done += take as u64;
        }
        self.cached = None;
        Ok(())
    }

    pub(crate) fn write_aligned(
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
        self.device.write(sector, bytes).map_err(FsError::Device)?;
        self.cached = None;
        Ok(())
    }

    pub(crate) fn checksum(&mut self, block: u64, bytes: u64) -> Result<u32, FsError> {
        let mut crc = Crc::new();
        let mut left = bytes;
        let mut index = block;
        while left > 0 {
            let take = left.min(BLOCK as u64) as usize;
            crc.update(&self.block(index)?[..take]);
            left -= take as u64;
            index += 1;
        }
        Ok(crc.finish())
    }

    pub(crate) fn write_checkpoint(&mut self, copy: u64, value: Super) -> Result<(), FsError> {
        if copy >= SUPERS {
            return Err(FsError::Corrupt);
        }
        self.block.fill(0);
        value.encode(self.block);
        self.device.write(copy * SECTORS, &self.block[..SECTOR]).map_err(FsError::Device)?;
        self.cached = None;
        Ok(())
    }

    pub(crate) fn flush(&mut self) -> Result<(), FsError> {
        self.device.flush().map_err(FsError::Device)
    }
}

fn read<D: Device>(device: &mut D, block: &mut [u8; BLOCK], index: u64) -> Result<(), FsError> {
    let sector = index.checked_mul(SECTORS).ok_or(FsError::Corrupt)?;
    device.read(sector, block.as_mut_slice()).map_err(FsError::Device)
}

fn verify_checkpoint<D: Device>(
    device: &mut D,
    block: &mut [u8; BLOCK],
    superblock: Super,
) -> Result<(), FsError> {
    for area in Area::ALL {
        let region = superblock.region(area);
        let mut crc = Crc::new();
        let mut left = region.bytes;
        let mut index = region.at;
        while left > 0 {
            let take = left.min(BLOCK as u64) as usize;
            read(device, block, index)?;
            crc.update(&block[..take]);
            left -= take as u64;
            index += 1;
        }
        if crc.finish() != region.crc {
            return Err(FsError::Checksum);
        }
    }
    Ok(())
}

#[cfg(all(test, feature = "format"))]
mod tests {
    use molt_block::Loopback;

    use super::Volume;
    use crate::crc::crc32c;
    use crate::format::{Tree, build};
    use crate::layout::{Area, BLOCK, Kind, Super};
    use crate::{FsError, MAX_NAME};

    fn image() -> alloc::vec::Vec<u8> {
        let mut tree = Tree::new();
        tree.file("hello.txt", b"hello, molt".to_vec()).expect("legal name");
        tree.file("big.bin", alloc::vec![0xa5; 3 * BLOCK + 7]).expect("legal name");
        tree.dir("docs").expect("legal name").file("readme", b"read me".to_vec()).unwrap();
        build(&tree, 1).expect("image that fits")
    }

    fn mount<'a>(bytes: &'a [u8], block: &'a mut [u8; BLOCK]) -> Volume<'a, Loopback<'a>> {
        Volume::mount(Loopback::new(bytes).expect("whole sectors"), block).expect("live volume")
    }

    #[test]
    fn file_reads_back_what_was_written() {
        let bytes = image();
        let mut buffer = [0u8; BLOCK];
        let mut volume = mount(&bytes, &mut buffer);

        let root = volume.object(volume.root()).expect("root object");
        let id = volume.lookup(&root, b"hello.txt").expect("name in the root");
        let file = volume.object(id).expect("file object");
        let mut text = [0u8; 16];
        let read = volume.read(&file, 0, &mut text).expect("readable file");

        assert_eq!(&text[..read], b"hello, molt");
    }

    #[test]
    fn read_crossing_blocks_stays_contiguous() {
        let bytes = image();
        let mut buffer = [0u8; BLOCK];
        let mut volume = mount(&bytes, &mut buffer);

        let root = volume.object(volume.root()).expect("root object");
        let id = volume.lookup(&root, b"big.bin").expect("name in the root");
        let file = volume.object(id).expect("file object");
        let mut window = [0u8; 8];
        let read = volume.read(&file, BLOCK as u64 - 4, &mut window).expect("readable file");

        assert_eq!(read, 8);
        assert_eq!(window, [0xa5; 8], "the block boundary lost bytes");
    }

    #[test]
    fn short_read_stops_at_end_of_file() {
        let bytes = image();
        let mut buffer = [0u8; BLOCK];
        let mut volume = mount(&bytes, &mut buffer);

        let root = volume.object(volume.root()).expect("root object");
        let id = volume.lookup(&root, b"hello.txt").expect("name in the root");
        let file = volume.object(id).expect("file object");

        assert_eq!(volume.read(&file, 6, &mut [0; 64]), Ok(5));
    }

    #[test]
    fn missing_name_reported() {
        let bytes = image();
        let mut buffer = [0u8; BLOCK];
        let mut volume = mount(&bytes, &mut buffer);

        let root = volume.object(volume.root()).expect("root object");

        assert_eq!(volume.lookup(&root, b"nothing"), Err(FsError::Missing));
    }

    #[test]
    fn entries_come_back_sorted() {
        let bytes = image();
        let mut buffer = [0u8; BLOCK];
        let mut volume = mount(&bytes, &mut buffer);

        let root = volume.object(volume.root()).expect("root object");
        let first = volume.entry(&root, 0).expect("entry").0;
        let second = volume.entry(&root, 1).expect("entry").0;

        assert_eq!(first.as_str(), Some("big.bin"));
        assert_eq!(second.as_str(), Some("docs"));
    }

    #[test]
    fn nested_directory_reachable() {
        let bytes = image();
        let mut buffer = [0u8; BLOCK];
        let mut volume = mount(&bytes, &mut buffer);

        let root = volume.object(volume.root()).expect("root object");
        let docs = volume.lookup(&root, b"docs").expect("subdirectory");
        let docs = volume.object(docs).expect("directory object");
        let id = volume.lookup(&docs, b"readme").expect("name in the subdirectory");

        assert_eq!(volume.object(id).expect("file object").kind, Kind::File);
    }

    #[test]
    fn entry_past_end_reported() {
        let bytes = image();
        let mut buffer = [0u8; BLOCK];
        let mut volume = mount(&bytes, &mut buffer);

        let root = volume.object(volume.root()).expect("root object");

        assert_eq!(volume.entry(&root, 3).map(|entry| entry.0), Err(FsError::Missing));
    }

    #[test]
    fn corrupt_data_block_refused() {
        let mut bytes = image();
        let data = super::Super::parse(&bytes[..BLOCK]).expect("superblock").data_at;
        bytes[data as usize * BLOCK] ^= 0xff;

        let mut buffer = [0u8; BLOCK];
        let mut volume = mount(&bytes, &mut buffer);
        let root = volume.object(volume.root()).expect("root object");
        let id = volume.lookup(&root, b"big.bin").expect("name in the root");
        let file = volume.object(id).expect("file object");

        assert_eq!(volume.read(&file, 0, &mut [0; 8]), Err(FsError::Checksum));
    }

    #[test]
    fn extent_physical_overflow_refused() {
        let mut bytes = image();
        let mut superblock = Super::parse(&bytes[..BLOCK]).expect("superblock");
        let mut extents = superblock.region(Area::Extents);
        let at = extents.at as usize * BLOCK;
        bytes[at + 8..at + 16].copy_from_slice(&u64::MAX.to_le_bytes());
        extents.crc = crc32c(&bytes[at..at + extents.bytes as usize]);
        superblock.set_region(Area::Extents, extents);
        for copy in 0..super::SUPERS {
            superblock.encode(&mut bytes[copy as usize * BLOCK..]);
        }

        let mut buffer = [0u8; BLOCK];
        let mut volume = mount(&bytes, &mut buffer);
        let root = volume.object(volume.root()).expect("root object");
        let id = volume.lookup(&root, b"big.bin").expect("name in the root");
        let file = volume.object(id).expect("file object");

        assert_eq!(volume.read(&file, BLOCK as u64, &mut [0; 8]), Err(FsError::Corrupt));
    }

    #[test]
    fn corrupt_metadata_refused_at_mount() {
        let mut bytes = image();
        let superblock = super::Super::parse(&bytes[..BLOCK]).expect("superblock");
        let at = superblock.region(Area::Objects).at as usize * BLOCK;
        bytes[at] ^= 0xff;

        let mut buffer = [0u8; BLOCK];
        let device = Loopback::new(&bytes).expect("whole sectors");

        assert_eq!(
            Volume::mount(device, &mut buffer).err(),
            Some(FsError::Checksum),
            "a damaged object region mounted"
        );
    }

    #[test]
    fn torn_superblock_falls_back_to_older_copy() {
        let mut bytes = image();
        bytes[0] ^= 0xff;

        let mut buffer = [0u8; BLOCK];
        let mut volume = mount(&bytes, &mut buffer);
        let root = volume.object(volume.root()).expect("root object");

        assert!(volume.lookup(&root, b"hello.txt").is_ok(), "the older copy did not serve");
    }

    #[test]
    fn overlong_name_never_stored() {
        let mut tree = Tree::new();

        assert_eq!(tree.file(&"a".repeat(MAX_NAME + 1), alloc::vec![]), Err(FsError::Name));
    }
}
