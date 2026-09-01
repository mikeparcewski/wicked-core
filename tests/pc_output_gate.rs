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
