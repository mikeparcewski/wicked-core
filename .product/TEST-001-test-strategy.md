---
name: TEST-001-test-strategy
title: "wicked-core — Test Strategy"
status: draft
version: 0.1
date: 2026-07-21
author: michael.parcewski@accenture.com
review-required: true
---

# TEST-001 — Test Strategy

## Overview

wicked-core's test strategy has three layers: integration tests in `tests/`, a CI gate, and a
planned adversarial review gate. A fourth layer (cross-language round-trip via napi bindings) is
covered within the existing integration suite.

---

## Layer 1 — Integration tests

**Location:** `tests/`

**Runner:** `cargo test` (Rust standard test harness)

**How to run:**

```bash
# Requires wicked-estate checked out as a sibling at the version pinned in ci.yml
cargo test
```

**CI requirement:** `wicked-estate` must be a sibling of `wicked-core` at the pinned `WICKED_ESTATE_REF` tag; the crates.io fallback does not exist. Set `WICKED_MEMORY_EMBEDDER=hash` to prevent model downloads in CI.

### What the tests cover

| File | What is tested |
|---|---|
| `p0_single_writer.rs` | Single-writer actor: no SQLITE_BUSY under concurrent reads while actor writes; gate-hook idempotency (hook writes ndjson, not store); governance veto on Deny claim |
| `p1_reentrant.rs` | Off-thread dispatch: step runner executes off actor thread; crash + resume from cursor; unit sequencing |
| `p2_contract.rs` | P2 API contract: typed session/workflow/phase round-trips |
| `p2_gates.rs` | Governance gate: council dispatch to ≥2 workers; gate policy application |
| `p2_worker_lifecycle.rs` | Worker lifecycle: spawn, complete, cleanup |
| `governance_in_run.rs` | Governance embedded in a run: coverage rules, conformance claims, deny-halts-run |
| `events_governance_deep.rs` | Deep governance event stream: EVT-008/009/010/011/016; deny propagation |
| `bus_bridge.rs` | Cross-language napi round-trip: `run.requested` → launch → `run.launched`; verifies wicked-bus bridge wiring |
| `exec_seam.rs` | Execution seam: CLI runner dispatches off actor thread via wicked-bus opt-in path |
| `events_foundation.rs` | Foundation event stream: session lifecycle events |
| `coverage.rs` / `coverage_cli.rs` / `coverage_schema.rs` | Coverage emitter: store-recomputed report, schema validation, CLI surface |
| `domain_extraction_e2e.rs` | Domain extraction end-to-end |
| `p10_methodology.rs` through `p14_gate_phase.rs` | Methodology pipeline stages (recon, adversarial review, functional test, gate phases) |
| `operator_api.rs` | Operator API (provision-validator, approve-validator) |
| `terminal.rs` | PTY terminal session runner |
| `seam_findings.rs` | Seam-level findings: governance output from real runs |
| `skills_live.rs` | Live skill invocation (requires Claude CLI; skipped in CI if unavailable) |
| `zz_mem.rs` | Memory subsystem: store-backed memory operations |

### What the tests do not cover

- **Concurrent actor-lifecycle correctness** — ISS-001 (actor never terminates) and ISS-002 (no idempotency guard in `apply_step_result`) are not exercised by any current test. The `p1_reentrant.rs` resume test proves correctness only incidentally (via a dedup-bail path), not explicitly.
- **Gate-hook under contention** — ISS-003 (SQLITE_BUSY from concurrent hook + actor write) is not reproduced; the p0 test only has a reader + hook, never two concurrent writers.
- **Two-actor scenarios** — ISS-005 (cross-actor double-dispatch) has no test.
- **Off-thread distribution** — ISS-006 (distribution runs on actor thread): the p1 test uses an instant stub dispatcher so the freeze window is invisible.

---

## Layer 2 — CI gate

**Location:** `.github/workflows/ci.yml`

**Trigger:** Pull requests + pushes to `main`

**Steps:** `cargo fmt --all --check` → `cargo clippy --workspace --all-targets -- -D warnings` → `cargo build --workspace` → `cargo test --workspace` → `cargo build --features postgres` (backend compile-parity); plus a separate `postgres-parity` job that provisions a real Postgres and runs the store round-trip

**Platform:** `ubuntu-latest` (single platform; no macOS/Windows matrix today)

**Note:** CI requires `wicked-estate` checked out as a sibling at `WICKED_ESTATE_REF`. The workflow handles this automatically via a checkout step.

**napi release:** `.github/workflows/napi-release.yml` builds and publishes the Node/TypeScript
bindings (`wicked-core-ts`) on version tags. This is a separate gate from the Rust CI.

---

## Layer 3 — Adversarial review gate

**Purpose:** Validate that the implementation matches the design spec (ORCHESTRATOR.md + DES-EXEC-001 + DES-OUTGOV-*); surface correctness bugs that integration tests miss.

**Status:** One completed adversarial review produced `REASSESS-P0-P1.md` with CRITICAL/HIGH findings (ISS-001 through ISS-006). These must be resolved before the DoD adversarial-review gate can PASS.

**How to run:** `wicked-garden:crew:reviewer` against the diff of each feature branch.

---

## Test coverage gaps (known)

| Gap | Risk | Linked issue |
|---|---|---|
| No test for actor shutdown / thread lifecycle | CRITICAL — leaked actor is not detected | ISS-001 |
| No idempotency test for `apply_step_result` with a duplicate or stale `StepOutput` | HIGH — double-apply goes undetected | ISS-002 |
| No contention test reproducing SQLITE_BUSY in the gate-hook path | HIGH — spurious Deny under load | ISS-003 |
| `p1_reentrant.rs` resume test does not assert from-cursor dispatch explicitly | MEDIUM — correctness is incidental | ISS-008 |
| No Deny-mid-run test asserting run-level terminal status | MEDIUM — behavior is untested | ISS-004 |
| No test for distribution blocking the actor thread | MEDIUM — P1 off-thread claim is unproven | ISS-006 |
| `skills_live.rs` skipped without Claude CLI in CI | LOW — live skill path not CI-gated | — |
