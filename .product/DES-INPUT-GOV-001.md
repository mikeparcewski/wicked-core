# DES-INPUT-GOV-001 — CLI-agnostic input-governance recon

**Issue:** wicked-core #360  
**Status:** recon only — no implementation made  
**Scope:** governed units assigned to the non-Claude council seats: `codex`, `opencode`, `copilot`, and `pi`.

## Decision summary

The protocol seam already exists, but admission to it is Claude-specific. `AcpStepRunner` advertises
the ACP permission capability for every ACP session and can answer `session/request_permission` for
every adapter. It constructs `AcpGate`, however, only when the selected CLI resolves to Claude
(`src/acp_runner.rs:3956-4037`, `4490-4495`). Thus a non-Claude ACP permission request is answered
with the permissive ungated result, not the policy/audit path (`2867-2908`).

The wrapped runner has the same Claude-only split: only a governed Claude invocation receives a
`PreToolUse` `gate-hook`; a governed non-Claude invocation is allowed to run with
`GovernanceUnenforced` emitted and `governed: false` (`src/execute_wrapped.rs:705-765`).

This is not evidence that every non-Claude adapter emits permission requests. Whether it does is an
upstream/runtime fact, and is deliberately recorded as an **OPEN QUESTION** below. The code-proven
finding is narrower and sufficient: *if* a non-Claude ACP adapter asks for permission, wicked-core
currently permits the call without governance; *if it does not ask*, no local per-call carrier exists.
Either way, no non-Claude seat has wicked policy-and-audit enforcement on the governed path today.

### DEFECT — High: non-Claude ACP sessions bypass per-call input governance

**Severity: High.** A governed Codex, OpenCode, Copilot, or Pi unit that runs through its live ACP
session has no per-call input-governance admission. This is a present defect, not merely a rollout
limitation: all four are built-in, council-enabled seats with ACP configuration
(`crates/wicked-council/src/registry.rs:145-260`), and the shared ACP runner explicitly sends
governed non-Claude units down its live-session path (`src/acp_runner.rs:4040-4044`).

The exact admission site is `src/acp_runner.rs:3956`:

```rust
let gate_ctx = match (&input.governance, cli_runs_claude(&cli_key)) {
    (Some(g), true) => { /* construct AcpGate */ }
    _ => None,
};
```

`cli_runs_claude` at `src/acp_runner.rs:4490-4495` classifies each registered non-Claude binary as
non-Claude. Consequently every governed non-Claude ACP session gets `gate_ctx = None`, creates no
`AcpGate` (`src/acp_runner.rs:4173-4195`), and is reported as `governed: false`
(`src/acp_runner.rs:4231-4286`). If an adapter emits `session/request_permission`,
`answer_permission_request` takes its `None` branch and returns `allow_result`, rather than
`permission_result` (`src/acp_runner.rs:2867-2907`). That bypasses policy selection, boundary
evaluation, the armed/fired markers, and durable `ConformanceClaim` recording. If an adapter does
not emit that request, its tool intent is likewise never presented to the shared evaluator. The
source does not establish any non-Claude adapter's upstream permission-frame behavior; either
outcome is an ungoverned non-Claude ACP turn.

The cause is the Claude-name admission predicate, not a missing implementation in
`acp_permission.rs`: that module already supplies the carrier-neutral evaluator and audit path.

## 1. Audit: actual containment by path

### Shared facts

- The registry declares ACP launch configuration for all four seats: `codex-acp`, `pi-acp`,
  `copilot --acp`, and `opencode acp` (`crates/wicked-council/src/registry.rs:145-260`). Having an
  `AcpConfig` means the runner attempts an ACP session; it does **not** prove that the binary is
  installed, that the handshake succeeds, or that tool permissions are emitted. On no config or
  an ACP startup/session failure, the runner falls back to `WrappedCliStepRunner`
  (`src/acp_runner.rs:4040-4044`, `4151-4166`, `4296-4346`).
- Council voting appends `trust_flags` to its command (`crates/wicked-council/src/dispatch.rs:729-730`).
  The wrapped worker independently resolves and applies the same merged registry posture
  (`src/execute_wrapped.rs:1700-1722`, `1854-1876`). Those flags are a seat-native containment
  declaration, not a wicked per-tool policy decision or wicked audit trail.
- The post-merge cap prevents known Codex full-sandbox-bypass forms from reaching an ungated worker,
  rewriting them to `--sandbox workspace-write` (`src/execute_wrapped.rs:1736-1851`). It is Codex
  syntax-specific; it does not invent a sandbox for an empty opencode/copilot/pi posture.

### Per-seat posture matrix

`Per-call governance` means a policy decision plus durable governance evidence for every surfaced
tool call. `Sandbox-only` means a locally declared native containment flag, not policy governance.
`Neither` means neither of those claims is established by this repository.

| Seat | Registry `trust_flags` | Governed wrapped path | Governed ACP path | Actual posture by path |
|---|---|---|---|---|
| **codex** | `--sandbox workspace-write` (`registry.rs:156-171`) | No `gate-hook`; emits `GovernanceUnenforced`. The declared Codex sandbox is applied. | ACP is attempted via `codex-acp`, but `gate_ctx` is absent because `cli_runs_claude("codex")` is false. Any permission request receives `allow_result`. `trust_flags` are not ACP launch controls. | Wrapped: **sandbox-only**. ACP: **neither** (no wicked per-call governance and no repository-proven ACP sandbox). |
| **pi** | Empty (`registry.rs:189-207`) | No `gate-hook`; emits `GovernanceUnenforced`; no repository-proven sandbox. | Live/shared ACP session path via `pi-acp`; `cli_runs_claude("pi")` makes `gate_ctx` absent. Any permission request receives `allow_result`. See the High defect above. | Wrapped: **neither**. ACP: **neither**. |
| **copilot** | Empty (`registry.rs:215-240`) | No `gate-hook`; emits `GovernanceUnenforced`; no repository-proven sandbox. | Native ACP is attempted as `copilot --acp`; no `AcpGate`; any permission request receives `allow_result`. | Wrapped: **neither**. ACP: **neither**. |
| **opencode** | Empty (`registry.rs:242-260`) | No `gate-hook`; emits `GovernanceUnenforced`; no repository-proven sandbox. | Native ACP is attempted as `opencode acp`; no `AcpGate`; any permission request receives `allow_result`. | Wrapped: **neither**. ACP: **neither**. |

The ACP column is intentionally conditional on a live session. Source code cannot establish a
particular environment's current `acp.byCli` health; that needs diagnostics or an observed run.
The wrapped column is the defined fallback, not proof that a given seat is currently falling back.

### What does not change the matrix

Every wrapped seat receives a hardened child environment and an in-boundary scratch directory
(`src/execute_wrapped.rs:790-805`). Those are useful hygiene and scratch-location controls; they
do not inspect individual tool intent, do not implement the non-Claude sandbox, and do not make a
seat policy-governed. Likewise, ACP uses the unit workdir rather than the daemon cwd
(`src/acp_runner.rs:3948-3955`), but a correct cwd is not a write boundary.

## 2. Protocol seam: identical policy and audit when the ACP gate is present

The intended carrier equivalence is real and should be retained.

1. **Carrier-specific normalization only.** The Claude hook reads `{tool_name, tool_input}` from
   stdin. ACP translates `toolName` (falling back to `toolCall.name`, then `toolCall.title`) and
   `toolCall.rawInput` into exactly that shape (`src/acp_permission.rs:56-104`). It then calls the
   shared `claude_pretool_context` builder (`138-146`).
2. **One evaluator.** `run_gate_hook` hands its normalized payload to
   `evaluate_tool_call`; so does `permission_result` (`src/gate_hook.rs:640-702`,
   `src/acp_permission.rs:133-171`). The shared evaluator applies boundary and phase-scope checks,
   selects and decides policy, and appends the conformance decision. This avoids an ACP-specific
   policy parser drifting from `PreToolUse`.
3. **One audit contract.** Before evaluation, the shared evaluator writes the hook-fired liveness
   sentinel; the launcher writes the armed marker before the session/tool run
   (`src/gate_hook.rs:1079-1095`; ACP arming at `src/acp_runner.rs:3961-3984`). The fold rejects a
   governed unit with missing/tampered evidence, and folds matching `ConformanceClaim`s into the
   store (`src/gate_hook.rs:1392-1530`). A gate-aware ACP turn therefore has the same audit and
   evidence-integrity contract as a wrapped hook turn.
4. **Fail closed at the protocol edge.** An unusable tool name, a missing requested option kind, or
   an unparseable request yields `cancelled`, not a guessed allow. Allow/reject is selected by the
   ACP option *kind* and prefers `*_once` over `*_always`
   (`src/acp_permission.rs:106-171`).

The hole is the call-site selection, not the seam: `answer_permission_request` calls
`permission_result` only when it receives `Some(AcpGate)`; otherwise it calls `allow_result`
(`src/acp_runner.rs:2867-2908`). Since non-Claude `gate_ctx` is `None`, the latter path is selected.
`allow_result` still fails closed if no allow option exists, but it performs no policy selection,
boundary judgement, sentinel write, or `ConformanceClaim` append.

### Auto-approve bypass pin

Advertising `clientCapabilities.permission: true` at ACP initialization makes a conforming adapter
able to ask the client; without it, the bridge never sends `session/request_permission`
(`src/acp_runner.rs:1509-1524`). This is necessary but not sufficient: an adapter may still
auto-approve internally or never route a tool through ACP permission.

For the current Claude ACP worker, the engine also asserts that its seeded settings contain the
shared deny fence and **no** `permissions.defaultMode`; the test explains that an auto-approving
mode could resolve a call before the governance gate sees it (`src/acp_runner.rs:5901-5929`). This
is the existing bypass pin. It is not proof that the same settings shape or control applies to
codex, pi, copilot, or opencode.

Admission of another adapter must therefore be pinned by evidence, not by the global capability
bit alone:

- verify a deliberately tool-using turn emits a `session/request_permission` request;
- verify the request carries canonical tool identity plus raw input sufficient for the shared
  normalizer;
- identify and disable that adapter's auto-approve/default-permission surface; and
- prove an allowed and a denied call each produce the shared decision-log evidence, including the
  markers and `ConformanceClaim`.

## 3. ACP bridge requirements and open questions

There are no registry-wrapped-only built-in seats: all four have an ACP configuration. A seat is
wrapped in a given run when ACP cannot start or continue, and it is input-ungoverned on ACP until
admitted by evidence. Thus the bridge/adaptor requirement below applies to every non-Claude seat
that is currently wrapped for a run or is not yet eligible for ACP governance: surface each tool
intent as a blocking
`session/request_permission` request, with canonical name and raw arguments, and honour the
selected allow/reject outcome before executing the tool. The engine then needs explicit,
evidence-gated admission to `AcpGate`; the present Claude-name predicate cannot be reused as that
admission rule.

| Seat | Locally known | ACP admission prerequisite — **OPEN QUESTION** |
|---|---|---|
| **codex** | The fallback has a declared native `workspace-write` sandbox. ACP binary is `codex-acp`. | **OQ-CODEX-ACP-001:** For a pinned `codex-acp` version, does every tool action block on `session/request_permission`, expose canonical tool identity plus raw input, honour a reject, and have a documented auto-approve/default-permission control that can be disabled? Verify from upstream documentation/source and captured local frames; do not infer from the registry comment. |
| **pi** | ACP binary is community `pi-acp`; registry only claims sessions/resumption. No declared fallback sandbox. | **OQ-PI-ACP-001:** Does `pi-acp` surface every Pi tool action as a blocking `session/request_permission` with compatible name/input, honour the reject result, and expose a default/auto-approve control that can be disabled? Verify upstream and with captured local frames. |
| **copilot** | Native `copilot --acp`; registry verifies initialize/session-new/session-prompt streaming, not permissions. No declared fallback sandbox. | **OQ-COPILOT-ACP-001:** Does `copilot --acp` surface every tool action as a blocking permission request with compatible name/input, honour rejection, and have a documented non-auto-approving headless mode? Separately identify any bounded native sandbox control. Verify upstream and with captured local frames. |
| **opencode** | Native `opencode acp`; registry says no bridge needed. No declared fallback sandbox. | **OQ-OPENCODE-ACP-001:** Does `opencode acp` surface every tool action as a blocking permission request with compatible name/input, honour rejection, and have a documented non-auto-approving headless mode? Separately identify any bounded native sandbox control. Verify upstream and with captured local frames. |

These questions require external/upstream research or controlled runtime tracing. No conclusion about
an upstream CLI's sandbox, permissions, or protocol support is made from the presence of an
`AcpConfig` alone.

## 4. Recommended rollout and fallback posture

1. **First, make the safety state explicit.** Preserve the wrapped `GovernanceUnenforced` signal
   and add equivalent disclosure for a governed non-Claude ACP turn until it has passed admission.
   Today the ACP path can be silent because it goes through `allow_result`; absence of a wrapped
   event is not evidence of enforcement.
2. **Research and prove the adapter contract, then admit one seat at a time.** Start with the
   first adapter that passes the captured permission round-trip and bypass-pin checks; the checked-in
   evidence does not justify a vendor-specific readiness ranking. Codex is the only seat with a
   repository-proven wrapped sandbox floor, so it is the least-bad fallback candidate while ACP
   evidence is collected—not evidence that `codex-acp` is ready for admission.
3. **Admit through a capability/evidence flag, default deny.** A registry-level
   `acp_input_governance` capability (default `false`) is safer than broadening a binary-name
   predicate. Enable it only after the four evidence checks in §2 pass for a pinned adapter version.
   The exact field is a design recommendation, not an implementation request.
4. **Keep Codex's bounded fallback.** `--sandbox workspace-write` is the only repository-proven
   non-Claude native floor. Retain the existing cap against its full bypass.
5. **Do not claim a fallback sandbox for pi/copilot/opencode yet.** Research each CLI's documented
   headless bounded mode before adding a flag. A guessed flag can be ignored, change semantics, or
   disable containment. Until then route write-heavy/high-risk governed work to a seat with
   demonstrated per-call governance, and label other seats input-ungoverned.

### Fallback policy

For a seat without proven ACP permission admission:

- If a native hook can expose tool intent, use an adapter that calls the same shared evaluator and
  writes the same markers/claims. A mere allow/deny callback without the audit contract is not
  acceptable because `fold_input_denial` correctly treats missing governed evidence as a failure.
- Otherwise run only under a verified, seat-native bounded sandbox where one is known, emit an
  explicit input-ungoverned event/state, and avoid dispatching it for work that requires the
  governed write boundary.
- If neither a shared-evaluator carrier nor a verified native sandbox exists, the honest posture is
  **neither**. It must not be represented as a governed unit merely because it has an ACP session
  or a registry record.

## 5. Mechanical follow-up issues

The following are deliberately mechanical issue drafts. They separate a locally demonstrated wiring
defect from upstream behavior that must be researched rather than guessed.

### `fix(acp): replace the Claude-name admission predicate with evidence-gated seat capability`

`src/acp_runner.rs:3956` constructs `gate_ctx` only for `(Some(governance),
cli_runs_claude(cli_key))`. Therefore every non-Claude seat gets no `AcpGate`, and its ACP
permission calls use `allow_result` instead of the shared policy-and-audit evaluator. Replace the
binary-name admission rule with an explicit, evidence-gated ACP input-governance capability. Admit
only an adapter whose pinned version has passed the open-question proof in §3. Preserve the existing
`AcpGate` boundary, policy, marker, and `ConformanceClaim` path. Add an end-to-end test that a
governed tool call denied by policy is rejected and leaves the same evidence as the Claude
`PreToolUse` carrier; prove malformed requests fail closed and unadmitted adapters remain explicitly
input-ungoverned.

### `research(opencode): establish no-regret bounded-sandbox flags for wrapped governed fallback`

The built-in opencode seat has no `trust_flags`, while its governed wrapped fallback has no
`gate-hook` adapter and therefore no repository-proven write boundary. Research the upstream
opencode CLI's documented noninteractive bounded-sandbox or workspace-write controls for the exact
pinned version. Validate candidate flags with a controlled probe: writes inside the assigned
worktree succeed, writes outside fail, and the command neither silently broadens approval nor
disables the sandbox. Only after that evidence, add the minimal verified flags to the registry and
cover their placement before `--` in the wrapped launch argv. Do not add guessed flags.

### `research(copilot): establish no-regret bounded-sandbox flags for wrapped governed fallback`

The built-in Copilot seat has no `trust_flags`, while its governed wrapped fallback has no
`gate-hook` adapter and therefore no repository-proven write boundary. Research the upstream
GitHub Copilot CLI's documented noninteractive bounded-sandbox or workspace-write controls for the
exact pinned version. Validate candidate flags with a controlled probe: writes inside the assigned
worktree succeed, writes outside fail, and the command neither silently broadens approval nor
disables the sandbox. Only after that evidence, add the minimal verified flags to the registry and
cover their placement before `--` in the wrapped launch argv. Do not add guessed flags.

### `research(codex-acp): resolve OQ-CODEX-ACP-001 before governance admission`

For a pinned `codex-acp` release, capture protocol frames from a deliberately tool-using turn and
determine whether each tool intent produces a blocking `session/request_permission` request with a
canonical tool name and raw input compatible with `acp_permission::pretool_payload`. Identify the
documented auto-approve/default-permission control, prove it can be disabled, and prove that a
selected reject prevents the tool action. Record source/version and a reproducible fixture. If any
property fails, do not set an ACP input-governance admission capability; retain the explicit
ungoverned/sandbox fallback posture.

### `research(pi-acp): resolve OQ-PI-ACP-001 before governance admission`

For a pinned `pi-acp` release, capture protocol frames from a deliberately tool-using Pi turn and
determine whether every tool intent produces a blocking `session/request_permission` request with a
canonical tool name and raw input compatible with `acp_permission::pretool_payload`. Identify the
adapter's default or auto-approve control, prove it can be disabled, and prove that a selected
reject prevents the tool action. Record source/version and a reproducible fixture. If any property
fails, Pi must not be admitted to `AcpGate`; preserve the High-severity defect tracking until the
absence of per-call governance is made explicit and a safe fallback is retained.

### `research(copilot-acp): resolve OQ-COPILOT-ACP-001 before governance admission`

For a pinned `copilot --acp` release, capture protocol frames from a deliberately tool-using turn
and determine whether every tool intent produces a blocking `session/request_permission` request
with a canonical tool name and raw input compatible with `acp_permission::pretool_payload`.
Identify and disable any default/auto-approve headless mode; prove a selected reject prevents the
tool action. Record source/version and a reproducible fixture, and separately record the native
bounded-sandbox result needed by the wrapped-fallback issue. Do not admit Copilot to `AcpGate` on
lifecycle/streaming evidence alone.

### `research(opencode-acp): resolve OQ-OPENCODE-ACP-001 before governance admission`

For a pinned `opencode acp` release, capture protocol frames from a deliberately tool-using turn
and determine whether every tool intent produces a blocking `session/request_permission` request
with a canonical tool name and raw input compatible with `acp_permission::pretool_payload`.
Identify and disable any default/auto-approve headless mode; prove a selected reject prevents the
tool action. Record source/version and a reproducible fixture, and separately record the native
bounded-sandbox result needed by the wrapped-fallback issue. Do not admit opencode to `AcpGate` on
its native-ACP registry entry alone.

## 6. Recon limits

This document is based only on the checked-out source. It did not inspect third-party CLI sources,
run external CLIs, or rely on a service/library that transforms payloads. Therefore no
`ASSUMPTION[external-transform]` entries apply.
