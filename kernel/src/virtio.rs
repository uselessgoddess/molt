//! Completing block I/O through a ring the kernel owns.
//!
//! This finds QEMU's modern virtio-blk function on bus zero, maps its BAR, and
//! hands [`Block`] a queue built out of [`Owner::Device`] frames drawn from the
//! pool no live mapping claims. It reads sector zero, checks it against the disk
//! `xtask` signs, then resets the device — which reclaims those frames only
//! after the device has been told to stop.
//!
//! Everything here is polled rather than interrupt-driven: no MSI-X vector is
//! routed, so the device never raises a completion and the driver drains the
//! used ring itself. A machine without a virtio disk, or a platform that hands
//! out no DMA frames, is reported and skipped rather than made a boot
//! requirement.

use molt_arch::dma::Arena;
use molt_arch::memory::{Inventory, Owner, Rights};
use molt_arch::{BootInfo, FrameAllocator, Platform, SerialWriter};
use molt_kernel::report;
use molt_pci::{Bus, Command, bus_span};
use molt_virtio::{Block, SECTOR, Transport};

/// QEMU's modern virtio-blk-pci function (`disable-legacy=on`).
const VIRTIO_VENDOR: u16 = 0x1af4;
const VIRTIO_BLOCK: u16 = 0x1042;

/// The signature `xtask` writes at the start of sector zero.
const SIGNATURE: [u8; 8] = *b"MOLTDISK";

/// Frames the arena claims: three ring regions, a control block, and a data
/// buffer, each a whole frame, with headroom to spare.
const DMA_FRAMES: usize = 8;

/// The owner tag stamped on the driver's device frames.
const BLOCK_TAG: u32 = 0xb10c;

pub fn smoke<P: Platform>(boot_info: &BootInfo<'_>, platform: &mut P) {
    let Ok(space) = platform.config_space(boot_info) else {
        // `pci::smoke` already reported the missing configuration space.
        return;
    };
    let (Some(cursor), Some(offset)) = (platform.free_frames(), boot_info.physical_offset()) else {
        report!(platform, "MOLT_VIRTIO_SKIPPED: this platform hands out no DMA frames");
        return;
    };

    let inventory = Inventory::new(boot_info.memory_map());
    let bus_zero = bus_span(space, space.first_bus()).expect("bus zero inside the ECAM window");
    let ecam = inventory.device(bus_zero).expect("the ECAM window is not memory the kernel owns");
    let window = platform.map_device(ecam, Rights::READ_WRITE).expect("a mappable ECAM window");

    let mut bus = Bus::new(&window, 0);
    let mut target = None;
    while let Some(function) = bus.function() {
        if function.vendor() == VIRTIO_VENDOR && function.device() == VIRTIO_BLOCK {
            target = Some(function);
            break;
        }
    }
    let Some(mut function) = target else {
        report!(platform, "MOLT_VIRTIO_SKIPPED: no virtio-blk device on bus zero");
        return;
    };

    let transport = Transport::probe(&function).expect("a modern device describes its structures");
    let bar_index = transport.common().bar();
    assert!(
        transport.notify().bar() == bar_index && transport.device().bar() == bar_index,
        "virtio structures split across BARs",
    );

    // The BAR is classified against the same firmware map the kernel's RAM came
    // from, so a device claiming a base inside RAM is refused rather than mapped.
    let bar = function.bar(bar_index).expect("a readable BAR").expect("the BAR the transport named");
    let span = bar.span().expect("a frame-aligned BAR span");
    let device = inventory.device(span).expect("a BAR outside the kernel's RAM");
    let registers = platform.map_device(device, Rights::READ_WRITE).expect("a mappable BAR");
    let delta = bar.base() - span.start();

    let common = registers
        .subwindow(delta + transport.common().offset() as u64, transport.common().length() as u64)
        .expect("the common structure inside the BAR");
    let notify = registers
        .subwindow(delta + transport.notify().offset() as u64, transport.notify().length() as u64)
        .expect("the notify structure inside the BAR");

    // Bus mastering is the device's licence to DMA; see the note in `pci.rs` on
    // why the kernel grants it here, once, for the one device it chose.
    let command = function.command().expect("the command register");
    function
        .set_command(command.with(Command::MEMORY).with(Command::BUS_MASTER))
        .expect("a writable command register");
    report!(
        platform,
        "MOLT_VIRTIO_OK: {} {:04x}:{:04x} bar {bar_index} at {:#x}",
        function.address(),
        function.vendor(),
        function.device(),
        bar.base(),
    );

    let mut allocator = FrameAllocator::resume(boot_info.memory_map(), cursor);
    let mut slots: [Option<Owner>; DMA_FRAMES] = [None; DMA_FRAMES];
    let arena = Arena::claim(&mut allocator, offset, BLOCK_TAG, &mut slots)
        .expect("contiguous device frames past the kernel's own");

    let mut block = Block::start(common, notify, transport.notify_multiplier(), arena)
        .expect("the device completes its handshake");

    let mut sector = [0u8; SECTOR];
    block.read(0, &mut sector).expect("sector zero reads back");
    verify(&sector);
    report!(platform, "MOLT_BLOCK_OK: sector zero matches the signed disk");

    // Reset stops the device before the arena reclaims its frames, so no
    // in-flight DMA can land in a frame after it returns to the table.
    block.reset().expect("the device stops and its frames return");
    report!(platform, "MOLT_VIRTIO_RESET_OK: device stopped and frames reclaimed");
}

/// Checks sector zero against the disk `xtask` signs: the signature, then a
/// byte pattern keyed by offset.
fn verify(sector: &[u8; SECTOR]) {
    assert_eq!(&sector[..SIGNATURE.len()], &SIGNATURE, "sector zero lacks the disk signature");
    for (index, &byte) in sector.iter().enumerate().skip(SIGNATURE.len()) {
        assert_eq!(byte, (index as u8) ^ 0x5a, "sector zero byte {index} broke the pattern");
    }
}
