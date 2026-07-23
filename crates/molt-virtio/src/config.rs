//! The common configuration structure: the registers that bring a device up
//! and program its queues.
//!
//! Every field is at a fixed offset the specification numbers, so the register
//! names live here and the rest of the driver speaks in methods. The one piece
//! of policy is [`negotiate`](Common::negotiate): the driver always demands
//! `VIRTIO_F_VERSION_1`, because a device that will not speak the modern
//! transport is not one this crate drives.

use molt_arch::Mmio;

use crate::VirtioError;

/// Common-configuration register offsets (VirtIO 1.x, §4.1.4.3).
mod register {
    pub const DEVICE_FEATURE_SELECT: u64 = 0x00;
    pub const DEVICE_FEATURE: u64 = 0x04;
    pub const DRIVER_FEATURE_SELECT: u64 = 0x08;
    pub const DRIVER_FEATURE: u64 = 0x0c;
    pub const NUM_QUEUES: u64 = 0x12;
    pub const DEVICE_STATUS: u64 = 0x14;
    pub const QUEUE_SELECT: u64 = 0x16;
    pub const QUEUE_SIZE: u64 = 0x18;
    pub const QUEUE_ENABLE: u64 = 0x1c;
    pub const QUEUE_NOTIFY_OFF: u64 = 0x1e;
    pub const QUEUE_DESC: u64 = 0x20;
    pub const QUEUE_DRIVER: u64 = 0x28;
    pub const QUEUE_DEVICE: u64 = 0x30;
}

/// Device-status bits written during the handshake (§2.1).
pub mod status {
    pub const ACKNOWLEDGE: u8 = 1;
    pub const DRIVER: u8 = 2;
    pub const DRIVER_OK: u8 = 4;
    pub const FEATURES_OK: u8 = 8;
}

/// The feature bit that says the device speaks the modern transport (§6).
pub const VERSION_1: u64 = 1 << 32;

/// How long to spin for the device to acknowledge a reset before giving up.
const RESET_SPINS: u32 = 1_000_000;

/// The common configuration structure of one device.
pub struct Common<'w> {
    window: Mmio<'w>,
}

impl<'w> Common<'w> {
    pub const fn new(window: Mmio<'w>) -> Self {
        Self { window }
    }

    /// Resets the device and waits for it to report a zero status.
    ///
    /// Writing zero starts the reset; the device clears the status when it is
    /// done, so a driver that programmed queues before the reset settled would
    /// be talking to a device that had not finished forgetting the old ones.
    pub fn reset(&mut self) -> Result<(), VirtioError> {
        self.set_status(0)?;
        for _ in 0..RESET_SPINS {
            if self.status()? == 0 {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err(VirtioError::Device)
    }

    pub fn status(&self) -> Result<u8, VirtioError> {
        Ok(self.window.read_u8(register::DEVICE_STATUS)?)
    }

    pub fn set_status(&mut self, bits: u8) -> Result<(), VirtioError> {
        self.window.write_u8(register::DEVICE_STATUS, bits)?;
        Ok(())
    }

    /// Adds `bits` to the device status without disturbing the rest.
    pub fn add_status(&mut self, bits: u8) -> Result<(), VirtioError> {
        let status = self.status()?;
        self.set_status(status | bits)
    }

    /// Accepts `VIRTIO_F_VERSION_1` plus whatever of `wanted` the device
    /// offers, and confirms the device is content with the choice.
    ///
    /// A device that does not offer the modern transport, or that clears
    /// `FEATURES_OK` after the driver writes it, is refused: the driver cannot
    /// fall back to the legacy interface.
    pub fn negotiate(&mut self, wanted: u64) -> Result<u64, VirtioError> {
        let offered = self.device_features()?;
        if offered & VERSION_1 == 0 {
            return Err(VirtioError::Features);
        }
        let accepted = offered & (wanted | VERSION_1);
        self.write_driver_features(accepted)?;

        self.add_status(status::FEATURES_OK)?;
        if self.status()? & status::FEATURES_OK == 0 {
            return Err(VirtioError::Features);
        }
        Ok(accepted)
    }

    fn device_features(&mut self) -> Result<u64, VirtioError> {
        self.window.write_u32(register::DEVICE_FEATURE_SELECT, 0)?;
        let low = self.window.read_u32(register::DEVICE_FEATURE)?;
        self.window.write_u32(register::DEVICE_FEATURE_SELECT, 1)?;
        let high = self.window.read_u32(register::DEVICE_FEATURE)?;
        Ok((high as u64) << 32 | low as u64)
    }

    fn write_driver_features(&mut self, features: u64) -> Result<(), VirtioError> {
        self.window.write_u32(register::DRIVER_FEATURE_SELECT, 0)?;
        self.window.write_u32(register::DRIVER_FEATURE, features as u32)?;
        self.window.write_u32(register::DRIVER_FEATURE_SELECT, 1)?;
        self.window.write_u32(register::DRIVER_FEATURE, (features >> 32) as u32)?;
        Ok(())
    }

    pub fn num_queues(&self) -> Result<u16, VirtioError> {
        Ok(self.window.read_u16(register::NUM_QUEUES)?)
    }

    /// Selects the queue subsequent queue registers apply to.
    pub fn select_queue(&mut self, queue: u16) -> Result<(), VirtioError> {
        self.window.write_u16(register::QUEUE_SELECT, queue)?;
        Ok(())
    }

    /// The selected queue's maximum size, or zero if it does not exist.
    pub fn queue_size(&self) -> Result<u16, VirtioError> {
        Ok(self.window.read_u16(register::QUEUE_SIZE)?)
    }

    pub fn set_queue_size(&mut self, size: u16) -> Result<(), VirtioError> {
        self.window.write_u16(register::QUEUE_SIZE, size)?;
        Ok(())
    }

    /// Programs the selected queue's three ring physical addresses.
    pub fn set_queue_rings(
        &mut self,
        desc: u64,
        driver: u64,
        device: u64,
    ) -> Result<(), VirtioError> {
        self.window.write_u64(register::QUEUE_DESC, desc)?;
        self.window.write_u64(register::QUEUE_DRIVER, driver)?;
        self.window.write_u64(register::QUEUE_DEVICE, device)?;
        Ok(())
    }

    pub fn enable_queue(&mut self) -> Result<(), VirtioError> {
        self.window.write_u16(register::QUEUE_ENABLE, 1)?;
        Ok(())
    }

    /// The selected queue's notification offset, in notify-multiplier units.
    pub fn queue_notify_off(&self) -> Result<u16, VirtioError> {
        Ok(self.window.read_u16(register::QUEUE_NOTIFY_OFF)?)
    }
}

#[cfg(test)]
mod tests {
    use molt_arch::Mmio;

    use super::{Common, register};

    fn common(bytes: &mut [u8]) -> Common<'_> {
        // SAFETY: the slice outlives the borrow, is uniquely borrowed, and no
        // other window is handed out over it.
        let window = unsafe { Mmio::new(bytes.as_mut_ptr(), bytes.len() as u64) };
        Common::new(window)
    }

    #[test]
    fn reset_settles_accepted() {
        let mut registers = [0xffu8; 64];
        let mut common = common(&mut registers);

        common.reset().expect("a device that clears its status");

        assert_eq!(common.status(), Ok(0), "reset left status bits set");
    }

    #[test]
    fn status_bits_accumulate() {
        let mut registers = [0u8; 64];
        let mut common = common(&mut registers);

        common.add_status(super::status::ACKNOWLEDGE).expect("a legal write");
        common.add_status(super::status::DRIVER).expect("a legal write");

        assert_eq!(registers[register::DEVICE_STATUS as usize], 0b11);
    }

    #[test]
    fn queue_writes_all_three_rings() {
        let mut registers = [0u8; 64];
        let mut common = common(&mut registers);

        common.select_queue(0).expect("a legal write");
        common.set_queue_rings(0x1000, 0x2000, 0x3000).expect("a legal write");

        assert_eq!(&registers[0x20..0x28], &0x1000u64.to_le_bytes());
        assert_eq!(&registers[0x28..0x30], &0x2000u64.to_le_bytes());
        assert_eq!(&registers[0x30..0x38], &0x3000u64.to_le_bytes());
    }
}
