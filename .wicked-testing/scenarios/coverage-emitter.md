---
id: coverage-emitter
title: wicked-core coverage emits a valid CoverageReport from a coverage-1.0 estate store
maps_to: [core#29, core#25]
trust_level: local-dev
tags: [wicked-core, coverage, domain-graph, cli, deterministic]
status: active
---

## Goal

`wicked-core coverage --db <store> --out <file>` reads the annotated estate store, recomputes
front-half coverage, and writes a valid `coverage-report.json`. With a fully-annotated store
(every behavior-bearing node has a `business_rule` or `risk` annotation), `coverage = 1.0` and
`unaccounted = 0`.

## Preconditions

- `wicked-core` binary is on PATH (or at `~/.local/bin/wicked-core`).
- `wicked-estate` binary is on PATH (or at `~/.local/bin/wicked-estate`).

## Steps

1. Create a fresh temp directory for this test run.

2. Create a minimal Rust source fixture with two functions:

   ```bash
   mkdir -p "$TMPDIR/wc-coverage-fixture/src"
   cat > "$TMPDIR/wc-coverage-fixture/src/lib.rs" << 'EOF'
   pub fn process_payment(amount: u64) -> Result<(), String> {
       if amount == 0 { return Err("amount must be positive".into()); }
       Ok(())
   }
   pub fn validate_order(order_id: u64) -> bool {
       order_id > 0
   }
   EOF
   ```

3. Index the fixture into a fresh estate DB:

   ```bash
   wicked-estate index "$TMPDIR/wc-coverage-fixture/src" \
     --db "$TMPDIR/wc-coverage-fixture.db"
   ```

   Verify: output mentions "3 nodes" indexed (2 Function + 1 Module shell node).

4. Annotate both functions so coverage reaches 1.0:

   ```bash
   wicked-estate annotate process_payment \
     --db "$TMPDIR/wc-coverage-fixture.db" \
     --type business_rule --key RULE-001 \
     --value "process_payment charges the customer; amount must be positive" \
     --confidence 0.95

   wicked-estate annotate validate_order \
     --db "$TMPDIR/wc-coverage-fixture.db" \
     --type business_rule --key RULE-002 \
     --value "validate_order returns true if and only if the order_id is positive" \
     --confidence 0.95
   ```

5. Run the coverage emitter:

   ```bash
   wicked-core coverage \
     --db "$TMPDIR/wc-coverage-fixture.db" \
     --out "$TMPDIR/coverage-report.json"
   ```

   Record: exit code, stdout/stderr.

6. Read and record `coverage-report.json` contents.

## Assertions

- A1: step 5 exits with code **0**.
- A2: `$TMPDIR/coverage-report.json` exists and is valid JSON.
- A3: `coverage-report.json` contains `"coverage": 1.0` (or `1`).
- A4: `coverage-report.json` contains `"unaccounted": 0`.
- A5: `coverage-report.json` contains `"behavior_bearing": 2` (the two indexed functions).
- A6: `coverage-report.json` contains a `"resolved"` count ≥ 1.

## Evidence

- `coverage-report.json` — the emitted coverage report
- `index-output.txt` — stdout from `wicked-estate index`
- `coverage-stdout.txt` — stdout/stderr from `wicked-core coverage`
