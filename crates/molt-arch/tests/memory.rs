use molt_arch::{
    FrameAllocator, FramePool, ImageSection, MapPermissions, MappingError, MemoryMap, MemoryRegion,
    MemoryRegionKind, PageProtection, UsableRange, UsableRegions,
};

struct TestMap([MemoryRegion; 3]);

impl MemoryMap for TestMap {
    fn len(&self) -> usize {
        self.0.len()
    }

    fn region(&self, index: usize) -> Option<MemoryRegion> {
        self.0.get(index).copied()
    }
}

#[test]
fn aligned_usable_mem() {
    let map = TestMap([
        MemoryRegion::new(0, 0x3000, MemoryRegionKind::Reserved),
        MemoryRegion::new(0x3101, 0x6100, MemoryRegionKind::Usable),
        MemoryRegion::new(0x8000, 0x9000, MemoryRegionKind::Bootloader),
    ]);
    let mut allocator = FrameAllocator::new(&map);

    assert_eq!(allocator.allocate().map(|frame| frame.start()), Some(0x4000));
    assert_eq!(allocator.allocate().map(|frame| frame.start()), Some(0x5000));
    assert_eq!(allocator.allocate(), None);
}

#[test]
fn resumed_allocator_skips_taken_frames() {
    let map = TestMap([
        MemoryRegion::new(0, 0x1000, MemoryRegionKind::Reserved),
        MemoryRegion::new(0x4000, 0x8000, MemoryRegionKind::Usable),
        MemoryRegion::new(0x9000, 0xa000, MemoryRegionKind::Usable),
    ]);
    let mut first = FrameAllocator::new(&map);

    assert_eq!(first.allocate().map(|frame| frame.start()), Some(0x4000));
    let mut second = FrameAllocator::resume(&map, first.cursor());

    assert_eq!(second.allocate().map(|frame| frame.start()), Some(0x5000), "no frame is reissued");
}

#[test]
fn usable_ranges_are_aligned_inward() {
    let map = TestMap([
        MemoryRegion::new(0x3101, 0x6100, MemoryRegionKind::Usable),
        MemoryRegion::new(0x7001, 0x7fff, MemoryRegionKind::Usable),
        MemoryRegion::new(0x9000, 0xb000, MemoryRegionKind::Reserved),
    ]);

    let ranges: Vec<_> = UsableRegions::above(&map, 0).collect();

    assert_eq!(
        ranges.iter().map(|range| (range.start(), range.end())).collect::<Vec<_>>(),
        [(0x4000, 0x6000)],
    );
}

#[test]
fn usable_ranges_start_above_floor() {
    let map = TestMap([
        MemoryRegion::new(0, 0x9000, MemoryRegionKind::Usable),
        MemoryRegion::new(0x9000, 0xa000, MemoryRegionKind::Reserved),
        MemoryRegion::new(0xa000, 0xc000, MemoryRegionKind::Usable),
    ]);

    let ranges: Vec<_> = UsableRegions::above(&map, 0x4000).collect();

    assert_eq!(
        ranges.iter().map(|range| (range.start(), range.end())).collect::<Vec<_>>(),
        [(0x4000, 0x9000), (0xa000, 0xc000)],
    );
}

#[test]
fn allocated_frames_lie_in_mapped_ranges() {
    let map = TestMap([
        MemoryRegion::new(0, 0x5000, MemoryRegionKind::Usable),
        MemoryRegion::new(0x5000, 0x6000, MemoryRegionKind::Firmware(3)),
        MemoryRegion::new(0x6000, 0x8000, MemoryRegionKind::Usable),
    ]);
    let ranges: Vec<_> = UsableRegions::above(&map, 0x3000).collect();
    let mut allocator = FrameAllocator::above(&map, 0x3000);

    let frames: Vec<_> =
        core::iter::from_fn(|| allocator.allocate()).map(|frame| frame.start()).take(8).collect();

    assert_eq!(frames, [0x3000, 0x4000, 0x6000, 0x7000]);
    assert!(frames.iter().all(|&frame| {
        ranges.iter().any(|range| range.start() <= frame && frame < range.end())
    }),);
}

#[test]
fn reserved_regions_never_become_usable() {
    let firmware = MemoryRegion::new(0x1000, 0x2000, MemoryRegionKind::Firmware(0));
    let reserved = MemoryRegion::new(0x2000, 0x3000, MemoryRegionKind::Reserved);

    assert_eq!(UsableRange::of(firmware, 0), None);
    assert_eq!(UsableRange::of(reserved, 0), None);
}

#[test]
fn writable_text_is_rejected() {
    let writable_text = PageProtection::new(true, true, true);

    assert_eq!(ImageSection::Text.verify(writable_text), Err(MappingError::WritableExecutable));
    assert_eq!(ImageSection::Text.verify(PageProtection::new(true, false, true)), Ok(()));
}

#[test]
fn executable_data_is_rejected() {
    assert_eq!(
        ImageSection::Rodata.verify(PageProtection::new(true, false, true)),
        Err(MappingError::Permissions)
    );
    assert_eq!(
        ImageSection::Data.verify(PageProtection::new(true, false, false)),
        Err(MappingError::Permissions),
    );
    assert_eq!(ImageSection::Data.verify(PageProtection::new(true, true, false)), Ok(()));
}

#[test]
fn write_exec_mappings_rejected() {
    assert_eq!(MapPermissions::new(true, true), Err(MappingError::WritableExecutable));
    assert!(MapPermissions::new(true, false).unwrap().is_write());
    assert!(MapPermissions::new(false, true).unwrap().is_execute());
}

#[test]
fn pool_hands_out_reserved_frames_once() {
    let map = TestMap([
        MemoryRegion::new(0, 0x1000, MemoryRegionKind::Reserved),
        MemoryRegion::new(0x4000, 0x8000, MemoryRegionKind::Usable),
        MemoryRegion::new(0x9000, 0xa000, MemoryRegionKind::Usable),
    ]);
    let mut allocator = FrameAllocator::new(&map);
    let mut pool = FramePool::<3>::empty();

    assert_eq!(pool.fill(&mut allocator), 3);
    assert_eq!(pool.remaining(), 3);
    let taken = [pool.allocate(), pool.allocate(), pool.allocate()]
        .map(|frame| frame.map(|frame| frame.start()));

    assert_eq!(taken, [Some(0x4000), Some(0x5000), Some(0x6000)]);
    assert_eq!(pool.allocate(), None, "a drained pool invented a frame");
    assert_eq!(pool.remaining(), 0);
    assert_eq!(allocator.allocate().map(|frame| frame.start()), Some(0x7000));
}

#[test]
fn pool_over_map_reports_actual() {
    let map = TestMap([
        MemoryRegion::new(0, 0x1000, MemoryRegionKind::Reserved),
        MemoryRegion::new(0x4000, 0x5000, MemoryRegionKind::Usable),
        MemoryRegion::new(0x8000, 0x9000, MemoryRegionKind::Reserved),
    ]);
    let mut pool = FramePool::<4>::empty();

    assert_eq!(pool.fill(&mut FrameAllocator::new(&map)), 1);
    assert!(pool.allocate().is_some());
    assert_eq!(pool.allocate(), None);
}
