use molt_arch::{
    FrameAllocator, ImageSection, MapPermissions, MappingError, MemoryMap, MemoryRegion,
    MemoryRegionKind, PageProtection,
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
    assert!(MapPermissions::new(false, true).unwrap().is_exec());
}
