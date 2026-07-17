//! Typed, restartable components for the single-address-space kernel.

/// A statically linked unit of code and owned state.
pub trait Cell: Send + 'static {
    type Message;
    type Reply;
    type State: Default;

    fn spawn(state: Self::State) -> Self;
    fn handle(&mut self, message: Self::Message) -> Self::Reply;
}

/// Owns a cell and tracks its restart generation and heartbeat.
pub struct Supervisor<C: Cell> {
    cell: C,
    generation: u64,
    heartbeat: u64,
}

impl<C: Cell> Supervisor<C> {
    pub fn new(state: C::State) -> Self {
        Self { cell: C::spawn(state), generation: 0, heartbeat: 0 }
    }

    pub fn call(&mut self, message: C::Message) -> C::Reply {
        self.cell.handle(message)
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub const fn heartbeat(&self) -> u64 {
        self.heartbeat
    }

    pub fn record_heartbeat(&mut self, tick: u64) {
        self.heartbeat = tick;
    }

    pub fn restart(&mut self, state: C::State) {
        self.cell = C::spawn(state);
        self.generation = self.generation.wrapping_add(1);
        self.heartbeat = 0;
    }

    pub fn restart_default(&mut self) {
        self.restart(C::State::default());
    }
}

impl<C: Cell> Default for Supervisor<C> {
    fn default() -> Self {
        Self::new(C::State::default())
    }
}
