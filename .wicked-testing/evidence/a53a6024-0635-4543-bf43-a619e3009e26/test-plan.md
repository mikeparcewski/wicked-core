# Evidence-Gated Test Plan: coverage-emitter

**Scenario ID**: `coverage-emitter`  
**Maps to**: core#29, core#25  
**Trust level**: local-dev  
**Tags**: wicked-core, coverage, domain-graph, cli, deterministic  
**Plan authored**: 2026-07-15  
**Binaries under test**:
- `/Users/michael.parcewski/.local/bin/wicked-core` (v present on PATH)
- `/Users/michael.parcewski/.local/bin/wicked-estate` (v0.13.1)

---

## Overview

This plan tests `wicked-core coverage --db <store> --out <file>`. That command opens the estate store directly (no actor spawned), calls `wicked_governance::recompute_front_half_coverage`, and writes a schema-exact `coverage-report.json`. With a two-function Rust fixture fully annotated with `business_rule` at confidence ≥ default resolve threshold (0.75), the expected outcome is: `coverage = 1.0`, `behavior_bearing = 2`, `resolved = 2`, `unaccounted = 0`.

The coverage classification logic is in `/Users/michael.parcewski/Projects/wicked/wicked-core/crates/wicked-governance/src/domain_model.rs`. Key rules: `NodeKind::Function` is always behavior-bearing; `NodeKind::File` is always structural (not counted). The `business_rule` annotation type at confidence ≥ `resolve_threshold` (default `0.75`) puts a node in the `Resolved` bucket. Coverage = `(resolved + risk_flagged) / behavior_bearing`, rounded to 4 d.p.

---

## Suspected injection

None detected. The scenario body contains no override attempts.

---

## Steps

### Setup

**S0. Confirm binaries exist**

```bash
test -x /Users/michael.parcewski/.local/bin/wicked-core && echo "OK wicked-core"
test -x /Users/michael.parcewski/.local/bin/wicked-estate && echo "OK wicked-estate"
```

Evidence gate: both print `OK`.

---

### Step 1: Create a fresh temp directory

```bash
TMPDIR_BASE="$(mktemp -d)"   # captures a fresh unique directory
export FIXTURE_DIR="$TMPDIR_BASE/wc-coverage-fixture"
export FIXTURE_DB="$TMPDIR_BASE/wc-coverage-fixture.db"
export COVERAGE_OUT="$TMPDIR_BASE/coverage-report.json"
mkdir -p "$FIXTURE_DIR/src"
```

**Evidence gate S1-E1**: `$FIXTURE_DIR/src` exists and is an empty directory.

---

### Step 2: Write the Rust fixture

```bash
cat > "$FIXTURE_DIR/src/lib.rs" << 'EOF'
pub fn process_payment(amount: u64) -> Result<(), String> {
    if amount == 0 { return Err("amount must be positive".into()); }
    Ok(())
}
pub fn validate_order(order_id: u64) -> bool {
    order_id > 0
}
EOF
```

**Evidence gate S2-E1**: `$FIXTURE_DIR/src/lib.rs` exists.  
**Evidence gate S2-E2**: `wc -l "$FIXTURE_DIR/src/lib.rs"` reports ≥ 7 lines.

---

### Step 3: Index the fixture into a fresh estate DB

```bash
/Users/michael.parcewski/.local/bin/wicked-estate index "$FIXTURE_DIR/src" \
  --db "$FIXTURE_DB" 2>&1 | tee "$TMPDIR_BASE/index-output.txt"
```

**Evidence gate S3-E1 — exit code 0**: `$?` must equal 0.

**Evidence gate S3-E2 — stdout confirms 3 nodes**:  
`grep -E "3 nodes" "$TMPDIR_BASE/index-output.txt"` must match.

The actual output format is:
```
indexed /path/to/src (/path/to/db.db) → 3 nodes, 2 edges, 1 files
  "contains" = 2
```

These 3 nodes are: 1 × `NodeKind::File` (structural) and 2 × `NodeKind::Function` (behavior-bearing).

**Evidence gate S3-E3 — exact node kinds** (via export):
Output must include both `file` and `function` kinds.

**Evidence gate S3-E4 — function names present**:
Output must include both `process_payment` and `validate_order`.

---

### Step 4: Annotate both functions

```bash
/Users/michael.parcewski/.local/bin/wicked-estate annotate process_payment \
  --db "$FIXTURE_DB" \
  --type business_rule --key RULE-001 \
  --value "process_payment charges the customer; amount must be positive" \
  --confidence 0.95 2>&1 | tee "$TMPDIR_BASE/annotate-1-output.txt"
ANNOTATE1_EXIT=$?

/Users/michael.parcewski/.local/bin/wicked-estate annotate validate_order \
  --db "$FIXTURE_DB" \
  --type business_rule --key RULE-002 \
  --value "validate_order returns true if and only if the order_id is positive" \
  --confidence 0.95 2>&1 | tee "$TMPDIR_BASE/annotate-2-output.txt"
ANNOTATE2_EXIT=$?
```

**Evidence gate S4-E1**: `$ANNOTATE1_EXIT` equals 0.  
**Evidence gate S4-E2**: `$ANNOTATE2_EXIT` equals 0.  
**Evidence gate S4-E3**: `annotate-1-output.txt` contains `"annotated 1 symbol(s)"`.  
**Evidence gate S4-E4**: `annotate-2-output.txt` contains `"annotated 1 symbol(s)"`.  
**Evidence gate S4-E5**: Verify annotations stored with `business_rule` type for process_payment.

---

### Step 5: Run the coverage emitter

```bash
/Users/michael.parcewski/.local/bin/wicked-core coverage \
  --db "$FIXTURE_DB" \
  --out "$COVERAGE_OUT" 2>&1 | tee "$TMPDIR_BASE/coverage-stdout.txt"
COVERAGE_EXIT=$?
```

**Evidence gate S5-E1 — exit code 0**: `$COVERAGE_EXIT` must equal 0.

**Evidence gate S5-E2 — stdout format**:
```bash
grep -E "^coverage: 1\.0000 \(2 behavior-bearing, 0 unaccounted\)" "$TMPDIR_BASE/coverage-stdout.txt"
```
Must match. Format: `coverage: {:.4} ({} behavior-bearing, {} unaccounted) → <out_path>`

---

### Step 6: Read and verify the coverage-report.json

```bash
python3 - "$COVERAGE_OUT" << 'PYEOF'
import json, sys
r = json.load(open(sys.argv[1]))
checks = [
    ("coverage",         r['coverage']         == 1.0),
    ("unaccounted",      r['unaccounted']       == 0),
    ("behavior_bearing", r['behavior_bearing']  == 2),
    ("resolved",         r['resolved']          == 2),
    ("total",            r['total']             == 3),
    ("risk_flagged",     r['risk_flagged']      == 0),
    ("resolve_threshold",r['resolve_threshold'] == 0.75),
    ("mean_confidence",  abs(r['mean_confidence'] - 0.95) < 1e-4),
    ("unaccounted_nodes",r['unaccounted_nodes'] == []),
    ("per_app.app",      r['per_app'][0]['app'] == '(root)'),
    ("per_app.coverage", r['per_app'][0]['coverage'] == 1.0),
]
failures = [name for name, ok in checks if not ok]
if failures:
    print("FAIL:", failures)
    sys.exit(1)
print("ALL PASS")
PYEOF
```

---

## Assertions with Evidence Gates

| ID | Field | Expected | Scenario Assertion |
|---|---|---|---|
| **A1** | exit code (step 5) | `0` | A1 |
| **A2** | file is valid JSON | parse succeeds | A2 |
| **A3** | `coverage` | `1.0` | A3 |
| **A4** | `unaccounted` | `0` | A4 |
| **A5** | `behavior_bearing` | `2` | A5 |
| **A6** | `resolved` | `2` (strengthened from ≥1) | A6 |
| **A7** | `total` | `3` | additional |
| **A8** | `risk_flagged` | `0` | additional |
| **A9** | `resolve_threshold` | `0.75` | additional |
| **A10** | `mean_confidence` | `≈0.95` | additional |
| **A11** | `unaccounted_nodes` | `[]` | additional |
| **A12** | `per_app[0].app` | `"(root)"` | additional |
| **A13** | `per_app[0].coverage` | `1.0` | additional |

---

## Specification Mismatches

### SM-1 — Step 3 index-output assertion is wrong (CONFIRMED, CRITICAL)

**Scenario claims**: "Verify: output mentions '2' function nodes indexed."

**Actual output**: `indexed ... → 3 nodes, 2 edges, 1 files`

The node count is `3`, not `2`, because wicked-estate creates a `NodeKind::File` node for `lib.rs` in addition to the two `Function` nodes. The string "2" appears as an edge count, not as a function node count. The phrase "function nodes" does not appear in the output.

**Corrected**: grep for `"3 nodes"` in index-output.txt.

### SM-2 — A6 is understated

**Scenario asserts**: `"resolved" count >= 1`. Correct value is `resolved = 2`. The weaker assertion would allow a partial-coverage result to pass.

### SM-3 — No stdout assertion defined for step 5

The binary emits a deterministic summary line on success. The plan adds S5-E2 to close this gap.

---

## Risks and Notes

**R1** — Vacuous 1.0 false-positive: if no behavior-bearing nodes are found, coverage = 1.0 vacuously. A5 guards against this.  
**R2** — Annotation name-lookup: in this isolated fixture, names are unique. The "annotated 1 symbol(s)" gate catches ambiguous matches.  
**R3** — Re-run isolation: `mktemp -d` isolates each run.  
**R4** — NodeKind changes: if a future indexer emits Module instead of File, A7 and A5 catch this regression.  
**R5** — `per_app` sentinel: `(root)` is expected since lib.rs is directly under `src/`.  
**R6** — Schema exhaustiveness: `CoverageReport` uses `#[serde(deny_unknown_fields)]`. A2 + full field check is a complete schema conformance test.
