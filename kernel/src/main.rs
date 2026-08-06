#![no_std]
#![no_main]

use alloc::boxed::Box;
use core::convert::Infallible;
use core::pin::pin;
use core::sync::atomic::{AtomicBool, Ordering};
use core::task::{Context, Poll, Waker};

use molt_arch::asid::{Asids, Flush};
use molt_arch::memory::{Error, FrameTable, Inventory, Kind, Owner, Rights, Span};
use molt_arch::refcount::{self, Leaves, Run};
use molt_arch::shootdown::Shootdown;
use molt_arch::va::{Class, Extent, Hole, Region, Space};
use molt_arch::{
    BootInfo, ExitStatus, FRAME_SIZE, Platform, PlatformError, SerialPort, SerialWriter,
    UsableRegions, view,
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
mod isolation;
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

    report_ram(boot_info, platform);

    verify_heap(boot_info, platform);

    platform.verify_owned_mapping(boot_info).expect("owned W^X mapping probe");
    report!(platform, "MOLT_MAPPING_OK");

    let analyzer = verify_address_space(platform);
    verify_refcounts(platform, &analyzer);

    platform.verify_image_protection(boot_info).expect("kernel image obeys W^X");
    report!(platform, "MOLT_WX_OK");

    report_huge_map(boot_info, platform);

    platform.verify_device_window(boot_info).expect("device window mapped and reachable");
    report!(platform, "MOLT_DEVICE_WINDOW_OK");

    let exec = verify_exec(platform);
    report!(platform, "MOLT_EXEC_OK");

    let (running, answered) = verify_smp(platform, exec);
    report!(platform, "MOLT_SMP_OK: cores={running} answered={answered}");

    verify_shootdown(platform, exec);
    verify_domain(boot_info, platform, exec);

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

/// Prints how much RAM firmware said the machine has.
///
/// This is a marker rather than a log line because it is the only evidence that
/// the number came from firmware at all. A kernel that carries a constant boots
/// identically on the machine the constant was written for, and silently wastes
/// or invents memory everywhere else; a number that tracks `-m` cannot be that
/// constant. Both platforms print it, and neither computes it.
fn report_ram<P: Platform>(boot_info: &BootInfo<'_>, platform: &mut P) {
    let map = boot_info.memory_map();
    let (mut usable, mut top) = (0u64, 0);
    for range in UsableRegions::above(map, FRAME_SIZE) {
        usable += range.end() - range.start();
        top = top.max(range.end());
    }

    // The top comes first because it is the part a test can pin: how much is
    // usable moves with the size of the image sitting in front of it.
    report!(platform, "MOLT_RAM_OK: top {top:#x}, {} MiB usable", usable >> 20);
}

/// Prints the biggest leaf RAM is mapped through, read back out of the tables.
///
/// A mapper that quietly fell back to small pages boots identically and passes
/// every other marker, so nothing else here would notice: the cost shows up
/// only as TLB misses under a program touching more memory than the boot smoke
/// ever does. The size is asked of the platform, which reads it back from the
/// live tables, so this is what the hardware translates through, not what the
/// mapper meant to write.
fn report_huge_map<P: Platform>(boot_info: &BootInfo<'_>, platform: &mut P) {
    let leaf = platform.largest_ram_leaf(boot_info).expect("RAM leaves readable back");
    let (size, unit) = scale(leaf.size());
    report!(platform, "MOLT_HUGE_MAP_OK: {size} {unit} leaf at {:#x}", leaf.start());
}

/// The largest binary unit `bytes` divides evenly, so a marker reads `1 GiB`
/// rather than `1048576 KiB` and stays legible when the leaf size changes.
fn scale(bytes: u64) -> (u64, &'static str) {
    for (unit, name) in [(1 << 30, "GiB"), (1 << 20, "MiB"), (1 << 10, "KiB")] {
        if bytes % unit == 0 {
            return (bytes / unit, name);
        }
    }
    (bytes, "B")
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

/// What the tier-2 example in `docs/address-space.md` asks for: a log analyzer
/// that wants a hundred gigabytes of logs addressable at once.
const ANALYZER: u64 = 100 << 30;

/// Free ranges per class, which is the budget `docs/va-allocator.md` sizes:
/// 24 bytes apiece, 64 per class, 4 608 bytes of kernel stack in total.
const HOLES: usize = 3 * 64;

/// Cuts the one global address space out of what the hardware turned out to
/// support, and proves an extent survives the round trip that a revoke is.
///
/// The claim is not that the allocator works — `molt-arch` tests that on the
/// host — but that the width it was cut from is the machine's own answer, and
/// that the addresses a booted kernel hands out of it are the ones the tests
/// describe. See `docs/va-allocator.md`.
fn verify_address_space<P: Platform>(platform: &mut P) -> Extent {
    let widths = platform.address_space().expect("the platform probed its own translation");
    let mut holes = [Hole::EMPTY; HOLES];
    let mut space = Space::over(widths.address(), &mut holes).expect("a space wide enough to cut");

    // A hart that implements only Sv39 has a 32 GiB gigabyte arena and cannot
    // seat the analyzer. It proves the same round trip with what it does have,
    // and the marker prints which it was.
    let wanted = ANALYZER.min(space.largest(Class::Giga));
    let extent = space.allocate(Class::Giga, wanted).expect("room in the gigabyte arena");
    let start = extent.start();
    let leaves = extent.leaves();
    assert_eq!(start % Class::Giga.granule(), 0, "a gigabyte extent came back unaligned");
    assert_eq!(leaves, wanted.div_ceil(Class::Giga.granule()), "the extent needs other leaves");

    // Giving it back does not give the addresses back: until every hart has
    // flushed, one of them may still translate through the mapping that was
    // revoked, and handing the range to someone else would hand them the pages.
    space.release(extent).expect("an extent this space issued");
    let during = space.allocate(Class::Giga, wanted).expect("room beside the quarantined range");
    assert_ne!(during.start(), start, "an unflushed range was handed out again");
    space.release(during).expect("an extent this space issued");

    let epoch = space.sweep();
    space.retire(epoch);
    let again = space.allocate(Class::Giga, wanted).expect("the flushed range, back in service");
    assert_eq!(again.start(), start, "a flushed range did not come back");
    assert_eq!(space.quarantined(Class::Giga), 0, "a retired epoch left bytes in quarantine");

    report!(
        platform,
        "MOLT_VA_OK: {} address bits, {} GiB at {start:#x} in {} leaves",
        widths.address(),
        wanted >> 30,
        leaves,
    );

    // The tag budget is the domain budget: a grant or a revoke costs the
    // shootdown above, not a tag, so this counts domains and nothing else.
    let mut asids = Asids::new(widths.asid());
    let grant = asids.assign();
    assert!(asids.live(grant.asid()), "a fresh tag was born stale");
    assert_eq!(
        grant.flush() == Flush::Everything,
        asids.capacity() == 0,
        "a hart with tags to give still flushed, or one without tags did not"
    );

    report!(platform, "MOLT_ASID_OK: bits={} domains={}", widths.asid(), asids.capacity());

    again
}

/// How many records the leaf counts get: three per class is enough for the
/// splits a revoke of part of one leaf goes through, with room to spare.
const RUNS: usize = 16;

/// Counts the leaves of that same extent the way a grant and a revoke would.
///
/// The claim under test is the keying, not the arithmetic — `molt-arch` tests
/// the arithmetic on the host. What a booted kernel shows is the size of the
/// thing being counted: a hundred gigabytes shared with a second view is one
/// record holding the number two, and the 26 million frames underneath it never
/// get a record at all. See `docs/address-space.md`.
fn verify_refcounts<P: Platform>(platform: &mut P, analyzer: &Extent) {
    let mut runs = [Run::EMPTY; RUNS];
    let mut leaves = Leaves::over(&mut runs);
    let start = analyzer.start();

    leaves.map(start, analyzer.class(), analyzer.leaves()).expect("leaves nobody counts yet");
    let mapped = leaves.leaves();
    let frames = leaves.frames();
    assert_eq!(leaves.runs(), 1, "leaves mapped together were counted apart");

    // A grant of the whole extent into a second view: every leaf gains the same
    // holder, so the accounting still fits in the one record it started in.
    leaves.share(analyzer.region()).expect("every leaf of a mapped extent");
    let shared = leaves.runs();
    assert_eq!(shared, 1, "a grant of everything fragmented the accounting");
    assert_eq!(leaves.count(start), Some(2), "the second view was not counted");
    assert_eq!(leaves.count(analyzer.end() - 1), Some(2), "the grant stopped short of the end");

    // Revoking part of a gigabyte leaf is a question the tables cannot answer
    // either, until the leaf becomes the 512 below it.
    let part = Region::new(start, start + 2 * Class::Mega.granule()).expect("two megabytes");
    assert_eq!(leaves.share(part), Err(refcount::Error::Straddle), "half a leaf was counted");
    assert_eq!(leaves.split(start), Ok(Class::Mega), "a gigabyte leaf did not split");
    assert_eq!(leaves.leaves(), mapped - 1 + Class::FANOUT, "the split lost addresses");

    let reclaimed = leaves.release(part).expect("leaves the second view holds");
    assert!(reclaimed.is_empty(), "a leaf the first view still holds was reported free");
    assert_eq!(leaves.count(start), Some(1), "the revoke did not reach the second view");
    assert_eq!(leaves.count(start + part.bytes()), Some(2), "the revoke reached past its range");

    report!(
        platform,
        "MOLT_REFCOUNT_OK: {} GiB in {mapped} leaves and {shared} record, {frames} frames \
         uncounted; revoking 2 MiB split one leaf into {} and left {} records",
        analyzer.bytes() >> 30,
        Class::FANOUT,
        leaves.runs(),
    );
}

/// Frees an extent the way a revoke does, and holds its addresses back until
/// every core has flushed for real.
///
/// This is the one property the host tests cannot show, because it needs other
/// cores: the epoch is retired by acknowledgements that came from the cores
/// themselves, each having run the flush instruction on its own hardware. The
/// order is the one `docs/threat-model.md` asks after — the leaf goes first, the
/// shootdown second, and [`retire`](Space::retire) only once nobody owes a
/// flush. Reversing the last two is a use-after-free the hardware performs for
/// whoever reads the address next.
fn verify_shootdown<P: Platform>(platform: &mut P, exec: &Executor) {
    let widths = platform.address_space().expect("the platform probed its own translation");
    let mut holes = [Hole::EMPTY; HOLES];
    let mut space = Space::over(widths.address(), &mut holes).expect("a space wide enough to cut");

    let wanted = ANALYZER.min(space.largest(Class::Giga));
    let extent = space.allocate(Class::Giga, wanted).expect("room in the gigabyte arena");
    let start = extent.start();

    // The revoke: the addresses leave the view and join the batch a flush has to
    // cover before any of them can be handed out again.
    space.release(extent).expect("an extent this space issued");
    let epoch = space.sweep();

    let mut shootdown = Shootdown::new();
    let cores = shootdown.begin(epoch, smp::attending()).expect("a core to flush");
    assert!(shootdown.pending(smp::cpu()), "the core that did the unmapping was trusted");

    // Meanwhile the range is nobody's to give: an allocation of the same size
    // has to come back from somewhere else entirely.
    let elsewhere = space.allocate(Class::Giga, wanted).expect("room beside the quarantined range");
    assert_ne!(elsewhere.start(), start, "an unflushed range was handed out again");

    let (flushed, asked) = smp::flush(exec);
    assert_eq!(flushed.len() as u16, asked + 1, "a core took the flush and never answered");

    let mut retirable = None;
    for cpu in flushed {
        assert!(retirable.is_none(), "the round closed with cores still owing a flush");
        retirable = shootdown.acknowledge(cpu).expect("a core this round asked");
    }
    let retired = retirable.expect("the epoch every core has now flushed");

    assert_eq!(retired, epoch, "a round retired an epoch it was not opened for");
    assert_eq!(shootdown.outstanding(), 0, "a core still owes a flush for a retired epoch");
    assert_eq!(space.quarantined(Class::Giga), wanted, "the freed range left quarantine early");

    space.retire(retired);
    assert_eq!(space.quarantined(Class::Giga), 0, "a flushed range stayed in quarantine");
    let again = space.allocate(Class::Giga, wanted).expect("the flushed range, back in service");
    assert_eq!(again.start(), start, "a flushed range did not come back");

    report!(
        platform,
        "MOLT_SHOOTDOWN_OK: {} GiB at {start:#x} held over {cores} cores until epoch {} flushed",
        wanted >> 30,
        retired.get(),
    );
}

/// The class a grant is proven at.
///
/// Megabytes and not gigabytes because this extent is backed for real: the
/// smoke gives QEMU two gigabytes, so there are no frames behind a gigabyte
/// leaf, and granting addresses nothing backs would prove nothing about a
/// grant.
const GRANTED: Class = Class::Mega;

/// Opens a second view, moves an extent into it, and takes it back.
///
/// This is tier 2 of `docs/address-space.md` doing the thing it exists for. The
/// claim being demonstrated is not that a mapping can be made — the kernel's own
/// tables show that at boot — but that a *second* view exists which does not
/// contain the kernel, that an extent can enter it without a byte moving, and
/// that it can be taken back out in the order a revoke has to go in.
///
/// The grant is real memory at a real address: frames claimed out of the same
/// RAM the kernel maps, reached from the view at the same global address the
/// kernel calls them by, which is the whole of what one address space buys.
fn verify_domain<P: Platform>(boot_info: &BootInfo<'_>, platform: &mut P, exec: &Executor) {
    let widths = platform.address_space().expect("the platform probed its own translation");
    let grant = Asids::new(widths.asid()).assign();
    let view = platform.open_view(grant.asid()).expect("a root for a second view");

    // What a fresh view holds, which is nothing. Three addresses the kernel is
    // demonstrably using right now — the code running this, the stack it runs
    // on, and the heap it allocates from — and not one of them is reachable
    // from the view that was just opened.
    let here = verify_domain::<P> as *const () as u64;
    let stack = (&raw const widths) as u64;
    let heap = Box::into_raw(Box::new(0u64));
    for (address, what) in
        [(here, "kernel text"), (stack, "the kernel stack"), (heap as u64, "the kernel heap")]
    {
        assert!(platform.resident(view, address).is_none(), "a fresh view could reach {what}");
    }
    // SAFETY: the box was leaked one statement ago and nothing else holds it.
    drop(unsafe { Box::from_raw(heap) });

    report!(
        platform,
        "MOLT_DOMAIN_OK: view {} tagged {} in generation {}",
        view.index(),
        grant.asid().value(),
        grant.asid().generation(),
    );
    report!(platform, "MOLT_DOMAIN_ABSENT_OK: kernel text, stack, and heap unreachable from it");

    // The backing. Twice the leaf is claimed and the leaf is cut out of the
    // aligned part of it, because a firmware map starts a usable region wherever
    // it likes and a megabyte leaf has to begin on a megabyte.
    let granule = GRANTED.granule();
    let claimed =
        platform.claim_ram(boot_info, 2 * granule / FRAME_SIZE).expect("RAM to back a grant");
    let base =
        molt_arch::align_up(claimed.start(), granule).expect("an aligned base below the end");
    let span = Span::new(base, base + granule).expect("a leaf's worth of claimed frames");

    let mut holes = [Hole::EMPTY; HOLES];
    let mut space = Space::over(widths.address(), &mut holes).expect("a space wide enough to cut");
    let extent = space.allocate(GRANTED, granule).expect("room in the megabyte arena");
    let start = extent.start();

    platform.grant(view, &extent, span, Rights::READ_WRITE).expect("a leaf the view lacked");
    let leaf = platform.resident(view, start).expect("the granted address, in the view's tables");
    assert_eq!(leaf.start(), start, "the grant landed somewhere else");
    assert_eq!(leaf.size(), granule, "the grant was cut smaller than the extent asked for");
    assert!(leaf.protection().is_write(), "a read-write grant arrived read-only");
    assert!(!leaf.protection().is_execute(), "a data grant arrived executable");
    assert!(platform.resident(view, extent.end() - 1).is_some(), "the grant stopped short");
    assert!(platform.resident(view, extent.end()).is_none(), "the grant ran past its extent");
    assert!(platform.resident(view, here).is_none(), "a grant of RAM brought the kernel along");

    report!(
        platform,
        "MOLT_GRANT_OK: {} MiB at {start:#x} from frames at {base:#x}, in {} leaf",
        extent.bytes() >> 20,
        extent.leaves(),
    );

    // The revoke, in the order `molt_arch::view` spells out. Step one clears the
    // leaves and nothing else: the addresses are still spoken for, because a
    // core that walked them a moment ago may still hold the translation.
    let cleared = platform.revoke(view, &extent).expect("leaves this view holds");
    assert_eq!(cleared, extent.leaves(), "a revoke took a different number of leaves");
    assert!(platform.resident(view, start).is_none(), "a revoked address still translated");
    assert!(
        matches!(platform.revoke(view, &extent), Err(PlatformError::View(view::Error::Absent))),
        "a second revoke found something left to take"
    );

    space.release(extent).expect("an extent this space issued");
    let epoch = space.sweep();
    let mut shootdown = Shootdown::new();
    let cores = shootdown.begin(epoch, smp::attending()).expect("a core to flush");
    let held = space.allocate(GRANTED, granule).expect("room beside the quarantined range");
    assert_ne!(held.start(), start, "a revoked range was handed out before the flush");
    space.release(held).expect("an extent this space issued");

    // Step two: every core drops what it cached, and says so itself.
    let (flushed, asked) = smp::flush(exec);
    assert_eq!(flushed.len() as u16, asked + 1, "a core took the flush and never answered");
    let mut retirable = None;
    for cpu in flushed {
        assert!(retirable.is_none(), "the round closed with cores still owing a flush");
        retirable = shootdown.acknowledge(cpu).expect("a core this round asked");
    }
    let retired = retirable.expect("the epoch every core has now flushed");

    // Step three, and not one instruction sooner.
    space.retire(retired);
    let again = space.allocate(GRANTED, granule).expect("the flushed range, back in service");
    assert_eq!(again.start(), start, "a flushed range did not come back");

    report!(
        platform,
        "MOLT_REVOKE_OK: {cleared} leaf out of view {}, held over {cores} cores until epoch {} \
         flushed",
        view.index(),
        retired.get(),
    );
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
