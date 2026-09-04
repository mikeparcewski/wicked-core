# OQ-PI-ACP-001 verdict

**Admission: NOT ADMITTED.** `acp_input_governance` stays `false` for `pi` in
`crates/wicked-council/src/registry.rs`. Property (a) fails; (b) and (c) are moot as a
consequence (there is nothing to disable or reject — the gate never exists).

See `manifest.md` for the exact pinned artifact and `capture-allow.ndjson` /
`capture-reject.ndjson` for the raw (redacted) frames this verdict is based on.

## Property (a): does every tool intent produce a blocking `session/request_permission`?

**FAIL.**

Static evidence (source read at the pinned `gitHead`, `pi-acp` repo cloned locally for
this evidence run):

- `src/acp/session.ts` (pi-acp source, not vendored into this repo) handles pi's
  `read`/`write`/`edit`/`bash` tool events by emitting ACP `tool_call` /
  `tool_call_update` notifications directly as pi executes them (~lines 540-770 of the
  cloned source). There is no call to `conn.requestPermission` anywhere on that path.
- `conn.requestPermission` (ACP `session/request_permission`) is called from exactly one
  place in the whole adapter: `requestExtensionPermission` (~line 950), which only
  serves pi's own extension `select`/`confirm` UI events — an unrelated, opt-in
  extension mechanism, not a gate in front of the built-in tools.

Runtime evidence (live capture, `capture-allow.ndjson`, this directory):

- Frame 9 (`agent->client`, `session/update`): `tool_call`, `toolCallId
  toolu_011hwktdozkBCraAyoMnuGqt`, `title: "write"`, `kind: "edit"`, `status:
  "pending"`, `rawInput: {"path":"marker-allow.txt","content":"OK"}`.
- Frame 10, 128ms later: `tool_call_update`, same `toolCallId`, `status:
  "in_progress"` — **no `session/request_permission` request appears between frames 9
  and 10** (or anywhere else in the 16-frame capture: `requestPermissionCallCount: 0`).
- Frame 11: `status: "completed"` with a diff showing the file content written.
- The marker file was confirmed present on disk with the exact expected content
  immediately after.

So the tool call was never blocked awaiting a client decision — it went straight from
"pending" to "in_progress" to "completed" while pi executed it locally, and the ACP
client (our harness) was only ever a passive observer of `tool_call`/`tool_call_update`
notifications. `pretool_payload` (`src/acp_permission.rs:56`) is never invoked for this
adapter today because the platform's own gate (`acp_runner.rs`) only calls it in
response to an actual incoming `session/request_permission`, which pi-acp never sends
for its core tools.

The one identity/rawInput compatibility question the design phase raised — "title vs.
canonical tool name" — turned out moot: the `tool_call` frame's `title` field for the
write tool *is* the canonical name (`"write"`), and `rawInput` is the original,
unmodified tool argument object (`{path, content}`). Had permission requests existed,
they would likely have carried compatible identity. That question is now academic
because no permission request is ever sent for these tools in the first place.

## Property (b): identify the adapter's default/auto-approve control and prove it can be disabled

**FAIL — no such control exists to test.**

- `pi --help` (installed `pi` 0.84.2) lists no tool-execution approval flag. The only
  related flag, `--approve, -a`, is documented as "Trust project-local files for this
  run" — it governs whether pi trusts project-local config/extension files, not whether
  a tool call requires confirmation before running.
- pi-acp's own source has no auto-approve/yolo setting either; it simply never wires a
  permission check in front of `read`/`write`/`edit`/`bash` at all (see property (a)).

There is no default-permission or auto-approve toggle because there is no permission
gate to toggle. This isn't "the control exists and is enabled"; it's "the control does
not exist." Recorded as a failed property per the design phase's instruction ("if no
such control exists, record that as a failed property").

## Property (c): prove a selected reject prevents the tool action

**FAIL — untestable because no permission request ever arrives to reject.**

`capture-reject.ndjson` used the same harness configured to select the
`reject_once`/`reject_always` option for any `session/request_permission` it receives.
None arrived (`requestPermissionCallCount: 0`, same as the allow run). The tool call
sequence is byte-for-byte structurally identical to the allow run: `pending` ->
`in_progress` -> `completed`, and `marker-reject.txt` was written to disk with the exact
requested content regardless of what the client's stance would have been. The client
never gets a chance to say no.

## Overall

All three properties fail. Per the design phase's explicit instruction ("If the
expected missing-permission capture occurs, record property (a) as failed, leave Pi
disclosed-ungoverned, and do not alter its admission tests or configuration"), Pi's
`AcpConfig.acp_input_governance` stays `false`. No admission test needed a change:
`only_claudes_proven_acp_adapter_is_admitted_in_the_builtin_roster`
(`crates/wicked-council/src/registry.rs`) already asserts only Claude is admitted among
all built-ins, including `pi`, and continues to pass unmodified. The registry comment
next to Pi's `AcpConfig` was updated to cite this evidence instead of pointing at an
open question, since the question is now answered (as a failure, not a pass).

This verdict is scoped to `pi-acp@0.0.32` (`gitHead 2f6e3c5`) exactly, per
`manifest.md`. Because the owning dependency is a semver range (`^0.0.32`), a future
version bump is not automatically covered by this evidence and should be re-verified
before ever flipping `acp_input_governance` for `pi`.

## Forward path to a governed `pi`

The negative verdict is not a dead end — the *seam already exists in the adapter*. In
`pi-acp@0.0.32` (`gitHead 2f6e3c5`), the shipped `dist/index.js` already implements a
full `session/request_permission` round-trip: `requestExtensionPermission` calls
`this.conn.requestPermission({ sessionId, toolCall, options })` and blocks on the
client's `selected`/`rejected` outcome. The single reason `pi` runs ungoverned is that
this machinery is wired **only** to pi's extension `select`/`confirm` UI events, never
to the core `read`/`write`/`edit`/`bash` tool path (which emits `tool_call` /
`tool_call_update` notifications directly as pi executes, with no permission gate).

So admitting `pi` requires one of two concrete changes, either of which would let this
same evidence harness observe a blocking `session/request_permission` before every tool
execution and re-run to a PASS:

1. **Upstream contribution to pi-acp** — route core tool intents through the existing
   `conn.requestPermission` seam before execution (emitting a canonical tool name in
   `toolCall` and the original args in `rawInput`, which the `tool_call` frames already
   carry today), and await the outcome before the `pending -> in_progress` transition.
2. **A wicked-owned ACP bridge** that sits between the platform and pi, intercepts pi's
   tool intents, and issues the `session/request_permission` round-trip itself before
   forwarding execution — keeping governance under wicked's control regardless of
   upstream.

Either path is compatible with the platform's existing gate: `acp_permission::pretool_payload`
(`src/acp_permission.rs:56`) already consumes exactly the `{canonical tool name, rawInput}`
shape the current `tool_call` frames prove pi-acp produces. Once a blocking permission
request carries that payload, flipping `acp_input_governance = true` (with a fresh
capture recorded here) becomes justified.
