# Changelog

All notable changes to `wicked-core`. Versions follow [SemVer](https://semver.org/).

## Unreleased

## 0.2.0 — 2026-07-21

### Added

- **P0→P3 orchestration pipeline (P4a partially complete)** — `WorkflowDef` JSON-driven execution: plan → distribute → govern → resume. Single-writer store actor with `Command`/`CoreEvent` API; no SQLite races from competing readers. (ISS-009 dual-cursor drift deferred to P4a; see Known open items.)
- **napi-rs TypeScript bindings** (`crates/wicked-core-ts`) — `launchRun`, `subscribe`, `confirmGate`, `sessions`, `sessionsDetail`, `workOutput`, `registryRoster`, `registerWorkflow`, `listPolicies`, `listConformanceRules`, `listClaims`, `upsertPolicy`, `getCoverageReport`, PTY terminal methods. Ships as platform-native `.node` binaries for macOS x64/arm64, Linux x64/arm64, Windows x64 via `napi-release.yml`.
- **Multi-platform CI** — `ci.yml` `check` job extended to 3-OS matrix (`ubuntu-latest`, `macos-latest`, `windows-latest`). Unix-gated tests (`#[cfg(unix)]`) skip cleanly on Windows.
- **wicked-apps-core Postgres backend** — store seam `&mut dyn GraphStore` + concrete `AnyStore` owner + `open_store_any`/`--features postgres`. Postgres round-trip tested in CI (`postgres-parity` job).
- **Output-governance observability** — full EVT-001..016 event wave: `WorkflowSelected`, `WorkerSessionStarted/Reused/Closed`, `AcpSessionStarted/Fallback`, `UnitContextInjected`, `UnitOutputCaptured`, `UnitReworkAmended`, `StepFailed`, `CrashRecoveryRedrive`, and governance-deep events (EVT-008..011, EVT-016).
- **Campaign scheduler** (`DES-CAMPAIGN-001`) — DAG-based multi-session orchestration with crash-resume for stranded campaign nodes.
- **PTY terminal sessions** (`DES-TERMINAL-001`) — interactive PTY capability with backpressure hardening; exposed via napi binding.
- **Workflow drop-in JSONs** — pre-built `chat` and `onboarding` sub-workflow definitions loadable via `registerWorkflow`.
- **Blind capability routing** — council voters never see CLI names; `AgenticCli` opaque to the router.
- **Worker message injection + unit reassignment** — `core#92` worker API: inject a message mid-run or reassign a unit to a different worker.
- **Gate-hook exe resolution** — correct path resolution when loaded as a napi-rs addon (`#95`).
- **Campaign crash-resume hardening** — running campaign nodes no longer stranded on resume (`374accc`).

### Fixed

- **ISS-001 Actor lifecycle** — actor thread now terminates when all `Core` handles are dropped: `ShutdownGuard` + `Command::Shutdown` + drain in-flight workers before exit. Test: `actor_shuts_down_when_last_core_drops`.
- **ISS-002 Idempotency** — duplicate `StepOutput` for an already-applied unit is discarded with no store change: four guards in `apply_step_result` (terminal status + cursor + attempt); stale result returns `StepApplied::Stale`.
- **ISS-003 Gate-hook read-only** — hook subprocess uses `open_store_ro` (`SQLITE_OPEN_READONLY`); no WAL/DDL; no `SQLITE_BUSY` from hook path.
- **ISS-004 Governance deny-mid-run** — a denied unit produces terminal `SessionStatus::Failed` (not `Completed`); subsequent units do not run.
- **ISS-008 Crash+resume cursor** — `resume_run` re-dispatches from `session.unit_ix` only; `FastRunner` fixture asserts `*ran == vec![1]` (not a full re-run from 0).
- Council distribution moved off actor thread (ISS-006): council vote no longer freezes the single-writer actor for the full vote duration.
- Git worktree creation moved off actor thread.
- `finalize_run` correctly propagates governance outcome for the interactive engine path.
- PTY terminal teardown hardened against backpressure races.
- `ThreadsafeFunction` lifecycle bugs in the napi binding repaired.
- `cross_language_roundtrip` test correctly marked `#[ignore]` (requires node + sibling wicked-bus; run with `--ignored`).

### Known open items (deferred)

- **ISS-007** (MEDIUM) — P0 SQLITE_BUSY test does not create real writer-writer contention; deferred.
- **ISS-009** (MEDIUM) — Dual-cursor drift between `workflow.current_index` and `session.unit_ix` on denial; deferred to P4a.
- **ISS-010** — crates.io publication blocked by path dependency on `wicked-estate-store` and four `publish = false` vendored crates; resolves when estate publishes.
