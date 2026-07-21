---
name: REQ-001-application-overview
title: "wicked-core — Application Overview"
status: draft
version: 0.1
date: 2026-07-21
author: michael.parcewski@accenture.com
review-required: true
---

# REQ-001 — Application Overview

## What wicked-core is

wicked-core is the **in-process composition runtime** for the wicked-* ecosystem. It provides three
things:

1. **Single-writer `StoreActor`** — one thread owns the SQLite write lock, eliminating in-process
   `SQLITE_BUSY` races. All write consumers hold a clonable `Core` handle that issues typed
   `Command`s with oneshot replies.
2. **Live event stream** — `Core::subscribe()` fans out `CoreEvent`s to any number of subscribers.
   No polling.
3. **Workflow execution engine** — `WorkflowDef` JSON + data-driven planning → skills-driven CLI
   invocation → governed gates + HITL pause/resume.

It also exposes **napi-rs N-API bindings** (`wicked-core-ts`) so TypeScript callers — specifically
the wicked-crew daemon and its bundled wicked-studio HITL UI — can drive runs and consume the event
stream via the same typed API without an out-of-process hop.

wicked-core is **internal**. Its consumers are:
- **wicked-crew** — the agentic-workflow governor that drives runs, gates, and worker dispatch
  through the napi bindings
- **wicked-estate** (future) — MCP server; wicked-core is the planned composition engine for
  estate's concurrent service surfaces

It is not end-user-facing and is not published to crates.io today (path dependency on the
unpublished `wicked-estate-store` 0.13, plus four vendored engine crates marked `publish = false`).

---

## Core user flows

### Flow 1 — Start a governed run
1. The wicked-crew daemon calls `core.launch_run(RunSpec)` via the napi bindings.
2. The `StoreActor` receives `Command::LaunchRun`, plans units from the `WorkflowDef`, distributes
   (council dispatch for multi-CLI phases), and dispatches the first unit to a worker thread.
3. The actor emits `CoreEvent::SessionStarted` and subsequent progress events to all subscribers.
4. The wicked-studio UI receives events via `Core::subscribe()` over the napi bridge and updates
   the session view in real time.

### Flow 2 — Governance gate evaluation
1. Per unit, the actor opens a governance phase, runs `select(select_key)` + `decide(context)` via
   the embedded governance engine (rules from `.product/evidence/` policies).
2. The actor applies the gate claim via `apply_gate`; a deny is sticky and emits
   `CoreEvent::GateDenied`, halting further dispatch for that unit.
3. The gate-hook subprocess (invoked by the CLI tool during a wrapped run) writes claims to an
   append-only `decisions.ndjson`; the actor drains and applies them via `Command::ApplyHookDecisions`.
   The hook never opens the SQLite file directly.

### Flow 3 — Crash recovery + resume
1. The wicked-crew daemon detects a missing session and calls `core.resume_run(run_id)`.
2. The actor reads the persisted cursor (`session.unit_ix`, `exec_phase`) from the estate store and
   re-dispatches from the last safe point.
3. Idempotency is enforced by attempt-scoped event IDs — duplicate `StepResult` messages for an
   already-applied unit are detected and discarded.

### Flow 4 — HITL gate (human-in-the-loop)
1. The actor sets `run.status = AwaitingHuman` and emits `CoreEvent::AwaitingHuman { run, stage_ix, prompt }`.
2. The wicked-crew daemon surfaces the prompt to the operator (terminal CLI or wicked-studio panel).
3. The operator calls `Core::confirm_gate(run_id, decision)` which applies the human decision and
   re-enters the run from the cursor.

---

## Success criteria

| ID | Criterion | Verification method |
|---|---|---|
| SC-001 | No `SQLITE_BUSY` errors under concurrent actor writes + concurrent reads (gate-hook, MCP queries, subscribers) | Integration test: concurrent reads while actor drives writes; `cargo test` exits 0 with no BUSY errors |
| SC-002 | Governance gate is deny-dominant: a Deny from any source blocks phase approval; no self-grading possible | Integration test: fixture verdict `Deny` → assert gate blocked; evaluator-seat ≠ creator-seat enforced at dispatch |
| SC-003 | napi bindings (`wicked-core-ts`): `launchRun`/`subscribe`/`confirmGate` callable from TypeScript with correct round-trip behavior | Integration test `tests/bus_bridge.rs` (cross-language round-trip) + `napi-release.yml` CI gate |
| SC-004 | Crash + resume: `resume_run` re-dispatches from `session.unit_ix`; units before the cursor are never re-applied | Integration test `tests/p1_reentrant.rs` crash-resume path; `cargo test` exits 0 |
| SC-005 | CI gate: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo build`, `cargo test` all pass on ubuntu-latest | `ci.yml` CI pipeline; green on every PR merge |
