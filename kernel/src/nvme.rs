use alloc::boxed::Box;

use molt_arch::dma::Arena;
use molt_arch::memory::{Inventory, Owner, Rights};
use molt_arch::{BootInfo, FrameAllocator, Platform, SerialWriter};
use molt_block::{BLOCK, BlockOp, Device, Disk, Queue, SECTOR};
use molt_core::ring::RequestId;
use molt_kernel::report;
use molt_nvme::{Config, Prepared};
use molt_pci::{Command, bus_span};

use crate::{device, isolation};

const MASS_STORAGE: u8 = 0x01;
const NVM: u8 = 0x08;
const NVME: u8 = 0x02;
const DMA_FRAMES: usize = 13;
const DMA_TAG: u32 = 0x6e76_6d65;
const IOMMU_TAG: u32 = 0x10ac;
const SIGNATURE: [u8; 8] = molt_fs::MAGIC;

pub fn smoke<P: Platform>(boot_info: &BootInfo<'_>, platform: &mut P) {
    let Ok(space) = platform.config_space(boot_info) else {
        return;
    };
    let (Some(cursor), Some(offset)) = (platform.free_frames(), boot_info.physical_offset()) else {
        report!(platform, "MOLT_NVME_SKIPPED: this platform hands out no DMA frames");
        return;
    };

    let inventory = Inventory::new(boot_info.memory_map());
    let bus_zero = bus_span(space, space.first_bus()).expect("bus zero inside the ECAM window");
    let ecam = inventory.device(bus_zero).expect("the ECAM window is outside kernel RAM");
    let window = platform.map_device(ecam, Rights::READ_WRITE).expect("a mappable ECAM window");
    let found = isolation::pair(&window, space.first_bus(), |function| {
        let class = function.class();
        class.class() == MASS_STORAGE && class.subclass() == NVM && class.interface() == NVME
    });
    let Some((mut function, controller)) = found else {
        report!(platform, "MOLT_NVME_SKIPPED: no NVMe/IOMMU pair on bus zero");
        return;
    };
    let control = isolation::Control::open(platform, &inventory, controller);

    let capability = function.msix().expect("NVMe exposes MSI-X");
    let table_index = capability.table_bar();
    let (bar, mapped_bar) = device::map_bar(platform, &inventory, &mut function, 0);
    let (table_bar, table_mapping) = if table_index == 0 {
        (bar, None)
    } else {
        let (table_bar, mapping) =
            device::map_bar(platform, &inventory, &mut function, table_index);
        (table_bar, Some(mapping))
    };
    let command = function.command().expect("the NVMe command register");
    let quiesced =
        command.with(Command::MEMORY).with(Command::INTX_DISABLE).without(Command::BUS_MASTER);
    function.set_command(quiesced).expect("NVMe remains quiesced before mappings exist");
    let table = table_mapping.as_ref().unwrap_or(&mapped_bar);
    let vectored = device::route(platform, &function, capability, table, device::delta(table_bar));
    let registers = mapped_bar
        .subwindow(device::delta(bar), bar.bytes())
        .expect("the NVMe registers fit BAR zero");

    let mut allocator = FrameAllocator::resume(boot_info.memory_map(), cursor);
    let mut iommu_slots = isolation::SLOTS;
    let iommu_arena = isolation::arena(&mut allocator, offset, IOMMU_TAG, &mut iommu_slots);
    let mut slots: [Option<Owner>; DMA_FRAMES] = [None; DMA_FRAMES];
    let arena = Arena::claim(&mut allocator, offset, DMA_TAG, &mut slots)
        .expect("contiguous frames for NVMe queues and payloads");
    let endpoint = device::requester(function.address());
    let iommu = control.start(iommu_arena, endpoint);
    let prepared = Prepared::prepare(
        Config::new(registers, endpoint, vectored.index(), vectored.line()),
        iommu,
        arena,
    )
    .expect("NVMe DMA resources prepare while bus mastering is off");
    function
        .set_command(quiesced.with(Command::BUS_MASTER))
        .expect("NVMe bus mastering follows mappings");
    let mut nvme =
        prepared.enable().expect("the NVMe controller enables and identifies namespace 1");
    report!(
        platform,
        "MOLT_NVME_IOMMU_OK: {} DMA regions mapped before bus mastering",
        nvme.mapper().mapped(),
    );

    let first = RequestId::new(0x20);
    let second = RequestId::new(0x21);
    assert!(
        nvme.start(
            first,
            BlockOp::Read { sector: 0, bytes: SECTOR, buffer: Box::new([0; BLOCK]) },
        )
        .is_ok(),
        "the first NVMe depth probe submits",
    );
    assert!(
        nvme.start(
            second,
            BlockOp::Read { sector: 1, bytes: SECTOR, buffer: Box::new([0; BLOCK]) },
        )
        .is_ok(),
        "the second NVMe depth probe submits before the first completes",
    );
    let mut first_seen = false;
    for _ in 0..2 {
        let (id, done) = nvme.reap().expect("an NVMe depth probe completes");
        done.result.expect("an NVMe depth probe succeeds");
        if id == first {
            let bytes = done.buffer.expect("an NVMe read returns its buffer");
            assert_eq!(&bytes[..SIGNATURE.len()], &SIGNATURE, "NVMe sector zero changed");
            first_seen = true;
        }
    }
    assert!(first_seen, "the first NVMe request never completed");
    report!(platform, "MOLT_NVME_DEPTH_OK: two reads live at depth {}", nvme.depth());

    let expected = [0x6eu8; SECTOR];
    nvme.write(8, &expected).expect("a basic NVMe write succeeds");
    nvme.flush().expect("the NVMe namespace flushes");
    let mut actual = [0u8; SECTOR];
    nvme.read(8, &mut actual).expect("the NVMe write reads back");
    assert_eq!(actual, expected, "the NVMe write did not persist through its flush");
    report!(platform, "MOLT_NVME_OK: identify, read, write, and flush completed");

    let iommu = nvme.reset().expect("the NVMe controller stops and unmaps its pages");
    function.set_command(quiesced).expect("NVMe bus mastering stays off after reset");
    control.stop(iommu, endpoint);
    report!(platform, "MOLT_NVME_RESET_OK: controller stopped before frames returned");
    vectored.stop(platform);
}
