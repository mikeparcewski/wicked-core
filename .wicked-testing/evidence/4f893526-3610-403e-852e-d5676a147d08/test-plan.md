# Test Plan: domain-graph-fail-closed

## Scenario summary

`wicked-core domain-graph` is fail-closed: when a store contains behavior-bearing function nodes with no `business_rule`, `risk`, or validated-requirement annotation, the command must exit non-zero, write no output file, and surface the unaccounted node count in stderr. The scenario exercises this by indexing a two-function Rust fixture with `wicked-estate index` (no annotations added), then invoking `wicked-core domain-graph --db ... --out ...` and asserting on exit code, absent output file, and error text.

---

## Prerequisites

- `wicked-core` binary is on `PATH` and resolves to the build under test.
- `wicked-estate` binary is on `PATH` and is able to index Rust source into the same SQLite store schema that `wicked-core` reads (`open_store` / `wicked_apps_core`).
- The `wicked-estate` Rust extractor must emit `NodeKind::Function` (not `NodeKind::Other`) for Rust `pub fn` items, because `recompute_front_half_coverage` classifies `Function` as behavior-bearing via an exhaustive match.
- `$TMPDIR` is set and writable (macOS default: `/var/folders/…`; set explicitly if running in CI).
- The store file (`$TMPDIR/wc-uncovered-fixture.db`) does not exist before the run, or is cleanly created during step 1.

---

## Steps

### Step 1: Index the unannotated fixture

**Action**: Create the fixture source tree and index it:

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
  --db "$TMPDIR/wc-uncovered-fixture.db"
```

Do NOT add any `business_rule`, `risk`, or requirement annotations. Both functions remain unaccounted.

**Evidence file**: `setup-index.txt`

**Evidence requirement**: The command exits 0 and the db file exists at `$TMPDIR/wc-uncovered-fixture.db`. Capture `ls -la "$TMPDIR/wc-uncovered-fixture.db"` into the evidence file. The file must be present and non-zero bytes.

**Assertions mapped**: Prerequisite for A3, A5

---

### Step 2: Verify coverage is 0.0 (optional cross-check)

**Action**:

```bash
wicked-core coverage \
  --db "$TMPDIR/wc-uncovered-fixture.db" \
  --out "$TMPDIR/coverage-check.txt" \
  > "$TMPDIR/coverage-cmd-stdout.txt" 2>&1
echo "exit=$?" >> "$TMPDIR/coverage-cmd-stdout.txt"
```

**Evidence file**: `coverage-cmd-stdout.txt`

**Evidence requirement**: The file must contain the line:

```
coverage: 0.0000 (2 behavior-bearing, 2 unaccounted) → <path>/coverage-check.txt
```

(The `→ <path>` suffix is always appended by `coverage_cmd`; the scenario's expected text omits it — see Specification notes §1.) The line must contain `0.0000`, `2 behavior-bearing`, and `2 unaccounted`. Exit code must be `0` (the coverage command itself succeeds even when coverage is below 1.0; it writes the report file and exits cleanly).

**Assertions mapped**: A5

---

### Step 3: Attempt to build the domain graph — expect failure

**Action**:

```bash
wicked-core domain-graph \
  --db "$TMPDIR/wc-uncovered-fixture.db" \
  --out "$TMPDIR/requirements_graph_should_not_exist.json" \
  > "$TMPDIR/domgraph-fail-output.txt" 2>&1
DOMGRAPH_EXIT=$?
echo "exit_code=$DOMGRAPH_EXIT" >> "$TMPDIR/domgraph-fail-output.txt"
```

**Evidence file**: `domgraph-fail-output.txt`

**Evidence requirement**: The file must contain all of the following:

1. `exit_code=1` (the `fail()` helper in `wicked-core.rs:934` calls `eprintln!` then `std::process::exit(1)`).
2. The substring `unaccounted` (from `assert_front_half_coverage`'s bail message: `"front-half coverage {:.4} — {} behavior-bearing node(s) unaccounted; …"`).
3. The substring `2` appearing in context with unaccounted count.
4. The substring `domain-graph:` (the prefix prepended by `fail(&format!("domain-graph: {e}"))`).
5. Must NOT contain the substring `wrote` (the success path in `domain_graph_cmd` prints `"domain-graph: wrote {} domain(s) → {out_path}"` only after a successful write — which must not occur here).

**Assertions mapped**: A1, A3, A4

---

### Step 4: Confirm the output file was not created

**Action**:

```bash
ls -la "$TMPDIR/requirements_graph_should_not_exist.json" 2>&1
```

**Evidence file**: `ls-output.txt`

**Evidence requirement**: The command must NOT produce a line showing a regular file. The output must contain `No such file or directory` (POSIX) or equivalent. An empty output (the file does not exist and `ls` produced no stdout) together with exit code non-zero is also acceptable.

**Assertions mapped**: A2

---

## Assertions

| ID | Criterion | Evidence file | Pass condition |
|----|-----------|---------------|----------------|
| A1 | `wicked-core domain-graph` exits non-zero (1 or 2) | `domgraph-fail-output.txt` | Line `exit_code=1` is present. Per `wicked-core.rs:934`, `fail()` always exits with code 1; code 2 is reserved for the CLI usage error path, which is not exercised here. Accept `1`; treat `2` as PLAUSIBLE but unexpected. |
| A2 | Output file `requirements_graph_should_not_exist.json` does NOT exist | `ls-output.txt` | `ls -la` output contains `No such file or directory`, confirming the file was never written. `domain_graph_cmd` only calls `std::fs::write` after `build_domain_model` returns `Ok`; when `assert_front_half_coverage` bails, control goes to `fail()` before any write. |
| A3 | Stderr from step 3 mentions "unaccounted" or "coverage" and the count 2 | `domgraph-fail-output.txt` | File contains substring `unaccounted` AND a digit `2` adjacent to the unaccounted count context. Actual message shape (from `domain_model.rs:537`): `"domain-graph: front-half coverage 0.0000 — 2 behavior-bearing node(s) unaccounted; run extraction + coverage first …"`. Both conditions are met. |
| A4 | Stderr does NOT contain "wrote" (no partial write) | `domgraph-fail-output.txt` | File does NOT contain the substring `wrote`. The only source of `wrote` in this subcommand is the success branch (`domain-graph: wrote N domain(s) → …`), which is unreachable when `build_domain_model` returns `Err`. |
| A5 | Step 2 (coverage check) confirms `coverage: 0.0000` before the fail-closed attempt | `coverage-cmd-stdout.txt` | File contains `0.0000` and `2 behavior-bearing` and `2 unaccounted`, confirming the store was populated correctly with two behavior-bearing (unaccounted) nodes before step 3 ran. |

---

## Specification notes

### §1 — Coverage command output format mismatch
The scenario (step 2) states the expected output is:
```
coverage: 0.0000 (2 behavior-bearing, 2 unaccounted)
```
The actual `coverage_cmd` stdout (`wicked-core.rs:735`) is:
```
coverage: {:.4} ({} behavior-bearing, {} unaccounted) → {out_path}
```
The trailing `→ {out_path}` suffix is always emitted and is absent from the scenario's expected text. **This is a wording error in the scenario spec.** The assertion for A5 must accept the `→ …` suffix.

### §2 — Double gate inside `build_domain_model`
The `domain_graph_cmd` passes the store-recomputed report directly to `build_domain_model`. That function independently calls `assert_front_half_coverage(coverage)` and then performs a second `recompute_front_half_coverage(store)` with its own `assert_front_half_coverage(&recomputed)` (defense-in-depth, `domain_model.rs:609-611`). In the scenario's case, both will fire the same way because both the passed report and the fresh store recompute reflect `unaccounted == 2`. The error surfaces from the first assertion (on the passed report), so the message is deterministic.

### §3 — `wicked-estate index` dependency on NodeKind::Function
The scenario's precondition is that `wicked-estate index` creates `NodeKind::Function` nodes for Rust `pub fn` items. The coverage predicate in `domain_model.rs:266` uses an exhaustive match where `Function` is behavior-bearing. If `wicked-estate` indexes Rust functions under a different `NodeKind` (e.g., `NodeKind::Other("fn")` or `NodeKind::Method`), then `behavior_bearing` count may differ from 2, and A3/A5 would fail. This is an external dependency that must be validated independently against the wicked-estate version in use.

### §4 — `recompute_front_half_coverage` success vs. fail exit from `domain_graph_cmd`
The `coverage_cmd` exits 0 even when coverage < 1.0 (it just writes the report). The fail-closed behavior is entirely in `domain_graph_cmd` via `build_domain_model`. If `recompute_front_half_coverage` itself returns `Err` (e.g., store open failure, out-of-range annotation confidence), the error message would say `"domain-graph: coverage recompute failed: …"` rather than the expected unaccounted-count message. The scenario does not include this case. Testers should confirm the store opened cleanly (step 1 exit code = 0) before treating a coverage-recompute-failed error as a test failure.

### §5 — Exit code 1 vs. 2
A1 accepts "1 or 2". Per the implementation, `fail()` at `wicked-core.rs:934-937` always uses `std::process::exit(1)`. Exit code 2 is emitted only by the CLI usage-error branch (`_ =>` arm at line 223). The fail-closed path in `domain_graph_cmd` goes through `fail()`, so code 1 is the definitive expected value. The scenario's acceptance of code 2 is permissive but would indicate an unexpected code path.
