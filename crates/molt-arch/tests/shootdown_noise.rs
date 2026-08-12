//! Whether a shootdown can be left in a state nobody can get out of.
//!
//! A tracker that refuses the wrong thing does not crash: it stops, and the
//! addresses it was holding are never handed out again. So the claim these
//! sweeps make is liveness, and they make it the only honest way — from every
//! state a seeded xorshift can churn the protocol into, drive it forward and
//! see that it goes. `Shootdown` is `Copy`, so the escape runs on a copy and
//! the churn carries on from where it was.

use molt_arch::CpuId;
use molt_arch::shootdown::{Error, Shootdown};
use molt_arch::va::{Class, Epoch, Extent, Hole, Region, Space};

const ROUNDS: usize = 1 << 12;
const SEED: u64 = 0x6d6f_6c74_7462_0000;
/// Cores in the machine. More than the smoke's four, so the mask is not the
/// only thing a round could be waiting on.
const CORES: u16 = 6;
/// Narrow on purpose, as in the allocator's own sweep: an arena that never runs
/// out proves nothing about an address held back.
const BITS: u32 = 35;
const HOLES: usize = 3 * 8;

struct Noise(u64);

impl Noise {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// A core, including two the machine does not have and, now and then, one
    /// no acknowledgement mask could name.
    fn cpu(&mut self) -> CpuId {
        match self.next() % 16 {
            0 => CpuId::new(Shootdown::LIMIT as u16),
            _ => CpuId::new(self.next() as u16 % (CORES + 2)),
        }
    }
}

fn cores() -> impl Iterator<Item = CpuId> {
    (0..CORES).map(CpuId::new)
}

/// Answers for every core the open round is still waiting on, in the order the
/// noise asks for, and returns the epoch that closed it.
fn finish(shootdown: &mut Shootdown, noise: &mut Noise) -> Option<Epoch> {
    let mut owing: Vec<CpuId> = cores().filter(|&cpu| shootdown.pending(cpu)).collect();
    for index in (1..owing.len()).rev() {
        owing.swap(index, noise.next() as usize % (index + 1));
    }

    let mut closed = None;
    for cpu in owing {
        assert_eq!(closed, None, "the round closed with cores still owing a flush");
        closed = shootdown.acknowledge(cpu).expect("a core the open round asked");
    }
    closed
}

/// Which refusal, as a place to count it in.
fn refusal(error: Error) -> usize {
    match error {
        Error::Width => 3,
        Error::Empty => 4,
        Error::Open => 5,
        Error::Closed => 6,
        Error::Stale => 7,
        Error::Foreign => 8,
    }
}

#[test]
fn no_run_of_answers_leaves_a_round_nobody_can_close() {
    let mut noise = Noise(SEED);
    let mut shootdown = Shootdown::new();
    // The round as the test has it: who was asked, and who the tracker has
    // accepted an answer from since.
    let mut asked = 0u64;
    let mut answered = 0u64;
    let mut epoch = Epoch::FIRST;
    let mut seen = [0; 9];

    for _ in 0..ROUNDS {
        let before = shootdown;
        match noise.next() % 4 {
            0 => {
                let mask = noise.next() & ((1 << CORES) - 1);
                // A core past the mask's width, and an epoch already retired:
                // both are rounds that must not open, and both are reached by
                // way of the checks that come first.
                let wide = noise.next() % 8 == 0;
                let wanted = if noise.next() % 4 == 0 { shootdown.retired() } else { epoch.next() };
                let asking = cores()
                    .filter(|cpu| mask >> cpu.index() & 1 == 1)
                    .chain(wide.then(|| CpuId::new(Shootdown::LIMIT as u16)));

                match shootdown.begin(wanted, asking) {
                    Ok(count) => {
                        assert_eq!(
                            count,
                            mask.count_ones(),
                            "a round waits on cores it never asked"
                        );
                        assert_eq!(shootdown.epoch(), Some(wanted));
                        assert_eq!(shootdown.outstanding(), count, "a round opened part flushed");
                        (asked, answered, epoch) = (mask, 0, wanted);
                        seen[0] += 1;
                    }
                    Err(error) => {
                        assert_eq!(shootdown, before, "a refused round moved the tracker");
                        seen[refusal(error)] += 1;
                    }
                }
            }
            1 | 2 => {
                // Cores that already answered, cores the round never asked,
                // cores that do not exist: all of them arrive here.
                let cpu = noise.cpu();
                let owed = shootdown.pending(cpu);

                match shootdown.acknowledge(cpu) {
                    Ok(closed) => {
                        assert!(asked & 1 << cpu.index() != 0, "a foreign answer was accepted");
                        assert_eq!(
                            shootdown.outstanding(),
                            before.outstanding() - u32::from(owed),
                            "an answer already given was counted again"
                        );
                        answered |= 1 << cpu.index();
                        match closed {
                            Some(retired) => {
                                assert_eq!(retired, epoch, "a round retired an epoch of its own");
                                assert_eq!(
                                    answered, asked,
                                    "a core that never answered was waived"
                                );
                                assert_eq!(shootdown.retired(), retired);
                                assert_eq!(shootdown.rounds(), before.rounds() + 1);
                                assert_eq!(shootdown.epoch(), None, "a closed round stayed open");
                                seen[2] += 1;
                            }
                            None => {
                                assert!(shootdown.outstanding() > 0, "a round nobody owes is open");
                                seen[1] += 1;
                            }
                        }
                    }
                    Err(error) => {
                        assert_eq!(shootdown, before, "a refused answer moved the tracker");
                        seen[refusal(error)] += 1;
                    }
                }
            }
            _ => {
                // The escape. An open round is closed by the cores it is still
                // waiting on, and a tracker with nothing open takes a new
                // round: refusing either one forever is the wedge this test is
                // named after.
                let mut copy = shootdown;
                match copy.epoch() {
                    Some(open) => {
                        assert_eq!(
                            finish(&mut copy, &mut noise),
                            Some(open),
                            "a round with no way out"
                        )
                    }
                    None => assert_eq!(
                        copy.begin(copy.retired().next(), cores()),
                        Ok(u32::from(CORES)),
                        "a tracker holding nobody refused the next round"
                    ),
                }
            }
        }
        assert!(shootdown.retired() >= before.retired(), "the tracker unretired an epoch");
    }

    assert!(seen.iter().all(|&count| count > 0), "the sweep never got to something: {seen:?}");
}

#[test]
fn no_run_of_flushes_leaves_an_address_stuck_in_quarantine() {
    let mut noise = Noise(SEED);
    let mut holes = [Hole::EMPTY; HOLES];
    let mut space = Space::over(BITS, &mut holes).expect("a space this wide cuts into arenas");
    let mut shootdown = Shootdown::new();
    let mut held: Vec<Extent> = Vec::new();
    // Released, and not safe to hand out until this epoch has been retired.
    let mut waiting: Vec<(Region, Epoch)> = Vec::new();

    for _ in 0..ROUNDS {
        match noise.next() % 8 {
            0..=3 => {
                let class = Class::ALL[noise.next() as usize % Class::ALL.len()];
                if let Ok(extent) = space.allocate(class, (1 + noise.next() % 4) * class.granule())
                {
                    let region = extent.region();
                    assert!(
                        waiting.iter().all(|&(held, _)| {
                            held.start() >= region.end() || region.start() >= held.end()
                        }),
                        "an address came back before its round closed"
                    );
                    held.push(extent);
                }
            }
            4 | 5 if !held.is_empty() => {
                let extent = held.remove(noise.next() as usize % held.len());
                let (region, open) = (extent.region(), space.open());
                match space.release(extent) {
                    Ok(()) => waiting.push((region, open)),
                    Err((_, extent)) => held.push(extent),
                }
            }
            // A sweep whose round cannot open yet is the interesting one: those
            // addresses wait on some later round instead, and waiting forever is
            // what this is watching for.
            6 => {
                let epoch = space.sweep();
                let _ = shootdown.begin(epoch, cores());
            }
            _ => {
                if let Ok(Some(retired)) = shootdown.acknowledge(noise.cpu()) {
                    space.retire(retired);
                    waiting.retain(|&(_, epoch)| epoch > space.retired());
                }
            }
        }
    }

    // The way out of whatever the churn left: give back what is still held,
    // close the round that is open, and open one for everything swept since.
    // A release refused for want of a free slot is retried once the flushes it
    // is waiting behind have joined its neighbours back up.
    for _ in 0..HOLES {
        held = held.into_iter().filter_map(|e| space.release(e).err().map(|(_, e)| e)).collect();
        if let Some(epoch) = finish(&mut shootdown, &mut noise) {
            space.retire(epoch);
        }
        let epoch = space.sweep();
        shootdown.begin(epoch, cores()).expect("a round for the epoch just swept");
        space.retire(finish(&mut shootdown, &mut noise).expect("every core answered"));
    }

    assert!(held.is_empty(), "{} extents the space would not take back", held.len());
    for class in Class::ALL {
        assert_eq!(space.quarantined(class), 0, "{class:?} left addresses waiting on nothing");
        assert_eq!(space.holes(class), 1, "churn left {class:?} in pieces");
        assert_eq!(space.largest(class), space.arena(class).bytes(), "{class:?} came back short");
    }
}
