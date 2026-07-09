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

// ─────────────────────────────────────────────────────────────────────────────────────────────
// WorkflowDef — workflows as DATA (DES-EXEC-001 §4, Law 2: capability lands as data + registration,
// never a core edit). A `WorkflowDef` is an ordered list of `PhaseDef`s; every field the reducer
// needs to drive a phase (its gate policy, whether it runs code, whether it needs verified evidence,
// its role for the evaluator≠creator split, its dependencies) is DATA on the phase. The reducer
// branches on these fields — never on the workflow `id` and never on a closed `match` over a phase
// name. Adding feature/bug/migration (below) or a new workflow is a data value, not a core change.
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
    /// Always require a human to confirm. `unconditional` gates cannot be downgraded by the dial.
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
    /// Optional skill to drive the phase (DES-EXEC-001 §4.1 — DEFERRED/spike-gated; `None` = the
    /// authored-prompt path for slice-1).
    #[serde(default)]
    pub skill_ref: Option<String>,
}

impl PhaseDef {
    /// A minimal phase: id + kind, no gate, no code, neutral role.
    fn new(id: &str, kind: StageKind) -> Self {
        PhaseDef {
            id: id.to_string(),
            kind,
            gate_type: None,
            gate: GateSpec::Auto,
            executes_code: false,
            verified_evidence: false,
            required_deliverables: Vec::new(),
            depends_on: Vec::new(),
            role: PhaseRole::Neutral,
            skill_ref: None,
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
    fn after(mut self, dep: &str) -> Self {
        self.depends_on.push(dep.to_string());
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

/// The registry of known workflows — id → def. `with_defaults()` seeds feature/bug/migration.
/// Registering a new workflow is a data insert (Law 2); the reducer only ever `get`s a def.
#[derive(Debug, Clone, Default)]
pub struct WorkflowRegistry {
    defs: HashMap<String, WorkflowDef>,
}

impl WorkflowRegistry {
    /// The built-in workflows (feature/bug/migration), each validated at construction.
    pub fn with_defaults() -> Self {
        let mut r = WorkflowRegistry::default();
        for def in [feature_def(), bug_def(), migration_def()] {
            r.register(def).expect("built-in workflow defs are valid");
        }
        r
    }
    /// Register (or replace) a workflow. Validates before inserting.
    pub fn register(&mut self, def: WorkflowDef) -> Result<(), WorkflowDefError> {
        def.validate()?;
        self.defs.insert(def.id.clone(), def);
        Ok(())
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
    /// A malformed or invalid file is an error naming the file, so a typo fails loud, never silent.
    /// Returns the ids loaded, in load order.
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
            let def = Self::def_from_file(&path)?;
            let id = def.id.clone();
            self.register(def)
                .map_err(|e| anyhow::anyhow!("workflow {id} in {}: {e}", path.display()))?;
            loaded.push(id);
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

/// `feature` — clarify(value) → design(strategy) → build(execution) → adversarial-review → test → review.
/// Gates: HumanConfirm after clarify + after adversarial-review; HumanConfirmIf(¬PASS) on test.
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
                .after("build"),
            PhaseDef::new("test", StageKind::Test)
                .gate(
                    GateType::Execution,
                    GateSpec::HumanConfirmIf(GateCond::VerdictNotPass),
                )
                .verified()
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
                .after("cutover"),
            PhaseDef::new("cleanup", StageKind::Build).after("verify"),
        ],
    }
}

#[cfg(test)]
mod workflow_def_tests {
    use super::*;

    #[test]
    fn registry_seeds_the_three_builtin_workflows() {
        let r = WorkflowRegistry::with_defaults();
        assert_eq!(r.ids(), vec!["bug", "feature", "migration"]);
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

    #[test]
    fn load_dir_on_a_missing_dir_is_empty_not_an_error() {
        let mut reg = WorkflowRegistry::with_defaults();
        let loaded = reg.load_dir("/no/such/wicked/workflows/dir").unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn load_dir_rejects_an_invalid_data_file_by_name() {
        let dir = ScratchDir::new("invalid");
        // Structurally parseable but semantically invalid: a self-referential dependency cycle.
        dir.write(
            "broken.json",
            r#"{ "id": "broken", "phases": [ { "id": "a", "kind": "build", "depends_on": ["a"] } ] }"#,
        );
        let mut reg = WorkflowRegistry::with_defaults();
        let err = reg.load_dir(&dir.0).unwrap_err().to_string();
        assert!(
            err.contains("broken.json"),
            "error must name the bad file: {err}"
        );
    }
}
