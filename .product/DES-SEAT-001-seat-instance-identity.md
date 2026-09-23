# DES-SEAT-001 — Seat-instance identity separated from model identity: a per-instance config home and credential

- **Status:** PROPOSED — **NOT BUILT.** No seam below is implemented. This document records an intended design and the evidence behind it; it does not describe engine behaviour. Where a mechanism already exists it is cited as an existing seam the design would *extend*, not as this design shipping. S1 and S2 are the subject of in-flight implementation work at the time of writing; S3, S4 and S5 are not started.
- **Date:** 2026-09-22
- **Scope (intended):** wicked-core `src/distribute.rs` (identity + routing), `crates/wicked-apps-core/src/spawn.rs` (config home), `crates/wicked-council/` (roster, `login_invocation`); wicked-crew (wire attribution, seat login route); wicked-studio (seat provisioning UI)
- **Source:** wicked-core#591 (filed 2026-09-22 by the operator, during a governed run whose council burned four convenings on a quota-exhausted seat)
- **Related:** DES-TEAM-001 (real-time teaming — this design is what makes its monitor fleet affordable), core#578 (agy `HOME` isolation, generalised here), core#585 (agy credential path, becomes per-instance), core#278 (`login_invocation` / PTY-hosted sign-in), wicked-crew#615 (daemon-driven seat login/logout — the natural home for S5), core#461 (`distinctness_fallback` disclosure), DES-INPUT-GOV-008 (the OS sandbox floor a pooled seat still runs under)

---

## 1. Problem

A run's worker pool holds **one seat per CLI**. Parallelism is therefore capped by how many distinct vendors are signed in, not by what the host can run — and a degraded seat cannot be replaced by another instance of a healthy one.

The cap bites hardest exactly when the roster is degraded. On run `aee254f1` (2026-09-22) `copilot` was `quota_exhausted`, was convened on all four councils, and produced 12 seat failures — while a healthy signed-in `claude` sat idle between units. Three `claude` workers would have been strictly better than one `claude` plus one dead seat. (As in DES-TEAM-001 §1.1, these are the operator's run-ledger figures as filed in #591; they were **not** re-derived for this document.)

---

## 2. Why it does not work today — two identity collisions

Both are verified at `origin/main`.

### 2.1 `builder_clis` is a set of CLI keys

```rust
let builder_clis: std::collections::HashSet<String> = units
```
— `src/distribute.rs:829`, built from `assigned_cli`.

Two `claude` instances collapse to one entry, so `enforce_evaluator_distinct` (`src/distribute.rs:820`) treats them as the **same seat**. The consequences are visible in the same function: the "every roster key is a builder" warning (`warns_about_missing_evaluator_seat`, `src/distribute.rs:892-894`, `roster_keys.len() >= 2 && roster_keys.iter().all(|k| builder_clis.contains(k))`) fires, the reseat candidate search `.find(|k| !builder_clis.contains(*k) && admits(k))` (`src/distribute.rs:870`) finds nothing, and the unit falls back to the creator seat with the `distinctness_fallback: "creator_seat"` disclosure (`src/distribute.rs:819`, `:875`).

### 2.2 `invocation_of` resolves by key, first match wins

```rust
pub(crate) fn invocation_of(clis: &[AgenticCli], key: &str) -> Option<String> {
```
— `src/distribute.rs:176`, resolving with `.find(|c| c.key == key)`. A duplicate key is silently ambiguous.

**So this is a genuine identity change, not "put the CLI in the roster twice."** Both collisions must be fixed before a pool of same-CLI instances means anything.

---

## 3. The isolation half — mostly built, but not the one-liner #591 describes

### 3.1 What exists

`seat_config_for` (`crates/wicked-apps-core/src/spawn.rs:1228`) resolves a per-seat root and points each CLI's configuration home at it:

```rust
let root = worker_home_base()?.join(name);
refuse_symlinked_home(&root)?;
```
— `spawn.rs:1240-1241`, inside `pub fn seat_config_for(cli: SeatCli)` (`spawn.rs:1228`), with the per-CLI mapping at `spawn.rs:1243-1260`:

| Seat | Variable set | Const |
|---|---|---|
| Claude | `CLAUDE_CONFIG_DIR` | `spawn.rs:102` |
| Codex | `CODEX_HOME` | `spawn.rs:385` |
| Pi | **`PI_CODING_AGENT_DIR`** | `spawn.rs:389` |
| Copilot | `COPILOT_HOME` | `spawn.rs:394` |
| Opencode | `XDG_CONFIG_HOME` / `XDG_DATA_HOME` / `XDG_STATE_HOME` under `root` | `spawn.rs:402`, `:404`, `:406` |
| agy | `HOME` (core#578) | `spawn.rs:445` |

> **Correction to #591.** The issue's table spells pi's variable `PI_AGENT_DIR`. The Rust **constant** is `PI_AGENT_DIR_ENV`, but its **value** — the variable actually set on the child — is `PI_CODING_AGENT_DIR` (`spawn.rs:389`), verified against the installed `pi-coding-agent` bundle per the doc comment at `spawn.rs:386-388`. Any doc repeating `PI_AGENT_DIR` as an environment variable is wrong.

Every seat variable a seat does **not** set is stripped (`let strip: Vec<&'static str> = SEAT_CONFIG_ENV…`, `spawn.rs:1272`), and `refuse_symlinked_home` (`spawn.rs:224`) plus the `hardened()` ordering contract keep working regardless of what `name` is.

### 3.2 Why "`name` becomes an instance name" is not a one-line change

#591 states: *"If `name` becomes an instance name rather than a CLI key, every seat gets its own config home for free."* The mechanism is right; the cost is understated, for two reasons.

**(a) `name` is a `&'static str` from a per-CLI enum.**

```rust
pub fn root_name(self) -> Option<&'static str> {
    match self {
        SeatCli::Claude => Some("claude"),
        …
```
— `spawn.rs:1038-1048`, consumed at `spawn.rs:1232` (`let Some(name) = cli.root_name() else { … }`).

`&'static str` cannot express `claude-2`. Instance keying requires threading an instance discriminator through `seat_config_for(cli: SeatCli)` (`spawn.rs:1228`) and changing or supplementing `root_name`'s return type. That is a signature change across every caller, not a substitution at `spawn.rs:1240`.

**(b) There is a second, independent resolver that hard-codes `"claude"`.**

```rust
pub fn worker_claude_config_dir() -> anyhow::Result<std::path::PathBuf> {
    let dir = worker_home_base()?.join("claude");
```
— `spawn.rs:297-299`.

Its own doc comment names the callers this must stay consistent with: *"the ACP spawn, the ballot spawn, the wrapped worker and the sign-in command all name, and run under, the same directory"* (`spawn.rs:293-294`), and the roster's claude `login_invocation` is listed among them (`spawn.rs:286`). **If instance identity keys only `seat_config_for`, then ballots and sign-in for `claude#2` would run under `<base>/claude` while its units run under `<base>/claude-2`** — the seat would report signed-in from one home and work out of another. S2 must cover both resolvers or state the divergence.

---

## 4. Per-instance auth — true for four seats, false for one

#591's claim is: *"two config homes are two credential slots, so two instances can hold two different accounts."* That holds where the credential is a **file under the configuration home**, and the codebase records exactly where it is not.

| Seat | Credential location | Two homes ⇒ two credentials? | Source |
|---|---|---|---|
| Codex | `auth.json` under `CODEX_HOME` — *"relocating it relocates the login too"* | ✅ | `spawn.rs:383-385` |
| Pi | settings, skills, extensions, prompts, sessions **and** `auth.json` under the agent dir | ✅ | `spawn.rs:386-389` |
| Opencode | credential store at `$XDG_DATA_HOME/opencode/auth.json` | ✅ | `spawn.rs:395-397` |
| agy | `~/.gemini/antigravity-cli/antigravity-oauth-token` resolved under `HOME` — *"moving the home is the whole isolation"* | ✅ on POSIX; ❌ on Windows | `spawn.rs:425-428`, residual at `:441-444` |
| **Copilot** | OAuth token in the **OS keychain, which is per-USER** — *"a copilot seat root isolates the configuration, not the keychain entry (documented limitation)"* | ❌ | `spawn.rs:390-393` |
| Claude | keychain service keyed by the config dir — see caveat below | ⚠️ asserted, not verified here | — |

**Two corrections to #591's premise:**

1. **Copilot does not get per-instance credentials from this mechanism.** `spawn.rs:391-393` states it plainly. Since the motivating incident on `aee254f1` was a **`copilot` `quota_exhausted` seat** (§1), the seat that provoked the design is the one seat for which "quota stops being a hard cap" does **not** follow from config-home isolation alone. Copilot needs a different provisioning story (a second OS user, or a credential surface that is not the keychain). This must not be glossed.
2. **agy on Windows.** `HOME` is not moved there — *"On Windows the home is `USERPROFILE`, which a seat decision does not move … so an agy seat there still resolves `~/.gemini` under the operator's profile — the same shape of documented limitation as copilot's per-USER keychain entry"* (`spawn.rs:441-444`).

**Claude's keychain caveat.** #591 asserts the Claude keychain service is literally `Claude Code-credentials-<sha256(config_dir)[:8]>`. That string does not appear anywhere in this repository (`git grep -i 'keychain' origin/main -- crates/ src/` returns only the copilot and agy limitation notes above, the `osxkeychain` git-credential-helper entries in `src/remote_write_fence.rs`, and test fixtures). It is an operator-observed behaviour of a third-party binary, not an engine invariant. **It should be re-verified against the installed `claude` binary before it is load-bearing for S5**, in the same way `PI_CODING_AGENT_DIR` was verified against the installed pi bundle (`spawn.rs:386-388`).

### 4.1 What per-instance credentials would unlock (intended)

- **Quota stops being a hard cap** — for the four file-credential seats. A vendor limit becomes a provisioning question rather than a dead seat.
- **Different model per instance** — a capable model for the creator, a cheap one for a fleet of monitors. This is what makes DES-TEAM-001's monitor pool affordable (its R-5).
- **Different posture per instance** — one instance read-only for monitoring, one writable for authoring, enforced by configuration rather than convention.
- **Per-instance credential tier** — an Antigravity instance pinned to Vertex versus one on an API key; the distinction core#585 needs, decided per instance.

---

## 5. Design

1. **Instance id alongside `cli`.** A seat gains an instance identity. Routing, `assigned_cli` and the wire carry the **instance**; predicates that care about the **model** read `cli`. Both collisions in §2 are fixed against the instance, not the key.
2. **Config home keyed on the instance id** (§3), covering `seat_config_for` **and** `worker_claude_config_dir` (§3.2b), with `refuse_symlinked_home` (`spawn.rs:224`) and the `hardened()` ordering contract intact.
3. **Pool size as a policy input** per phase / work kind.

### 5.1 Governance — is `claude#2` distinct from `claude#1`?

**Partly, and the difference must be disclosed rather than assumed away.**

- It **does** remove context contamination — the evaluator holds none of the creator's authoring reasoning.
- It **does not** remove model-level blind spots — same weights, same failure modes.

#591 records the evidence from the same run: creator `claude`, evaluator `opencode` — genuinely different models — and the evaluator still returned `agentVerdict: "skipped"`. The five defects, including a HIGH data-loss race, were caught afterwards by `agy`, a third model reading the finished diff. **Model diversity earned its keep at the reviewer, not at the in-run evaluator.**

Therefore, as design invariants:

| # | Invariant |
|---|---|
| I-1 | Pool freely for throughput on neutral / creator / monitor work — instance distinctness is sufficient there. |
| I-2 | Where distinctness is load-bearing (the **evaluator**), prefer a model-distinct seat; allow an instance-distinct one only **with disclosure**. |
| I-3 | An instance-distinct evaluator is disclosed through the existing mechanism: `distinctness_fallback: Option<String>` (`src/distribute.rs:169`) gains a second value beside `DISTINCTNESS_FALLBACK_CREATOR_SEAT` (`src/distribute.rs:173`). The field and its wire disclosure already exist (core#461, `src/distribute.rs:148`); this is one more value in it, not a new mechanism. |
| I-4 | Silently accepting an instance-distinct evaluator as though it were model-distinct is forbidden — that is the same silent-degradation shape this program keeps removing. |
| I-5 | The fail-closed ballot-bench path must stay fail-closed: `src/distribute.rs:3526` pins *"no silent creator_seat fallback"*. Pooling must not turn a benched seat into a silent same-model reseat. |

### 5.2 Seams

- **S1 — instance identity in the roster + routing**; fix both collisions in `src/distribute.rs` (§2.1, §2.2). **In flight at time of writing.**
- **S2 — config home keyed on the instance id** (`crates/wicked-apps-core/src/spawn.rs`), covering both resolvers (§3.2), with the existing symlink refusal and `hardened()` intact. **In flight at time of writing.**
- **S3 — `distinctnessFallback: "same_cli_instance"`** + the disclosure on the wire (I-3). Not started.
- **S4 — pool policy:** what sizes the fleet per phase / work kind. Not started.
- **S5 — per-instance login/provisioning.** `login_invocation` exists for PTY-hosted sign-in (`crates/wicked-council/src/types.rs:273`, defaults at `:341` `default_login_invocation`, with every built-in seat pinned to have one by `types.rs:1362`), and wicked-crew#615 already asks for a daemon-driven login/logout route with no terminal; per-instance login belongs there. Not started.

### 5.3 Risks

| # | Risk | Note |
|---|---|---|
| R-1 | **Host saturation.** Pool size must be bounded by real capacity — a per-phase council is already the spikiest thing the platform does. Note this is *not* an argument for the retired load-~10 rule: `ballot_load_factor` (`crates/wicked-council/src/dispatch.rs:205`) already scales the ballot budget by load, capped 5× (`:168`). See DES-TEAM-001 §7. |
| R-2 | **Quota burn.** N instances of one CLI consume that vendor's quota N times faster unless they carry different credentials — and §4 shows copilot cannot carry different credentials by this mechanism at all. The feature only helps where instances are provisioned distinctly. |
| R-3 | **Attribution.** Events, `cliUsage` and the ledger key on `assigned_cli`; they need the instance id **without** losing the ability to aggregate by model. This is the reason §5.1's split (wire carries instance, model predicates read `cli`) is stated as a rule and not left to each call site. |
| R-4 | **Login/provisioning UX** — S5; the part with no existing home. |
| R-5 | **Divergent homes** between units, ballots and sign-in if §3.2b is not addressed. |

---

## 6. Acceptance (intended — nothing here has been run)

1. Two `claude` instances in one roster are distinct to `enforce_evaluator_distinct`: `builder_clis` (`src/distribute.rs:829`) does not collapse them, and the evaluator seats on the second instance rather than taking the `creator_seat` fallback.
2. `invocation_of` (`src/distribute.rs:176`) resolves each instance unambiguously; no first-match-wins ambiguity for a duplicate CLI key.
3. `claude#1` and `claude#2` run under distinct config homes, and **both** `seat_config_for` and `worker_claude_config_dir` agree on each instance's home (§3.2b) — verified by a ballot and a unit for the same instance naming the same directory.
4. `refuse_symlinked_home` still refuses a planted link at any component of an instance root, and every non-owned seat variable is still stripped (`spawn.rs:1272`).
5. An instance-distinct evaluator is **disclosed** on the wire as `distinctnessFallback: "same_cli_instance"`; a model-distinct one carries no fallback (I-2, I-3).
6. A benched seat still fails closed — `src/distribute.rs:3526`'s "no silent creator_seat fallback" test still passes (I-5).
7. **Documented limitations stated, not silently inherited:** a pooled `copilot` is disclosed as sharing one per-USER keychain credential (§4), and an agy pool on Windows is disclosed as sharing the operator profile.
8. Attribution: per-instance `cliUsage` and ledger rows still aggregate correctly by model (R-3).

---

## 7. Open questions

- **OQ-SEAT-1.** What is the instance id's spelling on the wire (`claude#2`, `claude-2`, a separate `instanceId` field beside `assignedCli`)? R-3 makes this a compatibility decision, not a cosmetic one.
- **OQ-SEAT-2.** Does `root_name` gain a dynamic form, or does `seat_config_for` take an explicit instance suffix and leave `root_name` as the CLI default (§3.2a)?
- **OQ-SEAT-3.** Is the claude keychain-service-keyed-by-config-dir behaviour (§4) real on the installed binary? Unverified in this repository; S5 depends on it.
- **OQ-SEAT-4.** What is copilot's provisioning story given the per-USER keychain (§4)? Without one, `copilot` — the seat that motivated #591 — cannot be pooled with distinct credentials.
- **OQ-SEAT-5.** How does pool size interact with the OS sandbox floor (DES-INPUT-GOV-008 Boundary 1) — one sandbox per instance, or one per unit?
- **OQ-SEAT-6.** Does a monitor instance (DES-TEAM-001 S2) need a roster seat at all, or is it a lighter-weight spawn outside the council roster?
- **OQ-SEAT-7.** As with DES-TEAM-001 §6 OQ-TEAM-6, the `aee254f1` figures in §1 and §5.1 are the operator's ledger readings and were not re-derived here.
