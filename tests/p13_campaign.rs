//! Campaign DAG scheduler — integration proving tests (DES-CAMPAIGN-001, REQ §5 SCs).
//!
//! These drive a real `Core` (the single-writer actor + off-thread workers) with a CONTROLLED stub
//! runner so each node's Run can be held mid-flight, released, or made to fail on command — the same
//! shape the existing p1/p2 tests use. Covered here:
//!   * SC-C2 — the §1 A/B/C example: wave-1 concurrency + dependent dispatch as deps clear.
//!   * SC-C3 — max_concurrency cap: never more than N in-flight; the rest queue then run.
//!   * SC-C4 — fail_fast: a node fails → in-flight cancelled, campaign Failed.
//!   * SC-C5 — continue_independent: a failure blocks only its transitive dependents.
//!   * SC-C6 — crash-resume: a fresh Core over the same store completes without re-running a done node.
//!   * SC-C8 — per-node gate isolation at max_concurrency=1: a gating node frees its slot.
//!
//! (SC-C1 diamond join, SC-C7 cycle/validation, SC-C9 100-run determinism, and the mixed-edge truth
//! table are proven as pure unit tests in `src/campaign.rs`.)

use std::collections::{BTreeSet, HashSet};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

use wicked_core::{
    CampaignDef, CampaignEdge, CampaignGateDecision, CampaignNode, CampaignStatus, Core,
    EdgeCondition, EntityMode, FailurePolicy, HumanConfirm, NodeStatus, RunSpec, StepInput,
    StepOutput, StepRunner, StepStatus,
};

// ── stub council (votes without a subprocess; the real dispatcher hangs under the harness) ──
struct StubDispatcher;
impl Dispatcher for StubDispatcher {
    fn dispatch(&self, cli: &AgenticCli, _t: &CouncilTask) -> Option<Vote> {
        Some(Vote {
            cli: cli.key.clone(),
            recommendation: "x".into(),
            top_risk: "none".into(),
            change_my_mind: "no".into(),
            disqualifier: None,
            confidence: Confidence::default(),
            provenance: "stub".into(),
        })
    }
}

/// A controllable step runner keyed by run id:
///  * announces each run id it starts on `started`,
///  * records every run id it actually ran in `ran` (the resume test's decisive proof),
///  * blocks until the run id is `released` — unless `auto` is set (complete immediately),
///  * returns `Failed` for a run id in `fail`.
struct Ctl {
    started: Mutex<Sender<String>>,
    ran: Arc<Mutex<Vec<String>>>,
    released: Arc<(Mutex<HashSet<String>>, Condvar)>,
    fail: Arc<Mutex<HashSet<String>>>,
    auto: Arc<std::sync::atomic::AtomicBool>,
}
impl StepRunner for Ctl {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        let rid = input.run_id.clone();
        self.ran.lock().unwrap().push(rid.clone());
        let _ = self.started.lock().unwrap().send(rid.clone());
        if !self.auto.load(std::sync::atomic::Ordering::SeqCst) {
            let (lock, cv) = &*self.released;
            let mut set = lock.lock().unwrap();
            // Re-check `auto` on every wakeup (and periodically, to survive a missed notify) so
            // `release_all()` — which flips `auto` — releases workers already parked here.
            while !set.contains(&rid) && !self.auto.load(std::sync::atomic::Ordering::SeqCst) {
                let (g, _) = cv.wait_timeout(set, Duration::from_millis(50)).unwrap();
                set = g;
            }
        }
        let status = if self.fail.lock().unwrap().contains(&rid) {
            StepStatus::Failed
        } else {
            StepStatus::Ok
        };
        StepOutput {
            run_id: rid,
            unit_ix: input.unit_ix,
            attempt: input.attempt,
            output: format!("did {}", input.unit.description),
            status,
        }
    }
}

struct Harness {
    started_rx: Receiver<String>,
    ran: Arc<Mutex<Vec<String>>>,
    released: Arc<(Mutex<HashSet<String>>, Condvar)>,
    fail: Arc<Mutex<HashSet<String>>>,
    auto: Arc<std::sync::atomic::AtomicBool>,
}

fn make_runner(auto: bool) -> (Arc<Ctl>, Harness) {
    let (started_tx, started_rx) = channel();
    let ran = Arc::new(Mutex::new(Vec::new()));
    let released = Arc::new((Mutex::new(HashSet::new()), Condvar::new()));
    let fail = Arc::new(Mutex::new(HashSet::new()));
    let auto = Arc::new(std::sync::atomic::AtomicBool::new(auto));
    let ctl = Arc::new(Ctl {
        started: Mutex::new(started_tx),
        ran: ran.clone(),
        released: released.clone(),
        fail: fail.clone(),
        auto: auto.clone(),
    });
    (
        ctl,
        Harness {
            started_rx,
            ran,
            released,
            fail,
            auto,
        },
    )
}

impl Harness {
    fn release(&self, rid: &str) {
        let (lock, cv) = &*self.released;
        lock.lock().unwrap().insert(rid.to_string());
        cv.notify_all();
    }
    fn release_all(&self) {
        // Wildcard: mark auto so any still-blocked worker (and any future one) completes.
        self.auto.store(true, std::sync::atomic::Ordering::SeqCst);
        let (_lock, cv) = &*self.released;
        cv.notify_all();
    }
    fn fail_run(&self, rid: &str) {
        self.fail.lock().unwrap().insert(rid.to_string());
    }
    /// Drain exactly `n` node-starts, returning the SET of run ids (dispatch order across threads is
    /// nondeterministic; the SET is what the SC asserts — ordering is enforced by causal releases).
    fn drain(&self, n: usize) -> BTreeSet<String> {
        let mut set = BTreeSet::new();
        for _ in 0..n {
            let r = self
                .started_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("expected a node's Run to start");
            set.insert(r);
        }
        set
    }
    /// Assert no further node starts within a quiet window (nothing else was dispatched).
    fn assert_quiet(&self) {
        if let Ok(r) = self.started_rx.recv_timeout(Duration::from_millis(300)) {
            panic!("unexpected extra dispatch: {r}");
        }
    }
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
    }
}

/// A single-unit node (the problem has no split points → exactly one work unit, so one `run_unit`).
fn cnode(id: &str, hc: HumanConfirm) -> CampaignNode {
    CampaignNode {
        node_id: id.to_string(),
        run_spec: RunSpec {
            problem: format!("do {id}"),
            clis: vec![cli("a"), cli("b")],
            entity_mode: EntityMode::Shared,
            human_confirm: hc,
            repo_ref: None,
        },
    }
}

fn edge(from: &str, to: &str) -> CampaignEdge {
    CampaignEdge {
        from: from.into(),
        to: to.into(),
        condition: EdgeCondition::OnSuccess,
    }
}

/// The derived run id for a node's first attempt (§2.1: `"{campaign}:{node}:a0"`).
fn rid(campaign: &str, node: &str) -> String {
    format!("{campaign}:{node}:a0")
}

fn tmp_db(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!("wicked-core-p13-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("estate.db").to_str().unwrap().to_string()
}

fn spawn(db: &str, ctl: Arc<Ctl>) -> Core {
    Core::spawn_with_engine(db.to_string(), Arc::new(StubDispatcher), ctl)
}

fn wait_campaign(core: &Core, id: &str, want: CampaignStatus) -> bool {
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        if let Ok(Some(s)) = core.campaign_status(id) {
            if s == want {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    false
}

fn node_status(core: &Core, id: &str, node: &str) -> Option<NodeStatus> {
    core.campaign_detail(id)
        .ok()
        .flatten()
        .and_then(|c| c.node_status.get(node).copied())
}

fn wait_node(core: &Core, id: &str, node: &str, want: NodeStatus) -> bool {
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        if node_status(core, id, node) == Some(want) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    false
}

fn running_count(core: &Core, id: &str) -> usize {
    core.campaign_detail(id)
        .ok()
        .flatten()
        .map(|c| c.running_count())
        .unwrap_or(0)
}

// ── SC-C2 — the §1 example: wave-1 concurrency + dependent dispatch as deps clear ──
#[test]
fn sc_c2_example_dispatch_order_and_overlap() {
    let (ctl, h) = make_runner(false); // gated so we control completion
    let db = tmp_db("c2");
    let core = spawn(&db, ctl);
    let c = "camp2";

    // The §1 / §8 graph.
    let def = CampaignDef {
        id: c.into(),
        name: "example".into(),
        nodes: vec![
            cnode("A-build", HumanConfirm::None),
            cnode("A-test", HumanConfirm::None),
            cnode("B-design", HumanConfirm::None),
            cnode("B-build", HumanConfirm::None),
            cnode("C-design", HumanConfirm::None),
            cnode("C-build", HumanConfirm::None),
        ],
        edges: vec![
            edge("A-build", "A-test"),
            edge("B-design", "B-build"),
            edge("A-build", "B-build"),
            edge("C-design", "C-build"),
            edge("A-build", "C-build"),
        ],
        policy: FailurePolicy::ContinueIndependent,
        max_concurrency: 4,
    };
    core.launch_campaign(def).expect("launch");

    // Wave 1 (in-degree 0) all dispatch concurrently; NOTHING from wave 2 yet.
    let wave1 = h.drain(3);
    assert_eq!(
        wave1,
        BTreeSet::from([rid(c, "A-build"), rid(c, "B-design"), rid(c, "C-design")]),
        "wave-1 = the three in-degree-0 nodes, all in flight at once"
    );
    h.assert_quiet();

    // A-build completing dispatches ONLY A-test (B-build/C-build still need B-design/C-design).
    h.release(&rid(c, "A-build"));
    assert_eq!(
        h.drain(1),
        BTreeSet::from([rid(c, "A-test")]),
        "A-build clearing dispatches A-test the instant its single dep is done"
    );
    h.assert_quiet();

    // B-design completing now dispatches B-build (its other dep A-build already cleared).
    h.release(&rid(c, "B-design"));
    assert_eq!(h.drain(1), BTreeSet::from([rid(c, "B-build")]));
    h.assert_quiet();

    // C-design completing dispatches C-build.
    h.release(&rid(c, "C-design"));
    assert_eq!(h.drain(1), BTreeSet::from([rid(c, "C-build")]));

    // Release the rest → campaign completes.
    h.release_all();
    assert!(
        wait_campaign(&core, c, CampaignStatus::Completed),
        "the whole DAG runs to Completed"
    );
}

// ── SC-C3 — max_concurrency cap: never more than N in-flight ──
#[test]
fn sc_c3_respects_max_concurrency_cap() {
    let (ctl, h) = make_runner(false);
    let db = tmp_db("c3");
    let core = spawn(&db, ctl);
    let c = "camp3";

    let def = CampaignDef {
        id: c.into(),
        name: "cap".into(),
        nodes: vec![
            cnode("n1", HumanConfirm::None),
            cnode("n2", HumanConfirm::None),
            cnode("n3", HumanConfirm::None),
            cnode("n4", HumanConfirm::None),
        ],
        edges: vec![], // all independent
        policy: FailurePolicy::ContinueIndependent,
        max_concurrency: 2,
    };
    core.launch_campaign(def).expect("launch");

    // 4 ready, cap 2 → exactly 2 dispatch; the other 2 queue (no 3rd start).
    let first_two = h.drain(2);
    assert_eq!(first_two.len(), 2);
    h.assert_quiet();
    assert_eq!(running_count(&core, c), 2, "never more than the cap in flight");

    // Free one slot → a queued node dispatches; in-flight stays at the cap.
    let one = first_two.iter().next().unwrap().clone();
    h.release(&one);
    let third = h.drain(1);
    assert_eq!(third.len(), 1);
    assert!(!first_two.contains(third.iter().next().unwrap()), "a NEW node ran");
    // Give the completed node's reconcile a beat, then confirm the cap still holds.
    std::thread::sleep(Duration::from_millis(50));
    assert!(running_count(&core, c) <= 2, "the cap is a hard bound");

    h.release_all();
    assert!(wait_campaign(&core, c, CampaignStatus::Completed));
}

// ── SC-C4 — fail_fast: a node fails → in-flight cancelled, campaign Failed ──
#[test]
fn sc_c4_fail_fast_cancels_in_flight_and_fails_campaign() {
    let (ctl, h) = make_runner(false);
    let db = tmp_db("c4");
    let core = spawn(&db, ctl);
    let c = "camp4";

    // Two independent nodes; "boom" fails, "victim" is held in-flight and must be cancelled.
    h.fail_run(&rid(c, "boom"));
    let def = CampaignDef {
        id: c.into(),
        name: "ff".into(),
        nodes: vec![
            cnode("boom", HumanConfirm::None),
            cnode("victim", HumanConfirm::None),
        ],
        edges: vec![],
        policy: FailurePolicy::FailFast,
        max_concurrency: 4,
    };
    core.launch_campaign(def).expect("launch");

    // Both start; hold them, then let "boom" fail.
    let started = h.drain(2);
    assert_eq!(
        started,
        BTreeSet::from([rid(c, "boom"), rid(c, "victim")])
    );
    h.release(&rid(c, "boom")); // → Failed → fail-fast

    assert!(
        wait_campaign(&core, c, CampaignStatus::Failed),
        "fail-fast drives the campaign to Failed"
    );
    assert_eq!(node_status(&core, c, "boom"), Some(NodeStatus::Failed));
    assert_eq!(
        node_status(&core, c, "victim"),
        Some(NodeStatus::Cancelled),
        "the in-flight independent node was cancelled by fail-fast"
    );
    h.release_all(); // let the cancelled worker exit (its late result is ignored as Stale)
}

// ── SC-C5 — continue_independent: a failure blocks only its transitive dependents ──
#[test]
fn sc_c5_continue_independent_blocks_only_dependents() {
    let (ctl, h) = make_runner(true); // auto-complete; deterministic final state
    let db = tmp_db("c5");
    let core = spawn(&db, ctl);
    let c = "camp5";

    // X -> Y (OnSuccess); W independent. X fails.
    h.fail_run(&rid(c, "X"));
    let def = CampaignDef {
        id: c.into(),
        name: "ci".into(),
        nodes: vec![
            cnode("X", HumanConfirm::None),
            cnode("Y", HumanConfirm::None),
            cnode("W", HumanConfirm::None),
        ],
        edges: vec![edge("X", "Y")],
        policy: FailurePolicy::ContinueIndependent,
        max_concurrency: 4,
    };
    core.launch_campaign(def).expect("launch");

    assert!(
        wait_campaign(&core, c, CampaignStatus::PartiallyCompleted),
        "an independent branch completes; the campaign ends PartiallyCompleted"
    );
    assert_eq!(node_status(&core, c, "X"), Some(NodeStatus::Failed));
    assert_eq!(
        node_status(&core, c, "Y"),
        Some(NodeStatus::Blocked),
        "X's transitive OnSuccess dependent Y is Blocked"
    );
    assert_eq!(
        node_status(&core, c, "W"),
        Some(NodeStatus::Completed),
        "the independent branch W runs to completion"
    );
}

// ── SC-C8 — per-node gate isolation at max_concurrency=1 ──
#[test]
fn sc_c8_per_node_gate_frees_the_slot_at_concurrency_one() {
    let (ctl, _h) = make_runner(true); // independent node auto-completes; gate node pauses on its own
    let db = tmp_db("c8");
    let core = spawn(&db, ctl);
    let c = "camp8";

    // "a-gate" gates on its internal HITL (human_confirm=all) BEFORE running any unit; "b-indep" is
    // independent. cap=1, and a-gate < b-indep so a-gate is dispatched first.
    let def = CampaignDef {
        id: c.into(),
        name: "gate".into(),
        nodes: vec![
            cnode("a-gate", HumanConfirm::All),
            cnode("b-indep", HumanConfirm::None),
        ],
        edges: vec![],
        policy: FailurePolicy::ContinueIndependent,
        max_concurrency: 1,
    };
    core.launch_campaign(def).expect("launch");

    // a-gate opens its gate → frees the (only) slot → b-indep runs to completion WHILE a-gate waits.
    // If the slot were NOT freed, b-indep would be stuck Pending behind the cap.
    assert!(
        wait_node(&core, c, "a-gate", NodeStatus::AwaitingHuman),
        "the gating node is AwaitingHuman (its slot freed)"
    );
    assert!(
        wait_node(&core, c, "b-indep", NodeStatus::Completed),
        "the independent node runs concurrently at max_concurrency=1 (FR6)"
    );

    // Approve the gate → the node re-acquires the freed slot and resumes → campaign completes.
    core.confirm_campaign_gate(c, "a-gate", CampaignGateDecision::Approve { amend: None })
        .expect("approve the per-node gate");
    assert!(wait_campaign(&core, c, CampaignStatus::Completed));
    assert_eq!(node_status(&core, c, "a-gate"), Some(NodeStatus::Completed));
}

// ── SC-C6 — crash-resume: a fresh Core completes without re-running a done node ──
#[test]
fn sc_c6_crash_resume_never_reruns_a_completed_node() {
    let db = tmp_db("c6");
    let c = "camp6";
    let node_a = rid(c, "A");
    let node_b = rid(c, "B");

    // ── Core A: A auto-completes; B is held in-flight (its worker blocks). ──
    let (ctl_a, h_a) = make_runner(false);
    h_a.release(&node_a); // pre-release A so it completes; B stays blocked
    let core_a = spawn(&db, ctl_a);

    let def = CampaignDef {
        id: c.into(),
        name: "resume".into(),
        nodes: vec![cnode("A", HumanConfirm::None), cnode("B", HumanConfirm::None)],
        edges: vec![edge("A", "B")], // B depends on A
        policy: FailurePolicy::ContinueIndependent,
        max_concurrency: 2,
    };
    core_a.launch_campaign(def).expect("launch");

    // A completes → B dispatches and blocks. Persisted mid-campaign: A Completed, B Running.
    assert!(wait_node(&core_a, c, "A", NodeStatus::Completed));
    assert!(wait_node(&core_a, c, "B", NodeStatus::Running));
    let ran_a = h_a.ran.lock().unwrap().clone();
    assert!(ran_a.contains(&node_a), "Core A ran A");

    // Drop Core A (abandons B's blocked worker) — the last handle dropping releases the actor + store.
    drop(core_a);
    std::thread::sleep(Duration::from_millis(200));

    // ── Core B: a FRESH actor over the same store resumes the campaign. ──
    let (ctl_b, h_b) = make_runner(true); // auto-complete on the resuming process
    let core_b = spawn(&db, ctl_b);
    core_b.resume_campaign(c).expect("resume the campaign");

    assert!(
        wait_campaign(&core_b, c, CampaignStatus::Completed),
        "the resumed campaign runs B to completion and finishes"
    );
    assert_eq!(node_status(&core_b, c, "A"), Some(NodeStatus::Completed));
    assert_eq!(node_status(&core_b, c, "B"), Some(NodeStatus::Completed));

    // The decisive proof: Core B ran ONLY B (the interrupted node), never re-running the COMPLETED
    // node A — no node runs twice, no duplicate node (SC-C6 / FR7).
    let ran_b = h_b.ran.lock().unwrap().clone();
    assert!(
        ran_b.iter().any(|r| *r == node_b),
        "Core B ran B (the interrupted node), got {ran_b:?}"
    );
    assert!(
        !ran_b.iter().any(|r| *r == node_a),
        "Core B must NOT re-run the already-completed node A, got {ran_b:?}"
    );

    h_a.release_all(); // let Core A's abandoned B-worker exit (posts into a closed channel, harmless)
}
