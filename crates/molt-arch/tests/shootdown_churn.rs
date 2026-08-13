//! Whether a shootdown can be left in a state nobody can get out of.
//!
//! A tracker that refuses the wrong thing does not crash: it stops, and the
//! addresses it was holding are never handed out again. So the claim these
//! sweeps make is liveness, and they make it the only honest way — from every
//! state proptest can churn the protocol into, drive it forward and see that it
//! goes. `Shootdown` is `Copy`, so the escape runs on a copy and the churn
//! carries on from where it was.

mod churn;

use churn::{MOVES, Seen, class, sweep};
use molt_arch::CpuId;
use molt_arch::shootdown::{Error, Shootdown};
use molt_arch::va::{Class, Epoch, Extent, Hole, Region, Space};
use proptest::prelude::*;
use proptest::sample::Index;

/// Cores in the machine. More than the smoke's four, so the mask is not the
/// only thing a round could be waiting on.
const CORES: u16 = 6;
/// Narrow on purpose, as in the allocator's own sweep: an arena that never runs
/// out proves nothing about an address held back.
const BITS: u32 = 35;
const HOLES: usize = 3 * 8;

/// The order an escape answers in: one position per swap of a shuffle over the
/// cores a round is still waiting on.
type Order = [Index; CORES as usize];

/// One turn against the tracker.
#[derive(Clone, Copy, Debug)]
enum Move {
    /// A round for a mask of cores. `wide` asks for one no acknowledgement mask
    /// could name, and `stale` for an epoch already retired: both are rounds
    /// that must not open, and both are reached by way of the checks that come
    /// first.
    Begin { mask: u64, wide: bool, stale: bool },
    /// A core answering, including ones that already answered, ones the round
    /// never asked, and one the machine does not have.
    Answer(CpuId),
    /// The way out of wherever the churn is: closed by the cores still owing a
    /// flush, or opened again when nothing is outstanding.
    Escape(Order),
}

fn cpu() -> impl Strategy<Value = CpuId> {
    prop_oneof![
        15 => (0..CORES + 2).prop_map(CpuId::new),
        1 => Just(CpuId::new(Shootdown::LIMIT as u16)),
    ]
}

fn order() -> impl Strategy<Value = Order> {
    prop::array::uniform6(any::<Index>())
}

fn moves() -> impl Strategy<Value = Vec<Move>> {
    let turn = prop_oneof![
        1 => (0..1u64 << CORES, prop::bool::weighted(0.125), prop::bool::weighted(0.25))
            .prop_map(|(mask, wide, stale)| Move::Begin { mask, wide, stale }),
        2 => cpu().prop_map(Move::Answer),
        1 => order().prop_map(Move::Escape),
    ];
    prop::collection::vec(turn, MOVES)
}

fn cores() -> impl Iterator<Item = CpuId> {
    (0..CORES).map(CpuId::new)
}

/// Answers for every core the open round is still waiting on, in the order the
/// churn asks for, and returns the epoch that closed it.
fn finish(shootdown: &mut Shootdown, order: &[Index]) -> Result<Option<Epoch>, TestCaseError> {
    let mut owing: Vec<CpuId> = cores().filter(|&cpu| shootdown.pending(cpu)).collect();
    for (index, at) in (1..owing.len()).rev().zip(order) {
        owing.swap(index, at.index(index + 1));
    }

    let mut closed = None;
    for cpu in owing {
        prop_assert_eq!(closed, None, "the round closed with cores still owing a flush");
        closed = shootdown.acknowledge(cpu).expect("a core the open round asked");
    }
    Ok(closed)
}

/// Which refusal, to count it as one the sweep got to.
fn refusal(error: Error) -> &'static str {
    match error {
        Error::Width => "a core no mask can name",
        Error::Empty => "a round nobody was asked for",
        Error::Open => "a round on top of an open one",
        Error::Closed => "an answer with no round open",
        Error::Stale => "an epoch already retired",
        Error::Foreign => "an answer from a core the round never asked",
    }
}

#[test]
fn no_run_of_answers_leaves_a_round_nobody_can_close() {
    let seen = Seen::default();

    sweep(moves(), |moves| {
        let mut shootdown = Shootdown::new();
        // The round as the test has it: who was asked, and who the tracker has
        // accepted an answer from since.
        let mut asked = 0u64;
        let mut answered = 0u64;
        let mut epoch = Epoch::FIRST;

        for turn in moves {
            let before = shootdown;
            match turn {
                Move::Begin { mask, wide, stale } => {
                    let wanted = if stale { shootdown.retired() } else { epoch.next() };
                    let asking = cores()
                        .filter(|cpu| mask >> cpu.index() & 1 == 1)
                        .chain(wide.then(|| CpuId::new(Shootdown::LIMIT as u16)));

                    match shootdown.begin(wanted, asking) {
                        Ok(count) => {
                            prop_assert_eq!(
                                count,
                                mask.count_ones(),
                                "a round waits on cores it never asked"
                            );
                            prop_assert_eq!(shootdown.epoch(), Some(wanted));
                            prop_assert_eq!(
                                shootdown.outstanding(),
                                count,
                                "a round opened part flushed"
                            );
                            (asked, answered, epoch) = (mask, 0, wanted);
                            seen.saw("a round opened");
                        }
                        Err(error) => {
                            prop_assert_eq!(shootdown, before, "a refused round moved the tracker");
                            seen.saw(refusal(error));
                        }
                    }
                }
                Move::Answer(cpu) => {
                    let owed = shootdown.pending(cpu);

                    match shootdown.acknowledge(cpu) {
                        Ok(closed) => {
                            prop_assert!(
                                asked & 1 << cpu.index() != 0,
                                "a foreign answer was accepted"
                            );
                            prop_assert_eq!(
                                shootdown.outstanding(),
                                before.outstanding() - u32::from(owed),
                                "an answer already given was counted again"
                            );
                            answered |= 1 << cpu.index();
                            match closed {
                                Some(retired) => {
                                    prop_assert_eq!(
                                        retired,
                                        epoch,
                                        "a round retired an epoch of its own"
                                    );
                                    prop_assert_eq!(
                                        answered,
                                        asked,
                                        "a core that never answered was waived"
                                    );
                                    prop_assert_eq!(shootdown.retired(), retired);
                                    prop_assert_eq!(shootdown.rounds(), before.rounds() + 1);
                                    prop_assert_eq!(
                                        shootdown.epoch(),
                                        None,
                                        "a closed round stayed open"
                                    );
                                    seen.saw("a round closed");
                                }
                                None => {
                                    prop_assert!(
                                        shootdown.outstanding() > 0,
                                        "a round nobody owes is open"
                                    );
                                    seen.saw("a flush answered");
                                }
                            }
                        }
                        Err(error) => {
                            prop_assert_eq!(
                                shootdown,
                                before,
                                "a refused answer moved the tracker"
                            );
                            seen.saw(refusal(error));
                        }
                    }
                }
                Move::Escape(order) => {
                    // Refusing either of these forever is the wedge this test
                    // is named after.
                    let mut copy = shootdown;
                    match copy.epoch() {
                        Some(open) => prop_assert_eq!(
                            finish(&mut copy, &order)?,
                            Some(open),
                            "a round with no way out"
                        ),
                        None => prop_assert_eq!(
                            copy.begin(copy.retired().next(), cores()),
                            Ok(u32::from(CORES)),
                            "a tracker holding nobody refused the next round"
                        ),
                    }
                }
            }
            prop_assert!(shootdown.retired() >= before.retired(), "the tracker unretired an epoch");
        }
        Ok(())
    });

    seen.reached(&[
        "a round opened",
        "a round closed",
        "a flush answered",
        "a core no mask can name",
        "a round nobody was asked for",
        "a round on top of an open one",
        "an answer with no round open",
        "an epoch already retired",
        "an answer from a core the round never asked",
    ]);
}

/// One turn against a space whose freed addresses wait on a round.
#[derive(Clone, Copy, Debug)]
enum Held {
    Allocate {
        class: Class,
        leaves: u64,
    },
    Release(Index),
    /// A sweep whose round cannot open yet is the interesting one: those
    /// addresses wait on some later round instead, and waiting forever is what
    /// this is watching for.
    Sweep,
    Answer(CpuId),
}

fn quarantine() -> impl Strategy<Value = Vec<Held>> {
    let turn = prop_oneof![
        4 => (class(), 1u64..=4).prop_map(|(class, leaves)| Held::Allocate { class, leaves }),
        2 => any::<Index>().prop_map(Held::Release),
        1 => Just(Held::Sweep),
        1 => cpu().prop_map(Held::Answer),
    ];
    prop::collection::vec(turn, MOVES)
}

#[test]
fn no_run_of_flushes_leaves_an_address_stuck_in_quarantine() {
    sweep((quarantine(), order()), |(moves, order)| {
        let mut holes = [Hole::EMPTY; HOLES];
        let mut space = Space::over(BITS, &mut holes).expect("a space this wide cuts into arenas");
        let mut shootdown = Shootdown::new();
        let mut held: Vec<Extent> = Vec::new();
        // Released, and not safe to hand out until this epoch has been retired.
        let mut waiting: Vec<(Region, Epoch)> = Vec::new();

        for turn in moves {
            match turn {
                Held::Allocate { class, leaves } => {
                    if let Ok(extent) = space.allocate(class, leaves * class.granule()) {
                        let region = extent.region();
                        prop_assert!(
                            waiting.iter().all(|&(held, _)| {
                                held.start() >= region.end() || region.start() >= held.end()
                            }),
                            "an address came back before its round closed"
                        );
                        held.push(extent);
                    }
                }
                Held::Release(which) if !held.is_empty() => {
                    let extent = held.remove(which.index(held.len()));
                    let (region, open) = (extent.region(), space.open());
                    match space.release(extent) {
                        Ok(()) => waiting.push((region, open)),
                        Err((_, extent)) => held.push(extent),
                    }
                }
                Held::Release(_) => {}
                Held::Sweep => {
                    let epoch = space.sweep();
                    let _ = shootdown.begin(epoch, cores());
                }
                Held::Answer(cpu) => {
                    if let Ok(Some(retired)) = shootdown.acknowledge(cpu) {
                        space.retire(retired);
                        waiting.retain(|&(_, epoch)| epoch > space.retired());
                    }
                }
            }
        }

        // The way out of whatever the churn left: give back what is still held,
        // close the round that is open, and open one for everything swept since.
        // A release refused for want of a free slot is retried once the flushes
        // it is waiting behind have joined its neighbours back up.
        for _ in 0..HOLES {
            held =
                held.into_iter().filter_map(|e| space.release(e).err().map(|(_, e)| e)).collect();
            if let Some(epoch) = finish(&mut shootdown, &order)? {
                space.retire(epoch);
            }
            let epoch = space.sweep();
            shootdown.begin(epoch, cores()).expect("a round for the epoch just swept");
            space.retire(finish(&mut shootdown, &order)?.expect("every core answered"));
        }

        prop_assert!(held.is_empty(), "{} extents the space would not take back", held.len());
        for class in Class::ALL {
            prop_assert_eq!(
                space.quarantined(class),
                0,
                "{:?} left addresses waiting on nothing",
                class
            );
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
