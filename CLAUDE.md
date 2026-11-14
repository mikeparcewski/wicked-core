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
  per CLI), and `floor.Dockerfile`
  (the trusted launcher mounted into every seat, `src/container_floor.rs`).
- **`crates/wicked-core-ts` is workspace-EXCLUDED on purpose** (its own
  `[profile.release]` lto/strip shapes the shipped `.node` artifact; folding it in
  would silently drop LTO). `cargo test --workspace` never touches it — after ANY
  public-type change run its tests separately:
  `cd crates/wicked-core-ts && cargo test`
- Wire-shape gotcha for core-ts: a Rust `Option` without `skip_serializing_if`
  serializes as `null`, never absent — TS `=== undefined` guards are dead code.

## Local runner

Seats and the checks floor run as pods on a local Kubernetes cluster (Rancher Desktop or
Docker Desktop Kubernetes), installed once with `wicked-core cluster install --admin-context
<ctx> --home <state home>`. The container CLI (`docker`, `nerdctl` or `podman`) never runs a
seat: it builds images into the store the cluster shares (`imagePullPolicy: Never`) and
inspects a seat image before an arm (a short-lived helper running only the floor's busybox). Use
`wicked-core seat-image build|status` to build or report images. Every seat image builds on the
embedded checks floor recipe as a named stage (the engine also ensures the standalone checks
image, rebuilding it if removed; `WICKED_SEAT_BASE_IMAGE` overrides), which links the pinned Rust
toolchain into `/usr/local/bin`: seats have cargo and a repo `rust-toolchain` file is not applied. A seat unit's
`CARGO_HOME` always lives under `<worker home>/seat-build/<cli>` in the per-run, per-CLI replica
(never seeded or written back). A full-posture unit's `CARGO_TARGET_DIR` lives there too; a fenced
unit keeps its per-unit `scratch/target`, removed at settle.

Seat egress goes through the gateway's CONNECT listener by TLS SNI and is OPEN by default:
`WICKED_SEAT_EGRESS_HOSTS` unset or `*` renders a route with no `hostnames` key (never `["*"]`,
which agentgateway v1.6.0 matches against nothing); empty or `none` = no egress; any other value =
exactly that comma-separated list. Seats get no new credential.

## Conventions

- Single-writer store actor — never open the shared SQLite from a second writer.
- Workflows are data (`WorkflowDef` JSON; the built-in presets are `builtin_presets` in
  `src/catalog.rs`, the JSON ones under `src/presets/`); gates are
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
