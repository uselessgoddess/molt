//! Register pressure, so the compiler has to say what it will and will not use.
//!
//! `x18` and `x21` are callee-saved, so a leaf function never touches them and
//! a probe that does not spill proves nothing. `mix` keeps twelve values live
//! across a call each, which is the one thing that forces the allocator through
//! the saved registers; `reach` is the address arithmetic the verifier expects
//! to see, and exists to check that the Zba instruction the scheme is built on
//! is emitted rather than synthesised from a shift pair.

#![no_std]

unsafe extern "C" {
    fn step(value: u64) -> u64;
}

#[unsafe(no_mangle)]
pub extern "C" fn mix(input: &[u64; 12]) -> u64 {
    let mut held = [0u64; 12];
    let mut index = 0;
    while index < held.len() {
        held[index] = unsafe { step(input[index]) };
        index += 1;
    }

    let mut sum = 0u64;
    for (lane, value) in held.iter().enumerate() {
        sum = sum.wrapping_add(value.rotate_left(lane as u32 + 1));
    }
    sum
}

/// A 32-bit offset zero-extended onto a base, which is what `add.uw` does in
/// one instruction and what bounds an LFI sandbox to `[base, base + 2^32)`.
#[unsafe(no_mangle)]
pub extern "C" fn reach(base: *const u8, offset: u32) -> *const u8 {
    base.wrapping_add(offset as usize)
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
