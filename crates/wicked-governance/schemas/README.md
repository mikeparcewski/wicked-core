# Governance schema bundle — LIVE OWNER

**This directory is the canonical home of the wicked governance JSON Schemas.**
It was re-homed here (AW-2 / arch-R10, 2026-08) from the retired **wicked-brain**
repo (`wicked-brain/schemas/`, frozen archive — read-only, never modified), lifted
byte-for-byte at bundle `VERSION 1.1.0`. `wicked-governance` is the crate that
enforces these contracts (`conformance.rs` write-boundary invariants, the
`domain_model.rs` builder), so the crate owns the schemas it enforces.

| File | Role | Own contract version (`$id` / `metadata.schema_version`) |
|---|---|---|
| `conformance-rules.schema.json` | PRESCRIPTIVE rules applied TO code — the wire format `ConformanceRule` mirrors | 1.0.0 |
| `domain-model.schema.json` | DESCRIPTIVE domain model mined FROM code — garden STEERS on it | 1.0.0 |
| `coverage.schema.json` | Front-half coverage report wire shape | 1.0.0 |
| `vocabulary.schema.json` | Domain vocabulary spine | 1.0.0 |
| `VERSION` | Semver of the whole **bundle** (currently `1.1.0`) | — |

## Version semantics (two versions, deliberately)

- **Bundle version** (`VERSION` file): bumps when ANY file in the bundle changes
  (additive optional field = patch; new required field = minor; invariant change
  = major). Currently `1.1.0` (the 1.0.0→1.1.0 bump added `conformance-rules.schema.json`
  to the bundle — archive commit 75735b9).
- **Per-schema contract version** (the `$id` version segment and each schema's
  `metadata.schema_version` const): the version a *document* carries and a
  consumer validates against. Independent of the bundle semver — the schemas say
  so themselves. All four are at `1.0.0` today.

Never edit a schema without bumping both appropriately, and never edit the JSON
in a consumer's vendored copy — re-vendor from here.

## Wiring (how the crate stays the live owner)

- `src/schemas.rs` embeds every file via `include_str!` and unit-tests that each
  parses, that `$id`/const versions agree, and that the crate's fail-closed
  `VALID_SOURCE_KINDS` vocabulary (INV-C4) equals the schemas' shared
  `$defs/provenance.source_kinds` enum.
- The root-package wire-fidelity tests (`wicked-core/tests/domain_model_schema.rs`,
  `tests/coverage_schema.rs`) validate emitted output against THESE copies
  (`include_str!` into this directory) — no second in-repo copy exists.
- `wicked-core/tests/schema_vendor_pin.rs` proves lift fidelity: while this
  bundle's `VERSION` still equals the frozen archive's, every schema here must be
  byte-identical to `wicked-brain/schemas/` (skips when the archive is absent, or
  once this owner legitimately moves past the archived version).

## Consumers

- **wicked-garden** vendors `domain-model.schema.json` at
  `skills/domain/vendor/` and gates drift with
  `tests/domain/test_schema_vendor_pin.py`, which byte-compares its vendored copy
  (and bundle `VERSION`) against THIS directory when the sibling `wicked-core`
  checkout is present, or against `WICKED_SCHEMA_OWNER_DIR` in CI. That test is
  the cross-repo CI sync check.
- The retired `@wicked/domain-model-schema` npm bundle (`index.mjs`/`package.json`
  in the archive) was NOT carried over — JS consumers vendor bytes from here
  until/unless a package is published from this owner.

## Schema nodes in the graph (AW-3 — done)

arch-R10's graph-registration half landed with AW-3:
`wicked_governance::register_schema_nodes` (in `src/schemas.rs`) mints one node
per schema file — `NodeKind::Other("governance_schema")`, synthetic symbol keyed
by the schema's **`$id`** (version-addressed: a contract bump mints a new node),
metadata carrying `file`, `schema_id`, `contract_version`, `bundle_version`, and
`title` — so `Rule` nodes can point at the exact contract they were validated
under. `wicked-core rules ingest` registers/refreshes them on every successful
ingest. One residue stays deliberately open: `governance_schema` rides
`NodeKind::Other(...)` because node-kind consts live in `wicked-apps-core`,
whose consts pass belongs to AW-12/AW-19 — promote it to a first-class const
there when that lane lands.
