#![no_std]

//! One executor per core, and nothing shared between two of them.
//!
//! A core owns its tasks, its ready queues, and its timer wheel. The only
//! thing another core can reach is [`Handle`]: an inbox it pushes to and a
//! doorbell it rings. That is where `Send` is asked for, and the only place —
//! a task that never leaves its core is never made to prove it could.
//!
//! Three pieces make that work. A task is an `Arc` whose future is
//! touched by one core only, so a waker may cross cores while the future does
//! not. The inbox is a stack pushed by anyone and drained by its owner, which
//! is one atomic swap per turn rather than a lock per wake. And [`Timers`] is
//! a hierarchical wheel, so a deadline costs a shift to arm and a slot to
//! expire instead of a comparison per tick.

extern crate alloc;
#[cfg(test)]
extern crate std;

mod exec;
mod machine;
mod queue;
mod task;
mod timer;

pub use crate::exec::{Executor, Handle, SpawnError};
pub use crate::machine::{Machine, Solo};
pub use crate::task::Priority;
pub use crate::timer::{HORIZON, Sleep, Timeout, Timers};
