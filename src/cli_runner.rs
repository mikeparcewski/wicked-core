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
//!    with their OWN `rusqlite` connection to the bus db (a different file from the estate store the
//!    actor owns — no writer-lock contention). Neither holds a store handle: the `cli-runner` reads only
//!    the dispatched event + publishes the result; the actor stays the ONLY writer.
//!  * The actor reaches nothing here by a blocking poll. It only *publishes* `task.dispatched`, a single
//!    bounded local INSERT into a WAL-mode db via an actor-thread-local [`BusDb`] — the reducer's publish
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

/// The event the reducer publishes and the `cli-runner` consumes (filtered).
pub const TASK_DISPATCHED: &str = "wicked.crew.task.dispatched";
/// The event the `cli-runner` publishes and the reducer consumes.
pub const TASK_COMPLETED: &str = "wicked.crew.task.completed";

/// Gate evaluation events — wicked-core publishes a request; the governed evaluator daemon responds.
/// When `WICKED_BUS_DB` is set, `run_unit_and_judge_with_roster` publishes one of these instead of
/// spawning a raw `claude -p` subprocess, and blocks (up to [`GATE_EVAL_TIMEOUT`]) for the response.
/// The daemon runs under its OWN governed session (no `--dangerously-skip-permissions`).
pub const GATE_EVAL_REQUESTED: &str = "wicked.gate.eval.requested";
pub const GATE_EVAL_RESPONDED: &str = "wicked.gate.eval.responded";

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
}

/// Payload the governed evaluator daemon publishes back.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct GateEvalResponse {
    eval_id: String,
    pass: bool,
    reasoning: String,
}

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
}

fn status_to_str(s: StepStatus) -> &'static str {
    match s {
        StepStatus::Ok => "ok",
        StepStatus::Failed => "failed",
        StepStatus::Cancelled => "cancelled",
        // ACP elicitation terminal: non-retriable; bypasses FailureTriageReady (DES-002 I-7).
        StepStatus::ElicitationFailed => "elicitation_failed",
    }
}

fn status_from_str(s: &str) -> StepStatus {
    match s {
        "failed" => StepStatus::Failed,
        "cancelled" => StepStatus::Cancelled,
        // ACP elicitation terminal — explicit arm required; the wildcard default `Ok` is WRONG here
        // (DES-002-tests.md §StepStatus exhaustive match sites).
        "elicitation_failed" => StepStatus::ElicitationFailed,
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
/// Returns `Some(AgentVerdict)` on a successful response, `None` on timeout or bus error. A `None`
/// result is NOT a reject — the caller interprets it as "no agent verdict" so `combine_verdict`
/// falls through to the deterministic-only path (Approve iff deterministic passes).
///
/// WHY NOT A SUBPROCESS: the inline `agent_validate` spawns `claude -p --plugin-dir <garden>` whose
/// `SessionStart` hook fires `bootstrap.py` → bubbletea TUI → "open /dev/tty: device not configured"
/// on the first stdout line → `parse_agent_verdict` fails closed. The bus path routes evaluation to
/// a governed daemon running under its OWN Claude Code session with normal tool approvals — no
/// `--dangerously-skip-permissions`, no headless-TTY issues.
fn bus_request_agent_verdict(
    criterion: &str,
    work: &str,
    run_id: &str,
    unit_ix: usize,
    attempt: u32,
    bus_db_path: &str,
) -> Option<crate::validator::AgentVerdict> {
    let db = BusDb::open(bus_db_path)
        .map_err(|e| eprintln!("wicked-core: gate eval — cannot open bus db: {e}"))
        .ok()?;

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
    };
    let payload = serde_json::to_value(&request)
        .map_err(|e| eprintln!("wicked-core: gate eval — cannot serialize request: {e}"))
        .ok()?;
    let key = deterministic_key(&["gate-eval-req", &eval_id]);
    let ev = BusEmit::new(GATE_EVAL_REQUESTED, CORE_DOMAIN, "core.gate", payload).with_key(key);
    // Capture the emitted event_id so polling starts AFTER this request — no historical rescans.
    let floor_start = db
        .emit(&ev)
        .map_err(|e| eprintln!("wicked-core: gate eval — cannot publish request: {e}"))
        .ok()?;

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
                    return Some(crate::validator::AgentVerdict {
                        pass: resp.pass,
                        reasoning: resp.reasoning,
                    });
                }
            }
            floor = floor.max(ev.event_id);
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    eprintln!(
        "wicked-core: gate eval timed out after {GATE_EVAL_TIMEOUT:?} (eval_id={eval_id}) — \
         falling back to deterministic-only verdict"
    );
    None
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
/// Only worktree-relative, non-escaping deliverables are read — the SAME constraint the deterministic
/// floor (`missing_deliverables`, execute_wrapped.rs) enforces. A `required_deliverables` list comes from
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

pub(crate) fn run_unit_and_judge(
    runner: &Arc<dyn StepRunner>,
    input: &StepInput,
    agent_review_target: Option<&str>,
    emit_delta: &DeltaSink,
) -> (StepOutput, Option<(bool, String)>) {
    run_unit_and_judge_with_roster(
        runner,
        input,
        agent_review_target,
        emit_delta,
        &crate::registry_roster(),
    )
}

/// The roster-injectable core of [`run_unit_and_judge`] — split out ONLY so the seat-selection (C1) is
/// unit-testable with a fabricated roster and no live registry. Production always passes the live
/// [`crate::registry_roster`].
fn run_unit_and_judge_with_roster(
    runner: &Arc<dyn StepRunner>,
    input: &StepInput,
    agent_review_target: Option<&str>,
    emit_delta: &DeltaSink,
    roster: &[crate::AgenticCli],
) -> (StepOutput, Option<(bool, String)>) {
    let output = runner.run_unit_streaming(input, emit_delta);
    let work_owned = select_work_for_agent(
        input.unit.role,
        &input.unit.required_deliverables,
        input.workdir.as_deref(),
        agent_review_target,
        &output.output,
    );
    let work_for_agent: &str = &work_owned;
    let agent_verdict = if output.status == StepStatus::Ok && input.workdir.is_some() {
        input
            .unit
            .validator
            .as_ref()
            .filter(|v| v.approved)
            .and_then(|v| {
                // BUS PATH: when `WICKED_BUS_DB` is set, publish a gate-evaluation request and wait for
                // the governed evaluator daemon to respond (no subprocess, no dangerous flags, no TTY).
                // `None` on timeout ⇒ deterministic-only (combine_verdict approves iff det_pass=true).
                if let Ok(bus_path) = std::env::var("WICKED_BUS_DB") {
                    return bus_request_agent_verdict(
                        &v.criterion,
                        work_for_agent,
                        &input.run_id,
                        input.unit_ix,
                        input.attempt,
                        &bus_path,
                    )
                    .map(|av| (av.pass, av.reasoning));
                }
                // INLINE PATH (legacy — no bus): spawn a governed council seat subprocess.
                // GAP B + C1: run the agent judge under a council seat whose identity is DISTINCT from
                // BOTH the deterministic validator's author (`DETERMINISTIC_VALIDATOR_SEAT`) AND the
                // WORK's own author (the unit's `assigned_cli`, falling back to the deterministic author
                // when unassigned). Excluding the work author is what stops a self-grade — the judge can
                // never be dispatched under the very seat that WROTE the work. When no identity-distinct
                // seat exists, `agent_validate` falls back to the single default runner (documented).
                let work_author = input
                    .unit
                    .assigned_cli
                    .as_deref()
                    .unwrap_or(crate::validator::DETERMINISTIC_VALIDATOR_SEAT);
                let excluded = [crate::validator::DETERMINISTIC_VALIDATOR_SEAT, work_author];
                Some(
                    match crate::validator::agent_validate(
                        &v.criterion,
                        work_for_agent,
                        &excluded,
                        roster,
                        &**runner,
                    ) {
                        Ok(av) => (av.pass, av.reasoning),
                        Err(e) => (false, format!("agent validator errored (fail-closed): {e}")),
                    },
                )
            })
    } else {
        None
    };
    (output, agent_verdict)
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

/// Arm exec-mediation on the CURRENT (actor) thread with an open bus publisher. Returns `false` if the
/// bus db can't be opened — the caller then leaves exec mode OFF and the default in-process path stands
/// (the same disable-on-uninitialized posture as the launch bridge's floor snapshot).
pub(crate) fn arm_exec_publisher(bus_db_path: &str) -> bool {
    match BusDb::open(bus_db_path) {
        Ok(db) => {
            // #8: the publisher INSERT runs on the single-writer actor thread — a 5s busy-wait behind a
            // concurrent writer would stall every other actor command. A short timeout makes SQLITE_BUSY
            // surface fast so `try_publish_dispatched` falls back to the in-process worker instead.
            let _ = db.set_busy_timeout(Duration::from_millis(250));
            EXEC_PUBLISHER.with(|cell| *cell.borrow_mut() = Some(db));
            true
        }
        Err(e) => {
            eprintln!(
                "wicked-core: exec-mediation disabled — cannot open bus db {bus_db_path} to publish \
                 task.dispatched: {e}; falling back to in-process dispatch"
            );
            false
        }
    }
}

/// Disarm exec-mediation on the current thread (actor loop exit).
pub(crate) fn disarm_exec_publisher() {
    EXEC_PUBLISHER.with(|cell| *cell.borrow_mut() = None);
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
        match db.emit(&ev) {
            Ok(_) => true,
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

/// Both exec-mediation consumers, each with an OPEN bus connection and a RESOLVED start floor — built on
/// the actor thread BEFORE the publisher is armed (the ATOMIC-ARM invariant, finding #4). Owning the open
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
    /// Bus db path — kept so `run_cli_runner` can open a second connection for `find_completed`.
    bus_db_path: String,
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
/// publisher (the in-process path stands). Runs on the actor thread; the opened connections are MOVED
/// into the consumer threads by [`spawn_exec_consumers`] (`rusqlite::Connection` is `Send`), so a
/// successful init here == a working bus handle in the thread — no second-open race that could leave the
/// publisher armed with a dead consumer.
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

    let cli_runner_db = BusDb::open(bus_db_path)
        .map_err(|e| eprintln!("wicked-core: cli-runner cannot open bus db {bus_db_path}: {e}"))
        .ok()?;

    // Startup reclamation (DES-002 mechanism B) — migrate predecessor cursor rows.
    // Returns None on DB error (fail closed — exec-mediation stays OFF).
    // Returns Some(None) on first boot / clean shutdown predecessor.
    // Returns Some(Some(gen)) when a crashed predecessor's cursors were migrated.
    let predecessor_gen: Option<uuid::Uuid> =
        reclaim_predecessor_cursors(&cli_runner_db, workspace_id, &c_name, &cc_name)?;

    let cli_runner_floor = resume_floor(&cli_runner_db, &c_name)?;
    let completed_db = BusDb::open(bus_db_path)
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
        bus_db_path: bus_db_path.to_string(),
    })
}

/// Spawn both off-actor consumer threads from a pre-initialized [`ExecConsumers`]. Called ONLY after the
/// publisher is armed, so arm+consumers land together (finding #4).
pub(crate) fn spawn_exec_consumers(
    consumers: ExecConsumers,
    runner: Arc<dyn StepRunner>,
    tx: Sender<Command>,
    lifecycle_maps: Option<Arc<std::sync::Mutex<ElicitationMaps>>>,
    actor_process_gen: uuid::Uuid,
    poll_interval: Duration,
    stop: Arc<AtomicBool>,
) -> Vec<JoinHandle<()>> {
    let ExecConsumers {
        cli_runner_db,
        cli_runner_floor,
        completed_db,
        completed_floor,
        consumer_name,
        completed_consumer_name,
        predecessor_gen,
        bus_db_path,
    } = consumers;
    vec![
        run_cli_runner(
            cli_runner_db,
            bus_db_path,
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
    bus_db_path: String,
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
                        let real_completion = BusDb::open(&bus_db_path).ok().and_then(|scan_db| {
                            scan_db
                                .find_completed(
                                    &completed_consumer_name,
                                    &task.run_id,
                                    task.launch_seq,
                                )
                                .ok()
                                .flatten()
                        });

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
                            let agent_verdict =
                                completed.agent_verdict.map(|v| (v.pass, v.reasoning));
                            let (ack_tx, ack_rx) = std::sync::mpsc::sync_channel::<()>(0);
                            if actor_tx
                                .send(Command::ApplyStepResult {
                                    output,
                                    agent_verdict,
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
                };
                // Live-output sink (parity gap #11): stream each chunk to the actor's single emit
                // point as a `Command::CliOutputDelta`, exactly as the in-process worker does.
                let delta_run_id = task.run_id.clone();
                let delta_ord = task.unit.ord;
                let delta_process_gen = task.process_gen;
                let delta_launch_seq = task.launch_seq;
                let delta_tx = std::sync::Mutex::new(actor_tx.clone());
                let emit_delta = move |chunk: &str| {
                    if let Ok(g) = delta_tx.lock() {
                        let _ = g.send(Command::CliOutputDelta {
                            run_id: delta_run_id.clone(),
                            ord: delta_ord,
                            chunk: chunk.to_string(),
                            process_gen: delta_process_gen,
                            launch_seq: delta_launch_seq,
                        });
                    }
                };
                let (output, agent_verdict) = run_unit_and_judge(
                    &runner,
                    &input,
                    task.agent_review_target.as_deref(),
                    &emit_delta,
                );
                let completed = CompletedTask {
                    run_id: output.run_id.clone(),
                    unit_ix: output.unit_ix,
                    attempt: output.attempt,
                    output: output.output.clone(),
                    status: status_to_str(output.status).to_string(),
                    agent_verdict: agent_verdict
                        .map(|(pass, reasoning)| AgentVerdictWire { pass, reasoning }),
                    usage: output.usage.clone(),
                    files: output.files.clone(),
                    tools: output.tools.clone(),
                    governed: output.governed,
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
                let agent_verdict = task.agent_verdict.map(|v| (v.pass, v.reasoning));
                // Reach the actor via the command channel (self_tx write-back). Gate cursor advance
                // on the ack so a crash between dequeue and commit leaves the cursor behind for
                // redelivery (T7-g invariant). A closed channel ⇒ actor is gone → exit.
                let (ack_tx, ack_rx) = std::sync::mpsc::sync_channel::<()>(0);
                if tx
                    .send(Command::ApplyStepResult {
                        output,
                        agent_verdict,
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
        for s in [StepStatus::Ok, StepStatus::Failed, StepStatus::Cancelled] {
            assert_eq!(status_from_str(status_to_str(s)), s);
        }
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
        };

        // Arm the publisher on THIS thread, publish, then disarm (thread-local is per-thread).
        assert!(arm_exec_publisher(&bus_path), "arm publisher");
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
                db_path: "/abs/estate.db".into(),
                code_graph_db: Some("/abs/repo/.codegraph/estate.db".into()),
            }),
            prior_outputs: vec![],
            elicitation_epoch: 0,
            process_gen: None,
            launch_seq: 0,
        };

        assert!(arm_exec_publisher(&bus_path), "arm publisher");
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
            Some("/abs/repo/.codegraph/estate.db"),
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
            acp: None,
            capabilities: None,
        }
    }

    /// C1 (self-grading in the real path): the agent judge must NOT be dispatched under the seat that
    /// WROTE the work. `run_unit_and_judge` computes the work author from the unit's `assigned_cli` and
    /// excludes BOTH it and the deterministic author when selecting the judge seat. Proven via a recording
    /// stub that captures the assigned_cli of every dispatched unit — the LAST is the judge.
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
        let (_out, verdict) = run_unit_and_judge_with_roster(&runner, &input, None, noop, &roster3);
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

        // 2-seat roster [claude, agy]: both identities excluded ⇒ documented fallback (no explicit seat).
        let rec2 = Arc::new(RecordingRunner::default());
        let runner2: Arc<dyn StepRunner> = rec2.clone();
        let roster2 = vec![
            seat("claude", "claude -p {PROMPT}"),
            seat("agy", "agy run {PROMPT}"),
        ];
        let _ = run_unit_and_judge_with_roster(&runner2, &input, None, noop, &roster2);
        let seen2 = rec2.seen.lock().unwrap();
        assert_eq!(
            seen2.last().cloned().flatten(),
            None,
            "no distinct seat ⇒ fallback carries no explicit seat (and is NOT agy)"
        );
        drop(seen2);
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
            launch_seq: 5,                           // will be stripped below
        };
        // Publish a dispatched task to a real bus db so we can round-trip through serde.
        let bus_path = tmp_bus("t7a");
        assert!(arm_exec_publisher(&bus_path), "arm publisher");
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
        };
        assert!(arm_exec_publisher(&bus_path_a), "arm publisher t38a");
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
                    bus_path_clone,
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
        };
        assert!(arm_exec_publisher(&bus_path_b), "arm publisher t38b");
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
                    bus_path_clone,
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
}
