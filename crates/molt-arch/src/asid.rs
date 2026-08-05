//! Address space identifiers: the tag a TLB keeps two views of the same
//! address apart by, and what it costs when the hardware runs out of them.
//!
//! A tier-2 domain is a view of the one global address space, so the addresses
//! in two domains' translations collide by design. The tag is what stops one
//! domain's cached translation from answering the other's load. RISC-V calls it
//! an ASID, x86-64 a PCID, ARM an ASID again; the field is narrow — sixteen bits
//! at most on RV64, twelve on x86-64 — and a hart may implement fewer.
//!
//! Tags are spent by *domains*, not by mappings: granting or revoking a page
//! changes a view in place, which costs a shootdown (see
//! [`va::Epoch`](crate::va::Epoch)) and no tag at all. So the budget question is
//! how many domains exist at once, not how busy they are.
//!
//! When the numbers do run out, [`Asids`] does what Linux does on arm64: it
//! bumps a generation instead of hunting for a free number, and every tag from
//! the previous generation stops being [`live`](Asids::live) at once. The cost
//! is one flush of every hart's TLB per wrap, paid by whoever wrapped it.

/// The tag space of one machine.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Asids {
    width: u32,
    next: u32,
    generation: u64,
    assigned: u64,
    rollovers: u64,
}

/// A tag a domain holds, and the generation it was handed out in.
///
/// The generation is what makes a stale tag detectable: after a wrap the number
/// alone says nothing about who owns the entries cached under it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Asid {
    value: u16,
    generation: u64,
}

impl Asid {
    /// What the kernel's own translations are tagged with, and the value a hart
    /// with no tag support has to use for everything.
    pub const KERNEL: Self = Self { value: 0, generation: 0 };

    pub const fn value(self) -> u16 {
        self.value
    }

    pub const fn generation(self) -> u64 {
        self.generation
    }
}

/// What a hart has to flush before it may use a freshly assigned tag.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Flush {
    /// Nothing: no other domain has held this number in this generation.
    Nothing,
    /// Every hart's translations for every tag, because the numbers wrapped and
    /// entries cached under this one belong to a domain that has since lost it.
    Everything,
}

/// A tag, and the price of using it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use = "the flush a tag demands is what keeps the domains apart"]
pub struct Grant {
    asid: Asid,
    flush: Flush,
}

impl Grant {
    pub const fn asid(self) -> Asid {
        self.asid
    }

    pub const fn flush(self) -> Flush {
        self.flush
    }
}

impl Asids {
    /// The tag space of a hart that implements `width` tag bits.
    ///
    /// A width of zero is a real answer, not a broken one: RISC-V allows
    /// `ASIDLEN` to be zero, and such a hart shares one tag between everybody.
    pub const fn new(width: u32) -> Self {
        Self {
            width: if width > 16 { 16 } else { width },
            next: 1,
            generation: 1,
            assigned: 0,
            rollovers: 0,
        }
    }

    /// How many domains can hold a tag between two wraps.
    ///
    /// Tag zero is the kernel's, so a sixteen-bit field is 65 535 domains and a
    /// nine-bit one is 511.
    pub const fn capacity(self) -> u32 {
        (1u32 << self.width) - 1
    }

    /// Hands out the next tag, and says what using it costs.
    pub fn assign(&mut self) -> Grant {
        self.assigned += 1;
        if self.next > self.capacity() {
            // Nothing is searched for and nothing is reclaimed: the generation
            // moves, which retires every outstanding tag at once.
            self.generation += 1;
            self.rollovers += 1;
            // A hart with no tag bits has only the kernel's number to give, and
            // gives it again every time — which is why it pays every time.
            let value = if self.capacity() == 0 { 0 } else { 1 };
            self.next = value as u32 + 1;
            return Grant {
                asid: Asid { value, generation: self.generation },
                flush: Flush::Everything,
            };
        }
        let value = self.next as u16;
        self.next += 1;
        Grant { asid: Asid { value, generation: self.generation }, flush: Flush::Nothing }
    }

    /// Whether a tag a domain still holds means what it did when it was handed
    /// out, or has been retired by a wrap since.
    pub const fn live(&self, asid: Asid) -> bool {
        asid.generation == self.generation
    }

    /// The width the hart reported, as far as this kernel will use it.
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// How many tags have been handed out, wraps included.
    pub const fn assigned(&self) -> u64 {
        self.assigned
    }

    /// How many times the numbers have wrapped, which is how many global
    /// flushes the tag space has cost.
    pub const fn rollovers(&self) -> u64 {
        self.rollovers
    }
}

#[cfg(test)]
mod tests {
    use super::{Asid, Asids, Flush};

    /// What RV64 allows at most, and what QEMU's `virt` hart reports.
    const RISCV: u32 = 16;

    #[test]
    fn every_domain_gets_a_tag_of_its_own() {
        let mut asids = Asids::new(RISCV);

        let first = asids.assign();
        let second = asids.assign();

        assert_ne!(first.asid().value(), second.asid().value());
        assert_eq!(first.flush(), Flush::Nothing, "an unused number needed a flush");
        assert_eq!(second.flush(), Flush::Nothing);
    }

    #[test]
    fn the_kernel_keeps_tag_zero() {
        let mut asids = Asids::new(RISCV);

        for _ in 0..64 {
            assert_ne!(asids.assign().asid(), Asid::KERNEL, "a domain was handed the kernel's tag");
        }
        assert_eq!(Asid::KERNEL.value(), 0);
    }

    #[test]
    fn a_sixteen_bit_field_holds_sixty_five_thousand_domains() {
        assert_eq!(Asids::new(16).capacity(), 65_535);
        assert_eq!(Asids::new(9).capacity(), 511);
        assert_eq!(Asids::new(0).capacity(), 0, "a hart with no tag bits has no tags to give");
    }

    #[test]
    fn running_out_of_numbers_costs_one_flush() {
        let mut asids = Asids::new(4);
        let capacity = asids.capacity();

        let flushes = (0..capacity).filter(|_| asids.assign().flush() == Flush::Everything).count();
        let wrapped = asids.assign();

        assert_eq!(flushes, 0, "a tag nobody held asked for a flush");
        assert_eq!(wrapped.flush(), Flush::Everything, "the wrap reused a number for free");
        assert_eq!(asids.rollovers(), 1);
    }

    #[test]
    fn a_tag_from_before_the_wrap_is_not_live() {
        let mut asids = Asids::new(4);
        let old = asids.assign().asid();

        while asids.rollovers() == 0 {
            let _ = asids.assign();
        }

        let fresh = asids.assign().asid();

        assert!(!asids.live(old), "a domain kept a tag another domain now holds");
        assert!(asids.live(fresh), "a fresh tag was born stale");
    }

    #[test]
    fn a_hart_without_tag_bits_flushes_on_every_switch() {
        let mut asids = Asids::new(0);

        let grants = [asids.assign(), asids.assign(), asids.assign()];

        assert!(
            grants.iter().all(|grant| grant.flush() == Flush::Everything),
            "an untagged hart reused a translation from another domain"
        );
        assert!(grants.iter().all(|grant| grant.asid().value() == 0));
        assert_eq!(asids.rollovers(), 3);
    }

    #[test]
    fn a_width_wider_than_the_field_is_taken_as_the_field() {
        assert_eq!(Asids::new(64).width(), 16, "a bad probe must not widen the tag field");
    }
}
