---
id: review-loop-foreground-evidence
title: "Review loops: evidence is foreground and pasted"
status: active
date: 2026-10-08
enforcement_class: guidance
steering_type: development
scope: wiki:governance
domain: review-loop
confidence: 0.9
applies_to: [build, implement, fix, execute, test, verify]
---

# Review loops: evidence is foreground and pasted

A creator turn that ends while its own background checks are still running leaves the evaluator nothing to read. The creator then claims a result the record does not show, and the round is spent re-running checks instead of reviewing the change. A failure the creator calls "pre-existing" without a base run is just a claim. A reviewer's item that an operator note left out silently drops off the list.

This rule is guidance until the ACP adapter refuses to end a turn while the worker's background tasks are running. After that it becomes a `validator`.

## Rules

- `RVWL-1002` (error): A worker turn ends only after every check it started has finished; the record carries each command run and its exit code, in the done-when recipe's order; a failure called pre-existing cites its run on the base; and every item of a reviewer's verdict is in scope unless a human ruling strikes it by name.

## Sources

- program-2026-09 design/STEERING-DRAFTS-2026-10-07.md §3 change 2 (S15e loop recon, adjudicated)
