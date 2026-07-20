//! The live event stream. Replaces the (largely aspirational) apps-core event catalog with a
//! stream that consumers actually subscribe to — so the UI watches work happen instead of polling
//! the store on a timer.

/// Why a unit step failed (worker-reported failure kind; extensible for future tool / govauth errors).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StepFailureKind {
    /// The CLI worker process itself failed (non-zero exit, crash, or no output).
    WorkerError,
}

/// An event emitted by the core runtime as work progresses. Cheap to clone (fanned out to every
/// subscriber). The taxonomy mirrors the plan → distribute → execute → evidence pipeline; P1 emits
/// only `Heartbeat` (the rest land when the pipeline is lifted in P2).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum CoreEvent {
    /// Liveness tick (also the P1 proof that subscribe→emit works end to end).
    Heartbeat,
    /// A session was created and planning began.
    SessionStarted {
        session: String,
        problem: String,
        workflow_id: Option<String>,
        cli_count: u32,
        governed: bool,
        entity_mode: String,
    },
    /// A work unit was planned (one per decomposed piece).
    UnitPlanned {
        session: String,
        ord: u32,
        description: String,
        stage: String,
        role: String,
        gate: String,
        skill_ref: Option<String>,
        has_validator_pin: bool,
        executor_type: String,
    },
    /// A CLI was assigned to a unit (council, degraded fallback, or tool executor).
    UnitDistributed {
        session: String,
        ord: u32,
        cli: String,
        routing_method: String,
        agreement_pct: Option<u8>,
        returned: Option<u32>,
        dissent: Option<u32>,
        degraded_reason: Option<String>,
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
    /// (DES-STUDIO-COCKPIT-001 §3 B1) The gate's DEPTH alongside `GateDecided`: the criterion gated,
    /// whether the deterministic (layer-1) floor passed, the agent (layer-2) judge's verdict + reasoning
    /// when one ran, the evaluator≠creator second-pass result, and the final `combined` decision
    /// (deny-dominance over ALL layers). Emitted just before `GateDecided`; `GateDecided{allow}` is
    /// retained for back-compat and carries the same bool as `combined`.
    ///
    /// HONESTY (M5): `has_deterministic_floor` is `true` iff a pinned validator gated this unit. When
    /// `false` the phase is UNGATED — nothing deterministic ran — so `criterion` is `None` (the unit
    /// description is NEVER relabeled a "criterion") and `deterministic_pass` is vacuous (there was no
    /// floor to pass). `criterion` is `Some` only when `has_deterministic_floor` (the pinned validator's
    /// criterion).
    ///
    /// HONESTY (S2): `evaluator_pass` surfaces the evaluator≠creator second pass — `Some(false)` when
    /// that layer denied (even though `deterministic_pass == true` and no agent judge ran), `Some(true)`
    /// when it approved, `None` when it did not run. `denial_reason` carries the WINNING denial's reason
    /// whenever `combined == false`, so the record can never read "det pass + agent none + combined
    /// false" with no visible denying layer.
    GateEvaluated {
        session: String,
        ord: u32,
        criterion: Option<String>,
        has_deterministic_floor: bool,
        deterministic_pass: bool,
        agent_verdict: Option<String>,
        agent_reasoning: Option<String>,
        evaluator_pass: Option<bool>,
        denial_reason: Option<String>,
        combined: bool,
    },
    /// (DES-STUDIO-COCKPIT-001 §3 B2) A unit was dispatched to a worker — emitted at EVERY dispatch
    /// (initial + each re-dispatch), so a client sees rework happen. `attempt` increments on re-dispatch;
    /// the FIRST dispatch is `attempt=0`, so `attempt>0` marks rework (a re-dispatch).
    UnitDispatched {
        session: String,
        ord: u32,
        attempt: u32,
    },
    /// (DES-STUDIO-COCKPIT-001 §3 B3) Token/cost burn for one unit run, emitted after the unit completes.
    /// `cost_usd` is `Some` when the CLI reports cost directly (claude) or a price table resolves it, else
    /// `None` (tokens shown without a fabricated dollar figure). Only emitted for seats that report usage.
    CliUsage {
        session: String,
        ord: u32,
        attempt: u32,
        input_tokens: u64,
        output_tokens: u64,
        cost_usd: Option<f64>,
    },
    /// (DES-STUDIO-COCKPIT-001 §3 B4) The data files a unit's CLI touched (from `tool_use` file paths),
    /// emitted after the unit completes when ≥1 file was seen. Absent for seats that report no file access.
    DataUsed {
        session: String,
        ord: u32,
        files: Vec<String>,
    },
    /// A unit finished (approved + output captured).
    UnitDone { session: String, ord: u32 },
    /// A unit was denied (gate veto — never reaches approved).
    UnitDenied { session: String, ord: u32 },
    /// A worker failure halted this unit (run is transitioning to Failed). `detail` is a bounded
    /// excerpt of the worker's output; `failure_kind` names the category for UI dispatch.
    StepFailed {
        session: String,
        ord: u32,
        attempt: u32,
        detail: String,
        failure_kind: StepFailureKind,
    },
    /// The engine restarted while a unit was in-flight and is re-dispatching it. `attempt` is the
    /// NEW (post-bump) attempt number so the UI can show ⚠×N crash-redrive badges.
    CrashRecoveryRedrive {
        session: String,
        ord: u32,
        attempt: u32,
    },
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
    /// A PTY worker session opened for a run unit (the CLI process is now alive and accepting
    /// prompts). `terminal_id` matches the `TerminalOpened` id on the terminal event stream.
    WorkerSessionStarted {
        session: String,
        terminal_id: String,
        cli_key: String,
    },
    /// Something went wrong (surfaced to the operator rather than swallowed).
    Error {
        session: Option<String>,
        message: String,
    },
    // ── PTY terminal sessions (DES-TERMINAL-001) — ride the same single ordered emit point ──
    /// A PTY terminal session opened; its child is running in `cwd`.
    TerminalOpened { id: String, cwd: String },
    /// A chunk of raw PTY output. `bytes_b64` is the raw bytes base64-encoded (CoreEvent → tagged
    /// JSON can't carry a `Vec<u8>` cleanly). `seq` is per-terminal, monotonically increasing —
    /// assigned on the single actor thread so the output stream stays ordered.
    TerminalOutput {
        id: String,
        seq: u64,
        bytes_b64: String,
    },
    /// A PTY terminal session ended (its child exited, or it was closed/reaped). `status` is the
    /// child's exit code when known.
    TerminalExited { id: String, status: Option<i32> },
    // ── Campaign DAG scheduler (DES-CAMPAIGN-001) — ride the same single ordered emit point ──
    /// A campaign was validated + launched; its in-degree-0 nodes are being dispatched.
    CampaignLaunched { campaign: String },
    /// A node's every dependency cleared — it is `Ready` and queued for a concurrency slot.
    CampaignNodeReady { campaign: String, node: String },
    /// A node's Run was dispatched (`dispatch()` is the sole launcher). `run_id` is the node's live
    /// Run — per-node CLI output rides the existing `CliOutputDelta` tagged with this id.
    CampaignNodeStarted {
        campaign: String,
        node: String,
        run_id: String,
    },
    /// A HITL gate opened INSIDE a node's Run: the node is `AwaitingHuman` (its slot is freed so
    /// independent nodes run). The operator resolves it via `confirm_campaign_gate` (Approve/Reject).
    CampaignNodeAwaitingHuman {
        campaign: String,
        node: String,
        run_id: String,
        prompt: String,
    },
    /// A node reached `Completed`.
    CampaignNodeCompleted { campaign: String, node: String },
    /// A node reached `Failed`.
    CampaignNodeFailed { campaign: String, node: String },
    /// A node was `Blocked` — a transitive `OnSuccess` dependency failed (continue-independent).
    CampaignNodeBlocked { campaign: String, node: String },
    /// The campaign paused (human-gate-on-failure, or an operator `PauseCampaign`).
    CampaignPaused { campaign: String },
    /// The campaign finished with no hard failure (`Completed` — all nodes done — or
    /// `PartiallyCompleted` — some blocked/failed under continue-independent). The precise status is
    /// readable via `campaign_status`.
    CampaignCompleted { campaign: String },
    /// The campaign failed (fail-fast tripped, or an aborted human-gate).
    CampaignFailed { campaign: String },
    /// The campaign was cancelled by the operator.
    CampaignCancelled { campaign: String },
}
