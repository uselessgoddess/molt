use molt_core::CellId;
use molt_core::audit::{Action, Event, Log};
use molt_core::capability::Rights;

fn grant(owner: u32) -> Event {
    Event::grant(CellId::new(owner), owner, Rights::READ)
}

#[test]
fn log_keeps_order() {
    let mut log = Log::<4>::new();
    log.record(grant(1));
    log.record(grant(2));

    let resources: Vec<_> = log.iter().map(|event| event.resource).collect();
    assert_eq!(resources, [1, 2]);
    assert_eq!(log.dropped(), 0);
}

#[test]
fn full_log_drops_oldest() {
    let mut log = Log::<2>::new();
    log.record(grant(1));
    log.record(grant(2));
    log.record(grant(3));

    let resources: Vec<_> = log.iter().map(|event| event.resource).collect();
    assert_eq!(resources, [2, 3], "oldest event fell off the ring");
    assert_eq!(log.len(), 2);
    assert_eq!(log.dropped(), 1);
}

#[test]
fn last_is_newest() {
    let mut log = Log::<2>::new();
    log.record(grant(5));
    log.record(Event::revoke(CellId::new(5), 5, Rights::READ));

    assert_eq!(log.last().map(|event| event.action), Some(Action::Revoke));
}
