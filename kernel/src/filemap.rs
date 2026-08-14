//! A file, mapped where it already is.
//!
//! One address space means one copy: the window is read into frames once, given
//! an address out of the allocator every other extent comes from, and handed to
//! a domain as a leaf ([`molt_arch::cache`], `docs/address-space.md`).
//!
//! What this proves is that the leaf in a domain's tables points at the frames
//! the filesystem read into, not at a copy made for it — `resident` reads the
//! physical address back out of the live tables and both views report the same
//! one, so the device is read once however many views look at it.
//!
//! The teardown is the order [`molt_arch::view`] spells out: leaves out of the
//! views first, the cache handing the extent back rather than dropping it, and
//! the addresses nobody's until every core has flushed.

use molt_arch::asid::Asids;
use molt_arch::cache::{self, Window, Windows};
use molt_arch::memory::{Rights, Span};
use molt_arch::refcount::{Leaves, Run};
use molt_arch::va::{Class, Space};
use molt_arch::{BootInfo, FRAME_SIZE, Platform, SerialWriter, View};
use molt_block::Queue;
use molt_core::CellId;
use molt_core::buffer::{BufferOperation, BufferRegistry};
use molt_fs::{Fs, FsDone, FsOp, Handle, Name};
use molt_kernel::report;

use crate::config::CONFIG;
use crate::{smp, space};

/// The kernel's page cache, which is no domain's — so no domain asked for the
/// read.
const CACHE: CellId = CellId::new(4);
/// The root, the file under it, and room to be wrong about that.
const HANDLES: usize = 4;

/// The class a window is mapped at: megabytes, because QEMU is started with two
/// gigabytes and a window no frames back would prove nothing. What the design is
/// for is the class — a gigabyte-class window is the same one entry per view,
/// and `molt-arch` tests that on the host.
const MAPPED: Class = Class::Mega;

/// The file the smoke maps, and what the image builder put in it.
const MAPPING: &str = "hello.txt";
const CONTENT: &[u8] = b"hello, molt\n";

/// The window's key. A constant because there is one file and one offset;
/// handing out object ids is the filesystem's job, and nothing on the ring does
/// it yet.
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

    let mut space = space::global();
    let mut slots = [const { Window::EMPTY }; CONFIG.windows];
    let mut windows = Windows::over(&mut slots);
    let mut runs = [Run::EMPTY; CONFIG.runs];
    let mut leaves = Leaves::over(&mut runs);

    // Nothing is cached, which is what sent the kernel to the device above.
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

    // The first mapping, read-only, at a cost of one leaf and no bytes.
    let mapping = windows.lookup(WINDOW, 0).and_then(Window::extent).expect("the cached window");
    platform.grant(first, mapping, span, Rights::READ).expect("a leaf the view lacked");

    // The second domain asks for the same window and is told the same address:
    // no read, no copy, no second extent.
    let held = windows.hold(WINDOW, 0).expect("a window the first domain left cached");
    assert_eq!(held.region(), Some(region), "one window of one file got two addresses");
    platform
        .grant(second, held.extent().expect("the cached window"), span, Rights::READ)
        .expect("a leaf the second view lacked");
    leaves.share(region).expect("every leaf of a mapped extent");

    assert_eq!((windows.hits(), windows.misses()), (1, 1), "the second domain cost a second read");
    assert_eq!(windows.len(), 1, "mapping a cached window cached it again");

    // The claim, out of the hardware's own tables: two views, one set of frames,
    // and it is the set the filesystem read into.
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
/// The one copy there is, and it is the device's: the buffer the filesystem
/// writes into *is* the page cache, reached through the physmap. Nothing after
/// this moves a byte.
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
/// The order is fixed: leaves first, because an address still in a view is one a
/// core can walk; then the cache hands the extent out rather than dropping it;
/// then [`space::recycle`] waits for every core to say it flushed.
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

    // Only an unmapped window comes out, and what comes out is work still owed:
    // the addresses are not free until they are retired.
    let (extent, frames) = windows.evict(WINDOW, 0).expect("a window nobody holds");
    assert_eq!(frames, span, "the window came back over frames it was never read into");
    assert_eq!(extent.start(), start, "the window came back at an address it never had");
    assert!(windows.is_empty(), "an evicted window stayed cached");

    // The addresses rejoin the space they came from, and not one flush sooner;
    // `recycle` asserts both on the way through.
    space::recycle(smp::current(), space, extent);
}
