//! A device backed by bytes that are already in memory.

use crate::{BlockError, Device, Disk, SECTOR, bounds};

/// Storage read straight out of an image the caller holds.
///
/// This is what a filesystem test runs on, and what a kernel serves a built-in
/// image from: the same [`Device`] the virtio driver offers, with none of the
/// hardware. It borrows rather than owns, so the image stays wherever the
/// caller put it — a static in the kernel, a slice on a test's stack.
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

impl Disk for Loopback<'_> {
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
    use crate::{BlockError, Device, Disk, SECTOR};

    #[test]
    fn sector_reads_back_what_image_holds() -> Result<(), BlockError> {
        let mut image = [0u8; 2 * SECTOR];
        image[SECTOR] = 0xa5;

        let mut device = Loopback::new(&image)?;
        let mut sector = [0u8; SECTOR];
        device.read(1, &mut sector)?;

        assert_eq!(sector[0], 0xa5, "the second sector read back as the first");
        Ok(())
    }

    #[test]
    fn sectors_count_image_length() -> Result<(), BlockError> {
        let image = [0u8; 4 * SECTOR];

        assert_eq!(Loopback::new(&image)?.sectors(), 4);
        Ok(())
    }

    #[test]
    fn read_past_end_refused() -> Result<(), BlockError> {
        let image = [0u8; SECTOR];
        let mut device = Loopback::new(&image)?;

        assert_eq!(device.read(1, &mut [0; SECTOR]), Err(BlockError::Range));
        Ok(())
    }

    #[test]
    fn borrowed_device_reads_like_owned() -> Result<(), BlockError> {
        fn first_sector(mut device: impl Device) -> [u8; SECTOR] {
            let mut sector = [0u8; SECTOR];
            device.read(0, &mut sector).unwrap();
            sector
        }

        let image = [0xa5u8; SECTOR];
        let mut device = Loopback::new(&image)?;

        assert_eq!(first_sector(&mut device), image);
        assert_eq!(device.sectors(), 1, "lending it back does not consume it");
        Ok(())
    }

    #[test]
    fn partial_image_refused() {
        assert!(matches!(Loopback::new(&[0; SECTOR + 1]), Err(BlockError::Unaligned)));
    }

    #[test]
    fn sector_write_survives_flush() -> Result<(), BlockError> {
        let mut image = [0u8; 2 * SECTOR];
        let written = [0xa5; SECTOR];
        let mut device = Loopback::writable(&mut image)?;

        device.write(1, &written)?;
        device.flush()?;
        let mut read = [0u8; SECTOR];
        device.read(1, &mut read)?;

        assert_eq!(read, written);
        Ok(())
    }
}
