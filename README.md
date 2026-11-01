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

## Requirements graph

wicked-core also builds a repository's **requirements graph**: the business rules garden's
extraction stated on the code, grouped into domains by package directory and saved into the
repository's estate store (domain, requirement and business-rule nodes, `SatisfiedBy` edges to
the code). crew reads it from there; nothing is written into the checkout.

The graph is held to a minimum share of **real** requirements: behaviours in product code with a
validated requirement or a business rule at or above the confidence threshold (0.75 by default).
A placeholder (a behaviour that meets neither: a risk flag, a rule under the threshold, or an
unvalidated requirement alone) stays in the graph and never counts. A requirement's `review` status
is not that test: an unvalidated requirement whose rule clears the threshold still counts. What is product code is decided by file path:
test, spec and fixture files and data, manifest and document files are left out (`not_product`),
though a test module inside a product file still counts, and an extracted rules-engine rule outside
test and fixture paths counts whatever its file extension. A TypeScript interface counts only when the graph shows it does something.
Below the minimum the build fails closed and says why: "23% real requirements, minimum 80% (1362
of 5786 behaviours; 4397 placeholders, 27 unmapped)". The mapping done so far stays in the store.

Each command reads the store `--db` names, else `WICKED_COVERAGE_DB`, else `WICKED_ESTATE_DB`, else
`./wicked-estate.db`, so point it at the repository's estate store:

- `wicked-core coverage --db <store> [--min-coverage-pct [N]]` reports the share. With
  `--min-coverage-pct` it exits 3 below the minimum (N, or with no value the minimum in force).
- `wicked-core domain-graph --db <store>` builds and saves the graph (`--out` also writes a JSON
  export).
- `wicked-core requirements-target --db <store> --set N | --clear` keeps a repository's own minimum
  in its store, ahead of the daemon-wide one (`WICKED_REQUIREMENTS_MIN_COVERAGE_PCT`, default 80).

crew's `domain-extraction` run maps a repository page by page (a bounded loop over its own steps,
`repeat_from`) until the minimum is reached, the operator stops it, or nothing is left to map; a
run that ends its pages below the minimum still meets the fail-closed build. `requirements_graph_for_repo`
(core-ts `getRequirementsGraphForRepo`) returns the graph, the live coverage label, the `build`
record the listed graph came from, and the engine's verdict on each listed row that records its code unit (`rows`: `real`, and
`uncounted` with the reason a row is not in the share; a row from an older build has no entry,
which means unknown until a rebuild).

## Platform support

macOS is the primary platform. Linux is supported. **Windows is not supported for governed runs**
— it runs, and then it denies the verify gate on every one of them. The engine runs a repository's
own check scripts (the `repo_checks` floor behind `bug/verify`, `feature/test`,
`migration/verify` and crew's served mirrors) only inside a write boundary, because those
scripts are code the repository chose; there is no boundary the engine arms on a Windows host, so
the floor fails closed and the gate denies (core#416).

| Host | Where the repository's checks run | The verify floor | Governed runs |
|---|---|---|---|
| **macOS** | a fresh Linux container per floor side when a container runtime answers (`docker`, `podman` or `nerdctl`) and the floor can be prepared there; else `sandbox-exec` (base system), with the reason | runs the checks (`repoChecksEvaluated`'s `floorRunner.runner`: `container`, or `macos-sandbox` with its `fallbackReason`) | primary |
| **Linux** | `bwrap` — `apt install bubblewrap` / `dnf install bubblewrap` | runs the checks with `bwrap`; **without it, denies** | supported |
| **Windows** (incl. Git Bash / WSL host side) | none | **denies every floor phase, by construction** | not supported |

`WICKED_FLOOR_RUNNER` picks the floor: `auto` (the default) tries the container on macOS only,
`container` asks for it on any host and refuses the floor when it cannot start, `sandbox` keeps the
host floor. The container has no network, a read-only root, every capability dropped and a memory
cap (`WICKED_FLOOR_MEMORY`, default `6g`); the tree is copied in, and a check that rewrites a
tracked file stands for nothing. A check killed for memory, or a container that stops answering,
is "could not run", never a failure of the change. The image is `containers/checks/node22-rust.Dockerfile`,
built on first use. `wicked-core floor run --repo <dir> --base <commit> [--full]` runs one floor and
prints its report (which floor ran is its `runner` field). The container cannot run everything: an
install other than `npm ci`, a check that names the base commit (the copy has no history), a build
guide that reads results from a file, or a check whose command the image lacks sends `auto` back to
the host floor. Only each check's own command is probed: a tool a script calls inside the container
(`go test` from an npm script) fails there, so such a repository sets `WICKED_FLOOR_RUNNER=sandbox`.

The escape hatch is an opt-in, not a default: `WICKED_REPO_CHECKS_UNSANDBOXED=1` runs the
repository's own checks with **no OS write boundary**. The results are real; the pass is not
containment evidence, and every report, gate note and `repoChecksEvaluated` event says so
(`sandbox_level: "none"`). A real Windows boundary (AppContainer / a restricted-token job object,
or the checks under WSL2 `bwrap`) is not designed and not shipped.

**Seats run in containers too.** The `wicked-core` CLI (and crew's daemon) run governed seats in a
hardened container per seat by default (`WICKED_SEAT_FLOOR=container`): one image per CLI built
from `containers/seats/seat.Dockerfile`, the engine's own launcher mounted from
`containers/floor.Dockerfile`, a read-only root, and a network whose only way out is the model
gateway. A unit whose container cannot start is refused, never run on the host;
`WICKED_SEAT_FLOOR=host` keeps the labelled interim host floor. `wicked-core seat-image
build|hold|status` builds the images, holds them against a runtime's image cleanup (the `docker`
CLI only), and reports them.

Two smaller consequences of the same rule, worth knowing before you debug them:

- A floor that passed says nothing about the platforms a repository's own CI gates — the checks ran
  on one platform alone (the container's Linux, or the daemon's OS).
- Everything else (planning, councils, chat, documents, the gate ladder) is cross-platform; it is
  the *floors* that have a platform matrix.

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
