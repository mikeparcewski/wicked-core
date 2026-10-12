---
id: stack-builtins
title: "Use what the chosen stack already provides"
status: active
date: 2026-10-12
enforcement_class: guidance
steering_type: development
scope: wiki:architecture
domain: engineering-doctrine
applies_to: [build, review]
---
# Use what the chosen stack already provides

Operator ruling, 2026-10-12. The user and the steering policies choose the framework or
library. An agent works inside that choice. It does not swap in a different stack, and it does
not rebuild what the chosen one already ships. Where this came from: an MCP server had to be
exposed as an HTTP/REST API with an OpenAPI description. fastmcp, the chosen framework, has
that built in, but the plan was a hand-written generator. This doc groups under the
`engineering-doctrine` RuleSet beside the Universal Don'ts.

The check is cheap and it leaves a record. Before writing code, read the chosen stack's docs
or source at the pinned version to see what it already offers. Write down what you checked and
what you found in the step output. If you still write custom code that duplicates a built-in,
say why the built-in does not fit (a missing feature at the pinned version, a measured defect,
a policy conflict). A reviewer who finds custom code that duplicates a built-in flags it as a
finding.

## Rules

- `POL-2101` (error): The user and steering policies choose the framework or library; agents
  do not substitute another. Before writing code, check what the chosen stack already
  provides at its pinned version and record that check in the step output: what you looked
  at and what it offers. Custom code that duplicates a built-in needs a stated reason the
  built-in does not fit. Reviewers flag any unjustified duplicate as a finding.

## Sources

- Operator ruling 2026-10-12 (program NEXT-SESSION item 7): the framework is chosen by the user
  and policies; check built-ins, record the check, justify duplicates, reviewers flag them.
- Origin case: MCP-as-API uses fastmcp's built-in HTTP/REST and OpenAPI support, not a
  hand-written generator.
