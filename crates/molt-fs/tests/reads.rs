//! What a read costs the device underneath it.
//!
//! Over a loopback there is no device time for the ring to hide, so what is
//! left to count is the fetches: an extent record that says what every block
//! of its run hashes to, and a readahead that lands before the window asks for
//! it. [`Slow`] puts the device time back, as turns rather than as a clock,
//! and counts what queue depth does with it.

use std::cell::Cell;
use std::rc::Rc;

use molt_block::{BlockDone, BlockError, BlockOp, Device, Disk, Loopback, Queue, Queued, Serial};
use molt_core::buffer::{BufferOperation, BufferRegistry};
use molt_core::capability::CellId;
use molt_core::ring::RequestId;
use molt_fs::format::{Tree, build};
use molt_fs::{Fs, FsDone, FsError, FsOp, Handle, Name};

const OWNER: CellId = CellId::new(1);
/// Long enough to cross extents rather than sit in one.
const FILE: usize = 256 * 1024;
const WINDOW: usize = 4096;
const BLOCKS: usize = FILE / WINDOW;
/// Turns an answer is held for, standing in for the flight time of a device
/// nobody has attached.
const LATENCY: u64 = 16;

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

/// A queue that holds an answer for [`LATENCY`] turns before giving it back.
///
/// A turn is one look for a finished request while at least one is outstanding,
/// which is the driver waiting on the device and the only thing here worth
/// counting: it is the same number on every machine, where a clock is not.
struct Slow<Q, const DEPTH: usize> {
    queue: Q,
    turn: u64,
    turns: Rc<Cell<u64>>,
    holding: [Option<(RequestId, BlockOp, u64)>; DEPTH],
}

impl<Q: Queue, const DEPTH: usize> Slow<Q, DEPTH> {
    fn new(queue: Q, turns: Rc<Cell<u64>>) -> Self {
        Self { queue, turn: 0, turns, holding: [const { None }; DEPTH] }
    }
}

impl<Q: Queue, const DEPTH: usize> Queue for Slow<Q, DEPTH> {
    fn sectors(&self) -> u64 {
        self.queue.sectors()
    }

    fn depth(&self) -> usize {
        DEPTH
    }

    fn start(&mut self, id: RequestId, op: BlockOp) -> Result<(), BlockOp> {
        match self.holding.iter_mut().find(|slot| slot.is_none()) {
            Some(free) => {
                *free = Some((id, op, self.turn + LATENCY));
                Ok(())
            }
            None => Err(op),
        }
    }

    fn reap(&mut self) -> Option<(RequestId, BlockDone)> {
        if self.holding.iter().all(Option::is_none) {
            return None;
        }
        self.turn += 1;
        self.turns.set(self.turns.get() + 1);
        let turn = self.turn;
        let ready = |slot: &&mut Option<(RequestId, BlockOp, u64)>| {
            slot.as_ref().is_some_and(|&(.., due)| due <= turn)
        };
        let (id, op, _) = self.holding.iter_mut().find(ready)?.take()?;
        // The queue underneath is one deep and empty, so it takes this one.
        self.queue.start(id, op).ok().unwrap();
        self.queue.reap()
    }
}

fn image() -> Result<Vec<u8>, FsError> {
    let mut tree = Tree::new();
    tree.file("big", (0..FILE).map(|at| at as u8).collect())?;
    build(&tree, 1)
}

/// Reads the file through a window, saying what each one moved `counter` by.
///
/// Mount and open are outside the counts: they cost the same on every device
/// here, and what is compared is the streaming.
fn stream<Q: Queue>(queue: Q, counter: &Cell<usize>) -> Result<Vec<usize>, FsError> {
    let mut fs = Fs::<_, 4>::mount(queue)?;
    let mut window = [0; WINDOW];
    let mut buffers = BufferRegistry::<1>::new();
    let buffer = buffers.register_write(OWNER, &mut window).unwrap();
    let root = fs.root(OWNER)?;
    let open = FsOp::Open { dir: root, name: Name::try_from("big")? };
    let Some(Handle::File(file)) = fs.apply(OWNER, open, &mut buffers)?.handle() else {
        panic!("a file opened as something else")
    };

    let mut counts = Vec::new();
    let (mut offset, mut before) = (0, counter.get());
    while offset < FILE as u64 {
        let target = BufferOperation::new(buffer, 0, WINDOW);
        let op = FsOp::Read { file, buffer: target, offset };
        let FsDone::Read(read) = fs.apply(OWNER, op, &mut buffers)? else {
            panic!("a read answered something else")
        };
        offset += read as u64;
        counts.push(counter.get() - before);
        before = counter.get();
    }
    Ok(counts)
}

/// Fetches per window over a device `DEPTH` requests deep.
fn fetches<const DEPTH: usize>(bytes: &[u8]) -> Result<Vec<usize>, FsError> {
    let reads = Rc::new(Cell::new(0));
    let disk = Counted { disk: Loopback::new(bytes)?, reads: Rc::clone(&reads) };
    stream(Queued::<_, DEPTH>::new(disk), &reads)
}

/// Turns the driver spent waiting on a device `DEPTH` requests deep.
fn waited<const DEPTH: usize>(bytes: &[u8]) -> Result<u64, FsError> {
    let turns = Rc::new(Cell::new(0));
    let slow = Slow::<_, DEPTH>::new(Serial::new(Loopback::new(bytes)?), Rc::clone(&turns));
    let mounted = turns.get();
    stream(slow, &Cell::new(0))?;
    Ok(turns.get() - mounted)
}

#[test]
fn stream_fetches_block_once() -> Result<(), FsError> {
    let bytes = image()?;

    let fetched: usize = fetches::<1>(&bytes)?.iter().sum();

    // The block, and the extent record carrying the sums for its whole run.
    // 81 fetches for 64 blocks when this was written, against 97 with the
    // sums in a region of their own.
    assert!(fetched < BLOCKS + BLOCKS / 2, "{fetched} fetches for {BLOCKS} blocks");
    Ok(())
}

#[test]
fn readahead_lands_before_asked() -> Result<(), FsError> {
    let bytes = image()?;

    let counts = fetches::<1>(&bytes)?;

    assert!(counts[0] > 3, "the first window fetched nothing ahead of itself");
    let free = counts.iter().filter(|&&count| count == 0).count();
    assert!(free > BLOCKS / 4, "only {free} of {BLOCKS} windows were already here");
    Ok(())
}

#[test]
fn deep_device_fetches_no_more() -> Result<(), FsError> {
    let bytes = image()?;

    let serial: usize = fetches::<1>(&bytes)?.iter().sum();
    let deep: usize = fetches::<8>(&bytes)?.iter().sum();

    assert_eq!(deep, serial, "out of order answers cost {deep} fetches against {serial}");
    Ok(())
}

#[test]
fn depth_hides_latency() -> Result<(), FsError> {
    let bytes = image()?;

    let serial = waited::<1>(&bytes)?;
    let deep = waited::<8>(&bytes)?;

    // 769 turns against 1504 when this was written.
    assert!(3 * deep < 2 * serial, "eight deep waited {deep} turns against {serial}");
    Ok(())
}
