// Compile-time assertions over a domain-extraction run's scope and the domain model's rulings on
// `Core` (J6 C1b, C2): crew's Start and its rulings route call them by these shapes. Nothing here
// runs; `npm run typecheck` compiles it or fails.
import type { Core, LaunchOptions } from '../index'

type Equal<A, B> = (<T>() => T extends A ? 1 : 2) extends <T>() => T extends B ? 1 : 2
  ? true
  : false

const launchTakesScope: Equal<LaunchOptions['scopeJson'], string | undefined> = true
const supports: Equal<ReturnType<typeof Core.supportsDomainScope>, boolean> = true
const ruling: Equal<Parameters<Core['domainRuling']>, [repoRef: string, request: string]> = true
const rulingResult: Equal<ReturnType<Core['domainRuling']>, Promise<string>> = true

export { launchTakesScope, supports, ruling, rulingResult }
