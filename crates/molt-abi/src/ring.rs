//! Ring for untrusted shared memory.
//!
//! Six rules, from [`docs/threat-model.md`](../../../docs/threat-model.md), and
//! this module is where four of them live:
//!
//! 1. The consumer's index is kernel-private. [`Submissions`] carries its own
//!    `head` and never reads the shared one as truth; the shared copy exists
//!    for the producer's backpressure, and a domain that corrupts it starves
//!    itself.
//! 2. The producer's index is validated, not trusted. Read the tail once, and
//!    `tail - head` must be at most the ring's length. Anything else is a
//!    [`Fault`] rather than a panic, because a domain must not be able to take
//!    the kernel down by lying.
//! 3. The payload has no invalid bit pattern. A slot is words, and the kernel
//!    parses them; there is no `assume_init_read` on shared memory here or
//!    anywhere, and a parse that can fail is the point.
//! 4. Read once. [`Call::parse`] takes its words by value, so nothing the
//!    kernel decided with can be re-read after the decision — the double-fetch
//!    is closed by construction and not by care.
//!
//! Rule 5, the single masked range check against the submitting capability's
//! extent, is [`Region::fits`] to decide and [`Region::within`] to carry the
//! decision into the value ([`nospec`]). Rule 6 — the rings live in the
//! domain's own extent, so what the kernel writes back is memory the domain
//! could have written itself — is a placement decision the caller makes, not
//! something a type can hold.
//!
//! [`nospec`]: crate::nospec
//! [`Region::fits`]: crate::wire::Region::fits
//! [`Region::within`]: crate::wire::Region::within

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::wire::{Call, Reject, Reply, SLOT_WORDS};

/// Shared rings. Domain can write every byte concurrently.
#[derive(Debug)]
#[repr(C)]
pub struct Channel<const N: usize> {
    submissions: Ring<N>,
    completions: Ring<N>,
}

impl<const N: usize> Channel<N> {
    pub const fn new() -> Self {
        const {
            assert!(N.is_power_of_two(), "a ring's length must be a power of two");
        }

        Self { submissions: Ring::new(), completions: Ring::new() }
    }

    /// Kernel's part of both rings.
    pub const fn kernel(&self) -> (Submissions<'_, N>, Completions<'_, N>) {
        (
            Submissions { ring: &self.submissions, head: 0, fault: None },
            Completions { ring: &self.completions, tail: 0 },
        )
    }

    /// Domain's part.
    pub const fn domain(&self) -> Domain<'_, N> {
        Domain { channel: self, tail: 0, head: 0 }
    }
}

impl<const N: usize> Default for Channel<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared memory: indices + slots.
#[derive(Debug)]
#[repr(C, align(64))]
struct Ring<const N: usize> {
    /// Producer index (read once per drain).
    tail: AtomicU32,
    /// Consumer index (real one is private).
    head: AtomicU32,
    _reserved: [u64; 7],
    slots: [Slot; N],
}

impl<const N: usize> Ring<N> {
    const fn new() -> Self {
        Self {
            tail: AtomicU32::new(0),
            head: AtomicU32::new(0),
            _reserved: [0; 7],
            slots: [const { Slot::new() }; N],
        }
    }

    fn read(&self, index: u32) -> [u64; SLOT_WORDS] {
        // No `nospec` here, and none needed: the length is a power of two, so
        // the `&` is unconditional and every index — speculated or taken — is
        // already inside the slots.
        let slot = &self.slots[index as usize & (N - 1)];
        core::array::from_fn(|word| slot.0[word].load(Ordering::Acquire))
    }

    fn write(&self, index: u32, words: [u64; SLOT_WORDS]) {
        let slot = &self.slots[index as usize & (N - 1)];
        for (word, value) in words.into_iter().enumerate() {
            slot.0[word].store(value, Ordering::Release);
        }
    }
}

#[derive(Debug)]
#[repr(C)]
struct Slot([AtomicU64; SLOT_WORDS]);

impl Slot {
    const fn new() -> Self {
        Self([const { AtomicU64::new(0) }; SLOT_WORDS])
    }
}

/// Kind of`fault(kind, pc)` - the kernel stops driving the ring
/// [`Reject`] is the other case,
/// where the ring is honest and one submission was not.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum Fault {
    /// `tail - head > N`, producer claimed more slots than exist.
    Tail = 1,
}

/// Result of draining the submission ring.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Next {
    Empty,
    Ready(Call),
    Rejected { id: u64, reject: Reject },
}

/// Who is on the other end of a ring, which is what decides whether the words
/// it publishes are facts or inputs.
///
/// Sealed: there are two answers and a caller cannot invent a third, because
/// the third would be a ring the kernel validates halfway.
pub trait Peer: sealed::Sealed {
    /// Whether the far end may lie about what it wrote.
    const HOSTILE: bool;
}

/// The far end is code compiled with this one — the kernel's own rings, or the
/// kernel as a domain sees it.
pub enum Trusted {}

/// The far end is a domain's memory: every word is an input, and the indices
/// are claims to check rather than counts to use.
pub enum Hostile {}

impl Peer for Trusted {
    const HOSTILE: bool = false;
}

impl Peer for Hostile {
    const HOSTILE: bool = true;
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for super::Trusted {}
    impl Sealed for super::Hostile {}
}

/// A ring end that reads what the other end published, and names who that is.
///
/// The associated peer is the point of the trait. A function that drives a
/// domain writes `R: Reader<Peer = Hostile>`, and the in-kernel rings of
/// `molt_core::ring` cannot reach it: they implement nothing here, because
/// their far end is kernel code and their indices are read without checking.
/// The mistake that costs the most — a trusted ring wired to a sandbox — stops
/// being a thing to remember and becomes a type that does not fit.
pub trait Reader {
    /// What one read yields.
    type Item;

    /// Who wrote it.
    type Peer: Peer;

    /// Reads the next item, or faults when the far end lied about its index.
    fn take(&mut self) -> Result<Self::Item, Fault>;
}

/// Kernel's submission reader. Holds the private `head`.
#[derive(Debug)]
pub struct Submissions<'ring, const N: usize> {
    ring: &'ring Ring<N>,
    head: u32,
    fault: Option<Fault>,
}

impl<const N: usize> Submissions<'_, N> {
    /// Take next submission. Permanent `Fault` on protocol violation.
    pub fn take(&mut self) -> Result<Next, Fault> {
        if let Some(fault) = self.fault {
            return Err(fault);
        }

        let published = self.ring.tail.load(Ordering::Acquire);
        let ready = published.wrapping_sub(self.head);
        if ready == 0 {
            return Ok(Next::Empty);
        }
        if ready as usize > N {
            self.fault = Some(Fault::Tail);
            return Err(Fault::Tail);
        }

        let words = self.ring.read(self.head);
        self.head = self.head.wrapping_add(1);
        // publish *after* copy -> prevent TOCTOU
        self.ring.head.store(self.head, Ordering::Release);

        Ok(match Call::parse(words) {
            Ok(call) => Next::Ready(call),
            Err(reject) => Next::Rejected { id: words[0], reject },
        })
    }

    pub const fn fault(&self) -> Option<Fault> {
        self.fault
    }

    /// Kernel `head` counter.
    pub const fn taken(&self) -> u32 {
        self.head
    }
}

impl<const N: usize> Reader for Submissions<'_, N> {
    type Item = Next;
    type Peer = Hostile;

    fn take(&mut self) -> Result<Next, Fault> {
        Submissions::take(self)
    }
}

/// Kernel's completion writer.
#[derive(Debug)]
pub struct Completions<'ring, const N: usize> {
    ring: &'ring Ring<N>,
    tail: u32,
}

impl<const N: usize> Completions<'_, N> {
    /// Publish completion, returns `Err(reply)` if full.
    pub fn publish(&mut self, reply: Reply) -> Result<(), Reply> {
        let taken = self.ring.head.load(Ordering::Acquire);
        if self.tail.wrapping_sub(taken) as usize >= N {
            return Err(reply);
        }

        self.ring.write(self.tail, reply.encode());
        self.tail = self.tail.wrapping_add(1);
        self.ring.tail.store(self.tail, Ordering::Release);
        Ok(())
    }
}

/// Domain's part, no validation, used in tests to simulate lying domains.
#[derive(Debug)]
pub struct Domain<'ring, const N: usize> {
    channel: &'ring Channel<N>,
    tail: u32,
    head: u32,
}

impl<const N: usize> Domain<'_, N> {
    pub fn submit(&mut self, call: Call) {
        self.write(call.encode());
    }

    pub fn write(&mut self, words: [u64; SLOT_WORDS]) {
        self.channel.submissions.write(self.tail, words);
        self.tail = self.tail.wrapping_add(1);
        self.channel.submissions.tail.store(self.tail, Ordering::Release);
    }

    /// Publishes a tail without writing the slots it claims.
    pub fn claim(&mut self, ahead: u32) {
        self.tail = self.tail.wrapping_add(ahead);
        self.channel.submissions.tail.store(self.tail, Ordering::Release);
    }

    /// Publishes a head it never earned.
    pub fn consumed(&mut self, ahead: u32) {
        self.head = self.head.wrapping_add(ahead);
        self.channel.completions.head.store(self.head, Ordering::Release);
    }

    /// Takes a completion, trusting the kernel end as the domain has to.
    pub fn reply(&mut self) -> Option<Reply> {
        if self.channel.completions.tail.load(Ordering::Acquire) == self.head {
            return None;
        }

        let words = self.channel.completions.read(self.head);
        self.head = self.head.wrapping_add(1);
        self.channel.completions.head.store(self.head, Ordering::Release);
        Some(Reply::decode(words))
    }
}

impl<const N: usize> Reader for Domain<'_, N> {
    type Item = Option<Reply>;
    type Peer = Trusted;

    /// Never `Err`: the kernel does not publish a tail it did not write, which
    /// is the whole difference between the two ends of this channel.
    fn take(&mut self) -> Result<Option<Reply>, Fault> {
        Ok(self.reply())
    }
}

#[cfg(test)]
mod tests {
    use core::mem::{align_of, offset_of, size_of};

    use super::{Channel, Ring, Slot};
    use crate::wire::SLOT_BYTES;

    #[test]
    fn layout() {
        assert_eq!((size_of::<Slot>(), align_of::<Slot>()), (SLOT_BYTES, 8));
        assert_eq!(offset_of!(Ring<4>, tail), 0);
        assert_eq!(offset_of!(Ring<4>, head), 4);

        assert_eq!(offset_of!(Ring<4>, slots), 64, "the slots share a line with the indices");
        assert_eq!(size_of::<Ring<4>>(), 64 + 4 * SLOT_BYTES);
        assert_eq!(offset_of!(Channel<4>, completions), size_of::<Ring<4>>());
    }
}
