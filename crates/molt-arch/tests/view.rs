//! What both ports do the same way when they open and fill a view.

use molt_arch::asid::Asids;
use molt_arch::audit::Leaf;
use molt_arch::memory::Span;
use molt_arch::va::{Class, Extent, Hole, Space};
use molt_arch::view::{Error, VIEWS, Views, backing, leaves};
use molt_arch::{PageProtection, PlatformError};

const SV57: u32 = 57;
const MEGA: u64 = Class::Mega.granule();

/// Where the frames under a grant are; nothing here reads them.
const RAM: u64 = 1 << 30;

/// A space over storage it outlives, the way the kernel hands it a static.
fn space() -> Space<'static> {
    Space::over(SV57, vec![Hole::EMPTY; 9].leak()).expect("a space wide enough to cut")
}

fn extent(space: &mut Space<'_>, class: Class, bytes: u64) -> Extent {
    space.allocate(class, bytes).expect("room in the arena")
}

#[test]
fn root_built_only_with_slot() {
    let mut asids = Asids::new(16);
    let mut views = Views::<u64>::EMPTY;
    let mut built = 0;
    let mut open = || {
        views.open::<PlatformError>(asids.assign().asid(), || {
            built += 1;
            Ok(built)
        })
    };

    for _ in 0..VIEWS {
        open().expect("a free slot");
    }

    assert_eq!(open(), Err(PlatformError::View(Error::Capacity)));
    assert_eq!(built, VIEWS as u64, "a root was allocated into a table with nowhere to put it");
}

#[test]
fn views_keep_their_own_roots() {
    let mut asids = Asids::new(16);
    let mut views = Views::<u64>::EMPTY;

    let first = views.open::<PlatformError>(asids.assign().asid(), || Ok(0x1000)).expect("a slot");
    let second = views.open::<PlatformError>(asids.assign().asid(), || Ok(0x2000)).expect("a slot");

    assert_eq!(views.root(first), Ok(0x1000));
    assert_eq!(views.root(second), Ok(0x2000), "the second view was handed the first one's root");
    assert_ne!(first.index(), second.index());
    assert_ne!(first.asid(), second.asid(), "two views share one tag");
}

#[test]
fn foreign_view_is_unknown() {
    let views = Views::<u64>::EMPTY;
    let mut asids = Asids::new(16);
    let mut other = Views::<u64>::EMPTY;
    let elsewhere =
        other.open::<PlatformError>(asids.assign().asid(), || Ok(0x1000)).expect("a slot");

    assert_eq!(views.root(elsewhere), Err(Error::Unknown));
}

#[test]
fn grant_walks_every_leaf() {
    let mut space = space();
    let extent = extent(&mut space, Class::Mega, 3 * MEGA);
    let span = Span::new(RAM, RAM + 3 * MEGA).expect("frames to back it");

    let mapped: Vec<_> = backing(&extent, span).expect("a span that covers it").collect();

    assert_eq!(mapped.len(), 3, "the leaves the port has to map were not all named");
    assert_eq!(mapped[0], (extent.start(), RAM));
    assert_eq!(mapped[1], (extent.start() + MEGA, RAM + MEGA));
    assert_eq!(mapped[2], (extent.start() + 2 * MEGA, RAM + 2 * MEGA));
    assert_eq!(
        leaves(&extent).collect::<Vec<_>>(),
        mapped.iter().map(|&(va, _)| va).collect::<Vec<_>>()
    );
    space.release(extent).expect("the extent this test cut");
}

#[test]
fn short_or_askew_frames_refused() {
    let mut space = space();
    let extent = extent(&mut space, Class::Mega, 2 * MEGA);

    let short = Span::new(RAM, RAM + MEGA).expect("a span");
    assert!(matches!(backing(&extent, short), Err(Error::Backing)), "half a grant was accepted");

    // Frames one page into a megabyte, which cannot be the base of a megabyte
    // leaf: the hardware would drop the low bits and map somebody else's.
    let askew = Span::new(RAM + 4096, RAM + 4096 + 2 * MEGA).expect("a span");
    assert!(matches!(backing(&extent, askew), Err(Error::Backing)), "a misaligned base was mapped");
    space.release(extent).expect("the extent this test cut");
}

#[test]
fn leaf_reported_at_level_size() {
    let rights = PageProtection::new(true, true, false);
    // An address and a frame both a page into the two megabytes that hold them,
    // which is what a walk of a level-1 entry is handed.
    let leaf = Leaf::at(1, 0x4000_0000 + 4096, rights, 0x8000_0000 + 4096);

    assert_eq!(leaf.size(), 2 << 20);
    assert_eq!(leaf.start(), 0x4000_0000, "the leaf claimed a boundary the hardware has not got");
    assert_eq!(leaf.base(), Some(0x8000_0000), "the frame was reported a page past where it is");
    assert_eq!(Leaf::at(0, 0x4000_0000, rights, 0x8000_0000).size(), 4096);
    assert_eq!(Leaf::at(2, 0x4000_0000, rights, 0x8000_0000).size(), 1 << 30);
}
