---
id: phase-scope
title: Phase-scope write-denies
status: active
date: 2026-08-29
enforcement_class: policy
applies_to: [clarify, design, triage, reproduce]
scope: wiki:governance
domain: phase-scope
confidence: 1.0
---

# Phase-scope write-denies

A workflow's phase model is the product's story about **how** work is produced: recon plans,
creator builds, evaluators judge. A pre-build, non-creator phase (`feature`'s `clarify`/`design`,
`bug`'s `triage`/`reproduce` — every phase the planner marks `pre_build_scope` from def data)
declares a scope: produce the analysis/design/plan deliverable, leave implementation to the build
phase. If a recon phase can write production code, phase roles are advisory, the creator/evaluator
separation is weaker than advertised, and the run's evidence trail misattributes which phase
produced what.

That is exactly what open issue **wicked-core#296** reported: on run `d1bc72c2` the `design` unit
wrote `src/board/attentionReason.ts` and `tests/attentionReason.test.ts` before the creator phase
ran, under a prompt header that called itself "PHASE SCOPE (enforced)" — and the governance hook
allowed both writes, because the only question it asked was the filesystem one (the worktree IS
inside the unit's write roots).

## How this pack composes with the engine gate (the core#296 closure path)

Enforcement does **not** live in this document — a doc cannot enforce itself any more than a
prompt can. The deterministic enforcement is the engine-owned phase-scope gate shipped by
**wicked-core#306** (`gate_hook::phase_scope_denial`): a pre-build phase's `Write`/`Edit`/
`NotebookEdit` to a non-documentation path is refused at the tool call, on both carriers
(`BoundaryCtx::pre_build_scope` in-process, `WICKED_PRE_BUILD_SCOPE` on the hook subprocess), and
the refusal is recorded as a durable, advisory ConformanceClaim naming the denying rule
`engine:pre-build-scope`. #306 deliberately keeps that invariant OUT of the operator-editable
Policy/Trigger vocabulary — it is a structural property of the workflow def, so this pack does not
re-mint it as a `wicked-governance` Policy either (a second, drifting enforcement tier would be
worse than one honest gate).

What this pack adds is the **doctrine twin** the gate cannot carry by itself:

- the rules below are recallable (`rules recall`, estate `rules.recall`) and citable — a CI
  recall-report (AW-17) and a per-turn advisory both link a finding back to this doc by
  provenance ref;
- a gate denial's record (`policy_ids: ["engine:pre-build-scope"]`) and this doc name each other,
  so an operator can walk from the refusal to the rationale and back.

**Closure:** this pack + #306's gate together are core#296's stated closure — the scope is policy
data an operator can recall AND a gate the hook enforces, with a denial receipt
(`decision=deny, denyingPolicy=engine:pre-build-scope` where the reported run read
`decision=allow, denyingPolicy=None`). The orchestrator closes #296 once #306 merges.

## Rules

- `POL-2960` (critical): A pre-build, non-creator phase (any unit the planner marks
  `pre_build_scope` from def data — `feature`'s `clarify`/`design`, `bug`'s `triage`/`reproduce`)
  must not write production code. Its writable surface is documentation only: `.md`/`.txt`/`.rst`
  files, and paths under a `docs/` or `.product/` directory. Implementation belongs to the build
  phase. Enforced deterministically by the engine gate `engine:pre-build-scope`
  (`gate_hook::phase_scope_denial`), which refuses the `Write`/`Edit`/`NotebookEdit` at the tool
  call.
- `POL-2961` (error): A pre-build phase must not route a production write around the tool-call
  gate — a `Bash` heredoc, `git apply`, or any other shell write that lands source, config,
  lockfiles, or assets is a scope breach even though the gate structurally cannot see it. Such
  writes surface on the unit's persisted gate evidence as completion-path scope warnings
  (`actor::phase_scope_warning`, the audit backstop) and are judged against this rule.

## Appendix — scope and closure verification

The pack is deliberately only the write-denies. The adjacent lesson — a prompt, preamble, or doc
must never claim an enforcement no gate holds — is not a write-deny and is not minted as a rule
here; it is pinned mechanically in #306's plan test (`the prompt must not CLAIM enforcement`) and
recorded as doctrine in this prose.

The denial receipt core#296 asks for is reproduced as a pinned test in #306
(`a_pre_build_phase_write_to_production_code_is_denied_inside_its_own_worktree`): the exact
reported write, refused inside its own worktree, with the claim carrying
`phase-scope-deny:<phase>` / `wicked-governance-phase-scope` / `engine:pre-build-scope`. The CI
seam's job (wicked-ci `rules-conformance.yml`) is only to keep this doctrine visible on every PR —
severity-ordered, id-cited, linked back here.
