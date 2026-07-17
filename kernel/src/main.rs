#![no_std]
#![no_main]

use core::fmt::Write;
#[cfg(target_arch = "x86_64")]
use core::future::Future;
use core::panic::PanicInfo;
#[cfg(target_arch = "x86_64")]
use core::pin::pin;
#[cfg(target_arch = "x86_64")]
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use molt_arch::{BootInfo, ExitStatus, Platform, SerialPort, SerialWriter};
#[cfg(target_arch = "x86_64")]
use molt_core::capability::{CapabilityError, CapabilityTable, ReadWrite};
#[cfg(target_arch = "x86_64")]
use molt_core::cell::{Cell, CellId, Supervisor};
#[cfg(target_arch = "x86_64")]
use molt_core::completion::{CompletionError, CompletionSlab};
#[cfg(target_arch = "x86_64")]
use molt_core::ring::{Completion, IoRing, Submission};

#[cfg(target_arch = "x86_64")]
molt_x86_64::entry_point!(kernel_main);

#[cfg(target_arch = "x86_64")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum KernelOp {
    TimerWait { initial_count: u32 },
}

#[cfg_attr(target_arch = "riscv64", allow(dead_code))]
fn kernel_main<P: Platform>(boot_info: BootInfo<'_>, platform: &mut P) -> ! {
    platform.serial().init();
    write_line(platform, format_args!("MOLT: booting"));
    write_line(platform, format_args!("MOLT: memory regions={}", boot_info.memory_map().len()));

    #[cfg(target_arch = "x86_64")]
    run_stage_1_checks(&boot_info, platform);

    write_line(platform, format_args!("MOLT_BOOT_OK"));
    platform.terminate(ExitStatus::Success)
}

#[cfg(target_arch = "x86_64")]
fn run_stage_1_checks<P: Platform>(boot_info: &BootInfo<'_>, platform: &mut P) {
    platform.initialize(boot_info).expect("initialize exception tables and local APIC");
    assert!(platform.verify_exception_path(), "breakpoint handler did not return");
    write_line(platform, format_args!("MOLT_EXCEPTION_OK"));

    platform.verify_owned_mapping(boot_info).expect("owned W^X mapping probe");
    write_line(platform, format_args!("MOLT_MAPPING_OK"));

    run_timer_future(platform);
    write_line(platform, format_args!("MOLT_TIMER_OK"));

    let slab = CompletionSlab::<u32, 2>::new();
    let cancelled = slab.reserve().expect("free cancellation slot");
    slab.cancel(cancelled).expect("active cancellation token");
    assert_eq!(
        slab.complete(cancelled.request_id(), 7),
        Err(CompletionError::Stale),
        "cancelled request accepted a stale completion"
    );
    write_line(platform, format_args!("MOLT_CANCELLATION_OK"));
    write_line(platform, format_args!("MOLT_STALE_COMPLETION_OK"));

    verify_cell_restart();
    write_line(platform, format_args!("MOLT_RESTART_OK"));
}

#[cfg(target_arch = "x86_64")]
fn run_timer_future<P: Platform>(platform: &mut P) {
    let slab = CompletionSlab::<u64, 2>::new();
    let token = slab.reserve().expect("free timer completion slot");
    let mut future = pin!(slab.wait(token));
    let waker = noop_waker();
    let mut context = Context::from_waker(&waker);
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
    platform.arm_timer(initial_count).expect("arm local APIC one-shot timer");
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

#[cfg(target_arch = "x86_64")]
#[derive(Default)]
struct ProbeState(u32);

#[cfg(target_arch = "x86_64")]
struct ProbeCell(ProbeState);

#[cfg(target_arch = "x86_64")]
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

#[cfg(target_arch = "x86_64")]
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

fn write_line<P: Platform>(platform: &mut P, arguments: core::fmt::Arguments<'_>) {
    let _ = writeln!(SerialWriter::new(platform.serial()), "{arguments}");
}

#[cfg(target_arch = "x86_64")]
fn noop_waker() -> Waker {
    const VTABLE: RawWakerVTable = RawWakerVTable::new(clone_waker, wake, wake, drop_waker);

    unsafe fn clone_waker(_: *const ()) -> RawWaker {
        RawWaker::new(core::ptr::null(), &VTABLE)
    }
    unsafe fn wake(_: *const ()) {}
    unsafe fn drop_waker(_: *const ()) {}

    // SAFETY: the vtable never dereferences its data pointer, owns no resources, and every clone
    // uses the same static vtable. The kernel polls again only after an interrupt-ring completion.
    unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) }
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
