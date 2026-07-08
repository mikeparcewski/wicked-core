// Node smoke test for the wicked-core-ts napi binding.
//
// Proves the binding drives a real wicked-core run end-to-end IN-PROCESS with NO real LLM CLI:
//   1. spawn the STUB engine over a temp DB (deterministic dispatcher + StubStepRunner)
//   2. subscribe to the live CoreEvent stream (ThreadsafeFunction → JS callback)
//   3. launchRun a 2-unit problem gated BEFORE unit 1
//   4. confirm the stream carries SessionStarted / UnitPlanned / UnitDistributed and pauses (AwaitingHuman)
//   5. confirmGate(Approve) and confirm the run advances (Resumed → GateDecided → UnitDone → SessionCompleted)
//   6. read back a unit's captured stub output
//
// Deterministic + fast + offline (forces the lexical memory embedder so nothing is downloaded).

import { createRequire } from 'node:module'
import { fileURLToPath } from 'node:url'
import { dirname, join } from 'node:path'
import { tmpdir } from 'node:os'
import fs from 'node:fs'

// Offline: force the dependency-free lexical memory embedder (no Model2Vec download on spawn).
process.env.WICKED_MEMORY_EMBEDDER = 'hash'

const __dirname = dirname(fileURLToPath(import.meta.url))
const require = createRequire(import.meta.url)

// ── locate + stage the addon as a *.node file Node can require ────────────────
function stageAddon() {
  const dst = join(__dirname, 'index.node')
  const ext = process.platform === 'win32' ? 'dll' : process.platform === 'darwin' ? 'dylib' : 'so'
  const base = process.platform === 'win32' ? 'wicked_core_ts' : 'libwicked_core_ts'
  for (const profile of ['release', 'debug']) {
    const src = join(__dirname, 'target', profile, `${base}.${ext}`)
    if (fs.existsSync(src)) {
      fs.copyFileSync(src, dst)
      return dst
    }
  }
  if (fs.existsSync(dst)) return dst
  throw new Error('no cdylib found — run `cargo build -p wicked-core-ts` first')
}

const addonPath = stageAddon()
const { Core } = require(addonPath)

// ── a tiny event bus over the subscribe callback ──────────────────────────────
const events = []
const waiters = []

function onEvent(_err, json) {
  const ev = JSON.parse(json)
  events.push(ev)
  for (let i = waiters.length - 1; i >= 0; i--) {
    if (waiters[i].pred(ev)) {
      clearTimeout(waiters[i].timer)
      waiters[i].resolve(ev)
      waiters.splice(i, 1)
    }
  }
}

function waitFor(pred, label, ms = 10000) {
  const found = events.find(pred)
  if (found) return Promise.resolve(found)
  return new Promise((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new Error(`timed out after ${ms}ms waiting for: ${label}`)),
      ms,
    )
    waiters.push({ pred, resolve, timer })
  })
}

const has = (type) => events.some((e) => e.type === type)
const assert = (cond, msg) => {
  if (!cond) throw new Error(`assertion failed: ${msg}`)
}

async function main() {
  const dbPath = join(fs.mkdtempSync(join(tmpdir(), 'wicked-core-ts-')), 'core.db')
  console.log(`[smoke] db: ${dbPath}`)
  console.log(`[smoke] addon: ${addonPath}`)

  const core = Core.spawnStub(dbPath)
  const sub = core.subscribe(onEvent)

  // Liveness: the actor is up and the event stream works.
  const pong = await core.ping()
  assert(pong === 'ok', `ping should return "ok", got ${pong}`)
  await waitFor((e) => e.type === 'heartbeat', 'heartbeat')
  console.log('[smoke] ✓ ping → heartbeat received over the live stream')

  // Two council seats (stub dispatcher makes them converge; StubStepRunner produces the output).
  const clisJson = JSON.stringify([
    { key: 'alpha', display_name: 'Alpha', binary: 'alpha', headless_invocation: 'alpha {PROMPT}' },
    { key: 'beta', display_name: 'Beta', binary: 'beta', headless_invocation: 'beta {PROMPT}' },
  ])

  const sessionId = 'smoke-1'
  const runId = await core.launchRun({
    problem: 'Do step one. Do step two',
    sessionId,
    clisJson,
    entityMode: 'shared',
    humanConfirm: 'before:1', // pause BEFORE unit 1 → a human gate
  })
  assert(runId === sessionId, `launchRun should return the run id, got ${runId}`)
  console.log(`[smoke] ✓ launchRun → run id "${runId}"`)

  // The run pauses at the gate. Prove the plan/distribute events streamed AND it is awaiting a human.
  const gate = await waitFor((e) => e.type === 'awaitingHuman', 'awaitingHuman')
  assert(has('sessionStarted'), 'should have seen sessionStarted')
  assert(events.filter((e) => e.type === 'unitPlanned').length === 2, 'should have planned 2 units')
  assert(has('unitDistributed'), 'should have seen unitDistributed (council assignment)')
  assert(gate.ord === 1, `gate should pause before unit ord 1, got ${gate.ord}`)
  console.log(`[smoke] ✓ streamed plan/distribute; paused at human gate: "${gate.prompt}"`)

  // Approve the gate → the run resumes and drives to completion (stub steps, governance approves).
  const status = await core.confirmGate(runId, true)
  console.log(`[smoke] ✓ confirmGate(Approve) → status "${status}"`)
  await waitFor((e) => e.type === 'sessionCompleted', 'sessionCompleted')

  assert(has('resumed'), 'should have seen resumed after approval')
  assert(has('unitExecuting'), 'should have seen unitExecuting')
  const gates = events.filter((e) => e.type === 'gateDecided')
  assert(gates.length >= 1 && gates.every((g) => g.allow === true), 'gates should decide allow=true')
  assert(events.filter((e) => e.type === 'unitDone').length === 2, 'both units should be done')
  console.log('[smoke] ✓ run advanced past the gate to SessionCompleted (2 units done)')

  // Read back the captured stub transcript for unit 1.
  const out = JSON.parse(await core.workOutput(`${sessionId}:u1`))
  assert(typeof out === 'string' && out.includes('stub-output'), `unit 1 output should contain stub-output, got ${out}`)
  console.log(`[smoke] ✓ workOutput(${sessionId}:u1) = ${JSON.stringify(out)}`)

  // Read API: the session is on the store and terminal.
  const detail = JSON.parse(await core.sessionsDetail())
  assert(Array.isArray(detail) && detail.length === 1, 'sessionsDetail should list 1 session')
  assert(detail[0].session.status === 'completed', `session status should be completed, got ${detail[0].session.status}`)
  console.log('[smoke] ✓ sessionsDetail reflects a completed run')

  console.log(`\n[smoke] event stream (${events.length} events): ${events.map((e) => e.type).join(' → ')}`)
  sub.close() // tidy teardown (also exercises the Subscription handle)
  console.log('\n[smoke] PASS ✅')
  process.exit(0)
}

const guard = setTimeout(() => {
  console.error('\n[smoke] FAIL ❌ — overall timeout (30s)')
  console.error(`[smoke] events so far: ${events.map((e) => e.type).join(' → ')}`)
  process.exit(1)
}, 30000)
guard.unref?.()

main().catch((e) => {
  console.error(`\n[smoke] FAIL ❌ — ${e.stack || e}`)
  console.error(`[smoke] events so far: ${events.map((e) => e.type).join(' → ')}`)
  process.exit(1)
})
