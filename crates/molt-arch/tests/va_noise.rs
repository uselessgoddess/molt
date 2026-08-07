//! What churn nobody chose is allowed to do to the address space.
//!
//! The example tests next door take turns the author thought of. This one takes
//! turns the author did not: allocate, release, sweep and retire in whatever
//! order a seeded xorshift asks for, against a model that remembers every
//! address handed out and every address still waiting on a flush.

use molt_arch::va::{Class, Epoch, Extent, Hole, Region, Space};

const ROUNDS: usize = 1 << 12;
const SEED: u64 = 0x6d6f_6c74_7661_0000;
/// Narrow on purpose: Sv57's arenas are petabytes, and an allocator that never
/// runs out proves nothing about what it does when it does.
const BITS: u32 = 35;
/// Eight free ranges per class, which a few dozen extents can fill.
const HOLES: usize = 3 * 8;

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

    fn pick(&mut self, len: usize) -> usize {
        self.next() as usize % len
    }
}

fn overlap(one: Region, other: Region) -> bool {
    one.start() < other.end() && other.start() < one.end()
}

fn live(held: &[Extent], class: Class) -> u64 {
    held.iter().filter(|extent| extent.class() == class).map(Extent::bytes).sum()
}

#[test]
fn churn_hands_out_no_address_twice() {
    let mut noise = Noise(SEED);
    let mut holes = [Hole::EMPTY; HOLES];
    let mut space = Space::over(BITS, &mut holes).expect("a space this wide cuts into arenas");
    let mut held: Vec<Extent> = Vec::new();
    let mut waiting: Vec<(Region, Epoch)> = Vec::new();
    let mut swept = Epoch::FIRST;
    let mut taken = 0;
    let mut refused = 0;

    for _ in 0..ROUNDS {
        match noise.next() % 8 {
            0..=3 => {
                let class = noise.class();
                let Ok(extent) = space.allocate(class, (1 + noise.next() % 4) * class.granule())
                else {
                    continue;
                };
                let region = extent.region();
                assert_eq!(region.start() % class.granule(), 0, "{class:?} came out misaligned");
                assert!(space.arena(class).covers(region), "an extent left its class arena");
                assert!(
                    held.iter().all(|other| !overlap(other.region(), region)),
                    "two live extents were handed the same address"
                );
                assert!(
                    waiting.iter().all(|&(held, _)| !overlap(held, region)),
                    "an address came back before every hart had flushed it"
                );
                held.push(extent);
                taken += 1;
            }
            4 | 5 if !held.is_empty() => {
                let extent = held.remove(noise.pick(held.len()));
                let (region, open) = (extent.region(), space.open());
                match space.release(extent) {
                    Ok(()) => waiting.push((region, open)),
                    Err((_, extent)) => {
                        refused += 1;
                        held.push(extent);
                    }
                }
            }
            6 => swept = space.sweep(),
            _ => {
                space.retire(swept);
                waiting.retain(|&(_, ready)| ready > space.retired());
            }
        }

        for class in Class::ALL {
            assert_eq!(
                space.free(class) + space.quarantined(class) + live(&held, class),
                space.arena(class).bytes(),
                "{class:?} lost address space"
            );
        }
    }

    assert!(taken > ROUNDS / 100, "the sweep proved nothing: {taken} extents handed out");
    assert!(refused > 0, "the free list never filled, so nothing tested what happens when it does");
}

#[test]
fn churn_gives_every_arena_back_whole() {
    let mut noise = Noise(SEED);
    let mut holes = [Hole::EMPTY; HOLES];
    let mut space = Space::over(BITS, &mut holes).expect("a space this wide cuts into arenas");
    let mut held: Vec<Extent> = Vec::new();

    for _ in 0..ROUNDS {
        match noise.next() % 4 {
            0 | 1 => {
                let class = noise.class();
                if let Ok(extent) = space.allocate(class, (1 + noise.next() % 4) * class.granule())
                {
                    held.push(extent);
                }
            }
            2 if !held.is_empty() => {
                let extent = held.remove(noise.pick(held.len()));
                if let Err((_, extent)) = space.release(extent) {
                    held.push(extent);
                }
            }
            _ => {
                let flushed = space.sweep();
                space.retire(flushed);
            }
        }
    }

    // A refusal is a full free list, and a flush is what empties one, so the
    // drain retries rather than giving up on the addresses.
    held.sort_by_key(Extent::start);
    for _ in 0..HOLES {
        held = held.into_iter().filter_map(|e| space.release(e).err().map(|(_, e)| e)).collect();
        let flushed = space.sweep();
        space.retire(flushed);
    }

    assert!(held.is_empty(), "{} extents the space would not take back", held.len());
    for class in Class::ALL {
        assert_eq!(space.holes(class), 1, "churn left {class:?} in pieces");
        assert_eq!(space.largest(class), space.arena(class).bytes(), "{class:?} came back short");
    }
}
