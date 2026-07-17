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
    assert_eq!(executor.next_ready(), Some(second));
    assert_eq!(executor.next_ready(), None);

    // A wake arriving after the task was dequeued remains visible.
    executor.wake(first);
    assert_eq!(executor.next_ready(), Some(first));
}
