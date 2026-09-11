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

### Added
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
