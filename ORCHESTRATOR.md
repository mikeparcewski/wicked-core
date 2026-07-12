# AGENTIC-CLI ORCHESTRATOR DASHBOARD — Design (FINAL)

## 1. North star

The orchestrator is a **single in-process engine (wicked-core / COE) with multiple surfaces**, the first being an **egui desktop dashboard**. COE is the right engine: a single-writer `StoreActor` (`actor.rs:15`) owns one writable `SqliteStore` over the wicked-estate graph, and every consumer holds a clonable `Core` handle (`lib.rs:62`) that talks to it via typed `Command`s with oneshot replies plus a live `CoreEvent` fan-out (`lib.rs:78`). We do **not** build a parallel runtime: we extend COE's existing `plan → distribute(council) → execute(governance+orchestration gates) → evidence` pipeline (`pipeline.rs:36`) from a fire-and-forget straight-through loop into a **typed, resumable, multi-stage, interactive Workflow runtime**. wicked-agent is killed; what survives is a thin `wicked-core gate-hook` subcommand (porting `inject.rs:496 run_gate_hook`) plus the operator CLI (`bin/wicked-core.rs`). The same engine drives the egui dashboard today and a CLI REPL / webapp later — the "one engine, many surfaces" shape.

**One invariant, stated precisely (it survives P4).** The actor is the *only* writer of the COE SQLite store. Slow work (subprocesses, council dispatch, wicked-testing, git) runs off-thread on a bounded worker pool that holds **no store handle**; workers return `StepResult` messages that only the actor applies. The one place this was historically false — the out-of-process `gate-hook` that opened the same SQLite file (`inject.rs:502/522`) — is fixed at P0: **the hook writes only an append-only `decisions.ndjson`; it never opens the store.** The actor drains that file into the store via a new `ApplyHookDecisions` command. Single-writer therefore holds during wrapped runs, not just on the stub. (See §3.5, §10, §11.)

---

## 2. The workflow primitive

### 2.1 The problem with today's model

`pipeline::run_session` (`pipeline.rs:36`) is one straight-through loop: `plan_units` (`plan.rs:9`) splits text into a **flat homogeneous** `Vec<WorkUnit>`, the loop executes each, and the session is set `Completed` **unconditionally** (`pipeline.rs:170`). `HumanConfirm` (`domain.rs:29`) and `SessionStatus::AwaitingHuman` (`domain.rs:17`) are **declared, persisted, but never read** — `pipeline.rs:61` hardcodes `HumanConfirm::None`. There is no `Resume`. The workflow primitive replaces this with a typed stage machine that **yields at gates and re-enters from a cursor**.

wicked-orchestration already has most of the machinery: `Workflow { phases: Vec<(String,String)>, current_index, status }` (`domain.rs:80`), an ordered `advance()` returning `Advanced/Waiting/AwaitingHuman/Complete/Failed` (`runner.rs:183`), and a single-writer idempotent `apply_event` reducer (`reducer.rs:142`). We build **on top of** these — but with three explicit refactors the original draft glossed over (all pulled forward into P1, see §8):

1. **One cursor.** `Run.cursor.stage_ix` is *the projection of* `Workflow.current_index` — there is exactly one integer for "which stage." The session row stores only the sub-stage detail (`unit_ix`, `exec_phase`); `stage_ix` is always read from the workflow node. No second cursor, no drift.
2. **One driver.** We standardize on `advance()` and delete `tick_workflow` (`runner.rs:109`, boolean-driven, never opens phases). `create_workflow` + `advance` replace `register_workflow` + `tick_workflow` (`pipeline.rs:91/137`).
3. **One phase opener.** `advance()` is the *sole* opener of orchestration phases (`runner.rs:246`). The execute backend is made **phase-pure**: it receives an already-open phase, runs work, and reports status via `StepResult`; it no longer calls `Phase::open` itself. This removes the double-open collision between `advance()` and `execute_unit_wrapped` (`execute.rs:289`).

### 2.2 Typed Workflow / Run model

```rust
// New in wicked-core/src/workflow.rs — the typed spine (lives in CORE, not orchestration).

/// A reusable, ordered template. Persisted as its own node-kind.
pub struct WorkflowDef {
    pub id: String,
    pub name: String,
    pub stages: Vec<StageSpec>,      // typed, ordered (replaces flat plan_units)
}

pub struct StageSpec {
    pub kind: StageKind,             // Recon | AdversarialReview | FunctionalTest | Build | Custom
    pub name: String,
    pub select_key: String,          // governance select key, DERIVED from kind (see §2.3 step 2)
    pub gate: GateSpec,              // typed human-confirm policy for THIS stage
    pub cli_policy: CliPolicy,       // how CLIs are chosen for this stage (§5)
    pub prompt_template: String,
    pub consumes: Option<EvidenceRef>, // typed inter-stage input (§2.5)
    pub produces: EvidenceSpec,        // typed inter-stage output (§2.5)
    pub optional: bool,
    pub retries: u8,
}

pub enum StageKind { Recon, AdversarialReview, FunctionalTest, Build, Custom(String) }

pub enum GateSpec {
    Auto,                                  // governance-only, no human
    HumanConfirm { prompt: String },       // typed, clearable human gate (§2.3 step 3)
    HumanConfirmIf(Condition),             // e.g. gate only if verdict != PASS
}

/// A live execution instance of a WorkflowDef against a repo.
pub struct Run {
    pub id: String,                  // == AgentSession.id (reuse the node)
    pub def_id: String,
    pub repo_ref: String,            // FK into the repo registry (§4)
    pub cursor: Cursor,              // THE resumption point
    pub status: RunStatus,           // reuse SessionStatus + AwaitingHuman (domain.rs:17)
    pub testing_run_id: Option<String>, // correlation key for wicked-testing (§6)
}

pub struct Cursor {
    pub unit_ix: usize,              // session-owned sub-stage detail
    pub exec_phase: ExecPhase,       // session-owned
    pub attempt: u32,                // retry attempt counter — folded into ALL event ids (§2.6)
    // stage_ix is NOT stored here; it is read from Workflow.current_index (one cursor)
}
pub enum ExecPhase { Planned, Distributed, Executing, AwaitingGate, AwaitingHuman, Cancelling, Done }
```

**Mapping onto existing types — backward-compat done correctly.** `Run` extends `AgentSession` (`domain.rs:41`): we add `unit_ix`/`exec_phase`/`attempt`/`repo_ref`/`testing_run_id`. We do **not** change the arity of `Workflow.phases: Vec<(String,String)>` (which `FromNode` deserializes via `serde_json::from_value::<Vec<(String,String)>>`, `domain.rs:166`). Instead, the `StageSpec` for each phase is **JSON-encoded into the existing second `String` slot** (the first slot stays the `select_key` so `phase_name == governance select key` still holds). Existing workflow nodes deserialize unchanged; the parse of the second slot is documented and tolerant (additive-field policy: unknown keys ignored). `RunStatus` reuses `SessionStatus::AwaitingHuman`, which exists but is dead today.

**Layering.** All typed methodology types (`WorkflowDef`, `StageSpec`, `GateSpec`, `StageKind`, `RunSpec`, `RepoEntry`) live in **wicked-core**. wicked-orchestration stays lane-disjoint: it gains only the *generic* `AwaitingConfirmation` `PhaseStatus`, a *generic* clearable human-confirm reducer clause, and attempt-scoped event ids. It learns nothing about `StageKind` or governance; the JSON in the phase tuple is opaque to it.

### 2.3 How a stage executes, pauses, resumes

The engine is **re-entrant**. `run_session` is split into a `step(run)` function driven by the stored cursor + `advance()`:

1. **Execute step (off-thread).** The actor builds a `StepInput` (§3.4) by pre-loading every store read the step needs (prior `work_output`, units, scope, scenario path, creator output for evaluators), then dispatches it to a worker. The worker runs the slow work with no store handle: for `Recon`/`Build` the wrapped-CLI subprocess backend (§5, P4a); for `AdversarialReview` a council creator→evaluator pass (§2.4); for `FunctionalTest` a wicked-testing subprocess (§6). The worker streams stdout/stderr chunks back as it drains them (required to avoid the 64KB pipe-buffer hang) and posts a final `StepResult`.

2. **Governance gate (actor only).** `advance()` opens the phase; the actor advances `Pending→InProgress→ReadyForGate→GateRunning` (`execute.rs:50`), runs `select(select_key)`/`decide(context)` (`engine.rs:69/234`), `apply_gate`s the claim (`gate.rs:78`). **`select_key` is derived from `StageKind`** (e.g. `"adversarial_review"`, `"recon"`) — *not* the legacy `"unit-{ord}"` ordinal (`execute.rs:61/82`) — and an explicit `stage → policy` map binds it. (Per-stage policy was *not* already working; this binding is net-new and owned here.)

3. **Pause at the human-confirm gate — a CLEARABLE marker.** If `stage.gate` is `HumanConfirm`, the actor sets `run.status = AwaitingHuman`, `cursor.exec_phase = AwaitingHuman`, **persists**, emits `CoreEvent::AwaitingHuman { run, stage_ix, prompt }`, and returns to the recv loop. Enforcement: orchestration gains an `AwaitingConfirmation` `PhaseStatus` + `ALLOWED_TRANSITIONS` edges (`transitions.rs:21`), **a new arm in `advance()`** (`runner.rs:210`) that maps it to a real `AdvanceOutcome::AwaitingHuman` (without this it falls into the `other => Waiting` catch-all at `runner.rs:255` and the pause is invisible), and a **second structural veto clause** in `apply_event` parallel to the `gate_decision==Deny` veto (`reducer.rs:170`). Critically, **this marker is NOT modeled on `gate_decision`**, which is sticky/monotonic and never cleared (`reducer.rs:202`). The human-confirm marker is **clearable**: an unresolved `Event.human_decision == Pending` vetoes any approving transition; a `Confirm`/`Approve` event *clears* it and releases approval. "Unconfirmed ⇒ not approved" holds by any route, and "confirmed ⇒ approvable" actually works.

4. **Resume / confirm / redirect.** `Core::confirm_gate(run_id, decision)` reloads the `Run` + units, applies the human decision through `apply_event` (typed `human_decision` marker on `Event`, `reducer.rs:48`), advances the cursor, and re-enters `step(run)`. The decision carries an **optional redirect payload** (`HumanDecision::Approve { amend: Option<PromptAmendment> }`) so a human can inject guidance or rewrite the next stage's prompt — the gate is steering, not bless-or-bounce. Because all state lives in the estate store and event ids are deterministic *and attempt-scoped*, resume is idempotent for store transitions. (Subprocess re-run on crash is handled separately in §2.6.)

### 2.4 The three concrete stage templates (the methodology spine)

| Stage | What it does | Engine | evaluator ≠ creator |
|---|---|---|---|
| **RECON** | Decompose the problem; each unit = "map area X of the repo." Runs wrapped CLI(s) in the worktree producing structured recon output (`capabilities/seams/gaps`, typed — §2.5). | COE wrapped-CLI backend (P4a) + council distribute to pick mapper CLI(s). | n/a (creation stage) |
| **ADVERSARIAL REVIEW** | Take the prior stage's `work_output` as input; a **different** CLI critiques it. | wicked-council, role-parameterized: creator pass then evaluator pass with a distinct seat. | **Enforced by guard, including standalone** (§2.4.1). |
| **FUNCTIONAL TEST** | Produce/accept a scenario `.md`, run the 3-agent acceptance pipeline, gate on the verdict. | wicked-testing (Node) via host-CLI subprocess (§6). Writer→Executor→Reviewer is inherently evaluator≠creator (reviewer isolation, read-only). | Built into wicked-testing's reviewer isolation. |

#### 2.4.1 evaluator ≠ creator — enforced, including standalone
`next_cli_in_roster` (`pipeline.rs:199`) wraps around on a 1-element roster (`get(1)=None → or_else(first) → same cli`), so a standalone AdversarialReview would silently grade itself. The final design **does not rely on roster wrap-around**. At `DefineWorkflow`/`LaunchRun` we *validate* that an `AdversarialReview` stage either (a) has ≥2 distinct CLIs, or (b) forces a **distinct evaluator identity** for the same binary (distinct seat / model / system prompt + a distinct governance `claim_id` via `decide_as`). At runtime we **assert `evaluator_cli/seat != creator_cli/seat`** before dispatch and fail-fast otherwise. Standalone behavior is explicit: a single-CLI AdversarialReview runs the same binary under a distinct evaluator seat and distinct claim identity; it never reuses the creator's transcript or identity.

A default `WorkflowDef` ("standard project workflow") is `[Recon, Build, AdversarialReview, FunctionalTest]` with `HumanConfirm` gates after Recon (approve the plan) and after AdversarialReview (approve before testing), and an auto gate on FunctionalTest that becomes `HumanConfirmIf(verdict != PASS)`.

### 2.5 Typed inter-stage contract (the hand-off that makes a spine)
Each stage declares `produces: EvidenceSpec` and `consumes: Option<EvidenceRef>`. Outputs are persisted as `work_output` nodes carrying a typed `evidence_kind` and a versioned JSON body:

```rust
pub struct EvidenceSpec { pub kind: String, pub schema_version: u32, pub required_fields: Vec<String> }
pub struct EvidenceRef  { pub from_stage_ix: usize, pub kind: String }

// Recon output (kind = "recon.map", schema_version = 1)
{ "capabilities": [Capability], "seams": [Seam], "gaps": [Gap], "notes": String }
```

The actor validates a producing step's output against `produces.required_fields` before marking the stage `Done`, and resolves `consumes` into the `StepInput` for the next stage (so AdversarialReview gets the Recon map, and scenario authoring gets the Build output). Unknown fields are ignored (additive policy); a missing required field fails the stage into the human gate.

### 2.6 Retry, cancel, and crash re-run
- **Retry** is first-class (not an open decision). Orchestration gains a `Rejected → InProgress` edge bounded by `StageSpec.retries`, gated by a human `Retry` decision. Because `apply_event` dedups by deterministic event id (`reducer.rs:152`), **the `cursor.attempt` counter is folded into every event id** (open-, advance-, gate-) so a retried transition is not swallowed as a no-op.
- **Cancel / Pause** are first-class commands (§3.1). Each worker holds a child-process handle + cancellation token; `CancelRun` sends the kill signal and transitions the run to `Cancelled` (a terminal status); `PauseRun` is cooperative (no new units dispatched, current child optionally signalled). This makes "stop a runaway CLI / intervene mid-run" real rather than wait-for-`ApplyStepResult`.
- **Crash re-run honesty.** Resume is idempotent for *store* transitions but a real subprocess that ran before the crash is **not** idempotent. Before dispatching a real step the actor persists an `attempt-started` checkpoint; the worker writes output to a deterministic path. On resume the actor checks for a **completion sentinel** (the expected `work_output` node / output file): if present, it skips re-execution and applies the result; if an attempt was started but no sentinel exists, it does **not** blindly re-run — it surfaces `AwaitingHuman { prompt: "previous attempt interrupted — re-run or skip?" }`. This is an accepted, surfaced residual risk, not a silent re-execution (see §10).

---

## 3. COE API additions

### 3.1 New `Command` variants (`command.rs:10`)

```rust
// Repo registry
RegisterRepo { spec: RepoSpec, reply: oneshot::Sender<Result<RepoEntry>> },
ListRepos    { reply: oneshot::Sender<Vec<RepoEntry>> },

// Workflow definitions
DefineWorkflow { def: WorkflowDef, reply: oneshot::Sender<Result<String>> }, // validates §2.4.1
ListWorkflows  { reply: oneshot::Sender<Vec<WorkflowDef>> },

// Runs (interactive, resumable)
LaunchRun  { spec: RunSpec,  reply: oneshot::Sender<Result<String>> }, // returns run_id immediately
AdvanceRun { run_id: String, reply: oneshot::Sender<Result<RunStatus>> },
ResumeRun  { run_id: String, reply: oneshot::Sender<Result<RunStatus>> }, // after crash/restart
ConfirmGate { run_id: String, decision: HumanDecision, reply: oneshot::Sender<Result<RunStatus>> },

// Interaction / intervention (NEW — the "interactive" in interactive)
CancelRun { run_id: String, reply: oneshot::Sender<Result<RunStatus>> },
PauseRun  { run_id: String, reply: oneshot::Sender<Result<RunStatus>> },
AssignClis { run_id: String, stage_ix: usize, unit_ix: usize, clis: Vec<String> }, // not-yet-dispatched units only
ChatWithCli { run_id: Option<String>, cli: String, message: String, reply: oneshot::Sender<Result<ChatHandle>> },

// Internal: worker → actor write-back
ApplyStepResult     { run_id: String, result: StepResult },
ApplyHookDecisions  { run_id: String, ndjson_path: PathBuf }, // drains gate-hook decisions (§3.5)
```

`RunSpec { repo_ref, def_id, problem, clis, entity_mode }` replaces `LaunchSpec` (`lib.rs:36`). Session-level `HumanConfirm` is **retired**: per-stage `GateSpec` is the sole gate authority. When a caller supplies no `def_id`, COE synthesizes a default `WorkflowDef` and translates any legacy `HumanConfirm` value into per-stage `GateSpec`s (back-compat shim) so there is exactly one gate model.

### 3.2 New `CoreEvent` variants (`event.rs:9`)

```rust
StageStarted    { run, stage_ix, kind },
StageCompleted  { run, stage_ix, verdict: Option<String> },
AwaitingHuman   { run, stage_ix, prompt },       // the pause signal
Resumed         { run, stage_ix },
CliExecuting    { run, stage_ix, unit_ix, cli }, // per-CLI start
CliOutputDelta  { run, stage_ix, unit_ix, cli, chunk }, // live stdout/stderr (NEW)
VerdictRecorded { run, stage_ix, value },
RunCancelled    { run, stage_ix },
RepoRegistered  { repo_ref },
```

**Ordering is preserved by construction.** Workers never emit. They post results/deltas back to the actor; the **actor is the single emit point** (`actor.rs:77 emit`) and stamps every event with a monotonic `seq` plus the `run_id` before fan-out. So even though multiple workers feed back, the UI timeline sees one totally-ordered, per-run-filterable stream — the multi-producer ordering hazard is eliminated, not merely tagged.

### 3.3 The actor-blocks-during-launch fix + the in-flight-run guard (mandatory)

**Blocker (recon gap):** `actor.rs:45-71` runs `pipeline::run_session` synchronously on the single store-owning thread, freezing all reads during a launch. **Fix — keep single-writer, move execution off-thread:**

- The actor **never runs subprocesses.** On `LaunchRun`/`AdvanceRun`/`ResumeRun`/`ConfirmGate` it does the cheap store writes (persist Run/cursor, emit `StageStarted`), builds a `StepInput` snapshot, **dispatches the slow step to the bounded worker pool**, and returns immediately.
- The worker runs long I/O **with no store handle**, streams `CliOutputDelta`s, and posts `Command::ApplyStepResult` back over a `Sender<Command>` clone the actor holds.
- The actor applies the write (work_output, governance gate via `apply_gate`, cursor advance), emits events, and dispatches the next slow step or pauses at a human gate.

**The concurrency guard the original draft missed.** Once the actor returns to its recv loop mid-step, a second `AdvanceRun`/`ResumeRun`/`ConfirmGate`/`CancelRun` for the *same run* could double-dispatch — two real subprocesses, two worktree mutations, two work_output writes. Store-id idempotency does **not** cover these non-idempotent side effects. So the actor keeps an `in_flight: HashSet<run_id>`:
- A `run_id` is inserted when a worker is dispatched and removed only when its `ApplyStepResult` lands (or on cancellation).
- Any mutating command for a `run_id` in `in_flight` is rejected with a typed `RunBusy` result (the UI disables the relevant buttons), except `CancelRun`/`PauseRun`, which are allowed and route to the live worker's cancellation token.

This is what *engineers* the "cooperative/resumable" claim instead of asserting it.

### 3.4 `StepInput` / `StepResult` — how store-less workers get their data

```rust
pub struct StepInput {
    pub run_id: String,
    pub stage_ix: usize, pub unit_ix: usize, pub attempt: u32,
    pub workdir: PathBuf,                 // resolved worktree (§4)
    pub clis: Vec<String>,
    pub prompt: String,                   // template + redirect amendment applied
    pub prior_evidence: Vec<EvidenceBlob>,// pre-loaded by the actor (recon map, creator output, scenario path)
    pub cancel: CancellationToken,
    pub decisions_path: PathBuf,          // absolute WICKED_DECISIONS_PATH for the gate-hook (§3.5)
}
pub struct StepResult {
    pub run_id: String, pub stage_ix: usize, pub unit_ix: usize, pub attempt: u32,
    pub outputs: Vec<WorkOutputDraft>,    // one per (unit, cli)
    pub status: StepStatus,               // Ok | Failed | Cancelled | Interrupted
    pub verdict: Option<String>,
}
```

The actor pre-loads **every** read a worker needs into `StepInput` before spawning; the worker is genuinely store-less. Large transcripts are passed by reference (path) where size matters.

### 3.5 Single-writer reconciliation for the gate-hook (resolved, not deferred)

The out-of-process `gate-hook` that claude spawns per tool-call currently calls `open_store` + `conform(&mut store)` (`inject.rs:502/522`) — a *second* OS-process writer of the same SQLite file. **Final decision:** the hook **drops the store write entirely** and appends its `ConformanceClaim` to an **append-only `decisions.ndjson` at an absolute path supplied via `WICKED_DECISIONS_PATH`** (the worker sets it; not cwd-relative, so claude changing cwd cannot misplace it — fixing `inject.rs:547`'s `current_dir` fragility). The hook still fails-closed (exit 2 on deny). The **actor** drains that ndjson into the store via `ApplyHookDecisions`, deduping by deterministic claim id, and feeds Denies into the governance gate. SQLite has exactly one writer at all times. This is proven end-to-end at **P0** before any feature is hung on it.

### 3.6 New `Core` methods (`lib.rs`)

`register_repo`, `list_repos`, `define_workflow`, `list_workflows`, `launch_run`, `advance_run`, `resume_run`, `confirm_gate`, `cancel_run`, `pause_run`, `assign_clis`, `chat_with_cli` — each a thin `send Command + await oneshot`, mirroring `launch`/`sessions` (`lib.rs:95-135`).

---

## 4. Repo registry

Nothing exists today (recon gap). Add a first-class persistent entity:

```rust
// wicked-core/src/domain.rs — mirror AgentSession's ToNode/FromNode (domain.rs:41/61)
pub const REPO_ENTRY: &str = "repo_entry";

pub struct RepoEntry {
    pub id: String,
    pub name: String,
    pub root_path: PathBuf,
    pub default_branch: String,
    pub augment_tools: Vec<String>,
    pub deny_policies: Vec<String>,
    pub registered_at: i64,
}
```

Persistence reuses `put_node`/`all_sessions` (`domain.rs:199/234`). Commands `RegisterRepo`/`ListRepos` + Core methods per §3.

**Validation + lifecycle (new).** `RegisterRepo` validates `root_path` is a git repo with ≥1 commit (git worktree requires it). On `LaunchRun` COE creates a worktree at `<repo>/.wicked/worktrees/<run_id>`; this becomes the `workdir` in `StepInput`. Worktrees are **cleaned up** on any terminal `RunStatus` (`Completed`/`Failed`/`Cancelled`) and on explicit `AbandonRun`; on actor startup an **orphan reaper** prunes `.wicked/worktrees/*` with no live run. `EntityMode::Shared|Isolated` (`scope.rs:17`) decides whether stages share one worktree (default, Shared) or get isolated ones.

**Augment mode (not hermetic).** Launched CLIs get user tools + collection tools; governance = allow-by-default + deny-policies enforced via the hook. `RepoEntry.augment_tools`/`deny_policies` feed the generated `.claude/settings.json` (`inject.rs:393`) and the additive `--mcp-config` toolbox (`inject.rs:62`), pointing the hook command at the installed `wicked-core gate-hook` binary (§3.5) with `WICKED_DECISIONS_PATH` set. **Non-claude governance is not silently assumed (see §10):** the claude PreToolUse hook path (`inject.rs:304`, `is_claude`) only governs claude. For agy/pi/gemini, P4b adds a generic deny-policy enforcement path (worktree filesystem/network restriction at the boundary COE controls + post-hoc evidence review); if a CLI supports neither a hook nor a usable sandbox, the run is marked **`UNGOVERNED`** loudly and requires explicit operator opt-in. There is no quiet governance gap.

---

## 5. Multi-CLI execution

### 5.1 Standalone vs in-combination
Supported by construction: `Worker::queue` (`worker.rs:69`) takes an arbitrary `Vec<AgenticCli>` — single-element runs standalone, multi-element runs in combination, `synthesize` handles `total=1`. The missing piece is a selection layer, `CliPolicy`:

```rust
pub enum CliPolicy {
    Fixed(Vec<String>),
    CheapestCapable,       // route via council best_for (§5.3)
    AdversarialPair,       // creator + a distinct reviewer/seat (§5.4, §2.4.1)
    Council { min: usize },
}
```

### 5.2 Per-stage / per-unit assignment
Today `distribute` picks one `assigned_cli` per unit (`distribute.rs:29`, single `Option`). For multi-CLI: change `assigned_cli` to `Vec<String>` (a schema migration touching `domain.rs` serialization, `distribute.rs`, and every reader — landed in P4a), replace the stub execute body (`execute.rs:71`) with real per-CLI dispatch, and **fix the collision**: `work_output_node` keys solely on `unit.id` (`execute.rs:210`) — it must key on `unit.id + cli`. `AssignClis` overrides assignment **for not-yet-dispatched units only** (documented; for in-flight redirect use `Cancel`/`Pause` + relaunch).

### 5.3 Cheapest-capable routing
`RankStore::best_for` (`store.rs:320`, `types.rs:308`) returns capability scores per `(cli, work_kind)` but is never read at convene time. Build a router mapping `task.criteria → work_kind` (`lib.rs:59`) → `best_for` candidates ordered by ascending cost. Cost does not exist on `AgenticCli` (`types.rs:141`); add `cost: Option<f32>` and `capabilities: Vec<String>` to `AgenticCli` + `TomlCli` (`registry.rs:27/48`). (Coarseness caveat: `work_kind_for` uses first-criterion only — tracked in §9.)

### 5.4 Adversarial pairing
`CliPolicy::AdversarialPair` extends `render_scaffold` (`dispatch.rs:22`, currently role-agnostic) to role-parameterized `dispatch(cli, task, role)`, `role ∈ {Creator, Evaluator}`, the evaluator scaffold taking the creator's `work_output` as input; `Vote` gains `critique`/`evaluated_target`. The creator≠evaluator guarantee comes from the **§2.4.1 guard/assertion**, not roster wrap-around. **Drain stdout/stderr concurrently** during the watcher loop (`dispatch.rs:112/200`, `probe.rs:188`) — verbose CLIs exceeding ~64KB otherwise block and burn the full timeout. This same drain bug shape exists in `launch_wrapped` and is fixed in **P4a**, not just here.

---

## 6. Functional testing integration

wicked-testing is Node ESM, no Rust client, no RPC. Integration is **file + bus + subprocess**, never in-process.

**Invocation.** The FunctionalTest stage runs in the run's worktree (`process.cwd()` for wicked-testing's project-local `.wicked-testing/`). COE shells out to the host CLI that hosts the skill (the only sanctioned headless path — there is no `node` runner):
```
claude -p '/wicked-testing:acceptance scenarios/<unit>.md --json'   (cwd = run worktree)
```
This runs Writer→Executor→Reviewer inside the host CLI. The scenario `.md` is the input contract (`SCENARIO-FORMAT.md`); the Recon/Build typed evidence (§2.5) feeds scenario authoring — which is itself an LLM/CLI invocation, scoped explicitly into P6 (not folded silently).

**Run-id correlation (the crux, now specified).** The manifest lives at `.wicked-testing/evidence/<testing-run-id>/manifest.json`, where `<testing-run-id>` is wicked-testing's own id, *not* the COE `run_id`. COE **captures `testing-run-id` from the `--json` stdout** of the acceptance invocation and stores it on `Run.testing_run_id`. That id both locates the manifest dir and **filters the global bus** (`wicked.test.verdict.created` is not per-run) so concurrent COE runs don't cross-wire.

**Verdict flow.** Two sanctioned read seams:
1. **Manifest file** — parse against `schemas/evidence.json`; gate on `manifest_version`, ignore unknown keys; map `status`/`verdict.value` (PASS/FAIL/PARTIAL/CONDITIONAL/INCONCLUSIVE) into a `work_output` node.
2. **Bus events** — subscribe via the `wicked-bus` CLI, **filtered by `testing_run_id`**, to drive the live stream without polling.

COE re-emits the parsed verdict as `CoreEvent::VerdictRecorded`; the stage's `HumanConfirmIf(verdict != PASS)` gate decides pass-through vs human pause. `PASS` auto-approves the governance phase; anything else pauses for human review.

---

## 7. UI surface

### 7.1 Migration onto Core
The egui dashboard (today file-polling + binary-shelling) is rebuilt to hold a `Core` handle: drop file-polling, use `Core::subscribe` for the live `CoreEvent` stream and the read API (`sessions_detail`/`work_output`) for snapshots. egui's immediate-mode loop reads a shared event-buffer the subscriber thread fills. **Chat is brought into the engine** via `ChatWithCli`/`StreamChat` (no more direct UI shell-out, `data.rs ChatSession::start_claude`) — satisfying "one engine, many surfaces" for all five capabilities and enabling chat against a running run's CLI/worktree.

### 7.2 New views
- **Repo Registry** — list `RepoEntry`s, "Register repo" form (path picker, default branch, augment tools, deny-policy selection), per-repo run history.
- **Interactive Workflow Runner** (centerpiece):
  - **Stage timeline** — horizontal `StageSpec` strip driven by the cursor; status colored by `RunStatus`.
  - **Human-confirm gate prompt** — on `AwaitingHuman`, render the stage `prompt` with **Approve / Reject / Retry** buttons *plus an amend/inject text field* (the redirect payload, §2.3 step 4) wired to `Core::confirm_gate`. Retry is backed by the real `Rejected → InProgress` edge (P2), bounded by `StageSpec.retries`.
  - **Cancel / Pause** controls — wired to `CancelRun`/`PauseRun`; enabled even while a run is `in_flight` (the only mutating commands allowed during a busy run).
  - **Live output** — `CliOutputDelta` stream rendered as a scrolling per-CLI terminal pane (inspect mid-run), plus a worktree file panel for the run's `.wicked/worktrees/<run_id>`.
  - **Per-stage CLI panel** — assigned CLIs per unit with an override dropdown calling `AssignClis` (clearly labelled "applies to not-yet-started units"); surfaces `CliPolicy` (cheapest-capable choice + `best_for` score, adversarial pair).
  - **Run-filtered event stream** — totally-ordered by `seq`, filtered by `run_id`.
- **Recon / Adversarial / Functional panels** — Recon shows the typed map; Adversarial shows creator output vs reviewer critique side-by-side with the distinct evaluator identity; Functional embeds the wicked-testing manifest (verdict, artifacts[], duration).
- **Chat** — warm-claude + oneshot, now routed through `Core`.

---

## 8. Phased plan

Dependency-aware; each phase ~1 session, each with a *real* proving test. The methodology spine is no longer entirely back-loaded: a real, cancellable wrapped-CLI stage (P4a) lands before the full UI, and a thin interactive gate slice is exercised in P2.

> **BUILD STATUS (live).** **P0 · P1 · P1.5 · P2 · P3 · P4a + the operator CLI + P7 (dashboard CONTROL PLANE) all DONE+green.** Engine: **37 tests + clippy `-D warnings` clean**. Dashboard (`wicked-agent-ui`): **17 tests + 0 warnings**. The engine is functional end-to-end (CLI: `register-repo` → `run --repo <id> --confirm …` runs a REAL governed CLI in an isolated worktree with stdin gates) AND via the **egui dashboard** (launch/observe/gate-approve-reject-amend/cancel/register-repo/deny-policy, all through a testable `Dashboard` controller on the live `CoreEvent` stream). A `--demo` auto-drive verifies the full dashboard→engine flow (`demo-run [Completed] 2/2`). NEXT: **P5/P6** (adversarial-review + functional-test stages) — which also unlock the §7 dashboard OBSERVABILITY panels (live CLI output, per-stage council, recon/adversarial/functional, chat-through-Core) currently deferred.
>
> **P7 (egui dashboard on Core) — DONE+green.** Repointed `wicked-agent-ui` off the retired `wicked-agent` crate onto `wicked-core`; egui 0.29→0.30. New testable controller `src/dashboard.rs::Dashboard` owns the `Core` handle + a view-model + a live log: store-authoritative `reload()` + event-driven `on_event()` with a self-healing gate-prompt cache (survives reconciles); commands launch/confirm_gate/cancel/resume/advance/register_repo/register_deny_policy. `App` is pump+draw over it: interactive **gate bar** (Approve/Reject/Cancel + amend), live-events log, repos view, governance via `Core::register_deny_policy` (real, single-writer — added this phase), kill→`dash.cancel`. Selection tracked by **run identity** (the review caught a positional-index bug where actions fired on the wrong run). Tests: 9 controller integration tests vs a real `Core` + 2 mapping tests. egui_kittest frame-rendering / automated video are infeasible in this env (no headless GPU adapter, no screen-record permission) — a `--demo` mode + `scripts/record-demo.sh` let the operator record locally.
>
> **P4a (real wrapped-CLI execution) — DONE+green.** `src/execute_wrapped.rs::WrappedCliStepRunner` resolves each unit's assigned CLI → invocation (council registry, key-as-binary fallback), runs it as a no-shell subprocess (`--` guarded prompt) in `StepInput.workdir`, drains stdout/stderr CONCURRENTLY (no 64KB pipe-buffer deadlock) under a timeout, maps exit→`StepStatus`. `Core::spawn` injects it (stub stays for `spawn_with_engine` tests). Proof: `tests/p4a_wrapped.rs` (echo runs in the worktree → real stdout governed → persisted → Completed). The operator CLI (`src/bin/wicked-core.rs`) now exposes `repos`/`register-repo`/`run`/`resume`/`cancel`. **P4b deferred:** the per-tool-call gate-hook re-home (read-only/`busy_timeout`) + the `advance()`-sole-opener unification are preconditions for *governed* tool-calls; P4a runs augment-mode unit-level governance for now.
>
> **P2 (interactive gates) — DONE+green, hardened after its adversarial review.** Human-confirm gates (`HumanConfirm::None|All|Before(ord)`) pause BEFORE a unit (`SessionStatus::AwaitingHuman`), `confirm_gate(Approve{amend}|Reject)` resumes/cancels (amend = steering redirect), `cancel_run` terminates; implemented at the **COE level** (run lifecycle = `AgentSession.status`) NOT in the published orchestration reducer — so the design's old "clearable veto / attempt-scoped event id in `apply_event`" language (§2.3 step 3) is **superseded** by the actor's session-status machine.
> - **RUN-LEVEL DENY CONTRACT (decided here, was the open charter item):** a run is `Completed` only if EVERY unit was governance-approved and ran without worker failure. A governance `Deny` (unit `Rejected`) OR a `StepStatus::Failed` worker **halts the run as `SessionStatus::Failed`** — never advancing past a rejection. Operator `cancel_run` / gate `Reject` / a `StepStatus::Cancelled` worker → `Cancelled`. Tested in `tests/p2_contract.rs` (deny-through-engine, worker-fail, worker-cancel-no-wedge, cancel-in-flight).
> - **RETRY: DESCOPED (was a P2 deliverable; removed in writing).** `HumanDecision` has no `Retry`; there is no automatic `Rejected→InProgress` edge. Recovery from a `Failed` run is **operator relaunch/resume**, which is sufficient for a functional system and avoids editing the published orchestration crate. The design's earlier retry-edge anti-regression line no longer applies.
> - **P3 (repo registry) — DONE+green**, built in parallel: `RepoEntry` node-kind + `register_repo`/`list_repos` (validates git repo + ≥1 commit); a run targeting a repo gets an isolated worktree at `<repo>/.wicked/worktrees/<run_id>`, passed to the worker as `StepInput.workdir`; COMPLETED runs keep their worktree (review/merge), CANCELLED discard it; startup orphan reaper. Tests in `tests/p3_repo.rs`.
>
> **Revised critical path (post-P1 reassessment, see `REASSESS-P0-P1.md`):** `P1.5 → P2 → P4a → {P5 ∥ P6}`, with **P3 ∥ P2**, **P4b ∥ P4a**, **P7 last**. P3 (repo registry, additive) is independent of the actor protocol and can build concurrently in a git worktree.
>
> **P1.5 — hardening (DONE+green), inserted after the P0+P1 adversarial review caught a critical bug:**
> - *Critical:* the actor held its own `self_tx`, so the command channel never closed → the actor never terminated (leaking a thread + writable store per spawn, and letting two writable actors share one db). Fixed with `Command::Shutdown` + a handle-count `ShutdownGuard` (fires when the last `Core` drops). Proven by `actor_shuts_down_when_last_core_drops`.
> - *High:* `apply_step_result` now has an idempotency/cursor guard (ignores a stale/duplicate result whose `unit_ix` ≠ the cursor, or whose unit is already `Done`) → `StepApplied::{Continuing,Finished,Stale}`.
> - `finalize_run` now propagates store-write errors (no silent `RunBusy` wedge); `run_finished` store-re-read removed (control-flow driven).
> - Honesty fixes: gate-hook doc no longer claims "read-only" (the RW open + missing `busy_timeout` is recorded as a **hard P4b precondition**); the off-thread claim is scoped to the EXECUTE phase (distribute is still synchronous — moving it off-thread is P5); P0 test now proves "the hook did NOT write the claim (only the actor did)" pre-drain; the resume test proves it ran ONLY the remaining unit.
>
> **Hard preconditions carried forward (do NOT skip):** **P4a** must first do the §2.1 unification (`advance()` sole phase opener, delete `tick_workflow`, make execute phase-pure) — deferred from P1, safe only because `advance()` is currently unused. **P4b** must make the gate-hook open read-only or set `busy_timeout`. **P2** must decide the run-level deny contract (a `Rejected` unit currently still yields a `Completed` run) and add `StepStatus` to `StepOutput`.

### P0 — Single-writer reconciliation + phase-ownership decision **(BUILD FIRST — de-risks the P1 premise)**
- **Goal:** prove the gate-hook/single-writer story end-to-end and lock who-opens-phases, before any actor-protocol work.
- **Deliverables:** change `run_gate_hook` (`inject.rs:496`) to append to an absolute `WICKED_DECISIONS_PATH` ndjson and **drop the `conform(&mut store)` store write** (`inject.rs:522`); add `Command::ApplyHookDecisions` + actor drain (dedup by deterministic claim id); write the phase-ownership decision (`advance()` is sole opener; execute backend is phase-pure).
- **Crates:** wicked-core, (wicked-agent code being absorbed).
- **Test:** run the gate-hook as a **separate OS process** appending Denies/Allows to a temp absolute ndjson while the actor holds the store open — assert **no SQLite busy error**; then the actor drains the file and the claim appears in the store **exactly once** (idempotent on re-drain), and a Deny vetoes the governance gate.
- **Risk:** Medium — small surface, highest leverage; turns a P4 surprise into a P0 decision.

### P1 — Re-entrant engine + actor-off-thread execution + in-flight guard
- **Goal:** `run_session` → resumable `step(run)` driven by one cursor + `advance()`, with slow work off-thread and a per-run concurrency guard.
- **Deliverables:** `workflow.rs` (`Cursor`, `Run`, `ExecPhase`, `StepInput`, `StepResult`); split `pipeline.rs:36` into per-step; bounded worker pool + `Sender<Command>` clone for write-back + `ApplyStepResult`; `in_flight: HashSet<run_id>` + `RunBusy`; `advance()` as sole phase opener (strip `Phase::open` from the execute path); delete `tick_workflow`; single-emit-point `seq`+`run_id` stamping; extend `AgentSession` with cursor fields.
- **Crates:** wicked-core, wicked-orchestration.
- **Test:** with a **deliberately-blocking fake step** (sleeps on a channel), assert the actor services a `Sessions` read **while the step is in flight** (proves the off-thread split) and that a concurrent `AdvanceRun` for the same run returns `RunBusy` (proves the guard); separately, construct a persisted mid-run cursor in a **fresh Core** and assert `ResumeRun` continues to completion (explicitly proves resume-from-cursor, not crash-during-subprocess).
- **Risk:** Medium-High — actor protocol + concurrency; stub backend isolates it from subprocess complexity.

### P2 — Typed stages + CLEARABLE human-confirm gates + retry + redirect/cancel
- **Goal:** wire the dead `HumanConfirm`/`AwaitingHuman` path correctly (clearable, releasable), add retry edge, redirect payload, and intervention commands; surface ONE gate prompt in the existing UI as a thin interactivity proof.
- **Deliverables:** `WorkflowDef`/`StageSpec`/`GateSpec` + JSON-in-second-slot encoding; `AwaitingConfirmation` `PhaseStatus` + edges (`transitions.rs:21`) + **new `advance()` arm** (`runner.rs:210`); **clearable** `Event.human_decision` + second structural veto clause in `apply_event` (parallel to `reducer.rs:170`, but released on Confirm); `Rejected → InProgress` retry edge bounded by `StageSpec.retries` with attempt-scoped event ids; `DefineWorkflow` (with §2.4.1 validation) / `ConfirmGate` (with redirect payload) / `CancelRun` / `PauseRun` + events; retire session-level `HumanConfirm` (default-def shim).
- **Crates:** wicked-core, wicked-orchestration.
- **Test:** a 2-stage workflow with `HumanConfirm` after stage 0 pauses (`AwaitingHuman` emitted, status persisted, approving transition vetoed while pending); `confirm_gate(Approve)` **clears** the marker and resumes to stage 1; `confirm_gate(Reject)` then `Retry` re-enters stage 0 with `attempt=1` (not deduped); `CancelRun` mid-stage terminates the run.
- **Risk:** Medium — touches the safety-critical approval invariant (scoped as such, not "mostly wiring").

### P3 — Repo registry + worktree isolation + lifecycle
- **Goal:** first-class repos; runs target a registered repo via git worktree, with validation and cleanup.
- **Deliverables:** `RepoEntry` + node-kind + put/get/list; `RegisterRepo`/`ListRepos` (validate git repo + ≥1 commit); worktree create/cleanup + startup orphan reaper; `RunSpec.repo_ref → workdir`.
- **Crates:** wicked-core.
- **Test:** register a repo, launch a run → worktree exists at `<repo>/.wicked/worktrees/<run_id>` and writes land there; on terminal status the worktree is removed; a stale orphan dir is reaped on startup; registering a non-git path fails.
- **Risk:** Low — additive entity + git plumbing.

### P4a — Real drain-safe wrapped-CLI execute backend + multi-CLI schema
- **Goal:** replace the stub (`execute.rs:71`) with real governed subprocess execution — **the first real methodology stage (Recon/Build)**.
- **Deliverables:** port `execute_unit_wrapped` (`wicked-agent/execute.rs:273`) + `inject.rs` (`launch_wrapped`, `write_claude_settings`, `write_mcp_config`) with **concurrent stdout/stderr drain** (fixing the `dispatch.rs:200`-shape pipe-buffer hang in `launch_wrapped`); `CliOutputDelta` streaming; `assigned_cli: Vec<String>` migration + `work_output_node` keyed on `unit.id+cli`; crash-attempt checkpoint + completion sentinel (§2.6).
- **Crates:** wicked-core (absorbs wicked-agent), wicked-governance (consumed).
- **Test:** run a real CLI in a worktree, write a file, stream deltas; assert a deny policy blocks a denied tool-call (exit 2) and the run is cancellable mid-execution; a simulated crash-after-subprocess resume detects the sentinel and does **not** re-run.
- **Risk:** High — subprocess + cwd + drain; isolated from the hook by P0/P4b split.

### P4b — Gate-hook re-home + non-claude governance
- **Goal:** ship the `wicked-core gate-hook` binary and the governed-for-all-CLIs story.
- **Deliverables:** `wicked-core gate-hook` subcommand (porting `run_gate_hook`, ndjson-only per P0); installer/path resolution so `.claude/settings.json` hook command resolves to a real installed path; `WICKED_DECISIONS_PATH` wiring from worker; generic non-claude deny enforcement OR loud `UNGOVERNED` marking with operator opt-in.
- **Crates:** wicked-core.
- **Test:** a real `claude -p` run records a claim via the installed hook into the absolute ndjson (correct even if claude changes cwd), the actor ingests it, and a deny blocks; a non-claude CLI run either enforces a deny or is flagged `UNGOVERNED` (never silently allowed).
- **Risk:** High — cross-binary install + governance generalization.

### P5 — Council adversarial review + cheapest-capable routing
- **Goal:** AdversarialReview stage with enforced evaluator≠creator + CLI routing.
- **Deliverables:** role-parameterized `render_scaffold`/`dispatch`; `Vote.critique`; creator→evaluator pass in `run_council`; §2.4.1 guard/assertion (incl. standalone distinct-seat); `cost`/`capabilities` on `AgenticCli`+`TomlCli`; `best_for`-driven router; `CliPolicy`.
- **Crates:** wicked-council, wicked-core.
- **Test:** an AdversarialReview stage where evaluator seat ≠ creator seat (including a single-binary standalone case), the evaluator scaffold receives the creator output, a distinct evaluator-identity claim is recorded, and a same-cli-no-distinct-seat config is **rejected at DefineWorkflow**; router picks the cheapest capable seat.
- **Risk:** Medium.

### P6 — Functional testing stage (wicked-testing)
- **Goal:** FunctionalTest stage end-to-end with run-id correlation.
- **Deliverables:** scenario authoring from typed prior-stage evidence; subprocess `claude -p '/wicked-testing:acceptance ... --json'` in worktree; **capture `testing_run_id` from `--json` stdout**; manifest parser (gate on `manifest_version`); `wicked-bus` subscribe filtered by `testing_run_id`; verdict → work_output + `VerdictRecorded`; `HumanConfirmIf(verdict != PASS)`.
- **Crates:** wicked-core (+ wicked-testing consumed).
- **Test:** a FunctionalTest stage runs a known scenario, COE locates the correct manifest via `testing_run_id`, a PASS auto-approves the gate, a FAIL pauses for human review; a second concurrent run does not cross-wire verdicts.
- **Risk:** Medium — cross-language, correlation, host-CLI dependency.

### P7 — egui dashboard on Core (+ chat into Core)
- **Goal:** migrate UI off file-polling onto `Core`; ship the centerpiece views; route chat through the engine.
- **Deliverables:** `Core` handle in egui; subscriber→event-buffer; Repo Registry, Workflow Runner (timeline + gate prompts with amend/inject + cancel/pause + live `CliOutputDelta` terminal + worktree file panel + per-stage CLI), recon/adversarial/functional panels; `ChatWithCli` routed through Core.
- **Crates:** wicked-agent-ui (→ wicked-core-ui), wicked-core.
- **Test:** drive a full workflow from the GUI: register repo → launch → inspect live output → cancel a stage → relaunch → approve a gate with an injected amendment → see verdict, all via `Command`s (no file polling, no shelling, chat included).
- **Risk:** Medium — egui immediate-mode + async event plumbing.

---

## 9. Open decisions

**Resolved by the operator (2026-06-28) — LOCKED:**
1. **Worktree scope → SHARED per-run.** ✅ One worktree per run; Recon→Build→Review→Test share the evolving tree. `EntityMode::Isolated` remains an opt-in `RunSpec` knob. Crash re-run risk mitigated by the completion-sentinel check (§2.6).
2. **wicked-testing invocation → HOST-CLI SUBPROCESS.** ✅ `claude -p /wicked-testing:acceptance`, preserving writer→executor→reviewer isolation (anti-self-grading). Revisit only if wicked-testing ships a headless bin.
3. **Non-claude governance → LOUD `UNGOVERNED` OPT-IN.** ✅ claude governed via PreToolUse hook; agy/pi/gemini run as executors only under an explicit, clearly-flagged ungoverned opt-in. No silent gap (P4b). (Generic sandbox enforcement deferred, not chosen.)
4. **Default workflow template → `Recon → Build → AdversarialReview → FunctionalTest`.** ✅ Human-confirm gates after Recon (approve plan) and after AdversarialReview (approve before test); FunctionalTest auto-gates unless verdict != PASS. This is the `WorkflowDef` COE synthesizes when no `def_id` is supplied (§3.1).

**Internal tuning (defaults accepted; revisit if needed):**
5. **`work_kind` coarseness.** `work_kind_for` uses first-criterion only (`lib.rs:59`); the cheapest-capable router inherits this. A richer multi-criterion mapping is a follow-up (blocks nothing).
6. **Pool sizing / backpressure.** Bounded worker pool (default `num_cpus`) + bounded result channel; exact sizing + overflow policy (reject vs queue) tunable post-P1.

---

## 10. Critique resolutions

| Critical flaw raised | Resolution in final design |
|---|---|
| Off-thread fix introduces a concurrency hazard (double-dispatch of the same run) | §3.3 `in_flight: HashSet<run_id>` guard; mutating commands for a busy run return `RunBusy`; only `Cancel`/`Pause` allowed during in-flight. Built in P1; tested by the concurrent-`AdvanceRun` assertion. |
| Human-confirm modeled on sticky `gate_decision` would never release on approve | §2.3 step 3: a **clearable** `human_decision` marker — vetoes while `Pending`, *cleared/released* on Confirm — explicitly not the monotonic `gate_decision` shape. |
| `advance()` reuse breaks: `AwaitingConfirmation` falls into `other => Waiting` | §2.3 + P2: new explicit arm in `advance()` (`runner.rs:210`) returning a real `AwaitingHuman` outcome, plus `ALLOWED_TRANSITIONS` edges. |
| `phases` backward-compat overstated (changing tuple arity breaks deserialize) | §2.2: `StageSpec` JSON-encoded **into the existing second `String` slot**; tuple arity unchanged; first slot stays `select_key`. Genuinely back-compat. |
| Dual cursor / who-opens-phases | §2.1: one integer (`stage_ix` = projection of `Workflow.current_index`), one driver (`advance()`, delete `tick_workflow`), one phase opener (`advance()`; execute backend made phase-pure). Pulled into P1. |
| Single-writer contradicted by out-of-process gate-hook (two SQLite writers) | §3.5 + **P0**: hook drops the store write, appends ndjson at absolute `WICKED_DECISIONS_PATH`; actor drains via `ApplyHookDecisions`. One writer always. Proven at P0. |
| "Interactive" is just a confirm button — no cancel/intervene mid-run | §2.6/§3.1: `CancelRun`/`PauseRun` + child-process handle + cancellation token; allowed during in-flight runs. |
| Human cannot redirect — gates are bless-or-bounce | §2.3 step 4 / §7.2: `ConfirmGate` carries an optional amend/inject payload; gate becomes steering. |
| evaluator≠creator silently breaks in standalone (`next_cli_in_roster` wraps) | §2.4.1: validate at DefineWorkflow/LaunchRun (≥2 CLIs or distinct seat), assert at runtime; never rely on roster wrap-around. |
| Chat orphaned from the engine (UI shell-out) | §3.1/§7.1: `ChatWithCli` routed through Core. |
| Methodology spine sequenced last (3 phases of stubbed gates) | §8 re-sequence: real wrapped-CLI stage at P4a before the full UI; thin interactive gate slice surfaced in P2. |
| P1 proving test is theater on the instant stub | P1 test uses a deliberately-blocking fake step + a fresh-Core mid-run cursor; documented as resume-from-cursor, not crash-during-subprocess. |
| Crash resume re-runs real subprocesses (not idempotent) | §2.6: attempt checkpoint + completion sentinel; interrupted-with-no-sentinel surfaces `AwaitingHuman` instead of silently re-running. **Accepted residual risk, surfaced.** |
| Governance only enforced for claude | §4 + P4b: generic non-claude deny path or loud `UNGOVERNED` opt-in; no silent gap. |
| Event ordering nondeterministic once multi-producer | §3.2: workers never emit; actor is the single emit point and stamps monotonic `seq`+`run_id`. |
| Retry swallowed by deterministic-id dedup | §2.6: `cursor.attempt` folded into all event ids. |
| Two overlapping gate models (session `HumanConfirm` vs per-stage `GateSpec`) | §3.1: session-level `HumanConfirm` retired; per-stage `GateSpec` is sole authority; default-def shim translates legacy values. |
| Per-stage governance "already selectable" is false (`unit-{ord}`) | §2.3 step 2: `select_key` derived from `StageKind` + explicit stage→policy map (net-new, owned in core). |
| Orchestration lane-disjointness leaked | §2.2 layering: methodology types in core; orchestration gets only generic `AwaitingConfirmation` + clearable clause + attempt ids; no `StageKind`/governance dep. |
| Workers need store reads but have "no store handle" | §3.4: `StepInput` pre-loaded by the actor (prior evidence, units, scenario path). |
| Actor needs a `Sender<Command>` clone; backpressure | §3.3 + §9.4: actor holds a `tx` clone; bounded pool + bounded result channel. |
| P4 is secretly 3+ phases | §8: split into P4a (drain-safe subprocess + multi-CLI schema) and P4b (gate-hook re-home + non-claude governance). |
| P6 run-id correlation unspecified | §6: capture `testing_run_id` from `--json` stdout; locate manifest + filter bus by it. |
| Retry edge orphaned (only an open decision; P7 button unbacked) | §2.6 + P2: `Rejected → InProgress` edge built in P2. |
| `decisions.ndjson` cwd-relative fragility | §3.5: absolute `WICKED_DECISIONS_PATH` set by the worker; cwd-independent. |
| Pipe-buffer drain flagged only for P5 | §5.4 + P4a: concurrent drain added to `launch_wrapped` in P4a. |
| Worktree lifecycle / validation missing | §4 + P3: git-repo validation at register, cleanup on terminal status, startup orphan reaper. |
| No live partial-output streaming | §3.2: `CliOutputDelta` event (drain is required anyway). |
| `AssignClis` timing inert mid-run | §5.2: documented to apply only to not-yet-dispatched units; mid-run redirect via Cancel/Pause + relaunch. |
| Inter-stage contract / `EvidenceSpec` hand-waved | §2.5: typed `EvidenceSpec`/`EvidenceRef` + concrete Recon schema; actor validates produce/consume. |

---

## 11. First slice — concrete task list (P0 only)

The first buildable phase is **P0 — single-writer reconciliation + phase-ownership decision**. Implementation starts here.

**Crate:** wicked-core (absorbing the relevant wicked-agent gate-hook code).

1. **`wicked-core/src/gate_hook.rs` (new module; port of `wicked-agent/src/inject.rs:496 run_gate_hook`).**
   - Add `pub fn run_gate_hook(payload: HookPayload) -> ExitCode`.
   - **Remove** the `open_store` + `conform(&mut store)` calls (the `inject.rs:502/522` lines being ported) — the hook must not touch SQLite.
   - Read the decisions output path from `std::env::var("WICKED_DECISIONS_PATH")` (absolute); if unset, fail-closed (exit 2) rather than falling back to cwd (`inject.rs:547`).
   - Compute the deterministic claim id `claim_id = hash(run_id, tool_call_id, attempt)` and **append** one NDJSON line `{claim_id, run_id, tool, decision, ts}` to that path (open with append; create if missing).
   - Preserve fail-closed semantics: on `Deny`, write the line then `exit(2)`; on allow, write and `exit(0)`.

2. **`wicked-core/src/bin/wicked-core.rs` — add the `gate-hook` subcommand.**
   - Add a `gate-hook` arm that reads the hook JSON from stdin, deserializes `HookPayload`, calls `gate_hook::run_gate_hook`, and returns its `ExitCode`. (Path-install wiring into `.claude/settings.json` is deferred to P4b; P0 invokes the binary directly.)

3. **`wicked-core/src/command.rs` — add the drain command.**
   - Add `ApplyHookDecisions { run_id: String, ndjson_path: PathBuf }` to the `Command` enum (`command.rs:10`).

4. **`wicked-core/src/actor.rs` — handle the drain (single-writer ingest).**
   - In the `recv` match, add an arm for `ApplyHookDecisions { run_id, ndjson_path }`:
     - Read the NDJSON file line-by-line, parse each `HookDecision`.
     - For each, build the `ConformanceClaim` and apply it via the existing gate path (`apply_gate` / `apply_event`, `gate.rs:78` / `reducer.rs`), **deduping by `claim_id`** (the reducer already dedups by deterministic `event.id`, `reducer.rs:152` — map `claim_id` onto the event id).
     - A `Deny` line drives the governance gate veto for the run's current phase.
   - This is the only place these claims hit the store — single writer preserved.

5. **`wicked-core/src/lib.rs` — thin Core method.**
   - Add `pub async fn apply_hook_decisions(&self, run_id, ndjson_path) -> Result<()>` mirroring `launch`/`sessions` (`lib.rs:95-135`): send `Command::ApplyHookDecisions`, await oneshot.

6. **Phase-ownership decision (written artifact + code marker).**
   - Add a module doc comment in `wicked-core/src/workflow.rs` (stub for now) stating: *`advance()` is the sole opener of orchestration phases; the execute backend is phase-pure and must never call `Phase::open`.* (Enforced in P1; recorded here so P1 starts from the decision.)

**Proving test** (`wicked-core/tests/p0_single_writer.rs`):
- **(a) No second writer.** Build a `Core` (actor holds the store open). In a **separate OS process** (`std::process::Command` invoking the `wicked-core gate-hook` binary with `WICKED_DECISIONS_PATH=<tempdir>/decisions.ndjson` and a piped hook payload), write one Allow and one Deny line. Assert the process exits (0 then 2) and the actor performs a normal store read concurrently with **no SQLite busy/locked error**.
- **(b) Idempotent drain.** Call `core.apply_hook_decisions(run_id, path)` twice; assert the claim appears in the store **exactly once** (dedup by `claim_id`).
- **(c) Deny vetoes.** Assert the drained `Deny` causes the run's governance gate to be non-approving (the veto path fires).

This slice proves the §1 single-writer invariant survives a real out-of-process gate-hook and fixes the cwd-relative decisions fragility — turning the original design's biggest latent P4 contradiction into a settled precondition before the actor protocol (P1) is touched.