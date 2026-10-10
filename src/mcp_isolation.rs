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

/// (core#660) The MCP off-switches a NON-claude seat's launch carries, so nothing ambient loads
/// on the carriers claude's `--strict-mcp-config` does not cover. Empty for a seat with no MCP
/// (pi) and for claude (whose flags are [`CLAUDE_STRICT_MCP_ARGS`], injected with the rest of its
/// isolation). `seat_root` is the seat's ENGINE-MINTED configuration home
/// (`SeatConfig::Isolated::root`); `None` is the operator-inherit hatch, and the pin then reads the
/// CLI's OWN home instead ([`config_home`]) — the hatch inherits the operator's scopes, never an
/// ungoverned tool channel, so the pin rides every launch. `cwd` is the unit's working directory,
/// which for copilot is a config SOURCE of its own.
///
/// **copilot** (measured on GitHub Copilot CLI 1.0.88): `--disable-builtin-mcps` — copilot ships
/// a built-in `github-mcp-server`, ENABLED by default, which is a GitHub write path the
/// remote-write fence never sees — plus `--disable-mcp-server <name>` for every server named by
/// its config sources (`<COPILOT_HOME>/mcp-config.json`, and the workspace's `.mcp.json` /
/// `.github/mcp.json`, which load once the working directory is trusted). Verified: with
/// `COPILOT_HOME` pointed at a throwaway home holding one server, `copilot mcp list` reads
/// `probe (local)` + `github-mcp-server (http)`, and with these flags it reads
/// `probe (local, disabled)` + `github-mcp-server (http, disabled)`. A name that is not
/// configured is accepted and does nothing.
///
/// **codex** (measured on codex-cli 0.154.0): `-c mcp_servers.<name>.enabled=false` for every
/// server in `<CODEX_HOME>/config.toml`. NOT `-c mcp_servers={}`, which core#660 proposed and
/// which does NOT work: the table MERGES, and `codex mcp list` still reads the ambient server as
/// `enabled`. The per-server form reads `disabled`, and `codex exec … -c mcp_servers.x.enabled=…`
/// is parsed in that position (a bad value is rejected before the model runs). A server whose
/// name is not a bare TOML key REFUSES the launch: the quoted-key override
/// (`mcp_servers."weird name".enabled=false`) replaces the table instead of merging into it and
/// codex then fails to load its own config ("invalid transport"), so there is no override this
/// can emit — and a governed seat that loads an MCP server the engine did not hand it is what
/// this closes. The codex-acp bridge is a different binary that takes no `-c`: its half is
/// [`acp_codex_mcp_refusal`]. Not covered, disclosed: an enterprise-managed codex config source
/// this cannot read.
pub(crate) fn seat_mcp_pin_flags(
    cli: SeatCli,
    seat_root: Option<&std::path::Path>,
    cwd: Option<&std::path::Path>,
) -> Result<Vec<String>, String> {
    match cli {
        SeatCli::Copilot => {
            let mut flags = vec![COPILOT_DISABLE_BUILTIN_MCPS.to_string()];
            let mut sources = vec![config_home(cli, seat_root)?.join(COPILOT_MCP_CONFIG_FILE)];
            if let Some(cwd) = cwd {
                sources.push(cwd.join(".mcp.json"));
                sources.push(cwd.join(".github").join("mcp.json"));
            }
            for source in sources {
                for name in copilot_mcp_server_names(&source)? {
                    flags.push("--disable-mcp-server".to_string());
                    flags.push(name);
                }
            }
            Ok(flags)
        }
        SeatCli::Codex => {
            let mut flags = Vec::new();
            for name in codex_mcp_server_names(&config_home(cli, seat_root)?.join("config.toml"))? {
                flags.push("-c".to_string());
                flags.push(format!("mcp_servers.{name}.enabled=false"));
            }
            Ok(flags)
        }
        _ => Ok(Vec::new()),
    }
}

/// (core#660, the ACP half) The codex-acp bridge is a different program from `codex`: it takes no
/// `-c`, so [`seat_mcp_pin_flags`]' per-server override cannot ride it, and codex's MCP calls on it
/// never reach `session/request_permission` (its `auto_review` decides them), so layer 2 never sees
/// them either. Nothing can switch an ambient server off on this carrier, so an ACP codex seat whose
/// configuration ENABLES one is REFUSED at spawn (fail closed) — `Err` names the server and the
/// file, and the remedy is codex's own `enabled = false` on that server (or removing it).
///
/// Sources: the seat's configuration home (`<CODEX_HOME>/config.toml`, or the CLI's own home under
/// the operator-inherit hatch, as [`config_home`] resolves it) and the working directory's
/// `.codex/config.toml` — codex-acp marks every session root `trust_level: "trusted"`, and a trusted
/// project's config layer loads. A server with `enabled = false` is not loaded and passes. A file
/// that exists and cannot be read or parsed is an `Err` (a config the engine cannot read is not one
/// it can show is empty). The rig's seat homes configure no MCP server, so this refuses nothing
/// there.
pub(crate) fn acp_codex_mcp_refusal(
    seat_root: Option<&std::path::Path>,
    cwd: Option<&std::path::Path>,
) -> Result<(), String> {
    let mut sources = vec![config_home(SeatCli::Codex, seat_root)?.join("config.toml")];
    if let Some(cwd) = cwd {
        sources.push(cwd.join(".codex").join("config.toml"));
    }
    // (codex r1 on #825) codex-acp also merges a JSON `CODEX_CONFIG` from its environment into
    // the session config — the child inherits the daemon's — and that layer can add servers too.
    if let Some(raw) = std::env::var_os(CODEX_ACP_CONFIG_ENV).filter(|v| !v.is_empty()) {
        let enabled = codex_acp_config_mcp_servers(&raw.to_string_lossy())?;
        if !enabled.is_empty() {
            return Err(format!(
                "the inherited {CODEX_ACP_CONFIG_ENV} enables the MCP server(s) {} — the \
                 codex-acp bridge merges it and takes no `-c` override, so the engine cannot pin \
                 them off; drop them from {CODEX_ACP_CONFIG_ENV} (or set each `enabled: false`); \
                 {REFUSAL_TAIL}",
                enabled
                    .iter()
                    .map(|n| format!("`{n}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    for source in sources {
        let enabled = codex_enabled_mcp_servers(&source)?;
        if !enabled.is_empty() {
            return Err(format!(
                "{} enables the MCP server(s) {} — the codex-acp bridge takes no `-c` override, \
                 so the engine cannot pin them off on this carrier; set `enabled = false` on each \
                 (or remove it), or seat codex on the wrapped carrier, where the pin rides; \
                 {REFUSAL_TAIL}",
                source.display(),
                enabled
                    .iter()
                    .map(|n| format!("`{n}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    Ok(())
}

/// The variable codex-acp reads a JSON config layer from (its `startAcpServer`).
pub(crate) const CODEX_ACP_CONFIG_ENV: &str = "CODEX_CONFIG";

/// The MCP servers a `CODEX_CONFIG` JSON layer could enable: every entry of an `mcp_servers`
/// object whose `enabled` is not `false`, and every dotted `mcp_servers.<name>…` key other than
/// `mcp_servers.<name>.enabled: false`. Not JSON, or not an object, is an `Err` (fail closed).
fn codex_acp_config_mcp_servers(raw: &str) -> Result<Vec<String>, String> {
    let doc: Value = serde_json::from_str(raw).map_err(|e| {
        format!("the inherited {CODEX_ACP_CONFIG_ENV} is not valid JSON ({e}); {REFUSAL_TAIL}")
    })?;
    let Some(map) = doc.as_object() else {
        return Err(format!(
            "the inherited {CODEX_ACP_CONFIG_ENV} is not a JSON object; {REFUSAL_TAIL}"
        ));
    };
    let mut enabled: Vec<String> = Vec::new();
    for (key, value) in map {
        if key == "mcp_servers" {
            match value {
                Value::Object(servers) => enabled.extend(
                    servers
                        .iter()
                        .filter(|(_, v)| v.get("enabled").and_then(Value::as_bool) != Some(false))
                        .map(|(k, _)| k.clone()),
                ),
                _ => {
                    return Err(format!(
                        "the inherited {CODEX_ACP_CONFIG_ENV} has an `mcp_servers` that is not an \
                         object; {REFUSAL_TAIL}"
                    ))
                }
            }
        } else if let Some(rest) = key.strip_prefix("mcp_servers.") {
            let name = rest.split('.').next().unwrap_or(rest);
            let disables = rest.ends_with(".enabled") && value.as_bool() == Some(false);
            if !disables {
                enabled.push(name.to_string());
            }
        }
    }
    enabled.sort();
    enabled.dedup();
    Ok(enabled)
}

/// The ENABLED server names in a codex `config.toml`: every `[mcp_servers.<name>]` whose
/// `enabled` is not `false` (codex's default is enabled). Missing file ⇒ none.
fn codex_enabled_mcp_servers(path: &std::path::Path) -> Result<Vec<String>, String> {
    let Some(text) = read_optional(path)? else {
        return Ok(Vec::new());
    };
    let doc: toml::Value = text
        .parse()
        .map_err(|e| format!("{} is not valid TOML ({e}); {REFUSAL_TAIL}", path.display()))?;
    match doc.get("mcp_servers") {
        None => Ok(Vec::new()),
        Some(toml::Value::Table(table)) => Ok(table
            .iter()
            .filter(|(_, v)| v.get("enabled").and_then(|e| e.as_bool()) != Some(false))
            .map(|(k, _)| k.clone())
            .collect()),
        Some(_) => Err(format!(
            "{} has an `mcp_servers` that is not a table; {REFUSAL_TAIL}",
            path.display()
        )),
    }
}

/// The configuration home whose MCP config a seat will actually read: its ENGINE-MINTED root when
/// it has one, and otherwise — under the operator-inherit hatch, where `SeatConfig::Inherit`
/// touches no CLI configuration variable — the CLI's OWN home, `$CODEX_HOME` / `$COPILOT_HOME`
/// or its default under the home directory.
///
/// Review of #672 (codex, HIGH): with `seat_root: None` treated as "nothing to pin", the hatch
/// disabled the pin silently while the operator's real `~/.codex/config.toml` MCP servers loaded
/// — the one case where an ambient server is most likely to exist. The hatch inherits the
/// operator's scopes, never an ungoverned tool channel: the pin rides EVERY launch, hatch or not,
/// exactly like claude's `--strict-mcp-config` and the deny fence. A home that cannot be resolved
/// at all is an `Err`: a config whose location is unknown cannot be shown to be empty.
fn config_home(
    cli: SeatCli,
    seat_root: Option<&std::path::Path>,
) -> Result<std::path::PathBuf, String> {
    if let Some(root) = seat_root {
        return Ok(root.to_path_buf());
    }
    let (var, default_dir) = match cli {
        SeatCli::Codex => (wicked_apps_core::spawn::CODEX_HOME_ENV, ".codex"),
        SeatCli::Copilot => (wicked_apps_core::spawn::COPILOT_HOME_ENV, ".copilot"),
        other => return Err(format!("{other:?} has no MCP configuration home to pin")),
    };
    if let Some(v) = std::env::var_os(var).filter(|v| !v.is_empty()) {
        return Ok(std::path::PathBuf::from(v));
    }
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|h| !h.is_empty())
        .map(|h| std::path::PathBuf::from(h).join(default_dir))
        .ok_or_else(|| {
            format!(
                "this launch inherits the operator's {var} but neither {var} nor a home directory \
                 resolves, so the MCP servers `{default_dir}` configures cannot be named; \
                 {REFUSAL_TAIL}"
            )
        })
}

/// copilot's built-in MCP off-switch (its own flag; currently `github-mcp-server`).
pub(crate) const COPILOT_DISABLE_BUILTIN_MCPS: &str = "--disable-builtin-mcps";

/// The user-scope MCP config copilot reads from its configuration home.
pub(crate) const COPILOT_MCP_CONFIG_FILE: &str = "mcp-config.json";

/// The server names in one copilot MCP config file (`{"mcpServers": {"<name>": …}}`). A missing
/// file is no servers; a file that EXISTS and cannot be read, is not JSON, or whose `mcpServers`
/// is not an object is an `Err` — the launch is refused rather than run with servers the pin
/// could not name (fail closed, like the rest of this module).
fn copilot_mcp_server_names(path: &std::path::Path) -> Result<Vec<String>, String> {
    let Some(text) = read_optional(path)? else {
        return Ok(Vec::new());
    };
    let doc: Value = serde_json::from_str(&text)
        .map_err(|e| format!("{} is not valid JSON ({e}); {REFUSAL_TAIL}", path.display()))?;
    if !doc.is_object() {
        // Fail closed on a document this cannot read as a config at all: `.get("mcpServers")` on
        // a JSON array answers `None`, which would read as "no servers configured".
        return Err(format!(
            "{} is not a JSON object, so the MCP servers it configures cannot be named; \
             {REFUSAL_TAIL}",
            path.display()
        ));
    }
    match doc.get("mcpServers") {
        None => Ok(Vec::new()),
        Some(Value::Object(map)) => Ok(map.keys().cloned().collect()),
        Some(_) => Err(format!(
            "{} has an `mcpServers` that is not an object; {REFUSAL_TAIL}",
            path.display()
        )),
    }
}

/// The server names in a codex `config.toml` (`[mcp_servers.<name>]`). A missing file is no
/// servers; an unreadable or unparseable file, or a name codex's own `-c` override cannot address
/// (anything but `[A-Za-z0-9_-]+`), is an `Err`.
fn codex_mcp_server_names(path: &std::path::Path) -> Result<Vec<String>, String> {
    let Some(text) = read_optional(path)? else {
        return Ok(Vec::new());
    };
    let doc: toml::Value = text
        .parse()
        .map_err(|e| format!("{} is not valid TOML ({e}); {REFUSAL_TAIL}", path.display()))?;
    let names = match doc.get("mcp_servers") {
        None => Vec::new(),
        Some(toml::Value::Table(table)) => table.keys().cloned().collect(),
        Some(_) => {
            return Err(format!(
                "{} has an `mcp_servers` that is not a table; {REFUSAL_TAIL}",
                path.display()
            ))
        }
    };
    for name in &names {
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(format!(
                "{} names the MCP server `{name}`, which codex's own `-c \
                 mcp_servers.<name>.enabled=false` override cannot address (a quoted key replaces \
                 the table and codex then fails to load its config), so the engine cannot pin it \
                 off; {REFUSAL_TAIL} — rename or remove that server",
                path.display()
            ));
        }
    }
    Ok(names)
}

/// The tail every pin refusal carries: what the engine was doing and why it stopped.
const REFUSAL_TAIL: &str = "governed workers load no MCP server the engine did not hand them \
    (core#657/#660), and this launch cannot be pinned to none — refusing it rather than running \
    a worker with an ungoverned tool channel";

/// A file's text, or `None` when it does not exist. Any other read error is an `Err` (a config
/// the pin cannot READ is not a config the pin can be sure is empty).
fn read_optional(path: &std::path::Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!(
            "{} could not be read ({e}); {REFUSAL_TAIL}",
            path.display()
        )),
    }
}

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

    /// core#660 items 1 and 2. A copilot seat's launch disables copilot's BUILT-IN
    /// `github-mcp-server` (a GitHub write path the remote-write fence never sees) and every
    /// server its config sources name — the seat's own `mcp-config.json` and the workspace's
    /// `.mcp.json` / `.github/mcp.json`. A codex seat's launch disables every server in its seat
    /// home's `config.toml`, in the ONE spelling that works (measured on codex-cli 0.154.0:
    /// `-c mcp_servers={}` leaves the ambient server `enabled`; the per-server form reads
    /// `disabled`). claude and pi get nothing here — claude's pin is its own flag, pi has no MCP.
    #[test]
    fn a_copilot_or_codex_seat_launch_pins_every_ambient_mcp_server_off() {
        let base = std::env::temp_dir().join(format!("wicked-660-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let seat = base.join("copilot");
        let ws = base.join("wt");
        std::fs::create_dir_all(seat.join("x")).unwrap();
        std::fs::create_dir_all(ws.join(".github")).unwrap();
        std::fs::write(
            seat.join(COPILOT_MCP_CONFIG_FILE),
            r#"{"mcpServers":{"user-one":{"command":"/bin/echo"}}}"#,
        )
        .unwrap();
        std::fs::write(
            ws.join(".mcp.json"),
            r#"{"mcpServers":{"ws-one":{"command":"/bin/echo"}}}"#,
        )
        .unwrap();
        std::fs::write(
            ws.join(".github").join("mcp.json"),
            r#"{"mcpServers":{"gh-one":{"command":"/bin/echo"}}}"#,
        )
        .unwrap();
        let flags =
            seat_mcp_pin_flags(SeatCli::Copilot, Some(&seat), Some(&ws)).expect("copilot pins");
        assert_eq!(flags[0], COPILOT_DISABLE_BUILTIN_MCPS, "{flags:?}");
        for name in ["user-one", "ws-one", "gh-one"] {
            let ix = flags
                .iter()
                .position(|f| f == name)
                .unwrap_or_else(|| panic!("{name} is disabled: {flags:?}"));
            assert_eq!(flags[ix - 1], "--disable-mcp-server", "{flags:?}");
        }
        // A seat with no config file at all still disables the built-in server.
        let empty = base.join("copilot-empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(
            seat_mcp_pin_flags(SeatCli::Copilot, Some(&empty), Some(&empty)).unwrap(),
            vec![COPILOT_DISABLE_BUILTIN_MCPS.to_string()]
        );

        let codex = base.join("codex");
        std::fs::create_dir_all(&codex).unwrap();
        std::fs::write(
            codex.join("config.toml"),
            "model = \"x\"\n\n[mcp_servers.probe]\ncommand = \"/bin/echo\"\nargs = [\"hi\"]\n\n[mcp_servers.other-1]\ncommand = \"/bin/echo\"\n",
        )
        .unwrap();
        let flags =
            seat_mcp_pin_flags(SeatCli::Codex, Some(&codex), Some(&ws)).expect("codex pins");
        for name in ["probe", "other-1"] {
            assert!(
                flags
                    .windows(2)
                    .any(|w| w[0] == "-c" && w[1] == format!("mcp_servers.{name}.enabled=false")),
                "{name}: {flags:?}"
            );
        }
        // Review of #672 (HIGH): under the operator-inherit hatch there is no engine-minted seat
        // root (`SeatConfig::Inherit` sets no configuration variable), and the operator's OWN
        // home is exactly where an ambient MCP server is most likely to be. The pin reads that
        // home instead of quietly doing nothing.
        {
            let _env = crate::test_env::ENV_LOCK
                .write()
                .unwrap_or_else(|p| p.into_inner());
            struct Pin(&'static str, Option<std::ffi::OsString>);
            impl Pin {
                fn set(k: &'static str, v: &std::path::Path) -> Self {
                    let prev = std::env::var_os(k);
                    std::env::set_var(k, v);
                    Self(k, prev)
                }
            }
            impl Drop for Pin {
                fn drop(&mut self) {
                    match &self.1 {
                        Some(v) => std::env::set_var(self.0, v),
                        None => std::env::remove_var(self.0),
                    }
                }
            }
            let _codex_home = Pin::set(wicked_apps_core::spawn::CODEX_HOME_ENV, &codex);
            let _copilot_home = Pin::set(wicked_apps_core::spawn::COPILOT_HOME_ENV, &seat);
            let hatched = seat_mcp_pin_flags(SeatCli::Codex, None, Some(&ws)).expect("hatch pins");
            assert!(
                hatched
                    .windows(2)
                    .any(|w| w[0] == "-c" && w[1] == "mcp_servers.probe.enabled=false"),
                "the hatch reads the operator's own codex home: {hatched:?}"
            );
            let hatched =
                seat_mcp_pin_flags(SeatCli::Copilot, None, Some(&ws)).expect("hatch pins");
            assert!(
                hatched.iter().any(|f| f == "user-one"),
                "the hatch reads the operator's own copilot home: {hatched:?}"
            );
        }
        // A codex home with no config, and the seats with nothing to pin.
        let bare = base.join("codex-bare");
        std::fs::create_dir_all(&bare).unwrap();
        assert!(seat_mcp_pin_flags(SeatCli::Codex, Some(&bare), None)
            .unwrap()
            .is_empty());
        for cli in [
            SeatCli::Claude,
            SeatCli::Pi,
            SeatCli::Opencode,
            SeatCli::Other,
        ] {
            assert!(
                seat_mcp_pin_flags(cli, Some(&seat), Some(&ws))
                    .unwrap()
                    .is_empty(),
                "{cli:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Fail closed: a config the pin cannot READ, cannot PARSE, or names a server codex's own
    /// `-c` override cannot address refuses the launch. A worker that loads an MCP server the
    /// engine did not hand it is the thing core#657/#660 close; an unreadable file is not a
    /// reason to run one.
    #[test]
    fn a_config_the_mcp_pin_cannot_read_or_address_refuses_the_launch() {
        let base = std::env::temp_dir().join(format!("wicked-660-closed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let seat = base.join("copilot");
        std::fs::create_dir_all(&seat).unwrap();
        for bad in ["not json", "[]", r#"{"mcpServers":[]}"#] {
            std::fs::write(seat.join(COPILOT_MCP_CONFIG_FILE), bad).unwrap();
            let err = seat_mcp_pin_flags(SeatCli::Copilot, Some(&seat), None).expect_err(bad);
            assert!(err.contains(COPILOT_MCP_CONFIG_FILE), "{bad}: {err}");
            assert!(err.contains("refusing it"), "{bad}: {err}");
        }
        // A config that is a DIRECTORY is a read error, not "no servers".
        let dir_seat = base.join("copilot-dir");
        std::fs::create_dir_all(dir_seat.join(COPILOT_MCP_CONFIG_FILE)).unwrap();
        assert!(seat_mcp_pin_flags(SeatCli::Copilot, Some(&dir_seat), None).is_err());

        let codex = base.join("codex");
        std::fs::create_dir_all(&codex).unwrap();
        std::fs::write(codex.join("config.toml"), "this is not = = toml").unwrap();
        let err = seat_mcp_pin_flags(SeatCli::Codex, Some(&codex), None).expect_err("bad toml");
        assert!(err.contains("not valid TOML"), "{err}");
        // A name the `-c` override cannot address: the quoted-key form replaces the table instead
        // of merging into it, and codex then fails to load its own config.
        std::fs::write(
            codex.join("config.toml"),
            "[mcp_servers.\"weird name\"]\ncommand = \"/bin/echo\"\n",
        )
        .unwrap();
        let err = seat_mcp_pin_flags(SeatCli::Codex, Some(&codex), None).expect_err("weird name");
        assert!(
            err.contains("weird name") && err.contains("cannot address"),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(&base);
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

    /// core#660, the ACP half: the codex-acp bridge takes no `-c`, so a codex seat whose config
    /// ENABLES an MCP server — in its seat home or the trusted project's `.codex/config.toml` — is
    /// refused at spawn; `enabled = false` (codex's own switch) or no server at all passes.
    #[test]
    fn an_acp_codex_seat_with_an_enabled_mcp_server_is_refused() {
        // `CODEX_CONFIG` is read from the process environment: hold the env lock against the test
        // that sets it.
        let _env = crate::test_env::ENV_LOCK
            .read()
            .unwrap_or_else(|p| p.into_inner());
        let base = std::env::temp_dir().join(format!("wicked-660-acp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let seat = base.join("codex");
        let ws = base.join("wt");
        std::fs::create_dir_all(&seat).unwrap();
        std::fs::create_dir_all(ws.join(".codex")).unwrap();
        assert_eq!(
            acp_codex_mcp_refusal(Some(&seat), Some(&ws)),
            Ok(()),
            "no config"
        );

        std::fs::write(
            seat.join("config.toml"),
            "[mcp_servers.off]\ncommand = \"/bin/echo\"\nenabled = false\n",
        )
        .unwrap();
        assert_eq!(
            acp_codex_mcp_refusal(Some(&seat), Some(&ws)),
            Ok(()),
            "a disabled server passes"
        );

        std::fs::write(
            seat.join("config.toml"),
            "[mcp_servers.off]\ncommand = \"/bin/echo\"\nenabled = false\n\n[mcp_servers.probe]\ncommand = \"/bin/echo\"\n",
        )
        .unwrap();
        let err = acp_codex_mcp_refusal(Some(&seat), Some(&ws)).unwrap_err();
        assert!(err.contains("`probe`") && !err.contains("`off`"), "{err}");
        assert!(err.contains("enabled = false"), "names the remedy: {err}");

        std::fs::write(
            seat.join("config.toml"),
            "[mcp_servers.off]\ncommand = \"/bin/echo\"\nenabled = false\n",
        )
        .unwrap();
        std::fs::write(
            ws.join(".codex").join("config.toml"),
            "[mcp_servers.repo-one]\ncommand = \"/bin/echo\"\n",
        )
        .unwrap();
        let err = acp_codex_mcp_refusal(Some(&seat), Some(&ws)).unwrap_err();
        assert!(
            err.contains("`repo-one`"),
            "the project layer counts: {err}"
        );

        std::fs::write(ws.join(".codex").join("config.toml"), "not = [toml").unwrap();
        assert!(
            acp_codex_mcp_refusal(Some(&seat), Some(&ws))
                .unwrap_err()
                .contains("not valid TOML"),
            "an unreadable config fails closed"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// codex r1 on #825: the JSON layer codex-acp merges from `CODEX_CONFIG` counts as a source.
    #[test]
    fn a_codex_acp_config_layer_that_enables_a_server_is_refused() {
        assert_eq!(codex_acp_config_mcp_servers(r#"{"model":"x"}"#), Ok(vec![]));
        assert_eq!(
            codex_acp_config_mcp_servers(
                r#"{"mcp_servers":{"a":{"command":"x"},"b":{"command":"y","enabled":false}}}"#
            ),
            Ok(vec!["a".to_string()])
        );
        assert_eq!(
            codex_acp_config_mcp_servers(
                r#"{"mcp_servers.c.command":"x","mcp_servers.d.enabled":false}"#
            ),
            Ok(vec!["c".to_string()])
        );
        assert!(codex_acp_config_mcp_servers("[1]").is_err());
        assert!(codex_acp_config_mcp_servers("not json").is_err());

        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let saved = std::env::var_os(CODEX_ACP_CONFIG_ENV);
        let seat = std::env::temp_dir().join(format!("wicked-660-cc-{}", std::process::id()));
        std::fs::create_dir_all(&seat).unwrap();
        std::env::set_var(
            CODEX_ACP_CONFIG_ENV,
            r#"{"mcp_servers":{"ambient":{"command":"x"}}}"#,
        );
        let err = acp_codex_mcp_refusal(Some(&seat), None).unwrap_err();
        assert!(
            err.contains("`ambient`") && err.contains("CODEX_CONFIG"),
            "{err}"
        );
        match saved {
            Some(v) => std::env::set_var(CODEX_ACP_CONFIG_ENV, v),
            None => std::env::remove_var(CODEX_ACP_CONFIG_ENV),
        }
        let _ = std::fs::remove_dir_all(&seat);
    }
}
