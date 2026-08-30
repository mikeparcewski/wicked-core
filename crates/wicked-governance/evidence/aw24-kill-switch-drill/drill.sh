#!/usr/bin/env bash
# AW-24 / arch-R22 break-glass kill-switch drill — REHEARSED end-to-end on temp stores.
#
# Proves, through the exact read paths a governed run uses, that
#   wicked-core rules retire --id <ID> --manifest <M>
# withdraws a rule from EVERY fan-out lane in ONE manifest-keyed op:
#   - enforcement store: gone from `wicked-core rules recall` (the gate hook's funnel)
#   - discovery graph:   gone from the estate MCP's `rules.recall` (the worker's funnel)
#   - knowledge store:   rationale re-served NON-NORMATIVE behind the [RETIRED ...] marker
# and that a DELETED governed doc propagates as explicit retirement:
#   `rules drift` reports the orphans → `rules retire --doc` clears them → drift is clean.
#
# Every assertion is fail-loud: the drill exits non-zero the moment the estate
# disagrees with the receipt. Run it whenever retire/fanout or the manifest shape
# changes (see ../../docs/break-glass-kill-switch.md, "Rehearsal policy").
#
# Env:
#   WICKED_CORE_BIN        the wicked-core binary under drill (default: wicked-core on PATH)
#   WICKED_ESTATE_MCP_BIN  the installed estate MCP server    (default: wicked-estate-mcp on PATH)
#   DRILL_DIR              scratch dir for the temp stores    (default: mktemp -d)
#   EVIDENCE_DIR           where to copy the artifacts        (default: none — print only)
set -euo pipefail

WICKED_CORE_BIN="${WICKED_CORE_BIN:-wicked-core}"
export WICKED_ESTATE_MCP_BIN="${WICKED_ESTATE_MCP_BIN:-wicked-estate-mcp}"
DRILL_DIR="${DRILL_DIR:-$(mktemp -d -t aw24-drill)}"
HERE="$(cd "$(dirname "$0")" && pwd)"
SEED_CORPUS="$HERE/../../seed/corpus"
VICTIM="POL-2001"           # critical rule from universal-donts.md — the "bricks every run" shape
DELETED_DOC="cross-platform.md"  # mints PAT-1901..PAT-1904

# The emit seam must NEVER touch the operator's real outbox from a drill: spool the
# `wicked.estate.rule.retired` events into the drill dir (they double as evidence).
export WICKED_APPS_EMIT_DEADLETTER="$DRILL_DIR/emit-outbox.ndjson"
unset WICKED_ESTATE_DB 2>/dev/null || true

GOV="$DRILL_DIR/gov.db"              # enforcement lane — what the gate hook reads
GRAPH="$DRILL_DIR/repo-graph.db"     # discovery lane   — what the worker's estate MCP binds
KNOW="$DRILL_DIR/knowledge.db"       # knowledge lane   — what guidance recall reads
MANIFEST="$DRILL_DIR/fanout-manifest.json"

banner() { printf '\n=== %s ===\n' "$*"; }

banner "0. versions"
"$WICKED_CORE_BIN" --version || true
python3 - <<'PY'
import json, os, subprocess
binary = os.environ.get("WICKED_ESTATE_MCP_BIN", "wicked-estate-mcp")
init = {"jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                   "clientInfo": {"name": "aw24-kill-switch-drill", "version": "0"}}}
out = subprocess.run([binary, "--db", ":memory:"], input=json.dumps(init) + "\n",
                     capture_output=True, text=True, timeout=30).stdout
for line in out.splitlines():
    try:
        msg = json.loads(line)
    except json.JSONDecodeError:
        continue
    if msg.get("id") == 1:
        info = msg["result"].get("serverInfo", {})
        print(f"{info.get('name', 'wicked-estate-mcp')} {info.get('version', '?')} ({binary})")
        break
PY
echo "drill dir: $DRILL_DIR"

banner "1. stage the ruleset (the shipped seed corpus, copied — the drill never edits the repo)"
mkdir -p "$DRILL_DIR/ruleset"
cp "$SEED_CORPUS"/*.md "$DRILL_DIR/ruleset/"
ls "$DRILL_DIR/ruleset"

banner "2. fan out across the deliberate store split (one manifest, keyed on PAT-/POL- ids)"
"$WICKED_CORE_BIN" rules fanout "$DRILL_DIR/ruleset" \
  --scope workspace \
  --enforcement-db "$GOV" \
  --discovery-db "$GRAPH" \
  --knowledge-db "$KNOW" \
  --knowledge-scope wiki:governance \
  --manifest "$MANIFEST"

banner "3. BEFORE — every lane serves $VICTIM as current doctrine"
echo "--- 3a. enforcement lane: the gate hook's recall funnel (wicked-core rules recall)"
"$WICKED_CORE_BIN" rules recall --db "$GOV" --json > "$DRILL_DIR/gate-recall-before.json"
python3 - "$DRILL_DIR/gate-recall-before.json" "$VICTIM" <<'PY'
import json, sys
report = json.load(open(sys.argv[1]))
ids = [r["id"] for r in report["rules"]]
assert sys.argv[2] in ids, f"precondition failed: {sys.argv[2]} not recalled: {ids}"
print(f"gate recall serves {report['count']} rules, including {sys.argv[2]} — OK")
PY

echo "--- 3b. discovery lane: the worker's estate MCP rules.recall"
python3 "$HERE/mcp-call.py" "$GRAPH" "$KNOW" rules.recall '{"severity":"critical"}' \
  > "$DRILL_DIR/rules-recall-before.json"
python3 - "$DRILL_DIR/rules-recall-before.json" "$VICTIM" <<'PY'
import json, sys
result = json.loads(open(sys.argv[1]).readline())  # line 1 = the tool result JSON
ids = [r["id"] for r in result["rules"]]
assert sys.argv[2] in ids, f"precondition failed: {sys.argv[2]} not in MCP rules.recall: {ids}"
print(f"estate MCP rules.recall serves {ids} including {sys.argv[2]} — OK")
PY

echo "--- 3c. knowledge lane: rationale served as current (no marker)"
python3 "$HERE/mcp-call.py" "$GRAPH" "$KNOW" knowledge.recall "{\"query\":\"$VICTIM\"}" \
  > "$DRILL_DIR/knowledge-recall-before.json"
python3 - "$DRILL_DIR/knowledge-recall-before.json" "$VICTIM" <<'PY'
import json, sys
text = open(sys.argv[1]).read()
assert sys.argv[2] in text, f"precondition failed: no rationale recalled for {sys.argv[2]}"
assert "[RETIRED" not in text, "rationale already carries the retirement marker"
print(f"knowledge.recall serves the {sys.argv[2]} rationale as current — OK")
PY

banner "4. PULL THE SWITCH — one manifest-keyed op: wicked-core rules retire --id $VICTIM"
"$WICKED_CORE_BIN" rules retire --id "$VICTIM" \
  --manifest "$MANIFEST" \
  --out "$DRILL_DIR/retire-receipt-${VICTIM}.json"
python3 - "$DRILL_DIR/retire-receipt-${VICTIM}.json" <<'PY'
import json, sys
r = json.load(open(sys.argv[1]))
assert r["all_cli_lanes_verified"] and r["pending"] == 0, r
lanes = r["retirements"][0]["lanes"]
assert {l["lane"] for l in lanes} == {"enforcement", "discovery", "knowledge"}, lanes
assert all(l["status"] == "retired" and l["verified"] for l in lanes), lanes
print(f"receipt: {len(lanes)} lanes, all retired + verified — OK")
PY

banner "5. AFTER — the next recall no longer serves it, in EVERY lane"
echo "--- 5a. enforcement lane"
"$WICKED_CORE_BIN" rules recall --db "$GOV" --json > "$DRILL_DIR/gate-recall-after.json"
python3 - "$DRILL_DIR/gate-recall-after.json" "$DRILL_DIR/gate-recall-before.json" "$VICTIM" <<'PY'
import json, sys
after = json.load(open(sys.argv[1])); before = json.load(open(sys.argv[2]))
ids = [r["id"] for r in after["rules"]]
assert sys.argv[3] not in ids, f"{sys.argv[3]} STILL served by the gate funnel: {ids}"
assert after["count"] == before["count"] - 1, "exactly the victim must disappear"
print(f"gate recall: {before['count']} -> {after['count']} rules; {sys.argv[3]} gone, siblings intact — OK")
PY

echo "--- 5b. discovery lane (estate MCP rules.recall — the AC's 'rules.recall showing it gone')"
python3 "$HERE/mcp-call.py" "$GRAPH" "$KNOW" rules.recall '{"severity":"critical"}' \
  > "$DRILL_DIR/rules-recall-after.json"
python3 - "$DRILL_DIR/rules-recall-after.json" "$VICTIM" <<'PY'
import json, sys
result = json.loads(open(sys.argv[1]).readline())
ids = [r["id"] for r in result["rules"]]
assert sys.argv[2] not in ids, f"{sys.argv[2]} STILL served by MCP rules.recall: {ids}"
print(f"estate MCP rules.recall no longer serves {sys.argv[2]} (ids: {ids}) — OK")
PY

echo "--- 5c. knowledge lane: rationale survives but is NON-NORMATIVE behind the marker"
python3 "$HERE/mcp-call.py" "$GRAPH" "$KNOW" knowledge.recall "{\"query\":\"$VICTIM\"}" \
  > "$DRILL_DIR/knowledge-recall-after.json"
python3 - "$DRILL_DIR/knowledge-recall-after.json" "$VICTIM" <<'PY'
import sys
text = open(sys.argv[1]).read()
assert "[RETIRED" in text and "non-normative" in text, f"marker missing: {text[:400]}"
assert sys.argv[2] in text, "the enforceable twin's id must survive the marking"
print(f"knowledge.recall serves the {sys.argv[2]} rationale behind the [RETIRED ...] marker — OK")
PY

echo "--- 5d. the propagation trail: wicked.estate.rule.retired per store that changed"
python3 - "$WICKED_APPS_EMIT_DEADLETTER" "$VICTIM" <<'PY'
import json, sys
events = [json.loads(l) for l in open(sys.argv[1])]
retired = [e for e in events if e.get("type") == "wicked.estate.rule.retired"
           and e.get("payload", {}).get("rule_id") == sys.argv[2]]
assert len(retired) == 2, f"expected 2 events (enforcement + discovery), got {len(retired)}"
print("2 wicked.estate.rule.retired events spooled (enforcement + discovery) — OK")
PY

banner "6. DELETED DOC → EXPLICIT RETIRE (never silent orphaning)"
echo "--- 6a. the wiki doc is deleted"
rm "$DRILL_DIR/ruleset/$DELETED_DOC"

echo "--- 6b. rules drift REPORTS the orphans (read-only, never drops)"
set +e
"$WICKED_CORE_BIN" rules drift --db "$GOV" --dir "$DRILL_DIR/ruleset" --json \
  > "$DRILL_DIR/drift-before-retire.json"
echo "(drift exit code: $? — 3 = residue found, as expected)"
set -e
python3 - "$DRILL_DIR/drift-before-retire.json" "$DELETED_DOC" <<'PY'
import json, sys
report = json.load(open(sys.argv[1]))
orphans = {o["rule_id"]: o["reason"] for o in report["orphaned"] if o["doc_path"] == sys.argv[2]}
assert orphans == {f"PAT-190{i}": "doc_missing" for i in (1, 2, 3, 4)}, orphans
print(f"drift reports the deleted doc's rules orphaned (doc_missing): {sorted(orphans)} — OK")
PY

echo "--- 6c. rules retire --doc turns the drift report into the explicit retire set"
"$WICKED_CORE_BIN" rules retire --doc "$DELETED_DOC" \
  --manifest "$MANIFEST" \
  --out "$DRILL_DIR/retire-receipt-deleted-doc.json"
python3 - "$DRILL_DIR/retire-receipt-deleted-doc.json" <<'PY'
import json, sys
r = json.load(open(sys.argv[1]))
assert r["all_cli_lanes_verified"] and r["pending"] == 0, r
assert sorted(r["requested"]) == [f"PAT-190{i}" for i in (1, 2, 3, 4)], r["requested"]
print(f"receipt: all 4 doc-derived rules retired + verified in every lane — OK")
PY

echo "--- 6d. drift again: retirement IS the healed state (orphans cleared)"
set +e
"$WICKED_CORE_BIN" rules drift --db "$GOV" --dir "$DRILL_DIR/ruleset" --json \
  > "$DRILL_DIR/drift-after-retire.json"
echo "(drift exit code: $? — remaining residue is unresolvable symbol_refs only: the drill"
echo " store indexes no code, so engine-contract.md refs cannot resolve — unrelated to AW-24)"
set -e
python3 - "$DRILL_DIR/drift-after-retire.json" <<'PY'
import json, sys
report = json.load(open(sys.argv[1]))
assert report["orphaned"] == [], f"orphans must be cleared: {report['orphaned']}"
assert report["uningested"] == [], report["uningested"]
print(f"drift: 0 orphaned, 0 uningested (skipped_retired={report['skipped_retired']}) — OK")
PY

banner "7. drill artifacts"
ls "$DRILL_DIR"
if [ -n "${EVIDENCE_DIR:-}" ]; then
  cp "$MANIFEST" \
     "$DRILL_DIR/retire-receipt-${VICTIM}.json" \
     "$DRILL_DIR/retire-receipt-deleted-doc.json" \
     "$DRILL_DIR/gate-recall-before.json" "$DRILL_DIR/gate-recall-after.json" \
     "$DRILL_DIR/rules-recall-before.json" "$DRILL_DIR/rules-recall-after.json" \
     "$DRILL_DIR/knowledge-recall-after.json" \
     "$DRILL_DIR/drift-before-retire.json" "$DRILL_DIR/drift-after-retire.json" \
     "$DRILL_DIR/emit-outbox.ndjson" \
     "$EVIDENCE_DIR/"
  echo "artifacts copied to $EVIDENCE_DIR"
fi

banner "DRILL PASSED — kill switch propagates all lanes in one op; recall no longer serves the id"
