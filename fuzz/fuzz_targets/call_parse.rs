#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use molt_abi::wire::{APERTURE, Call, SLOT_WORDS};

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
    assert_eq!(Call::parse(call.encode()), Ok(call), "a call does not survive its own encoding");
});
