// Compile-time assertions over power mode on `Core` (program DES-POWER-MODE): crew calls these by
// these shapes. Nothing here runs; `npm run typecheck` compiles it or fails.
import type { Core, LaunchOptions } from '../index'

type Equal<A, B> = (<T>() => T extends A ? 1 : 2) extends <T>() => T extends B ? 1 : 2
  ? true
  : false

const launchTakesPower: Equal<LaunchOptions['power'], boolean | undefined> = true
const supports: Equal<ReturnType<typeof Core.supportsPowerMode>, boolean> = true
const setPower: Equal<
  Parameters<Core['setPowerMode']>,
  [runId: string, on: boolean, actor: string, reason?: string | undefined | null]
> = true
const raise: Equal<Parameters<Core['raiseRigor']>, [runId: string, atLeast: string, actor: string, reason: string]> = true
const pause: Equal<
  Parameters<Core['answerPowerPause']>,
  [runId: string, decision: string, note: string | undefined | null, actor: string]
> = true
const answersJson: Equal<ReturnType<Core['setPowerMode']>, Promise<string>> = true

export { launchTakesPower, supports, setPower, raise, pause, answersJson }
