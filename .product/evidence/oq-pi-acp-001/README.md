# OQ-PI-ACP-001 evidence

Evidence for wicked-core issue #368 (resolving whether `pi`'s community ACP adapter,
`pi-acp`, can be admitted for ACP input-governance — i.e. whether every tool call it
makes is gated behind a real, rejectable client permission request).

- `manifest.md` — exact pinned adapter version/commit/hashes, runtime environment,
  capture method.
- `verdict.md` — the per-property analysis and final admission decision (**not
  admitted**).
- `capture-harness.mjs` — the reproducible capture harness (spawns `pi-acp` directly,
  speaks ACP JSON-RPC over stdio, logs every frame verbatim). Re-run with:
  `node capture-harness.mjs <path-to-pi-acp-binary> <empty-fixture-dir> <output.ndjson> <allow|reject>`
- `capture-allow.ndjson`, `capture-reject.ndjson` — the captured protocol frames
  (redacted; see below). The harness advertises the exact client capabilities the real
  wicked-core ACP client sends — `{"fs":{},"terminal":false,"permission":true}` — so the
  "no permission request ever arrives" result cannot be dismissed as a harness that
  simply failed to declare it answers permission prompts. See `manifest.md`.

## Redaction note

The raw captures were produced by actually spawning `pi-acp` on this machine, which
means pi's ACP `session/new` response embeds environment identity: the operator's home
directory appears in the fixture `cwd` path, in `tool_call` `locations` fields, and in a
long list of skill file paths pi's own startup-info prelude enumerates
(`~/.pi/agent/skills/...`).

Before committing, every occurrence of the absolute home directory
(`/Users/<redacted-username>`) was replaced with the placeholder `<HOME>`, and the
worktree-specific absolute path prefix was replaced with `<WORKTREE_ROOT>`, via a
byte-for-byte string substitution over the full NDJSON text (see the redaction step
that produced these files — a plain Python string `.replace()`, verified afterwards by
grepping the output for the raw username with zero matches). No other classes of data
were present to redact: no gh account handles, no API keys/tokens, and no other
credentials appear anywhere in these captures — the harness never sent or received any
(pi's own model auth happens out-of-band via already-configured local credentials and
never crosses the ACP stdio channel captured here).

The unredacted raw captures remain only in this worktree's `tmp/oq-pi-acp-001/` (never
staged, never committed).
