//! (J6 C1b, DES-C1 item 7) The shipped `domain-extraction` preset, armed, end to end.
//!
//! The preset is two host steps (J6 v3): `domain-run` runs garden's `scripts/domain/run.py` through
//! the admitted garden generation, and `domain-graph` runs the real `wicked-core domain-graph` on the
//! repository's store. Here the garden generation is a fixture whose `run.py` stand-in records how
//! the engine ran it and makes three calls with the REAL `wicked-core model-call` client: two for
//! claude and one for codex. The run's seats are a stub runner that records each call.
//!
//! What it proves, through the real actor and the real client:
//! - the step is armed: the client reaches the channel, and each claude call reaches a seat as its
//!   own ballot (distinct ballot ids) whose parent run is the run;
//! - eligibility is the run's own (core#1905): a repository-bound run benches codex at launch
//!   (the codex worktree bench, no `WICKED_CODEX_WORKTREE_OK`), so the codex call is refused by
//!   name and no codex seat runs;
//! - the run's scope (coord ruling B') is written into the step's own channel directory, outside
//!   the worktree, named by `WICKED_DOMAIN_SCOPE`, byte-equal to what the launch carried, and gone
//!   with the channel when the step ends;
//! - a launch with no human confirm ends Done with no gate;
//! - a `domain-extraction` launch with no scope, or a scope on another launch, is refused by name;
//! - (J6 C2, F28) the graph step builds 1.2.0 from the rows the run step wrote: the store starts
//!   with garden's indexed nodes and no rows; while `domain-run` holds, the test writes the rows
//!   garden's real route wrote over the same tree (`tests/fixtures/domain_v3/mini-cobol.sql`), and
//!   the built graph holds their Checked statement. A second run whose step retires every row (a
//!   run that found no rules) leaves an empty model, not the first one.
//!
//! Unix-gated: the channel is a unix socket.
#![cfg(unix)]

use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wicked_apps_core::{open_store, GraphRead, GraphWrite};
use wicked_core::{
    Core, CoreEvent, EntityMode, HumanConfirm, LaunchSpec, PresetSpec, RepoEntry, RepoSpec,
    StepInput, StepOutput, StepRunner, StepStatus,
};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};
use wicked_estate_core::Annotation;

const BIN: &str = env!("CARGO_BIN_EXE_wicked-core");

#[path = "support/skills_snapshot_fixture.rs"]
mod skills_fixture;

/// Every emission this binary triggers spools into the per-process test outbox (core#311, core#749):
/// never the operator's real `~/.something-wicked/wicked-apps/emit-outbox.ndjson` replay queue.
///
/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only sets one process env var
/// via the std API — no allocator setup, no threads, no panics across the FFI boundary.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}

/// The fixture generation's `scripts/wicked-garden` launcher. For `run scripts/domain/run.py` it
/// records its arguments, working directory and `WICKED_DOMAIN_SCOPE`, copies the scope file, then
/// asks for two claude calls and one codex call with the real client (`$WICKED_CORE_EXE`, which the
/// channel hands the command), keeping each answer, stderr and exit code under
/// `.wicked/domain/e2e/` in the run's tree. It exits 0 whatever the calls did, so the test reads
/// every call's outcome.
const FIXTURE_GARDEN_LAUNCHER: &str = r#"#!/bin/sh
[ "$1" = run ] && [ "$2" = scripts/domain/run.py ] || exit 64
shift 2
d=.wicked/domain/e2e
mkdir -p "$d"
{
  printf '%s\n' "$@"
  printf 'cwd=%s\n' "$(pwd -P)"
  printf 'scope_env=%s\n' "${WICKED_DOMAIN_SCOPE-}"
} > "$d/args"
scope=
while [ $# -gt 0 ]; do
  [ "$1" = --scope ] && scope=$2
  shift
done
cp "$scope" "$d/scope.json" 2>"$d/scope.err"
printf 'Name one rule.' > "$d/prompt"
for call in "claude a" "claude b" "codex c"; do
  set -- $call
  "$WICKED_CORE_EXE" model-call --family "$1" --in "$d/prompt" --out "$d/$2.out" \
    --timeout-s 30 2>"$d/$2.err"
  echo $? > "$d/$2.rc"
done
# A checkout carrying the marker holds here while the test writes this run's rows into the store.
if [ -f e2e-handshake ]; then
  : > "$d/ready"
  i=0
  while [ ! -f "$d/written" ] && [ "$i" -lt 600 ]; do sleep 0.1; i=$((i+1)); done
fi
exit 0
"#;

struct StubDispatcher;
impl Dispatcher for StubDispatcher {
    fn dispatch(&self, cli: &AgenticCli, _t: &CouncilTask) -> Option<Vote> {
        Some(Vote {
            cli: cli.key.clone(),
            recommendation: "1".into(),
            top_risk: "none".into(),
            change_my_mind: "no".into(),
            disqualifier: None,
            confidence: Confidence::default(),
            provenance: "test".into(),
        })
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

/// One model call a seat answered: (the ballot's run id, the seat, the asking run).
type Answered = (String, Option<String>, Option<String>);

/// The seats: each call answers `<seat>: <prompt>` and is recorded.
#[derive(Default)]
struct Seats {
    seen: Mutex<Vec<Answered>>,
}

impl StepRunner for Seats {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        self.seen.lock().unwrap().push((
            i.run_id.clone(),
            i.unit.assigned_cli.clone(),
            i.unit.parent_run_id.clone(),
        ));
        StepOutput {
            run_id: i.run_id.clone(),
            unit_ix: i.unit_ix,
            attempt: i.attempt,
            output: format!(
                "{}: {}",
                i.unit.assigned_cli.clone().unwrap_or_default(),
                i.unit.description
            ),
            status: StepStatus::Ok,
            usage: None,
            files: vec![],
            tools: Vec::new(),
            governed: false,
        }
    }
}

/// The real-shape fixture (J6 C2): garden's indexed store of `mini-cobol` and the rows its
/// `domain-run` wrote over that tree.
fn fixture_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/domain_v3")
}

/// Load the fixture store into `db` (a new file), keeping `rows` of its annotations or none.
fn load_fixture_store(db: &std::path::Path, rows: bool) {
    for ext in ["", "-wal", "-shm"] {
        let mut p = db.as_os_str().to_owned();
        p.push(ext);
        let _ = std::fs::remove_file(p);
    }
    let sql = std::fs::read_to_string(fixture_dir().join("mini-cobol.sql")).unwrap();
    let con = rusqlite::Connection::open(db).unwrap();
    con.execute_batch(&sql).unwrap();
    if !rows {
        con.execute("DELETE FROM annotations", []).unwrap();
    }
}

/// What the run step writes, applied while it holds: the fixture's rows (`fixture` is the full
/// fixture store), or a tombstone on every live key (a run that found nothing retires them).
fn write_run_rows(db: &str, fixture: &std::path::Path, retire: bool) {
    if !retire {
        let con = rusqlite::Connection::open(db).unwrap();
        con.execute("ATTACH DATABASE ?1 AS f", [fixture.to_str().unwrap()])
            .unwrap();
        con.execute("INSERT INTO annotations SELECT * FROM f.annotations", [])
            .unwrap();
        return;
    }
    let mut store = open_store(Some(db)).unwrap();
    let mut keys = std::collections::BTreeSet::new();
    for ty in [
        "business_rule",
        "domain_target",
        "rule_group",
        "domain_followup",
    ] {
        for (sym, a) in store.annotations_by_type(ty).unwrap() {
            keys.insert((sym, ty, a.key));
        }
    }
    assert!(!keys.is_empty(), "the first run's rows are there to retire");
    for (sym, ty, key) in keys {
        store
            .annotate(&sym, Annotation::new(ty, key, r#"{"status": "retired"}"#))
            .unwrap();
    }
}

/// Hold until the run step reaches its handshake, apply `rows`, and let it go on.
fn while_the_step_holds(tree: &std::path::Path, rows: impl FnOnce()) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !tree.join("ready").exists() {
        assert!(
            Instant::now() < deadline,
            "the run step never reached its handshake"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    rows();
    std::fs::write(tree.join("written"), b"").unwrap();
}

/// The folder beside the checkout `repo` that holds its run trees (core#1109).
fn trees_beside(repo: &std::path::Path) -> std::path::PathBuf {
    let mut name = repo.file_name().unwrap().to_os_string();
    name.push(".wicked");
    repo.with_file_name(name)
}

/// Where the engine mints run `id`'s tree for the checkout `repo` (core#1109).
fn run_tree(repo: &std::path::Path, id: &str) -> std::path::PathBuf {
    trees_beside(repo).join("wicked-worktrees").join(id)
}

fn rm_repo(repo: &std::path::Path) {
    let _ = std::fs::remove_dir_all(trees_beside(repo));
    let _ = std::fs::remove_dir_all(repo);
}

/// A git checkout; with `fixture_tree`, it holds the fixture's tree (the files the rows were
/// written over) and the launcher's handshake marker.
fn make_git_repo(name: &str, fixture_tree: bool) -> std::path::PathBuf {
    let repo = std::env::temp_dir().join(format!("wicked-core-de-{name}-{}", std::process::id()));
    rm_repo(&repo);
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .output()
            .unwrap()
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    git(&["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join("README.md"), "hello").unwrap();
    if fixture_tree {
        copy_dir(&fixture_dir().join("mini-cobol"), &repo);
        std::fs::write(repo.join("e2e-handshake"), "").unwrap();
    }
    git(&["add", "."]);
    git(&["commit", "-qm", "init"]);
    repo
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let p = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &p);
        } else {
            std::fs::copy(e.path(), p).unwrap();
        }
    }
}

/// The process-wide environment, set once: the engine's own binary, the repository graph root,
/// the fixture garden generation and a stub indexer.
fn setup_env() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        // `run_tool_cmd` resolves `wicked-core` through `WICKED_CORE_EXE`; under `cargo test` the
        // current exe is this harness, so pin the binary Cargo built (and the channel hands the
        // same value to the step's client).
        std::env::set_var("WICKED_CORE_EXE", BIN);
        std::env::set_var(
            "WICKED_ESTATE_REPO_GRAPH_ROOT",
            std::env::temp_dir().join(format!("wicked-core-de-graphs-{}", std::process::id())),
        );
        // The garden root rides every Tool step from the admitted generation (core#802): publish
        // a fixture generation as crew lays one out, with the launcher above. Canonical base: the
        // loader refuses an ancestor symlink, and the OS temp dir is one on macOS.
        let base = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("wicked-core-de-skills-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let snapshot =
            skills_fixture::publish_fixture_snapshot(&base, "000001", &["wicked-garden-domain"]);
        use std::os::unix::fs::PermissionsExt;
        let launcher = snapshot.join("scripts").join("wicked-garden");
        std::fs::create_dir_all(launcher.parent().unwrap()).unwrap();
        std::fs::write(&launcher, FIXTURE_GARDEN_LAUNCHER).unwrap();
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
        // The J6 v3 script `domain-run` checks for (core#1955 R2-6); the launcher plays it.
        let run_py = snapshot.join("scripts").join("domain").join("run.py");
        std::fs::create_dir_all(run_py.parent().unwrap()).unwrap();
        std::fs::write(&run_py, "# stand-in: the fixture launcher plays run.py\n").unwrap();
        std::env::set_var("WICKED_SKILLS_SNAPSHOT", &snapshot);
        std::env::remove_var("WICKED_WORKER_INHERIT_OPERATOR_CONFIG");
        // codex is benched on a repository-bound run unless one of these says otherwise: pin both
        // so the refusal below does not depend on the invoking shell.
        std::env::remove_var("WICKED_CODEX_WORKTREE_OK");
        std::env::remove_var("WICKED_SEAT_FLOOR");
        // The engine strips an inherited scope path from every child (`ENGINE_INTERNAL_ENV`): an
        // ambient one here must never reach a step.
        std::env::set_var("WICKED_DOMAIN_SCOPE", "/nonexistent/ambient-scope.json");
        // garden#1370: a domain-extraction launch re-indexes the repo graph at its base; a hermetic
        // runner has no wicked-estate, and the fixture graph is what the run reads.
        let indexer = base.join("wicked-estate-stub");
        std::fs::write(&indexer, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&indexer, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::env::set_var("WICKED_ESTATE_BIN", &indexer);
    });
}

fn spec(
    run_id: &str,
    repo_ref: &str,
    workflow: &str,
    scope: Option<serde_json::Value>,
) -> LaunchSpec {
    LaunchSpec {
        scope,
        base_ref: None,
        project_id: None,
        problem: "Map the refund rules.".into(),
        clis: vec![cli("claude"), cli("codex")],
        entity_mode: EntityMode::Shared,
        session_id: run_id.into(),
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        repo_ref: Some(repo_ref.into()),
        workflow: Some(workflow.into()),
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        optional_read_roots: Vec::new(),
        project_graph: None,
        plan: None,
        deliver_step: None,
        exclude_seats: Vec::new(),
        evidence_root: None,
        primary: None,
        reduced_assurance: true,
        stand_in: Default::default(),
        power: false,
        voice: None,
        deliverables: Vec::new(),
        qe_acceptance: Default::default(),
    }
}

/// The run's terminal event, and whether it paused at any gate on the way.
fn wait_terminal(
    events: &std::sync::mpsc::Receiver<CoreEvent>,
    session: &str,
) -> (CoreEvent, bool) {
    let deadline = Instant::now() + Duration::from_secs(80);
    let mut gated = false;
    while Instant::now() < deadline {
        match events.recv_timeout(Duration::from_millis(500)) {
            Ok(ev) => {
                if matches!(&ev, CoreEvent::AwaitingHuman { session: s, .. } if s == session) {
                    gated = true;
                    return (ev, gated);
                }
                let terminal = matches!(&ev, CoreEvent::SessionCompleted { session: s } if s == session)
                    || matches!(&ev, CoreEvent::SessionFailed { session: s, .. } if s == session)
                    || matches!(&ev, CoreEvent::RunCancelled { session: s, .. } if s == session);
                if terminal {
                    return (ev, gated);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(e) => panic!("event stream closed: {e}"),
        }
    }
    panic!("run {session} reached no terminal state in 80 s");
}

/// A core on a scratch store with the recording seats, and a registered repository whose store
/// holds the fixture's indexed nodes and no rows yet.
struct Rig {
    core: Core,
    seats: Arc<Seats>,
    repo: std::path::PathBuf,
    entry: RepoEntry,
    dir: std::path::PathBuf,
}

impl Drop for Rig {
    fn drop(&mut self) {
        rm_repo(&self.repo);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn rig(name: &str, fixture_tree: bool) -> Rig {
    setup_env();
    let dir = std::env::temp_dir().join(format!("wicked-core-de-db-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let seats = Arc::new(Seats::default());
    let core = Core::spawn_with_engine(
        dir.join("estate.db").to_str().unwrap().to_string(),
        Arc::new(StubDispatcher),
        seats.clone(),
    );
    let repo = make_git_repo(name, fixture_tree);
    let entry = core
        .register_repo(RepoSpec {
            name: name.into(),
            root_path: repo.to_str().unwrap().into(),
            registered_at: 0,
        })
        .expect("register repo");
    std::fs::create_dir_all(std::path::Path::new(&entry.code_graph_db).parent().unwrap()).unwrap();
    load_fixture_store(std::path::Path::new(&entry.code_graph_db), false);
    Rig {
        core,
        seats,
        repo,
        entry,
        dir,
    }
}

/// The scope as studio's Start carries it, with a non-ASCII phrase so "byte-equal" means it.
fn a_scope() -> serde_json::Value {
    serde_json::json!({
        "intent": "refunds",
        "phrases": ["refund", "remboursé"],
        "scope_hash": "sha256:abc",
        "description": "How refunds are approved."
    })
}

/// The shipped `domain-extraction` steps, as the built-in list holds them.
fn shipped_steps() -> Vec<wicked_core::PlanStep> {
    wicked_core::builtin_presets()
        .into_iter()
        .find(|(n, _)| *n == "domain-extraction")
        .map(|(_, s)| s)
        .expect("the built-in preset")
}

#[test]
fn the_shipped_preset_runs_armed_with_its_scope_and_ends_with_no_gate() {
    let rig = rig("armed", true);
    let (core, seats, repo, entry) = (&rig.core, &rig.seats, &rig.repo, &rig.entry);
    let scope = a_scope();
    let run_id = format!("de-armed-{}", std::process::id());
    let ev = core.subscribe();
    core.launch_run(spec(
        &run_id,
        &entry.id,
        "domain-extraction",
        Some(scope.clone()),
    ))
    .expect("launch domain-extraction with a scope");
    let tree = run_tree(repo, &run_id).join(".wicked/domain/e2e");
    let fixture = rig.dir.join("fixture.db");
    load_fixture_store(&fixture, true);
    while_the_step_holds(&tree, || {
        write_run_rows(&entry.code_graph_db, &fixture, false)
    });
    let (end, gated) = wait_terminal(&ev, &run_id);
    let read = |f: &str| std::fs::read_to_string(tree.join(f)).unwrap_or_default();
    assert!(
        matches!(end, CoreEvent::SessionCompleted { .. }) && !gated,
        "the run ends Done with no gate: {end:?}; args: {}; a.err: {}; c.err: {}",
        read("args"),
        read("a.err"),
        read("c.err")
    );

    // The step ran garden's run.py with the store, the checkout and the scope file.
    let args = read("args");
    let lines: Vec<&str> = args.lines().collect();
    assert_eq!(lines[0], "--db", "{args}");
    assert_eq!(lines[1], entry.code_graph_db, "{args}");
    assert_eq!(lines[2], "--repo-root", "{args}");
    assert_eq!(lines[3], entry.root_path, "{args}");
    assert_eq!(lines[4], "--scope", "{args}");
    let scope_path = lines
        .iter()
        .find_map(|l| l.strip_prefix("scope_env="))
        .unwrap_or_default();
    assert_eq!(
        lines[5], scope_path,
        "--scope is the WICKED_DOMAIN_SCOPE file: {args}"
    );
    // …in the step's own channel directory, outside the run's tree, gone when the step ended.
    let scope_file = std::path::Path::new(scope_path);
    assert_eq!(scope_file.file_name().unwrap(), "scope.json", "{args}");
    let channel_dir = scope_file.parent().unwrap();
    assert!(
        channel_dir
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(&format!("wmc-{}-", std::process::id())),
        "{args}"
    );
    assert!(
        !scope_path.contains(run_tree(repo, &run_id).to_str().unwrap()),
        "{args}"
    );
    assert!(
        !channel_dir.exists(),
        "the channel directory went with the step"
    );
    assert_ne!(
        scope_path, "/nonexistent/ambient-scope.json",
        "the ambient value is stripped"
    );
    // …holding exactly the launch's scope.
    let copied: serde_json::Value = serde_json::from_str(&read("scope.json"))
        .unwrap_or_else(|e| panic!("{e}: {}", read("scope.err")));
    assert_eq!(copied, scope);

    // Both claude calls were served by a seat, each its own ballot of this run.
    assert_eq!(read("a.rc").trim(), "0", "{}", read("a.err"));
    assert_eq!(read("b.rc").trim(), "0", "{}", read("b.err"));
    assert!(read("a.out").contains("claude: "), "{}", read("a.out"));
    assert!(
        read("a.out").contains("Name one rule."),
        "{}",
        read("a.out")
    );
    let seen = seats.seen.lock().unwrap().clone();
    let claude: Vec<&Answered> = seen
        .iter()
        .filter(|(_, seat, _)| seat.as_deref() == Some("claude"))
        .collect();
    assert_eq!(claude.len(), 2, "{seen:?}");
    assert_ne!(claude[0].0, claude[1].0, "distinct ballot ids: {seen:?}");
    for (ballot, _, parent) in &claude {
        assert_ne!(ballot, &run_id, "a ballot is not the run itself");
        assert_eq!(parent.as_deref(), Some(run_id.as_str()), "{seen:?}");
    }
    // The codex call is refused by name: a repository-bound run benches codex at launch, and the
    // channel admits only the run's own seats (core#1905). No codex seat ran.
    assert_ne!(read("c.rc").trim(), "0");
    assert!(
        read("c.err").contains("the `codex` seat is benched for this run"),
        "{}",
        read("c.err")
    );
    assert!(!tree.join("c.out").exists());
    assert!(
        !seen
            .iter()
            .any(|(_, seat, _)| seat.as_deref() == Some("codex")),
        "{seen:?}"
    );

    // (C2, F28) The graph step built 1.2.0 from the rows the run step wrote: the Checked statement
    // is in the model, on its code, and the run's open work is counted.
    let graph = |db: &str| {
        wicked_governance::load_domain_graph(&open_store(Some(db)).unwrap(), None)
            .unwrap()
            .expect("the graph step built a model")
    };
    let doc = graph(&entry.code_graph_db);
    assert_eq!(doc["metadata"]["schema_version"], "1.2.0", "{doc:#}");
    let prog = &doc["domains"]["app/cbl/PAYCALC1.cbl"]["requirements"];
    let rules: Vec<&serde_json::Value> = prog
        .as_object()
        .unwrap_or_else(|| panic!("no PAYCALC1 domain: {doc:#}"))
        .values()
        .flat_map(|r| r["business_rules"].as_array().unwrap())
        .collect();
    let checked = rules
        .iter()
        .find(|r| r["status"] == "Checked")
        .unwrap_or_else(|| panic!("no Checked statement: {doc:#}"));
    assert_eq!(
        checked["id"],
        "ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA().#0"
    );
    assert_eq!(checked["source_ref"], "app/cbl/PAYCALC1.cbl#L20-L29");
    assert_eq!(
        doc["metadata"]["accounting"],
        serde_json::json!({"targets": 4, "accounted": 2, "open": 3, "stale": 0})
    );

    // A second run that finds nothing retires every row: the model empties, nothing is carried.
    let again = format!("{run_id}-2");
    let ev = core.subscribe();
    core.launch_run(spec(&again, &entry.id, "domain-extraction", Some(scope)))
        .expect("launch the second run");
    let tree2 = run_tree(repo, &again).join(".wicked/domain/e2e");
    while_the_step_holds(&tree2, || {
        write_run_rows(&entry.code_graph_db, &fixture, true)
    });
    let (end, gated) = wait_terminal(&ev, &again);
    assert!(
        matches!(end, CoreEvent::SessionCompleted { .. }) && !gated,
        "{end:?}"
    );
    let doc = graph(&entry.code_graph_db);
    assert_eq!(doc["domains"], serde_json::json!({}), "{doc:#}");
    assert_eq!(doc["metadata"]["accounting"]["targets"], 0, "{doc:#}");
}

/// (coord ruling B') A `domain-extraction` launch with no scope is refused by name, and so is a
/// scope on another launch or a malformed one; nothing is persisted.
#[test]
fn a_launch_without_its_scope_or_with_a_stray_one_is_refused_by_name() {
    let rig = rig("refused", false);
    let (core, entry) = (&rig.core, &rig.entry);
    let pid = std::process::id();
    let none = core
        .launch_run(spec(
            &format!("de-none-{pid}"),
            &entry.id,
            "domain-extraction",
            None,
        ))
        .expect_err("a domain-extraction launch needs a scope");
    assert!(
        none.to_string()
            .contains("domain-extraction needs a scope (Start carries it from the preview)"),
        "{none}"
    );
    let stray = core
        .launch_run(spec(
            &format!("de-stray-{pid}"),
            &entry.id,
            "feature",
            Some(serde_json::json!({"intent": "refunds"})),
        ))
        .expect_err("a scope is only for domain-extraction");
    assert!(
        stray
            .to_string()
            .contains("a scope is only for a domain-extraction launch"),
        "{stray}"
    );
    let bad = core
        .launch_run(spec(
            &format!("de-bad-{pid}"),
            &entry.id,
            "domain-extraction",
            Some(serde_json::json!({"intent": "refunds", "phrases": null})),
        ))
        .expect_err("a malformed scope is refused");
    assert!(
        bad.to_string()
            .contains("the scope's phrases are not a list of non-empty strings"),
        "{bad}"
    );
    for id in ["de-none", "de-stray", "de-bad"] {
        assert!(
            core.session_detail(&format!("{id}-{pid}"))
                .unwrap()
                .is_none(),
            "{id}: nothing persisted"
        );
    }
}

/// The arming rule through the actor (coord fold 1, C0): a project preset that shadows the
/// built-in's NAME with its exact steps is armed (the existing rule, asserted as such); a project
/// preset under another name with byte-identical steps, and a registered workflow with the same
/// `domain-run` step, are not. They cannot carry a scope (only `domain-extraction` takes one), so
/// their step refuses by name before the launcher runs, and no seat answers anything.
#[test]
fn only_the_shipped_name_arms_its_step_and_a_copy_under_another_name_runs_no_seat() {
    let rig = rig("ident", false);
    let (core, seats, repo, entry) = (&rig.core, &rig.seats, &rig.repo, &rig.entry);
    let pid = std::process::id();
    let project = core.project_create("j6-ident", None).unwrap();
    for name in ["domain-extraction", "my-domain"] {
        core.put_preset(PresetSpec {
            name: name.into(),
            project_id: Some(project.id.clone()),
            steps: shipped_steps(),
            created_by: "api".into(),
        })
        .unwrap_or_else(|e| panic!("put_preset {name}: {e}"));
    }
    let def = serde_json::json!({
        "id": "my-domain-flow",
        "phases": [{
            "id": "domain-run", "kind": "recon", "gate": "auto",
            "executor": serde_json::to_value(&shipped_steps()[0].executor).unwrap()
        }]
    });
    core.register_workflow(def.to_string())
        .expect("register workflow");

    // The same-name project shadow with the exact steps: armed.
    let shadow = format!("de-shadow-{pid}");
    let mut s = spec(&shadow, &entry.id, "domain-extraction", Some(a_scope()));
    s.project_id = Some(project.id.clone());
    let ev = core.subscribe();
    core.launch_run(s).expect("launch the shadow");
    let (end, gated) = wait_terminal(&ev, &shadow);
    let tree = run_tree(repo, &shadow).join(".wicked/domain/e2e");
    let read = |f: &str| std::fs::read_to_string(tree.join(f)).unwrap_or_default();
    assert!(
        matches!(end, CoreEvent::SessionCompleted { .. }) && !gated,
        "{end:?}: {}",
        read("a.err")
    );
    assert_eq!(read("a.rc").trim(), "0", "{}", read("a.err"));
    let served = seats.seen.lock().unwrap().len();
    assert_eq!(served, 2, "the shadow's two claude calls");

    // Another name, byte-identical steps; and a registered workflow with the same step.
    for (run, workflow, project_id) in [
        (
            format!("de-copy-{pid}"),
            "my-domain",
            Some(project.id.clone()),
        ),
        (format!("de-flow-{pid}"), "my-domain-flow", None),
    ] {
        let mut s = spec(&run, &entry.id, workflow, None);
        s.project_id = project_id;
        let ev = core.subscribe();
        core.launch_run(s)
            .unwrap_or_else(|e| panic!("launch {workflow}: {e}"));
        let (end, _) = wait_terminal(&ev, &run);
        assert!(
            !matches!(end, CoreEvent::SessionCompleted { .. }),
            "{workflow}: its step has no scope and refuses: {end:?}"
        );
        assert!(
            !run_tree(repo, &run)
                .join(".wicked/domain/e2e/args")
                .exists(),
            "{workflow}: the launcher never ran"
        );
    }
    assert_eq!(
        seats.seen.lock().unwrap().len(),
        served,
        "no seat answered a copy"
    );
}
