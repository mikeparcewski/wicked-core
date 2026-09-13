# DES-FLOOR-001 — The repo-checks floor: creator + verify, classified, base-compared

- **Status:** IMPLEMENTED (hardening train S4a; gate actions on the classifications are S4b)
- **Date:** 2026-09-13
- **Scope:** wicked-core `src/repo_checks.rs`, `src/plan.rs`, `src/pipeline.rs`, `src/cli_runner.rs`, `src/event.rs`, `src/domain.rs`, `src/actor.rs`; core-ts `CoreEventJson`
- **Related:** F-039 (the verify floor, core#414), F-7R2-005 (the default floor), core#456 (provisioning), core#467 (creator floor), core#469 (timeout classification / per-repo config / load-aware bound), F-RC2-009 (sandbox false failures → baseline diff), core#459 / core#464 (gate route-back — S4b)

## 1. Problem

The floor re-derives "done" by running the repository's own checks (`typecheck`, `lint`, `test`, `cargo test`) in the run's worktree and denying the unit when one exits non-zero. Three acceptance losses on 2026-09-13 showed it ran in the wrong place, classified the wrong way, and compared against nothing:

1. **No floor at the creator (core#467).** The `bug` `fix` phase was judged on "left a change" only. A fix worker left a tree failing typecheck (exit 2) and lint (exit 1), called the error "pre-existing" (the base was clean), passed, and the red tree reached the read-only evaluator, which fixed it in place, tripped the worktree guard, and the run was lost at an escalation gate with no route back.
2. **A timeout was a failure (core#469).** A correct crew fix passed typecheck and lint; the full `npm test` (5 min in CI) exceeded the fixed 1200 s bound under acceptance-host load and the unit was DENIED-final.
3. **The sandbox produces false failures (F-RC2-009).** A verify floor reported 27 `cargo test` failures on a change whose 27 tests pass on the run base and on the head in a normal shell. The floor had nothing to compare against.

## 2. Design

### 2.1 Whose floor: `FloorStage`

`plan_from_def` sets `default_floor` on the def's `executes_code` **Creator** phase even when a later `verified_evidence` phase follows (`default_floor = !is_tool && (!later_verifies || code_creator)`). The worker thread already arms the checks off `default_floor` when the tree changed, so the creator's floor runs at the END of its phase with no further wiring; the verify phase keeps `repo_checks_floor`. The stage rides the run as `FloorContext.stage` (`Creator` | `Verify`) and the event as `repoChecksEvaluated.floor`.

A red creator floor pauses the run at the escalation gate on the creator (core#464: every fold denial pauses; `denial_class` books `repo_checks` and `repo_checks_timeout` as `floor_failed`) one phase earlier than before, with the check tails on the record — retry / cancel today. S4b adds the arms those classes key on: re-dispatch the creator with the tails, extend / targeted / accept on a timeout, accept-suggestion.

### 2.2 Classification: a timeout is not a failure

Every `CheckRun` carries `outcome: passed | failed | timed_out | could_not_run`, the effective `bound_s` and a `bound_note`. `RepoChecksReport::timed_out()` is true when the floor stopped on a check that hit its bound; `outcome()` is then `timed_out` and `denial_source()` is **`repo_checks_timeout`** (new; `repo_checks` otherwise). The denial is worded "did not FINISH — the change is unverified by it, not refuted" and names the remedies. `passed()` stays false for a timeout — deny-dominates is unchanged; what changed is that the gate can tell a timeout apart.

### 2.3 Baseline diff (F-RC2-009): deny only regressions

When a check FAILS on the head (not a timeout, not a spawn failure, not the install step) and the run knows its base — `FloorContext.base_head` = the **run base commit** (`WorkUnit.run_base_commit`, recorded on the unit at dispatch from the session's `base_commit`; the dispatch baseline's HEAD only when the run has no recorded base — a creator that COMMITS its work moves that HEAD onto the head itself, and base == head compares nothing), `git_dir` = the pinned git dir the baseline was taken through — the floor:

1. reads this run's cache first (`base-cache/<head>-<name>.json`); on a miss exports the base commit into the checks' scratch (`<worktree>/tmp/wicked-checks/base`) with `git read-tree` + `git checkout-index --prefix` through the pinned git dir — plain files, no nested worktree, inside the OS write boundary, outside the guard's snapshot;
2. runs the same check there (the base's own detection of it; a targeted command keeps the head's substituted argv), after the base's install step when its tree needs provisioning; caches the result and **removes the export at once** — a full copy of the repo tree inside the worktree would otherwise be swept up by the next HEAD check that globs from the worktree root (vitest's default include, a broad `tsconfig`, eslint without ignores). A run pays for a base check once through the cache, never through a persisted export; a stale export (a crash mid-floor) is removed before the first HEAD check;
3. compares **failure identifiers** streamed off the runner's output while it runs (`failure_id_of_line`: `test x ... FAILED`, ` FAIL  file > name`, `● suite › name`, `path(l,c): error TSnnnn: …` with the position dropped, `FAILED tests/x.py::y`, `--- FAIL: TestX`, eslint stylish `file` + `l:c severity message rule`), capped at 1000 per check.

Classification (`CheckRun::classification`, `pre_existing`, `regressions`, `base`):

| base | head vs base | classification | denies |
|---|---|---|---|
| passes | — | `regression` (all head failures head-only) | yes |
| fails, ids both sides | any head-only id | `regression` (shared ids listed as `pre_existing`) | yes |
| fails, ids both sides | equal sets | `floor_env_mismatch` | no |
| fails, ids both sides | head ⊂ base | `pre_existing_in_sandbox` | no |
| fails, no ids either side | same exit code | `floor_env_mismatch` | no |
| fails, no ids either side | different exit code | `regression` | yes |
| fails, ids one side only | — | `regression` (cannot be compared) | yes |
| did not finish / could not run / export or install failed / opted out | — | none (`base.error` says why) | yes (fail-closed) |

`CheckRun::denies()` = `!passed() && classification ∉ {pre_existing_in_sandbox, floor_env_mismatch}`. The loop stops at the first DENYING failure; a tolerated failure is recorded and the floor moves on. `RepoChecksReport.passed` = no check denies. The floor's environment (`FloorEnv`: HOME, TMPDIR, locale, network policy, sandbox level, PATH, passthrough variable NAMES) is recorded as `env` so a `floor_env_mismatch` can be read against CI's environment. **The sandbox itself is not widened.**

`baseline_diff: false` in the repo config opts out (the failure denies as before, `base.error` names the opt-out).

### 2.4 The creator's "pre-existing" claim (core#467)

At the creator stage the transcript is scanned conservatively (`detect_claim`: a claim phrase — "pre-existing", "already failing", "fails on main", "unrelated to my change", … — AND a check-shaped word in the same sentence; a `.` ends a sentence only when followed by whitespace, so a dotted file name is one token). A claim is judged against the first failing check's classification: `claim_rejected` (regression — the base is green), `claim_confirmed` (the base fails it too), `unverified` (no comparison possible). Rides `repoChecksEvaluated.claim` and the denial text. The judge criterion for the creator already composes the pinned evidence floor with the checks' criterion when the checks ran (`pipeline.rs`), so "floor green" is part of what the creator's gate asserts.

### 2.5 Per-repo configuration: `.wicked/checks.json`

```json
{
  "typecheck": ["npx", "tsc", "-p", "."],
  "lint": false,
  "test": "npm run test:ci",
  "test_targeted": ["npx", "vitest", "run", "--changed", "{base}"],
  "timeout_s": 1800,
  "full": false,
  "baseline_diff": true
}
```

- A command is an argv array or a whitespace-split string (no shell); `false` disables the auto-detected check of that name; unknown keys, bad values and `timeout_s` outside `1..=14400` fail detection CLOSED (`detect_error`), never a silent default. `.wicked` and `checks.json` are probed like the manifests (symlinks refused).
- A configured `test` replaces every auto-detected test check (`test`, `cargo-test`).
- **Targeted first:** `test_targeted` stands in for the full test set at the creator stage always, and at verify unless `full: true`. `{files}` expands to the paths the change touched relative to the base (tracked A/C/M/R + untracked-not-ignored, engine scratch excluded), `{base}` to the base commit id; an anchored command with no known base falls back to the full set.
- `timeout_s` is the per-check BASE bound (the install step keeps its own 15 min).

### 2.6 Load-aware bound

`bound = base × factor`, `factor = clamp(load1 / ncpu, 1.0, 3.0)` — the host's 1-minute load average (`getloadavg`; unknown ⇒ 1.0 on Windows) over its logical CPUs. Never below 1 (an idle host keeps the base bound), capped at ×3 (a wedged host must not hold a unit for hours). Sampled per check; recorded as `bound_s` + `bound_note` (`1200s × 2.40 (1-min load 33.6 / 14 cpus)`) and logged before the check starts and, with the observed duration, after it ends.

## 3. Wire (all additive)

`repoChecksEvaluated`: `outcome`, `floor`, `claim: {phrase, check, verdict} | null`, `env: {home, tmpdir, locale[], network, sandboxLevel, path, passthrough[]} | null`. Each `checks[]` entry: `outcome`, `boundS`, `boundNote`, `failureIds[]`, `classification | null`, `preExisting[]`, `regressions[]`, `base: {head, cached, run: <check run> | null, error | null} | null`. Denial sources: `repo_checks`, `repo_checks_timeout`. `WorkUnit.run_base_commit: string | null` (serde default) on the persisted unit. Rust `Option` ⇒ `null` on the wire (never absent).

## 4. Tests

- `src/plan.rs::plan_from_def_floors_the_code_creator_even_when_a_later_phase_verifies` — fix `(default_floor, repo_checks_floor) = (true, false)`; triage/reproduce `(false, false)`; verify `(true, true)`.
- `src/repo_checks.rs::a_check_that_hits_its_bound_is_timed_out_not_failed` — `timed_out`, `bound_s` ∈ 1..=3 for a 1 s base, `denial_source == repo_checks_timeout`, "did not FINISH".
- `src/repo_checks.rs::per_repo_config_prefers_targeted_tests_and_fails_closed_on_a_bad_file` — targeted at creator, at verify until `full: true`; `{files}`/`{base}` substitution off a real git base; `timeout_s`; `false`; unknown key / bad value / out-of-range bound / non-JSON ⇒ `Err`.
- `src/repo_checks.rs::the_load_factor_is_bounded`, `::failure_identifiers_are_scanned_off_runner_output`, `::a_pre_existing_claim_is_detected_conservatively`.
- `src/repo_checks.rs::baseline_diff_denies_only_regressions` — Cargo fixture: regression (denied, claim rejected, base attached, export removed, cache present), identical failures (`floor_env_mismatch`, passes, base cached), subset (`pre_existing_in_sandbox`, passes), opt-out (denies, reason on record).
- `tests/evaluator_worktree_guard.rs::a_regression_the_creator_introduces_is_denied_at_the_creator_gate_before_verify` — through the real actor on the `bug` def: fix denied (`repo_checks`, `floor: creator`, `regression`, base green), verify never runs, run paused at the creator's escalation gate (`floor_failed`).
- `tests/evaluator_worktree_guard.rs::a_check_the_base_fails_identically_is_recorded_not_denied` — both floors `floor_env_mismatch`, run `Completed`, verify's base run `cached`.

On a host with no OS sandbox tool every ran-checks assertion branches to the existing fail-closed contract (nothing runs unsandboxed).

## 5. Out of scope here (S4b, Lane A)

Gate ACTIONS on the classifications: re-dispatch the creator with the check tails as the amendment (bounded retries); on `repo_checks_timeout` extend the bound / run `test_targeted` / accept typecheck+lint+targeted as the floor; apply a green evaluator suggestion ref as the creator's amendment. This PR emits the classified results those gates consume.
