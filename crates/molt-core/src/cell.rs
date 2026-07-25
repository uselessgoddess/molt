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
///
/// Neither `Send` nor `'static`: a cell owns what it was started with, and the
/// first real one owns a mounted device that borrows a mapped window. Those
/// bounds belong to whatever moves a cell between cores, which is Stage 4's to
/// add where the move happens rather than the trait's to demand from every
/// implementor.
pub trait Cell: Sized {
    /// What starting one takes — a device, a queue, a counter.
    type State;
    /// Why starting or restarting failed.
    type Error;

    fn spawn(state: Self::State) -> Result<Self, Self::Error>;

    /// Returns the cell to what it started from, keeping what it cannot rebuild.
    ///
    /// The supervisor's hooks have run by the time this is called, so nothing
    /// is in flight and no exported capability is live. A cell that cannot come
    /// back reports the error and refuses work from then on rather than serving
    /// out of half of itself; the supervisor keeps holding it either way, and
    /// recovery is its supervisor's, by dropping it and starting another.
    fn restart(&mut self) -> Result<(), Self::Error>;
}

/// A cell addressed by message, as one behind a ring is.
///
/// Separate from [`Cell`] because a lifecycle is not a protocol. A service
/// whose operations need more than one value — the filesystem takes an owner
/// and a buffer registry per request — is a cell without being one of these,
/// and a message pair that only wraps `FsOp` would say nothing.
pub trait Handler: Cell {
    type Message;
    type Reply;

    fn handle(&mut self, message: Self::Message) -> Self::Reply;
}

/// Supervisor integrations invoked in deterministic restart order.
pub trait RestartHooks {
    fn stop_submissions(&mut self);
    fn cancel_requests(&mut self);
    fn revoke_capabilities(&mut self);
}

/// Hooks for a cell with nothing in flight, where every step already holds.
pub struct Quiesced;

impl RestartHooks for Quiesced {
    fn stop_submissions(&mut self) {}
    fn cancel_requests(&mut self) {}
    fn revoke_capabilities(&mut self) {}
}

/// Whether the supervised cell is still answering.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Health {
    /// Started, or restarted, and serving.
    Running,
    /// A restart ran its hooks and the cell did not come back.
    Failed,
}

/// Owns a cell, its arena, restart generation, and heartbeat.
pub struct Supervisor<C: Cell, A = ()> {
    cell: C,
    arena: A,
    identity: CellIdentity,
    heartbeat: u64,
    health: Health,
}

impl<C: Cell> Supervisor<C, ()> {
    pub fn new(state: C::State) -> Result<Self, C::Error> {
        Self::with_arena(CellId::new(0), state, ())
    }

    /// Restarts the cell, there being no arena to replace.
    pub fn restart(&mut self, hooks: &mut impl RestartHooks) -> Result<(), C::Error> {
        self.restart_with_arena((), hooks)
    }
}

impl<C: Cell, A> Supervisor<C, A> {
    pub fn with_arena(id: CellId, state: C::State, arena: A) -> Result<Self, C::Error> {
        Ok(Self {
            cell: C::spawn(state)?,
            arena,
            identity: CellIdentity { id, generation: 0 },
            heartbeat: 0,
            health: Health::Running,
        })
    }

    pub fn cell(&self) -> &C {
        &self.cell
    }

    pub fn cell_mut(&mut self) -> &mut C {
        &mut self.cell
    }

    pub fn call(&mut self, message: C::Message) -> C::Reply
    where
        C: Handler,
    {
        self.cell.handle(message)
    }

    pub const fn identity(&self) -> CellIdentity {
        self.identity
    }

    pub const fn generation(&self) -> u64 {
        self.identity.generation
    }

    pub const fn health(&self) -> Health {
        self.health
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

    /// Performs stop, cancellation, revocation, arena replacement, and restart
    /// in order.
    ///
    /// The generation moves only when the cell comes back, so a caller cannot
    /// mistake a failed restart for an epoch nobody is using. A supervisor that
    /// is already [`Health::Failed`] skips the hooks: the epoch that had
    /// submissions and capabilities ended with the attempt that failed, and
    /// running them again would revoke on behalf of a cell that exported
    /// nothing.
    pub fn restart_with_arena(
        &mut self,
        arena: A,
        hooks: &mut impl RestartHooks,
    ) -> Result<(), C::Error> {
        if self.health == Health::Running {
            hooks.stop_submissions();
            hooks.cancel_requests();
            hooks.revoke_capabilities();
        }
        drop(core::mem::replace(&mut self.arena, arena));
        self.cell.restart().inspect_err(|_| self.health = Health::Failed)?;
        self.health = Health::Running;
        self.identity.generation = self.identity.generation.wrapping_add(1);
        self.heartbeat = 0;
        Ok(())
    }
}
