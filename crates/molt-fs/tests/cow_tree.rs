use molt_block::Loopback;
use molt_fs::format::{self, Tree};
use molt_fs::{BLOCK, FsError, Journal, Kind, Name};

fn image() -> Vec<u8> {
    format::build(&Tree::new(), 1).unwrap()
}

fn name(index: usize) -> Name {
    Name::try_from(format!("file-{index:02}").as_str()).unwrap()
}

#[test]
fn tree_splits_and_remounts() -> Result<(), FsError> {
    let mut bytes = image();
    {
        let mut block = [0; BLOCK];
        let mut journal = Journal::mount(Loopback::writable(&mut bytes)?, &mut block)?;
        for index in 0..40 {
            journal.create(journal.root(), name(index), Kind::File)?;
        }
        journal.sync()?;

        let stats = journal.tree_stats()?;
        assert!(stats.height >= 2, "forty keys stayed in one leaf: {stats:?}");
        assert!(stats.nodes >= 4, "split did not create a real tree: {stats:?}");
    }

    let mut block = [0; BLOCK];
    let mut journal = Journal::mount(Loopback::new(&bytes)?, &mut block)?;
    for index in 0..40 {
        assert!(journal.lookup(journal.root(), &name(index)).is_ok(), "missing key {index}");
        assert_eq!(journal.entry(journal.root(), index as u32)?.0, name(index));
    }

    Ok(())
}

#[test]
fn root_swing_hides_unsynced_tree() -> Result<(), FsError> {
    let mut bytes = image();
    let stable_root;
    {
        let mut block = [0; BLOCK];
        let mut journal = Journal::mount(Loopback::writable(&mut bytes)?, &mut block)?;
        journal.create(journal.root(), name(1), Kind::File)?;
        journal.sync()?;
        stable_root = journal.tree_stats()?.root;

        journal.create(journal.root(), name(2), Kind::File)?;
        let pending_root = journal.tree_stats()?.root;
        assert_ne!(pending_root, stable_root, "mutation rewrote committed root");
    }

    let mut block = [0; BLOCK];
    let mut journal = Journal::mount(Loopback::new(&bytes)?, &mut block)?;
    assert!(journal.lookup(journal.root(), &name(1)).is_ok());
    assert_eq!(journal.lookup(journal.root(), &name(2)), Err(FsError::Missing));
    assert_eq!(journal.tree_stats()?.root, stable_root);
    Ok(())
}

#[test]
fn cache_hit_skips_device_read() -> Result<(), FsError> {
    let mut bytes = image();
    {
        let mut block = [0; BLOCK];
        let mut journal = Journal::mount(Loopback::writable(&mut bytes)?, &mut block)?;
        journal.create(journal.root(), name(0), Kind::File)?;
        journal.sync()?;
    }
    let mut block = [0; BLOCK];
    let mut journal = Journal::mount(Loopback::new(&bytes)?, &mut block)?;
    journal.lookup(journal.root(), &name(0))?;
    let first = journal.tree_stats()?.cache;
    journal.lookup(journal.root(), &name(0))?;
    let second = journal.tree_stats()?.cache;

    assert!(second.hits > first.hits, "cached node was not hit: {first:?} -> {second:?}");
    assert_eq!(second.misses, first.misses, "cache hit fetched another node");
    Ok(())
}
