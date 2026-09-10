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
    // Judged on the literal spelling: `Path::components()` silently normalizes `.` away, and a
    // `..` that survives it would be resolved by the kernel at every consumer independently.
    let spelled = path.as_os_str().to_string_lossy();
    if spelled
        .split(['/', '\\'])
        .any(|segment| segment == "." || segment == "..")
    {
        anyhow::bail!(
            "{source}={} contains a `.` or `..` segment; the worker home must be spelled as a plain \
             absolute path (a `..` re-aims the resolved directory outside the declared base)",
            path.display()
        );
    }
    Ok(path)
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
    std::path::Path::new(bin)
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|stem| {
            if cfg!(windows) {
                stem.eq_ignore_ascii_case("claude")
            } else {
                stem == "claude"
            }
        })
        .unwrap_or(false)
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
    if !binary_is_claude(carrier_binary) {
        return Ok(CarrierClaudeConfig::NotClaude);
    }
    match seat_claude_config_dir() {
        None => Ok(CarrierClaudeConfig::Inherit),
        Some(dir) => dir.map(CarrierClaudeConfig::Dir),
    }
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
}
