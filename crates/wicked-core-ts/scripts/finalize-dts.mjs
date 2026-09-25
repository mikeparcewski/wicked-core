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
 * tool call a FENCED unit made was refused at the ACP permission boundary — \`role\` names the
 * unit's actual role, \`posture\` is 'read-only' for an executes_code:false evaluator/recon rung or
 * 'deliverable-roots' for a bound creator writing outside its granted write roots; the type
 * name is historical, read \`role\`); runBaseResolved
 * {session, baseRef, baseCommit, localHead, behind, fetched, lifted, note, runBranch} (which base a
 * fresh run worktree was minted from; runBranch is the \`wicked/<run>\` branch — wave 6).
 * Wave 6 additions (all additive): gateEvaluated carries \`ungated: boolean\` + \`ungatedReason:
 * string | null\` (true when NO machine layer gated the unit — render UNGATED, never "pass"),
 * \`floorNote: string | null\` (why the deterministic layer is absent, whenever it is) and
 * \`judgeSkippedReason: string | null\` (why no judge was convened for a unit that wanted one);
 * repoChecksEvaluated carries \`sandboxLevel: string\`, \`sandboxError: string | null\`,
 * \`detectError: string | null\` (an empty \`checks\` says WHY on the wire);
 * unitDistributed.degradedReason is set on EVERY routing method whenever eligible seats <
 * configured ("N of M seats benched: …"); workerToolCallDenied {session, ord, attempt, cli,
 * carrier, role, tool, command, reason, remedy} (a worker seat's \`git push\` / \`gh pr create\` /
 * \`gh api\` mutation refused — delivery is the deliver phase's job); acpFallback.fallbackKind gains
 * 'auth_failed' | 'unauthenticated' (no single-shot fallback follows an auth kind).
 * F-E2E-030/029/028 additions (all additive): awaitingHuman carries \`gateKind: 'run_level' | 'def' |
 * 'deliver' | 'terminal' | 'escalation' | 'failure' | 'triage'\` (WHY the run paused — key on it,
 * never on the prompt's wording; the engine's deliver gate is 'deliver'); workerToolCallDenied.reason
 * may start with 'install fence:' (a package-manager install outside the unit's worktree, judged
 * from the seat's shell cwd tracked across its tool calls — best-effort, never hermetic);
 * sandboxPosture {session, ord, cli, posture: 'os' | 'advisory', reason} (the write containment
 * the assigned seat runs under — 'advisory' = no OS write boundary on the seat record, command-text
 * fences + worktree guard only); worktreeRetained {session, path, reason} (a terminal run's worktree
 * kept because it holds uncommitted work — cancel included).
 * core#468 (additive): unitDispatched carries \`baseSkill: {name, role, handed} | null\` — the run's
 * role-keyed BASE skill directive (\`role\` is 'creator' | 'evaluator' | 'neutral'), null when the
 * run declares none; the generation it is handed from is the same unit's skillsSnapshotHanded.gen.
 * core#479 (additive, DES-L4 PR-⑥): \`baseSkill.handed: boolean\` — true when the seat's CLI has a
 * per-launch skills lever and the admitted generation holds the skill (the directive names a skill
 * the seat can invoke); false for a lever-less seat, which is told the discipline is NOT loaded.
 * DES-L1 PR-1A (additive): gateEvaluated carries \`evaluatorVerdict: string | null\` — the Evaluator
 * unit's OWN verdict token read from its output (\`'PASS'\` | \`'FAIL'\` | any other token it wrote; PASS
 * is the only pass, every other token denies into the escalation gate); null when the layer did not
 * read the unit (creator/neutral/tool) or the evaluator wrote no \`VERDICT:\` line (then
 * \`denial.source === 'evaluator_verdict'\` and \`denial.reason\` is the contract text).
 * DES-L1 PR-1B (additive): unitReworkAmended carries \`scope: 'cursor' | 'creator' | 'request_changes'\`
 * — which gate arm landed the amendment; for \`request_changes\` the \`ord\` is the rewound creator and
 * \`amendment\` is the rejected review's findings followed by the operator's note.
 * core#467/#469/F-RC2-009 additions (all additive): repoChecksEvaluated carries \`outcome: 'passed' |
 * 'failed' | 'timed_out' | 'not_run'\` (a timed-out floor denies under source 'repo_checks_timeout',
 * never 'repo_checks'), \`floor: 'creator' | 'verify'\`, \`claim: {phrase, check, verdict} | null\` and
 * \`env: {home, tmpdir, locale, network, sandboxLevel, path, passthrough} | null\`; every \`checks[]\`
 * entry carries \`outcome\`, \`boundS\`, \`boundNote\`, \`failureIds\`, \`classification: 'regression' |
 * 'pre_existing_in_sandbox' | 'floor_env_mismatch' | null\` (only a regression denies), \`preExisting\`,
 * \`regressions\` and \`base: {head, cached, run, error} | null\` (the same check run on the run base).
 * core#461/#591 (additive): unitDistributed.distinctnessFallback ('creator_seat' |
 * 'same_cli_instance' | null — the evaluator ≠ creator fallback as a field, see
 * UnitDistributedEventJson); gateEscalated.condition
 * gains the class 'dead_seat' (denialSource 'dead_seat'): a worker exited on a dead-seat refusal
 * (signed out / quota / not installed) and no eligible seat remains — the attended run pauses at
 * the escalation gate instead of failing.
 * core#549 (additive): gateEscalated gains \`verdictSummaryTrimmed\` (boolean) — true when the
 * evaluator's output exceeded EVALUATOR_FINDINGS_CAP chars and \`verdictSummary\` is a tail-trim
 * (the full text was too long; the trimmed portion is marked with a leading '…'). WorkUnit
 * gains \`rework_amendment\` (string | absent) — the full findings+note text stored at request_changes
 * so the creator's re-dispatch receives the amendment whole, never capped; unitContextInjected's
 * \`outputBytes\` per item reflects the full amendment byte length.
 * DES-TEAMING-001 S2 (#601, additive — three new \`type\` values, no existing shape changes):
 * unitCheckpoint {session, ord, attempt, seq, toolCallId, kind, title, status: 'completed' |
 * 'failed', paths} (a TEAMED unit's ACP tool call reached a terminal status; \`kind\` is the ACP
 * ToolKind, 'other' when absent); monitorAttached {session, ord, attempt, monitorId, seat, status:
 * 'attached' | 'failed', reason, error: string | null}; monitorFinding {session, ord, attempt,
 * findingId, monitorId, seat, severity: 'high' | 'medium', path, line, evidence, claim,
 * suggestion: string | null, tree, inDiff, checkpointSeq} (a read-only monitor's finding whose
 * \`evidence\` IS line \`line\` of \`path\` in snapshot tree \`tree\` — advisory, never a verdict).
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
  /** How the seat was chosen: \`'teamed'\` — the deterministic pick, no council (core#590 S5,
   * what every seated unit gets now) — an evaluator ≠ creator reassignment, or a deterministic
   * tool execution. \`'council'\` and \`'degraded'\` are no longer emitted by a new run; they stay
   * in the set because a recorded run's replayed frames carry them. */
  routingMethod: 'council' | 'degraded' | 'evaluator_distinct' | 'tool' | 'teamed'
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
  /**
   * (core#461, core#591) The evaluator ≠ creator DISCLOSURE as a field. \`null\` when there is
   * nothing to disclose. Two values:
   *
   * - \`'creator_seat'\` — a review/test unit STAYS on a seat that built what it checks because no
   *   eligible seat distinct from the builders admits it.
   * - \`'same_cli_instance'\` — the unit IS on a seat distinct from every builder seat, but that
   *   seat runs the SAME cli as a builder (\`claude#2\` grading \`claude#1\`: two seat INSTANCES of
   *   one cli). Instance distinctness removes the creator's CONTEXT, not the model's blind spots
   *   — same weights, same failure modes. Read it as a model-distinct evaluator and you are
   *   accepting a degraded gate, so it is on the wire. \`'creator_seat'\` dominates when both
   *   would apply.
   *
   * The paragraph below is about \`'creator_seat'\` only. The roster is always BENCH-FREE when it
   * is set (core#560/#567): a bench that leaves a review/test unit no distinct seat REFUSES the
   * plan instead, so that case never reaches the wire. Three shapes set it — a one-seat roster; a
   * roster whose every seat was assigned a Build/Recon unit; and one whose only non-builder seats
   * the unit's skills refuse. \`degradedReason\` does NOT name either value: the field is the
   * disclosure. The seat the unit stays on is therefore always a still-eligible one. Additive.
   */
  distinctnessFallback: 'creator_seat' | 'same_cli_instance' | null
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
