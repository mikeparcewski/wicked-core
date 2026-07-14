# DES-OUTGOV-001 — Output governance + domain graph, re-homed onto estate's graph

**Status:** DESIGN (recon → DEFINE → **DESIGN** → plan-test → do → test). Supersedes the wicked-brain
JS-SQLite implementation of the domain-brain + output-governance epic (crew#33).
**Owner directives (2026-07-13):** (1) the Rust apps on estate's graph via `wicked-apps-core` are the
target; (2) **no coexistence** — brain's JS SQLite stores (Migrations 7 + 8) are retired; (3) **both**
domain-modeling AND conformance move off brain; estate becomes **domain-aware** (this formally amends
DES-DOMAIN-BRAIN-CONTRACT §59's "estate is domain-agnostic" — estate's native `Rule/RuleSet/Condition/
Action/Fact` node kinds were built for exactly this). `archived/anti-legacy` is the proven reference.

## 1. Why (the fork this corrects)
The domain-brain/output-governance work was built as **new brain `better-sqlite3` tables** — which
(a) locks it to SQLite, breaking the "SQLite default, Postgres optional" model estate already honors,
and (b) duplicates data whose home is **estate's graph**. Root cause: the originating recon was scoped
to brain + anti-legacy + garden and never checked `wicked-core`/`wicked-apps-core`, which already maps
domain entities onto estate `Node`s (`ToNode`/`FromNode`) and defines a `ConformanceClaim`. See
`wicked-brain [[domain-brain-architecture-fork]]`.

## 2. What already exists (do NOT rebuild) — evidence
The load-bearing machine is live + tested inside `wicked-core`:
- **`wicked-apps-core`** — re-exports estate graph (`Node/Edge/NodeKind/EdgeKind/GraphRead/GraphWrite`),
  `ToNode`/`FromNode`, `open_store`, the domain node/edge vocab (`policy`, `conformance_claim`, `governs`,
  `gates`, …), the `ConformanceClaim` struct, the `wicked.crew.policy.*` events.
- **`wicked-governance`** (v0.3.0, 8/8 tests) — deny-dominates decision core; estate persistence via
  `ToNode`/`FromNode` + `begin_batch/upsert_nodes/upsert_edges/commit_batch`; deterministic attestable
  `ConformanceClaim` (`claim_id = sha256(scope,phase,decision,context_ref,evaluator)`); evaluator≠creator;
  writes a `Governs` edge rule→symbol; **per-output (PreToolUse) evaluation is BUILT** (`gate_hook.rs`).
- **`wicked-orchestration`** (12 tests, structural falsifiers) — single-writer reducer; structural gate
  veto `reject ⇒ ¬approved`, mutation-proved. **Do not touch** (a `Decision::Deny` already vetoes downstream).
- **`wicked-core/src`** — the gate ladder + deny-dominance `.or()` fold (`pipeline.rs`), governance verdict
  already composed at `execute.rs`.
- **estate** — Louvain `clusters` (call-affinity), `Rule/RuleSet/Condition/Action/Fact` node kinds,
  `Edge` carrying `Confidence`+`Provenance`, typed `Annotation`s, `nodes --json --semantics`.

## 3. Target model — domain + conformance as estate graph
Both the domain graph (anti-legacy: `domain`/`requirement`/`entity`/`rule`) and conformance rules
(pattern/policy) land on estate node kinds. The shapes are already correct (brain's schemas were ported
byte-faithfully from anti-legacy's `requirements-graph.enriched.schema.json`, rule = `{id, statement,
source_ref, confidence, provenance}`); only the **store** changes.

| Concept | estate node | grouped by | key edges | carriage |
|---|---|---|---|---|
| Domain (capability) | `RuleSet` | — | `Contains`→requirements | annotation `class=domain` |
| Requirement | `RuleSet` (or `Fact`) | Domain | `Contains`→rules; `Governs`→symbols | annotation `class=requirement` |
| Entity | `Fact` / `Other("entity")` | Domain | `HasType`/`References` | annotation |
| Business rule | `Rule` | Requirement | `Governs`→`symbol_ref` | see below |
| Conformance rule (pattern/policy) | `Rule` | conformance `RuleSet` | `Governs`→`symbol_ref` (optional) | see below + `rule_type`, `severity`, `targets{}`, `compliance{}` |

**Confidence + provenance carriage** (shared spine, structural not stringly):
- on the `Governs` **edge** — `Edge` natively carries `Confidence` + `Provenance` (`ResolutionTier` →
  `Provenance::Extractor("outgov-v1" | "domain-graph-v1")`), so recall/staleness are graph-native; AND
- as a typed **`Annotation`** on the `Rule` node (`confidence`, `source_type`/`source_kinds`,
  `extraction_method`, `last_verified`) — enables `annotations_by_type` / `annotations_stale_since` recall.
- `rule_type` (pattern|policy), `severity`, `targets{language,layer,framework}`, and the compliance binding
  ride as node annotations/metadata.

**Recall replaces brain's bespoke SELECTs with graph queries:** `find_symbols{kinds:[Rule]}`,
`annotations_by_type(severity=…)`, `traverse{edge_kinds:[Governs]}`, `annotations_stale_since(…)` — all
backend-agnostic (SQLite today, Postgres via `wicked_estate_store::open_store(spec)`).

## 4. The build (a graft, not greenfield) — sequenced disjoint PRs
Each PR: branch → build → adversarial review → wait for bots → resolve → merge. Commit trailers required.

**PR-A · Retirement + salvage (wicked-brain).** Retire the fork cleanly:
- SALVAGE FIRST → a contract-only home: keep the 4 JSON schemas as the serde/validation wire contract
  (domain-model, vocabulary, coverage, conformance-rules); record the invariants (INV-1/2/3, INV-C1/C2)
  + the coverage predicate `(resolved+risk)/behavior_bearing==1.0` + `buildDomainModel` assembly + the
  vocab miner + the ingest/framework seam concepts as the port spec (RET-BRAIN-DOMAIN-001).
- DELETE (targeted, NOT `git revert #92` — that resurrects the removed codegraph): Migration 7 + 8, the
  domain-store/domain-model/coverage/vocabulary/conformance-* modules + tests, roll `schemas/VERSION` back.
- Repoint garden's `skills/modernize/vendor/` canonical-source (Q3: schema contract home).

**PR-B · Governance rule model (wicked-governance).** The graft:
- Extend `domain.rs`: a `Rule`/`RuleSet` model with `rule_type ∈ {pattern,policy}`, `confidence ∈[0,1]`,
  `provenance{source,ref,source_kinds}`, `severity`, `targets{}`, optional `symbol_ref`, optional
  `compliance{framework,control_id}`; keep `trigger.contains` regex as ONE pattern kind. Enforce INV-C1/C2.
- Persist as estate `Rule` nodes + `Governs` edges via `ToNode`/`FromNode` + the batch write path.
- **Backend-swap fix:** open the store through `wicked_estate_store::open_store(spec)` (Box<dyn GraphStore>),
  not apps-core's SQLite-pinned convenience opener — inherits the built Postgres backend.
- Reuse the compliance-framework SEAM concept (interface + config-driven no-op default + registry drop-in).

**PR-C · Per-turn guardrail + gate dimension (wicked-core).** 
- New governance entry point evaluating generated **output text** (today only tool inputs); hard→deny,
  soft→advise. Runs in the single-writer-safe `gate_hook` binary (Q6), not garden's Python.
- Wire the governance conformance verdict as the deny-dominates **layer-3 governance** signal in the
  `.or()` fold (`pipeline.rs`) — confirm at build whether it enriches the existing composed verdict or
  needs a distinct signal (orchestration recon: "effectively already the additional dimension").

**PR-D · Domain-graph builder (wicked-core, port anti-legacy).**
- A Rust component porting `antilegacy_core/domain_graph.py`: read estate's graph (Louvain `clusters`
  = call-affinity, symbols, annotations via `GraphRead`) → synthesize capability domains (never
  file-derived) → requirements → business rules → upsert as the §3 node/edge model. In-process
  (`upsert_nodes/upsert_edges`), no CLI. Port INV-1/2/3 + the coverage predicate as fail-closed writer checks.

**PR-E · Test + adversarial acceptance.** The 3-agent acceptance pipeline (run≠evaluate), evidence to
`.wicked-testing/evidence/<run>/verdict.json`; cross-product review (event grammar, shared `ConformanceClaim`,
no duplicated garden-hook interceptor).

## 5. Test strategy (authored before build, per wicked-testing separation)
- **Unit (governance):** rule round-trip `ToNode`/`FromNode` losslessness; confidence/provenance survive
  persist→recall; deny-dominates across the NEW rule types; INV-C1/C2 at write; malformed pattern fails closed.
- **Structural falsifier:** the new governance Deny still yields `reject ⇒ ¬approved` via the raw-reducer route.
- **Determinism:** identical inputs ⇒ byte-identical `claim_id` incl. new fields.
- **Backend-parity:** the rule-store suite runs against `:memory:` AND `postgres://` (feature-gated) — proves
  backend-agnosticism (the whole point of the re-home).
- **Recall-by-facet:** the graph queries that replace brain's SELECTs return the right rules.
- **Domain-graph builder:** on a fixture legacy tree, domains are capability-nouns (not file-derived),
  coverage predicate holds, output validates against the salvaged schema.

## 6. Open decisions (resolve in review; none block PR-A)
- **Q5 node-model unification:** conformance on native `Rule`/`RuleSet` (recommended) vs keep `Other("policy")`
  decision engine too. Lean: native `Rule`/`RuleSet`; keep `Policy` decision-rows as the *trigger* layer.
- **Q3 schema-contract home:** a standalone contract package vs the wicked-core engine repo. Lean: contract
  package consumed by garden + the Rust engine.
- **Q7 event surface:** wire `policy.registered/evaluated/violated` or is `conformance_recorded` enough
  (no current consumer found) — wire when a consumer appears.
- estate charter: this doc AMENDS DES-DOMAIN-BRAIN-CONTRACT §59 per owner directive (estate is now domain-aware).

## 7. Retire/salvage table (the fork's PRs)
| forked artifact | disposition |
|---|---|
| estate#59/#61 (`resolve`, `clusters --json`, `nodes --semantics`) | **KEEP** — genuine estate read/cluster surfaces the builder uses |
| domain-model/vocabulary/coverage/conformance schemas | **SALVAGE** as the wire contract |
| brain#92/#91 SQLite stores + engines (domain-store, conformance-store, ingest, frameworks) | **RETIRE** (PR-A); port logic to Rust |
| core#17 (domain-extraction WorkflowDef + coverage validator/seed/gate) | **KEEP the gate/validator mechanism**; the "brain builds the model" premise is replaced by PR-D |
| garden#986 (modernize skills emitting to brain schema) | **REWORK** — retarget emit to the estate-graph model / the salvaged contract |
| compliance-framework seam (concept) | **SALVAGE** → PR-B Rust trait |

## 8. REVISION 1 — adversarial-review corrections (supersede §2/§3/§4 where noted)
The design review (GO-WITH-FIXES) found the "already built / graft, not greenfield" framing overstated for
PR-B/C/D. Corrections, evidence-cited:

- **M3 (riskiest) — the runtime is `SqliteStore`-pinned end-to-end**, not one crate. `pipeline.rs`/`execute.rs`/
  `gate_hook.rs` take concrete `&mut SqliteStore` (`pipeline.rs:40/233/358`, `execute.rs:64/192`, `gate_hook.rs:84/153`);
  `Box<dyn GraphStore>` won't coerce, and governance's `wicked-estate-store` dep has **no `postgres` feature**. →
  **NEW prerequisite PR-B0:** refactor those signatures to `&mut dyn GraphStore` + add the `postgres` feature. Blocks B/C/D.
- **M4 — node-kind + carriage impossible as written.** The tested SELECT keys `NodeKind::Other("policy")`;
  `ResolutionTier` can't carry `0.72` and never maps to `Extractor`. → Persist edges via **struct-literal**
  `Edge { confidence: Confidence::new(x), provenance: Provenance::Extractor("outgov-v1".into()), … }`. Q5 decided:
  **native `NodeKind::Rule`** — so PR-B ALSO rewrites `SymbolQuery.kinds`, `to_node` kind, Governs source ("only the
  store changes" is FALSE). No `#[derive(Eq)]` on a float-carrying Rule struct.
- **M1 — compose at the right seam.** Governance is already the base gate (composed at `execute.rs:97-114`, doubled by
  evaluator); the `pipeline.rs:431` `.or()` folds per-unit *validator* reasons only. → Wire the output verdict at
  `gate_hook.rs::apply_hook_decisions → apply_gate` (deny already dominates via the reducer). **Delete** the pipeline-fold plan.
- **M2 — output-text capture is greenfield.** PreToolUse is tool-input-only (`gate_hook.rs:231-271`). → PR-C adds a
  NEW output-capture entry point (wrapped-CLI stdout or Stop/SubagentStop) + reuses the select/decide/NDJSON engine.
  Drop the "per-output is BUILT" claim.
- **M5 — the domain-graph builder needs an extraction front-half estate doesn't produce.** `domain_graph.py` reads a
  per-node business-rule overlay (`.anti-legacy/annotations.jsonl`), requires coverage==1.0 + a vocab glossary, and
  for MODERN code partitions by **parent source directory, NOT Louvain** (`domain_graph.py:731-866`). → PR-D must
  either declare an upstream rule-extraction step writing estate `Annotation`s (`annotation.rs:130-165`) + re-home
  `is_behavior_bearing`/coverage into Rust, OR port only the grouping. Correct §4/PR-D's "never file-derived": for
  modern code the **capability boundary IS the package dir**.
- **M6 + M7 — the value trap.** PR-B's rules must (a) have a population path — port the ingest/source-connector seam
  (`conformance-ingest.mjs`: filesystem shipped, confluence/sharepoint stubbed), and (b) be WIRED into the per-output
  gate (recall `find_symbols{kinds:[Rule]}` etc.). → Assign ingest to **PR-B**; assign recall→per-output-gate wiring to **PR-C**.
- **M8 — retirement is a surgical edit, not dead-code delete.** Migrations 7+8 are INLINE in the live
  `sqlite-search.mjs` (`:439-531`, imported by the server). → PR-A = "delete the 9 orphaned modules" (safe) +
  "**excise** inline Migrations 7+8" (surgical; dead tables persist harmlessly in existing `.brain.db`).
- **M9 — keep the schemas in place (avoids the repoint + drift-test trap).** Garden vendors brain's schema behind a
  drift test that `pytest.skip`s (fails open) if the canonical path vanishes. → PR-A **KEEPS `schemas/`** in brain as
  the wire contract (no move, so garden's vendor + drift test are unaffected + `schemas/VERSION` stays). Harden the
  garden drift test to fail-not-skip as a small follow-up. (Resolves the Q3 home to "stay in brain-as-contract for now.")
- **M10 — coverage validator is pin-coupled to brain's artifact.** `domain_extraction.rs` greps brain's
  `coverage-report.json` + pins `c4cc487a030d57b7` + names the retiring `wicked-brain-coverage` skill. → PR-D emits
  the coverage report in the salvaged shape AND retargets the workflow JSON's skill_ref; re-approve the pin if the shape changes.

### Corrected PR sequence
**PR-A** retire (delete 9 orphaned modules + excise Migrations 7+8; KEEP schemas; RET-BRAIN-DOMAIN-001) — **GO NOW**.
**PR-B0** backend-agnostic runtime (`&mut dyn GraphStore` + `postgres` feature) — prerequisite, blocks B/C/D.
**PR-B** governance native-`Rule` model (struct-literal edge carriage; rewrite SELECT/to_node/Governs) + **ingest seam**.
**PR-C** output-capture entry point + compose at `gate_hook` + **recall→gate wiring** for PR-B's rules.
**PR-D** domain-graph builder (declare the extraction source; package-dir grouping for modern code; emit salvaged
coverage shape; retarget the workflow skill_ref) + vocabulary miner.
**PR-E** 3-agent acceptance + cross-product review. **§5 parity note:** backend-parity proves nothing without a
CI-provisioned Postgres + `--features postgres`; default CI only asserts postgres is *rejected*.

## 9. REVISION 2 — PR-B0 as-built (recon corrected M3's scope; the fix is real)
Recon on the branch confirmed M3's core claim (the runtime IS `SqliteStore`-pinned) but corrected WHERE and refined
the mechanism:
- **The pinned runtime is wicked-core's ROOT `src/` crate** (the workflow engine: `actor.rs`/`campaign.rs`/
  `pipeline.rs`/`execute.rs`/`gate_hook.rs`/`domain.rs` — 46 `&mut SqliteStore` param sites), NOT the workspace
  members. The orchestration/governance/council crates were already store-generic or `&dyn`. (The earlier scoping to
  `crates/` alone was the miss — same class of incomplete-recon that caused the original fork.)
- **`&mut dyn GraphStore` alone is insufficient.** The read side already mixed `&dyn GraphRead` + `&impl GraphRead`
  (anonymous generic). A `Box<dyn GraphStore>` owner satisfies NONE of the read-generic styles (`Box<dyn GraphStore>: !GraphRead`; `&dyn GraphStore → &dyn GraphRead` needs upcasting at every call). So the actor owns a **concrete
  `AnyStore` enum** (apps-core) that forwards `GraphRead`+`GraphWrite` to `Sqlite | Postgres`; `&`/`&mut` of it coerce
  to every param style AND satisfy `S: GraphRead+GraphWrite` bounds directly. `open_store_any(spec)` dispatches on the
  spec; the reducer/runner/gate (13 fns) went `&mut dyn GraphStore` so the root's dyn functions can call them.
- **Sync/async was a non-issue:** estate's `PostgresStore::open` is SYNC and impls the sync `GraphRead`/`GraphWrite`
  (a `global_rt()` hides the sqlx async), so no runtime/async rewrite in the actor.
- **Verified:** default build 0 warnings + full suite green (apps-core 10 / orchestration 20 / governance 8 /
  council 19 / core 148); `cargo build --features postgres` compiles end-to-end; a fail-closed test asserts a
  `postgres://` spec is REJECTED (not silently SQLite) when the feature is off (§5's rejection assertion).

## 10. PR-D as-designed — domain-graph builder (recon of anti-legacy `domain_graph.py`)
Source: `archived/anti-legacy/skills/anti-legacy-expert/scripts/antilegacy_core/{domain_graph,coverage,vocabulary}.py`
(1772-line builder). It is engine-independent — funnels through `wicked_estate` + `coverage` + `vocabulary`. Port shape:

- **The extraction front-half already exists** (domain-brain phase-2): estate#61 `nodes --json --semantics` emits
  `requirement`/`requirement_validated`/`rule_confidence(max over business_rule anns)`/`out_edges`; brain#93 proved
  coverage on the live estate DB (charge=resolved, coverage 0.33, gate denies). So the READ surfaces the builder needs
  are on estate. PR-D does NOT rebuild extraction — it consumes `clusters --json --summary` + `nodes --json --semantics`.
- **Front-half coverage GATE (fail-closed):** port `assert_front_half_coverage` — the coverage-report MUST show
  `coverage == 1.0` before translating; else bail listing the unaccounted SymbolIds (§I5 refuses to translate an
  unannotated graph). Predicate salvaged in RET-BRAIN-DOMAIN-001: `(resolved+risk)/behavior_bearing == 1.0`, dead-shell excluded.
- **Grouping (M5):** modern code partitions by **PACKAGE DIR (parent source directory)**, NOT Louvain — Louvain only
  for dense legacy blobs (`domain_graph.py:91-102`). The "never file-derived" line in §4 was wrong: for modern code the
  capability boundary IS the package dir.
- **Output:** `requirements_graph.json` = `{metadata, domains[]}`, each domain carrying `requirements[]` → `rules[]`
  (rule shape `{id, statement, confidence}`, seq ids `VAL-`/`ERR-`/`RULE-`), entities with typed fields. Byte-shape must
  match the KEPT `schemas/domain-model.schema.json` wire contract (garden/wicked-testing consume it). Vocab miner
  (`vocabulary.py`) drives glossary-direct naming — port the two-axis miner (salvaged shape in RET-BRAIN-DOMAIN-001).
- **Coverage artifact + workflow retarget (M10):** emit `coverage-report.json` in the salvaged shape AND retarget
  `wicked-core/workflows/domain-extraction.json` — the `domain-graph` phase `skill_ref` (`wicked-garden-domain-graph`)
  and the `coverage` phase `allowed_skills` (`wicked-brain-coverage`, retiring) → the new Rust builder; **re-approve the
  `validator_pin c4cc487a030d57b7`** if the coverage-report byte-shape changes (`src/domain_extraction.rs` + the pin test).
- **Home:** a `wicked-core domain-graph` subcommand consuming `wicked-apps-core` (reads estate in-process, emits the two
  artifacts) — the same composition-root pattern as `gate-hook`/`seed-domain-validators`. Fail-closed on coverage < 1.0.
- **PR-E** then adds the 3-agent acceptance pipeline over the builder's artifacts + the cross-product review + the
  CI-provisioned Postgres parity run (`--features postgres`).

### 10.1 PR-D split (as-built vs follow-on)
The **wicked-core side is self-contained and lands first**: the native Rust builder (`wicked-governance::
build_domain_model`, modern-mode package-dir grouping), the wire types round-trip-tested vs the kept schema, the
fail-closed coverage gate, and the `wicked-core domain-graph` subcommand. What genuinely spans OTHER repos / a workflow
design decision is deferred to a follow-on so this PR stays clean: (a) **workflow/garden retarget (M10)** —
`domain-extraction.json`'s `domain-graph` phase is skill-driven (`wicked-garden-domain-graph` + `wicked-brain-domain`);
pointing it at the deterministic native CLI means the garden skill invokes `wicked-core domain-graph` (or the phase
becomes `executes_code`), plus re-approving `validator_pin c4cc487a030d57b7` if the coverage-report byte-shape moves —
a garden + workflow change, not a wicked-core one; (b) the **vocabulary miner** (glossary-direct naming — port
`vocabulary.py`'s two-axis miner); (c) the **mainframe Louvain path** (legacy dense-code grouping — the modern
package-dir path is what M5 emphasizes and what ships here). All three are honest, scoped follow-ons; none block the
native builder's value.
