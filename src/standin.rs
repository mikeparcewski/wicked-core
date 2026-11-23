//! (DES-STANDIN-001 §3) Stand-in review: one mechanism for a run whose roster is short of the
//! independent seats its contract asks for.
//!
//! A stand-in is a BINDING, never a roster seat (§3.3). It is minted only here, after consent and
//! the role's own admission, as `<base>~<slot>`: `base` is the credential instance it runs on
//! (`claude`, `codex#2`), so every identity fence reads it as its base, and the binding key is the
//! unit's `assigned_cli` for events and attribution only. Bindings live on the run's assurance
//! contract (`RunAssurance::stand_ins`) and nowhere else: no roster, candidate set or failover
//! ladder ever holds one.
//!
//! Everything here is inert unless the launch carried `standInAware` (§8, "activation is separate
//! from support"): every caller checks [`active`] first, and an unaware run takes today's path
//! byte for byte.

use serde::{Deserialize, Serialize};
use wicked_apps_core::spawn::seat_id;

/// (§3.1) The independent roles a stand-in can fill. Wire: snake_case tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Evaluator,
    Judge,
    RuleJudge,
    QeReviewer,
    Council,
    Monitor,
    PartyExcluded,
}

impl Role {
    /// The wire token.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Evaluator => "evaluator",
            Role::Judge => "judge",
            Role::RuleJudge => "rule_judge",
            Role::QeReviewer => "qe_reviewer",
            Role::Council => "council",
            Role::Monitor => "monitor",
            Role::PartyExcluded => "party_excluded",
        }
    }

    /// The role in a person's words.
    pub fn words(self) -> &'static str {
        match self {
            Role::Evaluator => "the review",
            Role::Judge => "the judge",
            Role::RuleJudge => "the rule check",
            Role::QeReviewer => "the QE review",
            Role::Council => "the council",
            Role::Monitor => "the monitors",
            Role::PartyExcluded => "the review",
        }
    }
}

/// Consent scopes (§3.8).
pub const SCOPE_JOB: &str = "job";
pub const SCOPE_ALWAYS: &str = "always";
/// Where a consent came from (§3.8).
pub const VIA: [&str; 5] = ["proposal", "launch", "setting", "schedule", "grant"];

/// (§3.8) A human's consent to stand-ins. `by` and `at` are set by the launcher's server from an
/// authenticated human actor, never taken from a request body; the engine records them as given.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StandInConsent {
    /// `job` | `always`.
    pub scope: String,
    pub by: String,
    pub at: i64,
    /// `proposal` | `launch` | `setting` | `schedule` | `grant`.
    pub via: String,
}

impl StandInConsent {
    /// Refuses a consent the launcher could not have built: an unknown scope or source, or no
    /// actor.
    pub fn validate(&self) -> Result<(), String> {
        if self.scope != SCOPE_JOB && self.scope != SCOPE_ALWAYS {
            return Err(format!(
                "standIn.scope must be `{SCOPE_JOB}` or `{SCOPE_ALWAYS}`, not `{}`",
                self.scope
            ));
        }
        if !VIA.contains(&self.via.as_str()) {
            return Err(format!(
                "standIn.via must be one of {}, not `{}`",
                VIA.join(", "),
                self.via
            ));
        }
        if self.by.trim().is_empty() {
            return Err("standIn.by names no one: consent is a person's".to_string());
        }
        Ok(())
    }
}

/// (§3.8) The launch's stand-in facts, as the launcher sent them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchStandIn {
    /// `standInAware`: the activation switch. Only a launcher that implements the choice sends
    /// it; `false` keeps today's behaviour, reduced assurance included.
    pub aware: bool,
    /// Seats that authored the work under review (review-change) or ran the work in the window
    /// (insights).
    pub parties: Vec<String>,
    /// A human's consent, built by the launcher's server.
    pub consent: Option<StandInConsent>,
}

impl LaunchStandIn {
    /// (§3.8, rev 3 N4) Refuse a launch whose stand-in facts cannot hold together: stand-ins and
    /// reduced assurance are mutually exclusive (with or without consent), and consent or parties
    /// on a launch that is not `standInAware` would be silently dropped, so they are refused.
    pub fn validate(&self, reduced_assurance: bool) -> Result<(), String> {
        if self.aware && reduced_assurance {
            return Err(
                "a standInAware launch cannot also be reduced-assurance: stand-ins replace the \
                 creator-seat waiver (DES-STANDIN-001 §3.8)"
                    .to_string(),
            );
        }
        if !self.aware && (self.consent.is_some() || !self.parties.is_empty()) {
            return Err(
                "standIn and parties are honoured only on a standInAware launch".to_string(),
            );
        }
        if let Some(c) = &self.consent {
            c.validate()?;
        }
        if let Some(p) = self.parties.iter().find(|p| seat_id::is_stand_in(p)) {
            return Err(format!(
                "party `{p}` is a stand-in key: parties are roster seats"
            ));
        }
        Ok(())
    }

    /// Persist the facts on the run's contract (§3.3, rev 3 N3): every later fill, grant, replay
    /// and fence reads them from there.
    pub(crate) fn apply(
        &self,
        mut run: crate::assurance::RunAssurance,
    ) -> crate::assurance::RunAssurance {
        run.stand_in_aware = self.aware;
        if self.aware {
            let mut parties: Vec<String> = Vec::new();
            for p in &self.parties {
                let p = p.trim().to_string();
                if !p.is_empty() && !parties.contains(&p) {
                    parties.push(p);
                }
            }
            run.parties = parties;
            run.stand_in = self.consent.clone();
            run.roles = Some(Vec::new());
        }
        run
    }
}

/// (§3.3, T-R14) A launch roster (`clisJson`, before it is typed) that carries a stand-in: a
/// record with a `standIn` field, or a key in the reserved grammar. Bindings are minted only by
/// the engine, after consent; a roster can never hand one in.
pub fn refuse_forged_stand_ins(raw: &[serde_json::Value]) -> Result<(), String> {
    for rec in raw {
        let key = rec.get("key").and_then(|k| k.as_str()).unwrap_or("");
        if rec.get("standIn").is_some() || rec.get("stand_in").is_some() {
            return Err(format!(
                "roster seat `{key}` carries a standIn field: stand-ins are minted by the engine, \
                 never handed in"
            ));
        }
        if seat_id::is_stand_in(key) {
            return Err(format!(
                "roster seat `{key}` uses the reserved stand-in separator `{}`: rename the seat",
                wicked_apps_core::spawn::STAND_IN_SEP
            ));
        }
    }
    Ok(())
}

/// (§3.7) Why a role was stood in, as data: built from facts, never parsed from words. The first
/// three kinds are also the preflight's reason ([`crate::independence::ShortReason`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reason {
    OnlyOneHelper {
        cli: String,
    },
    AllParties {
        seats: Vec<String>,
    },
    HelpersShort {
        signed_in: usize,
        need: usize,
        have: usize,
    },
    LostMidRun {
        seat: String,
        cause: String,
    },
    SharesProvider {
        seat: String,
        provider: String,
    },
}

impl Reason {
    /// The reason in a person's words (display names are the caller's, from the roster).
    pub fn words(&self) -> String {
        match self {
            Reason::OnlyOneHelper { cli } => format!("only {cli} was signed in"),
            Reason::AllParties { seats } => format!(
                "every signed-in helper ({}) built or reviewed this work",
                seats.join(", ")
            ),
            Reason::HelpersShort {
                signed_in,
                need,
                have,
            } => format!(
                "{signed_in} helper{} signed in: {have} can check this job independently and it \
                 needs {need}",
                if *signed_in == 1 { " is" } else { "s are" }
            ),
            Reason::LostMidRun { seat, cause } => format!("{seat} stopped mid-run ({cause})"),
            Reason::SharesProvider { seat, provider } => {
                format!("{seat} calls the same provider ({provider}) as the work it would check")
            }
        }
    }
}

/// Binding states (§5.1, rev 3 N6). Only `answered` satisfies a check; every state counts as
/// taking part.
pub const STATE_SEATED: &str = "seated";
pub const STATE_ANSWERED: &str = "answered";
pub const STATE_FAILED: &str = "failed";

/// (§3.3) One stand-in: a role filled on a base credential, for one scope. Durable on the run's
/// contract (`RunAssurance::stand_ins`), so a restart replays it only through [`replayable`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StandInBinding {
    /// `claude~1`: the base and its slot. The unit's `assigned_cli` for events and attribution.
    pub key: String,
    /// The credential instance it runs on: `claude`, `codex#2`.
    pub base: String,
    pub role: Role,
    pub slot: u32,
    /// The model it runs, when the base's record names how to pick one (§3.5); `None` = the CLI's
    /// default model, relationship to the builder's unknown.
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub builder_model: Option<String>,
    /// Councils and monitors only, by position (§3.5).
    #[serde(default)]
    pub lens: Option<String>,
    #[serde(default)]
    pub ord: Option<u32>,
    #[serde(default)]
    pub attempt: Option<u32>,
    #[serde(default)]
    pub subject: Option<String>,
    /// (§3.4, rev 3 N8) The immutable evidence the role reviews, persisted before dispatch.
    #[serde(default)]
    pub evidence: Option<String>,
    /// A fresh id per reservation: a retry or a continuation always reserves a new binding.
    pub invocation: String,
    pub reason: Reason,
    /// `seated` | `answered` | `failed`.
    pub state: String,
    /// The base's display name when it was seated (`Claude Code`), for the stamp's words.
    #[serde(default)]
    pub display: Option<String>,
}

impl StandInBinding {
    /// The base's display name, else its key.
    pub fn display(&self) -> &str {
        self.display.as_deref().unwrap_or(&self.base)
    }
}

/// The binding a unit on seat `seat` runs on, when the run is stand-in-aware, has consent, and
/// the seat is one of the run's OWN recorded bindings. A `~` key the run never minted is no
/// stand-in (fail closed: every fence then reads it as its base).
pub(crate) fn binding_for<'a>(
    run: &'a crate::assurance::RunAssurance,
    seat: Option<&str>,
) -> Option<&'a StandInBinding> {
    let seat = seat.filter(|s| seat_id::is_stand_in(s))?;
    if !active(run) || run.stand_in.is_none() {
        return None;
    }
    run.stand_ins.iter().find(|b| b.key == seat)
}

/// (§3.2) Where a binding applies: the role's unit, attempt, subject and evidence.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct BindingScope {
    pub ord: Option<u32>,
    pub attempt: Option<u32>,
    pub subject: Option<String>,
    pub evidence: Option<String>,
}

/// (§3.2) What one role asks [`fill`] for, after its own admission gave up.
pub(crate) struct FillRequest<'a> {
    pub role: Role,
    pub need: usize,
    /// What the role's own admission already accepted, in its order.
    pub real: Vec<String>,
    /// Credential instances that PASS this role's admission (minus the exclusion that made it
    /// short), in preference order.
    pub bases: Vec<String>,
    pub builder_model: Option<&'a str>,
    pub consent: Option<&'a StandInConsent>,
    pub reason: Reason,
    pub scope: BindingScope,
    /// The run's bindings so far, so slots never repeat.
    pub taken: &'a [StandInBinding],
}

/// (§3.2) Why a role could not be filled without consent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Shortfall {
    pub role: Role,
    pub need: usize,
    pub have: usize,
    pub reason: Reason,
}

/// (§3.2) The one answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Fill {
    /// Enough: real seats plus any stand-ins (empty when the real seats sufficed).
    Seats {
        real: Vec<String>,
        stand_ins: Vec<StandInBinding>,
    },
    /// Short, and nobody has consented: the caller raises `stand_in_needed`.
    NeedConsent(Shortfall),
    /// Short, and nothing can perform the role: the existing dead-seat path.
    NoUsableBase(Role, String),
}

/// Whether stand-ins can apply to this run at all: the launch was `standInAware`. Every fill site
/// checks this first, so an unaware run never reaches [`fill`].
pub(crate) fn active(run: &crate::assurance::RunAssurance) -> bool {
    run.stand_in_aware
}

/// The next free slot on `base` across the run's bindings (slots never repeat, so two fills never
/// mint one key).
pub(crate) fn next_slot(taken: &[StandInBinding], base: &str) -> u32 {
    taken
        .iter()
        .filter(|b| b.base == base)
        .map(|b| b.slot)
        .max()
        .unwrap_or(0)
        + 1
}

/// `<base>~<slot>`, the reserved grammar ([`seat_id`]).
pub(crate) fn binding_key(base: &str, slot: u32) -> String {
    format!("{base}{}{slot}", wicked_apps_core::spawn::STAND_IN_SEP)
}

/// (§3.2) Fill a short role. Pure: never writes a binding; the caller persists what it returns
/// through the run's single writer before anything is dispatched.
pub(crate) fn fill(req: FillRequest<'_>) -> Fill {
    if req.real.len() >= req.need {
        return Fill::Seats {
            real: req.real,
            stand_ins: Vec::new(),
        };
    }
    let bases: Vec<&String> = req
        .bases
        .iter()
        .filter(|b| !seat_id::is_stand_in(b))
        .collect();
    if bases.is_empty() {
        return Fill::NoUsableBase(
            req.role,
            format!(
                "no signed-in helper can perform {}, even as a stand-in",
                req.role.words()
            ),
        );
    }
    let Some(_) = req.consent else {
        return Fill::NeedConsent(Shortfall {
            role: req.role,
            need: req.need,
            have: req.real.len(),
            reason: req.reason,
        });
    };
    let mut minted: Vec<StandInBinding> = Vec::new();
    for i in 0..(req.need - req.real.len()) {
        let base = bases[i % bases.len()].clone();
        let all: Vec<StandInBinding> = req.taken.iter().chain(minted.iter()).cloned().collect();
        let slot = next_slot(&all, &base);
        minted.push(StandInBinding {
            key: binding_key(&base, slot),
            base,
            role: req.role,
            slot,
            model: None,
            builder_model: req.builder_model.map(str::to_string),
            lens: None,
            ord: req.scope.ord,
            attempt: req.scope.attempt,
            subject: req.scope.subject.clone(),
            evidence: req.scope.evidence.clone(),
            invocation: uuid::Uuid::new_v4().to_string(),
            reason: req.reason.clone(),
            state: STATE_SEATED.to_string(),
            display: None,
        });
    }
    Fill::Seats {
        real: req.real,
        stand_ins: minted,
    }
}

/// (§3.3, rev 3 R6) Whether a recorded binding may be dispatched again (a restart, a retry of the
/// same attempt): the run is aware with consent, the base is still on the roster and not benched,
/// the attempt is current and the binding is still `seated`. Anything else is refused, and a fresh
/// binding is reserved instead.
pub(crate) fn replayable(
    run: &crate::assurance::RunAssurance,
    binding: &StandInBinding,
    roster: &[String],
    benched: &[crate::domain::BenchedSeat],
    attempt: u32,
) -> bool {
    active(run)
        && run.stand_in.is_some()
        && roster.iter().any(|k| k == &binding.base)
        && !benched
            .iter()
            .any(|b| seat_id::credential(&b.cli) == binding.base)
        && binding.attempt.is_none_or(|a| a == attempt)
        && binding.state == STATE_SEATED
}

/// (§3.3, rev 3 N2, R7) The invocation a binding runs: its base's EXACT record on the run roster
/// (the one the launch admitted), else the registry's exact record, else the family's. `None`
/// (fail closed) when no record resolves. Only for a `~` key; real seats keep their own
/// resolution.
pub(crate) fn resolve(
    roster: &[crate::AgenticCli],
    registry: &[crate::AgenticCli],
    key: &str,
) -> Option<String> {
    if !seat_id::is_stand_in(key) {
        return None;
    }
    let base = seat_id::credential(key);
    roster
        .iter()
        .find(|c| c.key == base)
        .or_else(|| registry.iter().find(|c| c.key == base))
        .or_else(|| registry.iter().find(|c| c.key == seat_id::family(base)))
        .map(|c| c.headless_invocation.clone())
}

/// (§3.4) The preface every stand-in prompt opens with. Real-seat prompts never carry it.
pub(crate) fn preface(lens: Option<&str>) -> String {
    let mut s = String::from(
        "You are a stand-in reviewer. You did not build this work and you have none of its \
         builder's reasoning. Review it as a skeptical outside reviewer would.",
    );
    if let Some(l) = lens {
        s.push_str(&format!(" Look at it first for {l}."));
    }
    s
}

/// The label the stand-in preface rides in a unit's context.
pub(crate) const PREFACE_LABEL: &str = "[stand-in reviewer — read this first]";

/// (§3.4) The context blocks a stand-in may be handed, by label: never team findings, earlier
/// verdicts of the review, a send-back's findings, a team step's record or operator messages —
/// everything that carries the builder's (or another reviewer's) reasoning. The step's checklist,
/// the work's outputs and the engine's own records (floor, requirements, rulings, conformance) are
/// facts and stay.
pub(crate) fn packet_keeps(label: &str) -> bool {
    const DROPPED: [&str; 6] = [
        "[team findings",
        "[prior verdicts",
        "[review — unit",
        "[team step",
        "[operator messages",
        "[team ",
    ];
    !DROPPED.iter().any(|d| label.starts_with(d))
}

/// (§3.5) The lens list, by position inside one council or pool.
pub(crate) const LENSES: [&str; 9] = [
    "correctness",
    "security and safety",
    "user impact",
    "operability",
    "maintainability",
    "cost",
    "data handling",
    "accessibility",
    "compatibility",
];

/// The lens for position `i` of one council or pool.
pub(crate) fn lens(i: usize) -> &'static str {
    LENSES[i % LENSES.len()]
}

/// (§3.7) The stamp's label: who stood in, on what, and why ("Claude Code stood in for the
/// review, using Claude Code's default model: only Claude Code was signed in").
pub(crate) fn label(binding: &StandInBinding, display: &str) -> String {
    let model = match (&binding.model, &binding.builder_model) {
        (Some(m), Some(b)) if m != b => format!("using another model ({m})"),
        (Some(m), _) => format!("using {m}"),
        (None, _) => format!("using {display}'s default model"),
    };
    format!(
        "{display} stood in for {}, {model}: {}",
        binding.role.words(),
        binding.reason.words()
    )
}

/// A seat's display name from the roster (`Claude Code`), else its key.
pub(crate) fn display_name(roster: &[crate::AgenticCli], key: &str) -> String {
    let base = seat_id::credential(key);
    roster
        .iter()
        .find(|c| c.key == base)
        .or_else(|| roster.iter().find(|c| c.key == seat_id::family(base)))
        .map(|c| c.display_name.clone())
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| base.to_string())
}

/// (§3.7) The run's `standInLabel`: the first stand-in's label; a later stand-in for a different
/// reason appends "and later: …" once.
pub(crate) fn note_label(
    run: &mut crate::assurance::RunAssurance,
    binding: &StandInBinding,
    display: &str,
) {
    let this = label(binding, display);
    match run.stand_in_label.as_mut() {
        None => run.stand_in_label = Some(this),
        Some(first) => {
            let reason = binding.reason.words();
            if !first.contains("and later:") && !first.ends_with(&reason) {
                first.push_str(&format!("; and later: {reason}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn consent() -> StandInConsent {
        StandInConsent {
            scope: SCOPE_JOB.into(),
            by: "operator".into(),
            at: 1,
            via: "launch".into(),
        }
    }

    fn req<'a>(
        real: &[&str],
        bases: &[&str],
        consent: Option<&'a StandInConsent>,
        taken: &'a [StandInBinding],
    ) -> FillRequest<'a> {
        FillRequest {
            role: Role::Evaluator,
            need: 1,
            real: real.iter().map(|s| s.to_string()).collect(),
            bases: bases.iter().map(|s| s.to_string()).collect(),
            builder_model: None,
            consent,
            reason: Reason::OnlyOneHelper {
                cli: "Claude Code".into(),
            },
            scope: BindingScope {
                ord: Some(2),
                attempt: Some(0),
                ..Default::default()
            },
            taken,
        }
    }

    /// T-B1's unit form: enough real seats changes nothing.
    #[test]
    fn enough_real_seats_mint_nothing() {
        let c = consent();
        assert_eq!(
            fill(req(&["codex"], &["claude"], Some(&c), &[])),
            Fill::Seats {
                real: vec!["codex".into()],
                stand_ins: vec![]
            }
        );
    }

    #[test]
    fn short_without_consent_asks() {
        assert!(matches!(
            fill(req(&[], &["claude"], None, &[])),
            Fill::NeedConsent(Shortfall {
                role: Role::Evaluator,
                need: 1,
                have: 0,
                ..
            })
        ));
    }

    #[test]
    fn short_with_no_base_is_no_usable_base() {
        let c = consent();
        assert!(matches!(
            fill(req(&[], &[], Some(&c), &[])),
            Fill::NoUsableBase(Role::Evaluator, _)
        ));
        // A binding is never a base.
        assert!(matches!(
            fill(req(&[], &["claude~1"], Some(&c), &[])),
            Fill::NoUsableBase(Role::Evaluator, _)
        ));
    }

    #[test]
    fn slots_never_repeat_per_base() {
        let c = consent();
        let Fill::Seats { stand_ins, .. } = fill(req(&[], &["codex#2"], Some(&c), &[])) else {
            panic!("filled")
        };
        assert_eq!(stand_ins[0].key, "codex#2~1");
        assert_eq!(seat_id::credential(&stand_ins[0].key), "codex#2");
        let Fill::Seats {
            stand_ins: again, ..
        } = fill(req(&[], &["codex#2"], Some(&c), &stand_ins))
        else {
            panic!("filled")
        };
        assert_eq!(again[0].key, "codex#2~2");
        assert_ne!(again[0].invocation, stand_ins[0].invocation);
    }

    #[test]
    fn consent_is_validated() {
        assert!(consent().validate().is_ok());
        let mut c = consent();
        c.scope = "forever".into();
        assert!(c.validate().is_err());
        let mut c = consent();
        c.by = " ".into();
        assert!(c.validate().is_err());
        let mut c = consent();
        c.via = "model".into();
        assert!(c.validate().is_err());
    }

    /// T-B3 / rev 3 N4: aware + reduced is refused with or without consent; consent or parties
    /// on an unaware launch are refused, never dropped.
    #[test]
    fn launch_facts_hold_together() {
        let aware = LaunchStandIn {
            aware: true,
            ..Default::default()
        };
        assert!(aware.validate(false).is_ok());
        assert!(aware.validate(true).is_err());
        let with_consent = LaunchStandIn {
            consent: Some(consent()),
            ..aware.clone()
        };
        assert!(with_consent.validate(true).is_err());
        assert!(LaunchStandIn::default().validate(true).is_ok());
        assert!(LaunchStandIn {
            consent: Some(consent()),
            ..Default::default()
        }
        .validate(false)
        .is_err());
        assert!(LaunchStandIn {
            parties: vec!["claude".into()],
            ..Default::default()
        }
        .validate(false)
        .is_err());
        let run = with_consent.apply(crate::assurance::RunAssurance::default());
        assert!(run.stand_in_aware && run.stand_in.is_some() && run.roles == Some(vec![]));
        let unaware = LaunchStandIn::default().apply(crate::assurance::RunAssurance::default());
        assert_eq!(unaware, crate::assurance::RunAssurance::default());
    }

    /// T-R14: a forged `standIn` field or a `~` key in the launch roster is refused.
    #[test]
    fn a_forged_stand_in_in_the_roster_is_refused() {
        let ok = serde_json::json!([{"key": "claude"}, {"key": "codex#2"}]);
        assert!(refuse_forged_stand_ins(ok.as_array().unwrap()).is_ok());
        let forged = serde_json::json!([{"key": "claude", "standIn": {"base": "claude"}}]);
        assert!(refuse_forged_stand_ins(forged.as_array().unwrap()).is_err());
        let tilde = serde_json::json!([{"key": "claude~1"}]);
        assert!(refuse_forged_stand_ins(tilde.as_array().unwrap())
            .unwrap_err()
            .contains("rename the seat"));
    }

    #[test]
    fn lenses_go_by_position() {
        assert_eq!(lens(0), "correctness");
        assert_eq!(lens(9), "correctness");
        assert!(preface(Some("security and safety")).ends_with("for security and safety."));
        assert!(!preface(None).contains("Look at it first"));
    }
}
