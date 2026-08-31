---
id: engine-contract
title: "ENGINE-CONTRACT invariants: the doc-to-gate exemplar pairing"
status: active
date: 2026-08-29
enforcement_class: validator
steering_type: architecture
scope: wiki:architecture
domain: storage-doctrine
applies_to: [build, review]
targets:
  language: rust
---
# Engine-contract invariants

Derived from `wicked-estate/docs/ENGINE-CONTRACT.md` — "the hard invariants every crate must
honor", enforced by `wicked_estate_core::conformance::graph_store_suite` and the edge-direction
tests. Seed-corpus item 4 (AW-13 / arch-R9 item 5): the exemplar doc↔gate pairing — each rule
carries a `symbol_ref` to the code that enforces it, so `wicked-core rules relink` re-derives
the `Governs` edge after every `wicked-estate index` (the durable-by-name, derived-by-id
contract, AW-9 / arch-R6).

`enforcement_class: validator` — these statements are verified by a deterministic test suite,
not by regex-over-output policies.

## Rules

- `PAT-1701` (critical): Edge direction: source = dependent, target = dependency — "A calls B"
  is an edge from A to B; dependents of X are edges where target == X, and blast radius is
  transitive dependents (reverse reachability).
  symbol_ref: crates/wicked-estate-core/src/conformance.rs::graph_store_suite
- `PAT-1702` (error): Two-phase pipeline: extractors are stateless and per-file (parallel);
  cross-file references emit UnresolvedRef bound later by swappable resolvers — changing
  resolution never requires re-parsing. A reference is unresolved iff no resolver emitted an
  edge attributed to it (exact location + kind, after per-ref re-resolution) — the one
  definition every other surface cites.
  symbol_ref: crates/wicked-estate-resolve/src/lib.rs::resolve_all_with_coverage
- `PAT-1703` (error): Every Edge carries confidence, provenance, and resolved_by; a heuristic
  edge is never presented as a fact (agent rule R7 is the recall-side twin).
  symbol_ref: crates/wicked-estate-core/src/edge.rs::Edge
- `POL-1704` (critical): Every graph store must pass the GraphStore conformance suite before it
  ships — the suite is the gate, the contract doc is the rationale; a store that skips the
  suite does not merge.
  symbol_ref: crates/wicked-estate-core/src/conformance.rs::graph_store_suite

## Sources

- `wicked-estate/docs/ENGINE-CONTRACT.md` §1 (edge direction), §2/§2.1 (two-phase pipeline +
  unresolved definition), §4 (GraphStore contract).
- Enforcing code: `wicked_estate_core::conformance::graph_store_suite`,
  `wicked_estate_resolve::resolve_all_with_coverage`, `wicked_estate_core::Edge`.
