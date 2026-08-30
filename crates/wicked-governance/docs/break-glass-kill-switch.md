# Break-glass: the bad-rule kill switch (AW-24 / arch-R22)

**When to use this:** a mis-authored rule or deny policy is denying (or obligating) every
governed run that recalls it, and you need it withdrawn from the whole estate NOW — not just
from the store in front of you.

**Who:** an OPERATOR, always. No agent self-retirement — R8's authorship contract applies in
reverse: workers argue against a rule in the run transcript; only a human retires it. Crew's
retire routes are audited (`governance.rule.retired` / `governance.policy.retired` audit events);
the CLI below is an operator terminal command.

**What "retire" means:** retire, not delete (FINDING-038). The node survives, marked
`retired: true`, so past decisions citing the id stay explicable; it just stops being selected
and recalled. The knowledge rationale keeps its text behind a `[RETIRED …]` marker for the same
reason.

---

## Why one command must touch three stores

A rule imported through `wicked-core rules fanout` lives as **three copies** (deliberate,
post-FINDING-067 — enforcement and discovery read DIFFERENT stores in the same governed run):

| lane | store | consumer read path |
|---|---|---|
| enforcement | the gate hook's `--db` (`WICKED_GATE_DB`) | `recall_rules` → gate obligations/denials |
| discovery | every repo/project code graph the manifest row names | worker's estate MCP `rules.recall` |
| knowledge | the guidance store | `knowledge.recall` (cited rationale) |

Retiring only the copy in front of you leaves the others silently serving a withdrawn rule.
The fan-out **manifest** (keyed on the stable `PAT-`/`POL-` ids) is the map of where every copy
landed — so retirement is manifest-keyed too, and ONE op reaches every lane.

## The drill (rehearsed — transcript in `../evidence/aw24-kill-switch-drill/`)

### 1. Identify the rule and the manifest that imported it

The manifest is the receipt `rules fanout` wrote (e.g. `fanout-manifest.json` next to your
import runbook; the seed corpus's copy is `../seed/evidence/fanout-manifest.json`). Confirm the
id is in it:

```sh
python3 -c "import json;m=json.load(open('fanout-manifest.json'));print(sorted(m['rules']),sorted(m['policies']))"
```

### 2. (Optional) preview what the estate currently serves

```sh
wicked-core rules recall --db <enforcement-db> --json   # the gate's view
wicked-core rules recall --db <repo-graph-db>  --json   # the worker MCP's view
```

On a crew-daemon store, use the daemon's preview instead:
`GET <crew>/api/v1/governance/rules/preview`.

### 3. Pull the switch

```sh
wicked-core rules retire --id PAT-XXXX --manifest fanout-manifest.json --out retire-receipt.json
```

- Multiple `--id` flags retire several rules in one op.
- A deny policy id (from the manifest's `policies` map) rides the same command; its only copy is
  the enforcement lane.
- **Deleted/superseded wiki doc** → retire its derived rules explicitly, never orphan them:
  `wicked-core rules drift --dir <docs> --db <store>` reports the orphans
  (`doc_missing`), then

  ```sh
  wicked-core rules retire --doc <path/as/the/manifest/records/it> --manifest fanout-manifest.json
  ```

  selects every rule the manifest derived from that doc (path component only — the `@sha` /
  `#anchor` parts of the recorded ref don't participate). Re-running drift afterwards shows the
  orphans cleared (`skipped_retired` — retirement IS the healed state).

What the one op does per id:

1. **enforcement** — cli store: retired here, then re-opened FRESH (read-only, the gate hook's
   own open) and verified gone from `recall_rules`. Daemon-held store: NEVER CLI-written
   (single-writer invariant) — the receipt records the pending action:
   `DELETE <crew>/api/v1/governance/rules/<id>` (policies:
   `DELETE <crew>/api/v1/governance/policies/<id>`), verify via
   `GET <crew>/api/v1/governance/rules/preview`.
2. **discovery** — every graph db the manifest ROW names (a `scope: workspace` rule retires in
   every repo replica), each verified gone from recall the same way.
3. **knowledge** — the rationale chunk (`rule-rationale/<ID>`) is re-written with a
   `[RETIRED at unix:<ts> — non-normative; withdrawn by operator kill-switch]` prefix, original
   text preserved. Verified on a fresh open. Idempotent (no double markers).

Each graph-lane retire also **bumps the store's graph version**: the estate MCP version-caches
`rules.recall` responses inside the store itself, and without the bump a worker could keep
recalling the PRE-retire cached response indefinitely (the first drill rehearsal caught exactly
this). A failed bump fails the lane.

Each graph-store state change emits `wicked.estate.rule.retired` on the bus seam (AW-22) — the
propagation trail observers (studio, catalog regen) key on.

### 4. Read the receipt — the op is not done until it verifies

`retire-receipt.json` is the retirement twin of the fan-out manifest: one row per id per lane,
with `status` (`retired` / `already_retired` / `absent` / `pending` / `failed`) and `verified`.

- **exit 0, `pending: 0`, `all_cli_lanes_verified: true`** — fully propagated.
- **`pending > 0`** — a crew-api lane awaits your DELETE; the kill switch is NOT fully
  propagated until you complete it and check `rules/preview`.
- **exit 1 / any `failed` lane** — the rule may still be recallable somewhere a governed run
  reads. Fix the named lane and re-run (the op is idempotent); treat the estate as UNGOVERNED
  for this rule until the receipt verifies.
- **`absent`** — the manifest names a store the copy isn't in. Recall serves nothing (the end
  state you wanted holds), but audit the disagreement: the live copy may sit in a store this
  manifest does not know (wrong manifest? a later re-import?).

### 5. Confirm through the consumer read path

```sh
wicked-core rules recall --db <enforcement-db> --json | python3 -c "import json,sys;r=json.load(sys.stdin);print('PAT-XXXX' in [x['id'] for x in r['rules']])"
# → False
```

The next governed run's gate recall no longer loads the rule; the worker's `rules.recall` no
longer lists it; a `knowledge.recall` that still surfaces the rationale shows the `[RETIRED …]`
marker instead of presenting it as current doctrine.

---

## Failure modes this design refuses

- **Unknown id** → the WHOLE op refuses before any store is touched (typo or wrong manifest —
  never a partial guess).
- **`--doc` matching nothing** → refuses ("I retired the deleted doc's rules" must never be
  claimable when zero rules were selected).
- **Daemon-held path in any cli lane** → refuses (single-writer invariant), naming the crew API
  route to use instead.
- **Mid-op lane fault** → does NOT bail (a mid-way bail leaves earlier lanes retired and later
  ones unknown — the exact silent partial state a kill switch exists to prevent); the fault is
  collected into the receipt, the exit code goes non-zero, and re-running is safe.
- **Node deleted instead of retired** → verification FAILS the lane ("retire-not-delete
  violated") — past decisions citing the id must stay explicable.

## Rehearsal policy

Re-run the drill whenever the retire/fanout modules or the manifest shape change — it is ONE
command, end-to-end on temp stores, with every verification going through the real consumer
surfaces (the gate's recall funnel, the installed estate MCP's `rules.recall` /
`knowledge.recall`, `rules drift`):

```sh
WICKED_CORE_BIN=<built wicked-core> bash ../evidence/aw24-kill-switch-drill/drill.sh
```

The committed transcript + artifacts of the last rehearsal live in
`../evidence/aw24-kill-switch-drill/`. A break-glass path that was never rehearsed is a
break-glass path that fails during the incident.
