---
id: gate-hook-fail-closed
title: wicked-core gate-hook exits 2 (DENY) when the decisions path or estate store is missing
maps_to: [core#29, core#24]
trust_level: local-dev
tags: [wicked-core, gate-hook, fail-closed, cli, deterministic]
status: active
---

## Goal

`wicked-core gate-hook` is fail-closed: any infrastructure failure (missing decisions log path,
missing estate store) produces exit code 2 (DENY). This prevents unrecorded governance decisions
from allowing tool calls to proceed.

Two sub-cases are tested:
- **Case A**: `WICKED_DECISIONS_PATH` is unset; DB is provided → exit 2 with "WICKED_DECISIONS_PATH unset" message.
- **Case B**: `WICKED_DECISIONS_PATH` and `WICKED_ESTATE_DB` are both unset; no `--db` flag → exit 2 with "no estate store resolvable" message.

## Preconditions

- `wicked-core` binary is on PATH.
- A valid estate DB is available for Case A (can reuse the covered fixture from `coverage-emitter`).

## Steps

### Case A — decisions path unset

1. Create the covered fixture DB (or reuse from another test run):

   ```bash
   mkdir -p "$TMPDIR/wc-hook-fixture/src"
   cat > "$TMPDIR/wc-hook-fixture/src/lib.rs" << 'EOF'
   pub fn process_payment(amount: u64) -> Result<(), String> { Ok(()) }
   EOF
   wicked-estate index "$TMPDIR/wc-hook-fixture/src" --db "$TMPDIR/wc-hook-fixture.db"
   ```

2. Invoke gate-hook with a valid DB but no `WICKED_DECISIONS_PATH`.
   Capture exit code explicitly before any further shell commands (piping stdin is fine, but do NOT
   pipe stdout/stderr through `tee` — that would capture `tee`'s exit code instead of gate-hook's):

   ```bash
   echo '{"type":"bash","input":{"command":"ls"}}' \
     | WICKED_DECISIONS_PATH="" wicked-core gate-hook --db "$TMPDIR/wc-hook-fixture.db" \
     > "$TMPDIR/case-a-output.txt" 2>&1
   CASE_A_EXIT=$?
   echo "exit_code=$CASE_A_EXIT" >> "$TMPDIR/case-a-output.txt"
   ```

   Record: exit code (from `$CASE_A_EXIT`), combined stdout/stderr.

### Case B — no estate store at all

3. Invoke gate-hook with no DB and no env vars (unset both):

   ```bash
   echo '{"type":"bash","input":{"command":"ls"}}' \
     | env -i PATH="$PATH" wicked-core gate-hook \
     > "$TMPDIR/case-b-output.txt" 2>&1
   CASE_B_EXIT=$?
   echo "exit_code=$CASE_B_EXIT" >> "$TMPDIR/case-b-output.txt"
   ```

   Record: exit code (from `$CASE_B_EXIT`), combined stdout/stderr.

## Assertions

- A1 (Case A): exit code is **2**.
- A2 (Case A): output contains "WICKED_DECISIONS_PATH unset" or "cannot record decision".
- A3 (Case A): output starts with "wicked-governance: DENY".
- A4 (Case B): exit code is **2**.
- A5 (Case B): output contains "no estate store resolvable" or "fail-closed".
- A6 (Case B): output starts with "wicked-governance: DENY".

## Evidence

- `case-a-output.txt` — stdout/stderr + exit code from Case A (step 2)
- `case-b-output.txt` — stdout/stderr + exit code from Case B (step 3)
