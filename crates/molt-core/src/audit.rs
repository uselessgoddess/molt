//! A bounded, in-order record of capability authority changes.
//!
//! A grant returns its handle and a revoke its count, but handing a copy of a
//! capability on leaves no trace in the table: the delegate holds a value the
//! delegator could keep too, and nothing says who passed what to whom. The log
//! is that trace. It lives in a fixed ring so a busy cell cannot exhaust it, and
//! counts what it overwrote so a full log reads as lossy rather than short.

use crate::CellId;
use crate::capability::Rights;

/// Which authority change an [`Event`] records.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    /// A resource entered the table under a new owner.
    Grant,
    /// A holder handed a copy on, possibly with fewer rights.
    Delegate,
    /// A capability was dropped, staling every name for it.
    Revoke,
}

/// One authority change: who acted, on which slot, with what rights.
///
/// `holder` is the cell left holding the capability — the delegate for a
/// [`Delegate`](Action::Delegate), and `actor` itself otherwise.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Event {
    pub actor: CellId,
    pub holder: CellId,
    pub resource: u32,
    pub rights: Rights,
    pub action: Action,
}

impl Event {
    pub const fn grant(owner: CellId, resource: u32, rights: Rights) -> Self {
        Self { actor: owner, holder: owner, resource, rights, action: Action::Grant }
    }

    pub const fn delegate(from: CellId, to: CellId, resource: u32, rights: Rights) -> Self {
        Self { actor: from, holder: to, resource, rights, action: Action::Delegate }
    }

    pub const fn revoke(owner: CellId, resource: u32, rights: Rights) -> Self {
        Self { actor: owner, holder: owner, resource, rights, action: Action::Revoke }
    }
}

/// Where authority changes are recorded, so a caller can log without naming the
/// ring's capacity.
pub trait Audit {
    fn record(&mut self, event: Event);
}

/// A fixed-capacity ring of the most recent [`Event`]s.
pub struct Log<const N: usize> {
    events: [Option<Event>; N],
    len: usize,
    next: usize,
    dropped: usize,
}

impl<const N: usize> Log<N> {
    pub const fn new() -> Self {
        const { assert!(N > 0, "an audit log needs at least one slot") };
        Self { events: [None; N], len: 0, next: 0, dropped: 0 }
    }

    /// Appends `event`, overwriting the oldest once the ring is full.
    pub fn record(&mut self, event: Event) {
        if self.events[self.next].is_some() {
            self.dropped += 1;
        } else {
            self.len += 1;
        }
        self.events[self.next] = Some(event);
        self.next = (self.next + 1) % N;
    }

    /// How many events the log currently holds, up to `N`.
    pub const fn len(&self) -> usize {
        self.len
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// How many events fell off the ring's tail, unseen.
    pub const fn dropped(&self) -> usize {
        self.dropped
    }

    /// The most recent event, or `None` when nothing has been recorded.
    pub fn last(&self) -> Option<Event> {
        if self.len == 0 {
            return None;
        }
        self.events[(self.next + N - 1) % N]
    }

    /// The retained events, oldest first.
    pub fn iter(&self) -> impl Iterator<Item = Event> + '_ {
        let start = if self.len == N { self.next } else { 0 };
        (0..self.len).map(move |offset| self.events[(start + offset) % N].unwrap())
    }
}

impl<const N: usize> Audit for Log<N> {
    fn record(&mut self, event: Event) {
        Log::record(self, event);
    }
}

impl<const N: usize> Default for Log<N> {
    fn default() -> Self {
        Self::new()
    }
}
