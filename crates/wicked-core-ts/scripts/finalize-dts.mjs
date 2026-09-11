// Post-build finalizer for the napi-generated `index.d.ts`.
//
// napi-rs generates the class/interface surface from the Rust `#[napi]` items, but it cannot emit
// `CoreEventJson` — a documentation-only helper interface with a `[k: string]: unknown` index
// signature (events carry arbitrary per-variant fields). No `#[napi]` annotation can produce an
// index signature, and there is no Rust type backing it (the subscribe callback delivers a raw JSON
// *string*, which consumers `JSON.parse` and cast to `CoreEventJson`). So we append it here.
//
// Deterministic + idempotent: the block is delimited by sentinels; a rerun strips the old block and
// re-appends the current one, so the committed `index.d.ts` is reproducible from a clean `napi build`.
// Cross-platform: pure Node, no shell builtins (per the repo's cross-platform hook/script rule).

import { readFileSync, writeFileSync, existsSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { dirname, join } from 'node:path'

const __dirname = dirname(fileURLToPath(import.meta.url))
const dtsPath = join(__dirname, '..', 'index.d.ts')

const BEGIN = '// ─── hand-authored (not napi-generated): see scripts/finalize-dts.mjs ───'
const END = '// ─── end hand-authored ───'

// The public contract's `CoreEventJson` — reproduced verbatim from the pre-migration hand-kept
// index.d.ts. This is the source of record for this type; keep it in lockstep with wicked-core's
// `CoreEvent` variants (event_to_json in src/lib.rs).
const HAND_AUTHORED = `${BEGIN}
/**
 * A CoreEvent, delivered as a JSON string to the {@link Core.subscribe} callback. Discriminated on
 * \`type\`. Fields vary by variant (see wicked-core \`CoreEvent\`): e.g.
 * \`sessionStarted\` \`{session, problem}\`, \`unitPlanned\` \`{session, ord, description}\`,
 * \`unitDistributed\` \`{session, ord, cli, routingMethod, seatConstraint}\` (\`seatConstraint\` is
 * \`null\` unless the unit's skill narrowed the candidate seats before the council voted),
 * \`awaitingHuman\` \`{session, ord, prompt}\`,
 * \`gateDecided\` \`{session, ord, allow}\`, \`unitDone\`/\`unitExecuting\`/\`resumed\` \`{session, ord}\`,
 * \`sessionCompleted\` \`{session}\`, \`sessionFailed\` \`{session, ord}\`, \`error\` \`{session, message}\`.
 * PTY terminal sessions emit \`terminalOpened\` \`{id, cwd}\`, \`terminalOutput\` \`{id, seq, bytesB64}\`
 * (raw output base64-encoded in \`bytesB64\`), and \`terminalExited\` \`{id, status}\`.
 * Gate evidence (wicked-core F-036/F-039): evaluatorMutatedWorktree {session, ord, attempt, cli,
 * phase, beforeTree, afterTree, headMoved, changed, restored, restoreError} and
 * repoChecksEvaluated {session, ord, attempt, passed, criterion, checks, skipped}.
 * core#431 additions: gateEvaluated carries \`judgeCli: string | null\` + \`judgeDistinct: boolean |
 * null\` (who rendered agentVerdict); worktreeRestored {session, ord, attempt, cli, phase, tree,
 * head, discarded, suggestionRef} (the creator's tree was put back after an evaluator mutation;
 * the discarded edit is pinned under suggestionRef);
 * deliverLiftEvaluated {session, ord, attempt, outcome: 'unchanged'|'lifted'|'conflict'|'skipped'|'failed',
 * baseRef, baseBefore, baseAfter, treeBefore, treeAfter, conflicts, note} (the deliver phase's
 * lift onto the remote tip, re-verified when it changed the tree); evaluatorToolCallDenied
 * {session, ord, attempt, cli, carrier, tool, kind, path, role, posture, reason} (a write-class
 * tool call a FENCED unit made was refused at the ACP permission boundary — `role` names the
 * unit's actual role, `posture` is 'read-only' for an executes_code:false evaluator/recon rung or
 * 'deliverable-roots' for a bound creator writing outside its granted write roots; the type
 * name is historical, read `role`); runBaseResolved
 * {session, baseRef, baseCommit, localHead, behind, fetched, lifted, note} (which base a fresh
 * run worktree was minted from).
 */
export interface CoreEventJson {
  type: string
  session?: string
  ord?: number
  [k: string]: unknown
}

/**
 * The \`unitDistributed\` event — a CLI was assigned to a unit — as delivered to the
 * {@link Core.subscribe} callback: the shape to read a parsed {@link CoreEventJson} as once
 * \`type === 'unitDistributed'\`. Every field is emitted unconditionally; the engine's \`Option\`
 * fields arrive as \`null\`, never absent. Pinned against wicked-core's \`event_to_json\` by the
 * binding's own tests (\`cargo test\` in this crate) and asserted at compile time by
 * \`types-test/\` (\`npm run typecheck\`).
 */
export interface UnitDistributedEventJson extends CoreEventJson {
  type: 'unitDistributed'
  session: string
  ord: number
  /** The roster key of the assigned seat. */
  cli: string
  /** How the seat was chosen: the council verdict, a degrade to the first candidate, an
   * evaluator ≠ creator reassignment, or a deterministic tool execution. */
  routingMethod: 'council' | 'degraded' | 'evaluator_distinct' | 'tool'
  agreementPct: number | null
  returned: number | null
  /** Seats convened for the council that produced this assignment (\`null\` = unknown). */
  seated: number | null
  dissent: number | null
  degradedReason: string | null
  /**
   * WHY the candidate seats were narrowed BEFORE the council voted (core#401): the unit's skill
   * (or a transitive mandate) is \`portable: false\` in the handed skills snapshot — or the root is
   * the Claude-only live-cache fallback — so only a claude seat could be handed it, and the
   * council chose among those. \`null\` when every roster seat was a candidate. Additive:
   * \`routingMethod\` and its fields read exactly as before.
   */
  seatConstraint: string | null
}

/**
 * One entry of the JSON array {@link Core.runEvents} resolves: the \`/ws\` frame ({@link CoreEventJson})
 * plus the durable log's envelope. Ordering contract (wicked-core#408): \`seq\` is strictly increasing
 * within a run for the run's WHOLE life — across daemon restarts, not just within one process — so
 * the array is in emission order and its last entry is the run's latest event. \`ts\` is capture time
 * (epoch millis) and may repeat within a burst; never order by it. The first entry a restarted
 * engine records for a run carries \`daemonRestarted: true\` (absent everywhere else), marking the
 * boundary for consumers that keep per-run state across the gap. All three are envelope-only: the
 * live \`/ws\` frame carries none of them.
 */
export interface RecordedEventJson extends CoreEventJson {
  /** Capture-time epoch millis. May repeat within a burst — not an order. */
  ts: number
  /**
   * Strictly increasing within the run, across daemon restarts. NOT the per-terminal \`seq\` of
   * \`terminalOutput\` frames — those are streaming chunks and are never recorded.
   */
  seq: number
  /** Present, and \`true\`, only on the first entry a restarted engine recorded for this run. */
  daemonRestarted?: true
}
${END}
`

if (!existsSync(dtsPath)) {
  console.error(`[finalize-dts] ${dtsPath} not found — did \`napi build\` run and emit the type defs?`)
  process.exit(1)
}

// LF throughout: a checkout with `core.autocrlf` (the Windows CI runner) hands us CRLF, and the
// block we append is LF — normalizing first keeps the committed file single-EOL and byte-identical
// across OSes (the binding's lockstep test compares this file's block to the script's on every OS).
let dts = readFileSync(dtsPath, 'utf8').replace(/\r\n/g, '\n')

// Strip any previously-appended block so reruns are idempotent.
const beginIdx = dts.indexOf(BEGIN)
if (beginIdx !== -1) {
  const endIdx = dts.indexOf(END, beginIdx)
  if (endIdx !== -1) {
    dts = dts.slice(0, beginIdx) + dts.slice(endIdx + END.length)
  } else {
    dts = dts.slice(0, beginIdx)
  }
}

dts = dts.replace(/\s*$/, '\n') + '\n' + HAND_AUTHORED
writeFileSync(dtsPath, dts, 'utf8')
console.log('[finalize-dts] appended CoreEventJson to index.d.ts')
