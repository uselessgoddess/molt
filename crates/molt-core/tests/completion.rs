use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::thread;

use molt_core::completion::{CompletionError, CompletionSlab};

struct CountWake(AtomicUsize);

impl Wake for CountWake {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn completion_registration_wakes_and_delivers_once() {
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
fn cancellation_and_restart_reject_stale_results() {
    let slab = CompletionSlab::<u32, 2>::new();
    let cancelled = slab.reserve().unwrap();
    assert_eq!(slab.cancel(cancelled), Ok(()));
    assert_eq!(slab.complete(cancelled.request_id(), 1), Err(CompletionError::Stale));

    let outstanding = slab.reserve().unwrap();
    assert_eq!(slab.cancel_all(), 1);
    assert_eq!(slab.complete(outstanding.request_id(), 2), Err(CompletionError::Stale));
}

#[test]
fn concurrent_registration_and_completion_never_loses_a_wakeup() {
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
