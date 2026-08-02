use alloc::boxed::Box;

use molt_arch::dma::Arena;
use molt_arch::memory::{Inventory, Owner, Rights};
use molt_arch::{BootInfo, FrameAllocator, Platform, SerialWriter};
use molt_block::{BlockOp, Device, Queue, SECTOR};
use molt_core::ring::RequestId;
use molt_kernel::report;
use molt_pci::{Bus, Command, bus_span};
use molt_virtio::{Arrivals, Block, Iommu, Transport};

use crate::device;

/// QEMU's modern virtio-blk-pci function (`disable-legacy=on`).
const VIRTIO_VENDOR: u16 = 0x1af4;
const VIRTIO_BLOCK: u16 = 0x1042;
const VIRTIO_IOMMU: u16 = 0x1057;

const SIGNATURE: [u8; 8] = molt_fs::MAGIC;
const DMA_FRAMES: usize = 12;
const IOMMU_FRAMES: usize = 8;
const BLOCK_TAG: u32 = 0xb10c;
const IOMMU_TAG: u32 = 0x10aa;

struct Poll;

impl Arrivals for Poll {
    fn wait(&mut self) -> u64 {
        core::hint::spin_loop();
        0
    }
}

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
    let mut controller = None;
    while let Some(function) = bus.function() {
        if function.vendor() == VIRTIO_VENDOR {
            match function.device() {
                VIRTIO_BLOCK => target = Some(function),
                VIRTIO_IOMMU => controller = Some(function),
                _ => {}
            }
        }
    }
    let (Some(mut function), Some(mut iommu_function)) = (target, controller) else {
        report!(platform, "MOLT_VIRTIO_SKIPPED: no virtio-blk/IOMMU pair on bus zero");
        return;
    };

    let iommu_transport =
        Transport::probe(&iommu_function).expect("the IOMMU describes its structures");
    let iommu_bar_index = iommu_transport.common().bar();
    assert!(
        iommu_transport.notify().bar() == iommu_bar_index
            && iommu_transport.device().bar() == iommu_bar_index,
        "IOMMU structures split across BARs",
    );
    let (iommu_bar, iommu_registers) =
        device::map_bar(platform, &inventory, &mut iommu_function, iommu_bar_index);
    let iommu_command = iommu_function.command().expect("the IOMMU command register");
    iommu_function
        .set_command(
            iommu_command
                .with(Command::MEMORY)
                .with(Command::BUS_MASTER)
                .with(Command::INTX_DISABLE),
        )
        .expect("a writable IOMMU command register");
    let iommu_delta = device::delta(iommu_bar);
    let iommu_common = device::subwindow(&iommu_registers, iommu_delta, iommu_transport.common());
    let iommu_notify = device::subwindow(&iommu_registers, iommu_delta, iommu_transport.notify());
    let iommu_config = device::subwindow(&iommu_registers, iommu_delta, iommu_transport.device());

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

    // The endpoint cannot initiate DMA until it has an isolated domain and all
    // of its queue mappings. INTx is disabled before MSI-X is enabled.
    let command = function.command().expect("the command register");
    let quiesced =
        command.with(Command::MEMORY).with(Command::INTX_DISABLE).without(Command::BUS_MASTER);
    function.set_command(quiesced).expect("a writable command register");
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
    let mut iommu_slots: [Option<Owner>; IOMMU_FRAMES] = [None; IOMMU_FRAMES];
    let iommu_arena = Arena::claim(&mut allocator, offset, IOMMU_TAG, &mut iommu_slots)
        .expect("contiguous frames for the IOMMU control queues");
    let mut slots: [Option<Owner>; DMA_FRAMES] = [None; DMA_FRAMES];
    let arena = Arena::claim(&mut allocator, offset, BLOCK_TAG, &mut slots)
        .expect("contiguous device frames past the kernel's own");

    let endpoint = device::requester(function.address());
    let mut iommu = Iommu::start(
        iommu_common,
        iommu_notify,
        iommu_config,
        iommu_transport.notify_multiplier(),
        u16::MAX,
        Poll,
        device::requester(iommu_function.address()),
        iommu_arena,
    )
    .expect("the IOMMU completes its handshake");
    iommu.attach(endpoint).expect("the block endpoint attaches to an isolated domain");
    report!(platform, "MOLT_IOMMU_OK: block endpoint attached before bus mastering");

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

    let mut iommu = block.reset().expect("the device stops and its mappings return");
    function.set_command(quiesced).expect("bus mastering stays off after reset");
    assert!(iommu.poll_faults().expect("the fault queue remains valid").is_none());
    report!(platform, "MOLT_IOMMU_FAULT_OK: no translation fault escaped the event queue");
    iommu.detach().expect("the empty block domain detaches");
    iommu.reset().expect("the IOMMU control queues stop and return");
    iommu_function
        .set_command(
            iommu_command
                .with(Command::MEMORY)
                .with(Command::INTX_DISABLE)
                .without(Command::BUS_MASTER),
        )
        .expect("IOMMU bus mastering stays off after reset");
    report!(platform, "MOLT_VIRTIO_RESET_OK: device stopped and frames reclaimed");
    vectored.stop(platform);
}
