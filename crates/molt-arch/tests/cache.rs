use molt_arch::cache::{Error, File, Window, Windows};
use molt_arch::memory::Span;
use molt_arch::va::{Class, Extent, Hole, Space};

/// Sv57, which is what the boards molt maps report.
const BITS: u32 = 57;
const HOLES: usize = 3 * 8;
const GIGA: u64 = Class::Giga.granule();

const LOGS: File = File::new(1);
const OTHER: File = File::new(2);

fn space(holes: &mut [Hole]) -> Space<'_> {
    Space::over(BITS, holes).expect("a space wide enough to cut")
}

/// Frames somewhere plausible, aligned to whatever the extent needs.
fn frames(base: u64, extent: &Extent) -> Span {
    Span::new(base, base + extent.bytes()).expect("a span the size of the extent")
}

#[test]
fn a_window_is_cached_at_one_address_for_everybody() -> Result<(), Error> {
    let mut holes = [Hole::EMPTY; HOLES];
    let mut space = space(&mut holes);
    let mut slots = [const { Window::EMPTY }; 4];
    let mut windows = Windows::over(&mut slots);

    let extent = space.allocate(Class::Giga, GIGA).expect("room in the gigabyte arena");
    let backing = frames(4 * GIGA, &extent);
    let first = windows.insert(LOGS, 0, extent, backing)?.region().expect("a cached window");

    // The second domain to want this window is told what the first was told.
    // Nothing is read, and nothing is copied: what it gets is an address.
    let second = windows.hold(LOGS, 0)?;

    assert_eq!(second.region(), Some(first), "one window of one file got two addresses");
    assert_eq!(second.holders(), 2, "the second holder was not counted");
    assert_eq!((windows.hits(), windows.misses()), (1, 0));
    assert_eq!(windows.len(), 1, "holding a cached window cached it again");
    assert_eq!(windows.bytes(), GIGA);
    Ok(())
}

#[test]
fn a_window_nobody_cached_is_a_miss() {
    let mut slots = [const { Window::EMPTY }; 4];
    let mut windows = Windows::over(&mut slots);

    assert_eq!(windows.hold(LOGS, 0).map(|_| ()), Err(Error::Unknown));
    assert_eq!((windows.hits(), windows.misses()), (0, 1));
    assert!(windows.is_empty());
}

#[test]
fn windows_of_one_file_are_told_apart_by_offset() -> Result<(), Error> {
    let mut holes = [Hole::EMPTY; HOLES];
    let mut space = space(&mut holes);
    let mut slots = [const { Window::EMPTY }; 4];
    let mut windows = Windows::over(&mut slots);

    let first = space.allocate(Class::Giga, GIGA).expect("room in the gigabyte arena");
    let next = space.allocate(Class::Giga, GIGA).expect("room in the gigabyte arena");
    let backing = (frames(4 * GIGA, &first), frames(8 * GIGA, &next));
    let one = windows.insert(LOGS, 0, first, backing.0)?.region();
    let two = windows.insert(LOGS, GIGA, next, backing.1)?.region();

    assert_ne!(one, two, "two windows of one file landed on each other");
    assert_eq!(windows.lookup(LOGS, GIGA).and_then(Window::region), two);
    assert!(windows.lookup(OTHER, 0).is_none(), "another file's window was found");
    assert_eq!((windows.hits(), windows.misses()), (0, 0), "a lookup counted as a hold");
    Ok(())
}

#[test]
fn the_same_window_is_not_cached_twice() -> Result<(), Error> {
    let mut holes = [Hole::EMPTY; HOLES];
    let mut space = space(&mut holes);
    let mut slots = [const { Window::EMPTY }; 4];
    let mut windows = Windows::over(&mut slots);

    let extent = space.allocate(Class::Giga, GIGA).expect("room in the gigabyte arena");
    let again = space.allocate(Class::Giga, GIGA).expect("room in the gigabyte arena");
    let backing = (frames(4 * GIGA, &extent), frames(8 * GIGA, &again));
    windows.insert(LOGS, 0, extent, backing.0)?;

    // A second address for a window that already has one is how a file ends up
    // with two page caches that disagree.
    assert_eq!(windows.insert(LOGS, 0, again, backing.1).map(|_| ()), Err(Error::Present));
    Ok(())
}

#[test]
fn a_window_has_to_start_on_its_own_leaf_boundary() {
    let mut holes = [Hole::EMPTY; HOLES];
    let mut space = space(&mut holes);
    let mut slots = [const { Window::EMPTY }; 4];
    let mut windows = Windows::over(&mut slots);

    let extent = space.allocate(Class::Giga, GIGA).expect("room in the gigabyte arena");
    let backing = frames(4 * GIGA, &extent);

    assert_eq!(windows.insert(LOGS, 4096, extent, backing).map(|_| ()), Err(Error::Misaligned));
}

#[test]
fn frames_that_could_not_be_a_leaf_are_refused() {
    let mut holes = [Hole::EMPTY; HOLES];
    let mut space = space(&mut holes);
    let mut slots = [const { Window::EMPTY }; 4];
    let mut windows = Windows::over(&mut slots);

    let extent = space.allocate(Class::Giga, GIGA).expect("room in the gigabyte arena");
    let short = Span::new(4 * GIGA, 4 * GIGA + Class::Mega.granule()).expect("a megabyte");

    // A gigabyte leaf over a megabyte of frames would translate a gigabyte of
    // addresses onto whatever follows.
    assert_eq!(windows.insert(LOGS, 0, extent, short).map(|_| ()), Err(Error::Backing));
}

#[test]
fn unaligned_frames_are_refused() {
    let mut holes = [Hole::EMPTY; HOLES];
    let mut space = space(&mut holes);
    let mut slots = [const { Window::EMPTY }; 4];
    let mut windows = Windows::over(&mut slots);

    let extent = space.allocate(Class::Giga, GIGA).expect("room in the gigabyte arena");
    let askew = Span::new(4 * GIGA + Class::Mega.granule(), 6 * GIGA).expect("a long enough span");

    assert_eq!(windows.insert(LOGS, 0, extent, askew).map(|_| ()), Err(Error::Backing));
}

#[test]
fn a_held_window_is_not_evicted() -> Result<(), Error> {
    let mut holes = [Hole::EMPTY; HOLES];
    let mut space = space(&mut holes);
    let mut slots = [const { Window::EMPTY }; 4];
    let mut windows = Windows::over(&mut slots);

    let extent = space.allocate(Class::Giga, GIGA).expect("room in the gigabyte arena");
    let start = extent.start();
    let backing = frames(4 * GIGA, &extent);
    windows.insert(LOGS, 0, extent, backing)?;
    windows.hold(LOGS, 0)?;

    assert_eq!(windows.evict(LOGS, 0).map(|(extent, _)| extent.start()), Err(Error::Held));
    assert_eq!(windows.release(LOGS, 0), Ok(1), "a release took more than one reference");
    assert_eq!(windows.evict(LOGS, 0).map(|(extent, _)| extent.start()), Err(Error::Held));
    assert_eq!(windows.release(LOGS, 0), Ok(0));

    // Only now, and the addresses come back out rather than going away: the
    // caller still owes the unmap, the shootdown, and the retire.
    let (extent, evicted) = windows.evict(LOGS, 0)?;
    assert_eq!((extent.start(), evicted), (start, backing));
    assert!(windows.is_empty());
    assert_eq!(windows.evict(LOGS, 0).map(|_| ()), Err(Error::Unknown));
    space.release(extent).expect("an extent this space issued");
    Ok(())
}

#[test]
fn a_reference_nobody_took_cannot_be_given_back() -> Result<(), Error> {
    let mut holes = [Hole::EMPTY; HOLES];
    let mut space = space(&mut holes);
    let mut slots = [const { Window::EMPTY }; 4];
    let mut windows = Windows::over(&mut slots);

    let extent = space.allocate(Class::Giga, GIGA).expect("room in the gigabyte arena");
    let backing = frames(4 * GIGA, &extent);
    windows.insert(LOGS, 0, extent, backing)?;

    assert_eq!(windows.release(LOGS, 0), Ok(0));
    assert_eq!(windows.release(LOGS, 0), Err(Error::Unreferenced));
    assert_eq!(windows.release(OTHER, 0), Err(Error::Unknown));
    Ok(())
}

#[test]
fn eviction_keeps_the_windows_around_it() -> Result<(), Error> {
    let mut holes = [Hole::EMPTY; HOLES];
    let mut space = space(&mut holes);
    let mut slots = [const { Window::EMPTY }; 4];
    let mut windows = Windows::over(&mut slots);

    let mut cached = [0; 3];
    for (index, at) in cached.iter_mut().enumerate() {
        let extent = space.allocate(Class::Giga, GIGA).expect("room in the gigabyte arena");
        let backing = frames((4 + index as u64) * GIGA, &extent);
        *at = extent.start();
        windows.insert(LOGS, index as u64 * GIGA, extent, backing)?;
        windows.release(LOGS, index as u64 * GIGA)?;
    }

    let (extent, _) = windows.evict(LOGS, GIGA)?;

    assert_eq!(extent.start(), cached[1]);
    assert_eq!(windows.len(), 2);
    assert_eq!(
        windows.lookup(LOGS, 0).and_then(Window::region).map(|at| at.start()),
        Some(cached[0])
    );
    assert_eq!(
        windows.lookup(LOGS, 2 * GIGA).and_then(Window::region).map(|at| at.start()),
        Some(cached[2]),
        "evicting a window in the middle lost the one after it"
    );
    space.release(extent).expect("an extent this space issued");
    Ok(())
}

#[test]
fn a_cache_with_no_room_says_so() -> Result<(), Error> {
    let mut holes = [Hole::EMPTY; HOLES];
    let mut space = space(&mut holes);
    let mut slots = [const { Window::EMPTY }; 1];
    let mut windows = Windows::over(&mut slots);

    let extent = space.allocate(Class::Giga, GIGA).expect("room in the gigabyte arena");
    let next = space.allocate(Class::Giga, GIGA).expect("room in the gigabyte arena");
    let backing = (frames(4 * GIGA, &extent), frames(8 * GIGA, &next));
    windows.insert(LOGS, 0, extent, backing.0)?;

    assert_eq!(windows.insert(LOGS, GIGA, next, backing.1).map(|_| ()), Err(Error::Storage));
    Ok(())
}
