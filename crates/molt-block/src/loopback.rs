//! A device backed by bytes that are already in memory.

use crate::{BlockError, Device, SECTOR, Writable, bounds};

/// Storage read straight out of an image the caller holds.
///
/// This is what a filesystem test runs on, and what a kernel serves a built-in
/// image from: the same [`Device`] the virtio driver offers, with none of the
/// hardware. It borrows rather than owns, so `molt-block` needs no allocator.
pub struct Loopback<'i> {
    image: Image<'i>,
}

enum Image<'i> {
    ReadOnly(&'i [u8]),
    Writable(&'i mut [u8]),
}

impl Image<'_> {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::ReadOnly(bytes) => bytes,
            Self::Writable(bytes) => bytes,
        }
    }

    fn bytes_mut(&mut self) -> Result<&mut [u8], BlockError> {
        match self {
            Self::ReadOnly(_) => Err(BlockError::ReadOnly),
            Self::Writable(bytes) => Ok(bytes),
        }
    }
}

impl<'i> Loopback<'i> {
    /// Wraps `image`, which must be a whole number of sectors.
    pub fn new(image: &'i [u8]) -> Result<Self, BlockError> {
        if image.len() % SECTOR != 0 {
            return Err(BlockError::Unaligned);
        }
        Ok(Self { image: Image::ReadOnly(image) })
    }

    /// Wraps mutable storage, which must be a whole number of sectors.
    pub fn writable(image: &'i mut [u8]) -> Result<Self, BlockError> {
        if image.len() % SECTOR != 0 {
            return Err(BlockError::Unaligned);
        }
        Ok(Self { image: Image::Writable(image) })
    }
}

impl Device for Loopback<'_> {
    fn sectors(&self) -> u64 {
        (self.image.bytes().len() / SECTOR) as u64
    }

    fn read(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        bounds(self.sectors(), sector, buf)?;
        let at = sector as usize * SECTOR;
        buf.copy_from_slice(&self.image.bytes()[at..at + buf.len()]);
        Ok(())
    }
}

impl Writable for Loopback<'_> {
    fn write(&mut self, sector: u64, buf: &[u8]) -> Result<(), BlockError> {
        bounds(self.sectors(), sector, buf)?;
        let at = sector as usize * SECTOR;
        self.image.bytes_mut()?[at..at + buf.len()].copy_from_slice(buf);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), BlockError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Loopback;
    use crate::{BlockError, Device, SECTOR, Writable};

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
    fn borrowed_device_reads_like_owned() {
        fn first_sector(mut device: impl Device) -> [u8; SECTOR] {
            let mut sector = [0u8; SECTOR];
            device.read(0, &mut sector).expect("first sector");
            sector
        }

        let image = [0xa5u8; SECTOR];
        let mut device = Loopback::new(&image).expect("whole sectors");

        assert_eq!(first_sector(&mut device), image);
        assert_eq!(device.sectors(), 1, "lending it back does not consume it");
    }

    #[test]
    fn partial_image_refused() {
        assert!(matches!(Loopback::new(&[0; SECTOR + 1]), Err(BlockError::Unaligned)));
    }

    #[test]
    fn sector_write_survives_flush() {
        let mut image = [0u8; 2 * SECTOR];
        let written = [0xa5; SECTOR];
        let mut device = Loopback::writable(&mut image).expect("whole sectors");

        device.write(1, &written).expect("writable sector");
        device.flush().expect("durable write");
        let mut read = [0u8; SECTOR];
        device.read(1, &mut read).expect("same sector");

        assert_eq!(read, written);
    }
}
