use molt_arch::dma::Arena;
use molt_arch::memory::{Inventory, Owner, Rights};
use molt_arch::{BootInfo, FrameAllocator, Platform, SerialWriter};
use molt_block::{Device, SECTOR};
use molt_kernel::report;
use molt_pci::{Bus, Command, bus_span};
use molt_virtio::{Block, Transport};

use crate::device;

/// QEMU's modern virtio-blk-pci function (`disable-legacy=on`).
const VIRTIO_VENDOR: u16 = 0x1af4;
const VIRTIO_BLOCK: u16 = 0x1042;

/// What a MoltFS volume starts with, which is what `xtask` puts on the disk.
const SIGNATURE: [u8; 8] = molt_fs::MAGIC;
const DMA_FRAMES: usize = 8;
const BLOCK_TAG: u32 = 0xb10c;

pub fn smoke<P: Platform>(boot_info: &BootInfo<'_>, platform: &mut P) {
    let Ok(space) = platform.config_space(boot_info) else {
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
    let capability = function.msix().expect("virtio-blk exposes MSI-X");
    let table_bar = capability.table_bar();
    let (bar, registers) = device::map_bar(platform, &inventory, &mut function, bar_index);
    let (table_bar, table_mapping) = if table_bar == bar_index {
        (bar, None)
    } else {
        let (table_bar, mapping) = device::map_bar(platform, &inventory, &mut function, table_bar);
        (table_bar, Some(mapping))
    };

    // `INTX_DISABLE` goes on with the rest: a function left free to assert a
    // pin interrupt as well would deliver the same queue twice.
    let command = function.command().expect("the command register");
    function
        .set_command(
            command.with(Command::MEMORY).with(Command::BUS_MASTER).with(Command::INTX_DISABLE),
        )
        .expect("a writable command register");
    report!(
        platform,
        "MOLT_VIRTIO_OK: {} {:04x}:{:04x} bar {bar_index} at {:#x}",
        function.address(),
        function.vendor(),
        function.device(),
        bar.base(),
    );

    let table = table_mapping.as_ref().unwrap_or(&registers);
    let vectored = device::route(platform, &function, capability, table, device::delta(table_bar));
    let delta = device::delta(bar);
    let common = device::subwindow(&registers, delta, transport.common());
    let notify = device::subwindow(&registers, delta, transport.notify());
    let config = device::subwindow(&registers, delta, transport.device());

    let mut allocator = FrameAllocator::resume(boot_info.memory_map(), cursor);
    let mut slots: [Option<Owner>; DMA_FRAMES] = [None; DMA_FRAMES];
    let arena = Arena::claim(&mut allocator, offset, BLOCK_TAG, &mut slots)
        .expect("contiguous device frames past the kernel's own");

    let mut block = Block::start(
        common,
        notify,
        config,
        transport.notify_multiplier(),
        vectored.index(),
        vectored.line(),
        arena,
    )
    .expect("the device completes its handshake");

    let mut sector = [0u8; SECTOR];
    block.read(0, &mut sector).expect("sector zero reads back");
    assert_eq!(&sector[..SIGNATURE.len()], &SIGNATURE, "sector zero holds no volume signature");
    report!(platform, "MOLT_BLOCK_OK: sector zero carries the volume signature");
    // The driver has no used-ring poll left: a read that returned at all
    // returned because the queue's vector fired and the line counted it.
    report!(platform, "MOLT_BLK_IRQ_OK: queue zero answered on vector {}", vectored.index());

    // The cells init starts borrow the driver, so the device is still this
    // function's to stop afterwards.
    crate::init::smoke(platform, &mut block);

    block.reset().expect("the device stops and its frames return");
    report!(platform, "MOLT_VIRTIO_RESET_OK: device stopped and frames reclaimed");
    vectored.stop(platform);
}
