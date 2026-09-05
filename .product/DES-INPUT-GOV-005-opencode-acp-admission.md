# DES-INPUT-GOV-005 — Resolve OQ-OPENCODE-ACP-001: opencode ACP input-governance admission

**Issue:** wicked-core #370
**Phase:** design (analysis + design + plan). **No production code in this design deliverable** —
the run's later implementation phase applies the accepted outcome as a comment-only registry.rs
update (no behavior change; `acp_input_governance` stays `false`). The config-provisioning route
described in §4 is a **separate, larger feature** this design files but does not build.
**Predecessors:**
- DES-INPUT-GOV-001 (recon, #360) — §OQ table defines OQ-OPENCODE-ACP-001 and its proof properties
  (blocking request / canonical name+rawInput / reject honoured / disableable auto-approve; plus
  "separately identify any bounded native sandbox control" and "do not admit on lifecycle/streaming
  evidence alone").
- DES-INPUT-GOV-002 (#364) — replaced the Claude-name admission predicate with a per-seat,
  evidence-gated `acp_input_governance` capability that defaults **OFF**. This design resolves one
  seat's gate; it changes no admission mechanism.
- DES-INPUT-GOV-003 (codex, #367) / DES-INPUT-GOV-004 (copilot, #369) — the precedent this mirrors:
  evidence + verdict + a registry comment update, `acp_input_governance` left `false` with no test
  change. DES-INPUT-GOV-004 §4.3 filed (did not resolve) **OQ-ACP-GOV-SCOPE-001**: whether
  `AcpConfig` should support scoped, per-tool-intent-kind admission instead of today's all-or-nothing
  bool. This design's §4.4 cross-references that question — opencode is a second, independent seat
  that would benefit from the same capability-shape evolution, for a different reason than copilot's.
- The clarify phase of this same run (#370, commit `76ac5c0`) — produced the evidence this design
  synthesizes.

**Carrier:** `src/acp_permission.rs` (`pretool_payload` at `:56`) is CLI-agnostic and unchanged by
this work. The decision is about *who is admitted to that carrier*, not the carrier.

**Evidence (the SUBJECT of this design, produced by the clarify-phase capture unit):**
`.product/evidence/oq-opencode-acp-001/` — `manifest.md`, `verdict.md`, `README.md` (redaction), eight
redacted `*.ndjson` captures, and four `*.mjs` harnesses. Raw captures stay in gitignored `tmp/`. All
committed evidence is REDACTED (no usernames/handles/emails/home paths).

**Operator amendment folded in (this phase):** approved the clarify phase's NOT ADMITTED verdict
under the current bar (default-silent + free-text identity), with the direction that the
config-driven finding — a project `opencode.json` fully gates every core intent and reject genuinely
works — is the batch's best forward path and must be recorded prominently as a **concrete admission
route**: the engine already provisions every governed worktree, so it could drop a permission-
tightening config at unit setup, making opencode gated **by construction** rather than by hoping the
CLI's shipped default changes upstream. Free-text tool identity remains the one blocker even under
that route. This design's §4 is that route, plus the operator-requested open question (how the
harness should provision the config, and how the identity gap gets closed), cross-referencing
OQ-ACP-GOV-SCOPE-001 under #360.

---

## 1. Decision

**opencode is NOT ADMITTED. `acp_input_governance` stays `false` for the built-in `opencode` seat.**

The decision requires **no field change** — the flag is already `false` (DES-INPUT-GOV-002 set it so
for every non-claude seat). The only production change — applied by the run's later implementation phase and included in this same PR — is the
**registry comment** on the `opencode` `AcpConfig`, updated to cite this verdict and the headline
finding, mirroring the OQ-CODEX-ACP-001 / OQ-COPILOT-ACP-001 → `registry.rs` updates.

### Headline (why this is neither a repeat of pi/codex nor of copilot)

opencode's registry invocation (`opencode acp`, no project config) is, in its **observed effect**,
the same as `pi`: every core in-workspace intent (read/edit/bash) proceeds with **zero**
`session/request_permission` calls, and a destructive `rm -rf` and a network `curl` both complete
unconditionally (`capture-allow.ndjson` ≡ `capture-reject.ndjson`; `probe-risky.ndjson`;
`probe-network.ndjson`). But the **cause** is entirely different, and matters for what happens next:
this is not absent wiring (`pi`) and not an internal reviewer routing around real plumbing (`codex`)
— it is a real, general, `Handler`-backed permission system (`packages/opencode/src/acp/
permission.ts`) whose *default data* (the `"build"` agent's baked `"*": "allow"` ruleset,
`packages/opencode/src/agent/agent.ts` at the pinned commit) happens to resolve every in-workspace
core intent to `allow` before that machinery ever fires. Feed that same machinery a stricter
`permission` config and it produces exactly the shape the OQ asks for: `capture-strict-allow.ndjson`
shows all four steps of the read/edit/bash/write turn genuinely gated (canonical `kind`, real
`rawInput`/`locations`/diffs), and `capture-strict-reject.ndjson` / `probe-strict-reject-bash.ndjson`
show a selected reject actually prevents both a read and an isolated destructive bash command. Only
`external_directory` (paths outside the working directory) gates by default, matching `codex`/
`copilot`'s out-of-workspace read boundary (`probe-outside-read.ndjson`).

This is the first of the four seats evaluated (`pi`, `codex`, `copilot`, `opencode`) where the gap
between "not admitted today" and "admitted" is a **configuration wicked itself could supply**, not an
upstream behavior change or a request to a third party. §4 is that route.

---

## 2. Candidate adapter (prerequisite check — passed to live-capture)

No separate adapter package exists to identify, unlike `codex-acp`/`pi-acp`. opencode speaks
**native** ACP: `opencode acp` is the same compiled binary as the headless `opencode run "..."`
invocation, started in JSON-RPC-over-stdio server mode instead of one-shot prompt mode.

- **Distribution:** opencode CLI, Homebrew tap `anomalyco/tap/opencode` (auto-updating; a newer
  `1.18.21` was already available at capture time — not an npm/Node bridge process at runtime).
- **Version actually invoked:** `1.17.18` (self-reported by `opencode --version`), sha256
  `652a34cab759c0fa348f107aa737df86355a49b1576834864e89ee43c059b25d`.
- **Source pin for the code citations in this design and in `verdict.md`:** tag `v1.17.18` → commit
  `b1fc8113948b518835c2a39ece49553cffe9b30c` on `github.com/anomalyco/opencode` (formerly
  `github.com/sst/opencode` — a repository rename/transfer, confirmed via the GitHub API, not two
  separate projects).

**Implementation language / architecture note**: like `copilot`, this is not a TypeScript/Node bridge
process wrapping a separate agent CLI in the sense `codex-acp`/`pi-acp` are. `opencode acp` is the
same compiled binary as `opencode run "..."`, started with a JSON-RPC-over-stdio server mode that
internally boots an embedded HTTP server and drives it via `@opencode-ai/sdk`. There is no separate
adapter package/version to pin independently of the CLI itself.

**Gap not closed by this evidence**: the Homebrew tap auto-updates and no lockfile pins the installed
build; a future upgrade could change the default agent's baked ruleset (the exact mechanism this
verdict turns on) without re-triggering this evidence — the same class of gap `oq-copilot-acp-001/
manifest.md` and `oq-codex-acp-001/manifest.md` recorded for their own distributions.

---

## 3. Proof summary

Full reasoning is in `evidence/oq-opencode-acp-001/verdict.md`; condensed here for the record. All
captures ran under client capabilities `{"fs":{},"terminal":false,"permission":true}` (the harness
explicitly declares it answers permission prompts) and byte-mirror `src/acp_runner.rs:1518-1523`.

| Property | Result (registry's actual invocation) | Result (config-tightened invocation) | Decisive capture(s) |
|---|---|---|---|
| (a) every core read/write/edit/bash intent produces a blocking `session/request_permission` with a canonical tool name + `rawInput` compatible with `pretool_payload` | **FAIL** — 0/4 steps gated; `rm -rf` and `curl` both proceed unconditionally. `external_directory` (outside-workspace) IS gated. Identity is free-text `toolCall.title` (bare path / literal shell command), never a top-level `toolName` or `toolCall.name` | **PASS on gating, same identity gap** — 4/4 steps gated with real `rawInput`/`locations`/diffs; identity still `toolCall.title` | default: `capture-allow.ndjson`, `capture-reject.ndjson`, `probe-risky.ndjson`, `probe-network.ndjson`, `probe-outside-read.ndjson`. tightened: `capture-strict-allow.ndjson` |
| (b) the auto-approve/default control can be disabled to yield the required property | **INVERTED SHAPE** — there is no flag to disable; the default is already allow-everything with no flag involved (`--auto` exists but is not wired into the `acp` subcommand at all). The actual control is the `permission` config object, which the registry's invocation does not supply | **PASS** — a project `opencode.json` `permission` field, merged after (and overriding) the agent's hardcoded default, provably flips every core intent to `ask` | `capture-strict-allow.ndjson`; source trace in `verdict.md` §(b) |
| (c) a selected reject prevents the action | **N/A / vacuously true** — reject and allow are behaviorally identical because nothing is ever asked | **PASS** — reject genuinely blocks both a read (`capture-strict-reject.ndjson`) and an isolated destructive bash command (`probe-strict-reject-bash.ndjson`, `subDirStillExists: true`) | `capture-strict-reject.ndjson`, `probe-strict-reject-bash.ndjson` |

**Overall for (a)/(b)/(c) at the registry's actual invocation:** fails outright — same practical
outcome as `pi`, different cause. **Overall under a wicked-supplied `permission` config:** clears (a)
for gating and (c) fully; the canonical-identity half of (a) does not improve (same gap `codex`/
`copilot` share) and is not something a permission config can fix — it needs a `pretool_payload`-side
or pre-`pretool_payload` normalization change (§4.4, §6 below).

**Scope of this verdict:** exactly `opencode` CLI `1.17.18` (sha256 `652a34ca...`), invoked as
`opencode acp` with no extra flags — the registry's exact `start_args`. The distribution auto-updates
with no lockfile pin, so a version bump is **not** covered by this evidence — re-verify before ever
flipping the flag, and before relying on the config-tightened result surviving an upgrade. Triggers
that would demand a fresh capture: any change to the `"build"` agent's default ruleset, any change to
how project/global `permission` config merges, or a change that makes `session/request_permission`
carry a canonical name.

**No `ASSUMPTION[external-transform]` applies** — same reasoning as DES-INPUT-GOV-002 §8,
DES-INPUT-GOV-003 §3, and DES-INPUT-GOV-004 §3: the ACP `session/request_permission` shape is an
in-repo-normalized protocol carrier, and opencode's ACP mode is native (no bridge translating a
second protocol), so there is no third-party payload-transforming library or service in this picture.

---

## 4. The concrete admission route: engine-provisioned permission config

This section is the operator-requested, prominent record of the forward path — not a decision to
build it now.

### 4.1 What the evidence already proves is possible

`capture-strict-allow.ndjson` / `capture-strict-reject.ndjson` / `probe-strict-reject-bash.ndjson`
prove that a single, small, wicked-authored artifact —
```json
{ "$schema": "https://opencode.ai/config.json", "permission": { "read": "ask", "edit": "ask", "bash": "ask" } }
```
— placed where opencode's config loader will find it, makes every core tool intent this OQ tests
genuinely gated, with reject genuinely honoured. Unlike `pi` (no permission path exists to configure)
and `codex` (the internal reviewer sits in front of the real permission machinery and is not
config-addressable the way this is), opencode's gap is closeable **today, without waiting on or
requesting an upstream change** — the engine can make opencode gated **by construction** for every
governed unit, rather than admission depending on what the CLI ships as its out-of-the-box default.

### 4.2 Where such a file would actually go — the operator's proposed mechanism vs. the codebase's existing precedent

The operator's framing ("drop a project opencode.json into the worktree at unit setup") assumes the
engine already has a "seed files into a fresh worktree" step. It does not: `create_worktree`
(`src/repo.rs:544`) is a bare `git worktree add` with no file-seeding logic at all. The engine's
**actual** established precedent for "write a per-CLI config the engine owns and re-sanitizes every
spawn" is `ensure_worker_config_home`/`worker_config_home` (`src/acp_runner.rs:1147-1244`) — but that
mechanism is **claude-specific today**: a persistent, engine-owned directory *outside* the worktree
(`~/.wicked-worker/claude`), where `settings.json` is overwritten with a deny fence on every spawn,
executable-config vectors are stripped, and login/session state is deliberately preserved.

Two candidate mechanisms follow from these two different precedents, and this design does not choose
between them:

- **(A) Worktree-root file drop** (the operator's literal suggestion): write `<worktree>/opencode.json`
  at unit setup, before the ACP spawn. Simple — no new generic per-CLI config-home plumbing needed.
  Two risks this design flags rather than resolves: (i) the file lives **inside** the governed unit's
  own read/write boundary — unlike claude's `settings.json`, which lives in a config home *outside*
  the worktree specifically so the worker cannot edit the file that constrains it, a worktree-root
  `opencode.json` is something the opencode worker itself could in principle edit or delete, though
  whether that has any effect depends on whether opencode's permission ruleset is resolved once at
  server boot (this evidence did not test a mid-session edit, so it is unverified either way); (ii) a
  target repository that already ships its **own** `opencode.json` — for its own contributors' use of
  opencode, unrelated to wicked — would collide with an engine-authored one; this needs a defined
  precedence/merge rule (overwrite vs. merge vs. refuse-and-fall-back-to-ungoverned), not silent
  clobbering of a repo's own file.
- **(B) Engine-owned config home, mirroring `worker_config_home`**: extend the existing claude-only
  pattern to be per-CLI, and point opencode's config resolution at an engine-owned directory outside
  the worktree (via `OPENCODE_CONFIG_DIR` or `XDG_CONFIG_HOME`, both of which `packages/core/src/
  global.ts`'s `xdg-basedir`-based resolution already honors at the pinned commit) — re-sanitized
  every spawn like claude's `settings.json`, keeping the enforcement file outside the boundary the
  governed worker can write to. This is more consistent with the codebase's existing security posture
  for exactly this class of problem, but requires generalizing a mechanism that is currently written
  assuming exactly one CLI (claude) exists, and it raises its own open question: does opencode's
  `Permission.merge` order let a **project**-level `opencode.json` (an unrelated file a target repo
  might ship) *override* a global-config-home injection, defeating it? This evidence did not test
  config-precedence when both a global and a conflicting project config exist — a real gap, not a
  hypothetical, since the whole point of option (B) is surviving a repo that ships its own config.

### 4.3 Operator question this design files (does not resolve)

> **OQ-OPENCODE-ACP-PROVISION-001.** Should wicked-core provision an opencode `permission`-tightening
> config for every governed unit that seats `opencode`, and if so: (1) via a worktree-root file drop
> (mechanism A, §4.2) or a generalized per-CLI config-home mechanism modeled on claude's
> `worker_config_home` (mechanism B, §4.2)? (2) How does the chosen mechanism behave when the target
> repository already ships its own `opencode.json`/`~/.config/opencode/opencode.jsonc` content —
> overwrite, merge, or refuse-and-stay-ungoverned? (3) Does opencode resolve its permission ruleset
> once per server-process-boot or re-read it live, i.e. can a governed opencode worker itself
> undermine the config mid-session by editing it (relevant only to mechanism A, since mechanism B
> keeps the file outside the worker's boundary)? None of these are answerable from this OQ's evidence
> alone — they need either a follow-up capture (mid-session edit + config-precedence probes) or an
> engine-architecture decision (which team owns worktree/unit-setup provisioning). This question is
> independent of, but related to, **OQ-ACP-GOV-SCOPE-001** (DES-INPUT-GOV-004 §4.3, filed under #360):
> that question asks whether `AcpConfig.acp_input_governance` should become a *scoped* per-intent-kind
> capability instead of a single bool; this question asks whether the engine should *actively author*
> the config that makes a seat's ruleset match what a scoped or unscoped admission would require in
> the first place. A resolution to OQ-ACP-GOV-SCOPE-001 does not answer this question and vice versa,
> but whoever eventually flips `opencode`'s `acp_input_governance` will need both answered — the scope
> question decides *what bar* opencode has to clear (all intents, or just edit+execute the way
> copilot's near-pass suggests might be acceptable), and this question decides *how opencode is made
> to clear it* rather than relying on an upstream default the engine does not control.

### 4.4 The free-text identity gap is orthogonal to §4.1–4.3 and blocks admission even if provisioning ships

Even a fully-provisioned, fully-gated opencode seat (mechanism A or B, working config precedence)
would still carry the same canonical-identity gap `codex` and `copilot` have: every
`session/request_permission` observed here carries **no top-level `toolName`** and **no
`toolCall.name`**, only `toolCall.title` — a bare absolute file path for edit/write, the literal shell
command for bash, and (coincidentally, not by contract) the bare string `"read"` for reads asked
before their input metadata attaches (`verdict.md`, Property (a) "Identity/rawInput compatibility").
`pretool_payload` (`src/acp_permission.rs:56`) would resolve `tool_name` to one of these free-text
values — a workable basis for a pure allow/deny-everything gate, but a degraded one for any policy
keyed on canonical tool identity (`"deny Bash"`, `"require review on Write"`). `toolCall.kind` is a
small, stable, already-present enum (`read`/`edit`/`execute`/`other`) that `pretool_payload` does not
consult today. Closing this — a wicked-side normalization keyed on `kind` ahead of `pretool_payload`,
for any adapter whose request lacks a canonical name — is the same fix `oq-codex-acp-001/verdict.md`
and `oq-copilot-acp-001/verdict.md` both proposed independently for their own seats; it would benefit
all three simultaneously rather than being opencode-specific, and is out of scope for a single-seat
OQ resolution (§6 records it as a candidate follow-up, not a decision).

---

## 5. Implementation-phase plan (the ONLY production change)

Scope is deliberately minimal and mirrors the codex/pi/copilot precedent. **Order matters** (per the
standing operator amendment — commit before gates):

1. **Commit EVERYTHING first** — this design doc plus (already committed by the clarify phase)
   `.product/evidence/oq-opencode-acp-001/` — **before** running gates, so the gates run against the
   committed tree. Keep redaction rules intact (no usernames/handles/emails/home paths in committed
   files; raw captures remain only under gitignored `tmp/`).
2. **Edit exactly one site (implementation phase, not this one):** the `opencode` `AcpConfig` block in
   `crates/wicked-council/src/registry.rs` (currently `registry.rs:330` "opencode speaks NATIVE ACP
   over stdio (\`opencode acp\`) — no bridge needed." and `registry.rs:336` "OQ-OPENCODE-ACP-001 must
   prove permission coverage before admission."). Replace with a comment summarizing this verdict —
   the default-allow ruleset finding, the config-tightened proof, and a pointer to §4's admission
   route and OQ-OPENCODE-ACP-PROVISION-001 — mirroring the comment style already present for `codex`
   (`registry.rs:207-231`), `pi` (`registry.rs:251-266`), and `copilot` (`registry.rs:286-309`). **Do
   not** touch the `acp_input_governance: false` line — it is already correct. **Do not** touch any
   other seat, field, or test. **Do not** implement mechanism A or B from §4 — that is a distinct,
   larger feature (worktree/unit-setup provisioning, or a generalized config-home mechanism) requiring
   its own design phase once OQ-OPENCODE-ACP-PROVISION-001 is answered.
3. **Run gates** (§7).

### 5.1 No test change

The built-in-roster assertion in `registry.rs` (only `claude` carries `acp_input_governance = true`;
`agy`/`codex`/`pi`/`copilot`/`opencode` are unadmitted) already asserts the correct state and passes
unmodified. This design adds no test because it changes no behaviour — only a comment.

---

## 6. Forward path (not in scope, recorded for the next attempt)

Three independent, non-mutually-exclusive prerequisites, in the order a follow-up would most
naturally tackle them:

1. **Answer OQ-OPENCODE-ACP-PROVISION-001** (§4.3) — decide whether wicked provisions the permission
   config at all, and if so, which mechanism (A or B) and its collision/precedence/live-reload
   semantics. This is an engine-architecture decision, not a re-capture.
2. **Re-capture against the exact provisioning the engine would ship**, once (1) is answered —
   including the risky/network probes this evidence ran only against the *default* invocation
   (`probe-risky.ndjson`/`probe-network.ndjson` should be re-run against the chosen provisioning to
   confirm destructive/network bash-class calls are gated there too, not just the plain
   `echo`/isolated `rm -rf sub` cases §3 already proves).
3. **Close the canonical-identity gap** (§4.4) — a `toolCall.kind`-keyed normalization ahead of
   `pretool_payload`, shared with `codex`/`copilot`, independent of (1) and (2).

Only when all three land — or a scoped-admission capability ships (OQ-ACP-GOV-SCOPE-001) and is
deliberately applied to `opencode` for just the intents provisioning can guarantee — is flipping
`acp_input_governance` justified. Unlike `pi`/`codex`, none of these three depend on an upstream
change wicked does not control; unlike `copilot`, none of them depend on an unresolved question about
what "every tool intent" should mean for reads specifically. opencode's path to admission is the
straightest of the four seats evaluated so far, and also the one requiring the most new
engine-side work before it can be walked.

---

## 7. Gates (run after committing evidence + design doc)

- `cargo fmt --all -- --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test -p wicked-council`
- `cargo test --lib`

**Known-benign failure to expect:** `builtin_floors::tests::floor_fails_closed_outside_a_git_repo` in
`cargo test --lib` fails in this sandbox because `TMPDIR` is set inside the worktree, violating that
test's own "outside any repo" premise. It is a pre-existing environment artifact, unrelated to this
evidence/design work (the clarify phase already confirmed `wicked-council` is 88/88 and `--lib` is
666/667 with only this one env failure, reproduced in isolation on the same commit; DES-INPUT-GOV-004
§6 recorded the identical failure for the copilot run). Do not "fix" it as part of #370.
