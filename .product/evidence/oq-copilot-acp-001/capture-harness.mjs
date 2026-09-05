#!/usr/bin/env node
// OQ-COPILOT-ACP-001 evidence capture harness.
// Minimal ACP JSON-RPC-2.0-over-NDJSON client speaking directly to the installed
// `copilot --acp` server. Captures every frame verbatim (both directions) to an
// NDJSON file for later analysis/redaction. No dependency on the ACP SDK's client
// helper so the capture reflects exactly what crosses the wire.
// Modeled on .product/evidence/oq-codex-acp-001/capture-harness.mjs.

import { spawn } from 'node:child_process'
import { createInterface } from 'node:readline'
import { writeFileSync, appendFileSync, existsSync, readFileSync } from 'node:fs'
import { resolve } from 'node:path'

const COPILOT_BIN = process.argv[2]
const FIXTURE_CWD = process.argv[3]
const CAPTURE_FILE = process.argv[4]
const SCENARIO = process.argv[5] || 'allow'
// Extra args appended to the spawn (e.g. --allow-all-tools) to probe whether a
// startup flag changes ACP-mode behavior. Space-separated in argv[6].
const EXTRA_ARGS = (process.argv[6] || '').split(' ').filter(Boolean)

if (!COPILOT_BIN || !FIXTURE_CWD || !CAPTURE_FILE) {
  console.error(
    'usage: capture-harness.mjs <copilot-bin> <fixture-cwd> <capture-file> <allow|reject> ["extra --flags"]'
  )
  process.exit(2)
}

writeFileSync(CAPTURE_FILE, '')

function record(direction, raw) {
  const entry = {
    ts: new Date().toISOString(),
    direction, // 'client->agent' | 'agent->client' | 'harness-meta'
    raw
  }
  appendFileSync(CAPTURE_FILE, JSON.stringify(entry) + '\n')
}

const child = spawn(COPILOT_BIN, ['--acp', ...EXTRA_ARGS], {
  cwd: FIXTURE_CWD,
  stdio: ['pipe', 'pipe', 'pipe'],
  env: process.env
})

child.stderr.on('data', d => {
  appendFileSync(CAPTURE_FILE.replace('.ndjson', '.stderr.log'), d)
})

let nextId = 1
const pending = new Map()
const events = [] // all parsed messages, in order

function send(obj) {
  const line = JSON.stringify(obj)
  record('client->agent', line)
  child.stdin.write(line + '\n')
}

function request(method, params) {
  const id = nextId++
  return new Promise((resolvePromise, rejectPromise) => {
    pending.set(id, { resolvePromise, rejectPromise })
    send({ jsonrpc: '2.0', id, method, params })
  })
}

const rl = createInterface({ input: child.stdout })
rl.on('line', line => {
  if (!line.trim()) return
  record('agent->client', line)
  let msg
  try {
    msg = JSON.parse(line)
  } catch {
    return
  }
  events.push(msg)

  if (msg.id !== undefined && (msg.result !== undefined || msg.error !== undefined) && !msg.method) {
    const p = pending.get(msg.id)
    if (p) {
      pending.delete(msg.id)
      if (msg.error) p.rejectPromise(msg.error)
      else p.resolvePromise(msg.result)
    }
    return
  }

  // Incoming request from the agent to us (the client): e.g. session/request_permission,
  // fs/read_text_file, fs/write_text_file, terminal/*.
  if (msg.method && msg.id !== undefined) {
    handleIncomingRequest(msg)
  }
})

function handleIncomingRequest(msg) {
  const { method, id, params } = msg

  if (method === 'session/request_permission') {
    const options = params?.options ?? []
    let chosen
    if (SCENARIO === 'reject') {
      chosen = options.find(o => o.kind === 'reject_once' || o.kind === 'reject_always') ?? options[0]
    } else {
      chosen = options.find(o => o.kind === 'allow_once' || o.kind === 'allow_always') ?? options[0]
    }
    send({
      jsonrpc: '2.0',
      id,
      result: { outcome: { outcome: 'selected', optionId: chosen?.optionId ?? 'yes' } }
    })
    return
  }

  if (method === 'fs/read_text_file') {
    record(
      'harness-meta',
      JSON.stringify({
        note: 'adapter sent fs/read_text_file despite fs:{} capabilities — capability-noncompliant; served for capture continuity'
      })
    )
    try {
      const content = readFileSync(params.path, 'utf-8')
      send({ jsonrpc: '2.0', id, result: { content } })
    } catch (e) {
      send({ jsonrpc: '2.0', id, error: { code: -32000, message: String(e) } })
    }
    return
  }

  // Any other client-side request (fs/write_text_file, terminal/*) — respond with a
  // generic "not supported" style error so the agent's own execution path (if any) is
  // forced to reveal itself rather than silently succeeding via our stub.
  send({
    jsonrpc: '2.0',
    id,
    error: { code: -32601, message: `harness stub: method not implemented: ${method}` }
  })
}

async function main() {
  await request('initialize', {
    protocolVersion: 1,
    // Byte-mirror the REAL wicked-core ACP client (src/acp_runner.rs:1518-1523):
    // fs:{} (no filesystem capability), terminal:false, permission:true. Advertising
    // permission:true is what makes this capture authoritative — it tells the agent this
    // client ANSWERS session/request_permission, so a copilot ACP server that gated
    // permission requests on that capability would now be forced to send them.
    clientCapabilities: { fs: {}, terminal: false, permission: true }
  })

  const newSession = await request('session/new', {
    cwd: FIXTURE_CWD,
    mcpServers: []
  })

  const sessionId = newSession.sessionId

  const markerName = SCENARIO === 'reject' ? 'marker-reject.txt' : 'marker-allow.txt'
  const promptText = [
    `Perform exactly these four actions, in order, using your normal tools (do not ask me anything, do not explain, just do them):`,
    `1. Read the file seed.txt in the current directory.`,
    `2. Edit seed.txt by appending a new line "edited" to the end of it.`,
    `3. Run the shell command: echo hello-from-copilot`,
    `4. Create a new file named ${markerName} in the current directory containing exactly the text OK.`,
    `Do all four steps now.`
  ].join('\n')

  const promptResult = await request('session/prompt', {
    sessionId,
    prompt: [{ type: 'text', text: promptText }]
  })

  record('harness-meta', JSON.stringify({ scenario: SCENARIO, sessionId, promptResult, markerName }))

  // Give any trailing async fs writes a moment to land before we check.
  await new Promise(r => setTimeout(r, 1500))

  const markerPath = resolve(FIXTURE_CWD, markerName)
  const markerExists = existsSync(markerPath)
  const markerContents = markerExists ? readFileSync(markerPath, 'utf-8') : null
  const seedPath = resolve(FIXTURE_CWD, 'seed.txt')
  const seedContents = existsSync(seedPath) ? readFileSync(seedPath, 'utf-8') : null

  const requestPermissionCalls = events.filter(e => e.method === 'session/request_permission')
  const toolCallEvents = events.filter(
    e =>
      e.method === 'session/update' &&
      (e.params?.update?.sessionUpdate === 'tool_call' || e.params?.update?.sessionUpdate === 'tool_call_update')
  )

  const summary = {
    scenario: SCENARIO,
    extraArgs: EXTRA_ARGS,
    markerPath,
    markerExists,
    markerContents,
    seedContents,
    requestPermissionCallCount: requestPermissionCalls.length,
    requestPermissionCalls: requestPermissionCalls.map(e => ({
      id: e.id,
      toolCall: e.params?.toolCall,
      options: e.params?.options
    })),
    toolCallEventCount: toolCallEvents.length,
    toolCallEvents: toolCallEvents.map(e => ({
      sessionUpdate: e.params.update.sessionUpdate,
      toolCallId: e.params.update.toolCallId,
      title: e.params.update.title,
      kind: e.params.update.kind,
      status: e.params.update.status,
      rawInput: e.params.update.rawInput
    }))
  }

  console.log(JSON.stringify(summary, null, 2))

  child.stdin.end()
  child.kill()
  process.exit(0)
}

main().catch(err => {
  console.error('harness error:', err)
  child.kill()
  process.exit(1)
})
