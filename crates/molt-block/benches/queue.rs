use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use molt_block::{BLOCK, BlockOp, Buffer, Loopback, Queue, Queued, RequestId, SECTOR};

const IMAGE_BLOCKS: usize = 64;
const BATCH: usize = 8;

fn read(block: usize) -> BlockOp {
    BlockOp::Read {
        sector: (block * BLOCK / SECTOR) as u64,
        bytes: BLOCK,
        buffer: Box::new([0; BLOCK]),
    }
}

fn cycle<const DEPTH: usize>(queue: &mut Queued<Loopback<'_>, DEPTH>, next: &mut u64) -> Buffer {
    for offset in 0..DEPTH {
        let id = RequestId::new(*next + offset as u64);
        assert!(queue.start(id, read((*next as usize + offset) % IMAGE_BLOCKS)).is_ok());
    }
    *next += DEPTH as u64;
    let mut last = None;
    for _ in 0..DEPTH {
        let (_, done) = queue.reap().expect("every submitted read completes");
        done.result.expect("the benchmark stays inside the image");
        last = done.buffer;
    }
    last.expect("a non-empty queue returns a buffer")
}

fn block_queue(criterion: &mut Criterion) {
    let image = vec![0xa5; IMAGE_BLOCKS * BLOCK];
    let mut group = criterion.benchmark_group("block_queue");
    group.throughput(Throughput::Bytes((BATCH * BLOCK) as u64));

    group.bench_with_input(BenchmarkId::new("depth", 1), &(), |bencher, ()| {
        let mut queue = Queued::<_, 1>::new(Loopback::read(&image).unwrap());
        let mut next = 0;
        bencher.iter(|| {
            for _ in 0..BATCH {
                criterion::black_box(cycle(&mut queue, &mut next));
            }
        });
    });
    group.bench_with_input(BenchmarkId::new("depth", 8), &(), |bencher, ()| {
        let mut queue = Queued::<_, 8>::new(Loopback::read(&image).unwrap());
        let mut next = 0;
        bencher.iter(|| criterion::black_box(cycle(&mut queue, &mut next)));
    });
    group.finish();
}

criterion_group!(benches, block_queue);
criterion_main!(benches);
