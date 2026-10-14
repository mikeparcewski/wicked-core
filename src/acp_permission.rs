//! The ACP carrier for input governance (FINDING-062).
//!
//! # Why this exists
//!
//! A governed unit has to be gated on every tool call. The wrapped path does that with a Claude
//! `PreToolUse` hook: `wicked-core gate-hook` is spawned per call, reads `{tool_name, tool_input}`
//! on stdin, and answers with an exit code.
//!
//! The ACP path has no such subprocess. The bridge does not run `claude` as a child at all — it
//! drives `@anthropic-ai/claude-agent-sdk` in-process with a `canUseTool` callback, and surfaces
//! that callback to whoever is driving it as a `session/request_permission` REQUEST. That is why
//! passing `--settings` was inert: the bridge parses four flags and discards the rest, and even if
//! it forwarded them there is no CLI process for a settings file to reach.
//!
//! So the carrier the bridge honours is the ACP protocol itself. This module answers those
//! requests using the SAME policy, and writing the SAME audit records, as the hook.
//!
//! # Why the audit trail is part of the answer
//!
//! [`crate::gate_hook::evaluate_tool_call`] does not merely decide. It writes the hook-fired
//! liveness sentinel and appends a durable `ConformanceClaim` per call, and `fold_input_denial`
//! DENIES a unit whose claims exist without that sentinel — the signature of a suppressed hook.
//! A carrier that returned allow/deny without those records would be rejected downstream for
//! looking bypassed. Sharing that function is what makes the two carriers indistinguishable to the
//! fold, which is the property that matters: governance must not depend on which transport ran.
//!
//! # Fail-closed
//!
//! Every ambiguity here denies. An unparseable request, a missing tool name, an options list with
//! no reject choice — none of them are reasons to let a tool call through on a governed unit. The
//! wrapped path already takes this position (an unreadable payload is "UN-EVALUABLE — fail closed,
//! never allow"); this path takes the same one.

#[cfg(test)]
mod tests {}
