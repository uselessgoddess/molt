//! Registered buffers used by capability-based I/O operations.

use crate::capability::{Capability, CapabilityError, CapabilityTable, Read, ReadWrite, Write};
use crate::cell::CellId;

/// An I/O buffer range represented entirely by a typed capability and bounds.
///
/// The operation intentionally contains no address. Only the supervisor-owned
/// registry can turn it into a slice for a trusted driver.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BufferOperation<R> {
    buffer: Capability<R>,
    offset: usize,
    len: usize,
}

impl<R> BufferOperation<R> {
    pub const fn new(buffer: Capability<R>, offset: usize, len: usize) -> Self {
        Self { buffer, offset, len }
    }

    pub const fn capability(self) -> Capability<R> {
        self.buffer
    }

    pub const fn offset(self) -> usize {
        self.offset
    }

    pub const fn len(self) -> usize {
        self.len
    }

    pub const fn is_empty(self) -> bool {
        self.len == 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BufferError {
    Capability(CapabilityError),
    OutOfBounds,
}

impl From<CapabilityError> for BufferError {
    fn from(error: CapabilityError) -> Self {
        Self::Capability(error)
    }
}

struct RegisteredBuffer<'buffer> {
    bytes: &'buffer mut [u8],
}

/// Supervisor-owned registry for memory that drivers may use for I/O.
pub struct BufferRegistry<'buffer, const N: usize> {
    table: CapabilityTable<RegisteredBuffer<'buffer>, N>,
}

impl<'buffer, const N: usize> BufferRegistry<'buffer, N> {
    pub const fn new() -> Self {
        Self { table: CapabilityTable::new() }
    }

    pub fn register_read(
        &mut self,
        owner: CellId,
        bytes: &'buffer mut [u8],
    ) -> Result<Capability<Read>, &'buffer mut [u8]> {
        self.register(owner, bytes)
    }

    pub fn register_write(
        &mut self,
        owner: CellId,
        bytes: &'buffer mut [u8],
    ) -> Result<Capability<Write>, &'buffer mut [u8]> {
        self.register(owner, bytes)
    }

    pub fn register_read_write(
        &mut self,
        owner: CellId,
        bytes: &'buffer mut [u8],
    ) -> Result<Capability<ReadWrite>, &'buffer mut [u8]> {
        self.register(owner, bytes)
    }

    pub fn read_capability(
        &self,
        capability: Capability<ReadWrite>,
    ) -> Result<Capability<Read>, CapabilityError> {
        self.table.attenuate(capability)
    }

    pub fn write_capability(
        &self,
        capability: Capability<ReadWrite>,
    ) -> Result<Capability<Write>, CapabilityError> {
        self.table.attenuate(capability)
    }

    pub fn resolve_read(&self, operation: BufferOperation<Read>) -> Result<&[u8], BufferError> {
        let buffer = self.table.get(operation.buffer)?;
        range(buffer.bytes, operation.offset, operation.len)
    }

    pub fn resolve_write(
        &mut self,
        operation: BufferOperation<Write>,
    ) -> Result<&mut [u8], BufferError> {
        let buffer = self.table.get_mut(operation.buffer)?;
        range_mut(buffer.bytes, operation.offset, operation.len)
    }

    pub fn revoke_owner(&mut self, owner: CellId) -> usize {
        self.table.revoke_owner(owner)
    }

    fn register<R: crate::capability::CapabilityRights>(
        &mut self,
        owner: CellId,
        bytes: &'buffer mut [u8],
    ) -> Result<Capability<R>, &'buffer mut [u8]> {
        match self.table.insert(owner, RegisteredBuffer { bytes }) {
            Ok(capability) => Ok(capability),
            Err(buffer) => Err(buffer.bytes),
        }
    }
}

impl<const N: usize> Default for BufferRegistry<'_, N> {
    fn default() -> Self {
        Self::new()
    }
}

fn range(bytes: &[u8], offset: usize, len: usize) -> Result<&[u8], BufferError> {
    let end = offset.checked_add(len).ok_or(BufferError::OutOfBounds)?;
    bytes.get(offset..end).ok_or(BufferError::OutOfBounds)
}

fn range_mut(bytes: &mut [u8], offset: usize, len: usize) -> Result<&mut [u8], BufferError> {
    let end = offset.checked_add(len).ok_or(BufferError::OutOfBounds)?;
    bytes.get_mut(offset..end).ok_or(BufferError::OutOfBounds)
}
