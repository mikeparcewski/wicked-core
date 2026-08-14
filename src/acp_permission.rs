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
    /// The unit's filesystem boundary (core#260). This carrier evaluates IN-PROCESS, so the env
    /// vars the wrapped path arms would read the DAEMON's environment — never set → no boundary,
    /// which is exactly the asymmetry core#260 closes. `None` preserves that (boundary-less)
    /// behavior only for callers that genuinely have no unit filesystem, e.g. tests of pure
    /// policy evaluation; the runner always supplies it for governed units.
    pub boundary: Option<crate::gate_hook::BoundaryCtx>,
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
    // The bridge sends the tool name at the top level (`toolName`) and the arguments under
    // `toolCall.rawInput`. `rawInput` is the tool's own argument object — the same thing
    // Claude's hook calls `tool_input` — so the two line up field-for-field.
    //
    // Fallback chain (FINDING-100 / core#100): some bridge versions (and some tool types, such as
    // MCP tools surfaced through the estate server) omit the top-level `toolName` field and carry
    // the canonical tool name only in `toolCall.name`. `toolCall.title` is a human-readable
    // per-call description (e.g. "Reading /tmp/foo") — NOT the canonical name — so it is the last
    // resort and must not substitute for `toolCall.name` when the latter is present.
    // Without the `toolCall.name` step this function returned `None`, causing `permission_result`
    // to answer `cancelled` (deny) with no governance record, silently blocking legitimate calls.
    // Empty strings at any step must not short-circuit the fallback — an explicit `"toolName": ""`
    // is semantically absent and must fall through to `toolCall.name` / `toolCall.title`.
    let tool = params
        .get("toolName")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            params
                .pointer("/toolCall/name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            params
                .pointer("/toolCall/title")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })?
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
        gate.boundary.as_ref(),
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

    /// THE END-TO-END PROOF, and the reason this module exists.
    ///
    /// Everything else here is structural — the handler is wired, the capability advertised, the
    /// marker written. Structural wiring is exactly what "looks governed" means. This asserts the
    /// claim itself: a governed ACP permission request for a tool a policy DENIES comes back as a
    /// refusal AND leaves a durable ConformanceClaim, using the same store, the same policy engine
    /// and the same append-only log as the wrapped path's hook.
    ///
    /// Without this the reroute should not have been removed.
    #[test]
    fn a_governed_acp_request_is_denied_by_policy_and_recorded() {
        use wicked_apps_core::open_store;
        use wicked_governance::{register_policy, Effect, Policy, Severity, Trigger};

        let dir = std::env::temp_dir().join(format!("wicked-acpgate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("gov.db");
        let decisions = dir.join("decisions.jsonl");

        let mut store = open_store(Some(db.to_str().unwrap())).unwrap();
        // Fires on the tool name, which `claude_pretool_context` puts into the evaluated context.
        register_policy(
            &mut store,
            &Policy {
                id: "pol-deny-bash".to_string(),
                kind: "test".to_string(),
                applies_to: vec!["unit-1".to_string()],
                effect: Effect::Deny,
                trigger: Trigger {
                    contains: Some("rm -rf".to_string()),
                },
                obligations: vec![],
                criteria: "no destructive shell".to_string(),
                severity: Severity::High,
                rule: "Deny destructive shell commands.".to_string(),
                retired: false,
            },
        )
        .unwrap();
        drop(store);

        let gate = AcpGate {
            scope: "unit",
            phase: "unit-1",
            phase_alias: None,
            db: Some(db.to_str().unwrap()),
            decisions_path: decisions.to_str().unwrap(),
            boundary: None, // pure policy-evaluation test — no unit filesystem
        };
        let params = json!({
            "sessionId": "s1",
            "toolName": "Bash",
            "toolCall": {"toolCallId": "t1", "rawInput": {"command": "rm -rf /"}},
            "options": [
                {"optionId": "allow", "kind": "allow_once"},
                {"optionId": "reject", "kind": "reject_once"},
            ],
        });

        let (result, allowed) = permission_result(&gate, &params);

        assert!(!allowed, "a policy-denied tool call must not be permitted");
        assert_eq!(
            result["outcome"]["optionId"], "reject",
            "the agent must be told to refuse, not merely told nothing: {result}"
        );

        // …and it is DURABLE. A refusal the audit cannot see is a refusal the fold cannot verify.
        let log = std::fs::read_to_string(&decisions).expect("the decisions log must exist");
        // Assert the SPECIFIC claim, not a substring that any prose could satisfy: the decision
        // is a deny AND it names the policy that produced it. A log containing the word somewhere
        // would prove nothing about what was recorded.
        assert!(
            log.contains(r#""decision":"deny""#),
            "no Deny claim was appended: {log}"
        );
        assert!(
            log.contains("pol-deny-bash"),
            "the claim does not name the policy that denied, so the record cannot be audited: {log}"
        );
        assert!(
            log.contains(r#""_wicked_tool_call":"Bash""#),
            "the tool-call annotation is missing, so the claim cannot be tied to a call: {log}"
        );
        // The liveness sentinel proves the gate RAN for this phase. `fold_input_denial` denies a
        // unit whose claims exist without it, so a carrier that skipped this would be rejected
        // downstream even when it answered correctly.
        assert!(
            log.contains("unit-1"),
            "the hook-fired sentinel for the phase is missing: {log}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// core#260 — THE ASYMMETRY CLOSED. The ACP carrier evaluates in-process, where the env vars
    /// the wrapped launcher arms are never set, so `boundary_denial` answered "no boundary
    /// configured" and a governed ACP unit could write ANYWHERE — including the gate pin that
    /// FINDING-098 is about. This proves the explicit `BoundaryCtx` carrier: the SAME governed
    /// Write is denied outside the declared roots (with a durable boundary claim) and permitted
    /// inside them, with no policy involved — the boundary is judged BEFORE policy.
    #[test]
    fn a_governed_acp_write_outside_the_boundary_is_denied_and_inside_is_allowed() {
        let dir = std::env::temp_dir().join(format!("wicked-acpbnd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sandbox = dir.join("sandbox");
        let inbox = dir.join("inbox"); // the launcher-declared extra write root
        std::fs::create_dir_all(&sandbox).unwrap();
        std::fs::create_dir_all(&inbox).unwrap();
        // A path OUTSIDE the roots AND outside the system temp: the scratch carve-out (core#264)
        // downgrades temp-located escapes to the advisory claim, and this arm proves the FATAL
        // one. Derive it from the temp dir's own filesystem root so it is absolute on every
        // platform (a bare `/x` is not absolute on Windows and would resolve INSIDE the cwd).
        let fs_root = std::env::temp_dir()
            .ancestors()
            .last()
            .expect("every path has a root")
            .to_path_buf();
        let outside = fs_root.join("wicked-nonexistent-outside").join("evil.html");
        let decisions = dir.join("decisions.jsonl");
        // A REAL (empty) store for the allow arm: after the boundary passes, evaluation
        // proceeds to policy selection, and an unresolvable store is an infra-deny — which is
        // fail-closed and correct, but not what this test is about. (The deny arm never reaches
        // the store: boundary is judged first, which the first arm also proves.)
        let db = dir.join("gov.db");
        drop(wicked_apps_core::open_store(Some(db.to_str().unwrap())).unwrap());

        let request = |path: &std::path::Path| {
            json!({
                "sessionId": "s1",
                "toolName": "Write",
                "toolCall": {"toolCallId": "t1", "rawInput": {
                    "file_path": path.to_str().unwrap(), "content": "x"}},
                "options": [
                    {"optionId": "allow", "kind": "allow_once"},
                    {"optionId": "reject", "kind": "reject_once"},
                ],
            })
        };
        let boundary = || {
            Some(crate::gate_hook::BoundaryCtx {
                roots: crate::path_policy::AllowedRoots {
                    write: vec![sandbox.clone(), inbox.clone()],
                    read: vec![],
                },
                cwd: sandbox.clone(),
                home: None, // no `~` paths in this test; the carve-out is out of scope here
            })
        };

        // OUTSIDE both roots → denied, and the deny is durable as a boundary claim.
        let g = AcpGate {
            scope: "unit",
            phase: "unit-1",
            phase_alias: None,
            db: db.to_str(), // empty store — no policies; only the boundary can deny
            decisions_path: decisions.to_str().unwrap(),
            boundary: boundary(),
        };
        let (result, allowed) = permission_result(&g, &request(&outside));
        assert!(
            !allowed,
            "a write outside every declared root must be denied"
        );
        assert_eq!(result["outcome"]["optionId"], "reject");
        let log = std::fs::read_to_string(&decisions).expect("decisions log");
        assert!(
            log.contains("boundary-deny"),
            "the denial must be recorded as a BOUNDARY claim the fold can see: {log}"
        );
        assert!(
            log.contains("outside this unit's boundary"),
            "the claim must name the escape: {log}"
        );

        // INSIDE the declared inbox (the crew#263 deliverable shape) → allowed.
        let decisions_ok = dir.join("decisions-ok.jsonl");
        let g = AcpGate {
            scope: "unit",
            phase: "unit-1",
            phase_alias: None,
            db: db.to_str(),
            decisions_path: decisions_ok.to_str().unwrap(),
            boundary: boundary(),
        };
        let (result, allowed) = permission_result(&g, &request(&inbox.join("doc-v1.html")));
        assert!(
            allowed,
            "a write inside a launcher-declared root must be permitted: {result}"
        );
        assert_eq!(result["outcome"]["optionId"], "allow");

        let _ = std::fs::remove_dir_all(&dir);
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

    /// FINDING-100 / core#100 regression: the ACP bridge sometimes omits the top-level `toolName`
    /// and only carries the canonical tool name in `toolCall.name`. The old fallback went straight
    /// to `toolCall.title` (a human-readable description, not the canonical name), so these
    /// permission requests returned `None` → `cancelled` (deny) with no governance record,
    /// silently blocking legitimate tool calls.
    ///
    /// This test proves both that `toolCall.name` is now resolved AND that it is preferred over
    /// `toolCall.title` when both are present (a display title like "Reading /tmp/foo" is NOT a
    /// valid tool identity for governance evaluation).
    #[test]
    fn tool_name_resolves_from_tool_call_name_when_top_level_is_absent() {
        // Case 1: only `toolCall.name` present (no top-level `toolName`, no `toolCall.title`).
        // This is the exact shape that caused the (unknown) deny before the fix.
        let params = json!({
            "sessionId": "s1",
            "toolCall": {
                "toolCallId": "tc-1",
                "name": "Bash",
                "rawInput": {"command": "ls -la"}
            },
            "options": [
                {"optionId": "allow", "kind": "allow_once"},
                {"optionId": "reject", "kind": "reject_once"},
            ],
        });
        let (tool, payload) = pretool_payload(&params)
            .expect("toolCall.name must resolve the tool when toolName is absent");
        assert_eq!(tool, "Bash", "canonical name extracted from toolCall.name");
        assert_eq!(payload["tool_name"], "Bash");
        assert_eq!(payload["tool_input"]["command"], "ls -la");

        // The shared context builder must read it — same as the `acp_params_translate` test.
        let (context, name) =
            crate::gate_hook::claude_pretool_context(&payload.to_string(), "unit", "build");
        assert_eq!(name, "Bash");
        assert_eq!(context["command"], "ls -la");

        // Case 2: `toolCall.name` wins over `toolCall.title` — title is a display string, not an
        // identity, so governance must never evaluate a call under its display description.
        let params_with_title = json!({
            "sessionId": "s2",
            "toolCall": {
                "toolCallId": "tc-2",
                "name": "Read",
                "title": "Reading /tmp/important.txt",
                "rawInput": {"file_path": "/tmp/important.txt"}
            },
        });
        let (tool2, _) = pretool_payload(&params_with_title)
            .expect("toolCall.name must win over toolCall.title");
        assert_eq!(
            tool2, "Read",
            "toolCall.name ('Read') must take precedence over toolCall.title ('Reading /tmp/…')"
        );

        // Case 3: `toolName` at the top level still wins when all three are present — preserving
        // existing behaviour for bridges that do send the top-level field.
        let params_all = json!({
            "toolName": "Edit",
            "toolCall": {"name": "Edit", "title": "Editing /src/main.rs", "rawInput": {}},
        });
        let (tool3, _) = pretool_payload(&params_all).expect("toolName wins when present");
        assert_eq!(tool3, "Edit");

        // Case 4: MCP-prefixed tool names (e.g. estate server tools) resolve correctly.
        // `mcp__wicked-estate__SearchEntity` must survive the extraction unchanged so governance
        // policies can match on the full name.
        let mcp_params = json!({
            "toolCall": {
                "toolCallId": "tc-mcp",
                "name": "mcp__wicked-estate__SearchEntity",
                "rawInput": {"query": "fn pretool_payload"}
            },
        });
        let (tool4, _) =
            pretool_payload(&mcp_params).expect("MCP tool name must resolve from toolCall.name");
        assert_eq!(
            tool4, "mcp__wicked-estate__SearchEntity",
            "full MCP-prefixed name must be preserved for policy evaluation"
        );

        // Case 5: empty `toolName` string must fall through to `toolCall.name`, not return None.
        // An explicit `"toolName": ""` sent by a bridge is semantically absent — the filter must
        // apply BEFORE the or_else chain so the fallback is actually reached.
        let params_empty_toplevel = json!({
            "toolName": "",
            "toolCall": {
                "toolCallId": "tc-5",
                "name": "Write",
                "rawInput": {"file_path": "/src/lib.rs", "content": ""}
            },
        });
        let (tool5, _) = pretool_payload(&params_empty_toplevel)
            .expect("empty toolName must fall through to toolCall.name");
        assert_eq!(
            tool5, "Write",
            "toolCall.name must be reached when toolName is an empty string"
        );
    }
}
