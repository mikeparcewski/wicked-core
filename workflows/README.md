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
- The `*.json` files in *this* directory are the human-editable mirror of the
  seed builders (a drift-guard test keeps them identical). Copy one as a starting
  point.

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
| `skill_ref` | `null` | Skill that drives the phase (e.g. `wicked-testing-semantic-reviewer`). |

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
