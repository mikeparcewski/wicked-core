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
    /// A live chunk of a unit's CLI output, streamed AS the subprocess produces it (P8 live output).
    CliOutputDelta {
        session: String,
        ord: u32,
        chunk: String,
    },
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
    /// The run paused at a human-confirm gate BEFORE the unit with this `ord`. The operator must
    /// `confirm_gate` (approve / reject / cancel) to proceed. `prompt` is the gate question.
    AwaitingHuman {
        session: String,
        ord: u32,
        prompt: String,
    },
    /// A paused run was resumed by a human approval (optionally with an amendment applied).
    Resumed { session: String, ord: u32 },
    /// A run was cancelled (by the operator, or by a rejected gate).
    RunCancelled { session: String },
    /// A run halted as `Failed` at the unit with this `ord` — a governance deny or a worker failure
    /// (the run-level deny contract: never complete past a rejection).
    SessionFailed { session: String, ord: u32 },
    /// A repository was registered into the registry.
    RepoRegistered { repo_ref: String },
    /// The session reached a terminal/awaiting state.
    SessionCompleted { session: String },
    /// Something went wrong (surfaced to the operator rather than swallowed).
    Error {
        session: Option<String>,
        message: String,
    },
}
