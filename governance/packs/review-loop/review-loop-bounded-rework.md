---
id: review-loop-bounded-rework
title: "Review loops: rework is bounded"
status: active
date: 2026-10-08
enforcement_class: guidance
steering_type: operations
scope: wiki:governance
domain: review-loop
confidence: 0.9
applies_to: [build, implement, fix, review, adversarial-review, test, verify]
---

# Review loops: rework is bounded

Nothing used to cap an evaluator's send-backs, so a unit could cycle until the operator stepped in. The cap is two send-backs. This mirrors the engine's per-step rework bound and BUILD-PLAN §12.2. The third FAIL pauses for a human adjudication with three arms: land with carried items (recommended when every item is RULED, about test shape, or can become a follow-up; the carried list goes onto the unit and the PR body), send back once more (an explicit extra round), or re-slice (stop and keep the worktree).

No gate enforces this yet. The planned engine gate is `engine:review-adjudication`. Until it ships, the operator applies the cap by hand: at the third FAIL, approve with the carried list. When the gate lands, this rule becomes a `policy` that names it.

## Rules

- `RVWL-1003` (error): An evaluator may send a unit back at most twice; the third FAIL pauses for a human adjudication that lists re-raised versus new items and offers land with carried items, one more round, or re-slice; no seat error (a quota or auth refusal as the sole output) counts as a completed turn, no floor result stands across a rework, and no stalled unit is handed to a benched seat.

## Sources

- program-2026-09 design/STEERING-DRAFTS-2026-10-07.md §3 change 3 (S15e loop recon, adjudicated)
- program-2026-09 BUILD-PLAN.md §12.2
