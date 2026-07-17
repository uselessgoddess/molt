use std::hint::{black_box, spin_loop};
use std::thread;
use std::time::Instant;

use criterion::{Criterion, criterion_group, criterion_main};
use molt_core::ring::{Completion, IoRing, RequestId, SpscRing, Submission};

fn ring(criterion: &mut Criterion) {
    let mut ring = SpscRing::<u64, 256>::new();
    let (mut producer, mut consumer) = ring.split();

    criterion.bench_function("spsc_ring_round_trip", |bencher| {
        bencher.iter(|| {
            producer.try_push(black_box(42)).unwrap();
            black_box(consumer.try_pop().unwrap());
        });
    });
}

fn io_ring(criterion: &mut Criterion) {
    let mut ring = IoRing::<u64, u64, 256>::new();
    let (mut client, mut driver) = ring.split();
    let mut next_id = 0_u64;

    criterion.bench_function("io_ring_round_trip", |bencher| {
        bencher.iter(|| {
            let request = Submission::new(RequestId::new(next_id), black_box(42));
            next_id = next_id.wrapping_add(1);
            client.try_submit(request).unwrap();
            let submitted = driver.try_next().unwrap();
            driver.try_complete(Completion::new(submitted.id(), 42)).unwrap();
            black_box(client.try_completion().unwrap());
        });
    });
}

fn cross_core_ring(c: &mut Criterion) {
    c.bench_function("cross_core_ping_pong", |b| {
        b.iter_custom(|iters| {
            let mut ring = SpscRing::<u64, 256>::new();
            let (mut tx, mut rx) = ring.split();

            let start = Instant::now();
            thread::scope(|s| {
                s.spawn(move || {
                    for i in 0..iters {
                        while tx.try_push(i).is_err() {
                            spin_loop();
                        }
                    }
                });
                for i in 0..iters {
                    while rx.try_pop() != Some(i) {
                        spin_loop();
                    }
                }
            });
            start.elapsed()
        });
    });
}

criterion_group!(benches, ring, io_ring, cross_core_ring);
criterion_main!(benches);
