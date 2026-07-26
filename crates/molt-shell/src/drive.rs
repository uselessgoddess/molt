//! Running one future when the only other work is the driver below it.

use core::future::Future;
use core::pin::pin;
use core::task::{Context, Poll, Waker};

/// Polls `future` to completion, running `serve` between polls.
///
/// This is a shell in a boot log, not an executor: one task, one driver, and a
/// noop waker because there is nobody else to run when the task is not ready.
/// `serve` must make progress — a driver that answers nothing spins here until
/// the machine is stopped, exactly as the caller asked for.
pub fn drive<F: Future>(future: F, mut serve: impl FnMut()) -> F::Output {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
        serve();
    }
}

/// Polls `future` at most `rounds` times, returning `None` if it wants more.
///
/// The budget is what makes a watchdog possible from outside the cell: a task
/// that will never finish — waiting on a service that stopped answering, or
/// looping on work that cannot complete — is dropped here rather than spun on,
/// and its supervisor is the one that decides what to do about it.
pub fn drive_bounded<F: Future>(
    future: F,
    rounds: usize,
    mut serve: impl FnMut(),
) -> Option<F::Output> {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    for _ in 0..rounds {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return Some(output);
        }
        serve();
    }
    None
}
