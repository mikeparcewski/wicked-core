# OQ-OPENCODE-ACP-PROVISION-001 — provenance manifest

Captured 2026-09-05, same session as `.product/DES-INPUT-GOV-006-opencode-acp-provisioned-admission.md`.
Re-proves opencode's ACP admission against the wicked-provisioned config route this design ships
(`AcpConfig::acp_governance_env`, `crates/wicked-council/src/registry.rs`), closing the follow-up
`oq-opencode-acp-001/verdict.md` filed under "Forward path to a governed opencode" and the mechanism
question DES-INPUT-GOV-005 §4.3 recorded as OQ-OPENCODE-ACP-PROVISION-001.

## Adapter under test — same pinned artifact as oq-opencode-acp-001, re-verified this session

| Field | Value |
|---|---|
| Distribution | opencode CLI, Homebrew tap `anomalyco/tap`, `/opt/homebrew/bin/opencode` |
| Self-reported version (`opencode --version`) | `1.17.18` — unchanged from oq-opencode-acp-001 |
| Invocation | `opencode acp --cwd <fixture>` — byte-identical to the registry's `AcpConfig.start_args` (`["acp"]`); `--cwd` only pins the working directory (oq-opencode-acp-001/manifest.md) |

This evidence set changes exactly one thing relative to oq-opencode-acp-001: the environment the
harness's `spawn()` call inherits. No project-level `opencode.json` (except the deliberate collision
fixtures, §"Collision fixtures" below), no other flag, no other invocation change.

## The mechanism under test

`OPENCODE_CONFIG_CONTENT`, opencode's own documented "Inline config — runtime overrides" env var
(`https://opencode.ai/docs/config/`), set to:
```json
{"$schema":"https://opencode.ai/config.json","permission":{"read":"ask","edit":"ask","bash":"ask"}}
```
— the exact literal `crates/wicked-council/src/registry.rs`'s opencode `AcpConfig.acp_governance_env`
now ships. Set via the harness's inherited `process.env` (`capture-harness.mjs`/`probe-*.mjs` already
spawn with `env: process.env`, unmodified from oq-opencode-acp-001) — **no file was written to disk
for any capture in this directory** except the two collision fixtures' own committed `opencode.json`,
which exists specifically to test whether it can defeat the injection (it cannot — see `verdict.md`).

## Harnesses reused verbatim from oq-opencode-acp-001 (not duplicated here)

- `../oq-opencode-acp-001/capture-harness.mjs` — the four-step read/edit/bash/write turn
- `../oq-opencode-acp-001/probe-strict-reject-bash.mjs` — isolated destructive `rm -rf`, reject-only
- `../oq-opencode-acp-001/probe-network.mjs` — isolated `curl`, allow-only
- `../oq-opencode-acp-001/probe-outside-read.mjs` — outside-workspace read (cross-validation only,
  see below)

No harness code changed. Re-run any capture with, e.g.:
```
OPENCODE_CONFIG_CONTENT='{"$schema":"https://opencode.ai/config.json","permission":{"read":"ask","edit":"ask","bash":"ask"}}' \
  node ../oq-opencode-acp-001/capture-harness.mjs opencode <fresh-fixture-dir> <output.ndjson> <allow|reject>
```

## Fixtures

Every fixture is its own freshly `git init`'d repository with no remote (oq-opencode-acp-001's own
project-identity-isolation finding: opencode keys its permission ruleset off `git remote get-url
origin`, so a shared-remote fixture would confound results with the operator's accumulated
interactive-opencode grants). Committer identity `oq-evidence@example.invalid` — an invented
placeholder, not a real address, same convention as oq-opencode-acp-001.

- **Plain fixtures** (`capture-cc-plain-allow.ndjson`): no project `opencode.json` at all.
- **Collision fixtures** (`capture-cc-collision-{allow,reject}.ndjson`,
  `xval-envperm-collision-{allow,reject}.ndjson`): the fixture's OWN committed `opencode.json` sets
  `{"permission":{"read":"allow","edit":"allow","bash":"allow"}}` — i.e. a target repo actively
  trying to defeat governance by shipping a maximally permissive config of its own.
- **Isolated single-command probes** (`probe-cc-reject-bash.ndjson`, `probe-cc-network.ndjson`,
  `xval-envperm-reject-bash.ndjson`, `xval-envperm-network.ndjson`): a fixture with only a `sub/`
  directory (destructive probe) or a bare `seed.txt` (network probe) — isolates one action from the
  four-step turn's own stop-after-first-rejection model behavior (oq-opencode-acp-001 already
  recorded this same isolation need).

## Cross-validation files (`xval-envperm-*.ndjson`) — what they are and why they exist

The `xval-envperm-*` captures use a SECOND, undocumented env var (`OPENCODE_PERMISSION`, found by
reading the installed binary's string table — not documented at `https://opencode.ai/docs/config/`,
confirmed absent by fetching that page directly) that merges into the same resolved `permission`
object as `OPENCODE_CONFIG_CONTENT`, just at a different point in config resolution. **This is NOT
the mechanism the registry ships** — `AcpConfig::acp_governance_env` uses `OPENCODE_CONFIG_CONTENT`
exclusively (DES-INPUT-GOV-006 §1, §2.4). These files exist purely as corroboration that the
underlying merge architecture generalizes across two different injection points, and to additionally
re-confirm two properties orthogonal to the provisioning question (a network `curl` gates as
`kind: execute`; the pre-existing `external_directory` outside-workspace-read boundary is unaffected
by provisioning) that were not re-run under `OPENCODE_CONFIG_CONTENT` specifically for the
outside-workspace-read case — `xval-envperm-outside-read.ndjson` is the only evidence for that one
property in this directory; `probe-cc-network.ndjson` (the network case) WAS re-run under the actual
shipped mechanism, `OPENCODE_CONFIG_CONTENT` (see `probe-cc-*` naming convention: `cc` = "config
content", the real mechanism).

**Stated gap, not a hidden one**: the outside-workspace-read property is proven for the mechanism
that generalizes to `OPENCODE_CONFIG_CONTENT` (same merge sink) but not re-captured under
`OPENCODE_CONFIG_CONTENT` byte-for-byte itself in this evidence set. A future capture session should
close this by re-running `probe-outside-read.mjs` with `OPENCODE_CONFIG_CONTENT` set instead of
`OPENCODE_PERMISSION` — mechanical, no code change, same expected result as every other property that
WAS re-run under both env vars and matched.

## Redaction

Same two-pass method as `oq-opencode-acp-001/README.md`: whole-path substitution (worktree root →
`<WORKTREE_ROOT>`, home directory → `<HOME>`, every per-fixture `mktemp -d` temp root →
`<FIXTURE_ROOT>`, the outside-workspace probe directory → `<OUTSIDE_DIR>`) plus blanket elision of
every `agent_thought_chunk`/`agent_message_chunk` frame's `content.text` (streamed model narration
can fragment a secret across frames, so it is elided wholesale rather than pattern-matched — see
`oq-opencode-acp-001/README.md` for the full rationale, unchanged here). Verified after redaction: a
scan for the operator's username/surname/home-directory prefix and a separate email-shaped-string
scan both found zero matches across every file in this directory; every `agent_thought_chunk`/
`agent_message_chunk` frame carries the elision placeholder (counts cross-checked 1:1 per file, see
`README.md`). Unredacted raw captures remain only in this operator's local scratch (`/tmp/oq2-*.ndjson`
outside the worktree, never staged) — not preserved anywhere durable; this is a one-time capture
session's output, same disposability convention as oq-opencode-acp-001's gitignored `tmp/`.

## Gap not closed by this evidence (unchanged from oq-opencode-acp-001)

The Homebrew tap auto-updates with no lockfile; a version drift is exactly what
`AcpConfig::verified_version` (`"1.17.18"`, DES-INPUT-GOV-006 §3.4) guards against at spawn time
going forward — this evidence is the proof that pin cites, not a claim that stays true across an
upgrade.
