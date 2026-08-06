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

use serde_json::{json, Value};

/// The run-scoped facts an ACP permission decision needs — the same four the wrapped path puts in
/// `WICKED_GATE_*` env vars, plus the decisions log the hook resolves from `WICKED_DECISIONS_PATH`.
pub(crate) struct AcpGate<'a> {
    pub scope: &'a str,
    pub phase: &'a str,
    pub phase_alias: Option<&'a str>,
    pub db: Option<&'a str>,
    pub decisions_path: &'a str,
}

/// ACP permission option kinds, per the protocol's `PermissionOption.kind`.
const ALLOW_KINDS: [&str; 2] = ["allow_once", "allow_always"];
const REJECT_KINDS: [&str; 2] = ["reject_once", "reject_always"];

/// Rewrite an ACP `session/request_permission` params object into the Claude `PreToolUse` shape.
///
/// Deliberately a translation rather than a second parser: the resulting value goes through
/// [`crate::gate_hook::claude_pretool_context`], so both carriers derive the evaluation context
/// from ONE piece of code. A separate ACP-shaped context builder would be a second definition of
/// "what a tool call means to a policy", and the two would drift the first time a tool grew a
/// field — the defect class this campaign keeps filing.
///
/// Returns `None` when the request carries no usable tool name, which the caller treats as a deny.
pub(crate) fn pretool_payload(params: &Value) -> Option<(String, Value)> {
    // The bridge sends the tool name at the top level and the arguments under `toolCall.rawInput`.
    // `rawInput` is the tool's own argument object — the same thing Claude's hook calls
    // `tool_input` — so the two line up field-for-field.
    let tool = params
        .get("toolName")
        .and_then(Value::as_str)
        .or_else(|| params.pointer("/toolCall/title").and_then(Value::as_str))
        .filter(|s| !s.is_empty())?
        .to_string();
    let input = params
        .pointer("/toolCall/rawInput")
        .cloned()
        .unwrap_or(Value::Null);
    Some((
        tool.clone(),
        json!({ "tool_name": tool, "tool_input": input }),
    ))
}

/// Pick the `optionId` to answer with, from the options the agent offered.
///
/// Selects BY KIND rather than by id. The bridge's own ids (`allow`, `allow_always`, `acceptEdits`,
/// `auto`, …) are its business and can change; `kind` is the protocol's vocabulary. Hardcoding ids
/// would make this silently answer the wrong thing after a bridge upgrade — and answering the
/// wrong thing here means allowing a call the policy denied.
///
/// `None` when the agent offered nothing of the required kind, which the caller escalates to a
/// cancelled outcome rather than guessing.
pub(crate) fn choose_option(options: &Value, allow: bool) -> Option<String> {
    let wanted: &[&str] = if allow { &ALLOW_KINDS } else { &REJECT_KINDS };
    let opts = options.as_array()?;
    // Prefer the "once" variant: a governed run re-evaluates every call, and an `_always` answer
    // asks the agent to stop consulting us — which would turn one allow into a standing grant and
    // silently unhook the gate for the rest of the turn.
    for kind in wanted {
        if let Some(id) = opts.iter().find_map(|o| {
            (o.get("kind").and_then(Value::as_str) == Some(kind))
                .then(|| o.get("optionId").and_then(Value::as_str))
                .flatten()
        }) {
            return Some(id.to_string());
        }
    }
    None
}

/// The JSON-RPC `result` for one `session/request_permission`.
///
/// Evaluates the call through the shared gate, records it, and answers. Any failure to understand
/// the request produces a cancelled outcome, which the agent treats as "not permitted" — the
/// fail-closed direction.
pub(crate) fn permission_result(gate: &AcpGate<'_>, params: &Value) -> (Value, bool) {
    let Some((tool, payload)) = pretool_payload(params) else {
        return (cancelled("unparseable permission request"), false);
    };
    let payload_raw = payload.to_string();
    let (context, tool_name) =
        crate::gate_hook::claude_pretool_context(&payload_raw, gate.scope, gate.phase);
    let allowed = crate::gate_hook::evaluate_tool_call(
        gate.scope,
        gate.phase,
        gate.phase_alias,
        gate.db,
        gate.decisions_path,
        &context,
        &tool_name,
    ) == 0;

    match choose_option(params.get("options").unwrap_or(&Value::Null), allowed) {
        Some(option_id) => (
            json!({"outcome": {"outcome": "selected", "optionId": option_id}}),
            allowed,
        ),
        // The policy reached a verdict but the agent offered no way to express it. Cancel rather
        // than fall back to any option that happens to exist: on a deny, picking the wrong one
        // permits the call.
        None => (
            cancelled(&format!(
                "no {} option offered for `{tool}`",
                if allowed { "allow" } else { "reject" }
            )),
            false,
        ),
    }
}

/// The answer for an UNGOVERNED turn: permitted.
///
/// Ungoverned units have always been allowed to call tools on this path — there was no gate. What
/// changes is that the permission is now stated rather than obtained by withholding the client
/// capability so the agent never asked. Saying it explicitly is what lets the governed case exist
/// at all: the capability has to be advertised per-session, and sessions are shared across units.
///
/// Still fail-closed on a malformed request: an ungoverned unit is not a licence to answer a
/// question we could not read.
pub(crate) fn allow_result(params: &Value) -> Value {
    match choose_option(params.get("options").unwrap_or(&Value::Null), true) {
        Some(option_id) => json!({"outcome": {"outcome": "selected", "optionId": option_id}}),
        None => cancelled("no allow option offered"),
    }
}

fn cancelled(_why: &str) -> Value {
    json!({"outcome": {"outcome": "cancelled"}})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> Value {
        json!([
            {"optionId": "allow", "name": "Allow", "kind": "allow_once"},
            {"optionId": "allow_always", "name": "Always", "kind": "allow_always"},
            {"optionId": "reject", "name": "Reject", "kind": "reject_once"},
            {"optionId": "reject_always", "name": "Never", "kind": "reject_always"},
        ])
    }

    #[test]
    fn options_are_chosen_by_kind_not_by_id() {
        assert_eq!(choose_option(&opts(), true).as_deref(), Some("allow"));
        assert_eq!(choose_option(&opts(), false).as_deref(), Some("reject"));
    }

    /// The bridge's ids are its own; only `kind` is protocol vocabulary. A rename must not flip an
    /// answer, because flipping a deny means permitting the call.
    #[test]
    fn renamed_ids_still_resolve() {
        let renamed = json!([
            {"optionId": "yes-please", "kind": "allow_once"},
            {"optionId": "absolutely-not", "kind": "reject_once"},
        ]);
        assert_eq!(choose_option(&renamed, true).as_deref(), Some("yes-please"));
        assert_eq!(
            choose_option(&renamed, false).as_deref(),
            Some("absolutely-not")
        );
    }

    /// `_once` must win over `_always`: an `_always` answer tells the agent to stop asking, which
    /// converts one decision into a standing grant for the rest of the turn.
    #[test]
    fn the_once_variant_is_preferred_over_always() {
        let reversed = json!([
            {"optionId": "always", "kind": "allow_always"},
            {"optionId": "once", "kind": "allow_once"},
        ]);
        assert_eq!(choose_option(&reversed, true).as_deref(), Some("once"));
    }

    #[test]
    fn a_missing_kind_yields_nothing_rather_than_a_guess() {
        let vague = json!([{"optionId": "ok", "name": "OK"}]);
        assert!(choose_option(&vague, true).is_none());
        assert!(choose_option(&vague, false).is_none());
        assert!(choose_option(&Value::Null, false).is_none());
    }

    #[test]
    fn acp_params_translate_into_the_pretool_shape() {
        let params = json!({
            "sessionId": "s1",
            "toolName": "Bash",
            "toolCall": {"toolCallId": "t1", "rawInput": {"command": "rm -rf /"}},
        });
        let (tool, payload) = pretool_payload(&params).expect("translates");
        assert_eq!(tool, "Bash");
        assert_eq!(payload["tool_name"], "Bash");
        assert_eq!(payload["tool_input"]["command"], "rm -rf /");

        // …and the SHARED context builder must read it, which is the whole point of translating
        // rather than writing a second parser.
        let (context, name) =
            crate::gate_hook::claude_pretool_context(&payload.to_string(), "unit", "build");
        assert_eq!(name, "Bash");
        assert_eq!(context["command"], "rm -rf /");
    }

    #[test]
    fn a_request_without_a_tool_name_is_not_evaluable() {
        assert!(pretool_payload(&json!({"sessionId": "s1"})).is_none());
        assert!(pretool_payload(&json!({"toolName": ""})).is_none());
    }
}
