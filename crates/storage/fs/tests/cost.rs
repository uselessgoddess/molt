use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use molt_block::{BLOCK, Loopback, Serial};
use molt_core::CellId;
use molt_core::buffer::{BufferOperation, BufferRegistry};
use molt_fs::format::{Tree, build, build_with_capacity};
use molt_fs::{Fs, FsDone, FsError, FsOp, Handle, Journal, Kind, Name, attach};

#[global_allocator]
static ALLOC: Counter = Counter;

const OWNER: CellId = CellId::new(1);
/// Long enough that a read crosses extents rather than sitting in one.
const FILE: usize = 256 * 1024;
const WINDOW: usize = 4096;
/// Bytes one write appends, small enough that the log outlives the rounds.
const CHUNK: usize = 512;
/// Rounds measured after the pools and the node cache have filled.
const ROUNDS: usize = 64;
/// Small allocations one write may still make: two arena bitmaps and the walk
/// that fills them, per transaction it opens.
const BOOKKEEPING: usize = 16;

#[test]
fn read_costs_no_heap() -> Result<(), FsError> {
    let mut tree = Tree::new();
    tree.file("big", (0..FILE).map(|at| at as u8).collect())?;
    let bytes = build(&tree, 1)?;

    let mut fs = Fs::<_, 4>::mount(Serial::new(Loopback::read(&bytes)?))?;
    let mut window = [0; WINDOW];
    let mut buffers = BufferRegistry::<1>::new();
    let buffer = buffers.register_write(OWNER, &mut window).unwrap();
    let root = fs.root(OWNER)?;
    let open = FsOp::Open { dir: root, name: Name::try_from("big")? };
    let Some(Handle::File(file)) = fs.apply(OWNER, open, &mut buffers)?.handle() else {
        panic!("a file opened as something else")
    };

    let mut read = |at: usize| {
        let target = BufferOperation::new(buffer, 0, WINDOW);
        let op = FsOp::Read { file, buffer: target, offset: (at * WINDOW) as u64 };
        let FsDone::Read(read) = fs.apply(OWNER, op, &mut buffers)? else {
            panic!("a read answered something else")
        };
        Ok::<_, FsError>(read)
    };

    let blocks = FILE / WINDOW;
    for at in 0..blocks {
        read(at * 37 % blocks)?;
    }

    let (spent, _) = cost(|| {
        for at in 0..ROUNDS {
            read(at * 37 % blocks).unwrap();
        }
    });
    assert_eq!(spent, 0, "{spent} allocations over {ROUNDS} reads");
    Ok(())
}

#[test]
fn write_costs_no_nodes() -> Result<(), FsError> {
    let mut bytes = build_with_capacity(&Tree::new(), 1, 256, 256)?;
    let (blocks, mut backing) = attach(Serial::new(Loopback::write(&mut bytes)?))?;
    let mut journal = backing.run(Journal::mount(blocks))?;
    let root = journal.root();

    let file = backing.run(journal.create(root, Name::try_from("log")?, Kind::File))?;
    let mut round = |at: usize| {
        backing.run(async {
            journal.write(file, (at * CHUNK) as u64, &[at as u8; CHUNK]).await?;
            journal.sync().await
        })
    };
    for at in 0..ROUNDS {
        round(at)?;
    }

    let (spent, blocks) = cost(|| {
        for at in 0..ROUNDS {
            round(at).unwrap();
        }
    });
    // A write copies a root-to-leaf path and hands the blocks it copied back to
    // the arena, so every node it builds stands in one it just retired.
    assert_eq!(blocks, 0, "{blocks} block-sized allocations over {ROUNDS} writes");
    assert!(spent < ROUNDS * BOOKKEEPING, "{spent} allocations over {ROUNDS} writes");
    Ok(())
}

fn cost(body: impl FnOnce()) -> (usize, usize) {
    let before = (COUNT.with(Cell::get), BLOCKS.with(Cell::get));
    body();
    (COUNT.with(Cell::get) - before.0, BLOCKS.with(Cell::get) - before.1)
}

thread_local! {
    static COUNT: Cell<usize> = const { Cell::new(0) };
    static BLOCKS: Cell<usize> = const { Cell::new(0) };
}

struct Counter;

impl Counter {
    fn tally(layout: Layout) {
        COUNT.try_with(|count| count.set(count.get() + 1)).ok();
        if layout.size() >= BLOCK / 2 {
            BLOCKS.try_with(|blocks| blocks.set(blocks.get() + 1)).ok();
        }
    }
}

// SAFETY: every request is handed to the system allocator unchanged.
unsafe impl GlobalAlloc for Counter {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        Self::tally(layout);
        // SAFETY: the caller's layout, passed straight on.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: as above, and the pointer is one this allocator returned.
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: as above.
        Self::tally(unsafe { Layout::from_size_align_unchecked(size, layout.align()) });
        // SAFETY: as above.
        unsafe { System.realloc(pointer, layout, size) }
    }
}
