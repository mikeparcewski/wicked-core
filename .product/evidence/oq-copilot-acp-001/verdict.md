# OQ-COPILOT-ACP-001 verdict

**Recommended admission: NOT ADMITTED (for now) — but this is a materially different, much
closer-to-passing result than `codex`/`pi`.** This is a **clarify-phase** deliverable: it records
evidence and a recommendation only. `acp_input_governance` for `copilot` in
`crates/wicked-council/src/registry.rs` (the copilot seat entry) is **not modified by this work** — that edit (if this
recommendation is accepted) is applied in this same change — see the copilot seat entry in `crates/wicked-council/src/registry.rs`. See `manifest.md` for
the exact pinned artifact and the six `*.ndjson` capture files for the raw (redacted) frames this
verdict is based on.

## Candidate adapter identification (prerequisite check)

A viable candidate exists and needs no separate identification step: Copilot speaks **native** ACP
over stdio via its own `--acp` flag (`copilot --acp`) — there is no third-party bridge package the
way `codex-acp`/`pi-acp` are separate npm packages. The registry's built-in `AcpConfig` for
`copilot` (`crates/wicked-council/src/registry.rs:288-295`) already invokes exactly this: `binary:
"copilot"`, `start_args: ["--acp"]`. This evidence run used the same invocation, verbatim, on the
installed CLI (`GitHub Copilot CLI 1.0.83`, see `manifest.md` — newer than the `v1.0.75` the
registry comment cites). So this OQ proceeds to the live-capture proof.

## Property (a): does every core tool intent produce a blocking `session/request_permission` with a canonical name + raw input?

**PARTIAL — edit and bash-class intents pass cleanly on every occurrence tested; the read intent
does not, for in-workspace reads specifically; and the identity carried is not a stable canonical
name.**

### edit and bash-class (execute): consistent, real gating

Across `capture-allow.ndjson`, `capture-reject.ndjson`, `probe-risky.ndjson`, and
`probe-network.ndjson` — six independent tool calls covering an ordinary file edit, a file create, an
ordinary shell echo, a destructive `rm -rf`, and a `curl` network fetch — **every single one**
produced a blocking `session/request_permission` request before the tool call transitioned past
`pending`, carrying the real, unmodified tool arguments:

- edit calls: `rawInput: {fileName, diff}` (a real unified diff of the actual change) plus a
  `locations: [{path}]` array with the real absolute path.
- execute calls: `rawInput: {command, commands: [command]}` with the real, literal shell command
  (not redacted/summarized) — including for the `rm -rf sub` destructive probe and the `curl -sI
  https://example.com` network probe. Neither the risky deletion nor the network fetch was resolved
  by an internal reviewer the way `codex-acp`'s `approvalsReviewer: "auto_review"` resolved its own
  `rm -rf` probe (`oq-codex-acp-001/verdict.md` probe 5, `Risk: medium … Approved` with zero
  `session/request_permission` round-trips). Here the harness's `requestPermissionCallCount` was
  exactly 1 for each single-command probe, and the action only proceeded after the harness answered
  the request — this is genuine, working, per-call client-answerable gating, not an internally
  pre-resolved risk assessment.

This is a stronger result on this axis than either prior OQ: `pi` never sent a single permission
request for any core tool (`requestPermissionCallCount: 0` throughout); `codex`'s default mode also
never did (`0` across five captures, including its own `rm -rf` probe). Copilot's default invocation
— the exact one the registry already uses — sent **3 requests per 4-step turn** (both edits + the
bash step) and **1 request per single-command probe** (echo, `rm -rf`, `curl`), one for every
mutating/executing call observed.

### read: gated only outside the session's trusted directory, not inside it

`capture-allow.ndjson` and `capture-reject.ndjson` both include a `read` tool call ("Viewing
seed.txt") that goes straight from `pending` to `completed` with **zero** permission requests —
consistent across both runs. `probe-outside-read.ndjson`, which asks Copilot to read a file at an
absolute path outside the session's cwd, shows the opposite: a `session/request_permission` **does**
arrive, `kind: "read"`, `title: "Access paths outside trusted directories"`, `rawInput: {path:
"<the real absolute path>"}` — and, notably, the request is for the *harness's absolute-path
rewrite* of the same read the model first attempted with the caller-given relative-looking path
(the tool_call sequence shows two `read` tool_calls: the first `failed`, the second — reissued
against the resolved outside path — is the one that was gated and, once allowed, `completed`).

This matches Copilot's documented path-permission model (`copilot help permissions`): "By default,
file access is restricted to paths within the current working directory and its subdirectories,
plus the system temporary directory" — reads *inside* that trusted scope are treated as
pre-authorized and never reach the ACP client at all; only reads *outside* it are gated. This
platform's own `gate_hook` boundary model cares precisely about this same distinction (`Read` calls
outside the worktree boundary are denied, e.g. `boundary_denial(&ctx("/etc/passwd"), "Read")` in
`src/gate_hook.rs`), so Copilot's out-of-scope-read gating is not accidental alignment — it is the
one case that matters most for *boundary* enforcement, and it works. But it is not "every tool
intent": a workspace-scoped read *policy* (e.g. "deny reading `.env`", "audit every file read") would
never see an in-workspace read from Copilot, because it never reaches `session/request_permission`
at all. The OQ's bar, and this task's instruction, is explicit — "every CORE tool intent
(read/write/edit/bash-class)" — and the plain in-cwd read fails that bar twice, deterministically,
across two independent captures.

### Identity/rawInput compatibility with `acp_permission::pretool_payload`

`pretool_payload` (`src/acp_permission.rs:56`) resolves `tool_name` via the fallback chain
`toolName` (top-level) → `toolCall.name` → `toolCall.title`. Every `session/request_permission`
captured here (see the raw frames in `capture-allow.ndjson`) carries **no top-level `toolName`** and
**no `toolCall.name`** — only `toolCall.title`, a per-call human-readable label: `"Create file"`,
`"Edit file"`, `"Echo test string"`, `"Remove the sub directory"`, `"Fetch headers from
example.com"`, `"Access paths outside trusted directories"`. `pretool_payload` would therefore
resolve `tool_name` to one of these free-text titles — a different string on nearly every call, not
a stable canonical tool identity. This is the same class of gap `oq-codex-acp-001/verdict.md` found
(§ "identity/rawInput compatibility question"), though Copilot's titles are somewhat more
templated/generic than codex's literal-shell-command titles. `toolCall.kind` *is* a small, stable
enum (`"read" | "edit" | "execute"` observed here) that would make a genuinely canonical identity —
but `pretool_payload` does not consult `kind` today, only `toolName`/`name`/`title`. A policy keyed
on canonical tool identity (e.g. "deny Bash", "deny Write") would see inconsistent free-text
`tool_name` values from Copilot today, independent of the read-gating gap above.

## Property (b): identify the adapter's default/auto-approve control and prove it can be disabled

**PASS.** The control is documented and named exactly: `--allow-all-tools` (equivalently
`--allow-all`/`--yolo`, which also disable path/URL restrictions), env `COPILOT_ALLOW_ALL`
(`copilot --help`, `copilot help permissions`). `capture-allow-all-tools.ndjson` reproduces the
identical four-step turn with `--allow-all-tools` added to the spawn: `requestPermissionCallCount`
drops from `3` (the default-invocation `capture-allow.ndjson`) to `0`, and all four tool calls
complete without a single client round-trip — confirming this flag is in fact the mechanism that
suppresses per-call gating, exactly as documented.

Critically, the registry's actual built-in invocation (`crates/wicked-council/src/registry.rs` (the copilot seat entry),
`start_args: ["--acp"]`) does **not** pass `--allow-all-tools`, `--allow-all`, `--yolo`, or set
`COPILOT_ALLOW_ALL` — so the control is disabled by default in the exact configuration a governed
wicked-council seat would actually run under, and `capture-allow.ndjson`/`capture-reject.ndjson`
(both spawned with the registry's exact invocation, no extra flags) are the proof that this default
state genuinely produces blocking requests rather than merely being "off" in name. This is a cleaner
result than `codex`'s: codex's literal auto-approve token (`AskForApproval: "never"`) was also off by
default, but a *second*, undocumented-to-the-OQ auto-approve mechanism
(`approvalsReviewer: "auto_review"`) sat in front of it and could not be disabled independently of a
mode swap that didn't help either. No second mechanism was observed here for edit/execute — the
default invocation's gating for those two classes held across every probe run (ordinary, destructive,
and network-adjacent).

## Property (c): prove a selected reject prevents the tool action

**PASS — cleanly, and directly, unlike `pi`/`codex` where this was untestable.**
`capture-reject.ndjson` used the harness configured to select the `reject_once` option for every
incoming `session/request_permission`. Result: `markerExists: false` (the marker file was never
created), `seedContents` unchanged from the original seed (`seed.txt` was never edited), and all
three gated tool calls (create, edit, execute) show `status: "failed"` in their `tool_call_update`
frames. The read tool call still completed (consistent with the property (a) finding that reads
inside the workspace are never gated at all — reject has nothing to intercept there), but every
intent that *was* gated was genuinely blocked by the reject. This is the only one of the three
platform seats evaluated so far (`pi`, `codex`, `copilot`) where property (c) could be tested at all,
because it is the only one where a permission request reliably arrives for the mutating/executing
intents in the first place.

## Overall

Property (a) does not clear the OQ's literal bar ("every" core tool intent) because of the
in-workspace-read gap and the free-text (not canonical) tool-name identity — so the recommendation
is **NOT ADMITTED** under the same rigor `oq-codex-acp-001` and `oq-pi-acp-001` applied. But this
adapter is qualitatively closer to admissible than either: edit and bash-class intents are gated on
every observed occurrence (ordinary, destructive, and network-adjacent) with real rawInput, the
auto-approve control is off by default in the registry's actual invocation and provably suppresses
gating when turned on, and — uniquely among the three seats evaluated — a reject was proven to
actually prevent the action. The registry's existing `acp_input_governance: false` for `copilot`
(`crates/wicked-council/src/registry.rs` (the copilot seat entry)) is therefore still correct and should be left
unchanged by any implementation phase that acts on this recommendation; the comment there should be
updated to cite this evidence directory instead of "must prove permission coverage before
admission" (a later, implementation-phase edit, not made here per phase scope).

## Forward path to a governed `copilot`

Two independent, non-mutually-exclusive gaps, both narrower than `codex`'s or `pi`'s:

1. **Close the in-workspace-read gap.** Either (a) accept that in-workspace reads are
   pre-authorized by the CLI's own trusted-directory model and scope the admission claim to
   "mutating/executing intents are gated; reads are gated only at the workspace boundary" — a
   deliberate, reviewable narrowing of what "every tool intent" means for this seat, consistent with
   how `gate_hook`'s own boundary model already treats in-boundary reads as unremarkable — or (b)
   find/request a Copilot CLI flag or setting that forces per-call read confirmation even inside the
   trusted directory (none was found in `copilot --help`/`copilot help permissions` at this pinned
   version) and re-capture. Path (a) is a policy/scope decision for whoever owns the admission
   predicate, not a technical fix; path (b) may not exist upstream.
2. **Fix the tool-name identity gap.** A wicked-side normalization layer keyed on `toolCall.kind`
   (a small, stable, already-present enum: `read`/`edit`/`execute`) instead of `toolCall.title` would
   give `pretool_payload` (or a Copilot-specific pre-step before it) a genuinely canonical identity.
   This is the same shape of fix `oq-codex-acp-001/verdict.md` proposed for codex's title-as-shell-
   command problem, and is independent of gap 1.

Unlike `pi` (no permission plumbing exists at all) and `codex` (plumbing exists but is
short-circuited by an internal reviewer for the exact intents that matter), Copilot's gap is narrow
enough that a follow-up capture after either fix — or after an explicit policy decision to accept
read exemption within the trusted workspace — could plausibly flip this OQ to ADMITTED without
requiring an upstream adapter change.

## Registry disposition (deferred to the implementation phase)

This clarify-phase deliverable makes **no edit** to
`crates/wicked-council/src/registry.rs`. If this recommendation is accepted, the implementation
phase should: (1) leave `acp_input_governance: false` on the `copilot` built-in `AcpConfig`
unchanged, and (2) replace the comment at `registry.rs:286-293` (currently "OQ-COPILOT-ACP-001 must
prove permission coverage before admission") with a summary citing
`.product/evidence/oq-copilot-acp-001/` and the specific gaps above, mirroring the comment style
already present for `codex` (`registry.rs:207-231`) and `pi` (`registry.rs:251-266`). No test change
is required: the built-in-roster assertion that only `claude` carries `acp_input_governance: true`
already asserts `copilot` is unadmitted and continues to pass unmodified.
