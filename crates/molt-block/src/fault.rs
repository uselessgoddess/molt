//! Volatile memory storage with deterministic power-loss injection.

use crate::{BlockError, Device, SECTOR, Writable, bounds};

/// Storage that keeps writes volatile until flush and can cut power at one
/// device action.
///
/// `stable` models bytes that survive a crash; `volatile` models a controller
/// cache. A write changes only `volatile`, while a successful flush copies the
/// full cache to `stable`. [`cut_after`](Self::cut_after) makes the operation
/// after that many successful writes or flushes fail with
/// [`BlockError::PowerLoss`].
pub struct Fault<'a> {
    stable: &'a mut [u8],
    volatile: &'a mut [u8],
    cut: Option<u64>,
    steps: u64,
    failed: bool,
}

impl<'a> Fault<'a> {
    /// Starts with `volatile` equal to `stable`.
    pub fn new(stable: &'a mut [u8], volatile: &'a mut [u8]) -> Result<Self, BlockError> {
        if stable.len() != volatile.len() || stable.len() % SECTOR != 0 {
            return Err(BlockError::Unaligned);
        }
        volatile.copy_from_slice(stable);
        Ok(Self { stable, volatile, cut: None, steps: 0, failed: false })
    }

    /// Cuts power before the action after `steps` successful actions.
    pub fn cut_after(mut self, steps: u64) -> Self {
        self.cut = Some(steps);
        self
    }

    /// How many writes and flushes completed.
    pub const fn steps(&self) -> u64 {
        self.steps
    }

    fn action(&mut self) -> Result<(), BlockError> {
        if self.failed || self.cut == Some(self.steps) {
            self.failed = true;
            return Err(BlockError::PowerLoss);
        }
        self.steps += 1;
        Ok(())
    }
}

impl Device for Fault<'_> {
    fn sectors(&self) -> u64 {
        (self.volatile.len() / SECTOR) as u64
    }

    fn read(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        bounds(self.sectors(), sector, buf)?;
        let at = sector as usize * SECTOR;
        buf.copy_from_slice(&self.volatile[at..at + buf.len()]);
        Ok(())
    }
}

impl Writable for Fault<'_> {
    fn write(&mut self, sector: u64, buf: &[u8]) -> Result<(), BlockError> {
        bounds(self.sectors(), sector, buf)?;
        self.action()?;
        let at = sector as usize * SECTOR;
        self.volatile[at..at + buf.len()].copy_from_slice(buf);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), BlockError> {
        self.action()?;
        self.stable.copy_from_slice(self.volatile);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Fault;
    use crate::{BlockError, SECTOR, Writable};

    #[test]
    fn unflushed_write_is_not_durable() {
        let mut stable = [0u8; SECTOR];
        let mut volatile = [0u8; SECTOR];
        {
            let mut device = Fault::new(&mut stable, &mut volatile).expect("matching storage");
            device.write(0, &[0xa5; SECTOR]).expect("volatile write");
        }

        assert_eq!(stable, [0; SECTOR]);
    }

    #[test]
    fn flush_makes_write_durable() {
        let mut stable = [0u8; SECTOR];
        let mut volatile = [0u8; SECTOR];
        {
            let mut device = Fault::new(&mut stable, &mut volatile).expect("matching storage");
            device.write(0, &[0xa5; SECTOR]).expect("volatile write");
            device.flush().expect("durable write");
        }

        assert_eq!(stable, [0xa5; SECTOR]);
    }

    #[test]
    fn cut_refuses_selected_action() {
        let mut stable = [0u8; SECTOR];
        let mut volatile = [0u8; SECTOR];
        let mut device =
            Fault::new(&mut stable, &mut volatile).expect("matching storage").cut_after(1);

        assert_eq!(device.write(0, &[1; SECTOR]), Ok(()));
        assert_eq!(device.flush(), Err(BlockError::PowerLoss));
    }
}
