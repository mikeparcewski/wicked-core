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
            // User TOML records carry no ACP config; the engine falls back to single-shot.
            acp: None,
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
            }),
        },
        AgenticCli {
            key: "agy".into(),
            display_name: "Antigravity".into(),
            binary: "agy".into(),
            headless_invocation: "agy run \"{PROMPT}\"".into(),
            category: Category::AgenticCoder,
            input_mode: InputMode::PromptArg,
            version_probe: vec!["agy".into(), "--version".into()],
            trust_flags: vec![],
            alt_binaries: vec![],
            confidence: Confidence::Verified,
            enabled_for_council: true,
            acp: Some(AcpConfig {
                binary: "agy-acp".into(),
                start_args: vec![],
                transport: AcpTransport::Stdio,
            }),
        },
        AgenticCli {
            key: "codex".into(),
            display_name: "Codex".into(),
            binary: "codex".into(),
            headless_invocation: "codex exec \"{PROMPT}\"".into(),
            category: Category::AgenticCoder,
            input_mode: InputMode::PromptArg,
            version_probe: vec!["codex".into(), "--version".into()],
            trust_flags: vec!["--dangerously-bypass-approvals-and-sandbox".into()],
            alt_binaries: vec![],
            confidence: Confidence::Verified,
            enabled_for_council: true,
            acp: Some(AcpConfig {
                binary: "codex-acp".into(),
                start_args: vec![],
                transport: AcpTransport::Stdio,
            }),
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
            acp: Some(AcpConfig {
                binary: "pi-acp".into(),
                start_args: vec![],
                transport: AcpTransport::Stdio,
            }),
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
            // copilot uses its built-in HTTP ACP server on a dynamic port.
            acp: Some(AcpConfig {
                binary: "copilot".into(),
                start_args: vec!["--acp".into(), "--port".into(), "3000".into()],
                transport: AcpTransport::Http,
            }),
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
            // opencode exposes ACP via its HTTP server; pending transport implementation.
            acp: None,
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
}
