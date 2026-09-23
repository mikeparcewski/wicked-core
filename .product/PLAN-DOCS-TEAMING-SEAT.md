# PLAN-DOCS-TEAMING-SEAT — staged documentation work order for DES-TEAM-001 (#590) and DES-SEAT-001 (#591)

- **Status:** WORK ORDER — **no edits have been made from this plan.** It is the catalogue of what will need changing and when.
- **Date:** 2026-09-22
- **Scope:** documentation, site and type-declaration surfaces across the wicked-* ecosystem
- **Related:** wicked-core#590 (DES-TEAM-001), wicked-core#591 (DES-SEAT-001), wicked-core#537/#559/#560 (the load-factor train), wicked-crew#662 (`revisesPr` contract), wicked-crew#660 (the docs-ahead-of-code precedent)

---

## 0. Rules of engagement

**Do not change a user-facing capability claim for #590 or #591 until the code lands.** Neither design is built. wicked-crew#660 shipped this morning and had to say explicitly *"the plumbing is not wired yet"* to avoid exactly this error. A doc that describes teaming or seat-instance identity in the present tense before the seam exists is the same defect in the other direction.

Three tiers, and they must not be mixed in one PR:

| Tier | What | When |
|---|---|---|
| **A** | Wrong about **shipped** behaviour today | **Now.** Independent of #590/#591. |
| **B** | Contract/type drift on shipped behaviour | **Now**, in the owning repo, tracked separately. |
| **C / D** | Describes behaviour #590 / #591 will change | **With the code**, never before. |

---

## 1. Verification method used to build this catalogue

Every repo was searched on `origin/main` (`git fetch -q origin main`, then `git grep … origin/main`) with **no pathspec restriction**, because a `--include='*.md'` scoping error hid a stale claim in an `.astro` file earlier in this program. Every negative finding below carries a positive control run with the same command shape and the same unrestricted scope, reporting both counts — a control proves the **scope** was right, not merely that grep executed. Concepts were searched by distinctive single words (`council`, `ballot`, `seat`, `loadavg`, `benched`, `revisesPr`) and the surrounding paragraph read, because claims wrap across lines.

---

## 2. Tier A — already stale, safe to fix now

### 2.1 The obsolete "load ~10" launch rule

**Ground truth (shipped):** `crates/wicked-council/src/dispatch.rs:205` `ballot_load_factor` scales the ballot budget by `clamp(max(load1, load5) / cpus, 1, BALLOT_LOAD_FACTOR_CAP)` (doc comment `:199`; clamp at `:213`; cap `BALLOT_LOAD_FACTOR_CAP: f64 = 5.0` at `:168`), applied at `:727`. Two further mechanisms close the rule's stated consequences:

- A load-induced timeout is **exempt from the bench streak** — `dispatch.rs:730` (`let load_induced = factor >= 2.0;`) consumed at `:561-579`.
- A benched seat can no longer silently reseat the evaluator on the creator: distribution returns `NoEligibleSeat` (`src/actor.rs:1309`; core CHANGELOG entry for #537/#560).

So the fixed `~10` threshold was replaced by a **ratio** (`max(load1,load5)/cpus`). On the 14-cpu rig, load 10.9 gives factor 1.00 and load 33.6 gives 2.40 — the rule's number is not merely conservative, it is not the same quantity.

| # | File:line | Current claim | Action |
|---|---|---|---|
| A1 | `/Users/michael.parcewski/Projects/wicked/program-2026-09/backlog-recon-2026-09-22/ADDENDUM.md:76-81` | *"## Launch discipline (unchanged, learned the hard way) … Governed runs are **serialized** — parallel councils starve each other. … **Check `loadavg < ~10` before every launch.** Above it, ballots get benched and the evaluator lands on the creator seat, which invalidates the verdict."* | Replace. All three clauses are now handled in code (budget scaling, streak exemption, `NoEligibleSeat`). The header word *"unchanged"* is the falsified part. |
| A2 | `/Users/michael.parcewski/Projects/wicked/program-2026-09/backlog-recon-2026-09-22/PLAN.md:216` | *"check `loadavg < ~10` before each launch — a reviewer's test suite on the rig host benches council seats and lands the evaluator on the creator"*, stated as a **standing rule of the program** | Replace the load clause. The `api-types`-must-be-published half of the same sentence is unaffected and stays. |
| A3 | `/Users/michael.parcewski/Projects/wicked/program-2026-09/backlog-recon-2026-09-22/PLAN.md:199` | *"Governed runs are serialized here (parallel councils starve each other)"* | Partially stale. Memory-spike serialization may still be a valid reason; *"starve each other"* is no longer the mechanism, and the plan's whole sequencing is justified on it. Re-justify or drop. |
| A4 | `/Users/michael.parcewski/Projects/wicked/CLAUDE.md:78-80` | *"councils are the single spikiest thing the platform does. Serialize governed runs — parallel councils starve each other **and can't reach agreement**, while a solo council converges in ~3 min."* | Partially stale, and this is the doctrine A1/A2 cite. *"can't reach agreement"* is the obsolete clause — agreement failed because ballots timed out at a fixed 40 s, which `ballot_load_factor` now scales. The memory-spike rationale in the preceding sentence is untouched. **Note this file states no numeric threshold, so a phrase-grep for `load ~10` never reaches it** — it was found by concept. |
| A5 | `wicked-studio` `e2e/test_feature_live.py:24-27`, implemented at `:849` | A **hard refusal-to-launch preflight** requiring *"1-minute load average < 20"* (`sysctl -n vm.loadavg`, first field only) | Adjacent, partially stale: same operating pattern at a different number, and it reads only the 1-min average — which `dispatch.rs:200-204` records as the one that did **not** predict the failure (1-min 10.9 vs 5-min 15.6; pi answered at 40.8 s). Not simply wrong: it also gates swap and concurrent runs, which #537 does not address. Re-scope the load clause only. |
| A6 | `wicked-studio` `e2e/artifacts/test-feature-live/LT-1-plan-d293f4d7-1e45-4346-809a-d6f2107c6b18.md:43` | *"heaviest/spikiest tier (ops playbook: serialize, never run concurrently)"* | Frozen run artifact, not live guidance — lowest priority. Listed because it restates the rule as doctrine and cites "the ops playbook" (A4) as authority. |

**Not stale — deliberately excluded after reading:** `wicked-core/src/repo_checks.rs:92` (the verify floor's own `clamp(load1/ncpu, 1, 3)` — a sibling of the ballot factor, engine code, not an operator rule); `wicked-crew/scripts/test-related.mjs:51` and `packages/crew/vitest.config.ts:18` (load recorded as **telemetry** so a verdict at load 130 is distinguishable from one at load 10 — explicitly not a gate); `wicked-ci/smoke/bin/wicked-smoke.mjs:169` (loadavg in a smoke report); `wicked-crew/packages/crew/src/core/adapter.ts:645` (*"the ecosystem's spikiest operation, serialized on purpose"* — argues phase composition, not a load threshold; same premise as A4, worth a glance when A4 is edited).

**Negative findings, with positive controls.** Searched `origin/main`, no pathspec, all file types; the per-repo control counts `.astro`/`.html`/code/`.md` separately to prove site pages were in range.

| Repo | Load-rule hits | Positive control (`governed|wicked`, same scope) | `.astro`/`.html` in control |
|---|---|---|---|
| wicked-core | 0 | 288 files | 0 (repo has none) |
| wicked-crew | 0 | 406 | 2 |
| wicked-garden | 0 | 722 | 3 |
| wicked-studio | 0 for the `~10` string (A5/A6 are different strings, found by concept) | 333 | 3 |
| wicked-estate | 0 | 537 | 1 |
| wicked-interactive | 0 | 126 | 5 |
| wicked-bus / installer / ledger / vault | 0 | 120 / 34 / 26 / 43 | 0 (none exist) |
| wicked-web | 0 | 9 | 4 (all four of its `.astro`) |
| wickedagile | 0 | 19 | 3 |
| wicked-ci | 0 | 77 | 1 |
| `scratch/` (non-repo, plain `grep -rl`) | 0 | 233 | — |
| `estate-review/`, `recon-2026-08/` | 0 | 127 / 6 | — |

wicked-core's own root docs are clean: `README.md`/`DESIGN.md`/`ORCHESTRATOR.md`/`AGENTS.md`/`HANDOFF.md`/`CLAUDE.md`/`REASSESS-P0-P1.md` yield 0 load-rule hits (the `serialize`/`deserialize` hits are SQLite single-writer text), against a positive control of 1–12 `council` hits per file in the same set.

### 2.2 The `.product/` location claim is wrong in every code product

**Verified:** `.product/` is **gitignored and tracked in zero commits** on `origin/main` in all six code products — wicked-core, wicked-crew, wicked-estate, wicked-garden, wicked-studio, wicked-interactive (each `.gitignore` carries a `.product/` entry; `git ls-tree -r origin/main` returns 0 `.product/` paths in each). wicked-core removed it deliberately in `8663d4b` — *"chore: remove .product from the published tree (#562)"*, an index-only removal plus a `.gitignore` entry *"so they are not re-added"*.

| # | File:line | Current claim | Action |
|---|---|---|---|
| A7 | `/Users/michael.parcewski/Projects/wicked/CLAUDE.md:126` | *"**Per-product design + requirements artifacts**: `<product>/.product/` — present in the six code products (estate, garden, interactive, studio, crew, core)"* | Wrong for all six. They are local-only and absent from a fresh clone. |
| A8 | `/Users/michael.parcewski/Projects/wicked/CLAUDE.md:27` | *"See `wicked-core/README.md` + `.product/DES-EXEC-001`."* | `.product/DES-EXEC-001` cannot be opened from any clone. |
| A9 | `wicked-core` `CLAUDE.md:5-10` | Already correct and should be the model for A7: *"The `.product/` design docs … are cited all over the source but are **gitignored** … and tracked in no commit — a fresh clone has none of them."* | **No change needed** — unless this PR's placement decision (§5) is accepted, in which case this paragraph must be amended to say some `.product/` files are now tracked. |

### 2.3 Other claims that are wrong about **shipped** behaviour

Independent of #590/#591. Each row marks whether it was **re-derived here** or is **reported, pending verification** — verify before editing.

| # | File:line | Claim vs reality | Verified |
|---|---|---|---|
| A10 | `wicked-crew` `packages/crew/defaults/workers.json` | The shipped default roster is **one seat**: a single `{"id":"claude","command":"claude","args":["--print"],"council_capable":true}`. The site advertises an 8-entry roster (`site/src/pages/index.astro:73-86`) and *"a full six-seat roster"* (`site/src/content/control-plane.md:127`). | ✅ re-derived |
| A11 | `wicked-core` `crates/wicked-council/src/lib.rs:7-8` | *"with the real CLIs available in this environment (`claude`, `agy`, `pi`)"* — the registry holds six (claude, agy, codex, copilot, opencode, pi; `registry.rs:130-133`). | ✅ re-derived |
| A12 | `wicked-core` `crates/wicked-council/src/lib.rs:22-24` and `crates/wicked-council/src/bus.rs:7` | Both name the events `wicked.council.requested` / `wicked.council.voted`. The shipped constants are **`wicked.crew.council.*`** — `EVENTS.md:22-25` (`wicked.crew.council.{requested,voted,deliberated}`, `wicked.crew.council_seat.failed`). Event-grammar drift in a doc comment. | ✅ re-derived |
| A13 | `wicked-garden` `hooks/scripts/post_tool.py:829` | `import consensus_gate` — but **`scripts/crew/consensus_gate.py` does not exist on `origin/main`** (0 tracked paths). It is wrapped in a fail-open `try/except`, so garden's consensus/reviewer gate is **dead code**. Consequently `WICKED_GARDEN_BUS_EVENTS.md:58-61` documents four `wicked.garden.consensus.*` events with no live producer, and `scenarios/crew/consensus-chain-id-uniqueness.md` cannot run. | ✅ re-derived (file absence + import site) |
| A14 | `wicked-crew` `site/src/content/control-plane.md:126-138` and `docs/articles/…:126-138` | The article states its own convergence math is **obsolete** and ships anyway: *"the convergence math is under revision because implementation showed it wasn't doing the work the design assumed."* | ⚠️ reported |
| A15 | `wicked-crew` `site/src/pages/index.astro:20` vs `:74-76` | *"Never depict one-shot `-p` dispatch"* against *"Each is dispatched as a headless CLI subprocess: the prompt is substituted into argv (`{PROMPT}`) and the result is read from stdout."* Same file. | ⚠️ reported |
| A16 | `wicked-crew` `packages/crew-api-types/index.d.ts:550-557`, `:638-648` | `inactive` is *"NO LONGER PRODUCED from crew 0.7.36"*; `council_bench` is `@deprecated` and *"ABSENT from crew 0.7.36"*. Studio's `HealthRailSection.tsx:66-74` and `ReassignControl.tsx:26-29` still carry copy written around a roster that has no council-eligibility field. | ⚠️ reported |
| A17 | `wicked-core` `ORCHESTRATOR.md:262`, `:315`; `HANDOFF.md:66-67` | `:262` *"Today `distribute` picks one `assigned_cli` per unit… For multi-CLI: change `assigned_cli` to `Vec<String>`"* is present-tense design that never happened (the wire still carries `cli: string`). `:315` / `HANDOFF.md:66-67` list **P5 as NEXT** when P5 shipped. | ⚠️ reported |
| A18 | `wicked-garden` `skills/qe/refs/accept.md:51-60` | The reviewer-isolation CLI table lists *"Gemini CLI / Codex, Cursor, Kiro"*. Gemini CLI is sunsetted per the ecosystem roster; antigravity/opencode/pi/copilot are absent. | ⚠️ reported |
| A19 | `wicked-garden` `skills/workflow/SKILL.md:110` | Names *"Fallback fork skills (facilitator, researcher, implementer, reviewer)"* as live; `CHANGELOG.md:101` records `crew-implementer`/`crew-reviewer`/`crew-researcher` **retired into `governed-worker`**. | ⚠️ reported |
| A20 | `wicked-crew` `README.md:118`; `wicked-core` `src/validator.rs:19` | Both say *"a **human/council** approves it"* / *"the human / council step"* on the validator-approval path, and neither resolves which. **AMBIGUOUS — do not guess**; resolve against the code when C9 is edited. | ⚠️ reported |

**Checked and rejected:** the report that `packages/crew/src/api/server.ts:859` (*"Convening a 6-seat council…"*) contradicts `packages/crew/src/interactive/council-outcome.ts:9` (*"Convening a 5-seat council…"*). Reading both: `server.ts:857-861` is a comment describing a **stub runner narrating a fake lifecycle** (*"Nothing ran. Nothing was written. Two gate approvals were announced anyway."*), and `council-outcome.ts:5-11` records a **fresh-rig observation** where four of five seats were signed out. Neither asserts a roster size. Not a contradiction; no action.

---

## 3. Tier B — `wicked-crew` `revisesPr` (crew#662)

**Catalogue only. Do not fix the type in a wicked-core PR.**

### 3.1 Ground truth

`LaunchRunBody.revisesPr` is a **positive integer PR number**:

- `wicked-crew` `packages/crew/src/api/routes.ts:519` — `revisesPr: z.number().int().positive().optional(),`
- Co-requirements at `routes.ts:545-550` — needs `repoRef` + `workflow`, and `deliver !== 'none'`.
- Reinforced at `packages/crew/src/core/deliver.ts:218` — *"revisesPr must be a positive pull request number"*.

### 3.2 **The "published `.d.ts` is wrong" premise did not reproduce — read this before acting on crew#662**

crew#662 states the contract defect as:

| Source | #662 says it declares |
|---|---|
| `packages/crew-api-types/index.d.ts:193` | `revisesPr?: boolean;` |

Line 193 does read `revisesPr?: boolean;`. **But it is not on `LaunchRunBody`.** Verified on `origin/main`:

- `export interface HealthCapabilities {` opens at `packages/crew-api-types/index.d.ts:186`; `revisesPr?: boolean;` at `:193` is inside it, documented at `:190-191` as *"`LaunchRunBody.revisesPr` is accepted (crew#550; crew ≥ 0.7.36). ABSENT on a daemon before the field — read as `false`"*. As a **capability flag** this is correctly boolean, and `GET /health.capabilities.revisesPr` genuinely is a boolean (`packages/crew/src/api/routes.ts:1127`, `packages/crew/src/core/adapter.ts:1384-1390`).
- `export interface LaunchRunBody {` opens at `index.d.ts:3339`; its `revisesPr?: number;` is at `:3401` — **correct**. The published npm artifact `wicked-crew-api-types@0.38.0` carries the same `number` at its `index.d.ts:3363`.

So **asks 1 and 2 of crew#662 are already satisfied**: the launch field is typed `number`, and the `repoRef` + `workflow` co-requirement *is* stated in the type's doc comment at `index.d.ts:3395-3396` (*"Needs `repoRef` and `workflow` (400 without)"*).

**What remains genuinely valid in crew#662:**

- The **name collision** — the same field name carries two different types on two interfaces ~3200 lines apart, and the `HealthCapabilities` doc comment names `LaunchRunBody.revisesPr` while typing the capability. A reader who greps `revisesPr?: boolean` lands on `:193` and concludes the launch field is boolean. That is how the issue itself went wrong.
- **Ask 3** — `400 Invalid request body` names neither field nor expected type. Valid and unaddressed.
- **Ask 4** — a contract test driving the published `.d.ts` against the live zod schema. Valid and unaddressed.

**Action:** retarget crew#662 onto the collision + asks 3/4, and correct its premise table. Do **not** "fix" `index.d.ts:3401`; it is right.

### 3.3 Catalogue of `revisesPr` doc references

No README, site page, skill or workflow in any repo mentions `revisesPr`. The references are code comments, types and changelogs:

| Repo | File:line | Implies boolean for the launch field? |
|---|---|---|
| wicked-crew | `packages/crew-api-types/index.d.ts:150` | Ambiguous — a capability bullet ("Absent or `false`") that names `LaunchRunBody.revisesPr` |
| wicked-crew | `packages/crew-api-types/index.d.ts:190-193` | Reads as boolean, but is `HealthCapabilities` — correct there |
| wicked-crew | `packages/crew-api-types/index.d.ts:3390-3401` | No — explicitly a number |
| wicked-crew | `packages/crew/src/api/routes.ts:512-519` | No |
| wicked-crew | `packages/crew/src/core/types.ts:58-72`, `core/deliver.ts:49,153,205,218`, `api/retry-index.ts:20-21` | No |
| wicked-crew | `CHANGELOG.md:181, 206-224, 290, 294, 299` | No (`{revisesPr: N}`); `:548-549` ambiguous — both fields named on one line, untyped |
| wicked-core | `crates/wicked-core-ts/index.d.ts:51`, `crates/wicked-core-ts/src/lib.rs:617`, `src/lib.rs:202`, `src/repo.rs:740,772,2417`, `src/actor.rs:1395` | No — all prose about the explicit worktree base |
| wicked-studio | `src/store/retryPrefill.ts:29-34`, `src/components/ChatInput.tsx:613-616`, `src/components/RunDelivery.tsx:347-373` | No — assigns `.number` |
| wicked-studio | `src/components/ChatInput.tsx:249-251` | Yes, and correct — it is the capability |
| wicked-studio | `CHANGELOG.md:37` no; `:90-91` ambiguous (untyped, beside a boolean capability check) |

**Negative findings with positive controls** (pattern `revises[_-]?pr|revision run|revise an existing pr`, `origin/main`, no pathspec): wicked-garden 0 hits / control 1112 tracked files incl. 2 `.astro` with `wicked`; wicked-estate 0 / 859 incl. 1 `.astro`; wicked-interactive 0 / 223; wicked-installer 0 / 43; wicked-web 0 / 10 files of which **all 4 `.astro` matched the control**; wickedagile 0 / 37; wicked-bus 0 / 162 with **21 `.d.ts` matching the control** (proves `.d.ts` in range); wicked-ledger 0 / 29; wicked-vault 0 / 48; wicked-ci 0 / 95. Non-repo dirs (plain `grep -rn`): `scratch/` 0 hits / control 282; `program-2026-09/` 0 / 35; `estate-review/` 0 / 174; `recon-2026-08/` 0 / 78; root `CLAUDE.md` 0 / 11.

Installed copies: `wicked-studio/node_modules/wicked-crew-api-types` is pinned at **0.25.0**, which predates the field (0 hits; control `LaunchRunBody` = 6). The global `wicked-crew` install is **0.7.33**, also predating it (0 hits).

---

## 4. Tier C / D — the work order for when #590 and #591 land

**Nothing in this section may be edited before the corresponding code merges.**

### 4.0 The distinction that governs this section

**Four** different things share the word **council**. Only the first is touched by #590.

| Sense | Where | Touched by #590? |
|---|---|---|
| **The per-phase council** — N seats ballot per workflow phase, 75% agreement, seats bench, produces routing + a verdict | wicked-core `crates/wicked-council/`, `src/distribute.rs`; wicked-crew | **YES** |
| **wicked-garden's `jam-council`** — a multi-model brainstorming / second-opinion panel a user invokes on request | wicked-garden `skills/jam-council/SKILL.md` and 13 sibling skill files | **NO — product feature, stays** |
| **wicked-garden's daemon council** — a synchronous "POST a question, get votes + synthesis" API backing the skill above | wicked-garden `daemon/council.py` (*"Council orchestrator for the wicked-garden daemon … the caller POSTs a question, gets back votes + synthesis"*, `council.py:1-8`), plus its `council_sessions` table | **NO — garden's own, stays** |
| **wicked-garden's consensus/reviewer gate** — `agreement_ratio`, `consensus_threshold`, `strong_dissent_blocks` | wicked-garden `hooks/scripts/post_tool.py`, `WICKED_GARDEN_BUS_EVENTS.md:58-61` | **NO — and it is dead code; see A13** |

**14 wicked-garden `skills/` files mention "council"** (garden has 74 in total): `skills/archetype/refs/{build,decide,review}.md`, `skills/classify/SKILL.md`, `skills/core/SKILL.md`, `skills/jam-brainstorm-facilitator/SKILL.md`, `skills/jam-council/SKILL.md`, `skills/jam/SKILL.md`, `skills/jam/refs/council-verdict.md`, `skills/qe/SKILL.md`, `skills/swarm/SKILL.md`, `skills/swarm/refs/{independent-verification,ship-discipline}.md`, `skills/workflow/SKILL.md`.

**11 of the 14 are clean jam-council and are excluded.** None references 75% agreement, ballots, benching or `RoutingInfo`. Editing them would rewrite a product feature for a change that does not touch it.

**But three are NOT clean, and a blanket exclusion would be wrong:**

| File:line | Text | Sense |
|---|---|---|
| `skills/workflow/SKILL.md:97` | *"CONDITIONAL auto-resolution (AC-4.4): spec gap conditions → fixed inline. Intent-changing conditions → escalate to user or **council**."* — a bare noun inside a crew CONDITIONAL-gate paragraph, in a file whose own frontmatter reads *"Reference for how the **wicked-crew workflow engine** operates — phase catalog, gate enforcement, rigor tiers"* | **AMBIGUOUS, leans per-phase.** Re-read against the code before excluding. ✅ re-derived |
| `skills/archetype/refs/build.md:31` | *"Approve happens through PR review, **council**, or the `review` archetype"* — bare noun; jam-council only by sibling-file inference | AMBIGUOUS ⚠️ reported |
| `skills/archetype/refs/decide.md:36` | *"the user (or a **council**) picks"* — bare noun, but the same file names `wicked-garden-jam-council` at `:79` | Leans jam-council ⚠️ reported |

Garden also carries per-phase surfaces that contain **no "council" word at all** and that a word-grep misses — notably `skills/governed-worker/SKILL.md` (the floor skill handed to every governed unit) and the *"the run's deliver phase opens the PR"* boilerplate repeated across the `qe*` skills. Enumerate these against the #590 diff; do not assume garden is uniformly out of scope. ⚠️ reported

### 4.1 wicked-crew — Tier C (#590)

| # | File:line | Claim it makes today | Why #590 changes it |
|---|---|---|---|
| C1 | `site/src/content/control-plane.md:117-120` | *"every unit is auctioned to the roster: each seat … distinct lens (capability fit, risk, efficiency, output quality), and **the council needs 75% agreement**, with runoff rounds where every seat sees the tally and the dissenters'…"* | The per-phase council becomes one **mode** (DES-TEAM-001 S5), not the routing story. |
| C2 | `site/src/content/control-plane.md:127-130` | *"On a full **six-seat roster**, generic one-line tasks never converged: every seat held its own lens through all three ballots — 17% each — and plurality decided"* | Roster arithmetic assumes one seat per CLI (#591) and a balloting council (#590). |
| C3 | `site/src/content/control-plane.md:203, 211` | *"who won each unit's **council vote** and at what agreement"*; *"**Council ballots per unit**, an occasional triage judge, and a second…"* (cost/overhead section) | Under teaming the per-unit cost model is monitors, not ballots. |
| C4 | `docs/articles/studio-a-control-plane-for-coding-agents.md:117-120, 127-130, 141, 166, 203, 211` | **Near-duplicate of C1–C3 at the same line numbers** (the two files are not byte-identical — verify both independently). | Same claims, second copy. Any edit must hit both or they drift. |
| C5 | `site/src/pages/index.astro:105-135` | The `COUNCIL_ROUNDS` marketing animation: *"A council: heterogeneous CLIs, each answering in its own isolated sandbox … the seats disagree, and deny-dominates the synthesis"*; ballot tails at `:112`, `:118`; *"profiles are numbered, names hidden — no seat can vote for itself"* | The landing page's central mechanic. **This is an `.astro` file — the exact surface a `--include='*.md'` sweep misses.** |
| C6 | `site/src/pages/index.astro:291` | *"council vote, what the human approved, what each CLI consumed…"* (evidence list) | Ditto. |
| C7 | `README.md:137` | *"the agent validator now runs under a **genuinely distinct council seat** — identity-distinct…"* | Wording depends on the council being the seating mechanism. |
| C8 | `README.md:145-146` | *"**Trust model, named honestly:** diverse-seat agent consensus, on a deterministic structural floor… A green run means 'diverse seats + the escalation policy agreed'"* | The trust model statement itself changes shape under teaming (gate decides, findings are input). Highest-care item in crew: it is the honesty claim. |
| C9 | `README.md:118` | *"a human/council approves it (`approve-validator`…)"* | — |

### 4.2 wicked-crew — Tier D (#591)

| # | File:line | Claim | Why #591 changes it |
|---|---|---|---|
| D1 | `site/src/pages/index.astro:73-80` | The seat roster list, annotated *"opencode, pi — wicked-council/src/registry.rs … plus the two catch-alls"*, each entry tagged *"your seat"* | One entry per CLI; instance identity makes the roster a pool. |
| D2 | `site/src/content/control-plane.md:127` **and** `docs/articles/…:127` | *"a full **six-seat roster**"* | Six seats = six CLIs today. Under #591 seat count and CLI count decouple. |
| D3 | `site/src/content/control-plane.md:84` **and** `docs/articles/…:84` | *"(`agy`) that turns on the credential the seat runs under — a consumer Google account…"* | Becomes a **per-instance** credential decision (DES-SEAT-001 §4, core#585). |
| D4 | `README.md:218-229` | *"which credential the seat runs under, not on the fact that it is driven programmatically"*; the OAuth-vs-API-key table at `:222`, `:227`; *"Configuring the seat's credential… remove it from your roster if in doubt"* | Per-instance credentials (DES-SEAT-001 §4). **Must also gain the copilot caveat** — per-USER keychain, so two config homes are *not* two credential slots (`wicked-core` `crates/wicked-apps-core/src/spawn.rs:390-393`). |
| D5 | `README.md:97` | *"an independent **evaluator seat** that reads cold evidence only"* | Needs the instance-distinct-vs-model-distinct disclosure (DES-SEAT-001 I-2/I-3). |
| D6 | `README.md:199` | *"a verified claim about claude-seated governed units, not a blanket property of every run"* | "claude-seated" becomes ambiguous once `claude#1`/`claude#2` exist. |
| D7 | `packages/crew/defaults/workers.json` | The shipped default roster — one entry per CLI | Gains instance shape / pool size (DES-SEAT-001 S4). |
| D8 | `packages/crew-api-types/index.d.ts` | `assignedCli` and the `distinctnessFallback` wire disclosure | New `same_cli_instance` value (DES-SEAT-001 S3) + instance id (OQ-SEAT-1). **Published contract — version and changelog it.** |
| D9 | `packages/crew/src/api/seat-signin.ts:20-37` | **The per-seat config-home table as crew reads it** — and it says *"no configuration-home variable is known for agy, so it runs where the operator does"*, which core#578 changed (agy now gets `HOME = root`, `spawn.rs:445`). Stale today as well as changed by #591. | Instance keying; and S5's per-instance login (wicked-crew#615). ⚠️ reported |
| D10 | `packages/crew/src/api/seat-standing.ts:131, 138-140, 174, 184-186` | Rendered operator strings: *"not enabled for council"*, *"signed out — **a council would bench this seat on its first ballot**"* | Both #590 and #591. ⚠️ reported |
| D11 | `packages/crew/src/core/engine-roster.ts:147-162` | `NO_ELIGIBLE_SEAT_REMEDY` and the 409 *"no eligible seat for `<run>`: `<benched>` — sign a seat in, or add one"* (mirrored at `wicked-core` `src/actor.rs:104`) | Under pooling, "add one" becomes "add an instance". ⚠️ reported |
| D12 | `packages/crew/src/interactive/{draft,chat,demo,edit}-events.ts` + `interactive/council-outcome.ts:44-58` | The **"Convening a N-seat council…"** strings users actually read in document threads, and the suffix *"(1 of 5 seats answered — 4 benched)"* | Four call sites, one helper — change the helper. ⚠️ reported |
| D13 | `packages/crew/skills/wicked-crew/SKILL.md:19-26, 107-108` | **A shipped agent skill installed into users' CLIs**: *"## The one hard rule — evaluator ≠ creator (do not break this)"* | Survives #590 (DES-TEAM-001 §2.5) — listed so it is **not** swept into the rewrite. Note `packages/crew/src/cli/mcp.ts:142-143` exposes the `gate` tool with **no** such warning; worth closing on its own merits. ⚠️ reported |
| D14 | `packages/crew/src/core/deliver-text.ts:439-471` | Text composed into **every delivered PR body** — `'## Evaluator gate'`, `'_This workflow has no evaluator phase._'`, per-phase seat + verdict lines | Highest blast radius of any string here: it lands in public PRs. ⚠️ reported |

### 4.3 wicked-studio

| # | File:line | Claim | Tier |
|---|---|---|---|
| S1 | `site/src/pages/index.astro:378-381` | *"Put one question to several models in the same thread and see **how many seats answered** — '3 of 5 seats', or '3 polled' when the count is unknown"* | **D** (#591) — "5 seats" becomes instances, not vendors. Studio has exactly **one** `.astro` file and it is in scope (positive control: 1 of 1 matched `wicked`). |
| S2 | `site/src/pages/index.astro:68, 313, 895` | *"Group chat with your whole roster: fan one question out, watch each seat answer side by side"*; *"roster fan-out … three seats answered"* | **D** (#591). Note this is the **chat roster**, not the per-phase council — #590 does not touch it. |
| S3 | `e2e/test_feature_live.py:24-27, 849` | The load-preflight gate | **A** — see A5. |
| S4 | **Studio's live UI copy is the largest single surface in the estate** — 82 files mention `council`, 143 mention `seat`, and the repo has **zero** `jam-council` references, so *every* council token in studio is the per-phase one. Highest-value, all ⚠️ reported: `src/components/ChatPanel.tsx:1190` — the BUILD composer subhead *"Describe your goal. **The council elects a CLI**, decomposes the plan, and executes it — you approve each gate."* (the most prominent council claim in the app); `src/components/narrator.ts:201-218` — *"Council convened — polling N agents"*, *"Ballot N: X% — below the ${neededPct ?? 75}% bar, runoff"*, *"Seat X did not vote"*, and `:55` *"evaluator ≠ creator not held — review stays on the creator seat"*; `src/components/councilQuorum.ts:22-44`; `src/components/HealthRailSection.tsx:78-82` — *"signed out — **councils may bench this seat**"*; `src/components/IntakePlan.tsx:125-132` — *"seats are chosen by the council at dispatch"*; `src/components/SystemSettings.tsx:318-343, 546-553` — the **Workers** screen, the only user-facing worker-config-home copy, which renders a raw `CODEX_HOME=… codex login` line verbatim (**directly in #591's path**); plus `RoutingProvenance.tsx`, `AssumptionsPanel.tsx`, `RunDegradedNote.tsx`, `ReassignControl.tsx`, `gateVerdictModel.ts`, `GateVerdict.tsx`, `store/runtime.ts`, `README.md:25-27`. | **C/D.** Sequence behind the wire contract (D8), but the copy is prose, not generated — it will not follow the types automatically. **Preserve** `src/components/GroupChat.tsx:55` (*"NOT a run — no council, no gates, no units"*): it is the line that keeps chat fan-out distinct from the council. |

### 4.4 wicked-core (this repo)

| # | File:line | Claim | Tier |
|---|---|---|---|
| N1 | `ORCHESTRATOR.md` (12 `council` mentions), `HANDOFF.md` (6), `DESIGN.md` (3), `REASSESS-P0-P1.md` (3), `README.md` (1) | Per-phase council as the routing mechanism | **C** (#590) — re-read each when S5 lands; several are historical revision logs that should be left as history, not rewritten. |
| N2 | `crates/wicked-apps-core/src/spawn.rs:386-389` | Names the pi variable correctly as `PI_CODING_AGENT_DIR` | **No change.** Recorded because #591's own table spells it `PI_AGENT_DIR`; the *issue* is wrong, not the code. Fix the issue text. |
| N3 | `crates/wicked-apps-core/src/spawn.rs:292-295` | *"the ACP spawn, the ballot spawn, the wrapped worker and the sign-in command all name, and run under, the same directory"* | **D** (#591) — this invariant is exactly what instance keying must preserve across **both** config-home resolvers (DES-SEAT-001 §3.2b). Update with S2. |
| N4 | `crates/wicked-council/src/worker.rs:67-73` | **The canonical 75%** — `pub const APPROVAL_THRESHOLD: f32 = 0.75;` with `MAX_BALLOTS = 3` and *"degrades to plurality"*. **Every "75%" on every site traces here.** | **C** — if the number changes or becomes mode-dependent, this is the root. ⚠️ reported |
| N5 | `crates/wicked-council/src/dispatch.rs:56-63`, `:100-102` | The ballot prompt **as rendered to the voter**: *"The council needs at least {pct}% of its live seats to converge on one option (a seat the dispatcher has benched abstains and is not counted…)"*; *"You are one independent evaluator on a routing council."* | **C** — this is the council's user-visible text, not just config. ⚠️ reported |
| N6 | `crates/wicked-council/src/types.rs:555-600` | `pub const SEATS` — the four deliberation lenses (Capability Fit / Risk & Failure Modes / Efficiency / Output Quality) with full prompt text | **C** — monitors are a different role model; these prompts do not carry over. ⚠️ reported |
| N7 | `src/distribute.rs:378-408` | The routing-rule doc header — the single load-bearing statement of the whole mechanism (eligibility → ballot ledger → bench → `NoEligibleSeat` refusal → `degraded_reason`) | **C/D.** ⚠️ reported. **Note the file boundary:** `src/distribute.rs` is under active implementation; coordinate, do not edit blind. |
| N8 | `crates/wicked-core-ts/index.d.ts:107-121` **and** `crates/wicked-core-ts/scripts/finalize-dts.mjs:46, 64, 68` | The published napi contract carrying `routingMethod: 'council' \| 'degraded' \| 'evaluator_distinct' \| 'tool'` | **C** (`Teamed`) + **D** (instance id). **The `.d.ts` is GENERATED — edit both files or the change is reverted on the next build.** ⚠️ reported |
| N9 | `EVENTS.md:22-25` (crew council events) beside `:192`, `:223` (garden's jam-council events) | **Both council senses in one file**, and its header says **GENERATED — do not hand-edit** | **C** — change the emit seam, not the doc. See also A12. ⚠️ reported |
| N10 | `crates/wicked-governance/seed/corpus/plane-boundaries.md:37-39` | `PAT-1303`, a **seeded steering rule projected into the estate graph** — *"evaluator≠creator …, deny-dominates dual gates, and 'done' re-derived from evidence"* | Survives #590 unchanged (§4.6 W6). Listed because agents **recall** it: if doctrine ever does change, re-seeding is part of the change, not a follow-up. ⚠️ reported |
| N11 | `workflows/README.md:59, 73, 76, 87` | The workflow-authoring contract — `role: creator\|evaluator\|neutral`, *"Never narrows seat selection"* | **D** — "seat selection" becomes instance selection. ⚠️ reported |

### 4.5 wicked-ci — the densest operator-facing council surface after crew

**Not in the original survey list, and it should have been.** `wicked-ci` `smoke/README.md` is a live operator document describing the per-phase council in detail. Confirmed by reading it. Tier **C/D**.

| # | File:line | Claim |
|---|---|---|
| I1 | `smoke/README.md:121` | *"the published core-ts engine addon (plan → distribute → **councils** → gates → repo-checks floor → deliver script)"* |
| I2 | `smoke/README.md:129-133` | The seat table: `claude` *"the one live seat"*; `opencode` *"a second live seat so the evaluator can be **identity-distinct from the creator**"*; `codex` *"the engine must learn `not_logged_in` from the ballot, not the probe"*; `copilot` *"the engine must **bench `quota_exhausted`**"*; `pi` *"`not_installed` from the spawn error"*. **This table is the #591 scenario in miniature** — it is the smoke harness that encodes one-seat-per-CLI. |
| I3 | `smoke/README.md:134` | `acp-agent` *"Council-disabled in the overlay so S04's roster is unchanged"* |
| I4 | `smoke/README.md:139-141` | *"Every **seat** shim answers four prompt kinds … a **council BALLOT** (`RECOMMENDATION: 1 …`), an agent JUDGE, the FAILURE-TRIAGE judge, and a WORKER turn"* |
| I5 | `smoke/README.md:143-147` | The EVALUATOR-turn convention sentence and the evaluator-verdict gate (F-RC1-131) |
| I6 | `smoke/README.md:149-154` | *"pinned through a **council registry overlay** … `$HOME/.config/wicked-council/clis.toml` … `enabled_for_council = false`"* |
| I7 | `smoke/README.md:32` (S04), `:33` (S06), `:175`, `:93`, `:197` | The mixed-roster bench scenario, *"no council convenes"*, *"the ledger DOES bench a dead seat"*, the overlay edit, the bench-on-abstention record |
| I8 | `README.md:30, 319-327`; `docs/smoke-consumer-recipes.md:6-8, 160`; `smoke/lib/{shims,expect,env}.mjs`, `smoke/lib/steps/S04-bug-run.mjs:1-22`; `docs-lint/registry.json:46` | Supporting harness + recipe surfaces ⚠️ reported |

**This is the surface most likely to break on #591**: the smoke harness asserts a fixed roster shape, so instance identity changes the fixtures, not just the prose.

### 4.6 Claims with no "council" word — the sites' real exposure

A word-grep for `council` misses these entirely. Both are Tier **C** (#590 changes the trust-model wording) and both are ⚠️ reported, not re-derived.

| # | File:line | Claim |
|---|---|---|
| W1 | `wicked-web` `src/components/SameGarden.astro:70` | *"**Evaluator ≠ creator** — no agent grades its own homework. \"Done\" is re-derived from evidence, never asserted."* — **wicked-web is a shared library consumed by every product site**, so this renders on wg/wc/we/ws. One edit, four sites. |
| W2 | `wicked-web` `src/components/SameGarden.astro:73` | *"invokes skills as governed workers — **deny dominates**, every verdict lands in the record"* |
| W3 | `wickedagile` `src/components/Shipped.astro:70` | *"invokes skills as governed workers — **evaluator ≠ creator**"* — note the **drift**: the same riser says "deny dominates" in wicked-web and "evaluator ≠ creator" here. |
| W4 | `wickedagile` `src/components/Shipped.astro:83`, `src/scripts/data.js:49`, `src/scripts/terminal.js:69` | *"evaluator ≠ creator, \"done\" re-derived from evidence, the human in command"* — three copies of one sentence. |
| W5 | `wicked-estate` `docs/adr/ADR-012-rule-authorship.md:20-23` | *"The wicked platform's core invariant is **evaluator ≠ creator**"* |
| W6 | `wicked-core` `crates/wicked-governance/seed/corpus/plane-boundaries.md:37-39` | `PAT-1303` is a **seeded steering rule ingested into the estate graph**, so agents *recall* it: *"evaluator≠creator …, deny-dominates dual gates, and \"done\" re-derived from evidence"*. Changing doctrine without re-seeding leaves agents reciting the old rule. |

**All six survive #590 unchanged** — DES-TEAM-001 §2.5 and §5 keep `evaluator ≠ creator` explicitly out of scope. They are listed so a future editor does **not** sweep them in with the council rewrite. The wicked-web/wickedagile drift (W2 vs W3) is worth fixing on its own merits.

**Excluded from these sites — jam-council, do not touch:** `wickedagile` `src/components/Shipped.astro:85`, `src/scripts/data.js:45`, `src/scripts/terminal.js:71` (all three are the `wicked-garden` package entry: *"multi-model councils, graph-aware refactors, repo playbooks, the QE specialist fleet"*); `wicked-web` `SameGarden.astro:79, 84`; root `CLAUDE.md:18` (*"the multi-model council *is* the router"*).

### 4.7 Sites and the rest

| Repo | Council/seat surface | Note |
|---|---|---|
| wicked-estate | 2 files mention `council`, 0 mention `seat` (positive control: 540 files match `wicked`) | Not a council/seat surface. No action. |
| wicked-web | 1 file mentions `council`, 0 `seat` (control: 9 files match `wicked`, **4 of 4 `.astro` in range**) | Shared chrome only. Check the single hit when C5 lands. |
| wickedagile | 3 files mention `council`, 0 `seat` (control: 19 match `wicked`, 3 `.astro`) | Apex site. Re-read with C1/C5 — a family-level claim about councils would need the same treatment. |
| wicked-installer | 0 council, 0 seat (control: 34 files match `wicked`) | No action. |
| wicked-garden | 74 files mention `council` | **All excluded** — see §4.0. |

---

## 5. A placement decision this PR had to make, and is flagging rather than hiding

The brief specified `wicked-core/.product/` for these documents. That directory is **gitignored** and tracked in no commit (`.gitignore:19`), removed from the published tree two days ago by `8663d4b` (#562) whose message says the ignore entry exists *"so they are not re-added"*. wicked-core's own `CLAUDE.md:5-10` currently tells readers that every `.product/…` path is *"a pointer to a local-only artifact"* and *"tracked in no commit"*.

The three documents in this PR were therefore added with `git add -f`. They are the **only** tracked files under `.product/`; everything else there stays ignored.

**This is reversible and needs an explicit decision:**

- **Accept** → `wicked-core/CLAUDE.md:5-10` must be amended (some `.product/` files are now tracked), and root `CLAUDE.md:126` fixed per A7.
- **Reject** → move these three files to a published path (`docs/design/`) in a follow-up; nothing else changes.

---

## 6. Sequencing

1. **Now, one PR per owning repo:** A1–A4 (program docs + root `CLAUDE.md`), A7–A8 (`.product/` location). Independent of everything else.
2. **Now, wicked-studio:** A5 (re-scope the load clause of the e2e preflight). A6 optional.
3. **Now, wicked-crew:** retarget crew#662 per §3.2 — correct the premise, keep asks 3 and 4.
4. **Now, on their own merits** (verify each ⚠️ row first): A10–A20. A11/A12 are one small wicked-core doc-comment PR. A13 (garden's dead consensus gate) is the largest — it is a code decision, not a doc edit: delete the path or land the missing module.
5. **On #591 merge:** D1–D14, N3, N8, N11, S1–S2, I2. **`packages/crew-api-types` (D8) leads** — studio's copy (S4) and the smoke fixtures (I2) both key off it. N8 requires editing the generator **and** the generated `.d.ts`.
6. **On #590 merge:** C1–C9, N1, N4–N7, N9, I1/I3–I8. C1 and C4 must land together (near-duplicate article). N4 (`APPROVAL_THRESHOLD`) is the root of every "75%" downstream — change it first, then the sites.
7. **Verify before excluding:** `wicked-garden` `skills/workflow/SKILL.md:97` and `skills/archetype/refs/build.md:31` (§4.0) — ambiguous, leaning per-phase.
8. **Never touch:** the 11 clean jam-council files in wicked-garden `skills/`, `daemon/council.py`, the jam-council entries on wickedagile/wicked-web (§4.6), and root `CLAUDE.md:18` (§4.0).

## 7. Confidence

Rows marked ✅ were re-derived against `origin/main` in the course of writing this plan. Rows marked ⚠️ come from a dedicated inventory pass and carry a `file:line` but were **not** independently re-read here — verify each before editing it. One reported finding was checked and **rejected** (§2.3, the "6-seat vs 5-seat" contradiction), which is the reason for the distinction.

**Known gaps:** `.product/` is gitignored in wicked-crew, wicked-garden, wicked-studio, wicked-estate and wicked-interactive, so `DES-*` citations throughout those repos' source point at artifacts no clone can open — this catalogue could not read them. The `aee254f1` run-ledger figures underlying both design records were not re-derived (DES-TEAM-001 §6 OQ-TEAM-6). Studio's live-UI classifications come from reading render paths, not from running the app.
