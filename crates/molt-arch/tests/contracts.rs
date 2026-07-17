use core::fmt::Write;

use molt_arch::{
    BootInfo, InterruptController, MemoryMap, MemoryRegion, MemoryRegionKind, SerialPort,
    SerialWriter,
};

struct TestMemoryMap([MemoryRegion; 2]);

impl MemoryMap for TestMemoryMap {
    fn len(&self) -> usize {
        self.0.len()
    }

    fn region(&self, index: usize) -> Option<MemoryRegion> {
        self.0.get(index).copied()
    }
}

#[derive(Default)]
struct TestSerial(Vec<u8>);

impl SerialPort for TestSerial {
    fn write_byte(&mut self, byte: u8) {
        self.0.push(byte);
    }
}

#[derive(Default)]
struct TestInterrupts {
    enabled: Vec<u8>,
}

impl InterruptController for TestInterrupts {
    fn enable_irq(&mut self, irq: u8) {
        self.enabled.push(irq);
    }
}

#[test]
fn boot_contract_is_independent_of_a_bootloader() {
    let map = TestMemoryMap([
        MemoryRegion::new(0, 4096, MemoryRegionKind::Reserved),
        MemoryRegion::new(4096, 8192, MemoryRegionKind::Usable),
    ]);
    let boot_info = BootInfo::new(&map, Some(0xffff_8000_0000_0000));

    assert_eq!(boot_info.memory_map().len(), 2);
    assert_eq!(boot_info.memory_map().region(1), Some(map.0[1]));
    assert_eq!(boot_info.physical_memory_offset(), Some(0xffff_8000_0000_0000));
}

#[test]
fn hardware_contracts_can_be_exercised_with_safe_mocks() {
    let mut serial = TestSerial::default();
    writeln!(SerialWriter::new(&mut serial), "MOLT").unwrap();
    assert_eq!(serial.0, b"MOLT\n");

    let mut interrupts = TestInterrupts::default();
    interrupts.enable_irq(4);
    assert_eq!(interrupts.enabled, [4]);
}
