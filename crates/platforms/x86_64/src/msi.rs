//! Message-signalled interrupts: a fixed bank of vectors and where they go.
//!
//! An MSI is a memory write: the device stores `data` at `address`, the local
//! APIC decodes that into a vector, and the CPU takes an interrupt. This module
//! is the agreement about which vectors exist and what happens when one arrives.
//!
//! The bank is fixed because the IDT is built once at boot. Every handler does
//! two things — raise the [`Sink`] and EOI — because anything longer runs with
//! interrupts disabled on whatever stack was current.

use core::sync::atomic::{AtomicU32, Ordering};

use molt_arch::{FabricError, MsiMessage, Sink};
use spin::Once;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame};

use crate::apic;

/// Above the timer at `0x40`, below the spurious vector at `0xff`.
pub const FIRST_VECTOR: u8 = 0x50;
pub const VECTORS: u8 = 16;

const MESSAGE_BASE: u64 = 0xfee0_0000;
const DESTINATION_SHIFT: u64 = 12;

/// A [`Once`] rather than an atomic pointer: `&dyn Sink` is a fat pointer, and
/// a torn read is a jump through the wrong vtable.
static SINK: Once<&'static dyn Sink> = Once::new();

/// One bit per vector, set while allocated.
static TAKEN: AtomicU32 = AtomicU32::new(0);

/// Installs a handler for every vector in the bank.
///
/// Unhandled vectors are a #GP; a device that fires early or a stale message
/// must be swallowed, not fatal.
pub fn install(idt: &mut InterruptDescriptorTable) {
    for (line, handler) in HANDLERS.iter().enumerate() {
        idt[FIRST_VECTOR + line as u8].set_handler_fn(*handler);
    }
}

pub fn route(sink: &'static dyn Sink) {
    SINK.call_once(|| sink);
}

/// Claims a free vector and the message that reaches it.
pub fn allocate() -> Result<(u16, MsiMessage), FabricError> {
    let mut taken = TAKEN.load(Ordering::Acquire);
    loop {
        let line = taken.trailing_ones();
        if line >= u32::from(VECTORS) {
            return Err(FabricError::Exhausted);
        }
        let claimed = taken | 1 << line;
        match TAKEN.compare_exchange_weak(taken, claimed, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => {
                let line = line as u16;
                return Ok((line, message(line)));
            }
            Err(current) => taken = current,
        }
    }
}

/// Releases a vector. Double-release is [`FabricError::Unknown`]: two owners
/// disagree about who holds the line.
pub fn release(line: u16) -> Result<(), FabricError> {
    if line >= u16::from(VECTORS) {
        return Err(FabricError::Unknown);
    }
    let bit = 1u32 << line;
    let previous = TAKEN.fetch_and(!bit, Ordering::AcqRel);
    if previous & bit == 0 { Err(FabricError::Unknown) } else { Ok(()) }
}

/// Fixed delivery, physical destination, edge triggered — all zero fields.
/// Every vector goes to the boot CPU; molt starts one.
fn message(line: u16) -> MsiMessage {
    let destination = u64::from(apic::id()) << DESTINATION_SHIFT;
    let vector = u32::from(FIRST_VECTOR) + u32::from(line);
    MsiMessage::new(MESSAGE_BASE | destination, vector)
}

/// EOI is unconditional: skipping it leaves the in-service register set and
/// blocks every interrupt at or below that priority, timer included.
fn deliver(line: u16) {
    if let Some(sink) = SINK.get() {
        sink.raise(line);
    }
    apic::eoi();
}

extern "x86-interrupt" fn handler<const LINE: u16>(_frame: InterruptStackFrame) {
    deliver(LINE);
}

macro_rules! handlers {
    ($($line:expr),* $(,)?) => {
        [$(handler::<$line>),*]
    };
}

const HANDLERS: [extern "x86-interrupt" fn(InterruptStackFrame); VECTORS as usize] =
    handlers![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];

#[cfg(test)]
mod tests {
    use core::sync::atomic::Ordering;

    use molt_arch::FabricError;

    use super::{TAKEN, VECTORS, release};

    fn claim() -> Option<u16> {
        let mut taken = TAKEN.load(Ordering::Acquire);
        loop {
            let line = taken.trailing_ones();
            if line >= u32::from(VECTORS) {
                return None;
            }
            match TAKEN.compare_exchange(
                taken,
                taken | 1 << line,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(line as u16),
                Err(current) => taken = current,
            }
        }
    }

    #[test]
    fn hands_out_each_vector_once_reuses_released() {
        let first = claim().expect("an empty bank");
        let second = claim().expect("a bank with room");
        assert_ne!(first, second, "one vector was handed to two owners");

        assert_eq!(release(first), Ok(()));
        assert_eq!(release(first), Err(FabricError::Unknown), "a line was released twice");
        assert_eq!(claim(), Some(first), "a released vector was not reused");
        assert_eq!(release(VECTORS.into()), Err(FabricError::Unknown), "a line off the end");

        let mut claimed = 2;
        while claim().is_some() {
            claimed += 1;
            assert!(claimed <= VECTORS, "the bank handed out more vectors than it has");
        }
        assert_eq!(TAKEN.load(Ordering::Acquire).count_ones(), u32::from(VECTORS));

        for line in 0..u16::from(VECTORS) {
            assert_eq!(release(line), Ok(()));
        }
        assert_eq!(TAKEN.load(Ordering::Acquire), 0, "the bank did not come back empty");
    }
}
