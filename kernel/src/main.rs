#![no_std]
#![no_main]

use alloc::boxed::Box;
use core::convert::Infallible;
use core::pin::pin;
use core::sync::atomic::{AtomicBool, Ordering};
use core::task::{Context, Poll, Waker};

use molt_arch::memory::{Error, FrameTable, Inventory, Kind, Owner, Span};
use molt_arch::{
    BootInfo, ExitStatus, FRAME_SIZE, Platform, SerialPort, SerialWriter, UsableRegions,
};
use molt_core::capability::{CapabilityError, CapabilityTable, ReadWrite};
use molt_core::cell::{Cell, CellId, Handler, RestartHooks, Supervisor};
use molt_core::completion::{CompletionError, CompletionSlab};
use molt_core::ring::{Completion, IoRing, Submission};
use molt_exec::Executor;

extern crate alloc;

mod device;
mod heap;
mod init;
mod network;
mod nvme;
mod pci;
mod smp;
mod virtio;

use molt_kernel::report;

#[cfg(target_arch = "x86_64")]
molt_x86_64::entry_point!(kernel_main);

#[cfg(target_arch = "riscv64")]
molt_riscv::entry_point!(kernel_main);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum KernelOp {
    TimerWait { ticks: u64 },
}

fn kernel_main<P: Platform>(boot_info: BootInfo<'_>, platform: &mut P) -> ! {
    platform.serial().init();
    #[cfg(feature = "panic-smoke")]
    panic!("panic-smoke");

    report!(platform, "MOLT: booting");
    report!(platform, "MOLT: memory regions={}", boot_info.memory_map().len());

    smoke(&boot_info, platform);

    report!(platform, "MOLT_BOOT_OK");
    platform.terminate(ExitStatus::Success)
}

fn smoke<P: Platform>(boot_info: &BootInfo<'_>, platform: &mut P) {
    platform.initialize(boot_info).expect("initialize traps and timer source");
    assert!(platform.verify_exception_path(), "breakpoint handler did not return");
    report!(platform, "MOLT_EXCEPTION_OK");

    verify_heap(boot_info, platform);

    platform.verify_owned_mapping(boot_info).expect("owned W^X mapping probe");
    report!(platform, "MOLT_MAPPING_OK");

    platform.verify_image_protection(boot_info).expect("kernel image obeys W^X");
    report!(platform, "MOLT_WX_OK");

    platform.verify_device_window(boot_info).expect("device window mapped and reachable");
    report!(platform, "MOLT_DEVICE_WINDOW_OK");

    let exec = verify_exec(platform);
    report!(platform, "MOLT_EXEC_OK");

    let (running, answered) = verify_smp(platform, exec);
    report!(platform, "MOLT_SMP_OK: cores={running} answered={answered}");

    run_timer_future(exec);
    report!(platform, "MOLT_TIMER_OK");

    let slab = CompletionSlab::<u32, 2>::new();
    let cancelled = slab.reserve().expect("free cancellation slot");
    slab.cancel(cancelled).expect("active cancellation token");
    assert_eq!(
        slab.complete(cancelled.request_id(), 7),
        Err(CompletionError::Stale),
        "cancelled request accepted a stale completion"
    );
    report!(platform, "MOLT_CANCELLATION_OK");
    report!(platform, "MOLT_STALE_COMPLETION_OK");

    verify_cell_restart();
    report!(platform, "MOLT_RESTART_OK");

    let usable = verify_inventory(boot_info);
    report!(platform, "MOLT_PHYSMAP_OK");

    verify_frame_ownership(usable);
    report!(platform, "MOLT_FRAME_OWNER_OK");

    pci::smoke(boot_info, platform);
    virtio::smoke(boot_info, platform);
    nvme::smoke(boot_info, platform);
    network::smoke(boot_info, platform);
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

/// Donates the boot heap and proves an allocation round-trips through it.
fn verify_heap<P: Platform>(boot_info: &BootInfo<'_>, platform: &mut P) {
    let bytes = heap::init(boot_info, platform).expect("RAM for the kernel heap");

    // `black_box` keeps the release build from eliding a box it can prove is
    // never read: the accounting it drives is the whole point of the probe.
    let probe = core::hint::black_box(Box::new([0x4du8; 64]));
    assert!(heap::used() >= probe.len(), "a live box left the heap empty");
    assert!(probe.iter().all(|&byte| byte == 0x4d), "the heap handed back other bytes");
    drop(core::hint::black_box(probe));
    assert_eq!(heap::used(), 0, "a dropped box left the heap holding bytes");

    report!(platform, "MOLT_HEAP_OK: {bytes} bytes");
}

/// Starts this core's tick and its executor, and proves a task runs on it.
fn verify_exec<P: Platform>(platform: &mut P) -> &'static Executor {
    static RAN: AtomicBool = AtomicBool::new(false);

    platform.ticking().expect("this core's tick");
    let exec = smp::attach();
    exec.spawn(async { RAN.store(true, Ordering::Release) }).expect("a free task slot");

    assert_eq!(exec.run_until_idle(), 1, "the spawned task was never polled");
    assert!(RAN.load(Ordering::Acquire), "the polled task did not run");
    exec
}

/// Starts the other cores, and proves work crosses to every one that came up.
fn verify_smp<P: Platform>(platform: &mut P, exec: &Executor) -> (u16, u16) {
    let running = smp::start(platform);
    let (answered, asked) = smp::crossing(exec);

    assert_eq!(asked, running - 1, "a running core left no handle to reach it by");
    assert_eq!(answered, asked, "a core took work and never answered");
    (running, answered)
}

fn run_timer_future(exec: &Executor) {
    let slab = CompletionSlab::<u64, 2>::new();
    let token = slab.reserve().expect("free timer completion slot");
    let mut future = pin!(slab.wait(token));
    let mut context = Context::from_waker(Waker::noop());
    assert_eq!(future.as_mut().poll(&mut context), Poll::Pending);

    let mut ring = IoRing::<KernelOp, u64, 2>::new();
    let (mut client, mut timer_driver) = ring.split();
    client
        .try_submit(Submission::new(token.request_id(), KernelOp::TimerWait { ticks: 2 }))
        .expect("empty timer submission queue");

    let request = timer_driver.try_next().expect("submitted timer request");
    let KernelOp::TimerWait { ticks } = *request.operation();
    // The wait is the executor's: the core parks, its tick brings it back, and
    // the wheel is what says the deadline arrived.
    let elapsed = exec.block_on(async {
        exec.timers().after(ticks).await;
        exec.timers().now()
    });
    timer_driver
        .try_complete(Completion::new(request.id(), elapsed))
        .expect("empty timer completion queue");

    let completion = client.try_completion().expect("interrupt-driven timer completion");
    slab.complete(completion.id(), completion.into_result()).expect("live timer request ID");
    assert_eq!(future.as_mut().poll(&mut context), Poll::Ready(Ok(elapsed)));
}

struct ProbeCell(u32);

impl Cell for ProbeCell {
    type Error = Infallible;
    type State = u32;

    fn spawn(start: Self::State) -> Result<Self, Infallible> {
        Ok(Self(start))
    }

    fn restart(&mut self) -> Result<(), Infallible> {
        self.0 = 0;
        Ok(())
    }
}

impl Handler for ProbeCell {
    type Message = u32;
    type Reply = u32;

    fn handle(&mut self, increment: Self::Message) -> Self::Reply {
        self.0 += increment;
        self.0
    }
}

/// Drops what the restarting cell handed out, which is what the filesystem's
/// supervisor will hook up to its handle table.
struct Revoke<'t>(&'t mut CapabilityTable<u32, 2>, CellId);

impl RestartHooks for Revoke<'_> {
    fn stop_submissions(&mut self) {}

    fn cancel_requests(&mut self) {}

    fn revoke_capabilities(&mut self) {
        assert_eq!(self.0.revoke_owner(self.1), 1, "the cell exported one capability");
    }
}

fn verify_cell_restart() {
    let owner = CellId::new(1);
    let mut capabilities = CapabilityTable::<u32, 2>::new();
    let old = capabilities.insert::<ReadWrite>(owner, 9).expect("free capability slot");
    let mut supervisor = Supervisor::<ProbeCell>::new(4).unwrap();
    assert_eq!(supervisor.call(1), 5);

    supervisor.restart(&mut Revoke(&mut capabilities, owner)).unwrap();

    assert_eq!(supervisor.generation(), 1);
    assert_eq!(capabilities.get(old), Err(CapabilityError::Stale));
    assert_eq!(supervisor.call(2), 2);
}
