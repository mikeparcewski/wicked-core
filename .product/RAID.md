---
name: RAID
title: wicked-core — Risks, Assumptions, Issues, Decisions
status: active
version: 0.1
date: 2026-07-21
author: michael.parcewski@accenture.com
---

# RAID — wicked-core

## Risks

| ID | Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|---|
| R-001 | Actor thread never terminates (CRITICAL finding from REASSESS-P0-P1): `self_tx` held on the actor's stack means the channel never closes and the actor blocks indefinitely, leaking a thread + writable store handle | High — structural | Critical | Fix: add `Command::Shutdown`; track live `Core` handles with `Arc`; send Shutdown on last drop; drain in-flight workers before exit. Tracked as ISS-001. |
| R-002 | `apply_step_result` has no idempotency/cursor guard: stale or duplicate `ApplyStepResult` re-applies a finished unit (store corruption) | Medium | High | Fix: early-return `Ok(())` if `output.unit_ix != session.unit_ix` or unit status is already Done. Tracked as ISS-002. |
| R-003 | Gate-hook opens SQLite READ-WRITE with no busy_timeout: concurrent actor write transaction causes immediate `SQLITE_BUSY` → spurious governance Deny | Medium (happens under load) | High | Fix: give the hook a genuinely read-only path (`SQLITE_OPEN_READ_ONLY`, skip DDL/migrate); add `busy_timeout` as fallback. Tracked as ISS-003. |
| R-004 | Deny-mid-run in the interactive engine: a rejected unit does not halt the run — `finalize_run` marks the session `Completed` unconditionally | Medium | Medium | Fix: distinguish run-level outcome (Completed-with-rejections vs Clean-completed); surface via `SessionStatus`. Tracked as ISS-004. |
| R-005 | `in_flight` HashSet is per-actor only: two live actors on the same db (crash-recovery race, leaked actor) can both dispatch the same unit | Low (requires leaked actor) | High | Mitigation: fix R-001 first (no leaked actors); add OS-level exclusive file lock; per-session execution lease in store. Tracked as ISS-005. |
| R-006 | Distribution runs synchronously on the actor thread: a blocking council-dispatch freezes the actor for the full vote duration, contradicting the "single-writer thread is no longer frozen" design claim | Medium | Medium | Fix: move distribute off-thread (dispatch a distribute worker step); or explicitly document that distribute is synchronous until P5. Tracked as ISS-006. |

## Assumptions

| ID | Assumption |
|---|---|
| A-001 | wicked-core is consumed only by wicked-crew (via napi bindings) and potentially wicked-estate MCP surfaces — no other callers |
| A-002 | SQLite single-writer constraint is accepted; multi-host / horizontally-scaled scenarios are out of scope |
| A-003 | The gate-hook subprocess runs sequentially with the actor (not concurrently at the same instant) in the typical case; R-003 is a race condition, not the steady state |
| A-004 | wicked-estate is a sibling repo checked out at a known tag — crates.io publication is blocked by this dependency |
| A-005 | napi-rs is the correct FFI boundary for TypeScript callers; no alternative IPC protocol is planned |

## Issues

| ID | Issue | Severity | Status |
|---|---|---|---|
| ISS-001 | Actor thread never terminates (`self_tx` held) — leaked thread + writable store handle per spawn | CRITICAL | **RESOLVED** — `ShutdownGuard` (sends `Command::Shutdown` on last `Core` drop) + `Command::Shutdown` break in actor loop. Test: `actor_shuts_down_when_last_core_drops` in `tests/p1_reentrant.rs`. |
| ISS-002 | `apply_step_result` no idempotency/cursor guard — stale result double-applies a finished unit | HIGH | **RESOLVED** — triple guard in `apply_step_result`: terminal-status check → cursor mismatch check → attempt guard (`output.attempt < session.attempt`). Stale path: `StepApplied::Stale`. |
| ISS-003 | Gate-hook opens SQLite READ-WRITE with no busy_timeout — spurious Deny under contention | HIGH | **RESOLVED** — `gate_hook.rs` now uses `open_store_ro` (P4b, wicked-core#36 + wicked-estate#63): hook opens with `SQLITE_OPEN_READONLY`, no WAL/DDL, actor is sole writer. |
| ISS-004 | Deny-mid-run leaves session Completed unconditionally | MEDIUM | **RESOLVED** — deny path drives `fail_run` (→ `SessionStatus::Failed`), never `finalize_run`. Proven by `sync_launch_halts_as_failed_on_a_governance_deny` in `tests/seam_findings.rs`. |
| ISS-005 | No cross-actor execution lease — two actors on same db can double-dispatch | MEDIUM | **Mitigated** — ISS-001 is resolved (no leaked actors), so the two-actor scenario requires explicit misuse. OS-level file lock or per-session lease remains a recommended hardening; deferred. |
| ISS-006 | Distribution runs on actor thread — design claim "actor not frozen" is false for distribute window | HIGH | **RESOLVED** — `distribute_units_on` called via `std::thread::spawn` in `ContinueLaunch`, `WorktreeReady`, and `ReassignUnit` handlers; actor thread is not blocked. |
| ISS-007 | P0 SQLITE_BUSY test is theater: never creates writer-writer contention; passes even without the fix | MEDIUM | Open — test needs strengthening (writer-writer contention fixture). Deferred. |
| ISS-008 | Resume test under-specifies from-cursor proof — correctness is incidental, not asserted | MEDIUM | Open — add `FastRunner` recording to assert only `[unit_ix]` dispatched on resume. Deferred. |
| ISS-009 | Dual-cursor drift between `workflow.current_index` and `session.unit_ix` on denial — latent, will materialize in P4a | MEDIUM | Open — pull cursor unification forward. Deferred to P4a. |
| ISS-010 | crates.io publication blocked by path dependency on `wicked-estate-store` and four `publish = false` vendored crates | LOW | Will resolve when estate is published |

## Decisions

| ID | Decision | Rationale |
|---|---|---|
| D-001 | Single-writer `StoreActor` over multi-writer SQLite | Eliminates in-process `SQLITE_BUSY`; the single-writer invariant is the product's core differentiator |
| D-002 | napi-rs N-API bindings for JS/TS callers (not JSON-RPC or HTTP) | Zero out-of-process hop for the primary caller (wicked-crew daemon); type safety at the boundary |
| D-003 | Gate-hook writes only `decisions.ndjson` (append-only); actor drains | Preserves single-writer invariant for the external hook subprocess |
| D-004 | WorkflowDef as JSON (`workflows/*.json`), loaded via `load_dir` | Capability-as-data: adding a workflow does not require editing core; drift-guarded with `deny_unknown_fields` |
| D-005 | CI requires checkout of `wicked-estate` as a sibling at a pinned tag | Path deps on estate crates resolve from source; crates.io fallback does not exist today |
| D-006 | Adversarial review findings (REASSESS-P0-P1.md) are tracked as open ISS-* items with explicit fix plans | Transparency: the product is mid-flight; the DoD reflects open gaps rather than papering over them |
