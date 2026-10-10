# Seam C1 fixtures (DES-TEAMING-002 §8.3, §11.2)

Read by `tests/catalog_compose.rs`.

- `crew-defs.json`: today's defs for the §11.2 consumers that only crew defines. They were dumped
  from crew `main` at `4870c87` by evaluating crew's own exports, with no hand transcription:
  `deliverPrPhase([], ...)` for `deliver` (the phase `composeDeliverWorkflow` appends). `is_system` is dropped because crew strips it before the
  engine sees a def. The consumers core owns are not here: the test reads them live from
  `WorkflowRegistry::with_defaults()` overlaid with `workflows/*.json`.
- `mappings.json`: for each of the 13 remaining §11.2 consumers, `steps` (the plan that maps today's phases
  onto catalog entries) and `bold` (§11.2's bold cells as `"<phase>.<field>": <composed value>`).
  A step carries a field only where today's phase differs from its catalog entry and the cell is
  not bold. `compose` enforces the step rules, so a step that weakened an entry would be refused.
  The bold values are fixed. They are not derived.

- `migrated-defs.json`: the today-defs of the consumers whose def a migration seam deleted: `chat`
  (M3, the deleted `workflows/chat.json`) and `onboarding` (M4, the deleted `onboarding_def()`,
  serialized with every field spelled), `qe-author-tests` (M10, crew's def, unchanged since the
  dump), `capture-learnings` (M7, crew's def as of crew `842f099`, current instructions and
  `requires_capture_report` on capture) and `steering-author` (M8, crew's def as of crew
  `842f099`, with the #789 reply-is-the-proposal instruction) and `interactive-chat`,
  `interactive-draft` and `interactive-edit` (M9, crew's defs as of crew `fb1bca7`, where the draft
  is one plan-then-write phase: `withDraftSkill(<def>, true)`, the form crew registered when garden
  held the draft skill). They keep pinning the built-in presets
  that replaced them.
  `survey-repo`, `memories`, `domain-graph-slice` and `collab` were deleted outright with no preset
  (operator decision, 2026-09-26), so they have no fixture and no mapping.

When an M-seam deletes a consumer's def, it moves that consumer's today-def into this directory
before deleting it, so the fixture keeps pinning the preset.

`interactive-demo` and `interactive-demo-reauthor` (M9b) have no fixture: the built-in `demo` preset
replaces them with the wicked-garden demo skill's plan → record → review (studio#373) instead of
mapping their phases, so there is no today-def for it to equal. `tests/presets.rs` pins its steps.
