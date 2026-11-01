# wicked-core

In-process composition runtime AND the execution engine behind wicked-crew.
Thin pointer stub — the real references are `README.md`, `DESIGN.md` and
`ORCHESTRATOR.md`. The `.product/` design docs (DES-*, REQ-*, ADRs, evidence
directories) are cited all over the source but are **gitignored** (`.gitignore`
line 19) and tracked in no commit — a fresh clone has none of them. Treat every
`.product/…` path in a comment as a pointer to a local-only artifact that may or
may not exist on the machine you are reading from, never as something you can
open to check a claim.

## Layout (the part that bites)

- **The engine is the ROOT `src/` crate** (`wicked-core`, lib + `src/bin/wicked-core.rs`
  CLI) — NOT something under `crates/`. `crates/` holds the workspace members it
  composes: `wicked-apps-core` (estate API + node/edge/event consts),
  `wicked-governance` (deterministic policy/conformance engine; owns the
  governance JSON Schemas in `crates/wicked-governance/schemas/`),
  `wicked-orchestration`, `wicked-council`.
- **`containers/` holds the images the engine builds itself** (from the file alone, no
  build context, tagged by a hash of its inputs): `checks/node22-rust.Dockerfile` (the
  repo-checks floor, `src/checks_container.rs`), `seats/seat.Dockerfile` (one seat image
  per CLI), `seats/netns.Dockerfile` (each seat's network sidecar) and `floor.Dockerfile`
  (the trusted launcher mounted into every seat, `src/container_floor.rs`).
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
  deny-dominates. evaluator≠creator is enforced where it can be and DISCLOSED where it
  cannot (`src/distribute.rs`): a roster whose distinct seat is benched, or a run whose
  assurance contract enforces `distinct_evaluator` (every run not launched `reduced`, unless an
  ordinary run's workflow drops the instrument; a team run needs the explicit `reduced`),
  refuses with `NoEligibleSeat`; otherwise a bench-free run on a too-small roster keeps the
  review on the creator seat and stamps `distinctness_fallback: "creator_seat"` (a second
  instance of the same CLI is `"same_cli_instance"`). A pinned gate with no distinct judge
  skips the judge (`judge_skipped`, wire `agentVerdict: "skipped"`, `judgeDistinct: false`)
  and the deterministic floor decides alone. Don't document either as unconditional.
- PR merge protocol per the ecosystem root `CLAUDE.md`: branch, wait for bot
  reviewers + CI, address comments, then merge.
