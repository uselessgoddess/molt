use std::hint::spin_loop;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use molt_core::ring::{Completion, IoRing, RequestId, SpscRing, Submission};

struct DropCounter<'counter>(&'counter AtomicUsize);

impl Drop for DropCounter<'_> {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn preserves_fifo_order() {
    let mut ring = SpscRing::<u32, 2>::new();
    let (mut producer, mut consumer) = ring.split();

    assert_eq!(producer.try_push(10), Ok(()));
    assert_eq!(producer.try_push(20), Ok(()));
    assert_eq!(producer.try_push(30), Err(30));
    assert_eq!(consumer.try_pop(), Some(10));
    assert_eq!(producer.try_push(30), Ok(()));
    assert_eq!(consumer.try_pop(), Some(20));
    assert_eq!(consumer.try_pop(), Some(30));
    assert_eq!(consumer.try_pop(), None);
}

#[test]
fn drop_pending_values() {
    let drops = AtomicUsize::new(0);

    {
        let mut ring = SpscRing::<DropCounter<'_>, 4>::new();
        let (mut producer, mut consumer) = ring.split();
        assert!(producer.try_push(DropCounter(&drops)).is_ok());
        assert!(producer.try_push(DropCounter(&drops)).is_ok());
        assert!(producer.try_push(DropCounter(&drops)).is_ok());

        drop(consumer.try_pop());
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    assert_eq!(drops.load(Ordering::Relaxed), 3);
}

#[test]
fn transfer_values_between_threads() {
    const ITEMS: usize = 10_000;
    let mut ring = SpscRing::<usize, 64>::new();
    let (mut producer, mut consumer) = ring.split();

    thread::scope(|scope| {
        scope.spawn(move || {
            for expected in 0..ITEMS {
                while consumer.try_pop() != Some(expected) {
                    spin_loop();
                }
            }
        });

        for value in 0..ITEMS {
            let mut pending = value;
            loop {
                match producer.try_push(pending) {
                    Ok(()) => break,
                    Err(value) => {
                        pending = value;
                        spin_loop();
                    }
                }
            }
        }
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TestOp {
    Read { sector: u64 },
}

#[test]
fn matches_completion_to_submission() {
    let mut ring = IoRing::<TestOp, u32, 4>::new();
    let (mut client, mut driver) = ring.split();
    let first = Submission::new(RequestId::new(7), TestOp::Read { sector: 1 });
    let second = Submission::new(RequestId::new(8), TestOp::Read { sector: 2 });

    client.try_submit(first).unwrap();
    client.try_submit(second).unwrap();
    assert_eq!(driver.try_next(), Some(first));
    assert_eq!(driver.try_next(), Some(second));

    driver.try_complete(Completion::new(second.id(), 22)).unwrap();
    driver.try_complete(Completion::new(first.id(), 11)).unwrap();
    assert_eq!(client.try_completion(), Some(Completion::new(second.id(), 22)));
    assert_eq!(client.try_completion(), Some(Completion::new(first.id(), 11)));
}
