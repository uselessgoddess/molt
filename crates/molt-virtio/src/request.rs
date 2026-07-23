//! The in-flight request table: what lets a completion be matched, cancelled,
//! or recognized as stale.
//!
//! Each descriptor head owns one slot. Submitting a request marks its slot
//! [`Pending`](State::Pending) and hands back a [`Token`] stamped with the
//! slot's generation. [`cancel`](Requests::cancel) gives up on a request
//! without freeing its head — the device may still write it — so a later
//! completion for that head arrives as [`Completion::Stale`] instead of being
//! delivered to a caller that walked away. The generation bumps on every
//! completion, so a token never matches a slot that has since been reused.

/// A submitted request, matched against its slot on completion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Token {
    head: u16,
    generation: u32,
}

/// What became of a completion the device returned.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Completion {
    /// The head belonged to a live request; deliver its result.
    Delivered,
    /// The head belonged to a cancelled or already-finished request; drop it.
    Stale,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum State {
    Free,
    Pending,
    Cancelled,
}

#[derive(Clone, Copy)]
struct Slot {
    state: State,
    generation: u32,
}

/// A fixed table of `N` request slots, one per possible descriptor head.
pub struct Requests<const N: usize> {
    slots: [Slot; N],
}

impl<const N: usize> Requests<N> {
    pub const fn new() -> Self {
        Self { slots: [Slot { state: State::Free, generation: 0 }; N] }
    }

    /// Marks `head` pending and returns a token stamped with its generation.
    pub fn issue(&mut self, head: u16) -> Token {
        let slot = &mut self.slots[head as usize];
        slot.state = State::Pending;
        Token { head, generation: slot.generation }
    }

    /// Gives up on `token`'s request, leaving its head reserved until the
    /// device returns it. Returns whether the token still named a live request.
    pub fn cancel(&mut self, token: Token) -> bool {
        let slot = &mut self.slots[token.head as usize];
        if slot.state == State::Pending && slot.generation == token.generation {
            slot.state = State::Cancelled;
            true
        } else {
            false
        }
    }

    /// Resolves a completion for `head`, freeing its slot and bumping its
    /// generation so any outstanding token for it can no longer match.
    pub fn complete(&mut self, head: u16) -> Completion {
        let slot = &mut self.slots[head as usize];
        let outcome = match slot.state {
            State::Pending => Completion::Delivered,
            State::Free | State::Cancelled => Completion::Stale,
        };
        slot.state = State::Free;
        slot.generation = slot.generation.wrapping_add(1);
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::{Completion, Requests};

    #[test]
    fn a_live_request_completes_and_frees_its_slot() {
        let mut requests = Requests::<4>::new();
        requests.issue(1);

        let outcome = requests.complete(1);

        assert_eq!(outcome, Completion::Delivered);
        assert_eq!(requests.complete(1), Completion::Stale, "a freed slot stayed live");
    }

    #[test]
    fn a_cancelled_requests_completion_is_stale() {
        let mut requests = Requests::<4>::new();
        let token = requests.issue(2);

        assert!(requests.cancel(token), "cancel disowned a live request");
        assert_eq!(requests.complete(2), Completion::Stale);
    }

    #[test]
    fn a_reused_head_rejects_the_old_token() {
        let mut requests = Requests::<4>::new();
        let stale = requests.issue(3);
        requests.complete(3);

        requests.issue(3);

        assert!(!requests.cancel(stale), "the old token cancelled a fresh request");
        assert_eq!(requests.complete(3), Completion::Delivered, "the fresh request was lost");
    }
}
