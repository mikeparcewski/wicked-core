//! The session/unit domain — ported into COE from the retired wicked-agent. These are the entities
//! the pipeline plans, distributes, executes, and the UI reads. Each round-trips losslessly through
//! one estate `Node.metadata` object (serde), so adding a field needs no per-field plumbing.

use serde::{Deserialize, Serialize};
use wicked_apps_core::{
    synthetic_symbol, FromNode, GraphRead, GraphStore, Language, Location, Node, NodeKind, Span,
    ToNode, AGENT_SESSION, SYMBOL_SCHEME, WORK_UNIT,
};
use wicked_estate_core::SymbolQuery;

use crate::scope::EntityMode;

/// Lifecycle status of an [`AgentSession`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Planning,
    Distributing,
    Executing,
    /// Paused BEFORE a not-yet-done unit, awaiting a human to resume.
    AwaitingHuman,
    /// Terminal: every unit was governance-approved and ran without worker failure.
    Completed,
    /// Terminated by the operator (or a rejected gate) before completing. Terminal.
    Cancelled,
    /// Terminal: stopped because a unit was governance-DENIED or its worker reported failure. The
    /// RUN-LEVEL DENY CONTRACT (decided in P2): a `Completed` run means EVERY unit was approved; a
    /// governance `Deny` (or a `StepStatus::Failed` worker) halts the run here, never silently
    /// completing past a rejection.
    Failed,
}

/// The human-confirm gate policy for a run — whether to pause BEFORE executing a unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum HumanConfirm {
    /// Never pause (default).
    #[default]
    None,
    /// Pause before EVERY not-yet-done unit.
    All,
    /// Pause before the unit whose `ord` equals the value.
    Before(u32),
}

impl HumanConfirm {
    /// The ONE parser for a human-confirm wire token (`none` | `all` | `before:<ord>`). Every entry
    /// point — the bus bridge, the CLI, the napi layer, the HTTP API — must route through this so a
    /// token cannot mean three different things depending on which door it came through (FINDING-019).
    ///
    /// FAILS CLOSED. An absent field is the legitimate unattended default (`Ok(HumanConfirm::None)`),
    /// but a PRESENT unrecognised token — a typo like `"al"`, or an unparseable `"before:x"` — is an
    /// `Err`, never a silent downgrade. The old per-door parsers all fell through to `None` on a typo, so
    /// an operator who asked to pause silently got an UNGATED run (the bus path dropped `before:<ord>`
    /// entirely). Returning the option to `None` on a typo is a governance bypass; refusing the launch
    /// is not.
    pub fn parse(raw: Option<&str>) -> Result<Self, String> {
        match raw.map(str::trim) {
            None | Some("") | Some("none") => Ok(Self::None),
            Some("all") => Ok(Self::All),
            Some(s) if s.starts_with("before:") => {
                let ord = &s["before:".len()..];
                ord.trim().parse::<u32>().map(Self::Before).map_err(|_| {
                    format!("humanConfirm 'before:{ord}' needs a unit ordinal (a number), e.g. 'before:2'")
                })
            }
            Some(other) => Err(format!(
                "unrecognised humanConfirm '{other}' (expected 'none', 'all', or 'before:<ord>')"
            )),
        }
    }
}

/// A session — the owned interactive flow, persisted as `Node(Other(AGENT_SESSION))`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSession {
    /// Stable session id (the node identity).
    pub id: String,
    /// The orchestration workflow id backing this session.
    pub workflow_id: String,
    /// The free-text problem this session decomposes.
    pub problem: String,
    /// Shared (one scope for all units) vs isolated (per-unit scope) — §6 toggle.
    pub entity_mode: EntityMode,
    /// The collection scope under shared mode (`None` under isolated).
    pub collection_scope: Option<String>,
    /// The CLI seats convened for this session (council options).
    pub clis: Vec<String>,
    /// Lifecycle status.
    pub status: SessionStatus,
    /// The human-confirm gate policy. `#[serde(default)]` so older sessions still deserialize.
    #[serde(default)]
    pub human_confirm: HumanConfirm,
    /// Resume cursor: the index of the NEXT unit to execute (0-based into the ordered units). The
    /// interactive engine advances this as each unit's outcome is applied; `ResumeRun` re-enters
    /// here. `#[serde(default)]` so older sessions deserialize at 0.
    #[serde(default)]
    pub unit_ix: usize,
    /// Retry attempt for the unit at `unit_ix` — folded into event ids so a retried step is not
    /// deduped as a no-op (P2). `#[serde(default)]` for back-compat.
    #[serde(default)]
    pub attempt: u32,
    /// The git worktree this run executes in (set when the run targets a registered repo, P3).
    /// `None` for a repo-less run. `#[serde(default)]` for back-compat.
    #[serde(default)]
    pub workdir: Option<String>,
    /// The registered repo this run targets, if any (P3).
    #[serde(default)]
    pub repo_ref: Option<String>,
    /// Launcher-declared extra write roots for the run's deliverables (core#259). Persisted on the
    /// session so a resume/redrive re-arms the SAME boundary the launch declared — the widening
    /// must never depend on the daemon's memory of the launch. `#[serde(default)]` for back-compat:
    /// older sessions deserialize with no extra roots, i.e. the pre-#259 boundary.
    #[serde(default)]
    pub extra_write_roots: Vec<String>,
    /// The project code graph the launcher bound this run to, if any (see
    /// [`crate::project::ProjectGraphBinding`]). Persisted on the session for the same reason
    /// `extra_write_roots` is (core#259): a resume/redrive re-enters through the actor with no
    /// `LaunchSpec` in hand, so a binding held only in the launcher's memory would silently
    /// narrow a half-finished run's tools from the whole project back to one repo between two of
    /// its own units. `#[serde(default)]` for back-compat: older sessions deserialize unbound,
    /// i.e. the pre-change per-repo behaviour.
    #[serde(default)]
    pub project_graph: Option<crate::project::ProjectGraphBinding>,
    /// When the operator ARCHIVED this run (crew#265) — a write-off, not a delete: the run and
    /// every artifact stay fully readable (same retire-not-delete contract as retired policies,
    /// FINDING-038), but default run listings exclude it. Only a TERMINAL run can be archived.
    /// Unix millis; `None` = live. `#[serde(default)]` for back-compat.
    #[serde(default)]
    pub archived_at: Option<i64>,
    /// Optional operator note recorded at archival ("superseded by fix X", "campaign backlog").
    #[serde(default)]
    pub archive_note: Option<String>,
}

impl ToNode for AgentSession {
    fn node_kind() -> &'static str {
        AGENT_SESSION
    }

    fn to_node(&self) -> Node {
        let mut node = Node::new(
            synthetic_symbol(AGENT_SESSION, &self.id),
            NodeKind::Other(AGENT_SESSION.to_string()),
            self.id.clone(),
            Language::new(SYMBOL_SCHEME),
            Location::new(format!("{AGENT_SESSION}/{}", self.id), Span::ZERO),
        );
        if let serde_json::Value::Object(map) =
            serde_json::to_value(self).expect("AgentSession serializes to JSON")
        {
            node.metadata = map;
        }
        node
    }
}

impl FromNode for AgentSession {
    fn from_node(node: &Node) -> anyhow::Result<Self> {
        match &node.kind {
            NodeKind::Other(k) if k == AGENT_SESSION => {}
            other => anyhow::bail!("expected NodeKind::Other({AGENT_SESSION:?}), got {other:?}"),
        }
        serde_json::from_value(serde_json::Value::Object(node.metadata.clone()))
            .map_err(|e| anyhow::anyhow!("node {} is not a valid AgentSession: {e}", node.name))
    }
}

/// The MACHINE-READABLE twin of a unit's prose `denial_reason` (usability review #1): which layer
/// denied, which rule/policy fired, which claim recorded it, and — for an input-governance deny —
/// which tool-call was refused. Additive everywhere it appears (unit record, work-output record,
/// `GateEvaluated`), so a consumer that only reads the prose keeps working while a UI can render a
/// plain-language banner from structure instead of parsing a sentence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnitDenial {
    /// The layer that denied — a stable token, one of: `governance` (the unit's own gate),
    /// `input_governance` (the tool-call hook / boundary), `pinned_validator` (deterministic
    /// re-verify), `agent_validator` (LLM judge), `evaluator` (evaluator≠creator second pass),
    /// `worker_failure` (the CLI process failed), `substance` (no reviewable substance),
    /// `deliverables` (declared deliverables missing), `elicitation` (ACP elicitation ended).
    pub source: String,
    /// The operator-facing prose — byte-identical to the `denial_reason` the record carries.
    pub reason: String,
    /// The `ConformanceClaim` id that recorded the deny (e.g. `boundary-deny:unit-2`), when one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_id: Option<String>,
    /// The policy/rule ids that fired (e.g. `engine:pre-build-scope`). Empty when the deny came
    /// from a layer with no named rule (a worker failure, a fail-closed infra deny).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rule_ids: Vec<String>,
    /// The tool whose call was refused (`Bash`, `Edit`, …) — input-governance denies only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub denied_tool: Option<String>,
    /// The unit-phase token the deny targeted (e.g. `unit-2`), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
}

impl UnitDenial {
    /// A denial with only a source + prose reason — the shape of every layer that carries no
    /// claim/rule/tool identity (validators, worker failures, substance/deliverable floors).
    pub fn new(source: impl Into<String>, reason: impl Into<String>) -> Self {
        UnitDenial {
            source: source.into(),
            reason: reason.into(),
            claim_id: None,
            rule_ids: Vec::new(),
            denied_tool: None,
            phase: None,
        }
    }
}

/// A unit of distributed work, persisted as `Node(Other(WORK_UNIT))`. Plan creates it `Pending`;
/// distribute records the assignment; execute records the outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkUnit {
    /// Stable unit id (the node identity), e.g. `<session>:u1`.
    pub id: String,
    /// The owning session id.
    pub session_id: String,
    /// 1-based order in the plan.
    pub ord: u32,
    /// The unit's description (becomes the gate's governance `work` context).
    pub description: String,
    /// The methodology stage this unit belongs to (recon/build/review/test), classified at plan time.
    /// `#[serde(default)]` (→ `Build`) so older units deserialize.
    #[serde(default)]
    pub stage: StageKind,
    /// The CLI the council assigned (set in distribute).
    #[serde(default)]
    pub assigned_cli: Option<String>,
    /// The assigned CLI's invocation template (from the launch roster) — lets the runner execute an
    /// AD-HOC CLI not in the council registry. `None` ⇒ the runner resolves the key via the registry.
    #[serde(default)]
    pub assigned_invocation: Option<String>,
    /// The council task id whose verdict produced the assignment (provenance).
    #[serde(default)]
    pub council_task_ref: Option<String>,
    /// WHY the assigned CLI won — the council verdict/ranking made visible (set in distribute). `None`
    /// for units distributed before this field existed. `#[serde(default)]` for back-compat.
    #[serde(default)]
    pub routing: Option<RoutingInfo>,
    /// WHY the unit was rejected — a governance deny (which policies) or a worker failure. Set only
    /// when the run halts on this unit. `#[serde(default)]` for back-compat.
    #[serde(default)]
    pub denial_reason: Option<String>,
    /// The STRUCTURED twin of [`Self::denial_reason`] (usability review #1): source layer, firing
    /// rule ids, recording claim id, denied tool. Additive — absent on units persisted before it
    /// existed and on approved units; skip-if-none keeps the wire byte-identical for those.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub denial: Option<UnitDenial>,
    /// The orchestration phase id backing this unit (set in execute).
    #[serde(default)]
    pub phase_ref: Option<String>,
    /// The ConformanceClaim id the gate consumed (set in execute).
    #[serde(default)]
    pub conformance_ref: Option<String>,
    /// The phase status token the gate resolved to, e.g. `approved` / `rejected`.
    #[serde(default)]
    pub phase_status: Option<String>,
    /// The collection scope this unit's output is written to.
    #[serde(default)]
    pub collection_scope: Option<String>,
    /// The skill that drives this unit's work (DES-EXEC-001 §4.1) — carried from the backing phase's
    /// `skill_ref` at plan time (def-driven runs). `None` ⇒ the authored-prompt path. `#[serde(default)]`
    /// for back-compat with units persisted before the skills seam.
    #[serde(default)]
    pub skill_ref: Option<String>,
    /// The runtime skill ALLOWLIST for this unit's agent (DES-EXEC-001 §4.2) — carried from the phase's
    /// `allowed_skills`. The runner passes it as the invocation's skill/tool scope. Empty ⇒ unscoped.
    #[serde(default)]
    pub allowed_skills: Vec<String>,
    /// The backing phase's declared human-confirm gate (DES-EXEC-001 §3) — carried from the phase's
    /// `GateSpec` so the def, not just the run-level `--confirm` flag, drives when a run pauses for a
    /// human. A phase's gate fires AFTER its work (before the next unit). `Auto` (the default) ⇒ defer
    /// to the run-level policy. `#[serde(default)]` for back-compat with pre-gate-wiring units.
    #[serde(default)]
    pub gate: crate::workflow::GateSpec,
    /// The backing phase's evaluator≠creator role (DES-EXEC-001 §4). An `Evaluator`-role unit reviews
    /// the COLD output of the most recent prior `Creator`-role unit (real artifact-passing), not its
    /// own. `Neutral` (default) keeps the generic per-unit second pass. `#[serde(default)]` back-compat.
    #[serde(default)]
    pub role: crate::workflow::PhaseRole,
    /// The APPROVED, pinned deterministic validator for this unit's phase (rev0.4 gate layer-1). When
    /// present, the gate RE-VERIFIES it against the worktree after the governance pass — a fail denies
    /// the unit (deny-dominates). Authored + approved out of band; `None` ⇒ no validator (the pre-gate
    /// behavior). `#[serde(default)]` for back-compat.
    #[serde(default)]
    pub validator: Option<crate::validator::DeterministicValidator>,
    /// Files this unit's phase DECLARED it must produce (FINDING-101). Carried from
    /// `PhaseDef::required_deliverables`, which was parsed and never read — so every workflow could
    /// promise artifacts nothing checked. Verified before the unit may report Ok, which is the same
    /// rule the rest of the engine applies: done is re-derived from evidence, not asserted.
    ///
    /// Resolved against the unit's worktree (or its per-run sandbox when the run is unbound) AND
    /// against the run's declared `extra_write_roots` — see
    /// [`crate::path_policy::missing_deliverables`] for the rules. Enforced at the
    /// runner-independent fold in `actor::apply_step_result`, NOT in any runner: it lived in the
    /// wrapped runner alone until core#297, which made the gate's presence depend on which seat the
    /// run resolved to.
    #[serde(default)]
    pub required_deliverables: Vec<String>,
    /// The exact command this unit runs when `PhaseExecutor::Tool` (carried from the phase def).
    /// `None` for Agent-executor units. `#[serde(default)]` for back-compat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_cmd: Option<Vec<String>>,
    /// Seats that WORKER-FAILED this unit — the CLI process itself failed (exited nonzero, could
    /// not spawn, timed out), never a judged rejection of the work. Recorded by the actor's seat
    /// failover ladder (core#282) and read by both the in-run failover and the resume dispatch so
    /// the unit is never handed back to a seat that already worker-failed it until every eligible
    /// seat has been tried. Persisted ON the unit (not actor state) so the guarantee survives a
    /// resume/restart. `#[serde(default)]` for back-compat with units persisted before this field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub worker_failed_clis: Vec<String>,
    /// The PHASE IDS this unit's work depends on — carried verbatim from the backing phase's
    /// [`depends_on`](crate::workflow::PhaseDef::depends_on) at plan time (FINDING-024).
    ///
    /// The def already states the handoff graph and the engine already honors it for ORDERING; this
    /// field carries it to the dispatch site so it can also drive CONTEXT. Without it the actor has
    /// no route back to the def — [`AgentSession`] records only a synthetic `wf-<session>` id, not the
    /// workflow that produced the plan — so the declared dependency was structurally unreachable at
    /// the moment it was needed. Empty for prose-planned runs (no def ⇒ no declared graph, and none
    /// is invented). `#[serde(default)]` for back-compat with units planned before this existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    /// TRUE when this unit's backing phase is a PRE-BUILD, NON-CREATOR rung of a def that later
    /// runs an `executes_code` Creator phase (core#283) — set at plan time from def DATA (role +
    /// `executes_code` + declaration order), the same way stage/role/gate flow from the def.
    ///
    /// Three consumers, ONE marker, so they cannot disagree about which phases are pre-build:
    /// * the plan-time PHASE SCOPE preamble on this unit's prompt ([`crate::plan`]);
    /// * **the GATE** (core#296) — the launcher rides this flag to the governance hook
    ///   (`WICKED_PRE_BUILD_SCOPE` on the subprocess carrier, `BoundaryCtx::pre_build_scope`
    ///   in-process) and `evaluate_tool_call` REFUSES a `Write`/`Edit` to a non-documentation path;
    /// * the completion path's after-the-fact WARNING onto [`Self::scope_warnings`], which still
    ///   catches what a tool-call gate structurally cannot see (a `Bash` heredoc, a `git apply`).
    ///
    /// `#[serde(default)]` + skip-if-false: pre-existing units deserialize, and unmarked units
    /// serialize byte-identical to before the field existed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pre_build_scope: bool,
    /// Operator-visible WARNINGS the completion path recorded WITHOUT denying (core#283): today,
    /// a pre-build phase whose worktree contribution touches non-documentation files — the
    /// design-before-build ladder collapsing into implementation. Advisory gate evidence on the
    /// persisted unit; it never drives the unit `Rejected` (that is [`Self::denial_reason`]'s job).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scope_warnings: Vec<String>,
    /// The final unit status: `pending` → `distributed` → `done` | `rejected`.
    pub status: UnitStatus,
}

/// Lifecycle status of a [`WorkUnit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitStatus {
    Pending,
    Distributed,
    Done,
    Rejected,
}

/// The methodology stage a unit belongs to (the recon → build → adversarial-review → functional-test
/// spine). Classified from the unit's description in [`crate::plan`]; surfaced as a per-unit badge so
/// the methodology is legible (you can tell a Recon unit from a Build unit from a Review unit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StageKind {
    /// Decompose / explore / map the problem before building.
    Recon,
    /// The main implementation work (the default).
    #[default]
    Build,
    /// Adversarial review — a distinct critic checks the build (evaluator ≠ creator).
    Review,
    /// Functional testing — verify the build actually works.
    Test,
}

impl StageKind {
    /// Classify a unit's stage from its description (a v1 keyword heuristic — the spine made visible
    /// without changing how units are planned).
    pub fn classify(description: &str) -> StageKind {
        let d = description.to_lowercase();
        let has = |words: &[&str]| words.iter().any(|w| d.contains(w));
        if has(&["test", "verify", "validate", "functional", "qa "]) {
            StageKind::Test
        } else if has(&[
            "review",
            "audit",
            "adversarial",
            "critique",
            "evaluate",
            "inspect",
        ]) {
            StageKind::Review
        } else if has(&[
            "recon",
            "research",
            "explore",
            "investigate",
            "decompose",
            "map the",
            "scope ",
        ]) {
            StageKind::Recon
        } else {
            StageKind::Build
        }
    }

    /// Short label for the UI badge.
    pub fn label(self) -> &'static str {
        match self {
            StageKind::Recon => "recon",
            StageKind::Build => "build",
            StageKind::Review => "review",
            StageKind::Test => "test",
        }
    }
}

/// WHY a particular CLI was assigned to a unit — the council's decision made visible. The verdict is
/// otherwise computed in [`crate::distribute`] and thrown away; capturing it here is what lets the UI
/// answer "why *this* CLI". Percentages are `0..=100` (not `f32`) so `WorkUnit` stays `Eq`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum RoutingInfo {
    /// The council convened and its verdict named the winning seat.
    Council {
        winner: String,
        /// Council agreement ratio, `0..=100`.
        agreement_pct: u8,
        /// How many seats returned a vote.
        returned: u32,
        /// How many seats were CONVENED — the denominator `returned` must be read against.
        ///
        /// `returned: 1` describes a complete one-seat council and a three-seat council that
        /// lost two seats, and only this field separates them. Recorded on the routing artifact
        /// itself so an auditor reading a stored decision never has to reconstruct the quorum
        /// from the session's roster (FINDING-026 D).
        ///
        /// `None` means UNKNOWN, never "equal to `returned`" — a unit distributed by an engine
        /// older than that fix has no seat count on it.
        ///
        /// The `Option` is what carries the back-compat: serde reads a missing `Option` field as
        /// `None`, so the `default` below is belt-and-braces (proven by mutation — the legacy-load
        /// test passes with it removed). It matters because `WorkUnit` round-trips its WHOLE
        /// struct through `metadata` and `session_units` DROPS any unit that fails to deserialize
        /// (`from_node(n).ok()`): a required field here would silently erase every historical
        /// council unit from the run view, no error anywhere.
        ///
        /// Not `u32` defaulting to 0 — that is an impossible denominator, and it would reach the
        /// UI as "3 of 0 seats" instead of falling back to the unquantified wording.
        #[serde(default)]
        seated: Option<u32>,
        /// How many dissenting voices the verdict recorded.
        dissent: u32,
    },
    /// No usable verdict (no quorum, or the winner named no roster seat) — degraded to the first seat.
    Degraded { reason: String },
    /// A review/test unit was REASSIGNED off the council's pick to enforce evaluator ≠ creator (the
    /// critic must differ from the CLI that produced the work it checks). `was` is the council's pick.
    EvaluatorDistinct { winner: String, was: String },
    /// No council convened — this unit is a deterministic tool execution. The `assigned_cli` is
    /// the literal command name (first element of `tool_cmd`).
    Tool,
}

impl WorkUnit {
    /// Build a fresh `Pending` unit for the plan.
    pub fn pending(
        id: impl Into<String>,
        session_id: impl Into<String>,
        ord: u32,
        description: impl Into<String>,
    ) -> Self {
        let description = description.into();
        WorkUnit {
            id: id.into(),
            session_id: session_id.into(),
            ord,
            stage: StageKind::classify(&description),
            description,
            assigned_cli: None,
            assigned_invocation: None,
            council_task_ref: None,
            routing: None,
            denial_reason: None,
            denial: None,
            phase_ref: None,
            conformance_ref: None,
            phase_status: None,
            collection_scope: None,
            skill_ref: None,
            allowed_skills: Vec::new(),
            gate: crate::workflow::GateSpec::default(),
            role: crate::workflow::PhaseRole::default(),
            validator: None,
            required_deliverables: Vec::new(),
            tool_cmd: None,
            worker_failed_clis: Vec::new(),
            depends_on: Vec::new(),
            pre_build_scope: false,
            scope_warnings: Vec::new(),
            status: UnitStatus::Pending,
        }
    }

    /// The workflow PHASE ID backing this unit — the suffix of [`Self::id`] after the session
    /// prefix (`<session>:<phase_id>` for def-driven runs, `<session>:u<ord>` for prose-planned
    /// ones; see [`crate::plan`]). Distinct from [`Self::phase_ref`], which the execute path sets
    /// to the synthetic ORCHESTRATION phase (`unit-<ord>`).
    ///
    /// This is the token an operator actually sees in the API and would naturally author a
    /// governance `applies_to` against, so policy selection matches it alongside `unit-<ord>`.
    /// `None` when the id does not carry the session prefix (hand-built units in tests).
    pub fn phase_id(&self) -> Option<&str> {
        self.id
            .strip_prefix(&self.session_id)?
            .strip_prefix(':')
            .filter(|suffix| !suffix.is_empty())
    }
}

impl ToNode for WorkUnit {
    fn node_kind() -> &'static str {
        WORK_UNIT
    }

    fn to_node(&self) -> Node {
        let mut node = Node::new(
            synthetic_symbol(WORK_UNIT, &self.id),
            NodeKind::Other(WORK_UNIT.to_string()),
            self.id.clone(),
            Language::new(SYMBOL_SCHEME),
            Location::new(format!("{WORK_UNIT}/{}", self.id), Span::ZERO),
        );
        if let serde_json::Value::Object(map) =
            serde_json::to_value(self).expect("WorkUnit serializes to JSON")
        {
            node.metadata = map;
        }
        node
    }
}

impl FromNode for WorkUnit {
    fn from_node(node: &Node) -> anyhow::Result<Self> {
        match &node.kind {
            NodeKind::Other(k) if k == WORK_UNIT => {}
            other => anyhow::bail!("expected NodeKind::Other({WORK_UNIT:?}), got {other:?}"),
        }
        serde_json::from_value(serde_json::Value::Object(node.metadata.clone()))
            .map_err(|e| anyhow::anyhow!("node {} is not a valid WorkUnit: {e}", node.name))
    }
}

// ── Store primitives (the single shared-store read/write the pipeline uses) ──────

/// Upsert a node onto the store via the batch write path. Called only from the actor thread (the
/// single writer for `shared_writers=false` backends).
pub fn put_node(store: &mut dyn GraphStore, node: Node) -> anyhow::Result<()> {
    store.begin_batch()?;
    store.upsert_nodes(&[node])?;
    store.commit_batch()?;
    Ok(())
}

/// Upsert SEVERAL nodes in ONE batch — the atomic multi-record write (e.g. a run's launch stub +
/// its project membership, or an `AwaitingHuman` session + its durable interaction request, both
/// DES-PROJECT-001). One `begin_batch`/`commit_batch`, so the records commit together or not at all.
pub fn put_nodes(store: &mut dyn GraphStore, nodes: &[Node]) -> anyhow::Result<()> {
    store.begin_batch()?;
    store.upsert_nodes(nodes)?;
    store.commit_batch()?;
    Ok(())
}

/// Read an [`AgentSession`] back by id.
pub fn get_session(
    store: &dyn GraphRead,
    session_id: &str,
) -> anyhow::Result<Option<AgentSession>> {
    match store.get_node(&synthetic_symbol(AGENT_SESSION, session_id))? {
        Some(node) => Ok(Some(AgentSession::from_node(&node)?)),
        None => Ok(None),
    }
}

/// Read every [`WorkUnit`] belonging to `session_id`, ordered by `ord`.
pub fn session_units(store: &dyn GraphRead, session_id: &str) -> anyhow::Result<Vec<WorkUnit>> {
    let query = SymbolQuery {
        kinds: vec![NodeKind::Other(WORK_UNIT.to_string())],
        ..Default::default()
    };
    let mut units: Vec<WorkUnit> = store
        .find_symbols(&query)?
        .iter()
        .filter_map(|n| WorkUnit::from_node(n).ok())
        .filter(|u| u.session_id == session_id)
        .collect();
    units.sort_by_key(|u| u.ord);
    Ok(units)
}

/// Every session on the store (unordered).
pub fn all_sessions(store: &dyn GraphRead) -> anyhow::Result<Vec<AgentSession>> {
    let query = SymbolQuery {
        kinds: vec![NodeKind::Other(AGENT_SESSION.to_string())],
        ..Default::default()
    };
    Ok(store
        .find_symbols(&query)?
        .iter()
        .filter_map(|n| AgentSession::from_node(n).ok())
        .collect())
}

/// A unit's APPROVED work output, if the unit ran + its phase resolved. This is the read the
/// engine itself builds on — evaluator artifact-passing ([`crate::pipeline`]'s `creator_output_for`)
/// and prior-unit context injection — so it deliberately returns `None` for a REJECTED unit's
/// partial record: no unapproved output may ever be handed to a later unit as reviewed work
/// (ADR-0003). The rejected record is read through [`get_unit_transcript`] instead.
pub fn get_work_output(store: &dyn GraphRead, unit_id: &str) -> Option<String> {
    let node = store
        .get_node(&synthetic_symbol(crate::execute::WORK_OUTPUT, unit_id))
        .ok()??;
    // Records written before `resolution` existed were only ever written on approval, so an
    // ABSENT marker reads as resolved; only an explicit rejected marker filters.
    if node
        .metadata
        .get(crate::execute::RESOLUTION_KEY)
        .and_then(|v| v.as_str())
        == Some(crate::execute::RESOLUTION_REJECTED)
    {
        return None;
    }
    node.metadata
        .get("output")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// A unit's transcript RECORD — what [`get_work_output`] cannot say (usability review #1): a
/// REJECTED unit's partial output survives here, flagged, with the structured denial beside it,
/// and a unit denied BEFORE any output existed still answers with an explicit failure record
/// (`output: None` + the deny's claim/rule/tool) instead of nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnitTranscript {
    /// The unit this record belongs to.
    pub unit_id: String,
    /// `resolved` — the phase resolved approved and `output` is the gated work product; or
    /// `rejected` — the unit was denied/failed and `output` (when present) is PARTIAL: whatever
    /// existed at rejection, never an approved artifact.
    pub resolution: String,
    /// `true` ⇔ `resolution == "rejected"` — the one-bool flag a reader needs to distinguish
    /// partial-from-failure output from resolved output.
    pub partial: bool,
    /// The phase status token the record was written under (`approved`, `rejected`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase_status: Option<String>,
    /// The transcript text. `None` on a rejected record when NO output existed at rejection
    /// (a pre-output deny) — the record then exists purely to carry the denial.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// The prose WHY, when rejected (same string as the unit's `denial_reason`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub denial_reason: Option<String>,
    /// The machine-readable WHY, when rejected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub denial: Option<UnitDenial>,
}

/// Read a unit's transcript record — resolved OR rejected — or `None` when the unit never ran far
/// enough to leave one (see [`UnitTranscript`]).
pub fn get_unit_transcript(store: &dyn GraphRead, unit_id: &str) -> Option<UnitTranscript> {
    let node = store
        .get_node(&synthetic_symbol(crate::execute::WORK_OUTPUT, unit_id))
        .ok()??;
    let meta_str = |key: &str| {
        node.metadata
            .get(key)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    };
    // Pre-`resolution` records were only written on approval — absent marker ⇒ resolved.
    let resolution = meta_str(crate::execute::RESOLUTION_KEY)
        .unwrap_or_else(|| crate::execute::RESOLUTION_RESOLVED.to_string());
    let partial = resolution == crate::execute::RESOLUTION_REJECTED;
    let denial = node
        .metadata
        .get("denial")
        .and_then(|v| serde_json::from_value::<UnitDenial>(v.clone()).ok());
    Some(UnitTranscript {
        unit_id: unit_id.to_string(),
        resolution,
        partial,
        phase_status: meta_str("phase_status"),
        output: meta_str("output"),
        denial_reason: meta_str("denial_reason"),
        denial,
    })
}

/// A session plus its ordered units — the read the UI builds its project list from.
#[derive(Debug, Clone)]
pub struct SessionView {
    pub session: AgentSession,
    pub units: Vec<WorkUnit>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FINDING-019: the ONE human-confirm parser accepts the three real tokens (absent = unattended
    /// default) and FAILS CLOSED on everything else — a typo must not silently downgrade to an
    /// ungated run. Mutation: change the unknown-token arm to `Ok(Self::None)` and the typo/`before:x`
    /// assertions flip to Ok, failing here.
    #[test]
    fn human_confirm_parse_is_canonical_and_fails_closed() {
        // The legitimate tokens.
        assert_eq!(HumanConfirm::parse(None), Ok(HumanConfirm::None));
        assert_eq!(HumanConfirm::parse(Some("")), Ok(HumanConfirm::None));
        assert_eq!(HumanConfirm::parse(Some("none")), Ok(HumanConfirm::None));
        assert_eq!(HumanConfirm::parse(Some("all")), Ok(HumanConfirm::All));
        assert_eq!(
            HumanConfirm::parse(Some("before:2")),
            Ok(HumanConfirm::Before(2))
        );
        assert_eq!(
            HumanConfirm::parse(Some("  before: 3 ")),
            Ok(HumanConfirm::Before(3)),
            "surrounding/inner whitespace is tolerated"
        );

        // FAIL CLOSED — never a silent None. A typo, an unknown word, and a non-numeric ordinal.
        assert!(
            HumanConfirm::parse(Some("al")).is_err(),
            "a typo of 'all' must be rejected, not silently downgraded to None"
        );
        assert!(HumanConfirm::parse(Some("yes")).is_err());
        assert!(
            HumanConfirm::parse(Some("before:x")).is_err(),
            "an unparseable ordinal must be rejected, not silently None"
        );
        assert!(HumanConfirm::parse(Some("before:")).is_err());
    }

    fn sample_session() -> AgentSession {
        AgentSession {
            id: "s-demo".to_string(),
            workflow_id: "wf-s-demo".to_string(),
            problem: "Build a thing".to_string(),
            entity_mode: EntityMode::Shared,
            collection_scope: Some("wicked-agent/s-demo/shared".to_string()),
            clis: vec!["claude".to_string(), "agy".to_string()],
            status: SessionStatus::Planning,
            human_confirm: HumanConfirm::Before(2),
            unit_ix: 0,
            attempt: 0,
            workdir: None,
            repo_ref: None,
            extra_write_roots: Vec::new(),
            project_graph: None,
            archived_at: None,
            archive_note: None,
        }
    }

    #[test]
    fn session_round_trips_through_node() {
        let s = sample_session();
        let back = AgentSession::from_node(&s.to_node()).expect("from_node");
        assert_eq!(
            s, back,
            "AgentSession must survive a node round-trip losslessly"
        );
    }

    #[test]
    fn unit_round_trips_through_node() {
        let mut u = WorkUnit::pending("s-demo:u1", "s-demo", 1, "Do step one");
        u.assigned_cli = Some("claude".to_string());
        u.status = UnitStatus::Distributed;
        let back = WorkUnit::from_node(&u.to_node()).expect("from_node");
        assert_eq!(
            u, back,
            "WorkUnit must survive a node round-trip losslessly"
        );
    }

    /// A unit distributed before `seated` existed must still load. `WorkUnit::from_node` parses the
    /// whole struct out of `metadata`, and `session_units` drops anything that fails
    /// (`from_node(n).ok()`), so a required `seated` would have made every historical council unit
    /// vanish from the run view with no error anywhere — the run would just show fewer units than
    /// it ran. Caught in review on #151.
    #[test]
    fn a_council_unit_stored_before_seated_existed_still_loads() {
        let mut u = WorkUnit::pending("s-legacy:u1", "s-legacy", 1, "Do step one");
        u.status = UnitStatus::Distributed;
        u.assigned_cli = Some("claude".to_string());
        let mut node = u.to_node();

        // Rewrite the routing artifact into its pre-`seated` shape, exactly as it sits on disk.
        node.metadata.insert(
            "routing".to_string(),
            serde_json::json!({
                "method": "council",
                "winner": "claude",
                "agreement_pct": 100,
                "returned": 1,
                "dissent": 0,
            }),
        );

        let back = WorkUnit::from_node(&node).expect("a pre-`seated` council unit must still load");
        let routing = back.routing.expect("a council unit carries routing");
        let RoutingInfo::Council {
            seated, returned, ..
        } = &routing
        else {
            panic!("expected council routing, got {routing:?}");
        };
        assert_eq!(*returned, 1);
        assert_eq!(
            *seated, None,
            "an absent seat count is UNKNOWN — inferring `seated == returned` would relabel every \
             historical collapsed council as a complete one"
        );

        // …and it goes back out as an explicit `null`, NOT as an absent key. There is no
        // `skip_serializing_if` here on purpose: the event surface already emits `seated: null`
        // when unknown, so a UI reading a routing artifact and a UI reading the event stream get
        // ONE rule for "unknown" instead of two. It is pinned by a test because the run view
        // reserializes every unit it loads, which makes `null` — not absence — the shape a
        // consumer actually sees for a legacy council.
        let wire = serde_json::to_value(&routing).expect("serialize");
        assert_eq!(
            wire["seated"],
            serde_json::Value::Null,
            "unknown must reach consumers as null, got {wire}"
        );
    }

    #[test]
    fn units_are_stage_classified_from_their_description() {
        assert_eq!(StageKind::classify("Add JWT auth"), StageKind::Build);
        assert_eq!(StageKind::classify("Then review it"), StageKind::Review);
        assert_eq!(
            StageKind::classify("Write functional tests"),
            StageKind::Test
        );
        assert_eq!(
            StageKind::classify("Research the codebase"),
            StageKind::Recon
        );
        // The classification rides through `pending` + the node round-trip.
        let u = WorkUnit::pending("s:u1", "s", 1, "Adversarial review of the change");
        assert_eq!(u.stage, StageKind::Review);
        assert_eq!(
            WorkUnit::from_node(&u.to_node()).unwrap().stage,
            StageKind::Review
        );
    }

    #[test]
    fn session_and_units_persist_and_read_back_from_the_store() {
        use wicked_apps_core::open_store;
        let dir = std::env::temp_dir().join("wicked-core-domain-test");
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("domain.db");
        let _ = std::fs::remove_file(&db);
        let mut store = open_store(Some(db.to_str().unwrap())).expect("open_store");

        let s = sample_session();
        put_node(&mut store, s.to_node()).expect("put session");
        put_node(
            &mut store,
            WorkUnit::pending("s-demo:u1", "s-demo", 1, "step one").to_node(),
        )
        .expect("put unit 1");
        put_node(
            &mut store,
            WorkUnit::pending("s-demo:u2", "s-demo", 2, "step two").to_node(),
        )
        .expect("put unit 2");

        let read = get_session(&store, "s-demo")
            .expect("get_session")
            .expect("present");
        assert_eq!(read, s);
        let units = session_units(&store, "s-demo").expect("session_units");
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].ord, 1);
        assert_eq!(units[1].description, "step two");
    }
}
