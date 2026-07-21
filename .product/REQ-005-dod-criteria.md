---
name: REQ-005-dod-criteria
title: wicked-core — Definition of Done Criteria
status: partially-verified
version: 0.2
date: 2026-07-21
author: michael.parcewski@accenture.com
review-required: true
---

# REQ-005 — Definition of Done Criteria

## Overview

wicked-core DoD is gated on three levels. Level 1 is the minimum bar for any merge to main. Level 2
is required before wicked-crew depends on a release commit. Level 3 is required before crates.io
publication (currently blocked by path dependencies).

---

## Level 1 — Build and Correctness Gate

Required on every PR and merge to main.

| # | Criterion | How Verified | Verified |
|---|---|---|---|
| L1-1 | `cargo fmt --all --check` exits 0 | CI `ci.yml` `cargo fmt --all --check` step | — |
| L1-2 | `cargo clippy --workspace --all-targets -- -D warnings` exits 0 | CI `ci.yml` `cargo clippy` step | — |
| L1-3 | `cargo build --workspace` exits 0 | CI `ci.yml` `cargo build --workspace` step | — |
| L1-4 | `cargo test --workspace` exits 0 on ubuntu-latest | CI `ci.yml` `cargo test --workspace` step | — |
| L1-5 | `cargo build --features postgres` exits 0 (backend compile-parity) | CI `ci.yml` backend compile step | — |
| L1-6 | napi bindings build: `napi-release.yml` runs `npx napi build --platform --release` for 5 targets (macOS x64/arm64, Linux x64/arm64, Windows x64) | CI `napi-release.yml` build matrix (triggered on version tags) | — |

**Current status:** L1-1 through L1-5 pass in CI on every merged PR (`check` job in `ci.yml`). L1-6 passes on version tags via `napi-release.yml`. All L1 criteria are effectively ✓ as of main HEAD, but open CRITICAL bugs (ISS-001, ISS-002, ISS-003) mean correctness is not fully established even when tests pass.

---

## Level 2 — Correctness and Integration Gate

Required before wicked-crew can depend on a wicked-core release commit.

| # | Criterion | How Verified | Verified |
|---|---|---|---|
| L2-1 | Actor lifecycle: actor thread terminates when all `Core` handles are dropped | Integration test: drop all handles, assert actor thread joins within timeout | ✓ — `tests/p1_reentrant.rs::actor_shuts_down_when_last_core_drops` passes in CI. ISS-001 resolved via `ShutdownGuard` + `Command::Shutdown`. |
| L2-2 | `apply_step_result` is idempotent: a duplicate `StepOutput` for an already-applied unit is discarded with no store change | Integration test: send same `StepOutput` twice, assert unit state unchanged | ✓ — ISS-002 resolved: triple guard (terminal status + cursor + attempt) in `apply_step_result`; stale result returns `StepApplied::Stale`. CI passes. |
| L2-3 | Gate-hook path is genuinely read-only: hook subprocess does not acquire SQLite write lock | Integration test: concurrent actor write + hook invocation; assert no SQLITE_BUSY and actor write succeeds | ✓ (structural) — ISS-003 resolved: `gate_hook.rs` uses `open_store_ro` (`SQLITE_OPEN_READONLY`, no WAL/DDL). ISS-007 notes the existing P0 test does not create writer-writer contention; full contention test deferred. |
| L2-4 | Cross-language round-trip: `Core::launch_run` / `Core::subscribe` / `Core::confirm_gate` callable from TypeScript with correct event delivery | `tests/bus_bridge.rs` cross-language round-trip test exits 0 | ✓ — `tests/bus_bridge.rs` passes in CI |
| L2-5 | Crash + resume: `resume_run` re-dispatches from `session.unit_ix`; cursor is explicitly asserted (not inferred from dedup-bail) | Integration test with `FastRunner` recording dispatched unit indices; assert only `[unit_ix]` was dispatched on resume | ✓ — ISS-008 resolved: `tests/p1_reentrant.rs::engine_is_off_thread_guards_inflight_and_resumes_from_cursor` uses `FastRunner` (records dispatched `unit_ix` in a vec) and asserts `*ran == vec![1]` — only the remaining unit was dispatched on resume, not a full re-run from 0. |
| L2-6 | Governance deny-mid-run: a denied unit produces a terminal `Failed` session status, not `Completed` | Integration test: fixture Deny policy + run; assert run-level status is `Failed` | ✓ — ISS-004 resolved: `seam_findings.rs::sync_launch_halts_as_failed_on_a_governance_deny` asserts `SessionStatus::Failed` and unit 2 never ran. Passes in CI. |
| L2-7 | Adversarial review PASS: all CRITICAL and HIGH findings from `REASSESS-P0-P1.md` resolved | Adversarial review gate — PASS verdict recorded in `.product/reviews/adversarial-review-reassess-round2.md` | ✓ — All CRITICAL (1) and HIGH (3) findings resolved. 5 MEDIUM findings deferred with rationale (ISS-005/007/008/009, gate-aggregation trade-off). |

---

## Level 3 — Release Gate

Required for crates.io publication and a stable semver version tag.

| # | Criterion | How Verified | Verified |
|---|---|---|---|
| L3-1 | Path dependencies resolved: `wicked-estate-store` published to crates.io; vendored `publish = false` crates removed or published | `cargo publish --dry-run` exits 0 | — (blocked on estate publication) |
| L3-2 | Multi-platform CI: `cargo test` passes on ubuntu-latest + macos-latest + windows-latest | CI matrix extended to include macOS/Windows | — (currently ubuntu-only) |
| L3-3 | Semver: `Cargo.toml` version ≥ 0.2.0; all open ISS-* items resolved or explicitly deferred with documented rationale | Manual gate + CHANGELOG.md entry | — |
| L3-4 | `CHANGELOG.md` entry exists for the release version | File inspection | — |
| L3-5 | wicked-testing acceptance pipeline: PASS verdict against a real governed wicked-crew run driven by this version of wicked-core | `.wicked-testing/evidence/` with `verdict: PASS` | — |

---

## Build Phase DoD (current)

The build phase is **in progress**. The table below tracks what has been verified.

| Item | Status |
|---|---|
| `cargo test` exits 0 in CI on every merge to main | ✓ |
| All DES-EXEC-001 / DES-OUTGOV-* / DES-COUNCIL-SKILL-001 design docs written | ✓ |
| `WorkflowDef` JSON data-driven execution (P0/P1/P1.5/P2/output-governance) built and tested | ✓ |
| napi bindings ship: `launchRun`/`subscribe`/`confirmGate` (TypeScript surface) | ✓ |
| Adversarial review: `REASSESS-P0-P1.md` produced; CRITICAL/HIGH findings identified | ✓ (review done) |
| CRITICAL findings resolved (ISS-001: actor lifecycle) | ✓ — `ShutdownGuard` + `Command::Shutdown`; test `actor_shuts_down_when_last_core_drops` passes |
| HIGH findings resolved (ISS-002: idempotency, ISS-003: gate-hook, ISS-006: distribute off-thread) | ✓ — all three resolved in code+tests; see RAID.md issue entries for evidence references |
| L2-1 through L2-7 integration tests all pass | ✓ L2-1,2,3(structural),4,5,6,7 verified; L2-5 cursor assertion in `engine_is_off_thread_guards_inflight_and_resumes_from_cursor` |

---

## Non-Negotiable DoD Items

These cannot be waived or deferred:

- ISS-001 (actor thread lifecycle): **RESOLVED** — `ShutdownGuard` + `Command::Shutdown` + `actor_shuts_down_when_last_core_drops` test.
- ISS-002 (idempotency): **RESOLVED** — four guards in `apply_step_result` (`actor.rs:1997-2023`); stale results return `StepApplied::Stale` without a store write.
- ISS-003 (gate-hook read-only path): **RESOLVED** — `open_store_ro` (`SQLITE_OPEN_READONLY`); no WAL/DDL from hook subprocess. ISS-007 (test quality) remains open.
- Adversarial review PASS verdict: `.product/reviews/adversarial-review-reassess-round2.md` records PASS — all CRITICAL and HIGH findings from REASSESS-P0-P1.md resolved. L2 gate is formally clear. Outstanding deferred items: ISS-007/008/009 (MEDIUM), gate-aggregation trade-off (MEDIUM), StepOutput failure representation (MEDIUM). These do not block L2.

---

## Revision History

| Version | Date | Author | Change |
|---------|------|--------|--------|
| 0.1 | 2026-07-21 | michael.parcewski@accenture.com | Initial draft — all L2/L3 items unchecked; L1 CI passing; open CRITICAL/HIGH bugs tracked as ISS-001 through ISS-009 |
| 0.2 | 2026-07-21 | michael.parcewski@accenture.com | Evidence pass: ISS-001/002/003/004/006/008 verified resolved in code+tests (CI green). L2-1,2,3(structural),4,5,6,7 verified. L2-5: FastRunner cursor assertion in `engine_is_off_thread_guards_inflight_and_resumes_from_cursor`. Adversarial re-review PASS recorded in `.product/reviews/adversarial-review-reassess-round2.md`. All CRIT/HIGH cleared. Remaining open: ISS-007/009 (MEDIUM, deferred). |
