# OQ-CODEX-ACP-001 — provenance manifest

Captured 2026-09-04. This freezes the exact artifact this evidence set is about, per
the open question's requirement for an immutable pin (a semver range is not
sufficient).

## Adapter under test

| Field | Value |
|---|---|
| npm package | `@agentclientprotocol/codex-acp` |
| Resolved version | `1.9.0` |
| Repository | `https://github.com/agentclientprotocol/codex-acp.git` |
| `gitHead` (from npm registry metadata) | `67db0d3d4a8a9b4bd3040c4dfdfa0919e9d97be9` |
| npm tarball URL | `https://registry.npmjs.org/@agentclientprotocol/codex-acp/-/codex-acp-1.9.0.tgz` |
| npm tarball integrity (sha512, base64) | `sha512-JYuQEH066Rvl/q5viSSFM/g4jNb3AxPWXQV39T4iiZdBoJO0SdDl10ZDGe/ZSqXlpHjndgCj4+uJp98F2wM+rA==` |
| Tarball sha256 (independently computed) | `1471eec4113f45ab0796f5657fc698fcbbd2f73b481836ea99ad076ed713e67c` |
| Installed `dist/index.js` sha256 | `be72db4042b5ee10b8c7051e8b9ba7e3234549e505d22e6a4f2e7ecf50a4af22` |
| Owning dependency pin | `wicked-crew@<installed>` → `"@agentclientprotocol/codex-acp": "^1.1.7"` (range, resolved to `1.9.0` at capture time — see gap below) |

**Implementation language**: the built-in registry correctly identifies this as a TypeScript/Node
bridge, not a Rust binary. The shipped package is bundled with esbuild/bun to `dist/index.js`
(`"type": "module"`; development dependencies include `@types/node` and `tsx`). It vendors and
drives the actual `codex` CLI binary
(`@openai/codex: ^0.153.3`, its own npm dependency) as a subprocess and translates its
app-server JSON-RPC protocol into ACP. This does not change the admission verdict but the
implementation language matters when reproducing the capture: the candidate is the npm-provided
`codex-acp` executable, not a separately compiled Rust program.

ASSUMPTION[external-transform] library=@agentclientprotocol/codex-acp transform=Codex app-server JSON-RPC to ACP protocol frames confidence=known :: The Node adapter spawns the Codex CLI, receives its app-server JSON-RPC events and approval decisions, and presents ACP session updates and permission requests to the client; this evidence evaluates those client-visible ACP frames.

Verification performed: downloaded the tarball fresh from the npm registry URL above, extracted
it, and confirmed its `dist/index.js` is byte-identical (sha256) to the copy already installed at
`node_modules/@agentclientprotocol/codex-acp/dist/index.js` under the local `wicked-crew` global
install. Independently recomputed the sha512 tarball hash and it matches the registry's
`dist.integrity` exactly. The repository was cloned at `gitHead` and its `package.json` confirms
`"version": "1.9.0"`, so the TypeScript source read in this evidence set is the exact source the
shipped `dist/index.js` was built from.

**Gap not closed by this evidence**: `wicked-crew`'s dependency is `^1.1.7`, a semver range, not a
lockfile-pinned exact version, and the package publishes very frequently (34 versions between
2026-04-24 and 2026-09-04, including a newer `1.10.0` published roughly 90 minutes after `1.9.0`
during this evidence run). This manifest freezes what was resolved *today*; a future `npm install`
could silently resolve a materially different version without re-triggering this evidence. That is
a follow-up (pin or re-verify-on-bump), not something this capture can resolve on its own.

## Runtime environment

| Field | Value |
|---|---|
| `codex` (backing agent binary) version | `codex-cli 0.153.3` (matches codex-acp's `@openai/codex: ^0.153.3` dependency) |
| Node.js | as installed on the capture host |
| OS | Darwin 25.5.0, arm64 (macOS) |
| codex-acp invocation | `codex-acp` (no args), stdio transport, cwd = isolated fixture directory per run |
| Auth | pre-existing local ChatGPT login (`codex login status` → "Logged in using ChatGPT"); no credentials cross the captured ACP stdio channel |
| Agent mode exercised | `AgentMode.DEFAULT_AGENT_MODE` = `AgentMode.Agent` (`approvalPolicy: "on-request"`, `approvalsReviewer: "auto_review"`, `sandboxMode: "workspace-write"`) for the primary captures — this is what a governed wicked-council seat would get, since the registry's `codex-acp` `AcpConfig` sets no `INITIAL_AGENT_MODE` env var and no per-session mode override. One additional capture used `INITIAL_AGENT_MODE=read-only` (`AgentMode.ReadOnly`: `approvalPolicy: "on-request"`, `approvalsReviewer: "user"`) to test whether a different built-in mode changes the outcome. |

## Capture method

The capture harness advertises the **exact** client capabilities the real wicked-core ACP client
sends (`src/acp_runner.rs:1521-1523`): `{"fs":{},"terminal":false,"permission":true}`. Advertising
`permission: true` is load-bearing for this evidence: it declares that the client ANSWERS
`session/request_permission`, so any codex-acp configuration that gated permission requests on
that capability would be forced to send them here.

A minimal ACP JSON-RPC-2.0-over-NDJSON client (`capture-harness.mjs`, in this directory) was
spawned against the installed `codex-acp` binary directly (no SDK-version coupling — the capture
reflects exactly what crosses the wire). It performs `initialize` → `session/new` (cwd = a fresh,
empty, isolated fixture directory under `tmp/`, never committed, seeded with a `seed.txt` file) →
`session/prompt` asking codex to, in order: (1) read `seed.txt`, (2) edit `seed.txt` to append a
line, (3) run a harmless shell command, (4) create a small marker file — exercising all four CORE
tool-intent classes (read/write/edit/bash) in one turn. It logs every JSON-RPC frame in both
directions verbatim to NDJSON with a timestamp and direction tag.

Five captures were taken, each in its own fresh fixture directory:

- `capture-allow.ndjson` — the four-step read/edit/bash/write turn under the DEFAULT agent mode
  (`Agent`), harness auto-approves any incoming `session/request_permission`.
- `capture-reject.ndjson` — the identical four-step turn under the DEFAULT agent mode, harness
  auto-rejects any incoming `session/request_permission`.
- `capture-readonly.ndjson` — the identical four-step turn with `INITIAL_AGENT_MODE=read-only`
  (`AgentMode.ReadOnly`), harness auto-approves.
- `probe-network.ndjson` (`probe-network.mjs`) — a single-command turn (`curl -sI
  https://example.com`) under the DEFAULT agent mode, to see whether a sandbox-denied network
  action escalates to a permission request.
- `probe-risky.ndjson` (`probe-risky.mjs`) — a single-command turn (`rm -rf` on a scratch
  subdirectory) under the DEFAULT agent mode, to see whether an action codex's own internal risk
  reviewer would plausibly flag escalates to a permission request.

See `verdict.md` for the analysis and `README.md` for what was redacted before these files were
committed.
