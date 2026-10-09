---
id: review-loop-one-slice-intent
title: "Review loops: an intent is one slice"
status: active
date: 2026-10-08
enforcement_class: guidance
steering_type: operations
scope: wiki:governance
domain: review-loop
confidence: 0.85
applies_to: [scope, clarify, plan, design]
---

# Review loops: an intent is one slice

An 8–10 KB intent that spans several surface families, with one journey covering all of them, gives the evaluator no checklist to mark. It also gives the creator room to reinterpret boundaries the operator already decided. A slice is one surface family: expected at most about 8 files / 600 lines, at most 2 rounds budgeted, and one journey proving that family. Its sections are: where things stand; pre-ruled boundaries (one sentence per decision, naming the test pins it rewrites); do, in order; done when, as a numbered checklist that is also the reviewer's list; out of scope. Do not restate a boundary in a parenthetical, do not use a wire literal the engine may refuse, and do not append linked-issue bodies unseen. A steer about design goes to the design unit.

This rule is guidance. It becomes a `validator` once an intent lint checks length and the presence of the checklist.

## Rules

- `RVWL-1004` (warn): A governed intent names one surface family, states its boundaries as pre-ruled sentences naming the pins they rewrite, and ends in a numbered done-when the evaluator marks item by item; anything else is a second slice.

## Sources

- program-2026-09 design/STEERING-DRAFTS-2026-10-07.md §3 change 4 and §4 (S15e loop recon, adjudicated)
