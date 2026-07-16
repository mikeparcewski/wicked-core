# Test Plan: wicked-core gate-hook exits 2 (DENY) when the decisions path or estate store is missing

## Overview

This plan covers the fail-closed guarantee of `wicked-core gate-hook`: any invocation that cannot evaluate and record a governance decision must exit with code 2 (DENY) and emit a "wicked-governance: DENY" prefix on stderr. Two infrastructure-failure sub-cases are exercised independently. No injection was detected in the scenario body.

---

## Preconditions

| # | Precondition | Verification method |
|---|---|---|
| P1 | `wicked-core` binary is on PATH | `which wicked-core` exits 0 and prints an absolute path |
| P2 | `wicked-estate` binary is on PATH (for fixture creation in Case A) | `which wicked-estate` exits 0; **see Specification Note SN-1 if absent** |
| P3 | `$TMPDIR` is set and writable | `ls "$TMPDIR"` exits 0 |
| P4 | `env -i` is available (macOS / Linux standard) | `env -i true` exits 0 |
| P5 | Neither `WICKED_DECISIONS_PATH` nor `WICKED_ESTATE_DB` is exported in the ambient shell that runs Case B's `env -i` invocation | `env -i PATH="$PATH" printenv WICKED_ESTATE_DB WICKED_DECISIONS_PATH` produces no output |

---

## Test Steps

### Step 1: Create the estate DB fixture (Case A precondition)

**Action**:
```bash
mkdir -p "$TMPDIR/wc-hook-fixture/src"
cat > "$TMPDIR/wc-hook-fixture/src/lib.rs" << 'EOF'
pub fn process_payment(amount: u64) -> Result<(), String> { Ok(()) }
EOF
wicked-estate index "$TMPDIR/wc-hook-fixture/src" --db "$TMPDIR/wc-hook-fixture.db"
```

**Evidence file**: `step1-fixture.txt`

Capture with:
```bash
( wicked-estate index "$TMPDIR/wc-hook-fixture/src" --db "$TMPDIR/wc-hook-fixture.db" \
  > "$TMPDIR/step1-fixture.txt" 2>&1 )
STEP1_EXIT=$?
echo "exit_code=$STEP1_EXIT" >> "$TMPDIR/step1-fixture.txt"
ls -la "$TMPDIR/wc-hook-fixture.db" >> "$TMPDIR/step1-fixture.txt" 2>&1
```

**Evidence gate**: The file must contain `exit_code=0` and the `ls -la` line must show `wc-hook-fixture.db` exists with size > 0. If `wicked-estate` is absent, see SN-1 for the minimum viable alternative.

---

### Step 2: Case A — invoke gate-hook with valid DB but empty WICKED_DECISIONS_PATH

**Action**:
```bash
echo '{"tool_name":"Bash","tool_input":{"command":"ls"}}' \
  | WICKED_DECISIONS_PATH="" wicked-core gate-hook --db "$TMPDIR/wc-hook-fixture.db" \
  > "$TMPDIR/case-a-output.txt" 2>&1
CASE_A_EXIT=$?
echo "exit_code=$CASE_A_EXIT" >> "$TMPDIR/case-a-output.txt"
```

**Evidence file**: `case-a-output.txt`

**Evidence gate**: File must contain all three of:
- `exit_code=2`
- the substring `WICKED_DECISIONS_PATH unset` OR the substring `cannot record decision`
- a line beginning with `wicked-governance: DENY`

---

### Step 3: Case B — invoke gate-hook with no DB and no env vars

**Action**:
```bash
echo '{"tool_name":"Bash","tool_input":{"command":"ls"}}' \
  | env -i PATH="$PATH" wicked-core gate-hook \
  > "$TMPDIR/case-b-output.txt" 2>&1
CASE_B_EXIT=$?
echo "exit_code=$CASE_B_EXIT" >> "$TMPDIR/case-b-output.txt"
```

**Evidence file**: `case-b-output.txt`

**Evidence gate**: File must contain all three of:
- `exit_code=2`
- the substring `no estate store resolvable`
- a line beginning with `wicked-governance: DENY`

---

### Step 4: Verify no decisions file was written (negative evidence)

**Action**:
```bash
ls "$TMPDIR/wc-hook-fixture.db.lock" > "$TMPDIR/case-a-no-decisions.txt" 2>&1
echo "lockfile_present=$?" >> "$TMPDIR/case-a-no-decisions.txt"
ls "$TMPDIR"/wicked-core-gov/ > "$TMPDIR/case-a-no-decisions.txt" 2>&1 || \
  echo "gov_dir_absent=1" >> "$TMPDIR/case-a-no-decisions.txt"
```

**Evidence file**: `case-a-no-decisions.txt`

**Evidence gate**: No `decisions.ndjson` file was created under `$TMPDIR/wicked-core-gov/` for the invocations in Steps 2 and 3. Both fail before `append_decision` is called (Case B before stdin is read; Case A before `open_store` is called), so the governance dir for this fixture must not have a decisions log attributed to these invocations.

---

## Assertions

| ID | Criterion | Evidence File | Verification Method |
|----|-----------|--------------|---------------------|
| A1 | Case A exit code is 2 | `case-a-output.txt` | `grep -F 'exit_code=2' case-a-output.txt` exits 0 |
| A2 | Case A output contains "WICKED_DECISIONS_PATH unset" | `case-a-output.txt` | `grep -F 'WICKED_DECISIONS_PATH unset' case-a-output.txt` exits 0 |
| A2b | Case A output contains "cannot record decision" | `case-a-output.txt` | `grep -F 'cannot record decision' case-a-output.txt` exits 0 |
| A3 | Case A output's first non-empty content line starts with "wicked-governance: DENY" | `case-a-output.txt` | `head -1 case-a-output.txt \| grep -F 'wicked-governance: DENY'` exits 0 |
| A4 | Case B exit code is 2 | `case-b-output.txt` | `grep -F 'exit_code=2' case-b-output.txt` exits 0 |
| A5 | Case B output contains "no estate store resolvable" | `case-b-output.txt` | `grep -F 'no estate store resolvable' case-b-output.txt` exits 0 |
| A5b | Case B output contains "fail-closed" | `case-b-output.txt` | `grep -F 'fail-closed' case-b-output.txt` exits 0 |
| A6 | Case B output's first non-empty content line starts with "wicked-governance: DENY" | `case-b-output.txt` | `head -1 case-b-output.txt \| grep -F 'wicked-governance: DENY'` exits 0 |
| A7 | Case B fires at store-unavailable (before stdin is read), not at decisions-path | `case-b-output.txt` | The DENY message must NOT contain "WICKED_DECISIONS_PATH" |
| A8 | Case A does not mention "no estate store resolvable" (distinct DENY reasons) | `case-a-output.txt` | `grep -F 'no estate store resolvable' case-a-output.txt` exits non-zero |
| A9 | Case B does not mention "WICKED_DECISIONS_PATH" (store check fires before env check) | `case-b-output.txt` | `grep -F 'WICKED_DECISIONS_PATH' case-b-output.txt` exits non-zero |
| A10 | Step 1 fixture DB exists and is non-empty | `step1-fixture.txt` | `grep 'exit_code=0' step1-fixture.txt` exits 0 |

---

## Evidence Requirements

The following files must be collected and retained as the run's evidence package:

| File | Produced at | Role |
|---|---|---|
| `step1-fixture.txt` | Step 1 | Proves fixture DB was created (precondition for A10) |
| `case-a-output.txt` | Step 2 | Combined stdout+stderr + exit code for Case A |
| `case-b-output.txt` | Step 3 | Combined stdout+stderr + exit code for Case B |
| `case-a-no-decisions.txt` | Step 4 | Negative evidence: no decisions log written by failing cases |

---

## Specification Notes

**SN-1 — `wicked-estate` is not required for Case A exit-code correctness.**
The implementation evaluates `store_unavailable(db)` before opening the store file. Any non-empty, non-postgres, non-`:memory:` path passes the `store_unavailable` check. Case A exits at the `WICKED_DECISIONS_PATH` check, which occurs after `store_unavailable` returns `None` but before `open_store` is called. Therefore the DB file does not need to exist or be valid for A1–A3 to hold. If `wicked-estate` is unavailable, Case A can be exercised with `--db "$TMPDIR/any-path.db"` without running Step 1.

**SN-2 — Stdin format mismatch (non-blocking).**
The scenario pipes `{"type":"bash","input":{"command":"ls"}}` but the implementation parses `{"tool_name":…,"tool_input":{…}}`. For Case A, stdin is read but the parsed context is discarded before the DENY fires; for Case B, stdin is never read. This format mismatch has no effect on exit code or DENY message for either sub-case.

**SN-3 — Scope and phase are not passed in either sub-case.**
Both resolve to empty strings. Acceptable because both DENY paths exit before scope/phase values are used.

**SN-4 — "Starts with" assertion (A3, A6) applies to stderr content.**
The DENY messages are emitted via `eprintln!` (stderr). The scenario captures stderr via `2>&1`. Since neither code path emits anything to stdout before the DENY message, the first line of the combined output file is the DENY line.

**SN-5 — Case B order of checks is implementation-specific and validated by A7/A9.**
`store_unavailable` is the first check in `run_gate_hook`. With `db = None` it immediately returns `Some(reason)` and exits. The DECISIONS_PATH check is never reached. Assertions A7 and A9 make this ordering observable in the evidence.
