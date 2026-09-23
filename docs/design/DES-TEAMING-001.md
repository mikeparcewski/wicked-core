# DES-TEAMING-001 — Real-time teaming: monitors on the live unit, advice into the worker, the gate decides

- **Status:** PROPOSED (rev 2). **S6 is BLOCKED on an operator decision (§6.7).** S2 and S3 are not blocked.
- **Rev 2 (2026-09-23):** moved from `.product/` (untracked since #562) to `docs/design/`; §6.7 records the HIGH-dispute escalation as a blocking decision with two exact options; §6.2 extends monitor exclusion to the bus-mediated judge (`GateEvalRequest.excluded_seats`). Both come from review on #604.
- **Date:** 2026-09-23
- **Scope:** wicked-core (S2 monitor subscription, S3 monitor→worker injection, S6 gate adjudication — engine half), wicked-crew (S6 read route + api-types), wicked-studio (S6 surfaces)
- **Related:** #590 (the operator-approved proposal), #599 (S1, merged as `d5d9708`: the `AskUserQuestion` elicitation channel), #595 (seat-instance keys, `claude#2`), S4 (complexity policy, `feat/590-s4-review-scale`), S5 (deterministic `RoutingInfo::Teamed` + the decision-council entry point) — S4 and S5 are built elsewhere; this document names only the interface it consumes from them (§8).
- **Evidence base:** every `file:line` below is `origin/main` at `d5d9708` unless it names another repo. Adapter citations are the version crew locks: `@agentclientprotocol/claude-agent-acp@0.73.0` (`wicked-crew/package-lock.json`, `"version": "0.73.0"`), file `dist/acp-agent.js`.

## 1. Problem

#590 measured a per-phase council that contributed nothing to the verdict (run `aee254f1`: 12 `councilSeatFailed`, `agentVerdict: "skipped"`, 4/4 gates passed on the floor + evaluator). The HIGH defect in that run was visible the moment the handler was written and survived to a PR. The proposal is teaming: peers watch the unit while it works, advise the worker, the worker may refuse with evidence, and **the gate is the only decider**.

The council survives in exactly one role (operator scope correction, 2026-09-23): a **one-off council on one concrete disputed decision**. It is not a routing mode and there is no mode selector. §6.3 defines when a dispute reaches it, what it is handed, and how its verdict reaches the gate as evidence.

## 2. What exists today (verified at the cited lines)

| Fact | Evidence |
|---|---|
| The production daemon runs units on the ACP runner. crew's napi `Core::spawn` calls `Core::spawn`, which is `spawn_with_acp_sessions`. | `crates/wicked-core-ts/src/lib.rs:705-710`; `src/lib.rs:361-364`, `:545` |
| Governed claude units run on ACP now, answered through `session/request_permission`. | `src/acp_runner.rs:6724-6729` |
| Only the claude seat is ACP-admitted to input governance. agy, codex, pi and copilot are not. | `crates/wicked-council/src/registry.rs:154` (`true`); `:209`, `:300`, `:374`, `:424` (`false`) |
| **No live tool-call event exists on the ACP path.** `handle_update` records `tool_call`/`tool_call_update` into local accumulators and emits nothing. `ToolInvoked` fires after the unit and is absent for ACP. | `src/acp_runner.rs:4860-4895`; `src/event.rs:393-398` |
| `GovernanceHookFired` is replayed **at gate time, post-fold**, not live. | `src/event.rs:586-592` |
| `UnitOutputDelta` is prose: coalesced at most every 500 ms / 2 KB. | `src/event.rs:246-253` |
| The ACP turn loop dispatches agent frames by `method` (`session/update` at `:4385`, `session/request_permission` at `:4400`). It ignores a response to any id other than its own prompt's. | `src/acp_runner.rs:4383-4431`, `:4470` |
| The `initialize` result is captured, but only `authMethods` is read from it. | `src/acp_runner.rs:2645-2657` |
| The operator inject bar: on ACP, messages queue per `(run, cli)` and ride the **next unit's** prompt. On PTY, bytes go to the terminal's stdin. | `src/actor.rs:2588-2690`; `src/acp_runner.rs:6412-6445`, `:6665-6684`, `:7644-7658` |
| The adapter implements a mid-turn steering request `_session/steering`, advertised as `InitializeResponse._meta.steering.supported`. | adapter `acp-agent.js:98-103`, `:853-859`, handler `:1186-1272`, registration `:7503` |
| Steering on a turn in flight **injects** into it. On an idle session, by default it **starts a new detached turn**. The opt-in `_meta.steering.idleBehavior: "promptRequired"` returns `{outcome:"promptRequired"}` and does nothing. | adapter `acp-agent.js:1228-1242`, `:1271` |
| Steering priority is the adapter's choice: `now` (pre-empts the current generation), or `later` while a permission or elicitation is pending. | adapter `acp-agent.js:1196-1206`, `:1254-1255` |
| A tool call's terminal frame is `tool_call_update` with `status: "completed" \| "failed"` and `_meta.claudeCode.toolName`. `kind`/`title`/`locations` ride the earlier `tool_call` and refinement frames. | adapter `acp-agent.js:7280-7290`, `:6902-6950` |
| Wrapped CLIs run with `stdin` = null. The per-unit settings file arms only a `PreToolUse` hook. | `src/execute_wrapped.rs:3468`, `:2793-2798` |
| The PTY runner ends a turn at the **first** `result` sentinel. `run_sessions` holds PTY sessions only. | `src/session_runner.rs:610-632`; `src/actor.rs:2257-2275` |
| A read-only chat boundary: the scratch root is writable, the scoped roots are read-only, everything else is denied. It is judged per permission request. | `src/acp_permission.rs:420-429`; wired at `src/acp_runner.rs:4731`, `:5920` |
| A worktree content snapshot through a scratch index (it never touches the worktree's index). The dispatch baseline is persisted on every bound agent unit. | `src/worktree_guard.rs:385-442`, `:445`; `src/actor.rs:6517-6545`; `src/domain.rs:552` |
| The worker-thread seam where a unit runs, is judged and yields evidence. It is shared by the in-process and bus paths. | `src/cli_runner.rs:748` (fn), `:761` (run), `:871` (guard first look), `:876-896` (`work_for_agent`), `:988` (judge); callers `src/actor.rs:7083`, `src/cli_runner.rs:2013` |
| The judge prompt treats the WORK fence as untrusted data. | `src/validator.rs:1656-1658` |
| A creator re-run after `request_changes` receives the amendment as prior context. | `src/actor.rs:6673-6700` |
| The engine gate emits `GateEvaluated` → `GateDecided` → `UnitDone`/`UnitDenied`. | `src/pipeline.rs:1540-1576` |
| crew relays every CoreEvent to `/ws` with no allowlist. Human gate: `GET`/`POST /runs/:id/gate`. | `wicked-crew packages/crew/src/api/server.ts:1202-1235`; `routes.ts:2942-2943`, `:3206` |
| Studio: the pinned approval dock hosts the gate. `VerdictDetail` renders the engine gate. | `wicked-studio src/components/ApprovalDock.tsx`, `SteeringGate.tsx:85`, `VerdictDetail.tsx:90`; `src/api/client.ts:213-217`, `:294` |

Two consequences shape the design. First, a monitor cannot be driven off an existing live tool-call event: there is none (row 4), and the only per-tool-call event fires after the fold (row 5). S2 adds one. Second, exactly one carrier can accept content mid-turn: the ACP claude adapter's `_session/steering` (§5.1).

## 3. Shape in one paragraph

A worker unit runs on its carrier. Each time a tool call finishes, the ACP carrier emits `unitCheckpoint`. A per-daemon **TeamSupervisor** (core) subscribes to the engine's event fan-out. At most once per batch interval it snapshots the worktree, diffs the snapshot against the unit's dispatch baseline, and asks S4 how many monitors that diff warrants. It feeds each monitor the incremental diff. A monitor is a warm, **read-only** ACP session on a seat instance distinct from the creator (`claude#2`). It replies with findings in a strict line format. The supervisor confirms each finding **mechanically** against the snapshot tree (the quoted line must be at `path:line`), drops anything below the bar, dedups, and emits `monitorFinding`. HIGH findings go into the running turn through `_session/steering` at the next tool-call boundary. The worker answers each one `ACCEPT` or `DECLINE` with evidence in its final output. When the worker's turn ends, the supervisor runs a bounded **final pass** on the settled tree: it re-confirms every finding, asks each monitor to HOLD or WITHDRAW each declined finding, and sends each held HIGH dispute to a one-off council. The result is one **TeamLedger**. The ledger reaches the engine gate (the judge's WORK payload, the evaluator's prior context, the rework amendment) and the human gate (crew route, studio panel). No finding and no council verdict denies anything by itself.

## 4. S2 — monitor subscription

### 4.1 Who spawns it, on which seat

- **Process.** The crew daemon hosts wicked-core in-process (napi). wicked-core's `AcpStepRunner` spawns the monitor's ACP bridge child (`claude-agent-acp`) through the same `start_acp_process` path chat seats use. crew spawns nothing new.
- **Seat.** It is a seat **instance** key from the unit's monitor candidates (§8, from S5), e.g. `claude#2`. Its configuration home follows #595 (`<worker home>/claude-2`, `crates/wicked-apps-core/src/spawn.rs:1295`). The supervisor refuses a candidate that is (a) the creator's instance (`unit.assigned_cli`), or (b) a seat whose `[cli.acp]` is not `acp_input_governance: true`. For (b), the read-only boundary is enforced by answering `session/request_permission`, and an unadmitted adapter never asks (`src/acp_runner.rs:6752-6776`). Today that makes every monitor a claude instance (§13, Q2). A refused or failed start is disclosed as `monitorAttached{status:"failed"}` and never retried within the attempt.
- **Read-only.** The monitor session is opened with the chat boundary (`chat_boundary`, `src/acp_runner.rs:5920`; judged by `chat_boundary_result`, `src/acp_permission.rs:429`), with `ChatScope { cwd: <private per-monitor scratch>, code_graph_db: <run's graph>, read_roots: [<unit worktree>] }`. It can read the worktree and the graph. It cannot write the worktree. It runs with epoch 0, so any elicitation it raises is cancelled (`src/acp_runner.rs:3895`): monitors never ask a human.
- **No chat events.** The monitor uses the pool and boundary machinery through a new `monitor_ensure`/`monitor_turn` pair mirroring `chat_ensure`/`chat_turn` (`src/acp_runner.rs:6187`). It emits **no** `ChatDelta`/`ChatReply`/`ChatClosed`. crew folds those into chat state (`server.ts:1203-1225`), and a monitor is not a chat. Pool key `team:<run>:<ord>:<attempt>:<monitorId>`.
- **Lifetime.** A monitor opens lazily, when its first batch is due, so a unit whose diff never warrants a monitor spawns nothing. It closes when the attempt's final pass returns and on `on_run_complete` (`src/acp_runner.rs:7676`).

### 4.2 What it receives, and from where

- **Source: engine CoreEvents in-process, not crew `/ws`.** The supervisor calls `Core::subscribe` (`src/lib.rs:634`; `Command::Subscribe`, `src/actor.rs:1132`) once at spawn. Reading `/ws` would add a network hop and a second copy of engine state in crew, for events the engine emits itself.
- **The one event it consumes: `unitCheckpoint` (new, §7).** It is emitted by the ACP carrier in the `session/update` arm (`src/acp_runner.rs:4385`), right after `handle_update`, when the frame is a `tool_call_update` with a terminal `status` (`completed`/`failed`). `kind`, `title` and `locations` are remembered per `toolCallId` from the earlier `tool_call`/refinement frames, because the terminal frame does not repeat them (adapter `acp-agent.js:7280-7290`). It is emitted **only for a unit with a team context** (§8), so non-teamed volume is zero.
- **Not consumed:** `unitOutputDelta` (prose; §11 risk 3: intent is the source of premature findings), `governanceHookFired` (post-fold, `src/event.rs:589`), `toolInvoked` (post-unit, absent on ACP).
- **Attach, finish and context** are configuration rather than events. They travel on a direct `TeamCmd` channel: `Attach` is sent by `exec_turn_inner` at turn start (workdir, pinned git dir, baseline tree, creator instance, monitor candidates); `Finish` is sent by the worker thread (§4.7).
- **Wrapped and PTY carriers emit no checkpoints.** There is no mid-turn channel to act on (§5.1), so a mid-turn batch there would cost tokens for findings that can reach only the gate. On those carriers the monitor runs **once**, on the final settled diff (§4.7).

### 4.3 Batching at semantic checkpoints, never per token

The supervisor keeps, per `(run, ord, attempt)`, a pending flag set by any checkpoint whose `kind` may change the tree: `edit`, `delete`, `move`, `execute`, `other` (ACP ToolKind; `read`/`search`/`fetch`/`think` never set it). A **batch** starts when:

1. the pending flag is set, **and**
2. at least `BATCH_MIN_INTERVAL` (60 s) has passed since the previous batch started, **and**
3. no batch is in flight for that monitor (checkpoints arriving meanwhile coalesce into the next one), **and**
4. the monitor's batch budget (`MAX_BATCHES` = 10 per attempt) is not spent (spending it is disclosed in the ledger).

The batch snapshots once (§4.4). If the tree id equals the last batch's tree, the batch is skipped and the flag is cleared: an `execute` that wrote nothing costs one snapshot and no model turn. The monitor never sees a token stream. It sees at most one prompt per minute, each carrying a settled diff.

### 4.4 The settled worktree diff

- **Snapshot:** `worktree_guard::snapshot_through(worktree, pinned_git_dir)` (`src/worktree_guard.rs:385`) gives tree `T_k`. It reads the worktree through a scratch index and never touches the worker's index. The baseline is `unit.worktree_baseline.tree` (`src/actor.rs:6517-6545`). A unit with no baseline (unbound, or the snapshot failed) is not monitored, and that is disclosed as `monitorAttached{status:"failed", error:"no worktree baseline"}`.
- **Diff:** `git diff-tree -p -U3 --no-renames <T_{k-1}> <T_k>` (incremental; for the first batch `T_{k-1}` = baseline) runs over the object DB only. It is capped at `DIFF_CAP` (48 KB) per batch. Past the cap, the prompt carries the name-status list and the first hunks up to the cap, and says so. The monitor can `Read` any file for more context (read root).
- **Settled** means: the snapshot is taken after a tool call reported a terminal status, so the edit that call made has landed. The **final** pass snapshots after the worker's turn has returned, the same moment the guard's first look uses (`src/cli_runner.rs:871`).

### 4.5 Monitor prompt and reply contract

The first batch's prompt names the unit's criterion and phase, the monitor's role ("you advise; you do not decide; the worker may refuse you with evidence; the gate decides"), the bar (§4.6), and the reply grammar. Every later batch carries only the incremental diff, the checkpoint titles since the last batch (at most 20, 256 B each), and the dispositions recorded since the last batch, so a monitor never re-raises a finding the worker already answered.

Reply grammar: zero or more lines, then a final line `DONE`. Anything else is ignored.

```
FINDING {"severity":"high|medium|low","path":"src/x.rs","line":41,"evidence":"<the exact text of that line>","claim":"<what is wrong and why>","suggestion":"<optional fix>"}
```

At the final pass (§4.7) a monitor may also emit `HOLD <findingId> — <reason>` or `WITHDRAW <findingId> — <reason>` for each finding the worker declined.

### 4.6 Confirmation, severity bar, dedup

Every `FINDING` line goes through these checks in order. The first failure drops it and increments a `rejected` counter in the ledger.

1. **Parse:** the line is strict JSON with all required keys, `path` is repo-relative with no `..`, and `line` ≥ 1. Else `malformed`.
2. **Bar:** `severity ∈ {high, medium}`. `low` is dropped (`belowBar`). Only `high` is ever injected into the worker (§5). `medium` reaches the gate only.
3. **Confirm against the settled tree:** `git cat-file -p <T_k>:<path>` exists, and its line `line` equals `evidence` after trimming and collapsing whitespace. Else `unconfirmed`. This is the mechanical "file:line or it did not happen" rule: a finding that cites a line that is not there never surfaces. `inDiff` is recorded as whether `path` is in the baseline..`T_k` name-status list. Out-of-diff findings are allowed (a caller the change broke) and labelled.
4. **Dedup:** `findingId = "f-" + hex(sha256(path ‖ "\n" ‖ normalized evidence))[..16]`. The key is the line's **text**, not its number, so an edit that shifts lines does not mint a new finding. A second monitor with the same id is recorded in `corroboratedBy` and not re-emitted. The same monitor repeating it counts as `duplicate`. An id the worker already answered is never re-injected.

Survivors are emitted as `monitorFinding` (§7).

### 4.7 Final pass and lifecycle

After `runner.run_unit_streaming` returns Ok (`src/cli_runner.rs:761`), the worker thread calls a new trait method `StepRunner::team_finish(&StepInput, &StepOutput) -> Option<TeamLedger>`. Its default returns `None`, so every non-ACP runner and test runner is unchanged. `AcpStepRunner` forwards it to the supervisor as `TeamCmd::Finish` and blocks for at most `FINAL_PASS_BUDGET` (300 s):

1. Abandon any in-flight batch. Snapshot `T_final`. Summon monitors per S4 on the baseline..`T_final` diff if none are attached yet. This is how wrapped units are monitored.
2. Run one final batch per monitor over `T_{last}`..`T_final`. The prompt also lists every finding the worker declined, with the worker's reason, and asks for `HOLD` or `WITHDRAW` on each.
3. Re-confirm every finding against `T_final`. If the evidence text is gone from the file, the finding becomes `superseded` (the worker changed it). If the text moved, `finalLine` is updated.
4. Parse the worker's `ADVICE` lines (§5.3) from `StepOutput.output`.
5. Run disputes through the council (§6.3).
6. Return the `TeamLedger`. The worker thread puts it in `UnitEvidence.team` (`src/workflow.rs:265`, additive) and renders it into `work_for_agent` (§6.2).

If the budget expires, the ledger records `finalPass: "timed_out"` with what was gathered so far, and the gate proceeds. A failed unit (`status != Ok`) skips the final pass (`finalPass: "skipped"`), as the guard's first look already does (`src/cli_runner.rs:871`).

### 4.8 Constants (engine-owned, env-overridable for rigs only)

| Name | Default | Why |
|---|---|---|
| `BATCH_MIN_INTERVAL` | 60 s | Bounds monitor turns per unit-hour. #590's 380 deltas/run become ≤ 60 batches/hour worst case, typically far fewer (§4.3 skip rule). |
| `MAX_BATCHES` | 10 per monitor per attempt | A hard cost ceiling. Exhausting it is disclosed. |
| `DIFF_CAP` | 48 KB per batch | Prompt bound. Truncation is disclosed in the prompt. |
| `MONITOR_TURN_BUDGET` | 240 s | A batch turn that runs over is abandoned, and the batch is lost (disclosed). |
| `FINAL_PASS_BUDGET` | 300 s | The gate never waits unboundedly on advice. |
| `MAX_DISPUTES` | 3 per attempt | Councils are the spikiest thing the platform does. HIGH-first, and a dispute past the cap is marked `notAdjudicated`. |

## 5. S3 — monitor→worker injection

### 5.1 What each carrier can actually do

| Carrier | Mid-turn? | Evidence | Decision |
|---|---|---|---|
| **ACP, adapter advertises `_meta.steering.supported`** (claude-agent-acp 0.73.0) | **Yes.** `_session/steering` pushes an `SDKUserMessage` into the in-flight turn. | adapter `acp-agent.js:1186-1272`, `:853-859`. The client already writes stdin under `write_lock` inside the turn loop (`src/acp_runner.rs:3884-3887`, `:3920-3931`). | **The one S3 mechanism.** |
| **ACP, adapter does not advertise it** (codex-acp, pi-acp, agy-acp, copilot as shipped) | No. ACP has no client→agent content channel inside a turn. `session/request_permission` answers carry only an option id (`src/acp_permission.rs:404-405`). | — | Findings reach the gate only. The next-unit operator queue (`src/acp_runner.rs:6665`) is **not** reused: advice about unit *n* is stale by unit *n+1*, and the rework amendment (§6.2) already carries it where it matters. |
| **Wrapped `claude -p`** | Possible, but not built. `stdin` is null (`src/execute_wrapped.rs:3468`), so the only channel is a hook. Claude Code's `PostToolBatch` hook can return `hookSpecificOutput.additionalContext`, "injected once before the next model call" (Claude Code hooks reference, *PostToolBatch decision control*). The per-unit settings file today arms only `PreToolUse` (`src/execute_wrapped.rs:2793-2798`). | as cited | **Not in S3.** Claude units run on ACP in the production daemon (§2 rows 1-2), and wrapped is the fallback. A second injection mechanism for the fallback carrier is the fallback ladder this design refuses. Wrapped units get the final-pass monitor (§4.7) and the gate. Revisit only if rig data shows claude units landing on wrapped routinely. |
| **Wrapped non-claude** (`codex exec`, …) | No. It is a one-shot argv prompt with null stdin. | `src/execute_wrapped.rs:3468` | Gate only. |
| **PTY** (`session_runner.rs`) | Bytes can be written mid-turn (`src/actor.rs:2641-2678`), but it is **unsafe**: `collect_turn` ends at the first `result` sentinel (`src/session_runner.rs:610-632`), so a message typed mid-turn produces a second result that the next unit's collect reads as its own. The carrier is also unreachable from crew: napi spawns the ACP runner (§2 row 1), never `spawn_with_pty_sessions`. | as cited | Not used. |

### 5.2 The ACP mechanism, precisely

- **Capability, read once.** In `start_acp_process`, read `init["result"]["_meta"]["steering"]["supported"] == true` from the already-captured `initialize` result (`src/acp_runner.rs:2645`) and store it as `AcpProcess.steering_supported`, beside `elicitation_advertised` (`:924`, `:2752`). It is never re-probed per turn. This is the same "one decision per process" rule core#341 applied to elicitation.
- **Mailbox.** `AcpStepRunner.steer_mailbox: Arc<Mutex<HashMap<(run, ord, attempt), Vec<Advice>>>>`, written by the supervisor after a HIGH `monitorFinding`. Keying by attempt means advice for a superseded attempt can never reach its successor.
- **Delivery point.** This is the same place the checkpoint is emitted: the `session/update` arm (`src/acp_runner.rs:4385`), on a terminal `tool_call_update`. If `proc.steering_supported` and the mailbox holds advice for this unit/attempt, drain **all** of it into **one** request:
  ```json
  {"jsonrpc":"2.0","id":<next_id>,"method":"_session/steering",
   "params":{"sessionId":"<proc.session_id>",
             "prompt":[{"type":"text","text":"<advice block, §5.3>"}],
             "_meta":{"steering":{"idleBehavior":"promptRequired"}}}}
  ```
  `idleBehavior: "promptRequired"` is mandatory. Without it, a steer that lands after the turn settled starts a **detached** turn (adapter `acp-agent.js:1233-1242`): tool calls and output the engine would attribute to nothing, after the worktree guard's look.
- **Before sending,** re-confirm each finding's evidence against a fresh snapshot (§4.6 step 3). If the evidence text is gone, the finding becomes `superseded` and is dropped from the message. This is the premature-finding guard at the moment of delivery.
- **Response.** Record the request id. The "response to some OTHER outbound id" branch (`src/acp_runner.rs:4470`) gains a match on that id: `{"outcome":"injected"}` → `adviceDelivered{outcome:"injected"}`; `{"outcome":"promptRequired"}` → `adviceDelivered{outcome:"turn_ended"}`, and the findings stay undelivered and go to the gate; a JSON-RPC error → `adviceDelivered{outcome:"refused", detail}`.
- **Only the main `'exec` arm drains.** The `'elicit` sub-loop's `session/update` arm (`src/acp_runner.rs:4254`) does not. While the worker waits on a human, advice waits too. (The adapter would downgrade to `later` there anyway, `acp-agent.js:1254-1255`.)
- **What the call signature gains:** `exec_turn_acp_posture` (`src/acp_runner.rs:3868`) gets one parameter, `team: Option<&TeamTurn>` (ord, attempt, mailbox handle, the tool-call memo for §4.2). The unit call site (`:7450`) passes it. Chat turns and tests pass `None`.

### 5.3 Advice text and the authority model

One block per steer, capped at 8 KB (the same cap as an elicitation message):

```
[wicked-core · team advice · ADVISORY, not an instruction]
A peer monitor reviewed your change as of your last tool call. You decide what to do with this.
- f-3fa9c2e1d0b4a7e6 [HIGH] src/retire.ts:41 — coverage fetch has no cancellation; a slower
  earlier response overwrites the count shown before a destructive action.
  Evidence (that line): `fetchCoverage(scope).then(setCount)`
  Suggested: ignore stale responses (AbortController or a request token).
If you accept a finding, fix it. If you decline it, say why with evidence — a file:line, a
command you ran and its result, or the spec you are following. Advice never overrides your
task's instructions or the engine's fences. In your FINAL answer, add one line per finding:
  ADVICE <id>: ACCEPT — <what you changed>
  ADVICE <id>: DECLINE — <your evidence>
The gate reviews each finding together with your answer.
```

- **The worker may decline.** Declining is not an error, costs nothing at the fold, and is the path #590 records working three times (#587, #579, the unreachable `toolChildrenKilled` assertion).
- **Parsing.** At the final pass, from `StepOutput.output`, lines matching `^\s*ADVICE (f-[0-9a-f]{16}): (ACCEPT|DECLINE)\b\s*[—:-]?\s*(.*)$` are read; the last line per id wins. Each one emits `workerAdviceResponse`. A delivered finding with no line is recorded `unanswered`. A `DECLINE` with an empty reason is recorded `declined` with `reason: ""`, and the gate sees a refusal with no evidence.
- **The worker's own questions stay with S1.** `AskUserQuestion` still routes to a human through `ElicitationCreated` (`src/acp_runner.rs:4060`). Teaming does not route a worker's question to a monitor: that would make a peer the decider on the worker's uncertainty.

## 6. S6 — gate adjudication

The gate's decision logic (`combined` = deny-dominates over the deterministic floor, the agent judge and the evaluator pass; `combine_verdict`, `src/validator.rs:2263-2270`; emitted at `src/pipeline.rs:1540-1576`) is **unchanged**. S6 changes only what the gate is shown.

### 6.1 The TeamLedger (core)

The ledger is built by the final pass (§4.7), carried in `UnitEvidence.team`, and persisted on the unit record by the fold as `WorkUnit.team_ledger` (additive, `#[serde(default, skip_serializing_if = "Option::is_none")]`, beside `worktree_mutation`, `src/domain.rs:556`). It is emitted **once** as `teamLedger` from `apply_and_finish_unit`, immediately before `GateEvaluated` (`src/pipeline.rs:1540`), so the wire order is ledger → verdict. Wire shape in §7.

### 6.2 How it reaches the engine gate

One renderer, `team::render_for_gate(&TeamLedger) -> String`, feeds three existing inputs:

1. **The judge.** `work_for_agent` is extended at the same seam `worktree_evidence_for_judge` uses (`src/cli_runner.rs:885-895`), inside the WORK fence the judge already treats as untrusted data (`src/validator.rs:1656-1658`). **A monitor never grades its own finding.** This is the evaluator≠creator rule applied one level up. The judge is chosen on **three** paths today, and the rule must hold on all of them:

   | Path | Where today | What it excludes today | Change |
   |---|---|---|---|
   | Bus-mediated, pinned validator (`WICKED_BUS_DB` set) | `src/cli_runner.rs:911-925` → `bus_request_agent_verdict` `:351-394` | Only `work_author`, carried as `GateEvalRequest.work_author` (`:97-112`). The consuming evaluator daemon is outside core and crew (catalog `crates/wicked-governance/seed/event-catalog-annotations.json:79`). The daemon does not report which seat judged (`:915-916`). | `GateEvalRequest` gains `excluded_seats: Vec<String>` (`#[serde(default)]`, so old payloads read as `[]`), filled with `ledger_authors` (below). The daemon **must** select no seat whose instance **or** cli key is in `excluded_seats ∪ {work_author}`. `GateEvalResponse` gains `judge_cli: Option<String>` (`#[serde(default)]`). **Verification, fail-closed:** when `excluded_seats` is non-empty, core treats a response whose `judge_cli` is `None` or is in the excluded set as a DENY with reason `"gate eval bus-path DENY (fail-closed): evaluator did not prove monitor exclusion"`, the same deny posture `bus_deny!` already uses (`:360-370`). With `excluded_seats` empty (no findings), behaviour is byte-identical to today, so an old daemon keeps working for non-teamed units. |
   | Inline, pinned validator | `src/cli_runner.rs:932-988` | `[DETERMINISTIC_VALIDATOR_SEAT, work_author]` (`:937`) | `excluded` becomes that array **plus** `ledger_authors`, passed to `distinct_judge_available`, to the distinct-seat pre-check at `:938-960`, and to `agent_validate_with_refusals` (`:988`). |
   | Inline, default judge (F-7R2-005) | `src/cli_runner.rs:1010-1032` | `[work_author]` (`:1020`) | Same: plus `ledger_authors`, for `distinct_judge_available` (`:1021`) and `agent_validate_with_refusals` (`:1029`). |

   `ledger_authors` = every `seat` and every `corroboratedBy` monitor's seat among the ledger's findings (any status), deduplicated. When excluding them leaves no eligible judge, the existing "no eligible judge seat distinct from …" path records `judge_skipped` with the monitors named in the reason (`:960-985`, `:1062-1081`). That is disclosed as UNGATED, not a silent self-grade, and it is exactly the case §6.7 decides about.
2. **The evaluator unit.** When the actor assembles `prior_outputs` for a unit that reviews ord *n* (`src/actor.rs:6650-6671`), it appends the rendered ledger of *n* as `PriorUnitOutput { label: "[team findings — unit n]" }`.
3. **The rework.** When the gate or evaluator `request_changes`, the amendment persisted by `rewind_to_creator` (`src/actor.rs:6673-6700`) includes the unresolved findings. They reach the creator's next attempt the way every other review note does.

### 6.3 Disputes: when a one-off council is convened, and what it returns

**Trigger (all four must hold):**
1. `severity == "high"` (the injection bar),
2. the finding was delivered (`delivery == "injected"`),
3. the worker answered `DECLINE` with a non-empty reason, and
4. at the final pass, the authoring monitor answered `HOLD` (a monitor that withdraws, or does not answer, ends the dispute: the finding goes to the gate as `declined` or `withdrawn`).

**What the council is handed.** This is the input to S5's decision-council entry point (§8). One council per dispute, at most `MAX_DISPUTES`:
- `question`: "Does the finding stand against the settled change?"
- `positions`: `[{by:"monitor <seat>", position:"finding stands", reason:<HOLD reason>}, {by:"worker <seat>", position:"refusal stands", reason:<DECLINE reason>}]`
- `evidence`: the finding (severity, claim, suggestion), `path:finalLine` with the evidence line, the `T_final` hunk around it (±20 lines from `git diff-tree -p -U20 <baseline> <T_final> -- <path>`, capped at 16 KB), the unit's criterion, and the tree id `T_final`.
- `excluded seats`: the creator instance and every monitor in `corroboratedBy ∪ {author}`. Parties do not vote.

**What comes back and where it goes.** The verdict `{verdict: "finding_stands" | "refusal_stands" | "no_consensus", agreementPct, dissent, seats}` is written into the finding's `dispute` object in the ledger. It then reaches the gate through §6.2 like every other ledger fact. **The council verdict is evidence, not a decision.** Nothing in the fold reads `dispute.verdict`. The judge, the evaluator and the human read it. A council that cannot convene (no eligible seats, budget) records `verdict: "not_convened"` with the reason. It never blocks and never defaults either way.

### 6.4 crew (read side only)

- **Route:** `GET /api/v1/runs/:id/team` → `{ runId, units: [{ ord, attempt, ledger: TeamLedger }] }`, read from the run's unit records (`adapter.sessionsDetail()`, which is durable across restarts). A run with no ledgers returns `units: []`, not 404. It is registered beside `GET /runs/:id/gate` (`routes.ts:3206`). There is **no new decision route**: the human still decides through `POST /runs/:id/gate` (`routes.ts:2942-2943`).
- **`/ws`:** nothing to add. The relay already forwards every frame (`server.ts:1235`).
- **crew-api-types:** the seven event types of §7 as `type` aliases in the style of `WorkerToolCallDeniedEvent` (`packages/crew-api-types/index.d.ts:1883`), plus `TeamLedger` and the route's response type. The Rust `to_json` arm is the source of truth. The api-types spelling must match it byte for byte.
- **Evidence bundle:** `GET /runs/:id/evidence` includes each unit's `team_ledger` (it rides the unit record already).

### 6.5 studio

- **Gate panel.** `SteeringGate` (`src/components/SteeringGate.tsx:85`, inside `ApprovalDock`) gains a *Team findings* section, fed by `GET /runs/:id/team`, for the units the gate covers. Each row shows: severity, `path:line` (links to the run diff), claim, the worker's disposition and reason, delivery, and, for a dispute, the council verdict with agreement and dissent. The approve/reject/amend actions are unchanged.
- **Verdict detail.** `VerdictDetail` (`src/components/VerdictDetail.tsx:90`) shows the unit's ledger next to `gateEvaluated`, including `rejected` counters and `finalPass`, so a silent monitor reads as silent and not as clean.
- **Live feed.** `NarratorFeed` renders `monitorAttached`, `monitorFinding`, `adviceDelivered` and `workerAdviceResponse` as one-line narrator entries. They are display only.

### 6.6 What S6 deliberately does not do

It adds no auto-deny on an open finding and never reads the council verdict as a decision. Each would make a monitor or a council the decider (#590 risk 4). Whether an unresolved HIGH forces a **human pause** (not a deny) is the blocking decision in §6.7.

### 6.7 BLOCKING S6: does an unresolved HIGH dispute force a human pause? (operator decision pending)

**The hole (review on #604, HIGH).** `combine_verdict` approves when the deterministic floor passes and no agent verdict rejects (`src/validator.rs:2263-2270`); `agent == None` counts as no rejection. When no judge runs (`judge_skipped`, `src/cli_runner.rs:960-985`, `:1062-1081`; also when excluding the monitors leaves no eligible seat, §6.2), a unit can auto-approve while its ledger holds a HIGH the worker declined, the monitor held, and a council either upheld or never adjudicated. Run `aee254f1` had `agentVerdict: "skipped"`. S6's builder must not pick an option. The operator's answer drops into one of the two blocks below unchanged.

**Shared definition.** A finding is a **qualifying HIGH dispute** when all of the following hold in the attempt's `TeamLedger`:
- `severity == "high"`,
- `delivery == "injected"`,
- `status == "declined"` (the worker declined it with a reason),
- `monitorReply.kind == "hold"`, and
- `dispute.verdict ∈ {"finding_stands", "not_adjudicated", "not_convened", "no_consensus"}`. That is every verdict except `"refusal_stands"`. `no_consensus` and `not_convened` count as not adjudicated.

**Option A: no escalation (rev 1 as written).**
- The fold never reads the ledger. The ledger is evidence to the judge (when one runs), to the evaluator unit and to the human at any gate the run already has.
- Consequence, stated plainly: with the judge skipped and the floor passing, a qualifying HIGH dispute **can** auto-approve. The protection is only that `gateEvaluated.ungatedReason` and the ledger disclose it.
- Code: nothing beyond §6.1–§6.5.
- Acceptance: #16 (Option A form).

**Option B: a qualifying HIGH dispute forces a conditional human pause.**
- **Where:** in the actor's fold, **after** `apply_and_finish_unit` computes `outcome.approved` (`src/pipeline.rs:1540-1576`) and **before** the unit is advanced. The decision logic (`combine_verdict`) is unchanged.
- **Condition:** `outcome.approved == true` **and** the ledger holds ≥1 qualifying HIGH dispute. If the fold denied, nothing changes: the pause never turns a deny into anything.
- **Action:** `pause_for_human(…, gate_kind: "team_dispute", prompt)` (`src/actor.rs:6070-6110`). This pause fires on **every** run, including runs launched with no human confirmation. The prompt lists each qualifying finding (`findingId`, `path:finalLine`, claim, worker reason, monitor reason, council verdict with agreement and dissent). `GateDecided`/`UnitDone` are **not** emitted until a human answers `POST /runs/:id/gate`: approve advances the unit, reject cancels the run, and approve+amend reruns the creator with the amendment (the existing arms, `routes.ts:2942`). `awaitingHuman.gateKind` gains the token `"team_dispute"`. The api-types `gateKind` stays an open string, and the studio `SteeringGate` renders the ledger for that ord (§6.5).
- **What this does and does not make authoritative:** monitors and the council still cannot deny or approve. They can only require that a human looks. The human remains the decider.
- **Acceptance (B-1, the proof it cannot auto-approve with the judge skipped):** Build a teamed unit whose floor passes, whose judge is skipped (`judge_skipped = Some(..)`, `agent_verdict = None`), whose evaluator pass is true, and whose ledger holds one qualifying HIGH dispute. For each `dispute.verdict` in `{finding_stands, not_adjudicated, not_convened, no_consensus}`, the fold must emit `awaitingHuman{gateKind:"team_dispute"}` for that ord, the session status must be `awaiting_human`, and **no** `unitDone` or `gateDecided{allow:true}` may be emitted for that ord.
- **Acceptance (B-2):** the same unit with `dispute.verdict == "refusal_stands"`, or with the finding `accepted`/`withdrawn`/`superseded`, or with `monitorReply.kind == "withdraw"`, emits `unitDone` with no pause.
- **Acceptance (B-3):** the same unit with the floor failing emits `unitDenied` and no `team_dispute` pause.
- **Acceptance (B-4):** approving the `team_dispute` gate through `POST /runs/:id/gate` emits `resumed` then `unitDone` for that ord.

**Out of both options (named so it is not assumed):** a HIGH that was never delivered (non-steering carrier) or left `unanswered` is not a dispute. It reaches the gate as evidence only, under either option.

## 7. Wire contract — new CoreEvent variants

Every variant is mapped by hand in `CoreEvent::to_json` (`src/event.rs:1269-1275`; an unmapped variant is a build failure). `session` is the run id, as in every run event. Every field is always present: `Option` becomes `null`, never an absent key. Strings are capped as stated, at a UTF-8 boundary.

```jsonc
// S2 — ACP carrier, per terminal tool_call_update of a teamed unit.
{"type":"unitCheckpoint","session":"<run>","ord":3,"attempt":1,
 "seq":17,                        // per attempt, monotonic from 1
 "toolCallId":"toolu_…",
 "kind":"edit",                   // ACP ToolKind verbatim; "other" when absent
 "title":"Edit src/retire.ts",    // ≤256 B
 "status":"completed",            // "completed" | "failed"
 "paths":["src/retire.ts"]}       // from locations, ≤16

// S2 — supervisor, when S4 summons a monitor (or its start fails).
{"type":"monitorAttached","session":"<run>","ord":3,"attempt":1,
 "monitorId":"m1","seat":"claude#2",
 "status":"attached",             // "attached" | "failed"
 "reason":"review_plan monitors=1 (critical,destructive)",
 "error":null}

// S2 — supervisor, per confirmed, above-bar, first-seen finding.
{"type":"monitorFinding","session":"<run>","ord":3,"attempt":1,
 "findingId":"f-3fa9c2e1d0b4a7e6","monitorId":"m1","seat":"claude#2",
 "severity":"high",               // "high" | "medium"
 "path":"src/retire.ts","line":41,
 "evidence":"fetchCoverage(scope).then(setCount)",   // ≤512 B
 "claim":"…",                     // ≤2 KB
 "suggestion":null,               // ≤2 KB or null
 "tree":"<T_k tree id>","inDiff":true,"checkpointSeq":17}

// S3 — ACP carrier, per steering request answered.
{"type":"adviceDelivered","session":"<run>","ord":3,"attempt":1,
 "findingIds":["f-3fa9c2e1d0b4a7e6"],
 "carrier":"acp_steering",
 "outcome":"injected",            // "injected" | "turn_ended" | "refused"
 "detail":null}

// S3 — final pass, per parsed ADVICE line.
{"type":"workerAdviceResponse","session":"<run>","ord":3,"attempt":1,
 "findingId":"f-3fa9c2e1d0b4a7e6",
 "disposition":"declined",        // "accepted" | "declined"
 "reason":"…"}                    // ≤2 KB, may be ""

// S6 — fold, once per attempt, immediately before gateEvaluated.
{"type":"teamLedger","session":"<run>","ord":3,"attempt":1,
 "finalPass":"completed",         // "completed" | "timed_out" | "skipped"
 "renderedToJudge":true,
 "monitors":[{"monitorId":"m1","seat":"claude#2","batches":4,
              "status":"completed",   // "completed" | "budget_exhausted" | "failed" | "timed_out"
              "error":null}],
 "findings":[{"findingId":"f-3fa9c2e1d0b4a7e6","monitorId":"m1","seat":"claude#2",
              "severity":"high","path":"src/retire.ts","line":41,"finalLine":43,
              "evidence":"…","claim":"…","suggestion":null,"tree":"<T_k>","inDiff":true,
              "corroboratedBy":[],
              "delivery":"injected",          // "injected" | "not_delivered"
              "status":"declined",            // "accepted" | "declined" | "withdrawn" | "unanswered" | "superseded"
              "workerReason":"…",             // null unless answered
              "monitorReply":{"kind":"hold","reason":"…"},   // null | {"kind":"hold"|"withdraw","reason"}
              "dispute":{"verdict":"finding_stands",         // null when no dispute
                         // "finding_stands" | "refusal_stands" | "no_consensus" | "not_convened" | "not_adjudicated"
                         "agreementPct":67,"dissent":1,"seats":["codex","pi"],"reason":null}}],
 "rejected":{"malformed":0,"belowBar":2,"unconfirmed":1,"duplicate":0}}
```

**Durable log.** All seven go to the per-run event log like any event. `unitCheckpoint` is the only frequent one, it exists only for teamed units, and it is small (≤ ~600 B).

## 8. Interfaces consumed (not designed here)

| From | What this design reads | Where it is read |
|---|---|---|
| **S4** (`src/review_scale.rs`, branch `feat/590-s4-review-scale` @ `da957f4`, stub) | `signals_from_diff(&str) -> ChangeSignals` and `review_plan(&ChangeSignals) -> ReviewPlan`, from which **only `monitors: u8`** is used: the target monitor count for the diff so far. It is evaluated at every batch and at the final pass, and the count only grows within an attempt. `depth` and `post_hoc_reviewer` are not read by S2. | supervisor, per batch (§4.3) and final pass (§4.7) |
| **S5** (deterministic `RoutingInfo::Teamed`, no mode selector) | For the unit: the creator's seat instance and an **ordered list of monitor-candidate seat instances** (e.g. `["claude#2","claude#3"]`). The supervisor takes the first `monitors` candidates that pass §4.1. An empty list means no monitors. | `exec_turn_inner` builds `TeamCmd::Attach` (§4.2) |
| **S5** (decision-council entry point) | The call `question, positions, evidence, excluded seats → verdict, agreementPct, dissent, seats`. It must be callable **off the actor**, from the worker thread, and must emit its own council events. | final pass step 5 (§6.3) |
| **S1** (#599, merged) | Nothing new. The worker's `AskUserQuestion` stays human-routed (§5.3). | — |

If S4's or S5's final Rust names differ, only the read sites named in the right-hand column change.

## 9. Where each piece lives

| Piece | Repo | Location |
|---|---|---|
| `TeamSupervisor`, `TeamCmd`, `TeamLedger`, confirmation, dedup, `render_for_gate` | core | new `src/team.rs`; constructed in `Core::spawn_with_acp_sessions` (`src/lib.rs:545`), subscribes via `Core::subscribe` |
| `unitCheckpoint` emission, `steering_supported`, steer mailbox + delivery, response matching | core | `src/acp_runner.rs` (`:2645`, `:924`, `:3868`, `:4385`, `:4470`, `:7450`) |
| `monitor_ensure` / `monitor_turn` | core | `src/acp_runner.rs`, beside `chat_turn` (`:6187`) |
| `StepRunner::team_finish` (default `None`) | core | `src/workflow.rs:365` trait |
| Final pass call, ledger render into WORK, monitor seats excluded from the judge (all three paths) | core | `src/cli_runner.rs:761`, `:885-895`; bus `:97-112`, `:351-394`, `:911-925`; inline `:937`, `:988`, `:1020`, `:1029` |
| (Option B only) `team_dispute` pause after an approving fold | core | `src/actor.rs` fold, `pause_for_human` `:6070` |
| `UnitEvidence.team`, `WorkUnit.team_ledger`, `teamLedger` emission | core | `src/workflow.rs:265`, `src/domain.rs`, `src/pipeline.rs:1540` |
| Ledger into evaluator prior context and rework amendment | core | `src/actor.rs:6650-6700` |
| Seven `to_json` arms + core-ts `.d.ts` regen | core | `src/event.rs`, `crates/wicked-core-ts` |
| `GET /runs/:id/team`, api-types events + DTO | crew | `packages/crew/src/api/routes.ts`, `packages/crew-api-types/index.d.ts` |
| Gate panel, verdict detail, feed lines | studio | `SteeringGate.tsx`, `VerdictDetail.tsx`, `NarratorFeed.tsx`, `api/client.ts` |

## 10. Build order

1. **Wire contract first (core, S2-owned).** The seven `CoreEvent` variants + `to_json` arms + the `TeamLedger` type, with no emitters. Merge. From here, crew and studio can build against fixtures, and S3 can build against the types.
2. **In parallel, three builders:**
   - **S2 (core):** `unitCheckpoint` emission, `team.rs` supervisor, monitor sessions, batching, snapshot/diff, confirmation, dedup, `monitorFinding`, final pass (steps 1-3), `team_finish`.
   - **S3 (core):** `steering_supported`, mailbox, delivery at the checkpoint arm, response matching, `adviceDelivered`, advice text, `ADVICE` parsing → `workerAdviceResponse` (final pass step 4). It is tested against a mock bridge that advertises and answers `_session/steering`.
   - **S6 (core → crew → studio):** ledger persistence, `teamLedger` emission, `render_for_gate` into judge/evaluator/rework, monitor exclusion from the judge, the dispute trigger and the council call (step 5, behind S5's entry point), then the crew route + api-types, then the studio surfaces.
3. **core-ts release → crew → studio** on the normal train (crew bundles studio; bump the studio pin).
4. **Rig proof (acceptance §12-E2E)** after all three land.

The S2/S3 seam is the mailbox type and the `TeamTurn` parameter, both fixed by this document. The S2/S6 seam is `TeamLedger`, fixed in step 1.

## 11. Risks → the mechanism that addresses each

| Risk (#590) | Mechanism | Where |
|---|---|---|
| **1. Stream volume** (380 `unitOutputDelta`/run) | Monitors never see deltas. A batch needs a tree-changing checkpoint **and** 60 s **and** an idle monitor **and** budget, **and** a changed tree id. Hard caps: `MAX_BATCHES`, `DIFF_CAP`. Incremental diffs go to a warm session. Zero monitors (and zero checkpoints) until S4's policy asks for one. | §4.2, §4.3, §4.8 |
| **2. Monitor noise** | The bar drops `low`. Only `high` interrupts the worker. Dedup is on line text across monitors. Answered ids are never re-raised. The `rejected` counters are shown at the gate, so a monitor's noise rate is visible evidence. | §4.6, §5.2, §7 |
| **3. Premature findings** | Monitors see settled diffs, not narration. Every finding must quote the exact line at `path:line` in the snapshot tree (mechanical, no model). It is re-confirmed before injection and again at `T_final`. A finding whose text disappeared becomes `superseded`. | §4.2, §4.6, §5.2, §4.7 |
| **4. Monitors becoming authoritative** | No fold branch reads a finding or a council verdict. Monitors are excluded from judging. The worker may decline. Disputes go to a council of non-parties whose verdict is evidence. The judge, evaluator and human decide. | §5.3, §6.2, §6.3, §6.6 |
| Advice arriving after the turn → a detached turn | `idleBehavior: "promptRequired"` on every steer. `turn_ended` is disclosed and the advice goes to the gate. | §5.2 |
| Advice for one attempt reaching another | The mailbox is keyed `(run, ord, attempt)`. | §5.2 |
| A monitor writing the worktree | Admitted seats only. The chat boundary makes the worktree read-only. The worktree guard's final look is unchanged. | §4.1 |
| The gate stalled by a slow monitor or council | `FINAL_PASS_BUDGET`, `MAX_DISPUTES`. On expiry the ledger says so and the gate proceeds. | §4.7, §4.8 |

## 12. Acceptance (testable)

**S2**
1. A teamed ACP unit on a mock bridge that emits `tool_call` (kind `edit`) then `tool_call_update{status:"completed"}` emits exactly one `unitCheckpoint` with that `kind`, `title` and `paths`. A non-teamed unit emits none.
2. 30 checkpoints inside 60 s with one tree change produce **one** monitor batch. A checkpoint burst with an unchanged tree id produces zero batches.
3. A monitor reply citing `path:line` whose text does not match the snapshot tree emits no `monitorFinding` and increments `rejected.unconfirmed`. A matching reply emits one, with the tree id.
4. Two monitors citing the same line text emit one `monitorFinding`, and the ledger lists the second in `corroboratedBy`. A `low` finding emits nothing and counts `belowBar`.
5. A monitor candidate equal to the creator instance, or on an unadmitted adapter, yields `monitorAttached{status:"failed"}` and no process. A monitor's `Write` into the worktree is denied by the boundary (the permission answer is the reject option), and the worktree tree id is unchanged.
6. A unit whose diff is docs-only (S4 `monitors: 0`) spawns no monitor process and emits no `monitorAttached`.

**S3**
7. On a mock bridge advertising `_meta.steering.supported`, a HIGH finding queued before a terminal `tool_call_update` produces exactly one `_session/steering` request with `_meta.steering.idleBehavior == "promptRequired"`, and `adviceDelivered{outcome:"injected"}` when the bridge answers `{"outcome":"injected"}`.
8. The same bridge answering `{"outcome":"promptRequired"}` yields `outcome:"turn_ended"`, the finding is `delivery:"not_delivered"` in the ledger, and **no** further `session/prompt` is sent.
9. A bridge that does not advertise steering receives **no** `_session/steering` frame.
10. A MEDIUM finding is never sent through steering. A finding whose evidence text is gone from the fresh snapshot is not sent and ends `superseded`.
11. Final output lines `ADVICE f-…: DECLINE — campaign.rs:325 documents the exclusion` and `ADVICE f-…: ACCEPT — added AbortController` produce one `workerAdviceResponse` each, with the matching disposition and reason. A delivered id with no line ends `unanswered`.
12. Advice queued for attempt 1 is not delivered to attempt 2.

**S6**
13. `teamLedger` is emitted immediately before `gateEvaluated` for the same `(session, ord)`, and the unit record carries `team_ledger` after a daemon restart.
14. **Monitor exclusion, all three judge paths.** For a ledger whose findings were authored by `claude#2` (corroborated by `claude#3`), with creator `claude`:
    (a) **inline pinned:** `agent_validate_with_refusals` is called with an excluded set containing `claude#2` and `claude#3`, and neither is selected (roster fixture with only those plus a distinct seat → the distinct seat judges);
    (b) **inline default judge:** the same;
    (c) **bus path:** the published `GateEvalRequest` JSON carries `"excluded_seats":["claude#2","claude#3"]`; a response with `judge_cli: "claude#2"` or `judge_cli: null` folds as a DENY with the fail-closed reason; a response with a distinct `judge_cli` is honoured;
    (d) with an empty ledger, the bus request carries `"excluded_seats":[]` and a `judge_cli: null` response is honoured exactly as today;
    (e) excluding the monitors leaves no eligible seat → `judge_skipped` names the monitors. The judge prompt on (a)–(c) contains the rendered ledger inside the WORK fence.
15. **Dispute trigger:** HIGH + injected + `DECLINE` with a reason + monitor `HOLD` convenes exactly one council, whose input carries the finding, both reasons, the `T_final` hunk at `path:finalLine`, and excludes the creator and author seats. Any one condition removed convenes none.
16. A council verdict never changes `combined` (`combine_verdict` inputs are identical with and without it). **Option A form:** the same unit with and without the verdict folds to the same `GateDecided.allow`. **Option B form:** replaced by §6.7 B-1…B-4. Which form ships is the §6.7 decision.
17. `GET /api/v1/runs/:id/team` returns the ledgers for a teamed run and `units: []` for a run without monitors. The api-types fixture round-trips the Rust `to_json` output for all seven events.
18. Studio: the gate panel shows each finding's severity, `path:line`, the worker's disposition and reason, and the council verdict. Approve/reject still go through `POST /runs/:id/gate`.

**E2E (rig, after all three).** Re-run the #590 B18 shape: a retire-flow unit that writes a cancellation-free coverage fetch. Pass requires a HIGH `monitorFinding` at that handler's `file:line` **before** the unit's turn ends, an `adviceDelivered{injected}`, a `workerAdviceResponse`, and a `teamLedger` the gate panel renders. Launch alone is not a pass: the run must reach a terminal state.

## 13. Open questions

- **Q1. BLOCKING S6: does a qualifying HIGH dispute force a human pause?** Moved to §6.7, with Option A and Option B written out and their acceptance tests. S6 does not start its fold work until the operator picks one.
- **Q2. Monitor independence.** Only claude is ACP-admitted (§2), so every monitor today is `claude#N` reviewing a claude creator. Same model, correlated blind spots. Each instance also needs its own signed-in config home, and #591's per-instance login is out of scope there. Admitting a second adapter to input governance (the codex-acp research is `registry.rs:252-300`) is what makes monitors model-diverse. Until then, independence is instance-level only.
- **Q3. Pre-emptive steering.** The adapter delivers a steer at priority `now`, which **aborts** the current generation (adapter `acp-agent.js:1196-1206`). The client cannot ask for `later`. Every injected HIGH therefore costs an interrupted cycle and a context jolt mid-task. Whether that helps or hurts work quality is an empirical question for the rig. If it hurts, the remedy is to batch HIGH advice to fewer, later checkpoints, not a second mechanism.
- Smaller: `unitCheckpoint` also serves studio (live tool activity) and could be emitted for every ACP unit, but volume argues against it. The per-attempt ledger is lost on a daemon restart mid-unit: findings emitted before the crash survive in the event log only.
