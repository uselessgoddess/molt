#![no_std]

//! Checked integer fields in ordinary byte slices.

/// An integer that has a fixed byte representation.
pub trait Word: private::Sealed + Copy {
    const BYTES: usize;

    fn read_le(bytes: &[u8]) -> Option<Self>;
    fn read_be(bytes: &[u8]) -> Option<Self>;
    fn write_le(self, bytes: &mut [u8]) -> Option<()>;
    fn write_be(self, bytes: &mut [u8]) -> Option<()>;
}

/// Checked reads from an ordinary byte slice.
pub trait Bytes {
    fn read_le<T: Word>(&self, offset: usize) -> Option<T>;
    fn read_be<T: Word>(&self, offset: usize) -> Option<T>;
}

/// Checked writes to an ordinary byte slice.
pub trait BytesMut {
    fn write_le<T: Word>(&mut self, offset: usize, value: T) -> Option<()>;
    fn write_be<T: Word>(&mut self, offset: usize, value: T) -> Option<()>;
}

impl Bytes for [u8] {
    fn read_le<T: Word>(&self, offset: usize) -> Option<T> {
        let end = offset.checked_add(T::BYTES)?;
        T::read_le(self.get(offset..end)?)
    }

    fn read_be<T: Word>(&self, offset: usize) -> Option<T> {
        let end = offset.checked_add(T::BYTES)?;
        T::read_be(self.get(offset..end)?)
    }
}

impl BytesMut for [u8] {
    fn write_le<T: Word>(&mut self, offset: usize, value: T) -> Option<()> {
        let end = offset.checked_add(T::BYTES)?;
        value.write_le(self.get_mut(offset..end)?)
    }

    fn write_be<T: Word>(&mut self, offset: usize, value: T) -> Option<()> {
        let end = offset.checked_add(T::BYTES)?;
        value.write_be(self.get_mut(offset..end)?)
    }
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
                if bytes.len() != Self::BYTES {
                    return None;
                }
                bytes.copy_from_slice(&self.to_le_bytes());
                Some(())
            }

            fn write_be(self, bytes: &mut [u8]) -> Option<()> {
                if bytes.len() != Self::BYTES {
                    return None;
                }
                bytes.copy_from_slice(&self.to_be_bytes());
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
    use super::{Bytes, BytesMut};

    #[test]
    fn reads_each_word_width() {
        let bytes = [1, 2, 3, 4, 5, 6, 7, 8];

        assert_eq!(bytes.read_le::<u16>(1), Some(0x0302));
        assert_eq!(bytes.read_le::<u32>(1), Some(0x0504_0302));
        assert_eq!(bytes.read_le::<u64>(0), Some(0x0807_0605_0403_0201));
    }

    #[test]
    fn reads_both_byte_orders() {
        let bytes = [0x12, 0x34, 0x56, 0x78];

        assert_eq!(bytes.read_le::<u32>(0), Some(0x7856_3412));
        assert_eq!(bytes.read_be::<u32>(0), Some(0x1234_5678));
    }

    #[test]
    fn range_is_checked() {
        let bytes = [0; 4];

        assert_eq!(bytes.read_le::<u32>(1), None);
        assert_eq!(bytes.read_le::<u16>(usize::MAX), None);
    }

    #[test]
    fn writes_both_byte_orders() {
        let mut bytes = [0; 8];

        assert_eq!(bytes.write_le(1, 0x1234_u16), Some(()));
        assert_eq!(bytes.write_be(4, 0x5678_u16), Some(()));
        assert_eq!(bytes, [0, 0x34, 0x12, 0, 0x56, 0x78, 0, 0]);
    }

    #[test]
    fn write_range_is_checked() {
        let mut bytes = [0; 4];

        assert_eq!(bytes.write_le(2, 1_u32), None);
        assert_eq!(bytes.write_be(usize::MAX, 1_u16), None);
        assert_eq!(bytes, [0; 4]);
    }
}
