//! A device backed by bytes that are already in memory.

use crate::{BlockError, Device, SECTOR, bounds};

/// Storage read straight out of an image the caller holds.
///
/// This is what a filesystem test runs on, and what a kernel serves a built-in
/// image from: the same [`Device`] the virtio driver offers, with none of the
/// hardware. It borrows rather than owns, so `molt-block` needs no allocator.
pub struct Loopback<'image> {
    image: &'image [u8],
}

impl<'image> Loopback<'image> {
    /// Wraps `image`, which must be a whole number of sectors.
    pub fn new(image: &'image [u8]) -> Result<Self, BlockError> {
        if image.len() % SECTOR != 0 {
            return Err(BlockError::Unaligned);
        }
        Ok(Self { image })
    }
}

impl Device for Loopback<'_> {
    fn sectors(&self) -> u64 {
        (self.image.len() / SECTOR) as u64
    }

    fn read(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        bounds(self.sectors(), sector, buf)?;
        let at = sector as usize * SECTOR;
        buf.copy_from_slice(&self.image[at..at + buf.len()]);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Loopback;
    use crate::{BlockError, Device, SECTOR};

    #[test]
    fn sector_reads_back_what_image_holds() {
        let mut image = [0u8; 2 * SECTOR];
        image[SECTOR] = 0xa5;

        let mut device = Loopback::new(&image).expect("whole sectors");
        let mut sector = [0u8; SECTOR];
        device.read(1, &mut sector).expect("second sector");

        assert_eq!(sector[0], 0xa5, "the second sector read back as the first");
    }

    #[test]
    fn sectors_count_image_length() {
        let image = [0u8; 4 * SECTOR];

        assert_eq!(Loopback::new(&image).expect("whole sectors").sectors(), 4);
    }

    #[test]
    fn read_past_end_refused() {
        let image = [0u8; SECTOR];
        let mut device = Loopback::new(&image).expect("whole sectors");

        assert_eq!(device.read(1, &mut [0; SECTOR]), Err(BlockError::Range));
    }

    #[test]
    fn partial_image_refused() {
        assert!(matches!(Loopback::new(&[0; SECTOR + 1]), Err(BlockError::Unaligned)));
    }
}
