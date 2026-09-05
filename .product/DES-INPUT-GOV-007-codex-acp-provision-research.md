# DES-INPUT-GOV-007 — OQ-CODEX-ACP-PROVISION-001: does codex-acp expose a provisionable route to client-side approval?

**Issue:** wicked-core #367 (follow-up to OQ-CODEX-ACP-001).
**Phase:** clarify (research + analysis + forward-path design). **No production code in this
deliverable** — `crates/wicked-council/src/registry.rs`'s `codex` `AcpConfig` is unchanged; the flag
stays `acp_input_governance: false`, exactly as `DES-INPUT-GOV-003` left it.

**Predecessors:**
- DES-INPUT-GOV-001 (recon, #360) — defines the OQ proof bar (blocking request / canonical name +
  rawInput / reject honoured / disableable auto-approve).
- DES-INPUT-GOV-002 (#364) — the per-seat `acp_input_governance` capability, defaults OFF.
- **DES-INPUT-GOV-003 (codex, #367)** — resolved OQ-CODEX-ACP-001 NOT ADMITTED under the registry's
  *unconfigured* invocation. Five live captures against pinned `codex-acp@1.9.0`
  (`.product/evidence/oq-codex-acp-001/`) showed the adapter's default `AgentMode.Agent`
  (`approvalsReviewer: "auto_review"`) self-approve every core intent — including an explicit
  `rm -rf` its own internal reviewer logged as "Risk: medium … Approved" — with **zero**
  `session/request_permission` round-trips. §6 there filed this open question: does codex/codex-acp
  expose *some* config/flag/env that routes approvals to the client instead of `auto_review`,
  analogous to what `OPENCODE_CONFIG_CONTENT` did for opencode (`DES-INPUT-GOV-006`)? This design
  answers it.
- **DES-INPUT-GOV-006 (opencode provisioning, #360/#377/#378)** — the template this task explicitly
  points at: it found a genuine provisioning route for opencode (`OPENCODE_CONFIG_CONTENT`, zero
  file writes, wins over project config) and shipped `acp_input_governance: true`. This design ran
  the same question against codex-acp and reached the opposite, equally well-evidenced conclusion.

**Carrier:** `src/acp_permission.rs` (`pretool_payload`) and `crates/wicked-council/src/registry.rs`
(the `codex` `AcpConfig` block, `registry.rs:219-243`) are the sites a YES answer would have touched.
Neither is touched by this deliverable.

**No `ASSUMPTION[external-transform]` applies** — same reasoning as DES-INPUT-GOV-002 §8 and
DES-INPUT-GOV-006: this design evaluates whether a third-party adapter's *own* config surface can be
provisioned; it does not itself introduce a payload-transforming library or service.

---

## 1. Decision

**NOT ADMITTED. `acp_input_governance` stays `false` for the built-in `codex` seat — no field change,
no comment change** (the existing `DES-INPUT-GOV-003` registry comment already states the correct
disposition and needs no update, since the finding below sharpens *why* rather than changing *what*).

**codex-acp exposes no config, flag, or environment variable that routes tool approvals to the ACP
client instead of `auto_review`, independent of its three hardcoded `AgentMode` presets — and none of
those three presets combines "ask the client for every core intent" with "the sandbox actually
permits the write it's asking about."** This is a structural (source-verified) finding, not an
absence-of-search: every RPC method, env var, and config-option ID codex-acp's shipped runtime
actually reads was enumerated and checked (§2–§4). Unlike opencode (whose default ruleset was
*configurable* — a data problem DES-INPUT-GOV-006 could route around with an inline-config env var),
codex-acp's approval/sandbox pairing is *hardcoded* per mode — there is no data-shaped seam to inject
into. This is genuinely **blocked on upstream**: closing it requires either a `codex-acp` code change
(a fourth mode, or a config option that decouples `approvalsReviewer` from `sandboxPolicy`) or a
`session/request_permission`-compatible replacement for the two fixed knobs that exist today.

---

## 2. What was checked, and how (source-verified, not guessed)

Per the task's instruction to verify at source, not guess: the actual shipped adapter code was
pulled and read directly, at two points in time, rather than reasoning from the `codex-acp` README's
prose alone.

### 2.1 Artifacts examined

| Artifact | Version | Provenance |
|---|---|---|
| `@agentclientprotocol/codex-acp` tarball | `1.9.0` | `npm pack`, fresh download — the exact version `DES-INPUT-GOV-003`/`oq-codex-acp-001` pinned (`gitHead 67db0d3d4a8a9b4bd3040c4dfdfa0919e9d97be9`) |
| `@agentclientprotocol/codex-acp` tarball | `1.10.0` | `npm pack`, fresh download — **current npm `latest`** as of this research (`gitHead 061f9a4a2e463a220d7a3ab2ae5e9732837085ef`), published the day after `1.9.0` per `oq-codex-acp-001/manifest.md`'s own recorded gap ("a newer `1.10.0` published roughly 90 minutes after `1.9.0`") — checked specifically to close that gap for this question |
| `wicked-crew`'s installed copy | `1.1.7` | `wicked-crew/node_modules/@agentclientprotocol/codex-acp` (resolved from its own `^1.1.7` pin) — read first as a sanity baseline; found **materially older** (predates the `approvalsReviewer`/`auto_review` concept entirely — absent from that build's strings), confirming the owning dependency range is stale relative to what a fresh install resolves and is *not* representative of current behavior |
| GitHub repo tree | at `061f9a4a…` (1.10.0's `gitHead`) | `gh api repos/agentclientprotocol/codex-acp/git/trees/<sha>?recursive=1` — full file listing, to check for a permission/config surface not reachable from the npm tarball's bundled `dist/index.js` alone |

Both `1.9.0` and `1.10.0`'s bundled `dist/index.js` were diffed at the structural level relevant
here (the three `AgentMode` static definitions) and are **byte-identical**: no upstream change to
this surface occurred between the evidence pin and current `latest`.

### 2.2 The full RPC surface codex-acp's runtime actually serves

Enumerated directly from the method-registration table (`dist/index.js`, the `AGENT_METHODS`-keyed
dispatch object) rather than assumed from docs:

```
initialize, session/new, session/load, session/fork, session/list, session/delete, session/resume,
session/close, session/setMode, session/setConfigOption, authenticate, providers/list,
providers/set, providers/disable, logout, session/prompt, nes/start, nes/suggest, nes/close,
+ extension methods: authentication/status, authentication/logout, session/set_model (legacy),
_session/steering, async-task stop, goal control
```

None of these is a general "set approval policy" or "set sandbox policy" request — the two knobs
that gate every tool intent (`approvalPolicy`+`approvalsReviewer`, and `sandboxPolicy`) are never
independently settable RPC parameters. They are always read as a *pair*, off one hardcoded object.

### 2.3 `session/setConfigOption`'s config-option IDs — the closest thing to a provisioning surface

`setSessionConfigOption`'s `applySessionConfigOption` switches on exactly five config IDs, verified
by reading the switch statement itself (not the type declarations):

| Config ID | What it changes | Independent of `mode`? |
|---|---|---|
| `fast-mode` | a boolean model-speed toggle | yes, but irrelevant to approvals |
| `mode` | **calls `AgentMode.find(value)`, which only matches the three fixed IDs** (`read-only`/`agent`/`agent-full-access`) — an unrecognized value throws `RequestError.invalidParams()` | this **is** the approval/sandbox knob, and it is closed-set |
| `collaboration_mode` | a separate collaboration mode (not approval-related) | yes, orthogonal |
| `model` | model selection | yes, orthogonal |
| `reasoning_effort` | reasoning budget | yes, orthogonal |

There is no sixth option for `approvalsReviewer`, `sandboxPolicy`, or any decomposed piece of either.
`session/setMode` (the ACP-standard session-mode RPC) reaches the identical `AgentMode.find` gate via
`applyModeChange`. Both entry points funnel to the same closed set of three.

### 2.4 Documented env vars (`README.md`, "Runtime options" — cross-checked against what the code actually reads)

```
CODEX_API_KEY, OPENAI_API_KEY, CODEX_PATH, CODEX_CONFIG, MODEL_PROVIDER, DEFAULT_AUTH_REQUEST,
INITIAL_AGENT_MODE, NO_BROWSER, APP_SERVER_LOGS
```

- **`INITIAL_AGENT_MODE`** is documented, and confirmed in code (`AgentMode.getInitialAgentMode`,
  `dist/index.js:27297`), to accept only `read-only | agent | agent-full-access` — the same closed
  set as §2.3, just selected at spawn time instead of via RPC. This is the mechanism
  `oq-codex-acp-001/manifest.md` already used for its `capture-readonly.ndjson` run, and its *own*
  runtime result already falls inside this design's scope (§3).
- **`CODEX_CONFIG`** is documented as "JSON object merged into the Codex session config" — the
  closest analog to opencode's `OPENCODE_CONFIG_CONTENT`. Read at the source (`dist/index.js:27997`,
  `this.config = codexConfig ?? {}`): every downstream read of `this.config` was enumerated by grep
  (`this.config["model_provider"]`, `this.config["model_providers"]` — both at `:28247-28248`, both
  used only to resolve which model-provider/gateway config block to talk to). **`approval_policy` and
  `sandbox_mode` do not appear anywhere in the 35,101-line bundle, as either string literal** —
  confirmed by an exhaustive `grep` for both tokens, zero hits. `CODEX_CONFIG` cannot set either
  field: `sendPrompt` (`dist/index.js:28655-28669`) always sources `approvalPolicy`,
  `approvalsReviewer`, and `sandboxPolicy` from the in-memory `agentMode` object — the same closed
  set §2.3 already covers — never from `this.config`.

### 2.5 The `src/app-server/v2/*` type surface — checked and ruled out

The GitHub tree (§2.1) surfaces a large generated-type directory
(`src/app-server/v2/PermissionProfile.ts`, `ActivePermissionProfile.ts`,
`AdditionalFileSystemPermissions.ts`, `GuardianApprovalReview.ts`, `AutoReviewRequirements.ts`, and
~500 more `.ts` files) that looks, by name, like a much finer-grained permission-profile system than
the three `AgentMode` presets. This is codex's full app-server protocol surface, vendored into the
repo for type generation — **most of it is not reachable from codex-acp's compiled runtime.**
Verified directly: grepping the bundled `dist/index.js` for `PermissionProfile`,
`AutoReviewRequirements`, `RequestPermissionProfile`, and `ActivePermissionProfile` returns **zero
matches**. `GuardianApprovalReview` *does* appear (24 references) — but exclusively in
notification handlers (`handleGuardianApprovalReviewStarted/Completed`) that convert the reviewer's
one-way status broadcast into an informational ACP tool-call update (`kind: "think"`, the exact
`guardian_assessment` tool call `oq-codex-acp-001/probe-risky.ndjson` captured). It is a
**notification the client can watch, not a request the client can answer** — there is no code path
turning a Guardian review into a blocking `session/request_permission`. The finer-grained
permission-profile system exists somewhere in codex's protocol surface, but `codex-acp` (the
adapter wicked-core actually spawns) does not surface it.

---

## 3. Why the one real candidate (`INITIAL_AGENT_MODE=read-only`) still fails property (a)

This is not a new capture — it is `oq-codex-acp-001/capture-readonly.ndjson`, re-read against the
now-confirmed source (`dist/index.js:27183-27198`, identical across `1.9.0` and `1.10.0`):

```
ReadOnly: approvalPolicy="on-request", approvalsReviewer="user",
          sandboxPolicy={type:"workspaceWrite", writableRoots:[], networkAccess:false, ...}
```

`approvalsReviewer:"user"` looks, on paper, like exactly the "route to the client" knob this OQ asks
for. It is not sufficient, because **the escalation trigger is "the sandbox would deny this," not
"every intent."** `ReadOnly`'s `sandboxPolicy.type` is `"workspaceWrite"` (not `"readOnly"` — a
distinct policy type that exists in the protocol, confirmed present as a literal in `dist/index.js`
but never attached to any of the three shipped presets — it is used only internally, for an
unrelated one-shot audit fork, `runAgentFileChangeReport`, at `sandbox: "read-only"`). A
`workspaceWrite` sandbox with `writableRoots: []` still permits writes inside the session's own `cwd`
regardless of that empty list — this is the exact behavior `oq-codex-acp-001` already measured live
(the edit and marker-file write both landed on disk under `ReadOnly`, `requestPermissionCallCount:
0`). Since the sandbox never denies the write, `on-request` never has anything to escalate, so
`approvalsReviewer:"user"` never gets invoked — it is dead configuration for every in-workspace
core intent a governed unit would actually perform. `INITIAL_AGENT_MODE` is a real, working env-var
provisioning mechanism (structurally the same shape as opencode's `acp_governance_env`); it simply
routes to a preset whose approval knob is provably unreachable for real work.

---

## 4. Conclusion against the task's YES/NO branch

**NO — codex-acp exposes no config/flag/env that routes tool approvals to the client instead of
`auto_review`, in a form that would actually gate real writes.** Per the task's own branching:
*"IF NO such config: honest NOT-ADMITTED, blocked-on-upstream."* That is this design's disposition.
No `AcpConfig` field changes (`acp_governance_env` stays unset for `codex`, matching every other
unadmitted seat); no registry edit; no new evidence directory under `.product/evidence/` (there is no
live re-proof to package — §2–§3 are a source-level negative result, not a runtime capture, and
`oq-codex-acp-001`'s existing captures already cover the one env var, `INITIAL_AGENT_MODE`, that
exists). The registry comment `DES-INPUT-GOV-003` shipped already states the correct disposition and
does not need to change; this design's contribution is closing the "did we actually check for a
provisioning route" question `DES-INPUT-GOV-003 §6` left open, with a source-verified NO rather than
an assumption.

---

## 5. Forward path — genuinely blocked on upstream, not on wicked-core's own design

Both non-mutually-exclusive paths `DES-INPUT-GOV-003 §6` already named remain the only ways forward,
now confirmed to require an **upstream** change (no wicked-owned config/env can substitute):

1. **An upstream `codex-acp` change** adding a fourth `AgentMode` (or extending
   `setSessionConfigOption` with an independent knob) that pairs `approvalsReviewer: "user"` with a
   `sandboxPolicy` that actually denies workspace writes by default (so `on-request` has something to
   escalate) while still permitting them once approved — i.e., a real ask-then-allow loop, not
   `ReadOnly`'s ask-that-never-fires or `Agent`'s allow-without-asking. This is a request to file
   upstream (`agentclientprotocol/codex-acp`), not something wicked-core can provision around, because
   the pairing is hardcoded in the adapter's own three static objects, not exposed as decomposable
   config.
2. **Close the tool-name identity gap regardless of (1)** — `presentation.ts`'s `toolCall` still
   carries no canonical `toolName`/`toolCall.name`, only a free-text `title` (often the literal shell
   command). Even if (1) lands, `pretool_payload` would key policy on that free text, same gap
   `codex`/`copilot`/`opencode`'s own verdicts each recorded independently. A shared, cross-adapter
   normalization (keyed on `toolCall.kind` rather than `title`) is the durable fix `DES-INPUT-GOV-006
   §4` already flagged as a shared follow-up across all three.

Until (1) ships upstream and a fresh live capture (a new `oq-codex-acp-00N` evidence set, mirroring
`oq-opencode-acp-002`'s re-proof structure) shows a blocking `session/request_permission` for every
core intent **with the write actually landing on approval**, flipping `acp_input_governance = true`
for `codex` is not justified. This OQ is closed as **blocked-on-upstream**, not as
**unresearched** — the distinction this design exists to establish.

---

## 6. Gates

**None required by this deliverable.** No production code, registry field, or test changes are made
(§4). The next actor to touch `crates/wicked-council/src/registry.rs` for this seat (if and when an
upstream fix lands) runs the standard set: `cargo fmt --all -- --check`, `cargo test -p
wicked-council`, `cargo test --lib`, `cargo clippy --all-targets -- -D warnings` — recorded here only
so the eventual implementation phase doesn't have to rediscover them.
