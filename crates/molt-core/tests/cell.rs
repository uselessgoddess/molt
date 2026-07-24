use std::sync::{Arc, Mutex};

use molt_core::cell::{Cell, CellId, RestartHooks, Supervisor};

#[derive(Default)]
struct CounterState {
    value: u64,
}

struct Counter(CounterState);

impl Cell for Counter {
    type Message = u64;
    type Reply = u64;
    type State = CounterState;

    fn spawn(state: Self::State) -> Self {
        Self(state)
    }

    fn handle(&mut self, increment: Self::Message) -> Self::Reply {
        self.0.value += increment;
        self.0.value
    }
}

#[test]
fn supervisor_lifecycle() {
    let mut supervisor = Supervisor::<Counter>::new(CounterState::default());

    assert_eq!(supervisor.call(2), 2);
    assert_eq!(supervisor.call(3), 5);
    assert_eq!(supervisor.generation(), 0);

    supervisor.restart(CounterState { value: 40 });
    assert_eq!(supervisor.generation(), 1);
    assert_eq!(supervisor.call(2), 42);
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
fn managed_restart_ordered_replaces_owned_arena() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut supervisor = Supervisor::<Counter, Arena>::with_arena(
        CellId::new(9),
        CounterState { value: 3 },
        Arena(events.clone()),
    );
    let mut hooks = Hooks(events.clone());

    supervisor.restart_managed(CounterState { value: 10 }, Arena(events.clone()), &mut hooks);

    assert_eq!(*events.lock().unwrap(), ["stop", "cancel", "revoke", "drop arena"]);
    assert_eq!(supervisor.identity().id(), CellId::new(9));
    assert_eq!(supervisor.identity().generation(), 1);
    assert_eq!(supervisor.call(2), 12);
}
