# OQ-COPILOT-ACP-001 — provenance manifest

Captured 2026-09-04. This freezes the exact artifact this evidence set is about, per the open
question's requirement for an immutable pin.

## Candidate adapter identification (prerequisite check)

Copilot has **no separate third-party ACP bridge package** (unlike `codex-acp` or `pi-acp`). The
registry's own comment already identifies the candidate correctly: the GitHub Copilot CLI speaks
**native ACP over stdio** via its own `--acp` flag — `copilot --acp` is not a bridge around a
different underlying agent, it is the same binary the `headless_invocation` already drives, started
in a different mode. That is the candidate this evidence evaluates. A viable candidate exists, so
this OQ proceeds to the live-capture proof rather than resolving NOT ADMITTED on identification
alone.

## Adapter under test

| Field | Value |
|---|---|
| Distribution | GitHub Copilot CLI, installed via Homebrew cask `copilot-cli` (`/opt/homebrew/bin/copilot`, a compiled native Mach-O arm64 binary — **not** an npm/Node bridge process) |
| Official ACP source | GitHub Docs, ["Copilot CLI ACP server"](https://docs.github.com/en/copilot/reference/copilot-cli-reference/acp-server): `copilot --acp` starts the CLI's ACP server and defaults to stdio when no transport is selected |
| Self-reported version (`copilot --version`) | `GitHub Copilot CLI 1.0.83` |
| Cask formula | `copilot-cli` 1.0.83 (`auto_updates`; originally cask-installed as 1.0.5 on 2026-03-16, the binary self-updates independent of the cask's tracked version — the self-reported `1.0.83` above is what was actually invoked for this capture, and is the authoritative version pin) |
| Installed binary sha256 | `15f218a936f693a6b73df248824b9f7f528c2c61949ff446e4ca6062ee48b084` |
| Corresponding npm package (same upstream release channel, for cross-reference) | `@github/copilot@1.0.83` (`npm view @github/copilot version` resolves to the identical `1.0.83`; `dist.shasum 1418990ba2f811d45c04ec74e4de3e352b16e1c9`, repository `github/copilot-cli`) — the npm package is a *parallel* distribution of the same CLI, not the artifact actually invoked here (the invoked artifact is the Homebrew-cask compiled binary) |
| Registry comment cited version at authoring time | `v1.0.75` (the PRE-change comment, replaced in this same change) — this evidence is scoped to the newer `1.0.83` actually installed on the evidence host |

**Implementation language / architecture note**: unlike `codex-acp`/`pi-acp`, this is not a
TypeScript/Node bridge process wrapping a separate agent CLI. `copilot --acp` is the same compiled
binary as `copilot -p "..."`, started with a JSON-RPC-over-stdio server mode instead of one-shot
prompt mode. There is no separate adapter package/version to pin independently of the CLI itself.

**Gap not closed by this evidence**: the CLI has an internal auto-update mechanism
(`--no-auto-update` exists as a flag, implying updates are otherwise automatic) and no lockfile pins
it; a future in-place update could silently change ACP-mode behavior without re-triggering this
evidence. That is a follow-up (pin the binary, or re-verify on version bump), not something this
capture can resolve on its own — the same class of gap `oq-codex-acp-001/manifest.md` recorded for
its semver range.

## Runtime environment

| Field | Value |
|---|---|
| Invocation captured | `copilot --acp` (no other flags) — **byte-identical** to the registry's built-in `AcpConfig { binary: "copilot", start_args: ["--acp"], ... }` (`crates/wicked-council/src/registry.rs:288-295`); one additional capture added `--allow-all-tools` to probe the auto-approve control (§ Property (b) below), which the registry's built-in invocation does **not** pass |
| Node.js | not applicable — the invoked binary is a compiled native executable, not spawned via `node` |
| OS | Darwin 25.5.0, arm64 (macOS) |
| Transport | stdio, cwd = isolated fixture directory per run |
| Auth | pre-existing local GitHub Copilot CLI login (local session store and configuration already present); no credentials cross the captured ACP stdio channel |

## Capture method

The capture harness advertises the **exact** client capabilities the real wicked-core ACP client
sends (`src/acp_runner.rs:1518-1523`): `{"fs":{},"terminal":false,"permission":true}`. Advertising
`permission: true` is load-bearing: it declares that the client ANSWERS
`session/request_permission`, so any copilot ACP-mode configuration that gated permission requests
on that capability would be forced to send them here.

A minimal ACP JSON-RPC-2.0-over-NDJSON client (`capture-harness.mjs`, in this directory, modeled on
`.product/evidence/oq-codex-acp-001/capture-harness.mjs`) was spawned against the installed
`copilot` binary directly with `--acp` (no SDK-version coupling — the capture reflects exactly what
crosses the wire). It performs `initialize` → `session/new` (cwd = a fresh, empty, isolated fixture
directory under `tmp/`, never committed, seeded with a `seed.txt` file) → `session/prompt` asking
copilot to, in order: (1) read `seed.txt`, (2) edit `seed.txt` to append a line, (3) run a harmless
shell command, (4) create a small marker file — exercising all four CORE tool-intent classes
(read/write/edit/bash) in one turn. It logs every JSON-RPC frame in both directions verbatim to
NDJSON with a timestamp and direction tag.

Six captures were taken, each in its own fresh fixture directory:

- `capture-allow.ndjson` — the four-step read/edit/bash/write turn under the DEFAULT invocation
  (`copilot --acp`, no extra flags), harness auto-approves any incoming
  `session/request_permission`.
- `capture-reject.ndjson` — the identical four-step turn under the DEFAULT invocation, harness
  auto-**rejects** any incoming `session/request_permission`.
- `capture-allow-all-tools.ndjson` — the identical four-step turn with `--allow-all-tools` added to
  the spawn (the CLI's documented "required for non-interactive mode" auto-approve flag), harness
  still auto-approves (irrelevant if no requests arrive) — tests whether this flag is what
  suppresses permission requests, and confirms it is **not** part of the registry's built-in
  invocation.
- `probe-outside-read.ndjson` (`probe-outside-read.mjs`) — a single read of a file **outside** the
  session's cwd, to test whether reads are gated when the target path is outside the CLI's trusted
  directory (the plain in-cwd read in `capture-allow.ndjson` never triggered a permission request).
- `probe-risky.ndjson` (`probe-risky.mjs`) — a single-command turn (`rm -rf` on a scratch
  subdirectory) under the DEFAULT invocation, to check whether an explicitly destructive bash-class
  action escalates to a permission request or is resolved internally the way `codex-acp`'s
  `approvalsReviewer: "auto_review"` did (`oq-codex-acp-001/verdict.md` probe 5).
- `probe-network.ndjson` (`probe-network.mjs`) — a single-command turn (`curl -sI
  https://example.com`) under the DEFAULT invocation, to check whether network/URL access escalates
  to a permission request.

See `verdict.md` for the analysis and `README.md` for what was redacted before these files were
committed.
