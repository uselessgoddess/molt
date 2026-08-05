//! Kernel-owned translation tables, Sv39 through Sv57.
//!
//! [`init`] maps each image section with W^X rights and only firmware-usable RAM
//! beyond the image. Reserved ranges and device holes remain unmapped so the
//! live-table audit can reject stray leaves.
//!
//! Every one of those mappings lives below 512 GiB, so the tree is built three
//! levels deep and then rooted as deep as the hart allows: see [`enable`]. The
//! width that buys is not for the kernel, which does not need it — it is the
//! address space [`docs/address-space.md`] spends on user domains.
//!
//! [`docs/address-space.md`]: https://github.com/uselessgoddess/molt/blob/main/docs/address-space.md

use core::arch::asm;
use core::cell::UnsafeCell;

use molt_arch::audit::{Audit, Declared, Leaf, MappedRange, PageWalk};
use molt_arch::memory::{Cache, Device, Inventory, Kind, Rights, Span};
use molt_arch::{
    BootInfo, FrameAllocator, FrameCursor, FramePool, ImageSection, MapPermissions, MappingError,
    Mmio, PageProtection, PhysicalFrame, PlatformError, UsableRegions,
};

use crate::satp::Mode;

const PTE_V: u64 = 1 << 0;
const PTE_R: u64 = 1 << 1;
const PTE_W: u64 = 1 << 2;
const PTE_X: u64 = 1 << 3;
const PTE_A: u64 = 1 << 6;
const PTE_D: u64 = 1 << 7;

/// Non-zero permission bits distinguish a leaf from a table pointer.
const PTE_RWX: u64 = PTE_R | PTE_W | PTE_X;

const PAGE_4K: usize = 4096;
const PAGE_2M: usize = 2 * 1024 * 1024;

const PROBE_VALUE: u64 = 0x004d_4f4c_545f_5758;

/// The depth [`init`] builds at, before [`enable`] roots the tree deeper.
const BUILD_LEVEL: usize = Mode::Sv39.level();

unsafe extern "C" {
    static __text_start: u8;
    static __text_end: u8;
    static __rodata_start: u8;
    static __rodata_end: u8;
    static __data_start: u8;
    static __kernel_end: u8;
}

/// Usable free-RAM ranges the audit stores inline. QEMU `virt` exposes one.
const MAX_RAM_RANGES: usize = 8;

/// Device windows the kernel may map: UART, ECAM, and a handful of BARs.
const MAX_DEVICE_RANGES: usize = 8;

/// Three image sections, free-RAM regions, and device windows.
type MappingLog = Declared<{ 3 + MAX_RAM_RANGES + MAX_DEVICE_RANGES }>;

/// Page-table frames drained at boot for mappings made after the memory map
/// is gone. One level-0 table per 2 MiB a window spans, one level-1 per
/// gigabyte; the largest window is one ECAM bus (256 functions, one table).
/// Running short is `MappingError::OutOfFrames`, not corruption.
const TABLE_FRAMES: usize = 160;

macro_rules! bound {
    ($symbol:ident) => {
        (&raw const $symbol) as usize
    };
}

struct BootPaging {
    root: *mut u64,
    /// The level `root` sits at, which is [`Mode::level`] of the mode the hart
    /// accepted. Every walk in this module starts from it.
    top: usize,
    mode: Mode,
    cursor: FrameCursor,
    /// Table frames drained out of the boot allocator while the memory map was
    /// still borrowable, for mappings made after it is gone.
    pool: FramePool<TABLE_FRAMES>,
    /// Every mapped range, in the order [`init`] built them.
    log: MappingLog,
    /// The next free device address; bumps forward and never back, so a window
    /// handed out once is never handed out again.
    devices: usize,
}

struct Active(UnsafeCell<Option<BootPaging>>);

// SAFETY: the boot address space is built and used on the single boot hart
// before any other hart is started, so there is no concurrent access to share.
unsafe impl Sync for Active {}

static ACTIVE: Active = Active(UnsafeCell::new(None));

fn active() -> Result<&'static mut BootPaging, PlatformError> {
    // SAFETY: single boot hart, traps do not touch this cell, and the returned
    // borrow is confined to one call.
    unsafe { &mut *ACTIVE.0.get() }.as_mut().ok_or(PlatformError::Mapping(MappingError::Unmapped))
}

/// Builds the per-section boot address space and turns translation on.
pub fn init(boot_info: &BootInfo<'_>) -> Result<(), PlatformError> {
    let kernel_end = bound!(__kernel_end) as u64;
    let mut frames = FrameAllocator::above(boot_info.memory_map(), kernel_end);
    let root = alloc_table(&mut frames)?;

    let mut log = MappingLog::new();
    map_section(
        root,
        &mut frames,
        &mut log,
        ImageSection::Text,
        bound!(__text_start),
        bound!(__text_end),
    )?;
    map_section(
        root,
        &mut frames,
        &mut log,
        ImageSection::Rodata,
        bound!(__rodata_start),
        bound!(__rodata_end),
    )?;
    map_section(
        root,
        &mut frames,
        &mut log,
        ImageSection::Data,
        bound!(__data_start),
        bound!(__kernel_end),
    )?;

    for range in UsableRegions::above(boot_info.memory_map(), kernel_end) {
        let start = usize::try_from(range.start())
            .map_err(|_| PlatformError::Mapping(MappingError::InvalidAddress))?;
        let end = usize::try_from(range.end())
            .map_err(|_| PlatformError::Mapping(MappingError::InvalidAddress))?;
        map_range(root, BUILD_LEVEL, &mut frames, start, end, PTE_R | PTE_W, Granularity::LargeOk)?;
        log.push(MappedRange::ram(range.start(), range.end())).map_err(PlatformError::Mapping)?;
    }

    // SAFETY: every address the kernel executes from, reads, or writes — code,
    // constants, stack, and the page tables themselves — was just identity
    // mapped, so translation can be switched on in place.
    let (root, mode) = unsafe { enable(root, &mut frames)? };

    let mut pool = FramePool::empty();
    pool.fill(&mut frames);

    let cursor = frames.cursor();
    // SAFETY: same reasoning as `active`; this runs once on the boot hart.
    unsafe {
        *ACTIVE.0.get() = Some(BootPaging {
            root,
            top: mode.level(),
            mode,
            cursor,
            pool,
            log,
            devices: DEVICE_REGION,
        });
    }
    Ok(())
}

/// Roots `built` as deep as the hart allows and switches translation on.
///
/// Nothing about the tree below changes: every mapping [`init`] made is inside
/// the first 512 GiB, so a wider mode needs no new leaves, only a root whose
/// first entry points at the root below it. Two frames buy the option on both.
///
/// The probe itself is free, because the privileged specification made it so:
///
/// > If `satp` is written with an unsupported MODE, the entire write has no
/// > effect; no fields in `satp` are modified.
///
/// So the widest write that reads back is the widest mode the hart implements,
/// a failed attempt leaves the hart exactly where it was — untranslated, still
/// running from identity-mapped physical addresses — and no rebuild is needed
/// between tries.
///
/// # Safety
///
/// `built` must be a three-level tree covering the code, stack, and tables of
/// the caller, because the first accepted write translates the next
/// instruction fetch through it.
unsafe fn enable(
    built: *mut u64,
    frames: &mut dyn Frames,
) -> Result<(*mut u64, Mode), PlatformError> {
    let mut roots = [built; Mode::WIDEST.len()];
    for level in (BUILD_LEVEL + 1)..=Mode::Sv57.level() {
        let above = alloc_table(frames)?;
        // SAFETY: a freshly allocated, zeroed, identity-mapped table frame;
        // entry zero is the only one a lower root can hang from.
        unsafe { above.write(pte(roots[level - BUILD_LEVEL - 1] as u64, 0)) };
        roots[level - BUILD_LEVEL] = above;
    }

    for mode in Mode::WIDEST {
        let root = roots[mode.level() - BUILD_LEVEL];
        // SAFETY: forwarded from this function's contract; `root` roots the
        // caller's tree at exactly `mode.level()`.
        if unsafe { switch(mode, root as u64) } {
            return Ok((root, mode));
        }
    }
    // A hart that implements none of Sv39, Sv48, or Sv57 cannot run this kernel.
    Err(PlatformError::Mapping(MappingError::Backend))
}

/// Writes `satp` and reports whether the hart kept the value.
///
/// # Safety
///
/// `root_phys` must root a tree valid for `mode` that maps the caller's code
/// and stack: if the write takes, the next instruction is already translated.
unsafe fn switch(mode: Mode, root_phys: u64) -> bool {
    let wanted = mode.field() | (root_phys >> 12);
    let live: u64;
    // SAFETY: the flush brackets the `satp` write so no stale translation is
    // used, and the read-back is a plain CSR read either way.
    unsafe {
        asm!(
            "csrw satp, {wanted}",
            "sfence.vma",
            "csrr {live}, satp",
            wanted = in(reg) wanted,
            live = out(reg) live,
            options(nostack),
        );
    }
    live == wanted
}

fn map_section(
    root: *mut u64,
    frames: &mut dyn Frames,
    log: &mut MappingLog,
    section: ImageSection,
    start: usize,
    end: usize,
) -> Result<(), PlatformError> {
    let flags = match section {
        ImageSection::Text => PTE_R | PTE_X,
        ImageSection::Rodata => PTE_R,
        ImageSection::Data => PTE_R | PTE_W,
    };
    map_range(root, BUILD_LEVEL, frames, start, end, flags, Granularity::Small)?;
    let aligned_start = align_down(start, PAGE_4K) as u64;
    let aligned_end =
        align_up(end, PAGE_4K).ok_or(PlatformError::Mapping(MappingError::InvalidAddress))? as u64;
    log.push(MappedRange::section(section, aligned_start, aligned_end))
        .map_err(PlatformError::Mapping)
}

/// The `satp` the boot hart runs on, for a hart coming up beside it.
pub fn satp() -> Option<u64> {
    active().ok().map(|state| state.mode.field() | (state.root as u64 >> 12))
}

/// The translation mode the boot hart accepted, once [`init`] has run.
pub fn mode() -> Option<Mode> {
    active().ok().map(|state| state.mode)
}

/// A cursor past the RAM the boot address space is already built out of.
pub fn free_frames() -> Option<FrameCursor> {
    active().ok().map(|state| state.cursor)
}

/// Hands out `count` frames of that RAM and moves the cursor past them.
///
/// Usable RAM is identity mapped read-write at boot, so the caller reaches the
/// span at its physical address without mapping anything.
///
/// The move is the point and it is permanent: the frames are the caller's for
/// good, no later claim can be handed the same ones, and a [`FrameCursor`] read
/// before this call is stale.
pub fn claim_ram(boot_info: &BootInfo<'_>, count: u64) -> Result<Span, PlatformError> {
    let state = active()?;
    let mut frames = FrameAllocator::resume(boot_info.memory_map(), state.cursor);
    let span = frames.run(count)?;
    state.cursor = frames.cursor();
    Ok(span)
}

pub fn verify_owned_mapping(boot_info: &BootInfo<'_>) -> Result<(), PlatformError> {
    let state = active()?;
    let mut frames = FrameAllocator::resume(boot_info.memory_map(), state.cursor);

    let permissions = MapPermissions::new(true, false).map_err(PlatformError::Mapping)?;
    let mut leaf = PTE_A | PTE_D;
    if permissions.is_write() {
        leaf |= PTE_R | PTE_W;
    }
    if permissions.is_execute() {
        leaf |= PTE_X;
    }

    // The probe address belongs to the mode: on Sv48 it is at 64 TiB and on
    // Sv57 at 16 PiB, addresses no narrower mode could have produced. Writing
    // and reading one back is what turns "the wide mode was accepted" from a
    // CSR read-back into a translation the hardware actually performed.
    let probe_va = state.mode.probe_va();
    let probe = alloc_frame(&mut frames)?;
    map_leaf(state.root, state.top, &mut frames, probe_va, probe, leaf, 0)?;
    state.cursor = frames.cursor();
    // SAFETY: the new leaf is visible in memory; the fence retires any negative
    // caching of `probe_va` from before it existed.
    unsafe {
        asm!("sfence.vma", options(nostack));
    }

    let pointer = probe_va as *mut u64;
    // SAFETY: `probe_va` is now mapped present, readable, and writable to a
    // uniquely owned frame; the access is naturally aligned and volatile.
    let outcome = unsafe {
        pointer.write_volatile(PROBE_VALUE);
        if pointer.read_volatile() != PROBE_VALUE {
            Err(PlatformError::Mapping(MappingError::Backend))
        } else {
            Ok(())
        }
    };
    // Remove the probe before auditing the declared mappings.
    // SAFETY: nothing else references `probe_va` after this scope, and the
    // fence retires the stale translation before another access can hit it.
    unsafe {
        clear_leaf(state.root, state.top, probe_va);
        asm!("sfence.vma", options(nostack));
    }
    outcome
}

/// Clears the level-0 PTE that translates `va`, if one exists.
///
/// # Safety
///
/// The caller must ensure no other thread holds a cached translation for `va`
/// after this returns; the boot hart is single-threaded, so a following
/// `sfence.vma` on it is enough.
unsafe fn clear_leaf(root: *mut u64, top: usize, va: usize) {
    let mut table = root;
    for level in (1..=top).rev() {
        // SAFETY: `table` addresses a 512-entry table frame, identity mapped
        // by `init`, and the index is masked to nine bits.
        let entry = unsafe { *table.add(index(va, level)) };
        if entry & PTE_V == 0 || entry & PTE_RWX != 0 {
            return;
        }
        table = ((entry >> 10) << 12) as *mut u64;
    }
    // SAFETY: `table` addresses the level-0 table frame covering `va`.
    unsafe {
        table.add(index(va, 0)).write(0);
    }
}

pub fn verify_image_protection(boot_info: &BootInfo<'_>) -> Result<(), PlatformError> {
    let state = active()?;
    let inventory = Inventory::new(boot_info.memory_map());
    let walk = TableWalk { root: state.root, top: state.top, inventory: &inventory };
    state.log.audit().cover(&walk).map_err(PlatformError::Mapping)?;
    walk_leaves(state.root, state.top, &state.log.audit(), &inventory)
}

const UART_MMIO: u64 = 0x1000_0000;
const UART_WINDOW: usize = 0x3000_0000;
const UART_THR: usize = 0;
const UART_LSR: usize = 5;
const UART_LSR_THRE: u8 = 1 << 5;

/// Maps, exercises, and audits a typed UART device window.
pub fn verify_device_window(boot_info: &BootInfo<'_>) -> Result<(), PlatformError> {
    let state = active()?;
    let inventory = Inventory::new(boot_info.memory_map());
    let span = Span::frames(UART_MMIO, 1).map_err(|_| address_error())?;
    let window = inventory.device(span).map_err(|_| address_error())?;
    let (rights, cache) = window.mapping(Rights::READ_WRITE).map_err(PlatformError::Mapping)?;
    debug_assert_eq!(cache, Cache::Device);

    let mut frames = FrameAllocator::resume(boot_info.memory_map(), state.cursor);
    map_leaf(
        state.root,
        state.top,
        &mut frames,
        UART_WINDOW,
        window.span().start(),
        leaf_flags(rights),
        0,
    )?;
    state.cursor = frames.cursor();
    state
        .log
        .push(MappedRange::device(UART_WINDOW as u64, UART_WINDOW as u64 + PAGE_4K as u64))
        .map_err(PlatformError::Mapping)?;
    // SAFETY: the new leaf is in memory; the fence retires any negative caching
    // of `UART_WINDOW` from before it existed.
    unsafe {
        asm!("sfence.vma", options(nostack));
    }

    for byte in b"MOLT_UART_WINDOW: ns16550a\n" {
        // SAFETY: the window is mapped read/write to the UART's own frame, and
        // both registers are single bytes at fixed offsets within it.
        unsafe {
            while (UART_WINDOW as *const u8).add(UART_LSR).read_volatile() & UART_LSR_THRE == 0 {}
            (UART_WINDOW as *mut u8).add(UART_THR).write_volatile(*byte);
        }
    }

    let walk = TableWalk { root: state.root, top: state.top, inventory: &inventory };
    state.log.audit().cover(&walk).map_err(PlatformError::Mapping)?;
    walk_leaves(state.root, state.top, &state.log.audit(), &inventory)
}

/// Where device windows live: 128 GiB, clear of RAM and inside the narrowest
/// mode's lower canonical half, because [`init`] builds the tree before it
/// knows which mode the hart will take. Not identity-mapped — a driver that
/// reaches a device by guessing its physical address has not been given a
/// capability.
const DEVICE_REGION: usize = 0x20_0000_0000;
const DEVICE_REGION_END: usize = DEVICE_REGION + (1 << 30);

/// Maps one device window into the boot address space.
///
/// A page table without `Svpbmt` has no cacheability bits in a PTE: uncached
/// ordering comes from the physical address's PMA. The check that matters is
/// [`Inventory::device`] refusing a span firmware called RAM — a window into
/// RAM would be cacheable by hardware regardless of what this code asks.
pub fn map_device(window: Device, rights: Rights) -> Result<Mmio<'static>, MappingError> {
    let (rights, _cache) = window.mapping(rights)?;
    let span = window.span();
    let bytes = usize::try_from(span.bytes()).map_err(|_| MappingError::InvalidAddress)?;

    let state = active().map_err(mapping_error)?;
    let base = state.devices;
    let end = base.checked_add(bytes).ok_or(MappingError::InvalidAddress)?;
    if end > DEVICE_REGION_END {
        return Err(MappingError::OutOfFrames);
    }

    state.log.push(MappedRange::device(base as u64, end as u64))?;
    let mut address = 0;
    while address < bytes {
        // Level zero only: a megapage would reach past the window.
        map_leaf(
            state.root,
            state.top,
            &mut state.pool,
            base + address,
            span.start() + address as u64,
            leaf_flags(rights),
            0,
        )
        .map_err(mapping_error)?;
        address += PAGE_4K;
    }
    // SAFETY: the new leaves are in memory; the fence retires any negative
    // caching of the window from before it was mapped.
    unsafe {
        asm!("sfence.vma", options(nostack));
    }

    state.devices = end.next_multiple_of(PAGE_2M);
    // SAFETY: every frame of `span` was just mapped at `base`, never
    // executable, and never unmapped. The cursor only moves forward.
    Ok(unsafe { Mmio::new(base as *mut u8, span.bytes()) })
}

/// The mapping error inside a [`PlatformError`], for the paths that report one.
fn mapping_error(error: PlatformError) -> MappingError {
    match error {
        PlatformError::Mapping(error) => error,
        _ => MappingError::Backend,
    }
}

/// Leaf flags for `rights`, with access and dirty pre-set.
fn leaf_flags(rights: Rights) -> u64 {
    let mut flags = PTE_A;
    if rights.is_read() {
        flags |= PTE_R;
    }
    if rights.is_write() {
        flags |= PTE_R | PTE_W | PTE_D;
    }
    if rights.is_execute() {
        flags |= PTE_X;
    }
    flags
}

fn address_error() -> PlatformError {
    PlatformError::Mapping(MappingError::InvalidAddress)
}

fn walk_leaves(
    root: *const u64,
    top: usize,
    audit: &Audit<'_>,
    inventory: &Inventory<'_>,
) -> Result<(), PlatformError> {
    walk_table(root, top, 0, audit, inventory)
}

fn walk_table(
    table: *const u64,
    level: usize,
    base: u64,
    audit: &Audit<'_>,
    inventory: &Inventory<'_>,
) -> Result<(), PlatformError> {
    let span_bits = 12 + 9 * level;
    for i in 0..512u64 {
        // SAFETY: `table` points at a 512-entry table frame, identity mapped
        // read/write by `init`, and the offset is masked to nine bits.
        let entry = unsafe { *table.add(i as usize) };
        if entry & PTE_V == 0 {
            continue;
        }
        let start = base | (i << span_bits);
        if entry & PTE_RWX != 0 {
            let size = 1u64 << span_bits;
            audit
                .accepts(Leaf::new(start, size, protection(entry, inventory)))
                .map_err(PlatformError::Mapping)?;
            continue;
        }
        if level == 0 {
            return Err(PlatformError::Mapping(MappingError::Backend));
        }
        let next = ((entry >> 10) << 12) as *const u64;
        walk_table(next, level - 1, start, audit, inventory)?;
    }
    Ok(())
}

/// Decodes leaf rights and the physical memory attribute.
///
/// This target lacks `Svpbmt`, so the firmware map supplies the PMA: described
/// memory is write-back and holes are device-ordered.
fn protection(entry: u64, inventory: &Inventory<'_>) -> PageProtection {
    let physical = (entry >> 10) << 12;
    let cache = match inventory.kind(physical) {
        Kind::Ram | Kind::Image | Kind::Reserved => Cache::WriteBack,
        Kind::Device => Cache::Device,
    };
    PageProtection::new(entry & PTE_R != 0, entry & PTE_W != 0, entry & PTE_X != 0).cached(cache)
}

struct TableWalk<'i> {
    root: *const u64,
    top: usize,
    inventory: &'i Inventory<'i>,
}

impl PageWalk for TableWalk<'_> {
    fn leaf(&self, address: u64) -> Option<Leaf> {
        let va = usize::try_from(address).ok()?;
        let mut table = self.root;
        let mut level = self.top;
        loop {
            // SAFETY: `table` is a readable 512-entry root or identity-mapped child,
            // and the index is masked to nine bits.
            let entry = unsafe { *table.add(index(va, level)) };
            if entry & PTE_V == 0 {
                return None;
            }
            if entry & PTE_RWX != 0 {
                let span = 1u64 << (12 + 9 * level);
                let start = address & !(span - 1);
                return Some(Leaf::new(start, span, protection(entry, self.inventory)));
            }
            if level == 0 {
                return None;
            }
            table = ((entry >> 10) << 12) as *const u64;
            level -= 1;
        }
    }
}

/// Somewhere page-table frames come from.
trait Frames {
    fn take(&mut self) -> Option<u64>;
}

impl Frames for FrameAllocator<'_> {
    fn take(&mut self) -> Option<u64> {
        self.allocate().map(PhysicalFrame::start)
    }
}

impl<const N: usize> Frames for FramePool<N> {
    fn take(&mut self) -> Option<u64> {
        self.allocate().map(PhysicalFrame::start)
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Granularity {
    Small,
    LargeOk,
}

fn map_range(
    root: *mut u64,
    top: usize,
    frames: &mut dyn Frames,
    start: usize,
    end: usize,
    rights: u64,
    granularity: Granularity,
) -> Result<(), PlatformError> {
    let mut flags = rights | PTE_A;
    if rights & PTE_W != 0 {
        flags |= PTE_D;
    }
    let mut va = align_down(start, PAGE_4K);
    let end = align_up(end, PAGE_4K).ok_or(PlatformError::Mapping(MappingError::InvalidAddress))?;
    while va < end {
        let level = (granularity == Granularity::LargeOk
            && va % PAGE_2M == 0
            && end - va >= PAGE_2M) as usize;
        map_leaf(root, top, frames, va, va as u64, flags, level)?;
        va += if level == 1 { PAGE_2M } else { PAGE_4K };
    }
    Ok(())
}

fn map_leaf(
    root: *mut u64,
    top: usize,
    frames: &mut dyn Frames,
    va: usize,
    pa: u64,
    flags: u64,
    level: usize,
) -> Result<(), PlatformError> {
    let mut table = root;
    for above in ((level + 1)..=top).rev() {
        // SAFETY: `table` points to a valid 512-entry table frame.
        let entry = unsafe { &mut *table.add(index(va, above)) };
        if *entry & PTE_V == 0 {
            let next = alloc_table(frames)?;
            *entry = pte(next as u64, 0);
        } else if *entry & PTE_RWX != 0 {
            // Ranges are mapped once; splitting a leaf is unsupported.
            return Err(PlatformError::Mapping(MappingError::Backend));
        }
        table = ((*entry >> 10) << 12) as *mut u64;
    }
    // SAFETY: `table` is the level-`level` table covering `va`.
    unsafe {
        table.add(index(va, level)).write(pte(pa, flags));
    }
    Ok(())
}

fn index(va: usize, level: usize) -> usize {
    (va >> (12 + 9 * level)) & 0x1ff
}

fn align_down(value: usize, alignment: usize) -> usize {
    molt_arch::align_down(value as u64, alignment as u64) as usize
}

fn align_up(value: usize, alignment: usize) -> Option<usize> {
    molt_arch::align_up(value as u64, alignment as u64).map(|value| value as usize)
}

fn pte(pa: u64, flags: u64) -> u64 {
    ((pa >> 12) << 10) | flags | PTE_V
}

/// Allocates one frame and returns its physical base address.
fn alloc_frame(frames: &mut dyn Frames) -> Result<u64, PlatformError> {
    frames.take().ok_or(PlatformError::Mapping(MappingError::OutOfFrames))
}

/// Allocates and zeroes one frame for use as a page table.
fn alloc_table(frames: &mut dyn Frames) -> Result<*mut u64, PlatformError> {
    let frame = alloc_frame(frames)?;
    let table = frame as *mut u64;
    // SAFETY: every frame the allocator hands out is identity mapped — before
    // `init` because translation is off, after it because the writable span
    // covers all free RAM — and holds 512 aligned doublewords.
    unsafe {
        for index in 0..512 {
            table.add(index).write(0);
        }
    }
    Ok(table)
}
