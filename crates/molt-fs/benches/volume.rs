//! What the filesystem costs above the device.
//!
//! Every run is over [`Loopback`], so a block fetch is a memcpy and what is
//! left is the path around it: the extent search, the block cache, the tree
//! lookup, and the checksum. That is the part Stage 4.4 moves onto a ring, so
//! it is the part worth a number.

use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use molt_block::Loopback;
use molt_core::buffer::{BufferOperation, BufferRegistry};
use molt_core::capability::CellId;
use molt_fs::format::{Tree, build};
use molt_fs::{Fs, FsDone, FsOp, Handle, Kind, Name};

const OWNER: CellId = CellId::new(1);
/// Long enough to cross extents rather than sit in one.
const FILE: usize = 256 * 1024;
/// Deliberately smaller than the file, so a read has to loop.
const WINDOW: usize = 4096;

fn image() -> Vec<u8> {
    let mut tree = Tree::new();
    tree.file("big", (0..FILE).map(|at| at as u8).collect()).unwrap();
    tree.dir("docs").unwrap().file("readme", b"read me".to_vec()).unwrap();
    build(&tree, 1).unwrap()
}

fn name(text: &str) -> Name {
    Name::try_from(text).unwrap()
}

fn mount(criterion: &mut Criterion) {
    let bytes = image();

    criterion.bench_function("fs_mount", |bencher| {
        bencher.iter(|| Fs::<_, 4>::mount(Loopback::new(&bytes).unwrap()).unwrap());
    });
}

fn open(criterion: &mut Criterion) {
    let bytes = image();
    let mut fs = Fs::<_, 4>::mount(Loopback::new(&bytes).unwrap()).unwrap();
    let mut buffers = BufferRegistry::<1>::new();
    let root = fs.root(OWNER).unwrap();

    criterion.bench_function("fs_open", |bencher| {
        bencher.iter(|| {
            let op = FsOp::Open { dir: root, name: name("big") };
            let opened = fs.apply(OWNER, op, &mut buffers).unwrap();
            let Some(handle) = opened.handle() else { unreachable!() };
            fs.apply(OWNER, FsOp::Close(handle), &mut buffers).unwrap();
        });
    });
}

fn read(criterion: &mut Criterion) {
    let bytes = image();
    let mut window = [0u8; WINDOW];
    let mut fs = Fs::<_, 4>::mount(Loopback::new(&bytes).unwrap()).unwrap();
    let mut buffers = BufferRegistry::<1>::new();
    let buffer = buffers.register_write(OWNER, &mut window).unwrap();
    let root = fs.root(OWNER).unwrap();
    let op = FsOp::Open { dir: root, name: name("big") };
    let Some(Handle::File(file)) = fs.apply(OWNER, op, &mut buffers).unwrap().handle() else {
        unreachable!()
    };

    criterion.bench_function("fs_read_stream", |bencher| {
        bencher.iter(|| {
            let mut offset = 0;
            while offset < FILE as u64 {
                let target = BufferOperation::new(buffer, 0, WINDOW);
                let op = FsOp::Read { file, buffer: target, offset };
                let FsDone::Read(read) = fs.apply(OWNER, op, &mut buffers).unwrap() else {
                    unreachable!()
                };
                offset += read as u64;
            }
        });
    });
}

/// Transactions a fresh image holds before its log is full. Nothing reclaims a
/// write record, so a commit loop is measured in batches and remounted between
/// them, with the remount outside the clock.
const BATCH: u64 = 32;

fn commit(criterion: &mut Criterion) {
    let image = image();
    let mut source = [0xa5; WINDOW];
    let mut buffers = BufferRegistry::<1>::new();
    let buffer = buffers.register_read(OWNER, &mut source).unwrap();

    criterion.bench_function("fs_commit", |bencher| {
        bencher.iter_custom(|iters| {
            let mut elapsed = Duration::ZERO;
            let mut done = 0;
            while done < iters {
                let mut bytes = image.clone();
                let mut fs = Fs::<_, 4>::mount(Loopback::writable(&mut bytes).unwrap()).unwrap();
                let root = fs.root(OWNER).unwrap();
                let create = FsOp::Create { dir: root, name: name("written"), kind: Kind::File };
                let opened = fs.apply(OWNER, create, &mut buffers).unwrap();
                let Some(Handle::File(file)) = opened.handle() else { unreachable!() };

                let batch = (iters - done).min(BATCH);
                let start = Instant::now();
                for _ in 0..batch {
                    let source = BufferOperation::new(buffer, 0, WINDOW);
                    let write = FsOp::Write { file, buffer: source, offset: 0 };
                    fs.apply(OWNER, write, &mut buffers).unwrap();
                    fs.apply(OWNER, FsOp::Sync, &mut buffers).unwrap();
                }
                elapsed += start.elapsed();
                done += batch;
            }
            elapsed
        });
    });
}

criterion_group!(benches, mount, open, read, commit);
criterion_main!(benches);
