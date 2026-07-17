//! Generation-checked, typed authority handles.

use core::marker::PhantomData;

pub use crate::cell::CellId;

/// Runtime rights stored beside a resource in the supervisor-owned table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rights(u8);

impl Rights {
    pub const READ: Self = Self(1 << 0);
    pub const WRITE: Self = Self(1 << 1);
    pub const READ_WRITE: Self = Self(Self::READ.0 | Self::WRITE.0);

    const fn contains(self, requested: Self) -> bool {
        self.0 & requested.0 == requested.0
    }
}

pub trait CapabilityRights {
    const MASK: Rights;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Read {}

impl Read {
    pub const MASK: Rights = Rights::READ;
}

impl CapabilityRights for Read {
    const MASK: Rights = Self::MASK;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Write {}

impl Write {
    pub const MASK: Rights = Rights::WRITE;
}

impl CapabilityRights for Write {
    const MASK: Rights = Self::MASK;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadWrite {}

impl ReadWrite {
    pub const MASK: Rights = Rights::READ_WRITE;
}

impl CapabilityRights for ReadWrite {
    const MASK: Rights = Self::MASK;
}

/// A table index and generation pair that safe code cannot forge.
#[derive(Debug, Eq, PartialEq)]
#[repr(transparent)]
pub struct Capability<R> {
    raw: u64,
    rights: PhantomData<fn() -> R>,
}

impl<R> Capability<R> {
    const fn new(index: u32, generation: u32) -> Self {
        Self { raw: index as u64 | ((generation as u64) << 32), rights: PhantomData }
    }

    pub const fn raw(self) -> u64 {
        self.raw
    }

    const fn index(self) -> usize {
        self.raw as u32 as usize
    }

    const fn generation(self) -> u32 {
        (self.raw >> 32) as u32
    }
}

impl<R> Clone for Capability<R> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<R> Copy for Capability<R> {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapabilityError {
    Invalid,
    Stale,
    InsufficientRights,
}

struct Slot<T> {
    generation: u32,
    owner: CellId,
    rights: Rights,
    resource: Option<T>,
}

impl<T> Slot<T> {
    const fn empty() -> Self {
        Self { generation: 1, owner: CellId::new(0), rights: Rights(0), resource: None }
    }

    fn advance_generation(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        if self.generation == 0 {
            self.generation = 1;
        }
    }
}

/// A fixed-size, supervisor-owned resource table.
pub struct CapabilityTable<T, const N: usize> {
    slots: [Slot<T>; N],
}

impl<T, const N: usize> CapabilityTable<T, N> {
    pub const fn new() -> Self {
        Self { slots: [const { Slot::empty() }; N] }
    }

    pub fn insert<R: CapabilityRights>(
        &mut self,
        owner: CellId,
        resource: T,
    ) -> Result<Capability<R>, T> {
        let Some((index, slot)) =
            self.slots.iter_mut().enumerate().find(|(_, slot)| slot.resource.is_none())
        else {
            return Err(resource);
        };
        slot.owner = owner;
        slot.rights = R::MASK;
        slot.resource = Some(resource);
        Ok(Capability::new(index as u32, slot.generation))
    }

    pub fn attenuate<From: CapabilityRights, To: CapabilityRights>(
        &self,
        capability: Capability<From>,
    ) -> Result<Capability<To>, CapabilityError> {
        let slot = self.validate(capability)?;
        if !From::MASK.contains(To::MASK) || !slot.rights.contains(To::MASK) {
            return Err(CapabilityError::InsufficientRights);
        }
        Ok(Capability { raw: capability.raw, rights: PhantomData })
    }

    pub fn get<R: CapabilityRights>(
        &self,
        capability: Capability<R>,
    ) -> Result<&T, CapabilityError> {
        self.validate(capability)?.resource.as_ref().ok_or(CapabilityError::Stale)
    }

    pub fn get_mut<R: CapabilityRights>(
        &mut self,
        capability: Capability<R>,
    ) -> Result<&mut T, CapabilityError> {
        let index = capability.index();
        let generation = capability.generation();
        let slot = self.slots.get_mut(index).ok_or(CapabilityError::Invalid)?;
        if slot.generation != generation || slot.resource.is_none() {
            return Err(CapabilityError::Stale);
        }
        if !slot.rights.contains(R::MASK) {
            return Err(CapabilityError::InsufficientRights);
        }
        slot.resource.as_mut().ok_or(CapabilityError::Stale)
    }

    pub fn revoke_owner(&mut self, owner: CellId) -> usize {
        let mut revoked = 0;
        for slot in &mut self.slots {
            if slot.resource.is_some() && slot.owner == owner {
                drop(slot.resource.take());
                slot.rights = Rights(0);
                slot.advance_generation();
                revoked += 1;
            }
        }
        revoked
    }

    fn validate<R: CapabilityRights>(
        &self,
        capability: Capability<R>,
    ) -> Result<&Slot<T>, CapabilityError> {
        let slot = self.slots.get(capability.index()).ok_or(CapabilityError::Invalid)?;
        if slot.generation != capability.generation() || slot.resource.is_none() {
            return Err(CapabilityError::Stale);
        }
        if !slot.rights.contains(R::MASK) {
            return Err(CapabilityError::InsufficientRights);
        }
        Ok(slot)
    }
}

impl<T, const N: usize> Default for CapabilityTable<T, N> {
    fn default() -> Self {
        Self::new()
    }
}
