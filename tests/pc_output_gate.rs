//! PR-C proving test — the per-OUTPUT gate-hook subprocess fails CLOSED.
//!
//! Governance must never silently allow an output it could not record. The most load-bearing
//! fail-closed condition is an unset `WICKED_DECISIONS_PATH` (the launcher forgot to wire the
//! decisions log) — the real `wicked-core output-gate-hook` subprocess must exit 2 (deny) then,
//! exactly like the input `gate-hook`. This exercises the actual binary, not the library fn.

use std::io::Write;
use std::process::{Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_wicked-core");

#[test]
fn output_gate_hook_fails_closed_when_decisions_path_unset() {
    let mut child = Command::new(BIN)
        .args(["output-gate-hook", "--scope", "s", "--phase", "review"])
        // Ensure the decisions path is UNSET for the child — the fail-closed trigger.
        .env_remove("WICKED_DECISIONS_PATH")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn output-gate-hook subprocess");
    // Feed it some produced output; it must still deny because it cannot RECORD the decision.
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"some produced output text")
        .unwrap();
    let status = child.wait().expect("wait for output-gate-hook");
    assert_eq!(
        status.code(),
        Some(2),
        "an unset decisions path must fail CLOSED (exit 2 = deny), never silently allow"
    );
}
