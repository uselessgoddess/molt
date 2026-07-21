use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use molt_core::cache::Padded;
use molt_core::interrupt::{InterruptError, InterruptSlab};

struct CountWake(AtomicUsize);

impl Wake for CountWake {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn arrival_wakes_waiter() {
    let slab = InterruptSlab::<2>::new();
    let token = slab.reserve().unwrap();
    let wake = Arc::new(CountWake(AtomicUsize::new(0)));
    let waker = Waker::from(wake.clone());
    let mut cx = Context::from_waker(&waker);
    let mut future = pin!(slab.watch(token));

    assert_eq!(future.as_mut().poll(&mut cx), Poll::Pending);
    assert!(slab.signal(token.index()));
    assert_eq!(wake.0.load(Ordering::Relaxed), 1);
    assert_eq!(future.as_mut().poll(&mut cx), Poll::Ready(Ok(1)));
}

#[test]
fn early_arrival_is_kept() {
    let slab = InterruptSlab::<2>::new();
    let token = slab.reserve().unwrap();
    let mut future = pin!(slab.watch(token));

    assert!(slab.signal(token.index()));
    assert_eq!(future.as_mut().poll(&mut Context::from_waker(Waker::noop())), Poll::Ready(Ok(1)));
}

#[test]
fn bursts_coalesce_into_a_count() {
    let slab = InterruptSlab::<2>::new();
    let token = slab.reserve().unwrap();
    let mut future = pin!(slab.watch(token));

    for _ in 0..3 {
        slab.signal(token.index());
    }
    assert_eq!(future.as_mut().poll(&mut Context::from_waker(Waker::noop())), Poll::Ready(Ok(3)));
}

#[test]
fn stale_vector_is_reported() {
    let slab = InterruptSlab::<1>::new();
    let token = slab.reserve().unwrap();

    assert_eq!(slab.release(token), Ok(()));
    assert!(!slab.signal(token.index()), "a released vector accepted an interrupt");
    assert_eq!(slab.arrivals(token), Err(InterruptError::Released));
}

#[test]
fn released_vector_ends_the_wait() {
    let slab = InterruptSlab::<1>::new();
    let token = slab.reserve().unwrap();
    let mut future = pin!(slab.watch(token));
    let mut cx = Context::from_waker(Waker::noop());

    assert_eq!(future.as_mut().poll(&mut cx), Poll::Pending);
    assert_eq!(slab.release(token), Ok(()));
    assert_eq!(future.as_mut().poll(&mut cx), Poll::Ready(Err(InterruptError::Released)));
}

#[test]
fn reissued_vector_rejects_the_old_token() {
    let slab = InterruptSlab::<1>::new();
    let old = slab.reserve().unwrap();
    slab.release(old).unwrap();

    let new = slab.reserve().unwrap();
    assert_eq!(new.index(), old.index());
    assert_eq!(slab.arrivals(old), Err(InterruptError::Released));
    assert_eq!(slab.arrivals(new), Ok(0));
}

#[test]
fn exhausted_slab_refuses_reservations() {
    let slab = InterruptSlab::<1>::new();
    slab.reserve().unwrap();
    assert_eq!(slab.reserve(), Err(InterruptError::Full));
}

#[test]
fn a_vector_can_be_claimed_by_number() {
    let slab = InterruptSlab::<4>::new();
    let token = slab.claim(2).unwrap();

    assert_eq!(token.index(), 2);
    assert!(slab.signal(2));
    assert_eq!(slab.arrivals(token), Ok(1));
    assert_eq!(slab.reserve().unwrap().index(), 0, "a claim moved the free vector");
}

#[test]
fn a_vector_someone_else_holds_is_refused() {
    let slab = InterruptSlab::<2>::new();
    let token = slab.claim(1).unwrap();

    assert_eq!(slab.claim(1), Err(InterruptError::Taken));
    assert_eq!(slab.claim(2), Err(InterruptError::Taken), "a vector past the slab was handed out");
    slab.release(token).unwrap();
    assert_eq!(slab.claim(1).map(|token| token.index()), Ok(1));
}

#[test]
fn padded_vectors_deliver_the_same_way() {
    let slab = InterruptSlab::<2, Padded>::new();
    let token = slab.reserve().unwrap();
    let mut future = pin!(slab.watch(token));

    assert!(slab.signal(token.index()));
    assert_eq!(future.as_mut().poll(&mut Context::from_waker(Waker::noop())), Poll::Ready(Ok(1)));
}
