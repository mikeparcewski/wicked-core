---
id: mcp-defaults
title: MCP call governance defaults
status: active
date: 2026-09-28
enforcement_class: policy
steering_type: security
scope: wiki:governance
domain: mcp
confidence: 1.0
---

# MCP call governance defaults

Every MCP call a governed worker makes goes through crew's broker, and the broker asks core
(`mcp_gate::evaluate_mcp_call`) whether this unit may make this call. One evaluation per call, on
the same steering engine the tool gate uses: `select_any` over the unit's phase tokens plus the
MCP subject tokens, then `decide`, deny dominates (DES-MCP-TOOLS-001 §3-§4).

The subject tokens are `mcp`, `mcp:<server>`, `mcp:<server>/<tool>` and the run's mode,
`mcp-mode:<ask|balanced|autonomous>`. A rule names what it governs with `applies_to` and narrows
it with a `trigger` over the canonical context (`"class"`, `"phase_role"`, `"seat"`, `"args"`, …).

## Two tiers

**Engine gates.** The structural invariants hold in every mode and cannot be retired. They are
code, not rows, so this doc carries their recall-only doctrine twins (`MCP-D1`…`MCP-D6` below),
each naming the engine rule id a denial records.

**Posture rules.** The operator-editable posture ships as effect-bearing rules in
`rules/mcp-defaults.json`. The daemon seeds them into the store at boot, insert-only: a restart
never undoes an approval or resurrects a retired rule.

| Rule | Mode | Effect |
|---|---|---|
| `MCP-FIRST-USE` | every mode | The approvals ledger of the engine gate `engine:mcp-first-use` (`MCP-D6`), not a decide-lane rule. Approving a server adds `mcp:<server>` (or one tool, `mcp:<server>/<tool>`) to its `excludes`; a changed tool schema withdraws the approval. |
| `MCP-POSTURE-READ` (P-1) | every mode | A read tool of an approved server runs. |
| `MCP-POSTURE-WRITE` (P-2) | balanced | A write tool asks until approved. Approving it adds `mcp:<server>/<tool>` (or `mcp:<server>`) to its `excludes`. |
| `MCP-MODE-ASK-WRITE` | ask | Every write call asks. |

Autonomous mode has no write rule: after the first-use approval, reads and writes run and are
recorded. "Ask" is `allow_with_conditions` with the obligation `mcp:approval`. There is no new
effect. An approval never lifts an engine gate or an explicit `deny`, because deny dominates.

## Denied means blocked and disclosed

A denied MCP call never ran, so the refusal is recorded as the advisory claim class `mcp-deny:`
(evaluator `wicked-governance-mcp`) and disclosed as `workerToolCallDenied`. The unit continues.
A call that cannot be recorded is refused and stays fatal.

## Rules

- `MCP-D1` (critical): A unit whose write posture is read-only, or whose phase plays evaluator,
  never calls a `write` or `destructive` MCP tool, on any seat and in any mode. Enforced by the
  engine gate `engine:mcp-phase-role` (`mcp_gate::evaluate`), from the same `WritePosture` every
  carrier reads.
- `MCP-D2` (critical): Upstream secrets never reach a worker. The worker holds only its
  `WICKED_MCP_TOKEN`; the broker resolves the secret reference and scrubs results. Enforced by
  the broker (crew, DES-MCP-TOOLS-001 S3).
- `MCP-D3` (critical): Every MCP call is recorded as a claim in the unit's decisions log, or it is
  refused. Enforced by `mcp_gate::evaluate_mcp_call`, which returns `guard_error` when the append
  fails.
- `MCP-D4` (error): A tool without annotations is class `write`. Enforced by `mcp_gate::classify`,
  the one class derivation; an operator overrides it per tool in the registry.
- `MCP-D5` (critical): An unregistered, disabled or removed MCP server or tool is denied. Enforced by the engine gate `engine:mcp-unregistered`; a native `mcp__*`
  call that reaches a carrier is refused by the carrier-side fence (core#657).
- `MCP-D6` (critical): The first use of an MCP server, or of a tool whose schema changed, waits
  for the operator's approval in every run mode. Enforced by the engine gate
  `engine:mcp-first-use`, which reads the approvals from the `MCP-FIRST-USE` rule's `excludes`
  whether that rule is active or retired; a store without the rule approves nothing. A deny
  still dominates it.
