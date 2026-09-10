// Compile-time assertions over the hand-authored event declarations in `index.d.ts`
// (`scripts/finalize-dts.mjs` is their source of record) — core#401, review pass 2 on #402.
//
// Nothing here runs: `npm run typecheck` (`tsc --noEmit -p types-test/tsconfig.json`) either
// compiles this file or fails, and every `@ts-expect-error` below fails the build if the line it
// guards STOPS being an error (i.e. the contract loosened). `skipLibCheck` is on because the
// napi-generated half of `index.d.ts` names Node's `Buffer` (this package declares no `@types/node`);
// the declarations under test are still resolved and checked HERE.
import type { CoreEventJson, UnitDistributedEventJson } from '../index'

/** `true` iff `A` and `B` are the same type (mutual assignability under conditional-type identity). */
type Equal<A, B> = (<T>() => T extends A ? 1 : 2) extends <T>() => T extends B ? 1 : 2
  ? true
  : false

// `seatConstraint` is a REAL property, typed `string | null` — not `unknown` through the index
// signature, not optional.
const seatConstraintIsStringOrNull: Equal<UnitDistributedEventJson['seatConstraint'], string | null> =
  true
// The event is discriminated by its literal tag.
const tagIsLiteral: Equal<UnitDistributedEventJson['type'], 'unitDistributed'> = true
// The engine's `Option` fields arrive as `null`, never absent.
const degradedReasonIsStringOrNull: Equal<UnitDistributedEventJson['degradedReason'], string | null> =
  true
const seatedIsNumberOrNull: Equal<UnitDistributedEventJson['seated'], number | null> = true

// Reading a parsed event: narrow on the tag, then the field is `string | null`.
declare const parsed: CoreEventJson
if (parsed.type === 'unitDistributed') {
  const d = parsed as UnitDistributedEventJson
  const constraint: string | null = d.seatConstraint
  const method: 'council' | 'degraded' | 'evaluator_distinct' | 'tool' = d.routingMethod
  void constraint
  void method
  // @ts-expect-error — never `undefined`: emitted unconditionally, `null` when unconstrained
  const absent: undefined = d.seatConstraint
  // @ts-expect-error — not a number
  const numeric: number = d.seatConstraint
  void absent
  void numeric
}

// Additive: a `UnitDistributedEventJson` IS a `CoreEventJson`, so consumers reading the base shape
// keep working unchanged.
declare const distributed: UnitDistributedEventJson
const asBase: CoreEventJson = distributed
void asBase

// A literal must carry `seatConstraint` — a producer cannot silently omit it.
// @ts-expect-error — `seatConstraint` is required
const omitted: UnitDistributedEventJson = {
  type: 'unitDistributed',
  session: 'run-1',
  ord: 1,
  cli: 'claude',
  routingMethod: 'council',
  agreementPct: 100,
  returned: 1,
  seated: 1,
  dissent: 0,
  degradedReason: null,
}
const complete: UnitDistributedEventJson = { ...omitted, seatConstraint: null }
const constrained: UnitDistributedEventJson = {
  ...complete,
  seatConstraint: 'the skills snapshot marks wicked-garden-repo-learn as portable: false',
}
// @ts-expect-error — an unknown routing method is not part of the contract
const unknownMethod: UnitDistributedEventJson = { ...complete, routingMethod: 'ranked' }

void seatConstraintIsStringOrNull
void tagIsLiteral
void degradedReasonIsStringOrNull
void seatedIsNumberOrNull
void constrained
void unknownMethod
