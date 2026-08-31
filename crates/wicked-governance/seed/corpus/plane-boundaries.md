---
id: plane-boundaries
title: "Four-plane model: planes, contracts, boundary rules"
status: active
date: 2026-08-29
enforcement_class: guidance
steering_type: architecture
scope: wiki:architecture
domain: plane-boundaries
applies_to: [plan, build, review]
---
# Plane boundaries — the four-plane model as enforceable doctrine

Derived from `scratch/TARGET-ARCHITECTURE.md` (adopted 2026-08-11, amended 2026-08-29 — "one
surface, one control plane, one catalog, one record") and the root `CLAUDE.md` "Key architectural
decisions" table. Seed-corpus item 1 (AW-13 / arch-R9).

**Provenance note (parked P-4):** `TARGET-ARCHITECTURE.md` lives at the workspace root's
`scratch/` directory, which is NOT inside any git repo — its owned home is an open decision
(RECON-ARCH-WIKI open question 4). Until that lands, this seed doc is the ingestable projection
and `seed/manifest.json` records the upstream path + content sha the projection was taken from.

The model: **experience** (wicked-studio) → **control** (wicked-crew + wicked-core engine) →
**capability** (wicked-garden) → **foundation** (wicked-estate + bus/vault/ledger +
wicked-interactive). Four roles, four contracts — not four repos, not four binaries.

## Rules

- `POL-1301` (critical): Every cross-plane interaction goes through the owning plane's
  contract, never around it: experience reaches control only via crew's public `/api/v1` + WS,
  control drives capability only as governed workers over workflows-as-data, and capability
  reaches the foundation only through garden's skills (estate MCP, vault/ledger evidence,
  bus events).
- `PAT-1302` (error): The experience surface is a pure HTTP/WS client of the control plane's
  public API — rendering and editing only, nothing semantic; a surface that imports control
  internals has crossed the plane boundary.
- `PAT-1303` (error): Control means intent in, governed verified work out: evaluator≠creator
  (a creator structurally cannot self-grade), deny-dominates dual gates, and "done" re-derived
  from evidence — never asserted.
- `PAT-1304` (error): Enforcement is two-tier by design: hooks are fail-open advisory,
  crew/core gates are fail-closed — never introduce a third enforcement tier
  (TARGET-ARCHITECTURE contract 3).
- `PAT-1305` (error): estate is what the system knows (data plane); wicked-core is how crew
  executes (control runtime) — neither leaks into the other's box, and the published
  version-deps pattern keeps them decoupled with zero repo-layout constraints.
- `PAT-1306` (warn): Single binary per product — no micro-process soup.
- `PAT-1307` (warn): wicked-estate is the center of gravity: code graph, memory, and knowledge
  live there; everything else is a consumer.
- `PAT-1308` (warn): Rust for foundations, JS/TS for products and tooling.

## Sources

- `scratch/TARGET-ARCHITECTURE.md` — "The model", "The four contracts", "Boundary rule
  (core vs estate)" (workspace root, outside git — see the provenance note above).
- Root `CLAUDE.md` — "Key architectural decisions".
