//! A directory entry's name, carried inline by value.
//!
//! An operation crossing a ring cannot hold a borrow, and registering a buffer
//! for every path is more ceremony than a name is worth, so a name is a fixed
//! [`layout::MAX_NAME`] bytes and travels in the operation itself.

use core::fmt;

use crate::{FsError, layout};

/// A bounded, inline entry name.
#[derive(Clone, Copy, Eq)]
pub struct Name {
    bytes: [u8; Self::MAX],
    len: u8,
}

impl Name {
    pub const MAX: usize = layout::MAX_NAME;

    /// Copies `bytes`, which must be no longer than [`Self::MAX`] and must not
    /// contain a path separator or a null.
    pub fn new(bytes: &[u8]) -> Result<Self, FsError> {
        if bytes.is_empty() || bytes.len() > Self::MAX {
            return Err(FsError::Name);
        }
        if bytes.iter().any(|&byte| byte == b'/' || byte == 0) {
            return Err(FsError::Name);
        }
        let mut name = Self { bytes: [0; Self::MAX], len: bytes.len() as u8 };
        name.bytes[..bytes.len()].copy_from_slice(bytes);
        Ok(name)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }

    /// The name as text, or `None` if the volume stored bytes that are not
    /// UTF-8.
    pub fn as_str(&self) -> Option<&str> {
        core::str::from_utf8(self.as_bytes()).ok()
    }

    pub const fn len(&self) -> usize {
        self.len as usize
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl PartialEq for Name {
    fn eq(&self, other: &Self) -> bool {
        self.as_bytes() == other.as_bytes()
    }
}

impl fmt::Debug for Name {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.as_str() {
            Some(text) => write!(formatter, "{text:?}"),
            None => write!(formatter, "{:?}", self.as_bytes()),
        }
    }
}

impl TryFrom<&str> for Name {
    type Error = FsError;

    fn try_from(text: &str) -> Result<Self, FsError> {
        Self::new(text.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::Name;
    use crate::FsError;
    use crate::layout::MAX_NAME;

    #[test]
    fn name_keeps_its_bytes() -> Result<(), FsError> {
        let name = Name::new(b"molt.txt")?;

        assert_eq!(name.as_str(), Some("molt.txt"));
        Ok(())
    }

    #[test]
    fn overlong_name_refused() {
        assert_eq!(Name::new(&[b'a'; MAX_NAME + 1]), Err(FsError::Name));
    }

    #[test]
    fn empty_name_refused() {
        assert_eq!(Name::new(b""), Err(FsError::Name));
    }

    #[test]
    fn separator_refused() {
        assert_eq!(Name::new(b"dir/file"), Err(FsError::Name));
    }

    #[test]
    fn padding_does_not_affect_equality() -> Result<(), FsError> {
        assert_eq!(Name::new(b"a")?, Name::new(b"a")?);
        assert_ne!(Name::new(b"a")?, Name::new(b"ab")?);
        Ok(())
    }
}
