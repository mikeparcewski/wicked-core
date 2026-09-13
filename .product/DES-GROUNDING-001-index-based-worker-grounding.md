# DES-GROUNDING-001 — Index-based grounding for governed workers

- **Status:** PROPOSED (rev 2 — incorporates adversarial review)
- **Date:** 2026-09-05
- **Scope:** wicked-estate (read-only MCP mode), wicked-core (carrier wiring — keystone), wicked-crew (interactive grounding clause)
- **Related:** DES-INPUT-GOV-008 (governed MCP tool server), CREW-UX-8 (repo-snapshot grounding, superseded in part), FINDING-067 (operational-store wipe), FINDING-122 (one estate-MCP helper, two carrier shapes)

## 1. Problem

Governed workers cannot ground their work in the estate index. A worker asked to produce a document (or reason about a repo) gets a capped, single-repo **file snapshot** cloned into its inbox — or, when that snapshot fails, nothing at all — and falls back to placeholders. The estate code graph, memory, knowledge, and rules already index every repo in the project, but the worker never reaches them.

Observed live (2026-09-05): a "high-end marketing deck" document drafted through the platform came back as all placeholders. Root cause of that specific failure: the interactive-draft worker is sandboxed **unbound** (cannot read the live repo — wicked-core#294), its only grounding path is the repo snapshot, and the snapshot failed the 200 MB size check because it walks the working tree including `target/` (4.8 GB) even though the tracked content is 8 MB (`wicked-crew/.../repo-snapshot.ts:54-60`). Snapshot gone + no index access ⇒ nothing to ground on.

## 2. What already exists (and what's actually broken)

- The estate MCP (a stdio server; `tools/list` returns **24** tools — 23 always-on plus `SemanticSearch` when embeddings exist, `lib.rs:51-98`) covers code-graph, memory, knowledge, and rules. Direct handshake against the project graph returns all of them.
- A populated **145 MB multi-repo project graph** indexes all 8 wicked repos with `--repo` label prefixes on node paths (`wicked-core/…`, `wicked-estate/…`; verified) — so the engine's label-membership check can pass.
- wicked-core **already attaches** an estate MCP to every governed worker: wrapped path builds it into the `--settings` file's `mcpServers` key (`execute_wrapped.rs:1606-1624`); ACP path emits it as the `session/new` `mcpServers` array (`acp_runner.rs:1662-1671`). Both via one helper, `repo_estate_mcp_parts` (`execute_wrapped.rs:1196-1206`).

But at runtime the tools **never reach the worker's function set**. A proof run's worker said so — *"The MCP tools aren't wired into this session's function set"* — and fell back to reading files. Empirical isolation with the worker's own claude binary, the real project graph, and the real estate-mcp binary:

| Variant | MCP loaded via | Permission | Result |
|---|---|---|---|
| A / D | `--settings` `mcpServers` (current) | deny-only / skip | **`NO_ESTATE_TOOLS`** — tool absent |
| C | `--mcp-config` | skip-permissions | ✅ `SearchEntity("council")` → 20 real cross-repo results |
| F | `--mcp-config` | `acceptEdits`, no allow | Tool **registers** but call **blocked** (headless can't answer the prompt) |
| **E** | `--mcp-config` | `acceptEdits` + `permissions.allow:["mcp__wicked-estate"]` | ✅ **Works** — real cross-repo results |

**Proven conclusions:**
1. **Gap #1 (keystone).** claude only surfaces MCP tools loaded via **`--mcp-config`**, not via `--settings` `mcpServers`; and in `acceptEdits`/headless mode a registered MCP tool is still blocked unless allow-listed. Both are required; the pre-change config had **neither** (before this change, `grep mcp-config src/` returned zero hits — the estate MCP rode only the inert `--settings` `mcpServers` key).
2. **Gap #2 (smaller than first written).** The interactive-draft launch already resolves and passes a `projectGraph` binding and runs **repo-less** (`draft-events.ts:859-889`), so `run_code_graph_db` binds the labeled 145 MB project graph, not a single repo (`actor.rs:498-510`; repo-less ⇒ `repo_code_graph_db(None)=None`, `:169-173`). The single-repo graph seen in the proof (`wicked-core/.codegraph/estate.db`) was an artifact of a **generic** `chat` run via `POST /runs`, which does **not** pass the binding. So the draft path needs no gap-#2 code change — only a live check; passing the binding on generic runs is a follow-on.
3. **Gap #3.** The interactive-draft prompt still names only the file snapshot (`draft-events.ts:287-290 draftProblem`); nothing instructs the worker to ground via the estate tools.

## 3. Design

### 3.0 Safety keystone — a read-only estate MCP mode (wicked-estate, blocks everything else)

The estate MCP exposes **destructive** tools: `memory.erase` (with `scope_prefix:""` deletes **all** memories — `memory.rs:205-215`, `lib.rs:832`), `memory.capture`/`memory.learn`, `knowledge.write`/`knowledge.ingest`/`knowledge.relate` (`memory.rs:20-27`, `knowledge.rs:20-27`, dispatched whenever domains open — `lib.rs:731-746`). A worker's MCP is spawned with only `--db <code_graph_db>` and **no** `WICKED_MEMORY_DB`/`WICKED_KNOWLEDGE_DB`, so those domains default to `$WICKED_HOME/{memory,knowledge}.db` — the **operator's global stores** (`main.rs:259-265,65-71`). Allow-listing the whole server (variant E) would therefore auto-approve wiping the operator's memories — a FINDING-067-class hole. The gate-hook is no backstop (an MCP call has no path/Bash to deny — `gate_hook.rs:283-339`). No `--readonly` flag exists today (`main.rs:32-63` parses only `--db`).

**Fix — mandatory, and it must land first:** add a `--readonly` mode to `wicked-estate-mcp`. In read-only mode the server advertises and dispatches **only** the read/query tools (code-graph reads, `rules.recall`/`RulesInventory`, `memory.recall`/`memory.coverage`, `knowledge.recall`/`knowledge.recall_about_code`/`knowledge.coverage`, `SemanticSearch`) and refuses every write/destructive tool — omitted from `tools/list` and hard-rejected if called. The write set is **8** tools: `memory.capture`, `memory.erase`, `memory.learn`, **`memory.reflect`** (build-surfaced: `reflect` consolidates *and persists* distilled facts — `consolidate.rs:232`, `&mut self` — so it is a write, not a read), `knowledge.ingest`, `knowledge.write`, `knowledge.relate`, `knowledge.relate_code`. (Note: live `memory.erase` requires a non-empty `scope_prefix`, so it cannot wipe *all* memories in one call — but it is still destructive and stays omitted.) Enforced in the binary, so it covers **both** carriers (wrapped and ACP) identically — the only remedy that does, since the ACP carrier has no `permissions.allow` analogue.

### 3.1 Gap #1 — expose the (read-only) estate MCP to wrapped workers (wicked-core, keystone)

In `arm_input_governance` (`execute_wrapped.rs:1558-1668`):

1. **Load via `--mcp-config`, not `--settings`.** Write a separate per-unit mcp-config file `{"mcpServers":{"wicked-estate":{command,args}}}` (args now include `--readonly`; `repo_estate_mcp_parts` unchanged except the flag) and inject `--mcp-config <path>`. Because the flag is **variadic and takes a file path** (not comma-joinable), place it at **argv position 1** exactly like `--settings` (`:1667-1668`), not via the append-or-before-`--` path of `inject_isolation_flags` (which could let a bare positional prompt be swallowed as a second config, `:335-338,344-351`). Add an `argv_states(&["--mcp-config"])` suppression guard (`:364-370`) so an operator template that already pins it wins. Remove `mcpServers` from the `--settings` object (inert there, misleading).
2. **Allow the estate tools.** Add `permissions.allow: ["mcp__wicked-estate"]` to the `--settings` object so the tools are callable under `acceptEdits` in a non-interactive session. Whole-server allow is safe **because §3.0 makes the server read-only** — there is nothing destructive left to allow.
3. **Unchanged safety:** `None ⇒ no estate MCP` (never the operational store — FINDING-067); the `--db` handle stays repo/project-local.

### 3.2 Gap #2 — verify the draft binds the project graph (wicked-core / wicked-crew)

No draft-path code change (re-diagnosed in §2.2). Acceptance verifies live that the interactive-draft worker's estate MCP `--db` points at the 145 MB project graph. Follow-on (separate change): have the generic `POST /runs` launch also resolve and pass the `projectGraph` binding so any filed run grounds multi-repo.

### 3.3 Gap #3 — ground the interactive draft via the index (wicked-crew)

Rewrite `draftProblem` (`draft-events.ts`) so the grounding clause instructs the worker to research via the estate tools (`SearchEntity`/`ContextBundle`/`FetchContent`, and `knowledge.recall`/`rules.recall` once those domains are wired) across **all** bound repos, grounding every claim in what the tools return. **Demote the file snapshot to a fallback** used only when the estate MCP is unavailable — removing the `target/`-size failure, the 200 MB cap, and the single-repo limit for the common case.

### 3.4 Non-graph domains (follow-on)

Memory/knowledge/rules recall from a worker resolve to `$WICKED_HOME` defaults, not project stores (`main.rs:258-320`; no env set at spawn, no sidecars). Wire `WICKED_MEMORY_DB`/`WICKED_KNOWLEDGE_DB` at spawn to worker-safe project stores. Deferred: the code-graph domain alone restores grounding, and §3.0's read-only mode makes even the mis-pointed defaults non-destructive.

## 4. Acceptance (evidence-derived)

1. **Unit — wicked-estate:** in `--readonly`, `tools/list` omits every write tool and a direct `memory.erase` call is rejected; without the flag, behavior is unchanged.
2. **Unit — wicked-core:** the wrapped config emits `--mcp-config <file>` (position 1, with the estate server + `--readonly`) and `permissions.allow` includes `mcp__wicked-estate`; `--settings` no longer carries `mcpServers`. Tests to update: `execute_wrapped.rs:3818` (`mcpServers…args`), `:3839` (no-graph `is_null`), `:4990` (ArmingRunner helper) → read the new mcp-config file. **Do not touch** the ACP array test `acp_runner.rs:6271-6272` (separate carrier). `repo_estate_mcp_parts` still returns `None` for a missing graph.
3. **Live keystone proof (real armed launch, not the isolation harness):** a governed run under the full arming (gate-hook `PreToolUse` matcher `*` + `--mcp-config` + `permissions.allow`) produces a worker transcript with an actual `mcp__wicked-estate__*` read call returning real results — not `NO_ESTATE_TOOLS`, not a permission block. Re-run the exact proof that exposed the bug.
4. **Gap #2 live:** the interactive-draft worker's estate MCP `--db` points at the 145 MB project graph.
5. **Gap #3:** a regenerated interactive-draft document is grounded in real repo content with the snapshot unavailable — no placeholders.
6. **Safety / no regression:** a worker cannot erase/write the operator's memory/knowledge stores (read-only refuses); governance still enforced (gate-hook sentinel present per unit); no estate write access to the operational store.

## 5. Rollout & safety

- The read-only MCP mode (§3.0) is the safety gate and lands first; the wrapped-path change depends on it. With read-only enforced in the binary, default-ON `--mcp-config` + allow is defensible (the estate surface is now genuinely read-only and repo-scoped). Keep a one-line kill for a bad-graph escape hatch.
- **Per-call cost:** the gate-hook fires a subprocess (protocol re-probe + store open) on **every** estate tool call; a grounding-heavy worker calling `SearchEntity`/`FetchContent` many times pays it each time. Acceptable; a deployment note, and a reason the grounding clause should encourage a few broad queries over many narrow ones.
- Deploying the wicked-core change needs a napi rebuild + daemon restart (core-ts train); the wicked-estate change needs the estate-mcp binary rebuilt/installed. Serialize against in-flight governed runs.
- **ACP parity is a build task, not an assumption:** verify a codex/opencode ACP seat surfaces the `session/new` estate tools into its function set and that `AcpGate`/`permission_result` (`acp_runner.rs:2949-2980`) allows an `mcp__wicked-estate__*` read. §3.0 read-only protects the ACP carrier regardless.

## 6. Open questions / follow-ons

- **Other interactive seams (demo/video, chat, edit).** §3.3 rewrites only the DRAFT grounding clause (`draftProblem`). The demo (`demoProblem`), chat (`chatProblem`), and edit (`editProblem`) seams author governed runs too, so they inherit the keystone (their workers now get the read-only estate tools via gap #1) but their prompts do NOT direct the worker to ground via the index. Note: chat/edit are *deliberately* ungrounded today (the CREW-UX-8 split, `chat-events.ts:273`) on the premise that grounding = an expensive repo snapshot; with cheap index-tool grounding that premise is weaker. Whether revisions and demos should now ground via the index is a per-seam DESIGN decision, not an automatic copy of the draft clause — assess each before applying.
- Passing the `projectGraph` binding on generic `POST /runs` launches (§3.2), and wiring the memory/knowledge env for workers (§3.4).
- Index staleness: the project graph is a few commits behind; refresh cadence for grounding freshness.
- **(§7) Shim rule inert until wicked-garden #1130 lands** — it spells `--readonly` on the backend argv and forwards it to the spawned `wicked-estate-mcp`; until then every shim call is denied for the missing flag (safe, disclosed). Whether `nodes` / `hotspots` — pure reads the CLI also offers — join the read allowlist; they are denied fail-closed today.

## 7. Transport — Bash-path grounding allowlist (issue #463, 2026-09-13)

**Decision (2026-09-13).** Governed workers ground through **skills + a deterministic script/CLI seam**, not through a Claude-Code-registered MCP server. The organization-managed `allowedMcpServers` allowlist silently drops a handed `wicked-estate` server (core#462 detects and discloses that), but a Bash call that spawns a process is not subject to it, behaves identically on every seat CLI (MCP support is uneven outside Claude Code), and every call is visible to wicked-governance. This supersedes §3.1's premise *for grounding* — the `--mcp-config` hand-off stays as the first rung where the org policy admits it — and moves the write boundary from the MCP process flag (§3.0 `--readonly`) into a governance **allowlist rule** the gate hook enforces on the command text. It answers §6's "wiring the memory/knowledge env for workers" only in part: the rule *requires* a pinned store, §3.4 still owns *setting* one.

### 7.1 Allowlist (`classify_estate_command`, `src/gate_hook.rs`)

| Shape (judged on every pipeline / `;` / `&&` segment) | Condition | Verdict |
|---|---|---|
| `wicked-estate {query, blast-radius, rank, stats, source, semantic, cross-graph, subscribe}` | — | **ALLOW** |
| `wicked-estate clusters` | without `--annotate` | **ALLOW** |
| `wicked-estate clusters --annotate` | — | **DENY** (write) |
| `wicked-estate {index, scip, tfstate, import-telemetry, compact, watch}` | — | **DENY** (write) |
| `wicked-estate <anything else>` | — | **DENY** (unknown verb, fail-closed) |
| `wicked-estate-mcp …` | `--readonly` **and** a pinned store | **ALLOW** |
| the estate shim / a `mem` backend (§7.3) | `--readonly` **and** a pinned store | **ALLOW** |
| `wicked-estate-mcp` / shim / backend | missing `--readonly` | **DENY** |
| `wicked-estate-mcp` / shim / backend | `--readonly` but no pinned store | **DENY** |

A **pinned store** is any of: `--db <path>` / `--db=<path>` on the segment's argv; a leading `WICKED_ESTATE_DB=…` / `WICKED_HOME=…` / `WICKED_MEMORY_DB=…` assignment (bare or through `env`); or the same variables in the **worker's environment** (`ESTATE_STORE_PIN_ENV`). The worker-env fact is a *parameter* of the pure judgement, like the roots and the posture (core#260): the wrapped carrier's hook reads its own environment (the launcher re-sets `WICKED_ESTATE_DB` to the repo graph after `hardened()` — `arm_worker_estate_channel`), the ACP bridge derives it on the runner as the pins that survive `hardened()` (`WICKED_HOME` / `WICKED_MEMORY_DB`; the ACP child never receives `WICKED_ESTATE_DB` — its graph rides `session/new` `mcpServers`). `--readonly` alone is not enough: an unpinned shim resolves whatever store the cwd or the operator's defaults happen to name.

`proposal.submit` through the `--readonly` shim stays ALLOWED — it is the safe write §3.0 carved out (lands `pending`, provenance server-stamped from `WICKED_RUN_*`), and the rule never inspects the JSON-RPC payload.

### 7.2 Advisory / fatal split, and what a denied unit gets (F-RC1-046 / F-RC1-047)

Every estate deny is a **real decision record**: the tool-call annotation rides in the same buffer as the claim (so `GovernanceHookFired.toolName` and `UnitDenial.denied_tool` name the tool — never `(unknown)`), `obligations[1]` carries the offending command, and the reason names the segment and *why* (write verb / unknown verb / no `--readonly` / no pin) plus the remedy. The unit's **posture** decides the arm:

- **Advisory** — the unit's write posture fences writes or it is a pre-build rung (`WICKED_NO_CODE_SCOPE` / `WICKED_PRE_BUILD_SCOPE` on the wrapped carrier, `BoundaryCtx` on ACP): claim `estate-deny:<phase>`, advisory by the allowlist. The call is blocked, the seat is handed the remedy and continues, the unit is **not** denied, and the fold discloses `workerToolCallDenied {tool, command, reason, remedy, carrier}` — the carrier read back off the armed marker (`_wicked_gov_carrier`, written by both carriers), not assumed. This is the `remote-write-deny` precedent, **not a human pause**: with read-only grounding allowed, the F-RC1-046 shape (`wicked-estate stats` on a recon unit) no longer denies at all, and a write attempt on a recon unit is disclosed without killing the unit.
- **Fatal** — a code-executing unit: claim `boundary-deny:<phase>`, the class the fence always emitted for a write escape; the fold denies the unit (`UnitDenied`, `denied_tool` = the tool, the command in the claim) exactly as before. A write escape on the shared graph from a unit that may run code is not recoverable by "continue".

Issue #463 item 3 — a denied command on a captured unit opening a **gate** naming the command and a remedy instead of a retroactive `sessionFailed` — is the actor-seam change tracked with core#464 (the same `apply_step_result` fold); the records above are the payload that gate reads, on both arms.

### 7.3 Argv contract — the shim / backend pattern (cross-repo, wicked-garden #1130)

The fence sees only the **outer** Bash argv. wicked-garden grounds through `scripts/_estate_client.py` (the stdio shim that spawns `wicked-estate-mcp`) and the backends that import it — every `scripts/mem/*.py` (`estate_memory.py`, `auto_memorize.py`, `session_fact_extractor.py`) and `scripts/_context_backend.py` — always through a launcher: `sh "$ROOT/scripts/_python.sh" "$ROOT/scripts/mem/estate_memory.py" recall '{…}'`. The classifier therefore recognises the **script in executing position** — the program word itself, the first non-flag argument of `python*` / `py` / `sh` / `bash` / `zsh` / `dash` (looking through garden's `_python.sh` / `_run.py` resolvers), or the module of `python -m <mod>` — matched by basename (`_estate_client.py`, `_context_backend.py`) or by the `scripts/mem/` path segment. A mention is not an invocation (`grep readonly scripts/_estate_client.py` passes). A new backend that spawns the shim from another directory must be added to `ESTATE_SHIM_SCRIPTS` or live under `scripts/mem/` — until then it is invisible to the scan, exactly as before.

The contract garden #1130 implements: in governed mode the skill text spells **`--readonly` on the backend's own argv**, the backend forwards `--readonly` (and `--db`, when given) to the `wicked-estate-mcp` it spawns, and the store pin normally rides the worker environment (§7.1). Until #1130 lands, every shim call is denied for the missing `--readonly` — safe (it closes the hole the old binary-name scan never saw: the shim's program word is `python`) and disclosed with the exact remedy. End-to-end acceptance (a `capture-learnings` run lands proposals via the shim on the rig with an org MCP allowlist that lacks `wicked-estate`) needs both halves.

Limits, unchanged from the fences beside it: a literal scan does not see through `sh -c '…'`, `python -c '…'`, a renamed binary, raw SQLite, or a backend spawned from an unlisted path; the OS sandbox is the hermetic layer (§5).
