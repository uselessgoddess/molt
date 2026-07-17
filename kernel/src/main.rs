#![no_std]
#![no_main]

use core::fmt::Write;
use core::panic::PanicInfo;

use molt_arch::{BootInfo, ExitStatus, Platform, SerialPort, SerialWriter};
use molt_core::ring::{Completion, IoRing, RequestId, Submission};

#[cfg(target_arch = "x86_64")]
molt_x86_64::entry_point!(kernel_main);

// The RISC-V platform is compile-checked before its boot adapter is implemented.
#[cfg_attr(target_arch = "riscv64", allow(dead_code))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum KernelOp {
    TimerWait { deadline_ticks: u64 },
}

#[cfg_attr(target_arch = "riscv64", allow(dead_code))]
fn kernel_main<P: Platform>(boot_info: BootInfo<'_>, platform: &mut P) -> ! {
    platform.serial().init();
    {
        let mut serial = SerialWriter::new(platform.serial());
        let _ = writeln!(serial, "MOLT: booting");
        let _ = writeln!(serial, "MOLT: memory regions={}", boot_info.memory_map().len());

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
    }
    platform.terminate(ExitStatus::Success)
}

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    #[cfg(target_arch = "x86_64")]
    {
        molt_x86_64::panic(info)
    }

    #[cfg(target_arch = "riscv64")]
    {
        molt_riscv::panic(info)
    }
}
