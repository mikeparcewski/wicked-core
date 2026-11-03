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
//! 4. A publish seats no builder anywhere, takes no QE skip at the gate, and its produce step's
//!    consent gate takes no approve without a bound QE PASS.

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
}
impl StepRunner for ScriptedSeat {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        let phase = input.unit.phase_id().unwrap_or("").to_string();
        let mut output = format!("did: {phase}");
        if input.unit.session_id == "validator" {
            output = "PASS\nthe work meets the criterion\nPASS".to_string();
        } else {
            self.ran.lock().unwrap().push((
                phase.clone(),
                input.unit.assigned_cli.clone().unwrap_or_default(),
            ));
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

fn core_with(name: &str, build: Build, review_passes: bool) -> (Core, Ran, String) {
    install_crew_stub();
    let dir = std::env::temp_dir().join(format!(
        "wicked-core-apps-db-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let ran = Arc::new(Mutex::new(Vec::new()));
    let core = Core::spawn_with_engine(
        dir.join("estate.db").to_str().unwrap().to_string(),
        Arc::new(StubDispatcher),
        Arc::new(ScriptedSeat {
            build,
            review_passes,
            ran: ran.clone(),
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
    (core, ran, entry.id)
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
    let (core, ran, repo) = core_with("lands", Build::SafeQuery, true);
    let events = core.subscribe();
    core.launch_run(spec("r-lands", "apps-turn", &repo, &["claude", "codex"]))
        .expect("launch");
    assert!(wait_status(&core, "r-lands", SessionStatus::Completed));
    let evs = drain(&events);
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
    let mut builders = spec("r-builders", "apps-publish", &repo, &["claude", "codex"]);
    builders.exclude_seats = vec!["claude".into(), "codex".into()];
    let lonely = spec("r-lonely", "apps-turn", &repo, &["claude"]);
    for (s, want) in [
        (reduced, "reduced-assurance"),
        (skip, "cannot be skipped"),
        (builders, "built a turn since the last publish"),
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

/// A publish seats no builder anywhere; its QE is forced, a gate skip is refused, and the produce
/// step's consent gate takes no approve without a bound QE PASS — so produce never runs.
#[test]
fn a_publish_refuses_qe_skips_and_produces_only_after_a_qe_pass() {
    let (core, ran, repo) = core_with("publish", Build::SafeQuery, true);
    let mut s = spec("r-publish", "apps-publish", &repo, &["claude", "codex"]);
    s.exclude_seats = vec!["claude".into()];
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
    for (phase, seat) in ran.lock().unwrap().iter() {
        assert_ne!(seat, "claude", "{phase} ran on a builder seat");
    }
    // Paused at the produce step's consent gate (unit 3), with produce not run.
    assert_eq!(v.session.unit_ix, 2, "{:?}", v.session);
    assert!(!crew_log("r-publish").contains("apps-produce"));
    let err = core
        .confirm_gate(
            "r-publish",
            HumanDecision::SkipQe {
                reason: "demo".into(),
            },
        )
        .unwrap_err();
    assert!(format!("{err:#}").contains("cannot be skipped"), "{err:#}");
    let err = core
        .confirm_gate(
            "r-publish",
            HumanDecision::Approve {
                amend: None,
                amend_scope: Default::default(),
            },
        )
        .unwrap_err();
    assert!(format!("{err:#}").contains("bound QE PASS"), "{err:#}");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        view(&core, "r-publish").session.status,
        SessionStatus::AwaitingHuman
    );
    assert!(
        !crew_log("r-publish").contains("apps-produce"),
        "produce never ran"
    );
}
