# Seam C1 fixtures (DES-TEAMING-002 §8.3, §11.2)

Read by `tests/catalog_compose.rs`.

- `crew-defs.json`: today's defs for the §11.2 consumers that only crew defines. They were dumped
  from crew `main` at `4870c87` by evaluating crew's own exports, with no hand transcription:
  `BUILTIN_WORKFLOWS` (`capture-learnings`, `steering-author`, `qe-author-tests`),
  `withDraftSkill(<def>, true)` for `interactive-chat`, `interactive-draft` and `interactive-edit`
  (the form crew registers when garden holds the draft skill), `INTERACTIVE_DEMO_WORKFLOW_DEF`,
  `INTERACTIVE_DEMO_REAUTHOR_WORKFLOW_DEF`, and `deliverPrPhase([], ...)` for `deliver` (the phase
  `composeDeliverWorkflow` appends). `is_system` is dropped because crew strips it before the
  engine sees a def. The consumers core owns are not here: the test reads them live from
  `WorkflowRegistry::with_defaults()` overlaid with `workflows/*.json`.
- `mappings.json`: for each of the 19 §11.2 consumers, `steps` (the plan that maps today's phases
  onto catalog entries) and `bold` (§11.2's bold cells as `"<phase>.<field>": <composed value>`).
  A step carries a field only where today's phase differs from its catalog entry and the cell is
  not bold. `compose` enforces the step rules, so a step that weakened an entry would be refused.
  The bold values are fixed. They are not derived.

When an M-seam deletes a consumer's def, it moves that consumer's today-def into this directory
before deleting it, so the fixture keeps pinning the preset.
