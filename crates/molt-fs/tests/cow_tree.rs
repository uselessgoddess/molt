use molt_block::{Backing, Disk, Loopback};
use molt_fs::format::{self, Tree};
use molt_fs::{DEPTH, FsError, Journal, Kind, Name, attach};

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

#[test]
fn tree_splits_and_remounts() -> Result<(), FsError> {
    let mut bytes = image();
    {
        let (mut journal, mut backing) = mount(Loopback::writable(&mut bytes)?)?;
        let root = journal.root();
        let stats = backing.run(async {
            for index in 0..40 {
                journal.create(root, name(index), Kind::File).await?;
            }
            journal.sync().await?;
            journal.tree_stats().await
        })?;

        assert!(stats.height >= 2, "forty keys stayed in one leaf: {stats:?}");
        assert!(stats.nodes >= 4, "split did not create a real tree: {stats:?}");
    }
    let (mut journal, mut backing) = mount(Loopback::new(&bytes)?)?;
    let root = journal.root();
    backing.run(async {
        for index in 0..40 {
            assert!(journal.lookup(root, &name(index)).await.is_ok(), "missing key {index}");
            assert_eq!(journal.entry(root, index as u32).await?.0, name(index));
        }
        Ok::<_, FsError>(())
    })?;

    Ok(())
}

#[test]
fn root_swing_hides_unsynced_tree() -> Result<(), FsError> {
    let mut bytes = image();
    let stable_root;
    {
        let (mut journal, mut backing) = mount(Loopback::writable(&mut bytes)?)?;
        let root = journal.root();
        stable_root = backing.run(async {
            journal.create(root, name(1), Kind::File).await?;
            journal.sync().await?;
            let stable = journal.tree_stats().await?.root;

            journal.create(root, name(2), Kind::File).await?;
            let pending = journal.tree_stats().await?.root;
            assert_ne!(pending, stable, "mutation rewrote committed root");
            Ok::<_, FsError>(stable)
        })?;
    }
    let (mut journal, mut backing) = mount(Loopback::new(&bytes)?)?;
    let root = journal.root();
    backing.run(async {
        assert!(journal.lookup(root, &name(1)).await.is_ok());
        assert_eq!(journal.lookup(root, &name(2)).await, Err(FsError::Missing));
        assert_eq!(journal.tree_stats().await?.root, stable_root);
        Ok::<_, FsError>(())
    })?;
    Ok(())
}

#[test]
fn cache_hit_skips_device_read() -> Result<(), FsError> {
    let mut bytes = image();
    {
        let (mut journal, mut backing) = mount(Loopback::writable(&mut bytes)?)?;
        let root = journal.root();
        backing.run(async {
            journal.create(root, name(0), Kind::File).await?;
            journal.sync().await
        })?;
    }
    let (mut journal, mut backing) = mount(Loopback::new(&bytes)?)?;
    let root = journal.root();
    let (first, second) = backing.run(async {
        journal.lookup(root, &name(0)).await?;
        let first = journal.tree_stats().await?.cache;
        journal.lookup(root, &name(0)).await?;
        Ok::<_, FsError>((first, journal.tree_stats().await?.cache))
    })?;

    assert!(second.hits > first.hits, "cached node was not hit: {first:?} -> {second:?}");
    assert_eq!(second.misses, first.misses, "cache hit fetched another node");
    Ok(())
}
