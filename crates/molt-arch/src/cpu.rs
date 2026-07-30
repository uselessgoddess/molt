//! The block a core reaches without being told which core it is.
//!
//! Every layer above this needs one thing from the hardware that nothing else
//! can give: a pointer the running core can read without a lock, an index, or a
//! search. It is one register — `gs` on x86_64, `tp` on RISC-V — and everything
//! per-core hangs off it: the executor to wake, the heap shard to free into,
//! the peer rings to drain.
//!
//! The pointer is untyped here on purpose. `molt-arch` owns the register, not
//! what a kernel decided to keep behind it, and a kernel that put the wrong
//! type there would not be helped by naming it once in a crate below.

/// A core's own pointer, held in whichever register the architecture reserves.
///
/// # Safety
///
/// An implementation must return exactly what was last installed on the core
/// that asks, and nothing of another core's. The register is per-core state, so
/// a preserved-across-context implementation is one that never migrates it.
pub unsafe trait Local {
    /// Points this core's register at `block`.
    ///
    /// # Safety
    ///
    /// `block` must outlive the core and belong to no other. Installing twice
    /// on one core leaks whatever the first pointer named.
    unsafe fn install(block: *mut ());

    /// What this core installed, or null before it installed anything.
    fn block() -> *mut ();
}
