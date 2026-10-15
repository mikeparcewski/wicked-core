// Compile-time assertions over the engine's standing QE word on `Core` (`setQeAcceptanceOff` /
// `qeAcceptanceOff`, napi-generated into `index.d.ts`): crew calls them by these shapes. Nothing
// here runs; `npm run typecheck` compiles it or fails.
import type { Core } from '../index'

type Equal<A, B> = (<T>() => T extends A ? 1 : 2) extends <T>() => T extends B ? 1 : 2
  ? true
  : false

// A reason turns it off, `null` turns it back on; it answers nothing.
const setTakesReasonOrNull: Equal<Parameters<Core['setQeAcceptanceOff']>, [reason?: string | undefined | null]> =
  true
const setReturnsVoid: Equal<ReturnType<Core['setQeAcceptanceOff']>, void> = true
// The standing reason, or `null` while QE acceptance is required.
const getIsStringOrNull: Equal<ReturnType<Core['qeAcceptanceOff']>, string | null> = true

export { setTakesReasonOrNull, setReturnsVoid, getIsStringOrNull }
