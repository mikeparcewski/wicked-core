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

/// The canonical tool name of a `session/request_permission` — see [`pretool_payload`] for the
/// fallback chain and why `toolCall.title` is the last resort.
fn tool_name(params: &Value) -> Option<String> {
    params
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
        })
        .map(str::to_string)
}

/// A tool call that would WRITE — refused for an `executes_code: false` phase (core#431,
/// F-3R2-009). `kind` is the ACP `toolCall.kind` when the agent sent one; `path` the target the
/// call's arguments named, when any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WriteClassCall {
    pub tool: String,
    pub kind: Option<String>,
    pub path: Option<String>,
}

/// ACP `ToolKind`s that change the tree. `execute` (bash) is deliberately NOT here: the phase
/// must run tests — this is a posture, not a guarantee, exactly like the wrapped carrier's
/// `--exclude-tools edit,write`; the worktree guard holds the rest.
const WRITE_KINDS: [&str; 3] = ["edit", "delete", "move"];

/// Tool NAMES that write, case-folded — the built-ins of the seats the engine convenes (claude's
/// `Write`/`Edit`/`MultiEdit`/`NotebookEdit`, pi's `edit`/`write`, codex's `apply_patch`, the
/// `str_replace_*` family, opencode/copilot file tools) and the generic delete/move spellings.
/// A name match denies even when the agent sent no `kind`.
const WRITE_TOOL_NAMES: [&str; 22] = [
    "write",
    "edit",
    "multiedit",
    "notebookedit",
    "write_file",
    "edit_file",
    "create_file",
    "apply_patch",
    "str_replace_editor",
    "str_replace_based_edit_tool",
    "delete",
    "delete_file",
    "remove_file",
    "move",
    "move_file",
    "rename",
    "rename_file",
    "mkdir",
    "create_directory",
    "patch",
    "writefile",
    "editfile",
];

/// Tool-name PREFIXES that write (Copilot on #433): the `str_replace_*` family has more members
/// than the two exact spellings above (`str_replace_file`, `str_replace_edit`, …), and bridges
/// that surface file tools as `write_to_file` / `edit_notebook` / `delete_path` / `move_path` /
/// `rename_symbol_file` follow the same verb-first convention. A prefix match denies even when
/// the agent sent `kind: "other"`.
const WRITE_TOOL_PREFIXES: [&str; 9] = [
    "str_replace",
    "write_",
    "edit_",
    "delete_",
    "remove_",
    "move_",
    "rename_",
    "create_file",
    "apply_patch",
];

/// Is `tool` (case-folded) a write tool by name — an exact member of [`WRITE_TOOL_NAMES`] or a
/// [`WRITE_TOOL_PREFIXES`] match?
fn is_write_tool_name(tool: &str) -> bool {
    let lower = tool.to_ascii_lowercase();
    WRITE_TOOL_NAMES.contains(&lower.as_str())
        || WRITE_TOOL_PREFIXES.iter().any(|p| lower.starts_with(p))
}

/// Classify a permission request as a WRITE-class call, or `None` when it reads/searches/
/// executes/thinks. Matches by ACP `kind` first (the protocol's vocabulary), then by tool name
/// (a bridge that omits `kind`, or sends `other`, still names the tool).
pub(crate) fn write_class_call(params: &Value) -> Option<WriteClassCall> {
    let kind = params
        .pointer("/toolCall/kind")
        .and_then(Value::as_str)
        .filter(|k| !k.is_empty())
        .map(|k| k.to_ascii_lowercase());
    let by_kind = kind.as_deref().is_some_and(|k| WRITE_KINDS.contains(&k));
    // A request that carries a write-class `kind` and NO tool name is still a write (Copilot on
    // #433): the kind is the protocol's own classification, and a nameless request must not
    // fall through to the allow path on a read-only unit.
    let tool = match tool_name(params) {
        Some(t) => t,
        None if by_kind => "(unnamed)".to_string(),
        None => return None,
    };
    let by_name = is_write_tool_name(&tool);
    if !(by_kind || by_name) {
        return None;
    }
    let input = params.pointer("/toolCall/rawInput");
    let path = input.and_then(|i| {
        [
            "path",
            "file_path",
            "filePath",
            "filename",
            "file",
            "target_file",
            "notebook_path",
            "destination",
        ]
        .into_iter()
        .find_map(|k| i.get(k).and_then(Value::as_str))
        .filter(|p| !p.trim().is_empty())
        .map(str::to_string)
    });
    Some(WriteClassCall { tool, kind, path })
}

/// ACP `ToolKind`s / tool names that RUN A COMMAND — the calls the remote-write fence judges
/// (F-7R2-012). `execute` is the protocol's kind; the names are the seats' shell tools (claude's
/// `Bash`, pi's `bash`, codex's `shell`/`exec_command`, copilot/opencode `run_command`/`terminal`).
const EXECUTE_TOOL_NAMES: [&str; 14] = [
    "bash",
    "shell",
    "sh",
    "execute",
    "exec",
    "exec_command",
    "execute_command",
    "run_command",
    "run_shell_command",
    "run_terminal_cmd",
    "shell_command",
    "terminal",
    "command",
    "powershell",
];

/// The command text of an EXECUTE-class permission request, or `None` when the call runs no
/// command. Matched by ACP `kind == "execute"` or by tool name; the text is read from the tool's
/// own arguments (`command`, `cmd`, `commandLine`, `script`, `args` joined), falling back to
/// `toolCall.title` for a bridge that describes the call only in prose — the remote-write fence
/// ([`crate::remote_write_fence::remote_write_command`]) then judges that text.
pub(crate) fn execute_command(params: &Value) -> Option<String> {
    let kind = params
        .pointer("/toolCall/kind")
        .and_then(Value::as_str)
        .map(|k| k.to_ascii_lowercase());
    let by_kind = kind.as_deref() == Some("execute");
    let tool = tool_name(params).map(|t| t.to_ascii_lowercase());
    let by_name = tool
        .as_deref()
        .is_some_and(|t| EXECUTE_TOOL_NAMES.contains(&t) || t.ends_with("__bash"));
    if !(by_kind || by_name) {
        return None;
    }
    let input = params.pointer("/toolCall/rawInput");
    let from_input = input.and_then(|i| {
        ["command", "cmd", "commandLine", "command_line", "script"]
            .into_iter()
            .find_map(|k| i.get(k).and_then(Value::as_str))
            .filter(|c| !c.trim().is_empty())
            .map(str::to_string)
            .or_else(|| {
                i.get("args").and_then(Value::as_array).map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
            })
    });
    from_input
        .or_else(|| {
            params
                .pointer("/toolCall/title")
                .and_then(Value::as_str)
                .filter(|t| !t.trim().is_empty())
                .map(str::to_string)
        })
        .filter(|c| !c.trim().is_empty())
}

/// The answer that REFUSES a call: the agent's reject option, else cancelled (never an allow).
pub(crate) fn reject_result(params: &Value) -> Value {
    match choose_option(params.get("options").unwrap_or(&Value::Null), false) {
        Some(option_id) => json!({"outcome": {"outcome": "selected", "optionId": option_id}}),
        None => cancelled("no reject option offered"),
    }
}

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
    let tool = tool_name(params)?;
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

/// The answer for a CHAT turn (core#410, review): a chat is not a run — no scope, no phase, no
/// policy store, no decisions log — but it HAS a filesystem boundary: its scratch root is writable,
/// the scoped repository roots are READ-ONLY, and nothing path-bearing outside either is reachable.
/// Judged through the same pure check the governed carriers share
/// (`gate_hook::boundary_denial_with`), so "outside the boundary" means one thing everywhere; a
/// denied call is answered with the agent's own reject option. Fail-closed on an unreadable request
/// or an agent that offers no way to say no, exactly like the governed answer — a read-only
/// contract that only claude's `disallowedTools` honoured was a promise the other seats broke.
pub(crate) fn chat_boundary_result(
    boundary: &crate::gate_hook::BoundaryCtx,
    params: &Value,
) -> (Value, bool) {
    let Some((tool, payload)) = pretool_payload(params) else {
        return (cancelled("unparseable permission request"), false);
    };
    let (context, tool_name) =
        crate::gate_hook::claude_pretool_context(&payload.to_string(), "chat", "chat");
    let allowed = crate::gate_hook::boundary_denial_with(
        &boundary.roots,
        &boundary.cwd,
        boundary.home.as_deref(),
        boundary.claude_config_dir.as_deref(),
        &context,
        &tool_name,
    )
    .is_none();
    match choose_option(params.get("options").unwrap_or(&Value::Null), allowed) {
        Some(option_id) => (
            json!({"outcome": {"outcome": "selected", "optionId": option_id}}),
            allowed,
        ),
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
                claude_config_dir: None,
                pre_build_scope: false, // a build-phase unit: the FILESYSTEM boundary is what is on trial here
                write_posture: crate::write_posture::WritePosture::Full,
                deliverable_roots: vec![],
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

    /// core#296 — THE ISSUE, REPRODUCED AND CLOSED. On run `d1bc72c2` the `design` unit (a
    /// `stage=recon`, `pre_build_scope=true` phase) wrote `src/board/attentionReason.ts` and
    /// `tests/attentionReason.test.ts` INTO ITS OWN WORKTREE, before the creator phase ever ran.
    /// The governance hook saw both calls (`seq104`/`seq105`) and answered `decision=allow,
    /// denyingPolicy=None` — correctly, for the only question it asked: the worktree is inside the
    /// unit's write roots, so the FILESYSTEM boundary had nothing to object to. The phase's declared
    /// scope existed only as prompt text that called itself "(enforced)".
    ///
    /// So this test pins the axis that was missing, with the filesystem boundary deliberately
    /// SATISFIED on every arm — every path below is inside the sandbox the unit may write:
    ///
    /// 1. pre-build phase + `Write` to `src/…​.ts` → DENIED, with a claim naming the rule;
    /// 2. same phase, same directory, `Write` to a `.md` deliverable → ALLOWED (the phase must be
    ///    able to produce the thing it exists to produce);
    /// 3. the SAME production-code write with `pre_build_scope=false` (a build phase) → ALLOWED —
    ///    the gate is scoped by phase role, not a blanket ban on writing code.
    ///
    /// Against the pre-fix engine arm 1 returns `allow`, which is exactly the reported bug.
    #[test]
    fn a_pre_build_phase_write_to_production_code_is_denied_inside_its_own_worktree() {
        let dir = std::env::temp_dir().join(format!("wicked-acpscope-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sandbox = dir.join("sandbox");
        std::fs::create_dir_all(sandbox.join("src")).unwrap();
        std::fs::create_dir_all(sandbox.join("docs")).unwrap();
        // A REAL (empty) store: after the boundary and the phase scope pass, evaluation proceeds to
        // policy selection, and an unresolvable store is an infra-deny — fail-closed and correct,
        // but it would make the ALLOW arms pass for the wrong reason.
        let db = dir.join("gov.db");
        drop(wicked_apps_core::open_store(Some(db.to_str().unwrap())).unwrap());

        let request = |path: &std::path::Path| {
            json!({
                "sessionId": "s1",
                "toolName": "Write",
                "toolCall": {"toolCallId": "t1", "rawInput": {
                    "file_path": path.to_str().unwrap(), "content": "export const x = 1;"}},
                "options": [
                    {"optionId": "allow", "kind": "allow_once"},
                    {"optionId": "reject", "kind": "reject_once"},
                ],
            })
        };
        // The boundary is SATISFIED on every arm: the sandbox IS the unit's write root, which is
        // the whole point — this is the case the filesystem boundary correctly allows.
        let boundary = |pre_build_scope: bool| {
            Some(crate::gate_hook::BoundaryCtx {
                roots: crate::path_policy::AllowedRoots {
                    write: vec![sandbox.clone()],
                    read: vec![],
                },
                cwd: sandbox.clone(),
                home: None,
                claude_config_dir: None,
                pre_build_scope,
                write_posture: crate::write_posture::WritePosture::Full,
                deliverable_roots: vec![],
            })
        };
        // 1. THE REPORTED BUG: a recon phase writing production code into its own worktree.
        let denied_log = dir.join("decisions-denied.jsonl");
        let g = AcpGate {
            scope: "unit",
            phase: "unit-2",
            phase_alias: Some("design"),
            db: db.to_str(),
            decisions_path: denied_log.to_str().unwrap(),
            boundary: boundary(true),
        };
        let src_file = sandbox.join("src").join("attentionReason.ts");
        let (result, allowed) = permission_result(&g, &request(&src_file));
        assert!(
            !allowed,
            "a PRE-BUILD phase writing production code must be refused — this is run d1bc72c2's \
             `Write src/board/attentionReason.ts`, which the hook allowed: {result}"
        );
        assert_eq!(result["outcome"]["optionId"], "reject");
        let log = std::fs::read_to_string(&denied_log).expect("decisions log");
        // LEGIBILITY is the requirement, not just the refusal: the operator must be able to tell
        // WHY. A deny with `denyingPolicy=None` is what made the original allow so hard to see.
        assert!(
            log.contains("phase-scope-deny"),
            "the refusal must be recorded under its OWN claim id, not filed as a boundary or infra \
             deny: {log}"
        );
        assert!(
            log.contains("engine:pre-build-scope"),
            "the claim must NAME the denying rule — an operator reading the record has to be able \
             to cite it: {log}"
        );
        assert!(
            log.contains("wicked-governance-phase-scope"),
            "the evaluator identity must distinguish a phase-scope refusal from a filesystem \
             containment block: {log}"
        );
        assert!(
            log.contains("attentionReason.ts"),
            "the claim must name the FILE that was refused: {log}"
        );
        assert!(
            log.contains("docs/"),
            "the refusal must say where the deliverable MAY go — a remedy that cannot be acted on \
             is not a remedy: {log}"
        );

        // 2. The same phase's REAL deliverable, in the same worktree: allowed.
        let ok_log = dir.join("decisions-doc.jsonl");
        let g = AcpGate {
            scope: "unit",
            phase: "unit-2",
            phase_alias: Some("design"),
            db: db.to_str(),
            decisions_path: ok_log.to_str().unwrap(),
            boundary: boundary(true),
        };
        let (result, allowed) = permission_result(&g, &request(&sandbox.join("docs/design.md")));
        assert!(
            allowed,
            "a pre-build phase MUST be able to write its own analysis/design deliverable: {result}"
        );

        // 3. The build phase writing the identical file: allowed. The gate is scoped by phase role;
        //    scoping a creator away from creating would be the inverse — and worse — failure.
        let build_log = dir.join("decisions-build.jsonl");
        let g = AcpGate {
            scope: "unit",
            phase: "unit-3",
            phase_alias: Some("build"),
            db: db.to_str(),
            decisions_path: build_log.to_str().unwrap(),
            boundary: boundary(false),
        };
        let (result, allowed) = permission_result(&g, &request(&src_file));
        assert!(
            allowed,
            "the BUILD phase must still be able to write production code: {result}"
        );

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

    /// core#431 (F-3R2-009): the read-only posture on the ACP carrier. pi's built-in `edit` /
    /// `write` (lower-case, never in the hook's `WRITE_TOOLS`) are write-class by NAME; an unknown
    /// tool is write-class by ACP `kind`; reads, searches and `bash` are not — the phase must be
    /// able to run the suite.
    #[test]
    fn write_class_calls_are_recognised_by_kind_and_by_name() {
        let call = |name: &str, kind: Option<&str>, input: Value| {
            let mut tc = json!({"toolCallId": "t1", "name": name, "rawInput": input});
            if let Some(k) = kind {
                tc["kind"] = json!(k);
            }
            json!({"sessionId": "s1", "toolCall": tc})
        };
        // pi: by name, no kind sent.
        let w = write_class_call(&call("edit", None, json!({"path": "src/App.tsx"})))
            .expect("pi's edit is a write");
        assert_eq!(w.tool, "edit");
        assert_eq!(w.kind, None);
        assert_eq!(w.path.as_deref(), Some("src/App.tsx"));
        assert!(write_class_call(&call("write", None, json!({}))).is_some());
        // claude-sdk bridge spellings, case-folded.
        assert!(
            write_class_call(&call("Write", Some("edit"), json!({"file_path": "x"}))).is_some()
        );
        assert!(write_class_call(&call("NotebookEdit", None, json!({}))).is_some());
        // An unknown tool the agent labels as an edit/delete/move: by kind.
        let w = write_class_call(&call("frobnicate", Some("delete"), json!({"file": "a.ts"})))
            .expect("kind delete is a write");
        assert_eq!(w.kind.as_deref(), Some("delete"));
        assert_eq!(w.path.as_deref(), Some("a.ts"));
        assert!(write_class_call(&call("frobnicate", Some("MOVE"), json!({}))).is_some());
        // Copilot on #433: the `str_replace_*` FAMILY and verb-first file tools, by prefix, even
        // when the bridge labels the call `other`.
        for name in [
            "str_replace_file",
            "Str_Replace_Edit",
            "write_to_file",
            "edit_notebook",
            "delete_path",
            "move_path",
            "rename_symbol_file",
            "create_file_v2",
            "apply_patch_v4",
        ] {
            assert!(
                write_class_call(&call(name, Some("other"), json!({}))).is_some(),
                "{name} is a write tool by prefix"
            );
        }
        // …but a read-ish tool that merely CONTAINS a verb is not (prefix, not substring).
        assert!(
            write_class_call(&call("read_write_lock_status", Some("other"), json!({}))).is_none()
        );
        assert!(write_class_call(&call("preview_edit_diff", Some("read"), json!({}))).is_none());
        // Reads, searches, thinking and bash stay allowed (posture, not guarantee).
        assert!(write_class_call(&call("read", Some("read"), json!({"path": "a"}))).is_none());
        assert!(write_class_call(&call("Read", None, json!({}))).is_none());
        assert!(write_class_call(&call("grep", Some("search"), json!({}))).is_none());
        assert!(write_class_call(&call(
            "bash",
            Some("execute"),
            json!({"command": "npm test"})
        ))
        .is_none());
        assert!(write_class_call(&call("frobnicate", Some("other"), json!({}))).is_none());
        // No tool name and no kind ⇒ not classifiable (the caller's fail-closed rules apply)…
        assert!(write_class_call(&json!({"sessionId": "s1"})).is_none());
        // …but a write-class KIND with no name is still a write (Copilot on #433).
        let nameless = write_class_call(&json!({
            "sessionId": "s1",
            "toolCall": {"toolCallId": "t9", "kind": "edit", "rawInput": {"path": "src/x.ts"}},
        }))
        .expect("kind-only edit is a write");
        assert_eq!(nameless.tool, "(unnamed)");
        assert_eq!(nameless.kind.as_deref(), Some("edit"));
        assert_eq!(nameless.path.as_deref(), Some("src/x.ts"));
        assert!(write_class_call(&json!({
            "sessionId": "s1", "toolCall": {"toolCallId": "t9", "kind": "read"},
        }))
        .is_none());
    }

    #[test]
    fn reject_result_picks_the_reject_option_and_never_an_allow() {
        let params = json!({"options": opts()});
        let r = reject_result(&params);
        assert_eq!(r["outcome"]["outcome"], "selected");
        assert_eq!(r["outcome"]["optionId"], "reject");
        // No reject option offered ⇒ cancelled, never the allow that happens to exist.
        let only_allow = json!({"options": [{"optionId": "allow", "kind": "allow_once"}]});
        assert_eq!(
            reject_result(&only_allow)["outcome"]["outcome"],
            "cancelled"
        );
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

    /// F-7R2-012 (wave 6): the command of an EXECUTE-class request — by ACP kind or by the
    /// seats' shell tool names, from the tool's own arguments (several spellings) or the title
    /// as a last resort; a non-execute call yields nothing.
    #[test]
    fn execute_command_reads_the_shell_tools_command() {
        let by_kind = json!({
            "toolName": "Bash",
            "toolCall": {"kind": "execute", "title": "Run git push", "rawInput": {"command": "git push origin main"}},
        });
        assert_eq!(
            execute_command(&by_kind).as_deref(),
            Some("git push origin main")
        );
        let pi_bash = json!({
            "toolCall": {"name": "bash", "kind": "other", "rawInput": {"cmd": "gh pr create --fill"}},
        });
        assert_eq!(
            execute_command(&pi_bash).as_deref(),
            Some("gh pr create --fill")
        );
        let codex_shell = json!({
            "toolCall": {"name": "shell", "rawInput": {"args": ["gh", "api", "-X", "POST", "repos/o/r/pulls"]}},
        });
        assert_eq!(
            execute_command(&codex_shell).as_deref(),
            Some("gh api -X POST repos/o/r/pulls")
        );
        let title_only = json!({
            "toolCall": {"kind": "execute", "title": "git push --force"},
        });
        assert_eq!(
            execute_command(&title_only).as_deref(),
            Some("git push --force")
        );
        let a_read = json!({
            "toolName": "Read",
            "toolCall": {"kind": "read", "rawInput": {"file_path": "/wt/README.md"}},
        });
        assert_eq!(execute_command(&a_read), None, "a read carries no command");
        let an_edit = json!({
            "toolName": "Edit",
            "toolCall": {"kind": "edit", "rawInput": {"file_path": "/wt/a.rs", "command": "not a shell"}},
        });
        assert_eq!(
            execute_command(&an_edit),
            None,
            "an edit's `command` field is not a shell"
        );
    }
}
