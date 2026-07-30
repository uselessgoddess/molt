use std::cell::Cell;
use std::future::poll_fn;
use std::hint::{black_box, spin_loop};
use std::rc::Rc;
use std::task::{Poll, Waker};
use std::thread;
use std::time::Instant;

use criterion::{Criterion, criterion_group, criterion_main};
use molt_exec::{Executor, Solo};

static SOLO: Solo = Solo;

/// Tasks a burst wakes at once, and the core's size in every bench here.
const BURST: usize = 64;

fn spawn(criterion: &mut Criterion) {
    let executor = Executor::new(&SOLO, BURST);
    let mut group = criterion.benchmark_group("spawn");

    group.bench_function("local", |bencher| {
        bencher.iter(|| {
            let seen = black_box(0_u64);
            executor
                .spawn(async move {
                    black_box(seen);
                })
                .unwrap();
            executor.run_until_idle()
        });
    });
    group.bench_function("remote", |bencher| {
        bencher.iter_custom(|iters| {
            let executor = Executor::new(&SOLO, BURST);
            let handle = executor.handle();

            let start = Instant::now();
            thread::scope(|scope| {
                scope.spawn(move || {
                    for _ in 0..iters {
                        while handle.spawn(async {}).is_err() {
                            spin_loop();
                        }
                    }
                });
                let mut polls = 0;
                while polls < iters {
                    polls += executor.run_until_idle() as u64;
                }
            });
            start.elapsed()
        });
    });
    group.finish();
}

fn wake(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("wake");

    let executor = Executor::new(&SOLO, BURST);
    let slot = Rc::new(Cell::new(None));
    executor.spawn(parked(slot.clone())).unwrap();
    executor.run_until_idle();
    group.bench_function("one", |bencher| {
        bencher.iter(|| {
            slot.take().unwrap().wake();
            executor.run_until_idle()
        });
    });

    let executor = Executor::new(&SOLO, BURST);
    let slots: Vec<_> = (0..BURST).map(|_| Rc::new(Cell::new(None))).collect();
    for slot in &slots {
        executor.spawn(parked(slot.clone())).unwrap();
    }
    executor.run_until_idle();
    group.bench_function("burst", |bencher| {
        bencher.iter(|| {
            for slot in &slots {
                slot.take().unwrap().wake();
            }
            executor.run_until_idle()
        });
    });
    group.finish();
}

/// Parks forever, leaving its waker where the bench can ring it.
fn parked(slot: Rc<Cell<Option<Waker>>>) -> impl Future<Output = ()> + 'static {
    poll_fn(move |context| {
        slot.set(Some(context.waker().clone()));
        Poll::<()>::Pending
    })
}

criterion_group!(benches, spawn, wake);
criterion_main!(benches);
