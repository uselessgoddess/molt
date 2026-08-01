use std::cell::Cell;
use std::rc::Rc;

use molt_block::{BlockError, Device, Disk, Loopback, Serial};
use molt_fs::format::{Tree, build_with_capacity};
use molt_fs::{FsError, Journal, Kind, Name, attach};

/// Writes the file takes, each landing past the one before it.
const WRITES: u64 = 512;
const CHUNK: usize = 64;
/// Writes per transaction, so the arena is recycled rather than outgrown.
const BATCH: u64 = 64;

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

#[test]
fn read_skips_the_writes_before_it() -> Result<(), FsError> {
    let mut bytes = build_with_capacity(&Tree::new(), 1, 128, 1024)?;
    let reads = Rc::new(Cell::new(0));
    let disk = Counted { disk: Loopback::write(&mut bytes)?, reads: Rc::clone(&reads) };
    let (blocks, mut backing) = attach(Serial::new(disk))?;
    let mut journal = backing.run(Journal::mount(blocks))?;
    let root = journal.root();
    let mut window = [0; CHUNK];

    let (near, far) = backing.run(async {
        let file = journal.create(root, Name::try_from("long")?, Kind::File).await?;
        for at in 0..WRITES {
            journal.write(file, at * CHUNK as u64, &[at as u8; CHUNK]).await?;
            if at % BATCH == BATCH - 1 {
                journal.sync().await?;
            }
        }

        let before = reads.get();
        journal.read(file, 0, &mut window).await?;
        let near = reads.get() - before;
        let before = reads.get();
        journal.read(file, (WRITES - 1) * CHUNK as u64, &mut window).await?;
        Ok::<_, FsError>((near, reads.get() - before))
    })?;

    assert_eq!(window, [(WRITES - 1) as u8; CHUNK]);
    // Both cost the block the payload is in, wherever in the file they land: 1
    // and 1 fetches for 512 writes when this was written, against 96 and 98
    // with a key per write and a walk over all of them.
    assert!(near + far < WRITES as usize / 16, "{near} and {far} fetches after {WRITES} writes");
    Ok(())
}

#[test]
fn writes_survive_the_extents_they_cut() -> Result<(), FsError> {
    let mut bytes = build_with_capacity(&Tree::new(), 1, 128, 1024)?;
    let (blocks, mut backing) = attach(Serial::new(Loopback::write(&mut bytes)?))?;
    let mut journal = backing.run(Journal::mount(blocks))?;
    let root = journal.root();

    // Every write is checked against the same picture the model keeps, so a
    // trim that loses or duplicates a byte shows up on the read after it.
    let mut model = [0u8; (WRITES * CHUNK as u64) as usize];
    let mut window = [0; 3 * CHUNK];
    backing.run(async {
        let file = journal.create(root, Name::try_from("cut")?, Kind::File).await?;
        journal.write(file, 0, &model).await?;
        for at in 0..WRITES {
            // A stride coprime with the length walks the file unaligned, so
            // every write lands across the ones already there.
            let offset = at * 37 % (model.len() - CHUNK) as u64;
            let chunk = [at as u8 ^ 0xa5; CHUNK];
            journal.write(file, offset, &chunk).await?;
            model[offset as usize..offset as usize + CHUNK].copy_from_slice(&chunk);
            if at % BATCH == BATCH - 1 {
                journal.sync().await?;
            }

            let seen = offset.saturating_sub(CHUNK as u64);
            journal.read(file, seen, &mut window).await?;
            assert_eq!(window, model[seen as usize..seen as usize + window.len()]);
        }
        journal.sync().await
    })?;

    // Remount checks the index it left: extents in order, none overlapping,
    // each still pointing at bytes a log record holds.
    let (blocks, mut backing) = attach(Serial::new(Loopback::read(&bytes)?))?;
    let mut journal = backing.run(Journal::mount(blocks))?;
    let root = journal.root();
    let mut contents = vec![0; model.len()];

    backing.run(async {
        let file = journal.lookup(root, &Name::try_from("cut")?).await?;
        journal.read(file, 0, &mut contents).await
    })?;
    assert!(contents == model, "the file came back changed");
    Ok(())
}
