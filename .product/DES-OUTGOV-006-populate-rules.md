# DES-OUTGOV-006 — Populate conformance rules + policies into a run (milestone #3 / core#26)

Output governance can RECALL + ENFORCE, but nothing LOADS a ruleset into a run's store. This wires
population, so the output guardrail (`run_output_gate_hook`, core#21) has real rules to recall and real
policies to deny on. User-directed (2026-07-14): **both** — policies deny, conformance rules obligate.

## The two governance object types (do NOT conflate)
- **governance `Policy`** (`domain.rs`): `{ id, kind, applies_to:[phase], effect: Deny|AllowWithConditions|
  Allow, trigger:{contains: regex}, obligations, criteria, severity, rule }`. `decide` DENIES an output
  whose context (`work: <output>`) matches a fired `Deny` policy's regex trigger. **This is the
  deterministic deny path.**
- **`ConformanceRule`** (`conformance.rs`): `{ id, rule_type: Pattern|Policy, statement, severity(conf),
  targets:{facets}, confidence, … }` — carries NO regex. `recall_rules` returns those matching the
  output's facets; `attach_recalled_rules` attaches them as OBLIGATIONS on the claim (the applicable
  ruleset the output must conform to). Whether the output actually VIOLATES a pattern rule is a SEMANTIC
  check — garden's job (documented seam). NOTE: a `ConformanceRule{rule_type: Policy}` is STILL a
  conformance rule (obligation), NOT a governance `Policy` — they are different types.

## Design
### §1 — Population: `wicked-core rules ingest <dir>`
A new pre-`Core::spawn` subcommand (mirrors `coverage`/`domain-graph`), opening the store directly (a
brief single-writer command, no actor). Directory layout:
- `<dir>/rules/*.json` → `ingest_from(FilesystemAdapter::new(<dir>/rules))` → `register_rule` each
  (INV-C1/C2/C3/C4 validated at ingest; a bad rule fails LOUD — fail-closed, never a partial silent load).
- `<dir>/policies/*.json` → each file is a `Policy` OR a `[Policy]` array → `register_policy` each.
- Reports counts: `rules ingest: registered N policies + M conformance rules from <dir>`.
- Missing `rules/` or `policies/` subdir is TOLERATED (a ruleset may carry only one kind); a present-but-
  unreadable/invalid file fails LOUD. An EMPTY effective load (0 policies + 0 rules) is an error (a silent
  no-op population would read as "governed" while enforcing nothing — fail-loud, matching the adapter's
  own contract).

### §2 — Enforcement is ALREADY wired (no gate code change)
`run_output_gate_hook` (core#21/#24) already: `select`+`decide` over policies → a claim whose decision is
`Deny` when a `Deny` policy fires on the output; then `attach_recalled_rules` recalls the facet-matching
conformance rules as obligations; exit 2 on `Deny`. So once the store is populated, the guardrail enforces
with no change. `--scope`/`--phase` on the hook select which policies apply (policy `applies_to` must
include the hook's `phase`).

## Scope boundary (honest)
- IN: population (`rules ingest`) + a proof that `run_output_gate_hook` against a `rules ingest`-populated
  store DENIES a policy-violating output WITH the recalled conformance rules attached as obligations.
- OUT (a DIFFERENT gap, not this milestone): wiring the output-gate-hook into the full run LOOP as a
  Claude Stop hook. core#24 wired INPUT (PreToolUse) governance; output-POLICY deny already rides
  `apply_unit`'s unit gate (the unit's `work:output` is in the gate context), but the conformance-rule
  RECALL→obligation currently lives only in `run_output_gate_hook`, which the run loop does not yet invoke
  per-turn. Wiring that per-turn recall into a real run is end-to-end work (core#28), tracked separately.
  This milestone proves the guardrail + population, not the per-turn run wiring.

## Test strategy
- `rules ingest` round-trip: a fixture `<dir>/{policies,rules}/*.json` → ingest → the store has the
  policies (`select` returns them for the phase) + the conformance rules (`recall_rules` returns them).
- END-TO-END deny (the proof-of-done): populate via `rules ingest`, then run the REAL `output-gate-hook`
  binary against a violating output on that store → exit 2 (deny) AND the appended claim carries the
  recalled conformance rule as an obligation (`conform:<Severity>:<id>:<statement>`). Prove a BENIGN
  output that trips no policy → exit 0 but STILL carries the recalled rules as obligations (recall is
  facet-based, independent of the deny).
- Fail-loud: an empty ruleset dir (no policies, no rules) → `rules ingest` errors; a malformed
  policy/rule file → errors (never a partial load).
