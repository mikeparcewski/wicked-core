---
id: event-grammar
title: "Event grammar: four segments, whitelisted producer domains"
status: active
date: 2026-08-29
enforcement_class: guidance
scope: wiki:architecture
domain: event-grammar
applies_to: [plan, build, review]
---
# Event grammar

Derived from `wicked-bus/reqs/SPEC.md` §Naming Convention + §Validation Rules (catalog refreshed
2026-08-29 to the live emitters, DT-16) and `wicked-core` `crates/wicked-governance/src/events.rs`
(the `wicked.estate.*` governance-lifecycle producer, AW-22). Seed-corpus item 5 (AW-13 /
arch-R9 item 6). **Valued for recall/discovery, not gate enforcement** — the bus already
validates the pattern at emit (WB-001), so these rules exist to be recalled while naming a new
event, not to deny outputs.

## Rules

- `POL-1801` (error): Event types are four-segment wicked.<domain>.<noun>.<past-tense-verb> —
  all lowercase, dot-separated, matching the WB-001 pattern; three-segment names are never
  constructed, statically or dynamically.
- `POL-1802` (warn): Producer domains are whitelisted: qe, crew, garden, interactive (bus SPEC
  live-domain list) plus estate (governance lifecycle events, wicked-core AW-22); the `test`
  namespace survives only as the legacy-stable QE-lifecycle spelling emitted under the qe
  domain column. A new producer domain lands in the bus SPEC catalog before its first emit.

## Sources

- `wicked-bus/reqs/SPEC.md` — "Naming Convention", "Validation Rules (WB-001 triggers)",
  "Event Catalog" currency note (2026-08-29).
- `wicked-core/crates/wicked-governance/src/events.rs` — `wicked.estate.rule.ingested`,
  `wicked.estate.rule.retired`, `wicked.estate.doc.drifted`.
- MEMORY "wicked event grammar canonical" — SPEC.md wins over the naming skill.
