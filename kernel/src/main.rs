#![no_std]
#![no_main]

use core::fmt::Write;
use core::future::Future;
use core::pin::pin;
use core::task::{Context, Poll, Waker};

use molt_arch::memory::{Error, FrameTable, Inventory, Kind, Owner, Span};
use molt_arch::{
    BootInfo, ExitStatus, FRAME_SIZE, Platform, SerialPort, SerialWriter, UsableRegions,
};
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

    platform.verify_image_protection(boot_info).expect("kernel image obeys W^X");
    println!(platform, "MOLT_WX_OK");

    platform.verify_device_window(boot_info).expect("device window mapped and reachable");
    println!(platform, "MOLT_DEVICE_WINDOW_OK");

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

    let usable = verify_inventory(boot_info);
    println!(platform, "MOLT_PHYSMAP_OK");

    verify_frame_ownership(usable);
    println!(platform, "MOLT_FRAME_OWNER_OK");
}

const OWNED_FRAMES: u64 = 4;

fn verify_inventory(boot_info: &BootInfo<'_>) -> Span {
    let map = boot_info.memory_map();
    let inventory = Inventory::new(map);

    let usable = UsableRegions::above(map, FRAME_SIZE)
        .find(|range| range.end() - range.start() >= OWNED_FRAMES * FRAME_SIZE)
        .expect("one usable region of at least four frames");
    let span = Span::frames(usable.start(), OWNED_FRAMES).expect("aligned usable range");
    assert_eq!(inventory.classify(span), Ok(Kind::Ram), "usable RAM did not classify as RAM");

    let mut top = 0;
    let mut index = 0;
    while index < map.len() {
        if let Some(region) = map.region(index) {
            top = top.max(region.end().saturating_add(FRAME_SIZE - 1) / FRAME_SIZE * FRAME_SIZE);
        }
        index += 1;
    }
    let hole = Span::frames(top, 1).expect("aligned hole above the map");
    assert_eq!(inventory.classify(hole), Ok(Kind::Device), "a hole is not device memory");
    let window = inventory.device(hole).expect("device window above the map");
    assert_eq!(inventory.device(span), Err(Error::Kind), "RAM was handed out as a device window");
    assert_eq!(window.span(), hole);

    span
}

fn verify_frame_ownership(span: Span) {
    let mut slots = [None; OWNED_FRAMES as usize];
    let mut frames = FrameTable::over(span, &mut slots).expect("one slot per tracked frame");
    let first = Span::frames(span.start(), 2).expect("two frames of the tracked span");

    let claimed = frames.claim(first, Owner::Tables).expect("free frames");
    assert_eq!(frames.claim(first, Owner::Kernel), Err(Error::Owned), "frames handed out twice");
    assert_eq!(frames.owner(first.start()), Ok(Some(Owner::Tables)));
    assert_eq!(frames.claimed(), 2);

    frames.release(claimed).expect("frames this table issued");
    assert_eq!(frames.claimed(), 0, "released frames stayed claimed");
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
