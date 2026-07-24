use molt_block::Loopback;
use molt_fs::format::{Tree, build};
use molt_fs::{BLOCK, FsError, Journal, Kind, Name};

fn image() -> Vec<u8> {
    build(&Tree::new(), 1).expect("image")
}

fn name(index: usize) -> Name {
    Name::try_from(format!("file-{index:02}").as_str()).expect("name")
}

#[test]
fn tree_splits_and_remounts() {
    let mut bytes = image();
    {
        let mut block = [0; BLOCK];
        let mut journal =
            Journal::mount(Loopback::writable(&mut bytes).expect("disk"), &mut block)
                .expect("mount");
        for index in 0..40 {
            journal.create(journal.root(), name(index), Kind::File).expect("create");
        }
        journal.sync().expect("sync");

        let stats = journal.tree_stats().expect("stats");
        assert!(stats.height >= 2, "forty keys stayed in one leaf: {stats:?}");
        assert!(stats.nodes >= 4, "split did not create a real tree: {stats:?}");
    }

    let mut block = [0; BLOCK];
    let mut journal =
        Journal::mount(Loopback::new(&bytes).expect("disk"), &mut block).expect("remount");
    for index in 0..40 {
        assert!(journal.lookup(journal.root(), &name(index)).is_ok(), "missing key {index}");
    }
}

#[test]
fn root_swing_hides_unsynced_tree() {
    let mut bytes = image();
    let stable_root;
    {
        let mut block = [0; BLOCK];
        let mut journal =
            Journal::mount(Loopback::writable(&mut bytes).expect("disk"), &mut block)
                .expect("mount");
        journal.create(journal.root(), name(1), Kind::File).expect("create");
        journal.sync().expect("sync");
        stable_root = journal.tree_stats().expect("stats").root;

        journal.create(journal.root(), name(2), Kind::File).expect("create");
        let pending_root = journal.tree_stats().expect("stats").root;
        assert_ne!(pending_root, stable_root, "mutation rewrote committed root");
    }

    let mut block = [0; BLOCK];
    let mut journal =
        Journal::mount(Loopback::new(&bytes).expect("disk"), &mut block).expect("remount");
    assert!(journal.lookup(journal.root(), &name(1)).is_ok());
    assert_eq!(journal.lookup(journal.root(), &name(2)), Err(FsError::Missing));
    assert_eq!(journal.tree_stats().expect("stats").root, stable_root);
}

#[test]
fn cache_hit_skips_device_read() {
    let mut bytes = image();
    {
        let mut block = [0; BLOCK];
        let mut journal =
            Journal::mount(Loopback::writable(&mut bytes).expect("disk"), &mut block)
                .expect("mount");
        journal.create(journal.root(), name(0), Kind::File).expect("create");
        journal.sync().expect("sync");
    }

    let mut block = [0; BLOCK];
    let mut journal =
        Journal::mount(Loopback::new(&bytes).expect("disk"), &mut block).expect("remount");
    journal.lookup(journal.root(), &name(0)).expect("first lookup");
    let first = journal.tree_stats().expect("stats").cache;
    journal.lookup(journal.root(), &name(0)).expect("second lookup");
    let second = journal.tree_stats().expect("stats").cache;

    assert!(second.hits > first.hits, "cached node was not hit: {first:?} -> {second:?}");
    assert_eq!(second.misses, first.misses, "cache hit fetched another node");
}
