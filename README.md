# wicked-core

**The runtime that makes wicked-estate concurrency-safe.**

wicked-core is the in-process composition runtime for wicked-estate's services. A single-writer
store actor owns the SQLite file on one thread while the agent, UI, and MCP servers compose through
a shared command API and a live event stream — so consumers stop re-opening, racing on, and polling
the shared database. It is also being grown into the agentic-CLI orchestrator engine
(recon → adversarial-review → functional-test) that [wicked-crew](https://github.com/mikeparcewski/wicked-crew)
drives.

> **Status:** design/active. **v0.1.0, unpublished** — and structurally unpublishable to crates.io
> today (a path dependency on the unpublished estate 0.13, plus four vendored engine crates marked
> `publish = false`). The orchestrator build is mid-flight (P0/P1/P1.5 done and green; the CI gate
> has landed). Not end-user-facing.

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
- **napi-rs Node/TS bindings** (`wicked-core-ts`) so JS/TS callers — the crew daemon, the studio UI —
  drive runs and consume the event stream.

## Audience

Internal. The consumers are the other wicked-* products — the [wicked-crew](https://github.com/mikeparcewski/wicked-crew)
daemon, the wicked-studio UI, and the MCP servers — that compose
[wicked-estate](https://github.com/mikeparcewski/wicked-estate).

## The foundation

wicked-core is the **runtime** of the [wicked-* foundation](https://we.wickedagile.com): a
local-first stack for AI coding agents anchored by
[wicked-estate](https://github.com/mikeparcewski/wicked-estate) (the code graph), with
[wicked-bus](https://github.com/mikeparcewski/wicked-bus) (the event substrate),
[wicked-brain](https://github.com/mikeparcewski/wicked-brain) (memory), and
[wicked-crew](https://github.com/mikeparcewski/wicked-crew) (the workflow governor).

## License

MIT © Michael Parcewski <mike.parcewski@gmail.com> — see [LICENSE](./LICENSE).
