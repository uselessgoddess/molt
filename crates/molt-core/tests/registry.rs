use molt_core::capability::{CapabilityError, CapabilityRights, CellId, Rights};
use molt_core::registry::{Registry, RegistryError, Scheme};

/// A service whose endpoint is the checkpoint it was published at.
enum Storage {}

impl CapabilityRights for Storage {
    const MASK: Rights = Rights::READ_WRITE;
}

impl Scheme for Storage {
    type Endpoint = u64;

    const NAME: &'static str = "storage";
}

const SERVICE: CellId = CellId::new(3);

#[test]
fn a_scheme_nobody_publishes_answers_nobody() {
    let names = Registry::<Storage, 2>::new();

    assert_eq!(names.acquire().err(), Some(RegistryError::Unavailable));
}

#[test]
fn a_lease_reads_the_published_endpoint() {
    let mut names = Registry::<Storage, 2>::new();
    names.publish(SERVICE, 7).unwrap();

    let lease = names.acquire().unwrap();

    assert_eq!(names.endpoint(lease), Ok(&7));
}

#[test]
fn withdrawal_leaves_the_lease_stale() {
    let mut names = Registry::<Storage, 2>::new();
    names.publish(SERVICE, 7).unwrap();
    let lease = names.acquire().unwrap();

    assert_eq!(names.withdraw(SERVICE), 1);

    assert_eq!(names.endpoint(lease), Err(CapabilityError::Stale));
    assert_eq!(names.acquire().err(), Some(RegistryError::Unavailable), "a withdrawn scheme");
}

#[test]
fn a_republished_endpoint_needs_a_fresh_lease() {
    let mut names = Registry::<Storage, 2>::new();
    names.publish(SERVICE, 7).unwrap();
    let stale = names.acquire().unwrap();
    names.withdraw(SERVICE);

    names.publish(SERVICE, 9).unwrap();

    assert_eq!(names.endpoint(stale), Err(CapabilityError::Stale), "a lease outlived its epoch");
    assert_eq!(names.endpoint(names.acquire().unwrap()), Ok(&9));
}

#[test]
fn a_full_registry_refuses_another_publication() {
    let mut names = Registry::<Storage, 1>::new();
    names.publish(SERVICE, 7).unwrap();

    assert_eq!(names.publish(CellId::new(4), 9).err(), Some(RegistryError::Full));
}
