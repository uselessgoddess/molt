//! Fan-in between cores: one ring per peer, never one queue for all of them.
//!
//! A shared inbox is a shared cache line, and a shared cache line is every core
//! in the machine taking turns at one address whether or not they have anything
//! to say to each other. A ring per sender costs `P` times the memory and none
//! of that: a sender touches its own ring, the owner drains them in turn, and a
//! full ring refuses that one sender instead of the service behind all of them.
//!
//! The rings are the [`SpscRing`] the I/O path already uses, so the contract is
//! the same one and it is still checked by the endpoints rather than by
//! convention: [`split`](Peers::split) hands out one [`Sender`] per peer and
//! one [`Inbox`], and neither can be cloned.

use crate::cpu::CpuId;
use crate::ring::SpscRing;

/// The inbox of one core, addressed by who is writing to it.
pub struct Peers<T, const N: usize, const P: usize> {
    owner: CpuId,
    rings: [SpscRing<T, N>; P],
}

impl<T, const N: usize, const P: usize> Peers<T, N, P> {
    #[cfg(not(loom))]
    pub const fn new(owner: CpuId) -> Self {
        Self { owner, rings: [const { SpscRing::new() }; P] }
    }

    #[cfg(loom)]
    pub fn new(owner: CpuId) -> Self {
        Self { owner, rings: core::array::from_fn(|_| SpscRing::new()) }
    }

    /// Whose inbox this is.
    pub const fn owner(&self) -> CpuId {
        self.owner
    }

    /// Divides the inbox into one sender per peer and the owner's end.
    ///
    /// A peer's index is its [`CpuId`], the owner's own slot included: a core
    /// that hands work to itself takes the same path as one that does not, so
    /// nothing on it needs to ask which case it is in.
    pub fn split(&mut self) -> ([Sender<'_, T, N, P>; P], Inbox<'_, T, N, P>) {
        (core::array::from_fn(|peer| Sender { peers: self, peer }), Inbox { peers: self, next: 0 })
    }
}

/// One peer's end: the only producer on the ring it names.
pub struct Sender<'p, T, const N: usize, const P: usize> {
    peers: &'p Peers<T, N, P>,
    peer: usize,
}

impl<T, const N: usize, const P: usize> Sender<'_, T, N, P> {
    /// Which core sends here.
    pub const fn peer(&self) -> CpuId {
        CpuId::new(self.peer as u16)
    }

    /// Which core reads it.
    pub const fn owner(&self) -> CpuId {
        self.peers.owner
    }

    /// Posts `message`, handing it back when this peer's ring is full.
    ///
    /// A full ring is back pressure on one sender: the owner is behind on this
    /// peer alone, and every other peer keeps its own room.
    pub fn post(&mut self, message: T) -> Result<(), T> {
        self.peers.rings[self.peer].try_push(message)
    }
}

/// The owner's end, which drains every peer in turn.
pub struct Inbox<'p, T, const N: usize, const P: usize> {
    peers: &'p Peers<T, N, P>,
    next: usize,
}

impl<T, const N: usize, const P: usize> Inbox<'_, T, N, P> {
    pub const fn owner(&self) -> CpuId {
        self.peers.owner
    }

    /// Takes one message, resuming after the peer the last one came from.
    ///
    /// Round-robin rather than in peer order, so a peer that always has
    /// something ready cannot keep the ones after it from being read.
    pub fn take(&mut self) -> Option<T> {
        for step in 0..P {
            let peer = (self.next + step) % P;
            if let Some(message) = self.peers.rings[peer].try_pop() {
                self.next = (peer + 1) % P;
                return Some(message);
            }
        }
        None
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::Peers;
    use crate::cpu::CpuId;

    #[test]
    fn peers_keep_own_room() {
        let mut peers = Peers::<u8, 1, 2>::new(CpuId::new(1));
        let ([mut first, mut second], mut inbox) = peers.split();

        assert_eq!(first.post(1), Ok(()));
        assert_eq!(first.post(2), Err(2), "a full ring took a second message");
        assert_eq!(second.post(3), Ok(()), "one full peer blocked another");
        assert_eq!(inbox.take(), Some(1));
    }

    #[test]
    fn drain_rotates() {
        let mut peers = Peers::<u8, 2, 2>::new(CpuId::BOOT);
        let ([mut first, mut second], mut inbox) = peers.split();
        first.post(1).unwrap();
        first.post(2).unwrap();
        second.post(3).unwrap();

        assert_eq!([inbox.take(), inbox.take()], [Some(1), Some(3)]);
    }

    #[test]
    fn empty_inbox_is_none() {
        let mut peers = Peers::<u8, 1, 1>::new(CpuId::BOOT);
        let (_senders, mut inbox) = peers.split();

        assert_eq!(inbox.take(), None);
    }
}
