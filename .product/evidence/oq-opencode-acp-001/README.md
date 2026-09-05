# OQ-OPENCODE-ACP-001 evidence

Evidence for wicked-core issue #370 (resolving whether opencode's native ACP mode, `opencode acp`,
can be admitted for ACP input-governance — i.e. whether every tool call it makes is gated behind a
real, rejectable client permission request).

- `manifest.md` — candidate-adapter identification, exact pinned CLI version/hash/source commit,
  runtime environment, capture method, and the methodological note explaining why a second
  ("strict") fixture variant was captured alongside the registry's literal invocation.
- `verdict.md` — the per-property analysis and the recommended admission decision. **This is a
  CLARIFY-phase deliverable: it records the evidence and a recommended verdict but does not modify
  `crates/wicked-council/src/registry.rs`. The run's later implementation phase applied the comment-only registry update citing this verdict — both land in this PR.**
- `capture-harness.mjs` — the reproducible capture harness for the four-step read/edit/bash/write
  turn (allow and reject scenarios, against either a plain or a `permission`-configured fixture).
  Re-run with:
  `node capture-harness.mjs opencode <empty-fixture-dir-seeded-with-seed.txt> <output.ndjson> <allow|reject>`
- `probe-outside-read.mjs` — probes a read of a path outside the session cwd. Re-run with:
  `node probe-outside-read.mjs opencode <fixture-cwd> <path-outside-fixture-cwd> <output.ndjson>`
- `probe-risky.mjs` / `probe-network.mjs` — single-command probes (`rm -rf`, `curl`) under the
  default (no project config) invocation, auto-approving any incoming request. Re-run with:
  `node probe-risky.mjs opencode <fixture-dir-containing-sub/> <output.ndjson>` (same signature for
  `probe-network.mjs`, no `sub/` needed).
- `probe-strict-reject-bash.mjs` — a single-command `rm -rf` probe, identical to `probe-risky.mjs`
  except every incoming permission request is auto-**rejected**; only meaningful against a fixture
  whose `opencode.json` sets `bash: "ask"` (the default invocation never asks in the first place).
  Re-run with: `node probe-strict-reject-bash.mjs opencode <fixture-dir-containing-sub/-and-opencode.json> <output.ndjson>`
- `capture-allow.ndjson`, `capture-reject.ndjson`, `probe-risky.ndjson`, `probe-network.ndjson`,
  `probe-outside-read.ndjson`, `capture-strict-allow.ndjson`, `capture-strict-reject.ndjson`,
  `probe-strict-reject-bash.ndjson` — the captured protocol frames (redacted; see below). Every
  capture advertises the exact client capabilities the real wicked-core ACP client sends —
  `{"fs":{},"terminal":false,"permission":true}` — so results here cannot be dismissed as a harness
  that failed to declare it answers permission prompts. See `manifest.md`.

## Redaction note

The raw captures were produced by actually spawning the installed `opencode` binary (which uses the
operator's pre-existing local opencode provider login), which means `session/new`/`session/prompt`
params, `tool_call` `rawInput`/`locations` fields, diff bodies, and (unexpectedly — see below) the
model's own streamed reasoning text embed environment identity: the operator's home directory and
this worktree's absolute path appear throughout.

Two redaction passes were applied before committing, both via a Python script operating on the full
NDJSON text/structure (not manual editing):

1. **Whole-path substitution.** Every occurrence of the worktree-specific absolute path prefix was
   replaced with `<WORKTREE_ROOT>`, and every occurrence of the operator's absolute home-directory
   prefix was replaced with `<HOME>`. This covers every `rawInput`, `locations`, and diff body field,
   since ACP sends those as complete JSON string values in a single frame.
2. **Streamed-narration elision.** Unlike the structural tool-call fields, opencode's ACP mode streams
   the model's own reasoning/response text token-by-token across many `agent_thought_chunk`/
   `agent_message_chunk` frames. In one probe (`probe-strict-reject-bash.ndjson`) the model narrated
   a file path in its reasoning, and the character stream happened to split the operator's home
   directory string across multiple delta frames (e.g. one frame ending `` `/Users/m` `` and the next
   beginning `ichael.parcew`, then `ski/Projects/w`, etc.) — a whole-string substitution over each
   individual frame's text cannot catch a secret that is fragmented *across* frames. Rather than rely
   on a fragile fragment-matching regex, **every** `agent_thought_chunk`/`agent_message_chunk` frame's
   `content.text` value, in every one of the eight captures, was replaced wholesale with a fixed
   elision placeholder before the whole-path substitution ran. This is a blanket, not a
   targeted, redaction: it applies uniformly regardless of whether a given chunk actually contained
   anything sensitive, specifically because the fragmentation risk cannot be ruled out chunk-by-chunk.
   The evidentiary content of every capture — `session/request_permission` requests, `tool_call`/
   `tool_call_update` frames with their `rawInput`/`locations`/`kind`/`title`, and the harness's own
   `harness-meta` summaries — is **not** narration and is unaffected; only the model's free-text
   commentary is elided.

After both passes, a full-text scan for the operator's username, surname, and home-directory prefix
(`grep -oE 'michael[a-z._]*|parcewski|/Users/[A-Za-z._-]*|211485d2[0-9a-f-]*'`) across every committed
file found zero matches, and a separate scan for email-address-shaped strings
(`grep -oE '[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}'`) also found zero matches. No gh/opencode
account handles, API keys, tokens, or other credentials appear anywhere in these captures — opencode's
own model-provider auth happens out-of-band via the already-configured local credential store and
never crosses the ACP stdio channel captured here. The commit-author identity used for the fixtures'
throwaway internal `git init` (`oq-evidence@example.invalid`) is a placeholder invented for this
evidence, not a real address, and is intentionally left unredacted since it identifies nothing.

Some captures are large (`probe-strict-reject-bash.ndjson` in particular, ~150 frames) because the
elision placeholder is verbose and repeated once per streamed chunk rather than collapsed — this
preserves an honest frame-for-frame, one-line-per-wire-message record (matching every other capture in
this and prior evidence directories) at the cost of some redundant boilerplate; no frames were merged
or dropped.

The unredacted raw captures, harness output logs (`*.stderr.log`), and scratch fixture directories
(including their disposable nested `.git` repos) remain only in this worktree's
`tmp/oq-opencode-acp-001/` (never staged, never committed; `tmp/` is gitignored at the repo root).
