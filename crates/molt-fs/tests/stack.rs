#![cfg(not(miri))]

use std::hint::black_box;

use molt_block::{Backing, Disk, Loopback};
use molt_fs::format::{self, Tree};
use molt_fs::{DEPTH, FsError, Journal, Kind, Name, attach};

/// Painted window, and what one call may spend of it.
const WINDOW: usize = 96 * 1024;
const BUDGET: usize = 16 * 1024;
const MARK: u8 = 0xa5;

fn image() -> Vec<u8> {
    format::build(&Tree::new(), 1).unwrap()
}

fn name(index: usize) -> Name {
    Name::try_from(format!("file-{index:02}").as_str()).unwrap()
}

fn mount<D: Disk>(device: D) -> Result<(Journal, Backing<D, DEPTH>), FsError> {
    let (blocks, mut backing) = attach(device)?;
    let journal = backing.run(Journal::mount(blocks))?;
    Ok((journal, backing))
}

/// Marks the frames a later call at this depth will occupy, top-down.
#[inline(never)]
fn paint() -> *const u8 {
    let mut canvas = [MARK; WINDOW];
    black_box(canvas.as_mut_ptr());
    canvas.as_ptr()
}

/// Bytes disturbed below the top of the painted window.
#[inline(never)]
fn depth(base: *const u8) -> usize {
    // SAFETY: the window is this thread's own stack, painted at this depth and
    // read one byte at a time; volatile keeps the reads out of the optimizer's
    // model of a frame it considers dead.
    (0..WINDOW)
        .find(|at| unsafe { base.add(*at).read_volatile() } != MARK)
        .map_or(0, |at| WINDOW - at)
}

#[test]
fn mount_fits_kernel_stack() -> Result<(), FsError> {
    let bytes = image();

    let base = paint();
    let mounted = mount(Loopback::new(&bytes)?).is_ok();
    let spent = depth(base);

    assert!(mounted, "image did not mount");
    assert!(spent < BUDGET, "mount spent {spent} bytes of stack");
    Ok(())
}

#[test]
fn commit_fits_kernel_stack() -> Result<(), FsError> {
    let mut bytes = image();
    let (mut journal, mut backing) = mount(Loopback::writable(&mut bytes)?)?;
    let root = journal.root();
    // A tree deep enough that one more key rewrites a root-to-leaf path.
    backing.run(async {
        for index in 0..40 {
            journal.create(root, name(index), Kind::File).await?;
        }
        Ok::<_, FsError>(())
    })?;

    let base = paint();
    let created = backing.run(journal.create(root, name(40), Kind::File)).is_ok();
    let spent = depth(base);

    assert!(created, "create failed");
    assert!(spent < BUDGET, "create spent {spent} bytes of stack");
    Ok(())
}
