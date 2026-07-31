//! crc32c, the checksum every block on a volume carries.
//!
//! Castagnoli rather than the zlib polynomial because it is the one hardware
//! implements. Every block read verifies, so the checksum sits on the read path
//! and is worth the instruction: `crc32` on x86_64 folds eight bytes per
//! instruction against the eight loads a table costs, and its operands are
//! general registers, so a kernel that never enables SSE for its own code can
//! still issue it. The table is the portable fallback and the answer both paths
//! agree on.
//!
//! No crate does this for `no_std`: `crc32c` and `crc-fast` reach the
//! instruction through `is_x86_feature_detected!`, which is `std`, and drag a
//! build script behind it; `crc` is table-only. Detection here is one `cpuid`
//! leaf, cached.

/// The reflected Castagnoli polynomial.
const POLY: u32 = 0x82f6_3b78;

/// Bytes the table folds per round.
const SLICES: usize = 8;

/// Slice-by-eight: `TABLE[n][b]` is the residue of byte `b` shifted `n` places.
static TABLE: [[u32; 256]; SLICES] = table();

const fn table() -> [[u32; 256]; SLICES] {
    let mut table = [[0; 256]; SLICES];
    let mut byte = 0;
    while byte < 256 {
        let mut residue = byte as u32;
        let mut bit = 0;
        while bit < 8 {
            residue = (residue >> 1) ^ (POLY & (residue & 1).wrapping_neg());
            bit += 1;
        }
        table[0][byte] = residue;
        byte += 1;
    }
    let mut slice = 1;
    while slice < SLICES {
        let mut byte = 0;
        while byte < 256 {
            let shorter = table[slice - 1][byte];
            table[slice][byte] = (shorter >> 8) ^ table[0][(shorter & 0xff) as usize];
            byte += 1;
        }
        slice += 1;
    }
    table
}

/// A crc32c accumulated over any number of pieces.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Crc(u32);

impl Crc {
    pub const fn new() -> Self {
        Self(!0)
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.0 = fold(self.0, bytes);
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

#[cfg(target_arch = "x86_64")]
fn fold(crc: u32, bytes: &[u8]) -> u32 {
    if intrinsic::present() {
        // SAFETY: the CPU reports SSE4.2, which is all the function asks.
        return unsafe { intrinsic::fold(crc, bytes) };
    }
    sliced(crc, bytes)
}

#[cfg(not(target_arch = "x86_64"))]
fn fold(crc: u32, bytes: &[u8]) -> u32 {
    sliced(crc, bytes)
}

/// Folds eight bytes a round through the table, then the tail a byte at a time.
fn sliced(mut crc: u32, bytes: &[u8]) -> u32 {
    let mut rounds = bytes.chunks_exact(SLICES);
    for round in &mut rounds {
        let low = u32::from_le_bytes([round[0], round[1], round[2], round[3]]) ^ crc;
        let high = u32::from_le_bytes([round[4], round[5], round[6], round[7]]);
        crc = TABLE[7][(low & 0xff) as usize]
            ^ TABLE[6][(low >> 8 & 0xff) as usize]
            ^ TABLE[5][(low >> 16 & 0xff) as usize]
            ^ TABLE[4][(low >> 24) as usize]
            ^ TABLE[3][(high & 0xff) as usize]
            ^ TABLE[2][(high >> 8 & 0xff) as usize]
            ^ TABLE[1][(high >> 16 & 0xff) as usize]
            ^ TABLE[0][(high >> 24) as usize];
    }
    for &byte in rounds.remainder() {
        crc = (crc >> 8) ^ TABLE[0][((crc ^ byte as u32) & 0xff) as usize];
    }
    crc
}

#[cfg(target_arch = "x86_64")]
mod intrinsic {
    use core::arch::x86_64::{__cpuid, _mm_crc32_u8, _mm_crc32_u64};
    use core::sync::atomic::{AtomicU8, Ordering};

    /// What [`present`] remembers, zero standing for "not asked yet".
    const UNKNOWN: u8 = 0;
    const ABSENT: u8 = 1;
    const HERE: u8 = 2;

    /// `CPUID.01H:ECX.SSE4_2`.
    const SSE42: u32 = 1 << 20;

    static ANSWER: AtomicU8 = AtomicU8::new(UNKNOWN);

    /// Whether this CPU has the instruction, asked once and remembered.
    ///
    /// Racing callers ask twice and store the same answer, which is cheaper
    /// than the lock that would stop them.
    pub fn present() -> bool {
        if cfg!(target_feature = "sse4.2") {
            return true;
        }
        match ANSWER.load(Ordering::Relaxed) {
            HERE => true,
            ABSENT => false,
            _ => {
                // Leaf 1 is defined on every CPU that reaches long mode.
                let here = __cpuid(1).ecx & SSE42 != 0;
                ANSWER.store(if here { HERE } else { ABSENT }, Ordering::Relaxed);
                here
            }
        }
    }

    /// Folds `bytes` into `crc` eight at a time.
    ///
    /// # Safety
    ///
    /// The CPU must have SSE4.2, which [`present`] answers.
    #[target_feature(enable = "sse4.2")]
    pub unsafe fn fold(crc: u32, bytes: &[u8]) -> u32 {
        let mut rounds = bytes.chunks_exact(8);
        let mut wide = crc as u64;
        for round in &mut rounds {
            wide = _mm_crc32_u64(wide, u64::from_le_bytes(round.try_into().unwrap()));
        }
        let mut crc = wide as u32;
        for &byte in rounds.remainder() {
            crc = _mm_crc32_u8(crc, byte);
        }
        crc
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::{Crc, POLY, crc32c, sliced};

    /// Lengths that land on a round boundary, short of one, and past one.
    const LENGTHS: [usize; 8] = [0, 1, 7, 8, 9, 63, 4096, 4101];

    fn run(len: usize) -> Vec<u8> {
        (0..len).map(|at| (at * 31 + 7) as u8).collect()
    }

    /// The definition, one bit at a time.
    fn bitwise(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &byte in bytes {
            crc ^= byte as u32;
            for _ in 0..8 {
                crc = (crc >> 1) ^ (POLY & (crc & 1).wrapping_neg());
            }
        }
        !crc
    }

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

    #[test]
    fn table_answers_as_bitwise() {
        let bytes = run(LENGTHS[LENGTHS.len() - 1]);

        for len in LENGTHS {
            assert_eq!(!sliced(!0, &bytes[..len]), bitwise(&bytes[..len]), "parted at {len}");
        }
    }

    #[test]
    fn dispatch_answers_as_table() {
        let bytes = run(LENGTHS[LENGTHS.len() - 1]);

        for len in LENGTHS {
            assert_eq!(crc32c(&bytes[..len]), !sliced(!0, &bytes[..len]), "parted at {len}");
        }
    }
}

