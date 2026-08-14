//! What the ticket lock is for: many cores, one structure, no lost writes.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use molt_core::lock::Spinlock;

const CORES: u64 = 8;
const EACH: u64 = 2000;

#[test]
fn contention_loses_no_increment() {
    let lock = Arc::new(Spinlock::new(0u64));
    let cores: Vec<_> = (0..CORES)
        .map(|_| {
            let lock = Arc::clone(&lock);
            thread::spawn(move || {
                for _ in 0..EACH {
                    *lock.lock() += 1;
                }
            })
        })
        .collect();
    for core in cores {
        core.join().expect("a thread that only counted");
    }

    assert_eq!(*lock.lock(), CORES * EACH, "an increment was lost under the lock");
}

#[test]
fn every_core_gets_its_turn() {
    let lock = Arc::new(Spinlock::new(()));
    let turns = Arc::new((0..CORES).map(|_| AtomicU64::new(0)).collect::<Vec<_>>());

    let cores: Vec<_> = (0..CORES)
        .map(|core| {
            let (lock, turns) = (Arc::clone(&lock), Arc::clone(&turns));
            thread::spawn(move || {
                for _ in 0..EACH {
                    let held = lock.lock();
                    turns[core as usize].fetch_add(1, Ordering::Relaxed);
                    drop(held);
                }
            })
        })
        .collect();
    for core in cores {
        core.join().expect("a thread that only took the lock");
    }

    for (core, taken) in turns.iter().enumerate() {
        assert_eq!(taken.load(Ordering::Relaxed), EACH, "core {core} was starved of the lock");
    }
}
