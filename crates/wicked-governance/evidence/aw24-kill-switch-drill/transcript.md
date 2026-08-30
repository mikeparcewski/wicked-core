# AW-24 kill-switch drill — transcript (rehearsed 2026-08-30)

One verbatim run of `drill.sh` (see that file for the step-by-step contract):

- **binary under drill**: `wicked-core 0.4.0` built from branch `feat/aw24-kill-switch` (base `a4d3f85` + this change), debug profile
- **worker read surface**: installed `wicked-estate-mcp` **0.15.1** (`~/.cargo/bin`) — the drill's `rules.recall` / `knowledge.recall` verifications go through the real MCP stdio protocol, not this repo's code
- **stores**: temp-only (`gov.db` enforcement / `repo-graph.db` discovery / `knowledge.db`), created for the drill and never a daemon's
- **ruleset**: a copy of the shipped seed corpus (`../../seed/corpus`, 36 rules); victim = `POL-2001` (critical), deleted doc = `cross-platform.md` (PAT-1901..1904)
- **artifacts beside this file**: the fan-out manifest, both retire receipts, before/after gate + MCP recall reports, before/after drift reports, and the drill's event outbox (`emit-outbox.ndjson` — the `EMIT-DEADLETTER:` stderr lines in the transcript are the emit seam spooling `wicked.estate.rule.ingested/retired` there because the drill configures no shared estate store; that spool is deliberate drill hygiene, not a fault)

Exit code: **0** (every assertion passed).

```text

=== 0. versions ===
wicked-core 0.4.0
wicked-estate 0.15.1 (/Users/michael.parcewski/.cargo/bin/wicked-estate-mcp)
drill dir: /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run

=== 1. stage the ruleset (the shipped seed corpus, copied — the drill never edits the repo) ===
agent-behavior.md
cross-platform.md
engine-contract.md
event-grammar.md
mcp-surface.md
plane-boundaries.md
storage-doctrine.md
universal-donts.md

=== 2. fan out across the deliberate store split (one manifest, keyed on PAT-/POL- ids) ===
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.ingested` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.ingested` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
rules fanout: 36 conformance rules + 0 policies from /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/ruleset
  enforcement [cli] /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/gov.db — VERIFIED
  discovery   /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/repo-graph.db — VERIFIED
  knowledge   /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/knowledge.db — VERIFIED
  manifest → /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/fanout-manifest.json

=== 3. BEFORE — every lane serves POL-2001 as current doctrine ===
--- 3a. enforcement lane: the gate hook's recall funnel (wicked-core rules recall)
gate recall serves 36 rules, including POL-2001 — OK
--- 3b. discovery lane: the worker's estate MCP rules.recall
estate MCP rules.recall serves ['PAT-1601', 'PAT-1701', 'POL-1301', 'POL-1704', 'POL-2001', 'POL-2002'] including POL-2001 — OK
--- 3c. knowledge lane: rationale served as current (no marker)
knowledge.recall serves the POL-2001 rationale as current — OK

=== 4. PULL THE SWITCH — one manifest-keyed op: wicked-core rules retire --id POL-2001 ===
EMIT-DEADLETTER: event `wicked.estate.rule.retired` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.retired` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.retired` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.retired` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
rules retire: 1 id(s) across the manifest's lanes
  POL-2001 (conformance_rule)
    enforcement [cli] /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/gov.db — RETIRED, verified
    discovery   [cli] /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/repo-graph.db — RETIRED, verified
    knowledge   [cli] /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/knowledge.db — RETIRED, verified
  receipt → /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/retire-receipt-POL-2001.json
receipt: 3 lanes, all retired + verified — OK

=== 5. AFTER — the next recall no longer serves it, in EVERY lane ===
--- 5a. enforcement lane
gate recall: 36 -> 35 rules; POL-2001 gone, siblings intact — OK
--- 5b. discovery lane (estate MCP rules.recall — the AC's 'rules.recall showing it gone')
estate MCP rules.recall no longer serves POL-2001 (ids: ['PAT-1601', 'PAT-1701', 'POL-1301', 'POL-1704', 'POL-2002']) — OK
--- 5c. knowledge lane: rationale survives but is NON-NORMATIVE behind the marker
knowledge.recall serves the POL-2001 rationale behind the [RETIRED ...] marker — OK
--- 5d. the propagation trail: wicked.estate.rule.retired per store that changed
2 wicked.estate.rule.retired events spooled (enforcement + discovery) — OK

=== 6. DELETED DOC → EXPLICIT RETIRE (never silent orphaning) ===
--- 6a. the wiki doc is deleted
--- 6b. rules drift REPORTS the orphans (read-only, never drops)
(drift exit code: 3 — 3 = residue found, as expected)
drift reports the deleted doc's rules orphaned (doc_missing): ['PAT-1901', 'PAT-1902', 'PAT-1903', 'PAT-1904'] — OK
--- 6c. rules retire --doc turns the drift report into the explicit retire set
rules retire: --doc cross-platform.md → PAT-1901, PAT-1902, PAT-1903, PAT-1904
EMIT-DEADLETTER: event `wicked.estate.rule.retired` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.retired` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.retired` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.retired` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.retired` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.retired` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.retired` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.retired` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.retired` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.retired` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.retired` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.retired` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.retired` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.retired` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
EMIT-DEADLETTER: event `wicked.estate.rule.retired` not stored (no shared store (WICKED_ESTATE_DB unset)); spooling to outbox
EMIT-DEADLETTER: spooled `wicked.estate.rule.retired` to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/emit-outbox.ndjson
rules retire: 4 id(s) across the manifest's lanes
  PAT-1901 (conformance_rule)
    enforcement [cli] /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/gov.db — RETIRED, verified
    discovery   [cli] /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/repo-graph.db — RETIRED, verified
    knowledge   [cli] /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/knowledge.db — RETIRED, verified
  PAT-1902 (conformance_rule)
    enforcement [cli] /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/gov.db — RETIRED, verified
    discovery   [cli] /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/repo-graph.db — RETIRED, verified
    knowledge   [cli] /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/knowledge.db — RETIRED, verified
  PAT-1903 (conformance_rule)
    enforcement [cli] /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/gov.db — RETIRED, verified
    discovery   [cli] /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/repo-graph.db — RETIRED, verified
    knowledge   [cli] /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/knowledge.db — RETIRED, verified
  PAT-1904 (conformance_rule)
    enforcement [cli] /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/gov.db — RETIRED, verified
    discovery   [cli] /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/repo-graph.db — RETIRED, verified
    knowledge   [cli] /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/knowledge.db — RETIRED, verified
  receipt → /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/aw24-drill-run/retire-receipt-deleted-doc.json
receipt: all 4 doc-derived rules retired + verified in every lane — OK
--- 6d. drift again: retirement IS the healed state (orphans cleared)
(drift exit code: 3 — remaining residue is unresolvable symbol_refs only: the drill
 store indexes no code, so engine-contract.md refs cannot resolve — unrelated to AW-24)
drift: 0 orphaned, 0 uningested (skipped_retired=0) — OK

=== 7. drill artifacts ===
drift-after-retire.json
drift-before-retire.json
emit-outbox.ndjson
fanout-manifest.json
gate-recall-after.json
gate-recall-before.json
gov.db
gov.db-shm
gov.db-wal
knowledge-recall-after.json
knowledge-recall-before.json
knowledge.db
repo-graph.db
repo-graph.db-shm
repo-graph.db-wal
retire-receipt-deleted-doc.json
retire-receipt-POL-2001.json
rules-recall-after.json
rules-recall-before.json
ruleset
artifacts copied to /private/tmp/claude-501/-Users-michael-parcewski-Projects-wicked/f5d30481-90ff-4c55-9faa-abdb54e1619c/scratchpad/wave5/wicked-core-aw24/crates/wicked-governance/evidence/aw24-kill-switch-drill

=== DRILL PASSED — kill switch propagates all lanes in one op; recall no longer serves the id ===
```
