//! What the kernel asks of the machine it booted on.
//!
//! One trait, implemented once per port. Every method defaults to refusing —
//! [`PlatformError::Unsupported`], `None`, or `false` — so a port grows by
//! answering more of them, and the kernel reports a skipped marker for the rest
//! rather than failing to build.

use crate::audit::Leaf;
use crate::irq::{FabricError, Sink};
use crate::mmio::DeviceMapper;
use crate::{
    BootInfo, ConfigSpace, DomainExit, DomainState, FrameCursor, InterruptFabric, Local,
    MappingError, RunError, SerialPort, SerialWriter, Smp, View, asid, memory, va, view,
};

/// Interrupt routing implemented by a concrete architecture crate.
pub trait InterruptController {
    fn init(&mut self) {}
    fn enable_irq(&mut self, irq: u8);
}

/// Terminal state reported by the kernel to its platform.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExitStatus {
    Success,
    Failure,
}

/// Failure while enabling a platform's hardware services.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlatformError {
    Unsupported,
    MissingPhysicalMemoryMap,
    InvalidHardware,
    Mapping(MappingError),
    Fabric(FabricError),
    MissingConfigSpace,
    /// Free RAM could not cover a request for frames.
    Frames(RunError),
    /// A view refused to be opened, filled, or emptied.
    View(view::Error),
}

impl From<RunError> for PlatformError {
    fn from(error: RunError) -> Self {
        Self::Frames(error)
    }
}

impl From<MappingError> for PlatformError {
    fn from(error: MappingError) -> Self {
        Self::Mapping(error)
    }
}

impl From<FabricError> for PlatformError {
    fn from(error: FabricError) -> Self {
        Self::Fabric(error)
    }
}

impl From<view::Error> for PlatformError {
    fn from(error: view::Error) -> Self {
        Self::View(error)
    }
}

/// Hardware services used directly by architecture-independent kernel code.
pub trait Platform: DeviceMapper + InterruptFabric + Local + Smp {
    type Serial: SerialPort;

    fn serial(&mut self) -> &mut Self::Serial;

    fn initialize(&mut self, _boot_info: &BootInfo<'_>) -> Result<(), PlatformError> {
        Ok(())
    }

    fn verify_exception_path(&mut self) -> bool {
        false
    }

    fn verify_owned_mapping(&mut self, _boot_info: &BootInfo<'_>) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported)
    }

    fn verify_image_protection(&mut self, _boot_info: &BootInfo<'_>) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported)
    }

    /// Maps, exercises, and audits an MMIO window from [`Inventory::device`].
    ///
    /// [`Inventory::device`]: crate::memory::Inventory::device
    fn verify_device_window(&mut self, _boot_info: &BootInfo<'_>) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported)
    }

    /// The largest leaf the boot mapping of RAM ended up using, read back out of
    /// the live tables: what the mapper meant to do is not evidence that the
    /// hardware translates that way.
    fn largest_ram_leaf(&mut self, _boot_info: &BootInfo<'_>) -> Result<Leaf, PlatformError> {
        Err(PlatformError::Unsupported)
    }

    /// The PCI configuration space firmware described, if there is one.
    fn config_space(&mut self, _boot_info: &BootInfo<'_>) -> Result<ConfigSpace, PlatformError> {
        Err(PlatformError::MissingConfigSpace)
    }

    /// Sends every interrupt line this platform raises to `sink`.
    fn route_interrupts(&mut self, _sink: &'static dyn Sink) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported)
    }

    /// What this machine's translation hardware can do, once
    /// [`initialize`](Self::initialize) has probed it.
    ///
    /// The VA allocator is cut from the address width and the domain budget
    /// follows from the tag width, so both are asked rather than assumed. A port
    /// that has not probed returns `None`, and hands out no addresses.
    fn address_space(&self) -> Option<va::Widths> {
        None
    }

    /// A cursor past the RAM the kernel's own tables and image already own, for
    /// a driver resuming a [`FrameAllocator`](crate::FrameAllocator) to back DMA.
    ///
    /// A snapshot, not a reservation: a later [`claim_ram`](Self::claim_ram)
    /// moves the platform past it, so a cursor taken before that call names
    /// frames somebody else now owns. Read it again rather than keeping one.
    fn free_frames(&self) -> Option<FrameCursor> {
        None
    }

    /// Hands out `count` frames of that same free RAM, for keeps.
    ///
    /// [`free_frames`](Self::free_frames) only says where the kernel's mappings
    /// end, so two callers resuming there get the same RAM. This moves the
    /// platform's cursor past what it returns, so the span is the caller's for
    /// the life of the kernel: no free list stands behind it, and the next
    /// consumer starts where the cursor now is.
    fn claim_ram(
        &mut self,
        _boot_info: &BootInfo<'_>,
        _count: u64,
    ) -> Result<memory::Span, PlatformError> {
        Err(PlatformError::Unsupported)
    }

    /// Opens an empty view of the one address space, tagged `asid`.
    ///
    /// Empty means empty, the kernel's own text included, which is what makes a
    /// tier-2 domain a boundary rather than a convention ([`view`]).
    fn open_view(&mut self, _asid: asid::Asid) -> Result<View, PlatformError> {
        Err(PlatformError::Unsupported)
    }

    /// Makes `extent` reachable from `view`, backed by `span`, with `rights`.
    ///
    /// At the extent's own class, so a gigabyte-class extent costs one entry per
    /// gigabyte. The span has to cover the extent and share its alignment, or a
    /// leaf would name memory nobody meant to hand over
    /// ([`view::Error::Backing`]).
    fn grant(
        &mut self,
        _view: View,
        _extent: &va::Extent,
        _span: memory::Span,
        _rights: memory::Rights,
    ) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported)
    }

    /// Takes `extent` back out of `view`, and says how many leaves went.
    ///
    /// Clears the leaves and stops. The tables above them stay: one that held a
    /// leaf will hold the next, and freeing it would cost a second shootdown.
    /// The flush and the return of the addresses are the caller's, in that
    /// order, for the reason [`view`] spells out.
    fn revoke(&mut self, _view: View, _extent: &va::Extent) -> Result<u64, PlatformError> {
        Err(PlatformError::Unsupported)
    }

    /// Cuts the leaf covering `address` in `view` into [`Class::FANOUT`] leaves
    /// of the class below, and says which class that is.
    ///
    /// The hardware half of [`Leaves::split`]: revoking one megabyte of a
    /// gigabyte grant must stop that leaf translating the other 1023. The
    /// address keeps translating to the same frame throughout, so the shootdown
    /// the caller owes is for the coarse entry a core may hold, not for an
    /// unmapping.
    ///
    /// [`Class::FANOUT`]: crate::va::Class::FANOUT
    /// [`Leaves::split`]: crate::refcount::Leaves::split
    fn split_leaf(&mut self, _view: View, _address: u64) -> Result<va::Class, PlatformError> {
        Err(PlatformError::Unsupported)
    }

    /// What `view` translates `address` through, read back out of its tables.
    ///
    /// `None` for an address the view cannot reach — which is what makes a
    /// domain marker evidence about the hardware rather than about intent.
    fn resident(&self, _view: View, _address: u64) -> Option<Leaf> {
        None
    }

    /// Direct-map address of RAM already returned by [`claim_ram`](Self::claim_ram).
    ///
    /// A raw pointer because the kernel and the domain intentionally alias ring
    /// pages. The caller still owns the claimed span and decides when either
    /// side may access it.
    fn claimed_pointer(&mut self, _span: memory::Span) -> Result<*mut u8, PlatformError> {
        Err(PlatformError::Unsupported)
    }

    /// Enters or resumes a user context in its hardware-protected view.
    fn enter_domain(&mut self, _state: &mut DomainState) -> Result<DomainExit, PlatformError> {
        Err(PlatformError::Unsupported)
    }

    fn terminate(&mut self, status: ExitStatus) -> !;
}

/// Reports a bare-metal panic through the selected platform.
pub fn panic_handler<P>(info: &core::panic::PanicInfo<'_>) -> !
where
    P: Platform + Default,
{
    use core::fmt::Write as _;

    let mut platform = P::default();
    let serial = platform.serial();
    serial.init();
    let _ = writeln!(SerialWriter::new(serial), "MOLT_PANIC: {info}");
    platform.terminate(ExitStatus::Failure)
}
