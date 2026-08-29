# wicked-core

In-process composition runtime AND the execution engine behind wicked-crew.
Thin pointer stub — the real references are `README.md`, `DESIGN.md`,
`ORCHESTRATOR.md`, and the design docs in `.product/`.

## Layout (the part that bites)

- **The engine is the ROOT `src/` crate** (`wicked-core`, lib + `src/bin/wicked-core.rs`
  CLI) — NOT something under `crates/`. `crates/` holds the workspace members it
  composes: `wicked-apps-core` (estate API + node/edge/event consts),
  `wicked-governance` (deterministic policy/conformance engine; owns the
  governance JSON Schemas in `crates/wicked-governance/schemas/`),
  `wicked-orchestration`, `wicked-council`.
- **`crates/wicked-core-ts` is workspace-EXCLUDED on purpose** (its own
  `[profile.release]` lto/strip shapes the shipped `.node` artifact; folding it in
  would silently drop LTO). `cargo test --workspace` never touches it — after ANY
  public-type change run its tests separately:
  `cd crates/wicked-core-ts && cargo test`
- Wire-shape gotcha for core-ts: a Rust `Option` without `skip_serializing_if`
  serializes as `null`, never absent — TS `=== undefined` guards are dead code.

## Conventions

- Single-writer store actor — never open the shared SQLite from a second writer.
- Workflows are data (`WorkflowDef` JSON, see `workflows/`); gates are
  deny-dominates and evaluator≠creator.
- PR merge protocol per the ecosystem root `CLAUDE.md`: branch, wait for bot
  reviewers + CI, address comments, then merge.
