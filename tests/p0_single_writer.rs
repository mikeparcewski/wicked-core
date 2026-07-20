//! P0 proving test — single-writer reconciliation for the out-of-process gate-hook.
//!
//! Proves the §1 invariant survives a REAL out-of-process gate-hook subprocess:
//!  (a) the hook (a separate OS process) runs while the actor holds the store open and the actor
//!      keeps serving reads with NO `SQLITE_BUSY` — because the hook never writes the store;
//!  (b) draining the hook's `decisions.ndjson` is idempotent (a re-drain yields the claim once);
//!  (c) a drained `Deny` vetoes the run's governance gate (the phase resolves non-approving).

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use wicked_apps_core::open_store;
use wicked_governance::{register_policy, Effect, Policy, Severity, Trigger};
use wicked_orchestration::{get_phase, PhaseStatus};

use wicked_core::Core;

/// Path to the freshly-built `wicked-core` binary (cargo sets this for integration tests).
const GATE_HOOK_BIN: &str = env!("CARGO_BIN_EXE_wicked-core");

fn tmp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wicked-core-p0-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A deny policy selected for `phase` whose regex trigger fires when the context contains `pattern`.
fn deny_policy(id: &str, phase: &str, pattern: &str) -> Policy {
    Policy {
        id: id.to_string(),
        kind: "guard".to_string(),
        applies_to: vec![phase.to_string()],
        effect: Effect::Deny,
        trigger: Trigger {
            contains: Some(pattern.to_string()),
        },
        obligations: vec![],
        criteria: String::new(),
        severity: Severity::High,
        rule: format!("deny anything containing {pattern}"),
    }
}

/// Spawn the gate-hook subprocess for one tool-call, returning its exit code. The decisions log is
/// addressed by the ABSOLUTE `WICKED_DECISIONS_PATH` env (never cwd-relative).
fn run_hook(db: &str, decisions: &PathBuf, scope: &str, phase: &str, tool_json: &str) -> i32 {
    let mut child = Command::new(GATE_HOOK_BIN)
        .args(["gate-hook", "--scope", scope, "--phase", phase, "--db", db])
        .env("WICKED_DECISIONS_PATH", decisions)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn gate-hook subprocess");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(tool_json.as_bytes())
        .unwrap();
    child.wait().unwrap().code().unwrap_or(-1)
}

#[test]
fn gate_hook_is_single_writer_idempotent_and_vetoes() {
    let dir = tmp_dir("single-writer");
    let db = dir.join("estate.db");
    let db_str = db.to_str().unwrap().to_string();
    let decisions = dir.join("decisions.ndjson");
    let run_id = "p0run";

    // ── Seed a deny policy BEFORE the actor exists (one sequential writer; no contention). ──
    {
        let mut store = open_store(Some(&db_str)).expect("open store to seed policy");
        register_policy(&mut store, &deny_policy("no-secrets", "exec", "DENYME"))
            .expect("register deny policy");
        // store dropped here — its connection is released before the actor opens the file.
    }

    // ── The actor now owns the writable store. ──
    let core = Core::spawn(db_str.clone());

    // Seed the decisions log: an allow-phase call (recon: no deny policy applies) and a deny call
    // (exec: trigger fires). These are sequential just to capture the two exit codes cleanly.
    let allow_code = run_hook(
        &db_str,
        &decisions,
        "p0scope",
        "recon",
        r#"{"tool_name":"Bash","tool_input":{"command":"echo hello"}}"#,
    );
    let deny_code = run_hook(
        &db_str,
        &decisions,
        "p0scope",
        "exec",
        r#"{"tool_name":"Bash","tool_input":{"command":"echo DENYME please"}}"#,
    );
    assert_eq!(allow_code, 0, "an allowed tool-call exits 0");
    assert_eq!(
        deny_code, 2,
        "a denied tool-call exits 2 (claude aborts the call)"
    );

    // The hook appended three lines per invocation:
    //   1. a hook-fired sentinel (core#34),
    //   2. a tool-call annotation (_wicked_tool_call) — written together with the claim
    //      under a single lock so they stay adjacent,
    //   3. the conformance claim.
    let lines: Vec<String> = std::fs::read_to_string(&decisions)
        .expect("decisions log written by the hook")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect();
    assert_eq!(
        lines.len(),
        6,
        "one sentinel + one annotation + one claim line per hook invocation"
    );

    // Parse the deny claim id from the ndjson the hook produced.
    let deny_claim_id = lines
        .iter()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v.get("decision").and_then(|d| d.as_str()) == Some("deny"))
        .and_then(|v| {
            v.get("claim_id")
                .and_then(|c| c.as_str())
                .map(str::to_string)
        })
        .expect("a deny claim line exists");

    // ── (a) The decisive single-writer proof: the hook does NOT write the claim to the store — only
    // the actor does. Under the OLD design the hook called `conform(&mut store)` from the subprocess,
    // so the claim would already be a store node here. After the fix the hook only appended ndjson,
    // so BEFORE the actor drains, the claim is ABSENT from the store. (This distinguishes the two
    // designs without provoking the unfixed P4b open-time write contention — see gate_hook.rs caveat.)
    {
        let store = open_store(Some(&db_str)).expect("open store to check pre-drain state");
        assert_eq!(
            wicked_core::count_claims(&store, &deny_claim_id).expect("count"),
            0,
            "the out-of-process hook appended ndjson but did NOT write the claim to the store"
        );
    }

    // The actor still serves reads while an external hook subprocess runs (readers coexist under WAL).
    // The hook here writes to a SEPARATE decisions file so it doesn't pollute the `decisions` log the
    // drain assertions below count.
    let decisions_side = dir.join("decisions-side.ndjson");
    let reader = {
        let core = core.clone();
        std::thread::spawn(move || {
            for _ in 0..100 {
                core.sessions()
                    .expect("actor read must not fail during an external hook");
            }
        })
    };
    let _ = run_hook(
        &db_str,
        &decisions_side,
        "p0scope",
        "recon",
        r#"{"tool_name":"Bash","tool_input":{"command":"echo hello"}}"#,
    );
    reader.join().expect("concurrent reader thread panicked");

    // ── (b) Drain twice — idempotent. ──
    let first = core
        .apply_hook_decisions(run_id, decisions.clone())
        .expect("first drain");
    assert_eq!(first.applied, 2, "both claims applied on the first drain");
    assert_eq!(first.denied, 1, "exactly one Deny among them");

    let second = core
        .apply_hook_decisions(run_id, decisions.clone())
        .expect("second drain");
    assert_eq!(second.applied, 2, "the re-drain still reads both lines");

    // Each claim persisted exactly once (upsert-by-symbol), even after two drains. Read with a
    // fresh store handle (a reader — the actor remains the sole writer).
    {
        let store = open_store(Some(&db_str)).expect("open store to count claims");
        let count = wicked_core::count_claims(&store, &deny_claim_id).expect("count claims");
        assert_eq!(
            count, 1,
            "the deny claim is persisted exactly once across two drains"
        );

        // ── (c) The drained Deny vetoed the run's governance gate. ──
        let phase_id = format!("wf-{run_id}:exec");
        let phase = get_phase(&store, &phase_id)
            .expect("read phase")
            .expect("the drain opened + resolved the exec phase");
        assert_eq!(
            phase.status,
            PhaseStatus::Rejected,
            "a Deny drives the phase to Rejected — never approved by any route"
        );
        assert!(!phase.status.is_approving(), "Rejected is non-approving");

        // Sanity: the allow phase (recon) resolved approving, proving the gate isn't blanket-denying.
        let recon = get_phase(&store, &format!("wf-{run_id}:recon"))
            .expect("read recon phase")
            .expect("recon phase resolved");
        assert!(
            recon.status.is_approving(),
            "an allowed call drives its phase to an approving status"
        );
    }
}
