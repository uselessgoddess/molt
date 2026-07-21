use std::cell::RefCell;

use molt_pci::address::WINDOW;
use molt_pci::bar::Width;
use molt_pci::config::Config;
use molt_pci::{Address, Command, Ecam, Error, Function, capability, scan};

/// One function's configuration space, with read-only bits modelled: a write
/// only lands where the device implements writable bits, which is what makes
/// BAR sizing behave the way it does on hardware.
struct Space {
    words: RefCell<Vec<u32>>,
    writable: Vec<u32>,
}

impl Space {
    fn endpoint(vendor: u16, device: u16) -> Self {
        let mut space =
            Self { words: RefCell::new(vec![0; WINDOW / 4]), writable: vec![0; WINDOW / 4] };
        space.set(0x00, u32::from(vendor) | u32::from(device) << 16);
        // The command half of the word is writable; the status half is not.
        space.writable[1] = 0x0000_ffff;
        space
    }

    fn multifunction(mut self) -> Self {
        self.set(0x0c, 0x80 << 16);
        self
    }

    /// A memory window of `size` bytes decoding at `base`.
    fn bar(mut self, index: u16, base: u64, size: u64, width: Width) -> Self {
        let offset = 0x10 + index * 4;
        let kind = match width {
            Width::Bits32 => 0,
            Width::Bits64 => 0b100,
        };
        self.set(offset, base as u32 | kind);
        self.writable[usize::from(offset) / 4] = !(size as u32 - 1) & !0xf;
        if width == Width::Bits64 {
            self.set(offset + 4, (base >> 32) as u32);
            self.writable[usize::from(offset) / 4 + 1] = !((size - 1) >> 32) as u32;
        }
        self
    }

    fn io_bar(mut self, index: u16, port: u32) -> Self {
        self.set(0x10 + index * 4, port | 1);
        self
    }

    /// Starts the capability list at `offset` and announces it in status.
    fn capabilities(mut self, offset: u8) -> Self {
        self.set(0x04, self.read(0x04) | 1 << (16 + 4));
        self.set(0x34, u32::from(offset));
        self
    }

    fn capability(mut self, offset: u16, id: u8, next: u8) -> Self {
        self.set(offset, u32::from(id) | u32::from(next) << 8);
        self
    }

    /// An MSI-X capability with `vectors` entries, its table at `offset` bytes
    /// into `bar`. Only the enable and function-mask bits of control are
    /// writable, as on a device.
    fn msix(mut self, at: u16, vectors: u16, bar: u8, table: u32) -> Self {
        self.set(at, u32::from(capability::MSIX) | u32::from(vectors - 1) << 16);
        self.writable[usize::from(at) / 4] = 0xc000_0000;
        self.set(at + 4, table | u32::from(bar));
        self.set(at + 8, table | u32::from(bar));
        self
    }

    fn set(&mut self, offset: u16, value: u32) {
        self.words.borrow_mut()[usize::from(offset) / 4] = value;
    }

    fn read(&self, offset: u16) -> u32 {
        self.words.borrow()[usize::from(offset) / 4]
    }
}

/// A bus that answers for the functions it was given and all ones elsewhere.
struct Bus(Vec<(Address, Space)>);

impl Bus {
    fn of(at: Address, space: Space) -> Self {
        Self(vec![(at, space)])
    }

    fn with(mut self, at: Address, space: Space) -> Self {
        self.0.push((at, space));
        self
    }

    fn space(&self, at: Address) -> Option<&Space> {
        self.0.iter().find(|(address, _)| *address == at).map(|(_, space)| space)
    }
}

impl Config for Bus {
    fn read(&self, at: Address, offset: u16) -> u32 {
        self.space(at).map_or(!0, |space| space.read(offset))
    }

    fn write(&self, at: Address, offset: u16, value: u32) {
        let Some(space) = self.space(at) else { return };
        let index = usize::from(offset) / 4;
        let mask = space.writable[index];
        let mut words = space.words.borrow_mut();
        words[index] = value & mask | words[index] & !mask;
    }

    fn buses(&self) -> (u8, u8) {
        (0, 0)
    }
}

fn at(bus: u8, device: u8, function: u8) -> Address {
    Address::new(bus, device, function).expect("an encodable address")
}

#[test]
fn a_window_is_a_bus_a_device_and_a_function() {
    assert_eq!(at(0, 0, 0).window(), 0);
    assert_eq!(at(0, 1, 0).window(), 0x8000);
    assert_eq!(at(0, 0, 1).window(), 0x1000);
    assert_eq!(at(1, 0, 0).window(), 0x10_0000);
}

#[test]
fn an_unencodable_address_is_refused() {
    assert_eq!(Address::new(0, 32, 0), Err(Error::Address));
    assert_eq!(Address::new(0, 0, 8), Err(Error::Address));
}

#[test]
fn a_silent_address_holds_no_function() {
    let bus = Bus::of(at(0, 1, 0), Space::endpoint(0x1b36, 0x0005));

    assert_eq!(Function::probe(&bus, at(0, 2, 0)).err(), Some(Error::Absent));
    assert_eq!(scan(&bus).count(), 1);
}

#[test]
fn a_sweep_finds_every_function() {
    let bus = Bus::of(at(0, 1, 0), Space::endpoint(0x1b36, 0x0005).multifunction())
        .with(at(0, 1, 3), Space::endpoint(0x1b36, 0x0006));

    let found: Vec<_> = scan(&bus).map(|function| function.address()).collect();
    assert_eq!(found, [at(0, 1, 0), at(0, 1, 3)]);
}

#[test]
fn a_single_function_device_hides_its_other_functions() {
    let bus = Bus::of(at(0, 1, 0), Space::endpoint(0x1b36, 0x0005))
        .with(at(0, 1, 1), Space::endpoint(0x1b36, 0x0006));

    assert_eq!(scan(&bus).count(), 1, "a function was probed past a single-function device");
}

#[test]
fn a_bar_reports_the_size_it_decodes() {
    let space = Space::endpoint(0x1b36, 0x0005).bar(0, 0xfebf_1000, 0x1000, Width::Bits32);
    let bus = Bus::of(at(0, 1, 0), space);

    let bar = Function::probe(&bus, at(0, 1, 0)).unwrap().bar(0).unwrap();
    assert_eq!((bar.base(), bar.size(), bar.width()), (0xfebf_1000, 0x1000, Width::Bits32));
}

#[test]
fn a_wide_bar_occupies_two_registers() {
    let space = Space::endpoint(0x1b36, 0x0005).bar(2, 0x8_0000_0000, 0x10_0000, Width::Bits64);
    let bus = Bus::of(at(0, 1, 0), space);

    let bars: Vec<_> = Function::probe(&bus, at(0, 1, 0)).unwrap().bars().collect();
    assert_eq!(bars.len(), 1, "the upper half of a 64-bit window was measured as a window");
    assert_eq!((bars[0].base(), bars[0].size(), bars[0].next()), (0x8_0000_0000, 0x10_0000, 4));
}

#[test]
fn measuring_a_bar_leaves_the_device_as_it_was() {
    let bus = Bus::of(
        at(0, 1, 0),
        Space::endpoint(0x1b36, 0x0005).bar(0, 0xfebf_1000, 0x1000, Width::Bits32),
    );
    let function = Function::probe(&bus, at(0, 1, 0)).unwrap();
    function.enable(Command::MEMORY | Command::BUS_MASTER);

    function.bar(0).unwrap();
    assert_eq!(bus.read(at(0, 1, 0), 0x10), 0xfebf_1000, "sizing left the window somewhere else");
    assert!(function.command().contains(Command::MEMORY), "sizing left memory decoding off");
}

#[test]
fn an_io_bar_is_not_a_window() {
    let bus = Bus::of(at(0, 1, 0), Space::endpoint(0x1b36, 0x0005).io_bar(0, 0xc000));

    let function = Function::probe(&bus, at(0, 1, 0)).unwrap();
    assert_eq!(function.bar(0).err(), Some(Error::NotMemory));
    assert_eq!(function.bars().count(), 0);
}

#[test]
fn an_unimplemented_bar_is_not_a_window() {
    let bus = Bus::of(at(0, 1, 0), Space::endpoint(0x1b36, 0x0005));

    assert_eq!(Function::probe(&bus, at(0, 1, 0)).unwrap().bar(0).err(), Some(Error::Bar));
}

#[test]
fn capabilities_follow_the_chain() {
    let space = Space::endpoint(0x1b36, 0x0005)
        .capabilities(0x40)
        .capability(0x40, capability::VENDOR, 0x60)
        .capability(0x60, capability::MSIX, 0x00);
    let bus = Bus::of(at(0, 1, 0), space);

    let found: Vec<_> = Function::probe(&bus, at(0, 1, 0)).unwrap().capabilities().collect();
    assert_eq!(
        found.iter().map(|entry| entry.id()).collect::<Vec<_>>(),
        [capability::VENDOR, capability::MSIX]
    );
}

#[test]
fn a_function_without_the_status_bit_has_no_capabilities() {
    let space = Space::endpoint(0x1b36, 0x0005).capability(0x40, capability::MSIX, 0);
    let bus = Bus::of(at(0, 1, 0), space);

    assert_eq!(Function::probe(&bus, at(0, 1, 0)).unwrap().capabilities().count(), 0);
}

#[test]
fn a_looping_capability_list_ends() {
    let space = Space::endpoint(0x1b36, 0x0005).capabilities(0x40).capability(
        0x40,
        capability::VENDOR,
        0x40,
    );
    let bus = Bus::of(at(0, 1, 0), space);

    assert!(Function::probe(&bus, at(0, 1, 0)).unwrap().capabilities().count() < 64);
}

#[test]
fn msix_reports_where_its_table_lives() {
    let space = Space::endpoint(0x1b36, 0x0005).capabilities(0x60).msix(0x60, 3, 1, 0x2000);
    let bus = Bus::of(at(0, 1, 0), space);

    let msix = Function::probe(&bus, at(0, 1, 0)).unwrap().msix().unwrap();
    assert_eq!(msix.vectors(), 3);
    assert_eq!((msix.table().bar(), msix.table().offset()), (1, 0x2000));
}

#[test]
fn enabling_msix_masks_every_vector() {
    let space = Space::endpoint(0x1b36, 0x0005).capabilities(0x60).msix(0x60, 3, 1, 0x2000);
    let bus = Bus::of(at(0, 1, 0), space);
    let msix = Function::probe(&bus, at(0, 1, 0)).unwrap().msix().unwrap();

    msix.enable();
    assert!(msix.enabled());
    assert_eq!(bus.read(at(0, 1, 0), 0x60) >> 30, 0b11, "delivery was turned on unmasked");
}

#[test]
fn a_function_without_msix_says_so() {
    let bus = Bus::of(at(0, 1, 0), Space::endpoint(0x1b36, 0x0005));

    assert_eq!(Function::probe(&bus, at(0, 1, 0)).unwrap().msix().err(), Some(Error::Missing));
}

#[test]
fn ecam_addresses_one_window_per_function() {
    let mut region = vec![0u32; (Ecam::span(0, 0) / 4) as usize];
    region[at(0, 1, 2).window() / 4] = 0x0005_1b36;
    let ecam = unsafe { Ecam::new(region.as_mut_ptr(), 0, 0) };

    assert_eq!(ecam.read(at(0, 1, 2), 0), 0x0005_1b36);
    assert_eq!(ecam.read(at(0, 1, 3), 0), 0, "a neighbouring window answered");
}

#[test]
fn ecam_refuses_a_bus_it_does_not_map() {
    let mut region = vec![0u32; (Ecam::span(0, 0) / 4) as usize];
    let ecam = unsafe { Ecam::new(region.as_mut_ptr(), 0, 0) };

    assert_eq!(ecam.read(at(1, 0, 0), 0), !0);
    ecam.write(at(1, 0, 0), 0, 0);
    assert_eq!(region[0], 0, "a write outside the window landed inside it");
}
