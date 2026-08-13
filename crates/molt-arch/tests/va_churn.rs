//! What churn nobody chose is allowed to do to the address space.
//!
//! The example tests next door take turns the author thought of. These take
//! turns the author did not: allocate, release, sweep and retire in whatever
//! order proptest asks for, against a model that remembers every address handed
//! out and every address still waiting on a flush.

mod churn;

use churn::{MOVES, Seen, class, sweep};
use molt_arch::va::{Class, Epoch, Extent, Hole, Region, Space};
use proptest::prelude::*;
use proptest::sample::Index;

/// Narrow on purpose: Sv57's arenas are petabytes, and an allocator that never
/// runs out proves nothing about what it does when it does.
const BITS: u32 = 35;
/// Eight free ranges per class, which a few dozen extents can fill.
const HOLES: usize = 3 * 8;

/// One turn against the space.
#[derive(Clone, Copy, Debug)]
enum Move {
    /// One to four leaves of a class, which the arena may or may not have.
    Allocate { class: Class, leaves: u64 },
    /// Whichever extent is live at this position, if the space holds any.
    Release(Index),
    /// Close the open epoch, as a hart telling the others what to flush.
    Sweep,
    /// Retire the last epoch swept, as the last hart answering.
    Retire,
}

fn moves() -> impl Strategy<Value = Vec<Move>> {
    let turn = prop_oneof![
        4 => (class(), 1u64..=4).prop_map(|(class, leaves)| Move::Allocate { class, leaves }),
        2 => any::<Index>().prop_map(Move::Release),
        1 => Just(Move::Sweep),
        1 => Just(Move::Retire),
    ];
    prop::collection::vec(turn, MOVES)
}

fn overlap(one: Region, other: Region) -> bool {
    one.start() < other.end() && other.start() < one.end()
}

fn live(held: &[Extent], class: Class) -> u64 {
    held.iter().filter(|extent| extent.class() == class).map(Extent::bytes).sum()
}

fn space(holes: &mut [Hole]) -> Space<'_> {
    Space::over(BITS, holes).expect("a space this wide cuts into arenas")
}

#[test]
fn churn_hands_out_no_address_twice() {
    let seen = Seen::default();

    sweep(moves(), |moves| {
        let mut holes = [Hole::EMPTY; HOLES];
        let mut space = space(&mut holes);
        let mut held: Vec<Extent> = Vec::new();
        let mut waiting: Vec<(Region, Epoch)> = Vec::new();
        let mut swept = Epoch::FIRST;

        for turn in moves {
            match turn {
                Move::Allocate { class, leaves } => {
                    match space.allocate(class, leaves * class.granule()) {
                        Ok(extent) => {
                            let region = extent.region();
                            prop_assert_eq!(
                                region.start() % class.granule(),
                                0,
                                "{:?} came out misaligned",
                                class
                            );
                            prop_assert!(
                                space.arena(class).covers(region),
                                "an extent left its class arena"
                            );
                            prop_assert!(
                                held.iter().all(|other| !overlap(other.region(), region)),
                                "two live extents were handed the same address"
                            );
                            prop_assert!(
                                waiting.iter().all(|&(held, _)| !overlap(held, region)),
                                "an address came back before every hart had flushed it"
                            );
                            held.push(extent);
                            seen.saw("an extent handed out");
                        }
                        Err(_) => seen.saw("an allocation the arena had no room for"),
                    }
                }
                Move::Release(which) if !held.is_empty() => {
                    let extent = held.remove(which.index(held.len()));
                    let (region, open) = (extent.region(), space.open());
                    match space.release(extent) {
                        Ok(()) => waiting.push((region, open)),
                        Err((_, extent)) => {
                            held.push(extent);
                            seen.saw("a release the free list had no slot for");
                        }
                    }
                }
                Move::Release(_) => {}
                Move::Sweep => swept = space.sweep(),
                Move::Retire => {
                    space.retire(swept);
                    waiting.retain(|&(_, ready)| ready > space.retired());
                }
            }

            for class in Class::ALL {
                prop_assert_eq!(
                    space.free(class) + space.quarantined(class) + live(&held, class),
                    space.arena(class).bytes(),
                    "{:?} lost address space",
                    class
                );
            }
        }
        Ok(())
    });

    // An arena that never runs out and a free list that never fills prove
    // nothing about what either does when it happens, so the sweep has to have
    // seen both happen somewhere.
    seen.reached(&[
        "an extent handed out",
        "an allocation the arena had no room for",
        "a release the free list had no slot for",
    ]);
}

#[test]
fn churn_gives_every_arena_back_whole() {
    sweep(moves(), |moves| {
        let mut holes = [Hole::EMPTY; HOLES];
        let mut space = space(&mut holes);
        let mut held: Vec<Extent> = Vec::new();

        for turn in moves {
            match turn {
                Move::Allocate { class, leaves } => {
                    if let Ok(extent) = space.allocate(class, leaves * class.granule()) {
                        held.push(extent);
                    }
                }
                Move::Release(which) if !held.is_empty() => {
                    let extent = held.remove(which.index(held.len()));
                    if let Err((_, extent)) = space.release(extent) {
                        held.push(extent);
                    }
                }
                Move::Release(_) => {}
                Move::Sweep | Move::Retire => {
                    let flushed = space.sweep();
                    space.retire(flushed);
                }
            }
        }

        // A refusal is a full free list, and a flush is what empties one, so the
        // drain retries rather than giving up on the addresses.
        held.sort_by_key(Extent::start);
        for _ in 0..HOLES {
            held =
                held.into_iter().filter_map(|e| space.release(e).err().map(|(_, e)| e)).collect();
            let flushed = space.sweep();
            space.retire(flushed);
        }

        prop_assert!(held.is_empty(), "{} extents the space would not take back", held.len());
        for class in Class::ALL {
            prop_assert_eq!(space.holes(class), 1, "churn left {:?} in pieces", class);
            prop_assert_eq!(
                space.largest(class),
                space.arena(class).bytes(),
                "{:?} came back short",
                class
            );
        }
        Ok(())
    });
}
