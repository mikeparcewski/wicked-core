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
}

/// How a worker step finished. P2 wires `Ok`/`Failed`; `Cancelled` lands with real subprocess kill
/// (P4a). A `Failed` step does NOT silently complete the run — the actor surfaces it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StepStatus {
    #[default]
    Ok,
    Failed,
    Cancelled,
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
        }
    }
}
