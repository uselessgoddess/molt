//! The notification structure: where a queue kick is written.
//!
//! A modern device gives each queue a notification address of
//! `base + notify_off * multiplier`, so one BAR region serves every queue. The
//! driver writes the queue index there to tell the device fresh descriptors are
//! available.

use molt_arch::Mmio;

use crate::VirtioError;

/// The notification structure of one device.
pub struct Notify<'w> {
    window: Mmio<'w>,
    multiplier: u32,
}

impl<'w> Notify<'w> {
    pub const fn new(window: Mmio<'w>, multiplier: u32) -> Self {
        Self { window, multiplier }
    }

    /// Kicks `queue`, whose notification offset the common configuration
    /// reported as `notify_off`.
    pub fn signal(&self, queue: u16, notify_off: u16) -> Result<(), VirtioError> {
        let at = notify_off as u64 * self.multiplier as u64;
        self.window.write_u16(at, queue)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use molt_arch::Mmio;

    use super::Notify;

    #[test]
    fn kick_lands_queues_scaled_offset() {
        let mut registers = [0u8; 32];
        // SAFETY: the slice outlives the borrow and is uniquely borrowed.
        let window = unsafe { Mmio::new(registers.as_mut_ptr(), registers.len() as u64) };
        let notify = Notify::new(window, 4);

        notify.signal(1, 3).expect("a write inside the notify window");

        assert_eq!(&registers[12..14], &1u16.to_le_bytes(), "kick missed offset 3 * 4");
    }
}
