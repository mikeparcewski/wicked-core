# The architecture wiki — operator guide

The architecture wiki is the wicked ecosystem's doctrine — planes and contracts, storage
doctrine, event grammar, agent-behavior rules, engineering don'ts, ADRs — stored as **nodes in
the estate graph instead of prose in a folder**. Git stays the source of truth (a rule changes
only by doc PR); `wicked-core rules ingest` projects each frontmattered markdown doc into
`ConformanceRule`/`Policy` nodes grouped under per-domain `RuleSet` parents, every rule carrying
a provenance ref (`<doc path>@<git blob sha>#<RULE-ID>`) that points back at the exact doc
revision that minted it. From there the same corpus serves every consumer: agents recall it
mid-turn (estate MCP `rules.recall` + `knowledge.recall` under `wiki:` scopes), humans browse
and retire it in wicked-studio, CI comments with it on PRs, and crew's gates read it as
enforcement — so the doctrine you wrote is the doctrine that governs, with citations.
(Design of record: estate `docs/adr/ADR-011-architecture-wiki.md`; schema + invariants live in
this crate.)

**Advisory-first is the default posture.** Recall reports, CI comments, and the per-turn hook
never block (arch-R15); the only fail-closed enforcement is the crew/core gate ladder. Seeding
the wiki makes agents *better informed* on day one, not suddenly gated.

---

## The 60-second tour (zero → recalling)

From a `wicked-core` checkout, against a scratch store — no daemon, no real state touched:

```sh
cargo build --bin wicked-core

# 1. Ingest the shipped seed corpus (8 doctrine docs → 36 rules under 7 RuleSets)
target/debug/wicked-core rules ingest crates/wicked-governance/seed/corpus --db /tmp/wiki-demo.db

# 2. Recall what applies — severity-ordered, every rule citing its wiki URI
target/debug/wicked-core rules recall --db /tmp/wiki-demo.db --severity critical
#   Critical POL-1301: Every cross-plane interaction goes through the owning plane's contract …
#     [source: plane-boundaries.md@d4924f…#POL-1301]

# 3. Score it — population % / connection % / enforcement evidence
target/debug/wicked-core rules scoreboard --db /tmp/wiki-demo.db --dir crates/wicked-governance/seed/corpus
```

That is the whole mechanism: docs in git → rules in a store → cited recall. Everything below is
the same three verbs pointed at the real stores.

---

## Where things live

| Piece | Where |
|---|---|
| Schemas, adapters, invariants (INV-C1..C4) | this crate (`wicked-core/crates/wicked-governance`) |
| Operator CLI (`rules ingest/fanout/relink/drift/recall/list/scoreboard/retire`) | `wicked-core` binary (`src/bin/wicked-core.rs`) |
| Seed corpus + repeatable seed driver | [`seed/`](./seed/README.md) |
| Governed policy packs (per-doctrine ingest units) | [`governance/packs/`](../../governance/packs/README.md) |
| Bad-rule kill switch runbook | [`docs/break-glass-kill-switch.md`](./docs/break-glass-kill-switch.md) |
| Agent recall surface | wicked-estate MCP: `rules.recall`, `knowledge.recall` |
| Human surface | wicked-studio: **Settings → Rules**, plus the per-run Governance panel |
| Daemon-held store CRUD | wicked-crew `/api/v1/governance/*` routes |
| CI conformance seam | wicked-ci reusable workflow `rules-conformance.yml` |

---

## Seed it (AW-13)

### The proof run — scratch stores, end to end

[`seed/README.md`](./seed/README.md) is the full runbook. The driver stages the doctrine
corpus, ingests, fans out across the store split, relinks rule→code edges, checks drift, bulk-
ingests knowledge, and proves recall through the **installed released** `wicked-estate-mcp` —
refusing to touch any real store (`~/.wicked-estate`, `~/.wicked-crew`, `~/.wicked-brain`):

```sh
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

It exits non-zero unless every lane smoke-verifies through the consumer's own read path — the
committed captures from the reference run are in [`seed/evidence/`](./seed/evidence/).

### Production seeding — the real stores

One `rules fanout` call replicates a ruleset across the **deliberate store split** (AW-5/AW-6):
enforcement, discovery (one copy per live repo graph), and knowledge rationale — each lane
smoke-verified against the very store a worker will read, and the whole placement recorded in a
**manifest** (keep it: retirement is keyed on it).

```sh
wicked-core rules fanout crates/wicked-governance/seed/corpus \
  --enforcement-crew-api http://127.0.0.1:7701 \
  --discovery-db  <estate-home>/repo-graphs/<repo-key>/estate.db \   # repeat per live repo
  --knowledge-db  <estate knowledge store> \
  --scope workspace --knowledge-scope wiki:architecture \
  --manifest fanout-manifest.json
```

- **Crew daemon store = never CLI-written** (single-writer invariant, DES-OUTGOV-008).
  `--enforcement-crew-api <daemon base url>` records the lane as *pending* and emits the
  enforcement payload for you to `POST /api/v1/governance/rules` (policies:
  `/api/v1/governance/policies`), verified via `GET /api/v1/governance/rules/preview`; a
  daemon-held path passed as a `--*-db` flag is refused outright.
- **Discovery copies** go to the per-repo estate graphs (default home
  `~/.wicked-estate/repo-graphs/<key>/estate.db`, overridable via
  `$WICKED_ESTATE_REPO_GRAPH_ROOT`); `--scope workspace` means every live repo graph gets a
  replica — pass repeated `--discovery-db` flags.
- **Knowledge rationale** chunks land under the scope you pass (`wiki:architecture` for the
  seed corpus), recallable via `knowledge.recall {scope_prefix: "wiki:"}`.
- After any `wicked-estate index`, run `wicked-core rules relink` to re-derive the rule→code
  `Governs` edges at the new epoch (see *Monitor* below).

### Turn on the per-turn advisory

wicked-garden's Stop hook asks the agent to self-check each turn against `rules.recall` —
**default-on at `WG_OUTGOV=warn`** once the corpus is seeded. It is fail-open advisory at every
setting (`strict` only strengthens the wording; `off` opts a repo out — the per-repo noise
budget escape hatch). See `wicked-garden/docs/outgov-per-turn.md`.

---

## Author a rule doc (AW-3 / AW-12)

**The lifecycle is: write a frontmattered markdown doc → PR it into the owning repo → on merge,
re-run `rules ingest` (idempotent, id-keyed — re-ingest is a non-event).** There is
deliberately no `rules.write` MCP tool and no agent-side promotion path: git is the wiki's
write surface (arch-R8).

```markdown
---
id: my-doctrine-area            # required — doc identity
title: My doctrine area         # required
status: active                  # active|draft|superseded|retired (non-active mints retired rules)
date: 2026-08-30                # ISO date — required by the ADR contract (AW-12)
enforcement_class: guidance     # policy|validator|guidance (see cheat sheet)
scope: wiki:architecture        # recall scope
domain: my-doctrine-area        # RuleSet parent — what RulesInventory lists
applies_to: [plan, build]       # optional phase ids — rides onto every minted rule (STEERING inclusion)
excludes: [clarify]             # optional — the STEERING exclusion twin (withdraws these phases; dominates)
steering_type: architecture     # optional — the studio Steering sub-page (architecture|development|
                                #   security|testing|operations|compliance|design-ux; default architecture)
weight: 1.0                     # optional, finite ≥ 0 — recall order within a severity band + gate priority
---
# My doctrine area

Prose anywhere outside `## Rules` is rationale — ignored by the rule parser,
ingestable into the knowledge lane.

## Rules

- `POL-3001` (critical): One-sentence, checkable statement of the invariant.
- `PAT-3002` (warn): A pattern rule; continuation lines indent by two spaces.
  symbol_ref: crates/my-crate/src/gate.rs::enforcing_fn
```

The load-bearing details (full contract in [`src/markdown.rs`](./src/markdown.rs)):

- **Rule items** are `` - `ID` (severity): statement `` — id matches `^(PAT|POL)-[0-9]{3,6}$`
  (`PAT-` = pattern, `POL-` = policy; the type is derived from the prefix), severity is
  `info|warn|error|critical`.
- **`symbol_ref:`** on an indented continuation line is a directive, not prose: it names the
  code that enforces the rule (`<repo-relative path>::<name>`), and `rules relink` re-derives
  the `Governs` edge from it after every re-index — the doc↔gate pairing.
- **Malformed docs fail loud, per file, with path and reason** — never a silent skip. Unknown
  frontmatter keys, bad severities, duplicate ids across bundles: all hard errors.
- A doc with **no `## Rules` section** is a valid doc-only ingest (rationale/knowledge value,
  zero rules).

### Enforcement-class cheat sheet (arch-R4)

| `enforcement_class` | Means | Honest when |
|---|---|---|
| `policy` | Deterministically enforced — a `wicked-governance` Policy trigger or a named engine gate denies on violation | the gate actually exists; name it in the statement (e.g. `engine:pre-build-scope`) |
| `validator` | Verified by a named deterministic check (behavior tests, conformance suite) | the check runs in CI |
| `guidance` | Recall-valued doctrine — informs agents and reviewers, nothing denies on it | always (the honest default) |

**Class is a claim, not a wish** (the core#296 lesson): a rule that says "enforced" when no gate
holds it is worse than silence. When in doubt, ship as `guidance` and upgrade the class in the
PR that lands the gate.

Ready-made examples: the [seed corpus](./seed/corpus/) (8 doctrine docs) and the
[governance packs](../../governance/packs/README.md) (`phase-scope` pairs a `policy`-class doc
with its engine gate).

---

## Consume it

### Agents (estate MCP)

- **`rules.recall`** — the single per-turn rule source (arch-R14). Faceted
  (`language`/`layer`/`framework` are wildcards: a rule with no facet matches everything),
  filterable by `severity` and `rule_type` (`pattern`|`policy`), scopeable
  (`{"scope": "wiki:architecture"}`), severity-ordered critical→info, every hit citing its
  provenance ref.
- **`knowledge.recall`** with `{"scope_prefix": "wiki:"}` — the rationale and bulk doctrine
  (ADR text, spec prose) behind the rules, each chunk carrying a `source` URI.
- Via wicked-garden, the same surfaces are skill-routed: the `mem` skill's `recall`/`answer`
  actions (knowledge + memory), and the `engineering-conformance-reviewer` skill (rule-by-rule
  semantic evaluation of an artifact against `rules.recall` output).
- The CLI twin for terminals and scripts: `wicked-core rules recall --db <store> [--type <t>] [--json]`;
  the management/audit view (decide-lane rules included, retired rows via `--include-retired`) is
  `wicked-core rules list --db <store> [--type <t>] [--include-retired] [--json]`.

### Humans (wicked-studio)

- **Settings → Rules** (`/rules`) — the RuleManager: browse the registered conformance rules,
  add/retire on the daemon store, and preview "which rules apply?" for a facet query.
- **Run view → Governance panel** — GovernanceAudit: the gate decisions of a run with the wiki
  rules each claim cites (the AW-14 acceptance-view conformance section does the same on the
  acceptance gate).

### CI (wicked-ci)

The reusable **`rules-conformance.yml`** workflow (AW-17 seam, v1) ingests the repo's rule
corpus and posts one sticky, severity-ordered PR comment citing rule ids + wiki URIs. It is
**advisory by design — it never blocks** (missing corpus/toolchain = honest fail-open skip).
Copy `examples/rules-conformance.yml` from the wicked-ci repo:

```yaml
jobs:
  rules-conformance:
    permissions:
      contents: read
      pull-requests: write
    uses: mikeparcewski/wicked-ci/.github/workflows/rules-conformance.yml@v1
    with:
      rules_dir: governance/packs   # the repo's governed rule docs
```

### Daemon API (wicked-crew)

For a crew-daemon-held store, the governance routes are the write/read surface:
`GET|POST /api/v1/governance/rules`, `GET /api/v1/governance/rules/preview` (faceted recall
preview), `DELETE /api/v1/governance/rules/:id` (audited retire), and the `policies`
equivalents — plus `governance/claims`, `governance/coverage`, `governance/graph` for the
evidence side.

---

## Kill a bad rule (AW-24)

A mis-authored rule denying every governed run is withdrawn with **one manifest-keyed command**
that retires the enforcement copy, every discovery replica, and the knowledge rationale, then
re-verifies each lane through the consumer's own read path:

```sh
wicked-core rules retire --id PAT-XXXX --manifest fanout-manifest.json --out retire-receipt.json
# deleted/superseded doc → retire everything it minted:
wicked-core rules retire --doc <path/as/the/manifest/records/it> --manifest fanout-manifest.json
```

Operator-only (no agent self-retirement), retire-not-delete (past decisions citing the id stay
explicable), and **not done until the receipt verifies** — a `pending` lane means a crew-api
`DELETE /api/v1/governance/rules/<id>` still awaits you. The full drill, receipt semantics, and
the failure modes the design refuses:
**[docs/break-glass-kill-switch.md](./docs/break-glass-kill-switch.md)**.

---

## Monitor it (AW-23 / AW-10 / AW-9)

```sh
# The population/connection scoreboard — is the wiki a populated wiki or a beautiful empty one?
wicked-core rules scoreboard --db <store> --dir <docs> [--json]
#   population:  rules active/retired; % statements typed into enforcement classes
#   connection:  % symbol_refs resolving at the current epoch; rules with live Governs links
#   enforcement: denials citing wiki rules (evidenced_by / Governs evidence_count)

# The residue re-ingest can't self-heal — orphaned / uningested / unresolvable / unlinked /
# extraneous. Read-only; exit 3 = residue found (0 clean, 1 operational error) — CI-friendly.
wicked-core rules drift --dir <docs> --db <store> [--json]

# After EVERY `wicked-estate index`: re-derive rule→code Governs edges at the new epoch.
# Unresolvable refs are REPORTED as drift, never silently dropped.
wicked-core rules relink [--ambiguity-cap N] [--json]
```

Both report commands are read-only (`open_store_ro`) — safe to run beside a live daemon. The
rhythm that keeps the wiki honest: **re-index → relink → drift → scoreboard**, and a doc PR
merge → re-ingest. Lifecycle events (`wicked.estate.rule.ingested/retired/…`, AW-22) ride the
bus seam for observers.
