//! Bringing an application processor from reset to Rust.
//!
//! A core comes out of reset in real mode at `vector << 12`, knowing nothing:
//! no tables, no long mode, not even a stack. The blob below is the shortest
//! path from there into the kernel's address space, and the boot core copies
//! it into the frame [`memory::trampoline`] reserved, answers appended.
//!
//! Nothing in the blob is patched instruction by instruction. The one absolute
//! address it needs is its own, and it lives in the descriptor table it
//! carries: the 32-bit code segment is based at the frame, so every label
//! stays the offset the assembler already computed.

use core::arch::global_asm;
use core::hint::spin_loop;
use core::sync::atomic::{Ordering, fence};

use molt_arch::{CpuId, Entry, SmpError, Stack};

use crate::{apic, interrupts, memory, percpu};

/// How long the boot core waits, in spins, for a core to answer.
///
/// There is no clock yet to measure the milliseconds the manual asks for, and
/// a core answers by running: generous enough not to give up on a slow one,
/// bounded so a dead one does not hang the boot.
const SPINS: u32 = 1 << 22;

/// A frame holds the blob and the stack the blob pushes on.
const FRAME: usize = 4096;

unsafe extern "C" {
    static __molt_ap_start: u8;
    static __molt_ap_end: u8;
    static __molt_ap_params: u8;
}

global_asm!(
    r#"
.pushsection .rodata, "a"
.balign 16
.globl __molt_ap_start
__molt_ap_start:
.code16
    cli
    cld
    // `cs` is the frame the startup interrupt named, and the only thing this
    // core knows about itself. Everything below is an offset from it.
    movw %cs, %ax
    movw %ax, %ds
    movw %ax, %ss
    movzwl %ax, %eax
    shll $4, %eax
    movl %eax, %ebp

    // The base the 32-bit code runs at, and the table's own linear address —
    // which is the frame plus where the table sits in it, not the frame.
    movw %ax, (__molt_ap_gdt - __molt_ap_start + 10)
    shrl $16, %eax
    movb %al, (__molt_ap_gdt - __molt_ap_start + 12)
    leal (__molt_ap_gdt - __molt_ap_start)(%ebp), %eax
    movl %eax, (__molt_ap_gdtr - __molt_ap_start + 2)

    lgdtl (__molt_ap_gdtr - __molt_ap_start)
    movl %cr0, %eax
    orl $1, %eax
    movl %eax, %cr0
    ljmpl $0x08, $(__molt_ap_protected - __molt_ap_start)

.code32
__molt_ap_protected:
    movl $0x10, %eax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    leal (__molt_ap_stack - __molt_ap_start)(%ebp), %esp

    movl %cr4, %eax
    orl $(1 << 5), %eax               // physical address extension
    movl %eax, %cr4
    movl (__molt_ap_params - __molt_ap_start)(%ebp), %eax
    movl %eax, %cr3
    movl $0xc0000080, %ecx            // extended feature enable
    rdmsr
    // No-execute goes on with long mode, not after it: the kernel's tables mark
    // every data page, and to a core that never enabled it that bit is reserved.
    orl $(1 << 8 | 1 << 11), %eax
    wrmsr

    // Pushed before paging, which is the last write this frame takes: with the
    // kernel's tables live it is executable and nothing more, and a far return
    // only reads its stack.
    pushl $0x18
    leal (__molt_ap_long - __molt_ap_start)(%ebp), %eax
    pushl %eax
    movl %cr0, %eax
    orl $0x80000001, %eax             // paging, and protection it already had
    movl %eax, %cr0
    lretl

.code64
__molt_ap_long:
    movq (__molt_ap_params - __molt_ap_start + 8)(%rbp), %rsp
    leaq (__molt_ap_params - __molt_ap_start)(%rbp), %rdi
    jmpq *(__molt_ap_params - __molt_ap_start + 16)(%rbp)

.balign 16
__molt_ap_gdt:
    .quad 0
    .quad 0x00cf9b000000ffff          // 32-bit code, based at the frame
    .quad 0x00cf93000000ffff          // flat data
    .quad 0x00af9b000000ffff          // 64-bit code
__molt_ap_gdt_end:
__molt_ap_gdtr:
    .word __molt_ap_gdt_end - __molt_ap_gdt - 1
    .long 0
.balign 16
.globl __molt_ap_params
__molt_ap_params:
    .space 40
    .space 64                         // the eight bytes the far return needs
__molt_ap_stack:
.globl __molt_ap_end
__molt_ap_end:
.popsection
"#,
    options(att_syntax)
);

/// What the trampoline reads, in the order it reads it.
#[repr(C)]
#[allow(dead_code, reason = "the trampoline reads what Rust only writes")]
struct Params {
    /// The kernel's tables, loaded in 32 bits — so, under four gigabytes.
    root: u64,
    stack: u64,
    /// [`enter`], because the blob has no way to name a Rust function.
    enter: unsafe extern "C" fn(*const Params) -> !,
    cpu: u64,
    /// What the kernel wanted run on this core.
    hand: Entry,
}

/// Starts `cpu` on `stack` and enters `entry` there.
///
/// # Safety
///
/// `stack` must be nothing else's, and `entry` must be ready to run beside
/// everything the caller is doing.
pub unsafe fn start(cpu: CpuId, stack: Stack, entry: Entry) -> Result<(), SmpError> {
    let target = percpu::of(cpu).ok_or(SmpError::Unknown)?;
    if target.up() {
        return Err(SmpError::Running);
    }
    let pad = memory::trampoline().ok_or(SmpError::Unsupported)?;
    // The core writes `CR3` in 32-bit mode, where the register is 32 bits wide.
    if pad.root() >= 1 << 32 {
        return Err(SmpError::Unsupported);
    }

    // SAFETY: the blob is one object between its own symbols, and the frame is
    // a whole page the kernel reserved and nothing else maps.
    unsafe {
        let blob = &raw const __molt_ap_start;
        let bytes = (&raw const __molt_ap_end) as usize - blob as usize;
        if bytes > FRAME {
            return Err(SmpError::Unsupported);
        }
        blob.copy_to_nonoverlapping(pad.page(), bytes);

        let params = (&raw const __molt_ap_params) as usize - blob as usize;
        pad.page().add(params).cast::<Params>().write(Params {
            root: pad.root(),
            stack: stack.top() as u64,
            enter,
            cpu: u64::from(cpu.get()),
            hand: entry,
        });
    }
    // The core reads the frame with no idea a write ever happened to it.
    fence(Ordering::SeqCst);

    let apic = target.apic();
    let vector = (pad.physical() >> 12) as u8;
    apic::init_ipi(apic).map_err(|_| SmpError::Refused)?;
    settle(|| false);
    // Twice, as firmware does: the first is dropped by a core still taking the
    // reset, the second ignored by one already running.
    for _ in 0..2 {
        apic::startup_ipi(apic, vector).map_err(|_| SmpError::Refused)?;
        if settle(|| target.up()) {
            return Ok(());
        }
    }
    Err(SmpError::Refused)
}

/// Spins until `ready`, and reports whether it ever was.
fn settle(ready: impl Fn() -> bool) -> bool {
    (0..SPINS).any(|_| {
        spin_loop();
        ready()
    })
}

/// Where a core arrives: molt's address space live, and nothing else set up.
///
/// # Safety
///
/// Called by the trampoline, on the core it started, with `params` addressing
/// the frame that core came up on.
unsafe extern "C" fn enter(params: *const Params) -> ! {
    // Copied out first: the frame is the next core's the moment `attach` below
    // reports this one up.
    // SAFETY: the trampoline passes back the block the boot core wrote for it.
    let params = unsafe { params.read() };
    let cpu = CpuId::new(params.cpu as u16);
    // SAFETY: this is that core, and this is its only call.
    unsafe { percpu::attach(cpu) };
    interrupts::init();
    let _ = apic::init(apic::window());
    (params.hand)(cpu)
}

#[cfg(test)]
mod tests {
    use super::{FRAME, Params};

    #[test]
    fn params_fit_what_asm_reads() {
        assert_eq!(size_of::<Params>(), 40, "the trampoline reads five words");
        assert!(size_of::<Params>() < FRAME);
    }
}
