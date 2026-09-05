# OQ-COPILOT-ACP-001 evidence

Evidence for wicked-core issue #369 (resolving whether GitHub Copilot CLI's native ACP mode,
`copilot --acp`, can be admitted for ACP input-governance — i.e. whether every tool call it makes is
gated behind a real, rejectable client permission request).

- `manifest.md` — candidate-adapter identification, exact pinned CLI version/hash, runtime
  environment, capture method.
- `verdict.md` — the per-property analysis and the recommended admission decision. **This is a
  CLARIFY-phase deliverable: it records the evidence and a recommended verdict but does not modify
  `crates/wicked-council/src/registry.rs`. That registry edit (if the recommendation is accepted) is
  production code and belongs to a later phase.**
- `capture-harness.mjs` — the reproducible capture harness for the four-step read/edit/bash/write
  turn (allow, reject, and `--allow-all-tools` scenarios). Re-run with:
  `node capture-harness.mjs copilot <empty-fixture-dir-seeded-with-seed.txt> <output.ndjson> <allow|reject> ["extra --flags"]`
- `probe-outside-read.mjs` — probes whether a read of a path outside the session cwd escalates to a
  permission request. Re-run with: `node probe-outside-read.mjs copilot <fixture-cwd>
  <path-outside-fixture-cwd> <output.ndjson>`
- `probe-risky.mjs` / `probe-network.mjs` — single-command probes (`rm -rf`, `curl`) used to check
  whether a destructive or network action escalates to a permission request or is resolved
  internally the way codex's `auto_review` did. Re-run with: `node probe-risky.mjs copilot
  <fixture-dir-containing-sub/> <output.ndjson>` (same signature for `probe-network.mjs`, no `sub/`
  needed).
- `capture-allow.ndjson`, `capture-reject.ndjson`, `capture-allow-all-tools.ndjson`,
  `probe-outside-read.ndjson`, `probe-risky.ndjson`, `probe-network.ndjson` — the captured protocol
  frames (redacted; see below). Every capture advertises the exact client capabilities the real
  wicked-core ACP client sends — `{"fs":{},"terminal":false,"permission":true}` — so results here
  cannot be dismissed as a harness that failed to declare it answers permission prompts. See
  `manifest.md`.

## Redaction note

The raw captures were produced by actually spawning the installed `copilot` binary (which uses the
operator's pre-existing local GitHub Copilot CLI login), which means `session/new`/`session/prompt`
params, `tool_call` `rawInput`/`locations` fields, and diff bodies embed environment identity: the
operator's home directory and this worktree's absolute path appear throughout.

Before committing, every occurrence of the worktree-specific absolute path prefix
(`/Users/<redacted-username>/Projects/wicked/wicked-core/wicked-worktrees/243ab76b-.../`) was
replaced with `<WORKTREE_ROOT>`, and every occurrence of the operator's absolute home directory
(`/Users/<redacted-username>`) was replaced with `<HOME>`, via plain Python string `.replace()` over
the full NDJSON text, verified afterwards by grepping the output for the raw username and for
`/Users/` with zero matches remaining. No other classes of data were present to redact: a full-text
scan for email-address-shaped strings (`grep -oE '[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}'`)
across every raw capture found zero matches, and no gh account handles, API keys, tokens, or other
credentials appear anywhere in these captures (Copilot's own model/GitHub auth happens out-of-band
via the already-configured local login and never crosses the ACP stdio channel captured here). No
`available_commands_update`-style large context dump was observed in any capture (unlike
`codex-acp`), so no size-elision was needed — every capture is committed verbatim modulo the two
path substitutions above.

The unredacted raw captures, harness output logs, and scratch fixture directories remain only in
this worktree's `tmp/oq-copilot-acp-001/` (never staged, never committed; `tmp/` is gitignored at
the repo root).
