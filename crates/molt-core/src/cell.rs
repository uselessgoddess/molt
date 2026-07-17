//! Typed, restartable components for the single-address-space kernel.

/// Stable identity assigned by the supervisor.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct CellId(u32);

impl CellId {
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

/// A cell ID paired with its current restart generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CellIdentity {
    id: CellId,
    generation: u64,
}

impl CellIdentity {
    pub const fn id(self) -> CellId {
        self.id
    }

    pub const fn generation(self) -> u64 {
        self.generation
    }
}

/// A statically linked unit of code and owned state.
pub trait Cell: Send + 'static {
    type Message;
    type Reply;
    type State: Default;

    fn spawn(state: Self::State) -> Self;
    fn handle(&mut self, message: Self::Message) -> Self::Reply;
}

/// Supervisor integrations invoked in deterministic restart order.
pub trait RestartHooks {
    fn stop_submissions(&mut self);
    fn cancel_requests(&mut self);
    fn revoke_capabilities(&mut self);
}

struct NoopHooks;

impl RestartHooks for NoopHooks {
    fn stop_submissions(&mut self) {}
    fn cancel_requests(&mut self) {}
    fn revoke_capabilities(&mut self) {}
}

/// Owns a cell, its arena, restart generation, and heartbeat.
pub struct Supervisor<C: Cell, A = ()> {
    cell: C,
    arena: A,
    identity: CellIdentity,
    heartbeat: u64,
}

impl<C: Cell> Supervisor<C, ()> {
    pub fn new(state: C::State) -> Self {
        Self::with_arena(CellId::new(0), state, ())
    }

    pub fn restart(&mut self, state: C::State) {
        self.restart_managed(state, (), &mut NoopHooks);
    }

    pub fn restart_default(&mut self) {
        self.restart(C::State::default());
    }
}

impl<C: Cell, A> Supervisor<C, A> {
    pub fn with_arena(id: CellId, state: C::State, arena: A) -> Self {
        Self {
            cell: C::spawn(state),
            arena,
            identity: CellIdentity { id, generation: 0 },
            heartbeat: 0,
        }
    }

    pub fn call(&mut self, message: C::Message) -> C::Reply {
        self.cell.handle(message)
    }

    pub const fn identity(&self) -> CellIdentity {
        self.identity
    }

    pub const fn generation(&self) -> u64 {
        self.identity.generation
    }

    pub const fn heartbeat(&self) -> u64 {
        self.heartbeat
    }

    pub fn record_heartbeat(&mut self, tick: u64) {
        self.heartbeat = tick;
    }

    pub fn arena(&self) -> &A {
        &self.arena
    }

    /// Performs stop, cancellation, revocation, arena drop, and respawn in order.
    pub fn restart_managed(&mut self, state: C::State, arena: A, hooks: &mut impl RestartHooks) {
        hooks.stop_submissions();
        hooks.cancel_requests();
        hooks.revoke_capabilities();
        let old_arena = core::mem::replace(&mut self.arena, arena);
        drop(old_arena);
        self.cell = C::spawn(state);
        self.identity.generation = self.identity.generation.wrapping_add(1);
        self.heartbeat = 0;
    }
}

impl<C: Cell> Default for Supervisor<C, ()> {
    fn default() -> Self {
        Self::new(C::State::default())
    }
}
