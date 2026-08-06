# Spec: DES-002 ACP Elicitation Maps

- **Status:** Implementing
- **Owner:** eugenelim
- **Plan:** [`plan.md`](plan.md)
- **Constrained by:** DES-002-acp-session-elicitation (full feature brief; TS half in wicked-crew)
- **Brief:** none
- **Discovery:** none
- **Contract:** none (internal runtime protocol over stdio; not a public API surface)
- **Shape:** service

> **Spec contract:** this document defines what "done" means. The implementing
> PR must match this spec, or update it. Verification must be derivable from it.

## Objective

Add `elicitation/create` handling to wicked-core's ACP turn loop so that MCP
servers running inside **native ACP adapters only** (`claude-agent-acp`,
`codex-acp`) can ask the operator a question during a tool call and receive a
typed response. wicked-core mints an `elicitationId`, registers it under a
single atomic `ElicitationMaps` lock, suspends the pending ACP request on a
50 ms dual-poll loop (to keep stdout drained), emits `CoreEvent::ElicitationCreated`
to crew via NAPI, and responds to the adapter once `resolveElicitation` delivers
the human answer. All three terminal paths (human dismiss, deadline, adapter
disconnect) terminate the unit without retry via the new `StepStatus::ElicitationFailed`
variant. A full bus-consumer redesign (ack-gated cursor advance, startup cursor
reclamation, predecessor terminalization) ensures correctness across daemon restarts.

agy-acp is explicitly out of scope. Multi-property and non-string schemas are
immediately cancelled before registration.

## Boundaries

### Always do

- Cap inbound `message` at 8 KB (truncate with `[truncated]`); individual `options`
  entries exceeding 512 bytes are **dropped** (not truncated) with `tracing::warn`;
  `options` list capped at 100 entries total.
- Route `StepStatus::ElicitationFailed` to the run-terminal path, bypassing
  `FailureTriageReady` / `Retry` everywhere (actor.rs, cli_runner.rs, exhaustive
  match sites).
- Add `#[serde(default)]` to every new non-optional field on `DispatchedTask`
  and `CompletedTask` so pre-change rows deserialize cleanly.
- When performing startup cursor reclamation: migrate old cursor positions to the
  new consumer **before** deleting old cursor rows; delete old rows only after
  positions are safely migrated; call `set_stable` to record the new owner only
  after both migration and deletion are complete.

### Ask first

- Enabling elicitation for any adapter beyond the `ELICITATION_VERIFIED_ADAPTERS`
  allow-list — requires EC-3 (OQ-R-6 adapter serialization confirmed) and an
  explicit verification artifact (link to passing integration test run or
  source-code audit in the PR description); self-assertion in a PR description
  alone is insufficient.
- Removing or relaxing the `chat_turn` elicitation guard (`elicitation_enabled=false`)
  — requires OQ-R-7 explicitly resolved with a verifiable artifact; cannot be
  removed on the grounds that OQ-R-7 is "probably fine" (EC-6).
- Widening the `requestedSchema` parser to support integer/boolean/multi-property
  schemas — blocked on crew/Studio changes for v2.
- Adding a per-elicitation minimum timeout (`MIN_ELICITATION_SECS`) — named v2
  extension point; skip for v1.

### Never do

- Elicitation for agy-acp — structurally unreachable (stdin is `'ignore'`); do
  not add a code path, even a guarded one.
- Call `cleanup_run` from `on_run_complete` — `EpochCleanup::drop` is the sole
  call site; a second call double-decrements `active_workers`.
- Write `proc.stdin` from any thread other than the turn-loop thread — interleaves
  corrupt JSON-RPC framing (I-10).
- Run `exec_turn_acp` on the actor thread — it blocks; the actor thread must
  remain free to process `Command::ResolveElicitation` (I-9).
- Collapse `elicitation_timed_out` and `cancelled` into one flag — these route
  to different `StepStatus` values (I-7).
- Send the ack reply before the actor commits `ApplyStepResult` to the store —
  premature ack advances the cursor before the result is durable, creating a
  crash window where the task is permanently lost.

## Testing Strategy

| Behaviour | Mode | Test # |
|-----------|------|--------|
| ElicitationMaps register / remove / deliver / cancel_epoch atomicity | TDD | 1–6 |
| Message truncation: 8 KB **byte-length** cap (not character count); UTF-8 multi-byte strings counted by bytes | TDD | 7 |
| Options entry 512-byte byte-length drop; empty-string options drop | TDD | 8–9 |
| `prop_key` preserved from schema (not hardcoded `"response"`) | TDD | 10, 10a |
| ElicitationMaps unit tests (all) | TDD | 1–11, 10a |
| EpochCleanup drop fires cleanup_run and clears bus_in_flight | TDD | 35 |
| exec_turn_acp elicitation arm: schema variants, dual-poll, timeout, cancel, decline | TDD | 12–20 |
| `rpc_respond` echoes `id` verbatim as `Value` for string and numeric IDs (`id != null`, not `as_u64()`) | TDD | 21 |
| `rpc_expect` frame routing: elicitation guard in handshake phase | TDD | 22–24 |
| Tombstone race, epoch separation, gate-eval, cleanup_run reclamation | TDD | 25–36 |
| `session/prompt` usage captured during elicitation **replaces** (not sums) prior `handle_update`-derived token counts | TDD | 37 |
| Degraded-mode bus dispatch (has_activated_seq, worker not in-flight) is ack-gated | TDD | 38 |
| `StepStatus::ElicitationFailed` round-trip + exhaustive match sites | Goal-based | `cargo check` + grep |
| Bus cursor APIs (delete_cursor, get_stable, set_stable, find_completed) | TDD | bus.rs unit tests |
| Predecessor terminalization — completed-stream reconciliation | TDD | Gate tests 25–36 |
| Startup cursor reclamation ordering (old_consumer before set_stable; delete after migrate) | TDD | bus.rs startup unit test |
| Full feature end-to-end: adapter → crew → human → adapter | Manual QA | Deferred to post-integration smoke |

Test files: `src/acp_runner.rs` (inline `#[cfg(test)]`), `src/bus.rs` (inline),
`src/cli_runner.rs` (inline). No new test crate. Tests 37 and 38 are new additions
to the 36-case DES-002-tests.md baseline.

## Acceptance Criteria

- [ ] `elicitation/create` from `claude-agent-acp` or `codex-acp` reaches crew as
      `CoreEvent::ElicitationCreated { session, epoch, elicitation_id, message, options, prop_type }`. (G-1)
- [ ] `resolveElicitation` (NAPI) delivers the human response without blocking the
      stdout pipe; dual-poll loop keeps pipe drained at ≤50 ms intervals. (G-2)
- [ ] `on_run_complete` (fired by cancel, fail, and actor `Shutdown`) releases all
      pending elicitations for the run; no worker thread hangs past the turn deadline. (G-3)
- [ ] `ElicitationMaps` is mutually consistent at all times: an `elicitation_id`
      exists in both `pending` and `run_index`, or in neither. (G-4)
- [ ] `StepStatus::ElicitationFailed` round-trips cleanly through `status_to_str` /
      `status_from_str` with wire token `"elicitation_failed"`, and routes to the
      run-terminal path at every exhaustive match site (no retry). (I-7)
- [ ] All tests pass: ElicitationMaps unit tests (1–11, 10a), arm-level turn tests
      (12–20), `rpc_respond` string-id echo (21), `rpc_expect` frame tests (22–24),
      gate tests (25–36), usage-replace (37), degraded-mode ack-gate (38) — 39 total.
      (EC-4)
- [ ] Pre-change `DispatchedTask` and `CompletedTask` rows (missing `launch_seq`,
      `is_acp`, `process_gen`) deserialize without error and follow the legacy path.
      `#[serde(default)]` on all new non-optional wire fields. (File map: cli_runner.rs)
- [ ] Cursor advances in the bus consumer are gated on ack success at all three
      ack-gated sites: (a) degraded mode (task was activated but no in-flight worker;
      `has_activated_seq=true`, `is_bus_worker_in_flight=false`), (b) predecessor real
      completion (find_completed returns Some), (c) predecessor synthetic terminal
      (find_completed returns None). Normal worker-initiated paths use `ack: None` and
      are NOT ack-gated. Test 38 verifies the degraded-mode path. (Gate-83 correctness)
- [ ] The actor sends the ack reply (`ack.take()`) only **after** `ApplyStepResult`
      is committed to the store — never before; verified by test 38 and the gate
      tests asserting cursor position after a simulated crash between dequeue and commit.
- [ ] `rpc_respond` echoes the inbound JSON-RPC `id` verbatim as a `Value` for all ID
      types (number and string); the guard is `id != null`, not `as_u64()`. An adapter
      using a UUID string as the request id gets the correct response. Test 21 verifies
      this with a string-typed id. (I-1)
- [ ] `session/prompt` usage captured mid-elicitation replaces (does not sum) any
      prior `handle_update`-derived token counts, matching the outer-loop merge behavior.
      Test 37 verifies replace vs. sum. (I-8)
- [ ] `message` exceeding 8,192 bytes (**byte length**, not character count) is
      truncated to 8,192 bytes with `[truncated]` appended. A 4-byte-per-codepoint
      UTF-8 string of 2,049 characters (8,196 bytes) is truncated. Test 7 verifies
      byte-length enforcement. (Trust boundary — message cap)
- [ ] Options entries exceeding 512 bytes (byte length, not character count) are dropped
      with `tracing::warn`; options under 512 bytes are passed through. Empty-string
      options are dropped. Test 8 verifies byte-length enforcement; test 9 verifies
      empty-string drop. (Trust boundary — options cap)
- [ ] Startup reclamation captures `old_consumer` via `get_stable` before calling
      `set_stable`; migrates cursor positions to the new consumer **before** deleting
      old cursor rows; deletes old rows before calling `set_stable`. `predecessor_gen`
      is correctly derived and non-None when a predecessor consumer exists.
      (Gate-83 ordering fix + security-review startup ordering)
- [ ] Concurrent reassignment workers track independently: `bus_in_flight_workers`
      keyed by `(run_id, launch_seq)` pairs. (Gate-83 race fix — type canonical
      definition is in plan Constraints)
- [ ] OQ-R-4 resolved: `clientCapabilities.elicitation.form` emitted as `{"form":{}}`. (EC-1)
- [ ] OQ-R-5 resolved: `params.message` and `params.requestedSchema.properties` paths
      confirmed against SDK v1.3.0 `types.gen.ts`; extraction code matches. (EC-2)
- [ ] OQ-R-6 resolved or guard in place: elicitation enabled only for verified adapters
      in `ELICITATION_VERIFIED_ADAPTERS`; enablement for any new adapter requires an
      explicit verification artifact in the PR. (EC-3)
- [ ] `resolveElicitation` NAPI binding compiles with `"serde-json"` napi feature;
      crew TS wrapper unpacks `result.content?.response ?? null` before calling. (EC — NAPI wiring)
- [ ] **[Blocking pre-merge]** Studio `ElicitationPrompt` escapes `message` before
      render — this is the sole XSS/injection control (no Rust-side sanitization);
      the Rust PR must not merge until the wicked-crew TS PR with this control is
      reviewed and approved. Tracked as a PR pre-condition, not an advisory. (EC-5)
- [ ] `chat_turn` elicitation guard in place (`elicitation_enabled=false`) until OQ-R-7
      is resolved with a verifiable artifact (e.g., passing integration test run or
      source-code audit cited in the PR description). Self-assertion alone is
      insufficient. (EC-6)

## Assumptions

- Technical: root crate uses `std::sync::mpsc` — no Tokio dep; ack channels use
  `sync_channel(0)` (rendezvous). (source: Cargo.toml read + DES-002-tests.md file map)
- Technical: `uuid` v1 with `["v4","serde"]` features and `tracing = "0.1"` are not
  yet in Cargo.toml; must be added. (source: DES-002-tests.md file map)
- Technical: `wicked-core-ts/Cargo.toml` needs `"serde-json"` added to the `napi`
  feature list for `Option<serde_json::Value>` NAPI deserialization. (source: DES-002-tests.md)
- Technical: The bus DB (SQLite) schema requires a `cursor_owners` or `meta` KV
  table for `get_stable`/`set_stable`; schema migration needed. (source: DES-002-actor-teardown.md)
- Technical: `find_completed` must scan (not just peek) the completed stream by
  `(run_id, launch_seq)` without advancing the cursor for non-matching events.
  (source: gate-83 correctness finding)
- Product: agy-acp elicitation deferred; no code path, even guarded. (source: DES-002-overview.md §Non-goals)
- Product: Multi-property / non-string schemas immediately cancelled (v2 extension). (source: DES-002-overview.md §Non-goals)
- Process: All nine source files must compile together atomically (change sequencing in DES-002-tests.md §Change sequencing); implement in dependency order.
