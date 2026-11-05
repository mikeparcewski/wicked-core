//! DES-RULE-CHECK-001 S2: the review gate folds the rule check deny-dominant, persists it, and
//! puts every rule's verdict on the receipt.

use super::*;
use crate::rule_check::{RuleCheckReport, RuleHow, RuleStatus, RuleVerdict};
use wicked_council::types::{Category, Confidence, InputMode};

fn cli(key: &str) -> AgenticCli {
    AgenticCli {
        key: key.into(),
        display_name: key.into(),
        binary: key.into(),
        alt_binaries: Vec::new(),
        headless_invocation: format!("{key} -p {{PROMPT}}"),
        category: Category::AgenticCoder,
        input_mode: InputMode::PromptArg,
        version_probe: Vec::new(),
        trust_flags: Vec::new(),
        confidence: Confidence::default(),
        enabled_for_council: true,
        seat_eligible_for_work: true,
        acp: None,
        capabilities: None,
        login_invocation: None,
        logout_invocation: None,
        governance_class: None,
        credential: None,
        free_tier: None,
        health: None,
    }
}

fn row(id: &str, status: RuleStatus, how: RuleHow, reason: &str) -> RuleVerdict {
    RuleVerdict {
        rule_id: id.into(),
        status,
        how,
        reason: reason.into(),
        cite: (how == RuleHow::Judge).then(|| reason.to_string()),
        judge: (how == RuleHow::Judge).then(|| "codex".to_string()),
        rule_key: "k".into(),
        evidence_key: Some("e".into()),
        origin: (how == RuleHow::Judge).then_some(crate::rule_check::RuleOrigin::Judge),
        ..Default::default()
    }
}

/// The fold of one unit whose evidence carries `report`: approval, winning source, persisted unit.
fn fold(
    sid: &str,
    report: Option<RuleCheckReport>,
) -> (
    bool,
    Option<crate::domain::UnitDenial>,
    crate::domain::WorkUnit,
) {
    let (a, d, u, _) = fold_with_session(sid, report, None);
    (a, d, u)
}

fn fold_with_session(
    sid: &str,
    report: Option<RuleCheckReport>,
    verdict: Option<&crate::validator::AgentVerdict>,
) -> (
    bool,
    Option<crate::domain::UnitDenial>,
    crate::domain::WorkUnit,
    crate::domain::AgentSession,
) {
    let mut store = wicked_apps_core::open_store(Some(":memory:")).unwrap();
    let mut sink = |_e: CoreEvent| {};
    let Planned {
        mut units,
        workflow_id,
        cli_keys,
        ..
    } = plan_and_distribute(
        &mut store,
        &[cli("a")],
        "Do step one.",
        EntityMode::Shared,
        sid,
        crate::domain::HumanConfirm::None,
        false,
        None,
        None,
        Vec::new(),
        Vec::new(),
        None,
        None,
        None,
        &mut sink,
        None,
        false,
        false,
        Vec::new(),
        None,
        true,
        &crate::assurance::QeOverride::Auto,
    )
    .expect("plan");
    let evidence = crate::workflow::UnitEvidence {
        rule_check: report,
        ..Default::default()
    };
    let output = crate::workflow::stub_output(&units[0]);
    let outcome = apply_and_finish_unit(
        &mut store,
        &mut units[0],
        &output,
        &workflow_id,
        EntityMode::Shared,
        sid,
        0,
        false,
        false,
        &cli_keys,
        verdict,
        &evidence,
        &mut |_e| {},
        None,
    )
    .expect("apply");
    let persisted = crate::domain::session_units(&store, sid).unwrap()[0].clone();
    let session = crate::domain::get_session(&store, sid)
        .unwrap()
        .expect("session");
    (outcome.approved, outcome.denial, persisted, session)
}

fn report(rows: Vec<RuleVerdict>) -> RuleCheckReport {
    RuleCheckReport {
        rows,
        ..Default::default()
    }
}

#[test]
fn a_broken_rule_denies_the_gate_names_it_and_lands_on_the_receipt() {
    let (approved, denial, unit) = fold(
        "rc-broken",
        Some(report(vec![
            row(
                "DEV-TINYMATH-2",
                RuleStatus::Broken,
                RuleHow::Judge,
                "src/add.rs:3 takes floats",
            ),
            row("K", RuleStatus::Kept, RuleHow::Judge, "src/add.rs:1"),
            row(
                "MCPS-1001",
                RuleStatus::NotApplicable,
                RuleHow::Fact,
                "selected for install",
            ),
        ])),
    );
    assert!(!approved, "a broken rule denies");
    let d = denial.expect("denied");
    assert_eq!(d.source, crate::rule_check::DENIAL_SOURCE_BROKEN);
    assert_eq!(d.rule_ids, ["DEV-TINYMATH-2"]);
    assert!(
        d.reason.contains("src/add.rs:3 takes floats"),
        "{}",
        d.reason
    );
    let receipt = unit.assurance.as_ref().expect("a receipt");
    assert_eq!(receipt.rules.len(), 3, "every rule, n/a included");
    assert!(receipt
        .ran
        .iter()
        .any(|r| r == crate::rule_check::INSTRUMENT));
    assert_eq!(
        unit.rule_check.as_ref().map(|r| r.rows.len()),
        Some(3),
        "kept as the cache"
    );
}

#[test]
fn an_unchecked_rule_holds_the_gate_and_a_broken_one_still_wins() {
    let (approved, denial, _) = fold(
        "rc-unchecked",
        Some(report(vec![
            row("K", RuleStatus::Kept, RuleHow::Judge, "x"),
            row(
                "SILENT",
                RuleStatus::Unchecked,
                RuleHow::None,
                "the judge did not answer",
            ),
        ])),
    );
    assert!(!approved, "unchecked holds (operator decision 1)");
    let d = denial.expect("held");
    assert_eq!(d.source, crate::rule_check::DENIAL_SOURCE_UNCHECKED);
    assert_eq!(d.rule_ids, ["SILENT"]);
    assert_eq!(
        crate::actor::denial_class_for_test(Some(&d)),
        crate::rule_check::DENIAL_SOURCE_UNCHECKED,
        "the escalation condition"
    );
    let (_, denial, unit) = fold(
        "rc-both",
        Some(report(vec![
            row("B", RuleStatus::Broken, RuleHow::Judge, "y"),
            row("SILENT", RuleStatus::Unchecked, RuleHow::None, "no answer"),
        ])),
    );
    assert_eq!(
        denial.map(|d| d.source).as_deref(),
        Some(crate::rule_check::DENIAL_SOURCE_BROKEN)
    );
    assert!(
        unit.denials_behind.as_ref().is_some_and(|b| b
            .iter()
            .any(|d| d.source == crate::rule_check::DENIAL_SOURCE_UNCHECKED)),
        "the hold is kept behind the deny: {:?}",
        unit.denials_behind
    );
}

#[test]
fn a_check_that_could_not_run_holds_and_says_why() {
    let (approved, denial, unit) = fold(
        "rc-norun",
        Some(RuleCheckReport {
            note: Some("the steering rules could not be read from the store: locked".into()),
            ..Default::default()
        }),
    );
    assert!(!approved);
    let d = denial.expect("held");
    assert_eq!(d.source, crate::rule_check::DENIAL_SOURCE_UNCHECKED);
    assert!(d.reason.contains("could not be read"), "{}", d.reason);
    let receipt = unit.assurance.as_ref().unwrap();
    assert!(receipt
        .skipped
        .iter()
        .any(|s| s.instrument == crate::rule_check::INSTRUMENT));
}

#[test]
fn a_skip_is_labelled_on_the_receipt_and_the_delivery_and_does_not_hold() {
    let (approved, denial, unit, session) = fold_with_session(
        "rc-skip",
        Some(report(vec![
            row("K", RuleStatus::Kept, RuleHow::Judge, "x"),
            row(
                "SILENT",
                RuleStatus::Skipped,
                RuleHow::Operator,
                "skipped by operator: the judge cannot read the fixture",
            ),
        ])),
        None,
    );
    assert!(approved, "{denial:?}");
    let receipt = unit.assurance.as_ref().unwrap();
    let skipped = receipt
        .rules
        .iter()
        .find(|r| r.rule_id == "SILENT")
        .unwrap();
    assert_eq!(skipped.status, RuleStatus::Skipped);
    assert!(skipped.reason.contains("cannot read the fixture"));
    let delivery = crate::deliver_lift::delivery_receipt(&session, std::slice::from_ref(&unit));
    assert!(delivery.rules.iter().any(|r| r.rule_id == "SILENT"
        && r.status == RuleStatus::Skipped
        && r.reason.contains("fixture")));
}

#[test]
fn no_rule_check_leaves_the_gate_as_it_was() {
    let (approved, denial, unit) = fold("rc-none", None);
    assert!(approved, "{denial:?}");
    assert!(unit.assurance.as_ref().unwrap().rules.is_empty());
    assert!(unit.rule_check.is_none());
}

/// codex r1: an unchecked rule outranks the judge's own no-verdict hold, so the operator judgment
/// that answers a missing judge can never also count an unchecked rule; a broken rule's gate is its
/// own class, never the review loop's adjudication (whose approve lands the unit).
#[test]
fn an_unchecked_rule_outranks_a_missing_judge_and_a_broken_rule_has_its_own_gate() {
    let no_judge = crate::validator::AgentVerdict {
        pass: false,
        reasoning: "every judge seat failed".into(),
        judge_cli: None,
        judge_distinct: None,
        seat_failure: Some("pi (not_logged_in)".into()),
        judge_fallback: None,
    };
    let (approved, denial, unit, _) = fold_with_session(
        "rc-over-judge",
        Some(report(vec![row(
            "SILENT",
            RuleStatus::Unchecked,
            RuleHow::None,
            "no answer",
        )])),
        Some(&no_judge),
    );
    assert!(!approved);
    assert_eq!(
        denial.map(|d| d.source).as_deref(),
        Some(crate::rule_check::DENIAL_SOURCE_UNCHECKED)
    );
    assert!(unit.denials_behind.as_ref().is_some_and(|b| b
        .iter()
        .any(|d| d.source == crate::domain::DENIAL_SOURCE_JUDGE_UNAVAILABLE)));
    let (_, denial, _) = fold(
        "rc-own-class",
        Some(report(vec![row(
            "B",
            RuleStatus::Broken,
            RuleHow::Judge,
            "src/a.rs",
        )])),
    );
    assert_eq!(
        crate::actor::denial_class_for_test(denial.as_ref()),
        "rule_broken"
    );
}

/// (DES-RULE-CHECK-001 rev 2, A.7.2) A DISPUTED rule — the council reached no ruling — holds the
/// gate on its own: everything else passes, and the fold is not approved, under `rules_unchecked`.
/// Without `hold()` learning `Disputed`, the judge's BROKEN would be gone and the gate would pass.
#[test]
fn a_disputed_rule_alone_holds_the_gate() {
    let mut disputed = row(
        "R1",
        RuleStatus::Disputed,
        RuleHow::Council,
        "the council reached no ruling (1 of 3 answered): no_quorum",
    );
    disputed.origin = Some(crate::rule_check::RuleOrigin::Judge);
    let r = report(vec![
        disputed,
        row("R2", RuleStatus::Kept, RuleHow::Judge, "src/a.rs:1 typed"),
    ]);
    let (approved, denial, _) = fold("rules-disputed", Some(r));
    assert!(!approved, "a disputed rule is never approved");
    let d = denial.expect("held");
    assert_eq!(d.source, crate::rule_check::DENIAL_SOURCE_UNCHECKED);
    assert_eq!(d.rule_ids, vec!["R1".to_string()]);
    assert!(d.reason.contains("R1 DISPUTED"), "{}", d.reason);
}
