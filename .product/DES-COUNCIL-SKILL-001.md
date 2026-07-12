---
name: DES-COUNCIL-SKILL-001-council-assigns-skill
title: council-assigns-skill — the council picks a phase's skill when the def leaves skill_ref open
status: draft
version: 0.1
date: 2026-07-09
author: mike.parcewski@gmail.com
review-required: true
grounded-in:
  - crates/wicked-council/{types.rs,dispatch.rs,synthesis.rs,registry.rs,worker.rs} — the real council
    (CouncilTask → Vote → Verdict), its scaffold prompt, its consensus synthesis, and its data-driven
    CLI registry (builtin ∪ user TOML).
  - src/distribute.rs — the EXISTING "council picks the CLI for a unit" pattern this design mirrors
    (distribute_one / route_from_status / graceful degrade / enforce_evaluator_distinct).
  - src/workflow.rs (PhaseDef.skill_ref, PhaseRole, WorkflowDef) + src/domain.rs (StageKind,
    RoutingInfo) — the data the phase-skill choice reads and writes.
  - src/execute_wrapped.rs::skill_prompt — the real `/{skill_ref} {description}` invocation form that
    fixes the skill NAME format (hyphenated dir name, e.g. `wicked-testing-plan`).
  - the installed inventory: `~/.claude/skills/wicked-testing-*` (51 SKILL.md) + the Tier-1 commands
    in `wicked-testing/commands/` (acceptance, authoring, execution, insight, plan, review, setup).
  - DES-EXEC-001 §4.1 (phase→skill mapping table) + §4.2 (skill allowlist + provisioner) + rev0.2 F9–F12
    (the honest "47 skills, 7 invocable; no build/design skill; names must be installed first").
supersedes-note: >
  Fills the last ⬜ in DES-EXEC-001 rev "Remaining honest gaps": **council-assigns-skill (needs a
  grounded skill-ranking design)**. It does NOT change the §4.1 table — it makes that table the
  seed of a DATA registry and lets the council pick a row when the def leaves the choice open.
---

# DES-COUNCIL-SKILL-001 — council-assigns-skill

> When a `PhaseDef` ships with `skill_ref: None`, the workflow author has declared *what* the phase is
> (its `kind`, `role`, gate, acceptance intent) but not *which* installed skill should drive it. This
> design has the **council choose the skill** — by the SAME convene→vote→synthesize→route→degrade
> mechanism `src/distribute.rs` already uses to choose a CLI — ranking a set of candidate skills that
> are **pulled from the real installed inventory**, never invented. The choice is DATA in, DATA out: a
> **skill registry** (drop-in JSON, mirroring `wicked-council`'s CLI registry) + a **ranking-prompt
> scaffold** (mirroring `dispatch.rs::render_scaffold`). Core code never branches on a skill name.

---

## 0. The one-paragraph shape (so the rest is detail)

`plan_from_def` walks the phases. For each phase whose `skill_ref` is `None`, it asks the
**SkillRegistry** for the candidate skills that fit this `{kind, role}` (a pure data lookup — every
candidate id is a real installed skill dir). If there are ≥1 candidates it convenes a council with
`options = candidate skill ids`, `criteria` derived from `{kind, role, gate_type, executes_code}`, and
the **skill descriptions injected into the prompt** so each seat ranks against real capability text.
`synthesize()` returns a `Verdict`; a `route_skill_from_status` (a near-copy of
`route_from_status`) maps the winning recommendation back to a registry id, or **gracefully degrades**
to the registry's declared default for that `{kind, role}` (or to `None` → the authored-prompt path).
The chosen id is written to `unit.skill_ref` and **pinned** so a resumed run does not re-convene and
drift. That is the whole feature. Everything below is the honest detail.

---

## 1. Requirements

### 1.1 Functional
- **FR-1 — Fill the gap, don't override.** If `PhaseDef.skill_ref` is `Some(_)`, use it verbatim
  (author's explicit choice wins). The council runs ONLY for `skill_ref: None`. (Mirrors
  `distribute.rs`: distribution never overrides an explicit assignment; it fills what's open.)
- **FR-2 — Real names only.** Every option the council can pick, and every value ever written to
  `skill_ref`, MUST be an id that resolves to an installed skill directory under `~/.claude/skills/`
  (the hyphenated form, e.g. `wicked-testing-plan`, `wicked-testing-semantic-reviewer`). The registry
  is seeded from the real inventory; a lint + the §4.2 provisioner guarantee presence. **No LLM-invented
  skill name can ever reach `skill_ref`** — the router only accepts a winner that matches a registry id
  (the exact `route_from_status` "winner must be a roster seat" guard, applied to skills).
- **FR-3 — Candidates fit the phase.** The option set for a phase is pre-filtered by `{kind, role}` so
  the council never ranks a Test skill for a Recon phase. Filtering is data (`phase_kinds`, `roles`
  columns on each registry row), not code.
- **FR-4 — Ranking is by capability, not by name-guessing.** The ranking prompt injects each
  candidate's one-line description (mirrored from its SKILL.md) so a seat ranks
  `requirements-quality-analyst` vs `test-strategist` on what they *do*, given the phase's acceptance
  criterion/intent — not on lexical overlap with the phase id.
- **FR-5 — Always yields (fail-safe).** Like `distribute_units_on`, skill selection ALWAYS resolves:
  council winner → registry default for `{kind, role}` → `None` (authored-prompt path). It never fails a
  phase and never blocks planning.
- **FR-6 — Deterministic once chosen (pin).** The selected id is recorded on the run so re-planning a
  resumed/crash-recovered run reuses it rather than re-convening (which could drift to a different
  skill). Ties into DES-EXEC-001 §2.4 idempotency and mirrors `validator_pin`.
- **FR-7 — Evaluator≠creator preserved.** For a `role: Evaluator` phase the candidate set is the
  reviewer skills (`review`, `semantic-reviewer`, `acceptance-test-reviewer`, …); the chosen skill still
  runs under a seat distinct from the creator via the existing `enforce_evaluator_distinct`
  (`distribute.rs:71`). Skill choice and seat-distinctness are orthogonal and compose.

### 1.2 Non-functional / constraints
- **NFR-1 — Law 2 (capability is data).** Adding a skill option, retargeting a phase kind, or changing
  a default is a registry-row edit (drop-in JSON) — never a core edit. The reducer/planner reads the
  registry; it never names a skill.
- **NFR-2 — Reuse the council, don't fork it.** Selection goes through the existing `wicked_council`
  crate (`Worker`, `Dispatcher`, `synthesize`) exactly as `distribute.rs` does. No new voting engine.
- **NFR-3 — Offline-testable.** The whole flow must run under `cargo test` with a stub `Dispatcher`
  (deterministic votes), no subprocess. The `Dispatcher` trait **is** injectable (`real_dispatcher`
  returns `Arc<dyn Dispatcher>`, `distribute.rs:20`), so a stub is achievable. **Correction (review
  finding):** the existing `distribute.rs` tests (`distribute.rs:238-263`) do **not** drive a stub
  Dispatcher through the `Worker` — they unit-test `route_from_status` directly against a hand-built
  `PollStatus`. So slice-1 must *add* the stub-Dispatcher-through-Worker test (the seam supports it; no
  copyable precedent exists), plus the direct `route_skill_from_status` unit tests that do have a
  precedent to mirror.
- **NFR-4 — Cost-bounded.** Convening a council per unassigned phase is N extra CLI fan-outs. The
  design must offer a cheap path (single-seat / registry-default / cached) so a fully-`None` workflow
  isn't unaffordable. (See §5 cost.)

---

## 2. What already exists (grounded — the mechanism we mirror)

Verified by reading the source:

- **The council IS a ranking engine over options.** `CouncilTask { id, topic, options: Vec<String>,
  criteria: Vec<String>, session_id }` (`types.rs:178`). `dispatch.rs::render_scaffold` renders the
  options + criteria into a fixed 4-line prompt (`RECOMMENDATION / TOP_RISK / CHANGE_MY_MIND /
  DISQUALIFIER`); each CLI's stdout is parsed to a `Vote` (`parse_vote`). `synthesize()`
  (`synthesis.rs:77`) counts recommendations, computes `winning_recommendation`, `consensus` (strict
  majority), `agreement_ratio`, and `risk_convergence`. **This is precisely "rank these options and pick
  one, with dissent surfaced."**
- **`distribute.rs` ALREADY uses it to pick a CLI per unit.** `distribute_one` (`:103`) builds a
  `CouncilTask` with `topic = "which CLI should own work unit {id}: {desc}"`, `options = roster_keys`,
  `criteria = ["general"]`, runs the `Worker`, and `route_from_status` (`:167`) maps the verdict winner
  back to a roster seat — with a **graceful degrade to the first seat** when there's no vote / no winner
  / a non-roster winner. `distribution ALWAYS yields an assignment — never fails a unit` (module doc).
- **The registry is DATA (builtin ∪ user TOML).** `registry.rs::builtin()` ships the verified rows;
  `load()` merges `~/.config/wicked-council/clis.toml`; user rows default to `ConfirmOnProbe`. This is
  the exact "capability as a data registry, discover-don't-hardcode-only" pattern we replicate for
  skills.
- **`PhaseDef.skill_ref: Option<String>`** (`workflow.rs:220`) already exists and already flows:
  `plan.rs:78` copies `phase.skill_ref` → `unit.skill_ref`; `execute_wrapped.rs::skill_prompt`
  (`:263`) turns it into `/{skill_ref} {description}`. **The hyphenated dir name is the canonical
  value** — e.g. `validator.rs:103` sets `skill_ref = Some("wicked-testing-acceptance-test-writer")`.
- **`StageKind {Recon, Build, Review, Test}`** (`domain.rs:209`) and **`PhaseRole {Neutral, Creator,
  Evaluator}`** (`workflow.rs:175`) are the axes we filter and rank on. `RoutingInfo` (`domain.rs:267`,
  a `#[serde(tag="method")]` enum with `Council`, `EvaluatorDistinct`, `Degraded` variants) is the
  provenance record we extend with a skill variant.
- **A per-(cli × work-kind) ranking memory exists** (`types.rs` `Ranking`/`RankSignal`/`RankStore`;
  `distribute.rs` wires `EstateRankStore` + `work_kind_for`) — but `distribute.rs:120` documents the
  historical-ranking fast path was **removed** because distribution always runs on an in-memory estate.
  So today the council convenes every time; skill selection inherits that (ranking memory is a §7
  follow-up, not slice-1).

**The real installed inventory (what the names must come from).** `~/.claude/skills/` holds **51**
`wicked-testing-*` skills; the SKILL.md `name:`, **directory, and `skill_ref` value all use the hyphen
form** (`wicked-testing-plan`) — the colon form (`wicked-testing:plan`) is only the Claude plugin
dispatch / slash-command form. The
Tier-1 invocable commands in `wicked-testing/commands/` are: `acceptance, authoring, execution, insight,
plan, review, setup`. Representative rows (id → what it does, per its SKILL.md):

| id (real dir / `skill_ref`) | does (from SKILL.md) | natural phase fit |
|---|---|---|
| `wicked-testing-plan` | Tier-1 test-planning orchestrator (strategy, risk, testability, AC quality) | Recon/Neutral (test planning) |
| `wicked-testing-requirements-quality-analyst` | clarify-phase AC quality (SMART+T), pre-code | Recon/Neutral (clarify) |
| `wicked-testing-test-strategist` | test strategy + coverage plans from code+reqs | Recon/Neutral |
| `wicked-testing-code-analyzer` | static analysis: testability, quality, coverage gaps | Recon or Review |
| `wicked-testing-review` | Tier-1 judgment orchestrator (independent verdict on evidence) | Review/Evaluator |
| `wicked-testing-semantic-reviewer` | spec-to-code alignment; aligned/divergent/missing gap report | Review/Evaluator |
| `wicked-testing-acceptance-test-reviewer` | evaluates evidence artifacts vs plan assertions, independently | Review/Evaluator |
| `wicked-testing-testability-reviewer` | reviews design/code structure for testability | Review/Evaluator |
| `wicked-testing-security-test-engineer` | Tier-2 app-security (SAST/DAST/secrets/authz) | Review/Test (risk-tagged) |
| `wicked-testing-execution` | Tier-1 run tests + capture evidence + write verdict | Test |
| `wicked-testing-acceptance-testing` | evidence-gated 3-agent acceptance (writer→executor→reviewer) | Test |
| `wicked-testing-acceptance-test-writer` | qualitative criteria → evidence-gated test plan | Test / bug-reproduce |
| `wicked-testing-scenario-executor` | runs scenario files end-to-end | Test |

(These are examples the registry seed uses; the full 51 are catalogued from the live inventory at build
time — §6. The point: **every option is a name that already exists on disk**.)

**Honest gap (from DES-EXEC-001 F12):** there is **no installed skill that cleanly drives a `Build`
phase** (`plan` is *test* planning; `authoring`/`execution` are test-authoring/running). So for
`StageKind::Build` the registry's candidate set is intentionally **empty**, and selection degrades to
`None` → the authored-prompt path. council-assigns-skill does not invent a build skill; it honestly
declines to assign one until one is authored.

---

## 3. Design — the skill registry (DATA artifact #1)

A new registry, structurally a twin of `crates/wicked-council/src/registry.rs`, but for skills.

### 3.1 The row type
```
/// One installed-skill capability record. Seeded from the real ~/.claude/skills inventory;
/// extended by a drop-in JSON (discover, don't hardcode-only) exactly like the CLI registry.
struct SkillDescriptor {
    /// The canonical id == the installed skill DIRECTORY name == the `skill_ref` value == the
    /// `/{id}` slash form (execute_wrapped::skill_prompt). e.g. "wicked-testing-plan".
    /// MUST resolve under ~/.claude/skills/<id>/SKILL.md (FR-2, lint-checked).
    id: String,
    /// The Claude plugin dispatch / slash-command form (colon, e.g. "wicked-testing:plan") — display/provenance only.
    display_name: String,
    /// One-line capability text, mirrored from the SKILL.md `description:` — this is what the
    /// ranking prompt injects so seats rank on capability, not name overlap (FR-4).
    capability: String,
    /// Which phase kinds this skill is a candidate for (the pre-filter, FR-3).
    phase_kinds: Vec<StageKind>,            // e.g. [Review]
    /// Which evaluator≠creator roles it fits (FR-7). Empty ⇒ any role.
    roles: Vec<PhaseRole>,                  // e.g. [Evaluator]
    /// Free intent tags for finer matching against the acceptance criterion (e.g. "security",
    /// "a11y", "spec-alignment", "coverage"). Pure data; feeds the criteria/prompt.
    intent_tags: Vec<String>,
    /// The CLI seat that can run this skill (today "claude" — the only grounded /{skill} form,
    /// per execute_wrapped LIMITATION note). Data, so non-claude forms are addable later.
    cli: String,
    /// The runtime contract the cli-runner must satisfy (DES-EXEC-001 F11): required plugin,
    /// evidence format, host-enforcement level. Carried so the runner can validate/provision.
    required_plugin: Option<String>,
    evidence_format: Option<String>,        // e.g. ".wicked-testing/evidence/<run>/verdict.json"
    /// Whether this row is eligible for council selection (mirrors enabled_for_council).
    enabled: bool,
}
```

### 3.2 Registry shape + defaults
```
struct SkillRegistry { skills: Vec<SkillDescriptor> }

impl SkillRegistry {
    /// builtin() seeds from the real inventory (§6 codegen from ~/.claude/skills), then load()
    /// merges a drop-in `skills.json` / `~/.config/wicked-core/skills.json` (deny_unknown_fields;
    /// user rows override builtins by id) — the registry.rs::load() pattern, verbatim shape.
    fn load(user_path: Option<&Path>) -> Result<Self, String>;

    /// The candidate set for a phase: enabled rows whose phase_kinds contains `kind` AND
    /// (roles empty OR contains `role`). PURE data filter — this is FR-3 and the whole reason a
    /// Test skill can never be ranked for a Recon phase.
    fn candidates(&self, kind: StageKind, role: PhaseRole) -> Vec<&SkillDescriptor>;

    /// The declared fallback when the council can't decide (FR-5). Data, per {kind, role}. e.g.
    /// (Recon,*)→wicked-testing-plan, (Review,Evaluator)→wicked-testing-semantic-reviewer,
    /// (Test,*)→wicked-testing-execution, (Build,*)→None (authored-prompt path).
    fn default_for(&self, kind: StageKind, role: PhaseRole) -> Option<&SkillDescriptor>;
}
```
The `default_for` table is a small data map inside the registry file (`[[default]] kind=… role=…
skill=…`), NOT a `match` in core — so retargeting a default is a data edit (NFR-1).

### 3.3 The §4.1 table becomes the seed
DES-EXEC-001 §4.1's phase→skill table is exactly the `builtin()` seed. E.g. its "adversarial-review
→ `review` · `semantic-reviewer` · `acceptance-test-reviewer`" row becomes three `SkillDescriptor`s
with `phase_kinds:[Review], roles:[Evaluator]` — which is precisely the candidate set the council
ranks for a `role:Evaluator` phase. Nothing invented; the table is promoted from prose to data.

---

## 4. Design — the ranking mechanism (DATA artifact #2 + the flow)

### 4.1 The ranking-prompt scaffold (mirrors `render_scaffold`)
`dispatch.rs::render_scaffold` is already a data-shaped template (topic + numbered options + criteria +
the fixed 4 answer keys). Skill selection uses a **skill-ranking variant** that additionally injects
each candidate's `capability` line so the seat ranks on what the skill does:

```
You are one independent member of a council choosing the SKILL to drive a workflow phase.
Phase: {phase_id}  kind={kind}  role={role}  gate={gate_type}  executes_code={bool}
Acceptance intent for this phase:
{intent}                      # the phase's acceptance criterion / required_deliverables / description
Candidate skills (choose ONE by its id):
  1. wicked-testing-semantic-reviewer — Verify spec-to-code alignment; aligned/divergent/missing gap report
  2. wicked-testing-acceptance-test-reviewer — Evaluates evidence artifacts vs plan assertions, independently
  3. wicked-testing-review — Tier-1 judgment orchestrator; independent verdict on captured evidence
Evaluation criteria: {criteria}          # derived, §4.2
Answer with EXACTLY these four lines:
RECOMMENDATION: <the ONE candidate id, and why it fits the intent>
TOP_RISK: <the biggest risk of that choice>
CHANGE_MY_MIND: <evidence that would flip it>
DISQUALIFIER: <any candidate fundamentally unfit, or 'None'>
```
> **⚠️ Reality check (adversarial-review finding, CONFIRMED against the code).** The scaffold above is
> the *target*, but it is **NOT** reachable through pure reuse the way an earlier draft claimed.
> `dispatch.rs::render_scaffold` (`dispatch.rs:22-42`) is a **hardcoded Rust `format!` literal**,
> rendered **unconditionally** by `RealDispatcher::dispatch` (`dispatch.rs:72`); the `Dispatcher` trait
> only receives a `CouncilTask{topic, options, criteria, session_id}` (`types.rs:178`) — there is **no
> field for capability text, no phase header, and no injection seam**. The fixed intro line ("You are
> one independent member of a council") cannot be changed without editing wicked-council. So the earlier
> claim "the prompt template is authored text held next to the registry (data), not a Rust string
> literal" is **false**. Two honest ways to get FR-4 (rank on *what the skill does*), pick one:
>
> - **Option A — reuse-preserving (default, NFR-2 intact, no wicked-council edit).** Pack the capability
>   text **into the free-text `options` strings** and the phase header/intent into `topic` + `criteria`:
>   each option becomes `"wicked-testing-semantic-reviewer — Verify spec-to-code alignment; …"`. The seat
>   still sees the capabilities; the fixed council intro stays. Cost: the returned `RECOMMENDATION` echoes
>   prose, so selection must **match a candidate id by substring/id-extraction, not exact `norm()`
>   equality** (see §4.3 — this is also the fix for the prose-variance convergence risk). This is weaker
>   and less legible than the numbered scaffold drawn above, but it needs zero engine-crate change.
> - **Option B — minimal additive council seam ("new primitive = code", allowed by Law 2).** Add one
>   optional field to `CouncilTask` (e.g. `option_notes: Vec<String>` or a `preamble: Option<String>`)
>   and have `render_scaffold` emit it when present — a small, backward-compatible change to
>   wicked-council (every existing caller passes `None`/empty and renders identically). This buys the
>   clean scaffold above and keeps parsing on the id. It is a *code* change to a shared crate, so it needs
>   its own review; it does **not** "fork" the council (the parser + 4-key contract are untouched).
>
> Recommendation: **ship Option A for slice-1** (no cross-crate change, provably real ids), and land
> Option B only if the packed-`options` prompt measurably degrades ranking quality.

Because `RECOMMENDATION` is coached to name *the candidate id*, the existing `parse_vote` +
`synthesize` machinery is reused, but selection matches the winner to a candidate by **id-substring**
(§4.3), not raw `norm()` equality — `norm()` (`synthesis.rs:26`) only lowercases/trims/collapses
whitespace, so a recommendation carrying trailing prose ("…— best fit") would NOT converge on exact
match. The scaffold (whichever option) reuses the same fixed 4 answer keys, so we do NOT fork the parser.

### 4.2 Criteria derivation (data-driven, from the phase)
`criteria` (the `Vec<String>` the scaffold lists) is built from the phase, via a small **data map**
(`[[criteria_rule]]` rows: `when kind/role/flag → add "…"`), not hardcoded. Examples:
- `role == Evaluator` → `["independence-from-creator", "reads-cold-evidence", "judges-vs-intent"]`
- `kind == Recon` → `["decomposition-quality", "surfaces-unknowns", "acceptance-criteria-fitness"]`
- `kind == Test` → `["evidence-gated", "reproducible", "framework-fit"]`
- `executes_code == true` → add `"least-privilege-tool-scope"`
- any `intent_tags` on the winning-candidate class (e.g. a phase tagged `security`) → add that tag so a
  seat weights `security-test-engineer` appropriately.
This is the direct analog of `distribute.rs`'s `DISTRIBUTE_CRITERIA` (there a flat `["general"]`);
here criteria are richer because the skill axis benefits from the phase's shape and intent (FR-4).

### 4.3 The selection flow (mirrors `distribute_one` → `route_from_status`)
In `plan_from_def`, after phases are built into units and BEFORE distribution of CLIs:

```
for (phase, unit) in phases.zip(units) {
    if phase.skill_ref.is_some() { continue; }                       // FR-1: author's choice wins
    let cands = registry.candidates(phase.kind, phase.role);         // FR-3, real names only
    if cands.is_empty() {                                            // e.g. Build
        unit.skill_ref = registry.default_for(kind, role).map(id);  // FR-5 → often None (authored)
        continue;
    }
    if cands.len() == 1 {                                            // NFR-4: no council needed
        unit.skill_ref = Some(cands[0].id); record SkillRouting::Sole; continue;
    }
    let task = CouncilTask {
        id: ids::new_task_id(),
        topic: format!("which skill should drive phase {} ({:?}/{:?})", phase.id, kind, role),
        options: cands.iter().map(|c| c.id.clone()).collect(),      // FR-2: options ARE real ids
        criteria: derive_criteria(phase, &registry),                // §4.2
        session_id,
    };
    // Reuse the SAME Worker/Dispatcher path distribute_one uses (in-memory council estate,
    // NoopEventSink, stub-injectable Dispatcher for cargo test — NFR-2/NFR-3).
    let status = worker.queue_blocking + worker.poll(...);
    let chosen = route_skill_from_status(status, &cands, &registry, phase.kind, phase.role);
    unit.skill_ref = chosen.id;                                     // may be None → authored path
    unit.routing_skill = chosen.routing;                            // provenance (§4.4)
}
```

`route_skill_from_status` is a line-for-line analog of `route_from_status` (`distribute.rs:167`):
```
- no status / not Voted / no verdict / no winner        → degrade to default_for(kind,role)  (FR-5)
- winner matches a candidate id (exact OR norm-contains) → that skill, SkillRouting::Council{...}
- winner is NOT a candidate id (LLM hallucinated a name) → degrade to default_for(kind,role)  (FR-2!)
```
The **"winner must be a candidate id" guard is the FR-2 enforcement point**: a seat that answers with
an invented skill name is treated exactly like `distribute.rs` treats a non-roster CLI winner — it is
rejected and we degrade. No invented name can ever be written to `skill_ref`.

### 4.4 Provenance (extend `RoutingInfo`, don't invent a parallel system)
Add skill-routing provenance so the UI can answer "why THIS skill" — either a new `RoutingInfo`
variant set or a sibling `SkillRouting` enum with the same three shapes already proven in
`domain.rs::RoutingInfo` + `distribute.rs`:
```
enum SkillRouting {
    Author,                                   // FR-1: def named it
    Sole { skill },                           // single candidate, no council
    Council { skill, agreement_pct, returned, dissent },   // mirrors RoutingInfo::Council
    Default { skill: Option<String>, reason },// degrade (incl. None→authored path)
}
```
This rides the unit like `RoutingInfo` does today, is `Eq`-friendly (percent as `u8`), and makes the
skill choice auditable in the same place the CLI choice already is.

### 4.5 Composition with CLI distribution + evaluator≠creator
Order in `plan_from_def`: **skill first, then CLI.** Skill selection sets `unit.skill_ref`; the
existing `distribute_units_on` then picks the CLI seat and `enforce_evaluator_distinct`
(`distribute.rs:71`) still reassigns a Review/Test unit off a builder seat. The two councils are
independent axes (which skill vs which seat) and both degrade safely. A registry row's `cli` field
constrains which seats can host a chosen skill (today all rows are `claude`), so a future non-claude
skill won't be dispatched to a seat that can't run it.

### 4.6 Pinning (FR-6, idempotency)
The chosen `skill_ref` is written into the unit and persisted with the run (the units are already
durable in the store). On resume/replan the planner sees `skill_ref: Some(_)` on the persisted unit and
**does not re-convene** — the FR-1 short-circuit doubles as the idempotency guard. If a def-level pin is
preferred (to survive a full re-plan from the def), add an optional `skill_pin: Option<String>` to the
run's def-instance, filled on first selection — the exact pattern `validator_pin` (`workflow.rs:235`)
uses for validators. Recommended: **unit-level persistence for slice-1** (simplest, already durable);
def-instance pin is a follow-up if full re-plans become a path.

---

## 5. Cost & the cheap path (NFR-4)
- A fully-`None` workflow with K multi-candidate phases = K councils × |roster| dispatches. Bound it:
  1. **Single-candidate & Build phases never convene** (§4.3) — e.g. Build→None, clarify→often one
     dominant candidate.
  2. **Optional single-seat mode** for skill selection: convene only the cheapest-capable seat (a
     roster of one) — the council degrades to that seat's pick, still routed through the same guard.
     A `skill_council: {full | single-seat | default-only}` knob (data) picks the tier — this is the
     "engagement dial for skill choice," analogous to DES-EXEC-001 §rev0.5 #6's routing-judgment dial.
  3. **`default-only`** skips the council entirely and takes `default_for` — the zero-cost floor, useful
     in CI / print-mode.
- Ranking memory (`RankStore` over `work_kind = kind+role`) can later bias/skip the council for a
  phase whose best skill is already known with high `n` — but per `distribute.rs:120` that needs a
  durable (non-in-memory) council estate first, so it is §7, not slice-1.

---

## 6. Build plan (slice order, each cargo-testable)

1. **Registry codegen from the real inventory.** A build/test helper that reads `~/.claude/skills/*/
   SKILL.md` (name + description) and emits the `builtin()` seed — so ids are provably real. **Note
   (review finding):** the SKILL.md `description:` is a **multi-paragraph `|` block** (a paragraph plus
   "Use when:" / examples / "NOT THIS WHEN:"), **not** a single copyable line — so `capability` is a
   **summarized one-liner** (first sentence, or the "Use when:" clause, truncated to ~120 chars), a
   deterministic summarization step this helper must implement; it is not a verbatim copy (FR-2/FR-4).
   Ship the generated seed + a `skills.json`
   drop-in loader (`registry.rs::load` twin, `deny_unknown_fields`). **Lint:** every builtin id must
   resolve to an installed dir (fail the build otherwise); ties to §4.2 provisioner for fresh envs.
2. **`candidates()` + `default_for()` + the §4.1 seed rows** (Recon/Review/Test populated; Build empty
   by design). Unit-test: `candidates(Review, Evaluator)` returns exactly the reviewer set; `(Build,*)`
   is empty; every returned id resolves on disk.
3. **`derive_criteria()` from the data map** + the ranking scaffold template file. Unit-test: an
   Evaluator phase yields the independence criteria; the rendered prompt lists candidate ids +
   capability lines.
4. **`route_skill_from_status` + `SkillRouting`** — copy `route_from_status` + its two tests
   (`winner_matching_a_candidate…`, `hallucinated_name_degrades…`) adapted to skills. This is the
   FR-2 guard, tested.
5. **Wire into `plan_from_def`** (skill-first, before `distribute_units_on`) behind the
   `skill_council` knob; default `default-only` in CI/print-mode so the CI stays offline (NFR-3), `full`
   for real runs. Reuse the `distribute.rs` Worker/stub-Dispatcher seam for the deterministic test.
6. **Provenance surfacing** — `SkillRouting` onto the unit; assert it in the plan-level test; expose in
   the existing routing UI path alongside `RoutingInfo`.
7. **One real-CLI e2e** (`#[ignore]` in CI, like DES-EXEC-001 §6): a `role:Evaluator` phase with
   `skill_ref:None` convenes a real council and lands on a reviewer skill; assert `skill_ref` resolves
   on disk and the `/{skill}` invocation expands.

**Gate:** `cargo build/test/clippy -D warnings` green per change (wicked-core CI contract).

---

## 7. Open questions
1. **Registry source of truth** — generate the `builtin()` seed at build time from the live
   `~/.claude/skills` (freshest, but couples the build to the dev machine's install) vs check in a
   generated `skills.json` refreshed by a `wicked-testing:update`-style step (reproducible builds, but
   can drift from disk). Leaning: **check-in + a lint that fails when a builtin id is missing on disk**,
   refreshed by the §4.2 provisioner. Confirm.
2. **Where the councils run relative to distribution** — skill-council and CLI-council are two fan-outs
   per phase. Fold them into ONE council with a richer topic ("pick skill AND seat")? Rejected for
   slice-1 (muddies the two guards and the evaluator≠creator reassignment); confirm keeping them
   separate is acceptable given the extra dispatch cost, or default to `single-seat` skill mode.
3. **Non-claude skill forms (Law-2 debt inherited from execute_wrapped)** — the `/{skill}` prefix is
   Claude-only. The registry `cli` field is the seam, but until per-CLI skill forms are authored, every
   skill row is `cli:"claude"`, which can conflict with a CLI-council that wants a non-claude seat for
   evaluator-distinctness on a 2-seat roster. Resolve the precedence: does a skill's required `cli`
   override the evaluator-distinct reassignment, or vice-versa?
4. **Intent text for ranking** — what exactly is injected as `{intent}`: the phase's
   `required_deliverables`, a dedicated `acceptance` field on `PhaseDef` (new data), or the unit
   description? The richer the intent, the better FR-4 ranking — but adding an `acceptance` field is new
   `PhaseDef` data. Decide whether to add it now or reuse `required_deliverables` + description.
5. **Build/design phases (F12 honest gap)** — council-assigns-skill correctly declines to assign a
   Build skill (empty candidate set → authored prompt). When the missing build/design crew skills are
   authored (DES-EXEC-001 rev0.2 F12), they become new registry rows with `phase_kinds:[Build]` and the
   council starts selecting them with zero core change — confirm that is the intended path (it is the
   Law-2 payoff, but worth stating as the acceptance test for "the design composes forward").
6. **Ranking memory activation** — the `RankStore` bias (§5) needs a durable council estate that
   `distribute.rs:120` says doesn't exist in the current single-writer setup. Is giving skill-selection
   its own durable ranking projection worth it, or does council-every-time stay acceptable given the
   cheap-path knobs?
