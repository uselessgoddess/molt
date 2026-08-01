//! File payload records in the append-only checkpoint log.

use crate::FsError;

/// Every record starts on a sector so one device write never tears two records.
pub const ALIGN: u64 = molt_block::SECTOR as u64;

/// Bytes before a record's name or file data.
pub const HEADER: usize = 32;

const MAGIC: [u8; 4] = *b"MLOG";
const WRITE: u8 = 2;

/// One file payload header. Its bytes follow immediately.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Record {
    pub object: u32,
    pub offset: u64,
    pub bytes: u32,
}

impl Record {
    pub fn write(object: u32, offset: u64, bytes: usize) -> Result<Self, FsError> {
        let bytes = u32::try_from(bytes).map_err(|_| FsError::Range)?;
        if bytes == 0 {
            return Err(FsError::Range);
        }
        Ok(Self { object, offset, bytes })
    }

    pub const fn payload(self) -> u32 {
        self.bytes
    }

    pub fn span(self) -> Result<u64, FsError> {
        let bytes = (HEADER as u64).checked_add(u64::from(self.payload())).ok_or(FsError::Range)?;
        bytes.checked_next_multiple_of(ALIGN).ok_or(FsError::Range)
    }

    pub fn encode(self, header: &mut [u8; HEADER]) {
        header.fill(0);
        header[..MAGIC.len()].copy_from_slice(&MAGIC);
        header[4] = WRITE;
        put_u32(header, 8, self.bytes);
        put_u32(header, 12, self.object);
        put_u64(header, 20, self.offset);
    }

    pub fn parse(header: &[u8]) -> Result<Self, FsError> {
        let header = header.get(..HEADER).ok_or(FsError::Corrupt)?;
        if header[..MAGIC.len()] != MAGIC {
            return Err(FsError::Corrupt);
        }
        let payload = u32_at(header, 8);
        if header[4] != WRITE || payload == 0 {
            return Err(FsError::Corrupt);
        }
        Ok(Self { object: u32_at(header, 12), offset: u64_at(header, 20), bytes: payload })
    }
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("fixed field"))
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("fixed field"))
}

fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::Record;
    use crate::FsError;

    #[test]
    fn write_span_covers_padding() -> Result<(), FsError> {
        let record = Record::write(3, 7, 600)?;

        assert_eq!(record.span(), Ok(1024));
        Ok(())
    }
}
