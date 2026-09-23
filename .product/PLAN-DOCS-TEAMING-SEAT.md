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

Three different things share the word **council**. Only the first is touched by #590.

| Sense | Where | Touched by #590? |
|---|---|---|
| **The per-phase council** — N seats ballot per workflow phase, 75% agreement, seats bench, produces routing + a verdict | wicked-core `crates/wicked-council/`, `src/distribute.rs`; wicked-crew | **YES** |
| **wicked-garden's `jam-council`** — a multi-model brainstorming / second-opinion panel a user invokes on request | wicked-garden `skills/jam-council/SKILL.md` and 13 sibling skill files | **NO — product feature, stays** |
| **wicked-garden's daemon council** — a synchronous "POST a question, get votes + synthesis" API backing the skill above | wicked-garden `daemon/council.py` (*"Council orchestrator for the wicked-garden daemon … the caller POSTs a question, gets back votes + synthesis"*, `council.py:1-8`), plus its `council_sessions` table | **NO — garden's own, stays** |

**Verified exclusion list — 14 wicked-garden `skills/` files mention "council" and none is the per-phase council** (none references 75% agreement, ballots, benching or `RoutingInfo`): `skills/archetype/refs/{build,decide,review}.md`, `skills/classify/SKILL.md`, `skills/core/SKILL.md`, `skills/jam-brainstorm-facilitator/SKILL.md`, `skills/jam-council/SKILL.md`, `skills/jam/SKILL.md`, `skills/jam/refs/council-verdict.md`, `skills/qe/SKILL.md`, `skills/swarm/SKILL.md`, `skills/swarm/refs/{independent-verification,ship-discipline}.md`, `skills/workflow/SKILL.md`. Garden has 74 files mentioning "council" in total; **none of them is in scope for #590.** Editing them would rewrite a product feature for a change that does not touch it.

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
| D9 | `packages/crew/src/api/{seat-health,seat-signin,seat-standing,roster-standing}.ts` doc comments | Seat identity as a CLI key | Instance keying; and S5's per-instance login (wicked-crew#615). |

### 4.3 wicked-studio

| # | File:line | Claim | Tier |
|---|---|---|---|
| S1 | `site/src/pages/index.astro:378-381` | *"Put one question to several models in the same thread and see **how many seats answered** — '3 of 5 seats', or '3 polled' when the count is unknown"* | **D** (#591) — "5 seats" becomes instances, not vendors. Studio has exactly **one** `.astro` file and it is in scope (positive control: 1 of 1 matched `wicked`). |
| S2 | `site/src/pages/index.astro:68, 313, 895` | *"Group chat with your whole roster: fan one question out, watch each seat answer side by side"*; *"roster fan-out … three seats answered"* | **D** (#591). Note this is the **chat roster**, not the per-phase council — #590 does not touch it. |
| S3 | `e2e/test_feature_live.py:24-27, 849` | The load-preflight gate | **A** — see A5. |
| S4 | Studio's seat/council UI components (82 files mention `council`, 143 mention `seat` on `origin/main`) | Not individually catalogued here | **C/D**, but driven by the wire contract (D8) rather than by prose. Enumerate against the api-types diff when D8 lands, not before. |

### 4.4 wicked-core (this repo)

| # | File:line | Claim | Tier |
|---|---|---|---|
| N1 | `ORCHESTRATOR.md` (12 `council` mentions), `HANDOFF.md` (6), `DESIGN.md` (3), `REASSESS-P0-P1.md` (3), `README.md` (1) | Per-phase council as the routing mechanism | **C** (#590) — re-read each when S5 lands; several are historical revision logs that should be left as history, not rewritten. |
| N2 | `crates/wicked-apps-core/src/spawn.rs:386-389` | Names the pi variable correctly as `PI_CODING_AGENT_DIR` | **No change.** Recorded because #591's own table spells it `PI_AGENT_DIR`; the *issue* is wrong, not the code. Fix the issue text. |
| N3 | `crates/wicked-apps-core/src/spawn.rs:292-295` | *"the ACP spawn, the ballot spawn, the wrapped worker and the sign-in command all name, and run under, the same directory"* | **D** (#591) — this invariant is exactly what instance keying must preserve across **both** config-home resolvers (DES-SEAT-001 §3.2b). Update with S2. |

### 4.5 Sites and the rest

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
2. **Now, wicked-studio:** A5 (re-scope the load clause of the e2e preflight). A6 is optional.
3. **Now, wicked-crew:** retarget crew#662 per §3.2 — correct the premise, keep asks 3 and 4.
4. **On #591 merge:** D1–D9, N3, S1–S2. `packages/crew-api-types` (D8) leads; studio (S4) follows the published contract.
5. **On #590 merge:** C1–C9, N1. C4 and C1 must land together (duplicate article).
6. **Never:** the 14 wicked-garden `skills/` council files and `daemon/council.py` (§4.0).
