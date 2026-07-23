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
