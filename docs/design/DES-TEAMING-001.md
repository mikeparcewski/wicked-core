# DES-TEAMING-001 — Real-time teaming: monitors on the live unit, advice into the worker, the gate decides

- **Status:** PROPOSED (rev 9). **One item is PENDING an operator decision: Q4 (§13).** Until it is answered, every clause in this document marked **`[Q4-pending]`** (the "unaccepted" extension of §6.3 item 2 to unanswered and not-delivered HIGHs, and everything that follows from it, including acceptance #16 (h)–(i)) is **not approved for build**; the text outside those markers is the approved reading (declined-and-held). Builders implement the ruling as given (declined-and-held → council). Everything else in S2, S3 and S6 is unblocked.
- **Rev 2 (2026-09-23):** moved from `.product/` (untracked since #562) to `docs/design/`; §6.7 records the HIGH-dispute escalation as a blocking decision with two exact options; §6.2 extends monitor exclusion to the bus-mediated judge (`GateEvalRequest.excluded_seats`). Both come from review on #604.
- **Rev 3 (2026-09-23):** operator ruling on Q1, written into §6.3/§6.7. An unresolved HIGH goes to a council. YES continues the run autonomously. NO **or no verdict** pauses for a human (fail-closed). The rev 2 Option A/B block is replaced by the ruling.
- **Rev 4 (2026-09-23):** S2 builder corrections, verified on `d5d9708`. The ACP spawn path does not resolve seat-instance keys yet, so that prerequisite is now build step 0 (§4.1, §9, §10). §4.7 is re-ordered: S2's final batch carries no declines; S3 parses `ADVICE` and then runs the HOLD/WITHDRAW round. §7 notes that fixtures compare JSON values, not key order.
- **Rev 5 (2026-09-23):** review on #604 at `0493046`. (1) `[Q4-pending]` The ruling is **extended** (status: Q4, §13): every **unaccepted** HIGH (declined-and-held, unanswered, or never delivered) is unresolved and takes the council path, so `combine_verdict(true, None)` can no longer approve one. (2) `findingId` now includes a stable location anchor, so two identical hazardous lines in one file are two findings; a secondary `lineKey` keeps moved-line correlation (§4.6).
- **Rev 6 (2026-09-23):** review on #604 at `609d0dd`. (1) The S3 hold-round acceptance now says **unaccepted** (declined, unanswered, not delivered), matching §6.3 (`[Q4-pending]` since rev 10). (2) The `team_dispute` pause is a **pause intent** returned on `UnitOutcome`; the actor's `apply_step_result` performs `pause_for_human` (§6.7), since `apply_and_finish_unit` has no session or actor handles. (3) There are **six** new events, not seven; every reference agrees.
- **Rev 7 (2026-09-23):** review on #604 at `f7635b8`, consistency pass on the fail-closed rule. (1) A final-pass timeout no longer "proceeds": every still-declined (`[Q4-pending]` still-unaccepted) HIGH is synthesized as held with `no_verdict`/`timeout` before the fold condition, so the pause fires (§4.7, §11, acceptance #16 j). (2) `confirm_gate` reads the open gate's `gate_kind` first; a `team_dispute` approve never re-dispatches the unit (§6.7, §9, acceptance #16 g/k), with the current approve path cited. (3) Every remaining "declined" is an enum value, a quotation of the ruling, or one of the three explicit states.
- **Rev 8 (2026-09-23):** review on #604 at `24eeaf9`. The final pass is owned by the **shared worker-thread seam** (`run_unit_and_judge_with_roster`, `src/cli_runner.rs:748`, after `:761`), not by a `StepRunner` method only `AcpStepRunner` implemented, so wrapped (and PTY) units get the same final-pass ledger. Only the live checkpoint stream and the steer stay ACP-only. Attach moves to the same seam. Acceptance S2 #8 proves it on a wrapped unit.
- **Rev 9 (2026-09-23):** review on #604 at `c9b01ab`. Header at rev 9; the pending status of the unaccepted-HIGH extension is stated in exactly one place (Q4) and referenced from §6.3, §6.7 and #16 (h)–(i). The team-pause condition is evaluated **before** `teamLedger` is emitted, so the ledger carries the decided `teamPause`; the fold's emission order is now explicit: `teamLedger` → `gateEvaluated` → [`awaitingHuman(team_dispute)` | `gateDecided` + `unitDone`].
- **Rev 10 (2026-09-23):** review on #604 at `a360c36`. Every occurrence of the unaccepted (unanswered / not-delivered) behaviour is now inside a `[Q4-pending]` clause; the approved-now text everywhere says declined-and-held (§3, §4.5, §4.7 step 5 and budget expiry, §6.3, §9, §11, acceptance #11 and #15).
- **Rev 11 (2026-09-23):** review on #604 at `f96f74b`. Acceptance S2 #8 now tests the approved path for a wrapped unit (an undelivered finding ends `unanswered`, no hold round, no council, `teamPause:false`), with the withdrawal variant in a `[Q4-pending]` clause. Every client-facing route reads `/api/v1/…`; crew's `${V}` mount prefix is named once in §6.4.
- **Rev 12 (2026-09-23):** review on #604 at `02fc8c5`. The approved council input (§6.3 `question` and `positions`) is declined-only; the "no answer" / "not delivered — <outcome>" variants are a `[Q4-pending]` clause. Final grep: every prose "no answer" / "not delivered" is marked or is a §7 value.
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
| crew relays every CoreEvent to `/ws` with no allowlist. Human gate: `GET`/`POST /api/v1/runs/:id/gate` (route-internal: `${V}/runs/:id/gate`). | `wicked-crew packages/crew/src/api/server.ts:1202-1235`; `routes.ts:2942-2943`, `:3206` |
| Studio: the pinned approval dock hosts the gate. `VerdictDetail` renders the engine gate. | `wicked-studio src/components/ApprovalDock.tsx`, `SteeringGate.tsx:85`, `VerdictDetail.tsx:90`; `src/api/client.ts:213-217`, `:294` |

Two consequences shape the design. First, a monitor cannot be driven off an existing live tool-call event: there is none (row 4), and the only per-tool-call event fires after the fold (row 5). S2 adds one. Second, exactly one carrier can accept content mid-turn: the ACP claude adapter's `_session/steering` (§5.1).

## 3. Shape in one paragraph

A worker unit runs on its carrier. Each time a tool call finishes, the ACP carrier emits `unitCheckpoint`. A per-daemon **TeamSupervisor** (core) subscribes to the engine's event fan-out. At most once per batch interval it snapshots the worktree, diffs the snapshot against the unit's dispatch baseline, and asks S4 how many monitors that diff warrants. It feeds each monitor the incremental diff. A monitor is a warm, **read-only** ACP session on a seat instance distinct from the creator (`claude#2`). It replies with findings in a strict line format. The supervisor confirms each finding **mechanically** against the snapshot tree (the quoted line must be at `path:line`), drops anything below the bar, dedups, and emits `monitorFinding`. HIGH findings go into the running turn through `_session/steering` at the next tool-call boundary. The worker answers each one `ACCEPT` or `DECLINE` with evidence in its final output. When the worker's turn ends, the supervisor runs a bounded **final pass** on the settled tree: it re-confirms every finding, asks each monitor to HOLD or WITHDRAW each declined finding (`[Q4-pending]` each unaccepted one: declined, unanswered, or never delivered), and sends each declined-and-held HIGH to a one-off council. The result is one **TeamLedger**. The ledger reaches the engine gate (the judge's WORK payload, the evaluator's prior context, the rework amendment) and the human gate (crew route, studio panel). No finding and no council verdict denies anything by itself.

## 4. S2 — monitor subscription

### 4.1 Who spawns it, on which seat

- **Process.** The crew daemon hosts wicked-core in-process (napi). wicked-core's `AcpStepRunner` spawns the monitor's ACP bridge child (`claude-agent-acp`) through the same `start_acp_process` path chat seats use. crew spawns nothing new.
- **Seat.** It is a seat **instance** key from the unit's monitor candidates (§8, from S5), e.g. `claude#2`. Its configuration home is meant to follow #595 (`<worker home>/claude-2`, `crates/wicked-apps-core/src/spawn.rs:1295`). **The ACP spawn path does not do that yet.** `registry_record` matches the exact key only (`src/acp_runner.rs:7755-7761`), so `claude#2` finds no record. `start_acp_process_with_write_roots` resolves the home with `seat_config_for(seat_cli)` (`:2221`), which is the cli's **primary** home. Monitors on instance keys therefore depend on the prerequisite PR (#591 ACP gap: two-step lookup, seat-key home, fence), build step 0 (§10). The supervisor refuses a candidate that is (a) the creator's instance (`unit.assigned_cli`), or (b) a seat whose `[cli.acp]` is not `acp_input_governance: true`. For (b), the read-only boundary is enforced by answering `session/request_permission`, and an unadmitted adapter never asks (`src/acp_runner.rs:6752-6776`). Today that makes every monitor a claude instance (§13, Q2). A refused or failed start is disclosed as `monitorAttached{status:"failed"}` and never retried within the attempt.
- **Read-only.** The monitor session is opened with the chat boundary (`chat_boundary`, `src/acp_runner.rs:5920`; judged by `chat_boundary_result`, `src/acp_permission.rs:429`), with `ChatScope { cwd: <private per-monitor scratch>, code_graph_db: <run's graph>, read_roots: [<unit worktree>] }`. It can read the worktree and the graph. It cannot write the worktree. It runs with epoch 0, so any elicitation it raises is cancelled (`src/acp_runner.rs:3895`): monitors never ask a human.
- **No chat events.** The monitor uses the pool and boundary machinery through a new `monitor_ensure`/`monitor_turn` pair mirroring `chat_ensure`/`chat_turn` (`src/acp_runner.rs:6187`). It emits **no** `ChatDelta`/`ChatReply`/`ChatClosed`. crew folds those into chat state (`server.ts:1203-1225`), and a monitor is not a chat. Pool key `team:<run>:<ord>:<attempt>:<monitorId>`.
- **Lifetime.** A monitor opens lazily, when its first batch is due, so a unit whose diff never warrants a monitor spawns nothing. It closes when the attempt's final pass returns and on `on_run_complete` (`src/acp_runner.rs:7676`).

### 4.2 What it receives, and from where

- **Source: engine CoreEvents in-process, not crew `/ws`.** The supervisor calls `Core::subscribe` (`src/lib.rs:634`; `Command::Subscribe`, `src/actor.rs:1132`) once at spawn. Reading `/ws` would add a network hop and a second copy of engine state in crew, for events the engine emits itself.
- **The one event it consumes: `unitCheckpoint` (new, §7).** It is emitted by the ACP carrier in the `session/update` arm (`src/acp_runner.rs:4385`), right after `handle_update`, when the frame is a `tool_call_update` with a terminal `status` (`completed`/`failed`). `kind`, `title` and `locations` are remembered per `toolCallId` from the earlier `tool_call`/refinement frames, because the terminal frame does not repeat them (adapter `acp-agent.js:7280-7290`). It is emitted **only for a unit with a team context** (§8), so non-teamed volume is zero.
- **Not consumed:** `unitOutputDelta` (prose; §11 risk 3: intent is the source of premature findings), `governanceHookFired` (post-fold, `src/event.rs:589`), `toolInvoked` (post-unit, absent on ACP).
- **Attach, finish and context** are configuration rather than events. They travel on a direct `TeamCmd` channel from the **shared worker-thread seam** that every carrier's unit passes through, `run_unit_and_judge_with_roster` (`src/cli_runner.rs:748`; callers `src/actor.rs:7083` in-process and `src/cli_runner.rs:2013` bus): `Attach` (workdir, pinned git dir, baseline tree, creator instance, monitor candidates) immediately before `runner.run_unit_streaming` (`:761`), `Finish` immediately after it returns (§4.7). The carrier is not involved, so a wrapped or PTY unit is attached exactly like an ACP one.
- **Wrapped and PTY carriers emit no checkpoints.** There is no mid-turn channel to act on (§5.1), so a mid-turn batch there would cost tokens for findings that can reach only the gate. On those carriers the monitor runs **once**, on the final settled diff, from the same shared seam (§4.7). The promise is kept by construction: the final pass is not a carrier feature.

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

In the hold round (§4.7 step 5, S3-owned), a monitor answers `HOLD <findingId> — <reason>` or `WITHDRAW <findingId> — <reason>` for each **declined** finding (`[Q4-pending]` for each **unaccepted** finding: declined, unanswered, or never delivered). A review batch never asks for or accepts these.

### 4.6 Confirmation, severity bar, dedup

Every `FINDING` line goes through these checks in order. The first failure drops it and increments a `rejected` counter in the ledger.

1. **Parse:** the line is strict JSON with all required keys, `path` is repo-relative with no `..`, and `line` ≥ 1. Else `malformed`.
2. **Bar:** `severity ∈ {high, medium}`. `low` is dropped (`belowBar`). Only `high` is ever injected into the worker (§5). `medium` reaches the gate only.
3. **Confirm against the settled tree:** `git cat-file -p <T_k>:<path>` exists, and its line `line` equals `evidence` after trimming and collapsing whitespace. Else `unconfirmed`. This is the mechanical "file:line or it did not happen" rule: a finding that cites a line that is not there never surfaces. `inDiff` is recorded as whether `path` is in the baseline..`T_k` name-status list. Out-of-diff findings are allowed (a caller the change broke) and labelled.
4. **Dedup, two keys.**
   - **`anchor`** — a stable location context, resolved in this order: (i) the nearest enclosing symbol at `path:line` in the run's estate graph (the graph the monitor is bound to, §4.1; a `SearchEntity`-class lookup of the symbol whose span contains the line, used only when the graph's copy of the file matches the blob at `T_k`, since the graph may lag the worktree); else (ii) the hunk header context of the hunk containing the line in `git diff-tree -p <baseline> <T_k> -- <path>` (git's `@@ … @@ <funcname>` text, which is git's own enclosing-function heuristic); else (iii) the empty string (file-level). The anchor is recorded on the finding.
   - **`findingId = "f-" + hex(sha256(path ‖ "\n" ‖ anchor ‖ "\n" ‖ normalized evidence))[..16]`** — the identity. Two identical hazardous lines in two functions of one file are two findings. The same line text in the same function is one finding wherever the line number lands, so an edit that shifts lines does not mint a new one.
   - **`lineKey = "l-" + hex(sha256(path ‖ "\n" ‖ normalized evidence))[..16]`** — the secondary key, used for moved-line correlation only: re-confirmation (§4.7 step 3) finds the finding's line at `T_final` by `lineKey` within the same anchor first, then anywhere in the file. It never merges findings.
   - A second monitor with the same `findingId` is recorded in `corroboratedBy` and not re-emitted. The same monitor repeating it counts as `duplicate`. An id the worker already answered is never re-injected. Two identical lines inside one function (a rare true collision) are one finding by design; the claim names one line and the worker fixes both or neither.

Survivors are emitted as `monitorFinding` (§7).

### 4.7 Final pass and lifecycle

**Owner: the shared worker-thread seam, for every carrier.** `run_unit_and_judge_with_roster` (`src/cli_runner.rs:748`) is the one function every unit passes through whatever runner executed it: the in-process worker thread (`src/actor.rs:7083` → `run_unit_and_judge` `src/cli_runner.rs:682`) and the bus worker (`src/cli_runner.rs:2013`, which rebuilds `StepInput` from the bus payload at `:1971`, so no handle can ride the input). Immediately after `runner.run_unit_streaming` returns Ok (`:761`), it calls `team::finish(input, &output) -> Option<TeamLedger>` (new, `src/team.rs`). That function reaches the `TeamSupervisor` through a process-wide handle installed once by `Core::spawn_with_acp_sessions` (`src/lib.rs:545`; a `OnceLock`, absent under `spawn_with_engine` and in tests, where `finish` returns `None` and nothing changes). It is **not** a `StepRunner` method: a trait default of `None` would have made the final pass an ACP-only feature and broken the §4.2 promise to wrapped units. The supervisor's monitor sessions are ACP sessions on the monitor's own seat regardless of the unit's carrier. `team::finish` sends `TeamCmd::Finish` and blocks for at most `FINAL_PASS_BUDGET` (300 s):

1. **(S2)** Abandon any in-flight batch. Snapshot `T_final`. Summon monitors per S4 on the baseline..`T_final` diff if none are attached yet. This is how wrapped units are monitored.
2. **(S2)** Run one final **review** batch per monitor over `T_{last}`..`T_final`. It carries **no declines**: the worker's dispositions do not exist until step 4.
3. **(S2)** Re-confirm every finding against `T_final` by `lineKey`: first inside its `anchor`, then anywhere in the file. If the evidence text is gone from the file, the finding becomes `superseded` (the worker changed it). If the text moved, `finalLine` is updated.
4. **(S3)** Parse the worker's `ADVICE` lines (§5.3) from `StepOutput.output` → `workerAdviceResponse`.
5. **(S3)** **Hold round.** For each monitor with ≥1 **declined** finding (`[Q4-pending]` ≥1 **unaccepted** finding: declined, unanswered, or never delivered), send one more turn on the same warm session. It lists each such finding with the worker's decline reason (`[Q4-pending]` or its state: "no answer", or "not delivered — <adviceDelivered outcome or carrier>") and asks for `HOLD <id> — <reason>` or `WITHDRAW <id> — <reason>`, then `DONE`. No reply for an id counts as HOLD (§6.3). Monitors whose findings were all accepted or superseded get no turn.
6. **(S6)** Send each unresolved HIGH to the council (§6.3).
7. Return the `TeamLedger`. The worker thread puts it in `UnitEvidence.team` (`src/workflow.rs:265`, additive) and renders it into `work_for_agent` (§6.2).

**Budget expiry is fail-closed.** If `FINAL_PASS_BUDGET` expires at any step, the final pass stops where it is and the ledger records `finalPass: "timed_out"` with what was gathered. Then, **before** the ledger is returned and before the §6.7 fold condition runs, every HIGH that is still `declined` (`[Q4-pending]` still unaccepted, §6.3 item 2: `status ∉ {accepted, withdrawn, superseded}`) and has no hold-round reply or no council verdict yet is **synthesized**: `monitorReply` becomes `{kind:"hold", reason:"no reply (final pass timed out)"}` where it is missing, and `dispute` becomes `{verdict:"no_verdict", reason:"timeout"}` where it is missing. Results already recorded (a `WITHDRAW`, a council YES or NO) are kept. MEDIUM findings and accepted/withdrawn/superseded findings are untouched. The fold still runs, but with an unresolved HIGH lacking a YES it returns `team_pause`, so a timeout **pauses for a human**; it never lets the run continue unattended. A failed unit (`status != Ok`) skips the final pass (`finalPass: "skipped"`), as the guard's first look already does (`src/cli_runner.rs:871`).

### 4.8 Constants (engine-owned, env-overridable for rigs only)

| Name | Default | Why |
|---|---|---|
| `BATCH_MIN_INTERVAL` | 60 s | Bounds monitor turns per unit-hour. #590's 380 deltas/run become ≤ 60 batches/hour worst case, typically far fewer (§4.3 skip rule). |
| `MAX_BATCHES` | 10 per monitor per attempt | A hard cost ceiling. Exhausting it is disclosed. |
| `DIFF_CAP` | 48 KB per batch | Prompt bound. Truncation is disclosed in the prompt. |
| `MONITOR_TURN_BUDGET` | 240 s | A batch turn that runs over is abandoned, and the batch is lost (disclosed). |
| `FINAL_PASS_BUDGET` | 300 s | The gate never waits unboundedly on advice. |
| `MAX_DISPUTES` | 3 per attempt | Councils are the spikiest thing the platform does. A dispute past the cap gets **no verdict** (`reason: "cap"`), which pauses for a human (§6.7). The cap bounds cost; it never lets a dispute through. |

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

The ledger is built by the final pass (§4.7), carried in `UnitEvidence.team`, and persisted on the unit record by the fold as `WorkUnit.team_ledger` (additive, `#[serde(default, skip_serializing_if = "Option::is_none")]`, beside `worktree_mutation`, `src/domain.rs:556`). It is emitted **once** as `teamLedger` from `apply_and_finish_unit`, **after** the team-pause condition has been evaluated (§6.7, so the event carries the decided `teamPause`) and immediately before `GateEvaluated` (`src/pipeline.rs:1540`). The fold's wire order for a unit is fixed: `teamLedger` → `gateEvaluated` → then either `awaitingHuman{gateKind:"team_dispute"}` (emitted by the actor; no `gateDecided`) or `gateDecided` + `unitDone`/`unitDenied`. Wire shape in §7.

### 6.2 How it reaches the engine gate

One renderer, `team::render_for_gate(&TeamLedger) -> String`, feeds three existing inputs:

1. **The judge.** `work_for_agent` is extended at the same seam `worktree_evidence_for_judge` uses (`src/cli_runner.rs:885-895`), inside the WORK fence the judge already treats as untrusted data (`src/validator.rs:1656-1658`). **A monitor never grades its own finding.** This is the evaluator≠creator rule applied one level up. The judge is chosen on **three** paths today, and the rule must hold on all of them:

   | Path | Where today | What it excludes today | Change |
   |---|---|---|---|
   | Bus-mediated, pinned validator (`WICKED_BUS_DB` set) | `src/cli_runner.rs:911-925` → `bus_request_agent_verdict` `:351-394` | Only `work_author`, carried as `GateEvalRequest.work_author` (`:97-112`). The consuming evaluator daemon is outside core and crew (catalog `crates/wicked-governance/seed/event-catalog-annotations.json:79`). The daemon does not report which seat judged (`:915-916`). | `GateEvalRequest` gains `excluded_seats: Vec<String>` (`#[serde(default)]`, so old payloads read as `[]`), filled with `ledger_authors` (below). The daemon **must** select no seat whose instance **or** cli key is in `excluded_seats ∪ {work_author}`. `GateEvalResponse` gains `judge_cli: Option<String>` (`#[serde(default)]`). **Verification, fail-closed:** when `excluded_seats` is non-empty, core treats a response whose `judge_cli` is `None` or is in the excluded set as a DENY with reason `"gate eval bus-path DENY (fail-closed): evaluator did not prove monitor exclusion"`, the same deny posture `bus_deny!` already uses (`:360-370`). With `excluded_seats` empty (no findings), behaviour is byte-identical to today, so an old daemon keeps working for non-teamed units. |
   | Inline, pinned validator | `src/cli_runner.rs:932-988` | `[DETERMINISTIC_VALIDATOR_SEAT, work_author]` (`:937`) | `excluded` becomes that array **plus** `ledger_authors`, passed to `distinct_judge_available`, to the distinct-seat pre-check at `:938-960`, and to `agent_validate_with_refusals` (`:988`). |
   | Inline, default judge (F-7R2-005) | `src/cli_runner.rs:1010-1032` | `[work_author]` (`:1020`) | Same: plus `ledger_authors`, for `distinct_judge_available` (`:1021`) and `agent_validate_with_refusals` (`:1029`). |

   `ledger_authors` = every `seat` and every `corroboratedBy` monitor's seat among the ledger's findings (any status), deduplicated. When excluding them leaves no eligible judge, the existing "no eligible judge seat distinct from …" path records `judge_skipped` with the monitors named in the reason (`:960-985`, `:1062-1081`). That is disclosed as UNGATED, not a silent self-grade, and it is exactly the case the §6.7 ruling covers.
2. **The evaluator unit.** When the actor assembles `prior_outputs` for a unit that reviews ord *n* (`src/actor.rs:6650-6671`), it appends the rendered ledger of *n* as `PriorUnitOutput { label: "[team findings — unit n]" }`.
3. **The rework.** When the gate or evaluator `request_changes`, the amendment persisted by `rewind_to_creator` (`src/actor.rs:6673-6700`) includes the unresolved findings. They reach the creator's next attempt the way every other review note does.

### 6.3 Disputes: when a one-off council is convened, and what it returns

**Unresolved HIGH.** The operator's definition (2026-09-23) was "declined by the worker, held by the monitor". Review on #604 (`0493046`, HIGH) showed that leaves a hole: a HIGH the worker never answered, or that never reached the worker, was not unresolved, so with the judge skipped `combine_verdict(true, None)` (`src/validator.rs:2263-2270`) approved it. This document therefore writes the **extended, fail-closed** definition below. **Its status is Q4 (§13): the unanswered and not-delivered arms of item 2 are not approved for build until Q4 is answered.**

A finding is **unresolved** when:
1. `severity == "high"`, and
2. the worker **declined** it: `status == "declined"` (refused with a reason, or with none). **`[Q4-pending]`** — the extension widens this to **unaccepted**, `status ∉ {"accepted", "withdrawn", "superseded"}`, adding:
   - `unanswered` with `delivery == "injected"` — delivered, no `ADVICE` line;
   - `unanswered` with `delivery == "not_delivered"` — never reached the worker (non-steering carrier, `turn_ended`, `refused`); and
3. the authoring monitor **held** it in the hold round (`monitorReply.kind == "hold"`, which includes no reply).

**Assumption, stated because the ruling is fail-closed:** a monitor that gives no HOLD/WITHDRAW for a declined (`[Q4-pending]` unaccepted) finding in the hold round (turn timeout, crash, final-pass budget expired, malformed reply) counts as **HOLD**. Withdrawal must be explicit. A monitor's silence never clears a finding.

**What the council is handed.** This is the input to S5's decision-council entry point (§8). One council per unresolved HIGH, at most `MAX_DISPUTES` per attempt:
- `question`: "The worker declined this HIGH finding and the monitor holds it. Should the run continue autonomously with the worker's refusal standing? YES = continue. NO = a human must decide." `[Q4-pending]`: for an unanswered or undelivered finding the question reads "The worker did not accept this HIGH finding (gave no answer / never received it) and the monitor holds it. Should the run continue autonomously with the work as submitted?"
- `positions`: `[{by:"worker <seat>", position:"YES — the refusal stands", reason:<the DECLINE reason>}, {by:"monitor <seat>", position:"NO — the finding stands", reason:<HOLD reason, or "no reply (counted as hold)">}]`. `[Q4-pending]`: for an unanswered or undelivered finding the worker's `reason` is `"no answer"` or `"not delivered — <outcome>"`; its position is still YES (the work as submitted stands), and the council then judges the finding on the evidence alone.
- `evidence`: the finding (severity, claim, suggestion), `path:finalLine` with the evidence line, the `T_final` hunk around it (±20 lines from `git diff-tree -p -U20 <baseline> <T_final> -- <path>`, capped at 16 KB), the unit's criterion, and the tree id `T_final`.
- `excluded seats`: the creator instance and every monitor in `corroboratedBy ∪ {author}`. Parties do not vote.

**What comes back.** Each dispute is recorded in the finding's `dispute` object:
- `verdict: "yes"` — the council produced a YES;
- `verdict: "no"` — the council produced a NO;
- `verdict: "no_verdict"` — anything else, with `reason ∈ {"no_quorum", "seats_benched", "error", "timeout", "cap"}`:
  - `no_quorum`: S5 reports no verdict;
  - `seats_benched`: no eligible non-party seats;
  - `error`: the council call failed;
  - `timeout`: the council or the final-pass budget expired before a verdict;
  - `cap`: the dispute was beyond `MAX_DISPUTES`.

`agreementPct`, `dissent` and `seats` are recorded for `yes`/`no` and are `null` for `no_verdict`. The verdict is **recorded as evidence at the gate**: it is in the ledger, in the judge's WORK, in the evaluator's prior context and in the human gate prompt. It controls exactly one thing, whether the run continues autonomously or pauses for a human (§6.7). It never approves or denies the unit: `combine_verdict` does not read it.

### 6.4 crew (read side only)

- **Route:** `GET /api/v1/runs/:id/team` (in `routes.ts` the prefix is the `${V}` mount, which is `/api/v1`; every path in this document is written client-facing, with the prefix) → `{ runId, units: [{ ord, attempt, ledger: TeamLedger }] }`, read from the run's unit records (`adapter.sessionsDetail()`, which is durable across restarts). A run with no ledgers returns `units: []`, not 404. It is registered beside `GET /api/v1/runs/:id/gate` (`routes.ts:3206`). There is **no new decision route**: the human still decides through `POST /api/v1/runs/:id/gate` (`routes.ts:2942-2943`).
- **`/ws`:** nothing to add. The relay already forwards every frame (`server.ts:1235`).
- **crew-api-types:** the six event types of §7 as `type` aliases in the style of `WorkerToolCallDeniedEvent` (`packages/crew-api-types/index.d.ts:1883`), plus `TeamLedger` and the route's response type. The Rust `to_json` arm is the source of truth. The api-types spelling must match it byte for byte.
- **Evidence bundle:** `GET /api/v1/runs/:id/evidence` includes each unit's `team_ledger` (it rides the unit record already).

### 6.5 studio

- **Gate panel.** `SteeringGate` (`src/components/SteeringGate.tsx:85`, inside `ApprovalDock`) gains a *Team findings* section, fed by `GET /api/v1/runs/:id/team`, for the units the gate covers. Each row shows: severity, `path:line` (links to the run diff), claim, the worker's disposition and reason, delivery, and, for a dispute, the council verdict with agreement and dissent. The approve/reject/amend actions are unchanged.
- **Verdict detail.** `VerdictDetail` (`src/components/VerdictDetail.tsx:90`) shows the unit's ledger next to `gateEvaluated`, including `rejected` counters and `finalPass`, so a silent monitor reads as silent and not as clean.
- **Live feed.** `NarratorFeed` renders `monitorAttached`, `monitorFinding`, `adviceDelivered` and `workerAdviceResponse` as one-line narrator entries. They are display only.

### 6.6 What S6 deliberately does not do

- No auto-deny on any finding.
- No fold input from monitors: `combine_verdict` and `combined` are computed exactly as today.
- The council verdict can only choose between **continue autonomously** and **pause for a human** (§6.7). It can never approve a unit the gate denied, and never deny one. A human pause is not a denial: the human decides.

### 6.7 Unresolved HIGH → council → continue or pause (operator ruling, 2026-09-23)

**The hole this closes (review on #604, HIGH).** `combine_verdict` approves when the deterministic floor passes and no agent verdict rejects (`src/validator.rs:2263-2270`); `agent == None` counts as no rejection. When no judge runs (`judge_skipped`, `src/cli_runner.rs:960-985`, `:1062-1081`; also when excluding the monitors leaves no eligible seat, §6.2), a unit could auto-approve with an unresolved HIGH in its ledger. Run `aee254f1` had `agentVerdict: "skipped"`.

**Ruling (operator, 2026-09-23).** Every unresolved HIGH (§6.3) goes to a council. Council **YES** → the run continues autonomously. Council **NO** → human pause. **Fail-closed:** if the council cannot produce a verdict (no quorum, benched seats, error, timeout, over the cap), that is treated as NO and the run pauses for a human. **It never auto-continues without a YES.** The verdict is recorded at the gate as evidence.

**Mechanism.**
- **Where, in two halves — the fold decides, the actor pauses.** `apply_and_finish_unit` (`src/pipeline.rs:827-841`) receives only the store, the fold inputs and an `emit` callback; it has no `AgentSession`, no subscribers and no `self_tx`, and `pause_for_human` (`src/actor.rs:6071`) needs all three. So:
  1. **Fold (`src/pipeline.rs`):** after `outcome` is computed and **before** `teamLedger` is emitted, evaluate the condition below and set `ledger.teamPause` from it, so the `teamLedger` event carries the decided value. Emission order in the fold, always: `teamLedger` → `GateEvaluated` (`:1540`) → then, when the condition does not hold, `GateDecided` + `UnitDone`/`UnitDenied` (`:1558-1576`); when it holds, nothing more from the fold (the actor emits `awaitingHuman`). When it holds, **do not** emit `GateDecided`/`UnitDone`, and return the outcome with a new additive field `UnitOutcome.team_pause: Option<TeamPause>` (`src/execute.rs:36`; `#[serde(default, skip_serializing_if = "Option::is_none")]`), where `TeamPause { prompt: String, finding_ids: Vec<String> }`. `outcome.approved` stays `true`: the gate approved; the run is only not allowed to continue unattended.
  2. **Actor (`apply_step_result`, `src/actor.rs:4318`):** at the call site `src/actor.rs:5358-5376`, after the existing denied branch (`if !outcome.approved`, `:5400-5427`, which returns `StepApplied::Paused` through `escalate_denied_unit`) and **before** `advance_or_pause` (`:5444`), add: `if let Some(tp) = outcome.team_pause { pause_for_human(store, subscribers, self_tx, &mut session, unit.ord, None, "team_dispute", tp.prompt)?; return Ok(StepApplied::Paused); }`. `pause_for_human` writes the `AwaitingHuman` session state and the open `interaction_request` in one batch and emits `awaitingHuman` (`:6071-6110`), so the pause is durable before the actor returns; the cursor stays on this unit.
  3. **Resume (`confirm_gate`, `src/actor.rs:7785`).** The current Approve arm is built for a unit that must run: it bumps `attempt` when the cursor unit is already `Done`/`Rejected` (`:8023-8034`), emits `Resumed` (`:8055`) and **always** calls `dispatch_unit` at the cursor (`:8061-8072`). Applied unchanged to a `team_dispute` gate, a human approve would **rerun the already-approved unit at attempt+1**. So `confirm_gate` gains a branch, decided **before** the open request is resolved: read the open gate's `gate_kind` from the durable row (`InteractionRequest.gate_kind`, `src/interaction.rs:59-76`, via `list_interactions` `:161`) ahead of `resolve_open_for_session` (`src/actor.rs:7864-7870`, `src/interaction.rs:184`). When it is `"team_dispute"`:
     - **Approve without amend:** resolve the request as answered; emit `Resumed{ord}`, then `GateDecided{allow:true}` and `UnitDone` for the cursor ord (the two the fold withheld); advance `unit_ix` past the unit exactly as the approved path's `advance_or_pause` (`:6120`) would have; **never** call `dispatch_unit` for that unit and **never** bump `attempt`.
     - **Approve with amend:** `rewind_to_creator` (`:8101`) as today — the human asked for a rerun with the amendment, and that re-dispatch is the intent.
     - **Reject:** cancel, as today.
     Every other `gate_kind` keeps the existing arms byte for byte.
- **Condition:** `outcome.approved == true` **and** the ledger holds ≥1 unresolved HIGH whose `dispute.verdict != "yes"`.
  - If the fold denied, the unit is denied as today (`unitDenied`) and there is no pause. The rework amendment carries the findings (§6.2).
  - If every unresolved HIGH has `dispute.verdict == "yes"`, the unit continues as today. This holds even when the judge was skipped: under the ruling, a council YES is the only way an unresolved HIGH continues autonomously.
- **Action (as above):** the fold withholds `GateDecided`/`UnitDone` and returns `team_pause`; the actor calls `pause_for_human(…, gate_kind: "team_dispute", prompt)`.
  - The pause fires on **every** run, including runs launched with no human confirmation.
  - The prompt lists each unresolved HIGH: `findingId`, `path:finalLine`, claim, the worker's reason, the monitor's reason, and the council verdict (agreement and dissent, or the no-verdict reason).
  - The human answers through the existing `POST /api/v1/runs/:id/gate` (`routes.ts:2942`): approve → `resumed` then `gateDecided{allow:true}` + `unitDone`; reject → cancel; approve+amend → the creator reruns with the amendment.
  - `awaitingHuman.gateKind` gains the token `"team_dispute"`. The api-types `gateKind` stays an open string, and studio's `SteeringGate` renders the ledger for that ord (§6.5).
- **`[Q4-pending]` Extension of the ruling (review on #604 at `0493046`, HIGH) — status: Q4 (§13).** The operator's words were "declined by the worker, held by the monitor". Read literally, a HIGH that was **not delivered** (non-steering carrier, a `turn_ended`/`refused` steer) or that the worker left **unanswered** was never unresolved, never reached a council, and with the judge skipped `combine_verdict(true, None)` approved it. The same fail-closed reasoning the ruling applies to a missing council verdict applies here: an unaccepted HIGH is unresolved whatever the reason it was not accepted (§6.3). Under the extension, **every** HIGH that is not `accepted`, `withdrawn` or `superseded` and that the monitor holds takes the council path: YES → continue, NO or no verdict → human pause. Whether this reading stands is Q4. If it is rejected, §6.3 item 2 shrinks back to `declined` and acceptance #16 (h)–(i) are removed; nothing else changes.

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
 "lineKey":"l-9c0e4b7a1d2f3e58",  // sha256(path ‖ normalized evidence): moved-line correlation only
 "anchor":"retire",               // enclosing symbol (estate graph), else the hunk header funcname, else ""
 "anchorSource":"graph",          // "graph" | "hunk" | "none"
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
              "lineKey":"l-9c0e4b7a1d2f3e58","anchor":"retire","anchorSource":"graph",
              "severity":"high","path":"src/retire.ts","line":41,"finalLine":43,
              "evidence":"…","claim":"…","suggestion":null,"tree":"<T_k>","inDiff":true,
              "corroboratedBy":[],
              "delivery":"injected",          // "injected" | "not_delivered"
              "status":"declined",            // "accepted" | "declined" | "withdrawn" | "unanswered" | "superseded"
              "workerReason":"…",             // null unless answered
              "monitorReply":{"kind":"hold","reason":"…"},   // null | {"kind":"hold"|"withdraw","reason"}
              "dispute":{"verdict":"no",                     // null when the finding is not an unresolved HIGH
                         // "yes" (continue) | "no" (pause) | "no_verdict" (pause, fail-closed)
                         "reason":null,                      // no_verdict only: "no_quorum" | "seats_benched" | "error" | "timeout" | "cap"
                         "agreementPct":67,"dissent":1,"seats":["codex","pi"]}}],   // null ×3 for no_verdict
 "teamPause":true,               // decided by the fold BEFORE this event is emitted (§6.7); true ⇒ no gateDecided follows, the actor emits awaitingHuman{team_dispute}
 "rejected":{"malformed":0,"belowBar":2,"unconfirmed":1,"duplicate":0}}
```

**Key order.** wicked-core's `serde_json` has no `preserve_order` feature (`Cargo.toml:59`, `serde_json = "1"`), so object keys serialize in sorted order, not the order shown above. Fixture tests in core, core-ts and crew-api-types must compare parsed JSON **values**, never serialized strings or key order.

**Durable log.** All six go to the per-run event log like any event. `unitCheckpoint` is the only frequent one, it exists only for teamed units, and it is small (≤ ~600 B).

## 8. Interfaces consumed (not designed here)

| From | What this design reads | Where it is read |
|---|---|---|
| **S4** (`src/review_scale.rs`, PR #600, impact model per §8.1) | `signals_from_diff(&str) -> ChangeSignals`, `graph_age(&store, base_commit) -> GraphAge`, and `assess(&ChangeSignals, Graph, Option<&dyn ModelAssessment>) -> Assessment`, from which **only `plan.monitors: u8`** is used: the target monitor count for the diff so far. It is evaluated at every batch and at the final pass, and the count only grows within an attempt. `plan.depth`, `plan.post_hoc_reviewer` and `plan.post_hoc_other_cli` are not read by S2. The supervisor passes `Graph::Ready { store, base_commit }`: the run's repo graph (`code_graph::resolved_code_graph_db`, opened read-only) and the run's `base_commit` (`WorktreeReady`, `src/actor.rs:1406`); `assess` checks `graph_age` itself, and a missing graph is passed as `Graph::Unavailable(reason)`. Either scores 100. | supervisor, per batch (§4.3) and final pass (§4.7) |
| **S5** (deterministic `RoutingInfo::Teamed`, no mode selector) | For the unit: the creator's seat instance and an **ordered list of monitor-candidate seat instances** (e.g. `["claude#2","claude#3"]`). The supervisor takes the first `monitors` candidates that pass §4.1. An empty list means no monitors. | `exec_turn_inner` builds `TeamCmd::Attach` (§4.2) |
| **S5** (decision-council entry point) | The call `question, positions, evidence, excluded seats → verdict, agreementPct, dissent, seats`. It must be callable **off the actor**, from the worker thread, and must emit its own council events. | final pass step 5 (§6.3) |
| **S1** (#599, merged) | Nothing new. The worker's `AskUserQuestion` stays human-routed (§5.3). | — |

If S4's or S5's final Rust names differ, only the read sites named in the right-hand column change.

### 8.1 S4 — impact model (operator decision, 2026-09-23; supersedes the size buckets)

How much review a change summons is read from **what depends on what it touched**, not from how many lines it has. A ten-line edit to a symbol with forty callers outranks a six-hundred-line new leaf file. `src/review_scale.rs` is a pure policy over plain data plus two read seams; every number and word list is one table (`THRESHOLDS`).

**Inputs per unit** (the settled diff + the run's repo estate graph, read in-process through `GraphRead` — the same `traverse`/`blast_radius(3)` the `wicked-estate blast-radius` CLI runs; no subprocess, no new dependency):

| Signal | How it is read |
|---|---|
| **C** changed symbols | `signals_from_diff` keeps, per non-docs file, the base-side lines each hunk touches (removed lines, and both neighbours of an insertion). A graph node whose span contains a touched line is in C; a touched file with no indexed symbol (a new file) counts as one changed symbol nothing reaches. |
| **R** dependents | `store.traverse(c, TraversalSpec::blast_radius(3))` per seed: callers, importers, and injected-edge consumers (`EdgeKind::Other`) within 3 hops, union over C, minus C. |
| **span** | distinct products (`crates/<x>`, `packages/<x>`, else the root) C and R land in |
| **contract change** | a touched path matches `contract_path_markers` (`api-types`, `/event.rs`, `schema`, `/mcp`, `.d.ts`, …) or a changed **type** matches `contract_symbol_markers` (`event`, `schema`, `tool`, `api`, `request`, `response`, `dto`, `payload`) |
| **G** test gap | the share of C that no test symbol (test path, or `test`-prefixed name) reaches within 3 hops |
| **destructive**, **critical** | the diff-only line and path markers. Destructive stays diff-only so it works with no graph. |

**Deterministic score 0–100:** reach 0 with no changed symbols; else 20 for 0–5 dependents (a behavioural change nothing depends on yet still gets one monitor), 40 for 6–20, 60 for 21–100, 80 above; +10 per product beyond the first (max +20); +20 contract change; +20 × G only when R is non-empty; +20 critical; destructive ⇒ `max(score, 70)`; cap 100. Docs-only scores 0 (no symbols). Every contribution is a line in `Assessment.reasons`.

**Bands → plan:** 0–19 → 0 monitors / none / no post-hoc; 20–39 → 1 / standard / no; 40–69 → 2 / deep / post-hoc; 70–100 → 3 / deep / post-hoc **on a different CLI than the worker** (`post_hoc_other_cli`).

**Model hook (optional, non-deterministic):** `ModelAssessment::assess(&ImpactSignals, deterministic) -> Option<ModelBonus>` may ADD 0, 10 or 20 with a recorded rationale, is consulted only when the deterministic score is ≥ 20, and can never subtract (anything else is clamped). Not wired to a CLI in #600.

**Fail closed, one rule:** the indexed commit must BE the run's base commit (`graph_age`: `repo_info().commit == base_commit`). The run tree is created at `base.commit` (`src/repo.rs:963`, `git worktree add … <base.commit>`), the run diff is `base_commit..run_branch` (`src/actor.rs:1588`), and hunks are mapped by base-side path and line, so the graph's spans must be the base's spans. A graph at a descendant is stale too: an insertion above a hot symbol shifts its span and the base-side lookup misses it (review on #600). The engine never re-indexes at run start (`WorktreeReady`, `src/actor.rs:1534`, only persists `workdir`/`base_commit`); the graph is whatever onboarding indexed the registered root at, which the base lift (`RunBase::lifted`, `src/repo.rs:705`) can leave behind. No graph, an unreadable graph, or any other commit ⇒ score 100 with the reason recorded; `assess` applies the rule itself. There is no quiet fallback to line counts.

**Fixed expectations (tests in `src/review_scale/tests.rs`):** a 10-line change to a symbol with 40 dependents scores 80 untested / 60 tested; a 600-line new leaf file scores 20; an event-schema change with 3 consumers scores 40 (the same change to a plain type: 20); no graph ⇒ 100; a head-indexed graph against a base-side diff (hot symbol shifted by an insertion above it) ⇒ 100, never 20, while the base graph still scores 80; a destructive leaf floors at 70; the model hook can raise but not lower.

## 9. Where each piece lives

| Piece | Repo | Location |
|---|---|---|
| **Step 0 prerequisite:** instance-key ACP lookup + seat-key config home + fence (#591 ACP gap) | core | `src/acp_runner.rs:7755-7761` (`registry_record`), `:2221` (`seat_config_for`) |
| `TeamSupervisor`, `TeamCmd`, `TeamLedger`, confirmation, dedup, `render_for_gate` | core | new `src/team.rs`; constructed in `Core::spawn_with_acp_sessions` (`src/lib.rs:545`), subscribes via `Core::subscribe` |
| `unitCheckpoint` emission, `steering_supported`, steer mailbox + delivery, response matching | core | `src/acp_runner.rs` (`:2645`, `:924`, `:3868`, `:4385`, `:4470`, `:7450`) |
| `monitor_ensure` / `monitor_turn` | core | `src/acp_runner.rs`, beside `chat_turn` (`:6187`) |
| `team::attach` / `team::finish` at the shared seam (every carrier); process-wide supervisor handle | core | `src/cli_runner.rs:748`, `:761` (attach before, finish after); handle installed in `src/lib.rs:545`; bus caller `src/cli_runner.rs:2013`, `:1971` |
| Final pass call, ledger render into WORK, monitor seats excluded from the judge (all three paths) | core | `src/cli_runner.rs:761`, `:885-895`; bus `:97-112`, `:351-394`, `:911-925`; inline `:937`, `:988`, `:1020`, `:1029` |
| `team_dispute` pause: `UnitOutcome.team_pause` set by the fold, performed by the actor | core | fold `src/pipeline.rs:827`, `:1540-1576`; `UnitOutcome` `src/execute.rs:36`; actor `src/actor.rs:5358-5376`, `:5400-5444`; `pause_for_human` `:6071` |
| `team_dispute` resume: `confirm_gate` reads `gate_kind` first; approve emits `Resumed` + `GateDecided` + `UnitDone` and advances the cursor, no re-dispatch | core | `src/actor.rs:7785`, `:7864-7870`, `:8023-8034`, `:8055`, `:8061-8072`, `:8101`; `src/interaction.rs:59-76`, `:161`, `:184` |
| Final-pass timeout synthesis (hold + `no_verdict`/`timeout` for every still-declined HIGH; `[Q4-pending]` every still-unaccepted HIGH) | core | `src/team.rs` final pass, before `team::finish` returns (§4.7) |
| `UnitEvidence.team`, `WorkUnit.team_ledger`, `teamLedger` emission | core | `src/workflow.rs:265`, `src/domain.rs`, `src/pipeline.rs:1540` |
| Ledger into evaluator prior context and rework amendment | core | `src/actor.rs:6650-6700` |
| Six `to_json` arms + core-ts `.d.ts` regen | core | `src/event.rs`, `crates/wicked-core-ts` |
| `GET /api/v1/runs/:id/team`, api-types events + DTO | crew | `packages/crew/src/api/routes.ts`, `packages/crew-api-types/index.d.ts` |
| Gate panel, verdict detail, feed lines | studio | `SteeringGate.tsx`, `VerdictDetail.tsx`, `NarratorFeed.tsx`, `api/client.ts` |

## 10. Build order

0. **Prerequisite (in progress, separate PR): #591 ACP gap.** `registry_record` does a two-step lookup (the exact instance key, then its cli key via `seat_cli_key`). `start_acp_process_with_write_roots` resolves the **seat-key** configuration home, not `seat_config_for(seat_cli)` (`src/acp_runner.rs:7755-7761`, `:2221`). The instance-key fence holds on the ACP path. S2's monitor attach (§4.1) needs it. S3 and S6 do not.
1. **Wire contract first (core, S2-owned).** The six `CoreEvent` variants + `to_json` arms + the `TeamLedger` type, with no emitters. Merge. From here, crew and studio can build against fixtures, and S3 can build against the types.
2. **In parallel, three builders:**
   - **S2 (core):** `unitCheckpoint` emission, `team.rs` supervisor, monitor sessions, batching, snapshot/diff, confirmation, dedup, `monitorFinding`, final pass steps 1–3, `team::attach`/`team::finish` at the shared seam.
   - **S3 (core):** `steering_supported`, mailbox, delivery at the checkpoint arm, response matching, `adviceDelivered`, advice text, `ADVICE` parsing → `workerAdviceResponse` and the hold round (final pass steps 4–5). It is tested against a mock bridge that advertises and answers `_session/steering`.
   - **S6 (core → crew → studio):** ledger persistence, `teamLedger` emission, `render_for_gate` into judge/evaluator/rework, monitor exclusion from the judge, the dispute trigger and the council call (final pass step 6, behind S5's entry point), then the crew route + api-types, then the studio surfaces.
3. **core-ts release → crew → studio** on the normal train (crew bundles studio; bump the studio pin).
4. **Rig proof (acceptance §12-E2E)** after all three land.

The S2/S3 seam is the mailbox type and the `TeamTurn` parameter, both fixed by this document. The S2/S6 seam is `TeamLedger`, fixed in step 1.

## 11. Risks → the mechanism that addresses each

| Risk (#590) | Mechanism | Where |
|---|---|---|
| **1. Stream volume** (380 `unitOutputDelta`/run) | Monitors never see deltas. A batch needs a tree-changing checkpoint **and** 60 s **and** an idle monitor **and** budget, **and** a changed tree id. Hard caps: `MAX_BATCHES`, `DIFF_CAP`. Incremental diffs go to a warm session. Zero monitors (and zero checkpoints) until S4's policy asks for one. | §4.2, §4.3, §4.8 |
| **2. Monitor noise** | The bar drops `low`. Only `high` interrupts the worker. Dedup is on line text across monitors. Answered ids are never re-raised. The `rejected` counters are shown at the gate, so a monitor's noise rate is visible evidence. | §4.6, §5.2, §7 |
| **3. Premature findings** | Monitors see settled diffs, not narration. Every finding must quote the exact line at `path:line` in the snapshot tree (mechanical, no model). It is re-confirmed before injection and again at `T_final`. A finding whose text disappeared becomes `superseded`. | §4.2, §4.6, §5.2, §4.7 |
| **4. Monitors becoming authoritative** | `combine_verdict` never reads a finding or a verdict. Monitors are excluded from judging on every path. The worker may decline. A disputed HIGH goes to a council of non-parties, whose only power is to let the run continue (YES) or send it to a human (NO or no verdict). The human, not the monitor, decides the paused case. | §5.3, §6.2, §6.3, §6.6, §6.7 |
| Advice arriving after the turn → a detached turn | `idleBehavior: "promptRequired"` on every steer. `turn_ended` is disclosed and the advice goes to the gate. | §5.2 |
| Advice for one attempt reaching another | The mailbox is keyed `(run, ord, attempt)`. | §5.2 |
| A monitor writing the worktree | Admitted seats only. The chat boundary makes the worktree read-only. The worktree guard's final look is unchanged. | §4.1 |
| The gate stalled by a slow monitor or council | `FINAL_PASS_BUDGET`, `MAX_DISPUTES` bound the wait. On expiry every still-declined (`[Q4-pending]` still-unaccepted) HIGH is synthesized as held with `no_verdict`/`timeout`, so the fold pauses for a human instead of continuing. The gate never waits unboundedly and never continues unattended past a timeout. | §4.7, §4.8, §6.7 |

## 12. Acceptance (testable)

**S2**
1. A teamed ACP unit on a mock bridge that emits `tool_call` (kind `edit`) then `tool_call_update{status:"completed"}` emits exactly one `unitCheckpoint` with that `kind`, `title` and `paths`. A non-teamed unit emits none.
2. 30 checkpoints inside 60 s with one tree change produce **one** monitor batch. A checkpoint burst with an unchanged tree id produces zero batches.
3. A monitor reply citing `path:line` whose text does not match the snapshot tree emits no `monitorFinding` and increments `rejected.unconfirmed`. A matching reply emits one, with the tree id.
4. Two monitors citing the same line text in the same anchor emit one `monitorFinding`, and the ledger lists the second in `corroboratedBy`. **Collision:** two identical hazardous lines in two functions of one file (fixture: the same `store.erase_scope(scope)?;` in `retire()` and `purge()`) emit **two** `monitorFinding`s with different `findingId`s, the same `lineKey`, and different `anchor`s. A finding whose line moves between batches keeps its `findingId` and gets its `finalLine` updated by `lineKey`. With no graph and no hunk header (a new file), the anchor is `""` and the id still differs from a finding in another file. A `low` finding emits nothing and counts `belowBar`.
5. A monitor candidate equal to the creator instance, or on an unadmitted adapter, yields `monitorAttached{status:"failed"}` and no process. A monitor's `Write` into the worktree is denied by the boundary (the permission answer is the reject option), and the worktree tree id is unchanged.
6. A unit whose diff is docs-only (S4 `monitors: 0`) spawns no monitor process and emits no `monitorAttached`.
8. **A wrapped unit gets the final-pass ledger.** A unit executed by `WrappedCliStepRunner` (a mock CLI binary that writes a tree-changing code diff into the worktree and exits 0), with S4 answering `monitors: 1` and a stub monitor session that returns one confirmable `FINDING`: the run emits **no** `unitCheckpoint` and **no** `adviceDelivered`; after the unit returns it emits `monitorAttached{status:"attached"}`, one `monitorFinding`, and a `teamLedger` with `monitors[0].batches == 1`, `finalPass:"completed"`, and the finding `delivery:"not_delivered"`, `status:"unanswered"` (the approved status of a finding the worker never received, §6.3), `monitorReply:null` (no hold round: the approved hold round covers `declined` findings only, §4.7 step 5), `dispute:null` (not unresolved under the approved reading), and `teamPause:false`, so the unit continues with `gateDecided` + `unitDone`. The judge prompt contains the rendered ledger. `[Q4-pending]`: under the extension the same finding gets a hold-round turn; a `WITHDRAW` ends it `status:"withdrawn"`, a `HOLD` makes it unresolved and takes the §6.3 council path (tests #16 i). The same test on the in-process path and on the bus path (`StepInput` rebuilt at `src/cli_runner.rs:1971`) produces the same ledger. Under `spawn_with_engine` with no supervisor installed, the same unit produces no team events and `UnitEvidence.team` is `None`.

**S3**
7. On a mock bridge advertising `_meta.steering.supported`, a HIGH finding queued before a terminal `tool_call_update` produces exactly one `_session/steering` request with `_meta.steering.idleBehavior == "promptRequired"`, and `adviceDelivered{outcome:"injected"}` when the bridge answers `{"outcome":"injected"}`.
8. The same bridge answering `{"outcome":"promptRequired"}` yields `outcome:"turn_ended"`, the finding is `delivery:"not_delivered"` in the ledger, and **no** further `session/prompt` is sent.
9. A bridge that does not advertise steering receives **no** `_session/steering` frame.
10. A MEDIUM finding is never sent through steering. A finding whose evidence text is gone from the fresh snapshot is not sent and ends `superseded`.
11. Final output lines `ADVICE f-…: DECLINE — campaign.rs:325 documents the exclusion` and `ADVICE f-…: ACCEPT — added AbortController` produce one `workerAdviceResponse` each, with the matching disposition and reason. A delivered id with no line ends `unanswered`. **Hold round:** only monitors that authored a `declined` finding get a hold-round turn, and it lists exactly those ids with the worker's decline reasons. `[Q4-pending]`: the turn also covers `unanswered` with `delivery:"injected"` and `unanswered` with `delivery:"not_delivered"`, each listed with its state ("no answer", or "not delivered — <outcome>"). A monitor whose findings are all `accepted`, `withdrawn` or `superseded` gets no turn. A missing reply for an id records `monitorReply: {kind:"hold", reason:"no reply (counted as hold)"}`. The S2 final review batch prompt contains no decline text.
12. Advice queued for attempt 1 never reaches attempt 2.

**S6**
13. `teamLedger` is emitted immediately before `gateEvaluated` for the same `(session, ord)`, and the unit record carries `team_ledger` after a daemon restart. **Order and `teamPause`:** on a pause the sequence for that ord is exactly `teamLedger{teamPause:true}` → `gateEvaluated` → `awaitingHuman{gateKind:"team_dispute"}`, with no `gateDecided`; on continue it is `teamLedger{teamPause:false}` → `gateEvaluated` → `gateDecided` → `unitDone`. A `teamLedger` never follows its `gateEvaluated`.
14. **Monitor exclusion, all three judge paths.** For a ledger whose findings were authored by `claude#2` (corroborated by `claude#3`), with creator `claude`:
    (a) **inline pinned:** `agent_validate_with_refusals` is called with an excluded set containing `claude#2` and `claude#3`, and neither is selected (roster fixture with only those plus a distinct seat → the distinct seat judges);
    (b) **inline default judge:** the same;
    (c) **bus path:** the published `GateEvalRequest` JSON carries `"excluded_seats":["claude#2","claude#3"]`; a response with `judge_cli: "claude#2"` or `judge_cli: null` folds as a DENY with the fail-closed reason; a response with a distinct `judge_cli` is honoured;
    (d) with an empty ledger, the bus request carries `"excluded_seats":[]` and a `judge_cli: null` response is honoured exactly as today;
    (e) excluding the monitors leaves no eligible seat → `judge_skipped` names the monitors. The judge prompt on (a)–(c) contains the rendered ledger inside the WORK fence.
15. **Council trigger:** a HIGH the worker declined and the monitor held convenes exactly one council. Its input carries the finding, the worker's decline reason, the monitor's reason, the `T_final` hunk at `path:finalLine` and the criterion, and it excludes the creator and the authoring/corroborating monitors. Any one of HIGH / declined / held removed convenes none; an `accepted`, `withdrawn` or `superseded` HIGH, a MEDIUM, or a monitor `WITHDRAW` convenes none. A monitor with no hold-round reply for a declined HIGH counts as held and convenes one. `[Q4-pending]`: any unaccepted HIGH the monitor held convenes one, with the worker's position carried as "no answer" or "not delivered — <outcome>" (tests #16 h/i).
16. **Continue or pause (§6.7).** Fixture: a teamed unit whose floor passes, whose evaluator passes and whose ledger holds one unresolved HIGH. The council result is injected through a stub of S5's entry point.
    (a) **Council YES → no pause:** `teamLedger{teamPause:false}` precedes `gateEvaluated`; `gateDecided{allow:true}` and `unitDone` follow; there is no `awaitingHuman`; the ledger records `dispute.verdict:"yes"`.
    (b) **Council NO → human pause:** `teamLedger{teamPause:true}` precedes `gateEvaluated`, then `awaitingHuman{gateKind:"team_dispute"}` for that ord; the session is `awaiting_human`; there is no `unitDone` and no `gateDecided{allow:true}` for that ord.
    (c) **No verdict → human pause:** for each of `no_quorum`, `seats_benched`, `error`, `timeout` and `cap`, the outcome is the same as (b), with `dispute.verdict:"no_verdict"` and the matching `reason`.
    (d) **A skipped judge can never auto-approve an unresolved HIGH:** repeat (b) and (c) with `judge_skipped = Some(..)` and `agent_verdict = None`, and the outcome is identical: a pause, never `unitDone`. Only (a) continues, and it continues because of the council's YES, not the absent judge.
    (e) The same fixture with the floor failing emits `unitDenied` and no `team_dispute` pause.
    (f) `combine_verdict` receives identical inputs in (a)–(c).
    (g) **Approve never re-dispatches:** approving the `team_dispute` gate through `POST /api/v1/runs/:id/gate` (no amend) emits `resumed`, then `gateDecided{allow:true}` and `unitDone` for that ord; `unit_ix` advances past the unit; **no** `unitDispatched` is emitted for that ord afterwards and `session.attempt` is unchanged. The same fixture with the cursor unit already `Done` proves the `:8023-8034` bump did not fire.
    (j) **Final-pass timeout pauses:** the fixture with a hold round that never answers, and again with a council stub that never returns, under a `FINAL_PASS_BUDGET` shorter than the stub's delay: the ledger records `finalPass:"timed_out"`, the HIGH carries `monitorReply.kind:"hold"` and `dispute:{verdict:"no_verdict", reason:"timeout"}`, and the outcome is a `team_dispute` pause, never `unitDone`. With `judge_skipped` set, identical.
    (k) **Approve with amend reruns:** approving with an amendment routes through `rewind_to_creator` and emits `unitReworkAmended` then `unitDispatched` at the next attempt, as today.
    (h) **Unanswered (extension — blocked on Q4, §13):** the same fixture with the HIGH `delivery:"injected"`, no `ADVICE` line, and the monitor holding (or silent): council NO and every no-verdict reason pause exactly as (b)–(d); council YES continues as (a). With `judge_skipped` set, there is never a `unitDone` without a council YES.
    (i) **Not delivered (extension — blocked on Q4, §13):** the same fixture on a bridge that does not advertise steering (`delivery:"not_delivered"`), and again with a steer answered `promptRequired`: identical outcomes to (h). The council input's worker position reads `"not delivered — …"`.
17. `GET /api/v1/runs/:id/team` returns the ledgers for a teamed run and `units: []` for a run without monitors. The api-types fixture round-trips the Rust `to_json` output for all six events.
18. Studio: the gate panel shows each finding's severity, `path:line`, the worker's disposition and reason, and the council verdict. Approve/reject still go through `POST /api/v1/runs/:id/gate`.

**E2E (rig, after all three).** Re-run the #590 B18 shape: a retire-flow unit that writes a cancellation-free coverage fetch. Pass requires a HIGH `monitorFinding` at that handler's `file:line` **before** the unit's turn ends, an `adviceDelivered{injected}`, a `workerAdviceResponse`, and a `teamLedger` the gate panel renders. Launch alone is not a pass: the run must reach a terminal state.

## 13. Open questions

- **Q1. Resolved (operator, 2026-09-23):** an unresolved HIGH goes to a council. YES continues; NO or no verdict pauses for a human (§6.7).
- **Q2. Monitor independence.** Only claude is ACP-admitted (§2), so every monitor today is `claude#N` reviewing a claude creator. Same model, correlated blind spots. Each instance also needs its own signed-in config home, and #591's per-instance login is out of scope there. Admitting a second adapter to input governance (the codex-acp research is `registry.rs:252-300`) is what makes monitors model-diverse. Until then, independence is instance-level only.
- **Q3. Pre-emptive steering.** The adapter delivers a steer at priority `now`, which **aborts** the current generation (adapter `acp-agent.js:1196-1206`). The client cannot ask for `later`. Every injected HIGH therefore costs an interrupted cycle and a context jolt mid-task. Whether that helps or hurts work quality is an empirical question for the rig. If it hurts, the remedy is to batch HIGH advice to fewer, later checkpoints, not a second mechanism.
- Smaller: `unitCheckpoint` also serves studio (live tool activity) and could be emitted for every ACP unit, but volume argues against it. The per-attempt ledger is lost on a daemon restart mid-unit: findings emitted before the crash survive in the event log only.
- **Q4. PENDING operator decision — the only open blocker, and it blocks part of S6 only.** The ruling named "declined by the worker, held by the monitor". This document extends *unresolved* to every unaccepted HIGH the monitor holds — declined, **unanswered, or never delivered** (§6.3 item 2, §6.7) — because otherwise a skipped judge auto-approves those via `combine_verdict(true, None)`. **Until the operator confirms or narrows this:** builders implement §6.3 with the ruling's own definition (declined-and-held); the unanswered and not-delivered arms of §6.3 item 2 and acceptance #16 (h)–(i) are **blocked** and must not be implemented as approved. Nothing else in S2, S3 or S6 waits on it. This is the one place the pending status is stated. Every clause that depends on it is marked `[Q4-pending]` inline (§3, §4.5, §4.7, §6.3, §6.7, §9, §11, acceptance #11, #15, #16 h/i); text outside those markers is the approved reading.
