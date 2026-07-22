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
const REG_EOI: u64 = 0x0b0;
const REG_SPURIOUS: u64 = 0x0f0;
const REG_LVT_TIMER: u64 = 0x320;
const REG_INITIAL_COUNT: u64 = 0x380;
const REG_DIVIDE: u64 = 0x3e0;
const APIC_SOFTWARE_ENABLE: u32 = 1 << 8;
const TIMER_MASKED: u32 = 1 << 16;

static APIC_VIRTUAL_BASE: AtomicU64 = AtomicU64::new(0);
static TICKS: AtomicU64 = AtomicU64::new(0);

/// Initializes the local APIC through an uncacheable kernel-owned window.
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
    // The window maps the architectural base, so reject firmware relocation.
    if base & APIC_BASE_MASK != APIC_MMIO {
        return Err(PlatformError::InvalidHardware);
    }
    APIC_VIRTUAL_BASE.store(window, Ordering::Release);

    write(REG_SPURIOUS, APIC_SOFTWARE_ENABLE | u32::from(SPURIOUS_VECTOR))?;
    write(REG_LVT_TIMER, TIMER_MASKED | u32::from(TIMER_VECTOR))?;
    write(REG_DIVIDE, 0b0011)?; // divide by 16
    TICKS.store(0, Ordering::Release);
    // SAFETY: masking both legacy PICs prevents their vectors from colliding with CPU exceptions.
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
    let _ = write(REG_EOI, 0);
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
    // SAFETY: `init` provides an aligned APIC MMIO address that requires volatile access.
    unsafe { core::ptr::write_volatile(address as *mut u32, value) };
    Ok(())
}

/// Reads an architectural model-specific register.
///
/// # Safety
///
/// `msr` must be readable at privilege level zero.
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
