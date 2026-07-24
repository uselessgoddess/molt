//! A disk that loses power after a set number of writes.
//!
//! Crash consistency is a claim about what a device does when the machine dies
//! mid-checkpoint, and the only way to test a claim like that is to make the
//! death happen on purpose. [`Fault`] wraps any [`Disk`], counts the writes and
//! flushes through it, and once a budget runs out drops every later one on the
//! floor — exactly what a power cut does to writes still in flight. Reads keep
//! working, because a crash test's next move is to remount the bytes that did
//! land and prove they still add up to a filesystem.

use crate::{BlockError, Device, Disk};

/// A [`Disk`] that stops taking writes once its budget is spent.
pub struct Fault<D> {
    disk: D,
    /// Writes and flushes still permitted before the power cuts out.
    budget: usize,
    /// Writes and flushes seen, whether they landed or were dropped.
    attempts: usize,
}

impl<D> Fault<D> {
    /// Wraps `disk` so nothing interrupts it, to measure a clean checkpoint.
    pub fn healthy(disk: D) -> Self {
        Self { disk, budget: usize::MAX, attempts: 0 }
    }

    /// Wraps `disk` so the power cuts out after `budget` writes or flushes.
    pub fn after(disk: D, budget: usize) -> Self {
        Self { disk, budget, attempts: 0 }
    }

    /// How many writes and flushes have been tried, dropped ones included.
    ///
    /// A clean run reports the number of crash points a checkpoint has, which
    /// is the range a test sweeps its budget over.
    pub fn attempts(&self) -> usize {
        self.attempts
    }

    /// Hands the wrapped disk back, as a crash test does to remount it.
    pub fn into_inner(self) -> D {
        self.disk
    }
}

impl<D: Device> Device for Fault<D> {
    fn sectors(&self) -> u64 {
        self.disk.sectors()
    }

    fn read(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        self.disk.read(sector, buf)
    }
}

impl<D: Disk> Disk for Fault<D> {
    fn write(&mut self, sector: u64, buf: &[u8]) -> Result<(), BlockError> {
        self.attempts += 1;
        if self.budget == 0 {
            return Err(BlockError::Device);
        }
        self.budget -= 1;
        self.disk.write(sector, buf)
    }

    fn flush(&mut self) -> Result<(), BlockError> {
        self.attempts += 1;
        if self.budget == 0 {
            return Err(BlockError::Device);
        }
        self.budget -= 1;
        self.disk.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::Fault;
    use crate::{BlockError, Device, Disk, Loopback, SECTOR};

    #[test]
    fn writes_up_to_budget_land() {
        let mut disk = Fault::after(Loopback::new([0u8; 2 * SECTOR]).expect("whole sectors"), 1);

        assert_eq!(disk.write(0, &[0xa5; SECTOR]), Ok(()));
        assert_eq!(disk.write(1, &[0xa5; SECTOR]), Err(BlockError::Device));
    }

    #[test]
    fn dropped_write_never_reaches_disk() {
        let mut disk = Fault::after(Loopback::new([0u8; SECTOR]).expect("whole sectors"), 0);

        assert_eq!(disk.write(0, &[0xa5; SECTOR]), Err(BlockError::Device));
        assert_eq!(disk.into_inner().into_inner(), [0u8; SECTOR], "a lost write still landed");
    }

    #[test]
    fn attempts_count_every_write_and_flush() {
        let mut disk = Fault::healthy(Loopback::new([0u8; SECTOR]).expect("whole sectors"));

        disk.write(0, &[0; SECTOR]).expect("healthy write");
        disk.flush().expect("healthy flush");

        assert_eq!(disk.attempts(), 2, "a flush is a crash point too");
    }

    #[test]
    fn reads_survive_the_power_loss() {
        let mut disk = Fault::after(Loopback::new([0xa5u8; SECTOR]).expect("whole sectors"), 0);

        let mut sector = [0u8; SECTOR];
        disk.read(0, &mut sector).expect("a crashed disk still reads");

        assert_eq!(sector, [0xa5; SECTOR], "the read came back changed");
    }
}
