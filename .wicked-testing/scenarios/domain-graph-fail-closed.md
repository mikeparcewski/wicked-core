---
id: domain-graph-fail-closed
title: wicked-core domain-graph refuses to build when coverage < 1.0 (unaccounted nodes)
maps_to: [core#29, core#22]
trust_level: local-dev
tags: [wicked-core, domain-graph, fail-closed, cli, deterministic]
status: active
---

## Goal

`wicked-core domain-graph` is fail-closed: it refuses to translate a graph that contains
unaccounted behavior-bearing nodes (`coverage < 1.0`). When the store has function nodes with
no `business_rule` or `risk` annotation, the command exits non-zero, writes no output file, and
reports the unaccounted node count.

## Preconditions

- `wicked-core` binary is on PATH.
- `wicked-estate` binary is on PATH.

## Steps

1. Create a minimal Rust fixture with two functions but NO annotations:

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

   Do NOT annotate. Both functions remain unaccounted → coverage = 0.0.

2. Confirm coverage is below 1.0 (optional verification step):

   ```bash
   wicked-core coverage --db "$TMPDIR/wc-uncovered-fixture.db"
   ```

   Expect: `coverage: 0.0000 (2 behavior-bearing, 2 unaccounted)`.

3. Attempt to build the domain graph, capturing stdout+stderr and exit code separately
   (do NOT pipe through `tee` — piping causes `$?` to capture `tee`'s exit code, not wicked-core's):

   ```bash
   wicked-core domain-graph \
     --db "$TMPDIR/wc-uncovered-fixture.db" \
     --out "$TMPDIR/requirements_graph_should_not_exist.json" \
     > "$TMPDIR/domgraph-fail-output.txt" 2>&1
   DOMGRAPH_EXIT=$?
   echo "exit_code=$DOMGRAPH_EXIT" >> "$TMPDIR/domgraph-fail-output.txt"
   ```

   Record: exit code (from `$DOMGRAPH_EXIT`), combined stdout/stderr.

4. Check whether the output file was created:

   ```bash
   ls -la "$TMPDIR/requirements_graph_should_not_exist.json" 2>&1
   ```

## Assertions

- A1: step 3 exits with a **non-zero** exit code (1 or 2).
- A2: `$TMPDIR/requirements_graph_should_not_exist.json` does **NOT** exist.
- A3: stderr from step 3 mentions "unaccounted" or "coverage" and the number 2 (unaccounted count).
- A4: stderr from step 3 does NOT contain "wrote" (no partial write).
- A5: The step 2 verification confirms `coverage: 0.0000` before the fail-closed attempt.

## Evidence

- `coverage-check.txt` — output from `wicked-core coverage` (step 2)
- `domgraph-fail-stdout.txt` — stdout/stderr from `wicked-core domain-graph` (step 3)
- `ls-output.txt` — output of the file-existence check (step 4)
