//! The live event stream. Replaces the (largely aspirational) apps-core event catalog with a
//! stream that consumers actually subscribe to — so the UI watches work happen instead of polling
//! the store on a timer.

/// Why a unit step failed (worker-reported failure kind; extensible for future tool / govauth errors).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StepFailureKind {
    /// The CLI worker process itself failed (non-zero exit, crash, or no output).
    WorkerError,
    /// The CLI refused its ENVIRONMENT (untrusted directory, missing TTY, folder-trust
    /// prompt) rather than failing the work itself. Emitted when the escalation ladder
    /// engages: the detail names the action taken — an automatic trust-grant retry on
    /// the same CLI, or a pause for the operator's decision.
    EnvironmentRefused,
}

/// One prior unit whose output was injected into a receiving unit's ACP context (EVT-007).
/// Content is intentionally absent — only identity and size are carried to avoid doubling volume.
#[derive(Debug, Clone, PartialEq)]
pub struct InjectedContext {
    /// The prior unit's ord.
    pub ord: u32,
    /// The CLI label used in the prompt block (e.g. `"[codex — unit 2]"`).
    pub label: String,
    /// Byte length of the injected output (for size debugging; not the content itself).
    pub output_bytes: usize,
}

/// An event emitted by the core runtime as work progresses. Cheap to clone (fanned out to every
/// subscriber). The taxonomy mirrors the plan → distribute → execute → evidence pipeline; P1 emits
/// only `Heartbeat` (the rest land when the pipeline is lifted in P2).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum CoreEvent {
    /// Liveness tick (also the P1 proof that subscribe→emit works end to end).
    Heartbeat,
    /// A chat seat's warm ACP session is ready to receive messages (crew#165 / core#13).
    ChatSessionReady { chat: String, cli_key: String },
    /// A chat seat's session could not start (or died); the seat is out of the group
    /// until re-opened. `reason` is operator-facing.
    ChatSessionFailed {
        chat: String,
        cli_key: String,
        reason: String,
    },
    /// A streamed token/delta from one seat's in-progress chat reply.
    ChatDelta {
        chat: String,
        cli_key: String,
        text: String,
    },
    /// One seat's completed reply to a chat message (terminal per message × seat).
    ChatReply {
        chat: String,
        cli_key: String,
        text: String,
        ok: bool,
    },
    /// The chat's warm sessions were closed and their processes reaped.
    ChatClosed {
        chat: String,
        /// Why — `"requested"`, `"idle"`, or `"pool_cap"`
        /// (see [`crate::acp_runner::ChatCloseReason`]).
        ///
        /// Required, not optional: the daemon now closes chats on its own, and a client that saw
        /// only `chat` could not tell a reclaim from an operator's own close — it would report the
        /// reclaim as a chat that disappeared for no reason.
        reason: String,
    },
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
        /// Seats convened for the council that produced this assignment. `returned` on its own
        /// cannot say whether the quorum held, and `agreement_pct` is a ratio over `returned`.
        seated: Option<u32>,
        dissent: Option<u32>,
        degraded_reason: Option<String>,
    },
    /// The council was convened to pick a CLI for a unit (distribution vote started).
    CouncilConvened {
        session: String,
        ord: u32,
        /// Roster keys polled for this vote.
        clis: Vec<String>,
    },
    /// A deliberation ballot landed BELOW the approval bar — the council is running a
    /// runoff round where every seat sees the tally + dissent (governance as conversation).
    CouncilDeliberated {
        session: String,
        ord: u32,
        /// The completed ballot number (1-based).
        round: u32,
        agreement_pct: u8,
        /// The approval bar the council must reach, as a percent.
        needed_pct: u8,
        votes: u32,
    },
    /// One convened seat produced no vote — the named dispatch branch plus what the CLI wrote.
    ///
    /// The council used to collapse ten distinct failure branches into a bare "no vote", so a
    /// 92.6% degradation rate had no signal to diagnose it from. Every no-vote emits one of these.
    CouncilSeatFailed {
        session: String,
        ord: u32,
        /// The ballot the seat failed on (1-based).
        round: u32,
        /// The roster key of the seat.
        cli: String,
        /// Which dispatch branch was taken (`spawn_failed`, `non_zero_exit`, `timed_out`, …).
        kind: String,
        /// The process exit code, when the process ran to completion.
        exit_code: Option<i32>,
        /// Captured stderr, truncated. The artifact `run_in_isolation` used to discard.
        stderr: String,
        /// The OS/IO error text, where the branch has one.
        detail: String,
        /// How long the seat ran before failing. A spawn error costs ~0 ms; a timeout costs the
        /// whole budget. Without this the two are indistinguishable in the event stream.
        latency_ms: u64,
    },
    /// The council reached a verdict for a unit's assignment vote.
    CouncilVoted {
        session: String,
        ord: u32,
        consensus: bool,
        agreement_pct: u8,
        votes: u32,
        /// Seats convened. `votes` alone cannot distinguish a unanimous council from the one
        /// survivor of a collapsed one, and `agreement_pct` is computed over `votes`.
        ///
        /// `None` (wire `null`) means the emitter did not report one — the same "unknown" that
        /// [`CoreEvent::UnitDistributed`] carries, so a consumer has ONE rule for the field rather
        /// than a sentinel `0` on one event and a null on the other. Never inferred from `votes`:
        /// that would report every collapsed council as a complete one, which is the defect this
        /// field exists to expose (FINDING-026 D; review on #151).
        seated: Option<u32>,
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
    ///
    /// HONESTY (FINDING-025): `evaluator_policies` lists the policy ids that were APPLICABLE to the
    /// unit's eval phase — the second pass's SELECTION, not the subset whose triggers fired. It is
    /// the layer-3 analogue of `has_deterministic_floor`: that flag exists so a vacuously-true
    /// `deterministic_pass` is not misread as an enforced pass, and `evaluator_pass` has exactly the
    /// same failure mode. The policy engine runs on every unit and default-allows on an EMPTY
    /// SELECTION (no policy declared `applies_to` this phase), so `Some(true)` alone cannot
    /// distinguish "a policy examined this unit and approved it" from "no policy applied at all".
    /// The distinction is selection, not triggering: a selected policy whose trigger found nothing
    /// to deny DID examine the unit and is listed — that allow is genuine enforcement. An EMPTY list
    /// with `evaluator_pass == Some(true)` is the other case — the unit was effectively UNGATED by
    /// this layer, and no consumer may present it as governed.
    GateEvaluated {
        session: String,
        ord: u32,
        criterion: Option<String>,
        has_deterministic_floor: bool,
        deterministic_pass: bool,
        agent_verdict: Option<String>,
        agent_reasoning: Option<String>,
        evaluator_pass: Option<bool>,
        /// Policy ids applied by the evaluator second pass. Empty ⇒ default-allow (see HONESTY above).
        evaluator_policies: Vec<String>,
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
    /// A live PTY worker has produced NO output for `stalled_secs` while its turn is
    /// still open — it may be sitting at an interactive prompt the sentinel parser
    /// cannot answer. Emitted once per turn; the operator can inspect via the terminal
    /// surface or inject a response.
    WorkerStalled {
        session: String,
        ord: u32,
        terminal_id: String,
        stalled_secs: u64,
    },
    /// A structured assumption parsed from a completed unit's output — currently the
    /// external-transform convention: a third-party library/service transforms a payload.
    /// `known=false` marks a needs-research placeholder a human should review.
    AssumptionRecorded {
        session: String,
        ord: u32,
        kind: String,
        library: String,
        transform: String,
        known: bool,
        detail: String,
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
    /// The agent triage judge decided the remedy for an UNRECOGNIZED worker failure
    /// (the generalization of the static environment-refusal table): `decision` is one of
    /// `retry_with_flag` / `retry` / `escalate` / `fail`; `analysis` is the judge's
    /// bounded reasoning.
    FailureTriaged {
        session: String,
        ord: u32,
        decision: String,
        analysis: String,
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
    ///
    /// `reviewing_ord` names the unit whose OUTPUT the human is being asked to judge, which is
    /// usually not `ord`. A DEF-declared gate fires *after* its phase's work, so the preceding
    /// phase demanded the pause and its output is the artifact under review; a triage, refusal, or
    /// not-pass-verdict escalation is about the failed unit's own output. `None` means nothing has
    /// been produced to review — the run-level `--confirm` policy paused before `ord` ran.
    ///
    /// Without it the log cannot answer "why did this run pause here, and on what?" — recovering
    /// that took re-reading the workflow def and re-deriving `should_pause` by hand (FINDING-032).
    /// For a system that re-derives "done" from evidence, a governance pause has to describe itself.
    AwaitingHuman {
        session: String,
        ord: u32,
        reviewing_ord: Option<u32>,
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
    /// A PTY worker session opened for a run (keyed per `run_id`, not per unit — one PTY
    /// session spans the whole run). The CLI process is now alive and accepting prompts.
    /// `terminal_id` matches the `TerminalOpened` id on the terminal event stream.
    WorkerSessionStarted {
        session: String,
        terminal_id: String,
        cli_key: String,
    },
    /// An ACP (Agent Client Protocol) persistent session opened for a `(run_id, cli_key)` pair.
    /// `acp_session_id` is the session identifier the ACP binary assigned during the handshake.
    AcpSessionStarted {
        session: String,
        cli_key: String,
        acp_session_id: String,
    },
    /// The ACP runner fell back to single-shot wrapped-CLI execution for this unit.
    /// `reason` is the human-readable warning already prepended to step output; `fallback_kind`
    /// is a stable slug for UI dispatch (see acp_runner constants).
    AcpFallback {
        session: String,
        cli_key: String,
        reason: String,
        fallback_kind: String,
    },
    /// (EVT-003) A PTY-based persistent worker session is being REUSED for a subsequent unit
    /// in the same run — the session was already open and the prompt will be written into it.
    /// Fires before the prompt write. Confirms the prompt-cache sharing invariant; absence for a
    /// multi-unit run means every unit paid cold-start cost.
    WorkerSessionReused {
        session: String,
        /// The existing PTY terminal id being reused.
        terminal_id: String,
        /// The unit ord this prompt is being submitted for.
        ord: u32,
    },
    /// (EVT-004) A PTY-based persistent worker session was explicitly closed. `reason` is one of:
    /// - `"run_complete"` — normal end-of-run teardown via `on_run_complete`
    /// - `"error"` — the PTY write failed and the stale session is being dropped
    /// - `"reassigned"` — the unit was reassigned to a different CLI; the old session is closed
    ///   before the new one is opened for the re-dispatched unit
    ///
    /// Paired with `WorkerSessionStarted` and `WorkerSessionReused` to form the full session lifecycle.
    WorkerSessionClosed {
        session: String,
        /// The PTY terminal id that was closed.
        terminal_id: String,
        /// Why the session was closed: `"run_complete"`, `"error"`, or `"reassigned"`.
        reason: String,
    },
    /// (EVT-007) One or more prior unit outputs were injected into the current unit's ACP context
    /// before dispatch. Only fires when `prior_units` is non-empty; fires before `UnitExecuting` for
    /// the receiving unit.
    ///
    /// Two independent reasons put a prior in this list, and the `label` says which
    /// (`actor::prior_context_label`): the receiving unit's `depends_on` DECLARED the
    /// prior's phase, or the prior ran on a different CLI (the original multi-CLI Tutti-inspired
    /// sharing path). Before FINDING-024 only the second reason existed, so this event **never
    /// fired on a single-CLI run** — every shipped workflow's default. Its absence across the whole
    /// P3 campaign is what proved those phases executed context-free, so treat a run with declared
    /// dependencies and no EVT-007 as a regression signal, not as a quiet success.
    UnitContextInjected {
        session: String,
        /// The unit receiving the injected context.
        ord: u32,
        /// The CLI key of the receiving unit.
        recipient_cli: String,
        /// The prior units whose outputs were injected (by ord + label + byte size).
        prior_units: Vec<InjectedContext>,
    },
    /// (EVT-008) One governance hook decision replayed from the NDJSON decisions log, collected
    /// by `gate_hook::collect_hook_decisions` and emitted from `pipeline::apply_and_finish_unit`
    /// after `fold_input_denial` returns. One event per tool-call decision entry in the log.
    /// Fires at gate time (post-fold), not in real-time during execution. `tool_name` is the
    /// tool the hook intercepted (e.g. `"Bash"`, `"Edit"`); `decision` is `"allow"`,
    /// `"allow_with_conditions"`, or `"deny"`; `denying_policy` is the first policy id that
    /// denied, when `decision == "deny"`.
    GovernanceHookFired {
        session: String,
        ord: u32,
        attempt: u32,
        tool_name: String,
        decision: String,
        denying_policy: Option<String>,
    },
    /// (EVT-009) A pinned, approved deterministic validator was successfully loaded from the vault
    /// and attached to a unit during `attach_pinned_validators`. Fires at plan time (before the
    /// unit runs). `pin` is the content-hash pin; `criterion` is the validator's human-readable
    /// acceptance criterion.
    ValidationPinAttached {
        session: String,
        ord: u32,
        pin: String,
        criterion: String,
    },
    /// (EVT-010) A `HumanConfirmIf(VerdictNotPass)` gate fired — the unit's verdict was not-pass
    /// and the run was escalated to human review instead of being auto-denied. Fires alongside
    /// `AwaitingHuman` to identify the escalation type. `condition` is currently always
    /// `"verdict_not_pass"`; `verdict_summary` is the denial reason that triggered escalation.
    GateEscalated {
        session: String,
        ord: u32,
        condition: String,
        verdict_summary: String,
    },
    /// (EVT-011) A unit's work was dispatched via the `PhaseExecutor::Tool` path (a direct
    /// subprocess command, bypassing the council and CLI runner entirely). Fires just before
    /// the tool command spawns. `cmd` is the full command including the binary; `workdir` is
    /// the working directory (the session workdir, or `None` if unset).
    ToolExecutorDispatched {
        session: String,
        ord: u32,
        cmd: Vec<String>,
        workdir: Option<String>,
    },
    /// (EVT-016) Input governance was successfully armed for a unit — the gate-hook settings file
    /// was written, the ARMED marker was written to the decisions log, and the governed CLI
    /// invocation is about to start. `path` is `"wrapped_cli"` (the `--settings`-injection path)
    /// or `"acp"` (the ACP stdio-mode governed session path). `db_path` is the estate store the
    /// gate-hook subprocess will open read-only to evaluate tool calls.
    GovernanceContextArmed {
        session: String,
        ord: u32,
        attempt: u32,
        path: String,
        db_path: String,
    },
    /// (EVT-017) A unit that the workflow declared GOVERNED ran with input governance UNENFORCED,
    /// because the CLI it was routed to has no gate-hook adapter. Input arming is claude-only (it
    /// works by injecting a PreToolUse hook via `--settings`), so any governed unit the router
    /// sends elsewhere executes its tool calls unchecked.
    ///
    /// This is not a fallback and nothing retries: the unit runs, and `UnitOutputCaptured` reports
    /// `governed: false`. That bare `false` is indistinguishable from an ungoverned-by-design unit,
    /// which is why this event exists — it names the CLI and the reason, so an evidence packet can
    /// tell "no governance was asked for" apart from "governance was asked for and could not be
    /// applied". Measured on `pilot-migration-001`: the `evaluator_distinct` router moved ord 4 off
    /// claude to `agy`, and that unit produced no ARMED marker, no hook firings, and no claims,
    /// while units 1–3 armed normally (FINDING-063).
    ///
    /// The routing interaction is the sharp edge: `evaluator_distinct` exists to keep the evaluator
    /// off the creator's CLI, so on a claude-creator run it *necessarily* selects a CLI that cannot
    /// be governed. The unit whose job is to judge the work independently is the one structurally
    /// guaranteed to run unchecked.
    GovernanceUnenforced {
        session: String,
        ord: u32,
        attempt: u32,
        /// The binary the unit was actually routed to (argv[0]), not the seat key.
        cli: String,
        reason: String,
    },
    /// (EVT-001) A structured workflow def was selected for this session — the authoritative
    /// decomposition signal. Fires once per session, after `SessionStarted` and before the first
    /// `UnitPlanned`. Only emitted when a `--workflow` id was resolved (not for free-text runs).
    /// `unit_count` is the number of phases the def decomposed into.
    WorkflowSelected {
        session: String,
        workflow_id: String,
        unit_count: u32,
    },
    /// (EVT-012) A human approved a gate AND supplied an amendment text that was injected into
    /// the unit's description before re-dispatch. Fires after the amendment is persisted to the
    /// store, before `Resumed`. Only emitted when the amendment text is non-empty. This is the
    /// authoritative record for the human-in-the-loop paper trail: `Resumed` alone carries no
    /// amendment text.
    UnitReworkAmended {
        session: String,
        ord: u32,
        /// The raw amendment text supplied by the operator.
        amendment: String,
        /// The unit's description after the amendment was injected.
        updated_description: String,
    },
    /// (EVT-013) A worker's `ApplyStepResult` arrived and the output is ready to be gated. Fires
    /// after all terminal/idempotency/attempt guards pass, before the gate runs. `output_bytes` is
    /// the byte length of the worker's output — lets an operator immediately distinguish "0 bytes"
    /// from "8 MB that was truncated by MAX_OUT". `step_status` is `"ok"`, `"failed"`, or
    /// `"cancelled"`. `governed` reflects whether the runner armed input governance for this unit.
    UnitOutputCaptured {
        session: String,
        ord: u32,
        attempt: u32,
        output_bytes: usize,
        step_status: String,
        governed: bool,
    },
    /// An operator message was delivered — written into a live PTY worker, or (ACP runs)
    /// appended to a unit's prompt at dispatch after having been queued.
    WorkerMessageInjected {
        session: String,
        message: String,
        /// `"all"` or the cli_key that was targeted.
        target: String,
    },
    /// An operator message had no live PTY to write to (ACP-backed run) and was QUEUED —
    /// it rides the next matching unit's prompt as an operator context block.
    WorkerMessageQueued {
        session: String,
        message: String,
        /// `"all"` or the cli_key that was targeted.
        target: String,
    },
    /// A unit was stopped and re-dispatched to a different CLI (or re-routed via council).
    UnitReassigned {
        session: String,
        ord: u32,
        attempt: u32,
        previous_cli: String,
        /// `None` means the council was re-convened and its choice is the new assignment.
        new_cli: Option<String>,
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

impl CoreEvent {
    /// Serialize to the tagged JSON object (`{ "type": "...", ...fields }`) that IS this event's
    /// wire identity — the shape the studio's `/ws` stream carries and the shape the durable event
    /// log ([`crate::event_log`]) records. `CoreEvent` is deliberately not `serde::Serialize`
    /// (variant names are snake_cased and several fields are reshaped for JS), so every variant is
    /// mapped by hand.
    ///
    /// One mapping, two consumers, on purpose (FINDING-014). This previously lived PRIVATE inside the
    /// napi bridge, so core had no way to name its own events — which is how the daemon came to invent
    /// its own vocabulary (`routingDecided`) for evidence it re-derived from unit records instead of
    /// reading a real event trail. Anything that needs to name a `CoreEvent` now goes through here, so
    /// the log, the socket, and the evidence bundle cannot drift apart.
    ///
    /// The object this returns is also what the log routes on. [`crate::event_log::run_key`] picks
    /// the per-run file by reading `session` — or `runId`, for campaign node events — straight out
    /// of it, rather than re-matching the enum: a second hand-written per-variant match would be a
    /// second thing to forget to update. (`type` names the event; it is not the routing key.)
    ///
    /// `runId`, not `run_id`: the emitted JSON is camelCase throughout, and an earlier cut of
    /// `run_key` looked for a key this function emits nowhere.
    ///
    /// The match is EXHAUSTIVE and must stay that way. `CoreEvent` is `#[non_exhaustive]`, so while
    /// this lived outside the crate it needed catch-all arms — and a variant nobody had mapped
    /// serialized as `{"type":"unknown"}`, a silent hole in the very trail this is the evidence for.
    /// In the defining crate `non_exhaustive` does not apply, so those arms are dead and have been
    /// removed: adding a variant without mapping it is now a BUILD failure, not a mystery event.
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::json;
        match self {
            CoreEvent::Heartbeat => json!({ "type": "heartbeat" }),
            CoreEvent::SessionStarted {
                session,
                problem,
                workflow_id,
                cli_count,
                governed,
                entity_mode,
            } => {
                json!({
                    "type": "sessionStarted",
                    "session": session,
                    "problem": problem,
                    "workflowId": workflow_id,
                    "cliCount": cli_count,
                    "governed": governed,
                    "entityMode": entity_mode,
                })
            }
            CoreEvent::UnitPlanned {
                session,
                ord,
                description,
                stage,
                role,
                gate,
                skill_ref,
                has_validator_pin,
                executor_type,
            } => json!({
                "type": "unitPlanned",
                "session": session,
                "ord": ord,
                "description": description,
                "stage": stage,
                "role": role,
                "gate": gate,
                "skillRef": skill_ref,
                "hasValidatorPin": has_validator_pin,
                "executorType": executor_type,
            }),
            CoreEvent::UnitDistributed {
                session,
                ord,
                cli,
                routing_method,
                agreement_pct,
                returned,
                seated,
                dissent,
                degraded_reason,
            } => {
                json!({
                    "type": "unitDistributed",
                    "session": session,
                    "ord": ord,
                    "cli": cli,
                    "routingMethod": routing_method,
                    "agreementPct": agreement_pct,
                    "returned": returned,
                    "seated": seated,
                    "dissent": dissent,
                    "degradedReason": degraded_reason,
                })
            }
            CoreEvent::CouncilConvened { session, ord, clis } => json!({
                "type": "councilConvened",
                "session": session,
                "ord": ord,
                "clis": clis,
            }),
            CoreEvent::CouncilDeliberated {
                session,
                ord,
                round,
                agreement_pct,
                needed_pct,
                votes,
            } => json!({
                "type": "councilDeliberated",
                "session": session,
                "ord": ord,
                "round": round,
                "agreementPct": agreement_pct,
                "neededPct": needed_pct,
                "votes": votes,
            }),
            CoreEvent::CouncilSeatFailed {
                session,
                ord,
                round,
                cli,
                kind,
                exit_code,
                stderr,
                detail,
                latency_ms,
            } => json!({
                "type": "councilSeatFailed",
                "session": session,
                "ord": ord,
                "round": round,
                "cli": cli,
                "kind": kind,
                "exitCode": exit_code,
                "stderr": stderr,
                "detail": detail,
                "latencyMs": latency_ms,
            }),
            CoreEvent::CouncilVoted {
                session,
                ord,
                consensus,
                agreement_pct,
                votes,
                seated,
            } => json!({
                "type": "councilVoted",
                "session": session,
                "ord": ord,
                "consensus": consensus,
                "agreementPct": agreement_pct,
                "votes": votes,
                "seated": seated,
            }),
            CoreEvent::UnitExecuting { session, ord } => {
                json!({ "type": "unitExecuting", "session": session, "ord": ord })
            }
            CoreEvent::CliOutputDelta {
                session,
                ord,
                chunk,
            } => {
                json!({ "type": "cliOutputDelta", "session": session, "ord": ord, "chunk": chunk })
            }
            CoreEvent::GateDecided {
                session,
                ord,
                allow,
            } => json!({ "type": "gateDecided", "session": session, "ord": ord, "allow": allow }),
            // (DES-STUDIO-COCKPIT-001 §3 B1) The gate's DEPTH alongside `gateDecided`. camelCase fields;
            // `criterion`/`agentVerdict`/`agentReasoning`/`denialReason` are nullable (Option → null),
            // `evaluatorPass` is a nullable bool (`None` = the evaluator≠creator pass did not run).
            // `evaluatorPolicies` is the applicable-policy id list; EMPTY alongside `evaluatorPass: true`
            // means nothing applied — a default-allow, not an enforced pass (FINDING-025).
            CoreEvent::GateEvaluated {
                session,
                ord,
                criterion,
                has_deterministic_floor,
                deterministic_pass,
                agent_verdict,
                agent_reasoning,
                evaluator_pass,
                evaluator_policies,
                denial_reason,
                combined,
            } => json!({
                "type": "gateEvaluated",
                "session": session,
                "ord": ord,
                "criterion": criterion,
                "hasDeterministicFloor": has_deterministic_floor,
                "deterministicPass": deterministic_pass,
                "agentVerdict": agent_verdict,
                "agentReasoning": agent_reasoning,
                "evaluatorPass": evaluator_pass,
                "evaluatorPolicies": evaluator_policies,
                "denialReason": denial_reason,
                "combined": combined,
            }),
            // (DES-STUDIO-COCKPIT-001 §3 B2) Durable-rework signal — emitted at every dispatch; `attempt>0`
            // marks a re-dispatch.
            CoreEvent::UnitDispatched {
                session,
                ord,
                attempt,
            } => {
                json!({ "type": "unitDispatched", "session": session, "ord": ord, "attempt": attempt })
            }
            // (DES-STUDIO-COCKPIT-001 §3 B3) Token/cost burn for one unit run. `costUsd` is nullable
            // (`None` → null when the CLI reports no cost and no price table resolves it).
            CoreEvent::CliUsage {
                session,
                ord,
                attempt,
                input_tokens,
                output_tokens,
                cost_usd,
            } => json!({
                "type": "cliUsage",
                "session": session,
                "ord": ord,
                "attempt": attempt,
                "inputTokens": input_tokens,
                "outputTokens": output_tokens,
                "costUsd": cost_usd,
            }),
            // (DES-STUDIO-COCKPIT-001 §3 B4) The data files a unit's CLI touched.
            CoreEvent::DataUsed {
                session,
                ord,
                files,
            } => json!({ "type": "dataUsed", "session": session, "ord": ord, "files": files }),
            CoreEvent::UnitDone { session, ord } => {
                json!({ "type": "unitDone", "session": session, "ord": ord })
            }
            CoreEvent::UnitDenied { session, ord } => {
                json!({ "type": "unitDenied", "session": session, "ord": ord })
            }
            CoreEvent::AwaitingHuman {
                session,
                ord,
                reviewing_ord,
                prompt,
            } => {
                json!({ "type": "awaitingHuman", "session": session, "ord": ord, "reviewingOrd": reviewing_ord, "prompt": prompt })
            }
            CoreEvent::Resumed { session, ord } => {
                json!({ "type": "resumed", "session": session, "ord": ord })
            }
            CoreEvent::RunCancelled { session } => {
                json!({ "type": "runCancelled", "session": session })
            }
            CoreEvent::SessionFailed { session, ord } => {
                json!({ "type": "sessionFailed", "session": session, "ord": ord })
            }
            CoreEvent::RepoRegistered { repo_ref } => {
                json!({ "type": "repoRegistered", "repoRef": repo_ref })
            }
            CoreEvent::SessionCompleted { session } => {
                json!({ "type": "sessionCompleted", "session": session })
            }
            CoreEvent::WorkerMessageQueued {
                session,
                message,
                target,
            } => json!({
                "type": "workerMessageQueued",
                "session": session,
                "message": message,
                "target": target,
            }),
            CoreEvent::WorkerMessageInjected {
                session,
                message,
                target,
            } => json!({
                "type": "workerMessageInjected",
                "session": session,
                "message": message,
                "target": target,
            }),
            CoreEvent::UnitReassigned {
                session,
                ord,
                attempt,
                previous_cli,
                new_cli,
            } => json!({
                "type": "unitReassigned",
                "session": session,
                "ord": ord,
                "attempt": attempt,
                "previousCli": previous_cli,
                "newCli": new_cli,
            }),
            CoreEvent::Error { session, message } => {
                json!({ "type": "error", "session": session, "message": message })
            }
            // PTY terminal sessions (DES-TERMINAL-001). Mapped minimally to keep this exhaustive match
            // compiling now that core carries the terminal capability; the full TS surface (openTerminal
            // etc.) is a separate follow-on task.
            CoreEvent::TerminalOpened { id, cwd } => {
                json!({ "type": "terminalOpened", "id": id, "cwd": cwd })
            }
            CoreEvent::TerminalOutput { id, seq, bytes_b64 } => {
                json!({ "type": "terminalOutput", "id": id, "seq": seq, "bytesB64": bytes_b64 })
            }
            CoreEvent::TerminalExited { id, status } => {
                json!({ "type": "terminalExited", "id": id, "status": status })
            }
            // Campaign DAG scheduler (DES-CAMPAIGN-001). Additive tagged-JSON mappings — the studio
            // ignores unknown event types, so these never disturb existing consumers. The full campaign
            // binding surface (launchCampaign etc.) is a separate follow-on task.
            CoreEvent::CampaignLaunched { campaign } => {
                json!({ "type": "campaignLaunched", "campaign": campaign })
            }
            CoreEvent::CampaignNodeReady { campaign, node } => {
                json!({ "type": "campaignNodeReady", "campaign": campaign, "node": node })
            }
            CoreEvent::CampaignNodeStarted {
                campaign,
                node,
                run_id,
            } => {
                json!({ "type": "campaignNodeStarted", "campaign": campaign, "node": node, "runId": run_id })
            }
            CoreEvent::CampaignNodeAwaitingHuman {
                campaign,
                node,
                run_id,
                prompt,
            } => {
                json!({ "type": "campaignNodeAwaitingHuman", "campaign": campaign, "node": node, "runId": run_id, "prompt": prompt })
            }
            CoreEvent::CampaignNodeCompleted { campaign, node } => {
                json!({ "type": "campaignNodeCompleted", "campaign": campaign, "node": node })
            }
            CoreEvent::CampaignNodeFailed { campaign, node } => {
                json!({ "type": "campaignNodeFailed", "campaign": campaign, "node": node })
            }
            CoreEvent::CampaignNodeBlocked { campaign, node } => {
                json!({ "type": "campaignNodeBlocked", "campaign": campaign, "node": node })
            }
            CoreEvent::CampaignPaused { campaign } => {
                json!({ "type": "campaignPaused", "campaign": campaign })
            }
            CoreEvent::CampaignCompleted { campaign } => {
                json!({ "type": "campaignCompleted", "campaign": campaign })
            }
            CoreEvent::CampaignFailed { campaign } => {
                json!({ "type": "campaignFailed", "campaign": campaign })
            }
            CoreEvent::CampaignCancelled { campaign } => {
                json!({ "type": "campaignCancelled", "campaign": campaign })
            }
            // P1 observability events — worker failure + crash recovery.
            CoreEvent::StepFailed {
                session,
                ord,
                attempt,
                detail,
                failure_kind,
            } => json!({
                "type": "stepFailed",
                "session": session,
                "ord": ord,
                "attempt": attempt,
                "detail": detail,
                "failureKind": match failure_kind {
                    StepFailureKind::WorkerError => "workerError",
                    StepFailureKind::EnvironmentRefused => "environmentRefused",
                },
            }),
            CoreEvent::WorkerStalled {
                session,
                ord,
                terminal_id,
                stalled_secs,
            } => json!({
                "type": "workerStalled",
                "session": session,
                "ord": ord,
                "terminalId": terminal_id,
                "stalledSecs": stalled_secs,
            }),
            CoreEvent::AssumptionRecorded {
                session,
                ord,
                kind,
                library,
                transform,
                known,
                detail,
            } => json!({
                "type": "assumptionRecorded",
                "session": session,
                "ord": ord,
                "kind": kind,
                "library": library,
                "transform": transform,
                "known": known,
                "detail": detail,
            }),
            CoreEvent::FailureTriaged {
                session,
                ord,
                decision,
                analysis,
            } => json!({
                "type": "failureTriaged",
                "session": session,
                "ord": ord,
                "decision": decision,
                "analysis": analysis,
            }),
            CoreEvent::CrashRecoveryRedrive {
                session,
                ord,
                attempt,
            } => json!({
                "type": "crashRecoveryRedrive",
                "session": session,
                "ord": ord,
                "attempt": attempt,
            }),
            CoreEvent::WorkerSessionStarted {
                session,
                terminal_id,
                cli_key,
            } => json!({
                "type": "workerSessionStarted",
                "session": session,
                "terminalId": terminal_id,
                "cliKey": cli_key,
            }),
            CoreEvent::AcpSessionStarted {
                session,
                cli_key,
                acp_session_id,
            } => json!({
                "type": "acpSessionStarted",
                "session": session,
                "cliKey": cli_key,
                "acpSessionId": acp_session_id,
            }),
            CoreEvent::AcpFallback {
                session,
                cli_key,
                reason,
                fallback_kind,
            } => json!({
                "type": "acpFallback",
                "session": session,
                "cliKey": cli_key,
                "reason": reason,
                "fallbackKind": fallback_kind,
            }),
            // P2 observability events — worker-lifecycle wave (EVT-003, EVT-004, EVT-007).
            CoreEvent::WorkerSessionReused {
                session,
                terminal_id,
                ord,
            } => json!({
                "type": "workerSessionReused",
                "session": session,
                "terminalId": terminal_id,
                "ord": ord,
            }),
            CoreEvent::WorkerSessionClosed {
                session,
                terminal_id,
                reason,
            } => json!({
                "type": "workerSessionClosed",
                "session": session,
                "terminalId": terminal_id,
                "reason": reason,
            }),
            CoreEvent::UnitContextInjected {
                session,
                ord,
                recipient_cli,
                prior_units,
            } => json!({
                "type": "unitContextInjected",
                "session": session,
                "ord": ord,
                "recipientCli": recipient_cli,
                "priorUnits": prior_units.iter().map(|c| json!({
                    "ord": c.ord,
                    "label": c.label,
                    "outputBytes": c.output_bytes,
                })).collect::<Vec<_>>(),
            }),
            // ── P2 governance-deep wave (EVT-008, EVT-009, EVT-010, EVT-011, EVT-016) ──────────
            CoreEvent::GovernanceHookFired {
                session,
                ord,
                attempt,
                tool_name,
                decision,
                denying_policy,
            } => json!({
                "type": "governanceHookFired",
                "session": session,
                "ord": ord,
                "attempt": attempt,
                "toolName": tool_name,
                "decision": decision,
                "denyingPolicy": denying_policy,
            }),
            CoreEvent::ValidationPinAttached {
                session,
                ord,
                pin,
                criterion,
            } => json!({
                "type": "validationPinAttached",
                "session": session,
                "ord": ord,
                "pin": pin,
                "criterion": criterion,
            }),
            CoreEvent::GateEscalated {
                session,
                ord,
                condition,
                verdict_summary,
            } => json!({
                "type": "gateEscalated",
                "session": session,
                "ord": ord,
                "condition": condition,
                "verdictSummary": verdict_summary,
            }),
            CoreEvent::ToolExecutorDispatched {
                session,
                ord,
                cmd,
                workdir,
            } => json!({
                "type": "toolExecutorDispatched",
                "session": session,
                "ord": ord,
                "cmd": cmd,
                "workdir": workdir,
            }),
            CoreEvent::GovernanceContextArmed {
                session,
                ord,
                attempt,
                path,
                db_path,
            } => json!({
                "type": "governanceContextArmed",
                "session": session,
                "ord": ord,
                "attempt": attempt,
                "path": path,
                "dbPath": db_path,
            }),
            CoreEvent::GovernanceUnenforced {
                session,
                ord,
                attempt,
                cli,
                reason,
            } => json!({
                "type": "governanceUnenforced",
                "session": session,
                "ord": ord,
                "attempt": attempt,
                "cli": cli,
                "reason": reason,
            }),
            // P2 decisions-full wave (EVT-001, EVT-012, EVT-013).
            CoreEvent::WorkflowSelected {
                session,
                workflow_id,
                unit_count,
            } => json!({
                "type": "workflowSelected",
                "session": session,
                "workflowId": workflow_id,
                "unitCount": unit_count,
            }),
            CoreEvent::UnitReworkAmended {
                session,
                ord,
                amendment,
                updated_description,
            } => json!({
                "type": "unitReworkAmended",
                "session": session,
                "ord": ord,
                "amendment": amendment,
                "updatedDescription": updated_description,
            }),
            CoreEvent::UnitOutputCaptured {
                session,
                ord,
                attempt,
                output_bytes,
                step_status,
                governed,
            } => json!({
                "type": "unitOutputCaptured",
                "session": session,
                "ord": ord,
                "attempt": attempt,
                "outputBytes": output_bytes,
                "stepStatus": step_status,
                "governed": governed,
            }),
            // This mapping used to live in the napi crate, OUTSIDE the defining one, where
            // `#[non_exhaustive]` forced a `_` arm and an unmapped variant surfaced as a benign
            // `{"type":"unknown"}` frame. In here that arm is dead code and has been removed: a new
            // variant without an arm is a BUILD failure, not a silent hole in the audit trail.
            CoreEvent::ChatSessionReady { chat, cli_key } => {
                json!({ "type": "chatSessionReady", "chat": chat, "cliKey": cli_key })
            }
            CoreEvent::ChatSessionFailed {
                chat,
                cli_key,
                reason,
            } => {
                json!({ "type": "chatSessionFailed", "chat": chat, "cliKey": cli_key, "reason": reason })
            }
            CoreEvent::ChatDelta {
                chat,
                cli_key,
                text,
            } => {
                json!({ "type": "chatDelta", "chat": chat, "cliKey": cli_key, "text": text })
            }
            CoreEvent::ChatReply {
                chat,
                cli_key,
                text,
                ok,
            } => {
                json!({ "type": "chatReply", "chat": chat, "cliKey": cli_key, "text": text, "ok": ok })
            }
            CoreEvent::ChatClosed { chat, reason } => {
                json!({ "type": "chatClosed", "chat": chat, "reason": reason })
            }
        }
    }
}
