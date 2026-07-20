use molt_arch::memory::{Error, FrameTable, Owner, Rights, Span};
use molt_arch::{FRAME_SIZE, MappingError};

fn table(slots: &mut [Option<Owner>]) -> FrameTable<'_> {
    FrameTable::over(Span::frames(0x4000, slots.len() as u64).unwrap(), slots).unwrap()
}

#[test]
fn claimed_frames_are_not_claimed_twice() {
    let mut slots = [None; 4];
    let mut table = table(&mut slots);
    let span = Span::frames(0x4000, 2).unwrap();

    let frames = table.claim(span, Owner::Kernel).unwrap();

    assert_eq!(table.claim(span, Owner::Tables), Err(Error::Owned));
    assert_eq!(frames.owner(), Owner::Kernel);
}

#[test]
fn overlapping_claim_takes_nothing() {
    let mut slots = [None; 4];
    let mut table = table(&mut slots);
    let held = table.claim(Span::frames(0x5000, 1).unwrap(), Owner::Tables).unwrap();

    let overlap = table.claim(Span::frames(0x4000, 3).unwrap(), Owner::Kernel);

    assert_eq!(overlap, Err(Error::Owned));
    assert_eq!(table.owner(0x4000), Ok(None), "the free frames stayed free");
    assert_eq!(table.owner(0x5000), Ok(Some(Owner::Tables)));
    table.release(held).unwrap();
}

#[test]
fn released_frames_return_to_pool() {
    let mut slots = [None; 4];
    let mut table = table(&mut slots);
    let span = Span::frames(0x4000, 2).unwrap();

    let frames = table.claim(span, Owner::Device(3)).unwrap();
    table.release(frames).unwrap();

    assert_eq!(table.claimed(), 0);
    assert!(table.claim(span, Owner::Cell(1)).is_ok(), "the span is claimable again");
}

#[test]
fn frames_from_another_table_rejected() {
    let mut mine = [None; 4];
    let mut theirs = [None; 4];
    let mut mine = table(&mut mine);
    let mut theirs = table(&mut theirs);
    let span = Span::frames(0x4000, 1).unwrap();

    let frames = theirs.claim(span, Owner::Kernel).unwrap();

    assert_eq!(mine.release(frames), Err(Error::NotOwner));
}

#[test]
fn claim_outside_table_rejected() {
    let mut slots = [None; 2];
    let mut table = table(&mut slots);

    assert_eq!(table.claim(Span::frames(0x9000, 1).unwrap(), Owner::Kernel), Err(Error::Range));
    assert_eq!(table.owner(0x9000), Err(Error::Range));
}

#[test]
fn table_smaller_than_span_rejected() {
    let mut slots = [None; 2];

    let short = FrameTable::over(Span::frames(0x4000, 3).unwrap(), &mut slots);
    assert!(short.is_err());
}

#[test]
fn spans_name_whole_frames() {
    assert_eq!(Span::new(0x4000, 0x4800), Err(Error::Misaligned));
    assert_eq!(Span::new(0x4000, 0x4000), Err(Error::Misaligned));
    assert_eq!(Span::new(0x5000, 0x4000), Err(Error::Misaligned));
    assert_eq!(Span::frames(0x4000, 2).unwrap().bytes(), 2 * FRAME_SIZE);
}

#[test]
fn we_rights_rejected() {
    assert_eq!(Rights::new(true, true, true), Err(MappingError::WritableExecutable));
    assert_eq!(Rights::new(false, true, false), Err(MappingError::Permissions));
    assert!(Rights::new(true, false, true).unwrap().is_execute());
}
