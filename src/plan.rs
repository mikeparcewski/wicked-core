//! PLAN — deterministic decomposition of a problem into ordered work units.
//! Two planners, both pure and deterministic (no randomness, no model):
//!   * [`plan_units`] — free-text: ONE unit, the brief verbatim (D-11). The operator's prose is the
//!     unit's description as written — no sentence / line / semicolon split (core#393: a
//!     three-paragraph recon brief became 11 councils per repo; "launch nothing until approved"
//!     became its own unit). The legacy path; def runs take [`plan_from_def`].
//!   * [`plan_from_def`] — data-driven: derives one unit per [`WorkflowDef`] phase, taking each
//!     unit's [`StageKind`] from the phase's declared `kind` (never a keyword guess); the backing
//!     phase is encoded in the unit id (`<session>:<phase_id>`). The plan is a function of workflow
//!     *data* (Law 2), so a new workflow changes the plan without touching this module.

use crate::domain::WorkUnit;
use crate::workflow::WorkflowDef;

/// Separator folding a phase's own instructions onto its unit description (FINDING-011).
///
/// SINGLE-LINE by contract. The description IS the worker's prompt, and the PTY session runner
/// submits a turn on the FIRST newline (`session_runner` writes `{prompt}\n` to an interactive,
/// line-based PTY). A `\n`-joined description would submit only the `<phase> — <intent>` head — the
/// near-identical prompt this finding set out to kill — and strand the instructions as a stray
/// follow-up line that desyncs the reused session's result sentinel. So the fold uses the same
/// ` ||| ` segment marker `execute_wrapped`'s `LAYOUT_PREFIX` and `assumptions::PROMPT_CONVENTION`
/// already use for exactly this reason (both documented single-line-by-contract). Two guards keep
/// it honest: `folded_instructions_never_introduce_a_newline_into_the_prompt` here, and the
/// call-site `pty_unit_prompt` refusal in `execute_wrapped`.
const INSTRUCTION_SEP: &str = " ||| ";

/// The recognizable head of the engine-side scope preamble (core#283) — a const so the tests that
/// assert its presence/absence and any operator grepping a prompt share one spelling.
///
/// It used to read `PHASE SCOPE (enforced):` and that word was FALSE (core#296): a prompt cannot
/// enforce itself, and run `d1bc72c2` proved it — a `design` unit carrying this exact preamble wrote
/// `src/board/attentionReason.ts` before the build phase ran, and the governance hook watched both
/// writes go by with `decision=allow`. A prompt string that claims enforcement is worse than one
/// that doesn't, because the claim is what stops anyone looking for the missing gate. The word is
/// gone from the prompt and the enforcement now lives where enforcement can live: the gate refuses
/// a pre-build phase's non-documentation Write/Edit before the tool call runs — see
/// [`crate::gate_hook::phase_scope_denial`], reached from `evaluate_tool_call` on both carriers.
pub(crate) const PHASE_SCOPE_PREFIX: &str = "PHASE SCOPE:";

/// The scope preamble injected into the UNIT PROMPT of every PRE-BUILD, non-creator phase
/// (core#283). Phase-role scope was a suggestion: a Neutral pre-build phase (e.g. `feature`'s
/// `design`) received the same problem statement as every phase and routinely implemented the
/// entire deliverable, collapsing the design-before-build ladder — proven twice, including against
/// an explicit prompt-level discipline paragraph, so this is injected ENGINE-SIDE from def data
/// (role + `executes_code` + declaration order), never hardcoded per workflow id and never left to
/// workflow prose. Single-line by construction: the description IS the PTY prompt
/// (see [`INSTRUCTION_SEP`]).
///
/// This is the PROMPT half and nothing more — it TELLS the worker the scope, it does not hold it to
/// it (core#296). The holding is [`crate::gate_hook::phase_scope_denial`], which refuses the
/// non-documentation write itself; the deny message names the same rule this sentence states, so a
/// worker that ignores the prompt still gets a legible reason at the tool call.
fn phase_scope_preamble(phase_id: &str) -> String {
    format!(
        "{PHASE_SCOPE_PREFIX} this is the {phase_id} phase. Produce this phase's deliverable only \
         (analysis/design/plan as applicable). Do NOT write or commit production code; \
         implementation belongs to a later phase."
    )
}

/// Plan a free-text `problem` as exactly ONE [`WorkUnit`] owned by `session_id`: the unit's
/// description is the trimmed problem verbatim (newlines kept — the live carriers pass the prompt
/// as an argv element / a JSON string and carry no line limit; the production-dead PTY runner keeps
/// its own named refusal). An empty problem plans the unit `"unit"`. Unit id `<session_id>:u1`.
///
/// D-11 (core#393 / crew #471 / #473): the old planner split the prose on newlines, sentence
/// terminators and semicolons and minted one unit per piece, so a multi-paragraph brief fanned
/// out into a council per sentence. The split had no journey; deleting it makes "a free-text
/// problem plans one unit" literally true. Def-driven runs are unaffected ([`plan_from_def`]).
pub fn plan_units(problem: &str, session_id: &str) -> Vec<WorkUnit> {
    let trimmed = problem.trim();
    let description = if trimmed.is_empty() {
        "unit".to_string()
    } else {
        trimmed.to_string()
    };
    let mut unit = WorkUnit::pending(format!("{session_id}:u1"), session_id, 1, description);
    // F-7R2-005: a prose-planned unit declares nothing — no `executes_code`, no
    // `verified_evidence`, no pinned validator — so nothing else will ever gate its
    // work. It carries the DEFAULT floor: if it changes the worktree tree, the
    // repository's own checks run and a distinct judge is convened (or the gate says
    // `ungated`, and why).
    unit.default_floor = true;
    vec![unit]
}

/// Decompose a run into ordered [`WorkUnit`]s from a [`WorkflowDef`] — one unit per phase, in the
/// def's phase order. Unlike [`plan_units`], the stage is taken from each phase's declared `kind`
/// (data-driven, not keyword-classified). `intent` is the run's problem statement; each unit's
/// description scopes that intent to its phase so the gate gets meaningful `work` context. Unit ids
/// are `<session_id>:<phase_id>` (stable across resumes) — that id is the backing-phase linkage;
/// `phase_ref` is left untouched (the execute path owns it).
pub fn plan_from_def(def: &WorkflowDef, intent: &str, session_id: &str) -> Vec<WorkUnit> {
    // Precondition: `def` is validated — phase ids are unique, so `<session>:<phase_id>` unit ids
    // are collision-free. The registry only ever hands out validated defs (`register` validates),
    // so the runtime path upholds this; the assert catches a raw unvalidated def in dev.
    debug_assert!(
        {
            let mut seen = std::collections::HashSet::new();
            def.phases.iter().all(|p| seen.insert(p.id.as_str()))
        },
        "plan_from_def requires a validated def (unique phase ids); call WorkflowDef::validate first"
    );
    let intent = intent.trim();
    // core#283: the index where implementation legitimately begins — the def's first
    // `executes_code` Creator phase. Phases BEFORE it that play neither creator nor evaluator are
    // the pre-build ladder (clarify/design/plan …): their prompts gain the scope preamble and
    // their units are marked `pre_build_scope` so the completion path can WARN when one implements
    // anyway. `None` (a def with no code-executing creator, e.g. `collab`, `survey-repo`) ⇒ there
    // is no ladder to protect and no phase gets the preamble.
    let first_code_creator = def
        .phases
        .iter()
        .position(|p| p.executes_code && p.role == crate::workflow::PhaseRole::Creator);
    def.phases
        .iter()
        .enumerate()
        .map(|(i, phase)| {
            let ord = (i + 1) as u32;
            let mut description = if intent.is_empty() {
                phase.id.clone()
            } else {
                format!("{} — {intent}", phase.id)
            };
            // FINDING-011: fold the phase's own INSTRUCTIONS into the description. The description
            // IS the worker's prompt (`execute_wrapped::skill_prompt` sends it bare on the authored
            // path), so without this every phase of a multi-phase workflow gets a prompt that
            // differs only by the phase-id token — N recon phases each re-survey the whole intent.
            // Appended after the intent so the shared goal still leads and the phase's slice of it
            // follows; a phase with no instructions keeps the historical prompt byte-exact.
            // Joined with a SINGLE-LINE separator (`INSTRUCTION_SEP`): a `\n` here would be submitted
            // by the line-based PTY runner as an early turn end, sending only the head and stranding
            // the instructions — the very failure this fold exists to remove, reintroduced.
            if let Some(instr) = phase
                .instructions
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                description.push_str(INSTRUCTION_SEP);
                description.push_str(instr);
            }
            // core#283 PHASE-ROLE SCOPE, the PROMPT half (core#296 — it was called "the enforced
            // half" here, and it enforced nothing; the enforcement is the gate, below). Prompt-level
            // discipline proved insufficient twice, so the planner itself injects the scope preamble
            // into the unit prompt of every phase that (a) plays neither Creator nor Evaluator,
            // (b) runs BEFORE the first `executes_code` Creator phase, and (c) is agent-executed (a
            // Tool phase's description is argv context, never a prompt). Threaded ALONGSIDE the
            // instructions fold above (same single-line separator, appended after — the shared
            // intent and the phase's own slice still lead), so authored instructions are never
            // clobbered.
            let pre_build_scope = first_code_creator.is_some_and(|b| i < b)
                && !matches!(
                    phase.role,
                    crate::workflow::PhaseRole::Creator | crate::workflow::PhaseRole::Evaluator
                )
                && !matches!(phase.executor, crate::workflow::PhaseExecutor::Tool { .. });
            if pre_build_scope {
                description.push_str(INSTRUCTION_SEP);
                description.push_str(&phase_scope_preamble(&phase.id));
            }
            let mut unit = WorkUnit::pending(
                format!("{session_id}:{}", phase.id),
                session_id,
                ord,
                description,
            );
            // Stage is DATA from the def, not a keyword guess over the description. The phase linkage
            // lives in the unit id (`<session>:<phase_id>`) — we do NOT touch `phase_ref`, which the
            // execute path owns (it records the orchestration phase, set at execute time).
            unit.stage = phase.kind;
            // Carry the phase's skill + runtime allowlist (DES-EXEC-001 §4.1/§4.2) onto the unit so the
            // step runner invokes the right skill under least-privilege — pure data from the def.
            unit.skill_ref = phase.skill_ref.clone();
            unit.allowed_skills = phase.allowed_skills.clone();
            // Carry the phase's declared human-confirm gate (§3) so the DEF drives when the run pauses
            // for a human, not just the run-level --confirm flag.
            unit.gate = phase.gate;
            // Carry the evaluator≠creator role (§4) so the gate can do real artifact-passing (an
            // Evaluator unit reviews the prior Creator's cold output).
            unit.role = phase.role;
            // Carry the step OWNER (DES-TEAMING-002 §8.8) the same way: pure data from the def.
            unit.owner = phase.owner;
            // Carry the DECLARED dependency graph (FINDING-024). The def states which phases this one
            // consumes; the engine honored that for ordering and dropped it for context, so an
            // Evaluator phase declared `.after("build")` still ran blind to the build. Carrying it
            // onto the unit is what lets the dispatch site inject the right priors — and keeps the
            // bound author-controlled rather than a guessed "last N units".
            unit.depends_on = phase.depends_on.clone();
            // Carry the phase's DECLARED deliverables (FINDING-101). PhaseDef parsed this and
            // nothing read it, so a workflow could list required outputs the engine never verified.
            // The completion path checks them, mirroring how validator/gate/role flow from def to unit.
            unit.required_deliverables = phase.required_deliverables.clone();
            // Carry the phase's `executes_code` declaration (crew#311 / core#297 §2) so the
            // completion path's CODE-EVIDENCE floor can re-derive a build phase's "done" from the
            // worktree diff — the fold has no route back to the def, so like role/gate/deps the
            // declaration must ride the unit.
            unit.executes_code = phase.executes_code;
            // Carry the tool command for Tool-executor phases so the actor can run it directly.
            if let crate::workflow::PhaseExecutor::Tool { cmd } = &phase.executor {
                unit.tool_cmd = Some(cmd.clone());
            }
            // The marker BOTH other halves read. core#283 gave it one consumer — the completion
            // path's after-the-fact warning (`actor::phase_scope_warning`, still live: it catches
            // what the gate cannot see, e.g. a `Bash` heredoc). core#296 gave it the one that
            // actually holds the scope: the launcher rides it to the governance gate
            // (`gate_hook::PRE_BUILD_SCOPE_ENV` on the hook-subprocess carrier,
            // `BoundaryCtx::pre_build_scope` in-process), which REFUSES a non-documentation
            // Write/Edit before it lands. One field, so the prompt, the gate and the warning can
            // never disagree about which phases are pre-build.
            unit.pre_build_scope = pre_build_scope;
            // F-036 — the WORKTREE GUARD marker. A def-driven, AGENT-executed phase that declared
            // `executes_code: false` (an evaluator, a recon rung, a review — or a creator whose
            // deliverable is a document outside the tree) may not change the tree it works in:
            // the actor snapshots the worktree at dispatch and the gate denies any non-exempt
            // change when the work ends (`worktree_guard`). Read off the def, not guessed — a
            // prose-planned unit carries no declaration and is never guarded; a Tool phase is the
            // engine's own command (a `deliver` push MOVES HEAD on purpose). What the unit may
            // WRITE is NOT decided here: the carriers derive that from this marker together with
            // `role` and the run's tree (`write_posture`, F-4R2-004), so a creator keeps its
            // declared write roots.
            let is_tool = matches!(phase.executor, crate::workflow::PhaseExecutor::Tool { .. });
            unit.worktree_guarded = !phase.executes_code && !is_tool;
            // F-7R2-005 — the DEFAULT floor marker for an agent phase that NO LATER phase
            // verifies: when no `verified_evidence` phase follows this one, no `repo_checks_floor`
            // will ever re-derive its work, so a unit that changes the tree owes the repository's
            // own checks and a distinct judge itself. A NON-creator phase FOLLOWED by a verify
            // phase leaves the floor to it; a creator AFTER the def's verify phase is floored
            // (review of #449, FL-3).
            //
            // core#467 — the def's `executes_code` CREATOR is floored EVEN WHEN a later phase
            // verifies. The `bug` `fix` gate used to leave the checks to `verify` (its gate is
            // `auto`, and hard-failing on checks the def routes to `verify`'s human gate was the
            // worry); on 2026-09-13 that let a fix worker hand a tree failing typecheck AND lint
            // to the read-only evaluator, which fixed it in place and tripped the guard — the run
            // was lost with no route back. The creator now owes provision + typecheck + lint +
            // (targeted) tests at the END of its own phase (`repo_checks::FloorStage::Creator`;
            // a failure the run base shares never denies, a regression does). A red creator
            // floor pauses the run at the escalation gate on the creator (core#464) one phase
            // earlier, with the check tails on the record, instead of at the evaluator's guard
            // escalation; the rework route (re-dispatch with the tails) is S4b.
            let later_verifies = def.phases[i + 1..].iter().any(|p| p.verified_evidence);
            let code_creator =
                phase.executes_code && phase.role == crate::workflow::PhaseRole::Creator;
            unit.default_floor = !is_tool && (!later_verifies || code_creator);
            // F-039 — the REPO CHECKS floor marker: the def's code-VERIFYING step, i.e. a
            // `verified_evidence` agent phase with an `executes_code` Creator before it. The engine
            // runs the repository's own checks in the worktree after the seat's work and folds the
            // exit codes into the gate (`repo_checks`). Same selection rule as the evidence floor's
            // "a diff is the evidence" test, so the two floors always agree on which phase
            // verifies code.
            unit.repo_checks_floor = phase.verified_evidence
                && !is_tool
                && def.phases[..i]
                    .iter()
                    .any(|p| p.executes_code && p.role == crate::workflow::PhaseRole::Creator);
            unit
        })
        .collect()
}

/// Copy the run's BASE skill (core#468) onto every AGENT unit of the plan — never onto a Tool
/// unit, whose description is argv context and never a prompt. `None` (or a blank name) leaves
/// every unit as planned. Applied at the plan choke point (`pipeline::pre_distribute`), AFTER
/// the def or the prose planner produced the units and after the intake admission judged the
/// skill present (`skills_snapshot::admit_base_skill`), so def-driven and prose-planned runs get
/// the same directive from the same code — and the planner itself stays pure data-in, units-out.
pub(crate) fn apply_base_skill(units: &mut [WorkUnit], base: Option<&str>) {
    let Some(base) = base.map(str::trim).filter(|b| !b.is_empty()) else {
        return;
    };
    for u in units.iter_mut().filter(|u| u.tool_cmd.is_none()) {
        u.base_skill_ref = Some(base.to_string());
    }
}

/// Bind a run's repo into the placeholders its Tool phases declare.
///
/// A Tool phase's argv is DATA from the workflow def, which is shared by every run of that id. The
/// paths a run actually needs are not: they belong to the repo the run targets. Crew used to close
/// that gap by rewriting the def with one repo's absolute paths and writing it to a single shared
/// overlay file per launch — so three concurrent registrations raced on one file and two of them
/// indexed a third repo's tree into a third repo's database, reported under their own names
/// (FINDING-075, wicked-crew#196). The lock contention that exposed it was luck; the general case is
/// a run that silently does another run's work.
///
/// Substituting here removes the shared artifact entirely. The def stays constant and shared; the
/// per-run values reach the unit, which is already per-run and already persisted.
///
/// Unresolved tokens are an ERROR, never a passthrough. A command carrying a literal `{repo_root}`
/// would be handed to a shell as a path that cannot exist — a confusing failure at best, and at
/// worst (for a tool that treats an unknown path as "index the cwd") the FINDING-067 shape.
pub fn bind_repo_paths(units: &mut [WorkUnit], repo: &crate::repo::RepoEntry) {
    for unit in units.iter_mut() {
        let Some(cmd) = unit.tool_cmd.as_mut() else {
            continue;
        };
        for arg in cmd.iter_mut() {
            if arg == crate::workflow::REPO_ROOT_TOKEN {
                *arg = repo.root_path.clone();
            } else if arg == crate::workflow::CODE_GRAPH_DB_TOKEN {
                *arg = repo.code_graph_db.clone();
            }
        }
    }
}

/// Every placeholder a Tool phase may declare — the set [`bind_repo_paths`] can satisfy.
const REPO_TOKENS: &[&str] = &[
    crate::workflow::REPO_ROOT_TOKEN,
    crate::workflow::CODE_GRAPH_DB_TOKEN,
];

/// The `<phase>: <token>` pairs a def declares that no repo was bound for.
///
/// Separate from [`bind_repo_paths`] so the caller can refuse the launch BEFORE anything is
/// persisted: a run whose def wants a repo but was launched without one must fail at the door, not
/// dispatch a command with a brace-wrapped literal in it.
pub fn unbound_repo_tokens(units: &[WorkUnit]) -> Vec<String> {
    let mut out = Vec::new();
    for unit in units {
        let Some(cmd) = &unit.tool_cmd else { continue };
        for arg in cmd {
            if REPO_TOKENS.contains(&arg.as_str()) {
                // `phase_id()` and not `unit.id`: the id is `<session>:<phase>`, so naming the unit
                // repeats the session on every line of a message that is already about one run.
                // Falls back to the full id for a hand-built unit that carries no session prefix.
                out.push(format!("{}: {arg}", unit.phase_id().unwrap_or(&unit.id)));
            }
        }
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────────────────────────
// Plan composition (DES-TEAMING-002 §8.3, seam C1): catalog entries + a plan's steps → the per-run
// `WorkflowDef`. The ONLY constructor of a per-run def.
// ─────────────────────────────────────────────────────────────────────────────────────────────

/// The id [`compose`] gives the def it returns. The caller that registers a composed def renames
/// it to `"<run>:plan-<rev>"` (§8.3); compose itself knows nothing of runs.
pub const COMPOSED_DEF_ID: &str = "plan";

/// A plan's ordered steps — the `steps[]` of a `plan.proposed` payload (§8.4). `deny_unknown_fields`
/// so a misspelled key is refused at parse, never silently dropped.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanSteps {
    pub steps: Vec<PlanStep>,
}

/// One plan step: the catalog entry it instantiates, its phase id, and the step fields §8.3 lets
/// a step set. Every field but `catalog` and `id` is optional; an absent field keeps the entry's
/// value. `deny_unknown_fields`: a misspelled step key is refused (C1 acceptance (c)).
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanStep {
    /// The catalog id (`understand`, `build`, … — `crate::catalog::CATALOG_IDS`).
    pub catalog: String,
    /// The phase id in the composed def (unique within the plan; referenced by `depends_on`).
    pub id: String,
    /// Free text (§8.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// May only RAISE the entry's gate along `auto` < `human_confirm_if` <
    /// `human_confirm{unconditional:false}` < `human_confirm{unconditional:true}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<crate::workflow::GateSpec>,
    /// Free (no production reader). `null` clears it; absent keeps the entry's.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub gate_type: Option<Option<crate::workflow::GateType>>,
    /// Set only if the entry has none: a step may ADD a pin to an unpinned entry (approval is
    /// enforced at attach, `pipeline::attach_pinned_validators`). On a pinned entry, restating the
    /// entry's pin is a no-op, a different pin is refused (`pin_changed`, even an approved one), and
    /// an explicit `null` is a removal and is refused (`pin_removed`).
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub validator_pin: Option<Option<String>>,
    /// May be raised to `true`; never lowered on an entry that sets it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executes_code: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_skills: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_deliverables: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depends_on: Option<Vec<String>>,
    /// Only on the Tool entries (`run`, `deliver`), where it is REQUIRED (the command).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor: Option<crate::workflow::PhaseExecutor>,
    /// `pa` (the default) or `team` (§8.8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<crate::workflow::StepOwner>,
    /// Only on `run`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<crate::domain::StageKind>,
    /// Never changes: accepted only when it equals the entry's role, so a plan cannot move
    /// evaluator ≠ creator (it stays a property of the catalog).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<crate::workflow::PhaseRole>,
}

/// Deserialize a PRESENT field (value or `null`) as `Some(..)`, so an absent field (`None`, via
/// `default`) is distinguishable from an explicit `null` (`Some(None)`).
fn present<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    <Option<T> as serde::Deserialize>::deserialize(d).map(Some)
}

/// Why [`compose`] refused a plan. Every variant names the step and a stable
/// [`reason`](PlanRefusal::reason) token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanRefusal {
    /// The step names a catalog id the catalog does not define.
    UnknownCatalogEntry { step: String, catalog: String },
    /// The step sets a `role` other than its entry's.
    RoleChanged { step: String, catalog: String },
    /// The step's gate is weaker than its entry's.
    GateLowered { step: String, catalog: String },
    /// The step clears the pin of an entry that carries one.
    PinRemoved { step: String, catalog: String },
    /// The step sets a pin other than the one its entry carries (a swap, even to an approved pin).
    PinChanged { step: String, catalog: String },
    /// The step sets `executes_code: false` on an entry that sets it.
    ExecutesCodeLowered { step: String, catalog: String },
    /// The step sets a `kind` other than its entry's on an entry other than `run`.
    KindNotAllowed { step: String, catalog: String },
    /// The step sets an `executor` on an Agent entry.
    ExecutorNotAllowed { step: String, catalog: String },
    /// A Tool-entry step (`run`, `deliver`) supplies no non-empty Tool command.
    ToolCommandMissing { step: String, catalog: String },
    /// The step replaces `instructions` its entry already carries.
    InstructionsChanged { step: String, catalog: String },
    /// The step replaces the `skill_ref` its entry already carries (a `security_review` step
    /// cannot run a non-security skill).
    SkillRefChanged { step: String, catalog: String },
    /// The step replaces the non-empty `allowed_skills` its entry already carries.
    AllowedSkillsChanged { step: String, catalog: String },
    /// The step's `required_deliverables` drop one its entry requires.
    DeliverableRemoved { step: String, catalog: String },
    /// The composed def fails the registry's own validation (empty, duplicate or dangling ids,
    /// forward dependencies, a code phase whose gate evaluates nothing).
    InvalidDef(crate::workflow::WorkflowDefError),
}

impl PlanRefusal {
    /// The stable reason token (what a caller, the studio, or a test matches on).
    pub fn reason(&self) -> &'static str {
        match self {
            PlanRefusal::UnknownCatalogEntry { .. } => "unknown_catalog_entry",
            PlanRefusal::RoleChanged { .. } => "role_changed",
            PlanRefusal::GateLowered { .. } => "gate_lowered",
            PlanRefusal::PinRemoved { .. } => "pin_removed",
            PlanRefusal::PinChanged { .. } => "pin_changed",
            PlanRefusal::ExecutesCodeLowered { .. } => "executes_code_lowered",
            PlanRefusal::KindNotAllowed { .. } => "kind_not_allowed",
            PlanRefusal::ExecutorNotAllowed { .. } => "executor_not_allowed",
            PlanRefusal::ToolCommandMissing { .. } => "tool_command_missing",
            PlanRefusal::InstructionsChanged { .. } => "instructions_changed",
            PlanRefusal::SkillRefChanged { .. } => "skill_ref_changed",
            PlanRefusal::AllowedSkillsChanged { .. } => "allowed_skills_changed",
            PlanRefusal::DeliverableRemoved { .. } => "deliverable_removed",
            PlanRefusal::InvalidDef(_) => "invalid_def",
        }
    }
}

impl std::fmt::Display for PlanRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let r = self.reason();
        match self {
            PlanRefusal::UnknownCatalogEntry { step, catalog } => {
                write!(f, "{r}: step {step} names catalog entry {catalog}, which the catalog does not define")
            }
            PlanRefusal::RoleChanged { step, catalog } => write!(
                f,
                "{r}: step {step} sets a role other than {catalog}'s — a step never changes role \
                 (evaluator ≠ creator is a property of the catalog)"
            ),
            PlanRefusal::GateLowered { step, catalog } => {
                write!(
                    f,
                    "{r}: step {step} lowers {catalog}'s gate — a step may only raise it"
                )
            }
            PlanRefusal::PinRemoved { step, catalog } => write!(
                f,
                "{r}: step {step} removes {catalog}'s validator pin — a step may add a pin to an \
                 unpinned entry, never remove one"
            ),
            PlanRefusal::PinChanged { step, catalog } => write!(
                f,
                "{r}: step {step} swaps {catalog}'s validator pin — a pinned entry's pin is final; \
                 a step may add a pin only to an unpinned entry"
            ),
            PlanRefusal::ExecutesCodeLowered { step, catalog } => write!(
                f,
                "{r}: step {step} sets executes_code false on {catalog}, which sets it"
            ),
            PlanRefusal::KindNotAllowed { step, catalog } => write!(
                f,
                "{r}: step {step} changes {catalog}'s kind — only a run step sets its kind"
            ),
            PlanRefusal::ExecutorNotAllowed { step, catalog } => write!(
                f,
                "{r}: step {step} sets an executor on {catalog}, an agent entry — only run and \
                 deliver take one"
            ),
            PlanRefusal::ToolCommandMissing { step, catalog } => write!(
                f,
                "{r}: step {step} instantiates the tool entry {catalog} without a tool command"
            ),
            PlanRefusal::InstructionsChanged { step, catalog } => write!(
                f,
                "{r}: step {step} replaces the instructions {catalog} carries — a step may set \
                 instructions only on an entry that has none"
            ),
            PlanRefusal::SkillRefChanged { step, catalog } => write!(
                f,
                "{r}: step {step} replaces {catalog}'s skill_ref — a step may set a skill only on \
                 an entry that has none"
            ),
            PlanRefusal::AllowedSkillsChanged { step, catalog } => write!(
                f,
                "{r}: step {step} replaces {catalog}'s allowed_skills — a step may scope skills \
                 only on an entry that has no allowlist"
            ),
            PlanRefusal::DeliverableRemoved { step, catalog } => write!(
                f,
                "{r}: step {step} drops a deliverable {catalog} requires — a step may only add \
                 deliverables"
            ),
            PlanRefusal::InvalidDef(e) => write!(f, "{r}: {e}"),
        }
    }
}
impl std::error::Error for PlanRefusal {}

/// How a plan step may treat one field of its catalog entry (DES-TEAMING-002 §8.3). The catalog
/// entry is the FLOOR of its step's controls; only [`FieldRule::Free`] fields may move freely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldRule {
    /// Names the step (`catalog`, `id`); not a control.
    Identity,
    /// Any value — the field constrains nothing (`gate_type` has no production reader; `depends_on`
    /// is the plan's own order, validated by the registry; `owner` is who answers for the step).
    Free,
    /// Only toward stricter: `gate` up the ladder, `executes_code` false → true,
    /// `required_deliverables` may only grow.
    TightenOnly,
    /// Settable only when the entry leaves it unset (`None` / empty); a value the entry carries is
    /// final. Restating the entry's own value is a no-op. For `validator_pin` this bans swaps: a
    /// different pin is `pin_changed` and a `null` is `pin_removed`.
    SetIfUnset,
    /// Only on the Tool entries (`run`, `deliver`), where the entry carries no command and the step
    /// MUST supply one; refused on every agent entry.
    ToolEntriesOnly,
    /// Only on `run`; anywhere else it is fixed (the entry's own value is a no-op).
    RunOnly,
    /// Never changes; the entry's own value is a no-op.
    Fixed,
}

/// Every [`PlanStep`] field and its [`FieldRule`] — the one table `compose` applies
/// ([`apply_step`] has one arm per row, in this order). A test pins that the table covers every
/// `PlanStep` field and that each rule refuses what it forbids.
pub const STEP_FIELD_RULES: [(&str, FieldRule); 15] = [
    ("catalog", FieldRule::Identity),
    ("id", FieldRule::Identity),
    ("role", FieldRule::Fixed),
    ("kind", FieldRule::RunOnly),
    ("gate", FieldRule::TightenOnly),
    ("validator_pin", FieldRule::SetIfUnset),
    ("executes_code", FieldRule::TightenOnly),
    ("required_deliverables", FieldRule::TightenOnly),
    ("executor", FieldRule::ToolEntriesOnly),
    ("instructions", FieldRule::SetIfUnset),
    ("skill_ref", FieldRule::SetIfUnset),
    ("allowed_skills", FieldRule::SetIfUnset),
    ("gate_type", FieldRule::Free),
    ("depends_on", FieldRule::Free),
    ("owner", FieldRule::Free),
];

/// A gate's position on the §8.3 ladder: `auto` < `human_confirm_if` <
/// `human_confirm{unconditional:false}` < `human_confirm{unconditional:true}`.
fn gate_rank(g: crate::workflow::GateSpec) -> u8 {
    use crate::workflow::GateSpec;
    match g {
        GateSpec::Auto => 0,
        GateSpec::HumanConfirmIf(_) => 1,
        GateSpec::HumanConfirm {
            unconditional: false,
        } => 2,
        GateSpec::HumanConfirm {
            unconditional: true,
        } => 3,
    }
}

/// Compose a plan into its per-run [`WorkflowDef`] (DES-TEAMING-002 §8.3): each step instantiates
/// its catalog entry, and the step's fields are applied under the step rules — a step may only
/// make its entry stricter, field by field as [`STEP_FIELD_RULES`] says. Refused, with a named
/// [`PlanRefusal`], when a step loosens any field of its entry, and when the composed def fails
/// the registry's own checks. The def's id is [`COMPOSED_DEF_ID`].
pub fn compose(
    catalog: &[crate::workflow::PhaseDef],
    plan: &PlanSteps,
) -> Result<WorkflowDef, PlanRefusal> {
    let mut phases = Vec::with_capacity(plan.steps.len());
    for step in &plan.steps {
        let Some(entry) = catalog.iter().find(|e| e.id == step.catalog) else {
            return Err(PlanRefusal::UnknownCatalogEntry {
                step: step.id.clone(),
                catalog: step.catalog.clone(),
            });
        };
        phases.push(apply_step(entry, step)?);
    }
    let def = WorkflowDef {
        id: COMPOSED_DEF_ID.to_string(),
        phases,
        base_skill_ref: None,
    };
    // The composed def is judged exactly as a registered one is (`WorkflowRegistry::register`).
    let mut probe = crate::workflow::WorkflowRegistry::default();
    probe
        .register(def.clone())
        .map_err(PlanRefusal::InvalidDef)?;
    Ok(def)
}

/// Apply one step to its catalog entry under [`STEP_FIELD_RULES`] — one arm per row, in the
/// table's order. The entry is the floor: nothing here can make the phase looser than its entry.
fn apply_step(
    entry: &crate::workflow::PhaseDef,
    step: &PlanStep,
) -> Result<crate::workflow::PhaseDef, PlanRefusal> {
    use crate::workflow::PhaseExecutor;
    let refuse =
        |make: fn(String, String) -> PlanRefusal| Err(make(step.id.clone(), step.catalog.clone()));
    let mut phase = entry.clone();
    // catalog, id — Identity.
    phase.id = step.id.clone();
    // role — Fixed.
    if step.role.is_some_and(|r| r != entry.role) {
        return refuse(|step, catalog| PlanRefusal::RoleChanged { step, catalog });
    }
    // kind — RunOnly.
    if let Some(kind) = step.kind {
        if kind != entry.kind && step.catalog != "run" {
            return refuse(|step, catalog| PlanRefusal::KindNotAllowed { step, catalog });
        }
        phase.kind = kind;
    }
    // gate — TightenOnly (the §8.3 ladder).
    if let Some(gate) = step.gate {
        if gate_rank(gate) < gate_rank(entry.gate) {
            return refuse(|step, catalog| PlanRefusal::GateLowered { step, catalog });
        }
        phase.gate = gate;
    }
    // validator_pin — SetIfUnset: add to an unpinned entry (attach refuses an unapproved one);
    // a pinned entry's pin is final — restating it is a no-op, a swap or a removal is refused.
    match (&step.validator_pin, &entry.validator_pin) {
        (None, _) => {}
        (Some(None), Some(_)) => {
            return refuse(|step, catalog| PlanRefusal::PinRemoved { step, catalog });
        }
        (Some(Some(pin)), Some(own)) if pin != own => {
            return refuse(|step, catalog| PlanRefusal::PinChanged { step, catalog });
        }
        (Some(pin), _) => phase.validator_pin = pin.clone(),
    }
    // executes_code — TightenOnly (false → true).
    if let Some(code) = step.executes_code {
        if !code && entry.executes_code {
            return refuse(|step, catalog| PlanRefusal::ExecutesCodeLowered { step, catalog });
        }
        phase.executes_code = code;
    }
    // required_deliverables — TightenOnly: a superset of the entry's.
    if let Some(deliverables) = &step.required_deliverables {
        if !entry
            .required_deliverables
            .iter()
            .all(|d| deliverables.contains(d))
        {
            return refuse(|step, catalog| PlanRefusal::DeliverableRemoved { step, catalog });
        }
        phase.required_deliverables = deliverables.clone();
    }
    // executor — ToolEntriesOnly: required (non-empty) on run/deliver, refused on agent entries.
    let tool_entry = crate::catalog::is_tool_entry(entry);
    match (&step.executor, tool_entry) {
        (Some(_), false) => {
            return refuse(|step, catalog| PlanRefusal::ExecutorNotAllowed { step, catalog });
        }
        (Some(PhaseExecutor::Tool { cmd }), true) if !cmd.is_empty() => {
            phase.executor = PhaseExecutor::Tool { cmd: cmd.clone() };
        }
        (_, true) => {
            return refuse(|step, catalog| PlanRefusal::ToolCommandMissing { step, catalog });
        }
        (None, false) => {}
    }
    // instructions, skill_ref — SetIfUnset.
    if let Some(text) = &step.instructions {
        match &entry.instructions {
            Some(own) if own != text => {
                return refuse(|step, catalog| PlanRefusal::InstructionsChanged { step, catalog });
            }
            _ => phase.instructions = Some(text.clone()),
        }
    }
    if let Some(skill) = &step.skill_ref {
        match &entry.skill_ref {
            Some(own) if own != skill => {
                return refuse(|step, catalog| PlanRefusal::SkillRefChanged { step, catalog });
            }
            _ => phase.skill_ref = Some(skill.clone()),
        }
    }
    // allowed_skills — SetIfUnset (an empty list is unset: no extra scoping).
    if let Some(allowed) = &step.allowed_skills {
        if !entry.allowed_skills.is_empty() && &entry.allowed_skills != allowed {
            return refuse(|step, catalog| PlanRefusal::AllowedSkillsChanged { step, catalog });
        }
        phase.allowed_skills = allowed.clone();
    }
    // gate_type, depends_on, owner — Free.
    if let Some(gate_type) = step.gate_type {
        phase.gate_type = gate_type;
    }
    if let Some(deps) = &step.depends_on {
        phase.depends_on = deps.clone();
    }
    if let Some(owner) = step.owner {
        phase.owner = owner;
    }
    Ok(phase)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::UnitStatus;

    /// D-11 (core#393): free text is ONE unit, the brief verbatim — newlines, sentence
    /// terminators and semicolons are content, not unit boundaries.
    #[test]
    fn free_text_is_one_unit() {
        let units = plan_units("First task.\nSecond task; third task", "s1");
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].description, "First task.\nSecond task; third task");
        assert_eq!(units[0].id, "s1:u1");
        assert_eq!(units[0].ord, 1);
        assert_eq!(units[0].status, UnitStatus::Pending);
        assert!(
            units[0].default_floor,
            "a prose-planned unit carries the default floor"
        );
    }

    /// The studio's recon launcher joins its prefix and the operator's brief with a blank line
    /// (`\n\n`); a paragraph rule would read that as two units per repo — it is one.
    #[test]
    fn a_blank_line_joined_brief_is_still_one_unit() {
        let brief = "Recon: survey the attached codebases.\n\nLaunch nothing until approved.\n\n\
                     Third paragraph; with a semicolon. And a sentence!";
        let units = plan_units(brief, "s1");
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].description, brief);
    }

    #[test]
    fn surrounding_whitespace_is_trimmed_but_inner_newlines_are_kept() {
        let units = plan_units("  \n keep\nthese\nlines \n\n", "s");
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].description, "keep\nthese\nlines");
    }

    #[test]
    fn deterministic_same_input_same_units() {
        assert_eq!(plan_units("Do X; do Y", "s"), plan_units("Do X; do Y", "s"));
    }

    #[test]
    fn empty_problem_falls_back_to_one_unit() {
        let units = plan_units("   ", "s");
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].description, "unit");
    }

    #[test]
    fn decimal_point_does_not_split() {
        assert_eq!(plan_units("Upgrade to version 3.5 now", "s").len(), 1);
    }

    // ---- plan_from_def: the data-driven planner (Law 2) ----
    use crate::workflow::{bug_def, feature_def, migration_def};

    #[test]
    fn plan_from_def_yields_one_unit_per_phase_in_order() {
        let def = feature_def();
        let units = plan_from_def(&def, "add SSO login", "s1");
        assert_eq!(units.len(), def.phases.len());
        // 1:1, same order, unit id derived from the phase id — not from prose splitting. The unit id
        // IS the backing-phase linkage; phase_ref is left for the execute path.
        for (unit, phase) in units.iter().zip(def.phases.iter()) {
            assert_eq!(unit.id, format!("s1:{}", phase.id));
            assert!(unit.phase_ref.is_none(), "plan must not pre-set phase_ref");
        }
        assert_eq!(units[0].ord, 1);
        assert_eq!(units.last().unwrap().ord, units.len() as u32);
        assert!(units.iter().all(|u| u.status == UnitStatus::Pending));
    }

    /// FINDING-024: the DECLARED dependency graph reaches the unit, so the dispatch site can inject
    /// the priors a phase actually consumes. Asserted against the SHIPPED `feature` def rather than a
    /// fixture — the whole finding was that real workflows already declare the edges the engine
    /// dropped, so a synthetic def would prove nothing about them.
    #[test]
    fn plan_from_def_carries_the_declared_dependency_graph_onto_the_unit() {
        let def = feature_def();
        let units = plan_from_def(&def, "add SSO login", "s1");
        for (unit, phase) in units.iter().zip(def.phases.iter()) {
            assert_eq!(
                unit.depends_on, phase.depends_on,
                "phase `{}` must carry its own depends_on verbatim",
                phase.id
            );
        }

        let dep = |id: &str| {
            units
                .iter()
                .find(|u| u.id == format!("s1:{id}"))
                .unwrap_or_else(|| panic!("feature has a `{id}` phase"))
                .depends_on
                .clone()
        };
        // The Evaluator phase declares the Creator phase it reviews — the exact edge whose loss made
        // `adversarial-review` re-solve the original task against a different file.
        assert_eq!(dep("adversarial-review"), vec!["build".to_string()]);
        assert_eq!(dep("test"), vec!["build".to_string()]);
        assert_eq!(dep("review"), vec!["test".to_string()]);
        // The first phase depends on nothing; an empty list must stay empty (not a defaulted guess).
        assert!(dep("clarify").is_empty());
    }

    /// crew#311 / core#297 §2: the phase's `executes_code` declaration rides the unit — the fold's
    /// CODE-EVIDENCE floor has no route back to the def, so without the carry every build unit
    /// would read `false` and the floor would be armed nowhere.
    #[test]
    fn plan_from_def_carries_executes_code_onto_the_unit() {
        let def = feature_def();
        let units = plan_from_def(&def, "add SSO login", "s1");
        for (unit, phase) in units.iter().zip(def.phases.iter()) {
            assert_eq!(
                unit.executes_code, phase.executes_code,
                "phase `{}` must carry its own executes_code verbatim",
                phase.id
            );
        }
        // The concrete shape this exists for: `build` is marked, the prose phases are not.
        let marked = |id: &str| {
            units
                .iter()
                .find(|u| u.id == format!("s1:{id}"))
                .unwrap_or_else(|| panic!("feature has a `{id}` phase"))
                .executes_code
        };
        assert!(marked("build"), "feature/build is the executes_code phase");
        assert!(!marked("clarify") && !marked("design") && !marked("adversarial-review"));
    }

    /// core#468: the base skill lands on every AGENT unit — def-driven and prose-planned alike —
    /// and on NO Tool unit (a tool command has no prompt to lead). `None`/blank is a no-op, so a
    /// run without a base skill plans byte-identical to before the field existed.
    #[test]
    fn the_base_skill_lands_on_every_agent_unit_and_no_tool_unit() {
        use crate::domain::StageKind;
        use crate::workflow::{PhaseDef, PhaseExecutor, PhaseRole, WorkflowDef};
        let def = WorkflowDef {
            base_skill_ref: None,
            id: "bs".into(),
            phases: vec![
                PhaseDef::new("triage", StageKind::Recon),
                PhaseDef::new("index", StageKind::Build).executor(PhaseExecutor::Tool {
                    cmd: vec!["true".into()],
                }),
                {
                    let mut p = PhaseDef::new("verify", StageKind::Test);
                    p.role = PhaseRole::Evaluator;
                    p
                },
            ],
        };
        let mut units = plan_from_def(&def, "fix it", "s");
        let before = units.clone();
        apply_base_skill(&mut units, None);
        assert_eq!(units, before, "no base skill ⇒ the plan is untouched");
        apply_base_skill(&mut units, Some("   "));
        assert_eq!(units, before, "a blank base skill is no base skill");
        apply_base_skill(&mut units, Some(" wicked-garden-governed-worker "));
        assert_eq!(
            units
                .iter()
                .map(|u| u.base_skill_ref.as_deref())
                .collect::<Vec<_>>(),
            vec![
                Some("wicked-garden-governed-worker"),
                None,
                Some("wicked-garden-governed-worker")
            ],
            "trimmed onto both agent units, never onto the tool unit"
        );
        // Prose-planned units carry no def and still get it — the directive is engine-level.
        let mut prose = plan_units("Investigate the flake. Then fix it.", "p");
        apply_base_skill(&mut prose, Some("wicked-garden-governed-worker"));
        assert!(prose
            .iter()
            .all(|u| u.base_skill_ref.as_deref() == Some("wicked-garden-governed-worker")));
    }

    /// core#467: the def's `executes_code` Creator carries the DEFAULT floor even though `verify`
    /// follows it — the repository's checks run at the end of the creator's own phase. The
    /// read-only rungs before it still leave the floor to `verify`, and `verify` keeps its own
    /// declared floor.
    #[test]
    fn plan_from_def_floors_the_code_creator_even_when_a_later_phase_verifies() {
        let def = crate::workflow::bug_def();
        let units = plan_from_def(&def, "fix the bug", "s1");
        let floors = |id: &str| {
            let u = units
                .iter()
                .find(|u| u.id == format!("s1:{id}"))
                .unwrap_or_else(|| panic!("bug has a `{id}` phase"));
            (u.default_floor, u.repo_checks_floor)
        };
        assert_eq!(
            floors("triage"),
            (false, false),
            "a guarded recon rung leaves the floor to verify"
        );
        assert_eq!(floors("reproduce"), (false, false));
        assert_eq!(
            floors("fix"),
            (true, false),
            "the creator owes the floor at the end of its own phase (core#467)"
        );
        assert_eq!(
            floors("verify"),
            (true, true),
            "verify keeps its declared floor"
        );
    }

    /// FINDING-024, the join that makes the fix work at all. `prior_context_label` matches a prior's
    /// `phase_id()` (the unit-id suffix) against this unit's `depends_on` (phase ids copied from the
    /// def). Those are two different vocabularies meeting at a string compare, which is exactly the
    /// shape of FINDING-021 — there the phase token the policy engine selected on and the token the
    /// public API accepted diverged, and every gate silently no-op'd while looking correct.
    ///
    /// Nothing above proves they agree: the plan test proves the list is COPIED, and the actor tests
    /// construct units by hand, so both would still pass if real defs named their dependencies in a
    /// vocabulary `phase_id()` never produces — and the fix would inject nothing, silently, on every
    /// shipped workflow. This asserts the join RESOLVES across every builtin: each declared id must
    /// name a real phase that is planned EARLIER, since `prior_context_label` only offers priors with
    /// a lower ord. A forward or dangling edge is unreachable context, not a handoff.
    #[test]
    fn every_builtin_declares_dependencies_that_actually_resolve_to_earlier_units() {
        let registry = crate::workflow::WorkflowRegistry::with_defaults();
        let mut edges = 0usize;
        for id in registry.ids() {
            let def = registry.get(&id).expect("registry returned its own id");
            let units = plan_from_def(def, "some intent", "s1");
            // The lookup `prior_context_label` performs, built from the same `phase_id()` accessor.
            let by_phase: Vec<(Option<&str>, u32)> =
                units.iter().map(|u| (u.phase_id(), u.ord)).collect();
            for unit in &units {
                for dep in &unit.depends_on {
                    let target = by_phase
                        .iter()
                        .find(|(phase, _)| *phase == Some(dep.as_str()))
                        .unwrap_or_else(|| {
                            panic!(
                                "workflow `{id}`: phase `{}` declares depends_on `{dep}`, which no \
                                 unit's phase_id() yields — the declared graph and the unit-id \
                                 vocabulary have diverged, so injection silently no-ops",
                                unit.phase_id().unwrap_or("<none>")
                            )
                        });
                    assert!(
                        target.1 < unit.ord,
                        "workflow `{id}`: phase `{}` (ord {}) depends on `{dep}` (ord {}), which is \
                         not EARLIER — a forward edge is never offered to the dispatch site",
                        unit.phase_id().unwrap_or("<none>"),
                        unit.ord,
                        target.1
                    );
                    edges += 1;
                }
            }
        }
        // Guard the guard: if the builtins ever stop declaring dependencies this test would pass
        // vacuously while asserting nothing at all.
        assert!(
            edges >= 10,
            "expected the builtin defs to declare a real dependency graph, found {edges} edges"
        );
    }

    #[test]
    fn plan_from_def_takes_stage_from_the_phase_not_the_words() {
        // Every unit shares the SAME prose ("build ..."), which the keyword classifier would
        // stamp Build for all of them. plan_from_def must instead carry each phase's declared
        // kind — proving the stage is data from the def, not a guess over the description.
        let def = feature_def();
        let units = plan_from_def(&def, "build the thing", "s");
        for (unit, phase) in units.iter().zip(def.phases.iter()) {
            assert_eq!(unit.stage, phase.kind, "stage must come from phase.kind");
        }
        // And the def genuinely spans more than one kind (otherwise the test is vacuous).
        let first = units[0].stage;
        assert!(
            units.iter().any(|u| u.stage != first),
            "feature def should span multiple stages"
        );
    }

    #[test]
    fn plan_from_def_scopes_the_intent_into_each_phase() {
        let units = plan_from_def(&bug_def(), "500 on empty cart", "s");
        assert!(units
            .iter()
            .all(|u| u.description.contains("500 on empty cart")));
        assert!(units[0].description.starts_with(&bug_def().phases[0].id));
    }

    #[test]
    fn plan_from_def_is_deterministic() {
        let a = plan_from_def(&migration_def(), "move to pg", "s");
        let b = plan_from_def(&migration_def(), "move to pg", "s");
        assert_eq!(a, b);
    }

    #[test]
    fn plan_from_def_handles_empty_intent() {
        let units = plan_from_def(&feature_def(), "   ", "s");
        // Falls back to the bare phase id (plus, on a pre-build phase like `clarify`, the core#283
        // scope preamble) — never an empty description (gate needs work context).
        assert!(
            units[0]
                .description
                .starts_with(&feature_def().phases[0].id),
            "{}",
            units[0].description
        );
        assert!(units.iter().all(|u| !u.description.is_empty()));
    }

    /// core#283, the PROMPT half, asserted against the SHIPPED `feature` def (the workflow the
    /// collapse was proven on — its `design` phase received the same problem statement as `build`
    /// and implemented the entire deliverable, twice, once THROUGH an explicit prompt-level
    /// discipline paragraph). The scope preamble must land on exactly the pre-build non-creator
    /// phases (`clarify`, `design`) and on no build/evaluator/post-build phase — a preamble on
    /// `build` would scope the creator away from building, which is the inverse failure.
    #[test]
    fn scope_preamble_lands_exactly_on_pre_build_non_creator_phases_of_the_shipped_feature_def() {
        let def = feature_def();
        let units = plan_from_def(&def, "add SSO login", "s1");
        let by_phase = |id: &str| {
            units
                .iter()
                .find(|u| u.id == format!("s1:{id}"))
                .unwrap_or_else(|| panic!("feature has a `{id}` phase"))
        };

        for id in ["clarify", "design"] {
            let u = by_phase(id);
            assert!(
                u.description.contains(PHASE_SCOPE_PREFIX),
                "pre-build phase `{id}` must carry the scope preamble: {}",
                u.description
            );
            assert!(
                u.description.contains(&format!("this is the {id} phase")),
                "the preamble is composed from THIS phase's id, not a workflow-wide blurb: {}",
                u.description
            );
            assert!(
                u.description.contains("add SSO login"),
                "the shared intent still leads — the preamble threads alongside, not instead: {}",
                u.description
            );
            assert!(
                !u.description.contains('\n'),
                "the preamble must stay single-line (the PTY runner submits on the first newline): {}",
                u.description
            );
            // core#296. The preamble used to announce itself as `PHASE SCOPE (enforced):` while
            // enforcing nothing — run d1bc72c2's `design` unit wrote `src/board/attentionReason.ts`
            // under that exact header and the hook allowed it. A prompt cannot enforce itself; the
            // word is what stopped anyone from looking for the gate that was missing.
            assert!(
                !u.description.to_ascii_lowercase().contains("enforced"),
                "the prompt must not CLAIM enforcement — enforcement is the gate \
                 (`gate_hook::phase_scope_denial`), not this sentence: {}",
                u.description
            );
            assert!(
                u.pre_build_scope,
                "`{id}` must carry the marker the GATE reads (core#296) and the completion-path \
                 warning reads (core#283) — an unmarked phase is scoped in prose only"
            );
        }
        // build (Creator), adversarial-review (Evaluator), and the POST-build neutral phases: no
        // preamble, no marker.
        for id in ["build", "adversarial-review", "test", "review"] {
            let u = by_phase(id);
            assert!(
                !u.description.contains(PHASE_SCOPE_PREFIX),
                "`{id}` must NOT carry the preamble: {}",
                u.description
            );
            assert!(!u.pre_build_scope, "`{id}` must not be marked pre-build");
        }
    }

    /// core#283 across EVERY builtin: the preamble is a function of def DATA — role +
    /// `executes_code` + declaration order — never of a workflow id. Three invariants: (1) the
    /// prompt carries the preamble IFF the unit carries the marker (the two halves never diverge);
    /// (2) no creator/evaluator-role unit and no unit at-or-after the first code-executing Creator
    /// is ever scoped; (3) a def with NO code-executing Creator (`collab`, `survey-repo`,
    /// `onboarding`…) gets no preamble anywhere — there is no later build rung to defer to, so the
    /// preamble's promise would be a lie. Vacuity-guarded: the shipped defs must actually produce
    /// marked phases (feature: clarify+design, bug: triage+reproduce, migration: plan).
    #[test]
    fn scope_preamble_is_derived_from_def_data_across_every_builtin() {
        use crate::workflow::{PhaseExecutor, PhaseRole};
        let registry = crate::workflow::WorkflowRegistry::with_defaults();
        let mut marked = 0usize;
        for id in registry.ids() {
            let def = registry.get(&id).expect("registry returned its own id");
            let first_code_creator = def
                .phases
                .iter()
                .position(|p| p.executes_code && p.role == PhaseRole::Creator);
            let units = plan_from_def(def, "some intent", "s1");
            for (ix, (unit, phase)) in units.iter().zip(def.phases.iter()).enumerate() {
                let has_preamble = unit.description.contains(PHASE_SCOPE_PREFIX);
                assert_eq!(
                    has_preamble, unit.pre_build_scope,
                    "workflow `{id}` phase `{}`: prompt preamble and completion marker diverged",
                    phase.id
                );
                if matches!(phase.role, PhaseRole::Creator | PhaseRole::Evaluator) {
                    assert!(
                        !has_preamble,
                        "workflow `{id}` phase `{}` plays {:?} and must never be scoped away \
                         from its role",
                        phase.id, phase.role
                    );
                }
                if first_code_creator.is_none_or(|b| ix >= b) {
                    assert!(
                        !has_preamble,
                        "workflow `{id}` phase `{}` is not PRE-build (no code-executing creator \
                         after it) and must not carry the preamble",
                        phase.id
                    );
                }
                if let PhaseExecutor::Tool { .. } = phase.executor {
                    assert!(
                        !has_preamble,
                        "workflow `{id}` phase `{}` is Tool-executed — its description is argv \
                         context, not a prompt",
                        phase.id
                    );
                }
                marked += has_preamble as usize;
            }
        }
        assert!(
            marked >= 5,
            "the shipped defs must produce pre-build scoped phases (feature ×2, bug ×2, \
             migration ×1) or this guard is vacuous; found {marked}"
        );
    }

    /// core#283 + FINDING-011 interplay: a pre-build phase that ALSO authors `instructions` keeps
    /// them — the preamble threads alongside via the same single-line fold, never clobbers.
    #[test]
    fn scope_preamble_threads_alongside_instructions_without_clobbering_them() {
        use crate::domain::StageKind;
        use crate::workflow::{PhaseDef, PhaseRole};
        let def = WorkflowDef {
            base_skill_ref: None,
            id: "ladder".to_string(),
            phases: vec![
                PhaseDef {
                    instructions: Some("write the design doc and nothing else".to_string()),
                    ..PhaseDef::new("design", StageKind::Recon)
                },
                PhaseDef {
                    executes_code: true,
                    role: PhaseRole::Creator,
                    depends_on: vec!["design".to_string()],
                    ..PhaseDef::new("build", StageKind::Build)
                },
            ],
        };
        let units = plan_from_def(&def, "add SSO", "s");
        let d = &units[0].description;
        assert!(d.contains("add SSO"), "intent survives: {d}");
        assert!(
            d.contains("write the design doc and nothing else"),
            "authored instructions survive: {d}"
        );
        assert!(d.contains(PHASE_SCOPE_PREFIX), "preamble joins them: {d}");
        assert!(
            d.find("write the design doc") < d.find(PHASE_SCOPE_PREFIX),
            "instructions lead, the scope preamble follows: {d}"
        );
        assert!(!d.contains('\n'), "single-line contract holds: {d}");
        assert!(
            !units[1].description.contains(PHASE_SCOPE_PREFIX),
            "the creator phase is never scoped away from building: {}",
            units[1].description
        );
        assert!(!units[1].pre_build_scope);
    }

    /// FINDING-011: a phase's own `instructions` reach ITS unit's description — the worker prompt —
    /// and no other unit's. Without the threading, every unit of an N-phase workflow carries a
    /// prompt that differs only by the phase-id token, so N recon phases run N near-identical
    /// surveys (survey-repo: $3.09 / 1.74M tokens to answer one question three times).
    #[test]
    fn plan_from_def_threads_each_phases_instructions_into_its_own_unit_only() {
        use crate::domain::StageKind;
        use crate::workflow::PhaseDef;
        let instr_a = "map the directory layout and nothing else";
        let instr_b = "identify the language stack and nothing else";
        let def = WorkflowDef {
            base_skill_ref: None,
            id: "instructed".to_string(),
            phases: vec![
                PhaseDef {
                    instructions: Some(instr_a.to_string()),
                    ..PhaseDef::new("a", StageKind::Recon)
                },
                PhaseDef {
                    instructions: Some(instr_b.to_string()),
                    depends_on: vec!["a".to_string()],
                    ..PhaseDef::new("b", StageKind::Recon)
                },
                PhaseDef::new("c", StageKind::Recon),
            ],
        };
        let units = plan_from_def(&def, "survey the repo", "s");

        // Each unit carries the shared intent AND its own phase's instructions…
        assert!(units[0].description.contains("survey the repo"));
        assert!(
            units[0].description.contains(instr_a),
            "{}",
            units[0].description
        );
        assert!(
            units[1].description.contains(instr_b),
            "{}",
            units[1].description
        );
        // …and never a sibling's (the whole point is that the prompts stop being interchangeable).
        assert!(
            !units[0].description.contains(instr_b),
            "unit a leaked unit b's instructions: {}",
            units[0].description
        );
        assert!(
            !units[1].description.contains(instr_a),
            "unit b leaked unit a's instructions: {}",
            units[1].description
        );
        // A phase with no instructions keeps the historical prompt byte-exact (no trailing junk).
        assert_eq!(units[2].description, "c — survey the repo");
    }

    /// The degenerate authoring cases: an empty intent still gets the instructions (bare phase id
    /// first), and whitespace-only instructions are treated as absent rather than appending blank
    /// lines to the prompt.
    #[test]
    fn instructions_survive_an_empty_intent_and_blank_instructions_are_ignored() {
        use crate::domain::StageKind;
        use crate::workflow::PhaseDef;
        let def = WorkflowDef {
            base_skill_ref: None,
            id: "instructed".to_string(),
            phases: vec![
                PhaseDef {
                    instructions: Some("do the one thing".to_string()),
                    ..PhaseDef::new("a", StageKind::Recon)
                },
                PhaseDef {
                    instructions: Some("   \n ".to_string()),
                    ..PhaseDef::new("b", StageKind::Recon)
                },
            ],
        };
        let units = plan_from_def(&def, "  ", "s");
        assert_eq!(
            units[0].description,
            format!("a{INSTRUCTION_SEP}do the one thing")
        );
        assert!(
            !units[0].description.contains('\n'),
            "the instruction fold must stay single-line (PTY submits on the first newline): {}",
            units[0].description
        );
        assert_eq!(
            units[1].description, "b",
            "blank instructions must not append"
        );
    }

    /// FINDING-011 (remediation): the instruction fold MUST stay single-line, because the
    /// description is the worker prompt and the PTY session runner submits a turn on the first
    /// newline — a `\n`-joined description would send only `<phase> — <intent>` (the near-identical
    /// prompt the fold exists to kill) and strand the instructions as a stray follow-up that desyncs
    /// the reused session's result sentinel. Asserted against the SHIPPED `survey-repo` def (the one
    /// carrying real multi-sentence instructions), not a fixture, so the guard tracks what ships.
    ///
    /// Falsifier: restore the `\n\n` join in `plan_from_def` — the folded descriptions regain a
    /// newline and the `contains('\n')` assert fires.
    #[test]
    fn folded_instructions_never_introduce_a_newline_into_the_prompt() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("workflows/survey-repo.json");
        let def = crate::workflow::WorkflowRegistry::def_from_file(&path)
            .expect("shipped survey-repo parses");
        // Vacuity guard: the def must actually carry instructions on multiple phases, or a
        // single-line join proves nothing.
        let carrying = def
            .phases
            .iter()
            .filter(|p| {
                p.instructions
                    .as_deref()
                    .map(str::trim)
                    .is_some_and(|s| !s.is_empty())
            })
            .count();
        assert!(
            carrying >= 3,
            "survey-repo must carry instructions on multiple phases or this guard is vacuous; \
             found {carrying}"
        );

        let units = plan_from_def(&def, "what is this repo and how do I work in it", "s");
        for (unit, phase) in units.iter().zip(def.phases.iter()) {
            assert!(
                !unit.description.contains('\n'),
                "phase `{}` planned a multi-line description; the PTY runner submits the turn at \
                 the first newline and strands the rest (FINDING-011): {:?}",
                phase.id,
                unit.description
            );
            // …and the instructions genuinely reached the description — the single-line join must
            // FOLD them in, not drop them (a fix that silently discarded them would also pass the
            // newline assert above).
            if let Some(instr) = phase
                .instructions
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                assert!(
                    unit.description.contains(instr),
                    "phase `{}` instructions did not reach its description: {:?}",
                    phase.id,
                    unit.description
                );
            }
        }
    }

    fn repo_at(id: &str, root: &str) -> crate::repo::RepoEntry {
        crate::repo::RepoEntry {
            id: id.to_string(),
            name: id.to_string(),
            root_path: root.to_string(),
            default_branch: "main".to_string(),
            registered_at: 0,
            // Through the engine's ONE resolver, never a hand-join — a second spelling here is
            // exactly the FINDING-069 drift this fixture exists to test against.
            code_graph_db: crate::code_graph::resolved_code_graph_db(std::path::Path::new(root))
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            findings: Vec::new(),
        }
    }

    /// FINDING-075 (wicked-crew#196): the run's OWN repo reaches its units.
    ///
    /// Two runs planned from the SAME shared def must end up with different argv. That is the whole
    /// property: crew rewrote one shared overlay file per launch instead, so three concurrent
    /// registrations resolved whichever write landed last and two of them indexed a third repo.
    #[test]
    fn two_runs_of_one_def_bind_their_own_repos() {
        let def = crate::workflow::onboarding_def();
        let mut a = plan_from_def(&def, "onboard a", "sa");
        let mut b = plan_from_def(&def, "onboard b", "sb");
        let alpha = repo_at("alpha", "/repos/alpha");
        let beta = repo_at("beta", "/repos/beta");
        bind_repo_paths(&mut a, &alpha);
        bind_repo_paths(&mut b, &beta);

        let index_a = a[0].tool_cmd.as_ref().expect("index is a tool phase");
        let index_b = b[0].tool_cmd.as_ref().expect("index is a tool phase");
        assert!(index_a.contains(&"/repos/alpha".to_string()), "{index_a:?}");
        assert!(index_b.contains(&"/repos/beta".to_string()), "{index_b:?}");
        assert!(
            !index_a.iter().any(|s| s.contains("beta")),
            "run `sa` carries run `sb`'s repo — the cross-repo contamination this guards: {index_a:?}"
        );
        assert!(
            !index_b.iter().any(|s| s.contains("alpha")),
            "run `sb` carries run `sa`'s repo: {index_b:?}"
        );

        // Both phases target the graph the ENGINE resolved — the exact `code_graph_db` the
        // record publishes, never a re-derived spelling (FINDING-069). Asserted against the
        // fixture's resolver-produced value rather than a shape suffix, so this cannot drift
        // into a second spelling of either home.
        for (units, repo) in [(&a, &alpha), (&b, &beta)] {
            for u in units.iter() {
                let cmd = u
                    .tool_cmd
                    .as_ref()
                    .expect("onboarding phases are tool phases");
                let db =
                    cmd[cmd.iter().position(|s| s == "--db").expect("carries --db") + 1].clone();
                assert_eq!(db, repo.code_graph_db, "{db}");
            }
        }
    }

    #[test]
    fn binding_leaves_no_placeholder_behind() {
        let def = crate::workflow::onboarding_def();
        let mut units = plan_from_def(&def, "onboard", "s1");
        assert!(
            !unbound_repo_tokens(&units).is_empty(),
            "the def must DECLARE placeholders, or this guard is vacuous"
        );
        bind_repo_paths(&mut units, &repo_at("alpha", "/repos/alpha"));
        assert_eq!(
            unbound_repo_tokens(&units),
            Vec::<String>::new(),
            "a bound run must carry no `{{...}}` literal into a spawned command"
        );
    }

    /// A phase with no repo placeholders is untouched — binding is not a blanket rewrite.
    #[test]
    fn binding_does_not_touch_commands_that_declare_nothing() {
        let mut units = plan_from_def(&crate::workflow::onboarding_def(), "x", "s1");
        units[0].tool_cmd = Some(vec![
            "wicked-estate".into(),
            "index".into(),
            "/literal".into(),
        ]);
        bind_repo_paths(&mut units, &repo_at("alpha", "/repos/alpha"));
        assert_eq!(
            units[0].tool_cmd.as_deref(),
            Some(
                &[
                    "wicked-estate".to_string(),
                    "index".to_string(),
                    "/literal".to_string()
                ][..]
            )
        );
    }
}
