# OQ-OPENCODE-ACP-PROVISION-001 verdict

**Recommended admission: ADMITTED.** The provisioned config (`OPENCODE_CONFIG_CONTENT`, injected via
`AcpConfig::acp_governance_env`) closes the gap `oq-opencode-acp-001/verdict.md` found in the
registry's unconfigured invocation, survives a target repo shipping its own colliding
`opencode.json`, requires zero file writes, and a selected reject genuinely blocks a destructive
action under it. `acp_input_governance: true` is justified for opencode subject to the version pin
(`AcpConfig::verified_version = "1.17.18"`) staying current — see "Gap not closed" in `manifest.md`.

## Property (a): does every core tool intent produce a blocking `session/request_permission` with
a canonical tool name + raw input?

**PASS on gating (title-as-name basis, documented and accepted — not blocking, per DES-INPUT-GOV-006
§4).** `capture-cc-plain-allow.ndjson`: all four steps of the read/edit/bash/write turn produce a
`session/request_permission`, `requestPermissionCallCount: 4`, real `rawInput`/`locations`. Isolated
single-command probes confirm the same for a destructive bash call (`probe-cc-reject-bash.ndjson`,
`kind: execute`, `title: "rm -rf sub"`) and a network call (`probe-cc-network.ndjson`, `kind:
execute`, `title: "curl -sI https://example.com"`). Identity is still free-text `toolCall.title` (a
bare path, a literal shell command, or the coincidental string `"read"`), never a top-level
`toolName` or `toolCall.name` — unchanged from `oq-opencode-acp-001`'s finding. This admission
accepts that basis (DES-INPUT-GOV-006 §4): every action still reaches wicked-core's own `AcpGate`
and is answered by boundary/scope policy, which is a gate-bypass concern that this evidence closes;
the free-text identity is a separate, degraded-precision concern the `toolCall.kind`-normalization
follow-up (DES-INPUT-GOV-006 §4, shared with codex/copilot) addresses independently.

## Property (b): auto-approve default is genuinely displaced — including against a colliding
project config

**PASS, and this is the property beyond `oq-opencode-acp-001`'s own scope.** opencode's shipped
default (baked `"*": "allow"`) is the thing `oq-opencode-acp-001` proved never asks. This evidence
proves the DISPLACEMENT: `capture-cc-plain-allow.ndjson` (no project config at all) shows 4/4 gated
under `OPENCODE_CONFIG_CONTENT` alone. More decisively, `capture-cc-collision-allow.ndjson` and
`capture-cc-collision-reject.ndjson` run against a fixture whose OWN committed `opencode.json`
explicitly sets `{"permission":{"read":"allow","edit":"allow","bash":"allow"}}` — a target repo
actively trying to defeat governance — and the env var still wins outright: 4/4 gated under allow,
and the reject scenario's first step still gates and blocks despite the file's explicit "allow".
This resolves the open config-precedence question `DES-INPUT-GOV-005 §4.2` left for the file-based
mechanisms it considered (a worktree-root file or a config-home directory could in principle be
overridden or collide with a repo's own file); `OPENCODE_CONFIG_CONTENT` cannot be, because it loads
strictly after project config in opencode's own documented precedence order and no file is ever
involved at all.

## Property (c): a selected reject genuinely prevents the action

**PASS.** `probe-cc-reject-bash.ndjson`: the isolated `rm -rf sub` request is rejected, the
`tool_call_update` resolves `status: "failed"`, and the target directory is confirmed still present
after the turn — the destructive command never ran. `capture-cc-collision-reject.ndjson` shows the
same shape (turn stops after the first rejected step, matching `oq-opencode-acp-001`'s own
stop-after-first-failure model-behavior finding) even with the colliding permissive project config
present.

## Property: zero tracked-file mutation

**PASS, and provable by construction, not just by these captures.** Every capture in this directory
that used the shipped mechanism (`capture-cc-*`, `probe-cc-*`) set `OPENCODE_CONFIG_CONTENT` as a
process environment variable only — no file was written to any fixture for these captures (the two
`*-collision-*` fixtures' `opencode.json` is the fixture's OWN pre-existing committed file, present
to test whether it can defeat the injection, not something the mechanism itself created). The
production implementation (`crates/wicked-council/src/registry.rs`'s opencode `AcpConfig`,
`src/acp_runner.rs`'s `build_cmd` closure in `start_acp_process`) only ever calls `cmd.env(k, v)` —
it contains no file-write call on any code path. This is additionally covered by an automated test,
`acp_runner::tests::governance_env_injection_reaches_the_child_and_writes_nothing_into_the_unit_cwd`,
which seeds a fixture cwd with a stand-in tracked file, spawns a stub ACP bridge with
`acp_governance_env` set, confirms the child actually observed the injected value, and asserts the
fixture directory's file set and the tracked file's content are byte-for-byte unchanged afterward.
The task's "restore the tracked file before deliver" fallback clause therefore never triggers: there
is no tracked-file mutation to restore.

## Overall

All three OQ-OPENCODE-ACP-001 proof properties pass under the actual provisioned config, the
provisioning mechanism is proven immune to a repo's own colliding config, and proven to write zero
files (backed by both this evidence and an automated test). `acp_input_governance: true` for opencode
is justified, gated by `verified_version: "1.17.18"` so an unpinned Homebrew-tap upgrade downgrades
this specific process's governance claim (`AcpProcess::governance_verified`) rather than silently
keeping a stale admission. The one remaining, explicitly-stated gap (outside-workspace-read not yet
re-captured under `OPENCODE_CONFIG_CONTENT` byte-for-byte, only under the cross-validation env var —
see `manifest.md`) is orthogonal to admission: it is a property `oq-opencode-acp-001` already
established is unaffected by provisioning (the `external_directory` default gates regardless of any
`permission` config), not one this evidence set introduces uncertainty about.

## Registry disposition (applied in this same change)

`crates/wicked-council/src/registry.rs`'s opencode `AcpConfig`: `acp_input_governance: true`,
`acp_governance_env: Some(("OPENCODE_CONFIG_CONTENT".into(), <the JSON literal above>))`,
`verified_version: Some("1.17.18".into())`. The built-in-roster test
(`only_proven_acp_adapters_are_admitted_in_the_builtin_roster`, formerly asserting only `claude`) and
`acp_runner::tests::acp_input_governance_is_admitted_by_capability_only` both now assert
`claude || opencode`. A new test,
`opencode_admission_carries_its_governance_env_and_version_pin`, asserts the admitted config actually
carries both the forcing-function env var and the exact version pin — an admission without either
would be indistinguishable from the codex/pi/copilot pattern of a bare, unverified boolean flip.
