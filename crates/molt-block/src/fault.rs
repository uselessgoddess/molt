//! A device that models a volatile write-back cache, so a test can cut power.
//!
//! [`Fault`] wraps any [`Write`] device and keeps every write in a caller-owned
//! cache until [`flush`](Write::flush) commits it. That is the one thing a plain
//! [`Loopback`](crate::Loopback) cannot show: a write is durable only once it is
//! flushed, so [`crash`](Fault::crash) — power lost between flushes — drops the
//! cache, and [`cut_after`](Fault::cut_after) stops a flush partway, the torn
//! write a checkpoint must survive. It allocates nothing; the cache is a slice
//! of [`Line`] the caller supplies.

use crate::{BlockError, Device, SECTOR, Write, bounds};

/// One buffered sector write, the storage a [`Fault`] cache is built from.
#[derive(Clone, Copy)]
pub struct Line {
    sector: u64,
    data: [u8; SECTOR],
}

impl Line {
    /// An empty line, for laying out the `[Line; N]` a [`Fault`] borrows.
    pub const EMPTY: Self = Self { sector: 0, data: [0; SECTOR] };
}

/// A [`Write`] device whose writes are volatile until they are flushed.
///
/// The cache holds one line per distinct sector written since the last flush,
/// so it must have room for every sector a checkpoint touches between flushes;
/// a write past that returns [`BlockError::Device`], the backpressure a real
/// cache would apply. Reads see the cache over the disk, as the running system
/// sees its own not-yet-flushed writes.
pub struct Fault<'c, D> {
    inner: D,
    cache: &'c mut [Line],
    len: usize,
    power: Option<u64>,
    reorder: bool,
    dead: bool,
}

impl<'c, D: Write> Fault<'c, D> {
    /// Wraps `inner`, caching writes in `cache` until a flush.
    pub fn new(inner: D, cache: &'c mut [Line]) -> Self {
        Self { inner, cache, len: 0, power: None, reorder: false, dead: false }
    }

    /// Lets `count` sectors reach the disk on the next flush before power dies.
    ///
    /// The flush commits its first `count` sectors, then fails and loses the
    /// rest — the torn write a crash leaves at every point of a checkpoint.
    pub fn cut_after(&mut self, count: u64) {
        self.power = Some(count);
    }

    /// Commits the cache back to front on flush, the reordering a device is free
    /// to do between two flushes.
    pub fn reorder(&mut self) {
        self.reorder = true;
    }

    /// Drops every unflushed write, as a power loss between flushes does.
    pub fn crash(&mut self) {
        self.len = 0;
    }

    /// How many sectors are waiting for a flush.
    pub fn dirty(&self) -> usize {
        self.len
    }

    fn stash(&mut self, sector: u64, chunk: &[u8]) -> Result<(), BlockError> {
        for line in &mut self.cache[..self.len] {
            if line.sector == sector {
                line.data.copy_from_slice(chunk);
                return Ok(());
            }
        }
        let line = self.cache.get_mut(self.len).ok_or(BlockError::Device)?;
        line.sector = sector;
        line.data.copy_from_slice(chunk);
        self.len += 1;
        Ok(())
    }
}

impl<D: Write> Device for Fault<'_, D> {
    fn sectors(&self) -> u64 {
        self.inner.sectors()
    }

    fn read(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        let span = bounds(self.sectors(), sector, buf)?;
        self.inner.read(sector, buf)?;
        for step in 0..span {
            let at = step as usize * SECTOR;
            if let Some(line) = self.cache[..self.len].iter().find(|l| l.sector == sector + step) {
                buf[at..at + SECTOR].copy_from_slice(&line.data);
            }
        }
        Ok(())
    }
}

impl<D: Write> Write for Fault<'_, D> {
    fn write(&mut self, sector: u64, buf: &[u8]) -> Result<(), BlockError> {
        if self.dead {
            return Err(BlockError::Device);
        }
        let span = bounds(self.sectors(), sector, buf)?;
        for step in 0..span {
            let at = step as usize * SECTOR;
            self.stash(sector + step, &buf[at..at + SECTOR])?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), BlockError> {
        if self.dead {
            return Err(BlockError::Device);
        }
        for step in 0..self.len {
            if self.power == Some(0) {
                // Power died mid-flush: the rest of the cache never lands.
                self.dead = true;
                self.len = 0;
                return Err(BlockError::Device);
            }
            let index = if self.reorder { self.len - 1 - step } else { step };
            let line = self.cache[index];
            self.inner.write(line.sector, &line.data)?;
            if let Some(power) = &mut self.power {
                *power -= 1;
            }
        }
        self.len = 0;
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::{Fault, Line};
    use crate::{Device, Loopback, SECTOR, Write};

    fn disk() -> [u8; 4 * SECTOR] {
        [0u8; 4 * SECTOR]
    }

    #[test]
    fn unflushed_write_lost_on_crash() {
        let mut image = disk();
        let mut cache = [Line::EMPTY; 4];
        let mut device = Fault::new(Loopback::new(&mut image[..]).unwrap(), &mut cache);

        device.write(1, &[0xa5; SECTOR]).expect("cached write");
        device.crash();

        let mut read = [0xffu8; SECTOR];
        device.read(1, &mut read).expect("read");
        assert_eq!(read, [0; SECTOR], "an unflushed write reached the disk");
    }

    #[test]
    fn flushed_write_survives_crash() {
        let mut image = disk();
        let mut cache = [Line::EMPTY; 4];
        let mut device = Fault::new(Loopback::new(&mut image[..]).unwrap(), &mut cache);

        device.write(1, &[0xa5; SECTOR]).expect("cached write");
        device.flush().expect("flush");
        device.crash();

        let mut read = [0u8; SECTOR];
        device.read(1, &mut read).expect("read");
        assert_eq!(read, [0xa5; SECTOR], "a flushed write did not survive");
    }

    #[test]
    fn read_sees_own_unflushed_write() {
        let mut image = disk();
        let mut cache = [Line::EMPTY; 4];
        let mut device = Fault::new(Loopback::new(&mut image[..]).unwrap(), &mut cache);

        device.write(2, &[0x5a; SECTOR]).expect("cached write");

        let mut read = [0u8; SECTOR];
        device.read(2, &mut read).expect("read");
        assert_eq!(read, [0x5a; SECTOR], "the cache was not read back over the disk");
    }

    #[test]
    fn cut_flush_lands_a_prefix_only() {
        let mut image = disk();
        let mut cache = [Line::EMPTY; 4];
        let mut device = Fault::new(Loopback::new(&mut image[..]).unwrap(), &mut cache);
        device.write(0, &[0x11; 2 * SECTOR]).expect("two cached sectors");

        device.cut_after(1);
        assert_eq!(device.flush(), Err(crate::BlockError::Device), "the cut flush reported ok");

        assert_eq!(&image[..SECTOR], &[0x11; SECTOR], "the first sector was not durable");
        assert_eq!(&image[SECTOR..2 * SECTOR], &[0; SECTOR], "the cut sector still landed");
    }

    #[test]
    fn reorder_commits_back_to_front() {
        let mut image = disk();
        let mut cache = [Line::EMPTY; 4];
        let mut device = Fault::new(Loopback::new(&mut image[..]).unwrap(), &mut cache);
        device.write(0, &[0x11; SECTOR]).expect("cached");
        device.write(1, &[0x22; SECTOR]).expect("cached");

        device.reorder();
        device.cut_after(1);
        let _ = device.flush();

        assert_eq!(&image[SECTOR..2 * SECTOR], &[0x22; SECTOR], "the later write did not go first");
        assert_eq!(&image[..SECTOR], &[0; SECTOR], "the earlier write was not the one dropped");
    }

    #[test]
    fn full_cache_refuses_rather_than_overwrites() {
        let mut image = disk();
        let mut cache = [Line::EMPTY; 1];
        let mut device = Fault::new(Loopback::new(&mut image[..]).unwrap(), &mut cache);

        device.write(0, &[0; SECTOR]).expect("first sector fits");

        assert_eq!(device.write(1, &[0; SECTOR]), Err(crate::BlockError::Device));
    }
}
