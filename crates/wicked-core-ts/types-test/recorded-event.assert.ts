// Compile-time assertions over `RecordedEventJson` — the shape of one entry in the array
// `Core.runEvents` resolves (`scripts/finalize-dts.mjs` is its source of record) — wicked-core#408.
//
// Nothing here runs: `npm run typecheck` (`tsc --noEmit -p types-test/tsconfig.json`) either
// compiles this file or fails, and every `@ts-expect-error` below fails the build if the line it
// guards STOPS being an error (i.e. the contract loosened).
import type { CoreEventJson, RecordedEventJson } from '../index'

/** `true` iff `A` and `B` are the same type (mutual assignability under conditional-type identity). */
type Equal<A, B> = (<T>() => T extends A ? 1 : 2) extends <T>() => T extends B ? 1 : 2
  ? true
  : false

// The envelope is REAL, required, numeric — not `unknown` through the index signature. `seq` is
// the ordering consumers take the tail of; `ts` is capture time and may repeat.
const seqIsNumber: Equal<RecordedEventJson['seq'], number> = true
const tsIsNumber: Equal<RecordedEventJson['ts'], number> = true
// The restart marker is `true` or absent — never `false`, never any other value.
const markerIsTrueOrAbsent: Equal<RecordedEventJson['daemonRestarted'], true | undefined> = true

// Additive: a recorded event IS a `/ws` frame, so consumers reading the base shape keep working.
declare const recorded: RecordedEventJson
const asFrame: CoreEventJson = recorded
void asFrame

// A literal must carry the envelope — a producer cannot silently omit it.
// @ts-expect-error — `seq` is required
const noSeq: RecordedEventJson = { type: 'unitDone', session: 'run-1', ord: 1, ts: 1 }
// @ts-expect-error — `ts` is required
const noTs: RecordedEventJson = { type: 'unitDone', session: 'run-1', ord: 1, seq: 0 }
const plain: RecordedEventJson = { type: 'unitDone', session: 'run-1', ord: 1, ts: 1, seq: 294 }
// The first record a restarted engine writes for the run, continuing the run's `seq`.
const afterRestart: RecordedEventJson = { ...plain, type: 'resumed', seq: 295, daemonRestarted: true }
// @ts-expect-error — the marker is never spelled `false`; it is simply absent
const falseMarker: RecordedEventJson = { ...plain, daemonRestarted: false }

// The consumer contract in one line: the tail of the history is the latest event.
declare const history: RecordedEventJson[]
const latest: RecordedEventJson | undefined = history[history.length - 1]
void latest

void seqIsNumber
void tsIsNumber
void markerIsTrueOrAbsent
void noSeq
void noTs
void afterRestart
void falseMarker
