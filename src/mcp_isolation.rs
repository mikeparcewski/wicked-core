//! No ambient MCP on a governed worker (core#657, F-11; DES-MCP-TOOLS-001 §6 step 0, slice S0).
//!
//! # The gap
//!
//! A worker seat could load MCP servers the engine never handed it. On claude those came from a
//! repository's `.mcp.json`, a local-scope entry in the worker home's `.claude.json`, and, for a
//! signed-in account, the claude.ai connectors. On opencode they came from the worker home's
//! `opencode.json`, `~/.opencode` or the repository's own config. Their calls reached the ACP
//! permission bridge as `kind: "other"`, which is not write-class, and with no MCP-aware policy the
//! engine allowed them, even in a read-only evaluator phase. So an evaluator could write through an
//! MCP tool, and evaluator ≠ creator did not hold.
//!
//! # The fix: two layers, both fail closed
//!
//! 1. **Nothing ambient loads.** Wrapped claude runs with `--strict-mcp-config` and no MCP config
//!    ([`CLAUDE_STRICT_MCP_ARGS`]). The claude ACP bridge gets the SDK's
//!    `strictMcpConfig: true` on every session, beside the `mcpServers: []` it already sends.
//!    Every seat spawn sets `ENABLE_CLAUDEAI_MCP_SERVERS=false`, the CLI's own connector
//!    off-switch (`wicked_apps_core::spawn::SeatConfig::apply`). opencode has no strict switch, so
//!    its inline config hides every MCP tool ([`opencode_config`]).
//! 2. **Any MCP call that still arrives is denied.** [`is_mcp_call`] recognises one on either
//!    carrier. MCP calls count as write-class, and no MCP server is registered until the broker
//!    lands (DES-MCP-TOOLS-001 S1), so every MCP call is refused, in every phase and on every
//!    posture. Each refusal is recorded: an advisory `mcp-deny:` claim in the decisions log for a
//!    governed unit (`gate_hook::append_mcp_deny`), plus a `workerToolCallDenied` event on a
//!    fenced ACP unit.
//!
//! Measured against the installed CLIs (claude 2.1.283, claude-agent-acp 0.73.0, opencode 1.18.31)
//! with a scripted fake model and a throwaway home. The lane evidence is under
//! `program-2026-09/lanes/p0-657/evidence/`.

use serde_json::Value;
use wicked_apps_core::spawn::SeatCli;

/// Wrapped claude: load MCP servers from `--mcp-config` ONLY, and pass none — the empty MCP
/// config. `--strict-mcp-config` drops the repository's `.mcp.json`, the config home's user- and
/// local-scope entries, plugin servers and, being "restricted to explicitly passed config", the
/// claude.ai connectors. No `--mcp-config` value rides with it on purpose: measured on claude
/// 2.1.283, the flag alone and the flag with `--mcp-config={"mcpServers":{}}` load the same
/// nothing (evidence `b-*-strictonly` vs `b-*-strict`), it is exactly what the Agent SDK sends for
/// an empty `mcpServers`, and a JSON argv value would have to survive Windows `.cmd` quoting.
pub(crate) const CLAUDE_STRICT_MCP_ARGS: [&str; 1] = ["--strict-mcp-config"];

/// The claude flags a template may not state: the engine owns which MCP servers a worker loads.
/// A template `--mcp-config` would be loaded even under `--strict-mcp-config`, so it is refused,
/// like `--setting-sources` (see `execute_wrapped::inject_isolation_flags`).
pub(crate) const CLAUDE_ENGINE_OWNED_MCP_FLAGS: [&str; 2] = ["--mcp-config", "--strict-mcp-config"];

/// The claude ACP bridge's session option for the same thing (Agent SDK `Options.strictMcpConfig`,
/// "maps to the CLI `--strict-mcp-config` flag"; claude-agent-sdk 0.3.257 `sdk.d.ts`). The bridge
/// spreads `_meta.claudeCode.options` into the SDK options (acp-agent.js:5312).
pub(crate) const ACP_STRICT_MCP_OPTION: &str = "strictMcpConfig";

/// The opencode permission pattern for "a tool id with an underscore". opencode names every MCP
/// tool `<server>_<tool>`, and none of its built-in tools carries an underscore in the name the
/// permission rules match (1.18.31 offers `bash edit glob grep read skill todowrite webfetch
/// write`; `apply_patch` is matched as `edit`). A rule with pattern `*` and action `deny` also
/// REMOVES the tool from the model's tool list (opencode `Permission.disabled`), so the model
/// never sees an MCP tool at all.
pub(crate) const OPENCODE_MCP_TOOL_PATTERN: &str = "*_*";

/// The opencode agent a governed session runs as. Pinned because an agent's own `permission`
/// block is evaluated after the top-level one, so an ambient `agent.<name>.permission` allow could
/// otherwise re-admit an MCP tool. The deny is restated on this agent, and `default_agent` keeps
/// the session on it.
pub(crate) const OPENCODE_GOVERNED_AGENT: &str = "build";

/// `OPENCODE_CONFIG_CONTENT` for an opencode seat: `existing` (the seat's governance content, the
/// skills-composed document, or nothing) with every MCP tool denied and hidden. Composed, never
/// replaced: every other key is kept, except a rule in the same permission object that would be
/// read after the deny and could re-admit an MCP tool ([`deny_mcp_in`]). opencode loads this
/// variable after the global, `~/.opencode` and project configs, and a key it adds lands after
/// theirs, so under opencode's last-match-wins rule this deny outranks an ambient
/// `"<server>_*": "allow"` in those files, at the top level or on the governed agent (measured,
/// evidence `d-top`, `d-agent2`, `d-final-ovr`).
///
/// The load order this relies on, read from the installed opencode 1.18.31 config loader: global
/// (`$XDG_CONFIG_HOME/opencode`), then `OPENCODE_CONFIG`, then the project files, then the
/// `.opencode` directories and `OPENCODE_CONFIG_DIR` (agent/mode markdown included), then
/// `OPENCODE_CONFIG_CONTENT`. So the operator's `OPENCODE_CONFIG` or `OPENCODE_CONFIG_DIR` under
/// the inherit hatch is outranked like any other ambient file. Two sources load AFTER it and are
/// not overridden here: a signed-in opencode console org's remote config, and the admin-managed
/// config directory (the opencode analog of claude's managed settings).
///
/// Fails closed like the skills composition: a value that is not a JSON object, or whose
/// `permission` / `agent` / `agent.build` / `agent.build.permission` is not an object, is an
/// `Err`. The seat is then not launched rather than launched with the MCP tools visible.
pub(crate) fn opencode_config(existing: Option<&str>) -> Result<String, String> {
    const VAR: &str = crate::skills_snapshot::OPENCODE_CONFIG_ENV;
    let mut doc = match existing {
        None => serde_json::json!({}),
        Some(text) => match serde_json::from_str::<Value>(text) {
            Ok(v) if v.is_object() => v,
            Ok(_) => {
                return Err(format!(
                    "{VAR} is not a JSON object; the MCP deny cannot be composed into it"
                ))
            }
            Err(e) => {
                return Err(format!(
                    "{VAR} is not valid JSON ({e}); the MCP deny cannot be composed into it"
                ))
            }
        },
    };
    let obj = doc.as_object_mut().expect("an object by construction");
    obj.entry("$schema")
        .or_insert_with(|| Value::String("https://opencode.ai/config.json".to_string()));
    deny_mcp_in(obj, "permission")?;
    let agent = object_entry(obj, "agent", VAR)?;
    let governed = object_entry(agent, OPENCODE_GOVERNED_AGENT, VAR)?;
    deny_mcp_in(governed, "permission")?;
    obj.insert(
        "default_agent".to_string(),
        Value::String(OPENCODE_GOVERNED_AGENT.to_string()),
    );
    Ok(doc.to_string())
}

/// Set `parent[key][OPENCODE_MCP_TOOL_PATTERN] = "deny"`, creating the object where absent, so
/// that the deny is the LAST rule in this object that can match a `<server>_<tool>` id.
///
/// opencode reads a permission object's rules in document order, and the last match wins. The
/// order this document is emitted in is serde_json's `Map` order: sorted keys by default, and
/// insertion order if a dependency ever turns on `preserve_order`. Under sorted keys `"*_*"` lands
/// before every letter-led key, so an existing `"wt_*": "allow"` in the SAME document would be
/// emitted after the deny and re-admit `wt_wt_note` (review of #659). So, whatever the map order:
/// the old `*_*` entry is dropped and the deny re-inserted, then every rule emitted AFTER it that
/// could match an underscore id (its key holds `*`, `?` or `_`) and is not itself a `deny` is
/// dropped. A rule emitted before the deny (`"*": "ask"` under sorted keys) is kept: the deny
/// outranks it. Literal non-underscore keys (`read`, `bash`) cannot match an MCP id and are kept.
fn deny_mcp_in(parent: &mut serde_json::Map<String, Value>, key: &str) -> Result<(), String> {
    let rules = object_entry(parent, key, crate::skills_snapshot::OPENCODE_CONFIG_ENV)?;
    // Rebuilt rather than edited in place: `Map::remove` is a swap-remove under `preserve_order`,
    // which would itself reorder the rules.
    let mut staged = serde_json::Map::new();
    for (k, v) in std::mem::take(rules) {
        if k != OPENCODE_MCP_TOOL_PATTERN {
            staged.insert(k, v);
        }
    }
    staged.insert(
        OPENCODE_MCP_TOOL_PATTERN.to_string(),
        Value::String("deny".to_string()),
    );
    let mut after_deny = false;
    for (k, v) in staged {
        if k == OPENCODE_MCP_TOOL_PATTERN {
            after_deny = true;
        } else if after_deny && could_match_mcp_id(&k) && v.as_str() != Some("deny") {
            continue;
        }
        rules.insert(k, v);
    }
    Ok(())
}

/// Could an opencode permission key match a `<server>_<tool>` tool id? A glob (`*`, `?`) or a key
/// that itself holds an underscore can; a literal non-underscore name (`read`, `bash`) cannot.
fn could_match_mcp_id(key: &str) -> bool {
    key.contains(['*', '?', '_'])
}

/// `parent[key]` as an object, created where absent; a non-object is an error, never replaced.
fn object_entry<'a>(
    parent: &'a mut serde_json::Map<String, Value>,
    key: &str,
    var: &str,
) -> Result<&'a mut serde_json::Map<String, Value>, String> {
    parent
        .entry(key.to_string())
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or_else(|| {
            format!("{var} has a `{key}` that is not an object; the MCP deny cannot be composed into it")
        })
}

/// Whether a tool call is an MCP call.
///
/// - **Any seat:** a name with the `mcp__` prefix (claude's `mcp__<server>__<tool>`, in the hook
///   payload's `tool_name` and in the ACP bridge's `toolCall.name` / `title`), case-insensitive.
/// - **opencode:** its MCP titles are `<server>_<tool>` (`wt_wt_note`, evidence e7b), with kind
///   `other`, or `search` for the context7 server's `context7_resolve_library_id` /
///   `context7_get_library_docs` (opencode 1.18.31 `toToolKind`). That shape is judged only on an opencode seat. Other seats name
///   built-ins the same way (copilot's `write_bash`, codex's `update_plan`), and refusing those
///   would break the seat. opencode's own built-ins carry no underscore (1.18.31 offers
///   `bash edit glob grep read skill todowrite webfetch write`) except `apply_patch`, which is
///   excluded by name ([`OPENCODE_UNDERSCORE_BUILT_INS`]). The launch config's `*_*` deny matches
///   `apply_patch` as `edit`, so the two layers agree.
pub(crate) fn is_mcp_call(names: &[&str], kind: Option<&str>, seat: SeatCli) -> bool {
    if names.iter().any(|n| has_mcp_prefix(n)) {
        return true;
    }
    // The kinds opencode can give an MCP tool: `other` (its default) and `search` (the context7
    // pair). A built-in's own kind (`execute` for bash, whose title may be a one-word command,
    // `edit`, `read`, `fetch`, `think`) is never an MCP call.
    let mcp_kind = kind.is_none_or(|k| {
        k.is_empty() || k.eq_ignore_ascii_case("other") || k.eq_ignore_ascii_case("search")
    });
    seat == SeatCli::Opencode
        && mcp_kind
        && names.iter().any(|n| {
            is_opencode_mcp_title(n)
                && !OPENCODE_UNDERSCORE_BUILT_INS
                    .iter()
                    .any(|b| n.trim().eq_ignore_ascii_case(b))
        })
}

/// opencode built-in tool ids that hold an underscore — the only titles of that shape on an
/// opencode seat that are NOT MCP tools.
pub(crate) const OPENCODE_UNDERSCORE_BUILT_INS: [&str; 1] = ["apply_patch"];

/// `mcp__…`, case-insensitive, with something after the prefix.
pub(crate) fn has_mcp_prefix(name: &str) -> bool {
    const PREFIX: &str = "mcp__";
    name.len() > PREFIX.len()
        && name
            .get(..PREFIX.len())
            .is_some_and(|p| p.eq_ignore_ascii_case(PREFIX))
}

/// One identifier token (`[A-Za-z0-9_-]`) holding an inner underscore: `<server>_<tool>`.
fn is_opencode_mcp_title(title: &str) -> bool {
    let t = title.trim();
    !t.is_empty()
        && t.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && t.trim_matches('_').contains('_')
}

/// The operator-facing reason every MCP refusal carries, on both carriers.
pub(crate) fn denial_reason(tool: &str) -> String {
    format!(
        "mcp fence: `{tool}` is an MCP tool call. MCP calls count as writes, and no MCP server is \
         registered for governed workers (the MCP broker has not landed), so the call is refused \
         in every phase. {REMEDY}"
    )
}

/// What the seat is told to do instead.
pub(crate) const REMEDY: &str = "Do the work with the built-in tools, or report in this phase's \
     output that the task needs the tool; ambient MCP servers from the worker home, the \
     repository or claude.ai connectors are never loaded for governed workers";

/// The ACP permission request's candidate tool names, in [`acp_permission`]'s precedence:
/// `toolName`, `toolCall.name`, `toolCall.title`. All of them are checked, so a canonical name in
/// any one field is enough.
///
/// [`acp_permission`]: crate::acp_permission
pub(crate) fn acp_names(params: &Value) -> Vec<&str> {
    [
        params.get("toolName"),
        params.pointer("/toolCall/name"),
        params.pointer("/toolCall/title"),
    ]
    .into_iter()
    .flatten()
    .filter_map(Value::as_str)
    .filter(|s| !s.trim().is_empty())
    .collect()
}

/// The MCP tool a `session/request_permission` asks for, or `None` when it is not an MCP call.
pub(crate) fn acp_mcp_tool(params: &Value, seat: SeatCli) -> Option<String> {
    let names = acp_names(params);
    let kind = params.pointer("/toolCall/kind").and_then(Value::as_str);
    if !is_mcp_call(&names, kind, seat) {
        return None;
    }
    names
        .iter()
        .find(|n| has_mcp_prefix(n))
        .or_else(|| names.first())
        .map(|n| n.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn frame(name: Option<&str>, title: &str, kind: &str) -> Value {
        let mut call = json!({"toolCallId": "t1", "title": title, "kind": kind, "rawInput": {}});
        if let Some(n) = name {
            call["name"] = json!(n);
        }
        json!({"sessionId": "s", "toolCall": call, "options": []})
    }

    /// The claude shape (evidence e2): `toolCall.name` and `title` are `mcp__<server>__<tool>`,
    /// kind `other`. MCP on every seat, whichever field carries the name.
    #[test]
    fn a_claude_mcp_call_is_recognised_on_every_seat_and_in_every_name_field() {
        for seat in [
            SeatCli::Claude,
            SeatCli::Other,
            SeatCli::Opencode,
            SeatCli::Copilot,
        ] {
            let f = frame(Some("mcp__wt__wt_note"), "mcp__wt__wt_note", "other");
            assert_eq!(acp_mcp_tool(&f, seat).as_deref(), Some("mcp__wt__wt_note"));
            // Name only in the title (a bridge that omits `name`).
            let f = frame(None, "mcp__wt__wt_note", "other");
            assert!(acp_mcp_tool(&f, seat).is_some(), "{seat:?}");
            // Canonical name present, prose title.
            let f = frame(Some("MCP__wt__x"), "Calling a tool", "other");
            assert!(acp_mcp_tool(&f, seat).is_some(), "{seat:?}");
        }
        assert!(has_mcp_prefix("mcp__claude_ai_Linear__create_issue"));
        assert!(!has_mcp_prefix("mcp__"), "a bare prefix names no tool");
        assert!(!has_mcp_prefix("mcp_wt"));
    }

    /// The opencode shape (evidence e7b): title `<server>_<tool>`, kind `other`, no name.
    #[test]
    fn an_opencode_mcp_title_is_recognised_on_the_opencode_seat_only() {
        let f = frame(None, "wt_wt_note", "other");
        assert_eq!(
            acp_mcp_tool(&f, SeatCli::Opencode).as_deref(),
            Some("wt_wt_note")
        );
        // Review of #659: an MCP tool opencode gives a specific kind is still an MCP call — the
        // context7 server's tools arrive as `search`.
        for kind in ["search", "other", ""] {
            let f = frame(None, "context7_resolve_library_id", kind);
            assert!(
                acp_mcp_tool(&f, SeatCli::Opencode).is_some(),
                "kind {kind:?}"
            );
        }
        // Another seat's underscore built-in is not an MCP call: copilot's `write_bash`,
        // codex's `update_plan`.
        for (seat, title) in [
            (SeatCli::Copilot, "write_bash"),
            (SeatCli::Codex, "update_plan"),
        ] {
            assert!(acp_mcp_tool(&frame(None, title, "other"), seat).is_none());
        }
    }

    /// opencode's built-ins keep working: no underscore (`todowrite`, `skill`), or the one
    /// underscore built-in, `apply_patch`, whatever its kind. Prose titles are not identifiers.
    #[test]
    fn opencode_built_in_tools_are_not_mcp_calls() {
        for (title, kind) in [
            ("todowrite", "other"),
            ("skill", "other"),
            ("bash", "execute"),
            ("read", "read"),
            ("apply_patch", "edit"),
            ("apply_patch", "other"),
            // A one-word bash command is `execute`, never an MCP title.
            ("run_tests", "execute"),
            ("Run git status", "other"),
            ("_", "other"),
        ] {
            assert!(
                acp_mcp_tool(&frame(None, title, kind), SeatCli::Opencode).is_none(),
                "{title} ({kind})"
            );
        }
        // A claude built-in is not MCP either.
        for name in ["Read", "Bash", "Write", "TodoWrite", "WebFetch"] {
            assert!(acp_mcp_tool(&frame(Some(name), name, "other"), SeatCli::Claude).is_none());
        }
    }

    /// The composed opencode document: the MCP deny at the top level AND on the governed agent,
    /// the agent pinned, every existing key kept.
    #[test]
    fn the_opencode_config_denies_every_mcp_tool_and_keeps_the_governance_content() {
        let seat = r#"{"$schema":"https://opencode.ai/config.json","permission":{"read":"ask","edit":"ask","bash":"ask","task":"deny"},"skills":{"paths":["/s"]}}"#;
        let doc: Value = serde_json::from_str(&opencode_config(Some(seat)).unwrap()).unwrap();
        assert_eq!(doc["permission"]["*_*"], "deny");
        assert_eq!(doc["agent"]["build"]["permission"]["*_*"], "deny");
        assert_eq!(doc["default_agent"], "build");
        for k in ["read", "edit", "bash"] {
            assert_eq!(doc["permission"][k], "ask", "{k} kept");
        }
        assert_eq!(doc["permission"]["task"], "deny");
        assert_eq!(doc["skills"]["paths"], json!(["/s"]));
        // An ambient allow for the same pattern is overwritten, not kept beside it.
        let ambient =
            r#"{"permission":{"*_*":"allow"},"agent":{"build":{"permission":{"wt_*":"allow"}}}}"#;
        let doc: Value = serde_json::from_str(&opencode_config(Some(ambient)).unwrap()).unwrap();
        assert_eq!(doc["permission"]["*_*"], "deny");
        assert_eq!(doc["agent"]["build"]["permission"]["*_*"], "deny");
        // No existing content: a bare document still carries the deny.
        let doc: Value = serde_json::from_str(&opencode_config(None).unwrap()).unwrap();
        assert_eq!(doc["permission"]["*_*"], "deny");
    }

    /// Review of #659: an allow for an MCP-shaped pattern INSIDE the composed document is not
    /// left to be emitted after the deny (serde_json sorts keys, so `"wt_*"` follows `"*_*"`, and
    /// opencode's last match wins). Rules the deny outranks, a `deny` of its own, and literal
    /// non-underscore keys stay; the emitted text puts the deny after every kept matching rule.
    #[test]
    fn no_rule_in_the_composed_document_is_read_after_the_mcp_deny() {
        let existing = r#"{"permission":{"wt_*":"allow","*":"ask","bash":{"git *":"allow"},"read":"ask","x_y":"deny","wt?note":{"*":"allow"},"*_*":"allow"},"agent":{"build":{"permission":{"wt_*":"allow","edit":"ask"}}}}"#;
        let out = opencode_config(Some(existing)).unwrap();
        let doc: Value = serde_json::from_str(&out).unwrap();
        for scope in [&doc["permission"], &doc["agent"]["build"]["permission"]] {
            let rules = scope.as_object().unwrap();
            assert_eq!(rules["*_*"], "deny", "{out}");
            assert!(
                !rules.contains_key("wt_*"),
                "the outranking allow is dropped: {out}"
            );
            assert!(!rules.contains_key("wt?note"), "{out}");
            // In emitted order, nothing after the deny can match an MCP id unless it denies.
            let keys: Vec<&String> = rules.keys().collect();
            let pos = keys.iter().position(|k| *k == "*_*").unwrap();
            for k in &keys[pos + 1..] {
                assert!(
                    !could_match_mcp_id(k) || rules[k.as_str()] == "deny",
                    "`{k}` is read after the deny: {out}"
                );
            }
        }
        let top = doc["permission"].as_object().unwrap();
        assert_eq!(top["*"], "ask", "a rule the deny outranks is kept: {out}");
        assert_eq!(top["bash"], serde_json::json!({"git *": "allow"}), "{out}");
        assert_eq!(top["read"], "ask");
        assert_eq!(top["x_y"], "deny", "a deny of its own is kept");
        assert_eq!(doc["agent"]["build"]["permission"]["edit"], "ask");
    }

    /// Fail closed: a document the deny cannot be composed into refuses the launch.
    #[test]
    fn an_opencode_config_the_deny_cannot_be_composed_into_is_an_error() {
        for bad in [
            "not json",
            "[]",
            r#"{"permission":"allow"}"#,
            r#"{"agent":[]}"#,
            r#"{"agent":{"build":{"permission":"allow"}}}"#,
        ] {
            let err = opencode_config(Some(bad)).expect_err(bad);
            assert!(err.contains("OPENCODE_CONFIG_CONTENT"), "{bad}: {err}");
        }
    }
}
