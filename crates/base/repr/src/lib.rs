#![no_std]

//! What a typed value looks like as bytes, and how to reach one inside them.
//!
//! Molt writes its own layouts rather than borrowing one — a superblock, a
//! B-tree node, a log record, the `repr(C)` descriptors in `molt_abi` — so the
//! reading and writing of those bytes belongs somewhere shared rather than
//! being reinvented per format. This is that place. Today it holds the part
//! every one of them needs: an integer at an offset, checked where the check
//! belongs.
//!
//! A parser reads a field at an offset, and the only interesting question is
//! who knows the bound. Every layout Molt reads — a superblock, a B-tree node,
//! a log record, an ELF header — is a window whose size is a constant with
//! offsets that are constants beside it, so the bound is a fact the compiler
//! already holds. [`Field`] makes it say so: a field that runs past its window
//! fails the build and names the call site, and what survives into the binary
//! is a load.
//!
//! The other shape — a checked read handing back an `Option` — moves that same
//! fact to run time, where it becomes a branch no input can reach. In a parser
//! of hostile bytes those are worse than noise: a fuzzer cannot cover them, and
//! they read exactly like the branches it must.
//!
//! Narrowing a slice to a window is the one step that can genuinely fail, and
//! `slice::first_chunk` already spells it. A table of fixed-size records is the
//! same step repeated, which is [`records`].

use core::{iter, mem};

/// An integer with a fixed byte representation.
pub trait Word: private::Sealed + Copy {
    /// How wide the representation is.
    const BYTES: usize;

    #[doc(hidden)]
    fn read_le(bytes: &[u8]) -> Option<Self>;
    #[doc(hidden)]
    fn read_be(bytes: &[u8]) -> Option<Self>;
    #[doc(hidden)]
    fn write_le(self, bytes: &mut [u8]) -> Option<()>;
    #[doc(hidden)]
    fn write_be(self, bytes: &mut [u8]) -> Option<()>;
}

/// Reads at a constant offset in a window of constant size.
///
/// Both halves of the bound are generic parameters, so `AT + T::BYTES <= N` is
/// checked once at compile time and never again.
pub trait Field<const N: usize> {
    fn at_le<T: Word, const AT: usize>(&self) -> T;
    fn field_be<T: Word, const AT: usize>(&self) -> T;
}

/// Writes at a constant offset in a window of constant size.
pub trait FieldMut<const N: usize> {
    fn put_le<T: Word, const AT: usize>(&mut self, value: T);
    fn put_be<T: Word, const AT: usize>(&mut self, value: T);
}

impl<const N: usize> Field<N> for [u8; N] {
    fn at_le<T: Word, const AT: usize>(&self) -> T {
        const { assert!(AT + T::BYTES <= N, "a field that runs past its window") }
        match T::read_le(&self[AT..AT + T::BYTES]) {
            Some(value) => value,
            None => unreachable!(),
        }
    }

    fn field_be<T: Word, const AT: usize>(&self) -> T {
        const { assert!(AT + T::BYTES <= N, "a field that runs past its window") }
        match T::read_be(&self[AT..AT + T::BYTES]) {
            Some(value) => value,
            None => unreachable!(),
        }
    }
}

impl<const N: usize> FieldMut<N> for [u8; N] {
    fn put_le<T: Word, const AT: usize>(&mut self, value: T) {
        const { assert!(AT + T::BYTES <= N, "a field that runs past its window") }
        match value.write_le(&mut self[AT..AT + T::BYTES]) {
            Some(()) => (),
            None => unreachable!(),
        }
    }

    fn put_be<T: Word, const AT: usize>(&mut self, value: T) {
        const { assert!(AT + T::BYTES <= N, "a field that runs past its window") }
        match value.write_be(&mut self[AT..AT + T::BYTES]) {
            Some(()) => (),
            None => unreachable!(),
        }
    }
}

/// The windows a table of fixed-size records is made of.
///
/// Stopping is what a slice too short for another record means, so the walk is
/// total: there is no bound left for a caller to check, and none to get wrong.
/// A tail shorter than a record is not yielded, which is why callers pair this
/// with the count they expect rather than trusting the length they were given.
pub fn records<const N: usize>(bytes: &[u8]) -> impl Iterator<Item = &[u8; N]> {
    let mut rest = bytes;
    iter::from_fn(move || {
        let (record, tail) = rest.split_first_chunk::<N>()?;
        rest = tail;
        Some(record)
    })
}

/// [`records`], for a table being written.
pub fn records_mut<const N: usize>(bytes: &mut [u8]) -> impl Iterator<Item = &mut [u8; N]> {
    let mut rest = bytes;
    iter::from_fn(move || {
        let (record, tail) = mem::take(&mut rest).split_first_chunk_mut::<N>()?;
        rest = tail;
        Some(record)
    })
}

mod private {
    pub trait Sealed {}
}

macro_rules! word {
    ($type:ty, $bytes:literal) => {
        impl private::Sealed for $type {}

        impl Word for $type {
            const BYTES: usize = $bytes;

            fn read_le(bytes: &[u8]) -> Option<Self> {
                Some(Self::from_le_bytes(bytes.try_into().ok()?))
            }

            fn read_be(bytes: &[u8]) -> Option<Self> {
                Some(Self::from_be_bytes(bytes.try_into().ok()?))
            }

            fn write_le(self, bytes: &mut [u8]) -> Option<()> {
                let bytes: &mut [u8; $bytes] = bytes.try_into().ok()?;
                *bytes = self.to_le_bytes();
                Some(())
            }

            fn write_be(self, bytes: &mut [u8]) -> Option<()> {
                let bytes: &mut [u8; $bytes] = bytes.try_into().ok()?;
                *bytes = self.to_be_bytes();
                Some(())
            }
        }
    };
}

word!(u16, 2);
word!(u32, 4);
word!(u64, 8);

#[cfg(test)]
mod tests {
    use super::{Field, FieldMut};

    #[test]
    fn reads_each_word_width() {
        let bytes = [1, 2, 3, 4, 5, 6, 7, 8];

        assert_eq!(bytes.at_le::<u16, 1>(), 0x0302);
        assert_eq!(bytes.at_le::<u32, 1>(), 0x0504_0302);
        assert_eq!(bytes.at_le::<u64, 0>(), 0x0807_0605_0403_0201);
    }

    #[test]
    fn reads_both_byte_orders() {
        let bytes = [0x12, 0x34, 0x56, 0x78];

        assert_eq!(bytes.at_le::<u32, 0>(), 0x7856_3412);
        assert_eq!(bytes.field_be::<u32, 0>(), 0x1234_5678);
    }

    #[test]
    fn writes_both_byte_orders() {
        let mut bytes = [0; 8];

        bytes.put_le::<u16, 1>(0x1234);
        bytes.put_be::<u16, 4>(0x5678);

        assert_eq!(bytes, [0, 0x34, 0x12, 0, 0x56, 0x78, 0, 0]);
    }

    #[test]
    fn last_field_fits() {
        let mut bytes = [0; 8];

        bytes.put_le::<u32, 4>(0x1234_5678);

        assert_eq!(bytes.at_le::<u32, 4>(), 0x1234_5678);
        assert_eq!(bytes, [0, 0, 0, 0, 0x78, 0x56, 0x34, 0x12]);
    }

    #[test]
    fn window_is_narrowed_once() {
        let bytes = [1, 2, 3, 4, 5, 6];

        let head: Option<&[u8; 4]> = bytes.first_chunk();
        assert_eq!(head.map(Field::at_le::<u32, 0>), Some(0x0403_0201));

        let short = &bytes[..3];
        assert_eq!(short.first_chunk::<4>(), None);
    }

    #[test]
    fn table_walks_whole_records() {
        let bytes = [1, 0, 2, 0, 3];

        let mut table = super::records::<2>(&bytes);

        assert_eq!(table.next().map(Field::at_le::<u16, 0>), Some(1));
        assert_eq!(table.next().map(Field::at_le::<u16, 0>), Some(2));
        assert!(table.next().is_none(), "the odd byte at the end was read as a record");
    }

    #[test]
    fn table_written_record() {
        let mut bytes = [0; 5];

        for (record, value) in super::records_mut::<2>(&mut bytes).zip([1u16, 2]) {
            record.put_le::<u16, 0>(value);
        }

        assert_eq!(bytes, [1, 0, 2, 0, 0]);
    }
}
