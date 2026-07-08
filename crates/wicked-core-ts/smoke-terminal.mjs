// Terminal smoke test for the wicked-core-ts napi binding.
//
// Proves the four PTY terminal methods (openTerminal / writeTerminal / resizeTerminal /
// closeTerminal) drive a real PTY through the Rust engine end-to-end, IN-PROCESS, with NO real LLM:
//   1. spawnStub a Core over a temp DB, subscribe to the live CoreEvent stream
//   2. openTerminal(tmpdir, ["cat"], 80, 24, governed=false) → await the `terminalOpened` event
//   3. writeTerminal(id, Buffer.from("hi-core-ts\n")) → assert a `terminalOutput` whose
//      base64-decoded bytes contain "hi-core-ts" (cat echoes stdin back through the PTY)
//   4. resizeTerminal(id, 100, 30) → resolves "ok"
//   5. closeTerminal(id) → assert a `terminalExited` event for that id
//
// The round-trip runs in a CHILD process that returns from main WITHOUT process.exit and WITHOUT
// closing the subscription — so a clean child exit (code 0) within the deadline ALSO proves the tsfn
// is unref'd and the process exits on its own (mirrors smoke-lifecycle's SIG-2). The parent observes
// that exit. WICKED_MEMORY_EMBEDDER is forced to `hash` (offline: no model download on spawn).
//
// Run: WICKED_MEMORY_EMBEDDER=hash node smoke-terminal.mjs

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

function loadAddon() {
  const entry = join(__dirname, 'index.js')
  if (!fs.existsSync(entry)) {
    throw new Error('index.js not found — run `npm run build` (napi build --platform --release) first')
  }
  return entry
}

const assert = (cond, msg) => { if (!cond) throw new Error(`assertion failed: ${msg}`) }

// ── the child role: the actual terminal round-trip ────────────────────────────
async function roleTerminal() {
  const addonPath = loadAddon()
  const { Core } = require(addonPath)

  // tiny event bus over the subscribe callback (same shape as smoke.mjs)
  const events = []
  const waiters = []
  const onEvent = (_err, json) => {
    const ev = JSON.parse(json)
    events.push(ev)
    for (let i = waiters.length - 1; i >= 0; i--) {
      if (waiters[i].pred(ev)) { clearTimeout(waiters[i].timer); waiters[i].resolve(ev); waiters.splice(i, 1) }
    }
  }
  const waitFor = (pred, label, ms = 10000) => {
    const found = events.find(pred)
    if (found) return Promise.resolve(found)
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error(`timed out after ${ms}ms waiting for: ${label}`)), ms)
      waiters.push({ pred, resolve, timer })
    })
  }
  // Output can arrive in >1 chunk (PTY echo + cat), so accumulate all bytes for this terminal.
  const decodedFor = (id) =>
    Buffer.concat(
      events.filter((e) => e.type === 'terminalOutput' && e.id === id).map((e) => Buffer.from(e.bytesB64, 'base64')),
    ).toString('utf8')
  async function waitForOutput(id, needle, ms = 10000) {
    const t0 = Date.now()
    for (;;) {
      const got = decodedFor(id)
      if (got.includes(needle)) return got
      if (Date.now() - t0 > ms) {
        throw new Error(`timed out (${ms}ms) waiting for terminal output containing ${JSON.stringify(needle)}; got ${JSON.stringify(got)}`)
      }
      await new Promise((r) => setTimeout(r, 20))
    }
  }

  const dbPath = join(fs.mkdtempSync(join(tmpdir(), 'wicked-core-ts-term-')), 'core.db')
  const cwd = tmpdir()
  console.log(`[child:terminal] db=${dbPath} cwd=${cwd}`)

  const core = Core.spawnStub(dbPath)
  // Deliberately NOT held/closed: proving exit-on-its-own (SIG-2) — the unref'd tsfn must let the
  // process end on its own after main returns, with no process.exit and no sub.close().
  core.subscribe(onEvent)

  // 1) open a `cat` PTY (ungoverned operator shell). `cat` with no args echoes stdin → stdout.
  const openPromise = core.openTerminal(cwd, ['cat'], 80, 24, false)
  const opened = await waitFor((e) => e.type === 'terminalOpened', 'terminalOpened')
  const id = await openPromise
  assert(typeof id === 'string' && id.length > 0, `openTerminal should resolve a non-empty id, got ${JSON.stringify(id)}`)
  assert(opened.id === id, `terminalOpened id (${opened.id}) should match openTerminal's resolved id (${id})`)
  console.log(`[child:terminal] OPENED id=${id} cwd=${opened.cwd}`)

  // 2) write keystrokes; assert the bytes round-trip back as base64 terminalOutput.
  const wr = await core.writeTerminal(id, Buffer.from('hi-core-ts\n'))
  assert(wr === 'ok', `writeTerminal should resolve "ok", got ${wr}`)
  const decoded = await waitForOutput(id, 'hi-core-ts')
  assert(decoded.includes('hi-core-ts'), `terminal output should contain "hi-core-ts", got ${JSON.stringify(decoded)}`)
  console.log(`[child:terminal] OUTPUT-OK decoded=${JSON.stringify(decoded)}`)

  // 3) resize (exercises the method; `cat` ignores SIGWINCH). Resolves "ok".
  const rz = await core.resizeTerminal(id, 100, 30)
  assert(rz === 'ok', `resizeTerminal should resolve "ok", got ${rz}`)
  console.log('[child:terminal] RESIZE-OK')

  // 4) close; assert the exit event fires for this terminal.
  const cl = await core.closeTerminal(id)
  assert(cl === 'ok', `closeTerminal should resolve "ok", got ${cl}`)
  const exited = await waitFor((e) => e.type === 'terminalExited' && e.id === id, 'terminalExited')
  console.log(`[child:terminal] EXITED-OK id=${exited.id} status=${JSON.stringify(exited.status ?? null)}`)

  console.log(`[child:terminal] event stream: ${events.map((e) => e.type).join(' → ')}`)
  console.log('[child:terminal] DONE — returning from main WITHOUT process.exit / WITHOUT sub.close()')
  // no process.exit, no sub.close(): the process MUST now exit on its own (unref'd tsfn)
}

// ── the parent driver: run the child, assert a clean exit within a deadline ───
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

const role = process.argv[2]
if (role === '--role-terminal') {
  roleTerminal().catch((e) => { console.error(`[child:terminal] error ${e.stack || e}`); process.exit(3) })
} else {
  ;(async () => {
    console.log('── terminal round-trip + exit-on-its-own ──')
    const r = await runChild('--role-terminal', 20000)
    assert(!r.timedOut, `child HUNG (>20s) — terminal round-trip stalled OR the process did not exit on its own (tsfn not unref'd)`)
    assert(r.code === 0, `child should exit cleanly on its own (code 0) with no process.exit(); got code=${r.code} signal=${r.signal}`)
    assert(/OPENED id=/.test(r.out), 'child should reach OPENED (openTerminal + terminalOpened)')
    assert(/OUTPUT-OK/.test(r.out), 'child should reach OUTPUT-OK (writeTerminal bytes round-tripped as base64 terminalOutput)')
    assert(/RESIZE-OK/.test(r.out), 'child should reach RESIZE-OK (resizeTerminal)')
    assert(/EXITED-OK/.test(r.out), 'child should reach EXITED-OK (closeTerminal → terminalExited)')
    console.log(`\n[terminal] ✓ open→write→output→resize→close round-trip proven; process exited on its own (code 0) in ${r.ms}ms`)
    console.log('[terminal] PASS ✅')
    process.exit(0)
  })().catch((e) => { console.error(`\n[terminal] FAIL ❌ — ${e.stack || e}`); process.exit(1) })
}
