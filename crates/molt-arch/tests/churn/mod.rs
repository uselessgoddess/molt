//! What this crate's property sweeps share on top of [`molt_churn`].

#![allow(dead_code)]

use molt_arch::va::Class;
pub use molt_churn::{MOVES, Seen, sweep};
use proptest::prelude::*;

/// A page class, uniformly. Every sweep here is over a space cut into one arena
/// per class, and every one of them has to be asked for.
pub fn class() -> impl Strategy<Value = Class> {
    (0..Class::ALL.len()).prop_map(|index| Class::ALL[index])
}
