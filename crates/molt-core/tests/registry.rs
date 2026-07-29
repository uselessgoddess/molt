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
fn empty_scheme_answers_nobody() {
    let names = Registry::<Storage, 2>::new();

    assert_eq!(names.acquire().err(), Some(RegistryError::Unavailable));
}

#[test]
fn lease_reads_published_endpoint() -> Result<(), RegistryError> {
    let mut names = Registry::<Storage, 2>::new();
    names.publish(SERVICE, 7)?;

    let lease = names.acquire()?;

    assert_eq!(names.endpoint(lease), Ok(&7));
    Ok(())
}

#[test]
fn withdraw_leaves_lease_stale() -> Result<(), RegistryError> {
    let mut names = Registry::<Storage, 2>::new();
    names.publish(SERVICE, 7)?;
    let lease = names.acquire()?;

    assert_eq!(names.withdraw(SERVICE), 1);

    assert_eq!(names.endpoint(lease), Err(CapabilityError::Stale));
    assert_eq!(names.acquire().err(), Some(RegistryError::Unavailable), "a withdrawn scheme");
    Ok(())
}

#[test]
fn republished_needs_fresh_lease() -> Result<(), RegistryError> {
    let mut names = Registry::<Storage, 2>::new();
    names.publish(SERVICE, 7)?;
    let stale = names.acquire()?;
    names.withdraw(SERVICE);

    names.publish(SERVICE, 9)?;

    assert_eq!(names.endpoint(stale), Err(CapabilityError::Stale), "a lease outlived its epoch");
    assert_eq!(names.endpoint(names.acquire()?), Ok(&9));
    Ok(())
}

#[test]
fn full_registry() -> Result<(), RegistryError> {
    let mut names = Registry::<Storage, 1>::new();
    names.publish(SERVICE, 7)?;

    assert_eq!(names.publish(CellId::new(4), 9).err(), Some(RegistryError::Full));
    Ok(())
}
