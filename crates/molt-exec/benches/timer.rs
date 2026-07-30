use std::hint::black_box;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Waker};
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use molt_exec::{Machine, Sleep, Timers};

static CLOCK: Clock = Clock(AtomicU64::new(0));

/// Deadlines a burst arms at once.
const BURST: usize = 64;

/// Ticks that put a deadline one level up, so expiring it cascades down.
const CASCADE: u64 = 70;

/// A machine whose only trait is a clock the bench winds.
struct Clock(AtomicU64);

impl Machine for Clock {
    fn ticks(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// Arming and cancelling, each iteration paying the tick that reclaims it: what
/// a timeout costs when the thing it guards answers in time.
fn arm(criterion: &mut Criterion) {
    let timers = wound();

    criterion.bench_function("timer/arm", |bencher| {
        bencher.iter(|| {
            let sleep = black_box(timers.after(black_box(2)));
            drop(sleep);
            CLOCK.0.fetch_add(1, Ordering::Relaxed);
            timers.advance()
        });
    });
}

/// The floor every core pays per tick, with nothing armed at all.
fn tick(criterion: &mut Criterion) {
    let timers = wound();

    criterion.bench_function("timer/tick", |bencher| {
        bencher.iter(|| {
            CLOCK.0.fetch_add(1, Ordering::Relaxed);
            timers.advance()
        });
    });
}

fn expire(criterion: &mut Criterion) {
    let timers = wound();

    criterion.bench_function("timer/expire", |bencher| {
        bencher.iter_custom(|iters| walk(&timers, iters, 1));
    });
}

fn cascade(criterion: &mut Criterion) {
    let timers = wound();

    criterion.bench_function("timer/cascade", |bencher| {
        bencher.iter_custom(|iters| walk(&timers, iters, CASCADE));
    });
}

/// Times `iters` rounds of `BURST` deadlines `delay` ticks out, counting the
/// walk that expires them and not the arming that filled the wheel.
fn walk(timers: &Timers, iters: u64, delay: u64) -> Duration {
    let mut context = Context::from_waker(Waker::noop());
    let mut sleeps: Vec<Sleep> = Vec::with_capacity(BURST);
    let mut elapsed = Duration::ZERO;

    for _ in 0..iters {
        sleeps.extend((0..BURST).map(|_| timers.after(delay)));
        for sleep in &mut sleeps {
            let _ = Pin::new(sleep).poll(&mut context);
        }
        CLOCK.0.fetch_add(delay, Ordering::Relaxed);

        let start = Instant::now();
        black_box(timers.advance());
        elapsed += start.elapsed();
        sleeps.clear();
    }
    elapsed
}

/// A wheel walked up to the clock the last bench left behind.
fn wound() -> Timers {
    let timers = Timers::new(&CLOCK);
    timers.advance();
    timers
}

criterion_group!(benches, arm, tick, expire, cascade);
criterion_main!(benches);
