//! (DES-CATALOG-002 CS6) The `run-cost` preset through the REAL engine (`Core::spawn_with_engine`), repo-less,
//! with a scripted seat and a stand-in for crew's `wicked-crew`:
//!
//! 1. The `facts` Tool is crew's fixed command, armed with its unit's capability token: while it runs, the token
//!    resolves through the engine's own resolver (`resolve_estate_scope_json`, what crew's `POST /insights/facts`
//!    verifies it with) to this run's `facts` unit. Before CS6 only an Apps run's Tools were armed, so the command
//!    ran with no token and crew's route could never place it.
//! 2. The facts the command printed reach the `model` step as its prior output, and `check` (fidelity to source)
//!    runs on a seat other than the model's.

// The `wicked-crew` stand-in is POSIX shell.
#![cfg(unix)]

use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

use wicked_core::{
    Core, EntityMode, HumanConfirm, LaunchSpec, SessionStatus, StepInput, StepOutput, StepRunner,
    StepStatus,
};

/// The stand-in `wicked-crew` lives in this process's own dir.
fn crew_dir() -> PathBuf {
    std::env::temp_dir().join(format!("wicked-core-run-cost-crew-{}", std::process::id()))
}

/// Route emissions to a temp spool (core#311); point `wicked-crew` at the stand-in and give the daemon a broker URL,
/// so a crew Tool can be armed with its unit's token.
/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only sets process env vars.
#[ctor::ctor(unsafe)]
fn arm_env() {
    wicked_apps_core::emit::hermetic_test_spool();
    std::env::set_var("WICKED_CREW_URL", "http://127.0.0.1:9");
    std::env::set_var(
        "WICKED_CREW_BIN",
        crew_dir()
            .join("wicked-crew")
            .to_string_lossy()
            .into_owned(),
    );
}

/// The facts block the stand-in prints (crew's renders the same shape).
const FACTS_MARKER: &str = "RUN-COST FACTS v1";

/// crew's `wicked-crew` stand-in. `insights-facts` logs whether it holds a token, hands that token to the test
/// (which resolves it while the command is still running, as crew's route does), waits for the test's word, and
/// prints a facts block.
const CREW_STUB: &str = r#"#!/bin/sh
dir=$(cd "$(dirname "$0")" && pwd)
run="${WICKED_RUN_ID:-norun}"
echo "argv=$* unit=${WICKED_RUN_UNIT} token=${WICKED_MCP_TOKEN:+bound}" >> "$dir/$run.log"
case "$1" in
  insights-facts)
    if [ -n "$WICKED_MCP_TOKEN" ]; then
      printf '%s' "$WICKED_MCP_TOKEN" > "$dir/$run.token.tmp" && mv "$dir/$run.token.tmp" "$dir/$run.token"
      i=0
      while [ ! -f "$dir/$run.go" ] && [ $i -lt 600 ]; do sleep 0.1; i=$((i+1)); done
    fi
    echo "RUN-COST FACTS v1 (stand-in)"
    echo "G1 workflow=bug runs=3 passed=2 failed=1 cancelled=0"
    echo "G1.spend (USD, fully priced runs only): n=3 p50=1.500000 p75=2.000000 p90=2.400000 total=5.000000 partial_total=0.000000"
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
        // Written by a `/bin/sh` child so this process never holds a writable descriptor to a file it later execs
        // (ETXTBSY on Linux).
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
    });
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

type Inputs = Arc<Mutex<Vec<StepInput>>>;

/// A seat that plays `model` and `check`: the model cites the facts, the check passes it.
struct ScriptedSeat {
    inputs: Inputs,
}
impl StepRunner for ScriptedSeat {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        let phase = input.unit.phase_id().unwrap_or("").to_string();
        let output = if let Some(v) = wicked_core::stub_rule_verdicts(&input.unit.description) {
            v
        } else if input.unit.session_id == "validator" {
            "PASS\nthe work meets the criterion\nPASS".to_string()
        } else {
            self.inputs.lock().unwrap().push(input.clone());
            match phase.as_str() {
                "model" => "Population: 3 terminal bug runs (G1). For the volume: 40 x G1.spend.p50 = 40 x 1.500000 = 60.00 USD".to_string(),
                "check" => "checked 1, held 1, did not hold 0\nVERDICT: PASS".to_string(),
                other => format!("did: {other}"),
            }
        };
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

fn spec(run_id: &str) -> LaunchSpec {
    LaunchSpec {
        base_ref: None,
        project_id: None,
        problem: "what would 40 bug fixes a month cost?".into(),
        clis: ["claude", "codex"].iter().map(|k| cli(k)).collect(),
        entity_mode: EntityMode::Shared,
        session_id: run_id.into(),
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        repo_ref: None,
        workflow: Some("run-cost".into()),
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

#[test]
fn run_costs_facts_tool_carries_a_token_crews_route_can_place_and_feeds_the_model() {
    install_crew_stub();
    let run_id = "r-run-cost";
    let dir = std::env::temp_dir().join(format!("wicked-core-run-cost-db-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let inputs: Inputs = Arc::new(Mutex::new(Vec::new()));
    let core = Core::spawn_with_engine(
        dir.join("estate.db").to_str().unwrap().to_string(),
        Arc::new(StubDispatcher),
        Arc::new(ScriptedSeat {
            inputs: inputs.clone(),
        }),
    );
    core.launch_run(spec(run_id)).expect("launch");

    // While the facts command runs, its token resolves to THIS run's facts unit (crew's route verifier).
    let crew = crew_dir();
    let token_file = crew.join(format!("{run_id}.token"));
    let start = Instant::now();
    let mut placed = None;
    let mut last = None;
    while start.elapsed() < WAIT_BUDGET {
        if let Ok(token) = std::fs::read_to_string(&token_file) {
            let scope = wicked_core::resolve_estate_scope_json(
                &serde_json::json!({ "token": token }).to_string(),
            )
            .expect("the live token resolves");
            placed = Some(serde_json::from_str::<serde_json::Value>(&scope).unwrap());
            break;
        }
        if let Ok(views) = core.sessions_detail() {
            if let Some(v) = views.iter().find(|v| v.session.id == run_id) {
                last = Some(format!(
                    "{:?} {:?}",
                    v.session.status,
                    v.units
                        .iter()
                        .map(|u| format!("{:?}:{:?}:{:?}", u.phase_id(), u.status, u.denial_reason))
                        .collect::<Vec<_>>()
                ));
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
    std::fs::write(crew.join(format!("{run_id}.go")), "").unwrap();
    let log = std::fs::read_to_string(crew.join(format!("{run_id}.log"))).unwrap_or_default();
    let placed =
        placed.unwrap_or_else(|| panic!("the facts command held no token: {log}; run {last:?}"));
    assert_eq!(placed["unit"]["runId"], run_id, "{placed}");
    // crew places the step as its route does: the unit at that ord in the run's own units.
    let ord = placed["unit"]["ord"].as_u64().expect("an ord") as u32;
    let at_ord = core
        .sessions_detail()
        .unwrap()
        .into_iter()
        .find(|v| v.session.id == run_id)
        .and_then(|v| v.units.into_iter().find(|u| u.ord == ord))
        .expect("the unit the token names");
    assert_eq!(at_ord.phase_id(), Some("facts"), "{placed}");
    assert_eq!(
        at_ord.tool_cmd.as_deref(),
        Some(&["wicked-crew".to_string(), "insights-facts".to_string()][..])
    );
    assert_eq!(log.lines().count(), 1, "{log}");
    assert!(
        log.contains("argv=insights-facts ") && log.contains("token=bound"),
        "{log}"
    );

    // The run completes: model, then a check on another seat.
    let start = Instant::now();
    let mut status = None;
    while start.elapsed() < WAIT_BUDGET {
        let v = core
            .sessions_detail()
            .unwrap()
            .into_iter()
            .find(|v| v.session.id == run_id)
            .unwrap();
        status = Some(v.session.status);
        if matches!(
            v.session.status,
            SessionStatus::Failed | SessionStatus::Cancelled | SessionStatus::Completed
        ) {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let v = core
        .sessions_detail()
        .unwrap()
        .into_iter()
        .find(|v| v.session.id == run_id)
        .unwrap();
    let units: Vec<String> = v
        .units
        .iter()
        .map(|u| format!("{:?}:{:?}:{:?}", u.phase_id(), u.status, u.denial_reason))
        .collect();
    assert_eq!(status, Some(SessionStatus::Completed), "{units:?}");
    let seat_of = |phase: &str| {
        v.units
            .iter()
            .find(|u| u.phase_id() == Some(phase))
            .and_then(|u| u.assigned_cli.clone())
            .unwrap_or_else(|| panic!("{phase} has a seat: {units:?}"))
    };
    assert_ne!(
        seat_of("model"),
        seat_of("check"),
        "the check keeps off the model's seat"
    );
    let inputs = inputs.lock().unwrap();
    let model = inputs
        .iter()
        .find(|i| i.unit.phase_id() == Some("model"))
        .expect("the model ran");
    assert!(
        model
            .prior_outputs
            .iter()
            .any(|p| p.output.contains(FACTS_MARKER) && p.label.contains("depends_on `facts`")),
        "the model was handed the facts: {:?}",
        model.prior_outputs
    );
    let _ = std::fs::remove_dir_all(&dir);
}
