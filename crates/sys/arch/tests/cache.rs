use molt_arch::cache::{Error, File, Window, Windows};
use molt_arch::memory::Span;
use molt_arch::va::{Class, Extent, Hole, Space};

/// Sv57, which is what the boards molt maps report.
const BITS: u32 = 57;
const GIGA: u64 = Class::Giga.granule();

const LOGS: File = File::new(1);
const OTHER: File = File::new(2);

fn cache(slots: usize) -> (Space<'static>, Windows<'static>) {
    let holes = vec![Hole::EMPTY; 3 * 8].leak();
    let windows = Vec::from_iter((0..slots).map(|_| Window::EMPTY)).leak();
    (Space::over(BITS, holes).expect("a space wide enough to cut"), Windows::over(windows))
}

/// Frames somewhere plausible, aligned to whatever the extent needs.
fn frames(base: u64, extent: &Extent) -> Span {
    Span::new(base, base + extent.bytes()).expect("a span the size of the extent")
}

#[test]
fn window_cached_at_one_address() -> Result<(), Error> {
    let (mut space, mut windows) = cache(4);

    let extent = space.allocate(Class::Giga, GIGA).expect("room in the gigabyte arena");
    let backing = frames(4 * GIGA, &extent);
    let first = windows.insert(LOGS, 0, extent, backing)?.region().expect("a cached window");

    let second = windows.hold(LOGS, 0)?;

    assert_eq!(second.region(), Some(first), "one window of one file got two addresses");
    assert_eq!(second.holders(), 2, "the second holder was not counted");
    assert_eq!((windows.hits(), windows.misses()), (1, 0));
    assert_eq!(windows.len(), 1, "holding a cached window cached it again");
    assert_eq!(windows.bytes(), GIGA);
    Ok(())
}

#[test]
fn uncached_window_misses() {
    let (_space, mut windows) = cache(4);

    assert_eq!(windows.hold(LOGS, 0).unwrap_err(), Error::Unknown);
    assert_eq!((windows.hits(), windows.misses()), (0, 1));
    assert!(windows.is_empty());
}

#[test]
fn windows_of_one_file_are_told_apart_by_offset() -> Result<(), Error> {
    let (mut space, mut windows) = cache(4);

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
fn same_window_not_cached_twice() -> Result<(), Error> {
    let (mut space, mut windows) = cache(4);

    let extent = space.allocate(Class::Giga, GIGA).expect("room in the gigabyte arena");
    let again = space.allocate(Class::Giga, GIGA).expect("room in the gigabyte arena");
    let backing = (frames(4 * GIGA, &extent), frames(8 * GIGA, &again));
    windows.insert(LOGS, 0, extent, backing.0)?;

    assert_eq!(windows.insert(LOGS, 0, again, backing.1).unwrap_err(), Error::Present);
    Ok(())
}

#[test]
fn window_starts_on_leaf_boundary() {
    let (mut space, mut windows) = cache(4);

    let extent = space.allocate(Class::Giga, GIGA).expect("room in the gigabyte arena");
    let backing = frames(4 * GIGA, &extent);

    assert_eq!(windows.insert(LOGS, 4096, extent, backing).unwrap_err(), Error::Misaligned);
}

#[test]
fn nonleaf_frames_refused() {
    let (mut space, mut windows) = cache(4);

    let extent = space.allocate(Class::Giga, GIGA).expect("room in the gigabyte arena");
    let short = Span::new(4 * GIGA, 4 * GIGA + Class::Mega.granule()).expect("a megabyte");

    assert_eq!(windows.insert(LOGS, 0, extent, short).unwrap_err(), Error::Backing);
}

#[test]
fn unaligned_frames_are_refused() {
    let (mut space, mut windows) = cache(4);

    let extent = space.allocate(Class::Giga, GIGA).expect("room in the gigabyte arena");
    let askew = Span::new(4 * GIGA + Class::Mega.granule(), 6 * GIGA).expect("a long enough span");

    assert_eq!(windows.insert(LOGS, 0, extent, askew).unwrap_err(), Error::Backing);
}

#[test]
fn held_window_not_evicted() -> Result<(), Error> {
    let (mut space, mut windows) = cache(4);

    let extent = space.allocate(Class::Giga, GIGA).expect("room in the gigabyte arena");
    let start = extent.start();
    let backing = frames(4 * GIGA, &extent);
    windows.insert(LOGS, 0, extent, backing)?;
    windows.hold(LOGS, 0)?;

    assert_eq!(windows.evict(LOGS, 0).unwrap_err(), Error::Held);
    assert_eq!(windows.release(LOGS, 0), Ok(1), "a release took more than one reference");
    assert_eq!(windows.evict(LOGS, 0).unwrap_err(), Error::Held);
    assert_eq!(windows.release(LOGS, 0), Ok(0));

    let (extent, evicted) = windows.evict(LOGS, 0)?;
    assert_eq!((extent.start(), evicted), (start, backing));
    assert!(windows.is_empty());
    assert_eq!(windows.evict(LOGS, 0).unwrap_err(), Error::Unknown);
    space.release(extent).expect("an extent this space issued");
    Ok(())
}

#[test]
fn untaken_reference_not_returned() -> Result<(), Error> {
    let (mut space, mut windows) = cache(4);

    let extent = space.allocate(Class::Giga, GIGA).expect("room in the gigabyte arena");
    let backing = frames(4 * GIGA, &extent);
    windows.insert(LOGS, 0, extent, backing)?;

    assert_eq!(windows.release(LOGS, 0), Ok(0));
    assert_eq!(windows.release(LOGS, 0), Err(Error::Unreferenced));
    assert_eq!(windows.release(OTHER, 0), Err(Error::Unknown));
    Ok(())
}

#[test]
fn eviction_keeps_neighbour_windows() -> Result<(), Error> {
    let (mut space, mut windows) = cache(4);

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
fn full_cache_says_so() -> Result<(), Error> {
    let (mut space, mut windows) = cache(1);

    let extent = space.allocate(Class::Giga, GIGA).expect("room in the gigabyte arena");
    let next = space.allocate(Class::Giga, GIGA).expect("room in the gigabyte arena");
    let backing = (frames(4 * GIGA, &extent), frames(8 * GIGA, &next));
    windows.insert(LOGS, 0, extent, backing.0)?;

    assert_eq!(windows.insert(LOGS, GIGA, next, backing.1).unwrap_err(), Error::Storage);
    Ok(())
}
