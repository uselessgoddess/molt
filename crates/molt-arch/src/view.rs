//! What a domain can see of the one address space.
//!
//! Molt has one address space and no processes, so a domain is not a private
//! map from its own addresses onto memory: an address means the same thing
//! everywhere, or it means nothing anywhere. A domain is a *view* — the same
//! addresses, and fewer of them present.
//!
//! That is what makes a grant cheap. Handing a hundred gigabytes to a domain
//! copies no bytes and relocates no pointer, because the pointer already names
//! the right place; all that is missing is the leaf that makes the place
//! reachable from there. `docs/address-space.md` calls this tier 2, and this
//! module is the part of it the hardware performs.
//!
//! A view is a page-table root and a tag. The root starts empty — the kernel is
//! not in it, which is the entire claim tier 2 makes — and gains exactly what
//! is granted into it, at the leaf size the extent was cut for.
//!
//! # The order a revoke goes in
//!
//! Taking an extent back is three steps, and this module performs only the
//! first:
//!
//! 1. [`revoke`](crate::Platform::revoke) clears the leaves, so a walk of the
//!    view no longer finds the address.
//! 2. Every core flushes, tracked by [`shootdown`](crate::shootdown) — a core
//!    that cached the leaf before step 1 still translates through it.
//! 3. Only then may the addresses go back to the allocator, by
//!    [`retire`](crate::va::Space::retire)ing the epoch they were swept into.
//!
//! Doing step 3 before step 2 is a use-after-free that the memory management
//! unit carries out on behalf of whoever gets the addresses next. Nothing here
//! can enforce the order across all three, because the allocator and the
//! shootdown tracker are the caller's; what it can do is refuse to pretend the
//! flush is part of the unmap, which is why [`revoke`](crate::Platform::revoke)
//! returns the epoch's worth of nothing and leaves the rest to the caller.

use crate::asid::Asid;

/// How many views one platform keeps roots for.
///
/// A ceiling rather than a budget: the domain budget is the tag width the
/// hardware reported, which is thousands on any machine with ASIDs, and this is
/// only how many roots a port stores inline. Running out is
/// [`Error::Capacity`], not corruption.
pub const VIEWS: usize = 4;

/// What can go wrong naming or filling a view.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// No room for another root.
    Capacity,
    /// A view that was never opened, or has been closed.
    Unknown,
    /// The physical span does not cover the extent, or is not aligned to the
    /// leaf size the extent's class asks for.
    Backing,
    /// The extent is not mapped in this view, so there is nothing to revoke.
    Absent,
}

/// One domain's view of the one address space.
///
/// The tag travels with the identity because they are switched together: the
/// tag is what lets the hardware keep this view's translations cached across a
/// switch to another, and a root without one is a flush on every entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct View {
    index: u16,
    asid: Asid,
}

impl View {
    /// Names the `index`th view a platform holds, tagged `asid`.
    ///
    /// Called by the port that keeps the roots, which is the only thing that
    /// knows an index is real.
    pub const fn new(index: u16, asid: Asid) -> Self {
        Self { index, asid }
    }

    /// Which of the platform's roots this is.
    pub const fn index(self) -> u16 {
        self.index
    }

    /// The tag the hardware caches this view's translations under.
    pub const fn asid(self) -> Asid {
        self.asid
    }
}
