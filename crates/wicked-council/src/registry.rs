//! The CLI registry: built-in verified records ∪ an optional user TOML.
//!
//! Discover, don't hardcode-only. We ship a built-in set of verified seats, but a TOML
//! at `~/.config/wicked-council/clis.toml` (or a path override for tests) is merged on
//! load. User records default to `ConfirmOnProbe` so the probe verifies their headless
//! flag before the council relies on them.
//!
//! The registry record is the de-drift source of truth — flags are encoded here, never
//! re-derived per call. The built-in roster uses the CLIs that actually exist in this
//! environment (**claude, agy, codex, copilot, opencode, pi**) so a real probe can detect
//! them. agy stays listed but council-disabled: it has no working headless/ACP path, so
//! seating it only buys dispatch timeouts (liveness ≠ readiness — `agy --version` answering
//! proves nothing about completing a ballot).

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
    acp: Option<TomlAcpConfig>,
}

/// TOML-only ACP shape. The resolved [`AcpConfig`] deliberately uses a plain bool on the
/// cross-language wire; this mirror retains omission so a same-binary override can inherit an
/// already-proven built-in admission without treating an explicit `false` as omitted.
#[derive(Debug, Deserialize)]
struct TomlAcpConfig {
    binary: String,
    #[serde(default)]
    start_args: Vec<String>,
    #[serde(default)]
    transport: AcpTransport,
    #[serde(default)]
    auth_method: Option<String>,
    #[serde(default)]
    acp_input_governance: Option<bool>,
}

impl From<TomlAcpConfig> for AcpConfig {
    fn from(t: TomlAcpConfig) -> Self {
        Self {
            binary: t.binary,
            start_args: t.start_args,
            transport: t.transport,
            auth_method: t.auth_method,
            acp_input_governance: t.acp_input_governance.unwrap_or(false),
        }
    }
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
            acp: t.acp.map(Into::into),
            capabilities: t.capabilities,
            login_invocation: t.login_invocation,
        }
    }
}

/// The built-in, hand-verified registry. These are the agentic CLIs available in this
/// environment (**claude, agy, codex, copilot, opencode, pi**; agy listed but
/// council-disabled — no working headless/ACP path). The full roster is data, not logic;
/// it grows by appending records here or via the user TOML.
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
                // claude-agent-acp's pinned adapter proof passed DES-INPUT-GOV-001 §3.
                acp_input_governance: true,
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
            // Disabled 2026-09: agy never establishes an ACP session, so every seating
            // costs a full dispatch-budget timeout and re-deliberation pressure.
            // Re-enable once an agy bridge completes a real ballot round-trip.
            enabled_for_council: false,
            // wicked-crew's own bridge (packages/agent-acp-bridges) — no ecosystem
            // adapter exists for Antigravity yet.
            acp: Some(AcpConfig {
                binary: "agy-acp".into(),
                start_args: vec![],
                transport: AcpTransport::Stdio,
                auth_method: None,
                // Unadmitted pending its ACP permission-round-trip proof.
                acp_input_governance: false,
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
            // has no prompt to answer). Approvals/sandboxing are set by `trust_flags`, below.
            headless_invocation: "codex exec --skip-git-repo-check \"{PROMPT}\"".into(),
            category: Category::AgenticCoder,
            input_mode: InputMode::PromptArg,
            version_probe: vec!["codex".into(), "--version".into()],
            // BOUNDED posture, not the full bypass (crew#427). codex is the seat the
            // evaluator≠creator router moves review/test units onto, and its default sandbox is
            // READ-ONLY — every write/temp/socket the verification suite needs is refused, so the
            // reviewer defaults to "not ready". The obvious fix (`--dangerously-bypass-approvals-
            // and-sandbox`, which turns codex's OWN sandbox off) is UNSAFE on the governed-worker
            // path: wicked-core's boundary gate is claude-specific (`gate_hook` PreToolUse), so a
            // codex worker with no sandbox AND no gate would be UNBOUNDED — free to write outside
            // its worktree. `--sandbox workspace-write` is codex's native bounded mode: writes are
            // confined to the workspace (the worktree + in-boundary scratch) and DENIED outside,
            // and `codex exec` is already non-interactive so no approval flag is needed. codex's
            // own workspace-write sandbox is then the boundary — aligned with path_policy's intent
            // that the worktree is the write root — which is safe WITHOUT the claude-only gate.
            // Operators overriding codex in their wicked-council clis.toml (see `default_user_path`;
            // `~/.config/...` on Unix, `%USERPROFILE%\...` on Windows) should mirror this:
            //   trust_flags = ["--sandbox", "workspace-write"]
            trust_flags: vec!["--sandbox".into(), "workspace-write".into()],
            alt_binaries: vec![],
            confidence: Confidence::Verified,
            enabled_for_council: true,
            // Official ACP-org adapter (@agentclientprotocol/codex-acp) — a TS/Node bridge
            // around the `codex` CLI, not a Rust binary.
            acp: Some(AcpConfig {
                binary: "codex-acp".into(),
                start_args: vec![],
                transport: AcpTransport::Stdio,
                auth_method: None,
                // OQ-CODEX-ACP-001 resolved NOT admitted. Five live captures against the pinned
                // codex-acp@1.9.0 (gitHead 67db0d3d) driving codex-cli 0.153.3
                // (see .product/evidence/oq-codex-acp-001/) show the adapter OBSERVE an action's
                // risk and proceed anyway — worse than absent plumbing: an explicit `rm -rf` turn
                // logged codex's own internal reviewer ("Risk: medium, Authorization: high,
                // Approved") and self-deleted the directory with zero session/request_permission
                // round-trips to the client. The permission machinery (CodexApprovalHandler) exists
                // and works, but the default AgentMode's approvalsReviewer:"auto_review" resolves
                // essentially every core read/edit/bash/write intent itself before that machinery is
                // reached — confirmed across ordinary, sandbox-denied, and destructive turns, and
                // even under ReadOnly. Secondary gap: its permission requests carry no canonical
                // tool name, only a human-readable title (often the literal shell command) —
                // pretool_payload can still parse that (title becomes the name), but policy then
                // matches on free shell text instead of a canonical tool identity, a degraded
                // basis for admission. Stays disclosed-ungoverned
                // until a pinned adapter version proves per-call gating for every core intent.
                acp_input_governance: false,
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
                // OQ-PI-ACP-001 resolved NOT admitted: a live capture against the pinned
                // pi-acp@0.0.32 (gitHead 2f6e3c5, see .product/evidence/oq-pi-acp-001/)
                // shows a core `write` tool call go from pending -> in_progress -> completed
                // with zero session/request_permission round-trips, and the shipped
                // adapter source confirms the same path serves read/edit/bash — its
                // requestPermission is invoked only for pi's extension select/confirm UI,
                // never for tool execution. Stays disclosed-ungoverned until a fixed
                // adapter version proves otherwise.
                acp_input_governance: false,
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
                // OQ-COPILOT-ACP-001 must prove permission coverage before admission.
                acp_input_governance: false,
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
                // OQ-OPENCODE-ACP-001 must prove permission coverage before admission.
                acp_input_governance: false,
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
                // Whether the override OMITTED trust_flags entirely (`None`) vs. specified them
                // (`Some`, including an explicit `[]`). An omission must not silently strip a
                // built-in seat's trust posture: a hand-edited override of `codex` that drops
                // `trust_flags` otherwise runs codex in its own read-only sandbox, where every
                // governed unit REFUSES to edit/test/network (crew#419). An explicit `[]` stays
                // deliberately-untrusted.
                let omitted_trust = tcli.trust_flags.is_none();
                // Same omission-vs-specified distinction for `enabled_for_council`: the
                // `From` default is `true`, so a metadata-only override of a DISABLED
                // built-in (agy) would silently re-seat it. An omission inherits the
                // built-in's value; re-enabling takes an explicit `enabled_for_council = true`.
                let omitted_enabled = tcli.enabled_for_council.is_none();
                // A nested option is needed here: `AcpConfig` deliberately resolves the field to
                // bool for consumers, while merging must distinguish omission from explicit false.
                let omitted_acp_gov = tcli
                    .acp
                    .as_ref()
                    .is_some_and(|acp| acp.acp_input_governance.is_none());
                let mut cli: AgenticCli = tcli.into();
                if let Some(slot) = merged.iter_mut().find(|c| c.key == cli.key) {
                    // User record overrides a built-in with the same key.
                    if omitted_trust && !slot.trust_flags.is_empty() {
                        cli.trust_flags = slot.trust_flags.clone();
                        eprintln!(
                            "wicked-council: seat '{}' overrides a built-in that carries \
                             trust_flags but the override omits them — inheriting the built-in's \
                             {:?} (specify `trust_flags = []` to run it deliberately untrusted; \
                             crew#419)",
                            cli.key, cli.trust_flags
                        );
                    }
                    if omitted_enabled && !slot.enabled_for_council {
                        cli.enabled_for_council = false;
                        eprintln!(
                            "wicked-council: seat '{}' overrides a council-disabled built-in \
                             but the override omits enabled_for_council — staying disabled \
                             (set `enabled_for_council = true` to re-seat it deliberately)",
                            cli.key
                        );
                    }
                    if omitted_acp_gov {
                        if let (Some(new_acp), Some(builtin_acp)) =
                            (cli.acp.as_mut(), slot.acp.as_ref())
                        {
                            // A proof belongs to the pinned adapter, never merely its seat key.
                            // An operator who swaps the binary must explicitly re-admit it.
                            if builtin_acp.acp_input_governance
                                && !new_acp.acp_input_governance
                                && new_acp.binary == builtin_acp.binary
                            {
                                new_acp.acp_input_governance = true;
                                eprintln!(
                                    "wicked-council: seat '{}' overrides a built-in whose ACP adapter \
                                     '{}' is admitted to input governance but [cli.acp] omits \
                                     acp_input_governance — inheriting true (set \
                                     acp_input_governance = false to opt out deliberately; \
                                     wicked-core#364)",
                                    cli.key, builtin_acp.binary
                                );
                            }
                        }
                    }
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
        // agy stays listed (roster completeness) but council-disabled: it has no working
        // headless/ACP path, so seating it only buys dispatch timeouts.
        let agy = clis.iter().find(|c| c.key == "agy").unwrap();
        assert!(
            !agy.enabled_for_council,
            "agy must stay council-disabled until its bridge completes a real ballot"
        );
        // Built-ins ship Verified confidence.
        assert!(clis.iter().all(|c| c.confidence == Confidence::Verified));
    }

    #[test]
    fn only_claudes_proven_acp_adapter_is_admitted_in_the_builtin_roster() {
        for cli in builtin() {
            let admitted = cli.acp.as_ref().is_some_and(|acp| acp.acp_input_governance);
            assert_eq!(
                admitted,
                cli.key == "claude",
                "only the pinned Claude adapter has passed ACP input-governance proof"
            );
        }
    }

    #[test]
    fn acp_admission_inherits_only_for_an_omitted_same_binary_override() {
        let dir = std::env::temp_dir().join(format!(
            "wc-acp-admission-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clis.toml");

        let write_override = |binary: &str, flag: Option<bool>| {
            let flag = flag
                .map(|value| format!("acp_input_governance = {value}"))
                .unwrap_or_default();
            std::fs::write(
                &path,
                format!(
                    r#"
[[cli]]
key = "claude"
display_name = "Claude (override)"
binary = "claude"
headless_invocation = "claude -p \"{{PROMPT}}\""

[cli.acp]
binary = "{binary}"
{flag}
"#
                ),
            )
            .unwrap();
        };

        let builtin_binary = builtin()
            .into_iter()
            .find(|cli| cli.key == "claude")
            .and_then(|cli| cli.acp)
            .expect("Claude must ship an ACP config")
            .binary;

        write_override(&builtin_binary, None);
        let merged = load(Some(&path)).unwrap();
        assert!(merged
            .iter()
            .find(|cli| cli.key == "claude")
            .and_then(|cli| cli.acp.as_ref())
            .is_some_and(|acp| acp.acp_input_governance));

        write_override(&builtin_binary, Some(false));
        let merged = load(Some(&path)).unwrap();
        assert!(!merged
            .iter()
            .find(|cli| cli.key == "claude")
            .and_then(|cli| cli.acp.as_ref())
            .is_some_and(|acp| acp.acp_input_governance));

        write_override("deliberately-admitted-acp", Some(true));
        let merged = load(Some(&path)).unwrap();
        assert!(merged
            .iter()
            .find(|cli| cli.key == "claude")
            .and_then(|cli| cli.acp.as_ref())
            .is_some_and(|acp| acp.acp_input_governance));

        write_override("unproven-acp", None);
        let merged = load(Some(&path)).unwrap();
        assert!(!merged
            .iter()
            .find(|cli| cli.key == "claude")
            .and_then(|cli| cli.acp.as_ref())
            .is_some_and(|acp| acp.acp_input_governance));

        let _ = std::fs::remove_dir_all(&dir);
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
    fn metadata_override_of_a_disabled_builtin_stays_disabled() {
        // A user TOML that tweaks agy metadata but OMITS enabled_for_council must not
        // silently re-seat it (the From default is `true`); re-enabling is explicit.
        let dir = std::env::temp_dir().join(format!("wc-registry-agy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clis.toml");
        std::fs::write(
            &path,
            r#"
[[cli]]
key = "agy"
display_name = "Antigravity (renamed)"
binary = "agy"
headless_invocation = "agy -p \"{PROMPT}\""
"#,
        )
        .unwrap();
        let merged = load(Some(&path)).expect("load must succeed");
        let agy = merged.iter().find(|c| c.key == "agy").unwrap();
        assert_eq!(agy.display_name, "Antigravity (renamed)");
        assert!(
            !agy.enabled_for_council,
            "an omitted enabled_for_council must inherit the built-in's disabled state"
        );

        // An EXPLICIT re-enable still wins.
        std::fs::write(
            &path,
            r#"
[[cli]]
key = "agy"
display_name = "Antigravity"
binary = "agy"
headless_invocation = "agy -p \"{PROMPT}\""
enabled_for_council = true
"#,
        )
        .unwrap();
        let merged = load(Some(&path)).expect("load must succeed");
        let agy = merged.iter().find(|c| c.key == "agy").unwrap();
        assert!(
            agy.enabled_for_council,
            "an explicit true must re-seat the CLI"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn override_omitting_trust_flags_inherits_the_builtin_posture() {
        // crew#419: a hand-edited override of `codex` (a built-in that carries a bounded
        // `--sandbox workspace-write` posture) that OMITS trust_flags must not silently run under
        // codex's default read-only sandbox — it inherits the built-in's. An explicit `[]` stays
        // untrusted. Asserted against `builtin()` codex's actual flags, so it holds whatever the
        // built-in posture is.
        let dir = std::env::temp_dir().join(format!(
            "wc-trust-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clis.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"
[[cli]]
key = "codex"
display_name = "Codex (override, trust omitted)"
binary = "codex"
headless_invocation = "codex exec {{PROMPT}}"

[[cli]]
key = "claude"
display_name = "Claude (override, trust explicitly empty)"
binary = "claude"
headless_invocation = "claude -p {{PROMPT}}"
trust_flags = []
"#,
        )
        .unwrap();

        let merged = load(Some(&path)).expect("load must succeed");
        // Assert INHERITANCE against the built-in's actual posture, not a hard-coded string —
        // the point is that the override adopts whatever the built-in codex carries.
        let builtin_codex_trust = builtin()
            .into_iter()
            .find(|c| c.key == "codex")
            .expect("built-in codex seat")
            .trust_flags;
        assert!(
            !builtin_codex_trust.is_empty(),
            "precondition: the built-in codex seat must carry trust_flags for this test to mean anything"
        );
        let codex = merged.iter().find(|c| c.key == "codex").unwrap();
        assert_eq!(
            codex.trust_flags, builtin_codex_trust,
            "an override that OMITS trust_flags inherits the built-in's posture"
        );
        assert_eq!(codex.display_name, "Codex (override, trust omitted)");
        let claude = merged.iter().find(|c| c.key == "claude").unwrap();
        assert!(
            claude.trust_flags.is_empty(),
            "an explicit `trust_flags = []` stays deliberately untrusted"
        );

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
