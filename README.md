# wicked-core

The in-process composition runtime **and** event-driven execution engine for the wicked-* ecosystem.
One thread — the **actor** — owns the writable estate store; everything else holds a clonable `Core`
handle and talks to it via commands + a live `CoreEvent` stream. That single-writer seam is what lets
multiple consumers (agent, studio, MCP) compose the core services without racing on the shared SQLite
file — and it's the durable, ordering authority the workflow engine runs on.

## What it is

Two things in one binary:

1. **A single-writer composition runtime** — the actor owns the estate store; reads and writes funnel
   through it, so there's exactly one writer and no file contention.
2. **An execution engine** — it drives multi-phase AI development runs (feature / bug / migration) as a
   function of **workflow data**, executing each phase by invoking a real agentic CLI, and governing
   every phase transition through a gate.

## Architecture (built)

- **Single-writer actor** (`actor::run` on a dedicated thread, reached through the `Core` handle) —
  owns the SQLite estate store; a command API + `CoreEvent` fan-out is the only way in. Live output
  streams to subscribers as work happens.
- **Governed run pipeline** — plan → distribute → execute → evidence, all on the one store. Governance
  is **deny-dominates** and fail-closed (a run with more units than the governed span is rejected, not
  run ungoverned).
- **Real CLI execution** — the wrapped-CLI step runner runs an actual agentic CLI as a subprocess in the
  run's git worktree, streaming its output; the prompt is a guarded argv element (no shell, no flag
  smuggling), bounded by a timeout.
- **Resume / cursor** — runs are resumable from a persisted cursor after a crash.
- **Council routing** — multi-CLI seats vote/rank to assign a CLI per unit (the seat diversity behind
  evaluator≠creator).
- **Campaign DAG** — cross-workflow composition (fan-out, failure edges) as a scheduler primitive.

## Workflows are DATA (not code)

A workflow is a `WorkflowDef` = `{ id, phases: [PhaseDef] }` — pure, serde data. **Adding a workflow is
dropping a JSON file, not editing this crate** (Law 2):

```rust
let mut registry = WorkflowRegistry::with_defaults(); // seeds feature / bug / migration
registry.load_dir(workflows_dir)?;                    // overlay your drop-in *.json files
```

`load_dir` takes an **already-resolved** path (no `~` expansion). At runtime the engine resolves the
overlay dir itself: `$WICKED_WORKFLOWS_DIR`, else `$HOME/.config/wicked-core/workflows`.

- The three built-ins ship as `workflows/*.json` (the human-editable mirror of the seed builders,
  drift-guarded) — see [`workflows/README.md`](workflows/README.md) for the full field contract.
- Every `PhaseDef` field but `id` defaults, so the minimal phase is `{"id":"x"}`. A misspelled key is a
  **loud** parse error naming the file (`deny_unknown_fields`), never a silent default.
- Validation: unique phase ids, resolvable deps, and **declaration order = execution order** (a
  dependency must point to an earlier phase — which makes the layout a valid topological order and makes
  cycles unexpressible).

The boundary: **primitives** (`GateType`, `StageKind`, `PhaseRole`) are the engine's typed vocabulary;
**workflows that compose them are open data**. Event names are free strings, so a new workflow publishes
/ subscribes new `wicked.*` events with zero code. New primitive = code; new workflow/event = data.

## Data-driven planning

Selecting a workflow makes **planning** a function of its data. `plan_from_def` derives one work unit
per phase, taking each unit's stage from the phase's **declared** `kind` (not a keyword guess over the
prose) — so the def's phase list + stages drive the plan, not a sentence-splitter:

```
wicked-core run --problem "add SSO login" --workflow feature
```

Without `--workflow`, planning falls back to the legacy free-text planner (prose split + keyword
classify), so existing callers are unchanged.

## Skills-driven phase execution

A phase's work is driven by a **skill**, not an ad-hoc prompt (consistent control). A `PhaseDef` carries
an optional `skill_ref` and a runtime `allowed_skills` allowlist (least-privilege tool/skill scope, the
`--allowedTools` analog); both ride onto the work unit. When a unit is skill-driven, the runner invokes
`/{skill_ref} {description}` — the verified headless recipe (`claude -p "/wicked-testing-<skill> …"`),
which the harness expands deterministically. An unskilled unit stays on the authored prompt.

## Gates (design — see DES-EXEC-001)

The gate's deterministic check is **not** a generic precanned verifier. A test-strategy agent authors a
**grounded, deterministic validation script** for the specific phase/task, stored in the vault as its
evidence evaluator, versioned + approved; a spec change regenerates it (diff → re-approve). The
evaluator is a **pair** authored by **two independent strategists** — a deterministic validator +
an agent-based (semantic) validator — combined so that **Approve requires the deterministic piece to
PASS; the agent piece can REJECT but is never the sole approver.** This makes evaluator≠creator
deterministic and auditable, and preserves "a model may never *solely* approve a gate."

**Trust model (named, not overclaimed):** diverse-seat agent consensus, on a deterministic structural
floor, with human escalation above a threshold. A green run means "diverse seats + the escalation policy
agreed," **not** "proven."

## Status

| Layer | State |
|---|---|
| Single-writer actor, CoreEvent stream, worktree exec, resume, governance, council, campaign DAG | ✅ built |
| WorkflowDef spine as data + registry + `load_dir` + shipped JSON | ✅ built |
| Data-driven planner (`plan_from_def`) wired into the runtime (`--workflow`) | ✅ built |
| Per-phase **gate** — a phase's `GateSpec` drives the human-confirm pause, OR'd with the run-level `--confirm` | ✅ built |
| Per-phase **role** / artifact-passing — an `Evaluator`-role unit reviews the prior `Creator`-role unit's **cold** output | ✅ built |
| Skills-driven invocation (headless slash form) — **live-verified** vs real `claude` (`tests/skills_live.rs`) | ✅ built |
| `allowed_skills` injection (`{SKILLS}` template placeholder, CLI-agnostic) | ✅ built |
| **Dual-validator gate core** — deterministic (skill-authored + re-verify) + agent (controlled reviewer) + combination rule; approval-gated + fail-closed + denylist; **live-verified**, adversarially reviewed (14 findings incl. RCE + fail-open, all fixed) | ✅ built |
| **Validator vault** — content-hash `pin` + `store`/`load` with read-time content-address verification (`validator_vault.rs`) | ✅ built |
| **Provisioning** — `provision-validator` / `approve-validator` CLI (author→approve→pin, **live-verified**) + a phase's `validator_pin` auto-loaded from the vault at plan time so the gate **engages** | ✅ built |
| Gate wired into the phase flow — deterministic re-verify + off-thread agent judge fold, deny-dominates, **repo-less runs fail-closed** (agent can't lone-approve) | ✅ built |
| **Rust↔wicked-bus bridge** — emit/poll matching the JS schema (config TTL), `wicked.run.requested`→launch + `run.launched`, at-least-once retry; **cross-language round-trip verified** (`src/bus.rs`) | ✅ built |
| **Law 1 execution-mediation seam** — actor publishes `task.dispatched` → a `cli-runner` subscriber executes off-actor → `task.completed` → actor; **opt-in** (`WICKED_BUS_EXEC`), default stays in-process; round-trip verified identical to in-process (`src/cli_runner.rs`) | ✅ built |
| **Gate a shipped workflow** — the `gate-phase` CLI authors→approves→pins a validator into a drop-in def (one command) | ✅ built |
| **napi bridge** (`../wicked-core-ts`) — `launchRun`/`subscribe`/`confirmGate` over FFI; adversarially reviewed (9 findings incl. tsfn leak + OOM, fixed); Node smoke passes | ✅ built |
| Real **OS sandbox** for validator execution (the denylist is a backstop, not a boundary) | ⬜ hardening |
| napi → **studio UI** wiring (the addon exists; the Tauri/React surface consumes it) | ⬜ follow-up |
| Council-assigns-skill-at-runtime | ⬜ deferred — needs a grounded skill-ranking design (guessing skill names would violate grounding) |
| Whether a given CLI *honors* its allowlist flag (e.g. does `--allowedTools` scope skills) | ⬜ per-CLI spike |

> **⚠️ Shipped-workflow caveat:** the built-in `feature`/`bug`/`migration` defs ship with `validator_pin: null`, so the dual-validator gate is **inert for the shipped workflows by default** — it engages only for a def whose phase carries a `validator_pin`. Turn it on with one command: `wicked-core gate-phase --workflow feature --phase build --criterion "…"` authors→approves→pins a validator and emits a gated drop-in def you then run with `--workflow <new-id>`.

## Reference

- `.product/DES-EXEC-001-event-driven-workflow-execution.md` — the execution-engine design (the two
  laws, gate model rev0.4/0.5, skills seam §4.1/§4.2, event architecture).
- `ORCHESTRATOR.md` — the deep orchestrator reference.
- `workflows/README.md` — the drop-in workflow file contract.
