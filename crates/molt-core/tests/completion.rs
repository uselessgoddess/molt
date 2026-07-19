use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::thread;

use molt_core::cache::Padded;
use molt_core::completion::{CompletionError, CompletionSlab};

struct CountWake(AtomicUsize);

impl Wake for CountWake {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn completion_wakes_once() {
    let slab = CompletionSlab::<u32, 2>::new();
    let token = slab.reserve().unwrap();
    let wake = Arc::new(CountWake(AtomicUsize::new(0)));
    let waker = Waker::from(wake.clone());
    let mut cx = Context::from_waker(&waker);
    let mut future = pin!(slab.wait(token));

    assert_eq!(future.as_mut().poll(&mut cx), Poll::Pending);
    assert_eq!(slab.complete(token.request_id(), 42), Ok(()));
    assert_eq!(wake.0.load(Ordering::Relaxed), 1);
    assert_eq!(future.as_mut().poll(&mut cx), Poll::Ready(Ok(42)));
}

#[test]
fn restart_rejects_stale() {
    let slab = CompletionSlab::<u32, 2>::new();
    let cancelled = slab.reserve().unwrap();
    assert_eq!(slab.cancel(cancelled), Ok(()));
    assert_eq!(slab.complete(cancelled.request_id(), 1), Err(CompletionError::Stale));

    let outstanding = slab.reserve().unwrap();
    assert_eq!(slab.cancel_all(), 1);
    assert_eq!(slab.complete(outstanding.request_id(), 2), Err(CompletionError::Stale));
}

#[test]
fn poll_race_keeps_wake() {
    for expected in 0..256 {
        let slab = Arc::new(CompletionSlab::<usize, 1>::new());
        let token = slab.reserve().unwrap();
        let publisher = slab.clone();
        let completion = thread::spawn(move || {
            thread::yield_now();
            publisher.complete(token.request_id(), expected).unwrap();
        });

        let wake = Arc::new(CountWake(AtomicUsize::new(0)));
        let waker = Waker::from(wake.clone());
        let mut cx = Context::from_waker(&waker);
        let mut future = pin!(slab.wait(token));
        let first_poll = future.as_mut().poll(&mut cx);
        completion.join().unwrap();

        if first_poll == Poll::Pending {
            assert_eq!(wake.0.load(Ordering::Acquire), 1);
            assert_eq!(future.as_mut().poll(&mut cx), Poll::Ready(Ok(expected)));
        } else {
            assert_eq!(first_poll, Poll::Ready(Ok(expected)));
        }
    }
}

#[test]
fn cancel_race_reuses_slot() {
    // A cancel racing a completion is serialized by the slot's claim flag: the
    // waiter observes exactly one terminal outcome (the delivered value or a
    // cancellation) and the slot is always returned to a reusable empty state
    // without corruption, regardless of which side wins.
    for _ in 0..1024 {
        let slab = Arc::new(CompletionSlab::<usize, 1>::new());
        let token = slab.reserve().unwrap();

        let completer = slab.clone();
        let complete = thread::spawn(move || {
            thread::yield_now();
            completer.complete(token.request_id(), 7)
        });
        let canceller = slab.clone();
        let cancel = thread::spawn(move || {
            thread::yield_now();
            canceller.cancel(token)
        });

        let _ = complete.join().unwrap();
        let _ = cancel.join().unwrap();

        // The waiter never observes a torn value: it is either the completion
        // (if it won and was not yet consumed) or a cancellation.
        let wake = Arc::new(CountWake(AtomicUsize::new(0)));
        let waker = Waker::from(wake);
        let mut cx = Context::from_waker(&waker);
        let outcome = pin!(slab.wait(token)).as_mut().poll(&mut cx);
        match outcome {
            Poll::Ready(Ok(value)) => assert_eq!(value, 7),
            Poll::Ready(Err(CompletionError::Cancelled)) => {}
            other => panic!("unexpected terminal outcome: {other:?}"),
        }

        // Whatever raced, the slot is free again: a fresh reservation succeeds
        // with a new id and a stale completion against the old id is rejected.
        let reused = slab.reserve().unwrap();
        assert_ne!(reused.request_id(), token.request_id());
        assert_eq!(slab.complete(token.request_id(), 9), Err(CompletionError::Stale));
    }
}

#[test]
fn producers_keep_slots() {
    // Independent producers completing distinct slots must each land in their
    // own slot without clobbering a neighbour.
    let slab = Arc::new(CompletionSlab::<usize, 8>::new());
    let tokens: Vec<_> = (0..8).map(|_| slab.reserve().unwrap()).collect();
    assert_eq!(slab.reserve(), Err(CompletionError::Full));

    let handles: Vec<_> = tokens
        .iter()
        .enumerate()
        .map(|(value, token)| {
            let producer = slab.clone();
            let id = token.request_id();
            thread::spawn(move || {
                thread::yield_now();
                producer.complete(id, value).unwrap();
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }

    for (value, token) in tokens.into_iter().enumerate() {
        let wake = Arc::new(CountWake(AtomicUsize::new(0)));
        let waker = Waker::from(wake);
        let mut cx = Context::from_waker(&waker);
        let mut future = pin!(slab.wait(token));
        assert_eq!(future.as_mut().poll(&mut cx), Poll::Ready(Ok(value)));
    }
}

#[test]
fn padded_layout_completes() {
    let slab = CompletionSlab::<u32, 1, Padded>::new();
    let token = slab.reserve().unwrap();

    slab.complete(token.request_id(), 7).unwrap();

    assert_eq!(slab.reserve(), Err(CompletionError::Full));
}
