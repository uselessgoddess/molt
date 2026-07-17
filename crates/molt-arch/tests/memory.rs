use molt_arch::{
    FrameAllocator, MapPermissions, MappingError, MemoryMap, MemoryRegion, MemoryRegionKind,
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
fn write_exec_mappings_rejected() {
    assert_eq!(MapPermissions::new(true, true), Err(MappingError::WritableExecutable));
    assert!(MapPermissions::new(true, false).unwrap().is_writable());
    assert!(MapPermissions::new(false, true).unwrap().is_executable());
}
