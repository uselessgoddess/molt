use molt_core::cell::{Cell, Supervisor};

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
fn supervisor_dispatches_and_restarts_a_typed_cell() {
    let mut supervisor = Supervisor::<Counter>::new(CounterState::default());

    assert_eq!(supervisor.call(2), 2);
    assert_eq!(supervisor.call(3), 5);
    assert_eq!(supervisor.generation(), 0);

    supervisor.restart(CounterState { value: 40 });
    assert_eq!(supervisor.generation(), 1);
    assert_eq!(supervisor.call(2), 42);
}
