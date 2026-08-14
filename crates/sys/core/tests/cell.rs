use std::sync::{Arc, Mutex};

use molt_core::cell::{Cell, CellId, Handler, Health, Quiesced, RestartHooks, Supervisor};

#[derive(Debug, Eq, PartialEq)]
struct Fault;

struct Counter(u64);

impl Cell for Counter {
    type Error = Fault;
    type State = u64;

    fn spawn(start: u64) -> Result<Self, Fault> {
        Ok(Self(start))
    }

    fn restart(&mut self) -> Result<(), Fault> {
        self.0 = 0;
        Ok(())
    }
}

impl Handler for Counter {
    type Message = u64;
    type Reply = u64;

    fn handle(&mut self, increment: u64) -> u64 {
        self.0 += increment;
        self.0
    }
}

/// Starts only when asked to, and never comes back — a mount whose disk left.
struct Fragile;

impl Cell for Fragile {
    type Error = Fault;
    type State = bool;

    fn spawn(starts: bool) -> Result<Self, Fault> {
        starts.then_some(Self).ok_or(Fault)
    }

    fn restart(&mut self) -> Result<(), Fault> {
        Err(Fault)
    }
}

struct Arena(Arc<Mutex<Vec<&'static str>>>);

impl Drop for Arena {
    fn drop(&mut self) {
        self.0.lock().unwrap().push("drop arena");
    }
}

struct Hooks(Arc<Mutex<Vec<&'static str>>>);

impl RestartHooks for Hooks {
    fn stop_submissions(&mut self) {
        self.0.lock().unwrap().push("stop");
    }

    fn cancel_requests(&mut self) {
        self.0.lock().unwrap().push("cancel");
    }

    fn revoke_capabilities(&mut self) {
        self.0.lock().unwrap().push("revoke");
    }
}

#[test]
fn restart_drops_what_it_counted() -> Result<(), Fault> {
    let mut supervisor = Supervisor::<Counter>::new(4)?;
    assert_eq!(supervisor.call(1), 5);

    supervisor.restart(&mut Quiesced)?;

    assert_eq!(supervisor.call(2), 2);
    assert_eq!(supervisor.generation(), 1);
    Ok(())
}

#[test]
fn spawn_error_reaches_caller() {
    assert_eq!(Supervisor::<Fragile>::new(false).err(), Some(Fault));
}

#[test]
fn restart_drops_arena_after_revoke() -> Result<(), Fault> {
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut supervisor =
        Supervisor::<Counter, Arena>::with_arena(CellId::new(9), 3, Arena(events.clone()))?;
    let mut hooks = Hooks(events.clone());

    supervisor.restart_with_arena(Arena(events.clone()), &mut hooks)?;

    assert_eq!(*events.lock().unwrap(), ["stop", "cancel", "revoke", "drop arena"]);
    assert_eq!(supervisor.identity().id(), CellId::new(9));
    assert_eq!(supervisor.identity().generation(), 1);
    assert_eq!(supervisor.call(2), 2);
    Ok(())
}

#[test]
fn failed_restart_holds_generation() -> Result<(), Fault> {
    let mut supervisor = Supervisor::<Fragile>::new(true)?;

    assert_eq!(supervisor.restart(&mut Quiesced), Err(Fault));

    assert_eq!(supervisor.health(), Health::Failed);
    assert_eq!(supervisor.generation(), 0, "a failed restart published an epoch");
    Ok(())
}

#[test]
fn cell_reports_alone() -> Result<(), Fault> {
    let mut supervisor = Supervisor::<Counter>::new(4)?;
    supervisor.record_heartbeat(10);

    assert_eq!(supervisor.watch(20, 10, &mut Quiesced), None);

    assert_eq!(supervisor.call(1), 5, "the cell kept what it had counted");
    assert_eq!(supervisor.generation(), 0);
    Ok(())
}

#[test]
fn missed_deadline_restarts() -> Result<(), Fault> {
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut supervisor = Supervisor::<Counter>::new(4)?;
    let mut hooks = Hooks(events.clone());
    supervisor.record_heartbeat(10);

    assert_eq!(supervisor.watch(21, 10, &mut hooks), Some(Ok(())));

    assert_eq!(*events.lock().unwrap(), ["stop", "cancel", "revoke"]);
    assert_eq!(supervisor.call(2), 2, "the cell came back from where it started");
    assert_eq!(supervisor.generation(), 1);
    Ok(())
}

#[test]
fn watchdog_starts_next_deadline() -> Result<(), Fault> {
    let mut supervisor = Supervisor::<Counter>::new(4)?;

    supervisor.watch(11, 10, &mut Quiesced).unwrap()?;

    assert_eq!(supervisor.watch(12, 10, &mut Quiesced), None, "the new epoch inherited the miss");
    Ok(())
}

#[test]
fn asked_once_per_deadline() -> Result<(), Fault> {
    let mut supervisor = Supervisor::<Fragile>::new(true)?;

    assert_eq!(supervisor.watch(11, 10, &mut Quiesced), Some(Err(Fault)));

    assert_eq!(supervisor.watch(15, 10, &mut Quiesced), None);
    assert_eq!(supervisor.watch(22, 10, &mut Quiesced), Some(Err(Fault)));
    Ok(())
}

#[test]
fn failed_restart_runs_hooks_once() -> Result<(), Fault> {
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut supervisor = Supervisor::<Fragile>::new(true)?;
    let mut hooks = Hooks(events.clone());

    assert_eq!(supervisor.restart(&mut hooks), Err(Fault));
    assert_eq!(supervisor.restart(&mut hooks), Err(Fault));

    assert_eq!(*events.lock().unwrap(), ["stop", "cancel", "revoke"]);
    Ok(())
}
