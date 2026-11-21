// Crew's install gate uses this static binding; it accepts and returns JSON strings.
import type { Core } from '../index'

type Equal<A, B> = (<T>() => T extends A ? 1 : 2) extends <T>() => T extends B ? 1 : 2
  ? true
  : false

const requestIsJson: Equal<Parameters<typeof Core.runEditorConformance>, [requestJson: string]> = true
const resultIsAsyncJson: Equal<ReturnType<typeof Core.runEditorConformance>, Promise<string>> = true

export { requestIsJson, resultIsAsyncJson }
