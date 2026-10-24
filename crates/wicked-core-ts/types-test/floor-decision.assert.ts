// Compile-time assertions over the DES-FLOOR-V2 decision on `repoChecksEvaluated.checks[]`
// (`scripts/finalize-dts.mjs` is its source of record). Nothing here runs: `npm run typecheck`
// compiles this file or fails, and every `@ts-expect-error` fails the build if the line it guards
// STOPS being an error (the contract loosened).
import type {
  CoreEventJson,
  FloorCheckDecisionJson,
  FloorLogRefJson,
  RepoChecksEvaluatedEventJson,
} from '../index'

type Equal<A, B> = (<T>() => T extends A ? 1 : 2) extends <T>() => T extends B ? 1 : 2
  ? true
  : false

// The outcome is the closed set the engine produces — a consumer can switch on it exhaustively.
const outcomes: Equal<
  FloorCheckDecisionJson['outcome'],
  'pass' | 'pass_excluded' | 'new_failure' | 'base_red_unseparated' | 'flaky_twice' | 'timed_out' | 'could_not_run'
> = true
// The engine's Option fields arrive as null, never absent.
const baseShaNullable: Equal<FloorCheckDecisionJson['baseSha'], string | null> = true
const decisionNullable: Equal<RepoChecksEvaluatedEventJson['checks'][number]['decision'], FloorCheckDecisionJson | null> = true
const refHash: Equal<FloorLogRefJson['sha256'], string> = true

// Additive: the typed frame IS a CoreEventJson.
declare const frame: RepoChecksEvaluatedEventJson
const asFrame: CoreEventJson = frame
void asFrame

function passes(d: FloorCheckDecisionJson): boolean {
  switch (d.outcome) {
    case 'pass':
    case 'pass_excluded':
      return true
    case 'new_failure':
    case 'base_red_unseparated':
    case 'flaky_twice':
    case 'timed_out':
    case 'could_not_run':
      return false
  }
}
void passes

// @ts-expect-error — not an outcome the engine produces
const bogus: FloorCheckDecisionJson['outcome'] = 'pre_existing_in_sandbox'
