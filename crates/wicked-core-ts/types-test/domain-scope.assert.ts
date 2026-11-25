// Compile-time assertions over a domain-extraction run's scope on `Core` (J6 C1b): crew's Start
// sends it by these shapes. Nothing here runs; `npm run typecheck` compiles it or fails.
import type { Core, LaunchOptions } from '../index'

type Equal<A, B> = (<T>() => T extends A ? 1 : 2) extends <T>() => T extends B ? 1 : 2
  ? true
  : false

const launchTakesScope: Equal<LaunchOptions['scopeJson'], string | undefined> = true
const supports: Equal<ReturnType<typeof Core.supportsDomainScope>, boolean> = true

export { launchTakesScope, supports }
