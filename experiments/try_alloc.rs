//! Does the pinned toolchain hand out the fallible constructors this needs?
#![feature(allocator_api)]

extern crate alloc;

use alloc::alloc::AllocError;
use alloc::boxed::Box;
use alloc::rc::Rc;

fn main() -> Result<(), AllocError> {
    let boxed: Box<[u8; 4096]> = unsafe { Box::try_new_zeroed()?.assume_init() };
    let slice: Box<[u64]> = unsafe { Box::try_new_zeroed_slice(7)?.assume_init() };
    let shared: Rc<u64> = unsafe { Rc::try_new_zeroed()?.assume_init() };
    let value: Box<u32> = Box::try_new(5)?;
    println!("{} {} {shared} {value}", boxed.len(), slice.len());
    Ok(())
}
