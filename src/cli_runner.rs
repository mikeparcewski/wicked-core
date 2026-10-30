//! The Law 1 EXECUTION-MEDIATION SEAM (DES-EXEC-001 §2.3, §5) — the one edge that must decouple: the
//! reducer (actor) no longer calls execution directly, it PUBLISHES `wicked.task.dispatched`; a
//! `cli-runner` SUBSCRIBER consumes it, runs the unit's work OFF the actor via the *same* [`StepRunner`]
//! seam, and PUBLISHES `wicked.task.completed` back; the actor consumes that and folds it into the SAME
//! `apply_step_result` it already runs. This makes "the actor no longer calls execution directly" REAL
//! for the execution seam (Law 1 already held for the launch trigger — `run.requested` → `LaunchRun`).
//!
//! ## Opt-in — the default in-process path is byte-for-byte untouched
//! The whole seam is gated on [`is_exec_enabled`] (set from `WICKED_BUS_EXEC` + `WICKED_BUS_DB`, or the
//! explicit `Core::spawn_with_engine_exec` test entry). When OFF (the default), [`dispatch_unit`] spawns
//! the in-process worker exactly as before and NONE of this module runs. When ON, `dispatch_unit`
//! publishes `task.dispatched` instead of spawning, and two dedicated OFF-ACTOR threads carry the work.
//!
//! ## Actor-safety (the load-bearing invariant — same posture as the launch bridge)
//!  * The `cli-runner` subscriber and the `task.completed` poller each run on their OWN `std::thread`
//!    and use the process-wide shared bus handle ([`BusDb::shared`], one connection per bus file per
//!    process; the bus is a different file from the estate store the actor owns — no writer-lock
//!    contention). They are actor-safe because they run off the actor thread. Neither holds a store
//!    handle: the `cli-runner` reads only the dispatched event + publishes the result; the actor stays
//!    the ONLY writer.
//!  * The actor reaches nothing here by a blocking poll. It only *publishes* `task.dispatched`, a single
//!    bounded local INSERT (`BusDb::emit_bounded`) through a clone of that shared handle held
//!    actor-thread-locally — opened by a bus thread, never by the actor — the reducer's publish
//!    role (§2.3), analogous to the actor's own store writes, never an unbounded poll or a CLI call.
//!  * The `task.completed` poller reaches the actor ONLY by sending `Command::ApplyStepResult` over a
//!    `Sender<Command>` clone — the exact `self_tx` write-back the in-process worker already uses.
//!
//! ## Idempotency (exactly-once *effect* over at-least-once delivery)
//!  * `task.dispatched` and `task.completed` carry a DETERMINISTIC idempotency key per
//!    `(run_id, unit_ix, attempt)`, so a re-emit dedups to one physical row (the bus's UNIQUE key).
//!  * The `cli-runner` dedups on that key in-process (never runs the same task twice within a run) and,
//!    across process restarts, a re-run publishes the SAME-keyed completed row (harmless dedup).
//!  * The actor's `apply_step_result` guard applies a `task.completed` only when its `(unit_ix, attempt)`
//!    is the CURRENT one (`unit_ix == cursor` AND `attempt == session.attempt`) and the unit isn't
//!    `Done` — a redelivered or SUPERSEDED-attempt result is a no-op (`Stale`), exactly-once apply.
//!
//! ## Durability across a crash/restart (the LOST-ON-CRASH fix)
//! Both consumers persist a DURABLE cursor in the bus db's `core_exec_cursors` table
//! ([`BusDb::save_cursor`]), advanced ONLY AFTER an event is handled+acked. On start each consumer
//! RESUMES from its persisted cursor and falls back to the bus tail only on a true first run (no
//! persisted cursor). So a `task.dispatched` that arrived before a crash but was never handled is
//! re-polled and run on the next start rather than skipped forever. Complementarily, on actor bootstrap
//! in ARMED mode any session left `Executing` is RE-DRIVEN (its cursor unit re-dispatched under a bumped
//! attempt) so a dispatch lost across the restart recovers. Earlier revisions of this module claimed a
//! cross-restart re-run the code did not actually provide (seam finding #9); it now does.
//!
//! ## Live output under exec-mediation (parity gap #11 — now bridged in-process)
//!  * The `cli-runner` runs IN-PROCESS with the actor (spawned by `actor::run` on its own thread,
//!    holding no store handle). So it reaches the actor's SINGLE emit point via the exact `self_tx`
//!    write-back the in-process worker uses: each incremental output chunk becomes a
//!    `Command::CliOutputDelta` the actor fans out as `CoreEvent::CliOutputDelta`. The studio's live
//!    pane therefore ticks under exec-mediation with BYTE-IDENTICAL incremental streaming to the
//!    in-process path (same `run_unit_streaming` sink).
//!  * HONEST LIMIT: the delta stream does NOT ride the bus. Execution MEDIATION is over the bus
//!    (`task.dispatched` → run → `task.completed`); the live-output deltas are an in-process UX
//!    side-channel over the command channel. This is correct precisely because the `cli-runner` is
//!    co-process with the actor here. If the `cli-runner` were ever moved to a SEPARATE process, live
//!    output would need the optional §2.2 `wicked.crew.task.delta` bus event instead (the command channel
//!    would no longer reach the actor). The FINAL output + verdict already ride the bus and are
//!    identical on both paths.
//!  * TTL (#10): a `task.dispatched`/`task.completed` event is subject to the bus's 72h `expires_at`
//!    TTL. A consumer offline past the TTL would find the event swept before it polls — an unconsumed
//!    task event can be dropped. The restart re-drive (above) is the recovery for a lost dispatch; a
//!    lost completed is recovered the same way (the re-driven unit re-runs and re-publishes).

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::acp_runner::ElicitationMaps;
use crate::bus::{deterministic_key, BusDb, BusEmit, CORE_DOMAIN};
use crate::command::Command;
use crate::scope::EntityMode;
use crate::workflow::{DeltaSink, StepInput, StepOutput, StepRunner, StepStatus};
use wicked_apps_core::HardenedCommand;

/// The event the reducer publishes and the `cli-runner` consumes (filtered).
pub const TASK_DISPATCHED: &str = "wicked.crew.task.dispatched";
/// The event the `cli-runner` publishes and the reducer consumes.
pub const TASK_COMPLETED: &str = "wicked.crew.task.completed";

/// Gate evaluation events — wicked-core publishes a request; the governed evaluator daemon responds.
/// When exec mediation is armed for the unit ([`gate_eval_bus_db`]),
/// `run_unit_and_judge_with_roster` publishes one of these instead of
/// spawning a raw `claude -p` subprocess, and blocks (up to [`GATE_EVAL_TIMEOUT`]) for the response.
/// The daemon runs under its OWN governed session (no `--dangerously-skip-permissions`).
pub const GATE_EVAL_REQUESTED: &str = "wicked.gate.eval.requested";
pub const GATE_EVAL_RESPONDED: &str = "wicked.gate.eval.responded";

/// The bus db the gate judge publishes its evaluation request to, or `None` for the inline judge.
///
/// DES-TEAMING-002 T0: the bus judge belongs to EXEC MEDIATION, so it is used ONLY over a bus exec
/// mediation actually ARMED for this unit — `exec_bus`, passed by the caller: the cli-runner's own
/// bus, or the actor's armed publisher's when a publish fell back in-process. Never process env:
/// `WICKED_BUS_DB` alone is only the bus handoff (crew sets it on every boot), and `WICKED_BUS_EXEC`
/// with a bus exec could not initialise leaves exec UNARMED — re-reading env there would send the
/// in-process worker's judge to a bus that is not there and DENY on the infrastructure failure. The
/// bus round-trip has no in-product responder except under exec (`scripts/gate_eval_daemon.py`),
/// and denies after [`GATE_EVAL_TIMEOUT`].
fn gate_eval_bus_db(exec_bus: Option<&str>) -> Option<String> {
    exec_bus.filter(|p| !p.is_empty()).map(str::to_string)
}

/// Wall-clock budget for the bus-mediated gate evaluator. On timeout the gate falls back to
/// deterministic-only (no agent verdict), so `combine_verdict(det_pass, None)` decides.
const GATE_EVAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

/// Payload published to the bus when the gate needs an agent verdict.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct GateEvalRequest {
    /// Deterministic per-(run, unit, attempt) key used to match the response.
    eval_id: String,
    criterion: String,
    work: String,
    run_id: String,
    unit_ix: usize,
    attempt: u32,
    /// The CLI key that AUTHORED the work being evaluated (from `unit.assigned_cli`).
    /// The governed evaluator daemon MUST exclude this seat from its selection so the author
    /// cannot self-grade — this is the bus-path equivalent of the inline path's
    /// `let excluded = [DETERMINISTIC_VALIDATOR_SEAT, work_author]` guard (cli_runner.rs ~499).
    /// Core never publishes a request without it (core#625: a unit with no work author is a DENY
    /// before any request); `None` only arrives from an older producer, and the daemon then
    /// applies maximum exclusion (judges nothing, fail-closed).
    #[serde(default)]
    work_author: Option<String>,
    /// (DES-TEAMING-001 §6.2, T5) The team monitors that authored or corroborated a finding in
    /// the unit's ledger — computed from the ledger, never supplied. The daemon MUST select no
    /// seat whose instance or cli key is here or is `work_author`. Always serialized (`[]` for
    /// an un-teamed unit), so an old payload reads as `[]`.
    #[serde(default)]
    excluded_seats: Vec<String>,
}

/// Payload the governed evaluator daemon publishes back.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct GateEvalResponse {
    eval_id: String,
    pass: bool,
    reasoning: String,
    /// (T5) The seat that judged. Required to PROVE monitor exclusion when `excluded_seats` was
    /// non-empty; `None` from an older daemon.
    #[serde(default)]
    judge_cli: Option<String>,
}

/// The fail-closed DENY a bus judge gets when it cannot prove it excluded the team's monitors.
pub(crate) const MONITOR_EXCLUSION_DENY: &str =
    "gate eval bus-path DENY (fail-closed): evaluator did not prove monitor exclusion";

/// (core#625) The DENY for a pinned unit with no work author: evaluator≠creator cannot be proven,
/// so no request is published at all.
pub(crate) const WORK_AUTHOR_UNKNOWN_DENY: &str =
    "gate eval bus-path DENY (fail-closed): the work author is unknown, so evaluator≠creator \
     cannot be proven";

/// (core#625) The DENY for a response that names no judge seat: exclusion cannot be verified.
pub(crate) const JUDGE_UNNAMED_DENY: &str =
    "gate eval bus-path DENY (fail-closed): evaluator did not name its judge seat";

/// (core#625) The DENY for a response judged by the work author (instance or cli key).
pub(crate) const CREATOR_JUDGE_DENY: &str =
    "gate eval bus-path DENY (fail-closed): the evaluator judged as the work author";

/// The `wicked.task.dispatched` payload. Carries what the `cli-runner` needs to reconstruct the
/// [`StepInput`] the in-process worker would have run — so it reuses the same [`StepRunner`] with
/// no store handle and no duplicated execution logic. `agent_review_target` is the creator's COLD output
/// the actor resolved on-thread (seam finding #8) so the evaluator judges the right artifact off-actor.
///
/// SECURITY (seam finding #7): the unit's APPROVED deterministic validator's shell SCRIPT is NOT
/// serialized here. The `cli-runner` needs only the validator's CRITERION (for the LLM agent judge),
/// never the script — the deterministic script is re-verified at the GATE on the ACTOR, from the unit
/// the actor reads out of its OWN store. [`strip_validator_script`] blanks the script before the unit
/// rides the bus; `validator_pin` carries the content-address of the approved validator for provenance
/// (a re-load-by-pin handle should the cli-runner ever need the full script, which today it does not).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DispatchedTask {
    run_id: String,
    unit_ix: usize,
    attempt: u32,
    workflow_id: String,
    entity_mode: EntityMode,
    /// The run's worktree (the wrapped-CLI runner's cwd). `None` ⇒ the runner's default cwd.
    workdir: Option<String>,
    /// The unit — with its validator's SCRIPT blanked (finding #7); carries description, validator
    /// criterion, role, skill scope… everything the off-actor runner + agent judge legitimately need.
    unit: crate::domain::WorkUnit,
    /// The creator's cold output an Evaluator-role unit judges (else `None` ⇒ judge the unit's own output).
    agent_review_target: Option<String>,
    /// The assigned CLI key — the routing/filter dimension (§2.2: `task.dispatched` filtered by cli).
    cli: Option<String>,
    /// The content-address PIN of the unit's approved validator (finding #7) — provenance / re-load
    /// handle. `None` ⇒ the unit carried no validator.
    #[serde(default)]
    validator_pin: Option<String>,
    /// GOVERNANCE OPT-IN carried across the bus (DES-OUTGOV-003 §5): without this the off-actor
    /// cli-runner would rebuild an UNGOVERNED `StepInput`, silently disabling input governance for the
    /// whole exec-mediation delivery mode. `None` ⇒ ungoverned (default preserves old wire compat).
    #[serde(default)]
    governance: Option<crate::workflow::GovernanceContext>,
    /// Cross-CLI shared context (ACP multi-CLI): prior completed units that ran on a different CLI,
    /// actor-populated before dispatch so the worker holds no store handle.
    #[serde(default)]
    prior_outputs: Vec<PriorOutputWire>,
    /// The actor-lifetime UUID generated once per `actor::run` invocation. Used by the bus consumer
    /// to detect foreign tasks (wrong actor) and predecessor tasks (migrated cursor from a crashed
    /// predecessor). `None` on old wire payloads (pre-DES-002); those are discarded as legacy.
    #[serde(default)]
    process_gen: Option<uuid::Uuid>,
    /// Monotonic sequence number from `ElicitationMaps::begin_launch`. Combined with `process_gen`
    /// as the bus dedup key and stale-completion guard. `0` on pre-DES-002 payloads; those are
    /// discarded (the bus consumer rejects `launch_seq == 0` for ACP activation).
    #[serde(default)]
    launch_seq: u64,
    /// True when the dispatching actor was running in ACP mode (elicitation bus path). Controls
    /// whether the consumer calls `try_next_epoch_bus` to allocate an ACP epoch.
    #[serde(default)]
    is_acp: bool,
    /// The run's whole skill set ([`StepInput::required_skills`], core#396), so the off-actor
    /// runner refuses a launch whose snapshot lacks any of them exactly as the in-process worker
    /// would. Empty on older payloads — the runner then admits the unit on its own `skill_ref`.
    #[serde(default)]
    required_skills: Vec<String>,
}

/// Wire representation of a cross-CLI prior-unit output (Serialize/Deserialize for the event bus).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PriorOutputWire {
    label: String,
    output: String,
}

/// Blank the deterministic validator's SHELL SCRIPT before a unit rides the bus (seam finding #7): the
/// approved script must never be serialized in plaintext onto the event log. Returns the sanitized unit
/// plus the validator's content-address pin (computed over the ORIGINAL, script-and-all, so it is the
/// real approved-validator address). The cli-runner uses only the criterion; the gate re-verifies the
/// script from the actor's own store, so blanking it here changes nothing about the outcome.
fn strip_validator_script(
    unit: &crate::domain::WorkUnit,
) -> (crate::domain::WorkUnit, Option<String>) {
    let pin = unit.validator.as_ref().map(crate::validator_vault::pin);
    let mut sanitized = unit.clone();
    if let Some(v) = sanitized.validator.as_mut() {
        v.script = String::new();
    }
    (sanitized, pin)
}

/// The `wicked.task.completed` payload — mirrors the fields `Command::ApplyStepResult` carries
/// (`StepOutput` + the LAYER-2 agent verdict). `status` is a string because [`StepStatus`] is not
/// `Serialize` (and workflow.rs is out of scope); [`status_to_str`]/[`status_from_str`] map it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CompletedTask {
    run_id: String,
    unit_ix: usize,
    attempt: u32,
    output: String,
    status: String,
    agent_verdict: Option<AgentVerdictWire>,
    // (DES-STUDIO-COCKPIT-001 §3 B3/B4) The runner's adapter usage/files ride the bus so the armed
    // (exec-mediated) daemon path emits `CliUsage`/`DataUsed` just like the in-process path. `#[serde(default)]`
    // keeps older payloads (no usage/files) parseable — absent ⇒ silent (passthrough seats).
    #[serde(default)]
    usage: Option<crate::workflow::Usage>,
    #[serde(default)]
    files: Vec<String>,
    /// Tool NAMES the runner's adapter captured (FINDING-046) — rides the bus alongside usage/files
    /// so the exec-mediated daemon path emits `ToolInvoked` identically to the in-process path.
    /// `#[serde(default)]` keeps older payloads (no tools) parseable — absent ⇒ empty.
    #[serde(default)]
    tools: Vec<String>,
    /// Whether the off-actor runner armed input governance (wrote the armed marker) — carried so the
    /// actor-side fold applies evidence-integrity fail-closure identically for the bus delivery mode.
    #[serde(default)]
    governed: bool,
    /// Engine-derived evidence gathered by the off-actor runner after the seat's work (F-036
    /// worktree guard outcome, F-039 repo-checks report), so the exec-mediated path folds the
    /// SAME evidence the in-process path does. `#[serde(default)]` keeps pre-evidence payloads
    /// parseable — absent ⇒ nothing gathered (the fold then fails a GUARDED unit closed).
    #[serde(default)]
    evidence: crate::workflow::UnitEvidence,
    /// Actor-lifetime UUID echoed from `DispatchedTask` — lets the `task.completed` poller pass the
    /// stale-completion guard token to `Command::ApplyStepResult`. `None` on pre-DES-002 payloads.
    #[serde(default)]
    process_gen: Option<uuid::Uuid>,
    /// Launch sequence number echoed from `DispatchedTask`. `0` on pre-DES-002 payloads.
    #[serde(default)]
    launch_seq: u64,
}

/// The wire form of the `(pass, reasoning)` agent verdict `ApplyStepResult` carries.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AgentVerdictWire {
    pass: bool,
    reasoning: String,
    /// (core#431) The judge seat + distinctness, carried across the bus so the actor's gate fold
    /// can name the judge on `gateEvaluated`. `default` so a `task.completed` from an older
    /// cli-runner (no judge fields) still deserializes — as an unknown judge, never a guessed one.
    #[serde(default)]
    judge_cli: Option<String>,
    #[serde(default)]
    judge_distinct: Option<bool>,
    /// (core#772) The seat-failure marker, carried so the fold books a judge that could not
    /// run as `judge_unavailable` on the bus path too. `default`: absent from an older runner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    seat_failure: Option<String>,
    /// (operator ruling 2026-10-14) The fallback-judge disclosure. `default`: absent from an
    /// older runner (the bus path's judge is the evaluator daemon's, never a fallback seat).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    judge_fallback: Option<crate::assurance::JudgeFallback>,
}

impl From<AgentVerdictWire> for crate::validator::AgentVerdict {
    fn from(v: AgentVerdictWire) -> Self {
        crate::validator::AgentVerdict {
            pass: v.pass,
            reasoning: v.reasoning,
            judge_cli: v.judge_cli,
            judge_distinct: v.judge_distinct,
            seat_failure: v.seat_failure,
            judge_fallback: v.judge_fallback,
        }
    }
}

impl From<crate::validator::AgentVerdict> for AgentVerdictWire {
    fn from(v: crate::validator::AgentVerdict) -> Self {
        AgentVerdictWire {
            pass: v.pass,
            reasoning: v.reasoning,
            judge_cli: v.judge_cli,
            judge_distinct: v.judge_distinct,
            seat_failure: v.seat_failure,
            judge_fallback: v.judge_fallback,
        }
    }
}

fn status_to_str(s: StepStatus) -> &'static str {
    match s {
        StepStatus::Ok => "ok",
        StepStatus::Failed => "failed",
        StepStatus::Cancelled => "cancelled",
        // ACP elicitation terminal: non-retriable; bypasses FailureTriageReady (DES-002 I-7).
        StepStatus::ElicitationFailed => "elicitation_failed",
        // The engine's own turn ceiling — kept apart from "cancelled" so a consumer can
        // separate the platform's deadline from an operator cancel (616c8661).
        StepStatus::TimedOut => "timed_out",
    }
}

fn status_from_str(s: &str) -> StepStatus {
    match s {
        "failed" => StepStatus::Failed,
        "cancelled" => StepStatus::Cancelled,
        // ACP elicitation terminal — explicit arm required; the wildcard default `Ok` is WRONG here
        // (DES-002-tests.md §StepStatus exhaustive match sites).
        "elicitation_failed" => StepStatus::ElicitationFailed,
        // Turn-timeout terminal — same rule: falling through to `Ok` would fold a killed turn
        // as completed work.
        "timed_out" => StepStatus::TimedOut,
        _ => StepStatus::Ok,
    }
}

/// The deterministic idempotency key for one exact launch. Generation and sequence
/// prevent a post-restart launch from deduplicating against an already-consumed
/// predecessor event with the same run, unit, and attempt.
fn task_key(
    event_type: &str,
    run_id: &str,
    unit_ix: usize,
    attempt: u32,
    process_gen: Option<uuid::Uuid>,
    launch_seq: u64,
) -> String {
    let process_gen = process_gen.map(|gen| gen.to_string()).unwrap_or_default();
    deterministic_key(&[
        event_type,
        run_id,
        &unit_ix.to_string(),
        &attempt.to_string(),
        &process_gen,
        &launch_seq.to_string(),
    ])
}

// ── Bus-mediated gate evaluation (replaces the inline `agent_validate` subprocess) ───────────────────

/// Publish `wicked.gate.eval.requested` to the bus and BLOCK until the governed evaluator daemon
/// responds with `wicked.gate.eval.responded` — or until [`GATE_EVAL_TIMEOUT`] elapses.
///
/// Returns an infallible `AgentVerdict`. On timeout or any bus error the verdict is a **hard deny**
/// (`pass: false`) — governance fails closed; a timeout or infrastructure failure must never silently
/// approve a gate.
///
/// WHY NOT A SUBPROCESS: the inline `agent_validate` spawns `claude -p --plugin-dir <garden>` whose
/// `SessionStart` hook fires `bootstrap.py` → bubbletea TUI → "open /dev/tty: device not configured"
/// on the first stdout line → `parse_agent_verdict` fails closed. The bus path routes evaluation to
/// a governed daemon running under its OWN Claude Code session with normal tool approvals — no
/// `--dangerously-skip-permissions`, no headless-TTY issues.
// `work_author`: the CLI key that authored the work — carried in `GateEvalRequest` so the
// evaluator daemon can enforce evaluator≠creator on the bus path (same guarantee the inline
// path enforces via `excluded = [DETERMINISTIC_VALIDATOR_SEAT, work_author]` at ~499).
#[allow(clippy::too_many_arguments)]
fn bus_request_agent_verdict(
    criterion: &str,
    work: &str,
    run_id: &str,
    unit_ix: usize,
    attempt: u32,
    bus_db_path: &str,
    work_author: Option<&str>,
    excluded_seats: &[String],
) -> crate::validator::AgentVerdict {
    // Fail-closed helper: any error on the bus path is a governance deny, never a silent pass.
    macro_rules! bus_deny {
        ($reason:expr) => {
            return crate::validator::AgentVerdict {
                judge_cli: None,
                judge_distinct: None,
                seat_failure: None,
                judge_fallback: None,
                pass: false,
                reasoning: $reason,
            }
        };
    }

    // core#625: the work author is part of every request's exclusion set; with none there is
    // nothing to prove evaluator≠creator against, so no request is published at all.
    let Some(author) = work_author.map(str::trim).filter(|a| !a.is_empty()) else {
        bus_deny!(WORK_AUTHOR_UNKNOWN_DENY.to_string());
    };
    let author_set = [author.to_string()];

    let db = match BusDb::shared(bus_db_path) {
        Ok(d) => d,
        Err(e) => bus_deny!(format!(
            "gate eval bus-path DENY (fail-closed): cannot open bus db: {e}"
        )),
    };

    let eval_id = deterministic_key(&[
        "gate-eval",
        run_id,
        &unit_ix.to_string(),
        &attempt.to_string(),
    ]);

    let request = GateEvalRequest {
        eval_id: eval_id.clone(),
        criterion: criterion.to_string(),
        work: work.to_string(),
        run_id: run_id.to_string(),
        unit_ix,
        attempt,
        work_author: Some(author.to_string()),
        excluded_seats: excluded_seats.to_vec(),
    };
    let payload = match serde_json::to_value(&request) {
        Ok(p) => p,
        Err(e) => bus_deny!(format!(
            "gate eval bus-path DENY (fail-closed): cannot serialize request: {e}"
        )),
    };
    let key = deterministic_key(&["gate-eval-req", &eval_id]);
    let ev = BusEmit::new(GATE_EVAL_REQUESTED, CORE_DOMAIN, "core.gate", payload).with_key(key);
    // Capture the emitted event_id so polling starts AFTER this request — no historical rescans.
    let floor_start = match db.emit(&ev) {
        Ok(id) => id,
        Err(e) => bus_deny!(format!(
            "gate eval bus-path DENY (fail-closed): cannot publish request: {e}"
        )),
    };

    eprintln!("wicked-core: gate eval request published (eval_id={eval_id}, floor={floor_start}); waiting up to {GATE_EVAL_TIMEOUT:?}");

    // Poll for the matching response (by eval_id) until timeout. Start from the request event_id
    // so we never replay historical GATE_EVAL_RESPONDED events from prior runs.
    let start = std::time::Instant::now();
    let mut floor: i64 = floor_start;
    while start.elapsed() < GATE_EVAL_TIMEOUT {
        let events = match db.poll(GATE_EVAL_RESPONDED, floor, 20) {
            Ok(evs) => evs,
            Err(e) => {
                eprintln!("wicked-core: gate eval poll error: {e}");
                std::thread::sleep(std::time::Duration::from_millis(500));
                continue;
            }
        };
        for ev in &events {
            if let Ok(resp) = serde_json::from_value::<GateEvalResponse>(ev.payload.clone()) {
                if resp.eval_id == eval_id {
                    eprintln!(
                        "wicked-core: gate eval response received — pass={} reasoning={:?}",
                        resp.pass, resp.reasoning
                    );
                    // T5 + core#625: only a response that NAMES a judge outside
                    // `work_author ∪ excluded_seats` (instance or cli key) is honoured; a missing
                    // or excluded judge is a DENY — an empty `excluded_seats` never bypasses the
                    // evaluator≠creator check (DES-001 §6.2).
                    let monitors = !excluded_seats.is_empty();
                    match resp.judge_cli.as_deref() {
                        None if monitors => bus_deny!(MONITOR_EXCLUSION_DENY.to_string()),
                        None => bus_deny!(JUDGE_UNNAMED_DENY.to_string()),
                        Some(j)
                            if monitors && crate::team::runner::is_excluded(j, excluded_seats) =>
                        {
                            bus_deny!(MONITOR_EXCLUSION_DENY.to_string())
                        }
                        Some(j) if crate::team::runner::is_excluded(j, &author_set) => {
                            bus_deny!(CREATOR_JUDGE_DENY.to_string())
                        }
                        Some(_) => {}
                    }
                    return crate::validator::AgentVerdict {
                        judge_cli: resp.judge_cli,
                        judge_distinct: None,
                        seat_failure: None,
                        judge_fallback: None,
                        pass: resp.pass,
                        reasoning: resp.reasoning,
                    };
                }
            }
            floor = floor.max(ev.event_id);
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    // GOVERNANCE FAIL-CLOSED: treat evaluator daemon timeout as a hard DENY.
    // Returning None here would make combine_verdict approve the gate silently (agent_rejects=false
    // via unwrap_or(false)), creating a timing-based bypass of the dual-validator model. A 3-minute
    // disruption to the daemon would eliminate semantic judgment while the gate reports clean approval.
    eprintln!(
        "wicked-core: gate eval timed out after {GATE_EVAL_TIMEOUT:?} (eval_id={eval_id}) — \
         GOVERNANCE DENY (fail-closed; a timeout must never silently approve)"
    );
    crate::validator::AgentVerdict {
        judge_cli: None,
        judge_distinct: None,
        seat_failure: None,
        judge_fallback: None,
        pass: false,
        reasoning: format!(
            "gate eval bus-path DENY: evaluator daemon did not respond within {GATE_EVAL_TIMEOUT:?}. \
             Governance fails closed — a timeout must not silently approve a gate. \
             Ensure the governed evaluator daemon is running and subscribed to '{GATE_EVAL_REQUESTED}'."
        ),
    }
}

// ── The shared execute+judge core (reused by BOTH the in-process worker AND the cli-runner) ──────────

/// Run one unit's slow work via `runner` and compute the rev0.4 LAYER-2 agent verdict — the EXACT
/// behavior the in-process worker had, extracted so both dispatch paths run byte-identical logic (this
/// is what guarantees "same outcome as the in-process path"). Holds no store handle: `agent_review_target`
/// is passed in (resolved by the actor on-thread). The LLM `agent_validate` runs here — OFF the actor —
/// exactly as it did on the worker thread. A non-`Ok` step or a workdir-less run gets no agent verdict
/// (the actor handles a failed/cancelled worker before any gate; layer-1 fails closed without a worktree).
/// Concatenate the readable declared deliverables (each a path relative to `workdir`) into one blob for
/// the agent judge, each headed by its filename. `None` if none are readable.
///
/// Only worktree-relative, non-escaping deliverables are read. That is NARROWER than the deterministic
/// floor ([`crate::path_policy::missing_deliverables`]), which since core#297 §3 also resolves a
/// deliverable inside a declared `extra_write_roots` entry — deliberately so. The floor only asks
/// whether a path EXISTS; this reads the file's CONTENT into a judge prompt, and content from outside
/// the worktree is exactly the exfiltration the three fences below exist to stop. A deliverable in a
/// write root is therefore still floored (it must exist) but not fed to the judge. A `required_deliverables`
/// list comes from
/// a WorkflowDef (arbitrary author data) and the worker itself writes the files, so the judge input must
/// never pull content from OUTSIDE the worktree. Three fences, all fail-closed (skip on violation):
///
/// - TEXTUAL: reject an absolute or `..`-escaping declared path (already unverifiable at the det floor).
/// - SYMLINK: canonicalize the resolved path and require it to stay under the canonicalized worktree, so
///   an in-worktree symlink to `/etc/passwd` cannot exfiltrate outside content (Copilot review #229).
/// - SIZE: cap each deliverable so a huge file can't balloon the judge prompt / bus payload (#229).
///
/// `None` if the worktree can't be canonicalized or nothing readable remains.
fn read_deliverables_for_judge(
    deliverables: &[String],
    workdir: &std::path::Path,
) -> Option<String> {
    /// Per-deliverable byte cap for what is fed to the LAYER-2 judge.
    const MAX_DELIVERABLE_BYTES: u64 = 1_000_000;
    // Resolve the worktree once (also collapses macOS `/var`→`/private/var`) for the containment check.
    // Fail-closed: if the worktree itself can't be canonicalized, read nothing.
    let wt_canon = std::fs::canonicalize(workdir).ok()?;
    let mut blob = String::new();
    for rel in deliverables {
        let trimmed = rel.trim();
        if trimmed.is_empty() {
            continue;
        }
        let p = std::path::Path::new(trimmed);
        if p.is_absolute()
            || p.components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            continue;
        }
        // Resolve symlinks and confirm the real target is still inside the worktree.
        let Ok(canon) = std::fs::canonicalize(wt_canon.join(p)) else {
            continue;
        };
        if !canon.starts_with(&wt_canon) {
            continue;
        }
        // Skip an over-cap file rather than read it (metadata failure ⇒ treat as over-cap, fail-closed).
        if std::fs::metadata(&canon)
            .map(|m| m.len())
            .unwrap_or(u64::MAX)
            > MAX_DELIVERABLE_BYTES
        {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(&canon) {
            if !blob.is_empty() {
                blob.push('\n');
            }
            blob.push_str("=== ");
            blob.push_str(trimmed);
            blob.push_str(" ===\n");
            blob.push_str(&content);
        }
    }
    (!blob.is_empty()).then_some(blob)
}

/// Pick the text the LAYER-2 agent judge reviews.
///
/// An Evaluator-role unit that declares its OWN `required_deliverables` (e.g. `coverage` →
/// `coverage-report.json`) is judged against a criterion that TARGETS that deliverable, so the judge must
/// see the deliverable content. `agent_review_target` (the prior creator's cold output) is the right
/// target ONLY for a PURE reviewer — an Evaluator with no deliverable of its own (adversarial-review),
/// which judges the work it reviews. Before this, `coverage`'s judge was fed the upstream domain-model
/// narrative and rejected a correct `coverage-report.json` (behavior_bearing=766, coverage=1.0, 0
/// unaccounted) as "a narrative … never presents a coverage computation" — a false-NEGATIVE gate, the
/// mirror of FINDING-091. If the deliverable can't be read (should not happen once the deterministic
/// floor passed), fall back to the creator-output target — fail-closed, not silently permissive.
fn select_work_for_agent<'a>(
    role: crate::workflow::PhaseRole,
    required_deliverables: &[String],
    workdir: Option<&std::path::Path>,
    agent_review_target: Option<&'a str>,
    own_output: &'a str,
) -> std::borrow::Cow<'a, str> {
    if role == crate::workflow::PhaseRole::Evaluator && !required_deliverables.is_empty() {
        if let Some(blob) =
            workdir.and_then(|wd| read_deliverables_for_judge(required_deliverables, wd))
        {
            return std::borrow::Cow::Owned(blob);
        }
    }
    std::borrow::Cow::Borrowed(agent_review_target.unwrap_or(own_output))
}

/// Head bytes kept when an oversized review target is elided (core#282).
const REVIEW_TARGET_HEAD: usize = 24 * 1024;
/// Tail bytes kept when an oversized review target is elided (core#282).
const REVIEW_TARGET_TAIL: usize = 24 * 1024;

/// Bound the review target handed to an Evaluator unit's worker (core#282).
///
/// `agent_review_target` — the creator's COLD output — rides verbatim into the evaluator seat's
/// prompt (and, under exec-mediation, into the `task.dispatched` payload), so a huge creator
/// output can blow a prompt-arg seat's backend outright. Same instrument as
/// [`worktree_evidence_for_judge`]'s `clip`, but head+TAIL rather than head-only: creators put
/// their summary/conclusion LAST by convention, so an evaluator fed only the head would judge N
/// KB of preamble with the actual result cut off. The marker states the elision so the evaluator
/// knows it is looking at an excerpt, not the whole artifact. Under the cap the input passes
/// through untouched (no realloc).
pub(crate) fn clip_review_target(s: String) -> String {
    clip_head_tail(s, REVIEW_TARGET_HEAD, REVIEW_TARGET_TAIL)
}

/// [`clip_review_target`] with an explicit head and tail budget (core#754: the prior-context block
/// shares one total budget across every prior output).
pub(crate) fn clip_head_tail(s: String, head: usize, tail: usize) -> String {
    if s.len() <= head + tail {
        return s;
    }
    let mut head_end = head;
    while !s.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = s.len() - tail;
    while !s.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    format!(
        "{}\n[...elided by harness: {} bytes of the creator's output omitted; head and tail \
         retained...]\n{}",
        &s[..head_end],
        tail_start - head_end,
        &s[tail_start..]
    )
}

/// Harness-derived worktree evidence, appended to the WORK the layer-2 agent judge reviews.
///
/// Without this the judge sees only PROSE — the creator's cold output (Evaluator units) or the
/// unit's own output — and a worker that COMMITTED its work presents a clean porcelain plus text
/// that "merely asserts" a deliverable exists. That is core#280's false rejection: the judge
/// truthfully reported "no diff is provided" about a run whose branch carried an 838-line committed
/// deliverable. The harness holds the worktree, so the harness states what it observes, with the
/// same two instruments as the deterministic floor ([`crate::builtin_floors::EVIDENCE_SCRIPT`]):
/// porcelain for uncommitted work, run-branch-only commits (`HEAD --not --exclude=wicked/*
/// --branches`, `--stat` so the judge sees WHICH files) for committed work.
///
/// Returns `None` when there is nothing to report (both instruments empty or git unavailable) —
/// the judge then sees exactly what it saw before this fix. Output is bounded (4 KiB per section,
/// head-truncated with a marker) so a large diff cannot crowd the criterion out of the prompt.
fn worktree_evidence_for_judge(workdir: &std::path::Path) -> Option<String> {
    fn git(workdir: &std::path::Path, args: &[&str]) -> String {
        // spawn-audit: hardened — read-only git plumbing over the run's own worktree.
        let out = std::process::Command::new("git")
            .hardened()
            .args(args)
            .current_dir(workdir)
            .output();
        match out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
            _ => String::new(),
        }
    }
    fn clip(s: &str) -> String {
        const CAP: usize = 4096;
        if s.len() <= CAP {
            return s.to_string();
        }
        let mut end = CAP;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}\n… [truncated by harness]", &s[..end])
    }
    let uncommitted = git(workdir, &["status", "--porcelain"]);
    let committed = git(
        workdir,
        &[
            "log",
            "--stat",
            "--oneline",
            "HEAD",
            "--not",
            "--exclude=wicked/*",
            "--branches",
        ],
    );
    if uncommitted.trim().is_empty() && committed.trim().is_empty() {
        return None;
    }
    let mut s = String::from(
        "\n\nWORKTREE EVIDENCE (stated by the harness from the run's worktree, not by the worker):\n",
    );
    if !uncommitted.trim().is_empty() {
        s.push_str("uncommitted changes (git status --porcelain):\n");
        s.push_str(&clip(&uncommitted));
    }
    if !committed.trim().is_empty() {
        s.push_str(
            "commits reachable from HEAD and from no non-wicked branch (git log --stat) — \
             in a run worktree these are the commits made on the run's branch:\n",
        );
        s.push_str(&clip(&committed));
    }
    Some(s)
}

/// `run_roster` — the RUN's configured seat keys (`AgentSession::clis`): the judge is drawn from
/// the registry seats the launcher actually configured for this run (review of #449, RT-1), not
/// from every registry record; empty = no intersection (the bus path, tests). `bench` — the
/// run's BENCHED seats with their reasons (`AgentSession::benched_seats`, F-7R2-006): never a
/// judge — except a seat benched for governed work only (`BenchedSeat::governed_only`, C5: the
/// judge is ungoverned), which this drops first — and named in a fallback judge's disclosure (operator ruling 2026-10-14). `work_authors` —
/// for an evaluator unit, every seat that authored the work its judge reads
/// ([`crate::pipeline::work_authors`]): never a judge of it; `author_binaries` — the binaries
/// they ran ([`JudgeFallbackCtx::author_binaries`]).
///
/// `exec_bus` — the bus exec mediation resolved for this unit (`None` off exec): the pinned judge
/// publishes its evaluation request there ([`gate_eval_bus_db`]).
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_unit_and_judge(
    runner: &Arc<dyn StepRunner>,
    input: &StepInput,
    agent_review_target: Option<&str>,
    emit_delta: &DeltaSink,
    run_roster: &[String],
    bench: &[crate::domain::BenchedSeat],
    work_authors: &[String],
    author_binaries: Option<&[String]>,
    exec_bus: Option<&str>,
    team: Option<&crate::team::runner::TeamRunner>,
) -> (
    StepOutput,
    Option<crate::validator::AgentVerdict>,
    crate::workflow::UnitEvidence,
) {
    // (operator ruling 2026-10-14) The fallback judge seats, read from the registry per call —
    // only for a run the caller knows (its roster): the bus worker knows neither roster nor
    // bench, and its judge is the evaluator daemon's.
    let registry = if run_roster.is_empty() {
        Vec::new()
    } else {
        full_registry()
    };
    // (C5) The judge is an ungoverned engine call: a seat benched for governed work only (the
    // host-floor bench) still judges.
    let bench = &crate::domain::ungoverned_bench(bench);
    let benched: Vec<String> = bench.iter().map(|b| b.cli.clone()).collect();
    let pool = judge_fallback_pool(&registry);
    run_unit_and_judge_on(
        runner,
        input,
        agent_review_target,
        emit_delta,
        &crate::registry_roster(),
        run_roster,
        &benched,
        JudgeFallbackCtx {
            pool: &pool,
            registry: &registry,
            bench,
            work_authors,
            author_binaries,
            // (PRODUCT-WORKS J4) Re-probe the run's due quota benches before the judge reads them.
            reprobe: !run_roster.is_empty(),
        },
        exec_bus,
        team,
    )
}

/// (PRODUCT-WORKS J4) Re-probe every seat of `bench` whose QUOTA bench has fallen due at `now`
/// ([`crate::seat_bench::due_for_reprobe`]) with one cheap call ([`crate::validator::reprobe_seat`])
/// on its `registry` record, and return the bench as it stands after them (a seat that answered
/// is dropped; one that did not is re-benched for longer) with the outcomes, for the actor to fold
/// ([`crate::workflow::UnitEvidence::bench_reprobes`]). A due seat with no registry record cannot be
/// asked and stays benched. Nothing is due: the bench comes back unchanged and nothing runs.
pub(crate) fn reprobe_due_benches(
    runner: &dyn StepRunner,
    registry: &[crate::AgenticCli],
    bench: &[crate::domain::BenchedSeat],
    now: i64,
    emit_delta: &DeltaSink,
) -> (
    Vec<crate::domain::BenchedSeat>,
    Vec<crate::seat_bench::BenchReprobe>,
) {
    let mut after = bench.to_vec();
    let reprobes: Vec<crate::seat_bench::BenchReprobe> = bench
        .iter()
        .filter(|b| crate::seat_bench::due_for_reprobe(b, now))
        .filter_map(|b| {
            let key = crate::team::runner::seat_key(&b.cli);
            let seat = registry
                .iter()
                .find(|c| c.key == b.cli)
                .or_else(|| registry.iter().find(|c| c.key == key))?;
            let answer = crate::validator::reprobe_seat(&b.cli, seat, runner);
            let r = crate::seat_bench::reprobe_outcome(
                b,
                answer.as_deref().map_err(String::as_str),
                now,
            );
            engine_note(
                emit_delta,
                &match r.benched_until {
                    None => format!(
                        "seat {} answered its re-probe after its quota bench ({}): back on the \
                         run's roster",
                        b.cli, b.reason
                    ),
                    Some(until) => format!(
                        "seat {} did not answer its re-probe after its quota bench ({}): benched \
                         again until {} (strike {})",
                        b.cli,
                        b.reason,
                        crate::seat_bench::iso(until),
                        r.strikes
                    ),
                },
            );
            Some(r)
        })
        .collect();
    crate::seat_bench::apply_reprobes(&mut after, &reprobes);
    (after, reprobes)
}

/// (core#782) The unit a floor fix's seat runs: the operator's note as the whole task, on
/// `fix.seat`, with a creator's write posture — not the read-only phase's skill, validator or
/// floor (those stay on the phase, which does not run again).
pub(crate) fn floor_fix_unit(
    unit: &crate::domain::WorkUnit,
    fix: &crate::repo_checks::FloorFix,
) -> crate::domain::WorkUnit {
    let mut u = unit.clone();
    u.description = format!(
        "Make exactly this fix in the worktree, and nothing else (core#782 floor fix): {} — The \
         `{}` phase reviewed this tree and its verdict stands; the engine's floor re-runs the \
         repository's checks on the tree you leave. Do not commit.",
        fix.note.split_whitespace().collect::<Vec<_>>().join(" "),
        unit.phase_id().unwrap_or("read-only"),
    );
    u.assigned_cli = Some(fix.seat.clone());
    u.assigned_invocation = None;
    u.role = crate::workflow::PhaseRole::Creator;
    u.executes_code = true;
    u.worktree_guarded = false;
    u.pre_build_scope = false;
    u.skill_ref = None;
    u.validator = None;
    u.repo_checks_floor = false;
    // (codex r1 on wicked-core#889) The fixer is not the QE unit: no QE ledger root, no grant.
    u.qe_ledger_root = None;
    u.required_deliverables.clear();
    u.member_step = None;
    u.team = None;
    u
}

/// (core#969, core#970) An ENGINE note on a unit's live output (the floor's heartbeat, a floor fix,
/// a fallback judge): on a line of its own. The seat's own text streams through the same sink
/// with no trailing newline, so a bare note fused onto its last sentence (run 24590a4b:
/// "…confirming the bug and the fix empirically.agent validator: the run's judge is out …").
pub(crate) fn engine_note(emit_delta: &DeltaSink, text: &str) {
    emit_delta(&format!("\n{}\n", text.trim()));
}

/// (core#782) Run a floor fix's seat ([`floor_fix_unit`]), then hand back the read-only phase's
/// input for the floor that follows: its guard baseline re-taken on the fixed tree (a baseline
/// that cannot be re-taken stays, so the guard sees the fix as a mutation and denies — fail
/// closed) and the fixing seat excluded from any judge, beside the seat's own output. Whatever
/// the seat's turn did, the floor decides.
fn run_floor_fix(
    runner: &Arc<dyn StepRunner>,
    input: &StepInput,
    fix: &crate::repo_checks::FloorFix,
    emit_delta: &DeltaSink,
) -> (StepInput, StepOutput) {
    engine_note(
        emit_delta,
        &format!(
        "floor fix: seat '{}' makes the operator's fix; the `{}` phase does not run again — only \
         the floor re-runs, on the fixed tree",
        fix.seat,
        input.unit.phase_id().unwrap_or("read-only")
    ),
    );
    let fixer = StepInput {
        unit: floor_fix_unit(&input.unit, fix),
        ..input.clone()
    };
    let out = runner.run_unit_streaming(&fixer, emit_delta);
    if out.status != StepStatus::Ok {
        engine_note(
            emit_delta,
            &format!(
                "floor fix: seat '{}' ended {:?}; the floor still decides",
                fix.seat, out.status
            ),
        );
    }
    let mut next = input.clone();
    let git_dir = next
        .unit
        .worktree_baseline
        .as_ref()
        .and_then(|b| b.git_dir.clone());
    let rebased = match (next.workdir.as_deref(), git_dir) {
        (Some(wd), Some(gd)) => {
            crate::worktree_guard::snapshot_through(wd, std::path::Path::new(&gd))
                .map_err(|e| e.to_string())
        }
        _ => Err("no pinned worktree baseline to re-take".to_string()),
    };
    match rebased {
        Ok(snap) => next.unit.worktree_baseline = Some(snap),
        // No baseline at all: the guard reads the tree as UNVERIFIABLE and denies — never a
        // comparison that might let the fix (or anything else) through unjudged.
        Err(e) => {
            eprintln!(
                "wicked-core: unit {}: could not re-baseline after the floor fix ({e}); the \
                 guard cannot verify the tree and denies (fail-closed, core#782)",
                next.unit.ord
            );
            next.unit.worktree_baseline = None;
        }
    }
    if !next.unit.exclude_seats.contains(&fix.seat) {
        next.unit.exclude_seats.push(fix.seat.clone());
    }
    (next, out)
}

/// Cadence of the repo-checks floor heartbeat (crew #581 / F-BM-010): 5 min — well under the
/// watchdog's default `workerStallMinutes` (15). A setting below 5 would still read a live
/// floor as stalled (disclosed; the engine does not read crew settings).
const FLOOR_HEARTBEAT_EVERY: Duration = Duration::from_secs(300);

/// (rig run 1d372696) The unit's live note while a distinct seat judges the change, before the
/// repository's checks run (the hand-off to floor gap read as 1.5 min with no event).
const JUDGE_NOTE: &str = "judge: a judge seat is reviewing the change before the checks run";

/// Run `work` while a scoped side thread emits `"<label> — N min"` through `emit` at `t = 0` and
/// then every `every` until `work` returns — liveness for a long, otherwise silent phase on the
/// unit's EXISTING live-output stream (`Command::CliOutputDelta` → the coalesced, durable,
/// relayed `unitOutputDelta`). The sink is `Send + Sync` (`workflow::DeltaSink`), and the
/// runner's own stdout/stderr drains already call it from scoped threads. The heartbeat thread
/// is stopped (channel send) and joined by the scope before the result is returned, so no frame
/// is ever emitted after `work` completed.
fn with_floor_heartbeat<R>(
    emit: &DeltaSink,
    label: &str,
    every: Duration,
    work: impl FnOnce() -> R,
) -> R {
    // The floor's passes are recorded on this thread and named by the ticker, so a floor that runs
    // the suite on the change, the base and the change again says which one it is in.
    let now: crate::repo_checks::FloorNow = Default::default();
    let passes = std::sync::Arc::clone(&now);
    std::thread::scope(|s| {
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        s.spawn(move || {
            let t0 = std::time::Instant::now();
            let doing = || {
                passes
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .describe()
                    .map(|d| format!("; {d}"))
                    .unwrap_or_default()
            };
            engine_note(emit, &format!("{label} — 0 min"));
            // Tick ONLY on a timeout. `Disconnected` means `stop_tx` was dropped without a send —
            // `work` panicked and the closure unwound — and the ticker must exit so the scope can
            // join it and propagate the panic; `is_err()` would spin here forever, emitting false
            // liveness for a floor that is already dead (review on #512).
            while matches!(
                stop_rx.recv_timeout(every),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ) {
                engine_note(
                    emit,
                    &format!("{label} — {} min{}", t0.elapsed().as_secs() / 60, doing()),
                );
            }
        });
        let r = crate::repo_checks::with_floor_now(&now, work);
        let _ = stop_tx.send(());
        r
    })
}

/// The roster-injectable core of [`run_unit_and_judge`] — split out ONLY so the seat-selection (C1) is
/// unit-testable with a fabricated roster and no live registry. Production always passes the live
/// [`crate::registry_roster`].
#[cfg(test)]
fn run_unit_and_judge_with_roster(
    runner: &Arc<dyn StepRunner>,
    input: &StepInput,
    agent_review_target: Option<&str>,
    emit_delta: &DeltaSink,
    roster: &[crate::AgenticCli],
    run_roster: &[String],
    benched: &[String],
) -> (
    StepOutput,
    Option<crate::validator::AgentVerdict>,
    crate::workflow::UnitEvidence,
) {
    run_unit_and_judge_on(
        runner,
        input,
        agent_review_target,
        emit_delta,
        roster,
        run_roster,
        benched,
        JudgeFallbackCtx::default(),
        None,
        None,
    )
}

/// [`run_unit_and_judge_with_roster`] with the fallback judge armed (operator ruling 2026-10-14):
/// `fallback` is what production reads from the registry and the run's bench.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn run_unit_and_judge_with_fallback(
    runner: &Arc<dyn StepRunner>,
    input: &StepInput,
    emit_delta: &DeltaSink,
    roster: &[crate::AgenticCli],
    run_roster: &[String],
    bench: &[crate::domain::BenchedSeat],
    pool: &[crate::AgenticCli],
    work_authors: &[String],
) -> (
    StepOutput,
    Option<crate::validator::AgentVerdict>,
    crate::workflow::UnitEvidence,
) {
    run_unit_and_judge_with_launches(
        runner,
        input,
        emit_delta,
        roster,
        run_roster,
        bench,
        pool,
        work_authors,
        Some(&[]),
    )
}

/// [`run_unit_and_judge_with_fallback`] with the authors' recorded binaries (codex r2/r3).
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn run_unit_and_judge_with_launches(
    runner: &Arc<dyn StepRunner>,
    input: &StepInput,
    emit_delta: &DeltaSink,
    roster: &[crate::AgenticCli],
    run_roster: &[String],
    bench: &[crate::domain::BenchedSeat],
    pool: &[crate::AgenticCli],
    work_authors: &[String],
    author_binaries: Option<&[String]>,
) -> (
    StepOutput,
    Option<crate::validator::AgentVerdict>,
    crate::workflow::UnitEvidence,
) {
    // (C5) The judge is an ungoverned engine call: a seat benched for governed work only (the
    // host-floor bench) still judges.
    let bench = &crate::domain::ungoverned_bench(bench);
    let benched: Vec<String> = bench.iter().map(|b| b.cli.clone()).collect();
    run_unit_and_judge_on(
        runner,
        input,
        None,
        emit_delta,
        roster,
        run_roster,
        &benched,
        JudgeFallbackCtx {
            pool,
            registry: roster,
            bench,
            work_authors,
            author_binaries,
            reprobe: false,
        },
        None,
        None,
    )
}

/// (PRODUCT-WORKS J4) The production judge path with the re-probe armed: what [`run_unit_and_judge`]
/// does, over a fabricated `roster` (the registry the due seats are looked up in).
#[cfg(test)]
fn run_unit_and_judge_reprobing(
    runner: &Arc<dyn StepRunner>,
    input: &StepInput,
    roster: &[crate::AgenticCli],
    run_roster: &[String],
    bench: &[crate::domain::BenchedSeat],
) -> (
    StepOutput,
    Option<crate::validator::AgentVerdict>,
    crate::workflow::UnitEvidence,
) {
    let bench = &crate::domain::ungoverned_bench(bench);
    let benched: Vec<String> = bench.iter().map(|b| b.cli.clone()).collect();
    run_unit_and_judge_on(
        runner,
        input,
        None,
        &|_| {},
        roster,
        run_roster,
        &benched,
        JudgeFallbackCtx {
            pool: &[],
            registry: roster,
            bench,
            work_authors: &[],
            author_binaries: Some(&[]),
            reprobe: true,
        },
        None,
        None,
    )
}

/// [`run_unit_and_judge_with_roster`] with a team runner (T5 tests): the worker-thread seam's
/// `step.claimed` / injector / gate wait around the same judge paths.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_unit_and_judge_with_team(
    runner: &Arc<dyn StepRunner>,
    input: &StepInput,
    emit_delta: &DeltaSink,
    roster: &[crate::AgenticCli],
    exec_bus: Option<&str>,
    team: Option<&crate::team::runner::TeamRunner>,
) -> (
    StepOutput,
    Option<crate::validator::AgentVerdict>,
    crate::workflow::UnitEvidence,
) {
    run_unit_and_judge_on(
        runner,
        input,
        None,
        emit_delta,
        roster,
        &[],
        &[],
        JudgeFallbackCtx::default(),
        exec_bus,
        team,
    )
}

/// The seats a judge may be drawn from: the run's CONFIGURED registry seats, in registry order
/// (review RT-1: an ordinary run is judged on its own seats). Only a run whose roster names NO
/// registry seat (an ad-hoc launch roster, a test's fabricated keys) draws from the registry, minus
/// the opt-in seats it did not pick (operator ruling 2026-10-11: opencode judges only when the run
/// picked it) — there is nothing to intersect with, and a judge must exist.
///
/// (core#937) NEVER a registry seat outside a configured run's pool. #893 let a pinned judge reach
/// outside seats when no configured seat was distinct; on run 0d2b9fa4 that seated the judge on
/// copilot — deselected in the launch wizard, its CLI rejecting both `--acp` and `-p` — and every
/// approve re-ran the step into the same failure. A run whose own seats hold no distinct judge now
/// reaches its gate's explicit `judge_unavailable` / judge-skipped path, where the operator decides.
fn judge_pool<'a>(
    roster: &'a [crate::AgenticCli],
    run_roster: &[String],
) -> Vec<&'a crate::AgenticCli> {
    let configured: Vec<&crate::AgenticCli> = roster
        .iter()
        .filter(|c| in_run_roster(run_roster, &c.key))
        .collect();
    if !configured.is_empty() {
        return configured;
    }
    roster
        .iter()
        .filter(|c| {
            // An opt-in seat the run picked under an instance key (`opencode#2`) counts.
            let picked = run_roster
                .iter()
                .any(|k| wicked_apps_core::spawn::seat_cli_key(k) == c.key);
            !wicked_council::registry::is_opt_in(&c.key) || picked
        })
        .collect()
}

/// (core#937, codex r1) Whether registry seat `key` is one the run configured: its own key, or the
/// CLI of an instance key the launch named (`claude#2` configures the `claude` seat). An
/// instance-only roster used to match nothing and so drew the whole registry.
fn in_run_roster(run_roster: &[String], key: &str) -> bool {
    run_roster
        .iter()
        .any(|k| k == key || wicked_apps_core::spawn::seat_cli_key(k) == key)
}

/// The invocation identity of every author key, resolved against `registry` (the full, unfiltered
/// registry), so the exclusion holds even when an author's own record was benched out of the pool.
fn author_identities(excluded: &[&str], registry: &[crate::AgenticCli]) -> Vec<String> {
    excluded
        .iter()
        .map(|k| crate::validator::excluded_identity(k, registry))
        .collect()
}

/// `seats` minus every seat that is not itself an author key but invokes an author's binary
/// (`author_ids`, resolved against the full registry). The authors' own seats stay: the rotation
/// anchors on them and excludes them by identity itself.
fn without_author_binaries(
    seats: &[crate::AgenticCli],
    excluded: &[&str],
    author_ids: &[String],
) -> Vec<crate::AgenticCli> {
    seats
        .iter()
        .filter(|c| {
            excluded.contains(&c.key.as_str())
                || !author_ids.contains(&crate::validator::seat_identity(c))
        })
        .cloned()
        .collect()
}

/// Whether a pinned judge can come only from seats the run did not configure: the run's own
/// unbenched seats (`run_unbenched`, authors' binaries removed) hold no distinct judge.
fn outside_seats_only(excluded: &[&str], run_unbenched: &[crate::AgenticCli]) -> bool {
    !crate::validator::distinct_judge_available(excluded, run_unbenched)
}

/// The roster a judge rotation walks: the run's configured seats when one of them is distinct from
/// the work's authors (`excluded`), else the whole ordered pool ([`judge_pool`]: configured
/// first, then the registry's other seats). Keeping the rotation on the configured seats whenever
/// they suffice is what makes an outside seat a last resort rather than a rotation peer.
fn judge_roster<'a>(
    excluded: &[&str],
    configured: &'a [crate::AgenticCli],
    all: &'a [crate::AgenticCli],
) -> &'a [crate::AgenticCli] {
    if crate::validator::distinct_judge_available(excluded, configured) {
        configured
    } else {
        all
    }
}

/// (rejudge) The seats a unit's last attempt's TEAM LEDGER names as authors or corroborators of a
/// finding — the monitors its judge never is (DES-001 §6.2). Empty off the bus transport.
pub(crate) fn unit_team_monitors(unit: &crate::domain::WorkUnit) -> Vec<String> {
    let mut out: Vec<String> = unit
        .team
        .as_ref()
        .filter(|t| t.transport == crate::team::events::Transport::Bus)
        .and_then(|t| t.ledger.as_ref())
        .map(crate::team::runner::ledger_authors)
        .unwrap_or_default();
    // …and those of the seat attempt an earlier re-judge carried: a re-judge attempt's own team
    // snapshot is fresh, so its ledger no longer names the monitors of the work still judged.
    for m in unit
        .requested_rejudge
        .iter()
        .flat_map(|r| r.monitors.iter())
    {
        if !out.contains(m) {
            out.push(m.clone());
        }
    }
    out
}

/// (rejudge) The judge seats the engine's own selection would draw from NOW for `unit`'s gate, and
/// every run seat it would not, with why — the readiness of a [`crate::workflow::HumanDecision::Rejudge`].
/// The same exclusions the judge applies ([`run_unit_and_judge_on`]): the work's authors and the
/// launch exclusion, the team monitors its ledger names, the pinned validator's author on a pinned
/// gate, an evaluator unit's creators (re-admitted only when nothing else is left, as the rotation
/// does), seats that share an author's binary, the run's bench and seats the launcher found
/// unusable. When every distinct preferred seat is benched, the fallback seats that may judge in
/// their place ([`fallback_candidates`]) are the eligible ones; when a preferred seat is eligible
/// they follow it (a preferred judge that fails as a seat falls back to them). Pure over its
/// inputs: `roster` is [`crate::registry_roster`], `pool` / `registry` the fallback context
/// ([`judge_fallback_pool`] / [`full_registry`]).
/// The judge pool's reason for a seat recorded as having built the work.
const BUILT_THIS_WORK: &str = "built this work";

#[allow(clippy::too_many_arguments)]
pub(crate) fn rejudge_seats(
    unit: &crate::domain::WorkUnit,
    roster: &[crate::AgenticCli],
    run_roster: &[String],
    bench: &[crate::domain::BenchedSeat],
    work_authors: &[String],
    author_binaries: Option<&[String]>,
    pool: &[crate::AgenticCli],
    registry: &[crate::AgenticCli],
) -> (Vec<String>, Vec<crate::MissingJudgeSeat>) {
    use crate::team::runner::seat_key;
    // (PRODUCT-WORKS J4) A seat whose QUOTA bench has fallen due is a candidate: the re-judge's
    // attempt re-probes it before its judge reads the bench, and seats it when it answers.
    let bench = crate::domain::ungoverned_bench(&crate::seat_bench::without_due(
        bench,
        crate::seat_bench::now_ms(),
    ));
    let benched: Vec<String> = bench.iter().map(|b| b.cli.clone()).collect();
    let pinned = unit.validator.as_ref().is_some_and(|v| v.approved);
    let work_author = unit
        .assigned_cli
        .clone()
        .unwrap_or_else(|| crate::validator::DETERMINISTIC_VALIDATOR_SEAT.to_string());
    // (party, why) — instance and cli key, first reason wins.
    let mut parties: Vec<(String, &'static str)> = Vec::new();
    fn party(parties: &mut Vec<(String, &'static str)>, k: &str, why: &'static str) {
        for key in [k, crate::team::runner::seat_key(k)] {
            if !key.is_empty() && !parties.iter().any(|(p, _)| p == key) {
                parties.push((key.to_string(), why));
            }
        }
    }
    // The rotation anchors on the first excluded key: the pinned validator's author leads on a
    // pinned gate, the work's author otherwise — as the judge's own `excluded` is built.
    if pinned {
        party(
            &mut parties,
            crate::validator::DETERMINISTIC_VALIDATOR_SEAT,
            "authors the pinned validator, so it never judges against it",
        );
    }
    // (core#1143) The unit's OWN seat never judges its own output — but it built the work only
    // when the unit is one that builds (a Build/Recon author or a Creator). An evaluator's seat
    // wrote the review; a neutral test step's seat ran it. Rig run 85ddad0d's review gate said
    // "Codex (built this work)" of the seat that wrote the review while Claude built the fix.
    // `authors_by_dispatch`, not `authors_work`: a read step with a recorded floor-fix author
    // counts as authoring work, but its own seat wrote nothing (xreview on #1147).
    let own_why: &'static str = if unit.role == crate::workflow::PhaseRole::Creator
        || crate::pipeline::authors_by_dispatch(unit)
    {
        BUILT_THIS_WORK
    } else if unit.role == crate::workflow::PhaseRole::Evaluator {
        "wrote this review, so it never judges its own verdict"
    } else {
        "ran this step, so it never judges its own output"
    };
    party(&mut parties, &work_author, own_why);
    for a in &unit.authored_by {
        party(&mut parties, a, BUILT_THIS_WORK);
    }
    // A seat that built the work (it is recorded as this unit's author) is said to have built it,
    // whatever else it is; the unit's own seat is said to be what it is here.
    let built = |k: &str| unit.authored_by.iter().any(|a| a == k || seat_key(a) == k);
    for (k, why) in parties.iter_mut() {
        if built(k) {
            *why = BUILT_THIS_WORK;
        } else if *k == work_author || seat_key(k) == seat_key(&work_author) {
            *why = own_why;
        }
    }
    for a in &unit.exclude_seats {
        party(&mut parties, a, "excluded from judging this step at launch");
    }
    let monitors_from = parties.len();
    for a in &unit_team_monitors(unit) {
        party(
            &mut parties,
            a,
            "monitored this step for the team, so it never grades its own findings",
        );
    }
    let mut base_len = parties.len();
    // (core#954) An evaluator unit's judge excludes the work's creators — and the binaries they
    // ran — unless that leaves no preferred seat; then the rotation re-admits them.
    let evaluator = unit.role == crate::workflow::PhaseRole::Evaluator;
    if evaluator {
        for a in work_authors {
            party(&mut parties, a, "created the work this review reads");
        }
    }
    let creator_bins: Vec<String> = if evaluator && parties.len() > base_len {
        author_binaries.map(<[String]>::to_vec).unwrap_or_default()
    } else {
        Vec::new()
    };
    let unbenched: Vec<crate::AgenticCli> = judge_pool(roster, run_roster)
        .into_iter()
        .filter(|c| c.health.as_ref().is_none_or(|h| h.usable))
        .cloned()
        .collect();
    let eligible_pool: Vec<crate::AgenticCli> = unbenched
        .iter()
        .filter(|c| !benched.iter().any(|b| b == &c.key))
        .cloned()
        .collect();
    // The preferred judges (the run's own seats, bench applied), the distinct seats the bench took
    // out, and the authors' identities — as the rotation computes them.
    let preferred_for = |parties: &[(String, &'static str)], bins: &[String]| {
        let excluded: Vec<&str> = parties.iter().map(|(p, _)| p.as_str()).collect();
        let mut ids = author_identities(&excluded, roster);
        ids.extend(bins.iter().cloned());
        let preferred = crate::validator::distinct_judge_keys(
            &excluded,
            &without_author_binaries(&eligible_pool, &excluded, &ids),
        );
        let lost: Vec<String> = crate::validator::distinct_judge_keys(
            &excluded,
            &without_author_binaries(&unbenched, &excluded, &ids),
        )
        .into_iter()
        .filter(|k| !preferred.contains(k))
        .collect();
        (preferred, lost, ids)
    };
    let (mut preferred, mut lost, mut ids) = preferred_for(&parties, &creator_bins);
    // (crew#972) As the rotation does ([`exclude_monitors`]): with no judge distinct from the
    // monitors and the creators, the monitors are re-admitted first.
    if preferred.is_empty() && base_len > monitors_from {
        parties.drain(monitors_from..base_len);
        base_len = monitors_from;
        // A creator that also monitored was deduplicated into the monitor slice: it stays out as a
        // creator, with its binaries (xreview on #1165).
        let mut bins = creator_bins.clone();
        if evaluator {
            let before = parties.len();
            for a in work_authors {
                party(&mut parties, a, "created the work this review reads");
            }
            if parties.len() > before && bins.is_empty() {
                bins = author_binaries.map(<[String]>::to_vec).unwrap_or_default();
            }
        }
        (preferred, lost, ids) = preferred_for(&parties, &bins);
    }
    if preferred.is_empty() && parties.len() > base_len {
        parties.truncate(base_len);
        (preferred, lost, ids) = preferred_for(&parties, &[]);
    }
    // A FALLBACK judge stands in when the preferred ones are benched or fail as seats — never when
    // no distinct seat ever existed (the judge is skipped), and never when an author's binary is
    // unknown (`fallback_ids` in the rotation: no seat can be shown distinct from it).
    let fallback: Vec<String> = match author_binaries {
        Some(bins)
            if !(preferred.is_empty() && lost.is_empty())
                && crate::pipeline::author_binaries_known(unit) =>
        {
            let excluded: Vec<&str> = parties.iter().map(|(p, _)| p.as_str()).collect();
            let mut fids = ids.clone();
            fids.extend(bins.iter().cloned());
            fids.extend(unit.authored_binaries.iter().cloned());
            if let Some(k) = unit.assigned_cli.as_deref() {
                fids.push(launched_identity(k, unit.assigned_invocation.clone()));
            }
            let fparties = fallback_parties(&excluded, work_authors);
            fallback_candidates(pool, registry, &fparties, &fids, &benched, &[])
                .unwrap_or_default()
                .into_iter()
                .map(|c| c.key)
                .filter(|k| !preferred.contains(k))
                .collect()
        }
        _ => Vec::new(),
    };
    let mut seats = preferred;
    seats.extend(fallback);
    let missing = run_roster
        .iter()
        .filter(|k| !seats.contains(k))
        .map(|k| {
            let reason = if let Some((_, why)) = parties
                .iter()
                .find(|(p, _)| p == k || p.as_str() == seat_key(k))
            {
                (*why).to_string()
            } else if let Some(why) = bench_reason(&bench, k) {
                format!("benched for this run: {}", short_reason(why))
            } else if let Some(h) = roster
                .iter()
                .find(|c| c.key == *k || c.key == seat_key(k))
                .and_then(|c| c.health.as_ref())
                .filter(|h| !h.usable)
            {
                format!(
                    "not usable now: {}",
                    h.reason
                        .as_deref()
                        .unwrap_or("the launcher found it unusable")
                )
            } else if !roster.iter().any(|c| c.key == *k || c.key == seat_key(k)) {
                "not a judge seat on this computer (no registry record)".to_string()
            } else {
                "runs the same CLI as a party to this work".to_string()
            };
            crate::MissingJudgeSeat {
                seat: k.clone(),
                reason,
            }
        })
        .collect();
    (seats, missing)
}

/// (core#772) When NO identity-distinct judge remains on the run's unbenched roster but one
/// WOULD exist without the run's bench, the judge is unavailable because of SEAT FAILURES earlier
/// in the run (a quota / sign-in / empty answer while judging) — not because the run never had
/// a distinct seat. That is a fail-closed seat-failure verdict naming the benched seats (booked
/// `judge_unavailable`, the human gate opens), never a skipped judge: skipping would let the
/// retry after a judge's quota refusal pass on the deterministic floor alone. `None` when a
/// distinct judge is still available, or when none ever was (the pre-existing UNGATED path).
fn benched_judge_unavailable(
    who: &str,
    excluded: &[&str],
    roster: &[crate::AgenticCli],
    unbenched: &[crate::AgenticCli],
    benched: &[String],
    bench: &[crate::domain::BenchedSeat],
) -> Option<BenchedJudge> {
    if benched.is_empty() || crate::validator::distinct_judge_available(excluded, roster) {
        return None;
    }
    let lost_keys: Vec<String> = crate::validator::distinct_judge_keys(excluded, unbenched)
        .into_iter()
        .filter(|k| benched.iter().any(|b| b == k))
        .collect();
    if lost_keys.is_empty() {
        return None;
    }
    let lost: Vec<String> = lost_keys
        .iter()
        .map(|k| match bench_reason(bench, k) {
            // (operator ruling 2026-10-14) The bench's own words when the run recorded them: a
            // launcher bench (codex's worktree bench) is not "a seat failure".
            Some(why) => format!("{k} (benched for this run: {})", short_reason(why)),
            None => format!("{k} (benched for this run after a seat failure)"),
        })
        .collect();
    Some(BenchedJudge {
        held: crate::validator::AgentVerdict::from_judge_error(
            who,
            crate::validator::JudgeUnavailable {
                refusals: lost.clone(),
            }
            .into(),
        ),
        lost_keys,
        lost,
    })
}

/// [`benched_judge_unavailable`]'s hold: the seat-failure verdict, and the benched preferred
/// judges it names (keys, and the refusal lines the verdict quotes).
struct BenchedJudge {
    held: crate::validator::AgentVerdict,
    lost_keys: Vec<String>,
    lost: Vec<String>,
}

/// (operator ruling 2026-10-14) The seats that may judge IN PLACE of the run's preferred judge
/// when every preferred judge seat is benched for the run or fails as a seat while judging, in
/// preference order: Claude Code, then Antigravity. `WICKED_JUDGE_FALLBACK` (comma-separated
/// keys, read at call time) narrows or reorders the list — only these two keys are honoured; empty or
/// `off` disables the fallback, which keeps the pre-ruling hold (`judge_unavailable`).
pub(crate) const JUDGE_FALLBACK_SEATS: &[&str] = &["claude", "agy"];

/// The judge's run context beyond its roster (operator ruling 2026-10-14): the fallback seats'
/// registry records (`pool`, [`judge_fallback_pool`]), the FULL registry they and every party's
/// identity are resolved against (`registry`), the run's bench WITH its reasons
/// (`AgentSession::benched_seats`, named in the disclosure), and the authors of the work an
/// evaluator unit's judge reads (`work_authors`, [`crate::pipeline::work_authors`]: the creator
/// never judges its own work). The default (all empty) is "no fallback, no extra authors": the bus
/// worker, which knows none of them, and the tests that pin the pre-ruling paths.
#[derive(Clone, Copy, Default)]
pub(crate) struct JudgeFallbackCtx<'a> {
    pub pool: &'a [crate::AgenticCli],
    pub registry: &'a [crate::AgenticCli],
    pub bench: &'a [crate::domain::BenchedSeat],
    pub work_authors: &'a [String],
    /// The binaries those authors actually ran ([`crate::pipeline::work_author_binaries`]); `None`
    /// when one cannot be established — then no fallback judge is seated.
    pub author_binaries: Option<&'a [String]>,
    /// (PRODUCT-WORKS J4) Re-probe the seats of `bench` whose QUOTA bench has fallen due, against
    /// `registry`, right before the judge reads the bench: after the unit's own work, so a bench
    /// that fell due while the worker ran is asked too ([`reprobe_due_benches`]). The outcomes ride
    /// [`crate::workflow::UnitEvidence::bench_reprobes`]. `false` (the default): no probe.
    pub reprobe: bool,
}

/// (codex r1 on the fallback) The parties a FALLBACK judge must be distinct from: the judge's own
/// exclusion plus every seat that authored the work an evaluator unit's judge reads
/// (`work_authors`, instance and cli key). The run's own rotation keeps its pre-ruling exclusion.
fn fallback_parties<'a>(excluded: &[&'a str], work_authors: &'a [String]) -> Vec<&'a str> {
    let mut out: Vec<&'a str> = excluded.to_vec();
    for a in work_authors {
        for k in [a.as_str(), wicked_apps_core::spawn::seat_cli_key(a)] {
            if !out.contains(&k) {
                out.push(k);
            }
        }
    }
    out
}

/// (core#954, operator decision 2026-10-17) Widen an evaluator unit's judge exclusion with the
/// work's `creators` when a judge distinct from all of them remains (`has_judge`, asked of the
/// widened set over the path's own eligible seats). When none does, `excluded` is left as it was
/// and the reason is returned: the rotation then judges as before, and a creator that judges is
/// disclosed ([`disclose_creator_judge`]) — never silently.
fn exclude_creators<'a>(
    excluded: &mut Vec<&'a str>,
    creators: &'a [String],
    has_judge: impl Fn(&[&str]) -> bool,
) -> Option<String> {
    if creators.is_empty() {
        return None;
    }
    let mut widened = excluded.clone();
    widened.extend(
        creators
            .iter()
            .map(String::as_str)
            .filter(|c| !excluded.contains(c)),
    );
    if has_judge(&widened) {
        *excluded = widened;
        return None;
    }
    Some(format!(
        "no eligible judge seat distinct from the work's creators [{}]",
        creators.join(", ")
    ))
}

/// (crew#972) Widen a judge's exclusion with the TEAM MONITORS that raised a finding on this step
/// (DES-001 §6.2: a monitor never grades its own finding) only while a judge distinct from them
/// AND from the work's `creators` remains (`has_judge`, asked of that set over the path's own
/// eligible seats). When none does, `excluded` is left as it was and the reason is returned: a
/// monitor then may judge, disclosed ([`disclose_monitor_judge`]) — never silently. Rig run
/// 115fabdb: after a stall reassign codex -> pi, codex monitored the review and raised findings;
/// with claude the validator author and creator and copilot benched, no judge was left and the
/// operator had to judge the gate although codex was signed in and had built nothing.
fn exclude_monitors<'a>(
    excluded: &mut Vec<&'a str>,
    monitors: &'a [String],
    creators: &[String],
    has_judge: impl Fn(&[&str]) -> bool,
) -> Option<String> {
    let extra: Vec<&'a str> = monitors
        .iter()
        .map(String::as_str)
        .filter(|m| !excluded.contains(m))
        .collect();
    if extra.is_empty() {
        return None;
    }
    let mut widened = excluded.clone();
    widened.extend(extra.iter().copied());
    let mut probe: Vec<&str> = widened.clone();
    probe.extend(
        creators
            .iter()
            .map(String::as_str)
            .filter(|c| !widened.contains(c)),
    );
    if has_judge(&probe) {
        *excluded = widened;
        return None;
    }
    let mut names: Vec<&str> = Vec::new();
    for m in &extra {
        let k = crate::team::runner::seat_key(m);
        if !names.contains(&k) {
            names.push(k);
        }
    }
    Some(format!(
        "no eligible judge seat distinct from the team monitors that raised findings on this step [{}]",
        names.join(", ")
    ))
}

/// (crew#972) A verdict rendered by a team monitor that [`exclude_monitors`] re-admitted: labelled
/// on the gate, its receipt and the delivery receipt as a [`crate::assurance::JudgeFallback`] (no
/// preferred seat — there was none) and named in the run's live output. The monitor is still a
/// seat distinct from the work's authors, so `judgeDistinct` is left as the rotation set it. A
/// verdict another seat rendered, or one a fallback already labels, passes through.
fn disclose_monitor_judge(
    mut av: crate::validator::AgentVerdict,
    monitor_fallback: Option<&str>,
    monitors: &[String],
    roster: &[crate::AgenticCli],
    emit_delta: &DeltaSink,
) -> crate::validator::AgentVerdict {
    let (Some(why), Some(seat), None) =
        (monitor_fallback, av.judge_cli.clone(), &av.judge_fallback)
    else {
        return av;
    };
    let cli = crate::team::runner::seat_key(&seat);
    // By key AND by the identity the seat runs, as the exclusion matched it (xreview on #1165): a
    // judge registered under another key that invokes a monitor's CLI is that monitor.
    let keys: Vec<&str> = monitors.iter().map(String::as_str).collect();
    let ids = author_identities(&keys, roster);
    if !monitors
        .iter()
        .any(|m| m == &seat || crate::team::runner::seat_key(m) == cli)
        && !ids.contains(&crate::validator::excluded_identity(&seat, roster))
    {
        return av;
    }
    let fb = crate::assurance::JudgeFallback {
        label: format!("judge: {seat} (team-monitor fallback; {why})"),
        seat,
        preferred: Vec::new(),
        reason: why.to_string(),
    };
    engine_note(emit_delta, &fb.label);
    eprintln!("wicked-core: {} (crew#972)", fb.label);
    av.judge_fallback = Some(fb);
    av
}

/// (core#954) Who the work's creators are, for [`disclose_creator_judge`]: their seat keys (instance
/// and cli key) and every identity they ran — resolved from the registry and recorded at dispatch.
struct CreatorIds<'a> {
    keys: &'a [String],
    ids: Vec<String>,
    roster: &'a [crate::AgenticCli],
}

impl<'a> CreatorIds<'a> {
    fn of(keys: &'a [String], binaries: &[String], roster: &'a [crate::AgenticCli]) -> Self {
        let k: Vec<&str> = keys.iter().map(String::as_str).collect();
        let mut ids = author_identities(&k, roster);
        ids.extend(binaries.iter().cloned());
        CreatorIds { keys, ids, roster }
    }

    fn names(&self, seat: &str) -> bool {
        let cli = wicked_apps_core::spawn::seat_cli_key(seat);
        self.keys.iter().any(|k| k == seat || k == cli)
            || self
                .ids
                .contains(&crate::validator::excluded_identity(seat, self.roster))
    }
}

/// (core#954) A verdict the rotation rendered after [`exclude_creators`] found no judge distinct
/// from the work's creators: labelled on the gate, its receipt and the delivery receipt as a
/// [`crate::assurance::JudgeFallback`] (no preferred seat — there was none), `judgeDistinct: false`,
/// and named in the run's live output. A verdict no seat rendered, or one a fallback seat already
/// labels, passes through.
fn disclose_creator_judge(
    mut av: crate::validator::AgentVerdict,
    seat: Option<String>,
    creator_fallback: Option<&str>,
    creators: &CreatorIds<'_>,
    emit_delta: &DeltaSink,
) -> crate::validator::AgentVerdict {
    let (Some(why), Some(seat), None) = (creator_fallback, seat, &av.judge_fallback) else {
        return av;
    };
    if !creators.names(&seat) {
        return av;
    }
    let fb = crate::assurance::JudgeFallback {
        label: format!("judge: {seat} (creator fallback; {why})"),
        seat,
        preferred: Vec::new(),
        reason: why.to_string(),
    };
    engine_note(emit_delta, &fb.label);
    eprintln!("wicked-core: {} (core#954)", fb.label);
    av.judge_distinct = Some(false);
    av.judge_fallback = Some(fb);
    av
}

/// The normalized identity of the binary a launch of `key` with `invocation` actually runs: the
/// launcher's own resolution ([`crate::execute_wrapped::launch_invocation`], then its quote-aware
/// [`crate::execute_wrapped::template_binary`]), basename + case-folded.
pub(crate) fn launched_identity(key: &str, invocation: Option<String>) -> String {
    invocation_identity(&crate::execute_wrapped::launch_invocation(key, invocation))
}

/// (codex r6) EVERY identity a launch of `key` with `invocation` may execute: the wrapped
/// invocation's binary ([`launched_identity`]) and its registry record's other carriers — the
/// `binary` field and the ACP adapter's CLI ([`record_identities`]) — so an author is excluded by
/// whichever carrier actually ran it.
pub(crate) fn launched_identities(key: &str, invocation: Option<String>) -> Vec<String> {
    let mut out = vec![launched_identity(key, invocation)];
    let registry = full_registry();
    let cli = wicked_apps_core::spawn::seat_cli_key(key);
    if let Some(rec) = registry
        .iter()
        .find(|c| c.key == key)
        .or_else(|| registry.iter().find(|c| c.key == cli))
    {
        for id in record_identities(rec) {
            if !out.contains(&id) {
                out.push(id);
            }
        }
    }
    out
}

/// Every identity a registry record may execute: its headless invocation's binary (quote-aware),
/// its `binary` field, and its ACP adapter's CLI (`claude-agent-acp` → `claude`, `codex-acp` →
/// `codex`, `agy-acp` → `agy`).
pub(crate) fn record_identities(c: &crate::AgenticCli) -> Vec<String> {
    let mut out = vec![invocation_identity(&c.headless_invocation)];
    let bin = crate::validator::normalize_identity(&c.binary);
    if !bin.is_empty() && !out.contains(&bin) {
        out.push(bin);
    }
    if let Some(a) = c.acp.as_ref() {
        let b = crate::validator::normalize_identity(&a.binary);
        let cli = b
            .strip_suffix("-agent-acp")
            .or_else(|| b.strip_suffix("-acp"))
            .unwrap_or(&b)
            .to_string();
        if !cli.is_empty() && !out.contains(&cli) {
            out.push(cli);
        }
    }
    out
}

/// The normalized identity of the binary an invocation template runs, read quote-aware as the
/// launcher reads it ([`crate::execute_wrapped::template_binary`]: `"/opt/Anti Gravity/agy" -p`
/// is `agy`), basename + case-folded.
pub(crate) fn invocation_identity(invocation: &str) -> String {
    crate::validator::normalize_identity(&crate::execute_wrapped::template_binary(invocation))
}

/// The fallback seat keys in force: [`JUDGE_FALLBACK_SEATS`], or `WICKED_JUDGE_FALLBACK`.
pub(crate) fn judge_fallback_keys() -> Vec<String> {
    judge_fallback_keys_from(std::env::var("WICKED_JUDGE_FALLBACK").ok().as_deref())
}

fn judge_fallback_keys_from(env: Option<&str>) -> Vec<String> {
    match env.map(str::trim) {
        None => JUDGE_FALLBACK_SEATS.iter().map(|k| k.to_string()).collect(),
        Some(v) if v.is_empty() || v.eq_ignore_ascii_case("off") => Vec::new(),
        // Only the ruling's seats, narrowed or reordered — never another key (codex r5).
        Some(v) => v
            .split(',')
            .map(str::trim)
            .filter(|k| JUDGE_FALLBACK_SEATS.contains(k))
            .map(str::to_string)
            .collect(),
    }
}

/// The FULL registry (built-ins merged with the user's `clis.toml`), unfiltered: what the
/// fallback seats and every party's identity are resolved against.
pub(crate) fn full_registry() -> Vec<crate::AgenticCli> {
    // The same user path the execution carriers resolve (HOME, then USERPROFILE), so the records
    // checked are the records run (codex r7).
    let user = wicked_council::registry::default_user_path();
    wicked_council::registry::load(user.as_deref()).unwrap_or_default()
}

/// The fallback seats' records from `registry` (the full one), in fallback order. Unfiltered on
/// purpose: Antigravity's built-in record is neither council- nor work-enabled, so
/// [`crate::registry_roster`] drops it, and the operator ruling seats it as a JUDGE only (a fenced
/// verdict prompt, not governed work on the tree).
pub(crate) fn judge_fallback_pool(registry: &[crate::AgenticCli]) -> Vec<crate::AgenticCli> {
    judge_fallback_keys()
        .iter()
        .filter_map(|k| registry.iter().find(|c| &c.key == k).cloned())
        .collect()
}

/// The recorded bench reason for `key` (its own key or its CLI's), if the run recorded one.
fn bench_reason<'a>(bench: &'a [crate::domain::BenchedSeat], key: &str) -> Option<&'a str> {
    bench
        .iter()
        .find(|b| b.cli == key)
        .or_else(|| {
            bench
                .iter()
                .find(|b| wicked_apps_core::spawn::seat_cli_key(&b.cli) == key)
        })
        .map(|b| b.reason.as_str())
}

/// A bench reason's headline for the disclosure line: the text before its first `: ` (the
/// worktree bench's "codex cannot work in this run's worktree on this host"), at most 160 chars.
/// The full reason stays on [`crate::assurance::JudgeFallback::reason`].
fn short_reason(why: &str) -> String {
    let head = why.split(": ").next().unwrap_or(why).trim();
    let mut out: String = head.chars().take(160).collect();
    if head.chars().count() > 160 {
        out.push('…');
    }
    out
}

/// The fallback seats that may judge THIS unit: in the pool, not ballot-only (core#572: judging
/// is work, and a seat recorded as ballot-only never does it), with an invocation, not declared
/// unusable by the launcher, not benched for the run (any instance), not already tried by the
/// rotation, and identity-distinct from every party — the creator, every author, the evaluator,
/// the validator author, the team and the launch exclusion (`excluded`), each resolved to the
/// binary it runs against the full `registry` (and the pool), plus `extra_ids` (the authors'
/// identities as the judge rotation resolved them, the unit's own dispatched invocation). Each is
/// marked work-eligible for the judge rotation only.
///
/// `None` — fail closed, no fallback — when a party's key resolves to NO registry record: its
/// binary is unknown (an ad-hoc launch seat, an alias outside the registry), so no seat can be
/// shown distinct from it.
fn fallback_candidates(
    pool: &[crate::AgenticCli],
    registry: &[crate::AgenticCli],
    excluded: &[&str],
    extra_ids: &[String],
    benched: &[String],
    tried: &[String],
) -> Option<Vec<crate::AgenticCli>> {
    let mut ids: Vec<String> = extra_ids.to_vec();
    for k in excluded {
        let cli = wicked_apps_core::spawn::seat_cli_key(k);
        let rec = registry
            .iter()
            .chain(pool)
            .find(|c| c.key == *k)
            .or_else(|| registry.iter().chain(pool).find(|c| c.key == cli));
        match rec {
            Some(c) => ids.extend(record_identities(c)),
            None => {
                eprintln!(
                    "wicked-core: no fallback judge — party `{k}` names no registry seat, so its \
                     binary is unknown and no seat can be shown distinct from it (operator ruling \
                     2026-10-14)"
                );
                return None;
            }
        }
    }
    Some(
        pool.iter()
            // (codex r5) The record must run the CLI its key names on EVERY carrier the runner may
            // pick — its headless invocation, its `binary`, and its ACP adapter (`claude-agent-acp`
            // runs claude, `agy-acp` runs agy) — so the identity checked is the identity executed.
            // An override record that names another binary is no fallback seat.
            .filter(|c| record_runs_its_key(c))
            .filter(|c| !c.enabled_for_council || c.seat_eligible_for_work)
            .filter(|c| !c.headless_invocation.trim().is_empty())
            .filter(|c| c.health.as_ref().is_none_or(|h| h.usable))
            .filter(|c| {
                // Any instance of a benched CLI is benched (codex r3).
                let cli = wicked_apps_core::spawn::seat_cli_key(&c.key);
                !benched
                    .iter()
                    .any(|b| wicked_apps_core::spawn::seat_cli_key(b) == cli)
            })
            .filter(|c| !tried.iter().any(|t| t == &c.key))
            .filter(|c| {
                // Never an instance of a party's CLI, whatever it invokes (codex r4).
                let cli = wicked_apps_core::spawn::seat_cli_key(&c.key);
                !excluded
                    .iter()
                    .any(|k| wicked_apps_core::spawn::seat_cli_key(k) == cli)
            })
            .filter(|c| {
                // Both spellings: the launcher's quote-aware read and the rotation's (codex r3).
                !ids.contains(&invocation_identity(&c.headless_invocation))
                    && !ids.contains(&crate::validator::seat_identity(c))
            })
            .map(|c| {
                let mut c = c.clone();
                c.seat_eligible_for_work = true;
                c
            })
            .collect(),
    )
}

/// (codex r5) Whether a fallback seat's registry record runs the CLI its key names on every
/// carrier: its headless invocation's binary, its `binary` field and (when set) its ACP adapter
/// binary all resolve to the key's CLI (`claude`, `agy`; an adapter is `<cli>-acp` or
/// `<cli>-agent-acp`).
fn record_runs_its_key(c: &crate::AgenticCli) -> bool {
    let cli = wicked_apps_core::spawn::seat_cli_key(&c.key).to_ascii_lowercase();
    let adapter_ok = c.acp.as_ref().is_none_or(|a| {
        let b = crate::validator::normalize_identity(&a.binary);
        b == format!("{cli}-acp") || b == format!("{cli}-agent-acp")
    });
    invocation_identity(&c.headless_invocation) == cli
        && crate::validator::normalize_identity(&c.binary) == cli
        && adapter_ok
}

/// (codex r1 on the fallback) Whether a rotation's `JudgeUnavailable` was made ONLY of genuine
/// seat refusals: every failure reported with its output (`refused` covers `refusals`), and no
/// output names a verdict. A seat that exited non-zero AFTER writing `REJECT …` answered: asking
/// another judge then would be verdict shopping, so no fallback is seated.
fn only_seat_refusals(refusals: &[String], refused: &[(String, String)]) -> bool {
    refused.len() >= refusals.len()
        && refused
            .iter()
            .all(|(_, out)| !crate::validator::names_a_verdict(out))
}

/// Why the preferred judges are out, for the disclosure: `"<seat> benched: <reason>"` per seat
/// (the run's recorded bench reason, else "a seat failure earlier in the run").
fn benched_preferred_reason(lost: &[String], bench: &[crate::domain::BenchedSeat]) -> String {
    lost.iter()
        .map(|k| match bench_reason(bench, k) {
            Some(why) => format!("{k} benched: {}", short_reason(why)),
            None => format!("{k} benched: a seat failure earlier in the run"),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// `JudgeUnavailable` refusal lines (`"<seat> (<cause>)"`) as the preferred keys and the
/// disclosure reason (`"<seat> unavailable: <cause>"`).
fn unavailable_preferred(refusals: &[String]) -> (Vec<String>, String) {
    let mut keys = Vec::new();
    let mut parts = Vec::new();
    for r in refusals {
        let (k, cause) = match r.split_once(" (") {
            Some((k, rest)) => (k.to_string(), rest.trim_end_matches(')').to_string()),
            None => (r.clone(), "failed as a seat".to_string()),
        };
        parts.push(format!("{k} unavailable: {cause}"));
        keys.push(k);
    }
    (keys, parts.join("; "))
}

/// Where a judge rotation's `(seat, output)` refusals are reported for the run's bench.
type RefusalSink<'a> = dyn Fn(&[(String, String)]) + 'a;

/// What a fallback judge attempt came to.
enum FallbackJudge {
    /// No fallback seat may judge this unit: the caller keeps the pre-ruling verdict (the hold).
    NoCandidate,
    /// A fallback seat answered: its verdict (labelled), or its verdict-less answer (escalates).
    Answered(crate::validator::AgentVerdict),
    /// Every fallback seat failed as a seat (or errored): the hold stands, naming them too.
    Failed(Vec<String>),
}

/// (operator ruling 2026-10-14) Seat a FALLBACK judge for a unit whose preferred judge seats
/// (`preferred`, why: `reason`) are all benched or failed as seats. Only a seat
/// [`fallback_candidates`] admits is asked; the rotation over them is the ordinary judge
/// rotation (seat refusals reported for the bench, no verdict shopping). A fallback that cannot
/// produce an answer — every candidate refused, timed out or errored — never REJECTs: the
/// caller's hold stands. The answer carries the [`crate::assurance::JudgeFallback`] disclosure,
/// and the run's live output says which seat judged in whose place.
#[allow(clippy::too_many_arguments)]
fn fallback_judge(
    who: &str,
    criterion: &str,
    work: &str,
    excluded: &[&str],
    candidates: &[crate::AgenticCli],
    preferred: Vec<String>,
    reason: String,
    runner: &dyn StepRunner,
    note_refusals: &RefusalSink<'_>,
    emit_delta: &DeltaSink,
) -> FallbackJudge {
    if candidates.is_empty() {
        return FallbackJudge::NoCandidate;
    }
    let names = crate::validator::distinct_judge_keys(excluded, candidates);
    if names.is_empty() {
        return FallbackJudge::NoCandidate;
    }
    engine_note(
        emit_delta,
        &format!(
            "{who}: the run's judge is out ({reason}) — asking a fallback judge seat ({})",
            names.join(", ")
        ),
    );
    let label = |seat: &str| {
        format!(
            "judge: {seat} (fallback; {})",
            if reason.is_empty() {
                format!("{} unavailable", preferred.join(", "))
            } else {
                reason.clone()
            }
        )
    };
    let disclosure = |seat: &str| crate::assurance::JudgeFallback {
        seat: seat.to_string(),
        preferred: preferred.clone(),
        reason: reason.clone(),
        label: label(seat),
    };
    // One seat at a time: the next is asked only after a GENUINE seat refusal
    // ([`only_seat_refusals`]); a seat that wrote a verdict and exited non-zero ends the fallback
    // with the hold (no verdict shopping).
    let mut failures: Vec<String> = Vec::new();
    for key in &names {
        let Some(seat) = candidates.iter().find(|c| &c.key == key) else {
            continue;
        };
        let (verdict, refused) = crate::validator::agent_validate_with_refusals(
            criterion,
            work,
            excluded,
            std::slice::from_ref(seat),
            runner,
        );
        note_refusals(&refused);
        match verdict {
            Ok(mut av) => {
                let fb = disclosure(av.judge_cli.as_deref().unwrap_or(key));
                engine_note(emit_delta, &fb.label);
                eprintln!("wicked-core: {} (operator ruling 2026-10-14)", fb.label);
                av.judge_fallback = Some(fb);
                return FallbackJudge::Answered(av);
            }
            Err(e) => {
                if let Some(nv) = e.downcast_ref::<crate::validator::JudgeNoVerdict>() {
                    // An answer without a verdict escalates, labelled — never a re-ask.
                    let fb = disclosure(&nv.seat);
                    engine_note(emit_delta, &fb.label);
                    let mut av = crate::validator::AgentVerdict::from_judge_error(who, e);
                    av.reasoning = format!("{} — {}", fb.label, av.reasoning);
                    av.judge_fallback = Some(fb);
                    return FallbackJudge::Answered(av);
                }
                match e.downcast_ref::<crate::validator::JudgeUnavailable>() {
                    Some(u) => {
                        failures.extend(u.refusals.iter().map(|r| format!("{r} — fallback judge")));
                        if !only_seat_refusals(&u.refusals, &refused) {
                            // It answered something and exited non-zero: stop, hold.
                            return FallbackJudge::Failed(failures);
                        }
                    }
                    None => {
                        failures.push(format!("fallback judge {key} could not judge ({e})"));
                        return FallbackJudge::Failed(failures);
                    }
                }
            }
        }
    }
    FallbackJudge::Failed(failures)
}

/// The hold a fallback could not lift: `held` (the pre-ruling seat-failure verdict) with the
/// fallback seats' own failures appended to what it names.
fn held_with_fallback_failures(
    who: &str,
    held: crate::validator::AgentVerdict,
    preferred_refusals: Vec<String>,
    fallback: FallbackJudge,
) -> crate::validator::AgentVerdict {
    match fallback {
        FallbackJudge::NoCandidate => held,
        FallbackJudge::Answered(av) => av,
        FallbackJudge::Failed(more) => {
            let mut refusals = preferred_refusals;
            refusals.extend(more);
            crate::validator::AgentVerdict::from_judge_error(
                who,
                crate::validator::JudgeUnavailable { refusals }.into(),
            )
        }
    }
}

/// (operator ruling 2026-10-14) A judge rotation's error: when every seat it walked failed AS A
/// SEAT ([`crate::validator::JudgeUnavailable`]), a fallback seat the rotation did not walk
/// judges in their place; any other error keeps its pre-ruling shape.
#[allow(clippy::too_many_arguments)]
fn judge_error_or_fallback(
    who: &str,
    e: anyhow::Error,
    refused: &[(String, String)],
    criterion: &str,
    work: &str,
    excluded: &[&str],
    extra_ids: Option<Vec<String>>,
    walked: &[crate::AgenticCli],
    benched: &[String],
    fallback: JudgeFallbackCtx<'_>,
    runner: &dyn StepRunner,
    note_refusals: &RefusalSink<'_>,
    emit_delta: &DeltaSink,
) -> crate::validator::AgentVerdict {
    let Some(refusals) = e
        .downcast_ref::<crate::validator::JudgeUnavailable>()
        .map(|u| u.refusals.clone())
    else {
        return crate::validator::AgentVerdict::from_judge_error(who, e);
    };
    let held = crate::validator::AgentVerdict::from_judge_error(who, e);
    if !only_seat_refusals(&refusals, refused) {
        return held;
    }
    let (preferred, reason) = unavailable_preferred(&refusals);
    let tried = crate::validator::distinct_judge_keys(excluded, walked);
    let Some(candidates) = extra_ids.and_then(|ids| {
        fallback_candidates(
            fallback.pool,
            fallback.registry,
            excluded,
            &ids,
            benched,
            &tried,
        )
    }) else {
        return held;
    };
    let fb = fallback_judge(
        who,
        criterion,
        work,
        excluded,
        &candidates,
        preferred,
        reason,
        runner,
        note_refusals,
        emit_delta,
    );
    held_with_fallback_failures(who, held, refusals, fb)
}

#[allow(clippy::too_many_arguments)]
fn run_unit_and_judge_on(
    runner: &Arc<dyn StepRunner>,
    input: &StepInput,
    agent_review_target: Option<&str>,
    emit_delta: &DeltaSink,
    roster: &[crate::AgenticCli],
    run_roster: &[String],
    benched: &[String],
    fallback: JudgeFallbackCtx<'_>,
    exec_bus: Option<&str>,
    team: Option<&crate::team::runner::TeamRunner>,
) -> (
    StepOutput,
    Option<crate::validator::AgentVerdict>,
    crate::workflow::UnitEvidence,
) {
    // (DES-QE-EVERY-SEAT) This attempt's start, before its executor turn: a QE run that turn names
    // must have been created at or after it (no grace; a resumed run is bound by its id instead).
    // Never the worktree baseline, which a resume or redrive keeps from an earlier attempt.
    let qe_not_before = crate::interaction::now_millis();
    // DES-TEAMING-002 T5 — the worker-thread seam, shared by the in-process worker and the bus
    // worker. `step.claimed` goes out BEFORE the turn starts (so it precedes every row of the
    // turn, §4.3); a claim that failed AND could not be tombstoned fails the step here, before
    // any work (fail closed, §4.8 row 5).
    let attempt_team = match crate::team::runner::claim(team, input) {
        Ok(a) => a,
        Err(why) => {
            eprintln!("wicked-core: unit {}: {why}", input.unit.ord);
            let snap = input
                .unit
                .team
                .as_ref()
                .map(|s| crate::team::runner::local_snapshot(s, Some(why.clone())));
            return (
                StepOutput {
                    run_id: input.run_id.clone(),
                    unit_ix: input.unit_ix,
                    attempt: input.attempt,
                    output: why,
                    status: StepStatus::Failed,
                    usage: None,
                    files: Vec::new(),
                    tools: Vec::new(),
                    governed: false,
                },
                None,
                crate::workflow::UnitEvidence {
                    team: snap,
                    ..Default::default()
                },
            );
        }
    };
    // (core#1155) SEAT DEPENDENCIES: before any seat of this attempt works in a JS worktree (the
    // phase's seat, a floor fix's, the QE reviewer, a judge), the engine installs its dependencies
    // there for the platform the seats run on (`crate::seat_deps`): the floor's own install lands
    // in its clean checkout, and a seat cannot install (no registry egress in a container, no
    // install at all on a read-only seat). Cheap once in place (a marker). Not for the pre-build
    // scope unit, which runs no code. The install heartbeats like the floor, so the stall watchdog
    // never reads a slow install as a wedged seat. The seats are told (one context block, on the
    // input every seat below is built from).
    let with_deps: Option<StepInput> = match input.workdir.as_deref() {
        Some(wd) if !input.unit.pre_build_scope && input.unit.tool_cmd.is_none() => {
            let git_dir = input
                .unit
                .worktree_baseline
                .as_ref()
                .and_then(|b| b.git_dir.as_deref())
                .map(std::path::Path::new);
            let provisioned = with_floor_heartbeat(
                emit_delta,
                "dependencies: checking the worktree's node_modules for the seats",
                FLOOR_HEARTBEAT_EVERY,
                || {
                    crate::seat_deps::provision(
                        wd,
                        git_dir,
                        &input.run_id,
                        &crate::seat_deps::SeatPlatform::for_seats(),
                    )
                },
            );
            if let Some(note) = provisioned.note() {
                eprintln!("wicked-core: unit {}: {note}", input.unit.ord);
                engine_note(emit_delta, &note);
            }
            provisioned.seat_context().map(|context| {
                let mut i = input.clone();
                i.prior_outputs.push(crate::workflow::PriorUnitOutput {
                    label: crate::seat_deps::CONTEXT_LABEL.into(),
                    output: context,
                });
                i
            })
        }
        _ => None,
    };
    let input = with_deps.as_ref().unwrap_or(input);
    // The step-boundary injector (§8.9): the advice every carrier receives, as prior context.
    // (ASK-K3c) Whether the boundary's team answers reached a seat turn: the stream read
    // succeeded and the seat ran (not a repo-checks re-run). Stamped on `step.completed`.
    let mut boundary_read = false;
    let advised: Option<StepInput> = match &attempt_team {
        crate::team::runner::Attempt::Claimed(c) => {
            let b = crate::team::runner::boundary(c);
            boundary_read = b.unread.is_none();
            if let Some(why) = &b.unread {
                eprintln!(
                    "wicked-core: unit {}: team advice unavailable at the step boundary ({why}); \
                     undelivered findings reach the gate as not delivered",
                    input.unit.ord
                );
            } else if !b.rendered.is_empty() {
                eprintln!(
                    "wicked-core: unit {}: {} team finding(s) rendered at the step boundary",
                    input.unit.ord,
                    b.rendered.len()
                );
            }
            // The carrier learns the attempt is claimed from its own snapshot: its `step.claimed`
            // event id is the steer point's floor and its checkpoints' licence (DES-002 §4.2).
            let mut i = input.clone();
            if let Some(t) = i.unit.team.as_mut() {
                t.claimed_event_id = Some(c.claimed_id);
            }
            if let Some(block) = b.block {
                i.prior_outputs.push(block);
            }
            Some(i)
        }
        _ => None,
    };
    // (core#469) A FLOOR RE-RUN the operator chose at a timed-out floor's gate: the seat does NOT
    // run again — its reviewed output is handed to the fold as this attempt's output, and only the
    // repository's checks re-run (below) under the chosen mode.
    let floor_rerun: Option<&crate::repo_checks::FloorRerun> = input
        .unit
        .repo_checks
        .as_ref()
        .and_then(|r| r.requested_rerun.as_ref());
    // (core#782) A FLOOR FIX: before the floor, a seat distinct from this read-only phase makes
    // the fix the operator's note asks for — the phase itself does not run again. The rest of
    // this thread then runs on the fixed tree: the guard re-baselined on it (the fix is not the
    // phase's own mutation; anything after it still is), the fixing seat excluded from any judge.
    let fixed: Option<(StepInput, StepOutput)> = floor_rerun
        .and_then(|r| r.fix.as_ref())
        .map(|fix| run_floor_fix(runner, input, fix, emit_delta));
    let input = fixed.as_ref().map(|(i, _)| i).unwrap_or(input);
    // (rejudge) The operator RE-ARMED the judge at this unit's `judge_unavailable` gate: the seat
    // does NOT run again — the output its reviewed attempt captured is handed to the judge, drawn
    // afresh from the run's roster as it stands, on the same tree. The default floor is not re-run
    // for it (its last result on this tree stands, core#544; a declared verify floor re-runs its
    // checks, as on every attempt); the fold judges the attempt like any other.
    let rejudge = input
        .unit
        .requested_rejudge
        .as_ref()
        .filter(|j| !j.consumed && floor_rerun.is_none());
    let output = match floor_rerun {
        Some(r) => {
            engine_note(
                emit_delta,
                &format!(
                    "repo checks floor re-run ({}): the seat does not run again; the repository's \
                 checks re-run on the tree as it stands",
                    r.mode.as_wire()
                ),
            );
            // (core#782) A floor fix's tool calls are this attempt's: its governance evidence
            // (the hook's decisions log) and its tools fold with the reviewed output.
            let (files, tools, governed) = match &fixed {
                Some((_, f)) => (f.files.clone(), f.tools.clone(), f.governed),
                None => (Vec::new(), Vec::new(), false),
            };
            StepOutput {
                run_id: input.run_id.clone(),
                unit_ix: input.unit_ix,
                attempt: input.attempt,
                output: r.output.clone(),
                status: StepStatus::Ok,
                usage: None,
                files,
                tools,
                governed,
            }
        }
        None => match rejudge {
            Some(j) => {
                engine_note(
                    emit_delta,
                    &format!(
                        "judge re-run: the seat does not run again; the judge re-reads the \
                         reviewed output on the tree as it stands (eligible: {})",
                        if j.seats.is_empty() {
                            "none named".to_string()
                        } else {
                            j.seats.join(", ")
                        }
                    ),
                );
                StepOutput {
                    run_id: input.run_id.clone(),
                    unit_ix: input.unit_ix,
                    attempt: input.attempt,
                    output: j.output.clone(),
                    status: StepStatus::Ok,
                    usage: None,
                    files: Vec::new(),
                    tools: Vec::new(),
                    governed: false,
                }
            }
            // (program DES-POWER-MODE) A power run's orchestrator build: turns of the same seat
            // session until it hands off, a checkpoint at each turn end.
            None => {
                match crate::power::take_armed_build(&input.run_id, input.unit.ord, input.attempt) {
                    Some(arm) => crate::power::orchestrate(
                        runner,
                        advised.as_ref().unwrap_or(input),
                        emit_delta,
                        arm,
                    ),
                    None => {
                        runner.run_unit_streaming(advised.as_ref().unwrap_or(input), emit_delta)
                    }
                }
            }
        },
    };
    // (DES-QE-EVERY-SEAT) THE ISOLATED QE REVIEWER. For the run's QE unit with a `required`
    // decision, the turn above was the EXECUTOR (writer + executor, no QE verdict); the engine now
    // runs the reviewer as its own fresh worker session on a run seat, handed only the evidence
    // path, and binds the verdict it recorded. Each reviewer turn's governance is its OWN decisions
    // log, folded as that turn's (the record carries its nonce and flags); the unit's output stays
    // the executor's. A floor re-run runs no reviewer (the seat does not run again): it carries or
    // invalidates the last record.
    let qe_review = match floor_rerun {
        Some(r) => crate::qe_acceptance::review::on_floor_rerun(&input.unit, r.fix.is_some()),
        // (rejudge) Nor does a judge re-run: the seat did not run, the tree is the reviewed one.
        None if rejudge.is_some() => {
            crate::qe_acceptance::review::on_floor_rerun(&input.unit, false)
        }
        None => {
            crate::qe_acceptance::review::run(runner, input, &output, qe_not_before, emit_delta)
        }
    };
    // T5: `step.completed`, then the bounded gate wait for S's `ledger.folded` (fail-closed
    // synthesis on timeout). The snapshot is the attempt's `UnitEvidence.team`.
    let mut team_snapshot: Option<crate::domain::UnitTeamSnapshot> = match &attempt_team {
        crate::team::runner::Attempt::NotTeam => None,
        crate::team::runner::Attempt::Local(s) => Some((**s).clone()),
        // Presented = the block was built from a read stream AND a seat turn consumed it: a
        // repo-checks re-run never runs the seat, and a carrier that refused the launch before
        // any prompt (a skills refusal) returns a failed output with no turn behind it (codex
        // review of #740 round 7) — only an `Ok` turn proves the seat saw its prior context.
        crate::team::runner::Attempt::Claimed(c) => Some(crate::team::runner::complete_with(
            c,
            &output,
            boundary_read
                && floor_rerun.is_none()
                && rejudge.is_none()
                && output.status == StepStatus::Ok,
        )),
    };
    // DES-002 §8.8: the PA's review of a member's step is team evidence, never the gate — the
    // member's step already had its gate (its creator, the member, excluded). No judge, no floor:
    // the engine reads the `STEP` verdict and the attempt's ledger from this result.
    if input
        .unit
        .member_step
        .as_ref()
        .is_some_and(|m| m.reviewing.is_some())
    {
        return (
            output,
            None,
            crate::workflow::UnitEvidence {
                team: team_snapshot,
                ..Default::default()
            },
        );
    }
    // The seats the team's ledger names as authors/corroborators of a finding (DES-001 §6.2): a
    // monitor never grades its own finding. Computed from the ledger alone, never supplied.
    let mut team_authors: Vec<String> = team_snapshot
        .as_ref()
        .filter(|t| t.transport == crate::team::events::Transport::Bus)
        .and_then(|t| t.ledger.as_ref())
        .map(crate::team::runner::ledger_authors)
        .unwrap_or_default();
    // (rejudge) …and the monitors of the seat attempt a re-judge carried (armed, or its consumed
    // basis on a floor-only re-run): this attempt's own ledger may not carry them (a re-dispatch
    // stamps a fresh team snapshot).
    for m in input
        .unit
        .requested_rejudge
        .as_ref()
        .map(|j| j.monitors.as_slice())
        .unwrap_or_default()
    {
        if !team_authors.contains(m) {
            team_authors.push(m.clone());
        }
    }
    // Instance AND cli key: excluding `claude#2` excludes a `claude` seat too.
    let mut team_excluded: Vec<String> = Vec::new();
    // (EP-K3) The launch's judge exclusion is UNIONED in (never replaces): it can only narrow.
    // (crew#972) So is every seat this unit was ever dispatched on: a reassigned or failed-over
    // creator authored part of the work and never judges it.
    for a in input
        .unit
        .exclude_seats
        .iter()
        .chain(&input.unit.authored_by)
    {
        for k in [a.as_str(), crate::team::runner::seat_key(a)] {
            if !team_excluded.iter().any(|e| e == k) {
                team_excluded.push(k.to_string());
            }
        }
    }
    // (crew#972) The team monitors that raised findings are excluded SEPARATELY, only while a
    // judge distinct from them remains ([`exclude_monitors`]); instance and cli key.
    let mut team_monitors: Vec<String> = Vec::new();
    for a in &team_authors {
        for k in [a.as_str(), crate::team::runner::seat_key(a)] {
            if !team_excluded.iter().any(|e| e == k) && !team_monitors.iter().any(|e| e == k) {
                team_monitors.push(k.to_string());
            }
        }
    }
    // (core#954, operator decision 2026-10-17) An EVALUATOR unit's judge reads the work's
    // creators' output, so its own rotation excludes them too (instance and cli key) — unless that
    // leaves no eligible judge seat ([`exclude_creators`]); then the rotation judges as before,
    // disclosed ([`disclose_creator_judge`]). Every other unit's rotation is unchanged.
    let creators: Vec<String> = if input.unit.role == crate::workflow::PhaseRole::Evaluator {
        let mut v: Vec<String> = Vec::new();
        for a in fallback.work_authors {
            for k in [a.as_str(), crate::team::runner::seat_key(a)] {
                if !v.iter().any(|e| e == k) {
                    v.push(k.to_string());
                }
            }
        }
        v
    } else {
        Vec::new()
    };
    // (core#954, codex r1) The binaries the creators actually ran (recorded at dispatch): a seat
    // invoking one judges the creator's work as surely as the creator's own key.
    let creator_bins: Vec<String> = if creators.is_empty() {
        Vec::new()
    } else {
        fallback
            .author_binaries
            .map(<[String]>::to_vec)
            .unwrap_or_default()
    };
    // What the bus-path request names as excluded: the team monitors plus the launch exclusion.
    let mut bus_excluded: Vec<String> = team_authors.clone();
    // (crew#972, codex r1) …and every seat this unit was ever dispatched on, as on the inline path.
    for s in input
        .unit
        .exclude_seats
        .iter()
        .chain(&input.unit.authored_by)
    {
        if !bus_excluded.contains(s) {
            bus_excluded.push(s.clone());
        }
    }
    let mut monitors_note = if team_authors.is_empty() {
        String::new()
    } else {
        format!(" and the team monitors [{}]", team_authors.join(", "))
    };
    if !input.unit.exclude_seats.is_empty() {
        monitors_note.push_str(&format!(
            " and the launch-excluded seats [{}]",
            input.unit.exclude_seats.join(", ")
        ));
    }
    // (F-7R2-006 / review RT-1) The seats a JUDGE may run under: the registry seats the RUN
    // configured (`run_roster`, when the caller knows it), minus the run's bench, minus every
    // seat the launcher's health probe declared unusable — a signed-out seat cannot render a
    // verdict, and in run b86c14c1 the judge itself ran on such a seat and errored; a registry
    // seat the launcher never configured has no business judging the run.
    // The pool is the run's configured seats when any of them is a registry seat; a run whose
    // roster names NO registry seat (an ad-hoc launch roster, a test's fabricated keys) keeps
    // the registry as its pool — there is nothing to intersect with, and a judge must exist.
    // (codex r1-r3 on the fallback) The identities a FALLBACK judge must not share beyond the
    // parties' registry records: the authors' identities as the rotation resolved them, the
    // binary this unit is launched with ([`launched_identity`], the launcher's own resolution),
    // and the binary every author of the work actually ran (`author_binaries`, recorded at each
    // dispatch). `None` — no fallback — when an author's binary cannot be established. Read only
    // when a fallback is considered (it reads the registry).
    let fallback_ids = |author_ids: &[String]| -> Option<Vec<String>> {
        let Some(binaries) = fallback.author_binaries else {
            eprintln!(
                "wicked-core: no fallback judge — an author of this work has no recorded binary \
                 (operator ruling 2026-10-14)"
            );
            return None;
        };
        // (codex r4) …and THIS unit's own recorded authors' binaries (a Build unit's judge).
        if !crate::pipeline::author_binaries_known(&input.unit) {
            eprintln!(
                "wicked-core: no fallback judge — an author of this unit has no recorded binary \
                 (operator ruling 2026-10-14)"
            );
            return None;
        }
        let mut v = author_ids.to_vec();
        v.extend(binaries.iter().cloned());
        v.extend(input.unit.authored_binaries.iter().cloned());
        if let Some(k) = input.unit.assigned_cli.as_deref() {
            v.push(launched_identity(k, input.unit.assigned_invocation.clone()));
        }
        Some(v)
    };
    // (PRODUCT-WORKS J4) A seat whose QUOTA bench has fallen due answers one cheap call first, now
    // (after the unit's own work, before the judge reads the bench): back on the roster when it
    // answers, benched again for longer when it does not. A gate whose only distinct judge was
    // benched for quota then picks the returned seat.
    let (bench_now, bench_reprobes) = if fallback.reprobe {
        reprobe_due_benches(
            &**runner,
            fallback.registry,
            fallback.bench,
            crate::seat_bench::now_ms(),
            emit_delta,
        )
    } else {
        (fallback.bench.to_vec(), Vec::new())
    };
    let benched_now: Vec<String> = benched
        .iter()
        .filter(|k| bench_now.iter().any(|b| &b.cli == *k) || !fallback.reprobe)
        .cloned()
        .collect();
    let benched: &[String] = &benched_now;
    let fallback = JudgeFallbackCtx {
        bench: &bench_now,
        ..fallback
    };
    let pool = judge_pool(roster, run_roster);
    // (core#772) The same pool BEFORE the run's bench: a judge seat benched earlier in this run
    // (a quota / sign-in refusal while judging) must not turn "no distinct judge" into an
    // UNGATED pass on the retry — when the only distinct judges left are benched ones, the gate
    // is a seat failure ([`benched_judge_unavailable`]), never a skipped judge.
    let unbenched: Vec<crate::AgenticCli> = pool
        .iter()
        .filter(|c| c.health.as_ref().is_none_or(|h| h.usable))
        .map(|c| (*c).clone())
        .collect();
    let eligible: Vec<crate::AgenticCli> = unbenched
        .iter()
        .filter(|c| !benched.iter().any(|b| b == &c.key))
        .cloned()
        .collect();
    // The run's own configured seats among them: the rotation stays on these whenever one is
    // distinct from the work's authors ([`judge_roster`]).
    let eligible_configured: Vec<crate::AgenticCli> = eligible
        .iter()
        .filter(|c| in_run_roster(run_roster, &c.key))
        .cloned()
        .collect();
    // The DEFAULT judge's pool is the run's own (pre-#893): its configured seats, or the whole
    // registry only for a roster that names no registry seat. A default judge is never required,
    // so a seat the launch never configured must not turn a skipped judge into a human pause
    // (crew main red 2026-10-11: an unauthenticated codex on CI parked a one-seat run).
    let run_has_registry_seat = roster.iter().any(|c| in_run_roster(run_roster, &c.key));
    let in_run_pool =
        |c: &crate::AgenticCli| !run_has_registry_seat || in_run_roster(run_roster, &c.key);
    let unbenched_run: Vec<crate::AgenticCli> = unbenched
        .iter()
        .filter(|c| in_run_pool(c))
        .cloned()
        .collect();
    let eligible_run: Vec<crate::AgenticCli> = eligible
        .iter()
        .filter(|c| in_run_pool(c))
        .cloned()
        .collect();
    // (review RT-1 / F-7R3-001) Judge seats that REFUSED while the rotation passed over them —
    // an authentication failure, a quota refusal, a binary that could not start — reported for
    // the fold to bench (`source: "judge"`; a quota refusal benches only a seat with no
    // successful unit in the run — review F3) so they are never re-tried by the next unit. The
    // transcript is judged by `classify_refusal` (authentication over the whole text, the quota
    // refusal frame over its tail) and a wrapped `(could not run …)` line by the spawn error it
    // carries.
    let judge_refusals: std::cell::RefCell<Vec<crate::workflow::JudgeRefusal>> =
        std::cell::RefCell::new(Vec::new());
    let note_refusals = |refused: &[(String, String)]| {
        use wicked_council::types::SeatFailureReason;
        for (seat, out) in refused {
            // (r2-N2) Exit evidence for the generic quota rule: the runner's own marker.
            let exited_nonzero =
                crate::execute_wrapped::wrapped_exit_code(out).is_some_and(|c| c != 0);
            let reason = SeatFailureReason::classify_refusal(out, exited_nonzero)
                .or_else(|| {
                    crate::execute_wrapped::spawn_failure_detail(out)
                        .and_then(SeatFailureReason::classify_spawn_detail)
                })
                .map(|r| (r.verb(), r.as_str()))
                // (core#772) A judge that exited clean and printed nothing: benched under its
                // own token (the fold benches a token it does not know as written).
                .or_else(|| {
                    (out == crate::validator::JUDGE_EMPTY_ANSWER).then_some((
                        "answered nothing",
                        crate::validator::JUDGE_EMPTY_ANSWER_REASON,
                    ))
                });
            if let Some((verb, token)) = reason {
                let mut v = judge_refusals.borrow_mut();
                if !v.iter().any(|r| &r.seat == seat) {
                    eprintln!(
                        "wicked-core: judge seat '{seat}' {} on unit {} ({}); reported for the \
                         run's bench (F-7R2-006 / review RT-1 / F-7R3-001)",
                        verb, input.unit.ord, token
                    );
                    // (PRODUCT-WORKS J4) A quota refusal that states when the provider resets
                    // lifts the bench then, instead of after the backoff.
                    let reset_at = (token == SeatFailureReason::QuotaExhausted.as_str())
                        .then(|| {
                            crate::seat_bench::provider_reset_at(out, crate::seat_bench::now_ms())
                        })
                        .flatten();
                    v.push(crate::workflow::JudgeRefusal {
                        seat: seat.clone(),
                        reason: token.to_string(),
                        reset_at,
                    });
                }
            }
        }
    };
    // (F-7R2-005) DID THE UNIT CHANGE THE TREE? For a bound AGENT unit the guard does not cover
    // (a creator, a prose-planned unit), compare the worktree against the baseline the actor
    // snapshotted at dispatch. `Some(true)` arms the DEFAULT floor + judge below; an unknown
    // (no baseline persisted, git failed) is treated as CHANGED — the checks run rather than a
    // change slipping through ungated. A guarded unit keeps `None`: the guard owns its tree.
    let bound_agent = output.status == StepStatus::Ok
        && input.unit.tool_cmd.is_none()
        && input.unit.default_floor
        && input.workdir.is_some()
        && !crate::worktree_guard::applies_to(&input.unit);
    let tree_changed: Option<bool> = if bound_agent {
        let wd = input.workdir.as_deref().expect("bound");
        match input.unit.worktree_baseline.as_ref() {
            Some(baseline) => match crate::worktree_guard::tree_changed_since(wd, baseline) {
                Ok(changed) => Some(changed),
                Err(e) => {
                    eprintln!(
                        "wicked-core: unit {} — could not compare the worktree against its \
                         dispatch baseline ({e}); treating the tree as CHANGED so the default \
                         repo-checks floor runs (fail-closed, F-7R2-005)",
                        input.unit.ord
                    );
                    Some(true)
                }
            },
            None => {
                eprintln!(
                    "wicked-core: unit {} carries no dispatch baseline; treating the tree as \
                     CHANGED so the default repo-checks floor runs (fail-closed, F-7R2-005)",
                    input.unit.ord
                );
                Some(true)
            }
        }
    } else {
        None
    };
    // (core#544) The tree id as it stands now: the floor's report records the tree it ran
    // ON, so a rework that leaves the tree identical to the one the unit's last floor judged
    // carries that verdict forward (the fold), while a tree no floor of this unit has judged —
    // an operator's hand edit between attempts — runs the checks even though this attempt
    // changed nothing.
    let current_tree: Option<String> = bound_agent
        .then(|| {
            let wd = input.workdir.as_deref()?;
            let git_dir = input.unit.worktree_baseline.as_ref()?.git_dir.as_deref()?;
            crate::worktree_guard::snapshot_through(wd, std::path::Path::new(git_dir))
                .map(|s| s.tree)
                .ok()
        })
        .flatten();
    let unfloored_tree =
        input.unit.repo_checks.as_ref().is_some_and(|r| {
            current_tree.is_none() || r.tree.as_deref() != current_tree.as_deref()
        });
    // The DEFAULT floor applies to a bound agent unit that changed (or may have changed) its
    // tree: the repository's own checks run and, when an eligible non-creator seat exists, a
    // judge distinct from the creator renders a verdict (F-7R2-005) — and to one whose
    // tree no floor of this unit has judged yet, or whose floor the operator asked to re-run.
    let default_floor_applies =
        bound_agent && (tree_changed != Some(false) || unfloored_tree || floor_rerun.is_some());
    // (rejudge) The DEFAULT judge re-runs on a re-judge even though the tree is the one its floor
    // already judged — the judge is what the operator asked for; the checks are not re-run.
    let default_judge_applies = default_floor_applies || (bound_agent && rejudge.is_some());
    // An OPERATOR RETRY of this unit's escalation gate armed ONE re-run of its floor
    // (`RepoChecksReport::recheck`): the checks run on the tree as it stands even when this
    // attempt left it unchanged — the single exception to the core#544 carry-forward, so a flaky
    // result is not sticky per tree. The checks only; the judge arms exactly as before.
    let recheck_armed = bound_agent
        && input
            .unit
            .repo_checks
            .as_ref()
            .is_some_and(|r| r.recheck.is_some());
    // F-036 WORKTREE GUARD, first look — taken right after the seat's own work so the repo checks
    // below are never run over a tree the seat already rewrote (they would certify the wrong
    // code). This is NOT the outcome the gate sees: the FINAL comparison is taken at the very end
    // of this function, after the judge and the checks, so a write that lands after the seat
    // returns (a backgrounded process, a "passing" check script that edits a tracked file) is
    // caught too (codex review on #414). Only for a unit that folded Ok: a failed unit fails on
    // its own account and never reaches the gate this feeds.
    let guard_first_look = if output.status == StepStatus::Ok {
        crate::worktree_guard::outcome_for_unit(&input.unit, input.workdir.as_deref())
    } else {
        None
    };
    let work_owned = select_work_for_agent(
        input.unit.role,
        &input.unit.required_deliverables,
        input.workdir.as_deref(),
        agent_review_target,
        &output.output,
    );
    // Judge-visible worktree evidence (core#280): computed only when a judge will actually run —
    // same conditions as the `agent_verdict` block below — so ordinary units pay no git spawns.
    let will_judge = output.status == StepStatus::Ok
        && input.workdir.is_some()
        && (input.unit.validator.as_ref().is_some_and(|v| v.approved) || default_judge_applies);
    let work_owned = match will_judge
        .then_some(input.workdir.as_deref())
        .flatten()
        .and_then(worktree_evidence_for_judge)
    {
        Some(evidence) => std::borrow::Cow::Owned(format!("{work_owned}{evidence}")),
        None => work_owned,
    };
    // T5 / DES-001 §6.2: the judge sees the team's outcomes AND its comms — the rendered ledger
    // and compact transcript ride INSIDE the WORK fence (untrusted data, never instructions).
    let work_owned = match will_judge
        .then(|| {
            team_snapshot
                .as_ref()
                .and_then(crate::team::runner::render_for_gate)
        })
        .flatten()
    {
        Some(team) => {
            if let Some(l) = team_snapshot.as_mut().and_then(|t| t.ledger.as_mut()) {
                l.rendered_to_judge = true;
            }
            std::borrow::Cow::Owned(format!("{work_owned}{team}"))
        }
        None => work_owned,
    };
    let work_for_agent: &str = &work_owned;
    // (F-7R2-005) WHY no judge ran for a unit that WANTED one — rides to `gateEvaluated.
    // ungatedReason` so the studio says "UNGATED — no eligible judge seat" instead of "pass".
    let mut judge_skipped: Option<String> = None;
    // (crew#972) Set by the judge path that re-admitted the team monitors; read once the verdict is in.
    let mut monitor_fallback: Option<String> = None;
    let agent_verdict = if output.status == StepStatus::Ok && input.workdir.is_some() {
        // (#539) Labeled block so the bus path and the distinct-judge pre-check can both exit
        // early without a closure (closures cannot mutate `judge_skipped`).
        let pinned: Option<crate::validator::AgentVerdict> = 'pinned: {
            let Some(v) = input.unit.validator.as_ref().filter(|v| v.approved) else {
                break 'pinned None;
            };
            // (core#799) The judge is told the pinned floor decides the criterion's
            // deterministic half — it runs after the judge, so the judge never sees its result.
            let judge_criterion = crate::validator::pinned_judge_criterion(v);
            // BUS PATH: only over a bus exec mediation ARMED for this unit (`gate_eval_bus_db`; never
            // process env — DES-TEAMING-002 T0), publish a gate-evaluation request and
            // wait for the governed evaluator daemon to respond (no subprocess, no dangerous flags,
            // no TTY). On timeout or any error the function returns a hard DENY (fail-closed
            // governance — a timeout must never silently approve a gate by falling back to
            // deterministic-only).
            if let Some(bus_path) = gate_eval_bus_db(exec_bus) {
                // Carry the work author so the evaluator daemon can enforce evaluator≠creator on
                // the bus path (same guarantee the inline path enforces via excluded[] at ~499).
                let work_author = input.unit.assigned_cli.as_deref();
                // The daemon does not report which seat judged: `judge_cli` stays `None` on
                // this path (core#431) — an honest unknown, never a guessed seat.
                break 'pinned Some(bus_request_agent_verdict(
                    &judge_criterion,
                    work_for_agent,
                    &input.run_id,
                    input.unit_ix,
                    input.attempt,
                    &bus_path,
                    work_author,
                    &bus_excluded,
                ));
            }
            // INLINE PATH (legacy — no bus): spawn a governed council seat subprocess.
            // GAP B + C1: run the agent judge under a council seat whose identity is DISTINCT from
            // BOTH the deterministic validator's author (`DETERMINISTIC_VALIDATOR_SEAT`) AND the
            // WORK's own author (the unit's `assigned_cli`, falling back to the deterministic author
            // when unassigned). Excluding the work author is what stops a self-grade.
            let work_author = input
                .unit
                .assigned_cli
                .as_deref()
                .unwrap_or(crate::validator::DETERMINISTIC_VALIDATOR_SEAT);
            let mut excluded: Vec<&str> =
                vec![crate::validator::DETERMINISTIC_VALIDATOR_SEAT, work_author];
            excluded.extend(team_excluded.iter().map(String::as_str));
            let has_judge = |ex: &[&str]| {
                let mut ids = author_identities(ex, roster);
                ids.extend(creator_bins.iter().cloned());
                let all = without_author_binaries(&eligible, ex, &ids);
                let configured = without_author_binaries(&eligible_configured, ex, &ids);
                crate::validator::distinct_judge_available(ex, judge_roster(ex, &configured, &all))
            };
            // (crew#972) The monitors first, so a monitor is re-admitted before a creator is.
            monitor_fallback =
                exclude_monitors(&mut excluded, &team_monitors, &creators, has_judge);
            if let Some(why) = &monitor_fallback {
                engine_note(
                    emit_delta,
                    &format!("agent validator: {why} — a team monitor may judge it, disclosed"),
                );
            }
            let creator_fallback = exclude_creators(&mut excluded, &creators, has_judge);
            if let Some(why) = &creator_fallback {
                engine_note(
                    emit_delta,
                    &format!("agent validator: {why} — the run's own judge rotation decides"),
                );
            }
            // The authors' invocation identities, resolved against the FULL registry before any
            // bench/health filter, drop every other seat sharing one: a benched creator alias must
            // not leave its binary looking distinct on another seat (codex r1/r2 on #893).
            let mut author_ids = author_identities(&excluded, roster);
            // (core#954, codex r1) …and every binary the creators actually ran, once excluded.
            if creator_fallback.is_none() {
                author_ids.extend(creator_bins.iter().cloned());
            }
            let creator_ids = CreatorIds::of(&creators, &creator_bins, roster);
            let eligible_x = without_author_binaries(&eligible, &excluded, &author_ids);
            let configured_x =
                without_author_binaries(&eligible_configured, &excluded, &author_ids);
            // Whether only OUTSIDE seats can judge: the run's own unbenched seats never had a
            // distinct judge (the pre-#893 "judge skipped" condition).
            let outside_only = outside_seats_only(
                &excluded,
                &without_author_binaries(&unbenched_run, &excluded, &author_ids),
            );
            let roster = judge_roster(&excluded, &configured_x, &eligible_x);
            // (#539) Pre-check: only run the judge when an identity-distinct seat exists.
            // Without this check, agent_validate falls back to the single default runner — a
            // self-grade when the creator is claude. Mirror the default-floor path (F-7R2-005).
            // When only outside seats could ever judge, the bench is read against the run's OWN
            // pre-bench pool (the pre-#893 meaning): an outside seat benched by an earlier unit
            // leaves the skip below, never a seat-failure pause (codex r1 on #895).
            let bench_pool: &[crate::AgenticCli] = if outside_only {
                &unbenched_run
            } else {
                &unbenched
            };
            if let Some(b) = benched_judge_unavailable(
                "agent validator",
                &excluded,
                roster,
                bench_pool,
                benched,
                fallback.bench,
            ) {
                // (operator ruling 2026-10-14) Every preferred judge is benched: a distinct
                // fallback seat judges in its place, disclosed; with none, the hold stands.
                let parties = fallback_parties(&excluded, fallback.work_authors);
                let fb = match fallback_ids(&author_ids).and_then(|ids| {
                    fallback_candidates(
                        fallback.pool,
                        fallback.registry,
                        &parties,
                        &ids,
                        benched,
                        &[],
                    )
                }) {
                    Some(candidates) => fallback_judge(
                        "agent validator",
                        &judge_criterion,
                        work_for_agent,
                        &parties,
                        &candidates,
                        b.lost_keys.clone(),
                        benched_preferred_reason(&b.lost_keys, fallback.bench),
                        &**runner,
                        &note_refusals,
                        emit_delta,
                    ),
                    None => FallbackJudge::NoCandidate,
                };
                break 'pinned Some(held_with_fallback_failures(
                    "agent validator",
                    b.held,
                    b.lost,
                    fb,
                ));
            }
            if !crate::validator::distinct_judge_available(&excluded, roster) {
                let creator_id = crate::validator::excluded_identity(work_author, roster);
                let validator_id = crate::validator::excluded_identity(
                    crate::validator::DETERMINISTIC_VALIDATOR_SEAT,
                    roster,
                );
                let seat_outcomes: Vec<String> = roster
                    .iter()
                    .map(|c| {
                        let id = crate::validator::seat_identity(c);
                        let outcome = if id == creator_id && id == validator_id {
                            "excluded (creator = validator author)".to_string()
                        } else if id == creator_id {
                            "excluded (creator)".to_string()
                        } else if id == validator_id {
                            "excluded (validator author)".to_string()
                        } else if c.headless_invocation.trim().is_empty() {
                            "unusable (empty invocation)".to_string()
                        } else {
                            format!("excluded (same identity as {})", id)
                        };
                        format!("{}: {}", c.key, outcome)
                    })
                    .collect();
                let why = format!(
                    "no eligible judge seat distinct from creator '{work_author}' and validator \
                     author '{}'{monitors_note} (seats: {}; benched: {})",
                    crate::validator::DETERMINISTIC_VALIDATOR_SEAT,
                    if seat_outcomes.is_empty() {
                        "none".to_string()
                    } else {
                        seat_outcomes.join(", ")
                    },
                    if benched.is_empty() {
                        "none".to_string()
                    } else {
                        benched.join(", ")
                    }
                );
                eprintln!(
                    "wicked-core: unit {} has a pinned validator but no distinct judge seat — \
                     judge skipped (evaluator ≠ creator enforced; F-7R2-005/#539)",
                    input.unit.ord
                );
                judge_skipped = Some(why);
                break 'pinned None;
            }
            // (rig run 1d372696) The judge runs between the seat's hand-off and the floor: without
            // a note the run showed 1.5 min with no event at all. Said on the unit's own stream.
            let (verdict, refused) =
                with_floor_heartbeat(emit_delta, JUDGE_NOTE, FLOOR_HEARTBEAT_EVERY, || {
                    crate::validator::agent_validate_with_refusals(
                        &judge_criterion,
                        work_for_agent,
                        &excluded,
                        roster,
                        &**runner,
                    )
                });
            note_refusals(&refused);
            // (crew main red 2026-10-11) An outside seat is a LAST RESORT that replaces a skipped
            // judge: when only outside seats could judge and every one of them failed as a seat
            // (signed out, out of quota), the gate gets the skip it had before #893 — the floor
            // decides under reduced assurance, and a required judge still holds the gate
            // (`judge_unavailable`) — never a seat-failure pause the run's own roster never asked
            // for. The refusals above are still reported for the bench.
            // A judge that ANSWERED without a verdict is not a failed seat: it escalates to the
            // human (operator ruling, revised), never the skip.
            let all_failed = match &verdict {
                Ok(av) => av.seat_failure.is_some(),
                Err(e) => e
                    .downcast_ref::<crate::validator::JudgeNoVerdict>()
                    .is_none(),
            };
            if outside_only && all_failed {
                judge_skipped = Some(format!(
                    "no judge seat distinct from creator '{work_author}' and validator author                      '{}'{monitors_note} on the run's own seats, and every other enabled seat                      failed to judge ({})",
                    crate::validator::DETERMINISTIC_VALIDATOR_SEAT,
                    if refused.is_empty() {
                        "no answer".to_string()
                    } else {
                        refused
                            .iter()
                            .map(|(k, why)| format!("{k}: {why}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                ));
                break 'pinned None;
            }
            Some(match verdict {
                // Carries the judge seat + distinctness for `gateEvaluated` (core#431).
                Ok(av) => {
                    let seat = av.judge_cli.clone();
                    disclose_creator_judge(
                        av,
                        seat,
                        creator_fallback.as_deref(),
                        &creator_ids,
                        emit_delta,
                    )
                }
                // (core#954, codex r1) A creator that answered without a verdict is disclosed too.
                Err(e)
                    if creator_fallback.is_some()
                        && e.downcast_ref::<crate::validator::JudgeNoVerdict>()
                            .is_some() =>
                {
                    let seat = e
                        .downcast_ref::<crate::validator::JudgeNoVerdict>()
                        .map(|nv| nv.seat.clone());
                    disclose_creator_judge(
                        crate::validator::AgentVerdict::from_judge_error("agent validator", e),
                        seat,
                        creator_fallback.as_deref(),
                        &creator_ids,
                        emit_delta,
                    )
                }
                // (core#772) Every judge seat failed as a seat ⇒ a seat-failure denial, not a
                // REJECT; any other error keeps the fail-closed REJECT shape. (operator ruling
                // 2026-10-14) Before the hold, a distinct fallback seat judges in their place.
                Err(e) => judge_error_or_fallback(
                    "agent validator",
                    e,
                    &refused,
                    &judge_criterion,
                    work_for_agent,
                    &fallback_parties(&excluded, fallback.work_authors),
                    fallback_ids(&author_ids),
                    roster,
                    benched,
                    fallback,
                    &**runner,
                    &note_refusals,
                    emit_delta,
                ),
            })
        };
        if pinned.is_some() {
            pinned
        } else if default_judge_applies && judge_skipped.is_none() {
            // (F-7R2-005) The DEFAULT judge: no pinned validator gated this unit, but it changed
            // the tree — so a seat DISTINCT from the creator judges the change against the
            // engine-authored default criterion. Never the single-runner fallback (that would be
            // a self-grade for a claude creator): with no eligible distinct seat the gate says
            // UNGATED and why, on the wire.
            let work_author = input
                .unit
                .assigned_cli
                .as_deref()
                .unwrap_or(crate::validator::DETERMINISTIC_VALIDATOR_SEAT);
            let mut excluded: Vec<&str> = vec![work_author];
            excluded.extend(team_excluded.iter().map(String::as_str));
            let has_judge = |ex: &[&str]| {
                let mut ids = author_identities(ex, roster);
                ids.extend(creator_bins.iter().cloned());
                crate::validator::distinct_judge_available(
                    ex,
                    &without_author_binaries(&eligible_run, ex, &ids),
                )
            };
            // (crew#972) As on the pinned path: a monitor is re-admitted before a creator is.
            monitor_fallback =
                exclude_monitors(&mut excluded, &team_monitors, &creators, has_judge);
            if let Some(why) = &monitor_fallback {
                engine_note(
                    emit_delta,
                    &format!("default judge: {why} — a team monitor may judge it, disclosed"),
                );
            }
            let creator_fallback = exclude_creators(&mut excluded, &creators, has_judge);
            if let Some(why) = &creator_fallback {
                engine_note(
                    emit_delta,
                    &format!("default judge: {why} — the run's own judge rotation decides"),
                );
            }
            // The authors' invocation identities, resolved against the FULL registry before any
            // bench/health filter, drop every other seat sharing one: a benched creator alias must
            // not leave its binary looking distinct on another seat (codex r1/r2 on #893).
            let mut author_ids = author_identities(&excluded, roster);
            if creator_fallback.is_none() {
                author_ids.extend(creator_bins.iter().cloned());
            }
            let creator_ids = CreatorIds::of(&creators, &creator_bins, roster);
            let run_x = without_author_binaries(&eligible_run, &excluded, &author_ids);
            let roster = run_x.as_slice();
            let criterion = crate::validator::default_judge_criterion(&input.unit);
            if let Some(b) = benched_judge_unavailable(
                "default judge",
                &excluded,
                roster,
                &unbenched_run,
                benched,
                fallback.bench,
            ) {
                // (operator ruling 2026-10-14) As on the pinned path: a distinct fallback seat
                // judges in place of the benched preferred judges; with none, the hold stands.
                let parties = fallback_parties(&excluded, fallback.work_authors);
                let fb = match fallback_ids(&author_ids).and_then(|ids| {
                    fallback_candidates(
                        fallback.pool,
                        fallback.registry,
                        &parties,
                        &ids,
                        benched,
                        &[],
                    )
                }) {
                    Some(candidates) => fallback_judge(
                        "default judge",
                        &criterion,
                        work_for_agent,
                        &parties,
                        &candidates,
                        b.lost_keys.clone(),
                        benched_preferred_reason(&b.lost_keys, fallback.bench),
                        &**runner,
                        &note_refusals,
                        emit_delta,
                    ),
                    None => FallbackJudge::NoCandidate,
                };
                Some(held_with_fallback_failures(
                    "default judge",
                    b.held,
                    b.lost,
                    fb,
                ))
            } else if crate::validator::distinct_judge_available(&excluded, roster) {
                eprintln!(
                    "wicked-core: unit {} changed the worktree tree with no pinned validator — \
                     convening the default judge (evaluator ≠ creator '{work_author}') \
                     (F-7R2-005)",
                    input.unit.ord
                );
                let (verdict, refused) =
                    with_floor_heartbeat(emit_delta, JUDGE_NOTE, FLOOR_HEARTBEAT_EVERY, || {
                        crate::validator::agent_validate_with_refusals(
                            &criterion,
                            work_for_agent,
                            &excluded,
                            roster,
                            &**runner,
                        )
                    });
                note_refusals(&refused);
                Some(match verdict {
                    Ok(av) => {
                        let seat = av.judge_cli.clone();
                        disclose_creator_judge(
                            av,
                            seat,
                            creator_fallback.as_deref(),
                            &creator_ids,
                            emit_delta,
                        )
                    }
                    // (core#954, codex r1) A creator that answered without a verdict is disclosed too.
                    Err(e)
                        if creator_fallback.is_some()
                            && e.downcast_ref::<crate::validator::JudgeNoVerdict>()
                                .is_some() =>
                    {
                        let seat = e
                            .downcast_ref::<crate::validator::JudgeNoVerdict>()
                            .map(|nv| nv.seat.clone());
                        disclose_creator_judge(
                            crate::validator::AgentVerdict::from_judge_error("default judge", e),
                            seat,
                            creator_fallback.as_deref(),
                            &creator_ids,
                            emit_delta,
                        )
                    }
                    Err(e) => judge_error_or_fallback(
                        "default judge",
                        e,
                        &refused,
                        &criterion,
                        work_for_agent,
                        &fallback_parties(&excluded, fallback.work_authors),
                        fallback_ids(&author_ids),
                        roster,
                        benched,
                        fallback,
                        &**runner,
                        &note_refusals,
                        emit_delta,
                    ),
                })
            } else {
                let creator_id = crate::validator::excluded_identity(work_author, roster);
                let seat_outcomes: Vec<String> = roster
                    .iter()
                    .map(|c| {
                        let id = crate::validator::seat_identity(c);
                        let outcome = if id == creator_id {
                            "excluded (creator)".to_string()
                        } else if c.headless_invocation.trim().is_empty() {
                            "unusable (empty invocation)".to_string()
                        } else {
                            format!("excluded (same identity as {})", id)
                        };
                        format!("{}: {}", c.key, outcome)
                    })
                    .collect();
                let why = format!(
                    "no eligible judge seat distinct from creator '{work_author}'{monitors_note} \
                     (seats: {}; benched: {})",
                    if seat_outcomes.is_empty() {
                        "none".to_string()
                    } else {
                        seat_outcomes.join(", ")
                    },
                    if benched.is_empty() {
                        "none".to_string()
                    } else {
                        benched.join(", ")
                    }
                );
                eprintln!(
                    "wicked-core: unit {} changed the worktree tree but {why} — no judge convened; \
                     the gate will say UNGATED unless the repo checks floor gates it (F-7R2-005)",
                    input.unit.ord
                );
                judge_skipped = Some(why);
                None
            }
        } else {
            None
        }
    } else {
        None
    };
    let agent_verdict = agent_verdict.map(|av| {
        disclose_monitor_judge(
            av,
            monitor_fallback.as_deref(),
            &team_monitors,
            roster,
            emit_delta,
        )
    });
    // F-039 REPO CHECKS FLOOR — the engine runs the repository's own checks LAST (slowest, and
    // its build artifacts must not colour the guard's comparison or the judge's evidence above),
    // only for the def's code-verifying unit, only when the unit folded Ok, and not when the
    // guard already caught the seat rewriting the tree (checks over a rewritten tree would
    // certify the wrong code, and the gate denies on the mutation regardless).
    let guard_denies = matches!(
        &guard_first_look,
        Some(crate::worktree_guard::WorktreeGuardOutcome::Mutated(m)) if m.denies()
    ) || matches!(
        &guard_first_look,
        Some(crate::worktree_guard::WorktreeGuardOutcome::Unverifiable(_))
    );
    let repo_checks = match input.workdir.as_deref() {
        Some(wd)
            if (input.unit.repo_checks_floor || default_floor_applies || recheck_armed)
                && input.unit.tool_cmd.is_none()
                && output.status == StepStatus::Ok
                && !guard_denies =>
        {
            eprintln!(
                "wicked-core: repo checks floor — running the repository's own checks in {} for \
                 unit {} ({})",
                wd.display(),
                input.unit.ord,
                if input.unit.repo_checks_floor {
                    "F-039: the def's verified_evidence phase"
                } else if default_floor_applies {
                    "F-7R2-005: the unit changed the worktree tree — default floor"
                } else {
                    "an operator retry re-runs the floor on the unchanged tree — once"
                }
            );
            // core#467 / core#469: WHOSE floor this is decides the test set (targeted first at
            // the creator) and whether the transcript's "pre-existing" claim is judged; the RUN
            // base (recorded on the unit at dispatch — or, unrecorded, the dispatch baseline's
            // HEAD) is the base the baseline diff compares against, through the PINNED git dir
            // the baseline was taken through.
            let stage = if input.unit.repo_checks_floor {
                crate::repo_checks::FloorStage::Verify
            } else {
                crate::repo_checks::FloorStage::Creator
            };
            let baseline = input.unit.worktree_baseline.as_ref();
            let ctx = crate::repo_checks::FloorContext {
                stage,
                force_install: false,
                // The run base first (F-RC2-009): a creator that committed its work has moved
                // the baseline's HEAD onto the head itself, and base == head compares nothing.
                base_head: input
                    .unit
                    .run_base_commit
                    .clone()
                    .filter(|h| !h.is_empty())
                    .or_else(|| baseline.map(|b| b.head.clone()).filter(|h| !h.is_empty())),
                git_dir: baseline
                    .and_then(|b| b.git_dir.clone())
                    .map(std::path::PathBuf::from),
                claim_text: (stage == crate::repo_checks::FloorStage::Creator)
                    .then(|| output.output.clone()),
                package_dir: None,
                run_id: Some(input.run_id.clone()),
                // (core#1187) The creator floor's affected set reads the repo's code graph.
                code_graph: input
                    .governance
                    .as_ref()
                    .and_then(|g| g.code_graph_db.as_deref())
                    .filter(|p| !p.is_empty())
                    .map(std::path::PathBuf::from),
            };
            // crew #581 (F-BM-010): the floor runs on THIS thread after the runner returned, so
            // the unit's live-output stream went silent for its whole duration (run 8: crew's
            // `test` > 25 min under a 3600 s bound → the stall watchdog read 30 min of silence as
            // a wedged worker and re-dispatched a second creator into the same worktree). The
            // floor now heartbeats through the sink this thread ALREADY holds — an ordinary
            // `unitOutputDelta` at the start and every 5 min until `run_floor` returns — so the
            // watchdog's ladder resets on the existing frame (crew clocks any frame carrying the
            // session). No new frame, hook, exemption or `FloorContext` change; the per-check
            // durations stay on `repoChecksEvaluated.checks[].durationMs`.
            // (core#417, operator ruling (a)) A GUARDED verify floor runs on a clean checkout of
            // exactly the tree the guard checked, never the live worktree — an untracked or
            // ignored file there cannot steer the checks.
            let guarded =
                stage == crate::repo_checks::FloorStage::Verify && input.unit.worktree_guarded;
            let git_dir = baseline.and_then(|b| b.git_dir.as_deref());
            // The tree the GUARD checked: its first look, which judged the tree clean (the floor
            // does not run over a mutated or unverifiable one).
            let guard_tree = match &guard_first_look {
                Some(crate::worktree_guard::WorktreeGuardOutcome::Clean { after, .. }) => {
                    Some(after.tree.as_str())
                }
                _ => None,
            };
            // (core#1144) The CREATOR floor runs on a clean checkout of the tree the seat left
            // too, whenever that tree and the pinned git dir are known — so its results are as
            // trustworthy as the verify floor's, and the verify floor of the same tree and base
            // reuses them instead of running the same checks again. A checkout that cannot be made
            // falls back to the live worktree (the creator floor's historical place, unkeyed and
            // never reused); the verify floor stays fail-closed.
            let creator_tree = (stage == crate::repo_checks::FloorStage::Creator)
                .then_some(current_tree.as_deref())
                .flatten();
            let phase = input.unit.phase_id().map(str::to_string);
            let journal_for = |tree: &str| crate::repo_checks::FloorJournal {
                ord: input.unit.ord,
                attempt: input.attempt,
                tree: tree.to_string(),
                phase: phase.clone(),
            };
            let (report, ran_on) = with_floor_heartbeat(
                emit_delta,
                &format!(
                    "repo checks floor ({}): running the repository's own checks",
                    stage.as_wire()
                ),
                FLOOR_HEARTBEAT_EVERY,
                || {
                    let checkout = |tree: &str, git_dir: &str| {
                        crate::repo_checks::checkout_guarded_tree(
                            wd,
                            std::path::Path::new(git_dir),
                            tree,
                            &format!("{}-{}-a{}", input.run_id, input.unit.ord, input.attempt),
                        )
                    };
                    match (guarded, guard_tree, creator_tree, git_dir) {
                        (true, Some(tree), _, Some(git_dir)) => match checkout(tree, git_dir) {
                            Ok(checkout) => (
                                checkout.run_floor_journaled(&ctx, floor_rerun, &journal_for(tree)),
                                Some(tree),
                            ),
                            Err(why) => (
                                crate::repo_checks::guarded_checkout_refused(wd, &ctx, &why),
                                None,
                            ),
                        },
                        (false, _, Some(tree), Some(git_dir)) => match checkout(tree, git_dir) {
                            Ok(checkout) => (
                                checkout.run_floor_journaled(&ctx, floor_rerun, &journal_for(tree)),
                                Some(tree),
                            ),
                            Err(why) => {
                                eprintln!(
                                    "wicked-core: repo checks floor (creator) for unit {}: the \
                                     tree could not be checked out clean ({why}); the checks run \
                                     in the live worktree, and the verify floor will not reuse \
                                     them",
                                    input.unit.ord
                                );
                                (
                                    crate::repo_checks::run_floor_journaled(
                                        wd,
                                        &ctx,
                                        floor_rerun,
                                        Some(&journal_for(tree)),
                                    ),
                                    None,
                                )
                            }
                        },
                        // No guard tree (a repo-less run: no git, no snapshot) — there is
                        // no guarded tree to check out, so the floor stays where the run is.
                        // (J4) Journaled on the tree the worktree holds now, so a floor a
                        // daemon restart interrupts resumes on the same tree.
                        _ => (
                            crate::repo_checks::run_floor_journaled(
                                wd,
                                &ctx,
                                floor_rerun,
                                current_tree.as_deref().map(journal_for).as_ref(),
                            ),
                            None,
                        ),
                    }
                },
            );
            // (core#544) The tree these checks judged — the one this unit's next attempt compares
            // against before it re-runs or carries this verdict.
            let mut report = report;
            report.tree = current_tree.clone();
            // (deliver honors an override) The result's exact identity — the tree the checks RAN
            // on, the base and the check set — so an operator's override of it can be stood on at
            // deliver, and only there, when all three are unchanged. Only a GUARDED floor is
            // keyed: it ran on a clean checkout of exactly that tree, as deliver's own floor does;
            // a floor on the live worktree (an untracked or ignored file could have steered it)
            // is never stood on in place of a clean one (xreview r1). The creator floor's key
            // carries the creator stage in its check-set digest, so deliver never stands on it.
            report.key = report.key_for(ran_on, &ctx);
            eprintln!(
                "wicked-core: repo checks floor ({}) for unit {}: {} — {}",
                stage.as_wire(),
                input.unit.ord,
                report.outcome().to_ascii_uppercase(),
                report.summary()
            );
            Some(report)
        }
        _ => None,
    };
    // F-036 WORKTREE GUARD, the FINAL comparison — the last thing this thread does before the
    // result is posted to the gate fold. Everything the phase owned has run and been quiesced by
    // now (the seat's process group is killed when it exits; each check's group likewise), so
    // this is the tree the gate is actually judging. The first look above only decided whether
    // the checks were worth running; the fold sees THIS.
    // Taken for EVERY status (Copilot on #433): a guarded seat that writes to the tree and then
    // exits failed/cancelled/timed-out never reaches the gate, but a human's Approve on ITS
    // failure clears the baseline and re-dispatches — against the leftover edit, unless it is
    // undone here. The fold reads the outcome only for a unit that folded Ok (below).
    let mut worktree_guard =
        crate::worktree_guard::outcome_for_unit(&input.unit, input.workdir.as_deref());
    // core#431 (F-3R2-010): a DENYING mutation is undone HERE, on the worker thread, before the
    // result reaches the gate — the engine runs the remedy its denial used to print for the
    // operator (`git read-tree --reset -u <before.tree>`), so the retry a human approves runs
    // against the creator's verified tree and never silently adopts the evaluator's edit. The
    // outcome carries `restored`/`restore_error`; the fold emits `worktreeRestored` and words
    // the denial accordingly. A failed restore is disclosed, never assumed.
    if let (Some(crate::worktree_guard::WorktreeGuardOutcome::Mutated(m)), Some(wd)) =
        (worktree_guard.as_mut(), input.workdir.as_deref())
    {
        if m.denies() {
            // F-433-008: the discarded edit is PINNED before the restore so it is never
            // gc-pruned — the #432 suggestion lane can read it back.
            let suggestion_ref = format!(
                "refs/wicked/suggestions/{}/{}/{}",
                crate::repo::sanitize_worktree_id(&input.run_id),
                input.unit.ord,
                input.attempt
            );
            match crate::worktree_guard::restore_creator_tree(wd, m, Some(&suggestion_ref)) {
                Ok(()) => eprintln!(
                    "wicked-core: unit {} (phase `{}`, executes_code:false) changed the tree it \
                     was reviewing — the evaluator's edit was discarded and the creator's tree {} \
                     restored in {} (F-036 / core#431)",
                    input.unit.ord,
                    input.unit.phase_id().unwrap_or("?"),
                    &m.before.tree[..m.before.tree.len().min(10)],
                    wd.display()
                ),
                Err(e) => eprintln!(
                    "wicked-core: unit {} changed the tree it was reviewing and the engine could \
                     NOT restore the creator's tree in {}: {e} — the denial keeps the manual \
                     remedy (core#431)",
                    input.unit.ord,
                    wd.display()
                ),
            }
        }
    }
    // (core#1024 follow-up) SEAT-SECRET GUARD — the unit's settled diff (everything it added since
    // its dispatch baseline: content, names, commit messages) must not carry a credential the
    // platform put into a seat. A match, or a scan that could not complete, FAILS the unit with a
    // reason naming the place and the credential's kind, never its value. Nothing to look for (no
    // seat was handed one by this process) = no scan.
    let mut output = output;
    if let (Some(wd), Some(baseline)) = (
        input.workdir.as_deref(),
        input.unit.worktree_baseline.as_ref(),
    ) {
        let refusal = match crate::seat_secret_guard::unit_leak(wd, baseline) {
            Ok(None) => None,
            Ok(Some(leak)) => {
                eprintln!(
                    "wicked-core: unit {} failed — {} (the value is not logged)",
                    input.unit.ord,
                    leak.describe()
                );
                Some(crate::seat_secret_guard::unit_refusal(
                    input.unit.ord,
                    &leak,
                ))
            }
            Err(why) => Some(format!(
                "{} — unit {} could not be checked for a credential the platform put into its seat \
                 ({why}); the unit failed so unchecked work is not carried on. Retry it.",
                crate::seat_secret_guard::SEAT_CREDENTIAL_MARKER,
                input.unit.ord
            )),
        };
        if let Some(text) = refusal {
            if output.status == StepStatus::Ok {
                output.status = StepStatus::Failed;
            }
            output.output = if output.output.is_empty() {
                text
            } else {
                format!("{text}\n\n{}", output.output)
            };
        }
    }
    // A unit that did not fold Ok fails on its own account and never reaches the gate this
    // evidence feeds — the outcome was taken (and any mutation undone) above; hand the fold
    // exactly what it used to see.
    if output.status != StepStatus::Ok {
        worktree_guard = None;
    }
    // (core#431, F-433-001) The tree the checks CERTIFIED: only when they ran and passed AND
    // the guard's final comparison says the tree is exactly what the seat left (Clean) — so the
    // after-snapshot, taken after the checks, is the tree the checks ran on.
    let verified_tree = match (&repo_checks, &worktree_guard) {
        (Some(report), Some(crate::worktree_guard::WorktreeGuardOutcome::Clean { after, .. }))
            if report.passed =>
        {
            Some(after.tree.clone())
        }
        _ => None,
    };
    let judge_refusals = judge_refusals.into_inner();
    // The pre-F-7R3-001 carrier keeps its meaning — authentication refusals only.
    let judge_auth_refusals: Vec<String> = judge_refusals
        .iter()
        .filter(|r| r.reason == wicked_council::types::SeatFailureReason::NotLoggedIn.as_str())
        .map(|r| r.seat.clone())
        .collect();
    let evidence = crate::workflow::UnitEvidence {
        worktree_guard,
        repo_checks,
        verified_tree,
        tree_changed,
        judge_skipped,
        judge_auth_refusals,
        judge_refusals,
        team: team_snapshot,
        qe_review,
        bench_reprobes,
    };
    (output, agent_verdict, evidence)
}

// ── The actor-thread publish seam (thread-local — dispatch_unit consults it) ─────────────────────────

thread_local! {
    /// The actor thread's bus publisher when exec-mediation is ON. `dispatch_unit` (which only ever runs
    /// on the actor thread) reads this: `Some` ⇒ publish `task.dispatched`; `None` (the default) ⇒ spawn
    /// the in-process worker as before. A thread-local is the clean way to make the mode available deep in
    /// the actor's private call tree WITHOUT threading a parameter through `launch_run_inner` /
    /// `advance_or_pause` / `confirm_gate` (whose signatures campaign.rs depends on — out of scope).
    static EXEC_PUBLISHER: RefCell<Option<BusDb>> = const { RefCell::new(None) };
}

/// Arm exec-mediation on the CURRENT (actor) thread with the bus handle a bus thread already opened
/// (DES-TEAMING-002 T0: the actor never opens the bus). The handle is the process-wide shared one, so
/// arming adds no connection.
pub(crate) fn arm_exec_publisher(db: BusDb) {
    EXEC_PUBLISHER.with(|cell| *cell.borrow_mut() = Some(db));
}

/// The actor's publish budget (finding #8): the `task.dispatched` INSERT runs on the single-writer
/// actor thread, so it waits at most this long for the shared connection and for SQLite's busy
/// handler; past it, `try_publish_dispatched` falls back to the in-process worker.
const PUBLISH_BUDGET: Duration = Duration::from_millis(250);

/// Disarm exec-mediation on the current thread (actor loop exit).
pub(crate) fn disarm_exec_publisher() {
    EXEC_PUBLISHER.with(|cell| *cell.borrow_mut() = None);
}

/// The bus exec mediation is armed over on THIS thread (the actor), if any — handed to the
/// in-process worker's judge when a publish falls back in-process.
pub(crate) fn armed_exec_bus() -> Option<String> {
    EXEC_PUBLISHER.with(|cell| cell.borrow().as_ref().map(|db| db.path().to_string()))
}

/// Whether exec-mediation is armed on THIS thread (the actor). `dispatch_unit` branches on this.
pub(crate) fn is_exec_enabled() -> bool {
    EXEC_PUBLISHER.with(|cell| cell.borrow().is_some())
}

/// Publish `task.dispatched` for one unit (the reducer's publish, on the actor thread). A bounded local
/// INSERT (see the module actor-safety note). Idempotent by the `(run, unit, attempt)` key so a re-issued
/// dispatch dedups. Returns `true` if published (exec mode armed), `false` if the in-process path should
/// run instead. A publish error is surfaced as `false` so the run still makes progress in-process rather
/// than wedging with no worker.
pub(crate) fn try_publish_dispatched(
    input: &StepInput,
    agent_review_target: Option<&str>,
    is_acp: bool,
) -> bool {
    EXEC_PUBLISHER.with(|cell| {
        let guard = cell.borrow();
        let Some(db) = guard.as_ref() else {
            return false;
        };
        // #7: blank the approved validator's shell SCRIPT before the unit is serialized onto the bus.
        let (unit, validator_pin) = strip_validator_script(&input.unit);
        let task = DispatchedTask {
            run_id: input.run_id.clone(),
            unit_ix: input.unit_ix,
            attempt: input.attempt,
            workflow_id: input.workflow_id.clone(),
            entity_mode: input.entity_mode,
            workdir: input
                .workdir
                .as_ref()
                .map(|p| p.to_string_lossy().to_string()),
            unit,
            agent_review_target: agent_review_target.map(|s| s.to_string()),
            cli: input.unit.assigned_cli.clone(),
            validator_pin,
            governance: input.governance.clone(),
            prior_outputs: input
                .prior_outputs
                .iter()
                .map(|p| PriorOutputWire {
                    label: p.label.clone(),
                    output: p.output.clone(),
                })
                .collect(),
            process_gen: input.process_gen,
            launch_seq: input.launch_seq,
            is_acp,
            required_skills: input.required_skills.clone(),
        };
        let payload = match serde_json::to_value(&task) {
            Ok(v) => v,
            Err(e) => {
                eprintln!(
                    "wicked-core: exec-mediation could not serialize task.dispatched for {}#{}: {e}; \
                     falling back to in-process dispatch",
                    input.run_id, input.unit_ix
                );
                return false;
            }
        };
        let key = task_key(
            TASK_DISPATCHED,
            &input.run_id,
            input.unit_ix,
            input.attempt,
            input.process_gen,
            input.launch_seq,
        );
        let ev = BusEmit::new(TASK_DISPATCHED, CORE_DOMAIN, "core.task", payload).with_key(key);
        match db.emit_bounded(&ev, PUBLISH_BUDGET) {
            Ok(Some(_)) => true,
            Ok(None) => {
                eprintln!(
                    "wicked-core: exec-mediation bus handle busy past {PUBLISH_BUDGET:?} for {}#{}; \
                     falling back to in-process dispatch",
                    input.run_id, input.unit_ix
                );
                false
            }
            Err(e) => {
                eprintln!(
                    "wicked-core: exec-mediation failed to publish task.dispatched for {}#{}: {e}; \
                     falling back to in-process dispatch",
                    input.run_id, input.unit_ix
                );
                false
            }
        }
    })
}

// ── The cli-runner SUBSCRIBER (off-actor: consumes task.dispatched → runs work → publishes task.completed) ─

// ── Durable-cursor consumer identities + atomic init (findings #1, #4, #5) ───────────────────────────

/// Legacy durable-cursor key (pre-DES-002). Retained for reference only — new consumers use
/// gen-scoped names: `"cli-runner-{actor_process_gen}"` and `"cli-runner-completed-{actor_process_gen}"`.
#[allow(dead_code)]
const CONSUMER_CLI_RUNNER_LEGACY: &str = "wicked-core.cli-runner";
/// Legacy durable-cursor key (pre-DES-002).
#[allow(dead_code)]
const CONSUMER_TASK_COMPLETED_LEGACY: &str = "wicked-core.task-completed";

/// Stable-key prefix stored in `core_exec_meta` to identify this Core instance across restarts.
/// Full key: `"{PREFIX}{workspace_id}"`.
const CORE_STABLE_ID_KEY_PREFIX: &str = "wicked-core-instance-stable-id-";

/// Derive the `cli-runner` consumer name for a given actor generation UUID.
fn consumer_name(gen: uuid::Uuid) -> String {
    format!("cli-runner-{gen}")
}

/// Derive the `task.completed` consumer name for a given actor generation UUID.
fn completed_consumer_name(gen: uuid::Uuid) -> String {
    format!("cli-runner-completed-{gen}")
}

/// Extract the generation UUID from a consumer name produced by `consumer_name(gen)`.
/// Returns `None` if the name does not have the expected prefix or UUID is invalid.
fn gen_from_consumer_name(name: &str) -> Option<uuid::Uuid> {
    name.strip_prefix("cli-runner-")
        .and_then(|u| uuid::Uuid::parse_str(u).ok())
}

/// Resolve a consumer's START floor: its DURABLE cursor if one exists (RESUME across a crash/restart —
/// the LOST-ON-CRASH fix), else the bus tail on a true first run (start at latest, never replay
/// history). `None` ⇒ the cursor row could not be read AND the tail could not be snapshotted → the
/// caller must NOT arm exec-mediation (refuse to replay from 0), leaving the in-process path.
fn resume_floor(db: &BusDb, consumer: &str) -> Option<i64> {
    match db.load_cursor(consumer) {
        Ok(Some(floor)) => Some(floor), // resume from the persisted cursor
        Ok(None) => db.tail_event_id().ok(), // true first run → start at the tail (no replay)
        Err(_) => None,                 // cursor unreadable → disable (don't replay from 0)
    }
}

/// Persist a consumer's durable cursor, logging (never failing the loop) on a write error. The floor and
/// the persisted cursor must advance TOGETHER so a restart resumes exactly where the consumer left off.
fn persist_cursor(db: &BusDb, consumer: &str, id: i64) {
    if let Err(e) = db.save_cursor(consumer, id) {
        eprintln!("wicked-core: {consumer} could not persist cursor at {id}: {e}");
    }
}

/// Both exec-mediation consumers, each with the bus handle and a RESOLVED start floor — built on a bus
/// thread (never the actor, DES-TEAMING-002 T0) BEFORE the publisher is armed (the ATOMIC-ARM invariant, finding #4). Owning the open
/// connections here (rather than opening lazily inside each spawned thread) is what makes "both consumers
/// can initialize" a fact the caller checks before arming: if either can't open its bus db or resolve its
/// cursor, [`init_exec_consumers`] returns `None` and the caller leaves exec-mediation OFF, so a
/// `task.dispatched` is never published with no runner to consume it.
pub(crate) struct ExecConsumers {
    cli_runner_db: BusDb,
    cli_runner_floor: i64,
    completed_db: BusDb,
    completed_floor: i64,
    /// Per-process consumer name for the `task.dispatched` cursor (DES-002 startup reclamation).
    /// Format: `"cli-runner-{actor_process_gen}"`.
    consumer_name: String,
    /// Per-process consumer name for the `task.completed` cursor.
    /// Format: `"cli-runner-completed-{actor_process_gen}"`.
    completed_consumer_name: String,
    /// Generation UUID of the predecessor consumer (from startup reclamation). `Some` when a
    /// prior process left cursor rows that were migrated; `None` on first boot or clean shutdown.
    predecessor_gen: Option<uuid::Uuid>,
}

/// Perform startup cursor reclamation for a Core instance (DES-002 mechanism B):
///  1. Look up any predecessor cursor names stored under the stable owner key.
///  2. Migrate cursor positions from old names → new names (BEFORE deleting old rows).
///  3. Delete old cursor rows (BEFORE calling `set_stable`).
///  4. Set stable to the new consumer name.
///
/// Returns `Some(predecessor_gen)` on success (where `predecessor_gen` is `None` when no predecessor
/// existed and `Some(gen)` when one was migrated). Returns `None` on any bus DB error (fail closed —
/// a DB error is treated as "cannot safely reclaim"; exec-mediation is disabled by the caller).
///
/// **Ordering invariant (T7-e)**: migrate first, delete second, set_stable third. Inverting any step
/// causes either data loss (delete without migrate) or perpetual "no predecessor" ghost (set_stable
/// before get_stable returns the new name to itself).
pub(crate) fn reclaim_predecessor_cursors(
    db: &BusDb,
    workspace_id: &str,
    new_consumer: &str,
    new_completed: &str,
) -> Option<Option<uuid::Uuid>> {
    // Step 1: look up (or create) the stable Core-instance ID. This ID MUST survive restarts —
    // using the same key each time is what lets the new process find the predecessor's cursor names.
    let stable_key = format!("{CORE_STABLE_ID_KEY_PREFIX}{workspace_id}");
    let core_stable_id = match db.get_stable(&stable_key) {
        Ok(Some(id)) => id,
        Ok(None) => {
            let new_id = uuid::Uuid::new_v4().to_string();
            if let Err(e) = db.set_stable(&stable_key, &new_id) {
                eprintln!("wicked-core: reclamation could not create core stable id: {e}");
                return None;
            }
            new_id
        }
        Err(e) => {
            eprintln!("wicked-core: reclamation failed to read core stable id: {e}");
            return None;
        }
    };

    // Step 2: look up the predecessor consumer name.
    let owner_key = format!("cli-runner-cursor-owner-{core_stable_id}");
    let old_consumer_opt: Option<String> = match db.get_stable(&owner_key) {
        Ok(v) => v.filter(|c| c.as_str() != new_consumer),
        Err(e) => {
            eprintln!("wicked-core: reclamation failed to read cursor owner: {e}");
            return None;
        }
    };

    // Derive predecessor_gen BEFORE consuming old_consumer_opt below.
    let predecessor_gen: Option<uuid::Uuid> =
        old_consumer_opt.as_deref().and_then(gen_from_consumer_name);

    // Step 3: migrate + delete (ordering is critical — T7-e).
    if let Some(ref old_consumer) = old_consumer_opt {
        // Reconstruct the completed consumer name from the old consumer name.
        let old_uuid = old_consumer.strip_prefix("cli-runner-").unwrap_or("");
        let old_completed = format!("cli-runner-completed-{old_uuid}");

        // MIGRATE cursor positions FIRST.
        match db.load_cursor(old_consumer) {
            Ok(Some(pos)) => {
                if let Err(e) = db.save_cursor(new_consumer, pos) {
                    eprintln!("wicked-core: reclamation could not migrate dispatch cursor: {e}");
                    return None;
                }
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!(
                    "wicked-core: reclamation could not read predecessor dispatch cursor: {e}"
                );
                return None;
            }
        }
        match db.load_cursor(&old_completed) {
            Ok(Some(pos)) => {
                if let Err(e) = db.save_cursor(new_completed, pos) {
                    eprintln!("wicked-core: reclamation could not migrate completed cursor: {e}");
                    return None;
                }
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!(
                    "wicked-core: reclamation could not read predecessor completed cursor: {e}"
                );
                return None;
            }
        }
        // DELETE old rows AFTER migration, BEFORE set_stable.
        if let Err(e) = db.delete_cursor(old_consumer) {
            eprintln!("wicked-core: reclamation could not delete predecessor dispatch cursor: {e}");
            return None;
        }
        if let Err(e) = db.delete_cursor(&old_completed) {
            eprintln!(
                "wicked-core: reclamation could not delete predecessor completed cursor: {e}"
            );
            return None;
        }
    }

    // Step 4: set_stable LAST.
    if let Err(e) = db.set_stable(&owner_key, new_consumer) {
        eprintln!("wicked-core: reclamation could not update cursor owner: {e}");
        return None;
    }

    Some(predecessor_gen)
}

/// Initialize BOTH consumers against `bus_db_path` (finding #4 — atomicity). Returns `None` if EITHER
/// consumer cannot open the bus db or resolve its durable cursor; the caller then does NOT arm the
/// publisher (the in-process path stands). Runs on a bus thread, never the actor (DES-TEAMING-002 T0);
/// both consumers hold clones of the ONE process-wide bus handle ([`BusDb::shared`]), MOVED into the
/// consumer threads by [`spawn_exec_consumers`], so a successful init here == a working bus handle in
/// the thread — no second-open race that could leave the publisher armed with a dead consumer.
///
/// `workspace_id` uniquely identifies this Core instance (used as the stable-key scope so different
/// Core actors sharing one bus db don't collide). `actor_process_gen` is the UUID generated once per
/// `actor::run` invocation; it becomes the suffix of the per-process consumer names.
pub(crate) fn init_exec_consumers(
    bus_db_path: &str,
    workspace_id: &str,
    actor_process_gen: uuid::Uuid,
) -> Option<ExecConsumers> {
    let c_name = consumer_name(actor_process_gen);
    let cc_name = completed_consumer_name(actor_process_gen);

    let cli_runner_db = BusDb::shared(bus_db_path)
        .map_err(|e| eprintln!("wicked-core: cli-runner cannot open bus db {bus_db_path}: {e}"))
        .ok()?;

    // Startup reclamation (DES-002 mechanism B) — migrate predecessor cursor rows.
    // Returns None on DB error (fail closed — exec-mediation stays OFF).
    // Returns Some(None) on first boot / clean shutdown predecessor.
    // Returns Some(Some(gen)) when a crashed predecessor's cursors were migrated.
    let predecessor_gen: Option<uuid::Uuid> =
        reclaim_predecessor_cursors(&cli_runner_db, workspace_id, &c_name, &cc_name)?;

    let cli_runner_floor = resume_floor(&cli_runner_db, &c_name)?;
    let completed_db = BusDb::shared(bus_db_path)
        .map_err(|e| {
            eprintln!("wicked-core: task.completed poller cannot open bus db {bus_db_path}: {e}")
        })
        .ok()?;
    let completed_floor = resume_floor(&completed_db, &cc_name)?;
    Some(ExecConsumers {
        cli_runner_db,
        cli_runner_floor,
        completed_db,
        completed_floor,
        consumer_name: c_name,
        completed_consumer_name: cc_name,
        predecessor_gen,
    })
}

/// Spawn both off-actor consumer threads from a pre-initialized [`ExecConsumers`]. Called ONLY after the
/// publisher is armed, so arm+consumers land together (finding #4).
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_exec_consumers(
    consumers: ExecConsumers,
    runner: Arc<dyn StepRunner>,
    tx: Sender<Command>,
    lifecycle_maps: Option<Arc<std::sync::Mutex<ElicitationMaps>>>,
    actor_process_gen: uuid::Uuid,
    poll_interval: Duration,
    stop: Arc<AtomicBool>,
    team: Option<crate::team::runner::TeamRunner>,
) -> Vec<JoinHandle<()>> {
    let ExecConsumers {
        cli_runner_db,
        cli_runner_floor,
        completed_db,
        completed_floor,
        consumer_name,
        completed_consumer_name,
        predecessor_gen,
    } = consumers;
    vec![
        run_cli_runner(
            cli_runner_db,
            cli_runner_floor,
            runner,
            tx.clone(),
            lifecycle_maps,
            actor_process_gen,
            predecessor_gen,
            consumer_name,
            completed_consumer_name.clone(),
            poll_interval,
            stop.clone(),
            team,
        ),
        run_task_completed_poller(
            completed_db,
            completed_floor,
            completed_consumer_name,
            tx,
            actor_process_gen,
            predecessor_gen,
            poll_interval,
            stop,
        ),
    ]
}

/// Bounded-join then DETACH an exec consumer thread at shutdown (finding #5). The `cli-runner` may be
/// mid-CLI (an unbounded subprocess) when `stop` is set — the flag is only observed at poll boundaries,
/// so a straight `join()` would block shutdown (and the actor's store release) for the CLI's full
/// duration, unlike the detached in-process worker. We wait up to `timeout` for a clean exit, then detach
/// (drop the handle) and rely on the stop flag + process exit. The consumer holds NO store handle, so
/// detaching is store-safe.
pub(crate) fn join_bounded(handle: JoinHandle<()>, timeout: Duration) {
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = handle.join();
        let _ = done_tx.send(());
    });
    let _ = done_rx.recv_timeout(timeout);
}

// ── The cli-runner SUBSCRIBER (off-actor: consumes task.dispatched → runs work → publishes task.completed) ─

/// Sleep `interval` in short slices, honoring `stop` promptly (shared cancellable-wait helper).
fn cancellable_sleep(stop: &Arc<AtomicBool>, interval: Duration) {
    let slice = Duration::from_millis(50);
    let mut slept = Duration::ZERO;
    while slept < interval && !stop.load(Ordering::SeqCst) {
        std::thread::sleep(slice);
        slept += slice;
    }
}

/// The `cli-runner` subscriber loop (own bus connection MOVED in, no store handle): poll
/// `wicked.task.dispatched` from `floor_init`, run each unit's work via the SAME `runner`, publish
/// `wicked.task.completed`, and PERSIST the durable cursor after each handled event so a restart RESUMES
/// here instead of re-snapshotting to the tail (the LOST-ON-CRASH fix, #1). Idempotent: an in-process
/// dedup set skips a `(run, unit, attempt, process_gen, launch_seq)` already completed, and the
/// completed event's deterministic key dedups across restarts. At-least-once: the floor advances (and
/// the cursor persists) only after a successful publish, so a transient publish fault re-attempts rather
/// than dropping the task.
///
/// **DES-002 bus consumer logic (T7)**:
///  - Foreign tasks (`process_gen` != `actor_process_gen`): advance cursor and skip.
///  - Predecessor tasks (`process_gen` == `predecessor_gen`): check `find_completed` for a real result;
///    if found apply it (with ack-gated cursor advance); else emit synthetic `ElicitationFailed` + ack.
///  - Own tasks: check epoch activation via `try_next_epoch_bus`; on degraded mode (activated but no
///    worker in-flight) emit synthetic `ElicitationFailed` + ack; else run normally.
///
/// LIVE OUTPUT (parity gap #11 closed): `actor_tx` is a clone of the actor's `self_tx`. The unit's
/// incremental output is streamed to the actor's single emit point via `Command::CliOutputDelta` — the
/// SAME write-back the in-process worker uses — so the studio's live pane ticks under exec-mediation
/// with byte-identical streaming. This reaches the actor ONLY over the command channel (no store handle)
/// and works because the `cli-runner` is co-process with the actor (see the module doc's HONEST LIMIT).
#[allow(clippy::too_many_arguments)]
fn run_cli_runner(
    db: BusDb,
    floor_init: i64,
    runner: Arc<dyn StepRunner>,
    actor_tx: Sender<Command>,
    lifecycle_maps: Option<Arc<std::sync::Mutex<ElicitationMaps>>>,
    actor_process_gen: uuid::Uuid,
    predecessor_gen: Option<uuid::Uuid>,
    consumer_name: String,
    completed_consumer_name: String,
    poll_interval: Duration,
    stop: Arc<AtomicBool>,
    team: Option<crate::team::runner::TeamRunner>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut floor = floor_init;
        // The `(run, unit, attempt, process_gen_str, launch_seq)` keys already completed in THIS
        // process — the at-least-once dedup that stops a redelivered dispatch from re-running the CLI.
        let mut done: std::collections::HashSet<String> = std::collections::HashSet::new();

        while !stop.load(Ordering::SeqCst) {
            let events = match db.poll(TASK_DISPATCHED, floor, 100) {
                Ok(evs) => evs,
                Err(e) => {
                    eprintln!("wicked-core: cli-runner poll error: {e}");
                    Vec::new()
                }
            };
            for ev in events {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                let task: DispatchedTask = match serde_json::from_value(ev.payload.clone()) {
                    Ok(t) => t,
                    Err(e) => {
                        // Poison payload — advance past it (retrying can never parse it).
                        eprintln!(
                            "wicked-core: cli-runner dropping unparseable task.dispatched {} ({e})",
                            ev.event_id
                        );
                        floor = ev.event_id;
                        persist_cursor(&db, &consumer_name, floor);
                        continue;
                    }
                };

                // ── DES-002 generation check ────────────────────────────────────────
                // Legacy tasks (no process_gen) are discarded — they predate the bus consumer
                // tracking protocol and have no stale-completion guard.
                let task_gen = match task.process_gen {
                    Some(g) => g,
                    None => {
                        // Pre-DES-002 payload: no generation tracking; skip silently.
                        floor = ev.event_id;
                        persist_cursor(&db, &consumer_name, floor);
                        continue;
                    }
                };

                if task_gen != actor_process_gen {
                    if Some(task_gen) == predecessor_gen {
                        // ── Predecessor path ────────────────────────────────────────
                        // This task was dispatched by the previous process (now dead). Check
                        // find_completed first: the predecessor may have finished the work and
                        // published task.completed before crashing without advancing its cursor.
                        let real_completion = db
                            .find_completed(&completed_consumer_name, &task.run_id, task.launch_seq)
                            .ok()
                            .flatten();

                        if let Some(completion_ev) = real_completion {
                            // Real completion found — apply it and gate cursor advance on ack.
                            let completed: CompletedTask = match serde_json::from_value(
                                completion_ev.payload.clone(),
                            ) {
                                Ok(c) => c,
                                Err(e) => {
                                    eprintln!(
                                            "wicked-core: cli-runner predecessor completion unparseable \
                                             for {}#{}: {e}",
                                            task.run_id, task.unit_ix
                                        );
                                    floor = ev.event_id;
                                    persist_cursor(&db, &consumer_name, floor);
                                    continue;
                                }
                            };
                            let output = StepOutput {
                                run_id: completed.run_id.clone(),
                                unit_ix: completed.unit_ix,
                                attempt: completed.attempt,
                                output: completed.output.clone(),
                                status: status_from_str(&completed.status),
                                usage: completed.usage.clone(),
                                files: completed.files.clone(),
                                tools: completed.tools.clone(),
                                governed: completed.governed,
                            };
                            let evidence = completed.evidence.clone();
                            let agent_verdict = completed
                                .agent_verdict
                                .map(crate::validator::AgentVerdict::from);
                            let (ack_tx, ack_rx) = std::sync::mpsc::sync_channel::<()>(0);
                            if actor_tx
                                .send(Command::ApplyStepResult {
                                    output,
                                    agent_verdict,
                                    evidence: Box::new(evidence),
                                    process_gen: task.process_gen,
                                    launch_seq: task.launch_seq,
                                    ack: Some(ack_tx),
                                })
                                .is_ok()
                                && ack_rx.recv().is_ok()
                            {
                                floor = ev.event_id;
                                persist_cursor(&db, &consumer_name, floor);
                                // Also advance the completed cursor past this event.
                                persist_cursor(
                                    &db,
                                    &completed_consumer_name,
                                    completion_ev.event_id,
                                );
                            }
                            continue;
                        }

                        // No real completion found — predecessor is truly dead. Emit synthetic terminal.
                        let failed_output = StepOutput {
                            status: StepStatus::ElicitationFailed,
                            output: String::new(),
                            run_id: task.run_id.clone(),
                            unit_ix: task.unit_ix,
                            attempt: task.attempt,
                            usage: None,
                            files: Vec::new(),
                            tools: Vec::new(),
                            governed: false,
                        };
                        let (ack_tx, ack_rx) = std::sync::mpsc::sync_channel::<()>(0);
                        if actor_tx
                            .send(Command::ApplyStepResult {
                                output: failed_output,
                                agent_verdict: None,
                                evidence: Default::default(),
                                process_gen: task.process_gen,
                                launch_seq: task.launch_seq,
                                ack: Some(ack_tx),
                            })
                            .is_ok()
                            && ack_rx.recv().is_ok()
                        {
                            floor = ev.event_id;
                            persist_cursor(&db, &consumer_name, floor);
                        }
                        continue;
                    }

                    // Truly foreign task from a different live actor — advance cursor to unblock.
                    floor = ev.event_id;
                    persist_cursor(&db, &consumer_name, floor);
                    continue;
                }

                // ── Own task (task_gen == actor_process_gen) ────────────────────────

                // Extended dedup key includes process_gen + launch_seq (plan step 8).
                let dedup = {
                    let gen_str = task.process_gen.map(|u| u.to_string()).unwrap_or_default();
                    deterministic_key(&[
                        "done",
                        &task.run_id,
                        &task.unit_ix.to_string(),
                        &task.attempt.to_string(),
                        &gen_str,
                        &task.launch_seq.to_string(),
                    ])
                };
                if done.contains(&dedup) {
                    floor = ev.event_id; // already handled — advance past the redelivery
                    persist_cursor(&db, &consumer_name, floor);
                    continue;
                }

                // ── Epoch activation via try_next_epoch_bus ─────────────────────────
                // Check has_activated_seq / is_bus_worker_in_flight BEFORE calling try_next_epoch_bus
                // to detect the degraded scenario.
                let elicitation_epoch = if let Some(ref maps_arc) = lifecycle_maps {
                    let mut maps = maps_arc.lock().unwrap_or_else(|p| p.into_inner());

                    if maps.has_activated_seq(&task.run_id, task.launch_seq) {
                        if maps.is_bus_worker_in_flight(&task.run_id, task.launch_seq) {
                            // Worker still running — re-poll next interval without advancing cursor.
                            drop(maps);
                            continue;
                        }
                        // Degraded mode: task was activated but worker is gone (crash recovery).
                        drop(maps);
                        let failed_output = StepOutput {
                            status: StepStatus::ElicitationFailed,
                            output: String::new(),
                            run_id: task.run_id.clone(),
                            unit_ix: task.unit_ix,
                            attempt: task.attempt,
                            usage: None,
                            files: Vec::new(),
                            tools: Vec::new(),
                            governed: false,
                        };
                        let (ack_tx, ack_rx) = std::sync::mpsc::sync_channel::<()>(0);
                        if actor_tx
                            .send(Command::ApplyStepResult {
                                output: failed_output,
                                agent_verdict: None,
                                evidence: Default::default(),
                                process_gen: task.process_gen,
                                launch_seq: task.launch_seq,
                                ack: Some(ack_tx),
                            })
                            .is_ok()
                            && ack_rx.recv().is_ok()
                        {
                            floor = ev.event_id;
                            persist_cursor(&db, &consumer_name, floor);
                        }
                        continue;
                    }

                    match maps.try_next_epoch_bus(&task.run_id, task.launch_seq, task.is_acp) {
                        Some(ep) => {
                            drop(maps);
                            ep
                        }
                        None => {
                            // Stale / cancelled — discard and advance.
                            drop(maps);
                            floor = ev.event_id;
                            persist_cursor(&db, &consumer_name, floor);
                            continue;
                        }
                    }
                } else {
                    0 // non-ACP / no lifecycle maps
                };

                // ── Normal execution path ───────────────────────────────────────────
                let input = StepInput {
                    run_id: task.run_id.clone(),
                    unit_ix: task.unit_ix,
                    attempt: task.attempt,
                    unit: task.unit.clone(),
                    workflow_id: task.workflow_id.clone(),
                    entity_mode: task.entity_mode,
                    workdir: task.workdir.clone().map(std::path::PathBuf::from),
                    governance: task.governance.clone(),
                    prior_outputs: task
                        .prior_outputs
                        .into_iter()
                        .map(|p| crate::workflow::PriorUnitOutput {
                            label: p.label,
                            output: p.output,
                        })
                        .collect(),
                    elicitation_epoch,
                    process_gen: task.process_gen,
                    launch_seq: task.launch_seq,
                    required_skills: task.required_skills.clone(),
                };
                // Live-output sink (parity gap #11): stream each chunk to the actor's single emit
                // point as a `Command::CliOutputDelta`, exactly as the in-process worker does.
                let delta_run_id = task.run_id.clone();
                let delta_ord = task.unit.ord;
                let delta_attempt = task.attempt; // in-band attempt for the throttled stream
                let delta_process_gen = task.process_gen;
                let delta_launch_seq = task.launch_seq;
                let delta_tx = std::sync::Mutex::new(actor_tx.clone());
                let emit_delta = move |chunk: &str| {
                    if let Ok(g) = delta_tx.lock() {
                        let _ = g.send(Command::CliOutputDelta {
                            run_id: delta_run_id.clone(),
                            ord: delta_ord,
                            attempt: delta_attempt,
                            chunk: chunk.to_string(),
                            process_gen: delta_process_gen,
                            launch_seq: delta_launch_seq,
                        });
                    }
                };
                let (output, agent_verdict, evidence) = run_unit_and_judge(
                    &runner,
                    &input,
                    task.agent_review_target.as_deref(),
                    &emit_delta,
                    // Neither the run's roster nor its bench is on the `DispatchedTask` wire
                    // (F-7R2-006): the bus-mediated judge picks from the whole registry roster.
                    &[],
                    &[],
                    &[],
                    None,
                    // Exec mediation's judge publishes over the bus this consumer runs on.
                    Some(db.path()),
                    // T5: the SAME attempt-runner seam as the in-process worker.
                    team.as_ref(),
                );
                let completed = CompletedTask {
                    run_id: output.run_id.clone(),
                    unit_ix: output.unit_ix,
                    attempt: output.attempt,
                    output: output.output.clone(),
                    status: status_to_str(output.status).to_string(),
                    agent_verdict: agent_verdict.map(AgentVerdictWire::from),
                    usage: output.usage.clone(),
                    files: output.files.clone(),
                    tools: output.tools.clone(),
                    governed: output.governed,
                    evidence,
                    process_gen: task.process_gen,
                    launch_seq: task.launch_seq,
                };
                let payload = match serde_json::to_value(&completed) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!(
                            "wicked-core: cli-runner could not serialize task.completed for {}#{}: {e}",
                            task.run_id, task.unit_ix
                        );
                        if let Some(ref maps) = lifecycle_maps {
                            maps.lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .clear_bus_in_flight(&task.run_id, task.launch_seq);
                        }
                        floor = ev.event_id; // can't ever serialize — don't wedge the batch
                        persist_cursor(&db, &consumer_name, floor);
                        continue;
                    }
                };
                let key = task_key(
                    TASK_COMPLETED,
                    &task.run_id,
                    task.unit_ix,
                    task.attempt,
                    task.process_gen,
                    task.launch_seq,
                );
                let ev_out =
                    BusEmit::new(TASK_COMPLETED, CORE_DOMAIN, "core.task", payload).with_key(key);
                match db.emit(&ev_out) {
                    Ok(_) => {
                        if let Some(ref maps) = lifecycle_maps {
                            maps.lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .clear_bus_in_flight(&task.run_id, task.launch_seq);
                        }
                        done.insert(dedup);
                        floor = ev.event_id; // handled — advance the floor + persist the durable cursor
                        persist_cursor(&db, &consumer_name, floor);
                    }
                    // Transient publish fault → do NOT advance; break the batch and re-poll (at-least-once).
                    Err(e) => {
                        if let Some(ref maps) = lifecycle_maps {
                            maps.lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .reset_bus_activation(&task.run_id, task.launch_seq);
                        }
                        eprintln!(
                            "wicked-core: cli-runner could not publish task.completed for {} (transient, \
                             will retry): {e}",
                            ev.event_id
                        );
                        break;
                    }
                }
            }
            cancellable_sleep(&stop, poll_interval);
        }
    })
}

// ── The actor-inbound poller (off-actor: task.completed → Command::ApplyStepResult) ──────────────────

/// The reducer's inbound poller loop (own bus connection MOVED in): read `wicked.task.completed` from
/// `floor_init` and post a `Command::ApplyStepResult` to the actor over `tx` — the same command the
/// in-process worker posts. Cursor advance is GATED on the actor's ack (a rendezvous sync_channel
/// send): if the actor dies between dequeue and commit the ack never arrives, the cursor stays behind,
/// and the event is redelivered on the next restart (T7-g). Exits when `stop` is set or the actor is gone.
#[allow(clippy::too_many_arguments)]
fn run_task_completed_poller(
    db: BusDb,
    floor_init: i64,
    consumer_name: String,
    tx: Sender<Command>,
    actor_process_gen: uuid::Uuid,
    predecessor_gen: Option<uuid::Uuid>,
    poll_interval: Duration,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut floor = floor_init;
        while !stop.load(Ordering::SeqCst) {
            let events = match db.poll(TASK_COMPLETED, floor, 100) {
                Ok(evs) => evs,
                Err(e) => {
                    eprintln!("wicked-core: task.completed poll error: {e}");
                    Vec::new()
                }
            };
            for ev in events {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                let task: CompletedTask = match serde_json::from_value(ev.payload.clone()) {
                    Ok(t) => t,
                    Err(e) => {
                        eprintln!(
                            "wicked-core: task.completed poller dropping unparseable event {} ({e})",
                            ev.event_id
                        );
                        floor = ev.event_id;
                        persist_cursor(&db, &consumer_name, floor);
                        continue;
                    }
                };
                // This cursor may share a bus with other live Core actors. Apply only
                // completions owned by this process or its single reclaimed predecessor;
                // foreign/legacy completions must never mutate this actor's store.
                let owned_generation = task.process_gen == Some(actor_process_gen)
                    || (task.process_gen.is_some() && task.process_gen == predecessor_gen);
                if !owned_generation {
                    floor = ev.event_id;
                    persist_cursor(&db, &consumer_name, floor);
                    continue;
                }
                let output = StepOutput {
                    run_id: task.run_id,
                    unit_ix: task.unit_ix,
                    attempt: task.attempt,
                    output: task.output,
                    status: status_from_str(&task.status),
                    usage: task.usage,
                    files: task.files,
                    tools: task.tools,
                    governed: task.governed,
                };
                let agent_verdict = task.agent_verdict.map(crate::validator::AgentVerdict::from);
                let evidence = task.evidence.clone();
                // Reach the actor via the command channel (self_tx write-back). Gate cursor advance
                // on the ack so a crash between dequeue and commit leaves the cursor behind for
                // redelivery (T7-g invariant). A closed channel ⇒ actor is gone → exit.
                let (ack_tx, ack_rx) = std::sync::mpsc::sync_channel::<()>(0);
                if tx
                    .send(Command::ApplyStepResult {
                        output,
                        agent_verdict,
                        evidence: Box::new(evidence),
                        process_gen: task.process_gen,
                        launch_seq: task.launch_seq,
                        ack: Some(ack_tx),
                    })
                    .is_err()
                {
                    return; // actor gone
                }
                // Advance cursor only after ack — recv() Err means actor died mid-processing.
                if ack_rx.recv().is_ok() {
                    floor = ev.event_id;
                    persist_cursor(&db, &consumer_name, floor);
                }
            }
            cancellable_sleep(&stop, poll_interval);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// core#282 — the review target handed to an Evaluator's worker is BOUNDED with head+TAIL
    /// elision. An under-cap target passes through untouched; an oversized one keeps its opening
    /// AND its ending (creators put conclusions last) around an explicit harness marker.
    #[test]
    fn an_oversized_review_target_is_elided_keeping_both_head_and_tail() {
        // Under the cap: byte-identical passthrough.
        let small = "a modest creator output".to_string();
        assert_eq!(clip_review_target(small.clone()), small);

        // Over the cap: head start survives, tail end survives, marker names the elision.
        let head_sentinel = "HEAD-SENTINEL-AT-BYTE-ZERO ";
        let tail_sentinel = " TAIL-SENTINEL-AT-THE-END";
        let big = format!(
            "{head_sentinel}{}{tail_sentinel}",
            "x".repeat(REVIEW_TARGET_HEAD + REVIEW_TARGET_TAIL + 10_000)
        );
        let original_len = big.len();
        let clipped = clip_review_target(big);
        assert!(
            clipped.starts_with(head_sentinel),
            "the head of the creator output must survive the elision"
        );
        assert!(
            clipped.ends_with(tail_sentinel),
            "the TAIL of the creator output must survive — conclusions come last by convention"
        );
        assert!(
            clipped.contains("[...elided by harness:"),
            "the evaluator must be told it is looking at an excerpt: {}",
            &clipped[..200]
        );
        assert!(
            clipped.len() < original_len,
            "the clipped form must actually be smaller"
        );
        assert!(
            clipped.len() <= REVIEW_TARGET_HEAD + REVIEW_TARGET_TAIL + 256,
            "head + tail + marker is the whole budget; got {} bytes",
            clipped.len()
        );

        // Multibyte safety: the single-byte bookends throw BOTH raw cut points off the 2-byte
        // char grid — the head cut lands mid-char (slides down) and the tail cut lands mid-char
        // (slides up). Must produce valid UTF-8, not panic.
        let multibyte = format!(
            "a{}b",
            "é".repeat((REVIEW_TARGET_HEAD + REVIEW_TARGET_TAIL) / 2 + 4_096)
        );
        let clipped = clip_review_target(multibyte);
        assert!(clipped.contains("[...elided by harness:"));
        assert!(clipped.starts_with('a') && clipped.ends_with('b'));
    }

    /// The coverage LAYER-2 finding: an Evaluator-role unit that produces its own analytical deliverable
    /// must be judged on that deliverable, not on the prior creator's cold output. Feeding the judge the
    /// upstream domain-model narrative made it reject a correct `coverage-report.json` (766/1.0) as "not a
    /// coverage computation". A PURE reviewer (Evaluator, no deliverable) must still judge the creator's
    /// work. Mutation: drop the deliverable branch of `select_work_for_agent` (always use
    /// `agent_review_target`) and the first block's asserts fail — the judge would see the narrative.
    #[test]
    fn evaluator_with_a_deliverable_is_judged_on_the_deliverable_not_the_creator_output() {
        let dir = std::env::temp_dir().join(format!("wcov_judge_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let report = r#"{"behavior_bearing":766,"coverage":1.0,"unaccounted":0}"#;
        std::fs::write(dir.join("coverage-report.json"), report).unwrap();
        let deliverables = vec!["coverage-report.json".to_string()];
        let narrative = "a narrative transcript of domain-model edits (BR-* additions/corrections)";

        // Evaluator WITH its own deliverable → judge sees the deliverable, not the creator narrative.
        let work = select_work_for_agent(
            crate::workflow::PhaseRole::Evaluator,
            &deliverables,
            Some(dir.as_path()),
            Some(narrative),
            "own worker transcript",
        );
        assert!(
            work.contains("\"coverage\":1.0") && work.contains("behavior_bearing"),
            "the coverage-report.json deliverable must be what the judge reviews: {work}"
        );
        assert!(
            !work.contains("narrative transcript"),
            "the judge must NOT be fed the upstream creator narrative for a deliverable-bearing evaluator"
        );

        // A PURE reviewer (Evaluator, NO deliverable) still judges the creator's work — guard
        // adversarial-review, whose target is correctly the creator's cold output.
        let reviewer = select_work_for_agent(
            crate::workflow::PhaseRole::Evaluator,
            &[],
            Some(dir.as_path()),
            Some(narrative),
            "own worker transcript",
        );
        assert_eq!(
            reviewer, narrative,
            "a reviewer with no deliverable of its own must still be judged on the creator's work"
        );

        // A Creator/Neutral unit judges its own output (no agent_review_target).
        let creator = select_work_for_agent(
            crate::workflow::PhaseRole::Creator,
            &deliverables,
            Some(dir.as_path()),
            None,
            "own worker transcript",
        );
        assert_eq!(
            creator, "own worker transcript",
            "a non-Evaluator unit is judged on its own output regardless of deliverables"
        );

        // PATH-ESCAPE GUARD (Copilot review on #229): a `..`-escaping (or absolute) declared deliverable
        // must NOT be read into the judge input — a WorkflowDef is arbitrary author data. Plant a secret
        // OUTSIDE the worktree and declare a `..` path to it; the judge must fall back to the creator
        // work, never the secret. Mutation: drop the is_absolute/ParentDir skip and this fails.
        let secret = dir
            .parent()
            .unwrap()
            .join(format!("wcov_secret_{}.txt", std::process::id()));
        std::fs::write(&secret, "SECRET-OUTSIDE-WORKTREE").unwrap();
        let escaping = vec![format!(
            "../{}",
            secret.file_name().unwrap().to_str().unwrap()
        )];
        let guarded = select_work_for_agent(
            crate::workflow::PhaseRole::Evaluator,
            &escaping,
            Some(dir.as_path()),
            Some(narrative),
            "own worker transcript",
        );
        assert!(
            !guarded.contains("SECRET-OUTSIDE-WORKTREE"),
            "a ..-escaping deliverable must never be read into the judge input: {guarded}"
        );
        assert_eq!(
            guarded, narrative,
            "an escaping deliverable falls back to the creator-output target (fail-closed)"
        );

        // SYMLINK-ESCAPE GUARD (Copilot review on #229): a deliverable whose path is textually relative
        // and non-escaping, but is an in-worktree SYMLINK pointing OUTSIDE, must NOT be read into the
        // judge input. The worker can write such a symlink (the boundary allows writes to in-worktree
        // paths). Mutation: drop the canonicalize/starts_with containment check and this reads the secret.
        #[cfg(unix)]
        {
            let link = dir.join("linked-report.json");
            std::os::unix::fs::symlink(&secret, &link).unwrap();
            let via_symlink = select_work_for_agent(
                crate::workflow::PhaseRole::Evaluator,
                &["linked-report.json".to_string()],
                Some(dir.as_path()),
                Some(narrative),
                "own worker transcript",
            );
            assert!(
                !via_symlink.contains("SECRET-OUTSIDE-WORKTREE"),
                "an in-worktree symlink to an outside file must not be read into the judge input: {via_symlink}"
            );
            std::fs::remove_file(&link).ok();
        }
        std::fs::remove_file(&secret).ok();

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn status_string_roundtrips() {
        for s in [
            StepStatus::Ok,
            StepStatus::Failed,
            StepStatus::Cancelled,
            StepStatus::ElicitationFailed,
            StepStatus::TimedOut,
        ] {
            assert_eq!(status_from_str(status_to_str(s)), s);
        }
        // TimedOut and Cancelled must never collapse on the wire — the whole point of the
        // distinguishing status is that an operator cancel and the engine's turn ceiling stay
        // separable (616c8661).
        assert_ne!(
            status_to_str(StepStatus::TimedOut),
            status_to_str(StepStatus::Cancelled)
        );
        // Unknown token fails safe to Ok (the actor's failed/cancelled arms are the deny paths; an
        // unknown status must not spuriously fail a run — Ok goes through the normal gate).
        assert_eq!(status_from_str("garbage"), StepStatus::Ok);
    }

    /// Seam finding #7: the APPROVED deterministic validator's shell SCRIPT must NOT be serialized onto
    /// the bus. `try_publish_dispatched` publishes a `task.dispatched` whose unit carries the validator
    /// CRITERION (the cli-runner's agent judge needs it) but a BLANK script — the deterministic script is
    /// re-verified at the gate from the actor's own store. The content-address `validator_pin` rides along
    /// for provenance, computed over the ORIGINAL script so it still addresses the real approved validator.
    #[test]
    fn validator_script_is_never_serialized_onto_the_bus() {
        let dir =
            std::env::temp_dir().join(format!("wicked-core-clirunner-v7-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bus_path = dir.join("bus.db").to_str().unwrap().to_string();

        let mut unit = crate::domain::WorkUnit::pending("r:u1", "r", 1, "do it");
        let validator = crate::validator::DeterministicValidator {
            criterion: "the file exists".into(),
            script: "test -f /super/secret/path && rm -rf /".into(),
            approved: true,
        };
        let expected_pin = crate::validator_vault::pin(&validator);
        unit.validator = Some(validator);
        let input = StepInput {
            run_id: "r".into(),
            unit_ix: 0,
            attempt: 0,
            unit,
            workflow_id: "wf-r".into(),
            entity_mode: EntityMode::Shared,
            workdir: None,
            governance: None,
            prior_outputs: vec![],
            elicitation_epoch: 0,
            process_gen: None,
            launch_seq: 0,
            required_skills: Vec::new(),
        };

        // Arm the publisher on THIS thread, publish, then disarm (thread-local is per-thread).
        arm_exec_publisher(BusDb::shared(&bus_path).expect("arm publisher"));
        assert!(
            try_publish_dispatched(&input, None, false),
            "publish task.dispatched"
        );
        disarm_exec_publisher();

        let bus = BusDb::open(&bus_path).unwrap();
        let evs = bus.poll(TASK_DISPATCHED, 0, 10).unwrap();
        assert_eq!(evs.len(), 1, "one task.dispatched published");
        // The RAW serialized payload must not contain the script anywhere.
        let raw = serde_json::to_string(&evs[0].payload).unwrap();
        assert!(
            !raw.contains("rm -rf") && !raw.contains("/super/secret/path"),
            "the validator SCRIPT must never appear in the serialized task.dispatched payload: {raw}"
        );
        let task: DispatchedTask = serde_json::from_value(evs[0].payload.clone()).unwrap();
        let v = task
            .unit
            .validator
            .expect("the criterion + approval still ride (only the script is stripped)");
        assert_eq!(v.criterion, "the file exists", "criterion is preserved");
        assert!(v.approved, "approval flag is preserved");
        assert_eq!(v.script, "", "the script is blanked");
        assert_eq!(
            task.validator_pin.as_deref(),
            Some(expected_pin.as_str()),
            "the content-address pin (over the ORIGINAL script) rides along for provenance"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Council [4]: a governed StepInput's `GovernanceContext` must SURVIVE the exec-mediation bus
    /// round-trip — a `#[serde(default)]` regression in `DispatchedTask` or the reconstruction would
    /// silently rebuild an UNGOVERNED input, disabling input governance for the WHOLE bus delivery mode
    /// (the exact silent-off failure class the milestone exists to prevent) with nothing to catch it.
    #[test]
    fn governance_context_survives_the_exec_mediation_round_trip() {
        let dir = std::env::temp_dir().join(format!("wc-clirunner-gov-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bus_path = dir.join("bus.db").to_str().unwrap().to_string();

        let unit = crate::domain::WorkUnit::pending("r:u1", "r", 1, "do it");
        let input = StepInput {
            run_id: "r".into(),
            unit_ix: 0,
            attempt: 0,
            unit,
            workflow_id: "wf-r".into(),
            entity_mode: EntityMode::Shared,
            workdir: None,
            governance: Some(crate::workflow::GovernanceContext {
                human_confirm: Default::default(),
                db_path: "/abs/estate.db".into(),
                // Through the resolver's spelling helper, not a hand-join (FINDING-069 discipline).
                code_graph_db: Some(
                    std::path::Path::new("/abs/repo")
                        .join(crate::code_graph::code_graph_rel())
                        .to_string_lossy()
                        .into_owned(),
                ),
                extra_write_roots: Vec::new(),
                extra_read_roots: Vec::new(),
                project_id: None,
                repo_id: None,
            }),
            prior_outputs: vec![],
            elicitation_epoch: 0,
            process_gen: None,
            launch_seq: 0,
            required_skills: Vec::new(),
        };

        arm_exec_publisher(BusDb::shared(&bus_path).expect("arm publisher"));
        assert!(
            try_publish_dispatched(&input, None, false),
            "publish task.dispatched"
        );
        disarm_exec_publisher();

        let bus = BusDb::open(&bus_path).unwrap();
        let evs = bus.poll(TASK_DISPATCHED, 0, 10).unwrap();
        assert_eq!(evs.len(), 1);
        let task: DispatchedTask = serde_json::from_value(evs[0].payload.clone()).unwrap();
        let gov = task
            .governance
            .expect("the governance context survives the bus (NOT dropped to None)");
        assert_eq!(
            gov.db_path, "/abs/estate.db",
            "the store path the off-actor launcher needs to arm the hook is preserved"
        );
        // FINDING-067: the repo-local graph must survive too. Only the actor thread can resolve it
        // (it holds the store); if the bus drops it, the off-actor launcher sees `None` and ships the
        // worker no estate tools at all — governance still on, recon silently gone.
        assert_eq!(
            gov.code_graph_db.as_deref(),
            std::path::Path::new("/abs/repo")
                .join(crate::code_graph::code_graph_rel())
                .to_str(),
            "the repo-local code graph survives the bus"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn seat(key: &str, invocation: &str) -> crate::AgenticCli {
        use wicked_council::{Category, Confidence, InputMode};
        crate::AgenticCli {
            key: key.into(),
            display_name: key.into(),
            binary: "unused".into(),
            headless_invocation: invocation.into(),
            category: Category::default(),
            input_mode: InputMode::default(),
            version_probe: vec![],
            trust_flags: vec![],
            alt_binaries: vec![],
            confidence: Confidence::default(),
            enabled_for_council: true,
            seat_eligible_for_work: true,
            acp: None,
            capabilities: None,
            login_invocation: None,
            logout_invocation: None,
            governance_class: None,
            credential: None,
            free_tier: None,
            health: None,
        }
    }

    /// Copilot on #433 (core#431): a guarded seat that rewrites the tree and then exits FAILED
    /// never reaches the gate — but a human's Approve on its failure re-dispatches it, and the
    /// baseline is cleared on approve. The restore must therefore run for every status, not only
    /// `Ok`: after this failed run the creator's file is back and the evaluator's is gone, while
    /// the evidence handed to the fold is what it always was for a failed unit (no guard outcome).
    #[test]
    fn a_failed_evaluator_that_rewrote_the_tree_still_has_the_creators_tree_restored() {
        use crate::workflow::{StepOutput, StepRunner};
        use std::process::Command;

        struct FailingRewriter;
        impl StepRunner for FailingRewriter {
            fn run_unit(&self, input: &StepInput) -> StepOutput {
                let wd = input.workdir.as_ref().expect("bound");
                std::fs::write(wd.join("src/a.ts"), "evaluator's rewrite\n").unwrap();
                std::fs::write(wd.join("src/evaluator.ts"), "left behind\n").unwrap();
                StepOutput {
                    run_id: input.run_id.clone(),
                    unit_ix: input.unit_ix,
                    attempt: input.attempt,
                    output: "crashed after editing".into(),
                    status: StepStatus::Failed,
                    usage: None,
                    files: Vec::new(),
                    tools: Vec::new(),
                    governed: false,
                }
            }
        }
        fn sh(cwd: &std::path::Path, args: &[&str]) {
            // spawn-audit: test-only — a git fixture building the layout under test; reads no engine state.
            let out = Command::new("git")
                .args(args)
                .current_dir(cwd)
                .output()
                .expect("git runs");
            assert!(out.status.success(), "git {args:?}: {out:?}");
        }
        let base =
            std::env::temp_dir().join(format!("wicked-core-failed-restore-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("repo");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        sh(&repo, &["init", "-q", "."]);
        sh(&repo, &["config", "user.email", "t@example.invalid"]);
        sh(&repo, &["config", "user.name", "t"]);
        sh(&repo, &["config", "commit.gpgsign", "false"]);
        sh(&repo, &["config", "core.autocrlf", "false"]); // byte-exact LF asserts on Windows
        std::fs::write(repo.join("src/a.ts"), "base\n").unwrap();
        sh(&repo, &["add", "-A"]);
        sh(&repo, &["commit", "-qm", "base"]);
        let wt = base.join("wt");
        sh(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                wt.to_str().unwrap(),
                "-b",
                "wicked/failed-restore",
            ],
        );
        // The creator's uncommitted fix.
        std::fs::write(wt.join("src/a.ts"), "fixed\n").unwrap();

        let mut unit = crate::domain::WorkUnit::pending("r:verify", "r", 4, "verify");
        unit.worktree_guarded = true;
        unit.worktree_baseline = Some(crate::worktree_guard::snapshot(&wt, &repo).unwrap());
        let input = StepInput {
            run_id: "r".into(),
            unit_ix: 3,
            attempt: 0,
            unit,
            workflow_id: "wf-r".into(),
            entity_mode: EntityMode::Isolated,
            workdir: Some(wt.clone()),
            governance: None,
            prior_outputs: vec![],
            elicitation_epoch: 0,
            process_gen: None,
            launch_seq: 0,
            required_skills: Vec::new(),
        };
        let noop: &DeltaSink = &|_: &str| {};
        let runner: Arc<dyn StepRunner> = Arc::new(FailingRewriter);
        let (output, verdict, evidence) =
            run_unit_and_judge_with_roster(&runner, &input, None, noop, &[], &[], &[]);
        assert_eq!(output.status, StepStatus::Failed);
        assert!(verdict.is_none(), "no judge runs for a failed unit");
        assert!(
            evidence.worktree_guard.is_none(),
            "a failed unit hands the fold no guard outcome, as before"
        );
        assert_eq!(
            std::fs::read_to_string(wt.join("src/a.ts")).unwrap(),
            "fixed\n",
            "the creator's fix is back even though the evaluator crashed"
        );
        assert!(
            !wt.join("src/evaluator.ts").exists(),
            "the evaluator's leftover file is gone"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// (core#1024 follow-up) SEAT-SECRET GUARD at the unit's settled diff: a creator that writes the
    /// token its seat was handed into the worktree FAILS, with a reason naming the file and the
    /// credential's kind and never the value; the same work with nothing injected passes untouched.
    #[test]
    fn a_unit_that_writes_its_seats_token_into_the_work_fails_without_the_value() {
        use crate::workflow::{StepOutput, StepRunner};
        // Built at runtime: no bearer-looking literal in the source (core#1024's GitGuardian fix).
        let token: String = format!(
            "{}-{}-{}-{}",
            "sk",
            "ant",
            "oat01",
            (0..96)
                .map(|i| (b'a' + (i * 5 % 26) as u8) as char)
                .collect::<String>()
        );
        struct Leaker(String);
        impl StepRunner for Leaker {
            fn run_unit(&self, input: &StepInput) -> StepOutput {
                let wd = input.workdir.as_deref().unwrap();
                std::fs::write(wd.join("debug.env"), format!("TOKEN={}\n", self.0)).unwrap();
                StepOutput {
                    run_id: input.run_id.clone(),
                    unit_ix: input.unit_ix,
                    attempt: input.attempt,
                    output: "done".into(),
                    status: StepStatus::Ok,
                    usage: None,
                    files: Vec::new(),
                    tools: Vec::new(),
                    governed: false,
                }
            }
        }
        fn sh(cwd: &std::path::Path, args: &[&str]) {
            // spawn-audit: test-only — a git fixture building the layout under test; reads no engine state.
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .output()
                .expect("git runs");
            assert!(out.status.success(), "git {args:?}: {out:?}");
        }
        let run = |tag: &str| {
            let base = std::env::temp_dir().join(format!(
                "wicked-core-seat-secret-{tag}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&base);
            let repo = base.join("repo");
            std::fs::create_dir_all(&repo).unwrap();
            sh(&repo, &["init", "-q", "."]);
            sh(&repo, &["config", "user.email", "t@example.invalid"]);
            sh(&repo, &["config", "user.name", "t"]);
            sh(&repo, &["config", "commit.gpgsign", "false"]);
            std::fs::write(repo.join("README.md"), "base\n").unwrap();
            sh(&repo, &["add", "-A"]);
            sh(&repo, &["commit", "-qm", "base"]);
            let wt = base.join("wt");
            let branch = format!("wicked/seat-secret-{tag}");
            sh(
                &repo,
                &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", &branch],
            );
            let mut unit = crate::domain::WorkUnit::pending("r:build", "r", 4, "build");
            unit.worktree_baseline = Some(crate::worktree_guard::snapshot(&wt, &repo).unwrap());
            let input = StepInput {
                run_id: "r".into(),
                unit_ix: 3,
                attempt: 0,
                unit,
                workflow_id: "wf-r".into(),
                entity_mode: EntityMode::Isolated,
                workdir: Some(wt.clone()),
                governance: None,
                prior_outputs: vec![],
                elicitation_epoch: 0,
                process_gen: None,
                launch_seq: 0,
                required_skills: Vec::new(),
            };
            let noop: &DeltaSink = &|_: &str| {};
            let runner: Arc<dyn StepRunner> = Arc::new(Leaker(token.clone()));
            let (output, _, _) =
                run_unit_and_judge_with_roster(&runner, &input, None, noop, &[], &[], &[]);
            let _ = std::fs::remove_dir_all(&base);
            output
        };
        // Nothing was injected by this (test-thread) process: no scan, the unit passes.
        let clean = run("none");
        assert_eq!(clean.status, StepStatus::Ok, "{}", clean.output);
        assert_eq!(clean.output, "done");
        // The launcher handed the seat this token: the unit fails, naming file and kind.
        crate::seat_secret_guard::register(
            crate::seat_secret_guard::SecretKind::ClaudeSetupToken,
            &token,
        );
        let out = run("leak");
        assert_eq!(out.status, StepStatus::Failed);
        assert!(
            out.output.starts_with("SEAT-CREDENTIAL — unit 4 "),
            "{}",
            out.output
        );
        assert!(out.output.contains("`debug.env`"), "{}", out.output);
        assert!(out.output.contains("claude setup-token"), "{}", out.output);
        assert!(
            !out.output.contains(&token),
            "the value must never be shown"
        );
        assert!(!out.output.contains(&token[14..60]));
    }

    /// C1 (self-grading in the real path): the agent judge must NOT be dispatched under the seat that
    /// WROTE the work. `run_unit_and_judge` computes the work author from the unit's `assigned_cli` and
    /// excludes BOTH it and the deterministic author when selecting the judge seat. Proven via a recording
    /// stub that captures the assigned_cli of every dispatched unit — the LAST is the judge.
    /// EP-K3 (DES-artifact-editor-plugins §7.6): a launch-time `exclude_seats` is UNIONED into
    /// the judge exclusion. A listed seat (by cli key, or by an instance of it) is never the judge;
    /// the list can only narrow the choice — the work author stays excluded whatever it says — and
    /// an empty list behaves as today.
    #[test]
    fn a_launch_excluded_seat_is_never_the_judge_and_cannot_unexclude_the_author() {
        use crate::workflow::{StepOutput, StepRunner};
        use std::sync::Mutex;

        #[derive(Default)]
        struct Rec {
            seen: Mutex<Vec<Option<String>>>,
        }
        impl StepRunner for Rec {
            fn run_unit(&self, input: &StepInput) -> StepOutput {
                self.seen
                    .lock()
                    .unwrap()
                    .push(input.unit.assigned_cli.clone());
                StepOutput {
                    run_id: input.run_id.clone(),
                    unit_ix: input.unit_ix,
                    attempt: input.attempt,
                    output: "PASS\nrecorded\nPASS".into(),
                    status: StepStatus::Ok,
                    usage: None,
                    files: Vec::new(),
                    tools: Vec::new(),
                    governed: false,
                }
            }
        }
        let dir = std::env::temp_dir().join(format!("wicked-core-k3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let judge_with = |exclude: &[&str]| -> (Option<String>, bool) {
            let mut unit = crate::domain::WorkUnit::pending("r:u1", "r", 1, "do the work");
            unit.assigned_cli = Some("agy".into());
            unit.validator = Some(crate::validator::DeterministicValidator {
                criterion: "the work is correct".into(),
                script: "test -f x".into(),
                approved: true,
            });
            unit.exclude_seats = exclude.iter().map(|s| s.to_string()).collect();
            let input = StepInput {
                run_id: "r".into(),
                unit_ix: 0,
                attempt: 0,
                unit,
                workflow_id: "wf-r".into(),
                entity_mode: EntityMode::Isolated,
                workdir: Some(dir.clone()),
                governance: None,
                prior_outputs: vec![],
                elicitation_epoch: 0,
                process_gen: None,
                launch_seq: 0,
                required_skills: Vec::new(),
            };
            let rec = Arc::new(Rec::default());
            let runner: Arc<dyn StepRunner> = rec.clone();
            let roster = vec![
                seat("claude", "claude -p {PROMPT}"),
                seat("agy", "agy run {PROMPT}"),
                seat("pi", "pi ask {PROMPT}"),
                seat("codex", "codex exec {PROMPT}"),
            ];
            let noop: &DeltaSink = &|_: &str| {};
            let (_out, verdict, _ev) =
                run_unit_and_judge_with_roster(&runner, &input, None, noop, &roster, &[], &[]);
            let seen = rec.seen.lock().unwrap();
            // The first run is the work unit itself; a judge, when one ran, is the second.
            (seen.get(1).cloned().flatten(), verdict.is_some())
        };
        // Today: the first seat distinct from the det author (claude) and the author (agy).
        assert_eq!(judge_with(&[]).0.as_deref(), Some("pi"));
        // A launch-excluded seat is skipped, by cli key or by an instance of it.
        assert_eq!(judge_with(&["pi"]).0.as_deref(), Some("codex"));
        assert_eq!(judge_with(&["pi#2"]).0.as_deref(), Some("codex"));
        // It only narrows: excluding every other seat leaves NO judge — never the author.
        let (judge, ran) = judge_with(&["pi", "codex"]);
        assert_eq!(
            judge, None,
            "no distinct judge remains, so none is convened"
        );
        assert!(!ran, "the judge is skipped, not run as a self-grade");
        // Naming the author or a nonsense seat changes nothing about the author's exclusion.
        assert_eq!(judge_with(&["agy", "nope"]).0.as_deref(), Some("pi"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// core#799: the agent judge of a pinned validator is told the deterministic floor decides
    /// the criterion's deterministic half (the floor runs after it, so its result is never in the
    /// WORK) — run e2e4039b's judge rejected a lint-exit-0 plan for "not showing the lint result".
    #[test]
    fn the_pinned_judge_is_handed_the_deterministic_floor_contract() {
        use crate::workflow::{StepOutput, StepRunner};
        use std::sync::Mutex;

        #[derive(Default)]
        struct Rec {
            prompts: Mutex<Vec<String>>,
        }
        impl StepRunner for Rec {
            fn run_unit(&self, input: &StepInput) -> StepOutput {
                self.prompts
                    .lock()
                    .unwrap()
                    .push(input.unit.description.clone());
                StepOutput {
                    run_id: input.run_id.clone(),
                    unit_ix: input.unit_ix,
                    attempt: input.attempt,
                    output: "PASS\nrecorded\nPASS".into(),
                    status: StepStatus::Ok,
                    usage: None,
                    files: Vec::new(),
                    tools: Vec::new(),
                    governed: false,
                }
            }
        }
        let dir = std::env::temp_dir().join(format!("wicked-core-799-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut unit = crate::domain::WorkUnit::pending("r:wp", "r", 1, "author the storyline");
        unit.assigned_cli = Some("agy".into());
        unit.validator = Some(crate::validator::DeterministicValidator {
            criterion: crate::builtin_floors::WALKTHROUGH_LINT_CRITERION.into(),
            script: "wicked-garden run scripts/demo/walkthrough.mjs lint".into(),
            approved: true,
        });
        let input = StepInput {
            run_id: "r".into(),
            unit_ix: 0,
            attempt: 0,
            unit,
            workflow_id: "wf-r".into(),
            entity_mode: EntityMode::Isolated,
            workdir: Some(dir.clone()),
            governance: None,
            prior_outputs: vec![],
            elicitation_epoch: 0,
            process_gen: None,
            launch_seq: 0,
            required_skills: Vec::new(),
        };
        let rec = Arc::new(Rec::default());
        let runner: Arc<dyn StepRunner> = rec.clone();
        let roster = vec![
            seat("claude", "claude -p {PROMPT}"),
            seat("agy", "agy run {PROMPT}"),
            seat("codex", "codex exec {PROMPT}"),
        ];
        let noop: &DeltaSink = &|_: &str| {};
        let (_out, verdict, _ev) =
            run_unit_and_judge_with_roster(&runner, &input, None, noop, &roster, &[], &[]);
        assert!(verdict.is_some(), "the pinned judge ran");
        let prompts = rec.prompts.lock().unwrap();
        let judge = prompts.get(1).expect("the judge's prompt");
        assert!(
            judge.contains(crate::builtin_floors::WALKTHROUGH_LINT_CRITERION),
            "{judge}"
        );
        assert!(judge.contains("[deterministic floor]"), "{judge}");
        assert!(
            judge.contains("do NOT reject because the WORK does not show that check's result"),
            "{judge}"
        );
        assert!(judge.contains("walkthrough.mjs lint"), "{judge}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// core#1155: a seat's turn on a JS worktree starts with the worktree's dependencies installed
    /// by the engine, and the seat is told so; the pre-build scope unit installs nothing.
    #[test]
    fn a_seat_on_a_crew_shaped_worktree_finds_its_dependencies_installed() {
        use crate::workflow::{StepOutput, StepRunner};
        use std::sync::Mutex;
        if !crate::seat_deps::tests::npm_available() || !crate::seat_deps::tests::sandbox_arms() {
            eprintln!("skipped: npm or the floor sandbox is not available on this host");
            return;
        }
        #[derive(Default)]
        struct DepsProbe {
            seen: Mutex<Vec<(bool, bool)>>,
        }
        impl StepRunner for DepsProbe {
            fn run_unit(&self, input: &StepInput) -> StepOutput {
                let wd = input.workdir.clone().unwrap();
                let installed = wd.join("node_modules/fake-vitest/package.json").is_file();
                let told = input.prior_outputs.iter().any(|p| {
                    p.label == crate::seat_deps::CONTEXT_LABEL && p.output.contains("npm ci")
                });
                self.seen.lock().unwrap().push((installed, told));
                StepOutput {
                    run_id: input.run_id.clone(),
                    unit_ix: input.unit_ix,
                    attempt: input.attempt,
                    output: "done".into(),
                    status: StepStatus::Ok,
                    usage: None,
                    files: Vec::new(),
                    tools: Vec::new(),
                    governed: false,
                }
            }
        }
        let (_tmp, wt) = crate::seat_deps::tests::crew_shaped("cli-seat-deps");
        let input_for = |ord: u32, scope: bool| {
            let mut unit = crate::domain::WorkUnit::pending(format!("r:u{ord}"), "r", ord, "work");
            unit.assigned_cli = Some("claude".into());
            unit.pre_build_scope = scope;
            // The baseline the actor takes at dispatch: it carries the pinned git dir.
            unit.worktree_baseline = Some(
                crate::worktree_guard::snapshot_through(
                    &wt,
                    &crate::seat_deps::tests::git_dir_of(&wt),
                )
                .unwrap(),
            );
            StepInput {
                run_id: "r-1155".into(),
                unit_ix: 0,
                attempt: 0,
                unit,
                workflow_id: "wf-r".into(),
                entity_mode: EntityMode::Isolated,
                workdir: Some(wt.clone()),
                governance: None,
                prior_outputs: vec![],
                elicitation_epoch: 0,
                process_gen: None,
                launch_seq: 0,
                required_skills: Vec::new(),
            }
        };
        let notes: &'static Mutex<Vec<String>> = Box::leak(Box::new(Mutex::new(Vec::new())));
        let sink = move |s: &str| notes.lock().unwrap().push(s.to_string());
        let sink: &DeltaSink = &sink;
        let probe = Arc::new(DepsProbe::default());
        let runner: Arc<dyn StepRunner> = probe.clone();
        let roster = vec![seat("claude", "claude -p {PROMPT}")];
        // The scope unit: no install, no word.
        let _ = run_unit_and_judge_with_roster(
            &runner,
            &input_for(1, true),
            None,
            sink,
            &roster,
            &[],
            &[],
        );
        // The fix unit: installed before the seat, the seat told.
        let _ = run_unit_and_judge_with_roster(
            &runner,
            &input_for(2, false),
            None,
            sink,
            &roster,
            &[],
            &[],
        );
        // The next unit of the run: still in place, nothing installed again.
        let _ = run_unit_and_judge_with_roster(
            &runner,
            &input_for(3, false),
            None,
            sink,
            &roster,
            &[],
            &[],
        );
        assert_eq!(
            *probe.seen.lock().unwrap(),
            vec![(false, false), (true, true), (true, true)]
        );
        let installs = notes
            .lock()
            .unwrap()
            .iter()
            .filter(|n| n.contains("the engine installed the worktree's node_modules"))
            .count();
        assert_eq!(installs, 1, "one install for the run");
    }

    #[test]
    fn agent_judge_excludes_the_work_author_seat_c1() {
        use crate::workflow::{StepOutput, StepRunner};
        use std::sync::Mutex;

        #[derive(Default)]
        struct RecordingRunner {
            seen: Mutex<Vec<Option<String>>>,
        }
        impl StepRunner for RecordingRunner {
            fn run_unit(&self, input: &StepInput) -> StepOutput {
                self.seen
                    .lock()
                    .unwrap()
                    .push(input.unit.assigned_cli.clone());
                StepOutput {
                    run_id: input.run_id.clone(),
                    unit_ix: input.unit_ix,
                    attempt: input.attempt,
                    output: "PASS\nrecorded\nPASS".into(),
                    status: StepStatus::Ok,
                    usage: None,
                    files: Vec::new(),
                    tools: Vec::new(),
                    governed: false,
                }
            }
        }

        // A work unit AUTHORED BY `agy` (assigned_cli = "agy"), carrying an APPROVED validator so the
        // agent judge runs. workdir must be Some for the layer-2 judge to fire.
        let dir = std::env::temp_dir().join(format!("wicked-core-c1-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut unit = crate::domain::WorkUnit::pending("r:u1", "r", 1, "do the work");
        unit.assigned_cli = Some("agy".into());
        unit.validator = Some(crate::validator::DeterministicValidator {
            criterion: "the work is correct".into(),
            script: "test -f x".into(),
            approved: true,
        });
        let input = StepInput {
            run_id: "r".into(),
            unit_ix: 0,
            attempt: 0,
            unit,
            workflow_id: "wf-r".into(),
            entity_mode: EntityMode::Isolated,
            workdir: Some(dir.clone()),
            governance: None,
            prior_outputs: vec![],
            elicitation_epoch: 0,
            process_gen: None,
            launch_seq: 0,
            required_skills: Vec::new(),
        };
        let noop: &DeltaSink = &|_: &str| {};

        // 3-seat roster [claude, agy, pi]: excluding BOTH the det author (claude) and the work author
        // (agy) leaves `pi` as the ONLY distinct judge — proving exclude-both DISPATCHES a distinct seat.
        let rec = Arc::new(RecordingRunner::default());
        let runner: Arc<dyn StepRunner> = rec.clone();
        let roster3 = vec![
            seat("claude", "claude -p {PROMPT}"),
            seat("agy", "agy run {PROMPT}"),
            seat("pi", "pi ask {PROMPT}"),
        ];
        let (_out, verdict, _evidence) =
            run_unit_and_judge_with_roster(&runner, &input, None, noop, &roster3, &[], &[]);
        assert!(
            verdict.is_some(),
            "an approved validator + workdir ⇒ a layer-2 verdict runs"
        );
        let seen = rec.seen.lock().unwrap();
        let judge_seat = seen.last().cloned().flatten();
        assert_eq!(
            judge_seat.as_deref(),
            Some("pi"),
            "the judge must be the distinct seat, NOT the work author (agy) NOR the det author (claude)"
        );
        assert_ne!(
            judge_seat.as_deref(),
            Some("agy"),
            "never self-grade under the work author"
        );
        drop(seen);

        // 2-seat roster [claude, agy]: both identities excluded ⇒ (#539) judge is SKIPPED, not
        // dispatched as a self-grade via the single-runner fallback. The runner is called once for
        // the work unit only; evidence.judge_skipped carries the eligibility reason.
        let rec2 = Arc::new(RecordingRunner::default());
        let runner2: Arc<dyn StepRunner> = rec2.clone();
        let roster2 = vec![
            seat("claude", "claude -p {PROMPT}"),
            seat("agy", "agy run {PROMPT}"),
        ];
        let (_out2, verdict2, evidence2) =
            run_unit_and_judge_with_roster(&runner2, &input, None, noop, &roster2, &[], &[]);
        let seen2 = rec2.seen.lock().unwrap();
        assert_eq!(
            seen2.len(),
            1,
            "only the work unit must be dispatched — no fallback judge call (#539)"
        );
        assert_eq!(
            seen2.last().cloned().flatten().as_deref(),
            Some("agy"),
            "the single dispatch is the work unit (assigned_cli = agy)"
        );
        drop(seen2);
        assert!(
            verdict2.is_none(),
            "no distinct seat ⇒ no agent verdict (#539 — judge skipped, not self-graded)"
        );
        assert!(
            evidence2.judge_skipped.is_some(),
            "evidence.judge_skipped must name the reason when the judge is skipped (#539)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (rejudge) An armed re-judge runs the JUDGE ONLY: the unit's seat never runs (no call carries
    /// the work unit), the reviewed output is what the attempt folds (`governed: false` — no seat
    /// turn to govern), and the judge is drawn by the ordinary selection — a seat distinct from
    /// the work's author AND the pinned validator's author. Its REJECT comes back as a REJECT: the
    /// fold denies on it like on any judge (deny-dominates).
    #[test]
    fn a_rejudge_runs_only_the_judge_on_the_reviewed_output() {
        use crate::workflow::{StepOutput, StepRunner};
        use std::sync::Mutex;
        struct Judge {
            answer: &'static str,
            seen: Mutex<Vec<(Option<String>, String)>>,
        }
        impl StepRunner for Judge {
            fn run_unit(&self, input: &StepInput) -> StepOutput {
                self.seen.lock().unwrap().push((
                    input.unit.assigned_cli.clone(),
                    input.unit.description.clone(),
                ));
                StepOutput {
                    run_id: input.run_id.clone(),
                    unit_ix: input.unit_ix,
                    attempt: input.attempt,
                    output: self.answer.into(),
                    status: StepStatus::Ok,
                    usage: None,
                    files: Vec::new(),
                    tools: Vec::new(),
                    governed: true,
                }
            }
        }
        let dir = std::env::temp_dir().join(format!("wicked-core-rejudge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut unit = crate::domain::WorkUnit::pending("r:u4", "r", 4, "fix the retry");
        unit.assigned_cli = Some("agy".into());
        unit.validator = Some(crate::validator::DeterministicValidator {
            criterion: "the work is correct".into(),
            script: "true".into(),
            approved: true,
        });
        unit.requested_rejudge = Some(crate::domain::RequestedRejudge {
            output: "Fixed the retry: callers pass an empty head.".into(),
            attempt: 2,
            seats: vec!["pi".into()],
            monitors: Vec::new(),
            consumed: false,
            at: 0,
        });
        let input = StepInput {
            run_id: "r".into(),
            unit_ix: 3,
            attempt: 3,
            unit,
            workflow_id: "wf-r".into(),
            entity_mode: EntityMode::Isolated,
            workdir: Some(dir.clone()),
            governance: None,
            prior_outputs: vec![],
            elicitation_epoch: 0,
            process_gen: None,
            launch_seq: 0,
            required_skills: Vec::new(),
        };
        let roster = vec![
            seat("claude", "claude -p {PROMPT}"),
            seat("agy", "agy run {PROMPT}"),
            seat("pi", "pi ask {PROMPT}"),
        ];
        let noop: &DeltaSink = &|_: &str| {};
        for (answer, pass) in [
            ("PASS\nthe fix is right\nPASS", true),
            ("REJECT: the error path is unhandled", false),
        ] {
            let judge = Arc::new(Judge {
                answer,
                seen: Mutex::new(Vec::new()),
            });
            let runner: Arc<dyn StepRunner> = judge.clone();
            let (out, verdict, _evidence) =
                run_unit_and_judge_with_roster(&runner, &input, None, noop, &roster, &[], &[]);
            let seen = judge.seen.lock().unwrap().clone();
            assert_eq!(
                seen.len(),
                1,
                "one call — the judge's, never the seat's: {seen:?}"
            );
            assert_eq!(
                seen[0].0.as_deref(),
                Some("pi"),
                "distinct from agy and claude"
            );
            assert_ne!(seen[0].1, "fix the retry", "the work unit did not run");
            assert_eq!(out.status, StepStatus::Ok);
            assert_eq!(out.output, "Fixed the retry: callers pass an empty head.");
            assert!(!out.governed, "no seat turn ran this attempt");
            let v = verdict.expect("the judge ruled");
            assert_eq!(v.pass, pass, "{v:?}");
            assert_eq!(v.judge_cli.as_deref(), Some("pi"));
        }
        // The reviewed attempt's team monitors never judge it, even when this attempt's own team
        // snapshot no longer names them: with pi a monitor, copilot judges.
        let mut monitored = input.clone();
        monitored.unit.requested_rejudge.as_mut().unwrap().monitors = vec!["pi".into()];
        let judge = Arc::new(Judge {
            answer: "PASS\nok\nPASS",
            seen: Mutex::new(Vec::new()),
        });
        let runner: Arc<dyn StepRunner> = judge.clone();
        let mut roster4 = roster.clone();
        roster4.push(seat("copilot", "copilot -p {PROMPT}"));
        let (_, verdict, _) =
            run_unit_and_judge_with_roster(&runner, &monitored, None, noop, &roster4, &[], &[]);
        assert_eq!(judge.seen.lock().unwrap().clone().len(), 1);
        assert_eq!(
            verdict.expect("ruled").judge_cli.as_deref(),
            Some("copilot")
        );
        // (crew#972, rig run 115fabdb) …unless no other judge is left: with copilot gone (claude
        // authors the validator, agy built the work), the monitor pi judges, disclosed on the
        // verdict as a team-monitor fallback, never a judge_unavailable hold.
        let judge = Arc::new(Judge {
            answer: "PASS\nok\nPASS",
            seen: Mutex::new(Vec::new()),
        });
        let runner: Arc<dyn StepRunner> = judge.clone();
        let (_, verdict, evidence) =
            run_unit_and_judge_with_roster(&runner, &monitored, None, noop, &roster, &[], &[]);
        assert_eq!(judge.seen.lock().unwrap().clone().len(), 1);
        assert!(
            evidence.judge_skipped.is_none(),
            "{:?}",
            evidence.judge_skipped
        );
        let v = verdict.expect("the monitor ruled");
        assert_eq!(v.judge_cli.as_deref(), Some("pi"));
        assert!(v.pass, "{v:?}");
        let fb = v.judge_fallback.expect("a monitor judge is disclosed");
        assert_eq!(fb.seat, "pi");
        assert!(fb.preferred.is_empty(), "{fb:?}");
        assert!(
            fb.label.starts_with("judge: pi (team-monitor fallback; no eligible judge seat distinct from the team monitors that raised findings on this step [pi]"),
            "{}",
            fb.label
        );
        // (xreview on #1165) The disclosure matches the monitor by the identity it runs, as the
        // exclusion does: `watcher` monitored and invokes pi, `pi` is the only other judge left.
        let mut aliased = input.clone();
        aliased.unit.requested_rejudge.as_mut().unwrap().monitors = vec!["watcher".into()];
        let alias_roster = vec![
            seat("claude", "claude -p {PROMPT}"),
            seat("agy", "agy run {PROMPT}"),
            seat("watcher", "pi ask {PROMPT}"),
            seat("pi", "pi ask {PROMPT}"),
        ];
        let judge = Arc::new(Judge {
            answer: "PASS\nok\nPASS",
            seen: Mutex::new(Vec::new()),
        });
        let runner: Arc<dyn StepRunner> = judge.clone();
        let (_, verdict, _) = run_unit_and_judge_with_roster(
            &runner,
            &aliased,
            None,
            noop,
            &alias_roster,
            &[],
            &["watcher".to_string()],
        );
        let v = verdict.expect("ruled");
        assert_eq!(v.judge_cli.as_deref(), Some("pi"));
        let fb = v
            .judge_fallback
            .expect("pi runs the monitor's CLI: disclosed");
        assert!(fb.label.contains("team-monitor fallback"), "{}", fb.label);
        // A consumed basis is never honored as a re-judge (the seat runs), but its monitors still
        // never judge.
        let mut basis = monitored.clone();
        basis.unit.requested_rejudge.as_mut().unwrap().consumed = true;
        let judge = Arc::new(Judge {
            answer: "PASS\nok\nPASS",
            seen: Mutex::new(Vec::new()),
        });
        let runner: Arc<dyn StepRunner> = judge.clone();
        let (out, verdict, _) =
            run_unit_and_judge_with_roster(&runner, &basis, None, noop, &roster4, &[], &[]);
        let seen = judge.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2, "the seat ran, then the judge: {seen:?}");
        assert_eq!(seen[0].1, "fix the retry", "the work unit ran");
        assert_eq!(out.output, "PASS\nok\nPASS");
        assert_eq!(
            verdict.expect("ruled").judge_cli.as_deref(),
            Some("copilot")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (rejudge) The DEFAULT judge (no pinned validator) re-runs on a re-judge although the tree is
    /// the one its floor already judged — and the floor is NOT re-run for it (its last result on
    /// that tree stands, core#544). The same unit without the re-judge convenes no judge at all.
    #[test]
    fn a_rejudge_convenes_the_default_judge_on_an_unchanged_tree_without_the_floor() {
        use crate::workflow::{StepOutput, StepRunner};
        use std::sync::Mutex;
        #[derive(Default)]
        struct Judge {
            seen: Mutex<Vec<Option<String>>>,
        }
        impl StepRunner for Judge {
            fn run_unit(&self, input: &StepInput) -> StepOutput {
                self.seen
                    .lock()
                    .unwrap()
                    .push(input.unit.assigned_cli.clone());
                StepOutput {
                    run_id: input.run_id.clone(),
                    unit_ix: input.unit_ix,
                    attempt: input.attempt,
                    output: "PASS\nthe change is right\nPASS".into(),
                    status: StepStatus::Ok,
                    usage: None,
                    files: Vec::new(),
                    tools: Vec::new(),
                    governed: false,
                }
            }
        }
        let git = |dir: &std::path::Path, args: &[&str]| {
            // spawn-audit: test-only — a git fixture building the layout under test; reads no engine state.
            let st = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .status()
                .unwrap();
            assert!(st.success(), "git {args:?}");
        };
        let repo = std::env::temp_dir().join(format!(
            "wicked-core-rejudge-default-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "."]);
        git(&repo, &["config", "user.email", "t@example.invalid"]);
        git(&repo, &["config", "user.name", "t"]);
        git(&repo, &["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.join("a.txt"), "base\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "base"]);
        std::fs::write(repo.join("a.txt"), "the fix\n").unwrap();
        let baseline = crate::worktree_guard::snapshot(&repo, &repo).unwrap();
        let mut unit = crate::domain::WorkUnit::pending("r:u4", "r", 4, "fix the retry");
        unit.role = crate::workflow::PhaseRole::Creator;
        unit.assigned_cli = Some("agy".into());
        unit.default_floor = true;
        unit.worktree_baseline = Some(baseline.clone());
        unit.repo_checks = Some(
            serde_json::from_value(serde_json::json!({
                "detected": [], "checks": [], "skipped": [], "passed": true,
                "sandbox_level": "none", "tree": baseline.tree,
            }))
            .unwrap(),
        );
        let mut input = StepInput {
            run_id: "r".into(),
            unit_ix: 3,
            attempt: 3,
            unit,
            workflow_id: "wf-r".into(),
            entity_mode: EntityMode::Isolated,
            workdir: Some(repo.clone()),
            governance: None,
            prior_outputs: vec![],
            elicitation_epoch: 0,
            process_gen: None,
            launch_seq: 0,
            required_skills: Vec::new(),
        };
        let roster = vec![
            seat("agy", "agy run {PROMPT}"),
            seat("pi", "pi ask {PROMPT}"),
        ];
        let noop: &DeltaSink = &|_: &str| {};

        input.unit.requested_rejudge = Some(crate::domain::RequestedRejudge {
            output: "I changed a.txt to the fix.".into(),
            attempt: 2,
            seats: vec!["pi".into()],
            monitors: Vec::new(),
            consumed: false,
            at: 0,
        });
        let judge = Arc::new(Judge::default());
        let runner: Arc<dyn StepRunner> = judge.clone();
        let (out, verdict, evidence) =
            run_unit_and_judge_with_roster(&runner, &input, None, noop, &roster, &[], &[]);
        assert_eq!(judge.seen.lock().unwrap().clone(), [Some("pi".to_string())]);
        assert_eq!(out.output, "I changed a.txt to the fix.");
        assert!(verdict.expect("the default judge ruled").pass);
        assert_eq!(evidence.tree_changed, Some(false), "the same tree");
        assert!(evidence.repo_checks.is_none(), "the floor is not re-run");

        input.unit.requested_rejudge = None;
        let judge = Arc::new(Judge::default());
        let runner: Arc<dyn StepRunner> = judge.clone();
        let (_, verdict, _) =
            run_unit_and_judge_with_roster(&runner, &input, None, noop, &roster, &[], &[]);
        assert_eq!(
            judge.seen.lock().unwrap().clone(),
            [Some("agy".to_string())],
            "without it the seat runs, and an unchanged floored tree convenes no judge"
        );
        assert!(verdict.is_none());
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// (rejudge) The readiness the gate serves is the judge's own selection: the work's author,
    /// the pinned validator's author, a benched seat and an unusable one are each named missing
    /// with why, and the seat the rotation would ask is eligible. With every other seat out, no
    /// seat is eligible.
    #[test]
    fn rejudge_seats_name_the_eligible_judge_and_why_the_rest_cannot() {
        let mut unit = crate::domain::WorkUnit::pending("r:u4", "r", 4, "fix the retry");
        unit.assigned_cli = Some("agy".into());
        unit.validator = Some(crate::validator::DeterministicValidator {
            criterion: "c".into(),
            script: "true".into(),
            approved: true,
        });
        let mut copilot = seat("copilot", "copilot -p {PROMPT}");
        copilot.health = Some(wicked_council::types::SeatHealth {
            usable: false,
            reason: Some("signed out".into()),
        });
        let roster = vec![
            seat("claude", "claude -p {PROMPT}"),
            seat("agy", "agy run {PROMPT}"),
            seat("pi", "pi ask {PROMPT}"),
            seat("opencode", "opencode run {PROMPT}"),
            copilot,
        ];
        let run: Vec<String> = ["claude", "agy", "pi", "opencode", "copilot"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        // (PRODUCT-WORKS J4) A quota bench that is not due yet keeps the seat out.
        let mut bench = vec![crate::domain::BenchedSeat {
            cli: "opencode".into(),
            reason: "quota_exhausted (no success in the run)".into(),
            source: "judge".into(),
            governed_only: false,
            benched_until: Some(crate::seat_bench::now_ms() + 60 * 60 * 1000),
            strikes: 1,
            replaced: None,
        }];
        let (eligible, missing) =
            rejudge_seats(&unit, &roster, &run, &bench, &[], Some(&[]), &[], &roster);
        assert_eq!(eligible, ["pi"]);
        // …and once it is due, the seat is a candidate: the re-judge's attempt re-probes it.
        bench[0].benched_until = Some(crate::seat_bench::now_ms() - 1);
        let (due, _) = rejudge_seats(&unit, &roster, &run, &bench, &[], Some(&[]), &[], &roster);
        assert_eq!(due, ["pi", "opencode"]);
        let why = |k: &str| {
            missing
                .iter()
                .find(|m| m.seat == k)
                .map(|m| m.reason.clone())
                .unwrap_or_else(|| panic!("{k} missing: {missing:?}"))
        };
        assert_eq!(why("agy"), "built this work");
        assert!(why("claude").contains("pinned validator"), "{missing:?}");
        assert!(
            why("opencode").starts_with("benched for this run: quota_exhausted"),
            "{missing:?}"
        );
        assert_eq!(why("copilot"), "not usable now: signed out");
        let (none, _) = rejudge_seats(
            &unit,
            &roster,
            &["claude".to_string(), "agy".to_string()],
            &bench,
            &[],
            Some(&[]),
            &[],
            &roster,
        );
        assert!(none.is_empty(), "{none:?}");

        // (core#954) An evaluator's creators are excluded unless that leaves no preferred seat —
        // then the rotation re-admits them, before any fallback judge.
        let mut review = crate::domain::WorkUnit::pending("r:u5", "r", 5, "review the fix");
        review.role = crate::workflow::PhaseRole::Evaluator;
        review.assigned_cli = Some("agy".into());
        let fallback = vec![seat("claude", "claude -p {PROMPT}")];
        let run2: Vec<String> = ["agy", "pi"].iter().map(|s| s.to_string()).collect();
        let (seats, missing) = rejudge_seats(
            &review,
            &roster,
            &run2,
            &[],
            &["pi".to_string()],
            Some(&[]),
            &fallback,
            &roster,
        );
        assert_eq!(
            seats.first().map(String::as_str),
            Some("pi"),
            "{seats:?} {missing:?}"
        );
        // Every preferred judge benched, and an author whose binary is unknown: no fallback judge
        // can be shown distinct from it, so none is eligible.
        let all_benched: Vec<crate::domain::BenchedSeat> = ["pi", "opencode"]
            .iter()
            .map(|k| crate::domain::BenchedSeat {
                cli: (*k).into(),
                reason: "signed out".into(),
                source: "judge".into(),
                governed_only: false,
                benched_until: None,
                strikes: 0,
                replaced: None,
            })
            .collect();
        let (unknown, _) = rejudge_seats(
            &unit,
            &roster,
            &run,
            &all_benched,
            &[],
            None,
            &fallback,
            &roster,
        );
        assert!(unknown.is_empty(), "{unknown:?}");
    }

    /// (crew#972, rig run 115fabdb review ord 7) The re-judge readiness mirrors the rotation: a
    /// team monitor that raised findings stays out while another judge is left, and is the
    /// eligible judge when none is (claude the validator author and creator, pi the reviewer,
    /// copilot benched for quota).
    #[test]
    fn a_monitor_is_the_eligible_judge_when_no_other_seat_is_left_972() {
        let roster = vec![
            seat("claude", "claude -p {PROMPT}"),
            seat("codex", "codex exec {PROMPT}"),
            seat("pi", "pi ask {PROMPT}"),
            seat("copilot", "copilot -p {PROMPT}"),
        ];
        let run: Vec<String> = ["claude", "codex", "pi", "copilot"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut review = crate::domain::WorkUnit::pending("r:review", "r", 7, "review the fix");
        review.stage = crate::domain::StageKind::Review;
        review.role = crate::workflow::PhaseRole::Evaluator;
        review.assigned_cli = Some("pi".into());
        review.validator = Some(crate::validator::DeterministicValidator {
            criterion: "c".into(),
            script: "true".into(),
            approved: true,
        });
        review.requested_rejudge = Some(crate::domain::RequestedRejudge {
            output: "VERDICT: PASS".into(),
            attempt: 1,
            seats: Vec::new(),
            monitors: vec!["codex".into()],
            consumed: false,
            at: 0,
        });
        let creators = ["claude".to_string()];
        let (seats, missing) = rejudge_seats(
            &review,
            &roster,
            &run,
            &[],
            &creators,
            Some(&["claude".to_string()]),
            &[],
            &roster,
        );
        assert_eq!(seats, ["copilot"], "the monitor stays out: {missing:?}");
        assert!(
            missing
                .iter()
                .any(|m| m.seat == "codex" && m.reason.starts_with("monitored this step")),
            "{missing:?}"
        );
        let bench = vec![crate::domain::BenchedSeat {
            cli: "copilot".into(),
            reason: "recent quota_exhausted".into(),
            source: "judge".into(),
            governed_only: false,
            benched_until: Some(crate::seat_bench::now_ms() + 60 * 60 * 1000),
            strikes: 1,
            replaced: None,
        }];
        let (seats, missing) = rejudge_seats(
            &review,
            &roster,
            &run,
            &bench,
            &creators,
            Some(&["claude".to_string()]),
            &[],
            &roster,
        );
        assert_eq!(seats, ["codex"], "the monitor is re-admitted: {missing:?}");

        // (xreview on #1165) A creator that also monitored stays out as a creator when the
        // monitors are re-admitted: codex built the work and monitored, agy monitored.
        let roster = vec![
            seat("claude", "claude -p {PROMPT}"),
            seat("codex", "codex exec {PROMPT}"),
            seat("pi", "pi ask {PROMPT}"),
            seat("agy", "agy run {PROMPT}"),
        ];
        let run: Vec<String> = ["claude", "codex", "pi", "agy"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        review.requested_rejudge.as_mut().unwrap().monitors = vec!["codex".into(), "agy".into()];
        let (seats, missing) = rejudge_seats(
            &review,
            &roster,
            &run,
            &[],
            &["codex".to_string()],
            Some(&["codex".to_string()]),
            &[],
            &roster,
        );
        assert_eq!(seats, ["agy"], "never the creator: {missing:?}");
        assert!(
            missing
                .iter()
                .any(|m| m.seat == "codex" && m.reason == "created the work this review reads"),
            "{missing:?}"
        );
    }

    /// (core#1143, rig run 85ddad0d) The judge pool states each excluded seat's reason as the run
    /// records it: at a REVIEW gate, the reviewer's own seat wrote the review (it never judges its
    /// own verdict) and did not build the work; the creator built it. A neutral test step's seat
    /// ran that step. Only a building unit's own seat "built this work".
    #[test]
    fn the_judge_pool_never_says_a_reviewer_built_the_work_1143() {
        let roster = vec![
            seat("claude", "claude -p {PROMPT}"),
            seat("codex", "codex exec {PROMPT}"),
            seat("pi", "pi ask {PROMPT}"),
        ];
        let run: Vec<String> = ["claude", "codex", "pi"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let why = |unit: &crate::domain::WorkUnit, k: &str| {
            let (_, missing) = rejudge_seats(
                unit,
                &roster,
                &run,
                &[],
                &["claude".to_string()],
                Some(&["claude".to_string()]),
                &[],
                &roster,
            );
            missing
                .iter()
                .find(|m| m.seat == k)
                .map(|m| m.reason.clone())
                .unwrap_or_else(|| panic!("{k} missing: {missing:?}"))
        };
        let mut review = crate::domain::WorkUnit::pending("r:review", "r", 5, "review the fix");
        review.stage = crate::domain::StageKind::Review;
        review.role = crate::workflow::PhaseRole::Evaluator;
        review.assigned_cli = Some("codex".into());
        assert_eq!(
            why(&review, "codex"),
            "wrote this review, so it never judges its own verdict"
        );
        assert_eq!(why(&review, "claude"), "created the work this review reads");

        let mut reproduce = crate::domain::WorkUnit::pending("r:reproduce", "r", 3, "reproduce it");
        reproduce.stage = crate::domain::StageKind::Test;
        reproduce.role = crate::workflow::PhaseRole::Neutral;
        reproduce.assigned_cli = Some("codex".into());
        assert_eq!(
            why(&reproduce, "codex"),
            "ran this step, so it never judges its own output"
        );

        let mut fix = crate::domain::WorkUnit::pending("r:fix", "r", 4, "fix the flake");
        fix.stage = crate::domain::StageKind::Build;
        fix.role = crate::workflow::PhaseRole::Creator;
        fix.assigned_cli = Some("claude".into());
        assert_eq!(why(&fix, "claude"), BUILT_THIS_WORK);
        // A reviewer recorded as an author (a floor fix changed the tree on it) did build part.
        review.authored_by = vec!["codex".into()];
        assert_eq!(why(&review, "codex"), BUILT_THIS_WORK);

        // (xreview on #1147) A guarded read step on codex whose tree claude's floor fix changed:
        // claude built that, codex only ran the step.
        let mut recon = crate::domain::WorkUnit::pending("r:scope", "r", 1, "scope the change");
        recon.stage = crate::domain::StageKind::Recon;
        recon.role = crate::workflow::PhaseRole::Neutral;
        recon.executes_code = false;
        recon.worktree_guarded = true;
        recon.assigned_cli = Some("codex".into());
        recon.authored_by = vec!["claude".into()];
        assert!(
            crate::pipeline::authors_work(&recon),
            "the step has a recorded author"
        );
        assert_eq!(
            why(&recon, "codex"),
            "ran this step, so it never judges its own output"
        );
        assert_eq!(why(&recon, "claude"), BUILT_THIS_WORK);
    }

    /// (#539) When the pinned-validator path skips the judge (no distinct seat), the DEFAULT floor
    /// must NOT convene a judge either — even though a seat that is distinct for the default path
    /// (excluded=[creator only]) exists. The fix gates the default arm on `judge_skipped.is_none()`.
    ///
    /// Roster {claude, pi}, creator pi, approved validator:
    ///   pinned excluded=[claude (det author), pi (creator)] → no distinct seat → judge_skipped set
    ///   default excluded=[pi] → claude IS distinct, so without the fix the default arm runs on claude
    ///   → agent_verdict=Some AND judge_skipped=Some (broken invariant)
    ///   After the fix: default arm is skipped → agent_verdict=None, judge_skipped=Some.
    #[test]
    fn pinned_judge_skip_suppresses_default_floor_with_creator_pi() {
        use crate::workflow::{StepOutput, StepRunner};
        use std::sync::Mutex;

        #[derive(Default)]
        struct CountingRunner {
            judge_calls: Mutex<u32>,
        }
        impl StepRunner for CountingRunner {
            fn run_unit(&self, input: &StepInput) -> StepOutput {
                if input.unit.session_id == "validator" {
                    *self.judge_calls.lock().unwrap() += 1;
                }
                StepOutput {
                    run_id: input.run_id.clone(),
                    unit_ix: input.unit_ix,
                    attempt: input.attempt,
                    output: "PASS\nok\nPASS".into(),
                    status: StepStatus::Ok,
                    usage: None,
                    files: Vec::new(),
                    tools: Vec::new(),
                    governed: false,
                }
            }
        }

        let dir = std::env::temp_dir().join(format!(
            "wicked-core-pinned-suppress-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut unit = crate::domain::WorkUnit::pending("r:u1", "r", 1, "do the work");
        unit.assigned_cli = Some("pi".into());
        unit.validator = Some(crate::validator::DeterministicValidator {
            criterion: "the work is correct".into(),
            script: "test -f x".into(),
            approved: true,
        });
        // default_floor = true so default_floor_applies fires (tree_changed = Some(true) via
        // fail-closed when worktree_baseline is None).
        unit.default_floor = true;

        let input = StepInput {
            run_id: "r".into(),
            unit_ix: 0,
            attempt: 0,
            unit,
            workflow_id: "wf-r".into(),
            entity_mode: EntityMode::Isolated,
            workdir: Some(dir.clone()),
            governance: None,
            prior_outputs: vec![],
            elicitation_epoch: 0,
            process_gen: None,
            launch_seq: 0,
            required_skills: Vec::new(),
        };
        let noop: &DeltaSink = &|_: &str| {};
        // WICKED_BUS_DB must be unset so the inline path runs. Remove it for this test.
        std::env::remove_var("WICKED_BUS_DB");

        let roster = vec![
            seat("claude", "claude -p {PROMPT}"),
            seat("pi", "pi ask {PROMPT}"),
        ];
        let rec = Arc::new(CountingRunner::default());
        let runner: Arc<dyn StepRunner> = rec.clone();
        let (_out, verdict, evidence) =
            run_unit_and_judge_with_roster(&runner, &input, None, noop, &roster, &[], &[]);

        assert!(
            verdict.is_none(),
            "pinned skip must suppress the default floor: wire must never carry \
             agentVerdict:pass together with judgeSkippedReason (#539)"
        );
        assert!(
            evidence.judge_skipped.is_some(),
            "judge_skipped must be set by the pinned path even when default_floor_applies"
        );
        assert_eq!(
            *rec.judge_calls.lock().unwrap(),
            0,
            "no judge subprocess must be spawned when the pinned path already set judge_skipped"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (#539) The judge-skip reason must name each seat's eligibility outcome and must NOT list
    /// unusable seats (empty invocation) as eligible.
    #[test]
    fn judge_skip_reason_names_per_seat_outcome_and_excludes_unusable_from_eligible() {
        use crate::workflow::{StepOutput, StepRunner};

        struct NoopRunner;
        impl StepRunner for NoopRunner {
            fn run_unit(&self, input: &StepInput) -> StepOutput {
                StepOutput {
                    run_id: input.run_id.clone(),
                    unit_ix: input.unit_ix,
                    attempt: input.attempt,
                    output: String::new(),
                    status: StepStatus::Ok,
                    usage: None,
                    files: Vec::new(),
                    tools: Vec::new(),
                    governed: false,
                }
            }
        }

        let dir =
            std::env::temp_dir().join(format!("wicked-core-skip-reason-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut unit = crate::domain::WorkUnit::pending("r:u1", "r", 1, "do the work");
        unit.assigned_cli = Some("pi".into());
        unit.validator = Some(crate::validator::DeterministicValidator {
            criterion: "correct".into(),
            script: "true".into(),
            approved: true,
        });
        // Roster: claude (excluded as validator author), pi (excluded as creator), empty (unusable).
        // All three excluded/unusable → no distinct seat → pinned skips.
        let input = StepInput {
            run_id: "r".into(),
            unit_ix: 0,
            attempt: 0,
            unit,
            workflow_id: "wf-r".into(),
            entity_mode: EntityMode::Isolated,
            workdir: Some(dir.clone()),
            governance: None,
            prior_outputs: vec![],
            elicitation_epoch: 0,
            process_gen: None,
            launch_seq: 0,
            required_skills: Vec::new(),
        };
        let noop: &DeltaSink = &|_: &str| {};
        std::env::remove_var("WICKED_BUS_DB");

        let runner: Arc<dyn StepRunner> = Arc::new(NoopRunner);
        let roster = vec![
            seat("claude", "claude -p {PROMPT}"),
            seat("pi", "pi ask {PROMPT}"),
            // headless_invocation="" → unusable
            seat("ghost", ""),
        ];
        let (_out, _verdict, evidence) =
            run_unit_and_judge_with_roster(&runner, &input, None, noop, &roster, &[], &[]);

        let reason = evidence
            .judge_skipped
            .expect("judge_skipped must be set when no distinct seat exists");

        // The seat 'ghost' has an empty invocation — it must not be labeled as eligible.
        assert!(
            !reason.contains("ghost: eligible"),
            "unusable seat must not be described as eligible; got: {reason}"
        );
        // Each seat's outcome must be named.
        assert!(
            reason.contains("ghost") && reason.contains("unusable"),
            "unusable seat 'ghost' must appear with 'unusable' in reason; got: {reason}"
        );
        assert!(
            reason.contains("pi") && reason.contains("excluded"),
            "excluded creator seat 'pi' must appear with 'excluded' in reason; got: {reason}"
        );
        assert!(
            reason.contains("claude") && reason.contains("excluded"),
            "excluded validator-author seat 'claude' must appear with 'excluded' in reason; got: {reason}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn task_key_is_deterministic_per_exact_launch() {
        let generation = Some(uuid::Uuid::new_v4());
        let a = task_key(TASK_DISPATCHED, "run-1", 2, 0, generation, 1);
        let b = task_key(TASK_DISPATCHED, "run-1", 2, 0, generation, 1);
        assert_eq!(a, b, "same launch token ⇒ same key (idempotent)");
        assert_ne!(
            a,
            task_key(TASK_DISPATCHED, "run-1", 2, 1, generation, 1),
            "attempt varies the key"
        );
        assert_ne!(
            a,
            task_key(TASK_COMPLETED, "run-1", 2, 0, generation, 1),
            "event type varies the key"
        );
        assert_ne!(
            a,
            task_key(TASK_DISPATCHED, "run-1", 2, 0, generation, 2),
            "launch sequence varies the key"
        );
        assert_ne!(
            a,
            task_key(
                TASK_DISPATCHED,
                "run-1",
                2,
                0,
                Some(uuid::Uuid::new_v4()),
                1,
            ),
            "process generation varies the key"
        );
    }

    /// Verifies the `GateEvalRequest` wire format: `work_author` survives JSON serialization and
    /// `#[serde(default)]` preserves backward compat for receivers that emit the field as `null` or
    /// omit it entirely.
    ///
    /// Note: the timeout-deny behavior of `bus_request_agent_verdict` itself is not exercised here
    /// because the production timeout is 180s — too long for a unit test. The timeout path is covered
    /// by the doc-level contract guarantee: every error/timeout branch uses `bus_deny!` (infallible
    /// deny) rather than returning `None`. The serialization test below remains the regression guard
    /// for the wire contract that the evaluator daemon depends on.
    #[test]
    fn gate_eval_request_wire_format_work_author_survives_round_trip() {
        let dir =
            std::env::temp_dir().join(format!("wicked-core-bus-timeout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bus_path = dir.join("bus.db").to_str().unwrap().to_string();
        // Open the DB so it exists, but publish NO GATE_EVAL_RESPONDED — the poller will time out.
        // Use a 0-second budget so the test runs in <1s (the real budget is 180s for production).
        let _db = crate::bus::BusDb::open(&bus_path).unwrap();

        // Override the timeout to zero so the poll loop exits immediately.
        // We can't set GATE_EVAL_TIMEOUT from here (it's a const), so we call bus_request_agent_verdict
        // with a deliberately-zero-response DB and verify the verdict is a deny.
        // Since there are no GATE_EVAL_RESPONDED events and the timeout fires instantly (the const is
        // 180s in production — too long for a test), we instead verify the SHAPE of GateEvalRequest:
        // it must include `work_author`, and the serialized payload must round-trip correctly.

        let req = GateEvalRequest {
            eval_id: "test-eval-id".into(),
            criterion: "did it pass?".into(),
            work: "output text".into(),
            run_id: "run-abc".into(),
            unit_ix: 1,
            attempt: 0,
            work_author: Some("claude".into()),
            excluded_seats: Vec::new(),
        };
        let json = serde_json::to_value(&req).unwrap();

        // work_author must survive the round-trip — the daemon needs it to enforce evaluator≠creator.
        assert_eq!(
            json["work_author"],
            serde_json::json!("claude"),
            "work_author must be serialized into GateEvalRequest so the evaluator daemon can \
             exclude the work author from seat selection"
        );
        // Deserialise back — `#[serde(default)]` means an absent field deserialises to None.
        let req2: GateEvalRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req2.work_author.as_deref(), Some("claude"));

        // Absent work_author (old wire compat) deserialises to None — the daemon must apply
        // maximum exclusion in that case rather than proceeding without the guard.
        let legacy = serde_json::json!({
            "eval_id": "e", "criterion": "c", "work": "w",
            "run_id": "r", "unit_ix": 0u32, "attempt": 0u32
        });
        let legacy_req: GateEvalRequest = serde_json::from_value(legacy).unwrap();
        assert!(
            legacy_req.work_author.is_none(),
            "absent work_author on old wire deserialises to None (not a parse error)"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// BUS-PATH EVALUATOR≠CREATOR — GateEvalRequest carries `work_author` so the evaluator daemon
    /// can enforce the same exclusion the inline path enforces via `excluded = [det_seat, work_author]`.
    ///
    /// This complements `agent_judge_excludes_the_work_author_seat_c1` (which covers the inline path)
    /// by verifying the WIRE CONTRACT carries the author identity across the bus boundary.
    ///
    /// Mutation test: remove the `work_author` field from `GateEvalRequest` and the assertion at the
    /// round-trip step fails — proving the field is structurally present, not just commented-about.
    #[test]
    fn gate_eval_request_carries_work_author_for_bus_path_evaluator_creator_separation() {
        // With work_author populated:
        let with_author = GateEvalRequest {
            eval_id: "e1".into(),
            criterion: "passed?".into(),
            work: "output".into(),
            run_id: "r1".into(),
            unit_ix: 0,
            attempt: 0,
            work_author: Some("agy".into()),
            excluded_seats: Vec::new(),
        };
        let v = serde_json::to_value(&with_author).unwrap();
        assert_eq!(
            v["work_author"], "agy",
            "work_author must be in the JSON payload"
        );

        // Without work_author (unit had no assigned_cli):
        let without_author = GateEvalRequest {
            eval_id: "e2".into(),
            criterion: "passed?".into(),
            work: "output".into(),
            run_id: "r1".into(),
            unit_ix: 0,
            attempt: 0,
            work_author: None,
            excluded_seats: Vec::new(),
        };
        let v2 = serde_json::to_value(&without_author).unwrap();
        assert!(
            v2["work_author"].is_null(),
            "absent work_author serialises to null so the daemon can detect and apply max-exclusion"
        );
    }

    // ── T7-a: backward-compat serde ──────────────────────────────────────────────────────────────────

    /// T7-a: A pre-DES-002 `DispatchedTask` JSON (missing `process_gen`, `launch_seq`, `is_acp`)
    /// deserializes successfully; the new fields default to `None`, `0`, and `false` respectively.
    #[test]
    fn t7a_old_dispatched_task_json_deserializes_with_defaults() {
        // Build a DispatchedTask using a real unit and serialize it to JSON, then strip the new
        // fields to simulate a pre-DES-002 payload arriving from an older sender.
        let unit = crate::domain::WorkUnit::pending("t7a:u1", "t7a", 1, "do it");
        let input = StepInput {
            run_id: "t7a-run".into(),
            unit_ix: 0,
            attempt: 0,
            unit,
            workflow_id: "wf-t7a".into(),
            entity_mode: EntityMode::Shared,
            workdir: None,
            governance: None,
            prior_outputs: vec![],
            elicitation_epoch: 0,
            process_gen: Some(uuid::Uuid::new_v4()), // will be stripped below
            launch_seq: 5,                           // will be stripped below,
            required_skills: Vec::new(),
        };
        // Publish a dispatched task to a real bus db so we can round-trip through serde.
        let bus_path = tmp_bus("t7a");
        arm_exec_publisher(BusDb::shared(&bus_path).expect("arm publisher"));
        assert!(
            try_publish_dispatched(&input, None, true),
            "publish task.dispatched"
        );
        disarm_exec_publisher();

        let bus = BusDb::open(&bus_path).unwrap();
        let evs = bus.poll(TASK_DISPATCHED, 0, 10).unwrap();
        assert_eq!(evs.len(), 1);

        // Now strip the new DES-002 fields from the payload to simulate a pre-DES-002 sender.
        let mut payload = evs[0].payload.clone();
        if let Some(obj) = payload.as_object_mut() {
            obj.remove("process_gen");
            obj.remove("launch_seq");
            obj.remove("is_acp");
        }

        let task: DispatchedTask = serde_json::from_value(payload)
            .expect("pre-DES-002 payload must deserialize even without new fields");
        assert_eq!(task.process_gen, None, "process_gen defaults to None");
        assert_eq!(task.launch_seq, 0, "launch_seq defaults to 0");
        assert!(!task.is_acp, "is_acp defaults to false");

        let _ = std::fs::remove_dir_all(std::path::Path::new(&bus_path).parent().unwrap());
    }

    // ── T7-b/c: status string roundtrips ─────────────────────────────────────────────────────────────

    /// T7-b: `status_to_str(ElicitationFailed)` returns `"elicitation_failed"`.
    #[test]
    fn t7b_status_to_str_elicitation_failed() {
        assert_eq!(
            status_to_str(StepStatus::ElicitationFailed),
            "elicitation_failed"
        );
    }

    /// T7-c: `status_from_str("elicitation_failed")` returns `StepStatus::ElicitationFailed`.
    #[test]
    fn t7c_status_from_str_elicitation_failed() {
        assert_eq!(
            status_from_str("elicitation_failed"),
            StepStatus::ElicitationFailed
        );
    }

    /// T7-d: `ElicitationFailed` round-trips through the status string encoding; it does NOT
    /// fall through to the `Ok` wildcard (which would silently succeed a failed run).
    #[test]
    fn t7d_elicitation_failed_roundtrips_through_status_string() {
        let s = StepStatus::ElicitationFailed;
        assert_eq!(
            status_from_str(status_to_str(s)),
            StepStatus::ElicitationFailed,
            "ElicitationFailed must survive a serialization round-trip"
        );
        // Confirm it is NOT remapped to Ok (the wildcard arm must NOT catch it).
        assert_ne!(
            status_from_str(status_to_str(s)),
            StepStatus::Ok,
            "ElicitationFailed must NOT fall through to the Ok wildcard"
        );
    }

    // ── T7-e/f: startup reclamation ──────────────────────────────────────────────────────────────────

    fn tmp_bus(tag: &str) -> String {
        let dir =
            std::env::temp_dir().join(format!("wc-clirunner-t7-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("bus.db").to_str().unwrap().to_string()
    }

    /// T7-e: startup reclamation migrates cursor positions BEFORE deleting old rows, and deletes
    /// old rows BEFORE calling `set_stable`. After reclamation, new consumer names hold the
    /// predecessor's cursor positions, old rows are gone, and stable reflects the new consumer name.
    #[test]
    fn t7e_startup_reclamation_ordering_migrate_delete_setstable() {
        let bus_path = tmp_bus("t7e");
        let bus = BusDb::open(&bus_path).unwrap();

        let old_gen = uuid::Uuid::new_v4();
        let new_gen = uuid::Uuid::new_v4();
        let old_consumer = consumer_name(old_gen);
        let old_completed = completed_consumer_name(old_gen);
        let new_consumer = consumer_name(new_gen);
        let new_completed = completed_consumer_name(new_gen);

        // Arrange: old process left cursor rows
        bus.save_cursor(&old_consumer, 42).unwrap();
        bus.save_cursor(&old_completed, 99).unwrap();

        // Set up stable so the reclamation knows the old consumer name.
        let workspace_id = "test-workspace-t7e";
        let stable_key = format!("{CORE_STABLE_ID_KEY_PREFIX}{workspace_id}");
        // First run: create core_stable_id and set owner_key to old_consumer.
        let core_stable_id = uuid::Uuid::new_v4().to_string();
        bus.set_stable(&stable_key, &core_stable_id).unwrap();
        let owner_key = format!("cli-runner-cursor-owner-{core_stable_id}");
        bus.set_stable(&owner_key, &old_consumer).unwrap();

        // Act: reclaim
        let predecessor_gen_opt =
            reclaim_predecessor_cursors(&bus, workspace_id, &new_consumer, &new_completed);

        // Assert outer Some (no DB error)
        assert!(predecessor_gen_opt.is_some(), "reclamation must succeed");
        let predecessor_gen = predecessor_gen_opt.unwrap();

        // New consumer has the migrated positions.
        assert_eq!(
            bus.load_cursor(&new_consumer).unwrap(),
            Some(42),
            "dispatch cursor migrated to new consumer"
        );
        assert_eq!(
            bus.load_cursor(&new_completed).unwrap(),
            Some(99),
            "completed cursor migrated to new consumer"
        );
        // Old rows are deleted.
        assert_eq!(
            bus.load_cursor(&old_consumer).unwrap(),
            None,
            "old dispatch cursor deleted"
        );
        assert_eq!(
            bus.load_cursor(&old_completed).unwrap(),
            None,
            "old completed cursor deleted"
        );
        // Stable updated to new consumer.
        assert_eq!(
            bus.get_stable(&owner_key).unwrap().as_deref(),
            Some(new_consumer.as_str()),
            "stable updated to new consumer name"
        );
        // predecessor_gen derived correctly
        assert_eq!(
            predecessor_gen,
            Some(old_gen),
            "predecessor_gen matches old actor gen"
        );

        let _ = std::fs::remove_dir_all(std::path::Path::new(&bus_path).parent().unwrap());
    }

    /// T7-f: `predecessor_gen` is `Some(_)` when an old consumer exists in the stable record;
    /// `None` when no old consumer was registered (first boot of this workspace).
    #[test]
    fn t7f_predecessor_gen_some_when_old_consumer_exists_none_otherwise() {
        let bus_path = tmp_bus("t7f");
        let bus = BusDb::open(&bus_path).unwrap();
        let workspace_id = "test-workspace-t7f";
        let new_gen = uuid::Uuid::new_v4();
        let new_consumer = consumer_name(new_gen);
        let new_completed = completed_consumer_name(new_gen);

        // First boot: no predecessor.
        let result = reclaim_predecessor_cursors(&bus, workspace_id, &new_consumer, &new_completed);
        assert!(result.is_some(), "reclamation succeeds on first boot");
        assert_eq!(result.unwrap(), None, "no predecessor on first boot → None");

        // Simulate second boot with a predecessor.
        let stable_key = format!("{CORE_STABLE_ID_KEY_PREFIX}{workspace_id}");
        let core_stable_id = bus.get_stable(&stable_key).unwrap().unwrap();
        let owner_key = format!("cli-runner-cursor-owner-{core_stable_id}");

        // Overwrite stable to simulate a predecessor having been set.
        let old_gen = uuid::Uuid::new_v4();
        let old_consumer = consumer_name(old_gen);
        bus.set_stable(&owner_key, &old_consumer).unwrap();
        let new_gen2 = uuid::Uuid::new_v4();
        let new2 = consumer_name(new_gen2);
        let new2_completed = completed_consumer_name(new_gen2);
        let result2 = reclaim_predecessor_cursors(&bus, workspace_id, &new2, &new2_completed);
        assert!(result2.is_some(), "reclamation succeeds with predecessor");
        assert_eq!(
            result2.unwrap(),
            Some(old_gen),
            "predecessor_gen is Some(old_gen) when old consumer exists"
        );

        let _ = std::fs::remove_dir_all(std::path::Path::new(&bus_path).parent().unwrap());
    }

    // ── T7-g: ack-gated cursor advance ───────────────────────────────────────────────────────────────

    /// T7-g: The `task.completed` poller does NOT advance its cursor if `ack_rx.recv()` returns
    /// `Err` (actor died between dequeue and commit, simulated by dropping the command channel
    /// receiver before the ack can be sent).
    #[test]
    fn t7g_task_completed_poller_cursor_not_advanced_if_ack_fails() {
        let bus_path = tmp_bus("t7g");
        let bus = BusDb::open(&bus_path).unwrap();
        let consumer = "cli-runner-completed-test-t7g".to_string();

        // Publish one task.completed event
        let actor_gen = uuid::Uuid::new_v4();
        let payload = serde_json::json!({
            "run_id": "r-t7g", "unit_ix": 0, "attempt": 0,
            "output": "ok", "status": "ok",
            "governed": false,
            "process_gen": actor_gen,
            "launch_seq": 1
        });
        let ev = crate::bus::BusEmit::new(TASK_COMPLETED, CORE_DOMAIN, "test", payload);
        bus.emit(&ev).unwrap();

        // Build the poller DB and set the floor before the event
        let poller_db = BusDb::open(&bus_path).unwrap();

        // Create a command channel where the receiver is dropped immediately, simulating a crash.
        let (tx, rx) = std::sync::mpsc::channel::<Command>();
        drop(rx); // actor is gone — send() will fail immediately

        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_clone = stop.clone();

        // Set cursor to 0 (before the event) and run the poller in a thread.
        poller_db.save_cursor(&consumer, 0).unwrap();

        let consumer_clone = consumer.clone();
        let handle = std::thread::spawn(move || {
            run_task_completed_poller(
                poller_db,
                0,
                consumer_clone,
                tx,
                actor_gen,
                None,
                std::time::Duration::from_millis(50),
                stop_clone,
            )
        });
        // Wait for the inner JoinHandle to spawn the worker thread
        let inner = handle.join().unwrap();

        // Signal stop and wait for the worker to exit
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = inner.join();

        // Verify the cursor was NOT advanced (actor channel was closed — send() fails → return)
        let new_bus = BusDb::open(&bus_path).unwrap();
        assert_eq!(
            new_bus.load_cursor(&consumer).unwrap(),
            Some(0),
            "cursor must stay at 0 when the actor channel is closed"
        );

        let _ = std::fs::remove_dir_all(std::path::Path::new(&bus_path).parent().unwrap());
    }

    // ── T7-h: find_completed with interleaved events ─────────────────────────────────────────────────

    /// T7-h: `BusDb::find_completed` returns the correct completion when events from other
    /// `run_id`s appear before the target in the stream (interleaved).
    #[test]
    fn t7h_find_completed_scans_past_unrelated_run_ids() {
        let bus_path = tmp_bus("t7h");
        let bus = BusDb::open(&bus_path).unwrap();
        let consumer = "cli-runner-completed-t7h".to_string();

        let launch_seq_target: u64 = 7;
        let launch_seq_other: u64 = 3;

        // Publish an UNRELATED completion first (different run_id and launch_seq)
        let unrelated = serde_json::json!({
            "run_id": "other-run", "unit_ix": 0, "attempt": 0,
            "output": "nope", "status": "ok", "governed": false,
            "launch_seq": launch_seq_other,
            "process_gen": uuid::Uuid::new_v4().to_string()
        });
        bus.emit(&crate::bus::BusEmit::new(
            TASK_COMPLETED,
            CORE_DOMAIN,
            "test",
            unrelated,
        ))
        .unwrap();

        // Then publish the TARGET completion
        let target_gen = uuid::Uuid::new_v4();
        let target = serde_json::json!({
            "run_id": "target-run", "unit_ix": 0, "attempt": 0,
            "output": "found-it", "status": "ok", "governed": false,
            "launch_seq": launch_seq_target,
            "process_gen": target_gen.to_string()
        });
        bus.emit(&crate::bus::BusEmit::new(
            TASK_COMPLETED,
            CORE_DOMAIN,
            "test",
            target,
        ))
        .unwrap();

        // Set cursor to 0 so find_completed scans from the start.
        bus.save_cursor(&consumer, 0).unwrap();

        let result = bus
            .find_completed(&consumer, "target-run", launch_seq_target)
            .expect("find_completed must not error");

        assert!(result.is_some(), "should find the target completion");
        let found = result.unwrap();
        let payload_output = found.payload.get("output").and_then(|v| v.as_str());
        assert_eq!(
            payload_output,
            Some("found-it"),
            "must return the target event, not the unrelated one"
        );

        // Searching for the unrelated run also works.
        let result2 = bus
            .find_completed(&consumer, "other-run", launch_seq_other)
            .expect("find_completed for unrelated must not error");
        assert!(
            result2.is_some(),
            "should find the unrelated completion too"
        );

        // A non-existent run returns None.
        let result3 = bus
            .find_completed(&consumer, "ghost-run", 99)
            .expect("find_completed for ghost must not error");
        assert!(result3.is_none(), "non-existent run returns None");

        let _ = std::fs::remove_dir_all(std::path::Path::new(&bus_path).parent().unwrap());
    }

    // ── Test 38: degraded-mode ack-gated cursor advance ──────────────────────────────────────────────

    /// Test 38: When `has_activated_seq=true` and `is_bus_worker_in_flight=false` (degraded mode —
    /// task was activated but the worker is gone), the bus consumer emits a synthetic
    /// `ElicitationFailed` `ApplyStepResult`, gates cursor advance on the ack, and the run state
    /// is driven to terminal by the actor's handler.
    ///
    /// Sub-case (a): actor acks → cursor advanced.
    /// Sub-case (b): actor drops ack sender without sending → cursor NOT advanced.
    #[test]
    fn test_38_degraded_mode_ack_gated_cursor_advance() {
        use crate::acp_runner::ElicitationMaps;
        use std::sync::{Arc, Mutex};

        // Stub runner that must never be called in degraded mode.
        struct NeverRunner;
        impl StepRunner for NeverRunner {
            fn run_unit(&self, _: &StepInput) -> StepOutput {
                panic!("NeverRunner.run_unit called — should not execute in degraded mode");
            }
        }

        // ── sub-case (a): actor acks → cursor must advance ────────────────────────────

        let bus_path_a = tmp_bus("t38a");
        let actor_gen_a = uuid::Uuid::new_v4();
        let c_name_a = consumer_name(actor_gen_a);
        let cc_name_a = completed_consumer_name(actor_gen_a);

        let maps_a = Arc::new(Mutex::new(ElicitationMaps::new()));
        {
            let mut m = maps_a.lock().unwrap();
            let seq = m.begin_launch("run-t38a", false);
            assert_eq!(seq, 1);
            m.try_next_epoch_bus("run-t38a", seq, false);
            m.mark_bus_in_flight("run-t38a", seq);
            m.clear_bus_in_flight("run-t38a", seq);
            assert!(m.has_activated_seq("run-t38a", seq), "should be activated");
            assert!(
                !m.is_bus_worker_in_flight("run-t38a", seq),
                "should NOT be in-flight"
            );
        }

        // Publish a properly-serialized task.dispatched so serde can parse it (GateSpec::Auto
        // serializes as the string "auto", not {"kind":"none"} — hand-crafted JSON would be a
        // poison payload and the cursor would advance for the wrong reason).
        let unit_a =
            crate::domain::WorkUnit::pending("run-t38a:u1", "run-t38a", 1, "degraded-mode test");
        let input_a = StepInput {
            run_id: "run-t38a".into(),
            unit_ix: 0,
            attempt: 0,
            unit: unit_a,
            workflow_id: "wf".into(),
            entity_mode: EntityMode::Shared,
            workdir: None,
            governance: None,
            prior_outputs: vec![],
            elicitation_epoch: 0,
            process_gen: Some(actor_gen_a),
            launch_seq: 1,
            required_skills: Vec::new(),
        };
        arm_exec_publisher(BusDb::shared(&bus_path_a).expect("arm publisher t38a"));
        assert!(
            try_publish_dispatched(&input_a, None, false),
            "publish t38a"
        );
        disarm_exec_publisher();

        {
            let bus_a = BusDb::open(&bus_path_a).unwrap();
            bus_a.save_cursor(&c_name_a, 0).unwrap();
            let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Command>();
            let stop_a = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let stop_a2 = stop_a.clone();

            let ack_thread = std::thread::spawn(move || {
                if let Ok(Command::ApplyStepResult {
                    ack: Some(ack_tx), ..
                }) = cmd_rx.recv_timeout(std::time::Duration::from_secs(5))
                {
                    let _ = ack_tx.send(());
                }
            });

            let maps_clone = maps_a.clone();
            let bus_path_clone = bus_path_a.clone();
            let c_name_clone = c_name_a.clone();
            let cc_name_clone = cc_name_a.clone();
            let runner: Arc<dyn StepRunner> = Arc::new(NeverRunner);
            let handle = std::thread::spawn(move || {
                run_cli_runner(
                    BusDb::open(&bus_path_clone).unwrap(),
                    0,
                    runner,
                    cmd_tx,
                    Some(maps_clone),
                    actor_gen_a,
                    None,
                    c_name_clone,
                    cc_name_clone,
                    std::time::Duration::from_millis(50),
                    stop_a2,
                    None,
                )
            });
            let inner = handle.join().unwrap();
            ack_thread.join().unwrap();
            stop_a.store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = inner.join();

            let pos = BusDb::open(&bus_path_a)
                .unwrap()
                .load_cursor(&c_name_a)
                .unwrap();
            assert!(
                pos.unwrap_or(0) > 0,
                "cursor must advance after ack in degraded mode (sub-case a)"
            );
        }
        let _ = std::fs::remove_dir_all(std::path::Path::new(&bus_path_a).parent().unwrap());

        // ── sub-case (b): actor drops ack → cursor must NOT advance ──────────────────

        let bus_path_b = tmp_bus("t38b");
        let actor_gen_b = uuid::Uuid::new_v4();
        let c_name_b = consumer_name(actor_gen_b);
        let cc_name_b = completed_consumer_name(actor_gen_b);

        let maps_b = Arc::new(Mutex::new(ElicitationMaps::new()));
        {
            let mut m = maps_b.lock().unwrap();
            let seq = m.begin_launch("run-t38b", false);
            assert_eq!(seq, 1);
            m.try_next_epoch_bus("run-t38b", seq, false);
            m.mark_bus_in_flight("run-t38b", seq);
            m.clear_bus_in_flight("run-t38b", seq);
            assert!(m.has_activated_seq("run-t38b", seq), "should be activated");
            assert!(
                !m.is_bus_worker_in_flight("run-t38b", seq),
                "should NOT be in-flight"
            );
        }

        // Same publish pattern as sub-case (a): use a real StepInput so GateSpec serializes
        // correctly as "auto", not {"kind":"none"}.
        let unit_b =
            crate::domain::WorkUnit::pending("run-t38b:u1", "run-t38b", 1, "degraded-mode test");
        let input_b = StepInput {
            run_id: "run-t38b".into(),
            unit_ix: 0,
            attempt: 0,
            unit: unit_b,
            workflow_id: "wf".into(),
            entity_mode: EntityMode::Shared,
            workdir: None,
            governance: None,
            prior_outputs: vec![],
            elicitation_epoch: 0,
            process_gen: Some(actor_gen_b),
            launch_seq: 1,
            required_skills: Vec::new(),
        };
        arm_exec_publisher(BusDb::shared(&bus_path_b).expect("arm publisher t38b"));
        assert!(
            try_publish_dispatched(&input_b, None, false),
            "publish t38b"
        );
        disarm_exec_publisher();

        {
            let bus_b = BusDb::open(&bus_path_b).unwrap();
            bus_b.save_cursor(&c_name_b, 0).unwrap();
            let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Command>();
            let stop_b = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let stop_b2 = stop_b.clone();

            // Actor: receive command, DROP ack without sending (simulates actor crash), then stop consumer.
            let stop_b3 = stop_b.clone();
            let ack_drop_thread = std::thread::spawn(move || {
                if let Ok(Command::ApplyStepResult { ack, .. }) =
                    cmd_rx.recv_timeout(std::time::Duration::from_secs(5))
                {
                    drop(ack); // drop without sending → ack_rx.recv() returns Err
                }
                // Signal the consumer to stop so it doesn't loop indefinitely.
                stop_b3.store(true, std::sync::atomic::Ordering::SeqCst);
            });

            let maps_clone = maps_b.clone();
            let bus_path_clone = bus_path_b.clone();
            let c_name_clone = c_name_b.clone();
            let cc_name_clone = cc_name_b.clone();
            let runner: Arc<dyn StepRunner> = Arc::new(NeverRunner);
            let handle = std::thread::spawn(move || {
                run_cli_runner(
                    BusDb::open(&bus_path_clone).unwrap(),
                    0,
                    runner,
                    cmd_tx,
                    Some(maps_clone),
                    actor_gen_b,
                    None,
                    c_name_clone,
                    cc_name_clone,
                    std::time::Duration::from_millis(50),
                    stop_b2,
                    None,
                )
            });
            let inner = handle.join().unwrap();
            ack_drop_thread.join().unwrap();
            let _ = inner.join();

            let pos = BusDb::open(&bus_path_b)
                .unwrap()
                .load_cursor(&c_name_b)
                .unwrap();
            assert_eq!(
                pos.unwrap_or(0),
                0,
                "cursor must NOT advance when actor drops ack (sub-case b)"
            );
        }
        let _ = std::fs::remove_dir_all(std::path::Path::new(&bus_path_b).parent().unwrap());
    }

    /// core#782: a FLOOR FIX armed on a read-only phase hands the operator's note to the fixing
    /// seat (a creator posture on a seat other than the phase's), the phase itself does not run
    /// again (its reviewed output is what the fold reads), the guard re-baselines on the fixed tree
    /// so the fix is not the phase's mutation, and only the floor runs on that tree.
    #[test]
    fn a_floor_fix_hands_the_note_to_a_distinct_seat_and_re_runs_only_the_floor() {
        use crate::workflow::{StepOutput, StepRunner};
        use std::process::Command;
        use std::sync::Mutex;

        struct Fixer {
            calls: Mutex<Vec<crate::domain::WorkUnit>>,
        }
        impl StepRunner for Fixer {
            fn run_unit(&self, input: &StepInput) -> StepOutput {
                self.calls.lock().unwrap().push(input.unit.clone());
                if let Some(wd) = &input.workdir {
                    std::fs::write(wd.join("fixed.txt"), "fixed\n").unwrap();
                }
                StepOutput {
                    run_id: input.run_id.clone(),
                    unit_ix: input.unit_ix,
                    attempt: input.attempt,
                    output: "made the fix".into(),
                    status: StepStatus::Ok,
                    usage: None,
                    files: Vec::new(),
                    tools: Vec::new(),
                    governed: false,
                }
            }
        }
        fn sh(cwd: &std::path::Path, args: &[&str]) {
            // spawn-audit: test-only — a git fixture building the layout under test.
            let out = Command::new("git")
                .args(args)
                .current_dir(cwd)
                .output()
                .expect("git runs");
            assert!(out.status.success(), "git {args:?}: {out:?}");
        }
        let base =
            std::env::temp_dir().join(format!("wicked-core-floor-fix-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("repo");
        std::fs::create_dir_all(repo.join(".wicked")).unwrap();
        sh(&repo, &["init", "-q", "."]);
        sh(&repo, &["config", "user.email", "t@example.invalid"]);
        sh(&repo, &["config", "user.name", "t"]);
        sh(&repo, &["config", "commit.gpgsign", "false"]);
        sh(&repo, &["config", "core.autocrlf", "false"]);
        std::fs::write(
            repo.join(crate::repo_checks::CONFIG_PATH),
            serde_json::json!({ "test": ["sh", "-c", "test -f fixed.txt"] }).to_string(),
        )
        .unwrap();
        sh(&repo, &["add", "-A"]);
        sh(&repo, &["commit", "-qm", "base"]);
        let wt = base.join("wt");
        sh(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                wt.to_str().unwrap(),
                "-b",
                "wicked/floor-fix",
            ],
        );

        const REVIEWED: &str = "Reviewed the change; it is correct.\nVERDICT: PASS";
        let red: crate::repo_checks::RepoChecksReport = serde_json::from_value(serde_json::json!({
            "detected": [], "checks": [], "skipped": [], "passed": false, "sandbox_level": "none"
        }))
        .unwrap();
        let mut unit = crate::domain::WorkUnit::pending("r:test", "r", 4, "test the change");
        unit.role = crate::workflow::PhaseRole::Evaluator;
        unit.assigned_cli = Some("claude".into());
        unit.worktree_guarded = true;
        unit.repo_checks_floor = true;
        unit.skill_ref = Some("wicked-garden-qe-tester".into());
        unit.worktree_baseline = Some(crate::worktree_guard::snapshot(&wt, &repo).unwrap());
        unit.repo_checks = Some(crate::repo_checks::RepoChecksReport {
            requested_rerun: Some(crate::repo_checks::FloorRerun {
                mode: crate::repo_checks::FloorRerunMode::FloorFix,
                waive: Vec::new(),
                output: REVIEWED.into(),
                fix: Some(crate::repo_checks::FloorFix {
                    note: "create fixed.txt".into(),
                    seat: "codex".into(),
                }),
            }),
            ..red
        });
        let input = StepInput {
            run_id: "r".into(),
            unit_ix: 3,
            attempt: 1,
            unit,
            workflow_id: "wf-r".into(),
            entity_mode: EntityMode::Isolated,
            workdir: Some(wt.clone()),
            governance: None,
            prior_outputs: vec![],
            elicitation_epoch: 0,
            process_gen: None,
            launch_seq: 0,
            required_skills: Vec::new(),
        };
        let fixer = Arc::new(Fixer {
            calls: Mutex::new(Vec::new()),
        });
        let runner: Arc<dyn StepRunner> = fixer.clone();
        let noop: &DeltaSink = &|_| {};
        let (out, verdict, evidence) =
            run_unit_and_judge_with_roster(&runner, &input, None, noop, &[], &[], &[]);

        // The note reached exactly one seat — the fixing one, with a creator's write posture and
        // the note as its task (not the read-only phase's skill) — and the phase did not re-run.
        let calls = fixer.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1, "only the fix seat ran: {calls:?}");
        let task = &calls[0].description;
        assert_eq!(
            calls[0].assigned_cli.as_deref(),
            Some("codex"),
            "the fix seat, never the phase's own"
        );
        assert_eq!(calls[0].role, crate::workflow::PhaseRole::Creator);
        assert!(
            !calls[0].worktree_guarded,
            "the fixing seat may write the tree"
        );
        assert!(
            task.contains("create fixed.txt"),
            "the note is the task: {task}"
        );
        assert!(!task.contains("qe-tester"), "not the phase's skill: {task}");
        // The fold reads the reviewed output: the phase's verdict stands.
        assert_eq!(out.status, StepStatus::Ok);
        assert_eq!(out.output, REVIEWED);
        assert!(verdict.is_none(), "no judge re-ran");
        // The guard re-baselined on the fixed tree: the fix is not the phase's mutation.
        assert!(
            matches!(
                evidence.worktree_guard,
                Some(crate::worktree_guard::WorktreeGuardOutcome::Clean { .. })
            ),
            "{:?}",
            evidence.worktree_guard
        );
        let report = evidence.repo_checks.expect("the floor ran");
        assert_eq!(
            report.rerun,
            Some(crate::repo_checks::FloorRerunMode::FloorFix)
        );
        if report.sandbox_error.is_some() {
            eprintln!("cli_runner: no sandbox tool here — the floor fix's floor cannot run");
            assert!(
                report.key.is_none(),
                "a floor that did not run is never keyed"
            );
        } else {
            assert!(report.passed, "the floor judged the FIXED tree: {report:?}");
            // (deliver honors an override) An operator's floor re-run (here a floor fix) is never
            // keyed: nothing stands on it in place of the ordinary floor at deliver.
            assert!(report.key.is_none(), "a re-run is never keyed");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// core#782 (review): a floor fix whose guard baseline cannot be re-taken leaves NO baseline,
    /// so the guard reads the tree as unverifiable and denies — fail closed, never a pass.
    #[test]
    fn a_floor_fix_that_cannot_re_baseline_leaves_the_guard_unverifiable() {
        use crate::workflow::{StepOutput, StepRunner};
        struct Ok1;
        impl StepRunner for Ok1 {
            fn run_unit(&self, i: &StepInput) -> StepOutput {
                StepOutput {
                    run_id: i.run_id.clone(),
                    unit_ix: i.unit_ix,
                    attempt: i.attempt,
                    output: "fixed".into(),
                    status: StepStatus::Ok,
                    usage: None,
                    files: Vec::new(),
                    tools: Vec::new(),
                    governed: false,
                }
            }
        }
        let dir =
            std::env::temp_dir().join(format!("wicked-core-ff-nobase-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut unit = crate::domain::WorkUnit::pending("r:test", "r", 4, "test it");
        unit.worktree_guarded = true;
        unit.worktree_baseline = Some(crate::worktree_guard::WorktreeSnapshot {
            head: "h".into(),
            head_ref: None,
            tree: "t".into(),
            taken_at_ms: 1,
            git_dir: None,
        });
        let input = StepInput {
            run_id: "r".into(),
            unit_ix: 3,
            attempt: 1,
            unit,
            workflow_id: "wf-r".into(),
            entity_mode: EntityMode::Isolated,
            workdir: Some(dir.clone()),
            governance: None,
            prior_outputs: vec![],
            elicitation_epoch: 0,
            process_gen: None,
            launch_seq: 0,
            required_skills: Vec::new(),
        };
        let runner: Arc<dyn StepRunner> = Arc::new(Ok1);
        let fix = crate::repo_checks::FloorFix {
            note: "n".into(),
            seat: "codex".into(),
        };
        let (next, _) = run_floor_fix(&runner, &input, &fix, &|_| {});
        assert!(next.unit.worktree_baseline.is_none());
        assert!(matches!(
            crate::worktree_guard::outcome_for_unit(&next.unit, Some(&dir)),
            Some(crate::worktree_guard::WorktreeGuardOutcome::Unverifiable(_))
        ));
        assert!(next.unit.exclude_seats.iter().any(|s| s == "codex"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review of #449, RT-1: the DEFAULT judge is drawn from the registry seats the RUN
    /// configured (never a registry seat the launcher never seated), skips the bench, and a judge
    /// seat that refuses with an authentication failure is reported for the bench — so the next
    /// unit never re-tries it.
    /// (PRODUCT-WORKS J4) The rig's copilot was benched for quota (429) and never re-checked, so
    /// every later review had no distinct judge and the operator judged instead. A due quota bench
    /// is re-probed right before the judge reads the bench: a seat that answers judges this very
    /// gate; one that refuses again is re-benched with the next backoff and the gate still has no
    /// judge; one that says anything but READY did not come back.
    #[test]
    fn a_due_quota_bench_is_reprobed_and_the_gate_that_had_no_judge_picks_the_returned_seat() {
        use std::process::Command;
        use std::sync::Mutex;
        const MIN: i64 = 60 * 1000;
        struct Seats {
            /// What a seat says to its re-probe.
            answer: Mutex<String>,
            /// How long the unit's own work takes (a bench can fall due meanwhile).
            work_ms: Mutex<u64>,
            calls: Mutex<Vec<String>>,
        }
        impl StepRunner for Seats {
            fn run_unit(&self, input: &StepInput) -> StepOutput {
                let mut out = StepOutput {
                    run_id: input.run_id.clone(),
                    unit_ix: input.unit_ix,
                    attempt: input.attempt,
                    output: "did".into(),
                    status: StepStatus::Ok,
                    usage: None,
                    files: Vec::new(),
                    tools: Vec::new(),
                    governed: false,
                };
                let seat = input.unit.assigned_cli.clone().unwrap_or_default();
                let call = if input.unit.id == crate::validator::REPROBE_UNIT {
                    "reprobe"
                } else {
                    input.unit.session_id.as_str()
                };
                match call {
                    "reprobe" => {
                        assert!(
                            input.workdir.is_none()
                                && input.governance.is_none()
                                && crate::execute_wrapped::is_engine_internal(&input.unit),
                            "the re-probe is the judge's ungoverned one-shot: no workdir, and one \
                             of the engine's own sessions (it follows the seat floor)"
                        );
                        self.calls.lock().unwrap().push(format!("probe:{seat}"));
                        out.output = self.answer.lock().unwrap().clone();
                    }
                    "validator" => {
                        self.calls.lock().unwrap().push(format!("judge:{seat}"));
                        out.output = "PASS\nfine\nPASS".into();
                    }
                    _ => {
                        std::thread::sleep(std::time::Duration::from_millis(
                            *self.work_ms.lock().unwrap(),
                        ));
                        if let Some(wd) = &input.workdir {
                            std::fs::write(wd.join("note.txt"), "changed\n").unwrap();
                        }
                    }
                }
                out
            }
        }
        fn sh(cwd: &std::path::Path, args: &[&str]) {
            // spawn-audit: test-only — a git fixture building the layout under test.
            let out = Command::new("git")
                .args(args)
                .current_dir(cwd)
                .output()
                .expect("git runs");
            assert!(out.status.success(), "git {args:?}: {out:?}");
        }
        let base = std::env::temp_dir().join(format!("wicked-core-reprobe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        sh(&repo, &["init", "-q", "."]);
        sh(&repo, &["config", "user.email", "t@example.invalid"]);
        sh(&repo, &["config", "user.name", "t"]);
        sh(&repo, &["config", "commit.gpgsign", "false"]);
        sh(&repo, &["config", "core.autocrlf", "false"]);
        std::fs::write(repo.join("README.md"), "base\n").unwrap();
        sh(&repo, &["add", "-A"]);
        sh(&repo, &["commit", "-qm", "base"]);
        let wt = base.join("wt");
        sh(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                wt.to_str().unwrap(),
                "-b",
                "wicked/reprobe",
            ],
        );
        let input_for = |wt: &std::path::Path| {
            let _ = std::fs::remove_file(wt.join("note.txt"));
            let mut unit = crate::domain::WorkUnit::pending("r:u1", "r", 1, "Add a note");
            unit.assigned_cli = Some("claude".into());
            unit.default_floor = true;
            unit.worktree_baseline = Some(crate::worktree_guard::snapshot(wt, &repo).unwrap());
            StepInput {
                run_id: "r".into(),
                unit_ix: 0,
                attempt: 0,
                unit,
                workflow_id: "wf-r".into(),
                entity_mode: EntityMode::Isolated,
                workdir: Some(wt.to_path_buf()),
                governance: None,
                prior_outputs: vec![],
                elicitation_epoch: 0,
                process_gen: None,
                launch_seq: 0,
                required_skills: Vec::new(),
            }
        };
        // claude built the work; copilot is the run's only other seat, benched for quota.
        let registry = [
            seat("claude", "claude -p {PROMPT}"),
            seat("copilot", "copilot -p {PROMPT}"),
        ];
        let run_roster: Vec<String> = vec!["claude".into(), "copilot".into()];
        let quota = |cli: &str, until: i64| crate::domain::BenchedSeat {
            cli: cli.into(),
            reason: "quota_exhausted (no success in the run)".into(),
            source: "worker".into(),
            governed_only: false,
            benched_until: Some(until),
            strikes: 1,
            replaced: None,
        };
        let seats = Arc::new(Seats {
            answer: Mutex::new(
                "Error: You have exceeded your monthly quota (Request ID: D7DE:0)".into(),
            ),
            work_ms: Mutex::new(0),
            calls: Mutex::new(Vec::new()),
        });
        let runner: Arc<dyn StepRunner> = seats.clone();
        let judge = |bench: &[crate::domain::BenchedSeat], roster: &[String]| {
            seats.calls.lock().unwrap().clear();
            let input = input_for(&wt);
            let (out, verdict, evidence) =
                run_unit_and_judge_reprobing(&runner, &input, &registry, roster, bench);
            let mut after = bench.to_vec();
            crate::seat_bench::apply_reprobes(&mut after, &evidence.bench_reprobes);
            (out, verdict, evidence, after)
        };

        // 1. Not due yet: no probe runs, and the gate has no judge (the seat failure the operator
        //    used to answer by judging the work himself).
        let not_due = quota("copilot", crate::seat_bench::now_ms() + 30 * MIN);
        let (_, v, ev, after) = judge(std::slice::from_ref(&not_due), &run_roster);
        assert!(ev.bench_reprobes.is_empty() && after == vec![not_due.clone()]);
        let v = v.expect("a seat-failure verdict");
        assert!(
            !v.pass && v.judge_cli.is_none() && v.seat_failure.is_some(),
            "{v:?}"
        );
        assert!(
            !seats
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c.contains("copilot")),
            "a bench that is not due is never probed or drawn"
        );

        // 2. Due, and the re-probe is refused again: re-benched with the next backoff (30 min),
        //    still no judge.
        let due = quota("copilot", crate::seat_bench::now_ms() - 1);
        let t0 = crate::seat_bench::now_ms();
        let (_, v, ev, after) = judge(std::slice::from_ref(&due), &run_roster);
        assert_eq!(
            *seats.calls.lock().unwrap(),
            vec!["probe:copilot".to_string()]
        );
        let r = &ev.bench_reprobes[0];
        assert!(
            !r.returned && r.prior_strikes == 1 && r.strikes == 2,
            "{r:?}"
        );
        let until = r.benched_until.expect("re-benched");
        assert!(until >= t0 + 30 * MIN && until <= crate::seat_bench::now_ms() + 30 * MIN);
        assert_eq!((after[0].strikes, after[0].benched_until), (2, Some(until)));
        assert!(v.expect("seat failure").judge_cli.is_none());

        // 3. Anything but the word is not an answer: "not ready" (exit 0) stays benched.
        *seats.answer.lock().unwrap() = "not ready".into();
        let (_, _, ev, _) = judge(std::slice::from_ref(&due), &run_roster);
        assert!(!ev.bench_reprobes[0].returned, "{:?}", ev.bench_reprobes);

        // 4. Due, and the seat answers: back on the roster, and THIS gate's judge is copilot.
        *seats.answer.lock().unwrap() = "**READY.**".into();
        let (out, v, ev, after) = judge(std::slice::from_ref(&due), &run_roster);
        assert_eq!(out.status, StepStatus::Ok);
        assert!(
            after.is_empty(),
            "the returned seat is no longer benched: {after:?}"
        );
        assert!(ev.bench_reprobes[0].returned);
        let v = v.expect("a judge ran");
        assert!(v.pass, "{v:?}");
        assert_eq!(
            v.judge_cli.as_deref(),
            Some("copilot"),
            "the returned seat judges"
        );
        assert_eq!(
            *seats.calls.lock().unwrap(),
            vec!["probe:copilot".to_string(), "judge:copilot".to_string()],
            "probed once, then seated as the judge"
        );

        // 5. A bench that falls due WHILE the unit's own work runs is probed before the judge
        //    reads the bench, not left out until the next unit.
        *seats.work_ms.lock().unwrap() = 1_500;
        let soon = quota("copilot", crate::seat_bench::now_ms() + 700);
        let (_, v, ev, _) = judge(std::slice::from_ref(&soon), &run_roster);
        assert!(
            ev.bench_reprobes.first().is_some_and(|r| r.returned),
            "{ev:?}"
        );
        assert_eq!(
            v.expect("a judge ran").judge_cli.as_deref(),
            Some("copilot")
        );
        *seats.work_ms.lock().unwrap() = 0;

        // 6. An instance bench probes THAT instance (its own configuration home), not the
        //    registry record's primary key.
        let roster2: Vec<String> = vec!["claude".into(), "copilot#2".into()];
        let instance = quota("copilot#2", crate::seat_bench::now_ms() - 1);
        let (_, _, ev, _) = judge(std::slice::from_ref(&instance), &roster2);
        assert_eq!(ev.bench_reprobes[0].seat, "copilot#2");
        assert!(
            seats.calls.lock().unwrap().first().map(String::as_str) == Some("probe:copilot#2"),
            "{:?}",
            seats.calls.lock().unwrap()
        );

        // 7. The re-judge readiness counts a due seat as a candidate (its attempt re-probes it),
        //    and not one that is still out.
        let mut cursor = crate::domain::WorkUnit::pending("r:u1", "r", 1, "Add a note");
        cursor.assigned_cli = Some("claude".into());
        let (eligible, _) = rejudge_seats(
            &cursor,
            &registry,
            &run_roster,
            std::slice::from_ref(&due),
            &[],
            Some(&[]),
            &[],
            &registry,
        );
        assert_eq!(eligible, vec!["copilot".to_string()]);
        let (eligible, _) = rejudge_seats(
            &cursor,
            &registry,
            &run_roster,
            std::slice::from_ref(&not_due),
            &[],
            Some(&[]),
            &[],
            &registry,
        );
        assert!(
            eligible.is_empty(),
            "a bench that is not due keeps the seat out"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_default_judge_uses_the_run_roster_minus_the_bench_and_reports_auth_refusals() {
        use crate::workflow::{StepOutput, StepRunner};
        use std::process::Command;
        use std::sync::Mutex;

        /// The unit writes a file (the tree changes); judge sessions record the seat they were
        /// dispatched under — `dead` refuses as signed out, every other seat answers PASS.
        struct Seats {
            judged_on: Mutex<Vec<String>>,
        }
        impl StepRunner for Seats {
            fn run_unit(&self, input: &StepInput) -> StepOutput {
                let mut out = StepOutput {
                    run_id: input.run_id.clone(),
                    unit_ix: input.unit_ix,
                    attempt: input.attempt,
                    output: "did".into(),
                    status: StepStatus::Ok,
                    usage: None,
                    files: Vec::new(),
                    tools: Vec::new(),
                    governed: false,
                };
                if input.unit.session_id == "validator" {
                    let seat = input.unit.assigned_cli.clone().unwrap_or_default();
                    self.judged_on.lock().unwrap().push(seat.clone());
                    if seat == "dead" {
                        out.status = StepStatus::Failed;
                        out.output =
                            "(cli `dead` exited 1) Not logged in · Please run /login".into();
                    } else if seat == "empty" {
                        // (core#772) Exit 0, nothing printed.
                        out.output = String::new();
                    } else if seat == "quota" {
                        // (core#772) Exit 0 + the provider's refusal: the captured copilot shape.
                        out.output =
                            "Error: You have exceeded your monthly quota (Request ID: D7DE:0)"
                                .into();
                    } else {
                        out.output = "PASS\nfine\nPASS".into();
                    }
                } else if let Some(wd) = &input.workdir {
                    std::fs::write(wd.join("note.txt"), "changed\n").unwrap();
                }
                out
            }
        }
        fn sh(cwd: &std::path::Path, args: &[&str]) {
            // spawn-audit: test-only — a git fixture building the layout under test.
            let out = Command::new("git")
                .args(args)
                .current_dir(cwd)
                .output()
                .expect("git runs");
            assert!(out.status.success(), "git {args:?}: {out:?}");
        }
        let base =
            std::env::temp_dir().join(format!("wicked-core-judge-roster-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        sh(&repo, &["init", "-q", "."]);
        sh(&repo, &["config", "user.email", "t@example.invalid"]);
        sh(&repo, &["config", "user.name", "t"]);
        sh(&repo, &["config", "commit.gpgsign", "false"]);
        sh(&repo, &["config", "core.autocrlf", "false"]);
        std::fs::write(repo.join("README.md"), "base\n").unwrap();
        sh(&repo, &["add", "-A"]);
        sh(&repo, &["commit", "-qm", "base"]);
        let wt = base.join("wt");
        sh(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                wt.to_str().unwrap(),
                "-b",
                "wicked/judge-roster",
            ],
        );

        let mut unit = crate::domain::WorkUnit::pending("r:u1", "r", 1, "Add a note");
        unit.assigned_cli = Some("creator".into());
        unit.default_floor = true;
        unit.worktree_baseline = Some(crate::worktree_guard::snapshot(&wt, &repo).unwrap());
        let input = StepInput {
            run_id: "r".into(),
            unit_ix: 0,
            attempt: 0,
            unit,
            workflow_id: "wf-r".into(),
            entity_mode: EntityMode::Isolated,
            workdir: Some(wt.clone()),
            governance: None,
            prior_outputs: vec![],
            elicitation_epoch: 0,
            process_gen: None,
            launch_seq: 0,
            required_skills: Vec::new(),
        };
        let registry = [
            seat("creator", "creator -p {PROMPT}"),
            seat("dead", "dead -p {PROMPT}"),
            seat("quota", "quota -p {PROMPT}"), // (core#772) runs, exits 0, prints a quota refusal
            seat("empty", "empty -p {PROMPT}"), // (core#772) runs, exits 0, prints nothing
            seat("alive", "alive -p {PROMPT}"),
            seat("stranger", "stranger -p {PROMPT}"), // in the registry, NOT on this run's roster
        ];
        let seats = Arc::new(Seats {
            judged_on: Mutex::new(Vec::new()),
        });
        let runner: Arc<dyn StepRunner> = seats.clone();
        let noop: &DeltaSink = &|_| {};
        let run_roster: Vec<String> = ["creator", "dead", "quota", "alive"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (out, verdict, evidence) = run_unit_and_judge_with_roster(
            &runner,
            &input,
            None,
            noop,
            &registry,
            &run_roster,
            &[],
        );
        assert_eq!(out.status, StepStatus::Ok);
        assert_eq!(
            evidence.tree_changed,
            Some(true),
            "the unit changed its tree"
        );
        let verdict =
            verdict.expect("a default judge ran: the tree changed and a distinct seat exists");
        assert!(verdict.pass);
        assert_eq!(
            verdict.judge_cli.as_deref(),
            Some("alive"),
            "the judge rotated past the dead seat AND the quota-refusing seat onto the run's other seat"
        );
        assert!(
            verdict.seat_failure.is_none(),
            "a seat judged, so no seat-failure marker"
        );
        let judged_on = seats.judged_on.lock().unwrap().clone();
        assert!(
            !judged_on.iter().any(|s| s == "stranger"),
            "a registry seat the run never configured is not a judge: {judged_on:?}"
        );
        assert!(
            !judged_on.iter().any(|s| s == "creator"),
            "evaluator ≠ creator: {judged_on:?}"
        );
        assert_eq!(
            evidence.judge_auth_refusals,
            vec!["dead".to_string()],
            "the signed-out judge seat is reported for the bench"
        );
        // (F-7R3-001) …and, with its cause, on the carrier the fold benches from — (core#772) the
        // seat that answered with a quota refusal beside it, classified from the same text the
        // worker path benches on, so the fold benches both and the next unit draws neither.
        assert_eq!(
            evidence.judge_refusals,
            vec![
                crate::workflow::JudgeRefusal {
                    seat: "dead".to_string(),
                    reason: "not_logged_in".to_string(),
                    reset_at: None,
                },
                crate::workflow::JudgeRefusal {
                    seat: "quota".to_string(),
                    reason: "quota_exhausted".to_string(),
                    reset_at: None,
                },
            ]
        );

        // With `dead` benched, the next unit never dispatches to it.
        seats.judged_on.lock().unwrap().clear();
        let mut unit2 = input.unit.clone();
        std::fs::remove_file(wt.join("note.txt")).unwrap();
        unit2.worktree_baseline = Some(crate::worktree_guard::snapshot(&wt, &repo).unwrap());
        let input2 = StepInput {
            unit: unit2,
            ..input.clone()
        };
        let (_, verdict2, evidence2) = run_unit_and_judge_with_roster(
            &runner,
            &input2,
            None,
            noop,
            &registry,
            &run_roster,
            &["dead".to_string(), "quota".to_string()],
        );
        assert_eq!(verdict2.unwrap().judge_cli.as_deref(), Some("alive"));
        assert!(
            evidence2.judge_refusals.is_empty()
                && !seats
                    .judged_on
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|s| s == "dead" || s == "quota"),
            "a benched judge seat is never tried again"
        );

        // (core#772) EVERY eligible judge seat failing as a seat: the gate still denies
        // (fail-closed) but as a SEAT FAILURE — no judge named, `seat_failure` set, the reasoning
        // names the seats and never a verdict word — and every seat is reported for the bench.
        seats.judged_on.lock().unwrap().clear();
        let mut unit3 = input.unit.clone();
        std::fs::remove_file(wt.join("note.txt")).unwrap();
        unit3.worktree_baseline = Some(crate::worktree_guard::snapshot(&wt, &repo).unwrap());
        let input3 = StepInput {
            unit: unit3,
            ..input.clone()
        };
        let only_failing: Vec<String> = ["creator", "dead", "quota", "empty"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (_, verdict3, evidence3) = run_unit_and_judge_with_roster(
            &runner,
            &input3,
            None,
            noop,
            &registry,
            &only_failing,
            &[],
        );
        let v3 = verdict3.expect("the default judge was convened (distinct seats existed)");
        assert!(
            !v3.pass && v3.judge_cli.is_none(),
            "fail-closed, nobody judged: {v3:?}"
        );
        let failures = v3
            .seat_failure
            .as_deref()
            .expect("a seat failure, not a verdict");
        assert!(
            failures.contains("quota (exhausted its quota") && failures.contains("dead ("),
            "{failures}"
        );
        assert!(!v3.reasoning.contains("REJECT"), "{}", v3.reasoning);
        let benched3: Vec<&str> = evidence3
            .judge_refusals
            .iter()
            .map(|r| r.seat.as_str())
            .collect();
        assert_eq!(
            benched3,
            vec!["dead", "quota", "empty"],
            "every failed seat reported for the bench"
        );
        assert_eq!(
            evidence3.judge_refusals[2].reason,
            crate::validator::JUDGE_EMPTY_ANSWER_REASON,
            "an empty answer is benched under its own token"
        );

        // (core#772, codex r1) The RETRY with those seats benched: no distinct judge remains on
        // the unbenched roster, but one existed before the bench. That is still a SEAT FAILURE
        // (the human gate opens) — never a skipped judge, which would pass the retry on the
        // deterministic floor alone.
        // The registry here holds only the run's own seats: no outside seat can judge either.
        seats.judged_on.lock().unwrap().clear();
        let benched_all = ["dead".to_string(), "quota".to_string(), "empty".to_string()];
        let run_only: Vec<crate::AgenticCli> = registry
            .iter()
            .filter(|c| only_failing.contains(&c.key))
            .cloned()
            .collect();
        let (_, verdict4, evidence4) = run_unit_and_judge_with_roster(
            &runner,
            &input3,
            None,
            noop,
            &run_only,
            &only_failing,
            &benched_all,
        );
        let v4 = verdict4.expect("a benched-out judge is a seat-failure verdict, not a skip");
        assert!(!v4.pass && v4.judge_cli.is_none(), "{v4:?}");
        let f4 = v4.seat_failure.as_deref().expect("seat failure");
        assert!(
            f4.contains("dead (benched") && f4.contains("quota (benched"),
            "{f4}"
        );
        assert!(
            seats.judged_on.lock().unwrap().is_empty() && evidence4.judge_refusals.is_empty(),
            "no benched seat is re-drawn"
        );

        // (crew main red 2026-10-11, crew run 38124235121) The DEFAULT judge (no pinned validator)
        // never walks seats the run did not configure: with the FULL registry and every
        // configured distinct seat benched, the retry is still the seat failure above — an
        // outside seat (on CI, an unauthenticated codex) must not turn the floor's answer into a
        // human pause. Only a PINNED validator's required judge reaches outside seats (bad4d9df).
        seats.judged_on.lock().unwrap().clear();
        let (_, verdict5, _) = run_unit_and_judge_with_roster(
            &runner,
            &input3,
            None,
            noop,
            &registry,
            &only_failing,
            &benched_all,
        );
        let v5 = verdict5.expect("the same seat-failure verdict as with the run-only registry");
        assert!(!v5.pass && v5.judge_cli.is_none(), "{v5:?}");
        let judged5 = seats.judged_on.lock().unwrap().clone();
        assert!(
            !judged5.iter().any(|s| s == "alive" || s == "stranger"),
            "the default judge never reaches a seat the run did not configure: {judged5:?}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// (crew main red 2026-10-11, crew run 38124235121) When does an all-refused OUTSIDE judge fall
    /// back to the pre-#893 skip? Only when the run's own unbenched seats never had a distinct
    /// judge: the crew smoke's one-seat run (creator `copilot`, validator author `claude`) does;
    /// the core#772 retry (distinct run seats that were BENCHED) does not — that stays a seat
    /// failure, never a skip.
    #[test]
    fn an_all_refused_outside_judge_is_a_skip_only_when_the_run_never_had_a_distinct_judge() {
        let copilot = seat("copilot", "node floor-seat-cli.cjs {PROMPT}");
        let parties = [crate::validator::DETERMINISTIC_VALIDATOR_SEAT, "copilot"];
        assert!(outside_seats_only(&parties, std::slice::from_ref(&copilot)));
        // The run's own pre-bench pool held distinct seats (`dead`, `quota`): not outside-only.
        let run_unbenched = [
            seat("creator", "creator -p {PROMPT}"),
            seat("dead", "dead -p {PROMPT}"),
            seat("quota", "quota -p {PROMPT}"),
        ];
        assert!(!outside_seats_only(&["creator"], &run_unbenched));
    }

    /// (operator ruling 2026-10-11) opencode is OPT-IN: never drawn from outside the run's roster as a
    /// judge; a run that picked it still judges on it.
    #[test]
    fn an_opt_in_seat_judges_only_when_the_run_picked_it() {
        let registry = [
            seat("claude", "claude -p {PROMPT}"),
            seat("pi", "pi -p {PROMPT}"),
            seat("opencode", "opencode run {PROMPT}"),
            seat("codex", "codex exec {PROMPT}"),
        ];
        let run: Vec<String> = vec!["claude".into(), "pi".into()];
        let pool: Vec<&str> = judge_pool(&registry, &run)
            .iter()
            .map(|c| c.key.as_str())
            .collect();
        assert_eq!(
            pool,
            vec!["claude", "pi"],
            "opencode is not a default judge, and (core#937) no seat outside the run judges it"
        );
        let picked: Vec<String> = vec!["claude".into(), "opencode".into()];
        let pool2: Vec<&str> = judge_pool(&registry, &picked)
            .iter()
            .map(|c| c.key.as_str())
            .collect();
        assert!(pool2.contains(&"opencode"), "{pool2:?}");
    }

    /// Editor E2E run bad4d9df (coordinator, 2026-10-11): the run seated claude, pi and opencode;
    /// pi created the work, claude authored the validator, opencode monitored — every seated
    /// identity is a party. (core#937) The judge pool is the RUN's seats only: with every one of
    /// them a party it comes back EMPTY — the gate's explicit `judge_unavailable` / judge-skipped
    /// path, where the operator decides — never a party, and never codex or copilot, which the
    /// launch did not seat (run 0d2b9fa4: copilot was deselected in the wizard, judged anyway,
    /// and its CLI rejected the flags it was invoked with).
    #[test]
    fn core_937_a_run_whose_seats_are_all_parties_gets_no_judge_from_outside_its_pool() {
        let registry = [
            seat("claude", "claude -p {PROMPT}"),
            seat("copilot", "copilot -p {PROMPT}"),
            seat("codex", "codex exec {PROMPT}"),
            seat("pi", "pi -p {PROMPT}"),
            seat("opencode", "opencode run {PROMPT}"),
        ];
        let run_roster: Vec<String> = ["claude", "pi", "opencode"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let benched = ["copilot".to_string()];
        let parties = [
            crate::validator::DETERMINISTIC_VALIDATOR_SEAT, // claude, the validator's author
            "pi",                                           // the creator
            "opencode",                                     // the monitor
        ];
        let pool_for = |registry: &[crate::AgenticCli]| -> Vec<String> {
            let eligible: Vec<crate::AgenticCli> = judge_pool(registry, &run_roster)
                .into_iter()
                .filter(|c| !benched.contains(&c.key))
                .cloned()
                .collect();
            let configured: Vec<crate::AgenticCli> = eligible
                .iter()
                .filter(|c| run_roster.contains(&c.key))
                .cloned()
                .collect();
            let roster = judge_roster(&parties, &configured, &eligible);
            crate::validator::distinct_judge_keys(&parties, roster)
        };
        assert!(
            pool_for(&registry).is_empty(),
            "no outside seat judges a configured run: {:?}",
            pool_for(&registry)
        );
        // 0d2b9fa4's pool: [codex (benched for the worktree), claude (creator + validator
        // author), pi (the unit's own seat)]; copilot is in the registry, not the run.
        let run_0d2b: Vec<String> = ["codex", "claude", "pi"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let pool_0d2b: Vec<&str> = judge_pool(&registry, &run_0d2b)
            .iter()
            .map(|c| c.key.as_str())
            .collect();
        assert_eq!(pool_0d2b, vec!["claude", "codex", "pi"], "registry order");
        assert!(!pool_0d2b.contains(&"copilot"));
        // (codex r1) An instance-only roster configures its CLIs' seats, never the registry.
        let instances: Vec<String> = vec!["claude#2".into(), "pi#2".into()];
        let pool_inst: Vec<&str> = judge_pool(&registry, &instances)
            .iter()
            .map(|c| c.key.as_str())
            .collect();
        assert_eq!(pool_inst, vec!["claude", "pi"], "{pool_inst:?}");
        // (codex r2) …and an instance author excludes its CLI's seat from judging its work.
        let inst_pool: Vec<crate::AgenticCli> = judge_pool(&registry, &instances)
            .into_iter()
            .cloned()
            .collect();
        let judges = crate::validator::distinct_judge_keys(
            &[crate::validator::DETERMINISTIC_VALIDATOR_SEAT, "pi#2"],
            &inst_pool,
        );
        assert!(judges.is_empty(), "pi never judges pi#2's work: {judges:?}");
        // A configured distinct seat always wins over an outside one (review RT-1).
        let configured_ok: Vec<String> = ["claude", "pi", "opencode"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let eligible: Vec<crate::AgenticCli> = judge_pool(&registry, &configured_ok)
            .into_iter()
            .cloned()
            .collect();
        let configured: Vec<crate::AgenticCli> = eligible
            .iter()
            .filter(|c| configured_ok.contains(&c.key))
            .cloned()
            .collect();
        let picked = crate::validator::distinct_judge_keys(
            &["pi"],
            judge_roster(&["pi"], &configured, &eligible),
        );
        assert!(
            !picked.is_empty() && picked.iter().all(|k| k != "codex"),
            "{picked:?}"
        );
        // (codex r1) A BENCHED creator alias `writer` that invokes codex: its record leaves the
        // pool, but its identity must still exclude the outside `codex` seat.
        let reg = [
            seat("writer", "codex exec {PROMPT}"),
            seat("codex", "codex exec {PROMPT}"),
            seat("pi", "pi -p {PROMPT}"),
        ];
        let run: Vec<String> = vec!["writer".into(), "pi".into()];
        let bench = ["writer".to_string()];
        let eligible: Vec<crate::AgenticCli> = judge_pool(&reg, &run)
            .into_iter()
            .filter(|c| !bench.contains(&c.key))
            .cloned()
            .collect();
        let configured: Vec<crate::AgenticCli> = eligible
            .iter()
            .filter(|c| run.contains(&c.key))
            .cloned()
            .collect();
        let excluded: Vec<&str> = vec!["writer", "pi"];
        let ids = author_identities(&excluded, &reg);
        let keys = crate::validator::distinct_judge_keys(
            &excluded,
            judge_roster(
                &excluded,
                &without_author_binaries(&configured, &excluded, &ids),
                &without_author_binaries(&eligible, &excluded, &ids),
            ),
        );
        assert!(
            keys.is_empty(),
            "the creator's binary never judges: {keys:?}"
        );

        // (codex r2) A registry KEY that collides with an author's identity (`codex` invokes pi)
        // must not redirect the exclusion: `codex-secondary` invokes the benched creator's binary.
        let reg2 = [
            seat("writer", "codex exec {PROMPT}"),
            seat("codex", "pi -p {PROMPT}"),
            seat("codex-secondary", "codex exec {PROMPT}"),
            seat("pi", "pi -p {PROMPT}"),
        ];
        let eligible2: Vec<crate::AgenticCli> = judge_pool(&reg2, &run)
            .into_iter()
            .filter(|c| !bench.contains(&c.key))
            .cloned()
            .collect();
        let configured2: Vec<crate::AgenticCli> = eligible2
            .iter()
            .filter(|c| run.contains(&c.key))
            .cloned()
            .collect();
        let ids2 = author_identities(&excluded, &reg2);
        let keys2 = crate::validator::distinct_judge_keys(
            &excluded,
            judge_roster(
                &excluded,
                &without_author_binaries(&configured2, &excluded, &ids2),
                &without_author_binaries(&eligible2, &excluded, &ids2),
            ),
        );
        assert!(
            !keys2.iter().any(|k| k == "codex-secondary"),
            "the creator's binary never judges: {keys2:?}"
        );
    }
}

/// crew #581 / F-BM-010 — the repo-checks floor heartbeats on the unit's live-output sink.
#[cfg(test)]
mod floor_heartbeat_tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    type Seen = Arc<Mutex<Vec<(Duration, String)>>>;

    fn recording_sink(seen: &Seen, t0: Instant) -> impl Fn(&str) + Send + Sync {
        let seen = Arc::clone(seen);
        move |s: &str| {
            seen.lock()
                .unwrap_or_else(|p| p.into_inner())
                .push((t0.elapsed(), s.to_string()))
        }
    }

    /// Design test (7): `every = 300 ms` around a 1.2 s closure → a delta at t = 0 and then one
    /// roughly every 300 ms, none after the closure returned, the closure's result passed through.
    #[test]
    fn the_floor_heartbeat_ticks_on_the_sink_while_the_work_runs_and_stops_when_it_returns() {
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let t0 = Instant::now();
        let sink = recording_sink(&seen, t0);
        let label = "repo checks floor (verify): running the repository's own checks";
        let r = with_floor_heartbeat(&sink, label, Duration::from_millis(300), || {
            std::thread::sleep(Duration::from_millis(1200));
            42
        });
        assert_eq!(r, 42, "the work's result passes through");
        let at_return = seen.lock().unwrap().clone();
        std::thread::sleep(Duration::from_millis(500));
        let after = seen.lock().unwrap().clone();
        assert_eq!(
            after.len(),
            at_return.len(),
            "no heartbeat after the work returned (the thread is joined by the scope)"
        );
        // (core#969) An engine note is on a line of its own ([`engine_note`]).
        assert_eq!(after[0].1, format!("\n{label} — 0 min\n"), "{after:?}");
        assert!(
            after[0].0 < Duration::from_millis(200),
            "the first heartbeat is immediate: {after:?}"
        );
        // 0, ~300, ~600, ~900 (and possibly ~1200 racing the return). A loaded host may wake a
        // tick late, so the floor is 3 frames, the ceiling 6.
        assert!(
            (3..=6).contains(&after.len()),
            "one heartbeat per `every` over 1.2 s: {after:?}"
        );
        assert!(
            after
                .iter()
                .all(|(_, s)| s.starts_with(&format!("\n{label} — ")) && s.ends_with('\n')),
            "{after:?}"
        );
        for w in after.windows(2) {
            assert!(
                w[1].0 >= w[0].0 + Duration::from_millis(250),
                "ticks are spaced by `every`, not a busy loop: {after:?}"
            );
        }
    }

    /// Review on #512: a floor that PANICS must not leave the ticker spinning with false liveness.
    /// `stop_tx` is dropped by the unwinding closure without a send → `Disconnected` → the ticker
    /// exits, the scope joins it, the panic propagates — promptly, with no further heartbeat.
    #[test]
    fn a_panicking_floor_stops_the_heartbeat_and_propagates() {
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let t0 = Instant::now();
        let sink = recording_sink(&seen, t0);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            with_floor_heartbeat(&sink, "floor", Duration::from_millis(50), || -> u8 {
                std::thread::sleep(Duration::from_millis(120));
                panic!("the floor blew up");
            })
        }));
        assert!(result.is_err(), "the panic propagates through the scope");
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "no hang: the ticker exited on Disconnected"
        );
        let at_return = seen.lock().unwrap_or_else(|p| p.into_inner()).len();
        std::thread::sleep(Duration::from_millis(300));
        let after = seen.lock().unwrap_or_else(|p| p.into_inner()).len();
        assert_eq!(after, at_return, "no heartbeat after the panic");
    }

    /// Rig run b6f7968b: the heartbeat names the pass the floor is in, so a floor that runs the
    /// suite on the change, the base and the change again does not read as two silent hours.
    #[test]
    fn the_floor_heartbeat_names_the_pass_in_progress() {
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let t0 = Instant::now();
        let sink = recording_sink(&seen, t0);
        with_floor_heartbeat(&sink, "floor", Duration::from_millis(100), || {
            crate::repo_checks::note_pass("test", crate::repo_checks::Tree::Head);
            std::thread::sleep(Duration::from_millis(350));
            crate::repo_checks::note_pass("test", crate::repo_checks::Tree::Base);
            std::thread::sleep(Duration::from_millis(350));
        });
        let lines: Vec<String> = seen
            .lock()
            .unwrap()
            .iter()
            .map(|(_, s)| s.clone())
            .collect();
        assert_eq!(
            lines[0], "\nfloor — 0 min\n",
            "the start beat precedes any pass: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("; now `test` on the change (pass 1 of `test`)")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("; now `test` on the base (pass 2 of `test`)")),
            "{lines:?}"
        );
    }

    /// Work that returns at once emits exactly the start heartbeat, and the call returns
    /// promptly — the stop channel wakes the ticker; the scope never waits out `every`.
    #[test]
    fn a_short_floor_emits_exactly_the_start_heartbeat_and_returns_at_once() {
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let t0 = Instant::now();
        let sink = recording_sink(&seen, t0);
        let r = with_floor_heartbeat(&sink, "floor", FLOOR_HEARTBEAT_EVERY, || "done");
        assert_eq!(r, "done");
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "returned without waiting out the 5 min cadence"
        );
        let seen = seen.lock().unwrap().clone();
        assert_eq!(
            seen.iter().map(|(_, s)| s.as_str()).collect::<Vec<_>>(),
            vec!["\nfloor — 0 min\n"]
        );
    }
}

/// DES-TEAMING-002 T0 (amended) — the gate judge's BUS path is exec mediation's, not the bus
/// handoff's. Crew now hands every engine `WICKED_BUS_DB`; before this seam that variable ALONE
/// rerouted every pinned judge to `wicked.gate.eval.requested` and a 180 s fail-closed wait for an
/// evaluator daemon no product runs. The judge must take the bus only when `WICKED_BUS_EXEC` is on.
#[cfg(test)]
mod judge_routing_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn seat(key: &str, invocation: &str) -> crate::AgenticCli {
        use wicked_council::{Category, Confidence, InputMode};
        crate::AgenticCli {
            key: key.into(),
            display_name: key.into(),
            binary: "unused".into(),
            headless_invocation: invocation.into(),
            category: Category::default(),
            input_mode: InputMode::default(),
            version_probe: vec![],
            trust_flags: vec![],
            alt_binaries: vec![],
            confidence: Confidence::default(),
            enabled_for_council: true,
            seat_eligible_for_work: true,
            acp: None,
            capabilities: None,
            login_invocation: None,
            logout_invocation: None,
            governance_class: None,
            credential: None,
            free_tier: None,
            health: None,
        }
    }

    struct OkRunner;
    impl StepRunner for OkRunner {
        fn run_unit(&self, input: &StepInput) -> StepOutput {
            StepOutput {
                run_id: input.run_id.clone(),
                unit_ix: input.unit_ix,
                attempt: input.attempt,
                output: "the work".into(),
                status: StepStatus::Ok,
                usage: None,
                files: Vec::new(),
                tools: Vec::new(),
                governed: false,
            }
        }
    }

    #[test]
    fn the_bus_judge_needs_an_armed_exec_bus() {
        assert_eq!(gate_eval_bus_db(None), None);
        assert_eq!(gate_eval_bus_db(Some("")), None);
        assert_eq!(
            gate_eval_bus_db(Some("/b/bus.db")),
            Some("/b/bus.db".to_string())
        );
    }

    /// Restores each named variable to what it was when captured, on drop.
    struct EnvRestore(Vec<(&'static str, Option<std::ffi::OsString>)>);
    impl EnvRestore {
        fn capture(keys: &[&'static str]) -> Self {
            Self(keys.iter().map(|k| (*k, std::env::var_os(k))).collect())
        }
    }
    impl Drop for EnvRestore {
        fn drop(&mut self) {
            for (k, v) in &self.0 {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    #[test]
    fn bus_db_without_exec_keeps_the_inline_judge_for_an_evidence_floor_pinned_unit() {
        // The crate-wide env lock for the WHOLE scope (it mutates process env the judge reads);
        // declared first so it is released last, after `_restore` puts both variables back.
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let _restore = EnvRestore::capture(&["WICKED_BUS_DB", "WICKED_BUS_EXEC"]);
        let dir =
            std::env::temp_dir().join(format!("wicked-core-judge-routing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bus_path = dir.join("bus.db").to_string_lossy().to_string();

        // The default crew boot after T0: the engine HAS a bus, exec mediation is OFF.
        std::env::set_var("WICKED_BUS_DB", &bus_path);
        std::env::remove_var("WICKED_BUS_EXEC");

        // A stand-in evaluator daemon, so the pre-fix bus path fails this test in seconds, not
        // after the 180 s timeout: it answers every request it sees and counts them.
        let requests = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let responder = {
            let (requests, stop, bus_path) = (requests.clone(), stop.clone(), bus_path.clone());
            std::thread::spawn(move || {
                let db = BusDb::open(&bus_path).unwrap();
                let mut floor = 0;
                while !stop.load(Ordering::SeqCst) {
                    for ev in db.poll(GATE_EVAL_REQUESTED, floor, 20).unwrap_or_default() {
                        floor = ev.event_id;
                        requests.fetch_add(1, Ordering::SeqCst);
                        let eval_id = ev.payload["eval_id"].as_str().unwrap_or("").to_string();
                        let _ = db.emit(&BusEmit::new(
                            GATE_EVAL_RESPONDED,
                            "test",
                            "test.gate",
                            serde_json::json!({"eval_id": eval_id, "pass": true, "reasoning": "stand-in"}),
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            })
        };

        // The built-in Evaluator phases' floor: the approved evidence-floor validator.
        let mut unit = crate::domain::WorkUnit::pending("r:u1", "r", 1, "review the work");
        unit.assigned_cli = Some("pi".into());
        unit.validator = Some(crate::validator::DeterministicValidator {
            criterion: crate::builtin_floors::EVIDENCE_CRITERION.into(),
            script: crate::builtin_floors::EVIDENCE_SCRIPT.into(),
            approved: true,
        });
        let input = StepInput {
            run_id: "r".into(),
            unit_ix: 0,
            attempt: 0,
            unit,
            workflow_id: "wf-r".into(),
            entity_mode: EntityMode::Isolated,
            workdir: Some(dir.clone()),
            governance: None,
            prior_outputs: vec![],
            elicitation_epoch: 0,
            process_gen: None,
            launch_seq: 0,
            required_skills: Vec::new(),
        };
        // claude = the deterministic validator's author, pi = the creator: the inline judge has
        // no distinct seat and records WHY (judge_skipped) without spawning anything.
        let roster = vec![
            seat("claude", "claude -p {PROMPT}"),
            seat("pi", "pi ask {PROMPT}"),
        ];
        let runner: Arc<dyn StepRunner> = Arc::new(OkRunner);
        let noop: &DeltaSink = &|_: &str| {};
        let started = Instant::now();
        let (_out, _verdict, evidence) =
            run_unit_and_judge_with_roster(&runner, &input, None, noop, &roster, &[], &[]);
        let elapsed = started.elapsed();

        stop.store(true, Ordering::SeqCst);
        let _ = responder.join();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            requests.load(Ordering::SeqCst),
            0,
            "WICKED_BUS_DB without WICKED_BUS_EXEC must never publish wicked.gate.eval.requested"
        );
        assert!(
            evidence.judge_skipped.is_some(),
            "the INLINE judge ran (it records why no distinct seat judged); got {:?}",
            evidence.judge_skipped
        );
        assert!(
            elapsed < Duration::from_secs(30),
            "no bus wait: the judge returned in {elapsed:?}"
        );
    }

    /// Review finding (#613 round 3): with `WICKED_BUS_EXEC=1` and a `WICKED_BUS_DB` exec mediation
    /// cannot initialise, exec never arms and the unit runs on the in-process worker (no armed exec
    /// bus). Its judge must stay inline — re-reading process env would take the bus path and DENY on
    /// the infrastructure failure. The judge uses the bus ONLY when the caller passes an armed one.
    #[test]
    fn an_unarmed_exec_env_keeps_the_inline_judge() {
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let _restore = EnvRestore::capture(&["WICKED_BUS_DB", "WICKED_BUS_EXEC"]);
        let dir = std::env::temp_dir().join(format!(
            "wicked-core-unarmed-exec-judge-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let not_a_dir = dir.join("not-a-dir");
        std::fs::write(&not_a_dir, "a file where the bus directory should be").unwrap();
        std::env::set_var("WICKED_BUS_EXEC", "1");
        std::env::set_var(
            "WICKED_BUS_DB",
            not_a_dir.join("bus.db").to_string_lossy().to_string(),
        );

        let mut unit = crate::domain::WorkUnit::pending("ua:u1", "ua", 1, "review the work");
        unit.assigned_cli = Some("pi".into());
        unit.validator = Some(crate::validator::DeterministicValidator {
            criterion: crate::builtin_floors::EVIDENCE_CRITERION.into(),
            script: crate::builtin_floors::EVIDENCE_SCRIPT.into(),
            approved: true,
        });
        let input = StepInput {
            run_id: "ua".into(),
            unit_ix: 0,
            attempt: 0,
            unit,
            workflow_id: "wf-ua".into(),
            entity_mode: EntityMode::Isolated,
            workdir: Some(dir.clone()),
            governance: None,
            prior_outputs: vec![],
            elicitation_epoch: 0,
            process_gen: None,
            launch_seq: 0,
            required_skills: Vec::new(),
        };
        let roster = vec![
            seat("claude", "claude -p {PROMPT}"),
            seat("pi", "pi ask {PROMPT}"),
        ];
        let runner: Arc<dyn StepRunner> = Arc::new(OkRunner);
        let noop: &DeltaSink = &|_: &str| {};
        // The in-process worker's call: no armed exec bus (`armed_exec_bus()` is `None`).
        let (_out, verdict, evidence) =
            run_unit_and_judge_with_roster(&runner, &input, None, noop, &roster, &[], &[]);
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            verdict
                .as_ref()
                .is_none_or(|v| !v.reasoning.contains("bus-path")),
            "no bus-path verdict for an unarmed exec: {verdict:?}"
        );
        assert!(
            evidence.judge_skipped.is_some(),
            "the INLINE judge ran (it records why no distinct seat judged)"
        );
    }

    /// Review finding (#613 round 2): the env-free exec entry (`Core::spawn_with_engine_exec`) arms
    /// exec mediation over an explicit bus path with NO `WICKED_BUS_*` in the process env. Its
    /// cli-runner must judge a pinned unit over THAT bus (`gate.eval.requested`), not inline —
    /// the judge must follow the resolved exec bus, not only process env.
    #[test]
    fn the_env_free_exec_consumer_judges_a_pinned_unit_over_its_own_bus() {
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let _restore = EnvRestore::capture(&["WICKED_BUS_DB", "WICKED_BUS_EXEC"]);
        std::env::remove_var("WICKED_BUS_DB");
        std::env::remove_var("WICKED_BUS_EXEC");

        let dir = std::env::temp_dir().join(format!(
            "wicked-core-exec-judge-routing-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bus_path = dir.join("bus.db").to_string_lossy().to_string();

        /// Counts judge turns that reach the runner (the INLINE judge runs through it).
        struct CountingRunner(AtomicUsize);
        impl StepRunner for CountingRunner {
            fn run_unit(&self, input: &StepInput) -> StepOutput {
                if input.unit.session_id == "validator" {
                    self.0.fetch_add(1, Ordering::SeqCst);
                }
                StepOutput {
                    run_id: input.run_id.clone(),
                    unit_ix: input.unit_ix,
                    attempt: input.attempt,
                    output: "PASS\nok".into(),
                    status: StepStatus::Ok,
                    usage: None,
                    files: Vec::new(),
                    tools: Vec::new(),
                    governed: false,
                }
            }
        }

        let gen = uuid::Uuid::new_v4();
        let mut unit = crate::domain::WorkUnit::pending("xr:u1", "xr", 1, "review the work");
        unit.assigned_cli = Some("pi".into());
        unit.validator = Some(crate::validator::DeterministicValidator {
            criterion: crate::builtin_floors::EVIDENCE_CRITERION.into(),
            script: crate::builtin_floors::EVIDENCE_SCRIPT.into(),
            approved: true,
        });
        let input = StepInput {
            run_id: "xr".into(),
            unit_ix: 0,
            attempt: 0,
            unit,
            workflow_id: "wf-xr".into(),
            entity_mode: EntityMode::Isolated,
            workdir: Some(dir.clone()),
            governance: None,
            prior_outputs: vec![],
            elicitation_epoch: 0,
            process_gen: Some(gen),
            launch_seq: 1,
            required_skills: Vec::new(),
        };
        let bus = BusDb::shared(&bus_path).unwrap();
        arm_exec_publisher(bus.clone());
        assert!(
            try_publish_dispatched(&input, None, false),
            "task.dispatched published"
        );
        disarm_exec_publisher();

        // A stand-in evaluator daemon, so the bus judge returns at once when it is used.
        let requests = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let responder = {
            let (requests, stop, bus_path) = (requests.clone(), stop.clone(), bus_path.clone());
            std::thread::spawn(move || {
                let db = BusDb::open(&bus_path).unwrap();
                let mut floor = 0;
                while !stop.load(Ordering::SeqCst) {
                    for ev in db.poll(GATE_EVAL_REQUESTED, floor, 20).unwrap_or_default() {
                        floor = ev.event_id;
                        requests.fetch_add(1, Ordering::SeqCst);
                        let eval_id = ev.payload["eval_id"].as_str().unwrap_or("").to_string();
                        let _ = db.emit(&BusEmit::new(
                            GATE_EVAL_RESPONDED,
                            "test",
                            "test.gate",
                            serde_json::json!({"eval_id": eval_id, "pass": true, "reasoning": "stand-in"}),
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            })
        };

        let runner = Arc::new(CountingRunner(AtomicUsize::new(0)));
        let (cmd_tx, _cmd_rx) = std::sync::mpsc::channel();
        let runner_stop = Arc::new(AtomicBool::new(false));
        let consumer = run_cli_runner(
            bus.clone(),
            0,
            runner.clone(),
            cmd_tx,
            None,
            gen,
            None,
            consumer_name(gen),
            completed_consumer_name(gen),
            Duration::from_millis(50),
            runner_stop.clone(),
            None,
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        while bus
            .poll(TASK_COMPLETED, 0, 10)
            .unwrap_or_default()
            .is_empty()
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(50));
        }
        runner_stop.store(true, Ordering::SeqCst);
        let _ = consumer.join();
        stop.store(true, Ordering::SeqCst);
        let _ = responder.join();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "the exec consumer judged over its bus (one gate.eval.requested)"
        );
        assert_eq!(runner.0.load(Ordering::SeqCst), 0, "and not inline");
    }
}

/// DES-TEAMING-002 T5 (a), the BUS-WORKER path (`task.dispatched` → the cli-runner consumer →
/// the same worker-thread seam): `step.claimed` precedes every `checkpoint.reached` of the attempt.
#[cfg(test)]
mod t5_bus_worker_tests {
    use super::*;
    use crate::team::events as tev;
    use crate::team::publish::tests::{fixture, fixture_with, rig};
    use crate::team::publish::{PublishOutcome, TeamConfig};
    use crate::team::runner::TeamRunner;
    use std::time::Instant;

    /// Publishes two checkpoints of its own attempt during the turn, as the ACP carrier would.
    struct Carrier(crate::team::publish::TeamBus);
    impl StepRunner for Carrier {
        fn run_unit(&self, i: &StepInput) -> StepOutput {
            for seq in 1..=2u64 {
                let cp = fixture_with(tev::CHECKPOINT_REACHED, 0, &i.run_id, |p| {
                    p["ord"] = serde_json::json!(i.unit.ord);
                    p["attempt"] = serde_json::json!(i.attempt);
                    p["seq"] = serde_json::json!(seq);
                });
                self.0.publish(&cp).unwrap();
            }
            StepOutput {
                run_id: i.run_id.clone(),
                unit_ix: i.unit_ix,
                attempt: i.attempt,
                output: "done".into(),
                status: StepStatus::Ok,
                usage: None,
                files: Vec::new(),
                tools: Vec::new(),
                governed: false,
            }
        }
    }

    #[test]
    fn t5_a_step_claimed_precedes_every_checkpoint_on_the_bus_worker_path() {
        let rig = rig("t5a-bus");
        let run = "t5a-bus";
        let floor = match rig
            .team_bus()
            .publish(&fixture(tev::PATH_STARTED, 0, run))
            .unwrap()
        {
            PublishOutcome::Published(id) => id,
            o => panic!("{o:?}"),
        };
        let gen = uuid::Uuid::new_v4();
        let mut unit = crate::domain::WorkUnit::pending(format!("{run}:u1"), run, 1, "step 1");
        unit.assigned_cli = Some("claude#1".into());
        unit.team_run = true;
        let mut stamp = crate::domain::UnitTeamSnapshot::stamped(tev::Transport::Bus, None, None);
        stamp.stream_floor = Some(floor);
        unit.team = Some(stamp);
        let input = StepInput {
            run_id: run.into(),
            unit_ix: 0,
            attempt: 0,
            unit,
            workflow_id: format!("{run}:plan-1"),
            entity_mode: EntityMode::Isolated,
            workdir: None,
            governance: None,
            prior_outputs: vec![],
            elicitation_epoch: 0,
            process_gen: Some(gen),
            launch_seq: 1,
            required_skills: Vec::new(),
        };
        let bus = BusDb::shared(&rig.bus).unwrap();
        arm_exec_publisher(bus.clone());
        assert!(try_publish_dispatched(&input, None, false));
        disarm_exec_publisher();

        let team = TeamRunner::from_config(
            &TeamConfig::new(Some(rig.bus.clone()), Some(rig.outbox.clone()))
                .with_schedule(vec![Duration::from_millis(20); 3])
                .with_final_pass_budget(Duration::from_millis(300))
                .with_gate_poll(Duration::from_millis(20)),
        );
        let (cmd_tx, _cmd_rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let consumer = run_cli_runner(
            bus.clone(),
            0,
            Arc::new(Carrier(rig.team_bus())),
            cmd_tx,
            None,
            gen,
            None,
            consumer_name(gen),
            completed_consumer_name(gen),
            Duration::from_millis(20),
            stop.clone(),
            team,
        );
        let deadline = Instant::now() + Duration::from_secs(60);
        while bus
            .poll(TASK_COMPLETED, 0, 10)
            .unwrap_or_default()
            .is_empty()
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        stop.store(true, Ordering::SeqCst);
        let _ = consumer.join();
        let rows = rig.rows(run);
        let claimed: Vec<i64> = rows
            .iter()
            .filter(|(_, t, _)| t == tev::STEP_CLAIMED)
            .map(|(id, _, _)| *id)
            .collect();
        let checkpoints: Vec<i64> = rows
            .iter()
            .filter(|(_, t, _)| t == tev::CHECKPOINT_REACHED)
            .map(|(id, _, _)| *id)
            .collect();
        assert_eq!(claimed.len(), 1, "{rows:?}");
        assert_eq!(checkpoints.len(), 2, "{rows:?}");
        assert!(checkpoints.iter().all(|c| *c > claimed[0]), "{rows:?}");
        let completed = bus.poll(TASK_COMPLETED, 0, 10).unwrap();
        assert_eq!(completed.len(), 1);
        assert_eq!(
            completed[0].payload["evidence"]["team"]["claimed_event_id"],
            serde_json::json!(claimed[0]),
            "the snapshot rides task.completed to the actor"
        );
    }
}

/// (operator ruling 2026-10-14) The FALLBACK judge: when every preferred judge seat is benched for
/// the run (codex's worktree bench) or fails as a seat, a distinct fallback seat (Claude Code, then
/// Antigravity) judges in its place, disclosed; with none, the `judge_unavailable` hold stands.
#[cfg(test)]
mod judge_fallback_tests {
    use super::*;
    use crate::workflow::{StepOutput, StepRunner};
    use std::sync::Mutex;

    fn seat(key: &str, invocation: &str) -> crate::AgenticCli {
        use wicked_council::{Category, Confidence, InputMode};
        crate::AgenticCli {
            key: key.into(),
            display_name: key.into(),
            binary: wicked_apps_core::spawn::seat_cli_key(key).into(),
            headless_invocation: invocation.into(),
            category: Category::default(),
            input_mode: InputMode::default(),
            version_probe: vec![],
            trust_flags: vec![],
            alt_binaries: vec![],
            confidence: Confidence::default(),
            enabled_for_council: true,
            seat_eligible_for_work: true,
            acp: None,
            capabilities: None,
            login_invocation: None,
            logout_invocation: None,
            governance_class: None,
            credential: None,
            free_tier: None,
            health: None,
        }
    }

    /// The host registry 59232ebc ran against (crew roster on :7801): agy is not on it.
    fn registry() -> Vec<crate::AgenticCli> {
        vec![
            seat("claude", "claude -p {PROMPT}"),
            seat("codex", "codex exec {PROMPT}"),
            seat("pi", "pi -p {PROMPT}"),
            seat("copilot", "copilot -p {PROMPT}"),
            seat("opencode", "opencode run {PROMPT}"),
        ]
    }

    /// The fallback pool as production reads it: claude's record, and agy's built-in record —
    /// neither council- nor work-enabled.
    fn fallback_pool() -> Vec<crate::AgenticCli> {
        let mut agy = seat("agy", "agy -p {PROMPT}");
        agy.enabled_for_council = false;
        agy.seat_eligible_for_work = false;
        vec![seat("claude", "claude -p {PROMPT}"), agy]
    }

    fn worktree_bench() -> Vec<crate::domain::BenchedSeat> {
        vec![crate::domain::BenchedSeat {
            cli: "codex".into(),
            reason: crate::distribute::CODEX_WORKTREE_BENCH.into(),
            source: "launcher".into(),
            governed_only: false,
            benched_until: None,
            strikes: 0,
            replaced: None,
        }]
    }

    /// Judge sessions record their seat and answer per `answers` (default PASS); the unit itself
    /// reports prose.
    struct Seats {
        judged_on: Mutex<Vec<String>>,
        answers: Vec<(&'static str, StepStatus, &'static str)>,
    }
    impl Seats {
        fn new(answers: Vec<(&'static str, StepStatus, &'static str)>) -> Arc<Self> {
            Arc::new(Self {
                judged_on: Mutex::new(Vec::new()),
                answers,
            })
        }
        fn judged(&self) -> Vec<String> {
            self.judged_on.lock().unwrap().clone()
        }
    }
    impl StepRunner for Seats {
        fn run_unit(&self, input: &StepInput) -> StepOutput {
            let mut out = StepOutput {
                run_id: input.run_id.clone(),
                unit_ix: input.unit_ix,
                attempt: input.attempt,
                output: "checked: npm test passes".into(),
                status: StepStatus::Ok,
                usage: None,
                files: Vec::new(),
                tools: Vec::new(),
                governed: false,
            };
            if input.unit.session_id == "validator" {
                let s = input.unit.assigned_cli.clone().unwrap_or_default();
                self.judged_on.lock().unwrap().push(s.clone());
                match self.answers.iter().find(|(k, _, _)| *k == s) {
                    Some((_, status, text)) => {
                        out.status = *status;
                        out.output = text.to_string();
                    }
                    None => out.output = "PASS\nthe fix is right\nPASS".into(),
                }
            }
            out
        }
    }

    /// Run 59232ebc's Check unit (ord 5): pi checks claude's fix under a pinned validator, on a
    /// run seated claude + codex + pi with codex benched for the worktree.
    fn check_unit(dir: &std::path::Path) -> StepInput {
        let mut unit = crate::domain::WorkUnit::pending("r:check", "r", 5, "Check the fix");
        unit.assigned_cli = Some("pi".into());
        unit.authored_by = vec!["claude".into()];
        unit.authored_binaries = vec!["claude".into()];
        unit.validator = Some(crate::validator::DeterministicValidator {
            criterion: "the run left a change in its worktree".into(),
            script: "git status --porcelain | grep -q .".into(),
            approved: true,
        });
        StepInput {
            run_id: "r".into(),
            unit_ix: 4,
            attempt: 0,
            unit,
            workflow_id: "wf-r".into(),
            entity_mode: EntityMode::Isolated,
            workdir: Some(dir.to_path_buf()),
            governance: None,
            prior_outputs: vec![],
            elicitation_epoch: 0,
            process_gen: None,
            launch_seq: 0,
            required_skills: Vec::new(),
        }
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "wicked-core-judge-fallback-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn run_roster() -> Vec<String> {
        ["claude", "codex", "pi"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    /// Run 59232ebc, before the ruling: the only distinct judge (codex) is benched for the
    /// worktree, so the Check gate's judge is a seat failure — `judge_unavailable`, the operator
    /// approves by hand. With the fallback armed, agy judges in codex's place — never claude (the
    /// creator AND the validator author), never pi (the evaluator), never codex — and every
    /// receipt says so.
    #[test]
    fn run_59232ebc_a_benched_codex_judge_falls_back_to_agy_disclosed() {
        let dir = tmp("59232ebc");
        let input = check_unit(&dir);
        let seats = Seats::new(vec![]);
        let runner: Arc<dyn StepRunner> = seats.clone();
        let deltas: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let d2 = deltas.clone();
        let sink = move |c: &str| d2.lock().unwrap().push(c.to_string());

        // TODAY (no fallback): the hold, naming codex.
        let (_, held, _) = run_unit_and_judge_with_roster(
            &runner,
            &input,
            None,
            &sink,
            &registry(),
            &run_roster(),
            &["codex".to_string()],
        );
        let held = held.expect("a required judge that cannot run is a verdict, not a skip");
        assert!(!held.pass && held.judge_cli.is_none(), "{held:?}");
        let f = held.seat_failure.as_deref().expect("seat failure");
        assert!(
            f.contains("codex (benched for this run after a seat failure)"),
            "{f}"
        );
        assert!(seats.judged().is_empty(), "nobody was asked");

        // AFTER: the fallback seats armed, the bench's reasons known.
        let bench = worktree_bench();
        let (_, v, ev) = run_unit_and_judge_with_fallback(
            &runner,
            &input,
            &sink,
            &registry(),
            &run_roster(),
            &bench,
            &fallback_pool(),
            &[],
        );
        let v = v.expect("the fallback judge ruled");
        assert!(v.pass, "{v:?}");
        assert!(v.seat_failure.is_none(), "{v:?}");
        assert_eq!(v.judge_cli.as_deref(), Some("agy"));
        assert_eq!(
            seats.judged(),
            vec!["agy".to_string()],
            "only agy was asked"
        );
        let fb = v
            .judge_fallback
            .as_ref()
            .expect("the fallback is disclosed");
        assert_eq!(fb.seat, "agy");
        assert_eq!(fb.preferred, vec!["codex".to_string()]);
        assert_eq!(
            fb.label,
            "judge: agy (fallback; codex benched: codex cannot work in this run's worktree on \
             this host)"
        );
        assert!(fb.reason.starts_with("codex benched: "), "{}", fb.reason);
        assert!(
            // (core#969) On a line of its own ([`engine_note`]).
            deltas
                .lock()
                .unwrap()
                .iter()
                .any(|d| d == &format!("\n{}\n", fb.label)),
            "the run's live output names the fallback: {:?}",
            deltas.lock().unwrap()
        );
        assert!(ev.judge_refusals.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (C5) On the host floor opencode keeps no per-call check, so the launch bench takes it out of
    /// governed work — never out of judging, which is ungoverned: it judges codex's check of
    /// claude's fix. The same seat under a bench that binds every call is never asked.
    #[test]
    fn a_host_floor_benched_seat_still_judges_but_is_never_routed_governed_work() {
        // The fallback ladder resolves seat identities through the registry: no operator file.
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let home = tmp("hostfloor-judge-home");
        let prev: Vec<_> = ["HOME", "USERPROFILE"]
            .iter()
            .map(std::env::var_os)
            .collect();
        std::env::set_var("HOME", &home);
        std::env::set_var("USERPROFILE", &home);
        let dir = tmp("hostfloor-judge");
        let mut input = check_unit(&dir);
        input.unit.assigned_cli = Some("codex".into());
        let roster: Vec<String> = ["claude", "codex", "opencode"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let host_floor = vec![crate::domain::BenchedSeat {
            cli: "opencode".into(),
            reason: crate::distribute::HOST_FLOOR_BENCH.into(),
            source: "launcher".into(),
            governed_only: true,
            benched_until: None,
            strikes: 0,
            replaced: None,
        }];
        // Routing reads the bench: opencode is never handed governed work.
        let reg = registry();
        let routable: Vec<&str> = crate::distribute::eligible_seats(&reg, &host_floor)
            .iter()
            .map(|c| c.key.as_str())
            .collect();
        assert!(!routable.contains(&"opencode"), "{routable:?}");

        let seats = Seats::new(vec![]);
        let runner: Arc<dyn StepRunner> = seats.clone();
        let (_, v, _) = run_unit_and_judge_with_fallback(
            &runner,
            &input,
            &|_| {},
            &reg,
            &roster,
            &host_floor,
            &fallback_pool(),
            &[],
        );
        let v = v.expect("opencode judged");
        assert_eq!(v.judge_cli.as_deref(), Some("opencode"), "{v:?}");
        assert!(v.judge_fallback.is_none(), "{v:?}");
        assert_eq!(seats.judged(), vec!["opencode".to_string()]);

        // The seat then refuses as a judge: that bench binds every call and REPLACES the host-floor
        // one (`bench_seat`), so the seat is never asked again.
        let mut every_call = host_floor.clone();
        assert!(crate::domain::bench_seat(
            &mut every_call,
            crate::domain::BenchedSeat {
                cli: "opencode".into(),
                reason: "not_logged_in".into(),
                source: "judge".into(),
                governed_only: false,
                benched_until: None,
                strikes: 0,
                replaced: None,
            }
        ));
        assert_eq!(every_call.len(), 1);
        assert!(!every_call[0].governed_only);
        let seats = Seats::new(vec![]);
        let runner: Arc<dyn StepRunner> = seats.clone();
        let (_, v, _) = run_unit_and_judge_with_fallback(
            &runner,
            &input,
            &|_| {},
            &reg,
            &roster,
            &every_call,
            &fallback_pool(),
            &[],
        );
        let v = v.expect("a verdict or a hold");
        assert_ne!(v.judge_cli.as_deref(), Some("opencode"), "{v:?}");
        assert!(!seats.judged().contains(&"opencode".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&home);
        for (k, v) in ["HOME", "USERPROFILE"].iter().zip(prev) {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }

    /// No distinct eligible fallback seat: today's hold, unchanged. agy benched (signed out) and
    /// claude a party ⇒ nobody is asked, and the hold names the bench in its own words.
    #[test]
    fn with_no_distinct_fallback_seat_the_hold_stands() {
        let dir = tmp("nofallback");
        let input = check_unit(&dir);
        let seats = Seats::new(vec![]);
        let runner: Arc<dyn StepRunner> = seats.clone();
        let mut bench = worktree_bench();
        bench.push(crate::domain::BenchedSeat {
            cli: "agy".into(),
            reason: "not_logged_in".into(),
            source: "judge".into(),
            governed_only: false,
            benched_until: None,
            strikes: 0,
            replaced: None,
        });
        let (_, v, _) = run_unit_and_judge_with_fallback(
            &runner,
            &input,
            &|_| {},
            &registry(),
            &run_roster(),
            &bench,
            &fallback_pool(),
            &[],
        );
        let v = v.expect("hold");
        assert!(!v.pass && v.judge_cli.is_none() && v.judge_fallback.is_none());
        let f = v.seat_failure.as_deref().unwrap();
        assert!(
            f.contains("codex (benched for this run: codex cannot work in this run's worktree"),
            "the launcher bench is named as what it is, not a seat failure: {f}"
        );
        assert!(seats.judged().is_empty(), "{:?}", seats.judged());

        // A ballot-only fallback record never judges (core#572), nor one the launcher declared
        // unusable.
        let mut pool = fallback_pool();
        pool[1].enabled_for_council = true; // agy: ballot-only now
        let mut claude = pool[0].clone();
        claude.health = Some(wicked_council::types::SeatHealth::unusable("signed out"));
        pool[0] = claude;
        let cands = fallback_candidates(&pool, &registry(), &["pi"], &[], &["codex".into()], &[]);
        assert_eq!(cands.map(|c| c.len()), Some(0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A fallback seat that cannot answer (signed out) never REJECTs: the hold stands and names
    /// it beside the benched preferred judge; it is reported for the run's bench.
    #[test]
    fn a_fallback_seat_that_refuses_keeps_the_hold_and_is_reported() {
        let dir = tmp("refuses");
        let input = check_unit(&dir);
        let seats = Seats::new(vec![(
            "agy",
            StepStatus::Failed,
            "(cli `agy` exited 1) Not logged in · Please run /login",
        )]);
        let runner: Arc<dyn StepRunner> = seats.clone();
        let (_, v, ev) = run_unit_and_judge_with_fallback(
            &runner,
            &input,
            &|_| {},
            &registry(),
            &run_roster(),
            &worktree_bench(),
            &fallback_pool(),
            &[],
        );
        let v = v.expect("hold");
        assert!(!v.pass && v.judge_cli.is_none() && v.judge_fallback.is_none());
        let f = v.seat_failure.as_deref().unwrap();
        assert!(
            f.contains("codex (benched for this run:") && f.contains("agy ("),
            "{f}"
        );
        assert!(!v.reasoning.contains("REJECT"), "{}", v.reasoning);
        assert_eq!(seats.judged(), vec!["agy".to_string()]);
        assert_eq!(
            ev.judge_refusals,
            vec![crate::workflow::JudgeRefusal {
                seat: "agy".into(),
                reason: "not_logged_in".into(),
                reset_at: None,
            }]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The preferred judge RAN and failed as a seat (copilot's quota refusal, exit 0): the
    /// rotation's seat failure is lifted by a fallback seat the rotation did not walk, disclosed
    /// as "unavailable" (not benched). A verdict-less fallback answer escalates (no REJECT, no
    /// re-ask).
    #[test]
    fn a_preferred_judge_that_fails_as_a_seat_falls_back_and_no_verdict_still_escalates() {
        let dir = tmp("quota");
        let input = check_unit(&dir);
        let roster: Vec<String> = ["claude", "copilot", "pi"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let seats = Seats::new(vec![(
            "copilot",
            StepStatus::Ok,
            "Error: You have exceeded your monthly quota (Request ID: D7DE:0)",
        )]);
        let runner: Arc<dyn StepRunner> = seats.clone();
        let (_, v, _) = run_unit_and_judge_with_fallback(
            &runner,
            &input,
            &|_| {},
            &registry(),
            &roster,
            &[],
            &fallback_pool(),
            &[],
        );
        let v = v.expect("judged");
        assert!(v.pass && v.judge_cli.as_deref() == Some("agy"), "{v:?}");
        assert_eq!(
            seats.judged(),
            vec!["copilot".to_string(), "agy".to_string()]
        );
        let fb = v.judge_fallback.expect("disclosed");
        assert_eq!(fb.preferred, vec!["copilot".to_string()]);
        assert!(
            fb.label
                .starts_with("judge: agy (fallback; copilot unavailable: "),
            "{}",
            fb.label
        );

        // agy answers with no verdict word: escalation, labelled, never a REJECT.
        let seats2 = Seats::new(vec![
            (
                "copilot",
                StepStatus::Ok,
                "Error: You have exceeded your monthly quota (Request ID: D7DE:0)",
            ),
            (
                "agy",
                StepStatus::Ok,
                "I looked at it and it seems fine overall",
            ),
        ]);
        let runner2: Arc<dyn StepRunner> = seats2.clone();
        let (_, v2, _) = run_unit_and_judge_with_fallback(
            &runner2,
            &input,
            &|_| {},
            &registry(),
            &roster,
            &[],
            &fallback_pool(),
            &[],
        );
        let v2 = v2.expect("escalates");
        assert!(!v2.pass && v2.judge_cli.is_none(), "{v2:?}");
        assert!(
            v2.seat_failure
                .as_deref()
                .is_some_and(|s| s.contains(crate::validator::JUDGE_NO_VERDICT)),
            "{v2:?}"
        );
        assert!(
            v2.reasoning.starts_with("judge: agy (fallback; "),
            "{}",
            v2.reasoning
        );
        // (codex r1) …and the receipt still carries the disclosure.
        assert_eq!(
            v2.judge_fallback.as_ref().map(|f| f.seat.as_str()),
            Some("agy")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The DEFAULT judge (no pinned validator): its contract excludes only the work's authors, so
    /// Claude Code — first in fallback order — judges pi's change when codex is benched.
    #[test]
    fn the_default_judge_falls_back_to_claude_when_claude_is_no_party() {
        use std::process::Command;
        fn sh(cwd: &std::path::Path, args: &[&str]) {
            // spawn-audit: test-only — a git fixture building the layout under test.
            let out = Command::new("git")
                .args(args)
                .current_dir(cwd)
                .output()
                .expect("git runs");
            assert!(out.status.success(), "git {args:?}: {out:?}");
        }
        let base = tmp("default");
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        sh(&repo, &["init", "-q", "."]);
        sh(&repo, &["config", "user.email", "t@example.invalid"]);
        sh(&repo, &["config", "user.name", "t"]);
        sh(&repo, &["config", "commit.gpgsign", "false"]);
        sh(&repo, &["config", "core.autocrlf", "false"]);
        std::fs::write(repo.join("README.md"), "base\n").unwrap();
        sh(&repo, &["add", "-A"]);
        sh(&repo, &["commit", "-qm", "base"]);
        let wt = base.join("wt");
        sh(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                wt.to_str().unwrap(),
                "-b",
                "wicked/fb",
            ],
        );
        let mut unit = crate::domain::WorkUnit::pending("r:fix", "r", 4, "Fix the tip");
        unit.assigned_cli = Some("pi".into());
        unit.default_floor = true;
        unit.worktree_baseline = Some(crate::worktree_guard::snapshot(&wt, &repo).unwrap());
        std::fs::write(wt.join("tip.mjs"), "export const tip = (p) => p / 100;\n").unwrap();
        let input = StepInput {
            workdir: Some(wt.clone()),
            unit,
            ..check_unit(&wt)
        };
        let seats = Seats::new(vec![]);
        let runner: Arc<dyn StepRunner> = seats.clone();
        let roster: Vec<String> = vec!["codex".into(), "pi".into()];
        let (_, v, _) = run_unit_and_judge_with_fallback(
            &runner,
            &input,
            &|_| {},
            &registry(),
            &roster,
            &worktree_bench(),
            &fallback_pool(),
            &[],
        );
        let v = v.expect("the default judge ruled");
        assert_eq!(v.judge_cli.as_deref(), Some("claude"), "{v:?}");
        let fb = v.judge_fallback.expect("disclosed");
        assert!(fb
            .label
            .starts_with("judge: claude (fallback; codex benched: "));
        assert!(!seats.judged().iter().any(|s| s == "pi" || s == "codex"));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// (codex r1, P1) A preferred judge that wrote a verdict and exited non-zero ANSWERED: no
    /// fallback judge is asked after it (no verdict shopping) — the pre-ruling hold stands.
    #[test]
    fn a_preferred_judge_that_answered_then_exited_nonzero_gets_no_fallback() {
        let dir = tmp("answered");
        let input = check_unit(&dir);
        let roster: Vec<String> = ["claude", "copilot", "pi"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let seats = Seats::new(vec![(
            "copilot",
            StepStatus::Failed,
            "REJECT\nthe change is unsafe\nREJECT",
        )]);
        let runner: Arc<dyn StepRunner> = seats.clone();
        let (_, v, _) = run_unit_and_judge_with_fallback(
            &runner,
            &input,
            &|_| {},
            &registry(),
            &roster,
            &[],
            &fallback_pool(),
            &[],
        );
        let v = v.expect("hold");
        assert!(!v.pass && v.judge_fallback.is_none(), "{v:?}");
        assert_eq!(
            seats.judged(),
            vec!["copilot".to_string()],
            "agy never asked"
        );
        // …and inside the fallback: a fallback seat that answered and exited non-zero ends it.
        let mut pool = fallback_pool();
        pool.insert(1, seat("gemini", "gemini -p {PROMPT}"));
        let seats2 = Seats::new(vec![("gemini", StepStatus::Failed, "PASS\nok\nPASS")]);
        let runner2: Arc<dyn StepRunner> = seats2.clone();
        let mut reg = registry();
        reg.push(seat("gemini", "gemini -p {PROMPT}"));
        let (_, v2, _) = run_unit_and_judge_with_fallback(
            &runner2,
            &input,
            &|_| {},
            &reg,
            &run_roster(),
            &worktree_bench(),
            &pool,
            &[],
        );
        let v2 = v2.expect("hold");
        assert!(!v2.pass && v2.judge_fallback.is_none(), "{v2:?}");
        assert_eq!(
            seats2.judged(),
            vec!["gemini".to_string()],
            "agy never asked"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (codex r1, P1) An evaluator unit's judge reads the CREATOR's work: every seat that
    /// authored it is excluded from the fallback — a fallback agy never judges agy's own build.
    #[test]
    fn the_creator_of_the_work_never_judges_it_even_as_a_fallback() {
        let dir = tmp("creator");
        let mut input = check_unit(&dir);
        input.unit.authored_by = Vec::new();
        let seats = Seats::new(vec![]);
        let runner: Arc<dyn StepRunner> = seats.clone();
        // The work's author is agy (a launch that made it work-eligible); pi evaluates.
        let authors = vec!["agy".to_string()];
        let (_, v, _) = run_unit_and_judge_with_fallback(
            &runner,
            &input,
            &|_| {},
            &registry(),
            &run_roster(),
            &worktree_bench(),
            &fallback_pool(),
            &authors,
        );
        let v = v.expect("hold");
        assert!(!v.pass && v.judge_cli.is_none(), "{v:?}");
        assert!(seats.judged().is_empty(), "{:?}", seats.judged());
        // A unit dispatched under an agy invocation (an alias key) is agy for the fallback.
        let mut aliased = check_unit(&dir);
        aliased.unit.assigned_invocation = Some("agy -p {PROMPT}".into());
        let seats3 = Seats::new(vec![]);
        let runner3: Arc<dyn StepRunner> = seats3.clone();
        let (_, v3, _) = run_unit_and_judge_with_fallback(
            &runner3,
            &aliased,
            &|_| {},
            &registry(),
            &run_roster(),
            &worktree_bench(),
            &fallback_pool(),
            &[],
        );
        assert!(v3.expect("hold").judge_cli.is_none());
        assert!(seats3.judged().is_empty(), "{:?}", seats3.judged());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (codex r2/r3, P1) The binary an author ACTUALLY ran is excluded: a creator dispatched as
    /// `copilot` with an agy invocation (recorded at dispatch, kept across a reassign), or this
    /// unit launched under a QUOTED agy path, leaves agy no fallback judge; work whose author has
    /// no recorded binary gets no fallback; a quoted fallback invocation is still agy; and any
    /// instance of a benched CLI is benched.
    #[test]
    fn an_authors_actual_binary_is_excluded_even_under_another_key_or_a_quoted_path() {
        let dir = tmp("launched");
        let seats = Seats::new(vec![]);
        let runner: Arc<dyn StepRunner> = seats.clone();
        let mut input = check_unit(&dir);
        input.unit.authored_by = Vec::new();
        let authors = vec!["copilot".to_string(), "claude".to_string()];
        // The build ran copilot-as-agy, then was reassigned to claude: both binaries recorded.
        let binaries = vec!["agy".to_string(), "claude".to_string()];
        let (_, v, _) = run_unit_and_judge_with_launches(
            &runner,
            &input,
            &|_| {},
            &registry(),
            &run_roster(),
            &worktree_bench(),
            &fallback_pool(),
            &authors,
            Some(&binaries),
        );
        assert!(v.expect("hold").judge_cli.is_none());
        assert!(seats.judged().is_empty(), "{:?}", seats.judged());
        // An author whose binary was never recorded: no fallback.
        let (_, v0, _) = run_unit_and_judge_with_launches(
            &runner,
            &input,
            &|_| {},
            &registry(),
            &run_roster(),
            &worktree_bench(),
            &fallback_pool(),
            &authors,
            None,
        );
        assert!(v0.expect("hold").judge_cli.is_none());
        assert!(seats.judged().is_empty(), "{:?}", seats.judged());
        // This unit dispatched under a QUOTED agy path (with a space): still agy.
        let mut quoted = check_unit(&dir);
        quoted.unit.assigned_invocation =
            Some("\"/Applications/Anti Gravity/bin/agy\" -p {PROMPT}".to_string());
        let (_, v2, _) = run_unit_and_judge_with_fallback(
            &runner,
            &quoted,
            &|_| {},
            &registry(),
            &run_roster(),
            &worktree_bench(),
            &fallback_pool(),
            &[],
        );
        assert!(v2.expect("hold").judge_cli.is_none());
        assert!(seats.judged().is_empty(), "{:?}", seats.judged());
        // A QUOTED fallback invocation is agy too: an agy author excludes it.
        let mut qpool = fallback_pool();
        qpool[1].headless_invocation = "\"/opt/Anti Gravity/agy\" -p {PROMPT}".into();
        let reg = registry();
        let c = fallback_candidates(&qpool, &reg, &["pi"], &["agy".to_string()], &[], &[])
            .expect("known parties");
        assert!(c.iter().all(|s| s.key != "agy"), "{c:?}");
        // Any instance of a benched CLI is benched, both ways round.
        let mut inst = seat("claude#2", "claude -p {PROMPT}");
        inst.enabled_for_council = false;
        let none = fallback_candidates(
            std::slice::from_ref(&inst),
            &reg,
            &["pi"],
            &[],
            &["claude".into()],
            &[],
        )
        .expect("known parties");
        assert!(none.is_empty(), "{none:?}");
        let none2 = fallback_candidates(
            std::slice::from_ref(&inst),
            &reg,
            &["pi"],
            &[],
            &["claude#1".into()],
            &[],
        )
        .expect("known parties");
        assert!(none2.is_empty(), "{none2:?}");
        // The 59232ebc shape with its recorded binary (claude built it): agy still judges.
        let built = vec!["claude".to_string()];
        let (_, v4, _) = run_unit_and_judge_with_launches(
            &runner,
            &check_unit(&dir),
            &|_| {},
            &registry(),
            &run_roster(),
            &worktree_bench(),
            &fallback_pool(),
            &["claude".to_string()],
            Some(&built),
        );
        assert_eq!(v4.expect("judged").judge_cli.as_deref(), Some("agy"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (codex r3) The authors' binaries come from the per-dispatch record, which a reassign
    /// never clears; a unit with authors but no record (persisted before it) is unknown.
    #[test]
    fn the_authors_binaries_are_the_dispatch_record() {
        use crate::domain::{StageKind, WorkUnit};
        let mut build = WorkUnit::pending("r:fix", "r", 1, "fix");
        build.stage = StageKind::Build;
        build.assigned_cli = Some("claude".into()); // reassigned from copilot; invocation cleared
        build.authored_by = vec!["copilot".into(), "claude".into()];
        build.authored_binaries = vec!["agy".into(), "claude".into()];
        let mut planned = WorkUnit::pending("r:later", "r", 2, "never dispatched");
        planned.stage = StageKind::Build;
        planned.assigned_cli = Some("pi".into());
        let check = WorkUnit::pending("r:check", "r", 3, "check");
        let units = vec![build.clone(), planned, check];
        assert_eq!(
            crate::pipeline::work_author_binaries(&units, 3),
            Some(vec!["agy".to_string(), "claude".to_string()])
        );
        // (codex r5) A Creator-role unit of another stage is an author for the fallback: its
        // recorded binary counts, and one dispatched with none recorded is unknown.
        let mut test_creator = WorkUnit::pending("r:test", "r", 2, "write the tests");
        test_creator.stage = StageKind::Test;
        test_creator.role = crate::workflow::PhaseRole::Creator;
        test_creator.assigned_cli = Some("copilot".into());
        test_creator.authored_binaries = vec!["agy".into()];
        let with_test = vec![
            test_creator.clone(),
            WorkUnit::pending("r:c", "r", 3, "check"),
        ];
        assert_eq!(
            crate::pipeline::work_author_binaries(&with_test, 3),
            Some(vec!["agy".to_string()])
        );
        assert!(
            crate::pipeline::fallback_work_authors(&with_test, 3).contains(&"copilot".to_string())
        );
        // (codex r7) A floor fix recorded on an earlier evaluator unit is an author too.
        let mut reviewed = WorkUnit::pending("r:review", "r", 2, "review");
        reviewed.stage = StageKind::Review;
        reviewed.role = crate::workflow::PhaseRole::Evaluator;
        reviewed.authored_by = vec!["copilot".into()];
        reviewed.authored_binaries = vec!["copilot".into(), "agy".into()];
        let with_fix = vec![reviewed, WorkUnit::pending("r:c", "r", 3, "check")];
        assert_eq!(
            crate::pipeline::work_author_binaries(&with_fix, 3),
            Some(vec!["copilot".to_string(), "agy".to_string()])
        );
        assert!(
            crate::pipeline::fallback_work_authors(&with_fix, 3).contains(&"copilot".to_string())
        );
        test_creator.authored_binaries.clear();
        test_creator.last_attempt = Some(0);
        assert_eq!(
            crate::pipeline::work_author_binaries(&[test_creator], 3),
            None
        );
        let mut legacy = build;
        legacy.authored_binaries.clear();
        assert_eq!(
            crate::pipeline::work_author_binaries(std::slice::from_ref(&legacy), 3),
            None
        );
        // (codex r4) A later dispatch of that legacy unit never makes its history look complete.
        assert!(crate::domain::stamp_author(
            &mut legacy,
            "pi".into(),
            vec!["pi".into()]
        ));
        assert_eq!(
            legacy.authored_binaries,
            vec![
                crate::domain::UNKNOWN_AUTHOR_BINARY.to_string(),
                "pi".to_string()
            ]
        );
        assert_eq!(crate::pipeline::work_author_binaries(&[legacy], 3), None);
        // A fresh unit's first dispatch records seat and binary, nothing unknown.
        let mut fresh = WorkUnit::pending("r:new", "r", 1, "fix");
        assert!(crate::domain::stamp_author(
            &mut fresh,
            "copilot".into(),
            vec!["agy".into()]
        ));
        assert_eq!(fresh.authored_binaries, vec!["agy".to_string()]);
        assert!(!crate::domain::stamp_author(
            &mut fresh,
            "copilot".into(),
            vec!["agy".into()]
        ));
        // (codex r6) A non-build Creator dispatched before the record: its next stamp keeps the
        // unknown mark, so a reassign never makes that history look complete.
        let mut creator = WorkUnit::pending("r:t", "r", 2, "tests");
        assert!(crate::domain::stamp_author_binaries(
            &mut creator,
            true,
            vec!["pi".into()]
        ));
        assert_eq!(
            creator.authored_binaries,
            vec![
                crate::domain::UNKNOWN_AUTHOR_BINARY.to_string(),
                "pi".to_string()
            ]
        );
    }

    /// (codex r6, P1) A party is excluded by EVERY carrier its record may run — its invocation,
    /// its `binary` field and its ACP adapter — so a seat keyed `copilot` whose record runs claude
    /// leaves claude no fallback.
    #[test]
    fn a_party_is_excluded_by_every_carrier_its_record_runs() {
        let mut odd = seat("copilot", "copilot -p {PROMPT}");
        odd.binary = "claude".into();
        let ids = record_identities(&odd);
        assert!(ids.contains(&"copilot".to_string()) && ids.contains(&"claude".to_string()));
        let mut acp = seat("copilot", "copilot -p {PROMPT}");
        acp.acp = crate::registry_roster()
            .into_iter()
            .find(|c| c.key == "claude")
            .and_then(|c| c.acp);
        if acp.acp.is_some() {
            assert!(record_identities(&acp).contains(&"claude".to_string()));
        }
        let mut reg = registry();
        reg.retain(|c| c.key != "copilot");
        reg.push(odd);
        let cands = fallback_candidates(&fallback_pool(), &reg, &["copilot", "pi"], &[], &[], &[])
            .expect("known parties");
        assert!(cands.iter().all(|c| c.key != "claude"), "{cands:?}");
        assert!(cands.iter().any(|c| c.key == "agy"), "{cands:?}");
    }

    /// (codex r4, P1/P2) A Build unit's OWN recorded binaries are excluded from its fallback
    /// judge (it ran copilot-as-agy, then was reassigned to claude), an unknown own history holds,
    /// and no instance of a party's CLI is a fallback whatever it invokes.
    #[test]
    fn a_units_own_author_binaries_and_party_instances_are_excluded() {
        let dir = tmp("own");
        let seats = Seats::new(vec![]);
        let runner: Arc<dyn StepRunner> = seats.clone();
        let mut build = check_unit(&dir);
        build.unit.assigned_cli = Some("claude".into());
        build.unit.authored_by = vec!["copilot".into(), "claude".into()];
        build.unit.authored_binaries = vec!["agy".into(), "claude".into()];
        // pi is launch-excluded from judging, so codex (benched) was the only preferred judge.
        build.unit.exclude_seats = vec!["pi".into()];
        let (_, v, _) = run_unit_and_judge_with_fallback(
            &runner,
            &build,
            &|_| {},
            &registry(),
            &run_roster(),
            &worktree_bench(),
            &fallback_pool(),
            &[],
        );
        assert!(v.expect("hold").judge_cli.is_none());
        assert!(seats.judged().is_empty(), "{:?}", seats.judged());
        let mut unknown = build.clone();
        unknown.unit.authored_binaries =
            vec![crate::domain::UNKNOWN_AUTHOR_BINARY.into(), "claude".into()];
        let (_, v2, _) = run_unit_and_judge_with_fallback(
            &runner,
            &unknown,
            &|_| {},
            &registry(),
            &run_roster(),
            &worktree_bench(),
            &fallback_pool(),
            &[],
        );
        assert!(v2.expect("hold").judge_cli.is_none());
        assert!(seats.judged().is_empty(), "{:?}", seats.judged());
        // `claude#2` invoking agy is still claude's instance: never a fallback when claude is a party.
        let mut inst = seat("claude#2", "agy -p {PROMPT}");
        inst.enabled_for_council = false;
        let reg = registry();
        let c = fallback_candidates(
            std::slice::from_ref(&inst),
            &reg,
            &["claude"],
            &[],
            &[],
            &[],
        )
        .expect("known parties");
        assert!(c.is_empty(), "{c:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_fallback_seats_and_the_off_switch() {
        assert_eq!(judge_fallback_keys_from(None), vec!["claude", "agy"]);
        assert!(judge_fallback_keys_from(Some("")).is_empty());
        assert!(judge_fallback_keys_from(Some(" off ")).is_empty());
        assert_eq!(
            judge_fallback_keys_from(Some("agy, claude")),
            vec!["agy", "claude"]
        );
        // (codex r5) Only the ruling's seats: another key is ignored.
        assert_eq!(judge_fallback_keys_from(Some("reviewer, agy")), vec!["agy"]);
        // (codex r5) A record must run its key's CLI on every carrier.
        let claude = seat("claude", "claude -p {PROMPT}");
        assert!(record_runs_its_key(&claude));
        let mut odd = claude.clone();
        odd.headless_invocation = "agy -p {PROMPT}".into();
        assert!(!record_runs_its_key(&odd), "invocation runs another CLI");
        let mut odd2 = claude.clone();
        odd2.binary = "agy".into();
        assert!(!record_runs_its_key(&odd2), "binary names another CLI");
        let mut acp = claude.clone();
        acp.acp = crate::registry_roster()
            .into_iter()
            .find(|c| c.key == "claude")
            .and_then(|c| c.acp);
        assert!(record_runs_its_key(&acp), "claude-agent-acp runs claude");
        if let Some(a) = acp.acp.as_mut() {
            a.binary = "agy-acp".into();
            assert!(!record_runs_its_key(&acp), "an ACP adapter of another CLI");
        }
        assert_eq!(
            short_reason(crate::distribute::CODEX_WORKTREE_BENCH),
            "codex cannot work in this run's worktree on this host"
        );
        assert_eq!(short_reason("not_logged_in"), "not_logged_in");
        // A fallback candidate never shares an author's identity, even under another key.
        let mut alias = seat("agy", "claude -p {PROMPT}");
        alias.enabled_for_council = false;
        let ids = vec!["claude".to_string()];
        let reg = registry();
        assert_eq!(
            fallback_candidates(&[alias], &reg, &["claude"], &ids, &[], &[]).map(|c| c.len()),
            Some(0)
        );
        // …nor a seat benched under an instance key.
        let c = seat("claude", "claude -p {PROMPT}");
        assert_eq!(
            fallback_candidates(
                std::slice::from_ref(&c),
                &reg,
                &["pi"],
                &[],
                &["claude#2".into()],
                &[]
            )
            .map(|c| c.len()),
            Some(0)
        );
        // (codex r1) A party whose key names no registry seat has an unknown binary: no fallback.
        assert!(fallback_candidates(&[c], &reg, &["writer"], &[], &[], &[]).is_none());
    }

    /// core#954: an EVALUATOR unit (claude, also the validator author) checks the work codex
    /// built, under a pinned validator. `roster` is the run's seats.
    fn evaluator_unit(dir: &std::path::Path) -> StepInput {
        let mut input = check_unit(dir);
        input.unit.assigned_cli = Some("claude".into());
        input.unit.authored_by = Vec::new();
        input.unit.authored_binaries = Vec::new();
        input.unit.role = crate::workflow::PhaseRole::Evaluator;
        input
    }

    fn judge_954(
        input: &StepInput,
        run: &[&str],
        work_authors: &[String],
    ) -> (Arc<Seats>, crate::validator::AgentVerdict, Vec<String>) {
        judge_954_with(input, run, work_authors, &[], vec![])
    }

    fn judge_954_with(
        input: &StepInput,
        run: &[&str],
        work_authors: &[String],
        binaries: &[String],
        answers: Vec<(&'static str, StepStatus, &'static str)>,
    ) -> (Arc<Seats>, crate::validator::AgentVerdict, Vec<String>) {
        let seats = Seats::new(answers);
        let runner: Arc<dyn StepRunner> = seats.clone();
        let deltas: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let d2 = deltas.clone();
        let sink = move |c: &str| d2.lock().unwrap().push(c.to_string());
        let run: Vec<String> = run.iter().map(|s| s.to_string()).collect();
        let (_, v, _) = run_unit_and_judge_with_launches(
            &runner,
            input,
            &sink,
            &registry(),
            &run,
            &[],
            &fallback_pool(),
            work_authors,
            Some(binaries),
        );
        let deltas = deltas.lock().unwrap().clone();
        (seats, v.expect("a pinned judge ruled"), deltas)
    }

    /// core#954 (codex r1): a creator dispatched as `copilot` that actually RAN codex's binary —
    /// codex is the creator too, and never judges while pi can.
    #[test]
    fn core_954_a_seat_running_a_creators_recorded_binary_does_not_judge() {
        let dir = tmp("954-bin");
        let input = evaluator_unit(&dir);
        let (seats, v, _) = judge_954_with(
            &input,
            &["claude", "codex", "pi", "copilot"],
            &["copilot".into()],
            &["codex".into()],
            vec![],
        );
        assert_eq!(seats.judged(), vec!["pi".to_string()], "{v:?}");
        assert!(v.judge_fallback.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// core#954 (codex r1): the creator fallback that answered without a verdict holds the gate
    /// AND is disclosed (as a fallback seat's verdict-less answer is).
    #[test]
    fn core_954_a_creator_judge_without_a_verdict_is_still_disclosed() {
        let dir = tmp("954-nv");
        let input = evaluator_unit(&dir);
        let (_, v, _) = judge_954_with(
            &input,
            &["claude", "codex"],
            &["codex".into()],
            &[],
            vec![(
                "codex",
                StepStatus::Ok,
                "I looked at it and it seems reasonable.",
            )],
        );
        assert!(!v.pass && v.seat_failure.is_some(), "{v:?}");
        let fb = v.judge_fallback.as_ref().expect("disclosed");
        assert_eq!(fb.seat, "codex");
        assert!(fb.label.contains("creator fallback"), "{}", fb.label);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// core#954: the run's own judge rotation never seats the work's creator on an evaluator
    /// unit's gate while another eligible seat exists. Before the fix the rotation (anchored on
    /// claude) picked codex — the seat that built the work judged it.
    #[test]
    fn core_954_an_evaluator_judge_rotation_skips_the_creator_when_a_seat_remains() {
        let dir = tmp("954-three");
        let input = evaluator_unit(&dir);
        let (seats, v, _) = judge_954(&input, &["claude", "codex", "pi"], &["codex".into()]);
        assert_eq!(
            seats.judged(),
            vec!["pi".to_string()],
            "codex built the work"
        );
        assert_eq!(v.judge_cli.as_deref(), Some("pi"));
        assert_eq!(v.judge_distinct, Some(true));
        assert!(
            v.judge_fallback.is_none(),
            "the routine case is not labelled: {v:?}"
        );
        // A non-evaluator unit's rotation is unchanged (the creator is its own assigned seat).
        let mut neutral = evaluator_unit(&dir);
        neutral.unit.role = crate::workflow::PhaseRole::Neutral;
        let (seats, _, _) = judge_954(&neutral, &["claude", "codex", "pi"], &["codex".into()]);
        assert_eq!(seats.judged(), vec!["codex".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// core#954: with no eligible seat distinct from the creators (a two-seat run), the rotation
    /// judges as before — and says so: a `creator fallback` disclosure on the verdict (the gate,
    /// its receipt and the delivery receipt carry it), `judgeDistinct: false`, and a line in the
    /// run's live output. Never silent.
    #[test]
    fn core_954_a_two_seat_run_falls_back_to_the_creator_judge_disclosed() {
        let dir = tmp("954-two");
        let input = evaluator_unit(&dir);
        let (seats, v, deltas) = judge_954(&input, &["claude", "codex"], &["codex".into()]);
        assert_eq!(seats.judged(), vec!["codex".to_string()]);
        assert_eq!(v.judge_cli.as_deref(), Some("codex"));
        assert_eq!(v.judge_distinct, Some(false));
        let fb = v.judge_fallback.as_ref().expect("disclosed");
        assert_eq!(fb.seat, "codex");
        assert!(fb.preferred.is_empty());
        assert_eq!(
            fb.label,
            "judge: codex (creator fallback; no eligible judge seat distinct from the work's \
             creators [codex])"
        );
        assert!(
            deltas.iter().any(|d| d == &format!("\n{}\n", fb.label)),
            "{deltas:?}"
        );
        // (rig run 1d372696) The judge's run is said on the unit's stream as it starts, so the
        // hand-off to floor gap is never silent.
        assert!(
            deltas
                .iter()
                .any(|d| d.contains(&format!("{JUDGE_NOTE} — 0 min"))),
            "{deltas:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
