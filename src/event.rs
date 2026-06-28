//! The live event stream. Replaces the (largely aspirational) apps-core event catalog with a
//! stream that consumers actually subscribe to — so the UI watches work happen instead of polling
//! the store on a timer.

/// An event emitted by the core runtime as work progresses. Cheap to clone (fanned out to every
/// subscriber). The taxonomy mirrors the plan → distribute → execute → evidence pipeline; P1 emits
/// only `Heartbeat` (the rest land when the pipeline is lifted in P2).
#[derive(Debug, Clone, PartialEq)]
pub enum CoreEvent {
    /// Liveness tick (also the P1 proof that subscribe→emit works end to end).
    Heartbeat,
    /// A session was created and planning began.
    SessionStarted { session: String, problem: String },
    /// A work unit was planned (one per decomposed piece).
    UnitPlanned {
        session: String,
        ord: u32,
        description: String,
    },
    /// The council assigned a CLI to a unit.
    UnitDistributed {
        session: String,
        ord: u32,
        cli: String,
    },
    /// A unit's execution started.
    UnitExecuting { session: String, ord: u32 },
    /// The governance gate decided for a unit (`allow=false` means a structural veto).
    GateDecided {
        session: String,
        ord: u32,
        allow: bool,
    },
    /// A unit finished (approved + output captured).
    UnitDone { session: String, ord: u32 },
    /// A unit was denied (gate veto — never reaches approved).
    UnitDenied { session: String, ord: u32 },
    /// The session reached a terminal/awaiting state.
    SessionCompleted { session: String },
    /// Something went wrong (surfaced to the operator rather than swallowed).
    Error {
        session: Option<String>,
        message: String,
    },
}
