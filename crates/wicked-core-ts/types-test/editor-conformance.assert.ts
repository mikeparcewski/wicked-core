// Crew's install gate uses this static binding; it accepts and returns JSON strings.
import type { Core } from '../index'

type Equal<A, B> = (<T>() => T extends A ? 1 : 2) extends <T>() => T extends B ? 1 : 2
  ? true
  : false

const requestIsJson: Equal<Parameters<typeof Core.runEditorConformance>, [requestJson: string]> = true
const resultIsAsyncJson: Equal<ReturnType<typeof Core.runEditorConformance>, Promise<string>> = true

// core#1708: the host's shutdown for in-flight install harnesses.
const shutdownTakesNothing: Equal<Parameters<typeof Core.shutdownEditorConformance>, []> = true
const shutdownIsSyncJson: Equal<ReturnType<typeof Core.shutdownEditorConformance>, string> = true

export { requestIsJson, resultIsAsyncJson, shutdownTakesNothing, shutdownIsSyncJson }
