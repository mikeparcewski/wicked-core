# AW-13 seed corpus — ingest runbook

The value-ordered doctrine seed of the graph-backed architecture wiki (estate ADR-011,
arch-R9), packaged as data + a repeatable driver over the shipped `wicked-core rules`
machinery. Nothing here is a new parse path: every rule materializes through the
MarkdownAdapter → `normalize_bundle` fail-closed pipeline, groups under a `NodeKind::RuleSet`
parent via frontmatter `domain:` (AW-13), fans out across the deliberate store split (AW-5/AW-6),
and relinks to enforcing code by qualified `symbol_ref` (AW-9).

## Layout

| Path | What |
|---|---|
| `manifest.json` | THE SEED MANIFEST — doctrine source list with per-source `enforcement_class`, RuleSet grouping, upstream digests, lanes, and the P-4 assumption for `TARGET-ARCHITECTURE.md` |
| `corpus/*.md` | Frontmattered rule docs (seed projections of upstream doctrine that is not itself adapter-ingestable); estate's ADR-001..012 are staged verbatim beside them at run time (`adr/`) |
| `seed_wiki.py` | The repeatable driver: stage → index → fanout → relink → drift → bulk knowledge → recall proof, scratch stores only |
| `gen_event_catalog.py` | AW-21 generated views: regenerates the event-catalog TABLES (`wicked-core/EVENTS.md` + the marker block in `wicked-bus/reqs/SPEC.md`) from the machine-readable producer seams; `--check` = staleness gate, `--drift [--json]` = declared-vs-emitted query |
| `event-catalog-annotations.json` | Curated trigger/payload prose merged into the generated catalog rows (keys must match scanned event types — unknown keys fail generation) |
| `evidence/` | Committed captures from the proof run against the INSTALLED released binaries (see below) |

## What "seeded" means (the AW-13 ACs)

1. **Doctrine recallable via BOTH tools, with citations** — `rules.recall` returns every seed
   rule with its `provenance.ref` (`<path>@<blob sha>#<RULE-ID>`), and
   `knowledge.recall {scope_prefix: "wiki:"}` returns scoped chunks each carrying a `source`
   URI. Proven against the **installed released `wicked-estate-mcp`** (0.15.1, crates.io) —
   never against a workspace build.
2. **RuleSets populated** — one native `NodeKind::RuleSet` node per doctrine domain
   (`plane-boundaries`, `storage-doctrine`, `mcp-surface`, `agent-behavior`, `event-grammar`,
   `cross-platform`, `engineering-doctrine`, plus `architecture-wiki` from estate ADR-011),
   membership as native `Contains` edges — what `RulesInventory` lists.
3. **The doc↔gate exemplar** — `corpus/engine-contract.md` rules carry `symbol_ref:` directives
   to the enforcing estate code; `rules relink` derives the `Governs` edges after
   `wicked-estate index` (report in `evidence/relink-report.json`).

## Running it

```sh
# Prereqs: the released estate binaries, installed from crates.io (the proof target):
cargo install wicked-estate --version 0.15.1 --root <PREFIX>
cargo install wicked-estate-mcp --version 0.15.1 --root <PREFIX>
# and a wicked-core binary carrying the rules subcommands (cargo build --bin wicked-core).

python3 crates/wicked-governance/seed/seed_wiki.py \
  --core-bin   target/debug/wicked-core \
  --estate-bin <PREFIX>/bin/wicked-estate \
  --mcp-bin    <PREFIX>/bin/wicked-estate-mcp \
  --estate-src <path to a wicked-estate checkout> \
  --workspace  <the wicked workspace root> \
  --bus-spec   <workspace>/wicked-bus/reqs/SPEC.md \
  --scratch    /tmp/aw13-seed-scratch \
  --evidence   crates/wicked-governance/seed/evidence
```

Every store the driver writes lives under `--scratch` (`enforcement.db`,
`discovery-graph.db`, `knowledge.db`, `memory.db`, `xedge.db`). It **refuses** any store path
under `~/.wicked-estate`, `~/.wicked-crew`, or `~/.wicked-brain`, and it never talks to a
daemon-held store (single-writer contract, DES-OUTGOV-008).

The driver **exits non-zero** unless: fanout smoke-verified every lane (rules + RuleSets +
policies re-read through the worker's own read path), relink linked every `symbol_ref` with
zero drift findings, `rules drift` came back clean over the staging root, every expected seed
rule came back from `rules.recall` **with** a provenance ref, and every scoped
`knowledge.recall` hit carried a `source`.

## Assumptions this seed documents (rather than hides)

- **`TARGET-ARCHITECTURE.md` has no owned repo home yet (parked P-4).** It lives at the
  workspace root's `scratch/`, outside every git repo, so it cannot be adapter-ingested in
  place; `corpus/plane-boundaries.md` is its committed projection and `manifest.json` pins the
  upstream content digest the projection was derived from. When P-4 lands the doc in an owned
  home, frontmatter it there and retire the projection via a supersedes edge.
- **Workspace fan-out** (AW-6 replicate-to-every-repo): the scratch proof run passes ONE
  indexed repo graph (`wicked-estate`) as the discovery stand-in; production seeding
  enumerates every live repo graph as repeated `--discovery-db` flags.
- **ADR direct ingest**: estate's `docs/adr/*.md` already satisfy the frontmatter contract
  (AW-12), so they stage verbatim — only ADR-011 mints rules (POL-1101..POL-1104, RuleSet
  `architecture-wiki`); the rest are doc-only ingests whose text value arrives through the
  bulk-knowledge lane (`wiki:adr`).

## Value order (arch-R9)

TARGET-ARCHITECTURE (planes/contracts/boundaries) → root CLAUDE.md key decisions → estate
Universal Don'ts → agent-behavior R1–R7 → ENGINE-CONTRACT invariants (with `enforced_by`
Governs edges) → event grammar (grammar POL + domain whitelist; recall-valued, the bus enforces
at emit) → ADR-001..012 → bulk docs as knowledge. Rule-id blocks per source live in
`manifest.json` (`rule_id_blocks`).
