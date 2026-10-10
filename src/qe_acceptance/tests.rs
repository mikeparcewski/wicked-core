use super::*;
use crate::assurance::{QeOverride, RunAssurance};
use crate::domain::{HumanConfirm, SessionStatus};
use crate::scope::EntityMode;
use crate::workflow::PhaseRole;

fn session(assurance: RunAssurance) -> AgentSession {
    AgentSession {
        intent_amendments: Vec::new(),
        id: "s".into(),
        workflow_id: "wf-s".into(),
        problem: "p".into(),
        entity_mode: EntityMode::Shared,
        collection_scope: None,
        clis: vec![],
        status: SessionStatus::Executing,
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        unit_ix: 0,
        attempt: 0,
        workdir: None,
        repo_ref: None,
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
        project_id: None,
        archived_at: None,
        archive_note: None,
        verified_tree: None,
        run_branch: None,
        base_commit: None,
        finished_at: None,
        benched_seats: Vec::new(),
        team: None,
        team_plan: None,
        exclude_seats: Vec::new(),
        evidence_root: None,
        assurance,
    }
}

fn requiring() -> RunAssurance {
    let declared = vec![
        a::DISTINCT_EVALUATOR.to_string(),
        a::JUDGE.to_string(),
        a::QE_ACCEPTANCE.to_string(),
    ];
    RunAssurance::new(Some(&declared), false)
}

fn qe_unit(ord: u32) -> WorkUnit {
    let mut u = WorkUnit::pending(format!("s:test{ord}"), "s", ord, "verify");
    u.repo_checks_floor = true;
    u.role = PhaseRole::Evaluator;
    u
}

fn creator(ord: u32) -> WorkUnit {
    let mut u = WorkUnit::pending(format!("s:build{ord}"), "s", ord, "build");
    u.role = PhaseRole::Creator;
    u.executes_code = true;
    u
}

fn waived_at(ord: u32) -> QeAcceptance {
    QeAcceptance {
        status: a::QE_WAIVED.into(),
        basis: a::QE_BASIS_DIFF.into(),
        score: Some(20),
        threshold: 20,
        reason: "waived: impact score 20".into(),
        reasons: vec!["reach 20: 1 changed symbol(s)".into()],
        ord: Some(ord),
        tree: Some("t1".into()),
    }
}

#[test]
fn a_run_that_does_not_require_qe_acceptance_is_never_decided() {
    let s = session(RunAssurance::default());
    assert!(matches!(
        on_dispatch(&s, &qe_unit(3), None, None),
        Dispatched::Unchanged
    ));
}

#[test]
fn an_operator_decision_is_never_rescored_or_revoked() {
    for over in [QeOverride::Skip("hotfix".into()), QeOverride::Force] {
        let mut s = session(requiring().with_qe_override(&over).unwrap());
        s.base_commit = Some("b".into());
        for u in [qe_unit(3), creator(2)] {
            assert!(
                matches!(on_dispatch(&s, &u, None, None), Dispatched::Unchanged),
                "{over:?}"
            );
        }
    }
}

#[test]
fn the_qe_unit_with_no_repo_diff_is_required_with_the_reason() {
    let s = session(requiring());
    let Dispatched::Decided(d) = on_dispatch(&s, &qe_unit(3), None, None) else {
        panic!("decided");
    };
    assert_eq!((d.status.as_str(), d.basis.as_str()), ("required", "diff"));
    assert_eq!(d.ord, Some(3));
    assert!(d.reason.contains("no repository diff to score"), "{d:?}");
    // The same dispatch again changes nothing.
    let mut s2 = s.clone();
    s2.assurance.qe = Some(d);
    assert!(matches!(
        on_dispatch(&s2, &qe_unit(3), None, None),
        Dispatched::Unchanged
    ));
}

#[test]
fn a_creator_after_a_waiver_revokes_it() {
    let mut s = session(requiring());
    s.assurance.qe = Some(waived_at(4));
    let Dispatched::Decided(d) = on_dispatch(&s, &creator(5), None, None) else {
        panic!("revoked");
    };
    assert_eq!(d.status, "required");
    assert!(d.reason.contains("creator unit 5 runs after the waiver at unit 4"), "{d:?}");
    // A non-creator after the waiver (a review, the deliver tool) leaves it standing.
    let mut review = WorkUnit::pending("s:review", "s", 5, "review");
    review.role = PhaseRole::Evaluator;
    assert!(matches!(
        on_dispatch(&s, &review, None, None),
        Dispatched::Unchanged
    ));
}

/// A real repository: the diff is the base commit against the QE unit's starting tree, and a
/// file with one commit of history is low history.
#[test]
fn the_run_diff_and_history_come_from_the_repository() {
    let dir = std::env::temp_dir().join(format!("wicked-qe-diff-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args(args)
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    git(&["init", "-q"]);
    std::fs::write(dir.join("src/a.rs"), "fn a() {\n    1\n}\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "base"]);
    let base = git(&["rev-parse", "HEAD"]);
    std::fs::write(dir.join("src/a.rs"), "fn a() {\n    2\n}\n").unwrap();
    std::fs::write(dir.join("src/b.rs"), "pub fn b() {}\n").unwrap();
    git(&["add", "-A"]);
    let tree = git(&["write-tree"]);
    let git_dir = dir.join(".git");
    let diff = run_diff(&dir, &git_dir, &base, &tree).unwrap();
    let mut signals = rs::signals_from_diff(&diff);
    assert_eq!(signals.code_files, 2, "{diff}");
    assert_eq!(
        signals.new_public_symbols,
        BTreeSet::from(["b".to_string()]),
        "{diff}"
    );
    with_history(&mut signals, &dir, &git_dir, &base);
    assert_eq!(signals.low_history, 1, "src/a.rs has one commit; src/b.rs is new");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_decision_words_name_the_score_and_the_line() {
    let a_ = rs::Assessment {
        deterministic: 20,
        score: 20,
        reasons: vec!["reach 20: 1 changed symbol(s), 0 dependent(s) within 3 hops".into()],
        model: None,
        signals: Some(rs::ImpactSignals {
            changed_symbols: 1,
            products: 1,
            ..Default::default()
        }),
        plan: rs::plan_for(20),
    };
    let d = from_assessment(&a_, 4, "t1");
    assert_eq!((d.status.as_str(), d.score, d.threshold), ("waived", Some(20), 20));
    assert!(d.reason.starts_with("waived: impact score 20 at or below the waiver line 20"), "{d:?}");
    let a_ = rs::Assessment {
        score: 30,
        deterministic: 30,
        signals: Some(rs::ImpactSignals {
            changed_symbols: 1,
            products: 1,
            unindexed: 1,
            ..Default::default()
        }),
        ..a_
    };
    let d = from_assessment(&a_, 4, "t1");
    assert_eq!(d.status, "required");
    assert!(d.reason.starts_with("required: impact score 30 above the waiver line 20"), "{d:?}");
}

#[test]
fn the_prompt_directive_says_run_or_do_not_run() {
    let mut q = waived_at(4);
    assert!(directive(&q, "wicked-garden:qe").contains("do not run the acceptance pipeline"));
    q.status = a::QE_REQUIRED.into();
    let line = directive(&q, "wicked-garden:qe");
    assert!(line.contains("\"wicked-garden:qe\" accept") && line.contains("WICKED_RUN_ID"), "{line}");
    q.status = a::QE_SKIPPED.into();
    assert!(directive(&q, "x").contains("skipped by the operator"));
}
