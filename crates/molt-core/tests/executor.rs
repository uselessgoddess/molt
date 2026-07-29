use std::convert::Infallible;
use std::sync::Arc;
use std::thread;

use molt_core::cell::{Cell, Handler, Quiesced, Supervisor};
use molt_core::executor::{Executor, SpawnError};

#[test]
fn wakes_coalesce() -> Result<(), SpawnError> {
    let executor = Executor::<2>::new();
    let first = executor.register()?;
    let second = executor.register()?;
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
    Ok(())
}

#[test]
fn poll_race_keeps_wake() -> Result<(), SpawnError> {
    let executor = Arc::new(Executor::<1>::new());
    let task = executor.register()?;
    executor.wake(task);
    assert_eq!(executor.next_ready(), Some(task));

    let notifier = executor.clone();
    thread::spawn(move || notifier.wake(task)).join().unwrap();
    executor.complete_poll(task);

    assert_eq!(executor.next_ready(), Some(task));
    executor.complete_poll(task);
    assert_eq!(executor.next_ready(), None);
    Ok(())
}

struct Worker(u32);

impl Cell for Worker {
    type Error = Infallible;
    type State = u32;

    fn spawn(start: u32) -> Result<Self, Infallible> {
        Ok(Self(start))
    }

    fn restart(&mut self) -> Result<(), Infallible> {
        self.0 = 0;
        Ok(())
    }
}

impl Handler for Worker {
    type Message = u32;
    type Reply = u32;

    fn handle(&mut self, value: u32) -> u32 {
        self.0 += value;
        self.0
    }
}

#[test]
fn restart_keeps_task() -> Result<(), Infallible> {
    let executor = Executor::<1>::new();
    let task = executor.register().unwrap();
    let mut cell = Supervisor::<Worker>::new(4)?;

    cell.restart(&mut Quiesced)?;
    executor.wake(task);

    assert_eq!(cell.call(2), 2);
    assert_eq!(executor.next_ready(), Some(task));
    Ok(())
}
