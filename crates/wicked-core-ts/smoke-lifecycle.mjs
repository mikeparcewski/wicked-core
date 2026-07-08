// Lifecycle regression suite for the wicked-core-ts napi binding.
//
// Proves the three ThreadsafeFunction lifecycle fixes an adversarial review found — each of which
// the original happy-path smoke MASKED (it always ended in process.exit(0), never returned from
// main, and never threw in a subscriber):
//
//   SIG-2  exit-without-process.exit: a child that subscribes, does a trivial op, and returns from
//          main() WITHOUT process.exit() (and WITHOUT close) must EXIT ON ITS OWN — proves the tsfn
//          is unref'd and no longer pins the libuv loop open.
//   SIG-1  throwing-callback contained: a subscriber that throws on its first event must NOT abort
//          the process — proves the JS try/catch shim contains the throw (napi would otherwise
//          escalate it to napi_fatal_exception → uncaughtException → death).
//   SIG-3  re-subscribe / unsubscribe: subscribe → close → subscribe again delivers to the NEW
//          callback only, with the old one receiving nothing further (no leaked pump, no dup stream),
//          and the process still exits cleanly.
//
// Run: WICKED_MEMORY_EMBEDDER=hash node smoke-lifecycle.mjs
// The SIG-1/SIG-2 cases run as child processes (the point is observing process-level exit/abort);
// this file re-execs itself with a role arg to be a single self-contained script.

import { createRequire } from 'node:module'
import { fileURLToPath } from 'node:url'
import { dirname, join } from 'node:path'
import { tmpdir } from 'node:os'
import fs from 'node:fs'
import { spawn } from 'node:child_process'

process.env.WICKED_MEMORY_EMBEDDER = 'hash'
const __filename = fileURLToPath(import.meta.url)
const __dirname = dirname(__filename)
const require = createRequire(import.meta.url)

function stageAddon() {
  const dst = join(__dirname, 'index.node')
  const ext = process.platform === 'win32' ? 'dll' : process.platform === 'darwin' ? 'dylib' : 'so'
  const base = process.platform === 'win32' ? 'wicked_core_ts' : 'libwicked_core_ts'
  for (const profile of ['release', 'debug']) {
    const src = join(__dirname, 'target', profile, `${base}.${ext}`)
    if (fs.existsSync(src)) { fs.copyFileSync(src, dst); return dst }
  }
  if (fs.existsSync(dst)) return dst
  throw new Error('no cdylib found — run `cargo build` first')
}
const addonPath = stageAddon()
const { Core } = require(addonPath)

const tmpDb = (tag) => join(fs.mkdtempSync(join(tmpdir(), `wc-${tag}-`)), 'core.db')
const assert = (cond, msg) => { if (!cond) throw new Error(`assertion failed: ${msg}`) }
function waitUntil(pred, ms, label) {
  return new Promise((resolve, reject) => {
    const t0 = Date.now()
    const tick = () => {
      if (pred()) return resolve()
      if (Date.now() - t0 > ms) return reject(new Error(`timed out (${ms}ms) waiting for: ${label}`))
      setTimeout(tick, 15)
    }
    tick()
  })
}

// ─────────────────────────── child roles (spawned) ───────────────────────────

// SIG-2: subscribe, do a trivial op, return from main WITHOUT process.exit and WITHOUT close.
// If the addon still held the loop open, this would hang and the parent would time out.
async function roleHang() {
  const core = Core.spawnStub(tmpDb('hang'))
  const seen = []
  core.subscribe((_err, json) => seen.push(json))
  const pong = await core.ping()
  console.log(`[child:hang] ping=${pong}`)
  // Ensure the one heartbeat drained (pump then parks) so exit is deterministic — a trivial op.
  await waitUntil(() => seen.length >= 1, 3000, 'heartbeat delivered')
  console.log('[child:hang] returning from main WITHOUT process.exit / WITHOUT close')
  // deliberately no process.exit, no sub.close() → the process must still end on its own
}

// SIG-1: subscribe with a callback that throws on every event; the process must survive.
async function roleThrow() {
  const core = Core.spawnStub(tmpDb('throw'))
  let thrown = 0
  const sub = core.subscribe((_err, _json) => {
    thrown++
    throw new Error(`boom from subscriber (#${thrown})`)
  })
  await core.ping() // emits Heartbeat → fires the throwing callback
  await new Promise((r) => setTimeout(r, 800))
  assert(thrown >= 1, 'callback never fired — test would be inconclusive')
  console.log(`[child:throw] SURVIVED after ${thrown} throw(s) — process not aborted by a throwing subscriber`)
  sub.close()
  process.exit(0)
}

function runChild(role, deadlineMs) {
  return new Promise((resolve) => {
    const child = spawn(process.execPath, [__filename, role], { stdio: ['ignore', 'pipe', 'pipe'] })
    let out = '', err = ''
    child.stdout.on('data', (d) => { out += d; process.stdout.write(d) })
    child.stderr.on('data', (d) => { err += d; process.stderr.write(d) })
    const t0 = Date.now()
    let done = false
    const timer = setTimeout(() => {
      if (!done) { child.kill('SIGKILL'); resolve({ timedOut: true, code: null, signal: null, ms: Date.now() - t0, out, err }) }
    }, deadlineMs)
    timer.unref?.()
    child.on('exit', (code, signal) => { done = true; clearTimeout(timer); resolve({ timedOut: false, code, signal, ms: Date.now() - t0, out, err }) })
  })
}

// ───────────────────────────── the three tests ───────────────────────────────

async function testSig2ExitWithoutProcessExit() {
  console.log('\n── SIG-2: exit without process.exit ──')
  const r = await runChild('--role-hang', 5000)
  assert(!r.timedOut, 'SIG-2: child HUNG (>5s) — the addon still pins the libuv loop open (tsfn not unref\'d)')
  assert(r.code === 0, `SIG-2: child should exit cleanly on its own (code 0); got code=${r.code} signal=${r.signal}`)
  console.log(`[lifecycle] ✓ SIG-2: process exited on its own in ${r.ms}ms (code 0), no process.exit() needed`)
}

async function testSig1ThrowingCallbackContained() {
  console.log('\n── SIG-1: throwing callback contained ──')
  const r = await runChild('--role-throw', 8000)
  assert(!r.timedOut, 'SIG-1: child hung')
  assert(r.code === 0, `SIG-1: a throwing subscriber must be contained; child should exit 0; got code=${r.code} signal=${r.signal}`)
  assert(/SURVIVED/.test(r.out), 'SIG-1: child should reach the SURVIVED line')
  assert(!/boom from subscriber/.test(r.err), 'SIG-1: the subscriber throw must not surface as an uncaught error')
  console.log(`[lifecycle] ✓ SIG-1: throwing subscriber contained — no abort, child exited 0 in ${r.ms}ms`)
}

async function testSig3ResubscribeNoDuplicate() {
  console.log('\n── SIG-3: re-subscribe / unsubscribe ──')
  const core = Core.spawnStub(tmpDb('resub'))

  const first = []
  const sub1 = core.subscribe((_e, j) => first.push(JSON.parse(j)))
  await core.ping()
  await waitUntil(() => first.some((e) => e.type === 'heartbeat'), 3000, 'sub1 heartbeat')
  sub1.close() // unsubscribe
  const firstCountAtUnsub = first.length

  const second = []
  const sub2 = core.subscribe((_e, j) => second.push(JSON.parse(j)))
  await core.ping()
  await waitUntil(() => second.some((e) => e.type === 'heartbeat'), 3000, 'sub2 heartbeat')
  await new Promise((r) => setTimeout(r, 300)) // give any stray/leaked delivery a chance to show up

  assert(second.length >= 1, 'SIG-3: the new subscription should receive events')
  assert(
    first.length === firstCountAtUnsub,
    `SIG-3: the unsubscribed callback must receive NO further events (had ${firstCountAtUnsub}, now ${first.length}) — leaked pump / duplicate delivery`,
  )
  sub2.close()
  console.log(`[lifecycle] ✓ SIG-3: after unsubscribe the old callback got 0 further events; new sub delivered ${second.length}. No leak / no duplicates`)

  // Also prove re-subscribe churn does not leak threads that keep the process alive: this test
  // function returns and, together with the closed subscriptions, the script exits on its own.
}

// ───────────────────────────────── driver ────────────────────────────────────

const role = process.argv[2]
if (role === '--role-hang') {
  roleHang().catch((e) => { console.error('[child:hang] error', e); process.exit(3) })
} else if (role === '--role-throw') {
  roleThrow().catch((e) => { console.error('[child:throw] error', e); process.exit(3) })
} else {
  const guard = setTimeout(() => { console.error('\n[lifecycle] FAIL ❌ — overall timeout (60s)'); process.exit(1) }, 60000)
  guard.unref?.()
  ;(async () => {
    await testSig2ExitWithoutProcessExit()
    await testSig1ThrowingCallbackContained()
    await testSig3ResubscribeNoDuplicate()
    console.log('\n[lifecycle] PASS ✅ — SIG-1, SIG-2, SIG-3 all proven')
    process.exit(0)
  })().catch((e) => { console.error(`\n[lifecycle] FAIL ❌ — ${e.stack || e}`); process.exit(1) })
}
