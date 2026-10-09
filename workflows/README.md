# Workflows are data, not code

A workflow is a **JSON file**, not a Rust edit. Drop a `*.json` file in a
workflows directory and the engine registers it at startup — no recompile, no PR
to `wicked-core`. This is the Law-2 seam: **new workflow = data; new primitive =
code.**

## How registration works

```rust
let mut registry = WorkflowRegistry::with_defaults(); // compiled-in seed (feature/bug/migration)
registry.load_dir("~/.wicked/workflows")?;            // overlay: your drop-in files
```

- The three built-ins (`feature`, `bug`, `migration`) are **seeded in-code** so
  they are always available with zero filesystem dependency.
- `load_dir` overlays every `*.json` file in a directory. A file whose `id`
  matches a built-in **replaces** it, so you can tune a shipped workflow or add
  entirely new ones by dropping a file.
- Files load in filename order (deterministic). A malformed or invalid file is a
  **loud error naming the file** — never a silent skip.
- The `feature`/`bug`/`migration` `*.json` files in *this* directory are the
  human-editable mirror of the seed builders (a drift-guard test keeps them
  identical). Copy one as a starting point.
- `chat` and `onboarding` are no longer workflows: they are **built-in presets**
  (`src/catalog.rs` `builtin_presets`, DES-TEAMING-002 M3/M4), launched by the
  same name. `survey-repo`, `memories`, `domain-graph-slice` and `collab` were
  deleted (no launcher used them).
- `domain-extraction.json` is a **shipped drop-in** (not a seeded built-in): it is
  registered only via `load_dir`, and demonstrates a *gated* workflow — its
  `coverage` phase carries an approved `validator_pin` (the coverage == 1.0
  terminal). The authored validator behind that pin lives in
  `src/domain_extraction.rs`; a test re-derives the pin so the JSON and the vaulted
  approved validator can never drift. See *Gating a phase* below.
  - **One-time seed step (required to run it).** Because the gate `validator_pin`
    must resolve to an **approved** validator in your vault, and this deterministic
    `coverage.py --check` port is not authored by the LLM writer path, run
    **`wicked-core seed-domain-validators`** once to vault + approve it. Without the
    seed, a run of this drop-in fails **closed** at plan time ("validator pin not in
    the vault") — deny-dominates, never a silent pass. The seed is idempotent
    (content-addressed) and yields exactly the pin embedded in the JSON.
- `mcp-server.json` is a **shipped drop-in** (not a seeded built-in) that makes an
  MCP server from an OpenAPI document or hand-written integration code, driven by the
  `wicked-garden-mcp-scaffold` skill, with the contract-testing, security and
  observability specialists as its test and review seats and an operator-gated Tool
  phase `install` that installs or updates the built server for running (wicked-crew
  places its composed `deliver` before it). It carries only the evidence-floor pin
  (`e2e7af1db9e48454`, seeded on the plan path), so it needs no one-time seed step. Its
  doctrine is the `governance/packs/mcp-server/` steering pack (MCPS-1001..1007).

## The minimal workflow

Only `id` is required on a phase — everything else defaults:

```json
{
  "id": "spike",
  "phases": [
    { "id": "explore",   "kind": "recon" },
    { "id": "prototype", "kind": "build", "depends_on": ["explore"] }
  ]
}
```

## Workflow fields

| Field | Default | Meaning |
|---|---|---|
| `id` | *(required)* | The workflow id a launch names (`--workflow <id>`). A drop-in whose `id` matches a built-in replaces it. |
| `phases` | *(required)* | The ordered phases — see *Phase fields*. |
| `base_skill_ref` | *(absent)* | The BASE skill every **agent** phase of this workflow follows (core#468): the engine leads every unit prompt with `Invoke your skill "<base>" … and follow its §<role> section` (`creator` \| `evaluator` \| `neutral`, from the phase's `role`) before the phase's own `skill_ref` directive. Absent ⇒ the engine-config default `WICKED_BASE_SKILL_REF` (unset ⇒ no base skill); `""` ⇒ an explicit opt-out for this workflow. Gated at intake: a run whose skills snapshot lacks the skill is refused before any unit is planned. Never narrows seat selection. |

## Phase fields

| Field | Default | Meaning |
|---|---|---|
| `id` | *(required)* | Unique within the workflow; referenced by `depends_on`. |
| `kind` | `"build"` | Methodology badge: `recon` \| `build` \| `review` \| `test`. |
| `instructions` | *(absent)* | Per-phase instructions folded into the phase's unit prompt (what THIS phase's slice of the work is) and printed on its gate card. Absent stays absent on the wire. |
| `gate_type` | `null` | Where the gate sits in the ladder: `value` \| `strategy` \| `execution` (`null` = ungated). |
| `gate` | `"auto"` | Confirm policy — see below. |
| `executes_code` | `false` | Phase changes the tree under review (provisions a git worktree, enables code tools). `false` means the phase does NOT change that tree — it is worktree-guarded and, unless it plays `creator`, read-only at the tool boundary. A `creator` phase with `executes_code: false` (a document/proposal deliverable) still writes into the run's declared `extra_write_roots` (F-4R2-004); a phase that must leave a file IN the tree declares `true`. |
| `verified_evidence` | `false` | Phase verdict must re-run the pinned verifier (re-verified evidence). |
| `required_deliverables` | `[]` | Files that MUST exist for the structural gate (fail-closed if missing). A zero-byte file or an empty directory counts as missing, and so does a symlink that resolves outside the run's cwd or declared write roots. |
| `depends_on` | `[]` | Phase ids that must finish first (intra-workflow DAG; validated acyclic). |
| `role` | `"neutral"` | `creator` (does the work) \| `evaluator` (reviews a creator's output cold) \| `neutral`. |
| `skill_ref` | `null` | Skill that drives the phase, headless (e.g. `wicked-testing-semantic-reviewer`). |
| `allowed_skills` | `[]` | Runtime skill allowlist for the phase's agent — the tool/skill scope it may load (least-privilege, like `--allowedTools`). |
| `validator_pin` | `null` | Content-hash pin of an **approved** deterministic validator in the vault. When set, the run loads it at plan time and the dual-validator gate re-verifies the phase's work against the worktree (deny-dominates). See *Gating a phase* below. |
| `executor` | `{"type":"agent"}` | How the phase runs: `agent` (a council-routed CLI seat) or `{"type":"tool","cmd":[...]}` (the `cmd` argv run directly, no seat). See *Tool executor* below. |

## Tool executor

A phase with `"executor": {"type": "tool", "cmd": [...]}` runs its `cmd` as written in the
run's worktree — no council, no seat. Notes:

- The launch preflight refuses a run whose Tool `cmd[0]` does not resolve (on `PATH` or at that
  path), loudly, before any unit is planned.
- A Tool phase satisfies the registration rule that an `executes_code` phase with an `auto` gate
  must carry a pin; it may still carry a human gate (`mcp-server`'s `install` carries
  `consent_before`).
- A Tool phase runs with the run's worktree as its working directory and inherits the daemon's
  environment, plus the run variables (core#776): `WICKED_RUN_ID`, `WICKED_RUN_UNIT` (the
  unit's ordinal), `WICKED_TREE` (the worktree's tree id, snapshotted when the unit starts; unset
  for a repo-less run) and `WICKED_EVIDENCE_ROOT` (the run's evidence root, when the launcher
  minted one), and `WICKED_GARDEN_ROOT` — the skills generation the run was admitted against
  (core#802), so a command runs `"$WICKED_GARDEN_ROOT/scripts/wicked-garden" run <script>` from
  the published snapshot instead of whatever `wicked-garden` a login shell's `PATH` finds (use
  `bash -c`, never `bash -lc`). The walkthrough recorder declares its own set (`src/walkthrough.rs`).
- **Send back** on a failed Tool phase re-runs that Tool phase only (core#803); it never
  reworks an earlier creator.

## Gating a phase (validator_pin)

The built-in defs ship `validator_pin: null` — **ungated**. To gate a phase, author + approve a validator, then reference its pin:

```
wicked-core provision-validator --criterion "the CHANGELOG has a new dated entry"   # → an UNAPPROVED pin
wicked-core approve-validator   --pin <that pin>                                     # → the APPROVED pin
```

Put the **approved** pin on the phase (`"validator_pin": "<approved pin>"`). At runtime the gate loads it from the vault and re-verifies it against the run's worktree (deterministic, deny-dominates) alongside the agent judge. A pin that isn't in the vault, or isn't approved, **fails closed** at plan time (the run won't proceed ungated).

## Gate policies (`gate`)

```json
"auto"                                        // no human pause
{ "human_confirm": { "unconditional": true } } // always pause for a human
{ "human_confirm_if": "verdict_not_pass" }      // pause only when the verdict is not PASS
"consent_before"                               // pause BEFORE the phase runs (core#801)
```

Every gate except `consent_before` fires AFTER its phase's work: the pause before unit N asks
about unit N-1's output. `consent_before` asks before the phase itself runs
(`awaitingHuman{gateKind: "consent"}`, its prompt = the phase's `instructions`), for a phase with
side effects outside the run such as `mcp-server`'s `install`. The run-level policy, `autoDeliver`,
a released plan and standing orders never skip it, and nothing pauses after the phase.

## Validation

Every def is validated on load: non-empty, unique phase ids, every `depends_on`
resolves, and the dependency graph is acyclic (Kahn). Invalid files are rejected
with the filename in the error.
