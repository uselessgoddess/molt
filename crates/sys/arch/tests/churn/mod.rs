//! What this crate's property sweeps share on top of [`churn`].

#![allow(dead_code)]

pub use churn::{MOVES, Seen, sweep};
use molt_arch::va::Class;
use proptest::prelude::*;

/// A page class, uniformly. Every sweep here is over a space cut into one arena
/// per class, and every one of them has to be asked for.
pub fn class() -> impl Strategy<Value = Class> {
    (0..Class::ALL.len()).prop_map(|index| Class::ALL[index])
}
