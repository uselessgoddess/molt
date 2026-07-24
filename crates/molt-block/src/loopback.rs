//! A device backed by bytes that are already in memory.

use crate::{BlockError, Device, Disk, SECTOR, bounds};

/// Storage read straight out of an image the caller holds.
///
/// This is what a filesystem test runs on, and what a kernel serves a built-in
/// image from: the same [`Device`] the virtio driver offers, with none of the
/// hardware. The backing is whatever holds the bytes — a borrowed `&[u8]` for
/// a read-only image, an owned `Vec<u8>` or `&mut [u8]` for one a checkpoint
/// writes — so `molt-block` still needs no allocator of its own.
pub struct Loopback<B> {
    image: B,
}

impl<B: AsRef<[u8]>> Loopback<B> {
    /// Wraps `image`, which must be a whole number of sectors.
    pub fn new(image: B) -> Result<Self, BlockError> {
        if image.as_ref().len() % SECTOR != 0 {
            return Err(BlockError::Unaligned);
        }
        Ok(Self { image })
    }

    /// Hands the backing bytes back, as a crash test does to remount them.
    pub fn into_inner(self) -> B {
        self.image
    }
}

impl<B: AsRef<[u8]>> Device for Loopback<B> {
    fn sectors(&self) -> u64 {
        (self.image.as_ref().len() / SECTOR) as u64
    }

    fn read(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        bounds(self.sectors(), sector, buf)?;
        let at = sector as usize * SECTOR;
        buf.copy_from_slice(&self.image.as_ref()[at..at + buf.len()]);
        Ok(())
    }
}

impl<B: AsRef<[u8]> + AsMut<[u8]>> Disk for Loopback<B> {
    fn write(&mut self, sector: u64, buf: &[u8]) -> Result<(), BlockError> {
        bounds(self.sectors(), sector, buf)?;
        let at = sector as usize * SECTOR;
        self.image.as_mut()[at..at + buf.len()].copy_from_slice(buf);
        Ok(())
    }

    // Memory is durable the instant it is written, so ordering is already kept.
    fn flush(&mut self) -> Result<(), BlockError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Loopback;
    use crate::{BlockError, Device, Disk, SECTOR};

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
    fn write_lands_where_a_read_finds_it() {
        let mut device = Loopback::new([0u8; 2 * SECTOR]).expect("whole sectors");

        device.write(1, &[0xa5; SECTOR]).expect("second sector");
        let mut sector = [0u8; SECTOR];
        device.read(1, &mut sector).expect("second sector");

        assert_eq!(sector, [0xa5; SECTOR], "the write did not reach the sector");
    }

    #[test]
    fn write_past_end_refused() {
        let mut device = Loopback::new([0u8; SECTOR]).expect("whole sectors");

        assert_eq!(device.write(1, &[0; SECTOR]), Err(BlockError::Range));
    }

    #[test]
    fn partial_sector_write_refused() {
        let mut device = Loopback::new([0u8; SECTOR]).expect("whole sectors");

        assert_eq!(device.write(0, &[0; SECTOR + 1]), Err(BlockError::Unaligned));
    }
}
