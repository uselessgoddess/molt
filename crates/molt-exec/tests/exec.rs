use std::cell::Cell;
use std::future::pending;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::thread;

use molt_exec::{Executor, Machine, Priority, Solo, SpawnError};

static SOLO: Solo = Solo;
static CLOCK: Clock = Clock(AtomicU64::new(0));

/// A machine whose only trait is a clock the test winds.
struct Clock(AtomicU64);

impl Machine for Clock {
    fn ticks(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
}

#[test]
fn runs_spawned() -> Result<(), SpawnError> {
    let executor = Executor::new(&SOLO, 4);
    let seen = Rc::new(Cell::new(0));

    for _ in 0..3 {
        let seen = seen.clone();
        executor.spawn(async move { seen.set(seen.get() + 1) })?;
    }
    assert_eq!(executor.run_until_idle(), 3);
    assert_eq!(seen.get(), 3);
    Ok(())
}

#[test]
fn wake_requeues() -> Result<(), SpawnError> {
    let executor = Executor::new(&SOLO, 4);
    executor.spawn(Yield(false))?;

    assert_eq!(executor.run_until_idle(), 2);
    assert_eq!(executor.run_until_idle(), 0);
    Ok(())
}

#[test]
fn high_runs_first() -> Result<(), SpawnError> {
    let executor = Executor::new(&SOLO, 4);
    let order = Rc::new(RefLog::default());

    for (priority, mark) in [(Priority::Low, 'l'), (Priority::High, 'h')] {
        let order = order.clone();
        executor.spawn_with(priority, async move {
            for _ in 0..3 {
                order.push(mark);
                Yield(false).await;
            }
        })?;
    }
    executor.run_until_idle();

    assert_eq!(order.take(), "hhhlll");
    Ok(())
}

#[test]
fn full_refuses() -> Result<(), SpawnError> {
    let executor = Executor::new(&SOLO, 1);
    executor.spawn(async {})?;
    assert_eq!(executor.spawn(async {}), Err(SpawnError::Full));

    executor.run_until_idle();
    executor.spawn(async {})?;
    Ok(())
}

#[test]
fn drop_ends_tasks() -> Result<(), SpawnError> {
    let dropped = Rc::new(Cell::new(false));
    {
        let executor = Executor::new(&SOLO, 4);
        let guard = Guard(dropped.clone());
        executor.spawn(async move {
            let _guard = guard;
            pending::<()>().await
        })?;
        executor.run_until_idle();
        assert!(!dropped.get(), "the future is still parked");
    }
    assert!(dropped.get(), "the core that owned it dropped it");
    Ok(())
}

#[test]
fn remote_spawns() -> Result<(), SpawnError> {
    let executor = Executor::new(&SOLO, 4);
    let handle = executor.handle();
    let done = Arc::new(AtomicBool::new(false));

    let flag = done.clone();
    thread::spawn(move || handle.spawn(async move { flag.store(true, Ordering::Release) }))
        .join()
        .unwrap()?;
    executor.run_until_idle();

    assert!(done.load(Ordering::Acquire));
    Ok(())
}

#[test]
fn blocks_until_ready() -> Result<(), SpawnError> {
    let executor = Executor::new(&SOLO, 4);
    let latch = Latch::default();

    let opener = latch.clone();
    executor.spawn(async move { opener.open() })?;
    executor.block_on(latch.wait());
    Ok(())
}

#[test]
fn parks_until_woken() {
    let executor = Executor::new(&SOLO, 4);
    let latch = Latch::default();

    let opener = latch.clone();
    let thread = thread::spawn(move || opener.open());
    executor.block_on(latch.wait());
    thread.join().unwrap();
}

#[test]
fn sleeps_to_deadline() -> Result<(), SpawnError> {
    let executor = Executor::new(&CLOCK, 4);
    let timers = executor.timers().clone();
    let woke = Rc::new(Cell::new(false));

    let flag = woke.clone();
    executor.spawn(async move {
        timers.after(5).await;
        flag.set(true);
    })?;
    executor.run_until_idle();
    assert!(!woke.get());

    CLOCK.0.store(5, Ordering::Release);
    executor.tick();
    executor.run_until_idle();

    assert!(woke.get());
    Ok(())
}

/// Pending once, and ready the time after.
struct Yield(bool);

impl Future for Yield {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            return Poll::Ready(());
        }
        self.0 = true;
        context.waker().wake_by_ref();
        Poll::Pending
    }
}

struct Guard(Rc<Cell<bool>>);

impl Drop for Guard {
    fn drop(&mut self) {
        self.0.set(true);
    }
}

#[derive(Default)]
struct RefLog(Cell<String>);

impl RefLog {
    fn push(&self, mark: char) {
        let mut log = self.0.take();
        log.push(mark);
        self.0.set(log);
    }

    fn take(&self) -> String {
        self.0.take()
    }
}

/// A flag one side sets and the other awaits, across cores or not.
#[derive(Clone, Default)]
struct Latch(Arc<Gate>);

#[derive(Default)]
struct Gate {
    open: AtomicBool,
    waker: Mutex<Option<Waker>>,
}

impl Latch {
    fn open(&self) {
        // Set before the lock: a waiter that missed the flag still holds the
        // lock, so its waker is here by the time we take it.
        self.0.open.store(true, Ordering::Release);
        if let Some(waker) = self.0.waker.lock().unwrap().take() {
            waker.wake();
        }
    }

    fn wait(&self) -> Wait {
        Wait(self.clone())
    }
}

struct Wait(Latch);

impl Future for Wait {
    type Output = ();

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<()> {
        let mut waker = self.0.0.waker.lock().unwrap();
        if self.0.0.open.load(Ordering::Acquire) {
            return Poll::Ready(());
        }
        *waker = Some(context.waker().clone());
        Poll::Pending
    }
}
