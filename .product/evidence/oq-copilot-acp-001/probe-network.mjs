#!/usr/bin/env node
// OQ-COPILOT-ACP-001 probe: a network action (curl), to see whether URL/network access
// escalates to a permission request under the default (no --allow-all-urls) invocation.
import { spawn } from 'node:child_process'
import { createInterface } from 'node:readline'
import { writeFileSync, appendFileSync } from 'node:fs'
const COPILOT_BIN = process.argv[2]
const FIXTURE_CWD = process.argv[3]
const CAPTURE_FILE = process.argv[4]
writeFileSync(CAPTURE_FILE, '')
function record(dir, raw) { appendFileSync(CAPTURE_FILE, JSON.stringify({ ts: new Date().toISOString(), dir, raw }) + '\n') }
const child = spawn(COPILOT_BIN, ['--acp'], { cwd: FIXTURE_CWD, stdio: ['pipe','pipe','pipe'], env: process.env })
child.stderr.on('data', d => appendFileSync(CAPTURE_FILE.replace('.ndjson','.stderr.log'), d))
let nextId = 1; const pending = new Map(); const events = []
function send(o){ const l=JSON.stringify(o); record('client->agent', l); child.stdin.write(l+'\n') }
function request(method, params){ const id=nextId++; return new Promise((res,rej)=>{ pending.set(id,{res,rej}); send({jsonrpc:'2.0',id,method,params}) }) }
const rl = createInterface({ input: child.stdout })
rl.on('line', line => {
  if (!line.trim()) return
  record('agent->client', line)
  let msg; try { msg = JSON.parse(line) } catch { return }
  events.push(msg)
  if (msg.id !== undefined && (msg.result !== undefined || msg.error !== undefined) && !msg.method) {
    const p = pending.get(msg.id); if (p) { pending.delete(msg.id); msg.error ? p.rej(msg.error) : p.res(msg.result) }
    return
  }
  if (msg.method && msg.id !== undefined) {
    if (msg.method === 'session/request_permission') {
      const options = msg.params?.options ?? []
      const chosen = options.find(o => o.kind === 'allow_once' || o.kind === 'allow_always') ?? options[0]
      send({ jsonrpc: '2.0', id: msg.id, result: { outcome: { outcome: 'selected', optionId: chosen?.optionId ?? 'yes' } } })
      return
    }
    send({ jsonrpc: '2.0', id: msg.id, error: { code: -32601, message: `harness stub: ${msg.method}` } })
  }
})
async function main() {
  await request('initialize', { protocolVersion: 1, clientCapabilities: { fs: {}, terminal: false, permission: true } })
  const s = await request('session/new', { cwd: FIXTURE_CWD, mcpServers: [] })
  const promptResult = await request('session/prompt', { sessionId: s.sessionId, prompt: [{ type: 'text', text: 'Run exactly this shell command now, do not ask me anything, do not explain: curl -sI https://example.com' }] })
  record('harness-meta', JSON.stringify({ promptResult }))
  await new Promise(r => setTimeout(r, 1500))
  const requestPermissionCalls = events.filter(e => e.method === 'session/request_permission')
  const toolCallEvents = events.filter(e => e.method === 'session/update' && ['tool_call','tool_call_update'].includes(e.params?.update?.sessionUpdate))
  console.log(JSON.stringify({
    requestPermissionCallCount: requestPermissionCalls.length,
    requestPermissionCalls: requestPermissionCalls.map(e => ({ kind: e.params?.toolCall?.kind, title: e.params?.toolCall?.title, rawInput: e.params?.toolCall?.rawInput })),
    toolCallEvents: toolCallEvents.map(e => ({ upd: e.params.update.sessionUpdate, kind: e.params.update.kind, status: e.params.update.status, title: e.params.update.title, rawInput: e.params.update.rawInput }))
  }, null, 2))
  child.stdin.end(); child.kill(); process.exit(0)
}
main().catch(e => { console.error(e); child.kill(); process.exit(1) })
