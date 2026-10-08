---
id: review-loop-stateful-verdicts
title: "Review loops: verdicts are stateful"
status: active
date: 2026-10-08
enforcement_class: guidance
steering_type: testing
scope: wiki:governance
domain: review-loop
confidence: 0.9
applies_to: [review, adversarial-review, test, verify, security-review, observability-review]
---

# Review loops: verdicts are stateful

A reworked unit used to go back to an evaluator that saw none of the earlier rounds: it re-derived the verdict from scratch, raised items a human had already ruled on, and found new items it could have raised in round one. One slice went fourteen Challenge rounds this way (S15e recon, 2026-10-07). A verdict can only converge if the evaluator gets the unit's history: the done-when as a checklist, every prior verdict with the creator's fixed / declined-with-reason marks, every operator ruling verbatim, and the floor's record for the tree under review.

This rule is guidance until the engine hands that context to the evaluator. After that it becomes a `validator`.

## Rules

- `RVWL-1001` (error): An evaluator re-deriving a verdict on a reworked unit receives the done-when checklist, every prior verdict and every human ruling on that unit, and the floor's record; it marks each item PASS, FAIL or RULED, never re-raises a RULED item (a RULED item is never Critical), and names why a new item was not visible in an earlier round — otherwise the item is a follow-up, not a FAIL cause; a verdict with conditions and no Critical recommends approve with carried conditions, and a verdict produced without that record is advisory, not a gate.

## Sources

- program-2026-09 design/STEERING-DRAFTS-2026-10-07.md §3 change 1 (S15e loop recon, adjudicated)
