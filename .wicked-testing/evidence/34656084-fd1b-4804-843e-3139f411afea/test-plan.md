## Test Plan: `domain-graph-happy-path`

### Metadata

| Field | Value |
|---|---|
| Scenario ID | `domain-graph-happy-path` |
| Title | wicked-core domain-graph emits requirements_graph.json when coverage is 1.0 |
| Scenario path | `.wicked-testing/scenarios/domain-graph-happy-path.md` |
| Maps to | core#29, core#22 |
| Trust level | local-dev |
| Tags | wicked-core, domain-graph, cli, deterministic |
| Status | active |
| Plan authored | 2026-07-15 |
| Implementation refs | `src/bin/wicked-core.rs` lines 586–700 (`domain_graph_cmd`); `crates/wicked-governance/src/domain_model.rs` (`build_domain_model`, `recompute_front_half_coverage`, `assert_front_half_coverage`) |

---

### Suspected Injection

None detected. The scenario body contains only a domain-graph test description. No phrases such as "ignore previous instructions", "just return PASS", or `IGNORE-ABOVE` were found.

---

### Specification Notes

**SN-1: Coverage gate is integer-exact, not float-based.**
`assert_front_half_coverage` checks `report.unaccounted != 0`, not `coverage float == 1.0`. On a large graph a single hole can round to `coverage == 1.0000` yet still carry `unaccounted == 1`. For this two-node fixture the distinction is not exercised (0 unaccounted nodes → coverage 1.0 both ways), but the scenario title's "coverage is 1.0" is technically the rounded float, not the definitive gate condition.

**SN-2: `build_domain_model` applies a defense-in-depth double store recompute.**
`build_domain_model` first calls `assert_front_half_coverage(coverage)` on the passed `CoverageReport` argument, then calls `recompute_front_half_coverage(store)` a second time internally and calls `assert_front_half_coverage` on that result too. Both must pass. The store IS the source of truth.

**SN-3: `domain_graph_cmd` uses `--schema-version` with default `"1.0.0"`.**
The scenario command does not pass `--schema-version`. The implementation defaults to `"1.0.0"`. A3 is satisfiable without the flag.

**SN-4: `--coverage` file is optional; its absence is not an error.**
The scenario omits `--coverage`. The implementation treats the absence of `--coverage` as "no cross-check requested". Absent is a valid and expected invocation.

**SN-5: Stdout message format is prefixed.**
The implementation emits: `"domain-graph: wrote N domain(s) → <out_path>"`. The scenario's A7 asserts the substring `"wrote 1 domain(s)"`. The substring IS present in the actual message, so A7 will pass as a substring match.

**SN-6: `domains` serializes as a JSON object.**
`DomainModel.domains` is a `BTreeMap<String, Domain>` serialized by Serde as a JSON object.

**SN-7: Default output path when `--out` is omitted.**
Without `--out`, defaults to `.wicked-estate/requirements/requirements_graph.json`. The scenario always supplies `--out "$TMPDIR/requirements_graph.json"`.

**SN-8: `migration_mode` is hardcoded to `"functional"`.**
`build_domain_model` always writes `migration_mode: "functional"`. The scenario does not assert on this field.

---

### Specification Mismatches

**MISMATCH-1: File path format stored by `wicked-estate index` is unspecified by the scenario.**
The domain grouping (`package_dir` function) extracts the parent directory component of `node.location.file`. If `wicked-estate index` stores the absolute path, then `package_dir` returns the absolute parent directory. Both functions still fall into the SAME parent directory, so domain count remains 1 and A7 ("wrote 1 domain(s)") holds.

**MISMATCH-2: A6 uses open lower bound `(0.0, 1.0]` but the implementation accepts `[0.0, 1.0]`.**
For this fixture the annotated confidence is `0.95`, so there is no practical conflict.

**MISMATCH-3: A7 says "stdout/stderr" but the positive confirmation message goes to stdout only.**
The executor should capture stdout specifically.

**MISMATCH-4: `wicked-estate annotate` interface is an external dependency not verified by this codebase.**
If the actual `wicked-estate annotate` interface uses different flag names, steps 1–4 will fail at the executor level rather than at an assertion level.

---

### Prerequisites

1. `wicked-core` binary is on PATH. Verify: `wicked-core --help` prints usage.
2. `wicked-estate` binary is on PATH. Verify: `wicked-estate --help` prints usage.
3. `$TMPDIR` is set and writable. If unset, substitute an explicit writable path.
4. No stale `$TMPDIR/wc-domgraph-fixture/` or `$TMPDIR/wc-domgraph-fixture.db` from a prior run.

---

### Test Steps

#### Step 0 — Cleanup prior state

```bash
rm -rf "$TMPDIR/wc-domgraph-fixture"
rm -f  "$TMPDIR/wc-domgraph-fixture.db"
rm -f  "$TMPDIR/requirements_graph.json"
```

**Evidence required:** None. Precondition step only.

---

#### Step 1 — Create the fixture source directory and Rust file

```bash
mkdir -p "$TMPDIR/wc-domgraph-fixture/src"
cat > "$TMPDIR/wc-domgraph-fixture/src/lib.rs" << 'EOF'
pub fn process_payment(amount: u64) -> Result<(), String> {
    if amount == 0 { return Err("amount must be positive".into()); }
    Ok(())
}
pub fn validate_order(order_id: u64) -> bool {
    order_id > 0
}
EOF
grep -c "^pub fn " "$TMPDIR/wc-domgraph-fixture/src/lib.rs"
```

**Evidence required:** File contents verbatim, function count (expected: 2).

**Assertions:**
- `step-1-fn-count` EQUALS 2

---

#### Step 2 — Index the fixture into a fresh estate store

```bash
wicked-estate index "$TMPDIR/wc-domgraph-fixture/src" \
  --db "$TMPDIR/wc-domgraph-fixture.db" \
  > "$EVIDENCE_DIR/index-output.txt" 2>&1
echo "index_exit=$?"
```

**Evidence required:**
- `index-output.txt` — full stdout/stderr from indexer (verbatim)
- Exit code recorded

**Assertions:**
- `index-exit-code` EQUALS 0
- `$TMPDIR/wc-domgraph-fixture.db` EXISTS

---

#### Step 3 — Annotate `process_payment` with RULE-001

```bash
wicked-estate annotate process_payment \
  --db "$TMPDIR/wc-domgraph-fixture.db" \
  --type business_rule \
  --key RULE-001 \
  --value "process_payment charges the customer; amount must be positive" \
  --confidence 0.95 \
  > "$EVIDENCE_DIR/annotate-1-output.txt" 2>&1
echo "annotate1_exit=$?"
```

**Evidence required:**
- `annotate-1-output.txt` — full output verbatim
- Exit code recorded

**Assertions:**
- `annotate-1-exit-code` EQUALS 0

---

#### Step 4 — Annotate `validate_order` with RULE-002

```bash
wicked-estate annotate validate_order \
  --db "$TMPDIR/wc-domgraph-fixture.db" \
  --type business_rule \
  --key RULE-002 \
  --value "validate_order returns true if and only if the order_id is positive" \
  --confidence 0.95 \
  > "$EVIDENCE_DIR/annotate-2-output.txt" 2>&1
echo "annotate2_exit=$?"
```

**Evidence required:**
- `annotate-2-output.txt` — full output verbatim
- Exit code recorded

**Assertions:**
- `annotate-2-exit-code` EQUALS 0

---

#### Step 5 — Run `wicked-core domain-graph` and capture output

```bash
wicked-core domain-graph \
  --db "$TMPDIR/wc-domgraph-fixture.db" \
  --out "$TMPDIR/requirements_graph.json" \
  > "$EVIDENCE_DIR/domgraph-stdout.txt" 2>&1
DOMAIN_GRAPH_EXIT=$?
echo "domain_graph_exit=$DOMAIN_GRAPH_EXIT"
```

**Evidence required:**
- `domgraph-stdout.txt` — full stdout/stderr verbatim
- Exit code recorded

**Assertions (A1, A7):**
- `domain-graph-exit-code` EQUALS 0  [A1]
- `domgraph-stdout.txt` CONTAINS "wrote 1 domain(s)"  [A7]

---

#### Step 6 — Verify file existence and JSON validity (A2)

```bash
test -f "$TMPDIR/requirements_graph.json" && echo "file_exists=true" || echo "file_exists=false"
python3 -c "
import json, sys
with open('$TMPDIR/requirements_graph.json') as f:
    d = json.load(f)
print('json_valid=true')
print('top_level_keys=' + str(list(d.keys())))
" 2>&1
cp "$TMPDIR/requirements_graph.json" "$EVIDENCE_DIR/requirements_graph.json"
```

**Evidence required:**
- `requirements_graph.json` — full content verbatim (copied to evidence dir)
- `file_exists` status
- `json_valid` status

**Assertions (A2):**
- `file-exists` EQUALS true
- `json-valid` EQUALS true

---

#### Step 7 — Verify `metadata.schema_version` (A3)

```bash
python3 -c "
import json, sys
with open('$TMPDIR/requirements_graph.json') as f:
    d = json.load(f)
sv = d.get('metadata', {}).get('schema_version')
print('schema_version=' + repr(sv))
sys.exit(0 if sv == '1.0.0' else 1)
"
echo "a3_exit=$?"
```

**Assertions (A3):**
- `schema_version` EQUALS "1.0.0"

---

#### Step 8 — Verify `domains` key is non-empty (A4)

```bash
python3 -c "
import json, sys
with open('$TMPDIR/requirements_graph.json') as f:
    d = json.load(f)
domains = d.get('domains', {})
print('domains_type=' + type(domains).__name__)
print('domains_count=' + str(len(domains)))
print('domain_keys=' + str(list(domains.keys())))
sys.exit(0 if isinstance(domains, dict) and len(domains) >= 1 else 1)
"
echo "a4_exit=$?"
```

**Assertions (A4):**
- `domains-type` EQUALS dict
- `domains-count` COUNT_GTE 1

---

#### Step 9 — Verify at least one domain has a requirement with non-empty `business_rules` (A5)

```bash
python3 -c "
import json, sys
with open('$TMPDIR/requirements_graph.json') as f:
    d = json.load(f)
found = False
for dn, dom in d.get('domains', {}).items():
    for rn, req in dom.get('requirements', {}).items():
        if len(req.get('business_rules', [])) > 0:
            print('found_domain=' + dn)
            print('found_req=' + rn)
            print('rules_count=' + str(len(req['business_rules'])))
            found = True
            break
    if found:
        break
sys.exit(0 if found else 1)
"
echo "a5_exit=$?"
```

**Assertions (A5):**
- `a5-exit-code` EQUALS 0

---

#### Step 10 — Verify all business rules have confidence in `(0.0, 1.0]` and non-empty statement (A6)

```bash
python3 -c "
import json, sys
with open('$TMPDIR/requirements_graph.json') as f:
    d = json.load(f)
failures = []
for dn, dom in d.get('domains', {}).items():
    for rn, req in dom.get('requirements', {}).items():
        for i, rule in enumerate(req.get('business_rules', [])):
            loc = f'{dn}/{rn}/rule[{i}]'
            c = rule.get('confidence')
            s = rule.get('statement', '')
            if not isinstance(c, (int, float)) or not (0.0 < c <= 1.0):
                failures.append(f'FAIL confidence {c!r} at {loc}')
            if not s.strip():
                failures.append(f'FAIL empty statement at {loc}')
            else:
                print(f'rule_ok: {loc}: confidence={c}')
if failures:
    for f in failures:
        print(f)
    sys.exit(1)
print('a6_pass=true')
"
echo "a6_exit=$?"
```

**Assertions (A6):**
- `a6-exit-code` EQUALS 0
- All confidence values in range `(0.0, 1.0]`
- All statements non-empty

---

### Acceptance Criteria Map

| Assertion | Step | Check | Evidence source |
|---|---|---|---|
| A1: exit code 0 | 5 | `domain-graph-exit-code EQUALS 0` | Shell `$?` in evidence |
| A2: file exists, valid JSON | 6 | File present; `json.load()` succeeds | `requirements_graph.json` (verbatim) |
| A3: `metadata.schema_version == "1.0.0"` | 7 | Exact string match | `requirements_graph.json` |
| A4: non-empty `domains` key | 8 | `len(domains) >= 1` | `requirements_graph.json` |
| A5: at least one requirement with non-empty `business_rules` | 9 | Any `len(business_rules) >= 1` | `requirements_graph.json` |
| A6: confidence `(0.0, 1.0]`, non-empty statement | 10 | Range check + non-empty strip | `requirements_graph.json` |
| A7: stdout contains confirmation | 5 | CONTAINS "wrote 1 domain(s)" | `domgraph-stdout.txt` |

---

### Evidence Manifest

| Artifact | Path | Role |
|---|---|---|
| Index output | `index-output.txt` | Confirms estate indexing succeeded |
| First annotation output | `annotate-1-output.txt` | Confirms `process_payment` annotation accepted |
| Second annotation output | `annotate-2-output.txt` | Confirms `validate_order` annotation accepted |
| domain-graph stdout/stderr | `domgraph-stdout.txt` | Primary process evidence; satisfies A1 + A7 |
| Domain model output | `requirements_graph.json` | Primary artifact; satisfies A2 + A3 + A4 + A5 + A6 |

---

### Flagged Items for Evaluator Attention

1. **MISMATCH-1 (file path grouping):** The actual domain key name in `requirements_graph.json` depends on how `wicked-estate index` stores `location.file`. If it stores absolute paths, the domain key will be an absolute path prefix, not the bare `"src"`. A7 asserts domain-count-only so it still passes; but the evaluator should inspect the actual domain name in the evidence.

2. **MISMATCH-4 (wicked-estate CLI interface):** Steps 3–4 depend on the `wicked-estate annotate` CLI surface. If flag names differ, the executor must adapt them to match the actual CLI before the test can proceed.

3. **Implementation note on `status` field:** Both requirements in the output will carry `status: "review"` rather than `status: "active"` because `wicked-estate annotate` does not call `set_node_semantics`. A5 asserts only on the presence of business rules, not on status, so this does not cause an assertion failure.

4. **Double store recompute (SN-2):** If Step 5 fails unexpectedly, the most likely cause is that `recompute_front_half_coverage` inside `build_domain_model` returns `unaccounted > 0` — meaning the annotations in Steps 3–4 did not register under the expected symbol names.
