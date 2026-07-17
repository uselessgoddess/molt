use molt_core::capability::{
    CapabilityError, CapabilityTable, CellId, Read, ReadWrite, Rights, Write,
};

#[derive(Debug, Eq, PartialEq)]
struct Buffer(u32);

#[test]
fn capabilities_are_typed_and_revoked_by_cell_generation() {
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
fn attenuation_cannot_add_rights() {
    let mut table = CapabilityTable::<Buffer, 1>::new();
    let read = table.insert::<Read>(CellId::new(1), Buffer(1)).unwrap();

    assert_eq!(table.attenuate::<Read, ReadWrite>(read), Err(CapabilityError::InsufficientRights));
}
