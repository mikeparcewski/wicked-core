//! DES-L1 PR-1C (core #465) — amendment SCOPING through the REAL engine (`Core`, stub seats, no
//! repo, temp store): a bug-shaped def (triage → reproduce → fix → verify) pauses at triage's
//! def gate — the cursor is then the read-only `reproduce`, the #465 shape; the operator approves
//! with `amendScope: creator` and the steer "Implement X". The steer must land on `fix` ONLY: the
//! read-only `reproduce` unit is dispatched with its own description (triage already ran), no
//! unit's prior-context block carries the steer text
//! (the engine passes OUTPUTS, never descriptions), `unitReworkAmended{ord: fix, scope: creator}`
//! fires exactly once, and the run completes. The control below pins today's default: without a
//! scope the same steer lands on the cursor (`scope: cursor`) — the shape #465 reported.
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wicked_core::{
    AmendScope, Core, CoreEvent, EntityMode, HumanConfirm, HumanDecision, LaunchSpec, PhaseRole,
    StepInput, StepOutput, StepRunner, StepStatus,
};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

// ── Test-harness hygiene (core#311) — not a test ─────────────────────────────────────────────
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
    let dir = std::env::temp_dir().join(format!("wicked-core-asj-{name}-{}", std::process::id()));
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

/// What a seat was handed: `(ord, description, prior-context outputs)`.
type Handed = Arc<Mutex<Vec<(u32, String, Vec<String>)>>>;

/// Records every dispatch; the Evaluator answers the verdict contract; nobody echoes its prompt.
struct RecordingSeat(Handed);
impl StepRunner for RecordingSeat {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        if i.unit.session_id != "validator" {
            self.0.lock().unwrap().push((
                i.unit.ord,
                i.unit.description.clone(),
                i.prior_outputs.iter().map(|p| p.output.clone()).collect(),
            ));
        }
        let output = if i.unit.session_id == "validator" {
            "PASS\nthe work meets the criterion\nPASS".to_string()
        } else if i.unit.role == PhaseRole::Evaluator {
            "reviewed the change\nVERDICT: PASS".to_string()
        } else {
            format!("did phase {}", i.unit.ord)
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

/// Bug-shaped: triage carries the def's own human gate (it pauses under `human_confirm: none`,
/// after triage's output, with the cursor on `reproduce`); declared handoffs so every later unit
/// is handed the prior's output as context.
const DEF: &str = r#"{
    "id": "asj",
    "phases": [
        { "id": "triage", "kind": "recon", "gate": {"human_confirm": {"unconditional": true}} },
        { "id": "reproduce", "kind": "test", "gate": "auto", "depends_on": ["triage"] },
        { "id": "fix", "kind": "build", "gate": "auto", "role": "creator", "depends_on": ["reproduce"] },
        { "id": "verify", "kind": "review", "gate": "auto", "role": "evaluator", "depends_on": ["fix"] }
    ]
}"#;

const STEER: &str = "Implement X: add the missing null check";

fn drain_until(
    ev: &std::sync::mpsc::Receiver<CoreEvent>,
    session: &str,
    stop: impl Fn(&CoreEvent) -> bool,
) -> Vec<CoreEvent> {
    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Ok(e) = ev.recv_timeout(Duration::from_millis(200)) {
            if e.to_json()["session"].as_str() == Some(session) {
                let done = stop(&e);
                seen.push(e);
                if done {
                    return seen;
                }
            }
        }
    }
    panic!("timed out waiting for the stop event; saw: {seen:?}");
}

fn launch(core: &Core, run: &str) -> std::sync::mpsc::Receiver<CoreEvent> {
    core.register_workflow(DEF)
        .expect("register the bug-shaped def");
    let ev = core.subscribe();
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
        workflow: Some("asj".into()),
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
        plan: None,
        deliver_step: None,
    })
    .expect("launch");
    drain_until(&ev, run, |e| matches!(e, CoreEvent::AwaitingHuman { .. }));
    ev
}

fn terminal(e: &CoreEvent) -> bool {
    matches!(
        e,
        CoreEvent::SessionCompleted { .. }
            | CoreEvent::SessionFailed { .. }
            | CoreEvent::RunCancelled { .. }
    )
}

#[test]
fn a_creator_scoped_steer_reaches_fix_only_and_no_prior_context_block() {
    let handed: Handed = Arc::new(Mutex::new(Vec::new()));
    let core = Core::spawn_with_engine(
        db_path("creator"),
        Arc::new(NumericDispatcher),
        Arc::new(RecordingSeat(Arc::clone(&handed))),
    );
    let run = "asj-creator";
    let ev = launch(&core, run);

    core.confirm_gate(
        run,
        HumanDecision::Approve {
            amend: Some(STEER.into()),
            amend_scope: AmendScope::Creator,
        },
    )
    .expect("approve triage's gate with a creator-scoped steer");
    let evs = drain_until(&ev, run, terminal);
    assert!(
        evs.iter()
            .any(|e| matches!(e, CoreEvent::SessionCompleted { .. })),
        "{evs:?}"
    );

    // The paper trail: exactly one amendment, on fix (ord 3), scope creator.
    let amended: Vec<_> = evs
        .iter()
        .filter_map(|e| match e {
            CoreEvent::UnitReworkAmended {
                ord,
                scope,
                amendment,
                ..
            } => Some((*ord, scope.clone(), amendment.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(amended, vec![(3, "creator".to_string(), STEER.to_string())]);

    // What the seats were handed: the steer is in fix's description and NOWHERE else — not in the
    // read-only units' descriptions, not in any unit's prior-context block (outputs only).
    let handed = handed.lock().unwrap().clone();
    let by_ord = |ord: u32| {
        handed
            .iter()
            .find(|(o, _, _)| *o == ord)
            .unwrap_or_else(|| panic!("ord {ord} ran: {handed:?}"))
    };
    // triage ran BEFORE the gate, so its "no steer" is trivially true (review-L1-stack LOW 2);
    // reproduce (the read-only cursor) and verify are the load-bearing assertions.
    assert!(
        !by_ord(1).1.contains("Implement X"),
        "triage: {}",
        by_ord(1).1
    );
    assert!(
        !by_ord(2).1.contains("Implement X"),
        "reproduce: {}",
        by_ord(2).1
    );
    assert!(
        by_ord(3)
            .1
            .ends_with(&format!(" (operator amendment: {STEER})")),
        "fix: {}",
        by_ord(3).1
    );
    assert!(
        !by_ord(4).1.contains("Implement X"),
        "verify: {}",
        by_ord(4).1
    );
    for (ord, _, priors) in &handed {
        assert!(
            priors.iter().all(|p| !p.contains("Implement X")),
            "unit {ord}'s prior-context block carries the steer: {priors:?}"
        );
    }
    // The declared handoffs still flow (reproduce saw triage's output; verify saw fix's).
    assert!(
        by_ord(2).2.iter().any(|p| p.contains("did phase 1")),
        "{:?}",
        by_ord(2).2
    );
    assert!(
        by_ord(4).2.iter().any(|p| p.contains("did phase 3")),
        "{:?}",
        by_ord(4).2
    );
}

/// Control — today's default (BC-04: absent `amendScope` = cursor): the same steer at the same
/// gate lands on `reproduce`, the read-only cursor, as `scope: cursor` — the #465 shape, kept as
/// the default by ruling; the studio sends `creator` whenever the cursor is not a creator.
#[test]
fn an_unscoped_steer_still_lands_on_the_read_only_cursor() {
    let handed: Handed = Arc::new(Mutex::new(Vec::new()));
    let core = Core::spawn_with_engine(
        db_path("cursor"),
        Arc::new(NumericDispatcher),
        Arc::new(RecordingSeat(Arc::clone(&handed))),
    );
    let run = "asj-cursor";
    let ev = launch(&core, run);
    core.confirm_gate(
        run,
        HumanDecision::Approve {
            amend: Some(STEER.into()),
            amend_scope: AmendScope::Cursor,
        },
    )
    .expect("approve triage's gate with an unscoped steer");
    let evs = drain_until(&ev, run, terminal);
    assert!(
        evs.iter()
            .any(|e| matches!(e, CoreEvent::SessionCompleted { .. })),
        "{evs:?}"
    );
    assert!(
        evs.iter().any(
            |e| matches!(e, CoreEvent::UnitReworkAmended { ord: 2, scope, .. } if scope == "cursor")
        ),
        "{evs:?}"
    );
    let handed = handed.lock().unwrap().clone();
    let cursor = handed.iter().find(|(o, _, _)| *o == 2).unwrap();
    assert!(
        cursor.1.contains("Implement X"),
        "the cursor (reproduce, read-only) carries the steer under the default scope"
    );
    let fix = handed.iter().find(|(o, _, _)| *o == 3).unwrap();
    assert!(
        !fix.1.contains("Implement X"),
        "and fix does not: {}",
        fix.1
    );
}
