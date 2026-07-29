//! What a driver waits on when its queue is vectored.

/// A queue's interrupt line, as the driver above it sees one.
///
/// Polling a used ring costs a bus read for every miss and a whole core for
/// the wait. A queue routed through MSI-X answers with one posted write the
/// platform already counts, so the driver waits on that count and leaves what
/// waiting *means* — parking a task, halting the core, spinning while there is
/// still no executor — to whoever owns the line.
pub trait Arrivals {
    /// Blocks until the vector has fired since the last call.
    ///
    /// Answers how many arrivals it drained, or zero when it gave up: a wedged
    /// device has to end a request rather than a core.
    fn wait(&mut self) -> u64;
}
