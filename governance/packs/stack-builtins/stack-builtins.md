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
exposed as an HTTP/REST API with an OpenAPI description, and the plan was a hand-written
generator, made before anyone checked what fastmcp, the chosen framework, already offers for
HTTP and OpenAPI. This doc groups under the `engineering-doctrine` RuleSet beside the
Universal Don'ts.

The check is cheap and it leaves a record. Before writing code, read the chosen stack's docs
or source at the pinned version to see what it already offers. Write down what you checked and
what you found in the step output. If you still write custom code that duplicates a built-in,
say why the built-in does not fit (for example, a feature missing at the pinned version, a
measured defect, or a policy conflict). A reviewer flags every piece of custom code that duplicates a built-in as
a finding, whether or not it carries a reason, and judges whether the stated reason holds.

How the engine records and judges the check (core#1381). A code-writing step is asked for one
output line, `STACK-CHECK: looked at <docs or source> :: offers <what it provides for this job, or
nothing this change needs>`. The record clause is kept when every code-writing step's record
states the check, or when the reviewer of the step records the check it made itself. Either way, a
duplicated built-in found in the change still breaks the rule.

When it does not apply: the rule is about using a library, API or framework capability, so a change
that neither uses nor rebuilds one is not in scope. A change that touches only docs, text or images
is ruled not applicable by that fact. A change that only edits text, string literals, prompts,
comments, or the test assertions that pin them, is ruled not applicable by the judge. A change
that adds or changes logic is always judged.

## Rules

- `POL-2101` (error): The user and steering policies choose the framework or library; agents
  do not substitute another. Before writing code, check what the chosen stack already
  provides at its pinned version and record that check in the step output: what you looked
  at and what it offers. Custom code that duplicates a built-in needs a stated reason the
  built-in does not fit. Reviewers flag every duplicate of a built-in as a finding and judge
  whether its stated reason holds.

## Sources

- Operator ruling 2026-10-12 (program NEXT-SESSION item 7): the framework is chosen by the user
  and policies; check built-ins, record the check, justify duplicates, reviewers flag them.
- Origin case: MCP-as-API checks fastmcp's built-in HTTP/REST and OpenAPI support at the
  pinned version before anyone writes a generator.
