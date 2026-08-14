//! The cores this kernel runs on, and the executor each one owns.
//!
//! A core gets three things and nothing else: its own tick, its own executor,
//! and a handle left where the others can find it. Nothing is shared past that
//! handle — no run queue, no lock, no stealing — so one core reaching another is
//! always a message and a doorbell.

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::future::poll_fn;
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};
use core::task::Poll;

use molt_arch::va::Epoch;
use molt_arch::{CpuId, Local, Platform, Shootdown, Smp, Stack, Tlb};
use molt_core::peers::Peers;
use molt_rt::{Executor, Handle, Machine};

/// Blocks the platforms keep, which is the ceiling on cores molt numbers.
pub(crate) const MAX: usize = 8;

/// Tasks one core can hold at once.
const CAPACITY: usize = 32;

/// Bytes of stack a started core runs on.
const STACK: usize = 64 * 1024;

/// Ticks a starting core is given to report its executor.
const STARTUP: u64 = 64;

/// Ticks the crossing probe waits for the cores it sent work to.
const CROSSING: u64 = 64;

/// Answers one ring holds. Rings are made per ask and a core asked once posts
/// once, so one is every answer an ask can bring.
const DEPTH: usize = 1;

#[cfg(target_arch = "x86_64")]
type Arch = molt_x86_64::X86_64;

#[cfg(target_arch = "riscv64")]
type Arch = molt_riscv::RiscV;

/// The machine, as an executor asks about it.
///
/// A second [`Arch`] beside the one `kernel_main` holds is not a second machine:
/// every answer comes out of the asking core's own block — identity, doorbell,
/// tick — which is hardware rather than a field, so one static answers all.
struct Cores(Arch);

static CORES: Cores = Cores(Arch::new());

impl Machine for Cores {
    fn cpu(&self) -> CpuId {
        self.0.cpu()
    }

    /// A refused wake names a core that is not there, which no waker can do
    /// anything about.
    fn wake(&self, cpu: CpuId) {
        let _ = self.0.wake(cpu);
    }

    fn park(&self) {
        self.0.park();
    }

    fn ticks(&self) -> u64 {
        self.0.ticks()
    }
}

/// Where a core leaves its handle for the others.
struct Slot {
    ready: AtomicBool,
    handle: UnsafeCell<Option<Handle>>,
}

// SAFETY: the owner writes once before `ready`, and a reader only looks after
// it — the one flag orders the pair, and nothing writes twice.
unsafe impl Sync for Slot {}

impl Slot {
    const fn new() -> Self {
        Self { ready: AtomicBool::new(false), handle: UnsafeCell::new(None) }
    }

    fn publish(&self, handle: Handle) {
        // SAFETY: this core's own slot, written before the flag anyone reads.
        unsafe { *self.handle.get() = Some(handle) };
        self.ready.store(true, Ordering::Release);
    }

    fn get(&self) -> Option<Handle> {
        if !self.ready.load(Ordering::Acquire) {
            return None;
        }
        // SAFETY: published before the flag, and never written again.
        unsafe { (*self.handle.get()).clone() }
    }
}

static HANDLES: [Slot; MAX] = [const { Slot::new() }; MAX];

/// Gives this core an executor and leaves its handle for the others.
///
/// Called once per core, on the core: the executor is built where it will run,
/// because everything inside it but the inbox is single-threaded on purpose.
pub(crate) fn attach() -> &'static Executor {
    let exec: &'static Executor = Box::leak(Box::new(Executor::new(&CORES, CAPACITY)));
    // SAFETY: leaked, so it outlives the core, and installed once on it.
    unsafe { Arch::install(ptr::from_ref(exec).cast_mut().cast()) };
    HANDLES[exec.cpu().index()].publish(exec.handle());
    // Whoever started this core is waiting on that flag with its doorbell armed.
    CORES.wake(CpuId::BOOT);
    exec
}

/// Which core this is, out of its own block.
///
/// Answerable before anything is attached — the platform installs the block on
/// its way in — which is what lets the heap route by it.
pub(crate) fn here() -> usize {
    CORES.cpu().index()
}

/// Which core this is, as the rest of the kernel names cores.
pub(crate) fn cpu() -> CpuId {
    CORES.cpu()
}

/// This core's executor.
pub(crate) fn current() -> &'static Executor {
    let block = Arch::block();
    assert!(!block.is_null(), "a core reached its executor before attaching one");
    // SAFETY: `attach` leaked it on this core, and nothing else installs here.
    unsafe { &*block.cast::<Executor>() }
}

/// Starts every core firmware described, and waits for each to report in.
///
/// Returns how many are running, this one included. A core that refuses to
/// start or never reports costs the parallelism it would have brought and
/// nothing else.
pub(crate) fn start<P: Platform>(platform: &mut P) -> u16 {
    let mut running = 1;
    for index in 1..platform.cpus().min(MAX as u16) {
        let cpu = CpuId::new(index);
        // SAFETY: the stack is leaked and handed to this core alone, and
        // `enter` is what every other core is already running.
        let started = unsafe { platform.start(cpu, stack(), enter) };
        if started.is_ok() && settle(cpu) {
            running += 1;
        }
    }
    running
}

/// Asks `cores` for an answer each, and collects what comes back before `ticks`
/// runs out. Also returns how many were asked.
///
/// The ask is a future spawned down that core's handle; the answer is a message
/// on the ring that pair alone shares, plus a waker rung over this core's
/// doorbell. Nothing two cores share here is reachable by a third.
///
/// The rings outlive the call on purpose: a core that never reached its task
/// still holds the end it would have posted on.
pub(crate) fn ask<T, U, F>(
    exec: &Executor,
    cores: impl Iterator<Item = (usize, Handle)>,
    ticks: u64,
    answer: F,
) -> (Vec<T>, u16)
where
    T: Send + 'static,
    U: Future<Output = T> + Send + 'static,
    F: Fn() -> U + Copy + Send + 'static,
{
    let rings: &'static mut Peers<T, DEPTH, MAX> = Box::leak(Box::new(Peers::new(exec.cpu())));
    let (senders, mut inbox) = rings.split();
    let mut senders = senders.map(Some);
    let mut cores = Some(cores);
    let mut asked = 0;
    let mut answers = Vec::new();

    let _ = exec.block_on(exec.timers().timeout(
        ticks,
        poll_fn(|context| {
            for (index, core) in cores.take().into_iter().flatten() {
                let Some(mut sender) = senders[index].take() else { continue };
                let waker = context.waker().clone();
                let sent = core.spawn(async move {
                    let answer = answer().await;
                    let _ = sender.post(answer);
                    // The ring is only half of it: what the ask is parked in
                    // wakes on this, not on the doorbell the post would ring.
                    waker.wake();
                });
                asked += u16::from(sent.is_ok());
            }
            while let Some(answer) = inbox.take() {
                answers.push(answer);
            }
            if answers.len() as u16 >= asked { Poll::Ready(()) } else { Poll::Pending }
        }),
    ));
    (answers, asked)
}

/// Sends a task to every other running core and waits for them to answer, which
/// each does with the identity its own block reports — so an answer is proof the
/// task ran *there* rather than that a counter moved. Returns answered, asked.
pub(crate) fn crossing(exec: &Executor) -> (u16, u16) {
    let here = CORES.cpu();
    let (answers, asked) = ask(exec, peers(), CROSSING, || async { CORES.cpu() });

    assert!(answers.iter().all(|&cpu| cpu != here), "a core answered as another");
    (answers.len() as u16, asked)
}

/// Drops every core's cached translations, and says which ones answered.
///
/// This core flushes inline, being the likeliest to hold the entry it just
/// walked; every other running core runs the same instruction as a task on its
/// own executor. Returns the cores that flushed, this one first, and how many
/// were asked — a core that was asked and never answered is missing from the
/// list, which is what keeps its epoch unretired.
pub(crate) fn flush(exec: &Executor) -> (Vec<CpuId>, u16) {
    Arch::flush();
    let (peers, asked) = ask(exec, peers(), CROSSING, || async {
        Arch::flush();
        CORES.cpu()
    });

    (core::iter::once(CORES.cpu()).chain(peers).collect(), asked)
}

/// Flushes every attending core and closes `round`, yielding the epoch whose
/// addresses are now safe to retire.
///
/// A core that took the flush and never answered, and a round closing while
/// cores still owe one, are both the use-after-free this protocol exists to
/// prevent, so both are refused here rather than at each caller.
pub(crate) fn close(exec: &Executor, round: &mut Shootdown) -> Epoch {
    let (flushed, asked) = flush(exec);
    assert_eq!(flushed.len() as u16, asked + 1, "a core took the flush and never answered");

    let mut retirable = None;
    for cpu in flushed {
        assert!(retirable.is_none(), "the round closed with cores still owing a flush");
        retirable = round.acknowledge(cpu).expect("a core this round asked");
    }
    retirable.expect("the epoch every core has now flushed")
}

/// Every core a shootdown has to reach: this one, and each that reported an
/// executor.
pub(crate) fn attending() -> impl Iterator<Item = CpuId> {
    core::iter::once(CORES.cpu()).chain(peers().map(|(index, _)| CpuId::new(index as u16)))
}

/// Every core that reported an executor, this one aside.
pub(crate) fn peers() -> impl Iterator<Item = (usize, Handle)> {
    let here = CORES.cpu().index();
    (0..MAX)
        .filter(move |&index| index != here)
        .filter_map(|index| Some((index, HANDLES[index].get()?)))
}

/// Where a started core lands: its own tick, its own executor, and whatever
/// the rest of the machine sends it from then on.
fn enter(_cpu: CpuId) -> ! {
    CORES.0.ticking().expect("this core's tick");
    attach().run()
}

/// Waits for `cpu` to report its executor, parking meanwhile.
///
/// No future to await: the core waited on is the one that would deliver the
/// wake. A doorbell to sleep on and a tick to give up on is enough.
fn settle(cpu: CpuId) -> bool {
    let deadline = CORES.ticks() + STARTUP;
    while HANDLES[cpu.index()].get().is_none() {
        if CORES.ticks() >= deadline {
            return false;
        }
        CORES.park();
    }
    true
}

/// Carves a stack for a core out of the heap, for keeps. Zeroed rather than
/// built on this core's stack, which has no room for an array that size.
fn stack() -> Stack {
    let bytes = Box::leak(vec![0u8; STACK].into_boxed_slice());
    let base = ptr::NonNull::from(&mut bytes[0]);
    // SAFETY: leaked, so it outlives the core, and handed out exactly once.
    unsafe { Stack::new(base, STACK) }
}
