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
1. **Gap #1 (keystone).** claude only surfaces MCP tools loaded via **`--mcp-config`**, not via `--settings` `mcpServers`; and in `acceptEdits`/headless mode a registered MCP tool is still blocked unless allow-listed. Both are required; the current config has **neither**. (`grep mcp-config src/` → zero hits.)
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
