//! SBI error codes, decoded away from the `ecall` that produces them.
//!
//! This is the half of the interface that has nothing to do with RISC-V
//! registers, so it stays outside the target-gated modules and can be tested
//! on the host.

/// What an SBI implementation reported in `a0`.
///
/// An unrecognised code keeps its raw value rather than collapsing into a
/// generic failure: the reason for moving off the legacy console call was that
/// it discards exactly this information.
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
    /// Decodes one `a0` value. `SBI_SUCCESS` (zero) is not an error.
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
    fn success_is_not_an_error() {
        assert_eq!(SbiError::from_code(0), None);
    }

    #[test]
    fn standard_codes_decode_to_named_errors() {
        assert_eq!(SbiError::from_code(-2), Some(SbiError::NotSupported));
        assert_eq!(SbiError::from_code(-5), Some(SbiError::InvalidAddress));
    }

    #[test]
    fn unknown_codes_keep_their_value() {
        assert_eq!(
            SbiError::from_code(-42),
            Some(SbiError::Other(-42)),
            "a code this build does not know is still worth reporting exactly"
        );
    }
}
