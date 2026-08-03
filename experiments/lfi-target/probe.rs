//! Does upstream rustc honour the register reservations LFI-RISCV requires?
//!
//! The LFI-RISCV verifier reserves `x21` for the sandbox base and lets only
//! `add.uw` write `x18`, `ra`, and `sp`. `lfi-rewrite` refuses input that uses
//! the reserved registers, so the question is whether stock rustc can be told
//! to leave them alone — without a compiler fork.
#![no_std]
#![no_main]

/// Enough live values to force the register allocator into the saved set.
#[unsafe(no_mangle)]
pub extern "C" fn pressure(input: &[u64]) -> u64 {
    let (mut a, mut b, mut c, mut d) = (1u64, 2u64, 3u64, 5u64);
    let (mut e, mut f, mut g, mut h) = (7u64, 11u64, 13u64, 17u64);
    let (mut i, mut j, mut k, mut l) = (19u64, 23u64, 29u64, 31u64);
    for (index, &value) in input.iter().enumerate() {
        a ^= value.rotate_left(1);
        b = b.wrapping_add(value ^ a);
        c ^= b.rotate_left(3);
        d = d.wrapping_mul(value | 1);
        e ^= c.wrapping_add(d);
        f = f.wrapping_add(e ^ index as u64);
        g ^= f.rotate_left(7);
        h = h.wrapping_add(g);
        i ^= h.rotate_left(11);
        j = j.wrapping_add(i);
        k ^= j.rotate_left(13);
        l = l.wrapping_add(k ^ a);
    }
    a ^ b ^ c ^ d ^ e ^ f ^ g ^ h ^ i ^ j ^ k ^ l
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
