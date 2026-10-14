//! The default OS write boundary for agent workers: the repository boundary (core#548).
//!
//! A unit in a run worktree may write its own tree and nothing else of the repository it was
//! cut from. Until this module, that rule was policy only — the gate hook and the ACP permission
//! bridge read the TEXT of each command and guessed whether it wrote (#540/#541/#542), and three
//! review cycles showed that a shell string cannot be judged soundly: a Creator's
//! `python3 -c "open('<sibling worktree>/x','w')"` was neither blocked nor detected, and the
//! F-E2E-029(b) install evasions (`env -C`, `npm_config_prefix`, a piped `sh -c`, an in-command
//! symlink) reached the clone root the same way.
//!
//! The boundary is a kernel mount, armed on the worker PROCESS TREE at spawn on both carriers
//! (the wrapped CLI spawn and the ACP bridge spawn):
//!
//! - **Read-only:** the clone root the worktree belongs to (its main working tree, its `.git`,
//!   every sibling run worktree under `wicked-worktrees/`) and the worktree's parent directory.
//! - **Writable inside that:** the unit's own worktree, its own gitdir
//!   (`.git/worktrees/<name>`), and the shared `objects`/`refs`/`logs` a commit on the run branch
//!   writes, plus any write root the unit was handed that lies inside the clone.
//! - **Untouched:** everything outside the clone. The seat's own configuration home, package
//!   caches (`~/.npm`, `~/.cargo`, `~/Library/Caches`, …) and the network behave exactly as
//!   before, so a normal build, install or commit inside the worktree is unchanged.
//!
//! macOS arms it with `sandbox-exec` (`(allow default)` + a write deny on the protected roots +
//! write allows on the admitted ones — the later rule wins); Linux with `bwrap` (`--bind / /`,
//! `--ro-bind` over each protected root, `--bind` back each admitted one). A write outside fails
//! at the OS with a typed error: `EPERM` ("Operation not permitted") on macOS, `EROFS`
//! ("Read-only file system") on Linux.
//!
//! **What it is not.** It is not a read jail and not exfiltration protection (reads and the
//! network stay open; the curated secret masks belong to the validator jail and the opt-in strict
//! `os_sandbox` profile). It does not wrap a seat whose CLI arms its OWN OS sandbox (codex's
//! `--sandbox workspace-write` / `read-only`): macOS refuses a nested `sandbox_apply` under any
//! profile that denies a write (`sandbox-exec: sandbox_apply: Operation not permitted`), so the
//! engine's wrap would break every codex command, and codex's own boundary is already stricter.
//! A seat record with `os_sandbox = true` keeps the strict allowlist profile instead
//! ([`crate::validator::detect_worker_sandbox`]). Windows has no launcher: nothing is armed there
//! and the posture stays advisory (core#416).
//!
//! **Arm-or-skip, never break the spawn.** The launcher is probed once per process with the same
//! shape (a `bwrap` on a kernel that refuses unprivileged user namespaces, or a daemon already
//! running inside a sandbox, cannot arm); a host that cannot arm runs the worker exactly as before.

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

/// Whether the seat's CLI arms its own OS sandbox, so the engine must not wrap it (a nested
/// `sandbox_apply` fails on macOS). `name` is a seat key, a CLI key or a bridge binary.
pub(crate) fn seat_arms_its_own_os_sandbox(name: &str) -> bool {
    let base = Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name);
    base == "codex" || base == "codex-acp" || base.starts_with("codex:")
}

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
    /// The seat's CLI arms its own OS sandbox (codex), so the engine must not wrap it.
    SeatArmsItsOwn,
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
    /// Container floor: the carrier the unit reached runs CLIs on the host (the PTY session
    /// carrier), never in a seat container.
    HostOnlyCarrier,
    /// Container floor: the daemon's model gateway (the seat's only egress) could not be started
    /// or attached.
    GatewayUnavailable,
    /// Container floor: a governed claude seat on an operator-supplied image, whose claude the
    /// engine cannot verify honours the blocking HTTP gate hook.
    UnverifiedHookImage,
    /// Container floor: the agent has no usable sign-in in its broker (signed out, expired, or a
    /// sign-in still being published), so its seat would only be refused upstream.
    AgentSignedOut,
    /// (C5) A governed unit on the interim host floor whose seat was governed only over ACP.
    NeedsContainerFloor,
}

impl FloorUnarmed {
    /// The reason in words, for a `governanceUnenforced` / `sandboxUnenforced` disclosure.
    pub(crate) fn describe(self) -> &'static str {
        match self {
            FloorUnarmed::NotAWorktree => {
                "not_a_worktree: the unit does not run in a linked run worktree, so there is no \
                 repository boundary to arm"
            }
            FloorUnarmed::SeatArmsItsOwn => {
                "seat_arms_its_own: the seat's CLI arms its own OS sandbox, which the engine does \
                 not wrap"
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
            FloorUnarmed::HostOnlyCarrier => {
                "host_only_carrier: this carrier runs the CLI on the host, outside any seat \
                 container; under the container floor a governed unit runs only on the headless \
                 carrier"
            }
            FloorUnarmed::GatewayUnavailable => {
                "gateway_unavailable: the daemon's model gateway (agentgateway, the seat's only \
                 egress) could not be started or attached to the seat's network"
            }
            FloorUnarmed::UnverifiedHookImage => {
                "unverified_hook_image: a governed claude seat runs only the engine's pinned claude \
                 image (2.1.295 or later fails its HTTP gate hook closed); unset \
                 WICKED_SEAT_IMAGE_CLAUDE for governed runs"
            }
            FloorUnarmed::AgentSignedOut => {
                "agent_signed_out: this agent is not signed in for container seats (signed out, \
                 expired, or a sign-in still finishing) — sign in from Agents → Sign in, which \
                 opens the agent's login container, then retry"
            }
            FloorUnarmed::NeedsContainerFloor => {
                "needs_container_floor: this seat's tool calls were checked over ACP, which is \
                 removed; on the interim host floor (WICKED_SEAT_FLOOR=host) nothing would check \
                 them — run with the container floor (the default), or use a claude seat"
            }
        }
    }

    /// The reason's wire spelling, for a disclosure (and `hostBoundary().reason`, core#678).
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            FloorUnarmed::NotAWorktree => "not_a_worktree",
            FloorUnarmed::SeatArmsItsOwn => "seat_arms_its_own",
            FloorUnarmed::NoLauncher => "no_launcher",
            FloorUnarmed::CannotArm => "cannot_arm",
            FloorUnarmed::RootUnresolvable => "root_unresolvable",
            FloorUnarmed::NoContainerRuntime => "no_container_runtime",
            FloorUnarmed::ContainerRuntimeDown => "container_runtime_down",
            FloorUnarmed::NoSeatImage => "no_seat_image",
            FloorUnarmed::ContainerUnsupportedHost => "container_unsupported_host",
            FloorUnarmed::GateHookUnreachable => "gate_hook_unreachable",
            FloorUnarmed::HostOnlyCarrier => "host_only_carrier",
            FloorUnarmed::GatewayUnavailable => "gateway_unavailable",
            FloorUnarmed::UnverifiedHookImage => "unverified_hook_image",
            FloorUnarmed::AgentSignedOut => "agent_signed_out",
            FloorUnarmed::NeedsContainerFloor => "needs_container_floor",
        }
    }
}

/// (IG1-core-1) The repository boundary as armed for one spawn: the wrapper, and the launcher that
/// armed it (`"sandbox-exec"` | `"bwrap"`) — the `_wicked_gov_boundary` an `os_sandbox` marker names.
#[derive(Debug)]
pub(crate) struct ArmedFloor {
    pub(crate) sandbox: crate::validator::WorkerSandbox,
    pub(crate) tool: &'static str,
}

/// (IG1-core-2) The `_wicked_gov_boundary` an `os_sandbox` marker names when the boundary is the
/// seat's OWN sandbox (codex `--sandbox workspace-write|read-only`), not an engine launcher.
pub(crate) const SEAT_CODEX_BOUNDARY: &str = "seat:codex";

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

/// (IG1-core-3) The write containment a seat is PREDICTED to run under at distribution — the
/// `sandboxPosture` decision. `Ok(boundary)` (`"sandbox-exec"` | `"bwrap"` | `"seat:codex"`) when
/// the seat sits on the OS-sandbox floor (governance class `os_sandbox`, or `acp.os_sandbox: true`)
/// AND the run is bound to a linked run worktree AND this host's launcher arms (a self-sandboxing
/// codex seat brings its own); else which of the three failed. The arm site's marker is the fact;
/// this predicts it.
pub(crate) fn predicted_posture(
    seat: &wicked_council::AgenticCli,
    workdir: Option<&Path>,
) -> Result<&'static str, PostureGap> {
    let floor = wicked_council::governance_class(seat)
        == wicked_council::GovernanceClass::OsSandboxFloor
        || seat.acp.as_ref().is_some_and(|a| a.os_sandbox);
    if !floor {
        return Err(PostureGap::NotOnTheFloor);
    }
    if workdir.and_then(|w| repo_boundary(w, &[])).is_none() {
        return Err(PostureGap::Unarmed(FloorUnarmed::NotAWorktree));
    }
    if seat_arms_its_own_os_sandbox(&seat.key) || seat_arms_its_own_os_sandbox(&seat.binary) {
        return Ok(SEAT_CODEX_BOUNDARY);
    }
    boundary_tool().map_err(PostureGap::Unarmed)
}

/// (IG1-core-3) Why [`predicted_posture`] is `advisory`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PostureGap {
    /// The seat's governance class is not `os_sandbox` and its record sets no `os_sandbox`.
    NotOnTheFloor,
    /// On the floor, but it will not arm: no run worktree, no launcher, or a launcher that
    /// cannot arm here.
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

/// The default worker wrapper for a spawn in `cwd` for seat/CLI `seat`: the repository boundary
/// rendered for this host's launcher with the launcher's name, or why it did not arm (not a run
/// worktree, a self-sandboxing seat, no launcher, a launcher that cannot arm here) — the worker
/// then spawns as before.
pub(crate) fn default_worker_sandbox(
    cwd: &Path,
    write_roots: &[PathBuf],
    seat: &str,
) -> Result<ArmedFloor, FloorUnarmed> {
    if seat_arms_its_own_os_sandbox(seat) {
        return Err(FloorUnarmed::SeatArmsItsOwn);
    }
    let boundary = repo_boundary(cwd, write_roots).ok_or(FloorUnarmed::NotAWorktree)?;
    let tool = boundary_launcher()?;
    Ok(ArmedFloor {
        sandbox: crate::validator::WorkerSandbox {
            wrapper: launcher_argv(tool, &boundary),
            level: crate::validator::SandboxLevel::Sandboxed,
            downgrade_reason: None,
        },
        tool: launcher_name(tool),
    })
}

/// The text of a governed unit refused because its seat container did not arm. Says `could not
/// run`, which the runner reads as a seat failure, so the actor fails the unit over.
pub(crate) fn floor_refusal_text(cli: &str, why: FloorUnarmed) -> String {
    format!(
        "(seat '{cli}' could not run this governed unit: its seat container did not arm — {}; \
         the unit is refused rather than run outside the floor (fail closed) — fix it, or let \
         the run fail over to an eligible seat)",
        why.describe()
    )
}

// ── The seat floor's root computation (rendered by the container floor) ──────────────────────

/// One network grant on the seat floor: a loopback port the seat serves itself (its egress is
/// the gateway, not a grant).
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
    /// The loopback ports the seat serves itself (its own server; the gateway is its egress).
    pub(crate) net: Vec<NetGrant>,
    /// The operator's home the secret masks resolve under (the process `HOME`).
    pub(crate) home: Option<PathBuf>,
    /// The git dir the engine pinned for this unit from the REGISTERED repository at dispatch
    /// (`worktree_baseline.git_dir`): the only anchor the container floor trusts for git grants
    /// outside the tree (the worktree's own `.git` is seat-writable).
    pub(crate) trusted_git_dir: Option<PathBuf>,
}

/// The seat floor as resolved for one spawn: what the container floor renders as mounts and
/// Landlock grants ([`crate::container_floor::mount_plan`]). Every path is canonical.
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
    /// The seat's own loopback ports.
    pub(crate) net: Vec<NetGrant>,
}

/// `p` made canonical even when its leaf does not exist yet: the nearest existing ancestor is
/// canonicalized and the rest re-appended (mounts sit at resolved paths: `/var` is
/// `/private/var` on macOS). `None` when nothing of it resolves.
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

/// The program stem of `argv[0]` (`/opt/bin/codex` → `codex`).
fn program_stem(argv: &[String]) -> Option<&str> {
    argv.first()
        .and_then(|b| Path::new(b).file_stem())
        .and_then(|s| s.to_str())
}

/// codex's own sandbox OFF, because ours is on: every `--sandbox <mode>` / `-s <mode>` /
/// `--sandbox=<mode>` becomes `danger-full-access`, `--full-auto` (which implies
/// `workspace-write`) and any `-c sandbox_mode=…` are dropped, and the mode is added when the
/// argv names none (codex's default is read-only, which also nests). Inside a seat container
/// codex's own sandbox cannot run (bwrap needs user namespaces) and fails every command closed.
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
    // Never the container floor's canary port: the seat launcher refuses a policy granting it.
    (0..4).find_map(|_| {
        std::net::TcpListener::bind(("127.0.0.1", 0))
            .ok()
            .and_then(|l| l.local_addr().ok())
            .map(|a| a.port())
            .filter(|p| *p != crate::container_floor::CANARY_PORT)
    })
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
    // A seat container's only route out is the gateway; the floor records just the loopback
    // ports the seat serves itself (opencode's pinned server).
    let mut net: Vec<NetGrant> = Vec::new();
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
            trusted_git_dir: None,
        },
        env,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::process::Command;

    /// agy c6-r1: a signed-out agent's refusal is read as the SEAT's sign-in refusal, so the
    /// actor benches that seat and fails the unit over, as for any other "not signed in" seat.
    #[test]
    fn an_agent_signed_out_refusal_is_a_not_logged_in_seat_refusal() {
        let text = floor_refusal_text("claude", FloorUnarmed::AgentSignedOut);
        assert_eq!(
            wicked_council::types::SeatFailureReason::classify_refusal(&text, false),
            Some(wicked_council::types::SeatFailureReason::NotLoggedIn),
            "{text}"
        );
        assert!(text.contains("agent_signed_out") && text.contains("Agents"));
    }

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
    fn codex_is_never_wrapped_because_it_arms_its_own_sandbox() {
        assert!(seat_arms_its_own_os_sandbox("codex"));
        assert!(seat_arms_its_own_os_sandbox("codex-acp"));
        assert!(seat_arms_its_own_os_sandbox("/opt/bin/codex-acp"));
        assert!(!seat_arms_its_own_os_sandbox("claude"));
        assert!(!seat_arms_its_own_os_sandbox("claude-agent-acp"));
        assert!(!seat_arms_its_own_os_sandbox("opencode"));
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
            default_worker_sandbox(&own, &[], "claude").is_ok(),
            "{tool} runs on this host, so the repository boundary must arm"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn default_worker_sandbox_names_why_it_did_not_arm() {
        // IG1-core-1: a scratch dir is not a worktree; a codex seat arms its own sandbox (checked
        // first, so it holds even inside a run worktree).
        let dir = std::env::temp_dir().join(format!("wicked-wsb-reason-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(
            default_worker_sandbox(&dir, &[], "claude").err(),
            Some(FloorUnarmed::NotAWorktree)
        );
        let (base, _clone, own, _sibling) = clone_with_two_runs("reason");
        for seat in ["codex", "codex-acp", "/usr/local/bin/codex"] {
            assert_eq!(
                default_worker_sandbox(&own, &[], seat).err(),
                Some(FloorUnarmed::SeatArmsItsOwn),
                "{seat}"
            );
        }
        // In a worktree a non-codex seat either arms (naming its launcher) or names the launcher
        // gap — never NotAWorktree.
        match default_worker_sandbox(&own, &[], "claude") {
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
        let Ok(sandbox) = default_worker_sandbox(&own, &[], "claude").map(|a| a.sandbox) else {
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

    /// (IG1-core-3) The `sandboxPosture` prediction and each of its three failures: a seat off the
    /// floor (class `none`), a run not bound to a linked worktree, and (host-dependent) a launcher
    /// that does not arm; a bounded codex seat brings its own boundary.
    #[test]
    fn the_predicted_posture_names_which_of_the_three_failed() {
        let seat = |key: &str, floor: Option<bool>, flags: &[&str]| {
            let mut c = wicked_council::registry::builtin()
                .into_iter()
                .find(|c| c.key == key)
                .expect("a built-in seat");
            if let Some(acp) = c.acp.as_mut() {
                acp.governance_floor = floor;
                acp.os_sandbox = false;
            }
            c.trust_flags = flags.iter().map(|f| f.to_string()).collect();
            c
        };
        let (base, _clone, own, _sibling) = clone_with_two_runs("posture");
        // (core#563) pi is ACP-governed now; copilot is the built-in floor-class seat with an ACP record.
        let pi = seat("copilot", None, &[]);
        // 1. Off the floor.
        assert_eq!(
            predicted_posture(&seat("copilot", Some(false), &[]), Some(&own)),
            Err(PostureGap::NotOnTheFloor)
        );
        // 2. On the floor, not bound to a worktree (none, or a plain directory).
        assert_eq!(
            predicted_posture(&pi, None),
            Err(PostureGap::Unarmed(FloorUnarmed::NotAWorktree))
        );
        assert_eq!(
            predicted_posture(&pi, Some(&base)),
            Err(PostureGap::Unarmed(FloorUnarmed::NotAWorktree))
        );
        // 3. Bound: the host's launcher decides.
        assert_eq!(
            predicted_posture(&pi, Some(&own)),
            boundary_tool().map_err(PostureGap::Unarmed)
        );
        // A bounded codex seat brings its own sandbox.
        assert_eq!(
            predicted_posture(
                &seat("codex", None, &["--sandbox", "workspace-write"]),
                Some(&own)
            ),
            Ok(SEAT_CODEX_BOUNDARY)
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
