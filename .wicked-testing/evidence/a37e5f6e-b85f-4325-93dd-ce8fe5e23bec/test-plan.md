# Test Plan: wicked-core domain-graph refuses to build when coverage < 1.0 (unaccounted nodes)

## Scenario Summary

`wicked-core domain-graph` is fail-closed: when the estate store contains behavior-bearing nodes with no `business_rule` or `risk` annotation, the command must exit non-zero, emit the unaccounted count to stderr, and write no output file. This plan verifies the gate fires deterministically against a minimal two-function Rust fixture where both functions remain unannotated.

---

## Suspected Injection

None detected. The scenario body contains only a domain-graph test description. No phrases such as "ignore previous instructions", "just return PASS", or `IGNORE-ABOVE` were found.

---

## Implementation Notes

**Sources examined**:
- `src/bin/wicked-core.rs` — `domain_graph_cmd` (lines 586–700), `coverage_cmd` (lines 702–741), `fail()` helper (line 934).
- `crates/wicked-governance/src/domain_model.rs` — `assert_front_half_coverage`, `build_domain_model`, `recompute_front_half_coverage`.

**Verified by live test run against the actual fixture** (both binaries confirmed on PATH at `~/.local/bin/`).

**Exact error message produced by the implementation** (stderr, live-verified):
```
domain-graph: front-half coverage 0.0000 — 2 behavior-bearing node(s) unaccounted; run extraction + coverage first (refusing to translate an unannotated graph). First unaccounted: ["ts-rust . . . lib/process_payment().", "ts-rust . . . lib/validate_order()."]
```

**Exact coverage command stdout** (live-verified):
```
coverage: 0.0000 (2 behavior-bearing, 2 unaccounted) → <full_path_to_coverage_report>
```

**Key behavioral facts**:
- `wicked-estate index` on the two-function Rust fixture produces 3 nodes (2 `Function` + 1 dead-shell `Module`) and 2 `Contains` edges. The Module has no outgoing behavior edges so it is NOT behavior-bearing; `behavior_bearing = 2`.
- `fail()` always calls `eprintln!` (stderr) then `std::process::exit(1)`. Stdout is empty on the fail path.
- `domain_graph_cmd` performs two coverage checks: once on the pre-computed `CoverageReport` passed into `build_domain_model`, and once on a fresh store recompute inside `build_domain_model` (defense-in-depth). Both must pass; both will fail on this fixture.
- The gate key is the EXACT INTEGER `unaccounted != 0`, not the rounded float `coverage < 1.0`. On a large graph a single hole can round to `coverage == 1.0000` yet still carry `unaccounted == 1`. For this two-function fixture the result is `coverage = 0.0000`, making the distinction moot.
- `coverage_cmd` writes a JSON file (default `coverage-report.json` in cwd, overridable with `--out`). The stdout summary line is separate from the file.

---

## Steps with Evidence Requirements

### Step 1: Create fixture and index into an isolated DB

**Action**:
```bash
mkdir -p "$TMPDIR/wc-uncovered-fixture/src"
cat > "$TMPDIR/wc-uncovered-fixture/src/lib.rs" << 'EOF'
pub fn process_payment(amount: u64) -> Result<(), String> {
    if amount == 0 { return Err("amount must be positive".into()); }
    Ok(())
}
pub fn validate_order(order_id: u64) -> bool {
    order_id > 0
}
EOF
wicked-estate index "$TMPDIR/wc-uncovered-fixture/src" \
  --db "$TMPDIR/wc-uncovered-fixture.db" 2>&1 | tee "$EVIDENCE_DIR/index-output.txt"
```
Do NOT annotate either function. Neither function receives a `business_rule` or `risk` annotation.

**Evidence file**: `index-output.txt`

**Evidence gate**: File must contain a line mentioning the indexed path and exactly `2` function nodes (e.g., `2 nodes` or `3 nodes` with the context that 2 are `Function` kind). The db file must exist at `$TMPDIR/wc-uncovered-fixture.db`. Indexer must exit 0.

---

### Step 2: Confirm coverage is below 1.0 (pre-condition verification)

**Action**:
```bash
wicked-core coverage \
  --db "$TMPDIR/wc-uncovered-fixture.db" \
  --out "$TMPDIR/coverage-report.json" \
  2>&1 | tee "$EVIDENCE_DIR/coverage-check.txt"
```

**Evidence file**: `coverage-check.txt`

**Evidence gate**: File must contain the substring `coverage: 0.0000` and the substring `2 unaccounted`. The command must exit 0.

---

### Step 3: Attempt domain-graph build (expect failure)

**Action**:
```bash
wicked-core domain-graph \
  --db "$TMPDIR/wc-uncovered-fixture.db" \
  --out "$TMPDIR/requirements_graph_should_not_exist.json" \
  2>&1 | tee "$EVIDENCE_DIR/domgraph-fail-stdout.txt"
echo "exit_code=$?" >> "$EVIDENCE_DIR/domgraph-fail-stdout.txt"
```
Note: `2>&1` is required because the error output is written to **stderr** via `eprintln!`. Without it, `domgraph-fail-stdout.txt` would be empty and all assertions on its content would trivially fail.

**Evidence file**: `domgraph-fail-stdout.txt`

**Evidence gate**: File must contain `unaccounted` and the number `2`, and must NOT contain the word `wrote`. Appended `exit_code=` line must show a non-zero value.

---

### Step 4: Confirm output file was not created

**Action**:
```bash
ls -la "$TMPDIR/requirements_graph_should_not_exist.json" 2>&1 \
  | tee "$EVIDENCE_DIR/ls-output.txt"
```

**Evidence file**: `ls-output.txt`

**Evidence gate**: File must contain the phrase `No such file or directory` (or equivalent "not found" message on the executor's OS). The file path must NOT appear as a normal ls entry.

---

## Assertions with Evidence Mapping

| # | Assertion | Evidence File | What to Check | Pass Condition |
|---|-----------|--------------|---------------|----------------|
| A1 | Non-zero exit code | `domgraph-fail-stdout.txt` | Appended `exit_code=` line | Must NOT be `exit_code=0`; must be `exit_code=1` |
| A2 | Output file absent | `ls-output.txt` | ls output text | Must contain `No such file or directory`; must NOT show the file as a directory entry |
| A3a | stderr contains "unaccounted" | `domgraph-fail-stdout.txt` | Combined output text | Must contain the substring `unaccounted` |
| A3b | stderr contains "2" (the unaccounted count) | `domgraph-fail-stdout.txt` | Combined output text | Must contain the substring `2` adjacent to or within the unaccounted count context |
| A3c | stderr contains "coverage" | `domgraph-fail-stdout.txt` | Combined output text | Must contain the substring `coverage` |
| A4 | stderr does NOT contain "wrote" | `domgraph-fail-stdout.txt` | Combined output text | Must NOT contain the substring `wrote` (no partial write occurred) |
| A5 | Pre-condition: coverage is 0.0000 | `coverage-check.txt` | stdout line | Must contain `coverage: 0.0000` |
| A6 | Pre-condition: 2 behavior-bearing unaccounted nodes | `coverage-check.txt` | stdout line | Must contain `2 behavior-bearing` and `2 unaccounted` |

---

## Specification Notes

**SN-1: Step 2 expected output in scenario is truncated.**
The scenario writes: `Expect: coverage: 0.0000 (2 behavior-bearing, 2 unaccounted)`. The actual stdout format (confirmed live) is `coverage: 0.0000 (2 behavior-bearing, 2 unaccounted) → <full_path>`. A5/A6 use substring match, so this is satisfied. However, the scenario text is misleading — it implies the line ends after `unaccounted` when it does not. Assertions A5 and A6 are written as substring checks to tolerate the path suffix.

**SN-2: Error output is on stderr, not stdout.**
The scenario names the evidence file `domgraph-fail-stdout.txt` but the implementation's `fail()` helper uses `eprintln!` (stderr-only). Stdout from `domain_graph_cmd` is completely empty on the fail path. The evidence capture command MUST redirect stderr (`2>&1`) to populate this file. Without that, every content-based assertion (A1 content check, A3a, A3b, A3c, A4) would trivially fail on an empty file. This is a naming mismatch in the scenario (the file is called "stdout" but must contain stderr).

**SN-3: `wicked-core coverage` also writes a JSON file as a side effect.**
The scenario does not mention this. To avoid polluting the working directory, the test plan supplies `--out "$TMPDIR/coverage-report.json"`. If `--out` is omitted, the file is written to `./coverage-report.json` in the executor's cwd.

**SN-4: The coverage gate is integer-exact, not float-based.**
`assert_front_half_coverage` gates on `report.unaccounted != 0`, not `coverage < 1.0`. On a large graph a single-node hole can round to `coverage == 1.0000` but still fail the gate because `unaccounted == 1`. For this two-function fixture the distinction is not exercised (`coverage == 0.0000` is unambiguous). A3b specifically checks for the unaccounted *count* `2`, not just the float.

**SN-5: Double coverage recompute inside `build_domain_model`.**
The scenario describes the gate as if coverage is checked once. In the implementation, `domain_graph_cmd` first recomputes coverage from the store and passes the `CoverageReport` to `build_domain_model`. Inside `build_domain_model`, `assert_front_half_coverage(coverage)` is called on the passed report, AND then a *second* `recompute_front_half_coverage(store)` is run with another `assert_front_half_coverage` on the result (defense-in-depth per DES-OUTGOV-005 decision #4). The scenario is not incorrect, but it understates the gate depth. Both recomputes will produce `unaccounted = 2` for this fixture, so the first check triggers the bail.

**SN-6: `wicked-estate index` produces 3 nodes, not 2.**
The scenario does not state an expected node count, but the indexer produces 3 nodes: 2 `Function` nodes (`process_payment`, `validate_order`) and 1 dead-shell `Module` node (the `lib` module with only `Contains` edges). The Module is NOT behavior-bearing (it has no outgoing behavior-out edges), so `behavior_bearing = 2` as the scenario predicts. This is correct behavior but worth noting if the indexer output is inspected.

**SN-7: Exact error message format (for strong A3 check).**
The exact stderr line produced by the implementation (live-verified) is:
```
domain-graph: front-half coverage 0.0000 — 2 behavior-bearing node(s) unaccounted; run extraction + coverage first (refusing to translate an unannotated graph). First unaccounted: [...]
```
Assertions A3a–A3c check substrings; a stricter check would match the full prefix `domain-graph: front-half coverage 0.0000 — 2 behavior-bearing node(s) unaccounted`.
