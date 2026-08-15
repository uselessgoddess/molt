use core::cell::UnsafeCell;
use core::mem::size_of;
use core::sync::atomic::{AtomicBool, Ordering};

use spin::Once;
use x86_64::instructions::segmentation::{CS, DS, ES, SS, Segment};
use x86_64::instructions::tables::load_tss;
use x86_64::registers::control::Cr2;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::{PrivilegeLevel, VirtAddr};

use crate::{apic, emergency_write, msi, percpu};

const DOUBLE_FAULT_IST_INDEX: u16 = 0;
const EXCEPTION_STACK: usize = 4096 * 5;

#[repr(align(16))]
struct ExceptionStack(UnsafeCell<[u8; EXCEPTION_STACK]>);

#[repr(align(4096))]
struct GatewayTable<T>(T);

// SAFETY: the TSS-selected CPU exclusively uses this storage as its double-fault stack.
unsafe impl Sync for ExceptionStack {}

/// Per-core, all three of them: a task segment names one stack, and loading one
/// twice sets a busy bit the second `ltr` faults on. The interrupt table is the
/// exception — its handlers say nothing about which core took the interrupt.
static DOUBLE_FAULT_STACK: [ExceptionStack; percpu::MAX] =
    [const { ExceptionStack(UnsafeCell::new([0; EXCEPTION_STACK])) }; percpu::MAX];
static TSS: [GatewayTable<Once<TaskStateSegment>>; percpu::MAX] =
    [const { GatewayTable(Once::new()) }; percpu::MAX];
static GDT: [GatewayTable<Once<(GlobalDescriptorTable, Selectors)>>; percpu::MAX] =
    [const { GatewayTable(Once::new()) }; percpu::MAX];
static IDT: GatewayTable<Once<InterruptDescriptorTable>> = GatewayTable(Once::new());
static BREAKPOINT_SEEN: AtomicBool = AtomicBool::new(false);

struct Selectors {
    code: SegmentSelector,
    data: SegmentSelector,
    user_code: SegmentSelector,
    user_data: SegmentSelector,
    tss: SegmentSelector,
}

/// Gives the running core its tables. Called once per core, on that core.
pub fn init() {
    let own = percpu::this().cpu().index();
    let tss = TSS[own].0.call_once(|| {
        let mut tss = TaskStateSegment::new();
        let stack = VirtAddr::from_ptr(DOUBLE_FAULT_STACK[own].0.get());
        tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = stack + EXCEPTION_STACK as u64;
        tss.privilege_stack_table[0] = VirtAddr::new(crate::domain::gateway_stack_top());
        tss
    });
    let (gdt, selectors) = GDT[own].0.call_once(|| {
        let mut gdt = GlobalDescriptorTable::new();
        let code = gdt.append(Descriptor::kernel_code_segment());
        let data = gdt.append(Descriptor::kernel_data_segment());
        let user_data = gdt.append(Descriptor::user_data_segment());
        let user_code = gdt.append(Descriptor::user_code_segment());
        let tss = gdt.append(Descriptor::tss_segment(tss));
        (gdt, Selectors { code, data, user_code, user_data, tss })
    });
    gdt.load();
    // SAFETY: both selectors name descriptors in the loaded static GDT.
    unsafe {
        CS::set_reg(selectors.code);
        SS::set_reg(selectors.data);
        DS::set_reg(selectors.data);
        ES::set_reg(selectors.data);
        load_tss(selectors.tss);
    }

    let idt = IDT.0.call_once(|| {
        let mut idt = InterruptDescriptorTable::new();
        let (no_error, with_error) = crate::domain::exception_entries();
        // SAFETY: the assembly stub has the x86-interrupt page-fault stack
        // shape. Kernel faults tail-jump to the typed Rust handler unchanged;
        // user faults switch roots inside the gateway first.
        unsafe {
            for (vector, address) in no_error {
                idt[vector].set_handler_addr(VirtAddr::new(address));
            }
            idt.invalid_tss.set_handler_addr(VirtAddr::new(with_error[0].1));
            idt.segment_not_present.set_handler_addr(VirtAddr::new(with_error[1].1));
            idt.stack_segment_fault.set_handler_addr(VirtAddr::new(with_error[2].1));
            idt.general_protection_fault.set_handler_addr(VirtAddr::new(with_error[3].1));
            idt.alignment_check.set_handler_addr(VirtAddr::new(with_error[4].1));
            idt.cp_protection_exception.set_handler_addr(VirtAddr::new(with_error[5].1));
            idt.page_fault.set_handler_addr(VirtAddr::new(crate::domain::page_fault_entry()));
            idt[0x80]
                .set_handler_addr(VirtAddr::new(crate::domain::call_entry()))
                .set_privilege_level(PrivilegeLevel::Ring3);
        }
        // SAFETY: the TSS entry points at the dedicated static exception stack above.
        unsafe {
            idt.double_fault
                .set_handler_fn(double_fault_handler)
                .set_stack_index(DOUBLE_FAULT_IST_INDEX);
        }
        idt[apic::TIMER_VECTOR].set_handler_fn(apic::timer_interrupt);
        idt[apic::WAKE_VECTOR].set_handler_fn(apic::wake_interrupt);
        idt[apic::SPURIOUS_VECTOR].set_handler_fn(apic::spurious_interrupt);

        msi::install(&mut idt);
        idt
    });
    idt.load();
}

pub fn verify_breakpoint() -> bool {
    BREAKPOINT_SEEN.store(false, Ordering::Release);
    x86_64::instructions::interrupts::int3();
    BREAKPOINT_SEEN.load(Ordering::Acquire)
}

#[unsafe(no_mangle)]
extern "x86-interrupt" fn __molt_kernel_breakpoint(_frame: InterruptStackFrame) {
    BREAKPOINT_SEEN.store(true, Ordering::Release);
}

#[unsafe(no_mangle)]
extern "x86-interrupt" fn __molt_kernel_page_fault(
    _frame: InterruptStackFrame,
    _error: PageFaultErrorCode,
) {
    emergency_write("MOLT_EXCEPTION: page fault at ");
    emergency_hex(Cr2::read().map_or(0, |address| address.as_u64()));
    emergency_write("\n");
    super::halt_forever()
}

pub fn user_selectors() -> (u16, u16) {
    let own = percpu::this().cpu().index();
    let (_, selectors) = GDT[own].0.get().expect("GDT initialized before domain entry");
    (
        SegmentSelector::new(selectors.user_code.index(), PrivilegeLevel::Ring3).0,
        SegmentSelector::new(selectors.user_data.index(), PrivilegeLevel::Ring3).0,
    )
}

/// Supervisor-only descriptor pages the processor reads while using a domain root.
pub fn domain_tables() -> [(u64, u64); 3] {
    let own = percpu::this().cpu().index();
    [bounds(&IDT), bounds(&GDT[own]), bounds(&TSS[own])]
}

fn bounds<T>(table: &GatewayTable<T>) -> (u64, u64) {
    let start = table as *const GatewayTable<T> as u64;
    (start, start + size_of::<GatewayTable<T>>() as u64)
}

extern "x86-interrupt" fn double_fault_handler(_frame: InterruptStackFrame, _error: u64) -> ! {
    emergency_write("MOLT_EXCEPTION: double fault\n");
    super::halt_forever()
}

fn emergency_hex(value: u64) {
    emergency_write("0x");
    for shift in (0..16).rev() {
        let nibble = ((value >> (shift * 4)) & 0xf) as u8;
        super::emergency_byte(if nibble < 10 { b'0' + nibble } else { b'a' + nibble - 10 });
    }
}
