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

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use wicked_apps_core::{open_store, ToNode};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

use wicked_core::{
    put_node, AgentSession, Campaign, CampaignDef, CampaignEdge, CampaignGateDecision,
    CampaignNode, CampaignStatus, Core, EdgeCondition, EntityMode, FailurePolicy, HumanConfirm,
    NodeStatus, RunSpec, SessionStatus, StepInput, StepOutput, StepRunner, StepStatus,
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
            usage: None,
            files: Vec::new(),
            governed: false,
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
    assert_eq!(
        running_count(&core, c),
        2,
        "never more than the cap in flight"
    );

    // Free one slot → a queued node dispatches; in-flight stays at the cap.
    let one = first_two.iter().next().unwrap().clone();
    h.release(&one);
    let third = h.drain(1);
    assert_eq!(third.len(), 1);
    assert!(
        !first_two.contains(third.iter().next().unwrap()),
        "a NEW node ran"
    );
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
    assert_eq!(started, BTreeSet::from([rid(c, "boom"), rid(c, "victim")]));
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
        nodes: vec![
            cnode("A", HumanConfirm::None),
            cnode("B", HumanConfirm::None),
        ],
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
        ran_b.contains(&node_b),
        "Core B ran B (the interrupted node), got {ran_b:?}"
    );
    assert!(
        !ran_b.contains(&node_a),
        "Core B must NOT re-run the already-completed node A, got {ran_b:?}"
    );

    h_a.release_all(); // let Core A's abandoned B-worker exit (posts into a closed channel, harmless)
}

/// Build a persisted crash-artifact campaign DIRECTLY (no public `Campaign::new`): `running` is the
/// node left `Running` at crash (with its derived run id recorded); every other node is `Pending`;
/// the campaign is `Running`. Models the exact on-store state a mid-dispatch SIGKILL leaves behind.
fn craft_campaign(def: CampaignDef, running: &str) -> Campaign {
    craft_campaign_at(def, running, NodeStatus::Running)
}

/// Like [`craft_campaign`] but the crash-artifact node is left at an arbitrary non-terminal `status`
/// — `Running` for a mid-dispatch crash, `AwaitingHuman`/`ReadyToResume` for a gate crash. Its
/// derived run id is recorded; every other node is `Pending`; the campaign is `Running`.
fn craft_campaign_at(def: CampaignDef, node: &str, status: NodeStatus) -> Campaign {
    let cid = def.id.clone();
    let node_status = def
        .nodes
        .iter()
        .map(|n| {
            let s = if n.node_id == node {
                status
            } else {
                NodeStatus::Pending
            };
            (n.node_id.clone(), s)
        })
        .collect();
    let mut node_run_id = BTreeMap::new();
    node_run_id.insert(node.to_string(), format!("{cid}:{node}:a0"));
    Campaign {
        id: cid.clone(),
        def_id: cid,
        status: CampaignStatus::Running,
        def,
        node_status,
        node_run_id,
        node_attempt: BTreeMap::new(),
        pending_decision: BTreeMap::new(),
        pending_decision_amend: BTreeMap::new(),
        pending_failure_gates: Vec::new(),
        fail_fast_tripped: false,
    }
}

/// A minimal `AgentSession` at an arbitrary `status` — the persisted session an interrupted node's
/// Run had written before the crash (resume reads its status to pick the reconcile outcome). A
/// pre-execution `Planning`/`Distributing` session models a mid-plan crash (no `WorkUnit` nodes are
/// written by the caller, so `session_units` is empty — the R1 window).
fn session_with(run_id: &str, status: SessionStatus) -> AgentSession {
    AgentSession {
        id: run_id.to_string(),
        workflow_id: format!("wf-{run_id}"),
        problem: "crashed mid-campaign".into(),
        entity_mode: EntityMode::Shared,
        collection_scope: None,
        clis: vec![],
        status,
        human_confirm: HumanConfirm::None,
        unit_ix: 0,
        attempt: 0,
        workdir: None,
        repo_ref: None,
    }
}

/// A minimal `AgentSession` already at `Completed` (F1a's terminal artifact).
fn completed_session(run_id: &str) -> AgentSession {
    session_with(run_id, SessionStatus::Completed)
}

// ── SC-C6 / F1a — resume reconciles a node whose SESSION finished before the crash ──
// Crash window: `finalize_run`/`fail_run`/`cancel_run` persist the session terminal and THEN queue the
// campaign reconcile via a LATER command. A crash in between leaves `session=terminal, node=Running`.
// Resume must reconcile that node from its persisted terminal session (NOT re-run it) and unblock its
// dependents — otherwise the node stays Running forever and the campaign never finalizes.
#[test]
fn sc_c6_f1a_resume_reconciles_a_node_whose_session_finished_before_crash() {
    let db = tmp_db("c6-f1a");
    let c = "camp6a";

    // Craft the F1a artifact directly: node X persisted `Running` (with its derived run id), while
    // X's session ALREADY reached Completed on the store (the pre-crash terminal write) — but the
    // campaign never got the deferred reconcile. Dependent Y never ran (no session).
    let def = CampaignDef {
        id: c.into(),
        name: "f1a".into(),
        nodes: vec![
            cnode("X", HumanConfirm::None),
            cnode("Y", HumanConfirm::None),
        ],
        edges: vec![edge("X", "Y")],
        policy: FailurePolicy::ContinueIndependent,
        max_concurrency: 2,
    };
    {
        let mut store = open_store(Some(&db)).expect("open store");
        put_node(&mut store, craft_campaign(def, "X").to_node()).expect("persist campaign");
        put_node(&mut store, completed_session(&rid(c, "X")).to_node()).expect("persist session");
    }

    // Core B resumes: X must reconcile from its terminal session (NOT re-run), then Y runs fresh.
    let (ctl_b, h_b) = make_runner(true);
    let core_b = spawn(&db, ctl_b);
    core_b.resume_campaign(c).expect("resume");
    assert!(
        wait_campaign(&core_b, c, CampaignStatus::Completed),
        "resume reconciles the finished node and drives the campaign to Completed (not wedged)"
    );
    assert_eq!(node_status(&core_b, c, "X"), Some(NodeStatus::Completed));
    assert_eq!(node_status(&core_b, c, "Y"), Some(NodeStatus::Completed));
    let ran_b = h_b.ran.lock().unwrap().clone();
    assert!(
        !ran_b.iter().any(|r| *r == rid(c, "X")),
        "X must be reconciled from its terminal session, never re-run, got {ran_b:?}"
    );
    assert!(
        ran_b.iter().any(|r| *r == rid(c, "Y")),
        "the dependent Y is promoted + run once X reconciles, got {ran_b:?}"
    );
}

// ── SC-C6 / F1b — resume launches a node whose SESSION was never written ──
// Crash window: `dispatch` persists `node=Running` + `node_run_id` BEFORE `launch_run_inner` writes the
// session (a multi-second worktree-create window for repo-backed nodes). A crash there leaves the node
// Running with NO session in core. Resume must LAUNCH it fresh under the same derived id
// (RunNotFound → Launch, DES §2.1/§6) — not bail + swallow, which stranded the node.
#[test]
fn sc_c6_f1b_resume_launches_a_node_whose_session_was_never_written() {
    let db = tmp_db("c6-f1b");
    let c = "camp6b";

    let def = CampaignDef {
        id: c.into(),
        name: "f1b".into(),
        nodes: vec![
            cnode("X", HumanConfirm::None),
            cnode("Y", HumanConfirm::None),
        ],
        edges: vec![edge("X", "Y")],
        policy: FailurePolicy::ContinueIndependent,
        max_concurrency: 2,
    };

    // Craft the artifact directly: node X persisted Running with its derived run id, NO session written.
    {
        let mut store = open_store(Some(&db)).expect("open store");
        put_node(&mut store, craft_campaign(def, "X").to_node()).expect("persist crash artifact");
    }

    // Core B resumes: X has no session → launch fresh under the same id, then Y runs after X completes.
    let (ctl_b, h_b) = make_runner(true);
    let core_b = spawn(&db, ctl_b);
    core_b.resume_campaign(c).expect("resume");
    assert!(
        wait_campaign(&core_b, c, CampaignStatus::Completed),
        "resume launches the never-written node fresh and completes (not wedged)"
    );
    assert_eq!(node_status(&core_b, c, "X"), Some(NodeStatus::Completed));
    assert_eq!(node_status(&core_b, c, "Y"), Some(NodeStatus::Completed));
    let ran_b = h_b.ran.lock().unwrap().clone();
    assert!(
        ran_b.iter().any(|r| *r == rid(c, "X")),
        "X is launched fresh under its derived id (RunNotFound → Launch), got {ran_b:?}"
    );
    assert!(
        ran_b.iter().any(|r| *r == rid(c, "Y")),
        "the dependent Y runs after X completes, got {ran_b:?}"
    );
}

// ── SC-C6 / R1 — resume FAILS a node whose Run crashed mid-PLANNING (session=Planning, 0 units) ──
// Crash window: `plan_and_distribute` writes `session=Planning` (pipeline.rs) BEFORE persisting the
// first unit. A crash there leaves `session=Planning` with ZERO units. The old resume advanced the
// cursor → `advance_or_pause` found `units.get(0)==None → Progress::Done → finalize_run` and
// mis-finalized a run (and its campaign node) as COMPLETED having planned nothing. Resume must FAIL a
// never-planned run instead, so the node reconciles Failed and its OnSuccess dependents are blocked —
// a run that never planned is not "done". This is the core resume primitive, shared with standalone
// `resume_run`.
#[test]
fn sc_c6_r1_resume_fails_a_node_that_crashed_mid_planning() {
    let db = tmp_db("c6-r1");
    let c = "camp6r1";

    let def = CampaignDef {
        id: c.into(),
        name: "r1".into(),
        nodes: vec![
            cnode("X", HumanConfirm::None),
            cnode("Y", HumanConfirm::None),
            cnode("W", HumanConfirm::None),
        ],
        edges: vec![edge("X", "Y")], // Y OnSuccess-depends on X; W is independent
        policy: FailurePolicy::ContinueIndependent,
        max_concurrency: 3,
    };
    // Craft: node X persisted `Running`, its session at `Planning` with NO units written — the exact
    // mid-plan crash artifact (a session in `Planning`/`Distributing` never finished a distributed plan).
    {
        let mut store = open_store(Some(&db)).expect("open store");
        put_node(&mut store, craft_campaign(def, "X").to_node()).expect("persist campaign");
        put_node(
            &mut store,
            session_with(&rid(c, "X"), SessionStatus::Planning).to_node(),
        )
        .expect("persist Planning session");
    }

    let (ctl_b, h_b) = make_runner(true);
    let core_b = spawn(&db, ctl_b);
    core_b.resume_campaign(c).expect("resume");
    assert!(
        wait_campaign(&core_b, c, CampaignStatus::PartiallyCompleted),
        "a never-planned node fails; its independent branch completes → PartiallyCompleted (not Completed)"
    );
    assert_eq!(
        node_status(&core_b, c, "X"),
        Some(NodeStatus::Failed),
        "the mid-plan-crashed node is reconciled Failed, NOT mis-finalized Completed"
    );
    assert_eq!(
        node_status(&core_b, c, "Y"),
        Some(NodeStatus::Blocked),
        "X's OnSuccess dependent Y is blocked, never promoted behind a phantom completion"
    );
    assert_eq!(node_status(&core_b, c, "W"), Some(NodeStatus::Completed));
    let ran_b = h_b.ran.lock().unwrap().clone();
    assert!(
        !ran_b.iter().any(|r| *r == rid(c, "X")),
        "X never planned; resume must not run it, got {ran_b:?}"
    );
    assert!(
        !ran_b.iter().any(|r| *r == rid(c, "Y")),
        "the blocked dependent Y must not run, got {ran_b:?}"
    );
}

// ── SC-C6 / R2 — resume reconciles a node stuck AwaitingHuman whose Run was Cancelled by a Reject ──
// Crash window: campaign `confirm_gate(Reject)` → core `cancel_run` persists `session=Cancelled` and
// DEFERS the node reconcile to `CampaignRunFinished`. A crash in between leaves `session=Cancelled,
// node=AwaitingHuman` — the symmetric twin of F1a, which the `Running`-only re-derivation never
// revisited (the node stays wedged and `finalize_if_done`'s `any_waiting` guard never clears). Resume
// must reconcile the node from its terminal session.
#[test]
fn sc_c6_r2_resume_reconciles_an_awaiting_human_node_whose_run_was_cancelled() {
    let db = tmp_db("c6-r2");
    let c = "camp6r2";

    let def = CampaignDef {
        id: c.into(),
        name: "r2".into(),
        nodes: vec![
            cnode("X", HumanConfirm::None),
            cnode("Y", HumanConfirm::None),
            cnode("W", HumanConfirm::None),
        ],
        edges: vec![edge("X", "Y")],
        policy: FailurePolicy::ContinueIndependent,
        max_concurrency: 3,
    };
    // Craft: node X persisted `AwaitingHuman`, its session ALREADY `Cancelled` (the Reject terminal
    // write) — but the deferred `CampaignRunFinished` never landed.
    {
        let mut store = open_store(Some(&db)).expect("open store");
        put_node(
            &mut store,
            craft_campaign_at(def, "X", NodeStatus::AwaitingHuman).to_node(),
        )
        .expect("persist campaign");
        put_node(
            &mut store,
            session_with(&rid(c, "X"), SessionStatus::Cancelled).to_node(),
        )
        .expect("persist Cancelled session");
    }

    let (ctl_b, h_b) = make_runner(true);
    let core_b = spawn(&db, ctl_b);
    core_b.resume_campaign(c).expect("resume");
    assert!(
        wait_campaign(&core_b, c, CampaignStatus::PartiallyCompleted),
        "the stuck AwaitingHuman node reconciles Cancelled; the campaign is no longer wedged"
    );
    assert_eq!(
        node_status(&core_b, c, "X"),
        Some(NodeStatus::Cancelled),
        "X reconciles from its terminal (Cancelled) session, not stranded AwaitingHuman"
    );
    assert_eq!(
        node_status(&core_b, c, "Y"),
        Some(NodeStatus::Blocked),
        "X's OnSuccess dependent Y is blocked by the cancellation"
    );
    assert_eq!(node_status(&core_b, c, "W"), Some(NodeStatus::Completed));
    let ran_b = h_b.ran.lock().unwrap().clone();
    assert!(
        !ran_b.iter().any(|r| *r == rid(c, "X")),
        "X was cancelled — resume must not run it, got {ran_b:?}"
    );
}

// ── SC-C6 / R2 (no-regress) — a node LEGITIMATELY paused for a human (its session is STILL
// AwaitingHuman) must survive resume untouched: it is neither reconciled terminal nor re-attached. ──
#[test]
fn sc_c6_r2_resume_leaves_a_genuinely_waiting_node_paused() {
    let db = tmp_db("c6-r2b");
    let c = "camp6r2b";

    let def = CampaignDef {
        id: c.into(),
        name: "r2b".into(),
        nodes: vec![
            cnode("X", HumanConfirm::None),
            cnode("Y", HumanConfirm::None),
        ],
        edges: vec![edge("X", "Y")],
        policy: FailurePolicy::ContinueIndependent,
        max_concurrency: 2,
    };
    // Craft: node X persisted `AwaitingHuman` with its session ALSO `AwaitingHuman` (a genuine, live
    // human wait — the non-terminal case the R2 re-derivation must leave alone).
    {
        let mut store = open_store(Some(&db)).expect("open store");
        put_node(
            &mut store,
            craft_campaign_at(def, "X", NodeStatus::AwaitingHuman).to_node(),
        )
        .expect("persist campaign");
        put_node(
            &mut store,
            session_with(&rid(c, "X"), SessionStatus::AwaitingHuman).to_node(),
        )
        .expect("persist AwaitingHuman session");
    }

    let (ctl_b, h_b) = make_runner(true);
    let core_b = spawn(&db, ctl_b);
    core_b.resume_campaign(c).expect("resume");

    // Give the resume a beat to settle, then assert the wait held: X stays AwaitingHuman (not cancelled,
    // not resumed) and the campaign stays Running (blocked on the human), never wedged-finalized.
    std::thread::sleep(Duration::from_millis(250));
    assert_eq!(
        node_status(&core_b, c, "X"),
        Some(NodeStatus::AwaitingHuman),
        "a genuine human wait survives a crash-resume untouched"
    );
    assert_eq!(
        core_b.campaign_status(c).ok().flatten(),
        Some(CampaignStatus::Running),
        "the campaign stays Running (still needs the human), never mis-finalized"
    );
    let ran_b = h_b.ran.lock().unwrap().clone();
    assert!(
        !ran_b.iter().any(|r| *r == rid(c, "X")),
        "the paused node's Run must not be resumed, got {ran_b:?}"
    );
}

// ── SC-C6 / R3 — resume reconciles a node whose SESSION FAILED before the crash (blocking path) ──
// F1a proves the Completed variant; this proves the FAILED variant the blocking / fail-fast behavior
// rests on (previously only code-shared, never crash-tested). Crash window: `fail_run` persists
// `session=Failed` then DEFERS the reconcile; a crash leaves `session=Failed, node=Running`. Resume
// must reconcile Failed AND apply the failure policy — under ContinueIndependent the failed node's
// OnSuccess dependents are Blocked (not promoted).
#[test]
fn sc_c6_r3_resume_reconciles_a_node_whose_session_failed_before_crash() {
    let db = tmp_db("c6-r3");
    let c = "camp6r3";

    let def = CampaignDef {
        id: c.into(),
        name: "r3".into(),
        nodes: vec![
            cnode("X", HumanConfirm::None),
            cnode("Y", HumanConfirm::None),
            cnode("W", HumanConfirm::None),
        ],
        edges: vec![edge("X", "Y")],
        policy: FailurePolicy::ContinueIndependent,
        max_concurrency: 3,
    };
    {
        let mut store = open_store(Some(&db)).expect("open store");
        put_node(&mut store, craft_campaign(def, "X").to_node()).expect("persist campaign");
        put_node(
            &mut store,
            session_with(&rid(c, "X"), SessionStatus::Failed).to_node(),
        )
        .expect("persist Failed session");
    }

    let (ctl_b, h_b) = make_runner(true);
    let core_b = spawn(&db, ctl_b);
    core_b.resume_campaign(c).expect("resume");
    assert!(
        wait_campaign(&core_b, c, CampaignStatus::PartiallyCompleted),
        "the failed node blocks its branch; the independent branch completes → PartiallyCompleted"
    );
    assert_eq!(
        node_status(&core_b, c, "X"),
        Some(NodeStatus::Failed),
        "X reconciles Failed from its terminal session (never re-run)"
    );
    assert_eq!(
        node_status(&core_b, c, "Y"),
        Some(NodeStatus::Blocked),
        "X's OnSuccess dependent Y is Blocked (not promoted) — the failure-policy path fired on resume"
    );
    assert_eq!(node_status(&core_b, c, "W"), Some(NodeStatus::Completed));
    let ran_b = h_b.ran.lock().unwrap().clone();
    assert!(
        !ran_b.iter().any(|r| *r == rid(c, "X")),
        "X is reconciled from its terminal session, never re-run, got {ran_b:?}"
    );
    assert!(
        !ran_b.iter().any(|r| *r == rid(c, "Y")),
        "the blocked dependent Y must not run, got {ran_b:?}"
    );
}

// ── SC-C6 / R3 (fail-fast) — the SAME Failed-at-crash artifact under FailFast: resume reconciles
// Failed AND the fail-fast policy cancels every other node + drives the campaign to Failed. ──
#[test]
fn sc_c6_r3_resume_failed_node_honors_fail_fast_policy() {
    let db = tmp_db("c6-r3ff");
    let c = "camp6r3ff";

    let def = CampaignDef {
        id: c.into(),
        name: "r3ff".into(),
        nodes: vec![
            cnode("X", HumanConfirm::None),
            cnode("Z", HumanConfirm::None), // independent — fail-fast must cancel it
        ],
        edges: vec![],
        policy: FailurePolicy::FailFast,
        max_concurrency: 2,
    };
    {
        let mut store = open_store(Some(&db)).expect("open store");
        put_node(&mut store, craft_campaign(def, "X").to_node()).expect("persist campaign");
        put_node(
            &mut store,
            session_with(&rid(c, "X"), SessionStatus::Failed).to_node(),
        )
        .expect("persist Failed session");
    }

    let (ctl_b, _h_b) = make_runner(true);
    let core_b = spawn(&db, ctl_b);
    core_b.resume_campaign(c).expect("resume");
    assert!(
        wait_campaign(&core_b, c, CampaignStatus::Failed),
        "fail-fast honored on resume: the reconciled failure fails the whole campaign"
    );
    assert_eq!(node_status(&core_b, c, "X"), Some(NodeStatus::Failed));
    assert_eq!(
        node_status(&core_b, c, "Z"),
        Some(NodeStatus::Cancelled),
        "fail-fast cancels every other (non-terminal) node on the resuming process"
    );
}
