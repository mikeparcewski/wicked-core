// Node smoke test for the governance-evals bindings (`governanceEvals` / `governanceCorpusImport`).
//
// Proves the built addon carries the two bindings crew's `/api/v1/testing` routes presence-gate
// on, and that they honor the PINNED wire contract end-to-end against temp stores:
//   1. presence sentinels: `typeof core.governanceEvals === 'function'` (crew's 501 gate) — same
//      for `governanceCorpusImport`
//   2. seed one decide-lane policy into a temp rules db THROUGH the single-writer actor
//   3. governanceEvals against that temp rules db + the compiled-in DEFAULT corpus — assert the
//      pinned report fields: results[].{sample{id,description,kind,steering_type}, expected,
//      fired, verdict}, summary{total,caught,gaps,false_positives}, degraded ("facet-only"|null)
//   4. governanceCorpusImport into a temp knowledge db — assert the pinned receipt
//      {imported, scope: "evals:<name>", embedded} (embedded VERIFIED true: hash embedder)
//   5. governanceEvals against the imported `evals:` scope — the seeded deny FIRES for the bad
//      sample (caught) and stays quiet for the good one (caught)
//   6. fail-closed: unknown steering type and a non-`evals:` corpus name both REJECT (crew's 400)
//
// Deterministic + fast + offline: temp dbs only (NEVER ~/.wicked-estate), hash embedders, no LLM.

import { createRequire } from 'node:module'
import { fileURLToPath } from 'node:url'
import { dirname, join } from 'node:path'
import { tmpdir } from 'node:os'
import fs from 'node:fs'

// Offline: force the dependency-free lexical memory embedder (no Model2Vec download on spawn).
process.env.WICKED_MEMORY_EMBEDDER = 'hash'

const __dirname = dirname(fileURLToPath(import.meta.url))
const require = createRequire(import.meta.url)

function loadAddon() {
  const entry = join(__dirname, 'index.js')
  if (!fs.existsSync(entry)) {
    throw new Error('index.js not found — run `npm run build` (napi build --platform --release) first')
  }
  return entry
}

const addonPath = loadAddon()
const { Core } = require(addonPath)

const assert = (cond, msg) => {
  if (!cond) throw new Error(`assertion failed: ${msg}`)
}

async function rejects(promise, needle, label) {
  try {
    await promise
  } catch (e) {
    assert(
      String(e.message ?? e).includes(needle),
      `${label}: rejection names the problem (wanted ${JSON.stringify(needle)}, got: ${e.message ?? e})`,
    )
    return
  }
  throw new Error(`assertion failed: ${label} must reject`)
}

/// The pinned per-row report fields (crew passes these through verbatim, snake_case).
function checkReportShape(report, label) {
  assert(Array.isArray(report.results), `${label}: results is an array`)
  const s = report.summary
  for (const k of ['total', 'caught', 'gaps', 'false_positives']) {
    assert(typeof s[k] === 'number', `${label}: summary.${k} is a number`)
  }
  assert(
    s.total === s.caught + s.gaps + s.false_positives,
    `${label}: total = caught + gaps + false_positives`,
  )
  assert(s.total === report.results.length, `${label}: one summary count per result row`)
  assert(
    report.degraded === null || report.degraded === 'facet-only',
    `${label}: degraded is ALWAYS "facet-only" | null, got ${JSON.stringify(report.degraded)}`,
  )
  for (const row of report.results) {
    for (const k of ['id', 'description', 'kind', 'steering_type']) {
      assert(typeof row.sample?.[k] === 'string', `${label}: results[].sample.${k} is a string`)
    }
    assert(['good', 'bad'].includes(row.sample.kind), `${label}: sample.kind is good|bad`)
    assert(['deny', 'allow'].includes(row.expected), `${label}: expected is deny|allow`)
    assert(Array.isArray(row.fired), `${label}: fired is an array of rule ids`)
    assert(
      ['caught', 'gap', 'false_positive'].includes(row.verdict),
      `${label}: verdict is caught|gap|false_positive`,
    )
    if (row.verdict === 'gap') {
      assert(Array.isArray(row.nearest_rules), `${label}: gaps carry nearest_rules (may be empty)`)
      for (const hint of row.nearest_rules) {
        assert(typeof hint.rule_id === 'string', `${label}: nearest_rules[].rule_id`)
        assert(typeof hint.similarity === 'number', `${label}: nearest_rules[].similarity`)
      }
    }
  }
}

async function main() {
  const dir = fs.mkdtempSync(join(tmpdir(), 'wicked-core-ts-evals-'))
  const dbPath = join(dir, 'core.db')
  const knowledgeDb = join(dir, 'knowledge.db')
  console.log(`[smoke-evals] db: ${dbPath}`)
  console.log(`[smoke-evals] addon: ${addonPath}`)

  const core = Core.spawn(dbPath)

  // 1. The presence sentinels crew's 501 gate reads.
  assert(typeof core.governanceEvals === 'function', 'core.governanceEvals is present')
  assert(typeof core.governanceCorpusImport === 'function', 'core.governanceCorpusImport is present')

  // 2. Seed one decide-lane policy through the single-writer actor: deny `push --force` in build.
  await core.upsertPolicy(
    JSON.stringify({
      id: 'GOV-FORCE-PUSH',
      kind: 'development',
      applies_to: ['build'],
      effect: 'deny',
      trigger: { contains: 'push\\s+--force' },
      criteria: 'history on shared branches is append-only',
      severity: 'high',
      rule: 'never force-push a shared branch',
    }),
  )

  // 3. Eval the compiled-in DEFAULT corpus (no `corpus` key) against the temp rules db.
  const defaultReport = JSON.parse(
    await core.governanceEvals(JSON.stringify({ dbPath, knowledgeDb })),
  )
  checkReportShape(defaultReport, 'default corpus')
  assert(defaultReport.summary.total > 0, 'the built-in corpus is non-empty')
  console.log(
    `[smoke-evals] default corpus: total=${defaultReport.summary.total} caught=${defaultReport.summary.caught} gaps=${defaultReport.summary.gaps} false_positives=${defaultReport.summary.false_positives} degraded=${JSON.stringify(defaultReport.degraded)}`,
  )

  // …and a `type` slice stays inside the pinned shape (every row is that steering type).
  const sliced = JSON.parse(
    await core.governanceEvals(JSON.stringify({ type: 'development', dbPath, knowledgeDb })),
  )
  checkReportShape(sliced, 'type-sliced corpus')
  for (const row of sliced.results) {
    assert(row.sample.steering_type === 'development', 'type slices the corpus')
  }

  // 4. Import a 2-sample corpus into the temp knowledge db — pinned receipt shape.
  const receipt = JSON.parse(
    await core.governanceCorpusImport(
      JSON.stringify({
        name: 'smoke',
        knowledgeDb,
        samples: [
          {
            id: 'dev-force-push',
            description: 'force-push to a shared branch',
            kind: 'bad',
            steering_type: 'development',
            signals: { phase: 'build', tool: 'Bash', content: 'git push --force origin main' },
          },
          {
            id: 'dev-small-pr',
            description: 'a plain feature-branch push',
            kind: 'good',
            steering_type: 'development',
            signals: { phase: 'build', tool: 'Bash', content: 'git push origin fix/null-guard' },
          },
        ],
      }),
    ),
  )
  assert(receipt.imported === 2, `receipt.imported is 2, got ${receipt.imported}`)
  assert(receipt.scope === 'evals:smoke', `receipt.scope is "evals:smoke", got ${receipt.scope}`)
  assert(receipt.embedded === true, 'receipt.embedded is VERIFIED true (hash embedder stores vectors)')
  console.log(`[smoke-evals] import: ${JSON.stringify(receipt)}`)

  // 5. Eval the imported `evals:` scope — the seeded deny fires through the REAL gate path.
  const scoped = JSON.parse(
    await core.governanceEvals(JSON.stringify({ corpus: 'evals:smoke', dbPath, knowledgeDb })),
  )
  checkReportShape(scoped, 'imported corpus')
  assert(scoped.summary.total === 2, 'both imported samples evaluated')
  assert(scoped.summary.caught === 2, 'bad denied + good allowed are both caught')
  const byId = Object.fromEntries(scoped.results.map((r) => [r.sample.id, r]))
  assert(byId['dev-force-push'].expected === 'deny', 'bad sample expects deny')
  assert(byId['dev-force-push'].verdict === 'caught', 'the deny fired → caught')
  assert(
    byId['dev-force-push'].fired.length === 1 && byId['dev-force-push'].fired[0] === 'GOV-FORCE-PUSH',
    `the seeded policy fired, got ${JSON.stringify(byId['dev-force-push'].fired)}`,
  )
  assert(byId['dev-small-pr'].expected === 'allow', 'good sample expects allow')
  assert(byId['dev-small-pr'].verdict === 'caught', 'nothing fired for the good sample → caught')
  assert(byId['dev-small-pr'].fired.length === 0, 'good sample fired nothing')
  console.log(`[smoke-evals] evals:smoke: ${JSON.stringify(scoped.summary)}`)

  // 6. Fail-closed (crew's 400 lane): bad inputs REJECT, never an empty report.
  await rejects(
    core.governanceEvals(JSON.stringify({ type: 'archtecture', dbPath, knowledgeDb })),
    'unknown steering type',
    'typo’d steering type',
  )
  await rejects(
    core.governanceEvals(JSON.stringify({ corpus: 'smoke', dbPath, knowledgeDb })),
    'evals:',
    'non-scope corpus name',
  )
  await rejects(
    core.governanceCorpusImport(JSON.stringify({ name: 'smoke', samples: [], knowledgeDb })),
    'no samples',
    'empty sample batch',
  )

  console.log('[smoke-evals] OK — governanceEvals + governanceCorpusImport honor the pinned wire contract')
}

main().then(
  () => process.exit(0),
  (e) => {
    console.error(`[smoke-evals] FAILED: ${e.stack ?? e}`)
    process.exit(1)
  },
)
