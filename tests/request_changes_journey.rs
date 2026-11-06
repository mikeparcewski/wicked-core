//! DES-L1 PR-1B — the `request_changes` journey through the REAL engine (`Core`, stub seats, no
//! repo, temp store): a two-phase def (build: creator → review: evaluator) whose review writes
//! `VERDICT: FAIL` first. The fold denies INTO the escalation gate (`verdict_not_pass` ×
//! `evaluator_verdict`, PR-1A); `RequestChanges` rewinds to `build`, which re-runs at attempt 1
//! with the rejected review in its prior-context block; the review re-runs at ITS attempt 1 (its
//! own fresh key — no phase-id collision, DES §7 (9)), writes `VERDICT: PASS`, and the run
//! completes. Never `sessionFailed`, never cancelled.
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wicked_core::{
    AmendScope, Core, CoreEvent, EntityMode, HumanConfirm, HumanDecision, LaunchSpec, PhaseRole,
    StepInput, StepOutput, StepRunner, StepStatus,
};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

/// Arm the hermetic emit spool BEFORE main (pre-main is single-threaded, so no test thread can
/// race it): engine paths under test fire coarse fire-and-forget `wicked.*` emissions, and with
/// no shared store configured those spool — which must land in a per-process temp file, never in
/// the operator's real emit-outbox replay queue under the wicked apps home. Every
/// binary in this suite carries this block; `harness_hygiene.rs` fails the suite if one is missing.
///
/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only sets one process env var
/// via the std API — no allocator setup, no threads, no panics across the FFI boundary.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}

fn db_path(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!("wicked-core-rcj-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("estate.db").to_str().unwrap().to_string()
}

fn cli(key: &str) -> AgenticCli {
    AgenticCli {
        key: key.into(),
        display_name: key.into(),
        binary: "unused".into(),
        headless_invocation: "unused {PROMPT}".into(),
        category: Category::default(),
        input_mode: InputMode::default(),
        version_probe: vec![],
        trust_flags: vec![],
        alt_binaries: vec![],
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

/// Votes "1" — resolves to the first CLI (council routing).
struct NumericDispatcher;
impl Dispatcher for NumericDispatcher {
    fn dispatch(&self, c: &AgenticCli, _: &CouncilTask) -> Option<Vote> {
        Some(Vote {
            cli: c.key.clone(),
            recommendation: "1".into(),
            top_risk: "none".into(),
            change_my_mind: "no".into(),
            disqualifier: None,
            confidence: Confidence::default(),
            provenance: "numeric".into(),
        })
    }
}

type Handed = Arc<Mutex<Vec<(u32, u32, Vec<String>)>>>;

/// The review seat FAILS its first review and PASSES its second; every seat records `(ord,
/// attempt, prior-context labels)` it was handed. The engine's judge (`validator`) is answered
/// with a well-formed PASS in case one is convened.
struct ScriptedSeat {
    reviews: Mutex<u32>,
    handed: Handed,
}
impl StepRunner for ScriptedSeat {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        if i.unit.session_id != "validator" {
            self.handed.lock().unwrap().push((
                i.unit.ord,
                i.attempt,
                i.prior_outputs.iter().map(|p| p.label.clone()).collect(),
            ));
        }
        let output = if i.unit.session_id == "validator" {
            "PASS\nthe work meets the criterion\nPASS".to_string()
        } else if i.unit.role == PhaseRole::Evaluator {
            let mut n = self.reviews.lock().unwrap();
            *n += 1;
            if *n == 1 {
                "the build has no regression test for the reported bug\nVERDICT: FAIL".to_string()
            } else {
                "the regression test is present and passes\nVERDICT: PASS".to_string()
            }
        } else {
            format!("built it (attempt {})", i.attempt)
        };
        StepOutput {
            run_id: i.run_id.clone(),
            unit_ix: i.unit_ix,
            attempt: i.attempt,
            output,
            status: StepStatus::Ok,
            usage: None,
            files: Vec::new(),
            tools: Vec::new(),
            governed: false,
        }
    }
}

const DEF: &str = r#"{
    "id": "rc-journey",
    "phases": [
        { "id": "build", "kind": "build", "gate": "auto", "role": "creator" },
        { "id": "review", "kind": "review", "gate": "auto", "role": "evaluator", "depends_on": ["build"] }
    ]
}"#;

/// Drain until `stop` matches (or 20 s), returning everything seen for `session`.
fn drain_until(
    ev: &std::sync::mpsc::Receiver<CoreEvent>,
    session: &str,
    stop: impl Fn(&CoreEvent) -> bool,
) -> Vec<CoreEvent> {
    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        match ev.recv_timeout(Duration::from_millis(200)) {
            Ok(e) => {
                let mine = e.to_json()["session"].as_str() == Some(session);
                if mine {
                    let done = stop(&e);
                    seen.push(e);
                    if done {
                        return seen;
                    }
                }
            }
            Err(_) => continue,
        }
    }
    panic!("timed out waiting for the stop event; saw: {seen:?}");
}

#[test]
fn a_failed_review_is_sent_back_to_the_creator_and_the_run_completes_on_the_second_pass() {
    let handed: Handed = Arc::new(Mutex::new(Vec::new()));
    let core = Core::spawn_with_engine(
        db_path("journey"),
        Arc::new(NumericDispatcher),
        Arc::new(ScriptedSeat {
            reviews: Mutex::new(0),
            handed: Arc::clone(&handed),
        }),
    );
    core.register_workflow(DEF)
        .expect("register the two-phase def");
    let ev = core.subscribe();
    let run = "rcj-run";
    core.launch_run(LaunchSpec {
        base_ref: None,
        project_id: None,
        problem: "fix the reported bug".into(),
        clis: vec![cli("a"), cli("b")],
        entity_mode: EntityMode::Shared,
        session_id: run.into(),
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        repo_ref: None,
        workflow: Some("rc-journey".into()),
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
        plan: None,
        deliver_step: None,
        exclude_seats: Vec::new(),
        evidence_root: None,
        primary: None,
        reduced_assurance: false,
        power: false,
        voice: None,
        deliverables: Vec::new(),
        qe_acceptance: Default::default(),
    })
    .expect("launch");

    // ── 1. The FAIL review parks the run at the escalation gate (PR-1A), never sessionFailed. ──
    let first = drain_until(&ev, run, |e| {
        matches!(
            e,
            CoreEvent::AwaitingHuman { .. } | CoreEvent::SessionFailed { .. }
        )
    });
    assert!(
        first.iter().any(|e| matches!(e, CoreEvent::GateEvaluated { ord: 2, evaluator_verdict: Some(v), combined: false, .. } if v == "FAIL")),
        "the review's own FAIL is read: {first:?}"
    );
    assert!(
        first.iter().any(
            |e| matches!(e, CoreEvent::GateEscalated { ord: 2, condition, denial_source, .. }
            if condition == "verdict_not_pass" && denial_source == "evaluator_verdict")
        ),
        "{first:?}"
    );
    assert!(
        first.iter().any(
            |e| matches!(e, CoreEvent::AwaitingHuman { gate_kind, .. } if gate_kind == "escalation")
        ),
        "{first:?}"
    );
    assert!(!first
        .iter()
        .any(|e| matches!(e, CoreEvent::SessionFailed { .. })));

    // ── 2. Request changes → the creator re-runs with the review in context, the review re-runs
    //       at its own fresh attempt and passes, the run completes. ──
    core.confirm_gate(
        run,
        HumanDecision::RequestChanges {
            note: Some("add the regression test".into()),
        },
    )
    .expect("request changes");
    let second = drain_until(&ev, run, |e| {
        matches!(
            e,
            CoreEvent::SessionCompleted { .. }
                | CoreEvent::SessionFailed { .. }
                | CoreEvent::RunCancelled { .. }
        )
    });
    let pos = |pred: &dyn Fn(&CoreEvent) -> bool| second.iter().position(pred);
    let amended = pos(&|e| {
        matches!(e, CoreEvent::UnitReworkAmended { ord: 1, scope, amendment, .. }
            if scope == "request_changes"
                && amendment.contains("no regression test")
                && amendment.ends_with("add the regression test"))
    })
    .expect("unitReworkAmended{ord 1, request_changes} carries the findings + the note");
    let resumed = pos(&|e| matches!(e, CoreEvent::Resumed { ord: 1, .. })).expect("resumed{1}");
    let build_again = pos(&|e| {
        matches!(
            e,
            CoreEvent::UnitDispatched {
                ord: 1,
                attempt: 1,
                ..
            }
        )
    })
    .expect("the creator re-dispatches at attempt 1");
    let context = pos(&|e| {
        matches!(e, CoreEvent::UnitContextInjected { ord: 1, prior_units, .. }
            if prior_units.iter().any(|p| p.ord == 2 && p.label.contains("requested changes")))
    })
    .expect("the rejected review rides the creator's prior-context block");
    let review_again = pos(&|e| {
        matches!(
            e,
            CoreEvent::UnitDispatched {
                ord: 2,
                attempt: 1,
                ..
            }
        )
    })
    .expect("the review re-dispatches at ITS attempt 1 — no phase-id collision (DES §7 (9))");
    let passed = pos(&|e| {
        matches!(e, CoreEvent::GateEvaluated { ord: 2, evaluator_verdict: Some(v), combined: true, .. } if v == "PASS")
    })
    .expect("the second review passes on its own verdict");
    let completed =
        pos(&|e| matches!(e, CoreEvent::SessionCompleted { .. })).expect("sessionCompleted");
    assert!(
        amended < resumed
            && resumed < build_again
            && build_again < review_again
            && review_again < passed
            && passed < completed,
        "{second:?}"
    );
    assert!(context > resumed && context < review_again, "{second:?}");
    assert!(!second.iter().any(|e| matches!(
        e,
        CoreEvent::SessionFailed { .. } | CoreEvent::RunCancelled { .. }
    )));

    // The seat's own record: build ran at attempts 0 and 1; only the re-run was handed the review.
    let handed = handed.lock().unwrap().clone();
    let builds: Vec<_> = handed.iter().filter(|(ord, _, _)| *ord == 1).collect();
    assert_eq!(
        builds.iter().map(|(_, a, _)| *a).collect::<Vec<_>>(),
        vec![0, 1],
        "{handed:?}"
    );
    assert!(builds[0].2.iter().all(|l| !l.contains("requested changes")));
    assert!(
        builds[1]
            .2
            .iter()
            .any(|l| l.contains("unit 2") && l.contains("requested changes")),
        "{handed:?}"
    );
    let reviews: Vec<_> = handed
        .iter()
        .filter(|(ord, _, _)| *ord == 2)
        .map(|(_, a, _)| *a)
        .collect();
    assert_eq!(reviews, vec![0, 1]);
}

/// (e2e F30) A creator then a Tool CHECK (qe-author-tests' `build` → `verify` shape). The check
/// exits 1 until the creator's second pass writes `flag`; the triage judge rules `fail`. Records
/// every `(ord, attempt)` a seat ran, and every triage call.
struct FailingCheckSeat {
    flag: std::path::PathBuf,
    handed: Arc<Mutex<Vec<(u32, u32)>>>,
    triaged: Arc<Mutex<u32>>,
}
impl StepRunner for FailingCheckSeat {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        let output = if i.run_id.starts_with("triage-") {
            *self.triaged.lock().unwrap() += 1;
            "DECISION: FAIL\n2 of the produced tests fail (fixture verdict)".to_string()
        } else if i.unit.session_id == "validator" {
            "PASS\nthe work meets the criterion\nPASS".to_string()
        } else {
            self.handed.lock().unwrap().push((i.unit.ord, i.attempt));
            if i.attempt >= 1 {
                std::fs::write(&self.flag, "fixed").unwrap();
            }
            format!("wrote the tests (attempt {})", i.attempt)
        };
        StepOutput {
            run_id: i.run_id.clone(),
            unit_ix: i.unit_ix,
            attempt: i.attempt,
            output,
            status: StepStatus::Ok,
            usage: None,
            files: Vec::new(),
            tools: Vec::new(),
            governed: false,
        }
    }
}

/// The `author` → `verify` def, its check logging `pass`/`fail` to `log` on every run, launched with
/// a person present. Returns the core, its event stream and the seat's records.
#[cfg(unix)]
#[allow(clippy::type_complexity)]
fn launch_tool_check(
    name: &str,
    run: &str,
) -> (
    Core,
    std::sync::mpsc::Receiver<CoreEvent>,
    Arc<Mutex<Vec<(u32, u32)>>>,
    Arc<Mutex<u32>>,
    std::path::PathBuf,
) {
    let db = db_path(name);
    let flag = std::path::Path::new(&db).with_file_name("creator-fixed");
    let log = std::path::Path::new(&db).with_file_name("check.log");
    let handed = Arc::new(Mutex::new(Vec::new()));
    let triaged = Arc::new(Mutex::new(0));
    let core = Core::spawn_with_engine(
        db,
        Arc::new(NumericDispatcher),
        Arc::new(FailingCheckSeat {
            flag: flag.clone(),
            handed: Arc::clone(&handed),
            triaged: Arc::clone(&triaged),
        }),
    );
    let script = format!(
        "if [ -f '{}' ]; then echo pass >> '{log}'; echo 'qe-verify: PASS'; exit 0; fi; echo fail \
         >> '{log}'; echo 'qe-verify: FAIL — 2 of 5 tests failed'; exit 1",
        flag.display(),
        log = log.display()
    );
    core.register_workflow(
        serde_json::json!({
            "id": "tool-check",
            "phases": [
                { "id": "author", "kind": "build", "gate": "auto", "role": "creator" },
                { "id": "verify", "kind": "test", "depends_on": ["author"],
                  "executor": { "type": "tool", "cmd": ["sh", "-c", script] } }
            ]
        })
        .to_string(),
    )
    .expect("register the def");
    let ev = core.subscribe();
    core.launch_run(LaunchSpec {
        base_ref: None,
        project_id: None,
        problem: "write tests for clamp".into(),
        clis: vec![cli("a"), cli("b")],
        entity_mode: EntityMode::Shared,
        session_id: run.into(),
        // A person is present (the failure triages), but no gate ord matches (nothing else pauses).
        human_confirm: HumanConfirm::Before(99),
        auto_deliver: false,
        repo_ref: None,
        workflow: Some("tool-check".into()),
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
        plan: None,
        deliver_step: None,
        exclude_seats: Vec::new(),
        evidence_root: None,
        primary: None,
        reduced_assurance: false,
        power: false,
        voice: None,
        deliverables: Vec::new(),
        qe_acceptance: Default::default(),
    })
    .expect("launch");
    (core, ev, handed, triaged, log)
}

#[cfg(unix)]
#[test]
fn a_failed_tool_check_ruled_fail_parks_and_send_back_reworks_the_creator() {
    let run = "tool-check-run";
    let (core, ev, handed, triaged, _log) = launch_tool_check("tool-check", run);

    let parked = |seen: &[CoreEvent]| {
        assert!(
            seen.iter().any(|e| matches!(e, CoreEvent::FailureTriaged { ord: 2, decision, .. } if decision == "escalate")),
            "the judge's fail on the check becomes an escalation: {seen:?}"
        );
        assert!(
            seen.iter().any(
                |e| matches!(e, CoreEvent::AwaitingHuman { ord: 2, gate_kind, prompt, .. }
                if gate_kind == "triage"
                    && prompt.starts_with("Unit 2 failed and triage escalated")
                    && prompt.contains("request changes to send the work back to unit 1"))
            ),
            "the check parks at the triage gate offering the send-back: {seen:?}"
        );
        assert!(!seen.iter().any(|e| matches!(
            e,
            CoreEvent::SessionFailed { .. } | CoreEvent::SessionCompleted { .. }
        )));
    };
    // ── 1. The check fails, the judge rules fail, the run PARKS (it used to end FAILED). ──
    let first = drain_until(&ev, run, |e| {
        matches!(
            e,
            CoreEvent::AwaitingHuman { .. } | CoreEvent::SessionFailed { .. }
        )
    });
    parked(&first);
    assert_eq!(
        *triaged.lock().unwrap(),
        1,
        "the triage judge actually ruled"
    );

    // ── 2. No answer skips the check: an operator judgment is refused here (it answers a failed
    //       repo-checks floor only), and approve runs the same check again, which parks again. ──
    assert!(core
        .confirm_gate(
            run,
            HumanDecision::OperatorJudgment {
                rationale: "these fail on main too".into()
            }
        )
        .is_err());
    core.confirm_gate(
        run,
        HumanDecision::Approve {
            amend: None,
            amend_scope: AmendScope::Cursor,
        },
    )
    .expect("approve re-runs the check");
    let again = drain_until(&ev, run, |e| {
        matches!(
            e,
            CoreEvent::AwaitingHuman { .. } | CoreEvent::SessionFailed { .. }
        )
    });
    parked(&again);
    assert_eq!(*triaged.lock().unwrap(), 2);

    // ── 3. Send back → the creator reworks (and fixes it), the check passes, the run completes. ──
    core.confirm_gate(
        run,
        HumanDecision::RequestChanges {
            note: Some("make the new tests pass".into()),
        },
    )
    .expect("request changes");
    let done = drain_until(&ev, run, |e| {
        matches!(
            e,
            CoreEvent::SessionCompleted { .. }
                | CoreEvent::SessionFailed { .. }
                | CoreEvent::RunCancelled { .. }
        )
    });
    assert!(
        done.iter().any(
            |e| matches!(e, CoreEvent::UnitReworkAmended { ord: 1, scope, amendment, .. }
            if scope == "request_changes" && amendment.contains("make the new tests pass"))
        ),
        "the send-back reworks the creator, not the check: {done:?}"
    );
    assert!(
        done.iter()
            .any(|e| matches!(e, CoreEvent::SessionCompleted { .. })),
        "{done:?}"
    );
    // The passing check is not re-judged by triage (a passing tool step is unchanged).
    assert_eq!(*triaged.lock().unwrap(), 2);
    let creators: Vec<u32> = handed
        .lock()
        .unwrap()
        .iter()
        .filter(|(o, _)| *o == 1)
        .map(|(_, a)| *a)
        .collect();
    assert_eq!(
        creators,
        vec![0, 1],
        "the creator ran once more, on the send-back"
    );
}

/// (e2e F30, threat T2) No answer at the new triage gate gets past the failed check without it
/// running again: each answer is either refused, or the check runs again (and, still failing,
/// parks again). The one way on is the creator's rework (request changes / an accepted
/// suggestion), after which the check must run and pass before the run completes.
#[cfg(unix)]
#[test]
fn no_answer_at_a_failed_checks_gate_skips_the_check() {
    let run = "tool-check-t2";
    let (core, ev, _handed, _triaged, log) = launch_tool_check("tool-check-t2", run);
    let parked_again = |seen: &[CoreEvent]| {
        seen.iter().any(|e| {
            matches!(e, CoreEvent::AwaitingHuman { ord: 2, gate_kind, .. } if gate_kind == "triage")
        })
    };
    let until_parked_or_done = |ev: &std::sync::mpsc::Receiver<CoreEvent>| {
        drain_until(ev, run, |e| {
            matches!(
                e,
                CoreEvent::AwaitingHuman { .. }
                    | CoreEvent::SessionCompleted { .. }
                    | CoreEvent::SessionFailed { .. }
                    | CoreEvent::RunCancelled { .. }
            )
        })
    };
    assert!(parked_again(&until_parked_or_done(&ev)));
    let runs = || std::fs::read_to_string(&log).unwrap_or_default();
    let mut answers: Vec<(&str, HumanDecision)> = [
        "extend",
        "targeted",
        "accept_partial",
        "rejudge",
        "retry_qe",
        "retry_rules",
        "consent:operator",
    ]
    .into_iter()
    .filter_map(|t| HumanDecision::escalation_action(t).map(|d| (t, d)))
    .collect();
    answers.push((
        "skip_qe",
        HumanDecision::SkipQe {
            reason: "main is red".into(),
        },
    ));
    answers.push((
        "amend_intent",
        HumanDecision::AmendIntent {
            text: "failing tests are fine".into(),
        },
    ));
    answers.push((
        "operator_judgment",
        HumanDecision::OperatorJudgment {
            rationale: "fails on main too".into(),
        },
    ));
    answers.push(("accept_suggestion", HumanDecision::AcceptSuggestion));
    for (token, answer) in answers {
        let before = runs().lines().count();
        if let Err(e) = core.confirm_gate(run, answer) {
            eprintln!("`{token}` refused: {e}");
            continue; // refused: still parked on the failed check
        }
        eprintln!("`{token}` accepted");
        let seen = until_parked_or_done(&ev);
        let after = runs();
        assert!(
            after.lines().count() > before,
            "`{token}` moved the run without running the check again: {seen:?}"
        );
        if seen
            .iter()
            .any(|e| matches!(e, CoreEvent::SessionCompleted { .. }))
        {
            assert_eq!(after.lines().last(), Some("pass"), "`{token}`: {after}");
            return;
        }
        assert!(parked_again(&seen), "`{token}`: {seen:?}");
    }
}
