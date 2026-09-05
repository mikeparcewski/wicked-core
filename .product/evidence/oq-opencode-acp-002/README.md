# OQ-OPENCODE-ACP-PROVISION-001 evidence

Evidence resolving whether wicked-core can provision opencode's ACP mode into governance without
mutating any repo-committed tracked file (DES-INPUT-GOV-006, umbrella #360). Re-proves
`oq-opencode-acp-001`'s properties against the config the engine now actually ships
(`OPENCODE_CONFIG_CONTENT`), rather than the registry's previously-unconfigured invocation.

- `manifest.md` — pinned artifact (unchanged from oq-opencode-acp-001), the mechanism under test,
  fixture conventions, and the cross-validation files' scope/limits.
- `verdict.md` — the per-property analysis and the admission conclusion this evidence supports.
- `capture-cc-*.ndjson`, `probe-cc-*.ndjson` — captures under the ACTUAL shipped mechanism
  (`OPENCODE_CONFIG_CONTENT`, `cc` = "config content"). These are the evidence
  `acp_input_governance: true` for opencode in `crates/wicked-council/src/registry.rs` cites.
- `xval-envperm-*.ndjson` — cross-validation captures under a second, undocumented env var
  (`OPENCODE_PERMISSION`). NOT the shipped mechanism; corroboration only — see `manifest.md`'s
  "Cross-validation files" section for exactly which properties each one covers and the one stated
  gap (outside-workspace-read not yet re-captured under `OPENCODE_CONFIG_CONTENT` byte-for-byte).
- No new harness `.mjs` scripts — every capture reuses `../oq-opencode-acp-001/*.mjs` verbatim
  (only the environment differs); see `manifest.md` for exact re-run commands.

## Redaction

Two passes, identical method to `oq-opencode-acp-001/README.md`:

1. **Whole-path substitution.** The worktree's absolute path → `<WORKTREE_ROOT>`; the operator's home
   directory → `<HOME>`; every per-fixture `mktemp -d` temp root (`/var/folders/.../T/tmp.XXXXXXXX`)
   → `<FIXTURE_ROOT>`; the outside-workspace probe's scratch directory → `<OUTSIDE_DIR>`.
2. **Streamed-narration elision.** Every `agent_thought_chunk`/`agent_message_chunk` frame's
   `content.text`, in every capture, replaced wholesale with `<ELIDED-NARRATION>` — a blanket, not a
   targeted, redaction, for the same fragmentation reason `oq-opencode-acp-001/README.md` records
   (a secret split across streamed delta frames cannot be caught by per-frame pattern matching).

After both passes, this directory was scanned for:
- the operator's username, surname, and home-directory prefix (`grep -roE
  'michael[a-z._]*|parcewski|/Users/[A-Za-z._-]*'`) — **zero matches**.
- email-address-shaped strings (`grep -roE '[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}'`) —
  **zero matches**.
- leaked `mktemp`-style fixture paths (`grep -roE '/var/folders/[A-Za-z0-9_./-]*tmp\.[A-Za-z0-9]+'`)
  — **zero matches**.

Every `agent_thought_chunk`/`agent_message_chunk` frame carries the elision placeholder — verified by
counting narration-frame occurrences and `<ELIDED-NARRATION>` occurrences per file and confirming
they match exactly (they do, in all twelve files). No gh/opencode account handles, API keys, tokens,
or other credentials appear anywhere in these captures — same as oq-opencode-acp-001, opencode's own
model-provider auth happens out-of-band and never crosses the captured ACP stdio channel. The
placeholder commit-author identity used for every fixture's throwaway internal `git init`
(`oq-evidence@example.invalid`) is an invented address, not a real one, and is intentionally left
unredacted since it identifies nothing.

Unredacted raw captures existed only in this operator's local `/tmp/oq2-*.ndjson` scratch during this
capture session — outside the worktree, never staged, not preserved after this evidence was written.
