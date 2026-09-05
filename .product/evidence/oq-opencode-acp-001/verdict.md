# OQ-OPENCODE-ACP-001 verdict

**Recommended admission: NOT ADMITTED.** `acp_input_governance` stays `false` for opencode's built-in
`AcpConfig`. This clarify phase produced this evidence and recommendation; any registry comment
update is deferred to a later, implementation phase (`acp_input_governance` itself is not touched).
See `manifest.md` for the exact pinned artifact and the eight `*.ndjson` capture files for the raw
(redacted) frames this verdict is based on.

## Candidate adapter identification (prerequisite check)

A viable candidate exists and needs no separate identification step: opencode speaks **native** ACP
over stdio via its own `acp` subcommand (`opencode acp`) — there is no third-party bridge package the
way `codex-acp`/`pi-acp` are separate npm packages. The registry's built-in `AcpConfig` for `opencode`
(`crates/wicked-council/src/registry.rs:331-338`) already invokes exactly this: `binary: "opencode"`,
`start_args: ["acp"]`. This evidence run used the same invocation, verbatim (`opencode acp --cwd
<fixture>`; `--cwd` only pins the working directory, see `manifest.md`), on the installed CLI
(`opencode 1.17.18`, tag `v1.17.18` / commit `b1fc8113948b518835c2a39ece49553cffe9b30c`, see
`manifest.md`). So this OQ proceeds to the live-capture proof.

## Property (a): does every core tool intent produce a blocking `session/request_permission` with a canonical name + raw input?

**FAIL — under the registry's actual, unmodified invocation, nothing is ever asked for
read/edit/bash/write inside the working directory. The permission machinery is real, present, and
independently provable, but the shipped default ruleset resolves every in-workspace core intent to
`allow` before that machinery is ever engaged.**

### Default invocation: zero gating for every core intent, proven at the source-code level

`capture-allow.ndjson` and `capture-reject.ndjson` — the identical four-step read/edit/bash/write turn
under the registry's exact invocation, differing only in whether the harness would allow or reject any
incoming request — are **byte-for-byte behaviorally identical**: `requestPermissionCallCount: 0` in
both, both leave `seed.txt` edited and the marker file created. `probe-risky.ndjson` (`rm -rf` on a
scratch subdirectory) and `probe-network.ndjson` (`curl` to an external host) are the same: zero
requests, the destructive deletion and the network fetch both complete unconditionally.

This is not a silent internal-reviewer short-circuit the way `codex-acp`'s `approvalsReviewer:
"auto_review"` was (`oq-codex-acp-001/verdict.md`) — this evidence traced the actual mechanism to
source. `opencode acp --print-logs --log-level DEBUG` (used only for this diagnostic step, not part
of the registry's invocation) logs every permission evaluation; for all four steps of the default-
invocation turn the log read `message=evaluated permission=<read|edit|bash>
pattern=<file-or-command> action.permission=* action.action=allow action.pattern=*`. Cross-referenced
against the pinned commit's `packages/opencode/src/agent/agent.ts`, the default `"build"` agent
(opencode's own default, primary agent — not something the registry selects or could avoid) merges a
hardcoded base ruleset:
```
Permission.fromConfig({
  "*": "allow",
  external_directory: { "*": "ask", ...whitelistedDirs: "allow" },
  read: { "*": "allow", "*.env": "ask", "*.env.*": "ask", "*.env.example": "allow" },
  question: "deny", plan_enter: "deny", plan_exit: "deny", doom_loop: "ask",
})
```
Every core intent this OQ cares about (`read`, `edit`, `bash`) resolves to the wildcard `"*": "allow"`
entry unless a project's own `opencode.json` overrides it — which the registry's invocation does not
supply. `Permission.ask()` (`packages/opencode/src/permission/index.ts`) returns immediately without
ever publishing a `permission.asked` event when every matched rule is `"allow"` — so the ACP
`Subscription`'s permission `Handler` (`packages/opencode/src/acp/permission.ts`) is never even
invoked for these calls. This is a genuinely different failure shape from `pi` (no permission wiring
exists at all) and from `codex` (wiring exists but an internal risk-reviewer resolves it before the
client is asked): opencode's wiring is real, general, and configurable — it is the *default data*
fed into that wiring that resolves to allow-everything for in-workspace core intents.

### The one thing that IS gated by default: `external_directory`

`probe-outside-read.ndjson` shows a read of a path **outside** the fixture's working directory
produces exactly one `session/request_permission` (`kind: "other"`, since this synthesizes a distinct
`external_directory` permission rather than a plain `read`), carrying `rawInput: {filepath,
parentDir}` for the real outside path. This matches the source's `external_directory: {"*": "ask",
...}` default and is the same practical shape `copilot` and `codex` both exhibit for out-of-workspace
reads — the one boundary case every seat evaluated so far treats specially. Harness answered
`allow_once` and the read completed. This confirms the machinery genuinely works end-to-end for the
one intent class that IS gated by default; it just is not the intent class this OQ's "every core
tool intent" bar cares about most (in-workspace read/edit/bash).

### Identity/rawInput compatibility with `acp_permission::pretool_payload`

`pretool_payload` (`src/acp_permission.rs:56`) resolves `tool_name` via the fallback chain
`toolName` (top-level) → `toolCall.name` → `toolCall.title`. Every `session/request_permission`
captured here (see the raw frames in `capture-strict-allow.ndjson`, where gating is active — see
Property (b)) carries **no top-level `toolName`** and **no `toolCall.name`** — only `toolCall.title`:
the bare absolute file path for edit/write (e.g. `.../seed.txt`, `.../marker-allow.txt`), the literal
shell command for bash (`"echo hello-from-opencode"`, `"rm -rf sub"`), and — because permission is
asked before the tool's own input metadata is attached — literally the string `"read"` for the read
case (a coincidental match with the tool name, not a documented guarantee: `permissionTitle()`'s
`editTitle()` path falls through to the bare `toolName` string whenever `input.filePath` is not yet
populated at ask-time, per `packages/opencode/src/acp/permission.ts`/`tool.ts`). `pretool_payload`
would therefore resolve `tool_name` to one of these free-text titles — the same class of gap
`oq-codex-acp-001/verdict.md` and `oq-copilot-acp-001/verdict.md` both found. `toolCall.kind` *is* a
small, stable enum (`"read" | "edit" | "execute" | "other"` observed here) that would make a
genuinely canonical identity, but `pretool_payload` does not consult `kind` today.

## Property (b): identify the adapter's default/auto-approve control and prove it can be disabled

**Inverted shape from `codex`/`copilot`/`pi`, PASS at the config level, but the control is not part of
the registry's actual invocation.** Unlike those three seats — each of which ships a documented flag
that is *off* by default and, if turned *on*, would suppress otherwise-real gating — opencode's
default posture *is already* "allow everything in-workspace" with no flag needed to reach it (the CLI
top-level `--auto` flag exists but is a `run`/TUI-mode option; `AcpCommand`'s own yargs builder
(`packages/opencode/src/cli/cmd/acp.ts`) accepts only network options and `--cwd`, and its handler
never reads `args.auto`, so passing `--auto` to `opencode acp` has zero effect — there is no
"auto-approve flag" to find and disable for the ACP path specifically).

The real, documented control is opencode's own `permission` config object (`opencode.json`'s
`permission` field, or the equivalent in the global `~/.config/opencode/opencode.jsonc`), merged
**after** (and therefore able to override) the `"build"` agent's hardcoded `"*": "allow"` default
(`Permission.merge(defaults, ..., user)` in `packages/opencode/src/agent/agent.ts`, where `findLast`
semantics mean a later-merged, matching rule wins). `capture-strict-allow.ndjson` proves this
empirically: adding a project-level `opencode.json` with
`{"permission": {"read": "ask", "edit": "ask", "bash": "ask"}}` to an otherwise-identical fixture
flips `requestPermissionCallCount` from `0` (default) to `4` (every one of the four steps), each
carrying real `rawInput`/`locations`/diffs. This demonstrates the permission machinery is fully
general and config-driven, not hardwired to allow — the same "genuine, working gating exists and can
be proven" property `copilot` demonstrated for edit/execute — but **the registry's actual built-in
invocation supplies no such config**, so out of the box this control is not merely off, it is absent
from the seat's configuration entirely. This is a materially different (and arguably more concerning)
shape than `copilot`'s off-by-default flag: there, an operator must take a deliberate, visible action
(add `--allow-all-tools` to `start_args`) to lose gating; here, an operator must take a deliberate,
*less visible* action (author a project `opencode.json`) to **gain** gating that the CLI does not
provide out of the box.

## Property (c): prove a selected reject prevents the tool action

**PASS, but only demonstrable under the strict (config-gated) invocation — meaningless under the
registry's actual default invocation, where reject and allow are behaviorally identical because
nothing is ever asked** (`capture-allow.ndjson` vs. `capture-reject.ndjson`: zero requests in both,
identical outcome).

Under the strict config: `capture-strict-reject.ndjson` shows the model's first tool call (`read`)
receive a `session/request_permission`, the harness reply `reject`, and the tool call's own
`tool_call_update` resolve to `status: "failed"` — the model then stopped the turn entirely rather
than attempting edit/bash/write (this is model behavior, not evidence about those other intents'
gating). To isolate a mutating/executing intent independent of that stop-after-first-failure
behavior, `probe-strict-reject-bash.ndjson` runs a single, self-contained `rm -rf sub` turn under the
same strict config with every request rejected: the request arrives (`kind: "execute", title: "rm -rf
sub"`), the harness rejects it, the `tool_call_update` resolves to `status: "failed"`, and
`subDirStillExists: true` — **the destructive command was never actually executed**. This is clean,
direct, positive evidence that a reject genuinely prevents the underlying action once gating is
active, for both a read and a bash-class intent.

## Overall

Property (a) fails at the OQ's literal bar for the registry's actual, unmodified invocation: the
default `"build"` agent's baked `"*": "allow"` ruleset means read/edit/bash calls inside the working
directory are never gated, and reject is consequently a no-op in that configuration
(`capture-allow.ndjson` ≡ `capture-reject.ndjson`). This produces the same practical outcome as `pi`
(every core intent proceeds unconditionally) even though the underlying cause is entirely different —
opencode's permission machinery is real, general, config-driven, and independently provable
(`capture-strict-allow.ndjson`/`capture-strict-reject.ndjson`/`probe-strict-reject-bash.ndjson`), it
simply is not what the registry's current invocation exercises. Both facts matter for the forward
path (below). The registry's existing `acp_input_governance: false` for `opencode`
(`crates/wicked-council/src/registry.rs:331-338`) is therefore still correct and should be left
unchanged by any implementation phase that acts on this recommendation; the comment there should be
updated to cite this evidence directory instead of "must prove permission coverage before admission"
(a later, implementation-phase edit, not made here per phase scope).

## Forward path to a governed `opencode`

Unlike `pi`/`codex`, where the gap is either absent wiring or an internal reviewer with no config
lever, opencode's gap has a documented, one-line fix available **today**, entirely on the wicked side:
the registry's `AcpConfig.start_args` for `opencode` could ship a `--config <path>` (or a project-root
`opencode.json`) that sets `permission: {"read": "ask", "edit": "ask", "bash": "ask", ...}` for every
governed unit's working directory, which `capture-strict-allow.ndjson` and
`probe-strict-reject-bash.ndjson` already prove produces real, reject-honouring gating for every core
intent this OQ tests. Two things would need to happen before that flip is safe to make:

1. **A fresh capture against the exact config the registry would ship**, including the risky/network
   probes this evidence ran only against the default invocation (`probe-risky.ndjson`/
   `probe-network.ndjson` should be re-run against the strict config to confirm destructive/network
   bash-class calls are gated there too, not just the plain `echo`/`rm -rf sub` cases already proven).
2. **Close the canonical-identity gap** (Property (a)'s last section) — the same shape of fix
   `oq-codex-acp-001/verdict.md` and `oq-copilot-acp-001/verdict.md` both proposed: a wicked-side
   normalization keyed on `toolCall.kind` (a small, stable, already-present enum:
   `read`/`edit`/`execute`/`other`) instead of `toolCall.title`, ahead of `pretool_payload`, for any
   adapter whose requests lack a canonical name.

Both are narrower, more mechanical gaps than `pi`'s "no wiring exists" or `codex`'s "an internal
reviewer overrides the client" — opencode is the first of the four seats evaluated where the
forward path is "ship a config the wicked side controls" rather than "wait for or request an upstream
change".

## Registry disposition (deferred to the implementation phase)

This clarify-phase deliverable makes **no edit** to `crates/wicked-council/src/registry.rs`. If this
recommendation is accepted, the implementation phase should: (1) leave `acp_input_governance: false`
on the `opencode` built-in `AcpConfig` unchanged, and (2) replace the two comment lines at
`registry.rs:330` and `registry.rs:336` (currently "opencode speaks NATIVE ACP over stdio (`opencode
acp`) — no bridge needed." and "OQ-OPENCODE-ACP-001 must prove permission coverage before
admission.") with a summary
citing `.product/evidence/oq-opencode-acp-001/` and the specific default-ruleset finding above,
mirroring the comment style already present for `codex` (`registry.rs:207-231`), `pi`
(`registry.rs:251-266`), and `copilot` (`registry.rs:286-309`). No test change is required: the
built-in-roster assertion that only `claude` carries `acp_input_governance: true` already asserts
`opencode` is unadmitted and continues to pass unmodified.
