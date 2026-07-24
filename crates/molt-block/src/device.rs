//! The read side of storage, and the bounds check every implementor owes.

use crate::{BlockError, SECTOR};

/// Sector-addressed storage a filesystem reads through.
///
/// A read is all-or-nothing: it either fills `buf` completely or fails. Short
/// reads would force every caller above to carry a resume loop for a case that
/// only a broken device produces.
pub trait Device {
    /// How many sectors the device holds.
    fn sectors(&self) -> u64;

    /// Reads consecutive sectors starting at `sector` into `buf`.
    ///
    /// Fails with [`BlockError::Unaligned`] unless `buf` is a whole number of
    /// sectors, and with [`BlockError::Range`] if the request would leave the
    /// device. Implementors get both checks from [`bounds`].
    fn read(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), BlockError>;
}

impl<D: Device + ?Sized> Device for &mut D {
    fn sectors(&self) -> u64 {
        (**self).sectors()
    }

    fn read(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        (**self).read(sector, buf)
    }
}

/// Checks `buf` against the device geometry, returning the sectors it spans.
pub fn bounds(sectors: u64, sector: u64, buf: &[u8]) -> Result<u64, BlockError> {
    if buf.len() % SECTOR != 0 {
        return Err(BlockError::Unaligned);
    }
    let span = (buf.len() / SECTOR) as u64;
    let end = sector.checked_add(span).ok_or(BlockError::Range)?;
    if end > sectors {
        return Err(BlockError::Range);
    }
    Ok(span)
}

#[cfg(test)]
mod tests {
    use super::bounds;
    use crate::{BlockError, SECTOR};

    #[test]
    fn whole_sectors_span_their_count() {
        assert_eq!(bounds(8, 4, &[0; 2 * SECTOR]), Ok(2));
    }

    #[test]
    fn partial_sector_refused() {
        assert_eq!(bounds(8, 0, &[0; SECTOR + 1]), Err(BlockError::Unaligned));
    }

    #[test]
    fn read_past_end_refused() {
        assert_eq!(bounds(8, 7, &[0; 2 * SECTOR]), Err(BlockError::Range));
    }

    #[test]
    fn sector_overflow_refused() {
        assert_eq!(bounds(8, u64::MAX, &[0; SECTOR]), Err(BlockError::Range));
    }
}
