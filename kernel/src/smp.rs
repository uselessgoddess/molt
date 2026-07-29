//! The cores this kernel runs on, and the executor each one owns.
//!
//! A core is given three things here and nothing else: its own tick, its own
//! executor, and a handle left where the others can find it. Nothing is shared
//! past that handle — no run queue, no lock, no stealing — so a core reaching
//! another one is always a message and a doorbell.

use alloc::boxed::Box;
use alloc::vec;
use core::cell::UnsafeCell;
use core::future::poll_fn;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use core::task::Poll;

use molt_arch::{CpuId, Local, Platform, Smp, Stack};
use molt_exec::{Executor, Handle, Machine};

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

#[cfg(target_arch = "x86_64")]
type Arch = molt_x86_64::X86_64;

#[cfg(target_arch = "riscv64")]
type Arch = molt_riscv::RiscV;

/// The machine, as an executor asks about it.
///
/// A second [`Arch`] beside the one `kernel_main` holds is not a second
/// machine: every question here is answered out of the asking core's own block
/// — its identity, its doorbell, its tick — which is hardware and not a field.
/// That is what lets one static answer every core.
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
        self.ready.load(Ordering::Acquire).then(|| {
            // SAFETY: published before the flag, and never written again.
            unsafe { (*self.handle.get()).clone() }
        })?
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
        let stack = stack();
        // SAFETY: the stack is leaked and handed to this core alone, and
        // `enter` is what every other core is already running.
        if unsafe { platform.start(cpu, stack, enter) }.is_ok() && settle(cpu) {
            running += 1;
        }
    }
    running
}

/// Sends a task to every other running core and waits for them to answer.
///
/// The whole crossing in one probe: a future made here, run there, and a waker
/// rung back over the doorbell. Returns how many answered, and how many were
/// asked.
pub(crate) fn crossing(exec: &Executor) -> (u16, u16) {
    static ANSWERED: AtomicU16 = AtomicU16::new(0);

    let asked = peers().count() as u16;
    let mut sent = false;
    let _ = exec.block_on(exec.timers().timeout(
        CROSSING,
        poll_fn(|context| {
            if !sent {
                sent = true;
                for peer in peers() {
                    let waker = context.waker().clone();
                    let _ = peer.spawn(async move {
                        ANSWERED.fetch_add(1, Ordering::Release);
                        waker.wake();
                    });
                }
            }
            match ANSWERED.load(Ordering::Acquire) >= asked {
                true => Poll::Ready(()),
                false => Poll::Pending,
            }
        }),
    ));
    (ANSWERED.load(Ordering::Acquire), asked)
}

/// Every core that reported an executor, this one aside.
fn peers() -> impl Iterator<Item = Handle> {
    let here = CORES.cpu().index();
    (0..MAX).filter(move |&index| index != here).filter_map(|index| HANDLES[index].get())
}

/// Where a started core lands: its own tick, its own executor, and whatever
/// the rest of the machine sends it from then on.
fn enter(_cpu: CpuId) -> ! {
    CORES.0.ticking().expect("this core's tick");
    attach().run()
}

/// Waits for `cpu` to report its executor, parking meanwhile.
///
/// The core waited on is the one that would deliver the wake, so there is no
/// future to await here — but there is a doorbell it rings and a tick that
/// bounds the wait, which is enough to sleep on and enough to give up on.
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

/// Carves a stack for a core out of the heap, for keeps.
///
/// Zeroed rather than built on this core's stack: a boxed array that size would
/// be written here first and copied there, and the boot stack has no room for
/// one.
fn stack() -> Stack {
    let bytes = Box::leak(vec![0u8; STACK].into_boxed_slice());
    let base = ptr::NonNull::from(&mut bytes[0]);
    // SAFETY: leaked, so it outlives the core, and handed out exactly once.
    unsafe { Stack::new(base, STACK) }
}
