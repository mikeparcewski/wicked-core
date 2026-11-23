//! DES-STANDIN-001 C3 (§3.2, §3.4, §3.8, §3.10, §5.1): stand-in review through the REAL engine
//! (`Core::launch_run` → plan → distribute → the `PlanFailed` arm → the gate → `grant_stand_ins`).
//!
//! The roster is one signed-in helper (`claude`) and a build→review plan whose contract requires a
//! distinct evaluator. On main that run cannot review independently: it is refused at distribution
//! and parks at `dead_seat` (T-B3 keeps that for every launch that is not `standInAware`). A
//! stand-in-aware launch instead reviews on a stand-in (`claude~1`) with consent (T-R4), parks at
//! `stand_in_needed` without it, and a grant there continues exactly the held check.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wicked_core::{
    get_session, Core, CoreEvent, EntityMode, HumanConfirm, HumanDecision, LaunchSpec,
    LaunchStandIn, SessionStatus, StandInConsent, StepInput, StepOutput, StepRunner, StepStatus,
};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

/// Pre-main: arm the hermetic emit spool (core#311). SAFETY (`ctor(unsafe)`): runs before `main`
/// on one thread and only sets process env vars via the std API.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}

fn db_path(name: &str) -> String {
    let dir =
        std::env::temp_dir().join(format!("wicked-core-standin-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("estate.db").to_str().unwrap().to_string()
}

fn cli(key: &str) -> AgenticCli {
    AgenticCli {
        key: key.into(),
        display_name: "Claude Code".into(),
        binary: "unused".into(),
        headless_invocation: format!("{key}-bin -p {{PROMPT}}"),
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

struct NoBallots;
impl Dispatcher for NoBallots {
    fn dispatch(&self, _c: &AgenticCli, _: &CouncilTask) -> Option<Vote> {
        None
    }
}

/// What each dispatched unit was handed: its seat, its template and its context labels.
#[derive(Debug, Clone)]
struct Seen {
    ord: u32,
    seat: Option<String>,
    invocation: Option<String>,
    labels: Vec<String>,
    texts: Vec<String>,
}

/// A marker only the first review's output carries: an earlier verdict a stand-in must not see.
const PRIOR_VERDICT_MARKER: &str = "PRIOR-VERDICT-MARKER-7f3a";
/// A marker only the build's output carries: the work, which every reviewer sees.
const BUILT_MARKER: &str = "BUILT-OUTPUT-MARKER-1c9e";

/// Records every dispatch; a review answers `VERDICT: PASS`.
struct Recorder(Arc<Mutex<Vec<Seen>>>);
impl StepRunner for Recorder {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        self.0.lock().unwrap().push(Seen {
            ord: i.unit.ord,
            seat: i.unit.assigned_cli.clone(),
            invocation: i.unit.assigned_invocation.clone(),
            labels: i.prior_outputs.iter().map(|p| p.label.clone()).collect(),
            texts: i.prior_outputs.iter().map(|p| p.output.clone()).collect(),
        });
        let marker = match i.unit.ord {
            1 => BUILT_MARKER,
            2 => PRIOR_VERDICT_MARKER,
            _ => "",
        };
        StepOutput {
            run_id: i.run_id.clone(),
            unit_ix: i.unit_ix,
            attempt: i.attempt,
            output: format!("did the work {marker}\nVERDICT: PASS"),
            status: StepStatus::Ok,
            usage: None,
            files: vec![],
            tools: Vec::new(),
            governed: false,
        }
    }
}

const BUILD_REVIEW: &str = r#"{"id":"standin-build-review","phases":[
  {"id":"build","kind":"build","gate":"auto"},
  {"id":"review","kind":"review","gate":"auto","depends_on":["build"]}]}"#;

fn consent() -> StandInConsent {
    StandInConsent {
        scope: "job".into(),
        by: "operator".into(),
        at: 1,
        via: "launch".into(),
    }
}

fn spec(sid: &str, stand_in: LaunchStandIn) -> LaunchSpec {
    LaunchSpec {
        base_ref: None,
        project_id: None,
        problem: "Build the thing.".into(),
        clis: vec![cli("claude")],
        entity_mode: EntityMode::Shared,
        session_id: sid.into(),
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        repo_ref: None,
        workflow: Some("standin-build-review".into()),
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
        plan: None,
        deliver_step: None,
        exclude_seats: Vec::new(),
        evidence_root: None,
        primary: None,
        reduced_assurance: false,
        stand_in,
        power: false,
        voice: None,
        deliverables: Vec::new(),
        qe_acceptance: Default::default(),
    }
}

fn aware(consent: Option<StandInConsent>) -> LaunchStandIn {
    LaunchStandIn {
        aware: true,
        parties: Vec::new(),
        consent,
    }
}

fn engine(name: &str) -> (Core, Arc<Mutex<Vec<Seen>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let core = Core::spawn_with_engine(
        db_path(name).clone(),
        Arc::new(NoBallots),
        Arc::new(Recorder(seen.clone())),
    );
    core.register_workflow(BUILD_REVIEW)
        .expect("register the build→review def");
    (core, seen)
}

/// The run's session as the store holds it (read-only).
fn session(name: &str, sid: &str) -> Option<wicked_core::AgentSession> {
    let dir =
        std::env::temp_dir().join(format!("wicked-core-standin-{name}-{}", std::process::id()));
    let db = dir.join("estate.db");
    let store = wicked_apps_core::open_store_ro(Some(db.to_str().unwrap())).expect("store");
    get_session(&store, sid).unwrap()
}

fn collect_until(
    events: &std::sync::mpsc::Receiver<CoreEvent>,
    within: Duration,
    until: impl Fn(&CoreEvent) -> bool,
) -> Vec<CoreEvent> {
    let mut out = Vec::new();
    let deadline = Instant::now() + within;
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match events.recv_timeout(remaining.min(Duration::from_millis(200))) {
            Ok(ev) => {
                let done = until(&ev);
                out.push(ev);
                if done {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
        }
    }
    out
}

fn ends(sid: &str) -> impl Fn(&CoreEvent) -> bool + '_ {
    move |e| {
        matches!(e, CoreEvent::SessionCompleted { session } if session == sid)
            || matches!(e, CoreEvent::SessionFailed { session, .. } if session == sid)
            || matches!(e, CoreEvent::AwaitingHuman { session, .. } if session == sid)
    }
}

/// The review's gate receipt (`gateEvaluated.assurance` on the review's ord) as JSON.
fn review_receipt(evs: &[CoreEvent], sid: &str) -> serde_json::Value {
    evs.iter()
        .filter_map(|e| match e {
            CoreEvent::GateEvaluated {
                session,
                ord,
                assurance,
                ..
            } if session == sid && *ord == 2 => Some(serde_json::to_value(assurance).unwrap()),
            _ => None,
        })
        .next_back()
        .unwrap_or_else(|| panic!("the review's gateEvaluated: {evs:?}"))
}

/// T-R4 (with consent): the review is seated on the stand-in `claude~1`, announced
/// (`standInSeated`) and distributed `stand_in` before it runs, runs on claude's own template with
/// the stand-in preface first in its context, and its receipt is canonical (`roles`,
/// `independence: stand_in`) with the conservative legacy projection.
#[test]
fn a_one_seat_aware_run_with_consent_reviews_on_a_stand_in() {
    let sid = "standin-consent";
    let (core, seen) = engine("consent");
    let ev = core.subscribe();
    core.launch_run(spec(sid, aware(Some(consent()))))
        .expect("an aware launch with consent is admitted");
    let evs = collect_until(&ev, Duration::from_secs(30), ends(sid));
    assert!(
        evs.iter()
            .any(|e| matches!(e, CoreEvent::SessionCompleted { session } if session == sid)),
        "the run completes on its stand-in review: {evs:?}"
    );
    let seated = evs
        .iter()
        .find_map(|e| match e {
            CoreEvent::StandInSeated {
                session, binding, ..
            } if session == sid => Some(binding.clone()),
            _ => None,
        })
        .expect("standInSeated on the wire");
    assert_eq!(seated.key, "claude~1");
    assert_eq!(seated.base, "claude");
    let distributed = evs
        .iter()
        .find_map(|e| match e {
            CoreEvent::UnitDistributed {
                session,
                ord: 2,
                cli,
                routing_method,
                distinctness_fallback,
                ..
            } if session == sid => Some((
                cli.clone(),
                routing_method.clone(),
                distinctness_fallback.clone(),
            )),
            _ => None,
        })
        .expect("the review's unitDistributed");
    assert_eq!(
        distributed,
        ("claude~1".to_string(), "stand_in".to_string(), None)
    );
    let seen = seen.lock().unwrap().clone();
    let review = seen.iter().find(|s| s.ord == 2).expect("the review ran");
    assert_eq!(review.seat.as_deref(), Some("claude~1"));
    assert_eq!(
        review.invocation.as_deref(),
        Some("claude-bin -p {PROMPT}"),
        "a stand-in runs its base's own template: {review:?}"
    );
    assert_eq!(
        review.labels.first().map(String::as_str),
        Some("[stand-in reviewer — read this first]"),
        "the stand-in preface leads its context: {review:?}"
    );
    let build = seen.iter().find(|s| s.ord == 1).expect("the build ran");
    assert_eq!(build.seat.as_deref(), Some("claude"));

    let r = review_receipt(&evs, sid);
    assert_eq!(r["independence"], "stand_in", "{r}");
    assert_eq!(r["roles"][0]["role"], "evaluator", "{r}");
    assert_eq!(r["roles"][0]["filled"], "stand_in", "{r}");
    assert_eq!(r["roles"][0]["state"], "answered", "{r}");
    assert_eq!(r["roles"][0]["seat"], "claude~1", "{r}");
    // The conservative legacy projection: never ran, a null evaluator, skipped `stand_in`.
    assert!(
        !r["ran"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i == "distinct_evaluator"),
        "{r}"
    );
    assert!(r["evaluator"].is_null(), "{r}");
    let skip = r["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["instrument"] == "distinct_evaluator")
        .unwrap_or_else(|| panic!("distinct_evaluator skipped: {r}"));
    assert_eq!(skip["reason"], "stand_in", "{r}");
    assert!(
        skip["detail"]
            .as_str()
            .unwrap()
            .contains("Claude Code stood in for the review"),
        "{r}"
    );

    let session = session("consent", sid).expect("session");
    assert_eq!(session.assurance.stand_ins.len(), 1);
    assert_eq!(session.assurance.stand_ins[0].key, "claude~1");
}

/// T-R4 (no consent) + §3.10: the run parks at `stand_in_needed` (never `dead_seat`, never a
/// self-review); the gate lists approve, `use_stand_ins` (once, note none) and reject; a retry with
/// still no helper re-opens the gate; a grant puts the consent on the contract and the held review
/// runs on a stand-in.
#[test]
fn without_consent_the_run_parks_at_stand_in_needed_and_a_grant_continues() {
    let sid = "standin-grant";
    let (core, seen) = engine("grant");
    let ev = core.subscribe();
    core.launch_run(spec(sid, aware(None)))
        .expect("an aware launch without consent is admitted");
    let evs = collect_until(&ev, Duration::from_secs(30), ends(sid));
    let condition = evs
        .iter()
        .find_map(|e| match e {
            CoreEvent::GateEscalated {
                session, condition, ..
            } if session == sid => Some(condition.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("gateEscalated: {evs:?}"));
    assert_eq!(condition, "stand_in_needed");
    assert!(
        seen.lock().unwrap().is_empty(),
        "nothing ran before consent"
    );

    let listed = core.gate_actions(sid).unwrap().expect("a gate is open");
    let tokens: Vec<(String, String)> = listed
        .actions
        .iter()
        .map(|a| (a.action.clone(), a.intent.clone()))
        .collect();
    assert_eq!(
        tokens,
        vec![
            ("approve".to_string(), "retry".to_string()),
            ("use_stand_ins".to_string(), "use_stand_ins".to_string()),
            ("reject".to_string(), "stop".to_string()),
        ],
        "{listed:?}"
    );

    // A retry with still no second helper and no consent: the gate opens again.
    let status = core
        .confirm_gate(
            sid,
            HumanDecision::Approve {
                amend: None,
                amend_scope: Default::default(),
            },
        )
        .expect("a retry is taken");
    assert_eq!(status, SessionStatus::AwaitingHuman);
    assert!(seen.lock().unwrap().is_empty(), "a retry ran nothing");

    // An answer the gate does not take is refused before the row resolves.
    assert!(core
        .confirm_gate(
            sid,
            HumanDecision::RequestChanges {
                note: Some("x".into())
            }
        )
        .is_err());

    // T-R12: a grant names the opening it answers. The first opening is gone (the retry re-opened
    // the gate): a grant for it is refused, and the current one is taken.
    let first = listed.gate_seq.expect("the open row's gate_seq");
    let now = core
        .gate_actions(sid)
        .unwrap()
        .expect("the gate re-opened")
        .gate_seq
        .expect("gate_seq");
    assert_ne!(first, now, "the retry is another opening");
    let stale = core
        .grant_stand_ins(sid, "job", Some("operator".into()), first, None)
        .expect_err("a grant for an earlier opening is refused");
    assert!(stale.to_string().starts_with("gate_changed"), "{stale:#}");
    // Read before the grant: the grant dispatches on the actor, so the build may start before
    // `grant_stand_ins` returns.
    let built_before = seen.lock().unwrap().iter().filter(|s| s.ord == 1).count();
    core.grant_stand_ins(sid, "job", Some("operator".into()), now, None)
        .expect("the grant is taken");
    let evs = collect_until(
        &ev,
        Duration::from_secs(30),
        |e| matches!(e, CoreEvent::SessionCompleted { session } if session == sid),
    );
    assert!(
        evs.iter().any(|e| matches!(
            e,
            CoreEvent::StandInsGranted { session, consent, .. }
                if session == sid && consent.via == "grant" && consent.scope == "job"
        )),
        "{evs:?}"
    );
    assert!(
        evs.iter()
            .any(|e| matches!(e, CoreEvent::SessionCompleted { session } if session == sid)),
        "the run completes after the grant: {evs:?}"
    );
    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen.iter()
            .find(|s| s.ord == 2)
            .and_then(|s| s.seat.clone())
            .as_deref(),
        Some("claude~1")
    );
    // The grant continued exactly the held check: the build ran once, never again.
    assert_eq!(built_before, 0, "nothing ran before the grant");
    assert_eq!(seen.iter().filter(|s| s.ord == 1).count(), 1, "{seen:?}");
    assert_eq!(seen.iter().filter(|s| s.ord == 2).count(), 1, "{seen:?}");
}

const BUILD_TWO_REVIEWS: &str = r#"{"id":"standin-two-reviews","phases":[
  {"id":"build","kind":"build","gate":"auto"},
  {"id":"review","kind":"review","gate":"auto","depends_on":["build"]},
  {"id":"recheck","kind":"review","gate":"auto","depends_on":["build","review"]}]}"#;

/// The second review's handed context (labels and texts), from a run of [`BUILD_TWO_REVIEWS`].
fn second_review_context(name: &str, clis: Vec<AgenticCli>, stand_in: LaunchStandIn) -> Seen {
    let sid = format!("standin-{name}");
    let (core, seen) = engine(name);
    core.register_workflow(BUILD_TWO_REVIEWS)
        .expect("register the two-review def");
    let ev = core.subscribe();
    let mut spec = spec(&sid, stand_in);
    spec.clis = clis;
    spec.workflow = Some("standin-two-reviews".into());
    core.launch_run(spec).expect("the launch is admitted");
    let evs = collect_until(&ev, Duration::from_secs(30), ends(&sid));
    assert!(
        evs.iter()
            .any(|e| matches!(e, CoreEvent::SessionCompleted { session } if *session == sid)),
        "the run completes: {evs:?}"
    );
    let seen = seen.lock().unwrap().clone();
    seen.iter()
        .find(|s| s.ord == 3)
        .cloned()
        .unwrap_or_else(|| panic!("the second review ran: {seen:?}"))
}

/// T-R6 (codex r1 T1): a stand-in is handed the work and never an earlier verdict, through the
/// real carrier (`StepInput.prior_outputs`, what the runner renders into the prompt). The control
/// run proves the marker does reach a real seat by the same route, so the check can fail.
#[test]
fn a_stand_in_is_handed_the_work_and_never_an_earlier_verdict() {
    // Control: two helpers, not aware: the real second review is handed the first review's
    // verdict as its declared dependency.
    let real = second_review_context(
        "fresh-control",
        vec![cli("claude"), cli("codex")],
        wicked_core::LaunchStandIn::default(),
    );
    assert_ne!(real.seat.as_deref(), Some("claude"), "{real:?}");
    assert!(
        real.texts.iter().any(|t| t.contains(PRIOR_VERDICT_MARKER)),
        "a real reviewer is handed the earlier review: {real:?}"
    );
    // One helper, aware, with consent: the second review stands in and is handed the work only.
    let stand_in = second_review_context("fresh", vec![cli("claude")], aware(Some(consent())));
    assert!(
        stand_in
            .seat
            .as_deref()
            .is_some_and(|s| s.starts_with("claude~")),
        "{stand_in:?}"
    );
    assert_eq!(
        stand_in.labels.first().map(String::as_str),
        Some("[stand-in reviewer — read this first]"),
        "{stand_in:?}"
    );
    assert!(
        stand_in.texts.iter().any(|t| t.contains(BUILT_MARKER)),
        "the stand-in is handed the work: {stand_in:?}"
    );
    assert!(
        !stand_in
            .texts
            .iter()
            .any(|t| t.contains(PRIOR_VERDICT_MARKER)),
        "the stand-in is never handed an earlier verdict: {stand_in:?}"
    );
}

/// T-B3: a launch WITHOUT `standInAware` behaves as on main — the review has no distinct seat and
/// the run parks at `dead_seat` — and no stand-in exists anywhere.
#[test]
fn an_unaware_launch_parks_at_dead_seat_as_on_main() {
    let sid = "standin-unaware";
    let (core, seen) = engine("unaware");
    let ev = core.subscribe();
    core.launch_run(spec(sid, LaunchStandIn::default()))
        .expect("launch");
    let evs = collect_until(&ev, Duration::from_secs(30), ends(sid));
    let condition = evs.iter().find_map(|e| match e {
        CoreEvent::GateEscalated {
            session, condition, ..
        } if session == sid => Some(condition.clone()),
        _ => None,
    });
    assert_eq!(condition.as_deref(), Some("dead_seat"), "{evs:?}");
    assert!(!evs
        .iter()
        .any(|e| matches!(e, CoreEvent::StandInSeated { .. })));
    assert!(seen.lock().unwrap().is_empty());
    let session = session("unaware", sid).expect("session");
    assert!(!session.assurance.stand_in_aware);
    assert!(session.assurance.roles.is_none());
    // `use_stand_ins` is refused on an unaware run.
    let seq = core
        .gate_actions(sid)
        .unwrap()
        .and_then(|g| g.gate_seq)
        .unwrap_or(0);
    assert!(core
        .grant_stand_ins(sid, "job", Some("operator".into()), seq, None)
        .is_err());
}

/// T-B3 / rev 3 N4: stand-ins and reduced assurance are mutually exclusive, with or without
/// consent, and consent on an unaware launch is refused — each with NO session persisted.
#[test]
fn aware_and_reduced_or_consent_without_aware_is_refused_at_launch() {
    let (core, _) = engine("refused");
    let mut s = spec("standin-reduced", aware(None));
    s.reduced_assurance = true;
    assert!(core.launch_run(s).is_err());
    let mut s = spec("standin-reduced-consent", aware(Some(consent())));
    s.reduced_assurance = true;
    assert!(core.launch_run(s).is_err());
    let s = spec(
        "standin-consent-unaware",
        LaunchStandIn {
            aware: false,
            parties: Vec::new(),
            consent: Some(consent()),
        },
    );
    assert!(core.launch_run(s).is_err());
    for sid in [
        "standin-reduced",
        "standin-reduced-consent",
        "standin-consent-unaware",
    ] {
        assert!(session("refused", sid).is_none(), "{sid}");
    }
}
