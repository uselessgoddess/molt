use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use molt_arch::InterruptSink;
use spin::Once;
use x86_64::VirtAddr;
use x86_64::instructions::segmentation::{CS, DS, ES, SS, Segment};
use x86_64::instructions::tables::load_tss;
use x86_64::registers::control::Cr2;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};
use x86_64::structures::tss::TaskStateSegment;

use crate::{apic, emergency_write};

const DOUBLE_FAULT_IST_INDEX: u16 = 0;

/// The first vector handed out to a message interrupt.
///
/// It sits above the timer so the two banks cannot be confused by eye in a
/// trace, and far above the 32 vectors the architecture reserves for faults.
pub const FIRST_MESSAGE_VECTOR: u8 = 0x41;

/// How many message vectors exist. One per device the kernel binds, and it
/// binds them one at a time, so this is a bank rather than a table: a device
/// that asks for a vector gets the next one and keeps it.
pub const MESSAGE_VECTORS: u8 = 8;

#[repr(align(16))]
struct ExceptionStack(UnsafeCell<[u8; 4096 * 5]>);

// SAFETY: only the CPU selected by the TSS uses this storage, exclusively as
// the double-fault stack. Normal Rust code never reads or writes its contents.
unsafe impl Sync for ExceptionStack {}

static DOUBLE_FAULT_STACK: ExceptionStack = ExceptionStack(UnsafeCell::new([0; 4096 * 5]));
static TSS: Once<TaskStateSegment> = Once::new();
static GDT: Once<(GlobalDescriptorTable, Selectors)> = Once::new();
static IDT: Once<InterruptDescriptorTable> = Once::new();
static BREAKPOINT_SEEN: AtomicBool = AtomicBool::new(false);
static SINK: Once<&'static dyn InterruptSink> = Once::new();
static NEXT_VECTOR: AtomicU8 = AtomicU8::new(FIRST_MESSAGE_VECTOR);

/// Points the message vectors at the kernel's interrupt table.
///
/// Attaching is separate from installing the descriptors: the descriptors exist
/// from `init` onwards because a vector that arrives with no entry behind it is
/// a general protection fault, whereas one that arrives before the kernel has
/// somewhere to record it is merely unobserved.
pub fn attach(sink: &'static dyn InterruptSink) {
    SINK.call_once(|| sink);
}

/// Takes the next unused message vector, or `None` once the bank is spent.
pub fn allocate() -> Option<u8> {
    let vector = NEXT_VECTOR.fetch_add(1, Ordering::Relaxed);
    (vector < FIRST_MESSAGE_VECTOR + MESSAGE_VECTORS).then_some(vector)
}

/// Reports one arrival and acknowledges it at the interrupt controller.
///
/// The order is deliberate: the kernel records the vector first, so a waiter
/// woken by the record cannot observe the arrival before the record exists, and
/// the end-of-interrupt goes last because it is what allows the next one.
fn deliver(vector: u8) {
    if let Some(sink) = SINK.get() {
        sink.signal(vector);
    }
    apic::eoi();
}

/// One `extern "x86-interrupt"` entry per vector.
///
/// A trap gate is a code address and nothing else — there is no argument to
/// carry the vector number in — so the number has to be baked into the function.
/// Eight near-identical functions is what that costs; writing them by hand is
/// what it would cost without the macro.
macro_rules! message_handlers {
    ($($name:ident = $offset:expr),* $(,)?) => {
        $(
            extern "x86-interrupt" fn $name(_frame: InterruptStackFrame) {
                deliver(FIRST_MESSAGE_VECTOR + $offset);
            }
        )*

        const MESSAGE_HANDLERS: [extern "x86-interrupt" fn(InterruptStackFrame);
            MESSAGE_VECTORS as usize] = [$($name),*];
    };
}

message_handlers! {
    message_0 = 0,
    message_1 = 1,
    message_2 = 2,
    message_3 = 3,
    message_4 = 4,
    message_5 = 5,
    message_6 = 6,
    message_7 = 7,
}

struct Selectors {
    code: SegmentSelector,
    data: SegmentSelector,
    tss: SegmentSelector,
}

pub fn init() {
    let tss = TSS.call_once(|| {
        let mut tss = TaskStateSegment::new();
        let stack_start = VirtAddr::from_ptr(DOUBLE_FAULT_STACK.0.get());
        tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] =
            stack_start + size_of::<[u8; 4096 * 5]>() as u64;
        tss
    });
    let (gdt, selectors) = GDT.call_once(|| {
        let mut gdt = GlobalDescriptorTable::new();
        let code = gdt.append(Descriptor::kernel_code_segment());
        let data = gdt.append(Descriptor::kernel_data_segment());
        let tss = gdt.append(Descriptor::tss_segment(tss));
        (gdt, Selectors { code, data, tss })
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

    let idt = IDT.call_once(|| {
        let mut idt = InterruptDescriptorTable::new();
        idt.breakpoint.set_handler_fn(breakpoint_handler);
        idt.page_fault.set_handler_fn(page_fault_handler);
        // SAFETY: the TSS entry points at the dedicated static exception stack above.
        unsafe {
            idt.double_fault
                .set_handler_fn(double_fault_handler)
                .set_stack_index(DOUBLE_FAULT_IST_INDEX);
        }
        idt[apic::TIMER_VECTOR].set_handler_fn(apic::timer_interrupt);
        idt[apic::SPURIOUS_VECTOR].set_handler_fn(apic::spurious_interrupt);
        for (offset, handler) in MESSAGE_HANDLERS.into_iter().enumerate() {
            idt[FIRST_MESSAGE_VECTOR + offset as u8].set_handler_fn(handler);
        }
        idt
    });
    idt.load();
}

pub fn verify_breakpoint() -> bool {
    BREAKPOINT_SEEN.store(false, Ordering::Release);
    x86_64::instructions::interrupts::int3();
    BREAKPOINT_SEEN.load(Ordering::Acquire)
}

extern "x86-interrupt" fn breakpoint_handler(_frame: InterruptStackFrame) {
    BREAKPOINT_SEEN.store(true, Ordering::Release);
}

extern "x86-interrupt" fn page_fault_handler(
    _frame: InterruptStackFrame,
    _error: PageFaultErrorCode,
) {
    emergency_write("MOLT_EXCEPTION: page fault at ");
    emergency_hex(Cr2::read().map_or(0, |address| address.as_u64()));
    emergency_write("\n");
    super::halt_forever()
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
