---
id: agent-behavior
title: "Agent-behavior rules R1-R7 (A/B-validated)"
status: active
date: 2026-08-29
enforcement_class: validator
steering_type: development
scope: wiki:architecture
domain: agent-behavior
applies_to: [build, review]
---
# Agent-behavior rules (R1–R7)

Derived from `wicked-estate/docs/agent-behavior-rules.md` — empirical constraints A/B-validated
against real agent sessions; every `RetrievalTool` and the MCP server MUST implement them.
Seed-corpus item 3b (AW-13 / arch-R9 item 4). The `Rn` aliases are preserved as the leading
token of each statement, so recall on either the `PAT-` id or the historical `Rn` name lands
on the same rule. `enforcement_class: validator` — verified by estate behavior tests (W4.3).

## Rules

- `PAT-1601` (critical): R1: Never return isError:true early in a session — an early error
  causes session-wide abandonment of the tool; if the graph cannot answer, return a successful
  empty/partial result with a diagnostic.
- `PAT-1602` (error): R2: Unindexed or empty graph — expose zero tools, not erroring tools;
  a tool that exists must work.
- `PAT-1603` (error): R3: Partial coverage is worse than none — always surface coverage as a
  diagnostic so the agent knows when to fall back; track coverage as a first-class signal.
- `PAT-1604` (warn): R4: Cap tool output at roughly 25K characters — rank and budget (elided
  stubs, signatures + docstrings); prefer a tight ranked answer over a complete dump.
- `PAT-1605` (warn): R5: Always report staleness — embed commits_behind in every response's
  diagnostics; a silently-stale graph is a correctness hazard.
- `PAT-1606` (warn): R6: Loud fallback markers — emit GRAPH-FALLBACK: when files are read
  because the graph could not answer, LSP-FALLBACK: when a missing language server downgrades
  to labeled graph results, and LSP-TRUNCATED: when a References answer was capped; all ride
  the diagnostics channel, never extra stdout lines.
- `PAT-1607` (error): R7: Confidence is visible and low-confidence is labeled — never present
  a 0.5-confidence synthesized edge as if it were a 1.0 SCIP fact.

## Sources

- `wicked-estate/docs/agent-behavior-rules.md` (R1–R7, W0.7; the LSP sibling markers are
  ADR-009's addition to R6).
