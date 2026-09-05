# DES-INPUT-GOV-006 — Provision opencode ACP into governance, resolve OQ-OPENCODE-ACP-PROVISION-001

**Umbrella:** wicked-core #360. **Phase:** clarify (this design) → **build (applied in this same
change)**. §3's schema/wiring/version-pin proposal, §5's conditions, and §6's evidence-packaging plan
are all now APPLIED: `crates/wicked-council/src/types.rs` carries `acp_governance_env` +
`verified_version` on `AcpConfig`; `crates/wicked-council/src/registry.rs`'s opencode entry sets both
and flips `acp_input_governance: true`; `src/acp_runner.rs` wires the unconditional env injection
(§3.2/§3.3) and the spawn-time version-pin check with fail-closed downgrade (§3.4); the formal,
redacted `oq-opencode-acp-002/` evidence directory (§6 item 2) is committed alongside this design,
closing the one gap §2.5 flagged (network re-proof under the actual shipped mechanism) — see its
`verdict.md` for the one property (outside-workspace-read) still resting on the cross-validation env
var rather than a fresh `OPENCODE_CONFIG_CONTENT` capture, an explicitly stated, non-blocking gap.
Three new/updated tests back this: `governance_env_injection_reaches_the_child_and_writes_nothing_
into_the_unit_cwd` (the zero-tracked-file-mutation claim, §5 condition 1), `resolved_binary_version_
matches_the_exact_pinned_string_only` + `version_pin_mismatch_downgrades_governance_verified_without_
failing_the_spawn` (§3.4), and `opencode_admission_carries_its_governance_env_and_version_pin` (the
admission itself carries both, not a bare flag flip). §3.3's open sub-decision was resolved
**unconditional** injection (the option this design already recommended as the default).

**Predecessors:**
- DES-INPUT-GOV-001 (recon, #360) — defines the OQ proof bar (blocking request / canonical name +
  rawInput / reject honoured / disableable auto-approve).
- DES-INPUT-GOV-002 (#364) — the per-seat `acp_input_governance` capability, defaults OFF.
- **DES-INPUT-GOV-005 (opencode, #370)** — resolved OQ-OPENCODE-ACP-001 NOT ADMITTED under the
  registry's *unconfigured* invocation, and filed **OQ-OPENCODE-ACP-PROVISION-001**: should wicked
  provision a permission-tightening config for opencode ACP units, and how? §4.2 there proposed two
  mechanisms (A: worktree-root file drop; B: generalize claude's engine-owned config-home directory)
  and left both open, plus an unresolved config-precedence question (does a target repo's own
  `opencode.json` defeat an engine injection?). This design answers all three questions with a THIRD,
  better mechanism neither predecessor considered, verified live against the same pinned opencode
  build DES-005 used.

**Carrier:** `src/acp_permission.rs` (`pretool_payload`) is unchanged. `crates/wicked-council/src/
registry.rs` (the `opencode` `AcpConfig` block, currently `registry.rs:318-356`) and `src/acp_runner.rs`
(`start_acp_process`, `:1313-1440ish`, and its governed-unit call site, `:4127`) are the sites this
design's implementation phase touches.

**No `ASSUMPTION[external-transform]` applies** — same reasoning as DES-INPUT-GOV-002 §8 and
DES-INPUT-GOV-005 §3: opencode's ACP mode is native, the `session/request_permission` shape is an
in-repo-normalized protocol carrier, and this design's mechanism is an environment variable the
engine sets on its own child process — no third-party payload-transforming library or service sits
in this picture.

---

## 1. Decision

**Provision opencode's ACP spawn with `OPENCODE_CONFIG_CONTENT`, an inline-JSON environment variable
opencode's own config loader documents as its highest ordinary-precedence layer — not a file drop
(mechanism A) and not a generalized claude-style config-home directory (mechanism B).** This closes
OQ-OPENCODE-ACP-PROVISION-001 with a mechanism strictly better than either candidate DES-005 §4.2
proposed:

- **Zero file writes, anywhere** — not the worktree, not an engine-owned config-home directory, not
  the operator's real `~/.config/opencode`. There is nothing to restore before deliver, because
  nothing is ever created. The task's "if only the project file works, restore the tracked file
  before deliver stages" fallback clause **does not trigger** — the env-var route works, verified
  live (§3).
- **Immune to a target repo shipping its own `opencode.json`** — DES-005 §4.2 flagged this as an
  unresolved risk for both file-based mechanisms ("does a project-level config defeat a global-config
  injection?"). Verified live (§3): a fixture whose own committed `opencode.json` sets
  `permission: {"*": "allow"}` for everything is still fully gated when `OPENCODE_CONFIG_CONTENT` is
  set — the env var is loaded strictly after project config in opencode's own precedence order.
- **A documented feature, not a reverse-engineered internal.** opencode's public config docs
  (`https://opencode.ai/docs/config/`) name `OPENCODE_CONFIG_CONTENT` explicitly as "Inline config —
  runtime overrides," the highest layer short of MDM-managed preferences. This matters because
  admission is about to flip a bit that stays flipped release over release; relying on documented
  behavior is a materially different risk than relying on an undocumented one (§3.4 records the
  undocumented sibling this design rejected).

### 1.1 What "provisioning" actually buys — and what it does not

This is worth stating precisely because it is easy to over-claim: **the injected config does not
decide policy.** Once a governed opencode unit's ACP session actually asks the client
(`session/request_permission`), wicked-core's own `acp_permission`/`AcpGate` machinery answers that
request using the SAME boundary/scope policy the wrapped-CLI gate-hook enforces
(`src/acp_runner.rs:3942`, "GOVERNED UNITS RUN HERE NOW" — `gate_ctx` is `Some` exactly when
`cli_acp_input_governed(&cli_key)` is true, and its `AcpGate` is what `exec_turn_acp` consults, not
opencode's own allow/ask/deny verdict). opencode's shipped default (`"*": "allow"`,
DES-005/oq-opencode-acp-001) never reaches that gate at all, because `Permission.ask()` returns
before ever publishing the event the ACP `Handler` listens for — the client is simply never asked.
**The provisioned config's only job is to make every core intent within the working directory reach
`Permission.ask()` in the first place, so wicked-core's own gate gets a turn to answer.** This is why
the exact verdict opencode's own config carries (`"ask"` here) is not load-bearing in itself — what
matters is that no core-intent key resolves to `"allow"` before `Permission.ask()` runs. This
reframing also answers a question DES-005 did not need to ask (nothing there was ever admitted):
the provisioned value does not need to match wicked-core's policy, because wicked-core's policy is
applied downstream of the ask, independently of what value forced the ask to happen.

---

## 2. Empirical verification (live captures — packaged as `.product/evidence/oq-opencode-acp-002/`)

All captures below ran against the **same pinned artifact** DES-INPUT-GOV-005/oq-opencode-acp-001
evidenced: opencode CLI `1.17.18`, Homebrew tap `anomalyco/tap`, sha256
`652a34cab759c0fa348f107aa737df86355a49b1576834864e89ee43c059b25d`, invoked exactly as the registry's
built-in `AcpConfig` does (`opencode acp --cwd <fixture>`), reusing oq-opencode-acp-001's own
committed harness scripts (`capture-harness.mjs`, `probe-strict-reject-bash.mjs`) verbatim — the only
variable changed across every test below is the environment the harness's `spawn()` call inherits.
Every fixture is its own freshly `git init`'d repo with no remote, per oq-opencode-acp-001's own
project-identity-isolation finding (a shared-remote fixture would confound results with the operator's
accumulated interactive opencode grants).

### 2.1 Plain fixture, no project `opencode.json`, `OPENCODE_CONFIG_CONTENT` set

Payload: `{"$schema":"https://opencode.ai/config.json","permission":{"read":"ask","edit":"ask","bash":"ask"}}`
set as `OPENCODE_CONFIG_CONTENT` before spawning `opencode acp`.

| Scenario | Result |
|---|---|
| Allow (4-step read/edit/bash/write turn) | `requestPermissionCallCount: 4` — every step gated, real `rawInput`, `seed.txt` shows the edit landed, marker file created |
| Reject (same turn) | First step (`read`) gated and rejected; model stops the turn (the same stop-after-first-rejection behavior oq-opencode-acp-001's `capture-strict-reject.ndjson` recorded — model behavior, not evidence about edit/bash specifically) |

Matches oq-opencode-acp-001's `capture-strict-allow.ndjson`/`capture-strict-reject.ndjson` results
exactly (same 4/4 and same stop-after-first-failure shape), now produced with **no file on disk**
instead of a project-root `opencode.json`.

### 2.2 Collision fixture — project's OWN `opencode.json` forces `"*": "allow"`, env var also set

Fixture ships a committed `opencode.json`:
`{"$schema":"https://opencode.ai/config.json","permission":{"read":"allow","edit":"allow","bash":"allow"}}`
— i.e., a target repo actively trying to defeat governance by shipping its own permissive config.
`OPENCODE_CONFIG_CONTENT` set to the same tightening payload as §2.1.

| Scenario | Result |
|---|---|
| Allow | `requestPermissionCallCount: 4` — env var wins outright, all four steps gated despite the committed file saying `allow` for everything |
| Reject | First step (`read`) gated and rejected, turn stops — same shape as §2.1, confirming the collision does not weaken gating even under reject |

**This resolves DES-005 §4.2's open config-precedence question for the mechanism this design
recommends: the env var is not merely "also considered," it wins deterministically**, consistent
with opencode's own documented precedence order (env-var inline config loads after project config,
ahead of only MDM-managed preferences, which are not in play here).

### 2.3 Reject genuinely prevents a destructive action, isolated from turn-stop behavior

Reusing oq-opencode-acp-001's own isolation technique (a single self-contained `rm -rf sub` turn,
independent of the four-step turn's stop-after-first-failure): with `OPENCODE_CONFIG_CONTENT` set and
no project config, the request arrives (`kind: execute`, `title: "rm -rf sub"`), is rejected, the
`tool_call_update` resolves `status: "failed"`, and `subDirStillExists: true` — the destructive
command never ran. Matches oq-opencode-acp-001's `probe-strict-reject-bash.ndjson` exactly.

### 2.4 Cross-validation with a second, undocumented env var — and why it was rejected as the mechanism

Reverse-engineered from the installed binary's strings (`strings` over the Homebrew-installed Bun
executable; no source repo clone was needed): `OPENCODE_PERMISSION`, a second env var that merges
directly into the resolved `permission` object as the final step of config resolution, JSON-parsed
with a fail-open catch ("contains invalid JSON, skipping" — logged, not fatal). Independently
reproduces every result in §2.1–§2.3, **plus** was used to additionally re-confirm two properties
oq-opencode-acp-001 already established are unaffected by provisioning: a network `curl` gates as
`kind: execute` (1 request), and the pre-existing `external_directory` (outside-workspace read)
boundary still gates (2 requests: the `external_directory` ask, then the granted `read`). **This
design does NOT recommend `OPENCODE_PERMISSION`** as the shipped mechanism — it is absent from
opencode's own docs (confirmed by fetching `https://opencode.ai/docs/config/` directly and searching
for it), meaning it is *only* known to work because this design's author read it out of a compiled
binary's string table. It served here purely as a second, independent data point that the merge
architecture generalizes (two different env vars, same downstream `permission` sink, same
override-wins-over-project-config behavior) — corroboration, not the recommendation.

### 2.5 What was, and was not, re-run under `OPENCODE_CONFIG_CONTENT` specifically before sign-off

The network-curl and outside-workspace-read re-proofs in §2.4 originally used the undocumented
sibling var, not `OPENCODE_CONFIG_CONTENT` itself. The network-curl case was subsequently re-run
under the actual shipped mechanism and passed (`probe-cc-network.ndjson` in
`oq-opencode-acp-002/`, `requestPermissionCallCount: 1`, `kind: execute`). The
outside-workspace-read case was NOT re-run under `OPENCODE_CONFIG_CONTENT` specifically — it remains
evidenced only via the cross-validation var (`xval-envperm-outside-read.ndjson`). Both intents share
the identical downstream `permission`-object merge and gating path as the four-step turn and the
destructive-bash probe (which WERE re-run under `OPENCODE_CONFIG_CONTENT` and passed) — there is no
code-path reason to expect a different result for the remaining case, and it is orthogonal to
admission (`external_directory` gates by default regardless of any `permission` config, per
`oq-opencode-acp-001`) — but this is a stated, explicitly-flagged residual gap, not an assumption
dressed as a finding. `oq-opencode-acp-002/manifest.md` records it for the next capture session.

---

## 3. Builtin `AcpConfig` for opencode — shape shipped in this change

### 3.1 Schema addition: a generic governance-env injection point

`AcpConfig` (`crates/wicked-council/src/types.rs:68-95`) gets one new optional field, in the same
style as the existing `auth_method`:

```rust
/// An environment variable (name, value) the engine sets on the ACP child process, injected
/// AFTER `hardened()` clears the slate and BEFORE spawn, whenever this seat is spawned. This is
/// intentionally not conditional on the current unit's governance state: ACP sessions are cached
/// by `(run_id, cli_key)`, so a configuration that depended on the first unit to open the session
/// could be reused later with the wrong permission posture. Ungoverned requests are explicitly
/// answered by `allow_result`; the env injection changes their wire traffic, not their outcome.
/// Exists so an adapter whose default ruleset resolves every core intent to
/// "allow" (opencode: OQ-OPENCODE-ACP-001) can be forced to route every intent through
/// `session/request_permission` instead — wicked-core's own AcpGate then answers that request;
/// this value is a FORCING FUNCTION, not a policy statement (see DES-INPUT-GOV-006 §1.1).
/// `None` for seats needing no such injection (claude's directory-home mechanism is a
/// different shape — see `worker_claude_config_dir` — and stays its own dedicated path).
#[serde(default, skip_serializing_if = "Option::is_none")]
pub acp_governance_env: Option<(String, String)>,
```

opencode's built-in entry sets:
```rust
acp_governance_env: Some((
    "OPENCODE_CONFIG_CONTENT".into(),
    r#"{"$schema":"https://opencode.ai/config.json","permission":{"read":"ask","edit":"ask","bash":"ask"}}"#.into(),
)),
```

This keeps `start_acp_process` CLI-agnostic (data-driven off `AcpConfig`, matching the struct's
existing philosophy) rather than adding an `if cli_key == "opencode"` branch alongside the
claude-specific `worker_claude_config_dir` code. A generic tuple is sufficient for one seat; if a
second seat ever needs a *different* env var, the field already generalizes.

### 3.2 Spawn-site wiring (implementation phase — exact insertion point)

`start_acp_process`'s `build_cmd` closure (`src/acp_runner.rs:1340-1360`) already has the pattern to
mirror:
```rust
if let Some(dir) = &worker_config_dir {
    cmd.env(CLAUDE_CONFIG_DIR_ENV, dir);
}
```
Add, in the same closure:
```rust
if let Some((k, v)) = &config.acp_governance_env {
    cmd.env(k, v);
}
```
`config: &AcpConfig` is already a parameter of `start_acp_process` — no new argument needed for the
injection itself.

### 3.3 Open sub-decision: unconditional vs. governed-only injection

`start_acp_process` is CLI-generic and **governance-unaware** — it is called from the shared ACP
session path used by "ungoverned units of every CLI, plus governed units whose adapter is not yet
admitted" (`src/acp_runner.rs:4059`), not only from governed calls. The real call site for a governed
unit (`src/acp_runner.rs:4127`) already computes `gate_ctx` (`:3958-4057`) — `Some` exactly when this
unit is governed and the seat is admitted — **before** reaching the `start_acp_process` call a few
lines later, so a `bool`/`Option` reflecting `gate_ctx.is_some()` is trivially available to thread
through if injection should be governed-only.

This design recommends the simpler default — **inject unconditionally, for every opencode ACP spawn,
governed or not** — because an ungoverned session's incoming `session/request_permission` calls are
already answered by the shared ACP client's own unconditional `allow_result` responder
(`acp_ungoverned_event`'s own wording: "answered by allow_result, unchecked"), so forcing the ask for
an ungoverned session changes wire traffic (extra round-trips), not outcome. The tradeoff is real and
stated, not hidden: every non-governed opencode ACP turn (chat, council seat) pays a permission
round-trip per core intent it did not pay before. If that overhead proves material once measured, the
narrower governed-only wiring is a small, well-understood follow-up (thread the existing
`gate_ctx.is_some()` value one call deeper) — not a re-design.

### 3.4 Version pin — new territory, because this is the first seat that actually flips

Every prior OQ (`pi`, `codex`, `copilot`, opencode's own DES-005) stayed NOT ADMITTED, so a Homebrew
tap auto-update silently changing behavior was a recorded risk with no consequence (the flag was
already `false`). **This design's whole point is to flip `acp_input_governance` to `true` for
opencode** — so an upstream update that changes the default `"build"` agent's ruleset, or changes how
`OPENCODE_CONFIG_CONTENT` merges, would silently re-open the exact gap oq-opencode-acp-001 found,
under a config the engine believes is still closing it. No existing field on `AgenticCli`/`AcpConfig`
guards against this: `version_probe` (`crates/wicked-council/src/probe.rs:142`) only classifies a
binary as usable/unusable from its `--version` output, it does not compare against a known-good
value anywhere in the codebase (confirmed by grep — no call site reads `version_probe`'s captured
stdout for a comparison, only for the usable/unusable classification).

Proposed: a **new**, opencode-specific runtime guard, checked at the same point `gate_ctx` decides
admission (`cli_acp_input_governed`, `src/acp_runner.rs:4509`) — not a new generic `AcpConfig` field,
because generalizing a version-pin mechanism for every future seat is exactly the kind of
speculative abstraction this codebase's own conventions warn against; solve opencode's instance
concretely, generalize only when a second seat needs the same shape.

```rust
/// The exact opencode --version output this seat's admission was proven against
/// (oq-opencode-acp-002). A version mismatch does not fail the spawn — it fails
/// CLOSED on the governance claim only: the unit still runs, but `gate_ctx` treats
/// this seat as unadmitted for this spawn (the same `GovernanceUnenforced` disclosure
/// path as `acp_input_governance: false`), because the ruleset this env-var override
/// forces onto was proven against one specific build, not a range.
const OPENCODE_VERIFIED_VERSION: &str = "1.17.18";
```

### 3.5 Pin-check contract (required implementation detail) — APPLIED

Implemented exactly as specified: `exec_turn_inner` no longer calls a separate
`cli_acp_input_governed(&cli_key)` (which independently reloaded the merged registry a second time
for the same seat) — that function is deleted. `gate_ctx`'s admission bool is now derived directly
from `acp_cfg_probe` (`acp_cfg_probe.as_ref().is_some_and(|c| c.acp_input_governance)`), the exact
same resolved `AcpConfig` value that a few lines later becomes `acp_config` and is passed to
`start_acp_process`. One resolution, reused for both the admission check and the actual spawn — no
second registry read that could race a concurrent `clis.toml` edit and diverge from what gets
spawned.

The guard must be bound to the exact registry record that will be spawned, rather than performing
a second registry lookup by `cli_key`. Today `exec_turn_inner` obtains `acp_cfg_probe` and then
`cli_acp_input_governed` independently reloads the disk-backed merged registry; that is harmless
while the predicate is only a bool, but is not acceptable once the predicate asserts a fact about
the launched executable.

The implementation should resolve `AgenticCli` once, retain its `AcpConfig`, and derive one
`AcpAdmission` result from that same value. For the built-in `opencode` seat with
`acp_input_governance: true`, it must invoke **that resolved `AcpConfig.binary`** with
`--version`, using the council's existing hardened, bounded version-probe semantics, and compare
the first trimmed output line exactly to `OPENCODE_VERIFIED_VERSION`. Spawn failure, timeout,
non-zero exit, missing output, or a mismatch all produce `NotAdmitted`; they must never fall back
to the bool alone. The resolved config carried by that result is then the one passed to
`start_acp_process`.

A mismatch does not prevent an ordinary ACP session from running. It prevents only the governance
claim: do not arm the ACP gate, emit the existing `GovernanceUnenforced` disclosure with a
pin-mismatch reason, and keep `output.governed` false. This preserves availability while failing
closed on the claim that was proved only for 1.17.18. The injection remains unconditional per
§3.3, so session reuse cannot accidentally make a later governed turn silent.

---

## 4. Identity gap — accepted, documented, not a blocker for this admission

Every `session/request_permission` opencode emits carries no top-level `toolName` and no
`toolCall.name` — only `toolCall.title` (bare file path for edit/write, literal shell command for
bash, the coincidental string `"read"` for reads). `pretool_payload`
(`src/acp_permission.rs:56`) resolves `tool_name` via `toolName` → `toolCall.name` → `toolCall.title`,
so it already falls through to this free-text value today, for every seat (`codex`, `copilot` share
the identical gap, per their own verdicts). **This design accepts title-as-name as the identity basis
for opencode's admission** rather than blocking on the `toolCall.kind`-keyed normalization DES-005 §4.4
and the codex/copilot verdicts each independently proposed as a shared follow-up. Rationale: `kind` is
a small, stable, already-observed enum (`read`/`edit`/`execute`/`other`), but `pretool_payload` not
consulting it is a **degraded-precision** gap (policy keyed on canonical tool identity, e.g. "deny
Bash specifically," loses precision and falls back to matching free shell text) — it is not a
**gate-bypass** gap (every action still reaches the gate and is still answered by wicked-core's own
boundary/scope policy, per §1.1). The task scoping this design explicitly calls for documenting the
title-as-name basis rather than resolving it, consistent with that severity distinction.

---

## 5. Conditions for flipping `acp_input_governance: true` — all three, DONE

1. **Provisioning mechanism verified to make zero tracked-file mutations** — DONE. §2's live
   captures plus two automated tests:
   `governance_env_injection_reaches_the_child_and_writes_nothing_into_the_unit_cwd` (a tracked
   fixture's file set + content is byte-for-byte unchanged after a governed spawn) and
   `provisioned_governance_env_leaves_a_tracked_permissive_config_git_clean` (the same claim, proven
   via `git diff --exit-code` + `git status --porcelain` against a git-tracked, permissive
   `opencode.json`, per §6.3(c)). The "restore the tracked file before deliver" fallback clause never
   triggers — there is nothing to restore.
2. **Builtin `AcpConfig` ships the injection + a version guard** — DONE. `AcpConfig::acp_governance_env`
   and `AcpConfig::verified_version` (`crates/wicked-council/src/types.rs`), opencode's registry
   entry setting both plus `acp_input_governance: true`
   (`crates/wicked-council/src/registry.rs`), unconditional injection in `start_acp_process`'s
   `build_cmd` closure, and the spawn-time version-pin check (`resolved_binary_version_matches`)
   feeding `AcpProcess::governance_verified`, consulted at the `gate` construction site alongside
   `gate_ctx` (`src/acp_runner.rs`). §3.5's single-resolution requirement is applied: the separate
   `cli_acp_input_governed` registry reload is deleted; admission and the spawned config now share
   one `acp_cfg_probe` resolution.
3. **oq-opencode-acp-002 evidence passes re-proof against the actual provisioned config** — DONE,
   packaged and committed at `.product/evidence/oq-opencode-acp-002/` (manifest, verdict, README,
   twelve redacted captures). The §2.5 gap is now partially closed: the network-curl probe was
   re-run under the actual `OPENCODE_CONFIG_CONTENT` mechanism and passes
   (`probe-cc-network.ndjson`); the outside-workspace-read probe was NOT re-run under
   `OPENCODE_CONFIG_CONTENT` specifically (only under the cross-validation env var,
   `xval-envperm-outside-read.ndjson`) — a small, explicitly-stated residual gap recorded in
   `oq-opencode-acp-002/manifest.md`, orthogonal to admission since that property is unaffected by
   provisioning (`oq-opencode-acp-001` already established `external_directory` gates by default
   regardless of any `permission` config).

**`acp_input_governance: true` is shipped for opencode in this same change** — all three conditions
pass; the one residual evidence gap (outside-workspace-read under the exact shipped env var) is
stated, not hidden, and does not bear on any of the three conditions above.

---

## 6. Status — all handoff items from the clarify phase are applied

1. ~~Close §2.5~~ — network-curl closed (see §5 item 3); outside-workspace-read remains a stated gap.
2. ~~Package `oq-opencode-acp-002/`~~ — DONE, committed with `manifest.md`, `verdict.md`, `README.md`,
   and twelve redacted `*.ndjson` captures (`capture-cc-*`/`probe-cc-*` under the shipped mechanism,
   `xval-envperm-*` cross-validation under a second, undocumented env var — see the manifest for
   exactly which properties each covers).
3. **Implementation** — DONE: §3's `AcpConfig`/`types.rs`/`registry.rs`/`acp_runner.rs` changes are
   applied, §3.3's unconditional-injection decision is taken, §3.4/§3.5's version guard (with the
   single-resolution fix) is applied, `acp_input_governance: true` ships for opencode, and both
   built-in-roster assertions that used to assert "only claude" now assert `claude || opencode`
   (`only_proven_acp_adapters_are_admitted_in_the_builtin_roster` in
   `crates/wicked-council/src/registry.rs`; `acp_input_governance_is_admitted_by_capability_only` in
   `src/acp_runner.rs`). The required test set landed: (a)
   `governance_env_injection_reaches_the_child_and_writes_nothing_into_the_unit_cwd` proves the
   spawn wiring end-to-end through the real `start_acp_process`, not a registry literal; (b)
   `resolved_binary_version_matches_the_exact_pinned_string_only` +
   `version_pin_mismatch_downgrades_governance_verified_without_failing_the_spawn` cover the
   pin-match and pin-mismatch cases (mismatch never produces an armed gate, but the spawn itself
   still succeeds); (c) `provisioned_governance_env_leaves_a_tracked_permissive_config_git_clean` is
   exactly the git-tracked collision/precedence + regression-guard test this section specified, and
   `opencode_admission_carries_its_governance_env_and_version_pin` additionally asserts the shipped
   registry entry itself carries both fields, not a bare flag flip.
4. **Commit order** — followed: this design doc's earlier revisions, the evidence directory, and the
   code changes all land in this same commit, gates run against the fully assembled tree (§7).

---

## 7. Gates (run against the fully committed tree in this same change)

- `cargo fmt --all -- --check`
- `cargo test -p wicked-council`
- `cargo test --lib`
- `cargo clippy --all-targets -- -D warnings`

**Known-benign failure to expect** (recorded identically by DES-INPUT-GOV-004 §6 and DES-INPUT-GOV-005
§7): `builtin_floors::tests::floor_fails_closed_outside_a_git_repo` fails in this sandbox because
`TMPDIR` is set inside the worktree, violating that test's own "outside any repo" premise — a
pre-existing environment artifact, not something this work introduces or should "fix."
