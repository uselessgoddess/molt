use std::sync::Arc;
use std::thread;

use molt_core::executor::{Executor, SpawnError};

#[test]
fn bounded_ready_queue_coalesces_wakes_without_losing_them() {
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

    // A wake arriving after the task was dequeued remains visible.
    executor.wake(first);
    assert_eq!(executor.next_ready(), Some(first));
    executor.complete_poll(first);
}

#[test]
fn wake_during_poll_remains_ready_after_poll_completion() {
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
