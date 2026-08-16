#![no_std]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

//! Admission and loading of hardware-domain images.
//!
//! Molt uses a deliberately small, static ELF64 profile as the common outer
//! container for tier-2 domains and the future tier-1 aperture. Admission is
//! complete before [`load`] calls the mapper, so a rejected image has never
//! caused an executable page-table entry to exist.

#[cfg(test)]
extern crate std;

use repr::{Field, records};

const ELF_HEADER: usize = 64;
const PROGRAM_HEADER: usize = 56;
const LOAD: u32 = 1;
const DYNAMIC: u32 = 2;
const INTERP: u32 = 3;
const FLAGS: u32 = 0b111;
const WRITE: u32 = 0b010;
const EXECUTE: u32 = 0b001;
const PAGE: u64 = 4096;
const SEGMENTS: usize = 8;

/// An instruction set named by the ELF `e_machine` field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Architecture {
    X86_64,
    RiscV,
}

impl Architecture {
    const fn machine(self) -> u16 {
        match self {
            Self::X86_64 => 62,
            Self::RiscV => 243,
        }
    }
}

/// Why an image was refused before any mapping was made.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Truncated,
    Format,
    Architecture,
    Unsupported,
    TooManySegments,
    Segment,
    Overlap,
    WriteExecute,
    Entry,
}

/// Protection carried by one admitted load segment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Protection(u32);

impl Protection {
    pub const fn is_read(self) -> bool {
        self.0 & 0b100 != 0
    }

    pub const fn is_write(self) -> bool {
        self.0 & WRITE != 0
    }

    pub const fn is_execute(self) -> bool {
        self.0 & EXECUTE != 0
    }
}

/// One page-aligned range admitted from a `PT_LOAD` header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Segment {
    virtual_address: u64,
    memory_size: u64,
    mapped_size: u64,
    file_offset: u64,
    file_size: u64,
    protection: Protection,
}

impl Segment {
    pub const fn virtual_address(self) -> u64 {
        self.virtual_address
    }

    pub const fn memory_size(self) -> u64 {
        self.memory_size
    }

    pub const fn mapped_size(self) -> u64 {
        self.mapped_size
    }

    pub const fn file_offset(self) -> u64 {
        self.file_offset
    }

    pub const fn file_size(self) -> u64 {
        self.file_size
    }

    pub const fn protection(self) -> Protection {
        self.protection
    }

    fn contains(self, address: u64) -> bool {
        self.virtual_address <= address
            && address < self.virtual_address.saturating_add(self.memory_size)
    }
}

/// A target that receives only fully admitted segments.
pub trait Mapper {
    type Error;

    fn map(&mut self, segment: Segment, file: &[u8]) -> Result<(), Self::Error>;
}

/// A load failure, separated into admission and mapping halves.
#[derive(Debug, Eq, PartialEq)]
pub enum LoadError<E> {
    Image(Error),
    Map(E),
}

/// Validates all bytes, then maps the segments in virtual-address order.
///
/// No call to `mapper` occurs when admission fails. That ordering is the
/// executable-page invariant: malformed and W+X images are data forever.
pub fn load<M: Mapper>(
    bytes: &[u8],
    architecture: Architecture,
    mapper: &mut M,
) -> Result<u64, LoadError<M::Error>> {
    let image = Image::parse(bytes, architecture).map_err(LoadError::Image)?;
    for segment in image.segments() {
        let start =
            usize::try_from(segment.file_offset).map_err(|_| LoadError::Image(Error::Segment))?;
        let len =
            usize::try_from(segment.file_size).map_err(|_| LoadError::Image(Error::Segment))?;
        mapper.map(segment, &bytes[start..start + len]).map_err(LoadError::Map)?;
    }
    Ok(image.entry)
}

struct Image {
    segments: [Option<Segment>; SEGMENTS],
    count: usize,
    entry: u64,
}

impl Image {
    fn parse(bytes: &[u8], architecture: Architecture) -> Result<Self, Error> {
        // The one length the image gets to decide. Every field below sits at a
        // constant offset inside this window, so none of them can run off it.
        let header = bytes.first_chunk::<ELF_HEADER>().ok_or(Error::Truncated)?;
        if header[..7] != [0x7f, b'E', b'L', b'F', 2, 1, 1]
            || header.at_le::<u16, 16>() != 2
            || header.at_le::<u32, 20>() != 1
            || header.at_le::<u16, 52>() as usize != ELF_HEADER
        {
            return Err(Error::Format);
        }
        if header.at_le::<u16, 18>() != architecture.machine() {
            return Err(Error::Architecture);
        }

        let entry = header.at_le::<u64, 24>();
        let offset = usize::try_from(header.at_le::<u64, 32>()).map_err(|_| Error::Truncated)?;
        let size = header.at_le::<u16, 54>() as usize;
        let count = header.at_le::<u16, 56>() as usize;
        if size != PROGRAM_HEADER || count == 0 {
            return Err(Error::Format);
        }
        let end = count
            .checked_mul(size)
            .and_then(|span| offset.checked_add(span))
            .ok_or(Error::Truncated)?;
        if end > bytes.len() {
            return Err(Error::Truncated);
        }

        let mut image = Self { segments: [None; SEGMENTS], count: 0, entry };
        let mut prior_end = 0;
        let mut executable_entry = false;
        // `size` was checked against `PROGRAM_HEADER` and `end` against the
        // image, so the table divides exactly and holds `count` of them.
        for header in records::<PROGRAM_HEADER>(&bytes[offset..end]) {
            let kind = header.at_le::<u32, 0>();
            if matches!(kind, DYNAMIC | INTERP) {
                return Err(Error::Unsupported);
            }
            if kind != LOAD {
                continue;
            }
            if image.count == SEGMENTS {
                return Err(Error::TooManySegments);
            }

            let flags = header.at_le::<u32, 4>();
            let file_offset = header.at_le::<u64, 8>();
            let virtual_address = header.at_le::<u64, 16>();
            let file_size = header.at_le::<u64, 32>();
            let memory_size = header.at_le::<u64, 40>();
            let alignment = header.at_le::<u64, 48>();
            let file_end = file_offset.checked_add(file_size).ok_or(Error::Segment)?;
            let memory_end = virtual_address.checked_add(memory_size).ok_or(Error::Segment)?;
            let mapped_end = memory_end
                .checked_add(PAGE - 1)
                .map(|end| end & !(PAGE - 1))
                .ok_or(Error::Segment)?;
            if memory_size == 0
                || file_size > memory_size
                || file_end > bytes.len() as u64
                || virtual_address % PAGE != 0
                || file_offset % PAGE != 0
                || alignment != PAGE
                || flags & !FLAGS != 0
                || flags & 0b100 == 0
            {
                return Err(Error::Segment);
            }
            if flags & (WRITE | EXECUTE) == (WRITE | EXECUTE) {
                return Err(Error::WriteExecute);
            }
            if image.count != 0 && virtual_address < prior_end {
                return Err(Error::Overlap);
            }

            let segment = Segment {
                virtual_address,
                memory_size,
                mapped_size: mapped_end - virtual_address,
                file_offset,
                file_size,
                protection: Protection(flags),
            };
            executable_entry |= segment.protection.is_execute() && segment.contains(entry);
            prior_end = mapped_end;
            image.segments[image.count] = Some(segment);
            image.count += 1;
        }
        if image.count == 0 || !executable_entry {
            return Err(Error::Entry);
        }
        Ok(image)
    }

    fn segments(&self) -> impl Iterator<Item = Segment> + '_ {
        self.segments[..self.count].iter().flatten().copied()
    }
}

#[cfg(test)]
mod tests {
    use std::vec;
    use std::vec::Vec;

    use super::{Architecture, Error, LoadError, Mapper, Segment, load};

    const BASE: u64 = 0x6000_0000_0000;

    fn elf(flags: u32, entry: u64) -> Vec<u8> {
        let mut bytes = vec![0u8; 0x1004];
        bytes[..7].copy_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1]);
        bytes[16..18].copy_from_slice(&2u16.to_le_bytes());
        bytes[18..20].copy_from_slice(&62u16.to_le_bytes());
        bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
        bytes[24..32].copy_from_slice(&entry.to_le_bytes());
        bytes[32..40].copy_from_slice(&64u64.to_le_bytes());
        bytes[52..54].copy_from_slice(&64u16.to_le_bytes());
        bytes[54..56].copy_from_slice(&56u16.to_le_bytes());
        bytes[56..58].copy_from_slice(&1u16.to_le_bytes());
        let header = &mut bytes[64..120];
        header[0..4].copy_from_slice(&1u32.to_le_bytes());
        header[4..8].copy_from_slice(&flags.to_le_bytes());
        header[8..16].copy_from_slice(&0x1000u64.to_le_bytes());
        header[16..24].copy_from_slice(&BASE.to_le_bytes());
        header[24..32].copy_from_slice(&BASE.to_le_bytes());
        header[32..40].copy_from_slice(&4u64.to_le_bytes());
        header[40..48].copy_from_slice(&8u64.to_le_bytes());
        header[48..56].copy_from_slice(&4096u64.to_le_bytes());
        bytes[0x1000..].copy_from_slice(b"molt");
        bytes
    }

    #[derive(Default)]
    struct Recording(Vec<Segment>);

    impl Mapper for Recording {
        type Error = ();

        fn map(&mut self, segment: Segment, file: &[u8]) -> Result<(), ()> {
            assert_eq!(file, b"molt");
            self.0.push(segment);
            Ok(())
        }
    }

    #[test]
    fn valid_static_elf_maps_after_admission() {
        let mut mapped = Recording::default();
        let entry = load(&elf(0b101, BASE), Architecture::X86_64, &mut mapped).unwrap();

        assert_eq!(entry, BASE);
        assert_eq!(mapped.0.len(), 1);
        assert!(mapped.0[0].protection().is_execute());
    }

    #[test]
    fn write_execute_image_is_never_mapped() {
        let mut mapped = Recording::default();
        let rejected = load(&elf(0b111, BASE), Architecture::X86_64, &mut mapped);

        assert_eq!(rejected, Err(LoadError::Image(Error::WriteExecute)));
        assert!(mapped.0.is_empty(), "a rejected image reached the mapper");
    }

    #[test]
    fn entry_must_name_executable_bytes() {
        let mut mapped = Recording::default();
        let rejected = load(&elf(0b100, BASE), Architecture::X86_64, &mut mapped);

        assert_eq!(rejected, Err(LoadError::Image(Error::Entry)));
        assert!(mapped.0.is_empty());
    }

    #[test]
    fn segment_rounding_overflow_is_rejected_before_mapping() {
        let mut image = elf(0b101, BASE);
        image[104..112].copy_from_slice(&(u64::MAX - BASE).to_le_bytes());
        let mut mapped = Recording::default();

        let rejected = load(&image, Architecture::X86_64, &mut mapped);

        assert_eq!(rejected, Err(LoadError::Image(Error::Segment)));
        assert!(mapped.0.is_empty());
    }
}
