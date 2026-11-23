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
/// taking part. (`failed` comes with the councils' stand-ins, whose seats can fail and still count.)
pub const STATE_SEATED: &str = "seated";
pub const STATE_ANSWERED: &str = "answered";

/// (§3.3) One stand-in: a role filled on a base credential, for one scope. Durable on the run's
/// contract (`RunAssurance::stand_ins`).
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

/// (§3.8) The denial source and gate class of a check held for want of consent.
pub(crate) const DENIAL_SOURCE_STAND_IN_NEEDED: &str = "stand_in_needed";

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

/// (§3.6) The stand-in a unit's pinned judge may run on when no distinct judge is left: minted on
/// the actor at dispatch (so slots never repeat), on the work's own seat (its base when that seat
/// is itself a stand-in), and recorded only if the judge path takes it ([`judge_answered`]).
/// `None` unless the run is stand-in-aware, consented and not reduced, and the unit pins an
/// approved validator.
pub(crate) fn judge_offer(
    run: &crate::assurance::RunAssurance,
    unit: &crate::domain::WorkUnit,
    attempt: u32,
    roster: &[crate::AgenticCli],
    benched: &[crate::domain::BenchedSeat],
) -> Option<StandInBinding> {
    if !active(run) || run.stand_in.is_none() || run.reduced() {
        return None;
    }
    unit.validator.as_ref().filter(|v| v.approved)?;
    let base = seat_id::credential(unit.assigned_cli.as_deref()?).to_string();
    // (T-R13) A benched base stands nothing in: its credential is what failed.
    if benched.iter().any(|b| seat_id::credential(&b.cli) == base) {
        return None;
    }
    // (§3.7) Why, from the run's facts: a helper lost during the run, else only the one.
    let reason = match benched.first() {
        Some(b) => Reason::LostMidRun {
            seat: display_name(roster, &b.cli),
            cause: b.reason.clone(),
        },
        None => Reason::OnlyOneHelper {
            cli: display_name(roster, &base),
        },
    };
    match fill(FillRequest {
        role: Role::Judge,
        need: 1,
        real: Vec::new(),
        bases: vec![base],
        builder_model: None,
        consent: run.stand_in.as_ref(),
        reason,
        scope: BindingScope {
            ord: Some(unit.ord),
            attempt: Some(attempt),
            ..Default::default()
        },
        taken: &run.stand_ins,
    }) {
        Fill::Seats { stand_ins, .. } => stand_ins.into_iter().next().map(|mut b| {
            b.display = Some(display_name(roster, &b.base));
            b
        }),
        Fill::NeedConsent(_) | Fill::NoUsableBase(..) => None,
    }
}

/// (§3.6) The binding of a stand-in judge that ANSWERED as `key`, for the fold to record: the
/// unit's own offer under that key, when the key is the offer's base with a slot the run has not
/// recorded. `None` for any other key (fail closed: a `~` key the run never offered is no
/// stand-in, and the receipt reads it as an ordinary judge seat).
pub(crate) fn judge_answered(
    run: &crate::assurance::RunAssurance,
    unit: &crate::domain::WorkUnit,
    attempt: u32,
    roster: &[crate::AgenticCli],
    benched: &[crate::domain::BenchedSeat],
    key: &str,
) -> Option<StandInBinding> {
    let mut b = judge_offer(run, unit, attempt, roster, benched)?;
    let (base, slot) = key.rsplit_once(wicked_apps_core::spawn::STAND_IN_SEP)?;
    if base != b.base || run.stand_ins.iter().any(|x| x.key == key) {
        return None;
    }
    b.slot = slot.parse().ok()?;
    b.key = key.to_string();
    b.state = STATE_ANSWERED.to_string();
    Some(b)
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

    /// T-R18 (rev 3 R7/N1): a binding on `codex#2` runs `codex#2`'s own record, never `codex`'s;
    /// a family record is the fallback only when the exact one is gone; a real key is never
    /// resolved here.
    #[test]
    fn a_binding_resolves_to_its_exact_credential_record() {
        let codex = wicked_council::registry::builtin()
            .into_iter()
            .find(|c| c.key == "codex")
            .expect("built-in codex");
        let mut codex2 = codex.clone();
        codex2.key = "codex#2".into();
        codex2.headless_invocation = "codex exec --profile two {PROMPT}".into();
        let roster = vec![codex.clone(), codex2.clone()];
        assert_eq!(
            resolve(&roster, &[], "codex#2~1").as_deref(),
            Some("codex exec --profile two {PROMPT}")
        );
        assert_eq!(
            resolve(&roster, &[], "codex~1"),
            Some(codex.headless_invocation.clone())
        );
        assert_eq!(
            resolve(&[], std::slice::from_ref(&codex), "codex#2~1"),
            Some(codex.headless_invocation.clone()),
            "the family's record when the exact one is gone"
        );
        assert_eq!(resolve(&roster, &[], "codex#2"), None);
        assert_eq!(seat_id::credential("codex#2~1"), "codex#2");
    }

    #[test]
    fn the_preface_names_a_lens_only_when_given_one() {
        assert!(preface(Some("security and safety")).ends_with("for security and safety."));
        assert!(!preface(None).contains("Look at it first"));
    }

    /// T-R5 (offer): a consented aware run offers its pinned judge a stand-in on the work's own
    /// base, at the next free slot; an unconsented, unaware or unpinned one offers none. The fold
    /// records only the key the run offered.
    #[test]
    fn a_pinned_judge_is_offered_a_stand_in_only_with_consent() {
        let mut unit = crate::domain::WorkUnit::pending("u1", "r1", 2, "review");
        unit.assigned_cli = Some("claude~1".into());
        unit.validator = Some(crate::validator::DeterministicValidator {
            criterion: "c".into(),
            script: "true".into(),
            approved: true,
        });
        let aware = LaunchStandIn {
            aware: true,
            consent: Some(consent()),
            ..Default::default()
        }
        .apply(crate::assurance::RunAssurance::default());
        let mut run = aware.clone();
        run.stand_ins.push(StandInBinding {
            key: "claude~1".into(),
            base: "claude".into(),
            role: Role::Evaluator,
            slot: 1,
            model: None,
            builder_model: None,
            lens: None,
            ord: Some(2),
            attempt: None,
            subject: None,
            evidence: None,
            invocation: "i".into(),
            reason: Reason::OnlyOneHelper {
                cli: "claude".into(),
            },
            state: STATE_SEATED.into(),
            display: None,
        });
        let offer = judge_offer(&run, &unit, 1, &[], &[]).expect("consented aware run");
        assert_eq!(
            (offer.key.as_str(), offer.base.as_str()),
            ("claude~2", "claude")
        );
        assert_eq!(
            (offer.role, offer.ord, offer.attempt),
            (Role::Judge, Some(2), Some(1))
        );
        let answered = judge_answered(&run, &unit, 1, &[], &[], "claude~2").expect("its own offer");
        assert_eq!(answered.state, STATE_ANSWERED);
        assert!(
            judge_answered(&run, &unit, 1, &[], &[], "claude~1").is_none(),
            "taken"
        );
        assert!(
            judge_answered(&run, &unit, 1, &[], &[], "codex~2").is_none(),
            "another base"
        );
        assert!(
            judge_answered(&run, &unit, 1, &[], &[], "claude").is_none(),
            "a real seat"
        );

        let mut no_consent = run.clone();
        no_consent.stand_in = None;
        assert!(judge_offer(&no_consent, &unit, 1, &[], &[]).is_none());
        assert!(judge_offer(
            &crate::assurance::RunAssurance::default(),
            &unit,
            1,
            &[],
            &[]
        )
        .is_none());
        let mut unpinned = unit.clone();
        unpinned.validator = None;
        assert!(judge_offer(&run, &unpinned, 1, &[], &[]).is_none());
        // T-R13: a benched base stands nothing in; a helper lost during the run is the reason.
        let bench = |cli: &str| crate::domain::BenchedSeat {
            cli: cli.into(),
            reason: "quota_exhausted".into(),
            source: "judge".into(),
            governed_only: false,
            benched_until: None,
            strikes: 1,
            replaced: None,
        };
        assert!(judge_offer(&run, &unit, 1, &[], &[bench("claude")]).is_none());
        let lost = judge_offer(&run, &unit, 1, &[], &[bench("codex")]).expect("claude is up");
        assert_eq!(
            lost.reason,
            Reason::LostMidRun {
                seat: "codex".into(),
                cause: "quota_exhausted".into()
            }
        );
    }
}
