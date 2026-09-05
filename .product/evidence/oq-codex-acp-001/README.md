# OQ-CODEX-ACP-001 evidence

Evidence for wicked-core issue #367 (resolving whether `codex`'s ACP adapter,
`@agentclientprotocol/codex-acp`, can be admitted for ACP input-governance — i.e. whether every
tool call it makes is gated behind a real, rejectable client permission request).

- `manifest.md` — exact pinned adapter version/commit/hashes, runtime environment, capture method.
- `verdict.md` — the per-property analysis and final admission decision (**not admitted**).
- `capture-harness.mjs` — the reproducible capture harness for the four-step read/edit/bash/write
  turn (allow and reject scenarios, default and read-only agent modes). Re-run with:
  `node capture-harness.mjs <path-to-codex-acp-binary> <empty-fixture-dir-seeded-with-seed.txt> <output.ndjson> <allow|reject>`
  (set `INITIAL_AGENT_MODE=read-only` in the environment to exercise `AgentMode.ReadOnly` instead
  of the default).
- `probe-network.mjs` / `probe-risky.mjs` — smaller single-command probes (a sandbox-denied network
  request, an `rm -rf`) used to check whether a denied or explicitly risky action escalates to a
  permission request. Re-run with: `node probe-network.mjs <codex-acp-bin> <fixture-dir>
  <output.ndjson>` (same signature for `probe-risky.mjs`, fixture dir must contain a `sub/`
  subdirectory to delete).
- `capture-allow.ndjson`, `capture-reject.ndjson`, `capture-readonly.ndjson`, `probe-network.ndjson`,
  `probe-risky.ndjson` — the captured protocol frames (redacted; see below). Every capture
  advertises the exact client capabilities the real wicked-core ACP client sends —
  `{"fs":{},"terminal":false,"permission":true}` — so the "no permission request ever arrives"
  result cannot be dismissed as a harness that simply failed to declare it answers permission
  prompts. See `manifest.md`.

## Size note

Every capture's `session/new` handshake triggers a large `available_commands_update` notification
(codex-acp's full locally-installed skills/commands catalog dump, ~95-105KB per capture,
unrelated to permission-request analysis). That one frame per file has been replaced with a short
elision marker (`_elided: "ELIDED for evidence size — ... (<N> bytes original)"`) so each file stays
reviewable; every other frame — every `tool_call`/`tool_call_update`, every `session/request_permission`
(there were none — that is the finding), and the harness's summary — is verbatim.

## Redaction note

The raw captures were produced by actually spawning `codex-acp` (which spawns the real, locally
authenticated `codex` CLI) on the evidence machine, which means the session responses and shell
command output embed environment identity: the operator's home directory and this worktree's
absolute path appear in `session/new` params, `tool_call` `rawInput`/`locations` fields, shell
command output (e.g. `pwd`, `ls -l` output lines), and codex's own startup skills-context listing.

Before committing, every occurrence of:

- the worktree-specific absolute path prefix was replaced with `<WORKTREE_ROOT>`,
- the operator's absolute home directory was replaced with `<HOME>`,
- the authenticated codex account's email address (surfaced by the adapter's
  `_auth/status_update` frame) was replaced with `<redacted-account-email>`,
- the operator's local account username (which additionally appeared as the file-owner column in
  one captured `ls -ld` command's output, independent of any path) was replaced with
  `<redacted-username>`,

via plain Python string `.replace()` over the full NDJSON text, verified afterwards by grepping the
output for the raw username with zero matches. No other classes of data were present to redact: no
gh account handles beyond the OS username already covered above, no API keys/tokens, and no other
credentials appear anywhere in these captures — the harness never sent or received any (codex's own
model auth happens out-of-band via the already-configured local ChatGPT login and never crosses the
ACP stdio channel captured here). The captures do include the names/descriptions of locally
installed wicked-garden skills, surfaced by codex's own startup context; these are non-sensitive
tooling metadata, not personal or credential data, and were left as-is.

The unredacted raw captures remain only in this worktree's `tmp/oq-codex-acp-001/` (never staged,
never committed; `tmp/` is gitignored at the repo root).
