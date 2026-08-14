//! The one address space, proven on the machine rather than on the host.
//!
//! `molt-arch` tests the allocator, the leaf counts and the shootdown protocol
//! as arithmetic. What only a booted kernel can show is that the width they were
//! cut from is this machine's own answer, that other cores really run the flush
//! instruction, and that a second view of the address space does not contain the
//! kernel. Tier 2 of `docs/address-space.md`, marker by marker.

use alloc::boxed::Box;

use molt_arch::asid::{Asids, Flush};
use molt_arch::memory::{Rights, Span};
use molt_arch::refcount::{self, Leaves, Run};
use molt_arch::va::{Class, Extent, Region};
use molt_arch::{BootInfo, FRAME_SIZE, Platform, PlatformError, SerialWriter, View, view};
use molt_kernel::report;
use molt_rt::Executor;

use crate::config::CONFIG;
use crate::space;

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
