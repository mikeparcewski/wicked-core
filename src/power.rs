//! POWER MODE (program DES-POWER-MODE; operator rulings 2026-10-29): one long-lived orchestrator
//! seat session builds the whole change in many turns, rigor is sized by an intake QE plan,
//! re-scored at checkpoints from the actual diff, and applied once at hand-off.
//!
//! What lives here, and what it reuses:
//!
//! - **The record** ([`PowerState`] on `AgentSession.power`): the switch, the intake
//!   [`RigorPlan`], the rigor history ([`RigorChange`], one row per re-score, raise, lowering or
//!   toggle), the toggles with their actor, the orchestrator's turns, the hand-off rounds, a
//!   checkpoint pause and the planned-vs-actual [`PowerOutcome`]. camelCase on the wire
//!   (crew-api-types `PowerState`).
//! - **Intake** is the PA's existing scope step (`plan_gate::scope`), asked in power mode to also
//!   run garden's QE risk assessor and test strategist (pass 1) and answer one `RIGOR {…}` line
//!   ([`parse_rigor_line`]). The band is the engine's intent score, never the model's: the QE
//!   plan can only RAISE it (`atLeast`), name phases that must stay phased, and estimate.
//! - **The build** is the plan's `build` unit, dispatched as a multi-turn orchestrator: the
//!   worker loops turns of the SAME seat session (the headless carrier resumes it per run, seat
//!   and posture) until the orchestrator writes `HANDOFF:` ([`orchestrate`]). Each turn ends at a
//!   natural stop (`CHECKPOINT:` after a commit); the engine re-scores the diff
//!   (`Command::PowerCheckpoint`) and the next turn's prompt carries the checkpoint's outcome and
//!   every operator message queued meanwhile ([`take_operator_messages`]). A turn keeps the
//!   engine's per-turn ceiling; the change is no longer one turn, so the ceiling is never raised.
//! - **Rigor** is the operator's word for the floor band (`review_scale` bands). Raising is
//!   automatic (the raise is also held for the next step boundary through the team re-score path,
//!   so the plan grows); lowering happens only at the final pre-hand-off checkpoint, from
//!   evidence, one band, within the hand-off only (fewer tests, never fewer reviewers), never
//!   below a floor ([`lowering`]).
//! - **Hand-off** is the composed plan's evaluator steps (distinct-seat review, security at
//!   highest, QE acceptance when required) and the build's own floor, with no human gate: a
//!   NOT-PASS goes back to the same orchestrator session as a `request_changes` rewind, up to
//!   [`MAX_HANDOFF_ROUNDS`], then the run asks the operator.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

/// The most hand-off rounds that go back to the orchestrator on their own; the next NOT-PASS asks
/// the operator (DES §6 "After 3 rounds, the dock asks the operator").
pub const MAX_HANDOFF_ROUNDS: u32 = 3;

/// The most orchestrator turns one build attempt runs; the last one is told to hand off.
pub const MAX_TURNS_DEFAULT: u32 = 12;

/// Env override of [`MAX_TURNS_DEFAULT`] (rigs only).
pub const MAX_TURNS_ENV: &str = "WICKED_POWER_MAX_TURNS";

/// The timer checkpoint's interval (DES §5.1: every 15-20 min of build time; default 15).
pub const TIMER_SECS_DEFAULT: u64 = 15 * 60;

/// Env override of [`TIMER_SECS_DEFAULT`].
pub const TIMER_SECS_ENV: &str = "WICKED_POWER_TIMER_SECS";

/// How often a turn's watcher looks for a moved HEAD (a commit checkpoint) between timer ticks.
pub const HEAD_POLL_SECS: u64 = 60;

/// The gate kind a "suddenly complex" checkpoint parks on.
pub const PAUSE_GATE_KIND: &str = "power_pause";

/// The actor id the engine answers a hand-off send-back as.
pub const HANDOFF_ACTOR: &str = "engine:power-handoff";

/// The catalog types power mode satisfies without a phased step, unless a rule or the QE plan
/// keeps them (operator ruling 2026-10-29): the intake QE plan is `test_plan`, and the
/// distinct-seat review of the orchestrator's approach at hand-off is `design`.
pub const ABSORBED: [&str; 2] = ["test_plan", "design"];

/// The operator's word for a floor band.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rigor {
    Light,
    Standard,
    High,
    Highest,
}

impl Rigor {
    /// The rigor a score lands in (`review_scale` bands: 0-19, 20-39, 40-69, 70-100); a
    /// destructive change is highest in any band.
    pub fn of(score: u8, destructive: bool) -> Self {
        if destructive || score >= 70 {
            Self::Highest
        } else if score >= 40 {
            Self::High
        } else if score >= 20 {
            Self::Standard
        } else {
            Self::Light
        }
    }

    /// The lowest score in this rigor's band (what a raise to it ratchets the run's score to).
    pub fn min_score(self) -> u8 {
        match self {
            Self::Light => 0,
            Self::Standard => 20,
            Self::High => 40,
            Self::Highest => 70,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Standard => "standard",
            Self::High => "high",
            Self::Highest => "highest",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "light" => Some(Self::Light),
            "standard" => Some(Self::Standard),
            "high" => Some(Self::High),
            "highest" => Some(Self::Highest),
            _ => None,
        }
    }

    /// One band down, if any.
    pub fn down(self) -> Option<Self> {
        match self {
            Self::Light => None,
            Self::Standard => Some(Self::Light),
            Self::High => Some(Self::Standard),
            Self::Highest => Some(Self::High),
        }
    }

    /// The hand-off test set this rigor sizes: targeted below high, the full suite from high.
    pub fn tests(self) -> &'static str {
        if self >= Self::High {
            "full"
        } else {
            "targeted"
        }
    }

    fn rank(self) -> i32 {
        match self {
            Self::Light => 0,
            Self::Standard => 1,
            Self::High => 2,
            Self::Highest => 3,
        }
    }
}

/// A level rigor never goes below, and who set it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RigorFloor {
    pub rigor: Rigor,
    /// `policy` | `compliance` | `qe_plan` | `operator` | `plugin` | `doc_edit`.
    pub by: String,
    pub why: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewRequirement {
    /// `distinct_seat` | `design` | `security`.
    pub kind: String,
    pub why: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QeRequirement {
    pub required: bool,
    pub why: String,
}

/// What hand-off applies, as the current rigor sizes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffRequirements {
    /// `targeted` | `full`.
    pub tests: String,
    pub reviews: Vec<ReviewRequirement>,
    pub qe_acceptance: QeRequirement,
    pub checks: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PhaseReason {
    pub phase: String,
    pub why: String,
}

/// The intake QE plan's rigor record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RigorPlan {
    pub rigor: Rigor,
    pub score: u8,
    pub band: String,
    pub high_risk: bool,
    pub reasons: Vec<String>,
    pub handoff: HandoffRequirements,
    pub mandatory_phases: Vec<PhaseReason>,
    pub phased_required: Option<String>,
    pub estimate_minutes: Option<u32>,
    pub risks: Vec<String>,
    pub scenarios: Vec<String>,
    pub floors: Vec<RigorFloor>,
    pub planned_by: String,
    pub at: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Struggle {
    pub failed_runs: u32,
    pub reverts: u32,
    pub turns: u32,
    pub elapsed_minutes: u32,
    pub estimate_minutes: Option<u32>,
    pub score: u8,
}

/// The signals one checkpoint read from the actual diff.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RigorSignals {
    pub files: u32,
    pub lines: u32,
    pub dependents: Option<u32>,
    pub spread: u32,
    pub new_dependencies: u32,
    pub sensitive: Vec<String>,
    pub struggle: Struggle,
    pub plan_gaps: Vec<String>,
}

/// One rigor-history row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RigorChange {
    pub seq: u32,
    pub at: i64,
    /// `intake` | `commit` | `turn` | `timer` | `pre_handoff` | `operator` | `plugin` |
    /// `doc_edit` | `toggle`.
    pub trigger: String,
    /// `None` only on a toggle row written before intake planned anything.
    pub from: Option<Rigor>,
    pub to: Option<Rigor>,
    /// `up` | `down` | `same`.
    pub direction: String,
    pub planned: u8,
    pub actual: u8,
    pub delta: i32,
    pub reasons: Vec<String>,
    pub signals: Option<RigorSignals>,
    pub by: String,
    pub floors: Vec<RigorFloor>,
    pub checkpoint: Option<u32>,
    pub turn: Option<u32>,
    pub paused: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PowerToggle {
    pub at: i64,
    pub on: bool,
    pub by: String,
    pub reason: Option<String>,
    pub stay_phased: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PowerPause {
    pub at: i64,
    pub checkpoint: u32,
    pub reasons: Vec<String>,
    /// `split` | `switch_phased` | `continue_high` | `stop`.
    pub recommended: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffStep {
    /// `rescore` | `checks` | `review` | `security` | `qe` | `compliance`.
    pub step: String,
    /// The hand-off round (1-based) the row belongs to.
    #[serde(default)]
    pub round: u32,
    pub ord: Option<u32>,
    /// `pending` | `running` | `passed` | `failed` | `skipped`.
    pub status: String,
    pub seat: Option<String>,
    pub summary: Option<String>,
    pub at: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Orchestrator {
    pub cli: Option<String>,
    pub ord: Option<u32>,
    pub turns: u32,
    pub in_turn: bool,
    pub session_restarts: u32,
}

/// The most turn records a run keeps.
pub const TURN_LOG_CAP: usize = 50;

/// One orchestrator turn as recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PowerTurnRecord {
    pub turn: u32,
    pub attempt: u32,
    pub cli: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub outcome: Option<String>,
    pub summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Handoff {
    pub round: u32,
    pub max_rounds: u32,
    pub steps: Vec<HandoffStep>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PowerOutcome {
    pub planned_rigor: Rigor,
    pub final_rigor: Rigor,
    pub planned_score: u8,
    pub final_score: u8,
    pub estimate_minutes: Option<u32>,
    pub actual_minutes: u32,
    pub turns: u32,
    pub checkpoints: u32,
    pub handoff_rounds: u32,
    pub operator_gates: u32,
    /// `delivered` | `completed` | `failed` | `cancelled`.
    pub outcome: String,
}

/// A run's power-mode state (`AgentSession.power`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PowerState {
    pub on: bool,
    pub available: bool,
    pub refused_reason: Option<String>,
    pub rigor: Option<Rigor>,
    pub plan: Option<RigorPlan>,
    #[serde(default)]
    pub history: Vec<RigorChange>,
    #[serde(default)]
    pub toggles: Vec<PowerToggle>,
    #[serde(default)]
    pub orchestrator: Orchestrator,
    /// Every orchestrator turn so far, oldest first (the last [`TURN_LOG_CAP`]).
    #[serde(default)]
    pub turn_log: Vec<PowerTurnRecord>,
    #[serde(default)]
    pub checkpoints: u32,
    pub handoff: Option<Handoff>,
    pub pause: Option<PowerPause>,
    pub outcome: Option<PowerOutcome>,
    /// Floors set after launch (an operator's or a plugin's raise), kept whether or not intake
    /// planned anything; [`PowerState::floor`] reads them with the intake plan's.
    #[serde(default)]
    pub floors: Vec<RigorFloor>,
    /// The rigor when the current turn started: a turn end compares against it, so a mid-turn
    /// checkpoint's raise never hides a two-band jump. Engine-internal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_start_rigor: Option<Rigor>,
    /// Pause triggers the operator already answered (`jump`, `destructive`, `spread`,
    /// `sensitive_dependency`, `struggle`): the cumulative diff never re-raises them. Engine-internal.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acknowledged: Vec<String>,
    /// When the build started (unix millis), for elapsed-against-estimate. Engine-internal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_started_at: Option<i64>,
    /// When the run launched (unix millis). Engine-internal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launched_at: Option<i64>,
    /// Gates the operator answered on this run (insights' `operatorGates`). Engine-internal.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub operator_gates: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

impl PowerState {
    /// A power launch's initial state: on, available, nothing planned yet.
    pub fn launched(on: bool, now: i64) -> Self {
        Self {
            on,
            available: true,
            refused_reason: None,
            rigor: None,
            plan: None,
            history: Vec::new(),
            toggles: vec![PowerToggle {
                at: now,
                on,
                by: "launch".into(),
                reason: None,
                stay_phased: Vec::new(),
            }],
            orchestrator: Orchestrator::default(),
            turn_log: Vec::new(),
            checkpoints: 0,
            handoff: None,
            pause: None,
            outcome: None,
            floors: Vec::new(),
            turn_start_rigor: None,
            acknowledged: Vec::new(),
            build_started_at: None,
            launched_at: Some(now),
            operator_gates: 0,
        }
    }

    /// The highest floor's rigor (light when there is none).
    pub fn floor(&self) -> Rigor {
        self.all_floors()
            .iter()
            .map(|f| f.rigor)
            .max()
            .unwrap_or(Rigor::Light)
    }

    /// Every floor: the intake plan's and those set since.
    pub fn all_floors(&self) -> Vec<RigorFloor> {
        self.plan
            .iter()
            .flat_map(|p| p.floors.iter())
            .chain(self.floors.iter())
            .cloned()
            .collect()
    }

    /// The intake score (0 before intake).
    pub fn planned_score(&self) -> u8 {
        self.plan.as_ref().map_or(0, |p| p.score)
    }

    /// Append a history row, numbering it.
    pub fn push_change(&mut self, mut row: RigorChange) -> RigorChange {
        row.seq = u32::try_from(self.history.len())
            .unwrap_or(u32::MAX)
            .saturating_add(1);
        self.history.push(row.clone());
        row
    }
}

/// Build a history row from `from` to `to`.
#[allow(clippy::too_many_arguments)]
pub fn change(
    trigger: &str,
    from: Rigor,
    to: Rigor,
    planned: u8,
    actual: u8,
    reasons: Vec<String>,
    signals: Option<RigorSignals>,
    by: &str,
    now: i64,
) -> RigorChange {
    let direction = match to.rank() - from.rank() {
        d if d > 0 => "up",
        d if d < 0 => "down",
        _ => "same",
    };
    RigorChange {
        seq: 0,
        at: now,
        trigger: trigger.into(),
        from: Some(from),
        to: Some(to),
        direction: direction.into(),
        planned,
        actual,
        delta: i32::from(actual) - i32::from(planned),
        reasons,
        signals,
        by: by.into(),
        floors: Vec::new(),
        checkpoint: None,
        turn: None,
        paused: false,
    }
}

// ── Intake: the QE plan's RIGOR line ─────────────────────────────────────────────────────────────

/// The line prefix of the intake QE plan's answer.
pub const RIGOR_PREFIX: &str = "RIGOR";

/// What the intake QE plan said (every field optional; the engine scores regardless).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QePlan {
    pub at_least: Option<Rigor>,
    pub why: Option<String>,
    pub risks: Vec<String>,
    pub scenarios: Vec<String>,
    pub checks: Vec<String>,
    pub estimate_minutes: Option<u32>,
    pub mandatory_phases: Vec<PhaseReason>,
    pub phased: Option<String>,
    /// Why the answer could not be read, when it could not (the plan then carries no raise).
    pub unread: Option<String>,
}

/// Parse the `RIGOR {…}` line of the intake answer. Lenient by design (an unknown key is ignored,
/// a bad field is dropped): the line can only ADD to what the engine computed, so a malformed one
/// costs the raise and the estimate, never the run. The LAST well-formed line wins.
pub fn parse_rigor_line(output: &str) -> QePlan {
    let mut out = QePlan {
        unread: Some("the intake answer has no RIGOR line".into()),
        ..QePlan::default()
    };
    for line in output.lines().map(str::trim) {
        let Some(rest) = line.strip_prefix(RIGOR_PREFIX) else {
            continue;
        };
        if !(rest.starts_with(char::is_whitespace) || rest.starts_with('{')) {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(rest.trim()) {
            Ok(v) => v,
            Err(e) => {
                out.unread = Some(format!("a malformed RIGOR line: {e}"));
                continue;
            }
        };
        let strs = |k: &str| -> Vec<String> {
            v.get(k)
                .and_then(|a| a.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s.as_str())
                        .map(|s| clip(s.trim(), 300))
                        .filter(|s| !s.is_empty())
                        .take(20)
                        .collect()
                })
                .unwrap_or_default()
        };
        let text = |k: &str| {
            v.get(k)
                .and_then(|s| s.as_str())
                .map(|s| clip(s.trim(), 400))
                .filter(|s| !s.is_empty())
        };
        let mandatory_phases = v
            .get("mandatoryPhases")
            .and_then(|a| a.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|p| {
                        let phase = p.get("phase")?.as_str()?.trim().to_string();
                        crate::catalog::catalog_entry(&phase)?;
                        Some(PhaseReason {
                            phase,
                            why: p
                                .get("why")
                                .and_then(|w| w.as_str())
                                .map(|w| clip(w.trim(), 300))
                                .unwrap_or_else(|| "the QE plan requires it".into()),
                        })
                    })
                    .take(8)
                    .collect()
            })
            .unwrap_or_default();
        out = QePlan {
            at_least: text("atLeast").and_then(|s| Rigor::parse(&s)),
            why: text("why"),
            risks: strs("risks"),
            scenarios: strs("scenarios"),
            checks: strs("checks"),
            estimate_minutes: v
                .get("estimateMinutes")
                .and_then(|n| n.as_u64())
                .and_then(|n| u32::try_from(n).ok())
                .filter(|n| *n > 0 && *n <= 24 * 60),
            mandatory_phases,
            phased: text("phased"),
            unread: None,
        };
    }
    out
}

fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// The intake addition to the scope step's instructions in power mode.
pub fn intake_instructions() -> &'static str {
    " POWER MODE INTAKE: you are also the run's QE planner. Run the wicked-garden \
     qe-risk-assessor skill (risk matrix) and the wicked-garden qe-test-strategist skill (pass 1, \
     pre-build) over this change, grounded in the estate blast radius you found. Then, after the \
     SCOPE line, end with exactly one more line:\n\
     RIGOR {\"atLeast\":null,\"why\":\"...\",\"risks\":[\"...\"],\"scenarios\":[\"...\"],\
     \"checks\":[\"...\"],\"estimateMinutes\":30,\"mandatoryPhases\":[],\"phased\":null}\n\
     `atLeast` (light|standard|high|highest or null) can only RAISE the engine's own score; \
     `scenarios` are the tests the change needs; `checks` are named checks beyond the repo's own; \
     `estimateMinutes` is your estimate of the build; `mandatoryPhases` lists catalog phases that \
     must stay a separate gated step ({\"phase\":\"design\",\"why\":\"...\"}); `phased` is a \
     one-line reason when this change must not run in power mode at all (else null)."
}

/// The catalog types a power launch drops from its preset or plan: the pre-build phases the
/// orchestrator does inside its own session (`understand`), or whose job power mode does another
/// way (`test_plan` = the intake QE plan, `design` = the hand-off review of the approach). A rule
/// or the QE plan that requires `test_plan` or `design` puts it back through the floor fill.
pub const DROPPED_AT_LAUNCH: [&str; 3] = ["understand", "test_plan", "design"];

/// A power launch's plan: the authored plan without its [`DROPPED_AT_LAUNCH`] steps, with every
/// `depends_on` that named a dropped step re-pointed at what that step depended on. A plan that
/// would keep no step at all is returned unchanged (nothing for an orchestrator to build).
pub fn launch_plan(plan: &crate::plan::PlanSteps) -> crate::plan::PlanSteps {
    let dropped: Vec<&crate::plan::PlanStep> = plan
        .steps
        .iter()
        .filter(|s| DROPPED_AT_LAUNCH.contains(&s.catalog.as_str()))
        .collect();
    if dropped.len() == plan.steps.len() || dropped.is_empty() {
        return plan.clone();
    }
    // A dropped step's own dependencies, resolved through chains of dropped steps.
    fn resolve(id: &str, plan: &crate::plan::PlanSteps, depth: usize) -> Vec<String> {
        let Some(step) = plan.steps.iter().find(|s| s.id == id) else {
            return vec![id.to_string()];
        };
        if depth > 16 || !DROPPED_AT_LAUNCH.contains(&step.catalog.as_str()) {
            return vec![id.to_string()];
        }
        step.depends_on
            .iter()
            .flatten()
            .flat_map(|d| resolve(d, plan, depth + 1))
            .collect()
    }
    let mut out = plan.clone();
    out.steps = plan
        .steps
        .iter()
        .filter(|s| !DROPPED_AT_LAUNCH.contains(&s.catalog.as_str()))
        .cloned()
        .map(|mut s| {
            if let Some(deps) = s.depends_on.take() {
                let mut next: Vec<String> = Vec::new();
                for d in deps.iter().flat_map(|d| resolve(d, plan, 0)) {
                    if !next.contains(&d) {
                        next.push(d);
                    }
                }
                s.depends_on = (!next.is_empty()).then_some(next);
            }
            s
        })
        .collect();
    out
}

/// The catalog types that are hand-off (or the build) in power mode; any other step of the
/// composed plan stays a phased step and is listed as mandatory.
const HANDOFF_TYPES: [&str; 9] = [
    "build",
    "produce",
    "review",
    "critique",
    "security_review",
    "test",
    "deliver",
    "walkthrough_plan",
    "walkthrough_review",
];

/// The hand-off requirements the composed `steps` and `rigor` size.
pub fn handoff_requirements(
    steps: &[crate::plan::PlanStep],
    rigor: Rigor,
    absorbs_design: bool,
    requires_qe: bool,
    checks: &[String],
) -> HandoffRequirements {
    let mut reviews = Vec::new();
    if let Some(r) = steps
        .iter()
        .find(|s| s.catalog == "review" || s.catalog == "critique")
    {
        reviews.push(ReviewRequirement {
            kind: "distinct_seat".into(),
            why: r
                .floor_reason
                .clone()
                .unwrap_or_else(|| "the plan reviews the change on another seat".into()),
        });
        if absorbs_design && rigor >= Rigor::High {
            reviews.push(ReviewRequirement {
                kind: "design".into(),
                why: "the review on another seat also checks the orchestrator's approach; it                       counts as this band's design phase (power mode)"
                    .into(),
            });
        }
    }
    if let Some(r) = steps.iter().find(|s| s.catalog == "security_review") {
        reviews.push(ReviewRequirement {
            kind: "security".into(),
            why: r
                .floor_reason
                .clone()
                .unwrap_or_else(|| "the plan includes a security review".into()),
        });
    }
    let qe_step = steps.iter().any(|s| s.catalog == "test");
    let qe_acceptance = QeRequirement {
        required: requires_qe && qe_step,
        why: if requires_qe && qe_step {
            "the workflow requires QE acceptance; it is waived only when the final diff scores in              the lowest band on every dimension"
                .into()
        } else if qe_step {
            "a test step verifies the change; QE acceptance is not required by the workflow".into()
        } else {
            "the workflow does not require QE acceptance".into()
        },
    };
    HandoffRequirements {
        tests: rigor.tests().into(),
        reviews,
        qe_acceptance,
        checks: checks.to_vec(),
    }
}

/// The intake rigor record, from the decided initial plan and the QE plan.
#[allow(clippy::too_many_arguments)]
pub fn intake_plan(
    qe: &QePlan,
    score: u8,
    destructive: bool,
    band: &str,
    high_risk: bool,
    steps: &[crate::plan::PlanStep],
    obligations: &[crate::plan::HeldObligation],
    absorbs: &[String],
    requires_qe: bool,
    mut reasons: Vec<String>,
    planned_by: &str,
    now: i64,
) -> RigorPlan {
    let rigor = Rigor::of(score, destructive);
    if let (Some(r), Some(why)) = (qe.at_least, qe.why.as_ref()) {
        reasons.push(format!("QE plan: at least {} ({why})", r.as_str()));
    } else if let Some(why) = &qe.why {
        reasons.push(format!("QE plan: {why}"));
    }
    if let Some(why) = &qe.unread {
        reasons.push(format!("QE plan unread: {why}"));
    }
    let mut floors = Vec::new();
    if let Some(r) = qe.at_least {
        floors.push(RigorFloor {
            rigor: r,
            by: "qe_plan".into(),
            why: qe
                .why
                .clone()
                .unwrap_or_else(|| "the intake QE plan raised it".into()),
        });
    }
    if high_risk || destructive {
        floors.push(RigorFloor {
            rigor: Rigor::Highest,
            by: "policy".into(),
            why: if destructive {
                "a destructive change is high risk in any band".into()
            } else {
                "high-risk band".into()
            },
        });
    }
    if !obligations.is_empty() {
        let mut rules: Vec<&str> = obligations.iter().map(|o| o.rule.as_str()).collect();
        rules.dedup();
        floors.push(RigorFloor {
            rigor,
            by: "compliance".into(),
            why: format!("steering rules hold: {}", rules.join(", ")),
        });
    }
    let mandatory_phases: Vec<PhaseReason> = steps
        .iter()
        .filter(|s| {
            !HANDOFF_TYPES.contains(&s.catalog.as_str()) && s.id != crate::plan_gate::SCOPE_STEP_ID
        })
        .map(|s| PhaseReason {
            phase: s.catalog.clone(),
            why: s
                .floor_reason
                .clone()
                .or_else(|| {
                    qe.mandatory_phases
                        .iter()
                        .find(|m| m.phase == s.catalog)
                        .map(|m| m.why.clone())
                })
                .unwrap_or_else(|| "the plan keeps it as its own step".into()),
        })
        .collect();
    let absorbs_design = absorbs.iter().any(|a| a == "design");
    RigorPlan {
        rigor,
        score,
        band: band.to_string(),
        high_risk,
        reasons,
        handoff: handoff_requirements(steps, rigor, absorbs_design, requires_qe, &qe.checks),
        mandatory_phases,
        phased_required: qe.phased.clone(),
        estimate_minutes: qe.estimate_minutes,
        risks: qe.risks.clone(),
        scenarios: qe.scenarios.clone(),
        floors,
        planned_by: planned_by.to_string(),
        at: now,
    }
}

/// The absorbed types a QE plan leaves absorbed (it may keep `design` or `test_plan` phased).
pub fn absorbs_after(qe: &QePlan, current: &[String]) -> Vec<String> {
    if qe.phased.is_some() {
        return Vec::new();
    }
    current
        .iter()
        .filter(|a| !qe.mandatory_phases.iter().any(|m| &m.phase == *a))
        .cloned()
        .collect()
}

// ── The orchestrator's turn protocol ─────────────────────────────────────────────────────────────

/// The line an orchestrator ends a non-final turn with.
pub const CHECKPOINT_PREFIX: &str = "CHECKPOINT:";
/// The line an orchestrator ends its last turn with.
pub const HANDOFF_PREFIX: &str = "HANDOFF:";

/// How a turn ended, from its output (the LAST marker line wins; none = a checkpoint).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnEnd {
    Checkpoint(String),
    Handoff(String),
}

pub fn turn_end(output: &str) -> TurnEnd {
    let mut end = TurnEnd::Checkpoint(String::new());
    for line in output.lines().map(str::trim) {
        let line = line.trim_start_matches(['*', '`', '>', ' ']);
        if let Some(rest) = line.strip_prefix(HANDOFF_PREFIX) {
            end = TurnEnd::Handoff(clip(rest.trim().trim_end_matches(['*', '`']), 300));
        } else if let Some(rest) = line.strip_prefix(CHECKPOINT_PREFIX) {
            end = TurnEnd::Checkpoint(clip(rest.trim().trim_end_matches(['*', '`']), 300));
        }
    }
    end
}

/// The orchestrator brief appended to the build unit's task at dispatch.
pub fn orchestrator_brief(rigor: Option<Rigor>, plan: Option<&RigorPlan>, round: u32) -> String {
    let rigor_line = match (rigor, plan) {
        (Some(r), Some(p)) => format!(
            "Rigor now: {} (score {}). Hand-off will run the {} test set, {}{}.",
            r.as_str(),
            p.score,
            r.tests(),
            p.handoff
                .reviews
                .iter()
                .map(|x| match x.kind.as_str() {
                    "security" => "a security review on another seat",
                    "design" => "a review of your approach",
                    _ => "a review on a different seat",
                })
                .collect::<Vec<_>>()
                .join(", "),
            if p.handoff.qe_acceptance.required {
                " and QE acceptance"
            } else {
                ""
            }
        ),
        (Some(r), None) => format!("Rigor now: {}.", r.as_str()),
        _ => String::new(),
    };
    let rework = if round > 1 {
        format!(
            " This is hand-off round {round}: the review findings above came back to you. Fix \
             them in this same session, commit, and hand off again."
        )
    } else {
        String::new()
    };
    format!(
        "\n\n[power mode] You are the ORCHESTRATOR for this whole change: one session, many turns, \
         no phase gates. Decide your own approach (write it directly, stage it, or fan out your \
         own helper subagents), work in this worktree, and commit as you go — one commit per \
         coherent step. Work in TURNS: after each commit, end your turn with exactly one line \
         `{CHECKPOINT_PREFIX} <what you did; what is next>`. The engine re-scores the change, \
         and your next turn (this same session) carries anything the operator said meanwhile. \
         When the change is complete, committed and its tests pass, end with exactly one line \
         `{HANDOFF_PREFIX} <one-line summary>`. Never push or open a pull request: delivery is \
         the engine's. {rigor_line}{rework}"
    )
}

/// The prompt of a continuation turn (turn > 1): what the checkpoint found and what the operator
/// said. The task is restated in brief so a turn whose session could not be resumed still knows
/// the job.
pub fn continuation_prompt(
    task: &str,
    turn: u32,
    max_turns: u32,
    answer: &CheckpointAnswer,
    last_summary: &str,
    restarted: bool,
) -> String {
    let mut out = format!(
        "[power mode] Turn {turn} of at most {max_turns} of the same change. Continue from where \
         you stopped."
    );
    if restarted {
        out.push_str(
            " Your previous session could not be resumed: re-read the worktree and its git log \
             before you continue.",
        );
    }
    if !last_summary.is_empty() {
        out.push_str(&format!(" Your last checkpoint said: {last_summary}."));
    }
    if let Some(note) = &answer.note {
        out.push_str(&format!(" Checkpoint: {note}"));
    }
    if !answer.messages.is_empty() {
        out.push_str("\n\nThe operator said, while you worked (act on it now):\n");
        for m in &answer.messages {
            out.push_str(&format!("- {}\n", m.trim()));
        }
    }
    if turn >= max_turns {
        out.push_str(&format!(
            "\n\nThis is your LAST turn: finish, commit, and end with `{HANDOFF_PREFIX} …`."
        ));
    } else {
        out.push_str(&format!(
            "\n\nEnd this turn with `{CHECKPOINT_PREFIX} …` after your next commit, or \
             `{HANDOFF_PREFIX} …` when the change is complete."
        ));
    }
    out.push_str(&format!(
        "\n\nThe change, for reference:\n{}",
        clip(task, 4000)
    ));
    out
}

/// What the actor answers a checkpoint with.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckpointAnswer {
    /// The rigor after this checkpoint.
    pub rigor: Option<Rigor>,
    /// One line for the next turn's prompt (a raise, a lowering), when there is one.
    pub note: Option<String>,
    /// Operator messages for the next turn (filled worker-side from the queue).
    pub messages: Vec<String>,
    /// The checkpoint paused the session: end the build and park at the pause gate.
    pub pause: Option<PowerPause>,
    /// Power mode was switched off: end the build after this turn (the remaining work is phased).
    pub stop: bool,
}

// ── Checkpoint evaluation (pure) ─────────────────────────────────────────────────────────────────

/// The struggle score (0-100) and whether it crossed its line: 3 failed test runs, 2 reverts, or
/// twice the estimate (DES §5.4).
pub fn struggle_score(s: &Struggle) -> (u8, Vec<String>) {
    let mut reasons = Vec::new();
    let mut score: u32 = 0;
    score += s.failed_runs.min(5) * 12;
    score += s.reverts.min(3) * 20;
    if let Some(est) = s.estimate_minutes.filter(|e| *e > 0) {
        let ratio_pct = s.elapsed_minutes.saturating_mul(100) / est;
        if ratio_pct > 100 {
            score += ((ratio_pct - 100) / 5).min(40);
        }
        if ratio_pct >= 200 {
            reasons.push(format!(
                "{} min against an estimate of {} min",
                s.elapsed_minutes, est
            ));
        }
    }
    if s.failed_runs >= 3 {
        reasons.push(format!("{} failed test runs", s.failed_runs));
    }
    if s.reverts >= 2 {
        reasons.push(format!("{} reverts", s.reverts));
    }
    (u8::try_from(score.min(100)).unwrap_or(100), reasons)
}

/// "Suddenly complex" (DES §5.4): the reasons to pause, empty when none holds.
pub fn sudden_complexity(
    from: Rigor,
    to: Rigor,
    destructive_new: bool,
    spread_new: bool,
    sensitive_new_dependency: bool,
    struggle_reasons: &[String],
) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    if to.rank() - from.rank() >= 2 {
        out.push((
            "jump",
            format!(
                "rigor jumped two bands at once ({} -> {})",
                from.as_str(),
                to.as_str()
            ),
        ));
    }
    if destructive_new {
        out.push(("destructive", "a destructive change appeared".into()));
    }
    if spread_new {
        out.push(("spread", "the change spread into another product".into()));
    }
    if sensitive_new_dependency {
        out.push((
            "sensitive_dependency",
            "a new dependency in a sensitive area".into(),
        ));
    }
    if !struggle_reasons.is_empty() {
        out.push((
            "struggle",
            format!("struggling: {}", struggle_reasons.join("; ")),
        ));
    }
    out
}

/// The final checkpoint's evidence-based lowering (operator ruling 2026-10-29): one band below
/// `current`, only when the diff's own rigor is lower, there is no struggle and no plan gap, and
/// never below the floors. `Err` names what held it (the capping floors).
pub fn lowering(
    current: Rigor,
    actual: Rigor,
    struggle: u8,
    plan_gaps: &[String],
    floors: &[RigorFloor],
) -> Result<Option<Rigor>, Vec<RigorFloor>> {
    if actual >= current || struggle >= 25 || !plan_gaps.is_empty() {
        return Ok(None);
    }
    let Some(down) = current.down() else {
        return Ok(None);
    };
    let capping: Vec<RigorFloor> = floors.iter().filter(|f| f.rigor > down).cloned().collect();
    if capping.is_empty() {
        Ok(Some(down))
    } else {
        Err(capping)
    }
}

// ── Engine plumbing shared by the actor and the worker ───────────────────────────────────────────

/// One queued operator message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedMessage {
    /// `all` or a cli key.
    pub target: String,
    pub message: String,
}

fn queue() -> &'static Mutex<HashMap<String, Vec<QueuedMessage>>> {
    static Q: OnceLock<Mutex<HashMap<String, Vec<QueuedMessage>>>> = OnceLock::new();
    Q.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Queue an operator message for the run's next turn or unit (the headless carrier has no live
/// process to write into: C5 removed the ACP inject queue, and before this the engine dropped a
/// broadcast with a log line while answering the inject OK).
pub fn queue_operator_message(run_id: &str, target: &str, message: &str) {
    queue()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .entry(run_id.to_string())
        .or_default()
        .push(QueuedMessage {
            target: target.to_string(),
            message: message.to_string(),
        });
}

/// Take every message queued for `run_id` that `cli` should hear (`all`, or its own key).
pub fn take_operator_messages(run_id: &str, cli: &str) -> Vec<QueuedMessage> {
    let mut g = queue().lock().unwrap_or_else(|p| p.into_inner());
    let Some(list) = g.get_mut(run_id) else {
        return Vec::new();
    };
    let (take, keep): (Vec<_>, Vec<_>) = list
        .drain(..)
        .partition(|m| m.target == "all" || m.target == cli);
    *list = keep;
    if list.is_empty() {
        g.remove(run_id);
    }
    take
}

fn full_suites() -> &'static Mutex<std::collections::HashSet<String>> {
    static F: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    F.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

/// The actor says whether `run_id`'s hand-off runs the full suite (its rigor is high or above):
/// the verify floor then runs the repo's full `test` set, not its declared `test_targeted`.
pub fn set_full_suite(run_id: &str, full: bool) {
    let mut g = full_suites().lock().unwrap_or_else(|p| p.into_inner());
    if full {
        g.insert(run_id.to_string());
    } else {
        g.remove(run_id);
    }
}

/// Whether `run_id`'s verify floor must run the full suite (power mode at high rigor).
pub fn full_suite_required(run_id: &str) -> bool {
    full_suites()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .contains(run_id)
}

/// Drop a finished run's queue.
pub fn forget_run(run_id: &str) {
    queue()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(run_id);
    // A worker waiting at a pause of an ended run stops waiting.
    answer_pause(run_id, PauseDecision::Stop);
    set_full_suite(run_id, false);
}

fn actors() -> &'static Mutex<HashMap<uuid::Uuid, std::sync::mpsc::Sender<crate::command::Command>>>
{
    static A: OnceLock<
        Mutex<HashMap<uuid::Uuid, std::sync::mpsc::Sender<crate::command::Command>>>,
    > = OnceLock::new();
    A.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The actor registers its command channel under its process generation, so a worker thread
/// (which holds only its `StepInput`) can reach it for a checkpoint.
pub(crate) fn register_actor(
    gen: uuid::Uuid,
    tx: std::sync::mpsc::Sender<crate::command::Command>,
) {
    actors()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(gen, tx);
}

pub(crate) fn unregister_actor(gen: uuid::Uuid) {
    actors()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&gen);
}

pub(crate) fn actor_tx(
    gen: Option<uuid::Uuid>,
) -> Option<std::sync::mpsc::Sender<crate::command::Command>> {
    let gen = gen?;
    actors()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&gen)
        .cloned()
}

/// A dispatched build unit armed as the orchestrator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildArm {
    /// The unit's task as planned, without the brief (restated in continuation prompts).
    pub task: String,
    pub cli: String,
    pub round: u32,
    /// The run's base commit: every checkpoint measures the WHOLE change from it (a rework round's
    /// own small diff must never read as "the change got easier").
    pub base: Option<String>,
}

/// Armed builds by `(run, ord, attempt)`.
type ArmedBuilds = Mutex<HashMap<(String, u32, u32), BuildArm>>;

fn armed() -> &'static ArmedBuilds {
    static A: OnceLock<ArmedBuilds> = OnceLock::new();
    A.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Arm `(run, ord, attempt)` as a multi-turn orchestrator build (the actor, at dispatch).
pub fn arm_build(run_id: &str, ord: u32, attempt: u32, arm: BuildArm) {
    let mut g = armed().lock().unwrap_or_else(|p| p.into_inner());
    g.retain(|(r, o, _), _| !(r == run_id && *o == ord));
    g.insert((run_id.to_string(), ord, attempt), arm);
}

/// The worker's read: is this attempt an armed orchestrator build? (Taken: one worker runs it.)
pub fn take_armed_build(run_id: &str, ord: u32, attempt: u32) -> Option<BuildArm> {
    armed()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&(run_id.to_string(), ord, attempt))
}

/// The operator's answer to a checkpoint pause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseDecision {
    /// Continue at high rigor (a raise to at least high, an operator floor).
    ContinueHigh,
    /// Continue as is (with the answer's note, if any, as the next turn's steer).
    Continue,
    /// Switch to phased: the build ends at this turn; later hand-off failures ask the operator.
    SwitchPhased,
    /// Stop: the caller cancels the run.
    Stop,
}

impl PauseDecision {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "continue_high" => Some(Self::ContinueHigh),
            "continue" => Some(Self::Continue),
            "switch_phased" => Some(Self::SwitchPhased),
            "stop" => Some(Self::Stop),
            _ => None,
        }
    }
}

fn pauses() -> &'static Mutex<HashMap<String, Option<PauseDecision>>> {
    static P: OnceLock<Mutex<HashMap<String, Option<PauseDecision>>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The actor opened a pause on `run_id`: the worker waits for its answer.
pub fn open_pause(run_id: &str) {
    pauses()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(run_id.to_string(), None);
}

/// Answer an open pause (no-op when none is open).
pub fn answer_pause(run_id: &str, decision: PauseDecision) {
    if let Some(slot) = pauses()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get_mut(run_id)
    {
        *slot = Some(decision);
    }
}

/// The worker's poll: `Some(decision)` once answered (and the pause is closed).
pub fn take_pause_answer(run_id: &str) -> Option<PauseDecision> {
    let mut g = pauses().lock().unwrap_or_else(|p| p.into_inner());
    match g.get(run_id) {
        Some(Some(d)) => {
            let d = *d;
            g.remove(run_id);
            Some(d)
        }
        _ => None,
    }
}

/// What a worker measured at a checkpoint (sent to the actor, which scores it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointReport {
    /// `commit` | `turn` | `timer` | `pre_handoff`.
    pub trigger: String,
    pub turn: u32,
    pub head: Option<String>,
    pub tree: Option<String>,
    pub paths: Vec<String>,
    pub lines: u32,
    pub new_dependencies: u32,
    pub reverts: u32,
    pub failed_runs: u32,
    pub summary: String,
}

// ── The worker's turn loop ───────────────────────────────────────────────────────────────────────

fn git(wd: &std::path::Path, args: &[&str]) -> Option<String> {
    use wicked_apps_core::HardenedCommand;
    let out = std::process::Command::new("git")
        .hardened()
        .arg("-C")
        .arg(wd)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Measure the build's diff since `start` (the HEAD its first turn started from): committed and
/// uncommitted changes, new untracked files, reverts among the build's commits.
pub fn measure(
    wd: &std::path::Path,
    start: Option<&str>,
    trigger: &str,
    turn: u32,
    failed_runs: u32,
    summary: &str,
) -> CheckpointReport {
    let head = git(wd, &["rev-parse", "--short", "HEAD"]);
    let tree = git(wd, &["rev-parse", "HEAD^{tree}"]);
    let mut paths: Vec<String> = Vec::new();
    let mut lines: u32 = 0;
    if let Some(base) = start {
        if let Some(numstat) = git(wd, &["diff", "--numstat", "--no-renames", base]) {
            for l in numstat.lines() {
                let mut it = l.split('\t');
                let add = it.next().and_then(|n| n.parse::<u32>().ok()).unwrap_or(0);
                let del = it.next().and_then(|n| n.parse::<u32>().ok()).unwrap_or(0);
                if let Some(p) = it.next() {
                    lines = lines.saturating_add(add).saturating_add(del);
                    paths.push(p.to_string());
                }
            }
        }
    }
    if let Some(untracked) = git(wd, &["ls-files", "--others", "--exclude-standard"]) {
        for p in untracked.lines().filter(|p| !p.is_empty()) {
            if !paths.iter().any(|x| x == p) {
                paths.push(p.to_string());
            }
        }
    }
    let reverts = start
        .and_then(|b| git(wd, &["log", "--format=%s", &format!("{b}..HEAD")]))
        .map(|log| log.lines().filter(|l| l.starts_with("Revert")).count())
        .unwrap_or(0);
    let manifests = [
        "package.json",
        "Cargo.toml",
        "go.mod",
        "pyproject.toml",
        "requirements.txt",
    ];
    let new_dependencies = paths
        .iter()
        .filter(|p| manifests.iter().any(|m| p.ends_with(m)))
        .count();
    CheckpointReport {
        trigger: trigger.to_string(),
        turn,
        head,
        tree,
        paths,
        lines,
        new_dependencies: u32::try_from(new_dependencies).unwrap_or(u32::MAX),
        reverts: u32::try_from(reverts).unwrap_or(u32::MAX),
        failed_runs,
        summary: summary.to_string(),
    }
}

/// The worktree's per-file +/- against `base` (committed and not), for activity rows.
fn numstat(wd: &std::path::Path, base: &str) -> HashMap<String, (u32, u32)> {
    let mut out = HashMap::new();
    if let Some(n) = git(wd, &["diff", "--numstat", "--no-renames", base]) {
        for l in n.lines() {
            let mut it = l.split('\t');
            let add = it.next().and_then(|n| n.parse::<u32>().ok()).unwrap_or(0);
            let del = it.next().and_then(|n| n.parse::<u32>().ok()).unwrap_or(0);
            if let Some(p) = it.next() {
                out.insert(p.to_string(), (add, del));
            }
        }
    }
    out
}

/// Reads the orchestrator's worktree activity between looks: files whose +/- changed, and new
/// commits. One per build; the watcher and the turn end share it.
pub struct ActivityReader {
    base: Option<String>,
    seen_files: HashMap<String, (u32, u32)>,
    seen_head: Option<String>,
}

impl ActivityReader {
    pub fn new(wd: &std::path::Path, base: Option<String>) -> Self {
        let seen_files = base.as_deref().map(|b| numstat(wd, b)).unwrap_or_default();
        Self {
            seen_head: git(wd, &["rev-parse", "HEAD"]),
            base,
            seen_files,
        }
    }

    /// The activity since the last look, as `PowerActivity` events (at most 20 edits per look).
    pub fn look(
        &mut self,
        wd: &std::path::Path,
        run_id: &str,
        ord: u32,
        turn: u32,
    ) -> Vec<crate::event::CoreEvent> {
        let now = crate::interaction::now_millis();
        let mut out = Vec::new();
        let head = git(wd, &["rev-parse", "HEAD"]);
        if head != self.seen_head {
            let range = match &self.seen_head {
                Some(h) => format!("{h}..HEAD"),
                None => "HEAD".to_string(),
            };
            if let Some(log) = git(
                wd,
                &["log", "--reverse", "--format=%h%x09%s", "-n", "20", &range],
            ) {
                for line in log.lines() {
                    let (sha, subject) = line.split_once('\t').unwrap_or((line, ""));
                    let (added, removed) = git(wd, &["show", "--numstat", "--format=", sha])
                        .map(|n| {
                            n.lines().fold((0u32, 0u32), |(a, d), l| {
                                let mut it = l.split('\t');
                                (
                                    a + it.next().and_then(|x| x.parse::<u32>().ok()).unwrap_or(0),
                                    d + it.next().and_then(|x| x.parse::<u32>().ok()).unwrap_or(0),
                                )
                            })
                        })
                        .unwrap_or((0, 0));
                    out.push(crate::event::CoreEvent::PowerActivity {
                        session: run_id.to_string(),
                        ord,
                        turn,
                        at: now,
                        kind: "commit".into(),
                        path: None,
                        added: Some(added),
                        removed: Some(removed),
                        sha: Some(sha.to_string()),
                        subject: Some(clip(subject, 200)),
                    });
                }
            }
            self.seen_head = head;
        }
        if let Some(base) = self.base.clone() {
            let now_files = numstat(wd, &base);
            let mut changed: Vec<(&String, &(u32, u32))> = now_files
                .iter()
                .filter(|(p, v)| self.seen_files.get(*p) != Some(*v))
                .collect();
            changed.sort();
            for (p, (a, d)) in changed.into_iter().take(20) {
                out.push(crate::event::CoreEvent::PowerActivity {
                    session: run_id.to_string(),
                    ord,
                    turn,
                    at: now,
                    kind: "edit".into(),
                    path: Some(p.clone()),
                    added: Some(*a),
                    removed: Some(*d),
                    sha: None,
                    subject: None,
                });
            }
            self.seen_files = now_files;
        }
        out
    }
}

fn send_checkpoint(
    tx: &std::sync::mpsc::Sender<crate::command::Command>,
    input: &crate::workflow::StepInput,
    report: CheckpointReport,
    wait: bool,
) -> Option<CheckpointAnswer> {
    let (reply, rx) = std::sync::mpsc::channel();
    tx.send(crate::command::Command::PowerCheckpoint {
        run_id: input.run_id.clone(),
        ord: input.unit.ord,
        attempt: input.attempt,
        report,
        reply: wait.then_some(reply),
    })
    .ok()?;
    if !wait {
        return None;
    }
    rx.recv_timeout(std::time::Duration::from_secs(120)).ok()
}

fn turn_event(
    tx: &std::sync::mpsc::Sender<crate::command::Command>,
    event: crate::event::CoreEvent,
    in_turn: bool,
) {
    let _ = tx.send(crate::command::Command::PowerTurn {
        event: Box::new(event),
        in_turn,
    });
}

fn max_turns() -> u32 {
    std::env::var(MAX_TURNS_ENV)
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|n| *n >= 1)
        .unwrap_or(MAX_TURNS_DEFAULT)
}

fn timer_secs() -> u64 {
    std::env::var(TIMER_SECS_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|n| *n >= 60)
        .unwrap_or(TIMER_SECS_DEFAULT)
}

/// Wait at a checkpoint pause for its answer, saying so every minute (the stall watchdog reads the
/// output as liveness). A shut-down engine (its generation unregistered) or a superseded attempt
/// (the actor answers `Stop` on a reassign or an ended run) ends the wait.
fn wait_for_pause(
    run_id: &str,
    gen: Option<uuid::Uuid>,
    emit: &crate::workflow::DeltaSink,
) -> PauseDecision {
    let mut waited = std::time::Duration::ZERO;
    loop {
        if let Some(d) = take_pause_answer(run_id) {
            return d;
        }
        if actor_tx(gen).is_none() {
            return PauseDecision::Stop;
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
        waited += std::time::Duration::from_secs(2);
        if waited.as_secs().is_multiple_of(60) {
            emit("[power mode] still paused, waiting for the operator\n");
        }
    }
}

/// Run a power build: turns of the same seat session until the orchestrator hands off, with a
/// checkpoint at every turn end (and, during a turn, at every new commit and every
/// [`TIMER_SECS_DEFAULT`]). Returns ONE step output for the hand-off: every turn's files, tools
/// and usage, the last turn's text with the earlier turns' checkpoint lines above it, and the
/// last turn's status. With no actor to reach (no generation registered) it is one plain turn.
pub fn orchestrate(
    runner: &std::sync::Arc<dyn crate::workflow::StepRunner>,
    input: &crate::workflow::StepInput,
    emit: &crate::workflow::DeltaSink,
    arm: BuildArm,
) -> crate::workflow::StepOutput {
    use crate::workflow::{StepOutput, StepStatus};
    let Some(tx) = actor_tx(input.process_gen) else {
        return runner.run_unit_streaming(input, emit);
    };
    let wd = input.workdir.clone();
    // Measured from the run's base (falling back to where this build started).
    let start = arm
        .base
        .clone()
        .or_else(|| wd.as_deref().and_then(|w| git(w, &["rev-parse", "HEAD"])));
    let max = max_turns();
    let resumable = crate::container_floor::container_mode();
    let activity = wd
        .as_deref()
        .map(|w| std::sync::Arc::new(Mutex::new(ActivityReader::new(w, start.clone()))));
    let mut turn: u32 = 1;
    let mut turn_input = input.clone();
    let mut summaries: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    let mut tools: Vec<String> = Vec::new();
    let mut usage: Option<crate::workflow::Usage> = None;
    let mut governed = false;
    let cli = arm.cli.clone();
    loop {
        turn_event(
            &tx,
            crate::event::CoreEvent::PowerTurnStarted {
                session: input.run_id.clone(),
                ord: input.unit.ord,
                attempt: input.attempt,
                turn,
                cli: cli.clone(),
            },
            true,
        );
        // During the turn: a new commit is a checkpoint, and so is every timer tick.
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let watcher = wd.clone().map(|w| {
            let stop = stop.clone();
            let tx = tx.clone();
            let probe = turn_input.clone();
            let start = start.clone();
            let activity = activity.clone();
            std::thread::spawn(move || {
                let mut last_head = git(&w, &["rev-parse", "HEAD"]);
                let mut last_tick = std::time::Instant::now();
                let tick = std::time::Duration::from_secs(timer_secs());
                let poll = std::time::Duration::from_secs(HEAD_POLL_SECS);
                let mut slept = std::time::Duration::ZERO;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    slept += std::time::Duration::from_millis(500);
                    if slept < poll {
                        continue;
                    }
                    slept = std::time::Duration::ZERO;
                    if let Some(a) = &activity {
                        let rows = a.lock().unwrap_or_else(|p| p.into_inner()).look(
                            &w,
                            &probe.run_id,
                            probe.unit.ord,
                            turn,
                        );
                        for ev in rows {
                            let _ = tx.send(crate::command::Command::EmitEvent(ev));
                        }
                    }
                    let head = git(&w, &["rev-parse", "HEAD"]);
                    let trigger = if head != last_head {
                        last_head = head;
                        "commit"
                    } else if last_tick.elapsed() >= tick {
                        "timer"
                    } else {
                        continue;
                    };
                    last_tick = std::time::Instant::now();
                    let report = measure(&w, start.as_deref(), trigger, turn, 0, "");
                    let _ = send_checkpoint(&tx, &probe, report, false);
                }
            })
        });
        let out: StepOutput = runner.run_unit_streaming(&turn_input, emit);
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = watcher {
            let _ = h.join();
        }
        // What the turn did since the watcher's last look.
        if let (Some(a), Some(w)) = (&activity, wd.as_deref()) {
            let rows = a.lock().unwrap_or_else(|p| p.into_inner()).look(
                w,
                &input.run_id,
                input.unit.ord,
                turn,
            );
            for ev in rows {
                let _ = tx.send(crate::command::Command::EmitEvent(ev));
            }
        }
        for f in &out.files {
            if !files.contains(f) {
                files.push(f.clone());
            }
        }
        tools.extend(out.tools.iter().cloned());
        governed |= out.governed;
        if let Some(u) = &out.usage {
            let acc = usage.get_or_insert_with(Default::default);
            acc.input_tokens += u.input_tokens;
            acc.output_tokens += u.output_tokens;
            acc.cache_read_tokens += u.cache_read_tokens;
            acc.cache_creation_tokens += u.cache_creation_tokens;
            acc.cost_usd = match (acc.cost_usd, u.cost_usd) {
                (Some(a), Some(b)) => Some(a + b),
                (a, b) => a.or(b),
            };
        }
        let finish = |status: StepStatus, text: &str, summaries: &[String]| StepOutput {
            run_id: input.run_id.clone(),
            unit_ix: input.unit_ix,
            attempt: input.attempt,
            output: if summaries.is_empty() {
                text.to_string()
            } else {
                format!(
                    "[power mode: {} turn(s) of one orchestrator session]\n{}\n\n{text}",
                    summaries.len() + 1,
                    summaries.join("\n")
                )
            },
            status,
            usage: usage.clone(),
            files: files.clone(),
            tools: tools.clone(),
            governed,
        };
        let completed = |outcome: &str, summary: Option<String>| {
            turn_event(
                &tx,
                crate::event::CoreEvent::PowerTurnCompleted {
                    session: input.run_id.clone(),
                    ord: input.unit.ord,
                    attempt: input.attempt,
                    turn,
                    cli: cli.clone(),
                    outcome: outcome.to_string(),
                    summary,
                },
                false,
            )
        };
        if out.status != StepStatus::Ok {
            completed("failed", None);
            return finish(out.status, &out.output, &summaries);
        }
        let end = turn_end(&out.output);
        let (handoff, summary) = match &end {
            TurnEnd::Handoff(s) => (true, s.clone()),
            TurnEnd::Checkpoint(s) => (false, s.clone()),
        };
        let last = turn >= max;
        let trigger = if handoff || last {
            "pre_handoff"
        } else {
            "turn"
        };
        let answer = wd
            .as_deref()
            .map(|w| measure(w, start.as_deref(), trigger, turn, 0, &summary))
            .and_then(|r| send_checkpoint(&tx, &turn_input, r, true))
            .unwrap_or_default();
        let mut answer = answer;
        let final_turn = handoff || last;
        if answer.pause.is_some() {
            completed("paused", Some(summary.clone()));
            emit(
                "\n[power mode] paused at a checkpoint: waiting for the operator (split, switch \
                 to phased, continue at high rigor, or stop)\n",
            );
            let decision = wait_for_pause(&input.run_id, input.process_gen, emit);
            match decision {
                PauseDecision::Stop => {
                    return finish(StepStatus::Cancelled, &out.output, &summaries);
                }
                // A pause at the final checkpoint: once answered, the build hands off.
                _ if final_turn => {
                    return finish(StepStatus::Ok, &out.output, &summaries);
                }
                _ => {}
            }
            match decision {
                PauseDecision::Stop => {
                    return finish(StepStatus::Cancelled, &out.output, &summaries);
                }
                PauseDecision::SwitchPhased => {
                    return finish(StepStatus::Ok, &out.output, &summaries);
                }
                PauseDecision::ContinueHigh => {
                    answer.note = Some(
                        "The operator chose to continue at high rigor: hand-off runs the full \
                         suite."
                            .into(),
                    );
                }
                PauseDecision::Continue => {}
            }
        } else if final_turn {
            completed(if handoff { "handoff" } else { "cap" }, Some(summary));
            return finish(StepStatus::Ok, &out.output, &summaries);
        } else {
            completed("checkpoint", Some(summary.clone()));
        }
        if answer.stop {
            // Switched off: the rest of the run is phased; this build hands off as it stands.
            return finish(StepStatus::Ok, &out.output, &summaries);
        }
        let taken = take_operator_messages(&input.run_id, &cli);
        for m in &taken {
            let _ = tx.send(crate::command::Command::EmitEvent(
                crate::event::CoreEvent::WorkerMessageInjected {
                    session: input.run_id.clone(),
                    message: m.message.clone(),
                    target: m.target.clone(),
                },
            ));
        }
        answer.messages = taken.into_iter().map(|m| m.message).collect();
        summaries.push(format!(
            "turn {turn}: {}",
            if summary.is_empty() {
                "(no checkpoint line)"
            } else {
                summary.as_str()
            }
        ));
        turn += 1;
        turn_input = input.clone();
        turn_input.prior_outputs = Vec::new();
        turn_input.unit.description =
            continuation_prompt(&arm.task, turn, max, &answer, &summary, !resumable);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_power_launch_drops_the_pre_build_phases_and_repoints_their_dependents() {
        let bug = crate::catalog::builtin_presets()
            .into_iter()
            .find(|(n, _)| *n == "bug")
            .unwrap()
            .1;
        let plan = crate::plan::PlanSteps {
            steps: bug,
            ..Default::default()
        };
        let p = launch_plan(&plan);
        let ids: Vec<&str> = p.steps.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["fix", "verify"]);
        assert_eq!(
            p.steps[0].depends_on, None,
            "fix depended on reproduce -> triage -> none"
        );
        assert_eq!(p.steps[1].depends_on, Some(vec!["fix".to_string()]));
        // Every built-in preset with a build step still composes after the drop.
        for (name, steps) in crate::catalog::builtin_presets() {
            let p = launch_plan(&crate::plan::PlanSteps {
                steps,
                ..Default::default()
            });
            let p = crate::plan_gate::with_default_ids(&p);
            assert!(
                crate::plan::compose(crate::catalog::catalog(), &p).is_ok(),
                "preset {name} does not compose in power mode"
            );
        }
    }

    #[test]
    fn power_absorbs_test_plan_and_design_from_the_band_floor_only() {
        let plan = crate::plan::PlanSteps {
            steps: vec![crate::plan::PlanStep {
                catalog: "build".into(),
                id: "build".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let absorbs: Vec<String> = ABSORBED.iter().map(|s| s.to_string()).collect();
        let fill = |absorbs: &[String], obligations: &[crate::plan::HeldObligation]| {
            crate::plan::floor_fill(
                crate::catalog::catalog(),
                &plan,
                crate::plan::FloorInput {
                    score: 50,
                    destructive: false,
                    human_confirm: &crate::domain::HumanConfirm::None,
                    deliver: None,
                    obligations,
                    ran: &[],
                    requires_evaluator: true,
                    requires_qe: false,
                    power_absorbs: absorbs,
                },
            )
            .unwrap()
            .steps
            .steps
            .iter()
            .map(|s| s.catalog.clone())
            .collect::<Vec<_>>()
        };
        let phased = fill(&[], &[]);
        assert!(phased.contains(&"design".to_string()));
        assert!(phased.contains(&"test_plan".to_string()));
        let power = fill(&absorbs, &[]);
        assert_eq!(
            power,
            ["build", "review"],
            "high band in power mode: build + review"
        );
        // A QE plan that keeps design phased leaves it in the floor.
        let qe = QePlan {
            mandatory_phases: vec![PhaseReason {
                phase: "design".into(),
                why: "new API".into(),
            }],
            ..Default::default()
        };
        let kept = absorbs_after(&qe, &absorbs);
        assert_eq!(kept, ["test_plan".to_string()]);
        assert!(fill(&kept, &[]).contains(&"design".to_string()));
        // A QE plan that requires phased absorbs nothing.
        let phased_qe = QePlan {
            phased: Some("regulated".into()),
            ..Default::default()
        };
        assert!(absorbs_after(&phased_qe, &absorbs).is_empty());
    }

    #[test]
    fn the_intake_plan_names_reviews_qe_floors_and_mandatory_phases() {
        let step = |c: &str, reason: Option<&str>| crate::plan::PlanStep {
            catalog: c.into(),
            id: c.into(),
            floor_reason: reason.map(str::to_string),
            ..Default::default()
        };
        let steps = vec![
            step("understand", None),
            step("architecture", Some("band 70-100 requires architecture")),
            step("build", None),
            step("review", Some("band 70-100 requires review")),
            step(
                "security_review",
                Some("band 70-100 requires security_review"),
            ),
            step("test", None),
            step("deliver", None),
        ];
        let mut steps = steps;
        steps[0].id = crate::plan_gate::SCOPE_STEP_ID.into();
        let qe = QePlan {
            at_least: Some(Rigor::High),
            why: Some("sign-in".into()),
            estimate_minutes: Some(40),
            ..Default::default()
        };
        let p = intake_plan(
            &qe,
            72,
            false,
            "70-100",
            true,
            &steps,
            &[crate::plan::HeldObligation {
                rule: "POL-1".into(),
                token: "review".into(),
            }],
            &ABSORBED.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            true,
            vec!["reach 40".into()],
            "claude",
            9,
        );
        assert_eq!(p.rigor, Rigor::Highest);
        assert_eq!(p.handoff.tests, "full");
        let kinds: Vec<&str> = p.handoff.reviews.iter().map(|r| r.kind.as_str()).collect();
        assert_eq!(kinds, ["distinct_seat", "design", "security"]);
        assert!(p.handoff.qe_acceptance.required);
        assert_eq!(p.mandatory_phases.len(), 1);
        assert_eq!(p.mandatory_phases[0].phase, "architecture");
        let by: Vec<&str> = p.floors.iter().map(|f| f.by.as_str()).collect();
        assert_eq!(by, ["qe_plan", "policy", "compliance"]);
        assert_eq!(p.estimate_minutes, Some(40));
        assert!(p.reasons.iter().any(|r| r.contains("at least high")));
    }

    #[test]
    fn a_checkpoint_measures_the_whole_change_from_the_base_with_reverts_and_new_files() {
        let dir = std::env::temp_dir().join(format!(
            "wicked-power-measure-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        struct Gone(std::path::PathBuf);
        impl Drop for Gone {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _gone = Gone(dir.clone());
        let wd = dir.as_path();
        let run = |args: &[&str]| {
            // spawn-audit: test-only
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(wd)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        run(&["init", "-q"]);
        run(&["config", "core.fsmonitor", "false"]);
        std::fs::write(wd.join("a.rs"), "fn a() {}\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "base"]);
        let base = git(wd, &["rev-parse", "HEAD"]).unwrap();
        let mut reader = ActivityReader::new(wd, Some(base.clone()));
        assert!(
            reader.look(wd, "r", 2, 1).is_empty(),
            "nothing happened yet"
        );
        std::fs::write(wd.join("a.rs"), "fn a() { b() }\nfn b() {}\n").unwrap();
        run(&["commit", "-qam", "change a"]);
        let rows: Vec<serde_json::Value> = reader
            .look(wd, "r", 2, 1)
            .iter()
            .map(|e| e.to_json())
            .collect();
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[0]["kind"], "commit");
        assert_eq!(rows[0]["subject"], "change a");
        assert_eq!(rows[1]["kind"], "edit");
        assert_eq!(rows[1]["path"], "a.rs");
        assert!(
            reader.look(wd, "r", 2, 1).is_empty(),
            "each change is said once"
        );
        run(&["revert", "--no-edit", "HEAD"]);
        std::fs::write(wd.join("a.rs"), "fn a() { c() }\n").unwrap();
        std::fs::write(wd.join("Cargo.toml"), "[package]\n").unwrap();
        let r = measure(wd, Some(&base), "turn", 2, 0, "s");
        assert_eq!(r.trigger, "turn");
        assert_eq!(r.turn, 2);
        assert!(r.paths.contains(&"a.rs".to_string()), "{:?}", r.paths);
        assert!(
            r.paths.contains(&"Cargo.toml".to_string()),
            "untracked files count"
        );
        assert_eq!(r.reverts, 1);
        assert_eq!(r.new_dependencies, 1);
        assert!(r.lines >= 2);
        assert!(r.head.is_some() && r.tree.is_some());
    }

    #[test]
    fn rigor_bands_match_the_review_scale_floor_bands() {
        assert_eq!(Rigor::of(0, false), Rigor::Light);
        assert_eq!(Rigor::of(19, false), Rigor::Light);
        assert_eq!(Rigor::of(20, false), Rigor::Standard);
        assert_eq!(Rigor::of(39, false), Rigor::Standard);
        assert_eq!(Rigor::of(40, false), Rigor::High);
        assert_eq!(Rigor::of(69, false), Rigor::High);
        assert_eq!(Rigor::of(70, false), Rigor::Highest);
        assert_eq!(Rigor::of(5, true), Rigor::Highest);
        for r in [Rigor::Light, Rigor::Standard, Rigor::High, Rigor::Highest] {
            assert_eq!(Rigor::of(r.min_score(), false), r);
            // The floor band of a rigor's minimum score is the band the engine computes.
            let band = crate::review_scale::floor_for(r.min_score(), false).band;
            assert!(band.starts_with(&r.min_score().to_string()), "{band}");
        }
        assert_eq!(
            serde_json::to_value(Rigor::Highest).unwrap(),
            serde_json::json!("highest")
        );
    }

    #[test]
    fn the_rigor_line_parses_leniently_and_only_adds() {
        let out = "found it\nSCOPE {\"touch\":[\"a.rs\"]}\nRIGOR {\"atLeast\":\"high\",\"why\":\"sign-in\",\
                   \"risks\":[\"auth\"],\"scenarios\":[\"EUR\",\"\"],\"checks\":[\"pdf-diff\"],\
                   \"estimateMinutes\":40,\"mandatoryPhases\":[{\"phase\":\"architecture\",\"why\":\"x\"},\
                   {\"phase\":\"nope\"}],\"phased\":null,\"extra\":1}";
        let p = parse_rigor_line(out);
        assert_eq!(p.at_least, Some(Rigor::High));
        assert_eq!(p.scenarios, vec!["EUR".to_string()]);
        assert_eq!(p.estimate_minutes, Some(40));
        assert_eq!(
            p.mandatory_phases.len(),
            1,
            "an unknown catalog phase is dropped"
        );
        assert_eq!(p.phased, None);
        assert!(p.unread.is_none());
        let none = parse_rigor_line("SCOPE {\"touch\":[]}");
        assert!(none.unread.is_some());
        assert_eq!(none.at_least, None);
        let bad = parse_rigor_line("RIGOR {not json");
        assert!(bad.unread.unwrap().contains("malformed"));
        // RIGOROUS is not the prefix.
        assert!(parse_rigor_line("RIGOROUS {}").unread.is_some());
    }

    #[test]
    fn a_turn_ends_on_its_last_marker_and_defaults_to_a_checkpoint() {
        assert_eq!(
            turn_end("work\nCHECKPOINT: commit 1\nmore\n**HANDOFF: done**"),
            TurnEnd::Handoff("done".into())
        );
        assert_eq!(
            turn_end("HANDOFF: early\nCHECKPOINT: then more"),
            TurnEnd::Checkpoint("then more".into())
        );
        assert_eq!(turn_end("no marker"), TurnEnd::Checkpoint(String::new()));
    }

    #[test]
    fn struggle_crosses_its_line_on_failures_reverts_or_twice_the_estimate() {
        let s = |f, r, e, est| Struggle {
            failed_runs: f,
            reverts: r,
            turns: 1,
            elapsed_minutes: e,
            estimate_minutes: est,
            score: 0,
        };
        assert!(struggle_score(&s(0, 0, 10, Some(30))).1.is_empty());
        assert_eq!(struggle_score(&s(3, 0, 10, None)).1.len(), 1);
        assert_eq!(struggle_score(&s(0, 2, 10, None)).1.len(), 1);
        assert_eq!(struggle_score(&s(0, 0, 61, Some(30))).1.len(), 1);
        assert!(struggle_score(&s(5, 3, 200, Some(30))).0 <= 100);
    }

    #[test]
    fn lowering_is_one_band_from_evidence_and_never_below_a_floor() {
        let floor = |r| RigorFloor {
            rigor: r,
            by: "policy".into(),
            why: "x".into(),
        };
        assert_eq!(
            lowering(Rigor::High, Rigor::Light, 0, &[], &[]),
            Ok(Some(Rigor::Standard)),
            "one band at most"
        );
        assert_eq!(lowering(Rigor::High, Rigor::High, 0, &[], &[]), Ok(None));
        assert_eq!(lowering(Rigor::High, Rigor::Light, 30, &[], &[]), Ok(None));
        assert_eq!(
            lowering(Rigor::High, Rigor::Light, 0, &["gap".into()], &[]),
            Ok(None)
        );
        let capped = lowering(Rigor::High, Rigor::Light, 0, &[], &[floor(Rigor::High)]);
        assert_eq!(capped.unwrap_err().len(), 1);
        assert_eq!(
            lowering(Rigor::High, Rigor::Light, 0, &[], &[floor(Rigor::Standard)]),
            Ok(Some(Rigor::Standard))
        );
    }

    #[test]
    fn a_two_band_jump_or_struggle_is_sudden_complexity() {
        assert!(
            sudden_complexity(Rigor::Light, Rigor::Standard, false, false, false, &[]).is_empty()
        );
        assert_eq!(
            sudden_complexity(Rigor::Light, Rigor::High, false, false, false, &[]).len(),
            1
        );
        assert_eq!(
            sudden_complexity(
                Rigor::High,
                Rigor::High,
                true,
                true,
                true,
                &["3 failed test runs".into()]
            )
            .len(),
            4,
            "jump excluded; destructive, spread, dependency, struggle"
        );
    }

    #[test]
    fn the_operator_queue_delivers_by_target_once() {
        let run = "power-queue-test-run";
        queue_operator_message(run, "all", "a");
        queue_operator_message(run, "codex", "b");
        queue_operator_message(run, "claude", "c");
        let got: Vec<String> = take_operator_messages(run, "claude")
            .into_iter()
            .map(|m| m.message)
            .collect();
        assert_eq!(got, vec!["a".to_string(), "c".to_string()]);
        assert!(take_operator_messages(run, "claude").is_empty());
        assert_eq!(take_operator_messages(run, "codex").len(), 1);
        forget_run(run);
    }

    #[test]
    fn the_state_serializes_camel_case_with_nulls() {
        let s = PowerState::launched(true, 5);
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["on"], true);
        assert!(v["refusedReason"].is_null());
        assert!(v["plan"].is_null());
        assert_eq!(v["orchestrator"]["inTurn"], false);
        assert_eq!(v["toggles"][0]["by"], "launch");
        assert!(v.get("operatorGates").is_none());
        let back: PowerState = serde_json::from_value(v).unwrap();
        assert_eq!(back, s);
    }
}
