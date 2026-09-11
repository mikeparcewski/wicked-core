//! The one place a child process's inherited environment is decided.
//!
//! # Why this exists
//!
//! FINDING-067: a governed worker ran `wicked-estate index .` and deleted all 833 nodes of the
//! platform's operational state. The worker needed no `--db` argument — it inherited
//! `WICKED_ESTATE_DB` from the launcher, and every estate binary resolves `--db` ELSE that variable.
//!
//! The fix landed at the two launch paths that were known to matter (the wrapped worker and the ACP
//! worker). Enumerating `Command::new` afterwards found **three more** agent-facing spawn sites that
//! inherited the same environment and had never been considered: council seats
//! (`wicked-council::dispatch`), workflow tool phases (`actor::run_tool_cmd`, which runs an arbitrary
//! argv straight out of a `WorkflowDef`), and source recon (`sources::run_cli`).
//!
//! That is the actual defect. Not "the worker path leaked a variable" — *"hardening is applied at
//! call sites, so it covers exactly the paths someone remembered."* Every new spawn site starts
//! un-hardened and stays that way until an incident finds it.
//!
//! # The rule
//!
//! **Every `Command::new` in this workspace is followed by [`HardenedCommand::hardened`].** No
//! allowlist, no exceptions, not even for `git`.
//!
//! The exception-free form is deliberate. An allowlist ("git is harmless") is a standing invitation
//! to argue a new site onto it, and the argument is always locally reasonable — the incident above
//! happened because passing the operational store to the worker's MCP was locally reasonable too. A
//! rule with no exceptions is one a test can enforce mechanically, and mechanical enforcement is the
//! only kind that survives someone adding a spawn site at 2am. Stripping six variables from a `git`
//! invocation costs nothing.
//!
//! [`enforced_by_test`] is the enforcement: it fails the build when a new spawn site appears without
//! the call.
//!
//! # Ordering
//!
//! `hardened()` clears; it does not decide policy. A path that legitimately needs to hand a child one
//! of these variables sets it **after**:
//!
//! ```ignore
//! let mut cmd = Command::new(exe);
//! cmd.hardened();                                  // start from a known-clean slate
//! cmd.env(GATE_DB_ENV, &gov.db_path);              // then pass exactly what this path intends
//! ```
//!
//! Inverting that order silently restores whatever the parent happened to export, which is the
//! condition this module exists to remove. A boundary that depends on the daemon's environment is not
//! a boundary.

use std::process::Command;

/// Variables the engine uses to talk to *itself* — none of which any child may inherit by accident.
///
/// Membership test: would a process that reads this variable, having inherited rather than been
/// handed it, act on state belonging to the engine instead of state belonging to its own job? If yes,
/// it belongs here. That covers both stores and the gate-hook's argument channel — the hook is a
/// *grandchild* of the worker CLI, so the only way to reach it is through the worker's environment,
/// which means every tool the worker spawns sees those variables too.
///
/// Spelled as literals rather than imported from `wicked-core::gate_hook`, because this crate is
/// *below* that one in the dependency graph — council depends on this crate and cannot see the root.
/// The duplication is pinned by [`enforced_by_test`]'s sibling assertion in the root crate, which
/// compares these strings against the consts. Two spellings with a test between them; not two
/// spellings and a comment asking for discipline.
pub const ENGINE_INTERNAL_ENV: &[&str] = &[
    // The operational store, and the variable that caused FINDING-067.
    "WICKED_ESTATE_DB",
    // The gate hook's store. Only the hook subprocess may resolve this; a worker's own tools seeing
    // it is the same leak wearing a different name.
    "WICKED_GATE_DB",
    // The append-only decisions log. A child that inherits this can forge gate decisions.
    "WICKED_DECISIONS_PATH",
    // The hook's argument channel. Inherited, these silently re-scope another unit's governance onto
    // whatever the child does.
    "WICKED_GATE_SCOPE",
    "WICKED_GATE_PHASE",
    "WICKED_GATE_PHASE_ID",
    // The unit's filesystem boundary. Inherited at an unrelated spawn site these silently re-scope
    // one unit's worktree onto another child, and a child that can merely OBSERVE them learns
    // exactly where the fence is. The launcher sets them deliberately on the governed child.
    "WICKED_WRITE_ROOTS",
    "WICKED_READ_ROOTS",
    // The unit's PHASE SCOPE (core#296) — the flag that refuses a pre-build phase's write to a
    // non-documentation path. Inherited at an unrelated spawn site it would scope a phase that is
    // supposed to write code away from writing it, which is the INVERSE failure and a louder one.
    "WICKED_PRE_BUILD_SCOPE",
];

/// The env var the engine's ACP spawn path resolves the persistent worker config home from
/// (`wicked-core`'s `worker_config_home`): unset ⇒ `<home>/.wicked-worker`, set ⇒ that base.
/// Owned HERE (below the root in the dependency graph) so the root's runtime resolution and the
/// shared test-support arming below spell the variable once.
pub const WORKER_HOME_ENV: &str = "WICKED_WORKER_HOME";

/// The env var claude's CLI and Agent SDK resolve their per-user configuration directory from —
/// user-scope settings, hooks, plugins, memory, and the LOGIN. It decides WHOSE configuration a
/// claude process runs under, and therefore whether it is signed in at all. The ACP bridge hands
/// its own environment to the SDK it drives (`CLAUDE_CONFIG_DIR = process.env.CLAUDE_CONFIG_DIR ??
/// homedir()`), and the headless CLI reads the same variable, so this ONE carrier reaches every
/// claude seat the engine spawns — the worker (`wicked-core::acp_runner`) and the council ballot
/// (`wicked-council::dispatch`) alike. Owned here, below both in the dependency graph, so they
/// spell it once.
pub const CLAUDE_CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";

/// Set to any value to let workers AND council seats run under the operator's own CLI
/// configuration again.
///
/// The escape hatch for the one legitimate case: an operator deliberately testing their own hooks
/// or skills through a run. It is opt-IN because the safe default has to be the one you get by not
/// knowing this exists. ONE hatch for every seat spawn — the wrapped worker's argv isolation, the
/// ACP worker's config-dir override and the council ballot's config-dir override all read it here —
/// because two opt-outs for one boundary is how one of them silently stops working.
pub const INHERIT_OPERATOR_CONFIG_ENV: &str = "WICKED_WORKER_INHERIT_OPERATOR_CONFIG";

/// Has the operator pulled the [`INHERIT_OPERATOR_CONFIG_ENV`] escape hatch?
pub fn inherits_operator_config() -> bool {
    std::env::var_os(INHERIT_OPERATOR_CONFIG_ENV).is_some()
}

/// The BASE of the engine-owned worker home: [`WORKER_HOME_ENV`] when set, else
/// `<HOME | USERPROFILE>/.wicked-worker`. ALWAYS an absolute path: an empty or relative override
/// (or home directory) is a configuration error and is REFUSED — the three consumers (the ACP
/// worker spawn, the council ballot spawn, the roster's sign-in command) would each resolve a
/// relative dir against a different working directory and silently disagree on which directory
/// "the worker home" is (codex review, PR#413). Deliberately NOT canonicalized: the home may not
/// exist yet on a fresh install (the ACP spawn creates it), and following symlinks is exactly what
/// [`refuse_symlinked_home`] forbids.
///
/// Reads the process environment; [`worker_home_base_from`] is the pure core for callers (and
/// tests) that already hold the values.
pub fn worker_home_base() -> anyhow::Result<std::path::PathBuf> {
    worker_home_base_from(
        std::env::var_os(WORKER_HOME_ENV),
        std::env::var_os("HOME"),
        std::env::var_os("USERPROFILE"),
    )
}

/// [`worker_home_base`] over explicit values: `override_base` is [`WORKER_HOME_ENV`]'s value,
/// `home`/`userprofile` the two spellings of the home directory. The platform's NATIVE spelling
/// is consulted first — `USERPROFILE` on Windows (always `C:\Users\<u>`; a Windows host's `HOME`,
/// when set at all, is frequently a Git-Bash/MSYS `/c/Users/<u>` that is not absolute in the
/// Windows sense), `HOME` everywhere else — with the other as the fallback. Same order Rust's own
/// `home_dir` uses.
pub fn worker_home_base_from(
    override_base: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
    userprofile: Option<std::ffi::OsString>,
) -> anyhow::Result<std::path::PathBuf> {
    if let Some(base) = override_base {
        return absolute_or_refuse(std::path::PathBuf::from(base), WORKER_HOME_ENV);
    }
    let (primary, secondary) = if cfg!(windows) {
        (("USERPROFILE", userprofile), ("HOME", home))
    } else {
        (("HOME", home), ("USERPROFILE", userprofile))
    };
    let (source, home) = match (primary, secondary) {
        ((name, Some(v)), _) | ((_, None), (name, Some(v))) => (name, v),
        ((_, None), (_, None)) => anyhow::bail!("neither HOME nor USERPROFILE is set"),
    };
    Ok(absolute_or_refuse(std::path::PathBuf::from(home), source)?.join(".wicked-worker"))
}

/// The worker home must be spelled absolutely AND normally by whichever variable supplied it: no
/// `.` or `..` segments (codex r2, PR#413 — `..` re-aims the resolved dir outside the declared base
/// while still reading as absolute). Not `fs::canonicalize`d: that FOLLOWS links, which is exactly
/// what [`refuse_symlinked_home`] exists to refuse, and the home may not exist yet.
fn absolute_or_refuse(
    path: std::path::PathBuf,
    source: &str,
) -> anyhow::Result<std::path::PathBuf> {
    if path.as_os_str().is_empty() {
        anyhow::bail!("{source} is set but empty; the worker home must be an absolute path");
    }
    if !path.is_absolute() {
        anyhow::bail!(
            "{source}={} is a relative path; the worker home must be absolute (ACP workers, council \
             ballots and the sign-in command would each resolve it against a different working \
             directory)",
            path.display()
        );
    }
    refuse_dot_segments(&path, source)?;
    Ok(path)
}

/// Refuse a path spelled with a `.` or `..` SEGMENT — judged on the literal spelling:
/// `Path::components()` silently normalizes `.` away, and a `..` that survives it would be resolved
/// by the kernel at every consumer independently, re-aiming the directory outside whatever base the
/// spelling appeared to sit under. The one rule the worker home, the seat roots and (core#410
/// hardening) a chat scope's cwd, read roots and graph all apply before any containment check; the
/// message names `source` (the requirement's owner: `WICKED_WORKER_HOME`, `read root`, …), never a
/// fixed role (Copilot, #435).
pub fn refuse_dot_segments(path: &std::path::Path, source: &str) -> anyhow::Result<()> {
    let spelled = path.as_os_str().to_string_lossy();
    if spelled
        .split(['/', '\\'])
        .any(|segment| segment == "." || segment == "..")
    {
        anyhow::bail!(
            "{source}={} contains a `.` or `..` segment; spell it as a plain absolute path (a `..` \
             re-aims the resolved directory outside the declared base)",
            path.display()
        );
    }
    Ok(())
}

/// Refuse a worker config home reached through a PLANTED symlink at ANY component — judged on
/// `symlink_metadata` (never a following stat), walking from the filesystem root of the declared
/// path down to the leaf (codex r2, PR#413: checking only the leaf and its parent let an
/// intermediate link re-aim `CLAUDE_CONFIG_DIR` and the ACP sanitization outside the declared
/// target). A link planted at any component re-aims every write the CLI makes there AND every
/// credential it reads: `<worker home>/claude -> ~/.claude` would hand a seat the OPERATOR's
/// login. ONE check for every consumer — applied by [`worker_claude_config_dir`] itself, and again
/// by the ACP spawn's `ensure_worker_config_home` after it creates the home.
///
/// "Planted" means writable by the user this engine runs as. A symlink component owned by root
/// (unix uid 0 — macOS's `/var -> /private/var` and `/tmp -> /private/tmp`, an NFS automount's
/// `/home/<u>`) is system-managed and cannot be planted by a same-uid attacker; it is followed.
/// Every other symlink component is refused by name. On non-unix hosts (no owner uid to judge)
/// every symlink component is refused. A missing component ends the walk — nothing below it exists
/// yet; the ACP spawn creates the home on first start.
pub fn refuse_symlinked_home(dir: &std::path::Path) -> anyhow::Result<()> {
    let mut probe = std::path::PathBuf::new();
    for component in dir.components() {
        probe.push(component.as_os_str());
        // A bare Windows drive prefix (`C:`) is not a filesystem entry — it is stat'ed together
        // with the root separator on the next component (`C:\`).
        if matches!(component, std::path::Component::Prefix(_)) {
            continue;
        }
        match std::fs::symlink_metadata(&probe) {
            Ok(m) if m.file_type().is_symlink() => {
                if symlink_is_planted(&m) {
                    anyhow::bail!(
                        "refusing worker config home {}: {} is a symlink",
                        dir.display(),
                        probe.display()
                    );
                }
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(e) => {
                anyhow::bail!(
                    "refusing worker config home {}: cannot stat {} ({e})",
                    dir.display(),
                    probe.display()
                );
            }
        }
    }
    Ok(())
}

/// Whether a symlink with these (non-following) metadata could have been planted by the user this
/// engine runs as: on unix, any owner but root; elsewhere, always.
fn symlink_is_planted(meta: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.uid() != 0
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        true
    }
}

/// The claude seat's configuration directory — `<worker home base>/claude` — the ONE answer to
/// "which configuration, and so whose login, does a claude process the engine spawns run under".
///
/// Persistent and engine-owned (crew#267 option 3): the operator signs THIS directory in once
/// (their own browser OAuth; the engine never reads, copies or holds credentials) and every
/// claude spawn stays logged in. Three consumers, one resolver:
///
///  - the ACP worker spawn (`wicked-core::acp_runner`), which also creates and re-sanitizes the
///    directory on every start;
///  - the council ballot spawn (`wicked-council::dispatch`), which sets it on every CLAUDE seat
///    (F-030: it used to inherit the daemon's `CLAUDE_CONFIG_DIR` — on a fresh install the
///    never-signed-in dir garden is registered in — so every claude ballot exited 1 "Not logged
///    in" and the seat was benched on every council while the worker path, resolving the worker
///    home, ran fine);
///  - the roster's claude `login_invocation` (`wicked-council::types::default_login_invocation`),
///    so the command the studio shows an operator signs in EXACTLY the directory the seats run
///    under (F-013: it used to hard-code `$HOME/.wicked-worker/claude`, wrong under
///    [`WORKER_HOME_ENV`]).
///
/// The ONE validated form every consumer uses (codex r2, PR#413): absolute and normally spelled
/// (see [`worker_home_base`]) AND no-follow checked at every component ([`refuse_symlinked_home`])
/// — so the ACP spawn, the ballot spawn, the wrapped worker and the sign-in command all name, and
/// run under, the same directory. Creating the directory (private, re-sanitized) stays with the ACP
/// spawn path; a ballot on a not-yet-created home simply runs a CLI that is not signed in there,
/// which is then reported as such.
pub fn worker_claude_config_dir() -> anyhow::Result<std::path::PathBuf> {
    let dir = worker_home_base()?.join("claude");
    refuse_symlinked_home(&dir)?;
    Ok(dir)
}

/// The [`CLAUDE_CONFIG_DIR_ENV`] value a CLAUDE seat spawn sets, after `hardened()`: `None` under
/// the operator's explicit [`INHERIT_OPERATOR_CONFIG_ENV`] hatch (inherit — the operator's own
/// configuration IS the intent), else the validated [`worker_claude_config_dir`]. An `Err` is the
/// resolver failing (no home directory, a relative or `..` override, a planted link at any
/// component); callers fail CLOSED on it — a seat that proceeded would run under the daemon's
/// inherited configuration, or under whatever directory a link points at, which are the exact leaks
/// this exists to remove. Carrier-agnostic: [`claude_config_for_carrier`] is the seat-aware form
/// spawns use.
pub fn seat_claude_config_dir() -> Option<anyhow::Result<std::path::PathBuf>> {
    if inherits_operator_config() {
        return None;
    }
    Some(worker_claude_config_dir())
}

/// Whether `bin` names claude: judged on the file STEM so `claude`, `/usr/local/bin/claude`,
/// `claude.exe` and `claude.cmd` all resolve, and `claude-code-wrapper` does not. THE carrier test
/// every path applies — the wrapped runner to its template's first token
/// (`wicked-core::execute_wrapped::binary_is_claude` delegates here), the ACP runner to the seat
/// record's `binary`, the council ballot to the program it is about to exec. Known boundary (M7):
/// a claude-compatible binary under another name is not recognised.
///
/// The comparison follows the OS's own executable lookup (codex r3, PR#413): case-INSENSITIVE on
/// Windows, where `CLAUDE.EXE` and `Claude.cmd` launch the same program as `claude.exe` — an
/// exact match there would classify them `NotClaude`, strip `CLAUDE_CONFIG_DIR` from a claude
/// process and leave it on the OPERATOR's home config; exact everywhere else, where the filesystem
/// is case-sensitive and `Claude` is a different binary from `claude`.
pub fn binary_is_claude(bin: &str) -> bool {
    // ONE stem judgement for every seat (core#410): the claude test is the `SeatCli` resolver
    // narrowed to its claude arm, so the carrier test and the per-seat configuration decision can
    // never classify the same binary two ways.
    SeatCli::from_binary(bin) == SeatCli::Claude
}

/// What a spawn of one carrier does about [`CLAUDE_CONFIG_DIR_ENV`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CarrierClaudeConfig {
    /// Not a claude carrier (codex, pi, copilot, opencode, …): it never reads the variable, so it
    /// gets NO ambient claude configuration path at all — callers STRIP an inherited one rather
    /// than hand a foreign process the daemon's (or the operator's) claude config dir for nothing.
    NotClaude,
    /// A claude carrier under the operator's inherit hatch: keep the operator's own configuration.
    Inherit,
    /// A claude carrier: set the variable to this validated worker dir.
    Dir(std::path::PathBuf),
}

/// The seat-aware decision for one spawn, from the binary it is about to run (codex review,
/// PR#413: the ballot used to export the claude dir to EVERY seat). `Err` only for a claude carrier
/// whose dir cannot be resolved or validated — fail closed.
pub fn claude_config_for_carrier(carrier_binary: &str) -> anyhow::Result<CarrierClaudeConfig> {
    // The seat resolver narrowed to claude (core#410): same hatch, same validated dir, same
    // fail-closed `Err` — kept as the claude-only view its tests and the roster's sign-in
    // command read; every spawn path applies the full [`SeatConfig`] instead.
    match seat_config_for_carrier(carrier_binary)? {
        SeatConfig::Isolated {
            cli: SeatCli::Claude,
            root: Some(dir),
            ..
        } => Ok(CarrierClaudeConfig::Dir(dir)),
        SeatConfig::Isolated { .. } => Ok(CarrierClaudeConfig::NotClaude),
        SeatConfig::Inherit if binary_is_claude(carrier_binary) => Ok(CarrierClaudeConfig::Inherit),
        SeatConfig::Inherit => Ok(CarrierClaudeConfig::NotClaude),
    }
}

// ── Per-seat configuration roots (core#410 — F-010 / F-068) ─────────────────────────────────────
//
// FINDING-061 / F-030 isolated the CLAUDE seat: `CLAUDE_CONFIG_DIR` points every claude spawn at the
// engine-owned worker home. Every OTHER seat kept running on the operator's OWN configuration —
// `~/.codex`, `~/.pi/agent`, `~/.copilot`, `~/.config/opencode` + `~/.local/share/opencode` — so a
// chat seat loaded the operator's personal skills and extensions (a retired skill set, F-068) and
// streamed a startup banner listing them into the customer's answer; and with a fresh
// `WICKED_WORKER_HOME` the roster reported claude `signed_in:false` but the others `signed_in:true`,
// because their credentials still lived under HOME (F-010). Each CLI has its own configuration-home
// variable. This section names them ONCE and decides, per seat, what a spawn SETS and what it
// STRIPS — for the ACP worker/chat spawn, the council ballot and the wrapped worker alike, and for
// the roster's sign-in command, which must name the very directory the seats run under.

/// codex's configuration home (`~/.codex` by default): `config.toml`, skills, sessions AND
/// `auth.json` — relocating it relocates the login too, so the seat root is signed in once
/// (`wicked-council::types::default_login_invocation`), exactly like the claude worker home.
pub const CODEX_HOME_ENV: &str = "CODEX_HOME";
/// pi's agent directory (`~/.pi/agent` by default): settings, skills, extensions, prompts,
/// sessions AND `auth.json`. Verified against the installed `pi-coding-agent` bundle, which reads
/// exactly this variable for its agent dir.
pub const PI_AGENT_DIR_ENV: &str = "PI_CODING_AGENT_DIR";
/// copilot's configuration home (`~/.copilot` by default): `config.json` (the recorded logged-in
/// users), `mcp-config.json`, skills, agents and the package cache. Its OAuth token lives in the
/// OS keychain, which is per-USER: a copilot seat root isolates the configuration, not the
/// keychain entry (documented limitation).
pub const COPILOT_HOME_ENV: &str = "COPILOT_HOME";
/// opencode resolves its GLOBAL configuration from `$XDG_CONFIG_HOME/opencode` (`opencode.json`,
/// agents, plugins, skills, commands), its credential store from
/// `$XDG_DATA_HOME/opencode/auth.json` and its state from `$XDG_STATE_HOME/opencode`. Its own
/// `OPENCODE_CONFIG_DIR` only ADDS a directory to the load order — the global one is still read
/// (verified in the installed binary's config loader) — so isolation has to move the XDG bases.
/// Side effect, documented: tools an opencode seat spawns (git, gh) resolve THEIR XDG-based
/// configuration from the seat root too; `~/.gitconfig` and a `GH_TOKEN` still apply.
pub const XDG_CONFIG_HOME_ENV: &str = "XDG_CONFIG_HOME";
/// See [`XDG_CONFIG_HOME_ENV`].
pub const XDG_DATA_HOME_ENV: &str = "XDG_DATA_HOME";
/// See [`XDG_CONFIG_HOME_ENV`].
pub const XDG_STATE_HOME_ENV: &str = "XDG_STATE_HOME";
/// opencode's extra-config-directory knob — STRIPPED from every seat: an opencode seat's
/// configuration is the seat root's `XDG_CONFIG_HOME/opencode`, and a foreign seat never reads it.
pub const OPENCODE_CONFIG_DIR_ENV: &str = "OPENCODE_CONFIG_DIR";
/// opencode's INLINE configuration — a complete config document in the environment, plugins and
/// permission rules included (Copilot, #426). Stripped from every isolated seat like the other
/// seat variables; the spawn paths then set exactly the value they intend (the seat's registry
/// governance content, or the skills-composed document) AFTER [`SeatConfig::apply`]. Spelled here,
/// below the root crate (`wicked_core::skills_snapshot::OPENCODE_CONFIG_ENV` is the runtime
/// spelling; a root-crate test pins the two equal).
pub const OPENCODE_CONFIG_CONTENT_ENV: &str = "OPENCODE_CONFIG_CONTENT";
/// opencode's ADDITIONAL config FILE (loaded after the global one — plugins, MCP servers, permission
/// rules) and its INLINE credential store (consulted before `auth.json`) — both read by opencode
/// 1.17.18 (independent review, C2) and both stripped from every isolated seat like the other seat
/// variables.
pub const OPENCODE_CONFIG_FILE_ENV: &str = "OPENCODE_CONFIG";
/// See [`OPENCODE_CONFIG_FILE_ENV`].
pub const OPENCODE_AUTH_CONTENT_ENV: &str = "OPENCODE_AUTH_CONTENT";
/// agy (Antigravity) has no configuration-home variable, so it is isolated by stripping alone — but
/// it does have quiet flags (brief §5: a CLI without a config-home override at least runs quiet):
/// these hide its logo and account banner so neither reaches a transcript. Set on every isolated
/// agy seat by [`SeatConfig::apply`]. Its configuration lives under the operator's `~/.gemini/…`.
pub const AGY_HIDE_LOGO_ENV: &str = "AGY_CLI_HIDE_LOGO";
/// See [`AGY_HIDE_LOGO_ENV`].
pub const AGY_HIDE_ACCOUNT_INFO_ENV: &str = "AGY_CLI_HIDE_ACCOUNT_INFO";

/// (F-7R2-012, wave 6) Remote-write CREDENTIALS a seat process never inherits. A worker seat
/// opened a GitHub PR from its own shell (`gh pr create`, `git push`) on the daemon's ambient
/// `gh` login — delivery is the ENGINE's job (the deliver tool phase lifts, re-verifies and
/// pushes the run branch, then opens the PR, so the ledger records it). These are stripped from
/// EVERY seat spawn — wrapped worker, ACP bridge, council ballot — by [`SeatConfig::apply`]; the
/// deliver tool phase (`actor::run_tool_cmd`) is not a seat, applies no `SeatConfig`, and keeps
/// the daemon's credentials, which is exactly the asymmetry the fence is built on.
pub const REMOTE_CREDENTIAL_ENV: &[&str] = &[
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
    // (review of #449, FN-1) The ssh and git-credential paths: an agent-held key, an askpass
    // helper, a seat-supplied ssh command, git's own environment-injected config, a re-pointed
    // subcommand directory — every one a way to push around the gh-mediated https login.
    "SSH_AUTH_SOCK",
    "SSH_AGENT_PID",
    "SSH_ASKPASS",
    "GIT_ASKPASS",
    "GIT_SSH",
    "GIT_SSH_COMMAND",
    "GIT_SSH_VARIANT",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_NOSYSTEM",
    "GIT_EXEC_PATH",
    "GIT_PROXY_COMMAND",
];

/// Name PREFIXES of remote-write credential variables stripped by enumeration (their suffix is
/// a counter): `GIT_CONFIG_KEY_n` / `GIT_CONFIG_VALUE_n` (git's environment-injected config —
/// the `GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=alias.p …` bypass) and any `GIT_CREDENTIAL*`.
pub const REMOTE_CREDENTIAL_ENV_PREFIXES: &[&str] =
    &["GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_", "GIT_CREDENTIAL"];

/// The scheme every seat push is rewritten to — one git has no remote helper for, so the push
/// fails before any transport is contacted ("unable to find remote helper for 'wicked-nopush'").
pub const NOPUSH_SCHEME: &str = "wicked-nopush://";

/// URL prefixes rewritten for PUSH ONLY (`url.<NOPUSH_SCHEME>.pushInsteadOf`): every transport
/// git speaks — https/http, ssh (URL and scp-like `git@host:` forms), the git protocol, `file://`
/// and an absolute POSIX-path remote. `insteadOf` is NOT touched: fetch and pull keep working on
/// every one of them. A seat fetches from its remotes; it never pushes to them. See
/// [`nopush_url_prefixes`] for the full list — this base plus the Windows path spellings.
pub const NOPUSH_URL_PREFIXES: &[&str] = &[
    "https://",
    "http://",
    "ssh://",
    "git+ssh://",
    "ssh+git://",
    "git://",
    "git@",
    "file://",
    "/",
];

/// EVERY push-killed URL prefix: [`NOPUSH_URL_PREFIXES`] plus the Windows spellings of a local
/// path remote — a drive letter (`C:`/`c:`, which git normalises to `C:/…` in remote URLs; the
/// CI Windows runner's `%TEMP%` bare remote is exactly this) and a UNC share (`\\`). Listed on
/// every platform: a remote URL beginning `X:` or `\\` means a Windows path nowhere else, so
/// the entries are inert where they do not apply and the seat config is identical everywhere.
pub fn nopush_url_prefixes() -> Vec<String> {
    let mut all: Vec<String> = NOPUSH_URL_PREFIXES
        .iter()
        .map(|p| (*p).to_string())
        .collect();
    for letter in b'A'..=b'Z' {
        all.push(format!("{}:", letter as char));
        all.push(format!("{}:", (letter as char).to_ascii_lowercase()));
    }
    all.push("\\\\".to_string());
    all
}

/// The `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_n`/`GIT_CONFIG_VALUE_n` entries a seat spawn carries —
/// git reads them with the precedence of `-c` (above every config FILE), so a repo-level
/// `credential.helper` or `url.*.pushInsteadOf` cannot undo them: the push kill for every
/// transport, credential helpers reset (an empty `credential.helper` clears the list), no askpass
/// program. Returned as `(name, value)` pairs, `GIT_CONFIG_COUNT` last.
pub fn nopush_git_config_env() -> Vec<(String, String)> {
    let mut entries: Vec<(String, String)> = nopush_url_prefixes()
        .iter()
        .map(|p| {
            (
                format!("url.{NOPUSH_SCHEME}.pushInsteadOf"),
                (*p).to_string(),
            )
        })
        .collect();
    entries.push(("credential.helper".to_string(), String::new()));
    entries.push(("core.askPass".to_string(), String::new()));
    let mut env: Vec<(String, String)> = Vec::with_capacity(entries.len() * 2 + 1);
    for (i, (key, value)) in entries.iter().enumerate() {
        env.push((format!("GIT_CONFIG_KEY_{i}"), key.clone()));
        env.push((format!("GIT_CONFIG_VALUE_{i}"), value.clone()));
    }
    env.push(("GIT_CONFIG_COUNT".to_string(), entries.len().to_string()));
    env
}

/// The seat-owned git config files a seat spawn is pointed at (`GIT_CONFIG_GLOBAL`,
/// `GIT_CONFIG_SYSTEM`), under [`seat_gh_config_dir`]: the GLOBAL file includes the operator's
/// own global config (so `user.name`/`user.email`, `core.*`, `diff.*` keep working for the seat's
/// commits) and then RESETS the credential helpers, askpass and the push transports; the SYSTEM
/// file is empty (a system `credential.helper = manager`, as Git for Windows installs, never
/// reaches a seat). Written idempotently on every spawn; `Err` when the worker home base cannot
/// be resolved or the files cannot be written — callers then rely on the environment entries
/// alone (which already outrank every file).
pub fn seat_git_config_files() -> anyhow::Result<(std::path::PathBuf, std::path::PathBuf)> {
    use anyhow::Context;
    let dir = seat_gh_config_dir()?;
    ensure_private_dir(&dir)?;
    let global = dir.join("gitconfig");
    let system = dir.join("gitconfig-system");
    let mut content = String::from(
        "# Written by wicked-core on every seat spawn (F-7R2-012, wave 6). A worker seat's git\n\
         # reads THIS as its global config: the operator's own global config is included below\n\
         # (identity, editor, diff settings keep working), then every credential helper and\n\
         # askpass program is reset and every push transport is re-aimed at a scheme git has no\n\
         # helper for. Fetch and pull are untouched. Delivery is the deliver phase's job.\n",
    );
    for path in operator_global_git_configs() {
        // git's include syntax accepts forward slashes on every platform; a backslash would be
        // read as an escape.
        let spelled = path.to_string_lossy().replace('\\', "/");
        content.push_str(&format!("[include]\n\tpath = {spelled}\n"));
    }
    content.push_str("[credential]\n\thelper =\n[core]\n\taskPass =\n");
    content.push_str(&format!("[url \"{NOPUSH_SCHEME}\"]\n"));
    for prefix in nopush_url_prefixes() {
        // A value that begins with a backslash is quoted so git reads it literally.
        if prefix.starts_with('\\') {
            let escaped = prefix.replace('\\', "\\\\");
            content.push_str(&format!("\tpushInsteadOf = \"{escaped}\"\n"));
        } else {
            content.push_str(&format!("\tpushInsteadOf = {prefix}\n"));
        }
    }
    write_if_changed(&global, &content).context("seat global gitconfig")?;
    write_if_changed(
        &system,
        "# Written by wicked-core (F-7R2-012): a worker seat's SYSTEM git config is empty.\n",
    )
    .context("seat system gitconfig")?;
    Ok((global, system))
}

/// The operator's global git config file(s) that exist: `$GIT_CONFIG_GLOBAL` when set, else
/// `$XDG_CONFIG_HOME/git/config` (or `~/.config/git/config`) and `~/.gitconfig` — the same two
/// files git itself reads, in git's order.
fn operator_global_git_configs() -> Vec<std::path::PathBuf> {
    if let Some(explicit) = std::env::var_os("GIT_CONFIG_GLOBAL") {
        let p = std::path::PathBuf::from(explicit);
        return if p.is_file() { vec![p] } else { Vec::new() };
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from);
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".config")))
        .map(|c| c.join("git").join("config"));
    [xdg, home.map(|h| h.join(".gitconfig"))]
        .into_iter()
        .flatten()
        .filter(|p| p.is_file())
        .collect()
}

fn write_if_changed(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    if std::fs::read_to_string(path)
        .map(|c| c == content)
        .unwrap_or(false)
    {
        return Ok(());
    }
    std::fs::write(path, content)
}

/// The `gh` CLI's configuration directory variable — where it reads `hosts.yml`, the stored
/// login `gh auth login` wrote. A seat is pointed at an ENGINE-OWNED, credential-less directory
/// ([`seat_gh_config_dir`]) so the operator's `~/.config/gh/hosts.yml` never authenticates a
/// seat's `gh` (and, through `gh auth git-credential`, its `git push` over https).
pub const GH_CONFIG_DIR_ENV: &str = "GH_CONFIG_DIR";

/// The name of the credential-less `gh` configuration directory under the worker home base.
pub const GH_UNAUTHENTICATED_DIR: &str = "gh-unauthenticated";

/// The credential-less `gh` configuration directory every seat runs under:
/// `<worker home base>/gh-unauthenticated`. Never holds a `hosts.yml` the engine wrote; `gh` may
/// create its own defaults file there, which carries no login. `Err` when the worker home base
/// cannot be resolved (no home directory, a relative override) — callers then strip the token
/// variables and leave `GH_CONFIG_DIR` untouched, disclosed.
pub fn seat_gh_config_dir() -> anyhow::Result<std::path::PathBuf> {
    Ok(worker_home_base()?.join(GH_UNAUTHENTICATED_DIR))
}

/// Strip the remote-write credentials from `cmd`, re-aim `gh` at the credential-less directory
/// and kill every push transport — the seat half of the F-7R2-012 fence, applied by
/// [`SeatConfig::apply`] for every seat decision (the inherit hatch INCLUDED: the hatch is about
/// whose CLI configuration a seat runs under, and `gh`/`git` remotes are not a seat CLI — a seat
/// never delivers, whatever it inherits).
///
/// Four things, in order: (1) every variable in [`REMOTE_CREDENTIAL_ENV`] and every one matching
/// [`REMOTE_CREDENTIAL_ENV_PREFIXES`] in the daemon's environment is REMOVED — tokens, the ssh
/// agent socket, askpass helpers, seat-supplied ssh commands, environment-injected git config;
/// (2) `GH_CONFIG_DIR` → the credential-less directory, `GIT_TERMINAL_PROMPT=0` (a push that
/// reaches an auth prompt fails instead of hanging the seat); (3) the transport kill rides the
/// spawn as `GIT_CONFIG_COUNT`/`KEY`/`VALUE` entries ([`nopush_git_config_env`]) — `-c`
/// precedence, above every file; (4) `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM` → the seat-owned
/// files ([`seat_git_config_files`]), so the operator's `~/.gitconfig` credential helpers and
/// `~/.git-credentials` are never consulted while identity settings still are. Review of #449
/// (FN-1): before this, `HOME`, `SSH_AUTH_SOCK` and the helpers rode into the seat, so
/// `git -c alias.p=push p` pushed over ssh with nothing in the way.
pub fn fence_remote_credentials(cmd: &mut Command) {
    for key in REMOTE_CREDENTIAL_ENV {
        cmd.env_remove(key);
    }
    for (name, _) in std::env::vars_os() {
        let name = name.to_string_lossy();
        if REMOTE_CREDENTIAL_ENV_PREFIXES
            .iter()
            .any(|p| name.starts_with(p))
        {
            cmd.env_remove(name.as_ref());
        }
    }
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    for (name, value) in nopush_git_config_env() {
        cmd.env(name, value);
    }
    match seat_gh_config_dir() {
        Ok(dir) => {
            cmd.env(GH_CONFIG_DIR_ENV, dir);
        }
        Err(e) => eprintln!(
            "wicked-core: seat spawn cannot resolve the credential-less gh config dir ({e}); \
             {GH_CONFIG_DIR_ENV} is left as inherited — the token variables are stripped, the \
             push transports are killed and the remote-write command fence still applies"
        ),
    }
    match seat_git_config_files() {
        Ok((global, system)) => {
            cmd.env("GIT_CONFIG_GLOBAL", global);
            cmd.env("GIT_CONFIG_SYSTEM", system);
        }
        Err(e) => eprintln!(
            "wicked-core: seat spawn cannot write the seat-owned git config files ({e}); the \
             operator's global/system git config stays readable — the push transports are still \
             killed by the environment entries and the credential helpers reset"
        ),
    }
}

/// Every CLI-SPECIFIC configuration variable a seat spawn decides. A seat gets its OWN set and
/// every other one STRIPPED — the daemon's `CODEX_HOME` must not ride into a pi bridge any more
/// than its `CLAUDE_CONFIG_DIR` rides into a codex one (PR#413). The XDG bases are deliberately
/// NOT listed: they are generic, so they are SET for opencode only and left as inherited
/// everywhere else (stripping them from a claude seat would re-aim git/gh for nothing).
pub const SEAT_CONFIG_ENV: &[&str] = &[
    CLAUDE_CONFIG_DIR_ENV,
    CODEX_HOME_ENV,
    PI_AGENT_DIR_ENV,
    COPILOT_HOME_ENV,
    OPENCODE_CONFIG_DIR_ENV,
    OPENCODE_CONFIG_CONTENT_ENV,
    OPENCODE_CONFIG_FILE_ENV,
    OPENCODE_AUTH_CONTENT_ENV,
];

/// Which agent CLI a seat runs — judged on the CLI binary's file STEM (the seat record's `binary`
/// on ACP, the template's first token when wrapped, the program a ballot execs), case-insensitive
/// on Windows only, exactly as [`binary_is_claude`] judges claude. `Other` is a CLI this engine
/// knows no configuration-home variable for (agy, a custom seat): it gets nothing set and every
/// [`SEAT_CONFIG_ENV`] variable stripped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SeatCli {
    Claude,
    Codex,
    Pi,
    Copilot,
    Opencode,
    /// Antigravity: no configuration-home variable (isolated by stripping alone), quiet flags set.
    Agy,
    Other,
}

impl SeatCli {
    /// The CLI a binary spelling names. `claude`, `/usr/local/bin/claude`, `claude.exe`,
    /// `claude.cmd` are all claude; `claude-code-wrapper` and `pi-acp` are `Other` (a bridge is
    /// not the CLI it carries — callers judge the SEAT binary, never the bridge).
    pub fn from_binary(bin: &str) -> Self {
        let Some(stem) = std::path::Path::new(bin)
            .file_stem()
            .and_then(|s| s.to_str())
        else {
            return SeatCli::Other;
        };
        let is = |name: &str| {
            if cfg!(windows) {
                stem.eq_ignore_ascii_case(name)
            } else {
                stem == name
            }
        };
        if is("claude") {
            SeatCli::Claude
        } else if is("codex") {
            SeatCli::Codex
        } else if is("pi") {
            SeatCli::Pi
        } else if is("copilot") {
            SeatCli::Copilot
        } else if is("opencode") {
            SeatCli::Opencode
        } else if is("agy") {
            SeatCli::Agy
        } else {
            SeatCli::Other
        }
    }

    /// The seat root's directory name under the worker home base (`<base>/<name>`) — the built-in
    /// seat's registry key. `None` for a CLI with no known configuration-home variable.
    pub fn root_name(self) -> Option<&'static str> {
        match self {
            SeatCli::Claude => Some("claude"),
            SeatCli::Codex => Some("codex"),
            SeatCli::Pi => Some("pi"),
            SeatCli::Copilot => Some("copilot"),
            SeatCli::Opencode => Some("opencode"),
            SeatCli::Agy | SeatCli::Other => None,
        }
    }
}

/// One seat spawn's configuration decision — the [`SeatCli`]-aware generalisation of
/// [`CarrierClaudeConfig`] (core#410). Resolved by [`seat_config_for`]; applied by
/// [`SeatConfig::apply`] AFTER `hardened()`, per this module's ordering contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeatConfig {
    /// The operator's [`INHERIT_OPERATOR_CONFIG_ENV`] hatch: the seat runs under the operator's
    /// own configuration — nothing set, nothing stripped. ONE hatch for every seat and CLI.
    Inherit,
    /// Isolated under the worker home.
    Isolated {
        cli: SeatCli,
        /// The seat's root, `<worker home base>/<root_name>` — validated like the claude dir
        /// (absolute, normally spelled, no planted link at any component). `None` for a CLI with
        /// no known configuration-home variable.
        root: Option<std::path::PathBuf>,
        /// The variables SET, in order: the CLI's own configuration-home variable(s), pointing
        /// into `root`. Empty for a rootless CLI.
        set: Vec<(&'static str, std::path::PathBuf)>,
        /// The [`SEAT_CONFIG_ENV`] variables this seat does NOT read — STRIPPED, so no foreign
        /// CLI's configuration path is ambient in the process.
        strip: Vec<&'static str>,
    },
}

impl SeatConfig {
    /// Apply this decision to `cmd` — AFTER `hardened()`: strip the foreign variables, then set
    /// this seat's own. `Inherit` touches no CLI configuration.
    ///
    /// BOTH variants apply the remote-write credential fence ([`fence_remote_credentials`],
    /// F-7R2-012): a seat is a creator or an evaluator, never the deliverer, so it runs without
    /// the daemon's `GH_TOKEN`/`GITHUB_TOKEN` and with `gh` aimed at a credential-less config
    /// directory — the inherit hatch keeps the operator's CLI configuration, not their GitHub
    /// login. The deliver tool phase never comes through here.
    pub fn apply(&self, cmd: &mut Command) {
        if let SeatConfig::Isolated {
            cli, set, strip, ..
        } = self
        {
            for key in strip {
                cmd.env_remove(key);
            }
            for (key, value) in set {
                cmd.env(key, value);
            }
            if *cli == SeatCli::Agy {
                // No config home to isolate; at least no banner in the transcript (brief §5).
                cmd.env(AGY_HIDE_LOGO_ENV, "1");
                cmd.env(AGY_HIDE_ACCOUNT_INFO_ENV, "1");
            }
        }
        fence_remote_credentials(cmd);
    }

    /// The seat root this decision runs the CLI under, when isolated with a known root.
    pub fn root(&self) -> Option<&std::path::Path> {
        match self {
            SeatConfig::Isolated {
                root: Some(root), ..
            } => Some(root.as_path()),
            _ => None,
        }
    }

    /// The claude configuration directory this decision sets — `Some` only for an isolated
    /// CLAUDE seat (the per-session settings file is written there).
    pub fn claude_dir(&self) -> Option<&std::path::Path> {
        match self {
            SeatConfig::Isolated {
                cli: SeatCli::Claude,
                root: Some(root),
                ..
            } => Some(root.as_path()),
            _ => None,
        }
    }

    /// Create every directory this seat is pointed at (the root and each `set` target), PRIVATE
    /// (0700 on unix) and no-follow checked at every component first — a CLI handed a variable
    /// naming a missing directory may refuse to start (codex) or fall back to its default home
    /// (the very leak this closes). Privacy is ENFORCED on an existing directory too, not only
    /// granted at creation (Copilot, #426): a root that pre-existed at 0755 is made 0700.
    /// Idempotent; `Inherit` and a rootless seat are no-ops. Claude's home is ALSO re-sanitized on
    /// every ACP spawn (`wicked-core::acp_runner`); that stays there — this guarantees existence
    /// and privacy.
    pub fn ensure_dirs(&self) -> anyhow::Result<()> {
        for dir in self.owned_dirs() {
            ensure_private_dir(&dir)
                .map_err(|e| anyhow::anyhow!("seat config root {}: {e}", dir.display()))?;
        }
        Ok(())
    }

    /// Every directory the CLI will actually READ under this decision: the root, each `set`
    /// target — and, for opencode, the APP directory under each XDG base (`<base>/opencode`),
    /// which is what the CLI resolves its config, `auth.json` and state from. Checking only the
    /// XDG parents left a pre-planted `<root>/data/opencode` link undetected (Copilot, #426).
    pub fn owned_dirs(&self) -> Vec<std::path::PathBuf> {
        let SeatConfig::Isolated { cli, root, set, .. } = self else {
            return Vec::new();
        };
        let mut dirs: Vec<std::path::PathBuf> = root.iter().cloned().collect();
        for (_, dir) in set {
            dirs.push(dir.clone());
            if *cli == SeatCli::Opencode {
                dirs.push(dir.join("opencode"));
            }
        }
        dirs
    }
}

/// Create `dir` private (0700 on unix) if absent — and MAKE it private if it exists — never through
/// a plantable symlink at any component ([`refuse_symlinked_home`]). Refuses a relative path: every
/// consumer would resolve it against its own working directory (the rule the worker home has).
/// Shared by the seat roots and by wicked-core's chat scratch roots, so "a private engine-owned
/// directory" is spelled once.
pub fn ensure_private_dir(dir: &std::path::Path) -> anyhow::Result<()> {
    use anyhow::Context;
    if !dir.is_absolute() {
        anyhow::bail!(
            "{} is a relative path; an engine-owned directory must be absolute",
            dir.display()
        );
    }
    refuse_symlinked_home(dir)?;
    if !dir.is_dir() {
        let mut b = std::fs::DirBuilder::new();
        b.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            b.mode(0o700);
        }
        b.create(dir)
            .with_context(|| format!("could not create {}", dir.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("could not make {} private", dir.display()))?;
    }
    Ok(())
}

/// The configuration decision for ONE seat spawn of `cli` (core#410): `Inherit` under the
/// operator's hatch; else every known CLI gets its own root under the validated worker home base
/// — claude `<base>/claude` (`CLAUDE_CONFIG_DIR`, the dir FINDING-061 introduced), codex
/// `<base>/codex` (`CODEX_HOME`), pi `<base>/pi` (`PI_CODING_AGENT_DIR`), copilot
/// `<base>/copilot` (`COPILOT_HOME`), opencode `<base>/opencode/{config,data,state}`
/// (`XDG_CONFIG_HOME` / `XDG_DATA_HOME` / `XDG_STATE_HOME`) — and every OTHER seat variable
/// stripped. An unknown CLI is isolated by stripping alone. `Err` is the resolver failing (no home
/// directory, a relative or `..` override, a planted link); callers fail CLOSED, as for claude.
pub fn seat_config_for(cli: SeatCli) -> anyhow::Result<SeatConfig> {
    if inherits_operator_config() {
        return Ok(SeatConfig::Inherit);
    }
    let Some(name) = cli.root_name() else {
        return Ok(SeatConfig::Isolated {
            cli,
            root: None,
            set: Vec::new(),
            strip: SEAT_CONFIG_ENV.to_vec(),
        });
    };
    let root = worker_home_base()?.join(name);
    refuse_symlinked_home(&root)?;
    let set: Vec<(&'static str, std::path::PathBuf)> = match cli {
        SeatCli::Claude => vec![(CLAUDE_CONFIG_DIR_ENV, root.clone())],
        SeatCli::Codex => vec![(CODEX_HOME_ENV, root.clone())],
        SeatCli::Pi => vec![(PI_AGENT_DIR_ENV, root.clone())],
        SeatCli::Copilot => vec![(COPILOT_HOME_ENV, root.clone())],
        SeatCli::Opencode => vec![
            (XDG_CONFIG_HOME_ENV, root.join("config")),
            (XDG_DATA_HOME_ENV, root.join("data")),
            (XDG_STATE_HOME_ENV, root.join("state")),
        ],
        SeatCli::Agy | SeatCli::Other => unreachable!("rootless CLIs returned above"),
    };
    // Every seat variable this seat does not SET is stripped — for opencode that includes its own
    // inline document (`OPENCODE_CONFIG_CONTENT`), extra config file and inline credentials: the
    // ambient operator values do not ride into the process by inheritance. The spawn then re-sets
    // exactly what it intends AFTER `apply` — the seat's registry governance content, or the
    // skills-composed document. Stated limit (independent review, C6): the skills COMPOSITION
    // (`wicked-core::skills_snapshot`, v3.2 §2) still takes the daemon's ambient
    // `OPENCODE_CONFIG_CONTENT` as its base when the registry names none, so a governed opencode
    // unit WITH skills delivery still receives the operator's document through that path; chats,
    // ballots and delivery-less units do not. Composing from the registry value or an empty
    // document is a later change, not this one.
    let strip: Vec<&'static str> = SEAT_CONFIG_ENV
        .iter()
        .copied()
        .filter(|var| !set.iter().any(|(own, _)| own == var))
        .collect();
    Ok(SeatConfig::Isolated {
        cli,
        root: Some(root),
        set,
        strip,
    })
}

/// [`seat_config_for`] judged on the binary a spawn is about to run — the wrapped worker's
/// template binary, the ballot's program, the ACP seat record's `binary` (never the bridge).
pub fn seat_config_for_carrier(carrier_binary: &str) -> anyhow::Result<SeatConfig> {
    seat_config_for(SeatCli::from_binary(carrier_binary))
}

/// TEST-SUPPORT — never call from runtime code. Points [`WORKER_HOME_ENV`] at one per-process
/// temp directory for the REST of the process, so a test that reaches the engine's real ACP
/// spawn path can never mutate the operator's REAL `~/.wicked-worker/claude` — the persistent
/// worker config home the spawn re-sanitizes on every start (settings.json is REWRITTEN and
/// `hooks/`, `plugins/`, `commands/`, `agents/`, `settings.local.json`, `managed-settings.json`
/// are DELETED there). Same disease as the emit-outbox leak (core#311), adjacent organ; armed by
/// the same pre-main block, because `emit::hermetic_test_spool` calls this.
///
/// Idempotent (`OnceLock`) and deliberately NEVER unset — a test that re-aims the variable at
/// its own fixture home must RESTORE it to this armed value afterwards (set-to-armed, not
/// `remove_var`): an unset window would hand a parallel or later real start the operator's home.
/// Returns the armed base dir — every caller in the same process gets the same one. Spawned
/// subprocesses inherit the variable, so arming the test process covers its children.
pub fn hermetic_test_worker_home() -> std::path::PathBuf {
    static ARMED: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    ARMED
        .get_or_init(|| {
            let path = std::env::temp_dir()
                .join(format!("wicked-worker-test-home-{}", std::process::id()));
            // SAFETY: process-global env write, serialized by `OnceLock` and never removed
            // afterwards, so there is no read-during-unset window to race. Pre-main callers
            // (the `#[ctor]` arming blocks) run single-threaded.
            unsafe { std::env::set_var(WORKER_HOME_ENV, &path) };
            path
        })
        .clone()
}

/// Chainable environment hardening for [`Command`].
///
/// Returns `&mut Command` so it composes with the builder style every spawn site already uses:
/// `Command::new(bin).hardened().args(...)`. A free function taking `&mut Command` would have forced
/// each of the ~30 call sites to be restructured, and a migration that requires rewriting the call
/// site is a migration that gets skipped.
pub trait HardenedCommand {
    /// Remove every [`ENGINE_INTERNAL_ENV`] variable from what this child would inherit.
    ///
    /// Unconditional by design — it does not check whether the parent actually has them set. Whether
    /// the daemon's environment is clean today is an accident of how the operator started it, and
    /// hardening that only engages when it is already needed is hardening you cannot test.
    fn hardened(&mut self) -> &mut Command;
}

impl HardenedCommand for Command {
    fn hardened(&mut self) -> &mut Command {
        for key in ENGINE_INTERNAL_ENV {
            self.env_remove(key);
        }
        self
    }
}

/// Marker documenting where the rule is mechanically enforced.
///
/// The enforcement lives in the root crate (`wicked-core`), not here: it must scan the whole
/// workspace, including this crate and `wicked-council`, and only the root sees all of them. See
/// `wicked_core::spawn_audit`.
pub const fn enforced_by_test() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardened_removes_every_engine_internal_variable() {
        // Not a tautology over the const: this asserts the **mechanism**, that `hardened()` actually
        // reaches Command's env map, rather than that the list contains what it contains. The
        // observable proof that the strip works end-to-end (a real child reporting UNSET) lives in
        // `execute_wrapped::tests::no_worker_inherits_an_estate_store_through_the_environment`.
        let mut cmd = Command::new("true");
        for key in ENGINE_INTERNAL_ENV {
            cmd.env(key, "leaked");
        }
        cmd.hardened();

        let surviving: Vec<_> = cmd
            .get_envs()
            .filter(|(_, v)| v.is_some())
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        assert!(
            surviving.is_empty(),
            "hardened() left engine-internal variables set: {surviving:?}"
        );
    }

    #[test]
    fn hardened_does_not_disturb_unrelated_variables() {
        // The strip is targeted, not an env_clear(). A worker still needs PATH, HOME and the
        // operator's own CLI credentials to function; clearing wholesale would trade a leak for a
        // different bug (FINDING-047's neighbourhood) and get reverted the first time a run failed.
        let mut cmd = Command::new("true");
        cmd.env("PATH", "/usr/bin");
        cmd.env("WICKED_ESTATE_DB", "/leak");
        cmd.hardened();

        let kept: Vec<_> = cmd
            .get_envs()
            .filter(|(_, v)| v.is_some())
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        assert_eq!(kept, vec!["PATH".to_string()]);
    }

    #[test]
    fn a_path_may_still_pass_a_variable_deliberately_after_hardening() {
        // The ordering contract from the module docs. `hardened()` clears; it does not forbid. If
        // this ever failed, every governed run would lose its gate-hook store and deny every tool
        // call — the exact skew filed as core#167.
        let mut cmd = Command::new("true");
        cmd.hardened();
        cmd.env("WICKED_GATE_DB", "/run/store.db");

        let found = cmd
            .get_envs()
            .find(|(k, _)| k.to_string_lossy() == "WICKED_GATE_DB")
            .and_then(|(_, v)| v)
            .map(|v| v.to_string_lossy().into_owned());
        assert_eq!(found.as_deref(), Some("/run/store.db"));
    }

    /// The worker-home resolver, over explicit values so no test mutates the process environment:
    /// the override wins outright, else `HOME`, else `USERPROFILE`, else an error — and the claude
    /// seat dir is always `<base>/claude`.
    /// An absolute fixture path spelled for the HOST platform — `is_absolute` needs a drive on
    /// Windows and a leading `/` elsewhere, so one literal cannot serve both.
    fn abs(tail: &str) -> String {
        if cfg!(windows) {
            format!("C:\\{}", tail.replace('/', "\\"))
        } else {
            format!("/{tail}")
        }
    }

    #[test]
    fn the_worker_home_base_resolves_override_then_home_then_userprofile() {
        use std::ffi::OsString;
        use std::path::PathBuf;
        let base = |o: Option<&str>, h: Option<&str>, u: Option<&str>| {
            worker_home_base_from(
                o.map(OsString::from),
                h.map(OsString::from),
                u.map(OsString::from),
            )
        };
        let (worker, home, profile) = (abs("tmp/fresh/worker"), abs("home/op"), abs("Users/op"));
        assert_eq!(
            base(Some(&worker), Some(&home), Some(&profile)).unwrap(),
            PathBuf::from(&worker),
            "WICKED_WORKER_HOME is the base itself, not a parent of it"
        );
        // Both set: the platform's NATIVE spelling wins (USERPROFILE on Windows, HOME elsewhere).
        let native = if cfg!(windows) { &profile } else { &home };
        assert_eq!(
            base(None, Some(&home), Some(&profile)).unwrap(),
            PathBuf::from(native).join(".wicked-worker")
        );
        // Only the other one set: it is the fallback on every platform.
        assert_eq!(
            base(None, None, Some(&profile)).unwrap(),
            PathBuf::from(&profile).join(".wicked-worker")
        );
        assert_eq!(
            base(None, Some(&home), None).unwrap(),
            PathBuf::from(&home).join(".wicked-worker")
        );
        // A Windows host whose HOME is a Git-Bash spelling still resolves through USERPROFILE.
        if cfg!(windows) {
            assert_eq!(
                base(None, Some("/c/Users/op"), Some(&profile)).unwrap(),
                PathBuf::from(&profile).join(".wicked-worker")
            );
        }
        let err = base(None, None, None).expect_err("no home at all must not invent one");
        assert!(err.to_string().contains("HOME"), "{err}");
    }

    /// An empty or relative worker home is a configuration error, refused by the resolver itself
    /// — so the ACP spawn, the ballot spawn and the sign-in command all refuse the SAME way instead
    /// of each resolving `relative/claude` against its own working directory (codex, PR#413).
    #[test]
    fn an_empty_or_relative_worker_home_is_refused_by_every_consumer_at_the_resolver() {
        use std::ffi::OsString;
        let base = |o: Option<&str>, h: Option<&str>| {
            worker_home_base_from(o.map(OsString::from), h.map(OsString::from), None)
        };
        let home = abs("home/op");
        for bad in ["", "relative/worker", "./worker", "worker"] {
            let err = base(Some(bad), Some(&home)).expect_err(bad);
            assert!(
                err.to_string().contains("absolute"),
                "override {bad:?} must be refused as non-absolute: {err}"
            );
            assert!(
                err.to_string().contains(WORKER_HOME_ENV),
                "names the source: {err}"
            );
        }
        // The home directory spelling is held to the same bar.
        let err = base(None, Some("relative-home")).expect_err("relative HOME");
        assert!(err.to_string().contains("absolute"), "{err}");
        assert!(err.to_string().contains("HOME"), "{err}");
        // And a good one still resolves.
        assert!(base(Some(&abs("abs/worker")), None).is_ok());
    }

    /// The carrier test is on the file stem, so the three paths that apply it agree.
    #[test]
    fn binary_is_claude_judges_the_file_stem() {
        for yes in [
            "claude",
            "/usr/local/bin/claude",
            "claude.exe",
            "claude.cmd",
        ] {
            assert!(binary_is_claude(yes), "{yes}");
        }
        // codex r3: the comparison follows the OS's executable lookup — Windows launches
        // `CLAUDE.EXE` / `Claude.cmd` as claude and so must the carrier test; a case-sensitive
        // filesystem does not, so `Claude` stays a different binary there.
        for spelled in ["CLAUDE.EXE", "Claude.cmd", r"C:\Tools\CLAUDE.exe", "Claude"] {
            assert_eq!(
                binary_is_claude(spelled),
                cfg!(windows),
                "{spelled}: case-insensitive on Windows only"
            );
        }
        for no in [
            "codex",
            "pi",
            "copilot",
            "opencode",
            "claude-code-wrapper",
            "claude-agent-acp",
        ] {
            assert!(!binary_is_claude(no), "{no}");
        }
    }

    /// codex r3: on Windows an upper/mixed-case `.exe`/`.cmd` spelling reaches the SAME carrier
    /// decision as `claude` — never `NotClaude` (which would strip the variable from a claude
    /// process and leave it on the operator's home config). Elsewhere it is not claude at all.
    #[test]
    fn a_case_variant_claude_spelling_reaches_the_claude_carrier_decision_on_windows() {
        for spelled in ["CLAUDE.EXE", "Claude.cmd", r"C:\Tools\CLAUDE.exe"] {
            let decision = claude_config_for_carrier(spelled);
            if cfg!(windows) {
                match decision {
                    Ok(CarrierClaudeConfig::NotClaude) => {
                        panic!("{spelled} launches claude on Windows and must be treated as claude")
                    }
                    Ok(CarrierClaudeConfig::Inherit) => assert!(inherits_operator_config()),
                    Ok(CarrierClaudeConfig::Dir(d)) => assert!(d.ends_with("claude"), "{spelled}"),
                    Err(e) => panic!("{spelled}: this host's worker home should resolve: {e}"),
                }
            } else {
                assert_eq!(
                    decision.unwrap(),
                    CarrierClaudeConfig::NotClaude,
                    "{spelled}: exact on a case-sensitive filesystem"
                );
            }
        }
    }

    /// A non-claude carrier gets NO claude configuration path — the decision is made before any
    /// resolver runs, so it holds even where the worker home is unresolvable.
    #[test]
    fn a_non_claude_carrier_gets_no_claude_config_decision_at_all() {
        for other in ["codex", "pi", "copilot", "opencode", "/opt/bin/codex"] {
            assert_eq!(
                claude_config_for_carrier(other).unwrap(),
                CarrierClaudeConfig::NotClaude,
                "{other}"
            );
        }
        // A claude carrier decides between the hatch and a validated dir (whichever this host's
        // environment selects — both are legitimate; neither is `NotClaude`).
        match claude_config_for_carrier("claude") {
            Ok(CarrierClaudeConfig::Inherit) => assert!(inherits_operator_config()),
            Ok(CarrierClaudeConfig::Dir(d)) => {
                assert!(d.is_absolute(), "{}", d.display());
                assert!(d.ends_with("claude"));
            }
            Ok(CarrierClaudeConfig::NotClaude) => panic!("claude is a claude carrier"),
            Err(e) => panic!("this host's worker home should resolve: {e}"),
        }
    }

    /// The no-follow check shared by both spawn paths: a link at the home, or at its parent, is
    /// refused; a real directory and a not-yet-created one pass.
    #[test]
    #[cfg(unix)]
    fn a_symlinked_worker_home_or_parent_is_refused_without_following_it() {
        let scratch = std::env::temp_dir().join(format!(
            "wicked-apps-core-symlink-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let base = scratch.join("worker");
        let operator_like = scratch.join("operator-config");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::create_dir_all(&operator_like).unwrap();
        // Not created yet: fine (the ACP spawn creates it).
        assert!(refuse_symlinked_home(&base.join("claude")).is_ok());
        // A real directory: fine.
        std::fs::create_dir_all(base.join("real")).unwrap();
        assert!(refuse_symlinked_home(&base.join("real")).is_ok());
        // `<home>/claude -> <operator-like dir>`: refused, and the target is never consulted.
        std::os::unix::fs::symlink(&operator_like, base.join("claude")).unwrap();
        let err = refuse_symlinked_home(&base.join("claude")).expect_err("link at the home");
        assert!(err.to_string().contains("symlink"), "{err}");
        // A link at the PARENT is refused too.
        std::os::unix::fs::symlink(&operator_like, scratch.join("linked-base")).unwrap();
        let err = refuse_symlinked_home(&scratch.join("linked-base").join("claude"))
            .expect_err("link at the parent");
        assert!(err.to_string().contains("symlink"), "{err}");
        // codex r2: a link at an INTERMEDIATE component — two levels above the leaf, where the old
        // leaf+parent check never looked — is refused too, and named.
        std::os::unix::fs::symlink(&operator_like, scratch.join("mid")).unwrap();
        let deep = scratch.join("mid").join("worker").join("claude");
        let err = refuse_symlinked_home(&deep).expect_err("link at an intermediate component");
        assert!(err.to_string().contains("symlink"), "{err}");
        assert!(
            err.to_string()
                .contains(&scratch.join("mid").display().to_string()),
            "names the planted component: {err}"
        );
        let _ = std::fs::remove_dir_all(&scratch);
    }

    /// codex r2, PR#413: a `.` or `..` segment is refused at the resolver even though the path
    /// reads as absolute — a `..` re-aims the resolved dir outside the declared base, and each
    /// consumer would otherwise have the kernel resolve it independently.
    #[test]
    fn a_dot_or_dotdot_segment_in_the_worker_home_is_refused() {
        use std::ffi::OsString;
        let home = abs("home/op");
        for bad in [
            abs("home/op/../other"),
            abs("home/./op/worker"),
            abs("home/op/worker/.."),
        ] {
            let err =
                worker_home_base_from(Some(OsString::from(&bad)), None, None).expect_err(&bad);
            assert!(err.to_string().contains("segment"), "{bad}: {err}");
            assert!(err.to_string().contains(WORKER_HOME_ENV), "{bad}: {err}");
        }
        // The home directory spelling is held to the same bar.
        let err = worker_home_base_from(None, Some(OsString::from(abs("home/../op"))), None)
            .expect_err("dotdot in HOME");
        assert!(err.to_string().contains("segment"), "{err}");
        // A plain absolute path — and a leading `.` that is merely part of a NAME — still resolve.
        assert!(worker_home_base_from(Some(OsString::from(&home)), None, None).is_ok());
        assert!(
            worker_home_base_from(
                Some(OsString::from(abs("home/op/.wicked-worker"))),
                None,
                None
            )
            .is_ok(),
            "`.wicked-worker` is a name, not a `.` segment"
        );
    }

    /// The seat dir the ballot sets is exactly `<base>/claude` — the SAME directory the ACP worker
    /// spawn ensures (`wicked-core::acp_runner::worker_config_home`, pinned to this resolver by
    /// its own test) and the roster's claude sign-in command names.
    #[test]
    fn the_claude_seat_dir_is_the_worker_home_base_joined_with_claude() {
        let base = worker_home_base().expect("this process has a home directory");
        assert_eq!(
            worker_claude_config_dir().unwrap(),
            base.join("claude"),
            "one resolver, one directory"
        );
    }

    // ── core#410: per-seat configuration roots ───────────────────────────────────────────────

    /// The seat CLI is judged on the file STEM, like the claude carrier test — and a BRIDGE
    /// (`pi-acp`, `codex-acp`, `claude-agent-acp`) is never mistaken for the CLI it carries.
    #[test]
    fn the_seat_cli_is_judged_on_the_file_stem_and_never_on_a_bridge() {
        use SeatCli::*;
        for (bin, cli) in [
            ("claude", Claude),
            ("/usr/local/bin/claude", Claude),
            ("claude.exe", Claude),
            ("codex", Codex),
            ("/opt/homebrew/bin/codex", Codex),
            ("codex.cmd", Codex),
            ("pi", Pi),
            ("copilot", Copilot),
            ("opencode", Opencode),
            ("agy", Agy),
            ("/Users/op/.local/bin/agy", Agy),
            ("pi-acp", Other),
            ("codex-acp", Other),
            ("claude-agent-acp", Other),
            ("claude-code-wrapper", Other),
            ("", Other),
        ] {
            assert_eq!(SeatCli::from_binary(bin), cli, "{bin:?}");
        }
        // Case follows the OS's executable lookup, exactly as `binary_is_claude` does.
        for spelled in ["CODEX.EXE", "Pi.cmd", "OpenCode"] {
            assert_eq!(
                SeatCli::from_binary(spelled) != Other,
                cfg!(windows),
                "{spelled}"
            );
        }
        // ONE stem judgement: the claude carrier test IS the resolver's claude arm.
        for bin in [
            "claude",
            "Claude",
            "codex",
            "pi",
            "claude-agent-acp",
            "/x/claude.cmd",
        ] {
            assert_eq!(
                binary_is_claude(bin),
                SeatCli::from_binary(bin) == Claude,
                "{bin}"
            );
        }
        assert_eq!(Other.root_name(), None);
        assert_eq!(
            Agy.root_name(),
            None,
            "no configuration-home variable is known for agy"
        );
        assert_eq!(Opencode.root_name(), Some("opencode"));
    }

    /// Every known seat gets its OWN root under the worker home base through its OWN
    /// configuration-home variable(s); every FOREIGN seat variable is stripped; the XDG bases are
    /// set for opencode only and never stripped from anyone.
    #[test]
    fn every_known_seat_gets_its_own_root_and_every_foreign_seat_variable_is_stripped() {
        use std::path::PathBuf;
        use SeatCli::*;
        let all = [Claude, Codex, Pi, Copilot, Opencode, Agy, Other];
        if inherits_operator_config() {
            for cli in all {
                assert_eq!(
                    seat_config_for(cli).unwrap(),
                    SeatConfig::Inherit,
                    "{cli:?}"
                );
            }
            return;
        }
        let base = worker_home_base().expect("this process has a home directory");
        let expect = |cli: SeatCli, want: Vec<(&'static str, PathBuf)>| match seat_config_for(cli)
            .unwrap()
        {
            SeatConfig::Isolated {
                cli: got_cli,
                root,
                set,
                strip,
            } => {
                assert_eq!(got_cli, cli);
                assert_eq!(
                    root,
                    Some(base.join(cli.root_name().unwrap())),
                    "{cli:?}: the root is <base>/<name>"
                );
                assert_eq!(
                    set, want,
                    "{cli:?}: its own variable(s), pointing into the root"
                );
                let own: Vec<&str> = set.iter().map(|(k, _)| *k).collect();
                for var in SEAT_CONFIG_ENV {
                    assert_eq!(
                        strip.contains(var),
                        !own.contains(var),
                        "{cli:?}: {var} is stripped iff it is not this seat's own"
                    );
                }
                for xdg in [XDG_CONFIG_HOME_ENV, XDG_DATA_HOME_ENV, XDG_STATE_HOME_ENV] {
                    assert!(
                        !strip.contains(&xdg),
                        "{cli:?}: XDG bases are never stripped"
                    );
                }
            }
            other => panic!("{cli:?}: expected an isolated decision, got {other:?}"),
        };
        expect(Claude, vec![(CLAUDE_CONFIG_DIR_ENV, base.join("claude"))]);
        expect(Codex, vec![(CODEX_HOME_ENV, base.join("codex"))]);
        expect(Pi, vec![(PI_AGENT_DIR_ENV, base.join("pi"))]);
        expect(Copilot, vec![(COPILOT_HOME_ENV, base.join("copilot"))]);
        expect(
            Opencode,
            vec![
                (XDG_CONFIG_HOME_ENV, base.join("opencode").join("config")),
                (XDG_DATA_HOME_ENV, base.join("opencode").join("data")),
                (XDG_STATE_HOME_ENV, base.join("opencode").join("state")),
            ],
        );
        // An unknown CLI — and agy, which has no configuration-home variable — is isolated by
        // stripping alone: nothing of its own to set.
        for rootless in [Other, Agy] {
            match seat_config_for(rootless).unwrap() {
                SeatConfig::Isolated {
                    cli,
                    root: None,
                    set,
                    strip,
                } => {
                    assert_eq!(cli, rootless);
                    assert!(set.is_empty());
                    assert_eq!(strip, SEAT_CONFIG_ENV.to_vec());
                }
                other => {
                    panic!("{rootless:?}: expected a rootless isolated decision, got {other:?}")
                }
            }
        }
        // The claude-only view agrees with the generalisation.
        assert_eq!(
            claude_config_for_carrier("claude").unwrap(),
            CarrierClaudeConfig::Dir(base.join("claude"))
        );
        for foreign in ["codex", "pi", "copilot", "opencode", "agy"] {
            assert_eq!(
                claude_config_for_carrier(foreign).unwrap(),
                CarrierClaudeConfig::NotClaude,
                "{foreign}"
            );
        }
    }

    /// `apply` strips the foreign seat variables and sets the seat's own — after `hardened()`,
    /// over an explicit decision (no environment read), so the mechanism is what is asserted:
    /// a daemon carrying a decoy for EVERY seat variable hands a pi seat exactly one of them.
    #[test]
    fn apply_strips_every_foreign_seat_variable_and_sets_the_seats_own() {
        use std::path::PathBuf;
        let root = PathBuf::from(abs("worker/pi"));
        let decision = SeatConfig::Isolated {
            cli: SeatCli::Pi,
            root: Some(root.clone()),
            set: vec![(PI_AGENT_DIR_ENV, root.clone())],
            strip: SEAT_CONFIG_ENV
                .iter()
                .copied()
                .filter(|v| *v != PI_AGENT_DIR_ENV)
                .collect(),
        };
        let mut cmd = Command::new("true");
        cmd.hardened();
        for var in SEAT_CONFIG_ENV {
            cmd.env(var, "decoy");
        }
        cmd.env(XDG_CONFIG_HOME_ENV, "operator-xdg");
        decision.apply(&mut cmd);
        let value = |key: &str| -> Option<Option<String>> {
            cmd.get_envs()
                .find(|(k, _)| k.to_string_lossy() == key)
                .map(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
        };
        assert_eq!(
            value(PI_AGENT_DIR_ENV),
            Some(Some(root.to_string_lossy().into_owned())),
            "the seat's own variable points into its root"
        );
        for var in SEAT_CONFIG_ENV.iter().filter(|v| **v != PI_AGENT_DIR_ENV) {
            assert_eq!(
                value(var),
                Some(None),
                "{var} is removed (not merely left as decoy)"
            );
        }
        assert_eq!(
            value(XDG_CONFIG_HOME_ENV),
            Some(Some("operator-xdg".to_string())),
            "a generic XDG base is left alone on a non-opencode seat"
        );
        // agy: nothing of its own to set, every seat variable stripped — and the two quiet flags
        // (no logo, no account banner in the transcript) set; a pi seat gets neither flag.
        let mut agy = Command::new("true");
        agy.hardened();
        SeatConfig::Isolated {
            cli: SeatCli::Agy,
            root: None,
            set: Vec::new(),
            strip: SEAT_CONFIG_ENV.to_vec(),
        }
        .apply(&mut agy);
        let flag = |c: &Command, key: &str| -> Option<Option<String>> {
            c.get_envs()
                .find(|(k, _)| k.to_string_lossy() == key)
                .map(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
        };
        assert_eq!(flag(&agy, AGY_HIDE_LOGO_ENV), Some(Some("1".to_string())));
        assert_eq!(
            flag(&agy, AGY_HIDE_ACCOUNT_INFO_ENV),
            Some(Some("1".to_string()))
        );
        assert_eq!(
            flag(&cmd, AGY_HIDE_LOGO_ENV),
            None,
            "a pi seat gets no agy flag"
        );
        // `Inherit` touches nothing at all (hardened first, like every spawn site — the strip
        // there is the engine's own variables, never a seat's).
        let mut untouched = Command::new("true");
        untouched.hardened();
        untouched.env(CODEX_HOME_ENV, "decoy");
        SeatConfig::Inherit.apply(&mut untouched);
        assert_eq!(
            untouched
                .get_envs()
                .find(|(k, _)| k.to_string_lossy() == CODEX_HOME_ENV)
                .and_then(|(_, v)| v)
                .map(|v| v.to_string_lossy().into_owned()),
            Some("decoy".to_string())
        );
    }

    /// `ensure_dirs` creates every target private (0700) and refuses a planted link — the same
    /// no-follow discipline as the claude home, now for every seat root.
    #[test]
    #[cfg(unix)]
    fn ensure_dirs_creates_every_seat_target_private_and_refuses_a_planted_link() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = std::env::temp_dir().join(format!(
            "wicked-apps-core-seat-dirs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let root = scratch.join("worker").join("opencode");
        let decision = SeatConfig::Isolated {
            cli: SeatCli::Opencode,
            root: Some(root.clone()),
            set: vec![
                (XDG_CONFIG_HOME_ENV, root.join("config")),
                (XDG_DATA_HOME_ENV, root.join("data")),
                (XDG_STATE_HOME_ENV, root.join("state")),
            ],
            strip: SEAT_CONFIG_ENV.to_vec(),
        };
        // A pre-existing, too-open root is made private, not left as found (Copilot, #426).
        std::fs::create_dir_all(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        decision.ensure_dirs().expect("creates the tree");
        decision.ensure_dirs().expect("idempotent");
        for dir in [
            &root,
            &root.join("config"),
            &root.join("data"),
            &root.join("state"),
        ] {
            let meta = std::fs::metadata(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
            assert!(meta.is_dir());
            assert_eq!(
                meta.permissions().mode() & 0o777,
                0o700,
                "{} is private",
                dir.display()
            );
        }
        // opencode's APP directories under the XDG bases are ensured and checked too.
        for app in ["config", "data", "state"] {
            let d = root.join(app).join("opencode");
            assert!(d.is_dir(), "{} is created", d.display());
            assert_eq!(
                std::fs::metadata(&d).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        // A planted link at the APP directory (`<root>/data/opencode -> operator's store`) is
        // refused even though its XDG parent is a real directory (Copilot, #426).
        let other_root = scratch.join("worker2").join("opencode");
        std::fs::create_dir_all(other_root.join("data")).unwrap();
        let operator_store = scratch.join("operator-opencode-data");
        std::fs::create_dir_all(&operator_store).unwrap();
        std::os::unix::fs::symlink(&operator_store, other_root.join("data").join("opencode"))
            .unwrap();
        let planted_app = SeatConfig::Isolated {
            cli: SeatCli::Opencode,
            root: Some(other_root.clone()),
            set: vec![
                (XDG_CONFIG_HOME_ENV, other_root.join("config")),
                (XDG_DATA_HOME_ENV, other_root.join("data")),
                (XDG_STATE_HOME_ENV, other_root.join("state")),
            ],
            strip: Vec::new(),
        };
        let err = planted_app
            .ensure_dirs()
            .expect_err("a planted app-dir link is refused");
        assert!(err.to_string().contains("symlink"), "{err}");
        // A planted link where a seat root should be: refused, never followed.
        let operator_like = scratch.join("operator-codex");
        std::fs::create_dir_all(&operator_like).unwrap();
        let linked = scratch.join("worker").join("codex");
        std::os::unix::fs::symlink(&operator_like, &linked).unwrap();
        let planted = SeatConfig::Isolated {
            cli: SeatCli::Codex,
            root: Some(linked.clone()),
            set: vec![(CODEX_HOME_ENV, linked)],
            strip: Vec::new(),
        };
        let err = planted
            .ensure_dirs()
            .expect_err("a planted link is refused");
        assert!(err.to_string().contains("symlink"), "{err}");
        assert_eq!(SeatConfig::Inherit.ensure_dirs().ok(), Some(()));
        // A relative directory is refused outright — never resolved against a working directory.
        let err = ensure_private_dir(std::path::Path::new("relative/dir")).expect_err("relative");
        assert!(err.to_string().contains("relative"), "{err}");
        let _ = std::fs::remove_dir_all(&scratch);
    }

    /// F-7R2-012 (wave 6): every seat decision — isolated or the inherit hatch — strips the
    /// remote-write credentials and aims `gh` at the credential-less directory, while the deliver
    /// tool phase (`hardened()` alone, no seat config) keeps the daemon's login.
    #[test]
    fn apply_strips_remote_credentials_and_aims_gh_at_a_credential_less_dir() {
        use std::path::PathBuf;
        let value = |cmd: &Command, key: &str| -> Option<Option<String>> {
            cmd.get_envs()
                .find(|(k, _)| k.to_string_lossy() == key)
                .map(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
        };
        let root = PathBuf::from(abs("worker/codex"));
        for decision in [
            SeatConfig::Inherit,
            SeatConfig::Isolated {
                cli: SeatCli::Codex,
                root: Some(root.clone()),
                set: vec![(CODEX_HOME_ENV, root.clone())],
                strip: vec![],
            },
        ] {
            let mut cmd = Command::new("true");
            cmd.hardened();
            for var in REMOTE_CREDENTIAL_ENV {
                cmd.env(var, "secret");
            }
            decision.apply(&mut cmd);
            for var in REMOTE_CREDENTIAL_ENV {
                // Every planted value is gone — REMOVED, or (for `GIT_CONFIG_COUNT`, which the
                // transport kill re-sets to its own count) replaced by the engine's own value.
                assert_ne!(
                    value(&cmd, var),
                    Some(Some("secret".to_string())),
                    "{decision:?}: {var} must not survive as planted"
                );
                if *var != "GIT_CONFIG_COUNT" {
                    assert_eq!(
                        value(&cmd, var),
                        Some(None),
                        "{decision:?}: {var} is REMOVED, not left as a decoy"
                    );
                }
            }
            match value(&cmd, GH_CONFIG_DIR_ENV) {
                Some(Some(dir)) => assert!(
                    dir.ends_with(GH_UNAUTHENTICATED_DIR),
                    "{decision:?}: gh is aimed at the credential-less dir, got {dir}"
                ),
                other => assert!(
                    worker_home_base().is_err(),
                    "{decision:?}: GH_CONFIG_DIR may stay unset only when the worker home base \
                     is unresolvable, got {other:?}"
                ),
            }
        }
        // The deliver tool phase applies no seat config: `hardened()` keeps the daemon's login,
        // and the credential fence is a SEAT rule — never folded into `hardened()`.
        let mut deliver = Command::new("true");
        deliver.env("GH_TOKEN", "daemon-login");
        deliver.hardened();
        assert_eq!(
            value(&deliver, "GH_TOKEN"),
            Some(Some("daemon-login".to_string())),
            "hardened() alone must not strip GH_TOKEN — the deliver phase pushes with it"
        );
        assert!(
            !ENGINE_INTERNAL_ENV
                .iter()
                .any(|v| REMOTE_CREDENTIAL_ENV.contains(v)),
            "the credential fence is applied per seat, not by hardened()"
        );
    }

    /// Review of #449, FN-1(b): the seat spawn strips the ssh/git credential paths too, kills
    /// every push transport with `-c` precedence, and re-points git's global/system config at
    /// seat-owned files — while `hardened()` alone (the deliver tool phase) keeps all of it.
    #[test]
    fn fence_strips_ssh_and_git_credential_paths_and_kills_every_push_transport() {
        let value = |cmd: &Command, key: &str| -> Option<Option<String>> {
            cmd.get_envs()
                .find(|(k, _)| k.to_string_lossy() == key)
                .map(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
        };
        let mut cmd = Command::new("true");
        cmd.hardened();
        for var in [
            "SSH_AUTH_SOCK",
            "GIT_SSH_COMMAND",
            "GIT_ASKPASS",
            "GIT_CONFIG_PARAMETERS",
            "GIT_EXEC_PATH",
        ] {
            cmd.env(var, "planted");
        }
        // Environment-injected config the daemon might carry (the review's bypass shape) — at
        // an index the engine's own entries never reach.
        std::env::set_var("GIT_CONFIG_KEY_97", "alias.p");
        std::env::set_var("GIT_CONFIG_VALUE_97", "push");
        fence_remote_credentials(&mut cmd);
        std::env::remove_var("GIT_CONFIG_KEY_97");
        std::env::remove_var("GIT_CONFIG_VALUE_97");
        for var in [
            "SSH_AUTH_SOCK",
            "GIT_SSH_COMMAND",
            "GIT_ASKPASS",
            "GIT_CONFIG_PARAMETERS",
            "GIT_EXEC_PATH",
            "GIT_CONFIG_KEY_97",
            "GIT_CONFIG_VALUE_97",
        ] {
            assert_eq!(value(&cmd, var), Some(None), "{var} is REMOVED");
        }
        assert_eq!(value(&cmd, "GIT_TERMINAL_PROMPT"), Some(Some("0".into())));
        let count: usize = value(&cmd, "GIT_CONFIG_COUNT")
            .flatten()
            .expect("the transport kill rides GIT_CONFIG_COUNT")
            .parse()
            .unwrap();
        let mut push_kills = 0;
        let mut helper_reset = false;
        for i in 0..count {
            let key = value(&cmd, &format!("GIT_CONFIG_KEY_{i}"))
                .flatten()
                .unwrap();
            let val = value(&cmd, &format!("GIT_CONFIG_VALUE_{i}"))
                .flatten()
                .unwrap();
            if key == format!("url.{NOPUSH_SCHEME}.pushInsteadOf") {
                assert!(nopush_url_prefixes().contains(&val), "{val}");
                push_kills += 1;
            }
            if key == "credential.helper" {
                assert!(val.is_empty(), "an empty helper RESETS the list");
                helper_reset = true;
            }
        }
        assert_eq!(
            push_kills,
            nopush_url_prefixes().len(),
            "every transport is killed — drive letters and UNC included"
        );
        assert!(
            nopush_url_prefixes().iter().any(|p| p == "C:")
                && nopush_url_prefixes().iter().any(|p| p == "c:"),
            "Windows drive-letter path remotes are killed too"
        );
        assert!(helper_reset);
        if worker_home_base().is_ok() {
            let global = value(&cmd, "GIT_CONFIG_GLOBAL")
                .flatten()
                .expect("global git config re-pointed");
            let system = value(&cmd, "GIT_CONFIG_SYSTEM")
                .flatten()
                .expect("system git config re-pointed");
            assert!(
                global.contains(GH_UNAUTHENTICATED_DIR) && system.contains(GH_UNAUTHENTICATED_DIR)
            );
            let text = std::fs::read_to_string(&global).unwrap();
            assert!(
                text.contains(&format!("[url \"{NOPUSH_SCHEME}\"]")),
                "{text}"
            );
            assert!(
                text.contains("pushInsteadOf = ssh://") && text.contains("pushInsteadOf = git@")
            );
            assert!(text.contains("helper =") && text.contains("askPass ="));
        }
        // The deliver tool phase: hardened() alone keeps every one of these.
        let mut deliver = Command::new("true");
        deliver.env("SSH_AUTH_SOCK", "/tmp/agent.sock");
        deliver.env("GIT_SSH_COMMAND", "ssh -i key");
        deliver.hardened();
        assert_eq!(
            value(&deliver, "SSH_AUTH_SOCK"),
            Some(Some("/tmp/agent.sock".into()))
        );
        assert_eq!(
            value(&deliver, "GIT_SSH_COMMAND"),
            Some(Some("ssh -i key".into()))
        );
        assert_eq!(
            value(&deliver, "GIT_CONFIG_COUNT"),
            None,
            "no transport kill on the deliver phase"
        );
    }

    /// Review of #449, FN-1: the transport kill HOLDS — a seat-fenced `git push` fails before any
    /// transport is contacted on an ssh remote (URL and scp-like), a `file://` remote and an
    /// absolute-path remote, aliased (`git -c alias.p=push p`) or not; the same command WITHOUT
    /// the fence pushes to the path remote (the fixture would otherwise have pushed), and
    /// `git fetch` from it still works under the fence.
    #[test]
    fn a_fenced_seat_cannot_push_over_any_transport_while_the_deliver_phase_can() {
        let base = std::env::temp_dir().join(format!(
            "wicked-nopush-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let bare = base.join("bare.git");
        let clone = base.join("clone");
        std::fs::create_dir_all(&bare).unwrap();
        std::fs::create_dir_all(&clone).unwrap();
        // spawn-audit: test-only fixture spawns; the fence under test is applied to the LAST one.
        let git = |cwd: &std::path::Path, args: &[&str], fenced: bool| -> (bool, String) {
            let mut c = Command::new("git");
            c.hardened();
            c.args(args).current_dir(cwd);
            c.env("GIT_CONFIG_NOSYSTEM", "1");
            if fenced {
                fence_remote_credentials(&mut c);
            }
            let out = match c.output() {
                Ok(o) => o,
                Err(e) => return (false, format!("spawn: {e}")),
            };
            (
                out.status.success(),
                format!(
                    "{}{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                ),
            )
        };
        if !git(&bare, &["init", "-q", "--bare"], false).0 {
            eprintln!("no usable git on this host — skipping the transport-kill fixture");
            let _ = std::fs::remove_dir_all(&base);
            return;
        }
        assert!(git(&clone, &["init", "-q"], false).0);
        for (k, v) in [
            ("user.email", "t@example.invalid"),
            ("user.name", "t"),
            ("commit.gpgsign", "false"),
            ("core.autocrlf", "false"),
        ] {
            assert!(git(&clone, &["config", k, v], false).0);
        }
        std::fs::write(clone.join("a.txt"), "a\n").unwrap();
        assert!(git(&clone, &["add", "."], false).0);
        assert!(git(&clone, &["commit", "-qm", "init"], false).0);
        let bare_url = bare.to_string_lossy().replace('\\', "/");
        let remotes = [
            ("sshurl", "ssh://example.invalid/o/r.git".to_string()),
            ("scp", "git@example.invalid:o/r.git".to_string()),
            ("fileurl", format!("file://{bare_url}")),
            ("path", bare_url.clone()),
        ];
        for (name, url) in &remotes {
            assert!(
                git(&clone, &["remote", "add", name, url], false).0,
                "{name}"
            );
        }
        // Under the fence: every push fails on the kill, before any transport is contacted.
        for (name, _) in &remotes {
            let (ok, out) = git(&clone, &["push", name, "HEAD:refs/heads/main"], true);
            assert!(!ok, "a fenced push to remote {name} must fail: {out}");
            assert!(
                out.contains("wicked-nopush"),
                "the failure is the transport kill, not a network error (remote {name}): {out}"
            );
        }
        // The alias spelling the review reproduced: still killed by layer 3.
        let (ok, out) = git(
            &clone,
            &["-c", "alias.p=push", "p", "path", "HEAD:refs/heads/main"],
            true,
        );
        assert!(!ok && out.contains("wicked-nopush"), "{out}");
        // Fetch keeps working under the fence (pushInsteadOf never touches fetch).
        let (ok, out) = git(&clone, &["fetch", "path"], true);
        assert!(ok, "a fenced fetch from the path remote works: {out}");
        // Control — the same push WITHOUT the fence lands (the deliver phase's shape).
        let (ok, out) = git(&clone, &["push", "path", "HEAD:refs/heads/main"], false);
        assert!(
            ok,
            "the unfenced push to the bare path remote succeeds: {out}"
        );
        let (ok, refs) = git(&bare, &["show-ref", "refs/heads/main"], false);
        assert!(ok && refs.contains("refs/heads/main"), "{refs}");
        let _ = std::fs::remove_dir_all(&base);
    }
}
