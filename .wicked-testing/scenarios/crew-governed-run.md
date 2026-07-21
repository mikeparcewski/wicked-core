---
id: crew-governed-run
title: wicked-core engine powers a governed wicked-crew run end-to-end
maps_to: [core#L3-5]
trust_level: local-dev
tags: [wicked-core, wicked-crew, governance, engine, acceptance]
status: active
---

## Goal

Verify that wicked-core's engine correctly powers a real governed wicked-crew run:
the engine creates a run, applies conformance governance, assigns workers with their
invocations, and transitions run state — all observable via the crew HTTP API.

This satisfies wicked-core DoD L3-5: "wicked-testing acceptance pipeline: PASS
verdict against a real governed wicked-crew run driven by this version of wicked-core."

## Preconditions

- `wicked-crew` is on PATH (globally installed via npm).
- `wicked-core` is the engine embedded in the installed wicked-crew package.
- A temp directory is writable for the DB.

> **Note on Step 1 command path:** the step uses `which wicked-crew | xargs dirname` to locate the
> CLI entry point. This avoids coupling to a specific npm prefix or global install layout. On
> machines where `wicked-crew` is not on PATH, adjust to use the explicit entry point path from
> `npm root -g`.

## Steps

### Step 1 — Start wicked-crew serve (stub mode)

Start the wicked-crew daemon in stub mode (no real worker CLIs launched — the
engine governs the run synchronously and the run completes deterministically):

```bash
PORT=19382
DB="$TMPDIR/wc-l3-5-run.db"
rm -f "$DB"
node "$(which wicked-crew | xargs dirname)/../lib/node_modules/wicked-crew/dist/cli/index.js" \
  serve --db "$DB" --port "$PORT" --stub > "$TMPDIR/wc-l3-5-serve.log" 2>&1 &
CREW_PID=$!
echo "$CREW_PID" > "$TMPDIR/wc-l3-5.pid"
# Wait for server ready
READY=""
for i in $(seq 1 20); do
  if curl -s "http://localhost:$PORT/api/v1/health" > /dev/null 2>&1; then READY=1; break; fi
  sleep 0.5
done
echo "ready=$READY pid=$CREW_PID port=$PORT"
```

Record: `ready=1` if the server is listening, PID, port.

### Step 2 — Submit a run and capture the response

```bash
PORT=19382
RESPONSE=$(curl -s -X POST "http://localhost:$PORT/api/v1/runs" \
  -H "Content-Type: application/json" \
  -d '{"problem": "L3-5 acceptance test: verify wicked-core engine governs a wicked-crew run"}')
echo "$RESPONSE" > "$TMPDIR/wc-l3-5-create.json"
echo "$RESPONSE"
```

Record: the full JSON response (must contain `runId`).

### Step 3 — List runs and verify governance state

> **Note:** Step 1 removes the DB file (`rm -f "$DB"`) before starting the server, so
> the run list always contains exactly the one run created in Step 2. The script reads
> `runs[0]` safely because no prior state can leak in.

```bash
PORT=19382
RUNS=$(curl -s "http://localhost:$PORT/api/v1/runs")
echo "$RUNS" > "$TMPDIR/wc-l3-5-runs.json"
echo "$RUNS" | python3 -c "
import json, sys
d = json.load(sys.stdin)
runs = d.get('runs', [])
print(f'run_count={len(runs)}')
if runs:
    r = runs[0]
    s = r.get('session', {})
    print(f'run_id={s.get(\"id\",\"\")}')
    print(f'status={s.get(\"status\",\"\")}')
    print(f'problem_prefix={s.get(\"problem\",\"\")[:40]}')
    units = r.get('units', [])
    if units:
        u = units[0]
        print(f'conformance_ref_len={len(u.get(\"conformance_ref\",\"\"))}')
        print(f'assigned_cli={u.get(\"assigned_cli\",\"\")}')
    else:
        print('units=none')
"
```

Record: `run_count`, `run_id`, `status`, `conformance_ref_len` (governance hash
length — non-zero means conformance was recorded), `assigned_cli`.

### Step 4 — Get health to confirm engine version

```bash
PORT=19382
HEALTH=$(curl -s "http://localhost:$PORT/api/v1/health")
echo "$HEALTH"
```

Record: `version` field from the health response.

### Step 5 — Shut down the server

```bash
PID=$(cat "$TMPDIR/wc-l3-5.pid" 2>/dev/null)
[ -n "$PID" ] && kill "$PID" 2>/dev/null
echo "shutdown=ok"
```

Record: `shutdown=ok`.

## Assertions

- **A1** (Step 1): `ready=1` — wicked-crew serve started successfully with wicked-core engine.
- **A2** (Step 2): Response contains `runId` with a non-empty UUID — the engine created a run entry.
- **A3** (Step 3): `run_count >= 1`, `status` is one of `{executing, planning, distributing, completed}`, `conformance_ref_len >= 64` — the engine applied conformance governance (SHA-256 hex digest = 64 chars; anything shorter would indicate a placeholder or truncated value).
- **A4** (Step 3): `assigned_cli` is non-empty (e.g. `claude`, `agy`) — the engine assigned a worker to the run.
- **A5** (Step 4): `version` is `0.2.0` — confirming this is the expected wicked-core engine version.

## Evidence

- `$TMPDIR/wc-l3-5-serve.log` — server startup log showing `stub: true`
- `$TMPDIR/wc-l3-5-create.json` — run creation response with `runId`
- `$TMPDIR/wc-l3-5-runs.json` — full run list response showing governance state
