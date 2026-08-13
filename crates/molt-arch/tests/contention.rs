//! Eight cores, one address space, and the lock the kernel actually holds.
//!
//! Every structure in this crate takes `&mut self`, which says what it needs
//! and not where it comes from: in the kernel the address space sits behind a
//! ticket lock and callers borrow it (`kernel/src/space.rs`). What that leaves
//! to show is that the sequence one core runs under that lock — take a window
//! or fill it, count the grant, give it back, hand the addresses to a shootdown
//! — is safe to interleave with seven other cores running it too.
//!
//! Three failures would not show up in a single-threaded test. An address
//! handed to two cores at once, which the counts catch: a second `map` over a
//! live range is [`refcount::Error::Overlap`]. An address that moves under a
//! core that was told it, which the cache catches. And a quarantine nobody can
//! close, which is what the end state is for: after the churn, every address
//! this test took is back in one free range, and no round is left open.
//!
//! [`refcount::Error::Overlap`]: molt_arch::refcount::Error::Overlap

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use molt_arch::cache::{File, Window, Windows};
use molt_arch::memory::Span;
use molt_arch::refcount::{Leaves, Run};
use molt_arch::shootdown::Shootdown;
use molt_arch::va::{Class, Hole, Region, Space};
use molt_core::cpu::CpuId;
use molt_core::lock::Spinlock;

/// The cores the smoke boots, and the ones this test pretends to be.
const CORES: u16 = 8;
const ROUNDS: u64 = 250;

const SV57: u32 = 57;
const MEGA: u64 = Class::Mega.granule();

/// Windows of one file, so cores contend over the same few of them rather than
/// each working on its own.
const WINDOWS: u64 = 4;
const LOGS: File = File::new(1);

/// Where the frames under a window are, which nothing here reads.
const RAM: u64 = 1 << 30;

/// The machine-wide tables, which is what one lock covers.
struct Machine {
    space: Space<'static>,
    leaves: Leaves<'static>,
    windows: Windows<'static>,
    shootdown: Shootdown,
}

impl Machine {
    fn new() -> Self {
        // Leaked because the tables outlive every core, which in the kernel is
        // a static and here is the closest a test gets to one.
        let holes = Box::leak(Box::new([Hole::EMPTY; 3 * 64]));
        let runs = Box::leak(Box::new([Run::EMPTY; 32]));
        let slots = Box::leak(Box::new([const { Window::EMPTY }; 8]));
        Self {
            space: Space::over(SV57, holes).expect("a space wide enough to cut"),
            leaves: Leaves::over(runs),
            windows: Windows::over(slots),
            shootdown: Shootdown::new(),
        }
    }

    /// The shootdown protocol, as a core reaches it: answer what is open, and
    /// start a round for the batch if nobody else has.
    ///
    /// A round closes only once all eight cores have been through here, so an
    /// address freed in it waits for every core that could be holding a
    /// translation — the property, and the way this test can livelock if the
    /// quarantine ever needs a core that is not coming.
    fn flush(&mut self, cpu: CpuId) {
        if self.shootdown.pending(cpu) {
            let retired = self.shootdown.acknowledge(cpu).expect("a round this core is in");
            if let Some(epoch) = retired {
                self.space.retire(epoch);
            }
        }
        if self.shootdown.epoch().is_none() && self.space.quarantined(Class::Mega) > 0 {
            let epoch = self.space.sweep();
            let cores = (0..CORES).map(CpuId::new);
            self.shootdown.begin(epoch, cores).expect("a round nobody else opened");
        }
    }
}

/// Takes a window of the file, filling it if this core is the one that found it
/// missing, and counts the grant that follows.
fn enter(machine: &Spinlock<Machine>, cpu: CpuId, offset: u64) -> Region {
    let mut held = machine.lock();
    let machine = &mut *held;
    machine.flush(cpu);

    let region = match machine.windows.hold(LOGS, offset) {
        Ok(window) => window.region().expect("a cached window"),
        Err(_) => {
            let extent = machine.space.allocate(Class::Mega, MEGA).expect("room in the arena");
            machine
                .leaves
                .map(extent.start(), Class::Mega, 1)
                .expect("an address no other core is already counting");
            let frames = Span::new(RAM + offset, RAM + offset + MEGA).expect("frames to back it");
            let window =
                machine.windows.insert(LOGS, offset, extent, frames).expect("a free window slot");
            window.region().expect("the window just filled")
        }
    };

    machine.leaves.share(region).expect("a region the table counts");
    region
}

/// Gives the window back, and the addresses under it if this core was the last
/// one out.
fn leave(machine: &Spinlock<Machine>, cpu: CpuId, offset: u64, region: Region) {
    let mut held = machine.lock();
    let machine = &mut *held;

    let cached = machine.windows.lookup(LOGS, offset).and_then(Window::region);
    assert_eq!(cached, Some(region), "a window moved under a core that was holding it");

    machine.leaves.release(region).expect("the grant this core counted");
    let holders = machine.windows.release(LOGS, offset).expect("a window this core held");
    // Counted once by the core that filled the window, and once per holder: a
    // window that still has holders is one no core may take the addresses of.
    let counted = machine.leaves.count(region.start());
    assert_eq!(counted, Some(holders + 1), "the counts and the cache disagree");
    if holders != 0 {
        return;
    }

    let (extent, _) = machine.windows.evict(LOGS, offset).expect("a window nobody holds");
    let reclaimed = machine.leaves.release(region).expect("the mapping the window was filled with");
    assert!(!reclaimed.is_empty(), "an evicted window left leaves nobody holds counted");
    machine.space.release(extent).expect("the extent this window was filled from");
    machine.flush(cpu);
}

#[test]
fn eight_cores_share_space_losing_none() {
    let machine = Arc::new(Spinlock::new(Machine::new()));
    let taken = Arc::new(AtomicU64::new(0));
    let gate = Arc::new(Barrier::new(CORES as usize));

    let cores: Vec<_> = (0..CORES)
        .map(|core| {
            let (machine, taken) = (Arc::clone(&machine), Arc::clone(&taken));
            let gate = Arc::clone(&gate);
            thread::spawn(move || {
                let cpu = CpuId::new(core);

                // One window every core is inside at once, rather than eight
                // cores taking turns at one: what the counts say has to be true
                // while all eight of them are holding it.
                let region = enter(&machine, cpu, 0);
                gate.wait();
                {
                    let held = machine.lock();
                    let window = held.windows.lookup(LOGS, 0).expect("the window every core took");
                    assert_eq!(window.holders(), u32::from(CORES), "a holder went uncounted");
                    assert_eq!(held.leaves.count(region.start()), Some(u32::from(CORES) + 1));
                    assert_eq!(
                        held.leaves.runs(),
                        1,
                        "one shared window cost more than one record"
                    );
                }
                gate.wait();
                leave(&machine, cpu, 0, region);

                for round in 0..ROUNDS {
                    // Cores start on different windows and move at the same
                    // rate, so every window is both filled and evicted while
                    // other cores are using their own.
                    let offset = (round + u64::from(core)) % WINDOWS * MEGA;
                    let region = enter(&machine, cpu, offset);
                    // The lock is not held here on purpose: this is where the
                    // core would be using the mapping, and where the other
                    // seven get their turn at the tables.
                    thread::yield_now();
                    leave(&machine, cpu, offset, region);
                    taken.fetch_add(1, Ordering::Relaxed);
                }
            })
        })
        .collect();
    for core in cores {
        core.join().expect("a core that only used the address space");
    }

    // The cores are gone, and the batches they freed last are still waiting on
    // rounds nobody is left to answer. In the kernel an idle core answers the
    // IPI; here the test answers for them until the space has settled.
    let mut held = machine.lock();
    loop {
        for core in (0..CORES).map(CpuId::new) {
            if held.shootdown.pending(core)
                && let Some(epoch) = held.shootdown.acknowledge(core).expect("an open round")
            {
                held.space.retire(epoch);
            }
        }
        if held.space.quarantined(Class::Mega) == 0 {
            break;
        }
        let epoch = held.space.sweep();
        held.shootdown.begin(epoch, (0..CORES).map(CpuId::new)).expect("no round left open");
    }

    assert_eq!(taken.load(Ordering::Relaxed), u64::from(CORES) * ROUNDS, "a core never finished");
    // A hit is a core finding a window another core had filled, which is the
    // churn actually overlapping rather than eight cores taking turns.
    assert!(held.windows.hits() > 0, "no core ever reached a window another core was holding");
    assert!(held.windows.misses() > 0, "no core ever filled a window");
    assert!(held.shootdown.rounds() > 0, "the churn never needed a shootdown");
    assert_eq!(held.shootdown.epoch(), None, "a round was left open with no core to close it");
    assert_eq!(held.windows.len(), 0, "a window outlived every core that held it");
    assert_eq!(held.leaves.runs(), 0, "an address is still counted for a core that is gone");
    assert_eq!(held.space.quarantined(Class::Mega), 0, "an address never came out of quarantine");
    assert_eq!(held.space.holes(Class::Mega), 1, "the churn left the arena fragmented");
    assert_eq!(held.space.free(Class::Mega), held.space.arena(Class::Mega).bytes());
}
