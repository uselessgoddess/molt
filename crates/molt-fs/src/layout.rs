//! The on-disk shape of a volume: superblock, regions, and the three records.
//!
//! Every number is little-endian and every record size divides [`BLOCK`], so no
//! record straddles a block boundary and a reader needs exactly one block of
//! buffer to reach any of them. That constraint is why an object is 32 bytes
//! rather than the 24 its fields need.

use crate::FsError;
use crate::crc::crc32c;

/// The unit everything on a volume is addressed in.
pub const BLOCK: usize = 4096;

/// The signature a volume opens with.
pub const MAGIC: [u8; 8] = *b"MOLTROFS";

/// The format this crate reads.
pub const VERSION: u32 = 1;

/// Superblock copies at the start of the volume.
///
/// A checkpoint writes the older copy, flushes, and only then makes it the
/// newer one, so a volume always has one superblock that predates the crash.
pub const SUPERS: u64 = 2;

/// How much of block zero the superblock occupies.
pub const SUPER_BYTES: usize = 192;

/// Where each superblock field sits.
mod field {
    pub const MAGIC: usize = 0;
    pub const VERSION: usize = 8;
    pub const BLOCK_SIZE: usize = 12;
    pub const GENERATION: usize = 16;
    pub const BLOCKS: usize = 24;
    pub const ROOT: usize = 32;
    pub const DATA_AT: usize = 40;
    pub const DATA_BLOCKS: usize = 48;
    pub const REGIONS: usize = 64;
    pub const CRC: usize = 188;
}

/// One region descriptor: where it starts, how long it is, what it hashes to.
const REGION_BYTES: usize = 24;

/// The longest name a directory entry may carry.
pub const MAX_NAME: usize = 64;

pub const OBJECT_BYTES: usize = 32;
pub const EXTENT_BYTES: usize = 16;
pub const ENTRY_BYTES: usize = 16;

const _: () = assert!(
    BLOCK % OBJECT_BYTES == 0 && BLOCK % EXTENT_BYTES == 0 && BLOCK % ENTRY_BYTES == 0,
    "a record that straddles a block cannot be read out of one block buffer",
);

/// The metadata regions a superblock describes, in the order it lists them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Area {
    /// One record per object, indexed by object id.
    Objects,
    /// Extent records, sorted by logical block within each file.
    Extents,
    /// Directory entries, sorted by name within each directory.
    Entries,
    /// The bytes every entry's name points into.
    Names,
    /// One crc32c per data block.
    Sums,
}

impl Area {
    pub const ALL: [Self; 5] =
        [Self::Objects, Self::Extents, Self::Entries, Self::Names, Self::Sums];

    const fn index(self) -> usize {
        match self {
            Self::Objects => 0,
            Self::Extents => 1,
            Self::Entries => 2,
            Self::Names => 3,
            Self::Sums => 4,
        }
    }
}

/// Where a region lives and what its contents hash to.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Region {
    pub at: u64,
    pub bytes: u64,
    pub crc: u32,
}

impl Region {
    /// How many blocks the region occupies, its tail padded out.
    pub const fn blocks(self) -> u64 {
        self.bytes.div_ceil(BLOCK as u64)
    }
}

/// What a volume is: a generation, a root, and where everything sits.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Super {
    pub generation: u64,
    pub blocks: u64,
    pub root: u32,
    pub data_at: u64,
    pub data_blocks: u64,
    pub(crate) regions: [Region; Area::ALL.len()],
}

impl Super {
    pub const fn region(&self, area: Area) -> Region {
        self.regions[area.index()]
    }

    pub fn set_region(&mut self, area: Area, region: Region) {
        self.regions[area.index()] = region;
    }

    /// Reads a superblock out of block zero of a copy.
    ///
    /// The checksum is checked before any field is trusted, so a torn write is
    /// rejected here rather than by whatever the region offsets would have
    /// pointed at.
    pub fn parse(block: &[u8]) -> Result<Self, FsError> {
        let block = block.get(..SUPER_BYTES).ok_or(FsError::Corrupt)?;
        if block[field::MAGIC..field::MAGIC + MAGIC.len()] != MAGIC {
            return Err(FsError::Magic);
        }
        if crc32c(&block[..field::CRC]) != u32_at(block, field::CRC) {
            return Err(FsError::Checksum);
        }

        let version = u32_at(block, field::VERSION);
        if version != VERSION {
            return Err(FsError::Version(version));
        }
        if u32_at(block, field::BLOCK_SIZE) as usize != BLOCK {
            return Err(FsError::Corrupt);
        }

        let mut parsed = Self {
            generation: u64_at(block, field::GENERATION),
            blocks: u64_at(block, field::BLOCKS),
            root: u32_at(block, field::ROOT),
            data_at: u64_at(block, field::DATA_AT),
            data_blocks: u64_at(block, field::DATA_BLOCKS),
            regions: [Region::default(); Area::ALL.len()],
        };
        for area in Area::ALL {
            let at = field::REGIONS + area.index() * REGION_BYTES;
            parsed.set_region(
                area,
                Region {
                    at: u64_at(block, at),
                    bytes: u64_at(block, at + 8),
                    crc: u32_at(block, at + 16),
                },
            );
        }
        parsed.check()?;
        Ok(parsed)
    }

    /// Writes the superblock into `block`, stamping its checksum last.
    pub fn encode(&self, block: &mut [u8]) {
        let block = &mut block[..SUPER_BYTES];
        block.fill(0);
        block[field::MAGIC..field::MAGIC + MAGIC.len()].copy_from_slice(&MAGIC);
        put_u32(block, field::VERSION, VERSION);
        put_u32(block, field::BLOCK_SIZE, BLOCK as u32);
        put_u64(block, field::GENERATION, self.generation);
        put_u64(block, field::BLOCKS, self.blocks);
        put_u32(block, field::ROOT, self.root);
        put_u64(block, field::DATA_AT, self.data_at);
        put_u64(block, field::DATA_BLOCKS, self.data_blocks);
        for area in Area::ALL {
            let region = self.region(area);
            let at = field::REGIONS + area.index() * REGION_BYTES;
            put_u64(block, at, region.at);
            put_u64(block, at + 8, region.bytes);
            put_u32(block, at + 16, region.crc);
        }
        put_u32(block, field::CRC, crc32c(&block[..field::CRC]));
    }

    /// Rejects a superblock whose regions do not fit the volume it describes.
    fn check(&self) -> Result<(), FsError> {
        let data_end = self.data_at.checked_add(self.data_blocks).ok_or(FsError::Corrupt)?;
        if data_end > self.blocks {
            return Err(FsError::Corrupt);
        }
        if self.region(Area::Sums).bytes != self.data_blocks * 4 {
            return Err(FsError::Corrupt);
        }
        for area in Area::ALL {
            let region = self.region(area);
            let end = region.at.checked_add(region.blocks()).ok_or(FsError::Corrupt)?;
            if region.at < SUPERS || end > self.blocks {
                return Err(FsError::Corrupt);
            }
        }
        Ok(())
    }
}

/// What an object is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Dir,
    File,
}

impl Kind {
    const fn of(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Dir),
            1 => Some(Self::File),
            _ => None,
        }
    }

    pub const fn byte(self) -> u8 {
        self as u8
    }
}

/// One object: a directory's entry range, or a file's extents and length.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Object {
    pub kind: Kind,
    /// First entry (directory) or extent (file) index.
    pub start: u32,
    /// How many of them belong to this object.
    pub count: u32,
    /// Length in bytes; zero for a directory.
    pub size: u64,
}

impl Object {
    pub fn parse(record: &[u8]) -> Result<Self, FsError> {
        let record = record.get(..OBJECT_BYTES).ok_or(FsError::Corrupt)?;
        Ok(Self {
            kind: Kind::of(record[0]).ok_or(FsError::Corrupt)?,
            start: u32_at(record, 4),
            count: u32_at(record, 8),
            size: u64_at(record, 16),
        })
    }

    pub fn encode(&self, record: &mut [u8]) {
        let record = &mut record[..OBJECT_BYTES];
        record.fill(0);
        record[0] = self.kind.byte();
        put_u32(record, 4, self.start);
        put_u32(record, 8, self.count);
        put_u64(record, 16, self.size);
    }
}

/// One run of a file's blocks, at a logical block offset within it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Extent {
    pub logical: u32,
    pub blocks: u32,
    pub block: u64,
}

impl Extent {
    pub fn parse(record: &[u8]) -> Result<Self, FsError> {
        let record = record.get(..EXTENT_BYTES).ok_or(FsError::Corrupt)?;
        Ok(Self { logical: u32_at(record, 0), blocks: u32_at(record, 4), block: u64_at(record, 8) })
    }

    pub fn encode(&self, record: &mut [u8]) {
        let record = &mut record[..EXTENT_BYTES];
        record.fill(0);
        put_u32(record, 0, self.logical);
        put_u32(record, 4, self.blocks);
        put_u64(record, 8, self.block);
    }

    /// The physical block holding `logical`, if this extent covers it.
    pub const fn covers(&self, logical: u32) -> Option<u64> {
        if logical < self.logical || logical - self.logical >= self.blocks {
            return None;
        }
        Some(self.block + (logical - self.logical) as u64)
    }
}

/// One directory entry: a name in the name region and the object it names.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Entry {
    pub name_at: u32,
    pub name_len: u16,
    pub object: u32,
}

impl Entry {
    pub fn parse(record: &[u8]) -> Result<Self, FsError> {
        let record = record.get(..ENTRY_BYTES).ok_or(FsError::Corrupt)?;
        let name_len = u16_at(record, 4);
        if name_len as usize > MAX_NAME {
            return Err(FsError::Corrupt);
        }
        Ok(Self { name_at: u32_at(record, 0), name_len, object: u32_at(record, 8) })
    }

    pub fn encode(&self, record: &mut [u8]) {
        let record = &mut record[..ENTRY_BYTES];
        record.fill(0);
        put_u32(record, 0, self.name_at);
        put_u16(record, 4, self.name_len);
        put_u32(record, 8, self.object);
    }
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    let mut word = [0; 2];
    word.copy_from_slice(&bytes[at..at + 2]);
    u16::from_le_bytes(word)
}

pub(crate) fn u32_at(bytes: &[u8], at: usize) -> u32 {
    let mut word = [0; 4];
    word.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(word)
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    let mut word = [0; 8];
    word.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(word)
}

fn put_u16(bytes: &mut [u8], at: usize, value: u16) {
    bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::{Area, BLOCK, Entry, Extent, Kind, Object, Region, Super, field};
    use crate::FsError;

    fn volume() -> Super {
        let mut parsed = Super {
            generation: 7,
            blocks: 16,
            root: 0,
            data_at: 14,
            data_blocks: 2,
            ..Super::default()
        };
        for area in Area::ALL {
            parsed.set_region(area, Region { at: 2, bytes: 0, crc: 0 });
        }
        parsed.set_region(Area::Objects, Region { at: 2, bytes: 32, crc: 1 });
        parsed.set_region(Area::Sums, Region { at: 3, bytes: 8, crc: 2 });
        parsed
    }

    #[test]
    fn superblock_survives_round_trip() {
        let mut block = [0u8; BLOCK];
        let written = volume();

        written.encode(&mut block);

        assert_eq!(Super::parse(&block), Ok(written));
    }

    #[test]
    fn torn_superblock_refused() {
        let mut block = [0u8; BLOCK];
        volume().encode(&mut block);

        block[field::ROOT] ^= 1;

        assert_eq!(Super::parse(&block), Err(FsError::Checksum));
    }

    #[test]
    fn foreign_block_refused() {
        assert_eq!(Super::parse(&[0u8; BLOCK]), Err(FsError::Magic));
    }

    #[test]
    fn future_version_refused() {
        let mut block = [0u8; BLOCK];
        volume().encode(&mut block);
        block[field::VERSION] = 9;
        let crc = super::crc32c(&block[..field::CRC]);
        super::put_u32(&mut block, field::CRC, crc);

        assert_eq!(Super::parse(&block), Err(FsError::Version(9)));
    }

    #[test]
    fn region_past_end_refused() {
        let mut block = [0u8; BLOCK];
        let mut parsed = volume();
        parsed.set_region(Area::Objects, Region { at: 15, bytes: 2 * BLOCK as u64, crc: 0 });
        parsed.encode(&mut block);

        assert_eq!(Super::parse(&block), Err(FsError::Corrupt));
    }

    #[test]
    fn object_survives_round_trip() {
        let mut record = [0u8; super::OBJECT_BYTES];
        let written = Object { kind: Kind::File, start: 3, count: 2, size: 5000 };

        written.encode(&mut record);

        assert_eq!(Object::parse(&record), Ok(written));
    }

    #[test]
    fn extent_covers_its_own_blocks_only() {
        let extent = Extent { logical: 4, blocks: 2, block: 100 };

        assert_eq!(extent.covers(5), Some(101));
        assert_eq!(extent.covers(6), None, "an extent claimed a block past its end");
    }

    #[test]
    fn entry_survives_round_trip() {
        let mut record = [0u8; super::ENTRY_BYTES];
        let written = Entry { name_at: 12, name_len: 5, object: 3 };

        written.encode(&mut record);

        assert_eq!(Entry::parse(&record), Ok(written));
    }

    #[test]
    fn overlong_name_refused() {
        let mut record = [0u8; super::ENTRY_BYTES];
        Entry { name_at: 0, name_len: super::MAX_NAME as u16 + 1, object: 0 }.encode(&mut record);

        assert_eq!(Entry::parse(&record), Err(FsError::Corrupt));
    }
}
