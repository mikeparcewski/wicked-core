//! MCP CALL GOVERNANCE — one evaluation per brokered MCP call, on the same steering engine the
//! tool gate uses (DES-MCP-TOOLS-001 §3-§4, slice S1).
//!
//! # Where this sits
//!
//! A worker reaches an MCP tool through crew's broker (the garden shim, `wicked-garden run mcp call`,
//! on all six seats). The broker holds the upstream connection and its secret; before it invokes
//! anything it asks core, through the `evaluateMcpCall` binding, whether THIS unit may make THIS
//! call. [`evaluate_mcp_call`] answers, records the answer as a claim in the unit's decisions log,
//! and refuses when it cannot record (D-3, fail closed).
//!
//! # The decision
//!
//! 1. **D-5 `engine:mcp-unregistered`**: a server or tool the broker's registry does not hold,
//!    or holds disabled or removed, is denied.
//! 2. **D-1 `engine:mcp-phase-role`**: a unit whose write posture is read-only, or whose phase plays
//!    evaluator, never calls a `write` or `destructive` tool — on every seat, in every mode. The
//!    posture is [`WritePosture::of`], the same derivation every carrier reads.
//! 3. **Policy**: `select_any` over the unit's phase tokens plus the MCP subject tokens
//!    ([`subject_tokens`]: `mcp`, `mcp:<server>`, `mcp:<server>/<tool>`, and the run's mode
//!    `mcp-mode:<ask|balanced|autonomous>`), then `decide` — deny dominates. An
//!    `allow_with_conditions` carrying [`APPROVAL_OBLIGATION`] is an **ask**: the call waits for the
//!    operator. The shipped posture rules live in the `mcp-defaults` policy pack
//!    (`governance/packs/mcp-defaults/`), seeded into the store at boot by [`seed_mcp_defaults`]:
//!    ask mode asks for every write; balanced runs reads and asks for a write until the tool is
//!    approved; autonomous runs both.
//! 4. **D-6 `engine:mcp-first-use`**: unless a policy denied, the first use of a server (or of a
//!    tool whose schema changed) asks in every mode. The approvals ledger is the `MCP-FIRST-USE`
//!    rule's `excludes`, read whether that rule is active or retired.
//!
//! D-4 (a tool with no annotations is `write`) is [`classify`]. D-2 (secrets stay in the broker)
//! and the call record are crew's; this module never sees a secret.
//!
//! # Denied means blocked and disclosed, not failed
//!
//! Every deny is recorded under the ADVISORY claim class [`crate::gate_hook::MCP_DENY_PREFIX`] with
//! [`MCP_EVALUATOR`]: the call never ran, the seat is handed the reason and the remedy, and the
//! unit continues (operator decision 2, 2026-09-28). The fold discloses it as
//! `workerToolCallDenied`. What stays fatal is an unrecordable call: [`evaluate_mcp_call`] returns
//! [`McpCallError::GuardError`] and the broker refuses it.
//!
//! # The capability token
//!
//! A worker holds `WICKED_MCP_TOKEN`, never a secret. The token is minted by the carrier that spawns
//! the worker process ([`McpToken::mint`]) and is BOUND to the unit currently running on that
//! process for exactly the unit's lifetime ([`McpToken::bind`] returns a guard). The wrapped carrier
//! spawns one process per unit; the ACP carrier reuses a process across the units of one run on one
//! seat, so its token outlives a unit but resolves to nothing between turns. The registry is
//! process-global: crew hosts core in-process, so the carriers and the binding share it. The token
//! holds no grant list; policy decides.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use wicked_apps_core::{open_store_ro, synthetic_symbol, ConformanceClaim, Decision, GraphRead};

use crate::domain::HumanConfirm;
use crate::workflow::PhaseRole;
use crate::write_posture::WritePosture;

/// The evaluator identity every MCP claim carries. The fold keys the advisory class on it together
/// with the claim-id prefix, so no other recorder's deny can pass as an MCP one.
pub(crate) const MCP_EVALUATOR: &str = "wicked-governance-mcp";

/// D-1: a read-only or evaluator unit called a write-class tool.
pub(crate) const RULE_PHASE_ROLE: &str = "engine:mcp-phase-role";

/// D-5: the server or tool is not registered, or is disabled or removed.
pub(crate) const RULE_UNREGISTERED: &str = "engine:mcp-unregistered";

/// First use of a server (or of a tool whose schema changed) waits for the operator, in every mode.
pub(crate) const RULE_FIRST_USE: &str = "engine:mcp-first-use";

/// The pack rule that is the first-use APPROVALS LEDGER: its `excludes` lists the approved
/// `mcp:<server>` (or `mcp:<server>/<tool>`) tokens, edited through crew's audited rule upsert. The
/// engine reads the list whether the rule is active or retired, so retiring it never lifts the ask,
/// and a store without it approves nothing (fail closed).
pub(crate) const FIRST_USE_LEDGER: &str = "MCP-FIRST-USE";

/// The obligation that turns an `allow_with_conditions` into an ASK: the call waits for the
/// operator. No new effect: "ask" lives inside the existing decision vocabulary.
pub const APPROVAL_OBLIGATION: &str = "mcp:approval";

/// The worker env var carrying the unit's capability token.
pub const TOKEN_ENV: &str = "WICKED_MCP_TOKEN";

/// The worker env var carrying the broker's base URL. Read from the daemon's own environment, which
/// crew sets when it listens; a daemon with none hands its workers no MCP channel at all.
pub const CREW_URL_ENV: &str = "WICKED_CREW_URL";

/// The `carrier` a brokered call is disclosed under (`workerToolCallDenied.carrier`).
pub(crate) const CARRIER_SHIM: &str = "shim";

/// Claim-id prefixes of the non-deny MCP decisions (the deny prefix is the gate hook's, shared with
/// the carrier-side fence).
const MCP_ALLOW_PREFIX: &str = "mcp-allow:";
const MCP_ASK_PREFIX: &str = "mcp-ask:";

/// The policy pack the defaults ship in, embedded so the seed and `rules ingest` read one source.
const MCP_DEFAULTS_RULES: &str =
    include_str!("../governance/packs/mcp-defaults/rules/mcp-defaults.json");

// ─────────────────────────────────────────────────────────────────────────────
// Classification (D-4)
// ─────────────────────────────────────────────────────────────────────────────

/// A tool's class, decided by the broker's registry from `tools/list`, never by the carrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpClass {
    Read,
    Write,
    Destructive,
}

impl McpClass {
    fn wire(self) -> &'static str {
        match self {
            McpClass::Read => "read",
            McpClass::Write => "write",
            McpClass::Destructive => "destructive",
        }
    }
}

/// The MCP tool annotations (spec names). Every hint is optional: absent is not `false`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpAnnotations {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only_hint: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destructive_hint: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotent_hint: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_world_hint: Option<bool>,
}

/// The ONE class derivation (§4.2). An operator override wins; `readOnlyHint: true` is `read`; a
/// tool with no hints at all is `write` (D-4); otherwise `destructiveHint` absent or `true` is
/// `destructive` (the MCP spec default) and `false` is `write`.
pub fn classify(
    annotations: Option<&McpAnnotations>,
    class_override: Option<McpClass>,
) -> McpClass {
    if let Some(c) = class_override {
        return c;
    }
    let Some(a) = annotations else {
        return McpClass::Write;
    };
    if a.read_only_hint == Some(true) {
        return McpClass::Read;
    }
    if a.read_only_hint.is_none() && a.destructive_hint.is_none() {
        return McpClass::Write;
    }
    if a.destructive_hint == Some(false) {
        McpClass::Write
    } else {
        McpClass::Destructive
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The run mode
// ─────────────────────────────────────────────────────────────────────────────

/// The run's MCP posture mode, read off the run-level autonomy the operator launched with (studio's
/// Ask / Balanced / Autonomous map onto `human_confirm` `all` / `before:<n>` / none). No second
/// knob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpMode {
    /// Gate every step: every write call asks.
    Ask,
    /// Gate by risk: read-only tools run; write tools ask until approved.
    Balanced,
    /// Reads and writes run, and are recorded.
    Autonomous,
}

impl McpMode {
    pub fn of(hc: &HumanConfirm) -> Self {
        match hc {
            HumanConfirm::All => McpMode::Ask,
            HumanConfirm::Before(_) => McpMode::Balanced,
            HumanConfirm::None => McpMode::Autonomous,
        }
    }

    fn wire(self) -> &'static str {
        match self {
            McpMode::Ask => "ask",
            McpMode::Balanced => "balanced",
            McpMode::Autonomous => "autonomous",
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The call and the unit
// ─────────────────────────────────────────────────────────────────────────────

/// One MCP call as the broker resolved it against its registry. The broker, not the worker, fills
/// `annotations`, `classOverride` and `registered`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpCall {
    pub server: String,
    pub tool: String,
    #[serde(default)]
    pub args: Value,
    #[serde(default)]
    pub annotations: Option<McpAnnotations>,
    #[serde(default)]
    pub class_override: Option<McpClass>,
    /// Whether the registry holds this server AND tool, enabled and not removed. `false` is D-5. A
    /// changed schema is not a deny: the registry withdraws the tool's first-use approval, so the
    /// next call asks again (D-6).
    pub registered: bool,
    /// `mcp-stdio` | `mcp-http` | `rest`.
    #[serde(default = "default_kind")]
    pub kind: String,
    /// `shim` for a brokered call.
    #[serde(default = "default_carrier")]
    pub carrier: String,
}

fn default_kind() -> String {
    "mcp-stdio".to_string()
}

fn default_carrier() -> String {
    CARRIER_SHIM.to_string()
}

/// What a token resolves to: the unit the call is judged for. Built by the carrier from the unit it
/// is running, never from anything the worker sends.
#[derive(Debug, Clone)]
pub(crate) struct McpUnit {
    pub run_id: String,
    pub attempt: u32,
    pub ord: u32,
    pub scope: String,
    /// `unit-<ord>`, the phase every claim of the unit is recorded under.
    pub phase: String,
    /// The workflow phase id and the catalog id (the tool gate's aliases); empty ⇒ absent.
    pub phase_id: String,
    pub catalog: String,
    pub role: PhaseRole,
    pub posture: WritePosture,
    pub seat: String,
    pub mode: McpMode,
    pub decisions_path: PathBuf,
    /// The operational store the policies are read from (read-only).
    pub db_path: String,
}

impl McpUnit {
    /// The unit a carrier is about to run, from the same facts its tool gate reads.
    pub(crate) fn of(input: &crate::workflow::StepInput, db_path: &str, mode: McpMode) -> Self {
        McpUnit {
            run_id: input.run_id.clone(),
            attempt: input.attempt,
            ord: input.unit.ord,
            scope: crate::scope::resolve_scope(input.entity_mode, &input.run_id, &input.unit.id),
            phase: crate::scope::unit_phase(input.unit.ord),
            phase_id: input.unit.phase_id().unwrap_or_default().to_string(),
            catalog: input.unit.catalog.clone().unwrap_or_default(),
            role: input.unit.role,
            posture: WritePosture::of(&input.unit, input.workdir.is_some()),
            seat: input
                .unit
                .assigned_cli
                .clone()
                .unwrap_or_else(|| "claude".to_string()),
            mode,
            decisions_path: crate::gate_hook::decisions_path_for(&input.run_id, input.attempt),
            db_path: db_path.to_string(),
        }
    }
}

/// The verdict the broker acts on. `decision` is `allow` | `ask` | `deny`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpVerdict {
    pub decision: &'static str,
    pub subject: String,
    pub class: McpClass,
    pub rule_ids: Vec<String>,
    pub obligations: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remedy: Option<String>,
    pub claim_id: String,
    /// Who made the call, for the broker's call record (run = trace, unit = parent span).
    pub unit: McpUnitRef,
}

/// The unit a verdict was judged for, as the call record names it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpUnitRef {
    pub run_id: String,
    pub ord: u32,
    pub attempt: u32,
    pub phase: String,
    pub seat: String,
}

impl McpUnit {
    fn unit_ref(&self) -> McpUnitRef {
        McpUnitRef {
            run_id: self.run_id.clone(),
            ord: self.ord,
            attempt: self.attempt,
            phase: self.phase.clone(),
            seat: self.seat.clone(),
        }
    }
}

/// Why a call could not be judged. Every arm is a refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpCallError {
    /// The token is unknown, revoked, or not bound to a running unit.
    InvalidToken,
    /// The request is malformed (bad server/tool name, unparseable body).
    BadRequest(String),
    /// The decision could not be evaluated or recorded — D-3, fail closed.
    GuardError(String),
}

impl McpCallError {
    fn code(&self) -> &'static str {
        match self {
            McpCallError::InvalidToken => "invalid_token",
            McpCallError::BadRequest(_) => "bad_request",
            McpCallError::GuardError(_) => "guard_error",
        }
    }
}

impl std::fmt::Display for McpCallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McpCallError::InvalidToken => write!(
                f,
                "invalid_token: the MCP token is not bound to a running unit"
            ),
            McpCallError::BadRequest(m) | McpCallError::GuardError(m) => {
                write!(f, "{}: {m}", self.code())
            }
        }
    }
}

/// A server or tool name is one segment of a subject token: it may not carry `/`, `:` or whitespace,
/// or `mcp:<server>/<tool>` would stop naming exactly one tool.
fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

/// The subject a policy names: `mcp:<server>/<tool>`.
pub fn subject_of(server: &str, tool: &str) -> String {
    format!("mcp:{server}/{tool}")
}

/// The MCP phase tokens `select_any` matches `applies_to` against, beside the unit's own.
fn subject_tokens(server: &str, tool: &str, mode: McpMode) -> [String; 4] {
    [
        "mcp".to_string(),
        format!("mcp:{server}"),
        subject_of(server, tool),
        format!("mcp-mode:{}", mode.wire()),
    ]
}

/// The evaluation context (§4.2), compatible with the tool gate's: `tool`/`work` name the subject,
/// and the canonical JSON (sorted keys) is what a rule's `trigger.contains` matches.
fn context(unit: &McpUnit, call: &McpCall, class: McpClass) -> Value {
    let subject = subject_of(&call.server, &call.tool);
    serde_json::json!({
        "phase": unit.phase,
        "scope": unit.scope,
        "tool": subject,
        "work": subject,
        "args": call.args,
        "mcp": {
            "server": call.server,
            "tool": call.tool,
            "subject": subject,
            "class": class.wire(),
            "kind": call.kind,
            "annotations": call.annotations.clone().unwrap_or_default(),
            "registered": call.registered,
        },
        "mode": unit.mode.wire(),
        "phase_role": crate::write_posture::role_wire(unit.role),
        "write_posture": match unit.posture {
            WritePosture::Full => "full",
            WritePosture::DeliverableRoots => "deliverable_roots",
            WritePosture::ReadOnly => "read_only",
        },
        "seat": unit.seat,
        "carrier": call.carrier,
    })
}

/// Judge `call` for `unit` against the policies in `store`, and return the verdict with the claim
/// to record. Pure: no record is written.
pub(crate) fn evaluate(
    store: &dyn GraphRead,
    unit: &McpUnit,
    call: &McpCall,
    evaluated_at: i64,
) -> Result<(McpVerdict, ConformanceClaim), McpCallError> {
    if !valid_name(&call.server) || !valid_name(&call.tool) {
        return Err(McpCallError::BadRequest(format!(
            "server and tool must be non-empty names of [A-Za-z0-9_.-] (got {:?} / {:?})",
            call.server, call.tool
        )));
    }
    let class = classify(call.annotations.as_ref(), call.class_override);
    let subject = subject_of(&call.server, &call.tool);
    let ctx = context(unit, call, class);

    // The two engine gates hold in every mode and no rule can lift them.
    let engine_deny = if !call.registered {
        Some((
            RULE_UNREGISTERED,
            format!("mcp: `{subject}` is not a registered, enabled tool (D-5)"),
            "register the server and enable the tool in studio → MCP tools",
        ))
    } else if class != McpClass::Read
        && (unit.posture == WritePosture::ReadOnly || unit.role == PhaseRole::Evaluator)
    {
        Some((
            RULE_PHASE_ROLE,
            format!(
                "mcp: `{subject}` is a {} tool and this {} unit is read-only (D-1: evaluator ≠ creator)",
                class.wire(),
                crate::write_posture::role_wire(unit.role),
            ),
            "report what the write would be in your output; a creator phase makes it",
        ))
    } else {
        None
    };
    if let Some((rule, reason, remedy)) = engine_deny {
        return Ok(deny(
            unit,
            &subject,
            class,
            vec![rule.to_string()],
            reason,
            remedy,
            evaluated_at,
        ));
    }

    require_defaults(store)?;
    let mcp_tokens = subject_tokens(&call.server, &call.tool, unit.mode);
    let mut phases: Vec<&str> =
        crate::scope::phase_aliases(&unit.phase, Some(&unit.phase_id), Some(&unit.catalog));
    phases.extend(mcp_tokens.iter().map(String::as_str));
    let selected = wicked_governance::select_any(store, &unit.scope, &phases, &ctx)
        .map_err(|e| McpCallError::GuardError(format!("policy select failed: {e}")))?;
    let claim = wicked_governance::decide_as(
        &selected,
        &unit.scope,
        &unit.phase,
        &ctx,
        evaluated_at,
        MCP_EVALUATOR,
    );
    // First use asks in every mode, but never over a deny: deny dominates.
    if claim.decision != Decision::Deny && !first_use_approved(store, &call.server, &call.tool)? {
        let mut claim = claim;
        claim.decision = Decision::AllowWithConditions;
        claim.policy_ids.retain(|id| id != RULE_FIRST_USE);
        claim.policy_ids.insert(0, RULE_FIRST_USE.to_string());
        if !claim.obligations.iter().any(|o| o == APPROVAL_OBLIGATION) {
            claim.obligations.insert(0, APPROVAL_OBLIGATION.to_string());
        }
        return Ok(ask(
            unit,
            &subject,
            class,
            claim,
            "the first use of this server",
        ));
    }
    match claim.decision {
        Decision::Deny => {
            // Name the rules that DENIED, in precedence order — not every rule that fired.
            let denying: Vec<String> = claim
                .policy_ids
                .iter()
                .filter(|id| {
                    selected
                        .iter()
                        .any(|p| &p.id == *id && p.effect == wicked_governance::Effect::Deny)
                })
                .cloned()
                .collect();
            // Never a deny that names nothing: fall back to every rule the claim carries.
            let denying = if denying.is_empty() {
                claim.policy_ids.clone()
            } else {
                denying
            };
            let reason = format!(
                "mcp: `{subject}` is denied by policy {}",
                denying.join(", ")
            );
            Ok(deny(
                unit,
                &subject,
                class,
                denying,
                reason,
                "the policy names what is allowed instead; recall it with its rule id",
                evaluated_at,
            ))
        }
        Decision::AllowWithConditions
            if claim.obligations.iter().any(|o| o == APPROVAL_OBLIGATION) =>
        {
            Ok(ask(unit, &subject, class, claim, "the run's mode"))
        }
        _ => {
            let mut claim = claim;
            claim.claim_id = format!("{MCP_ALLOW_PREFIX}{}", unit.phase);
            let verdict = McpVerdict {
                decision: "allow",
                subject,
                class,
                rule_ids: claim.policy_ids.clone(),
                obligations: claim.obligations.clone(),
                reason: None,
                remedy: None,
                claim_id: claim.claim_id.clone(),
                unit: unit.unit_ref(),
            };
            Ok((verdict, claim))
        }
    }
}

/// An ask verdict: the call did not run and waits for the operator. Recorded as the
/// `allow_with_conditions` claim it is (never as a deny, I5).
fn ask(
    unit: &McpUnit,
    subject: &str,
    class: McpClass,
    mut claim: ConformanceClaim,
    why: &str,
) -> (McpVerdict, ConformanceClaim) {
    claim.claim_id = format!("{MCP_ASK_PREFIX}{}", unit.phase);
    let verdict = McpVerdict {
        decision: "ask",
        subject: subject.to_string(),
        class,
        rule_ids: claim.policy_ids.clone(),
        obligations: claim.obligations.clone(),
        reason: Some(format!(
            "mcp: `{subject}` waits for the operator's approval ({why}; rules {})",
            claim.policy_ids.join(", ")
        )),
        remedy: Some(
            "the operator approves it in studio → MCP tools; continue with other work".to_string(),
        ),
        claim_id: claim.claim_id.clone(),
        unit: unit.unit_ref(),
    };
    (verdict, claim)
}

/// Whether the operator approved `server` (or this one tool) for first use: its token is on the
/// [`FIRST_USE_LEDGER`] rule's `excludes`. Read whatever the rule's `retired` flag says; no row
/// approves nothing.
fn first_use_approved(
    store: &dyn GraphRead,
    server: &str,
    tool: &str,
) -> Result<bool, McpCallError> {
    let Some(rule) = rule_row(store, FIRST_USE_LEDGER)? else {
        return Ok(false);
    };
    let server_token = format!("mcp:{server}");
    let tool_token = subject_of(server, tool);
    Ok(rule
        .excludes
        .iter()
        .any(|x| *x == server_token || *x == tool_token))
}

/// The store's row for rule `id`, active or retired, or `None`.
fn rule_row(
    store: &dyn GraphRead,
    id: &str,
) -> Result<Option<wicked_governance::ConformanceRule>, McpCallError> {
    use wicked_apps_core::FromNode;
    let symbol = synthetic_symbol(wicked_governance::CONFORMANCE_RULE, id);
    store
        .get_node(&symbol)
        .map_err(|e| McpCallError::GuardError(format!("rule read failed: {e}")))?
        .map(|n| {
            wicked_governance::ConformanceRule::from_node(&n)
                .map_err(|e| McpCallError::GuardError(format!("rule {id} unreadable: {e}")))
        })
        .transpose()
}

/// Refuse to judge a call on a store the `mcp-defaults` pack was never seeded into: without the
/// posture rules a balanced or ask-mode write would run unasked, so an unseeded store is a guard
/// error (fail closed), never a quieter posture. A rule the operator retired still counts as
/// present — retiring is their decision.
fn require_defaults(store: &dyn GraphRead) -> Result<(), McpCallError> {
    static IDS: OnceLock<Vec<String>> = OnceLock::new();
    let ids = IDS.get_or_init(|| {
        mcp_default_rules()
            .map(|rules| rules.into_iter().map(|r| r.id).collect())
            .unwrap_or_default()
    });
    if ids.is_empty() {
        return Err(McpCallError::GuardError(
            "the embedded mcp-defaults pack does not parse".to_string(),
        ));
    }
    for id in ids {
        if rule_row(store, id)?.is_none() {
            return Err(McpCallError::GuardError(format!(
                "the mcp-defaults posture rule {id} is not in the store (seed failed at boot)"
            )));
        }
    }
    Ok(())
}

/// A deny verdict and its advisory claim. `obligations` carries `[reason, subject, remedy]`, the
/// shape the fold reads to disclose `workerToolCallDenied` without parsing prose.
fn deny(
    unit: &McpUnit,
    subject: &str,
    class: McpClass,
    rule_ids: Vec<String>,
    reason: String,
    remedy: &str,
    evaluated_at: i64,
) -> (McpVerdict, ConformanceClaim) {
    let claim = ConformanceClaim {
        claim_id: format!("{}{}", crate::gate_hook::MCP_DENY_PREFIX, unit.phase),
        scope: unit.scope.clone(),
        phase: unit.phase.clone(),
        policy_ids: rule_ids.clone(),
        decision: Decision::Deny,
        obligations: vec![reason.clone(), subject.to_string(), remedy.to_string()],
        evaluated_context_ref: "sha256:mcp".to_string(),
        criteria: format!("mcp call (advisory: blocked, worker continues): {reason}"),
        evaluator_identity: MCP_EVALUATOR.to_string(),
        evaluated_at,
    };
    let verdict = McpVerdict {
        decision: "deny",
        subject: subject.to_string(),
        class,
        rule_ids,
        obligations: Vec::new(),
        reason: Some(reason),
        remedy: Some(remedy.to_string()),
        claim_id: claim.claim_id.clone(),
        unit: unit.unit_ref(),
    };
    (verdict, claim)
}

// ─────────────────────────────────────────────────────────────────────────────
// The capability token
// ─────────────────────────────────────────────────────────────────────────────

/// A token's slot: the unit it is bound to, tagged with the binding that bound it, or nothing.
type Slot = Option<(u64, McpUnit)>;

fn registry() -> &'static Mutex<HashMap<String, Slot>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Slot>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_registry() -> std::sync::MutexGuard<'static, HashMap<String, Slot>> {
    registry().lock().unwrap_or_else(|p| p.into_inner())
}

/// A worker process's MCP capability. Dropping it revokes the token.
#[derive(Debug)]
pub(crate) struct McpToken {
    value: String,
}

/// The unit a token is bound to, for as long as this guard lives.
#[derive(Debug)]
pub(crate) struct McpBinding {
    token: String,
    generation: u64,
}

impl McpToken {
    /// Mint a fresh token, bound to nothing yet.
    pub(crate) fn mint() -> Self {
        let value = format!("wmt_{}", uuid::Uuid::new_v4().simple());
        lock_registry().insert(value.clone(), None);
        McpToken { value }
    }

    /// The token's value, as the worker's env carries it.
    #[cfg(test)]
    pub(crate) fn value(&self) -> &str {
        &self.value
    }

    /// Bind the token to `unit` until the returned guard drops (the unit's end).
    pub(crate) fn bind(&self, unit: McpUnit) -> McpBinding {
        static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let generation = GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        lock_registry().insert(self.value.clone(), Some((generation, unit)));
        McpBinding {
            token: self.value.clone(),
            generation,
        }
    }

    /// The env a worker is handed: the token and the broker URL, or nothing when the daemon has no
    /// broker to point at (`crew_url` is the daemon's [`CREW_URL_ENV`]).
    pub(crate) fn worker_env(&self, crew_url: Option<&str>) -> Vec<(&'static str, String)> {
        match crew_url.map(str::trim).filter(|u| !u.is_empty()) {
            Some(url) => vec![
                (TOKEN_ENV, self.value.clone()),
                (CREW_URL_ENV, url.to_string()),
            ],
            None => Vec::new(),
        }
    }
}

impl Drop for McpToken {
    fn drop(&mut self) {
        lock_registry().remove(&self.value);
    }
}

impl Drop for McpBinding {
    fn drop(&mut self) {
        // Only an entry that still exists is unbound (a revoked token stays revoked), and only when
        // THIS binding still holds it: a guard outliving a newer binding never unbinds that unit.
        if let Some(slot) = lock_registry().get_mut(&self.token) {
            if slot.as_ref().is_some_and(|(g, _)| *g == self.generation) {
                *slot = None;
            }
        }
    }
}

/// The daemon's broker URL, read once per spawn.
pub(crate) fn daemon_crew_url() -> Option<String> {
    std::env::var(CREW_URL_ENV).ok()
}

/// Arm `cmd` with the MCP channel for a governed unit: mint a token, bind it to the unit, and hand
/// the worker the token and the broker URL. `None` (nothing armed) for an ungoverned unit or a
/// daemon with no broker URL. The returned guards must live until the worker process ends.
pub(crate) fn arm_worker_mcp_channel(
    cmd: &mut std::process::Command,
    input: &crate::workflow::StepInput,
) -> Option<(McpToken, McpBinding)> {
    arm_worker_mcp_channel_at(cmd, input, daemon_crew_url())
}

/// [`arm_worker_mcp_channel`] with the broker URL injected, so a test never writes the env.
fn arm_worker_mcp_channel_at(
    cmd: &mut std::process::Command,
    input: &crate::workflow::StepInput,
    crew_url: Option<String>,
) -> Option<(McpToken, McpBinding)> {
    let gov = input.governance.as_ref()?;
    let url = crew_url?;
    let token = McpToken::mint();
    let env = token.worker_env(Some(&url));
    if env.is_empty() {
        return None;
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    let binding = token.bind(McpUnit::of(
        input,
        &gov.db_path,
        McpMode::of(&gov.human_confirm),
    ));
    Some((token, binding))
}

fn resolve(token: &str) -> Option<McpUnit> {
    lock_registry()
        .get(token)
        .and_then(|slot| slot.as_ref().map(|(_, unit)| unit.clone()))
}

/// Judge and RECORD one brokered call. The claim is appended to the unit's decisions log with the
/// subject as its tool annotation; a failed append refuses the call (D-3).
pub fn evaluate_mcp_call(token: &str, call: &McpCall) -> Result<McpVerdict, McpCallError> {
    let unit = resolve(token).ok_or(McpCallError::InvalidToken)?;
    let store = open_store_ro(Some(&unit.db_path))
        .map_err(|e| McpCallError::GuardError(format!("policy store open failed: {e}")))?;
    let (verdict, claim) = evaluate(&store, &unit, call, crate::clock::eval_now())?;
    crate::gate_hook::append_annotated_claim_checked(
        &unit.decisions_path.to_string_lossy(),
        &unit.phase,
        &verdict.subject,
        &claim,
    )
    .map_err(|e| McpCallError::GuardError(format!("could not record the decision: {e}")))?;
    Ok(verdict)
}

/// The JSON face of [`evaluate_mcp_call`] for the core-ts binding: `{token, call}` in, the verdict
/// out; an error string is `<code>: <message>`.
pub fn evaluate_mcp_call_json(request_json: &str) -> Result<String, String> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Request {
        token: String,
        call: McpCall,
    }
    let req: Request = serde_json::from_str(request_json)
        .map_err(|e| McpCallError::BadRequest(format!("request: {e}")).to_string())?;
    let verdict = evaluate_mcp_call(&req.token, &req.call).map_err(|e| e.to_string())?;
    serde_json::to_string(&verdict).map_err(|e| format!("guard_error: {e}"))
}

// ─────────────────────────────────────────────────────────────────────────────
// The mcp-defaults policy pack
// ─────────────────────────────────────────────────────────────────────────────

/// The pack's effect-bearing posture rules (P-1/P-2 and the mode rules), parsed from the embedded
/// pack file.
pub(crate) fn mcp_default_rules() -> anyhow::Result<Vec<wicked_governance::ConformanceRule>> {
    let doc: Value = serde_json::from_str(MCP_DEFAULTS_RULES)?;
    wicked_governance::normalize_bundle(&doc, "filesystem")
}

/// Seed the posture rules into the store, INSERT-ONLY: a rule already present (edited by an
/// approval, or retired by the operator) is left exactly as it is, so a restart never undoes an
/// approval or resurrects a retired rule. Returns how many rules were inserted.
///
/// Written straight to the store in one batch, NOT through `register_rule`: that path also emits
/// `wicked.estate.rule.ingested` on the bus, synchronously, and this runs on the actor's boot path,
/// which must never wait on a busy or locked bus (`tests/bus_handoff.rs`).
pub(crate) fn seed_mcp_defaults(
    store: &mut dyn wicked_apps_core::GraphStore,
) -> anyhow::Result<usize> {
    use wicked_apps_core::ToNode;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut nodes = Vec::new();
    for mut rule in mcp_default_rules()? {
        let symbol = synthetic_symbol(wicked_governance::CONFORMANCE_RULE, &rule.id);
        if store.get_node(&symbol)?.is_some() {
            continue;
        }
        rule.validate()?;
        rule.created_at = Some(now);
        nodes.push(rule.to_node());
    }
    if nodes.is_empty() {
        return Ok(0);
    }
    store.begin_batch()?;
    store.upsert_nodes(&nodes)?;
    store.commit_batch()?;
    Ok(nodes.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wicked_apps_core::open_store;

    fn unit(posture: WritePosture, role: PhaseRole, seat: &str, mode: McpMode) -> McpUnit {
        McpUnit {
            run_id: "mcp-test".to_string(),
            attempt: 0,
            ord: 2,
            scope: "wicked-agent/mcp-test/shared".to_string(),
            phase: "unit-2".to_string(),
            phase_id: "review".to_string(),
            catalog: String::new(),
            role,
            posture,
            seat: seat.to_string(),
            mode,
            decisions_path: PathBuf::from("/nonexistent"),
            db_path: String::new(),
        }
    }

    fn call(tool: &str, annotations: Option<McpAnnotations>) -> McpCall {
        McpCall {
            server: "jira".to_string(),
            tool: tool.to_string(),
            args: serde_json::json!({"summary": "x"}),
            annotations,
            class_override: None,
            registered: true,
            kind: default_kind(),
            carrier: default_carrier(),
        }
    }

    fn read_only() -> Option<McpAnnotations> {
        Some(McpAnnotations {
            read_only_hint: Some(true),
            ..Default::default()
        })
    }

    fn seeded_store() -> wicked_apps_core::SqliteStore {
        let mut store = open_store(Some(":memory:")).unwrap();
        assert_eq!(
            seed_mcp_defaults(&mut store).unwrap(),
            mcp_default_rules().unwrap().len()
        );
        store
    }

    fn rule(json: Value) -> wicked_governance::ConformanceRule {
        let rule: wicked_governance::ConformanceRule = serde_json::from_value(json).unwrap();
        rule.validate().unwrap();
        rule
    }

    /// Approve the server's first use: its token joins MCP-FIRST-USE's `excludes` (what crew's
    /// approval route does through the audited rule upsert).
    fn approve(store: &mut dyn wicked_apps_core::GraphStore, rule_id: &str, token: &str) {
        let mut r = mcp_default_rules()
            .unwrap()
            .into_iter()
            .find(|r| r.id == rule_id)
            .unwrap();
        r.excludes.push(token.to_string());
        wicked_governance::register_rule(store, &r).unwrap();
    }

    const SEATS: [&str; 6] = ["claude", "codex", "opencode", "copilot", "pi", "agy"];

    #[test]
    fn classification_treats_an_unannotated_tool_as_write() {
        assert_eq!(classify(None, None), McpClass::Write);
        assert_eq!(
            classify(Some(&McpAnnotations::default()), None),
            McpClass::Write
        );
        assert_eq!(classify(read_only().as_ref(), None), McpClass::Read);
        let destructive = McpAnnotations {
            read_only_hint: Some(false),
            ..Default::default()
        };
        assert_eq!(classify(Some(&destructive), None), McpClass::Destructive);
        let write = McpAnnotations {
            destructive_hint: Some(false),
            ..Default::default()
        };
        assert_eq!(classify(Some(&write), None), McpClass::Write);
        assert_eq!(
            classify(read_only().as_ref(), Some(McpClass::Write)),
            McpClass::Write
        );
    }

    /// PROVING TEST (S1 row 1): a write tool in a read-only unit is denied on EVERY seat's token,
    /// in every mode, by the engine gate — even with the server approved and a policy that allows it.
    #[test]
    fn a_write_tool_in_a_read_only_unit_is_denied_on_every_seat_in_every_mode() {
        let mut store = seeded_store();
        approve(&mut store, "MCP-FIRST-USE", "mcp:jira");
        approve(&mut store, "MCP-POSTURE-WRITE", "mcp:jira/create_issue");
        wicked_governance::register_rule(
            &mut store,
            &rule(serde_json::json!({
                "id": "OPS-MCP-ALLOW-ALL", "rule_type": "policy", "statement": "allow jira",
                "severity": "info", "confidence": 1.0, "steering_type": "security",
                "applies_to": ["mcp:jira"], "effect": "allow",
                "provenance": {"source": "ui", "ref": "test", "source_kinds": ["doc"]}
            })),
        )
        .unwrap();
        for seat in SEATS {
            for mode in [McpMode::Ask, McpMode::Balanced, McpMode::Autonomous] {
                for (posture, role) in [
                    (WritePosture::ReadOnly, PhaseRole::Evaluator),
                    (WritePosture::ReadOnly, PhaseRole::Neutral),
                    (WritePosture::Full, PhaseRole::Evaluator),
                ] {
                    let u = unit(posture, role, seat, mode);
                    for annotations in [
                        None,
                        Some(McpAnnotations {
                            destructive_hint: Some(true),
                            ..Default::default()
                        }),
                    ] {
                        let (v, claim) =
                            evaluate(&store, &u, &call("create_issue", annotations), 1).unwrap();
                        assert_eq!(v.decision, "deny", "{seat} {mode:?} {posture:?}: {v:?}");
                        assert_eq!(v.rule_ids, vec![RULE_PHASE_ROLE.to_string()]);
                        assert!(claim
                            .claim_id
                            .starts_with(crate::gate_hook::MCP_DENY_PREFIX));
                        assert_eq!(claim.evaluator_identity, MCP_EVALUATOR);
                        assert_eq!(claim.obligations[1], "mcp:jira/create_issue");
                    }
                    // A READ tool of the same approved server still runs for that unit.
                    let (v, _) = evaluate(&store, &u, &call("get_issue", read_only()), 1).unwrap();
                    assert_eq!(v.decision, "allow", "{seat} {mode:?}: a read runs: {v:?}");
                }
            }
        }
    }

    /// PROVING TEST (S1 row 2): a fired `deny` rule wins over an approval.
    #[test]
    fn a_fired_deny_rule_wins_over_an_approval() {
        let mut store = seeded_store();
        approve(&mut store, "MCP-FIRST-USE", "mcp:jira");
        approve(&mut store, "MCP-POSTURE-WRITE", "mcp:jira/delete_project");
        let creator = unit(
            WritePosture::Full,
            PhaseRole::Creator,
            "codex",
            McpMode::Balanced,
        );
        let (v, _) = evaluate(&store, &creator, &call("delete_project", None), 1).unwrap();
        assert_eq!(v.decision, "allow", "approved and no deny yet: {v:?}");
        wicked_governance::register_rule(
            &mut store,
            &rule(serde_json::json!({
                "id": "SEC-MCP-NO-DELETE", "rule_type": "policy",
                "statement": "never delete a jira project", "severity": "critical",
                "confidence": 1.0, "steering_type": "security",
                "applies_to": ["mcp:jira/delete_project"], "effect": "deny",
                "provenance": {"source": "ui", "ref": "test", "source_kinds": ["doc"]}
            })),
        )
        .unwrap();
        for mode in [McpMode::Ask, McpMode::Balanced, McpMode::Autonomous] {
            let creator = unit(WritePosture::Full, PhaseRole::Creator, "codex", mode);
            let (v, claim) = evaluate(&store, &creator, &call("delete_project", None), 1).unwrap();
            assert_eq!(v.decision, "deny", "{mode:?}: {v:?}");
            assert_eq!(v.rule_ids, vec!["SEC-MCP-NO-DELETE".to_string()]);
            assert_eq!(claim.decision, Decision::Deny);
            assert!(
                crate::gate_hook::is_advisory_deny(&claim),
                "an MCP deny is advisory"
            );
        }
    }

    /// PROVING TEST (S1 row 3) under the operator's final decisions: first use of a server ASKS in
    /// every mode; after approval the mode decides; an ask is `allow_with_conditions` carrying
    /// `mcp:approval`, never a deny.
    #[test]
    fn first_use_asks_in_every_mode_then_the_mode_decides() {
        let mut store = seeded_store();
        let modes = [McpMode::Ask, McpMode::Balanced, McpMode::Autonomous];
        for mode in modes {
            let creator = unit(WritePosture::Full, PhaseRole::Creator, "claude", mode);
            for c in [call("get_issue", read_only()), call("create_issue", None)] {
                let (v, claim) = evaluate(&store, &creator, &c, 1).unwrap();
                assert_eq!(v.decision, "ask", "{mode:?} first use: {v:?}");
                assert_eq!(claim.decision, Decision::AllowWithConditions);
                assert!(claim.obligations.contains(&APPROVAL_OBLIGATION.to_string()));
                assert_eq!(v.rule_ids[0], RULE_FIRST_USE, "{v:?}");
            }
        }
        approve(&mut store, "MCP-FIRST-USE", "mcp:jira");
        fn expect(store: &wicked_apps_core::SqliteStore, mode: McpMode, c: McpCall) -> McpVerdict {
            let creator = unit(WritePosture::Full, PhaseRole::Creator, "claude", mode);
            evaluate(store, &creator, &c, 1).unwrap().0
        }
        // Reads run in every mode once the server is approved.
        for mode in modes {
            assert_eq!(
                expect(&store, mode, call("get_issue", read_only())).decision,
                "allow",
                "{mode:?}"
            );
        }
        // Writes: ask asks, balanced asks (P-2), autonomous runs.
        let ask = expect(&store, McpMode::Ask, call("create_issue", None));
        assert_eq!(
            (ask.decision, ask.rule_ids.clone()),
            ("ask", vec!["MCP-MODE-ASK-WRITE".to_string()])
        );
        let balanced = expect(&store, McpMode::Balanced, call("create_issue", None));
        assert_eq!(balanced.decision, "ask");
        assert!(balanced
            .obligations
            .contains(&APPROVAL_OBLIGATION.to_string()));
        assert_eq!(balanced.rule_ids, vec!["MCP-POSTURE-WRITE".to_string()]);
        assert_eq!(
            expect(&store, McpMode::Autonomous, call("create_issue", None)).decision,
            "allow"
        );
        // Approving the tool lifts balanced's ask, but never ask mode's per-call ask.
        approve(&mut store, "MCP-POSTURE-WRITE", "mcp:jira/create_issue");
        assert_eq!(
            expect(&store, McpMode::Balanced, call("create_issue", None)).decision,
            "allow"
        );
        assert_eq!(
            expect(&store, McpMode::Ask, call("create_issue", None)).decision,
            "ask"
        );
        // Another server is still on first use.
        let mut other = call("get_issue", read_only());
        other.server = "sentry".to_string();
        assert_eq!(expect(&store, McpMode::Autonomous, other).decision, "ask");
    }

    #[test]
    fn retiring_the_first_use_ledger_never_lifts_the_ask_and_no_ledger_approves_nothing() {
        let creator = unit(
            WritePosture::Full,
            PhaseRole::Creator,
            "codex",
            McpMode::Autonomous,
        );
        // An unseeded store is refused, never judged by a quieter posture; the engine gates still
        // deny on it.
        let empty = open_store(Some(":memory:")).unwrap();
        assert!(matches!(
            evaluate(&empty, &creator, &call("get_issue", read_only()), 1),
            Err(McpCallError::GuardError(_))
        ));
        let evaluator = unit(
            WritePosture::ReadOnly,
            PhaseRole::Evaluator,
            "codex",
            McpMode::Ask,
        );
        let (v, _) = evaluate(&empty, &evaluator, &call("create_issue", None), 1).unwrap();
        assert_eq!(
            (v.decision, v.rule_ids[0].as_str()),
            ("deny", RULE_PHASE_ROLE)
        );
        let mut store = seeded_store();
        let mut ledger = mcp_default_rules()
            .unwrap()
            .into_iter()
            .find(|r| r.id == FIRST_USE_LEDGER)
            .unwrap();
        ledger.retired = true;
        wicked_governance::register_rule(&mut store, &ledger).unwrap();
        let (v, _) = evaluate(&store, &creator, &call("get_issue", read_only()), 1).unwrap();
        assert_eq!(v.decision, "ask", "retired, nothing approved: {v:?}");
        ledger.excludes.push("mcp:jira".to_string());
        wicked_governance::register_rule(&mut store, &ledger).unwrap();
        let (v, _) = evaluate(&store, &creator, &call("get_issue", read_only()), 1).unwrap();
        assert_eq!(
            v.decision, "allow",
            "a retired ledger still carries its approvals: {v:?}"
        );
        // A single-tool approval approves that tool only.
        let mut other = call("get_issue", read_only());
        other.server = "sentry".to_string();
        ledger.excludes = vec!["mcp:sentry/get_issue".to_string()];
        wicked_governance::register_rule(&mut store, &ledger).unwrap();
        assert_eq!(
            evaluate(&store, &creator, &other, 1).unwrap().0.decision,
            "allow"
        );
        other.tool = "get_event".to_string();
        assert_eq!(
            evaluate(&store, &creator, &other, 1).unwrap().0.decision,
            "ask"
        );
    }

    /// The posture triggers read the broker's `mcp.class`, not any `"class"` an argument carries:
    /// a read tool whose args say `"class":"write"` still runs, and a write tool whose args say
    /// `"class":"read"` still asks.
    #[test]
    fn an_argument_named_class_never_moves_the_posture() {
        let mut store = seeded_store();
        approve(&mut store, FIRST_USE_LEDGER, "mcp:jira");
        let creator = unit(
            WritePosture::Full,
            PhaseRole::Creator,
            "claude",
            McpMode::Balanced,
        );
        let mut read = call("get_issue", read_only());
        read.args = serde_json::json!({"class": "write"});
        assert_eq!(
            evaluate(&store, &creator, &read, 1).unwrap().0.decision,
            "allow"
        );
        let mut write = call("create_issue", None);
        write.args = serde_json::json!({"class": "read"});
        let (v, _) = evaluate(&store, &creator, &write, 1).unwrap();
        assert_eq!(
            (v.decision, v.rule_ids),
            ("ask", vec!["MCP-POSTURE-WRITE".to_string()])
        );
    }

    #[test]
    fn an_unregistered_tool_is_denied_before_any_policy() {
        let store = seeded_store();
        let mut c = call("get_issue", read_only());
        c.registered = false;
        let creator = unit(
            WritePosture::Full,
            PhaseRole::Creator,
            "pi",
            McpMode::Autonomous,
        );
        let (v, _) = evaluate(&store, &creator, &c, 1).unwrap();
        assert_eq!(
            (v.decision, v.rule_ids),
            ("deny", vec![RULE_UNREGISTERED.to_string()])
        );
    }

    #[test]
    fn a_name_that_could_forge_a_subject_is_refused() {
        let store = seeded_store();
        let creator = unit(
            WritePosture::Full,
            PhaseRole::Creator,
            "pi",
            McpMode::Autonomous,
        );
        for (server, tool) in [
            ("jira/x", "t"),
            ("jira", "a/b"),
            ("", "t"),
            ("jira", "t y"),
            ("j:x", "t"),
        ] {
            let mut c = call(tool, None);
            c.server = server.to_string();
            assert!(matches!(
                evaluate(&store, &creator, &c, 1),
                Err(McpCallError::BadRequest(_))
            ));
        }
    }

    #[test]
    fn the_seed_is_insert_only_and_never_undoes_an_approval_or_a_retirement() {
        let mut store = seeded_store();
        approve(&mut store, "MCP-FIRST-USE", "mcp:jira");
        let mut retired = mcp_default_rules()
            .unwrap()
            .into_iter()
            .find(|r| r.id == "MCP-MODE-ASK-WRITE")
            .unwrap();
        retired.retired = true;
        wicked_governance::register_rule(&mut store, &retired).unwrap();
        assert_eq!(
            seed_mcp_defaults(&mut store).unwrap(),
            0,
            "a re-seed inserts nothing"
        );
        let creator = unit(
            WritePosture::Full,
            PhaseRole::Creator,
            "claude",
            McpMode::Ask,
        );
        let (v, _) = evaluate(&store, &creator, &call("create_issue", None), 1).unwrap();
        assert_eq!(
            v.decision, "allow",
            "approval kept, retired ask rule stays retired: {v:?}"
        );
    }

    #[test]
    fn a_token_resolves_only_while_bound_and_dies_with_its_guard() {
        let token = McpToken::mint();
        let value = token.value.clone();
        let c = call("get_issue", read_only());
        assert_eq!(
            evaluate_mcp_call(&value, &c),
            Err(McpCallError::InvalidToken),
            "unbound"
        );
        {
            let _b = token.bind(unit(
                WritePosture::Full,
                PhaseRole::Creator,
                "codex",
                McpMode::Autonomous,
            ));
            assert!(resolve(&value).is_some());
        }
        assert!(resolve(&value).is_none(), "unbound at unit end");
        // A guard that outlives a newer binding never unbinds the newer unit.
        let stale = token.bind(unit(
            WritePosture::Full,
            PhaseRole::Creator,
            "codex",
            McpMode::Ask,
        ));
        let fresh = token.bind(unit(
            WritePosture::ReadOnly,
            PhaseRole::Evaluator,
            "codex",
            McpMode::Ask,
        ));
        drop(stale);
        assert_eq!(
            resolve(&value).map(|u| u.posture),
            Some(WritePosture::ReadOnly)
        );
        drop(fresh);
        assert!(resolve(&value).is_none());
        drop(token);
        assert!(
            !lock_registry().contains_key(&value),
            "revoked with the process"
        );
        assert_eq!(
            evaluate_mcp_call("wmt_forged", &c),
            Err(McpCallError::InvalidToken)
        );
    }

    #[test]
    fn the_worker_env_carries_the_token_and_url_only_when_there_is_a_broker() {
        let token = McpToken::mint();
        assert!(token.worker_env(None).is_empty());
        assert!(token.worker_env(Some("  ")).is_empty());
        let env = token.worker_env(Some("http://127.0.0.1:7701"));
        assert_eq!(env[0], (TOKEN_ENV, token.value.clone()));
        assert_eq!(env[1], (CREW_URL_ENV, "http://127.0.0.1:7701".to_string()));
    }

    /// End to end through the token: the decision is recorded in the unit's decisions log with the
    /// subject as its tool, the deny is advisory in the fold, and an unwritable log refuses the
    /// call (D-3).
    #[test]
    fn a_recorded_call_lands_in_the_decisions_log_and_an_unrecordable_one_is_refused() {
        let tid = format!("{:?}", std::thread::current().id()).replace(['(', ')'], "");
        let base =
            std::env::temp_dir().join(format!("wicked-mcp-gate-{}-{tid}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let db = base.join("policy.db");
        {
            let mut store = open_store(Some(&db.to_string_lossy())).unwrap();
            seed_mcp_defaults(&mut store).unwrap();
        }
        let mut u = unit(
            WritePosture::ReadOnly,
            PhaseRole::Evaluator,
            "claude",
            McpMode::Balanced,
        );
        u.db_path = db.to_string_lossy().into_owned();
        u.decisions_path = base.join("attempt-0").join("decisions.ndjson");
        let token = McpToken::mint();
        let binding = token.bind(u.clone());
        let v = evaluate_mcp_call(&token.value, &call("create_issue", None)).unwrap();
        assert_eq!(v.decision, "deny");
        let log = std::fs::read_to_string(&u.decisions_path).unwrap();
        assert!(
            log.contains(r#""_wicked_tool_call":"mcp:jira/create_issue""#),
            "{log}"
        );
        let claim: ConformanceClaim = serde_json::from_str(log.lines().last().unwrap()).unwrap();
        assert!(crate::gate_hook::is_advisory_deny(&claim));
        drop(binding);

        // D-3: the log's directory is a FILE, so the append fails and the call is refused.
        let blocked = base.join("blocked");
        std::fs::write(&blocked, b"not a dir").unwrap();
        u.decisions_path = blocked.join("decisions.ndjson");
        let _b = token.bind(u);
        assert!(matches!(
            evaluate_mcp_call(&token.value, &call("get_issue", read_only())),
            Err(McpCallError::GuardError(_))
        ));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// PROVING TEST (S1 row 4): the fold treats an `mcp-deny` as advisory — the unit is not
    /// denied, the refusal is disclosed with its subject and remedy — and an `ask` never resolves
    /// the phase gate (it is not the unit's verdict), while a co-occurring policy deny still does.
    #[test]
    fn the_fold_treats_an_mcp_deny_as_advisory_and_an_ask_never_gates_the_phase() {
        use crate::gate_hook::{
            apply_hook_decisions, collect_hook_decisions, decisions_path_for, fold_input_denial,
            gov_run_dir, write_armed_marker_for, CARRIER_ACP,
        };
        let tid = format!("{:?}", std::thread::current().id()).replace(['(', ')'], "");
        let run_id = format!("mcp-fold-{}-{tid}", std::process::id());
        let _ = std::fs::remove_dir_all(gov_run_dir(&run_id));
        let dpath = decisions_path_for(&run_id, 0);
        write_armed_marker_for(&dpath, "unit-2", Some(CARRIER_ACP)).unwrap();
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&dpath)
                .unwrap();
            f.write_all(b"{\"_wicked_hook_fired\":\"unit-2\"}\n")
                .unwrap();
        }
        let db = gov_run_dir(&run_id).join("policy.db");
        {
            let mut store = open_store(Some(&db.to_string_lossy())).unwrap();
            seed_mcp_defaults(&mut store).unwrap();
        }
        let mut u = unit(
            WritePosture::ReadOnly,
            PhaseRole::Evaluator,
            "codex",
            McpMode::Balanced,
        );
        u.run_id = run_id.clone();
        u.db_path = db.to_string_lossy().into_owned();
        u.decisions_path = dpath.clone();
        let token = McpToken::mint();
        let _b = token.bind(u);
        assert_eq!(
            evaluate_mcp_call(&token.value, &call("create_issue", None))
                .unwrap()
                .decision,
            "deny"
        );
        assert_eq!(
            evaluate_mcp_call(&token.value, &call("get_issue", read_only()))
                .unwrap()
                .decision,
            "ask"
        );

        let mut store = open_store(Some(":memory:")).unwrap();
        assert_eq!(
            fold_input_denial(&mut store, &run_id, 0, "unit-2", true).unwrap(),
            None,
            "an MCP deny and an ask do not fail the unit"
        );
        let refusals: Vec<_> = collect_hook_decisions(&run_id, 0, "unit-2")
            .into_iter()
            .filter_map(|r| r.mcp_refusal().map(|x| (r.tool_name.clone(), x)))
            .collect();
        assert_eq!(refusals.len(), 1, "{refusals:?}");
        let (tool, (reason, subject, remedy)) = &refusals[0];
        assert_eq!(
            (tool.as_str(), subject.as_str()),
            ("mcp:jira/create_issue", "mcp:jira/create_issue")
        );
        assert!(
            reason.contains(RULE_PHASE_ROLE) || reason.contains("D-1"),
            "{reason}"
        );
        assert!(!remedy.is_empty());

        // The drain: the MCP claims resolve no phase gate (an ask must not turn it conditional).
        let summary = apply_hook_decisions(&mut store, &run_id, &dpath).unwrap();
        assert_eq!(summary.denied, 0);
        let phase = wicked_orchestration::get_phase(&store, &format!("wf-{run_id}:unit-2"))
            .unwrap()
            .expect("the drain opened the phase");
        assert_eq!(
            phase.status,
            wicked_orchestration::PhaseStatus::GateRunning,
            "no MCP claim resolves the phase gate"
        );

        // A co-occurring non-MCP policy deny is still fatal: the MCP class masks nothing.
        let mut policy_deny = ConformanceClaim {
            claim_id: "policy-x".to_string(),
            scope: "s".to_string(),
            phase: "unit-2".to_string(),
            policy_ids: vec!["SEC-X".to_string()],
            decision: Decision::Deny,
            obligations: vec![],
            evaluated_context_ref: "sha256:x".to_string(),
            criteria: String::new(),
            evaluator_identity: "wicked-governance".to_string(),
            evaluated_at: 1,
        };
        crate::gate_hook::append_annotated_claim_checked(
            &dpath.to_string_lossy(),
            "unit-2",
            "Bash",
            &policy_deny,
        )
        .unwrap();
        assert!(fold_input_denial(&mut store, &run_id, 0, "unit-2", true)
            .unwrap()
            .is_some());
        // …and an MCP-prefixed deny from another evaluator is neither advisory nor disclosed as a
        // brokered refusal.
        policy_deny.claim_id = format!("{}unit-2", crate::gate_hook::MCP_DENY_PREFIX);
        policy_deny.obligations = vec!["r".into(), "mcp:jira/x".into(), "m".into()];
        assert!(!crate::gate_hook::is_advisory_deny(&policy_deny));
        crate::gate_hook::append_annotated_claim_checked(
            &dpath.to_string_lossy(),
            "unit-2",
            "mcp:jira/x",
            &policy_deny,
        )
        .unwrap();
        let brokered = collect_hook_decisions(&run_id, 0, "unit-2")
            .into_iter()
            .filter(|r| r.mcp_refusal().is_some())
            .count();
        assert_eq!(
            brokered, 1,
            "only the broker's own refusal is disclosed as one"
        );
        let _ = std::fs::remove_dir_all(gov_run_dir(&run_id));
    }

    /// The wrapped carrier's arming: a governed unit's worker gets the token and the broker URL,
    /// the token resolves to THAT unit (posture, seat and mode read off the unit and the run) until
    /// the guards drop; an ungoverned unit or a daemon with no broker gets nothing.
    #[test]
    fn a_governed_worker_is_armed_with_a_token_bound_to_its_own_unit() {
        let mut u = crate::domain::WorkUnit::pending("s:u3", "s", 3, "review the change");
        u.role = PhaseRole::Evaluator;
        u.executes_code = false;
        u.worktree_guarded = true;
        u.assigned_cli = Some("pi".to_string());
        let mut input = crate::workflow::StepInput {
            run_id: "run-mcp-arm".to_string(),
            unit_ix: 0,
            attempt: 1,
            unit: u,
            workflow_id: "wf-x".to_string(),
            entity_mode: crate::scope::EntityMode::Shared,
            workdir: Some(std::env::temp_dir()),
            governance: Some(crate::workflow::GovernanceContext {
                db_path: "/abs/estate.db".to_string(),
                code_graph_db: None,
                extra_write_roots: Vec::new(),
                extra_read_roots: Vec::new(),
                project_id: None,
                human_confirm: HumanConfirm::All,
            }),
            prior_outputs: vec![],
            elicitation_epoch: 0,
            process_gen: None,
            launch_seq: 0,
            required_skills: Vec::new(),
        };
        // spawn-audit: test-only — never spawned; the test reads the env the carrier armed.
        let mut cmd = std::process::Command::new("true");
        let url = Some("http://127.0.0.1:60785".to_string());
        let (token, binding) =
            arm_worker_mcp_channel_at(&mut cmd, &input, url.clone()).expect("armed");
        let env: HashMap<String, String> = cmd
            .get_envs()
            .filter_map(|(k, v)| Some((k.to_str()?.to_string(), v?.to_str()?.to_string())))
            .collect();
        assert_eq!(env.get(TOKEN_ENV).map(String::as_str), Some(token.value()));
        assert_eq!(env.get(CREW_URL_ENV), url.as_ref());
        let bound = resolve(token.value()).expect("bound to the unit");
        assert_eq!(
            (bound.phase.as_str(), bound.attempt, bound.seat.as_str()),
            ("unit-3", 1, "pi")
        );
        assert_eq!(bound.posture, WritePosture::ReadOnly);
        assert_eq!(bound.mode, McpMode::Ask);
        assert_eq!(
            bound.decisions_path,
            crate::gate_hook::decisions_path_for("run-mcp-arm", 1)
        );
        let value = token.value().to_string();
        drop(binding);
        drop(token);
        assert!(resolve(&value).is_none(), "dead at unit end");

        assert!(
            // spawn-audit: test-only — never spawned; the test reads the env the carrier armed.
            arm_worker_mcp_channel_at(&mut std::process::Command::new("true"), &input, None)
                .is_none()
        );
        input.governance = None;
        // spawn-audit: test-only — never spawned; the test reads the env the carrier armed.
        let mut bare = std::process::Command::new("true");
        assert!(arm_worker_mcp_channel_at(&mut bare, &input, url).is_none());
        assert_eq!(
            bare.get_envs().count(),
            0,
            "an ungoverned worker gets no MCP env"
        );
    }

    /// The binding's wire shape is camelCase both ways (the broker is JS): `classOverride` is
    /// read, an unknown key is refused, and the verdict names `ruleIds` / `claimId` / `unit.runId`.
    #[test]
    fn the_wire_shape_is_camel_case_both_ways() {
        let c: McpCall = serde_json::from_value(serde_json::json!({
            "server": "jira", "tool": "t", "registered": true,
            "annotations": {"readOnlyHint": true}, "classOverride": "destructive"
        }))
        .unwrap();
        assert_eq!(
            classify(c.annotations.as_ref(), c.class_override),
            McpClass::Destructive
        );
        assert!(serde_json::from_value::<McpCall>(serde_json::json!({
            "server": "jira", "tool": "t", "registered": true, "class_override": "read"
        }))
        .is_err());
        let store = seeded_store();
        let u = unit(
            WritePosture::Full,
            PhaseRole::Creator,
            "claude",
            McpMode::Autonomous,
        );
        let (v, _) = evaluate(&store, &u, &c, 1).unwrap();
        let wire = serde_json::to_value(&v).unwrap();
        for key in [
            "decision",
            "subject",
            "class",
            "ruleIds",
            "obligations",
            "claimId",
            "unit",
        ] {
            assert!(wire.get(key).is_some(), "{key}: {wire}");
        }
        assert_eq!(wire["unit"]["runId"], "mcp-test");
        assert_eq!(wire["class"], "destructive");
    }

    #[test]
    fn the_json_face_reports_error_codes() {
        let err = evaluate_mcp_call_json("{}").unwrap_err();
        assert!(err.starts_with("bad_request:"), "{err}");
        let err = evaluate_mcp_call_json(
            r#"{"token":"wmt_nope","call":{"server":"jira","tool":"t","registered":true}}"#,
        )
        .unwrap_err();
        assert!(err.starts_with("invalid_token"), "{err}");
    }

    #[test]
    fn the_mode_follows_the_run_level_autonomy() {
        assert_eq!(McpMode::of(&HumanConfirm::All), McpMode::Ask);
        assert_eq!(McpMode::of(&HumanConfirm::Before(1)), McpMode::Balanced);
        assert_eq!(McpMode::of(&HumanConfirm::None), McpMode::Autonomous);
    }
}
