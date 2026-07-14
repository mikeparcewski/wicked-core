# DES-OUTGOV-002 — Remaining output-governance work (plan)

Successor plan to DES-OUTGOV-001 (PR-A→E merged). Grounded in a 6-area parallel recon (workflow
`wf_aec368a2-b96`) + verification. This plan is itself to be adversarially reviewed before execution.

## Recon corrections to the prior mental model (facts, cited)
- **The workflow was already broken before brain#97.** `domain-extraction.json` names FIVE garden
  skills that DO NOT EXIST — garden reorganized the whole surface into the `modernize` archetype
  (router `wicked-garden-modernize` + workers `modernize-{extractor,translator,antagonist}`). The 3
  now-dead `wicked-brain-*` in `allowed_skills` were an ADDITIONAL break, not the only one. These
  fields are load-bearing (execute_wrapped.rs builds `/{skill_ref} {desc}` + rides allowed_skills onto
  `--allowedTools`).
- **`CoverageReport` does NOT match the kept `coverage.schema.json`.** Schema: `unaccounted` is an
  **integer** count + a separate `unaccounted_nodes` **array** of `{symbol_id,name,kind,file,app}` +
  a `per_app` breakdown. Rust `CoverageReport.unaccounted` is `Vec<String>` (domain_model.rs:155) —
  a wire mismatch (same class as the PR-D schema bugs). A canonical report fails serde into it.
- **No coverage-report EMITTER exists.** wicked-core has only a CONSUMER (`domain-graph`) + the grep
  validator; nothing writes `coverage-report.json`. The garden coverage skill currently mocks brain.
- **`validator_pin c4cc487a030d57b7` needs NO re-approval** — content-addressed over
  (criterion, script, approved) only; the M10 retarget touches none of them.
- **`executes_code` is declared-but-inert** (no consumer in plan/execute) — set only as intent.
- **Mainframe/Louvain (`structural` mode) has ZERO live consumer** — anti-legacy is archived; the
  live path is `functional` (modern package-dir). (Recon agent for this area returned garbage; call
  verified by grep — no mainframe/cobol/cics reference in any live product.)
- **Schema is now vendored in 3 places** — brain canonical, `wicked-core/tests/` (PR-D), and garden
  `skills/modernize/vendor/` — a drift risk with no guard.

## Plan — sequenced by dependency + value

### PR-F (do-now, S–M) — coverage predicate + emitter + report-shape fix (the keystone)
Closes cross-product finding #15, fixes the coverage-report wire bug, and provides the missing
emitter M10 needs. Order matters: this unblocks PR-G's runnability and PR-H's fixtures.
1. **Fix `CoverageReport`** to the kept `coverage.schema.json` shape (integer `unaccounted` +
   `unaccounted_nodes[]` + `per_app`), with a **schema-validation test** (vendor `coverage.schema.json`,
   validate emitted output — the class-closer, mirroring `tests/domain_model_schema.rs`).
2. **`recompute_front_half_coverage(store: &dyn GraphRead) -> CoverageReport`** — port coverage.py's
   resolved-or-flagged predicate: `is_behavior_bearing(NodeKind, has_behavior_out_edge)` + module
   dead-shell exclusion; resolved via `node_semantics`/`business_rule` anns, risk via advisory/`risk`
   anns. Extract the builder's keep-set predicate into ONE shared helper both use.
3. **`wicked-core coverage [--out]`** subcommand emitting `coverage-report.json`.
4. **`domain-graph --coverage` becomes OPTIONAL:** recompute from the store by default; a supplied
   file is a cross-check that must AGREE (fail-closed on disagreement). Defense-in-depth: the builder
   recomputes internally and fails closed if any behavior-bearing node is unaccounted even when a
   passed report claims 1.0.

### PR-G (do-now, S) — M10 workflow retarget (hygiene; stops referencing deleted skills)
1. `domain-extraction.json`: retarget the 5 `skill_ref`s to the garden `modernize` surface
   (extract→`modernize-extractor`, domain-graph→`modernize-translator`, survey/analyze→`modernize`
   router; **DECISION: the `coverage` phase → `modernize-antagonist` or the router**) + set the 3 dead
   `allowed_skills` to `[]`.
2. The 2 lockstep tests in `domain_extraction.rs` (`every_phase_carries_a_garden_skill_ref…`,
   `plan_from_def_carries_skill_refs…`).
3. Garden: retarget `modernize-translator`'s doc to shell `wicked-core domain-graph` + `wicked-core
   coverage` (PR-F) instead of the retired `wicked-brain domain build`.
   **Honest caveat:** this is HYGIENE — end-to-end runnability still needs garden's own PHASE-2
   un-mock of `_mocks.BrainClient`. Ship to stop referencing deleted skills; do not claim it runs.

### PR-H (do-now, M) — 3-agent acceptance pipeline (PR-E's unfinished first half)
Almost entirely wicked-core-side AUTHORING; NO wicked-testing code change (the pipeline, scenario
format, reviewer isolation, evidence contract already exist).
1. Install wicked-testing in the wicked-core context + `/wicked-testing:setup` (`.wicked-testing/`).
2. Author ~4 scenarios black-boxing the shipped binary: domain-graph happy-path (coverage-1.0 fixture
   → valid `requirements_graph.json`), domain-graph fail-closed (coverage<1.0 → deny), output-gate-hook
   fail-closed (unset decisions path → exit 2), gate deny-dominance.
3. Deterministic fixtures (annotated estate `.db` at coverage 1.0, via PR-F's `wicked-core coverage`).
4. Land evidence at `.wicked-testing/evidence/<run>/verdict.json`.

### PR-I (do-later, M) — runtime Postgres parity (PR-E §5's real half)
1. CI `postgres-parity` job: `services: postgres:16` + estate-sibling checkout + `--features postgres`
   + `TEST_POSTGRES_URL`. `PostgresStore::open` self-inits schema (no migration step).
2. Backend-parametrized store-test helper (`&mut dyn GraphStore` per backend: SQLite `:memory:` always,
   Postgres when `TEST_POSTGRES_URL` set); parametrize the governance store tests + ≥1
   `open_store_any("postgres://")` round-trip (today: 0 coverage of the AnyStore Postgres arm).
3. Test isolation: per-test schema, or `--test-threads=1` with schema reset.

### Deferred / dropped
- **DEFER — vocabulary miner:** naming-refinement only; the schema places NO constraint on domain/
  title names, so it can never be a validity gate; blocks nothing. Do only if term-aware naming
  becomes a product ask.
- **DROP — mainframe/Louvain (`structural`) path:** no live consumer. Re-open only with a real one.
- **FOLD-IN — schema drift-guard:** a small test/CI check that the 3 vendored schema copies match the
  brain canonical (add to PR-F or a tiny follow-up).

## Open decisions for the user
1. PR-G's `coverage` phase → which garden worker (`modernize-antagonist` vs the router)?
2. Is PR-H (acceptance authoring) worth doing now, or defer until the workflow actually runs
   end-to-end (which needs garden's un-mock, out of our repos)?
3. Confirm DROP on mainframe + DEFER on vocab.

## REVISION 1 — the #2 reframe (the keystone the plan above MISSED) + adversarial-review corrections

### A. Reframe: the outcome is #2 (an orchestrated GOVERNED RUN), not just correct components
Verified: **governance is NOT wired into a real wrapped-CLI run.** `execute_wrapped.rs` never generates the
`.claude/settings.json` hooks that make the wrapped Claude CLI INVOKE `gate-hook` (PreToolUse) or
`output-gate-hook` (Stop/SubagentStop) — the code says "PreToolUse governance … is **P4b — until then**"
(execute_wrapped.rs:7). The actor DOES drain (`apply_hook_decisions`, actor.rs:287) but nothing writes decisions
during a run. So the per-output guardrail (PR-C) is INERT in real runs. **The #1 keystone below is unbuilt and
blocks #2; PR-F/G/H/I are necessary but not sufficient.** #2's milestones, ordered:
1. **Governance fires during a run** — launcher injects the hooks + `WICKED_DECISIONS_PATH` + P4b read-only/
   busy_timeout store-open safety. (❌ not built — the real unlock.)
2. Coverage emitter + store-bound `domain-graph` (= PR-F). 3. Conformance rules populated in a run.
4. Workflow references real skills (= PR-G). 5. **Garden un-mocks** its `modernize` skills (❌ **garden repo**).
6. End-to-end workflow run (integration). 7. Acceptance evidence (= PR-H). 8. Postgres runtime parity (= PR-I).
#2 is NOT reachable by wicked-core work alone — milestone 5 is garden's.

### B. Adversarial-review corrections (plan review wf_ebc41d9e-243, 7 confirmed)
- **CRITICAL — PR-F step 2 must use TWO predicates, not one shared keep-set.** The builder's keep-set counts
  `description-only` nodes as kept; reusing it as the coverage NUMERATOR makes the fail-closed gate VACUOUS (a
  merely-described, rule-unextracted graph reports coverage==1.0). Split: **denominator** =
  `is_behavior_bearing(kind, has_behavior_out_edge)` (counts bare nodes); **numerator** = `accounted(node)` =
  resolved (validated `requirement` OR business_rule ann) OR risk-flagged — **MUST EXCLUDE description** (matches
  `coverage.py::classify_node`). Description stays a builder-only additive, never in the coverage predicate.
- **MAJOR — PR-F is a BREAKING change to a fail-closed gate's contract; enumerate the blast radius.** `CoverageReport`
  gains ~9 schema-required fields (`unaccounted: u64`, `unaccounted_nodes: Vec<UnaccountedNode>{symbol_id,name?,
  kind?,file,app}`, `per_app`, total/behavior_bearing/resolved/risk_flagged/resolved_rate/mean_confidence/
  resolve_threshold). `assert_front_half_coverage` (domain_model.rs:162-176) currently reads `unaccounted` AS A LIST
  (`.iter().take(20)`, `.len()`) to surface SymbolIds — it MUST be rewired to source symbols from
  `unaccounted_nodes[].symbol_id`. Update the 5 struct-literal sites (domain_model.rs:412/424/502/580,
  tests/domain_model_schema.rs:74), re-author `coverage_gate_fails_closed_below_one`, and add a
  DESERIALIZE/consume test for the binary path (bin/wicked-core.rs:551) — the emitted-output schema test does NOT
  cover the consumer.
- **PR-G — it's FOUR dead `allowed_skills` slots, not 3** (analyze:vocabulary, extract:domain, coverage:coverage,
  domain-graph:domain — `wicked-brain-domain` on TWO phases). The 2 lockstep tests only pin extract+coverage → add
  `assert!(phase.allowed_skills.is_empty())` in the all-phase loop of `every_phase_carries_a_garden_skill_ref…` so a
  leftover on analyze/domain-graph fails CI.
- **DROP-mainframe** = don't PORT the structural/Louvain branch; do NOT remove `structural` from the schema enum
  (breaks the vendored-schema parity + contract). **Drift-guard** = check EVERY vendored schema byte-copy vs its
  brain canonical (domain-model in `tests/` + garden `vendor/`; coverage.schema once PR-F vendors it), not a
  hardcoded count.
