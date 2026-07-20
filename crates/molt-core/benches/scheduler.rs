use std::hint::black_box;
use std::pin::pin;
use std::task::{Context, Waker};
use std::thread;
use std::time::Instant;

use criterion::{Criterion, criterion_group, criterion_main};
use molt_core::cache::Padded;
use molt_core::completion::CompletionSlab;
use molt_core::executor::Executor;
use molt_core::waker::AtomicWaker;

fn executor(criterion: &mut Criterion) {
    let compact = Executor::<64>::new();
    let compact_task = compact.register().expect("free slot");
    let padded = Executor::<64, Padded>::new();
    let padded_task = padded.register().expect("free slot");
    let mut group = criterion.benchmark_group("executor_wake_and_scan");

    group.bench_function("compact", |bencher| {
        bencher.iter(|| {
            compact.wake(black_box(compact_task));
            let ready = compact.next_ready().expect("the task just woken");
            compact.complete_poll(ready);
        });
    });
    group.bench_function("padded", |bencher| {
        bencher.iter(|| {
            padded.wake(black_box(padded_task));
            let ready = padded.next_ready().expect("the task just woken");
            padded.complete_poll(ready);
        });
    });
    group.finish();
}

fn executor_contended(criterion: &mut Criterion) {
    const THREADS: usize = 4;

    let mut group = criterion.benchmark_group("executor_contended_wake");
    group.bench_function("compact", |bencher| {
        bencher.iter_custom(|iters| {
            let executor = Executor::<64>::new();
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
    group.bench_function("padded", |bencher| {
        bencher.iter_custom(|iters| {
            let executor = Executor::<64, Padded>::new();
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
    group.finish();
}

fn completion(criterion: &mut Criterion) {
    let compact = CompletionSlab::<u64, 64>::new();
    let padded = CompletionSlab::<u64, 64, Padded>::new();
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    let mut group = criterion.benchmark_group("completion_round_trip");

    group.bench_function("compact", |bencher| {
        bencher.iter(|| {
            let token = compact.reserve().expect("free slot");
            let mut future = pin!(compact.wait(token));
            let _ = future.as_mut().poll(&mut context);
            compact.complete(token.request_id(), black_box(7)).expect("live id");
            let _ = black_box(future.as_mut().poll(&mut context));
        });
    });
    group.bench_function("padded", |bencher| {
        bencher.iter(|| {
            let token = padded.reserve().expect("free slot");
            let mut future = pin!(padded.wait(token));
            let _ = future.as_mut().poll(&mut context);
            padded.complete(token.request_id(), black_box(7)).expect("live id");
            let _ = black_box(future.as_mut().poll(&mut context));
        });
    });
    group.finish();
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
