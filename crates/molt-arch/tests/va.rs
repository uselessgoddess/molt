use molt_arch::va::{Class, Error, Hole, Space};

/// The width QEMU's `virt` hart reports, and the one the smoke runs at.
const SV57: u32 = 57;

fn space(holes: &mut [Hole]) -> Space<'_> {
    Space::over(SV57, holes).expect("a fifty-seven bit space cuts into class arenas")
}

#[test]
fn every_class_hands_out_its_own_alignment() -> Result<(), Error> {
    let mut holes = [Hole::EMPTY; 12];
    let mut space = space(&mut holes);

    for class in Class::ALL {
        let extent = space.allocate(class, 1)?;

        assert_eq!(extent.start() % class.granule(), 0, "{class:?} extent was not aligned");
        assert_eq!(extent.bytes(), class.granule(), "a one-byte request took more than one leaf");
        assert_eq!(extent.leaves(), 1);
        space.release(extent)?;
    }
    Ok(())
}

#[test]
fn class_arenas_do_not_overlap() {
    let mut holes = [Hole::EMPTY; 12];
    let space = space(&mut holes);

    let [page, mega, giga] = Class::ALL.map(|class| space.arena(class));

    assert_eq!(page.end(), mega.start(), "a gap between arenas is address space nobody owns");
    assert_eq!(mega.end(), giga.start());
    assert!(giga.bytes() >= page.bytes() + mega.bytes(), "the largest extents got the least room");
    assert_eq!(giga.start() % Class::Giga.granule(), 0, "the gigabyte arena is misaligned");
}

#[test]
fn a_hundred_gigabyte_mapping_fits_in_one_extent() -> Result<(), Error> {
    let mut holes = [Hole::EMPTY; 12];
    let mut space = space(&mut holes);
    let log = 100 * (1 << 30);

    let extent = space.allocate(Class::Giga, log)?;

    assert_eq!(extent.bytes(), log);
    assert_eq!(extent.leaves(), 100, "a hundred gigabyte leaves, not twenty-six million pages");
    space.release(extent)?;
    Ok(())
}

#[test]
fn a_size_that_is_not_whole_leaves_rounds_up() -> Result<(), Error> {
    let mut holes = [Hole::EMPTY; 12];
    let mut space = space(&mut holes);

    let extent = space.allocate(Class::Mega, Class::Mega.granule() + 1)?;

    assert_eq!(extent.leaves(), 2, "a byte past a leaf did not take the next one");
    space.release(extent)?;
    Ok(())
}

#[test]
fn released_addresses_wait_for_the_shootdown() -> Result<(), Error> {
    let mut holes = [Hole::EMPTY; 12];
    let mut space = space(&mut holes);
    let first = space.allocate(Class::Giga, 1)?;
    let start = first.start();
    let taken = space.free(Class::Giga);

    space.release(first)?;

    assert_eq!(space.free(Class::Giga), taken, "a freed address was reusable before any flush");
    assert_eq!(space.quarantined(Class::Giga), Class::Giga.granule());
    let next = space.allocate(Class::Giga, 1)?;
    assert_ne!(next.start(), start, "the address came back while a stale entry could exist");
    space.release(next)?;
    Ok(())
}

#[test]
fn retiring_the_swept_epoch_returns_the_addresses() -> Result<(), Error> {
    let mut holes = [Hole::EMPTY; 12];
    let mut space = space(&mut holes);
    let first = space.allocate(Class::Giga, 1)?;
    let start = first.start();
    space.release(first)?;

    let flushed = space.sweep();
    space.retire(flushed);

    assert_eq!(space.quarantined(Class::Giga), 0, "a retired epoch left addresses held back");
    let again = space.allocate(Class::Giga, 1)?;
    assert_eq!(again.start(), start, "the flushed address did not come back");
    space.release(again)?;
    Ok(())
}

#[test]
fn an_epoch_that_was_never_swept_frees_nothing() -> Result<(), Error> {
    let mut holes = [Hole::EMPTY; 12];
    let mut space = space(&mut holes);
    let extent = space.allocate(Class::Mega, 1)?;
    space.release(extent)?;

    space.retire(space.open());

    assert_eq!(space.quarantined(Class::Mega), Class::Mega.granule(), "the open batch was freed");
    Ok(())
}

#[test]
fn churn_leaves_no_permanent_fragmentation() -> Result<(), Error> {
    let mut holes = [Hole::EMPTY; 48];
    let mut space = space(&mut holes);
    let whole = space.free(Class::Mega);

    // Grant and revoke in a different order than they were taken, which is what
    // a long-lived domain handing out ring buffers actually does.
    for round in 0..8 {
        let mut held = [const { None }; 6];
        for (index, slot) in held.iter_mut().enumerate() {
            *slot = Some(space.allocate(Class::Mega, (index as u64 + 1) * Class::Mega.granule())?);
        }
        for index in [1, 4, 0, 5, 2, 3] {
            let extent = held[index].take().expect("an extent this round handed out");
            space.release(extent)?;
        }
        let flushed = space.sweep();
        space.retire(flushed);
        assert_eq!(space.free(Class::Mega), whole, "round {round} lost address space");
    }

    assert_eq!(space.holes(Class::Mega), 1, "coalescing left the arena in pieces");
    assert_eq!(space.largest(Class::Mega), whole, "the arena did not come back whole");
    Ok(())
}

#[test]
fn an_exhausted_class_does_not_borrow_from_another() -> Result<(), Error> {
    let mut holes = [Hole::EMPTY; 12];
    let mut space = space(&mut holes);
    let whole = space.free(Class::Giga);

    let all = space.allocate(Class::Giga, whole)?;

    assert_eq!(space.allocate(Class::Giga, 1), Err(Error::Exhausted));
    assert!(space.free(Class::Mega) > 0, "an exhausted class emptied its neighbour");
    let elsewhere = space.allocate(Class::Mega, 1)?;
    assert!(!space.arena(Class::Giga).contains(elsewhere.start()), "a class crossed into another");
    space.release(elsewhere)?;
    space.release(all)?;
    Ok(())
}

#[test]
fn a_full_free_list_refuses_rather_than_loses_the_range() -> Result<(), Error> {
    // Three slots per class: the range above what is taken, and two islands.
    let mut holes = [Hole::EMPTY; 9];
    let mut space = space(&mut holes);
    let mut taken: [_; 6] = core::array::from_fn(|_| {
        Some(space.allocate(Class::Page, Class::Page.granule()).expect("a free page extent"))
    });
    let mut give = |space: &mut Space<'_>, index: usize| {
        space.release(taken[index].take().expect("an extent this test took"))
    };
    // Freeing every other extent makes each freed range an island of its own.
    give(&mut space, 0)?;
    give(&mut space, 2)?;
    let free = space.free(Class::Page);

    let refused = space.release(taken[4].take().expect("the fifth extent"));

    assert_eq!(refused, Err(Error::Full), "an island went in with no slot to record it");
    assert_eq!(space.free(Class::Page), free, "the refused release changed the free list");
    assert_eq!(space.holes(Class::Page), 3);
    Ok(())
}

#[test]
fn a_range_that_is_already_free_is_refused() -> Result<(), Error> {
    let mut mine = [Hole::EMPTY; 12];
    let mut theirs = [Hole::EMPTY; 12];
    let mut mine = space(&mut mine);
    let mut theirs = space(&mut theirs);

    // Two spaces of the same width lay their arenas out identically, so an
    // extent from one names a range the other still holds free. Nothing but a
    // released extent can name such a range, which is why the check is here.
    let extent = mine.allocate(Class::Page, 2 * Class::Page.granule())?;

    assert_eq!(theirs.release(extent), Err(Error::Overlap), "a live range was marked free");
    Ok(())
}

#[test]
fn a_space_too_narrow_to_cut_is_refused() {
    let mut holes = [Hole::EMPTY; 12];

    assert_eq!(Space::over(34, &mut holes).err(), Some(Error::Width));
    assert!(Space::over(39, &mut holes).is_ok(), "an Sv39 hart still gets an address space");
}

#[test]
fn a_space_without_a_slot_per_class_is_refused() {
    let mut holes = [Hole::EMPTY; 2];

    assert_eq!(Space::over(SV57, &mut holes).err(), Some(Error::Storage));
}

#[test]
fn the_narrowest_mode_clears_the_device_window() {
    // `paging::DEVICE_REGION` on RISC-V and the gigabyte it spans, which the
    // kernel's own tables own on every mode.
    const DEVICE_REGION_END: u64 = 0x20_0000_0000 + (1 << 30);

    let bounds = Space::bounds(39).expect("Sv39 is wide enough to cut");

    assert!(bounds.start() >= DEVICE_REGION_END, "the handed-out range ran into device windows");
    assert_eq!((bounds.start(), bounds.end()), (192 << 30, 256 << 30));
    assert_eq!(bounds.end(), 1 << 38, "Sv39's lower canonical half is 2^38 wide");
}

#[test]
fn zero_bytes_name_no_page() {
    let mut holes = [Hole::EMPTY; 12];
    let mut space = space(&mut holes);

    assert_eq!(space.allocate(Class::Page, 0), Err(Error::Empty));
}
