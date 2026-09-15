# Changelog

All notable changes to `wicked-core`. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versions follow [SemVer](https://semver.org/).

Two release tracks share this file, newest entry first regardless of track:

- **engine** — the root Rust crate (git tags `vX.Y.Z`; not on crates.io — see ISS-010 below).
  Headings: `[X.Y.Z]`.
- **core-ts** — the npm binding [`wicked-core-ts`](https://www.npmjs.com/package/wicked-core-ts)
  (git tags `core-ts-vX.Y.Z`), which bundles the engine at its release commit. Headings:
  `[core-ts X.Y.Z]`. An npm release therefore ships engine changes even when the engine
  version number does not move.

## [Unreleased]

- **Build fix: collapse the duplicate `"tool_call"` arm in the ACP `sessionUpdate` handler (#524 × #525 unreachable-pattern collision; main red at `-D warnings`).** #525 (L5) added `"tool_call" => { *answer_from = … }` and #524 (L4) added `"tool_call" | "tool_call_update" => { … }`; composed on main the first shadows the second, so `-D unreachable-patterns` failed the lib compile on all three OS legs. The two arms are merged into one — `*answer_from` still advances on `tool_call` only (never on an update frame, exactly as #525 shipped) and #524's failed-tool-call recording and update-frame `locations` collection run unchanged. No behaviour change.

- **Chat seats are handed the skills a unit gets; the turn budget is named; `chatReply.usage`; the
  chat boundary reads the seat's store pin (DES-L5 wave 1 "chat first", journey P6; core #487 +
  crew #563 core half, core #412 chat half, F-RC1-110/113/116).** `chat_ensure` handed every seat
  `SkillsDelivery::None` ("a chat is not a run unit"), so `launcher_env` set no `WICKED_GARDEN_ROOT`
  and no PATH prefix — garden's `wicked-garden-mem` / `-search` and the read-only estate shim were
  unreachable on every chat seat even with garden installed (P6: claude evicted twice with 0 estate
  calls). Now the SAME delivery a unit on that CLI gets — `resolve_ladder()` + `fence_admit`
  (inlined from `admit_turn`'s fresh arm; two `pub(crate)` words in `skills_snapshot.rs`), the
  `WorkerCli` off the ONE `acp_launch_facts` registry read — is handed on `session/new`, pinned to
  the process (`proc.skills`, core#396 parity) and logged (`chat <id> seat <cli> handed skills gen
  <gen> <hash>`); a seat with NOTHING deliverable is refused at open with the remedy ("install
  wicked-garden or publish a snapshot (System → Skills), then open the chat again"), scoped AND
  unscoped chats alike — fail loud, never a silent skill-less seat (BC-32 / R14). The per-turn
  budget `WICKED_CHAT_TURN_SECS` defaults 300 → **600 s** (env-only; a HYPOTHESIS re-derived from
  the P6 re-run's per-turn durations — BC-34 / R16) and a turn cut at it now says so first:
  `seat '<cli>' exceeded the <N> s turn budget (WICKED_CHAT_TURN_SECS) and was released — target it
  on your next message to re-seat it. Partial reply before the cut: …` (was `turn ended TimedOut:
  …`); other statuses keep their text. `chat_turn` returns `ChatTurnReply { text, usage }` and
  `CoreEvent::ChatReply` gains `usage: Option<Usage>` — wire `usage: {inputTokens, outputTokens,
  cacheReadTokens, cacheCreationTokens, costUsd} | null` (`null` on pi/agy, which emit none;
  additive). `chat_boundary.estate_store_pinned` was hard-coded `false` (argv `--db` required on
  every shim call); it now reads `estate_store_pinned_for_child() || scope.code_graph_db.is_some()`
  — the unit boundary's rule plus the scope's graph, which PR-⑦ sets on the child as
  `WICKED_ESTATE_DB` — and `chat_boundary_result` judges through `boundary_denial_tracked` with it
  (BC-36 / R16c). Tests: the chat-seat scope test pins a published fixture and asserts
  `WICKED_GARDEN_ROOT`, the bound snapshot and the reply's usage; nothing-deliverable refusal;
  budget-eviction text with the partial kept; boundary pin with a `--readonly` shim call admitted
  only when bound; `acp_launch_facts` couples all three identities; core-ts `chatReply` key set
  gains `usage`.

### Fixed
- **Governed containment: Bash writes are judged under a fenced posture; the notes root is admitted
  on both carriers; `mkdir` is a write target; the guard unstages after its restore; phase-scope
  denies name the tool and surface as `workerToolCallDenied` (DES-L4 PR-②, D-12; core #483,
  F-RC1-080, F-RC1-092, F-RC1-074).** The phase-scope fence saw only `Write`/`Edit`/`NotebookEdit`,
  so a read-only evaluator's `cat > notes.md <<EOF`, `| tee build.log` or `mkdir evidence/` walked
  through it and tripped the worktree guard (the unit died); at the same time the fence refused the
  very notes root the engine minted for that unit. Now `phase_scope_denial` judges `Bash` by its
  write targets (`bash_write_targets`, which gains `mkdir`) with the SAME admission a path-bearing
  tool gets — `write_posture::admitted_roots` = the evaluator's notes root under read-only, the
  creator's extras under deliverable-roots — armed on `WICKED_DELIVERABLE_ROOTS` for the hook and
  held in-process by the ACP fence (`judge` admits the notes root; the ACP unit prompt names it with
  the wrapped carrier's sentence). Pre-build phases' Bash writes to non-documentation paths are
  refused (advisory); `mkdir` outside the boundary is unit-FATAL like a redirect outside. The three
  fence appenders (phase-scope / infra / remote-write) now write their tool-call annotation in the
  same buffer, so a replayed record never reads `(unknown)`, and the fold discloses phase-scope
  refusals as `workerToolCallDenied{tool, command}` (the granted `pipeline.rs` arm). After a guard
  restore `git reset -q` returns the index to HEAD, so the creator's files read `??`/` M` — not
  staged `A` — and pass deliver's untracked classifier like a never-restored tree.
- **The CLI-registered estate MCP hand-off is DELETED on both carriers, units and chats (D-7, DES-L4
  PR-⑦; F-084 = F-RC1-045/086/113 product half, F-RC1-048 C1, core #485/#486).** The wrapped carrier
  wrote a per-unit `--mcp-config` file (`wicked-estate-mcp --db <graph> --readonly`) and allow-listed
  `mcp__wicked-estate`; the ACP carrier advertised the same server on `session/new` — a SECOND
  grounding transport that only claude had, that an org-managed MCP allowlist silently dropped
  (ungrounding the worker without a trace), and that put the graph's `--db` path in a worker-readable
  file. Both are gone: `mcpServers` is always `[]`, `permissions.allow` is `[]`, no config file, no
  argv flag (`repo_estate_mcp_parts` / `resolve_estate_mcp_exe` deleted). Grounding is the estate
  SHIM garden's skills run on every seat: the graph pin moves to `WICKED_ESTATE_DB` on the child
  (the ACP carrier now sets it like the wrapped `arm_worker_estate_channel`), and the read-only
  default moves from the server's `--readonly` flag to garden's existing `WICKED_ESTATE_READONLY=1`
  on EVERY worker child — so the shim spawns `wicked-estate-mcp --readonly` by default, while the
  estate fence keeps auditing the `--readonly` token and the store pin on each call. The ACP
  boundary's `estate_store_pinned` counts the child's `WICKED_ESTATE_DB` too. Provenance
  (`WICKED_RUN_*`) reaches the shim's MCP through the worker env (R12), no longer through a server
  `env` block.
- **Governed grounding recognises the `wicked-garden` launcher + more shim spellings (#463 §7.3,
  #474); Evaluator prompts carry the engine-owned VERDICT line (L1↔L4 contract).** The estate fence
  classified the shim only when it was spawned as a python/sh script; garden's own launcher —
  `wicked-garden run|python <script>`, directly or via `node <…/wicked-garden.mjs>` / `npx` — fell
  through to ALLOW with no `--readonly`/store-pin check. `executed_estate_shim` now looks through the
  launcher's mandatory `run`/`python` verb, and `script_path` collapses `//` and strips leading
  `./`, and a `mem` backend run by basename after a `cd scripts` is recognised by its module stem —
  so all three spellings are held to the same read-only + pinned-store rule. Separately, the
  engine now appends one pinned 159-byte `EVALUATOR_VERDICT_CONVENTION` line to every Evaluator-role
  work unit's prompt (all `SkillForm`s, both carriers; never to a creator, neutral, tool-command, or
  engine-internal unit), so a seat whose garden skill text is not loaded still emits the verdict
  shape the acceptance fold parses.
- **Every seat with a published generation is handed the launcher (`SkillsDelivery::LauncherOnly`,
  D1 / DES-L4 PR-⑤).** A seat with no skills lever — codex under the inherit-operator-config hatch,
  copilot with no view, or a CLI with no lever at all (agy) — was handed `SkillsDelivery::None`, so
  it got no `WICKED_GARDEN_ROOT` / `PATH` prefix and `wicked-garden run …` (the estate shim) was
  unreachable there. Those three sites now return `LauncherOnly(root)`: the launcher env reaches
  every seat that has a published root. A LIVE-CACHE root on a non-Claude seat still hands NOTHING
  (deliberate — no publish-time verdict). `skillsSnapshotHanded` keeps meaning "a lever was handed"
  via the new `delivers_skills()` (false for `None` and `LauncherOnly`); `LauncherOnly` is internal
  (`pub(crate)`, no serde) — no wire/d.ts change.
- **Run markers on every worker's own environment (R12, DES-L4 PR-③).** `WICKED_RUN_ID` /
  `WICKED_RUN_UNIT` / `WICKED_RUN_AGENT` — the pairs `estate_provenance_env` already produced — were
  handed ONLY to the estate MCP (the wrapped `--mcp-config` env object, the ACP `session/new` env
  array); neither worker `Command` carried them, so garden's estate shim, spawned from the worker's
  Bash, could not detect governed mode marker-first and a shim-submitted proposal had no provenance
  to inherit. Both carriers now stamp the three markers on the worker `Command` itself through one
  shared `stamp_run_markers`, after `hardened()` (which strips none of them), for every unit
  governed or not; a chat passes an empty slice and stays unmarked.
- **Governance Bash scan sees through one wrapper level (core #475).** `bash_write_targets` (the
  FINDING-045 write fence) and `classify_estate_command` (the estate allowlist) matched only a bare
  program word: `sh -lc 'echo x > src/y'`, `exec tee src/y`, `xargs tee src/y`, `nice`/`timeout`
  wrappers and a QUOTED program word (`"cp" a src/y`) all matched nothing. Both consumers now share
  `unwrap_program`: a fixed seven-word table (`sh`/`bash`/`zsh`/`dash` with a flag cluster ending in
  `c` → the string is rescanned as its own command line; `exec`; `xargs [flags]`; `env [flags]
  [NAME=val]…`; `nice [-n N]`; `timeout [flags] <duration>`) plus one quoting rule (one layer of
  quotes off the program word before the basename match). ONE level only — a nested `-c`, inline
  interpreters (`python3 -c`, `node -e`, `perl -e`), `$(…)`/`$VAR`, `sed -i`, `git apply`,
  `rm`/`touch` and a renamed binary remain the documented literal-scan limits (the list is now
  complete in the classifier's doc); the OS sandbox stays the only hermetic layer.
- **Estate grounding allowlist — read-only estate reads are allowed in governed units, writes stay
  denied, every estate deny names the tool and the command (DES-GROUNDING-001 §7, issue #463 items
  1 + 2, F-RC1-046 / F-RC1-047).** The governance gate denied every `wicked-estate` /
  `wicked-estate-mcp` invocation from Bash as unit-FATAL whatever the subcommand — a finished
  `recon` unit that ran `wicked-estate stats` died as `sessionFailed` with no route back — while
  the estate stdio shim garden's skills actually run (`sh …/_python.sh …/scripts/mem/estate_memory.py`)
  was invisible to the binary-name scan. `bash_denied_estate_indexer` is replaced by
  `classify_estate_command`, a per-command allowlist: the read-only CLI subcommands (`query`,
  `blast-radius`, `rank`, `stats`, `source`, `semantic`, `cross-graph`, `subscribe`, `clusters`
  without `--annotate`) are **allowed**; the write subcommands (`index`, `scip`, `tfstate`,
  `import-telemetry`, `compact`, `watch`, `clusters --annotate`) and any unrecognised verb are
  **denied** fail-closed. `wicked-estate-mcp` and garden's shim / `mem` backends — recognised by
  the script in executing position, through `python*` / `py` / `sh` / `_python.sh` / `python -m`
  — are allowed only with **both** `--readonly` **and** a pinned store (`--db <path>`, a leading
  `WICKED_ESTATE_DB=` / `WICKED_HOME=` / `WICKED_MEMORY_DB=` assignment, or those variables in the
  worker environment — a parameter of the judgement on both carriers:
  `BoundaryCtx.estate_store_pinned` on ACP, the hook's own environment on the wrapped path). A
  denied estate command is a real decision record on both arms: the tool annotation rides with
  the claim (no `(unknown)`), the command at `obligations[1]`, the reason naming the segment and
  why (write verb / unknown verb / no `--readonly` / no pin) plus the remedy. On a unit whose
  posture fences writes (recon / pre-build) the deny is **advisory** (`estate-deny:`): the seat
  continues with the remedy and the fold emits `workerToolCallDenied` — its `carrier` now read
  back off the armed marker (`write_armed_marker_for`, written by both carriers) instead of an
  assumed `wrapped_cli`. On a code-executing unit it stays **fatal** (`boundary-deny:`; the unit
  is denied as before, now with the tool named). `proposal.submit` through the `--readonly` shim
  stays allowed. The shim rule is inert until wicked-garden #1130 spells `--readonly` on the
  backend argv; item 3 (a denied recon command opens a gate, never `sessionFailed`) is tracked
  with core#464.

### Added
- **Each `wicked-core-ts` platform package carries the standalone `wicked-core` hook binary,
  stripped, thin-LTO, stamped with the engine semver it was built from (core#405, F-009, F-SMOKE-006
  — FIX-IT-ALL L10-9, register BC-68).** The per-tool-call governance hook binary was published
  NOWHERE: crew located it in a home-dir install (`<home>/.local/bin`, `<home>/.cargo/bin`, a dev
  path, PATH) — the stale-copy class the operator hit when a symlink there pointed at an old build —
  and the installer registry lacked it. `napi-release.yml` now (build job, per target) asserts the
  root `Cargo.lock` and `crates/wicked-core-ts/Cargo.lock` pin the same `wicked-estate*` versions
  (two lockfiles, one tree — the binary and the addon must embed the same estate), builds
  `--bin wicked-core` for the same triple with `--config profile.release.strip=true --config
  'profile.release.lto="thin"'` (the root crate declares neither; unstripped it measured 19.2 MB
  against a 14.3 MB platform package) and uploads it beside the `.node` under a per-target name;
  (publish job) moves each binary into its platform package as `wicked-core[.exe]`, `chmod +x`,
  appends it to `files[]` and stamps `wickedCoreVersion` = the ROOT crate's semver — the value the
  addon's gate compares against the binary's `--version` (`our_semver = env!("CARGO_PKG_VERSION")`,
  the root crate's, not this package's) — so `<pkg>/wicked-core --version == wickedCoreVersion ==
  GET /diagnostics.engineBinaries.wicked-core` from one tree at the tag. The root crate version is
  NOT bumped to core-ts's. Crew's locator prefers the bundled binary in a follow-up (crew half;
  `WICKED_CORE_EXE` still wins). **Merge before the `core-ts-v0.7.26` tag; rehearse the size on a
  `workflow_dispatch` of a branch first (every leg builds, nothing publishes) — the measured delta
  fills register L10-e.**
- **The creator owes the repo-checks floor too; a timeout is not a failure; a base failure is
  not a regression (core#467, core#469, F-RC2-009 — hardening train S4a).** On 2026-09-13 a `fix`
  worker left a tree failing `npm run typecheck` (exit 2) and `npm run lint` (exit 1), wrote
  "pre-existing typecheck error" although `main` was clean, and was judged PASS on "left a
  change"; the red tree reached the read-only evaluator, which fixed it in place, tripped the
  worktree guard, and the run was lost at an escalation gate with no route back (F-RC1-070/071).
  The same day a correct crew fix passed typecheck and lint and was DENIED because the full
  `npm test` exceeded the fixed 1200 s bound under host load (F-RC2-050), and a verify floor
  reported 27 `cargo test` failures that pass on the run base outside its sandbox (F-RC2-009).
  **Behaviour change — the `bug`/`feature` `fix` phase (every def phase with `executes_code` and
  `role: creator`) is now floored** (`plan_from_def` sets `default_floor` regardless of a later
  `verified_evidence` phase): provision + typecheck + lint + tests run at the END of the
  creator's phase in its worktree (`repo_checks::FloorStage::Creator`), and a red floor denies
  the creator's unit with the check tails on the record — the run pauses at the escalation gate
  ON the creator (core#464, class `floor_failed`; retry / cancel) one phase earlier than before,
  instead of at the evaluator's guard escalation; the rework route (re-dispatch the creator with
  the tails) is S4b. What the
  floor reports changed with it: every check carries `outcome: passed | failed | timed_out |
  could_not_run`, the effective bound it ran under (`bound_s`, `bound_note`) and, when it
  failed, a BASELINE DIFF — the same check run once per run on the run base (the run base commit
  recorded on the unit at dispatch — never a HEAD a creator may have moved by committing — exported
  through the pinned git dir into the checks' scratch for the base check only, then removed; cached
  by base sha + check name; `baseline_diff: false` opts out) with failure identifiers streamed off the
  runner's output (cargo, vitest/jest, tsc, pytest, go, eslint stylish): failures the base
  shares are `pre_existing_in_sandbox`, identical base/head failure sets a `floor_env_mismatch`
  (the floor's env is recorded on the verdict — `env`; the sandbox itself is unchanged), and
  only head-only `regression`s deny. A check that hit its bound is `timed_out` and the unit is
  denied under the NEW source `repo_checks_timeout` (never `repo_checks`), worded as "did not
  FINISH" with the remedies; the bound is `base × clamp(load1/ncpu, 1, 3)`. Per-repo
  `.wicked/checks.json` (`typecheck`, `lint`, `test`, `test_targeted` with `{files}`/`{base}`,
  `timeout_s`, `full`, `baseline_diff`; fail-closed on a malformed file): the floor prefers
  `test_targeted` at the creator always and at verify unless `full: true`. A creator transcript
  claiming a failure is pre-existing is judged against the diff (`claim: claim_rejected |
  claim_confirmed | unverified`). Wire (all additive): `repoChecksEvaluated` gains `outcome`,
  `floor: creator | verify`, `claim`, `env`; each `checks[]` entry gains `outcome`, `boundS`,
  `boundNote`, `failureIds`, `classification`, `preExisting`, `regressions`, `base`. Gate
  ACTIONS on these classifications (re-dispatch the creator with the tails, extend / targeted /
  accept on a timeout, accept an evaluator suggestion) are S4b. Design: `.product/DES-FLOOR-001`.
- **Role-keyed BASE skill directive on every unit (core#468).** The engine handed a unit exactly
  one skill directive, built from its phase's `skill_ref`; nothing could force a discipline skill
  on EVERY unit. `WorkflowDef.base_skill_ref: Option<String>` (drop-in JSON field, serde-default,
  absent on the wire when unset) — with the engine-config default `WICKED_BASE_SKILL_REF` (the
  def's field wins; the def's `""` is an explicit opt-out) — now lands on every AGENT unit of the
  plan as `WorkUnit.base_skill_ref` (`plan::apply_base_skill`; never on a Tool unit), and the
  prompt builder LEADS with one short, role-keyed directive before the phase directive:
  `Invoke your skill "<base>" (via the Skill tool) and follow its §<role> section; then: …`
  (`§creator` | `§evaluator` | `§neutral` from the unit's `PhaseRole`), spelled per CLI exactly as
  the phase directive is (core#396 forms; `execute_wrapped::base_skill_directive`) and held to
  `BASE_SKILL_DIRECTIVE_MAX` = 120 bytes so a pty-routed unit keeps its task-text budget (the
  skill text lives in the snapshot). Engine-internal judge/triage prompts carry none. GATED AT
  INTAKE: `skills_snapshot::admit_base_skill` requires the skill to EXIST in the resolved skills
  root before anything is planned — on the actor's synchronous launch path (a caller gets an
  `Err` naming the skill with NO session persisted) and in `pre_distribute` — as
  `SkillsError::BaseSkillRefused { skill, cause }` ("refused at intake, before any unit was
  planned"), and again plan-wide at every launch (it rides `StepInput::required_skills`). It is
  EXISTENCE-only by design: it never joins the seat half of `RequiredRefs`, so a `portable: false`
  base skill neither refuses a non-Claude seat nor collapses routing onto claude
  (`distribute::seat_candidates` reads `skill_ref` alone — pinned by a test with a non-portable
  fixture base skill). Wire: `unitDispatched` gains `baseSkill: {name, role} | null` (additive,
  emitted unconditionally; the handed generation is the same unit's `skillsSnapshotHanded.gen`).
  Default OFF in the engine (no def field, no env) — wicked-crew turns it on with
  `wicked-garden-governed-worker` (crew#554).
- **State-home fence: unregistered entries are a configuration error, refused at intake (core#411,
  wicked-crew#497; acceptance findings F-RC1-011, F-RC2-020, F-032/F-033).** The worker Read fence
  fails closed BY NAME on a state-home entry its static registry (`tests/fixtures/state-home-subtrees.json`,
  embedded by `state_home.rs`) does not classify — right, but it fired at the run's FIRST WORKER
  launch, after a planning council and the intake gate, as a failed unit the failure-triage judge
  labelled "triage judge errored" and escalated with an "Approve to retry" that could only fail
  again, while the daemon booted green. Twice in one day that shape cost every governed run on a
  host: the acceptance rig's `WICKED_WORKFLOWS_DIR=<state home>/workflows` and a
  `skills.fixture-debris-…` directory left in the operator's live state home. The message also cited
  a repository test-fixture path and asked the operator to "register it in core AND crew". Now:
  (1) `state_home::survey` lists EVERY unregistered entry at once (the fence stops at the first —
  removing `interactive` only moved the rig's refusal to `workflows`); (2) `Core::launch_run`
  runs the INTAKE fence in its synchronous fast path and refuses with a TYPED, downcastable
  `StateHomeConfigError { var, snapshot, state_home, unregistered: [{name, path, level}], remedy }`
  — no session persisted, nothing planned, no council, no judge; (3) `preflight_state_home(snapshot,
  db_path)` (core-ts `Core.preflightStateHome(snapshotPath, dbPath)`, a static resolving to JSON
  `{stateHome, derivedFrom, unregistered, refusesLaunches, error, remedy}`) is the boot-time call
  crew's `serve` makes so the daemon reports the blocker before anyone launches; (4) every
  refusal — intake and the launch-time fence that stays as the last line — is worded in operator
  terms (the entry, the state home, the variable, what to do) and none cites a fixture path;
  (5) the registry gains the three entries an OPERATOR variable can place under the state home —
  `workflows` (`WICKED_WORKFLOWS_DIR`), `steering-inbox` (`WICKED_STEERING_INBOX_DIR`),
  `interactive` (`WICKED_INTERACTIVE_ROOT`) — each carrying an `env` field, so a pre-existing
  placement is fenced (denied) rather than refusing every launch; crew refuses to boot with such a
  variable pointed inside the state home (wicked-crew#497). Debris is deliberately NOT patterned: a
  quarantine-by-rename inside the state home is what the fence must refuse. **Behaviour change:** a
  launch whose handed snapshot derives a state home with an unregistered entry now fails at intake
  with the configuration error instead of at unit 1 with a worker error; kind mismatches, an
  unset/empty/unresolvable/shapeless `WICKED_SKILLS_SNAPSHOT` keep their launch-time admission
  unchanged. Tests: `tests/state_home_intake.rs` (real `Core`: refused synchronously + typed + no
  session/event/worker; the same tree with the three env-placed names admitted), `state_home.rs`
  unit tests (survey lists all levels; wording has no fixture path; preflight `refusesLaunches`
  only for a handed snapshot; registry classifies the new names, not debris).
- **Deliver gate (acceptance finding F-E2E-030).** Run `0ab5ccb8` launched under the studio
  composer's default posture (`humanConfirm: before:1`): the intake gate was the only human gate,
  verify passed, and the crew-composed `deliver` Tool unit pushed `wicked/<run>` and opened the PR
  unattended, under whatever `gh` account the daemon held. The ENGINE now gates the deliver unit:
  `should_pause` pauses before a Tool unit whose phase id is `deliver` (`deliver_lift::is_deliver_unit`,
  the same recognition the lift uses) whatever the run-level `human_confirm` says, with a prompt
  that names the push (branch, repository, "under the gh account active in the daemon's
  environment — pin it now if it must differ"). The one opt-out is EXPLICIT and non-default:
  `LaunchSpec.auto_deliver: bool` / core-ts `LaunchOptions.autoDeliver?: boolean` (absent ⇒ `false`
  ⇒ gated), persisted as `AgentSession.auto_deliver` (serde-default, additive on the run DTO) so a
  resume re-arms the same posture. `human_confirm: none` is NOT an opt-out (FINDING-019/023: it is
  also the enum default and the typo fallback). A def gate on the preceding phase still fires as
  before; the deliver gate is judged first so its prompt is the one shown. Behaviour change for
  the non-daemon launchers — the `wicked-core` CLI, the bus bridge and campaign nodes — whose runs
  compose a deliver phase: they now park `awaiting_human` before it, and expose no `auto_deliver`
  opt-out in v1.
- **Floor provisioning by declared dependencies (F-E2E-029 a).** `repo_checks::detect` installed
  only when `node_modules/` was ABSENT. Run `0ab5ccb8`'s nested worktree carried a `node_modules/`
  holding only vitest's cache (written when the creator ran the suite; Node had resolved the
  runner upward into the customer's clone), so the floor skipped the install, `npm run test`
  started and three path-relative suites died on ENOENT under `node_modules/wicked-crew-api-types/`
  — a deterministic denial cleared only by steering the evaluator to `npm ci`. The floor now
  installs when `node_modules/` is absent OR any DECLARED top-level dependency (`dependencies` +
  `devDependencies`; a symlinked package counts, optional/peer are not required) lacks
  `node_modules/<name>/package.json`; the `install` check's `source` names the first missing
  dependency. A failed install is reported as **dependency provisioning failed … an environment
  finding, not a verdict on the change** (with the lockfile, the reason, and the install's own
  output), never a bare ENOENT denial — the gate stays closed (deny-dominates); the reason is now
  legible. Deferred: bun / pip / pyproject provisioning (npm, pnpm, yarn handled; Cargo
  self-provisions).
- **Package-manager install fence — BEST-EFFORT (F-E2E-029 b, `src/install_fence.rs`, new).**
  During run `01234444` the creator seat put 194 MB of `node_modules` into the CUSTOMER'S CLONE
  ROOT (the worktree's parent). The claude ACP seat's record arms no OS write boundary
  (`acp.os_sandbox: false`), the bridge judged `fs/write_text_file` paths and remote-write
  commands, and nothing judged WHERE a shell command's install would land. **The exact command was
  never captured** (ACP tool calls are not logged as run events; no unit-3 transcript exists), so
  the fence closes the spellings the review could reproduce, not a proven one. On the two carriers
  that see the command text per call (the wrapped carrier's `PreToolUse` hook and the ACP
  permission bridge, beside the remote-write fence) a mutating `npm`/`pnpm`/`yarn`/`bun`
  invocation whose effective directory is OUTSIDE the unit's worktree is refused — following
  `cd`/`pushd`/`popd` (subshell `( … )` scoped), `--prefix`/`-C`/`--dir`/`--cwd`, `env -C`/`--chdir`,
  `npm_config_prefix=`, `~`, canonical spellings, and global installs (`npm i -g`, `yarn global
  add`, `pnpm add -g`, `bun add -g` — the global prefix is outside by definition). **The shell cwd is
  tracked across tool calls per `(run, unit, attempt)`** — the natural two-step `cd <clone>` then
  `npm ci` in the next call is refused on both carriers (ACP: on the per-unit fence state; hook: a
  sidecar of the attempt's decisions log, `install-fence-cwd-<phase>`), updated only by ALLOWED
  calls, reset on a new attempt. Advisory (one tool call, not the unit), answered with the remedy,
  disclosed as `workerToolCallDenied` (`reason` prefix `install fence:`). Reads (`npm ls`), scripts
  (`npm run`, `npm test`) and installs inside the worktree pass. **Known evasions it does not
  close** (independent review, 55 probes): a path carried in a shell variable (`ROOT=../..; cd
  $ROOT`, `--prefix "$ROOT"`, `$(git rev-parse …)`); a script fed by pipe, `sh -c "$(… | base64
  -d)"`, or a file (`sh /tmp/x.sh`); a symlink CREATED in the same command; program indirection
  (`npx npm`, `corepack npm`, `$(which npm)`, `node -e "execSync(…)"`, `npm exec -- npm ci`);
  shell control-flow keywords (`if cd ../..; then npm ci; fi`, `for … do`). A text scan is never
  hermetic: **F-E2E-029(b) stays OPEN until OS containment (`os_sandbox: true`) is armed for the
  seat**; until then the engine discloses the posture per seat (below).
- **`sandboxPosture` (additive event; review of #456 F4).** At distribution every agent unit's
  assigned seat discloses its write containment: `os` (the record arms the kernel write boundary,
  `acp.os_sandbox: true` — read by both carriers) or `advisory` (no OS boundary — worktree guard +
  command-text fences only, a shell can evade a text scan; the rig's claude ACP seat). Wire:
  `{session, ord, cli, posture, reason}`. The studio fold (run header beside `run-degraded`) is a
  follow-up.
- **`awaitingHuman.gateKind` (additive; review of #456 F7)** — `run_level` | `def` | `deliver` |
  `terminal` | `escalation` | `failure` | `triage`, also on the durable interaction request
  (`gate_kind`), so a consumer keys on WHY the run paused, never on the prompt's wording.
- **`worktreeRetained` (additive event; review of #456 F6)** — `{session, path, reason}` when a
  cancelled run's worktree is kept because it holds uncommitted work; the
  `WICKED_COMPLETED_WORKTREE_KEEP_DAYS` window now covers cancelled runs too (then reaped
  clean-only, as before).

### Changed
- **State-home registry: `chats` registered; `interactive` is a crew-placed root (the ONE fence
  rule change of this RC — FIX-IT-ALL L10-5, core half).** wicked-crew persists chat transcripts
  at `<state home>/chats/<id>.jsonl` (crew BC-33) and moves the interactive bridge's default docs
  root and recorder browser under `<state home>/interactive/` (crew BC-49, D-L7-1 MOVE); both
  would be refused by the intake fence as unregistered entries the moment crew creates them.
  `tests/fixtures/state-home-subtrees.json` gains the `chats` entry (`kind: dir`, `owner: crew`,
  `worker_read: none`), and the `interactive` entry drops its `env` field and names both crew
  placements in `source` — `WICKED_INTERACTIVE_ROOT` may now name any path (the crew boot-refuse
  row is crew's to delete, in the same crew release as its joins). The fence RULE SET changes once:
  one new deny (`chats`); `env` is crew's boot-preflight classification, not a deny rule. Core's
  survey is directory-driven, so the row is inert until crew's release creates the directory — the
  crew copy of the fixture re-converges byte for byte when crew lands its halves. Tests:
  `state_home_intake.rs` admits `chats/` and refuses `chats-x` (a name claim is exact, never a
  prefix); the unit list names `chats`.
- **A denied unit pauses at the escalation gate; it no longer fails the run (core#464, core#463
  item 3).** Three governed `bug` runs in one day (wicked-garden `e20a3ffb`, RC1 Phase 3 r3
  `dd5b8f54`, garden L4 `b5c2739d`) ended `unitDenied` → `sessionFailed` at their second unit:
  the read-only `reproduce` rung wrote a 146-line analysis note into the worktree, the guard
  correctly restored the tree — and the engine then ended the run, because the escalation gate
  only opened for a unit whose own def gate was `human_confirm_if: verdict_not_pass` (verify).
  The restored-tree prompt and the studio banner ("approving this gate retries the phase")
  existed and were unreachable; a governance deny on a recon command was booked two seconds after
  `unitOutputCaptured ok` the same way. Now EVERY denial the gate fold produces — the worktree
  guard, an input-governance (boundary) deny, a deterministic floor (repo checks, pinned
  validator, substance, deliverables), the output gate's policy decision, the agent judge, the
  evaluator≠creator pass — opens the `escalation` gate on the denied unit: Approve re-dispatches
  the SAME unit (attempt+1, against the tree the guard restored), Approve+steer amends it, Reject
  cancels (a dirty worktree is kept, `worktreeRetained`). `gateEscalated` gains the DENIAL CLASS on
  `condition` (`verdict_not_pass` | `evaluator_mutated_worktree` | `boundary_deny` |
  `floor_failed`) and additive fields `attempt`, `denialSource`, `defGate`, `outputCaptured`,
  `restored`, `discarded`, `suggestionRef` — what a "reassign" / "accept the captured output"
  arm (core#459, follow-up) needs without re-reading the unit. The worktree-guard prompt names
  the reverted paths. `unitDenied` still fires (observability), followed by the gate instead of
  the run end. **Behaviour change**: a `bug`/`feature`/prose-planned run whose unit is denied by
  a policy, the hook, a floor or the guard now parks `awaiting_human` (gate kind `escalation`)
  where it used to end `failed` — including under `human_confirm: none` (the prompt discloses
  the precedence, as the def-authored escalation already did) and on the CLI / bus-bridge /
  campaign lanes, whose runs need a `confirm_gate` to proceed. The legacy SYNC lane
  (`Core::launch` → `run_session`) is untouched. Worker FAILURES (a CLI that exits non-zero,
  triage `fail`, elicitation loss) still fail the run — that lane is core#461's. Consumers keying
  on `condition`: a repo-checks / pinned-validator denial on the def-gated `verify` phase used to
  escalate as `verdict_not_pass` and now reports `floor_failed` (`defGate: true` still marks the
  def-authored pause); crew's `GateEscalatedEvent` type gains the additive fields (follow-up).
- **Evaluator notes root (core#464 item 2).** A bound read-only agent unit (an evaluator or
  recon rung of a run with a worktree) is given a per-unit NOTES ROOT —
  `<temp>/wicked-core-notes/<run>/unit-<ord>`, engine-owned, OUTSIDE every worktree and the
  customer's clone — on `WorkUnit.notes_root` (additive, `notesRoot` on the run DTO when set),
  joined into that unit's write boundary (`extra_write_roots` of its governance context, so every
  carrier arms it), and named in the guard-only seat's read-only instruction ("write them ONLY
  under …"). The guard compares the worktree and nothing else, so a note there never trips it; a
  creator keeps its declared write roots; a claude/codex seat with a read-only lever is refused
  write-class calls at its boundary as before and is not told about it.
- **Cancel keeps a dirty worktree (F-E2E-028).** `cancel_run` FORCE-discarded the worktree; run
  `01234444`'s creator fix (3 files) survived only as an unreferenced tree object after the
  operator rejected an escalation gate, while the evaluator's discarded edit was kept under
  `refs/wicked/suggestions`. Cancel now reaps by the rule every other terminal status uses
  (FINDING-003, `reap_worktree_if_clean`): a clean tree goes, one holding uncommitted work stays and
  is named on stderr. A reject at the new deliver gate is the same cancel over verified, unpushed
  work — its prompt says the worktree is kept.

- **Wave 6 — the governed testing journey (acceptance findings F-7R2-005/006/012/013/019).**
  - **Worker remote-write fence (F-7R2-012).** A worker seat opened wicked-studio PR #258 from
    its own shell (`git push`, `gh pr create`) on the daemon's ambient `gh` login; the ledger
    recorded `delivery: "none"`. Delivery is the deliver phase's job. Three layers, one module
    (`src/remote_write_fence.rs`): (1) claude Bash deny rules spelled as the CLI enforces them
    (`Bash(git push:*)`, `Bash(gh pr create:*)`, `Bash(gh api:*)`, `Bash(gh release:*)`, …) joined
    into every Bash deny list — the shared worker `settings.json`, each per-session file and ACP
    `session/new` options, the council ballot; (2) a segment-wise command filter on both carriers
    that see the command text — the wrapped carrier's `PreToolUse` gate hook and the ACP
    permission bridge — refusing `cd x && git -C x push`, `sh -c 'gh pr create …'`, `gh api -X
    POST …` while letting `gh pr view`, `gh api` GETs and local git through; a refusal is ADVISORY
    (one tool call, not the unit), answered with the remedy and disclosed as
    **`workerToolCallDenied` {session, ord, attempt, cli, carrier, role, tool, command, reason,
    remedy}** plus a log line — DENY-DOMINATES on git (independent review of #449, FN-1/FN-2): a
    `git` verb not in the builtin allow-list is refused as a possible alias, `-c`/`--config-env`/
    `GIT_CONFIG_*`/`git config` overrides of `alias.*`, `url.*`, `remote.*`, `credential.*`,
    `core.sshCommand`… are refused before the verb is read, `gh alias` is refused whole, and the
    parser reads `bash -e -c`, `gh pr -R o/r create`, `gh api -XPOST`, unquoted `cmd /c`,
    `timeout -k`, array-valued ACP commands (codex `shell`) and prose titles — and, after the r2
    review's live-git corpus, `bash -euo pipefail -c`, `bash -c -e`, `busybox sh -c`, a DECODED
    PowerShell `-EncodedCommand` (UTF-16LE base64; an undecodable payload is refused), `env -i`,
    `script -c`, `git subtree push`, `init.templateDir`, `gh ssh-key|gpg-key add`, `gh codespace
    create`, `gh extension install`, and every plaintext credential read (`git credential`,
    `git credential-store|cache|osxkeychain|…`, `gh auth token`, `gh auth status --show-token`);
    (3) every seat spawn (wrapped, ACP, ballot — the inherit hatch included) strips the
    `GH_*`/`GITHUB_*` tokens AND `SSH_AUTH_SOCK`/`GIT_SSH*`/`GIT_ASKPASS`/`GIT_CONFIG_*`/
    `GIT_EXEC_PATH`/`GIT_ALLOW_PROTOCOL`, aims `GH_CONFIG_DIR` at an engine-owned credential-less
    directory, sets `GIT_CONFIG_NOSYSTEM=1`, re-points `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM` at
    seat-owned files that COPY the operator's identity and presentation keys (`user.*`,
    `core.editor`, `diff.*`, `color.*`, `filter.*`, … — never `[include]` their file, never
    `url.*`/`remote.*`/`credential.*`/`alias.*`/`include*`/`core.sshCommand`/`http.*`/
    `protocol.*`/`init.templateDir`; the r2 review reproduced an operator `url."git@github.com:".
    pushInsteadOf` riding in through the include and sending a fenced https push to ssh) and
    RESET credential helpers and askpass, replaces git's ssh with a program that does not exist
    (`GIT_SSH_COMMAND` and `core.sshCommand` = `wicked-nopush-ssh`), refuses the ssh, git and
    ext transports outright (`protocol.<name>.allow = never` — the only way to reach the
    scp-like `user@host:` and bare `host:` remotes no URL prefix enumerates), and carries a push
    kill on every URL-form transport with `-c` precedence (`url.wicked-nopush://.pushInsteadOf`
    for https/http/`ssh://`/git/`git@`/`file://`/absolute and Windows paths). Exactly what layer
    3 guarantees: a `git` running with the seat's environment intact cannot push anywhere —
    https, `ssh://`, `git@host:`, `user@host:`, `host:`, `git://`, `file://`, a path — whether
    the remote is spelled on its command line, in the repository's config or through an operator
    `pushInsteadOf`, and cannot read a credential helper; fetch over https, `file://` and local
    paths keeps working, fetch over ssh/`git://` no longer does (the engine performs the base
    fetch; a seat cannot fetch a PRIVATE https remote either — helpers reset, no gh login). Not
    covered by layer 3, stated: a `git` carrying its own `-c` override of a fenced key (`-c
    protocol.ssh.allow=always -c core.sshCommand=ssh` — command-line `-c` outranks the
    environment entries) or its own `GIT_SSH_COMMAND`, and a seat that scrubs its environment
    (`env -i`) first — each refused as a literal by layer 2, each the stated script-file limit
    otherwise. Fixture-tested (`wicked_apps_core::spawn::fence_remote_credentials`) against
    `ssh://`, `ssh://user@`, `git@host:`, `nobody@host:`, `host:`, an https remote the operator's
    global config rewrites to ssh, `file://` and path remotes, with a stub `ssh` on `PATH`
    proving ssh is never reached under the fence and IS reached by the unfenced controls, while
    the deliver tool phase (`run_tool_cmd`, no seat config) keeps the daemon's login and still
    pushes. Carrier coverage, stated: the per-call command filter runs on the wrapped CLAUDE gate
    hook and the ACP bridge; wrapped non-claude single-shot seats and ACP chat sessions are fenced
    by layers 1 (claude only) and 3. The Bash rules ride the council ballot's own argv too (not
    only the shared worker file), and `~/.config/gh`, `~/.config/git`, `~/.git-credentials` join
    the read fence.
  - **Health-aware routing with `degradedReason` (F-7R2-006).** `AgenticCli.health: Option<{usable,
    reason}>` (additive, serde-default) carries the launcher's sign-in/usability verdict; a seat
    with `usable: false`, or one that fails authentication IN the run (a `not_logged_in` council
    ballot, a worker exit with an auth refusal, an ACP `auth_failed`/`unauthenticated`
    handshake), is BENCHED for the run — persisted as `AgentSession.benched_seats [{cli, reason,
    source}]` — and never convened (`councilConvened.clis` names only eligible seats), never the
    evaluator≠creator reassignment's pick, never a failover target, never a triage or agent
    judge. `unitDistributed.degradedReason` is now set on EVERY routing arm whenever eligible <
    configured ("N of M seats benched: codex (signed out — launcher), pi (not_logged_in —
    ballot)") — the Council arm used to emit `null` unconditionally. An all-benched roster
    refuses the plan by name instead of parking the run at a human gate per unit. The default
    and triage judges draw from the registry seats the RUN configured (`session.clis`) minus the
    bench — never a registry seat the launcher did not seat — and a judge seat that refuses with
    an authentication failure is benched (`source: "judge"`) so the next unit never re-tries it
    (review RT-1). core#447's registry `credential`/free-tier declaration is NOT folded here (see
    the PR body): `health` is the carrier either way and can be filled from the declaration when
    it lands, with no wire change.
  - **Default repo-checks floor + judge, or an honest UNGATED (F-7R2-005).** Every PROSE-planned
    agent unit — and every agent unit of a def that declares no `verified_evidence` phase —
    carries `WorkUnit.default_floor` (plan-time; a def that verifies owns its floor, so the
    `bug`/`feature` `fix` gate is not hard-failed on checks the def routes to `verify`'s human
    gate). Such a unit takes a worktree baseline at dispatch; when it CHANGED its tree it gets the
    DEFAULT floor — the repository's own checks (`repoChecksEvaluated`, the deliver-lift machinery
    incl. `verified_tree`) — and an agent judge distinct from the creator against an
    engine-authored criterion when an eligible non-creator seat exists. **`gateEvaluated.ungated: bool` +
    `ungatedReason: string | null`** (additive): `true` when no machine layer gated the unit (no
    floor, no judge, empty policy selection — the vacuous default-allow run b86c14c1 passed seven
    times), with the cause per absent layer ("no judge: no eligible judge seat distinct from
    creator 'claude' …"). On a host with no OS sandbox the DEFAULT floor discloses instead of
    denying (the declared `verified_evidence` floor stays fail-closed) — and the cause is ON THE
    WIRE whether or not a judge ran (review FL-1/FL-2): **`repoChecksEvaluated.sandboxLevel`,
    `sandboxError`, `detectError`** and **`gateEvaluated.floorNote`** (why the deterministic
    layer is absent, whenever it is) + **`judgeSkippedReason`** (why no judge was convened for a
    unit that wanted one). A changed tree with no check report denies fail-closed. The marker is
    "no `verified_evidence` phase AFTER this unit" (FL-3). Cost, stated (FL-4): the repository's
    own checks run once per tree-changing prose unit (bounded by `CHECK_TIMEOUT`/`INSTALL_TIMEOUT`
    per check); a repo with no detectable manifest yields `passed: true, checks: []` — consumers
    render "0 checks detected", never "checks passed".
  - **Auth fallback kinds, no wrapped retry (F-7R2-019).** `acpFallback.fallbackKind` gains
    `auth_failed` (the handshake's `authenticate` failed) and `unauthenticated` (`session/new`
    refused after `authenticate`, or no method advertised) — pi's 401 used to be filed as
    `binary_unavailable`. No auth kind (`auth_required` included) is followed by the single-shot
    wrapped fallback: it runs under the same worker home and fails identically; the unit fails
    with the seat's own words and the actor benches the seat.
  - **Run branch recorded; worktree retained (F-7R2-013).** `AgentSession.run_branch`,
    `base_commit` and `finished_at` (additive) are durable; `runBaseResolved` gains `runBranch`.
    A COMPLETED run's worktree is kept until the run is archived (`ArchiveRun` reaps clean-only)
    or `WICKED_COMPLETED_WORKTREE_KEEP_DAYS` (default 14; `0` = reap at completion) elapses — the
    boot reaper honours the window AND every run completion sweeps expired retained trees (review
    RN-1: no longer boot-only); a retained tree has its IGNORED files (`node_modules`, `target/`)
    dropped at completion (`git clean -fdX`), never its tracked or untracked-not-ignored files.
    Failed/cancelled runs are unchanged.

### Fixed
- **Seat-bench gap: a dead-class ballot followed by the dispatcher's own abstention benched
  nothing (core#461; F-SMOKE-002, F-RC2-023/029).** The council reported ONE outcome per seat per
  council — the latest ballot — so a seat that failed round 1 on `quota_exhausted` /
  `not_logged_in` / `not_installed` and was then health-gated for the runoff reached the run-level
  ledger as a `Benched` abstention alone, which `seat_ballots` dropped: the ledger saw no
  evidence, the seat stayed routable, `degradedReason` did not name it, and `evaluator_distinct`
  seated it (run 390b273e: copilot after 5/5 quota ballots; the worker died in 16 s). The council
  now keeps EVERY ballot's seat failures (`PollStatus.seat_failure_history` / the task record's
  `seat_failure_history`, additive; `seat_failures` is still the latest round) and the ledger
  tallies every round, one ballot per seat per round. The dispatcher's `Benched` abstention is
  recorded and read as CORROBORATION of a dead-class ballot beside it (`quota_exhausted (1/2
  ballots)`; `not_logged_in` / `not_installed` still bench on first occurrence), as part of the
  dispatcher's own streak after a timeout (`timed_out` once timeouts + abstentions reach the bench
  threshold, with at least one timeout), and as proof of nothing on its own. One vote still keeps
  a seat; an unclassified failure still proves nothing.
- **A worker that exits on a dead-seat refusal no longer dies through the triage judge
  (core#461 b).** With an operator in the loop, every non-environment worker failure on attempt 0
  went to the LLM triage judge FIRST — a classified seat refusal (`quota_exhausted`,
  `not_logged_in`, `not_installed`) included — and the judge's `fail` was run-fatal with no gate;
  the failover ladder below it (bench + next eligible seat, crew#277) was never reached. A
  classified seat refusal now skips the judge and takes the ladder: the seat is benched (as
  before), the work moves to the next eligible seat and the run continues. When NO eligible seat
  remains and a human is present, the run PAUSES at core#464's escalation gate
  (`gateEscalated.condition: "dead_seat"`, `awaitingHuman.gateKind: "escalation"`, prompt naming
  the seat, the class and the reassign lever) instead of `sessionFailed`;
  the unit carries a structured `dead_seat` denial for the gate's reassign arm to key on.
  Autonomous runs (`human_confirm: none`) keep the standard fail contract.
- **`unitDistributed.distinctnessFallback` (core#461 c).** The evaluator≠creator fallback — a
  review/test unit that STAYS on a seat that built what it checks because no eligible seat
  distinct from the builders admits it — was disclosed in `degradedReason` prose only, and only
  when a bench emptied the pool (a single-seat roster said nothing). It is now a first-class field
  on the event, the `Distribution` and core-ts' `UnitDistributedEventJson`: `"creator_seat"` when
  the fallback applies, `null` otherwise, emitted unconditionally (the `seatConstraint` rule);
  `degradedReason` reads exactly as before. The fallback seat is always a still-eligible one.
- **A roster with no eligible seat is refused at launch (core#461 d; F-RC2-041).** A launch whose
  every configured seat the launcher declared unusable (`health.usable: false`) was ACCEPTED and
  died 2 s later at distribution with an `error` event and no gate — after the composer had said
  "Ready to send" (run e5999520: "5 of 5 seats benched"). `launch_run` now refuses it synchronously
  when the plan needs a seat — the TYPED `NoEligibleSeat` error (the `RunBusy` / `RunExists` rule;
  `no eligible seat for <run>: N of N seats benched: <seat> (<cause> — launcher), …`) — with no
  session persisted; an empty roster is untouched (a tool-only plan needs no seat, F-E2E-011; a
  plan that does keeps its distribution-time refusal).
- **Routing benches a seat that is dead for the run, not only one that is signed out (F-7R3-001).**
  Run c7e42297 seated the review unit on copilot through `evaluator_distinct` after copilot had
  failed EVERY council ballot with "exceeded your monthly quota" — the wave-6 bench (#449) fired
  on `not_logged_in` and the launcher's `health.usable: false` only, so a quota-exhausted seat
  stayed routable and the run headed for a failure-escalation gate. The council now classifies
  two more causes on `councilSeatFailed.reason` — **`quota_exhausted`**, judged as a REFUSAL
  FRAME (the seats' own messages: a self-framed provider sentence or API code anywhere in the
  judged output — `exceeded your monthly quota`, `hit your usage limit`, `usage limit reached`,
  `too many requests`, `insufficient credits`, `credit balance is too low`, `billing details`,
  `payment required`, `rate_limit_error`, `insufficient_quota`, or a generic quota word — `quota`,
  `rate limit`, `usage limit`, as whole words — beside refusal phrasing — `exceeded`, `exhausted`,
  `reached`, `hit your`, `insufficient`, `out of`, … — on one of the last six lines and only under a non-zero exit; never `rate_limiter`, `RateLimiter`, `rate
  limiting`, `the usage limits section` or `the rate limit middleware has no tests` on their own,
  and never a bare `429`/`402`) and **`not_installed`** (a `Command::spawn` `NotFound`, judged
  from the error kind, never from text) — and the distribution keeps a per-seat ledger over every
  council it convened. A seat
  is benched for the run when a ballot fails authentication or cannot spawn (on first
  occurrence, as the auth rule always did), or when it returned NO vote on any ballot and every
  failure was quota-class, or it timed out on at least the dispatcher's own consecutive-failure
  streak (`WICKED_COUNCIL_SEAT_BENCH_THRESHOLD`, default 2) with no vote in between.
  Deny-dominates, in both directions: one successful ballot keeps the seat (a mixed record is
  not a dead seat) and an unclassified failure is not proof of one. Same bench, same
  `benched_seats` persistence, same `degradedReason` on every later `unitDistributed` — now
  naming the kind and the count: `1 of 5 seats benched: copilot (quota_exhausted (3/3 ballots)
  — ballot)`; a unit the council had handed to such a seat is reassigned with the cause
  ("council picked 'copilot', which exhausted its quota on its ballot (…); reassigned to
  'codex'"). The worker and judge paths classify with the same frame (`classify_refusal`:
  authentication over the whole transcript, the quota frame over its last 2 KiB) and bench by the
  same deny-dominates rule — `quota_exhausted` only while the seat has NO successful unit in the
  run (reason `quota_exhausted (no success in the run)`; one refusal beside a success is a flaky
  provider, not a dead seat), `not_logged_in` / `not_installed` on first occurrence; the wrapped
  runner's `(could not run …)` line is `not_installed`. **`UnitEvidence.judge_refusals`**
  (additive, `#[serde(default)]`) carries the judge's refusals with their cause beside the
  unchanged `judge_auth_refusals`. When the bench leaves no seat distinct from the builders, a
  review/test unit stays on its creator seat and `degradedReason` says so ("evaluator≠creator not
  enforceable for unit N: it stays on creator seat 'x' …") — never a routing error, never a silent
  stall; the gate still reports UNGATED unless the repo-checks floor gates it. A bench-free roster
  is unchanged on stderr and on the wire (a single seat stays silent, as before). Wire: additive
  (new `reason` tokens, new free text on `degradedReason`, one defaulted field); crew 0.7.31 keeps
  working. `BenchedSeat.reason` is free text and heterogeneous by design — the bare `not_logged_in`
  token, `quota_exhausted (k/n ballots)`, `not_installed (k/n ballots)`, `timed_out (k/n ballots,
  no vote returned)`, `quota_exhausted (no success in the run)`, or the launcher's own words —
  consumers render it, never parse it (api-types already declares `reason: string`; its doc line
  naming only `not_logged_in`/`unauthenticated` is crew's to refresh in 0.7.32).
- **core-ts build-from-source fixed; CI now syntax-checks the finalizer.** #444 left four unescaped
  backticks (`role`, `posture`, `role`) inside the template literal that `scripts/finalize-dts.mjs`
  emits the `CoreEventJson` doc block from, so the script no longer parsed (`SyntaxError: Unexpected
  identifier 'role'`) and `npm run build` / `build:debug` in `crates/wicked-core-ts` failed from
  source — wicked-crew's CI step "Build wicked-core-ts from source" went red on every PR. The
  published `wicked-core-ts@0.7.21` is unaffected: the release path ships the COMMITTED `index.d.ts`
  and never runs the finalizer. Nothing here caught it because nothing EXECUTED the script — the
  crate's lockstep test compares it as text (`include_str!`, unescaping \` first, so an escaped
  and an unescaped backtick read the same) and the tsc step `npm ci --ignore-scripts`. The core-ts
  CI job now `node --check`s the script AND runs it against the committed `index.d.ts`, requiring a
  byte-identical result. No shipped artifact changes; no version bump.
- **A plan whose every unit is a Tool executor needs no CLI seat (F-E2E-011).** On crew 0.7.31 +
  core-ts 0.7.22 every `onboarding` run failed about one second after launch: crew hands the
  tool-only workflow an empty seat pool by design (wicked-crew#533 — its two `wicked-estate`
  phases convene no council), and the wave-6 routing core (#449) refused the plan for an empty
  eligible seat set BEFORE it looked at executor types — `sessionStarted {cliCount: 0}` → "no
  eligible seat … every configured seat is benched — (sign a seat in, or add one …)" →
  `sessionFailed`, a remedy for a run that seats nobody. No registered repo got a graph. The seat
  requirement is now per UNIT: a plan whose every unit carries a `tool_cmd` is routed `tool`
  before any eligibility verdict (the bench still rides each distribution and is persisted as
  before), and the refusal fires only when at least one planned unit needs a seat — a tool unit
  beside an agent unit on an empty or all-benched roster is refused exactly as before, by the same
  message. Proven through the engine, not the routing function alone: the SEEDED `onboarding` def
  launched with `clis: []` against a registered repo distributes both units to `wicked-estate`,
  spawns them with the repo bound in, and completes; a tool + agent plan with `clis: []` still
  fails with the existing message; the #449 bench tests are unchanged. Wire: additive
  (`unitDistributed` reads exactly as before). No version bump.
- **Creators keep `Write`/`Edit` inside their granted write roots; the read-only fence is for
  evaluators (acceptance finding F-4R2-004, wave 5).** F-036's read-only posture was keyed on the
  worktree guard's marker alone (`worktree_guarded` = `!executes_code && !tool`), which is a
  SUPERSET of "evaluator": a CREATOR whose deliverable is a document outside the tree also declares
  `executes_code: false` — every wicked-crew interactive seam (`interactive-draft/edit/chat`:
  `draft`, `edit`, `revise`), steering `propose`, repo-learn `capture`, `memories` `store`. Those
  runs are UNBOUND and launch with `extra_write_roots` = the per-run inbox the deliverable must
  land in; the launch validated the root, the deliverable floor looked for the file there — and
  the ACP permission boundary refused the creator's `Write` of that very file before the gate (which
  carried the root in `boundary.roots.write`) could see it, logging "DENY (read-only evaluator,
  unit 2)" for a creator. `draft`/`edit` workers routed around it via `Bash`; both
  `interactive-chat` `revise` workers respected it → prose instead of `revised.html` → deliverable
  floor failed → `sessionFailed` (runs 37f020cc 10:39:39Z and 2c56cea1 10:48:29Z). The write
  posture is now derived per unit from its ROLE, the guard marker and whether the run has a tree
  (`write_posture::WritePosture::of`), one derivation read by every carrier: `executes_code: false`
  + evaluator/neutral ⇒ **read-only** (unchanged: every write-class call refused, bash stays);
  `executes_code: false` + creator + BOUND ⇒ **deliverable-roots** (writes allowed inside the
  launch-validated `extra_write_roots`, refused into the tree under review and anywhere else — the
  worktree guard still holds the tree); `executes_code: false` + creator + UNBOUND ⇒ no fence (the
  ordinary cwd + extras boundary; there is no tree to protect). `executes_code: false` keeps meaning
  "does not change the tree under review", never "writes nothing". Applied on the ACP permission
  bridge (`AcpWritePosture`, judged before any gate for admitted and unadmitted seats alike), the
  gate hook's phase scope (`WICKED_NO_CODE_SCOPE` keeps the `1` spelling for read-only — so a
  same-version pre-posture hook binary on PATH still reads an evaluator's fence as ON — and adds
  `deliverable-roots`; the creator's roots ride `WICKED_DELIVERABLE_ROOTS`, exactly the extras, so
  the hook and the ACP fence judge one identical root set — never the repo-graph key dir the
  filesystem boundary also admits), the wrapped argv lever and the PTY session
  (only the read-only posture takes `--sandbox read-only` / `--exclude-tools`; a deliverable-roots
  creator on a lever-less seat is guard-only and told so in its prompt), and process/session
  isolation + quiesce (any fenced unit). The read-only-requires-wrapped reroute applies to the
  read-only posture only — a creator is never sent to a carrier whose lever would refuse its
  deliverable. Wording: the log line names the posture and the role (`DENY (deliverable-roots
  posture, creator unit 2)`), the reason says `plays creator`/`plays evaluator`, and
  `evaluatorToolCallDenied` gains `role` (`creator` | `evaluator` | `neutral`) and `posture`
  (`read-only` | `deliverable-roots`) — the type name is historical; consumers read `role`. Tests:
  the posture table, the gate hook's creator fence (in-tree refused, declared root allowed, `..`
  judged on the resolved target, wording never says evaluator), the ACP fence (creator allowed
  inside its root / refused in-tree, outside, and path-less; evaluator refused everywhere), the
  reroute exemption, and a regression fixture replayed from run 2c56cea1's persisted unit, session
  and `session/request_permission` frame (`tests/fixtures/f_4r2_004_chat2_revise_write.json`).
  Crew's workflow declarations were already right (`role: creator`, `executes_code: false`,
  deliverable in `extraWriteRoots`) and are unchanged.
- **Chat scope validator hardening (core#410 follow-up; reviewer R11/b1–b3).** A `.`/`..` segment
  anywhere in a chat scope's cwd, read roots or graph is refused by spelling before any containment
  check (the worker-home rule, `spawn::refuse_dot_segments`), and a path whose MISSING tail still
  carries `..` is refused rather than re-appended lexically — `/tmp/missing/../../<state home>`
  could read as under the temp base while the kernel resolved it into the state home; launch extra
  read/write roots now refuse any `.`/`..` segment in their spelling (was: only an unresolvable
  tail). Every read root must be an existing DIRECTORY (a hard link to `core.db` placed elsewhere, a
  plain file or a missing path is refused). Scoped-chat admission now also rests on the PROCESS the
  spawn produced: a seat relying on the kernel write floor is refused when the floor did not arm
  (`sandbox_downgrade`), a governance-reliant seat when the version pin did not prove the admitted
  adapter (`governance_verified == false`) — reported as `ChatSessionFailed` with the reason.
- **Per-seat configuration roots for EVERY CLI; a seat's startup banner never reaches the answer
  (acceptance findings F-010 / F-068, core#410).** FINDING-061 / F-030 isolated the CLAUDE seat
  (`CLAUDE_CONFIG_DIR` → the engine-owned worker home); every other seat kept running on the
  OPERATOR's own configuration — `~/.codex`, `~/.pi/agent`, `~/.copilot`, `~/.config/opencode` +
  `~/.local/share/opencode` — so a chat seat loaded the operator's personal skills and extensions
  (a retired skill set) and pi streamed its 4.8 KB startup banner listing them as the answer's
  opening, and a fresh `WICKED_WORKER_HOME` reported claude signed-out but the others signed-in off
  the operator's logins. One resolver now decides every seat spawn — ACP worker AND chat, council
  ballot, wrapped worker (`wicked_apps_core::spawn::seat_config_for` / `SeatCli`): claude keeps
  `<worker home>/claude`; codex gets `CODEX_HOME=<worker home>/codex`; pi
  `PI_CODING_AGENT_DIR=<worker home>/pi`; copilot `COPILOT_HOME=<worker home>/copilot`; opencode
  `XDG_CONFIG_HOME` / `XDG_DATA_HOME` / `XDG_STATE_HOME` under `<worker home>/opencode/{config,data,
  state}` (its own `OPENCODE_CONFIG_DIR` only ADDS a directory — the global one is still read — so
  the XDG bases move; documented side effect: git/gh inside an opencode seat resolve their XDG
  config there too). Every FOREIGN seat variable (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`,
  `PI_CODING_AGENT_DIR`, `COPILOT_HOME`, `OPENCODE_CONFIG_DIR`) is STRIPPED; roots are created
  private (0700) and no-follow checked; the `WICKED_WORKER_INHERIT_OPERATOR_CONFIG` hatch inherits
  everything, as before. opencode's extra config file (`OPENCODE_CONFIG`), inline document
  (`OPENCODE_CONFIG_CONTENT`) and inline credentials (`OPENCODE_AUTH_CONTENT`) are stripped too
  (the skills composition still bases itself on the daemon's ambient document for governed opencode
  units with delivery — unchanged here); agy, which has no config-home variable, runs quiet
  (`AGY_CLI_HIDE_LOGO=1`, `AGY_CLI_HIDE_ACCOUNT_INFO=1`). **Behaviour change:** codex / pi /
  copilot / opencode seats now start signed OUT until the operator signs in the seat root once —
  every seat's `login_invocation` names
  it (`CODEX_HOME="<root>" codex login --device-auth`, …), so the studio's Sign-in terminal signs in
  the directory the seats read; copilot's OAuth token stays in the per-user keychain (config
  isolated, keychain not). agy has no known configuration-home variable and is isolated by stripping
  alone. A stream-aware `BannerGate` on chat turns holds only a pi-banner-shaped head and releases
  everything else at once, so the banner never enters `ChatDelta` (the assembled reply was already
  stripped at its seam).
- **Chats run in a recorded SCOPE, never the daemon's cwd (F-067, core#410 / crew#502).**
  `chat_open` used to fall back to `std::env::current_dir()` — the DAEMON's working directory — and
  advertised no estate MCP ("repo-less exploration"). It now takes a `ChatScope { cwd,
  code_graph_db, read_roots }` recorded per chat: the seats run in the scope's cwd (omitted ⇒ a
  private `<tmp>/wicked-core-chat-<id>` of the chat's own), their `session/new` advertises the
  READ-ONLY estate MCP over the scope's graph (DES-GROUNDING-001 — the grounding governed workers
  get) and the scoped repository roots as a claude seat's `additionalDirectories`; a seat re-warmed
  after an eviction lands in the same scope; `chat_list` reports it. **Behaviour change:** a
  SCOPED chat admits only seats that can be held to its read-only roots — an adapter admitted to
  input governance (claude, opencode) or a seat whose `[cli.acp]` record arms `os_sandbox`;
  pi / codex / copilot / agy (as registered: no permission requests, no floor) are refused for
  scoped chats by name with the remedy, and admitted to unscoped chats. The read roots are read-only
  IN FACT, for every seat: a claude seat's session fence denies `Edit`/`Write`/`NotebookEdit` under
  them, and every seat's `session/request_permission` is judged against the chat's boundary (the
  scratch root writable, the roots readable, nothing path-bearing beyond either) — a chat turn used
  to answer every permission request `allow`. The scope is validated before it is recorded:
  absolute roots outside the engine's own trees (`validate_extra_read_roots`), an existing graph
  that is never a top-level file of the engine's own state home. core-ts: `chatOpen(chatId,
  clisJson, cwd?, scopeJson?)` (`{"codeGraphDb"?, "readRoots"?}`), `chatSend`'s `cwd` accepted and
  ignored, `chatList` rows gain `cwd` / `codeGraphDb` / `readRoots`.

### Added
- **codex skills lever — the engine-minted `CODEX_HOME/skills` is populated from the pinned
  snapshot (acceptance finding F-079, core#441; closes the core#400 residual).** codex has no
  per-launch flag for skills, so until core#426 minted a per-seat `CODEX_HOME` it had no
  wicked-owned lever (`Absent`): a skill-bearing codex unit was refused `NoLever` by name and a
  skill-less one told its method was "NOT loaded". `SkillsLever::CodexSkillsDir` now populates
  `<CODEX_HOME>/skills/<frontmatter name>/` — the deliverable portable skills FLAT by name (the
  shape crew's `views/copilot` takes), COPIES (codex forbids symlinks; the read-only bit is cleared
  so a later generation can remove them on Windows too), a skill's OWN files with any indexed
  skill nested below it excluded (it lands under its own name), and a `.wicked-skills-gen` marker
  keyed by generation + content hash so an unchanged generation is a no-op and a new one replaces
  exactly the entries the previous one wrote; nothing else under `CODEX_HOME` — never the
  operator's own `~/.codex` — is touched (an entry no marker lists is carried across unchanged; one
  that collides with a skill's name refuses by path). Populations of one seat home are SERIALIZED
  by an exclusive OS file lock (`<CODEX_HOME>/.wicked-skills.lock`) — every unit of every run on a
  daemon shares the one `<worker>/codex`, and units run on parallel threads — and every generation
  is built COMPLETE in a uniquely named sibling directory and swapped into place by rename, so a
  seat never spawns against a half-built tree, a waiter re-checks the marker under the lock, and
  crash debris is swept on the next population. Both carriers populate BEFORE the seat spawns and
  before `skillsSnapshotHanded {path: "wrapped_cli" | "acp", cli: codex}` is emitted, through the
  one seat-config resolver; a failure refuses the launch (`SkillsError::SeatHome`), never a launch
  without the skills its directive names. The lever is carrier-independent (codex behind
  `codex-acp` is judged as codex, like pi behind `pi-acp`). Under the inherit-operator-config
  hatch there is no minted home, so codex is handed NOTHING: a skill-bearing codex unit is refused
  `NoLever` naming the hatch, a skill-less one runs exactly as before (no population, no
  handoff). Admission's portability and parent-nesting rules apply to codex exactly as to pi;
  `NONPORTABLE_SEAT` and the views are unchanged.
- **Seats are handed the garden launcher root, and the ACP skills lever is judged from the SEAT
  binary (acceptance finding F-079, core#441).** Every seat that receives a skills delivery — pi,
  copilot, opencode, claude, on both carriers — now also gets `WICKED_GARDEN_ROOT=<pinned snapshot
  root>` and `<root>/scripts` at the front of `PATH`, both derived from the SAME generation the
  skills came from (`SkillsDelivery` carries its root), so wicked-garden's `wicked-garden` launcher
  resolves the snapshot's synced `.venv` and never an npm package of another version; a seat handed
  nothing (codex, agy) gets neither. Over ACP the lever is decided off the SEAT binary for the
  levers that ride the environment: pi behind the separate `pi-acp` bridge is judged as pi (was
  `Absent` — handed nothing, told its skill was "NOT loaded in this session", and the rig's pi unit
  emitted no `skillsSnapshotHanded`), and the bridge is handed the deliverable portable skill
  directories as `WICKED_PI_SKILL_DIRS` (one OS path-list — `:` / `;` — in `--skill` order; the
  crew-side bridge, wicked-crew#531, turns it into `--no-skills --skill …`; the variable being
  SET is the delivery — an EMPTY value is a delivery of zero portable skills and means
  `--no-skills` alone, exactly what the wrapped carrier spells, so discovery is off on both
  carriers; UNSET means no delivery), with
  `skillsSnapshotHanded {path: "acp", cli: <seat key>}` emitted once per spawn; a skill-bearing pi
  unit over ACP is no longer refused `NoLever`. An argv-only lever (copilot's `--add-dir`) still
  needs the carrier to BE the CLI. A skill directory that cannot be spelled in the path-list
  refuses the launch naming the variable (never a truncated delivery). Admission,
  `NONPORTABLE_SEAT`, `deliverable_portable` and the views are unchanged; codex stays lever-less
  (follow-up on the same issue).
- **Dead letters carry when and who; the emit seam gets a read side and a drain (wicked-crew#495,
  acceptance finding F-022).** Every record the emit seam spools to the dead-letter outbox
  (`wicked-apps-core::emit`, `WICKED_APPS_EMIT_DEADLETTER`) is now stamped with `ts` (epoch
  milliseconds, the CoreEvent convention), `pid`, and `origin` — the launcher's "who am I" from the
  new `WICKED_APPS_EMIT_ORIGIN` variable (`emit::ORIGIN_ENV`; wicked-crew `serve` exports
  `wicked-crew@<version> serve pid=… port=… db=…`), absent when unset. Before this a host-wide
  outbox of 3,400+ governance events (every default install dead-lettered every conformance claim,
  phase transition and rule-lifecycle event, because nothing set `WICKED_ESTATE_DB`) had no
  recoverable order and no way to tell two daemons' entries apart. Two library functions and two
  `Core` statics on `wicked-core-ts` close the loop: `emit::count_events(&store)` /
  `Core.eventStoreCount(dbPath)` (EVENT nodes on a store, read-only — crew's `/diagnostics`
  reports `governance.records.total` / `sinceBoot`), and `emit::replay_outbox(path, &mut store)` /
  `Core.replayEmitOutbox(outboxPath, dbPath)` (each record lands as the EVENT node it should have
  been, with its ORIGINAL `ts` restored so id order stays chronological, plus `replayed: true`,
  `replayed_at_ms`, `deadletter_reason` and `spooled_by` provenance; torn or non-record lines are
  reported verbatim in `{ read, replayed, already_present, failed: [{ line, reason }] }`, never
  re-spooled and never fatal — behind `wicked-crew governance replay`). Replay is IDEMPOTENT: a
  replayed node's id is the spool line's SHA-256 (first 16 hex) plus its original stamp — never the
  replaying pid or a fresh sequence — so the same line replayed twice (a second run on an archive,
  a restore-and-retry) lands once and is reported `already_present`; each record is its own
  autocommit upsert, so a failed record leaves no open batch for the next one to commit into and is
  repaired by the next replay. Byte-identical UNSTAMPED lines (pre-stamp dead letters) are
  content-addressed with no stamp to tell them apart, so they conflate onto one node (the second
  is `already_present`); stamped lines never conflate unless `ts` and content both match. The
  `open shared store failed: …` reason redacts URL userinfo before it reaches stderr or the spool —
  greedily, up to the URL's last `@`, so a raw password containing `/`, `?` or `#` is still
  redacted — and `replay_outbox` redacts a legacy record's `deadletter_reason` / `spooled_by`
  before they become store metadata (#428). No default changes: the spool path resolution and the
  store resolution are untouched; crew resolves and exports both variables from its state home.
- **Evaluator phases cannot mutate the worktree; `verify` runs the repo's own checks as a
  deterministic floor (F-036 / F-039).** The acceptance run's `bug/verify` evaluator (codex,
  unchecked — governance is claude-only) REWROTE the fix it was reviewing and passed its own gate
  on the one criterion "the run left a change in its worktree", after a `fix` gate that had
  evaluated nothing (`agentVerdict: None, hasDeterministicFloor: false, evaluatorPolicies: []`).
  Four changes close it, for EVERY seat and carrier, independent of the CLI's governance adapter:
  - **Worktree guard** (`worktree_guard`): for a def-driven, agent-executed unit whose phase declared
    `executes_code: false` (`WorkUnit.worktree_guarded`, plan-derived like `pre_build_scope`), the
    actor snapshots the worktree at dispatch — git TREE HASH over tracked + untracked-not-ignored
    content via a scratch index seeded from `HEAD`'s tree (`read-tree HEAD`, then `add -A`
    re-hashes every path — never copied from the real index, so no `assume-unchanged` bit can hide
    a rewrite, and never empty, so a committed path that an ignore rule matches stays tracked and
    its rewrite is seen; the real index, refs and worktree are never touched; the engine's own
    `tmp/` scratch is excluded by construction through `core.excludesFile`) plus `HEAD`, taken
    THROUGH the git directory PINNED from the
    REGISTERED repository (`<repo>/.git/worktrees/<id>`, found from the repo side — never through
    the worktree's own `.git` file, which an evaluator can redirect to a repository it controls;
    both escapes were reproduced by the independent review) — and persists it ON the unit
    (`worktree_baseline` with its `git_dir`, restart-durable); the final comparison reuses exactly
    that directory, and a baseline without the pin is unverifiable, never clean. The FINAL
    comparison is taken after everything the phase owned has run — the seat's process group killed
    on exit (the wrapped runner now spawns the seat in its own group and `killpg`s it when it
    exits, is cancelled or times out), the agent judge rendered, the repo checks run — immediately
    before the result is posted to the gate fold, so a delayed write from a backgrounded process or
    a "passing" check script that edits a tracked file is caught too. ANY path that differs, or
    a moved `HEAD`, DENIES the unit (deny-dominates, source `worktree_guard`, judged BEFORE the
    pinned diff floor that would otherwise pass a rewritten tree) and emits the new
    `evaluatorMutatedWorktree` event (`session, ord, attempt, cli, phase, beforeTree, afterTree,
    headMoved, changed[{status,path}]`). There are NO exemptions — not documentation, not a
    declared deliverable, not tool state: an evaluator's write-up belongs in its output, a phase
    whose deliverable must live in the tree is a code phase (`domain-extraction/coverage` now
    declares `executes_code: true` — it writes `coverage-report.json` for its pinned validator —
    and is therefore never guarded), and an in-tree code graph moving under a recon phase is a
    defect to surface (core#406). Fail-closed: a guarded unit whose
    outcome is missing or unverifiable is denied, never assumed clean. The guard denies; it does not
    revert — the denial names the paths, both tree ids and the one-line `git read-tree --reset -u
    <before>` restore. A human APPROVING a mutation-denied gate re-baselines the re-dispatch; a
    restart-driven redrive keeps the persisted baseline. Tool units and prose-planned runs are never
    guarded.
  - **Read-only posture for no-code phases on non-claude seats** — ONE launch boundary for every
    argv-building carrier (`execute_wrapped::apply_no_code_posture`: the wrapped one-shot runner
    AND the persistent PTY session runner), applied to the template's own tokens and the seat's
    resolved posture. Seats are recognised SOLELY by the normalised file stem of the RESOLVED
    binary — the template's argv[0] as an absolute path or looked up on `PATH`
    (`/opt/homebrew/bin/codex`, `codex.exe`) — never by the seat's registry key, so a seat NAMED
    `codex` that runs some other binary is unknown and its write grants are refused rather than
    rewritten: codex runs `--sandbox read-only` with EVERY write-capable token dropped (`--yolo`
    included — codex's alias of the bypass, which beats a later read-only sandbox)
    (every sandbox spelling rewritten, the bypass and `--full-auto` dropped, one appended when none
    was declared); pi gets `--exclude-tools edit,write` (merged into an existing denylist). A
    lever-less seat whose template or posture carries ANY recognised write grant
    (`--allow-all-tools`, `--allow-all`, `--dangerously-skip-permissions`,
    `--dangerously-bypass-approvals-and-sandbox`, `--full-auto`, `--yolo`, `--auto`,
    `--approve-all`, or codex's `--sandbox workspace-write`/`danger-full-access` spellings on an
    unrecognised seat) has the launch REFUSED before any process spawns or PTY opens (`read-only
    posture refused the launch: …`, naming the token, seat and clis.toml remedy); a lever-less seat
    with a bare posture runs, and the existing `governanceUnenforced` reason says the worktree
    guard — not the posture — is what holds the line. The governed carriers gain the matching
    **NO-CODE phase scope**: `WICKED_NO_CODE_SCOPE` on the hook subprocess,
    `BoundaryCtx::no_code_scope` on the ACP carrier — `Write`/`Edit`/`NotebookEdit` to ANYTHING in
    the worktree is refused up front for ANY `executes_code: false` phase (the pre-build scope
    keeps its documentation allowance; the no-code scope permits nothing, matching the guard). The
    persistent PTY session carrier never reuses a session across postures: an `executes_code:
    false` phase reaching a creator-opened (write-posture) session closes it and opens a fresh
    read-only one (and a code phase never inherits a read-only session), and a no-code phase's
    session is closed — its whole process group `killpg`ed, SIGTERM then an UNCONDITIONAL SIGKILL
    (a TERM-trapping, stdio-detached descendant no longer survives a teardown whose reader
    already saw EOF) — the moment its turn ends, before the guard's final snapshot, so a writer
    the seat backgrounded cannot land after the comparison. The ACP carrier, persistent per run
    too, follows the same rule: an `executes_code: false` unit never reuses the process that
    served a write turn (a fresh bridge is started), that process is killed — group and all, the
    bridge's kill handle now `killpg`s — the moment the unit ends and BEFORE `run_unit` returns,
    and every bridge teardown reaps bounded.
  - **The code-writing Creator's gate evaluates something** (`bug/fix`, `feature/build`,
    `migration/execute` — compiled defs and the shipped `workflows/*.json`): each now pins the
    built-in evidence floor, so layer 1 re-derives the diff and layer 2 has a seat DISTINCT from
    the creator judge it. Registration REFUSES an `executes_code` agent phase with no
    `validator_pin` and no `human_confirm` gate — `WorkflowDefError::GateEvaluatesNothing`, "gate
    evaluates nothing: fix — … pin the built-in evidence floor (…), a phase-specific validator, or
    gate the phase with human_confirm" — and judges the def AS AUTHORED: `carry_shadowed_pins` and
    `enforce_verified_evidence` are gone from the load path (nothing is injected or restored at
    registration), so a same-id overlay copy that dropped a shipped pin is refused exactly like a
    def that never had one and the registered built-in stands, and a `verified_evidence` phase
    with no `validator_pin` is refused by name too (`WorkflowDefError::UnverifiedEvidence`,
    "verified_evidence declared but nothing pinned: <phase>"). The shipped defs pin every gate
    explicitly, in code and in `workflows/*.json`. `workflow::ungated_code_phases(&def)` is the
    pure lint a consumer runs first. Tool phases are exempt (their exit code is their gate).
    **Coupling to note:** wicked-crew composes per-run defs (`feature-pr`, …) from a mirror of the
    shipped defs — a mirror without these pins is refused at registration until the crew release
    that carries them (wicked-crew#507) is deployed alongside this engine.
  - **Repo checks floor** (`repo_checks`): for the def's code-VERIFYING unit (`verified_evidence`
    with an `executes_code` Creator upstream — `bug/verify`, `feature/test`, `migration/verify`;
    `WorkUnit.repo_checks_floor`), the engine runs the repository's OWN checks in the worktree
    after the seat's work, off the actor thread: `package.json` `typecheck`/`lint`/`test` scripts
    (those present, in that order, via the lockfile's package manager; `npm ci`/`install` first
    when `node_modules/` is absent — ALWAYS `--ignore-scripts`) and `Cargo.toml` → `cargo test`;
    each under a 20-minute bound (15 for install), own process group, killed on exit. The checks are
    repo-controlled code and run CONTAINED: inside the worker OS write boundary
    (`validator::detect_worker_sandbox` — macOS `sandbox-exec` / Linux `bwrap`, writes confined to
    the worktree, secret dirs unreadable, network open) with an isolated `HOME`, `npm_config_cache`,
    `CARGO_HOME`, `CARGO_TARGET_DIR` and `XDG_*` under `<worktree>/tmp/wicked-checks/`
    (`RUSTUP_HOME` preserved; `npm install --no-package-lock` when the repo ships no lockfile, so no
    build artifact lands in the reviewed tree) — and a MINIMAL environment: the daemon's env is
    cleared and only `PATH`, locale, `TERM`, `USER`, the Windows shell essentials, `RUSTUP_HOME`
    and those overrides are passed, so no token, API key or `WICKED_*` variable reaches a check
    (a repo-controlled script with the network open could otherwise read them). The stdout/stderr
    drains are BOUNDED after the group kill (a `setsid`-detached descendant holding the pipe
    cannot wedge the verify unit; the tail read so far is reported); a checkout that ships `tmp`
    as a symlink is refused, never followed, when the scratch is prepared; `cargo test` runs
    `--locked` when the repo ships a `Cargo.lock`, and when it ships none the `Cargo.lock` cargo
    writes — provably the engine's own (absent at detection, after the seat was quiesced) — is
    removed after the checks and disclosed on the report (`engine_writes_removed`), so the guard
    never denies the engine's side effect. A host where NO
    write boundary can be armed (no
    `sandbox-exec`/`bwrap` — all of Windows) does NOT run the checks: the floor FAILS with
    `sandbox_error` (+ `sandbox_level: "best-effort"` and the probe's reason) and the gate denies —
    repo-controlled scripts never run unsandboxed. Detection is fail-closed: an unreadable or
    malformed `package.json`, or a symlinked manifest/lockfile/`node_modules`, FAILS the floor with
    the reason — every probe `lstat`s the entry, opens it `O_NOFOLLOW`, and `fstat`s the OPENED
    descriptor (regular file, same device + inode) before a byte is read, so there is no
    lstat-then-open window — only a repo with no manifest at all is a disclosed
    vacuous pass (`checks: []`, `passed: true`). Exit code, duration and 4 KiB stdout/stderr TAILS
    ride the new `repoChecksEvaluated` event (`session, ord, attempt, passed, criterion,
    checks[{name, argv, source, exitCode, timedOut, spawnError, durationMs, stdoutTail,
    stderrTail}], skipped`) and the unit record (`repo_checks`); a non-zero exit, timeout or
    unspawnable command DENIES (source `repo_checks`), stopping at the first failure.
    `gateEvaluated.hasDeterministicFloor` is now true when EITHER instrument ran and `criterion`
    names both (`; `-joined); `deterministicPass` covers both. Checks are not run over a tree the
    guard's first look already caught being rewritten. `Command::ApplyStepResult` and the bus
    `task.completed` payload carry the new `UnitEvidence {worktree_guard, repo_checks}`
    (serde-default; a pre-evidence payload folds a GUARDED unit closed).
  Tests: an evaluator whose fake seat edits a file is denied with the path listed and the event
  emitted (real repo + worktree, through the actor); a PASSING `npm test` that appends to a tracked
  file is caught by the final comparison; a backgrounded writer dies with the seat's process group;
  nothing is exempt — a documentation file, a declared deliverable and tool state all deny; a
  code phase with no pin is refused at registration ("gate evaluates nothing: fix"), a same-id
  drop-in that drops a shipped pin is refused and the built-in stands, `verified_evidence` without
  a pin is refused; the floor captures a failing `cargo test` (exit 101 + the assertion text in
  the tail) and a failing `npm test` (exit 3); a check that writes outside the worktree fails the
  floor and never lands (where the host has a sandbox tool), and an injected best-effort probe
  makes the floor fail closed and run NOTHING; a malformed or symlinked manifest fails the floor by
  name; the codex argv for a no-code phase is `--sandbox read-only` by the RESOLVED binary's stem
  (bare name on `PATH`, absolute path) — never by key, a `codex`-keyed seat on another binary is
  refused — on both carriers; a write-capable lever-less seat is refused before launch on both
  carriers; a creator's PTY session is never reused by the evaluator that follows (fresh read-only
  session, closed with its process group when its turn ends — a backgrounded writer never lands).
  `wicked-core-ts` pins both new wire shapes and documents them in `CoreEventJson`; its
  `package-lock.json` now pins the five platform packages to the published 0.7.17 artifacts
  (`npm ci` on current npm refuses lock entries without a version — the types-test step).
- **Seat selection honours skill portability (#401)** — distribution narrows a unit's candidate
  seats to what its skills admit BEFORE the council votes: a unit whose `skill_ref` (or a
  transitive mandate) is `portable: false` in the handed snapshot — or whose root is the
  Claude-only live-cache fallback — is seated only on a claude seat (both carriers; one candidate
  takes the truthful 1-of-1 path, no ballot), and evaluator ≠ creator never moves such a unit
  onto a seat that cannot take it. A roster with NO eligible seat is refused at plan time, before
  the first unit does any work (`SkillsError::NoEligibleSeat`, naming the skill, its portability,
  the seat kind required and the roster) — never a council pick the ladder then refuses by name
  mid-run. `UnitDistributed` gains an additive `seatConstraint` (`null` when unconstrained;
  `routingMethod` and its fields read as before), mirrored in `wicked-core-ts`'s `CoreEventJson`
  doc. Portable skills, skill-free units, tool units and a run with no resolvable root route
  exactly as before (the launch admission stays the enforcer). Live evidence (2026-09-09): a
  `capture-learnings` run carrying `wicked-garden-repo-learn` (`portable: false`) was routed
  unit 1 → copilot, refused correctly, escalated, and could only be cancelled. Seat eligibility is
  read off the SAME resolutions the carriers execute — `acp_runner::acp_seat_identity` (the merged
  registry record by key, `clis.toml` overrides included) and `execute_wrapped::wrapped_seat_identity`
  (the launch template) — never the roster record's own fields, so a seat that passes routing as
  claude cannot run as anything else; `wicked-core-ts` declares `UnitDistributedEventJson`
  (literal `type`, `seatConstraint: string | null`) with compile-time assertions
  (`types-test/`, `npm run typecheck`) and a cargo lockstep test over `index.d.ts` that reads a
  CRLF checkout as LF; `finalize-dts.mjs` normalizes `index.d.ts` to LF, and `.gitattributes` pins
  the binding's hand-authored files to `eol=lf`. Every test that drives the wrapped/ACP exec path
  or a seat resolution now holds `test_env::ENV_LOCK` (audited).
- **Skills snapshot on both worker paths (#396)** — the engine consumes one skills input,
  `WICKED_SKILLS_SNAPSHOT` (the ABSOLUTE path of a crew-published, immutable garden-shaped plugin
  root; pinned to its canonical real path — relative paths and symlinks at ANY component, the last
  included, are config errors: crew resolves `current` before the handoff, see review pass 6), and hands it to each
  worker through the mechanism its CLI has, copying nothing: Claude over ACP gets it in
  `session/new` as `_meta.claudeCode.options.plugins = [{type:"local", path}]` (merged into the
  existing options) and the snapshot is BOUND to the cached session — every later turn of that
  session prompts, admits, read-widens and reports against the generation it was opened with, never
  a re-resolved `current`; wrapped Claude gets exactly one `--plugin-dir <snapshot>`, and any
  `--plugin-dir` a `clis.toml` template carries is stripped ALWAYS (snapshot or not) with a logged
  notice — a template is not a skills input. The snapshot joins the READ roots on both governance
  carriers (never a write root). **The worker Read fence over crew's state home is an explicit,
  tested denylist (design v3.1 §1)**: the snapshot lives under crew's one storage root
  (`~/.wicked-crew/skills/snapshots/<gen>/`), which the fence denies and whose deny would beat any
  allow, so when the handed snapshot sits exactly in that read slot the state home's blanket
  `Read(…/**)` is replaced by the STATIC rules of the state-home registry
  (`tests/fixtures/state-home-subtrees.json`, embedded via `include_str!` in `state_home.rs` and
  mirrored by crew) — one rule per registered top-level entry (`core.db*`, `bus.db*`, `daemon-*`,
  `audit.log`, `evals/**`, `project-graphs/**`, `interactive-*` …) and one per denied child of the
  skills root (`baseline/**`, `effective/**`, `manifest.json`, `current`, `.uv-cache/**`,
  `snapshots/.staging-*/**`, `snapshots/.tmp-*/**`); `Edit`/`Write` keep the blanket. Fail closed,
  never widen: no runtime listing builds a rule; the launch-time listing only REFUSES — a top-level
  entry (or a child of `skills/` other than the read slot) the registry does not classify fails
  admission by name (`fence_check`), and a snapshot under the state home outside the slot or under
  any other fenced directory is a config error naming both paths. Documented residuals: an entry
  created under the state home while a session runs is fenced at the next launch (crew is that
  directory's only writer); a generation published while a session runs is readable by that
  session until its next launch (v3.3 — see review pass 4 below; sibling generations are DENIED). The
  published index is VERIFIED at load (every component from the root down — `.claude-plugin/`,
  `plugin.json`, `snapshot.json`, `skills/`, each skill directory, each `SKILL.md` — is lstat-checked
  not to be a symlink and read without following one (`O_NOFOLLOW` on unix); a `skills -> /outside`
  link is refused at `skills`; `portable` is required; all defects are listed in one error); the
  live-cache fallback skips a nameless `SKILL.md` with a `skills.notice` instead of deriving an
  identity. Admission judges EXISTENCE plan-wide (the run's `skill_ref`s via
  `StepInput.required_skills`, expanded through transitive frontmatter/index `mandates`; a missing
  skill of ANY family is refused by name — no family is exempt, the snapshot is the worker's only
  skills source) and INVOCABILITY per seat (v3.1 §5: only the skills THIS unit's seat invokes are
  judged — a NESTED skill is refused for a Claude seat, since Claude Code discovers plugin skills one
  directory deep and names them by directory (verified against the live roster: 92/92 top-level
  garden dirs, 0/50 nested), a `portable: false` skill for every non-Claude seat — so a Codex unit
  is not refused because a Claude unit elsewhere needs a non-portable skill, nor a Claude unit
  because a mirror seat uses a nested one). **Design v3.2: skills reach a non-Claude seat ONLY
  through a per-launch, wicked-owned lever, decided off the binary the launch actually runs** —
  pi: `--no-skills` + one `--skill <snapshot>/skills/<dir>` per portable skill; copilot:
  `--add-dir <snapshot>/views/copilot` (crew publishes that view; a generation without one has no
  copilot lever); opencode 1.17: `OPENCODE_CONFIG_CONTENT.skills.paths` composed WITH the seat's
  governance content (verified in the installed binary); codex 0.153 and any ACP bridge that is a
  separate program (`pi-acp`, `codex-acp`): no lever. No lever ⇒ no skills, never a side channel: a
  unit that invokes a skill on such a seat is REFUSED naming the skill and the reason
  (`SkillsError::NoLever`), and nothing wicked does writes into `~/.codex`, `~/.pi`, `~/.copilot`,
  `~/.config/opencode` or `~/.claude` (pinned by a test that launches every seat against fake copies
  of those trees). The skill directive is CLI-aware (`wicked-garden:<top-level dir>` + the Skill-tool
  clause for Claude; the mirrored frontmatter name, no Skill-tool clause, for a seat with a lever;
  a "NOT loaded" form for a seat without one). **Exactly one input (v3.1 §2)**: `WICKED_SKILLS_SNAPSHOT`
  unset → the live installed plugin cache with a `skills.fallback` log (never the hand copy; the
  fallback sits inside the `~/.claude` fence and says so with a notice); set-but-EMPTY or invalid →
  config error; a final-component link (`current`, dangling or not) is a config error naming its
  target. `WICKED_SKILLS_CURRENT` is withdrawn — crew resolves `current` and passes the
  concrete generation. **Session-specific ACP configuration (v3.1 §3)**: the shared worker-home
  `settings.json` carries only the launch-independent fence (every fenced directory except the
  state home), written atomically; each session's fence rides its own `session/new`
  `_meta.claudeCode.options` (`disallowedTools`, merged by the bridge; `settings` = a per-session
  file at `<worker home>/sessions/<run_id>-<cli_key>/settings.json`, created fresh per launch,
  tmp+rename, reaped with the run) — never a shared mutable file two launches can race on.
  **Cached session first (v3.1 §4)**: a cached ACP session is admitted against the snapshot it was
  opened with (`proc.skills`) BEFORE any ambient resolution; only a fresh session resolves
  `current` — so a `current` that moves to a generation dropping a skill the pinned one has does
  not refuse the cached session's next turn. New `CoreEvent::SkillsSnapshotHanded`
  (`skillsSnapshotHanded`: session/ord/attempt/path/cli/gen/contentHash/root/source) reports the
  generation each launch used (every seat with a delivery, not only Claude) so crew can reap old
  generations safely. Tests: env mutation across the crate serializes on one crate-wide lock; the
  ladder's tests compare paths as `Path`s (the Windows separator-spelling failure); two-generation
  concurrency is exercised with barrier-released threads on both carriers, reading the per-session
  settings files the fake bridge received. **Review pass 3 (codex round 3 on #399):** the fence
  follows the ACTUAL state home — derived from the snapshot's own path
  (`<state home>/skills/snapshots/<gen>`, three components up; `state_home::of_snapshot`) — in
  round 3 also required to agree with an explicit `WICKED_CREW_STATE_HOME` when the daemon passed
  one (crew#480); that companion variable is RETIRED in review pass 6 (design v3.4 §2) — never from a `.wicked-crew`
  basename, so a scratch daemon's `/private/tmp/crew-state` has ITS sibling stores classified and
  fenced by the registry (unclassified ⇒ refused by name) while the default `~/.wicked-crew` keeps
  its blanket; a snapshot without that shape is a config error at load. The copilot view is
  VERIFIED at admission: `views`, `views/copilot`, `.github`, `.github/skills`, each required
  skill's dir and `SKILL.md` are lstat-walked (a symlink anywhere ⇒ config error, even for a unit
  invoking nothing — the launch would still `--add-dir` it), and every skill the seat invokes must
  be present in the view with a matching frontmatter `name` — an empty or partial view ⇒
  `SkillsError::Missing` naming the skills. Per-session ACP settings are collision-free: each
  launch gets `<worker home>/sessions/<run>-<cli>-<pid>-<seq>/` via an exclusive `create_dir`
  (EEXIST ⇒ a fresh suffix; never removing another launch's directory), the owning `AcpProcess`
  reaps only its own directory on drop, and ids that sanitize alike (`campaign:one` /
  `campaign_one`) cannot collide. A malformed `OPENCODE_CONFIG_CONTENT` (not a JSON object, or a
  `skills`/`skills.paths` that cannot take the paths) FAILS the launch on both carriers
  (`SkillsError::LeverConfig`, the unit refused by name — never composed onto a bare document);
  the ACP carrier refuses before spawning rather than falling back. Directory levers (pi
  `--skill`, opencode `skills.paths`, scanned recursively) never deliver a portable parent whose
  directory nests a non-portable skill — its portable descendants are delivered on their own paths
  and a non-Claude unit invoking the parent is refused (`SkillsError::NestsNonPortable`, parent and
  child named). Frontmatter is parsed with YAML semantics (`serde_yaml`): `name: x # comment` is
  `x`, `mandates: [a, b] # comment` is two mandates, quoted scalars/block lists/flow lists all
  read; an unterminated flow list, a tab in indentation, an unterminated block or a non-string
  `name` is a config error naming the file (the live walk skips it with a notice). The Windows
  clippy `unused_mut` in `private_dir` is gone (`private_dir_builder` has one body per `cfg`).
  Live evidence is now POSITIVE INVOCATION on both carriers, `#[ignore]`d and opt-in
  (`WICKED_SKILLS_LIVE_TEST=1`): `tests/skills_live.rs` launches the real `claude --plugin-dir
  <fixture>` with the unit's `skill_ref` set and asserts exit `Ok` plus the fixture skill's unique
  marker in the output; `acp_runner::tests::the_real_acp_bridge_…` drives the real
  `claude-agent-acp` through the real `AcpStepRunner` (`session/new` with the plugins option, then
  the prompt) and asserts the same marker in the streamed output. **Review pass 4 (codex round 4
  on #399; design amendment v3.3):** the handed generation is the ONLY readable path under the state
  home — `skills/snapshots/` is the one directory listed at launch to BUILD rules, one deny per
  sibling entry (every other generation, every `.staging-*`/`.tmp-*` entry; in addition to the
  static registry rules), failing closed on an unlistable slot or an entry that is neither a
  generation directory (a real directory named by decimal digits, as crew publishes them) nor a
  recognised staging name — round 3 left every sibling generation readable, which the fence test
  now asserts the opposite of; residual: a generation published while a session runs is readable by
  that session until its next launch. Path hygiene everywhere a persisted spelling is joined (v3.3
  §3): an index entry's `name` (joined onto the copilot view) and `dir` (onto `skills/`) and every
  view entry must be safe relative segments — no absolute, drive (`C:/x`), verbatim/UNC (`\\?\…`,
  any backslash), `.`/`..` or extra separators — refused at index verification, all defects listed;
  after every join the lstat-walked path must also canonicalize INSIDE its root before it is read.
  The copilot view is validated as a WHOLE tree (v3.3 §2): every entry of `views/copilot/.github/skills/`
  must be an indexed PORTABLE skill whose `SKILL.md` name matches, no symlink anywhere below the
  view, no stray file, no unindexed or non-portable entry, no Claude-only child nested inside a
  copy — each refuses the launch by name as a config error, whether or not the unit invokes a skill
  (the launch `--add-dir`s the whole view). The recursive-nesting restriction
  (`SkillsError::NestsNonPortable`) applies only to the levers that hand over ORIGINAL directories
  (pi `--skill`, opencode `skills.paths`); copilot is judged on its published view (a copy that
  excludes the non-portable child admits the parent), and a lever-less seat gets `NoLever`. ONE
  admission policy for fresh and cached ACP sessions (`skills_snapshot::admit_turn`): a cached
  session is judged against its pinned generation (the round-4 rule that the inherit-config escape
  hatch bypassed admission is reversed in review pass 6 — the hatch bypasses nothing). The shared
  worker-home settings writer's temp files are `settings.json.<pid>.<seq>.tmp`
  and the sweep removes only THIS process's leftovers — never another engine process's in-flight
  temp, whose rename it would otherwise race. Windows CI: the acp_runner test scratch helper is
  platform-independent (a non-`cfg(unix)` test used a `cfg(unix)` helper and the lib tests did not
  compile). **Review pass 5 (codex round 5 on #399):** the live-cache FALLBACK root is contained
  before it is traversed — `root` and `root/skills` are lstat-checked (no symlink component) and
  every indexed entry must canonicalize inside the root — so a `skills -> /outside` link in the
  installed plugin is REFUSED as a fallback (`SkillsError::Fallback`, naming the root), never
  indexed (round 4 lstat-checked only the children and handed an external tree's paths to
  pi/opencode); enumeration errors in the live walk propagate instead of reading as "no skills".
  The copilot view is enumerated from `views/copilot` ITSELF: it may hold exactly `.github`, which
  may hold exactly `skills`; any other file, directory or symlink at either level
  (`views/copilot/leak`, `.github/copilot-instructions.md`, `.github/workflows`) refuses the launch
  by name, since the whole directory is what `--add-dir` delivers (round 4 started at
  `.github/skills`). A cached ACP session that was opened WITHOUT a snapshot (`proc.skills =
  None`, no root on the ladder then) is told apart from a fresh launch (`skills_snapshot::Turn::
  {Fresh, Cached(Option<_>)}`): a skill-bearing turn on it is REFUSED (`SkillsError::NotDelivered`,
  naming the skills and advising a fresh session) instead of admitted off the ambient configuration
  — the plugin reaches a session only at `session/new`, so round 4 admitted such a turn, sent no
  handshake and still generated the invocation directive; a skill-free turn on it still runs, and a
  fresh session under the now-available snapshot is handed it. The shared worker-home
  `settings.json` is REPLACED, never unlinked first: the pid/seq temp is `rename`d over the target
  (atomic; a planted link is replaced as a link), so a concurrent reader in another engine process
  or a running CLI sees the previous or the new file and never none, and a failed write leaves the
  previous content — only a planted non-file, non-link entry (a directory) is cleared beforehand.
  Test hygiene: every test reading the inherit-config escape hatch or HOME through
  `inject_isolation_flags`/`deny_rules` now holds the crate-wide env lock (read side) — the
  round-4 plugin-flag regression raced the ACP test pinning `WICKED_WORKER_INHERIT_OPERATOR_CONFIG`
  — and the Unix-only recording-bridge helpers (`EnvPin`, `ledger_entries`) are `#[cfg(unix)]`, which
  is what failed the round-4 Windows clippy job (`dead_code` under `-D warnings`). **Review pass 6
  (codex round 6 on #399; design amendment v3.4 §2):** the engine input is EXACTLY ONE variable —
  `WICKED_CREW_STATE_HOME` (round 4's "passed alongside" state home) is RETIRED and not read
  anywhere; the state home is derived from `WICKED_SKILLS_SNAPSHOT`'s fixed layout alone
  (`<state home>/skills/snapshots/<gen>`: parent literally `snapshots`, grandparent literally
  `skills`, else a config error naming the path; the subtree registry fixture is unchanged;
  documented residual: a custom state home is fenced only through a snapshot handed from it —
  crew#480's `engine-env.ts` stops exporting the variable). The inherit-config escape hatch
  bypasses NOTHING of the skills contract: admission runs under it unchanged (an invalid explicit
  snapshot is a launch error, a missing required skill a refusal by name, a cached session that
  never received a plugin still refuses a skill turn), the snapshot is still handed
  (`--plugin-dir` / `session/new` plugins) and a template `--plugin-dir` is still stripped with a
  notice — the hatch decides only whether the operator's ambient configuration is inherited IN
  ADDITION (the isolation flags / the engine-minted worker home are what it withholds). EVERY
  component of the snapshot path, the LAST included, must be a real directory: a handed `current`
  link (dangling or not) is a config error naming the link, its target and the real path to pass
  — crew resolves `current` before the handoff, so a fresh launch can never change generation
  between two units of one run. Snapshot IDENTITY is verified: `snapshot.json.gen` must equal the
  generation directory's name (numerically — crew zero-pads the directory), the directory must be
  a generation name, `contentHash` and `gardenSource` (`{kind, path, plugin_version, baseline}`,
  crew's field names) are REQUIRED, and the generation the engine REPORTS (`skills.snapshot gen=`,
  `SkillsSnapshotHanded.gen`) is the verified directory name, never the index's unverified claim;
  a skill is keyed by its frontmatter `name` ONLY — an index `name` that is not the path-derived
  name (`wicked-garden-<dir joined by ->`) is a load-time defect naming both, never an alias
  (rounds 1–5 resolved a ref by the derived name too). The state-home fence checks each
  classified top-level entry's ACTUAL kind (lstat) against the registry's declared kind — a
  directory named `audit.log` or `daemon-x`, a file named `evals`, a symlink of any classified
  name (`skills`, `audit.log`) — and refuses the launch naming the entry, since the rule emitted
  follows the declared kind and would otherwise leave the entry uncovered. The run-wide skills
  EXISTENCE admission runs before the FIRST unit of ANY kind: a TOOL-COMMAND unit is admitted
  (`skills_snapshot::admit_plan`, off the actor thread, same ladder and refusal shape) before its
  command executes, so a run whose later agent unit names a missing skill fails at unit 1 without
  the command mutating anything (`tests/skills_plan_admission.rs` drives a real `Core`). Both
  carriers have DETERMINISTIC, normally-executed, no-network integration coverage: wrapped — a
  fake `claude` first on PATH recording its argv, a REGISTRY seat whose template carries the
  stop-gap `--plugin-dir`, exactly one `--plugin-dir` (the snapshot) with the registry's stripped,
  with and without the hatch; ACP — the recording bridge (stdio JSON-RPC echoing `session/new`)
  through the real `AcpStepRunner`, `_meta.claudeCode.options.plugins == [{type: local, path}]`
  merged beside `disallowedTools` and `settings`; the live `#[ignore]` tests remain opt-in extras.
  **Review pass 7 (CI on pass 6 + Copilot):** the loader reads a snapshot row's `dir` as crew
  writes it — PLUGIN-relative, `skills/<dir>` (crew's row validator requires the prefix) — and
  strips the prefix for the engine's under-`skills/` key; a row without it is a config error
  naming crew's spelling (rounds 1–6 read the field as already relative to `skills/`, so a real
  generation would have failed to load at its first skill — found while building the hermetic
  e2e fixture). `tests/domain_extraction_e2e.rs` is hermetic: `setup` publishes a crew-shaped
  fixture generation (`tests/support/skills_snapshot_fixture.rs`: gen == directory, 64-hex
  `contentHash`, `gardenSource`, `venv`, rows with `dir: skills/<…>`/`portable`/`nested`, the
  `views` block, `skills/<dir>/SKILL.md` per referenced name) under a canonical per-process
  temp state home and hands it via `WICKED_SKILLS_SNAPSHOT` in its set-once block — the runs
  passed locally only because the ladder's fallback found the developer's installed garden, an
  ambient dependency a hermetic CI runner does not have — and both governed-run tests assert from
  the run's events that generation `000001` was the one admitted: a TOOL-COMMAND unit's
  plan-wide admission now reports the verified generation it judged the plan by as
  `SkillsSnapshotHanded { path: "tool_cmd", cli: "tool" }` (crew's ledger pins the generation
  for the session from its first unit; `admit_plan` returns the admitted root). Copilot:
  `attach_skills_plugin` de-duplicates by canonical path (the same snapshot already in
  `plugins` — identical or another spelling of the same real directory — gains no second entry;
  the ACP twin of the single `--plugin-dir`); `parse_registry` refuses a non-string
  `denied_children` entry or a non-array `denied_children` (never a silently narrower fence);
  `tests/skills_live.rs` pins its variables with an RAII guard restored on panic;
  `PersistentStepRunner::exec_turn` resolves the session invocation ONCE and passes it to both
  the argv and the skill form (the docstring's single-resolution guarantee now holds by
  construction). **Review pass 8 (windows CI on pass 7):** the integration tests compare the
  generation an engine refusal/event names by canonical IDENTITY (`names_generation`,
  `refused_snapshot_path` in `tests/support/skills_snapshot_fixture.rs`), never by spelling — the
  windows runner's `temp_dir()` (an 8.3 short name) and `canonicalize` (which keeps the `\\?\`
  verbatim prefix) spell one directory differently from the engine's `\\?\`-free canonical form.
  **Review pass 9 (codex round 8 REJECT + windows CI on pass 8):** worker isolation is MANDATORY
  — a registry template that states `--setting-sources` or `--permission-mode` (either spelling)
  is a config error naming the template and the flag (`isolation_refusal`; rounds 1–7 deferred to
  it), a template's own `--disallowedTools` is UNIONED into the engine's single deny flag, and the
  deny fence is injected even under the inherit-config hatch (which withholds only the two
  scope/mode flags) — on both carriers (ACP: `disallowedTools` rides the frame under the hatch;
  `settingSources: ["project","local"]` is SET on every non-hatch `session/new`, the ACP analog
  of `--setting-sources`). Every `--plugin-dir` in the BUILT argv (placeholders expanded) is
  stripped except the prompt token, and exactly one (the snapshot) is asserted, else refused.
  `.venv` containment: every component of `<state_home>/skills/baseline/<64-hex>/.venv` is
  lstat-checked (no link but the snapshot-root `.venv` itself), the link's target is resolved
  lexically (crew's relative spelling) and must END exactly at `<64-hex>/.venv`; the state-home
  fence refuses any symlink among the skills root's children except `current`, and requires the
  read slot to be a real directory. The live-cache FALLBACK gets the same fail-closed whole-tree
  containment as a published generation (any symlink, an unreadable/non-UTF-8/nameless
  `SKILL.md`, a link under the support tree ⇒ `Ladder::Failed` with the reason; `SkillsError::
  Fallback` is gone). The engine's OWN operational state home — the canonical parent of the
  database it was spawned on (crew's `stateHomeOfDb`; no environment input, v3.4 §2 stands) — is
  fenced on EVERY launch (blanket without a snapshot, registry with one from it) and kept out of
  the shared worker-home file: `AcpStepRunner::new_for_store` / `WrappedCliStepRunner::
  with_tx_for_store` carry it; `admit_turn`/`fence_check`/`deny_rules` take it. The persistent
  PTY carrier REFUSES skill-bearing units by name (`SkillsError::CarrierWithoutSkills`) and never
  emits a directive (ADJUDICATED: it has no snapshot lever). The two live `#[ignore]` tests use
  ISOLATED state (`WICKED_SKILLS_LIVE_CLAUDE_CONFIG_DIR`, `WICKED_SKILLS_LIVE_WORKER_HOME` —
  refused when they resolve to the operator's real dirs), and the load+invoke proof stays
  adjudicated to the integrated functional test. Windows CI: the plan-admission test's tool
  command is spelled for `cmd.exe` (no `\\?\` verbatim prefix). **Review pass 10 (windows clippy
  on pass 9 + crew fixture mirror, v3.5 §2 + codex round 9 REJECT):** the worker fence FAILS
  CLOSED on a protected directory the rule syntax cannot spell — a comma, a POSIX backslash, a
  non-UTF-8 component ⇒ the launch is refused naming the directory and the character, on the
  argv, in the settings file, in the shared worker-home file, and under the inherit hatch (rounds
  1–8 logged and skipped it). The state-home registry is crew's fixture byte for byte: every
  settled AND transient name the store can create under `skills/` (`.staging-*`,
  `manifest.json.tmp-*`, `refused/`, `snapshots/.staging-*`, `snapshots/.tmp-current-*`) with its
  declared kind (`denied_children_kinds`, required to cover the children exactly); the launch-time
  listing of the skills root and of the read slot classifies each child by pattern AND lstat kind
  (a `current` that is a directory, a `.staging-*` that is a file, a `.tmp-current-*` that is not
  a link, an unknown name ⇒ refused; a parked publish's transients ⇒ admitted and denied). A
  set-but-empty `WICKED_SKILLS_SNAPSHOT` stays a config error naming the variable (v3.5 §4). The
  persistent PTY carrier refuses PLAN-WIDE: a run with any skill-bearing unit is refused at its
  first unit (`StepInput::required_skills`), before any session opens. ACP cleanup never swallows
  errors (a failed session-dir removal is logged; an unlistable worker home is an error). Documented
  residual (ADJUDICATED): codex has no lever and no engine-minted worker home — it runs under the
  operator's own `~/.codex`, which v3.2 forbids touching — so "no lever ⇒ no skills" means wicked
  delivers nothing and refuses skill-bearing codex units by name; `CODEX_HOME` isolation is
  core#400. Windows: the two test-module wrappers used only by Unix tests are `#[cfg(unix)]`, and
  the cfg audit ignores path-qualified and commented mentions (it had missed exactly those two).
  **Review pass 11 (confirmation review, one wire mismatch + one Copilot thread):** the snapshot's
  `.venv` link is BOUND to its metadata as crew's `verifyCurrent` binds it — the link's `<64-hex>`
  must equal `snapshot.json.gardenSource.baseline` (a link into another, equally valid baseline
  env is refused naming both hashes), the link may exist only when `snapshot.json.venv` is
  `synced` (present while `skipped`/`pending`/`failed` ⇒ refused), and a `synced` generation
  without the link is refused too; `gardenSource.baseline` must be a sha256 content hash and
  `venv` one of crew's four states, both required at load (`SkillsSnapshot::{baseline, venv}`).
  The gate-hook wiring audit matches the stable prefix of the `assemble_read_roots` call, not
  rustfmt's trailing comma. **Review pass 12 (one Copilot thread):** every containment and
  identity comparison in the worker fence goes through ONE canonical spelling
  (`state_home::canonical_spelling`: canonicalize + the Windows `\\?\` verbatim prefix dropped)
  and a whole-component prefix check (`under_spelled`) — `base_under` compared a simplified
  snapshot root against an UNSIMPLIFIED canonical denied directory, so on Windows a root under a
  denied directory could escape and `fence_check` failed to refuse (fail-open on that OS only);
  the `.venv` link's absolute target is simplified before its lexical check too. Unit-tested with
  Windows-shaped spellings on every OS.
- **Operator-authored `effect` in markdown steering rules + eval rule coverage (#395, #394).**
  The markdown doc lane gains the enforcement half of a steering rule: a frontmatter
  `effect: deny|warn|allow` key (rides onto every rule the doc mints) plus per-rule `effect:`
  and `trigger: <regex>` continuation directives, so an operator can author a rule the gate
  actually fires — until now no operator-facing path yielded an effect, every doc rule landed
  recall-only, and evals (which credit only `deny` firings) measured exactly the corpus's
  good/bad split. Absent the key nothing changes (every existing doc stays recall-only);
  INV-S3 is surfaced at parse with the doc + rule (`effect` without `applies_to`, malformed
  trigger regex, `trigger:` on an effect-less rule). `EvalReport` gains `rule_coverage`
  `{ exercised, unexercised: [{rule_id, steering_type}], recall_only, per_type }` — the
  decide-lane rules eligible for the run (narrowed by `--type`) that some sample fired vs.
  none did (the second blind spot: a rule no sample exercises produced no row and was
  invisible to `summary.gaps`), plus the count of effect-less rules the eval structurally
  cannot measure. `rules eval` prints the block and warns when nothing is decide-lane. Public
  wire shape change (additive) — crew/studio consume it via the next core-ts release.
  `rules eval --corpus` (and `--import <name> <path>`) now also take ONE corpus `*.json` file
  (the documented `{name, samples}` shape, a bare array, or a sample) so a script-derived
  corpus replays against a scratch store without an import (`CorpusSource::File`).
- **Wrapped units see their prior context; ASSUMPTION markers are anchored; `unitDispatched.baseSkill`
  says whether the discipline was handed (DES-L4 PR-⑥; core #470 / F-RC1-094, F-RC1-096, #479).**
  The ACP carrier injected a `depends_on` unit's prior-phase outputs as text blocks behind a
  FINDING-024 preamble, but the wrapped (argv) carrier's `unit_prompt` read `prior_outputs` nowhere —
  a pi/codex reviewer or creator saw no findings to act on. The preamble is now ONE const
  (`PRIOR_CONTEXT_PREAMBLE`, read by both carriers) and the wrapped exec prepends it plus the
  `<label>\n<output>` blocks to the argv prompt (empty when the unit has none; the pty composer is
  unreachable in production and untouched); each block's output is clipped like the evaluator's review
  target (`clip_review_target`, 24 KiB head + 24 KiB tail, elision marked) so the argv prompt stays
  under Linux's 128 KiB per-argument cap (E2BIG) after verbose creators. `assumptions::parse` anchors the marker after the list /
  quote gutters (`strip_prefix`, not `find`): a token quoted mid-sentence is prose, no longer a
  malformed record. `BaseSkill` gains `handed: bool` — the seat's CLI has a per-launch skills lever
  and intake proved the admitted generation holds the skill — derived at dispatch from BOTH carriers' seat identities
  (`acp_seat_identity` / `wrapped_seat_identity`; only their agreement reads `true`); additive on the wire (**core-ts** `index.d.ts` doc regenerated once through
  `scripts/finalize-dts.mjs`; api-types 0.38.0 already types `BaseSkill.handed?`).

- **The fold reads the evaluator's OWN verdict; a missing or non-PASS `VERDICT:` line denies INTO
  THE HUMAN GATE, never `sessionFailed` (DES-L1 PR-1A, D-9; core #488, F-RC1-131; des-adjudicated
  §4.1).** A reviewer wrote `VERDICT: FAIL` and the run shipped the tree: the only verdict parser
  was the layer-2 judge's over the CREATOR's cold output, and nothing read the Evaluator-role
  unit's own words. `apply_and_finish_unit` now parses the unit's output for every Evaluator AGENT
  unit (`PhaseRole::Evaluator`, no `tool_cmd`, not engine-internal — the same predicate as the
  `EVALUATOR_VERDICT_CONVENTION` line `skill_prompt` hands the seat): after trim and leading
  decoration, the LAST line whose first token splits on `:`/`=` into `VERDICT` decides, `PASS` is
  the only pass, no alias table — `FAIL`, `CONDITIONAL`, any other token, a bare head and NO line
  all deny as `UnitDenial{source: "evaluator_verdict"}` through the existing `escalate_denied_unit`
  → `gateEscalated{condition: "verdict_not_pass", denialSource: "evaluator_verdict",
  verdictSummary: <findings>}` → `awaitingHuman{gateKind: "escalation"}`. The slot sits after the
  deterministic floors and before the judge, so the reviewer's findings name the gate when both
  deny. `gateEvaluated` gains `evaluatorVerdict: string | null` (additive; the decisive token, null
  when the layer did not read the unit or the evaluator wrote no line — then the denial is the
  twin). The generic verdict-gate prompt names its three arms (retry · request changes · reject).
  The deterministic stub (`StubStepRunner` and the legacy sync path — one `stub_output` emitter)
  closes an Evaluator unit with `VERDICT: PASS` so stub-engine runs do not park.
- **The gate gains `request_changes` and `amendScope: creator`; attempts mint from each unit's own
  history; the deliver frame reads the verify floor (DES-L1 PR-1B; core #459, #465; des-adjudicated
  §4.7; L1↔L2 contract).** At a NOT-PASS review the operator's only answers were retry-the-same-review
  or cancel. `HumanDecision` gains `RequestChanges { note }`: the run rewinds to the most recent
  creator before the gated unit (or the cursor when it is one) — every unit from it on loses its
  worktree baseline / mutation and its denial (a stale baseline would make the re-run evaluator
  restore the OLD tree), the creator goes `Distributed` with `rework_of = <review ord>`, later units
  `Pending`, the cursor moves there at a fresh attempt, `verified_tree` is cleared, and the creator
  re-runs with the REJECTED review in its prior-context block (`[review — unit N — requested
  changes]`; ACP today, wrapped under L4 ⑥, the pty seat sees the marker + note). The description
  carries only a ≤ 160 B single-line marker that REPLACES its predecessor (` (requested changes
  r<n>: <head>)`); the full findings + note ride `unitReworkAmended{scope: "request_changes"}`. No
  creator before the gate ⇒ an error naming the remedy (approve = retry, or reject). `Approve` gains
  `amend_scope` (`cursor` = today | `creator` = the first creator phase at/after the cursor, so an
  intake steer lands on the phase that implements); an amendment the target already carries is not
  appended twice. `unitReworkAmended` gains `scope: cursor | creator | request_changes` (additive).
  Additive `WorkUnit.last_attempt` (written at the fold) and `rework_of` (cleared on approve); the
  advance and the crash redrive mint `next_attempt` / `redrive_attempt` from the unit's own history —
  a re-run never reuses a `(run, unit, attempt)` key, a never-run unit stays at attempt 0, and the
  redrive moves past the key the crash interrupted. The dead-seat gate prompt says "never seated"
  for a unit that never ran (L3's clause). core-ts `confirmGate(runId, approve, amend?, action?,
  amendScope?)` — absent `action` = today's mapping; a disagreement rejects before the engine is
  asked. The deliver TOOL unit's `repoChecksEvaluated.floor` reads `verify` (was `creator`).
  Reject = cancel is unchanged (D-2). Real-engine journey test: FAIL review → gate → request changes
  → creator re-runs at attempt 1 with the review in context → review re-runs at attempt 1 → PASS →
  completed.
- **Amendment scoping proven end to end; `unitReworkAmended.scope` renders the arm (DES-L1 PR-1C;
  core #465).** A steer approved with `amendScope: creator` at a read-only unit's gate lands on the
  first creator phase ONLY: the read-only cursor is dispatched with its own description, no unit's
  prior-context block ever carries the steer (the engine hands OUTPUTS, never descriptions), and
  exactly one `unitReworkAmended{ord: <creator>, scope: "creator"}` fires — a real-engine journey
  pins it, beside the control that today's default (absent `amendScope` = `cursor`) still lands the
  steer on the read-only cursor, the shape #465 reported, kept as the default by ruling (BC-04: the
  studio sends `creator` whenever the cursor is not a creator). The `scope` token renders on the
  wire for all three arms (`cursor` | `creator` | `request_changes`).
- **Campaigns gain `denial_gate: hold | auto_reject` — an unattended campaign no longer parks forever
  at the engine's escalation gate (DES-L1 PR-1D; core #484).** `CampaignDef` gains the additive
  `denial_gate` (absent ⇒ `hold`, today). Under `auto_reject`, when a node's run pauses,
  `on_node_awaiting` reads the run's DURABLE open gate row (`interaction_requests.gate_kind`) — never
  the prompt's wording — and answers an `escalation` gate (a denied unit) with Reject through the same
  `actor::confirm_gate` arm an operator's Reject takes (D-2: cancel): `campaignNodeAwaitingHuman` is
  disclosed first, the run cancels, the node reconciles to `Cancelled` and its dependents follow the
  `OnSuccess` edge rule. "Escalation" is EVERY `escalate_denied_unit` class — a verdict, floor or
  boundary denial AND the dead-seat gate (a quota-exhausted seat cancels the node under `auto_reject`
  instead of waiting for a reassign). Def-, run-level and deliver gates HOLD under both policies — a
  gate the def or the launch asked for is never answered for the operator. Campaign NODES only: a
  single run (crew's interactive chat/draft/demo/edit subscribers included) still parks as today.
  Real-engine test: `auto_reject` cancels
  the escalation-gated node and holds the `human_confirm: all` node; `hold` parks both. core-ts
  `launchCampaign` doc lists the field (snake_case wire).

- **Linux floor: bwrap masks only the secret dirs that EXIST (core #460 / #493 / #415, F-SMOKE-001;
  DES-L2 §5).** The checks-sandbox argv pushed `--tmpfs <HOME>/<dir>` for every one of the six curated
  secret dirs with no existence check; bwrap `mkdir`s a missing `--tmpfs` destination and, under
  `--ro-bind / /`, dies BEFORE exec — `bwrap: Can't mkdir <HOME>/.aws: Read-only file system`, exit 1.
  Every repo-checks floor and every pinned validator on a Linux daemon whose `HOME` lacked one of the
  six therefore failed and blamed the work (`install` "failed"; `pinned validator failed: <criterion>`).
  The loop now takes `filter(is_dir)` — a missing dir has nothing to mask and its parent is read-only
  inside the jail, so nothing widens. A Linux regression test (`HOME` lacking the dirs → the wrapper
  runs `/bin/true`) rides the CI ubuntu leg, which now installs bubblewrap so the `#[cfg(unix)]`
  sandbox tests arm the real jail instead of printing their skip.
- **A launcher that fails to arm is never the check's failure (core #493, #460).** ONE predicate
  (`validator::launcher_failure`: an armed wrapper + a first stderr line of `bwrap:` / `sandbox-exec:`
  on a non-zero exit) at both floor spawn sites: the repo-checks floor records the check as
  `could_not_run` with `spawnError: "the OS sandbox launcher exited before the check ran: …"`, and the deterministic
  validator reports `Unrunnable` (rendered `pinned validator COULD NOT BE RUN … the OS sandbox launcher
  exited before the check ran: …`). Both still deny (fail-closed); the attribution is now honest. The validator's
  stderr is teed for the classification — it reaches the daemon log exactly as before.
- **The checks' `TMPDIR` leaves the worktree (core #489, F-RC1-132 / F-RC2-009b; DES-L2 §5 2A).**
  The floor set `TMPDIR` to `<worktree>/tmp/wicked-checks/tmp`; on a deep worktree (the RC1 rig's
  was 136 bytes) any test binding a Unix socket under it overflowed `sun_path` (104 bytes on macOS)
  and failed with `listen EINVAL` on every retry — 35 failures in wicked-bus's suite the code did
  not have. `TMPDIR`/`TMP`/`TEMP` now point at `<system temp>/wc-<6 random hex>` (mode 0700, drawn
  per floor, an existing path at the drawn name refused for a fresh draw, reaped with the floor),
  armed as the boundary's second write root — `run_floor` now prepares the scratch, THEN probes the
  sandbox with both roots. The env record's `tmpdir` changes accordingly (58 bytes on a macOS
  per-user temp dir). The worktree scratch keeps every other leaf.
- **Head and base cargo runs no longer share one `CARGO_TARGET_DIR` (core #480; DES-L2 §5 2B).**
  `cargo-target/head` vs `cargo-target/base`: cargo's mtime freshness check could hand a head check
  the base's test binaries through the shared dir (the contamination behind the benchmark's false
  regression, F-BM-009).
- **Windows names its gap (core #416).** With no OS write boundary the sandbox probe's reason on
  Windows now reads `Windows has no OS write boundary the engine can arm — no sandbox-exec/bwrap
  equivalent; the repository-checks floor never runs on this OS and a code-verifying phase fails
  closed at its gate; run the daemon on macOS/Linux or verify in CI` instead of the (true, silent)
  `no OS-sandbox tool on PATH`.
- **`.wicked/checks.json` `e2e` (core #482 item 3 / F-3R2-023; DES-L2 §5 2D).** An end-to-end
  suite the floor runs at the VERIFY stage only, after `test`/`test_targeted` — never at the
  creator floor; the deliver re-verify is a verify-stage floor and runs it too (BC-12 / BC-07). Nothing is auto-detected (absent or `false` ⇒ no `e2e` check); `timeout_s` and
  the baseline diff apply, and a run base that lacks the key fails the diff CLOSED ("the run base
  declares no `e2e` check (the change introduced it)"). The key set is now `typecheck · lint ·
  test · test_targeted · e2e · full · baseline_diff · timeout_s`; unknown keys are still refused.
- **Deliver honours the baseline diff (core #489 / F-RC1-132 / P7; DES-L2 §5 2E, D-22).** The
  deliver re-verify ran the floor with no base (`run_forcing_install` → `FloorContext::default()`),
  so ANY red check denied the deliver — including 35 failures the tip shared and qe verify had
  just passed. Deliver now calls the ONE floor entry `run_floor` with a real context: stage
  `verify`, the install forced on lockfile drift as before, base = the tip the work was lifted onto
  (else the run's base commit, now carried on `LiftContext`), the PINNED git dir. A failure the
  base shares is classified and does not deny; a head-only failure is a `regression` and does. With
  a base known the floor prefers the repo's `test_targeted` (unless `full: true`) and runs `e2e`,
  like verify. `run_forcing_install` is deleted (its only caller). No base at all ⇒ any red check
  denies, as before.
- **The repo-checks floor heartbeats on the unit's transcript stream (crew #581, F-BM-010).**
  After the worker returned, the floor (`typecheck`/`lint`/`test`, up to 3600 s per check) ran on
  the same thread with only `eprintln!` — the unit's live-output stream went silent for its whole
  duration, and crew's stall watchdog read run 8's 25-min `test` as a wedged worker and
  re-dispatched a second creator into the same worktree. `run_floor` is now wrapped in
  `with_floor_heartbeat`: the EXISTING `emit_delta` sink (→ `unitOutputDelta`) carries
  `repo checks floor (creator|verify): running the repository's own checks — N min` at the start
  and every 5 min until the floor returns. No new frame, hook, setting or `FloorContext` change;
  per-check durations stay on `repoChecksEvaluated.checks[].durationMs`. Disclosed: a
  `workerStallMinutes` below 5 would still read a live floor as stalled (default 15).
- **Cancel run KILLS a live Tool executor child (core#500, F-BM-008 — a cancelled run opened a PR
  2.5 h later).** The tool carrier had no lifecycle: `dispatch_unit` gave a tool unit `launch_seq
  0` (the "no launch" sentinel) and `run_tool_cmd` blocked on `Command::output`, so `CancelRun`,
  `ReassignUnit` and shutdown — which invalidate every other carrier's launch identity — reached
  nothing, and run 6's deliver script committed, pushed and opened a PR hours after `runCancelled`.
  A tool unit now takes the run's launch identity (`begin_launch(.., false)`, no epoch), the child
  runs in its own process group and is polled every 50 ms with the wrapped carrier's own
  `has_exited_unreaped` / `kill_child_tree` / `reap_bounded` loop; an invalidated identity kills
  the group (SIGKILL; Windows: the leader only), posts `StepStatus::Cancelled` with a `[killed:
  <reason>]` transcript tail (discarded by the existing stale guards) and ONE additive frame
  `toolExecutorKilled{session, ord, attempt, pid, reason, ranMs}` after `runCancelled` (or after
  `unitReassigned` → `toolExecutorDispatched` on a supersede). `reason` is the signal the child
  observed and may read `superseded` on an operator cancel — consumers key on order. Shutdown kills
  best-effort with no frame. Natural exits are byte-identical. Disclosed: SIGKILL skips the deliver
  script's `trap … EXIT` (one temp dir leaks per killed deliver); a cancel between `git push` and
  `gh pr create` leaves `wicked/<run>` on the remote with no PR; on Windows only the leader dies —
  a `gh pr create` already running completes; on a NATURAL exit the leader's group is quiesced
  (anything the script backgrounded and left running is killed with the phase, as for seats).
- **A free-text problem plans exactly ONE unit, the brief verbatim (D-11; core#393, crew #471 /
  #473, F-090).** `plan_units` split the operator's prose on newlines, sentence terminators
  (`.`/`!`/`?`) and semicolons and minted one unit — one council — per piece, so a three-paragraph
  recon brief became 11 councils per repo and the sentence "launch nothing until approved" ran as its
  own `build` unit. `split_problem` is deleted: the trimmed problem is the single unit's description
  (newlines kept — the live carriers pass the prompt as an argv element / JSON string and carry no
  line limit). Def-driven plans (`plan_from_def`) are unchanged.
- **A run whose EVERY seat is benched at distribution parks at a `dead_seat` escalation gate
  instead of dying in ~2 s (D-10; core#473-M1 = F-RC2-007, core#466, core#379, R5b).** The two
  all-benched `bail!`s in `distribute` (launcher bench on a re-plan; every seat benched on its own
  council ballots) now return the SAME typed `NoEligibleSeat` the intake raises (+ `benched_seats`
  as data; `Display` byte-identical — crew's intake parser still matches); `Command::PlanFailed`
  carries `anyhow::Error`; the arm downcasts and — instead of `sessionFailed` — persists the bench,
  seats every still-undistributed agent unit provisionally on the roster's first seat, writes the
  `dead_seat` denial on the cursor (still `Pending`, attempt unchanged) and takes the one denial
  route (`gateEscalated{condition: dead_seat}` → `awaitingHuman{gateKind: escalation}`): Reject
  cancels, Approve retries on that seat (a still-dead seat re-gates one unit at a time),
  `/reassign {cli}` names another, `{cli:null}` re-councils over a CLEARED bench (was the run's
  bench — a no-op after a sign-in). The skills-constraint refusal still fails the run. The ballot
  ledger gains one arm: an UNCLASSIFIED persistent failure (non-zero exit on every ballot, no vote)
  benches the seat at the threshold — crew's cross-run council-count ledger is deleted in its wave-3
  release. `councilSeatFailed.stderr/stdout/detail` and the dead-seat gate's `verdictSummary` are
  redacted (`redact_paths`: home, worker home, temp roots, other users' homes → tokens; classified
  on the raw text first; cap 4096 kept, not #466's 400 B). A `clis.toml` override that omits
  `[cli.acp]` over a built-in that carries one now warns on stderr (the seat runs wrapped and
  ungoverned; the wholesale-replace rule is unchanged).
- **The opencode seat's harness config denies the `task` tool; a tool call the seat itself refused
  is recorded in the unit transcript (F-W1-002; FIX-IT-ALL L4; BC-75 — proposed, user decision
  owed).** opencode 1.17.18's ACP layer forwards a `permission.asked` only for sessions it opened
  itself (`acp/permission.ts` `process()` → `tryGet(sessionID)` → return when absent; the session
  store holds only `session/new|load|resume|fork`): a `task` (explore) subagent's asks — six
  `external_directory` reads in the wave-1 P6 chat — never reached wicked-core and were never
  rejected either, so the seat sat on a 600 s dead turn until the budget killed it (upstream
  anomalyco/opencode#48232; fix PRs #48326 / #37902 open). One token in the one mechanism that
  already governs the seat: `OPENCODE_CONFIG_CONTENT` `permission` gains `"task":"deny"`
  (`wicked-council` registry) — the subagent is never spawned under ACP; the root session's own
  asks are answered exactly as before. And so the refusal is visible: `handle_update` records
  **any** `tool_call` / `tool_call_update` that ends `status: failed` — on every ACP seat, a seat's
  own refusal or a failed read / shell command alike — as `[tool call failed] <title>: <the seat's
  text>` in the unit transcript (bounded like a chunk; `locations` still collected from the update
  frame only), so the governance token scan over the transcript now also sees tool-error text; the
  text is appended unredacted — transcript redaction is the run-wide policy, not this change.
  Behaviour change register: **BC-75** (proposed — user decision owed: a seat-visible tool denial,
  and every failed tool call now in the transcript).
- **core-ts 0.7.29** — 2026-09-15 — npm release carrying the two engine changes since 0.7.28, #533
  (BC-79) and #532 (codex OAuth default), on main tip ce74cb7 (plus #531, the 0.7.28
  platform-lockfile re-stamp). **Behaviour changes:** **BC-79** — governed workers now receive
  `WICKED_RUN_PROJECT` in their environment (stamped only for a governed, proposal-submitting unit
  that carries a project), so estate proposals emitted during a run scope to the run's project
  instead of the ambient default; additive and serde-default, and inert until the garden-side reader
  lands (a repo-only run and an ungoverned unit are byte-identical to before). **Codex seat** — the
  default sign-in suggestion for the codex seat is now browser OAuth (`codex login`) rather than the
  device-code flow (`codex login --device-auth`), which stays available manually for headless
  contexts; suggestion-string only, no change to how a seat runs. Wire shape: additive only;
  `index.d.ts` unchanged (zero drift).
- **core-ts 0.7.28** — 2026-09-15 — npm release carrying the one engine change since 0.7.27, #529
  (F-W1-012), on main tip 8632066 (plus #528, the 0.7.27 platform-lockfile re-stamp). **Behaviour
  change:** the engine now strips the internal handoff scaffold (Work State / Next Move / Relevant
  Files) from rendered chat replies on every reply-boundary path — including the whole-output /
  compaction fallback — so the scaffold never reaches a rendered chat reply. The strip is bounded
  and fence-aware (a fenced code block is left intact); the durable transcript and unit outputs are
  untouched. Wire shape: additive only; `index.d.ts` unchanged (zero drift).
- **core-ts 0.7.27** — 2026-09-14 — npm release carrying the fifteen engine changes since 0.7.26
  (FIX-IT-ALL wave 3: L1 #513/#517/#518/#520, L2 #505/#510/#514, L3 #508/#511/#512/#523, L9 #522,
  L5 #525, L4 #524, plus the #526 build hotfix), all on main tip 37633d8 (plus #519, the 0.7.26
  platform-lockfile re-stamp). **Behaviour changes, in one place:** **#513** (L1-1A) — the acceptance
  fold reads the evaluator's own `VERDICT:` line; a non-PASS verdict denies into the human gate
  instead of auto-passing. **#517** (L1-1B) — the human gate gains `request_changes` and `amendScope`
  arms; each unit's rework attempts mint from that unit's own history. **#518** (L1-1C) — amendment
  scoping is proven end to end; `unitReworkAmended.scope` is rendered. **#520** (L1-1D) —
  `denial_gate: hold | auto_reject` so an escalation gate can be answered instead of parking forever.
  **#505** (L2-1) — the repo-checks floor uses a short private `TMPDIR`, splits cargo targets, bwrap
  masks only existing secret dirs, and a launcher failure is never scored as the check's. **#510**
  (L2-3) — `.wicked/checks.json` `e2e` runs at the verify stage only, after the test set. **#514**
  (L2-4) — the deliver re-verify runs the baseline-diff floor against the lifted-onto tip (else the
  run base); `run_forcing_install` deleted. **#508** (L3-3C) — a free-text problem plans ONE unit
  with the brief verbatim; `split_problem` deleted. **#511** (L3-K) — Cancel run kills a live
  Tool-executor child (the tool carrier takes the launch identity). **#512** (L3-H) — the repo-checks
  floor heartbeats on the unit's live-output stream. **#523** (L3-3A) — every seat benched at
  distribution parks at the `dead_seat` gate, not `sessionFailed`; council paths redacted. **#522**
  (L9) — deliver refusals park at an escalation gate; an explicit run base for PR revision; a
  `bug.fix` sweep line. **#525** (L5) — the chat reply is the answer after the last tool call, not
  every assistant block concatenated. **#524** (L4, F-W1-002/BC-75) — the opencode seat denies the
  `task` tool, and a tool call the seat itself refused now reaches the transcript. **#526** — the
  #524 × #525 `"tool_call"` match-arm collision collapsed into one arm (`-D warnings` build hotfix;
  no behaviour change). Wire shape: additive only; `index.d.ts` unchanged (zero drift on
  `finalize-dts.mjs`, regenerated by the wave-3 PRs). **Coupling to note:** wicked-crew 0.7.36 pins
  `wicked-core-ts ^0.7.27`.
- **core-ts 0.7.26** — 2026-09-14 — npm release carrying the eleven engine changes since 0.7.25
  (FIX-IT-ALL wave 1: L4 ①–⑦, L5 1.8, L10-5/-8/-9), all on main tip cad267e (plus #491, the 0.7.25
  platform-lockfile re-stamp). **Behaviour changes, in one place:** **#506** (⑦) — **the
  CLI-registered estate MCP hand-off is DELETED on both carriers, units and chats**: `mcpServers`
  is always `[]`, `permissions.allow` is `[]`, no `--mcp-config` file, no argv flag; workers ground
  through garden's estate SHIM — the graph pin is `WICKED_ESTATE_DB` and read-only is
  `WICKED_ESTATE_READONLY=1` on every worker child. **Requires wicked-garden ≥ 12.37.0** (the shim +
  the verdict text). **#498** (④) — the estate fence holds the `wicked-garden run|python` launcher
  spellings to the same read-only + pinned-store rule, and **every Evaluator-role unit's prompt
  carries the engine-owned `EVALUATOR_VERDICT_CONVENTION` line** (the evaluator convention is ON;
  the acceptance fold that parses it is core-ts 0.7.27). **#495** (①) — the Bash write and estate
  scans see through one wrapper level. **#496** (③) — `WICKED_RUN_*` markers on both worker
  Commands. **#499** (⑤) — every seat with a published generation is handed the launcher
  (`SkillsDelivery::LauncherOnly`). **#501** (②) — Bash writes judged under a fenced posture (a
  ReadOnly seat's write is a deny), the notes root admitted on both carriers, `mkdir` a write target,
  the guard unstages after its restore. **#502** (⑥) — wrapped units see their prior context
  (`PRIOR_CONTEXT_PREAMBLE`, clipped), ASSUMPTION markers anchored, `unitDispatched.baseSkill.handed`
  (the role-keyed base skill directive stays intake-gated; unset = none). **#507** (1.8) — **chat
  seats are handed the skills a unit gets** (`admit_chat`; a seat with nothing deliverable is
  refused at open with a remedy), the turn budget is named (600 s, a hypothesis), typed `TimedOut`,
  `chatReply.usage`, the chat boundary reads the seat's store pin. **#494** (L10-5) — state-home
  registry: `chats` registered, `interactive` a crew-placed root. **#497** (L10-8) —
  `.wicked/checks.json` for the repo-checks floor. **#504** (L10-9) — each of the five platform
  packages carries the stripped `wicked-core` hook binary stamped `wickedCoreVersion` (= the ROOT
  crate's 0.4.0); the release workflow asserts both lockfiles pin the same `wicked-estate*`. Wire
  shape: additive only. **Coupling to note:** wicked-crew 0.7.35 pins `wicked-core-ts ^0.7.26`
  (crew mirrors: L4-⑧, L5-crew-1, the L10-9 locator half).
- **core-ts 0.7.25** — 2026-09-14 — npm release carrying the six engine changes since 0.7.24 (the
  hardening train, Tier 1), all on main tip ef6c0f9 (plus #458, the 0.7.24 platform-lockfile
  re-stamp). **Behaviour changes, in one place:** **#477** (core#464, S1) — **every denial pauses
  at the escalation gate instead of failing the run.** Whatever the gate fold denies a unit for —
  the worktree guard, a boundary deny, a deterministic floor, the output gate, the agent judge,
  the evaluator≠creator pass — the run parks `awaiting_human` (`gateKind: 'escalation'`,
  `gateEscalated.condition` names the denial class) where it used to end `failed`: Approve
  re-dispatches the same unit, Approve+steer amends it, Reject cancels (a dirty worktree is kept).
  Bound read-only units get an engine-owned notes root outside every worktree. **Campaign nodes
  park `AwaitingHuman` on a denial too** — an unattended campaign has no auto-decider for that
  gate yet (core#484, open; a campaign denial policy follows). **#476** (core#467, core#469,
  F-RC2-009, S4a) — **the creator owes the repo-checks floor**: the `fix` phase provisions,
  typechecks, lints and tests in its own worktree and a red floor denies there, before the
  evaluator; a floor that exceeds its bound is classified `timed_out` (denial source
  `repo_checks_timeout`), never a failure; **baseline-diff** runs the same check on the run base,
  so a base-shared failure never denies — only a regression does. `repoChecksEvaluated` gains
  `outcome`, `floor`, `claim`, `env` and per-check `classification` / `preExisting` /
  `regressions` / `base` (additive). **#473** (core#461, S5) — a dead-class ballot corroborated
  by the dispatcher's own abstention **benches the seat for the run**; a worker exiting on a
  dead-seat refusal skips the triage judge and takes the failover ladder (bench → next eligible
  seat → the `dead_seat` escalation gate, never `sessionFailed`); the evaluator≠creator fallback
  is **disclosed** as `unitDistributed.distinctnessFallback: 'creator_seat' | null`; a roster
  with **no eligible seat is refused synchronously at intake** with the typed `NoEligibleSeat`
  error. **#472** (core#411, S6) — **state-home preflight + intake refusal**: `preflightStateHome`
  surveys the state home at boot and `launch_run` refuses an unregistered entry at intake with
  the remedy, instead of a first-worker fence failure; env-placed subtrees are registered.
  **#471** (#463 items 1+2, F-RC1-046/047, S8) — **estate shim allowlist**: read-only
  `wicked-estate` CLI subcommands and the `--readonly` + store-pinned estate shim /
  `wicked-estate-mcp` are ALLOWED in governed units, writes and unknown verbs stay denied
  fail-closed, every estate deny names the tool and the command with a remedy (advisory on fenced
  units, fatal on code-executing ones). **Requires wicked-garden ≥ 12.36.0**: the shim rule
  admits only a backend argv that spells `--readonly` with a pinned store (garden #1130, shipped
  by #1134 in 12.36.0) — an older garden's shim invocations are still denied. **#478** (core#468,
  S7) — **role-keyed BASE skill directive** on every agent unit (`WorkflowDef.base_skill_ref` /
  `WICKED_BASE_SKILL_REF`, intake-gated, existence-only seating, disclosed as
  `unitDispatched.baseSkill: {name, role} | null`) — **default OFF**: unset means no base
  directive; crew ships it warn-first. Wire shape: additive only. **Coupling to note:**
  wicked-crew 0.7.34 pins `wicked-core-ts ^0.7.25`; its mirrors are crew #558
  (`distinctnessFallback` + the 409 intake refusal), #567 (gate-on-denial tests), #557 (base
  skill default + disclosure) and #555 (state-home boot warning).
- **core-ts 0.7.24** — npm release carrying the one engine change since 0.7.23, on main tip
  11d3b66 (plus #455, the 0.7.23 platform-lockfile re-stamp): **#456** (acceptance findings
  F-E2E-030 / F-E2E-029 / F-E2E-028). **The deliver phase is always human-gated.** The engine
  pauses before the `deliver` Tool unit — the one step that leaves the machine — whatever the
  run's `human_confirm` policy says, and the prompt names the branch, the repo and the gh account
  that will push. The only opt-out is the launch setting `autoDeliver` (core-ts
  `LaunchOptions.autoDeliver?: boolean`, absent ⇒ gated; `LaunchSpec.auto_deliver` on the engine;
  `human_confirm: none` is NOT an opt-out) (F-E2E-030). **The repo-checks floor provisions the
  declared dependencies INTO the run worktree** with a frozen lockfile (`--ignore-scripts`,
  isolated cache) before running checks — a hollow or partial `node_modules/` (any declared
  dependency without `node_modules/<name>/package.json`) triggers the install and the check names
  the first missing dependency; a failed install is reported as an environment finding
  ("dependency provisioning failed — … not a verdict on the change") with the install's own
  output, never a bare ENOENT denial (F-E2E-029a). **Best-effort install fence:** a mutating
  `npm`/`pnpm`/`yarn`/`bun` invocation whose effective directory escapes the unit's worktree is
  refused — following `cd`/`pushd`/`popd`, `--prefix`/`-C`/`--dir`/`--cwd`, `env -C`,
  `npm_config_prefix=` and global installs — stateful across tool calls per `(run, unit,
  attempt)` on both carriers (the ACP permission bridge and the hook), advisory per tool call,
  disclosed as `workerToolCallDenied` with the `install fence:` reason prefix. It is never
  claimed hermetic: each distributed agent unit now also emits an additive `sandboxPosture
  {session, ord, cli, posture: 'os' | 'advisory', reason}` naming the seat's write containment,
  and F-E2E-029b stays OPEN until OS containment (`os_sandbox: true`) is armed for the seat.
  **Cancel keeps a worktree that holds uncommitted work** (a clean one is still reaped) and emits
  `worktreeRetained {session, path, reason}`; kept worktrees ride the
  `WICKED_COMPLETED_WORKTREE_KEEP_DAYS` window (F-E2E-028). **`awaitingHuman.gateKind`**
  (`run_level` | `def` | `deliver` | `terminal` | `escalation` | `failure` | `triage`; `gate_kind`
  on the durable interaction request) — consumers key on it, never on the prompt text. Wire shape,
  additive only: `autoDeliver` optional on `LaunchOptions`; `auto_deliver` on the run DTO
  (`#[serde(default)]`, always present on a new engine — a consumer promises a gate only when it
  is present); new `gateKind`, `sandboxPosture`, `worktreeRetained`. **Coupling to note:**
  wicked-crew 0.7.33 pins `wicked-core-ts ^0.7.24` and reports `/health.capabilities.deliverGate`
  true on it.
- **core-ts 0.7.23** — npm release carrying the two engine fixes since 0.7.22, all on main tip
  f37e325 (plus #451, the 0.7.22 platform-lockfile re-stamp): **#452** (acceptance finding
  F-7R3-001) — routing benches a seat that is DEAD for the run, not only one that is signed out.
  A seat whose council ballots all fail with a quota / rate-limit / billing-class refusal, a seat
  whose binary is not installed, or one that times out for the dispatcher's whole
  consecutive-failure streak (`WICKED_COUNCIL_SEAT_BENCH_THRESHOLD`, default 2) with no vote in
  between is benched for the run, and `unitDistributed.degradedReason` names the seat and the
  kind (`1 of 5 seats benched: copilot (quota_exhausted (3/3 ballots) — ballot)`). The quota
  verdict is a refusal-frame classifier over the seat's own words — a self-framed provider
  sentence or API code anywhere in the judged output, or a whole-word quota term beside refusal
  phrasing on one of the last six lines and only under a non-zero exit; never an identifier such
  as `rate_limiter`, never a bare `429`/`402` — and not-installed is judged from the spawn error
  kind, never from text. Deny-dominates both ways: one successful ballot keeps the seat, an
  unclassified failure proves nothing. The worker and judge transcripts classify with the same
  frame and bench by the same rule (`quota_exhausted` only while the seat has no successful unit
  in the run). **`BenchedSeat.reason` is free text and heterogeneous by design** — the bare
  `not_logged_in` token, `quota_exhausted (k/n ballots)`, `not_installed (k/n ballots)`,
  `timed_out (k/n ballots, no vote returned)`, or the launcher's own words — consumers render it,
  never parse it. **`UnitEvidence.judge_refusals`** `[{seat, reason}]` (additive,
  `#[serde(default)]`) carries the judge's refusals with their cause beside the unchanged
  `judge_auth_refusals`. **#453** (acceptance finding F-E2E-011) — a plan whose every unit is a
  Tool executor needs no CLI seat: crew hands its tool-only `onboarding` workflow `clis: []` by
  design, and 0.7.22 refused it ("no eligible seat … every configured seat is benched") about one
  second after launch, so no registered repo got a graph. The seat requirement is now per UNIT —
  a tool-only plan is routed `tool` before any eligibility verdict, and the empty-eligible refusal
  fires only when at least one planned unit actually needs a seat (a tool unit beside an agent
  unit on an empty or all-benched roster is refused exactly as before). Wire shape, additive
  only: new `councilSeatFailed.reason` tokens `quota_exhausted` / `not_installed`, new free text
  on `degradedReason`, one defaulted field on `UnitEvidence`; `unitDistributed` for tool units
  reads exactly as before. **Coupling to note:** wicked-crew 0.7.32 pins `wicked-core-ts
  ^0.7.23`.
- **core-ts 0.7.22** — npm release carrying the two engine changes since 0.7.21, all on main tip
  18f1dab (plus #446, the 0.7.21 platform-lockfile re-stamp): **#448** — `scripts/finalize-dts.mjs`
  parses again (the four unescaped backticks #444 left in its doc template broke `npm run build`
  from source; the published 0.7.21 `index.d.ts` was intact) and CI now `node --check`s the
  finalizer and reproduces the committed `index.d.ts` from it. **#449** (wave 6 — the governed
  testing journey, acceptance findings F-7R2-005/006/012/013/019): the worker remote-write fence
  (claude Bash deny rules, a segment-wise command filter on both carriers, and a credential/ssh
  strip on every seat spawn) — exactly what layer 3 guarantees: a `git` running with the seat's
  environment intact cannot push anywhere — https, `ssh://`, `git@host:`, `user@host:`, `host:`,
  `git://`, `file://`, a path — whether the remote is spelled on its command line, in the
  repository's config or through an operator `pushInsteadOf`, and cannot read a credential helper;
  health-aware routing that benches signed-out / auth-failed seats for the run; the default
  repo-checks floor + a judge distinct from the creator for prose-planned units, or an honest
  UNGATED verdict; ACP auth fallback kinds with no wrapped retry; the run branch recorded and a
  completed run's worktree retained. Wire shape, additive only: **`workerToolCallDenied`**
  {session, ord, attempt, cli, carrier, role, tool, command, reason, remedy} (new event; a refusal
  is advisory — one tool call, not the unit); **`gateEvaluated.ungated`** + **`ungatedReason`**,
  **`floorNote`** (why the deterministic layer is absent) and **`judgeSkippedReason`** (why no
  judge was convened); **`repoChecksEvaluated.sandboxLevel`**, **`sandboxError`**, **`detectError`**;
  **`unitDistributed.degradedReason`** set on EVERY routing arm whenever eligible < configured
  (the Council arm used to emit `null` unconditionally); **`councilConvened.clis`** names only the
  eligible (unbenched) seats; **`acpFallback.fallbackKind`** gains `auth_failed` and
  `unauthenticated`; **`runBaseResolved.runBranch`**; `AgentSession` gains `run_branch`,
  `base_commit`, `finished_at` and `benched_seats [{cli, reason, source}]`; and on the INPUT side
  `AgenticCli.health: {usable, reason} | null` (serde-default) carries the launcher's sign-in
  verdict. Consumers: render `gateEvaluated.ungated: true` as UNGATED with its reason, and a
  `repoChecksEvaluated` with `checks: []` as "0 checks detected", never "checks passed".
  **Coupling to note:** wicked-crew 0.7.31 pins `wicked-core-ts ^0.7.22` with
  `wicked-crew-api-types` 0.36.0.
- **core-ts 0.7.21** — npm release carrying the three engine changes since 0.7.20, all on main tip
  d5b12bb: **#442** (core#441, acceptance finding F-079) — every seat that receives a skills
  delivery (pi, copilot, opencode, claude; both carriers) is also handed `WICKED_GARDEN_ROOT=<pinned
  snapshot root>` and `<root>/scripts` at the front of `PATH`, derived from the same generation as
  the skills, so the `wicked-garden` launcher resolves the snapshot's synced `.venv`; over ACP the
  skills lever is judged from the SEAT binary, so pi behind `pi-acp` is judged as pi and handed the
  deliverable portable skill directories as `WICKED_PI_SKILL_DIRS` (the variable being SET is the
  delivery, an EMPTY value means `--no-skills` alone, UNSET means no delivery), with
  `skillsSnapshotHanded {path: "acp", cli: <seat key>}` emitted once per spawn. **#443** (core#441,
  F-079) — the codex skills lever: the engine-minted `CODEX_HOME/skills` is populated from the
  pinned snapshot (flat by frontmatter name, copies, serialized by an exclusive OS file lock, a
  per-generation `.wicked-skills-gen` marker so an unchanged generation is a no-op and a new one
  replaces exactly what the previous generation wrote), before the seat spawns and before
  `skillsSnapshotHanded {cli: codex}` is emitted, on both carriers. **#444** (acceptance finding
  F-4R2-004) — the creator write posture is derived per unit from its ROLE (`WritePosture::of`):
  `executes_code: false` + evaluator ⇒ read-only (unchanged), + creator + BOUND ⇒ deliverable-roots
  (writes allowed inside the launch-validated `extra_write_roots`, refused into the tree under
  review), + creator + UNBOUND ⇒ no fence; `evaluatorToolCallDenied` gains the additive `role`
  (`creator` | `evaluator` | `neutral`) and `posture` (`read-only` | `deliverable-roots`) fields,
  and the gate hook (`WICKED_DELIVERABLE_ROOTS`) and the ACP fence judge ONE identical
  deliverable-root set. Wire shape: additive only. **Coupling to note:** wicked-crew 0.7.30 pins
  `wicked-core-ts ^0.7.21`, `wicked-crew-api-types` 0.35.0 and `agent-acp-bridges` 1.1.1 (the
  crew-side `pi-acp` bridge that turns `WICKED_PI_SKILL_DIRS` into `--no-skills --skill …`).
- **core-ts 0.7.20** — npm release carrying the one engine change since 0.7.19, #433 (core#431 — the
  Phase 3 acceptance re-run findings F-3R2-013 / -010 / -007 / -009): deliver lifts onto the current
  base — a freshly minted worktree is based on the fetched remote default tip when the clone's
  `HEAD` is strictly behind it (`runBaseResolved`), and the `deliver` unit lifts in memory (`git
  merge-tree`) then re-verifies whenever the tree is not the verified tree (`deliverLiftEvaluated
  {outcome: unchanged | lifted | conflict | skipped | failed}`; a manifest/lockfile move forces the
  install step; both fetches are non-interactive); the engine restores the creator's tree on an
  evaluator mutation and keeps the discarded edits reachable as a suggestion ref (`worktreeRestored
  {discarded, suggestionRef}`); `gateEvaluated` names the judge (`judgeCli` / `judgeDistinct`);
  read-only ACP evaluators — an admitted ACP seat's write-class tool call is refused under
  `executes_code: false` (`evaluatorToolCallDenied`) and an unadmitted or unproven
  (`governance_verified == false`) seat is rerouted to the wrapped carrier (`acpFallback
  {fallbackKind: "read_only_requires_wrapped"}`); and the run-branch precondition — `HEAD` must be
  attached to `refs/heads/wicked/<run>` before anything is lifted, reset or cleared. The four new
  events are pinned in `index.d.ts` / `CoreEventJson`. **Coupling to note:** `wicked-crew-api-types`
  0.33.0 and wicked-crew 0.7.30 consume the new events; crew 0.7.29 stays on `wicked-core-ts
  ^0.7.19`.
- **core-ts 0.7.19** — npm release carrying the engine changes since 0.7.18: event `seq` stays
  monotonic per run across daemon restarts — the first record an engine writes for a run raises the
  counter past the run's persisted max, and that record carries `daemonRestarted: true` (#420,
  core#408); dead-letter spool records carry `ts` / `pid` / `origin` (`WICKED_APPS_EMIT_ORIGIN`) and
  the emit seam gains the `eventStoreCount` + `replayEmitOutbox` statics (#421, wicked-crew#495
  companion), hardened by #429 (core#428 — greedy userinfo redaction, a legacy record's
  `deadletter_reason` / `spooled_by` redacted before they become store metadata, the unstamped-line
  conflation note); repo graphs live under the daemon state home and an in-tree `.codegraph` is
  never adopted (#425, core#406), with the boot migration hardened — no-replace install, a relative
  `WICKED_ESTATE_REPO_GRAPH_ROOT` rejected, a 300 s overall budget (#430); per-seat configuration
  roots for EVERY CLI, a seat's startup banner never reaching the answer (#426, core#410), and the
  chat-scope validator refusing dot segments and non-directory read roots with admission on an armed
  floor (#435, #410 hardening); worker deny rules are the ones the CLI enforces — `Edit(path)`, no
  inert `Write(path)` twin (#434, wicked-crew#524) with every inert path-rule form (`MultiEdit` /
  `NotebookEdit` / `Glob`) lifted to its enforced twin and the `bypassPermissions` sentence
  re-measured (#436); and the napi-release workflow now re-stamps main's lockfile itself after a
  publish (#427 — #423 was the last manual re-stamp, for 0.7.18). **Not in this release:** #433
  (deliver lifts onto the current base and re-verifies after a lift; the creator tree is restored on
  evaluator mutation; the judge is named; read-only ACP evaluators) was still open when this train
  left — it ships in core-ts 0.7.20 immediately after. **Coupling to note:** wicked-crew 0.7.29 pins
  `wicked-core-ts ^0.7.19` (its `it.runIf(replayEmitOutboxSupported())` governance branch runs, not
  skips, on this binding); `wicked-crew-api-types` 0.32.0 is already published, and 0.33.0 follows
  with #433's wire.
- **core-ts 0.7.18** — npm release carrying the two engine changes since 0.7.17: council ballots
  run on the seat's worker home, not the daemon's `CLAUDE_CONFIG_DIR` (#413, F-030 / F-031 / F-013
  — ONE carrier-aware, fail-closed resolver in `wicked_apps_core::spawn` shared by the ACP worker
  spawn, the wrapped worker and the ballot spawn; `councilSeatFailed` gains ADDITIVE `stdout` +
  `reason` (`not_logged_in`); the roster's claude `login_invocation` — `registryRoster()` — is the
  RESOLVED absolute worker dir, and no command at all when it cannot be resolved); and evaluator
  phases cannot mutate the worktree while `verify` runs the repo's own checks as a deterministic
  floor (#414, F-036 / F-039 — the worktree guard denying ANY change under an `executes_code:
  false` phase with the new `evaluatorMutatedWorktree` event, the read-only no-code posture on
  non-claude seats (codex `--sandbox read-only`, pi `--exclude-tools edit,write`, a write-capable
  lever-less seat refused before launch), registration refusing a code phase whose gate evaluates
  nothing (`WorkflowDefError::GateEvaluatesNothing` / `UnverifiedEvidence`), and the sandboxed
  `repo_checks` floor with the new `repoChecksEvaluated` event; `UnitEvidence {worktree_guard,
  repo_checks}` rides `ApplyStepResult` and the bus `task.completed` payload, serde-default). Both
  #414 events are pinned in `index.d.ts` / `CoreEventJson`. **Coupling to note:** a wicked-crew
  whose mirrored defs lack the #414 gate pins is refused at registration by this engine — deploy
  alongside the crew release that carries them (wicked-crew#507). Also carries the
  `start_acp_process_with_write_roots` doc fix parked from #413 ("Nine parameters" → ten).
- **core-ts 0.7.17** — npm release carrying the three engine changes since 0.7.16: the skills
  snapshot engine (#399 — one skills input, `WICKED_SKILLS_SNAPSHOT`, handed to both carriers —
  ACP `plugins` handshake bound to the cached session / wrapped `--plugin-dir` — joined to the
  READ roots, the explicit state-home Read fence with its static registry, and the degradation
  ladder); seat routing honours skill portability (#402 — candidate seats narrowed before the
  council votes, `NoEligibleSeat` refused at plan time, `UnitDistributed` gains the additive typed
  `seatConstraint` mirrored as `UnitDistributedEventJson` in `index.d.ts`); and the evals lane's
  operator-authored `effect` + `EvalReport.rule_coverage` (#398, additive wire shape). Dependency
  note: the engine now pulls `serde_yaml` 0.9.34 (deprecated upstream; the `+deprecated` lockfile
  pin) for the skills-manifest parse — a known, accepted residual until the parser migrates.
- **core-ts 0.7.16** — run-provenance env: wicked-core stamps `WICKED_RUN_ID`, `WICKED_RUN_UNIT`, and `WICKED_RUN_AGENT` into the worker's estate-mcp launch env (both carriers) so proposal.submit (DES-MEM-FACETED-001) attributes proposals to the run/unit/agent.
- **core-ts 0.7.15** — npm release carrying DES-GROUNDING-001 + gov-008 Boundary 1: governed workers
  now ground in the wicked-estate index — the estate MCP is loaded via `--mcp-config` (not the inert
  `--settings` `mcpServers` key) and allow-listed (`permissions.allow`), run `--readonly` so the tool
  surface is read-only (#383); the OS-sandbox write-deny floor is generalized from validator scripts
  to wrap the CLI worker spawn (worktree = only writable root, default-OFF `os_sandbox`), disclosing
  a new `SandboxUnenforced` CoreEvent and continuing when it cannot arm (#384); and the worker Bash
  boundary fatally denies the `wicked-estate` CLI family (#385).
- **core-ts 0.7.14** — npm release carrying #377 (umbrella #360): opencode is the FIRST admitted
  non-claude ACP seat, input-governed via a harness-provisioned config (`OPENCODE_CONFIG_CONTENT`,
  no tracked-file mutation), re-proven against the provisioned config, version-pin guarded; the
  version-pin probe now mirrors the spawn's Windows `.cmd`-shim retry so npm-shim adapters are
  not spuriously downgraded on Windows.
- **core-ts 0.7.13** — npm release carrying #371 (issue #364, umbrella #360): ACP
  input-governance admission is an explicit, evidence-gated adapter capability
  (`acp_input_governance`, default false) instead of the `cli_runs_claude` name predicate.
  claude (the one proven adapter) stays admitted; an unadmitted ACP-configured seat is loudly
  disclosed on the audit wire (`GovernanceUnenforced`, scoped to seats that actually take the
  ACP path); an omitted capability on a user-TOML override inherits the builtin's value.
- **core-ts 0.7.12** — npm release carrying #361 (issue #358): `ReassignUnit` now kills an
  in-flight WRAPPED-fallback worker via an identity-keyed cancel registry (`run_id`, epoch,
  `launch_seq`) with a reassign tombstone covering the dispatch-to-registration race — the
  superseded zombie can no longer mutate the run worktree until the 2h ceiling. Reassign kills
  report `Cancelled` (never `TimedOut`); operator cancel and the turn ceiling are unchanged.
- **core-ts 0.7.11** — npm release carrying the perf-program engine changes since 0.7.10:
  the agy seat council-disabled by default (#354), the actor-scoped seat-health bench +
  abstention-aware quorum + one-wave dispatch (#355), `StepStatus::TimedOut` distinguishing
  the turn ceiling from an operator cancel (#357), and the idle-tick WAL checkpoint on the
  actor's own connections (#356 — needs `wicked-estate-{store,memory,knowledge}` ≥0.14.7, the
  backport line carrying `checkpoint_truncate`; `wicked-estate-core` stays on its 0.14.x pin).
- **Idle-tick WAL checkpoint on the actor's own connections (perf #5).** The actor loop's
  blocking `recv` became `recv_timeout(5s)`: a full tick of channel silence with
  `in_flight` empty and ≥60s since the last checkpoint (env knob
  `WICKED_CORE_WAL_CHECKPOINT_SECS`, `0` disables) TRUNCATE-checkpoints all three actor-owned
  WALs — graph store (`AnyStore::checkpoint_truncate`; the Postgres arm is a no-op),
  memory (`<estate>.mem`), and knowledge (`<estate>.knowledge`) — via
  `wicked-estate-store 0.14.7`'s busy-tolerant `checkpoint_truncate` (a concurrent
  `open_readonly` gate-hook holder defers it; it never blocks the writer). Fixes WALs
  outgrowing their DBs (core.db 3.35MB vs 4.19MB WAL). Requires the estate-store-family 0.14.7
  backport publish (pins bumped; 0.14.6 already exists on crates.io without the API).
- **core-ts 0.7.10** — npm release with the crew#427 engine fix since 0.7.9: the non-claude
  adversarial-review seat (codex) now runs BOUNDED on the governed-worker path
  (`--sandbox workspace-write`). Its declared sandbox posture is applied on that path — it was
  dropped before, so a codex evaluator ran under codex's default read-only sandbox and refused the
  temp/socket writes the verification suite needs — and the in-boundary `TMPDIR→<cwd>/tmp` scratch
  redirect now covers non-claude seats too. A code cap (`bound_ungated_posture`) keeps the RESOLVED
  posture bounded regardless of a stale/hand-edited `clis.toml`: any sandbox-disabling token
  (`--dangerously-bypass-approvals-and-sandbox`, `--sandbox danger-full-access`, `=`-attached, or a
  stray value) is rewritten to `--sandbox workspace-write`, so the boundary holds in code rather
  than depending on operators migrating a TOML. User-registry path resolution now goes through the
  shared `default_user_path` (HOME→USERPROFILE) on both the posture and invocation paths, so the
  override composes consistently on Windows.
- **core-ts 0.7.9** — npm release with two engine fixes since 0.7.8: a seat override that omits
  `trust_flags` now inherits the built-in's trust posture (crew#419 / #349 — a stale codex
  override no longer silently runs the seat in a read-only sandbox where governed work refuses),
  and ACP bridges spawn in their own process group so a terminal/group signal can't reach an idle
  bridge (crew#290 / #350, `#[cfg(unix)]`).
- **core-ts 0.7.8** — npm release cut with the due-diligence engine fixes since 0.7.7:
  campaign-safe worktree names + ownership-marked trees + a reaper that spares live campaign
  worktrees (crew#337 / #345, #347), and the single elicitation-capability predicate — a seat
  advertises exactly what its turns serve, stock claude and codex seats included
  (crew#341-adjacent / #346).
- **core-ts 0.7.7** — npm release cut with everything below since 0.7.6: the ACP dead-session
  liveness probe (crew#340 / #343), the creator evidence floor + `executes_code` plan-carry
  (crew#311 / #342), launch-declared `extraReadRoots` (#294 / #340), widened markdown steering
  ids (#335 / #338), replace-scope eval-corpus import (#336), and hermetic `cargo test`
  (#339 — the suite no longer writes the operator's real home).
- **Rejected units keep their transcripts + machine-readable deny** (usability review #1,
  core-ts 0.7.6). The `work_output` record is now written for EVERY gated unit: a denied/failed
  unit keeps whatever PARTIAL output existed at rejection, flagged `resolution: "rejected"`
  beside the structured denial; a unit denied BEFORE any output existed persists an explicit
  failure record (no output, denial only) — so the transcript read returns honest structure
  instead of nothing exactly when an operator is diagnosing a failed run. ADR-0003 unchanged:
  `get_work_output` (evaluator artifact-passing, context injection) filters rejected records and
  still returns approved output only. The actor's own rejection paths (worker failure, the
  triage judge's FAIL verdict, substance gate, deliverable floor, elicitation failure) persist
  the same flagged record with the unit's FULL partial output. NEW `UnitDenial` — the machine-readable twin of the `denial_reason` prose:
  `{source, reason, claim_id, rule_ids, denied_tool, phase}` — rides the persisted `WorkUnit`
  (additive `denial` field), `UnitOutcome`, `gateEvaluated` (camelCase `denial`, beside the
  retained `denialReason`), and `fold_input_denial`'s return (claim id + firing policy ids + the
  denied tool recovered from the decisions log's tool-call annotation). NEW read path
  `get_unit_transcript` / `Core::unit_transcript` / napi `unitTranscript(unitId)` →
  `{unit_id, resolution, partial, phase_status?, output?, denial_reason?, denial?}`. The existing
  `workOutput` binding keeps its `string | null` shape — a rejected unit now answers with its
  partial output (`null` only when none was ever stored), which released crew 0.7.3 serves and
  studio 0.4.3 renders unchanged.
- **STEERING unification — one steering-rule model** (STEERING program, gov-model lane). The
  wiki/rules model and the standalone governance `Policy` model MERGE into `ConformanceRule`:
  new optional/defaulted fields `steering_type` (enum-as-string over
  `architecture|development|security|testing|operations|compliance|design-ux`, default
  `architecture` — INV-S1), `applies_to` (inclusion, the exact `Policy.applies_to` SELECT
  semantics), `excludes` (the NEW exclusion twin — exclusion dominates), `weight` (finite ≥ 0,
  default 1.0 — recall orders severity → weight desc → id; stored gate-priority signal — INV-S2),
  and the merged enforcement half `effect`/`trigger`/`obligations`/`criteria` (a rule WITHOUT
  `effect` is recall-only exactly as before; `effect` + blank `applies_to` or a bad trigger regex
  is refused — INV-S3). Every field is skipped at its default, so pre-steering rows parse
  unchanged (the additive migration happens on read) and a rule that uses none of them keeps the
  2.x wire shape byte-for-byte. INV-C1 is now scoped to the reserved `PAT-`/`POL-` namespace;
  other ids (migrated policies keep theirs verbatim; UI/chat-authored rules mint their own) need
  only be non-blank. `register_policy` became a thin shim (dual-writes the effect-bearing
  steering rule + the legacy `Other(POLICY)` audit node; refuses an id collision with a
  recall-only rule); `retire_policy` retires both rows; `select_any`/`decide` read the UNIFIED
  store (legacy-only rows union in at read time so an un-migrated store never fails open), and a
  golden test proves a migrated policy's decisions are BYTE-equal to its old row's.
  `migrate_policies_to_steering` is the one-time idempotent migration (`rules ingest` runs it;
  kind→steering_type mapping documented in `steering.rs`: the seven types map to themselves,
  `guardrail`/`gate` and every other legacy kind → `operations`; ids unchanged, `retired`
  honored, legacy nodes retained for decision-audit resolvability). `RuleQuery` gains the
  `steering_type` facet (recall + estate `rules.recall` wire-compatible); NEW `list_rules` +
  `wicked-core rules list [--type <t>] [--include-retired]` is the management/audit listing
  (decide-lane rows always shown; retired rows listable — closes the recall-skips-retired
  listing gap); `rules recall` gains `--type`; the scoreboard gains a per-steering-type
  population breakdown (`by_type`); MarkdownAdapter frontmatter gains optional
  `steering_type`/`excludes`/`weight` keys and `applies_to` now rides onto minted rules;
  UI/chat provenance sources are first-class. `conformance-rules.schema.json` bumped additively
  to contract 1.1.0 (bundle 1.2.0): new optional properties, id pattern relaxed outside the
  reserved namespace, `metadata.schema_version` widened to `enum [1.0.0, 1.1.0]`.
- **Fan-out contract across the deliberate store split** (AW-5 / arch-R3, decision record
  `.product/DES-OUTGOV-008-fanout-placement.md`). `wicked-core rules fanout <dir>` fans ONE ruleset
  (the `rules ingest` layout) out to the three lanes a governed run reads — (a) the enforcement
  store the gate hook selects/recalls from, (b) every discovery graph the workers' estate MCP binds
  (native `NodeKind::Rule` copies; deny-path policies do NOT replicate here), (c) one knowledge
  rationale chunk per rule (id-keyed `rule-rationale/<ID>` upsert, `source` = the rule's
  `provenance.ref`, the PAT-/POL- id embedded in the chunk text) — and smoke-verifies every cli
  lane against a FRESH handle on the same `--db` a worker is handed, through the consumers' own
  read paths (`recall_rules`, policy round-trip, knowledge recall). The receipt is a manifest
  (v1.0) keyed on the stable PAT-/POL- ids mapping each rule to its three copies; any missing copy
  fails the WHOLE fan-out loud. A daemon-held store is NEVER CLI-written (single-writer invariant):
  `--enforcement-crew-api <url>` records the pending transport and emits the
  `POST /api/v1/governance/{policies,rules}` payload instead, and any lane path under
  `~/.wicked-crew` is refused before a single lane is written. Crate surface:
  `wicked_governance::{fanout, load_ruleset, FanoutManifest, FanoutScope, FanoutTargets, …}`.
- **`scope: workspace` in the fan-out manifest** (AW-6 / arch-R20 decision). Cross-repo doctrine
  placement decided: replicate-to-every-repo — a workspace-scoped fan-out carries one discovery
  copy per live repo graph (caller-enumerated; zero discovery targets refuse loudly), with id-keyed
  idempotent re-ingest keeping the N copies syncable. Zero engine change. Option (b), a
  workspace-root store with multi-`--db` resolution, is documented and parked as P-2 in
  DES-OUTGOV-008 — unparking requires an estate-owner ruling on resolution + gate precedence.
- **MarkdownAdapter on the `SourceAdapter` ingest seam** (AW-3 / arch-R1). One parse convention —
  YAML frontmatter (`id`, `title`, plus optional `status`/`enforcement_class`/`applies_to`/`scope`/
  `supersedes`/`domain`/`confidence`/`targets`) and a `## Rules` section of
  `- <PAT|POL-nnn> (<severity>): <statement>` items. All output materializes through the existing
  `normalize_bundle` fail-closed invariants (no second parse path); a malformed doc fails LOUD
  per-file with path + reason, never a silent skip; a doc without a Rules section is a valid
  doc-only ingest. `wicked-core rules ingest --dir <path>` now ingests frontmattered `*.md` docs
  anywhere under the directory alongside the existing `policies/*.json` + `rules/*.json` lanes,
  with cross-lane duplicate-id refusal.
- **Schema-document nodes** — `wicked_governance::register_schema_nodes` registers the 4 governance
  schemas on the graph (one node per schema file, keyed by `$id`, carrying contract + bundle
  version); `rules ingest` refreshes them on every successful run (the schemas/README.md AW-3 seam).
- **wicked-governance owns the 4 governance schemas** (AW-2 / arch-R10, #309). Re-homed byte-for-byte
  from the retired wicked-brain repo at bundle VERSION 1.1.0 (`crates/wicked-governance/schemas/`),
  embedded via `include_str!` with lift-fidelity + INV-C4 vocabulary guards; garden vendors from
  this copy. Also adds the thin root `CLAUDE.md` pointer stub (AW-1).

- **A chat reply is the ANSWER — the text after the turn's last tool call — not every assistant block concatenated (F-W1-004, wave-1 P6 gate; R-L5-2).** `exec_turn_acp_posture` appended every `agent_message_chunk` to `output`, so claude's Q1 `chatReply.text` opened with "Let me explore the key repos in parallel to trace this flow.Now let me read the core files in parallel.Now let me look at…" — the tool monologue the scope statement forbids, glued without separators before the answer (F-RC1-115 / F-069 narration class; the criterion-5 regex passes). Crew cannot separate the two: a `tool_call` start fell into `handle_update`'s `_ => {}` arm and left no mark in the frames it receives (only text deltas; no tool frames for chats). Now `handle_update` records `answer_from = output.len()` at every `tool_call` start (`TurnResult.answer_from`, additive), and `chat_turn` surfaces `TurnResult::chat_answer()` — the banner-stripped, trimmed text after the LAST tool call, or the whole output when nothing was said after it (loss-averse) or no tool was called (today's behaviour). The narration was already streamed as `chatDelta`s, where the studio narrates it; it never re-enters the reply or the at-rest transcript. UNIT outputs are untouched (`output` stays whole: prior-output injection and the evaluator verdict line read the full text). Tests: `a_tool_call_starts_the_answer_and_the_chat_reply_is_the_text_after_the_last_one`; the chat-seat scope test's stub now speaks narration → `tool_call` → answer and asserts the reply is the answer while the deltas carry both.

### Fixed
- **Repo-graph migration hardening (#406 follow-up).** The completed copy is installed with a
  NO-REPLACE `hard_link` + `remove_file` (a `rename` would silently replace on Unix): a graph that
  appears at `<key>/estate.db` while the copy runs — an indexer racing the boot — wins and the temp
  is discarded (`Install::LiveGraphWon`). A RELATIVE `WICKED_ESTATE_REPO_GRAPH_ROOT` is rejected
  (warned once) and the precedence falls through to the state home, so the write resolver can no
  longer mint `./<key>/estate.db` in the cwd while the sandbox classifier grants nothing. The boot
  has an OVERALL migration budget (300 s across all repos, on top of the 60 s per copy): once spent,
  the remaining legacy graphs are `Deferred` — reported, untouched, copied at the next boot — so an
  upgrade with many stale or locked graphs cannot keep the daemon unavailable for N × 60 s. The
  STEERING guide's `rules fanout` example names `<state-home>/repo-graphs/<repo-key>/estate.db`.
- **Worker deny rules are the ones the CLI enforces — no more inert `Write(<path>)` twins
  (wicked-crew#524, acceptance finding F-3R2-004).** `execute_wrapped::deny_rules` /
  `shared_deny_rules` emitted a `Write(<dir>/**)` rule beside every `Edit(<dir>/**)` for the
  operator's `~/.claude`, `~/.ssh`, `~/.gnupg`, `~/.aws`, `~/.config/gcloud`, `~/.wicked*` and the
  daemon's config dir — on BOTH carriers (`--disallowedTools` argv, the wrapped per-unit settings
  file, the ACP worker home's shared `settings.json` and per-session options). Claude Code
  (measured on 2.1.268) does not match `Write(path)` rules at all: every claude ballot's stderr
  carried 12 `Permission deny rule … Write(…) is not matched by file permission checks — only
  Edit(path) rules are` warnings, the rules cost ballot time, and an operator reading the settings
  file saw a stronger fence than existed. The engine now emits `Read` + `Edit` only (`Edit` covers
  every file-editing tool per the CLI), and a template's OWN `Write(<path>)` deny — templates may
  add denies, never remove one — is lifted as the `Edit(<path>)` form that enforces it
  (`enforceable_rule`); a bare `Write` (whole tool) is left as stated. Tests pin both generators,
  the argv carrier and the settings-file carrier free of `Write(<path>)`; the live-CLI check is
  documented on `enforceable_rule`.
- **Deliver lifts onto the current base and re-verifies after a lift; the creator's tree is
  restored on an evaluator mutation; the judge is named; ACP evaluators are read-only (#431;
  F-3R2-013 / F-3R2-010 / F-3R2-007 / F-3R2-009).** Four gaps the Phase 3 acceptance re-run
  found on one governed `bug` run. (1) The run branched from the registered clone's `HEAD`, five
  commits behind `origin/main`; the deliver script's rebase then conflicted on a generated file
  (`LIFT-CONFLICT`), the operator resolved it by hand, and the tree that was pushed was not the
  tree the repo-checks floor had verified. Now `create_worktree` fetches `origin` and bases a new
  run worktree on the remote default branch's tip when the clone is strictly behind it
  (`runBaseResolved {baseRef, baseCommit, localHead, behind, fetched, lifted, note}`; a clone that
  is ahead of/diverged from the remote keeps its `HEAD`, disclosed), and before the `deliver` tool
  phase runs the engine LIFTS uncommitted work on a stale base onto the remote tip in memory
  (`git merge-tree --write-tree`, git ≥ 2.38) — `deliverLiftEvaluated {outcome: unchanged |
  lifted | conflict | skipped | failed, baseRef, baseBefore, baseAfter, treeBefore, treeAfter,
  conflicts, note}`. A conflict fails the deliver unit with a `LIFT-CONFLICT` remedy naming the
  files and leaves the worktree exactly as verified; a lift RE-RUNS the repository's own checks on
  the lifted tree (`repoChecksEvaluated` for the deliver unit) and the push runs only when they
  pass; an apply-phase failure (`failed`) fails the unit closed rather than letting the script run
  on a partial tree — the deliver gate never pushes a tree that was not verified. A branch
  carrying its own commits is skipped (the deliver rebase replays that history, as before). THE
  RULE that makes the sentence true on every path (independent review F-433-001): the session
  records the VERIFIED TREE (`AgentSession::verified_tree` — the verify unit's guard after-tree
  once its checks passed, or a deliver re-verify's post-check tree, via `UnitEvidence::
  verified_tree`), and at deliver the worktree is snapshotted and the repository's checks run
  whenever its tree ≠ that record — `unchanged` and `skipped` included — so a retry after a
  failed re-verify, an operator's by-hand rebase, or a run with no verify phase is re-checked,
  never waved through; a lift that moved a lockfile forces a frozen `--ignore-scripts` install
  ahead of the checks and names the drift (F-433-003); the checks' own writes are caught by a
  post-check snapshot (F-433-002). Both `git fetch origin` calls are non-interactive
  (`core.askPass=`, `GIT_TERMINAL_PROMPT=0`, `ssh -oBatchMode=yes`) and killed at 120 s
  (F-433-004); a failed fetch skips the deliver lift rather than trusting cached refs. (2) On
  `evaluatorMutatedWorktree` the only choices were Approve — which re-baselined on the CURRENT
  tree, silently adopting the evaluator's edit — or cancel, and the engine's remedy was a shell
  command. The worker thread now restores the creator's tree itself (`HEAD` reset when moved,
  `read-tree --reset -u <beforeTree>`, added paths deleted, re-snapshot proven equal):
  `evaluatorMutatedWorktree` carries `restored` + `restoreError`, a `worktreeRestored {tree,
  head, discarded, suggestionRef}` event follows — the discarded edit is PINNED first under
  `refs/wicked/suggestions/<run>/<ord>/<attempt>` so it is never gc-pruned and the #432
  suggestion lane can read it (F-433-008) — the gate's `denialReason` says the edit was
  discarded, and the
  `awaitingHuman` prompt says Approve retries against the restored tree. (3) `gateEvaluated`
  names the layer-2 judge: `judgeCli` (the seat key) and `judgeDistinct` (identity-distinct
  rotation pick vs. the single-runner fallback; both `null` when no judge ran or on the bus path)
  — evaluator ≠ creator is auditable from `/runs/:id/events`. `AgentVerdict` gained
  `judge_cli`/`judge_distinct`. (4) On the ACP carrier the read-only posture applied only to the
  wrapped argv path (`--exclude-tools edit,write`); a pi evaluator not admitted to input
  governance was answered `allow_result` and rewrote the fix under review. Every
  `executes_code: false` unit's ACP turn now refuses write-class `session/request_permission`
  calls (edit/delete/move by ACP `kind`, or a write tool by name or verb-first prefix — pi's
  lower-case `edit`/`write` and the `str_replace_*` family included) with the agent's reject
  option, disclosed as `evaluatorToolCallDenied {cli, carrier: "acp", tool, kind, path,
  reason}`; and because an UNADMITTED adapter never asks (pi-acp executes with zero permission
  round-trips, codex-acp auto-resolves edits), a guarded unit on such a seat is routed to the
  wrapped carrier — where the read-only lever is an argv fact — with `acpFallback
  {fallbackKind: "read_only_requires_wrapped"}`; a lever-less seat there (agy, copilot) is
  GUARD-ONLY: the read-only instruction rides its prompt and the daemon line says so
  (F-433-009). `bash` stays (posture, not guarantee — the
  worktree guard remains the backstop, and its restore now runs for every exit status, re-attaches
  a switched/detached `HEAD` via the recorded `WorktreeSnapshot.head_ref`, and reports an unborn
  baseline honestly). The deliver command receives `WICKED_DELIVER_VERIFIED_BASE` (the verified
  remote-tip commit) so crew's script can refuse a base that moved after the re-verify. core-ts
  `index.d.ts` documents the new frames; the key sets are pinned by the binding's own tests.
- **Repo graphs live under the daemon state home; an in-tree `.codegraph/` is never adopted
  (#406; F-016 / F-024).** `registerRepo`/onboarding minted every repo's code graph under the
  OPERATOR's `~/.wicked-estate/repo-graphs/<key>` whatever `--db` said — two daemons on one host
  shared and clobbered graphs, `--db` did not relocate the data a customer backs up or isolates,
  and crew's diagnostics could not list the store — and, when the checkout happened to carry (or
  git-TRACK) `.codegraph/estate.db`, the "legacy-first" resolver adopted that file as the live
  graph and read-only onboarding wrote INTO the customer's tree (`git status` dirty; a
  `git checkout .` silently reverting the graph). Now ONE deterministic rule
  (`code_graph.rs` ADR): `$WICKED_ESTATE_REPO_GRAPH_ROOT` when set, else
  `<state home>/repo-graphs/<key>/estate.db` where the state home is the canonical parent of the
  engine's own `--db` (the actor binds it per thread from the store it was spawned on —
  `code_graph::StateHomeScope`, the `GOV_DB_PATH` idiom; the launchers pass their
  `operational_home`; the two off-actor readers `coverage_report_for_repo` / `graph_kinds_for_repo`
  take the daemon's store path), else the DEFAULT state home `~/.wicked-crew/repo-graphs` for a
  library caller that never spawned a `Core`. The resolver NEVER reads or writes a graph inside
  `root_path`: a checkout carrying `.codegraph/` gets its graph under the state home like every
  other repo and a new `findings` entry on its `RepoEntry` (`RepoFinding { code:
  "in_tree_code_graph_ignored", message, path }` — additive on the wire, re-derived on every read
  like `code_graph_db`, logged once at registration) telling the operator to delete / `git rm
  --cached` it. The sandbox grants follow: `classify_code_graph_db_at` recognises ONLY
  `<root>/<key>/estate.db` (the `CodeGraphHome::InTree` arm is gone — an in-tree-shaped
  `code_graph_db` widens no read or write boundary), and the grants are derived from the state home
  the launcher carries, so a custom-`--db` daemon grants exactly `<its state home>/repo-graphs/
  <key>/`. `repo-graphs` is registered in the state-home subtree fixture
  (`tests/fixtures/state-home-subtrees.json`, owner `engine`; crew mirrors it byte for byte), so the
  worker Read fence denies the subtree and a daemon that has indexed a repo is not refused at
  launch. **Migration, once, at boot:** a registered repo whose graph sits under the old
  `~/.wicked-estate/repo-graphs/<key>` and nowhere under the new root is copied through SQLite's
  online-backup API (page-consistent even for a WAL-mode db another connection holds open; the
  `rusqlite` `backup` feature) and logged one line per repo; the source is LEFT IN PLACE for the
  operator to remove once the new daemon is verified and an existing destination is never
  overwritten. The copy is crash-safe and bounded: it lands in `<key>/estate.db.migrating-<pid>`
  and is renamed onto `estate.db` only on `Done` (a boot killed mid-copy leaves nothing at the
  served path; the next boot sweeps the stray temp and copies again), a locked source, a
  restarting backup or 60 s of wall clock fail it closed, and a failed copy removes its temp so
  the repo simply re-indexes. **Repos indexed in-tree are not migrated** (an in-tree graph is never
  read): they come through the upgrade with no live graph — the record's finding says "no graph has
  been indexed under the state home yet — re-run onboarding (`POST /repos/:id/onboard`)" instead of
  asserting a live path, and the boot logs one such line per affected repo next to the migration
  notices. New public helper
  `repo_graph_root_for_store(db_path)` spells a daemon's root for out-of-process consumers. Repo
  side: wicked-interactive#213 / wicked-studio#220 untrack their `.codegraph/estate.db`.
- **Event `seq` stays monotonic across daemon restarts (core#408, F-035).** The durable per-run
  event log stamped `seq` from a process-wide counter that started at 0 with every daemon, so a run
  resumed after a restart recorded its new events with `seq` 0, 1, … while its history already held
  0–294 — and `GET /runs/:id/events`, which sorted by `seq`, returned the new events BEFORE the old
  ones. Every consumer taking the tail as "latest" (the studio now-bar, crew's relays, the
  acceptance poller) read the pre-restart `awaitingHuman` as current while the run had moved on.
  The first record a process writes for a run now continues from the largest `seq` already in that
  run's log (raised into the counter with `fetch_max` so other in-flight runs stay monotonic too),
  with the first-touch check, the seed, the stamp and the enqueue under one lock so concurrent
  callers cannot slip below the history; `seq` is therefore strictly increasing within a run for the
  run's whole life. That first post-restart record also carries `daemonRestarted: true`
  (envelope-only, like `ts`/`seq`; absent everywhere else) so a consumer can see the boundary
  instead of inferring it. The reader no longer sorts: for the append-only single-writer log, file
  order IS emission order, so a log a pre-fix engine wrote across a restart — a repeated or a gapped
  second `seq` run — reads back exactly as emitted instead of interleaved. Proven across a REAL
  process boundary: the e2e re-executes the test binary as the "restarted daemon", which approves
  the gate and finishes the run. core-ts: `runEvents` states the contract and the hand-authored
  `index.d.ts` gains `RecordedEventJson` (`ts`, `seq`, `daemonRestarted?: true`) with compile-time
  assertions in `types-test/`.
- **Council ballots run on the seat's worker home, not the daemon's `CLAUDE_CONFIG_DIR` (F-030;
  F-031, F-013).** `wicked-council`'s ballot spawn inherited whatever `CLAUDE_CONFIG_DIR` the daemon
  was started with (`hardened()` strips only `WICKED_*`): on a fresh install that is the
  never-signed-in dir garden is registered in, so every claude ballot exited 1 `Not logged in` and
  the seat was benched on every council (4-of-5 verdicts) while the ACP worker path — which sets the
  variable from the worker home — ran the same CLI fine; on a laptop with no such variable the
  ballots ran on the OPERATOR's `~/.claude` login by accident. ONE resolver now
  (`wicked_apps_core::spawn::{worker_home_base, worker_claude_config_dir, seat_claude_config_dir,
  claude_config_for_carrier}`: `$WICKED_WORKER_HOME` else `~/.wicked-worker`, joined `/claude` —
  ALWAYS absolute: an empty or relative override/home is refused as a config error, since three
  consumers would each resolve it against a different cwd; VALIDATED no-follow via the shared
  `refuse_symlinked_home`, so a planted `<worker home>/claude -> ~/.claude` link is refused for
  ballots exactly as for ACP workers; and CARRIER-AWARE via the shared `binary_is_claude` file-stem
  test — only a claude carrier gets a claude config dir, a codex/pi/copilot/opencode seat or bridge
  gets the variable STRIPPED, never an ambient claude config path). Used by the ACP worker spawn
  (`acp_runner::worker_config_home` delegates to it; `start_acp_process*` take `seat_is_claude`),
  the ballot spawn (sets `CLAUDE_CONFIG_DIR` on claude seats after `hardened()`, honours the same
  `WICKED_WORKER_INHERIT_OPERATOR_CONFIG` hatch — the const moved below the root too — and fails
  CLOSED when the dir cannot be resolved or validated) and the roster's claude `login_invocation`,
  which is now DERIVED from the resolved dir (`CLAUDE_CONFIG_DIR="<resolved>/claude" claude`; plain
  `claude` under the hatch; NO command at all when the dir is unresolvable — fail closed, no
  `$HOME/...` fallback) instead of the hard-coded `$HOME/.wicked-worker/claude` that sent an operator
  under `WICKED_WORKER_HOME` to sign in the wrong directory (`default_login_invocation` returns
  `Option<String>`). Seat-failure diagnostics (F-031): `SeatFailure` keeps the stdout TAIL (claude
  prints its refusal on stdout with stderr empty), stderr as HEAD+TAIL around an elision marker, and
  a classified `reason` (`SeatFailureReason::NotLoggedIn` ⇢ `not_logged_in`) judged over the
  UNTRUNCATED streams (`with_output`), so a signature past the 4 KiB cap still classifies;
  `SeatFailure::reason()` is renamed `summary()` and includes the class. Review round 2 hardening:
  the resolver also refuses `.`/`..` segments and walks EVERY component from the filesystem root
  to the leaf refusing planted (non-root-owned) symlinks — applied inside `worker_claude_config_dir`
  so every consumer gets the same validated path; the WRAPPED worker applies the same carrier
  decision (`execute_wrapped::exec` sets the worker home for a claude carrier, strips the variable
  for any other — wrapped claude no longer runs on the operator's login by accident); and an ACP
  launch reads its `[cli.acp]` transport and its seat identity off ONE registry record
  (`acp_launch_facts`; the unit path reuses its single `seat` read). The shared `binary_is_claude`
  carrier test follows the OS's executable lookup: case-insensitive on Windows (`CLAUDE.EXE`,
  `Claude.cmd` are claude there), exact elsewhere.
  `councilSeatFailed` gains ADDITIVE `stdout` and `reason` (`null` when unclassified) beside
  `stderr`/`detail` — wire shape change (additive) — crew/studio consume it via the next core-ts
  release; the crew roster consumer of `login_invocation` sees the resolved path. Persisted
  `seat_failures` written before this read with the new fields defaulted.
- **ACP input-governance admission is evidence-gated (#364).** ACP permission requests now enter
  the shared policy, boundary, marker, and conformance-claim evaluator only for an ACP adapter
  explicitly marked `acp_input_governance = true`; the capability defaults off and only the
  pinned Claude adapter is currently admitted. Governed turns on a CLI that HAS an ACP config but
  isn't admitted emit the `governanceUnenforced` audit event rather than silently appearing
  governed; a CLI with no ACP config at all stays silent here (it never touches the ACP path, so
  claiming otherwise would be a false disclosure — an adversarial review caught this against a
  live `clis.toml`, since several built-ins are commonly wholesale-overridden with no `[cli.acp]`
  block at all). A user ACP override that keeps an admitted built-in's binary and omits the
  capability inherits it (with a warning); explicit `false` and adapter swaps remain unadmitted.
- **Markdown steering import accepts custom-family rule ids** (#335). The `## Rules`
  item grammar widens from `PAT|POL-nnn` only to any `<UPPERCASE-FAMILY>-<suffix>` id
  (`OPS-CUSTOM-10`), every one of them valid in the rules CRUD too — the doc lane is now a
  disciplined subset of the CRUD id namespace instead of disjoint from it. `PAT-`/`POL-` stays the RESERVED namespace (strict
  `^(PAT|POL)-[0-9]{3,6}$` shape, prefix ⇔ `rule_type`, enforced per entry at the doc line);
  custom families infer `rule_type` from the doc's `enforcement_class` (`policy` ⇒ policy, else
  pattern) and carry full doc provenance (`path@sha#id`, `source_kinds: ["doc"]`) — the doc is
  the source. Malformed ids still fail loud per entry; the import-panel hint (the error text
  studio echoes verbatim) now teaches the widened grammar. INV-C1 re-documented as a SHAPE
  contract on the reserved namespace, not a lane split (STEERING.md format section updated).
- `required_deliverables` enforced at the result fold, not in one runner (#297 → #308).
- Failure-excerpt triage keeps the TAIL of the output, where the error usually is (crew#322 → #307).
- Seat failover keyed to phase idempotency, not input governance (#292 → #304).
- Resume re-provisions a reaped worktree before re-dispatching into it (#290 → #303).

- **Deliver refusals PARK instead of failing the run; an explicit run base for revising a pull
  request; the `bug` fix phase sweeps retired behaviour (DES-L9 r2 §5 PR-L9-core; crew #549 /
  #550 = F-RC1-010 / F-RC1-043 / F-RC1-061, core #432; BC-57..BC-60).** (a) `apply_step_result`
  gains ONE deterministic arm ahead of the environment-refusal and triage arms: a FAILED `deliver`
  Tool unit (`is_deliver_unit` — tool_cmd + phase id `deliver`) whose output does not carry the
  `LIFT-CONFLICT` strand marker is `Rejected` (`denial.source: deliver_refusal`, `denial_reason:
  "deliver refused on unit N: <head+tail excerpt>"`, the full transcript persisted), `stepFailed
  {workerError, detail: <the script's own words>}` fires, and the run PARKS at
  `awaitingHuman{gateKind: "escalation", ord: N, reviewingOrd: N}` — regardless of `human_confirm`
  (the API default `None` used to fall through to `sessionFailed`, a clean tree reaped to the
  branch, committed work with no recovery — the crew#432 class) and of `auto_deliver` (D-5's
  opt-out gates the PUSH, not the refusal); no LLM judge reads a deterministic refusal (0
  `failureTriaged`). Approve re-dispatches the deliver unit through `confirm_gate` (attempt bumped;
  the lift + re-verify run again first — no second deliver gate); Reject cancels and keeps the
  worktree. A `LIFT-CONFLICT` output keeps today's terminal path end to end (crew derives
  `completed` + `delivery: stranded` and offers the post-hoc lift). `deliver_lift.rs` hoists the
  marker into `LIFT_CONFLICT_MARKER` (the engine's own Conflict refusal formats through it).
  (b) `LaunchSpec.base_ref: Option<String>` (additive; core-ts `LaunchOptions.baseRef`) names a
  branch on `origin` — an open PR's head — that `repo::resolve_run_base` resolves AFTER its fetch as
  `origin/<base_ref>` and mints the fresh worktree from (`runBaseResolved{baseRef: "origin/<x>",
  baseCommit: <tip>, lifted: false, behind: 0, note: "explicit base — the launch named origin/<x>
  (revises a pull request)"}`); a ref that does not resolve, a non-name, or a clone with no `origin`
  FAILS the launch by name (`WorktreeFailed` → `sessionFailed` + `error` naming the ref and the
  `--single-branch` remedy) — never a silent fall-back to the default branch, which would push a
  duplicate PR. The run still lives on `wicked/<run>`; `None` = today's resolution. (c)
  `PhaseDef::instructions(..)` builder; `bug_def()`'s `fix` phase carries
  `BUG_FIX_SWEEP_INSTRUCTIONS` ("Update every consumer of behaviour this fix retires or changes:
  tests, docs, comments." — ≤ 90 ASCII bytes so the PTY carrier's 1000 B prompt keeps its intent
  headroom; the DES's longer wording did not), one line, folded onto the creator's prompt after
  ` ||| `; `workflows/bug.json` mirrors it; crew's `BUILTIN_WORKFLOWS.bug`
  carries the same literal. Tests: the arm (parks under `None` and under `auto_deliver`; a strand
  and a non-deliver Tool unit keep today's path), `repo.rs` explicit base (resolves / refused by
  name / not a name / no origin / `None` = today), the sweep literal + fold, and the real-engine
  launch test `tests/deliver_refusal_gate.rs` (deliver gate → approve → refusal → `escalation` gate
  → fix → approve → `sessionCompleted` with the unit re-dispatched at attempt 1; `base_ref` on
  `runBaseResolved` and the unresolvable-ref failure).

## [core-ts 0.7.1] — 2026-08-25

### Added
- **Project-scoped graph vouching** — a run in a project sees the project's graph when the engine
  can vouch for it (#299, review follow-ups #300).
- ACP bridge-death instrumentation for the crew#290 session-death hunt (#289).
- Engine-injected phase-scope preamble + pre-build code-change warnings on the plan path (#287).

### Fixed
- ACP inbound frames dispatched by **method**, not id alone — a permission request no longer ends
  the turn (#295).
- Seat failover walks the full roster; evaluator prompts bounded (#286).
- napi cross-compile: aarch64-linux built with the GNU toolchain (zig 0.13 rejected the erratum
  flag) (#301, #302).

## [core-ts 0.7.0] — 2026-08-19

### Added
- **Live unit output streaming + phase substance gate** (#279).
- Per-seat `login_invocation` for PTY-hosted sign-in (#278); persistent worker config home
  (crew#267, #277); seat failover, failed-run resume, worker reaping, death instrumentation (#275).

### Fixed
- **The evidence gate sees committed work** (core#280 → #281). `EVIDENCE_SCRIPT` now also counts
  commits the run branch carries beyond every non-`wicked/*` local branch; the layer-2 agent judge
  receives harness-derived worktree evidence (porcelain + run-branch `git log --stat`, capped);
  the phase-substance gate widened identically. The built-in floor pin moved to `e2e7af1db9e48454`
  (const, shipped defs, and any operator overlay must agree).
- ACP bridge auth refusal named instead of a silent death (crew#267 root cause, #276); agent-memory
  carve-out follows the resolved Claude config home (#273).

## [core-ts 0.6.3] — 2026-08-14

### Fixed
- Strip the pi RPC banner at capture and at the exit-0 arm (restores the FINDING-101 audit
  windows); surface stderr on seat death (#269, #271).

## [core-ts 0.6.2] — 2026-08-14

### Added
- Run archival — write off terminal runs without deleting evidence (crew#265 core half, #266).
- Filesystem boundary armed on the ACP path (#263).

### Fixed
- System-temp scratch writes advisory + worker TMPDIR kept in-boundary (#265).

## [core-ts 0.6.1] — 2026-08-14

### Added
- Launcher-declared `extraWriteRoots` launch option (core#259, #261).
- ACP elicitation maps, Rust half (core#234 reland, #258).

### Fixed
- `write_lock` held around every ACP `proc.stdin` write (FINDING-254, #257); ACP tool name resolved
  from `toolCall.name` before `toolCall.title` (core#100, #247).
- Coverage gate requires at least one genuinely resolved requirement (#251); bus-path fail-closed +
  evaluator≠creator wire contract (P9, #250); warn when evaluator≠creator separation cannot be
  enforced (#248); `feature/test` workflows armed with `evidence_floor()` (#256).
- Estate deps bumped 0.14.3 → 0.14.5 (#252).

## [core-ts 0.6.0] — 2026-08-12

### Added
- **Project model** — projects + memberships + durable interaction requests (DES-PROJECT-001, #246).

### Changed
- Depend on wicked-estate via crates.io versions, not path (#245); napi release pinned to the
  estate release tag (#244).

## [core-ts 0.5.0] — 2026-08-10

### Added
- **A run consumes the repo's estate graph** — ACP parity + repo-scoped graph surface (core#122,
  #240); `ToolInvoked` observability event (FINDING-046, #239); cache-token breakdown on
  `cliUsage` (FINDING-012, #223); domain graph persisted into estate.db with a read boundary for
  governed extraction (#213, #237).

### Fixed
- Coverage gate recomputes from the store — never trusts the creator's `coverage-report.json`
  (#230); repo coverage computed over the repo's own graph (FINDING-009, #225); content-free
  requirement accounting denied (#210).
- Filesystem boundary extended to Bash write targets, with `/dev/null`/fd-dup and glued-separator
  fixes (FINDING-045, #226–#228); advisory boundary READ deny unblocks unattended governed runs
  (core#219, #220); a governed worker's write into its own `~/.claude` tree is advisory, not fatal
  (#236).
- One canonical `humanConfirm` parser that fails closed (FINDING-019, #224); single-seat roster
  short-circuits the council (FINDING-010, #222); transient single-shot worker failures retried
  (#216); `register_repo` root canonicalized (core#214, #221).
- Reverted the first ACP elicitation landing (#212 → #233); re-landed later in 0.6.1.

## [0.4.0] / [core-ts 0.4.1] — 2026-08-05

Joint release (#198; the engine bump carries no separate git tag — `core-ts-v0.4.1` is the release
commit).

### Added
- **An installer that verifies what it installed** — deploy step probes the deployed binary
  (FINDING-081, #195, #198).
- Requirement-string concentration reported in coverage (FINDING-131, #180).

### Fixed
- **Version-lock between the engine and the gate-hook CLI** (core#167, #181): the gate refuses a
  protocol-mismatched hook, keyed on the binary's identity, not its path (FINDING-083, #194).
- Coverage validator measures the REPO's graph, not the actor's (FINDING-091, #196); a coverage
  report over zero behavior-bearing nodes is not a pass (FINDING-009, #190); coverage gets its own
  store carrier (core#166, #182).
- A denial names what it MEASURED (FINDING-092, #197); skipped workflows and governance DENYs say
  WHY (#191); claims stamped from the wall clock (FINDING-017, #192); evaluator contradicting its
  own verdict fails closed (FINDING-085, #188).
- Installed workflow defs with an unknown pin refused at dispatch (#187); tool-call paths outside
  the unit's boundary refused (FINDING-045, #189); each run's own repo bound into its Tool phases
  (FINDING-075, #179); runs left `executing` with no worker announced (core#124, #183); validator
  seat rotation past a seat that cannot run (core#132, #185); estate `ValidationClaim` adopted,
  pin moved off a stale tag (FINDING-078, #184).

## [core-ts 0.4.0] — 2026-08-04

The E2E-campaign hardening wave (FINDING-0xx series from the 15-repo corpus).

### Added
- Policy/conformance-rule **retirement** (FINDING-038, #149).
- Deterministic **evidence floor** on the built-in Evaluator phases, extended to the shipped
  drop-ins (#154, #177); replacement workflows may not silently remove a validator pin (#155);
  shipped drop-in's validator seeded on the plan path (FINDING-066, #164).
- One hardening chokepoint for every process spawn (#168); cross-artifact constants pinned in
  lockstep tests (#171).

### Fixed
- **Operational store kept out of every worker's reach** (FINDING-067, #165) — the deliberate
  enforcement/discovery store split later formalized as the fan-out contract.
- Worker CLI config isolated from the operator's (FINDING-047/045, #153).
- Council: quorum counted, a panicked council no longer takes the run with it (FINDING-026, #151);
  a vote is the option it names (FINDING-056, #158); three timing budgets measured for real
  (FINDING-040, #150); dispatch budget real, correct, affordable (#147).
- Validator: "could not run the check" no longer reported as "the check said no" (#156); the
  judge-prompt reason kept (FINDING-064, #163); a governed unit that cannot be armed says so
  (FINDING-063, #162); governed units refuse ungovernable ACP paths (FINDING-060/061, #161).
- Abandoned chat sessions reclaimed — idle TTL, pool cap (FINDING-027, #152); cached input tokens
  counted on the wrapped path (FINDING-058, #159); worktree existence verified, not inferred
  (FINDING-059, #160); dead `repo-graph` workflow deleted (FINDING-070, #175); onboarding's
  unpassable `domain` phase dropped (FINDING-068, #174); one spelling for a repo's code graph
  (FINDING-069, #172).

## [core-ts 0.3.0] — 2026-07-30

### Added
- **Chat sessions** — warm ACP seat pool + parallel group fan-out (core#13, crew#165, #134).

## [0.3.1] / [core-ts 0.2.1] — 2026-07-28

### Fixed
- Launch preflight is synchronous at `LaunchRun`, covering Tool-executor phases (#120 → #121,
  #123); onboarding `domain` phase runs domain-graph, `--help` guarded (#125).
- Plugin-skill invocation form + Unknown-command no-op tripwire (#126 → #127); banner-tolerant
  validator verdict parse via keyword-alone contract lines (#128 → #129).

## [0.3.0] / [core-ts 0.2.0] — 2026-07-27

### Added
- **Full-roster ACP** — native copilot stdio + native opencode ACP, registry fixes, governed
  fall-through for non-claude CLIs, adapter provenance docs, Windows `.cmd` launcher shims
  (#109–#111).
- **Live council deliberation** — events, seat lenses, 75% approval bar, runoff ballots (#108);
  votes parallelized + distribution-thread panics guarded (#107).
- Agent-judged failure triage (#115); environment-refusal escalation ladder — auto-grant, else
  bubble to operator (#114); external-transform assumption capture (#116); PTY stall detection +
  `collab` built-in workflow (#117).

### Fixed
- Usage parsed from the ACP prompt result — Burn panel no longer empty for ecosystem adapters
  (#113); operator messages delivered to ACP-backed runs (#112).

## [0.2.0] — 2026-07-21

### Added

- **P0→P3 orchestration pipeline (P4a partially complete)** — `WorkflowDef` JSON-driven execution: plan → distribute → govern → resume. Single-writer store actor with `Command`/`CoreEvent` API; no SQLite races from competing readers. (ISS-009 dual-cursor drift deferred to P4a; see Known open items.)
- **napi-rs TypeScript bindings** (`crates/wicked-core-ts`) — `launchRun`, `subscribe`, `confirmGate`, `sessions`, `sessionsDetail`, `workOutput`, `registryRoster`, `registerWorkflow`, `listPolicies`, `listConformanceRules`, `listClaims`, `upsertPolicy`, `getCoverageReport`, PTY terminal methods. Ships as platform-native `.node` binaries for macOS x64/arm64, Linux x64/arm64, Windows x64 via `napi-release.yml`.
- **Multi-platform CI** — `ci.yml` `check` job extended to 3-OS matrix (`ubuntu-latest`, `macos-latest`, `windows-latest`). Unix-gated tests (`#[cfg(unix)]`) skip cleanly on Windows.
- **wicked-apps-core Postgres backend** — store seam `&mut dyn GraphStore` + concrete `AnyStore` owner + `open_store_any`/`--features postgres`. Postgres round-trip tested in CI (`postgres-parity` job).
- **Output-governance observability** — full EVT-001..016 event wave: `WorkflowSelected`, `WorkerSessionStarted/Reused/Closed`, `AcpSessionStarted/Fallback`, `UnitContextInjected`, `UnitOutputCaptured`, `UnitReworkAmended`, `StepFailed`, `CrashRecoveryRedrive`, and governance-deep events (EVT-008..011, EVT-016).
- **Campaign scheduler** (`DES-CAMPAIGN-001`) — DAG-based multi-session orchestration with crash-resume for stranded campaign nodes.
- **PTY terminal sessions** (`DES-TERMINAL-001`) — interactive PTY capability with backpressure hardening; exposed via napi binding.
- **Workflow drop-in JSONs** — pre-built `chat` and `onboarding` sub-workflow definitions loadable via `registerWorkflow`.
- **Blind capability routing** — council voters never see CLI names; `AgenticCli` opaque to the router.
- **Worker message injection + unit reassignment** — `core#92` worker API: inject a message mid-run or reassign a unit to a different worker.
- **Gate-hook exe resolution** — correct path resolution when loaded as a napi-rs addon (`#95`).
- **Campaign crash-resume hardening** — running campaign nodes no longer stranded on resume (`374accc`).

### Fixed

- **ISS-001 Actor lifecycle** — actor thread now terminates when all `Core` handles are dropped: `ShutdownGuard` + `Command::Shutdown` + drain in-flight workers before exit. Test: `actor_shuts_down_when_last_core_drops`.
- **ISS-002 Idempotency** — duplicate `StepOutput` for an already-applied unit is discarded with no store change: four guards in `apply_step_result` (terminal status + cursor + attempt); stale result returns `StepApplied::Stale`.
- **ISS-003 Gate-hook read-only** — hook subprocess uses `open_store_ro` (`SQLITE_OPEN_READONLY`); no WAL/DDL; no `SQLITE_BUSY` from hook path.
- **ISS-004 Governance deny-mid-run** — a denied unit produces terminal `SessionStatus::Failed` (not `Completed`); subsequent units do not run.
- **ISS-008 Crash+resume cursor** — `resume_run` re-dispatches from `session.unit_ix` only; `FastRunner` fixture asserts `*ran == vec![1]` (not a full re-run from 0).
- Council distribution moved off actor thread (ISS-006): council vote no longer freezes the single-writer actor for the full vote duration.
- Git worktree creation moved off actor thread.
- `finalize_run` correctly propagates governance outcome for the interactive engine path.
- PTY terminal teardown hardened against backpressure races.
- `ThreadsafeFunction` lifecycle bugs in the napi binding repaired.
- `cross_language_roundtrip` test correctly marked `#[ignore]` (requires node + sibling wicked-bus; run with `--ignored`).

### Known open items (deferred)

- **ISS-007** (MEDIUM) — P0 SQLITE_BUSY test does not create real writer-writer contention; deferred.
- **ISS-009** (MEDIUM) — Dual-cursor drift between `workflow.current_index` and `session.unit_ix` on denial; deferred to P4a.
- **ISS-010** — crates.io publication blocked by path dependency on `wicked-estate-store` and four `publish = false` vendored crates; resolves when estate publishes.
