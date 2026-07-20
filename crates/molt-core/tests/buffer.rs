use molt_core::buffer::{BufferError, BufferOperation, BufferRegistry};
use molt_core::capability::CapabilityError;
use molt_core::cell::CellId;

#[test]
fn typed_and_checked_ranges() {
    let owner = CellId::new(7);
    let mut bytes = [0_u8; 8];
    let mut registry = BufferRegistry::<2>::new();
    let read_write = registry.register_read_write(owner, &mut bytes).unwrap();
    let read = registry.read_capability(read_write).unwrap();
    let write = registry.write_capability(read_write).unwrap();

    registry.resolve_write(BufferOperation::new(write, 2, 3)).unwrap().copy_from_slice(&[1, 2, 3]);
    assert_eq!(registry.resolve_read(BufferOperation::new(read, 1, 5)), Ok(&[0, 1, 2, 3, 0][..]));
    assert_eq!(
        registry.resolve_read(BufferOperation::new(read, usize::MAX, 2)),
        Err(BufferError::OutOfBounds)
    );

    assert_eq!(registry.revoke_owner(owner), 1);
    assert_eq!(
        registry.resolve_read(BufferOperation::new(read, 0, 1)),
        Err(BufferError::Capability(CapabilityError::Stale))
    );
}
