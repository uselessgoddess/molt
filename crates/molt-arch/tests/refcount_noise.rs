//! What churn nobody chose is allowed to do to the counts.
//!
//! Grant, revoke, split and merge in whatever order a seeded xorshift asks
//! for, against a model that knows only which bytes are held how many times.
//! The model has no classes and no records, so anything the table does with
//! either — a split that loses a count, a refusal that spends a slot, a share
//! that stops halfway — shows up as the two disagreeing.

use molt_arch::refcount::{Leaves, Run};
use molt_arch::va::{Class, Region};

const ROUNDS: usize = 1 << 12;
const SEED: u64 = 0x6d6f_6c74_7265_6600;
/// Four gigabyte leaves' worth of addresses: enough for the whole ladder, and
/// small enough that requests meet each other.
const WINDOW: u64 = 4 << 30;
const RUNS: usize = 64;

struct Noise(u64);

impl Noise {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn class(&mut self) -> Class {
        Class::ALL[self.next() as usize % Class::ALL.len()]
    }

    /// An address in the window, on a boundary some class can start a leaf on.
    fn address(&mut self, class: Class) -> u64 {
        (self.next() % (WINDOW / class.granule())) * class.granule()
    }

    /// A range of one to four leaves of one class, which may or may not be
    /// anything the table has heard of.
    fn region(&mut self) -> Region {
        let class = self.class();
        let start = self.address(class);
        let bytes = (1 + self.next() % 4) * class.granule();
        Region::new(start, start + bytes).expect("a range of at least one leaf")
    }
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
fn canonical(leaves: &Leaves<'_>) {
    let records: Vec<Run> = leaves.iter().collect();
    for run in &records {
        let region = run.region().expect("a record in use covers addresses");
        assert!(run.count() > 0, "a record nobody holds was kept");
        assert_eq!(region.start() % run.class().granule(), 0, "a record left its leaf boundary");
    }
    for pair in records.windows(2) {
        let (run, next) = (pair[0], pair[1]);
        let end = run.region().expect("a record in use").end();
        assert!(end <= next.region().expect("a record in use").start(), "records overlap");
        let twice = end == next.region().expect("a record in use").start()
            && run.class() == next.class()
            && run.count() == next.count();
        assert!(!twice, "one fact in two records: {run:?} and {next:?}");
    }
}

#[test]
fn every_request_leaves_the_counts_the_model_expects() {
    let mut noise = Noise(SEED);
    let mut runs = [Run::EMPTY; RUNS];
    let mut leaves = Leaves::over(&mut runs);
    let mut expected = Model::default();
    let mut done = [0; 5];

    for _ in 0..ROUNDS {
        let region = match noise.next() % 4 {
            // Half the requests name a range the table already has a record
            // for, because a range nobody mapped only ever proves refusals.
            0 | 1 => leaves.iter().nth(noise.next() as usize % RUNS).and_then(Run::region),
            _ => None,
        }
        .unwrap_or_else(|| noise.region());

        match noise.next() % 5 {
            0 => {
                let class = noise.class();
                let start = region.start() - region.start() % class.granule();
                let count = 1 + noise.next() % 4;
                if leaves.map(start, class, count).is_ok() {
                    let mapped = Region::new(start, start + count * class.granule());
                    expected.insert(mapped.expect("a range of at least one leaf"));
                    done[0] += 1;
                }
            }
            1 => {
                if leaves.share(region).is_ok() {
                    expected.shift(region, 1);
                    done[1] += 1;
                }
            }
            2 => {
                let freed = expected.last(region);
                if let Ok(reclaimed) = leaves.release(region) {
                    assert_eq!(reclaimed.bytes(), freed, "the wrong bytes came free");
                    expected.shift(region, -1);
                    done[2] += 1;
                }
            }
            3 => done[3] += i32::from(leaves.split(region.start()).is_ok()),
            _ => done[4] += i32::from(leaves.merge(region.start()).is_ok()),
        }

        canonical(&leaves);
        assert_eq!(model(&leaves), expected, "the table and the model disagree");
    }

    assert!(done.iter().all(|&count| count > 0), "the sweep never got to something: {done:?}");
}
