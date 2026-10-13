//! The container seat floor: a governed seat runs its CLI inside a hardened OCI container
//! (operator decision 2026-10-13 — a container runtime is REQUIRED for governed runs; the macOS
//! Seatbelt floor in [`crate::worker_sandbox`] is the labelled interim fallback until this path is
//! proven).
//!
//! **Glue, not a runtime.** The engine does not talk to a container API. It renders one wrapper
//! argv for the runtime's own CLI (`docker`, `podman` or `nerdctl`: one flag dialect), exactly as
//! it renders `sandbox-exec` / `bwrap` today, so the seat stays an ordinary child of the existing
//! spawn path: stdio streaming, the exit code, the process group, the deadline and the cancel kill
//! are unchanged. The one addition is the reap ([`reap`]): killing the client does not stop the
//! container, so the runner removes it by name.
//!
//! **What the container is.** One image per CLI, built from an in-repo Dockerfile on an OSS base
//! (`containers/seats/<cli>.Dockerfile`, tagged by the file's hash so an edit rebuilds); the
//! operator's uid (`--user`), `no-new-privileges`, every capability dropped, a read-only root
//! filesystem with a tmpfs `/tmp` and `HOME`, a pids limit, and an internal network
//! ([`SEAT_NETWORK`]) whose only egress is the model gateway. Every host path is bind-mounted at the
//! SAME absolute path, so the unit's cwd, its argv paths and its env paths need no translation.
//!
//! **The mounts are the floor.** They are rendered from the step-0 [`SeatFloor`] the interim floor
//! renders its profile from (one root computation, two renderers): the writable roots read-write,
//! the repository boundary read-only, the unit's own tree and git state read-write again, the
//! pinned launch files read-only, and every secret mask that falls under a mount covered (tmpfs
//! over a directory, `/dev/null` over a file). Nothing else of the host exists inside, so this is
//! a read jail as well as a write floor: a sibling worktree, the operator's other CLI homes and the
//! credential files are absent rather than denied. The unit's READ roots (its skills snapshot, the
//! repo graph's key dir, the launch's read extras — [`crate::gate_hook::READ_ROOTS_ENV`]) and the
//! absolute paths its argv names are mounted read-only at spawn ([`finalize`]).
//!
//! **Fail closed.** A governed unit whose container cannot start — no runtime, the engine not
//! answering, no seat image, native Windows — is refused with the reason (the failover ladder moves
//! the work), never run on the host.
//!
//! Opt-in until the shadow proof (slice C5): [`SEAT_FLOOR_ENV`]` = container`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::worker_sandbox::{FloorUnarmed, SeatFloor};

/// Selects the seat floor for governed units: `container` runs them in the container floor.
/// Anything else (unset included) keeps the step-0 host floor until slice C5 makes the container
/// floor the default.
pub(crate) const SEAT_FLOOR_ENV: &str = "WICKED_SEAT_FLOOR";
/// The runtime CLI to use (a name on `PATH` or an absolute path); otherwise the first of
/// `docker`, `podman`, `nerdctl` found on `PATH` or in a well-known install dir.
pub(crate) const RUNTIME_ENV: &str = "WICKED_CONTAINER_RUNTIME";
/// A Debian-based base image to build the seat images on instead of the default (a project's own
/// toolchain image). Part of the image tag, so a change rebuilds.
pub(crate) const BASE_IMAGE_ENV: &str = "WICKED_SEAT_BASE_IMAGE";
/// The network seats join (default [`SEAT_NETWORK`]); `none` for an offline seat.
pub(crate) const NETWORK_ENV: &str = "WICKED_SEAT_NETWORK";
/// The record label of a governed unit that ran in the container floor.
pub(crate) const CONTAINER_FLOOR: &str = "container floor";
/// The internal network every seat joins: no route out except the gateway attached to it.
pub(crate) const SEAT_NETWORK: &str = "wicked-seat-int";
/// The label on every seat container (and the seat network), for the reap and the sweep.
pub(crate) const SEAT_LABEL: &str = "wicked.seat";

const DEFAULT_BASE: &str = "node:22-trixie-slim";
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
const BUILD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const REAP_TIMEOUT: Duration = Duration::from_secs(20);

/// The seat Dockerfiles, by CLI (the registry's seat keys).
const DOCKERFILES: &[(&str, &str)] = &[
    ("claude", include_str!("../containers/seats/claude.Dockerfile")),
    ("codex", include_str!("../containers/seats/codex.Dockerfile")),
    ("pi", include_str!("../containers/seats/pi.Dockerfile")),
    (
        "opencode",
        include_str!("../containers/seats/opencode.Dockerfile"),
    ),
    (
        "copilot",
        include_str!("../containers/seats/copilot.Dockerfile"),
    ),
];

/// Whether governed units run in the container floor on this host ([`SEAT_FLOOR_ENV`]).
pub(crate) fn container_mode() -> bool {
    container_mode_from(std::env::var(SEAT_FLOOR_ENV).ok().as_deref())
}

pub(crate) fn container_mode_from(v: Option<&str>) -> bool {
    v.is_some_and(|v| v.trim().eq_ignore_ascii_case("container"))
}

/// A container runtime CLI that answered its probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Runtime {
    /// The CLI the engine runs (absolute when found by search).
    pub(crate) cli: PathBuf,
}

impl Runtime {
    fn cmd(&self) -> Command {
        let mut c = Command::new(&self.cli);
        c.stdin(Stdio::null());
        c
    }
}

/// The runtime CLIs to try, in order: the operator's choice, then `docker`, `podman`, `nerdctl`
/// on `PATH`, then the install dirs the common desktops use (the daemon's `PATH` often lacks them).
pub(crate) fn runtime_candidates(
    choice: Option<&str>,
    home: Option<&Path>,
    on_path: impl Fn(&str) -> Option<PathBuf>,
) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        if !out.contains(&p) {
            out.push(p);
        }
    };
    if let Some(c) = choice.map(str::trim).filter(|c| !c.is_empty()) {
        let p = Path::new(c);
        if p.is_absolute() {
            push(p.to_path_buf());
        } else if let Some(found) = on_path(c) {
            push(found);
        }
        // An explicit choice is the only candidate: never silently swap the operator's runtime.
        return out;
    }
    for name in ["docker", "podman", "nerdctl"] {
        if let Some(found) = on_path(name) {
            push(found);
        }
    }
    if let Some(h) = home {
        for rel in [".rd/bin/docker", ".orbstack/bin/docker", ".colima/bin/docker"] {
            let p = h.join(rel);
            if p.is_file() {
                push(p);
            }
        }
    }
    for abs in [
        "/usr/local/bin/docker",
        "/opt/homebrew/bin/docker",
        "/opt/homebrew/bin/podman",
        "/Applications/Docker.app/Contents/Resources/bin/docker",
    ] {
        let p = PathBuf::from(abs);
        if p.is_file() {
            push(p);
        }
    }
    out
}

/// Run `cmd` to completion within `limit`, killing it past the deadline. `None` when it could not
/// start or ran out of time.
fn output_within(mut cmd: Command, limit: Duration) -> Option<std::process::Output> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) if start.elapsed() < limit => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// The runtime this host runs seats with, probed (`<cli> version` must reach the engine). A
/// success is cached for the process; a failure is not, so starting the runtime later is enough.
pub(crate) fn runtime() -> Result<Runtime, FloorUnarmed> {
    static OK: Mutex<Option<Runtime>> = Mutex::new(None);
    if let Some(rt) = OK.lock().ok().and_then(|g| g.clone()) {
        return Ok(rt);
    }
    if cfg!(windows) {
        return Err(FloorUnarmed::ContainerUnsupportedHost);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let cands = runtime_candidates(
        std::env::var(RUNTIME_ENV).ok().as_deref(),
        home.as_deref(),
        crate::validator::find_on_path,
    );
    if cands.is_empty() {
        return Err(FloorUnarmed::NoContainerRuntime);
    }
    for cli in cands {
        let rt = Runtime { cli };
        let mut probe = rt.cmd();
        probe.args(["version", "--format", "{{.Server.Version}}"]);
        if output_within(probe, PROBE_TIMEOUT).is_some_and(|o| o.status.success()) {
            if let Ok(mut g) = OK.lock() {
                *g = Some(rt.clone());
            }
            return Ok(rt);
        }
    }
    Err(FloorUnarmed::ContainerRuntimeDown)
}

/// The seat Dockerfile for `cli`, when the engine ships one.
pub(crate) fn dockerfile(cli: &str) -> Option<&'static str> {
    DOCKERFILES
        .iter()
        .find(|(k, _)| *k == cli)
        .map(|(_, d)| *d)
}

/// The CLIs the engine ships a seat image for.
pub(crate) fn seat_clis() -> impl Iterator<Item = &'static str> {
    DOCKERFILES.iter().map(|(k, _)| *k)
}

/// The base image the seat images build on ([`BASE_IMAGE_ENV`], else the default).
fn base_image() -> String {
    std::env::var(BASE_IMAGE_ENV)
        .ok()
        .map(|b| b.trim().to_string())
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| DEFAULT_BASE.to_string())
}

/// The tag of `cli`'s seat image built on `base`: `wicked-seat-<cli>:<12 hex of sha256(Dockerfile,
/// base)>`, so an edited Dockerfile or another base is a new image, never a stale one.
pub(crate) fn seat_image_tag(cli: &str, base: &str) -> Option<String> {
    use sha2::{Digest, Sha256};
    let df = dockerfile(cli)?;
    let mut h = Sha256::new();
    h.update(df.as_bytes());
    h.update([0u8]);
    h.update(base.as_bytes());
    let hex: String = h.finalize()[..6]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Some(format!("wicked-seat-{cli}:{hex}"))
}

/// The operator's own image for `cli` (`WICKED_SEAT_IMAGE_<CLI>`), used as-is.
fn image_override(cli: &str) -> Option<String> {
    let key = format!("WICKED_SEAT_IMAGE_{}", cli.to_ascii_uppercase());
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn image_present(rt: &Runtime, tag: &str) -> bool {
    let mut c = rt.cmd();
    c.args(["image", "inspect", "--format", "{{.Id}}", tag]);
    output_within(c, PROBE_TIMEOUT).is_some_and(|o| o.status.success())
}

/// Build `cli`'s seat image from its Dockerfile alone (no build context), streaming the
/// runtime's progress to the engine's stderr. `Err` carries the runtime's last words.
pub(crate) fn build_seat_image(rt: &Runtime, cli: &str) -> Result<String, String> {
    let df = dockerfile(cli).ok_or_else(|| format!("no seat image is defined for `{cli}`"))?;
    let base = base_image();
    let tag = seat_image_tag(cli, &base).expect("dockerfile exists");
    let mut c = rt.cmd();
    c.args([
        "build",
        "--label",
        &format!("{SEAT_LABEL}=1"),
        "--build-arg",
        &format!("BASE={base}"),
        "-t",
        &tag,
        "-",
    ]);
    c.stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped());
    let mut child = c
        .spawn()
        .map_err(|e| format!("could not start `{} build`: {e}", rt.cli.display()))?;
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        let _ = stdin.write_all(df.as_bytes());
    }
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if start.elapsed() < BUILD_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(200))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("building {tag} ran past {BUILD_TIMEOUT:?}"));
            }
        }
    };
    let mut err = String::new();
    if let Some(mut se) = child.stderr.take() {
        use std::io::Read;
        let _ = se.read_to_string(&mut err);
    }
    eprint!("{err}");
    if status.success() {
        Ok(tag)
    } else {
        let tail: Vec<&str> = err.lines().rev().take(6).collect();
        Err(format!(
            "building {tag} failed ({status}): {}",
            tail.into_iter().rev().collect::<Vec<_>>().join(" | ")
        ))
    }
}

/// The image `cli`'s seat runs: the operator's override as-is, else the engine's seat image,
/// built on first use (one build at a time per process).
pub(crate) fn ensure_seat_image(rt: &Runtime, cli: &str) -> Result<String, String> {
    if let Some(img) = image_override(cli) {
        return Ok(img);
    }
    static BUILD: Mutex<()> = Mutex::new(());
    let tag = seat_image_tag(cli, &base_image())
        .ok_or_else(|| format!("no seat image is defined for `{cli}`"))?;
    if image_present(rt, &tag) {
        return Ok(tag);
    }
    let _one = BUILD.lock();
    if image_present(rt, &tag) {
        return Ok(tag);
    }
    eprintln!("wicked-core: building seat image {tag} (first use)");
    build_seat_image(rt, cli)
}

/// The network a seat joins ([`NETWORK_ENV`], else [`SEAT_NETWORK`]), created internal on first
/// use. `none` and the runtime's built-ins are used as named.
pub(crate) fn ensure_seat_network(rt: &Runtime) -> Result<String, String> {
    let name = std::env::var(NETWORK_ENV)
        .ok()
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| SEAT_NETWORK.to_string());
    if matches!(name.as_str(), "none" | "host" | "bridge") {
        if name == "host" {
            return Err("the host network is never a seat network".into());
        }
        return Ok(name);
    }
    let mut inspect = rt.cmd();
    inspect.args(["network", "inspect", "--format", "{{.Internal}}", &name]);
    if let Some(o) = output_within(inspect, PROBE_TIMEOUT) {
        if o.status.success() {
            return if String::from_utf8_lossy(&o.stdout).trim() == "true" {
                Ok(name)
            } else {
                Err(format!(
                    "network `{name}` exists but is not internal; a seat network must have no \
                     route out except the gateway"
                ))
            };
        }
    }
    let mut create = rt.cmd();
    create.args([
        "network",
        "create",
        "--internal",
        "--label",
        &format!("{SEAT_LABEL}=1"),
        &name,
    ]);
    match output_within(create, PROBE_TIMEOUT) {
        Some(o) if o.status.success() => Ok(name),
        Some(o) => Err(format!(
            "could not create network `{name}`: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        None => Err(format!("creating network `{name}` timed out")),
    }
}

/// One mount of the container's filesystem, rendered as a `--mount` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Mount {
    /// `src` at the same path; `rw` or read-only.
    Bind { path: PathBuf, rw: bool },
    /// The host's `/dev/null` (the runtime VM's) over a file: a masked file reads empty.
    NullFile { path: PathBuf },
    /// An empty tmpfs over a directory: a masked dir is empty.
    Tmpfs { path: PathBuf },
}

impl Mount {
    fn dst(&self) -> &Path {
        match self {
            Mount::Bind { path, .. } | Mount::NullFile { path } | Mount::Tmpfs { path } => path,
        }
    }

    fn render(&self) -> String {
        // `--mount` values are comma-separated key=value pairs: a path with a comma or a quote
        // would split, so such a path is refused by [`mount_plan`] before it gets here.
        match self {
            Mount::Bind { path, rw } => format!(
                "type=bind,src={p},dst={p}{}",
                if *rw { "" } else { ",readonly" },
                p = path.display()
            ),
            Mount::NullFile { path } => {
                format!("type=bind,src=/dev/null,dst={},readonly", path.display())
            }
            Mount::Tmpfs { path } => format!("type=tmpfs,dst={},tmpfs-size=1m", path.display()),
        }
    }
}

/// A path a `--mount` value can carry: absolute, UTF-8, no comma, quote or newline.
fn mountable(p: &Path) -> bool {
    p.is_absolute()
        && p
            .to_str()
            .is_some_and(|s| !s.contains([',', '"', '\'', '\n', '=']))
}

/// The mounts that render `f`, in the floor's order (later wins at one path): the writable roots
/// read-write, the repository boundary read-only, the unit's tree and git state read-write, the
/// single re-admitted files read-write, the pinned launch files read-only, then each existing
/// mask that lies under a mount covered. A path that does not exist is left out (a bind needs a
/// source); `Err` names a path a mount cannot carry.
pub(crate) fn mount_plan(f: &SeatFloor) -> Result<Vec<Mount>, String> {
    fn put(out: &mut Vec<Mount>, m: Mount) -> Result<(), String> {
        if !mountable(m.dst()) {
            return Err(format!(
                "the path {} cannot be mounted into a seat container (a comma, quote, `=` or \
                 newline in it)",
                m.dst().display()
            ));
        }
        out.retain(|o| o.dst() != m.dst());
        out.push(m);
        Ok(())
    }
    let mut out: Vec<Mount> = Vec::new();
    let bind = |path: &PathBuf, rw: bool| Mount::Bind {
        path: path.clone(),
        rw,
    };
    for d in f.writable.iter().filter(|d| d.is_dir()) {
        put(&mut out, bind(d, true))?;
    }
    for d in f.protected.iter().filter(|d| d.is_dir()) {
        put(&mut out, bind(d, false))?;
    }
    for d in f.readmitted.iter().filter(|d| d.is_dir()) {
        put(&mut out, bind(d, true))?;
    }
    for p in f.readmitted_files.iter().filter(|p| p.is_file()) {
        put(&mut out, bind(p, true))?;
    }
    for p in f.pinned.iter().filter(|p| p.is_file()) {
        put(&mut out, bind(p, false))?;
    }
    let masks: Vec<Mount> = f
        .masks
        .iter()
        .filter(|m| {
            out.iter()
                .any(|o| matches!(o, Mount::Bind { .. }) && m.starts_with(o.dst()))
        })
        .filter_map(|m| {
            if m.is_dir() {
                Some(Mount::Tmpfs { path: m.clone() })
            } else if m.is_file() {
                Some(Mount::NullFile { path: m.clone() })
            } else {
                None
            }
        })
        .collect();
    for m in masks {
        put(&mut out, m)?;
    }
    Ok(out)
}

/// The facts one seat container is launched with.
#[derive(Debug, Clone)]
pub(crate) struct ContainerLaunch<'a> {
    pub(crate) runtime: &'a Path,
    pub(crate) image: &'a str,
    pub(crate) name: &'a str,
    pub(crate) network: &'a str,
    pub(crate) workdir: &'a Path,
    /// `uid:gid` the seat runs as (the operator's, so its writes land owned by them).
    pub(crate) user: &'a str,
    pub(crate) mounts: &'a [Mount],
}

/// The options of ours that take a value, and those that do not: [`finalize`] walks the wrapper
/// with exactly this table to find where the image (and so the CLI argv) begins.
const VALUE_OPTS: &[&str] = &[
    "--name",
    "--label",
    "--user",
    "--tmpfs",
    "--cap-drop",
    "--security-opt",
    "--pids-limit",
    "--network",
    "--workdir",
    "--mount",
    "--env",
];
const FLAG_OPTS: &[&str] = &["--rm", "--interactive", "--init", "--read-only"];

/// The wrapper argv that runs a seat in a container, ending with the image: the CLI argv follows
/// it. Pure (no runtime call), so the hardening is pinned by tests.
pub(crate) fn run_argv(l: &ContainerLaunch<'_>) -> Vec<String> {
    let mut w: Vec<String> = vec![
        l.runtime.to_string_lossy().into_owned(),
        "run".into(),
        "--rm".into(),
        "--interactive".into(),
        "--init".into(),
        "--name".into(),
        l.name.into(),
        "--label".into(),
        format!("{SEAT_LABEL}=1"),
        "--user".into(),
        l.user.into(),
        "--read-only".into(),
        "--tmpfs".into(),
        "/tmp:rw,mode=1777,size=2g".into(),
        "--tmpfs".into(),
        "/home/seat:rw,mode=1777,size=512m".into(),
        "--cap-drop".into(),
        "ALL".into(),
        "--security-opt".into(),
        "no-new-privileges".into(),
        "--pids-limit".into(),
        "4096".into(),
        "--network".into(),
        l.network.into(),
        "--workdir".into(),
        l.workdir.to_string_lossy().into_owned(),
        "--env".into(),
        "HOME=/home/seat".into(),
        "--env".into(),
        "TMPDIR=/tmp".into(),
        "--env".into(),
        "LANG=C.UTF-8".into(),
    ];
    for m in l.mounts {
        w.push("--mount".into());
        w.push(m.render());
    }
    w.push(l.image.into());
    w
}

/// The index of the image in a wrapper [`run_argv`] rendered (the first argument our option
/// table does not consume), or `None` when `args` is not one of ours.
fn image_index(args: &[String]) -> Option<usize> {
    if args.first().map(String::as_str) != Some("run") {
        return None;
    }
    let mut i = 1;
    let mut ours = false;
    while i < args.len() {
        let a = args[i].as_str();
        if VALUE_OPTS.contains(&a) {
            if a == "--label" && args.get(i + 1).map(String::as_str) == Some(&format!("{SEAT_LABEL}=1"))
            {
                ours = true;
            }
            i += 2;
        } else if FLAG_OPTS.contains(&a) {
            i += 1;
        } else {
            return ours.then_some(i);
        }
    }
    None
}

/// The container name of a seat `cmd` (one [`run_argv`] rendered), for the reap.
pub(crate) fn seat_container_name(cmd: &Command) -> Option<String> {
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let img = image_index(&args)?;
    args[..img]
        .windows(2)
        .find(|w| w[0] == "--name")
        .map(|w| w[1].clone())
}

/// Environment variables never passed into a seat: the image's own `PATH` (the host's points at
/// host binaries) and the host's temp / home spelling the container replaces.
const HOST_ONLY_ENV: &[&str] = &["PATH", "PWD", "OLDPWD", "SHELL", "DYLD_LIBRARY_PATH", "DYLD_INSERT_LIBRARIES"];

/// Make a seat `cmd` (one [`run_argv`] rendered, then given its env by the carrier) complete:
/// every variable the carrier set explicitly is passed in by NAME (`--env KEY`: the runtime CLI
/// reads the value from its own environment, so no value is ever on a command line), and the
/// unit's read roots plus the absolute paths its argv names are mounted read-only when no mount
/// covers them yet. Any other command is returned unchanged.
pub(crate) fn finalize(cmd: Command) -> Command {
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let Some(img) = image_index(&args) else {
        return cmd;
    };
    let mut existing: Vec<PathBuf> = Vec::new();
    for w in args[..img].windows(2) {
        if w[0] == "--mount" {
            if let Some(dst) = w[1].split(',').find_map(|kv| kv.strip_prefix("dst=")) {
                existing.push(PathBuf::from(dst));
            }
        }
    }
    let covered = |p: &Path, existing: &[PathBuf]| existing.iter().any(|m| p.starts_with(m));
    let mut extra: Vec<String> = Vec::new();
    let mut envs: Vec<(String, Option<std::ffi::OsString>)> = Vec::new();
    let mut ro: Vec<PathBuf> = Vec::new();
    for (k, v) in cmd.get_envs() {
        let key = k.to_string_lossy().into_owned();
        envs.push((key.clone(), v.map(|v| v.to_os_string())));
        let Some(v) = v else { continue };
        if key == crate::gate_hook::READ_ROOTS_ENV {
            ro.extend(std::env::split_paths(v));
        }
        if !HOST_ONLY_ENV.contains(&key.as_str()) {
            extra.push("--env".into());
            extra.push(key);
        }
    }
    // Absolute paths the CLI argv names (`--settings <file>`, `--plugin-dir <dir>`,
    // `--mcp-config=<file>`): an option's value, never a free-standing word (a prompt).
    let tail = &args[img + 1..];
    for (i, a) in tail.iter().enumerate() {
        let after_flag = i > 0 && tail[i - 1].starts_with('-');
        let value = match a.split_once('=') {
            Some((flag, v)) if flag.starts_with("--") => Some(v),
            _ if after_flag => Some(a.as_str()),
            _ => None,
        };
        if let Some(v) = value {
            let p = Path::new(v);
            if p.is_absolute() && p.exists() {
                ro.push(p.to_path_buf());
            }
        }
    }
    for p in ro {
        let Some(p) = crate::worker_sandbox::canon_lenient(&p) else {
            continue;
        };
        if !p.exists() || covered(&p, &existing) || !mountable(&p) {
            continue;
        }
        extra.push("--mount".into());
        extra.push(
            Mount::Bind {
                path: p.clone(),
                rw: false,
            }
            .render(),
        );
        existing.push(p);
    }
    let mut out = Command::new(cmd.get_program());
    out.args(&args[..img]);
    out.args(&extra);
    out.args(&args[img..]);
    if let Some(dir) = cmd.get_current_dir() {
        out.current_dir(dir);
    }
    for (k, v) in envs {
        match v {
            Some(v) => {
                out.env(k, v);
            }
            None => {
                out.env_remove(k);
            }
        }
    }
    out
}

/// Remove seat container `name` (after its client exited or was killed: a killed client leaves
/// the container running). Best effort and bounded; `--rm` already removed a container whose
/// client saw it exit.
pub(crate) fn reap(name: &str) {
    let Ok(rt) = runtime() else { return };
    let mut c = rt.cmd();
    c.args(["rm", "--force", name]);
    let _ = output_within(c, REAP_TIMEOUT);
}

/// The CLI argv rewritten for the image: the host's resolved binary path (`argv[0]`, and every
/// later argument spelling the same path, as the opencode pinned-server script does) becomes the
/// CLI's name on the image's `PATH`.
pub(crate) fn image_argv(argv: &[String], cli: &str) -> Vec<String> {
    let Some(host_bin) = argv.first().cloned() else {
        return Vec::new();
    };
    argv.iter()
        .enumerate()
        .map(|(i, a)| {
            if i == 0 || (*a == host_bin && Path::new(a).is_absolute()) {
                cli.to_string()
            } else {
                a.clone()
            }
        })
        .collect()
}

/// The operator's `uid:gid` (the seat's writes land owned by them).
fn operator_user() -> String {
    #[cfg(unix)]
    {
        // SAFETY: getuid/getgid have no preconditions and cannot fail.
        let (u, g) = unsafe { (libc::getuid(), libc::getgid()) };
        format!("{u}:{g}")
    }
    #[cfg(not(unix))]
    {
        "1000:1000".to_string()
    }
}

/// A seat armed in the container floor: the wrapper, the argv to run after it, the runtime that
/// armed it (by file name: `docker`, `podman`, `nerdctl`) and the floor it renders.
#[derive(Debug)]
pub(crate) struct ArmedContainer {
    pub(crate) wrapper: Vec<String>,
    pub(crate) argv: Vec<String>,
    pub(crate) tool: &'static str,
    pub(crate) spec: SeatFloor,
}

fn tool_name(cli: &Path) -> &'static str {
    match cli.file_stem().and_then(|s| s.to_str()) {
        Some("podman") => "podman",
        Some("nerdctl") => "nerdctl",
        _ => "docker",
    }
}

/// A container name for one spawn: `wicked-seat-<cli>-<run id prefix>-<ord>-<nonce>`, limited to
/// the characters every runtime accepts.
pub(crate) fn container_name(cli: &str, run_id: &str, ord: u32) -> String {
    let clean = |s: &str, n: usize| -> String {
        s.chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(n)
            .collect::<String>()
            .to_ascii_lowercase()
    };
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    format!(
        "wicked-seat-{}-{}-{ord}-{}",
        clean(cli, 12),
        clean(run_id, 8),
        &nonce[..8]
    )
}

/// Arm the container floor for one governed seat spawn: resolve the floor (the step-0
/// computation), find the runtime, the seat image and the network, and render the wrapper. `Err`
/// names why the unit cannot run in a container; the caller refuses it (fail closed).
pub(crate) fn arm(
    inp: &crate::worker_sandbox::FloorInputs,
    argv: &[String],
    run_id: &str,
    ord: u32,
) -> Result<ArmedContainer, (FloorUnarmed, String)> {
    let cli = argv
        .first()
        .map(|b| wicked_apps_core::spawn::SeatCli::from_binary(b))
        .and_then(|c| c.root_name())
        .filter(|c| dockerfile(c).is_some() || image_override(c).is_some())
        .ok_or_else(|| {
            (
                FloorUnarmed::NoSeatImage,
                format!(
                    "no seat image is defined for `{}`",
                    argv.first().map(String::as_str).unwrap_or("")
                ),
            )
        })?;
    let spec = crate::worker_sandbox::resolve_seat_floor(inp).map_err(|w| (w, String::new()))?;
    let rt = runtime().map_err(|w| (w, String::new()))?;
    let image = ensure_seat_image(&rt, cli).map_err(|e| (FloorUnarmed::NoSeatImage, e))?;
    let network = ensure_seat_network(&rt).map_err(|e| (FloorUnarmed::ContainerRuntimeDown, e))?;
    let mounts = mount_plan(&spec).map_err(|e| (FloorUnarmed::RootUnresolvable, e))?;
    let name = container_name(cli, run_id, ord);
    let user = operator_user();
    let workdir = inp.cwd.canonicalize().unwrap_or_else(|_| inp.cwd.clone());
    let wrapper = run_argv(&ContainerLaunch {
        runtime: &rt.cli,
        image: &image,
        name: &name,
        network: &network,
        workdir: &workdir,
        user: &user,
        mounts: &mounts,
    });
    Ok(ArmedContainer {
        wrapper,
        argv: image_argv(argv, cli),
        tool: tool_name(&rt.cli),
        spec,
    })
}

/// `wicked-core seat-image build [<cli>…|all]` / `seat-image status`: build the seat images
/// ahead of the first governed run, or report which are present. Returns the exit code.
pub fn seat_image_main(args: &[String]) -> i32 {
    let rt = match runtime() {
        Ok(rt) => rt,
        Err(why) => {
            eprintln!("wicked-core seat-image: {}", why.describe());
            return 1;
        }
    };
    let wanted: Vec<String> = match args.get(1..) {
        Some(rest) if !rest.is_empty() && !rest.iter().any(|a| a == "all") => rest.to_vec(),
        _ => seat_clis().map(str::to_string).collect(),
    };
    match args.first().map(String::as_str) {
        Some("build") => {
            let mut rc = 0;
            for cli in &wanted {
                match build_seat_image(&rt, cli) {
                    Ok(tag) => println!("{cli}: built {tag}"),
                    Err(e) => {
                        eprintln!("{cli}: {e}");
                        rc = 1;
                    }
                }
            }
            rc
        }
        Some("status") => {
            println!("runtime: {}", rt.cli.display());
            for cli in &wanted {
                let tag = image_override(cli)
                    .or_else(|| seat_image_tag(cli, &base_image()))
                    .unwrap_or_else(|| "(no image defined)".into());
                let present = image_present(&rt, &tag);
                println!("{cli}: {tag} {}", if present { "present" } else { "missing" });
            }
            0
        }
        _ => {
            eprintln!("usage: wicked-core seat-image build [<cli>…|all] | seat-image status");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wicked-cf-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    #[test]
    fn container_mode_is_an_explicit_opt_in() {
        assert!(container_mode_from(Some("container")));
        assert!(container_mode_from(Some(" Container ")));
        assert!(!container_mode_from(None));
        assert!(!container_mode_from(Some("")));
        assert!(!container_mode_from(Some("host")));
    }

    #[test]
    fn an_explicit_runtime_choice_is_the_only_candidate() {
        let found = |n: &str| Some(PathBuf::from(format!("/bin/{n}")));
        assert_eq!(
            runtime_candidates(Some("podman"), None, found),
            vec![PathBuf::from("/bin/podman")]
        );
        assert_eq!(
            runtime_candidates(Some("/opt/x/nerdctl"), None, |_| None),
            vec![PathBuf::from("/opt/x/nerdctl")]
        );
        // Not found: no silent fallback to another runtime.
        assert!(runtime_candidates(Some("podman"), None, |_| None).is_empty());
        let all = runtime_candidates(None, None, found);
        assert_eq!(
            &all[..3],
            &[
                PathBuf::from("/bin/docker"),
                PathBuf::from("/bin/podman"),
                PathBuf::from("/bin/nerdctl")
            ]
        );
    }

    #[test]
    fn every_seat_cli_has_a_dockerfile_on_an_oss_base_and_a_stable_hashed_tag() {
        for cli in ["claude", "codex", "pi", "opencode", "copilot"] {
            let df = dockerfile(cli).unwrap_or_else(|| panic!("{cli} has a Dockerfile"));
            assert!(df.contains("FROM ${BASE}"), "{cli}");
            assert!(df.contains(&format!("wicked.seat.cli={cli}")), "{cli}");
            let a = seat_image_tag(cli, DEFAULT_BASE).unwrap();
            assert_eq!(a, seat_image_tag(cli, DEFAULT_BASE).unwrap());
            assert!(a.starts_with(&format!("wicked-seat-{cli}:")), "{a}");
            assert_ne!(a, seat_image_tag(cli, "debian:trixie").unwrap(), "base is in the tag");
        }
        assert!(dockerfile("agy").is_none());
    }

    #[test]
    fn the_run_argv_is_hardened_and_ends_with_the_image() {
        let mounts = vec![Mount::Bind {
            path: PathBuf::from("/w/tree"),
            rw: true,
        }];
        let w = run_argv(&ContainerLaunch {
            runtime: Path::new("/usr/bin/docker"),
            image: "wicked-seat-claude:abc",
            name: "wicked-seat-claude-r-1-n",
            network: SEAT_NETWORK,
            workdir: Path::new("/w/tree"),
            user: "502:20",
            mounts: &mounts,
        });
        let has = |pair: &[&str]| w.windows(pair.len()).any(|x| x == pair);
        assert_eq!(w[0], "/usr/bin/docker");
        assert!(has(&["--user", "502:20"]));
        assert!(has(&["--cap-drop", "ALL"]));
        assert!(has(&["--security-opt", "no-new-privileges"]));
        assert!(has(&["--network", SEAT_NETWORK]));
        assert!(has(&["--mount", "type=bind,src=/w/tree,dst=/w/tree"]));
        assert!(w.contains(&"--read-only".to_string()));
        assert!(!w.iter().any(|a| a == "--privileged" || a.starts_with("--cap-add")));
        assert_eq!(w.last().unwrap(), "wicked-seat-claude:abc");
        assert_eq!(image_index(&w[1..]), Some(w.len() - 2));
    }

    #[test]
    fn the_mounts_render_the_seat_floor_in_order() {
        let base = tmp("plan");
        let clone = base.join("clone");
        let tree = base.join("wt");
        let home = base.join("home");
        let secret = home.join(".ssh");
        for d in [&clone, &tree, &home, &secret] {
            std::fs::create_dir_all(d).unwrap();
        }
        let pinned = home.join("settings.json");
        std::fs::write(&pinned, "{}").unwrap();
        let spec = SeatFloor {
            profile: "governed".into(),
            tree: tree.clone(),
            writable: vec![tree.clone(), home.clone(), base.join("missing")],
            protected: vec![clone.clone()],
            readmitted: vec![],
            readmitted_files: vec![],
            masks: vec![secret.clone(), base.join("not-mounted-anywhere")],
            pinned: vec![pinned.clone()],
            net: vec![],
        };
        let plan = mount_plan(&spec).unwrap();
        assert_eq!(
            plan,
            vec![
                Mount::Bind {
                    path: tree.clone(),
                    rw: true
                },
                Mount::Bind {
                    path: home.clone(),
                    rw: true
                },
                Mount::Bind {
                    path: clone.clone(),
                    rw: false
                },
                Mount::Bind {
                    path: pinned.clone(),
                    rw: false
                },
                Mount::Tmpfs {
                    path: secret.clone()
                },
            ]
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_path_a_mount_value_cannot_carry_is_refused() {
        let base = tmp("comma");
        let bad = base.join("a,b");
        std::fs::create_dir_all(&bad).unwrap();
        let spec = SeatFloor {
            profile: "governed".into(),
            tree: bad.clone(),
            writable: vec![bad],
            protected: vec![],
            readmitted: vec![],
            readmitted_files: vec![],
            masks: vec![],
            pinned: vec![],
            net: vec![],
        };
        assert!(mount_plan(&spec).unwrap_err().contains("cannot be mounted"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_image_argv_names_the_cli_on_the_image_path() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            image_argv(&s(&["/opt/homebrew/bin/claude", "-p", "x"]), "claude"),
            s(&["claude", "-p", "x"])
        );
        // The opencode pinned-server script names the host binary again as `$1`.
        assert_eq!(
            image_argv(
                &s(&["/opt/oc/opencode", "-c", "script", "n", "/opt/oc/opencode", "4096"]),
                "opencode"
            ),
            s(&["opencode", "-c", "script", "n", "opencode", "4096"])
        );
    }

    #[test]
    fn finalize_passes_env_by_name_and_mounts_read_roots_and_argv_paths() {
        let base = tmp("fin");
        let tree = base.join("wt");
        let skills = base.join("skills");
        let settings = base.join("gov").join("settings.json");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::create_dir_all(&skills).unwrap();
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        std::fs::write(&settings, "{}").unwrap();
        let mounts = vec![Mount::Bind {
            path: tree.clone(),
            rw: true,
        }];
        let w = run_argv(&ContainerLaunch {
            runtime: Path::new("docker"),
            image: "img:1",
            name: "wicked-seat-x",
            network: "none",
            workdir: &tree,
            user: "1:1",
            mounts: &mounts,
        });
        let mut cmd = Command::new(&w[0]);
        cmd.args(&w[1..]);
        cmd.args([
            "claude",
            "-p",
            "/etc/passwd",
            "--settings",
            settings.to_str().unwrap(),
        ]);
        cmd.env("SECRET_TOKEN", "value-never-on-argv");
        cmd.env("PATH", "/host/bin");
        cmd.env(crate::gate_hook::READ_ROOTS_ENV, &skills);
        cmd.env(
            crate::gate_hook::WRITE_ROOTS_ENV,
            tree.join("sub").as_os_str(),
        );
        let fin = finalize(cmd);
        let args: Vec<String> = fin
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let has = |pair: &[&str]| args.windows(2).any(|x| x == pair);
        assert!(has(&["--env", "SECRET_TOKEN"]));
        assert!(!args.iter().any(|a| a.contains("value-never-on-argv")));
        assert!(!has(&["--env", "PATH"]), "the image keeps its own PATH");
        let ro = |p: &Path| format!("type=bind,src={p},dst={p},readonly", p = p.display());
        assert!(has(&["--mount", &ro(&skills)]), "{args:?}");
        assert!(has(&["--mount", &ro(&settings)]), "{args:?}");
        // A prompt word that happens to be a path is not a mount.
        assert!(!args.iter().any(|a| a.contains("dst=/etc/passwd")));
        // The CLI argv still follows the image, untouched.
        let img = args.iter().position(|a| a == "img:1").unwrap();
        assert_eq!(args[img + 1], "claude");
        assert_eq!(seat_container_name(&fin).as_deref(), Some("wicked-seat-x"));
        // Not one of ours: unchanged.
        let mut plain = Command::new("claude");
        plain.args(["run", "--name", "x"]);
        assert!(seat_container_name(&plain).is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn container_names_are_unique_and_runtime_safe() {
        let a = container_name("claude", "run/../ID:1", 3);
        let b = container_name("claude", "run/../ID:1", 3);
        assert_ne!(a, b);
        assert!(a.starts_with("wicked-seat-claude-runid1-3-"), "{a}");
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }
}
