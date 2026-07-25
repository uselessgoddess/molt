//! A run of bits sized by whatever it has to track.
//!
//! The COW arena marks which metadata blocks a transaction has taken and which
//! an older checkpoint still needs. How many there are is a superblock field,
//! so the bits are allocated for that count instead of for a constant a kernel
//! stack would then have to carry twice per transaction.

use alloc::boxed::Box;
use alloc::vec;

/// Bits addressed by index. Indices past the end read false and write nowhere.
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct Bitmap {
    words: Box<[u64]>,
    bits: u32,
}

impl Bitmap {
    /// Zeroed map with room for `bits` indices.
    pub fn new(bits: u32) -> Self {
        Self { words: vec![0; bits.div_ceil(64) as usize].into_boxed_slice(), bits }
    }

    pub fn get(&self, at: u32) -> bool {
        self.words.get(at as usize / 64).is_some_and(|word| word & mask(at) != 0)
    }

    pub fn set(&mut self, at: u32) {
        if at < self.bits
            && let Some(word) = self.words.get_mut(at as usize / 64)
        {
            *word |= mask(at);
        }
    }

    pub fn clear(&mut self, at: u32) {
        if let Some(word) = self.words.get_mut(at as usize / 64) {
            *word &= !mask(at);
        }
    }

    /// Lowest index still zero, a word at a time.
    pub fn first_clear(&self) -> Option<u32> {
        self.words.iter().enumerate().find_map(|(index, word)| {
            let free = word.trailing_ones();
            let at = index as u32 * 64 + free;
            (free < 64 && at < self.bits).then_some(at)
        })
    }
}

const fn mask(at: u32) -> u64 {
    1 << (at % 64)
}

#[cfg(test)]
mod tests {
    use super::Bitmap;

    #[test]
    fn set_bit_reads_back() {
        let mut bits = Bitmap::new(100);
        bits.set(70);

        assert!(bits.get(70));
        assert!(!bits.get(69), "neighbour was set too");
    }

    #[test]
    fn first_clear_skips_full_words() {
        let mut bits = Bitmap::new(130);
        for at in 0..65 {
            bits.set(at);
        }

        assert_eq!(bits.first_clear(), Some(65));
    }

    #[test]
    fn cleared_bit_is_reused() {
        let mut bits = Bitmap::new(8);
        for at in 0..8 {
            bits.set(at);
        }
        assert_eq!(bits.first_clear(), None, "full map offered an index");

        bits.clear(3);

        assert_eq!(bits.first_clear(), Some(3));
    }

    #[test]
    fn tail_past_end_stays_unusable() {
        let mut bits = Bitmap::new(3);
        for at in 0..3 {
            bits.set(at);
        }
        bits.set(9);

        assert_eq!(bits.first_clear(), None, "map handed out index it does not hold");
        assert!(!bits.get(9));
    }
}
