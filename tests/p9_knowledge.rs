//! P9 KNOWLEDGE — the orchestrator's document knowledge base (wired to `wicked_knowledge_mcp`).
//! Proves ingest → recall round-trips through the engine.

use std::sync::Arc;

use wicked_core::{Core, StepInput, StepOutput, StepRunner, StepStatus};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

struct StubDispatcher;
impl Dispatcher for StubDispatcher {
    fn dispatch(&self, cli: &AgenticCli, _t: &CouncilTask) -> Option<Vote> {
        Some(Vote {
            cli: cli.key.clone(),
            recommendation: cli.key.clone(),
            top_risk: "none".into(),
            change_my_mind: "no".into(),
            disqualifier: None,
            confidence: Confidence::default(),
            provenance: "stub".into(),
        })
    }
}

struct OkRunner;
impl StepRunner for OkRunner {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        StepOutput {
            run_id: i.run_id.clone(),
            unit_ix: i.unit_ix,
            attempt: i.attempt,
            output: "ok".into(),
            status: StepStatus::Ok,
            usage: None,
            files: Vec::new(),
        }
    }
}

fn db_path(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!("wicked-core-p9-{name}"));
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
    }
}

#[test]
fn ingest_then_recall_round_trips() {
    let _ = cli("a"); // keep the helper referenced
    let core = Core::spawn_with_engine(db_path("kb"), Arc::new(StubDispatcher), Arc::new(OkRunner));
    let chunks = vec![
        "The deploy gate blocks any tool-call containing rm -rf in the exec phase.".to_string(),
        "Worktrees are created per-run under .wicked/worktrees and reaped on terminal status."
            .to_string(),
    ];
    let n = core
        .ingest_knowledge("Orchestrator governance notes", chunks)
        .expect("ingest");
    assert_eq!(n, 2, "both chunks ingested");

    let hits = core.recall_knowledge("deploy gate", 5).expect("recall");
    assert!(
        hits.iter().any(|h| h.content.contains("deploy gate")),
        "recall surfaces the ingested chunk, got: {hits:?}"
    );
}

#[test]
fn recall_empty_not_error_for_unknown_query() {
    let core = Core::spawn_with_engine(
        db_path("kb-empty"),
        Arc::new(StubDispatcher),
        Arc::new(OkRunner),
    );
    let hits = core
        .recall_knowledge("nothing ingested", 5)
        .expect("recall ok");
    assert!(hits.is_empty());
}
