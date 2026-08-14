//! What isolating one endpoint's DMA behind the VirtIO IOMMU takes.
//!
//! The block, NVMe and network smokes each attach one endpoint to a domain of
//! its own, in the same order: find the pair on the bus, map the controller's
//! BAR, grant it decode and DMA authority, claim its identity-mapped control
//! frames, start it, attach the endpoint, then take all of it back down. The
//! order *is* the isolation guarantee — no device initiates DMA before its
//! domain exists — so it lives here once rather than three times.

use molt_arch::dma::Arena;
use molt_arch::iommu::DeviceId;
use molt_arch::memory::{Inventory, Owner};
use molt_arch::{FrameAllocator, Mmio, Platform};
use molt_pci::{Bar, Bus, Command, Function};
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

/// Waiting on the control queues, which is spinning until the used ring moves.
///
/// The controller is started with `u16::MAX`, the "no vector" configuration,
/// because every request the kernel makes of it is answered before the call
/// returns.
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

/// Shows on the machine what `Domains::reserve` shows in a unit test: which
/// domain an endpoint lands in is the kernel's choice and nothing else's.
///
/// `ahead` is a second quiesced endpoint carrying the *higher* requester ID.
/// Attaching it first gives it the *lower* domain, so the numbers come out
/// opposite to the identifiers the devices carry — which is the claim, since
/// the order is a line of kernel code and no device can be earlier than the
/// kernel put it.
///
/// Nothing lasting changes: `endpoint` has no mappings yet, so it can be
/// detached and comes back in a domain of its own, and `ahead` gives its domain
/// back before this returns. Returns the two domains, `ahead`'s first.
pub(crate) fn ordered(
    iommu: &mut Iommu<'_, '_, Poll>,
    endpoint: DeviceId,
    ahead: DeviceId,
) -> (u32, u32) {
    assert!(ahead.get() > endpoint.get(), "the witness carries the lower requester ID");
    iommu.detach(endpoint).expect("an endpoint with no mappings detaches");
    iommu.attach(ahead).expect("a second endpoint attaches to a domain of its own");
    iommu.attach(endpoint).expect("the endpoint attaches behind it");

    let first = iommu.domain_of(ahead).expect("the witness is attached");
    let second = iommu.domain_of(endpoint).expect("the endpoint is attached");
    assert!(first != 0 && second != 0, "an endpoint landed in the domain molt never hands out");
    assert!(first < second, "the endpoint that attached first did not take the lower domain");
    iommu.detach(ahead).expect("the witness gives its domain back");
    (first, second)
}

/// A mapped IOMMU function, before and after its queues are running.
///
/// The registers stay here rather than in the [`Iommu`] that borrows them, so
/// the command register is still reachable once the queues have stopped.
pub(crate) struct Control<'bus> {
    function: Function<'bus>,
    registers: Mmio<'static>,
    transport: Transport,
    bar: Bar,
    /// The command register as it was found, so teardown restores it.
    command: Command,
}

impl<'bus> Control<'bus> {
    /// Maps the controller's BAR and grants it decode and DMA authority.
    ///
    /// The IOMMU masters the bus for its own control queues, which stay
    /// identity-mapped: it is the one function whose DMA nothing translates.
    pub(crate) fn open<P: Platform>(
        platform: &mut P,
        inventory: &Inventory<'_>,
        mut function: Function<'bus>,
    ) -> Self {
        let (transport, index) = device::transport(&function);
        let (bar, registers) = device::map_bar(platform, inventory, &mut function, index);
        let command = function.command().expect("the IOMMU command register");
        let authority = device::quiesced(command).with(Command::BUS_MASTER);
        function.set_command(authority).expect("IOMMU decode and DMA authority");
        Self { function, registers, transport, bar, command }
    }

    /// Starts the control queues and attaches `endpoint` to its own domain.
    ///
    /// The endpoint must still be quiesced: until this returns, nothing
    /// translates its DMA and it has no addresses it may use.
    pub(crate) fn start<'slots>(
        &self,
        arena: Arena<'slots>,
        endpoint: DeviceId,
    ) -> Iommu<'slots, '_, Poll> {
        let (common, notify, config) =
            device::structures(&self.registers, self.bar, &self.transport);
        let mut iommu = Iommu::start(
            common,
            notify,
            config,
            self.transport.notify_multiplier(),
            u16::MAX,
            Poll,
            self.function.address().requester(),
            arena,
        )
        .expect("the IOMMU completes its handshake");
        iommu.attach(endpoint).expect("the endpoint attaches to an isolated domain");
        iommu
    }

    /// Drains the fault queue, detaches `endpoint`, and stops the controller.
    ///
    /// The endpoint's own reset must already have happened: a device able to
    /// issue a transaction after its domain is gone is the fault this asserts
    /// nobody took.
    pub(crate) fn stop(&self, mut iommu: Iommu<'_, '_, Poll>, endpoint: DeviceId) {
        let fault = iommu.poll_faults().expect("the fault queue remains valid");
        assert!(fault.is_none(), "a translation fault escaped the event queue");
        iommu.detach(endpoint).expect("the empty domain detaches");
        iommu.reset().expect("the IOMMU control queues stop and return");
        self.function
            .set_command(device::quiesced(self.command))
            .expect("IOMMU bus mastering stays off after reset");
    }
}
