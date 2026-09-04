# OQ-PI-ACP-001 — provenance manifest

Captured 2026-09-04. This freezes the exact artifact this evidence set is about, per
the open question's requirement for an immutable pin (a semver range is not
sufficient).

## Adapter under test

| Field | Value |
|---|---|
| npm package | `pi-acp` |
| Resolved version | `0.0.32` |
| Repository | `https://github.com/svkozak/pi-acp.git` |
| `gitHead` (from npm registry metadata) | `2f6e3c530819489bd09a84139b0b757df6895556` |
| npm tarball URL | `https://registry.npmjs.org/pi-acp/-/pi-acp-0.0.32.tgz` |
| npm tarball integrity (sha512, base64) | `sha512-2/0dfoVhkDTHDQ0R8wwb1ykwlSJm46VEoUyMllzc9hNbEuzUleZXqUwzGScf6+GvepU/4qA4v7hRgGTLgFp5Mw==` |
| Tarball sha256 (independently computed) | `0faee4e31d75e987166d17ebf73cd970d90315877dadd73a765d1b46f716bab6` |
| Installed `dist/index.js` sha256 | `fffbdc67ce361866082b0f2ad78d64de85bfac5e4c89a1b7662a6d2785502d4a` |
| Owning dependency pin | `wicked-crew@0.7.14` → `"pi-acp": "^0.0.32"` (range, not a lockfile pin — see gap below) |

Verification performed: downloaded the tarball fresh from the npm registry URL above,
computed its sha256, and independently recomputed the published sha512 integrity
digest with `openssl dgst -sha512 | base64` — it matches the registry's `dist.integrity`
exactly. The repository was cloned at `gitHead` and its `package.json` confirms
`"version": "0.0.32"`, so the TypeScript source read in this evidence set is the exact
source the shipped `dist/index.js` was built from.

**Gap not closed by this evidence**: `wicked-crew`'s dependency is `^0.0.32`, a semver
range, not a lockfile-pinned exact version. This manifest freezes what was resolved
*today*; a future `npm install` could silently resolve `0.0.33+` without re-triggering
this evidence. That is a follow-up (pin or re-verify-on-bump), not something this
capture can resolve on its own.

## Runtime environment

| Field | Value |
|---|---|
| `pi` (agent binary) version | `0.84.2` |
| Node.js | `v26.0.0` |
| npm | `11.12.1` |
| OS | Darwin 25.5.0, arm64 (macOS) |
| pi-acp invocation | `pi-acp` (no args), stdio transport, cwd = isolated fixture directory |
| Underlying pi invocation (per `src/pi-rpc/process.ts`) | `pi --mode rpc --no-themes` (cwd = same fixture dir) |
| Model used for the captured turn | `anthropic/claude-opus-4-8` (session default; not pinned by the harness) |

## Capture method

The capture harness advertises the **exact** client capabilities the real wicked-core
ACP client sends (`src/acp_runner.rs:1521-1523`): `{"fs":{},"terminal":false,"permission":true}`.
Advertising `permission: true` is load-bearing for this evidence: it declares that the
client ANSWERS `session/request_permission`, so any pi-acp that gated permission
requests on that capability would be forced to send them here. It sends none anyway
(`requestPermissionCallCount: 0` in both runs) — confirming the negative result is a
property of the adapter, not a harness artifact. (Independently corroborated by source:
pi-acp@0.0.32 `dist/index.js` reads `clientCapabilities` only for `_meta["terminal-auth"]`,
never to gate permission, and calls `requestPermission` from exactly one site — the
extension select/confirm UI — never on the core tool path.)

A minimal ACP JSON-RPC-2.0-over-NDJSON client (`capture-harness.mjs`, in this
directory) was spawned against the installed `pi-acp` binary directly (no SDK-version
coupling — the capture reflects exactly what crosses the wire). It performs
`initialize` → `session/new` (cwd = a fresh, empty, isolated fixture directory under
`tmp/`, never committed) → `session/prompt` asking pi to use its `write` tool to create
a small marker file, and logs every JSON-RPC frame in both directions verbatim to
NDJSON with a timestamp and direction tag. Two independent runs were captured, each in
its own fresh fixture directory:

- `capture-allow.ndjson` — harness auto-approves any incoming `session/request_permission`.
- `capture-reject.ndjson` — harness auto-rejects any incoming `session/request_permission`.

See `verdict.md` for the analysis and `README.md` for what was redacted before these
files were committed.
