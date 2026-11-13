```
          _      _            _
__      _(_) ___| | _____  __| |       ___ ___  _ __ ___
\ \ /\ / / |/ __| |/ / _ \/ _` |_____ / __/ _ \| '__/ _ \
 \ V  V /| | (__|   <  __/ (_| |_____| (_| (_) | | |  __/
  \_/\_/ |_|\___|_|\_\___|\__,_|      \___\___/|_|  \___|

```

# wicked-core

**The engine behind [wicked-crew](https://github.com/mikeparcewski/wicked-crew).** It runs jobs
defined as data: it plans the steps, decides which agent builds and, by default, which different
agent reviews, runs each agent in its sandbox, runs the repository's detected or configured checks,
and records every verdict.

In power mode the code-writing build step runs as one agent's checkpointed turns while the engine
re-scores the diff (other required steps can run separately); a failed review or qualifying check
failure goes back to that agent, up to three hand-off attempts in all, and problems that need the
operator's judgment pause for them. It also builds a repository's requirements graph and measures
real coverage against a minimum. Underneath, a single-writer store actor owns the SQLite file, so
the daemon, the UI and the MCP servers compose through one command API and a live event stream
without racing on the database. Not end-user-facing: wicked-crew drives it through the
`wicked-core-ts` bindings.

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
wicked-studio's Steering page (Govern → Steering) and test it on Evaluate (Insight → Evaluate),
CI comments with it on PRs. Start here:
**[crates/wicked-governance/STEERING.md](./crates/wicked-governance/STEERING.md)** — the
operator guide from "never heard of it" to seeded and recalling.

## Built-in workflows

Workflows are data: a `WorkflowDef` (JSON) names its phases, and a preset is a fixed plan over the
phase catalog. The engine seeds its built-in presets into the store at boot (`builtin_presets` in
[`src/catalog.rs`](./src/catalog.rs); the JSON ones live in [`src/presets/`](./src/presets/)), and a
launch names one by its id. A preset that replaced an older workflow def kept that def's id, so a
launch naming it keeps working (`demo` is the exception: it replaced `interactive-demo` and
`interactive-demo-reauthor`):

`analyze`, `apps-publish`, `apps-turn`, `bug`, `capture-learnings`, `chat`, `demo`, `docs`,
`domain-extraction`, `editor-plugin`, `feature`, `interactive-chat`, `interactive-draft`,
`interactive-edit`, `learn`, `mcp-server`, `migration`, `onboarding`, `qe-author-tests`,
`steering-author`, `test-analysis`, `test-plan`, `validate-docs`.

The rules a built-in carries with it:

- **QE acceptance.** The presets that change the application (`feature`, `bug`, `editor-plugin`,
  `mcp-server`, `migration`) and `apps-publish` require QE acceptance on top of the default
  instruments (`builtin_preset_instruments`). An Apps publish's QE is forced (no score, launch
  word or gate skip waives it), and its QE seat must not have built any turn since the last
  publish.
- **Apps runs (D-7).** `apps-turn` (build → check → review) and `apps-publish` (check → qe →
  produce) are the runs behind wicked-crew's Apps. Every unit of either is refused any MCP tool
  whose class is `write` or `destructive` (a tool with no hints classes as `write`; an operator's
  override wins), registered or not, in every mode; the deny's rule id is
  `engine:apps-no-source-tool`. A read goes on to the gates every unit passes, and crew's data
  gate decides what it returns. An `apps-turn` has no QE unit: its instrument is the review, on a
  seat distinct from the builder.
- **Answer-only steps.** A one-shot answer step may declare `answer_only` (a `PhaseDef` and
  plan-step field): its answer must exist and be non-blank, instead of meeting the 200-character
  substance floor. `WorkflowDef::validate` accepts it only on the def's last phase: an agent step
  the PA owns, not an evaluator, that executes no code, has no validator pin and a pool of one.

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
`uncounted` with the reason a row is not in the share; a row persisted before requirements recorded
their code unit has no entry, which means unknown until a rebuild).

## Platform support

macOS is the primary platform. Linux is supported. **Windows is not supported for governed runs**
— it runs, and then it denies the verify gate on every one of them. The engine runs a repository's
own check scripts (the `repo_checks` floor behind `bug/verify`, `feature/test`,
`migration/verify` and crew's served mirrors) only inside a write boundary, because those
scripts are code the repository chose; there is no boundary the engine arms on a Windows host, so
the floor fails closed and the gate denies (core#416).

| Host | Where the repository's checks run | The verify floor | Governed runs |
|---|---|---|---|
| **macOS** | a fresh Linux pod per floor side on the local Kubernetes cluster when the floor can be prepared there; else `sandbox-exec` (base system), with the reason | runs the checks (`repoChecksEvaluated`'s `floorRunner.runner`: `container`, or `macos-sandbox` with its `fallbackReason`) | primary |
| **Linux** | `bwrap` — `apt install bubblewrap` / `dnf install bubblewrap` | runs the checks with `bwrap`; **without it, denies** | supported |
| **Windows** (incl. Git Bash / WSL host side) | none | **denies every floor phase, by construction** | not supported |

`WICKED_FLOOR_RUNNER` picks the floor: `auto` (the default) tries pods on macOS only,
`container` asks for it on any host and refuses the floor when it cannot start, `sandbox` keeps the
host floor. The checks pod has no network, a read-only root, every capability dropped and a memory
cap (`WICKED_FLOOR_MEMORY`, default `6g`); the tree is copied in, and a check that rewrites a
tracked file stands for nothing. A check killed for memory, or a pod that stops answering,
is "could not run", never a failure of the change. The image is `containers/checks/node22-rust.Dockerfile`,
built on first use. `wicked-core floor run --repo <dir> --base <commit> [--full]` runs one floor and
prints its report (which floor ran is its `runner` field). Each copied side carries a private
`.git` (HEAD the side, `origin/<default>` the run base, no other history). The container cannot run
everything: an install other than `npm ci`, a build guide that reads results from a file, or a
check whose command the image lacks sends `auto` back to the host floor. Only each check's own
command is probed: a tool a script calls inside the container
(`go test` from an npm script) fails there, so such a repository sets `WICKED_FLOOR_RUNNER=sandbox`.

The escape hatch is an opt-in, not a default: `WICKED_REPO_CHECKS_UNSANDBOXED=1` runs the
repository's own checks with **no OS write boundary**. The results are real; the pass is not
containment evidence, and every report, gate note and `repoChecksEvaluated` event says so
(`sandbox_level: "none"`). A real Windows boundary (AppContainer / a restricted-token job object,
or the checks under WSL2 `bwrap`) is not designed and not shipped.

**Seats run as Kubernetes pods too.** Governed seats and the checks floor run on a local
Kubernetes cluster (Rancher Desktop or Docker Desktop Kubernetes), installed once with
`wicked-core cluster install --admin-context <ctx> --home <state home>`. Seats use the trusted
floor launcher, a read-only root and the model gateway. A unit whose pod cannot start is refused;
`WICKED_SEAT_FLOOR=host` keeps the labelled interim host floor. The container CLI (`docker`,
`nerdctl` or `podman`) never runs a seat: it builds images into the store the cluster shares
(`imagePullPolicy: Never`) and inspects a seat image before an arm. `wicked-core seat-image build|status` builds or reports the images.
Every default seat image includes the checks floor recipe as a named stage (`containers/checks/node22-rust.Dockerfile`:
node, the pinned Rust toolchain linked into `/usr/local/bin`, git, a C toolchain), which the engine
builds first (and again if an image cleanup removed it), so a builder or QE seat on a Rust repository
has `cargo`, and a repository `rust-toolchain` file is not applied (the floor's pinned toolchain is
used). The seat build is self-contained so BuildKit OCI workers need no local base image.
`WICKED_SEAT_BASE_IMAGE` still overrides the base. Every container seat unit's `CARGO_HOME` uses a
per-run, per-CLI dir in the replica, neither seeded from the host nor written back. A full-posture
unit's `CARGO_TARGET_DIR` uses that dir too; a fenced unit keeps its per-unit `scratch/target`,
removed at settle.

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
