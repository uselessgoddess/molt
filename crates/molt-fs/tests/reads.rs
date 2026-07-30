//! What a read costs the device underneath it.
//!
//! Over a loopback there is no device time for the ring to hide, so what is
//! left to count is the fetches: slots that keep the sums block a file streams
//! past, and a readahead that lands before the window asks for it.

use std::cell::Cell;
use std::rc::Rc;

use molt_block::{BlockError, Device, Disk, Loopback};
use molt_core::buffer::{BufferOperation, BufferRegistry};
use molt_core::capability::CellId;
use molt_fs::format::{Tree, build};
use molt_fs::{Fs, FsDone, FsError, FsOp, Handle, Name};

const OWNER: CellId = CellId::new(1);
/// Long enough to cross extents rather than sit in one.
const FILE: usize = 256 * 1024;
const WINDOW: usize = 4096;
const BLOCKS: usize = FILE / WINDOW;

/// A disk that says how often it was asked.
struct Counted<'a> {
    disk: Loopback<'a>,
    reads: Rc<Cell<usize>>,
}

impl Device for Counted<'_> {
    fn sectors(&self) -> u64 {
        self.disk.sectors()
    }

    fn read(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        self.reads.set(self.reads.get() + 1);
        self.disk.read(sector, buf)
    }
}

impl Disk for Counted<'_> {
    fn write(&mut self, sector: u64, buf: &[u8]) -> Result<(), BlockError> {
        self.disk.write(sector, buf)
    }

    fn flush(&mut self) -> Result<(), BlockError> {
        self.disk.flush()
    }
}

fn image() -> Result<Vec<u8>, FsError> {
    let mut tree = Tree::new();
    tree.file("big", (0..FILE).map(|at| at as u8).collect())?;
    build(&tree, 1)
}

/// Reads the file through a window, counting what each turn fetched.
fn stream(bytes: &[u8]) -> Result<Vec<usize>, FsError> {
    let reads = Rc::new(Cell::new(0));
    let disk = Counted { disk: Loopback::new(bytes)?, reads: Rc::clone(&reads) };
    let mut fs = Fs::<_, 4>::mount(disk)?;
    let mut window = [0; WINDOW];
    let mut buffers = BufferRegistry::<1>::new();
    let buffer = buffers.register_write(OWNER, &mut window).unwrap();
    let root = fs.root(OWNER)?;
    let open = FsOp::Open { dir: root, name: Name::try_from("big")? };
    let Some(Handle::File(file)) = fs.apply(OWNER, open, &mut buffers)?.handle() else {
        panic!("a file opened as something else")
    };

    let mut counts = Vec::new();
    let (mut offset, mut before) = (0, reads.get());
    while offset < FILE as u64 {
        let target = BufferOperation::new(buffer, 0, WINDOW);
        let op = FsOp::Read { file, buffer: target, offset };
        let FsDone::Read(read) = fs.apply(OWNER, op, &mut buffers)? else {
            panic!("a read answered something else")
        };
        offset += read as u64;
        counts.push(reads.get() - before);
        before = reads.get();
    }
    Ok(counts)
}

#[test]
fn stream_fetches_a_block_once() -> Result<(), FsError> {
    let bytes = image()?;

    let fetched: usize = stream(&bytes)?.iter().sum();

    // A block and its sum, every window, is what a single-block window costs.
    // Slots hold the sums block across the file it covers, so the second read
    // is not paid again.
    assert!(fetched < 2 * BLOCKS, "{fetched} fetches for {BLOCKS} blocks");
    Ok(())
}

#[test]
fn readahead_lands_before_asked() -> Result<(), FsError> {
    let bytes = image()?;

    let counts = stream(&bytes)?;

    assert!(counts[0] > 3, "the first window fetched nothing ahead of itself");
    let free = counts.iter().filter(|&&count| count == 0).count();
    assert!(free > BLOCKS / 4, "only {free} of {BLOCKS} windows were already here");
    Ok(())
}
