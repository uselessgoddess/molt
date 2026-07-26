use molt_core::audit::{Action, Event, Log};
use molt_core::capability::{
    CapabilityError, CapabilityTable, CellId, Read, ReadWrite, Rights, Write,
};

#[derive(Debug, Eq, PartialEq)]
struct Buffer(u32);

#[test]
fn typed_caps_revoked_by_cell() {
    let owner = CellId::new(7);
    let mut table = CapabilityTable::<Buffer, 2>::new();
    let read_write = table.insert::<ReadWrite>(owner, Buffer(41)).unwrap();
    let read = table.attenuate::<ReadWrite, Read>(read_write).unwrap();

    assert_eq!(table.get(read).unwrap(), &Buffer(41));
    assert_eq!(Read::MASK, Rights::READ);
    assert_eq!(Write::MASK, Rights::WRITE);

    let revoked = table.revoke_owner(owner);
    assert_eq!(revoked, 1);
    assert_eq!(table.get(read), Err(CapabilityError::Stale));

    let replacement = table.insert::<ReadWrite>(owner, Buffer(99)).unwrap();
    assert_ne!(replacement.raw(), read_write.raw());
    assert_eq!(table.get(replacement).unwrap(), &Buffer(99));
}

#[test]
fn revoked_cap_frees_its_slot() {
    let owner = CellId::new(3);
    let mut table = CapabilityTable::<Buffer, 1>::new();
    let read = table.insert::<Read>(owner, Buffer(5)).unwrap();

    assert_eq!(table.revoke(read), Ok(Buffer(5)));
    assert_eq!(table.get(read), Err(CapabilityError::Stale));
    assert_eq!(table.revoke(read), Err(CapabilityError::Stale));
    assert!(table.insert::<Read>(owner, Buffer(6)).is_ok(), "the slot stayed taken");
}

#[test]
fn attenuation_cannot_add_rights() {
    let mut table = CapabilityTable::<Buffer, 1>::new();
    let read = table.insert::<Read>(CellId::new(1), Buffer(1)).unwrap();

    assert_eq!(table.attenuate::<Read, ReadWrite>(read), Err(CapabilityError::InsufficientRights));
}

#[test]
fn delegate_narrows_and_records() -> Result<(), CapabilityError> {
    let owner = CellId::new(1);
    let peer = CellId::new(2);
    let mut audit = Log::<4>::new();
    let mut table = CapabilityTable::<Buffer, 1>::new();
    let read_write = table.insert::<ReadWrite>(owner, Buffer(7)).unwrap();

    let read = table.delegate::<ReadWrite, Read>(read_write, owner, peer, &mut audit)?;

    assert_eq!(table.get(read)?, &Buffer(7));
    assert_eq!(audit.last(), Some(Event::delegate(owner, peer, 0, Rights::READ)));
    Ok(())
}

#[test]
fn delegate_cannot_widen() {
    let owner = CellId::new(1);
    let mut audit = Log::<1>::new();
    let mut table = CapabilityTable::<Buffer, 1>::new();
    let read = table.insert::<Read>(owner, Buffer(1)).unwrap();

    let widened = table.delegate::<Read, ReadWrite>(read, owner, CellId::new(2), &mut audit);

    assert_eq!(widened, Err(CapabilityError::InsufficientRights));
    assert!(audit.is_empty(), "a refused delegation records nothing");
}

#[test]
fn revoke_stales_delegated() -> Result<(), CapabilityError> {
    let owner = CellId::new(1);
    let mut audit = Log::<1>::new();
    let mut table = CapabilityTable::<Buffer, 1>::new();
    let read_write = table.insert::<ReadWrite>(owner, Buffer(3)).unwrap();
    let read = table.delegate::<ReadWrite, Read>(read_write, owner, CellId::new(2), &mut audit)?;

    table.revoke_owner(owner);

    assert_eq!(table.get(read), Err(CapabilityError::Stale));
    assert_eq!(audit.last().map(|event| event.action), Some(Action::Delegate));
    Ok(())
}
