//! How many device reads one filesystem call costs.
//!
//! Run as a molt-fs integration test: copy next to `crates/molt-fs/tests` or
//! point `--test` at it. Stage 4.4 moved the reads onto a ring, and this is the
//! count that says whether the ring changed what gets fetched.

use std::cell::Cell;
use std::rc::Rc;

use molt_block::{BlockError, Device, Disk, Loopback};
use molt_core::buffer::BufferRegistry;
use molt_core::capability::CellId;
use molt_fs::format::{Tree, build};
use molt_core::buffer::BufferOperation;
use molt_fs::{Fs, FsDone, FsOp, Handle, Name};

const OWNER: CellId = CellId::new(1);
const FILE: usize = 256 * 1024;
const WINDOW: usize = 4096;

struct Counted<'a> {
    inner: Loopback<'a>,
    reads: Rc<Cell<usize>>,
}

impl Device for Counted<'_> {
    fn sectors(&self) -> u64 {
        self.inner.sectors()
    }

    fn read(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        self.reads.set(self.reads.get() + 1);
        self.inner.read(sector, buf)
    }
}

impl Disk for Counted<'_> {
    fn write(&mut self, sector: u64, buf: &[u8]) -> Result<(), BlockError> {
        self.inner.write(sector, buf)
    }

    fn flush(&mut self) -> Result<(), BlockError> {
        self.inner.flush()
    }
}

fn main() {
    let mut tree = Tree::new();
    tree.file("big", (0..FILE).map(|at| at as u8).collect()).unwrap();
    let bytes = build(&tree, 1).unwrap();

    let reads = Rc::new(Cell::new(0));
    let device = Counted { inner: Loopback::new(&bytes).unwrap(), reads: Rc::clone(&reads) };
    let mut fs = Fs::<_, 4>::mount(device).unwrap();
    println!("mount {}", reads.get());

    let mut window = [0u8; WINDOW];
    let mut buffers = BufferRegistry::<1>::new();
    let buffer = buffers.register_write(OWNER, &mut window).unwrap();
    let root = fs.root(OWNER).unwrap();
    reads.set(0);
    let op = FsOp::Open { dir: root, name: Name::try_from("big").unwrap() };
    let opened = fs.apply(OWNER, op, &mut buffers).unwrap();
    println!("open {}", reads.get());
    let Some(Handle::File(file)) = opened.handle() else { unreachable!() };

    reads.set(0);
    let mut offset = 0;
    while offset < FILE as u64 {
        let target = BufferOperation::new(buffer, 0, WINDOW);
        let op = FsOp::Read { file, buffer: target, offset };
        let FsDone::Read(read) = fs.apply(OWNER, op, &mut buffers).unwrap() else { unreachable!() };
        offset += read as u64;
    }
    println!("stream {} for {} blocks", reads.get(), FILE / WINDOW);
}
