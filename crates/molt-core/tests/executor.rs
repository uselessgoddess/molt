use std::sync::Arc;
use std::thread;

use molt_core::cell::{Cell, Supervisor};
use molt_core::executor::{Executor, SpawnError};

#[test]
fn wakes_coalesce() {
    let executor = Executor::<2>::new();
    let first = executor.register().unwrap();
    let second = executor.register().unwrap();
    assert_eq!(executor.register(), Err(SpawnError::Full));

    executor.wake(first);
    executor.wake(first);
    executor.wake(second);

    assert_eq!(executor.next_ready(), Some(first));
    executor.complete_poll(first);
    assert_eq!(executor.next_ready(), Some(second));
    executor.complete_poll(second);
    assert_eq!(executor.next_ready(), None);

    executor.wake(first);
    assert_eq!(executor.next_ready(), Some(first));
    executor.complete_poll(first);
}

#[test]
fn poll_race_keeps_wake() {
    let executor = Arc::new(Executor::<1>::new());
    let task = executor.register().unwrap();
    executor.wake(task);
    assert_eq!(executor.next_ready(), Some(task));

    let notifier = executor.clone();
    thread::spawn(move || notifier.wake(task)).join().unwrap();
    executor.complete_poll(task);

    assert_eq!(executor.next_ready(), Some(task));
    executor.complete_poll(task);
    assert_eq!(executor.next_ready(), None);
}

#[derive(Default)]
struct State(u32);

struct Worker(State);

impl Cell for Worker {
    type Message = u32;
    type Reply = u32;
    type State = State;

    fn spawn(state: State) -> Self {
        Self(state)
    }

    fn handle(&mut self, value: u32) -> u32 {
        self.0.0 += value;
        self.0.0
    }
}

#[test]
fn restart_keeps_task() {
    let executor = Executor::<1>::new();
    let task = executor.register().unwrap();
    let mut cell = Supervisor::<Worker>::new(State(4));

    cell.restart_default();
    executor.wake(task);

    assert_eq!(cell.call(2), 2);
    assert_eq!(executor.next_ready(), Some(task));
}
