use molt_arch::refcount::{Error, Leaves, Run};
use molt_arch::va::{Class, Region};

/// Where the gigabyte arena of an Sv57 space starts, rounded to something a
/// test can read: any gigabyte-aligned address would do.
const BASE: u64 = 100 << 30;

const GIGA: u64 = Class::Giga.granule();
const MEGA: u64 = Class::Mega.granule();

fn region(start: u64, bytes: u64) -> Region {
    Region::new(start, start + bytes).expect("a non-empty region")
}

#[test]
fn a_hundred_gigabytes_cost_one_record() -> Result<(), Error> {
    let mut runs = [Run::EMPTY; 8];
    let mut leaves = Leaves::over(&mut runs);

    leaves.map(BASE, Class::Giga, 100)?;

    assert_eq!(leaves.runs(), 1, "the leaves that were mapped together were counted apart");
    assert_eq!(leaves.leaves(), 100);
    assert_eq!(leaves.frames(), 100 * GIGA / 4096, "the frames a per-frame table would have held");
    assert_eq!(leaves.count(BASE), Some(1));
    Ok(())
}

#[test]
fn sharing_the_whole_extent_stays_one_record() -> Result<(), Error> {
    let mut runs = [Run::EMPTY; 8];
    let mut leaves = Leaves::over(&mut runs);
    leaves.map(BASE, Class::Giga, 100)?;

    leaves.share(region(BASE, 100 * GIGA))?;

    assert_eq!(leaves.runs(), 1, "a grant of everything fragmented the accounting");
    assert_eq!(leaves.count(BASE), Some(2));
    assert_eq!(leaves.count(BASE + 99 * GIGA), Some(2));
    Ok(())
}

#[test]
fn a_grant_of_part_records_only_that_part() -> Result<(), Error> {
    let mut runs = [Run::EMPTY; 8];
    let mut leaves = Leaves::over(&mut runs);
    leaves.map(BASE, Class::Giga, 100)?;

    leaves.share(region(BASE + 10 * GIGA, 2 * GIGA))?;

    assert_eq!(leaves.runs(), 3, "the shared middle did not become its own record");
    assert_eq!(leaves.count(BASE + 9 * GIGA), Some(1));
    assert_eq!(leaves.count(BASE + 10 * GIGA), Some(2));
    assert_eq!(leaves.count(BASE + 11 * GIGA), Some(2));
    assert_eq!(leaves.count(BASE + 12 * GIGA), Some(1));
    assert_eq!(leaves.leaves(), 100, "splitting a record changed how many leaves exist");
    Ok(())
}

#[test]
fn two_megabytes_out_of_a_gigabyte_leaf_is_refused() -> Result<(), Error> {
    let mut runs = [Run::EMPTY; 8];
    let mut leaves = Leaves::over(&mut runs);
    leaves.map(BASE, Class::Giga, 1)?;

    let refused = leaves.share(region(BASE, 2 * MEGA));

    assert_eq!(refused, Err(Error::Straddle), "half a translation was accounted for");
    assert_eq!(leaves.count(BASE), Some(1), "the refused grant still changed a count");
    Ok(())
}

#[test]
fn splitting_a_leaf_keeps_everyone_who_held_it() -> Result<(), Error> {
    let mut runs = [Run::EMPTY; 8];
    let mut leaves = Leaves::over(&mut runs);
    leaves.map(BASE, Class::Giga, 100)?;
    leaves.share(region(BASE, 100 * GIGA))?;

    let child = leaves.split(BASE)?;

    assert_eq!(child, Class::Mega);
    assert_eq!(leaves.class(BASE), Some(Class::Mega));
    assert_eq!(leaves.class(BASE + GIGA), Some(Class::Giga), "the split reached the next leaf");
    assert_eq!(leaves.count(BASE), Some(2), "a view that held the gigabyte lost the megabytes");
    assert_eq!(leaves.count(BASE + 511 * MEGA), Some(2));
    assert_eq!(leaves.leaves(), 512 + 99, "the split did not conserve the addresses covered");
    Ok(())
}

#[test]
fn a_subrange_can_be_revoked_once_its_leaf_is_split() -> Result<(), Error> {
    let mut runs = [Run::EMPTY; 8];
    let mut leaves = Leaves::over(&mut runs);
    leaves.map(BASE, Class::Giga, 100)?;
    leaves.share(region(BASE, 100 * GIGA))?;
    leaves.split(BASE)?;

    let reclaimed = leaves.release(region(BASE, 2 * MEGA))?;

    assert!(reclaimed.is_empty(), "a leaf one view still holds was reported as free");
    assert_eq!(leaves.count(BASE), Some(1), "the revoked megabytes kept the second holder");
    assert_eq!(leaves.count(BASE + 2 * MEGA), Some(2), "the revoke reached past its range");
    assert_eq!(leaves.count(BASE + GIGA), Some(2));
    Ok(())
}

#[test]
fn the_last_holder_reports_what_it_freed() -> Result<(), Error> {
    let mut runs = [Run::EMPTY; 8];
    let mut leaves = Leaves::over(&mut runs);
    leaves.map(BASE, Class::Giga, 4)?;
    leaves.share(region(BASE, 2 * GIGA))?;

    let held = leaves.release(region(BASE, 4 * GIGA))?;
    let freed = leaves.release(region(BASE, 2 * GIGA))?;

    assert_eq!(held.leaves(), 2, "the leaves nobody held were not reported");
    assert_eq!(held.bytes(), 2 * GIGA);
    assert_eq!(freed.leaves(), 2);
    assert_eq!(leaves.runs(), 0, "a table nobody holds anything in kept records");
    assert_eq!(leaves.count(BASE), None);
    Ok(())
}

#[test]
fn merging_needs_a_whole_group_that_agrees() -> Result<(), Error> {
    let mut runs = [Run::EMPTY; 8];
    let mut leaves = Leaves::over(&mut runs);
    leaves.map(BASE, Class::Giga, 2)?;
    leaves.share(region(BASE, 2 * GIGA))?;
    leaves.split(BASE)?;
    leaves.release(region(BASE, 2 * MEGA))?;

    assert_eq!(leaves.merge(BASE), Err(Error::Uneven), "disagreeing leaves were merged anyway");

    leaves.share(region(BASE, 2 * MEGA))?;
    let parent = leaves.merge(BASE)?;

    assert_eq!(parent, Class::Giga);
    assert_eq!(leaves.class(BASE), Some(Class::Giga), "the group did not become one leaf again");
    assert_eq!(leaves.runs(), 1, "the merged leaf did not rejoin its neighbour");
    assert_eq!(leaves.leaves(), 2);
    Ok(())
}

#[test]
fn a_leaf_cannot_be_counted_twice() -> Result<(), Error> {
    let mut runs = [Run::EMPTY; 8];
    let mut leaves = Leaves::over(&mut runs);
    leaves.map(BASE, Class::Giga, 2)?;

    assert_eq!(leaves.map(BASE + GIGA, Class::Giga, 1), Err(Error::Overlap));
    assert_eq!(leaves.map(BASE, Class::Mega, 1), Err(Error::Overlap));
    Ok(())
}

#[test]
fn counting_an_address_nobody_mapped_is_refused() {
    let mut runs = [Run::EMPTY; 8];
    let mut leaves = Leaves::over(&mut runs);

    assert_eq!(leaves.share(region(BASE, GIGA)), Err(Error::Untracked));
    assert_eq!(leaves.split(BASE), Err(Error::Untracked));
    assert_eq!(leaves.count(BASE), None);
}

#[test]
fn a_page_leaf_has_nothing_left_to_split_into() -> Result<(), Error> {
    let mut runs = [Run::EMPTY; 8];
    let mut leaves = Leaves::over(&mut runs);
    leaves.map(BASE, Class::Page, 1)?;

    assert_eq!(leaves.split(BASE), Err(Error::Granule));
    assert_eq!(leaves.merge(BASE + GIGA), Err(Error::Untracked));
    Ok(())
}

#[test]
fn a_hole_in_the_range_is_not_silently_skipped() -> Result<(), Error> {
    let mut runs = [Run::EMPTY; 8];
    let mut leaves = Leaves::over(&mut runs);
    leaves.map(BASE, Class::Giga, 1)?;
    leaves.map(BASE + 2 * GIGA, Class::Giga, 1)?;

    assert_eq!(leaves.share(region(BASE, 3 * GIGA)), Err(Error::Untracked));
    assert_eq!(leaves.count(BASE), Some(1), "the refused grant counted the part it could reach");
    Ok(())
}

#[test]
fn the_table_refuses_to_run_out_of_records_silently() -> Result<(), Error> {
    let mut runs = [Run::EMPTY; 2];
    let mut leaves = Leaves::over(&mut runs);
    leaves.map(BASE, Class::Giga, 8)?;

    assert_eq!(leaves.share(region(BASE + GIGA, GIGA)), Err(Error::Storage));
    assert_eq!(leaves.leaves(), 8, "the failed grant lost leaves");
    Ok(())
}
