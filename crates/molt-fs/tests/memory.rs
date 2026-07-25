//! A heap that runs out, and a filesystem that answers instead of aborting.
//!
//! The kernel has one heap and no swap behind it, so the interesting question
//! is not whether allocation can fail but what the filesystem does when it
//! does. This binary replaces the allocator with one that can be closed for the
//! current thread, and asks for the two allocations a mount and a mutation
//! cannot do without: the block buffer and a tree node.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::ptr;

use molt_block::Loopback;
use molt_fs::format::{self, Tree};
use molt_fs::{FsError, Journal, Kind, Name};

/// Allocations this big are the filesystem's own — a block buffer or a node.
/// The arena maps, the cache, and whatever the harness does are all smaller,
/// so closing the heap here starves the paths under test and nothing else.
const LARGE: usize = 1024;

#[global_allocator]
static HEAP: Refusing = Refusing;

thread_local! {
    /// Whether large allocations on this thread are being refused.
    static CLOSED: Cell<bool> = const { Cell::new(false) };
}

/// The system heap, minus what a test has closed off.
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

#[test]
fn mount_refused_without_buffer() -> Result<(), FsError> {
    let bytes = image();

    let mounted = starved(|| Journal::mount(Loopback::new(&bytes)?));

    assert_eq!(mounted.err(), Some(FsError::Memory));
    assert!(Journal::mount(Loopback::new(&bytes)?).is_ok(), "heap came back and mount did not");
    Ok(())
}

#[test]
fn refused_node_rolls_back() -> Result<(), FsError> {
    let mut bytes = image();
    {
        let mut journal = Journal::mount(Loopback::writable(&mut bytes)?)?;
        journal.create(journal.root(), name("kept"), Kind::File)?;
        journal.sync()?;

        let refused = starved(|| journal.create(journal.root(), name("lost"), Kind::File));
        assert_eq!(refused.err(), Some(FsError::Memory));

        // The mutation that could not allocate is the one that is gone: the
        // transaction goes back to its snapshot, so the journal keeps taking work.
        journal.create(journal.root(), name("later"), Kind::File)?;
        journal.sync()?;
    }
    let mut journal = Journal::mount(Loopback::new(&bytes)?)?;

    assert!(journal.lookup(journal.root(), &name("kept")).is_ok());
    assert!(journal.lookup(journal.root(), &name("later")).is_ok());
    assert_eq!(journal.lookup(journal.root(), &name("lost")), Err(FsError::Missing));
    Ok(())
}
