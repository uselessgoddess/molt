//! Where a modern VirtIO device keeps its structures, read out of its PCI
//! vendor capabilities.
//!
//! A modern device describes each structure with a vendor capability (id
//! `0x09`) that names a BAR, an offset into it, and a length. [`Transport`]
//! walks that list once and records the three structures the driver needs, so
//! the rest of the crate works in [`Location`]s rather than raw capability
//! offsets.

use molt_pci::Function;

use crate::VirtioError;

/// The vendor capability every VirtIO structure is described by.
const VENDOR: u8 = 0x09;

/// Field offsets inside a `virtio_pci_cap`, relative to the capability start.
mod field {
    pub const CONFIG_TYPE: u64 = 3;
    pub const BAR: u64 = 4;
    pub const OFFSET: u64 = 8;
    pub const LENGTH: u64 = 12;
    pub const NOTIFY_MULTIPLIER: u64 = 16;
}

/// The kind of structure a vendor capability points at.
///
/// Only the three the driver uses are named; the ISR and PCI-config structures
/// are recognized by number and ignored.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Structure {
    /// The common configuration structure that drives initialization.
    Common,
    /// The notification structure a queue kick is written to.
    Notify,
    /// The device-specific configuration structure.
    Device,
}

impl Structure {
    fn of(config_type: u8) -> Option<Self> {
        match config_type {
            1 => Some(Self::Common),
            2 => Some(Self::Notify),
            4 => Some(Self::Device),
            _ => None,
        }
    }
}

/// One structure's place: which BAR holds it and where inside it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Location {
    bar: u8,
    offset: u32,
    length: u32,
}

impl Location {
    /// The BAR index the structure lives in.
    pub const fn bar(self) -> u8 {
        self.bar
    }

    /// The structure's offset from the BAR's base.
    pub const fn offset(self) -> u32 {
        self.offset
    }

    /// How many bytes the structure occupies.
    pub const fn length(self) -> u32 {
        self.length
    }
}

/// The three structures a block driver needs, plus how a queue notification is
/// addressed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Transport {
    common: Location,
    notify: Location,
    device: Location,
    notify_multiplier: u32,
}

impl Transport {
    /// Reads `function`'s vendor capabilities into a transport.
    ///
    /// The first capability of each kind wins, matching how a device is
    /// expected to list them. A device missing any of the three is not one this
    /// driver can drive, so it is refused rather than half-configured.
    pub fn probe(function: &Function<'_>) -> Result<Self, VirtioError> {
        let window = function.window();
        let mut common = None;
        let mut notify = None;
        let mut device = None;
        let mut multiplier = 0;

        for capability in function.capabilities()? {
            let capability = capability?;
            if capability.id() != VENDOR {
                continue;
            }
            let at = capability.offset();
            let config_type = window.read_u8(at + field::CONFIG_TYPE)?;
            let Some(structure) = Structure::of(config_type) else {
                continue;
            };
            let location = Location {
                bar: window.read_u8(at + field::BAR)?,
                offset: window.read_u32(at + field::OFFSET)?,
                length: window.read_u32(at + field::LENGTH)?,
            };
            match structure {
                Structure::Common => common.get_or_insert(location),
                Structure::Notify => {
                    multiplier = window.read_u32(at + field::NOTIFY_MULTIPLIER)?;
                    notify.get_or_insert(location)
                }
                Structure::Device => device.get_or_insert(location),
            };
        }

        match (common, notify, device) {
            (Some(common), Some(notify), Some(device)) => {
                Ok(Self { common, notify, device, notify_multiplier: multiplier })
            }
            _ => Err(VirtioError::Missing),
        }
    }

    pub const fn common(&self) -> Location {
        self.common
    }

    pub const fn notify(&self) -> Location {
        self.notify
    }

    pub const fn device(&self) -> Location {
        self.device
    }

    /// The stride a queue's notification offset is multiplied by.
    pub const fn notify_multiplier(&self) -> u32 {
        self.notify_multiplier
    }
}

#[cfg(test)]
mod tests {
    use molt_arch::Mmio;
    use molt_pci::{Address, Function};

    use super::Transport;
    use crate::VirtioError;

    /// A configuration space made of ordinary memory, with just enough of a
    /// header and vendor-capability list for [`Transport::probe`] to walk.
    struct Config {
        bytes: [u8; 256],
        last: Option<usize>,
    }

    impl Config {
        fn present() -> Self {
            let mut bytes = [0u8; 256];
            bytes[0x00..0x02].copy_from_slice(&0x1af4u16.to_le_bytes());
            bytes[0x02..0x04].copy_from_slice(&0x1042u16.to_le_bytes());
            Self { bytes, last: None }
        }

        /// Adds a `virtio_pci_cap` of `config_type` at `offset`, chaining it
        /// onto the capability list.
        fn cap(&mut self, offset: usize, config_type: u8, bar: u8, at: u32, length: u32) -> &mut Self {
            match self.last {
                Some(previous) => self.bytes[previous + 1] = offset as u8,
                None => {
                    self.bytes[0x06] |= 1 << 4;
                    self.bytes[0x34] = offset as u8;
                }
            }
            self.bytes[offset] = 0x09;
            self.bytes[offset + 1] = 0;
            self.bytes[offset + 3] = config_type;
            self.bytes[offset + 4] = bar;
            self.bytes[offset + 8..offset + 12].copy_from_slice(&at.to_le_bytes());
            self.bytes[offset + 12..offset + 16].copy_from_slice(&length.to_le_bytes());
            self.last = Some(offset);
            self
        }

        fn multiplier(&mut self, offset: usize, multiplier: u32) -> &mut Self {
            self.bytes[offset + 16..offset + 20].copy_from_slice(&multiplier.to_le_bytes());
            self
        }

        fn function(&mut self) -> Function<'_> {
            // SAFETY: the array outlives the borrow, is uniquely borrowed, and
            // no other window is handed out over it.
            let window = unsafe { Mmio::new(self.bytes.as_mut_ptr(), self.bytes.len() as u64) };
            Function::probe(window, Address::new(0, 0, 0).expect("00:00.0"))
                .expect("a legal read")
                .expect("a present function")
        }
    }

    #[test]
    fn probe_records_each_structure_by_bar_and_offset() {
        let mut config = Config::present();
        // The notify cap carries a trailing multiplier, so the device cap sits
        // past its twenty bytes rather than over them.
        config
            .cap(0x40, 1, 4, 0x0000, 0x1000)
            .cap(0x50, 2, 4, 0x3000, 0x0100)
            .multiplier(0x50, 4)
            .cap(0x70, 4, 4, 0x2000, 0x0100);

        let transport = Transport::probe(&config.function()).expect("all three structures");

        assert_eq!((transport.common().bar(), transport.common().offset()), (4, 0x0000));
        assert_eq!((transport.notify().bar(), transport.notify().offset()), (4, 0x3000));
        assert_eq!(transport.notify_multiplier(), 4);
    }

    #[test]
    fn probe_refuses_a_device_missing_a_structure() {
        let mut config = Config::present();
        config.cap(0x40, 1, 4, 0x0000, 0x1000).cap(0x50, 2, 4, 0x3000, 0x0100);

        assert_eq!(Transport::probe(&config.function()), Err(VirtioError::Missing));
    }
}
