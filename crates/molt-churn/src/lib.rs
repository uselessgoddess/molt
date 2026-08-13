//! What the property sweeps share.
//!
//! Every sweep in the workspace has the same shape: proptest generates a list of
//! moves, the sweep replays it against a model, and a list that breaks the model
//! is cut down until nothing more can be dropped from it. That is why a move is
//! a value and not a call — control flow cannot be shrunk, and the shortest
//! churn that reaches a bug is the whole of what a failure has to say.
//!
//! Nothing here is ever linked into the kernel: this is a dev-dependency of the
//! crates whose sweeps live next to their code, and the only reason it is a
//! crate at all is that a `tests/` module cannot be shared across crates.

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

/// Replays `property` over generated values, and reports the smallest value that
/// broke it rather than the one that happened to be generated.
///
/// Nothing is persisted: proptest files a counterexample next to the source of
/// the `proptest!` macro that produced it, and these sweeps are ordinary tests
/// calling a runner, so there is no such source to file it against. The shrunk
/// input in the failure is the whole report, and it reproduces by rerunning.
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
/// covered everything, so each of them names the states it is there to reach and
/// [`reached`](Seen::reached) says so when one of them never happened. No single
/// list is required to reach anything: that would make an honest short list a
/// failure, and shrinking would then report it as the bug.
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

    /// The floor is only worth having if it fails, and only useful if the
    /// failure names the state nobody reached rather than the ones they did.
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
