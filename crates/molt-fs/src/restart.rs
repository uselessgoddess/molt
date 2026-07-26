//! What ends an epoch on the filesystem ring.
//!
//! A restart is more than a cell coming back. The ring still holds work nobody
//! will run, a client still holds handles into a table that is about to be
//! replaced, and the namespace still says the service is answering. The
//! supervisor calls [`RestartHooks`] for exactly those, in an order that is the
//! whole point: stop first, so nothing new joins the epoch being ended; answer
//! next, so a client waiting on a request learns it died rather than waiting
//! for a service that is gone; revoke last, so authority is taken away only
//! once no request that would use it is still in flight.
//!
//! Both hooks here do all three. [`Teardown`] ends the service's epoch and
//! [`Disconnect`] ends one client's, and the difference between them is only
//! what the last step takes back: a publication everyone acquires, or the
//! handles one cell opened.

use core::cell::RefCell;

use molt_block::Disk;
use molt_core::capability::CellId;
use molt_core::cell::RestartHooks;
use molt_core::registry::Registry;
use molt_core::ring::{Completion, IoDriver, RequestId};

use crate::FsError;
use crate::op::{FsDone, FsOp};
use crate::service::Fs;
use crate::storage::Storage;

/// The filesystem ring as a restart sees it: full of work with no owner.
type Ring<'ring, const R: usize> = IoDriver<'ring, FsOp, Result<FsDone, FsError>, R>;

/// What an ending epoch left on the ring.
struct Stopped<const R: usize> {
    ids: [Option<RequestId>; R],
    taken: usize,
    unanswered: usize,
}

impl<const R: usize> Stopped<R> {
    const fn new() -> Self {
        Self { ids: [None; R], taken: 0, unanswered: 0 }
    }

    /// Takes every operation off the submission queue.
    ///
    /// The record is exactly as large as the queue, so it holds everything a
    /// drain can yield. Anything submitted after this belongs to the epoch
    /// starting, not the one ending, and is served normally.
    fn stop(&mut self, driver: &mut Ring<'_, R>) {
        while let Some(submission) = driver.try_next() {
            match self.ids.get_mut(self.taken) {
                Some(slot) => {
                    *slot = Some(submission.id());
                    self.taken += 1;
                }
                None => self.unanswered += 1,
            }
        }
    }

    /// Answers each of them, because every submission is answered.
    ///
    /// A completion queue with no room left leaves its request unanswered,
    /// which a client meets as a wait that nothing ends — its own supervisor's
    /// problem, and better than a queue that quietly forgot the request while
    /// claiming the ring keeps its side of the bargain.
    fn cancel(&mut self, driver: &mut Ring<'_, R>) {
        for id in self.ids.iter().take(self.taken).flatten() {
            if driver.try_complete(Completion::new(*id, Err(FsError::Cancelled))).is_err() {
                self.unanswered += 1;
            }
        }
    }
}

/// Ends the filesystem's epoch: cancels the ring, withdraws the publication.
///
/// The registry is shared rather than borrowed exclusively because a client
/// holds it too — acquiring a lease is a read, and the one moment it is written
/// is here.
pub struct Teardown<'a, 'ring, const R: usize, const M: usize> {
    driver: &'a mut Ring<'ring, R>,
    names: &'a RefCell<Registry<Storage, M>>,
    provider: CellId,
    stopped: Stopped<R>,
    withdrawn: usize,
}

impl<'a, 'ring, const R: usize, const M: usize> Teardown<'a, 'ring, R, M> {
    pub const fn new(
        driver: &'a mut Ring<'ring, R>,
        names: &'a RefCell<Registry<Storage, M>>,
        provider: CellId,
    ) -> Self {
        Self { driver, names, provider, stopped: Stopped::new(), withdrawn: 0 }
    }

    /// How many requests the ending epoch never ran.
    pub const fn cancelled(&self) -> usize {
        self.stopped.taken
    }

    /// How many of them nobody could be told about.
    pub const fn unanswered(&self) -> usize {
        self.stopped.unanswered
    }

    /// How many publications went, leaving every lease on them stale.
    pub const fn withdrawn(&self) -> usize {
        self.withdrawn
    }
}

impl<const R: usize, const M: usize> RestartHooks for Teardown<'_, '_, R, M> {
    fn stop_submissions(&mut self) {
        self.stopped.stop(self.driver);
    }

    fn cancel_requests(&mut self) {
        self.stopped.cancel(self.driver);
    }

    fn revoke_capabilities(&mut self) {
        self.withdrawn += self.names.borrow_mut().withdraw(self.provider);
    }
}

/// Ends one client's epoch: cancels the ring, revokes what it had open.
///
/// A client that restarts keeps nothing, and the filesystem it was talking to
/// keeps serving: this is the hook that makes those two statements agree, by
/// taking the handles back on the client's behalf, since the client is in no
/// position to close them itself.
pub struct Disconnect<'a, 'ring, D, const R: usize, const N: usize> {
    driver: &'a mut Ring<'ring, R>,
    fs: &'a mut Fs<D, N>,
    client: CellId,
    stopped: Stopped<R>,
    revoked: usize,
}

impl<'a, 'ring, D: Disk, const R: usize, const N: usize> Disconnect<'a, 'ring, D, R, N> {
    pub const fn new(driver: &'a mut Ring<'ring, R>, fs: &'a mut Fs<D, N>, client: CellId) -> Self {
        Self { driver, fs, client, stopped: Stopped::new(), revoked: 0 }
    }

    /// How many requests the client will never hear the answer to.
    pub const fn cancelled(&self) -> usize {
        self.stopped.taken
    }

    /// How many handles the client held when it stopped.
    pub const fn revoked(&self) -> usize {
        self.revoked
    }
}

impl<D: Disk, const R: usize, const N: usize> RestartHooks for Disconnect<'_, '_, D, R, N> {
    fn stop_submissions(&mut self) {
        self.stopped.stop(self.driver);
    }

    fn cancel_requests(&mut self) {
        self.stopped.cancel(self.driver);
    }

    fn revoke_capabilities(&mut self) {
        self.revoked += self.fs.revoke(self.client);
    }
}

#[cfg(all(test, feature = "format"))]
mod tests {
    use alloc::vec::Vec;
    use core::cell::RefCell;

    use molt_block::Loopback;
    use molt_core::buffer::BufferRegistry;
    use molt_core::capability::{CapabilityError, CellId};
    use molt_core::cell::{RestartHooks, Supervisor};
    use molt_core::registry::Registry;
    use molt_core::ring::{IoRing, RequestId, Submission};

    use super::{Disconnect, Teardown};
    use crate::cell::FsCell;
    use crate::format::{Tree, build};
    use crate::op::{FsDone, FsOp};
    use crate::service::Fs;
    use crate::storage::Storage;
    use crate::{FsError, Name};

    const SERVICE: CellId = CellId::new(2);
    const CLIENT: CellId = CellId::new(3);

    fn image() -> Vec<u8> {
        let mut tree = Tree::new();
        tree.dir("docs").unwrap().file("readme", b"read me\n".to_vec()).unwrap();
        build(&tree, 1).unwrap()
    }

    #[test]
    fn a_stopped_request_is_answered_cancelled() -> Result<(), FsError> {
        let bytes = image();
        let names = RefCell::new(Registry::<Storage, 1>::new());
        let mut fs = Fs::<_, 4>::mount(Loopback::new(&bytes)?)?;
        let mut ring = IoRing::<FsOp, Result<FsDone, FsError>, 4>::new();
        let (mut client, mut driver) = ring.split();
        let root = fs.publish(&mut names.borrow_mut(), SERVICE)?;
        let root = names.borrow().endpoint(root)?.root();
        let op = FsOp::Entry { dir: root, index: 0 };
        client.try_submit(Submission::new(RequestId::new(7), op)).unwrap();

        let mut hooks = Teardown::new(&mut driver, &names, SERVICE);
        hooks.stop_submissions();
        hooks.cancel_requests();

        assert_eq!(hooks.cancelled(), 1);
        assert_eq!(hooks.unanswered(), 0);
        let answer = client.try_completion().expect("every submission is answered");
        assert_eq!(answer.id(), RequestId::new(7));
        assert_eq!(answer.into_result(), Err(FsError::Cancelled));
        Ok(())
    }

    #[test]
    fn teardown_leaves_every_lease_stale() -> Result<(), FsError> {
        let bytes = image();
        let names = RefCell::new(Registry::<Storage, 1>::new());
        let mut fs = Fs::<_, 4>::mount(Loopback::new(&bytes)?)?;
        let mut ring = IoRing::<FsOp, Result<FsDone, FsError>, 4>::new();
        let (_client, mut driver) = ring.split();
        fs.publish(&mut names.borrow_mut(), SERVICE)?;
        let lease = names.borrow().acquire().expect("a published scheme");

        let mut hooks = Teardown::new(&mut driver, &names, SERVICE);
        hooks.revoke_capabilities();

        assert_eq!(hooks.withdrawn(), 1);
        assert_eq!(names.borrow().endpoint(lease), Err(CapabilityError::Stale));
        Ok(())
    }

    #[test]
    fn a_supervised_restart_republishes_the_mount() -> Result<(), FsError> {
        let bytes = image();
        let names = RefCell::new(Registry::<Storage, 1>::new());
        let mut ring = IoRing::<FsOp, Result<FsDone, FsError>, 4>::new();
        let (_client, mut driver) = ring.split();
        let mut service = Supervisor::<FsCell<_, 4>>::new(Loopback::new(&bytes)?)?;
        service.cell_mut().fs()?.publish(&mut names.borrow_mut(), SERVICE)?;
        let lease = names.borrow().acquire().expect("a published scheme");

        service.restart(&mut Teardown::new(&mut driver, &names, SERVICE))?;
        service.cell_mut().fs()?.publish(&mut names.borrow_mut(), SERVICE)?;

        assert_eq!(names.borrow().endpoint(lease), Err(CapabilityError::Stale), "a lease survived");
        let fresh = names.borrow().acquire().expect("a republished scheme");
        assert!(names.borrow().endpoint(fresh).is_ok());
        Ok(())
    }

    #[test]
    fn disconnect_revokes_only_what_the_client_opened() -> Result<(), FsError> {
        let bytes = image();
        let names = RefCell::new(Registry::<Storage, 1>::new());
        let mut buffers = BufferRegistry::<1>::new();
        let mut fs = Fs::<_, 4>::mount(Loopback::new(&bytes)?)?;
        let mut ring = IoRing::<FsOp, Result<FsDone, FsError>, 4>::new();
        let (_client, mut driver) = ring.split();
        let publication = fs.publish(&mut names.borrow_mut(), SERVICE)?;
        let root = names.borrow().endpoint(publication)?.root();
        let name = Name::try_from("docs")?;
        fs.apply(CLIENT, FsOp::Open { dir: root, name }, &mut buffers)?;

        let mut hooks = Disconnect::new(&mut driver, &mut fs, CLIENT);
        hooks.revoke_capabilities();

        assert_eq!(hooks.revoked(), 1, "the client's open directory outlived it");
        assert_eq!(names.borrow().endpoint(publication)?.root(), root);
        assert!(fs.apply(CLIENT, FsOp::Entry { dir: root, index: 0 }, &mut buffers).is_ok());
        Ok(())
    }
}
