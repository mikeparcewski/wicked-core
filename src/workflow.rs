//! WORKFLOW — the typed, ordered, resumable, multi-stage run primitive (the orchestrator spine).
//!
//! Stub in P0; built out in P1+ (see `ORCHESTRATOR.md`). This module will own `WorkflowDef`,
//! `StageSpec`, `StageKind` (Recon | AdversarialReview | FunctionalTest | Build | Custom),
//! `GateSpec` (the typed, *clearable* human-confirm gate), `Run`, and `Cursor` — the methodology
//! types live in CORE, keeping wicked-orchestration lane-disjoint (it learns only a generic
//! `AwaitingConfirmation` status, not `StageKind` or governance).
//!
//! ## Locked decision — phase ownership (recorded P0, enforced P1)
//!
//! There is exactly ONE opener of orchestration phases: the run engine's `advance()` step. The
//! execute backend is **phase-pure** — it receives an already-open phase, runs the unit's work, and
//! reports status back via a `StepResult`; it must never call `Phase::open` itself. This removes the
//! double-open collision between `advance()` and the execute path, and it is why the gate-hook drain
//! ([`crate::gate_hook::apply_hook_decisions`]) only *resolves* a gate rather than owning phases (its
//! P0 open-if-absent shim exists solely so a standalone veto is observable before the engine lands).
//!
//! And exactly ONE "which stage" cursor: `Workflow.current_index` in wicked-orchestration. The run's
//! own cursor stores only sub-stage detail (`unit_ix`, `exec_phase`, `attempt`); `stage_ix` is always
//! read from the workflow node — no second cursor, no drift.

use crate::domain::WorkUnit;
use crate::scope::EntityMode;

/// Output from a prior completed unit, injected into the current unit's ACP context so a phase sees
/// the work it builds on. Actor-populated (store read on the actor thread); the worker never queries
/// the store (single-writer invariant).
///
/// Selection is `actor::prior_context_label`: a prior qualifies either because this unit's
/// [`WorkUnit::depends_on`] names its phase (the def's DECLARED handoff — FINDING-024) or because it
/// ran on a DIFFERENT CLI (no conversational state can be shared, so it must be passed explicitly).
/// The two are unioned; the declared path is why this is **no longer cross-CLI-only**.
#[derive(Debug, Clone)]
pub struct PriorUnitOutput {
    /// Human-readable label for the context block. `[codex — unit 2]` for a cross-CLI carry-over;
    /// ``[claude — unit 2 — depends_on `build`]`` when the def declared the dependency, so an
    /// operator can tell the two apart in the transcript without diffing the def.
    pub label: String,
    /// The unit's produced work output.
    pub output: String,
}

/// Everything a worker needs to do one unit's *slow* work, pre-loaded by the actor so the worker
/// holds **no store handle** (the single-writer invariant). In P1 the slow work is the stub; P4a's
/// real backend runs the wrapped-CLI subprocess against `workdir`.
#[derive(Debug, Clone)]
pub struct StepInput {
    pub run_id: String,
    /// Which unit (index into the session's ordered units) this step runs.
    pub unit_ix: usize,
    /// Retry attempt — folded into event ids so a retried step is not deduped as a no-op (P2).
    pub attempt: u32,
    pub unit: WorkUnit,
    pub workflow_id: String,
    pub entity_mode: EntityMode,
    /// The git worktree to run in (set when the run targets a registered repo, P3). `None` ⇒ the
    /// runner uses its own default cwd. The real wrapped-CLI backend (P4a) runs the subprocess here.
    pub workdir: Option<std::path::PathBuf>,
    /// GOVERNANCE OPT-IN (DES-OUTGOV-003 §4). `Some` ⇒ a GOVERNED campaign unit: the wrapped-CLI
    /// launcher injects the `PreToolUse` gate-hook (input governance) + sets the decisions/store env, so
    /// the CLI's tool-calls are governed and a deny folds into the unit gate. `None` ⇒ an UNGOVERNED
    /// invocation — the engine's OWN internal agent-judge / validator-authoring `claude` calls, which
    /// must never self-govern against an empty scope. `None` is the unambiguous ungoverned signal
    /// (distinct from a governed unit that merely has an empty scope).
    pub governance: Option<GovernanceContext>,
    /// Completed outputs from earlier units in the same run, injected as additional prompt blocks by
    /// the ACP runner. Populated when this unit's [`WorkUnit::depends_on`] names the prior's phase
    /// (the DECLARED handoff) **or** the prior ran on a different CLI — see [`PriorUnitOutput`].
    /// Empty for prose-planned runs with a single CLI (nothing declares a dependency and no CLI
    /// boundary is crossed) and for non-ACP fallback paths. Actor-populated so the worker holds no
    /// store handle (single-writer invariant).
    pub prior_outputs: Vec<PriorUnitOutput>,
    // ── ACP elicitation (DES-002) ────────────────────────────────────────────────────────────────
    /// Epoch the actor allocated for ACP elicitation on this unit (`0` = no elicitation, local
    /// path, or non-ACP runner). The `EpochCleanup` RAII guard uses this to call `cleanup_run`
    /// at the right epoch even after a panic.
    pub elicitation_epoch: u64,
    /// Process-generation token minted at `actor::run` entry for this Core instance. Threaded
    /// through to `DispatchedTask` on the bus so the bus consumer can discard completions that
    /// belong to a different Core restart (stale-result guard).
    pub process_gen: Option<uuid::Uuid>,
    /// Per-run monotonic launch sequence number. Incremented at every `begin_launch` call.
    /// Used together with `process_gen` as the bus dedup key and stale-completion guard.
    pub launch_seq: u64,
}

/// The governance context threaded to a GOVERNED wrapped-CLI unit (DES-OUTGOV-003 §4). Carries the
/// store paths the worker cannot derive from a [`StepInput`] (the worker holds no store handle); the
/// hook's `scope` (`resolve_scope`), `phase` (`unit-{ord}`), and the decisions-log path are all derived
/// from the `StepInput`'s own fields. `Serialize`/`Deserialize` so it survives the exec-mediation bus
/// round-trip on `DispatchedTask`.
///
/// The two paths are DIFFERENT STORES ON PURPOSE and must never be collapsed back into one
/// (FINDING-067). `db_path` is the platform's operational store — every run, unit, phase, policy and
/// repo registration in it — and only the gate-hook reads it, through `open_store_ro`. The worker's own
/// tools get `code_graph_db`, a per-repo graph they may freely write. Handing a worker a writable handle
/// to `db_path` cost the platform its entire operational state once already: a governed worker asked to
/// recon its repo pointed the estate indexer at the store it had been given, and the indexer's
/// delete-sweep — "remove nodes whose file is no longer on disk" — deleted all 833 operational nodes,
/// since `agent_session/<id>` and friends are synthetic locations that never existed on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GovernanceContext {
    /// ABSOLUTE filesystem path of the estate SQLite store the gate-hook subprocess opens to read
    /// policies (never `:memory:` — an in-memory store cannot cross processes — nor `postgres://`,
    /// which the SQLite-only hook cannot open; the launcher only sets this for a file-backed store).
    ///
    /// GATE-HOOK ONLY. Never hand this path to anything the worker controls.
    pub db_path: String,
    /// ABSOLUTE path of the code graph the worker's own estate MCP server opens — the one store it is
    /// allowed to write. `None` ⇒ the run targets no registered repo, or that repo has never been
    /// indexed, and the launcher then injects NO estate MCP at all rather than substituting
    /// [`Self::db_path`] or pointing at a database nothing has written (FINDING-069).
    ///
    /// TWO STORES CAN APPEAR HERE, and `actor::run_code_graph_db` chooses between them:
    /// - the REPO-LOCAL graph (`<repo_root>/.codegraph/estate.db`, spelled once at
    ///   `code_graph::CODE_GRAPH_DB_REL`) — the default, and the only one before crew#326;
    /// - the run's PROJECT graph — one co-located database holding every member repo — when the
    ///   launcher bound one and the engine could vouch for it (`actor::project_code_graph_db`).
    ///
    /// The project graph is SHARED: every concurrent run in the project opens the same file, and
    /// this handle is writable. That is a wider blast radius than the repo-local case, where the
    /// worst a worker could do to a graph was to its own repo's — see the write-scope note on
    /// `execute_wrapped::repo_estate_mcp_parts`.
    ///
    /// `#[serde(default)]` so a `DispatchedTask` serialized by an older peer still deserializes — as
    /// `None`, i.e. no estate tools, which is the safe reading of "this peer never told me a repo".
    #[serde(default)]
    pub code_graph_db: Option<String>,
    /// ADDITIONAL absolute write roots the LAUNCHER declared for this run's deliverables (core#259)
    /// — e.g. wicked-crew's interactive-draft inbox, where the workflow's contract says the output
    /// file must land. Joined AFTER the unit cwd into `WICKED_WRITE_ROOTS` by the wrapped-CLI
    /// launcher, so `boundary_denial` admits the declared inbox and nothing else. Validated at
    /// launch by [`crate::path_policy::validate_extra_write_roots`]: every root must be absolute
    /// and outside the engine's own config/pin tree — a launcher-declared root must never reopen
    /// the FINDING-098 pin-rewrite escape. Empty (the default, and the deserialization fallback
    /// for tasks from older peers) means the boundary stays exactly the unit cwd.
    #[serde(default)]
    pub extra_write_roots: Vec<String>,
    /// ADDITIONAL absolute READ-ONLY roots the LAUNCHER declared for this run (core#294) — the
    /// mirror of [`Self::extra_write_roots`], for grounding a run in content it must NOT be able
    /// to change: a reference repo, a spec directory, a design corpus.
    ///
    /// Joined onto `WICKED_READ_ROOTS` (and the ACP carrier's in-process read set) alongside the
    /// evidence-derived roots, so `boundary_denial` admits READS there and nothing else — a WRITE
    /// into one of these roots is still refused, because [`crate::path_policy::check`] tests a
    /// write against the write list ONLY. That is the whole point: before this existed, the only
    /// launch-declared lever was `extra_write_roots`, so "let the worker read the repo" could only
    /// be spelled "let the worker rewrite the repo".
    ///
    /// Validated at launch by [`crate::path_policy::validate_extra_read_roots`] — the same rules
    /// as the write half. Empty (the default, and the deserialization fallback for tasks from
    /// older peers) means the read boundary stays exactly the evidence-derived assembly.
    #[serde(default)]
    pub extra_read_roots: Vec<String>,
}

/// How a worker step finished. P2 wires `Ok`/`Failed`; `Cancelled` lands with real subprocess kill
/// (P4a). A `Failed` step does NOT silently complete the run — the actor surfaces it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StepStatus {
    #[default]
    Ok,
    Failed,
    Cancelled,
    /// ACP elicitation reached a terminal state without a human response — deadline expiry,
    /// adapter disconnect, or `Err(Disconnected)` on the resolution channel (DES-002).
    /// Distinct from `Failed` (which enters the `FailureTriageReady` path and can produce
    /// `Retry`). `ElicitationFailed` routes directly to the run-terminal path, bypassing triage.
    ElicitationFailed,
}

/// End-of-unit resource usage a runner's `OutputAdapter` parsed from a CLI's
/// structured output (DES-STUDIO-COCKPIT-001 §3 B3). `cost_usd` is `Some` when the CLI reports cost
/// directly (claude's `total_cost_usd`) or a price table resolves it, else `None`. Mid-stream totals are
/// out of scope — this is the end-of-run total. `Serialize`/`Deserialize` so it survives the exec-mediation
/// bus round-trip (`CompletedTask`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// TOTAL input tokens presented to the model — fresh + cache-read + cache-creation. This is the
    /// number `cost_usd` is billed against (FINDING-058); the cache split below is a breakdown OF it,
    /// not additional tokens.
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Of `input_tokens`, how many were served from the prompt cache (a cache READ — the cheap part).
    /// `0` when the adapter reports no split (e.g. a usage_update notification carries only totals);
    /// the authoritative split arrives on the prompt result. Broken out so per-unit cost is
    /// attributable to cache reuse vs fresh work (FINDING-012).
    #[serde(default)]
    pub cache_read_tokens: u64,
    /// Of `input_tokens`, how many were spent WRITING the cache this turn (cache creation — billed at
    /// a premium). `0` when unreported. See [`Usage::cache_read_tokens`].
    #[serde(default)]
    pub cache_creation_tokens: u64,
    pub cost_usd: Option<f64>,
}

/// The result of a worker step — the unit's produced output. Posted back to the actor, which is the
/// only thing that writes it to the store.
#[derive(Debug, Clone)]
pub struct StepOutput {
    pub run_id: String,
    pub unit_ix: usize,
    pub attempt: u32,
    pub output: String,
    /// Whether the step succeeded. A worker signals failure here instead of encoding it in `output`.
    pub status: StepStatus,
    /// End-of-unit token/cost usage when the runner's adapter parsed it (claude stream-json); `None` for
    /// passthrough seats (DES-STUDIO-COCKPIT-001 §3 B3). Additive — the actor emits `CliUsage` when present.
    pub usage: Option<Usage>,
    /// Data files the unit's CLI touched, from `tool_use` file paths (B4). Empty for passthrough seats.
    pub files: Vec<String>,
    /// Tool NAMES the unit's CLI invoked (`tool_use.name` — Read/Bash/Edit/…), for per-tool
    /// observability (FINDING-046): the actor emits `ToolInvoked` from these for governed AND
    /// ungoverned units, closing the "N tool calls, zero events / operator blind" gap. Empty for
    /// passthrough seats and non-tool runners.
    pub tools: Vec<String>,
    /// Whether the RUNNER armed input governance for this unit (wrote the decisions-log armed marker). Set
    /// only by the wrapped-CLI runner when it injects the PreToolUse gate-hook; `false` for stub/test
    /// runners and ungoverned units. The actor-side fold uses THIS (not unit properties) to decide whether
    /// a missing/erased decisions log is a tamper DENY — so a claude-assigned STUB unit is never
    /// false-denied for a log the wrapped runner never wrote (DES-OUTGOV-003 evidence integrity).
    pub governed: bool,
}

/// A human's decision at a confirm gate. The gate is *steering*, not just bless-or-bounce: `Approve`
/// can carry an `amend` that is appended to the next unit's instruction (redirect the work).
#[derive(Debug, Clone)]
pub enum HumanDecision {
    /// Proceed; optionally inject an amendment into the next unit's instruction.
    Approve { amend: Option<String> },
    /// Stop the run here (treated as a cancellation).
    Reject,
}

/// Produces a unit's work output **off the actor thread**. The stub returns deterministic text;
/// P4a's impl runs the real wrapped-CLI subprocess. Injectable (like the council `Dispatcher`) so
/// the actor protocol — off-thread dispatch, the in-flight guard, resume-from-cursor — is testable
/// without real subprocesses.
/// A sink the runner calls with incremental output chunks (lines) AS the unit runs — the live-output
/// transport. Thread-safe so the runner's concurrent stdout/stderr drains can both push through it.
pub type DeltaSink = dyn Fn(&str) + Send + Sync;

pub trait StepRunner: Send + Sync {
    fn run_unit(&self, input: &StepInput) -> StepOutput;

    /// Run a unit while STREAMING incremental output through `emit` (live output). The default ignores
    /// `emit` and delegates to [`run_unit`](StepRunner::run_unit), so non-streaming runners (the stub +
    /// every test runner) need no change; the real wrapped-CLI runner overrides this to push stdout as
    /// the subprocess produces it.
    fn run_unit_streaming(&self, input: &StepInput, _emit: &DeltaSink) -> StepOutput {
        self.run_unit(input)
    }

    /// Called by the actor when a run reaches a terminal state (Completed / Failed / Cancelled).
    /// Runners that keep persistent sessions (ACP, PTY) close/kill them here so the CLI process
    /// exits cleanly and does not leak. The default is a no-op — stateless runners need no change.
    /// Fire-and-forget: implementations must not block the actor thread. Swallow channel-closed
    /// errors silently — the run is already gone; a send failure is expected and harmless.
    fn on_run_complete(&self, _run_id: &str) {}

    /// Queue an operator message for delivery on the run's NEXT matching unit prompt (the
    /// inject path for runs with no live PTY to write into — i.e. every ACP-backed run).
    /// Returns `true` when the runner accepted the message; the default declines, so
    /// stateless/stub runners keep the historical skip-with-warning behaviour.
    fn queue_operator_message(
        &self,
        _run_id: &str,
        _target: &crate::command::InjectTarget,
        _message: &str,
    ) -> bool {
        false
    }

    /// Close the session for a specific CLI within a run (called by `ReassignUnit` before
    /// re-dispatching to a different CLI). Callers:
    /// - `AcpStepRunner`: drops the `(run_id, cli_key)` ACP child process (spawns a background
    ///   thread because kill+wait may block).
    /// - `PersistentStepRunner`: removes the stale `run_id → terminal_id` cache entry so the
    ///   next dispatch opens a fresh PTY rather than reusing the now-closed terminal id. The
    ///   terminal itself is already killed by the actor via `finish_terminal` before this is called.
    ///
    /// The default no-op is correct for stateless runners (e.g. `WrappedCliStepRunner`).
    /// Must be fire-and-forget — never block the actor thread.
    fn close_cli_session(&self, _run_id: &str, _cli_key: &str) {}
}

/// The deterministic stub step — today's composition behavior (output = `stub-output for <desc>`),
/// moved behind the [`StepRunner`] seam unchanged.
pub struct StubStepRunner;

impl StepRunner for StubStepRunner {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        StepOutput {
            run_id: input.run_id.clone(),
            unit_ix: input.unit_ix,
            attempt: input.attempt,
            output: format!("stub-output for {}", input.unit.description),
            status: StepStatus::Ok,
            usage: None,
            files: Vec::new(),
            tools: Vec::new(),
            governed: false,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────────────────────
// WorkflowDef — workflows as DATA (DES-EXEC-001 §4, Law 2: capability lands as data + registration,
// never a core edit). A `WorkflowDef` is an ordered list of `PhaseDef`s; every field the reducer
// needs to drive a phase (its gate policy, whether it runs code, whether it needs verified evidence,
// its role for the evaluator≠creator split, its dependencies) is DATA on the phase. The reducer
// branches on these fields — never on the workflow `id` and never on a closed `match` over a phase
// name. Adding a built-in (feature/bug/migration/onboarding/collab, below) or a new workflow is a
// data value, not a core change.
// ─────────────────────────────────────────────────────────────────────────────────────────────

use crate::domain::StageKind;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Where a phase's gate sits in the value→strategy→execution ladder (DES-EXEC-001 §3; gcp-sdlc's
/// three gate positions). `None` = an ungated (e.g. setup/advisory) phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateType {
    /// Post-clarify: is the problem clear, scoped, testable?
    Value,
    /// Post-design: is the approach sound + testable?
    Strategy,
    /// Post-build: does the work meet the bar (quality, coverage, risk)?
    Execution,
}

/// A condition on a conditional human gate — evaluated from the phase's computed verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateCond {
    /// Only stop for a human when the phase verdict is not PASS (auto-advance on PASS).
    VerdictNotPass,
}

/// The confirm policy for a phase — demotes the run-level `HumanConfirm` enum into per-phase DATA so
/// a workflow declares its own gates. The engagement dial (just-finish|balanced|ask-first) may
/// select WHO confirms but NEVER the verdict — and it can never downgrade an `unconditional` gate
/// (e.g. migration cutover). (DES-EXEC-001 §3, the cardinal invariant of all three priors.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GateSpec {
    /// No human — the computed verdict advances or rejects.
    #[default]
    Auto,
    /// Always require a human to confirm.
    ///
    /// `unconditional`: RESERVED / not yet read by the engine (seam finding #10). It is the marker for
    /// a gate the engagement dial (just-finish|balanced|ask-first) must never downgrade — e.g. the
    /// migration `cutover`. Until the engagement dial lands, EVERY `HumanConfirm` pauses regardless of
    /// this flag (`should_pause` / the terminal-gate check match `HumanConfirm { .. }`), so the flag is
    /// authored-but-inert: it does NOT currently strengthen or weaken any gate. Do not mistake it for an
    /// active control — it records intent for the dial, nothing more, today.
    HumanConfirm { unconditional: bool },
    /// Require a human only when the condition holds (else auto-advance).
    HumanConfirmIf(GateCond),
}

/// Which side of the evaluator≠creator split a phase plays. The Evaluator phase runs under a seat
/// distinct from the Creator's and reads the creator's `work_output` as cold evidence
/// (DES-EXEC-001 §3/§4.1). `Neutral` = neither (setup/plan/advisory).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PhaseRole {
    #[default]
    Neutral,
    /// Does the work whose output is later reviewed.
    Creator,
    /// Reviews the creator's output cold (a real, seat-distinct second run).
    Evaluator,
}

/// How a phase executes: via a council-routed CLI agent, or a direct tool command.
/// `Agent` is the default (preserves all existing behaviour); `Tool` bypasses the council
/// and runs `cmd` as a subprocess with the session's `workdir` as the working directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PhaseExecutor {
    #[default]
    Agent,
    Tool {
        cmd: Vec<String>,
    },
}

/// LAUNCH PREFLIGHT for Tool-executor phases (core#120). A `PhaseExecutor::Tool` phase is a
/// deterministic dependency: if its binary cannot be resolved, the run must REFUSE TO START —
/// loudly, at launch — rather than let the unit fall through to agent dispatch where a CLI
/// improvises plausible prose and the expected artifact silently never materialises.
pub fn preflight_tool_phases(def: &WorkflowDef) -> anyhow::Result<()> {
    let mut missing: Vec<String> = Vec::new();
    for phase in &def.phases {
        if let PhaseExecutor::Tool { cmd } = &phase.executor {
            match cmd.first() {
                None => missing.push(format!("phase '{}' has an empty tool cmd", phase.id)),
                Some(bin) if !tool_binary_resolves(bin) => missing.push(format!(
                    "phase '{}' requires tool '{bin}' (not found on PATH or at that path)",
                    phase.id
                )),
                Some(_) => {}
            }
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        anyhow::bail!(
            "workflow '{}' cannot start — unresolved tool dependencies: {}. Install the missing tool(s) or fix PATH, then relaunch.",
            def.id,
            missing.join("; ")
        )
    }
}

/// Resolve a tool binary the way `Command::new` will: an explicit path must exist as a file; a
/// bare name must resolve on `PATH` via the shared PATHEXT-aware [`crate::validator::find_on_path`].
fn tool_binary_resolves(bin: &str) -> bool {
    // The engine's OWN binary: `run_tool_cmd` resolves `wicked-core` via `resolve_wicked_core_exe`
    // ($WICKED_CORE_EXE → current_exe → PATH → bare) at exec time, not as a literal `wicked-core` on
    // PATH. Report it resolvable IFF the engine can actually LOCATE itself — the `_opt` form, which
    // EXCLUDES the bare-name last resort. This keeps the core#120 fail-loud-at-launch guard honest: a
    // napi addon under `node` with no $WICKED_CORE_EXE and no `wicked-core` on PATH refuses to START
    // rather than reaching the domain-graph phase and failing at dispatch (Copilot review, #237/#238).
    // Under `cargo test`, current_exe is a real (non-`node`) binary, so this still resolves.
    if bin == "wicked-core" {
        return crate::execute_wrapped::resolve_wicked_core_exe_opt().is_some();
    }
    let p = std::path::Path::new(bin);
    if p.components().count() > 1 || p.is_absolute() {
        return p.is_file();
    }
    crate::validator::find_on_path(bin).is_some()
}

/// One ordered phase of a workflow — pure DATA the reducer dispatches on.
/// `deny_unknown_fields`: a misspelled key in a drop-in JSON is a loud parse error (naming the
/// file), never a silently-dropped default — matching the workflows/README.md contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhaseDef {
    /// Phase id, unique within the workflow (referenced by `depends_on`). The ONLY required field
    /// in a drop-in JSON file — everything below defaults, so the minimal phase is `{"id":"x"}`.
    pub id: String,
    /// The methodology badge (demoted from the classifier — declared, not guessed). Default: `build`.
    #[serde(default)]
    pub kind: StageKind,
    /// Per-phase INSTRUCTIONS the planner folds into this phase's unit description — i.e. into the
    /// worker's prompt (FINDING-011). Without this a multi-phase workflow's prompts differ only by
    /// the phase-id token (`plan_from_def` builds `<phase> — <intent>`), so N recon phases run N
    /// near-identical surveys with nothing telling each one what ITS slice of the work is.
    /// `None` (the default) keeps the historical prompt shape. Authored as data, like every other
    /// field here — the reducer never branches on the phase id to special-case a prompt.
    /// `skip_serializing_if`: an absent option stays absent on the wire, so defs authored before
    /// this field serialize back byte-identical (the shipped mirrors don't gain `null`s).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// Where this phase's gate sits in the ladder (`None` = ungated).
    #[serde(default)]
    pub gate_type: Option<GateType>,
    /// The confirm policy for this phase's gate. Default: `auto` (no human pause).
    #[serde(default)]
    pub gate: GateSpec,
    /// Whether this phase runs code (drives worktree provisioning + code-tool mode).
    #[serde(default)]
    pub executes_code: bool,
    /// Whether the phase verdict requires re-verified evidence (re-run the pinned verifier).
    ///
    /// ENFORCED AT REGISTRATION (FINDING-055). The only mechanism that re-verifies anything is a
    /// [`validator_pin`](PhaseDef::validator_pin) — `attach_pinned_validators` loads it and the
    /// gate re-runs it (layers 1+2). This field has no other reader, so a phase declaring the flag
    /// with no pin was a control that looked armed and gated nothing (`feature`'s `test` phase
    /// shipped exactly that way). [`WorkflowRegistry::register`] therefore pins the built-in
    /// evidence floor onto any `verified_evidence` phase that names no validator of its own —
    /// see [`enforce_verified_evidence`].
    #[serde(default)]
    pub verified_evidence: bool,
    /// Deliverables that MUST be present for the structural gate check (fail-closed if missing).
    #[serde(default)]
    pub required_deliverables: Vec<String>,
    /// Phase ids in the same workflow that must complete before this one (intra-workflow DAG).
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// The evaluator≠creator role this phase plays. Default: `neutral`.
    #[serde(default)]
    pub role: PhaseRole,
    /// Optional skill to drive the phase (DES-EXEC-001 §4.1 — the headless `/wicked-testing-<skill>`
    /// invocation). `None` = the authored-prompt path.
    #[serde(default)]
    pub skill_ref: Option<String>,
    /// The runtime skill ALLOWLIST for this phase's agent (DES-EXEC-001 §4.2) — the set of skills the
    /// invocation may load, passed as its tool/skill scope (the `--allowedTools` analog). Empty ⇒ no
    /// extra scoping beyond `skill_ref`. Least-privilege per phase, and pure DATA.
    #[serde(default)]
    pub allowed_skills: Vec<String>,
    /// The content-hash [`pin`](crate::validator_vault::pin) of an ALREADY-APPROVED deterministic
    /// validator sitting in the [validator vault](crate::validator_vault) (authored + approved OUT OF
    /// BAND via `provision_validator` → `approve_and_store` — an LLM authoring step that never runs on
    /// the actor thread). When present, the planner LOADS that validator (a pure store read, no LLM) and
    /// attaches it to the phase's unit, so the rev0.4 dual-validator gate ENGAGES: the deterministic
    /// re-verify + agent judge fire against this criterion. Loading is fail-closed — a pin that does not
    /// resolve in the vault is a misconfiguration and the run bails rather than silently running ungated.
    /// `None` (the default) ⇒ the phase runs ungated by a pinned validator (the pre-gate behavior).
    #[serde(default)]
    pub validator_pin: Option<String>,
    /// How this phase executes. `Agent` (default) = council routes to a CLI; `Tool` = run `cmd`
    /// directly in the session workdir, no council. `#[serde(default)]` for back-compat with
    /// existing workflow JSON files that omit this field.
    #[serde(default)]
    pub executor: PhaseExecutor,
}

impl PhaseDef {
    /// A minimal phase: id + kind, no gate, no code, neutral role. `pub(crate)` so sibling modules'
    /// tests (e.g. the planner's) can author fixture phases without a JSON detour.
    pub(crate) fn new(id: &str, kind: StageKind) -> Self {
        PhaseDef {
            id: id.to_string(),
            kind,
            instructions: None,
            gate_type: None,
            gate: GateSpec::Auto,
            executes_code: false,
            verified_evidence: false,
            required_deliverables: Vec::new(),
            depends_on: Vec::new(),
            role: PhaseRole::Neutral,
            skill_ref: None,
            allowed_skills: Vec::new(),
            validator_pin: None,
            executor: PhaseExecutor::default(),
        }
    }
    fn gate(mut self, gt: GateType, spec: GateSpec) -> Self {
        self.gate_type = Some(gt);
        self.gate = spec;
        self
    }
    fn codes(mut self) -> Self {
        self.executes_code = true;
        self
    }
    fn verified(mut self) -> Self {
        self.verified_evidence = true;
        self
    }
    fn role(mut self, r: PhaseRole) -> Self {
        self.role = r;
        self
    }
    /// Pin the shipped deterministic evidence floor onto this phase (FINDING-025 item 1). Engages
    /// gate layers 1 AND 2 — layer 2 (the agent judge) is keyed off the same `Option` layer 1 is,
    /// so a phase with no pin has neither.
    ///
    /// `builtin_floors::tests::the_floor_is_pinned_exactly_where_a_diff_is_the_evidence` asserts
    /// the placement in BOTH directions — which phases carry the floor and which must not — so
    /// neither a new Evaluator shipping ungated nor the floor spreading to a phase that writes no
    /// code can land silently.
    ///
    /// Safe to embed as data because `pipeline::pre_distribute` seeds the pin into the vault
    /// immediately before `attach_pinned_validators` reads it (`builtin_floors::seed_builtin_floors`,
    /// idempotent and content-addressed). `pre_distribute` — not the `plan_and_distribute` wrapper —
    /// because that is what the actor calls directly, so it is the one function BOTH entries reach.
    /// The seed is on the PLAN path, not just at actor boot, because `attach_pinned_validators` is
    /// fail-closed on an unresolvable pin and `run_session` is a public entry point that never
    /// constructs an actor.
    fn evidence_floor(mut self) -> Self {
        self.validator_pin = Some(crate::builtin_floors::EVIDENCE_FLOOR_PIN.to_string());
        self
    }
    fn after(mut self, dep: &str) -> Self {
        self.depends_on.push(dep.to_string());
        self
    }
    #[cfg_attr(not(test), allow(dead_code))]
    fn skill(mut self, skill_ref: &str, allowed: &[&str]) -> Self {
        self.skill_ref = Some(skill_ref.to_string());
        self.allowed_skills = allowed.iter().map(|s| s.to_string()).collect();
        self
    }
    pub(crate) fn executor(mut self, executor: PhaseExecutor) -> Self {
        self.executor = executor;
        self
    }
}

/// A workflow — an id + an ordered list of phases. Pure data; registered in the [`WorkflowRegistry`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowDef {
    pub id: String,
    pub phases: Vec<PhaseDef>,
}

/// Why a `WorkflowDef` failed validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowDefError {
    Empty,
    DuplicatePhaseId(String),
    UnknownDependency {
        phase: String,
        dep: String,
    },
    /// A phase depends on itself or a later-declared phase. Declaration order IS the execution order
    /// (the planner assigns `ord` from the phase index), so every dependency must point *backward*.
    /// This makes the Vec order a valid topological order — and any genuine cycle necessarily shows
    /// up here as a forward edge, so it subsumes cycle detection.
    ForwardDependency {
        phase: String,
        dep: String,
    },
}

impl std::fmt::Display for WorkflowDefError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkflowDefError::Empty => write!(f, "workflow has no phases"),
            WorkflowDefError::DuplicatePhaseId(p) => write!(f, "duplicate phase id: {p}"),
            WorkflowDefError::UnknownDependency { phase, dep } => {
                write!(f, "phase {phase} depends on unknown phase {dep}")
            }
            WorkflowDefError::ForwardDependency { phase, dep } => write!(
                f,
                "phase {phase} depends on {dep}, which is not declared before it \
                 (declaration order must be execution order — dependencies point backward)"
            ),
        }
    }
}
impl std::error::Error for WorkflowDefError {}

impl WorkflowDef {
    /// Validate: non-empty, unique phase ids, every `depends_on` resolves, and the depends-on graph
    /// is acyclic (Kahn) — the same discipline the Campaign DAG enforces on nodes.
    pub fn validate(&self) -> Result<(), WorkflowDefError> {
        if self.phases.is_empty() {
            return Err(WorkflowDefError::Empty);
        }
        let mut ids: HashSet<&str> = HashSet::new();
        for p in &self.phases {
            if !ids.insert(p.id.as_str()) {
                return Err(WorkflowDefError::DuplicatePhaseId(p.id.clone()));
            }
        }
        // Declaration order IS execution order: the planner assigns `ord` from the phase index, so
        // every dependency must reference an EARLIER phase. This one position check does three jobs —
        // resolves deps (unknown → error), forbids self/forward edges, and thereby guarantees the Vec
        // order is a valid topological order, so a genuine cycle can't even be expressed (it would
        // need a forward edge). Listing the same backward dep twice is harmless and accepted.
        let pos: HashMap<&str, usize> = self
            .phases
            .iter()
            .enumerate()
            .map(|(i, p)| (p.id.as_str(), i))
            .collect();
        for (i, p) in self.phases.iter().enumerate() {
            for d in &p.depends_on {
                match pos.get(d.as_str()) {
                    None => {
                        return Err(WorkflowDefError::UnknownDependency {
                            phase: p.id.clone(),
                            dep: d.clone(),
                        })
                    }
                    Some(&dp) if dp >= i => {
                        return Err(WorkflowDefError::ForwardDependency {
                            phase: p.id.clone(),
                            dep: d.clone(),
                        })
                    }
                    Some(_) => {}
                }
            }
        }
        Ok(())
    }
}

/// The registry of known workflows — id → def. `with_defaults()` seeds the built-ins
/// (feature/bug/migration/onboarding/collab).
/// Registering a new workflow is a data insert (Law 2); the reducer only ever `get`s a def.
#[derive(Debug, Clone, Default)]
pub struct WorkflowRegistry {
    defs: HashMap<String, WorkflowDef>,
}

impl WorkflowRegistry {
    /// The built-in workflows (feature/bug/migration/onboarding/collab), each validated at
    /// construction.
    pub fn with_defaults() -> Self {
        let mut r = WorkflowRegistry::default();
        for def in [
            feature_def(),
            bug_def(),
            migration_def(),
            onboarding_def(),
            collab_def(),
        ] {
            r.register(def).expect("built-in workflow defs are valid");
        }
        r
    }
    /// Register (or replace) a workflow. Validates before inserting.
    ///
    /// A replacement may change anything about a workflow EXCEPT quietly ungating it — see
    /// [`carry_shadowed_pins`](WorkflowRegistry::carry_shadowed_pins) — and a phase that declares
    /// `verified_evidence` is armed with a real verifier — see [`enforce_verified_evidence`].
    /// Order matters between the two: shadowed pins are carried forward FIRST, so a replacement
    /// that dropped a phase-specific pin gets that pin back rather than the generic floor.
    pub fn register(&mut self, def: WorkflowDef) -> Result<(), WorkflowDefError> {
        def.validate()?;
        let def = self.carry_shadowed_pins(def);
        let def = enforce_verified_evidence(def);
        self.defs.insert(def.id.clone(), def);
        Ok(())
    }

    /// A replacement may not silently REMOVE a `validator_pin` the definition it shadows carried.
    /// Where the incoming phase has no pin and the shadowed one did, the pin is carried forward and
    /// the substitution is announced.
    ///
    /// This exists because replacement is silent and the loss is invisible. `register` overwrites by
    /// id, `load_dir` runs AFTER `with_defaults`, and both the drop-in dir and the runtime
    /// `RegisterWorkflow` path funnel through here — so anything re-registering a built-in id
    /// replaces the shipped def wholesale, and a mirror of that def written before the floors existed
    /// takes the gates back out with no error, no warning, and a workflow that still reports the
    /// right id and phases. Observed exactly that way: `~/.config/wicked-core/workflows/feature.json`
    /// written by a consumer's hand-transcribed copy, `adversarial-review` role `evaluator`,
    /// `validator_pin: null` — the evidence floor shipped in the built-in and never engaged for the
    /// one caller that matters.
    ///
    /// A gate is a floor, so the safe direction is unambiguous: keep it. Everything else in the
    /// replacement still applies, which is what a legitimate shadow is actually for (the onboarding
    /// drop-in, for instance, exists to bake runtime-resolved executor paths). Deliberately dropping
    /// a gate is still available — under a NEW id, which is what `gate-phase` already does and what
    /// the warning points at. Changing a pin is untouched: only a phase with NO pin inherits one.
    fn carry_shadowed_pins(&self, mut def: WorkflowDef) -> WorkflowDef {
        let Some(shadowed) = self.defs.get(&def.id) else {
            return def;
        };
        for phase in def.phases.iter_mut() {
            if phase.validator_pin.is_some() {
                continue;
            }
            let Some(pin) = shadowed
                .phases
                .iter()
                .find(|p| p.id == phase.id)
                .and_then(|p| p.validator_pin.as_deref())
            else {
                continue;
            };
            eprintln!(
                "wicked-core: workflow `{}` was re-registered without the validator pin that phase \
                 `{}` carried ({}); KEEPING the pin — a replacement may change a gate but not \
                 silently remove one. To run this phase ungated, register under a different id.",
                def.id, phase.id, pin
            );
            phase.validator_pin = Some(pin.to_string());
        }
        def
    }
    /// Overwrite one phase's `validator_pin` on a registered def. Returns false when the workflow or
    /// phase is absent.
    ///
    /// This corrects an INSTALLED def that disagrees with the binary about a pin the binary owns
    /// (wicked-core#186). Repair rather than removal is deliberate: `register` overwrites by id, so
    /// removing leaves NO def behind — there is no shadowed built-in to fall back to, and a drop-in
    /// like `domain-extraction` has no compiled form at all. Removal would trade a wrong gate for an
    /// unknown-workflow failure.
    pub fn repin(&mut self, id: &str, phase_id: &str, pin: &str) -> bool {
        let Some(def) = self.defs.get_mut(id) else {
            return false;
        };
        let Some(phase) = def.phases.iter_mut().find(|p| p.id == phase_id) else {
            return false;
        };
        phase.validator_pin = Some(pin.to_string());
        true
    }

    pub fn get(&self, id: &str) -> Option<&WorkflowDef> {
        self.defs.get(id)
    }
    pub fn ids(&self) -> Vec<String> {
        let mut v: Vec<String> = self.defs.keys().cloned().collect();
        v.sort();
        v
    }

    /// Overlay every `*.json` workflow file in `dir` (non-recursive) onto this registry, validating
    /// and registering each in filename order. A file whose `id` matches a built-in REPLACES it, so
    /// operators tune the shipped workflows and add new ones by dropping a data file — no recompile,
    /// no edit to this crate (the Law-2 seam). A missing `dir` is `Ok(vec![])` (nothing to overlay).
    ///
    /// **Resilient per-file:** a malformed or invalid file is SKIPPED with a warning naming it (so one
    /// bad drop-in can't disable every other one) — not a hard error that aborts the whole overlay.
    /// The loud, per-file error is available via [`def_from_file`](WorkflowRegistry::def_from_file) for
    /// an explicit `workflow lint`. Returns the ids that loaded, in load order. (A caller that requested
    /// a SPECIFIC workflow still learns if it's missing — see the resolver, which errors on an unknown
    /// requested id rather than silently falling back.)
    pub fn load_dir(&mut self, dir: impl AsRef<Path>) -> anyhow::Result<Vec<String>> {
        let dir = dir.as_ref();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut paths: Vec<_> = std::fs::read_dir(dir)
            .with_context(|| format!("reading workflow dir {}", dir.display()))?
            .filter_map(Result::ok)
            .map(|e| e.path())
            // A regular file (following symlinks) ending in `.json`. Guards against a subdirectory
            // or symlink-to-dir named `x.json`, which would otherwise be read and abort the load.
            .filter(|p| p.is_file() && p.extension().and_then(|s| s.to_str()) == Some("json"))
            .collect();
        paths.sort(); // deterministic load order regardless of filesystem enumeration
        let mut loaded = Vec::new();
        for path in paths {
            let outcome = Self::def_from_file(&path).and_then(|def| {
                let id = def.id.clone();
                self.register(def)
                    .map_err(|e| anyhow::anyhow!("workflow {id} in {}: {e}", path.display()))?;
                Ok(id)
            });
            match outcome {
                Ok(id) => loaded.push(id),
                // Skip the offending file, keep the rest — but loudly (named), never silently.
                // The CAUSE is the whole point: `{e}` renders only anyhow's outermost context,
                // so this line used to print the path twice and no reason. See `diagnostic`.
                Err(e) => eprintln!(
                    "wicked-core: {}",
                    crate::diagnostic::with_cause(
                        &format!("skipping workflow file {}", path.display()),
                        &e
                    )
                ),
            }
        }
        Ok(loaded)
    }

    /// Parse + validate one [`WorkflowDef`] from a JSON file (no registration). Public so a caller
    /// (a CLI `workflow lint`, the studio) can check a drop-in file before committing it.
    pub fn def_from_file(path: impl AsRef<Path>) -> anyhow::Result<WorkflowDef> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading workflow file {}", path.display()))?;
        let def: WorkflowDef = serde_json::from_str(&raw)
            .with_context(|| format!("parsing workflow file {}", path.display()))?;
        def.validate()
            .map_err(|e| anyhow::anyhow!("invalid workflow in {}: {e}", path.display()))?;
        Ok(def)
    }
}

/// A `verified_evidence` phase must be able to DELIVER the re-verification it declares
/// (FINDING-055).
///
/// The flag's contract — "the phase verdict requires re-verified evidence" — is delivered by
/// exactly one mechanism: a [`validator_pin`](PhaseDef::validator_pin), which
/// `attach_pinned_validators` loads and the gate re-runs (layers 1 + 2). The flag itself has no
/// other reader anywhere in the engine, so a phase declaring it with no pin was a control that
/// looked armed and gated nothing — `feature`'s `test` phase shipped exactly that way, while its
/// siblings (`bug`/`verify`, `migration`/`verify`, `domain-extraction`/`coverage`) all pair the
/// flag with a pin.
///
/// Enforced here because [`WorkflowRegistry::register`] is the choke point every def crosses on
/// its way to the engine: `with_defaults` (built-ins), `load_dir` (drop-ins), and the runtime
/// RegisterWorkflow path all funnel through it — the same property `carry_shadowed_pins` leans on.
/// Only `def_from_file` (the explicit lint read) sees a def before this runs.
///
/// The fail-closed direction is to make the declaration TRUE rather than delete it: a flagged
/// phase with no pin of its own gains the built-in evidence floor
/// ([`crate::builtin_floors::EVIDENCE_FLOOR_PIN`] — seeded on the plan path by `pre_distribute`,
/// so the pin always resolves and `attach_pinned_validators` engages). Loudly, like every other
/// registration-time substitution. An author who wants a phase-specific criterion pins their own
/// validator — never overridden here (and `carry_shadowed_pins` runs first, so a shadowed
/// phase-specific pin is restored before this could floor it).
///
/// Opting OUT of re-verification is scoped: dropping the flag runs the phase unverified only on a
/// FRESH id — one with no already-registered def to shadow it. Once a phase has been floored, a
/// SAME-id re-registration that drops the flag does NOT ungate it: `carry_shadowed_pins` runs first
/// and carries the floor forward, because a replacement may change a gate but never silently remove
/// one (the same rule that keeps a hand-transcribed mirror from stripping a shipped gate). The floor
/// is a pin like any other by the time this runs, so it is indistinguishable from an author's pin
/// and inherits that protection. The escape hatch is therefore a new id — exactly the one
/// `carry_shadowed_pins` already documents — not a re-registration.
/// Guarded by `dropping_verified_evidence_keeps_the_floor_on_reregistration_but_a_fresh_id_runs_unverified`.
fn enforce_verified_evidence(mut def: WorkflowDef) -> WorkflowDef {
    for phase in def.phases.iter_mut() {
        if !phase.verified_evidence || phase.validator_pin.is_some() {
            continue;
        }
        eprintln!(
            "wicked-core: workflow `{}` phase `{}` declares verified_evidence but pins no \
             validator; PINNING the built-in evidence floor so the declaration is enforced rather \
             than silently inert (FINDING-055). Pin a phase-specific validator to replace it; to \
             run the phase unverified, drop `verified_evidence` on a FRESH workflow id — a same-id \
             re-registration keeps this floor (carry_shadowed_pins will not silently ungate it).",
            def.id, phase.id
        );
        phase.validator_pin = Some(crate::builtin_floors::EVIDENCE_FLOOR_PIN.to_string());
    }
    def
}

/// `feature` — clarify(value) → design(strategy) → build(execution) → adversarial-review → test → review.
/// Gates: HumanConfirm after clarify + after adversarial-review; HumanConfirmIf(¬PASS) on test.
/// The collaborative-discussion workflow: two (or more) seats argue a design to an
/// outcome. Roles alternate creator/evaluator so evaluator-distinct FORCES the critic
/// onto a different CLI, and cross-CLI context injection carries each side's actual
/// words to the other. Verified to produce genuine dialogue (grounded critique,
/// point-by-point revision, honest verdicts).
pub fn collab_def() -> WorkflowDef {
    WorkflowDef {
        id: "collab".to_string(),
        phases: vec![
            PhaseDef::new("propose", StageKind::Recon)
                .gate(GateType::Value, GateSpec::Auto)
                .role(PhaseRole::Creator),
            PhaseDef::new("critique", StageKind::Review)
                .gate(GateType::Value, GateSpec::Auto)
                .role(PhaseRole::Evaluator)
                .after("propose"),
            PhaseDef::new("revise", StageKind::Recon)
                .gate(GateType::Strategy, GateSpec::Auto)
                .role(PhaseRole::Creator)
                .after("critique"),
            PhaseDef::new("verdict", StageKind::Review)
                .gate(
                    GateType::Value,
                    GateSpec::HumanConfirm {
                        unconditional: false,
                    },
                )
                .role(PhaseRole::Evaluator)
                .after("revise"),
        ],
    }
}

pub fn feature_def() -> WorkflowDef {
    WorkflowDef {
        id: "feature".to_string(),
        phases: vec![
            PhaseDef::new("clarify", StageKind::Recon).gate(
                GateType::Value,
                GateSpec::HumanConfirm {
                    unconditional: false,
                },
            ),
            PhaseDef::new("design", StageKind::Recon)
                .gate(GateType::Strategy, GateSpec::Auto)
                .after("clarify"),
            PhaseDef::new("build", StageKind::Build)
                .gate(GateType::Execution, GateSpec::Auto)
                .codes()
                .role(PhaseRole::Creator)
                .after("design"),
            PhaseDef::new("adversarial-review", StageKind::Review)
                .gate(
                    GateType::Execution,
                    GateSpec::HumanConfirm {
                        unconditional: false,
                    },
                )
                .role(PhaseRole::Evaluator)
                .evidence_floor()
                .after("build"),
            PhaseDef::new("test", StageKind::Test)
                .gate(
                    GateType::Execution,
                    GateSpec::HumanConfirmIf(GateCond::VerdictNotPass),
                )
                .verified()
                .evidence_floor()
                .after("build"),
            PhaseDef::new("review", StageKind::Review)
                .gate(GateType::Execution, GateSpec::Auto)
                .after("test"),
        ],
    }
}

/// `bug` — triage(value) → reproduce(value) → fix(execution) → verify. Reproduce-first: `fix`
/// depends on `reproduce`; a bug is not fixed until the repro goes red→green.
pub fn bug_def() -> WorkflowDef {
    WorkflowDef {
        id: "bug".to_string(),
        phases: vec![
            PhaseDef::new("triage", StageKind::Recon).gate(GateType::Value, GateSpec::Auto),
            PhaseDef::new("reproduce", StageKind::Test)
                .gate(GateType::Value, GateSpec::Auto)
                .after("triage"),
            PhaseDef::new("fix", StageKind::Build)
                .gate(GateType::Execution, GateSpec::Auto)
                .codes()
                .role(PhaseRole::Creator)
                .after("reproduce"),
            PhaseDef::new("verify", StageKind::Test)
                .gate(
                    GateType::Execution,
                    GateSpec::HumanConfirmIf(GateCond::VerdictNotPass),
                )
                .verified()
                .role(PhaseRole::Evaluator)
                .evidence_floor()
                .after("fix"),
        ],
    }
}

/// `migration` — plan(strategy) → execute(execution) → cutover(UNCONDITIONAL human) → verify → cleanup(advisory).
/// `cutover` is the one gate the engagement dial can never downgrade.
pub fn migration_def() -> WorkflowDef {
    WorkflowDef {
        id: "migration".to_string(),
        phases: vec![
            PhaseDef::new("plan", StageKind::Recon).gate(
                GateType::Strategy,
                GateSpec::HumanConfirm {
                    unconditional: false,
                },
            ),
            PhaseDef::new("execute", StageKind::Build)
                .gate(GateType::Execution, GateSpec::Auto)
                .codes()
                .role(PhaseRole::Creator)
                .after("plan"),
            PhaseDef::new("cutover", StageKind::Build)
                .gate(
                    GateType::Execution,
                    GateSpec::HumanConfirm {
                        unconditional: true,
                    },
                )
                .codes()
                .after("execute"),
            PhaseDef::new("verify", StageKind::Test)
                .gate(
                    GateType::Execution,
                    GateSpec::HumanConfirmIf(GateCond::VerdictNotPass),
                )
                .verified()
                .role(PhaseRole::Evaluator)
                .evidence_floor()
                .after("cutover"),
            PhaseDef::new("cleanup", StageKind::Build).after("verify"),
        ],
    }
}

/// `onboarding` — estate indexing pipeline for a registered repo (2 deterministic tool phases).
/// Phases run in the session's `workdir` (the repo root); no council is convened.
/// index → annotate (sequential).
///
/// # What this deliberately does NOT do
///
/// It does not produce `requirements_graph.json`. A third phase used to run `wicked-core
/// domain-graph` here, and it could never succeed: that command gates fail-closed on front-half
/// coverage == 1.0, and coverage is the fraction of behavior-bearing symbols carrying a requirement
/// annotation or risk flag. `wicked-estate clusters --annotate` is CLUSTERING — it does not annotate
/// a single symbol with a requirement. On a real repo (AutoGPT, 42,925 nodes, indexed and annotated
/// by exactly these two phases) the recompute is **0.0000 — 28,885 of 28,885 behavior-bearing nodes
/// unaccounted**. Not "usually short of the bar": nothing had ever been in the numerator.
///
/// So every repo registration ended `sessionFailed` on a phase that was structurally incapable of
/// passing, after the two phases that matter had both succeeded (FINDING-068).
///
/// Coverage comes from the AGENTIC front-half — [`crate::DOMAIN_EXTRACTION_WORKFLOW_ID`], whose
/// `extract` phase writes the annotations and whose `coverage` phase measures them. `domain-graph`
/// is the last phase of THAT workflow, downstream of the four phases that produce its precondition.
/// Onboarding is deterministic tools and no council by construction, so it cannot host any of them.
///
/// The gate is not the defect and must not be relaxed to make this pass: refusing to translate a
/// partially-annotated graph is the design (DES-OUTGOV-001/005), and a domain model built from a
/// 0%-covered graph is a file full of confident nonsense.
/// Placeholder for the run's repo root, substituted per run by [`crate::plan::bind_repo_paths`].
pub const REPO_ROOT_TOKEN: &str = "{repo_root}";
/// Placeholder for the run's engine-resolved code graph, substituted per run.
pub const CODE_GRAPH_DB_TOKEN: &str = "{code_graph_db}";

pub fn onboarding_def() -> WorkflowDef {
    WorkflowDef {
        id: "onboarding".to_string(),
        phases: vec![
            PhaseDef::new("index", StageKind::Recon).executor(PhaseExecutor::Tool {
                cmd: vec![
                    "wicked-estate".to_string(),
                    "index".to_string(),
                    REPO_ROOT_TOKEN.to_string(),
                    "--db".to_string(),
                    CODE_GRAPH_DB_TOKEN.to_string(),
                ],
            }),
            PhaseDef::new("annotate", StageKind::Recon)
                .executor(PhaseExecutor::Tool {
                    cmd: vec![
                        "wicked-estate".to_string(),
                        "clusters".to_string(),
                        "--annotate".to_string(),
                        "--db".to_string(),
                        CODE_GRAPH_DB_TOKEN.to_string(),
                    ],
                })
                .after("index"),
        ],
    }
}

#[cfg(test)]
mod workflow_def_tests {
    use super::*;

    #[test]
    fn registry_seeds_the_builtin_workflows() {
        let r = WorkflowRegistry::with_defaults();
        assert_eq!(
            r.ids(),
            vec!["bug", "collab", "feature", "migration", "onboarding"]
        );
    }

    #[test]
    fn collab_alternates_creator_and_evaluator_roles() {
        let def = collab_def();
        let roles: Vec<_> = def.phases.iter().map(|p| p.role).collect();
        assert_eq!(
            roles,
            vec![
                PhaseRole::Creator,
                PhaseRole::Evaluator,
                PhaseRole::Creator,
                PhaseRole::Evaluator
            ],
            "evaluator-distinct must force the critic/verdict onto a different CLI"
        );
    }

    #[test]
    fn preflight_refuses_a_missing_tool_binary_loudly() {
        let def = WorkflowDef {
            id: "tooly".to_string(),
            phases: vec![
                PhaseDef::new("index", StageKind::Recon).executor(PhaseExecutor::Tool {
                    cmd: vec!["definitely-not-a-real-binary-xyzzy".to_string()],
                }),
            ],
        };
        let err = preflight_tool_phases(&def).unwrap_err().to_string();
        assert!(err.contains("cannot start"), "loud refusal, got: {err}");
        assert!(
            err.contains("definitely-not-a-real-binary-xyzzy"),
            "names the missing tool, got: {err}"
        );
    }

    #[test]
    fn preflight_passes_agent_phases_and_resolvable_absolute_paths() {
        // Agent phases have no tool dependency — always pass.
        let agent_only = WorkflowDef {
            id: "agenty".to_string(),
            phases: vec![PhaseDef::new("explore", StageKind::Recon)],
        };
        assert!(preflight_tool_phases(&agent_only).is_ok());

        // An absolute path that exists resolves (existence check — cross-platform).
        let dir = std::env::temp_dir().join(format!("wicked-preflight-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("toolbin");
        std::fs::write(&file, b"#!/bin/sh\n").unwrap();
        let def = WorkflowDef {
            id: "tooly".to_string(),
            phases: vec![
                PhaseDef::new("index", StageKind::Recon).executor(PhaseExecutor::Tool {
                    cmd: vec![file.to_string_lossy().into_owned()],
                }),
            ],
        };
        assert!(preflight_tool_phases(&def).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn feature_has_the_designed_phase_shape() {
        let def = feature_def();
        let ids: Vec<&str> = def.phases.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "clarify",
                "design",
                "build",
                "adversarial-review",
                "test",
                "review"
            ]
        );
    }

    #[test]
    fn all_builtins_validate() {
        for def in [feature_def(), bug_def(), migration_def()] {
            def.validate()
                .unwrap_or_else(|e| panic!("{} invalid: {e}", def.id));
        }
    }

    /// Onboarding runs the two deterministic phases and stops (FINDING-068).
    ///
    /// A third phase ran `wicked-core domain-graph`, which gates fail-closed on front-half coverage
    /// == 1.0. Neither phase here writes a requirement annotation — `clusters --annotate` is
    /// clustering — so the recompute is 0.0 and the phase could not pass. Measured on AutoGPT after
    /// exactly these two phases: 28,885 of 28,885 behavior-bearing nodes unaccounted. Every repo
    /// registration ended `sessionFailed` after the work that mattered had already succeeded.
    ///
    /// `domain-graph` belongs to `domain-extraction`, downstream of the `extract` + `coverage`
    /// phases that produce its precondition. Do not move it back here to "complete" onboarding, and
    /// do not relax the coverage gate to make it pass — a domain model translated from a 0%-covered
    /// graph is confident nonsense, which is why the gate fails closed.
    #[test]
    fn onboarding_runs_only_what_it_can_actually_finish() {
        let def = onboarding_def();
        def.validate().expect("onboarding is a valid def");
        assert_eq!(
            def.phases.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
            ["index", "annotate"]
        );

        // Stated as "no phase shells out to domain-graph" rather than "no phase named `domain`",
        // because the defect is the COMMAND's unmeetable precondition, not the phase's name.
        for phase in &def.phases {
            if let PhaseExecutor::Tool { cmd } = &phase.executor {
                assert!(
                    !cmd.iter().any(|a| a == "domain-graph"),
                    "onboarding phase `{}` runs `{}`, whose coverage gate no phase in this \
                     workflow can satisfy — see FINDING-068",
                    phase.id,
                    cmd.join(" ")
                );
            }
        }
    }

    #[test]
    fn reducer_can_branch_on_data_not_id() {
        // Law 2 proof: everything the reducer needs is a data field, reachable without matching on
        // the workflow id or a phase name. Here we derive "which phases pause a human" purely from
        // `gate` data — no `if id == "feature"`, no `match phase.id`.
        let def = feature_def();
        let human_phases: Vec<&str> = def
            .phases
            .iter()
            .filter(|p| matches!(p.gate, GateSpec::HumanConfirm { .. }))
            .map(|p| p.id.as_str())
            .collect();
        assert_eq!(human_phases, vec!["clarify", "adversarial-review"]);
    }

    #[test]
    fn evaluator_and_creator_are_distinct_phases() {
        // The evaluator≠creator split is data: build is the Creator, adversarial-review is the
        // Evaluator — a real seat-distinct second phase over the creator's output.
        let def = feature_def();
        let creator = def
            .phases
            .iter()
            .find(|p| p.role == PhaseRole::Creator)
            .unwrap();
        let evaluator = def
            .phases
            .iter()
            .find(|p| p.role == PhaseRole::Evaluator)
            .unwrap();
        assert_eq!(creator.id, "build");
        assert_eq!(evaluator.id, "adversarial-review");
        assert!(evaluator.depends_on.contains(&"build".to_string()));
    }

    #[test]
    fn migration_cutover_is_an_unconditional_gate() {
        let cutover = migration_def()
            .phases
            .into_iter()
            .find(|p| p.id == "cutover")
            .unwrap();
        assert_eq!(
            cutover.gate,
            GateSpec::HumanConfirm {
                unconditional: true
            }
        );
    }

    #[test]
    fn a_cyclic_def_is_rejected() {
        // A 2-cycle (a↔b) can't be laid out backward-only: whichever phase is declared first
        // depends on a later one, surfacing as a forward edge.
        let bad = WorkflowDef {
            id: "cyclic".to_string(),
            phases: vec![
                PhaseDef::new("a", StageKind::Build).after("b"),
                PhaseDef::new("b", StageKind::Build).after("a"),
            ],
        };
        assert!(matches!(
            bad.validate(),
            Err(WorkflowDefError::ForwardDependency { .. })
        ));
    }

    #[test]
    fn a_forward_or_self_dependency_is_rejected() {
        // Forward: "a" (declared first) depends on the later "b".
        let forward = WorkflowDef {
            id: "fwd".to_string(),
            phases: vec![
                PhaseDef::new("a", StageKind::Build).after("b"),
                PhaseDef::new("b", StageKind::Build),
            ],
        };
        assert!(matches!(
            forward.validate(),
            Err(WorkflowDefError::ForwardDependency { .. })
        ));
        // Self-dependency is also a forward edge (dp == i).
        let selfdep = WorkflowDef {
            id: "self".to_string(),
            phases: vec![PhaseDef::new("a", StageKind::Build).after("a")],
        };
        assert!(matches!(
            selfdep.validate(),
            Err(WorkflowDefError::ForwardDependency { .. })
        ));
    }

    #[test]
    fn plan_from_def_carries_skill_and_allowlist_onto_units() {
        // §4.1/§4.2: a phase's skill_ref + runtime allowlist ride onto its unit so the runner invokes
        // the right skill under least-privilege. A phase without a skill leaves both empty (authored path).
        let def = WorkflowDef {
            id: "skilled".to_string(),
            phases: vec![
                PhaseDef::new("build", StageKind::Build)
                    .skill("wicked-testing-execution", &["wicked-testing-authoring"]),
                PhaseDef::new("review", StageKind::Review).after("build"),
            ],
        };
        let units = crate::plan::plan_from_def(&def, "do it", "s");
        assert_eq!(
            units[0].skill_ref.as_deref(),
            Some("wicked-testing-execution")
        );
        assert_eq!(
            units[0].allowed_skills,
            vec!["wicked-testing-authoring".to_string()]
        );
        assert!(
            units[1].skill_ref.is_none(),
            "unskilled phase ⇒ authored path"
        );
        assert!(units[1].allowed_skills.is_empty());
    }

    #[test]
    fn a_backward_dep_listed_twice_is_not_a_false_cycle() {
        // Regression: the old Kahn indeg miscounted duplicate deps and reported a phantom cycle.
        let dup = WorkflowDef {
            id: "dup".to_string(),
            phases: vec![
                PhaseDef::new("a", StageKind::Build),
                PhaseDef {
                    depends_on: vec!["a".to_string(), "a".to_string()],
                    ..PhaseDef::new("b", StageKind::Build)
                },
            ],
        };
        assert_eq!(dup.validate(), Ok(()));
    }

    #[test]
    fn an_unknown_dependency_is_rejected() {
        let bad = WorkflowDef {
            id: "dangling".to_string(),
            phases: vec![PhaseDef::new("a", StageKind::Build).after("ghost")],
        };
        assert!(matches!(
            bad.validate(),
            Err(WorkflowDefError::UnknownDependency { .. })
        ));
    }

    // ---- data-driven registration (Law 2): a workflow is a JSON file, not a code edit ----

    /// A private, collision-free scratch dir for a filesystem test (no tempfile dep; process id +
    /// a per-test tag keep parallel tests disjoint). Best-effort cleanup on drop.
    struct ScratchDir(std::path::PathBuf);
    impl ScratchDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("wicked-wf-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            ScratchDir(dir)
        }
        fn write(&self, name: &str, body: &str) {
            std::fs::write(self.0.join(name), body).unwrap();
        }
    }
    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    #[ignore = "generator: run with --ignored --nocapture to (re)emit the shipped data files"]
    fn emit_builtin_data_files() {
        for def in [feature_def(), bug_def(), migration_def()] {
            println!("===FILE workflows/{}.json===", def.id);
            println!("{}", serde_json::to_string_pretty(&def).unwrap());
        }
    }

    #[test]
    fn shipped_data_files_match_the_seed_builders() {
        // The `workflows/*.json` files are the human-editable, copy-paste mirror of the compiled
        // seed builders. This guard keeps them in lock-step: if a builder changes, regenerate the
        // files (emit_builtin_data_files) — otherwise a non-maintainer reads stale example data.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("workflows");
        for def in [feature_def(), bug_def(), migration_def()] {
            let path = root.join(format!("{}.json", def.id));
            let from_file =
                WorkflowRegistry::def_from_file(&path).unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(
                from_file, def,
                "{} data file drifted from its builder",
                def.id
            );
        }
    }

    #[test]
    fn a_builtin_def_round_trips_through_json() {
        // The wire shape a non-maintainer authors IS the in-memory def: serialize → parse → equal.
        // If this fails, the drop-in JSON contract drifted from the type.
        let def = feature_def();
        let json = serde_json::to_string_pretty(&def).unwrap();
        let back: WorkflowDef = serde_json::from_str(&json).unwrap();
        assert_eq!(def, back);
    }

    #[test]
    fn load_dir_registers_a_dropped_in_workflow_without_touching_code() {
        // A non-maintainer's brand-new workflow, authored as pure data — no Rust, no builder fn.
        let dir = ScratchDir::new("dropin");
        dir.write(
            "spike.json",
            r#"{
                "id": "spike",
                "phases": [
                    { "id": "explore", "kind": "recon" },
                    { "id": "prototype", "kind": "build", "depends_on": ["explore"] }
                ]
            }"#,
        );
        let mut reg = WorkflowRegistry::with_defaults();
        let loaded = reg.load_dir(&dir.0).unwrap();
        assert_eq!(loaded, vec!["spike"]);
        let spike = reg.get("spike").expect("spike registered from data");
        let ids: Vec<&str> = spike.phases.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["explore", "prototype"]);
        // built-ins still present alongside the drop-in
        assert!(reg.get("feature").is_some());
    }

    #[test]
    fn load_dir_lets_a_data_file_override_a_builtin() {
        let dir = ScratchDir::new("override");
        // Same id as a built-in, one phase — replaces the shipped feature workflow.
        dir.write(
            "feature.json",
            r#"{ "id": "feature", "phases": [ { "id": "ship-it", "kind": "build" } ] }"#,
        );
        let mut reg = WorkflowRegistry::with_defaults();
        reg.load_dir(&dir.0).unwrap();
        let feature = reg.get("feature").unwrap();
        assert_eq!(feature.phases.len(), 1);
        assert_eq!(feature.phases[0].id, "ship-it");
    }

    /// FINDING-049. A drop-in that shadows a built-in must not take its gates out.
    ///
    /// This is how the evidence floor was nullified in practice: a consumer wrote a hand-transcribed
    /// mirror of the shipped `feature` def into the overlay dir, every phase `validator_pin: null`.
    /// `load_dir` runs after `with_defaults` and `register` overwrites, so the mirror replaced the
    /// gated built-in — same id, same phases, no gate, no error. The floor shipped and never engaged
    /// for the one caller that mattered.
    ///
    /// The shadow still applies (that is what a drop-in is for); only the REMOVAL of a gate is
    /// refused. Note the phase list here is the mirror's, not the built-in's.
    #[test]
    fn a_drop_in_cannot_silently_ungate_the_builtin_it_shadows() {
        let dir = ScratchDir::new("ungate");
        let base = feature_def();
        let gated = base
            .phases
            .iter()
            .find(|p| p.validator_pin.is_some())
            .expect("a shipped feature phase pins the evidence floor");
        let floor = gated.validator_pin.clone().unwrap();
        let gated_id = gated.id.clone();

        // The mirror: the shadowed phase, plus a phase of its own so the shadow is observably applied
        // and not just ignored. Every pin null, exactly as transcribed.
        dir.write(
            "feature.json",
            &format!(
                r#"{{ "id": "feature", "phases": [
                    {{ "id": "{gated_id}", "kind": "review", "role": "evaluator" }},
                    {{ "id": "mirror-only", "kind": "build" }}
                ] }}"#
            ),
        );
        let mut reg = WorkflowRegistry::with_defaults();
        reg.load_dir(&dir.0).unwrap();
        let feature = reg.get("feature").unwrap();

        let ids: Vec<&str> = feature.phases.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![gated_id.as_str(), "mirror-only"],
            "the drop-in still replaces the def — only the gate is protected"
        );
        assert_eq!(
            feature.phases[0].validator_pin.as_deref(),
            Some(floor.as_str()),
            "the shadowed phase keeps the floor the built-in carried"
        );
        assert!(
            feature.phases[1].validator_pin.is_none(),
            "a phase the built-in never had gains nothing — this carries pins forward, it does not invent them"
        );
    }

    /// The other direction: a shadow that states its OWN pin is honoured, and a fresh id inherits
    /// nothing. Without this, "keep the gate" could have been implemented as "gates are immutable",
    /// which would break `gate-phase` and lock operators out of their own criteria.
    #[test]
    fn carrying_a_pin_forward_never_overrides_one_the_drop_in_states() {
        let dir = ScratchDir::new("ungate-own");
        let gated_id = feature_def()
            .phases
            .iter()
            .find(|p| p.validator_pin.is_some())
            .expect("a pinned phase")
            .id
            .clone();
        dir.write(
            "feature.json",
            &format!(
                r#"{{ "id": "feature", "phases": [
                    {{ "id": "{gated_id}", "kind": "review", "validator_pin": "operators-own-pin" }}
                ] }}"#
            ),
        );
        // A brand-new id shadows nothing, so it stays exactly as authored.
        dir.write(
            "fresh.json",
            &format!(
                r#"{{ "id": "fresh", "phases": [ {{ "id": "{gated_id}", "kind": "review" }} ] }}"#
            ),
        );
        let mut reg = WorkflowRegistry::with_defaults();
        reg.load_dir(&dir.0).unwrap();

        assert_eq!(
            reg.get("feature").unwrap().phases[0]
                .validator_pin
                .as_deref(),
            Some("operators-own-pin"),
            "an operator changing the criterion is the point of the seam"
        );
        assert!(
            reg.get("fresh").unwrap().phases[0].validator_pin.is_none(),
            "a new id shadows nothing and inherits nothing"
        );
    }

    #[test]
    fn load_dir_on_a_missing_dir_is_empty_not_an_error() {
        let mut reg = WorkflowRegistry::with_defaults();
        let loaded = reg.load_dir("/no/such/wicked/workflows/dir").unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn load_dir_skips_an_invalid_file_and_still_loads_the_rest() {
        let dir = ScratchDir::new("invalid");
        // A semantically invalid file (self-referential dep) that sorts FIRST, next to a good one.
        // The old behavior aborted the whole overlay on the first bad file; now it skips + continues.
        dir.write(
            "aaa-broken.json",
            r#"{ "id": "broken", "phases": [ { "id": "a", "kind": "build", "depends_on": ["a"] } ] }"#,
        );
        dir.write(
            "zzz-good.json",
            r#"{ "id": "custom", "phases": [ { "id": "do", "kind": "build" } ] }"#,
        );
        let mut reg = WorkflowRegistry::with_defaults();
        let loaded = reg.load_dir(&dir.0).unwrap();
        assert_eq!(
            loaded,
            vec!["custom"],
            "the good drop-in loads; the broken one is skipped"
        );
        assert!(reg.get("custom").is_some());
        assert!(
            reg.get("broken").is_none(),
            "the invalid file is not registered"
        );
    }

    /// Every shipped drop-in parses, validates, and carries the id its filename claims.
    ///
    /// Enumerates the directory rather than listing names. The list version had to be edited in
    /// lockstep with `workflows/` and nothing failed when it was not: a new drop-in was simply never
    /// validated, and a deleted one broke the test for the wrong reason. Same shape as the defect
    /// this test now guards against — two artifacts that must agree, with only diligence between
    /// them.
    #[test]
    fn shipped_drop_in_workflows_load_and_validate() {
        let workflows_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("workflows");
        let mut seen = 0;
        for entry in std::fs::read_dir(&workflows_dir).expect("workflows/ is readable") {
            let path = entry.expect("readable dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .expect("utf-8 filename")
                .to_string();
            let def = WorkflowRegistry::def_from_file(&path)
                .unwrap_or_else(|e| panic!("{stem}.json must parse + validate: {e}"));
            assert_eq!(def.id, stem, "{stem}.json id field must match filename");
            seen += 1;
        }
        // Without this the test passes vacuously if the directory moves or empties — validating
        // nothing while reporting success.
        assert!(
            seen > 0,
            "workflows/ shipped no drop-in defs; the directory moved or emptied"
        );
    }

    /// FINDING-055, the mechanism: a phase declaring `verified_evidence` with no pin of its own is
    /// armed with the built-in evidence floor AT REGISTRATION; a phase-specific pin is never
    /// overridden; an unflagged phase gains nothing (this is enforcement of a declaration, not a
    /// blanket floor).
    #[test]
    fn verified_evidence_without_a_pin_is_floored_at_registration() {
        let mut reg = WorkflowRegistry::default();
        reg.register(WorkflowDef {
            id: "declares".to_string(),
            phases: vec![
                PhaseDef::new("work", StageKind::Build).codes(),
                PhaseDef::new("check", StageKind::Test)
                    .verified()
                    .after("work"),
                PhaseDef::new("unflagged", StageKind::Test).after("work"),
                PhaseDef {
                    validator_pin: Some("authors-own-pin".to_string()),
                    ..PhaseDef::new("custom", StageKind::Test)
                        .verified()
                        .after("work")
                },
            ],
        })
        .unwrap();
        let def = reg.get("declares").unwrap();
        let pin = |id: &str| {
            def.phases
                .iter()
                .find(|p| p.id == id)
                .unwrap()
                .validator_pin
                .clone()
        };
        assert_eq!(
            pin("check").as_deref(),
            Some(crate::builtin_floors::EVIDENCE_FLOOR_PIN),
            "a verified_evidence phase with no pin must gain the floor — without one the flag \
             gates nothing (it has no other reader)"
        );
        assert_eq!(
            pin("custom").as_deref(),
            Some("authors-own-pin"),
            "an author's own pin is never overridden — the floor is a default, not a cap"
        );
        assert_eq!(
            pin("unflagged"),
            None,
            "a phase that never declared the flag gains nothing"
        );
    }

    /// FINDING-055, the shipped subject and the closed class.
    ///
    /// `feature`'s `test` phase declared `verified_evidence` and gated nothing: the flag has no
    /// reader; the one re-verify mechanism is the validator pin, and the phase pinned none.
    /// Asserted on the REGISTERED registry — `with_defaults` + the shipped drop-in overlay, the
    /// exact stack `pipeline::resolve` clones defs out of and `attach_pinned_validators` reads —
    /// not on the builder, whose JSON mirror deliberately stays untouched (registration is where
    /// the declaration is made true).
    ///
    /// Then the class, not just the instance: after registration NO phase anywhere may declare the
    /// flag without a pin, so a new workflow shipping the same inert declaration fails here.
    #[test]
    fn no_registered_phase_declares_verified_evidence_it_cannot_deliver() {
        let mut reg = WorkflowRegistry::with_defaults();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("workflows");
        reg.load_dir(&dir).expect("shipped drop-ins load");

        // The instance the finding named.
        let test_phase = reg
            .get("feature")
            .unwrap()
            .phases
            .iter()
            .find(|p| p.id == "test")
            .expect("feature has a test phase");
        assert!(
            test_phase.verified_evidence,
            "the declaration is still authored — enforcement arms it, it does not erase it"
        );
        assert_eq!(
            test_phase.validator_pin.as_deref(),
            Some(crate::builtin_floors::EVIDENCE_FLOOR_PIN),
            "feature/test declares verified_evidence — registration must arm it with the floor"
        );

        // The class.
        let mut declared = 0;
        for id in reg.ids() {
            for p in &reg.get(&id).unwrap().phases {
                if p.verified_evidence {
                    declared += 1;
                    assert!(
                        p.validator_pin.is_some(),
                        "workflow `{id}` phase `{}` declares verified_evidence with no validator \
                         pin — the flag gates nothing without one (FINDING-055)",
                        p.id
                    );
                }
            }
        }
        // Vacuity guard: feature/test, bug/verify, migration/verify, domain-extraction/coverage.
        assert!(
            declared >= 4,
            "expected the shipped defs to declare verified_evidence somewhere; found {declared}"
        );
    }

    /// FINDING-055 (remediation): the "drop the flag to run unverified" escape hatch is scoped to a
    /// FRESH id, and this pins exactly that so the doc on `enforce_verified_evidence` cannot drift
    /// from the code.
    ///
    /// Once a phase is floored, `carry_shadowed_pins` runs BEFORE `enforce_verified_evidence` on the
    /// next `register`, so a same-id re-registration that drops the flag has the floor carried
    /// forward — a replacement may change a gate but never silently remove one. Dropping the flag
    /// therefore runs the phase unverified only under a NEW id, which has no shadow to inherit.
    ///
    /// Falsifiers (both compiling): teach `carry_shadowed_pins` to skip the floor
    /// (`if pin == EVIDENCE_FLOOR_PIN { continue; }`) and the re-registration assert fails — that
    /// mutation is precisely the silent-ungating hole the pin exists to close; or make
    /// `enforce_verified_evidence` floor unflagged phases and the fresh-id assert fails.
    #[test]
    fn dropping_verified_evidence_keeps_the_floor_on_reregistration_but_a_fresh_id_runs_unverified()
    {
        let pin_of = |reg: &WorkflowRegistry, id: &str, phase: &str| {
            reg.get(id)
                .unwrap()
                .phases
                .iter()
                .find(|p| p.id == phase)
                .unwrap()
                .validator_pin
                .clone()
        };
        let flagged = |id: &str| WorkflowDef {
            id: id.to_string(),
            phases: vec![
                PhaseDef::new("work", StageKind::Build).codes(),
                PhaseDef::new("check", StageKind::Test)
                    .verified()
                    .after("work"),
            ],
        };
        let unflagged = |id: &str| WorkflowDef {
            id: id.to_string(),
            phases: vec![
                PhaseDef::new("work", StageKind::Build).codes(),
                PhaseDef::new("check", StageKind::Test).after("work"),
            ],
        };

        let mut reg = WorkflowRegistry::default();
        // First registration arms the floor (no shadow to carry).
        reg.register(flagged("wf")).unwrap();
        assert_eq!(
            pin_of(&reg, "wf", "check").as_deref(),
            Some(crate::builtin_floors::EVIDENCE_FLOOR_PIN),
            "first registration of a flagged, unpinned phase must arm it with the floor"
        );

        // Same-id re-registration that DROPS the flag does NOT run the phase unverified: the floor
        // is a gate, so `carry_shadowed_pins` keeps it. The "drop the flag" remedy is inert here —
        // by design, not by accident, which is the narrowed claim under test.
        reg.register(unflagged("wf")).unwrap();
        assert_eq!(
            pin_of(&reg, "wf", "check").as_deref(),
            Some(crate::builtin_floors::EVIDENCE_FLOOR_PIN),
            "dropping the flag on the SAME id must keep the floor — carry_shadowed_pins forbids \
             silently ungating a replacement (FINDING-055 remedy is scoped to a fresh id)"
        );

        // A FRESH id with no flag and no pin is the actual escape hatch — no shadow, nothing carried.
        reg.register(unflagged("wf-unverified")).unwrap();
        assert_eq!(
            pin_of(&reg, "wf-unverified", "check"),
            None,
            "a fresh unflagged id runs unverified — the documented way to opt out of re-verification"
        );
    }

    /// FINDING-011, asserted against the SHIPPED `survey-repo` drop-in (the workflow the finding
    /// billed: $3.09 / 1.74M tokens for three near-identical surveys and no answer).
    ///
    /// Substance, not presence: the property is that the PLANNED PROMPTS stop being interchangeable
    /// and that something downstream consumes the recon phases. So this plans the def and asserts
    /// the prompt BODIES (after the `<phase> — ` prefix, the only part that ever differed) are
    /// pairwise distinct, and that the final phase declares a dependency on EVERY earlier phase —
    /// the declared-handoff edge (FINDING-024) is what makes the actor inject their outputs as
    /// prior context, so synthesis reads the surveys instead of re-running one.
    #[test]
    fn shipped_survey_repo_plans_distinct_prompts_and_a_synthesis_over_all_recon() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("workflows/survey-repo.json");
        let def = WorkflowRegistry::def_from_file(&path).expect("shipped survey-repo parses");

        // The last phase consumes every phase before it — a synthesis, not another survey.
        let last = def.phases.last().expect("non-empty");
        let earlier: Vec<&str> = def.phases[..def.phases.len() - 1]
            .iter()
            .map(|p| p.id.as_str())
            .collect();
        assert!(
            earlier.len() >= 3,
            "survey-repo must still fan out over multiple recon phases, found {earlier:?}"
        );
        for id in &earlier {
            assert!(
                last.depends_on.iter().any(|d| d == id),
                "final phase `{}` must depend on `{id}` so that phase's output is injected as \
                 prior context — without the edge the synthesis runs blind (FINDING-024/011)",
                last.id
            );
        }

        // Every phase states its own slice of the work, and no two slices are the same text.
        for p in &def.phases {
            let instr = p.instructions.as_deref().map(str::trim).unwrap_or("");
            assert!(
                !instr.is_empty(),
                "phase `{}` carries no instructions — its prompt collapses back to \
                 `<phase> — <intent>`, the near-identical shape this finding is about",
                p.id
            );
        }

        // The planned prompt bodies are pairwise distinct beyond the phase-id token. Strip the
        // `<phase.id> — ` prefix so the comparison cannot be satisfied by the id alone (which is
        // exactly how the defective prompts "differed").
        let units =
            crate::plan::plan_from_def(&def, "what is this repo and how do I work in it", "s");
        let bodies: Vec<String> = units
            .iter()
            .zip(def.phases.iter())
            .map(|(u, p)| {
                u.description
                    .strip_prefix(&format!("{} — ", p.id))
                    .unwrap_or(&u.description)
                    .to_string()
            })
            .collect();
        for i in 0..bodies.len() {
            for j in (i + 1)..bodies.len() {
                assert_ne!(
                    bodies[i], bodies[j],
                    "phases `{}` and `{}` plan the same prompt body — near-identical prompts again",
                    def.phases[i].id, def.phases[j].id
                );
            }
        }
    }
}
