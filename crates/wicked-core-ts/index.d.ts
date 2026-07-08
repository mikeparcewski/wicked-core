/* Hand-written TypeScript surface for the wicked-core-ts napi addon.
 * (napi-rs can emit this via `napi build`; kept by hand so a plain `cargo build` suffices.)
 *
 * Every async method returns a Promise. Complex results come back as JSON strings you `JSON.parse`;
 * the shape of each is noted below. The live event stream is delivered to `subscribe`'s callback as
 * one JSON string per CoreEvent.
 */

/** Options for {@link Core.launchRun}. */
export interface LaunchOptions {
  /** Free-text problem, decomposed into ordered work units (split on sentence/`;`/newline). */
  problem: string
  /** Stable session/run id (required). */
  sessionId: string
  /** JSON array of `AgenticCli` seats — the council roster. Use {@link Core.registryRoster}. */
  clisJson: string
  /** `'shared'` (default) | `'isolated'`. */
  entityMode?: string
  /** Human-confirm gate policy: `'none'` (default) | `'all'` | `'before:<ord>'`. */
  humanConfirm?: string
  /** Id of a registered repo to run within (isolated worktree). Omit for a repo-less run. */
  repoRef?: string
}

/**
 * A CoreEvent, delivered as a JSON string to the {@link Core.subscribe} callback. Discriminated on
 * `type`. Fields vary by variant (see wicked-core `CoreEvent`): e.g.
 * `sessionStarted` `{session, problem}`, `unitPlanned` `{session, ord, description}`,
 * `unitDistributed` `{session, ord, cli}`, `awaitingHuman` `{session, ord, prompt}`,
 * `gateDecided` `{session, ord, allow}`, `unitDone`/`unitExecuting`/`resumed` `{session, ord}`,
 * `sessionCompleted` `{session}`, `sessionFailed` `{session, ord}`, `error` `{session, message}`.
 */
export interface CoreEventJson {
  type: string
  session?: string
  ord?: number
  [k: string]: unknown
}

/** A handle to a wicked-core runtime. */
export class Core {
  /** Production engine: real council dispatcher + real wrapped-CLI subprocesses. */
  static spawn(path: string): Core
  /** Deterministic offline engine: stub dispatcher + StubStepRunner (no real LLM). */
  static spawnStub(path: string): Core
  /** Production council roster as a JSON array of `AgenticCli` — pass to `clisJson`. */
  static registryRoster(): string

  /**
   * Subscribe to the live event stream (call before `launchRun`). The callback follows the Node
   * error-first convention: `(err, eventJson)` — `err` is `null` on the normal path, and one JSON
   * string is delivered per event. A throw inside the callback is contained (swallowed), never
   * fatal. Returns a {@link Subscription} — hold it and call `close()`/`unsubscribe()` to stop
   * delivery and let the process exit cleanly.
   */
  subscribe(callback: (err: Error | null, eventJson: string) => void): Subscription

  /** Liveness probe → resolves `"ok"` after the actor acks a Heartbeat. */
  ping(): Promise<string>

  /** Launch an interactive, resumable run → resolves the run id. */
  launchRun(opts: LaunchOptions): Promise<string>
  /** Resume from the persisted cursor → resolves the status token. */
  resumeRun(runId: string): Promise<string>
  /** Resolve a human gate: approve (optional amend) or reject → resolves the status token. */
  confirmGate(runId: string, approve: boolean, amend?: string): Promise<string>
  /** Cancel a run → resolves the status token. */
  cancelRun(runId: string): Promise<string>

  /** Session ids on the store → resolves a JSON `string[]`. */
  sessions(): Promise<string>
  /** Every session + ordered units → resolves a JSON array of `{session, units}`. */
  sessionsDetail(): Promise<string>
  /** A unit's transcript → resolves a JSON value (string, or `null`). */
  workOutput(unitId: string): Promise<string>

  /** Register a git repo → resolves the `RepoEntry` as a JSON object. */
  registerRepo(name: string, rootPath: string): Promise<string>
  /** List registered repos → resolves a JSON array of `RepoEntry`. */
  listRepos(): Promise<string>
}

/**
 * A live event subscription handle returned by {@link Core.subscribe}. Hold it for the lifetime of
 * the subscription; call {@link Subscription.close} (or its alias {@link Subscription.unsubscribe})
 * to stop delivery, tear down the pump thread + callback, and let the process exit cleanly.
 */
export class Subscription {
  /** Stop delivery and release the pump thread + callback. Idempotent. */
  close(): void
  /** Alias for {@link Subscription.close}. */
  unsubscribe(): void
}
