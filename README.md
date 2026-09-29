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

## Steering

wicked-core also carries **Steering**, the ecosystem's governance surface: one steering-rule
model across seven steering types (architecture, development, security, testing, operations,
compliance, design-ux), authored as frontmattered markdown in git or through the governed
studio/crew surface, projected into the estate graph as recallable, citable rules
(`crates/wicked-governance` owns the schemas, the `wicked-core rules
ingest/fanout/relink/drift/recall/scoreboard/retire` CLI, and the seed corpus). Agents recall it
via the estate MCP's `rules.recall`/`knowledge.recall` (read-only), humans manage it in
wicked-studio's Steering section, CI comments with it on PRs. Start here:
**[crates/wicked-governance/STEERING.md](./crates/wicked-governance/STEERING.md)** — the
operator guide from "never heard of it" to seeded and recalling.

## Platform support

macOS is the primary platform. Linux is supported. **Windows is not supported for governed runs**
— it runs, and then it denies the verify gate on every one of them. The engine runs a repository's
own check scripts (the `repo_checks` floor behind `bug/verify`, `feature/test`,
`migration/verify` and crew's served mirrors) only inside an OS write boundary, because those
scripts are code the repository chose; there is no boundary the engine can arm on Windows, so the
floor fails closed and the gate denies (core#416).

| Host | OS write boundary | The verify floor | Governed runs |
|---|---|---|---|
| **macOS** | `sandbox-exec` (base system) | runs the checks (`sandbox_level: sandboxed`) | primary |
| **Linux** | `bwrap` — `apt install bubblewrap` / `dnf install bubblewrap` | runs the checks with `bwrap`; **without it, denies** | supported |
| **Windows** (incl. Git Bash / WSL host side) | none | **denies every floor phase, by construction** | not supported |

The escape hatch is an opt-in, not a default: `WICKED_REPO_CHECKS_UNSANDBOXED=1` runs the
repository's own checks with **no OS write boundary**. The results are real; the pass is not
containment evidence, and every report, gate note and `repoChecksEvaluated` event says so
(`sandbox_level: "none"`). A real Windows boundary (AppContainer / a restricted-token job object,
or the checks under WSL2 `bwrap`) is not designed and not shipped.

Two smaller consequences of the same rule, worth knowing before you debug them:

- A floor that passed says nothing about the platforms a repository's own CI gates — the checks ran
  on the daemon's OS alone.
- Everything else (planning, councils, chat, documents, the gate ladder) is cross-platform; it is
  the *floor* that has a platform matrix.

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
