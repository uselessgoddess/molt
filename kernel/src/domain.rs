//! The one address space, proven on the machine rather than on the host.
//!
//! `molt-arch` tests the allocator, the leaf counts and the shootdown protocol
//! as arithmetic. What only a booted kernel can show is that the width they were
//! cut from is this machine's own answer, that other cores really run the flush
//! instruction, and that a second view of the address space does not contain the
//! kernel. Tier 2 of `docs/address-space.md`, marker by marker.

use alloc::boxed::Box;
#[cfg(molt_user_image)]
use alloc::vec::Vec;

use molt_arch::asid::{Asids, Flush};
use molt_arch::memory::{Rights, Span};
use molt_arch::refcount::{self, Leaves, Run};
use molt_arch::va::{Class, Extent, Region};
use molt_arch::{BootInfo, FRAME_SIZE, Platform, PlatformError, SerialWriter, View, view};
#[cfg(molt_user_image)]
use molt_arch::{DomainExit, DomainState, SerialPort};
use molt_kernel::report;
use molt_rt::Executor;

use crate::config::CONFIG;
use crate::space;

#[cfg(molt_user_image)]
static USER_IMAGE: &[u8] = include_bytes!(env!("MOLT_USER_IMAGE"));
#[cfg(molt_user_image)]
static SHELL_IMAGE: &[u8] = include_bytes!(env!("MOLT_SHELL_IMAGE"));
#[cfg(molt_user_image)]
static DOMAIN_DISK_IMAGE: &[u8] = include_bytes!(env!("MOLT_DOMAIN_DISK_IMAGE"));

#[cfg(all(molt_user_image, target_arch = "x86_64"))]
const USER_ARCHITECTURE: molt_domain::Architecture = molt_domain::Architecture::X86_64;
#[cfg(all(molt_user_image, target_arch = "riscv64"))]
const USER_ARCHITECTURE: molt_domain::Architecture = molt_domain::Architecture::RiscV;

#[cfg(molt_user_image)]
const USER_RING: usize = 4;
#[cfg(molt_user_image)]
const USER_STACK: u64 = 64 * 1024;
#[cfg(molt_user_image)]
const USER_HEAP: u64 = 64 * 1024;
#[cfg(all(molt_user_image, target_arch = "x86_64"))]
const SHELL_BASE: u64 = 0x0000_6000_0010_0000;
#[cfg(all(molt_user_image, target_arch = "riscv64"))]
const SHELL_BASE: u64 = 0x00c0_0000_0010_0000;
#[cfg(molt_user_image)]
const SHELL_FILE_RESULT: u64 = 1 << 62;
#[cfg(molt_user_image)]
const SHELL_DIR_RESULT: u64 = 1 << 61;

/// What the tier-2 example in `docs/address-space.md` asks for: a log analyzer
/// that wants a hundred gigabytes of logs addressable at once.
const ANALYZER: u64 = 100 << 30;

/// The class a grant is proven at: megabytes, because this extent is backed for
/// real and QEMU is given two gigabytes, so nothing is behind a gigabyte leaf.
const GRANTED: Class = Class::Mega;

/// Proves an extent out of the machine's one address space survives the round
/// trip a revoke is, and reports the tag budget beside it.
pub fn addresses<P: Platform>(platform: &mut P) -> Extent {
    let widths = platform.address_space().expect("the platform probed its own translation");
    let mut space = space::global();

    // An Sv39-only hart has a 32 GiB gigabyte arena and cannot seat the
    // analyzer, so it runs the same round trip over what it does have.
    let wanted = ANALYZER.min(space.largest(Class::Giga));
    let extent = space.allocate(Class::Giga, wanted).expect("room in the gigabyte arena");
    let start = extent.start();
    let leaves = extent.leaves();
    assert_eq!(start % Class::Giga.granule(), 0, "a gigabyte extent came back unaligned");
    assert_eq!(leaves, wanted.div_ceil(Class::Giga.granule()), "the extent needs other leaves");

    // Giving it back does not give the addresses back: until every hart has
    // flushed, one may still translate through the revoked mapping.
    space.release(extent).expect("an extent this space issued");
    let during = space.allocate(Class::Giga, wanted).expect("room beside the quarantined range");
    assert_ne!(during.start(), start, "an unflushed range was handed out again");
    space.release(during).expect("an extent this space issued");

    let epoch = space.sweep();
    space.retire(epoch);
    let again = space.allocate(Class::Giga, wanted).expect("the flushed range, back in service");
    assert_eq!(again.start(), start, "a flushed range did not come back");
    assert_eq!(space.quarantined(Class::Giga), 0, "a retired epoch left bytes in quarantine");

    report!(
        platform,
        "MOLT_VA_OK: {} address bits, {} GiB at {start:#x} in {} leaves",
        widths.address(),
        wanted >> 30,
        leaves,
    );

    // The tag budget is the domain budget: a grant or a revoke costs the
    // shootdown above and no tag at all.
    let mut asids = Asids::new(widths.asid());
    let grant = asids.assign();
    assert!(asids.live(grant.asid()), "a fresh tag was born stale");
    assert_eq!(
        grant.flush() == Flush::Everything,
        asids.capacity() == 0,
        "a hart with tags to give still flushed, or one without tags did not"
    );

    report!(platform, "MOLT_ASID_OK: bits={} domains={}", widths.asid(), asids.capacity());

    again
}

/// Counts the leaves of that same extent the way a grant and a revoke would.
///
/// The keying, not the arithmetic. What a booted kernel shows is the *size* of
/// what is counted: a hundred gigabytes shared with a second view is one record
/// holding the number two, and the 26 million frames underneath never get one.
pub fn counts<P: Platform>(platform: &mut P, analyzer: Extent) {
    let mut runs = [Run::EMPTY; CONFIG.runs];
    let mut leaves = Leaves::over(&mut runs);
    let start = analyzer.start();

    leaves.map(start, analyzer.class(), analyzer.leaves()).expect("leaves nobody counts yet");
    let mapped = leaves.leaves();
    let frames = leaves.frames();
    assert_eq!(leaves.runs(), 1, "leaves mapped together were counted apart");

    // Every leaf gains the same holder, so the accounting stays in one record.
    leaves.share(analyzer.region()).expect("every leaf of a mapped extent");
    let shared = leaves.runs();
    assert_eq!(shared, 1, "a grant of everything fragmented the accounting");
    assert_eq!(leaves.count(start), Some(2), "the second view was not counted");
    assert_eq!(leaves.count(analyzer.end() - 1), Some(2), "the grant stopped short of the end");

    // Revoking part of a gigabyte leaf is a question the tables cannot answer
    // either, until it becomes the 512 leaves below it.
    let part = Region::new(start, start + 2 * Class::Mega.granule()).expect("two megabytes");
    assert_eq!(leaves.share(part), Err(refcount::Error::Straddle), "half a leaf was counted");
    assert_eq!(leaves.split(start), Ok(Class::Mega), "a gigabyte leaf did not split");
    assert_eq!(leaves.leaves(), mapped - 1 + Class::FANOUT, "the split lost addresses");

    let reclaimed = leaves.release(part).expect("leaves the second view holds");
    assert!(reclaimed.is_empty(), "a leaf the first view still holds was reported free");
    assert_eq!(leaves.count(start), Some(1), "the revoke did not reach the second view");
    assert_eq!(leaves.count(start + part.bytes()), Some(2), "the revoke reached past its range");

    report!(
        platform,
        "MOLT_REFCOUNT_OK: {} GiB in {mapped} leaves and {shared} record, {frames} frames \
         uncounted; revoking 2 MiB split one leaf into {} and left {} records",
        analyzer.bytes() >> 30,
        Class::FANOUT,
        leaves.runs(),
    );

    // The other cores have not started yet, so nothing can be holding a
    // translation and the flush this waits on is one nobody owes.
    let mut space = space::global();
    space.release(analyzer).expect("an extent this space issued");
    let epoch = space.sweep();
    space.retire(epoch);
}

/// Frees an extent the way a revoke does, over cores that flush for real.
///
/// The one property the host tests cannot show: the epoch is retired by
/// acknowledgements from the cores themselves, each having run the flush
/// instruction on its own hardware. [`space::recycle`] holds the order.
pub fn shootdown<P: Platform>(platform: &mut P, exec: &Executor) {
    let mut space = space::global();

    let wanted = ANALYZER.min(space.largest(Class::Giga));
    let extent = space.allocate(Class::Giga, wanted).expect("room in the gigabyte arena");
    let start = extent.start();

    let (retired, cores) = space::recycle(exec, &mut space, extent);

    assert_eq!(space.quarantined(Class::Giga), 0, "a flushed range stayed in quarantine");
    // Held rather than released: what goes back into the open batch is swept by
    // the next `recycle`, and every smoke after this one wants its own batch.
    let again = space.allocate(Class::Giga, wanted).expect("the flushed range, back in service");
    assert_eq!(again.start(), start, "a flushed range did not come back");

    report!(
        platform,
        "MOLT_SHOOTDOWN_OK: {} GiB at {start:#x} held over {cores} cores until epoch {} flushed",
        wanted >> 30,
        retired.get(),
    );
}

/// Opens a second view, moves an extent into it, and takes it back.
///
/// Not that a mapping can be made, which the kernel's own tables show at boot,
/// but that a *second* view exists which does not contain the kernel, that an
/// extent enters it without a byte moving, and that it leaves in the order a
/// revoke has to go in. The frames are real RAM, reached from the view at the
/// same global address the kernel calls them by.
pub fn smoke<P: Platform>(boot_info: &BootInfo<'_>, platform: &mut P, exec: &Executor) {
    let widths = platform.address_space().expect("the platform probed its own translation");
    let grant = Asids::new(widths.asid()).assign();
    let opened = platform.open_view(grant.asid()).expect("a root for a second view");

    // What a fresh view holds, which is nothing: not the code running this, not
    // the stack it runs on, not the heap it allocates from.
    let here = smoke::<P> as *const () as u64;
    let stack = (&raw const widths) as u64;
    let heap = Box::into_raw(Box::new(0u64));
    for (address, what) in
        [(here, "kernel text"), (stack, "the kernel stack"), (heap as u64, "the kernel heap")]
    {
        assert!(platform.resident(opened, address).is_none(), "a fresh view could reach {what}");
    }
    // SAFETY: the box was leaked one statement ago and nothing else holds it.
    drop(unsafe { Box::from_raw(heap) });

    report!(
        platform,
        "MOLT_DOMAIN_OK: view {} tagged {} in generation {}",
        opened.index(),
        grant.asid().value(),
        grant.asid().generation(),
    );
    report!(platform, "MOLT_DOMAIN_ABSENT_OK: kernel text, stack, and heap unreachable from it");

    // Twice the leaf is claimed and the leaf cut out of the aligned part: a
    // firmware map starts a region wherever it likes, and a megabyte leaf has to
    // begin on a megabyte.
    let granule = GRANTED.granule();
    let claimed =
        platform.claim_ram(boot_info, 2 * granule / FRAME_SIZE).expect("RAM to back a grant");
    let base =
        molt_arch::align_up(claimed.start(), granule).expect("an aligned base below the end");
    let span = Span::new(base, base + granule).expect("a leaf's worth of claimed frames");

    let mut space = space::global();
    let extent = space.allocate(GRANTED, granule).expect("room in the megabyte arena");
    let start = extent.start();

    platform.grant(opened, &extent, span, Rights::READ_WRITE).expect("a leaf the view lacked");
    let leaf = platform.resident(opened, start).expect("the granted address, in the view's tables");
    assert_eq!(leaf.start(), start, "the grant landed somewhere else");
    assert_eq!(leaf.size(), granule, "the grant was cut smaller than the extent asked for");
    assert!(leaf.protection().is_write(), "a read-write grant arrived read-only");
    assert!(!leaf.protection().is_execute(), "a data grant arrived executable");
    assert!(platform.resident(opened, extent.end() - 1).is_some(), "the grant stopped short");
    assert!(platform.resident(opened, extent.end()).is_none(), "the grant ran past its extent");
    assert!(platform.resident(opened, here).is_none(), "a grant of RAM brought the kernel along");

    report!(
        platform,
        "MOLT_GRANT_OK: {} MiB at {start:#x} from frames at {base:#x}, in {} leaf",
        extent.bytes() >> 20,
        extent.leaves(),
    );

    split(platform, opened, &extent, leaf.base());

    // Step one of the revoke clears the leaves and nothing else: a core that
    // walked them a moment ago may still hold the translation.
    let cleared = platform.revoke(opened, &extent).expect("leaves this view holds");
    assert_eq!(cleared, extent.leaves(), "a revoke took a different number of leaves");
    assert!(platform.resident(opened, start).is_none(), "a revoked address still translated");
    let second = platform.revoke(opened, &extent);
    assert!(refused(second, view::Error::Absent), "a second revoke found something left to take");

    // Steps two and three: every core drops what it cached and says so itself,
    // and only then are the addresses anybody's again.
    let (retired, cores) = space::recycle(exec, &mut space, extent);
    let again = space.allocate(GRANTED, granule).expect("the flushed range, back in service");
    assert_eq!(again.start(), start, "a flushed range did not come back");

    report!(
        platform,
        "MOLT_REVOKE_OK: {cleared} leaf out of view {}, held over {cores} cores until epoch {} \
         flushed",
        opened.index(),
        retired.get(),
    );
}

/// Admits a normal static binary, runs it in user mode, and contains a second
/// instance's deliberate fault. The only shared memory is the hostile ABI
/// channel; every other byte is private to the view.
#[cfg(molt_user_image)]
pub fn user_smoke<P: Platform>(boot_info: &BootInfo<'_>, platform: &mut P) {
    rejected_image_never_maps();
    report!(platform, "MOLT_DOMAIN_WX_OK: rejected image mapped zero executable pages");

    let widths = platform.address_space().expect("the platform probed its own translation");
    let mut asids = Asids::new(widths.asid());
    let view = platform.open_view(asids.assign().asid()).expect("a root for the user image");
    let mut mappings = Vec::new();
    let entry = {
        let mut mapper = ImageMapper { boot_info, platform, view, mappings: &mut mappings };
        molt_domain::load(USER_IMAGE, USER_ARCHITECTURE, &mut mapper)
            .expect("the build-produced user ELF passes admission")
    };
    let base = mappings.first().expect("one admitted load segment").extent.start();

    let (channel_address, channel_pointer) = allocate_mapping(
        boot_info,
        platform,
        view,
        &mut mappings,
        FRAME_SIZE,
        Rights::READ_WRITE,
        false,
    );
    let (stack_address, _) = allocate_mapping(
        boot_info,
        platform,
        view,
        &mut mappings,
        USER_STACK,
        Rights::READ_WRITE,
        true,
    );
    let (heap_address, _) = allocate_mapping(
        boot_info,
        platform,
        view,
        &mut mappings,
        USER_HEAP,
        Rights::READ_WRITE,
        true,
    );
    let aperture = mappings.last().expect("the heap mapping").extent.end() - base;
    assert!(aperture <= molt_abi::wire::APERTURE, "the domain escaped its four-GiB ABI aperture");

    // SAFETY: the page is freshly claimed, zeroed, aligned, and exclusively
    // owned by this domain bootstrap until the kernel and user ring ends borrow it.
    unsafe {
        channel_pointer.cast::<molt_abi::Channel<USER_RING>>().write(molt_abi::Channel::new())
    };
    // SAFETY: initialized in the preceding statement and retained by `mappings`.
    let channel = unsafe { &*channel_pointer.cast::<molt_abi::Channel<USER_RING>>() };
    let (mut submissions, mut completions) = channel.kernel();
    let stack = stack_address + USER_STACK;
    let arguments = [channel_address, base, 1, heap_address, USER_HEAP, 0];
    let mut state = DomainState::new(view, entry, stack, arguments);

    loop {
        match platform.enter_domain(&mut state).expect("domain entry and return") {
            DomainExit::Ring => {
                if !drive_user_ring(
                    platform,
                    &mappings,
                    base,
                    aperture,
                    &mut submissions,
                    &mut completions,
                ) {
                    report!(platform, "MOLT_USER_RING_FAULT: stopped hostile domain ring");
                    return;
                }
            }
            DomainExit::Exited(0) => break,
            DomainExit::Exited(status) => panic!("hello domain exited with {status}"),
            DomainExit::Fault { cause, address } => {
                panic!("hello domain faulted: cause={cause:#x} address={address:#x}")
            }
        }
    }
    report!(platform, "MOLT_USER_HELLO_OK: ring write from user mode");
    report!(platform, "MOLT_DOMAIN_EXIT_OK: status=0");

    let mut fault = DomainState::new(
        view,
        entry,
        stack,
        [channel_address, base, u64::MAX, heap_address, USER_HEAP, 0],
    );
    match platform.enter_domain(&mut fault).expect("faulting domain entry and return") {
        DomainExit::Fault { cause, address } => {
            report!(platform, "MOLT_DOMAIN_FAULT_OK: cause={cause:#x} address={address:#x}");
        }
        DomainExit::Ring => panic!("fault probe reached its ring"),
        DomainExit::Exited(status) => panic!("fault probe exited with {status}"),
    }

    let current = mappings.last().expect("the hello heap mapping").extent.end();
    assert!(current < SHELL_BASE, "hello image overlapped the shell image slot");
    let padding = space::global()
        .allocate(Class::Page, SHELL_BASE - current)
        .expect("the reserved shell slot");
    assert_eq!(padding.end(), SHELL_BASE, "the shell slot did not end at its link address");

    let device = molt_block::Loopback::read(DOMAIN_DISK_IMAGE).expect("aligned built-in disk");
    let mut filesystem = molt_fs::Fs::<_, 4>::mount(molt_block::Serial::new(device))
        .expect("the build-produced MoltFS image mounts");
    let root = filesystem.root(molt_core::CellId::new(3)).expect("a shell root capability");
    shell_smoke(boot_info, platform, asids.assign().asid(), &mut filesystem, root);
}

#[cfg(molt_user_image)]
fn shell_smoke<P: Platform, Q: molt_block::Queue>(
    boot_info: &BootInfo<'_>,
    platform: &mut P,
    asid: molt_arch::asid::Asid,
    filesystem: &mut molt_fs::Fs<Q, 4>,
    root: molt_core::capability::Capability<molt_fs::Dir>,
) {
    let view = platform.open_view(asid).expect("a root for the shell image");
    let mut mappings = Vec::new();
    let entry = {
        let mut mapper = ImageMapper { boot_info, platform, view, mappings: &mut mappings };
        molt_domain::load(SHELL_IMAGE, USER_ARCHITECTURE, &mut mapper)
            .expect("the build-produced shell ELF passes admission")
    };
    let base = mappings.first().expect("one admitted shell segment").extent.start();
    assert_eq!(base, SHELL_BASE, "the shell was not linked into its reserved slot");
    let (channel_address, channel_pointer) = allocate_mapping(
        boot_info,
        platform,
        view,
        &mut mappings,
        FRAME_SIZE,
        Rights::READ_WRITE,
        false,
    );
    let (stack_address, _) = allocate_mapping(
        boot_info,
        platform,
        view,
        &mut mappings,
        USER_STACK,
        Rights::READ_WRITE,
        true,
    );
    let (heap_address, _) = allocate_mapping(
        boot_info,
        platform,
        view,
        &mut mappings,
        USER_HEAP,
        Rights::READ_WRITE,
        true,
    );
    let aperture = mappings.last().expect("the shell heap mapping").extent.end() - base;
    assert!(aperture <= molt_abi::wire::APERTURE, "the shell escaped its ABI aperture");

    // SAFETY: this is a fresh, aligned, exclusively owned shared-ring page.
    unsafe {
        channel_pointer.cast::<molt_abi::Channel<USER_RING>>().write(molt_abi::Channel::new())
    };
    // SAFETY: the channel was initialized immediately above and its mapping is retained.
    let channel = unsafe { &*channel_pointer.cast::<molt_abi::Channel<USER_RING>>() };
    let (mut submissions, mut completions) = channel.kernel();
    let mut handles = ShellHandles::new(root.raw());
    let mut state = DomainState::new(
        view,
        entry,
        stack_address + USER_STACK,
        [channel_address, base, 1, heap_address, USER_HEAP, root.raw()],
    );
    loop {
        match platform.enter_domain(&mut state).expect("shell domain entry and return") {
            DomainExit::Ring => {
                if !drive_shell_ring(
                    platform,
                    filesystem,
                    &mut handles,
                    &mappings,
                    base,
                    aperture,
                    &mut submissions,
                    &mut completions,
                ) {
                    report!(platform, "MOLT_SHELL_RING_FAULT: stopped hostile domain ring");
                    return;
                }
            }
            DomainExit::Exited(0) => break,
            DomainExit::Exited(status) => panic!("shell domain exited with {status}"),
            DomainExit::Fault { cause, address } => {
                panic!("shell domain faulted: cause={cause:#x} address={address:#x}")
            }
        }
    }
    report!(platform, "MOLT_SHELL_DOMAIN_OK: unmodified shell completed through FsOp ring");
}

#[cfg(molt_user_image)]
#[derive(Debug)]
enum ImageMapError {
    Address,
    Platform,
}

#[cfg(molt_user_image)]
impl From<PlatformError> for ImageMapError {
    fn from(_error: PlatformError) -> Self {
        Self::Platform
    }
}

#[cfg(molt_user_image)]
struct Mapping {
    extent: Extent,
    _physical: Span,
    pointer: *mut u8,
    rights: Rights,
    payload: bool,
}

#[cfg(molt_user_image)]
struct ShellHandles {
    directories: [Option<u64>; 4],
    files: [Option<u64>; 4],
}

#[cfg(molt_user_image)]
impl ShellHandles {
    const fn new(root: u64) -> Self {
        Self { directories: [Some(root), None, None, None], files: [None; 4] }
    }

    fn directory(&self, raw: u64) -> bool {
        self.directories.iter().flatten().any(|&allowed| allowed == raw)
    }

    fn file(&self, raw: u64) -> bool {
        self.files.iter().flatten().any(|&allowed| allowed == raw)
    }

    fn remember(&mut self, raw: u64, directory: bool) -> bool {
        let handles = if directory { &mut self.directories } else { &mut self.files };
        let Some(slot) = handles.iter_mut().find(|slot| slot.is_none()) else {
            return false;
        };
        *slot = Some(raw);
        true
    }

    fn forget_file(&mut self, raw: u64) {
        if let Some(slot) = self.files.iter_mut().find(|slot| **slot == Some(raw)) {
            *slot = None;
        }
    }
}

#[cfg(molt_user_image)]
struct ImageMapper<'a, 'boot, P> {
    boot_info: &'a BootInfo<'boot>,
    platform: &'a mut P,
    view: View,
    mappings: &'a mut Vec<Mapping>,
}

#[cfg(molt_user_image)]
impl<P: Platform> molt_domain::Mapper for ImageMapper<'_, '_, P> {
    type Error = ImageMapError;

    fn map(&mut self, segment: molt_domain::Segment, file: &[u8]) -> Result<(), Self::Error> {
        let bytes = segment.mapped_size();
        let extent =
            space::global().allocate(Class::Page, bytes).map_err(|_| ImageMapError::Address)?;
        if extent.start() != segment.virtual_address() {
            return Err(ImageMapError::Address);
        }
        let physical = self.platform.claim_ram(self.boot_info, bytes / FRAME_SIZE)?;
        let pointer = self.platform.claimed_pointer(physical)?;
        // SAFETY: `claim_ram` exclusively transferred `bytes` initialized RAM
        // to the loader, and `file` was bounds-checked before mapping began.
        unsafe {
            pointer.write_bytes(0, bytes as usize);
            pointer.copy_from_nonoverlapping(file.as_ptr(), file.len());
        }
        let protection = segment.protection();
        let rights =
            Rights::new(protection.is_read(), protection.is_write(), protection.is_execute())
                .map_err(|_| ImageMapError::Address)?;
        self.platform.grant(self.view, &extent, physical, rights)?;
        self.mappings.push(Mapping { extent, _physical: physical, pointer, rights, payload: true });
        Ok(())
    }
}

#[cfg(molt_user_image)]
fn allocate_mapping<P: Platform>(
    boot_info: &BootInfo<'_>,
    platform: &mut P,
    view: View,
    mappings: &mut Vec<Mapping>,
    bytes: u64,
    rights: Rights,
    payload: bool,
) -> (u64, *mut u8) {
    let extent = space::global().allocate(Class::Page, bytes).expect("room in the page arena");
    let address = extent.start();
    let physical = platform
        .claim_ram(boot_info, extent.bytes() / FRAME_SIZE)
        .expect("RAM to back a domain mapping");
    let pointer = platform.claimed_pointer(physical).expect("claimed RAM in the direct map");
    // SAFETY: the freshly claimed span contains `extent.bytes()` writable bytes.
    unsafe { pointer.write_bytes(0, extent.bytes() as usize) };
    platform.grant(view, &extent, physical, rights).expect("an absent domain range");
    mappings.push(Mapping { extent, _physical: physical, pointer, rights, payload });
    (address, pointer)
}

#[cfg(molt_user_image)]
fn drive_user_ring<P: Platform>(
    platform: &mut P,
    mappings: &[Mapping],
    base: u64,
    aperture: u64,
    submissions: &mut molt_abi::Submissions<'_, USER_RING>,
    completions: &mut molt_abi::Completions<'_, USER_RING>,
) -> bool {
    loop {
        let reply = match submissions.take() {
            Err(_) => return false,
            Ok(molt_abi::Next::Empty) => return true,
            Ok(molt_abi::Next::Rejected { id, reject }) => molt_abi::Reply::rejected(id, reject),
            Ok(molt_abi::Next::Ready(call)) => {
                let result = match call.op() {
                    molt_abi::Op::Timer { .. } => 0,
                    molt_abi::Op::Write { cap, offset: 0, buf } if cap.get() == 1 => {
                        match resolve_region(mappings, base, aperture, buf) {
                            Some(bytes) => {
                                platform.serial().write_bytes(bytes);
                                bytes.len() as i64
                            }
                            None => -1,
                        }
                    }
                    _ => -1,
                };
                molt_abi::Reply::new(call.id(), result)
            }
        };
        if completions.publish(reply).is_err() {
            return false;
        }
    }
}

#[cfg(molt_user_image)]
fn drive_shell_ring<P: Platform, Q: molt_block::Queue>(
    platform: &mut P,
    filesystem: &mut molt_fs::Fs<Q, 4>,
    handles: &mut ShellHandles,
    mappings: &[Mapping],
    base: u64,
    aperture: u64,
    submissions: &mut molt_abi::Submissions<'_, USER_RING>,
    completions: &mut molt_abi::Completions<'_, USER_RING>,
) -> bool {
    loop {
        let reply = match submissions.take() {
            Err(_) => return false,
            Ok(molt_abi::Next::Empty) => return true,
            Ok(molt_abi::Next::Rejected { id, reject }) => molt_abi::Reply::rejected(id, reject),
            Ok(molt_abi::Next::Ready(call)) => {
                let result = match call.op() {
                    molt_abi::Op::Write { cap, offset: 0, buf } if cap.get() == 1 => {
                        match resolve_region(mappings, base, aperture, buf) {
                            Some(bytes) => {
                                platform.serial().write_bytes(bytes);
                                bytes.len() as i64
                            }
                            None => -1,
                        }
                    }
                    molt_abi::Op::Open { dir, name } => {
                        shell_open(filesystem, handles, mappings, base, aperture, dir, name)
                    }
                    molt_abi::Op::Read { cap, offset, buf } => {
                        shell_read(filesystem, handles, mappings, base, aperture, cap, offset, buf)
                    }
                    molt_abi::Op::Close { cap } => shell_close(filesystem, handles, cap),
                    _ => -1,
                };
                molt_abi::Reply::new(call.id(), result)
            }
        };
        if completions.publish(reply).is_err() {
            return false;
        }
    }
}

#[cfg(molt_user_image)]
fn shell_open<Q: molt_block::Queue>(
    filesystem: &mut molt_fs::Fs<Q, 4>,
    handles: &mut ShellHandles,
    mappings: &[Mapping],
    base: u64,
    aperture: u64,
    directory: molt_abi::Handle,
    name: molt_abi::Region,
) -> i64 {
    if !handles.directory(directory.get()) {
        return -1;
    }
    let Some(bytes) = resolve_region(mappings, base, aperture, name) else {
        return -1;
    };
    let Ok(name) = molt_fs::Name::new(bytes) else {
        return -1;
    };
    // SAFETY: this is only a typed transport name. `Fs::apply` validates its
    // index, generation, and rights before using it.
    let directory = unsafe { molt_core::capability::Capability::from_raw(directory.get()) };
    let mut buffers = molt_core::buffer::BufferRegistry::<1>::new();
    match filesystem.apply(
        molt_core::CellId::new(3),
        molt_fs::FsOp::Open { dir: directory, name },
        &mut buffers,
    ) {
        Ok(molt_fs::FsDone::Opened(molt_fs::Handle::File(file)))
            if handles.remember(file.raw(), false) =>
        {
            (file.raw() | SHELL_FILE_RESULT) as i64
        }
        Ok(molt_fs::FsDone::Opened(molt_fs::Handle::Dir(dir)))
            if handles.remember(dir.raw(), true) =>
        {
            (dir.raw() | SHELL_DIR_RESULT) as i64
        }
        _ => -1,
    }
}

#[cfg(molt_user_image)]
fn shell_read<Q: molt_block::Queue>(
    filesystem: &mut molt_fs::Fs<Q, 4>,
    handles: &ShellHandles,
    mappings: &[Mapping],
    base: u64,
    aperture: u64,
    file: molt_abi::Handle,
    offset: u64,
    buffer: molt_abi::Region,
) -> i64 {
    if !handles.file(file.get()) {
        return -1;
    }
    let Some(bytes) = resolve_region_mut(mappings, base, aperture, buffer) else {
        return -1;
    };
    let mut buffers = molt_core::buffer::BufferRegistry::<1>::new();
    let Ok(registered) = buffers.register_write(molt_core::CellId::new(3), bytes) else {
        return -1;
    };
    // SAFETY: the filesystem capability table validates this transported name.
    let file = unsafe { molt_core::capability::Capability::from_raw(file.get()) };
    let operation = molt_core::buffer::BufferOperation::new(registered, 0, buffer.len() as usize);
    match filesystem.apply(
        molt_core::CellId::new(3),
        molt_fs::FsOp::Read { file, buffer: operation, offset },
        &mut buffers,
    ) {
        Ok(molt_fs::FsDone::Read(read)) => read as i64,
        _ => -1,
    }
}

#[cfg(molt_user_image)]
fn shell_close<Q: molt_block::Queue>(
    filesystem: &mut molt_fs::Fs<Q, 4>,
    handles: &mut ShellHandles,
    handle: molt_abi::Handle,
) -> i64 {
    if !handles.file(handle.get()) {
        return -1;
    }
    // SAFETY: the shell only opens `hello.txt` in this smoke, so this transported
    // handle is a file. The table, not this type restoration, validates it.
    let file = unsafe { molt_core::capability::Capability::from_raw(handle.get()) };
    let mut buffers = molt_core::buffer::BufferRegistry::<1>::new();
    match filesystem.apply(
        molt_core::CellId::new(3),
        molt_fs::FsOp::Close(molt_fs::Handle::File(file)),
        &mut buffers,
    ) {
        Ok(molt_fs::FsDone::Closed) => {
            handles.forget_file(handle.get());
            0
        }
        _ => -1,
    }
}

#[cfg(molt_user_image)]
fn resolve_region<'a>(
    mappings: &'a [Mapping],
    base: u64,
    aperture: u64,
    region: molt_abi::Region,
) -> Option<&'a [u8]> {
    if !region.fits(aperture) {
        return None;
    }
    let region = region.within(aperture);
    let start = base + u64::from(region.offset());
    let end = start + u64::from(region.len());
    let mapping = mappings.iter().find(|mapping| {
        mapping.payload
            && mapping.rights.is_read()
            && mapping.extent.start() <= start
            && end <= mapping.extent.end()
    })?;
    let offset = usize::try_from(start - mapping.extent.start()).ok()?;
    // SAFETY: the masked range lies wholly within the still-owned physical span.
    Some(unsafe { core::slice::from_raw_parts(mapping.pointer.add(offset), region.len() as usize) })
}

#[cfg(molt_user_image)]
fn resolve_region_mut<'a>(
    mappings: &'a [Mapping],
    base: u64,
    aperture: u64,
    region: molt_abi::Region,
) -> Option<&'a mut [u8]> {
    if !region.fits(aperture) {
        return None;
    }
    let region = region.within(aperture);
    let start = base + u64::from(region.offset());
    let end = start + u64::from(region.len());
    let mapping = mappings.iter().find(|mapping| {
        mapping.payload
            && mapping.rights.is_write()
            && mapping.extent.start() <= start
            && end <= mapping.extent.end()
    })?;
    let offset = usize::try_from(start - mapping.extent.start()).ok()?;
    // SAFETY: the domain is stopped, this masked range is wholly inside one
    // owned mapping, and the borrow prevents a second kernel user of the slice.
    Some(unsafe {
        core::slice::from_raw_parts_mut(mapping.pointer.add(offset), region.len() as usize)
    })
}

#[cfg(molt_user_image)]
fn rejected_image_never_maps() {
    struct Counter(u32);
    impl molt_domain::Mapper for Counter {
        type Error = ();

        fn map(&mut self, _segment: molt_domain::Segment, _file: &[u8]) -> Result<(), Self::Error> {
            self.0 += 1;
            Ok(())
        }
    }

    let mut hostile = USER_IMAGE.to_vec();
    let header = hostile.get(32..40).expect("ELF program-header offset");
    let phoff = u64::from_le_bytes(header.try_into().expect("eight-byte ELF field")) as usize;
    let count =
        u16::from_le_bytes(hostile[56..58].try_into().expect("two-byte ELF program-header count"))
            as usize;
    let executable = (0..count)
        .map(|index| phoff + index * 56)
        .find(|&at| hostile[at..at + 4] == 1u32.to_le_bytes() && hostile[at + 4] & 1 != 0)
        .expect("one executable load segment");
    hostile[executable + 4] |= 2;

    let mut counter = Counter(0);
    assert!(
        matches!(
            molt_domain::load(&hostile, USER_ARCHITECTURE, &mut counter),
            Err(molt_domain::LoadError::Image(molt_domain::Error::WriteExecute))
        ),
        "a writable executable image passed admission"
    );
    assert_eq!(counter.0, 0, "a rejected image reached the mapper");
}

/// Whether the platform refused a call because a view could not answer it.
fn refused<T>(outcome: Result<T, PlatformError>, reason: view::Error) -> bool {
    matches!(outcome, Err(PlatformError::View(error)) if error == reason)
}

/// The hardware half of a partial revoke: one leaf becomes 512 naming the same
/// frames, so nothing the holder reads changes — but the kernel can now take one
/// back without the other 511.
fn split<P: Platform>(platform: &mut P, opened: View, extent: &Extent, frames: Option<u64>) {
    let (start, granule) = (extent.start(), GRANTED.granule());
    let child = platform.split_leaf(opened, start).expect("a leaf this view holds");
    assert_eq!(child, GRANTED.smaller().expect("a class below the granted one"));

    let cut = platform.resident(opened, start).expect("the split address, still translated");
    assert_eq!(cut.size(), child.granule(), "the split left the leaf the size it was");
    assert_eq!(cut.base(), frames, "the first child came out over other frames");
    assert!(cut.protection().is_write(), "the split dropped the rights it was cutting");

    let last = extent.end() - child.granule();
    let tail = platform.resident(opened, last).expect("the last child of the split leaf");
    assert_eq!(tail.base(), frames.map(|base| base + granule - child.granule()));
    assert!(platform.resident(opened, extent.end()).is_none(), "the split ran past the leaf");
    let again = platform.split_leaf(opened, start);
    assert!(refused(again, view::Error::Granule), "a smallest leaf was cut smaller still");

    report!(
        platform,
        "MOLT_SPLIT_OK: 1 leaf of {} KiB into {} of {} KiB at {start:#x}",
        granule >> 10,
        granule / child.granule(),
        child.granule() >> 10,
    );
}
