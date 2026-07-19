//! Baselines for the scheduling primitives the completion hot path walks.
//!
//! `executor_contended_wake` is the one that answers whether `cache-padded` is
//! worth its memory: every thread wakes a task nobody else touches, so all the
//! cost that remains is the cache line those independent state words share.
//! Compare a `cargo bench` baseline against `cargo bench --features
//! cache-padded` on the machine you actually deploy to.

use std::future::Future;
use std::hint::black_box;
use std::pin::pin;
use std::task::{Context, Waker};
use std::thread;
use std::time::Instant;

use criterion::{Criterion, criterion_group, criterion_main};
use molt_core::completion::CompletionSlab;
use molt_core::executor::Executor;
use molt_core::waker::AtomicWaker;

fn executor(criterion: &mut Criterion) {
    let executor = Executor::<64>::new();
    let task = executor.register().expect("free slot");

    criterion.bench_function("executor_wake_and_scan", |bencher| {
        bencher.iter(|| {
            executor.wake(black_box(task));
            let ready = executor.next_ready().expect("the task just woken");
            executor.complete_poll(ready);
        });
    });
}

fn executor_contended(criterion: &mut Criterion) {
    const THREADS: usize = 4;

    criterion.bench_function("executor_contended_wake", |bencher| {
        bencher.iter_custom(|iters| {
            let executor = Executor::<64>::new();
            // One private task per thread: nothing is shared but the cache line.
            let tasks: Vec<_> =
                (0..THREADS).map(|_| executor.register().expect("free slot")).collect();

            let start = Instant::now();
            thread::scope(|scope| {
                for task in &tasks {
                    let executor = &executor;
                    scope.spawn(move || {
                        for _ in 0..iters {
                            executor.wake(*task);
                            executor.complete_poll(*task);
                        }
                    });
                }
            });
            start.elapsed()
        });
    });
}

fn completion(criterion: &mut Criterion) {
    let slab = CompletionSlab::<u64, 64>::new();
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);

    criterion.bench_function("completion_round_trip", |bencher| {
        bencher.iter(|| {
            let token = slab.reserve().expect("free slot");
            let mut future = pin!(slab.wait(token));
            let _ = future.as_mut().poll(&mut context);
            slab.complete(token.request_id(), black_box(7)).expect("live id");
            let _ = black_box(future.as_mut().poll(&mut context));
        });
    });
}

fn waker(criterion: &mut Criterion) {
    let slot = AtomicWaker::new();
    let waker = Waker::noop();

    criterion.bench_function("atomic_waker_register_and_wake", |bencher| {
        bencher.iter(|| {
            slot.register(black_box(waker));
            slot.wake();
        });
    });
}

criterion_group!(benches, executor, executor_contended, completion, waker);
criterion_main!(benches);
