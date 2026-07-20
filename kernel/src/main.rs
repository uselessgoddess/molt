#![no_std]
#![no_main]

use core::fmt::Write;
use core::future::Future;
use core::pin::pin;
use core::task::{Context, Poll, Waker};

use molt_arch::{BootInfo, ExitStatus, Platform, SerialPort, SerialWriter};
use molt_core::capability::{CapabilityError, CapabilityTable, ReadWrite};
use molt_core::cell::{Cell, CellId, Supervisor};
use molt_core::completion::{CompletionError, CompletionSlab};
use molt_core::ring::{Completion, IoRing, Submission};

#[cfg(target_arch = "x86_64")]
molt_x86_64::entry_point!(kernel_main);

#[cfg(target_arch = "riscv64")]
molt_riscv::entry_point!(kernel_main);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum KernelOp {
    TimerWait { initial_count: u32 },
}

fn write_line<P: Platform>(platform: &mut P, arguments: core::fmt::Arguments<'_>) {
    let _ = writeln!(SerialWriter::new(platform.serial()), "{arguments}");
}

macro_rules! println {
    ($dst:expr, $($arg:tt)*) => {
        write_line($dst, core::format_args!($($arg)*))
    };
}

fn kernel_main<P: Platform>(boot_info: BootInfo<'_>, platform: &mut P) -> ! {
    platform.serial().init();
    #[cfg(feature = "panic-smoke")]
    panic!("panic-smoke");

    println!(platform, "MOLT: booting");
    println!(platform, "MOLT: memory regions={}", boot_info.memory_map().len());

    smoke(&boot_info, platform);

    println!(platform, "MOLT_BOOT_OK");
    platform.terminate(ExitStatus::Success)
}

fn smoke<P: Platform>(boot_info: &BootInfo<'_>, platform: &mut P) {
    platform.initialize(boot_info).expect("initialize traps and timer source");
    assert!(platform.verify_exception_path(), "breakpoint handler did not return");
    println!(platform, "MOLT_EXCEPTION_OK");

    platform.verify_owned_mapping(boot_info).expect("owned W^X mapping probe");
    println!(platform, "MOLT_MAPPING_OK");

    run_timer_future(platform);
    println!(platform, "MOLT_TIMER_OK");

    let slab = CompletionSlab::<u32, 2>::new();
    let cancelled = slab.reserve().expect("free cancellation slot");
    slab.cancel(cancelled).expect("active cancellation token");
    assert_eq!(
        slab.complete(cancelled.request_id(), 7),
        Err(CompletionError::Stale),
        "cancelled request accepted a stale completion"
    );
    println!(platform, "MOLT_CANCELLATION_OK");
    println!(platform, "MOLT_STALE_COMPLETION_OK");

    verify_cell_restart();
    println!(platform, "MOLT_RESTART_OK");
}

fn run_timer_future<P: Platform>(platform: &mut P) {
    let slab = CompletionSlab::<u64, 2>::new();
    let token = slab.reserve().expect("free timer completion slot");
    let mut future = pin!(slab.wait(token));
    let mut context = Context::from_waker(Waker::noop());
    assert_eq!(future.as_mut().poll(&mut context), Poll::Pending);

    let mut ring = IoRing::<KernelOp, u64, 2>::new();
    let (mut client, mut timer_driver) = ring.split();
    client
        .try_submit(Submission::new(
            token.request_id(),
            KernelOp::TimerWait { initial_count: 1_000_000 },
        ))
        .expect("empty timer submission queue");

    let request = timer_driver.try_next().expect("submitted timer request");
    let KernelOp::TimerWait { initial_count } = *request.operation();
    let previous = platform.monotonic_ticks();
    platform.arm_timer(initial_count).expect("arm one-shot timer");
    while platform.monotonic_ticks() == previous {
        platform.wait_for_timer_change(previous);
    }
    let elapsed = platform.monotonic_ticks();
    timer_driver
        .try_complete(Completion::new(request.id(), elapsed))
        .expect("empty timer completion queue");

    let completion = client.try_completion().expect("interrupt-driven timer completion");
    slab.complete(completion.id(), completion.into_result()).expect("live timer request ID");
    assert_eq!(future.as_mut().poll(&mut context), Poll::Ready(Ok(elapsed)));
}

#[derive(Default)]
struct ProbeState(u32);

struct ProbeCell(ProbeState);

impl Cell for ProbeCell {
    type Message = u32;
    type Reply = u32;
    type State = ProbeState;

    fn spawn(state: Self::State) -> Self {
        Self(state)
    }

    fn handle(&mut self, increment: Self::Message) -> Self::Reply {
        self.0.0 += increment;
        self.0.0
    }
}

fn verify_cell_restart() {
    let owner = CellId::new(1);
    let mut capabilities = CapabilityTable::<u32, 2>::new();
    let old = capabilities.insert::<ReadWrite>(owner, 9).expect("free capability slot");
    let mut supervisor = Supervisor::<ProbeCell>::new(ProbeState(4));
    assert_eq!(supervisor.call(1), 5);

    assert_eq!(capabilities.revoke_owner(owner), 1);
    supervisor.restart_default();
    assert_eq!(supervisor.generation(), 1);
    assert_eq!(capabilities.get(old), Err(CapabilityError::Stale));
    assert_eq!(supervisor.call(2), 2);
}
