//! FINDING-085 on the OTHER live verdict parser.
//!
//! `parse_agent_verdict` is not the only thing that turns an evaluator's prose into a governance
//! outcome. When `WICKED_BUS_DB` is set — the event-driven execution seam `wicked-crew` arms with
//! `--engine-exec` — `cli_runner::bus_request_agent_verdict` parses nothing: it publishes
//! `wicked.gate.eval.requested` and takes the `{pass, reasoning}` a daemon hands back.
//! `scripts/gate_eval_daemon.py` is that daemon, and it is the only implementation of that
//! subscriber anywhere in the ecosystem. Its `_parse_verdict` is a second, independent verdict
//! parser — so a fix that lands only in the Rust one is a control that is believed and absent.
//!
//! Which of the two parsed the captured incident cannot be established: `agentReasoning` is
//! truncated to 400 chars and the full evaluator output is retained nowhere (FINDING-087), and both
//! parsers can produce the stored string. Both are therefore held to the property.
//!
//! These tests run the REAL script, not a copy of its logic.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// The reply captured live (run 7ed97709 ord 4, second attempt): the model committed to a token,
/// reasoned its way to the opposite conclusion underneath it, and never wrote the word REJECT — so
/// nothing that hunts for a contradicting keyword can see it.
const CAPTURED: &str = "PASS - The work reports coverage progressing from 0.0 toward resolution, \
but the criterion requires coverage == 1.0 with zero unaccounted nodes. The WORK ends with the \
harness still running and never shows coverage reaching 1.0 - it explicitly states 766 unaccounted \
nodes and no completion. Wait - correcting myself: the first line must reflect the actual ...";

/// Import the daemon by path (its `main()` is under `__main__`, so importing runs nothing) and
/// expose it as `mod`. Prepended to every probe.
const PRELUDE: &str = "import importlib.util, sys, json\n\
                       spec = importlib.util.spec_from_file_location('ged', sys.argv[1])\n\
                       mod = importlib.util.module_from_spec(spec)\n\
                       spec.loader.exec_module(mod)\n";

/// Run `program` against the real daemon script and return its stdout.
///
/// A host with no Python FAILS rather than skips: the daemon is a Python program, so "no
/// interpreter" means this guard did not run, and a guard that silently does not run is the exact
/// failure mode this file exists to prevent.
fn probe(body: &str) -> String {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/gate_eval_daemon.py");
    assert!(
        script.is_file(),
        "the gate-eval daemon is missing at {} — the bus gate path has no evaluator",
        script.display()
    );
    let program = format!("{PRELUDE}{body}");

    let mut why = String::new();
    for py in ["python3", "python"] {
        let spawned = Command::new(py)
            .arg("-")
            .arg(&script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                why = format!("{py}: {e}");
                continue;
            }
        };
        child
            .stdin
            .as_mut()
            .expect("piped stdin")
            .write_all(program.as_bytes())
            .expect("write the probe program");
        let out = child
            .wait_with_output()
            .expect("interpreter ran to completion");
        assert!(
            out.status.success(),
            "{py} failed on the daemon: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        return String::from_utf8_lossy(&out.stdout).into_owned();
    }
    panic!(
        "no Python interpreter could run the gate-eval daemon ({why}) — the bus-path verdict \
         parser is UNVERIFIED on this host"
    );
}

/// A JSON string/array literal, which is also a valid Python literal for the ASCII text used here.
fn py_literal(v: &serde_json::Value) -> String {
    serde_json::to_string(v).expect("serialize probe input")
}

#[test]
fn the_bus_path_daemon_fails_closed_on_the_captured_incident() {
    let literal = py_literal(&serde_json::json!(CAPTURED));
    let out = probe(&format!(
        "print(json.dumps(mod._parse_verdict({literal})))\n"
    ));
    let (pass, reason): (bool, String) = serde_json::from_str(out.trim())
        .unwrap_or_else(|e| panic!("daemon probe output {out:?}: {e}"));
    assert!(
        !pass,
        "the bus-path daemon parsed the captured self-correcting reply as PASS: {reason}"
    );
    assert!(
        reason.contains("no closing verdict"),
        "the denial must name what was missing, not just deny: {reason}"
    );
}

#[test]
fn the_bus_path_daemon_keeps_a_compliant_verdict_and_denies_a_drifting_one() {
    let cases = py_literal(&serde_json::json!([
        "PASS\nthe deliverable is present\nPASS",
        "REJECT\nmissing coverage-report.json\nREJECT",
        "PASS\non reflection the criterion is not met\nREJECT",
        "REJECT\nactually it is fine\nPASS",
        "PASS the work is fine",
    ]));
    let out = probe(&format!(
        "for c in {cases}:\n    print(json.dumps(mod._parse_verdict(c)))\n"
    ));
    let verdicts: Vec<(bool, String)> = out
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("probe line {l:?}: {e}")))
        .collect();
    assert_eq!(verdicts.len(), 5, "expected one verdict per case: {out}");

    assert!(
        verdicts[0].0,
        "a contract-compliant PASS must still pass, or the daemon is a false-REJECT machine: {}",
        verdicts[0].1
    );
    assert!(!verdicts[1].0, "a compliant REJECT is still a reject");
    assert!(
        !verdicts[2].0 && verdicts[2].1.contains("verdict drift"),
        "opened PASS, closed REJECT ⇒ drift: {}",
        verdicts[2].1
    );
    assert!(
        !verdicts[3].0 && verdicts[3].1.contains("verdict drift"),
        "opened REJECT, closed PASS ⇒ drift (no favouritism toward denial): {}",
        verdicts[3].1
    );
    assert!(
        !verdicts[4].0,
        "an opening token with no closing commitment must fail closed: {}",
        verdicts[4].1
    );
}

/// The daemon's parser demands a closing line, so the daemon's PROMPT must ask for one — they sit
/// 40 lines apart in a file no compiler checks.
#[test]
fn the_bus_path_daemon_prompt_asks_for_the_closing_verdict_it_requires() {
    let out = probe("print(json.dumps(mod._build_prompt('crit', 'work')))\n");
    let prompt: String = serde_json::from_str(out.trim())
        .unwrap_or_else(|e| panic!("prompt probe output {out:?}: {e}"));
    // Needles built by CONCATENATION so this assertion can never match its own source text.
    let final_line = format!("{} {}", "FINAL", "line");
    let repeat_it = format!("{} that {} word", "repeat", "SAME");
    assert!(
        prompt.contains(&final_line) && prompt.contains(&repeat_it),
        "the daemon prompt never asks for a closing verdict while its parser requires one — every \
         honest evaluator would be denied. Prompt was:\n{prompt}"
    );
    assert!(
        prompt.contains("untrusted"),
        "the work must still be framed as untrusted data: {prompt}"
    );
}

/// DES-TEAMING-002 T5 / DES-001 §6.2 / core#625: the daemon's only judge seat is `claude`, and
/// its exclusion set is `work_author ∪ excluded_seats`, by instance or cli key. A request with no
/// `work_author` gets maximum exclusion (the author is unknown, so no seat is provably distinct).
#[test]
fn the_bus_path_daemon_never_judges_as_an_excluded_seat() {
    let out = probe(
        "print(json.dumps([\n\
           mod._judge_excluded({'work_author': 'codex', 'excluded_seats': ['claude#2', 'claude#3']}),\n\
           mod._judge_excluded({'work_author': 'codex', 'excluded_seats': ['claude']}),\n\
           mod._judge_excluded({'work_author': 'codex', 'excluded_seats': ['codex#2']}),\n\
           mod._judge_excluded({'work_author': 'codex', 'excluded_seats': []}),\n\
           mod._judge_excluded({'work_author': 'claude', 'excluded_seats': []}),\n\
           mod._judge_excluded({'work_author': 'claude#1'}),\n\
           mod._judge_excluded({'excluded_seats': []}),\n\
           mod._judge_excluded({}),\n\
           mod.JUDGE_CLI]))\n",
    );
    assert_eq!(
        out.trim(),
        r#"[true, true, false, false, true, true, true, true, "claude"]"#,
        "{out}"
    );
}

/// core#625: a claude-authored request is never judged by claude — the daemon refuses with a
/// named reason and NEVER invokes the model; a request whose author and monitors leave claude
/// eligible is judged and reports `judge_cli: "claude"`.
#[test]
fn the_bus_path_daemon_never_invokes_claude_on_a_claude_authored_request() {
    let out = probe(
        "calls = []\n\
         mod._evaluate = lambda c, w: (calls.append(1), (True, 'judged'))[1]\n\
         r = [mod._handle({'work_author': 'claude', 'excluded_seats': [], 'criterion': 'c', 'work': 'w'}),\n\
              mod._handle({'work_author': 'codex', 'excluded_seats': ['claude#2'], 'criterion': 'c', 'work': 'w'}),\n\
              mod._handle({'excluded_seats': [], 'criterion': 'c', 'work': 'w'}),\n\
              mod._handle({'work_author': 'codex', 'excluded_seats': [], 'criterion': 'c', 'work': 'w'})]\n\
         print(json.dumps({'calls': len(calls), 'verdicts': [[p, j] for p, _, j in r], 'why': r[0][1]}))\n",
    );
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect(&out);
    assert_eq!(
        v["calls"], 1,
        "claude ran only for the codex-authored request: {v}"
    );
    assert_eq!(
        v["verdicts"],
        serde_json::json!([
            [false, null],
            [false, null],
            [false, null],
            [true, "claude"]
        ]),
        "{v}"
    );
    assert!(
        v["why"].as_str().unwrap().contains("work author"),
        "the refusal names why: {v}"
    );
}

// ── Test-harness hygiene (core#311) — not a test ─────────────────────────────────────────────
/// Arm the hermetic emit spool BEFORE main (pre-main is single-threaded, so no test thread can
/// race it): engine paths under test fire coarse fire-and-forget `wicked.*` emissions, and with
/// no shared store configured those spool — which must land in a per-process temp file, never in
/// the operator's real `~/.something-wicked/wicked-apps/emit-outbox.ndjson` replay queue. Every
/// binary in this suite carries this block; `harness_hygiene.rs` fails the suite if one is missing.
///
/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only sets one process env var
/// via the std API — no allocator setup, no threads, no panics across the FFI boundary.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}
