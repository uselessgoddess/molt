//! Typed records in the append-only mutation log.

use crate::layout::{Kind, MAX_NAME};
use crate::{FsError, Name};

/// Every record starts on a sector so one device write never tears two records.
pub const ALIGN: u64 = molt_block::SECTOR as u64;

/// Bytes before a record's name or file data.
pub const HEADER: usize = 32;

const MAGIC: [u8; 4] = *b"MLOG";
const CREATE: u8 = 1;
const WRITE: u8 = 2;

/// One mutation header. Its payload follows immediately.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Record {
    Create { object: u32, parent: u32, kind: Kind, name_len: u16 },
    Write { object: u32, offset: u64, bytes: u32 },
}

impl Record {
    pub fn create(object: u32, parent: u32, kind: Kind, name: Name) -> Self {
        Self::Create { object, parent, kind, name_len: name.len() as u16 }
    }

    pub fn write(object: u32, offset: u64, bytes: usize) -> Result<Self, FsError> {
        Ok(Self::Write { object, offset, bytes: u32::try_from(bytes).map_err(|_| FsError::Range)? })
    }

    pub const fn payload(self) -> u32 {
        match self {
            Self::Create { name_len, .. } => name_len as u32,
            Self::Write { bytes, .. } => bytes,
        }
    }

    pub fn span(self) -> Result<u64, FsError> {
        let bytes = (HEADER as u64).checked_add(u64::from(self.payload())).ok_or(FsError::Range)?;
        bytes.checked_next_multiple_of(ALIGN).ok_or(FsError::Range)
    }

    pub fn encode(self, header: &mut [u8; HEADER]) {
        header.fill(0);
        header[..MAGIC.len()].copy_from_slice(&MAGIC);
        match self {
            Self::Create { object, parent, kind, name_len } => {
                header[4] = CREATE;
                header[5] = kind.byte();
                put_u16(header, 6, name_len);
                put_u32(header, 8, u32::from(name_len));
                put_u32(header, 12, object);
                put_u32(header, 16, parent);
            }
            Self::Write { object, offset, bytes } => {
                header[4] = WRITE;
                put_u32(header, 8, bytes);
                put_u32(header, 12, object);
                put_u64(header, 20, offset);
            }
        }
    }

    pub fn parse(header: &[u8]) -> Result<Self, FsError> {
        let header = header.get(..HEADER).ok_or(FsError::Corrupt)?;
        if header[..MAGIC.len()] != MAGIC {
            return Err(FsError::Corrupt);
        }
        let payload = u32_at(header, 8);
        let object = u32_at(header, 12);
        match header[4] {
            CREATE => {
                let name_len = u16_at(header, 6);
                let kind = match header[5] {
                    0 => Kind::Dir,
                    1 => Kind::File,
                    _ => return Err(FsError::Corrupt),
                };
                if name_len == 0 || name_len as usize > MAX_NAME || payload != u32::from(name_len) {
                    return Err(FsError::Corrupt);
                }
                Ok(Self::Create { object, parent: u32_at(header, 16), kind, name_len })
            }
            WRITE if payload != 0 => {
                Ok(Self::Write { object, offset: u64_at(header, 20), bytes: payload })
            }
            _ => Err(FsError::Corrupt),
        }
    }
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(bytes[at..at + 2].try_into().expect("fixed field"))
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("fixed field"))
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("fixed field"))
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
    use super::{HEADER, Record};
    use crate::{Kind, Name};

    #[test]
    fn create_header_survives_round_trip() {
        let name = Name::try_from("note").expect("legal name");
        let record = Record::create(4, 1, Kind::File, name);
        let mut header = [0; HEADER];

        record.encode(&mut header);

        assert_eq!(Record::parse(&header), Ok(record));
    }

    #[test]
    fn write_span_covers_header_data_and_padding() {
        let record = Record::write(3, 7, 600).expect("bounded payload");

        assert_eq!(record.span(), Ok(1024));
    }
}
