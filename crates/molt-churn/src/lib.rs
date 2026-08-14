//! What the property sweeps share.
//!
//! Every sweep has the same shape: proptest generates a list of moves, the sweep
//! replays it against a model, and a list that breaks the model is shrunk until
//! nothing more can be dropped. Hence a move being a value rather than a call —
//! control flow cannot be shrunk, and the shortest churn reaching a bug is the
//! whole of what a failure has to say.
//!
//! A dev-dependency, never linked into the kernel; a crate only because a
//! `tests/` module cannot be shared across crates.

use std::cell::RefCell;
use std::collections::BTreeMap;

use proptest::prelude::*;
use proptest::test_runner::{Config, TestCaseResult, TestRunner};

/// Move lists one sweep replays, and moves in one list.
///
/// Wider than deep. A fresh subject per list is what makes two lists
/// independent, and a couple of hundred short walks reach more first states than
/// one long one.
pub const CASES: u32 = 256;
pub const MOVES: usize = 256;

/// Replays `property` over generated values, reporting the smallest value that
/// broke it rather than the one generated.
///
/// Nothing is persisted: proptest files counterexamples next to the `proptest!`
/// macro that produced them, and these sweeps are ordinary tests calling a
/// runner. The shrunk input in the failure is the whole report.
#[track_caller]
pub fn sweep<S: Strategy>(strategy: S, property: impl Fn(S::Value) -> TestCaseResult) {
    let config = Config { cases: CASES, failure_persistence: None, ..Config::default() };
    let mut runner = TestRunner::new(config);
    if let Err(failure) = runner.run(&strategy, property) {
        panic!("{failure}");
    }
}

/// What a sweep got to, counted over every case rather than within one.
///
/// A sweep that generated nothing interesting passes as quietly as one that
/// covered everything, so each names the states it exists to reach and
/// [`reached`](Seen::reached) says which never happened. Counted across cases,
/// because requiring it of one list would make an honest short list the bug
/// shrinking reports.
#[derive(Default)]
pub struct Seen(RefCell<BTreeMap<&'static str, u64>>);

impl Seen {
    pub fn saw(&self, what: &'static str) {
        *self.0.borrow_mut().entry(what).or_default() += 1;
    }

    /// Panics unless every one of `wanted` happened at least once.
    #[track_caller]
    pub fn reached(&self, wanted: &[&'static str]) {
        let counts = self.0.borrow();
        let missed: Vec<_> = wanted.iter().filter(|what| !counts.contains_key(*what)).collect();
        assert!(missed.is_empty(), "the sweep never got to {missed:?}, only to {counts:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The floor is only useful if the failure names what nobody reached.
    #[test]
    #[should_panic(expected = "never got to [\"that\"]")]
    fn reached_names_what_never_happened() {
        let seen = Seen::default();
        seen.saw("this");
        seen.reached(&["this", "that"]);
    }

    #[test]
    fn reached_passes_once_every_state_happened() {
        let seen = Seen::default();
        seen.saw("this");
        seen.saw("that");
        seen.reached(&["this", "that"]);
    }
}
