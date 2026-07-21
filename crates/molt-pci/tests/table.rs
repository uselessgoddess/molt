use molt_pci::msix::{ENTRY, Table};
use molt_pci::{Error, Message};

/// The table is device memory in a BAR, so a test stands a buffer in for it:
/// what is being checked is the layout and the order the words are written in,
/// which is what a device reads either way.
fn table(vectors: u16) -> (Vec<u32>, Table) {
    let mut entries = vec![0u32; usize::from(vectors) * ENTRY as usize / 4];
    // SAFETY: the buffer is exactly `vectors` entries and outlives the table,
    // which the test keeps alongside it.
    let table = unsafe { Table::new(entries.as_mut_ptr(), vectors) };
    (entries, table)
}

#[test]
fn a_programmed_vector_stays_masked() {
    let (entries, table) = table(2);

    table.program(1, Message::new(0xfee0_0000, 0x41)).unwrap();
    assert_eq!(&entries[4..8], [0xfee0_0000, 0, 0x41, 1]);
}

#[test]
fn unmasking_leaves_the_message_alone() {
    let (entries, table) = table(1);
    table.program(0, Message::new(0x2800_0000, 7)).unwrap();

    table.mask(0, false).unwrap();
    assert_eq!(table.masked(0), Ok(false));
    assert_eq!(&entries[0..3], [0x2800_0000, 0, 7]);
}

#[test]
fn a_vector_past_the_table_is_refused() {
    let (_entries, table) = table(1);

    assert_eq!(table.program(1, Message::new(0, 0)), Err(Error::Vector));
    assert_eq!(table.mask(1, true), Err(Error::Vector));
}

#[test]
fn a_wide_message_reaches_both_halves() {
    let (entries, table) = table(1);

    table.program(0, Message::new(0x1_0000_2000, 9)).unwrap();
    assert_eq!(&entries[0..2], [0x0000_2000, 0x0000_0001]);
}
