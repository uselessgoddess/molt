//! What churn nobody chose is allowed to do to the counts.
//!
//! Grant, revoke, split and merge in whatever order proptest asks for, against
//! a model that knows only which bytes are held how many times. The model has
//! no classes and no records, so anything the table does with either — a split
//! that loses a count, a refusal that spends a slot, a share that stops halfway
//! — shows up as the two disagreeing.

mod churn;

use churn::{MOVES, Seen, class, sweep};
use molt_arch::refcount::{Leaves, Run};
use molt_arch::va::{Class, Region};
use proptest::prelude::*;
use proptest::sample::Index;
use proptest::test_runner::TestCaseResult;

/// Four gigabyte leaves' worth of addresses: enough for the whole ladder, and
/// small enough that requests meet each other.
const WINDOW: u64 = 4 << 30;
const RUNS: usize = 64;

/// A range of one to four leaves of one class, which the table may never have
/// heard of.
#[derive(Clone, Copy, Debug)]
struct Fresh {
    class: Class,
    leaf: u64,
    leaves: u64,
}

impl Fresh {
    fn region(self) -> Region {
        let start = self.leaf % (WINDOW / self.class.granule()) * self.class.granule();
        Region::new(start, start + self.leaves * self.class.granule())
            .expect("a range of at least one leaf")
    }
}

fn fresh() -> impl Strategy<Value = Fresh> {
    (class(), any::<u64>(), 1u64..=4).prop_map(|(class, leaf, leaves)| Fresh {
        class,
        leaf,
        leaves,
    })
}

/// Where a request points.
///
/// Half of them name a range the table already has a record for, because a
/// range nobody mapped only ever proves refusals. The other half is generated
/// outright, and so is the fallback: a live record is only there if an earlier
/// move made one.
#[derive(Clone, Copy, Debug)]
struct Aim {
    live: Option<Index>,
    fresh: Fresh,
}

impl Aim {
    fn region(self, leaves: &Leaves<'_>) -> Region {
        let mapped: Vec<Region> = leaves.iter().filter_map(Run::region).collect();
        match self.live.filter(|_| !mapped.is_empty()) {
            Some(index) => *index.get(&mapped),
            None => self.fresh.region(),
        }
    }
}

fn aim() -> impl Strategy<Value = Aim> {
    (proptest::option::of(any::<Index>()), fresh()).prop_map(|(live, fresh)| Aim { live, fresh })
}

/// One request against the table.
#[derive(Clone, Copy, Debug)]
enum Move {
    /// A grant of leaves nobody holds yet.
    Map {
        aim: Aim,
        class: Class,
        leaves: u64,
    },
    /// A second holder for a range, whoever holds it now.
    Share(Aim),
    /// One holder gone, which frees the leaves the last holder gives back.
    Release(Aim),
    /// A leaf broken into the class below, and the class below joined back up.
    Split(Aim),
    Merge(Aim),
}

fn moves() -> impl Strategy<Value = Vec<Move>> {
    let request = prop_oneof![
        (aim(), class(), 1u64..=4).prop_map(|(aim, class, leaves)| Move::Map {
            aim,
            class,
            leaves
        }),
        aim().prop_map(Move::Share),
        aim().prop_map(Move::Release),
        aim().prop_map(Move::Split),
        aim().prop_map(Move::Merge),
    ];
    prop::collection::vec(request, MOVES)
}

/// Which bytes are held, and how many times. No classes, no records: two
/// neighbouring stretches with one count are one fact.
#[derive(Debug, Default, Eq, PartialEq)]
struct Model(Vec<(u64, u64, u32)>);

impl Model {
    fn insert(&mut self, region: Region) {
        self.0.push((region.start(), region.end(), 1));
        self.0.sort_unstable();
        self.join();
    }

    /// Adds `delta` to every byte of `region`, forgetting what reaches zero.
    fn shift(&mut self, region: Region, delta: i32) {
        let (start, end) = (region.start(), region.end());
        self.0 = self
            .0
            .iter()
            .flat_map(|&(from, to, count)| {
                [
                    (from, to.min(start), count),
                    (from.max(start), to.min(end), count.saturating_add_signed(delta)),
                    (from.max(end), to, count),
                ]
            })
            .filter(|&(from, to, count)| from < to && count > 0)
            .collect();
        self.join();
    }

    /// Bytes of `region` that one more revoke would free.
    fn last(&self, region: Region) -> u64 {
        self.0
            .iter()
            .filter(|&&(_, _, count)| count == 1)
            .map(|&(from, to, _)| to.min(region.end()).saturating_sub(from.max(region.start())))
            .sum()
    }

    fn join(&mut self) {
        self.0.dedup_by(|next, last| {
            let joins = last.1 == next.0 && last.2 == next.2;
            if joins {
                last.1 = next.1;
            }
            joins
        });
    }
}

/// The table said the same way, so the two can be compared as lists.
fn model(leaves: &Leaves<'_>) -> Model {
    let mut flat = Model::default();
    for run in leaves.iter() {
        let region = run.region().expect("a record in use covers addresses");
        flat.0.push((region.start(), region.end(), run.count()));
    }
    flat.join();
    flat
}

/// What every record has to be true of, whatever a request did or was refused.
fn canonical(leaves: &Leaves<'_>) -> TestCaseResult {
    let records: Vec<Run> = leaves.iter().collect();
    for run in &records {
        let region = run.region().expect("a record in use covers addresses");
        prop_assert!(run.count() > 0, "a record nobody holds was kept");
        prop_assert_eq!(
            region.start() % run.class().granule(),
            0,
            "a record left its leaf boundary"
        );
    }
    for pair in records.windows(2) {
        let (run, next) = (pair[0], pair[1]);
        let end = run.region().expect("a record in use").end();
        prop_assert!(end <= next.region().expect("a record in use").start(), "records overlap");
        let twice = end == next.region().expect("a record in use").start()
            && run.class() == next.class()
            && run.count() == next.count();
        prop_assert!(!twice, "one fact in two records: {:?} and {:?}", run, next);
    }
    Ok(())
}

#[test]
fn requests_leave_counts_model_expects() {
    let seen = Seen::default();

    sweep(moves(), |moves| {
        let mut runs = [Run::EMPTY; RUNS];
        let mut leaves = Leaves::over(&mut runs);
        let mut expected = Model::default();

        for request in moves {
            match request {
                Move::Map { aim, class, leaves: count } => {
                    let region = aim.region(&leaves);
                    let start = region.start() - region.start() % class.granule();
                    if leaves.map(start, class, count).is_ok() {
                        let mapped = Region::new(start, start + count * class.granule());
                        expected.insert(mapped.expect("a range of at least one leaf"));
                        seen.saw("a grant");
                    }
                }
                Move::Share(aim) => {
                    let region = aim.region(&leaves);
                    if leaves.share(region).is_ok() {
                        expected.shift(region, 1);
                        seen.saw("a second holder");
                    }
                }
                Move::Release(aim) => {
                    let region = aim.region(&leaves);
                    let freed = expected.last(region);
                    if let Ok(reclaimed) = leaves.release(region) {
                        prop_assert_eq!(reclaimed.bytes(), freed, "the wrong bytes came free");
                        expected.shift(region, -1);
                        seen.saw("a holder gone");
                    }
                }
                Move::Split(aim) => {
                    if leaves.split(aim.region(&leaves).start()).is_ok() {
                        seen.saw("a leaf broken up");
                    }
                }
                Move::Merge(aim) => {
                    if leaves.merge(aim.region(&leaves).start()).is_ok() {
                        seen.saw("leaves joined back up");
                    }
                }
            }

            canonical(&leaves)?;
            prop_assert_eq!(&model(&leaves), &expected, "the table and the model disagree");
        }
        Ok(())
    });

    seen.reached(&[
        "a grant",
        "a second holder",
        "a holder gone",
        "a leaf broken up",
        "leaves joined back up",
    ]);
}
