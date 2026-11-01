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
//! filesystem with a tmpfs `/tmp` and `HOME`, a pids limit, and an internal network of its own
//! (one per seat container, named after it: seats sharing a network could reach each other) whose
//! only egress is the model gateway attached to it. Every host path is bind-mounted at the
//! SAME absolute path, so the unit's cwd, its argv paths and its env paths need no translation.
//! The `HOME` tmpfs (512 MB) holds no package cache: cargo's home and the npm / pip / go caches
//! are the seat's persistent cache dir on the host ([`crate::worker_sandbox::seat_cache_dir`],
//! keyed by toolchain in [`crate::worker_sandbox::SEAT_TOOLCHAIN_CACHES`]), bound read-write like
//! any seat dir, with each variable passed by name to the runtime and to landrun ([`finalize`]).
//! The toolchains' build settings ride the same way, from the table beside it
//! ([`crate::worker_sandbox::SEAT_TOOLCHAIN_BUILD_ENV`]): cargo runs one job per runtime CPU
//! ([`seat_cpus`]) up to two, with no debuginfo for the dev and test profiles and no incremental
//! state, so a cargo repository's tests build within a seat's memory limit; a variable the operator
//! sets in the daemon's environment is passed with the operator's value instead.
//!
//! **The mounts are the floor.** They are rendered from the step-0 [`SeatFloor`] the interim floor
//! renders its profile from (one root computation, two renderers): the writable roots read-write,
//! the repository boundary read-only, the unit's own tree and git state read-write again, the
//! pinned launch files read-only, and every secret mask that falls under a mount covered (tmpfs
//! over a directory, `/dev/null` over a file). Nothing else of the host exists inside, so this is
//! a read jail as well as a write floor: a sibling worktree, the operator's other CLI homes and the
//! credential files are absent rather than denied. The unit's READ roots (its skills snapshot, the
//! repo graph's key dir, the launch's read extras — [`crate::gate_hook::READ_ROOTS_ENV`]) and the
//! absolute paths its argv names are mounted read-only when the floor arms ([`add_read_roots`],
//! [`mount_argv_paths`]); [`finalize`] adds no mount.
//!
//! **Landlock inside the seat.** The image's entry requires a TCP-capable Landlock ABI and checks
//! a writable canary tmpfs before entering `landrun`, then requires an out-of-tree write to fail
//! inside the CLI's own domain.
//! Landlock admits writes and execution in the writable mounts, `/home/seat` and `/tmp`, reads
//! and execution in the image and read mounts, and TCP connects only to the gateway ports.
//! A failed canary refuses the CLI even when `--best-effort` degraded to no enforcement.
//!
//! **Fail closed.** A governed unit whose container cannot start — no runtime, the engine not
//! answering, no seat image, native Windows — is refused with the reason (the failover ladder moves
//! the work), never run on the host.
//!
//! Opt-in until the shadow proof (slice C5): [`SEAT_FLOOR_ENV`]` = container`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, Once};
use std::time::{Duration, Instant};

use crate::worker_sandbox::{FloorUnarmed, SeatFloor};
use wicked_apps_core::HardenedCommand;

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
/// The Dockerfile [`BASE_IMAGE_ENV`]'s image is built from (no build context), for a base that
/// exists only on this runtime: when the base is missing (a runtime's image cleanup deleted it),
/// the engine rebuilds it from this file before it builds a seat image on it.
pub(crate) const BASE_DOCKERFILE_ENV: &str = "WICKED_SEAT_BASE_DOCKERFILE";
/// `none` runs seats with no network at all; otherwise each seat gets its own internal network.
pub(crate) const NETWORK_ENV: &str = "WICKED_SEAT_NETWORK";
/// The container's memory limit, otherwise `3g`.
pub(crate) const MEMORY_ENV: &str = "WICKED_SEAT_MEMORY";
/// The label on every seat container (and the seat network), for the reap and the sweep.
pub(crate) const SEAT_LABEL: &str = "wicked.seat";

const DEFAULT_BASE: &str = "node:22-trixie-slim";
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
const BUILD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const REAP_TIMEOUT: Duration = Duration::from_secs(20);
const SWEEP_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// The gateway's listeners a seat may connect to: CONNECT 3000, Anthropic 4000, OpenAI 4001,
/// ChatGPT 4002, pi 4003, opencode 4004, Claude OAuth 4005, the subscription routes 4006-4012
/// ([`crate::seat_credentials::SUBSCRIPTION_ROUTES`]), agy's Gemini API key 4013
/// ([`crate::seat_gateway::GEMINI_PORT`]), the ask MCP 8700 and the gate hook 8711
/// ([`crate::seat_gateway`], [`crate::hook_relay`]). Each route admits only its owner agent's
/// unit keys, so a reachable port is not a usable credential.
pub(crate) const GATEWAY_PORTS: &[u16] = &[
    3000,
    4000,
    4001,
    4002,
    4003,
    4004,
    4005,
    4006,
    4007,
    4008,
    4009,
    4010,
    4011,
    4012,
    crate::seat_gateway::GEMINI_PORT,
    8700,
    crate::hook_relay::HOOK_PORT,
];
/// The engine's trusted launcher, from the floor image mounted read-only at [`FLOOR_DIR`]: the
/// seat's entrypoint, so no code from the seat image runs before the floor holds.
const SEAT_ENTRY: &str = "/floor/launch";
/// landrun (static) from the floor image.
const LANDRUN: &str = "/floor/landrun";
/// Where the floor image is mounted in every seat.
const FLOOR_DIR: &str = "/floor";
/// The floor's static busybox, the only client a container seat's gate hook uses
/// ([`crate::hook_relay::hook_command`]).
pub(crate) const FLOOR_BUSYBOX: &str = "/floor/busybox";
const CANARY_FILE: &str = "/wicked-canary/probe";
/// The loopback port the launcher's network canary listens on (outside every grant; the
/// launcher refuses a policy that grants it).
pub(crate) const CANARY_PORT: u16 = 47913;

/// The one seat Dockerfile; each CLI is its published package or its vendor tarball
/// ([`SEAT_PACKAGES`]).
const SEAT_DOCKERFILE: &str = include_str!("../containers/seats/seat.Dockerfile");
/// The floor image: static busybox, static landrun and the launcher, never run as a container.
const FLOOR_DOCKERFILE: &str = include_str!("../containers/floor.Dockerfile");

/// How a seat image installs its CLI, at the version the engine is verified against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SeatPackage {
    /// The CLI's published npm package (`<name>@<x.y.z>`).
    Npm(&'static str),
    /// A CLI not published to npm: its vendor's release tarball, pinned per architecture.
    Tarball(&'static SeatTarball),
}

/// One vendor release of a tarball-installed CLI. The vendor signs nothing, so the pinned sha256
/// IS the trust: the image build refuses any other bytes before unpacking them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SeatTarball {
    /// The CLI's own version (`agy --version`).
    pub(crate) version: &'static str,
    /// The vendor's build id, part of the release path.
    pub(crate) build: &'static str,
    /// The one file the image installs from the archive (as `/usr/local/bin/<cli>`).
    pub(crate) member: &'static str,
    /// `(url, sha256)` of the linux x86-64 archive.
    pub(crate) amd64: (&'static str, &'static str),
    /// `(url, sha256)` of the linux arm64 archive.
    pub(crate) arm64: (&'static str, &'static str),
}

/// Google's Antigravity CLI 1.4.2 (build 4835981233356800): the archives Google publishes for the
/// release (`storage.googleapis.com/antigravity-public/antigravity-cli/<ver>-<build>/…`), whose
/// sha256 are the Homebrew cask's for the same release and whose sha512 match Google's own update
/// manifest (`…/manifests/linux_{amd64,arm64}.json`). Each archive holds one static-ish Go binary,
/// `antigravity` (glibc >= 2.26).
const AGY_TARBALL: SeatTarball = SeatTarball {
    version: "1.4.2",
    build: "4835981233356800",
    member: "antigravity",
    amd64: (
        "https://storage.googleapis.com/antigravity-public/antigravity-cli/1.4.2-4835981233356800/linux-x64/cli_linux_x64.tar.gz",
        "6395837ac0e193dd3fe50f1228e1c4aea345ca99636d54404ab7e74397d4091c",
    ),
    arm64: (
        "https://storage.googleapis.com/antigravity-public/antigravity-cli/1.4.2-4835981233356800/linux-arm/cli_linux_arm64.tar.gz",
        "08dd4a2ca52d531fef1dfae4ec2fbafcd56342bfa694c8193a60da2b26fb1898",
    ),
};

impl SeatPackage {
    /// The CLI version the image pins.
    pub(crate) fn version(&self) -> Option<&'static str> {
        match self {
            SeatPackage::Npm(p) => p.rsplit_once('@').map(|(_, v)| v),
            SeatPackage::Tarball(t) => Some(t.version),
        }
    }

    /// Everything that identifies the installed bytes: part of the image tag and its label, so a
    /// new version, build or checksum is a new image, never a stale one.
    pub(crate) fn pin(&self) -> String {
        match self {
            SeatPackage::Npm(p) => (*p).to_string(),
            SeatPackage::Tarball(t) => format!(
                "{}-{} {} amd64:{} arm64:{}",
                t.version, t.build, t.member, t.amd64.1, t.arm64.1
            ),
        }
    }

    /// The seat Dockerfile's build arguments that install this CLI.
    fn build_args(&self) -> Vec<String> {
        let mut out = vec![format!("CLI_PIN={}", self.pin())];
        match self {
            SeatPackage::Npm(p) => out.push(format!("CLI_PACKAGE={p}")),
            SeatPackage::Tarball(t) => out.extend([
                format!("CLI_TARBALL_MEMBER={}", t.member),
                format!("CLI_TARBALL_URL_AMD64={}", t.amd64.0),
                format!("CLI_TARBALL_SHA256_AMD64={}", t.amd64.1),
                format!("CLI_TARBALL_URL_ARM64={}", t.arm64.0),
                format!("CLI_TARBALL_SHA256_ARM64={}", t.arm64.1),
            ]),
        }
        out
    }
}

/// The CLIs the engine ships a seat image for (the registry's seat keys), each with the package
/// at the version the engine is verified against.
const SEAT_PACKAGES: &[(&str, SeatPackage)] = &[
    // 2.1.295+: a hook honours `onFailure: "block"` (the gate hook fails closed; core#988 probed
    // the command hook's exit 1 and timeout on 2.1.296).
    (
        "claude",
        SeatPackage::Npm("@anthropic-ai/claude-code@2.1.296"),
    ),
    ("codex", SeatPackage::Npm("@openai/codex@0.162.1")),
    (
        "pi",
        SeatPackage::Npm("@earendil-works/pi-coding-agent@1.1.0"),
    ),
    ("opencode", SeatPackage::Npm("@opencode/cli@2.0.25")),
    ("copilot", SeatPackage::Npm("@github/copilot@1.0.96")),
    // Not on npm. Benched until its timed ballot (the registry's enabled flags); an online agy
    // seat has no gateway credential route yet ([`gateway_route_defined`]), so none runs online.
    ("agy", SeatPackage::Tarball(&AGY_TARBALL)),
];

/// The seat floor a PRODUCT process (the `wicked-core` CLI; crew's daemon does the same) runs
/// governed units on (C5): `WICKED_SEAT_FLOOR=container` unless the operator chose a floor —
/// `host` keeps the labelled interim host floor. Set once at process start, before any engine
/// exists; the library itself never defaults the variable (embedders and tests say what they run).
/// Returns the floor in force.
pub fn default_seat_floor() -> String {
    match std::env::var(SEAT_FLOOR_ENV) {
        Ok(v) if !v.trim().is_empty() => v,
        _ => {
            std::env::set_var(SEAT_FLOOR_ENV, "container");
            "container".into()
        }
    }
}

/// Whether a floor OTHER than the container floor was chosen (`WICKED_SEAT_FLOOR=host`, or any other
/// non-empty value, a typo included — codex c5-r10): the labelled interim host floor, with its
/// fail-closed rule. Products always choose a floor ([`default_seat_floor`]); only an UNSET floor
/// (a library embedding or a test) keeps the pre-C5 wrapped behaviour.
pub(crate) fn host_floor_chosen() -> bool {
    std::env::var(SEAT_FLOOR_ENV).is_ok_and(|v| {
        let v = v.trim();
        !v.is_empty() && !v.eq_ignore_ascii_case("container")
    })
}

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
    /// (core#1194) `None` for every real runtime: each command keeps its caller's deadline. A unit
    /// test's stand-in that always ANSWERS sets a failure guard here, so the test is judged on the
    /// answer and never on how long a loaded host took to start a shell script. A stand-in that
    /// stalls on purpose leaves it `None` and keeps the short deadline its test relies on.
    pub(crate) answer_bound: Option<Duration>,
}

impl Runtime {
    /// The bound for one command of this runtime whose caller allows `declared`: the longer of
    /// `declared` and [`Self::answer_bound`].
    pub(crate) fn bound(&self, declared: Duration) -> Duration {
        self.answer_bound.map_or(declared, |b| b.max(declared))
    }

    pub(crate) fn cmd(&self) -> Command {
        let mut c = Command::new(&self.cli);
        c.hardened().stdin(Stdio::null());
        if let Some(path) = client_path(&self.cli, std::env::var_os("PATH").as_deref()) {
            c.env("PATH", path);
        }
        c
    }
}

#[cfg(test)]
// Every caller is a unix-only test (stub shell scripts).
#[cfg_attr(not(unix), allow(dead_code))]
impl Runtime {
    /// A test stand-in that always answers (core#1194): its commands are bounded only by
    /// [`crate::test_env::PATIENT`], a failure guard no host load exhausts.
    pub(crate) fn answering(cli: PathBuf) -> Self {
        Runtime {
            cli,
            answer_bound: Some(crate::test_env::PATIENT),
        }
    }

    /// A test stand-in that stalls on purpose: every caller's own deadline holds.
    pub(crate) fn stalling(cli: PathBuf) -> Self {
        Runtime {
            cli,
            answer_bound: None,
        }
    }
}

/// `PATH` for the runtime CLI: its own directory first. The desktops ship the client's helpers
/// beside it (`docker-credential-osxkeychain`, `docker-buildx` in `~/.rd/bin`, Docker.app's
/// `bin`), and a daemon's `PATH` often lacks that directory: without it a build's base-image pull
/// fails on "error getting credentials".
fn client_path(cli: &Path, path: Option<&std::ffi::OsStr>) -> Option<std::ffi::OsString> {
    let dir = cli.parent().filter(|d| d.is_absolute())?;
    let mut dirs: Vec<PathBuf> = vec![dir.to_path_buf()];
    if let Some(p) = path {
        dirs.extend(std::env::split_paths(p).filter(|d| d != dir));
    }
    std::env::join_paths(dirs).ok()
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
            if p.is_file() {
                push(p.to_path_buf());
            }
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
        for rel in [
            ".rd/bin/docker",
            ".orbstack/bin/docker",
            ".colima/bin/docker",
        ] {
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
/// start or ran out of time. Both pipes are drained while waiting, so a large output cannot fill
/// a pipe and stall the child.
pub(crate) fn output_within(mut cmd: Command, limit: Duration) -> Option<std::process::Output> {
    use std::io::Read;
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    {
        // Its own group: a deadline kills every helper it started, not only the client.
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().ok()?;
    let drain = |p: Option<Box<dyn Read + Send>>| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = p {
                let _ = p.read_to_end(&mut buf);
            }
            let _ = tx.send(buf);
        });
        rx
    };
    let out = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let err = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if start.elapsed() < limit => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                #[cfg(unix)]
                // SAFETY: killpg on the child's own process group (pgid == its pid).
                unsafe {
                    libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
                }
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    // Bounded: a descendant still holding a pipe cannot hold this past the deadline.
    let grace = Duration::from_secs(2);
    let stdout = out.recv_timeout(grace).unwrap_or_default();
    let stderr = err.recv_timeout(grace).unwrap_or_default();
    status.map(|status| std::process::Output {
        status,
        stdout,
        stderr,
    })
}

/// The runtime this host runs seats with, probed (`<cli> version` must reach the engine). A
/// success is cached for the process; a failure is not, so starting the runtime later is enough.
/// The runtime [`runtime`] cached, keyed by the operator's choice, so a changed
/// `WICKED_CONTAINER_RUNTIME` is honoured.
static RUNTIME_OK: Mutex<Option<(Option<String>, Runtime)>> = Mutex::new(None);

pub(crate) fn runtime() -> Result<Runtime, FloorUnarmed> {
    let choice = std::env::var(RUNTIME_ENV).ok();
    if let Some((_, rt)) = RUNTIME_OK
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .filter(|(c, _)| *c == choice)
    {
        return Ok(rt);
    }
    let rt = probe_runtime(choice.as_deref())?;
    if let Ok(mut g) = RUNTIME_OK.lock() {
        *g = Some((choice.clone(), rt.clone()));
    }
    Ok(rt)
}

/// Ask the runtime engine NOW, never the cache [`runtime`] keeps (PRODUCT-WORKS J4): whether a
/// container runtime CLI is installed but its engine does not answer `version`. A desktop
/// runtime that restarts (Rancher Desktop, Docker Desktop) is gone for a while after [`runtime`]
/// has cached its first answer, so only a fresh probe tells a runtime blip from a seat's own
/// failure. `false` when it answers, and when no runtime CLI is installed at all (nothing to
/// wait for).
pub(crate) fn runtime_down_now() -> bool {
    let choice = std::env::var(RUNTIME_ENV).ok();
    // The runtime the seats USE: once [`runtime`] has cached one for this choice, only that one
    // counts — another healthy candidate (podman beside a restarting docker) runs no seat
    // (xreview on #1129).
    let cached = RUNTIME_OK
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .filter(|(c, _)| *c == choice)
        .map(|(_, rt)| rt);
    match cached {
        Some(rt) => !runtime_answers(&rt),
        None => matches!(
            probe_runtime(choice.as_deref()),
            Err(FloorUnarmed::ContainerRuntimeDown)
        ),
    }
}

/// Whether `rt`'s engine answers `version` now.
fn runtime_answers(rt: &Runtime) -> bool {
    let mut probe = rt.cmd();
    probe.args(["version", "--format", "{{.Server.Version}}"]);
    output_within(probe, rt.bound(PROBE_TIMEOUT)).is_some_and(|o| o.status.success())
}

/// The first runtime candidate whose engine answers `version`, probed (uncached).
fn probe_runtime(choice: Option<&str>) -> Result<Runtime, FloorUnarmed> {
    if cfg!(windows) {
        return Err(FloorUnarmed::ContainerUnsupportedHost);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let cands = runtime_candidates(choice, home.as_deref(), crate::validator::find_on_path);
    if cands.is_empty() {
        return Err(FloorUnarmed::NoContainerRuntime);
    }
    for cli in cands {
        let rt = Runtime {
            cli,
            answer_bound: None,
        };
        if runtime_answers(&rt) {
            return Ok(rt);
        }
    }
    Err(FloorUnarmed::ContainerRuntimeDown)
}

/// The CPUs a seat container can use: the runtime engine's (`<cli> info --format {{.NCPU}}`,
/// docker and nerdctl), which under a VM runtime (Docker Desktop, Rancher Desktop, colima) is the
/// VM's, not the host's. A seat container carries no CPU limit of its own ([`run_argv`] sets
/// `--memory`, never `--cpus`). When the runtime does not answer it (podman's template differs, a
/// probe that times out), the host's parallelism stands in. Probed once per runtime CLI for the
/// process, whatever the answer, so a runtime that cannot answer is not asked again at every
/// spawn; [`runtime`] itself is cached once it answers.
pub(crate) fn seat_cpus() -> usize {
    static CACHE: Mutex<Option<(PathBuf, usize)>> = Mutex::new(None);
    let host = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1);
    let Ok(rt) = runtime() else {
        return host;
    };
    if let Some((_, n)) = CACHE
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .filter(|(cli, _)| *cli == rt.cli)
    {
        return n;
    }
    let mut probe = rt.cmd();
    probe.args(["info", "--format", "{{.NCPU}}"]);
    let answer = output_within(probe, PROBE_TIMEOUT)
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
    let n = cpus_from_info(answer.as_deref(), host);
    if let Ok(mut g) = CACHE.lock() {
        *g = Some((rt.cli.clone(), n));
    }
    n
}

/// The runtime's CPU count from its `info` answer, or `host` when it is not a positive count.
fn cpus_from_info(answer: Option<&str>, host: usize) -> usize {
    answer
        .and_then(|a| a.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(host)
}

/// The package of `cli`'s seat image, when the engine ships one.
pub(crate) fn seat_package(cli: &str) -> Option<SeatPackage> {
    SEAT_PACKAGES
        .iter()
        .find(|(k, _)| *k == cli)
        .map(|(_, p)| *p)
}

/// The CLIs the engine ships a seat image for.
pub(crate) fn seat_clis() -> impl Iterator<Item = &'static str> {
    SEAT_PACKAGES.iter().map(|(k, _)| *k)
}

/// The base image the seat images build on ([`BASE_IMAGE_ENV`], else the default).
fn base_image() -> String {
    std::env::var(BASE_IMAGE_ENV)
        .ok()
        .map(|b| b.trim().to_string())
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| DEFAULT_BASE.to_string())
}

/// 12 hex of sha256 over `parts` (NUL-separated): an image tag that changes with its inputs.
fn content_tag(parts: &[&str]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0u8]);
    }
    h.finalize()[..6]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The tag of `cli`'s seat image built on `base`: `wicked-seat-<cli>:<12 hex of sha256(Dockerfile,
/// package, base)>`, so an edited Dockerfile, package or base is a new image, never a stale one.
pub(crate) fn seat_image_tag(cli: &str, base: &str) -> Option<String> {
    let package = seat_package(cli)?;
    Some(format!(
        "wicked-seat-{cli}:{}",
        content_tag(&[SEAT_DOCKERFILE, &package.pin(), base])
    ))
}

/// The floor image's tag: `wicked-floor:<12 hex of sha256(its Dockerfile)>`.
pub(crate) fn floor_image_tag() -> String {
    format!("wicked-floor:{}", content_tag(&[FLOOR_DOCKERFILE]))
}

/// The operator's own image for `cli` (`WICKED_SEAT_IMAGE_<CLI>`), used as-is.
pub(crate) fn image_override(cli: &str) -> Option<String> {
    let key = format!("WICKED_SEAT_IMAGE_{}", cli.to_ascii_uppercase());
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The image id `tag` names now, so a run uses exactly what was checked.
fn image_id(rt: &Runtime, tag: &str) -> Option<String> {
    let mut c = rt.cmd();
    c.args(["image", "inspect", "--format", "{{.Id}}", tag]);
    output_within(c, rt.bound(PROBE_TIMEOUT))
        .filter(|o| o.status.success())
        .and_then(|o| valid_image_id(String::from_utf8_lossy(&o.stdout).trim()))
}

/// [`image_id`] for the image holders ([`crate::image_keep`]).
pub(crate) fn image_id_of(rt: &Runtime, tag: &str) -> Option<String> {
    image_id(rt, tag)
}

/// An immutable local image id as the runtime spells it: docker's `sha256:<64 hex>` or podman's
/// bare `<64 hex>` (codex ln-r2). Kept in that spelling for the runtime's own later commands.
fn valid_image_id(id: &str) -> Option<String> {
    let hex = id.strip_prefix("sha256:").unwrap_or(id);
    (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then(|| id.to_string())
}

/// [`image_present`], putting back a tag a runtime's cleanup removed while its holder kept the
/// image ([`crate::image_keep::restore`]).
pub(crate) fn image_present_or_restored(rt: &Runtime, tag: &str) -> bool {
    image_present(rt, tag) || crate::image_keep::restore(rt, tag)
}

pub(crate) fn image_present(rt: &Runtime, tag: &str) -> bool {
    let mut c = rt.cmd();
    c.args(["image", "inspect", "--format", "{{.Id}}", tag]);
    output_within(c, rt.bound(PROBE_TIMEOUT)).is_some_and(|o| o.status.success())
}

/// Build `cli`'s seat image from the seat Dockerfile alone (no build context).
pub(crate) fn build_seat_image(rt: &Runtime, cli: &str) -> Result<String, String> {
    let package =
        seat_package(cli).ok_or_else(|| format!("no seat image is defined for `{cli}`"))?;
    build_seat_image_on(rt, cli, package, &SeatBase::from_env())
}

/// The base the seat images build on: the operator's own ([`BASE_IMAGE_ENV`], with its recipe
/// [`BASE_DOCKERFILE_ENV`]) or the default registry image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SeatBase {
    pub(crate) image: String,
    /// The operator set the base: it may exist only on this runtime.
    pub(crate) operator: bool,
    pub(crate) recipe: Option<PathBuf>,
}

impl SeatBase {
    pub(crate) fn from_env() -> Self {
        match configured_base() {
            Some(image) => SeatBase {
                image,
                operator: true,
                recipe: base_recipe(),
            },
            None => SeatBase {
                image: DEFAULT_BASE.to_string(),
                operator: false,
                recipe: None,
            },
        }
    }
}

/// [`build_seat_image`] on `base`: an operator base that is missing is rebuilt from its recipe
/// first (and held at once); a missing operator base with no recipe fails naming the base and the
/// fix. The default base is a registry image the build pulls.
fn build_seat_image_on(
    rt: &Runtime,
    cli: &str,
    package: SeatPackage,
    base: &SeatBase,
) -> Result<String, String> {
    let tag = seat_image_tag(cli, &base.image).expect("package exists");
    if base.operator {
        if image_present_or_restored(rt, &base.image) {
            // Held before the build, whatever the build does: a local-only base must not be
            // left to the image cleanup while a seat image builds on it.
            let _ = crate::image_keep::hold_now(rt, &[&base.image]);
        } else if let Some(recipe) = &base.recipe {
            rebuild_base(rt, &base.image, recipe)?;
        }
    }
    let mut args = vec![format!("BASE={}", base.image), format!("CLI={cli}")];
    args.extend(package.build_args());
    build_image(rt, SEAT_DOCKERFILE, &tag, &args).map_err(|e| {
        if base.operator && !image_present(rt, &base.image) {
            missing_base_error(&base.image, &e)
        } else {
            e
        }
    })
}

/// [`BASE_DOCKERFILE_ENV`], when set (trimmed).
fn base_recipe() -> Option<PathBuf> {
    std::env::var(BASE_DOCKERFILE_ENV)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Build the missing seat base `base` from `recipe` (a Dockerfile, no build context).
pub(crate) fn rebuild_base(rt: &Runtime, base: &str, recipe: &Path) -> Result<(), String> {
    let df = std::fs::read_to_string(recipe).map_err(|e| {
        format!(
            "the seat base image {base} is missing and its Dockerfile {} ({BASE_DOCKERFILE_ENV}) \
             cannot be read: {e}",
            recipe.display()
        )
    })?;
    eprintln!(
        "wicked-core: the seat base image {base} is missing; rebuilding it from {}",
        recipe.display()
    );
    build_image(rt, &df, base, &[])?;
    // Held at once: a seat build that fails after this must not leave the new base unheld.
    let _ = crate::image_keep::hold_now(rt, &[base]);
    Ok(())
}

/// The error of a seat build whose base is not on the runtime and could not be pulled.
pub(crate) fn missing_base_error(base: &str, build_error: &str) -> String {
    format!(
        "the seat base image {base} is not on the container runtime and could not be pulled \
         (a runtime's image cleanup may have deleted it); rebuild it, or set \
         {BASE_DOCKERFILE_ENV} to its Dockerfile so the engine rebuilds it: {build_error}"
    )
}

/// The floor image ([`FLOOR_DOCKERFILE`]), built on first use (one build at a time).
pub(crate) fn ensure_floor_image(rt: &Runtime) -> Result<String, String> {
    static BUILD: Mutex<()> = Mutex::new(());
    let tag = floor_image_tag();
    if !image_present(rt, &tag) {
        let _one = BUILD.lock();
        if !image_present(rt, &tag) {
            eprintln!("wicked-core: building floor image {tag} (first use)");
            build_image(rt, FLOOR_DOCKERFILE, &tag, &[])?;
            let _ = crate::image_keep::hold_now(rt, &[&tag]);
            return Ok(tag);
        }
    }
    let _ = crate::image_keep::hold(rt, &[&tag]);
    Ok(tag)
}

/// What the engine learned about one seat image (by image id): the paths in its root filesystem
/// that are symbolic links, and whether it declares volumes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ImageFacts {
    /// The image id the facts are about: the seat runs THIS id, never a tag that could move
    /// between the inspection and the run.
    pub(crate) id: String,
    pub(crate) symlinks: std::collections::BTreeSet<PathBuf>,
    pub(crate) volumes: Vec<String>,
    /// The image declares an active `HEALTHCHECK`: the runtime would exec it beside the seat,
    /// outside `/floor/launch` and its Landlock domain.
    pub(crate) healthcheck: bool,
}

/// The facts of `image`, read by the engine's OWN code on its OWN root filesystem: the floor image
/// runs `find -type l` over the seat image mounted as data (`--mount type=image`), so nothing of
/// the seat image runs and none of its links is followed. Memoized per image id.
pub(crate) fn inspect_seat_image(
    rt: &Runtime,
    floor: &str,
    image: &str,
) -> Result<ImageFacts, String> {
    static SEEN: Mutex<Option<std::collections::HashMap<String, ImageFacts>>> = Mutex::new(None);
    let mut c = rt.cmd();
    c.args([
        "image",
        "inspect",
        "--format",
        "{{.Id}} {{json .Config}}",
        image,
    ]);
    let out = output_within(c, rt.bound(PROBE_TIMEOUT))
        .filter(|o| o.status.success())
        .ok_or_else(|| format!("could not inspect seat image {image}"))?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let (id, config) = text
        .trim()
        .split_once(' ')
        .and_then(|(id, c)| Some((valid_image_id(id)?, c)))
        .ok_or_else(|| format!("could not inspect seat image {image}"))?;
    let id = id.as_str();
    if let Some(f) = SEEN.lock().ok().and_then(|m| m.as_ref()?.get(id).cloned()) {
        return Ok(f);
    }
    let config: serde_json::Value = serde_json::from_str(config).unwrap_or_default();
    let volumes: Vec<String> = config
        .get("Volumes")
        .and_then(|v| v.as_object())
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    // `{"Test": ["NONE"]}` (or no test) is a disabled check; anything else would run.
    let healthcheck = config
        .pointer("/Healthcheck/Test")
        .and_then(|t| t.as_array())
        .and_then(|t| t.first())
        .and_then(|t| t.as_str())
        .is_some_and(|first| first != "NONE");
    let mut c = rt.cmd();
    c.args([
        "run",
        "--rm",
        "--pull",
        "never",
        "--network",
        "none",
        "--read-only",
        "--cap-drop",
        "ALL",
        // Read every directory of the image, whatever its modes (no write, no other privilege).
        "--cap-add",
        "DAC_READ_SEARCH",
        "--security-opt",
        "no-new-privileges",
        "--user",
        "0:0",
        "--mount",
        &format!("type=image,src={id},dst=/seat"),
        "--entrypoint",
        "/busybox",
        floor,
        "find",
        "/seat",
        "-xdev",
        "-type",
        "l",
        "-print0",
    ]);
    let out = output_within(c, rt.bound(Duration::from_secs(120)))
        .ok_or_else(|| format!("inspecting seat image {image} did not finish"))?;
    if !out.status.success() {
        return Err(format!(
            "inspecting seat image {image} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let symlinks = out
        .stdout
        .split(|b| *b == 0)
        .filter_map(|p| std::str::from_utf8(p).ok())
        .filter_map(|p| p.strip_prefix("/seat"))
        .filter(|p| p.starts_with('/'))
        .map(PathBuf::from)
        .collect();
    let facts = ImageFacts {
        id: id.to_string(),
        symlinks,
        volumes,
        healthcheck,
    };
    if let Ok(mut m) = SEEN.lock() {
        m.get_or_insert_with(Default::default)
            .insert(id.to_string(), facts.clone());
    }
    Ok(facts)
}

/// Refuse a seat image that could move a mount: a declared volume (the runtime mounts it from the
/// image, deeper mounts win), or a symbolic link at any path a mount lands on, or at any of its
/// ancestors (the runtime follows image links when it resolves a mount destination, so a link at
/// `/floor` or at a worktree's path would put the trusted launcher, or the seat's writable tree,
/// somewhere the image chose).
pub(crate) fn check_seat_image(facts: &ImageFacts, dests: &[PathBuf]) -> Result<(), String> {
    if let Some(v) = facts.volumes.first() {
        return Err(format!(
            "the seat image declares a volume ({v}); seat images may not declare volumes"
        ));
    }
    if facts.healthcheck {
        return Err(
            "the seat image declares a HEALTHCHECK, which the runtime would run outside the seat \
             launcher; seat images may not declare one (use HEALTHCHECK NONE)"
                .into(),
        );
    }
    let reserved = [FLOOR_DIR, "/wicked-canary", "/tmp", "/home/seat"].map(PathBuf::from);
    for d in reserved.iter().chain(dests) {
        if let Some(link) = d.ancestors().find(|a| facts.symlinks.contains(*a)) {
            return Err(format!(
                "the seat image has a symbolic link at {} (where the mount {} resolves); refusing it",
                link.display(),
                d.display()
            ));
        }
    }
    Ok(())
}

/// The seat network sidecar's Dockerfile (alpine + iptables; see the file).
const NETNS_DOCKERFILE: &str = include_str!("../containers/seats/netns.Dockerfile");

/// The sidecar image's tag (its Dockerfile's hash).
pub(crate) fn netns_image_tag() -> String {
    format!("wicked-seat-netns:{}", content_tag(&[NETNS_DOCKERFILE]))
}

/// The sidecar image, built on first use (tagged by its Dockerfile's hash).
pub(crate) fn ensure_netns_image(rt: &Runtime) -> Result<String, String> {
    let tag = netns_image_tag();
    if !image_present(rt, &tag) {
        static BUILD: Mutex<()> = Mutex::new(());
        let _one = BUILD.lock();
        if !image_present(rt, &tag) {
            build_image(
                rt,
                NETNS_DOCKERFILE,
                &tag,
                &["BASE=alpine:3.22".to_string()],
            )?;
            let _ = crate::image_keep::hold_now(rt, &[&tag]);
            return Ok(tag);
        }
    }
    let _ = crate::image_keep::hold(rt, &[&tag]);
    Ok(tag)
}

/// Build `df` alone (no build context) as `tag`, streaming the runtime's progress to the engine's
/// stderr. `Err` carries the runtime's last words.
pub(crate) fn build_image(
    rt: &Runtime,
    df: &str,
    tag: &str,
    build_args: &[String],
) -> Result<String, String> {
    let tag = tag.to_string();
    let mut c = rt.cmd();
    c.args(["build", "--label", &format!("{SEAT_LABEL}=1")]);
    for a in build_args {
        c.args(["--build-arg", a]);
    }
    c.args(["-t", &tag, "-"]);
    c.stdin(Stdio::piped())
        .stdout(Stdio::null()) // crew E1: stdout is seat-login argv JSON only.
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        c.process_group(0);
    }
    let mut child = c
        .spawn()
        .map_err(|e| format!("could not start `{} build`: {e}", rt.cli.display()))?;
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        let _ = stdin.write_all(df.as_bytes());
    }
    // Stream the build's progress as it comes (a full pipe would stall the build), keeping
    // the last lines for the error.
    let progress = child.stderr.take().map(|se| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::BufRead;
            let mut tail: std::collections::VecDeque<String> = std::collections::VecDeque::new();
            for line in std::io::BufReader::new(se).lines().map_while(Result::ok) {
                eprintln!("{line}");
                if tail.len() == 6 {
                    tail.pop_front();
                }
                tail.push_back(line);
            }
            let _ = tx.send(tail.into_iter().collect::<Vec<_>>());
        });
        rx
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if start.elapsed() < BUILD_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(200))
            }
            _ => {
                #[cfg(unix)]
                // SAFETY: killpg on the build's own process group (pgid == its pid).
                unsafe {
                    libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
                }
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("building {tag} ran past {BUILD_TIMEOUT:?}"));
            }
        }
    };
    // Bounded: a descendant still holding stderr cannot hold the build past its exit.
    let tail = progress
        .and_then(|rx| rx.recv_timeout(Duration::from_secs(5)).ok())
        .unwrap_or_default();
    if status.success() {
        Ok(tag)
    } else {
        Err(format!(
            "building {tag} failed ({status}): {}",
            tail.join(" | ")
        ))
    }
}

/// The image `cli`'s seat runs: the operator's override as-is, else the engine's seat image,
/// built on first use (one build at a time per process).
// The seat sign-in path (unix) and the tests use it; native Windows runs no container floor.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn ensure_seat_image(rt: &Runtime, cli: &str) -> Result<String, String> {
    ensure_seat_image_noting(rt, cli).map(|(tag, _)| tag)
}

/// [`ensure_seat_image`], also saying how long the unit waited when the image was missing (a
/// first use, or a runtime's image cleanup deleted it) and had to be built, by this call or by
/// another unit it waited for, for the unit's note.
pub(crate) fn ensure_seat_image_noting(
    rt: &Runtime,
    cli: &str,
) -> Result<(String, Option<Duration>), String> {
    ensure_seat_image_with(rt, cli, image_override(cli), &SeatBase::from_env())
}

/// A wait on another unit's build shorter than this is not a rebuild worth a note.
const BUILD_WAIT_NOTE: Duration = Duration::from_secs(5);

/// [`ensure_seat_image_noting`] with the operator override and the base given.
fn ensure_seat_image_with(
    rt: &Runtime,
    cli: &str,
    over: Option<String>,
    base: &SeatBase,
) -> Result<(String, Option<Duration>), String> {
    if let Some(img) = over {
        // An operator's own seat image does not build on the seat base: only it is held (its
        // tag put back first if a cleanup took the tag but the holder kept the image).
        if !image_present(rt, &img) {
            let _ = crate::image_keep::restore(rt, &img);
        }
        let _ = crate::image_keep::hold(rt, &[&img]);
        return Ok((img, None));
    }
    let package =
        seat_package(cli).ok_or_else(|| format!("no seat image is defined for `{cli}`"))?;
    static BUILD: Mutex<()> = Mutex::new(());
    let tag = seat_image_tag(cli, &base.image).expect("package exists");
    if image_present_or_restored(rt, &tag) {
        hold_seat_images(rt, &tag, base, false);
        return Ok((tag, None));
    }
    let start = Instant::now();
    let _one = BUILD.lock();
    if image_present(rt, &tag) {
        // Another unit built it while this one waited.
        hold_seat_images(rt, &tag, base, false);
        let waited = start.elapsed();
        return Ok((tag, (waited >= BUILD_WAIT_NOTE).then_some(waited)));
    }
    eprintln!("wicked-core: building seat image {tag} (it is not on the container runtime)");
    let built = build_seat_image_on(rt, cli, package, base)?;
    hold_seat_images(rt, &built, base, true);
    Ok((built, Some(start.elapsed())))
}

/// Hold `seat` and the operator's seat base against the runtime's image cleanup
/// ([`crate::image_keep`]); `built` holds at once, past the per-process memo. Best effort.
fn hold_seat_images(rt: &Runtime, seat: &str, base: &SeatBase, built: bool) {
    let mut refs = vec![seat];
    if base.operator {
        refs.push(&base.image);
    }
    let _ = if built {
        crate::image_keep::hold_now(rt, &refs)
    } else {
        crate::image_keep::hold(rt, &refs)
    };
}

/// Whether seats run with no network at all ([`NETWORK_ENV`]` = none`).
pub(crate) fn offline_seats() -> bool {
    std::env::var(NETWORK_ENV).is_ok_and(|v| v.trim() == "none")
}

/// Create seat `name`'s own internal network (named after the container, labelled for the
/// sweep), or `none` for an offline seat. An internal network has no route out: the only egress
/// a seat gets is the gateway the engine attaches to it.
pub(crate) fn ensure_seat_network(rt: &Runtime, name: &str) -> Result<String, String> {
    if offline_seats() {
        return Ok("none".into());
    }
    let mut create = rt.cmd();
    create.args([
        "network",
        "create",
        "--internal",
        "--label",
        &format!("{SEAT_LABEL}=1"),
        name,
    ]);
    match output_within(create, rt.bound(PROBE_TIMEOUT)) {
        Some(o) if o.status.success() => Ok(name.to_string()),
        Some(o) => Err(format!(
            "could not create the seat network `{name}`: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        None => Err(format!("creating the seat network `{name}` timed out")),
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
        && p.to_str()
            .is_some_and(|s| !s.contains([',', '"', '\'', '\n', '=']))
        // The engine's own paths in the seat: a host bind there would shadow the trusted launcher
        // or the canary.
        && ![FLOOR_DIR, "/wicked-canary"]
            .iter()
            .any(|r| p.starts_with(r))
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
    // The tree itself, read-only, when nothing above exposes it (a read-only unit outside a
    // linked worktree: the host floor left it visible, a container only shows what is mounted).
    if f.tree.is_dir() && !out.iter().any(|o| f.tree.starts_with(o.dst())) {
        put(&mut out, bind(&f.tree, false))?;
    }
    // EVERY existing mask, not only those under a mount yet: a read path [`finalize`] mounts
    // later (a read root, an argv path) may be an ancestor, and the runtime mounts the deeper
    // mask over it (mounts apply in destination-depth order).
    let masks: Vec<Mount> = f
        .masks
        .iter()
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
    /// The floor image (the trusted launcher), mounted read-only at [`FLOOR_DIR`].
    pub(crate) floor: &'a str,
    pub(crate) memory: &'a str,
    /// The only TCP destination ports the CLI may connect to.
    pub(crate) gateway_ports: &'a [u16],
    /// Loopback ports the seat serves itself (opencode's pinned server): bind AND connect.
    pub(crate) own_ports: &'a [u16],
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
    "--memory",
    "--network",
    "--workdir",
    "--mount",
    "--env",
    "--entrypoint",
];
const FLAG_OPTS: &[&str] = &["--rm", "--interactive", "--init", "--read-only"];

/// The memory limit selected by the operator, or the default when unset or blank.
fn memory_from(choice: Option<&str>) -> &str {
    choice
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .unwrap_or("3g")
}

/// The Landlock policy from the same mounts as the runtime: writable trees are executable too
/// (test runners exec from them), and read mounts admit executable skills and toolchains. The
/// entry's second invocation checks enforcement inside this domain before executing the CLI.
fn landrun_prefix(mounts: &[Mount], gateway_ports: &[u16], own_ports: &[u16]) -> Vec<String> {
    let mut out = vec![
        LANDRUN.into(),
        "--best-effort".into(),
        "--ignore-missing".into(),
    ];
    for p in [
        "/usr", "/bin", "/lib", "/lib64", "/sbin", "/opt", "/etc", FLOOR_DIR,
    ] {
        out.extend(["--rox".into(), p.into()]);
    }
    // Kernel metadata and devices are image facilities, not host bind mounts. `/dev/null`
    // needs write access for ordinary shell redirection; no writable directory is admitted.
    for p in ["/proc", "/dev", "/wicked-canary"] {
        out.extend(["--ro".into(), p.into()]);
    }
    out.extend(["--rw".into(), "/dev/null".into()]);
    for p in ["/home/seat", "/tmp"] {
        out.extend(["--rwx".into(), p.into()]);
    }
    for m in mounts {
        let access = match m {
            Mount::Bind { rw: true, .. } => "--rwx",
            Mount::Bind { rw: false, .. } => "--rox",
            Mount::NullFile { .. } | Mount::Tmpfs { .. } => "--ro",
        };
        out.extend([access.into(), m.dst().to_string_lossy().into_owned()]);
    }
    for port in gateway_ports {
        out.extend(["--connect-tcp".into(), port.to_string()]);
    }
    for port in own_ports.iter().filter(|p| **p != 0) {
        out.extend(["--bind-tcp".into(), port.to_string()]);
        out.extend(["--connect-tcp".into(), port.to_string()]);
    }
    for name in ["PATH", "HOME", "TMPDIR", "LANG"] {
        out.extend(["--env".into(), name.into()]);
    }
    out.extend([
        "--".into(),
        SEAT_ENTRY.into(),
        "--check".into(),
        CANARY_FILE.into(),
        "--".into(),
    ]);
    out
}

/// The wrapper argv that runs a seat in a container and then under Landlock: the CLI argv
/// follows it. Pure (no runtime call), so the hardening is pinned by tests.
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
        // `exec`: the runtime's tmpfs default is noexec, and CLIs run code from both (copilot
        // unpacks its native addon into `$HOME/.cache`; test runners exec from `$TMPDIR`).
        "--tmpfs".into(),
        "/tmp:rw,exec,mode=1777,size=2g".into(),
        "--tmpfs".into(),
        "/home/seat:rw,exec,mode=1777,size=512m".into(),
        // Writable before Landlock, denied afterwards: a read-only root cannot fake a pass.
        "--tmpfs".into(),
        "/wicked-canary:rw,noexec,mode=1777,size=1m".into(),
        "--cap-drop".into(),
        "ALL".into(),
        "--security-opt".into(),
        "no-new-privileges".into(),
        "--pids-limit".into(),
        "4096".into(),
        "--memory".into(),
        l.memory.into(),
        "--network".into(),
        l.network.into(),
        "--workdir".into(),
        l.workdir.to_string_lossy().into_owned(),
        "--env".into(),
        SEAT_HOME_ENV.into(),
        "--env".into(),
        "TMPDIR=/tmp".into(),
        "--env".into(),
        "LANG=C.UTF-8".into(),
    ];
    for m in l.mounts {
        w.push("--mount".into());
        w.push(m.render());
    }
    // The engine's trusted launcher, read-only from its floor image: the entrypoint, so the
    // seat image's own entrypoint never runs (a custom or operator image cannot run code before
    // the canary and Landlock hold).
    w.push("--mount".into());
    w.push(format!("type=image,src={},dst={FLOOR_DIR}", l.floor));
    w.push("--entrypoint".into());
    w.push(SEAT_ENTRY.into());
    w.push(l.image.into());
    w.extend([CANARY_FILE.into(), "--".into()]);
    w.extend(landrun_prefix(l.mounts, l.gateway_ports, l.own_ports));
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
            if a == "--label"
                && args.get(i + 1).map(String::as_str) == Some(&format!("{SEAT_LABEL}=1"))
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

/// The end of our landrun options, walking their dialect rather than searching the CLI's argv
/// (a prompt may contain `--`, `landrun` or the entry's name).
fn landrun_end(args: &[String], img: usize) -> Option<usize> {
    let head = args.get(img + 1..img + 4)?;
    if head != [CANARY_FILE, "--", LANDRUN] {
        return None;
    }
    // The launcher must be ours: the floor mount and the entrypoint precede the image.
    if !args[..img]
        .windows(2)
        .any(|w| w == ["--entrypoint", SEAT_ENTRY])
    {
        return None;
    }
    let mut i = img + 4;
    while let Some(a) = args.get(i) {
        match a.as_str() {
            "--best-effort" | "--ignore-missing" => i += 1,
            "--ro" | "--rox" | "--rw" | "--rwx" | "--connect-tcp" | "--bind-tcp" | "--env" => {
                args.get(i + 1)?;
                i += 2;
            }
            "--" => {
                return (args.get(i + 1..i + 5)? == [SEAT_ENTRY, "--check", CANARY_FILE, "--"])
                    .then_some(i);
            }
            _ => return None,
        }
    }
    None
}

/// Removes a directory the container floor allocated for one unit (its fenced scratch) when
/// dropped, however the unit ends.
#[derive(Debug)]
pub(crate) struct RemoveOnDrop(pub(crate) PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Reaps a seat container when dropped (the end of the run, however it ends).
#[derive(Debug)]
pub(crate) struct ReapOnDrop(pub(crate) Option<SeatContainer>);

impl Drop for ReapOnDrop {
    fn drop(&mut self) {
        if let Some(seat) = self.0.take() {
            reap(&seat);
        }
    }
}

/// A seat container to reap: the runtime CLI that launched it and its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SeatContainer {
    pub(crate) runtime: PathBuf,
    pub(crate) name: String,
}

/// The seat container a `cmd` (one [`run_argv`] rendered) launches, for the reap: the reap uses
/// the same runtime CLI, never a fresh probe (the operator's choice may have changed since).
pub(crate) fn seat_container(cmd: &Command) -> Option<SeatContainer> {
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let img = image_index(&args)?;
    // Only the complete wrapper `run_argv` renders (the seat entry + landrun after the image):
    // a label alone never makes a command ours to reap.
    landrun_end(&args, img)?;
    args[..img]
        .windows(2)
        .find(|w| w[0] == "--name")
        .map(|w| SeatContainer {
            runtime: PathBuf::from(cmd.get_program()),
            name: w[1].clone(),
        })
}

/// Environment variables never passed into a seat: the image's own `PATH` (the host's points at
/// host binaries) and the host's temp / home spelling the container replaces.
const HOST_ONLY_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "TMPDIR",
    "PWD",
    "OLDPWD",
    "SHELL",
    "DYLD_LIBRARY_PATH",
    "DYLD_INSERT_LIBRARIES",
];

/// Whether `cmd` runs agy — on the host, or as the CLI of a seat container [`run_argv`] rendered
/// — and so must get its stdin as a pipe the engine closes at once, never `/dev/null`: headless
/// agy with `/dev/null` on stdin waits out its 60 s sign-in prompt whatever `--print-timeout`
/// says, while on a pipe it fails fast ("authentication required", exit 1) or runs (measured on
/// 1.4.2). A seat container's runtime client passes its own stdin through (`--interactive`).
pub(crate) fn stdin_must_be_a_pipe(cmd: &Command) -> bool {
    let is_agy = |b: &str| home_is_config_home(b);
    if is_agy(&cmd.get_program().to_string_lossy()) {
        return true;
    }
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    image_index(&args)
        .and_then(|img| landrun_end(&args, img))
        .and_then(|end| args.get(end + 5))
        .is_some_and(|cli| is_agy(cli))
}

/// Make a seat `cmd` (one [`run_argv`] rendered, then given its env by the carrier) complete:
/// every variable the carrier set explicitly is passed in by NAME to both the runtime and
/// landrun (`--env KEY`: each reads its own environment, so no value is on a command line). It adds
/// NO mount: the unit's read roots are mounted when the floor arms ([`arm`], [`add_read_roots`]),
/// where the claude seat's fence is narrowed to the mount set; a mount added here (a read-root link
/// re-resolved after arming) would land past that fence. Any other command is returned unchanged.
pub(crate) fn finalize(cmd: Command) -> Command {
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let Some(img) = image_index(&args) else {
        return cmd;
    };
    let Some(land_end) = landrun_end(&args, img) else {
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
    let mut extra: Vec<String> = Vec::new();
    let mut land_extra: Vec<String> = Vec::new();
    let mut envs: Vec<(String, Option<std::ffi::OsString>)> = Vec::new();
    let mut carrier_path: Option<std::ffi::OsString> = None;
    // Only the variables the carrier set EXPLICITLY reach the seat. Nothing the daemon merely
    // inherited does — by design: a seat's model credentials come from the gateway's broker,
    // never from the daemon's environment (an inherited OPENAI_API_KEY stays on the host).
    for (k, v) in cmd.get_envs() {
        let key = k.to_string_lossy().into_owned();
        envs.push((key.clone(), v.map(|v| v.to_os_string())));
        let Some(v) = v else { continue };
        if key == "PATH" {
            carrier_path = Some(v.to_os_string());
        }
        if !HOST_ONLY_ENV.contains(&key.as_str()) {
            extra.push("--env".into());
            extra.push(key.clone());
            if !args[img + 3..land_end]
                .windows(2)
                .any(|w| w == ["--env", &key])
            {
                land_extra.extend(["--env".into(), key]);
            }
        }
    }
    // The seat's PATH: the carrier's entries that exist inside (a mounted skills snapshot's
    // `scripts`, where garden's launcher lives), then the image's own. Host-only entries go.
    if let Some(p) = &carrier_path {
        extra.push("--env".into());
        extra.push(format!("PATH={}", container_path(p, &existing)));
    }
    // A CLI whose configuration home IS its home (agy) runs with the carrier's seat home.
    let mut head = args[..img].to_vec();
    if let Some(home) = carrier_home(&args, img, land_end, &envs, &existing) {
        if let Some(h) = head.iter_mut().find(|a| *a == SEAT_HOME_ENV) {
            *h = format!("HOME={home}");
        }
    }
    // The seat command was hardened where the carrier built it; re-harden the rebuilt one too.
    let mut out = Command::new(cmd.get_program());
    out.hardened();
    out.args(&head);
    out.args(&extra);
    out.args(&args[img..land_end]);
    out.args(&land_extra);
    // Mounts sit at canonical paths; a host alias the launch spelled (`/tmp` for `/private/tmp`
    // on macOS) does not exist inside, so env values are re-spelled (argv path options were
    // re-spelled at arm time, [`mount_argv_paths`], where the prompt is known).
    out.args(&args[land_end..]);
    if let Some(dir) = cmd.get_current_dir() {
        out.current_dir(dir);
    }
    for (k, v) in envs {
        // A carrier's `HOME` names the seat's home inside the container (rendered on the wrapper
        // above, never passed by name); the runtime client keeps the daemon's own, where its
        // configuration and context live (with the seat's, Rancher Desktop's client lost its
        // socket: "failed to connect to the docker API at unix:///var/run/docker.sock").
        if k == "HOME" {
            continue;
        }
        match v {
            Some(v) => {
                out.env(k, canonical_spelling(&v, &existing));
            }
            None => {
                out.env_remove(k);
            }
        }
    }
    out
}

/// The seat's default home: a tmpfs ([`run_argv`]).
const SEAT_HOME_ENV: &str = "HOME=/home/seat";

/// Whether `cli`'s configuration home IS its process home: agy publishes no configuration-home
/// variable (its `~/.gemini` and `~/.config/antigravity` are its whole state, conversations
/// included), so its seat home is its `HOME`. Every other CLI keeps the tmpfs home and names its
/// configuration home with its own variable.
fn home_is_config_home(cli: &str) -> bool {
    wicked_apps_core::spawn::SeatCli::from_binary(cli) == wicked_apps_core::spawn::SeatCli::Agy
}

/// The `HOME` a seat of a CLI whose home is its configuration home ([`home_is_config_home`]) runs
/// with: the one the carrier set (the engine-owned seat home of the run and posture, at its
/// mounted spelling), only when the deepest mount over it is a WRITABLE bind. `None` keeps the
/// tmpfs home: another CLI, no carrier home, or a home the container does not mount writable (it
/// would not exist, a mask or read-only bind covers it, or agy could not write its state there).
fn carrier_home(
    args: &[String],
    img: usize,
    land_end: usize,
    envs: &[(String, Option<std::ffi::OsString>)],
    mounts: &[PathBuf],
) -> Option<String> {
    let cli = args.get(land_end + 5)?;
    if !home_is_config_home(cli) {
        return None;
    }
    let home = envs
        .iter()
        .rev()
        .find(|(k, _)| k == "HOME")
        .and_then(|(_, v)| v.as_ref())?;
    let home = PathBuf::from(canonical_spelling(home, mounts));
    // The deepest mount over the home decides (a deeper read-only bind or a mask wins).
    let deepest = args[..img]
        .windows(2)
        .filter(|w| w[0] == "--mount")
        .filter_map(|w| {
            let dst = w[1].split(',').find_map(|kv| kv.strip_prefix("dst="))?;
            home.starts_with(dst)
                .then(|| (Path::new(dst).components().count(), w[1].as_str()))
        })
        .max_by_key(|(depth, _)| *depth);
    let writable =
        deepest.is_some_and(|(_, m)| m.starts_with("type=bind,") && !m.ends_with(",readonly"));
    (home.is_absolute() && writable && mountable(&home))
        .then(|| home.to_string_lossy().into_owned())
}

/// The devices a seat's CLI must open read-write beyond `/dev/null`: agy's shell tool runs every
/// command on a pseudo-terminal, so it needs `/dev/ptmx` and `/dev/pts` (without them every
/// command fails "failed to create PTY: open /dev/ptmx: permission denied", agy spike). Every
/// other CLI gets none: the grant is agy's alone.
fn seat_devices(cli: &str) -> &'static [&'static str] {
    match wicked_apps_core::spawn::SeatCli::from_binary(cli) {
        wicked_apps_core::spawn::SeatCli::Agy => &["/dev/ptmx", "/dev/pts"],
        _ => &[],
    }
}

/// Add `cli`'s [`seat_devices`] to the Landlock policy of a wrapper [`run_argv`] rendered, among
/// landrun's own options (before its `--`). `Err` when `wrapper` is not one of ours.
pub(crate) fn grant_seat_devices(wrapper: &mut Vec<String>, cli: &str) -> Result<(), String> {
    let devices = seat_devices(cli);
    if devices.is_empty() {
        return Ok(());
    }
    let args = wrapper.get(1..).unwrap_or_default();
    let end = image_index(args)
        .and_then(|img| landrun_end(args, img))
        .ok_or("the seat wrapper is not the engine's; no device grant")?;
    // `end` indexes `args` (the wrapper minus the runtime): landrun's `--` sits at `end + 1`.
    let at = end + 1;
    for d in devices.iter().rev() {
        wrapper.splice(at..at, ["--rw".to_string(), (*d).to_string()]);
    }
    Ok(())
}

/// `v` (a path or a path list) with each absolute entry whose canonical form is mounted
/// re-spelled canonically; anything else (not a path, not mounted) is returned unchanged.
fn canonical_spelling(v: &std::ffi::OsStr, mounts: &[PathBuf]) -> std::ffi::OsString {
    let parts: Vec<PathBuf> = std::env::split_paths(v).collect();
    if parts.is_empty() || !parts.iter().all(|p| p.is_absolute()) {
        return v.to_os_string();
    }
    let mapped: Vec<PathBuf> = parts
        .into_iter()
        .map(|p| match crate::worker_sandbox::canon_lenient(&p) {
            Some(c) if c != p && mounts.iter().any(|m| c.starts_with(m)) => c,
            _ => p,
        })
        .collect();
    std::env::join_paths(mapped).unwrap_or_else(|_| v.to_os_string())
}

/// The image's own `PATH` (Debian's default), after which a seat's PATH ends.
const IMAGE_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// A seat's PATH: the entries of `carrier` (the carrier's PATH) under a mount, then the image's.
fn container_path(carrier: &std::ffi::OsStr, mounts: &[PathBuf]) -> String {
    let mut out: Vec<String> = std::env::split_paths(carrier)
        .map(|d| crate::worker_sandbox::canon_lenient(&d).unwrap_or(d))
        .filter(|d| mounts.iter().any(|m| d.starts_with(m)))
        .filter_map(|d| d.to_str().map(str::to_string))
        .filter(|d| !d.contains(':'))
        .collect();
    out.push(IMAGE_PATH.to_string());
    out.join(":")
}

/// The options whose value is a host path the seat must read (claude's `--settings`,
/// `--plugin-dir`, `--mcp-config`, `--add-dir`, the system-prompt files; codex's `-C`/`--cd`).
const PATH_OPTS: &[&str] = &[
    "--settings",
    "--plugin-dir",
    "--mcp-config",
    "--add-dir",
    "--system-prompt-file",
    "--append-system-prompt-file",
    "--cd",
    "-C",
    "--skill",
];

/// Where `tail` (the CLI argv) carries a [`PATH_OPTS`] value, before any `--`: `(index, offset)`
/// of each value — `--opt <value>` is `(i + 1, 0)`, `--opt=<value>` is `(i, "--opt=".len())`.
/// The prompt is text, whatever it spells: a token equal to `prompt` is never read as an option
/// or a value (agy r1: a template may place the prompt after any bare flag, e.g. `-m {PROMPT}`,
/// where no `--` guards it), and neither is the value after `-p`/`--prompt`. The one parser for
/// both discovering the paths to mount and re-spelling them at their mounted spelling.
fn path_option_slots(tail: &[String], prompt: Option<&str>) -> Vec<(usize, usize)> {
    let is_prompt = |i: usize| prompt.is_some_and(|p| tail.get(i).map(String::as_str) == Some(p));
    let mut out = Vec::new();
    let mut i = 0;
    while i < tail.len() {
        let a = tail[i].as_str();
        if is_prompt(i) {
            i += 1;
            continue;
        }
        if a == "--" {
            break;
        }
        // (claude's `-p` is a bare flag: a following `--` still ends the options.)
        if matches!(a, "-p" | "--prompt" | "-i" | "--interactive") {
            if tail.get(i + 1).map(String::as_str) == Some("--") {
                break;
            }
            i += 2;
            continue;
        }
        if PATH_OPTS.contains(&a) {
            if i + 1 < tail.len() && !is_prompt(i + 1) {
                out.push((i + 1, 0));
            }
            i += 2;
            continue;
        }
        if let Some((f, _)) = a.split_once('=') {
            if PATH_OPTS.contains(&f) {
                out.push((i, f.len() + 1));
            }
        }
        i += 1;
    }
    out
}

/// The CLI argv's path-option values ([`path_option_slots`]: the engine's own `--settings`,
/// `--plugin-dir`, `--mcp-config=<file>`, codex's `-C`, ...; never the prompt) mounted read-only
/// when no mount exposes them yet and no mask covers them, then re-spelled at the canonical path
/// their mount sits at (a macOS alias such as `/tmp` does not exist inside).
pub(crate) fn mount_argv_paths(mounts: &mut Vec<Mount>, argv: &mut [String], prompt: Option<&str>) {
    let slots = path_option_slots(argv, prompt);
    let paths: Vec<PathBuf> = slots
        .iter()
        .map(|(i, at)| PathBuf::from(&argv[*i][*at..]))
        .filter(|p| p.is_absolute() && p.exists())
        .collect();
    add_read_roots(mounts, &paths);
    let binds: Vec<PathBuf> = mounts
        .iter()
        .filter(|m| matches!(m, Mount::Bind { .. }))
        .map(|m| m.dst().to_path_buf())
        .collect();
    for (i, at) in slots {
        let value = canonical_spelling(std::ffi::OsStr::new(&argv[i][at..]), &binds)
            .to_string_lossy()
            .into_owned();
        argv[i] = format!("{}{value}", &argv[i][..at]);
    }
}

/// Remove seat container `name` (after its client exited or was killed: a killed client leaves
/// the container running). Best effort and bounded; `--rm` already removed a container whose
/// client saw it exit.
pub(crate) fn reap(seat: &SeatContainer) {
    let rt = Runtime {
        cli: seat.runtime.clone(),
        answer_bound: None,
    };
    let name = seat.name.as_str();
    let mut c = rt.cmd();
    c.args(["rm", "--force", name, &netns_name(name)]);
    let _ = output_within(c, REAP_TIMEOUT);
    // Detach whatever else is attached (the gateway) so the network can go.
    let mut members = rt.cmd();
    members.args([
        "network",
        "inspect",
        "--format",
        "{{range .Containers}}{{.Name}} {{end}}",
        name,
    ]);
    if let Some(o) = output_within(members, PROBE_TIMEOUT).filter(|o| o.status.success()) {
        for m in String::from_utf8_lossy(&o.stdout).split_whitespace() {
            let mut d = rt.cmd();
            d.args(["network", "disconnect", "--force", name, m]);
            let _ = output_within(d, REAP_TIMEOUT);
        }
    }
    // Its own network goes with it. The runtime detaches the container's endpoint asynchronously
    // after `--rm`, so a first removal can find it still in use: retry briefly. An offline seat
    // has no network of its own (`network inspect` fails at once and the loop ends).
    let start = Instant::now();
    loop {
        let mut n = rt.cmd();
        n.args(["network", "rm", name]);
        if output_within(n, REAP_TIMEOUT).is_some_and(|o| o.status.success()) {
            return;
        }
        let mut i = rt.cmd();
        i.args(["network", "inspect", "--format", "{{.Name}}", name]);
        let exists = output_within(i, PROBE_TIMEOUT).is_some_and(|o| o.status.success());
        if !exists || start.elapsed() > REAP_TIMEOUT {
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// The runtime's own prune commands for what a crashed engine left behind: stopped seat
/// containers and unused seat networks (`wicked.seat=1`) older than `older_than`. A live seat
/// is never touched (prune skips running containers and networks in use).
pub(crate) fn sweep_argv(older_than: Duration) -> [Vec<String>; 2] {
    let until = format!("until={}h", older_than.as_secs() / 3600);
    let label = format!("label={SEAT_LABEL}=1");
    [
        vec![
            "container".into(),
            "prune".into(),
            "--force".into(),
            "--filter".into(),
            label.clone(),
            "--filter".into(),
            until.clone(),
        ],
        vec![
            "network".into(),
            "prune".into(),
            "--force".into(),
            "--filter".into(),
            label,
            "--filter".into(),
            until,
        ],
    ]
}

/// Run [`sweep_argv`], best effort and bounded. The first [`arm`] calls it once per process.
pub(crate) fn sweep(rt: &Runtime, older_than: Duration) {
    for argv in sweep_argv(older_than) {
        let mut c = rt.cmd();
        c.args(&argv);
        let _ = output_within(c, rt.bound(REAP_TIMEOUT));
    }
}

/// The runtime CLI's own failure code (`docker run` / `podman run` / `nerdctl run`): the
/// container never started, so nothing of the seat ran.
const RUNTIME_START_FAILED: i32 = 125;

/// A seat client's stderr, with a leading line naming the cause when the container itself never
/// started (exit 125: the image, a mount source the runtime's VM does not share, the engine).
pub(crate) fn explain_start_failure(name: &str, code: i32, err: String) -> String {
    if code != RUNTIME_START_FAILED {
        return err;
    }
    let hint = if err.contains("bind source path does not exist") {
        " — a mounted host path is not visible to the container runtime (share it with the \
         runtime's VM, e.g. Rancher Desktop / Colima / Podman machine mounts)"
    } else {
        ""
    };
    format!(
        "wicked-core: seat container {name} did not start (runtime exit {RUNTIME_START_FAILED}){hint}\n{err}"
    )
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

/// `argv` with the sign-in's route arguments ([`crate::seat_gateway::seat_args_for`]) at its
/// end, before a `--` separator if it has one: the innermost subcommand reads them. codex drops
/// root-level `-c` overrides once its `exec` subcommand carries its own (the engine's do), so a
/// provider placed after the binary was silently ignored (core#989, probed on 0.162.1).
pub(crate) fn with_seat_args(mut argv: Vec<String>, seat_args: Vec<String>) -> Vec<String> {
    if argv.is_empty() {
        return argv;
    }
    let at = argv.iter().position(|a| a == "--").unwrap_or(argv.len());
    argv.splice(at..at, seat_args);
    argv
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

/// One seat's gateway identity: its unit key, its env, and its placeholder credential files.
type SeatCredential = (
    crate::seat_gateway::UnitKey,
    Vec<(String, String)>,
    Vec<(PathBuf, String)>,
);

/// The env variable naming `cli`'s configuration home, where its placeholder files go.
fn config_home_env(cli: &str) -> Option<&'static str> {
    match cli {
        "claude" => Some("CLAUDE_CONFIG_DIR"),
        "codex" => Some("CODEX_HOME"),
        "pi" => Some("PI_CODING_AGENT_DIR"),
        "opencode" => Some("XDG_CONFIG_HOME"),
        "copilot" => Some("COPILOT_HOME"),
        // agy publishes no configuration-home variable: its home IS its configuration home
        // (`~/.gemini`, `~/.config/antigravity`), so the carrier's `HOME` names the seat home
        // ([`carrier_home`] carries it into the container).
        "agy" => Some("HOME"),
        _ => None,
    }
}

// codex lg-r1 #1: anchor every operation to a directory opened without following
// links. A seat renaming a parent cannot redirect an engine write to a host file.
#[cfg(unix)]
fn private_dir(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    if !path.is_absolute() {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    let mut dir = std::fs::File::open("/")?;
    for part in path.components() {
        let std::path::Component::Normal(name) = part else {
            if matches!(part, std::path::Component::RootDir) {
                continue;
            }
            return Err(std::io::ErrorKind::InvalidInput.into());
        };
        let name = std::ffi::CString::new(name.as_bytes())?;
        // SAFETY: the live directory fd and NUL-terminated component remain valid.
        let fd = unsafe {
            libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), 0o700);
            libc::openat(
                dir.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: openat returned a new owned fd.
        dir = unsafe { std::fs::File::from_raw_fd(fd) };
    }
    Ok(dir)
}

#[cfg(unix)]
fn write_private_file(path: &Path, body: &[u8]) -> Result<(), String> {
    use std::io::Write;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    let dir =
        private_dir(path.parent().ok_or("no credential parent")?).map_err(|e| e.to_string())?;
    let name = std::ffi::CString::new(path.file_name().ok_or("no credential name")?.as_bytes())
        .map_err(|e| e.to_string())?;
    let tmp =
        std::ffi::CString::new(format!(".wicked-{}.tmp", uuid::Uuid::new_v4().simple())).unwrap();
    // symlink_metadata refuses the supplied destination; fstatat repeats that check
    // against the pinned directory so parent renames cannot redirect the operation.
    let check = || -> Result<(), String> {
        match std::fs::symlink_metadata(path) {
            Ok(m) if m.file_type().is_file() => {}
            Ok(_) => return Err("credential destination is not a regular file".into()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
        let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: the live directory fd, name and output buffer remain valid.
        let result = unsafe {
            libc::fstatat(
                dir.as_raw_fd(),
                name.as_ptr(),
                metadata.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result == 0 {
            // SAFETY: successful fstatat initialized the buffer.
            let metadata = unsafe { metadata.assume_init() };
            if metadata.st_mode & libc::S_IFMT != libc::S_IFREG {
                return Err("credential destination is not a regular file".into());
            }
        } else if std::io::Error::last_os_error().kind() != std::io::ErrorKind::NotFound {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(())
    };
    check()?;
    // O_CREAT | O_EXCL implements create_new(true) relative to the pinned directory;
    // std OpenOptions has no directory-relative open, and macOS /dev/fd cannot do this.
    // SAFETY: the directory fd and unique name remain valid.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            tmp.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    // SAFETY: openat returned a new owned fd.
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    let result = (|| {
        file.write_all(body).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        check()?;
        // SAFETY: both names and the pinned directory fd are live.
        if unsafe {
            libc::renameat(
                dir.as_raw_fd(),
                tmp.as_ptr(),
                dir.as_raw_fd(),
                name.as_ptr(),
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(())
    })();
    if result.is_err() {
        // SAFETY: this is our exclusively created temporary entry.
        unsafe {
            libc::unlinkat(dir.as_raw_fd(), tmp.as_ptr(), 0);
        }
    }
    result
}

#[cfg(not(unix))]
fn write_private_file(_path: &Path, _body: &[u8]) -> Result<(), String> {
    Err("container credential homes require Unix".into())
}

#[cfg(unix)]
fn read_seed(source: &Path, name: &str) -> anyhow::Result<Option<Vec<u8>>> {
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd};
    let path = source.join(name);
    if !std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_file()) {
        return Ok(None);
    }
    let dir = private_dir(source)?;
    let name = std::ffi::CString::new(name)?;
    // codex lg-r1 #4: a seed swapped for an auth-file link is never followed.
    // SAFETY: the directory fd and NUL-terminated name remain valid.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: openat returned a new owned fd.
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    anyhow::ensure!(
        file.metadata()?.is_file(),
        "config seed is not a regular file"
    );
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() <= 1024 * 1024, "config seed exceeds size limit");
    Ok(Some(bytes))
}

#[cfg(not(unix))]
fn read_seed(_source: &Path, _name: &str) -> anyhow::Result<Option<Vec<u8>>> {
    anyhow::bail!("container config seeds require Unix")
}

fn seed_container_settings(
    cli: wicked_apps_core::spawn::SeatCli,
    source: &Path,
    target: &Path,
) -> anyhow::Result<()> {
    use wicked_apps_core::spawn::SeatCli;
    // codex lg-r1 #4: copy permitted preferences, never an auth file or credential
    // property. CLI configuration can embed keys, so its whole document is not a seed.
    let (name, fields): (&str, &[&str]) = match cli {
        SeatCli::Codex => (
            "config.toml",
            &[
                "model",
                "model_reasoning_effort",
                "model_reasoning_summary",
                "model_verbosity",
                "personality",
            ],
        ),
        SeatCli::Claude => (
            "settings.json",
            &["model", "alwaysThinkingEnabled", "effortLevel"],
        ),
        SeatCli::Pi => (
            "settings.json",
            &[
                "defaultProvider",
                "defaultModel",
                "defaultThinkingLevel",
                "theme",
            ],
        ),
        _ => return Ok(()),
    };
    if target.join(name).exists() {
        return Ok(());
    }
    let Some(bytes) = read_seed(source, name)? else {
        return Ok(());
    };
    let input = std::str::from_utf8(&bytes)?;
    let bytes = if cli == SeatCli::Codex {
        let parsed: toml::Table =
            toml::from_str(input).map_err(|_| anyhow::anyhow!("invalid TOML config seed"))?;
        let selected: toml::Table = parsed
            .into_iter()
            .filter(|(k, v)| {
                fields.contains(&k.as_str()) && (v.is_str() || v.is_bool() || v.is_integer())
            })
            .collect();
        toml::to_string(&selected)?.into_bytes()
    } else {
        let parsed: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(input).map_err(|_| anyhow::anyhow!("invalid JSON config seed"))?;
        let selected: serde_json::Map<String, serde_json::Value> = parsed
            .into_iter()
            .filter(|(k, v)| {
                fields.contains(&k.as_str()) && (v.is_string() || v.is_boolean() || v.is_number())
            })
            .collect();
        serde_json::to_vec(&selected)?
    };
    write_private_file(&target.join(name), &bytes).map_err(anyhow::Error::msg)
}

/// The run's own directory under a container seat root: `<root>/.wicked-container/<run>-<digest>`
/// (the posture leaf goes under it). One definition, so a caller that must clean a run's seat home
/// finds exactly the directory [`container_seat_config`] creates.
pub(crate) fn container_run_dir(root: &Path, run: &str) -> PathBuf {
    let run_name: String = run
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    // The digest avoids sanitized-name collisions across runs.
    use sha2::{Digest, Sha256};
    let suffix = format!("{:x}", Sha256::digest(run.as_bytes()));
    // The seat root is the engine's own worker-home path: resolve its existing ancestors (macOS
    // `/var` -> `/private/var`) so the no-follow walk only refuses links the seat could plant
    // under it.
    let base = crate::worker_sandbox::canon_lenient(root).unwrap_or_else(|| root.to_path_buf());
    base.join(".wicked-container")
        .join(format!("{run_name}-{}", &suffix[..12]))
}

// codex lg-r1 #4: shared only within one run/seat/posture; never seed credentials.
// This per-unit launcher has no run-end hook. Run homes remain for session resume until
// the worker home is cleaned; unit cleanup must not delete another live unit's home.
pub(crate) fn container_seat_config(
    config: wicked_apps_core::spawn::SeatConfig,
    run: &str,
    posture: &str,
) -> anyhow::Result<wicked_apps_core::spawn::SeatConfig> {
    use wicked_apps_core::spawn::SeatConfig;
    let SeatConfig::Isolated {
        cli,
        root: Some(root),
        set,
        strip,
    } = config
    else {
        anyhow::bail!("container seats require an isolated config home");
    };
    let target = container_run_dir(&root, run).join(posture);
    #[cfg(unix)]
    private_dir(&target)?;
    // Explicit non-credential documents supplied by seat homes. Config/auth JSON and TOML
    // can embed keys; they are deliberately not copied wholesale.
    for name in ["AGENTS.md", "CLAUDE.md"] {
        if !target.join(name).exists() {
            if let Some(bytes) = read_seed(&root, name)? {
                write_private_file(&target.join(name), &bytes).map_err(anyhow::Error::msg)?;
            }
        }
    }
    seed_container_settings(cli, &root, &target)?;
    let set = set
        .into_iter()
        .map(|(env, p)| {
            let rel = p.strip_prefix(&root).expect("seat config paths are rooted");
            (env, target.join(rel))
        })
        .collect();
    Ok(SeatConfig::Isolated {
        cli,
        root: Some(target),
        set,
        strip,
    })
}

// agy lg-r1 A1: Claude/Codex auth and Pi models.json have fixed paths under
// their session home. OpenCode accepts OPENCODE_CONFIG, so only it uses per-unit files.
static CONFIG_IN_USE: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

#[derive(Debug)]
pub(crate) struct ConfigHomeGuard(PathBuf);

impl Drop for ConfigHomeGuard {
    fn drop(&mut self) {
        CONFIG_IN_USE
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|p| p != &self.0);
    }
}

pub(crate) fn claim_config_home(cli: &str, home: &Path) -> Result<Option<ConfigHomeGuard>, String> {
    if cli == "opencode" {
        return Ok(None);
    }
    let mut live = CONFIG_IN_USE.lock().unwrap_or_else(|p| p.into_inner());
    if live.iter().any(|p| p == home) {
        return Err(format!("concurrent {cli} unit refused: shared run config home is already in use (fixed credential path)"));
    }
    live.push(home.to_path_buf());
    Ok(Some(ConfigHomeGuard(home.to_path_buf())))
}

pub(crate) fn write_unit_seat_files(
    cmd: &mut Command,
    cli: &str,
    files: &[(PathBuf, String)],
) -> Result<(), String> {
    if cli != "opencode" || files.is_empty() {
        return write_seat_files(cmd, cli, files);
    }
    let env = config_home_env(cli).ok_or("no credential home")?;
    let home = cmd
        .get_envs()
        .find(|(k, _)| *k == env)
        .and_then(|(_, v)| v)
        .map(PathBuf::from)
        .ok_or("no credential home")?;
    let unit = format!(".wicked-units/{}", uuid::Uuid::new_v4().simple());
    let scoped = files
        .iter()
        .map(|(p, b)| (PathBuf::from(&unit).join(p), b.clone()))
        .collect::<Vec<_>>();
    write_seat_files(cmd, cli, &scoped)?;
    let file = home.join("opencode").join(&scoped[0].0);
    cmd.env("OPENCODE_CONFIG", file);
    Ok(())
}

/// Write a seat's placeholder credential `files` into its configuration home as `cmd` names it,
/// private (0600). Refused unless that home is inside the engine's worker home: a placeholder
/// never lands in (or overwrites a real credential in) an operator's own configuration.
pub(crate) fn write_seat_files(
    cmd: &Command,
    cli: &str,
    files: &[(PathBuf, String)],
) -> Result<(), String> {
    if files.is_empty() {
        return Ok(());
    }
    let var = config_home_env(cli)
        .ok_or_else(|| format!("no placeholder credential path is defined for `{cli}`"))?;
    let home = cmd
        .get_envs()
        .find(|(k, _)| *k == var)
        .and_then(|(_, v)| v)
        .map(PathBuf::from)
        .ok_or_else(|| format!("the seat's {var} is not set"))?;
    let base = wicked_apps_core::spawn::worker_home_base().map_err(|e| e.to_string())?;
    let home_c = crate::worker_sandbox::canon_lenient(&home).unwrap_or(home.clone());
    let base_c = crate::worker_sandbox::canon_lenient(&base).unwrap_or(base);
    if !home_c.starts_with(&base_c) {
        return Err(format!(
            "the seat's {var} ({}) is not an engine worker home; its placeholder credential is \
             never written there",
            home.display()
        ));
    }
    for (rel, body) in files {
        if rel.is_absolute()
            || rel
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(format!(
                "placeholder path {} escapes the config home",
                rel.display()
            ));
        }
        // codex lg-r1 #5: XDG names the parent of opencode's config directory.
        let home = if cli == "opencode" {
            home.join("opencode")
        } else {
            home.clone()
        };
        let path = home.join(rel);
        write_private_file(&path, body.as_bytes())?;
    }
    Ok(())
}

/// A seat armed in the container floor: the wrapper, the argv to run after it, the runtime that
/// armed it (by file name: `docker`, `podman`, `nerdctl`). Its `Debug` names the env's
/// variables, never their values (a unit key, a claude seat's pass-through token).
pub(crate) struct ArmedContainer {
    pub(crate) wrapper: Vec<String>,
    pub(crate) argv: Vec<String>,
    pub(crate) tool: &'static str,
    /// Removes the seat's container and network on every path out of the unit (held by the
    /// caller until the unit ends; `run_bounded`'s own reap makes it a no-op on the normal path).
    pub(crate) cleanup: ReapOnDrop,
    /// The env the seat reaches the gateway with (its placeholder key, the model base URLs,
    /// the CONNECT proxy); empty for an offline seat.
    pub(crate) env: Vec<(String, String)>,
    /// Placeholder files, relative to this seat's isolated CLI config home. The runner writes
    /// them; arming never touches a CLI home or a provider credential.
    pub(crate) files: Vec<(PathBuf, String)>,
    /// The seat's gateway registration, released after the unit ([`GatewayLease::release`]).
    pub(crate) lease: Option<GatewayLease>,
    /// A copilot seat on its Copilot route: the account its sign-in may use, as published on
    /// that route (`Some(None)` = not resolved yet); `None` for every other seat. The runner
    /// gives the seat one of its models, or refuses the unit by name (OD-1).
    pub(crate) copilot_account: Option<Option<crate::copilot_account::CopilotAccount>>,
    /// What the seat container mounts, for narrowing a claude seat's fence to it
    /// (`execute_wrapped::narrow_container_fence`).
    pub(crate) view: MountView,
    /// A long prompt the seat reads from a file ([`carry_prompt_in_file`]): the file (the caller
    /// admits it to the seat's own read permissions) and its private directory, removed when the
    /// caller drops it at the end of the unit.
    pub(crate) prompt_file: Option<CarriedPrompt>,
}

/// A long prompt carried in a file (core#1228): `file` is mounted read-only into the seat; the
/// directory goes when this is dropped.
#[derive(Debug)]
pub(crate) struct CarriedPrompt {
    pub(crate) file: PathBuf,
    pub(crate) _dir: RemoveOnDrop,
}

impl std::fmt::Debug for ArmedContainer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let env: Vec<&str> = self.env.iter().map(|(k, _)| k.as_str()).collect();
        f.debug_struct("ArmedContainer")
            .field("wrapper", &self.wrapper)
            .field("argv", &self.argv)
            .field("tool", &self.tool)
            .field("cleanup", &self.cleanup)
            .field("env", &env)
            .field(
                "files",
                &self.files.iter().map(|(p, _)| p).collect::<Vec<_>>(),
            )
            .field("lease", &self.lease)
            .field("copilot_account", &self.copilot_account)
            .field("view", &self.view)
            .field("prompt_file", &self.prompt_file)
            .finish()
    }
}

/// How many times a lease retries an unconfirmed revocation before quarantining the gateway.
const REVOKE_ATTEMPTS: usize = 3;

/// One seat's registration on the daemon's gateway: its key is revoked and its records read
/// when the unit ends.
#[derive(Debug)]
pub(crate) struct GatewayLease {
    runtime: Runtime,
    dir: PathBuf,
    gateway: crate::seat_gateway::Gateway,
    key: String,
    /// `<run>#<ord>@<nonce>`: the identity the gateway's records carry for this execution.
    pub(crate) unit: String,
    since: String,
}

impl GatewayLease {
    /// Revoke the seat's key and return what the gateway recorded for it (model requests:
    /// status, model, tool calls, text, usage — never a credential).
    pub(crate) fn release(mut self) -> Vec<crate::seat_gateway::GatewayRecord> {
        let records = crate::seat_gateway::collect_unit_records(
            &self.runtime,
            &self.gateway,
            &self.since,
            &self.unit,
        );
        self.revoke();
        records
    }

    /// Revoke the key once; evidence collection is optional, revocation is not. A revocation
    /// the gateway does not confirm is retried, and if it still is not confirmed the gateway is
    /// removed (codex gw-r2 P2): no served policy outlives a key the engine meant to revoke. The
    /// next unit starts a fresh gateway with no keys.
    fn revoke(&mut self) {
        let key = std::mem::take(&mut self.key);
        if key.is_empty() {
            return;
        }
        let mut last = String::new();
        for _ in 0..REVOKE_ATTEMPTS {
            match crate::seat_gateway::deregister_live(
                &self.runtime,
                &self.gateway,
                &self.dir,
                &key,
            ) {
                Ok(()) => return,
                Err(e) => last = e,
            }
        }
        let removed = crate::seat_gateway::quarantine(&self.runtime, &self.gateway);
        eprintln!(
            "wicked-core: gateway key for {} not revoked ({last}); gateway {} {}",
            self.unit,
            self.gateway.name,
            if removed {
                "removed"
            } else {
                "could NOT be removed"
            }
        );
    }
}

/// A lease dropped without [`GatewayLease::release`] (an early refusal after arming, a panic
/// while the seat runs) still revokes its key.
impl Drop for GatewayLease {
    fn drop(&mut self) {
        self.revoke();
    }
}

/// Pure handoff used by arm: mint the unit placeholder, render its files, and register exactly
/// what the CLI will send. Keeping the raw key local avoids accidentally registering just the
/// signature segment of a Codex JWT.
/// Whether an online seat of `cli` has a gateway credential route at all (offline seats need
/// none). Which credential it presents is the agent's sign-in, read from the broker at arm time.
fn gateway_route_defined(cli: &str, online: bool) -> Result<(), String> {
    if online
        && !matches!(
            cli,
            "claude" | "codex" | "pi" | "opencode" | "copilot" | "agy"
        )
    {
        return Err(format!(
            "no gateway credential route is defined for seat {cli}"
        ));
    }
    Ok(())
}

fn placeholder_handoff(
    cli: &str,
    sign_in: impl Into<crate::seat_credentials::SeatSignIn>,
    run: &str,
    unit: &str,
) -> SeatCredential {
    let sign_in = sign_in.into();
    let mut key = crate::seat_gateway::UnitKey::new(run, unit, cli);
    let url = sign_in
        .kind
        .map(|kind| crate::seat_gateway::credential_gateway_url(cli, kind))
        .unwrap_or_default();
    let files = crate::seat_credentials::seat_files(cli, &sign_in, &key.key, &url);
    if let Some(kind) = sign_in.kind {
        key.bearer = crate::seat_credentials::sent_bearer(cli, kind, &key.key);
        key.key = key.bearer.clone();
    } else {
        key.bearer = key.key.clone();
    }
    let env = crate::seat_gateway::seat_env_for_sign_in(cli, &sign_in, &key.bearer);
    (key, env, files)
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

/// The longest prompt passed to a seat container as an argument (core#1228). Linux refuses to
/// exec any single argument over `MAX_ARG_STRLEN` (32 pages, 128 KiB): docker-init then fails
/// with "argument list too long" before the CLI starts. A longer prompt rides a file instead
/// ([`carry_prompt_in_file`]), with room to spare for the pointer and the rest of the argv.
pub(crate) const PROMPT_ARGV_MAX: usize = 64 * 1024;

/// Linux's limit on one exec argument (`MAX_ARG_STRLEN`, 32 pages of 4 KiB).
const EXEC_ARG_MAX: usize = 128 * 1024;

/// What all arguments together may take, well under the 2 MiB a default 8 MiB stack gives exec
/// (the environment shares that budget).
const EXEC_ARGS_TOTAL_MAX: usize = 1024 * 1024;

/// `Err` when an argument, or all of them together, is too large for a Linux exec, so the unit
/// is refused with the reason instead of failing inside the container (core#1228).
pub(crate) fn check_exec_args(argv: &[String]) -> Result<(), String> {
    if let Some((i, a)) = argv
        .iter()
        .enumerate()
        .find(|(_, a)| a.len() >= EXEC_ARG_MAX)
    {
        return Err(format!(
            "argument {i} of the seat's command is {} bytes; a container cannot pass an argument \
             of {} bytes or more",
            a.len(),
            EXEC_ARG_MAX
        ));
    }
    // Each argument also takes a pointer in the exec's argv table (8 bytes on 64-bit).
    let total: usize = argv.iter().map(|a| a.len() + 1 + 8).sum::<usize>() + 8;
    if total > EXEC_ARGS_TOTAL_MAX {
        return Err(format!(
            "the seat's command is {total} bytes; a container takes at most {EXEC_ARGS_TOTAL_MAX}"
        ));
    }
    Ok(())
}

/// The argument that stands for a long prompt carried in `file` (`bytes` long). Pi reads a file
/// named `@<path>` into its first message itself; every other CLI is told to read the file.
fn prompt_pointer(cli: &str, file: &Path, bytes: usize) -> Vec<String> {
    if cli == "pi" {
        return vec![
            format!("@{}", file.display()),
            "The attached file is this turn's whole task. Follow it exactly.".to_string(),
        ];
    }
    vec![format!(
        "This turn's task is too long to pass on the command line ({bytes} bytes), so it is in \
         the file {}. Read that whole file now, then do exactly what it says, as if it were \
         this message. Do not answer before you have read all of it.",
        file.display()
    )]
}

/// Write `prompt` to a fresh private directory under the engine's worker home, mount it into
/// the seat read-only, and put a pointer to it where the prompt stood in `argv` (an argument
/// that IS the prompt, or one that carries it glued to a flag). Returns the directory's guard:
/// the caller holds it until the unit ends. Under the worker home because seat config homes are
/// there already, so the runtime's VM shares it; the path is canonical, as every mount is.
pub(crate) fn carry_prompt_in_file(
    cli: &str,
    prompt: &str,
    argv: &mut Vec<String>,
    mounts: &mut Vec<Mount>,
) -> Result<Option<CarriedPrompt>, String> {
    let base = wicked_apps_core::spawn::worker_home_base().map_err(|e| e.to_string())?;
    carry_prompt_in_file_at(&base, cli, prompt, argv, mounts)
}

fn carry_prompt_in_file_at(
    base: &Path,
    cli: &str,
    prompt: &str,
    argv: &mut Vec<String>,
    mounts: &mut Vec<Mount>,
) -> Result<Option<CarriedPrompt>, String> {
    let base = crate::worker_sandbox::canon_lenient(base).unwrap_or_else(|| base.to_path_buf());
    let store = base.join(".wicked-prompts");
    // The store must lie outside everything this seat mounts: under a bind (a worker home
    // configured inside the repository, say) the seat would see other units' prompts, and under
    // a writable one it could move this unit's file away from its cleanup.
    let exposed = |p: &Path| {
        mounts
            .iter()
            .find(|m| p.starts_with(m.dst()) || m.dst().starts_with(p))
            .map(|m| m.dst().to_path_buf())
    };
    if let Some(m) = exposed(&store) {
        return Err(format!(
            "the long prompt's store {} lies under {}, which the seat mounts; set the worker \
             home outside the repository and the seat's roots",
            store.display(),
            m.display()
        ));
    }
    let dir = store.join(uuid::Uuid::new_v4().simple().to_string());
    let guard = RemoveOnDrop(dir.clone());
    let file = dir.join("prompt.md");
    write_private_file(&file, prompt.as_bytes())
        .map_err(|e| format!("the long prompt could not be written to a file: {e}"))?;
    let file = file
        .canonicalize()
        .map_err(|e| format!("the long prompt's file {}: {e}", file.display()))?;
    // Written without following a link (`write_private_file`); checked again where it landed.
    if !mountable(&file) || exposed(&file).is_some() {
        return Err(format!(
            "the long prompt's file {} cannot be mounted into a seat container on its own",
            file.display()
        ));
    }
    let pointer = prompt_pointer(cli, &file, prompt.len());
    let mut carried = false;
    let mut out = Vec::with_capacity(argv.len() + 1);
    for a in argv.drain(..) {
        if a == prompt {
            out.extend(pointer.iter().cloned());
            carried = true;
        } else if a.contains(prompt) {
            // Embedded in a larger argument (`--prompt=<text>`, `"{PROMPT} Be concise."`): the
            // read-the-file sentence takes the prompt's place (pi's `@path` must be an argument
            // of its own, so it is never glued).
            let sentence = prompt_pointer("", &file, prompt.len()).join(" ");
            out.push(a.replace(prompt, &sentence));
            carried = true;
        } else {
            out.push(a);
        }
    }
    *argv = out;
    if !carried {
        return Ok(None); // the argv does not carry the prompt: nothing to hand over (dir removed)
    }
    mounts.push(Mount::Bind {
        path: file.clone(),
        rw: false,
    });
    Ok(Some(CarriedPrompt { file, _dir: guard }))
}

/// Arm the container floor for one governed seat spawn: resolve the floor (the step-0
/// computation), find the runtime, the seat image and the network, and render the wrapper. `Err`
/// names why the unit cannot run in a container; the caller refuses it (fail closed).
/// `read_view` is a team member turn's repository view, mounted read-only ([`add_read_view`]).
/// [`arm_noting`] with no build note (the tests' spelling; unix-only tests).
#[cfg(test)]
#[cfg_attr(not(unix), allow(dead_code))]
#[allow(clippy::too_many_arguments)]
pub(crate) fn arm(
    inp: &crate::worker_sandbox::FloorInputs,
    argv: &[String],
    run_id: &str,
    ord: u32,
    read_roots: &[PathBuf],
    read_view: Option<&Path>,
    prompt: Option<&str>,
    gateway_dir: Option<&Path>,
) -> Result<ArmedContainer, (FloorUnarmed, String)> {
    arm_noting(
        inp,
        argv,
        run_id,
        ord,
        read_roots,
        read_view,
        prompt,
        gateway_dir,
        &mut |_, _| {},
    )
}

/// [`arm`], calling `on_built(image, waited)` as soon as the seat image had to be built for this
/// unit (it was missing), whatever the rest of the arming decides.
#[allow(clippy::too_many_arguments)]
pub(crate) fn arm_noting(
    inp: &crate::worker_sandbox::FloorInputs,
    argv: &[String],
    run_id: &str,
    ord: u32,
    read_roots: &[PathBuf],
    read_view: Option<&Path>,
    prompt: Option<&str>,
    gateway_dir: Option<&Path>,
    on_built: &mut dyn FnMut(&str, Duration),
) -> Result<ArmedContainer, (FloorUnarmed, String)> {
    let cli = argv
        .first()
        .map(|b| wicked_apps_core::spawn::SeatCli::from_binary(b))
        .and_then(|c| c.root_name())
        .filter(|c| seat_package(c).is_some() || image_override(c).is_some())
        .ok_or_else(|| {
            (
                FloorUnarmed::NoSeatImage,
                format!(
                    "no seat image is defined for `{}`",
                    argv.first().map(String::as_str).unwrap_or("")
                ),
            )
        })?;
    // An online seat whose agent has no gateway credential route cannot run: refused before any
    // image is built (an agy judge fallback must not cost a seat image build to be refused).
    gateway_route_defined(cli, gateway_dir.is_some() && !offline_seats())
        .map_err(|e| (FloorUnarmed::GatewayUnavailable, e))?;
    // The worktree's git pointer and metadata are seat-writable: verify them against the git dir
    // the engine pinned from the registered repository before anything resolves (or creates)
    // the git dirs they name.
    verify_linked_worktree(&inp.cwd, inp.trusted_git_dir.as_deref())
        .map_err(|e| (FloorUnarmed::RootUnresolvable, e))?;
    // The engine's launch files, checked at their TRUSTED spelling before anything resolves
    // them: never a symlink (a worker could plant one to bind a host file in, or to make the
    // engine create one), and created exclusively when absent.
    for p in &inp.pinned {
        trust_pin(p).map_err(|e| (FloorUnarmed::RootUnresolvable, e))?;
    }
    let mut spec =
        crate::worker_sandbox::resolve_seat_floor(inp).map_err(|w| (w, String::new()))?;
    // The directory holding the launch files is not writable inside a container: the gate
    // hook runs on the host, so the seat has nothing to write there (and nothing to plant).
    let pin_dirs: Vec<PathBuf> = spec
        .pinned
        .iter()
        .filter_map(|p| p.parent().map(Path::to_path_buf))
        .collect();
    spec.writable.retain(|w| !pin_dirs.contains(w));
    let rt = runtime().map_err(|w| (w, String::new()))?;
    static SWEEP: Once = Once::new();
    SWEEP.call_once(|| sweep(&rt, SWEEP_AGE));
    let (image, built_in) =
        ensure_seat_image_noting(&rt, cli).map_err(|e| (FloorUnarmed::NoSeatImage, e))?;
    if let Some(waited) = built_in {
        on_built(&image, waited);
    }
    let floor_image = ensure_floor_image(&rt)
        .and_then(|t| image_id(&rt, &t).ok_or_else(|| format!("floor image {t} vanished")))
        .map_err(|e| (FloorUnarmed::NoSeatImage, e))?;
    // A seat's only route out is the daemon's model gateway: an engine with no gateway dir (no
    // state home) runs only offline seats (an `--internal` network alone still reaches the VM
    // host's services).
    if !offline_seats() && gateway_dir.is_none() {
        return Err((
            FloorUnarmed::GatewayUnavailable,
            format!("no model gateway is configured; set {NETWORK_ENV}=none for an offline seat"),
        ));
    }
    // The engine's launch files are bound read-only: each must exist before the mounts are
    // rendered (a fresh phase's floor spec is written after arming).
    let mut mounts = mount_plan(&spec).map_err(|e| (FloorUnarmed::RootUnresolvable, e))?;
    add_read_roots(&mut mounts, read_roots);
    let mut argv = argv.to_vec();
    mount_argv_paths(&mut mounts, &mut argv, prompt);
    // A member turn's repository view: read-only, or the seat does not start.
    if let Some(view) = read_view {
        add_read_view(&mut mounts, view).map_err(|e| (FloorUnarmed::RootUnresolvable, e))?;
    }
    cover_gateway_dir(&mut mounts, gateway_dir)
        .map_err(|e| (FloorUnarmed::GatewayUnavailable, e))?;
    // core#1228: a prompt too long for one exec argument rides a private read-only file; the
    // argv carries a short pointer to it. After every other mount is known (the store must lie
    // outside all of them) and before the view the claude fence is narrowed to.
    let prompt_file = match prompt.filter(|p| p.len() > PROMPT_ARGV_MAX) {
        Some(p) => carry_prompt_in_file(cli, p, &mut argv, &mut mounts)
            .map_err(|e| (FloorUnarmed::ArgvTooLong, e))?,
        None => None,
    };
    // What the seat sees, for the claude fence ([`narrow_fenced_dir`]): the final mounts, and
    // the unit's working set among them.
    let working_set: Vec<PathBuf> = std::iter::once(spec.tree.clone())
        .chain(spec.protected.iter().cloned())
        .chain(spec.readmitted.iter().cloned())
        .chain(spec.readmitted_files.iter().cloned())
        .chain(inp.write_roots.iter().cloned())
        .chain(read_roots.iter().cloned())
        .chain(read_view.map(Path::to_path_buf))
        .chain(prompt_file.as_ref().map(|c| c.file.clone()))
        .collect();
    let view = mount_view(&mounts, &working_set);
    // The seat image cannot move a mount (a link where one lands, a declared volume).
    let dests: Vec<PathBuf> = mounts
        .iter()
        .map(|m| m.dst().to_path_buf())
        .chain(std::iter::once(
            inp.cwd.canonicalize().unwrap_or_else(|_| inp.cwd.clone()),
        ))
        .collect();
    let image = inspect_seat_image(&rt, &floor_image, &image)
        .and_then(|f| check_seat_image(&f, &dests).map(|()| f.id))
        .map_err(|e| (FloorUnarmed::NoSeatImage, e))?;
    let name = container_name(cli, run_id, ord);
    let network =
        ensure_seat_network(&rt, &name).map_err(|e| (FloorUnarmed::ContainerRuntimeDown, e))?;
    // Owned from here: whatever happens before the seat runs, its network goes.
    let cleanup = ReapOnDrop(Some(SeatContainer {
        runtime: rt.cli.clone(),
        name: name.clone(),
    }));
    // The seat's only egress: the daemon's gateway, attached to its network as `agw`, with a
    // fresh placeholder key. No gateway dir (an engine with no state home) = no egress at all.
    gateway_route_defined(cli, gateway_dir.is_some() && network != "none")
        .map_err(|e| (FloorUnarmed::GatewayUnavailable, e))?;
    let (env, files, lease, seat_args, copilot_account) =
        match gateway_dir.filter(|_| network != "none") {
            Some(dir) => {
                let down = |e: String| (FloorUnarmed::GatewayUnavailable, e);
                let gw = crate::seat_gateway::ensure_gateway(&rt, dir).map_err(down)?;
                // The broker's one refresher runs beside this daemon's gateway (a credential that
                // expired between runs is renewed, by its own CLI, before the seat starts).
                #[cfg(unix)]
                crate::seat_refresh::start(&rt, &gw, dir);
                // The agent's sign-in (DES §6): the user signed in once in its login container, and
                // the broker holds that credential. The seat's placeholder takes its shape and its
                // requests ride its route; signed out (or expired) = refused before the seat starts.
                let kind = crate::seat_login::seat_sign_in(&rt, &gw, dir, cli)
                    .map_err(|e| (FloorUnarmed::AgentSignedOut, e))?;
                // agy on its Gemini API key: the settings that select it, written by the floor and
                // mounted read-only (the seat cannot point agy at another provider).
                if cli == "agy" {
                    // A sign-in this floor cannot arm reads as signed out; a seat home it
                    // cannot write, as an unresolvable root.
                    let api_key =
                        kind.kind == Some(crate::seat_credentials::CredentialKind::GeminiApiKey);
                    let settings = agy_floor_settings(&inp.seat_dirs, kind.kind).map_err(|e| {
                        let why = if api_key {
                            FloorUnarmed::RootUnresolvable
                        } else {
                            FloorUnarmed::AgentSignedOut
                        };
                        (why, e)
                    })?;
                    mounts.retain(|m| m.dst() != settings.as_path());
                    mounts.push(Mount::Bind {
                        path: settings,
                        rw: false,
                    });
                }
                crate::seat_gateway::attach(&rt, &gw, &network).map_err(down)?;
                // The seat's namespace: a sidecar allows output only to the gateway's own address on
                // its ports (an internal network alone still reaches the VM host's services).
                let gw_ip = gateway_ip(&rt, &gw.name, &network).map_err(down)?;
                start_netns_sidecar(&rt, &name, &network, &gw_ip).map_err(down)?;
                // One execution's identity (codex gw-r1 P3): a retry or a reviewer session of the
                // same unit ordinal never reads another execution's requests as its own.
                let unit = format!(
                    "{run_id}#{ord}@{}",
                    &uuid::Uuid::new_v4().simple().to_string()[..8]
                );
                // The route a sign-in needs on the CLI's own argv (codex + ChatGPT: its provider).
                let seat_args = crate::seat_gateway::seat_args_for(cli, kind.kind);
                let copilot_account = (cli == "copilot"
                    && kind
                        .subscriptions
                        .contains(&crate::seat_credentials::Subscription::Copilot))
                .then(|| crate::seat_gateway::copilot_account(dir));
                let oauth = cli == "claude"
                    && kind.kind == Some(crate::seat_credentials::CredentialKind::ClaudeOauth);
                let (key, mut env, files) = placeholder_handoff(cli, kind, run_id, &unit);
                if oauth {
                    // core#1024: the seat's claude passes the operator's own `claude setup-token`
                    // token through (the gateway relays it unchanged and renews nothing). It
                    // reaches the runtime client's environment only: `--env NAME`, never argv.
                    let token = crate::seat_login::claude_seat_token(&rt, &gw, dir)
                        .map_err(|e| (FloorUnarmed::AgentSignedOut, e))?;
                    // The seat-secret guard checks every unit's settled diff, and the delivery,
                    // for the value this seat now holds (core#1024 follow-up).
                    crate::seat_secret_guard::register(
                        crate::seat_secret_guard::SecretKind::ClaudeSetupToken,
                        &token,
                    );
                    env.push(("CLAUDE_CODE_OAUTH_TOKEN".to_string(), token));
                }
                let since = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs().saturating_sub(1).to_string())
                    .unwrap_or_default();
                // The lease owns the key BEFORE the first registration step (codex gw-r2 P2): a
                // failed persist, publish or readiness check drops it, which revokes the key.
                let lease = GatewayLease {
                    runtime: rt.clone(),
                    dir: dir.to_path_buf(),
                    gateway: gw.clone(),
                    key: key.key.clone(),
                    unit,
                    since,
                };
                crate::seat_gateway::register_live(&rt, &gw, dir, key.clone()).map_err(down)?;
                (env, files, Some(lease), seat_args, copilot_account)
            }
            None => (Vec::new(), Vec::new(), None, Vec::new(), None),
        };
    let user = operator_user();
    let workdir = inp.cwd.canonicalize().unwrap_or_else(|_| inp.cwd.clone());
    let memory = std::env::var(MEMORY_ENV).ok();
    let own_ports: Vec<u16> = spec
        .net
        .iter()
        .filter_map(|g| match g {
            crate::worker_sandbox::NetGrant::Loopback { port } => Some(*port),
            crate::worker_sandbox::NetGrant::AnyHost { .. } => None,
        })
        .collect();
    // Online seats join their sidecar's namespace; an offline seat has no network.
    let seat_network = if network == "none" {
        network.clone()
    } else {
        format!("container:{}", netns_name(&name))
    };
    let mut wrapper = run_argv(&ContainerLaunch {
        runtime: &rt.cli,
        image: &image,
        name: &name,
        network: &seat_network,
        workdir: &workdir,
        user: &user,
        mounts: &mounts,
        floor: &floor_image,
        memory: memory_from(memory.as_deref()),
        gateway_ports: GATEWAY_PORTS,
        own_ports: &own_ports,
    });
    grant_seat_devices(&mut wrapper, cli).map_err(|e| (FloorUnarmed::RootUnresolvable, e))?;
    let argv = with_seat_args(image_argv(&argv, cli), seat_args);
    // Checked on what the container will exec (the seat's own args included); a refusal here
    // drops the lease (revoking its key) and the container's cleanup with it.
    check_exec_args(&argv).map_err(|e| (FloorUnarmed::ArgvTooLong, e))?;
    Ok(ArmedContainer {
        wrapper,
        argv,
        prompt_file,
        tool: tool_name(&rt.cli),
        cleanup,
        env,
        files,
        lease,
        copilot_account,
        view,
    })
}

/// agy's settings (`~/.gemini/antigravity-cli/settings.json`) for the sign-in its seat runs on,
/// written by the floor into the seat's own home (found among its `seat_dirs`) before the seat
/// starts, never through a link a seat planted there (the seat home is seat-writable and
/// persists across a run's units), and returned for a read-only bind over it. The API-key path:
/// `modelProvider: gemini`, which makes agy take `GEMINI_API_KEY` and `GOOGLE_GEMINI_BASE_URL`
/// (its Google-account sign-in is the default and reads its own token instead). `Err` when the
/// seat has no agy home, or its sign-in is not one this floor arms.
pub(crate) fn agy_floor_settings(
    seat_dirs: &[PathBuf],
    kind: Option<crate::seat_credentials::CredentialKind>,
) -> Result<PathBuf, String> {
    let body = match kind {
        Some(crate::seat_credentials::CredentialKind::GeminiApiKey) => {
            serde_json::json!({ "modelProvider": "gemini" })
        }
        _ => return Err("agy's sign-in is not one its container seat can run on yet".into()),
    };
    let dir = seat_dirs
        .iter()
        .find(|d| d.ends_with(".gemini/antigravity-cli"))
        .ok_or("the agy seat has no home of its own; its settings cannot be written")?;
    let path = dir.join("settings.json");
    if !mountable(&path) {
        return Err(format!(
            "agy's settings at {} cannot be mounted",
            path.display()
        ));
    }
    write_private_file(&path, body.to_string().as_bytes())?;
    // Mounted at its canonical spelling, as every mount is.
    path.canonicalize()
        .map_err(|e| format!("agy's settings at {}: {e}", path.display()))
}

/// A linked worktree's `.git` file (which a full-posture seat can rewrite) and the git metadata
/// it leads to (whose files a seat can also write: the gitdir, `objects`, `refs`, `logs` are
/// bound writable) are trusted only against `pinned`, the git dir the ENGINE pinned for this
/// unit from the registered repository at dispatch (`worktree_baseline.git_dir`, see
/// [`crate::worktree_guard::pinned_git_dir`]), never against their own self-consistency (codex
/// r5: a forged `gitdir`/`commondir`/back-pointer triple inside the tree agrees with itself).
/// The pointer must name exactly the pinned dir; the common dir is the pinned dir's
/// `<common>/worktrees/<id>` parent, its `commondir` must lead back to it, and none of the
/// granted shared-state roots may be a link. A linked worktree with no pin is refused. Not a
/// linked worktree (a main checkout, a scratch dir): nothing outside the tree is granted.
pub(crate) fn verify_linked_worktree(cwd: &Path, pinned: Option<&Path>) -> Result<(), String> {
    let Some(root) = cwd
        .ancestors()
        .find(|a| std::fs::symlink_metadata(a.join(".git")).is_ok())
    else {
        return Ok(());
    };
    let dot_git = root.join(".git");
    let meta = std::fs::symlink_metadata(&dot_git).map_err(|e| e.to_string())?;
    let bad = |why: &str| {
        format!(
            "the worktree's git pointer {} {why}; refusing it",
            dot_git.display()
        )
    };
    if meta.file_type().is_symlink() {
        return Err(bad("is a symbolic link"));
    }
    if meta.is_dir() {
        return Ok(());
    }
    let pinned = pinned
        .ok_or_else(|| bad("has no git dir pinned by the engine from the registered repository"))?;
    let is_link = |p: &Path| std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink());
    if is_link(pinned) {
        return Err(bad("leads to a pinned git dir that is a symbolic link"));
    }
    let pinned = pinned
        .canonicalize()
        .map_err(|_| bad("leads to a pinned git dir that is missing"))?;
    let text = std::fs::read_to_string(&dot_git).map_err(|_| bad("is unreadable"))?;
    let raw = text
        .lines()
        .find_map(|l| l.strip_prefix("gitdir:"))
        .map(str::trim)
        .ok_or_else(|| bad("names no gitdir"))?;
    let gitdir = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        root.join(raw)
    };
    let gitdir = gitdir
        .canonicalize()
        .map_err(|_| bad("names a missing gitdir"))?;
    if gitdir != pinned {
        return Err(bad("names a gitdir other than the one the engine pinned"));
    }
    if pinned.parent().and_then(Path::file_name) != Some(std::ffi::OsStr::new("worktrees")) {
        return Err(bad(
            "leads to a pinned dir that is not <repo>/.git/worktrees/<name>",
        ));
    }
    let common = pinned
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| bad("has no common dir"))?
        .to_path_buf();
    let named = std::fs::read_to_string(pinned.join("commondir"))
        .map_err(|_| bad("leads to a gitdir without commondir"))?;
    let named = Path::new(named.trim());
    let named = if named.is_absolute() {
        named.to_path_buf()
    } else {
        pinned.join(named)
    };
    if named.canonicalize().ok().as_deref() != Some(common.as_path()) {
        return Err(bad("leads to a gitdir whose commondir leads elsewhere"));
    }
    let back = std::fs::read_to_string(pinned.join("gitdir"))
        .map_err(|_| bad("leads to a gitdir without a back-pointer"))?;
    let back = Path::new(back.trim())
        .canonicalize()
        .map_err(|_| bad("leads to a gitdir whose back-pointer is missing"))?;
    let ours = dot_git.canonicalize().map_err(|_| bad("is unresolvable"))?;
    if back != ours {
        return Err(bad("leads to another worktree's gitdir"));
    }
    // The shared-state roots the floor grants: each a real entry directly under the common dir
    // (a link there would carry the grant to wherever it points).
    for sub in ["objects", "refs", "logs", "packed-refs", "commondir"] {
        let p = if sub == "commondir" {
            pinned.join(sub)
        } else {
            common.join(sub)
        };
        if is_link(&p) {
            return Err(bad(&format!(
                "leads to a redirected {sub} ({})",
                p.display()
            )));
        }
        if let Ok(c) = p.canonicalize() {
            if c.parent() != p.parent().and_then(|d| d.canonicalize().ok()).as_deref() {
                return Err(bad(&format!(
                    "leads to a redirected {sub} ({})",
                    p.display()
                )));
            }
        }
    }
    Ok(())
}

/// A launch file the seat gets read-only: refused when it is a symlink; created exclusively
/// (`O_EXCL`: a dangling symlink planted at its path makes the creation fail, never follow)
/// when absent.
pub(crate) fn trust_pin(p: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.file_type().is_symlink() => Err(format!(
            "pinned launch file {} is a symlink; refusing it",
            p.display()
        )),
        Ok(m) if m.is_file() => Ok(()),
        Ok(_) => Err(format!("pinned launch file {} is not a file", p.display())),
        Err(_) => {
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(p)
                .map(|_| ())
                .map_err(|e| format!("pinned launch file {}: {e}", p.display()))
        }
    }
}

/// The seat's network sidecar container name.
pub(crate) fn netns_name(seat: &str) -> String {
    format!("{seat}-netns")
}

/// `container`'s address on `network`.
pub(crate) fn gateway_ip(rt: &Runtime, container: &str, network: &str) -> Result<String, String> {
    let mut c = rt.cmd();
    c.args([
        "inspect",
        "--format",
        &format!("{{{{(index .NetworkSettings.Networks \"{network}\").IPAddress}}}}"),
        container,
    ]);
    let o = output_within(c, rt.bound(PROBE_TIMEOUT)).ok_or("inspecting the gateway timed out")?;
    let ip = String::from_utf8_lossy(&o.stdout).trim().to_string();
    if o.status.success() && ip.parse::<std::net::IpAddr>().is_ok() {
        Ok(ip)
    } else {
        Err(format!("the gateway has no address on {network}"))
    }
}

/// The sidecar's argv: on the seat network, only the capabilities iptables needs, and the
/// allowed gateway endpoints as arguments.
pub(crate) fn netns_argv(
    rt: &Path,
    image: &str,
    seat: &str,
    network: &str,
    gw_ip: &str,
) -> Vec<String> {
    let mut v: Vec<String> = [
        "run",
        "-d",
        "--rm",
        "--name",
        &netns_name(seat),
        "--label",
        &format!("{SEAT_LABEL}=1"),
        "--network",
        network,
        "--read-only",
        "--cap-drop",
        "ALL",
        "--cap-add",
        "NET_ADMIN",
        "--cap-add",
        "NET_RAW",
        "--security-opt",
        "no-new-privileges",
        "--pids-limit",
        "16",
        "--memory",
        "32m",
        image,
        "wicked-netns",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    v.extend(GATEWAY_PORTS.iter().map(|p| format!("{gw_ip}:{p}")));
    v.insert(0, rt.to_string_lossy().into_owned());
    v
}

/// Start seat `seat`'s sidecar and wait for its rules to be in place (fail closed).
pub(crate) fn start_netns_sidecar(
    rt: &Runtime,
    seat: &str,
    network: &str,
    gw_ip: &str,
) -> Result<(), String> {
    let image = ensure_netns_image(rt)?;
    let argv = netns_argv(&rt.cli, &image, seat, network, gw_ip);
    let mut c = rt.cmd();
    c.args(&argv[1..]);
    let o = output_within(c, rt.bound(PROBE_TIMEOUT))
        .ok_or("starting the network sidecar timed out")?;
    if !o.status.success() {
        return Err(format!(
            "the network sidecar did not start: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        ));
    }
    let start = Instant::now();
    while start.elapsed() < PROBE_TIMEOUT {
        let mut l = rt.cmd();
        l.args(["logs", &netns_name(seat)]);
        if output_within(l, rt.bound(PROBE_TIMEOUT))
            .is_some_and(|o| String::from_utf8_lossy(&o.stdout).contains("wicked-netns-ready"))
        {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err("the network sidecar's rules did not come up".into())
}

/// Mount the unit's read roots (its governance read roots, its skills delivery) read-only when
/// no mount exposes them yet and no mask covers them. They are readable on the host floor; in a
/// container only what is mounted exists.
/// The gateway's state directory is never seat-visible, wherever the daemon's state home sits
/// (a store beside a checkout would otherwise ride the repository mount): its rendered config
/// holds every live unit's key. Nothing a unit is handed lives under it, so a cover is exact.
/// Laid AFTER the read roots and argv paths, replacing every bind at or under it. The directory
/// is created first (codex gw-r2 P1: the first seat's cover was skipped because the gateway had
/// not created it yet), and a cover that cannot be laid refuses the seat.
pub(crate) fn cover_gateway_dir(
    mounts: &mut Vec<Mount>,
    gateway_dir: Option<&Path>,
) -> Result<(), String> {
    let Some(dir) = gateway_dir else {
        return Ok(());
    };
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("could not create the gateway directory: {e}"))?;
    let dir = dir
        .canonicalize()
        .map_err(|e| format!("could not resolve the gateway directory: {e}"))?;
    if !dir.is_dir() || !mountable(&dir) {
        return Err(format!(
            "the gateway directory {} cannot be covered in a seat",
            dir.display()
        ));
    }
    // Under a seat-WRITABLE tree the seat could rename an ancestor away and recreate the path,
    // leaving the cover over the moved directory while the engine writes the next unit's key
    // into the new one (codex gw-r3 P1): refuse instead of covering.
    if let Some(w) = mounts.iter().find_map(|m| match m {
        Mount::Bind { path, rw: true } if dir.starts_with(path) => Some(path),
        _ => None,
    }) {
        return Err(format!(
            "the daemon's gateway state {} lies inside the seat-writable {}; move the state home out of it",
            dir.display(),
            w.display()
        ));
    }
    mounts.retain(|m| !m.dst().starts_with(&dir));
    mounts.push(Mount::Tmpfs { path: dir });
    Ok(())
}

/// (DES-TEAMING-002 §8.8) Mount a team member turn's repository view READ-ONLY, at its own
/// canonical path, and nothing else: the one bind the view adds. Unlike [`add_read_roots`], which
/// skips a path a mount already exposes, this REFUSES (`Err`, the turn does not run) whenever the
/// view could end up writable or hidden: a writable bind at, above or below it, or a mask over
/// it. A read-only bind already at or under the view (an argv path such as claude's `--add-dir
/// <view>`) is replaced by the one view bind. Returns the path as mounted.
pub(crate) fn add_read_view(mounts: &mut Vec<Mount>, view: &Path) -> Result<PathBuf, String> {
    let p = view
        .canonicalize()
        .map_err(|e| format!("the repository view {} is unreadable: {e}", view.display()))?;
    if !p.is_dir() || !mountable(&p) {
        return Err(format!(
            "the repository view {} cannot be mounted into a seat container",
            p.display()
        ));
    }
    for m in mounts.iter() {
        let dst = m.dst();
        match m {
            Mount::Bind { rw: true, .. } if p.starts_with(dst) || dst.starts_with(&p) => {
                return Err(format!(
                    "the repository view {} overlaps the seat-writable {}; it would not be \
                     read-only",
                    p.display(),
                    dst.display()
                ));
            }
            Mount::NullFile { .. } | Mount::Tmpfs { .. } if p.starts_with(dst) => {
                return Err(format!(
                    "the repository view {} lies under the masked {}",
                    p.display(),
                    dst.display()
                ));
            }
            _ => {}
        }
    }
    mounts.retain(|m| !(matches!(m, Mount::Bind { rw: false, .. }) && m.dst().starts_with(&p)));
    mounts.push(Mount::Bind {
        path: p.clone(),
        rw: false,
    });
    Ok(p)
}

pub(crate) fn add_read_roots(mounts: &mut Vec<Mount>, read_roots: &[PathBuf]) {
    for r in read_roots {
        let Some(p) = crate::worker_sandbox::canon_lenient(r) else {
            continue;
        };
        let masked = mounts
            .iter()
            .any(|m| !matches!(m, Mount::Bind { .. }) && p.starts_with(m.dst()));
        let covered = mounts
            .iter()
            .any(|m| matches!(m, Mount::Bind { .. }) && p.starts_with(m.dst()));
        if p.exists() && mountable(&p) && !masked && !covered {
            mounts.push(Mount::Bind { path: p, rw: false });
        }
    }
}

/// What one seat container shows of the host, for the claude seat's file-tool fence: every
/// mount destination as rendered, and the unit's working set among them (its tree, its git
/// state, the repository boundary, its handed write and read roots, a member turn's view), each
/// with whether the seat can write it. Built from the FINAL mounts, never from the host
/// filesystem: inside the container nothing else of the host exists.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct MountView {
    /// Every mount destination (binds and masks), as mounted.
    pub(crate) mounted: Vec<PathBuf>,
    /// The working roots a bind exposes and no mask covers: `(path as mounted, seat-writable)`.
    pub(crate) roots: Vec<(PathBuf, bool)>,
}

/// The [`MountView`] of `mounts` for the unit whose working set is `working_set` (each path made
/// canonical, as the mounts are). A working path no bind exposes, or one under a mask, is not a
/// root: the seat cannot reach it. Whether a root is writable is the deepest bind over it.
pub(crate) fn mount_view(mounts: &[Mount], working_set: &[PathBuf]) -> MountView {
    let mut roots: Vec<(PathBuf, bool)> = Vec::new();
    for r in working_set {
        let Some(r) = crate::worker_sandbox::canon_lenient(r) else {
            continue;
        };
        let masked = mounts
            .iter()
            .any(|m| !matches!(m, Mount::Bind { .. }) && r.starts_with(m.dst()));
        let rw = mounts
            .iter()
            .filter_map(|m| match m {
                Mount::Bind { path, rw } if r.starts_with(path) => {
                    Some((path.components().count(), *rw))
                }
                _ => None,
            })
            .max_by_key(|(depth, _)| *depth)
            .map(|(_, rw)| rw);
        if let (false, Some(rw)) = (masked, rw) {
            if !roots.iter().any(|(p, _)| *p == r) {
                roots.push((r, rw));
            }
        }
    }
    MountView {
        mounted: mounts.iter().map(|m| m.dst().to_path_buf()).collect(),
        roots,
    }
}

/// How the claude fence over ONE denied directory reads inside a seat container, from `view`
/// alone. Every host path is mounted at the same path, so a host deny over an ancestor of the
/// unit's own tree (crew registers repositories under `~/.wicked/repos`, and `.wicked` is a
/// fenced directory) denies the seat its own worktree: Claude's deny beats any allow.
///
/// - `Ok(None)`: keep the directory's blanket rule. No working root lies strictly under it; or a
///   mount exposes more of it than the view can name — a mount at or above the directory, or one
///   below it on the way to a working root without being inside one (an `--add-dir` of
///   `~/.wicked/repos` exposes every sibling repository): narrowing would need the host listing,
///   so the fence stays closed (the seat is then denied its tree, as before, and told why).
/// - `Ok(Some(entries))`: drop the blanket and deny `entries` instead. For every mount under the
///   directory that is not inside a working root, the shallowest path below the directory that
///   is not on the way to a working root. A working root and its ancestors stay open; every
///   other mounted entry stays denied; what is not mounted does not exist in the seat.
/// - `Err`: the directory IS a seat-writable working root. That is a configuration error, and
///   the launch is refused rather than opening a fenced directory to the seat.
pub(crate) fn narrow_fenced_dir(
    dir: &Path,
    view: &MountView,
) -> Result<Option<Vec<PathBuf>>, String> {
    let d = crate::worker_sandbox::canon_lenient(dir).unwrap_or_else(|| dir.to_path_buf());
    if let Some((r, _)) = view.roots.iter().find(|(r, rw)| *rw && *r == d) {
        return Err(format!(
            "the fenced directory {} is the unit's seat-writable root {}; refusing the launch \
             rather than opening it to the seat (move the repository or the write root out of \
             the fenced directory)",
            dir.display(),
            r.display()
        ));
    }
    if view.mounted.iter().any(|m| d.starts_with(m)) {
        return Ok(None);
    }
    let opened: Vec<&PathBuf> = view
        .roots
        .iter()
        .map(|(r, _)| r)
        .filter(|r| r.starts_with(&d) && **r != d)
        .collect();
    if opened.is_empty() {
        return Ok(None);
    }
    let on_the_way = |p: &Path| opened.iter().any(|r| r.starts_with(p));
    let mut entries: Vec<PathBuf> = Vec::new();
    for m in view
        .mounted
        .iter()
        .filter(|m| m.starts_with(&d) && **m != d)
    {
        if opened.iter().any(|r| m.starts_with(r)) {
            continue;
        }
        if on_the_way(m) {
            eprintln!(
                "[wicked-core] fence.notice the seat container mounts {} under the fenced {} \
                 above the unit's working roots: it exposes entries the mount plan does not name, \
                 so the blanket rule stays (the seat cannot read its tree there)",
                m.display(),
                dir.display()
            );
            return Ok(None);
        }
        let mut p = d.clone();
        for c in m.strip_prefix(&d).into_iter().flat_map(Path::components) {
            p.push(c);
            if !on_the_way(&p) {
                if !entries.contains(&p) {
                    entries.push(p.clone());
                }
                break;
            }
        }
    }
    Ok(Some(entries))
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
    let json = args.iter().any(|a| a == "--json");
    let rest: Vec<String> = args
        .get(1..)
        .unwrap_or_default()
        .iter()
        .filter(|a| *a != "--json")
        .cloned()
        .collect();
    let wanted: Vec<String> = if !rest.is_empty() && !rest.iter().any(|a| a == "all") {
        rest
    } else {
        seat_clis().map(str::to_string).collect()
    };
    let unknown: Vec<&str> = wanted
        .iter()
        .map(String::as_str)
        .filter(|c| seat_package(c).is_none() && image_override(c).is_none())
        .collect();
    let usage =
        "usage: wicked-core seat-image build [<cli>…|all] | seat-image hold [<cli>…|all] | \
                 seat-image status [--json] [<cli>…]";
    match args.first().map(String::as_str) {
        Some("build" | "hold" | "status") if !unknown.is_empty() => {
            eprintln!(
                "wicked-core seat-image: no seat image is defined for {}",
                unknown.join(", ")
            );
            1
        }
        Some("build") => {
            let mut rc = 0;
            // What only the operator can rebuild is held before anything builds (a build can push
            // the disk past the cleanup line): the seat base and their own seat images.
            if crate::image_keep::supported(&rt) {
                let mut first: Vec<String> = configured_base().into_iter().collect();
                first.extend(wanted.iter().filter_map(|c| image_override(c)));
                let present: Vec<&str> = first
                    .iter()
                    .map(String::as_str)
                    .filter(|r| image_present_or_restored(&rt, r))
                    .collect();
                if let Err(e) = crate::image_keep::hold_now(&rt, &present) {
                    eprintln!("not held against image cleanup: {e}");
                    rc = 1;
                }
            }
            match ensure_floor_image(&rt) {
                Ok(tag) => println!("floor: {tag}"),
                Err(e) => {
                    eprintln!("floor: {e}");
                    rc = 1;
                }
            }
            for cli in &wanted {
                // The operator's own image is never built: it is held when it is there.
                if let Some(img) = image_override(cli) {
                    if !image_present(&rt, &img) {
                        eprintln!(
                            "{cli}: the operator's image {img} is missing (WICKED_SEAT_IMAGE_{}); the engine never builds it",
                            cli.to_ascii_uppercase()
                        );
                        rc = 1;
                    } else {
                        println!("{cli}: the operator's image {img} is present");
                    }
                    continue;
                }
                match build_seat_image(&rt, cli) {
                    Ok(tag) => {
                        println!("{cli}: built {tag}");
                        // Held at once: the next seat's build must not leave this one to the
                        // image cleanup.
                        if crate::image_keep::supported(&rt) {
                            if let Err(e) = crate::image_keep::hold_now(&rt, &[&tag]) {
                                eprintln!("{cli}: built, but not held against image cleanup: {e}");
                                rc = 1;
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("{cli}: {e}");
                        rc = 1;
                    }
                }
            }
            // Then every image the seats use that is there now is held (built or not). A runtime
            // that cannot hold images keeps its builds' exit code: holding is said, not failed.
            if crate::image_keep::supported(&rt) {
                if let Err(e) = hold_present(&rt, &wanted) {
                    eprintln!("not held against image cleanup: {e}");
                    rc = 1;
                }
            } else {
                println!(
                    "not held against image cleanup: holders protect images only on the docker CLI (moby); {} is not",
                    rt.cli.display()
                );
            }
            rc
        }
        Some("hold") => match hold_present(&rt, &wanted) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("seat-image hold: {e}");
                1
            }
        },
        Some("status") => {
            let st = match image_status(&rt, &wanted) {
                Ok(st) => st,
                Err(e) => {
                    eprintln!("wicked-core seat-image status: {e}");
                    return 1;
                }
            };
            if json {
                println!("{}", serde_json::to_string(&st).unwrap_or_default());
                return 0;
            }
            println!("runtime: {}", rt.cli.display());
            let word = |present: bool, held: bool| match (present, held, st.hold_supported) {
                (true, true, _) => "present, held",
                (true, false, true) => "present, not held",
                (true, false, false) => "present (holding not supported on this runtime)",
                _ => "missing",
            };
            if let Some(b) = &st.base {
                println!("base: {} {}", b.image, word(b.present, b.held));
            }
            println!(
                "floor: {} {}",
                st.floor.image,
                word(st.floor.present, st.floor.held)
            );
            for s in &st.seats {
                println!("{}: {} {}", s.cli, s.image, word(s.present, s.held));
            }
            if let Some(d) = &st.disk {
                println!(
                    "runtime disk: {}% used ({} of {} bytes free)",
                    d.used_pct, d.avail_bytes, d.total_bytes
                );
            }
            0
        }
        _ => {
            eprintln!("{usage}");
            2
        }
    }
}

/// Hold every image the seats of `clis` use that is on the runtime now (their seat images or the
/// operator's own, the operator base, the floor, netns and checks images), saying each; an image
/// that is not there is said as missing. `Err` when holding is not supported or failed.
fn hold_present(rt: &Runtime, clis: &[String]) -> Result<(), String> {
    if !crate::image_keep::supported(rt) {
        return Err(format!(
            "holders protect images only on the docker CLI (moby); {} is not",
            rt.cli.display()
        ));
    }
    let mut refs: Vec<String> = clis
        .iter()
        .filter_map(|cli| image_override(cli).or_else(|| seat_image_tag(cli, &base_image())))
        .collect();
    refs.extend(configured_base());
    refs.push(floor_image_tag());
    refs.push(netns_image_tag());
    refs.push(crate::checks_container::image_tag());
    let (present, missing): (Vec<&str>, Vec<&str>) = refs
        .iter()
        .map(String::as_str)
        .partition(|r| image_present_or_restored(rt, r));
    for m in &missing {
        println!("missing (not held): {m}");
    }
    crate::image_keep::hold_now(rt, &present)?;
    for p in &present {
        println!("held: {p}");
    }
    Ok(())
}

/// [`BASE_IMAGE_ENV`], when the operator set one.
fn configured_base() -> Option<String> {
    std::env::var(BASE_IMAGE_ENV)
        .ok()
        .map(|b| b.trim().to_string())
        .filter(|b| !b.is_empty())
}

/// One image in [`ImageStatus`].
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImageState {
    pub(crate) image: String,
    pub(crate) present: bool,
    /// A holder container keeps it ([`crate::image_keep`]).
    pub(crate) held: bool,
}

/// One seat CLI's image in [`ImageStatus`].
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SeatImageState {
    pub(crate) cli: String,
    pub(crate) image: String,
    pub(crate) present: bool,
    pub(crate) held: bool,
    /// The operator's own image (`WICKED_SEAT_IMAGE_<CLI>`): the engine never builds it.
    pub(crate) operator_image: bool,
}

/// The runtime VM's disk, read inside a throwaway container (its root is on that disk).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RuntimeDisk {
    pub(crate) total_bytes: u64,
    pub(crate) avail_bytes: u64,
    /// `(total - available) / total`, rounded up: the reading a kubelet's image GC compares to its
    /// threshold.
    pub(crate) used_pct: u64,
}

/// `seat-image status --json`: what the seats need, whether it is there and held, and the disk.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImageStatus {
    pub(crate) runtime: String,
    /// The operator's seat base ([`BASE_IMAGE_ENV`]); `null` when the default base is used.
    pub(crate) base: Option<ImageState>,
    /// Whether the operator base has a recipe the engine can rebuild it from: [`BASE_DOCKERFILE_ENV`]
    /// names a readable file (false with no operator base).
    pub(crate) base_recipe: bool,
    /// Whether this runtime's holders protect its images (the docker CLI; [`crate::image_keep`]).
    pub(crate) hold_supported: bool,
    pub(crate) floor: ImageState,
    pub(crate) seats: Vec<SeatImageState>,
    /// `null` when the probe container could not run (no floor image yet, a runtime error).
    pub(crate) disk: Option<RuntimeDisk>,
    /// Kubernetes' default `imageGCHighThresholdPercent`: past it a kubelet deletes unheld images.
    pub(crate) image_gc_high_pct: u64,
}

/// Parse `df -P -k /` (POSIX, 1024-byte blocks) into a [`RuntimeDisk`].
pub(crate) fn parse_df(out: &str) -> Option<RuntimeDisk> {
    let line = out.lines().nth(1)?;
    let f: Vec<&str> = line.split_whitespace().collect();
    let total: u64 = f.get(1)?.parse().ok()?;
    let avail: u64 = f.get(3)?.parse().ok()?;
    if total == 0 {
        return None;
    }
    let used = total.saturating_sub(avail);
    Some(RuntimeDisk {
        total_bytes: total * 1024,
        avail_bytes: avail * 1024,
        used_pct: (used * 100).div_ceil(total),
    })
}

/// What a status reads from the engine's configuration (the env, unless a test gives it).
pub(crate) struct StatusConfig {
    /// The operator base ([`BASE_IMAGE_ENV`]).
    pub(crate) base: Option<String>,
    /// [`BASE_DOCKERFILE_ENV`].
    pub(crate) recipe: Option<PathBuf>,
    /// Each CLI's operator image (`WICKED_SEAT_IMAGE_<CLI>`).
    pub(crate) overrides: std::collections::HashMap<String, String>,
}

impl StatusConfig {
    fn from_env(clis: &[String]) -> Self {
        StatusConfig {
            base: configured_base(),
            recipe: base_recipe(),
            overrides: clis
                .iter()
                .filter_map(|c| image_override(c).map(|i| (c.clone(), i)))
                .collect(),
        }
    }
}

fn image_status(rt: &Runtime, clis: &[String]) -> Result<ImageStatus, String> {
    image_status_with(rt, clis, &StatusConfig::from_env(clis))
}

/// Whether `recipe` is a regular file this process can read.
fn readable_file(recipe: &Path) -> bool {
    recipe.is_file() && std::fs::File::open(recipe).is_ok()
}

fn image_status_with(
    rt: &Runtime,
    clis: &[String],
    cfg: &StatusConfig,
) -> Result<ImageStatus, String> {
    // The three runtime reads at once (a status answers inside a Nerve Center probe's bound): the
    // holders, the images, and the VM disk (read in a throwaway floor container).
    let floor_tag = floor_image_tag();
    let (holders, listing, disk) = std::thread::scope(|sc| {
        let holders = sc.spawn(|| crate::image_keep::list(rt));
        let disk = sc.spawn(|| runtime_disk(rt, &floor_tag));
        let listing = list_images(rt);
        (
            holders
                .join()
                .unwrap_or_else(|_| Err("listing holders failed".into())),
            listing,
            disk.join().ok().flatten(),
        )
    });
    let listed = listing?;
    // A holder listing that failed is not "nothing is held": the status says it could not tell.
    let holders = holders?;
    let base_image = cfg.base.clone().unwrap_or_else(|| DEFAULT_BASE.to_string());
    // `operator`: a reference the operator wrote (an id, a digest, `docker.io/…`), which the
    // listing may spell differently: inspected alone when the listing has no match.
    let state = |image: &str, operator: bool| -> ImageState {
        let untagged =
            !image.rsplit('/').next().unwrap_or(image).contains(':') && !image.contains('@');
        let id = listed
            .get(image)
            .or_else(|| listed.get(&format!("localhost/{image}")))
            .or_else(|| {
                untagged
                    .then(|| listed.get(&format!("{image}:latest")))
                    .flatten()
            })
            .or_else(|| {
                untagged
                    .then(|| listed.get(&format!("localhost/{image}:latest")))
                    .flatten()
            })
            .cloned()
            .or_else(|| operator.then(|| image_id(rt, image)).flatten());
        ImageState {
            image: image.to_string(),
            present: id.is_some(),
            held: id.is_some_and(|id| crate::image_keep::holds(&holders, image, &id)),
        }
    };
    let floor = state(&floor_tag, false);
    let base = cfg.base.as_deref().map(|b| state(b, true));
    let seats = clis
        .iter()
        .filter_map(|cli| {
            let over = cfg.overrides.get(cli).cloned();
            let image = over.clone().or_else(|| seat_image_tag(cli, &base_image))?;
            let s = state(&image, over.is_some());
            Some(SeatImageState {
                cli: cli.clone(),
                image,
                present: s.present,
                held: s.held,
                operator_image: over.is_some(),
            })
        })
        .collect();
    // A disk reading needs the floor image; one taken while it was missing says nothing.
    let disk = disk.filter(|_| floor.present);
    Ok(ImageStatus {
        runtime: rt.cli.display().to_string(),
        base_recipe: base.is_some() && cfg.recipe.as_deref().is_some_and(readable_file),
        base,
        hold_supported: crate::image_keep::supported(rt),
        floor,
        seats,
        disk,
        image_gc_high_pct: 85,
    })
}

/// Every local image as `repository:tag` → id, in one listing: no per-image inspect, so a missing
/// image costs nothing.
fn list_images(rt: &Runtime) -> Result<std::collections::HashMap<String, String>, String> {
    let mut c = rt.cmd();
    c.args([
        "images",
        "--no-trunc",
        "--format",
        "{{.Repository}}:{{.Tag}}\t{{.ID}}",
    ]);
    let o = output_within(c, rt.bound(PROBE_TIMEOUT))
        .ok_or_else(|| "listing the images timed out".to_string())?;
    if !o.status.success() {
        return Err(format!(
            "listing the images: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&o.stdout)
        .lines()
        .filter_map(|l| {
            let (r, i) = l.split_once('\t')?;
            Some((r.trim().to_string(), valid_image_id(i.trim())?))
        })
        .collect())
}

/// The runtime VM's disk, read by `df` in a throwaway container of the floor image (its root is on
/// that disk); `None` when the floor image is not there or the container cannot run.
fn runtime_disk(rt: &Runtime, floor: &str) -> Option<RuntimeDisk> {
    let mut c = rt.cmd();
    c.args([
        "run",
        "--rm",
        "--network",
        "none",
        "--pull",
        "never",
        "--entrypoint",
        "/busybox",
        floor,
        "df",
        "-P",
        "-k",
        "/",
    ]);
    output_within(c, rt.bound(PROBE_TIMEOUT))
        .filter(|o| o.status.success())
        .and_then(|o| parse_df(&String::from_utf8_lossy(&o.stdout)))
}

/// Native Windows is refused (the floor mounts host paths at the same path, which a Linux
/// container cannot do with Windows paths): no runtime is ever resolved there.
#[cfg(all(test, windows))]
mod windows_tests {
    #[test]
    fn native_windows_never_resolves_a_runtime() {
        assert!(matches!(
            super::runtime(),
            Err(crate::worker_sandbox::FloorUnarmed::ContainerUnsupportedHost)
        ));
    }
}

// The agy seat's carrier: its tarball image, its devices, its home, its stdin, its live run.
#[cfg(all(test, unix))]
#[path = "container_floor_agy_tests.rs"]
mod agy_tests;

// The container floor runs on Unix hosts only (native Windows is refused: `windows_tests`); its
// tests use Unix paths, links and tools throughout.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A runtime stub that logs its argv and answers `image inspect` only for the tags a `build`
    /// made (or `present` names).
    fn image_stub(dir: &Path, present: &[&str]) -> PathBuf {
        // Named `docker`: the engine holds images on the docker CLI only.
        let rt = dir.join("docker");
        let have = dir.join("have");
        std::fs::write(
            &have,
            present.iter().map(|p| format!("{p}\n")).collect::<String>(),
        )
        .unwrap();
        crate::test_env::install_stub(
            &rt,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{log}'\n\
                 case \"$1 $2\" in\n\
                 'image inspect') last=; for a in \"$@\"; do last=$a; done\n\
                   case \"$*\" in *Volumes*) echo null; exit 0 ;; esac\n\
                   [ \"$last\" = sha256:{id} ] && {{ echo sha256:{id}; exit 0; }}\n\
                   grep -qx \"$last\" '{have}' || exit 1\n\
                   echo sha256:{id}; exit 0 ;;\n\
                 esac\n\
                 if [ \"$1\" = build ]; then cat > /dev/null; t=; while [ $# -gt 0 ]; do\n\
                   [ \"$1\" = -t ] && t=$2; shift; done; echo \"$t\" >> '{have}'; fi\n\
                 exit 0\n",
                log = dir.join("calls.log").display(),
                have = have.display(),
                id = "b".repeat(64)
            ),
        );
        rt
    }

    #[test]
    fn a_missing_seat_base_is_rebuilt_from_its_recipe_before_the_seat_image() {
        let dir = c4_temp();
        let recipe = dir.join("base.Dockerfile");
        std::fs::write(&recipe, "FROM node:22-trixie-slim\n").unwrap();
        let rt = Runtime::answering(image_stub(&dir, &[]));
        let pkg = seat_package("claude").unwrap();
        let base = SeatBase {
            image: "seat-base:rust".into(),
            operator: true,
            recipe: Some(recipe.clone()),
        };
        let tag = build_seat_image_on(&rt, "claude", pkg, &base).unwrap();
        let calls = std::fs::read_to_string(dir.join("calls.log")).unwrap();
        let base_build = calls
            .find("-t seat-base:rust -")
            .expect("the base was built");
        let seat_build = calls
            .find(&format!("-t {tag} -"))
            .expect("the seat image was built");
        assert!(base_build < seat_build, "{calls}");
        assert!(calls.contains("--build-arg BASE=seat-base:rust"), "{calls}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_present_seat_base_is_not_rebuilt() {
        let dir = c4_temp();
        let recipe = dir.join("base.Dockerfile");
        std::fs::write(&recipe, "FROM node:22-trixie-slim\n").unwrap();
        let rt = Runtime::answering(image_stub(&dir, &["seat-base:rust"]));
        let pkg = seat_package("codex").unwrap();
        let base = SeatBase {
            image: "seat-base:rust".into(),
            operator: true,
            recipe: Some(recipe.clone()),
        };
        build_seat_image_on(&rt, "codex", pkg, &base).unwrap();
        let calls = std::fs::read_to_string(dir.join("calls.log")).unwrap();
        assert!(!calls.contains("-t seat-base:rust"), "{calls}");
        // The operator base is held (its holder made) before the seat image builds on it.
        let held = calls
            .find("create --name wicked-keep-seat-base-rust-")
            .expect("the base's holder was made");
        let built = calls.find("build ").expect("the seat image was built");
        assert!(held < built, "{calls}");
        assert!(
            calls.contains("--label wicked.keep.image=seat-base:rust"),
            "{calls}"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_missing_base_with_no_recipe_names_the_base_and_the_fix() {
        let dir = c4_temp();
        let rt = dir.join("runtime");
        // Every build fails as a pull of a local-only base does.
        crate::test_env::install_stub(
            &rt,
            "#!/bin/sh\ncase \"$1\" in image) exit 1 ;; build) cat >/dev/null; \
             echo 'FAKE pull access denied for seat-base' >&2; exit 1 ;; esac\nexit 0\n",
        );
        let rt = Runtime::answering(rt);
        let pkg = seat_package("claude").unwrap();
        let base = SeatBase {
            image: "seat-base:rust".into(),
            operator: true,
            recipe: None,
        };
        let e = build_seat_image_on(&rt, "claude", pkg, &base).unwrap_err();
        assert!(e.contains("seat base image seat-base:rust"), "{e}");
        assert!(e.contains(BASE_DOCKERFILE_ENV), "{e}");
        // The default registry base is not the operator's: a failed pull is said as it is.
        let default = SeatBase {
            image: DEFAULT_BASE.into(),
            operator: false,
            recipe: Some(PathBuf::from("/nonexistent")),
        };
        let e = build_seat_image_on(&rt, "claude", pkg, &default).unwrap_err();
        assert!(!e.contains(BASE_DOCKERFILE_ENV), "{e}");
        assert!(e.contains("FAKE pull access denied"), "{e}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn status_reads_present_held_and_missing_images_and_the_disk_in_one_pass() {
        let dir = c4_temp();
        let rt = dir.join("docker");
        let claude = seat_image_tag("claude", DEFAULT_BASE).unwrap();
        let floor = floor_image_tag();
        let id = format!("sha256:{}", "e".repeat(64));
        crate::test_env::install_stub(
            &rt,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{log}'\n\
                 case \"$1\" in\n\
                 images) printf '{claude}\\t{id}\\n{floor}\\t{id}\\n' ;;\n\
                 ps) printf 'wicked-keep-x\\t{claude}\\t{id}\\t1\\n' ;;\n\
                 run) printf 'Filesystem 1024-blocks Used Available Capacity Mounted on\\noverlay 1000 900 100 90%% /\\n' ;;\n\
                 *) exit 1 ;;\n\
                 esac\n",
                log = dir.join("calls.log").display(),
            ),
        );
        let rt = Runtime::answering(rt);
        let cfg = StatusConfig {
            base: None,
            recipe: None,
            overrides: Default::default(),
        };
        let st =
            image_status_with(&rt, &["claude".to_string(), "codex".to_string()], &cfg).unwrap();
        assert!(st.hold_supported);
        assert!(st.floor.present && !st.floor.held);
        assert!(st.seats[0].present && st.seats[0].held, "{:?}", st.seats);
        assert!(!st.seats[1].present && !st.seats[1].held);
        assert_eq!(st.disk.as_ref().unwrap().used_pct, 90);
        let calls = std::fs::read_to_string(dir.join("calls.log")).unwrap();
        // One listing, no per-image inspect (a missing image costs nothing).
        assert!(!calls.contains("image inspect"), "{calls}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn status_says_it_cannot_tell_when_holders_cannot_be_listed_and_reads_operator_references() {
        let dir = c4_temp();
        let rt = dir.join("docker");
        let floor = floor_image_tag();
        let id = format!("sha256:{}", "f".repeat(64));
        let recipe = dir.join("base.Dockerfile");
        crate::test_env::install_stub(
            &rt,
            &format!(
                "#!/bin/sh\n\
                 case \"$1\" in\n\
                 images) printf '{floor}\\t{id}\\n' ;;\n\
                 ps) [ -f '{dir}/ps-fails' ] && {{ echo 'FAKE daemon busy' >&2; exit 1; }}; exit 0 ;;\n\
                 image) echo {id} ;;\n\
                 *) exit 1 ;;\n\
                 esac\n",
                dir = dir.display(),
            ),
        );
        let rt = Runtime::answering(rt);
        let cfg = StatusConfig {
            // A digest reference the listing spells as `<none>`: inspected alone.
            base: Some(format!("op/base@{id}")),
            recipe: Some(recipe.clone()),
            overrides: Default::default(),
        };
        let st = image_status_with(&rt, &[], &cfg).unwrap();
        assert!(st.base.as_ref().unwrap().present, "{:?}", st.base);
        // The recipe does not exist yet: not a recipe the engine can rebuild from.
        assert!(!st.base_recipe);
        std::fs::write(&recipe, "FROM x\n").unwrap();
        assert!(image_status_with(&rt, &[], &cfg).unwrap().base_recipe);
        std::fs::write(dir.join("ps-fails"), "").unwrap();
        let e = image_status_with(&rt, &[], &cfg).unwrap_err();
        assert!(e.contains("FAKE daemon busy"), "{e}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_runtime_disk_reads_as_a_kubelet_reads_it() {
        let out = "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
                   overlay 102625208 40367848 56998148 41% /\n";
        let d = parse_df(out).unwrap();
        assert_eq!(d.total_bytes, 102625208 * 1024);
        assert_eq!(d.avail_bytes, 56998148 * 1024);
        // (total - available) / total, so ext4's reserve counts as used: 45, not df's 41.
        assert_eq!(d.used_pct, 45);
        assert!(parse_df("garbage").is_none());
        assert!(parse_df("h\nx 0 0 0 0% /\n").is_none());
    }

    #[test]
    fn a_missing_seat_image_is_built_and_says_so_and_a_present_one_is_not() {
        let dir = c4_temp();
        let rt = Runtime::answering(image_stub(&dir, &[]));
        let base = SeatBase {
            image: DEFAULT_BASE.into(),
            operator: false,
            recipe: None,
        };
        let (tag, built) = ensure_seat_image_with(&rt, "pi", None, &base).unwrap();
        assert!(built.is_some(), "a missing image is built and noted");
        let (again, built) = ensure_seat_image_with(&rt, "pi", None, &base).unwrap();
        assert_eq!(tag, again);
        assert!(built.is_none(), "a present image is not rebuilt");
        let calls = std::fs::read_to_string(dir.join("calls.log")).unwrap();
        assert_eq!(calls.matches(&format!("-t {tag} -")).count(), 1, "{calls}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn c4_temp() -> PathBuf {
        let base = wicked_apps_core::spawn::hermetic_test_worker_home();
        std::fs::create_dir_all(&base).unwrap();
        let dir = base
            .canonicalize()
            .unwrap()
            .join(format!("c4-{}", uuid::Uuid::new_v4().simple()));
        private_dir(&dir).unwrap();
        dir
    }

    #[test]
    fn fixed_auth_homes_refuse_concurrency_and_guard_releases_on_every_exit() {
        let home = c4_temp();
        for cli in ["claude", "codex", "pi"] {
            let guard = claim_config_home(cli, &home).unwrap();
            assert!(claim_config_home(cli, &home)
                .unwrap_err()
                .contains("already in use"));
            assert!(claim_config_home(cli, &home.join("other-posture")).is_ok());
            drop(guard);
            assert!(claim_config_home(cli, &home).is_ok());
            let _ = std::panic::catch_unwind(|| {
                let _held = claim_config_home(cli, &home).unwrap();
                panic!("injected early exit");
            });
            assert!(claim_config_home(cli, &home).is_ok());
        }
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn opencode_units_keep_separate_placeholders_in_shared_session_home() {
        let _env = crate::test_env::ENV_LOCK
            .read()
            .unwrap_or_else(|p| p.into_inner());
        let home = c4_temp();
        let make = |key: &str| {
            // spawn-audit: test-only — never spawned; the command only carries env for the placeholder-write test.
            let mut cmd = Command::new("opencode");
            cmd.env("XDG_CONFIG_HOME", &home);
            write_unit_seat_files(
                &mut cmd,
                "opencode",
                &[("opencode.json".into(), key.to_string())],
            )
            .unwrap();
            cmd.get_envs()
                .find(|(k, _)| *k == "OPENCODE_CONFIG")
                .and_then(|(_, v)| v)
                .map(PathBuf::from)
                .unwrap()
        };
        let first = make("PLACEHOLDER-first");
        let second = make("PLACEHOLDER-second");
        assert_ne!(first, second);
        assert_eq!(
            std::fs::read_to_string(&first).unwrap(),
            "PLACEHOLDER-first"
        );
        assert_eq!(
            std::fs::read_to_string(&second).unwrap(),
            "PLACEHOLDER-second"
        );
        assert!(claim_config_home("opencode", &home).unwrap().is_none());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn image_build_stdout_is_null_so_login_argv_stays_json_only() {
        let home = c4_temp();
        let runtime = home.join("runtime");
        crate::test_env::install_stub(&runtime, "#!/bin/sh\ncat > /dev/null\n[ /dev/stdout -ef /dev/null ] || exit 77\nprintf 'FAKE-build-progress\n'\n");
        assert_eq!(
            build_image(
                &Runtime::answering(runtime),
                "FAKE-dockerfile",
                "FAKE-tag",
                &[]
            )
            .unwrap(),
            "FAKE-tag"
        );
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn planted_temp_destination_and_parent_links_cannot_redirect_placeholder_writes() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = c4_temp();
        let victim = dir.join("victim");
        std::fs::write(&victim, "FAKE-untouched").unwrap();
        symlink(&victim, dir.join("auth.wicked-new")).unwrap();
        write_private_file(&dir.join("auth.json"), b"PLACEHOLDER").unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "FAKE-untouched");
        assert_eq!(
            std::fs::metadata(dir.join("auth.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        std::fs::remove_file(dir.join("auth.json")).unwrap();
        symlink(&victim, dir.join("auth.json")).unwrap();
        assert!(write_private_file(&dir.join("auth.json"), b"bad").is_err());
        symlink(&dir, dir.join("parent-link")).unwrap();
        assert!(write_private_file(&dir.join("parent-link/other.json"), b"bad").is_err());
        std::fs::create_dir(dir.join("directory.json")).unwrap();
        assert!(write_private_file(&dir.join("directory.json"), b"bad").is_err());
        assert!(!dir.join("other.json").exists());
        assert_eq!(std::fs::read_to_string(victim).unwrap(), "FAKE-untouched");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn container_homes_share_within_run_posture_and_preserve_host_credentials() {
        let _env = crate::test_env::ENV_LOCK
            .read()
            .unwrap_or_else(|p| p.into_inner());
        use wicked_apps_core::spawn::{SeatCli, SeatConfig};
        let dir = c4_temp();
        std::fs::write(dir.join("auth.json"), "FAKE-host-credential").unwrap();
        std::fs::write(dir.join("AGENTS.md"), "permitted instructions").unwrap();
        std::fs::write(
            dir.join("config.toml"),
            "model = 'FAKE-model'\napi_key = 'FAKE-host-key'",
        )
        .unwrap();
        let config = SeatConfig::Isolated {
            cli: SeatCli::Codex,
            root: Some(dir.clone()),
            set: vec![("CODEX_HOME", dir.clone())],
            strip: vec![],
        };
        let first = container_seat_config(config.clone(), "run/one", "full").unwrap();
        let same = container_seat_config(config.clone(), "run/one", "full").unwrap();
        let other = container_seat_config(config.clone(), "run/one", "read-only").unwrap();
        let collision = container_seat_config(config, "run_one", "full").unwrap();
        assert_eq!(first.root(), same.root());
        assert_ne!(first.root(), other.root());
        assert_ne!(first.root(), collision.root());
        let private = first.root().unwrap();
        assert_eq!(
            std::fs::read_to_string(private.join("AGENTS.md")).unwrap(),
            "permitted instructions"
        );
        assert!(!private.join("auth.json").exists());
        let seeded = std::fs::read_to_string(private.join("config.toml")).unwrap();
        assert!(seeded.contains("FAKE-model"));
        assert!(!seeded.contains("api_key"));
        assert!(!seeded.contains("FAKE-host-key"));
        // spawn-audit: test-only — never spawned; the command only carries env for the placeholder-write test.
        let mut cmd = Command::new("unused");
        first.apply(&mut cmd);
        write_seat_files(&cmd, "codex", &[("auth.json".into(), "PLACEHOLDER".into())]).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("auth.json")).unwrap(),
            "FAKE-host-credential"
        );
        assert!(!other.root().unwrap().join("auth.json").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn opencode_placeholder_uses_the_xdg_app_subdirectory() {
        let _env = crate::test_env::ENV_LOCK
            .read()
            .unwrap_or_else(|p| p.into_inner());
        let dir = c4_temp();
        // spawn-audit: test-only — never spawned; the command only carries env for the placeholder-write test.
        let mut cmd = Command::new("unused");
        cmd.env("XDG_CONFIG_HOME", &dir);
        write_seat_files(
            &cmd,
            "opencode",
            &[("opencode.json".into(), "PLACEHOLDER".into())],
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("opencode/opencode.json")).unwrap(),
            "PLACEHOLDER"
        );
        assert!(!dir.join("opencode.json").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn offline_copilot_image_needs_no_credential_kind() {
        assert!(seat_package("copilot").is_some());
        assert!(gateway_route_defined("copilot", false).is_ok());
        for cli in ["claude", "codex", "pi", "opencode", "copilot"] {
            assert!(gateway_route_defined(cli, true).is_ok(), "{cli}");
        }
    }

    #[test]
    fn placeholder_handoff_registers_exact_file_credentials_and_separate_units() {
        use crate::seat_credentials::CredentialKind;
        // core#1024: a claude OAuth seat keeps no record; its unit rides a header of its own and
        // its Authorization is the operator's own token (added at arming, never here).
        let (key, env, files) = placeholder_handoff(
            "claude",
            CredentialKind::ClaudeOauth,
            "test-run",
            "test-unit",
        );
        assert_eq!(
            files,
            vec![(PathBuf::from(".credentials.json"), "{}".to_string())]
        );
        assert!(env.iter().any(|(k, v)| k == "ANTHROPIC_CUSTOM_HEADERS"
            && v == &format!("{}: {}", crate::seat_gateway::UNIT_HEADER, key.key)));
        assert!(env
            .iter()
            .any(|(k, v)| k == "ANTHROPIC_BASE_URL" && v == "http://agw:4005"));
        assert!(!env
            .iter()
            .any(|(k, _)| k == "ANTHROPIC_API_KEY" || k == "CLAUDE_CODE_OAUTH_TOKEN"));
        for (cli, kind, pointer) in [
            (
                "codex",
                CredentialKind::CodexChatgpt,
                "/tokens/access_token",
            ),
            ("codex", CredentialKind::CodexApiKey, "/OPENAI_API_KEY"),
            (
                "pi",
                CredentialKind::ProviderKey,
                "/providers/openai/apiKey",
            ),
            (
                "opencode",
                CredentialKind::ProviderKey,
                "/provider/openai/options/apiKey",
            ),
        ] {
            let (key, env, files) = placeholder_handoff(cli, kind, "test-run", "test-unit-1");
            let (other, _, _) = placeholder_handoff(cli, kind, "test-run", "test-unit-2");
            let credential: serde_json::Value = serde_json::from_str(&files[0].1).unwrap();
            assert_eq!(
                credential.pointer(pointer).unwrap().as_str().unwrap(),
                key.key
            );
            assert_eq!(key.key, key.bearer);
            assert_ne!(key.key, other.key);
            assert!(!format!("{key:?}").contains(&key.bearer));
            let config = crate::seat_gateway::GatewayConfig {
                units: vec![key.clone()],
                ..Default::default()
            };
            let rendered = crate::seat_gateway::render(&config);
            for (route, bind) in config.providers.iter().zip(
                rendered["binds"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .take(config.providers.len()),
            ) {
                let auth = &bind["listeners"][0]["routes"][0]["policies"]["apiKey"];
                if route.owner().is_some_and(|o| o != cli) {
                    // Another agent's route never admits this seat (route ownership).
                    assert_eq!(
                        auth["keys"],
                        serde_json::json!([]),
                        "{cli} on {}",
                        route.port
                    );
                    continue;
                }
                assert_eq!(auth["keys"][0]["key"], key.bearer);
                assert_eq!(auth["keys"][0]["metadata"]["unit"], "test-unit-1");
            }
            if matches!(
                kind,
                CredentialKind::ClaudeOauth | CredentialKind::CodexChatgpt
            ) {
                assert!(!env
                    .iter()
                    .any(|(k, _)| k == "OPENAI_API_KEY" || k == "ANTHROPIC_API_KEY"));
            }
        }
        let (key, env, files) = placeholder_handoff(
            "claude",
            CredentialKind::ClaudeApiKey,
            "test-run",
            "test-unit",
        );
        // An empty record over a stale OAuth placeholder; the key rides the env only.
        assert_eq!(
            files,
            vec![(PathBuf::from(".credentials.json"), "{}".to_string())]
        );
        assert!(env
            .iter()
            .any(|(k, v)| k == "ANTHROPIC_API_KEY" && v == &key.key));
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wicked-cf-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    /// The seat layout of the two parked runs (core#840 run 290a7fee, core#997 run 3d3c96a8): a
    /// clone registered under `<home>/.wicked/repos`, its run worktree and git state inside it,
    /// plus two other mounted entries under `.wicked` that are no working root (a seat dir, a
    /// masked file) and a sibling repository bound read-only.
    #[cfg(unix)]
    fn wicked_repos_view() -> MountView {
        let bind = |p: &str, rw: bool| Mount::Bind {
            path: PathBuf::from(p),
            rw,
        };
        let mounts = vec![
            bind("/h/.wicked/walkthroughs", true),
            bind("/h/.wicked/repos/x", false),
            bind("/h/.wicked/repos/other", false),
            bind("/h/.wicked/repos/x/wicked-worktrees/r", true),
            bind("/h/.wicked/repos/x/.git/worktrees/r", true),
            Mount::NullFile {
                path: PathBuf::from("/h/.wicked/memory.db"),
            },
            Mount::Tmpfs {
                path: PathBuf::from("/h/.ssh"),
            },
        ];
        let working: Vec<PathBuf> = [
            "/h/.wicked/repos/x/wicked-worktrees/r",
            "/h/.wicked/repos/x",
            "/h/.wicked/repos/x/.git/worktrees/r",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        mount_view(&mounts, &working)
    }

    /// The view: every mount destination; the working roots a bind exposes, writable by the
    /// deepest bind over each (the tree is writable inside the read-only clone); a working path
    /// under a mask, or exposed by no bind, is not a root.
    #[cfg(unix)]
    #[test]
    fn the_mount_view_is_the_mounts_and_the_working_roots_they_expose() {
        let v = wicked_repos_view();
        assert_eq!(v.mounted.len(), 7);
        assert_eq!(
            v.roots,
            vec![
                (PathBuf::from("/h/.wicked/repos/x/wicked-worktrees/r"), true),
                (PathBuf::from("/h/.wicked/repos/x"), false),
                (PathBuf::from("/h/.wicked/repos/x/.git/worktrees/r"), true),
            ]
        );
        let masked = mount_view(
            &[
                Mount::Bind {
                    path: PathBuf::from("/h/a"),
                    rw: true,
                },
                Mount::Tmpfs {
                    path: PathBuf::from("/h/a/hidden"),
                },
            ],
            &[PathBuf::from("/h/a/hidden/t"), PathBuf::from("/h/nowhere")],
        );
        assert!(masked.roots.is_empty(), "{masked:?}");
    }

    /// The narrowing over a fenced ANCESTOR of the unit's tree: the blanket goes, and every other
    /// mounted entry under it is denied by name — the seat dir and the masked file directly under
    /// `.wicked`, and the sibling repository one level down (`repos` itself is on the way to the
    /// tree, so it stays open). Nothing inside a working root is listed (the git state, the tree).
    #[cfg(unix)]
    #[test]
    fn a_fenced_ancestor_of_the_tree_is_narrowed_to_its_other_mounted_entries() {
        let v = wicked_repos_view();
        let entries = narrow_fenced_dir(Path::new("/h/.wicked"), &v)
            .unwrap()
            .expect("narrowed");
        assert_eq!(
            entries,
            vec![
                PathBuf::from("/h/.wicked/walkthroughs"),
                PathBuf::from("/h/.wicked/repos/other"),
                PathBuf::from("/h/.wicked/memory.db"),
            ]
        );
        // `repos` and the clone are ancestors of the tree: never denied.
        assert!(!entries
            .iter()
            .any(|e| Path::new("/h/.wicked/repos/x/wicked-worktrees/r").starts_with(e)));
        // Only mounted siblings: with nothing else mounted under `.wicked`, nothing is denied
        // there (an unmounted entry does not exist in the seat).
        let only_tree = mount_view(
            &[Mount::Bind {
                path: PathBuf::from("/h/.wicked/repos/x/wicked-worktrees/r"),
                rw: true,
            }],
            &[PathBuf::from("/h/.wicked/repos/x/wicked-worktrees/r")],
        );
        assert_eq!(
            narrow_fenced_dir(Path::new("/h/.wicked"), &only_tree).unwrap(),
            Some(Vec::new())
        );
    }

    /// A fenced directory with no working root under it keeps its blanket (`.ssh`, which is
    /// masked; `.wicked-crew`, which is not mounted at all), and so does one a mount exposes WHOLE
    /// (a bind at or above it: narrowing it would need the host's listing).
    #[cfg(unix)]
    #[test]
    fn a_fenced_dir_with_no_working_root_under_it_keeps_its_blanket() {
        let v = wicked_repos_view();
        for d in [
            "/h/.ssh",
            "/h/.wicked-crew",
            "/h/.claude",
            "/h/.wicked/repos/x/src",
        ] {
            assert_eq!(narrow_fenced_dir(Path::new(d), &v).unwrap(), None, "{d}");
        }
        let whole = mount_view(
            &[
                Mount::Bind {
                    path: PathBuf::from("/h"),
                    rw: false,
                },
                Mount::Bind {
                    path: PathBuf::from("/h/.wicked/repos/x/wt"),
                    rw: true,
                },
            ],
            &[PathBuf::from("/h/.wicked/repos/x/wt")],
        );
        assert_eq!(
            narrow_fenced_dir(Path::new("/h/.wicked"), &whole).unwrap(),
            None
        );
    }

    /// (codex review of #1042, P2) A bind BELOW the fenced directory that is on the way to a
    /// working root without being inside one (claude's `--add-dir <home>/.wicked/repos`) exposes
    /// entries the mount plan does not name (`repos/other/...`): the blanket stays.
    #[cfg(unix)]
    #[test]
    fn a_bind_above_the_working_roots_under_a_fenced_dir_keeps_its_blanket() {
        let bind = |p: &str, rw: bool| Mount::Bind {
            path: PathBuf::from(p),
            rw,
        };
        let v = mount_view(
            &[
                bind("/h/.wicked/repos", false),
                bind("/h/.wicked/repos/x", false),
                bind("/h/.wicked/repos/x/wicked-worktrees/r", true),
            ],
            &[
                PathBuf::from("/h/.wicked/repos/x/wicked-worktrees/r"),
                PathBuf::from("/h/.wicked/repos/x"),
            ],
        );
        assert_eq!(
            narrow_fenced_dir(Path::new("/h/.wicked"), &v).unwrap(),
            None
        );
    }

    /// A fenced directory that IS a seat-writable working root refuses the launch (a
    /// configuration error), never opens; one that is a READ-ONLY root keeps its blanket.
    #[cfg(unix)]
    #[test]
    fn a_fenced_dir_equal_to_a_writable_root_refuses_the_launch() {
        let v = wicked_repos_view();
        let why = narrow_fenced_dir(Path::new("/h/.wicked/repos/x/wicked-worktrees/r"), &v)
            .expect_err("a fenced writable root is refused");
        assert!(
            why.contains("is the unit's seat-writable root") && why.contains("refusing"),
            "{why}"
        );
        assert_eq!(
            narrow_fenced_dir(Path::new("/h/.wicked/repos/x"), &v).unwrap(),
            None,
            "a read-only root keeps its deny"
        );
    }

    /// C5: a product process (the CLI, crew's daemon) runs governed units in containers unless
    /// the operator chose a floor; `host` is kept as the labelled interim floor.
    #[test]
    fn products_default_the_seat_floor_to_container_and_keep_an_explicit_choice() {
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let saved = std::env::var_os(SEAT_FLOOR_ENV);
        for unset in [None, Some(""), Some("  ")] {
            match unset {
                None => std::env::remove_var(SEAT_FLOOR_ENV),
                Some(v) => std::env::set_var(SEAT_FLOOR_ENV, v),
            }
            assert_eq!(default_seat_floor(), "container");
            assert!(container_mode());
        }
        for chosen in ["host", "container"] {
            std::env::set_var(SEAT_FLOOR_ENV, chosen);
            assert_eq!(default_seat_floor(), chosen);
            assert_eq!(container_mode(), chosen == "container");
            assert_eq!(host_floor_chosen(), chosen == "host");
        }
        // codex c5-r10: an unknown value (a typo) is the host floor with its rule, never an escape.
        std::env::set_var(SEAT_FLOOR_ENV, "contaner");
        assert!(!container_mode() && host_floor_chosen());
        std::env::remove_var(SEAT_FLOOR_ENV);
        assert!(!host_floor_chosen(), "only an unset floor is exempt");
        match saved {
            Some(v) => std::env::set_var(SEAT_FLOOR_ENV, v),
            None => std::env::remove_var(SEAT_FLOOR_ENV),
        }
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
        let dir = tmp("rt");
        let nerdctl = dir.join("nerdctl");
        std::fs::write(&nerdctl, "").unwrap();
        assert_eq!(
            runtime_candidates(Some(nerdctl.to_str().unwrap()), None, |_| None),
            vec![nerdctl.clone()]
        );
        let _ = std::fs::remove_dir_all(&dir);
        // Not found: no silent fallback to another runtime.
        assert!(runtime_candidates(Some("podman"), None, |_| None).is_empty());
        assert!(runtime_candidates(Some("/nonexistent/docker"), None, found).is_empty());
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
    fn every_seat_cli_has_a_package_on_the_one_oss_seat_image_and_a_stable_hashed_tag() {
        assert!(SEAT_DOCKERFILE.contains("FROM ${BASE}"));
        assert!(SEAT_DOCKERFILE.contains("wicked.seat.cli=${CLI}"));
        assert!(SEAT_DOCKERFILE.contains("ENTRYPOINT []"));
        // core#1009: every seat image carries the wicked-ledger its QE unit is pointed at.
        let prefix = crate::qe_acceptance::SEAT_LEDGER_PKG_DIR
            .strip_suffix("/node_modules/wicked-ledger")
            .expect("an npm --prefix install");
        assert!(SEAT_DOCKERFILE.contains(&format!("--prefix {prefix} ")));
        assert!(SEAT_DOCKERFILE.contains("ARG LEDGER_PACKAGE=wicked-ledger@"));
        assert!(
            prefix.starts_with("/opt/"),
            "under a root the seat's landrun policy admits read-only"
        );
        let mut tags = Vec::new();
        for cli in ["claude", "codex", "pi", "opencode", "copilot", "agy"] {
            let pkg = seat_package(cli).unwrap_or_else(|| panic!("{cli} has a package"));
            match pkg {
                SeatPackage::Npm(p) => assert!(p
                    .rsplit_once('@')
                    .is_some_and(|(n, v)| !n.is_empty() && !v.is_empty())),
                SeatPackage::Tarball(_) => assert_eq!(cli, "agy", "only agy is not on npm"),
            }
            assert!(pkg.version().is_some_and(|v| !v.is_empty()), "{cli}");
            let a = seat_image_tag(cli, DEFAULT_BASE).unwrap();
            assert_eq!(a, seat_image_tag(cli, DEFAULT_BASE).unwrap());
            assert!(a.starts_with(&format!("wicked-seat-{cli}:")), "{a}");
            assert_ne!(
                a,
                seat_image_tag(cli, "debian:trixie").unwrap(),
                "base is in the tag"
            );
            tags.push(a.split_once(':').unwrap().1.to_string());
        }
        tags.sort();
        tags.dedup();
        assert_eq!(tags.len(), 6, "the package is in the tag");
        assert!(seat_package("gemini").is_none());
    }

    /// The floor image is static and never runs as a container: the trusted launcher, a static
    /// busybox and landrun built static from source (the release asset links glibc).
    /// Codex ln-r1 (P1): a seat image cannot move a mount: a declared volume, or a link at a
    /// mount destination or any of its ancestors (`/floor`, a worktree's path), is refused.
    #[test]
    fn a_seat_image_that_could_move_a_mount_is_refused() {
        let facts = |links: &[&str], vols: &[&str]| ImageFacts {
            id: "sha256:t".into(),
            symlinks: links.iter().map(PathBuf::from).collect(),
            volumes: vols.iter().map(|v| v.to_string()).collect(),
            healthcheck: false,
        };
        let dests = vec![
            PathBuf::from("/Users/op/wt"),
            PathBuf::from("/Users/op/clone/.git"),
        ];
        // A Debian base's own links (/bin, /lib, /var/run) are not on any mount's path.
        assert!(check_seat_image(&facts(&["/bin", "/lib", "/var/run"], &[]), &dests).is_ok());
        for bad in [
            "/floor",
            "/wicked-canary",
            "/tmp",
            "/home",
            "/Users",
            "/Users/op/wt",
        ] {
            let e = check_seat_image(&facts(&[bad], &[]), &dests).unwrap_err();
            assert!(e.contains("symbolic link"), "{bad}: {e}");
        }
        let e = check_seat_image(&facts(&[], &["/opt/evil"]), &dests).unwrap_err();
        assert!(e.contains("declares a volume"), "{e}");
        // Codex ln-r3 (P1): a health check is runtime-exec'd outside the launcher's Landlock.
        let mut hc = facts(&[], &[]);
        hc.healthcheck = true;
        let e = check_seat_image(&hc, &dests).unwrap_err();
        assert!(e.contains("HEALTHCHECK"), "{e}");
    }

    /// LIVE (`WICKED_CONTAINER_LIVE=1`): codex ln-r1's hostile image (`/floor` -> a dir it then
    /// declares as a volume, with its own `launch`) is refused by the engine's inspection, which
    /// runs only the floor's own busybox over the image as data; the engine's seat image passes.
    #[cfg(unix)]
    #[test]
    fn live_a_hostile_seat_image_is_refused_before_it_runs() {
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        if std::env::var("WICKED_CONTAINER_LIVE").ok().as_deref() != Some("1") {
            eprintln!("skipped: set WICKED_CONTAINER_LIVE=1 with a container runtime to run");
            return;
        }
        let rt = runtime().expect("a container runtime answers");
        let floor = ensure_floor_image(&rt).expect("the floor image");
        let good = ensure_seat_image(&rt, "claude").expect("the claude seat image");
        let dests = vec![PathBuf::from("/Users/op/wt")];
        let f = inspect_seat_image(&rt, &floor, &good).expect("inspected");
        assert!(
            f.symlinks.contains(Path::new("/bin")),
            "the base's own links are seen"
        );
        check_seat_image(&f, &dests).expect("the engine's seat image passes");
        assert!(
            valid_image_id(&f.id).is_some(),
            "the seat runs the inspected id: {}",
            f.id
        );
        let df = "FROM busybox:1.37-musl\nRUN mkdir -p /opt/evil && ln -s /opt/evil /floor && printf '#!/bin/sh\\necho BYPASSED\\n' > /opt/evil/launch && chmod 0755 /opt/evil/launch\nVOLUME [\"/opt/evil\"]\n";
        let tag = "wicked-test-hostile-seat:1";
        build_image(&rt, df, tag, &[]).expect("hostile image built");
        let f = inspect_seat_image(&rt, &floor, tag).expect("inspected");
        assert!(f.symlinks.contains(Path::new("/floor")), "{f:?}");
        let e = check_seat_image(&f, &dests).unwrap_err();
        assert!(e.contains("volume") || e.contains("symbolic link"), "{e}");
        // Codex ln-r3's image: no volume, no link, an active HEALTHCHECK -> refused.
        let df = "FROM busybox:1.37-musl\nHEALTHCHECK --interval=100ms CMD [\"/bin/sh\", \"-c\", \"printf BYPASSED > /wicked-canary/healthcheck-bypass\"]\n";
        let hc_tag = "wicked-test-healthcheck-seat:1";
        build_image(&rt, df, hc_tag, &[]).expect("healthcheck image built");
        let f = inspect_seat_image(&rt, &floor, hc_tag).expect("inspected");
        assert!(f.healthcheck, "{f:?}");
        let e = check_seat_image(&f, &dests).unwrap_err();
        assert!(e.contains("HEALTHCHECK"), "{e}");
        let mut rm = rt.cmd();
        rm.args(["image", "rm", "-f", tag, hc_tag]);
        let _ = output_within(rm, PROBE_TIMEOUT);
    }

    /// Codex ln-r2 (P2): both runtimes' image id spellings are immutable ids; nothing else is.
    #[test]
    fn image_ids_in_both_runtime_spellings_are_accepted() {
        let hex = "a".repeat(64);
        assert_eq!(
            valid_image_id(&format!("sha256:{hex}")),
            Some(format!("sha256:{hex}"))
        );
        assert_eq!(valid_image_id(&hex), Some(hex.clone()));
        for bad in [
            "",
            "sha256:",
            "seat:test",
            &"a".repeat(63),
            &format!("sha256:{}", "g".repeat(64)),
        ] {
            assert_eq!(valid_image_id(bad), None, "{bad}");
        }
    }

    /// Codex gw-r1 (P1): a gateway directory inside a mounted checkout (a daemon whose store sits
    /// beside it) is covered: its rendered config holds every live unit's key.
    #[test]
    fn the_gateway_dir_is_covered_even_inside_a_mounted_checkout() {
        let base = std::env::temp_dir().join(format!("wicked-gw-cover-{}", std::process::id()));
        let gw = base.join("repo/.state/gateway");
        std::fs::create_dir_all(&gw).unwrap();
        let repo = base.join("repo").canonicalize().unwrap();
        let gw = gw.canonicalize().unwrap();
        let mut mounts = vec![
            Mount::Bind {
                path: repo.clone(),
                rw: false,
            },
            Mount::Bind {
                path: gw.clone(),
                rw: false,
            },
            // A descendant bind would reopen the covered subtree.
            Mount::Bind {
                path: gw.join("config"),
                rw: false,
            },
        ];
        cover_gateway_dir(&mut mounts, Some(&gw)).unwrap();
        assert_eq!(
            mounts,
            vec![
                Mount::Bind {
                    path: repo.clone(),
                    rw: false
                },
                Mount::Tmpfs { path: gw.clone() }
            ]
        );
        let before = mounts.clone();
        cover_gateway_dir(&mut mounts, None).unwrap();
        assert_eq!(mounts, before);
        // Codex gw-r2 (P1): the FIRST seat runs before the gateway creates its directory; the
        // cover creates it and is laid anyway.
        let fresh = base.join("repo/.state2/gateway");
        let mut mounts = vec![Mount::Bind {
            path: repo.clone(),
            rw: false,
        }];
        cover_gateway_dir(&mut mounts, Some(&fresh)).unwrap();
        assert_eq!(
            mounts.last(),
            Some(&Mount::Tmpfs {
                path: fresh.canonicalize().unwrap()
            })
        );
        // Codex gw-r3 (P1): under a seat-WRITABLE bind the cover is refused (a renamed ancestor
        // would leave it over the moved directory).
        let mut writable = vec![Mount::Bind {
            path: repo.clone(),
            rw: true,
        }];
        let err = cover_gateway_dir(&mut writable, Some(&gw)).unwrap_err();
        assert!(err.contains("seat-writable"), "{err}");
        // An unmountable spelling refuses instead of skipping the cover.
        let bad = base.join("re,po/gateway");
        assert!(cover_gateway_dir(&mut mounts, Some(&bad)).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// No host path is ever mounted over the trusted launcher or the canary.
    #[test]
    fn the_launcher_and_canary_paths_are_never_mounted_over() {
        for p in ["/floor", "/floor/launch", "/wicked-canary/probe"] {
            assert!(!mountable(Path::new(p)), "{p}");
        }
        assert!(mountable(Path::new("/floorplans/x")));
        let mut mounts = Vec::new();
        add_read_roots(&mut mounts, &[PathBuf::from("/floor")]);
        assert!(mounts.is_empty());
    }

    #[test]
    fn the_floor_image_is_the_static_trusted_launcher() {
        let df = FLOOR_DOCKERFILE;
        assert!(df.contains("FROM scratch"));
        assert!(df.contains("CGO_ENABLED=0"));
        assert!(df.contains("github.com/zouuup/landrun/cmd/landrun@v0.1.17"));
        assert!(df.contains("busybox:1.37-musl"));
        assert!(df.contains(&format!("port={CANARY_PORT}")));
        assert!(df.contains("#!/floor/busybox sh"));
        assert_eq!(floor_image_tag(), floor_image_tag());
        assert!(floor_image_tag().starts_with("wicked-floor:"));
    }

    #[test]
    fn the_run_argv_is_hardened_and_ends_with_the_guard() {
        let mounts = vec![Mount::Bind {
            path: PathBuf::from("/w/tree"),
            rw: true,
        }];
        let w = run_argv(&ContainerLaunch {
            runtime: Path::new("/usr/bin/docker"),
            image: "wicked-seat-claude:abc",
            name: "wicked-seat-claude-r-1-n",
            network: "wicked-seat-claude-r-1-n",
            workdir: Path::new("/w/tree"),
            user: "502:20",
            mounts: &mounts,
            floor: "wicked-floor:t",
            memory: "3g",
            gateway_ports: GATEWAY_PORTS,
            own_ports: &[],
        });
        let has = |pair: &[&str]| w.windows(pair.len()).any(|x| x == pair);
        assert_eq!(w[0], "/usr/bin/docker");
        assert!(has(&["--user", "502:20"]));
        assert!(has(&["--cap-drop", "ALL"]));
        assert!(has(&["--security-opt", "no-new-privileges"]));
        assert!(has(&["--network", "wicked-seat-claude-r-1-n"]));
        assert!(has(&["--mount", "type=bind,src=/w/tree,dst=/w/tree"]));
        assert!(w.contains(&"--read-only".to_string()));
        assert!(has(&["--tmpfs", "/home/seat:rw,exec,mode=1777,size=512m"]));
        assert!(!w
            .iter()
            .any(|a| a == "--privileged" || a.starts_with("--cap-add")));
        assert!(has(&["--memory", "3g"]));
        let img = image_index(&w[1..]).unwrap();
        assert_eq!(w[img + 1], "wicked-seat-claude:abc");
        // The trusted launcher: the floor image mounted read-only, its launcher the entrypoint.
        assert!(has(&[
            "--mount",
            "type=image,src=wicked-floor:t,dst=/floor"
        ]));
        assert!(has(&["--entrypoint", "/floor/launch"]));
        assert_eq!(&w[img + 2..img + 5], &[CANARY_FILE, "--", LANDRUN]);
        assert_eq!(
            &w[w.len() - 5..],
            &["--", SEAT_ENTRY, "--check", CANARY_FILE, "--"]
        );
        assert_eq!(landrun_end(&w[1..], img), Some(w.len() - 6));
    }

    /// (DES-TEAMING-002 §8.8) A member turn's repository view adds exactly ONE mount: a
    /// read-only bind of the view at its own canonical path (rendered `readonly`, Landlock
    /// read+exec only), leaving every other mount as it was. A read-only bind under the view (an
    /// argv `--add-dir <view>`) folds into it.
    #[test]
    fn a_member_view_adds_one_read_only_bind_and_nothing_else() {
        let base = tmp("view-spec");
        let sandbox = base.join("sandbox");
        let view = base.join("member-view");
        let seat_home = base.join("home");
        for d in [&sandbox, &view, &seat_home] {
            std::fs::create_dir_all(d).unwrap();
        }
        // A member turn's floor: its scratch cwd and seat home writable, nothing of any repo.
        let before = vec![
            Mount::Bind {
                path: sandbox.clone(),
                rw: true,
            },
            Mount::Bind {
                path: seat_home.clone(),
                rw: true,
            },
            Mount::Bind {
                path: view.join("src"),
                rw: false,
            },
        ];
        let mut mounts = before.clone();
        let at = add_read_view(&mut mounts, &view).expect("the view mounts");
        assert_eq!(at, view);
        assert_eq!(
            mounts,
            vec![
                before[0].clone(),
                before[1].clone(),
                Mount::Bind {
                    path: view.clone(),
                    rw: false
                },
            ],
            "one read-only bind of the view; the bind under it folded in; nothing else"
        );
        let argv = run_argv(&ContainerLaunch {
            runtime: Path::new("docker"),
            image: "seat",
            name: "wicked-seat-x",
            network: "none",
            workdir: &sandbox,
            user: "501:20",
            mounts: &mounts,
            floor: "floor",
            memory: "3g",
            gateway_ports: GATEWAY_PORTS,
            own_ports: &[],
        });
        let v = view.display().to_string();
        let views: Vec<&String> = argv.iter().filter(|a| a.contains(&v)).collect();
        assert_eq!(
            views,
            vec![
                &format!("type=bind,src={v},dst={v},readonly"),
                &v, // landrun's path, after `--rox`
            ],
            "{argv:?}"
        );
        let rox = argv.iter().position(|a| *a == v).unwrap();
        assert_eq!(
            argv[rox - 1],
            "--rox",
            "Landlock grants it read and exec only"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The view is never mounted where it could be writable or hidden: under or over a writable
    /// bind, or under a mask, the turn is refused (fail closed), never mounted some other way.
    #[test]
    fn a_member_view_that_could_be_writable_or_masked_is_refused() {
        let base = tmp("view-refuse");
        let rw = base.join("rw");
        let inside = rw.join("view");
        let masked = base.join("masked");
        let under_mask = masked.join("view");
        for d in [&inside, &under_mask] {
            std::fs::create_dir_all(d).unwrap();
        }
        let bind = |p: &PathBuf, rw: bool| Mount::Bind {
            path: p.clone(),
            rw,
        };
        let mut m = vec![bind(&rw, true)];
        let e = add_read_view(&mut m, &inside).unwrap_err();
        assert!(e.contains("seat-writable"), "{e}");
        let mut m = vec![bind(&inside.join("sub"), true)];
        std::fs::create_dir_all(inside.join("sub")).unwrap();
        let e = add_read_view(&mut m, &inside).unwrap_err();
        assert!(
            e.contains("seat-writable"),
            "a writable bind inside it: {e}"
        );
        let mut m = vec![Mount::Tmpfs {
            path: masked.clone(),
        }];
        let e = add_read_view(&mut m, &under_mask).unwrap_err();
        assert!(e.contains("masked"), "{e}");
        let mut m = Vec::new();
        assert!(add_read_view(&mut m, &base.join("absent")).is_err());
        assert!(m.is_empty());
        let _ = std::fs::remove_dir_all(&base);
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
        let land = landrun_prefix(&plan, &[3100, 4100], &[]);
        let has = |flag: &str, path: &Path| {
            land.windows(2)
                .any(|w| w[0] == flag && Path::new(&w[1]) == path)
        };
        for p in [
            tree.as_path(),
            home.as_path(),
            Path::new("/home/seat"),
            Path::new("/tmp"),
        ] {
            assert!(has("--rwx", p), "{land:?}");
        }
        for p in [&clone, &pinned] {
            assert!(has("--rox", p), "{land:?}");
            assert!(!has("--rwx", p));
        }
        assert!(has("--ro", &secret));
        assert!(!has("--rwx", &base.join("missing")));
        assert!(land.contains(&"--best-effort".to_string()));
        let ports: Vec<&str> = land
            .windows(2)
            .filter(|w| w[0] == "--connect-tcp")
            .map(|w| w[1].as_str())
            .collect();
        assert_eq!(ports, ["3100", "4100"]);
        assert!(!land.iter().any(|a| a.starts_with("--unrestricted")));
        assert!(!landrun_prefix(&plan, &[], &[]).contains(&"--connect-tcp".to_string()));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn container_memory_defaults_and_honours_an_operator_override() {
        assert_eq!(memory_from(None), "3g");
        assert_eq!(memory_from(Some("  ")), "3g");
        let w = run_argv(&ContainerLaunch {
            runtime: Path::new("docker"),
            image: "img",
            name: "wicked-seat-x",
            network: "none",
            workdir: Path::new("/tmp"),
            user: "1000:1000",
            mounts: &[],
            floor: "wicked-floor:t",
            memory: memory_from(Some(" 6g ")),
            gateway_ports: &[],
            own_ports: &[],
        });
        assert!(w.windows(2).any(|w| w == ["--memory", "6g"]));
        assert_eq!(w[image_index(&w[1..]).unwrap() + 1], "img");
    }

    #[test]
    fn the_sweep_prunes_only_labelled_seat_leftovers_past_the_age() {
        let [c, n] = sweep_argv(SWEEP_AGE);
        assert_eq!(
            c,
            [
                "container",
                "prune",
                "--force",
                "--filter",
                "label=wicked.seat=1",
                "--filter",
                "until=24h"
            ]
        );
        assert_eq!(n[..2], ["network", "prune"]);
        assert!(n.contains(&"label=wicked.seat=1".to_string()));
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

    /// core#989: a sign-in's route arguments go LAST (before a `--`), where codex's innermost
    /// subcommand reads them (a root-level `-c` is dropped once `exec` has its own); an empty
    /// argv stays empty.
    #[test]
    fn seat_args_go_last_before_a_separator() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            with_seat_args(
                s(&["codex", "exec", "x", "--json", "-c", "notify=[]"]),
                s(&["-c", "a=1"])
            ),
            s(&[
                "codex",
                "exec",
                "x",
                "--json",
                "-c",
                "notify=[]",
                "-c",
                "a=1"
            ])
        );
        assert_eq!(
            with_seat_args(
                s(&["codex", "exec", "--json", "--", "-x"]),
                s(&["-c", "a=1"])
            ),
            s(&["codex", "exec", "--json", "-c", "a=1", "--", "-x"])
        );
        assert_eq!(
            with_seat_args(s(&["claude", "-p", "x"]), vec![]),
            s(&["claude", "-p", "x"])
        );
        assert!(with_seat_args(vec![], s(&["-c", "a=1"])).is_empty());
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
                &s(&[
                    "/opt/oc/opencode",
                    "-c",
                    "script",
                    "n",
                    "/opt/oc/opencode",
                    "4096"
                ]),
                "opencode"
            ),
            s(&["opencode", "-c", "script", "n", "opencode", "4096"])
        );
    }

    #[test]
    fn finalize_passes_env_by_name_and_adds_no_mount() {
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
            floor: "wicked-floor:t",
            memory: "3g",
            gateway_ports: GATEWAY_PORTS,
            own_ports: &[],
        });
        // spawn-audit: test-only — renders the wrapper argv; spawned only by the gated live test.
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
        cmd.env(crate::mcp_gate::TOKEN_ENV, "unit-token-never-on-argv");
        cmd.env("MCP_TOOL_TIMEOUT", "660000");
        cmd.env("PATH", "/host/bin");
        cmd.env("HOME", "/host/home");
        cmd.env("TMPDIR", "/host/tmp");
        cmd.env("LANG", "en_US.UTF-8");
        cmd.env_remove("REMOVED_TOKEN");
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
        assert!(!args.iter().any(|a| a.contains("unit-token-never-on-argv")));
        assert!(!args
            .iter()
            .any(|a| a == "/host/home" || a == "/host/tmp" || a == "en_US.UTF-8"));
        assert!(!has(&["--env", "REMOVED_TOKEN"]));
        let img = image_index(&args).unwrap();
        let land_end = landrun_end(&args, img).unwrap();
        let has_in = |part: &[String], pair: &[&str]| part.windows(2).any(|w| w == pair);
        for key in [
            "SECRET_TOKEN",
            crate::mcp_gate::TOKEN_ENV,
            "MCP_TOOL_TIMEOUT",
            crate::gate_hook::READ_ROOTS_ENV,
            crate::gate_hook::WRITE_ROOTS_ENV,
        ] {
            assert!(
                has_in(&args[..img], &["--env", key]),
                "runtime receives {key}"
            );
            assert!(
                has_in(&args[img + 3..land_end], &["--env", key]),
                "landrun receives {key}"
            );
        }
        assert!(
            !has_in(&args[..img], &["--env", "PATH"]),
            "the PATH is never passed by name (its host value)"
        );
        assert!(
            args[..img]
                .iter()
                .any(|a| a == &format!("PATH={IMAGE_PATH}")),
            "the seat's PATH = mounted carrier entries + the image's: {args:?}"
        );
        assert!(
            has_in(&args[img + 3..land_end], &["--env", "PATH"]),
            "landrun forwards the IMAGE path"
        );
        for key in ["HOME", "TMPDIR", "LANG"] {
            assert_eq!(
                args[img + 3..land_end]
                    .windows(2)
                    .filter(|w| *w == ["--env", key])
                    .count(),
                1
            );
        }
        assert!(has_in(&args[..img], &["--env", "LANG"]));
        assert!(!has_in(&args[..img], &["--env", "HOME"]));
        assert!(!has_in(&args[..img], &["--env", "TMPDIR"]));
        assert_eq!(
            &args[land_end + 5..],
            &[
                "claude",
                "-p",
                "/etc/passwd",
                "--settings",
                settings.to_str().unwrap()
            ]
        );
        let ro = |p: &Path| format!("type=bind,src={p},dst={p},readonly", p = p.display());
        // Read roots and argv path options are mounted at arm time ([`add_read_roots`],
        // [`mount_argv_paths`]), never here (#1042: past the claude seat's narrowed fence).
        assert!(!has(&["--mount", &ro(&skills)]), "{args:?}");
        assert!(!has(&["--mount", &ro(&settings)]), "{args:?}");
        // A prompt word that happens to be a path is not a mount.
        assert!(!args.iter().any(|a| a.contains("dst=/etc/passwd")));
        assert!(!has_in(
            &args[img + 3..land_end],
            &["--rox", skills.to_str().unwrap()]
        ));
        assert_eq!(
            args.iter().filter(|a| *a == "--mount").count(),
            2,
            "the tree and the floor image only: {args:?}"
        );
        assert_eq!(
            seat_container(&fin),
            Some(SeatContainer {
                runtime: PathBuf::from("docker"),
                name: "wicked-seat-x".into()
            })
        );
        // Not one of ours: unchanged.
        // spawn-audit: test-only — never spawned; checks a non-seat command is left unchanged.
        let mut plain = Command::new("claude");
        plain.args(["run", "--name", "x"]);
        assert!(seat_container(&plain).is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn the_runtime_cli_runs_with_its_own_dir_first_on_path() {
        let p = client_path(
            Path::new("/u/.rd/bin/docker"),
            Some(std::ffi::OsStr::new("/usr/bin:/u/.rd/bin:/bin")),
        )
        .unwrap();
        assert_eq!(p, std::ffi::OsString::from("/u/.rd/bin:/usr/bin:/bin"));
        assert!(client_path(Path::new("docker"), None).is_none());
    }

    #[test]
    fn a_container_that_never_started_is_named_as_such() {
        let e = explain_start_failure(
            "wicked-seat-x",
            125,
            "docker: Error response from daemon: invalid mount config for type \"bind\": bind \
             source path does not exist: /opt/x"
                .into(),
        );
        assert!(
            e.starts_with("wicked-core: seat container wicked-seat-x did not start"),
            "{e}"
        );
        assert!(e.contains("not visible to the container runtime"), "{e}");
        assert!(
            e.ends_with("/opt/x"),
            "the runtime's own words are kept: {e}"
        );
        assert_eq!(
            explain_start_failure("x", 1, "cli error".into()),
            "cli error"
        );
    }

    /// LIVE (needs a container runtime; `WICKED_CONTAINER_LIVE=1`, skipped otherwise): a seat
    /// container rendered from a seat floor, run for real. Inside it: the operator's uid, no
    /// capabilities, no-new-privileges, a read-only root, the tree writable (the write lands on
    /// the host owned by the operator), the clone read-only, a masked secret empty, a sibling of
    /// the tree absent, and no network route out. The reap removes the container and its network.
    #[cfg(unix)]
    #[test]
    fn live_a_seat_container_is_the_floor() {
        if std::env::var("WICKED_CONTAINER_LIVE").ok().as_deref() != Some("1") {
            eprintln!("skipped: set WICKED_CONTAINER_LIVE=1 with a container runtime to run");
            return;
        }
        let rt = runtime().expect("a container runtime answers");
        let floor = ensure_floor_image(&rt).expect("the floor image");
        let image = ensure_seat_image(&rt, "claude").expect("the claude seat image");
        let base = tmp("live");
        let clone = base.join("clone");
        let tree = base.join("wt");
        let sibling = base.join("wt-other");
        let home = base.join("home");
        let ssh = home.join(".ssh");
        for d in [&clone, &tree, &sibling, &home, &ssh] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(clone.join("README"), "clone").unwrap();
        std::fs::write(sibling.join("other"), "sibling").unwrap();
        std::fs::write(ssh.join("id_ed25519"), "FAKE-SECRET-CANARY").unwrap();
        // A mask outside every mount: rendered anyway (a late read path could be its ancestor).
        let outside_mask = base.join("elsewhere").join(".aws");
        std::fs::create_dir_all(&outside_mask).unwrap();
        let token = home.join(".token");
        std::fs::write(&token, "FAKE-TOKEN-CANARY").unwrap();
        let spec = SeatFloor {
            profile: "governed".into(),
            tree: tree.clone(),
            writable: vec![tree.clone(), home.clone()],
            protected: vec![clone.clone()],
            readmitted: vec![],
            readmitted_files: vec![],
            masks: vec![ssh.clone(), token.clone(), outside_mask.clone()],
            pinned: vec![],
            net: vec![],
        };
        let mounts = mount_plan(&spec).unwrap();
        let name = container_name("claude", "live", 1);
        let network = ensure_seat_network(&rt, &name).expect("the seat network");
        let user = operator_user();
        let mut w = run_argv(&ContainerLaunch {
            runtime: &rt.cli,
            image: &image,
            name: &name,
            network: &network,
            workdir: &tree,
            user: &user,
            mounts: &mounts,
            floor: &floor,
            memory: "3g",
            gateway_ports: GATEWAY_PORTS,
            own_ports: &[],
        });
        let script = format!(
            "id -u; id -g; grep -E '^(CapEff|NoNewPrivs)' /proc/self/status; \
             echo in-tree > {t}/wrote && echo TREE_OK; \
             (echo x > {c}/x) 2>/dev/null && echo CLONE_WRITABLE || echo CLONE_RO; \
             (echo x > /usr/x) 2>/dev/null && echo ROOT_WRITABLE || echo ROOT_RO; \
             echo SECRET=[$(cat {k}/id_ed25519 2>/dev/null)] TOKEN=[$(cat {tok} 2>/dev/null)]; \
             test -e {s}/other && echo SIBLING_VISIBLE || echo SIBLING_ABSENT; \
             (curl -sS --max-time 4 -o /dev/null https://example.com 2>/dev/null) && echo NET_OUT || echo NET_BLOCKED; \
             echo hello > /tmp/scratch && echo TMP_OK; \
             (echo x > /wicked-canary/outside) 2>/dev/null && echo LANDLOCK_ESCAPED || echo LANDLOCK_DENIED",
            t = tree.display(),
            c = clone.display(),
            k = ssh.display(),
            tok = token.display(),
            s = sibling.display(),
        );
        w.extend(["sh".into(), "-c".into(), script]);
        // spawn-audit: test-only — renders the wrapper argv; spawned only by the gated live test.
        let mut cmd = Command::new(&w[0]);
        cmd.args(&w[1..]);
        let out = output_within(cmd, Duration::from_secs(60)).expect("the seat ran");
        reap(&SeatContainer {
            runtime: rt.cli.clone(),
            name: name.clone(),
        });
        let so = String::from_utf8_lossy(&out.stdout).to_string();
        let se = String::from_utf8_lossy(&out.stderr).to_string();
        eprintln!("--- seat stdout ---\n{so}--- seat stderr ---\n{se}");
        assert!(out.status.success(), "{so}\n{se}");
        assert!(
            se.contains("wicked-seat-launch: floor canary passed"),
            "{se}"
        );
        let (uid, gid) = user.split_once(':').unwrap();
        let lines: Vec<&str> = so.lines().collect();
        assert_eq!(lines.first().copied(), Some(uid), "{so}");
        assert_eq!(lines.get(1).copied(), Some(gid), "{so}");
        assert!(so.contains("CapEff:\t0000000000000000"), "{so}");
        assert!(so.contains("NoNewPrivs:\t1"), "{so}");
        for want in [
            "TREE_OK",
            "CLONE_RO",
            "ROOT_RO",
            "SECRET=[] TOKEN=[]",
            "SIBLING_ABSENT",
            "NET_BLOCKED",
            "TMP_OK",
            "LANDLOCK_DENIED",
        ] {
            assert!(so.contains(want), "{want} missing:\n{so}");
        }
        assert_eq!(
            std::fs::read_to_string(tree.join("wrote")).unwrap().trim(),
            "in-tree"
        );
        {
            use std::os::unix::fs::MetadataExt;
            let md = std::fs::metadata(tree.join("wrote")).unwrap();
            assert_eq!(
                md.uid().to_string(),
                uid,
                "the write lands owned by the operator"
            );
        }
        assert!(!clone.join("x").exists());
        // The reap removed the container and its network.
        let mut ps = rt.cmd();
        ps.args(["container", "inspect", &name]);
        assert!(!output_within(ps, PROBE_TIMEOUT).unwrap().status.success());
        let mut ni = rt.cmd();
        ni.args(["network", "inspect", &name]);
        assert!(!output_within(ni, PROBE_TIMEOUT).unwrap().status.success());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The seat's floor plan for a Rust-capable seat (claude here; every CLI gets the same cache
    /// table) under a worker home at `worker`, with `tree` its writable tree: the plan, its
    /// resolved floor's mounts, and the wrapper [`finalize`] completes from the plan's env plus
    /// `extra_env`, running `script`. The caller holds the env lock.
    #[allow(clippy::too_many_arguments)]
    fn cargo_seat(
        worker: &Path,
        tree: &Path,
        read_roots: &[PathBuf],
        runtime: &Path,
        image: &str,
        name: &str,
        network: &str,
        floor: &str,
        memory: &str,
        extra_env: &[(String, String)],
        script: &str,
    ) -> (crate::worker_sandbox::SeatFloorPlan, Vec<Mount>, Command) {
        use wicked_apps_core::spawn::{SeatCli, SeatConfig, WORKER_HOME_ENV};
        let prior = std::env::var_os(WORKER_HOME_ENV);
        std::env::set_var(WORKER_HOME_ENV, worker);
        let config = SeatConfig::Isolated {
            cli: SeatCli::Claude,
            root: None,
            set: Vec::new(),
            strip: Vec::new(),
        };
        let plan = crate::worker_sandbox::plan_seat_floor(
            crate::worker_sandbox::SeatLaunch {
                cwd: tree,
                cli: SeatCli::Claude,
                config: &config,
                read_only: false,
                scratch: None,
                gov_log_dir: None,
                own_ports: &[],
            },
            vec![tree.to_path_buf()],
        );
        match prior {
            Some(v) => std::env::set_var(WORKER_HOME_ENV, v),
            None => std::env::remove_var(WORKER_HOME_ENV),
        }
        let spec = crate::worker_sandbox::resolve_seat_floor(&plan.inputs).unwrap();
        let mut mounts = mount_plan(&spec).unwrap();
        add_read_roots(&mut mounts, read_roots);
        let w = run_argv(&ContainerLaunch {
            runtime,
            image,
            name,
            network,
            workdir: tree,
            user: &operator_user(),
            mounts: &mounts,
            floor,
            memory,
            gateway_ports: GATEWAY_PORTS,
            own_ports: &[],
        });
        // spawn-audit: test-only — renders the wrapper argv; spawned only by the gated live test.
        let mut cmd = Command::new(&w[0]);
        cmd.args(&w[1..]);
        cmd.args(["sh", "-c", script]);
        for (k, v) in &plan.env {
            cmd.env(k, v);
        }
        for (k, v) in &plan.build_env {
            cmd.env(k, v);
        }
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        (plan, mounts, finalize(cmd))
    }

    /// A Rust-capable seat's cargo cache is the floor's persistent per-seat cache dir on the host,
    /// never the 512 MB tmpfs HOME: the plan points `CARGO_HOME` at `<worker home>/cache/<cli>/cargo`,
    /// the mount plan binds that cache dir read-write at its host path, and the finalized wrapper
    /// passes `CARGO_HOME` by name to the runtime AND to landrun, which admits the bind read-write
    /// (landrun keeps only the variables it is told to: a value the runtime alone got would be
    /// dropped and cargo would fall back to `$HOME/.cargo`). The value is never on the argv.
    #[test]
    fn a_rust_capable_seats_cargo_cache_is_a_persistent_bind_outside_the_tmpfs_home() {
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let base = tmp("cargo-cache");
        let tree = base.join("wt");
        let worker = base.join("worker");
        std::fs::create_dir_all(&tree).unwrap();
        let (plan, mounts, cmd) = cargo_seat(
            &worker,
            &tree,
            &[],
            Path::new("docker"),
            "img:1",
            "wicked-seat-cargo",
            "none",
            "wicked-floor:t",
            "3g",
            &[],
            "cargo fetch",
        );
        let cache = worker.join("cache").join("claude");
        let cargo_home = cache.join("cargo");
        // The table's cargo row is the plan's CARGO_HOME.
        assert!(crate::worker_sandbox::SEAT_TOOLCHAIN_CACHES.contains(&(
            "cargo",
            "CARGO_HOME",
            "cargo"
        )));
        assert!(
            plan.env
                .iter()
                .any(|(k, v)| *k == "CARGO_HOME" && *v == cargo_home),
            "{:?}",
            plan.env
        );
        // Build output stays out of the cache: the full posture's target is the tree's own.
        assert!(!plan.env.iter().any(|(k, _)| *k == "CARGO_TARGET_DIR"));
        assert!(cache.is_dir(), "the cache dir exists before the bind");
        assert!(
            mounts.contains(&Mount::Bind {
                path: cache.clone(),
                rw: true,
            }),
            "{mounts:?}"
        );
        assert!(!cargo_home.starts_with("/home/seat"));
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let img = image_index(&args).unwrap();
        let land = landrun_end(&args, img).unwrap();
        let has = |range: &[String], pair: &[&str]| range.windows(2).any(|x| x == pair);
        assert!(has(&args[..img], &["--env", "CARGO_HOME"]), "{args:?}");
        assert!(has(&args[img..land], &["--env", "CARGO_HOME"]), "{args:?}");
        let cache_s = cache.to_string_lossy().into_owned();
        assert!(has(&args[img..land], &["--rwx", &cache_s]), "{args:?}");
        assert!(args
            .iter()
            .any(|a| a == &format!("type=bind,src={cache_s},dst={cache_s}")));
        assert!(!args
            .iter()
            .any(|a| a.contains(&cargo_home.to_string_lossy().into_owned())));
        // The HOME tmpfs is unchanged: the cache is not made to fit by growing it.
        assert!(has(
            &args[..img],
            &["--tmpfs", "/home/seat:rw,exec,mode=1777,size=512m"]
        ));
        assert_eq!(
            cmd.get_envs()
                .find(|(k, _)| *k == "CARGO_HOME")
                .and_then(|(_, v)| v)
                .map(PathBuf::from),
            Some(cargo_home)
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The [`crate::worker_sandbox::SEAT_TOOLCHAIN_BUILD_ENV`] variables removed from this process's
    /// environment while it lives (the plan reads an operator's value from there), restored on drop
    /// (a panic included). The caller holds the env lock.
    struct BuildEnvCleared(Vec<(&'static str, Option<std::ffi::OsString>)>);

    impl BuildEnvCleared {
        fn new() -> Self {
            let prior = crate::worker_sandbox::SEAT_TOOLCHAIN_BUILD_ENV
                .iter()
                .map(|(_, k, _)| (*k, std::env::var_os(k)))
                .collect();
            for (_, k, _) in crate::worker_sandbox::SEAT_TOOLCHAIN_BUILD_ENV {
                std::env::remove_var(k);
            }
            Self(prior)
        }
    }

    impl Drop for BuildEnvCleared {
        fn drop(&mut self) {
            for (k, v) in self.0.drain(..) {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    /// A container seat's cargo runs with the seat build settings ([`SEAT_TOOLCHAIN_BUILD_ENV`],
    /// core#997's end-to-end run): one job per CPU up to two, no debuginfo for the dev and test
    /// profiles, no incremental state. The plan carries them, the finalized wrapper passes each by
    /// NAME to the runtime and to landrun (a value only the runtime got would be dropped by
    /// landrun), and no value is on the argv. A variable the operator set in the daemon's
    /// environment is passed with the operator's value instead; a blank one is not a choice.
    ///
    /// [`SEAT_TOOLCHAIN_BUILD_ENV`]: crate::worker_sandbox::SEAT_TOOLCHAIN_BUILD_ENV
    #[test]
    fn a_container_seats_cargo_runs_with_the_seat_build_settings_and_the_operator_wins() {
        use crate::worker_sandbox::{floor_build_env, seat_build_jobs, SEAT_TOOLCHAIN_BUILD_ENV};
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        // The jobs follow the seat's CPUs, capped at two, never zero.
        assert_eq!(seat_build_jobs(0), 1);
        assert_eq!(seat_build_jobs(1), 1);
        assert_eq!(seat_build_jobs(2), 2);
        assert_eq!(seat_build_jobs(14), 2);
        let get = |env: &[(&'static str, String)], k: &str| {
            env.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone())
        };
        let ours = floor_build_env(|| 1, |_| None);
        assert_eq!(get(&ours, "CARGO_BUILD_JOBS").as_deref(), Some("1"));
        assert_eq!(get(&ours, "CARGO_PROFILE_DEV_DEBUG").as_deref(), Some("0"));
        assert_eq!(get(&ours, "CARGO_PROFILE_TEST_DEBUG").as_deref(), Some("0"));
        assert_eq!(get(&ours, "CARGO_INCREMENTAL").as_deref(), Some("0"));
        assert_eq!(ours.len(), SEAT_TOOLCHAIN_BUILD_ENV.len());
        // An operator's jobs value means the runtime is never asked for its CPUs.
        let theirs = floor_build_env(
            || panic!("the CPUs are not asked for"),
            |k| match k {
                "CARGO_BUILD_JOBS" => Some("6".into()),
                "CARGO_INCREMENTAL" => Some("  ".into()),
                _ => None,
            },
        );
        assert_eq!(get(&theirs, "CARGO_BUILD_JOBS").as_deref(), Some("6"));
        assert_eq!(get(&theirs, "CARGO_INCREMENTAL").as_deref(), Some("0"));
        // The CPUs are the runtime's (a VM's), never the host's, when the runtime answers.
        assert_eq!(cpus_from_info(Some("1\n"), 14), 1);
        assert_eq!(cpus_from_info(Some(" 2 "), 14), 2);
        assert_eq!(cpus_from_info(None, 14), 14);
        assert_eq!(cpus_from_info(Some("0"), 4), 4);
        assert_eq!(cpus_from_info(Some("{{.NCPU}}"), 4), 4);

        // Through the engine's own plan and wrapper, with one operator choice in the environment
        // and none of the other table variables inherited from the test's own.
        let _clean = BuildEnvCleared::new();
        std::env::set_var("CARGO_PROFILE_DEV_DEBUG", "line-tables-only");
        let base = tmp("cargo-build-env");
        let tree = base.join("wt");
        let worker = base.join("worker");
        std::fs::create_dir_all(&tree).unwrap();
        let (plan, _, cmd) = cargo_seat(
            &worker,
            &tree,
            &[],
            Path::new("docker"),
            "img:1",
            "wicked-seat-cargo-env",
            "none",
            "wicked-floor:t",
            "3g",
            &[],
            "cargo test",
        );
        let jobs = seat_build_jobs(seat_cpus()).to_string();
        assert_eq!(
            get(&plan.build_env, "CARGO_PROFILE_DEV_DEBUG").as_deref(),
            Some("line-tables-only"),
            "the operator's value wins: {:?}",
            plan.build_env
        );
        assert_eq!(get(&plan.build_env, "CARGO_BUILD_JOBS"), Some(jobs.clone()));
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let img = image_index(&args).unwrap();
        let land = landrun_end(&args, img).unwrap();
        let has = |range: &[String], pair: &[&str]| range.windows(2).any(|x| x == pair);
        let set = |k: &str| {
            cmd.get_envs()
                .find(|(n, _)| *n == k)
                .and_then(|(_, v)| v)
                .map(|v| v.to_string_lossy().into_owned())
        };
        for (_, var, _) in SEAT_TOOLCHAIN_BUILD_ENV {
            assert!(
                has(&args[..img], &["--env", var]),
                "{var} to the runtime: {args:?}"
            );
            assert!(
                has(&args[img..land], &["--env", var]),
                "{var} to landrun: {args:?}"
            );
            assert_eq!(
                set(var),
                get(&plan.build_env, var),
                "{var} on the client's env"
            );
        }
        assert!(
            !args
                .iter()
                .any(|a| a.starts_with("CARGO_") && a.contains('=')),
            "no build value on the argv: {args:?}"
        );
        assert_eq!(set("CARGO_INCREMENTAL").as_deref(), Some("0"));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// LIVE (`WICKED_CONTAINER_LIVE=1`, with `WICKED_SEAT_BASE_IMAGE` naming a Rust toolchain base;
    /// skipped otherwise): `cargo fetch --locked` of THIS repository's lockfile in a seat container
    /// rendered by the engine's own path (seat floor plan, mount plan, wrapper, finalize) with the
    /// 512 MB tmpfs HOME, through the gateway's egress allowlist. The fetch (~684 MB) succeeds and
    /// lands in the host cache dir, `HOME` stays near empty, and a second seat fetches the same
    /// lockfile offline from the cache the first one left.
    #[cfg(unix)]
    #[test]
    fn live_a_seat_fetches_this_repos_lockfile_into_its_persistent_cargo_cache() {
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        if std::env::var("WICKED_CONTAINER_LIVE").ok().as_deref() != Some("1") {
            eprintln!("skipped: set WICKED_CONTAINER_LIVE=1 with a container runtime to run");
            return;
        }
        let rt = runtime().expect("a container runtime answers");
        let floor = ensure_floor_image(&rt).expect("the floor image");
        let image = ensure_seat_image(&rt, "claude").expect("the claude seat image");
        let base = tmp("cargo-live");
        let tree = base.join("wt");
        let worker = base.join("worker");
        let gw_dir = base.join("gateway");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::create_dir_all(&gw_dir).unwrap();
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .canonicalize()
            .unwrap();
        let run = |argv: &[&str], limit: u64| {
            let mut c = rt.cmd();
            c.args(argv);
            output_within(c, Duration::from_secs(limit)).unwrap()
        };
        let prior = std::env::var(crate::seat_gateway::EGRESS_HOSTS_ENV).ok();
        std::env::set_var(
            crate::seat_gateway::EGRESS_HOSTS_ENV,
            "index.crates.io,static.crates.io",
        );
        let gw = crate::seat_gateway::ensure_gateway(&rt, &gw_dir);
        match &prior {
            Some(v) => std::env::set_var(crate::seat_gateway::EGRESS_HOSTS_ENV, v),
            None => std::env::remove_var(crate::seat_gateway::EGRESS_HOSTS_ENV),
        }
        let gw = gw.expect("the gateway starts");
        let key = crate::seat_gateway::UnitKey::new("cargo-live", "1", "claude");
        let env: Vec<(String, String)> = crate::seat_gateway::seat_env_all(&key.key)
            .into_iter()
            .chain([("CARGO_NET_RETRY".to_string(), "3".to_string())])
            .collect();
        // Shell-quoted: a checkout path may hold spaces or quotes.
        let repo_q = format!("'{}'", repo.display().to_string().replace('\'', "'\\''"));
        let script = |offline: bool| {
            format!(
                "echo CARGO_HOME=$CARGO_HOME; cargo --version; cd {r} && \
                 cargo fetch --locked {o} >/tmp/fetch.log 2>&1 && echo FETCH_OK || \
                 {{ echo FETCH_FAILED; tail -20 /tmp/fetch.log; }}; \
                 echo HOME_KB=$(du -sk /home/seat | cut -f1); \
                 echo CACHE_KB=$(du -sk \"$CARGO_HOME\" | cut -f1)",
                r = repo_q,
                o = if offline { "--offline" } else { "" },
            )
        };
        let mut outs: Vec<String> = Vec::new();
        let mut ok = true;
        for (i, offline) in [false, true].into_iter().enumerate() {
            let name = container_name("claude", "cargolive", i as u32);
            let network = ensure_seat_network(&rt, &name).expect("the seat network");
            crate::seat_gateway::attach(&rt, &gw, &network).unwrap();
            let ip = gateway_ip(&rt, &gw.name, &network).unwrap();
            start_netns_sidecar(&rt, &name, &network, &ip).expect("sidecar up");
            if i == 0 {
                crate::seat_gateway::register_live(&rt, &gw, &gw_dir, key.clone())
                    .expect("the unit key is served");
            }
            let (_, _, cmd) = cargo_seat(
                &worker,
                &tree,
                std::slice::from_ref(&repo),
                &rt.cli,
                &image,
                &name,
                &format!("container:{}", netns_name(&name)),
                &floor,
                "3g",
                &env,
                &script(offline),
            );
            let out = output_within(cmd, Duration::from_secs(900));
            reap(&SeatContainer {
                runtime: rt.cli.clone(),
                name: name.clone(),
            });
            let (so, se) = out
                .map(|o| {
                    (
                        String::from_utf8_lossy(&o.stdout).to_string(),
                        String::from_utf8_lossy(&o.stderr).to_string(),
                    )
                })
                .unwrap_or_else(|| (String::new(), "the seat timed out".to_string()));
            eprintln!("--- seat {i} stdout ---\n{so}--- seat {i} stderr ---\n{se}");
            ok &= so.contains("FETCH_OK");
            outs.push(so);
            if !ok {
                break;
            }
        }
        run(&["rm", "-f", &gw.name], 60);
        run(&["network", "rm", &gw.external_network], 60);
        let (cfg, broker) = crate::seat_gateway::volumes(&gw);
        run(&["volume", "rm", &cfg, &broker], 60);
        let cargo_home = worker.join("cache").join("claude").join("cargo");
        let persisted = cargo_home.join("registry").join("cache").is_dir();
        let _ = std::fs::remove_dir_all(&base);
        let first = outs.first().cloned().unwrap_or_default();
        assert!(
            first.contains(&format!("CARGO_HOME={}", cargo_home.display())),
            "the seat's CARGO_HOME is the host cache dir:\n{first}"
        );
        assert!(ok, "both fetches succeed:\n{outs:?}");
        assert!(
            persisted,
            "the registry cache is on the host after the seat"
        );
        let kb = |s: &str, tag: &str| -> u64 {
            s.lines()
                .find_map(|l| l.strip_prefix(tag))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or_else(|| panic!("no number after {tag} in:\n{s}"))
        };
        assert!(
            kb(&first, "HOME_KB=") < 64 * 1024,
            "HOME stays near empty:\n{first}"
        );
        assert!(
            kb(&first, "CACHE_KB=") > 512 * 1024,
            "the fetch would not have fit the HOME tmpfs:\n{first}"
        );
    }

    /// LIVE (`WICKED_CONTAINER_LIVE=1`, with `WICKED_SEAT_BASE_IMAGE` naming a Rust toolchain base;
    /// skipped otherwise), core#997's end-to-end run: THIS repository's lib tests compile in a seat
    /// container at the engine's default seat memory limit (3g, [`memory_from`]; the run's seat had
    /// it), rendered by the engine's own path (seat floor plan, mount plan, wrapper, finalize), with
    /// the seat build settings, and the same compile with cargo's default profile (the settings
    /// unset inside the seat, its own target dir) has rustc killed. A first seat fetches the
    /// lockfile through the gateway into the seat's cargo cache; the build seats then run offline
    /// (`--network none`). The cgroup's peak memory (`memory.peak`, read with `exec` while the seat
    /// runs; it counts page cache too) is printed for both. `WICKED_LIVE_SEAT_MEMORY` measures
    /// another limit (the control is then reported, not asserted). Measured on a 2 CPU / 6 GB
    /// Rancher VM: at 3g the settings build (peak 3072 MiB, the limit, page cache reclaimed) and the
    /// default profile is killed on the lib test crate; at 4g both build (3188 MiB and 3515 MiB).
    #[cfg(unix)]
    #[test]
    fn live_a_seat_compiles_this_repos_lib_tests_within_the_seat_memory_limit() {
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        if std::env::var("WICKED_CONTAINER_LIVE").ok().as_deref() != Some("1") {
            eprintln!("skipped: set WICKED_CONTAINER_LIVE=1 with a container runtime to run");
            return;
        }
        // The seat's settings are the table's, not ones this test process inherited.
        let _clean = BuildEnvCleared::new();
        let rt = runtime().expect("a container runtime answers");
        let floor = ensure_floor_image(&rt).expect("the floor image");
        let image = ensure_seat_image(&rt, "claude").expect("the claude seat image");
        let base = tmp("cargo-build-live");
        let tree = base.join("wt");
        let worker = base.join("worker");
        let gw_dir = base.join("gateway");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::create_dir_all(&gw_dir).unwrap();
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .canonicalize()
            .unwrap();
        let repo_q = format!("'{}'", repo.display().to_string().replace('\'', "'\\''"));
        let run = |argv: &[&str], limit: u64| {
            let mut c = rt.cmd();
            c.args(argv);
            output_within(c, Duration::from_secs(limit))
        };
        // The fetch seat: online through the gateway's crates.io allowlist, as in the cache test.
        let prior = std::env::var(crate::seat_gateway::EGRESS_HOSTS_ENV).ok();
        std::env::set_var(
            crate::seat_gateway::EGRESS_HOSTS_ENV,
            "index.crates.io,static.crates.io",
        );
        let gw = crate::seat_gateway::ensure_gateway(&rt, &gw_dir);
        match &prior {
            Some(v) => std::env::set_var(crate::seat_gateway::EGRESS_HOSTS_ENV, v),
            None => std::env::remove_var(crate::seat_gateway::EGRESS_HOSTS_ENV),
        }
        let gw = gw.expect("the gateway starts");
        let key = crate::seat_gateway::UnitKey::new("cargo-build-live", "1", "claude");
        let env: Vec<(String, String)> = crate::seat_gateway::seat_env_all(&key.key)
            .into_iter()
            .chain([("CARGO_NET_RETRY".to_string(), "3".to_string())])
            .collect();
        let name = container_name("claude", "buildfetch", 0);
        let network = ensure_seat_network(&rt, &name).expect("the seat network");
        crate::seat_gateway::attach(&rt, &gw, &network).unwrap();
        let ip = gateway_ip(&rt, &gw.name, &network).unwrap();
        start_netns_sidecar(&rt, &name, &network, &ip).expect("sidecar up");
        crate::seat_gateway::register_live(&rt, &gw, &gw_dir, key.clone())
            .expect("the unit key is served");
        let (_, _, cmd) = cargo_seat(
            &worker,
            &tree,
            std::slice::from_ref(&repo),
            &rt.cli,
            &image,
            &name,
            &format!("container:{}", netns_name(&name)),
            &floor,
            "3g",
            &env,
            &format!(
                "cd {repo_q} && cargo fetch --locked >/tmp/fetch.log 2>&1 && echo FETCH_OK || \
                 {{ echo FETCH_FAILED; tail -20 /tmp/fetch.log; }}"
            ),
        );
        let fetched = output_within(cmd, Duration::from_secs(900))
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();
        reap(&SeatContainer {
            runtime: rt.cli.clone(),
            name: name.clone(),
        });
        let _ = run(&["rm", "-f", &gw.name], 60);
        let _ = run(&["network", "rm", &gw.external_network], 60);
        let (cfg, broker) = crate::seat_gateway::volumes(&gw);
        let _ = run(&["volume", "rm", &cfg, &broker], 60);
        assert!(fetched.contains("FETCH_OK"), "the fetch seat: {fetched}");

        // The limit under test: the engine's default, or `WICKED_LIVE_SEAT_MEMORY` to measure another.
        let measuring = std::env::var("WICKED_LIVE_SEAT_MEMORY")
            .ok()
            .filter(|v| !v.trim().is_empty());
        let memory = memory_from(measuring.as_deref()).to_string();
        // One offline build seat: `unset` names the variables a control removes inside the seat.
        let build = |label: &str, unset: &str| -> (String, Option<u64>, Option<u64>) {
            let name = container_name("claude", label, 0);
            let target = tree.join(format!("target-{label}"));
            let (_, _, cmd) = cargo_seat(
                &worker,
                &tree,
                std::slice::from_ref(&repo),
                &rt.cli,
                &image,
                &name,
                "none",
                &floor,
                &memory,
                &[("CARGO_TARGET_DIR".to_string(), target.display().to_string())],
                &format!(
                    "{unset} echo JOBS=$CARGO_BUILD_JOBS DEBUG=$CARGO_PROFILE_DEV_DEBUG/\
                     $CARGO_PROFILE_TEST_DEBUG INCREMENTAL=$CARGO_INCREMENTAL; \
                     echo SWAP_KB=$(awk '/^SwapTotal:/ {{print $2}}' /proc/meminfo); \
                     cd {repo_q} && cargo test --lib --no-run --locked --offline \
                     >/tmp/build.log 2>&1 && echo BUILD_OK || {{ echo BUILD_FAILED; \
                     grep -E '^error' /tmp/build.log | cut -c1-120 | tail -3; \
                     echo KILLED=$(grep -c 'SIGKILL' /tmp/build.log); }}"
                ),
            );
            // The cgroup's peak, read from outside while the seat runs (Landlock denies /sys inside).
            let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let poll = {
                let (rt, name, done) = (rt.clone(), name.clone(), done.clone());
                std::thread::spawn(move || {
                    let (mut peak, mut ooms) = (None, None);
                    while !done.load(std::sync::atomic::Ordering::SeqCst) {
                        let mut c = rt.cmd();
                        c.args([
                            "exec",
                            &name,
                            "cat",
                            "/sys/fs/cgroup/memory.peak",
                            "/sys/fs/cgroup/memory.events",
                        ]);
                        if let Some(o) = output_within(c, Duration::from_secs(20)) {
                            let s = String::from_utf8_lossy(&o.stdout).to_string();
                            let mut lines = s.lines();
                            if let Some(p) = lines.next().and_then(|l| l.trim().parse().ok()) {
                                peak = Some(p);
                            }
                            if let Some(n) = s
                                .lines()
                                .find_map(|l| l.strip_prefix("oom_kill "))
                                .and_then(|v| v.trim().parse().ok())
                            {
                                ooms = Some(n);
                            }
                        }
                        std::thread::sleep(Duration::from_secs(3));
                    }
                    (peak, ooms)
                })
            };
            let out = output_within(cmd, Duration::from_secs(2400));
            done.store(true, std::sync::atomic::Ordering::SeqCst);
            reap(&SeatContainer {
                runtime: rt.cli.clone(),
                name,
            });
            let (peak, ooms) = poll.join().unwrap_or((None, None));
            let so = out
                .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                .unwrap_or_else(|| "the seat timed out".to_string());
            let _ = std::fs::remove_dir_all(&target);
            eprintln!(
                "--- {label}: peak={:?} MiB oom_kill(last read)={ooms:?} ---\n{so}",
                peak.map(|p| p / (1024 * 1024))
            );
            (so, peak, ooms)
        };
        let (seat, seat_peak, _) = build("seatbuild", "");
        let (control, control_peak, control_ooms) = build(
            "defaultbuild",
            "unset CARGO_BUILD_JOBS CARGO_PROFILE_DEV_DEBUG CARGO_PROFILE_TEST_DEBUG \
             CARGO_INCREMENTAL;",
        );
        let _ = std::fs::remove_dir_all(&base);
        eprintln!(
            "seat build: peak {:?} MiB; default profile: peak {:?} MiB, oom_kill {control_ooms:?}, \
             {}",
            seat_peak.map(|p| p / (1024 * 1024)),
            control_peak.map(|p| p / (1024 * 1024)),
            if control.contains("BUILD_OK") {
                "built"
            } else {
                "did not build"
            }
        );
        assert!(
            seat.contains("JOBS=") && !seat.contains("JOBS= ") && seat.contains("DEBUG=0/0"),
            "the seat carries the build settings: {seat}"
        );
        assert!(seat.contains("INCREMENTAL=0"), "{seat}");
        assert!(
            seat.contains("BUILD_OK"),
            "the lib tests compile within {memory} with the seat build settings: {seat}"
        );
        // The red control holds where the seat has no swap to spill into (the run's runtime had
        // none; with swap, `--memory` alone lets a seat use as much again, Docker's default).
        let swap_kb = control
            .lines()
            .find_map(|l| l.strip_prefix("SWAP_KB="))
            .and_then(|v| v.trim().parse::<u64>().ok());
        eprintln!("the runtime's swap: {swap_kb:?} kB");
        if measuring.is_none() && swap_kb == Some(0) {
            assert!(
                !control.contains("BUILD_OK")
                    && control.contains("KILLED=")
                    && !control.contains("KILLED=0"),
                "cargo's default profile has rustc killed at the default {memory} limit: {control}"
            );
        }
    }

    /// LIVE (needs a container runtime; `WICKED_CONTAINER_LIVE=1`, skipped otherwise;
    /// DES-TEAMING-002 §8.8): a team member turn's seat container with its repository view. The
    /// view is a real checkpoint tree written by `team::Repo::materialize`, mounted by
    /// [`add_read_view`] into a member turn's floor (its scratch cwd and seat home writable,
    /// nothing else). Inside: the view's files read; the kernel lists its mount `ro`; overwriting,
    /// creating, removing and making a directory there all fail; the live worktree it was written from, and a sibling of the
    /// view, do not exist. On the host the view's file is unchanged.
    #[cfg(unix)]
    #[test]
    fn live_a_member_reads_its_repository_view_and_cannot_write_it() {
        if std::env::var("WICKED_CONTAINER_LIVE").ok().as_deref() != Some("1") {
            eprintln!("skipped: set WICKED_CONTAINER_LIVE=1 with a container runtime to run");
            return;
        }
        let rt = runtime().expect("a container runtime answers");
        let floor = ensure_floor_image(&rt).expect("the floor image");
        let image = ensure_seat_image(&rt, "claude").expect("the claude seat image");
        let base = tmp("live-view");
        let wt = base.join("wt");
        let sandbox = base.join("sandbox");
        let home = base.join("home");
        let view = base.join("view");
        let sibling = base.join("view-sibling");
        for d in [&wt, &sandbox, &home, &view, &sibling] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(sibling.join("other"), "sibling").unwrap();
        // The worktree's checkpoint tree, written into the view the way a member turn's is.
        let git = |args: &[&str]| {
            crate::worktree_guard::git(&wt, args, &[]).unwrap();
        };
        git(&["init", "-q"]);
        std::fs::create_dir_all(wt.join("src")).unwrap();
        std::fs::write(wt.join("src/lib.rs"), "fn checkpoint() {}\n").unwrap();
        let repo = crate::team::Repo {
            workdir: wt.clone(),
            git_dir: wt.join(".git"),
        };
        let tree = repo.snapshot().unwrap();
        repo.materialize(&tree, &view, crate::team::REPO_VIEW_MAX_BYTES)
            .unwrap();
        let spec = SeatFloor {
            profile: "member".into(),
            tree: sandbox.clone(),
            writable: vec![sandbox.clone(), home.clone()],
            protected: vec![],
            readmitted: vec![],
            readmitted_files: vec![],
            masks: vec![],
            pinned: vec![],
            net: vec![],
        };
        let mut mounts = mount_plan(&spec).unwrap();
        add_read_view(&mut mounts, &view).expect("the view mounts read-only");
        let name = container_name("claude", "liveview", 1);
        let user = operator_user();
        let mut w = run_argv(&ContainerLaunch {
            runtime: &rt.cli,
            image: &image,
            name: &name,
            network: "none",
            workdir: &sandbox,
            user: &user,
            mounts: &mounts,
            floor: &floor,
            memory: "3g",
            gateway_ports: GATEWAY_PORTS,
            own_ports: &[],
        });
        let script = format!(
            "echo READ=[$(cat {v}/src/lib.rs)]; \
             grep -Eq \"^[^ ]+ {v} [^ ]+ ro[, ]\" /proc/self/mounts && echo MOUNT_RO || echo MOUNT_NOT_RO; \
             (echo x > {v}/src/lib.rs) 2>/dev/null && echo OVERWROTE || echo OVERWRITE_DENIED; \
             (echo x > {v}/new) 2>/dev/null && echo CREATED || echo CREATE_DENIED; \
             rm -f {v}/src/lib.rs 2>/dev/null; test -e {v}/src/lib.rs && echo RM_DENIED || echo REMOVED; \
             mkdir {v}/d 2>/dev/null && echo MKDIR_OK || echo MKDIR_DENIED; \
             test -e {wt}/src/lib.rs && echo WORKTREE_VISIBLE || echo WORKTREE_ABSENT; \
             test -e {s}/other && echo SIBLING_VISIBLE || echo SIBLING_ABSENT; \
             echo ok > {sb}/note && echo SCRATCH_OK",
            v = view.display(),
            wt = wt.display(),
            s = sibling.display(),
            sb = sandbox.display(),
        );
        w.extend(["sh".into(), "-c".into(), script]);
        // spawn-audit: test-only — renders the wrapper argv; spawned only by the gated live test.
        let mut cmd = Command::new(&w[0]);
        cmd.args(&w[1..]);
        let out = output_within(cmd, Duration::from_secs(60)).expect("the seat ran");
        reap(&SeatContainer {
            runtime: rt.cli.clone(),
            name: name.clone(),
        });
        let so = String::from_utf8_lossy(&out.stdout).to_string();
        let se = String::from_utf8_lossy(&out.stderr).to_string();
        eprintln!("--- seat stdout ---\n{so}--- seat stderr ---\n{se}");
        assert!(out.status.success(), "{so}\n{se}");
        for want in [
            "READ=[fn checkpoint() {}]",
            "MOUNT_RO",
            "OVERWRITE_DENIED",
            "CREATE_DENIED",
            "RM_DENIED",
            "MKDIR_DENIED",
            "WORKTREE_ABSENT",
            "SIBLING_ABSENT",
            "SCRATCH_OK",
        ] {
            assert!(so.contains(want), "{want} missing:\n{so}");
        }
        assert_eq!(
            std::fs::read_to_string(view.join("src/lib.rs")).unwrap(),
            "fn checkpoint() {}\n",
            "the view is unchanged on the host"
        );
        assert!(!view.join("new").exists() && !view.join("d").exists());
        // Files are written read-only; the dirs are the test's to remove.
        let _ = std::fs::remove_dir_all(&base);
    }

    fn argv_paths_for(tail: &[String], prompt: Option<&str>) -> Vec<PathBuf> {
        path_option_slots(tail, prompt)
            .into_iter()
            .map(|(i, at)| PathBuf::from(&tail[i][at..]))
            .filter(|p| p.is_absolute() && p.exists())
            .collect()
    }

    fn argv_paths(tail: &[String]) -> Vec<PathBuf> {
        argv_paths_for(tail, None)
    }

    /// agy review r1 (P1): a template may place the prompt after any bare flag (`-m {PROMPT}`:
    /// no `--` guards it); the prompt token is never read as an option or a value.
    #[test]
    fn the_prompt_token_never_names_a_mount() {
        let base = tmp("argvprompt");
        let p = base.to_string_lossy().into_owned();
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let glued = format!("--skill={p}");
        let argv = s(&["custom-runner", "-m", &glued]);
        assert_eq!(argv_paths(&argv), vec![base.clone()], "unguarded: the hole");
        assert!(argv_paths_for(&argv, Some(&glued)).is_empty());
        // The prompt as a path option's separate value is not a path either.
        let argv = s(&["custom-runner", "--settings", &p]);
        assert!(argv_paths_for(&argv, Some(&p)).is_empty());
        // Through arm's mount step: nothing mounted, the prompt token unchanged.
        let mut mounts = Vec::new();
        let mut argv = s(&["custom-runner", "-m", &glued]);
        mount_argv_paths(&mut mounts, &mut argv, Some(&glued));
        assert!(mounts.is_empty());
        assert_eq!(argv[2], glued);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Codex review r1 (P1): only a known path option's value before `--` is a mount; prompt
    /// text that spells a path or an option never is.
    #[test]
    fn only_path_options_before_the_guard_name_mounts() {
        let base = tmp("argvp");
        let p = base.to_string_lossy().into_owned();
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            argv_paths(&s(&["claude", "--settings", &p])),
            vec![base.clone()]
        );
        assert_eq!(
            argv_paths(&s(&["claude", &format!("--mcp-config={p}")])),
            vec![base.clone()]
        );
        assert!(argv_paths(&s(&["codex", "exec", "--", &p])).is_empty());
        assert!(argv_paths(&s(&["codex", "exec", "--", &format!("--settings={p}")])).is_empty());
        assert!(argv_paths(&s(&["claude", "-p", &p])).is_empty());
        assert!(argv_paths(&s(&["claude", "--model", &p])).is_empty());
        // Codex review r2: a prompt passed as an option value is text.
        assert!(argv_paths(&s(&["copilot", "-p", &format!("--settings={p}")])).is_empty());
        // claude's bare `-p` before the guard still ends at `--`.
        assert!(argv_paths(&s(&["claude", "-p", "--", &format!("--settings={p}")])).is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Codex review r1 (P1): a read path never uncovers a mask — every mask is rendered up front,
    /// and a read root or argv path inside one is never bound (at arm time; `finalize` binds
    /// nothing, #1042).
    #[test]
    fn a_late_read_path_never_uncovers_a_mask() {
        let base = tmp("mask");
        let tree = base.join("wt");
        let home = base.join("home");
        let ssh = home.join(".ssh");
        for d in [&tree, &ssh] {
            std::fs::create_dir_all(d).unwrap();
        }
        let spec = SeatFloor {
            profile: "governed".into(),
            tree: tree.clone(),
            writable: vec![tree.clone()],
            protected: vec![],
            readmitted: vec![],
            readmitted_files: vec![],
            masks: vec![ssh.clone()],
            pinned: vec![],
            net: vec![],
        };
        let mut mounts = mount_plan(&spec).unwrap();
        assert!(
            mounts.contains(&Mount::Tmpfs { path: ssh.clone() }),
            "{mounts:?}"
        );
        add_read_roots(&mut mounts, std::slice::from_ref(&home));
        let mut argv = vec![
            "claude".to_string(),
            "--add-dir".to_string(),
            ssh.to_string_lossy().into_owned(),
        ];
        mount_argv_paths(&mut mounts, &mut argv, None);
        let w = run_argv(&ContainerLaunch {
            runtime: Path::new("docker"),
            image: "img:1",
            name: "wicked-seat-m",
            network: "none",
            workdir: &tree,
            user: "1:1",
            mounts: &mounts,
            floor: "wicked-floor:t",
            memory: "1g",
            gateway_ports: &[],
            own_ports: &[],
        });
        // spawn-audit: test-only — never spawned; inspects the finalized argv.
        let mut cmd = Command::new(&w[0]);
        cmd.args(&w[1..]).args(&argv);
        cmd.env(crate::gate_hook::READ_ROOTS_ENV, &home);
        let fin = finalize(cmd);
        let args: Vec<String> = fin
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let bind = |p: &Path| format!("type=bind,src={p},dst={p},readonly", p = p.display());
        assert!(
            !args.contains(&bind(&ssh)),
            "a masked dir is never bound: {args:?}"
        );
        // The ancestor may be mounted; the mask over its .ssh stays.
        assert!(args.contains(&bind(&home)), "{args:?}");
        assert!(args
            .iter()
            .any(|a| a == &format!("type=tmpfs,dst={},tmpfs-size=1m", ssh.display())));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Codex review r1 (P2): a read-only unit outside a linked worktree still sees its tree.
    #[test]
    fn a_tree_no_list_exposes_is_mounted_read_only() {
        let base = tmp("rotree");
        let spec = SeatFloor {
            profile: "governed".into(),
            tree: base.clone(),
            writable: vec![],
            protected: vec![],
            readmitted: vec![],
            readmitted_files: vec![],
            masks: vec![],
            pinned: vec![],
            net: vec![],
        };
        assert_eq!(
            mount_plan(&spec).unwrap(),
            vec![Mount::Bind {
                path: base.clone(),
                rw: false
            }]
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Codex review r1 (P2): the seat's own loopback server (opencode) may bind and connect.
    #[test]
    fn own_ports_get_bind_and_connect_grants() {
        let land = landrun_prefix(&[], &[3000], &[4567]);
        let has = |pair: &[&str]| land.windows(2).any(|w| w == pair);
        assert!(has(&["--bind-tcp", "4567"]) && has(&["--connect-tcp", "4567"]));
        assert!(!has(&["--bind-tcp", "3000"]));
    }

    /// Codex review r2 (P2): read roots and the skills delivery are mounted read-only for every
    /// container launch, never over a mask.
    #[test]
    fn read_roots_are_mounted_read_only_unless_covered_or_masked() {
        let base = tmp("rr");
        let (tree, skills, home, secret) = (
            base.join("wt"),
            base.join("skills"),
            base.join("home"),
            base.join("home/.ssh"),
        );
        for d in [&tree, &skills, &secret] {
            std::fs::create_dir_all(d).unwrap();
        }
        let mut mounts = vec![
            Mount::Bind {
                path: tree.clone(),
                rw: true,
            },
            Mount::Tmpfs {
                path: secret.clone(),
            },
        ];
        add_read_roots(
            &mut mounts,
            &[
                skills.clone(),
                tree.join("sub"),
                secret.join("id"),
                base.join("missing"),
            ],
        );
        assert_eq!(
            mounts,
            vec![
                Mount::Bind {
                    path: tree.clone(),
                    rw: true
                },
                Mount::Tmpfs {
                    path: secret.clone()
                },
                Mount::Bind {
                    path: skills.clone(),
                    rw: false
                },
            ]
        );
        let _ = home;
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Codex review r2 (P2): a runtime call past its deadline returns at the deadline even when
    /// a descendant keeps its pipes open.
    #[cfg(unix)]
    #[test]
    fn a_deadline_holds_even_when_a_descendant_keeps_the_pipes() {
        // spawn-audit: test-only — a shell whose background child holds stdout for 30 s.
        let mut c = Command::new("sh");
        c.args(["-c", "sleep 30 & sleep 30"]);
        let t = Instant::now();
        assert!(output_within(c, Duration::from_millis(300)).is_none());
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    }

    /// Codex review r3 (P2): a mounted snapshot's scripts dir stays on the seat's PATH.
    #[test]
    fn the_seat_path_keeps_mounted_entries_and_the_images() {
        let p = std::ffi::OsString::from("/snap/gen1/scripts:/opt/homebrew/bin:/usr/bin");
        assert_eq!(
            container_path(&p, &[PathBuf::from("/snap/gen1")]),
            format!("/snap/gen1/scripts:{IMAGE_PATH}")
        );
    }

    /// Codex review r3 (P1): a planted symlink at a launch file's path is refused, and a
    /// dangling one is never followed to create a host file.
    #[cfg(unix)]
    #[test]
    fn a_planted_pin_symlink_is_refused_and_never_followed() {
        let base = tmp("pin");
        let target = base.join("operator-private.txt");
        std::fs::write(&target, "private").unwrap();
        let pin = base.join("settings-unit-2.json");
        std::os::unix::fs::symlink(&target, &pin).unwrap();
        assert!(trust_pin(&pin).unwrap_err().contains("symlink"));
        let dangling = base.join("floor-spec-unit-3.json");
        let elsewhere = base.join("created-by-follow");
        std::os::unix::fs::symlink(&elsewhere, &dangling).unwrap();
        assert!(trust_pin(&dangling).is_err());
        assert!(!elsewhere.exists(), "never created through the link");
        let fresh = base.join("gov/floor-spec-unit-4.json");
        trust_pin(&fresh).unwrap();
        assert!(fresh.is_file());
        trust_pin(&fresh).unwrap();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Codex review r4 + r5 (P1): a rewritten `.git` pointer, or a forged gitdir triple that
    /// agrees with itself, never names the dirs a seat is granted: the anchor is the git dir the
    /// engine pinned from the registered repository.
    #[cfg(unix)]
    #[test]
    fn a_redirected_git_pointer_is_refused() {
        let base = tmp("gitptr");
        let common = base.join("clone/.git");
        let wtgit = common.join("worktrees/wt1");
        let wt = base.join("wt1");
        let other = base.join("private-project");
        for d in [
            &wtgit,
            &wt,
            &other,
            &common.join("objects"),
            &common.join("refs"),
        ] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(wtgit.join("commondir"), "../..\n").unwrap();
        std::fs::write(
            wtgit.join("gitdir"),
            format!("{}\n", wt.join(".git").display()),
        )
        .unwrap();
        let point = |to: &Path| {
            std::fs::write(wt.join(".git"), format!("gitdir: {}\n", to.display())).unwrap()
        };
        let pin = Some(wtgit.as_path());
        point(&wtgit);
        assert!(verify_linked_worktree(&wt, pin).is_ok());
        assert!(
            verify_linked_worktree(&wt.join("sub"), pin).is_ok(),
            "a subdir resolves its worktree"
        );
        // No pin from the registered repository: a linked worktree is refused.
        assert!(verify_linked_worktree(&wt, None)
            .unwrap_err()
            .contains("pinned by the engine"));
        point(&other);
        assert!(verify_linked_worktree(&wt, pin)
            .unwrap_err()
            .contains("other than the one the engine pinned"));
        // r5: a forged triple entirely inside the tree, self-consistent, objects -> a host dir.
        let forged = wt.join("tmp/forged/.git");
        let fgit = forged.join("worktrees/x");
        std::fs::create_dir_all(&fgit).unwrap();
        std::fs::write(fgit.join("commondir"), "../..\n").unwrap();
        std::fs::write(
            fgit.join("gitdir"),
            format!("{}\n", wt.join(".git").display()),
        )
        .unwrap();
        std::os::unix::fs::symlink(&other, forged.join("objects")).unwrap();
        point(&fgit);
        assert!(
            verify_linked_worktree(&wt, pin).is_err(),
            "forged triple refused"
        );
        // Another worktree's gitdir: refused against the pin.
        let wt2git = common.join("worktrees/wt2");
        std::fs::create_dir_all(&wt2git).unwrap();
        point(&wt2git);
        assert!(verify_linked_worktree(&wt, pin).is_err());
        // The pinned dir's commondir redirected, or a shared root replaced by a link.
        point(&wtgit);
        std::fs::write(wtgit.join("commondir"), "../../../../tmp/forged/.git\n").unwrap();
        assert!(verify_linked_worktree(&wt, pin)
            .unwrap_err()
            .contains("commondir leads elsewhere"));
        std::fs::write(wtgit.join("commondir"), "../..\n").unwrap();
        std::fs::remove_dir_all(common.join("objects")).unwrap();
        std::os::unix::fs::symlink(&other, common.join("objects")).unwrap();
        assert!(verify_linked_worktree(&wt, pin)
            .unwrap_err()
            .contains("redirected objects"));
        // A `.git` that is itself a link is refused; a main checkout grants nothing outside.
        std::fs::remove_file(wt.join(".git")).unwrap();
        std::os::unix::fs::symlink(&wtgit, wt.join(".git")).unwrap();
        assert!(verify_linked_worktree(&wt, pin)
            .unwrap_err()
            .contains("symbolic link"));
        assert!(verify_linked_worktree(&base.join("clone"), None).is_ok());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The pin the actor really takes (`worktree_guard::pinned_git_dir` from the registered
    /// repository) over a real `git worktree add` is accepted; the worktree's `.git` rewritten to
    /// another dir afterwards is refused against that same pin.
    #[test]
    fn the_actors_pin_of_a_real_linked_worktree_is_accepted() {
        let base = tmp("realwt");
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |dir: &Path, args: &[&str]| {
            // spawn-audit: test-only — local git over a temp repo.
            let out = Command::new("git")
                .hardened()
                .args(args)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
                .output()
                .unwrap();
            assert!(out.status.success(), "{:?}", out);
        };
        git(&repo, &["init", "-q"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "i"]);
        let wt = base.join("wt");
        git(&repo, &["worktree", "add", "-q", wt.to_str().unwrap()]);
        let pin = crate::worktree_guard::pinned_git_dir(&wt, &repo).unwrap();
        assert!(verify_linked_worktree(&wt, Some(&pin)).is_ok());
        let decoy = base.join("decoy/.git/worktrees/x");
        std::fs::create_dir_all(&decoy).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", decoy.display())).unwrap();
        assert!(verify_linked_worktree(&wt, Some(&pin)).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Codex review r4 (P2): a command that merely carries the seat label is not ours to reap.
    #[test]
    fn only_the_complete_wrapper_is_reaped() {
        // spawn-audit: test-only — never spawned.
        let mut c = Command::new("docker");
        c.args([
            "run",
            "--label",
            "wicked.seat=1",
            "--name",
            "keep",
            "-d",
            "alpine",
            "sleep",
            "9",
        ]);
        assert!(seat_container(&c).is_none());
    }

    /// Codex review r4 (P2): a host alias in an env value or a path option is re-spelled to the
    /// canonical path the mount sits at.
    #[cfg(unix)]
    #[test]
    fn aliases_are_respelled_to_the_mounted_canonical_path() {
        let base = tmp("alias");
        let real = base.join("real");
        std::fs::create_dir_all(real.join("home")).unwrap();
        let alias = base.join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let mounts = vec![real.clone()];
        assert_eq!(
            canonical_spelling(alias.join("home").as_os_str(), &mounts),
            real.join("home").into_os_string()
        );
        assert_eq!(
            canonical_spelling(std::ffi::OsStr::new("value"), &mounts),
            "value"
        );
        let unmounted = base.join("elsewhere");
        assert_eq!(
            canonical_spelling(unmounted.as_os_str(), &mounts),
            unmounted.into_os_string()
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Codex review r5 (P2): the attached `--opt=<alias>` spelling is re-spelled like the
    /// separate one, and mounted read-only; a value after the CLI's `--` stays as written.
    #[cfg(unix)]
    #[test]
    fn argv_path_options_are_mounted_and_respelled_in_both_spellings() {
        let base = tmp("alias-fin");
        let real = base.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let alias = base.join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let real = real.canonicalize().unwrap();
        let a = alias.to_string_lossy().into_owned();
        let r = real.to_string_lossy().into_owned();
        let mut argv: Vec<String> = [
            "codex",
            "exec",
            &format!("--cd={a}"),
            "-C",
            &a,
            "--",
            &format!("--cd={a}"),
        ]
        .iter()
        .map(|x| x.to_string())
        .collect();
        let mut mounts = Vec::new();
        mount_argv_paths(&mut mounts, &mut argv, Some("the prompt"));
        assert_eq!(
            mounts,
            vec![Mount::Bind {
                path: real.clone(),
                rw: false
            }]
        );
        assert_eq!(
            argv,
            vec![
                "codex".to_string(),
                "exec".into(),
                format!("--cd={r}"),
                "-C".into(),
                r.clone(),
                "--".into(),
                format!("--cd={a}"),
            ]
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_unit_scratch_is_removed_when_its_guard_drops() {
        let base = tmp("scratch");
        let d = base.join("checks/run/unit");
        std::fs::create_dir_all(&d).unwrap();
        drop(RemoveOnDrop(d.clone()));
        assert!(!d.exists());
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

/// core#1228: a prompt or payload too large for one Linux exec argument never reaches the seat
/// container's argv (docker-init failed with "argument list too long" on a coverage judge).
#[cfg(all(test, unix))]
mod long_prompt_tests {
    use super::*;

    fn base() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wicked-long-prompt-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn big_prompt() -> String {
        // Over 256 KiB, past Linux's 128 KiB per-argument limit twice over.
        let p = "Judge the coverage step. ".repeat(11_000);
        assert!(p.len() > 256 * 1024);
        p
    }

    #[test]
    fn a_prompt_over_256_kib_rides_a_read_only_file_and_the_argv_fits_an_exec() {
        let root = base();
        let prompt = big_prompt();
        // Before: the prompt itself is an argument, which no Linux exec takes.
        let mut argv = crate::execute_wrapped::build_argv("codex exec \"{PROMPT}\"", &prompt, &[]);
        assert!(check_exec_args(&argv).unwrap_err().contains("argument"));
        let mut mounts = Vec::new();
        let carried = carry_prompt_in_file_at(&root, "codex", &prompt, &mut argv, &mut mounts)
            .unwrap()
            .expect("carried");
        check_exec_args(&argv).expect("the argv now fits one exec");
        assert!(argv.iter().all(|a| a.len() < PROMPT_ARGV_MAX), "{argv:?}");
        assert_eq!(argv[..2], ["codex".to_string(), "exec".to_string()]);
        let [Mount::Bind { path, rw: false }] = mounts.as_slice() else {
            panic!("one read-only bind: {mounts:?}");
        };
        assert_eq!(std::fs::read_to_string(path).unwrap(), prompt);
        assert!(path.starts_with(root.canonicalize().unwrap()));
        let pointer = argv.last().unwrap();
        assert!(
            pointer.contains(&path.display().to_string())
                && pointer.contains("Read that whole file"),
            "{pointer}"
        );
        // Private to the operator; gone with the unit.
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode & 0o077, 0, "{mode:o}");
        assert_eq!(&carried.file, path);
        let dir = carried._dir.0.clone();
        drop(carried);
        assert!(!dir.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn pi_reads_the_long_prompt_file_itself() {
        let root = base();
        let prompt = big_prompt();
        let mut argv = crate::execute_wrapped::build_argv("pi -p \"{PROMPT}\"", &prompt, &[]);
        let mut mounts = Vec::new();
        let _guard = carry_prompt_in_file_at(&root, "pi", &prompt, &mut argv, &mut mounts)
            .unwrap()
            .expect("carried");
        let Mount::Bind { path, .. } = &mounts[0] else {
            panic!("{mounts:?}")
        };
        assert_eq!(argv[..2], ["pi".to_string(), "-p".to_string()]);
        assert_eq!(argv[2], format!("@{}", path.display()));
        assert!(argv[3].contains("whole task"), "{argv:?}");
        check_exec_args(&argv).unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn pi_with_a_decorated_template_gets_the_read_sentence_not_a_glued_at_path() {
        let root = base();
        let prompt = big_prompt();
        let mut argv =
            crate::execute_wrapped::build_argv("pi -p \"{PROMPT} Be concise.\"", &prompt, &[]);
        let mut mounts = Vec::new();
        let _c = carry_prompt_in_file_at(&root, "pi", &prompt, &mut argv, &mut mounts)
            .unwrap()
            .expect("carried");
        let last = argv.last().unwrap();
        assert!(
            !last.starts_with('@') && last.contains("Read that whole file"),
            "{last}"
        );
        assert!(last.ends_with(" Be concise."), "{last}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_prompt_glued_to_a_flag_is_replaced_in_place() {
        let root = base();
        let prompt = big_prompt();
        let mut argv = vec!["tool".to_string(), format!("--prompt={prompt}")];
        let mut mounts = Vec::new();
        let _guard = carry_prompt_in_file_at(&root, "tool", &prompt, &mut argv, &mut mounts)
            .unwrap()
            .expect("carried");
        assert!(
            argv[1].starts_with("--prompt=This turn's task"),
            "{}",
            &argv[1][..80]
        );
        check_exec_args(&argv).unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_prompt_the_argv_does_not_carry_is_not_handed_over() {
        let root = base();
        let prompt = big_prompt();
        let mut argv = vec!["agy".to_string(), "-p".to_string(), "short".to_string()];
        let mut mounts = Vec::new();
        let none = carry_prompt_in_file_at(&root, "agy", &prompt, &mut argv, &mut mounts).unwrap();
        assert!(none.is_none() && mounts.is_empty());
        assert_eq!(argv[2], "short");
        let store = root.canonicalize().unwrap().join(".wicked-prompts");
        assert_eq!(std::fs::read_dir(&store).map(|d| d.count()).unwrap_or(0), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_seat_mount_inside_the_prompt_store_is_refused_too() {
        let root = base();
        let prompt = big_prompt();
        let mut argv = crate::execute_wrapped::build_argv("codex exec \"{PROMPT}\"", &prompt, &[]);
        let inside = root
            .canonicalize()
            .unwrap()
            .join(".wicked-prompts")
            .join("x");
        let mut mounts = vec![Mount::Bind {
            path: inside,
            rw: true,
        }];
        let why =
            carry_prompt_in_file_at(&root, "codex", &prompt, &mut argv, &mut mounts).unwrap_err();
        assert!(why.contains("which the seat mounts"), "{why}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_prompt_store_under_a_seat_mount_is_refused() {
        // A worker home inside a mounted root would show every unit's prompts to the seat.
        let root = base();
        let prompt = big_prompt();
        let mut argv = crate::execute_wrapped::build_argv("codex exec \"{PROMPT}\"", &prompt, &[]);
        let canon = root.canonicalize().unwrap();
        for rw in [false, true] {
            let mut mounts = vec![Mount::Bind {
                path: canon.clone(),
                rw,
            }];
            let why = carry_prompt_in_file_at(&root, "codex", &prompt, &mut argv, &mut mounts)
                .unwrap_err();
            assert!(why.contains("which the seat mounts"), "{why}");
            assert!(!canon.join(".wicked-prompts").exists());
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn any_other_argument_too_large_for_an_exec_is_refused_with_the_reason() {
        // A skills list of 256 KiB, not the prompt: nothing can carry it, so arming refuses.
        let skills: Vec<String> = (0..16_000)
            .map(|i| format!("skill-number-{i:06}"))
            .collect();
        let argv = crate::execute_wrapped::build_argv(
            "claude -p {PROMPT} --skills={SKILLS}",
            "hi",
            &skills,
        );
        assert!(argv.iter().any(|a| a.len() > 256 * 1024));
        let why = check_exec_args(&argv).unwrap_err();
        assert!(why.contains("cannot pass an argument"), "{why}");
        // Many small arguments that add up past the budget are refused too.
        let many = vec!["x".repeat(100 * 1024); 12];
        assert!(check_exec_args(&many).unwrap_err().contains("at most"));
        // Many tiny ones too: each also costs its pointer in the exec's argv table.
        let tiny = vec!["x".to_string(); 120_000];
        assert!(check_exec_args(&tiny).unwrap_err().contains("at most"));
        assert_eq!(
            crate::worker_sandbox::FloorUnarmed::ArgvTooLong.as_str(),
            "argv_too_long"
        );
    }
}
