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
    Core, CoreEvent, EntityMode, HumanConfirm, HumanDecision, LaunchSpec, PhaseRole, StepInput,
    StepOutput, StepRunner, StepStatus,
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
        acp: None,
        capabilities: None,
        login_invocation: None,
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
