use alloc::boxed::Box;

use molt_arch::dma::Arena;
use molt_arch::memory::{Inventory, Owner, Rights};
use molt_arch::{BootInfo, FrameAllocator, Platform, SerialWriter};
use molt_block::{BlockOp, Device, Queue, SECTOR};
use molt_core::ring::RequestId;
use molt_kernel::report;
use molt_pci::{Command, bus_span};
use molt_virtio::Block;

use crate::{device, isolation};

/// QEMU's modern virtio-blk-pci function (`disable-legacy=on`).
const VIRTIO_VENDOR: u16 = 0x1af4;
const VIRTIO_BLOCK: u16 = 0x1042;
/// The NIC, which the block smoke borrows as a second endpoint before the
/// network smoke drives it.
const VIRTIO_NET: u16 = 0x1041;

const SIGNATURE: [u8; 8] = molt_fs::MAGIC;
const DMA_FRAMES: usize = 12;
const BLOCK_TAG: u32 = 0xb10c;
const IOMMU_TAG: u32 = 0x10aa;

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

    let found = isolation::pair(&window, space.first_bus(), |function| {
        function.vendor() == VIRTIO_VENDOR && function.device() == VIRTIO_BLOCK
    });
    let Some((mut function, controller)) = found else {
        report!(platform, "MOLT_VIRTIO_SKIPPED: no virtio-blk/IOMMU pair on bus zero");
        return;
    };
    let control = isolation::Control::open(platform, &inventory, controller);

    let (transport, bar_index) = device::transport(&function);
    let capability = function.msix().expect("virtio-blk exposes MSI-X");
    let (bar, registers) = device::map_bar(platform, &inventory, &mut function, bar_index);
    let (table_bar, table_mapping) = device::table_bar(
        platform,
        &inventory,
        &mut function,
        (bar, bar_index),
        capability.table_bar(),
    );

    let quiesced = device::quiesce(&mut function);
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
    let (common, notify, config) = device::structures(&registers, bar, &transport);

    let mut allocator = FrameAllocator::resume(boot_info.memory_map(), cursor);
    let mut iommu_slots = isolation::SLOTS;
    let iommu_arena = isolation::arena(&mut allocator, offset, IOMMU_TAG, &mut iommu_slots);
    let mut slots: [Option<Owner>; DMA_FRAMES] = [None; DMA_FRAMES];
    let arena = Arena::claim(&mut allocator, offset, BLOCK_TAG, &mut slots)
        .expect("contiguous device frames past the kernel's own");

    let endpoint = function.address().requester();
    let mut iommu = control.start(iommu_arena, endpoint);
    report!(platform, "MOLT_IOMMU_OK: block endpoint attached before bus mastering");

    // A second quiesced endpoint, borrowed for the length of one proof: which
    // domain a device lands in follows the order the kernel attached in, so
    // the one with the higher requester ID takes the lower domain when the
    // kernel puts it there first. It has to be a function that is really on
    // the bus — the controller answers ATTACH for endpoints it can see.
    let witness = isolation::pair(&window, space.first_bus(), |function| {
        function.vendor() == VIRTIO_VENDOR && function.device() == VIRTIO_NET
    })
    .map(|(net, _)| net.address().requester())
    .filter(|witness| witness.get() > endpoint.get());
    match witness {
        Some(witness) => {
            let (first, second) = isolation::ordered(&mut iommu, endpoint, witness);
            report!(
                platform,
                "MOLT_IOMMU_DOMAIN_OK: endpoint {:#x} took domain {first} ahead of {:#x} in {second}",
                witness.get(),
                endpoint.get(),
            );
        }
        None => report!(platform, "MOLT_IOMMU_DOMAIN_SKIPPED: no later endpoint on bus zero"),
    }

    let mut block = Block::start_mapped(
        common,
        notify,
        config,
        transport.notify_multiplier(),
        vectored.index(),
        vectored.line(),
        endpoint,
        arena,
        iommu,
    )
    .expect("the mapped device completes its handshake");
    function
        .set_command(quiesced.with(Command::BUS_MASTER))
        .expect("bus mastering enabled after mappings exist");
    report!(platform, "MOLT_IOMMU_MAP_OK: {} block DMA regions installed", block.mapper().mapped(),);

    let first = RequestId::new(0x10);
    let second = RequestId::new(0x11);
    assert!(
        Queue::start(
            &mut block,
            first,
            BlockOp::Read { sector: 0, bytes: SECTOR, buffer: Box::new([0; molt_block::BLOCK]) },
        )
        .is_ok(),
        "the first depth probe submits"
    );
    assert!(
        Queue::start(
            &mut block,
            second,
            BlockOp::Read { sector: 1, bytes: SECTOR, buffer: Box::new([0; molt_block::BLOCK]) },
        )
        .is_ok(),
        "the second depth probe submits before the first completes"
    );
    let mut first_seen = false;
    for _ in 0..2 {
        let (id, done) = Queue::reap(&mut block).expect("a depth probe completes");
        done.result.expect("a depth probe read succeeds");
        if id == first {
            let bytes = done.buffer.expect("a read returns its buffer");
            assert_eq!(&bytes[..SIGNATURE.len()], &SIGNATURE, "the first queued read was mixed up");
            first_seen = true;
        }
    }
    assert!(first_seen, "the first queued request never completed");
    report!(
        platform,
        "MOLT_BLOCK_DEPTH_OK: two reads were live together at depth {}",
        block.depth()
    );

    let mut sector = [0u8; SECTOR];
    block.read(0, &mut sector).expect("sector zero reads back");
    assert_eq!(&sector[..SIGNATURE.len()], &SIGNATURE, "sector zero holds no volume signature");
    report!(platform, "MOLT_BLOCK_OK: sector zero carries the volume signature");
    report!(platform, "MOLT_BLK_IRQ_OK: queue zero answered on vector {}", vectored.index());

    crate::init::smoke(platform, &mut block);
    crate::filemap::smoke(boot_info, platform, &mut block);

    let iommu = block.reset().expect("the device stops and its mappings return");
    function.set_command(quiesced).expect("bus mastering stays off after reset");
    control.stop(iommu, endpoint);
    report!(platform, "MOLT_IOMMU_FAULT_OK: no translation fault escaped the event queue");
    report!(platform, "MOLT_VIRTIO_RESET_OK: device stopped and frames reclaimed");
    vectored.stop(platform);
}
