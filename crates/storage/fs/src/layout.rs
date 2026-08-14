//! On-disk shape of a volume checkpoint.

use alloc::boxed::Box;

use molt_bytes::{Bytes, BytesMut};

use crate::crc::crc32c;
use crate::{FsError, mem};

/// The unit everything on a volume is addressed in.
pub const BLOCK: usize = 4096;

/// A zeroed block of buffer on the heap.
///
/// Whoever needs one needs it for as long as they live, and none of them is
/// small enough to be a local.
pub(crate) fn buffer() -> Result<Box<[u8; BLOCK]>, FsError> {
    mem::zeroed()
}

/// The signature a volume opens with.
pub const MAGIC: [u8; 8] = *b"MOLTFS05";

/// The format this crate reads.
pub const VERSION: u32 = 5;

/// Superblock copies at the start of the volume.
///
/// A checkpoint writes the older copy, flushes, and only then makes it the
/// newer one, so a volume always has one superblock that predates the crash.
pub const SUPERS: u64 = 2;

/// How much of block zero the superblock occupies.
pub const SUPER_BYTES: usize = 220;

/// Log banks kept so newest and previous checkpoints remain intact while a
/// third bank receives the next generation.
pub const LOG_BANKS: u64 = 3;

/// Log capacity an image builder reserves unless its caller chooses another.
#[cfg(feature = "format")]
pub const DEFAULT_LOG_BLOCKS: u32 = 128;

/// COW metadata nodes an image builder reserves by default.
#[cfg(feature = "format")]
pub const DEFAULT_TREE_BLOCKS: u32 = 256;

/// Largest tree arena a superblock may claim.
///
/// Mount sizes its arena bitmaps from this field, so the bound is what keeps a
/// corrupt superblock from asking the heap for megabytes of bits. At 64 Ki
/// nodes that is a 256 MiB arena tracked by 8 KiB.
pub const MAX_TREE_BLOCKS: u32 = 1 << 16;

/// Where each superblock field sits.
mod field {
    pub const MAGIC: usize = 0;
    pub const VERSION: usize = 8;
    pub const BLOCK_SIZE: usize = 12;
    pub const GENERATION: usize = 16;
    pub const BLOCKS: usize = 24;
    pub const ROOT: usize = 32;
    pub const REGIONS: usize = 64;
    pub const LOG_BLOCKS: usize = 184;
    pub const TREE_AT: usize = 192;
    pub const TREE_BLOCKS: usize = 200;
    pub const TREE_ROOT: usize = 208;
    pub const CRC: usize = 216;
}

/// One region descriptor: where it starts, how long it is, what it hashes to.
const REGION_BYTES: usize = 24;

/// The longest name a directory entry may carry.
pub const MAX_NAME: usize = 255;

/// The metadata regions a superblock describes, in the order it lists them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Area {
    /// File payload records committed by the active checkpoint.
    Log,
}

impl Area {
    pub const ALL: [Self; 1] = [Self::Log];

    const fn index(self) -> usize {
        0
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
    pub log_blocks: u32,
    pub tree_at: u64,
    pub tree_blocks: u32,
    pub tree_root: u64,
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
        if crc32c(&block[..field::CRC]) != block.read_le::<u32>(field::CRC).unwrap() {
            return Err(FsError::Checksum);
        }

        let version = block.read_le::<u32>(field::VERSION).unwrap();
        if version != VERSION {
            return Err(FsError::Version(version));
        }
        if block.read_le::<u32>(field::BLOCK_SIZE).unwrap() as usize != BLOCK {
            return Err(FsError::Corrupt);
        }

        let mut parsed = Self {
            generation: block.read_le::<u64>(field::GENERATION).unwrap(),
            blocks: block.read_le::<u64>(field::BLOCKS).unwrap(),
            root: block.read_le::<u32>(field::ROOT).unwrap(),
            log_blocks: block.read_le::<u32>(field::LOG_BLOCKS).unwrap(),
            tree_at: block.read_le::<u64>(field::TREE_AT).unwrap(),
            tree_blocks: block.read_le::<u32>(field::TREE_BLOCKS).unwrap(),
            tree_root: block.read_le::<u64>(field::TREE_ROOT).unwrap(),
            regions: [Region::default(); Area::ALL.len()],
        };
        for area in Area::ALL {
            let at = field::REGIONS + area.index() * REGION_BYTES;
            parsed.set_region(
                area,
                Region {
                    at: block.read_le::<u64>(at).unwrap(),
                    bytes: block.read_le::<u64>(at + 8).unwrap(),
                    crc: block.read_le::<u32>(at + 16).unwrap(),
                },
            );
        }
        parsed.check()?;
        Ok(parsed)
    }

    /// Writes the superblock into `block`, stamping its checksum last.
    ///
    /// The image builder writes both initial copies; sync writes one new copy.
    pub fn encode(&self, block: &mut [u8]) {
        let block = &mut block[..SUPER_BYTES];
        block.fill(0);
        block[field::MAGIC..field::MAGIC + MAGIC.len()].copy_from_slice(&MAGIC);
        block.write_le(field::VERSION, VERSION).unwrap();
        block.write_le(field::BLOCK_SIZE, BLOCK as u32).unwrap();
        block.write_le(field::GENERATION, self.generation).unwrap();
        block.write_le(field::BLOCKS, self.blocks).unwrap();
        block.write_le(field::ROOT, self.root).unwrap();
        block.write_le(field::LOG_BLOCKS, self.log_blocks).unwrap();
        block.write_le(field::TREE_AT, self.tree_at).unwrap();
        block.write_le(field::TREE_BLOCKS, self.tree_blocks).unwrap();
        block.write_le(field::TREE_ROOT, self.tree_root).unwrap();
        for area in Area::ALL {
            let region = self.region(area);
            let at = field::REGIONS + area.index() * REGION_BYTES;
            block.write_le(at, region.at).unwrap();
            block.write_le(at + 8, region.bytes).unwrap();
            block.write_le(at + 16, region.crc).unwrap();
        }
        block.write_le(field::CRC, crc32c(&block[..field::CRC])).unwrap();
    }

    /// Rejects a superblock whose regions do not fit the volume it describes.
    fn check(&self) -> Result<(), FsError> {
        let log_span = u64::from(self.log_blocks).checked_mul(LOG_BANKS).ok_or(FsError::Corrupt)?;
        let log_start = self.blocks.checked_sub(log_span).ok_or(FsError::Corrupt)?;
        let tree_end =
            self.tree_at.checked_add(u64::from(self.tree_blocks)).ok_or(FsError::Corrupt)?;
        if self.log_blocks == 0
            || self.tree_blocks == 0
            || self.tree_blocks > MAX_TREE_BLOCKS
            || self.tree_at < SUPERS
            || tree_end > log_start
            || (self.tree_root != 0
                && (self.tree_root < self.tree_at || self.tree_root >= tree_end))
        {
            return Err(FsError::Corrupt);
        }
        for area in Area::ALL {
            let region = self.region(area);
            let end = region.at.checked_add(region.blocks()).ok_or(FsError::Corrupt)?;
            if region.at < SUPERS || end > self.blocks {
                return Err(FsError::Corrupt);
            }
            let bank_bytes =
                u64::from(self.log_blocks).checked_mul(BLOCK as u64).ok_or(FsError::Corrupt)?;
            let in_bank = (0..LOG_BANKS)
                .any(|bank| region.at == log_start + bank * u64::from(self.log_blocks));
            if !in_bank || region.bytes > bank_bytes {
                return Err(FsError::Corrupt);
            }
        }
        Ok(())
    }

    /// First block of log bank `bank`.
    pub fn log_bank(&self, bank: u64) -> Result<u64, FsError> {
        if bank >= LOG_BANKS {
            return Err(FsError::Corrupt);
        }
        let span = u64::from(self.log_blocks).checked_mul(LOG_BANKS).ok_or(FsError::Corrupt)?;
        let start = self.blocks.checked_sub(span).ok_or(FsError::Corrupt)?;
        Ok(start + bank * u64::from(self.log_blocks))
    }
}

/// What an object is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Dir,
    File,
}

impl Kind {
    pub const fn byte(self) -> u8 {
        self as u8
    }
}

/// One object indexed by the metadata tree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Object {
    pub kind: Kind,
    /// Directory entries; zero for a file.
    pub count: u32,
    /// File length in bytes; zero for a directory.
    pub size: u64,
}

#[cfg(test)]
mod tests {
    use molt_bytes::BytesMut;

    use super::{Area, BLOCK, Region, Super, field};
    use crate::FsError;

    fn volume() -> Super {
        let mut parsed = Super {
            generation: 7,
            blocks: 32,
            root: 0,
            log_blocks: 4,
            tree_at: 2,
            tree_blocks: 18,
            ..Super::default()
        };
        parsed.set_region(Area::Log, Region { at: 20, bytes: 0, crc: 0 });
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
        block.write_le(field::CRC, crc).unwrap();

        assert_eq!(Super::parse(&block), Err(FsError::Version(9)));
    }

    #[test]
    fn region_past_end_refused() {
        let mut block = [0u8; BLOCK];
        let mut parsed = volume();
        parsed.set_region(Area::Log, Region { at: 31, bytes: 2 * BLOCK as u64, crc: 0 });
        parsed.encode(&mut block);

        assert_eq!(Super::parse(&block), Err(FsError::Corrupt));
    }

    #[test]
    fn tree_over_log_refused() {
        let mut block = [0u8; BLOCK];
        let mut parsed = volume();
        parsed.tree_blocks += 1;
        parsed.encode(&mut block);

        assert_eq!(Super::parse(&block), Err(FsError::Corrupt));
    }

    #[test]
    fn tree_root_outside_arena_refused() {
        let mut block = [0u8; BLOCK];
        let mut parsed = volume();
        parsed.tree_root = parsed.tree_at + u64::from(parsed.tree_blocks);
        parsed.encode(&mut block);

        assert_eq!(Super::parse(&block), Err(FsError::Corrupt));
    }
}
