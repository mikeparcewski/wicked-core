# DES-INPUT-GOV-008 — CLI-agnostic input governance via a wicked-owned OS sandbox (DENY) + a wicked-owned governed MCP tool server (AUDIT)

**Umbrella:** wicked-core #360.
**Phase:** design / DESIGN-RECON (revises the clarify-phase draft of this same doc) — **no production
code in this deliverable.** Implementation is a later phase.
**Design-phase amendments folded in (operator review of the clarify draft):** (1) §2.1 makes the
**layering** explicit — Boundary 1 is a floor UNDER the working carriers (claude PreToolUse, opencode
provisioned ACP, wrapped deny-fence), all **KEPT**; Boundary 2 is an **alternative** audit path for
seats with no working per-call carrier, not a replacement for the ones that work. (2) §4.2 + the §5
matrix make native-tool-disable a **HARD per-seat gate**: a seat that cannot be forced off native
tools gets **Boundary-1 DENY only, disclosed contained-NOT-audited** — never falsely marked audited.
(3) §3.3/OQ-SANDBOX-MACOS-001 names the `sandbox-exec` deprecation contingency concretely (live
functionality probe + App Sandbox vs. Endpoint Security), not a hand-wave. (4) §6/§8 keep the
default-OFF `mcp_input_governance` capability with **no admission without a live capture**, mirroring
the `oq-opencode-acp-002` re-proof discipline.
**Adversarial-review corrections folded in (this revision):** (5) §2/§3.4/§9 now state plainly that
Boundary 1 is a **WRITE-containment floor, not exfiltration protection** — the model-API network
channel and the non-curated-dir read surface stay open by necessity, so anything the agent
legitimately reads can still leave via ordinary model traffic; exfiltration/DLP is explicitly OUT OF
SCOPE, a separate egress-proxy problem. (6) §4.2's second "safety net" (claiming `wicked-tools`
uniquely widens writes to the estate graph dir) is struck as false — Boundary 1's own sandbox profile
grants that write at the kernel level to every process, native or governed, so the only real
asymmetry native-tool-disable buys is that a governed call is *recorded* and an un-disabled native
call is not.
**Predecessors (the dead end this reframes away from):**
- DES-INPUT-GOV-001 (recon, #360) — established that per-call input governance is Claude-name-gated,
  and that admitting other seats requires each ACP adapter to surface a blocking
  `session/request_permission` with canonical identity that honours a reject.
- DES-INPUT-GOV-002 (#364) — replaced the Claude-name predicate with a per-seat, default-OFF
  `acp_input_governance` capability; admitted **claude only**.
- DES-INPUT-GOV-003/004/005 + the OQ-*-ACP-001 evidence runs — resolved the four non-Claude seats:
  - `pi-acp`: **NOT ADMITTED** — never calls `session/request_permission` on the built-in tool path
    at all (`.product/evidence/oq-pi-acp-001/verdict.md`).
  - `codex-acp`: **NOT ADMITTED** — real permission plumbing exists but the default `Agent` mode's
    internal `auto_review` reviewer resolves core intents before the client is ever asked
    (`.product/evidence/oq-codex-acp-001/verdict.md`).
  - `copilot --acp`: **NOT ADMITTED (closest)** — edit/bash gate, but in-workspace reads do not, and
    identity is not canonical (`.product/evidence/oq-copilot-acp-001/verdict.md`).
  - `opencode acp`: **NOT ADMITTED unconfigured** → **ADMITTED only after** DES-INPUT-GOV-006
    injected `OPENCODE_CONFIG_CONTENT` to force every intent through the ask
    (`.product/evidence/oq-opencode-acp-002/verdict.md`).

**The reframe.** Per-adapter `session/request_permission` admission is a **dead end for three of
four non-Claude seats** and, even for the one that worked (opencode), it only worked by injecting a
wicked-owned config that *forces* the adapter to ask. The load-bearing lesson is already in
DES-INPUT-GOV-006 §1.1: **the adapter's own permission verdict is never trusted — wicked-core's gate
answers.** This design stops trying to make each adapter *ask*, and instead builds governance on two
boundaries **wicked already owns end-to-end**, neither of which depends on any adapter surfacing a
permission frame:

1. **DENY — a wicked-owned OS sandbox** (macOS `sandbox-exec`, Linux `bwrap`/namespaces+network
   unshare) wraps the CLI worker process so the run worktree is the only writable root, enforced by
   the kernel **whatever the CLI approves**.
2. **AUDIT — a wicked-owned MCP tool server** (governed `read`/`write`/`edit`/`bash`) injected per
   worker with the adapter's native tools disabled, so the only *working* tool path is the governed
   one, and every call emits a `ConformanceClaim`.

The two are defence-in-depth and independent: (1) is a hard containment floor that needs nothing
from the adapter; (2) is a per-call audit+policy trail that needs only two adapter capabilities
(inject an MCP server, disable native tools) that are *config surfaces*, not *protocol behaviours*.

---

## 1. Why the ACP-permission path is a dead end (recap, at source)

The admission bar (DES-INPUT-GOV-001 §3) requires each adapter to, per tool intent, emit a blocking
`session/request_permission` carrying canonical identity + raw input, and honour a reject. Verified
outcomes:

| Seat | Emits blocking permission per intent? | Honours reject? | Canonical identity? | Verdict |
|---|---|---|---|---|
| claude | yes (PreToolUse hook, not ACP frame) | yes | yes | ADMITTED (DES-002) |
| opencode | **only when wicked injects `OPENCODE_CONFIG_CONTENT`** | yes | no (title-as-name) | ADMITTED w/ provisioning (DES-006) |
| copilot | partial — reads in-workspace do **not** gate | edit/bash yes | no | NOT ADMITTED |
| codex | no — internal `auto_review` pre-resolves | n/a | n/a | NOT ADMITTED |
| pi | no — tools execute locally, ACP is a passive observer | n/a | n/a | NOT ADMITTED |

The pattern: an adapter's willingness to route every intent through a client-answerable permission
frame is an **upstream behavioural property we do not control and mostly did not get.** Even the one
success is really wicked reaching *around* the protocol into the adapter's config to change what it
does. So the durable strategy is to govern at surfaces wicked configures and the kernel enforces —
not at a protocol frame each vendor may or may not send.

There is already precedent in-repo that "the adapter's verdict is not the governance verdict":
`src/acp_runner.rs:1656-1668` advertises the estate MCP server to *every* ACP seat via `session/new`'s
`mcpServers` array, and DES-INPUT-GOV-006 §1.1 states plainly that opencode's own allow/ask/deny is
not consulted — wicked's `AcpGate` is. This design extends that stance to its logical end.

---

## 2. Architecture — two wicked-owned boundaries

```
                        wicked-core daemon (single-writer store, policy engine)
                                   │ spawns worker
                                   ▼
   ┌─────────────────────────────────────────────────────────────────────────────┐
   │ BOUNDARY 1 (DENY): wicked-owned OS sandbox wraps the ENTIRE worker process    │
   │   macOS sandbox-exec SBPL   |   Linux bwrap: ro-bind / + rw-bind <worktree>   │
   │   • deny file-write* except writable set: worktree + estate dir + scratch     │
   │   • deny network* (unshare, if chosen) • block reads of a curated secret-dir  │
   │     denylist (~/.aws,~/.ssh,…) — NOT a read jail; NOT exfiltration protection │
   │   ENFORCED BY THE KERNEL regardless of what the CLI or its tools "approve".   │
   │  ┌────────────────────────────────────────────────────────────────────────┐  │
   │  │ agent CLI (claude/codex/copilot/opencode/pi), native FS/bash tools OFF  │  │
   │  │                                                                        │  │
   │  │ BOUNDARY 2 (AUDIT): wicked-owned MCP tool server, injected per worker   │  │
   │  │   tools: read / write / edit / bash  → each runs wicked's evaluator,    │  │
   │  │   emits ConformanceClaim + armed/fired markers (same audit contract     │  │
   │  │   as the Claude PreToolUse gate). The ONLY working tool path.           │  │
   │  └────────────────────────────────────────────────────────────────────────┘  │
   └─────────────────────────────────────────────────────────────────────────────┘
```

**Separation of concerns.**
- Boundary 1 answers *"can this process physically damage anything outside the worktree?"* — **No, by
  kernel policy.** It requires **nothing** from the adapter. It is the fail-closed floor: even a
  fully-ungoverned, native-tools-on adapter cannot write outside the worktree. **This is a
  WRITE-containment floor, not exfiltration protection** — see the scope note below and §3.3.
- Boundary 2 answers *"what did the worker try to do, was it allowed by policy, and is there durable
  evidence?"* — the per-call **audit + policy** trail (`ConformanceClaim`s) that the store folds. It
  requires two adapter *config* capabilities (§4), not a protocol behaviour.

**Exfiltration is explicitly OUT OF SCOPE for Boundary 1 — a separate egress-proxy problem.** The
agent CLI must keep a network path open to its own model backend for the run to function at all
(§3.2 sub-option (A) or (B) — some model-host egress is unconditionally required); that channel is
exactly how the agent already sends anything it *has read* to the model provider as ordinary conversation
traffic, and Boundary 1's file-read side is a curated denylist of a handful of secret dirs
(`secret_read_block_dirs()`), not a read jail — reads of everything else (source, other repos on
disk, non-curated credential files) stay open **on purpose**, carried forward unchanged from the
validator module's own HONEST LIMITS note (`validator.rs:46-58`: "a script can still READ the rest of
the filesystem ... and could exfiltrate a file that is NOT on the block list"). So Boundary 1 denies
*writes* outside the worktree and blocks reads of the handful of curated secret dirs; it does **not**
and **cannot**, by itself, stop state the agent legitimately read from leaving over the model-API
channel that is always open. Preventing that is a distinct, unaddressed egress-proxy/DLP problem, not
a property this design claims.

Neither replaces the other. Boundary 1 without Boundary 2 is contained-but-unaudited (a worker can
still thrash inside its worktree with no per-call record). Boundary 2 without Boundary 1 is
audited-but-escapable (if native tools are not fully disabled, a call can bypass the MCP path). The
design ships them as a pair; §6 sequences which lands first.

### 2.1 LAYERING, not replacement — the working carriers stay

This design **adds two layers; it removes nothing that works today.** State this explicitly so no
build phase reads "reframe" as "rip out."

- **Boundary 1 is a floor UNDER everything.** It wraps the worker process regardless of carrier, so
  it sits beneath *all* existing governance simultaneously:
  - beneath claude's `PreToolUse` gate-hook (`src/gate_hook.rs`, ORCHESTRATOR.md §3.5) — **KEPT**;
  - beneath opencode's ACP `session/request_permission` governance provisioned by
    `OPENCODE_CONFIG_CONTENT` (`acp_input_governance: true`, DES-INPUT-GOV-006) — **KEPT**;
  - beneath the wrapped-path `--disallowedTools` + `permissions.deny` fence — **KEPT**.
  Boundary 1 does not consult or replace any of these; it is a second, kernel-enforced containment
  layer under whatever policy verdict those carriers already produce. A claude unit keeps its
  PreToolUse audit trail **and** gains a kernel write-jail; an admitted opencode unit keeps its ACP
  gate **and** gains the same jail.
- **Boundary 2 is an ALTERNATIVE audit path, not a replacement for the ones that work.** It exists
  for seats that *cannot* produce a per-call audit trail through their carrier — i.e. the seats that
  failed ACP admission (`codex`/`pi`/`copilot`) and, if ever desired, as a uniform path. Where a
  carrier already yields the `ConformanceClaim` trail (claude PreToolUse; opencode provisioned ACP),
  that carrier is **KEPT as the audit source** and Boundary 2 is optional/redundant for it. Boundary 2
  is how a seat *with no working per-call carrier* earns an audit trail — not a mandate to re-route
  claude/opencode off their proven paths.
- **Consequence for the matrix (§5):** a seat is "audited" if it has *either* a working per-call
  carrier *or* a working Boundary-2 (native tools provably OFF + governed MCP injected). A seat with
  neither is **Boundary-1-DENY-only** and is disclosed as such — contained but not audited. That is
  the honest floor, never dressed up as governed.

---

## 3. Boundary 1 — the wicked-owned OS sandbox (DENY)

### 3.1 This is not greenfield — lift the existing validator sandbox

`src/validator.rs` **already contains a complete, working OS-sandbox implementation**, today used to
jail validator script runs, not CLI workers:

- `sandbox_availability()` / `detect_sandbox_launcher()` (`validator.rs:382,516`) probe PATH for
  `sandbox-exec` (macOS) → `bwrap` (Linux) → `firejail` (Linux, network-only fallback), returning a
  `SandboxLevel` (`Sandboxed` / `NetworkOnly` / `BestEffort`) — the honest capability disclosure.
- `macos_sandbox_profile()` (`validator.rs:471`) builds an SBPL profile: `(allow default)`,
  `(deny network*)`, `(deny file-read*)` for a curated secret-dir set, `(deny file-write*)` then
  `(allow file-write* (subpath <canonical run dir>))` + system temp + `/dev/{null,stdout,stderr}`.
  Canonicalization handles the macOS `/var → /private/var` symlink.
- The `bwrap` branch (`validator.rs:530+`) `--ro-bind / /`, rw-binds only the run dir, `--unshare`
  the network, masks the secret dirs with empty tmpfs, and puts the child in its own PID namespace
  tied to the launcher (`--unshare-pid --die-with-parent`) so the tree dies on timeout.
- `secret_read_block_dirs()` (`validator.rs:428`) curates `~/.aws ~/.ssh ~/.gnupg
  ~/.config/wicked-council ~/.claude ~/.config/gh`.

**The design is: generalize this from `run the validator script` to `wrap the agent CLI worker
spawn`.** The write-root becomes the unit's **worktree** instead of the validator run dir; the
scratch carve-out becomes the in-boundary `<cwd>/tmp` the wrapped/ACP paths already mint
(`execute_wrapped` / `start_acp_process`'s `scratch_tmp` param, `src/acp_runner.rs` — the `scratch_tmp` binding around the session-spawn env setup).

### 3.2 What changes vs. the validator use

1. **Write root = the unit worktree (+ estate-home graph dir when applicable).** The wrapped path
   already computes exactly this set: `WICKED_WRITE_ROOTS` (`src/gate_hook.rs:114`) plus the
   estate-home graph widening (`src/execute_wrapped.rs:824-881`). The sandbox profile's writable
   subpaths must be **the same set** so a legitimate governed write (worktree edit, estate graph
   `-wal`/`-shm`) is not killed by the kernel while a policy-allowed op. The `extra_write` parameter
   already threaded through `macos_sandbox_profile` (`validator.rs:471`, added for the coverage
   store, core#217) is the exact shape needed.
2. **Network deny is now a policy choice, not unconditional.** Validator scripts get `deny network*`
   flat. An agent CLI often *must* reach its model endpoint. Two sub-options, **OPEN — decide in
   design phase 008b**:
   - **(A) deny-all-network** — only viable if the model call is proxied through the daemon (the
     daemon is outside the sandbox and holds the credential); the worker never egresses directly.
     Strongest, but requires a daemon-side model proxy that does not exist today (OQ-SANDBOX-NET-001).
   - **(B) allow-egress-to-model-host-only** — SBPL `(allow network-outbound (remote ...))` / bwrap
     with a filtering proxy. Adapter-specific host lists; weaker; leak surface via DNS. Lower build
     cost, weaker guarantee.
   The DENY boundary's *filesystem* containment does not depend on this choice; network is a
   separable, explicitly-disclosed dimension (`SandboxLevel` already distinguishes `NetworkOnly`).
3. **Long-lived process, not a 120s script.** `VALIDATOR_TIMEOUT` (`validator.rs`) is a per-check
   bound; a worker runs a whole unit. The PID-namespace `--die-with-parent` tie stays (kills the
   tree if the daemon dies), but the wall-clock bound comes from the unit budget, not
   `VALIDATOR_TIMEOUT`.
4. **ACP session caching interaction.** ACP sessions are cached by `(run_id, cli_key)` and reused
   across turns (`probe_cached_session`, cited in DES-INPUT-GOV-006 §3.3). The sandbox wraps the
   **spawn**, so a cached session inherits the sandbox of its first spawn — same lifetime as the
   `acp_governance_env` injection, and the same reasoning applies: wrap unconditionally at spawn, do
   not try to change containment per-turn.

### 3.3 Honest limits (carried forward from the validator module's own SAFETY note)

- **Windows: `BestEffort` only.** No `sandbox-exec`/`bwrap`; `sandbox_availability()` returns
  `(BestEffort, None)`. A Windows worker gets **no kernel write containment** — it must be marked
  loudly ungoverned (ORCHESTRATOR.md §10 "no quiet governance gap") and excluded from write-heavy
  governed work. Job Objects / restricted tokens are a possible future floor — **OQ-SANDBOX-WIN-001**.
- **`sandbox-exec` is deprecated-but-present on macOS — this is a real, named risk, not a hand-wave.**
  The `sandbox-exec(1)` binary and its `sandbox_init` C API have carried a deprecation notice in the
  man page / SDK headers since ~macOS 10.7, yet the mechanism (the kernel Sandbox/`Seatbelt` KEXT it
  drives) is the *same* one App Sandbox and system daemons use and is not going away — Chromium,
  Firefox, and Apple's own services still run SBPL profiles today. The design ships on `sandbox-exec`
  now because it is present and working (`validator.rs` already depends on it in production), while
  treating removal as a tracked contingency. **OQ-SANDBOX-MACOS-001** must, at source: (i) confirm on
  the current pinned macOS target that `sandbox-exec -p <profile>` still functions for a worker spawn
  (not merely that it is deprecated on paper); (ii) evaluate the modern replacement paths and their
  fit for wrapping a *child process we launch* rather than *self-sandboxing a bundled app*:
  the **App Sandbox** entitlement (requires a signed `.app`/entitlement — a poor fit for an
  arbitrary CLI child) vs. an **Endpoint Security** (ESF) system-extension monitor (observes/authorizes
  `AUTH_*` events kernel-side, requires a signed+notarized sysextension and TCC approval — heavier,
  but the actual supported "deny writes for another process" API) vs. staying on SBPL until it breaks.
  Record which is viable with a live probe; do not assume. Until resolved, macOS containment rests on
  `sandbox-exec` with this contingency filed, exactly as Linux rests on `bwrap`.
- **A sandbox is a floor, not a policy.** It denies writes *outside* the worktree; it does **not**
  record *which* files inside were touched or *why*. That per-call semantics is Boundary 2's job.
  Do not let the sandbox's presence be misread as "governed" — reuse the `SandboxLevel` honest
  disclosure on the wire.

### 3.4 What Boundary 1 buys with zero adapter cooperation

Every seat — including `pi`/`codex`/`copilot` that failed ACP admission, on a platform with a real
sandbox tool — gets, **today, with no upstream research**: no writes outside the worktree, no network
(if we choose deny-all), no reads of the curated `~/.aws`/`~/.ssh`/credential-store denylist. That is
a strictly stronger *write-containment* floor than any of them has now (three of them have
`SandboxLevel::BestEffort`-equivalent today — "neither" in the DES-001 matrix). **It is not an
exfiltration or general read-containment floor** — see the scope note in §2 and the honest limits in
§3.3: reads outside the curated denylist stay open, and the model-API network channel stays open, so
anything the agent legitimately reads can still leave via ordinary model traffic. This is the biggest
buildable-now *write*-containment win in this document, not a data-loss-prevention control.

---

## 4. Boundary 2 — the wicked-owned governed MCP tool server (AUDIT)

### 4.1 The estate is already an MCP server — this is a second, governed one

wicked-core already speaks MCP: `mcp_call` dispatches JSON-RPC to the in-process memory tool server
(`src/lib.rs:1044-1053`), and the estate MCP server is injected into workers on **both** carriers:
- wrapped path: `--mcp-config` toolbox + `.claude/settings.json` (ORCHESTRATOR.md §3.5;
  `repo_estate_mcp_parts`, `src/execute_wrapped.rs:1196`).
- ACP path: `session/new`'s `mcpServers` array `{name,command,args,env}` to **every** ACP seat
  (`src/acp_runner.rs:1656-1668`).

So the **injection channel already exists and is already CLI-agnostic on the ACP path** (native ACP
agents even *require* `mcpServers` to be present or reject `session/new` with `-32602`). This design
adds a **second** MCP server — call it `wicked-tools` — exposing governed file/exec tools:

| MCP tool | Behaviour | Evidence emitted |
|---|---|---|
| `read` | read within read-roots; deny outside | ConformanceClaim (read intent) |
| `write` / `edit` | write within worktree; boundary+phase-scope+policy via the shared evaluator | armed marker → `evaluate_tool_call` → fired sentinel → ConformanceClaim |
| `bash` | exec with the same `DENIED_BASH` verb + path guards the wrapped path uses (`src/execute_wrapped.rs:264`) | ConformanceClaim (exec intent) |

Crucially, **the evaluator is the one that already exists**: `evaluate_tool_call` /
`claude_pretool_context` (`src/gate_hook.rs:640-702`) and the `AcpGate` boundary/policy path. The MCP
tool handler normalizes `{tool, args}` into the same `{tool_name, tool_input}` shape
`acp_permission::pretool_payload` produces (`src/acp_permission.rs:56-104`) and calls the shared
evaluator — so `wicked-tools` writes **byte-for-byte the same audit records** as the Claude
PreToolUse hook, satisfying the DES-002 equivalence contract by construction. This is the estate-is-
an-MCP-server insight applied to *governance*: governance becomes a tool surface, not a hook or a
protocol frame.

### 4.2 The hard requirement: native tools must be OFF

Injecting `wicked-tools` is necessary but not sufficient. If the adapter's **native** `read`/`write`/
`edit`/`bash` tools stay enabled, the model will mostly use those and bypass the governed path. So
Boundary 2 requires, per seat: **disable the native FS/exec tools, leaving `wicked-tools` as the only
working tool path.** This is the single per-seat variable that decides feasibility, and it is a
**config** question (can the CLI be told "no built-in file tools"?), not a *protocol-behaviour*
question — which is precisely why it is more tractable than ACP-permission admission.

One safety net makes "native tools OFF" fail-safe rather than fail-open: even if disabling is
*incomplete*, Boundary 1's kernel sandbox still contains any native write to outside the worktree. A
native-tool bypass degrades Boundary 2's *audit completeness*, not the write-*containment* guarantee.

**Correction (this doc previously claimed a second safety net that does not hold — struck, not
carried forward):** an earlier draft argued `wicked-tools` would be "the only tool that succeeds
outside the worktree" because only it could widen writes to the estate graph dir, creating a
usability gradient toward the governed path. That is false as designed: §3.2 point 1 requires
Boundary 1's own sandbox profile to include the estate graph dir in its writable set (the same
`WICKED_WRITE_ROOTS`-derived set), so that write permission is granted **at the kernel level to any
process in the sandbox** — native or MCP-mediated — not to `wicked-tools` exclusively. For claude
specifically, the existing native Write/Edit path already reaches the graph dir today via the
PreToolUse hook reading that same env (`src/execute_wrapped.rs:824-881`, core#217). So there is no
filesystem-access asymmetry to lean on here; the *only* asymmetry native-tool-disable buys is that a
governed call is **recorded** (`ConformanceClaim`) and a native call, if left enabled, is not. That
recording gap — not a write-access gap — is the entire reason native-tool-disable must be a hard gate
(below), and it is why there is no fallback "usability" argument to soften an incomplete disable: an
incompletely-disabled native surface is simply unaudited, with no compensating access restriction.

**Native-tool-disable is a HARD per-seat gate — the central admission rule for Boundary 2.** If a seat
cannot be *forced* off its native file/bash tools, then the governed `wicked-tools` path is
**bypassable**: the model can still act through native tools that emit no `ConformanceClaim`, so the
audit trail is incomplete and any "this seat is audited" claim is **false**. There is no partial credit
here — an incompletely-disabled native surface is an audit-bypass, full stop. The fail-safe rule is
therefore categorical:

> **A seat earns a Boundary-2 audit claim ONLY if native FS/bash tools are provably, completely OFF
> (confirmed by live capture, §7). A seat where they cannot be fully disabled gets Boundary-1 DENY
> ONLY, and is disclosed as "contained, NOT audited" — never marked governed/audited.**

This is why the one safety net above is framed as *degradation*, not *rescue*: it means a native-tool
bypass never breaks *write-containment* — but it does not lessen the audit loss, and it is not a
reason to relax the gate. The §5 matrix marks this failure mode explicitly per seat, and the
default-OFF `mcp_input_governance` capability (§6/§8) is what mechanically prevents an unproven seat
from being treated as audited.

### 4.3 Identity is canonical here (unlike ACP title-as-name)

A recurring ACP defect (opencode/codex/copilot) is degraded identity: `toolCall.title` is free text,
so policy "deny Bash specifically" loses precision (DES-006 §4). A wicked-owned MCP tool has a
**fixed tool name we define** (`wicked-tools/bash`), so identity is canonical by construction — this
boundary structurally fixes the precision gap the ACP path could only paper over.

---

## 5. Per-seat feasibility matrix

Columns: **(a) inject a wicked MCP server?** **(b) disable native FS/bash tools so the governed MCP
path is the only working one?** **Boundary 1 (OS sandbox)** applies to *every* seat identically on a
supported platform — it is CLI-agnostic — so it is not a per-seat variable except for the network
sub-choice. Everything marked **OPEN** requires upstream CLI research (config docs + a live probe),
exactly like the OQ-*-ACP series; the repo source cannot settle another project's config surface.

The **"if (b) fails" column is the load-bearing one** per the operator amendment: it names, per seat,
the honest posture when native tools **cannot** be fully disabled. Boundary 1 (OS sandbox) applies to
every seat identically on a supported platform, so `if (b) fails` is never "ungoverned" — it is always
"**Boundary-1 DENY only, disclosed contained-NOT-audited**."

| Seat | (a) MCP inject — evidence | (b) disable native tools — evidence | If (b) FAILS (fallback posture) | Combined Boundary-2 status |
|---|---|---|---|---|
| **claude** | **YES, repo-proven.** `.claude/settings.json` + `--mcp-config` toolbox (ORCHESTRATOR.md §3.5); estate MCP already injected both carriers. | **Plausible, repo-adjacent.** Wrapped path already uses `--disallowedTools` + `permissions.deny` (`src/execute_wrapped.rs:331`, `deny_rules()`) to fence file tools; a full `deny` of `Read/Edit/Write/Bash` leaving only `mcp__wicked-tools__*` is the same mechanism turned up to total. Needs a probe that the model actually falls back to the MCP tools. **OQ-CLAUDE-MCP-001.** | N/A for audit loss — claude keeps its **PreToolUse gate-hook** audit trail regardless (§2.1, KEPT), + Boundary-1 DENY. | **Buildable-now candidate** (strongest); also already audited via its own carrier. |
| **opencode** | **YES, repo-proven channel.** `mcpServers` in `session/new` reaches it (`src/acp_runner.rs:1656`); `OPENCODE_CONFIG_CONTENT` (DES-006) can also carry an `mcp` block. | **OPEN.** opencode config has a `tools`/`permission` surface; whether native `read/edit/bash` can be globally disabled while an MCP server stays usable is unverified. **OQ-OPENCODE-MCP-001.** | Keeps its **provisioned ACP `session/request_permission`** audit (DES-006, KEPT) + Boundary-1 DENY. Boundary 2 is redundant here. | Already audited via ACP; Boundary 2 optional. |
| **codex** | **OPEN.** codex config (`config.toml`) documents `mcp_servers`; whether `codex-acp` forwards an injected server and the model can call it is unverified. **OQ-CODEX-MCP-001.** | **OPEN, and suspicious.** codex's `auto_review` reviewer (the thing that broke ACP admission) governs *native* tools; unclear it can be turned off *and* leave the model able to act only via MCP. **OQ-CODEX-MCP-002.** | **Boundary-1 DENY only, disclosed contained-NOT-audited.** No working per-call carrier (ACP failed), so if native tools can't be forced off, codex has no audit trail. | Blocked; **most likely to fall to DENY-only.** |
| **copilot** | **OPEN.** Copilot CLI documents MCP support; injection channel over `--acp` `mcpServers` is the generic path but unproven for copilot specifically. **OQ-COPILOT-MCP-001.** | **OPEN.** Whether native file/bash tools can be disabled in headless `--acp` mode is unverified. **OQ-COPILOT-MCP-002.** | **Boundary-1 DENY only, disclosed contained-NOT-audited.** ACP admission failed (in-workspace reads ungated), so no carrier fallback. | Blocked pending research. |
| **pi** | **OPEN.** pi supports MCP servers + custom tools (this harness is pi); the `pi-acp` bridge forwarding an injected server is unverified at source. **OQ-PI-MCP-001.** | **OPEN.** Whether pi's built-in read/write/edit/bash can be disabled leaving only an MCP tool path is unverified. **OQ-PI-MCP-002.** | **Boundary-1 DENY only, disclosed contained-NOT-audited.** ACP never asks at all, so no carrier fallback. | Blocked pending research. |

**Reading the failure mode (operator amendment #2).** A seat only moves from "contained" to "audited"
by *passing (b) with live capture*. The three seats that failed ACP admission (`codex`/`pi`/`copilot`)
have **no carrier fallback**, so for them `(b) fails` collapses directly to Boundary-1-DENY-only —
contained but honestly not audited, never marked governed. `claude` and `opencode` already hold a
working per-call audit carrier (§2.1), so they retain an audit trail even if their Boundary-2 (b)
never lands; for them Boundary 2 is redundant, not required.

**Every "(b)" OPEN is the same shape** and should be resolved by the same probe recipe (§7): read the
CLI's own config docs for a "disable built-in tools" surface, set it, inject `wicked-tools`, run a
deliberately tool-using turn, and confirm from captured traffic that (i) native FS/bash tools did not
execute and (ii) the model completed the task via `wicked-tools`, emitting ConformanceClaims. Verify
at source (docs + live capture); do not infer from the presence of an `mcp` config key. **No seat's
`mcp_input_governance` flips on without this capture — the same re-proof discipline that gated
opencode's `acp_input_governance` on `oq-opencode-acp-002` (DES-006 §5).**

---

## 6. Buildable-now vs. blocked

### Buildable now (no upstream CLI research required)

1. **Boundary 1 for every seat, macOS + Linux.** Generalize `validator.rs`'s sandbox launcher to
   wrap the worker spawn, write-root = worktree (+ estate graph dir), reusing `detect_sandbox_launcher`
   / `macos_sandbox_profile` / the bwrap branch and the `WICKED_WRITE_ROOTS` set. Ship the
   `SandboxLevel` honest disclosure on the wire (an event mirroring `GovernanceUnenforced` for
   `BestEffort`/Windows). This is the single highest-value, lowest-risk item and needs nothing from
   any adapter. **Decide the network sub-choice (§3.2) as a fast follow / 008b.**
2. **The `wicked-tools` MCP server itself.** Its handlers reuse the *existing* evaluator
   (`evaluate_tool_call`), the *existing* payload normalizer (`acp_permission::pretool_payload`
   shape), the *existing* armed/fired markers and `ConformanceClaim` fold. Building the server is an
   in-repo composition of parts that already exist and are already tested; it does not require any
   CLI to accept it yet.
3. **claude end-to-end (Boundary 1 + Boundary 2).** claude's inject-MCP + deny-native mechanisms are
   repo-proven or repo-adjacent (§5). This is the reference implementation that proves the full
   two-boundary model before any research seat.

### Blocked pending upstream research (OQ items in §7)

- opencode/codex/copilot/pi **Boundary 2** — each blocked on its `(b)` OPEN (can native tools be
  disabled?) and, for the non-opencode three, its `(a)` OPEN (does injection reach the model?).
- The macOS `sandbox-exec` deprecation contingency (OQ-SANDBOX-MACOS-001) and Windows floor
  (OQ-SANDBOX-WIN-001) — do not block shipping Boundary 1 on macOS/Linux; they bound its edges.
- The network model (OQ-SANDBOX-NET-001) — blocks *only* the deny-all-network variant, not the
  filesystem-containment variant.

---

## 7. Open questions (research bar — verify at source, do not infer)

Each follows the OQ-*-ACP evidence discipline: pinned CLI version, live capture, redacted frames,
a written verdict. None may be resolved from a config-key's mere existence.

- **OQ-CLAUDE-MCP-001** — With native `Read/Edit/Write/Bash` denied and `wicked-tools` injected, does
  claude complete a deliberately tool-using unit via `mcp__wicked-tools__*`, emitting the same
  ConformanceClaims as the PreToolUse hook? (reference-implementation proof.)
- **OQ-OPENCODE-MCP-001** — Can opencode disable native `read/edit/bash` (config surface) while an
  injected MCP server remains callable, and does the model use it?
- **OQ-CODEX-MCP-001 / -002** — Does `codex-acp` forward an injected `mcp_servers` entry to a callable
  tool? Can native tools + `auto_review` be disabled leaving only the MCP path?
- **OQ-COPILOT-MCP-001 / -002** — Does `copilot --acp` deliver an injected MCP server to the model?
  Can native file/bash tools be disabled in headless mode?
- **OQ-PI-MCP-001 / -002** — Does `pi-acp` forward an injected MCP server? Can pi's built-in tools be
  disabled leaving only the MCP path?
- **OQ-SANDBOX-NET-001** — Feasibility/cost of a daemon-side model-call proxy enabling deny-all-network
  for workers vs. an allow-model-host-only egress policy.
- **OQ-SANDBOX-MACOS-001** — `sandbox-exec` is deprecated-but-present. (i) Confirm by live probe on
  the pinned macOS target that `sandbox-exec -p <profile>` still jails a worker spawn today; (ii)
  assess the modern replacement for sandboxing *a child we launch*: App Sandbox entitlement (needs a
  signed bundle — poor fit) vs. an Endpoint Security system-extension monitor (the supported
  "authorize another process's `AUTH_*` events" API — heavier: signed+notarized sysext + TCC) vs.
  staying on SBPL until it breaks. Record the viable path; do not assume.
- **OQ-SANDBOX-WIN-001** — A Windows write-containment floor (Job Objects / restricted tokens /
  WSL2 delegation) to replace `BestEffort`.

---

## 8. Rollout

1. **Phase A — Boundary 1, macOS + Linux, all seats (buildable now).** Lift `validator.rs`'s sandbox
   to wrap the worker spawn; write-root = worktree + estate graph dir; ship `SandboxLevel` disclosure;
   `BestEffort`/Windows marked loudly ungoverned. Immediate, adapter-independent containment uplift
   for the three seats that failed ACP admission. Network stays as-is (no new deny) until 008b.
2. **Phase B — `wicked-tools` MCP server + claude reference (OQ-CLAUDE-MCP-001).** Build the governed
   MCP server against the existing evaluator; prove the full two-boundary model on claude (native
   tools denied, only `wicked-tools` works, ConformanceClaims emitted). This is the equivalence proof
   the other seats' admission will be measured against.
3. **Phase C — admit research seats one at a time, evidence-gated, default OFF.** Mirror
   DES-INPUT-GOV-002's capability pattern: a per-seat, default-OFF `mcp_input_governance` capability
   on the seat config. **The flip is not a code review decision — it is an evidence decision.** It
   may be set `true` for a seat ONLY when that seat's `OQ-*-MCP-001/-002` pair has a committed
   `oq-<seat>-mcp-00N/` evidence directory (pinned CLI version, redacted live-capture `*.ndjson`
   frames, written `verdict.md`) proving, from a real turn: (i) native FS/bash tools did **not**
   execute, and (ii) the task completed via `wicked-tools` emitting `ConformanceClaim`s — exactly the
   discipline that gated opencode's `acp_input_governance` on `oq-opencode-acp-002` (DES-006 §5), and
   including a `verified_version` pin per seat (DES-006 §3.4) so an upstream update that re-enables a
   native tool downgrades the seat to disclosed-ungoverned rather than silently un-auditing it. A seat
   whose (b) capture fails stays `false` and is **Boundary-1-DENY-only, disclosed contained-NOT-audited**
   (§5). Suggested order by nearness: opencode → copilot → pi → codex (codex last; its `auto_review`
   coupling is the deepest unknown, and it is the seat most likely to end at DENY-only).
4. **Phase D — network model (008b, OQ-SANDBOX-NET-001).** Tighten Boundary 1's network dimension once
   the proxy-vs-egress-list decision is made.

**Fail-safe defaults throughout:** absent a sandbox tool → `BestEffort` disclosed, not silent; absent
a proven MCP-governance seat capability → native path with Boundary-1 containment only, disclosed
ungoverned on the audit dimension; never a quiet governance gap (ORCHESTRATOR.md §10).

---

## 9. Tradeoffs

- **Two boundaries, more surface than one hook.** More to build/maintain than the single PreToolUse
  path. Bought: a containment floor that needs zero adapter cooperation (works for the seats that
  failed every ACP attempt) **and** a canonical-identity audit trail. The ACP-permission path, by
  contrast, delivered *neither* for three of four seats after four design docs of effort.
- **Boundary 1 is a floor, not policy; audit comes from a per-call carrier.** Stated plainly so the
  `SandboxLevel`/audit disclosures are not over-read. A seat is **contained** by Boundary 1 and
  **audited** iff it has a working per-call carrier — *either* an existing one that is KEPT (claude
  PreToolUse, opencode provisioned ACP) *or* Boundary 2 (native tools provably OFF + `wicked-tools`).
  "Governed" = contained **and** audited; Boundary-1-only is honestly labelled contained-NOT-audited.
  "Contained" itself means *write*-contained (§2, §3.3–3.4): it is not an exfiltration or DLP claim —
  the model-API network channel and the non-curated-dir read surface stay open on every seat, by
  design, regardless of Boundary-1/2 status.
- **`wicked-tools` MCP re-implements file/bash tool ergonomics.** The model must be as productive via
  `wicked-tools/edit` as via a native edit tool, or task success drops. Mitigation: keep the tool
  schemas close to the adapters' native shapes; the reference-seat proof (Phase B) measures this.
- **`sandbox-exec` deprecation + Windows gap** are real, bounded, and disclosed (OQ-SANDBOX-*), not
  hidden. macOS/Linux is the covered majority today; Windows is honestly `BestEffort`.
- **Network deny may break model calls** unless proxied — deferred as a separable dimension (§3.2/
  008b) so it does not hold up the filesystem-containment win.
- **Per-seat "disable native tools" is upstream config we do not control** and could change release
  to release — the same version-pin risk DES-006 §3.4 handled for opencode; reuse the
  `verified_version` pin mechanism per seat when a seat is admitted.

---

## 10. Concrete artifacts a later build phase will touch (recon note, not an implementation request)

- **Boundary 1:** promote `src/validator.rs`'s `detect_sandbox_launcher` / `macos_sandbox_profile` /
  bwrap branch / `secret_read_block_dirs` / `SandboxLevel` into a worker-spawn-capable module used by
  `start_acp_process` (`src/acp_runner.rs:1362`) and the wrapped launch (`src/execute_wrapped.rs`);
  write-root fed from the `WICKED_WRITE_ROOTS` set + estate-home widening.
- **Boundary 2:** a new `wicked-tools` MCP server (sibling to the estate/memory MCP surfaces,
  `src/lib.rs:1044`) whose handlers normalize to the `pretool_payload` shape and call
  `evaluate_tool_call` (`src/gate_hook.rs:640`); injected via the existing `mcpServers`
  (`src/acp_runner.rs:1656`) and `--mcp-config`/settings (`src/execute_wrapped.rs:1196`) channels.
- **Seat config:** a default-OFF `mcp_input_governance` capability + per-seat "disable native tools"
  directive on `AgenticCli`/`AcpConfig` (`crates/wicked-council/src/types.rs`,`registry.rs`),
  mirroring the `acp_input_governance` + `acp_governance_env` + `verified_version` precedent, with the
  same inherit-on-omit merge semantics (DES-002 §3.6). **core-ts caveat applies** (CLAUDE.md): any new
  `AcpConfig`/`AgenticCli` field is deserialized by `crates/wicked-core-ts`; use a plain `bool`
  `#[serde(default)]` (never bare `Option`), and run `cd crates/wicked-core-ts && cargo test` after
  the change.

---

## 11. External-transform convention

This design relies on no third-party library or service that transforms a payload. The OS sandbox is
a kernel-enforced policy (macOS `sandbox-exec` SBPL / Linux `bwrap` namespaces) applied to wicked's
own child process — not a payload transformation. The MCP tool server is wicked-owned and normalizes
tool calls in-repo via the existing `acp_permission::pretool_payload` shape. Therefore no
`ASSUMPTION[external-transform]` entries apply — consistent with DES-INPUT-GOV-001 §6 and
DES-INPUT-GOV-006's external-transform note.

The one place a third-party *behaviour* is load-bearing — each non-Claude CLI's ability to disable its
native tools and forward an injected MCP server — is deliberately **not** recorded as a known
transform. It is captured as the OQ-*-MCP open questions in §7 with `confidence=needs-research`,
because the repo source cannot establish another project's config surface.

ASSUMPTION[external-transform] library=agent-cli-config(claude|codex|copilot|opencode|pi) transform=disable-native-tools + accept-injected-mcp-server confidence=needs-research :: Boundary 2 assumes each CLI exposes a config surface that (1) globally disables its built-in read/write/edit/bash tools and (2) forwards a wicked-injected MCP server to the model as a callable tool path. Verified in-repo only for the injection channel on the ACP carrier (session/new mcpServers, src/acp_runner.rs:1656) and for claude's wrapped-path deny/toolbox mechanism; the "disable native tools" half and the non-claude injection-reaches-model half are unverified per §5 and must be settled by the OQ-*-MCP live-capture probes in §7 before that seat's mcp_input_governance capability is flipped on.
