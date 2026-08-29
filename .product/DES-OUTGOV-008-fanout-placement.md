# DES-OUTGOV-008 — Fan-out contract across the store split + cross-repo doctrine placement (AW-5 / AW-6)

**Status:** decided (placement default) / shipped (fan-out contract) · 2026-08-29 · recon-2026-08 arch-R3 + arch-R20.
**Decision half (AW-6):** cross-repo doctrine placement — this is the decision record the recon
required before the seed corpus (AW-13) lands anywhere.
**Contract half (AW-5):** the manifest + `wicked-core rules fanout` implementation the decision
rides on (`crates/wicked-governance/src/fanout.rs`, CLI in `src/bin/wicked-core.rs`).

## 1. Context — why an import is not "done" after `rules ingest`

Enforcement and discovery read DIFFERENT stores in the same governed run, deliberately
(post-FINDING-067):

- the **gate hook** reads the operational store handed via `WICKED_GATE_DB`
  (`execute_wrapped.rs` sets it from `gov.db_path`);
- the **worker's estate MCP** binds the run repo's (or verified project's) code graph
  (`repo_estate_mcp_parts`, decided upstream by `actor::run_code_graph_db`) — it deliberately no
  longer falls back to `gov.db_path`;
- **guidance recall** reads a knowledge store (the estate MCP's knowledge engine; core's own
  `RunKnowledge` sidecar convention is `<estate>.knowledge`).

`wicked-core rules ingest <dir> --db F` populates exactly ONE store. An import into one home
silently misses the other lanes: the rule enforces but is undiscoverable (or the reverse), and the
rationale is never citable. Separately (recon §3.11), graphs and knowledge sidecars are
per-repo/per-project and edges do not resolve across repos — so cross-repo doctrine (root CLAUDE.md
decisions, TARGET-ARCHITECTURE planes, event grammar) had NO home a worker governed in an arbitrary
repo would read.

## 2. AW-5 — the fan-out contract

One import = one **manifest**, keyed on the stable `PAT-`/`POL-` conformance-rule ids, mapping every
rule to its copy in each lane, with per-lane smoke verification through the same read path a
governed run uses. Implemented as `wicked-core rules fanout <dir> …` over
`wicked_governance::{load_ruleset, fanout}`; `<dir>` is the `rules ingest` layout
(`policies/*.json`, `rules/*.json`, frontmattered `**/*.md`), loaded with the same fail-loud
semantics (empty population, cross-lane duplicate ids, malformed files all refuse).

### 2.1 Lanes

| Lane | Copy | Transport | Smoke (same read path the consumer uses) |
|---|---|---|---|
| (a) enforcement | `Policy` nodes (deny path) + `ConformanceRule` nodes (recall→obligation) | `cli:<db>` for OFFLINE stores; `crew-api:<url>` for daemon-held stores — **never the CLI against a daemon store** (single-writer invariant, `gate_hook.rs`) | `recall_rules` + policy-node round-trip on a FRESH handle; crew-api lanes verify out-of-process via `GET /api/v1/governance/rules/preview` |
| (b) discovery | `ConformanceRule` as native `NodeKind::Rule` in each repo/project graph the workers' estate MCP binds. Deny-path `Policy` objects do NOT replicate here — they are enforcement machinery, not doctrine | `cli:<db>` per graph | `recall_rules` (what MCP `rules.recall` / `RulesInventory` serve) on a fresh handle, per graph |
| (c) knowledge | One rationale chunk per rule, chunk id `rule-rationale/<ID>` (stable ⇒ re-ingest UPSERTS, never duplicates), `source` = the rule's `provenance.ref` (the wiki URI), the `PAT-`/`POL-` id embedded in the chunk text so a cited answer names the enforceable twin | `cli:<db>` per knowledge store | `KnowledgeEngine::recall(<rule-id>)` must return a chunk carrying the id, per store |

Fail-loud: any missing copy fails the WHOLE fan-out naming the lane, target, and missing ids — a
partial import that reads as "governed" is the exact fail-open this contract exists to prevent. The
smoke re-opens every store after the write handle drops, so it proves the durable state the
worker-visible `--db` serves to the next process. **Addition only** — retirement propagation across
the same lanes is AW-24 (arch-R22), manifest-keyed on these same ids.

### 2.2 Daemon stores

`--enforcement-crew-api <url>` records the transport in the manifest (`verified: false`, a note
naming the invariant) and emits the concrete enforcement copy as
`<manifest>.crew-payload.json` (`{policies, rules}`, ready for
`POST /api/v1/governance/policies` + `POST /api/v1/governance/rules` — audited, retire-not-delete).
Belt-and-braces: any lane path resolving under `~/.wicked-crew` (the daemon state home) is refused
outright, before any lane is written.

### 2.3 Manifest format (version 1.0)

```json
{
  "manifest_version": "1.0",
  "scope": "workspace",                      // "repo" | "workspace" — §3
  "source_dir": "<ruleset dir>",
  "generated_at": 1750000000,
  "enforcement": { "transport": "cli", "target": "<db>", "verified": true },
  "discovery":  [ { "db": "<repo-a graph>", "verified": true },
                  { "db": "<repo-b graph>", "verified": true } ],
  "knowledge":  [ { "db": "<knowledge db>", "verified": true } ],
  "rules": {
    "PAT-001": {
      "rule_type": "pattern", "severity": "critical",
      "statement": "no plaintext secrets",
      "source": "wiki://secure-coding#PAT-001",
      "enforcement": "cli:<db>",
      "discovery": ["<repo-a graph>", "<repo-b graph>"],
      "knowledge": ["<knowledge db>#kchunk:rule-rationale/PAT-001"]
    }
  },
  "policies": { "pol-deny-secretleak": { "enforcement": "cli:<db>" } }
}
```

A `crew-api` enforcement lane serializes as
`{ "transport": "crew-api", "target": "<url>", "verified": false, "note": "…" }` and per-rule rows
carry `"crew-api:<url> (pending)"`. `verified` is only ever `true` for a lane THIS process smoked;
the fan-out errors rather than emit a manifest with an unverified cli lane.

## 3. AW-6 — DECISION: where cross-repo doctrine lives

**Decision: option (a) — replicate-to-every-repo via fan-out `scope: workspace`. Zero engine
change.** (Recon arch-R20's recommendation, adopted.)

### 3.1 `scope: workspace` semantics (spec'd into the manifest format, §2.3)

- `scope: "workspace"` declares the ruleset cross-repo doctrine: the discovery lane MUST carry one
  copy per **live repo graph**. The caller (orchestrator / CI / the AW-13 corpus job) enumerates
  the repos and passes one `--discovery-db` per graph; the manifest records each as its own
  verified target, and each per-rule row lists every graph holding a copy.
- A workspace fan-out with ZERO discovery targets is an error (`fanout.rs`) — a workspace rule
  that lands in no repo graph is recallable by no governed worker, which is the recon's original
  gap reproduced with extra steps.
- `scope: "repo"` (default) is single-repo doctrine: that repo's graph (and optionally its
  verified project graph).
- Sync across the N copies is tractable because every write is **id-keyed and idempotent**: rule
  nodes upsert at `conformance_rule/<id>`, rationale chunks at `rule-rationale/<id>` — re-running
  the fan-out refreshes in place (proven by `refanning_out_is_idempotent_in_every_lane`).

### 3.2 Why (a) over (b)

- Uses only shipped mechanics: `register_rule`, the per-repo graphs workers already bind, the
  knowledge engine recall already serves. No new resolution machinery, no second `--db` on the
  estate MCP or the gate.
- Composes with arch-R3 (this contract) and arch-R7 (drift/re-ingest) as pure data.
- The cost — N copies to keep in sync — is bounded by idempotent re-ingest plus the manifest
  receipt saying exactly where every copy went (which is also what AW-24's kill switch needs).

### 3.3 Option (b) — workspace-root store: documented and PARKED (P-2)

A single workspace-level store that the estate MCP and the gate additionally resolve. One copy, no
replication — but it requires new engine machinery: multi-`--db` resolution in the estate MCP, a
second store handle at the gate, and a precedence rule for a repo rule vs a workspace rule with the
same facet match. **Parked as P-2 in the recon-2026-08 task plan: revisit only if replication cost
bites.** Unparking has a hard precondition — an **estate-owner ruling** on multi-`--db` resolution
and gate precedence (recon §7 open question 1). That ruling has NOT been sought or given; nothing
in this record pre-empts it, and no engine work toward (b) may start before it exists.

## 4. Verification

- `crates/wicked-governance/tests/fanout.rs` — the AC: one import lands in ALL lanes a governed
  run reads, smoke-proven per lane on temp stores; idempotent re-run; crew-api pending semantics;
  fail-loud loading.
- `tests/rules_fanout_cli.rs` — the CLI end-to-end: manifest receipt shape, per-lane VERIFIED
  reporting, the `~/.wicked-crew` fence (fires before any lane is written), crew payload emission,
  argument hygiene.
- Module tests in `fanout.rs` — rationale-chunk content, stable chunk ids, workspace-scope
  refusal, manifest serialization round-trip.
