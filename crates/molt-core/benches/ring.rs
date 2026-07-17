use criterion::{Criterion, criterion_group, criterion_main};
use molt_core::ring::{Completion, IoRing, RequestId, SpscRing, Submission};
use std::hint::black_box;

fn ring_round_trip(criterion: &mut Criterion) {
    let mut ring = SpscRing::<u64, 256>::new();
    let (mut producer, mut consumer) = ring.split();

    criterion.bench_function("spsc_ring_round_trip", |bencher| {
        bencher.iter(|| {
            producer.try_push(black_box(42)).unwrap();
            black_box(consumer.try_pop().unwrap());
        });
    });
}

fn io_ring_round_trip(criterion: &mut Criterion) {
    let mut ring = IoRing::<u64, u64, 256>::new();
    let (mut client, mut driver) = ring.split();
    let mut next_id = 0_u64;

    criterion.bench_function("io_ring_round_trip", |bencher| {
        bencher.iter(|| {
            let request = Submission::new(RequestId::new(next_id), black_box(42));
            next_id = next_id.wrapping_add(1);
            client.try_submit(request).unwrap();
            let submitted = driver.try_next().unwrap();
            driver
                .try_complete(Completion::new(submitted.id(), 42))
                .unwrap();
            black_box(client.try_completion().unwrap());
        });
    });
}

criterion_group!(benches, ring_round_trip, io_ring_round_trip);
criterion_main!(benches);
