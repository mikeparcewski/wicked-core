---
id: universal-donts
title: "Universal Don'ts (estate engineering doctrine)"
status: active
date: 2026-08-29
enforcement_class: guidance
steering_type: development
scope: wiki:architecture
domain: engineering-doctrine
applies_to: [build, review]
---
# Universal Don'ts

Derived from `wicked-estate/CLAUDE.md` "Universal Don'ts" — already enforceable-shaped
(arch-R9 item 3). Seed-corpus item 3a. The per-edge confidence/provenance don't is deliberately
NOT duplicated here: it is `PAT-1703` in `engine-contract.md`, where it carries the enforcing
`symbol_ref`.

## Rules

- `POL-2001` (critical): No grandfathering — warnings, clippy lints, and failing/ignored tests
  go to zero by fixing code, never by allow-attributes, ignore-markers, or skipping; done means
  0 warnings, 0 ignored, conformance and bench green.
- `POL-2002` (critical): The verdict is the verdict — conformance, bake-off, benchmark, and
  gate results never change based on who runs them or how urgent the moment is; a NO-GO is a
  NO-GO for everyone.
- `PAT-2003` (error): Rules as data, not code — resolution/extraction logic lives in query
  files and config, never compiled per-language match arms; a new language is a new grammar
  plus query file with zero core change.
- `PAT-2004` (error): Stable IDs only — never key a node by content hash or line number
  (ADR-002); that was the rename-breaks-everything bug.
- `PAT-2005` (error): Bounded traversal only — every traverse carries max_depth and max_nodes;
  no unbounded whole-graph walks, use a real recursive traversal, never N statements per node.
- `PAT-2006` (warn): Never claim dead code without history — run git log -S for the symbol
  first; "no callers" can mean an incomplete migration, not permission to delete.

## Sources

- `wicked-estate/CLAUDE.md` — "Universal Don'ts".
