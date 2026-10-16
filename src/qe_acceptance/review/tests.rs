use super::*;
use crate::assurance::{self as a, QeAcceptance, RunAssurance};
use crate::domain::{BenchedSeat, HumanConfirm, SessionStatus};
use crate::scope::EntityMode;
use crate::workflow::PhaseRole;
use crate::workflow::PriorUnitOutput;
use std::sync::Mutex;

const RUN: &str = "run-qe-1";

fn session(clis: &[&str], reduced: bool) -> AgentSession {
    let declared = vec![
        a::DISTINCT_EVALUATOR.to_string(),
        a::JUDGE.to_string(),
        a::QE_ACCEPTANCE.to_string(),
    ];
    AgentSession {
        intent_amendments: Vec::new(),
        id: RUN.into(),
        workflow_id: "wf".into(),
        problem: "p".into(),
        entity_mode: EntityMode::Shared,
        collection_scope: None,
        clis: clis.iter().map(|s| s.to_string()).collect(),
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
        assurance: RunAssurance::new(Some(&declared), reduced),
    }
}

fn required() -> QeAcceptance {
    QeAcceptance {
        status: a::QE_REQUIRED.into(),
        basis: a::QE_BASIS_DIFF.into(),
        score: Some(60),
        threshold: 20,
        reason: "required".into(),
        reasons: Vec::new(),
        ord: Some(3),
        tree: Some("t1".into()),
    }
}

fn creator(ord: u32, seat: &str) -> WorkUnit {
    let mut u = WorkUnit::pending(format!("{RUN}:build{ord}"), RUN, ord, "build it");
    u.role = PhaseRole::Creator;
    u.assigned_cli = Some(seat.into());
    u
}

fn baseline(tree: &str) -> crate::worktree_guard::WorktreeSnapshot {
    crate::worktree_guard::WorktreeSnapshot {
        head: "h".into(),
        head_ref: None,
        tree: tree.into(),
        taken_at_ms: 0,
        git_dir: None,
    }
}

fn qe_unit(seat: &str, ledger: &Path) -> WorkUnit {
    let mut u = WorkUnit::pending(format!("{RUN}:test"), RUN, 3, "verify the build");
    u.role = PhaseRole::Evaluator;
    u.stage = crate::domain::StageKind::Test;
    u.repo_checks_floor = true;
    u.assigned_cli = Some(seat.into());
    u.qe_ledger_root = Some(ledger.to_string_lossy().into_owned());
    u.qe_acceptance = Some(required());
    u.base_skill_ref = Some("wicked-garden-governed-worker".into());
    u.instructions = Some("phase instructions".into());
    u.worktree_baseline = Some(baseline("t1"));
    u
}

/// A temp QE ledger root (canonical JSON tables, as wicked-ledger writes them).
struct Ledger {
    base: PathBuf,
    root: PathBuf,
}

impl Ledger {
    fn new(name: &str) -> Self {
        let base = std::env::temp_dir().join(format!(
            "wicked-qe-review-{name}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let root = base.join(".wicked-qe");
        std::fs::create_dir_all(root.join("runs")).unwrap();
        std::fs::create_dir_all(root.join("verdicts")).unwrap();
        let root = std::fs::canonicalize(&root).unwrap();
        Ledger { base, root }
    }

    /// An evidence run: dir with plan + scenario, and its run record.
    fn evidence(&self, crew_run: &str, created_at: &str) -> (String, PathBuf) {
        let id = uuid::Uuid::new_v4().to_string();
        let dir = self.root.join("evidence").join(&id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("test-plan.md"), "# plan").unwrap();
        std::fs::write(dir.join("scenario.md"), "# scenario").unwrap();
        self.run(&id, crew_run, &dir, created_at);
        (id, dir)
    }

    fn run(&self, id: &str, crew_run: &str, dir: &Path, created_at: &str) {
        let rec = serde_json::json!({
            "id": id, "created_at": created_at, "status": "running",
            "crew_run_id": crew_run, "evidence_path": dir.to_string_lossy(),
        });
        std::fs::write(
            self.root.join("runs").join(format!("{id}.json")),
            rec.to_string(),
        )
        .unwrap();
    }

    fn verdict(
        &self,
        run_id: &str,
        verdict: &str,
        session: Option<&str>,
        crew_run: &str,
        at: &str,
    ) {
        write_verdict(&self.root, run_id, verdict, session, crew_run, at);
    }
}

/// One verdict row, as garden's step 6 writes it through wicked-ledger.
fn write_verdict(
    root: &Path,
    run_id: &str,
    verdict: &str,
    session: Option<&str>,
    crew_run: &str,
    at: &str,
) {
    let vid = uuid::Uuid::new_v4().to_string();
    let mut rec = serde_json::json!({
        "id": vid, "created_at": at, "run_id": run_id, "verdict": verdict,
        "crew_run_id": crew_run, "reviewer": "acceptance-test-reviewer",
    });
    if let Some(s) = session {
        rec["review_session"] = serde_json::json!(s);
    }
    std::fs::write(
        root.join("verdicts").join(format!("{vid}.json")),
        rec.to_string(),
    )
    .unwrap();
}

impl Drop for Ledger {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn now_iso() -> String {
    // Far in the future relative to any `not_before` the tests pass.
    "2099-01-01T00:00:00.000Z".to_string()
}

#[test]
fn the_marker_is_the_last_evidence_line_with_decoration_tolerated() {
    let out =
        "plan written\nQE-EVIDENCE: /a/old\n- **QE-EVIDENCE: `/l/evidence/x`**\nVERDICT: PASS";
    assert_eq!(parse_marker(out).as_deref(), Some("/l/evidence/x"));
    assert_eq!(parse_marker("no marker here\nVERDICT: PASS"), None);
    assert_eq!(parse_marker("QE-EVIDENCE:   \n"), None);
}

#[test]
fn iso_timestamps_parse_as_node_writes_them() {
    assert_eq!(iso_millis("1970-01-01T00:00:00.000Z"), Some(0));
    assert_eq!(iso_millis("1970-01-02T00:00:01.5Z"), Some(86_401_500));
    assert_eq!(
        iso_millis("2026-10-12T14:02:03.123Z"),
        Some(1_791_813_723_123)
    );
    assert_eq!(iso_millis("2026-10-12 14:02:03"), None);
    assert_eq!(iso_millis("2026-13-12T14:02:03Z"), None);
    // Impossible dates, signs, short fields and out-of-range years are refused, never wrapped.
    assert_eq!(iso_millis("2026-02-31T00:00:00.000Z"), None);
    assert_eq!(iso_millis("2025-02-29T00:00:00.000Z"), None);
    assert!(
        iso_millis("2024-02-29T00:00:00.000Z").is_some(),
        "a leap day"
    );
    assert_eq!(iso_millis("2026-10-12T-1:00:00.000Z"), None);
    assert_eq!(iso_millis("2026-1-12T01:00:00.000Z"), None);
    assert_eq!(iso_millis("1969-12-31T23:59:59.999Z"), None);
    assert_eq!(iso_millis("99999999999-01-01T00:00:00.000Z"), None);
    assert_eq!(iso_millis("2026-10-12T14:02:03.Z"), None);
}

#[test]
fn evidence_binds_only_to_this_runs_ledger_dir_from_this_attempt() {
    let l = Ledger::new("bind");
    let (id, dir) = l.evidence(RUN, &now_iso());
    let d = dir.to_string_lossy().into_owned();
    assert_eq!(
        bind_evidence(&l.root, &d, RUN, 0, None).unwrap(),
        (id.clone(), dir.clone())
    );
    // A relative path, a foreign run, a pre-attempt run (unless it is the one resumed).
    assert!(bind_evidence(&l.root, "evidence/x", RUN, 0, None).is_err());
    let foreign = bind_evidence(&l.root, &d, "another-run", 0, None).unwrap_err();
    assert!(foreign.contains("not stamped with this run"), "{foreign}");
    let later = iso_millis("2099-06-01T00:00:00.000Z").unwrap();
    let stale = bind_evidence(&l.root, &d, RUN, later, None).unwrap_err();
    assert!(stale.contains("not created in this attempt"), "{stale}");
    assert!(
        bind_evidence(&l.root, &d, RUN, later, Some(&d)).is_ok(),
        "the resumed run binds"
    );
    // A dir outside the ledger, and a non-UUID dir name.
    let outside = l.base.join(&id);
    std::fs::create_dir_all(&outside).unwrap();
    let why = bind_evidence(&l.root, &outside.to_string_lossy(), RUN, 0, None).unwrap_err();
    assert!(why.contains("not this run's ledger evidence dir"), "{why}");
    let odd = l.root.join("evidence").join("not-a-uuid");
    std::fs::create_dir_all(&odd).unwrap();
    assert!(bind_evidence(&l.root, &odd.to_string_lossy(), RUN, 0, None)
        .unwrap_err()
        .contains("not a QE run id"));
    // No plan.
    std::fs::remove_file(dir.join("test-plan.md")).unwrap();
    assert!(bind_evidence(&l.root, &d, RUN, 0, None)
        .unwrap_err()
        .contains("test-plan.md"));
}

#[cfg(unix)]
#[test]
fn a_linked_evidence_dir_is_refused_and_containment_is_canonical() {
    let l = Ledger::new("link");
    let (id, dir) = l.evidence(RUN, &now_iso());
    // The dir itself a link to the real one.
    let elsewhere = l.base.join("elsewhere");
    std::fs::rename(&dir, &elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &dir).unwrap();
    let why = bind_evidence(&l.root, &dir.to_string_lossy(), RUN, 0, None).unwrap_err();
    assert!(why.contains("not a plain directory"), "{why}");
    // A parent link: <base>/alias -> <root>; the path through it is not canonical.
    std::fs::remove_file(&dir).unwrap();
    std::fs::rename(&elsewhere, &dir).unwrap();
    let alias = l.base.join("alias");
    std::os::unix::fs::symlink(&l.root, &alias).unwrap();
    let via = alias.join("evidence").join(&id);
    // Containment is canonical: a path through a parent link resolves to the real ledger dir and
    // binds as that dir (never as wherever the alias claims to be).
    let (bound_id, bound) = bind_evidence(&l.root, &via.to_string_lossy(), RUN, 0, None).unwrap();
    assert_eq!((bound_id, bound), (id, dir));
}

#[test]
fn the_verdict_binds_only_when_the_reviewer_session_recorded_it() {
    let l = Ledger::new("verdict");
    let (id, _) = l.evidence(RUN, &now_iso());
    assert!(bind_verdict(&l.root, &id, RUN, "n1", "PASS")
        .unwrap_err()
        .contains("recorded no verdict"));
    l.verdict(&id, "PASS", Some("n1"), RUN, "2099-01-01T00:00:01.000Z");
    assert_eq!(
        bind_verdict(&l.root, &id, RUN, "n1", "PASS").unwrap(),
        "PASS"
    );
    assert!(bind_verdict(&l.root, &id, RUN, "n1", "FAIL")
        .unwrap_err()
        .contains("not the reviewer's own"));
    // A newer verdict without the nonce (the executor grading itself) is the governing one, so
    // it does not bind.
    l.verdict(&id, "PASS", None, RUN, "2099-01-01T00:00:02.000Z");
    assert!(bind_verdict(&l.root, &id, RUN, "n1", "PASS")
        .unwrap_err()
        .contains("not recorded by the isolated reviewer"));
    // A newer nonced verdict stamped with another crew run.
    l.verdict(&id, "PASS", Some("n1"), "other", "2099-01-01T00:00:03.000Z");
    assert!(bind_verdict(&l.root, &id, RUN, "n1", "PASS")
        .unwrap_err()
        .contains("not stamped with this run"));
}

#[test]
fn candidates_come_from_the_runs_own_non_author_seats_in_order() {
    let ledger = Path::new("/l/.wicked-qe");
    let s = session(&["claude", "pi", "opencode"], false);
    let units = vec![creator(2, "claude")];
    let u = qe_unit("pi", ledger);
    let seats = |c: &[QeReviewerSeat]| {
        c.iter()
            .map(|c| (c.seat.clone(), c.basis.clone()))
            .collect::<Vec<_>>()
    };
    let pair = |a: &str, b: &str| (a.to_string(), b.to_string());
    assert_eq!(
        seats(&candidates(&s, &units, &u)),
        vec![pair("opencode", BASIS_DISTINCT), pair("pi", BASIS_EXECUTOR)]
    );
    // Benched or launch-excluded seats never review.
    let mut benched = s.clone();
    benched.benched_seats.push(BenchedSeat {
        cli: "opencode".into(),
        reason: "quota_exhausted".into(),
        source: "worker".into(),
        governed_only: false,
    });
    assert_eq!(
        seats(&candidates(&benched, &units, &u)),
        vec![pair("pi", BASIS_EXECUTOR)]
    );
    let mut excluded = s.clone();
    excluded.exclude_seats = vec!["opencode".into()];
    assert_eq!(
        seats(&candidates(&excluded, &units, &u)),
        vec![pair("pi", BASIS_EXECUTOR)]
    );
    // An executor on an author's seat is not row 2; with no other seat there is no reviewer.
    let on_author = qe_unit("claude", ledger);
    let two = session(&["claude", "pi"], false);
    assert_eq!(
        seats(&candidates(&two, &units, &on_author)),
        vec![pair("pi", BASIS_DISTINCT)]
    );
    let one = session(&["claude"], false);
    assert!(candidates(&one, &units, &on_author).is_empty());
    // The unit's own authored_by (a floor fixer) is an author too.
    let mut fixed = qe_unit("pi", ledger);
    fixed.authored_by = vec!["pi".into()];
    assert_eq!(
        seats(&candidates(&s, &units, &fixed)),
        vec![pair("opencode", BASIS_DISTINCT)]
    );
}

#[test]
fn reduced_assurance_relaxes_authors_only_and_instances_are_distinct() {
    let ledger = Path::new("/l/.wicked-qe");
    let units = vec![creator(2, "claude")];
    let on_author = qe_unit("claude", ledger);
    let mut reduced = session(&["claude"], true);
    let got = candidates(&reduced, &units, &on_author);
    assert_eq!(got.len(), 1);
    assert_eq!(
        (got[0].seat.as_str(), got[0].basis.as_str()),
        ("claude", BASIS_REDUCED)
    );
    // Never a launch-excluded seat, even in reduced assurance.
    reduced.exclude_seats = vec!["claude".into()];
    assert!(candidates(&reduced, &units, &on_author).is_empty());
    // Another instance of the creator's CLI is a distinct reviewer, labelled same_cli.
    let s = session(&["claude", "claude#2"], false);
    let got = candidates(&s, &units, &qe_unit("claude#2", ledger));
    assert_eq!(got.len(), 1);
    assert_eq!(
        (got[0].seat.as_str(), got[0].basis.as_str()),
        ("claude#2", BASIS_EXECUTOR)
    );
    assert!(got[0].same_cli);
}

#[test]
fn the_reviewer_unit_carries_only_the_path_the_skill_and_the_nonce() {
    let ledger = Path::new("/l/.wicked-qe");
    let mut u = qe_unit("pi", ledger);
    u.amendments = vec!["operator note".into()];
    u.qe_reviewer_seats = vec![seat("claude", BASIS_DISTINCT)];
    let claude = QeReviewerSeat {
        seat: "claude".into(),
        basis: BASIS_DISTINCT.into(),
        same_cli: false,
        invocation: Some("claude -p {PROMPT}".into()),
    };
    let r = reviewer_unit(&u, &claude, "/l/.wicked-qe/evidence/x", Some("n-1"));
    assert_eq!(r.assigned_cli.as_deref(), Some("claude"));
    assert_eq!(
        r.assigned_invocation.as_deref(),
        Some("claude -p {PROMPT}"),
        "the run roster's template for that seat, not the registry default"
    );
    assert_eq!(
        r.skill_ref.as_deref(),
        Some(crate::execute_wrapped::QE_SKILL)
    );
    assert!(r
        .description
        .contains("accept --phase review --evidence /l/.wicked-qe/evidence/x"));
    assert!(!r.description.contains("verify the build"));
    assert_eq!(
        (
            r.qe_acceptance.as_ref(),
            r.base_skill_ref.as_ref(),
            r.instructions.as_ref()
        ),
        (None, None, None)
    );
    assert!(r.amendments.is_empty() && r.qe_reviewer_seats.is_empty());
    assert_eq!(
        r.role,
        PhaseRole::Evaluator,
        "the reviewer keeps the evaluator role"
    );
    assert!(
        super::super::carries_ledger_root(&r),
        "and the writable ledger boundary"
    );
    assert!(
        !applies(&r),
        "the reviewer turn never runs a reviewer of its own"
    );
    // Only the reviewer's child gets the nonce.
    let env = super::super::ledger_env(&r);
    assert!(
        env.contains(&(REVIEW_SESSION_ENV.to_string(), "n-1".to_string())),
        "{env:?}"
    );
    assert!(!super::super::ledger_env(&u)
        .iter()
        .any(|(k, _)| k == REVIEW_SESSION_ENV));
    assert!(wicked_apps_core::spawn::ENGINE_INTERNAL_ENV.contains(&REVIEW_SESSION_ENV));
}

#[test]
fn the_reviewer_session_variable_never_reaches_a_hardened_child() {
    use wicked_apps_core::spawn::HardenedCommand;
    let mut cmd = std::process::Command::new("true");
    cmd.env(REVIEW_SESSION_ENV, "leaked");
    cmd.hardened();
    let set = cmd
        .get_envs()
        .find(|(k, _)| *k == std::ffi::OsStr::new(REVIEW_SESSION_ENV))
        .map(|(_, v)| v.is_some());
    assert_ne!(
        set,
        Some(true),
        "hardened() must strip {REVIEW_SESSION_ENV}"
    );
}

#[test]
fn the_executor_directive_is_evidence_only_and_resumes_a_same_tree_run() {
    let ledger = Path::new("/l/.wicked-qe");
    let mut u = qe_unit("pi", ledger);
    let line = super::super::unit_directive(&u, "wicked-garden-qe");
    assert!(
        line.contains("accept --phase evidence (writer and executor only"),
        "{line}"
    );
    assert!(line.contains(EVIDENCE_MARKER) && line.contains("$WICKED_QE_LEDGER_DIR"));
    assert!(
        line.len() < 520,
        "the directive rides the PTY line budget: {}",
        line.len()
    );
    assert_eq!(resume_dir(&u), None);
    u.qe_review = Some(QeReview {
        outcome: OUTCOME_FAIL.into(),
        reason: "r".into(),
        evidence_dir: Some("/l/.wicked-qe/evidence/abc".into()),
        qe_run_id: Some("abc".into()),
        tree: Some("t1".into()),
        verdict: Some("FAIL".into()),
        attempts: Vec::new(),
    });
    assert_eq!(resume_dir(&u), Some("/l/.wicked-qe/evidence/abc"));
    assert!(super::super::unit_directive(&u, "q").contains("--resume /l/.wicked-qe/evidence/abc"));
    u.worktree_baseline = Some(baseline("t2"));
    assert_eq!(resume_dir(&u), None, "a changed tree starts a fresh QE run");
    // Without a ledger root the unit keeps the full-pipeline directive (no reviewer can bind).
    let mut bare = qe_unit("pi", ledger);
    bare.qe_ledger_root = None;
    assert!(super::super::unit_directive(&bare, "q").contains("accept (writer, executor, isolated"));
    assert!(reviewer_brief("/d").len() < 520);
}

/// A scripted seat: every reviewer turn it is handed is recorded, and it answers per seat.
struct Scripted {
    ledger: PathBuf,
    qe_run: String,
    answers: Vec<(&'static str, &'static str)>,
    seen: Mutex<Vec<StepInput>>,
}

impl StepRunner for Scripted {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        self.seen.lock().unwrap().push(input.clone());
        let seat = input.unit.assigned_cli.clone().unwrap_or_default();
        let answer = self
            .answers
            .iter()
            .find(|(s, _)| *s == seat)
            .map(|(_, a)| *a)
            .unwrap_or("nothing");
        let (status, output) = match answer {
            "pass" | "fail" => {
                let verdict = if answer == "pass" { "PASS" } else { "FAIL" };
                write_verdict(
                    &self.ledger,
                    &self.qe_run,
                    verdict,
                    input.unit.qe_review_session.as_deref(),
                    &input.run_id,
                    "2099-01-01T00:00:05.000Z",
                );
                (StepStatus::Ok, format!("reviewed\nVERDICT: {verdict}"))
            }
            "signed-out" => (
                StepStatus::Failed,
                "Error: not logged in. Run /login".to_string(),
            ),
            "timeout-signed-out" => (
                StepStatus::TimedOut,
                "Error: not logged in. Run /login".to_string(),
            ),
            "ok-signed-out" => (StepStatus::Ok, "it said: not logged in".to_string()),
            "silent" => (StepStatus::Ok, "I looked at it.".to_string()),
            _ => (StepStatus::Failed, "boom".to_string()),
        };
        StepOutput {
            run_id: input.run_id.clone(),
            unit_ix: input.unit_ix,
            attempt: input.attempt,
            output,
            status,
            usage: None,
            files: vec![format!("{seat}.log")],
            tools: vec![format!("{seat}:Read")],
            governed: true,
        }
    }
}

fn input_for(unit: WorkUnit) -> StepInput {
    StepInput {
        run_id: RUN.into(),
        unit_ix: 2,
        attempt: 1,
        unit,
        workflow_id: "wf".into(),
        entity_mode: EntityMode::Shared,
        workdir: None,
        governance: None,
        prior_outputs: vec![PriorUnitOutput {
            label: "[build]".into(),
            output: "the creator's reasoning".into(),
        }],
        elicitation_epoch: 0,
        process_gen: None,
        launch_seq: 0,
        required_skills: Vec::new(),
    }
}

fn executor_output(marker: &str) -> StepOutput {
    StepOutput {
        run_id: RUN.into(),
        unit_ix: 2,
        attempt: 1,
        output: format!("ran the plan\nQE-EVIDENCE: {marker}\nVERDICT: PASS"),
        status: StepStatus::Ok,
        usage: None,
        files: Vec::new(),
        tools: Vec::new(),
        governed: false,
    }
}

fn seat(s: &str, basis: &str) -> QeReviewerSeat {
    QeReviewerSeat {
        seat: s.into(),
        basis: basis.into(),
        same_cli: false,
        invocation: None,
    }
}

fn review_with(
    answers: Vec<(&'static str, &'static str)>,
    seats: Vec<QeReviewerSeat>,
) -> (Option<QeReview>, Vec<StepInput>) {
    let l = Ledger::new("run");
    let (id, dir) = l.evidence(RUN, &now_iso());
    let mut u = qe_unit("pi", &l.root);
    u.qe_reviewer_seats = seats;
    let scripted = Arc::new(Scripted {
        ledger: l.root.clone(),
        qe_run: id,
        answers,
        seen: Mutex::new(Vec::new()),
    });
    let runner: Arc<dyn StepRunner> = scripted.clone();
    let got = run(
        &runner,
        &input_for(u),
        &executor_output(&dir.to_string_lossy()),
        0,
        &|_: &str| {},
    );
    let seen = scripted.seen.lock().unwrap().clone();
    (got, seen)
}

#[test]
fn a_pi_executor_is_reviewed_in_a_fresh_isolated_session_and_the_verdict_binds() {
    let (got, seen) = review_with(
        vec![("claude", "pass")],
        vec![seat("claude", BASIS_DISTINCT), seat("pi", BASIS_EXECUTOR)],
    );
    let got = got.expect("the QE unit runs a reviewer");
    assert_eq!(got.outcome, OUTCOME_PASS, "{:?}", got);
    assert_eq!(got.verdict.as_deref(), Some("PASS"));
    assert_eq!(got.tree.as_deref(), Some("t1"));
    assert_eq!(seen.len(), 1);
    let r = &seen[0];
    assert!(
        r.prior_outputs.is_empty(),
        "the reviewer never sees prior context"
    );
    assert!(r.unit.qe_acceptance.is_none());
    assert!(r.unit.description.contains("--phase review --evidence"));
    assert!(r.unit.qe_review_session.is_some());
    // The reviewer turn's governance is recorded per turn: its nonce names its own decisions log.
    let a = &got.attempts[0];
    assert!(a.governed && a.tool_activity, "{a:?}");
    assert_eq!(Some(&a.session), r.unit.qe_review_session.as_ref());
}

#[test]
fn a_reviewer_fail_binds_as_fail() {
    let (got, _) = review_with(
        vec![("claude", "fail")],
        vec![seat("claude", BASIS_DISTINCT)],
    );
    let got = got.unwrap();
    assert_eq!(got.outcome, OUTCOME_FAIL);
    let d = denial(&got).unwrap();
    assert_eq!(d.source, DENIAL_SOURCE_QE_REVIEW);
}

#[test]
fn only_a_classified_seat_failure_moves_on_to_the_next_seat() {
    let (got, seen) = review_with(
        vec![("claude", "signed-out"), ("pi", "pass")],
        vec![seat("claude", BASIS_DISTINCT), seat("pi", BASIS_EXECUTOR)],
    );
    let got = got.unwrap();
    assert_eq!(got.outcome, OUTCOME_PASS, "{:?}", got);
    assert_eq!(seen.len(), 2);
    assert_eq!(got.attempts[0].status, "seat_failure:not_logged_in");
    assert_ne!(
        seen[0].unit.qe_review_session, seen[1].unit.qe_review_session,
        "a fresh nonce per reviewer turn"
    );
    // An unclassified failure is invalid, never another seat.
    let (got, seen) = review_with(
        vec![("claude", "boom"), ("pi", "pass")],
        vec![seat("claude", BASIS_DISTINCT), seat("pi", BASIS_EXECUTOR)],
    );
    assert_eq!(got.unwrap().outcome, OUTCOME_INVALID);
    assert_eq!(seen.len(), 1);
    // A completed turn with no verdict line is invalid.
    let (got, _) = review_with(
        vec![("claude", "silent")],
        vec![seat("claude", BASIS_DISTINCT)],
    );
    let got = got.unwrap();
    assert_eq!(got.outcome, OUTCOME_INVALID);
    assert!(got.reason.contains("no VERDICT line"));
}

#[test]
fn no_seat_or_every_seat_failing_is_unavailable_for_the_operator() {
    let (got, _) = review_with(vec![], vec![]);
    let got = got.unwrap();
    assert_eq!(got.outcome, OUTCOME_UNAVAILABLE);
    assert_eq!(
        denial(&got).unwrap().source,
        DENIAL_SOURCE_QE_REVIEWER_UNAVAILABLE
    );
    let (got, _) = review_with(
        vec![("claude", "signed-out")],
        vec![seat("claude", BASIS_DISTINCT)],
    );
    let review = got.unwrap();
    assert_eq!(review.outcome, OUTCOME_UNAVAILABLE);
    let mut u = WorkUnit::pending("r:test", "r", 3, "v");
    u.qe_review = Some(review);
    let d = operator_judged(&u, "mike").unwrap();
    assert_eq!(
        (d.status.as_str(), d.basis.as_str()),
        (a::QE_SKIPPED, a::QE_BASIS_OPERATOR)
    );
    assert!(
        d.reason.starts_with("judged by operator (mike)"),
        "{}",
        d.reason
    );
    u.qe_review.as_mut().unwrap().outcome = OUTCOME_FAIL.into();
    assert!(
        operator_judged(&u, "mike").is_none(),
        "only an unavailable reviewer is judged"
    );
}

#[test]
fn no_marker_or_no_requirement_runs_no_reviewer() {
    let l = Ledger::new("nomarker");
    let u = qe_unit("pi", &l.root);
    let runner: Arc<dyn StepRunner> = Arc::new(crate::workflow::StubStepRunner);
    let mut out = executor_output("x");
    out.output = "VERDICT: PASS".into();
    let got = run(&runner, &input_for(u.clone()), &out, 0, &|_: &str| {}).unwrap();
    assert_eq!(got.outcome, OUTCOME_INVALID);
    assert!(got.reason.contains("QE-EVIDENCE"), "{}", got.reason);
    let mut waived = u.clone();
    waived.qe_acceptance.as_mut().unwrap().status = a::QE_WAIVED.into();
    assert!(run(&runner, &input_for(waived), &out, 0, &|_: &str| {}).is_none());
    let mut failed = executor_output("x");
    failed.status = StepStatus::Failed;
    assert!(run(&runner, &input_for(u), &failed, 0, &|_: &str| {}).is_none());
}

#[test]
fn a_floor_rerun_carries_on_the_same_tree_and_a_floor_fix_invalidates() {
    let ledger = Path::new("/l/.wicked-qe");
    let mut u = qe_unit("pi", ledger);
    let pass = QeReview {
        outcome: OUTCOME_PASS.into(),
        reason: "ok".into(),
        evidence_dir: Some("/l/.wicked-qe/evidence/abc".into()),
        qe_run_id: Some("abc".into()),
        tree: Some("t1".into()),
        verdict: Some("PASS".into()),
        attempts: Vec::new(),
    };
    u.qe_review = Some(pass.clone());
    assert_eq!(on_floor_rerun(&u, false), Some(pass));
    assert_eq!(on_floor_rerun(&u, true).unwrap().outcome, OUTCOME_INVALID);
    u.worktree_baseline = Some(baseline("t2"));
    assert_eq!(on_floor_rerun(&u, false).unwrap().outcome, OUTCOME_INVALID);
}

/// A seat that answers the EXECUTOR turn with the evidence marker and every reviewer turn per
/// `Scripted` — the worker seam end to end.
struct Seam {
    marker: String,
    reviewer: Scripted,
    executor_inputs: Mutex<Vec<StepInput>>,
}

impl StepRunner for Seam {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        if input.unit.qe_review_session.is_some() {
            return self.reviewer.run_unit(input);
        }
        self.executor_inputs.lock().unwrap().push(input.clone());
        let mut out = executor_output(&self.marker);
        out.tools = vec!["pi:Bash".into()];
        out
    }
}

/// (DES-QE-EVERY-SEAT §3.1) Through the real worker seam (`run_unit_and_judge_on`): the pi
/// executor turn produces evidence, the engine runs the isolated reviewer on the distinct seat,
/// the unit's output stays the executor's, the reviewer's tool calls join the envelope, and the
/// record rides `UnitEvidence.qe_review` to the fold.
#[test]
fn the_worker_seam_runs_the_executor_then_the_isolated_reviewer() {
    let l = Ledger::new("seam");
    let (id, dir) = l.evidence(RUN, &now_iso());
    let mut u = qe_unit("pi", &l.root);
    u.qe_reviewer_seats = vec![seat("claude", BASIS_DISTINCT), seat("pi", BASIS_EXECUTOR)];
    let seam = Arc::new(Seam {
        marker: dir.to_string_lossy().into_owned(),
        reviewer: Scripted {
            ledger: l.root.clone(),
            qe_run: id.clone(),
            answers: vec![("claude", "pass")],
            seen: Mutex::new(Vec::new()),
        },
        executor_inputs: Mutex::new(Vec::new()),
    });
    let runner: Arc<dyn StepRunner> = seam.clone();
    let (output, _verdict, evidence) = crate::cli_runner::run_unit_and_judge_with_team(
        &runner,
        &input_for(u),
        &|_: &str| {},
        &[],
        None,
        None,
    );
    assert!(
        output.output.contains("QE-EVIDENCE:"),
        "the unit's output is the executor's"
    );
    // The envelope stays the executor's: the reviewer turn's governance is folded from its own
    // decisions log, by the flags its attempt record carries.
    assert_eq!(output.tools, vec!["pi:Bash".to_string()]);
    assert!(!output.governed);
    let review = evidence.qe_review.expect("the reviewer ran");
    assert!(review.attempts[0].governed && review.attempts[0].tool_activity);
    assert_eq!(review.outcome, OUTCOME_PASS, "{review:?}");
    assert_eq!(review.qe_run_id.as_deref(), Some(id.as_str()));
    assert_eq!(seam.executor_inputs.lock().unwrap().len(), 1);
    let reviewed = seam.reviewer.seen.lock().unwrap();
    assert_eq!(reviewed.len(), 1);
    assert_eq!(reviewed[0].unit.assigned_cli.as_deref(), Some("claude"));
}

/// (codex r1 on the code) Rotation only on a FAILED turn the classifiers name: a timeout or a
/// completed turn that merely mentions a sign-in refusal is `invalid`, never the next seat.
#[test]
fn a_timeout_or_a_completed_turn_with_refusal_text_is_invalid_not_rotated() {
    for answer in ["timeout-signed-out", "ok-signed-out"] {
        let (got, seen) = review_with(
            vec![("claude", answer), ("pi", "pass")],
            vec![seat("claude", BASIS_DISTINCT), seat("pi", BASIS_EXECUTOR)],
        );
        assert_eq!(got.unwrap().outcome, OUTCOME_INVALID, "{answer}");
        assert_eq!(seen.len(), 1, "{answer}: no second reviewer seat");
    }
}

/// (§3.7, codex r1 on the code) A same-tree retry that fails to bind keeps the QE run it was told
/// to resume, so the next retry still resumes it.
#[test]
fn an_invalid_same_tree_retry_keeps_the_resumed_qe_run() {
    let l = Ledger::new("lineage");
    let (id, dir) = l.evidence(RUN, "2020-01-01T00:00:00.000Z");
    let mut u = qe_unit("pi", &l.root);
    u.qe_review = Some(QeReview {
        outcome: OUTCOME_FAIL.into(),
        reason: "failed".into(),
        evidence_dir: Some(dir.to_string_lossy().into_owned()),
        qe_run_id: Some(id.clone()),
        tree: Some("t1".into()),
        verdict: Some("FAIL".into()),
        attempts: Vec::new(),
    });
    let runner: Arc<dyn StepRunner> = Arc::new(crate::workflow::StubStepRunner);
    let mut out = executor_output("x");
    out.output = "forgot the marker\nVERDICT: PASS".into();
    let got = run(&runner, &input_for(u.clone()), &out, 0, &|_: &str| {}).unwrap();
    assert_eq!(got.outcome, OUTCOME_INVALID);
    assert_eq!(
        got.qe_run_id.as_deref(),
        Some(id.as_str()),
        "the lineage survives"
    );
    let mut next = u;
    next.qe_review = Some(got);
    assert_eq!(resume_dir(&next), Some(dir.to_string_lossy().as_ref()));
}

/// (codex r1 on the code) The engine reads the ledger the way crew does: a record with no id is
/// refused, any truthy `deleted` drops a row, and a verdict with no valid timestamp does not bind.
#[test]
fn the_ledger_is_read_with_crews_record_rules() {
    let l = Ledger::new("shape");
    let (id, dir) = l.evidence(RUN, &now_iso());
    let run_file = l.root.join("runs").join(format!("{id}.json"));
    let mut rec: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&run_file).unwrap()).unwrap();
    rec.as_object_mut().unwrap().remove("id");
    std::fs::write(&run_file, rec.to_string()).unwrap();
    assert!(bind_evidence(&l.root, &dir.to_string_lossy(), RUN, 0, None)
        .unwrap_err()
        .contains("no record id"));
    rec["id"] = serde_json::json!(id);
    rec["deleted"] = serde_json::json!("1");
    std::fs::write(&run_file, rec.to_string()).unwrap();
    assert!(bind_evidence(&l.root, &dir.to_string_lossy(), RUN, 0, None)
        .unwrap_err()
        .contains("deleted"));
    // A nonced PASS without a valid created_at never binds.
    write_verdict(&l.root, &id, "PASS", Some("n"), RUN, "yesterday");
    assert!(bind_verdict(&l.root, &id, RUN, "n", "PASS")
        .unwrap_err()
        .contains("no valid created_at"));
}

/// (codex r1 on the code) A reviewer seat carries the launch roster's template for it, read off a
/// unit the roster seated there.
#[test]
fn a_candidate_carries_the_run_rosters_invocation() {
    let ledger = Path::new("/l/.wicked-qe");
    let s = session(&["claude", "pi", "opencode#2"], false);
    let mut other = creator(1, "opencode#2");
    other.role = PhaseRole::Neutral;
    other.stage = crate::domain::StageKind::Test;
    other.assigned_invocation = Some("opencode --model x run {PROMPT}".into());
    let units = vec![other, creator(2, "claude")];
    let got = candidates(&s, &units, &qe_unit("pi", ledger));
    let oc = got
        .iter()
        .find(|c| c.seat == "opencode#2")
        .expect("opencode#2");
    assert_eq!(
        oc.invocation.as_deref(),
        Some("opencode --model x run {PROMPT}")
    );
}

/// (codex r1/r2 on the code) A reviewer turn's hook writes its OWN decisions log, under its
/// attempt's dir, keyed by its nonce — never the executor's.
#[test]
fn the_reviewer_turns_decisions_log_is_its_own() {
    let ledger = Path::new("/l/.wicked-qe");
    let executor = input_for(qe_unit("pi", ledger));
    let reviewer = reviewer_input(&executor, &seat("claude", BASIS_DISTINCT), "/d", "n-9");
    let exec_log = crate::gate_hook::decisions_path_for_input(&executor);
    let rev_log = crate::gate_hook::decisions_path_for_input(&reviewer);
    assert_eq!(exec_log, crate::gate_hook::decisions_path_for(RUN, 1));
    assert_eq!(
        rev_log,
        crate::gate_hook::qe_review_decisions_path(RUN, 1, "n-9")
    );
    assert_ne!(exec_log, rev_log);
    assert!(rev_log.starts_with(exec_log.parent().unwrap()));
}
