//! RISC-V user entry through one supervisor-only gateway page.

use core::arch::{asm, global_asm};

use molt_arch::{DomainExit, DomainState, PlatformError};

use crate::paging;

const KICK: u64 = 0;
const EXIT: u64 = 1;

// Gateway data offsets. Registers x1..x31 begin at 0x80.
const DOMAIN_SATP: usize = 0x28;
const REASON: usize = 0x30;
const VALUE: usize = 0x38;
const CAUSE: usize = 0x40;
const ADDRESS: usize = 0x48;
const USER_SEPC: usize = 0x50;
const USER_SSTATUS: usize = 0x58;
const REGISTERS: usize = 0x80;

global_asm!(
    r#"
.pushsection .text.domain_gateway, "ax"
.balign 4096
.global __molt_domain_gateway_start
__molt_domain_gateway_start:
.global __molt_domain_trap
__molt_domain_trap:
    csrw sscratch, t0
    la t0, __molt_domain_gateway_data
    sd x1,  0x88(t0)
    sd x2,  0x90(t0)
    sd x3,  0x98(t0)
    sd x4,  0xa0(t0)
    sd x6,  0xb0(t0)
    sd x7,  0xb8(t0)
    sd x8,  0xc0(t0)
    sd x9,  0xc8(t0)
    sd x10, 0xd0(t0)
    sd x11, 0xd8(t0)
    sd x12, 0xe0(t0)
    sd x13, 0xe8(t0)
    sd x14, 0xf0(t0)
    sd x15, 0xf8(t0)
    sd x16, 0x100(t0)
    sd x17, 0x108(t0)
    sd x18, 0x110(t0)
    sd x19, 0x118(t0)
    sd x20, 0x120(t0)
    sd x21, 0x128(t0)
    sd x22, 0x130(t0)
    sd x23, 0x138(t0)
    sd x24, 0x140(t0)
    sd x25, 0x148(t0)
    sd x26, 0x150(t0)
    sd x27, 0x158(t0)
    sd x28, 0x160(t0)
    sd x29, 0x168(t0)
    sd x30, 0x170(t0)
    sd x31, 0x178(t0)
    csrr t1, sscratch
    sd t1, 0xa8(t0)
    csrr t1, sepc
    sd t1, 0x50(t0)
    csrr t1, sstatus
    sd t1, 0x58(t0)
    csrr t1, scause
    sd t1, 0x40(t0)
    csrr t1, stval
    sd t1, 0x48(t0)

    ld t1, 0x40(t0)
    li t2, 8
    bne t1, t2, 1f
    ld t1, 0x50(t0)
    addi t1, t1, 4
    sd t1, 0x50(t0)
    ld t1, 0x108(t0)
    sd t1, 0x30(t0)
    ld t1, 0xd0(t0)
    sd t1, 0x38(t0)
    j 2f
1:
    li t1, 2
    sd t1, 0x30(t0)
2:
    ld t1, 0x00(t0)
    csrw satp, t1
    sfence.vma
    ld t1, 0x18(t0)
    csrw stvec, t1
    ld t1, 0x20(t0)
    csrw sstatus, t1
    ld sp, 0x08(t0)
    ld ra, 0x10(t0)
    ret

.global __molt_domain_enter
__molt_domain_enter:
    la t0, __molt_domain_gateway_data
    csrr t1, satp
    sd t1, 0x00(t0)
    sd sp, 0x08(t0)
    sd ra, 0x10(t0)
    csrr t1, stvec
    sd t1, 0x18(t0)
    csrr t1, sstatus
    sd t1, 0x20(t0)
    la t1, __molt_domain_trap
    csrw stvec, t1
    ld t1, 0x58(t0)
    csrw sstatus, t1
    ld t1, 0x50(t0)
    csrw sepc, t1
    ld t1, 0x28(t0)
    csrw satp, t1
    sfence.vma

    ld x1,  0x88(t0)
    ld x2,  0x90(t0)
    ld x3,  0x98(t0)
    ld x4,  0xa0(t0)
    ld x7,  0xb8(t0)
    ld x8,  0xc0(t0)
    ld x9,  0xc8(t0)
    ld x10, 0xd0(t0)
    ld x11, 0xd8(t0)
    ld x12, 0xe0(t0)
    ld x13, 0xe8(t0)
    ld x14, 0xf0(t0)
    ld x15, 0xf8(t0)
    ld x16, 0x100(t0)
    ld x17, 0x108(t0)
    ld x18, 0x110(t0)
    ld x19, 0x118(t0)
    ld x20, 0x120(t0)
    ld x21, 0x128(t0)
    ld x22, 0x130(t0)
    ld x23, 0x138(t0)
    ld x24, 0x140(t0)
    ld x25, 0x148(t0)
    ld x26, 0x150(t0)
    ld x27, 0x158(t0)
    ld x28, 0x160(t0)
    ld x29, 0x168(t0)
    ld x30, 0x170(t0)
    ld x31, 0x178(t0)
    ld t1, 0xb0(t0)
    csrw sscratch, t1
    ld t0, 0xa8(t0)
    csrr t1, sscratch
    sret
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
"#
);

unsafe extern "C" {
    static __molt_domain_gateway_start: u8;
    static __molt_domain_gateway_end: u8;
    static mut __molt_domain_gateway_data: u8;
    static __molt_domain_gateway_data_end: u8;
    fn __molt_domain_enter();
}

pub fn enter(state: &mut DomainState) -> Result<DomainExit, PlatformError> {
    if !state.started() {
        let code =
            bounds(&raw const __molt_domain_gateway_start, &raw const __molt_domain_gateway_end);
        let data = bounds(
            &raw const __molt_domain_gateway_data,
            &raw const __molt_domain_gateway_data_end,
        );
        let satp = paging::prepare_domain(state.view(), code, data)?;
        write(DOMAIN_SATP, satp);
        write(USER_SEPC, state.entry());
        let mut status: usize;
        // SAFETY: reading sstatus has no side effects. SPP and SPIE are cleared
        // below so `sret` enters U-mode with interrupts masked.
        unsafe {
            asm!("csrr {status}, sstatus", status = out(reg) status, options(nomem, nostack))
        };
        write(USER_SSTATUS, (status & !((1 << 8) | (1 << 5))) as u64);
        write(register(2), state.stack());
        for (number, value) in (10..16).zip(state.arguments()) {
            write(register(number), value);
        }
        state.resume();
    }

    // SAFETY: the gateway pages are present supervisor-only in both roots, and
    // the saved context names admitted user pages.
    unsafe { __molt_domain_enter() };
    let reason = read(REASON);
    let value = read(VALUE);
    Ok(match reason {
        KICK => DomainExit::Ring,
        EXIT => DomainExit::Exited(value as i64),
        _ => DomainExit::Fault { cause: read(CAUSE), address: read(ADDRESS) },
    })
}

const fn register(number: usize) -> usize {
    REGISTERS + number * 8
}

fn bounds(start: *const u8, end: *const u8) -> (u64, u64) {
    (start as u64, end as u64)
}

fn write(offset: usize, value: u64) {
    // SAFETY: the linker reserves one page at this symbol and every offset is
    // an aligned u64 inside it. Domain entry is serialized on the boot hart.
    unsafe {
        (&raw mut __molt_domain_gateway_data).add(offset).cast::<u64>().write_volatile(value);
    }
}

fn read(offset: usize) -> u64 {
    // SAFETY: paired with `write` and the assembly gateway's aligned stores.
    unsafe { (&raw const __molt_domain_gateway_data).add(offset).cast::<u64>().read_volatile() }
}
