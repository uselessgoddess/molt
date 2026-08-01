//! Which cell answers for a kind of service.
//!
//! A capability says what its holder may do; it does not say how a cell that
//! holds nothing gets the first one. Everywhere else in the kernel the answer
//! is delegation — somebody who already has the authority hands part of it over
//! — and one place has to break that circle or init would keep wiring clients
//! to services by hand.
//!
//! This is that place, and it is deliberately the only one. A provider
//! publishes an endpoint under a [`Scheme`], a client asks the scheme for it,
//! and what comes back is a capability with the same generation check as every
//! other: withdrawing a publication leaves every lease on it
//! [`CapabilityError::Stale`], which is how a client learns the service behind
//! it restarted. The scheme is a type rather than a string, so a lease on
//! storage cannot be passed where one on the network belongs, and a typo is a
//! compile error instead of a lookup that finds nothing.
//!
//! A publication also names the core that answers it. Cores share nothing, so
//! reaching a service is reaching the core that owns it, and a client that
//! learns the name without the core would have to ask somebody else where to
//! send — which is the shared table this design is spent avoiding.

use crate::capability::{Capability, CapabilityError, CapabilityRights, CapabilityTable};
use crate::{CellId, CpuId};

/// A kind of service, named by a type rather than by a path.
///
/// The rights the scheme carries are the rights its endpoint is published with:
/// a client acquires the authority the provider chose to offer and no more.
pub trait Scheme: CapabilityRights {
    /// What acquiring one hands the holder.
    type Endpoint;

    /// What a boot log calls this scheme.
    const NAME: &'static str;
}

/// Why a registry refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistryError {
    /// Nobody answers for this scheme, either yet or any more.
    Unavailable,
    /// No free slot for another publication.
    Full,
}

/// The published endpoints of one scheme.
///
/// A lease and the publication are the same name here: a client acquires
/// exactly what the provider offered, because the rights are the scheme's and
/// there is nothing per-client to narrow. Handing a lease on with fewer rights
/// is [`CapabilityTable::delegate`], which the generation check that ends a
/// lease already backs.
pub struct Registry<S: Scheme, const N: usize> {
    published: CapabilityTable<Published<S::Endpoint>, N>,
}

/// An endpoint and the core it is served on.
struct Published<E> {
    cpu: CpuId,
    endpoint: E,
}

impl<S: Scheme, const N: usize> Registry<S, N> {
    pub const fn new() -> Self {
        Self { published: CapabilityTable::new() }
    }

    /// Offers `endpoint`, served on `cpu`, on `provider`'s behalf.
    ///
    /// The capability returned is the publication itself, so a provider that
    /// keeps it can reach its own endpoint without acquiring one.
    pub fn publish(
        &mut self,
        provider: CellId,
        cpu: CpuId,
        endpoint: S::Endpoint,
    ) -> Result<Capability<S>, RegistryError> {
        self.published
            .insert::<S>(provider, Published { cpu, endpoint })
            .map_err(|_| RegistryError::Full)
    }

    /// Names the endpoint that answers for this scheme.
    ///
    /// The lease is only as good as the publication behind it: a client holds
    /// it across calls and finds out that the provider restarted when
    /// [`endpoint`](Self::endpoint) answers [`CapabilityError::Stale`].
    pub fn acquire(&self) -> Result<Capability<S>, RegistryError> {
        self.published.first::<S>().ok_or(RegistryError::Unavailable)
    }

    /// What a lease names, while it still names anything.
    pub fn endpoint(&self, lease: Capability<S>) -> Result<&S::Endpoint, CapabilityError> {
        self.published.get(lease).map(|published| &published.endpoint)
    }

    /// Which core answers this lease.
    ///
    /// The client sends there instead of calling: the endpoint belongs to that
    /// core's executor, and reaching it from another one is a message.
    pub fn home(&self, lease: Capability<S>) -> Result<CpuId, CapabilityError> {
        self.published.get(lease).map(|published| published.cpu)
    }

    /// Withdraws what `provider` published, leaving every lease on it stale.
    ///
    /// Returns how many publications went, which a restart reports rather than
    /// assumes: a provider that published nothing is a provider whose clients
    /// were talking to somebody else.
    pub fn withdraw(&mut self, provider: CellId) -> usize {
        self.published.revoke_owner(provider)
    }
}

impl<S: Scheme, const N: usize> Default for Registry<S, N> {
    fn default() -> Self {
        Self::new()
    }
}
