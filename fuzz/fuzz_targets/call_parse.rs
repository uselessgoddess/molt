//! What a slot the domain wrote is allowed to parse into.
//!
//! [`Call::parse`] is the whole of what the kernel believes about a submission,
//! and the bytes it reads are entirely the other end's. So the target is not
//! "does it crash" alone: an accepted call has to be one the wire can carry
//! back unchanged, and its buffer has to lie inside the aperture, because
//! everything downstream is written as though both were already true.

#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use molt_abi::wire::{APERTURE, Call, SLOT_WORDS};

/// A slot, from either end of the distribution.
///
/// Bytes taken raw reach the refusals and nothing else: a tag byte is one of
/// ten out of four billion. The shaped variant puts the tag in its own field,
/// so the fuzzer reaches the arms behind it and can then spend its mutations on
/// the fields each arm reads.
#[derive(Arbitrary, Debug)]
enum Slot {
    Raw([u64; SLOT_WORDS]),
    Shaped { id: u64, tag: u8, first: u64, second: u64, third: u64 },
}

impl Slot {
    fn words(self) -> [u64; SLOT_WORDS] {
        match self {
            Self::Raw(words) => words,
            Self::Shaped { id, tag, first, second, third } => {
                [id, tag as u64, first, second, third, 0, 0, 0]
            }
        }
    }
}

fuzz_target!(|slot: Slot| {
    let words = slot.words();
    let Ok(call) = Call::parse(words) else { return };

    assert_eq!(call.id(), words[0], "the completion would answer a call nobody made");
    if let Some(buf) = call.op().region() {
        assert!(buf.fits(APERTURE), "an accepted call named a buffer outside the aperture");
    }
    // A call that parses out of one encoding and into another is a call whose
    // meaning depends on which copy is read.
    assert_eq!(Call::parse(call.encode()), Ok(call), "a call does not survive its own encoding");
});
