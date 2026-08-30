---
id: storage-doctrine
title: "Storage doctrine: embedded-first, trait-fronted, single-writer"
status: active
date: 2026-08-29
enforcement_class: guidance
scope: wiki:architecture
domain: storage-doctrine
applies_to: [plan, build, review]
---
# Storage doctrine

Derived from the root `CLAUDE.md` "Key architectural decisions" and `wicked-estate/CLAUDE.md`
"Locked decisions". Seed-corpus item 2 (AW-13 / arch-R9). The engine-level graph invariants
(edge direction, two-phase pipeline, per-edge provenance) live in the sibling
`engine-contract.md` under this same `storage-doctrine` RuleSet.

## Rules

- `POL-1401` (error): The embedded durable store (SQLite, ACID/WAL, crash-safe) is the
  zero-infra default; Postgres is optional behind a feature/profile — the local case never
  requires a server to run.
- `PAT-1402` (error): Storage lives behind the `GraphRead`/`GraphWrite` traits and a single
  `open_store(spec)` factory — a new backend drops in as one factory arm, never as call-site
  branching.
- `PAT-1403` (error): One writer per store file: writes go through a single-writer seam (the
  crew actor in-daemon, or exactly one CLI process offline); a daemon-held store is never
  CLI-written (DES-OUTGOV-008 — record the crew-api transport instead).

## Sources

- Root `CLAUDE.md` — "Embedded durable store (SQLite), Postgres optional"; wicked-core row
  ("single-writer actor, no SQLite races").
- `wicked-estate/CLAUDE.md` — "Locked decisions — do not relitigate" (storage bullet).
- `wicked-core/.product/DES-OUTGOV-008-fanout-placement.md` — daemon-held stores are never
  CLI-written.
