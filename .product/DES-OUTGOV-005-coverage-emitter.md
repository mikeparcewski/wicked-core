# DES-OUTGOV-005 — Coverage emitter + store-bound domain-graph (milestone #2 / core#25)

Milestone #2 of the outcome-#2 roadmap. Makes the front-half **coverage gate real**: a runnable
`wicked-core coverage` emitter + a `domain-graph` that recomputes coverage **from the store** (not a
trusted external file), with a `CoverageReport` that matches the kept `coverage.schema.json`. Grounded in
recon `wf_98abea3a-e9b` (verified against HEAD `fa985d1`). To be adversarially reviewed before build.

## The core hazard this closes (why the two-predicate split is load-bearing)
The builder's **keep-set** (`build_domain_model`, domain_model.rs:274) admits `description-only` nodes
(`if !resolved && risk_notes.is_empty() && description_text.is_none() { continue }`). Reusing that keep-set
as the coverage NUMERATOR makes the fail-closed gate **VACUOUS**: a merely-*described*,
rule-unextracted graph would report `coverage == 1.0` and pass. The gate must use **TWO distinct
predicates**, and the numerator MUST EXCLUDE description.

## Two-predicate spec (ports `coverage.py::classify_node`)
> **This section is the SINGLE, self-contained normative source for classification.** Decision #1 below
> is rationale only. Both membership sets are EXHAUSTIVE and the native match is compiler-enforced (see
> below), because a dropped behavior kind is fail-OPEN — it silently escapes the denominator and passes
> the gate on unextracted logic (the very hazard §"core hazard" names).
- **DENOMINATOR — `is_behavior_bearing(node, has_behavior_out)`** (counts BARE nodes). The `NodeKind` enum
  is CLOSED, so the recompute matches EVERY native variant with NO wildcard arm — adding a variant to the
  enum then breaks the build until it is explicitly classified (the fail-closed guarantee; a silent
  default is impossible for native kinds).
  - **behavior-bearing** = `{Module, Namespace, Function, Method, Constructor, Class, Struct, Interface,
    Trait, Rule}` AND not the Module dead-shell exception (a `Module` with ZERO outgoing behavior edge is
    excluded). `Rule` IS behavior-bearing — it is the atomic unit of business logic a rules-engine
    extractor emits (Drools/DMN/OPA/decision-table); a bare `Rule` is UNACCOUNTED until the
    domain-extraction phase writes a `business_rule` annotation onto it. `Namespace`≈Module,
    `Trait`≈Interface, `Constructor`≈Method carry behavior.
  - **structural (excluded)** = `{File, Import, Field, Constant, Variable, Parameter, TypeAlias, Enum,
    Macro, RuleSet, Condition, Action, Fact, Synthetic}`. `RuleSet` (container), `Condition`/`Action`
    (sub-clauses of a Rule — analog of Parameter/Field), `Fact` (data model) are structural PROVIDED the
    annotation target is the `Rule` node, so a fully-extracted rules codebase reaches coverage 1.0.
  - **`Other(tag)`** (open domain) is split on the normalized tag against CONFIG-overridable sets
    (defaults ported from `coverage.py`):
    - estate-behavior → behavior-bearing: `{"cics_program", "step", "db2_table"}`
      (`coverage.py` `DEFAULT_ESTATE_BEHAVIOR_KINDS`) + `{"uses"/"accesses"/"invokes"` are EDGE kinds, not
      node kinds — not here).
    - estate-structural / any other `Other(_)` → structural:
      `{"dataset","cics_map","ims_database","ims_segment","parent", …}` and every unrecognized tag. NOTE
      this Other-default is fail-OPEN (a NEW behavior extractor's tag silently escapes) — the ONLY residual
      fail-open path, mitigated by the config seam + a `debug_assert`/log on an unrecognized `Other` tag so
      it is surfaced, not silent.
  - `has_behavior_out` = precomputed ONCE from `all_edges()`: an edge whose source == node.symbol and kind
    ∈ `{Calls, References, Evaluates, Produces, Governs, Other("uses"), Other("accesses"), Other("invokes")}`.
    (Verified against the real `EdgeKind` enum, node.rs — the native rule-engine edges `Evaluates`/
    `Produces`/`Governs` ARE included so a live rules Module is never falsely excluded by the dead-shell
    exception; the exception SHRINKS the denominator, so its input set is deliberately generous =
    fail-closed-safe. `Contains`/`Defines`/`Imports`/`Extends`/`Implements`/`HasType`/`Returns`/
    `Instantiates`/`Overrides`/`InvokedBy` are structural/inverse and excluded.)
  - **Config seam** (`coverage.schema.json` mandates "config-driven … never hardcoded"): the
    behavior/structural kind sets are overridable (a `CoverageConfig` with sensible defaults = the sets
    above), so a rules-engine or mainframe target can tune classification with NO code change — but the
    DEFAULT for `Rule` + the three estate-behavior tags is behavior-bearing.
- **NUMERATOR — `accounted(sem, anns)`** (coverage membership; MUST EXCLUDE description):
  `sem.requirement.is_some()` (validated OR not) **OR** any `business_rule` annotation **OR** any
  `advisory`/`risk` annotation. `sem.description` is NEVER in `accounted` — description stays a
  builder-keep-set-only additive. A behavior-bearing node that is BARE or DESCRIPTION-ONLY is
  UNACCOUNTED and drives `coverage < 1.0`.
- `coverage = accounted / behavior_bearing`; **vacuously 1.0 when behavior_bearing == 0** (empty graph —
  acceptable per contract; assert it in a fixture).
- **resolved vs risk split** (re-buckets WITHIN accounted; drives ONLY the
  resolved/risk_flagged/resolved_rate/mean_confidence fields — NEVER the coverage pass/fail):
  `resolved` = (requirement present AND `requirement_validated == true`) OR (business_rule ann with
  `confidence >= resolve_threshold`, default 0.75). `risk_flagged` = accounted-but-not-resolved. Note:
  `NodeSemantics` has NO confidence field → requirement nodes split on `requirement_validated` (bool);
  annotation nodes split on `annotation.confidence` vs threshold. `mean_confidence` averages only settled
  nodes carrying a numeric confidence (annotation-backed).

## Wire shape — `CoverageReport` (schema-exact; the wire mismatch fix)
Current `{coverage: f64, unaccounted: Vec<String>}` (domain_model.rs:150-156) does NOT match the kept
`coverage.schema.json` (a canonical brain report FAILS serde into it, and the shipped grep validator
`COVERAGE_SCRIPT` greps `"unaccounted":[[:space:]]*[1-9]` — a `Vec<String>` emit serializes
`"unaccounted":["sym…` and the grep would NOT match a non-empty hole → **silent pass**). New shape (all
11 top-level fields required, `additionalProperties:false`):
- `total: u64`, `behavior_bearing: u64`, `resolved: u64`, `risk_flagged: u64`, **`unaccounted: u64`**
  (integer, NOT `Vec<String>`), `coverage: f64` (0..1), `resolved_rate: f64`, `mean_confidence: f64`,
  `resolve_threshold: f64`, `per_app: Vec<PerApp>` (an ARRAY, not an object), `unaccounted_nodes:
  Vec<UnaccountedNode>`.
- `PerApp { app: String, behavior_bearing: u64, resolved: u64, risk_flagged: u64, unaccounted: u64,
  coverage: f64 }` — `additionalProperties:false`, **NO `db`, NO `total`** (coverage.py emits those; the
  schema forbids them — anchor the WIRE on the schema, take only the ALGORITHM from coverage.py).
- `UnaccountedNode { symbol_id: String, name: Option<String>, kind: Option<String>, file: Option<String>,
  app: Option<String> }` with `skip_serializing_if = "Option::is_none"` (only `symbol_id` is schema-
  required; name/kind are `[string,null]`; file/app are plain strings, omitted-not-null when absent).
- `assert_front_half_coverage` (domain_model.rs:162-177) rewired: count from the integer `unaccounted`;
  surface SymbolIds from `unaccounted_nodes[].symbol_id`.

## The recompute + emitter + consumer
1. **`recompute_front_half_coverage(&dyn GraphRead) -> CoverageReport`** in wicked-governance —
   `all_nodes()`/`all_edges()`/`node_semantics()`/`annotations()` (no new store method needed). Populates
   all 11 fields + `per_app` + `unaccounted_nodes` (sorted by symbol_id).
2. **`wicked-core coverage [--out]`** subcommand (bin dispatch arm) — open store → recompute → write
   `coverage-report.json`.
3. **`domain-graph --coverage` becomes OPTIONAL** (bin:558-601): recompute from the store by default; a
   supplied file is a CROSS-CHECK that must AGREE (fail-closed on disagreement). Defense-in-depth: the
   builder recomputes internally + fails closed if any behavior-bearing node is unaccounted even when a
   passed report claims 1.0.

## Open-risk DECISIONS (recon surfaced; resolved here — review these hardest)
1. **NodeKind classification** — the normative sets live in §Two-predicate (above); this is the RATIONALE
   (adversarial-review corrected — the original draft wrongly bucketed `Rule` + the estate-behavior tags as
   structural, which under-counts the denominator → vacuous gate for a rules-engine / mainframe graph):
   - `Rule` is behavior-bearing: the `Rule` NODE is a STRUCTURAL survey artifact an extractor emits; the
     GOVERNANCE is the `business_rule`/`requirement` annotation written ONTO it. **Invariant: never drop a
     survey-emitted behavior node from the denominator to make it "self-accounting" — if self-accounting is
     genuinely intended, add the kind to BOTH denominator and numerator, never to neither.**
   - The three estate-behavior `Other` tags (`cics_program`/`step`/`db2_table`) match coverage.py's
     defaults and MUST count (a mainframe graph's behavior nodes arrive as `Other(...)` since `NodeKind` has
     no dedicated variant). We port the CLASSIFICATION faithfully (cheap, correctness-only) — this is NOT
     porting the dropped mainframe/Louvain STRUCTURAL-clustering path.
   - `Namespace`/`Trait`/`Constructor` are behavior-bearing; `RuleSet`/`Condition`/`Action`/`Fact`/
     `Synthetic` structural. This is the single most gate-soundness-sensitive decision — under-counting the
     denominator silently passes holes, so the native match is exhaustive/compiler-enforced.
2. **Behavior-out EdgeKind set** (Module dead-shell test): {Calls, References, Other("uses"/"accesses"/
   "invokes")}. Imports/Instantiates/Implements/InvokedBy EXCLUDED (structural/inverse). Only affects
   Module nodes (rare in the functional path) — pinned for determinism.
3. **`per_app` grouping**: group by the builder's private `package_dir` fn (reuse it); a store with no
   package structure collapses to a single synthetic app `"graph"` (matches coverage.py's default).
   Emit the ARRAY form (the grep-validator's mock uses an object — the emitter must emit the schema array).
4. **`--out` path collision**: the grep validator reads bare `coverage-report.json` from the phase
   worktree cwd (domain_extraction.rs:43) while domain-graph defaults `--coverage` to
   `.wicked-estate/coverage-report.json`. **DECISION: make domain-graph's internal recompute PRIMARY** so
   the file location stops mattering for the gate consumer; `coverage --out` defaults to bare
   `coverage-report.json` (cwd) to match the grep validator when run standalone.
5. **Internal-recompute vs existing tests** (LOAD-BEARING): the unconditional store-recompute + fail-closed
   breaks builder unit tests that seed a bare/description-only node while passing a hand-fed `coverage:1.0`
   (those stores are genuinely `<1.0` under a faithful recompute). **DECISION: re-author those fixtures**
   to seed genuinely-accounted nodes (add a `business_rule`/`requirement` ann) so recomputed coverage is
   really 1.0 — do NOT weaken the recompute. Fix the false doc comment at domain_model.rs:220-228.
6. **Consumer deserialize compat**: +9 required fields break deserializing an older/hand-written report.
   **DECISION: hard error (schema-faithful fail-closed)** — a report missing the fields is not trustworthy;
   the emitter is the only sanctioned producer.

## Test strategy (authored before build)
- `tests/coverage_schema.rs`: recompute from a seeded in-memory store → `serde_json::to_value` →
  `jsonschema` compile+validate (mirror `tests/domain_model_schema.rs`). Assert a bare/description-only
  behavior-bearing node drives `coverage < 1.0`; assert an empty graph is vacuously 1.0 (explicit).
- Two-predicate proof: a described-but-rule-unextracted node → UNACCOUNTED → gate DENIES (the
  vacuous-gate regression guard).
- **Rules-engine regression guard** (critical finding #1): a store of N bare `NodeKind::Rule` nodes and
  ZERO Module/Function → recompute yields `coverage < 1.0` (gate DENIES), NOT vacuous 1.0.
- **Estate-behavior regression guard** (critical finding #2): a bare `Other("cics_program")`/
  `Other("db2_table")` node drives `coverage < 1.0`; a bare `Other("dataset")`/`Other("racf_user")` node is
  NOT counted (denominator stays 0 for a purely-structural graph).
- **Exhaustiveness**: a compile-time guarantee (no wildcard arm) — noted so a reviewer confirms the match
  has no `_ =>` escape; plus a test that every native `NodeKind` variant classifies deterministically.
- Defense-in-depth: a passed report claiming `coverage:1.0` while the store has an unaccounted node →
  domain-graph STILL fails closed.
- `domain-graph` consumer deserialize test (bin:567 path) + the recompute-agrees-with-file cross-check.
- `tests/schema_vendor_pin.rs` drift-guard: iterate EVERY vendored schema byte-copy (tests/
  domain-model.schema.json + the NEW tests/coverage.schema.json) vs `../wicked-brain/schemas/`, skipping
  when the sibling is absent.
- Re-authored builder fixtures (decision #5) genuinely recompute to 1.0.

## Scope / out-of-scope
- IN: the coverage half (emitter + store-bound recompute + wire fix + drift-guard). Closes cross-product
  finding #15 + the coverage-report wire bug; provides the emitter milestone #6 (end-to-end run) needs.
- OUT (tracked elsewhere): OUTPUT governance is a SEPARATE gap (no Stop/SubagentStop → output-gate-hook);
  the header comment at execute_wrapped.rs:7 now contradicts core#24 and should be corrected in passing.
  Milestone #3 (core#26) populates rules; #4 (core#27) retargets the workflow.
