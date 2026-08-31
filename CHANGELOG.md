# Changelog

All notable changes to `wicked-core`. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versions follow [SemVer](https://semver.org/).

Two release tracks share this file, newest entry first regardless of track:

- **engine** — the root Rust crate (git tags `vX.Y.Z`; not on crates.io — see ISS-010 below).
  Headings: `[X.Y.Z]`.
- **core-ts** — the npm binding [`wicked-core-ts`](https://www.npmjs.com/package/wicked-core-ts)
  (git tags `core-ts-vX.Y.Z`), which bundles the engine at its release commit. Headings:
  `[core-ts X.Y.Z]`. An npm release therefore ships engine changes even when the engine
  version number does not move.

## [Unreleased]

### Added
- **Rejected units keep their transcripts + machine-readable deny** (usability review #1,
  core-ts 0.7.6). The `work_output` record is now written for EVERY gated unit: a denied/failed
  unit keeps whatever PARTIAL output existed at rejection, flagged `resolution: "rejected"`
  beside the structured denial; a unit denied BEFORE any output existed persists an explicit
  failure record (no output, denial only) — so the transcript read returns honest structure
  instead of nothing exactly when an operator is diagnosing a failed run. ADR-0003 unchanged:
  `get_work_output` (evaluator artifact-passing, context injection) filters rejected records and
  still returns approved output only. The actor's own rejection paths (worker failure, substance
  gate, deliverable floor, elicitation failure) persist the same flagged record with the unit's
  FULL partial output. NEW `UnitDenial` — the machine-readable twin of the `denial_reason` prose:
  `{source, reason, claim_id, rule_ids, denied_tool, phase}` — rides the persisted `WorkUnit`
  (additive `denial` field), `UnitOutcome`, `gateEvaluated` (camelCase `denial`, beside the
  retained `denialReason`), and `fold_input_denial`'s return (claim id + firing policy ids + the
  denied tool recovered from the decisions log's tool-call annotation). NEW read path
  `get_unit_transcript` / `Core::unit_transcript` / napi `unitTranscript(unitId)` →
  `{unit_id, resolution, partial, phase_status?, output?, denial_reason?, denial?}`. The existing
  `workOutput` binding keeps its `string | null` shape — a rejected unit now answers with its
  partial output (`null` only when none was ever stored), which released crew 0.7.3 serves and
  studio 0.4.3 renders unchanged.
- **STEERING unification — one steering-rule model** (STEERING program, gov-model lane). The
  wiki/rules model and the standalone governance `Policy` model MERGE into `ConformanceRule`:
  new optional/defaulted fields `steering_type` (enum-as-string over
  `architecture|development|security|testing|operations|compliance|design-ux`, default
  `architecture` — INV-S1), `applies_to` (inclusion, the exact `Policy.applies_to` SELECT
  semantics), `excludes` (the NEW exclusion twin — exclusion dominates), `weight` (finite ≥ 0,
  default 1.0 — recall orders severity → weight desc → id; stored gate-priority signal — INV-S2),
  and the merged enforcement half `effect`/`trigger`/`obligations`/`criteria` (a rule WITHOUT
  `effect` is recall-only exactly as before; `effect` + blank `applies_to` or a bad trigger regex
  is refused — INV-S3). Every field is skipped at its default, so pre-steering rows parse
  unchanged (the additive migration happens on read) and a rule that uses none of them keeps the
  2.x wire shape byte-for-byte. INV-C1 is now scoped to the reserved `PAT-`/`POL-` namespace;
  other ids (migrated policies keep theirs verbatim; UI/chat-authored rules mint their own) need
  only be non-blank. `register_policy` became a thin shim (dual-writes the effect-bearing
  steering rule + the legacy `Other(POLICY)` audit node; refuses an id collision with a
  recall-only rule); `retire_policy` retires both rows; `select_any`/`decide` read the UNIFIED
  store (legacy-only rows union in at read time so an un-migrated store never fails open), and a
  golden test proves a migrated policy's decisions are BYTE-equal to its old row's.
  `migrate_policies_to_steering` is the one-time idempotent migration (`rules ingest` runs it;
  kind→steering_type mapping documented in `steering.rs`: the seven types map to themselves,
  `guardrail`/`gate` and every other legacy kind → `operations`; ids unchanged, `retired`
  honored, legacy nodes retained for decision-audit resolvability). `RuleQuery` gains the
  `steering_type` facet (recall + estate `rules.recall` wire-compatible); NEW `list_rules` +
  `wicked-core rules list [--type <t>] [--include-retired]` is the management/audit listing
  (decide-lane rows always shown; retired rows listable — closes the recall-skips-retired
  listing gap); `rules recall` gains `--type`; the scoreboard gains a per-steering-type
  population breakdown (`by_type`); MarkdownAdapter frontmatter gains optional
  `steering_type`/`excludes`/`weight` keys and `applies_to` now rides onto minted rules;
  UI/chat provenance sources are first-class. `conformance-rules.schema.json` bumped additively
  to contract 1.1.0 (bundle 1.2.0): new optional properties, id pattern relaxed outside the
  reserved namespace, `metadata.schema_version` widened to `enum [1.0.0, 1.1.0]`.
- **Fan-out contract across the deliberate store split** (AW-5 / arch-R3, decision record
  `.product/DES-OUTGOV-008-fanout-placement.md`). `wicked-core rules fanout <dir>` fans ONE ruleset
  (the `rules ingest` layout) out to the three lanes a governed run reads — (a) the enforcement
  store the gate hook selects/recalls from, (b) every discovery graph the workers' estate MCP binds
  (native `NodeKind::Rule` copies; deny-path policies do NOT replicate here), (c) one knowledge
  rationale chunk per rule (id-keyed `rule-rationale/<ID>` upsert, `source` = the rule's
  `provenance.ref`, the PAT-/POL- id embedded in the chunk text) — and smoke-verifies every cli
  lane against a FRESH handle on the same `--db` a worker is handed, through the consumers' own
  read paths (`recall_rules`, policy round-trip, knowledge recall). The receipt is a manifest
  (v1.0) keyed on the stable PAT-/POL- ids mapping each rule to its three copies; any missing copy
  fails the WHOLE fan-out loud. A daemon-held store is NEVER CLI-written (single-writer invariant):
  `--enforcement-crew-api <url>` records the pending transport and emits the
  `POST /api/v1/governance/{policies,rules}` payload instead, and any lane path under
  `~/.wicked-crew` is refused before a single lane is written. Crate surface:
  `wicked_governance::{fanout, load_ruleset, FanoutManifest, FanoutScope, FanoutTargets, …}`.
- **`scope: workspace` in the fan-out manifest** (AW-6 / arch-R20 decision). Cross-repo doctrine
  placement decided: replicate-to-every-repo — a workspace-scoped fan-out carries one discovery
  copy per live repo graph (caller-enumerated; zero discovery targets refuse loudly), with id-keyed
  idempotent re-ingest keeping the N copies syncable. Zero engine change. Option (b), a
  workspace-root store with multi-`--db` resolution, is documented and parked as P-2 in
  DES-OUTGOV-008 — unparking requires an estate-owner ruling on resolution + gate precedence.
- **MarkdownAdapter on the `SourceAdapter` ingest seam** (AW-3 / arch-R1). One parse convention —
  YAML frontmatter (`id`, `title`, plus optional `status`/`enforcement_class`/`applies_to`/`scope`/
  `supersedes`/`domain`/`confidence`/`targets`) and a `## Rules` section of
  `- <PAT|POL-nnn> (<severity>): <statement>` items. All output materializes through the existing
  `normalize_bundle` fail-closed invariants (no second parse path); a malformed doc fails LOUD
  per-file with path + reason, never a silent skip; a doc without a Rules section is a valid
  doc-only ingest. `wicked-core rules ingest --dir <path>` now ingests frontmattered `*.md` docs
  anywhere under the directory alongside the existing `policies/*.json` + `rules/*.json` lanes,
  with cross-lane duplicate-id refusal.
- **Schema-document nodes** — `wicked_governance::register_schema_nodes` registers the 4 governance
  schemas on the graph (one node per schema file, keyed by `$id`, carrying contract + bundle
  version); `rules ingest` refreshes them on every successful run (the schemas/README.md AW-3 seam).
- **wicked-governance owns the 4 governance schemas** (AW-2 / arch-R10, #309). Re-homed byte-for-byte
  from the retired wicked-brain repo at bundle VERSION 1.1.0 (`crates/wicked-governance/schemas/`),
  embedded via `include_str!` with lift-fidelity + INV-C4 vocabulary guards; garden vendors from
  this copy. Also adds the thin root `CLAUDE.md` pointer stub (AW-1).

### Fixed
- `required_deliverables` enforced at the result fold, not in one runner (#297 → #308).
- Failure-excerpt triage keeps the TAIL of the output, where the error usually is (crew#322 → #307).
- Seat failover keyed to phase idempotency, not input governance (#292 → #304).
- Resume re-provisions a reaped worktree before re-dispatching into it (#290 → #303).

## [core-ts 0.7.1] — 2026-08-25

### Added
- **Project-scoped graph vouching** — a run in a project sees the project's graph when the engine
  can vouch for it (#299, review follow-ups #300).
- ACP bridge-death instrumentation for the crew#290 session-death hunt (#289).
- Engine-injected phase-scope preamble + pre-build code-change warnings on the plan path (#287).

### Fixed
- ACP inbound frames dispatched by **method**, not id alone — a permission request no longer ends
  the turn (#295).
- Seat failover walks the full roster; evaluator prompts bounded (#286).
- napi cross-compile: aarch64-linux built with the GNU toolchain (zig 0.13 rejected the erratum
  flag) (#301, #302).

## [core-ts 0.7.0] — 2026-08-19

### Added
- **Live unit output streaming + phase substance gate** (#279).
- Per-seat `login_invocation` for PTY-hosted sign-in (#278); persistent worker config home
  (crew#267, #277); seat failover, failed-run resume, worker reaping, death instrumentation (#275).

### Fixed
- **The evidence gate sees committed work** (core#280 → #281). `EVIDENCE_SCRIPT` now also counts
  commits the run branch carries beyond every non-`wicked/*` local branch; the layer-2 agent judge
  receives harness-derived worktree evidence (porcelain + run-branch `git log --stat`, capped);
  the phase-substance gate widened identically. The built-in floor pin moved to `e2e7af1db9e48454`
  (const, shipped defs, and any operator overlay must agree).
- ACP bridge auth refusal named instead of a silent death (crew#267 root cause, #276); agent-memory
  carve-out follows the resolved Claude config home (#273).

## [core-ts 0.6.3] — 2026-08-14

### Fixed
- Strip the pi RPC banner at capture and at the exit-0 arm (restores the FINDING-101 audit
  windows); surface stderr on seat death (#269, #271).

## [core-ts 0.6.2] — 2026-08-14

### Added
- Run archival — write off terminal runs without deleting evidence (crew#265 core half, #266).
- Filesystem boundary armed on the ACP path (#263).

### Fixed
- System-temp scratch writes advisory + worker TMPDIR kept in-boundary (#265).

## [core-ts 0.6.1] — 2026-08-14

### Added
- Launcher-declared `extraWriteRoots` launch option (core#259, #261).
- ACP elicitation maps, Rust half (core#234 reland, #258).

### Fixed
- `write_lock` held around every ACP `proc.stdin` write (FINDING-254, #257); ACP tool name resolved
  from `toolCall.name` before `toolCall.title` (core#100, #247).
- Coverage gate requires at least one genuinely resolved requirement (#251); bus-path fail-closed +
  evaluator≠creator wire contract (P9, #250); warn when evaluator≠creator separation cannot be
  enforced (#248); `feature/test` workflows armed with `evidence_floor()` (#256).
- Estate deps bumped 0.14.3 → 0.14.5 (#252).

## [core-ts 0.6.0] — 2026-08-12

### Added
- **Project model** — projects + memberships + durable interaction requests (DES-PROJECT-001, #246).

### Changed
- Depend on wicked-estate via crates.io versions, not path (#245); napi release pinned to the
  estate release tag (#244).

## [core-ts 0.5.0] — 2026-08-10

### Added
- **A run consumes the repo's estate graph** — ACP parity + repo-scoped graph surface (core#122,
  #240); `ToolInvoked` observability event (FINDING-046, #239); cache-token breakdown on
  `cliUsage` (FINDING-012, #223); domain graph persisted into estate.db with a read boundary for
  governed extraction (#213, #237).

### Fixed
- Coverage gate recomputes from the store — never trusts the creator's `coverage-report.json`
  (#230); repo coverage computed over the repo's own graph (FINDING-009, #225); content-free
  requirement accounting denied (#210).
- Filesystem boundary extended to Bash write targets, with `/dev/null`/fd-dup and glued-separator
  fixes (FINDING-045, #226–#228); advisory boundary READ deny unblocks unattended governed runs
  (core#219, #220); a governed worker's write into its own `~/.claude` tree is advisory, not fatal
  (#236).
- One canonical `humanConfirm` parser that fails closed (FINDING-019, #224); single-seat roster
  short-circuits the council (FINDING-010, #222); transient single-shot worker failures retried
  (#216); `register_repo` root canonicalized (core#214, #221).
- Reverted the first ACP elicitation landing (#212 → #233); re-landed later in 0.6.1.

## [0.4.0] / [core-ts 0.4.1] — 2026-08-05

Joint release (#198; the engine bump carries no separate git tag — `core-ts-v0.4.1` is the release
commit).

### Added
- **An installer that verifies what it installed** — deploy step probes the deployed binary
  (FINDING-081, #195, #198).
- Requirement-string concentration reported in coverage (FINDING-131, #180).

### Fixed
- **Version-lock between the engine and the gate-hook CLI** (core#167, #181): the gate refuses a
  protocol-mismatched hook, keyed on the binary's identity, not its path (FINDING-083, #194).
- Coverage validator measures the REPO's graph, not the actor's (FINDING-091, #196); a coverage
  report over zero behavior-bearing nodes is not a pass (FINDING-009, #190); coverage gets its own
  store carrier (core#166, #182).
- A denial names what it MEASURED (FINDING-092, #197); skipped workflows and governance DENYs say
  WHY (#191); claims stamped from the wall clock (FINDING-017, #192); evaluator contradicting its
  own verdict fails closed (FINDING-085, #188).
- Installed workflow defs with an unknown pin refused at dispatch (#187); tool-call paths outside
  the unit's boundary refused (FINDING-045, #189); each run's own repo bound into its Tool phases
  (FINDING-075, #179); runs left `executing` with no worker announced (core#124, #183); validator
  seat rotation past a seat that cannot run (core#132, #185); estate `ValidationClaim` adopted,
  pin moved off a stale tag (FINDING-078, #184).

## [core-ts 0.4.0] — 2026-08-04

The E2E-campaign hardening wave (FINDING-0xx series from the 15-repo corpus).

### Added
- Policy/conformance-rule **retirement** (FINDING-038, #149).
- Deterministic **evidence floor** on the built-in Evaluator phases, extended to the shipped
  drop-ins (#154, #177); replacement workflows may not silently remove a validator pin (#155);
  shipped drop-in's validator seeded on the plan path (FINDING-066, #164).
- One hardening chokepoint for every process spawn (#168); cross-artifact constants pinned in
  lockstep tests (#171).

### Fixed
- **Operational store kept out of every worker's reach** (FINDING-067, #165) — the deliberate
  enforcement/discovery store split later formalized as the fan-out contract.
- Worker CLI config isolated from the operator's (FINDING-047/045, #153).
- Council: quorum counted, a panicked council no longer takes the run with it (FINDING-026, #151);
  a vote is the option it names (FINDING-056, #158); three timing budgets measured for real
  (FINDING-040, #150); dispatch budget real, correct, affordable (#147).
- Validator: "could not run the check" no longer reported as "the check said no" (#156); the
  judge-prompt reason kept (FINDING-064, #163); a governed unit that cannot be armed says so
  (FINDING-063, #162); governed units refuse ungovernable ACP paths (FINDING-060/061, #161).
- Abandoned chat sessions reclaimed — idle TTL, pool cap (FINDING-027, #152); cached input tokens
  counted on the wrapped path (FINDING-058, #159); worktree existence verified, not inferred
  (FINDING-059, #160); dead `repo-graph` workflow deleted (FINDING-070, #175); onboarding's
  unpassable `domain` phase dropped (FINDING-068, #174); one spelling for a repo's code graph
  (FINDING-069, #172).

## [core-ts 0.3.0] — 2026-07-30

### Added
- **Chat sessions** — warm ACP seat pool + parallel group fan-out (core#13, crew#165, #134).

## [0.3.1] / [core-ts 0.2.1] — 2026-07-28

### Fixed
- Launch preflight is synchronous at `LaunchRun`, covering Tool-executor phases (#120 → #121,
  #123); onboarding `domain` phase runs domain-graph, `--help` guarded (#125).
- Plugin-skill invocation form + Unknown-command no-op tripwire (#126 → #127); banner-tolerant
  validator verdict parse via keyword-alone contract lines (#128 → #129).

## [0.3.0] / [core-ts 0.2.0] — 2026-07-27

### Added
- **Full-roster ACP** — native copilot stdio + native opencode ACP, registry fixes, governed
  fall-through for non-claude CLIs, adapter provenance docs, Windows `.cmd` launcher shims
  (#109–#111).
- **Live council deliberation** — events, seat lenses, 75% approval bar, runoff ballots (#108);
  votes parallelized + distribution-thread panics guarded (#107).
- Agent-judged failure triage (#115); environment-refusal escalation ladder — auto-grant, else
  bubble to operator (#114); external-transform assumption capture (#116); PTY stall detection +
  `collab` built-in workflow (#117).

### Fixed
- Usage parsed from the ACP prompt result — Burn panel no longer empty for ecosystem adapters
  (#113); operator messages delivered to ACP-backed runs (#112).

## [0.2.0] — 2026-07-21

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
