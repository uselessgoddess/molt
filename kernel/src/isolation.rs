//! What isolating one endpoint's DMA behind the VirtIO IOMMU takes.
//!
//! The block, NVMe, and network smokes each attach a single endpoint to a
//! domain of its own on the same controller, in the same order: find the pair
//! on the bus, map the controller's BAR, grant it decode and DMA authority,
//! claim its identity-mapped control frames, start it, attach the endpoint,
//! and take all of that back down afterwards. That order is the isolation
//! guarantee — a device must not be able to initiate DMA before its domain
//! exists — so it lives here once rather than three times over.

use molt_arch::dma::Arena;
use molt_arch::iommu::DeviceId;
use molt_arch::memory::{Inventory, Owner};
use molt_arch::{FrameAllocator, Mmio, Platform};
use molt_pci::{Bus, Command, Function};
use molt_virtio::{Arrivals, Iommu, Transport};

use crate::device;

/// QEMU's `virtio-iommu-pci` function.
const VIRTIO_VENDOR: u16 = 0x1af4;
const VIRTIO_IOMMU: u16 = 0x1057;

/// Frames the control plane's own queues and buffers fit in.
pub(crate) const FRAMES: usize = 8;

/// The frame ownership slots [`arena`] tracks its claim in.
///
/// The caller owns them because the arena borrows them for as long as it
/// lives, and the arena outlives every call this module makes.
pub(crate) type Slots = [Option<Owner>; FRAMES];

/// Fresh slots for one control plane.
pub(crate) const SLOTS: Slots = [None; FRAMES];

/// The control plane's end of an interrupt line it was never routed one on.
///
/// The controller is started with `u16::MAX`, the "no vector" configuration,
/// because every request the kernel makes of it is answered before the call
/// returns. Waiting is spinning until the used ring moves.
pub(crate) struct Poll;

impl Arrivals for Poll {
    fn wait(&mut self) -> u64 {
        core::hint::spin_loop();
        0
    }
}

/// The IOMMU and the one endpoint `wanted` picked out, on bus `number`.
///
/// Both have to be there for a smoke to run: an endpoint without a controller
/// would have to be trusted with unmediated DMA, which is the thing this
/// kernel does not do.
pub(crate) fn pair<'bus>(
    window: &'bus Mmio<'_>,
    number: u8,
    wanted: impl Fn(&Function<'bus>) -> bool,
) -> Option<(Function<'bus>, Function<'bus>)> {
    let mut bus = Bus::new(window, number);
    let (mut target, mut controller) = (None, None);
    while let Some(function) = bus.function() {
        if function.vendor() == VIRTIO_VENDOR && function.device() == VIRTIO_IOMMU {
            controller = Some(function);
        } else if wanted(&function) {
            target = Some(function);
        }
    }
    Some((target?, controller?))
}

/// Claims the control plane's own frames, which stay identity-mapped.
pub(crate) fn arena<'slots>(
    allocator: &mut FrameAllocator<'_>,
    offset: u64,
    tag: u32,
    slots: &'slots mut Slots,
) -> Arena<'slots> {
    Arena::claim(allocator, offset, tag, slots).expect("contiguous frames for the IOMMU queues")
}

/// A mapped IOMMU function, before and after its queues are running.
///
/// The registers stay here rather than in the [`Iommu`], which borrows them:
/// the controller outlives the control plane it is cut from, so the command
/// register is still reachable once the queues have stopped.
pub(crate) struct Control<'bus> {
    function: Function<'bus>,
    registers: Mmio<'static>,
    transport: Transport,
    delta: u64,
    /// The command register as it was found, so teardown restores it.
    command: Command,
}

impl<'bus> Control<'bus> {
    /// Maps the controller's BAR and grants it decode and DMA authority.
    ///
    /// The IOMMU masters the bus for its own control queues, which are
    /// identity-mapped: it is the one function whose DMA nothing translates.
    pub(crate) fn open<P: Platform>(
        platform: &mut P,
        inventory: &Inventory<'_>,
        mut function: Function<'bus>,
    ) -> Self {
        let transport = Transport::probe(&function).expect("the IOMMU describes its structures");
        let index = transport.common().bar();
        assert!(
            transport.notify().bar() == index && transport.device().bar() == index,
            "IOMMU structures split across BARs",
        );
        let (bar, registers) = device::map_bar(platform, inventory, &mut function, index);
        let command = function.command().expect("the IOMMU command register");
        function
            .set_command(
                command.with(Command::MEMORY).with(Command::BUS_MASTER).with(Command::INTX_DISABLE),
            )
            .expect("IOMMU decode and DMA authority");
        Self { function, registers, transport, delta: device::delta(bar), command }
    }

    /// Starts the control queues and attaches `endpoint` to its own domain.
    ///
    /// The endpoint must still be quiesced: what comes back translates its
    /// DMA, and until it does the device has no addresses it may use.
    pub(crate) fn start<'slots>(
        &self,
        arena: Arena<'slots>,
        endpoint: DeviceId,
    ) -> Iommu<'slots, '_, Poll> {
        let common = device::subwindow(&self.registers, self.delta, self.transport.common());
        let notify = device::subwindow(&self.registers, self.delta, self.transport.notify());
        let config = device::subwindow(&self.registers, self.delta, self.transport.device());
        let mut iommu = Iommu::start(
            common,
            notify,
            config,
            self.transport.notify_multiplier(),
            u16::MAX,
            Poll,
            device::requester(self.function.address()),
            arena,
        )
        .expect("the IOMMU completes its handshake");
        iommu.attach(endpoint).expect("the endpoint attaches to an isolated domain");
        iommu
    }

    /// Drains the fault queue, detaches `endpoint`, and stops the controller.
    ///
    /// The endpoint's own reset has to have happened already: a device still
    /// able to issue a transaction after its domain is gone is exactly the
    /// fault this asserts nobody took.
    pub(crate) fn stop(&self, mut iommu: Iommu<'_, '_, Poll>, endpoint: DeviceId) {
        let fault = iommu.poll_faults().expect("the fault queue remains valid");
        assert!(fault.is_none(), "a translation fault escaped the event queue");
        iommu.detach(endpoint).expect("the empty domain detaches");
        iommu.reset().expect("the IOMMU control queues stop and return");
        self.function
            .set_command(
                self.command
                    .with(Command::MEMORY)
                    .with(Command::INTX_DISABLE)
                    .without(Command::BUS_MASTER),
            )
            .expect("IOMMU bus mastering stays off after reset");
    }
}
