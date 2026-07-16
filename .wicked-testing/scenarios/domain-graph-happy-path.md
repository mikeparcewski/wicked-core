---
id: domain-graph-happy-path
title: wicked-core domain-graph emits requirements_graph.json when coverage is 1.0
maps_to: [core#29, core#22]
trust_level: local-dev
tags: [wicked-core, domain-graph, cli, deterministic]
status: active
---

## Goal

`wicked-core domain-graph --db <store> --out <file>` translates a fully-annotated estate store
into a `requirements_graph.json` domain model. When every behavior-bearing node is accounted for
(`coverage == 1.0`), the command exits 0 and the output file is valid JSON containing at least one
domain with business rules.

## Preconditions

- `wicked-core` binary is on PATH.
- `wicked-estate` binary is on PATH.

## Steps

1. Create the same covered fixture as in `coverage-emitter` (steps 1–4):

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

   wicked-estate index "$TMPDIR/wc-domgraph-fixture/src" \
     --db "$TMPDIR/wc-domgraph-fixture.db"

   wicked-estate annotate process_payment \
     --db "$TMPDIR/wc-domgraph-fixture.db" \
     --type business_rule --key RULE-001 \
     --value "process_payment charges the customer; amount must be positive" \
     --confidence 0.95

   wicked-estate annotate validate_order \
     --db "$TMPDIR/wc-domgraph-fixture.db" \
     --type business_rule --key RULE-002 \
     --value "validate_order returns true if and only if the order_id is positive" \
     --confidence 0.95
   ```

2. Run the domain-graph builder:

   ```bash
   wicked-core domain-graph \
     --db "$TMPDIR/wc-domgraph-fixture.db" \
     --out "$TMPDIR/requirements_graph.json"
   ```

   Record: exit code, stdout/stderr.

3. Read and record `requirements_graph.json` contents.

4. Validate JSON schema structure (check `metadata.schema_version`, `domains` key).

## Assertions

- A1: step 2 exits with code **0**.
- A2: `$TMPDIR/requirements_graph.json` exists and is valid JSON.
- A3: `requirements_graph.json` has `metadata.schema_version == "1.0.0"`.
- A4: `requirements_graph.json` has a non-empty `domains` key (at least 1 domain).
- A5: At least one domain contains at least one requirement with a non-empty `business_rules` array.
- A6: Each business rule has `confidence` in range `(0.0, 1.0]` and a non-empty `statement`.
- A7: stdout/stderr contains "wrote 1 domain(s)" (or similar positive confirmation).

## Evidence

- `requirements_graph.json` — the emitted domain model
- `domgraph-stdout.txt` — stdout/stderr from `wicked-core domain-graph`
