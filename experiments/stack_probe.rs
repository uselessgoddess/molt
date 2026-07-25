//! Where a filesystem call's stack goes, phase by phase.
//!
//! Same painting trick as `crates/molt-fs/tests/stack.rs`, but reporting each
//! step instead of asserting one budget. Run it as a molt-fs test:
//!
//! ```text
//! cp experiments/stack_probe.rs crates/molt-fs/tests/probe.rs
//! cargo test -p molt-fs --all-features --test probe -- --nocapture
//! ```

#![cfg(not(miri))]

use std::hint::black_box;

use molt_block::Loopback;
use molt_fs::format::{self, Tree};
use molt_fs::{BLOCK, FsError, Journal, Kind, Name, Volume};

const WINDOW: usize = 96 * 1024;
const MARK: u8 = 0xa5;

fn image() -> Vec<u8> {
    format::build(&Tree::new(), 1).unwrap()
}

fn name(index: usize) -> Name {
    Name::try_from(format!("file-{index:02}").as_str()).unwrap()
}

#[inline(never)]
fn paint() -> *const u8 {
    let mut canvas = [MARK; WINDOW];
    black_box(canvas.as_mut_ptr());
    canvas.as_ptr()
}

#[inline(never)]
fn depth(base: *const u8) -> usize {
    // SAFETY: as in tests/stack.rs, this thread's own painted frames.
    (0..WINDOW)
        .find(|at| unsafe { base.add(*at).read_volatile() } != MARK)
        .map_or(0, |at| WINDOW - at)
}

fn report(what: &str, spent: usize) {
    println!("{what:>24}: {spent:>6} bytes");
}

#[test]
fn probe() -> Result<(), FsError> {
    let mut bytes = image();

    let base = paint();
    let volume = Volume::mount(Loopback::new(&bytes)?, &mut [0; BLOCK]).is_ok();
    report("Volume::mount", depth(base));
    assert!(volume);

    let base = paint();
    let mounted = Journal::mount(Loopback::new(&bytes)?, &mut [0; BLOCK]).is_ok();
    report("Journal::mount", depth(base));
    assert!(mounted);

    let mut block = [0; BLOCK];
    let mut journal = Journal::mount(Loopback::writable(&mut bytes)?, &mut block)?;

    let base = paint();
    journal.create(journal.root(), name(0), Kind::File)?;
    report("first create", depth(base));

    for index in 1..40 {
        journal.create(journal.root(), name(index), Kind::File)?;
    }

    let base = paint();
    journal.create(journal.root(), name(40), Kind::File)?;
    report("create at depth", depth(base));

    let base = paint();
    journal.sync()?;
    report("sync", depth(base));
    Ok(())
}
