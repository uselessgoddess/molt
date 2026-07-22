use molt_arch::memory::{Cache, Error, Inventory, Kind, Rights, Span};
use molt_arch::{MappingError, MemoryMap, MemoryRegion, MemoryRegionKind};

struct TestMap([MemoryRegion; 3]);

impl MemoryMap for TestMap {
    fn len(&self) -> usize {
        self.0.len()
    }

    fn region(&self, index: usize) -> Option<MemoryRegion> {
        self.0.get(index).copied()
    }
}

fn map() -> TestMap {
    TestMap([
        MemoryRegion::new(0x1000, 0x4000, MemoryRegionKind::Usable),
        MemoryRegion::new(0x4000, 0x5000, MemoryRegionKind::Firmware(0)),
        MemoryRegion::new(0x5000, 0x8000, MemoryRegionKind::Usable),
    ])
}

#[test]
fn firmware_regions_reserved_holes_are_devices() {
    let map = map();
    let inventory = Inventory::new(&map);

    assert_eq!(inventory.kind(0x1000), Kind::Ram);
    assert_eq!(inventory.kind(0x4000), Kind::Reserved);
    assert_eq!(inventory.kind(0x9000), Kind::Device, "no region covers the hole");
    assert_eq!(inventory.kind(0), Kind::Device);
}

#[test]
fn image_outranks_region() {
    let map = map();
    let inventory = Inventory::new(&map).with_image(Span::new(0x5000, 0x6000).unwrap());

    assert_eq!(inventory.kind(0x5000), Kind::Image);
    assert_eq!(inventory.kind(0x6000), Kind::Ram, "one frame past the image");
}

#[test]
fn a_span_straddling_two_kinds_has_none() {
    let map = map();
    let inventory = Inventory::new(&map);

    assert_eq!(inventory.classify(Span::new(0x1000, 0x4000).unwrap()), Ok(Kind::Ram));
    assert_eq!(inventory.classify(Span::new(0x3000, 0x5000).unwrap()), Err(Error::Mixed));
    assert_eq!(inventory.classify(Span::new(0x8000, 0xa000).unwrap()), Ok(Kind::Device));
}

#[test]
fn a_device_window_inside_ram_is_refused() {
    let map = map();
    let inventory = Inventory::new(&map);

    assert_eq!(inventory.device(Span::new(0x1000, 0x2000).unwrap()), Err(Error::Kind));
    assert_eq!(inventory.device(Span::new(0x4000, 0x5000).unwrap()), Err(Error::Kind));
}

#[test]
fn a_device_window_is_uncached_and_never_executable() {
    let map = map();
    let inventory = Inventory::new(&map);
    let window = inventory.device(Span::new(0x8000, 0x9000).unwrap()).unwrap();

    assert_eq!(window.mapping(Rights::READ_WRITE), Ok((Rights::READ_WRITE, Cache::Device)));
    assert_eq!(window.mapping(Rights::READ_EXECUTE), Err(MappingError::Permissions));
}

#[test]
fn ram_is_never_mapped_with_device_ordering() {
    assert_eq!(
        Kind::Ram.allows(Rights::READ_WRITE, Cache::Device),
        Err(MappingError::Cacheability)
    );
    assert_eq!(Kind::Ram.allows(Rights::READ_WRITE, Cache::WriteBack), Ok(()));
    assert_eq!(Kind::Image.allows(Rights::READ_EXECUTE, Cache::WriteBack), Ok(()));
    assert_eq!(
        Kind::Device.allows(Rights::READ_WRITE, Cache::WriteBack),
        Err(MappingError::Cacheability),
    );
    assert_eq!(
        Kind::Reserved.allows(Rights::READ, Cache::WriteBack),
        Err(MappingError::Permissions),
        "firmware's memory is not the kernel's to map",
    );
}
