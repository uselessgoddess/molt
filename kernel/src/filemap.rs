//! A file, mapped where it already is.
//!
//! Reading a file into a process costs two copies elsewhere: one from the
//! device into the kernel's page cache, and one from there into the caller,
//! because the caller's address for those bytes is not the kernel's. Molt has
//! one address space, so the second copy has nothing to pay for. The window is
//! read into frames once, given an address out of the same allocator every
//! other extent comes from, and handed to a domain as a leaf — see
//! [`molt_arch::cache`] and `docs/address-space.md`.
//!
//! What this proves, and it is the whole of the claim: the leaf in a domain's
//! tables points at the frames the filesystem read into. Not at a copy of them
//! made for that domain, and not at a copy made for the second domain either —
//! `resident` reads the physical address back out of the live tables, and both
//! views report the same one. A second domain asking for the same window is a
//! cache hit at the same address, so the device is read once no matter how many
//! views end up looking at it.
//!
//! The teardown is the order `docs/threat-model.md` asks after and
//! [`molt_arch::view`] spells out: the leaves leave the views first, the cache
//! gives the extent back rather than dropping it, and the addresses stay
//! nobody's until every core has flushed.

use molt_arch::asid::Asids;
use molt_arch::cache::{self, Window, Windows};
use molt_arch::memory::{Rights, Span};
use molt_arch::refcount::{Leaves, Run};
use molt_arch::shootdown::Shootdown;
use molt_arch::va::{Class, Hole, Space};
use molt_arch::{BootInfo, FRAME_SIZE, Platform, SerialWriter, View};
use molt_block::Queue;
use molt_core::CellId;
use molt_core::buffer::{BufferOperation, BufferRegistry};
use molt_fs::{Fs, FsDone, FsOp, Handle, Name};
use molt_kernel::report;

use crate::smp;

/// Whose buffer the window is read into: the kernel's page cache, which is not
/// any domain's, which is why no domain had to ask for the read.
const CACHE: CellId = CellId::new(4);
/// The root, the file under it, and room to be wrong about that.
const HANDLES: usize = 4;

/// The class a window is mapped at.
///
/// Megabytes and not gigabytes for the reason the grant smoke gives: QEMU is
/// started with two gigabytes, so there are no frames behind a gigabyte leaf,
/// and a window nothing backs would prove nothing about a mapping. The size the
/// design is for is the class, not the smoke — a gigabyte-class window is the
/// same one entry per view, and `molt-arch` tests it on the host.
const MAPPED: Class = Class::Mega;

const HOLES: usize = 3 * 8;
const RUNS: usize = 4;
/// One window, and a slot to prove the second domain does not take another.
const SLOTS: usize = 2;

/// The file the smoke maps, and what the image builder put in it.
const MAPPING: &str = "hello.txt";
const CONTENT: &[u8] = b"hello, molt\n";

/// The window's key.
///
/// One file and one offset, so the number is a constant. Which number a file
/// gets is the filesystem's to say, and nothing on the ring hands out an object
/// id yet; what the cache needs is only that two requests for the same file
/// agree, and here they do because there is one.
const WINDOW: cache::File = cache::File::new(1);

/// Reads a file into frames once and maps it into two domains.
pub fn smoke<P: Platform, Q: Queue>(boot_info: &BootInfo<'_>, platform: &mut P, queue: Q) {
    let (Some(widths), Some(offset)) = (platform.address_space(), boot_info.physical_offset())
    else {
        report!(platform, "MOLT_FILE_MAP_SKIPPED: this platform opens no second view");
        return;
    };

    // The frames the window lives in, claimed out of the same RAM the kernel
    // maps. Twice the leaf, because a firmware map starts a usable region
    // wherever it likes and a megabyte leaf has to begin on a megabyte.
    let granule = MAPPED.granule();
    let Ok(claimed) = platform.claim_ram(boot_info, 2 * granule / FRAME_SIZE) else {
        report!(platform, "MOLT_FILE_MAP_SKIPPED: no RAM left to cache a window in");
        return;
    };
    let base =
        molt_arch::align_up(claimed.start(), granule).expect("an aligned base below the end");
    let span = Span::new(base, base + granule).expect("a leaf's worth of claimed frames");

    let mut fs = match Fs::<Q, HANDLES>::mount(queue) {
        Ok(mounted) => mounted,
        Err(error) => {
            report!(platform, "MOLT_FILE_MAP_FAILED: {error:?}");
            return;
        }
    };
    let read = fill(&mut fs, offset, span);

    let mut holes = [Hole::EMPTY; HOLES];
    let mut space = Space::over(widths.address(), &mut holes).expect("a space wide enough to cut");
    let mut slots = [const { Window::EMPTY }; SLOTS];
    let mut windows = Windows::over(&mut slots);
    let mut runs = [Run::EMPTY; RUNS];
    let mut leaves = Leaves::over(&mut runs);

    // Nothing is cached, which is the only thing that makes the kernel go to
    // the device at all. The read above was that miss being answered.
    assert_eq!(
        windows.hold(WINDOW, 0).map(|_| ()),
        Err(cache::Error::Unknown),
        "a window nobody had read was already cached"
    );

    let extent = space.allocate(MAPPED, granule).expect("room in the megabyte arena");
    let (start, count) = (extent.start(), extent.leaves());
    let region =
        windows.insert(WINDOW, 0, extent, span).expect("a window nothing else cached").region();
    let region = region.expect("the window just cached");
    leaves.map(region.start(), MAPPED, count).expect("leaves nobody counts yet");

    let mut asids = Asids::new(widths.asid());
    let first = platform.open_view(asids.assign().asid()).expect("a root for a domain");
    let second = platform.open_view(asids.assign().asid()).expect("a root for another domain");

    // The first mapping. Read-only, because a file window is a file window:
    // what it costs is one leaf, and what moves is nothing.
    let mapping = windows.lookup(WINDOW, 0).and_then(Window::extent).expect("the cached window");
    platform.grant(first, mapping, span, Rights::READ).expect("a leaf the view lacked");

    // The second domain asks for the same window and is told the same address.
    // No read, no copy, no second extent: the saving is that this arm of the
    // code does nothing the first arm already did.
    let held = windows.hold(WINDOW, 0).expect("a window the first domain left cached");
    assert_eq!(held.region(), Some(region), "one window of one file got two addresses");
    platform
        .grant(second, held.extent().expect("the cached window"), span, Rights::READ)
        .expect("a leaf the second view lacked");
    leaves.share(region).expect("every leaf of a mapped extent");

    assert_eq!((windows.hits(), windows.misses()), (1, 1), "the second domain cost a second read");
    assert_eq!(windows.len(), 1, "mapping a cached window cached it again");

    // The claim, read back out of the hardware's own tables: two views, one set
    // of frames, and it is the set the filesystem read into.
    for (view, who) in [(first, "the first domain"), (second, "the second domain")] {
        let leaf = platform.resident(view, start).expect("the mapped window, in the view's tables");
        assert_eq!(leaf.start(), start, "the mapping landed somewhere else");
        assert_eq!(leaf.size(), granule, "the mapping was cut smaller than the window");
        assert_eq!(leaf.base(), Some(span.start()), "{who} was given a copy, not the window");
        assert!(!leaf.protection().is_write(), "a read-only file mapping arrived writable");
        assert!(!leaf.protection().is_execute(), "a file mapping arrived executable");
        assert!(platform.resident(view, start + granule).is_none(), "the mapping ran past itself");
    }

    // SAFETY: the registry that borrowed these frames is gone, the platform
    // hands each claimed span out once, and the physmap covers all of it.
    let cached = unsafe { core::slice::from_raw_parts((offset + span.start()) as *const u8, read) };
    assert_eq!(cached, CONTENT, "mapping the window moved its bytes");

    let holders = windows.lookup(WINDOW, 0).map(Window::holders).expect("the cached window");
    assert_eq!(
        Some(holders),
        leaves.count(start),
        "the cache and the leaf counts disagree about who holds the window"
    );

    report!(
        platform,
        "MOLT_FILE_MAP_OK: {MAPPING} at {start:#x} over frames at {base:#x}, {} MiB in 1 leaf per \
         view, {holders} views, {} device read, 0 copies",
        granule >> 20,
        windows.misses(),
    );

    unmap(platform, &mut space, &mut windows, &mut leaves, span, [first, second]);
}

/// Reads the file into the claimed frames, and says how many bytes landed.
///
/// This is the one copy there is, and it is the device's: the buffer the
/// filesystem writes into *is* the page cache, because the kernel reaches a
/// claimed frame through the physmap and the cache is what it hands out
/// afterwards. Nothing after this moves a byte.
fn fill<Q: Queue>(fs: &mut Fs<Q, HANDLES>, offset: u64, span: Span) -> usize {
    // SAFETY: the platform hands each claimed span out once and keeps no claim
    // on it, and the physmap maps every frame of it.
    let bytes = unsafe {
        core::slice::from_raw_parts_mut((offset + span.start()) as *mut u8, span.bytes() as usize)
    };

    let mut buffers = BufferRegistry::<1>::new();
    let buffer = buffers.register_read_write(CACHE, bytes).expect("a free buffer slot");
    let target = buffers.write_capability(buffer).expect("a writable view of the window");
    let root = fs.root(CACHE).expect("the bootstrap root");
    let name = Name::try_from(MAPPING).expect("a name the volume allows");
    let opened = fs
        .apply(CACHE, FsOp::Open { dir: root, name }, &mut buffers)
        .expect("a file on the volume");
    let Some(Handle::File(file)) = opened.handle() else {
        panic!("{MAPPING} opened as a directory: {opened:?}");
    };
    let stat = fs.apply(CACHE, FsOp::Stat(Handle::File(file)), &mut buffers).expect("an open file");
    let FsDone::Stat(stat) = stat else {
        panic!("a stat answered with {stat:?}");
    };

    let len = stat.size as usize;
    let window = BufferOperation::new(target, 0, len);
    let read = fs.apply(CACHE, FsOp::Read { file, buffer: window, offset: 0 }, &mut buffers);
    assert_eq!(read, Ok(FsDone::Read(len)), "the window did not fill");
    assert_eq!(buffers.resolve_write(window).expect("the filled window"), CONTENT, "wrong bytes");

    fs.apply(CACHE, FsOp::Close(Handle::File(file)), &mut buffers).expect("an open handle");
    fs.apply(CACHE, FsOp::Close(Handle::Dir(root)), &mut buffers).expect("an open handle");
    len
}

/// Takes the window out of both views and gives its addresses back.
///
/// The order is not the caller's taste: the leaves go first, because an address
/// still in a view is an address a core can still walk; the cache hands the
/// extent out rather than dropping it, because dropping it would be the
/// allocator handing the range to somebody else; and the retire waits for every
/// core to say it flushed.
fn unmap<P: Platform>(
    platform: &mut P,
    space: &mut Space<'_>,
    windows: &mut Windows<'_>,
    leaves: &mut Leaves<'_>,
    span: Span,
    views: [View; 2],
) {
    let region = windows.lookup(WINDOW, 0).and_then(Window::region).expect("the cached window");
    let start = region.start();
    let mapping = windows.lookup(WINDOW, 0).and_then(Window::extent).expect("the cached window");

    for view in views {
        assert_eq!(
            platform.revoke(view, mapping).expect("leaves this view holds"),
            1,
            "a revoke took a different number of leaves"
        );
        assert!(platform.resident(view, start).is_none(), "a revoked window still translated");
    }

    assert!(leaves.release(region).expect("leaves a view holds").is_empty(), "one holder freed it");
    assert_eq!(windows.release(WINDOW, 0), Ok(1), "a release took more than one reference");
    assert!(!leaves.release(region).expect("leaves a view holds").is_empty(), "nobody freed it");
    assert_eq!(windows.release(WINDOW, 0), Ok(0));

    // Only a window nobody has mapped comes out, and what comes out is work the
    // caller still owes: the addresses are not free until they are retired.
    let (extent, frames) = windows.evict(WINDOW, 0).expect("a window nobody holds");
    assert_eq!(frames, span, "the window came back over frames it was never read into");
    assert_eq!(extent.start(), start, "the window came back at an address it never had");
    assert!(windows.is_empty(), "an evicted window stayed cached");

    space.release(extent).expect("an extent this space issued");
    let epoch = space.sweep();
    let mut shootdown = Shootdown::new();
    shootdown.begin(epoch, smp::attending()).expect("a core to flush");
    let held = space.allocate(MAPPED, MAPPED.granule()).expect("room beside the quarantine");
    assert_ne!(held.start(), start, "an unflushed window was handed out again");
    space.release(held).expect("an extent this space issued");

    let (flushed, asked) = smp::flush(smp::current());
    assert_eq!(flushed.len() as u16, asked + 1, "a core took the flush and never answered");
    let mut retirable = None;
    for cpu in flushed {
        assert!(retirable.is_none(), "the round closed with cores still owing a flush");
        retirable = shootdown.acknowledge(cpu).expect("a core this round asked");
    }
    let retired = retirable.expect("the epoch every core has now flushed");

    space.retire(retired);
    let again = space.allocate(MAPPED, MAPPED.granule()).expect("the flushed range, back");
    assert_eq!(again.start(), start, "a flushed window did not come back");
    space.release(again).expect("an extent this space issued");
}
