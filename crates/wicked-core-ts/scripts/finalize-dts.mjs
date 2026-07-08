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
 * \`unitDistributed\` \`{session, ord, cli}\`, \`awaitingHuman\` \`{session, ord, prompt}\`,
 * \`gateDecided\` \`{session, ord, allow}\`, \`unitDone\`/\`unitExecuting\`/\`resumed\` \`{session, ord}\`,
 * \`sessionCompleted\` \`{session}\`, \`sessionFailed\` \`{session, ord}\`, \`error\` \`{session, message}\`.
 * PTY terminal sessions emit \`terminalOpened\` \`{id, cwd}\`, \`terminalOutput\` \`{id, seq, bytesB64}\`
 * (raw output base64-encoded in \`bytesB64\`), and \`terminalExited\` \`{id, status}\`.
 */
export interface CoreEventJson {
  type: string
  session?: string
  ord?: number
  [k: string]: unknown
}
${END}
`

if (!existsSync(dtsPath)) {
  console.error(`[finalize-dts] ${dtsPath} not found — did \`napi build\` run and emit the type defs?`)
  process.exit(1)
}

let dts = readFileSync(dtsPath, 'utf8')

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
