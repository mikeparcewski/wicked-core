//! The OS floor for agent workers: the repository boundary (core#548) and, for every GOVERNED
//! unit, the step-0 seat floor (DES-PROC-GOVERNANCE §12).
//!
//! A unit in a run worktree may write its own tree and nothing else of the repository it was
//! cut from. Until this module, that rule was policy only — the gate hook and the ACP permission
//! bridge read the TEXT of each command and guessed whether it wrote (#540/#541/#542), and three
//! review cycles showed that a shell string cannot be judged soundly: a Creator's
//! `python3 -c "open('<sibling worktree>/x','w')"` was neither blocked nor detected, and the
//! F-E2E-029(b) install evasions (`env -C`, `npm_config_prefix`, a piped `sh -c`, an in-command
//! symlink) reached the clone root the same way.
//!
//! **The seat floor ([`seat_floor`], every governed unit, every seat — codex included).** Armed on
//! the worker PROCESS TREE at spawn on both carriers (the wrapped CLI spawn and the ACP bridge
//! spawn), inherited by every child, and irreversible from inside:
//!
//! - **Writes are denied by default.** Allowed back: the unit's tree (not under a fenced posture —
//!   the read-only profile), the repository metadata a commit on the run branch writes (also not
//!   under a fenced posture), the write roots the unit was handed, the seat's own home, its cache
//!   and temp dirs, and the run's governance-log dir. The operator's home is therefore
//!   write-denied except for those engine-minted dirs.
//! - **Reads of secret paths are denied** ([`floor_mask_dirs_under`]): the validator's credential
//!   list plus the other agent CLIs' own homes, minus the seat's own inherited home.
//! - **Network (macOS only)**: outbound TCP to port 443 on any host (model APIs), the system DNS
//!   socket, and the pinned loopback / proxy ports the seat needs ([`floor_net_grants`]). SBPL
//!   cannot filter by host name (`remote` takes `localhost` or `*` only), so "only model hosts" is
//!   not expressible here; a loopback gateway is the follow-up. Linux (`bwrap`) leaves the
//!   network open: pinning it needs a network namespace plus an egress proxy.
//!
//! **Each CLI's own sandbox goes off under ours.** macOS refuses a nested sandbox apply under ANY
//! outer profile (`sandbox_apply: Operation not permitted`, exit 71 — even under
//! `(allow default)`; only re-applying the identical profile is a no-op), so a CLI that arms its
//! own Seatbelt inside ours fails every command. codex therefore runs `--sandbox
//! danger-full-access` under the floor ([`codex_sandbox_off_under_floor`]); claude's own sandbox is
//! never enabled; opencode runs its server on a pinned loopback port inside the same floor.
//!
//! **Fail closed.** On macOS and Linux a governed unit whose floor cannot arm is refused
//! ([`governed_floor`]), never run unconfined. Windows has no floor yet: the unit keeps today's
//! behaviour (the text fences and the worktree guard) and the record labels it `no OS floor`.
//!
//! **The repository boundary ([`default_worker_sandbox`], ungoverned spawns).** `(allow default)` +
//! a write deny on the clone root the worktree belongs to (its main working tree, its `.git`,
//! every sibling run worktree under `wicked-worktrees/`) and the worktree's parent, with the
//! unit's own tree, gitdir and the shared `objects`/`refs`/`logs` allowed back. Not a read jail
//! and not exfiltration protection. Arm-or-skip: a host that cannot arm runs the worker as before.
//!
//! macOS arms both with `sandbox-exec` (SBPL: the later matching rule wins); Linux with `bwrap`
//! (later mounts win). A write outside fails at the OS with a typed error: `EPERM` ("Operation not
//! permitted") on macOS, `EROFS` ("Read-only file system") on Linux. The launcher is probed once
//! per process with the boundary's own shape.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use wicked_apps_core::HardenedCommand;

/// The repository boundary for one worker spawn, before it is rendered for a launcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoBoundary {
    /// Directories made read-only (canonical).
    pub(crate) protected: Vec<PathBuf>,
    /// Directories inside `protected` given back as writable (canonical, existing).
    pub(crate) admitted: Vec<PathBuf>,
    /// Single files inside `protected` given back as writable. `FETCH_HEAD` (a `git fetch` in the
    /// worktree writes it in place) is admitted on both launchers — bwrap binds it when it exists.
    /// `packed-refs` and its lock are admitted on macOS only (`literal` can name a file that does
    /// not exist yet); git replaces `packed-refs` by RENAME, which a bind-mounted file refuses, so
    /// on Linux a packed-ref rewrite in the clone stays denied (a warning on a ref update, never
    /// a failed commit on the run branch).
    pub(crate) admitted_files: Vec<PathBuf>,
}

/// The one admitted file bwrap may bind: written in place, never replaced by rename.
const IN_PLACE_FILE: &str = "FETCH_HEAD";

/// The linked worktree `dir` belongs to — `dir` itself or its nearest ancestor holding a `.git`
/// entry — with that worktree's gitdir read from its `.git` FILE (`gitdir: <path>`). A unit may
/// run in a package directory below its worktree root, so the walk matters. `None` when the
/// nearest `.git` is a directory (a main checkout, not a run worktree), when there is none, or
/// when it is unreadable.
fn linked_worktree(dir: &Path) -> Option<(PathBuf, PathBuf)> {
    let root = dir.ancestors().find(|a| a.join(".git").exists())?;
    let dot_git = root.join(".git");
    if !dot_git.is_file() {
        return None;
    }
    let worktree = root;
    let text = std::fs::read_to_string(&dot_git).ok()?;
    let raw = text.lines().find_map(|l| l.strip_prefix("gitdir:"))?.trim();
    let path = Path::new(raw);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        worktree.join(path)
    };
    Some((worktree.to_path_buf(), path.canonicalize().ok()?))
}

/// The repository boundary for a worker whose cwd is `worktree`, admitting `write_roots` too
/// when they lie inside the clone. `None` when `worktree` is not a linked git worktree (a
/// repo-less scratch dir, a clone root itself) — there is no sibling to protect.
pub(crate) fn repo_boundary(cwd: &Path, write_roots: &[PathBuf]) -> Option<RepoBoundary> {
    let (tree, gitdir) = linked_worktree(&cwd.canonicalize().ok()?)?;
    let common = std::fs::read_to_string(gitdir.join("commondir"))
        .ok()
        .map(|c| {
            let c = c.trim();
            let p = Path::new(c);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                gitdir.join(p)
            }
        })
        .unwrap_or_else(|| gitdir.clone())
        .canonicalize()
        .ok()?;
    // A non-bare clone's common dir is `<clone>/.git`: protect the whole working tree. A bare
    // common dir protects itself.
    let clone_root = if common.file_name().is_some_and(|n| n == ".git") {
        common
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or(common.clone())
    } else {
        common.clone()
    };
    let mut protected = vec![clone_root];
    if let Some(parent) = tree.parent() {
        if !protected.iter().any(|p| parent.starts_with(p)) {
            protected.push(parent.to_path_buf());
        }
    }
    // A commit on the run branch writes the shared object store, the branch ref and its reflog.
    // `logs` may not exist yet in a fresh clone; creating the empty directory lets the bind name
    // it (bwrap binds existing paths only) and changes nothing else.
    let _ = std::fs::create_dir_all(common.join("logs"));
    let mut admitted = vec![tree.clone(), gitdir];
    for sub in ["objects", "refs", "logs"] {
        if let Ok(p) = common.join(sub).canonicalize() {
            admitted.push(p);
        }
    }
    for root in write_roots {
        // A grant root whose LEAF is a symbolic link is never admitted into the clone (codex r3 on
        // core#878): the engine mints its roots as plain directories (the notes root, the QE
        // ledger root, vouched at dispatch), so a link in one's place was planted after the
        // fact — and following it would grant the clone or a sibling worktree.
        if root
            .symlink_metadata()
            .is_ok_and(|m| m.file_type().is_symlink())
        {
            continue;
        }
        if let Ok(p) = root.canonicalize() {
            if protected.iter().any(|g| p.starts_with(g)) && !admitted.contains(&p) {
                admitted.push(p);
            }
        }
    }
    let admitted_files = vec![
        common.join(IN_PLACE_FILE),
        common.join("packed-refs"),
        common.join("packed-refs.lock"),
    ];
    Some(RepoBoundary {
        protected,
        admitted,
        admitted_files,
    })
}

fn sbpl_quote(p: &Path) -> String {
    let mut out = String::from("\"");
    for c in p.to_string_lossy().chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// The macOS `sandbox-exec` profile for `b`: everything allowed, writes to the protected roots
/// denied, the admitted roots allowed back (SBPL: the later matching rule wins).
pub(crate) fn sbpl_profile(b: &RepoBoundary) -> String {
    let mut p = String::from("(version 1)\n(allow default)\n");
    for dir in &b.protected {
        p.push_str(&format!(
            "(deny file-write* (subpath {}))\n",
            sbpl_quote(dir)
        ));
    }
    for dir in &b.admitted {
        p.push_str(&format!(
            "(allow file-write* (subpath {}))\n",
            sbpl_quote(dir)
        ));
    }
    for file in &b.admitted_files {
        p.push_str(&format!(
            "(allow file-write* (literal {}))\n",
            sbpl_quote(file)
        ));
    }
    p
}

/// The Linux `bwrap` argv prefix for `b` (ending in `--`): the host root bound read-write, a
/// private `/dev`, each protected root re-bound read-only, then each admitted root bound back
/// read-write (later mounts win). No network or PID namespace: this is write containment only.
pub(crate) fn bwrap_argv(tool: &Path, b: &RepoBoundary) -> Vec<String> {
    let mut w: Vec<String> = vec![
        tool.to_string_lossy().into_owned(),
        "--bind".into(),
        "/".into(),
        "/".into(),
        "--dev".into(),
        "/dev".into(),
    ];
    for dir in &b.protected {
        let s = dir.to_string_lossy().into_owned();
        w.extend(["--ro-bind".to_string(), s.clone(), s]);
    }
    for dir in &b.admitted {
        let s = dir.to_string_lossy().into_owned();
        w.extend(["--bind".to_string(), s.clone(), s]);
    }
    for file in b
        .admitted_files
        .iter()
        .filter(|f| f.file_name().is_some_and(|n| n == IN_PLACE_FILE) && f.is_file())
    {
        let s = file.to_string_lossy().into_owned();
        w.extend(["--bind".to_string(), s.clone(), s]);
    }
    w.push("--".into());
    w
}

/// Whether `tool` can arm on this host: one run of the boundary's own shape against a throwaway
/// path (a daemon already inside a sandbox, or a kernel refusing unprivileged user namespaces,
/// fails here and the worker runs as before).
fn launcher_arms(tool: &Path) -> bool {
    let probe = std::env::temp_dir().join(format!("wicked-wsb-probe-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&probe);
    let Ok(dir) = probe.canonicalize() else {
        return false;
    };
    let b = RepoBoundary {
        protected: vec![dir.clone()],
        admitted: vec![dir.clone()],
        admitted_files: Vec::new(),
    };
    let mut argv = launcher_argv(tool, &b);
    argv.extend([
        "/bin/sh".to_string(),
        "-c".to_string(),
        "exit 0".to_string(),
    ]);
    let ok = std::process::Command::new(&argv[0])
        .hardened()
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    let _ = std::fs::remove_dir_all(&dir);
    ok
}

fn launcher_argv(tool: &Path, b: &RepoBoundary) -> Vec<String> {
    if tool.file_name().is_some_and(|n| n == "sandbox-exec") {
        vec![
            tool.to_string_lossy().into_owned(),
            "-p".into(),
            sbpl_profile(b),
        ]
    } else {
        bwrap_argv(tool, b)
    }
}

/// (IG1-core-1) Why the default repository boundary did not arm for a spawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FloorUnarmed {
    /// The spawn's cwd is not a linked run worktree (a chat's scratch root, a clone root, a
    /// repo-less directory): there is no clone or sibling to protect.
    NotAWorktree,
    /// No launcher (`sandbox-exec`, `bwrap`) is on this host's `PATH` (all of Windows).
    NoLauncher,
    /// A launcher is present but its probe failed here (a kernel refusing unprivileged user
    /// namespaces, a daemon already running inside a sandbox).
    CannotArm,
    /// The unit's working directory could not be resolved, so no floor can be rendered for it.
    RootUnresolvable,
    /// Container floor: no container runtime CLI (`docker`, `podman`, `nerdctl`) on this host.
    NoContainerRuntime,
    /// Container floor: a runtime CLI is present but its engine did not answer.
    ContainerRuntimeDown,
    /// Container floor: the seat's image is not defined for this CLI or could not be built.
    NoSeatImage,
    /// Container floor: native Windows (seat containers mount host paths at the same path).
    ContainerUnsupportedHost,
    /// Container floor: the unit's gate hook is a host command the seat container cannot run, and
    /// no hook route from the seat to the engine exists.
    GateHookUnreachable,
    /// Container floor: the daemon's model gateway (the seat's only egress) could not be started
    /// or attached.
    GatewayUnavailable,
}

impl FloorUnarmed {
    /// The reason in words, for a `governanceUnenforced` / `sandboxUnenforced` disclosure.
    pub(crate) fn describe(self) -> &'static str {
        match self {
            FloorUnarmed::NotAWorktree => {
                "not_a_worktree: the unit does not run in a linked run worktree, so there is no \
                 repository boundary to arm"
            }
            FloorUnarmed::NoLauncher => {
                "no_launcher: neither sandbox-exec nor bwrap is on this host's PATH"
            }
            FloorUnarmed::CannotArm => {
                "cannot_arm: the host's sandbox launcher is present but its probe failed here"
            }
            FloorUnarmed::RootUnresolvable => {
                "root_unresolvable: the unit's working directory could not be resolved"
            }
            FloorUnarmed::NoContainerRuntime => {
                "no_container_runtime: a container runtime is required for governed runs and none \
                 was found — install Docker, Rancher Desktop, Podman, Colima or OrbStack (or set \
                 WICKED_CONTAINER_RUNTIME to its CLI)"
            }
            FloorUnarmed::ContainerRuntimeDown => {
                "container_runtime_down: a container runtime CLI is installed but its engine did not \
                 answer (`<runtime> version` failed) — start it (Docker Desktop, Rancher Desktop, \
                 `podman machine start`, `colima start`) and retry"
            }
            FloorUnarmed::NoSeatImage => {
                "no_seat_image: the seat's container image is not defined for this CLI or could not \
                 be built (`wicked-core seat-image build <cli>` shows why)"
            }
            FloorUnarmed::ContainerUnsupportedHost => {
                "container_unsupported_host: the container floor mounts host paths at the same path, \
                 which native Windows paths cannot be — run the daemon inside WSL2 with a container \
                 runtime"
            }
            FloorUnarmed::GateHookUnreachable => {
                "gate_hook_unreachable: the unit's PreToolUse gate hook is a host command the seat \
                 container cannot run, and no hook route from the seat to the engine is configured \
                 — refusing rather than running its tool calls unchecked"
            }
            FloorUnarmed::GatewayUnavailable => {
                "gateway_unavailable: the daemon's model gateway (agentgateway, the seat's only \
                 egress) could not be started or attached to the seat's network"
            }
        }
    }

    /// The reason's wire spelling, for a disclosure (and `hostBoundary().reason`, core#678).
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            FloorUnarmed::NotAWorktree => "not_a_worktree",
            FloorUnarmed::NoLauncher => "no_launcher",
            FloorUnarmed::CannotArm => "cannot_arm",
            FloorUnarmed::RootUnresolvable => "root_unresolvable",
            FloorUnarmed::NoContainerRuntime => "no_container_runtime",
            FloorUnarmed::ContainerRuntimeDown => "container_runtime_down",
            FloorUnarmed::NoSeatImage => "no_seat_image",
            FloorUnarmed::ContainerUnsupportedHost => "container_unsupported_host",
            FloorUnarmed::GateHookUnreachable => "gate_hook_unreachable",
            FloorUnarmed::GatewayUnavailable => "gateway_unavailable",
        }
    }
}

/// (IG1-core-1) The repository boundary as armed for one spawn: the wrapper, and the launcher that
/// armed it (`"sandbox-exec"` | `"bwrap"`) — the `_wicked_gov_boundary` an `os_sandbox` marker names.
#[derive(Debug)]
pub(crate) struct ArmedFloor {
    pub(crate) sandbox: crate::validator::WorkerSandbox,
    pub(crate) tool: &'static str,
    /// The seat floor this wrapper renders — `Some` for [`seat_floor`], `None` for the ungoverned
    /// repository boundary. Recorded for shadow mode ([`crate::floor_shadow`]).
    pub(crate) spec: Option<SeatFloor>,
}

/// (IG1-core-2) The launcher an ARMED sandbox wrapper argv starts with: `"sandbox-exec"` or
/// `"bwrap"` — the `_wicked_gov_boundary` for a strict-profile (`os_sandbox = true`) spawn
/// (firejail never arms: it is network-only and always downgrades).
pub(crate) fn launcher_name_of(wrapper: &[String]) -> &'static str {
    match wrapper
        .first()
        .and_then(|w| Path::new(w).file_name())
        .and_then(|n| n.to_str())
    {
        Some("sandbox-exec") => "sandbox-exec",
        _ => "bwrap",
    }
}

/// The launcher this host arms the repository boundary with (`sandbox-exec`, then `bwrap`),
/// resolved AND probed once per process — the cached value is the tool that armed, so a later
/// call can never reuse one launcher's probe for another. `Err` names why none armed: no launcher
/// on `PATH`, or every one present failed its probe.
fn boundary_launcher() -> Result<&'static PathBuf, FloorUnarmed> {
    static TOOL: OnceLock<Result<PathBuf, FloorUnarmed>> = OnceLock::new();
    TOOL.get_or_init(|| {
        let found: Vec<PathBuf> = ["sandbox-exec", "bwrap"]
            .iter()
            .filter_map(|t| crate::validator::find_on_path(t))
            .collect();
        if found.is_empty() {
            return Err(FloorUnarmed::NoLauncher);
        }
        found
            .into_iter()
            .find(|tool| launcher_arms(tool))
            .ok_or(FloorUnarmed::CannotArm)
    })
    .as_ref()
    .map_err(|e| *e)
}

/// (IG1-core-3) The launcher this host arms the repository boundary with, by name, or why none
/// arms (`no_launcher` | `cannot_arm`) — the probe [`default_worker_sandbox`] uses, cached.
pub(crate) fn boundary_tool() -> Result<&'static str, FloorUnarmed> {
    boundary_launcher().map(|t| launcher_name(t))
}

/// (core#678 item 1) What this HOST can contain, known before any run: the launcher that arms the
/// repository boundary (or why none does), the operator's unsandboxed opt-in, and so what a VERIFY
/// floor will do here — `contained` (its checks run inside the boundary), `uncontained` (no
/// boundary, but the operator opted in: the checks run, disclosed) or `refused` (no boundary and no
/// opt-in: every check-running floor denies, which used to be learned only at verify). The probe is
/// the floor's own ([`crate::repo_checks::floor_probe`], on top of [`boundary_tool`]), cached:
/// taken once at boot ([`crate::Core`] spawn) and read for free after.
pub(crate) fn host_boundary() -> serde_json::Value {
    let opted_in = matches!(
        std::env::var(crate::repo_checks::UNSANDBOXED_OPT_IN_ENV)
            .unwrap_or_default()
            .trim(),
        "1" | "true"
    );
    // The repo-checks floor's OWN cached execution probe (codex r1, r2): the floor arms only what
    // it says arms (`repo_checks::arm_probed`), so the prediction and the floor cannot disagree.
    host_boundary_from(
        crate::repo_checks::floor_probe(),
        opted_in,
        std::env::consts::OS,
    )
}

/// [`host_boundary`] over explicit inputs — the testable seam.
pub(crate) fn host_boundary_from(
    probe: Result<&'static str, FloorUnarmed>,
    opted_in: bool,
    platform: &str,
) -> serde_json::Value {
    let (tool, reason) = match probe {
        Ok(t) => (Some(t), None),
        Err(r) => (None, Some(r)),
    };
    let floor = match (tool, opted_in) {
        (Some(_), _) => "contained",
        (None, true) => "uncontained",
        (None, false) => "refused",
    };
    serde_json::json!({
        "platform": platform,
        "armed": tool.is_some(),
        "tool": tool,
        "reason": reason.map(FloorUnarmed::as_str),
        "reasonText": reason.map(FloorUnarmed::describe),
        "unsandboxedOptIn": opted_in,
        "verifyFloor": floor,
    })
}

/// (IG1-core-3) The write containment a GOVERNED unit is PREDICTED to run under at distribution —
/// the `sandboxPosture` decision. Since step 0 every governed unit gets the seat floor, whatever
/// its seat, its governance class or whether the run is bound to a worktree, so the prediction is
/// the host's: `Ok(launcher)` (`"sandbox-exec"` | `"bwrap"`) when this host's launcher arms, else
/// why not (Windows: `no_launcher`). The arm site's marker is the fact; this predicts it.
pub(crate) fn predicted_posture(
    _seat: &wicked_council::AgenticCli,
    _workdir: Option<&Path>,
) -> Result<&'static str, PostureGap> {
    if cfg!(windows) {
        return Err(PostureGap::Unarmed(FloorUnarmed::NoLauncher));
    }
    boundary_tool().map_err(PostureGap::Unarmed)
}

/// (IG1-core-3) Why [`predicted_posture`] is `advisory`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PostureGap {
    /// The floor will not arm: no launcher, or a launcher that cannot arm here.
    Unarmed(FloorUnarmed),
}

/// The short name of a launcher path: `"sandbox-exec"` or `"bwrap"`.
fn launcher_name(tool: &Path) -> &'static str {
    if tool.file_name().is_some_and(|n| n == "sandbox-exec") {
        "sandbox-exec"
    } else {
        "bwrap"
    }
}

/// The default worker wrapper for an UNGOVERNED spawn in `cwd`: the repository boundary rendered
/// for this host's launcher with the launcher's name, or why it did not arm (not a run worktree,
/// no launcher, a launcher that cannot arm here) — the worker then spawns as before. A governed
/// unit takes [`governed_floor`] instead.
pub(crate) fn default_worker_sandbox(
    cwd: &Path,
    write_roots: &[PathBuf],
) -> Result<ArmedFloor, FloorUnarmed> {
    let boundary = repo_boundary(cwd, write_roots).ok_or(FloorUnarmed::NotAWorktree)?;
    let tool = boundary_launcher()?;
    Ok(ArmedFloor {
        sandbox: crate::validator::WorkerSandbox {
            wrapper: launcher_argv(tool, &boundary),
            level: crate::validator::SandboxLevel::Sandboxed,
            downgrade_reason: None,
        },
        tool: launcher_name(tool),
        spec: None,
    })
}

// ---------------------------------------------------------------------------------------------
// The step-0 seat floor: every GOVERNED unit, every seat (DES-PROC-GOVERNANCE §12)
// ---------------------------------------------------------------------------------------------

/// The record label of a governed unit that runs with no OS floor (Windows, step 0).
pub(crate) const NO_OS_FLOOR: &str = "no OS floor";

/// The record label of the macOS (Seatbelt) seat floor: the INTERIM fallback until the container
/// floor (a required container runtime for governed runs) replaces it. Its profile logic is
/// isolated in [`sbpl_seat_profile`] so it can be removed cleanly.
pub(crate) const MACOS_INTERIM_FLOOR: &str = "macOS interim floor";

/// The macOS socket every DNS lookup goes through: without it a host name never resolves.
const MDNS_SOCKET: &str = "/private/var/run/mDNSResponder";

/// The port model APIs are reached on. SBPL cannot name a host, so this is "any host on 443".
const MODEL_API_PORT: u16 = 443;

/// Operator override: extra outbound grants, comma separated, each `localhost:<port>` (a pinned
/// loopback endpoint such as a model gateway) or `*:<port>` (any host on that port).
pub(crate) const FLOOR_NET_ENV: &str = "WICKED_WORKER_NET";

/// One outbound network grant on the seat floor (macOS; Linux leaves the network open).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub(crate) enum NetGrant {
    /// `localhost:<port>`: a pinned loopback endpoint (the crew MCP broker, a model gateway, the
    /// seat's own opencode server). Every other local daemon stays unreachable.
    Loopback { port: u16 },
    /// `*:<port>`: any host on this TCP port (443 for the model APIs; a configured proxy's port).
    AnyHost { port: u16 },
}

/// What one governed seat spawn asks of the floor. The caller decides the roots; the floor
/// resolves, renders and records them.
#[derive(Debug, Clone, Default)]
pub(crate) struct FloorInputs {
    /// The unit's working directory (its worktree, a package dir below it, or a run sandbox).
    pub(crate) cwd: PathBuf,
    /// The write roots the unit was handed. Under the full posture: the armed write set (cwd
    /// first, the launch extras, the graph key dir). Under a fenced posture: only the posture's
    /// admitted roots (the notes root, the QE ledger root, a creator's deliverable roots).
    pub(crate) write_roots: Vec<PathBuf>,
    /// The READ-ONLY profile (a fenced posture): no write grant on the tree, and none on the
    /// repository metadata either (gitdir, `objects`, `refs`, `logs`, `packed-refs`) — otherwise a
    /// "read-only" seat could `git update-ref` another branch.
    pub(crate) read_only: bool,
    /// The seat's own directories: its configuration home(s), its cache and temp dirs, a check
    /// scratch, and the run's governance-log dir (the gate hook writes its decisions there).
    pub(crate) seat_dirs: Vec<PathBuf>,
    /// Paths no secret mask may cover: the seat's own configuration when it inherits the
    /// operator's (the `WICKED_INHERIT_OPERATOR_CONFIG` hatch).
    pub(crate) unmask: Vec<PathBuf>,
    /// Files inside the writable dirs that stay read-only ([`SeatFloor::pinned`]).
    pub(crate) pinned: Vec<PathBuf>,
    /// Outbound grants ([`floor_net_grants`]).
    pub(crate) net: Vec<NetGrant>,
    /// The operator's home the secret masks resolve under (the process `HOME`).
    pub(crate) home: Option<PathBuf>,
}

/// The seat floor as resolved for one spawn: what the profile renders and what shadow mode
/// judges each tool call against ([`crate::floor_shadow`]). Every path is canonical.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SeatFloor {
    /// `"governed"` or `"read-only"`.
    pub(crate) profile: String,
    /// The unit's tree (its linked worktree root, else its cwd).
    pub(crate) tree: PathBuf,
    /// Allowed back after the blanket write deny.
    pub(crate) writable: Vec<PathBuf>,
    /// Then denied again: the clone and the worktrees' parent (the repository boundary).
    pub(crate) protected: Vec<PathBuf>,
    /// Then allowed back inside `protected`: the unit's own tree and git state (full posture) or
    /// only the handed write roots that lie there (read-only).
    pub(crate) readmitted: Vec<PathBuf>,
    /// Single files allowed back inside `protected` (`FETCH_HEAD`, `packed-refs`; full posture).
    pub(crate) readmitted_files: Vec<PathBuf>,
    /// Read AND write denied, last: secret paths.
    pub(crate) masks: Vec<PathBuf>,
    /// Single files kept READ-ONLY inside a writable dir: the engine's own launch files in the
    /// governance-log dir (the claude hook settings, the floor spec), which a worker that rewrote
    /// them would use to switch its own gate off.
    #[serde(default)]
    pub(crate) pinned: Vec<PathBuf>,
    /// Outbound grants (macOS).
    pub(crate) net: Vec<NetGrant>,
}

impl SeatFloor {
    /// Whether the floor lets a process write `path` (canonical or lexically normal), judged
    /// exactly as the SBPL renders it: deny-all, `writable`, then `protected`, then `readmitted`,
    /// then the masks — the later matching rule wins.
    pub(crate) fn allows_write(&self, path: &Path) -> bool {
        let under = |roots: &[PathBuf]| roots.iter().any(|r| path.starts_with(r));
        let mut ok = under(&self.writable);
        if under(&self.protected) {
            ok = false;
        }
        if under(&self.readmitted) || self.readmitted_files.iter().any(|f| path == f) {
            ok = true;
        }
        ok && !under(&self.masks) && !self.pinned.iter().any(|f| path == f)
    }

    /// Whether the floor lets a process read `path`: everything but the masks.
    pub(crate) fn allows_read(&self, path: &Path) -> bool {
        !self.masks.iter().any(|m| path.starts_with(m))
    }
}

/// `p` made canonical even when its leaf does not exist yet: the nearest existing ancestor is
/// canonicalized and the rest re-appended (SBPL matches resolved paths: `/var` is `/private/var`
/// on macOS). `None` when nothing of it resolves.
pub(crate) fn canon_lenient(p: &Path) -> Option<PathBuf> {
    if let Ok(c) = p.canonicalize() {
        return Some(c);
    }
    let mut rest: Vec<&std::ffi::OsStr> = Vec::new();
    let mut cur = p;
    loop {
        let name = cur.file_name()?;
        rest.push(name);
        cur = cur.parent()?;
        if let Ok(c) = cur.canonicalize() {
            let mut out = c;
            for n in rest.iter().rev() {
                out.push(n);
            }
            return Some(out);
        }
    }
}

/// The secret paths the floor masks (read and write denied) under `home`: the validator jail's
/// credential list ([`crate::validator::secret_read_block_dirs_under`]) plus the other agent CLIs'
/// own homes and the plain-text credential files. A seat's own home is never under `home` unless
/// it inherits the operator's, and then the caller unmasks it.
pub(crate) fn floor_mask_dirs_under(home: Option<&Path>) -> Vec<PathBuf> {
    let Some(home) = home else {
        return Vec::new();
    };
    let mut out = crate::validator::secret_read_block_dirs_under(Some(home));
    const MORE: &[&[&str]] = &[
        &[".claude.json"],
        &[".codex"],
        &[".pi"],
        &[".copilot"],
        &[".gemini"],
        &[".config", "opencode"],
        &[".local", "share", "opencode"],
        &[".netrc"],
        &[".git-credentials"],
        &[".docker"],
        &[".kube"],
    ];
    for segs in MORE {
        let mut p = home.to_path_buf();
        for s in *segs {
            p.push(s);
        }
        out.push(p);
    }
    out
}

/// The `(host, port)` of a URL-ish value (`http://127.0.0.1:7701/x`, `user:pw@proxy:8080`,
/// `[::1]:9`), with the scheme's default port when none is spelled. `None` for anything else.
fn url_host_port(v: &str) -> Option<(String, u16)> {
    let v = v.trim();
    if v.is_empty() {
        return None;
    }
    let (scheme, rest) = match v.split_once("://") {
        Some((s, r)) => (s.to_ascii_lowercase(), r),
        None => (String::new(), v),
    };
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit_once('@').map_or(authority, |(_, a)| a);
    let (host, port) = if let Some(stripped) = authority.strip_prefix('[') {
        let (h, tail) = stripped.split_once(']')?;
        (h.to_string(), tail.strip_prefix(':').map(str::to_string))
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), Some(p.to_string())),
            None => (authority.to_string(), None),
        }
    };
    if host.is_empty() {
        return None;
    }
    let port = match port {
        Some(p) => p.parse().ok()?,
        None => match scheme.as_str() {
            "https" => 443,
            "http" => 80,
            "socks5" | "socks5h" | "socks" => 1080,
            _ => return None,
        },
    };
    Some((host, port))
}

fn is_loopback_host(h: &str) -> bool {
    matches!(h.to_ascii_lowercase().as_str(), "localhost" | "::1")
        || h.parse::<std::net::Ipv4Addr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// The environment variables whose URL the seat must be able to reach: the crew MCP broker, the
/// model base URLs a seat honours, and an HTTP(S) proxy.
const FLOOR_URL_ENVS: &[&str] = &[
    crate::mcp_gate::CREW_URL_ENV,
    "ANTHROPIC_BASE_URL",
    "OPENAI_BASE_URL",
    "OPENAI_API_BASE",
    "OLLAMA_HOST",
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "ALL_PROXY",
    "all_proxy",
];

/// The outbound grants for a seat whose environment reads through `env`: TCP 443 to any host (the
/// model APIs), each URL in [`FLOOR_URL_ENVS`] (a loopback one pinned to `localhost:<port>`, a
/// remote one as `*:<port>`), and the operator's [`FLOOR_NET_ENV`] entries. Deduplicated, ordered.
pub(crate) fn floor_net_grants(env: impl Fn(&str) -> Option<String>) -> Vec<NetGrant> {
    let mut out = vec![NetGrant::AnyHost {
        port: MODEL_API_PORT,
    }];
    let mut push = |g: NetGrant| {
        if !out.contains(&g) {
            out.push(g);
        }
    };
    for key in FLOOR_URL_ENVS {
        if let Some((host, port)) = env(key)
            .as_deref()
            .and_then(url_host_port)
            .filter(|(_, p)| *p != 0)
        {
            push(if is_loopback_host(&host) {
                NetGrant::Loopback { port }
            } else {
                NetGrant::AnyHost { port }
            });
        }
    }
    for entry in env(FLOOR_NET_ENV).unwrap_or_default().split(',') {
        let entry = entry.trim();
        if let Some((host, port)) = entry.rsplit_once(':') {
            let Ok(port) = port.parse::<u16>() else {
                continue;
            };
            if port == 0 {
                continue;
            }
            match host {
                "localhost" | "127.0.0.1" => push(NetGrant::Loopback { port }),
                "*" => push(NetGrant::AnyHost { port }),
                _ => {}
            }
        }
    }
    out
}

/// Resolve `inp` into the floor one spawn runs under. `Err(RootUnresolvable)` when the unit's
/// working directory does not resolve; every other root that does not resolve is left out (it
/// cannot be written through a path that does not exist).
pub(crate) fn resolve_seat_floor(inp: &FloorInputs) -> Result<SeatFloor, FloorUnarmed> {
    let cwd = inp
        .cwd
        .canonicalize()
        .map_err(|_| FloorUnarmed::RootUnresolvable)?;
    // A write root whose LEAF is a symbolic link is never admitted (codex r3 on core#878): the
    // engine mints its roots as plain directories, so a link in one's place was planted.
    let roots: Vec<PathBuf> = inp
        .write_roots
        .iter()
        .filter(|r| {
            !r.symlink_metadata()
                .is_ok_and(|m| m.file_type().is_symlink())
        })
        .filter_map(|r| canon_lenient(r))
        .collect();
    let mut seat_dirs: Vec<PathBuf> = Vec::new();
    for d in &inp.seat_dirs {
        let _ = std::fs::create_dir_all(d);
        if let Some(c) = canon_lenient(d) {
            if !seat_dirs.contains(&c) {
                seat_dirs.push(c);
            }
        }
    }
    let repo = repo_boundary(&cwd, &roots);
    let tree = repo
        .as_ref()
        .and_then(|b| b.admitted.first().cloned())
        .unwrap_or_else(|| cwd.clone());
    let in_protected = |p: &Path| {
        repo.as_ref()
            .is_some_and(|b| b.protected.iter().any(|g| p.starts_with(g)))
    };
    let mut writable = seat_dirs.clone();
    let mut readmitted: Vec<PathBuf> = Vec::new();
    let mut readmitted_files: Vec<PathBuf> = Vec::new();
    let mut protected: Vec<PathBuf> = Vec::new();
    match (&repo, inp.read_only) {
        (Some(b), false) => {
            protected = b.protected.clone();
            readmitted = b.admitted.clone();
            readmitted_files = b.admitted_files.clone();
            for r in &roots {
                if !in_protected(r) && !writable.contains(r) {
                    writable.push(r.clone());
                }
            }
        }
        (Some(b), true) => {
            protected = b.protected.clone();
            // The tree's own git state (gitdir, objects, refs, logs), without any handed root.
            let git_state: Vec<PathBuf> = repo_boundary(&cwd, &[])
                .map(|base| base.admitted.into_iter().skip(1).collect())
                .unwrap_or_default();
            for r in &roots {
                // Never the tree or its git state, whatever a root list says.
                if r.starts_with(&tree)
                    || tree.starts_with(r)
                    || git_state
                        .iter()
                        .any(|g| r.starts_with(g) || g.starts_with(r))
                {
                    continue;
                }
                let list = if in_protected(r) {
                    &mut readmitted
                } else {
                    &mut writable
                };
                if !list.contains(r) {
                    list.push(r.clone());
                }
            }
        }
        (None, read_only) => {
            if !read_only {
                writable.push(tree.clone());
            }
            for r in &roots {
                if read_only && (r.starts_with(&tree) || tree.starts_with(r)) {
                    continue;
                }
                if !writable.contains(r) {
                    writable.push(r.clone());
                }
            }
        }
    }
    let home = inp.home.clone();
    let unmask: Vec<PathBuf> = inp
        .unmask
        .iter()
        .map(|u| canon_lenient(u).unwrap_or_else(|| u.clone()))
        .collect();
    let mut masks: Vec<PathBuf> = Vec::new();
    for m in floor_mask_dirs_under(home.as_deref()) {
        let m = canon_lenient(&m).unwrap_or(m);
        // A mask never covers what the seat must use: its own inherited home, or a dir it writes.
        let clashes = unmask
            .iter()
            .chain(writable.iter())
            .chain(std::iter::once(&tree))
            .any(|u| u.starts_with(&m) || m.starts_with(u));
        if !clashes && !masks.contains(&m) {
            masks.push(m);
        }
    }
    Ok(SeatFloor {
        profile: if inp.read_only {
            "read-only".into()
        } else {
            "governed".into()
        },
        tree,
        writable,
        protected,
        readmitted,
        readmitted_files,
        masks,
        pinned: inp.pinned.iter().filter_map(|p| canon_lenient(p)).collect(),
        net: inp.net.clone(),
    })
}

/// The macOS `sandbox-exec` profile for `f` (SBPL: the later matching rule wins).
pub(crate) fn sbpl_seat_profile(f: &SeatFloor) -> String {
    let mut p = String::from("(version 1)\n(allow default)\n(deny file-write*)\n");
    // The device files every shell and CLI writes to.
    p.push_str(
        "(allow file-write* (literal \"/dev/null\") (literal \"/dev/zero\") \
         (literal \"/dev/dtracehelper\") (literal \"/dev/tty\") (literal \"/dev/ptmx\") \
         (literal \"/dev/stdout\") (literal \"/dev/stderr\") (regex #\"^/dev/ttys[0-9]+$\") \
         (regex #\"^/dev/fd/\"))\n",
    );
    let rule = |p: &mut String, verb: &str, kind: &str, paths: &[PathBuf]| {
        for d in paths {
            p.push_str(&format!(
                "({verb} file-write* ({kind} {}))\n",
                sbpl_quote(d)
            ));
        }
    };
    rule(&mut p, "allow", "subpath", &f.writable);
    rule(&mut p, "deny", "subpath", &f.protected);
    rule(&mut p, "allow", "subpath", &f.readmitted);
    rule(&mut p, "allow", "literal", &f.readmitted_files);
    for m in &f.masks {
        let q = sbpl_quote(m);
        p.push_str(&format!(
            "(deny file-read* (subpath {q}))\n(deny file-write* (subpath {q}))\n"
        ));
    }
    rule(&mut p, "deny", "literal", &f.pinned);
    p.push_str("(deny network-outbound)\n");
    p.push_str(&format!(
        "(allow network-outbound (remote unix-socket (path-literal \"{MDNS_SOCKET}\")))\n"
    ));
    // Unix sockets the seat's own processes create (an inspector, a language server, opencode).
    for d in &f.writable {
        p.push_str(&format!(
            "(allow network-outbound (remote unix-socket (subpath {})))\n",
            sbpl_quote(d)
        ));
    }
    for g in &f.net {
        match g {
            NetGrant::Loopback { port } => p.push_str(&format!(
                "(allow network-outbound (remote ip \"localhost:{port}\"))\n"
            )),
            NetGrant::AnyHost { port } => p.push_str(&format!(
                "(allow network-outbound (remote tcp \"*:{port}\"))\n"
            )),
        }
    }
    p
}

/// The Linux `bwrap` argv prefix for `f` (ending in `--`): the host root bound READ-ONLY, a private
/// `/dev`, each writable root bound back, the repository boundary on top, and each existing mask
/// covered (an empty tmpfs over a directory, `/dev/null` over a file). No network namespace:
/// pinning egress needs one plus an egress proxy, a later step.
pub(crate) fn bwrap_seat_argv(tool: &Path, f: &SeatFloor) -> Vec<String> {
    let mut w: Vec<String> = vec![
        tool.to_string_lossy().into_owned(),
        "--ro-bind".into(),
        "/".into(),
        "/".into(),
        "--dev".into(),
        "/dev".into(),
    ];
    let bind = |w: &mut Vec<String>, flag: &str, p: &Path| {
        let s = p.to_string_lossy().into_owned();
        w.extend([flag.to_string(), s.clone(), s]);
    };
    for d in f.writable.iter().filter(|d| d.is_dir()) {
        bind(&mut w, "--bind", d);
    }
    for d in f.protected.iter().filter(|d| d.is_dir()) {
        bind(&mut w, "--ro-bind", d);
    }
    for d in f.readmitted.iter().filter(|d| d.is_dir()) {
        bind(&mut w, "--bind", d);
    }
    // Only an in-place file can be bound (see `IN_PLACE_FILE`).
    for file in f
        .readmitted_files
        .iter()
        .filter(|p| p.file_name().is_some_and(|n| n == IN_PLACE_FILE) && p.is_file())
    {
        bind(&mut w, "--bind", file);
    }
    for p in f.pinned.iter().filter(|p| p.is_file()) {
        bind(&mut w, "--ro-bind", p);
    }
    for m in &f.masks {
        let s = m.to_string_lossy().into_owned();
        if m.is_dir() {
            w.extend(["--tmpfs".to_string(), s]);
        } else if m.is_file() {
            w.extend(["--ro-bind".to_string(), "/dev/null".to_string(), s]);
        }
    }
    w.push("--".into());
    w
}

/// Arm the seat floor for `inp` on this host's launcher. `Err` names why it cannot arm.
pub(crate) fn seat_floor(inp: &FloorInputs) -> Result<ArmedFloor, FloorUnarmed> {
    let spec = resolve_seat_floor(inp)?;
    let tool = boundary_launcher()?;
    let wrapper = if launcher_name(tool) == "sandbox-exec" {
        vec![
            tool.to_string_lossy().into_owned(),
            "-p".into(),
            sbpl_seat_profile(&spec),
        ]
    } else {
        bwrap_seat_argv(tool, &spec)
    };
    Ok(ArmedFloor {
        sandbox: crate::validator::WorkerSandbox {
            wrapper,
            level: crate::validator::SandboxLevel::Sandboxed,
            downgrade_reason: None,
        },
        tool: launcher_name(tool),
        spec: Some(spec),
    })
}

/// What a GOVERNED unit runs under.
#[derive(Debug)]
pub(crate) enum GovernedFloor {
    /// The floor armed: wrap the worker in it.
    Armed(Box<ArmedFloor>),
    /// Windows, step 0: there is no floor yet. The unit keeps today's behaviour (the text fences,
    /// the worktree guard) and the record says [`NO_OS_FLOOR`] — refusing would block every
    /// Windows run.
    NoOsFloor,
    /// macOS / Linux: the floor could not arm, so the unit is REFUSED (fail closed), never run
    /// unconfined. The reason is the refusal's text; the failover ladder moves the work.
    Refused(FloorUnarmed),
}

/// The floor for a governed unit on this host ([`governed_floor_on`] with this platform).
pub(crate) fn governed_floor(inp: &FloorInputs) -> GovernedFloor {
    governed_floor_on(cfg!(windows), || seat_floor(inp))
}

/// [`governed_floor`] over an explicit platform: the testable seam for the Windows label and the
/// fail-closed refusal.
pub(crate) fn governed_floor_on(
    windows: bool,
    arm: impl FnOnce() -> Result<ArmedFloor, FloorUnarmed>,
) -> GovernedFloor {
    if windows {
        return GovernedFloor::NoOsFloor;
    }
    match arm() {
        Ok(a) => GovernedFloor::Armed(Box::new(a)),
        Err(why) => GovernedFloor::Refused(why),
    }
}

/// The text of a governed unit refused because its floor did not arm. Says `could not run`, which
/// the runner reads as a seat failure, so the actor fails the unit over (as core#925 does).
pub(crate) fn floor_refusal_text(cli: &str, why: FloorUnarmed) -> String {
    format!(
        "(seat '{cli}' could not run this governed unit: its OS floor did not arm — {}; the unit \
         is refused rather than run unconfined (fail closed) — fix the host's sandbox launcher, \
         or let the run fail over to an eligible seat)",
        why.describe()
    )
}

/// The program stem of `argv[0]` (`/opt/bin/codex` → `codex`).
fn program_stem(argv: &[String]) -> Option<&str> {
    argv.first()
        .and_then(|b| Path::new(b).file_stem())
        .and_then(|s| s.to_str())
}

/// codex's own sandbox OFF, because ours is on: every `--sandbox <mode>` / `-s <mode>` /
/// `--sandbox=<mode>` becomes `danger-full-access`, `--full-auto` (which implies
/// `workspace-write`) and any `-c sandbox_mode=…` are dropped, and the mode is added when the
/// argv names none (codex's default is read-only, which also nests). A nested Seatbelt apply is
/// refused under any outer profile, so codex's own sandbox would fail every command with exit 71.
/// Only for a launch the floor wraps; `false` (nothing changed) when `argv` is not codex.
pub(crate) fn codex_sandbox_off_under_floor(argv: &mut Vec<String>) -> bool {
    if program_stem(argv) != Some("codex") {
        return false;
    }
    const OFF: &str = "danger-full-access";
    let mut out: Vec<String> = Vec::with_capacity(argv.len() + 2);
    let mut named = false;
    let mut i = 0;
    while i < argv.len() {
        let a = argv[i].as_str();
        if a == "--" {
            out.extend(argv[i..].iter().cloned());
            break;
        }
        if (a == "--sandbox" || a == "-s") && i + 1 < argv.len() {
            out.push(a.to_string());
            out.push(OFF.to_string());
            named = true;
            i += 2;
            continue;
        }
        if a.starts_with("--sandbox=") {
            out.push(format!("--sandbox={OFF}"));
            named = true;
            i += 1;
            continue;
        }
        if a == "--full-auto" {
            i += 1;
            continue;
        }
        if (a == "-c" || a == "--config")
            && argv
                .get(i + 1)
                .is_some_and(|v| v.trim_start().starts_with("sandbox_mode"))
        {
            i += 2;
            continue;
        }
        out.push(a.to_string());
        i += 1;
    }
    if !named {
        let at = if out.get(1).is_some_and(|s| s == "exec") {
            2
        } else {
            1
        };
        out.insert(at.min(out.len()), OFF.to_string());
        out.insert(at.min(out.len()), "--sandbox".to_string());
    }
    *argv = out;
    true
}

/// The opencode server-pinning launch under the floor: `opencode run …` with no `--server`
/// becomes a small `/bin/sh` program that starts `opencode serve` on `port` INSIDE the same
/// floor, waits for it, and runs `opencode run --server http://127.0.0.1:<port> …` against it.
/// Without this, `opencode run` connects to the shared background service (which runs outside the
/// floor) or, with `--standalone`, to a private server on a random port the floor's port pin
/// refuses. `None` when `argv` is not such an opencode launch. The caller admits
/// `localhost:<port>` and sets the password env ([`OPENCODE_PASSWORD_ENVS`]) to a per-spawn nonce.
pub(crate) fn opencode_pinned_server_argv(argv: &[String], port: u16) -> Option<Vec<String>> {
    if program_stem(argv) != Some("opencode") || argv.get(1).map(String::as_str) != Some("run") {
        return None;
    }
    if argv
        .iter()
        .any(|a| a == "--server" || a.starts_with("--server="))
    {
        return None;
    }
    const SCRIPT: &str = r#"oc="$1"; port="$2"; shift 2
"$oc" serve --hostname 127.0.0.1 --port "$port" >"${TMPDIR:-.}/opencode-serve-$port.log" 2>&1 &
srv=$!
trap 'kill "$srv" 2>/dev/null' EXIT INT TERM
i=0
while [ "$i" -lt 150 ]; do
  if curl -s -o /dev/null "http://127.0.0.1:$port/" 2>/dev/null; then break; fi
  i=$((i + 1)); sleep 0.1
done
"$oc" run --server "http://127.0.0.1:$port" "$@"
"#;
    let mut out = vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        SCRIPT.to_string(),
        "wicked-opencode-floor".to_string(),
        argv[0].clone(),
        port.to_string(),
    ];
    out.extend(
        argv[2..]
            .iter()
            .filter(|a| a.as_str() != "--standalone")
            .cloned(),
    );
    Some(out)
}

/// The variables opencode's server and client read their shared password from.
pub(crate) const OPENCODE_PASSWORD_ENVS: [&str; 2] =
    ["OPENCODE_SERVER_PASSWORD", "OPENCODE_PASSWORD"];

/// A free loopback port for a seat's own server (bound and released; the seat binds it next).
pub(crate) fn free_loopback_port() -> Option<u16> {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map(|a| a.port())
}

/// The conventional configuration dirs a CLI reads under the operator's own home — what a seat
/// running under the inherit hatch must keep writable (and unmasked).
pub(crate) fn inherited_cli_dirs(
    cli: wicked_apps_core::spawn::SeatCli,
    home: Option<&Path>,
) -> Vec<PathBuf> {
    use wicked_apps_core::spawn::SeatCli;
    let Some(home) = home else {
        return Vec::new();
    };
    let rel: &[&[&str]] = match cli {
        SeatCli::Claude => &[&[".claude"], &[".claude.json"], &[".config", "claude"]],
        SeatCli::Codex => &[&[".codex"]],
        SeatCli::Pi => &[&[".pi"]],
        SeatCli::Copilot => &[&[".copilot"]],
        SeatCli::Opencode => &[
            &[".config", "opencode"],
            &[".local", "share", "opencode"],
            &[".local", "state", "opencode"],
            &[".cache", "opencode"],
        ],
        SeatCli::Agy => &[&[".gemini"]],
        SeatCli::Other => &[],
    };
    rel.iter()
        .map(|segs| {
            let mut p = home.to_path_buf();
            for s in *segs {
                p.push(s);
            }
            p
        })
        .collect()
}

/// The per-seat cache dir every governed seat's package caches are pointed into
/// ([`floor_cache_env`]): `<worker home>/cache/<seat root name>`. Writes to the operator's own
/// `~/.cargo`, `~/.npm` or `~/.cache` are denied by the floor, and admitting them would let a
/// worker poison the operator's next build (cargo does not re-verify extracted sources).
pub(crate) fn seat_cache_dir(cli: wicked_apps_core::spawn::SeatCli) -> Option<PathBuf> {
    let base = wicked_apps_core::spawn::worker_home_base().ok()?;
    Some(base.join("cache").join(cli.root_name().unwrap_or("other")))
}

/// The cache variables a governed seat gets under the floor, all inside `cache`: cargo's home,
/// npm / yarn / pnpm / pip / uv / go caches and the XDG cache base (opencode, many tools).
pub(crate) fn floor_cache_env(cache: &Path) -> Vec<(&'static str, PathBuf)> {
    vec![
        ("CARGO_HOME", cache.join("cargo")),
        ("npm_config_cache", cache.join("npm")),
        ("YARN_CACHE_FOLDER", cache.join("yarn")),
        ("npm_config_store_dir", cache.join("pnpm-store")),
        ("PIP_CACHE_DIR", cache.join("pip")),
        ("UV_CACHE_DIR", cache.join("uv")),
        ("GOCACHE", cache.join("go-build")),
        ("GOMODCACHE", cache.join("go-mod")),
        ("XDG_CACHE_HOME", cache.join("xdg")),
    ]
}

/// One governed seat spawn's floor, planned from the carrier's launch facts: the inputs the floor
/// arms from, and the environment the seat must run with under it (its caches and temp redirected
/// into dirs the floor admits).
#[derive(Debug, Clone)]
pub(crate) struct SeatFloorPlan {
    pub(crate) inputs: FloorInputs,
    /// Set on the worker command AFTER every other temp redirect, so these win.
    pub(crate) env: Vec<(&'static str, PathBuf)>,
}

/// The launch facts a carrier hands [`plan_seat_floor`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct SeatLaunch<'a> {
    /// The unit's working directory.
    pub(crate) cwd: &'a Path,
    /// The CLI the seat runs (its configuration-home layout).
    pub(crate) cli: wicked_apps_core::spawn::SeatCli,
    /// The seat's configuration decision (isolated roots, or the operator's inherited home).
    pub(crate) config: &'a wicked_apps_core::spawn::SeatConfig,
    /// A fenced posture (read-only / deliverable-roots): the read-only profile.
    pub(crate) read_only: bool,
    /// Where a fenced unit's temp, build output and caches go instead of the tree (its check
    /// scratch); `None` under the full posture, whose temp is `<cwd>/tmp`.
    pub(crate) scratch: Option<&'a Path>,
    /// The dir the unit's gate hook writes its decisions log in (the wrapped claude carrier); the
    /// in-process ACP gate writes from the daemon, outside the floor, so it passes `None`.
    pub(crate) gov_log_dir: Option<&'a Path>,
    /// Loopback ports the seat's own processes listen on (opencode's pinned server).
    pub(crate) own_ports: &'a [u16],
}

/// Plan the floor for one governed seat spawn whose write roots are `write_roots`.
pub(crate) fn plan_seat_floor(launch: SeatLaunch<'_>, write_roots: Vec<PathBuf>) -> SeatFloorPlan {
    use wicked_apps_core::spawn::{SeatCli, SeatConfig};
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut seat_dirs: Vec<PathBuf> = Vec::new();
    let mut unmask: Vec<PathBuf> = Vec::new();
    match launch.config {
        SeatConfig::Isolated { .. } => seat_dirs.extend(launch.config.owned_dirs()),
        // The hatch: the seat runs under the operator's own configuration, so that is its home.
        SeatConfig::Inherit => {
            let dirs = inherited_cli_dirs(launch.cli, home.as_deref());
            unmask.extend(dirs.iter().cloned());
            seat_dirs.extend(dirs);
        }
    }
    let cache = seat_cache_dir(launch.cli);
    seat_dirs.extend(cache.iter().cloned());
    seat_dirs.extend(launch.scratch.map(Path::to_path_buf));
    seat_dirs.extend(launch.gov_log_dir.map(Path::to_path_buf));
    let mut env: Vec<(&'static str, PathBuf)> =
        cache.as_deref().map(floor_cache_env).unwrap_or_default();
    let tmp = match (launch.read_only, launch.scratch) {
        (true, Some(scratch)) => {
            let tmp = scratch.join("tmp");
            let _ = std::fs::create_dir_all(&tmp);
            for k in ["TMPDIR", "TMP", "TEMP"] {
                env.push((k, tmp.clone()));
            }
            env.push(("CARGO_TARGET_DIR", scratch.join("target")));
            tmp
        }
        _ => launch.cwd.join("tmp"),
    };
    // claude ignores `TMPDIR` for its Bash tool's temp dir (`/private/tmp/claude-<uid>`), which
    // the floor denies: every Bash call failed EPERM without this (DES-PROC-GOVERNANCE §3.1).
    if launch.cli == SeatCli::Claude {
        env.push(("CLAUDE_CODE_TMPDIR", tmp));
    }
    let mut net = floor_net_grants(|k| std::env::var(k).ok());
    for p in launch.own_ports.iter().filter(|p| **p != 0) {
        let g = NetGrant::Loopback { port: *p };
        if !net.contains(&g) {
            net.push(g);
        }
    }
    SeatFloorPlan {
        inputs: FloorInputs {
            cwd: launch.cwd.to_path_buf(),
            write_roots,
            read_only: launch.read_only,
            seat_dirs,
            unmask,
            pinned: Vec::new(),
            net,
            home,
        },
        env,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .hardened()
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A clone with two run worktrees under `wicked-worktrees/` — the layout `repo.rs` creates.
    pub(crate) fn clone_with_two_runs(tag: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!("wicked-wsb-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let clone = base.join("clone");
        std::fs::create_dir_all(&clone).unwrap();
        git(&clone, &["init", "-q", "-b", "main", "."]);
        std::fs::write(clone.join("add.js"), "a\n").unwrap();
        git(&clone, &["add", "."]);
        git(&clone, &["commit", "-qm", "init"]);
        git(
            &clone,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "wicked/run1",
                "wicked-worktrees/run1",
            ],
        );
        git(
            &clone,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "wicked/run2",
                "wicked-worktrees/run2",
            ],
        );
        let own = clone.join("wicked-worktrees").join("run1");
        let sibling = clone.join("wicked-worktrees").join("run2");
        (base, clone, own, sibling)
    }

    #[test]
    fn a_unit_in_a_package_dir_below_its_worktree_gets_the_same_boundary() {
        let (base, _clone, own, _sibling) = clone_with_two_runs("subdir");
        let pkg = own.join("packages").join("api");
        std::fs::create_dir_all(&pkg).unwrap();
        assert_eq!(repo_boundary(&pkg, &[]), repo_boundary(&own, &[]));
        assert!(repo_boundary(&pkg, &[]).is_some());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The #548 proofs skip where no launcher arms, so a CI leg that silently stopped arming would
    /// read green. Pin it: wherever the platform launcher itself runs a trivial command, the
    /// repository boundary must arm for a run worktree.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn the_boundary_arms_wherever_the_platform_launcher_runs() {
        let (tool, args): (&str, &[&str]) = if cfg!(target_os = "macos") {
            (
                "sandbox-exec",
                &["-p", "(version 1)(allow default)", "/usr/bin/true"],
            )
        } else {
            (
                "bwrap",
                &["--ro-bind", "/", "/", "--dev", "/dev", "--", "/bin/true"],
            )
        };
        let Some(path) = crate::validator::find_on_path(tool) else {
            eprintln!("worker_sandbox: {tool} not on PATH — nothing to pin here");
            return;
        };
        let runs = Command::new(path)
            .hardened()
            .args(args)
            .status()
            .is_ok_and(|s| s.success());
        if !runs {
            eprintln!("worker_sandbox: {tool} cannot run here — nothing to pin");
            return;
        }
        let (base, _clone, own, _sibling) = clone_with_two_runs("arms");
        assert!(
            default_worker_sandbox(&own, &[]).is_ok(),
            "{tool} runs on this host, so the repository boundary must arm"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn default_worker_sandbox_names_why_it_did_not_arm() {
        // IG1-core-1: a scratch dir is not a worktree (step 0: the codex exception is gone).
        let dir = std::env::temp_dir().join(format!("wicked-wsb-reason-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(
            default_worker_sandbox(&dir, &[]).err(),
            Some(FloorUnarmed::NotAWorktree)
        );
        let (base, _clone, own, _sibling) = clone_with_two_runs("reason");
        // In a worktree a seat either arms (naming its launcher) or names the launcher
        // gap — never NotAWorktree.
        match default_worker_sandbox(&own, &[]) {
            Ok(armed) => assert!(["sandbox-exec", "bwrap"].contains(&armed.tool)),
            Err(e) => assert!(
                matches!(e, FloorUnarmed::NoLauncher | FloorUnarmed::CannotArm),
                "{e:?}"
            ),
        }
        assert_eq!(FloorUnarmed::NotAWorktree.as_str(), "not_a_worktree");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_plain_directory_is_not_a_run_worktree_and_gets_no_boundary() {
        let dir = std::env::temp_dir().join(format!("wicked-wsb-plain-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(repo_boundary(&dir, &[]), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_boundary_protects_the_clone_and_admits_the_own_tree_and_its_git_state() {
        let (base, clone, own, _sibling) = clone_with_two_runs("shape");
        let b = repo_boundary(&own, &[]).expect("a linked worktree has a boundary");
        let clone = clone.canonicalize().unwrap();
        assert_eq!(b.protected, vec![clone.clone()]);
        let git = clone.join(".git");
        for want in [
            own.canonicalize().unwrap(),
            git.join("worktrees").join("run1"),
            git.join("objects"),
            git.join("refs"),
            git.join("logs"),
        ] {
            assert!(b.admitted.contains(&want), "{want:?} in {:?}", b.admitted);
        }
        assert!(
            !b.admitted
                .iter()
                .any(|p| p.ends_with("run2") || p == &clone),
            "neither the sibling nor the clone root is admitted: {:?}",
            b.admitted
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// codex r3 on core#878: a grant root swapped for a LINK into the clone after it was minted
    /// (the QE ledger root, the notes root) is never admitted — while a real root inside the clone
    /// still is. Mutation: drop the leaf-link check → the sibling worktree is admitted.
    #[cfg(unix)]
    #[test]
    fn a_grant_root_that_is_a_link_into_the_clone_is_never_admitted() {
        let (base, _clone, own, sibling) = clone_with_two_runs("linkroot");
        let planted = base.join("evidence").join(".wicked-qe");
        std::fs::create_dir_all(planted.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&sibling, &planted).unwrap();
        let b = repo_boundary(&own, std::slice::from_ref(&planted))
            .expect("a linked worktree has a boundary");
        let sibling_real = sibling.canonicalize().unwrap();
        assert!(
            !b.admitted.contains(&sibling_real),
            "a planted link must not admit the sibling worktree: {:?}",
            b.admitted
        );
        let b = repo_boundary(&own, std::slice::from_ref(&sibling)).expect("a boundary");
        assert!(
            b.admitted.contains(&sibling_real),
            "a real root inside the clone is still admitted (the check is the link, not the place)"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// core#548 acceptance: a Creator's `python3 -c "open('<sibling>/x','w')"` fails AT THE OS
    /// with a typed reason (EPERM on macOS, EROFS on Linux) through the production default
    /// wrapper, while its own tree, a commit on its run branch, and a write outside the clone
    /// still succeed. Skips (printed) where no launcher can arm (Windows; a runner without user
    /// namespaces).
    #[cfg(unix)]
    #[test]
    fn a_creator_cannot_write_a_sibling_worktree_or_the_clone_root_548() {
        let (base, clone, own, sibling) = clone_with_two_runs("escape");
        let Ok(sandbox) = default_worker_sandbox(&own, &[]).map(|a| a.sandbox) else {
            eprintln!("worker_sandbox: no launcher can arm on this host — the #548 proof skips");
            let _ = std::fs::remove_dir_all(&base);
            return;
        };
        let outside = base.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let script = String::from(
            r#"
import errno, subprocess, sys
def attempt(p):
    try:
        open(p, 'w').write('x')
        return 'wrote'
    except OSError as e:
        return errno.errorcode.get(e.errno, str(e.errno))
print('sibling=' + attempt(sys.argv[1]))
print('clone=' + attempt(sys.argv[2]))
print('own=' + attempt('mine.txt'))
print('outside=' + attempt(sys.argv[3]))
open('add.js', 'a').write('b\n')
r = subprocess.run(['git', 'commit', '-qam', 'fix'], capture_output=True, text=True)
print('commit=' + str(r.returncode) + ' ' + r.stderr.strip().replace('\n', ' | '))
"#,
        );
        let argv: Vec<String> = sandbox
            .wrapper
            .iter()
            .cloned()
            .chain([
                "python3".to_string(),
                "-c".to_string(),
                script,
                sibling.join("x").to_string_lossy().into_owned(),
                clone.join("node_modules").to_string_lossy().into_owned(),
                outside.join("ok").to_string_lossy().into_owned(),
            ])
            .collect();
        let out = Command::new(&argv[0])
            .hardened()
            .args(&argv[1..])
            .current_dir(&own)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .output()
            .expect("the wrapped worker spawns");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let field = |k: &str| {
            stdout
                .lines()
                .find_map(|l| l.strip_prefix(&format!("{k}=")))
                .unwrap_or("<missing>")
                .to_string()
        };
        let denied = |v: &str| v == "EPERM" || v == "EROFS" || v == "EACCES";
        assert!(
            denied(&field("sibling")),
            "a sibling worktree write must fail at the OS: {stdout} / {stderr}"
        );
        assert!(
            denied(&field("clone")),
            "a clone-root write must fail at the OS: {stdout} / {stderr}"
        );
        assert!(!sibling.join("x").exists() && !clone.join("node_modules").exists());
        assert_eq!(field("own"), "wrote", "{stdout} / {stderr}");
        assert_eq!(field("outside"), "wrote", "{stdout} / {stderr}");
        assert!(
            field("commit").starts_with("0"),
            "a commit on the run branch must succeed: {stdout} / {stderr}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// core#678 item 1: the host's boundary verdict, known before any run — armed ⇒ contained;
    /// unarmed with the operator's opt-in ⇒ uncontained; unarmed without ⇒ refused, with the
    /// reason in both spellings.
    #[test]
    fn the_host_boundary_says_what_a_verify_floor_will_do_here() {
        let armed = host_boundary_from(Ok("bwrap"), false, "linux");
        assert_eq!(armed["armed"], true);
        assert_eq!(armed["tool"], "bwrap");
        assert_eq!(armed["verifyFloor"], "contained");
        assert!(armed["reason"].is_null());
        let windows = host_boundary_from(Err(FloorUnarmed::NoLauncher), false, "windows");
        assert_eq!(windows["armed"], false);
        assert_eq!(windows["verifyFloor"], "refused");
        assert_eq!(windows["reason"], "no_launcher");
        assert!(windows["reasonText"]
            .as_str()
            .is_some_and(|t| t.starts_with("no_launcher")));
        let opted = host_boundary_from(Err(FloorUnarmed::CannotArm), true, "linux");
        assert_eq!(opted["verifyFloor"], "uncontained");
        assert_eq!(opted["unsandboxedOptIn"], true);
    }

    /// (IG1-core-3, step 0) The `sandboxPosture` prediction is the host's: every governed unit
    /// gets the seat floor whatever its seat (codex included), its class or its tree.
    #[test]
    fn the_predicted_posture_is_the_hosts_launcher_for_every_seat() {
        let seat = |key: &str| {
            wicked_council::registry::builtin()
                .into_iter()
                .find(|c| c.key == key)
                .expect("a built-in seat")
        };
        let (base, _clone, own, _sibling) = clone_with_two_runs("posture");
        let want = if cfg!(windows) {
            Err(PostureGap::Unarmed(FloorUnarmed::NoLauncher))
        } else {
            boundary_tool().map_err(PostureGap::Unarmed)
        };
        for key in ["copilot", "codex", "claude", "pi"] {
            assert_eq!(predicted_posture(&seat(key), Some(&own)), want, "{key}");
            assert_eq!(predicted_posture(&seat(key), None), want, "{key}, unbound");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    // ---------------------------------------------------------------------------------------
    // Step 0: the seat floor, per seat kind, per spike action (DES-PROC-GOVERNANCE §4)
    // ---------------------------------------------------------------------------------------

    use wicked_apps_core::spawn::{SeatCli, SeatConfig};

    /// One lane-local fixture: a clone with two run worktrees, a bare remote OUTSIDE the tree, an
    /// outside dir, a fake operator home with a fake secret, a seat home, a notes root.
    struct Fixture {
        base: PathBuf,
        own: PathBuf,
        sibling: PathBuf,
        outside: PathBuf,
        home: PathBuf,
        seat_root: PathBuf,
        notes: PathBuf,
    }

    fn fixture(tag: &str) -> Fixture {
        let (base, _clone, own, sibling) = clone_with_two_runs(tag);
        let outside = base.join("outside");
        let home = base.join("ophome");
        let seat_root = base.join("worker-home").join("seat");
        let notes = base.join("notes");
        for d in [&outside, &home.join(".ssh"), &seat_root, &notes] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(home.join(".ssh").join("id_rsa"), "FAKE-KEY-NOT-A-SECRET\n").unwrap();
        let remote = base.join("origin.git");
        git(&base, &["init", "-q", "--bare", "origin.git"]);
        git(
            &own,
            &["remote", "add", "origin", &remote.to_string_lossy()],
        );
        let c = |p: &Path| p.canonicalize().unwrap();
        Fixture {
            own: c(&own),
            sibling: c(&sibling),
            outside: c(&outside),
            home: c(&home),
            seat_root: c(&seat_root),
            notes: c(&notes),
            base,
        }
    }

    /// The seat's configuration decision as the resolver makes it: the CLI's own home variable(s)
    /// pointing into the lane-local seat root.
    fn seat_config(cli: SeatCli, root: &Path) -> SeatConfig {
        use wicked_apps_core::spawn::*;
        let set: Vec<(&'static str, PathBuf)> = match cli {
            SeatCli::Claude => vec![(CLAUDE_CONFIG_DIR_ENV, root.to_path_buf())],
            SeatCli::Codex => vec![(CODEX_HOME_ENV, root.to_path_buf())],
            SeatCli::Pi => vec![(PI_AGENT_DIR_ENV, root.to_path_buf())],
            SeatCli::Copilot => vec![(COPILOT_HOME_ENV, root.to_path_buf())],
            SeatCli::Opencode => vec![
                (XDG_CONFIG_HOME_ENV, root.join("config")),
                (XDG_DATA_HOME_ENV, root.join("data")),
                (XDG_STATE_HOME_ENV, root.join("state")),
            ],
            _ => vec![(HOME_ENV, root.to_path_buf())],
        };
        SeatConfig::Isolated {
            cli,
            root: Some(root.to_path_buf()),
            set,
            strip: Vec::new(),
        }
    }

    /// The floor for `cli` in `f`, planned exactly as a carrier plans it, with the fixture's fake
    /// operator home under the masks and `admit` as a pinned loopback port.
    fn plan_for(f: &Fixture, cli: SeatCli, read_only: bool, admit: u16) -> SeatFloorPlan {
        let cfg = seat_config(cli, &f.seat_root);
        let scratch = f.base.join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        let roots = if read_only {
            vec![f.notes.clone()]
        } else {
            vec![f.own.clone(), f.notes.clone()]
        };
        let mut plan = plan_seat_floor(
            SeatLaunch {
                cwd: &f.own,
                cli,
                config: &cfg,
                read_only,
                scratch: read_only.then_some(scratch.as_path()),
                gov_log_dir: Some(&f.base.join("gov")),
                own_ports: &[admit],
            },
            roots,
        );
        plan.inputs.home = Some(f.home.clone());
        plan
    }

    /// Run `script` (sh) as the seat's worker under its armed floor; `KEY=value` lines out.
    fn run_under(
        f: &Fixture,
        cli: SeatCli,
        plan: &SeatFloorPlan,
        armed: &ArmedFloor,
        script: &str,
    ) -> std::collections::BTreeMap<String, String> {
        let mut argv = armed.sandbox.wrapper.clone();
        argv.extend(["/bin/sh".to_string(), "-c".to_string(), script.to_string()]);
        let mut cmd = Command::new(&argv[0]);
        cmd.hardened()
            .args(&argv[1..])
            .current_dir(&f.own)
            .stdin(std::process::Stdio::null())
            .env("HOME", &f.home)
            .env("OPH", &f.home)
            .env("TMPDIR", f.own.join("tmp"))
            .env("OUT", &f.outside)
            .env("SIB", &f.sibling)
            .env("SEAT", &f.seat_root)
            .env("NOTES", &f.notes)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid");
        if let SeatConfig::Isolated { set, .. } = seat_config(cli, &f.seat_root) {
            for (k, v) in set {
                cmd.env(k, v);
            }
        }
        for (k, v) in &plan.env {
            cmd.env(k, v);
        }
        let out = cmd.output().expect("the floored worker spawns");
        let mut r: std::collections::BTreeMap<String, String> =
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|l| l.split_once('='))
                .map(|(k, v)| (k.to_string(), v.trim().to_string()))
                .collect();
        r.insert(
            "_stderr".to_string(),
            String::from_utf8_lossy(&out.stderr)
                .chars()
                .take(2000)
                .collect(),
        );
        r
    }

    /// The spike's actions as one shell script: each prints `name=ok` or `name=denied`.
    const SPIKE_ACTIONS: &str = r#"
t() { if eval "$2" >/dev/null 2>&1; then echo "$1=ok"; else echo "$1=denied"; fi; }
t edit_in 'printf x > edited-by-agent.txt'
t write_out_file 'python3 -c "open(\"$OUT/filetool-outside.txt\",\"w\").write(\"x\")"'
t shell_in 'echo ok > shell-inside.txt'
t write_out_shell 'echo x > "$OUT/shell-outside.txt"'
t child_sh_out "/bin/sh -c 'echo c > \"\$OUT/child-sh.txt\"'"
t child_bash_out "bash -c 'echo c > \"\$OUT/child-bash.txt\"'"
t sibling_write 'echo x > "$SIB/pwned"'
t operator_home_write 'echo x > "$OPH/planted"'
t seat_home_write 'echo x > "$SEAT/state.json"'
t tmp_write 'mkdir -p "$TMPDIR" && echo x > "$TMPDIR/t"'
t notes_write 'echo x > "$NOTES/n.md"'
t secret_read 'cat "$OPH/.ssh/id_rsa"'
t commit 'git add -A && git commit -qm floor'
t git_push 'git push -q origin HEAD:refs/heads/escape'
t nested_sandbox '/usr/bin/sandbox-exec -p "(version 1)(allow default)" /usr/bin/true'
"#;

    /// The spike's per-action results for every seat kind, through the production plan and
    /// renderer: in-tree edits and shell writes, a commit on the run branch, the seat's home,
    /// temp and notes root are allowed; every write outside (file tool, shell, child shells,
    /// python, a sibling worktree, the operator's home), a push into a remote outside the tree,
    /// and a secret read are refused by the kernel. Skips where no launcher arms (printed).
    #[cfg(unix)]
    #[test]
    fn the_seat_floor_holds_every_spike_action_for_every_seat_kind() {
        if boundary_tool().is_err() {
            eprintln!("worker_sandbox: no launcher can arm here — the step-0 matrix skips");
            return;
        }
        for cli in [
            SeatCli::Claude,
            SeatCli::Codex,
            SeatCli::Pi,
            SeatCli::Opencode,
            SeatCli::Copilot,
            SeatCli::Agy,
        ] {
            let f = fixture(&format!("matrix-{cli:?}").to_lowercase());
            let plan = plan_for(&f, cli, false, 0);
            let armed = seat_floor(&plan.inputs).expect("the floor arms where the launcher does");
            let r = run_under(&f, cli, &plan, &armed, SPIKE_ACTIONS);
            let want_ok = [
                "edit_in",
                "shell_in",
                "seat_home_write",
                "tmp_write",
                "notes_write",
                "commit",
            ];
            let want_denied = [
                "write_out_file",
                "write_out_shell",
                "child_sh_out",
                "child_bash_out",
                "sibling_write",
                "operator_home_write",
                "secret_read",
                "git_push",
            ];
            for k in want_ok {
                assert_eq!(
                    r.get(k).map(String::as_str),
                    Some("ok"),
                    "{cli:?} {k}: {r:?}"
                );
            }
            for k in want_denied {
                assert_eq!(
                    r.get(k).map(String::as_str),
                    Some("denied"),
                    "{cli:?} {k}: {r:?}"
                );
            }
            // Filesystem truth, not only the exit codes.
            assert!(
                std::fs::read_dir(&f.outside).unwrap().next().is_none(),
                "{cli:?}"
            );
            assert!(!f.sibling.join("pwned").exists() && !f.home.join("planted").exists());
            let refs = Command::new("git")
                .hardened()
                .args(["--git-dir", &f.base.join("origin.git").to_string_lossy()])
                .args(["for-each-ref", "--format=%(refname)"])
                .output()
                .unwrap();
            assert!(refs.stdout.is_empty(), "{cli:?}: no ref reached the remote");
            if cfg!(target_os = "macos") {
                // Why each CLI's own sandbox must be off under ours: a nested apply fails.
                assert_eq!(r.get("nested_sandbox").map(String::as_str), Some("denied"));
            }
            let _ = std::fs::remove_dir_all(&f.base);
        }
    }

    /// The read-only profile (a fenced posture): the tree AND its git state are not writable —
    /// no edit, no commit, no `git update-ref` of another branch — while the notes root, the
    /// seat's home and its scratch temp are.
    #[cfg(unix)]
    #[test]
    fn the_read_only_profile_drops_the_tree_and_its_git_metadata() {
        if boundary_tool().is_err() {
            eprintln!("worker_sandbox: no launcher can arm here — the read-only proof skips");
            return;
        }
        for cli in [
            SeatCli::Claude,
            SeatCli::Codex,
            SeatCli::Pi,
            SeatCli::Opencode,
        ] {
            let f = fixture(&format!("ro-{cli:?}").to_lowercase());
            let plan = plan_for(&f, cli, true, 0);
            let armed = seat_floor(&plan.inputs).expect("arms");
            assert_eq!(armed.spec.as_ref().unwrap().profile, "read-only");
            let script = r#"
t() { if eval "$2" >/dev/null 2>&1; then echo "$1=ok"; else echo "$1=denied"; fi; }
t edit_in 'printf x > edited.txt'
t update_ref 'git update-ref refs/heads/wicked/run2 HEAD'
t commit 'git commit -q --allow-empty -m ro'
t notes_write 'echo x > "$NOTES/n.md"'
t seat_home_write 'echo x > "$SEAT/s"'
t scratch_tmp 'echo x > "$TMPDIR/t"'
"#;
            let r = run_under(&f, cli, &plan, &armed, script);
            for k in ["edit_in", "update_ref", "commit"] {
                assert_eq!(
                    r.get(k).map(String::as_str),
                    Some("denied"),
                    "{cli:?} {k}: {r:?}"
                );
            }
            for k in ["notes_write", "seat_home_write", "scratch_tmp"] {
                assert_eq!(
                    r.get(k).map(String::as_str),
                    Some("ok"),
                    "{cli:?} {k}: {r:?}"
                );
            }
            let _ = std::fs::remove_dir_all(&f.base);
        }
    }

    /// The network half (macOS: SBPL pins outbound; Linux leaves it open — documented). A pinned
    /// loopback port is reachable, any other loopback port (a local daemon) and a raw IP on port
    /// 80 are refused at connect.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_seat_floor_pins_outbound_network_on_macos() {
        if boundary_tool().is_err() {
            eprintln!("worker_sandbox: no launcher can arm here — the network proof skips");
            return;
        }
        let admitted = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let other = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let (pa, po) = (
            admitted.local_addr().unwrap().port(),
            other.local_addr().unwrap().port(),
        );
        let f = fixture("net");
        let plan = plan_for(&f, SeatCli::Claude, false, pa);
        let armed = seat_floor(&plan.inputs).expect("arms");
        let py = |port: u16| {
            format!(
                "python3 -c \"import socket; socket.create_connection(('127.0.0.1', {port}), 3)\""
            )
        };
        let script = format!(
            "t() {{ if eval \"$2\" >/dev/null 2>&1; then echo \"$1=ok\"; else echo \"$1=denied\"; fi; }}\n\
             t admitted_port '{}'\nt other_port '{}'\n\
             t raw_ip_80 'python3 -c \"import socket; socket.create_connection((\\\"1.1.1.1\\\", 80), 3)\"'\n",
            py(pa).replace('\'', "'\\''"),
            py(po).replace('\'', "'\\''"),
        );
        let r = run_under(&f, SeatCli::Claude, &plan, &armed, &script);
        assert_eq!(
            r.get("admitted_port").map(String::as_str),
            Some("ok"),
            "{r:?}"
        );
        assert_eq!(
            r.get("other_port").map(String::as_str),
            Some("denied"),
            "{r:?}"
        );
        assert_eq!(
            r.get("raw_ip_80").map(String::as_str),
            Some("denied"),
            "{r:?}"
        );
        drop((admitted, other));
        let _ = std::fs::remove_dir_all(&f.base);
    }

    /// Fail closed: on macOS / Linux a floor that cannot arm REFUSES the governed unit (with the
    /// `could not run` text the failover ladder reads); on Windows there is no floor yet and the
    /// unit keeps today's behaviour, labelled `no OS floor` — the arm is never even attempted.
    #[test]
    fn a_governed_floor_fails_closed_except_on_windows_where_it_is_labelled() {
        match governed_floor_on(false, || Err(FloorUnarmed::CannotArm)) {
            GovernedFloor::Refused(FloorUnarmed::CannotArm) => {}
            other => panic!("unix must refuse: {other:?}"),
        }
        let mut attempted = false;
        match governed_floor_on(true, || {
            attempted = true;
            Err(FloorUnarmed::NoLauncher)
        }) {
            GovernedFloor::NoOsFloor => {}
            other => panic!("windows is labelled, not refused: {other:?}"),
        }
        assert!(!attempted);
        assert_eq!(NO_OS_FLOOR, "no OS floor");
        let text = floor_refusal_text("pi", FloorUnarmed::NoLauncher);
        assert!(text.contains("could not run this governed unit"), "{text}");
        assert!(
            crate::acp_runner::is_worker_originated_failure(&text),
            "fails over"
        );
        // An unresolvable working directory refuses too.
        let inp = FloorInputs {
            cwd: std::env::temp_dir()
                .join("wicked-wsb-no-such-dir-xyz")
                .join("u"),
            ..Default::default()
        };
        assert_eq!(
            resolve_seat_floor(&inp).err(),
            Some(FloorUnarmed::RootUnresolvable)
        );
        if cfg!(windows) {
            assert!(matches!(governed_floor(&inp), GovernedFloor::NoOsFloor));
        } else {
            assert!(matches!(governed_floor(&inp), GovernedFloor::Refused(_)));
        }
    }

    /// The network grants: 443 to any host always; a loopback URL pinned to its port, a remote
    /// one (a proxy) as `*:<port>`; the operator's `WICKED_WORKER_NET` entries; junk ignored.
    #[test]
    fn the_floor_net_grants_come_from_the_seats_urls() {
        let env = |k: &str| match k {
            "WICKED_CREW_URL" => Some("http://127.0.0.1:7701".to_string()),
            "ANTHROPIC_BASE_URL" => Some("http://localhost:4000/v1".to_string()),
            "HTTPS_PROXY" => Some("http://user:pw@proxy.corp:8080".to_string()),
            "WICKED_WORKER_NET" => Some("localhost:9999, *:8443, bogus, 1.2.3.4:5".to_string()),
            _ => None,
        };
        assert_eq!(
            floor_net_grants(env),
            vec![
                NetGrant::AnyHost { port: 443 },
                NetGrant::Loopback { port: 7701 },
                NetGrant::Loopback { port: 4000 },
                NetGrant::AnyHost { port: 8080 },
                NetGrant::Loopback { port: 9999 },
                NetGrant::AnyHost { port: 8443 },
            ]
        );
        assert_eq!(url_host_port("[::1]:9"), Some(("::1".into(), 9)));
        assert_eq!(
            url_host_port("https://api.example"),
            Some(("api.example".into(), 443))
        );
        assert_eq!(url_host_port("not a url"), None);
    }

    /// The profile's shape: deny-by-default writes, the masks after every allow, the engine's
    /// pinned launch files kept read-only, and the outbound pin — and the spec judges a path
    /// exactly as the rules order it.
    #[test]
    fn the_seat_profile_denies_writes_by_default_and_masks_last() {
        let f = SeatFloor {
            profile: "governed".into(),
            tree: "/w/clone/wt/run1".into(),
            writable: vec!["/seat".into()],
            protected: vec!["/w/clone".into()],
            readmitted: vec!["/w/clone/wt/run1".into()],
            readmitted_files: Vec::new(),
            masks: vec!["/h/.ssh".into()],
            pinned: vec!["/seat/settings.json".into()],
            net: vec![
                NetGrant::AnyHost { port: 443 },
                NetGrant::Loopback { port: 7701 },
            ],
        };
        let p = sbpl_seat_profile(&f);
        let at = |needle: &str| p.find(needle).unwrap_or_else(|| panic!("{needle} in {p}"));
        assert!(at("(deny file-write*)") < at("(allow file-write* (subpath \"/seat\"))"));
        assert!(
            at("(deny file-write* (subpath \"/w/clone\"))")
                < at("(allow file-write* (subpath \"/w/clone/wt/run1\"))")
        );
        assert!(
            at("(allow file-write* (subpath \"/w/clone/wt/run1\"))")
                < at("(deny file-read* (subpath \"/h/.ssh\"))")
        );
        at("(deny file-write* (literal \"/seat/settings.json\"))");
        at("(allow network-outbound (remote tcp \"*:443\"))");
        at("(allow network-outbound (remote ip \"localhost:7701\"))");
        assert!(f.allows_write(Path::new("/w/clone/wt/run1/src/a.rs")));
        assert!(!f.allows_write(Path::new("/w/clone/wt/run2/x")));
        assert!(!f.allows_write(Path::new("/elsewhere")));
        assert!(!f.allows_write(Path::new("/seat/settings.json")));
        assert!(f.allows_write(Path::new("/seat/other.json")));
        assert!(!f.allows_read(Path::new("/h/.ssh/id_rsa")));
    }

    /// opencode under the floor runs its own server on the pinned port inside the same floor:
    /// `opencode run …` (never `--standalone`, never the shared background service) becomes the
    /// pinned-server program; any other argv, or one already naming `--server`, is left alone.
    #[test]
    fn opencode_runs_its_server_on_a_pinned_port_under_the_floor() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let out =
            opencode_pinned_server_argv(&v(&["opencode", "run", "--standalone", "do it"]), 47001)
                .expect("an opencode run is rewritten");
        assert_eq!(out[0], "/bin/sh");
        assert_eq!(&out[4..], &v(&["opencode", "47001", "do it"])[..]);
        assert!(out[2].contains("serve --hostname 127.0.0.1 --port"));
        assert!(out[2].contains("run --server"));
        assert!(
            opencode_pinned_server_argv(&v(&["opencode", "run", "--server", "x", "p"]), 1)
                .is_none()
        );
        assert!(opencode_pinned_server_argv(&v(&["opencode", "acp"]), 1).is_none());
        assert!(opencode_pinned_server_argv(&v(&["claude", "run"]), 1).is_none());
    }

    /// Lane tool, not a CI check (`#[ignore]`): render the PRODUCTION seat floor for a live check
    /// of a real CLI against an offline model stub. Reads `WICKED_FLOOR_DUMP` (output prefix),
    /// `WICKED_FLOOR_CWD`, `WICKED_FLOOR_CLI` (claude | codex | pi | opencode | copilot),
    /// `WICKED_FLOOR_SEAT_ROOT`, `WICKED_FLOOR_HOME` (the operator home the masks resolve under),
    /// `WICKED_FLOOR_PORTS` (loopback ports, comma separated) and `WICKED_FLOOR_READ_ONLY=1`;
    /// writes `<prefix>.sb` (the profile) and `<prefix>.env` (`KEY=value` the seat runs with).
    #[test]
    #[ignore]
    fn dump_seat_floor_profile_for_a_live_check() {
        let Ok(prefix) = std::env::var("WICKED_FLOOR_DUMP") else {
            return;
        };
        let var = |k: &str| std::env::var(k).unwrap_or_default();
        let cli = match var("WICKED_FLOOR_CLI").as_str() {
            "claude" => SeatCli::Claude,
            "codex" => SeatCli::Codex,
            "pi" => SeatCli::Pi,
            "opencode" => SeatCli::Opencode,
            "copilot" => SeatCli::Copilot,
            other => panic!("unknown cli {other}"),
        };
        let cwd = PathBuf::from(var("WICKED_FLOOR_CWD"));
        let root = PathBuf::from(var("WICKED_FLOOR_SEAT_ROOT"));
        let read_only = var("WICKED_FLOOR_READ_ONLY") == "1";
        let ports: Vec<u16> = var("WICKED_FLOOR_PORTS")
            .split(',')
            .filter_map(|p| p.trim().parse().ok())
            .collect();
        let cfg = seat_config(cli, &root);
        let scratch = root.join("scratch");
        let mut plan = plan_seat_floor(
            SeatLaunch {
                cwd: &cwd,
                cli,
                config: &cfg,
                read_only,
                scratch: read_only.then_some(scratch.as_path()),
                gov_log_dir: None,
                own_ports: &ports,
            },
            if read_only {
                Vec::new()
            } else {
                vec![cwd.clone()]
            },
        );
        plan.inputs.home = Some(PathBuf::from(var("WICKED_FLOOR_HOME")));
        let spec = resolve_seat_floor(&plan.inputs).expect("resolves");
        std::fs::write(format!("{prefix}.sb"), sbpl_seat_profile(&spec)).unwrap();
        let mut env = String::new();
        if let SeatConfig::Isolated { set, .. } = &cfg {
            for (k, v) in set {
                env.push_str(&format!("{k}={}\n", v.display()));
            }
        }
        for (k, v) in &plan.env {
            env.push_str(&format!("{k}={}\n", v.display()));
        }
        std::fs::write(format!("{prefix}.env"), env).unwrap();
        // The argv the engine launches, through the production rewrites (`WICKED_FLOOR_ARGV`,
        // unit-separator (0x1f) joined in, NUL-separated out): codex's own sandbox off, opencode's pinned server.
        if let Ok(raw) = std::env::var("WICKED_FLOOR_ARGV") {
            let mut argv: Vec<String> = raw.split('\u{1f}').map(str::to_string).collect();
            codex_sandbox_off_under_floor(&mut argv);
            if let Some(p) = ports.first() {
                if let Some(pinned) = opencode_pinned_server_argv(&argv, *p) {
                    argv = pinned;
                }
            }
            std::fs::write(format!("{prefix}.argv"), argv.join("\0")).unwrap();
        }
    }
}
