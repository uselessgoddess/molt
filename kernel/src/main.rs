#![no_std]
#![no_main]

use core::arch::asm;
use core::fmt::{self, Write};
use core::panic::PanicInfo;

use bootloader_api::{BootInfo, entry_point};
use molt_core::ring::{Completion, IoRing, RequestId, Submission};

entry_point!(kernel_main);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum KernelOp {
    TimerWait { deadline_ticks: u64 },
}

fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    let mut serial = SerialPort::com1();
    serial.init();
    let _ = writeln!(serial, "MOLT: booting");
    let _ = writeln!(serial, "MOLT: memory regions={}", boot_info.memory_regions.len());

    let mut ring = IoRing::<KernelOp, u32, 8>::new();
    let (mut client, mut timer_driver) = ring.split();
    let request = Submission::new(RequestId::new(1), KernelOp::TimerWait { deadline_ticks: 0 });
    client.try_submit(request).expect("empty submission queue");

    let submitted = timer_driver.try_next().expect("submitted timer request");
    let KernelOp::TimerWait { deadline_ticks } = *submitted.operation();
    timer_driver
        .try_complete(Completion::new(submitted.id(), deadline_ticks as u32))
        .expect("empty completion queue");
    let completion = client.try_completion().expect("timer completion");

    let _ = writeln!(
        serial,
        "MOLT: ring request={} result={}",
        completion.id().get(),
        completion.result()
    );
    let _ = writeln!(serial, "MOLT_BOOT_OK");
    exit_qemu(QemuExitCode::Success)
}

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    let mut serial = SerialPort::com1();
    serial.init();
    let _ = writeln!(serial, "MOLT_PANIC: {info}");
    exit_qemu(QemuExitCode::Failure)
}

#[derive(Clone, Copy)]
#[repr(u32)]
enum QemuExitCode {
    Success = 0x10,
    Failure = 0x11,
}

fn exit_qemu(code: QemuExitCode) -> ! {
    // SAFETY: 0xf4 is the explicitly configured QEMU isa-debug-exit port.
    unsafe {
        out_u32(0xf4, code as u32);
    }
    loop {
        // SAFETY: the kernel has no work after reporting its terminal result.
        unsafe {
            asm!("hlt", options(nomem, nostack));
        }
    }
}

struct SerialPort {
    base: u16,
}

impl SerialPort {
    const fn com1() -> Self {
        Self { base: 0x3f8 }
    }

    fn init(&mut self) {
        // SAFETY: these are the standard 16550 UART registers for COM1.
        unsafe {
            out_u8(self.base + 1, 0x00);
            out_u8(self.base + 3, 0x80);
            out_u8(self.base, 0x03);
            out_u8(self.base + 1, 0x00);
            out_u8(self.base + 3, 0x03);
            out_u8(self.base + 2, 0xc7);
            out_u8(self.base + 4, 0x0b);
        }
    }

    fn write_byte(&mut self, byte: u8) {
        // SAFETY: reading the line status and writing the data register are the
        // defined operations for the initialized COM1 UART.
        unsafe {
            while in_u8(self.base + 5) & 0x20 == 0 {
                core::hint::spin_loop();
            }
            out_u8(self.base, byte);
        }
    }
}

impl Write for SerialPort {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for byte in text.bytes() {
            self.write_byte(byte);
        }
        Ok(())
    }
}

unsafe fn out_u8(port: u16, value: u8) {
    // SAFETY: callers validate that `port` belongs to the device they control.
    unsafe {
        asm!(
            "out dx, al",
            in("dx") port,
            in("al") value,
            options(nomem, nostack, preserves_flags)
        );
    }
}

unsafe fn out_u32(port: u16, value: u32) {
    // SAFETY: callers validate that `port` belongs to the device they control.
    unsafe {
        asm!(
            "out dx, eax",
            in("dx") port,
            in("eax") value,
            options(nomem, nostack, preserves_flags)
        );
    }
}

unsafe fn in_u8(port: u16) -> u8 {
    let value: u8;
    // SAFETY: callers validate that `port` belongs to the device they control.
    unsafe {
        asm!(
            "in al, dx",
            in("dx") port,
            out("al") value,
            options(nomem, nostack, preserves_flags)
        );
    }
    value
}
