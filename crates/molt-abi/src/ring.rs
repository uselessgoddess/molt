//! The ring a domain the kernel does not trust is allowed to write.
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
//! extent, is [`Region::within`]. Rule 6 — the rings live in the domain's own
//! extent, so what the kernel writes back is memory the domain could have
//! written itself — is a placement decision the caller makes, not something a
//! type can hold.
//!
//! There is no `unsafe` in this module, which is the shape of the claim: the
//! hostile end can write every byte of a [`Channel`] at any time, and the worst
//! it reaches is a `Fault`.
//!
//! [`Region::within`]: crate::wire::Region::within

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::wire::{Call, Reject, Reply, SLOT_WORDS};

/// The two rings one domain and the kernel share.
///
/// It lives in the domain's own extent, so every byte here is writable by the
/// domain whenever it likes — including between two of the kernel's own
/// instructions.
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

    /// The kernel's end of both rings.
    ///
    /// One per channel: the private head that makes rule 1 work lives in the
    /// returned [`Submissions`], and a second one would be a second consumer.
    pub const fn kernel(&self) -> (Submissions<'_, N>, Completions<'_, N>) {
        (
            Submissions { ring: &self.submissions, head: 0, fault: None },
            Completions { ring: &self.completions, tail: 0 },
        )
    }

    /// The other end, as the domain has it.
    pub const fn domain(&self) -> Domain<'_, N> {
        Domain { channel: self, tail: 0, head: 0 }
    }
}

impl<const N: usize> Default for Channel<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// One ring's shared memory: an index each, and the slots between them.
#[derive(Debug)]
#[repr(C, align(64))]
struct Ring<const N: usize> {
    /// Written by whichever end produces, and read exactly once per drain.
    tail: AtomicU32,
    /// Written by whichever end consumes, for the producer's backpressure. The
    /// consumer keeps the real one privately.
    head: AtomicU32,
    /// The rest of the line the indices sit on. Explicit because this ABI has
    /// no implicit padding, and there at all because the slots start on the
    /// next line: the producer stores `tail` while the consumer reads a slot,
    /// and sharing a line would make every submission a coherence round trip.
    reserved: [u64; 7],
    slots: [Slot; N],
}

impl<const N: usize> Ring<N> {
    const fn new() -> Self {
        Self {
            tail: AtomicU32::new(0),
            head: AtomicU32::new(0),
            reserved: [0; 7],
            slots: [const { Slot::new() }; N],
        }
    }

    /// Copies a slot out, once.
    ///
    /// The index is the consumer's own counter reduced modulo a power of two,
    /// so it is in range as arithmetic rather than as a checked claim, and
    /// there is no branch for speculation to take the wrong way.
    fn read(&self, index: u32) -> [u64; SLOT_WORDS] {
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

/// One slot, as words with no invalid bit pattern.
///
/// Atomics and not a `[u8; SLOT_BYTES]` behind a cell: the other end writes
/// while this end reads, by design, so every access is a race unless the type
/// says otherwise. Saying otherwise costs nothing here — these are relaxed
/// loads of plain integers on both ports — and it is what lets the whole
/// module be safe code.
#[derive(Debug)]
#[repr(C)]
struct Slot([AtomicU64; SLOT_WORDS]);

impl Slot {
    const fn new() -> Self {
        Self([const { AtomicU64::new(0) }; SLOT_WORDS])
    }
}

/// A protocol violation the ring cannot attribute to one submission.
///
/// This is the `kind` of `fault(kind, pc)`: the kernel stops driving the ring
/// and the domain is the one that dies. A [`Reject`] is the other case, where
/// the ring is honest and one submission was not.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum Fault {
    /// A tail further ahead of the consumer than the ring is long — more
    /// submissions than there are slots to have written them in, so at least
    /// one was never written at all.
    Tail = 1,
}

/// What one drain of the submission ring found.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Next {
    /// Nothing published since the last drain.
    Empty,
    Ready(Call),
    /// A slot that parsed to nothing this kernel runs. The id is whatever the
    /// slot held, so the domain gets a completion rather than silence.
    Rejected {
        id: u64,
        reject: Reject,
    },
}

/// The kernel's end of the submission ring.
///
/// The `head` here is the one that counts, and it is in kernel memory. The
/// shared copy is published after each slot is taken, which is what the
/// producer waits on when the ring fills.
#[derive(Debug)]
pub struct Submissions<'ring, const N: usize> {
    ring: &'ring Ring<N>,
    head: u32,
    fault: Option<Fault>,
}

impl<const N: usize> Submissions<'_, N> {
    /// Takes the next submission, or says why there will not be another one.
    ///
    /// A fault is permanent: the ring has proven it cannot be read as a queue,
    /// so every later call answers the same way without touching shared memory
    /// again.
    pub fn take(&mut self) -> Result<Next, Fault> {
        if let Some(fault) = self.fault {
            return Err(fault);
        }

        // Once. Everything below is decided from this number, and the producer
        // is free to move the shared one in the meantime.
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
        // Published after the copy, so a producer that reuses the slot the
        // instant it sees room rewrites bytes the kernel is no longer reading.
        self.ring.head.store(self.head, Ordering::Release);

        Ok(match Call::parse(words) {
            Ok(call) => Next::Ready(call),
            Err(reject) => Next::Rejected { id: words[0], reject },
        })
    }

    /// The fault this ring died of, if it has.
    pub const fn fault(&self) -> Option<Fault> {
        self.fault
    }

    /// How many submissions the kernel has taken, which is the private head
    /// itself and not what the shared word says.
    pub const fn taken(&self) -> u32 {
        self.head
    }
}

/// The kernel's end of the completion ring.
#[derive(Debug)]
pub struct Completions<'ring, const N: usize> {
    ring: &'ring Ring<N>,
    tail: u32,
}

impl<const N: usize> Completions<'_, N> {
    /// Publishes a completion, handing it back when the ring is full.
    ///
    /// The domain's head is advisory here, and a corrupted one is not a fault:
    /// a head claiming more is outstanding than the ring holds reads as full,
    /// so the domain stalls itself and reaches nobody else. That is rule 6 paying
    /// off — everything written back is the domain's own memory.
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

/// The other end of a [`Channel`], as the domain has it.
///
/// Nothing here validates anything, which is the point: it is the side the
/// kernel has no say over, so tests and the boot smoke use it to play a domain
/// — including a domain that lies about what it has written.
#[derive(Debug)]
pub struct Domain<'ring, const N: usize> {
    channel: &'ring Channel<N>,
    tail: u32,
    head: u32,
}

impl<const N: usize> Domain<'_, N> {
    /// Writes a submission and publishes the tail it earns.
    pub fn submit(&mut self, call: Call) {
        self.write(call.encode());
    }

    /// Writes raw words, for a submission no [`Call`] can express.
    pub fn write(&mut self, words: [u64; SLOT_WORDS]) {
        self.channel.submissions.write(self.tail, words);
        self.tail = self.tail.wrapping_add(1);
        self.channel.submissions.tail.store(self.tail, Ordering::Release);
    }

    /// Publishes a tail without writing the slots it claims.
    ///
    /// This is the attack rule 2 exists for, and it is a method rather than a
    /// test helper because the kernel's answer to it is a property worth
    /// keeping a caller for.
    pub fn claim(&mut self, ahead: u32) {
        self.tail = self.tail.wrapping_add(ahead);
        self.channel.submissions.tail.store(self.tail, Ordering::Release);
    }

    /// Publishes a head it never earned, corrupting its own backpressure.
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

#[cfg(test)]
mod tests {
    use core::mem::{align_of, offset_of, size_of};

    use super::{Channel, Ring, Slot};
    use crate::wire::SLOT_BYTES;

    #[test]
    fn layout_is_the_one_both_ends_were_built_against() {
        assert_eq!((size_of::<Slot>(), align_of::<Slot>()), (SLOT_BYTES, 8));
        assert_eq!(offset_of!(Ring<4>, tail), 0);
        assert_eq!(offset_of!(Ring<4>, head), 4);

        assert_eq!(offset_of!(Ring<4>, slots), 64, "the slots share a line with the indices");
        assert_eq!(size_of::<Ring<4>>(), 64 + 4 * SLOT_BYTES);
        assert_eq!(offset_of!(Channel<4>, completions), size_of::<Ring<4>>());
    }
}
