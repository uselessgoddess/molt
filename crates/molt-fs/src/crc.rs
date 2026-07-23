//! crc32c, the checksum every block on a volume carries.
//!
//! Castagnoli rather than the zlib polynomial because it is the one hardware
//! implements — `crc32` on x86_64, `crc32c` on aarch64 — so the software loop
//! here is the portable fallback for a checksum the format can keep.

/// The reflected Castagnoli polynomial.
const POLY: u32 = 0x82f6_3b78;

/// A crc32c accumulated over any number of pieces.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Crc(u32);

impl Crc {
    pub const fn new() -> Self {
        Self(!0)
    }

    pub fn update(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 ^= byte as u32;
            for _ in 0..8 {
                let carry = self.0 & 1;
                self.0 >>= 1;
                self.0 ^= POLY & carry.wrapping_neg();
            }
        }
    }

    pub const fn finish(self) -> u32 {
        !self.0
    }
}

impl Default for Crc {
    fn default() -> Self {
        Self::new()
    }
}

/// The crc32c of one contiguous piece.
pub fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = Crc::new();
    crc.update(bytes);
    crc.finish()
}

#[cfg(test)]
mod tests {
    use super::{Crc, crc32c};

    #[test]
    fn check_value_matches_castagnoli() {
        assert_eq!(crc32c(b"123456789"), 0xe306_9283, "not the crc32c check value");
    }

    #[test]
    fn empty_input_hashes_to_zero() {
        assert_eq!(crc32c(b""), 0);
    }

    #[test]
    fn pieces_hash_as_whole() {
        let mut crc = Crc::new();
        crc.update(b"1234");
        crc.update(b"56789");

        assert_eq!(crc.finish(), crc32c(b"123456789"), "a split update changed the digest");
    }

    #[test]
    fn single_bit_flip_changes_digest() {
        assert_ne!(crc32c(&[0; 64]), crc32c(&[1; 64]));
    }
}
