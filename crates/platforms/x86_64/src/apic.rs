use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

use molt_arch::PlatformError;
use x86_64::instructions::interrupts;

pub const TIMER_VECTOR: u8 = 0x40;
pub const SPURIOUS_VECTOR: u8 = 0xff;

/// The architectural local APIC MMIO base, which the kernel maps a window for.
pub const APIC_MMIO: u64 = 0xfee0_0000;

const IA32_APIC_BASE: u32 = 0x1b;
const APIC_ENABLE: u64 = 1 << 11;
const APIC_BASE_MASK: u64 = 0xffff_f000;
const REG_ID: u64 = 0x020;
const REG_EOI: u64 = 0x0b0;
const REG_SPURIOUS: u64 = 0x0f0;
const REG_LVT_TIMER: u64 = 0x320;
const REG_INITIAL_COUNT: u64 = 0x380;
const REG_DIVIDE: u64 = 0x3e0;
const APIC_SOFTWARE_ENABLE: u32 = 1 << 8;
const TIMER_MASKED: u32 = 1 << 16;

static APIC_VIRTUAL_BASE: AtomicU64 = AtomicU64::new(0);
static TICKS: AtomicU64 = AtomicU64::new(0);

/// Brings up the local APIC through the `window` the kernel mapped for it.
///
/// The window replaces the loader's direct map: it is the kernel's own leaf,
/// uncacheable and non-executable, so an APIC register write is a write to the
/// APIC rather than a store that reaches it whenever a cache line is evicted.
pub fn init(window: u64) -> Result<(), PlatformError> {
    let features = core::arch::x86_64::__cpuid(1);
    if features.edx & (1 << 9) == 0 {
        return Err(PlatformError::InvalidHardware);
    }

    // SAFETY: IA32_APIC_BASE is an architectural MSR and CPUID reported APIC support.
    let mut base = unsafe { read_msr(IA32_APIC_BASE) };
    base |= APIC_ENABLE;
    // SAFETY: only the APIC enable bit is changed; the firmware-selected base is preserved.
    unsafe { write_msr(IA32_APIC_BASE, base) };
    // The window is mapped for the architectural base; firmware that relocated
    // the APIC would leave it pointing at the wrong frame, so refuse instead.
    if base & APIC_BASE_MASK != APIC_MMIO {
        return Err(PlatformError::InvalidHardware);
    }
    APIC_VIRTUAL_BASE.store(window, Ordering::Release);

    write(REG_SPURIOUS, APIC_SOFTWARE_ENABLE | u32::from(SPURIOUS_VECTOR))?;
    write(REG_LVT_TIMER, TIMER_MASKED | u32::from(TIMER_VECTOR))?;
    write(REG_DIVIDE, 0b0011)?; // divide the bus clock by 16
    TICKS.store(0, Ordering::Release);
    // SAFETY: Stage 1 uses the local APIC exclusively; masking both legacy PICs prevents their
    // firmware vector assignments (notably IRQ 8) from colliding with CPU exception vectors.
    unsafe {
        super::out_u8(0x21, 0xff);
        super::out_u8(0xa1, 0xff);
    }
    interrupts::enable();
    Ok(())
}

pub fn arm(initial_count: u32) -> Result<(), PlatformError> {
    if initial_count == 0 {
        return Err(PlatformError::InvalidHardware);
    }
    write(REG_LVT_TIMER, u32::from(TIMER_VECTOR))?;
    write(REG_INITIAL_COUNT, initial_count)
}

/// This processor's local APIC identifier, or zero before [`init`] ran.
///
/// Zero is also a legal APIC ID, and that is deliberately not distinguished:
/// molt starts one processor, so an MSI addressed to destination zero reaches
/// it either way.
pub fn id() -> u8 {
    let base = APIC_VIRTUAL_BASE.load(Ordering::Acquire);
    if base == 0 {
        return 0;
    }
    // SAFETY: `init` derived this direct-mapped base from IA32_APIC_BASE; the
    // ID register is a naturally aligned 32-bit MMIO location.
    let raw = unsafe { core::ptr::read_volatile((base + REG_ID) as *const u32) };
    (raw >> 24) as u8
}

/// Signals end-of-interrupt to the local APIC.
///
/// Every interrupt handler in the bank calls this, including the ones that had
/// nothing to deliver: an in-service bit left set blocks every later interrupt
/// at or below its priority.
pub fn eoi() {
    let _ = write(REG_EOI, 0);
}

pub fn ticks() -> u64 {
    TICKS.load(Ordering::Acquire)
}

pub fn wait_for_change(previous: u64) {
    interrupts::disable();
    if ticks() == previous {
        interrupts::enable_and_hlt();
    } else {
        interrupts::enable();
    }
}

pub extern "x86-interrupt" fn timer_interrupt(
    _frame: x86_64::structures::idt::InterruptStackFrame,
) {
    TICKS.fetch_add(1, Ordering::Release);
    eoi();
}

pub extern "x86-interrupt" fn spurious_interrupt(
    _frame: x86_64::structures::idt::InterruptStackFrame,
) {
}

fn write(offset: u64, value: u32) -> Result<(), PlatformError> {
    let base = APIC_VIRTUAL_BASE.load(Ordering::Acquire);
    if base == 0 {
        return Err(PlatformError::InvalidHardware);
    }
    let address = base.checked_add(offset).ok_or(PlatformError::InvalidHardware)?;
    // SAFETY: `init` derives this direct-mapped address from IA32_APIC_BASE; APIC registers
    // are naturally aligned 32-bit MMIO locations and volatile access is required.
    unsafe { core::ptr::write_volatile(address as *mut u32, value) };
    Ok(())
}

unsafe fn read_msr(msr: u32) -> u64 {
    let low: u32;
    let high: u32;
    // SAFETY: the caller selected an architectural MSR available at privilege level zero.
    unsafe {
        asm!("rdmsr", in("ecx") msr, out("eax") low, out("edx") high, options(nomem, nostack));
    }
    u64::from(low) | (u64::from(high) << 32)
}

/// # Safety
///
/// The caller must preserve reserved bits for the selected MSR.
pub unsafe fn write_msr(msr: u32, value: u64) {
    // SAFETY: the caller preserves reserved bits for the selected architectural MSR.
    unsafe {
        asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") value as u32,
            in("edx") (value >> 32) as u32,
            options(nomem, nostack)
        );
    }
}
