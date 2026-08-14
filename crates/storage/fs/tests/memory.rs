use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::ptr;

use molt_block::{Backing, Disk, Loopback, Serial};
use molt_fs::format::{self, Tree};
use molt_fs::{DEPTH, FsError, Journal, Kind, Name, attach};

const LARGE: usize = 1024;

#[global_allocator]
static HEAP: Refusing = Refusing;

thread_local! {
    /// Whether large allocations on this thread are being refused.
    static CLOSED: Cell<bool> = const { Cell::new(false) };
}

struct Refusing;

// SAFETY: every allocation is the system allocator's, freed through it, and the
// refusal path hands back null rather than a pointer of its own.
unsafe impl GlobalAlloc for Refusing {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        match layout.size() >= LARGE && CLOSED.get() {
            true => ptr::null_mut(),
            false => unsafe { System.alloc(layout) },
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

/// Runs `operation` on a heap with nothing large left in it.
fn starved<T>(operation: impl FnOnce() -> T) -> T {
    CLOSED.set(true);
    let done = operation();
    CLOSED.set(false);
    done
}

fn image() -> Vec<u8> {
    format::build(&Tree::new(), 1).unwrap()
}

fn name(text: &str) -> Name {
    Name::try_from(text).unwrap()
}

fn mount<D: Disk>(device: D) -> Result<(Journal, Backing<Serial<D>, DEPTH>), FsError> {
    let (blocks, mut backing) = attach(Serial::new(device))?;
    let journal = backing.run(Journal::mount(blocks))?;
    Ok((journal, backing))
}

#[test]
fn mount_refused_without_buffer() -> Result<(), FsError> {
    let bytes = image();

    let mounted = starved(|| mount(Loopback::read(&bytes)?));

    assert_eq!(mounted.err(), Some(FsError::Memory));
    assert!(mount(Loopback::read(&bytes)?).is_ok(), "heap came back and mount did not");
    Ok(())
}

#[test]
fn refused_node_rolls_back() -> Result<(), FsError> {
    let mut bytes = image();
    {
        let (mut journal, mut backing) = mount(Loopback::write(&mut bytes)?)?;
        let root = journal.root();
        backing.run(async {
            journal.create(root, name("kept"), Kind::File).await?;
            journal.sync().await
        })?;

        let refused = starved(|| backing.run(journal.create(root, name("lost"), Kind::File)));
        assert_eq!(refused.err(), Some(FsError::Memory));

        // The mutation that could not allocate is the one that is gone: the
        // transaction goes back to its snapshot, so the journal keeps taking work.
        backing.run(async {
            journal.create(root, name("later"), Kind::File).await?;
            journal.sync().await
        })?;
    }
    let (mut journal, mut backing) = mount(Loopback::read(&bytes)?)?;
    let root = journal.root();

    backing.run(async {
        assert!(journal.lookup(root, &name("kept")).await.is_ok());
        assert!(journal.lookup(root, &name("later")).await.is_ok());
        assert_eq!(journal.lookup(root, &name("lost")).await, Err(FsError::Missing));
    });
    Ok(())
}
