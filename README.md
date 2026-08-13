```
          _      _            _
__      _(_) ___| | _____  __| |       ___ ___  _ __ ___
\ \ /\ / / |/ __| |/ / _ \/ _` |_____ / __/ _ \| '__/ _ \
 \ V  V /| | (__|   <  __/ (_| |_____| (_| (_) | | |  __/
  \_/\_/ |_|\___|_|\_\___|\__,_|      \___\___/|_|  \___|

```

# wicked-core

**The execution engine behind wicked-crew — and the concurrency-safe runtime for wicked-estate.**

wicked-core is the in-process composition runtime that powers [wicked-crew](https://github.com/mikeparcewski/wicked-crew):
workflows-as-data, data-driven planning, skills-driven CLI invocation, and the governed gate ladder.
A single-writer store actor owns the SQLite file on one thread while the agent, UI, and MCP servers
compose through a shared command API and a live event stream — no consumer ever re-opens or races on
the shared DB.

> **Status:** active. **v0.4.0, not published to crates.io** — four internal workspace crates are
> marked `publish = false` (`wicked-apps-core`, `wicked-governance`, `wicked-orchestration`,
> `wicked-council`). The estate path-dep coupling is resolved (now pins published crates.io semver).
> Not end-user-facing — consumed by the wicked-crew daemon via napi-rs bindings.

**The differentiator:** it cleanly separates the *system-of-record* (SQLite, one owning writer
thread) from the *orchestration seam* (a command API + a live event stream), so no consumer ever
re-opens or races on the shared DB.

## Key ideas

- **Single-writer `StoreActor`** — one thread is the sole writer, eliminating in-process
  `SQLITE_BUSY` and read/write races.
- **Live event stream** via `subscribe()` — consumers watch `CoreEvent`s instead of polling the DB
  on a timer.
- **Capability-driven concurrency** — a single-writer actor for SQLite, a connection pool for
  Postgres, the same command/event API across both backends.
- **One composition surface** for plan → distribute → execute → evidence, plus cross-platform
  PTY terminal sessions streamed as events.
- **napi-rs Node/TS bindings** (`wicked-core-ts`) so JS/TS callers — the crew daemon and
  [wicked-studio](https://github.com/mikeparcewski/wicked-studio) (the Studio HITL UI, now its own repo) — drive runs and consume the event stream.

## Audience

Internal. The consumers are the other wicked-* products — the [wicked-crew](https://github.com/mikeparcewski/wicked-crew)
daemon (via napi-rs bindings; the Studio UI lives in the separate [wicked-studio](https://github.com/mikeparcewski/wicked-studio) repo), and the MCP servers — that compose
[wicked-estate](https://github.com/mikeparcewski/wicked-estate).

## The foundation

wicked-core is the **execution engine** of the [wicked-* foundation](https://wickedagile.com): a
local-first stack for AI coding agents anchored by
[wicked-estate](https://github.com/mikeparcewski/wicked-estate) (the code graph + memory + knowledge), with
[wicked-bus](https://github.com/mikeparcewski/wicked-bus) (the event substrate), and
[wicked-crew](https://github.com/mikeparcewski/wicked-crew) (the workflow governor, which drives this engine).

## License

MIT © Michael Parcewski <mike.parcewski@gmail.com> — see [LICENSE](./LICENSE).
