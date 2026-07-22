//! Message-signalled interrupts: a bank of vectors, and where they go.
//!
//! An MSI on x86_64 is a memory write, not a wire. The device writes `data` to
//! `address`, the interrupt-remapping-free local APIC decodes the address as a
//! destination and the data as a vector, and the CPU takes an interrupt at that
//! vector. So the "interrupt controller" this module programs is really just an
//! agreement about which vectors exist and what happens when one arrives.
//!
//! Two decisions are worth stating. The vectors are a fixed bank rather than
//! anything dynamic, because the IDT is built once at boot and an entry that
//! can appear later is an entry that can be missing when the interrupt does.
//! And every handler does exactly two things — hand the line to the routed
//! [`Sink`] and signal end-of-interrupt — because anything longer runs with
//! interrupts disabled on whatever stack happened to be current.

use core::sync::atomic::{AtomicU32, Ordering};

use molt_arch::{FabricError, MsiMessage, Sink};
use spin::Once;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame};

use crate::apic;

/// The first vector of the MSI bank, and how many it holds.
///
/// Above the timer at `0x40` and well clear of the CPU's exception range, and
/// below the spurious vector at `0xff`.
pub const FIRST_VECTOR: u8 = 0x50;
pub const VECTORS: u8 = 16;

/// The local APIC's memory-mapped message address, and the field the
/// destination APIC ID sits in.
const MESSAGE_BASE: u64 = 0xfee0_0000;
const DESTINATION_SHIFT: u64 = 12;

/// Where the kernel wants arrivals delivered.
///
/// A [`Once`] rather than an atomic pointer because `&dyn Sink` is a fat
/// pointer, and a torn read of one is a jump through a wrong vtable.
static SINK: Once<&'static dyn Sink> = Once::new();

/// One bit per vector in the bank, set while the vector is allocated.
static TAKEN: AtomicU32 = AtomicU32::new(0);

/// Installs the bank's handlers into `idt`.
///
/// Every vector gets a handler whether or not anything is routed to it yet: an
/// unhandled vector is a general-protection fault, and a device that fires
/// early — or a stale message a previous owner left programmed — should be
/// swallowed, not fatal.
pub fn install(idt: &mut InterruptDescriptorTable) {
    for (line, handler) in HANDLERS.iter().enumerate() {
        idt[FIRST_VECTOR + line as u8].set_handler_fn(*handler);
    }
}

/// Sends every arrival on this bank to `sink`.
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

/// Releases a vector claimed by [`allocate`].
///
/// Releasing a vector nothing claimed is [`FabricError::Unknown`] rather than
/// silence: it means two owners disagree about who holds the line, and the
/// second release would hand a live vector to a third.
pub fn release(line: u16) -> Result<(), FabricError> {
    if line >= u16::from(VECTORS) {
        return Err(FabricError::Unknown);
    }
    let bit = 1u32 << line;
    let previous = TAKEN.fetch_and(!bit, Ordering::AcqRel);
    if previous & bit == 0 { Err(FabricError::Unknown) } else { Ok(()) }
}

/// The message that reaches `line` on the boot processor.
///
/// Fixed delivery, physical destination, edge triggered — the encoding is all
/// zero fields, which is why none of them appear here. Every vector goes to the
/// boot CPU because that is the only one molt starts.
fn message(line: u16) -> MsiMessage {
    let destination = u64::from(apic::id()) << DESTINATION_SHIFT;
    let vector = u32::from(FIRST_VECTOR) + u32::from(line);
    MsiMessage::new(MESSAGE_BASE | destination, vector)
}

/// What runs when a vector in the bank arrives.
///
/// The end-of-interrupt is unconditional and last. Skipping it when no sink is
/// routed would leave the local APIC's in-service register set, and every
/// interrupt at or below that priority — the timer included — would stop
/// arriving, which is a much worse failure than a dropped stray interrupt.
fn deliver(line: u16) {
    if let Some(sink) = SINK.get() {
        sink.raise(line);
    }
    apic::eoi();
}

extern "x86-interrupt" fn handler<const LINE: u16>(_frame: InterruptStackFrame) {
    deliver(LINE);
}

/// One handler per vector, named by the line it delivers.
///
/// Written out because `set_handler_fn` needs a distinct monomorphisation per
/// vector: the handler has no argument saying which vector it is, so the vector
/// has to be baked into the function.
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

    /// `allocate` reads the live APIC, so only the bookkeeping half is testable
    /// on the host; this is the part that would silently double-issue a vector.
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

    /// One test, not three: `TAKEN` is process-global and the test harness runs
    /// threads in parallel, so separate tests would race each other's bank.
    #[test]
    fn the_bank_hands_out_each_vector_once_and_reuses_a_released_one() {
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
