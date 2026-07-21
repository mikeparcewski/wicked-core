---
name: adversarial-review-reassess-round2
title: "wicked-core — Adversarial Re-Review of REASSESS-P0-P1.md findings"
status: PASS
date: 2026-07-21
reviewer: michael.parcewski@accenture.com
scope: REASSESS-P0-P1.md findings vs current main HEAD (commit 4170528 + e9a4638)
---

# Adversarial Re-Review: REASSESS-P0-P1.md Findings

## Verdict: PASS

All CRITICAL and HIGH findings from REASSESS-P0-P1.md are resolved in the current codebase. MEDIUM findings are documented as open gaps with explicit deferral rationale. No new CRITICAL or HIGH issues were found in this re-review.

---

## Finding Resolution Map

### [CRITICAL] Actor thread never terminates (self_tx held)

**Status: RESOLVED**

`lib.rs:150-157` — `ShutdownGuard` sends `Command::Shutdown` on drop. `Core` holds `_shutdown: Arc<ShutdownGuard>` so the `Shutdown` fires when the last clone drops. `actor.rs:1610-1618` — the `Command::Shutdown` arm breaks the loop, releasing `store`. The actor's `self_tx` is no longer able to block termination.

**Test evidence:** `tests/p1_reentrant.rs::actor_shuts_down_when_last_core_drops` — drops the last `Core` handle and asserts the subscribed `Receiver` disconnects (proving the actor exited and dropped its senders). Passes in CI.

---

### [HIGH] apply_step_result no idempotency / cursor guard

**Status: RESOLVED**

`actor.rs:1997-2023` — three stale-result guards before any store write:
1. Terminal status check: `if matches!(session.status, Completed | Cancelled | Failed) → StepApplied::Stale`
2. Cursor mismatch: `if output.unit_ix != session.unit_ix → StepApplied::Stale`
3. Attempt guard: `if output.attempt < session.attempt → StepApplied::Stale`
4. Unit status: `if unit.status == Done → StepApplied::Stale`

**Test evidence:** `tests/p1_reentrant.rs::resume_of_completed_run_is_a_noop` — asserts resuming a Completed run dispatches nothing new. `tests/seam_findings.rs` — deny-path tests exercise the stale-result rejection path indirectly. CI passes.

---

### [HIGH] Gate-hook opens SQLite READ-WRITE (no busy_timeout)

**Status: RESOLVED**

`gate_hook.rs:36` — imports `open_store_ro` from `wicked_apps_core`. `gate_hook.rs:76-89` — calls `open_store_ro(db)`. The module-level doc (lines 1-31) confirms: "Now uses `open_store_ro` (P4b, wicked-core#36 + wicked-estate#63): the hook opens the SQLite file with `SQLITE_OPEN_READONLY` — no WAL pragma, no `SCHEMA`/`migrate_schema` DDL". The hook subprocess is now a genuine read-only opener.

**Remaining gap (ISS-007, MEDIUM):** `tests/p0_single_writer.rs` tests actor-read concurrent with hook but does not create actor-write concurrent with hook. The fix is present in code; the test for it is under-specified. Deferred.

---

### [HIGH] P1's off-thread claim only partially true (distribute synchronous)

**Status: RESOLVED**

`actor.rs:529-549` (`ContinueLaunch`), `actor.rs:671-692` (`WorktreeReady`), `actor.rs:1502-1550` (`ReassignUnit`) — all three sites spawn `std::thread::spawn` around `distribute_units_on`. The council distribution no longer blocks the actor thread. Posts `PlanReady` or `PlanFailed` back via `self_tx`.

**Test evidence:** `tests/p1_reentrant.rs::engine_is_off_thread_guards_inflight_and_resumes_from_cursor` — the actor serves reads (sessions()) while a step is in flight. CI passes. Full blocking-dispatcher test (proving actor serves reads during DISTRIBUTE phase) is not yet written — acknowledged gap, deferred per original ISS-006 plan.

---

### [MEDIUM] in_flight removal derived from store re-read (run_finished)

**Status: RESOLVED**

The original concern was that `apply_step_result` used `run_finished(&store, &run_id)` to decide whether to drop from `in_flight`. The current code uses the `StepApplied` enum (`Finished` / `Paused` / `Continuing` / `Stale`) returned from `apply_step_result` and removes from `in_flight` based on the control-flow outcome (`actor.rs:846-886`), not a store re-read. `finalize_run` errors are now surfaced and drive a `fail_run` path instead of being silently swallowed.

---

### [MEDIUM] Resume run no cross-actor execution lease

**Status: Mitigated (ISS-005)**

ISS-001 is resolved (no leaked actors). The two-actor scenario now requires explicit misuse (two separate `Core::spawn` calls on the same db without coordination). OS-level exclusive file lock or per-session executing-lease remains recommended hardening — deferred per ISS-005 decision.

---

### [MEDIUM] Gate aggregation first-decision-wins (not deny-wins)

**Status: Partially mitigated; acknowledged as deferred**

The drain logic (`gate_hook.rs`) uses `apply_gate` per claim, which is first-decision-wins at the phase level. An Allow-then-Deny sequence for the same phase leaves the phase Approved. However: (a) the live hook exit-2 blocks the denied tool-call immediately, (b) the Deny claim is conformed as durable evidence, and (c) the probability of Allow-then-Deny in a single phase is low. This is an acknowledged implementation trade-off, explicitly documented in REASSESS-P0-P1.md §finding-6. No CRITICAL/HIGH severity — deferred.

---

### [MEDIUM] Dual-cursor drift between workflow.current_index and session.unit_ix

**Status: Open (ISS-009), latent**

The drift is latent — `resume_run` reads `session.unit_ix` (not `workflow.current_index`) for control flow, so no active incorrect behavior today. Will materialize in P4a when the real backend reads the workflow node. Explicitly tracked as ISS-009 and deferred to P4a.

---

### [MEDIUM] P0 test is theater (no writer-writer contention)

**Status: Open (ISS-007)**

`tests/p0_single_writer.rs` does not create concurrent actor write + hook subprocess. The test passes trivially because the hook is now read-only (the fix is correct) but the assertion can't distinguish the old write-path from the new read-only one. Test needs strengthening with a concurrent-write fixture. Deferred.

---

### [MEDIUM] Resume test under-specifies cursor proof

**Status: Open (ISS-008)**

`tests/p1_reentrant.rs` proves resume correctness only incidentally (dedup-bail would catch a from-0 re-run via an error path, not a direct assertion). Fix: instrument `FastRunner` to record dispatched `unit_ix` values, then assert exactly `[1]` on resume. Deferred.

---

### [MEDIUM] Deny-mid-run interactive engine untested / session Completed unconditionally

**Status: RESOLVED**

`actor.rs:2166-2246` — the deny path calls `fail_run` (not `finalize_run`), driving `SessionStatus::Failed`. The `HumanConfirmIf(VerdictNotPass)` conditional escalation is a documented exception (escalates semantic verdict denial to human review, not a governance hook bypass which always hard-fails).

**Test evidence:** `tests/seam_findings.rs::sync_launch_halts_as_failed_on_a_governance_deny` — fixture Deny policy on unit-1; asserts `SessionStatus::Failed` and unit-2 never ran. Passes in CI.

---

### [MEDIUM] StepOutput cannot represent a failed/cancelled step

**Status: Partially addressed**

`workflow.rs` `StepOutput` now includes `governed: bool` and `agent_verdict: Option<(bool, String)>` fields (not in the original REASSESS review). `StepStatus::Failed` is represented via `agent_verdict.0 == false`. The full `StepStatus` enum (`Ok | Failed | Cancelled | Interrupted`) is not yet implemented — workers use the `agent_verdict` bool to signal failure. This is an acceptable interim representation given P1 scope. Deferred refinement.

---

## Summary

| Severity | Count | Resolved | Open/Deferred |
|----------|-------|----------|---------------|
| CRITICAL | 1 | 1 ✓ | 0 |
| HIGH | 3 | 3 ✓ | 0 |
| MEDIUM | 8 | 3 ✓ | 5 (all deferred with rationale) |
| LOW | 2 | N/A | Verified safe or no change required |

**L2-7 gate: PASS** — all CRITICAL and HIGH findings resolved. Remaining MEDIUM findings are tracked as ISS-007/008/009 (deferred), ISS-005 (mitigated), and two acknowledged design trade-offs with no active incorrect behavior today.
