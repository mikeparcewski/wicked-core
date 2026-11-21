# Refusal fixtures (core#1612, core#1069)

The Codex and Claude Code refusal wording is the CLIs' own: read from the message templates in
the codex 0.166.0 and claude-code 2.1.311 release binaries. They are rendered from those
templates, not live provider captures: a live capture means provoking a provider refusal on a
real account, which the tests must never need.

- `codex-0.166.0-cyber.jsonl`: codex's own sentence ("This request has been flagged for possible
  cybersecurity risk.") in the `codex exec --json` `error` and `turn.failed` events that
  `CodexJsonl` reads. The thread id is synthetic.
- `claude-2.1.311-safeguards.txt`: the refusal Claude Code prints when it knows the model
  (`API Error: <model>'s safeguards flagged this message (...)`), rendered with an example model
  name.
- `claude-2.1.311-cyber.txt`: the cyber-category refusal (`API Error: This model's safeguards
  flagged this message. Our intentionally broad safeguards ...`).
- `claude-2.1.311-session.txt`: the template that names the session
  (`API Error: <model>'s safeguards flagged this session (...). You may be seeing this for the first
  time: ... Claude Code can't respond to your last message with <model>.`). 2.1.311 assembles it at
  runtime: its `w6o` builds `` `${e}'s safeguards flagged ${Eb}${s}. ${h(e)}` `` with
  `Eb="this session"`, `h` is the category's first-time note (`wc.cyber` here), and the refusal
  handler appends `` `${He} can't respond to your last message with ${b}.` ``. That is why no literal
  "flagged this session" string is in the binary.
- `claude-2.1.311-refusal.stream.jsonl`: the same refusal in `--output-format stream-json`,
  after earlier assistant text: an assistant event carrying the `API Error:` text, then a
  `result` with `is_error: true`. The test also drops the assistant error event, so the
  refusal arrives only in the failed result.
- `claude-2.1.311-cant-help.txt`: the refusal with no model name (`API Error: Claude can't help
  with this. Start a new session to continue.`).
- `echoed-review.txt`: a failed review that quotes every signature as subject matter (source
  lines, a Markdown bullet, quoted and mid-sentence text) and then fails for an unrelated reason.
  It must never classify as a refusal.
- `pi-out-of-extra-usage.txt`: a captured transcript, verbatim: pi's failed judge turn in run
  `290a7fee` (the judge-unavailable gate of core#1069), including the launcher lines under it.

The stderr-only case needs no fixture file: the wrapped-runner test's stub CLI prints progress on
stdout, a Claude refusal on stderr, and exits 1 on an ordinary unit.
