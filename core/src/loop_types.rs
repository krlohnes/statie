//! Data types for the session loop. The loop itself (`Session`/`Active`/
//! `step`) is implemented in a later milestone; only the data types live
//! here.

#[derive(Debug, Clone)]
pub enum Event {
    User(String),
    Tick,
    Cancel,
}

#[derive(Debug, Clone)]
pub enum Outcome {
    /// A tool ran; observation is pending; call `step(Event::Tick)` again.
    Continue,
    /// Wait for the next `User` event.
    Parked(Park),
    /// Activation terminal.
    Done,
    /// Router chose the named skill.
    Handoff(String),
}

#[derive(Debug, Clone)]
pub enum Park {
    AskUser(String),
    Yield,
    /// Retry cap hit.
    Failure(String),
    Cancelled,
}

/// An action that has committed and is ready to execute, but has not yet
/// executed (A3, A4). Not `Clone`. Returned only by `Session::commit`.
pub struct Executable<A> {
    inner: A,
}

impl<A> Executable<A> {
    /// Sole constructor, per the architecture: only `Session::commit`
    /// (the loop unit) is meant to produce an `Executable`.
    #[allow(dead_code)]
    pub(crate) fn new(action: A) -> Self {
        Executable { inner: action }
    }

    pub fn into_inner(self) -> A {
        self.inner
    }
}
