//! (DES-APPS-001 §9.2-§9.3, K0) The Apps presets through the REAL engine (`Core::spawn_with_engine`)
//! against a real registered app repo and worktree, with a scripted seat and a stand-in for crew's
//! `wicked-crew` (K2a's check route is not built yet: the stand-in refuses a query that executes a
//! string as SQL, as the read policy does, and logs every source dispatch it would make).
//!
//! 1. A turn lands only after a review on a seat distinct from its builder, whose gate receipt names
//!    the tree it judged; a REJECTing review lands nothing.
//! 2. A fixed creator candidate holding `SELECT query_to_xml($1, true, false, '')` fails the check
//!    with zero source dispatches: the review never runs and the turn fails, landing nothing.
//! 3. The launch refusals hold on the real launch path, with nothing persisted.
//! 4. A publish seats no builder anywhere and takes no QE skip at the gate; with a bound QE PASS
//!    from an isolated reviewer on the run's tree, its consent gate produces exactly once.

// The `wicked-crew` stand-in and the scripted QE turns are POSIX shell and paths.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

#[path = "support/skills_snapshot_fixture.rs"]
mod skills_fixture;

use wicked_core::assurance::QeOverride;
use wicked_core::{
    Core, CoreEvent, EntityMode, HumanConfirm, HumanDecision, LaunchSpec, RepoSpec, SessionStatus,
    StepInput, StepOutput, StepRunner, StepStatus,
};

/// The stand-in `wicked-crew` lives in this process's own dir.
fn crew_dir() -> PathBuf {
    std::env::temp_dir().join(format!("wicked-core-apps-crew-{}", std::process::id()))
}

/// Route emissions to a temp spool (core#311); run the Apps seats on the container floor (the
/// scripted seat stands in for the carrier, so no container starts); point `wicked-crew` at the
/// stand-in and give the daemon a broker URL, so the check Tool is armed with its unit's token.
/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only sets process env vars.
#[ctor::ctor(unsafe)]
fn arm_env() {
    wicked_apps_core::emit::hermetic_test_spool();
    std::env::set_var("WICKED_SEAT_FLOOR", "container");
    std::env::set_var("WICKED_CREW_URL", "http://127.0.0.1:9");
    std::env::set_var(
        "WICKED_CREW_BIN",
        crew_dir()
            .join("wicked-crew")
            .to_string_lossy()
            .into_owned(),
    );
    std::env::set_var("WICKED_REPO_CHECKS_RETRY_WAIT_S", "0");
}

/// crew's `wicked-crew` stand-in. `apps-check` judges the queries the build left with the read
/// policy's refusal of string-executing functions (DES §5.3 rule 4) BEFORE anything reaches a
/// source; only a check that passes the policy would describe a source, and it logs that dispatch.
const CREW_STUB: &str = r#"#!/bin/sh
dir=$(cd "$(dirname "$0")" && pwd)
run="${WICKED_RUN_ID:-norun}"
echo "argv=$* unit=${WICKED_RUN_UNIT} tree=${WICKED_TREE} token=${WICKED_MCP_TOKEN:+bound}" >> "$dir/$run.log"
case "$1" in
  apps-check)
    hit=$(grep -rlI 'query_to_xml' queries 2>/dev/null | head -n 1)
    if [ -n "$hit" ]; then
      echo "apps_check: refused: $hit: read policy: query_to_xml executes a string as SQL"
      exit 1
    fi
    echo "describe $*" >> "$dir/$run.dispatch"
    echo "apps_check: ok tree=${WICKED_TREE}"
    ;;
  apps-produce)
    echo "produced"
    ;;
  *)
    echo "unknown command $1"
    exit 2
    ;;
esac
"#;

fn install_crew_stub() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let dir = crew_dir();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("wicked-crew");
        // Written by a `/bin/sh` child so this process never holds a writable descriptor to a
        // file it later execs (ETXTBSY on Linux).
        let mut child = Command::new("/bin/sh")
            .args(["-c", "cat > \"$1\" && chmod 700 \"$1\"", "sh"])
            .arg(&path)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(CREW_STUB.as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success());
        // core#396: the presets' agent steps name `wicked-garden-apps`, and the plan-wide skills
        // admission runs before the first unit. A published fixture generation, laid out as crew
        // publishes one (never the developer's live garden cache). Canonical base: the loader
        // refuses an ancestor symlink, and the OS temp dir is one on macOS.
        let skills_base = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("wicked-core-apps-skills-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&skills_base);
        let snapshot = skills_fixture::publish_fixture_snapshot(
            &skills_base,
            "000001",
            &["wicked-garden-apps"],
        );
        std::env::set_var("WICKED_SKILLS_SNAPSHOT", &snapshot);
    });
}

fn crew_log(run_id: &str) -> String {
    std::fs::read_to_string(crew_dir().join(format!("{run_id}.log"))).unwrap_or_default()
}

fn crew_dispatches(run_id: &str) -> usize {
    std::fs::read_to_string(crew_dir().join(format!("{run_id}.dispatch")))
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

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

#[derive(Clone, Copy)]
enum Build {
    /// A safe read: a count over a declared table.
    SafeQuery,
    /// The fixed creator candidate: a query that executes a string as SQL.
    QueryToXml,
}

/// A seat that plays every role of both presets deterministically.
struct ScriptedSeat {
    build: Build,
    review_passes: bool,
    ran: Ran,
    /// Every governed input the seat was handed (what the engine put in its context).
    inputs: Inputs,
    /// The publish QE reviewer's verdict.
    qe_passes: bool,
}
impl StepRunner for ScriptedSeat {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        let phase = input.unit.phase_id().unwrap_or("").to_string();
        let mut output = format!("did: {phase}");
        // (DES-RULE-CHECK-001 rev 2 Part B) The review's rule-check judge calls (the shipped pack
        // rules are judged now): the stub answers each asked rule N/A in the call's frame.
        if let Some(v) = wicked_core::stub_rule_verdicts(&input.unit.description) {
            output = v;
        } else if input.unit.session_id == "validator" {
            output = "PASS\nthe work meets the criterion\nPASS".to_string();
        } else {
            self.ran.lock().unwrap().push((
                phase.clone(),
                input.unit.assigned_cli.clone().unwrap_or_default(),
            ));
            self.inputs.lock().unwrap().push(input.clone());
            if let (Some(wd), "build") = (&input.workdir, phase.as_str()) {
                let sql = match self.build {
                    Build::SafeQuery => {
                        "SELECT count(*) AS open FROM support.tickets WHERE status = $1"
                    }
                    Build::QueryToXml => "SELECT query_to_xml($1, true, false, '')",
                };
                std::fs::write(wd.join("queries/open.sql"), format!("{sql}\n")).unwrap();
                std::fs::write(
                    wd.join("apps.json"),
                    r#"{"pages":[{"id":"home","components":[{"id":"kpi-open","type":"kpi","query":"open"}]}],"queries":[{"id":"open","source":"support-pg","params":{"status":"text"},"columns":[{"name":"open","type":"int8"}]}]}"#,
                )
                .unwrap();
                output = "Changed apps.json (kpi-open) and queries/open.sql (open)".to_string();
            }
            if let (Some(root), "qe") = (input.unit.qe_ledger_root.as_deref(), phase.as_str()) {
                output.push_str(&qe_turn(input, Path::new(root), self.qe_passes));
            }
            if input.unit.role == wicked_core::PhaseRole::Evaluator && input.unit.tool_cmd.is_none()
            {
                output.push_str(if phase == "review" && !self.review_passes {
                    "\nTREE: judged\nVERDICT: FAIL — the query reads a table the message never asked for"
                } else {
                    "\nTREE: judged\nVERDICT: PASS"
                });
            }
        }
        StepOutput {
            run_id: input.run_id.clone(),
            unit_ix: input.unit_ix,
            attempt: input.attempt,
            output,
            status: StepStatus::Ok,
            usage: None,
            files: Vec::new(),
            tools: Vec::new(),
            governed: false,
        }
    }
}

/// The publish QE unit's two turns, as garden's QE pipeline plays them on a seat with no sub-agent
/// (DES-QE-EVERY-SEAT): the executor turn records its evidence under the run's ledger root and names
/// it on a `QE-EVIDENCE:` line; the engine's isolated reviewer turn (its nonce set) records a PASS
/// stamped with that nonce.
fn qe_turn(input: &StepInput, root: &Path, pass: bool) -> String {
    let id = "5d1e7c2a-9b3f-4e8d-a1c6-0f2e4b6d8a9c";
    match input.unit.qe_review_session.as_deref() {
        None => {
            let dir = root.join("evidence").join(id);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::create_dir_all(root.join("runs")).unwrap();
            std::fs::write(dir.join("test-plan.md"), "# publish check evidence\n").unwrap();
            std::fs::write(dir.join("scenario.md"), "# every page, filter and card\n").unwrap();
            std::fs::write(dir.join("evidence.json"), "{}\n").unwrap();
            std::fs::write(
                root.join("runs").join(format!("{id}.json")),
                serde_json::json!({
                    "id": id, "created_at": iso_now(),
                    "status": "running", "crew_run_id": input.run_id,
                    "evidence_path": dir.to_string_lossy(),
                })
                .to_string(),
            )
            .unwrap();
            format!("\nQE-EVIDENCE: {}", dir.display())
        }
        Some(nonce) => {
            let verdicts = root.join("verdicts");
            std::fs::create_dir_all(&verdicts).unwrap();
            std::fs::write(
                verdicts.join("v-review.json"),
                serde_json::json!({
                    "id": "v-review", "created_at": iso_now(),
                    "run_id": id,
                    "verdict": if pass { "PASS" } else { "FAIL" },
                    "crew_run_id": input.run_id,
                    "review_session": nonce,
                })
                .to_string(),
            )
            .unwrap();
            String::new()
        }
    }
}

fn iso_now() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let (days, rem) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    // Civil date from days (H. Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1000 % 60,
        rem % 1000
    )
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

fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

/// An app repo: `apps.json` and `queries/`, nothing to build or test.
fn app_repo(name: &str) -> PathBuf {
    let repo = std::env::temp_dir().join(format!(
        "wicked-core-apps-repo-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(repo.join("queries")).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "t@example.invalid"]);
    git(&repo, &["config", "user.name", "t"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    git(&repo, &["config", "core.autocrlf", "false"]);
    std::fs::write(repo.join("apps.json"), "{\"pages\":[],\"queries\":[]}\n").unwrap();
    std::fs::write(repo.join("queries/.keep"), "").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "an empty app"]);
    repo
}

/// `(phase, seat)` for every governed unit the scripted seat ran.
type Ran = Arc<Mutex<Vec<(String, String)>>>;
type Inputs = Arc<Mutex<Vec<StepInput>>>;

fn core_with(name: &str, build: Build, review_passes: bool) -> (Core, Ran, String) {
    let (core, ran, _inputs, repo) = core_full(name, build, review_passes);
    (core, ran, repo)
}

fn core_full(name: &str, build: Build, review_passes: bool) -> (Core, Ran, Inputs, String) {
    core_rig(name, build, review_passes, true)
}

fn core_rig(
    name: &str,
    build: Build,
    review_passes: bool,
    qe_passes: bool,
) -> (Core, Ran, Inputs, String) {
    install_crew_stub();
    let dir = std::env::temp_dir().join(format!(
        "wicked-core-apps-db-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let ran = Arc::new(Mutex::new(Vec::new()));
    let inputs = Arc::new(Mutex::new(Vec::new()));
    let core = Core::spawn_with_engine(
        dir.join("estate.db").to_str().unwrap().to_string(),
        Arc::new(StubDispatcher),
        Arc::new(ScriptedSeat {
            build,
            review_passes,
            ran: ran.clone(),
            inputs: inputs.clone(),
            qe_passes,
        }),
    );
    let repo = app_repo(name);
    let entry = core
        .register_repo(RepoSpec {
            name: name.into(),
            root_path: repo.to_str().unwrap().into(),
            registered_at: 1,
        })
        .expect("register");
    (core, ran, inputs, entry.id)
}

/// Every governed unit the engine dispatched carried the run's Apps marker (the registry-tool ban
/// rides it), from the session's contract, on every agent turn — the QE reviewer turn included.
fn assert_every_input_carries_the_ban(inputs: &Inputs, preset: &str) {
    let inputs = inputs.lock().unwrap();
    assert!(!inputs.is_empty());
    for i in inputs.iter() {
        assert_eq!(
            i.governance.as_ref().and_then(|g| g.apps.as_deref()),
            Some(preset),
            "unit {:?} (reviewer turn: {})",
            i.unit.phase_id(),
            i.unit.qe_review_session.is_some()
        );
    }
}

fn spec(run_id: &str, workflow: &str, repo: &str, clis: &[&str]) -> LaunchSpec {
    LaunchSpec {
        base_ref: None,
        project_id: None,
        problem: "Open escalations as a KPI".into(),
        clis: clis.iter().map(|k| cli(k)).collect(),
        entity_mode: EntityMode::Shared,
        session_id: run_id.into(),
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        repo_ref: Some(repo.into()),
        workflow: Some(workflow.into()),
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
    }
}

const WAIT_BUDGET: Duration = Duration::from_secs(240);

fn wait_status(core: &Core, run_id: &str, want: SessionStatus) -> bool {
    let start = Instant::now();
    let mut last = None;
    while start.elapsed() < WAIT_BUDGET {
        if let Ok(views) = core.sessions_detail() {
            if let Some(v) = views.iter().find(|v| v.session.id == run_id) {
                if v.session.status == want {
                    return true;
                }
                last = Some(v.session.status);
                if matches!(
                    v.session.status,
                    SessionStatus::Failed | SessionStatus::Cancelled | SessionStatus::Completed
                ) {
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let units = core
        .sessions_detail()
        .ok()
        .and_then(|vs| vs.into_iter().find(|v| v.session.id == run_id))
        .map(|v| {
            v.units
                .iter()
                .map(|u| format!("{:?}:{:?}:{:?}", u.phase_id(), u.status, u.denial_reason))
                .collect::<Vec<_>>()
        });
    eprintln!("wait_status({run_id}): wanted {want:?}, last {last:?}; units {units:?}");
    false
}

fn view(core: &Core, run_id: &str) -> wicked_core::SessionView {
    core.sessions_detail()
        .unwrap()
        .into_iter()
        .find(|v| v.session.id == run_id)
        .unwrap_or_else(|| panic!("run {run_id} exists"))
}

fn drain(events: &std::sync::mpsc::Receiver<CoreEvent>) -> Vec<CoreEvent> {
    let mut out = Vec::new();
    while let Ok(ev) = events.recv_timeout(Duration::from_millis(300)) {
        out.push(ev);
    }
    out
}

fn ran_phases(ran: &Ran) -> Vec<String> {
    ran.lock().unwrap().iter().map(|(p, _)| p.clone()).collect()
}

/// Acceptance: a turn lands only after a review by a seat distinct from its builder, and the
/// review's gate receipt names the tree it judged — the tree the check offered.
#[test]
fn a_turn_lands_only_after_a_distinct_seat_review_naming_its_tree() {
    let (core, ran, inputs, repo) = core_full("lands", Build::SafeQuery, true);
    let events = core.subscribe();
    core.launch_run(spec("r-lands", "apps-turn", &repo, &["claude", "codex"]))
        .expect("launch");
    assert!(wait_status(&core, "r-lands", SessionStatus::Completed));
    let evs = drain(&events);
    assert_every_input_carries_the_ban(&inputs, "apps-turn");
    let v = view(&core, "r-lands");
    assert_eq!(
        v.session.assurance.apps.as_ref().map(|a| a.preset.as_str()),
        Some("apps-turn")
    );
    let seat_of = |phase: &str| {
        v.units
            .iter()
            .find(|u| u.phase_id() == Some(phase))
            .and_then(|u| u.assigned_cli.clone())
            .unwrap()
    };
    assert_ne!(
        seat_of("build"),
        seat_of("review"),
        "the review is a distinct seat"
    );
    assert_eq!(ran_phases(&ran), ["build", "review"]);
    // The check ran crew's fixed command once, on the build's tree, with its unit's token.
    let log = crew_log("r-lands");
    assert_eq!(log.lines().count(), 1, "{log}");
    assert!(
        log.contains("argv=apps-check unit=2 ") && log.contains("token=bound"),
        "{log}"
    );
    let checked_tree = log
        .split("tree=")
        .nth(1)
        .and_then(|t| t.split_whitespace().next())
        .filter(|t| !t.is_empty())
        .expect("the check saw the tree")
        .to_string();
    let receipt = evs
        .iter()
        .find_map(|e| match e {
            CoreEvent::GateEvaluated {
                ord: 3,
                combined: true,
                assurance,
                ..
            } => Some(assurance.clone()),
            _ => None,
        })
        .expect("the review's passing gate");
    assert_eq!(
        receipt.evaluator.as_deref(),
        Some(seat_of("review").as_str())
    );
    assert_eq!(receipt.creator.as_deref(), Some(seat_of("build").as_str()));
    assert!(
        receipt.ran.iter().any(|r| r == "distinct_evaluator"),
        "{receipt:?}"
    );
    assert_eq!(
        receipt.tree.as_deref(),
        Some(checked_tree.as_str()),
        "{receipt:?}"
    );
}

/// …and a review that rejects lands nothing.
#[test]
fn a_turn_whose_review_rejects_never_lands() {
    let (core, ran, repo) = core_with("rejects", Build::SafeQuery, false);
    core.launch_run(spec("r-rejects", "apps-turn", &repo, &["claude", "codex"]))
        .expect("launch");
    assert!(wait_status(
        &core,
        "r-rejects",
        SessionStatus::AwaitingHuman
    ));
    assert_ne!(
        view(&core, "r-rejects").session.status,
        SessionStatus::Completed
    );
    assert!(ran_phases(&ran).contains(&"review".to_string()));
}

/// Acceptance: a fixed creator candidate holding `SELECT query_to_xml($1, true, false, '')` fails
/// the check with zero source dispatches; nothing after the check runs, and the turn fails.
#[test]
fn a_query_to_xml_candidate_fails_check_with_zero_source_dispatches() {
    let (core, ran, repo) = core_with("xml", Build::QueryToXml, true);
    let events = core.subscribe();
    core.launch_run(spec("r-xml", "apps-turn", &repo, &["claude", "codex"]))
        .expect("launch");
    // A failed check fails the turn: it lands nothing, and no gate offers to wave it through.
    assert!(wait_status(&core, "r-xml", SessionStatus::Failed));
    let evs = drain(&events);
    let check_passed = evs.iter().any(|e| {
        matches!(
            e,
            CoreEvent::GateEvaluated {
                ord: 2,
                combined: true,
                ..
            }
        )
    });
    assert!(!check_passed, "the check denied");
    assert_eq!(ran_phases(&ran), ["build"], "the review never ran");
    let log = crew_log("r-xml");
    assert_eq!(log.lines().count(), 1, "{log}");
    assert!(log.contains("argv=apps-check"), "{log}");
    assert_eq!(crew_dispatches("r-xml"), 0, "no source was dispatched");
    let check = view(&core, "r-xml")
        .units
        .into_iter()
        .find(|u| u.phase_id() == Some("check"))
        .unwrap();
    assert!(
        check
            .denial_reason
            .as_deref()
            .is_some_and(|r| r.contains("query_to_xml")),
        "the refusal names the query: {:?}",
        check.denial_reason
    );
    assert!(core
        .confirm_gate(
            "r-xml",
            HumanDecision::Approve {
                amend: None,
                amend_scope: Default::default(),
            },
        )
        .is_err());
}

/// The launch refusals on the real launch path: a synchronous Err, nothing persisted.
#[test]
fn apps_launch_refusals_persist_nothing() {
    let (core, _ran, repo) = core_with("refusals", Build::SafeQuery, true);
    let mut reduced = spec("r-reduced", "apps-turn", &repo, &["claude", "codex"]);
    reduced.reduced_assurance = true;
    let mut skip = spec("r-skip", "apps-publish", &repo, &["claude", "codex"]);
    skip.qe_acceptance = QeOverride::Skip("ship it".into());
    let evidence = std::env::temp_dir().join(format!(
        "wicked-core-apps-refusal-evidence-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&evidence).unwrap();
    let mut builders = spec("r-builders", "apps-publish", &repo, &["claude", "codex"]);
    builders.exclude_seats = vec!["claude".into(), "codex".into()];
    builders.evidence_root = Some(evidence.to_string_lossy().into_owned());
    let no_evidence = spec("r-no-evidence", "apps-publish", &repo, &["claude", "codex"]);
    let lonely = spec("r-lonely", "apps-turn", &repo, &["claude"]);
    for (s, want) in [
        (reduced, "reduced-assurance"),
        (skip, "cannot be skipped"),
        (builders, "built a turn since the last publish"),
        (no_evidence, "evidence root"),
        (lonely, "distinct from its builder"),
    ] {
        let id = s.session_id.clone();
        let err = core.launch_run(s).unwrap_err();
        assert!(format!("{err:#}").contains(want), "{id}: {err:#}");
        assert!(
            !core
                .sessions_detail()
                .unwrap()
                .iter()
                .any(|v| v.session.id == id),
            "{id} persisted nothing"
        );
    }
}

/// A publish seats no builder anywhere, its QE is forced, and a gate skip is refused. With a bound
/// QE PASS (the QE executor's evidence reviewed by the engine's isolated reviewer on a third seat,
/// on the tree the run holds), the approve produces exactly once and the publish completes.
#[test]
fn a_publish_produces_once_after_a_bound_qe_pass_and_takes_no_skip() {
    let (core, ran, inputs, repo) = core_full("publish", Build::SafeQuery, true);
    let evidence = std::env::temp_dir().join(format!(
        "wicked-core-apps-evidence-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&evidence);
    std::fs::create_dir_all(&evidence).unwrap();
    let mut s = spec(
        "r-publish",
        "apps-publish",
        &repo,
        &["claude", "codex", "pi"],
    );
    s.exclude_seats = vec!["claude".into()];
    s.evidence_root = Some(evidence.to_string_lossy().into_owned());
    core.launch_run(s).expect("launch");
    assert!(wait_status(
        &core,
        "r-publish",
        SessionStatus::AwaitingHuman
    ));
    let v = view(&core, "r-publish");
    let qe = v.session.assurance.qe.clone().expect("QE required");
    assert_eq!(
        (qe.status.as_str(), qe.basis.as_str()),
        ("required", "preset")
    );
    assert!(v
        .session
        .benched_seats
        .iter()
        .any(|b| b.cli == "claude" && b.source == "apps"));
    // Paused at the produce step's consent gate (unit 3), with produce not run.
    assert_eq!(v.session.unit_ix, 2, "{:?}", v.session);
    assert!(!crew_log("r-publish").contains("apps-produce"));
    // The QE executor and its isolated reviewer: two seats, neither a builder.
    let seats = |reviewer: bool| -> Vec<String> {
        inputs
            .lock()
            .unwrap()
            .iter()
            .filter(|i| i.unit.phase_id() == Some("qe"))
            .filter(|i| i.unit.qe_review_session.is_some() == reviewer)
            .filter_map(|i| i.unit.assigned_cli.clone())
            .collect()
    };
    let (executor, reviewer) = (seats(false), seats(true));
    assert!(
        !executor.is_empty() && !reviewer.is_empty(),
        "{executor:?} {reviewer:?}"
    );
    for seat in executor.iter().chain(reviewer.iter()) {
        assert_ne!(seat, "claude", "a builder seat took a QE turn");
    }
    assert!(
        reviewer.iter().all(|r| !executor.contains(r)),
        "{executor:?} {reviewer:?}"
    );
    for (phase, seat) in ran.lock().unwrap().iter() {
        assert_ne!(seat, "claude", "{phase} ran on a builder seat");
    }
    assert_every_input_carries_the_ban(&inputs, "apps-publish");
    // A gate skip is refused; nothing moved. (Every answer but reject is refused at this gate while
    // no bound PASS exists: `apps::tests::the_produce_consent_gate_refuses_an_approve_without_a_qe_pass`.)
    let err = core
        .confirm_gate(
            "r-publish",
            HumanDecision::SkipQe {
                reason: "demo".into(),
            },
        )
        .unwrap_err();
    assert!(format!("{err:#}").contains("cannot be skipped"), "{err:#}");
    assert!(!crew_log("r-publish").contains("apps-produce"));
    // The bound PASS opens the consent gate: produce runs once, and the publish completes.
    core.confirm_gate(
        "r-publish",
        HumanDecision::Approve {
            amend: None,
            amend_scope: Default::default(),
        },
    )
    .expect("a bound QE PASS on the run's tree opens the consent gate");
    assert!(wait_status(&core, "r-publish", SessionStatus::Completed));
    let produced = crew_log("r-publish")
        .lines()
        .filter(|l| l.contains("argv=apps-produce"))
        .count();
    assert_eq!(produced, 1, "{}", crew_log("r-publish"));
    let _ = std::fs::remove_dir_all(&evidence);
}

/// codex r2: no operator lever moves a publish's QE onto a seat that built a turn. The QE reviewer
/// fails the publish, which parks at the QE unit's gate; a reassign there to an excluded builder
/// seat (benched, in the roster) or to an excluded seat outside the roster is refused, the QE skip
/// is refused, and produce never runs.
#[test]
fn a_parked_publish_takes_no_reassign_to_a_builder_seat() {
    let (core, ran, _inputs, repo) = core_rig("parked", Build::SafeQuery, true, false);
    let evidence = std::env::temp_dir().join(format!(
        "wicked-core-apps-parked-evidence-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&evidence);
    std::fs::create_dir_all(&evidence).unwrap();
    let mut s = spec(
        "r-parked",
        "apps-publish",
        &repo,
        &["claude", "codex", "pi"],
    );
    s.exclude_seats = vec!["claude".into(), "opencode".into()];
    s.evidence_root = Some(evidence.to_string_lossy().into_owned());
    core.launch_run(s).expect("launch");
    assert!(wait_status(&core, "r-parked", SessionStatus::AwaitingHuman));
    let v = view(&core, "r-parked");
    let qe = v
        .units
        .iter()
        .find(|u| u.phase_id() == Some("qe"))
        .expect("the QE unit");
    assert_eq!(
        v.session.unit_ix, 1,
        "parked at the QE unit: {:?}",
        v.session
    );
    for seat in ["claude", "opencode"] {
        let err = core
            .reassign_unit("r-parked", qe.ord, Some(seat.into()))
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("built an Apps turn"),
            "{seat}: {err:#}"
        );
    }
    let still = view(&core, "r-parked");
    let qe_now = still
        .units
        .iter()
        .find(|u| u.phase_id() == Some("qe"))
        .unwrap();
    assert_eq!(qe_now.assigned_cli, qe.assigned_cli, "nothing moved");
    let err = core
        .confirm_gate(
            "r-parked",
            HumanDecision::SkipQe {
                reason: "demo".into(),
            },
        )
        .unwrap_err();
    assert!(format!("{err:#}").contains("cannot be skipped"), "{err:#}");
    for (phase, seat) in ran.lock().unwrap().iter() {
        assert_ne!(seat, "claude", "{phase} ran on a builder seat");
    }
    assert!(!crew_log("r-parked").contains("apps-produce"));
    let _ = std::fs::remove_dir_all(&evidence);
}
