---
id: mcp-surface
title: "MCP surface doctrine: JSON-RPC everywhere, read-only rules"
status: active
date: 2026-08-29
enforcement_class: guidance
scope: wiki:architecture
domain: mcp-surface
applies_to: [plan, build, review]
---
# MCP-surface doctrine

Derived from the root `CLAUDE.md` "Key architectural decisions" and estate ADR-011/ADR-012.
Seed-corpus item 2b (AW-13 / arch-R9). The authorship half (no `rules.write`, promotion only by
doc PR) is owned by estate `POL-1101` (ADR-011) — referenced here, not duplicated.

## Rules

- `POL-1501` (error): MCP over custom protocols — every agent-facing surface speaks JSON-RPC
  2.0 (MCP); a bespoke agent protocol does not ship.
- `PAT-1502` (error): Rules are read-only over MCP: recall surfaces exist (rules.recall,
  RulesInventory, knowledge.recall), a write surface deliberately does not — the absence of a
  rules.write tool is the guardrail (see estate POL-1101 / ADR-012 for the authorship
  contract).

## Sources

- Root `CLAUDE.md` — "MCP over custom protocols — all agent-facing surfaces speak JSON-RPC 2.0".
- `wicked-estate/docs/adr/ADR-011-architecture-wiki.md` (POL-1101) and
  `ADR-012-rule-authorship.md`.
