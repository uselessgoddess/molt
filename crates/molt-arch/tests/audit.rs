use molt_arch::audit::{Audit, Contents, Declared, Leaf, MappedRange, PageWalk};
use molt_arch::{Cache, FRAME_SIZE, ImageSection, MappingError, PageProtection};

const TEXT: u64 = 0x8020_0000;
const RODATA: u64 = 0x8020_2000;
const RAM: u64 = 0x8040_0000;

fn read_execute() -> PageProtection {
    PageProtection::new(true, false, true)
}

fn read_only() -> PageProtection {
    PageProtection::new(true, false, false)
}

fn read_write() -> PageProtection {
    PageProtection::new(true, true, false)
}

struct Table(Vec<Leaf>);

impl PageWalk for Table {
    fn leaf(&self, address: u64) -> Option<Leaf> {
        self.0.iter().copied().find(|leaf| leaf.start() <= address && address < leaf.end())
    }
}

/// `.text` and `.rodata`, one 4 KiB leaf each, mapped as they should be.
fn image() -> Vec<Leaf> {
    vec![
        Leaf::new(TEXT, FRAME_SIZE, read_execute()),
        Leaf::new(TEXT + FRAME_SIZE, FRAME_SIZE, read_execute()),
        Leaf::new(RODATA, FRAME_SIZE, read_only()),
    ]
}

fn image_ranges() -> [MappedRange; 2] {
    [
        MappedRange::section(ImageSection::Text, TEXT, RODATA),
        MappedRange::section(ImageSection::Rodata, RODATA, RODATA + FRAME_SIZE),
    ]
}

#[test]
fn correct_image_passes_the_audit() {
    let ranges = image_ranges();
    let audit = Audit::new(&ranges);
    let table = Table(image());

    assert_eq!(audit.cover(&table), Ok(()));
    for leaf in table.0.iter().copied() {
        assert_eq!(audit.accepts(leaf), Ok(()));
    }
}

#[test]
fn writable_page_between_correct_probes_is_caught() {
    let ranges = image_ranges();
    let mut leaves = image();
    // Exactly what probing `__text_start` and `__text_end - 1` would miss.
    leaves[1] = Leaf::new(TEXT + FRAME_SIZE, FRAME_SIZE, PageProtection::new(true, true, true));
    let table = Table(leaves);

    assert_eq!(Audit::new(&ranges).cover(&table), Err(MappingError::WritableExecutable));
}

#[test]
fn hole_in_declared_range_is_caught() {
    let ranges = image_ranges();
    let mut leaves = image();
    leaves.remove(1);
    let table = Table(leaves);

    assert_eq!(Audit::new(&ranges).cover(&table), Err(MappingError::Unmapped));
}

#[test]
fn megapage_over_image_sections_is_caught() {
    let ranges = image_ranges();
    // One 2 MiB leaf cannot hold `.text` and `.rodata` at once, so mapping the
    // image with it hands `.rodata` execute rights.
    let table = Table(vec![Leaf::new(TEXT, 2 * 1024 * 1024, read_execute())]);

    assert_eq!(Audit::new(&ranges).cover(&table), Err(MappingError::Granularity));
}

#[test]
fn free_ram_may_use_megapages_but_never_execute() {
    let megapage = 2 * 1024 * 1024;
    let ranges = [MappedRange::ram(RAM, RAM + megapage)];
    let audit = Audit::new(&ranges);

    assert_eq!(audit.cover(&Table(vec![Leaf::new(RAM, megapage, read_write())])), Ok(()));
    assert_eq!(
        audit.cover(&Table(vec![Leaf::new(RAM, megapage, PageProtection::new(true, true, true))])),
        Err(MappingError::WritableExecutable)
    );
}

#[test]
fn megapage_reaching_past_its_range_is_caught() {
    let ranges = [MappedRange::ram(RAM, RAM + FRAME_SIZE)];
    let leaf = Leaf::new(RAM, 2 * 1024 * 1024, read_write());

    assert_eq!(Audit::new(&ranges).accepts(leaf), Err(MappingError::Straddling));
}

#[test]
fn mapping_nobody_declared_is_caught() {
    let ranges = image_ranges();
    // A reserved or firmware range mapped read/write is exactly this shape.
    let stray = Leaf::new(0x1000_0000, FRAME_SIZE, read_write());

    assert_eq!(Audit::new(&ranges).accepts(stray), Err(MappingError::Unexpected));
}

#[test]
fn imag_without_named_sections_enforces_wx() {
    let ranges = [MappedRange::image(TEXT, TEXT + FRAME_SIZE)];
    let audit = Audit::new(&ranges);

    assert_eq!(audit.cover(&Table(vec![Leaf::new(TEXT, FRAME_SIZE, read_execute())])), Ok(()));
    assert_eq!(audit.cover(&Table(vec![Leaf::new(TEXT, FRAME_SIZE, read_write())])), Ok(()));
    assert_eq!(
        audit.cover(&Table(vec![Leaf::new(
            TEXT,
            FRAME_SIZE,
            PageProtection::new(true, true, true)
        )])),
        Err(MappingError::WritableExecutable)
    );
    assert_eq!(
        Contents::Image.verify(Leaf::new(TEXT, 2 * 1024 * 1024, read_execute())),
        Err(MappingError::Granularity)
    );
}

#[test]
fn cacheable_device_window_is_caught() {
    let uart = 0x1000_0000;
    let ranges = [MappedRange::device(uart, uart + FRAME_SIZE)];
    let audit = Audit::new(&ranges);
    let device = read_write().cached(Cache::Device);

    assert_eq!(audit.cover(&Table(vec![Leaf::new(uart, FRAME_SIZE, device)])), Ok(()));
    assert_eq!(
        audit.cover(&Table(vec![Leaf::new(uart, FRAME_SIZE, read_write())])),
        Err(MappingError::Cacheability),
    );
}

#[test]
fn executable_device_window_is_caught() {
    let uart = 0x1000_0000;
    let leaf = Leaf::new(uart, FRAME_SIZE, read_execute().cached(Cache::Device));

    assert_eq!(Contents::Device.verify(leaf), Err(MappingError::Permissions));
}

#[test]
fn declarations_merge_into_ono() {
    let mut declared = Declared::<4>::new();

    declared.push(MappedRange::ram(RAM, RAM + FRAME_SIZE)).unwrap();
    declared.push(MappedRange::ram(RAM + FRAME_SIZE, RAM + 2 * FRAME_SIZE)).unwrap();

    assert_eq!(declared.as_slice(), &[MappedRange::ram(RAM, RAM + 2 * FRAME_SIZE)]);
}

#[test]
fn declarations_stay_apart() {
    let mut declared = Declared::<4>::new();

    declared.push(MappedRange::ram(RAM, RAM + FRAME_SIZE)).unwrap();
    declared.push(MappedRange::device(RAM + FRAME_SIZE, RAM + 2 * FRAME_SIZE)).unwrap();

    assert_eq!(declared.as_slice().len(), 2);
}

#[test]
fn empty_declarations_dropped() {
    let mut declared = Declared::<1>::new();

    declared.push(MappedRange::ram(RAM, RAM)).unwrap();

    assert!(declared.as_slice().is_empty());
}

#[test]
fn overflowing_declaration_error() {
    let mut declared = Declared::<1>::new();

    declared.push(MappedRange::ram(RAM, RAM + FRAME_SIZE)).unwrap();

    assert_eq!(
        declared.push(MappedRange::ram(RAM + 2 * FRAME_SIZE, RAM + 3 * FRAME_SIZE)),
        Err(MappingError::Backend)
    );
}
