//! Heap that is allowed to say no.
//!
//! `Box::new`, `vec!`, and `Rc::from` end in `handle_alloc_error`, which in a
//! kernel is a panic taking the whole machine down over one four-kilobyte node.
//! A filesystem is a service that already returns errors, so an allocation it
//! cannot get is [`FsError::Memory`] and the caller rolls back like it would for
//! a refused write. These are the `try_` constructors with that mapping applied
//! once, so no call site has to remember which form it wanted.
//!
//! `format` is the exception and stays infallible: `mkfs` builds an image on a
//! host with an operating system under it, and it is behind a feature the kernel
//! does not turn on.

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::vec::Vec;
use core::ops::{Deref, DerefMut};

use crate::FsError;

/// A type whose all-zero bytes are already a value of it.
///
/// # Safety
///
/// Implementors must be inhabited by zeroes: integers and arrays of them, no
/// references, no enums that lack a zero variant.
pub(crate) unsafe trait Zeroed {}

// SAFETY: any byte pattern is a `u8`, so a zeroed array is an array of zeroes.
unsafe impl<const N: usize> Zeroed for [u8; N] {}

// SAFETY: zero is a `u64`.
unsafe impl Zeroed for u64 {}

/// A zeroed `T` on the heap, allocated without going through the stack first.
pub(crate) fn zeroed<T: Zeroed>() -> Result<Box<T>, FsError> {
    let value = Box::try_new_zeroed()?;
    // SAFETY: `Zeroed` is the promise that zeroes are a `T`.
    Ok(unsafe { value.assume_init() })
}

/// `len` zeroed `T`s on the heap.
pub(crate) fn zeroed_slice<T: Zeroed>(len: usize) -> Result<Box<[T]>, FsError> {
    let values = Box::try_new_zeroed_slice(len)?;
    // SAFETY: as above, for every element.
    Ok(unsafe { values.assume_init() })
}

/// Appends `value`, growing the vector only if the heap agrees.
pub(crate) fn push<T>(values: &mut Vec<T>, value: T) -> Result<(), FsError> {
    values.try_reserve(1)?;
    values.push(value);
    Ok(())
}

/// Appends `tail`, growing the vector only if the heap agrees.
pub(crate) fn extend<T: Copy>(values: &mut Vec<T>, tail: &[T]) -> Result<(), FsError> {
    values.try_reserve(tail.len())?;
    values.extend_from_slice(tail);
    Ok(())
}

/// A shared value no one else holds yet, so it can still be written through.
///
/// Nodes are built field by field and only then handed to the cache, but they
/// are shared once they get there. Building them in a `Box` and converting
/// would allocate the four kilobytes twice and copy between them, and
/// `Rc::try_new(value)` would build the node on the stack a mount is measured
/// against — so the allocation is an [`Rc`] from the start, and this is the
/// window while it is still the only handle.
pub(crate) struct Unique<T>(Rc<T>);

impl<T: Zeroed> Unique<T> {
    /// A zeroed `T` allocated straight into its handle.
    pub fn zeroed() -> Result<Self, FsError> {
        let value = Rc::try_new_zeroed()?;
        // SAFETY: `Zeroed` is the promise that zeroes are a `T`.
        Ok(Self(unsafe { value.assume_init() }))
    }
}

impl<T> Unique<T> {
    /// Gives up the exclusive half and hands out the shared handle.
    pub fn into_shared(self) -> Rc<T> {
        self.0
    }
}

impl<T> Deref for Unique<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> DerefMut for Unique<T> {
    fn deref_mut(&mut self) -> &mut T {
        Rc::get_mut(&mut self.0).expect("a unique handle is the only one")
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::{Unique, extend, push, zeroed, zeroed_slice};
    use crate::FsError;

    #[test]
    fn zeroed_box_is_zero() -> Result<(), FsError> {
        assert!(zeroed::<[u8; 64]>()?.iter().all(|&byte| byte == 0));
        assert_eq!(*zeroed_slice::<u64>(3)?, [0, 0, 0]);
        Ok(())
    }

    #[test]
    fn unique_writes_before_sharing() -> Result<(), FsError> {
        let mut value = Unique::<u64>::zeroed()?;
        *value = 7;

        assert_eq!(*value.into_shared(), 7);
        Ok(())
    }

    #[test]
    fn vectors_grow_through_reserve() -> Result<(), FsError> {
        let mut values = Vec::new();
        push(&mut values, 1u64)?;
        extend(&mut values, &[2, 3])?;

        assert_eq!(values, [1, 2, 3]);
        Ok(())
    }
}
