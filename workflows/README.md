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
- `domain-extraction.json` is a **shipped drop-in** (not a seeded built-in): it is
  registered only via `load_dir`, and demonstrates a *gated* workflow — its
  `coverage` phase carries an approved `validator_pin` (the coverage == 1.0
  terminal). The authored validator behind that pin lives in
  `src/domain_extraction.rs`; a test re-derives the pin so the JSON and the vaulted
  approved validator can never drift. See *Gating a phase* below.

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

## Phase fields

| Field | Default | Meaning |
|---|---|---|
| `id` | *(required)* | Unique within the workflow; referenced by `depends_on`. |
| `kind` | `"build"` | Methodology badge: `recon` \| `build` \| `review` \| `test`. |
| `gate_type` | `null` | Where the gate sits in the ladder: `value` \| `strategy` \| `execution` (`null` = ungated). |
| `gate` | `"auto"` | Confirm policy — see below. |
| `executes_code` | `false` | Phase runs code (provisions a git worktree, enables code tools). |
| `verified_evidence` | `false` | Phase verdict must re-run the pinned verifier (re-verified evidence). |
| `required_deliverables` | `[]` | Files that MUST exist for the structural gate (fail-closed if missing). |
| `depends_on` | `[]` | Phase ids that must finish first (intra-workflow DAG; validated acyclic). |
| `role` | `"neutral"` | `creator` (does the work) \| `evaluator` (reviews a creator's output cold) \| `neutral`. |
| `skill_ref` | `null` | Skill that drives the phase, headless (e.g. `wicked-testing-semantic-reviewer`). |
| `allowed_skills` | `[]` | Runtime skill allowlist for the phase's agent — the tool/skill scope it may load (least-privilege, like `--allowedTools`). |
| `validator_pin` | `null` | Content-hash pin of an **approved** deterministic validator in the vault. When set, the run loads it at plan time and the dual-validator gate re-verifies the phase's work against the worktree (deny-dominates). See *Gating a phase* below. |

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
```

## Validation

Every def is validated on load: non-empty, unique phase ids, every `depends_on`
resolves, and the dependency graph is acyclic (Kahn). Invalid files are rejected
with the filename in the error.
