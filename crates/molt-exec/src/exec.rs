//! The core's half of the executor: the tasks it owns and the turn it takes.
//!
//! Everything a neighbour can reach lives in [`Shared`], and it is three
//! atomics and a stack. Everything else — the ready queues, the task table, the
//! wheel — is behind `&self` on one core and never locked, because there is
//! nobody to lock it against.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::task::Wake;
use alloc::vec::Vec;
use core::array;
use core::cell::RefCell;
use core::pin::{Pin, pin};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use core::task::{Context, Poll, Waker};

use molt_core::cpu::CpuId;

use crate::machine::Machine;
use crate::queue::Inbox;
use crate::task::{Priority, Task};
use crate::timer::Timers;

/// Polls a level is given before the next one gets its turn. High is not
/// unbounded: a busy device queue must not be able to starve the rest.
const SLICE: [u32; Priority::LEVELS] = [32, 8, 2];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpawnError {
    /// The core is already running every task it was sized for.
    Full,
}

/// What another core is allowed to see of this one.
pub(crate) struct Shared {
    owner: CpuId,
    machine: &'static dyn Machine,
    capacity: usize,
    live: AtomicUsize,
    inbox: [Inbox; Priority::LEVELS],
}

impl Shared {
    /// Hands a task to its owner, and rings if the inbox needed it.
    ///
    /// One ring covers a whole burst, because the owner drains the burst
    /// whole: sixty-four completions from one interrupt are one interrupt
    /// back, not sixty-four.
    ///
    /// The doorbell is read off the block first, because the task may be the
    /// last thing holding the block: once it is in the inbox its owner is free
    /// to finish it, and this reference with it.
    pub(crate) fn queue(&self, task: Arc<Task>) {
        let (owner, machine) = (self.owner, self.machine);
        if self.inbox[task.priority().level()].push(task) {
            machine.wake(owner);
        }
    }

    fn claim(&self) -> Result<(), SpawnError> {
        let mut live = self.live.load(Ordering::Relaxed);
        loop {
            if live >= self.capacity {
                return Err(SpawnError::Full);
            }
            match self.live.compare_exchange_weak(
                live,
                live + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Ok(()),
                Err(actual) => live = actual,
            }
        }
    }

    fn free(&self) {
        self.live.fetch_sub(1, Ordering::Release);
    }

    fn pending(&self) -> bool {
        self.inbox.iter().any(|inbox| !inbox.empty())
    }
}

/// Where a core sends tasks, from a core that is not it.
#[derive(Clone)]
pub struct Handle {
    shared: Arc<Shared>,
}

impl Handle {
    pub fn cpu(&self) -> CpuId {
        self.shared.owner
    }

    /// Moves a future to the owning core and wakes it.
    ///
    /// This is the crossing, so this is where `Send` is asked for. A future
    /// that stays where it was made is spawned with [`Executor::spawn`] and
    /// proves nothing.
    pub fn spawn(
        &self,
        future: impl Future<Output = ()> + Send + 'static,
    ) -> Result<(), SpawnError> {
        self.spawn_with(Priority::default(), future)
    }

    pub fn spawn_with(
        &self,
        priority: Priority,
        future: impl Future<Output = ()> + Send + 'static,
    ) -> Result<(), SpawnError> {
        submit(&self.shared, priority, Box::pin(future))
    }
}

fn submit(
    shared: &Arc<Shared>,
    priority: Priority,
    future: Pin<Box<dyn Future<Output = ()>>>,
) -> Result<(), SpawnError> {
    shared.claim()?;
    Task::new(shared.clone(), priority, future).schedule();
    Ok(())
}

/// The ready queues, which only the owner ever looks at.
struct Local {
    ready: [VecDeque<Arc<Task>>; Priority::LEVELS],
    level: usize,
    credit: u32,
}

impl Local {
    /// Room for every task at every level, taken once. A queue that grows is a
    /// queue that allocates on the path an interrupt is waiting on.
    fn new(capacity: usize) -> Self {
        Self {
            ready: array::from_fn(|_| VecDeque::with_capacity(capacity)),
            level: 0,
            credit: SLICE[0],
        }
    }

    /// Round robin by weight: a level keeps the core until its slice or its
    /// queue runs out, and every level is reached within one pass.
    fn take(&mut self) -> Option<Arc<Task>> {
        for _ in 0..2 * Priority::LEVELS {
            if self.credit == 0 || self.ready[self.level].is_empty() {
                self.level = (self.level + 1) % Priority::LEVELS;
                self.credit = SLICE[self.level];
                continue;
            }
            self.credit -= 1;
            return self.ready[self.level].pop_front();
        }
        None
    }
}

/// One core's executor: its tasks, its queues, its clock.
pub struct Executor {
    shared: Arc<Shared>,
    local: RefCell<Local>,
    /// A strong reference per live task, so the future is dropped here and
    /// nowhere else. The task remembers its index, so leaving is a swap.
    tasks: RefCell<Vec<Arc<Task>>>,
    timers: Timers,
}

impl Executor {
    /// Sized at run time: room for `capacity` tasks, taken up front and never
    /// again, so nothing after this asks the heap for a place to put one.
    pub fn new(machine: &'static dyn Machine, capacity: usize) -> Self {
        Self {
            shared: Arc::new(Shared {
                owner: machine.cpu(),
                machine,
                capacity,
                live: AtomicUsize::new(0),
                inbox: [const { Inbox::new() }; Priority::LEVELS],
            }),
            local: RefCell::new(Local::new(capacity)),
            tasks: RefCell::new(Vec::with_capacity(capacity)),
            timers: Timers::new(machine),
        }
    }

    pub fn cpu(&self) -> CpuId {
        self.shared.owner
    }

    pub fn handle(&self) -> Handle {
        Handle { shared: self.shared.clone() }
    }

    pub fn timers(&self) -> &Timers {
        &self.timers
    }

    /// Spawns a future that never leaves this core, so it is never made to
    /// prove it could.
    pub fn spawn(&self, future: impl Future<Output = ()> + 'static) -> Result<(), SpawnError> {
        self.spawn_with(Priority::default(), future)
    }

    pub fn spawn_with(
        &self,
        priority: Priority,
        future: impl Future<Output = ()> + 'static,
    ) -> Result<(), SpawnError> {
        submit(&self.shared, priority, Box::pin(future))
    }

    /// Walks the wheel to the machine's clock, waking whatever came due.
    pub fn tick(&self) -> usize {
        self.timers.advance()
    }

    /// Polls until nothing is ready, and reports how many polls that took.
    pub fn run_until_idle(&self) -> usize {
        let mut polls = 0;
        loop {
            self.collect();
            let Some(task) = self.local.borrow_mut().take() else {
                return polls;
            };
            if task.run() {
                self.retire(&task);
            }
            polls += 1;
        }
    }

    /// Runs this core until it is switched off, halting whenever it is idle.
    pub fn run(&self) -> ! {
        loop {
            if self.tick() + self.run_until_idle() == 0 && !self.shared.pending() {
                self.shared.machine.park();
            }
        }
    }

    /// Drives `future` to its value, running the rest of the core meanwhile.
    ///
    /// This is what a driver waits on: no spinning, no polling by count — the
    /// core parks and an interrupt brings it back.
    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        let mut future = pin!(future);
        let blocker = Arc::new(Blocker::new(self.shared.owner, self.shared.machine));
        let waker = Waker::from(blocker.clone());
        let mut context = Context::from_waker(&waker);
        loop {
            if blocker.answered() {
                if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
                    return value;
                }
            }
            let ran = self.tick() + self.run_until_idle();
            if ran == 0 && !blocker.rung() && !self.shared.pending() {
                self.shared.machine.park();
            }
        }
    }

    /// Empties the inbox into the ready queues, adopting anything new.
    fn collect(&self) {
        for priority in Priority::ALL {
            let level = priority.level();
            if self.shared.inbox[level].empty() {
                continue;
            }
            let mut local = self.local.borrow_mut();
            let mut tasks = self.tasks.borrow_mut();
            for task in self.shared.inbox[level].drain() {
                if !task.held() {
                    task.hold(tasks.len());
                    tasks.push(task.clone());
                }
                local.ready[level].push_back(task);
            }
        }
    }

    fn retire(&self, task: &Arc<Task>) {
        if !task.held() {
            return;
        }
        task.release();
        let mut tasks = self.tasks.borrow_mut();
        let slot = task.slot();
        tasks.swap_remove(slot);
        if let Some(moved) = tasks.get(slot) {
            moved.moved(slot);
        }
        self.shared.free();
    }
}

impl Drop for Executor {
    /// Futures die on the core that owned them, whether they finished or not:
    /// a handle outliving the executor must find nothing left to drop.
    fn drop(&mut self) {
        for task in self.tasks.borrow().iter() {
            task.finish();
        }
        for inbox in &self.shared.inbox {
            for task in inbox.drain() {
                task.finish();
            }
        }
    }
}

/// The waker [`Executor::block_on`] hands out: a flag, and the doorbell that
/// keeps a park from sleeping through it.
struct Blocker {
    rung: AtomicBool,
    owner: CpuId,
    machine: &'static dyn Machine,
}

impl Blocker {
    fn new(owner: CpuId, machine: &'static dyn Machine) -> Self {
        Self { rung: AtomicBool::new(true), owner, machine }
    }

    fn rung(&self) -> bool {
        self.rung.load(Ordering::Acquire)
    }

    fn answered(&self) -> bool {
        self.rung.swap(false, Ordering::AcqRel)
    }
}

impl Wake for Blocker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.rung.store(true, Ordering::Release);
        self.machine.wake(self.owner);
    }
}
