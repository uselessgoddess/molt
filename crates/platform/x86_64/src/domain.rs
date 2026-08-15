//! x86_64 ring-3 entry through supervisor-only gateway pages.

use core::arch::global_asm;

use molt_arch::{DomainExit, DomainState, PlatformError};

use crate::{interrupts, memory};

/// Runtime calls, as `molt_program` numbers them in `rax`.
const KICK: u64 = 0;
const EXIT: u64 = 1;

/// Which gateway path produced the exit. Written by the gateway, not the domain.
const FROM_CALL: u64 = 0;

const DOMAIN_CR3: usize = 0x40;
const REASON: usize = 0x48;
const VALUE: usize = 0x50;
const CAUSE: usize = 0x58;
const ADDRESS: usize = 0x60;
const USER_RIP: usize = 0x68;
const USER_RSP: usize = 0x70;
const USER_RFLAGS: usize = 0x78;
const USER_CS: usize = 0x80;
const USER_SS: usize = 0x88;
const RCX: usize = 0xa0;
const RDX: usize = 0xa8;
const RSI: usize = 0xb0;
const RDI: usize = 0xb8;
const R8: usize = 0xc8;
const R9: usize = 0xd0;
/// The user register file, `rax` first, as the gateway saves and restores it.
/// 0x108 holds the kernel's `RFLAGS` while the domain runs, and only the
/// gateway reads it back.
const REGISTERS: usize = 0x90;
const REGISTER_SLOTS: usize = 15;
/// The runtime call number a domain asked for, when the exit came from a call.
const CALL: usize = 0x110;
/// What the gateway reports through, cleared with the file it belongs to.
const REPORTS: [usize; 5] = [REASON, VALUE, CAUSE, ADDRESS, CALL];

type ExceptionEntries<const N: usize> = [(u8, u64); N];

global_asm!(
    r#"
.pushsection .text.domain_gateway, "ax"
.balign 4096
.global __molt_domain_gateway_start
__molt_domain_gateway_start:

.global __molt_domain_call
__molt_domain_call:
    movq %rax, __molt_domain_gateway_data+0x90(%rip)
    movq %rbx, __molt_domain_gateway_data+0x98(%rip)
    movq %rcx, __molt_domain_gateway_data+0xa0(%rip)
    movq %rdx, __molt_domain_gateway_data+0xa8(%rip)
    movq %rsi, __molt_domain_gateway_data+0xb0(%rip)
    movq %rdi, __molt_domain_gateway_data+0xb8(%rip)
    movq %rbp, __molt_domain_gateway_data+0xc0(%rip)
    movq %r8,  __molt_domain_gateway_data+0xc8(%rip)
    movq %r9,  __molt_domain_gateway_data+0xd0(%rip)
    movq %r10, __molt_domain_gateway_data+0xd8(%rip)
    movq %r11, __molt_domain_gateway_data+0xe0(%rip)
    movq %r12, __molt_domain_gateway_data+0xe8(%rip)
    movq %r13, __molt_domain_gateway_data+0xf0(%rip)
    movq %r14, __molt_domain_gateway_data+0xf8(%rip)
    movq %r15, __molt_domain_gateway_data+0x100(%rip)
    movq 0(%rsp), %r10
    movq %r10, __molt_domain_gateway_data+0x68(%rip)
    movq 16(%rsp), %r10
    movq %r10, __molt_domain_gateway_data+0x78(%rip)
    movq 24(%rsp), %r10
    movq %r10, __molt_domain_gateway_data+0x70(%rip)
    // Which family this exit belongs to is decided by the path that ran, never
    // by a register the domain filled in: a call cannot dress itself as a fault
    // and be handed a cause the processor never raised.
    movq $0, __molt_domain_gateway_data+0x48(%rip)
    movq %rax, __molt_domain_gateway_data+0x110(%rip)
    movq %rdi, __molt_domain_gateway_data+0x50(%rip)
    jmp __molt_domain_return

.macro MOLT_DOMAIN_NO_ERROR entry, vector, kernel
.global \entry
\entry:
    testb $3, 8(%rsp)
    jz \kernel
    pushq $0
    pushq $\vector
    jmp __molt_domain_exception
.endm

.macro MOLT_DOMAIN_ERROR entry, vector
.global \entry
\entry:
    testb $3, 16(%rsp)
    jz __molt_kernel_domain_exception
    pushq $\vector
    jmp __molt_domain_exception
.endm

MOLT_DOMAIN_NO_ERROR __molt_domain_divide, 0, __molt_kernel_domain_exception
MOLT_DOMAIN_NO_ERROR __molt_domain_debug, 1, __molt_kernel_domain_exception
MOLT_DOMAIN_NO_ERROR __molt_domain_breakpoint, 3, __molt_kernel_breakpoint
MOLT_DOMAIN_NO_ERROR __molt_domain_overflow, 4, __molt_kernel_domain_exception
MOLT_DOMAIN_NO_ERROR __molt_domain_bound, 5, __molt_kernel_domain_exception
MOLT_DOMAIN_NO_ERROR __molt_domain_invalid_opcode, 6, __molt_kernel_domain_exception
MOLT_DOMAIN_NO_ERROR __molt_domain_device, 7, __molt_kernel_domain_exception
MOLT_DOMAIN_ERROR __molt_domain_invalid_tss, 10
MOLT_DOMAIN_ERROR __molt_domain_segment, 11
MOLT_DOMAIN_ERROR __molt_domain_stack, 12
MOLT_DOMAIN_ERROR __molt_domain_general_protection, 13
MOLT_DOMAIN_NO_ERROR __molt_domain_x87, 16, __molt_kernel_domain_exception
MOLT_DOMAIN_ERROR __molt_domain_alignment, 17
MOLT_DOMAIN_NO_ERROR __molt_domain_simd, 19, __molt_kernel_domain_exception
MOLT_DOMAIN_ERROR __molt_domain_control, 21

__molt_domain_exception:
    movq %rax, __molt_domain_gateway_data+0x90(%rip)
    movq %rbx, __molt_domain_gateway_data+0x98(%rip)
    movq %rcx, __molt_domain_gateway_data+0xa0(%rip)
    movq %rdx, __molt_domain_gateway_data+0xa8(%rip)
    movq %rsi, __molt_domain_gateway_data+0xb0(%rip)
    movq %rdi, __molt_domain_gateway_data+0xb8(%rip)
    movq %rbp, __molt_domain_gateway_data+0xc0(%rip)
    movq %r8,  __molt_domain_gateway_data+0xc8(%rip)
    movq %r9,  __molt_domain_gateway_data+0xd0(%rip)
    movq %r10, __molt_domain_gateway_data+0xd8(%rip)
    movq %r11, __molt_domain_gateway_data+0xe0(%rip)
    movq %r12, __molt_domain_gateway_data+0xe8(%rip)
    movq %r13, __molt_domain_gateway_data+0xf0(%rip)
    movq %r14, __molt_domain_gateway_data+0xf8(%rip)
    movq %r15, __molt_domain_gateway_data+0x100(%rip)
    movq 16(%rsp), %r10
    movq %r10, __molt_domain_gateway_data+0x68(%rip)
    movq 32(%rsp), %r10
    movq %r10, __molt_domain_gateway_data+0x78(%rip)
    movq 40(%rsp), %r10
    movq %r10, __molt_domain_gateway_data+0x70(%rip)
    movq $1, __molt_domain_gateway_data+0x48(%rip)
    movq 0(%rsp), %r10
    movq %r10, __molt_domain_gateway_data+0x58(%rip)
    movq 8(%rsp), %r10
    movq %r10, __molt_domain_gateway_data+0x60(%rip)
    jmp __molt_domain_return

// A kernel-side exception on one of the vectors this gateway took over. Before
// the gateway existed these escalated into the double fault, which said so on
// the serial line; halting mutely would leave the smoke with a timeout and no
// diagnosis, which is the one thing a fatal path must not do.
__molt_kernel_domain_exception:
    cli
    jmp __molt_kernel_domain_fatal

.global __molt_domain_page_fault
__molt_domain_page_fault:
    testb $3, 16(%rsp)
    jz __molt_kernel_page_fault
    movq %rax, __molt_domain_gateway_data+0x90(%rip)
    movq %rbx, __molt_domain_gateway_data+0x98(%rip)
    movq %rcx, __molt_domain_gateway_data+0xa0(%rip)
    movq %rdx, __molt_domain_gateway_data+0xa8(%rip)
    movq %rsi, __molt_domain_gateway_data+0xb0(%rip)
    movq %rdi, __molt_domain_gateway_data+0xb8(%rip)
    movq %rbp, __molt_domain_gateway_data+0xc0(%rip)
    movq %r8,  __molt_domain_gateway_data+0xc8(%rip)
    movq %r9,  __molt_domain_gateway_data+0xd0(%rip)
    movq %r10, __molt_domain_gateway_data+0xd8(%rip)
    movq %r11, __molt_domain_gateway_data+0xe0(%rip)
    movq %r12, __molt_domain_gateway_data+0xe8(%rip)
    movq %r13, __molt_domain_gateway_data+0xf0(%rip)
    movq %r14, __molt_domain_gateway_data+0xf8(%rip)
    movq %r15, __molt_domain_gateway_data+0x100(%rip)
    movq 8(%rsp), %r10
    movq %r10, __molt_domain_gateway_data+0x68(%rip)
    movq 24(%rsp), %r10
    movq %r10, __molt_domain_gateway_data+0x78(%rip)
    movq 32(%rsp), %r10
    movq %r10, __molt_domain_gateway_data+0x70(%rip)
    movq $1, __molt_domain_gateway_data+0x48(%rip)
    movq 0(%rsp), %r10
    movq %r10, __molt_domain_gateway_data+0x58(%rip)
    movq %cr2, %r10
    movq %r10, __molt_domain_gateway_data+0x60(%rip)

__molt_domain_return:
    movq __molt_domain_gateway_data+0x00(%rip), %r10
    movq %r10, %cr3
    movq __molt_domain_gateway_data+0x10(%rip), %rbx
    movq __molt_domain_gateway_data+0x18(%rip), %rbp
    movq __molt_domain_gateway_data+0x20(%rip), %r12
    movq __molt_domain_gateway_data+0x28(%rip), %r13
    movq __molt_domain_gateway_data+0x30(%rip), %r14
    movq __molt_domain_gateway_data+0x38(%rip), %r15
    movq __molt_domain_gateway_data+0x08(%rip), %rsp
    // Every way into this gateway is an interrupt or trap gate, so `IF` is
    // clear on arrival. Without this the kernel would resume with ticks stopped
    // and stay that way until something happened to park. Last, so that what it
    // re-enables lands on a fully restored kernel context.
    pushq __molt_domain_gateway_data+0x108(%rip)
    popfq
    retq

.global __molt_domain_enter
__molt_domain_enter:
    // The CR3 below leaves the kernel unmapped for a dozen instructions that
    // still run at CPL 0. An APIC tick there would vector into kernel text that
    // is no longer present, fault into a handler that is also absent, and
    // triple-fault without a marker. `IF` comes back in `__molt_domain_return`.
    pushfq
    popq %r10
    movq %r10, __molt_domain_gateway_data+0x108(%rip)
    cli
    movq %cr3, %r10
    movq %r10, __molt_domain_gateway_data+0x00(%rip)
    movq %rsp, __molt_domain_gateway_data+0x08(%rip)
    movq %rbx, __molt_domain_gateway_data+0x10(%rip)
    movq %rbp, __molt_domain_gateway_data+0x18(%rip)
    movq %r12, __molt_domain_gateway_data+0x20(%rip)
    movq %r13, __molt_domain_gateway_data+0x28(%rip)
    movq %r14, __molt_domain_gateway_data+0x30(%rip)
    movq %r15, __molt_domain_gateway_data+0x38(%rip)
    leaq __molt_domain_gateway_data_end(%rip), %rsp
    pushq __molt_domain_gateway_data+0x88(%rip)
    pushq __molt_domain_gateway_data+0x70(%rip)
    pushq __molt_domain_gateway_data+0x78(%rip)
    pushq __molt_domain_gateway_data+0x80(%rip)
    pushq __molt_domain_gateway_data+0x68(%rip)
    movq __molt_domain_gateway_data+0x40(%rip), %r10
    movq %r10, %cr3
    movq __molt_domain_gateway_data+0x98(%rip), %rbx
    movq __molt_domain_gateway_data+0xa0(%rip), %rcx
    movq __molt_domain_gateway_data+0xa8(%rip), %rdx
    movq __molt_domain_gateway_data+0xb0(%rip), %rsi
    movq __molt_domain_gateway_data+0xb8(%rip), %rdi
    movq __molt_domain_gateway_data+0xc0(%rip), %rbp
    movq __molt_domain_gateway_data+0xc8(%rip), %r8
    movq __molt_domain_gateway_data+0xd0(%rip), %r9
    movq __molt_domain_gateway_data+0xe0(%rip), %r11
    movq __molt_domain_gateway_data+0xe8(%rip), %r12
    movq __molt_domain_gateway_data+0xf0(%rip), %r13
    movq __molt_domain_gateway_data+0xf8(%rip), %r14
    movq __molt_domain_gateway_data+0x100(%rip), %r15
    movq __molt_domain_gateway_data+0x90(%rip), %rax
    movq __molt_domain_gateway_data+0xd8(%rip), %r10
    iretq
    .balign 4096
.global __molt_domain_gateway_end
__molt_domain_gateway_end:
.popsection

.pushsection .data.domain_gateway, "aw", @progbits
.balign 4096
.global __molt_domain_gateway_data
__molt_domain_gateway_data:
    .zero 4096
.global __molt_domain_gateway_data_end
__molt_domain_gateway_data_end:
.popsection
"#,
    options(att_syntax)
);

unsafe extern "C" {
    static __molt_domain_gateway_start: u8;
    static __molt_domain_gateway_end: u8;
    static mut __molt_domain_gateway_data: u8;
    static __molt_domain_gateway_data_end: u8;
    static __molt_domain_call: u8;
    static __molt_domain_page_fault: u8;
    fn __molt_domain_enter();
}

pub fn call_entry() -> u64 {
    (&raw const __molt_domain_call) as u64
}

pub fn page_fault_entry() -> u64 {
    (&raw const __molt_domain_page_fault) as u64
}

pub fn exception_entries() -> (ExceptionEntries<9>, ExceptionEntries<6>) {
    unsafe extern "C" {
        static __molt_domain_divide: u8;
        static __molt_domain_debug: u8;
        static __molt_domain_breakpoint: u8;
        static __molt_domain_overflow: u8;
        static __molt_domain_bound: u8;
        static __molt_domain_invalid_opcode: u8;
        static __molt_domain_device: u8;
        static __molt_domain_invalid_tss: u8;
        static __molt_domain_segment: u8;
        static __molt_domain_stack: u8;
        static __molt_domain_general_protection: u8;
        static __molt_domain_x87: u8;
        static __molt_domain_alignment: u8;
        static __molt_domain_simd: u8;
        static __molt_domain_control: u8;
    }
    let no_error = [
        (0, (&raw const __molt_domain_divide) as u64),
        (1, (&raw const __molt_domain_debug) as u64),
        (3, (&raw const __molt_domain_breakpoint) as u64),
        (4, (&raw const __molt_domain_overflow) as u64),
        (5, (&raw const __molt_domain_bound) as u64),
        (6, (&raw const __molt_domain_invalid_opcode) as u64),
        (7, (&raw const __molt_domain_device) as u64),
        (16, (&raw const __molt_domain_x87) as u64),
        (19, (&raw const __molt_domain_simd) as u64),
    ];
    let with_error = [
        (10, (&raw const __molt_domain_invalid_tss) as u64),
        (11, (&raw const __molt_domain_segment) as u64),
        (12, (&raw const __molt_domain_stack) as u64),
        (13, (&raw const __molt_domain_general_protection) as u64),
        (17, (&raw const __molt_domain_alignment) as u64),
        (21, (&raw const __molt_domain_control) as u64),
    ];
    (no_error, with_error)
}

/// Reports a kernel fault on a gateway vector, then stops.
///
/// Reached with interrupts already off and the faulting frame still on the
/// stack; it reads nothing from it, because the one thing worth saying is that
/// the kernel — not a domain — is where this came from.
#[unsafe(no_mangle)]
extern "C" fn __molt_kernel_domain_fatal() -> ! {
    crate::emergency_write("MOLT_EXCEPTION: kernel fault on a domain gateway vector\n");
    crate::halt_forever()
}

pub fn gateway_stack_top() -> u64 {
    (&raw const __molt_domain_gateway_data_end) as u64
}

pub fn enter(state: &mut DomainState) -> Result<DomainExit, PlatformError> {
    if !state.started() {
        let code =
            bounds(&raw const __molt_domain_gateway_start, &raw const __molt_domain_gateway_end);
        let data = bounds(
            &raw const __molt_domain_gateway_data,
            &raw const __molt_domain_gateway_data_end,
        );
        let cr3 = memory::prepare_domain(state.view(), code, data, &interrupts::domain_tables())?;
        let (code, data) = interrupts::user_selectors();
        // The register file is shared by every domain, and the gateway restores
        // all of it. A domain that began holding the last one's registers would
        // read across a boundary `docs/threat-model.md` says it cannot.
        for slot in 0..REGISTER_SLOTS {
            write(REGISTERS + slot * 8, 0);
        }
        for offset in REPORTS {
            write(offset, 0);
        }
        write(DOMAIN_CR3, cr3);
        write(USER_RIP, state.entry());
        write(USER_RSP, state.stack());
        write(USER_RFLAGS, 1 << 1);
        write(USER_CS, u64::from(code));
        write(USER_SS, u64::from(data));
        for (offset, value) in [RDI, RSI, RDX, RCX, R8, R9].into_iter().zip(state.arguments()) {
            write(offset, value);
        }
        state.resume();
    }

    // SAFETY: the gateway is supervisor-only in both roots and the iret frame
    // names an admitted ring-3 image, stack, and user selectors.
    unsafe { __molt_domain_enter() };
    Ok(match read(REASON) {
        FROM_CALL => match read(CALL) {
            KICK => DomainExit::Ring,
            EXIT => DomainExit::Exited(read(VALUE) as i64),
            call => DomainExit::Unknown(call),
        },
        _ => DomainExit::Fault { cause: read(CAUSE), address: read(ADDRESS) },
    })
}

fn bounds(start: *const u8, end: *const u8) -> (u64, u64) {
    (start as u64, end as u64)
}

fn write(offset: usize, value: u64) {
    // SAFETY: the linker reserves one aligned gateway page, and domain entry
    // is serialized on the boot CPU.
    unsafe {
        (&raw mut __molt_domain_gateway_data).add(offset).cast::<u64>().write_volatile(value);
    }
}

fn read(offset: usize) -> u64 {
    // SAFETY: paired with `write` and the gateway's aligned stores.
    unsafe { (&raw const __molt_domain_gateway_data).add(offset).cast::<u64>().read_volatile() }
}
