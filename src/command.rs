//! The command channel into the store actor. Every command carries its own reply sender (a
//! oneshot, modeled with a `std::sync::mpsc` channel) so callers get a typed result back while all
//! store access stays serialized on the single actor thread.

use crate::event::CoreEvent;
use std::sync::mpsc::Sender;

/// A request processed by the [`crate::actor`] store-owning thread. Internal — callers use the
/// typed methods on [`crate::Core`].
pub(crate) enum Command {
    /// Liveness probe: emit a `Heartbeat` to subscribers and ack.
    Ping(Sender<()>),
    /// Enumerate the agent session ids currently on the store (the read the UI needs first).
    Sessions(Sender<anyhow::Result<Vec<String>>>),
    /// Every session + its ordered units — what the UI builds its project list from.
    Projects(Sender<anyhow::Result<Vec<crate::SessionView>>>),
    /// A unit's captured work output (transcript), if any.
    WorkOutput(String, Sender<Option<String>>),
    /// Register a live event subscriber.
    Subscribe(Sender<CoreEvent>),
    /// Run a full governed session (fire-and-forget — progress + outcome arrive as `CoreEvent`s,
    /// including `CoreEvent::Error` on failure). Runs on the actor thread (the single writer).
    Launch(crate::LaunchSpec),
}
