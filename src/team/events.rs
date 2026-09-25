//! The team wire contract (DES-TEAMING-002 T1): every `wicked.team.*` event type, its payload,
//! its idempotency key, and [`fold`], the pure function that turns one attempt's rows into its
//! [`TeamLedger`].
//!
//! **Types only.** Nothing here publishes: `TeamBus::publish` and the `TeamPublisher` are P1's.
//! [`TeamEvent::bus_emit`] builds the row a publisher would write, so tests (and later P1) use the
//! one key rule.
//!
//! **Payload shape (DES-002 §6).** Every payload carries the envelope ([`Envelope`]: `run_id`,
//! `ord`, `attempt`, `by`, `at`, `re`) plus its body's fields, all at the top level, keys in
//! snake_case. Every field is always present: an absent value is `null`, never a missing key.
//! The one exception is a plan step ([`PlanStep`]), whose optional step fields are omitted when
//! unset, exactly as a plan names them. The embedded [`TeamLedger`] keeps DES-001 §7's camelCase.
//! wicked-core's `serde_json` has no `preserve_order`, so fixtures compare parsed values, never
//! strings.
//!
//! **Keys (DES-002 §4.1, §6.1).** Every key is
//! `deterministic_key(["team", <event type>, <run id>, <parts…>])`: SHA-256 over each part's
//! UTF-8 bytes followed by one `0x00`, the first 16 bytes as lowercase hex. The parts are
//! producer-assigned sequences or ids, never content a later distinct request can repeat: the
//! key builders below take only ids and counters, and a source test fails the build if one takes
//! a payload text field.
//!
//! **Owners (DES-002 §7).** Each type has exactly one publisher ([`owner`]): the engine (E), the
//! supervisor (S) or the attempt runner (R).

use std::collections::{BTreeMap, HashSet};

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    Dispute, FinalPass, Finding, FindingStatus, LedgerDelivery, LedgerFinding, LedgerMonitor,
    MonitorReply, MonitorStatus, NoVerdictReason, ReplyKind, Severity, TeamLedger, Verdict,
};
use crate::bus::{deterministic_key, BusEmit, CORE_DOMAIN};

/// The bus `subdomain` every team event is published under (DES-002 §4.1).
pub const TEAM_SUBDOMAIN: &str = "core.team";

// ── Event types (DES-002 §6): `wicked.team.<noun>.<verb>`, four segments ─────────────────────────

pub const PATH_STARTED: &str = "wicked.team.path.started";
pub const PATH_SCORED: &str = "wicked.team.path.scored";
pub const PLAN_PROPOSED: &str = "wicked.team.plan.proposed";
pub const PLAN_REVISED: &str = "wicked.team.plan.revised";
pub const PLAN_ACCEPTED: &str = "wicked.team.plan.accepted";
pub const PLAN_REFUSED: &str = "wicked.team.plan.refused";
pub const MEMBER_JOINED: &str = "wicked.team.member.joined";
pub const MEMBER_LEFT: &str = "wicked.team.member.left";
pub const STEP_CLAIMED: &str = "wicked.team.step.claimed";
pub const CHECKPOINT_REACHED: &str = "wicked.team.checkpoint.reached";
pub const FINDING_RAISED: &str = "wicked.team.finding.raised";
pub const ADVICE_DELIVERED: &str = "wicked.team.advice.delivered";
pub const ADVICE_ANSWERED: &str = "wicked.team.advice.answered";
pub const HELP_REQUESTED: &str = "wicked.team.help.requested";
pub const HELP_ANSWERED: &str = "wicked.team.help.answered";
pub const CHANGE_REQUESTED: &str = "wicked.team.change.requested";
pub const STEP_COMPLETED: &str = "wicked.team.step.completed";
pub const STEP_REVIEWED: &str = "wicked.team.step.reviewed";
pub const FINDING_SETTLED: &str = "wicked.team.finding.settled";
pub const COUNCIL_CALLED: &str = "wicked.team.council.called";
pub const COUNCIL_RULED: &str = "wicked.team.council.ruled";
pub const LEDGER_FOLDED: &str = "wicked.team.ledger.folded";
pub const GATE_OPENED: &str = "wicked.team.gate.opened";
pub const GATE_DECIDED: &str = "wicked.team.gate.decided";
pub const PATH_ENDED: &str = "wicked.team.path.ended";

/// The 25 types, in DES-002 §6 table order.
pub const ALL_TYPES: [&str; 25] = [
    PATH_STARTED,
    PATH_SCORED,
    PLAN_PROPOSED,
    PLAN_REVISED,
    PLAN_ACCEPTED,
    PLAN_REFUSED,
    MEMBER_JOINED,
    MEMBER_LEFT,
    STEP_CLAIMED,
    CHECKPOINT_REACHED,
    FINDING_RAISED,
    ADVICE_DELIVERED,
    ADVICE_ANSWERED,
    HELP_REQUESTED,
    HELP_ANSWERED,
    CHANGE_REQUESTED,
    STEP_COMPLETED,
    STEP_REVIEWED,
    FINDING_SETTLED,
    COUNCIL_CALLED,
    COUNCIL_RULED,
    LEDGER_FOLDED,
    GATE_OPENED,
    GATE_DECIDED,
    PATH_ENDED,
];

/// The one component that publishes a type (DES-002 §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Owner {
    /// The actor, through the `TeamPublisher`.
    Engine,
    /// The team supervisor, on its own thread and connection.
    Supervisor,
    /// The attempt runner: the attempt's worker thread plus its ACP carrier.
    Runner,
}

/// The sole publisher of `event_type` (DES-002 §7), `None` for a type that is not a team event.
pub fn owner(event_type: &str) -> Option<Owner> {
    use Owner::{Engine as E, Runner as R, Supervisor as S};
    Some(match event_type {
        PATH_STARTED | PATH_SCORED | PLAN_PROPOSED | PLAN_REVISED | PLAN_ACCEPTED
        | PLAN_REFUSED | GATE_OPENED | GATE_DECIDED | PATH_ENDED => E,
        MEMBER_JOINED | MEMBER_LEFT | FINDING_RAISED | HELP_ANSWERED | CHANGE_REQUESTED
        | FINDING_SETTLED | COUNCIL_CALLED | COUNCIL_RULED | LEDGER_FOLDED => S,
        STEP_CLAIMED | CHECKPOINT_REACHED | ADVICE_DELIVERED | ADVICE_ANSWERED | HELP_REQUESTED
        | STEP_COMPLETED | STEP_REVIEWED => R,
        _ => return None,
    })
}

// ── Keys (DES-002 §4.1, §6.1) ────────────────────────────────────────────────────────────────────

/// `deterministic_key(["team", event_type, run_id, parts…])`.
fn team_key(event_type: &str, run_id: &str, parts: &[&str]) -> String {
    let mut all: Vec<&str> = vec!["team", event_type, run_id];
    all.extend_from_slice(parts);
    deterministic_key(&all)
}

pub fn key_path_started(run_id: &str) -> String {
    team_key(PATH_STARTED, run_id, &[])
}

pub fn key_path_scored(run_id: &str, score_source: &str) -> String {
    team_key(PATH_SCORED, run_id, &[score_source])
}

pub fn key_plan_proposed(run_id: &str, proposal_id: &str) -> String {
    team_key(PLAN_PROPOSED, run_id, &[proposal_id])
}

pub fn key_plan_revised(run_id: &str, plan_rev: u32) -> String {
    team_key(PLAN_REVISED, run_id, &[&plan_rev.to_string()])
}

pub fn key_plan_accepted(run_id: &str, plan_rev: u32) -> String {
    team_key(PLAN_ACCEPTED, run_id, &[&plan_rev.to_string()])
}

pub fn key_plan_refused(run_id: &str, proposal_id: &str) -> String {
    team_key(PLAN_REFUSED, run_id, &[proposal_id])
}

pub fn key_member_joined(
    run_id: &str,
    ord: u32,
    attempt: u32,
    member_id: &str,
    open_seq: u32,
) -> String {
    let (o, a, s) = (ord.to_string(), attempt.to_string(), open_seq.to_string());
    team_key(MEMBER_JOINED, run_id, &[&o, &a, member_id, &s])
}

pub fn key_member_left(
    run_id: &str,
    ord: u32,
    attempt: u32,
    member_id: &str,
    open_seq: u32,
) -> String {
    let (o, a, s) = (ord.to_string(), attempt.to_string(), open_seq.to_string());
    team_key(MEMBER_LEFT, run_id, &[&o, &a, member_id, &s])
}

pub fn key_step_claimed(run_id: &str, step_id: &str, attempt: u32, by: &str) -> String {
    team_key(STEP_CLAIMED, run_id, &[step_id, &attempt.to_string(), by])
}

pub fn key_checkpoint_reached(run_id: &str, ord: u32, attempt: u32, seq: u64) -> String {
    let (o, a, s) = (ord.to_string(), attempt.to_string(), seq.to_string());
    team_key(CHECKPOINT_REACHED, run_id, &[&o, &a, &s])
}

pub fn key_finding_raised(run_id: &str, ord: u32, attempt: u32, raise_seq: u32) -> String {
    let (o, a, s) = (ord.to_string(), attempt.to_string(), raise_seq.to_string());
    team_key(FINDING_RAISED, run_id, &[&o, &a, &s])
}

pub fn key_advice_delivered(
    run_id: &str,
    ord: u32,
    attempt: u32,
    raise_seq: u32,
    delivery_id: &str,
) -> String {
    let (o, a, s) = (ord.to_string(), attempt.to_string(), raise_seq.to_string());
    team_key(ADVICE_DELIVERED, run_id, &[&o, &a, &s, delivery_id])
}

pub fn key_advice_answered(
    run_id: &str,
    ord: u32,
    attempt: u32,
    raise_seq: u32,
    answered_in: &str,
) -> String {
    let (o, a, s) = (ord.to_string(), attempt.to_string(), raise_seq.to_string());
    team_key(ADVICE_ANSWERED, run_id, &[&o, &a, &s, answered_in])
}

pub fn key_help_requested(run_id: &str, help_id: &str) -> String {
    team_key(HELP_REQUESTED, run_id, &[help_id])
}

pub fn key_help_answered(run_id: &str, help_id: &str, answer_id: &str) -> String {
    team_key(HELP_ANSWERED, run_id, &[help_id, answer_id])
}

pub fn key_change_requested(run_id: &str, change_id: &str) -> String {
    team_key(CHANGE_REQUESTED, run_id, &[change_id])
}

pub fn key_step_completed(run_id: &str, step_id: &str, attempt: u32, by: &str) -> String {
    team_key(STEP_COMPLETED, run_id, &[step_id, &attempt.to_string(), by])
}

pub fn key_step_reviewed(run_id: &str, step_id: &str, attempt: u32) -> String {
    team_key(STEP_REVIEWED, run_id, &[step_id, &attempt.to_string()])
}

pub fn key_finding_settled(run_id: &str, ord: u32, attempt: u32, raise_seq: u32) -> String {
    let (o, a, s) = (ord.to_string(), attempt.to_string(), raise_seq.to_string());
    team_key(FINDING_SETTLED, run_id, &[&o, &a, &s])
}

pub fn key_council_called(run_id: &str, ord: u32, attempt: u32, subject: &str) -> String {
    let (o, a) = (ord.to_string(), attempt.to_string());
    team_key(COUNCIL_CALLED, run_id, &[&o, &a, subject])
}

pub fn key_council_ruled(run_id: &str, ord: u32, attempt: u32, subject: &str) -> String {
    let (o, a) = (ord.to_string(), attempt.to_string());
    team_key(COUNCIL_RULED, run_id, &[&o, &a, subject])
}

pub fn key_ledger_folded(run_id: &str, ord: u32, attempt: u32) -> String {
    let (o, a) = (ord.to_string(), attempt.to_string());
    team_key(LEDGER_FOLDED, run_id, &[&o, &a])
}

pub fn key_gate_opened(run_id: &str, gate_id: &str) -> String {
    team_key(GATE_OPENED, run_id, &[gate_id])
}

pub fn key_gate_decided(run_id: &str, gate_id: &str) -> String {
    team_key(GATE_DECIDED, run_id, &[gate_id])
}

pub fn key_path_ended(run_id: &str) -> String {
    team_key(PATH_ENDED, run_id, &[])
}

// ── Producer-assigned ids that feed the keys (DES-002 §6.1) ──────────────────────────────────────

/// Where a plan proposal came from. Every source is an id the engine received with the command
/// that carried the plan (DES-002 §6.1 row 3), never the plan's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProposalSource {
    /// A user plan or a preset named at launch: the launch's session id.
    Launch { session_id: String },
    /// The PA's plan from its `understand` step: that step's `ord:attempt`.
    Understand { ord: u32, attempt: u32 },
    /// A `PLAN+` block, numbered by the step-output parser: `ord:attempt:plan_block_seq`.
    PlanBlock {
        ord: u32,
        attempt: u32,
        plan_block_seq: u32,
    },
    /// An accepted member request: its `change_id`.
    Change { change_id: String },
    /// An edit at the approval gate: the `gate_id`.
    Gate { gate_id: String },
    /// A mid-run human edit through `Core::propose_plan`: crew's per-POST request id.
    Edit { request_id: String },
}

impl ProposalSource {
    /// The source id exactly as DES-002 §6.1 spells it.
    pub fn source_id(&self) -> String {
        match self {
            ProposalSource::Launch { session_id } => session_id.clone(),
            ProposalSource::Understand { ord, attempt } => format!("{ord}:{attempt}"),
            ProposalSource::PlanBlock {
                ord,
                attempt,
                plan_block_seq,
            } => format!("{ord}:{attempt}:{plan_block_seq}"),
            ProposalSource::Change { change_id } => change_id.clone(),
            ProposalSource::Gate { gate_id } => gate_id.clone(),
            ProposalSource::Edit { request_id } => request_id.clone(),
        }
    }
}

/// `"p-" + deterministic_key([run, by, source])`.
pub fn mint_proposal_id(run_id: &str, by: &str, source: &ProposalSource) -> String {
    format!(
        "p-{}",
        deterministic_key(&[run_id, by, &source.source_id()])
    )
}

/// `"h-" + deterministic_key([run, ord, attempt, by, help_seq])`: R's per-attempt counter over
/// `HELP:` lines, never the question.
pub fn mint_help_id(run_id: &str, ord: u32, attempt: u32, by: &str, help_seq: u32) -> String {
    let (o, a, s) = (ord.to_string(), attempt.to_string(), help_seq.to_string());
    format!("h-{}", deterministic_key(&[run_id, &o, &a, by, &s]))
}

/// `"c-" + deterministic_key([run, ord, attempt, member_id, change_seq])`: S's per-attempt
/// counter, never the request's text.
pub fn mint_change_id(
    run_id: &str,
    ord: u32,
    attempt: u32,
    member_id: &str,
    change_seq: u32,
) -> String {
    let (o, a, s) = (ord.to_string(), attempt.to_string(), change_seq.to_string());
    format!("c-{}", deterministic_key(&[run_id, &o, &a, member_id, &s]))
}

/// `path.scored`'s key part for the intent score of a proposal.
pub fn score_source_intent(proposal_id: &str) -> String {
    format!("intent:{proposal_id}")
}

/// `path.scored`'s key part for a diff re-score; `rescore_seq` is the supervisor's.
pub fn score_source_diff(ord: u32, attempt: u32, rescore_seq: u32) -> String {
    format!("diff:{ord}:{attempt}:{rescore_seq}")
}

/// `advice.delivered.delivery_id` for a step-boundary delivery.
pub fn delivery_id_boundary(step_id: &str, attempt: u32) -> String {
    format!("boundary:{step_id}:{attempt}")
}

/// `advice.delivered.delivery_id` for the attempt-end not-delivered row.
pub fn delivery_id_end(attempt: u32) -> String {
    format!("end:{attempt}")
}

/// `advice.answered.answered_in`: the `step_id:attempt` whose output carried the line.
pub fn answered_in(step_id: &str, attempt: u32) -> String {
    format!("{step_id}:{attempt}")
}

/// A council subject for a finding.
pub fn subject_finding(raise_seq: u32) -> String {
    format!("finding:{raise_seq}")
}

/// A council subject for a member-step dispute.
pub fn subject_step(step_id: &str, attempt: u32) -> String {
    format!("step:{step_id}:{attempt}")
}

/// `"g-<run>-<gate_seq>"` for every gate kind (`AgentSession.gate_seq`).
pub fn gate_id(run_id: &str, gate_seq: u32) -> String {
    format!("g-{run_id}-{gate_seq}")
}

// ── Closed token sets (DES-002 §6): every enum a payload documents is typed ─────────────────────

wire_enum! {
    /// `path.started.selection`.
    pub enum Selection { Chosen = "chosen", Random = "random" }
}
wire_enum! {
    /// `path.scored.basis`.
    pub enum ScoreBasis { Intent = "intent", Diff = "diff" }
}
wire_enum! {
    /// `path.scored.plan.depth` (S4 `Depth`).
    pub enum ScoreDepth { None = "none", Standard = "standard", Deep = "deep" }
}
wire_enum! {
    /// `plan.proposed.kind`.
    pub enum ProposalKind { Initial = "initial", Change = "change", Edit = "edit" }
}
wire_enum! {
    /// A plan step's `owner`.
    pub enum StepOwner { Pa = "pa", Team = "team" }
}
wire_enum! {
    /// A composed step's `added_by`.
    pub enum AddedBy { Plan = "plan", Floor = "floor" }
}
wire_enum! {
    /// `plan.revised.reason`.
    pub enum ReviseReason {
        FloorRaised = "floor_raised",
        PaAdded = "pa_added",
        MemberRequest = "member_request",
    }
}
wire_enum! {
    /// The approval mode (`plan.accepted.mode`, `gate.opened{plan_approval}.mode`).
    pub enum PlanMode { Auto = "auto", Manual = "manual" }
}
wire_enum! {
    /// `member.joined.role`.
    pub enum MemberRole { Monitor = "monitor" }
}
wire_enum! {
    /// `member.joined.status`.
    pub enum AttachStatus { Attached = "attached", Failed = "failed" }
}
wire_enum! {
    /// `checkpoint.reached.status`.
    pub enum CheckpointStatus { Completed = "completed", Failed = "failed" }
}
wire_enum! {
    /// `finding.raised.anchor_source`.
    pub enum AnchorSource { Graph = "graph", Hunk = "hunk", None = "none" }
}
wire_enum! {
    /// `advice.delivered.channel`.
    pub enum Channel { AcpSteering = "acp_steering", Boundary = "boundary", None = "none" }
}
wire_enum! {
    /// `advice.delivered.outcome`.
    pub enum DeliveryOutcome {
        Injected = "injected",
        TurnEnded = "turn_ended",
        Refused = "refused",
        NotDelivered = "not_delivered",
    }
}
wire_enum! {
    /// `advice.answered.disposition`.
    pub enum AdviceDisposition { Accepted = "accepted", Declined = "declined" }
}
wire_enum! {
    /// `step.completed.status`: `StepStatus` as `status_to_str` spells it.
    pub enum StepCompletion {
        Ok = "ok",
        Failed = "failed",
        Cancelled = "cancelled",
        ElicitationFailed = "elicitation_failed",
        TimedOut = "timed_out",
    }
}
wire_enum! {
    /// `step.reviewed.verdict`.
    pub enum StepVerdict { Accepted = "accepted", Rejected = "rejected" }
}
wire_enum! {
    /// `step.reviewed.to`: who reworks a rejected step.
    pub enum ReworkBy { Member = "member", Pa = "pa" }
}
wire_enum! {
    /// `finding.settled.status`.
    pub enum SettledStatus { Held = "held", Withdrawn = "withdrawn", Superseded = "superseded" }
}
wire_enum! {
    /// `council.called.trigger`.
    pub enum CouncilTrigger { UnresolvedHigh = "unresolved_high", MemberStep = "member_step" }
}
wire_enum! {
    /// `ledger.folded.transport`.
    pub enum Transport { Bus = "bus", None = "none" }
}
wire_enum! {
    /// `gate.opened{unit_review}.ledger_source`.
    pub enum LedgerSource { Folded = "folded", Synthesized = "synthesized", NoBus = "no_bus" }
}
wire_enum! {
    /// `gate.opened{plan_approval}.reason`.
    pub enum ApprovalReason {
        ManualMode = "manual_mode",
        HighRisk = "high_risk",
        IntoHighRisk = "into_high_risk",
        Override = "override",
    }
}
wire_enum! {
    /// A gate's kind (`gate.decided.kind`; `gate.opened` is tagged by the same tokens).
    pub enum GateKind {
        UnitReview = "unit_review",
        PlanApproval = "plan_approval",
        TeamDispute = "team_dispute",
        TeamTransport = "team_transport",
    }
}
wire_enum! {
    /// `gate.decided.decision`.
    pub enum GateDecision {
        Allow = "allow",
        Deny = "deny",
        Paused = "paused",
        HumanApproved = "human_approved",
        HumanAmended = "human_amended",
        HumanRejected = "human_rejected",
    }
}
wire_enum! {
    /// `path.ended.status`: the terminal `SessionStatus`.
    pub enum PathStatus { Completed = "completed", Failed = "failed", Cancelled = "cancelled" }
}

// ── The envelope and the payload bodies (DES-002 §6) ─────────────────────────────────────────────

/// The fields every team payload carries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub run_id: String,
    pub ord: Option<u32>,
    pub attempt: Option<u32>,
    /// Who acted: a seated CLI instance (the PA, a member or the authoring monitor, e.g.
    /// `claude#1`, `claude#2`, `codex`), `engine`, `human` or `council:<task id>`.
    pub by: String,
    /// Epoch milliseconds at the producer.
    pub at: i64,
    /// The row this one answers, e.g. `"finding.raised#4"`.
    pub re: Option<String>,
}

/// One plan step: a catalog entry plus the step fields a plan may set. Unset fields are omitted
/// (the plan names only what it overrides); the catalog entry supplies the rest (C1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanStep {
    pub catalog: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// Default `pa`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<StepOwner>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depends_on: Option<Vec<String>>,
    /// A raised gate, as the workflow's externally tagged `GateSpec`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<Value>,
    /// On a composed plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_by: Option<AddedBy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floor_reason: Option<String>,
    /// On `plan.revised.added`: the step's catalog position precedes a done step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub late: Option<bool>,
}

/// A manual-mode override of the floor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanOverride {
    pub remove: Vec<String>,
    pub reason: String,
}

/// 1 — `path.started` (E).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PathStarted {
    /// The PA seat instance.
    pub cli: String,
    pub selection: Selection,
    pub roster: Vec<String>,
    /// The problem text, ≤8 KB.
    pub request: String,
    /// The preset the launch named.
    pub workflow: Option<String>,
    /// The launch carried a user-composed plan.
    pub plan: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoreModel {
    pub add: u8,
    pub rationale: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoreSignals {
    pub changed_symbols: u32,
    pub dependents: u32,
    pub products: u32,
    pub contract_change: bool,
    pub test_gap: f32,
    pub critical: bool,
    pub destructive: bool,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScorePlan {
    pub monitors: u8,
    pub depth: ScoreDepth,
    pub post_hoc_reviewer: bool,
    pub post_hoc_other_cli: bool,
}

/// 2 — `path.scored` (E). `score` and `plan` are computed at parse (S4's rule), never read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(from = "PathScoredWire")]
pub struct PathScored {
    /// [`score_source_intent`] | [`score_source_diff`].
    pub score_source: String,
    pub basis: ScoreBasis,
    pub score: u8,
    pub deterministic: u8,
    pub reasons: Vec<String>,
    pub model: Option<ScoreModel>,
    /// `null` when the graph was unusable.
    pub signals: Option<ScoreSignals>,
    pub plan: ScorePlan,
    /// The tree the diff was taken at; `null` for an intent score.
    pub tree: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonitorsAsk {
    pub asked: u8,
}

/// 3 — `plan.proposed` (E).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanProposed {
    /// [`mint_proposal_id`].
    pub proposal_id: String,
    pub base_rev: Option<u32>,
    pub kind: ProposalKind,
    pub preset: Option<String>,
    pub steps: Vec<PlanStep>,
    pub monitors: MonitorsAsk,
    pub asks: Vec<String>,
    pub touch: Vec<String>,
    #[serde(rename = "override")]
    pub override_: Option<PlanOverride>,
    pub rationale: String,
}

/// 4 — `plan.revised` (E).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanRevised {
    pub plan_rev: u32,
    pub proposal_id: Option<String>,
    pub reason: ReviseReason,
    pub from_band: String,
    pub to_band: String,
    pub high_risk: bool,
    pub added: Vec<PlanStep>,
}

/// 5 — `plan.accepted` (E).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanAccepted {
    pub plan_rev: u32,
    pub workflow_id: String,
    pub band: String,
    pub high_risk: bool,
    pub mode: PlanMode,
    pub steps: Vec<PlanStep>,
    #[serde(rename = "override")]
    pub override_: Option<PlanOverride>,
    pub proposal_id: Option<String>,
}

/// 6 — `plan.refused` (E).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanRefused {
    pub proposal_id: String,
    pub base_rev: Option<u32>,
    pub reason: String,
}

/// 7 — `member.joined` (S).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemberJoined {
    pub member_id: String,
    pub open_seq: u32,
    pub seat: String,
    pub role: MemberRole,
    pub status: AttachStatus,
    pub reason: String,
    pub error: Option<String>,
}

/// 8 — `member.left` (S).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemberLeft {
    pub member_id: String,
    pub open_seq: u32,
    pub seat: String,
    pub status: MonitorStatus,
    pub batches: u32,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoRef {
    pub workdir: String,
    pub git_dir: String,
}

/// 9 — `step.claimed` (R).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepClaimed {
    pub step_id: String,
    pub role: String,
    pub kind: String,
    pub phase: String,
    pub criterion: String,
    pub baseline_tree: Option<String>,
    pub repo: Option<RepoRef>,
    pub code_graph_db: Option<String>,
}

/// 10 — `checkpoint.reached` (R, the carrier).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckpointReached {
    pub seq: u64,
    pub tool_call_id: String,
    pub kind: String,
    pub title: String,
    pub status: CheckpointStatus,
    pub paths: Vec<String>,
}

/// 11 — `finding.raised` (S; the envelope's `by` is the authoring member's seat). `finding_id` is
/// computed at parse from `path` and `evidence`, never read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(from = "FindingRaisedWire")]
pub struct FindingRaised {
    /// S's per-attempt emission counter: the key.
    pub raise_seq: u32,
    /// Identity, never a key.
    pub finding_id: String,
    pub member_id: String,
    /// DES-001 §4.6 fields T6 builds; `null` until then.
    pub line_key: Option<String>,
    pub anchor: Option<String>,
    pub anchor_source: Option<AnchorSource>,
    /// The bar: `high` | `medium`; anything else is refused at parse.
    pub severity: Severity,
    pub path: String,
    pub line: u32,
    pub evidence: String,
    pub claim: String,
    pub suggestion: Option<String>,
    pub tree: String,
    pub in_diff: bool,
    pub corroborated_by: Vec<String>,
}

/// 12 — `advice.delivered` (R).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdviceDelivered {
    pub raise_seq: u32,
    pub finding_id: String,
    /// The steer id | [`delivery_id_boundary`] | [`delivery_id_end`].
    pub delivery_id: String,
    pub steer_id: Option<String>,
    pub channel: Channel,
    pub outcome: DeliveryOutcome,
    pub detail: Option<String>,
}

/// 13 — `advice.answered` (R).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdviceAnswered {
    pub raise_seq: u32,
    /// [`answered_in`].
    pub answered_in: String,
    pub finding_id: String,
    pub disposition: AdviceDisposition,
    pub reason: String,
}

/// 14 — `help.requested` (R).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HelpRequested {
    /// [`mint_help_id`].
    pub help_id: String,
    pub help_seq: u32,
    pub question: String,
    pub context: String,
}

/// 15 — `help.answered` (S).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HelpAnswered {
    pub help_id: String,
    /// S's member-turn id.
    pub answer_id: String,
    pub answer: String,
    pub evidence: Vec<String>,
}

/// 16 — `change.requested` (S).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangeRequested {
    /// [`mint_change_id`].
    pub change_id: String,
    pub change_seq: u32,
    pub steps: Vec<PlanStep>,
    pub reason: String,
}

/// 17 — `step.completed` (R).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepCompleted {
    pub step_id: String,
    pub status: StepCompletion,
    pub tree: Option<String>,
    pub output_bytes: u64,
    pub output_ref: String,
}

/// 18 — `step.reviewed` (R).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepReviewed {
    pub step_id: String,
    pub verdict: StepVerdict,
    /// On `rejected`: who reworks it.
    pub to: Option<ReworkBy>,
    pub reason: String,
}

/// 19 — `finding.settled` (S).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FindingSettled {
    pub raise_seq: u32,
    pub finding_id: String,
    pub status: SettledStatus,
    pub reason: String,
    pub final_line: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CouncilPosition {
    pub by: String,
    pub position: String,
    pub reason: String,
}

/// 20 — `council.called` (S).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CouncilCalled {
    /// [`subject_finding`] | [`subject_step`].
    pub subject: String,
    pub finding_id: Option<String>,
    pub trigger: CouncilTrigger,
    pub question: String,
    pub positions: Vec<CouncilPosition>,
    pub evidence: String,
    pub excluded_seats: Vec<String>,
    /// Event ids of the finding's raised/delivered/answered/settled rows.
    pub transcript: Vec<i64>,
}

/// 21 — `council.ruled` (S): `convene_decision`'s `DecisionVerdict`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CouncilRuled {
    pub subject: String,
    pub verdict: Verdict,
    /// On `no_verdict`.
    pub reason: Option<NoVerdictReason>,
    /// `null` when no council was convened (`seats_benched`, `cap`).
    pub task_id: Option<String>,
    pub consensus: bool,
    pub agreement_pct: u8,
    pub dissent: Vec<String>,
    pub returned: u32,
    pub seated: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptRow {
    pub event_id: i64,
    pub event_type: String,
    pub payload: Value,
}

/// `count` is the attempt's row count: computed as `events.len()` at parse unless `truncated`
/// (then it is the uncapped total, a fact the capped list cannot give).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "TranscriptWire")]
pub struct Transcript {
    pub from_event_id: i64,
    pub to_event_id: i64,
    pub count: u32,
    /// The rows were capped (≤256 KB).
    pub truncated: bool,
    pub events: Vec<TranscriptRow>,
}

/// 22 — `ledger.folded` (S). `final_pass` mirrors the embedded ledger's and is taken from it at
/// parse, never read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(from = "LedgerFoldedWire")]
pub struct LedgerFolded {
    pub final_pass: FinalPass,
    pub ledger: TeamLedger,
    pub transport: Transport,
    pub transcript: Transcript,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanDiff {
    pub from_rev: Option<u32>,
    pub added: Vec<String>,
}

/// `gate.opened`'s kind-specific fields, tagged by `kind`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GateOpenedKind {
    UnitReview {
        /// The S row the gate used; `null` for a synthesized or no-bus snapshot.
        ledger_ref: Option<String>,
        ledger_source: LedgerSource,
    },
    PlanApproval {
        reviewing_ord: u32,
        plan_rev: u32,
        band: String,
        high_risk: bool,
        mode: PlanMode,
        reason: ApprovalReason,
        diff: PlanDiff,
    },
    TeamDispute {
        /// The unresolved HIGHs the pause names (DES-001 §6.7 `TeamPause.finding_ids`).
        finding_ids: Vec<String>,
    },
    TeamTransport {
        /// The required fact that could not be published (DES-002 §4.1).
        fact: String,
        reason: String,
    },
}

/// 23 — `gate.opened` (E).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateOpened {
    /// [`gate_id`].
    pub gate_id: String,
    #[serde(flatten)]
    pub kind: GateOpenedKind,
}

/// 24 — `gate.decided` (E).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateDecided {
    pub gate_id: String,
    pub kind: GateKind,
    /// `unit_review`: allow | deny | paused; a human decision (any kind): human_*.
    pub decision: GateDecision,
    pub combined: Option<bool>,
    pub team_pause: bool,
    pub unresolved: Vec<String>,
}

/// 25 — `path.ended` (E).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PathEnded {
    pub status: PathStatus,
}

// ── Wire mirrors: computed fields are never read from input (review of #616, round 5) ────────────
//
// Each type below deserializes through a mirror that omits its computed fields (an incoming one
// is ignored as an unknown key) and recomputes them. The computed fields:
//   TeamLedger.teamPause            ⇐ pauses(finalPass, findings)          (team.rs)
//   Finding.findingId               ⇐ finding_id(path, evidence)           (team.rs)
//   FindingRaised.finding_id        ⇐ finding_id(path, evidence)
//   PathScored.score                ⇐ min(100, deterministic + model.add)
//   PathScored.plan                 ⇐ S4 plan_for(score)
//   LedgerFolded.final_pass         ⇐ ledger.finalPass
//   Transcript.count                ⇐ events.len() unless truncated
//   HelpRequested.help_id           ⇐ mint_help_id(…, help_seq)            (from_payload)

#[derive(Deserialize)]
struct PathScoredWire {
    score_source: String,
    basis: ScoreBasis,
    deterministic: u8,
    reasons: Vec<String>,
    model: Option<ScoreModel>,
    signals: Option<ScoreSignals>,
    tree: Option<String>,
}

impl From<PathScoredWire> for PathScored {
    fn from(w: PathScoredWire) -> Self {
        let add = w.model.as_ref().map_or(0, |m| m.add);
        let score = w.deterministic.saturating_add(add).min(100);
        let p = crate::review_scale::plan_for(score);
        PathScored {
            score_source: w.score_source,
            basis: w.basis,
            score,
            deterministic: w.deterministic,
            reasons: w.reasons,
            model: w.model,
            signals: w.signals,
            plan: ScorePlan {
                monitors: p.monitors,
                depth: match p.depth {
                    crate::review_scale::Depth::None => ScoreDepth::None,
                    crate::review_scale::Depth::Standard => ScoreDepth::Standard,
                    crate::review_scale::Depth::Deep => ScoreDepth::Deep,
                },
                post_hoc_reviewer: p.post_hoc_reviewer,
                post_hoc_other_cli: p.post_hoc_other_cli,
            },
            tree: w.tree,
        }
    }
}

#[derive(Deserialize)]
struct FindingRaisedWire {
    raise_seq: u32,
    member_id: String,
    line_key: Option<String>,
    anchor: Option<String>,
    anchor_source: Option<AnchorSource>,
    severity: Severity,
    path: String,
    line: u32,
    evidence: String,
    claim: String,
    suggestion: Option<String>,
    tree: String,
    in_diff: bool,
    corroborated_by: Vec<String>,
}

impl From<FindingRaisedWire> for FindingRaised {
    fn from(w: FindingRaisedWire) -> Self {
        FindingRaised {
            raise_seq: w.raise_seq,
            finding_id: super::finding_id(&w.path, &w.evidence),
            member_id: w.member_id,
            line_key: w.line_key,
            anchor: w.anchor,
            anchor_source: w.anchor_source,
            severity: w.severity,
            path: w.path,
            line: w.line,
            evidence: w.evidence,
            claim: w.claim,
            suggestion: w.suggestion,
            tree: w.tree,
            in_diff: w.in_diff,
            corroborated_by: w.corroborated_by,
        }
    }
}

#[derive(Deserialize)]
struct TranscriptWire {
    from_event_id: i64,
    to_event_id: i64,
    count: u32,
    truncated: bool,
    events: Vec<TranscriptRow>,
}

impl From<TranscriptWire> for Transcript {
    fn from(w: TranscriptWire) -> Self {
        let count = if w.truncated {
            w.count
        } else {
            u32::try_from(w.events.len()).unwrap_or(u32::MAX)
        };
        Transcript {
            from_event_id: w.from_event_id,
            to_event_id: w.to_event_id,
            count,
            truncated: w.truncated,
            events: w.events,
        }
    }
}

#[derive(Deserialize)]
struct LedgerFoldedWire {
    ledger: TeamLedger,
    transport: Transport,
    transcript: Transcript,
}

impl From<LedgerFoldedWire> for LedgerFolded {
    fn from(w: LedgerFoldedWire) -> Self {
        LedgerFolded {
            final_pass: w.ledger.final_pass,
            ledger: w.ledger,
            transport: w.transport,
            transcript: w.transcript,
        }
    }
}

macro_rules! bodies {
    ($($variant:ident($ty:ident) = $const:ident),* $(,)?) => {
        /// A team event's body, one variant per type.
        #[derive(Debug, Clone, PartialEq)]
        pub enum TeamBody {
            $($variant($ty),)*
        }

        impl TeamBody {
            /// The bus `event_type`.
            pub fn event_type(&self) -> &'static str {
                match self {
                    $(TeamBody::$variant(_) => $const,)*
                }
            }

            fn to_value(&self) -> Result<Value> {
                Ok(match self {
                    $(TeamBody::$variant(b) => serde_json::to_value(b)?,)*
                })
            }

            fn from_value(event_type: &str, v: Value) -> Result<Self> {
                Ok(match event_type {
                    $($const => TeamBody::$variant(serde_json::from_value(v)?),)*
                    other => bail!("`{other}` is not a team event type"),
                })
            }
        }
    };
}

bodies! {
    PathStarted(PathStarted) = PATH_STARTED,
    PathScored(PathScored) = PATH_SCORED,
    PlanProposed(PlanProposed) = PLAN_PROPOSED,
    PlanRevised(PlanRevised) = PLAN_REVISED,
    PlanAccepted(PlanAccepted) = PLAN_ACCEPTED,
    PlanRefused(PlanRefused) = PLAN_REFUSED,
    MemberJoined(MemberJoined) = MEMBER_JOINED,
    MemberLeft(MemberLeft) = MEMBER_LEFT,
    StepClaimed(StepClaimed) = STEP_CLAIMED,
    CheckpointReached(CheckpointReached) = CHECKPOINT_REACHED,
    FindingRaised(FindingRaised) = FINDING_RAISED,
    AdviceDelivered(AdviceDelivered) = ADVICE_DELIVERED,
    AdviceAnswered(AdviceAnswered) = ADVICE_ANSWERED,
    HelpRequested(HelpRequested) = HELP_REQUESTED,
    HelpAnswered(HelpAnswered) = HELP_ANSWERED,
    ChangeRequested(ChangeRequested) = CHANGE_REQUESTED,
    StepCompleted(StepCompleted) = STEP_COMPLETED,
    StepReviewed(StepReviewed) = STEP_REVIEWED,
    FindingSettled(FindingSettled) = FINDING_SETTLED,
    CouncilCalled(CouncilCalled) = COUNCIL_CALLED,
    CouncilRuled(CouncilRuled) = COUNCIL_RULED,
    LedgerFolded(LedgerFolded) = LEDGER_FOLDED,
    GateOpened(GateOpened) = GATE_OPENED,
    GateDecided(GateDecided) = GATE_DECIDED,
    PathEnded(PathEnded) = PATH_ENDED,
}

/// One team event: the envelope plus its body.
#[derive(Debug, Clone, PartialEq)]
pub struct TeamEvent {
    pub env: Envelope,
    pub body: TeamBody,
}

impl TeamEvent {
    pub fn event_type(&self) -> &'static str {
        self.body.event_type()
    }

    /// The bus payload: the envelope's fields and the body's, at one level.
    pub fn to_payload(&self) -> Result<Value> {
        let Value::Object(mut out) = serde_json::to_value(&self.env)? else {
            bail!("envelope did not serialize to an object");
        };
        let Value::Object(body) = self.body.to_value()? else {
            bail!("{} body did not serialize to an object", self.event_type());
        };
        for (k, v) in body {
            if out.contains_key(&k) {
                bail!(
                    "{} body field `{k}` collides with the envelope",
                    self.event_type()
                );
            }
            out.insert(k, v);
        }
        Ok(Value::Object(out))
    }

    /// Parse a bus row's payload for `event_type`.
    pub fn from_payload(event_type: &str, payload: &Value) -> Result<Self> {
        let env: Envelope = serde_json::from_value(payload.clone())?;
        let mut body = TeamBody::from_value(event_type, payload.clone())?;
        // A computed id is never read from input: help_id is minted from the producer's counter.
        if let TeamBody::HelpRequested(b) = &mut body {
            let (Some(ord), Some(attempt)) = (env.ord, env.attempt) else {
                bail!("{event_type} needs `ord` and `attempt` to mint its help_id");
            };
            b.help_id = mint_help_id(&env.run_id, ord, attempt, &env.by, b.help_seq);
        }
        let ev = TeamEvent { env, body };
        // A row whose key cannot be built (a keyed type with a null `ord`/`attempt`) is refused:
        // it could be neither deduplicated nor attributed to its attempt.
        ev.key()?;
        Ok(ev)
    }

    /// The idempotency key (DES-002 §6.1). Errors when a type that is keyed on `ord`/`attempt`
    /// carries a `null` one.
    pub fn key(&self) -> Result<String> {
        key_parts_of(self)
    }

    /// The bus row a publisher writes for this event: `domain = wicked-core`,
    /// `subdomain = core.team`, the payload, and the key.
    pub fn bus_emit(&self) -> Result<BusEmit> {
        Ok(BusEmit::new(
            self.event_type(),
            CORE_DOMAIN,
            TEAM_SUBDOMAIN,
            self.to_payload()?,
        )
        .with_key(self.key()?))
    }
}

/// Dispatch to the key builders. Reads only the ids and counters the builders take.
fn key_parts_of(ev: &TeamEvent) -> Result<String> {
    let run = ev.env.run_id.as_str();
    let by = ev.env.by.as_str();
    let ord = || {
        ev.env
            .ord
            .ok_or_else(|| anyhow!("{} needs `ord` for its key", ev.event_type()))
    };
    let attempt = || {
        ev.env
            .attempt
            .ok_or_else(|| anyhow!("{} needs `attempt` for its key", ev.event_type()))
    };
    Ok(match &ev.body {
        TeamBody::PathStarted(_) => key_path_started(run),
        TeamBody::PathScored(b) => key_path_scored(run, &b.score_source),
        TeamBody::PlanProposed(b) => key_plan_proposed(run, &b.proposal_id),
        TeamBody::PlanRevised(b) => key_plan_revised(run, b.plan_rev),
        TeamBody::PlanAccepted(b) => key_plan_accepted(run, b.plan_rev),
        TeamBody::PlanRefused(b) => key_plan_refused(run, &b.proposal_id),
        TeamBody::MemberJoined(b) => {
            key_member_joined(run, ord()?, attempt()?, &b.member_id, b.open_seq)
        }
        TeamBody::MemberLeft(b) => {
            key_member_left(run, ord()?, attempt()?, &b.member_id, b.open_seq)
        }
        TeamBody::StepClaimed(b) => key_step_claimed(run, &b.step_id, attempt()?, by),
        TeamBody::CheckpointReached(b) => key_checkpoint_reached(run, ord()?, attempt()?, b.seq),
        TeamBody::FindingRaised(b) => key_finding_raised(run, ord()?, attempt()?, b.raise_seq),
        TeamBody::AdviceDelivered(b) => {
            key_advice_delivered(run, ord()?, attempt()?, b.raise_seq, &b.delivery_id)
        }
        TeamBody::AdviceAnswered(b) => {
            key_advice_answered(run, ord()?, attempt()?, b.raise_seq, &b.answered_in)
        }
        TeamBody::HelpRequested(b) => key_help_requested(run, &b.help_id),
        TeamBody::HelpAnswered(b) => key_help_answered(run, &b.help_id, &b.answer_id),
        TeamBody::ChangeRequested(b) => key_change_requested(run, &b.change_id),
        TeamBody::StepCompleted(b) => key_step_completed(run, &b.step_id, attempt()?, by),
        TeamBody::StepReviewed(b) => key_step_reviewed(run, &b.step_id, attempt()?),
        TeamBody::FindingSettled(b) => key_finding_settled(run, ord()?, attempt()?, b.raise_seq),
        TeamBody::CouncilCalled(b) => key_council_called(run, ord()?, attempt()?, &b.subject),
        TeamBody::CouncilRuled(b) => key_council_ruled(run, ord()?, attempt()?, &b.subject),
        TeamBody::LedgerFolded(_) => key_ledger_folded(run, ord()?, attempt()?),
        TeamBody::GateOpened(b) => key_gate_opened(run, &b.gate_id),
        TeamBody::GateDecided(b) => key_gate_decided(run, &b.gate_id),
        TeamBody::PathEnded(_) => key_path_ended(run),
    })
}

// ── fold (DES-002 §4.4, §8.11; DES-001 §6.3, §6.7) ───────────────────────────────────────────────

/// One bus row of an attempt, as the fold reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct TeamRow {
    pub event_id: i64,
    pub event: TeamEvent,
}

/// A finding the worker has not accepted: `status ∉ {accepted, withdrawn, superseded}`
/// (DES-001 §6.3 item 2).
pub fn is_unaccepted(f: &LedgerFinding) -> bool {
    !matches!(
        f.status,
        FindingStatus::Accepted | FindingStatus::Withdrawn | FindingStatus::Superseded
    )
}

/// DES-001 §6.3: HIGH, not accepted, and held by its monitor. A missing hold-round reply counts
/// as HOLD (a monitor's silence never clears a finding).
pub fn is_unresolved_high(f: &LedgerFinding) -> bool {
    f.finding.severity == Severity::High
        && is_unaccepted(f)
        && f.monitor_reply
            .as_ref()
            .is_none_or(|r| r.kind == ReplyKind::Hold)
}

/// The findings a council must rule on (DES-001 §6.3, acceptance #15).
pub fn unresolved_highs(ledger: &TeamLedger) -> Vec<&LedgerFinding> {
    ledger
        .findings
        .iter()
        .filter(|f| is_unresolved_high(f))
        .collect()
}

/// DES-001 §6.7: an unresolved HIGH whose council did not say YES (no verdict counts as NO).
/// An incomplete record (`stream_gap`) pauses too: it goes to a human, never auto-approved
/// (DES-002 §4.7).
pub fn ledger_pauses(ledger: &TeamLedger) -> bool {
    pauses(ledger.final_pass, &ledger.findings)
}

/// [`ledger_pauses`] over a ledger's parts: THE one place the rule lives. `TeamLedger::new` and
/// `TeamLedger::refresh_pause` are its only writers of `teamPause`.
pub fn pauses(final_pass: FinalPass, findings: &[LedgerFinding]) -> bool {
    final_pass == FinalPass::StreamGap
        || findings
            .iter()
            .filter(|f| is_unresolved_high(f))
            .any(|f| f.dispute.as_ref().is_none_or(|d| d.verdict != Verdict::Yes))
}

/// The gate's half of DES-001 §6.7: a unit pauses `team_dispute` only when the gate approved
/// (`outcome.approved`) and its ledger pauses. A denied unit is denied, never paused.
pub fn gate_pauses(approved: bool, ledger: &TeamLedger) -> bool {
    approved && ledger.team_pause
}

/// Fold one attempt's rows into its [`TeamLedger`]: a pure function of the rows, independent of
/// their delivery order and of duplicates (rows are ordered by `event_id` and deduplicated by
/// key, so a re-delivered or re-published row folds once).
///
/// What the stream carries, the fold reproduces: monitors (`member.joined`/`member.left`),
/// findings (`finding.raised`), delivery (`advice.delivered`; `injected` on any channel wins),
/// the worker's disposition (`advice.answered`; the latest line wins, as S3's parser keeps the
/// last `ADVICE` line), the hold round and re-confirmation (`finding.settled`), and each
/// finding's council (`council.ruled` on subject `finding:<raise_seq>`). `final_pass` is
/// `skipped` when the attempt's `step.completed` did not return `ok`, else `completed`.
/// `teamPause` is [`ledger_pauses`].
///
/// **Complete or unpaused only on positive evidence.** `final_pass` is `completed` only when a
/// terminal `step.completed{ok}` row of the attempt is present, and `skipped` only when that row
/// says the step did not return ok. Every other stream, an empty one included, is `stream_gap`
/// and pauses: the absence of a failure row never counts as success.
///
/// **No row is dropped silently, and none is misapplied.** Every payload enum is a closed set and
/// every row must carry its key, so a malformed row never parses into a [`TeamRow`]. Each of these
/// means the record is incomplete, so `final_pass` becomes `stream_gap`, the row is not applied,
/// and the ledger pauses:
/// - a row that reaches the fold without a key;
/// - a row that names a finding this stream never raised (an `advice.*`, `finding.settled` or
///   `council.ruled{finding:<n>}` with no `finding.raised` for `<n>`);
/// - a row that claims a raised finding's `raise_seq` but names another `finding_id`, or comes
///   from another attempt;
/// - no terminal `step.completed` row, terminal rows of two attempts, or a member, finding or
///   step row whose `(ord, attempt)` is not the terminal row's.
///
/// Not on the stream, so not folded: `rejected{}` (a rejected monitor line is never raised) and a
/// corroboration that arrives after the finding was raised. The supervisor, which owns both,
/// overlays them on the fold's result before it publishes `ledger.folded`.
pub fn fold(rows: &[TeamRow]) -> TeamLedger {
    let mut ordered: Vec<&TeamRow> = rows.iter().collect();
    ordered.sort_by_key(|r| r.event_id);
    let mut keys = HashSet::new();
    // A row without a key (built past `from_payload`) is never applied: it is a gap.
    let mut gap = false;
    ordered.retain(|r| match r.event.key() {
        Ok(k) => keys.insert(k),
        Err(_) => {
            gap = true;
            false
        }
    });

    // The attempt is the one its terminal `step.completed` names: the positive evidence that the
    // attempt's turn returned. No terminal row, or terminal rows of two attempts, is a gap.
    let terminals: Vec<(Option<u32>, Option<u32>)> = ordered
        .iter()
        .filter(|r| matches!(r.event.body, TeamBody::StepCompleted(_)))
        .map(|r| (r.event.env.ord, r.event.env.attempt))
        .collect();
    let attempt = terminals.first().copied();
    if attempt.is_none() || terminals.iter().any(|t| Some(*t) != attempt) {
        gap = true;
    }

    let mut monitors: Vec<MonitorAcc> = Vec::new();
    let mut findings: Vec<FindingAcc> = Vec::new();
    // Rows about findings, applied once every raise is known (order-independent).
    let mut about: Vec<&TeamRow> = Vec::new();
    let mut skipped = false;
    for row in ordered {
        let env = &row.event.env;
        let consumed = matches!(
            row.event.body,
            TeamBody::MemberJoined(_)
                | TeamBody::MemberLeft(_)
                | TeamBody::FindingRaised(_)
                | TeamBody::AdviceDelivered(_)
                | TeamBody::AdviceAnswered(_)
                | TeamBody::FindingSettled(_)
                | TeamBody::CouncilRuled(_)
                | TeamBody::StepCompleted(_)
        );
        // A consumed row of another attempt is not this attempt's: a gap, never applied. (With no
        // terminal row at all the rows are still folded, so no finding disappears, and the gap
        // above keeps the ledger paused.)
        if consumed && attempt.is_some_and(|at| (env.ord, env.attempt) != at) {
            gap = true;
            continue;
        }
        match &row.event.body {
            TeamBody::MemberJoined(b) => {
                let m = monitor_entry(&mut monitors, &b.member_id, &b.seat);
                if b.open_seq >= m.open_seq {
                    m.open_seq = b.open_seq;
                    m.seat = b.seat.clone();
                    m.joined_failed = (b.status == AttachStatus::Failed).then(|| b.error.clone());
                }
            }
            TeamBody::MemberLeft(b) => {
                let m = monitor_entry(&mut monitors, &b.member_id, &b.seat);
                m.open_seq = m.open_seq.max(b.open_seq);
                m.left.insert(b.open_seq, b.clone());
            }
            // Keys are (ord, attempt, raise_seq): after the key dedup one raise per raise_seq.
            TeamBody::FindingRaised(b) => findings.push(FindingAcc {
                raise_seq: b.raise_seq,
                at: (env.ord, env.attempt),
                finding: Finding {
                    finding_id: b.finding_id.clone(),
                    monitor_id: b.member_id.clone(),
                    seat: env.by.clone(),
                    severity: b.severity,
                    path: b.path.clone(),
                    line: b.line,
                    evidence: b.evidence.clone(),
                    claim: b.claim.clone(),
                    suggestion: b.suggestion.clone(),
                    tree: b.tree.clone(),
                    in_diff: b.in_diff,
                    checkpoint_seq: 0,
                },
                corroborated_by: b.corroborated_by.clone(),
                injected: false,
                answer: None,
                settled: None,
                ruling: None,
            }),
            TeamBody::AdviceDelivered(_)
            | TeamBody::AdviceAnswered(_)
            | TeamBody::FindingSettled(_)
            | TeamBody::CouncilRuled(_) => about.push(row),
            TeamBody::StepCompleted(b) => skipped |= b.status != StepCompletion::Ok,
            _ => {}
        }
    }

    for row in about {
        let seq = match &row.event.body {
            TeamBody::AdviceDelivered(b) => b.raise_seq,
            TeamBody::AdviceAnswered(b) => b.raise_seq,
            TeamBody::FindingSettled(b) => b.raise_seq,
            TeamBody::CouncilRuled(b) => match b.subject.strip_prefix("finding:") {
                Some(n) => match n.parse::<u32>() {
                    Ok(n) => n,
                    Err(_) => {
                        gap = true;
                        continue;
                    }
                },
                // A member-step dispute: not a finding's council.
                None => continue,
            },
            _ => unreachable!("only finding rows are collected"),
        };
        let Some(f) = finding_entry(&mut findings, seq) else {
            gap = true;
            continue;
        };
        // The row must be about THIS finding: the same attempt and, where it names one, the
        // same `finding_id`. A row matched by `raise_seq` alone could answer another finding.
        let named = match &row.event.body {
            TeamBody::AdviceDelivered(b) => Some(&b.finding_id),
            TeamBody::AdviceAnswered(b) => Some(&b.finding_id),
            TeamBody::FindingSettled(b) => Some(&b.finding_id),
            _ => None,
        };
        let env = &row.event.env;
        if (env.ord, env.attempt) != f.at || named.is_some_and(|id| *id != f.finding.finding_id) {
            gap = true;
            continue;
        }
        match &row.event.body {
            TeamBody::AdviceDelivered(b) => f.injected |= b.outcome == DeliveryOutcome::Injected,
            TeamBody::AdviceAnswered(b) => f.answer = Some(b.clone()),
            TeamBody::FindingSettled(b) => f.settled = Some(b.clone()),
            TeamBody::CouncilRuled(b) => f.ruling = Some(b.clone()),
            _ => {}
        }
    }

    let final_pass = if gap {
        FinalPass::StreamGap
    } else if skipped {
        FinalPass::Skipped
    } else {
        FinalPass::Completed
    };
    TeamLedger::new(
        final_pass,
        monitors.into_iter().map(MonitorAcc::into_ledger).collect(),
        findings.into_iter().map(FindingAcc::into_ledger).collect(),
        Default::default(),
    )
}

/// DES-001 §4.7 budget expiry, fail-closed: `finalPass: "timed_out"`, and every unaccepted HIGH
/// missing a hold-round reply or a council verdict gets `{kind:"hold", reason:"no reply (final
/// pass timed out)"}` / `{verdict:"no_verdict", reason:"timeout"}`. Recorded results are kept;
/// MEDIUM and accepted/withdrawn/superseded findings are untouched. `teamPause` is recomputed.
pub fn synthesize_timeout(mut ledger: TeamLedger) -> TeamLedger {
    ledger.final_pass = FinalPass::TimedOut;
    for f in &mut ledger.findings {
        if f.finding.severity != Severity::High || !is_unaccepted(f) {
            continue;
        }
        if f.monitor_reply.is_none() {
            f.monitor_reply = Some(MonitorReply {
                kind: ReplyKind::Hold,
                reason: "no reply (final pass timed out)".to_string(),
            });
        }
        if f.dispute.is_none() {
            f.dispute = Some(no_verdict(NoVerdictReason::Timeout));
        }
    }
    ledger.refresh_pause();
    ledger
}

fn no_verdict(reason: NoVerdictReason) -> Dispute {
    Dispute {
        verdict: Verdict::NoVerdict,
        agreement_pct: None,
        dissent: None,
        seats: Vec::new(),
        reason: Some(reason),
    }
}

/// One monitor's rows: its latest opening, and each opening's `member.left`.
struct MonitorAcc {
    id: String,
    seat: String,
    open_seq: u32,
    /// The latest opening failed to attach, with its error.
    joined_failed: Option<Option<String>>,
    left: BTreeMap<u32, MemberLeft>,
}

impl MonitorAcc {
    fn into_ledger(self) -> LedgerMonitor {
        let earlier_batches = self.left.values().next_back().map_or(0, |l| l.batches);
        let (status, batches, error) = match (self.left.get(&self.open_seq), self.joined_failed) {
            (Some(l), _) => (l.status, l.batches, l.error.clone()),
            (None, Some(error)) => (MonitorStatus::Failed, earlier_batches, error),
            // Open with no `member.left`: the final pass never closed it (fail-visible).
            (None, None) => (
                MonitorStatus::TimedOut,
                earlier_batches,
                Some("no member.left for this opening".to_string()),
            ),
        };
        LedgerMonitor {
            monitor_id: self.id,
            seat: self.seat,
            batches,
            status,
            error,
        }
    }
}

fn monitor_entry<'a>(
    monitors: &'a mut Vec<MonitorAcc>,
    id: &str,
    seat: &str,
) -> &'a mut MonitorAcc {
    let i = match monitors.iter().position(|m| m.id == id) {
        Some(i) => i,
        None => {
            monitors.push(MonitorAcc {
                id: id.to_string(),
                seat: seat.to_string(),
                open_seq: 0,
                joined_failed: None,
                left: BTreeMap::new(),
            });
            monitors.len() - 1
        }
    };
    &mut monitors[i]
}

/// One finding's rows, keyed by the supervisor's `raise_seq`.
struct FindingAcc {
    raise_seq: u32,
    /// The raise's `(ord, attempt)`: rows of another attempt are not about this finding.
    at: (Option<u32>, Option<u32>),
    finding: Finding,
    corroborated_by: Vec<String>,
    /// Some delivery answered `injected` (any channel).
    injected: bool,
    /// The latest `ADVICE` line for it.
    answer: Option<AdviceAnswered>,
    settled: Option<FindingSettled>,
    ruling: Option<CouncilRuled>,
}

impl FindingAcc {
    fn into_ledger(self) -> LedgerFinding {
        let settled = self.settled.as_ref().map(|s| s.status);
        let status = match settled {
            Some(SettledStatus::Withdrawn) => FindingStatus::Withdrawn,
            Some(SettledStatus::Superseded) => FindingStatus::Superseded,
            _ => match self.answer.as_ref().map(|a| a.disposition) {
                Some(AdviceDisposition::Accepted) => FindingStatus::Accepted,
                Some(AdviceDisposition::Declined) => FindingStatus::Declined,
                None => FindingStatus::Unanswered,
            },
        };
        let monitor_reply = self.settled.as_ref().and_then(|s| {
            let kind = match s.status {
                SettledStatus::Held => ReplyKind::Hold,
                SettledStatus::Withdrawn => ReplyKind::Withdraw,
                SettledStatus::Superseded => return None,
            };
            Some(MonitorReply {
                kind,
                reason: s.reason.clone(),
            })
        });
        let dispute = self.ruling.map(|r| match r.verdict {
            Verdict::Yes | Verdict::No => Dispute {
                verdict: r.verdict,
                agreement_pct: Some(r.agreement_pct),
                dissent: Some(r.dissent.len() as u32),
                seats: Vec::new(),
                reason: r.reason,
            },
            // A no-verdict with no reason is the council call failing to say why: `error`.
            Verdict::NoVerdict => no_verdict(r.reason.unwrap_or(NoVerdictReason::Error)),
        });
        LedgerFinding {
            finding: self.finding,
            final_line: self.settled.as_ref().and_then(|s| s.final_line),
            corroborated_by: self.corroborated_by,
            delivery: if self.injected {
                LedgerDelivery::Injected
            } else {
                LedgerDelivery::NotDelivered
            },
            status,
            worker_reason: self.answer.map(|a| a.reason),
            monitor_reply,
            dispute,
        }
    }
}

fn finding_entry(findings: &mut [FindingAcc], raise_seq: u32) -> Option<&mut FindingAcc> {
    findings.iter_mut().find(|f| f.raise_seq == raise_seq)
}

#[cfg(test)]
#[path = "events_tests.rs"]
mod tests;
