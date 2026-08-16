//! The CLI registry: built-in verified records ∪ an optional user TOML.
//!
//! Discover, don't hardcode-only. We ship a built-in set of verified seats, but a TOML
//! at `~/.config/wicked-council/clis.toml` (or a path override for tests) is merged on
//! load. User records default to `ConfirmOnProbe` so the probe verifies their headless
//! flag before the council relies on them.
//!
//! The registry record is the de-drift source of truth — flags are encoded here, never
//! re-derived per call. The built-in roster uses the CLIs that actually exist in this
//! environment (**claude, agy, codex, copilot, opencode, pi**) so a real probe can detect them.

use std::path::{Path, PathBuf};

use crate::types::{AcpConfig, AcpTransport, AgenticCli, Category, Confidence, InputMode};
use serde::Deserialize;

/// The shape of the user TOML file: `[[cli]]` array-of-tables.
#[derive(Debug, Deserialize)]
struct TomlRegistry {
    #[serde(default)]
    cli: Vec<TomlCli>,
}

/// One `[[cli]]` table. Mirrors [`AgenticCli`] but every field beyond the four required
/// ones is optional, so a minimal record is valid.
#[derive(Debug, Deserialize)]
struct TomlCli {
    key: String,
    display_name: String,
    binary: String,
    headless_invocation: String,
    #[serde(default)]
    category: Option<Category>,
    #[serde(default)]
    input_mode: Option<InputMode>,
    #[serde(default)]
    version_probe: Option<Vec<String>>,
    #[serde(default)]
    trust_flags: Option<Vec<String>>,
    #[serde(default)]
    alt_binaries: Option<Vec<String>>,
    #[serde(default)]
    confidence: Option<Confidence>,
    #[serde(default)]
    enabled_for_council: Option<bool>,
    #[serde(default)]
    capabilities: Option<String>,
    #[serde(default)]
    login_invocation: Option<String>,
    #[serde(default)]
    acp: Option<AcpConfig>,
}

impl From<TomlCli> for AgenticCli {
    fn from(t: TomlCli) -> Self {
        AgenticCli {
            key: t.key,
            display_name: t.display_name,
            binary: t.binary,
            headless_invocation: t.headless_invocation,
            category: t.category.unwrap_or_default(),
            input_mode: t.input_mode.unwrap_or_default(),
            version_probe: t.version_probe.unwrap_or_default(),
            trust_flags: t.trust_flags.unwrap_or_default(),
            alt_binaries: t.alt_binaries.unwrap_or_default(),
            // User records default to confirm-on-probe.
            confidence: t.confidence.unwrap_or(Confidence::ConfirmOnProbe),
            enabled_for_council: t.enabled_for_council.unwrap_or(true),
            // A user record without [cli.acp] falls back to single-shot; note that an
            // overlay REPLACES its built-in wholesale, so overriding a CLI that has a
            // built-in ACP config requires restating [cli.acp] in the TOML.
            acp: t.acp,
            capabilities: t.capabilities,
            login_invocation: t.login_invocation,
        }
    }
}

/// The built-in, hand-verified registry. These are the agentic CLIs available in this
/// environment (**claude, agy, codex, copilot, opencode, pi**). The full roster is data, not logic; it grows by
/// appending records here or via the user TOML.
pub fn builtin() -> Vec<AgenticCli> {
    vec![
        AgenticCli {
            key: "claude".into(),
            display_name: "Claude Code".into(),
            binary: "claude".into(),
            headless_invocation: "claude -p \"{PROMPT}\"".into(),
            category: Category::AgenticCoder,
            input_mode: InputMode::PromptArg,
            version_probe: vec!["claude".into(), "--version".into()],
            trust_flags: vec!["--dangerously-skip-permissions".into()],
            alt_binaries: vec![],
            confidence: Confidence::Verified,
            enabled_for_council: true,
            acp: Some(AcpConfig {
                binary: "claude-agent-acp".into(),
                start_args: vec![],
                transport: AcpTransport::Stdio,
                auth_method: None,
            }),
            capabilities: Some(
                "broad reasoning, architecture design, TypeScript/React/web, \
                 refactoring, API design, technical writing, multi-file edits"
                    .into(),
            ),
            login_invocation: None,
        },
        AgenticCli {
            key: "agy".into(),
            display_name: "Antigravity".into(),
            binary: "agy".into(),
            // `agy -p` is the documented non-interactive mode; `agy run` spawns the
            // bubbletea TUI and dies headless ("could not open TTY").
            headless_invocation: "agy -p \"{PROMPT}\"".into(),
            category: Category::AgenticCoder,
            input_mode: InputMode::PromptArg,
            version_probe: vec!["agy".into(), "--version".into()],
            trust_flags: vec![],
            alt_binaries: vec![],
            confidence: Confidence::Verified,
            enabled_for_council: true,
            // wicked-crew's own bridge (packages/agent-acp-bridges) — no ecosystem
            // adapter exists for Antigravity yet.
            acp: Some(AcpConfig {
                binary: "agy-acp".into(),
                start_args: vec![],
                transport: AcpTransport::Stdio,
                auth_method: None,
            }),
            capabilities: Some(
                "fast iteration, multi-language code generation, open-source models, \
                 structured output, scripting"
                    .into(),
            ),
            login_invocation: None,
        },
        AgenticCli {
            key: "codex".into(),
            display_name: "Codex".into(),
            binary: "codex".into(),
            // --skip-git-repo-check: unit sandboxes/worktrees are often not git repos;
            // without it codex refuses with "Not inside a trusted directory" (headless
            // has no prompt to answer). Approvals/sandboxing are NOT touched here.
            headless_invocation: "codex exec --skip-git-repo-check \"{PROMPT}\"".into(),
            category: Category::AgenticCoder,
            input_mode: InputMode::PromptArg,
            version_probe: vec!["codex".into(), "--version".into()],
            trust_flags: vec!["--dangerously-bypass-approvals-and-sandbox".into()],
            alt_binaries: vec![],
            confidence: Confidence::Verified,
            enabled_for_council: true,
            // Official ACP-org adapter (@agentclientprotocol/codex-acp, Rust).
            acp: Some(AcpConfig {
                binary: "codex-acp".into(),
                start_args: vec![],
                transport: AcpTransport::Stdio,
                auth_method: None,
            }),
            capabilities: Some(
                "algorithm implementation, Python/JavaScript code generation, \
                 code completion, OpenAI model family"
                    .into(),
            ),
            login_invocation: None,
        },
        AgenticCli {
            key: "pi".into(),
            display_name: "Pi CLI".into(),
            binary: "pi".into(),
            headless_invocation: "pi -p \"{PROMPT}\"".into(),
            category: Category::AgenticCoder,
            input_mode: InputMode::PromptArg,
            version_probe: vec!["pi".into(), "--version".into()],
            trust_flags: vec![],
            alt_binaries: vec![],
            confidence: Confidence::Verified,
            enabled_for_council: true,
            // Community adapter (npm `pi-acp`) — sessions + resumption.
            acp: Some(AcpConfig {
                binary: "pi-acp".into(),
                start_args: vec![],
                transport: AcpTransport::Stdio,
                auth_method: None,
            }),
            capabilities: Some(
                "conversational reasoning, nuanced analysis, cross-language tasks, \
                 explanation and documentation"
                    .into(),
            ),
            login_invocation: None,
        },
        AgenticCli {
            key: "copilot".into(),
            display_name: "GitHub Copilot CLI".into(),
            binary: "copilot".into(),
            headless_invocation: "copilot -p \"{PROMPT}\"".into(),
            category: Category::AgenticCoder,
            input_mode: InputMode::PromptArg,
            version_probe: vec!["copilot".into(), "--version".into()],
            trust_flags: vec![],
            alt_binaries: vec!["gh-copilot".into()],
            confidence: Confidence::Verified,
            enabled_for_council: true,
            // copilot speaks native ACP over stdio (`copilot --acp`): verified initialize /
            // session/new / session/prompt with agent_message_chunk streaming (v1.0.75).
            acp: Some(AcpConfig {
                binary: "copilot".into(),
                start_args: vec!["--acp".into()],
                transport: AcpTransport::Stdio,
                auth_method: None,
            }),
            capabilities: Some(
                "GitHub context, pull request review, commit-level changes, \
                 popular library patterns, IDE-native suggestions"
                    .into(),
            ),
            login_invocation: None,
        },
        AgenticCli {
            key: "opencode".into(),
            display_name: "opencode".into(),
            binary: "opencode".into(),
            headless_invocation: "opencode run \"{PROMPT}\"".into(),
            category: Category::AgenticCoder,
            input_mode: InputMode::PromptArg,
            version_probe: vec!["opencode".into(), "--version".into()],
            trust_flags: vec![],
            alt_binaries: vec![],
            confidence: Confidence::Verified,
            enabled_for_council: true,
            // opencode speaks NATIVE ACP over stdio (`opencode acp`) — no bridge needed.
            acp: Some(AcpConfig {
                binary: "opencode".into(),
                start_args: vec!["acp".into()],
                transport: AcpTransport::Stdio,
                auth_method: None,
            }),
            capabilities: Some(
                "open-source models, local/private code, broad language support, \
                 configurable backends"
                    .into(),
            ),
            login_invocation: None,
        },
    ]
}

/// The default user-registry path: `~/.config/wicked-council/clis.toml`.
/// Returns `None` if a home directory cannot be determined.
pub fn default_user_path() -> Option<PathBuf> {
    // Cross-platform home resolution without the `dirs` crate.
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)?;
    Some(
        home.join(".config")
            .join("wicked-council")
            .join("clis.toml"),
    )
}

/// Load the merged registry: built-ins overlaid with the user TOML at `user_path`
/// (if it exists and parses). On key collision the **user** record wins (the user can
/// override a built-in). A missing file is not an error — built-ins are returned.
///
/// Returns `Err` only if the file exists but cannot be parsed (so a malformed TOML is
/// surfaced honestly rather than silently dropped).
pub fn load(user_path: Option<&Path>) -> Result<Vec<AgenticCli>, String> {
    let mut merged: Vec<AgenticCli> = builtin();

    if let Some(path) = user_path {
        if path.exists() {
            let raw = std::fs::read_to_string(path)
                .map_err(|e| format!("reading {}: {e}", path.display()))?;
            let parsed: TomlRegistry =
                toml::from_str(&raw).map_err(|e| format!("parsing {}: {e}", path.display()))?;
            for tcli in parsed.cli {
                let cli: AgenticCli = tcli.into();
                // User record overrides a built-in with the same key.
                if let Some(slot) = merged.iter_mut().find(|c| c.key == cli.key) {
                    *slot = cli;
                } else {
                    merged.push(cli);
                }
            }
        }
    }

    Ok(merged)
}

/// `registry list` payload: the merged records as JSON, plus counts.
pub fn list_json(clis: &[AgenticCli]) -> serde_json::Value {
    serde_json::json!({
        "count": clis.len(),
        "clis": clis,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn builtin_lists_real_clis() {
        let clis = builtin();
        let keys: Vec<&str> = clis.iter().map(|c| c.key.as_str()).collect();
        // The real CLIs available in this environment.
        assert!(keys.contains(&"claude"), "claude must be a built-in seat");
        assert!(keys.contains(&"agy"), "agy must be a built-in seat");
        assert!(keys.contains(&"pi"), "pi must be a built-in seat");
        // Built-ins ship Verified confidence.
        assert!(clis.iter().all(|c| c.confidence == Confidence::Verified));
    }

    #[test]
    fn load_merges_user_toml_entry() {
        // Write a user TOML with one extra record and one built-in override.
        let dir = std::env::temp_dir().join(format!("wc-registry-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clis.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(
            br#"
[[cli]]
key = "myllm"
display_name = "My Local LLM"
binary = "myllm"
headless_invocation = "myllm \"{PROMPT}\""

[[cli]]
key = "claude"
display_name = "Claude (overridden)"
binary = "claude"
headless_invocation = "claude -p \"{PROMPT}\""
enabled_for_council = false
"#,
        )
        .unwrap();

        let merged = load(Some(&path)).expect("load must succeed");
        let keys: Vec<&str> = merged.iter().map(|c| c.key.as_str()).collect();
        // Built-in is still present, plus the new user record.
        assert!(keys.contains(&"agy"));
        assert!(keys.contains(&"myllm"), "user record must be merged in");

        // The user record defaults to ConfirmOnProbe.
        let myllm = merged.iter().find(|c| c.key == "myllm").unwrap();
        assert_eq!(myllm.confidence, Confidence::ConfirmOnProbe);

        // The collision override took effect (user wins).
        let claude = merged.iter().find(|c| c.key == "claude").unwrap();
        assert_eq!(claude.display_name, "Claude (overridden)");
        assert!(!claude.enabled_for_council);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_returns_builtins() {
        let merged = load(Some(Path::new("/nonexistent/path/clis.toml"))).unwrap();
        assert_eq!(merged.len(), builtin().len());
    }

    /// A user record can carry a `[cli.acp]` table, and an override WITHOUT one strips
    /// the built-in's ACP config (wholesale replacement — the documented semantic).
    #[test]
    fn load_merges_user_toml_acp_config() {
        let dir = std::env::temp_dir().join(format!("wc-registry-acp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clis.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(
            br#"
[[cli]]
key = "claude"
display_name = "Claude (overridden)"
binary = "claude"
headless_invocation = "claude -p \"{PROMPT}\""

[cli.acp]
binary = "my-claude-acp"
start_args = ["--flag"]
transport = "stdio"
auth_method = "gateway"

[[cli]]
key = "codex"
display_name = "Codex (overridden, no acp)"
binary = "codex"
headless_invocation = "codex exec \"{PROMPT}\""
"#,
        )
        .unwrap();

        let merged = load(Some(&path)).expect("load must succeed");

        // The [cli.acp] table parses and rides the override.
        let claude = merged.iter().find(|c| c.key == "claude").unwrap();
        let acp = claude.acp.as_ref().expect("overlay acp must survive merge");
        assert_eq!(acp.binary, "my-claude-acp");
        assert_eq!(acp.start_args, vec!["--flag".to_string()]);
        assert_eq!(acp.transport, AcpTransport::Stdio);
        // FINDING-015: an operator can pin the `authenticate` methodId per ACP server.
        assert_eq!(acp.auth_method.as_deref(), Some("gateway"));
        // …and the built-ins ship WITHOUT one — the default is the agent's own first
        // advertised method, not a hardcoded guess about someone else's auth surface.
        assert!(builtin()
            .iter()
            .filter_map(|c| c.acp.as_ref())
            .all(|a| a.auth_method.is_none()));

        // An override without [cli.acp] replaces the built-in wholesale — ACP stripped.
        let codex = merged.iter().find(|c| c.key == "codex").unwrap();
        assert!(
            codex.acp.is_none(),
            "override without [cli.acp] strips the built-in ACP config"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
