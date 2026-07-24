//! Host-testable SBI error decoding.

/// Decoded SBI error, preserving unrecognised raw codes in [`Self::Other`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SbiError {
    Failed,
    NotSupported,
    InvalidParam,
    Denied,
    InvalidAddress,
    AlreadyAvailable,
    AlreadyStarted,
    AlreadyStopped,
    NoShmem,
    Other(isize),
}

impl SbiError {
    /// Decodes an `a0` error code, returning `None` for `SBI_SUCCESS`.
    pub const fn from_code(code: isize) -> Option<Self> {
        Some(match code {
            0 => return None,
            -1 => Self::Failed,
            -2 => Self::NotSupported,
            -3 => Self::InvalidParam,
            -4 => Self::Denied,
            -5 => Self::InvalidAddress,
            -6 => Self::AlreadyAvailable,
            -7 => Self::AlreadyStarted,
            -8 => Self::AlreadyStopped,
            -9 => Self::NoShmem,
            other => Self::Other(other),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::SbiError;

    #[test]
    fn success_is_not_error() {
        assert_eq!(SbiError::from_code(0), None);
    }

    #[test]
    fn standard_codes_decode_to_named_errors() {
        assert_eq!(SbiError::from_code(-2), Some(SbiError::NotSupported));
        assert_eq!(SbiError::from_code(-5), Some(SbiError::InvalidAddress));
    }

    #[test]
    fn unknown_codes_keep_value() {
        assert_eq!(SbiError::from_code(-42), Some(SbiError::Other(-42)),);
    }
}
