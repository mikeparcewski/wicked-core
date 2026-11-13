//! Shared seat image builders, mount planning, and launch policy for the Kubernetes runner.
//!
//! Seats run as pods on a local Kubernetes cluster (Rancher Desktop or Docker Desktop
//! Kubernetes), installed once with `wicked-core cluster install --admin-context <ctx>
//! --home <state home>`. The container CLI (`docker`, `nerdctl`, or `podman`) never runs a
//! seat: it builds images into the store shared with the cluster (pods use `imagePullPolicy:
//! Never`) and inspects a seat image before an arm ([`inspect_seat_image`]: a short-lived helper
//! that runs only the floor's own busybox over the image as data).
//! With `nerdctl`, every command uses the kubelet's `k8s.io` containerd namespace.
//!
//! The mount plan comes from [`SeatFloor`]: writable roots, read views, pinned launch files,
//! and secret masks. [`crate::k8s::seat`] carries it into the pod along with the trusted floor
//! image and Landlock policy. A pod that cannot start refuses the unit.
//!
//! `wicked-core seat-image build|status` builds or inspects the required images.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::worker_sandbox::{FloorUnarmed, SeatFloor};
use wicked_apps_core::HardenedCommand;

/// Selects the seat floor for governed units: `container` runs them in the container floor.
/// Anything else (unset included) keeps the step-0 host floor until slice C5 makes the container
/// floor the default.
pub(crate) const SEAT_FLOOR_ENV: &str = "WICKED_SEAT_FLOOR";
/// The runtime CLI to use (a name on `PATH` or an absolute path); otherwise the first of
/// `docker`, `podman`, `nerdctl` found on `PATH` or in a well-known install dir.
/// `nerdctl` always uses the kubelet's `k8s.io` containerd namespace.
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

/// The default seat base: the checks floor image (node, the pinned Rust toolchain linked into the
/// default PATH, git, a C toolchain), so every seat image is Rust-capable. The engine builds it
/// from its embedded Dockerfile ([`build_seat_image_on`]); there is no registry fallback.
fn default_base() -> String {
    crate::checks_container::image_tag()
}
const CONTAINERD_NAMESPACE: &str = "k8s.io";
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
const BUILD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
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
        c.hardened()
            .args(runtime_global_args(&self.cli))
            .stdin(Stdio::null());
        if let Some(path) = client_path(&self.cli, std::env::var_os("PATH").as_deref()) {
            c.env("PATH", path);
        }
        c
    }
}

/// Global flags before the subcommand (wicked-core#1449): the kubelet reads images from
/// containerd's `k8s.io` namespace, and pods use `imagePullPolicy: Never`, so nerdctl's default
/// namespace would leave built images invisible to the pods.
fn runtime_global_args(cli: &Path) -> &'static [&'static str] {
    if cli.file_name().is_some_and(|name| name == "nerdctl") {
        &["--namespace", CONTAINERD_NAMESPACE]
    } else {
        &[]
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
/// Rancher Desktop's `~/.rd/bin/nerdctl` is last, after every Docker and Podman install candidate.
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
    if let Some(h) = home {
        let p = h.join(".rd/bin/nerdctl");
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

/// One `version` attempt's bound. A loaded laptop (1-min load 50-80, e2e-1105 09:02Z) made the
/// docker CLI miss the old single 15 s bound while `docker version` answered by hand every time.
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(30);
/// Attempts of one runtime operation before it counts as failed: the runtime is down only after
/// this many CONSECUTIVE failures ([`with_retries`]).
pub(crate) const RUNTIME_ATTEMPTS: u32 = 3;
/// The short pause between two attempts. A runtime that is really down fails each attempt at once
/// (the CLI cannot reach its socket), so it reads down about a second later than before; only a
/// slow host pays the longer bound, and it waits instead of failing a step.
pub(crate) const RUNTIME_RETRY_PAUSE: Duration = Duration::from_millis(500);

/// Run one runtime operation up to `attempts` times (at least once), pausing `pause` between
/// attempts, and answer its first success or its LAST error: one slow or failed answer from a
/// loaded host is never the operation's outcome on its own.
pub(crate) fn with_retries<T>(
    attempts: u32,
    pause: Duration,
    sleep: &mut dyn FnMut(Duration),
    op: &mut dyn FnMut() -> Result<T, String>,
) -> Result<T, String> {
    let mut last = Err(String::new());
    for attempt in 0..attempts.max(1) {
        if attempt > 0 {
            sleep(pause);
        }
        last = op();
        if last.is_ok() {
            return last;
        }
    }
    last
}

/// [`with_retries`] with the production attempts and pause, sleeping the thread.
pub(crate) fn retrying<T>(op: &mut dyn FnMut() -> Result<T, String>) -> Result<T, String> {
    with_retries(
        RUNTIME_ATTEMPTS,
        RUNTIME_RETRY_PAUSE,
        &mut |d| std::thread::sleep(d),
        op,
    )
}

/// Whether `rt`'s engine answers `version` now: up to [`RUNTIME_ATTEMPTS`] tries, each bound by
/// [`VERSION_PROBE_TIMEOUT`] ([`retrying`]). One slow or failed answer never makes the runtime read
/// down, and so never fails a step with `container_runtime_down`: an honest wait beats a false
/// outage.
fn runtime_answers(rt: &Runtime) -> bool {
    retrying(&mut || {
        let mut probe = rt.cmd();
        probe.args(["version", "--format", "{{.Server.Version}}"]);
        match output_within(probe, rt.bound(VERSION_PROBE_TIMEOUT)) {
            Some(o) if o.status.success() => Ok(()),
            _ => Err(String::new()),
        }
    })
    .is_ok()
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
/// VM's, not the host's. Shared seat build settings cap the resulting job count separately.
/// When the runtime does not answer (podman's template differs, a
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
pub(crate) fn image_id(rt: &Runtime, tag: &str) -> Option<String> {
    let mut c = rt.cmd();
    c.args(["image", "inspect", "--format", "{{.Id}}", tag]);
    output_within(c, rt.bound(PROBE_TIMEOUT))
        .filter(|o| o.status.success())
        .and_then(|o| valid_image_id(String::from_utf8_lossy(&o.stdout).trim()))
}

/// An immutable local image id as the runtime spells it: docker's `sha256:<64 hex>` or podman's
/// bare `<64 hex>` (codex ln-r2). Kept in that spelling for the runtime's own later commands.
fn valid_image_id(id: &str) -> Option<String> {
    let hex = id.strip_prefix("sha256:").unwrap_or(id);
    (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then(|| id.to_string())
}

pub(crate) fn image_present(rt: &Runtime, tag: &str) -> bool {
    let mut c = rt.cmd();
    c.args(["image", "inspect", "--format", "{{.Id}}", tag]);
    output_within(c, rt.bound(PROBE_TIMEOUT)).is_some_and(|o| o.status.success())
}

/// Build `cli`'s seat image, composing the embedded checks recipe by default (no build context).
pub(crate) fn build_seat_image(rt: &Runtime, cli: &str) -> Result<String, String> {
    let package =
        seat_package(cli).ok_or_else(|| format!("no seat image is defined for `{cli}`"))?;
    build_seat_image_on(rt, cli, package, &SeatBase::from_env())
}

/// The base the seat images build on: the operator's own ([`BASE_IMAGE_ENV`], with its recipe
/// [`BASE_DOCKERFILE_ENV`]) or the embedded checks floor stage.
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
                image: default_base(),
                operator: false,
                recipe: None,
            },
        }
    }
}

/// One self-contained build for BuildKit OCI workers, whose solver cannot see containerd's
/// local images. Keep the toolchain recipe embedded once; the seat's global BASE declaration
/// must be removed rather than shadowing the checks recipe's global ARG.
fn default_seat_dockerfile() -> String {
    let floor = crate::checks_container::DOCKERFILE.replacen(
        "FROM ${BASE}\n",
        "FROM ${BASE} AS wicked_toolchain\n",
        1,
    );
    let seat = SEAT_DOCKERFILE
        .lines()
        .filter(|line| !line.starts_with("ARG BASE=") && !line.starts_with("# syntax="))
        .map(|line| {
            if line == "FROM ${BASE}" {
                "FROM wicked_toolchain"
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("{floor}\n{seat}\n")
}

/// [`build_seat_image`] on `base`: an operator base that is missing is rebuilt from its recipe
/// first; a missing operator base with no recipe fails naming the base and the
/// fix. The default base is the checks floor image: it is ensured (built from the engine's
/// embedded Dockerfile, again if a GC removed it) before the self-contained seat build.
fn build_seat_image_on(
    rt: &Runtime,
    cli: &str,
    package: SeatPackage,
    base: &SeatBase,
) -> Result<String, String> {
    let tag = seat_image_tag(cli, &base.image).expect("package exists");
    if base.operator && !image_present(rt, &base.image) {
        if let Some(recipe) = &base.recipe {
            rebuild_base(rt, &base.image, recipe)?;
        }
    }
    if !base.operator && base.image == default_base() && !image_present(rt, &base.image) {
        eprintln!(
            "wicked-core: building the checks floor image {} (the seat base)",
            base.image
        );
        build_image(rt, crate::checks_container::DOCKERFILE, &base.image, &[])?;
    }
    let mut args = vec![format!("CLI={cli}")];
    let dockerfile = if base.operator {
        args.push(format!("BASE={}", base.image));
        SEAT_DOCKERFILE.to_string()
    } else {
        default_seat_dockerfile()
    };
    args.extend(package.build_args());
    build_image(rt, &dockerfile, &tag, &args).map_err(|e| {
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
            return Ok(tag);
        }
    }
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
    // The pod's own mounts too (codex b2-gw #2): its unit dir (env, argv, rc) and a read-only
    // unit's view source.
    let reserved = [
        FLOOR_DIR,
        "/wicked-canary",
        "/tmp",
        "/home/seat",
        crate::k8s::render::UNIT_DIR,
        crate::k8s::render::VIEW_SOURCE,
    ]
    .map(PathBuf::from);
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
        return Ok((img, None));
    }
    let package =
        seat_package(cli).ok_or_else(|| format!("no seat image is defined for `{cli}`"))?;
    static BUILD: Mutex<()> = Mutex::new(());
    let tag = seat_image_tag(cli, &base.image).expect("package exists");
    if image_present(rt, &tag) {
        return Ok((tag, None));
    }
    let start = Instant::now();
    let _one = BUILD.lock();
    if image_present(rt, &tag) {
        // Another unit built it while this one waited.
        let waited = start.elapsed();
        return Ok((tag, (waited >= BUILD_WAIT_NOTE).then_some(waited)));
    }
    eprintln!("wicked-core: building seat image {tag} (it is not on the container runtime)");
    let built = build_seat_image_on(rt, cli, package, base)?;
    Ok((built, Some(start.elapsed())))
}

/// Whether seats run with no network at all ([`NETWORK_ENV`]` = none`).
pub(crate) fn offline_seats() -> bool {
    std::env::var(NETWORK_ENV).is_ok_and(|v| v.trim() == "none")
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
    // Every existing mask: a read root or argv path added to the plan may be an ancestor.
    // The pod mounts the deeper
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
pub(crate) fn landrun_prefix(
    mounts: &[Mount],
    gateway_ports: &[u16],
    own_ports: &[u16],
) -> Vec<String> {
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

/// Removes a directory the container floor allocated for one unit (its fenced scratch) when
/// dropped, however the unit ends.
#[derive(Debug)]
pub(crate) struct RemoveOnDrop(pub(crate) PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Reaps a seat pod when dropped (the end of the run, however it ends).
#[derive(Debug)]
pub(crate) struct ReapOnDrop(pub(crate) Option<SeatContainer>);

impl Drop for ReapOnDrop {
    fn drop(&mut self) {
        if let Some(seat) = self.0.take() {
            crate::k8s::seat::reap(&seat.name);
        }
    }
}

/// The seat pod identity to reap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SeatContainer {
    pub(crate) name: String,
}
/// Whether the seat CLI needs stdin as a pipe, closed by the runner.
pub(crate) fn stdin_must_be_a_pipe(cmd: &Command) -> bool {
    home_is_config_home(&cmd.get_program().to_string_lossy())
}

/// Whether `cli`'s configuration home IS its process home: agy publishes no configuration-home
/// variable (its `~/.gemini` and `~/.config/antigravity` are its whole state, conversations
/// included), so its seat home is its `HOME`. Every other CLI keeps the tmpfs home and names its
/// configuration home with its own variable.
fn home_is_config_home(cli: &str) -> bool {
    wicked_apps_core::spawn::SeatCli::from_binary(cli) == wicked_apps_core::spawn::SeatCli::Agy
}

/// The devices a seat's CLI must open read-write beyond `/dev/null`: agy's shell tool runs every
/// command on a pseudo-terminal, so it needs `/dev/ptmx` and `/dev/pts` (without them every
/// command fails "failed to create PTY: open /dev/ptmx: permission denied", agy spike). Every
/// other CLI gets none: the grant is agy's alone.
pub(crate) fn seat_devices(cli: &str) -> &'static [&'static str] {
    match wicked_apps_core::spawn::SeatCli::from_binary(cli) {
        wicked_apps_core::spawn::SeatCli::Agy => &["/dev/ptmx", "/dev/pts"],
        _ => &[],
    }
}

/// `v` (a path or a path list) with each absolute entry whose canonical form is mounted
/// re-spelled canonically; anything else (not a path, not mounted) is returned unchanged.
pub(crate) fn canonical_spelling(v: &std::ffi::OsStr, mounts: &[PathBuf]) -> std::ffi::OsString {
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
pub(crate) fn container_path(carrier: &std::ffi::OsStr, mounts: &[PathBuf]) -> String {
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

/// A seat armed as a pod: the wrapper (`kubectl exec` into the seat pod), the argv to run after
/// it, and the tool that runs it (`kubectl`). Its `Debug` names the env's
/// variables, never their values (a unit key, a claude seat's pass-through token).
pub(crate) struct ArmedContainer {
    pub(crate) wrapper: Vec<String>,
    pub(crate) argv: Vec<String>,
    pub(crate) tool: &'static str,
    /// Reaps the seat's pod on every path out of the unit (held by the
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
    gw: crate::k8s::gw::Gw,
    key: String,
    /// `<run>#<ord>@<nonce>`: the identity the gateway's records carry for this execution.
    pub(crate) unit: String,
    since: String,
}

impl GatewayLease {
    /// Revoke the seat's key and return what the gateway recorded for it (model requests:
    /// status, model, tool calls, text, usage — never a credential).
    pub(crate) fn release(mut self) -> Vec<crate::seat_gateway::GatewayRecord> {
        // The gateway pod as it runs now, not as it ran at arm (review r6 #15).
        self.gw = self.gw.current();
        let records = self.gw.records(&self.since, &self.unit);
        self.revoke();
        records
    }

    /// Revoke the key once; a revocation the gateway does not confirm is retried, and if it still
    /// is not confirmed the gateway is stopped (no served policy outlives a key the engine meant
    /// to revoke; the next arm publishes a config without it before the gateway serves again).
    fn revoke(&mut self) {
        let key = std::mem::take(&mut self.key);
        if key.is_empty() {
            return;
        }
        self.gw = self.gw.current();
        let mut last = String::new();
        for _ in 0..REVOKE_ATTEMPTS {
            match crate::k8s::gw::deregister_live(&self.gw, &key) {
                Ok(()) => return,
                Err(e) => last = e,
            }
        }
        let removed = crate::k8s::gw::quarantine(&self.gw);
        eprintln!(
            "wicked-core: gateway key for {} not revoked ({last}); gateway {}",
            self.unit,
            if removed {
                "stopped"
            } else {
                "could NOT be stopped"
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

/// The longest prompt passed to a seat container as an argument (core#1228). Linux refuses to
/// exec any single argument over `MAX_ARG_STRLEN` (32 pages, 128 KiB): docker-init then fails
/// with "argument list too long" before the CLI starts. A longer prompt rides a file instead
/// ([`carry_prompt_in_file`]), with room to spare for the pointer and the rest of the argv.
pub(crate) const PROMPT_ARGV_MAX: usize = 64 * 1024;

/// Pi's options that take a value (pi 1.1 `--help`): a prompt right after one of these is that
/// option's value, not a positional message; every other flag before it is a boolean.
const PI_VALUE_OPTIONS: &[&str] = &[
    "--provider",
    "--model",
    "--api-key",
    "--system-prompt",
    "--append-system-prompt",
    "--mode",
    "--session",
    "--session-id",
    "--fork",
    "--session-dir",
    "--name",
    "-n",
    "--models",
    "--tools",
    "-t",
    "--exclude-tools",
    "-xt",
    "--thinking",
    "--extension",
    "-e",
    "--skill",
    "--prompt-template",
    "--theme",
    "--export",
    "--tui-mode",
];

/// Linux's limit on one exec argument (`MAX_ARG_STRLEN`, 32 pages of 4 KiB).
const EXEC_ARG_MAX: usize = 128 * 1024;

/// What all the seat's arguments together may take: half of macOS's 1 MiB `ARG_MAX` (the host's
/// runtime client receives these arguments too, with its wrapper and environment), and well under
/// the 2 MiB a Linux exec takes.
const EXEC_ARGS_TOTAL_MAX: usize = 512 * 1024;

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
        // What the argument before the prompt makes of it: pi's `@path` attaches a file only as
        // a positional argument (after `-p`/`--print` or another positional); as the value of
        // `--system-prompt`/`--append-system-prompt` pi loads a plain path's contents itself;
        // as any other option's value, the read-the-file sentence stands in.
        let prev = out.last().map(String::as_str).unwrap_or("");
        let option_value = PI_VALUE_OPTIONS.contains(&prev);
        if cli == "pi" && a != prompt && a.contains(prompt) {
            // A decorated positional (`"<text> Be concise."`) gets the file as its own `@path`
            // before it; anything glued to an option gets the read-the-file sentence.
            if option_value || a.starts_with('-') {
                out.push(a.replace(prompt, &prompt_pointer("", &file, prompt.len()).join(" ")));
            } else {
                out.push(format!("@{}", file.display()));
                out.push(a.replace(prompt, "the task in the attached file"));
            }
            carried = true;
        } else if a == prompt && cli == "pi" && option_value {
            if matches!(prev, "--system-prompt" | "--append-system-prompt") {
                out.push(file.to_string_lossy().into_owned());
            } else {
                out.push(prompt_pointer("", &file, prompt.len()).join(" "));
            }
            carried = true;
        } else if a == prompt {
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

/// Arm a seat pod, calling `on_built(image, waited)` as soon as its image had to be built for this
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
    before_copy: &mut dyn FnMut(&MountView) -> Result<(), String>,
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
    crate::k8s::clock::mark("plan");
    // The directory holding the launch files is not writable inside a container: the gate
    // hook runs on the host, so the seat has nothing to write there (and nothing to plant).
    let pin_dirs: Vec<PathBuf> = spec
        .pinned
        .iter()
        .filter_map(|p| p.parent().map(Path::to_path_buf))
        .collect();
    spec.writable.retain(|w| !pin_dirs.contains(w));
    // The engine's state home: its cluster settings, and its own credential the mounts cover.
    let state_home = gateway_dir
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .or_else(|| wicked_apps_core::spawn::worker_home_base().ok());
    // The unit's mounts and the view of them, settled before any image or cluster work.
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
    // The engine's own cluster credential (`<state home>/k8s`, codex b5 #1) is covered the same
    // way: never in a replica, and a seat-writable tree over it is refused.
    // From the state home the cluster settings were read from (codex r2: an offline seat has
    // no gateway dir, and is covered all the same).
    // (No state home is refused as `no_cluster` below, before any copy-in.)
    let engine_k8s = state_home.as_ref().map(|h| h.join("k8s"));
    cover_gateway_dir(&mut mounts, engine_k8s.as_deref())
        .map_err(|e| (FloorUnarmed::GatewayUnavailable, e))?;
    // core#1228: a prompt too long for one exec argument rides a private read-only file.
    let prompt_file = match prompt.filter(|p| p.len() > PROMPT_ARGV_MAX) {
        Some(p) => carry_prompt_in_file(cli, p, &mut argv, &mut mounts)
            .map_err(|e| (FloorUnarmed::ArgvTooLong, e))?,
        None => None,
    };
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
    // (core#1461) What the caller must settle from the final view BEFORE anything is copied in,
    // and before anything outside this process (image, cluster) is touched: the pod mounts each
    // launch file by subPath, bound to the inode the copy-in made, so a file rewritten after the
    // arm (claude's narrowed `--settings`) never reached the seat.
    before_copy(&view).map_err(|e| (FloorUnarmed::RootUnresolvable, e))?;
    // The image builder (D2: it builds the seat, floor and sync images into the store the
    // cluster shares; it never runs a seat).
    let rt = runtime().map_err(|w| (w, String::new()))?;
    let (image, built_in) =
        ensure_seat_image_noting(&rt, cli).map_err(|e| (FloorUnarmed::NoSeatImage, e))?;
    if let Some(waited) = built_in {
        on_built(&image, waited);
    }
    let floor_image = ensure_floor_image(&rt).map_err(|e| (FloorUnarmed::NoSeatImage, e))?;
    let floor_id = image_id(&rt, &floor_image).ok_or_else(|| {
        (
            FloorUnarmed::NoSeatImage,
            format!("floor image {floor_image} vanished"),
        )
    })?;
    let sync_image = crate::checks_container::image_tag();
    if !image_present(&rt, &sync_image) {
        build_image(&rt, crate::checks_container::DOCKERFILE, &sync_image, &[])
            .map_err(|e| (FloorUnarmed::NoSeatImage, e))?;
    }
    crate::k8s::clock::mark("images");
    // A seat's only route out is the daemon's model gateway: an engine with no gateway dir (no
    // state home) runs only offline seats.
    if !offline_seats() && gateway_dir.is_none() {
        return Err((
            FloorUnarmed::GatewayUnavailable,
            format!("no model gateway is configured; set {NETWORK_ENV}=none for an offline seat"),
        ));
    }
    // The engine's cluster: the daemon's own (its state home holds the kubeconfig `cluster
    // install` wrote), answering, with the pinned controller and a pod pids limit (B2).
    let home = state_home.ok_or_else(|| {
        (
            FloorUnarmed::NoCluster,
            "no state home for the cluster settings".to_string(),
        )
    })?;
    let kube = crate::k8s::kube_for(&home)?;
    crate::k8s::cluster::probe_cached(&kube)?;
    crate::k8s::cluster::ensure_pids_limited(&kube, &floor_image)?;
    crate::k8s::clock::mark("cluster_probe");
    // The seat image cannot move a mount (a link where one lands, a declared volume).
    let dests: Vec<PathBuf> = mounts
        .iter()
        .map(|m| m.dst().to_path_buf())
        .chain(std::iter::once(
            inp.cwd.canonicalize().unwrap_or_else(|_| inp.cwd.clone()),
        ))
        .collect();
    inspect_seat_image(&rt, &floor_id, &image)
        .and_then(|f| check_seat_image(&f, &dests))
        .map_err(|e| (FloorUnarmed::NoSeatImage, e))?;
    crate::k8s::clock::mark("image_inspect");
    let online = gateway_dir.is_some() && !offline_seats();
    gateway_route_defined(cli, online).map_err(|e| (FloorUnarmed::GatewayUnavailable, e))?;
    let (env, files, lease, seat_args, copilot_account, gateway_ip) =
        match gateway_dir.filter(|_| online) {
            Some(dir) => {
                let gw = crate::k8s::gw::ensure(&kube, dir)?;
                crate::k8s::clock::mark("gateway_ensure");
                // The broker's one refresher runs beside this daemon's gateway (a credential that
                // expired between runs is renewed, by its own CLI, before the seat starts). Each
                // pass finds the gateway again (a quarantine re-creates it).
                #[cfg(unix)]
                {
                    let (kube, dir) = (kube.clone(), dir.to_path_buf());
                    crate::seat_refresh::start(&dir.clone(), move || {
                        crate::k8s::login::K8sIo::attach(&kube, &dir, false)
                            .map(|io| Box::new(io) as Box<dyn crate::seat_login::BrokerIo>)
                    });
                }
                let down = |e: String| (FloorUnarmed::GatewayUnavailable, e);
                // The agent's sign-in, read from the broker the gateway alone mounts.
                let kind = crate::k8s::gw::seat_sign_in(&gw, cli)
                    .map_err(|e| (FloorUnarmed::AgentSignedOut, e))?;
                if cli == "agy" {
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
                crate::k8s::clock::mark("sign_in");
                let unit = format!(
                    "{run_id}#{ord}@{}",
                    &uuid::Uuid::new_v4().simple().to_string()[..8]
                );
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
                    // core#1024: the operator's own setup-token, passed through; it reaches the pod
                    // only in the unit env file, over exec stdin (never an argv).
                    let token = crate::k8s::gw::claude_seat_token(&gw)
                        .map_err(|e| (FloorUnarmed::AgentSignedOut, e))?;
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
                let ip = gw.service_ip.clone();
                let lease = GatewayLease {
                    gw: gw.clone(),
                    key: key.key.clone(),
                    unit,
                    since,
                };
                crate::k8s::gw::register_live(&gw, key.clone()).map_err(down)?;
                crate::k8s::clock::mark("key_register");
                (
                    env,
                    files,
                    Some(lease),
                    seat_args,
                    copilot_account,
                    Some(ip),
                )
            }
            None => (Vec::new(), Vec::new(), None, Vec::new(), None, None),
        };
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
    // B17: a read-only unit's tree is a throwaway view in the pod, writable there.
    let tree = spec
        .tree
        .canonicalize()
        .unwrap_or_else(|_| spec.tree.clone());
    let read_only_view = inp.read_only;
    let mut land_mounts = mounts.clone();
    if read_only_view {
        land_mounts.retain(|m| m.dst() != tree.as_path());
        land_mounts.push(Mount::Bind {
            path: tree.clone(),
            rw: true,
        });
    }
    let landrun = seat_launch_policy(cli, landrun_prefix(&land_mounts, GATEWAY_PORTS, &own_ports));
    let protected: Vec<PathBuf> = spec.protected.clone();
    let plan_mounts = crate::k8s::seat::plan_mounts(&mounts);
    // Canonical, like the mount plan's paths (review r2: macOS spells a worker home under /var
    // or /tmp as /private/...).
    let canon =
        |p: &Path| crate::worker_sandbox::canon_lenient(p).unwrap_or_else(|| p.to_path_buf());
    let seed_base =
        crate::worker_sandbox::seat_cache_dir(wicked_apps_core::spawn::SeatCli::from_binary(cli))
            .and_then(|d| d.parent().map(Path::to_path_buf))
            .map(|d| canon(&d));
    let seat_dirs: Vec<PathBuf> = inp.seat_dirs.iter().map(|d| canon(d)).collect();
    let scratch_dir: Option<PathBuf> = inp.scratch.as_deref().map(canon);
    let role = if crate::k8s::seat::one_shot_scope(run_id).is_some() {
        "judge"
    } else {
        "seat"
    };
    // In the mount plan's own spelling (review r5: `/tmp/...` names a `/private` path there).
    let write_roots: Vec<PathBuf> = inp
        .write_roots
        .iter()
        .map(|w| w.canonicalize().unwrap_or_else(|_| w.clone()))
        .collect();
    // The run's node_modules cache for this tree, when the unit's provisioning filled one (B14).
    let deps = crate::checks_container::seat_claim(run_id, &tree);
    let arm_spec = crate::k8s::seat::ArmSpec {
        run_id,
        ord,
        cli,
        image: &image,
        floor_image: &floor_image,
        sync_image: &sync_image,
        workdir: &workdir,
        tree: &tree,
        view: read_only_view,
        mounts: &plan_mounts,
        protected: &protected,
        write_roots: &write_roots,
        seat_dirs: &seat_dirs,
        scratch: scratch_dir.as_deref(),
        seed_base: seed_base.as_deref(),
        memory: &k8s_memory(memory_from(memory.as_deref())),
        gateway_ip: gateway_ip.as_deref(),
        // The engine's own ceiling for a turn (`WICKED_UNIT_TIMEOUT_SECS`; a unit's budget only
        // lowers it): the pod outlives no turn the engine would still wait for (review r4).
        deadline: crate::workflow::unit_timeout_ceiling(),
        role,
        deps: deps.as_deref(),
    };
    let (seat, wrapper) = crate::k8s::seat::arm(&kube, &arm_spec, landrun)?;
    // The gate hook judges this unit as a pod seat's (B17).
    let mut env = env;
    env.push((crate::gate_hook::SEAT_POD_ENV.to_string(), "1".to_string()));
    let cleanup = ReapOnDrop(Some(SeatContainer {
        name: seat.id.clone(),
    }));
    let argv = with_seat_args(image_argv(&argv, cli), seat_args);
    // Checked on what the pod will exec (the seat's own args included); a refusal here drops
    // the lease (revoking its key) and the pod's cleanup with it.
    check_exec_args(&argv).map_err(|e| (FloorUnarmed::ArgvTooLong, e))?;
    Ok(ArmedContainer {
        wrapper,
        argv,
        prompt_file,
        tool: "kubectl",
        cleanup,
        env,
        files,
        lease,
        copilot_account,
        view,
    })
}

/// The launcher argv a seat pod runs under (pure): the floor entry and canary, then the Landlock
/// policy `prefix`, with `cli`'s devices (agy's pseudo-terminal) granted read-write before
/// landrun's own `--` separator.
pub(crate) fn seat_launch_policy(cli: &str, prefix: Vec<String>) -> Vec<String> {
    let mut landrun: Vec<String> = vec![SEAT_ENTRY.into(), CANARY_FILE.into(), "--".into()];
    landrun.extend(prefix);
    if let Some(at) = landrun.iter().position(|a| a == "--").and_then(|first| {
        landrun[first + 1..]
            .iter()
            .position(|a| a == "--")
            .map(|i| first + 1 + i)
    }) {
        for d in seat_devices(cli).iter().rev() {
            landrun.splice(at..at, ["--rw".to_string(), (*d).to_string()]);
        }
    }
    landrun
}

/// A container memory limit (`3g`, `512m`) as a Kubernetes quantity (`3Gi`, `512Mi`).
pub(crate) fn k8s_memory(m: &str) -> String {
    let m = m.trim();
    // `6gb` / `512mb` / `64kb` (accepted by the floor's memory setting, B-3 review r2).
    let m = match m.char_indices().last() {
        Some((i, 'b' | 'B')) if m[..i].chars().last().is_some_and(|c| "gGmMkK".contains(c)) => {
            &m[..i]
        }
        _ => m,
    };
    match m.char_indices().last() {
        Some((i, 'g' | 'G')) => format!("{}Gi", &m[..i]),
        Some((i, 'm' | 'M')) => format!("{}Mi", &m[..i]),
        Some((i, 'k' | 'K')) => format!("{}Ki", &m[..i]),
        _ => m.to_string(),
    }
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
    let opened: Vec<PathBuf> = view.roots.iter().map(|(r, _)| r.clone()).collect();
    Ok(entries_around(&d, &opened, view, dir))
}

/// The walk behind [`narrow_fenced_dir`], for a canonical `d` and an explicit set of `opened`
/// roots (those strictly under `d` count): `None` keeps the rule over `d` (no opened root under it,
/// a mount at or above it, or a mount on the way to an opened root that is not inside one);
/// `Some(entries)` are the other mounted entries under `d` to deny by name instead. `shown` is how
/// the fence notice names the directory.
pub(crate) fn entries_around(
    d: &Path,
    opened: &[PathBuf],
    view: &MountView,
    shown: &Path,
) -> Option<Vec<PathBuf>> {
    let d = d.to_path_buf();
    if view.mounted.iter().any(|m| d.starts_with(m)) {
        return None;
    }
    let opened: Vec<&PathBuf> = opened
        .iter()
        .filter(|r| r.starts_with(&d) && **r != d)
        .collect();
    if opened.is_empty() {
        return None;
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
                shown.display()
            );
            return None;
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
    Some(entries)
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
    let usage = "usage: wicked-core seat-image build [<cli>…|all] | \
                 seat-image status [--json] [<cli>…]";
    match args.first().map(String::as_str) {
        Some("build" | "status") if !unknown.is_empty() => {
            eprintln!(
                "wicked-core seat-image: no seat image is defined for {}",
                unknown.join(", ")
            );
            1
        }
        Some("build") => {
            let mut rc = 0;
            match ensure_floor_image(&rt) {
                Ok(tag) => println!("floor: {tag}"),
                Err(e) => {
                    eprintln!("floor: {e}");
                    rc = 1;
                }
            }
            for cli in &wanted {
                // The operator's own image is inspected, never built.
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
                    }
                    Err(e) => {
                        eprintln!("{cli}: {e}");
                        rc = 1;
                    }
                }
            }
            rc
        }
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
            let word = |present: bool| if present { "present" } else { "missing" };
            if let Some(b) = &st.base {
                println!("base: {} {}", b.image, word(b.present));
            }
            println!("floor: {} {}", st.floor.image, word(st.floor.present));
            for s in &st.seats {
                println!("{}: {} {}", s.cli, s.image, word(s.present));
            }
            0
        }
        _ => {
            eprintln!("{usage}");
            2
        }
    }
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
}

/// One seat CLI's image in [`ImageStatus`].
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SeatImageState {
    pub(crate) cli: String,
    pub(crate) image: String,
    pub(crate) present: bool,
    /// The operator's own image (`WICKED_SEAT_IMAGE_<CLI>`): the engine never builds it.
    pub(crate) operator_image: bool,
}

/// `seat-image status --json`: the images seats need and whether they are present.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImageStatus {
    pub(crate) runtime: String,
    /// The operator's seat base ([`BASE_IMAGE_ENV`]); `null` when the default base is used.
    pub(crate) base: Option<ImageState>,
    /// Whether the operator base has a recipe the engine can rebuild it from: [`BASE_DOCKERFILE_ENV`]
    /// names a readable file (false with no operator base).
    pub(crate) base_recipe: bool,
    pub(crate) floor: ImageState,
    pub(crate) seats: Vec<SeatImageState>,
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
    let floor_tag = floor_image_tag();
    let listed = list_images(rt)?;
    let base_image = cfg.base.clone().unwrap_or_else(default_base);
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
                operator_image: over.is_some(),
            })
        })
        .collect();
    Ok(ImageStatus {
        runtime: rt.cli.display().to_string(),
        base_recipe: base.is_some() && cfg.recipe.as_deref().is_some_and(readable_file),
        base,
        floor,
        seats,
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

    /// core#1461 (runs d0f84ace, 12a50f95): the claude seat's `--settings` file was narrowed around
    /// the run's declared draft root only AFTER the arm, whose copy-in had already put the host
    /// fence into the replica; the pod mounts that file by subPath (bound to the copy-in's inode),
    /// so the seat kept `Read(<state home>/interactive-drafts/**)` and could neither read nor edit
    /// its own draft. The arm now hands the caller the final mount view before ANY work outside
    /// this process (this arm has no runtime or cluster to reach: its refusal is the callback's
    /// own), and the settings narrowed from that view are what any copy-in would read.
    #[test]
    fn the_arm_settles_the_launch_files_from_the_view_before_any_copy_in_1461() {
        let _env = crate::test_env::ENV_LOCK
            .read()
            .unwrap_or_else(|p| p.into_inner());
        let base = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("wicked-1461-{}", uuid::Uuid::new_v4().simple()));
        let sandbox = base.join("sandbox");
        let state = base.join("state");
        let draft = state.join("interactive-drafts").join("deck-1");
        let gov = base.join("gov").join("attempt-0");
        let gateway = state.join("gateway");
        for d in [&sandbox, &draft, &gov, &gateway] {
            std::fs::create_dir_all(d).unwrap();
        }
        let sh = crate::execute_wrapped::rule_path(&state).unwrap();
        let host_fence = [
            format!("Read({sh}/interactive-drafts)"),
            format!("Read({sh}/interactive-drafts/**)"),
            format!("Edit({sh}/**)"),
        ];
        let settings = gov.join("settings-unit-2.json");
        std::fs::write(
            &settings,
            serde_json::json!({ "permissions": { "deny": host_fence } }).to_string(),
        )
        .unwrap();
        let inp = crate::worker_sandbox::FloorInputs {
            cwd: sandbox.clone(),
            write_roots: vec![sandbox.clone(), draft.clone()],
            read_only: false,
            seat_dirs: vec![gov.clone()],
            scratch: None,
            unmask: vec![],
            pinned: vec![settings.clone()],
            net: vec![],
            home: None,
            trusted_git_dir: None,
        };
        let argv: Vec<String> = ["claude", "-p", "draft it", "--settings"]
            .iter()
            .map(|s| s.to_string())
            .chain(std::iter::once(settings.to_string_lossy().into_owned()))
            .collect();
        let deny_of = || -> Vec<String> {
            let v: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&settings).unwrap()).unwrap();
            v["permissions"]["deny"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r.as_str().unwrap().to_string())
                .collect()
        };
        let mut seen: Option<(MountView, Vec<String>)> = None;
        let mut before_copy = |v: &MountView| {
            crate::execute_wrapped::narrow_container_fence(
                &mut [],
                Some("draft it"),
                Some(&settings),
                None,
                Some(&state),
                std::slice::from_ref(&draft),
                v,
            )?;
            seen = Some((v.clone(), deny_of()));
            Err("stopped by the test before any copy-in".to_string())
        };
        let out = arm_noting(
            &inp,
            &argv,
            "run-1461",
            2,
            &[],
            None,
            Some("draft it"),
            Some(&gateway),
            &mut |_, _| {},
            &mut before_copy,
        );
        match out {
            Err((why, detail)) => {
                assert_eq!(why, FloorUnarmed::RootUnresolvable);
                assert_eq!(detail, "stopped by the test before any copy-in");
            }
            Ok(_) => panic!("the test's callback refuses the arm"),
        }
        let (view, at_copy) = seen.expect("the arm hands the caller its view before any copy-in");
        assert!(
            view.roots.iter().any(|(p, rw)| *p == draft && *rw),
            "the declared draft root is a writable root of the view: {view:?}"
        );
        assert!(
            view.mounted.contains(&settings),
            "the launch file is mounted (by itself) in the view: {view:?}"
        );
        for rule in &host_fence {
            assert!(
                !at_copy.contains(rule),
                "the settings a copy-in reads no longer deny the draft ({rule}): {at_copy:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// One failed (or slow) attempt is not the outcome: the second attempt answers, after one
    /// short pause. A single-attempt probe read this runtime down (e2e-1105 09:02Z).
    /// The floor's and every seat's Node: one pinned release (both Dockerfiles' `BASE` and the
    /// seat builder's default), recent enough for what the product repos run on it — crew's SQLite
    /// uploads need Node 24.12 or 25.1 or later (`packages/crew/src/sources/uploads.ts`,
    /// `sqliteAvailable`), and every product repo's `engines.node` (as of 2026-11-11; a repo that
    /// raises its floor raises it here too).
    #[test]
    fn the_floor_and_seat_node_satisfies_every_product_repos_engines() {
        fn version_of(base: &str) -> (u32, u32, u32) {
            let tag = base.strip_prefix("node:").expect("a node image");
            let v = tag.split('-').next().unwrap();
            let p: Vec<u32> = v
                .split('.')
                .map(|x| x.parse().expect("a pinned release"))
                .collect();
            assert_eq!(
                p.len(),
                3,
                "pinned to a full release, not a floating tag: {base}"
            );
            (p[0], p[1], p[2])
        }
        let arg = |df: &str| -> String {
            df.lines()
                .find_map(|l| l.strip_prefix("ARG BASE="))
                .expect("ARG BASE")
                .trim()
                .to_string()
        };
        let bases = [
            arg(crate::checks_container::DOCKERFILE),
            arg(SEAT_DOCKERFILE),
        ];
        assert!(
            bases.iter().all(|b| b == &bases[0]),
            "one Node for floor and seats: {bases:?}"
        );
        let v = version_of(&bases[0]);
        let (major, minor, _) = v;
        assert!(
            major > 25 || (major == 25 && minor >= 1) || (major == 24 && minor >= 12),
            "crew's uploads need Node 24.12, 25.1 or later: {v:?}"
        );
        for (repo, engines) in [
            ("wicked-crew", ">=22.12.0"),
            ("wicked-crew/packages/crew", ">=22.0.0"),
            ("wicked-studio", ">=22.0.0"),
            ("wicked-interactive", ">=20.0.0"),
            ("wicked-garden", ">=20.0.0"),
            ("wicked-installer", ">=20.0.0"),
            ("wicked-bus", ">=20.0.0"),
            ("wicked-ledger", ">=22.0.0"),
            ("wicked-vault", ">=20.0.0"),
        ] {
            let min = engines.strip_prefix(">=").expect("a lower bound");
            let m: Vec<u32> = min.split('.').map(|x| x.parse().unwrap()).collect();
            assert!(
                v >= (m[0], m[1], m[2]),
                "{repo} engines.node {engines}: {v:?}"
            );
        }
    }

    /// core#1401: the floor image and every seat image carry Python's test toolchain (pytest,
    /// PyYAML, zstandard) and jq so a pytest suite (wicked-garden) yields floor evidence — installed from
    /// Debian's archive with the rest of the image, never from PyPI or another registry.
    #[test]
    fn the_floor_and_seat_images_carry_pytest_from_the_debian_archive() {
        for (name, df) in [
            ("checks floor", crate::checks_container::DOCKERFILE),
            ("seat", SEAT_DOCKERFILE),
        ] {
            for pkg in ["python3-pytest", "python3-yaml", "python3-zstandard", "jq"] {
                assert!(df.contains(pkg), "the {name} image installs {pkg}");
            }
            for untrusted in [
                "pip install",
                "pip3 install",
                "pypi.org",
                "astral.sh",
                "uv pip",
            ] {
                assert!(
                    !df.contains(untrusted),
                    "the {name} image never uses {untrusted}"
                );
            }
        }
        // A seat makes its own venv (pip bootstraps from this Debian package).
        assert!(
            SEAT_DOCKERFILE.contains("python3-venv"),
            "the seat image installs python3-venv"
        );
    }

    #[test]
    fn one_slow_runtime_answer_is_retried_not_final() {
        let mut calls = 0;
        let mut pauses = Vec::new();
        let got = with_retries(
            3,
            Duration::from_millis(500),
            &mut |d| pauses.push(d),
            &mut || {
                calls += 1;
                if calls == 1 {
                    Err("`docker version` did not answer".to_string())
                } else {
                    Ok("29.5.3")
                }
            },
        );
        assert_eq!(got, Ok("29.5.3"));
        assert_eq!(calls, 2);
        assert_eq!(pauses, vec![Duration::from_millis(500)]);
    }

    /// Down only after consecutive failures: every attempt is made, and the last error stands.
    #[test]
    fn a_runtime_fails_only_after_every_attempt_failed() {
        let mut calls = 0;
        let got: Result<(), String> = with_retries(3, Duration::ZERO, &mut |_| {}, &mut || {
            calls += 1;
            Err(format!("attempt {calls}"))
        });
        assert_eq!(got, Err("attempt 3".to_string()));
        assert_eq!(calls, 3);
    }

    /// The production policy: retried, with a bound longer than the old single 15 s and a short
    /// pause (a real outage still reads down within about a second more).
    #[test]
    fn the_runtime_probe_retries_with_a_patient_bound() {
        let attempts = RUNTIME_ATTEMPTS;
        assert!(attempts >= 2, "{attempts}");
        assert!(VERSION_PROBE_TIMEOUT > Duration::from_secs(15));
        assert!(RUNTIME_RETRY_PAUSE <= Duration::from_secs(2));
    }

    /// A slow fake runtime through the PRODUCTION probe: its first `version` fails (the loaded
    /// host's miss), its second answers. `runtime_answers` reads it up; the old single attempt
    /// read it down.
    #[test]
    fn the_production_probe_reads_a_slow_runtime_up() {
        let dir = c4_temp();
        let cli = dir.join("docker");
        let calls = dir.join("calls");
        crate::test_env::install_stub(
            &cli,
            &format!(
                "#!/bin/sh\nn=$(($(cat '{c}' 2>/dev/null || echo 0) + 1))\necho $n > '{c}'\n\
                 [ \"$n\" -ge 2 ] || exit 1\necho 29.5.3\n",
                c = calls.display()
            ),
        );
        assert!(runtime_answers(&Runtime::answering(cli)));
        assert_eq!(std::fs::read_to_string(&calls).unwrap().trim(), "2");
    }

    /// A runtime stub that logs its argv and answers `image inspect` only for the tags a `build`
    /// made (or `present` names).
    fn image_stub(dir: &Path, present: &[&str]) -> PathBuf {
        // Named `docker` so these fixtures receive no runtime-specific global flags.
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
                 if [ \"$1\" = build ]; then cat > '{dockerfile}'; t=; while [ $# -gt 0 ]; do\n\
                   [ \"$1\" = -t ] && t=$2; shift; done; echo \"$t\" >> '{have}'; fi\n\
                 exit 0\n",
                log = dir.join("calls.log").display(),
                dockerfile = dir.join("Dockerfile").display(),
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
    fn default_seat_build_has_one_toolchain_stage_and_no_local_image_from() {
        let df = default_seat_dockerfile();
        let froms: Vec<_> = df.lines().filter(|l| l.starts_with("FROM ")).collect();
        assert_eq!(
            froms,
            ["FROM ${BASE} AS wicked_toolchain", "FROM wicked_toolchain"]
        );
        assert_eq!(df.matches("ARG BASE=").count(), 1);
        assert_eq!(df.matches("ARG RUST_TOOLCHAIN=").count(), 1);
        assert_eq!(df.matches("https://sh.rustup.rs").count(), 1);
        assert!(!df.contains("FROM wicked-checks-floor:"));
    }

    #[test]
    fn toolchain_ci_can_execute_the_binary_on_its_read_only_container() {
        let ci = include_str!("../.github/workflows/ci.yml");
        let job = ci.split("  seat-toolchain:").nth(1).unwrap();
        assert!(job.contains("--tmpfs /tmp:exec"));
        assert!(job.contains("--network none --read-only --user 1000:1000"));
        assert!(job.contains(
            "-i PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin HOME=/tmp"
        ));
        assert!(!job.contains("--build-arg BASE=wicked-checks-floor:"));
    }

    #[test]
    fn ci_builds_the_same_self_contained_seat_recipe_as_the_engine() {
        let ci: serde_yaml::Value =
            serde_yaml::from_str(include_str!("../.github/workflows/ci.yml")).unwrap();
        let script = ci["jobs"]["seat-toolchain"]["steps"][1]["run"]
            .as_str()
            .expect("the image build step");
        let dir = c4_temp();
        let docker = image_stub(&dir, &[]);
        let path = std::env::join_paths(
            std::iter::once(docker.parent().unwrap().to_path_buf()).chain(std::env::split_paths(
                &std::env::var_os("PATH").unwrap_or_default(),
            )),
        )
        .unwrap();
        let output = Command::new("bash")
            .hardened()
            .args(["-euc", script])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .env("PATH", path)
            .env("RUNNER_TEMP", &dir)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let actual = std::fs::read_to_string(dir.join("Dockerfile")).unwrap();
        let instructions = |s: &str| {
            s.lines()
                .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
                .map(str::to_string)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            instructions(&actual),
            instructions(&default_seat_dockerfile())
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The default seat base is the checks floor image; building a seat image ensures it first
    /// (the floor's own build from the embedded Dockerfile), and builds it again when a GC removed
    /// it. There is no registry fallback: a present one is not rebuilt.
    #[test]
    fn the_default_seat_base_is_the_checks_image_ensured_before_the_seat_build() {
        let checks = crate::checks_container::image_tag();
        assert_eq!(default_base(), checks);
        assert_eq!(SeatBase::from_env().image, checks);
        assert!(!SeatBase::from_env().operator);
        let pkg = seat_package("claude").unwrap();
        for present in [false, true] {
            let dir = c4_temp();
            let have: Vec<&str> = if present {
                vec![checks.as_str()]
            } else {
                vec![]
            };
            let rt = Runtime::answering(image_stub(&dir, &have));
            let base = SeatBase {
                image: checks.clone(),
                operator: false,
                recipe: None,
            };
            let tag = build_seat_image_on(&rt, "claude", pkg, &base).unwrap();
            let calls = std::fs::read_to_string(dir.join("calls.log")).unwrap();
            assert!(!calls.contains("--build-arg BASE="), "{calls}");
            let df = std::fs::read_to_string(dir.join("Dockerfile")).unwrap();
            assert_eq!(df, default_seat_dockerfile());
            let seat_build = calls.find(&format!("-t {tag} -")).expect("seat build");
            match calls.find(&format!("-t {checks} -")) {
                Some(base_build) => {
                    assert!(!present, "a present checks image is not rebuilt: {calls}");
                    assert!(base_build < seat_build, "{calls}");
                }
                None => assert!(present, "a missing checks image is rebuilt: {calls}"),
            }
            std::fs::remove_dir_all(dir).unwrap();
        }
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
        // The present base is not rebuilt; the seat image is built on it.
        assert!(!calls.contains("-t seat-base:rust"), "{calls}");
        assert!(calls.contains("build "), "{calls}");
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
            image: default_base(),
            operator: false,
            recipe: Some(PathBuf::from("/nonexistent")),
        };
        let e = build_seat_image_on(&rt, "claude", pkg, &default).unwrap_err();
        assert!(!e.contains(BASE_DOCKERFILE_ENV), "{e}");
        assert!(e.contains("FAKE pull access denied"), "{e}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn status_reads_present_and_missing_images_in_one_listing() {
        let dir = c4_temp();
        let rt = dir.join("docker");
        let claude = seat_image_tag("claude", &default_base()).unwrap();
        let floor = floor_image_tag();
        let id = format!("sha256:{}", "e".repeat(64));
        crate::test_env::install_stub(
            &rt,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{log}'\n\
                 case \"$1\" in\n\
                 images) printf '{claude}\\t{id}\\n{floor}\\t{id}\\n' ;;\n\
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
        assert!(st.floor.present);
        assert!(st.seats[0].present, "{:?}", st.seats);
        assert!(!st.seats[1].present);
        let calls = std::fs::read_to_string(dir.join("calls.log")).unwrap();
        // One listing, no per-image inspect (a missing image costs nothing).
        assert!(!calls.contains("image inspect"), "{calls}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn status_inspects_operator_references_and_reports_listing_failures() {
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
                 images) [ ! -f '{dir}/images-fails' ] || {{ echo 'FAKE daemon busy' >&2; exit 1; }}; printf '{floor}\\t{id}\\n' ;;\n\
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
        std::fs::write(dir.join("images-fails"), "").unwrap();
        let e = image_status_with(&rt, &[], &cfg).unwrap_err();
        assert!(e.contains("FAKE daemon busy"), "{e}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_missing_seat_image_is_built_and_says_so_and_a_present_one_is_not() {
        let dir = c4_temp();
        let rt = Runtime::answering(image_stub(&dir, &[]));
        let base = SeatBase {
            image: default_base(),
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
        let home = tmp("rd-runtimes");
        let bin = home.join(".rd/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let docker = bin.join("docker");
        let nerdctl = bin.join("nerdctl");
        std::fs::write(&docker, "").unwrap();
        std::fs::write(&nerdctl, "").unwrap();
        let orbstack = home.join(".orbstack/bin/docker");
        std::fs::create_dir_all(orbstack.parent().unwrap()).unwrap();
        std::fs::write(&orbstack, "").unwrap();
        let all = runtime_candidates(None, Some(&home), |_| None);
        assert_eq!(&all[..2], &[docker, orbstack]);
        assert_eq!(all.last(), Some(&nerdctl));
        std::fs::remove_dir_all(home).unwrap();
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
            let a = seat_image_tag(cli, &default_base()).unwrap();
            assert_eq!(a, seat_image_tag(cli, &default_base()).unwrap());
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

    /// LIVE (`WICKED_CONTAINER_LIVE=1`; kept by B-6 review: the Kubernetes arm still inspects the
    /// seat image through the image builder): codex ln-r1's hostile image (`/floor` -> a dir it then
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
        let policy = landrun_prefix(&mounts, &[], &[]);
        let v = view.display().to_string();
        assert!(policy.windows(2).any(|p| p == ["--rox", &v]));
        assert!(!policy.windows(2).any(|p| p == ["--rwx", &v]));
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
        assert_eq!(memory_from(Some(" 6g ")), "6g");
        assert_eq!(k8s_memory(memory_from(None)), "3Gi");
        assert_eq!(k8s_memory(memory_from(Some(" 6g "))), "6Gi");
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
    fn runtime_commands_select_kubelet_namespace_only_for_nerdctl() {
        for (cli, global_args) in [
            ("nerdctl", &["--namespace", "k8s.io"][..]),
            ("docker", &[][..]),
            ("podman", &[][..]),
        ] {
            let rt = Runtime {
                cli: Path::new("/some/dir").join(cli),
                answer_bound: None,
            };
            for argv in [
                &[
                    "build",
                    "--label",
                    "wicked.seat=1",
                    "-t",
                    "wicked-floor:x",
                    "-",
                ][..],
                &["image", "inspect", "--format", "{{.Id}}", "wicked-floor:x"][..],
            ] {
                let mut cmd = rt.cmd();
                cmd.args(argv);
                let actual: Vec<_> = cmd.get_args().map(|a| a.to_str().unwrap()).collect();
                let expected: Vec<_> = global_args.iter().chain(argv).copied().collect();
                assert_eq!(actual, expected, "{cli} {argv:?}");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn image_build_passes_kubelet_namespace_to_nerdctl() {
        let dir = c4_temp();
        let cli = dir.join("nerdctl");
        let log = dir.join("argv");
        let recipe = dir.join("Dockerfile");
        crate::test_env::install_stub(
            &cli,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{log}'\ncat > '{recipe}'\n",
                log = log.display(),
                recipe = recipe.display()
            ),
        );
        let df = "FROM busybox:1.37-musl\n";
        assert_eq!(
            build_image(
                &Runtime::answering(cli),
                df,
                "wicked-floor:x",
                &["BASE=busybox:1.37-musl".to_string()]
            )
            .unwrap(),
            "wicked-floor:x"
        );
        let argv = std::fs::read_to_string(log).unwrap();
        assert_eq!(
            argv.lines().collect::<Vec<_>>(),
            vec![
                "--namespace",
                "k8s.io",
                "build",
                "--label",
                "wicked.seat=1",
                "--build-arg",
                "BASE=busybox:1.37-musl",
                "-t",
                "wicked-floor:x",
                "-"
            ]
        );
        assert_eq!(std::fs::read_to_string(recipe).unwrap(), df);
        std::fs::remove_dir_all(dir).unwrap();
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

    /// The seat's floor plan for a Rust-capable seat (claude here; every CLI gets the same cache
    /// table) under a worker home at `worker`, with `tree` its writable tree. Returns the plan
    /// and its resolved mounts. The caller holds the environment lock.
    fn cargo_seat(
        worker: &Path,
        tree: &Path,
        read_roots: &[PathBuf],
    ) -> (crate::worker_sandbox::SeatFloorPlan, Vec<Mount>) {
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
        (plan, mounts)
    }

    /// A Rust-capable seat's cargo home and target dir are per-run state in the replica, never the
    /// 512 MB tmpfs HOME and never the host's seeded cache: the plan points `CARGO_HOME` and
    /// `CARGO_TARGET_DIR` into `<worker home>/seat-build/<cli>`, which is neither a seed root (under
    /// `<worker home>/cache`) nor a write-back root, and the mount plan and Landlock policy admit it.
    #[test]
    fn a_rust_capable_seats_cargo_state_is_outside_the_seed_and_the_write_back() {
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let base = tmp("cargo-cache");
        let tree = base.join("wt");
        let worker = base.join("worker");
        std::fs::create_dir_all(&tree).unwrap();
        // A retained replica's (or an old host's) cargo home under the seed path, with a config
        // that would redirect every build: it is never read, `CARGO_HOME` is elsewhere.
        let legacy = worker.join("cache").join("claude").join("cargo");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(
            legacy.join("config.toml"),
            "[build]\nrustc = \"/sentinel\"\n",
        )
        .unwrap();
        let (plan, mounts) = cargo_seat(&worker, &tree, &[]);
        let cache = worker.join("cache").join("claude");
        let build = worker.join("seat-build").join("claude");
        let get = |k: &str| {
            let all: Vec<_> = plan.env.iter().filter(|(n, _)| *n == k).collect();
            assert_eq!(all.len(), 1, "{k} is set once: {:?}", plan.env);
            all[0].1.clone()
        };
        assert_eq!(get("CARGO_HOME"), build.join("cargo-home"));
        assert_ne!(get("CARGO_HOME"), legacy);
        assert!(!get("CARGO_HOME").join("config.toml").exists());
        assert_eq!(get("CARGO_TARGET_DIR"), build.join("target"));
        // Neither is a seed root, a write root or in the tree.
        for p in [get("CARGO_HOME"), get("CARGO_TARGET_DIR")] {
            let seed = worker.join("cache");
            assert!(!p.starts_with(&seed), "{p:?} is a seed root");
            assert!(!p.starts_with(&tree), "{p:?} is in the tree");
            assert!(!plan.inputs.write_roots.iter().any(|w| p.starts_with(w)));
        }
        assert!(build.is_dir(), "the dir exists before the bind");
        assert!(cache.is_dir());
        assert!(
            mounts.contains(&Mount::Bind {
                path: build.clone(),
                rw: true,
            }),
            "{mounts:?}"
        );
        let policy = landrun_prefix(&mounts, &[], &[]);
        assert!(policy
            .windows(2)
            .any(|p| p == ["--rwx", &build.to_string_lossy()]));
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
    /// profiles, no incremental state. The plan carries the settings. An operator's explicit
    /// value wins; a blank value is not a choice.
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
        let (plan, _) = cargo_seat(&worker, &tree, &[]);
        let jobs = seat_build_jobs(seat_cpus()).to_string();
        assert_eq!(
            get(&plan.build_env, "CARGO_PROFILE_DEV_DEBUG").as_deref(),
            Some("line-tables-only"),
            "the operator's value wins: {:?}",
            plan.build_env
        );
        assert_eq!(get(&plan.build_env, "CARGO_BUILD_JOBS"), Some(jobs.clone()));
        assert_eq!(
            get(&plan.build_env, "CARGO_INCREMENTAL").as_deref(),
            Some("0")
        );
        assert_eq!(plan.build_env.len(), SEAT_TOOLCHAIN_BUILD_ENV.len());
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
    /// and a read root or argv path inside one is never bound (#1042).
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
        assert!(mounts.contains(&Mount::Tmpfs { path: ssh.clone() }));
        assert!(!mounts
            .iter()
            .any(|m| matches!(m, Mount::Bind { path, .. } if path == &ssh)));
        assert!(mounts.contains(&Mount::Bind {
            path: home.clone(),
            rw: false
        }));
        let policy = landrun_prefix(&mounts, &[], &[]);
        assert!(policy
            .windows(2)
            .any(|p| p == ["--ro", &ssh.to_string_lossy()]));
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
    fn pi_with_a_decorated_template_gets_its_at_path_as_an_argument_of_its_own() {
        let root = base();
        let prompt = big_prompt();
        let mut argv =
            crate::execute_wrapped::build_argv("pi -p \"{PROMPT} Be concise.\"", &prompt, &[]);
        let mut mounts = Vec::new();
        let _c = carry_prompt_in_file_at(&root, "pi", &prompt, &mut argv, &mut mounts)
            .unwrap()
            .expect("carried");
        // The file rides pi's own carrier as an argument of its own; the decoration stays.
        let n = argv.len();
        assert_eq!(argv[n - 2], format!("@{}", _c.file.display()), "{argv:?}");
        assert_eq!(argv[n - 1], "the task in the attached file Be concise.");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn pi_takes_a_prompt_given_as_an_option_value_as_that_option_expects() {
        let root = base();
        let prompt = big_prompt();
        for (template, want_path) in [
            ("pi -p --system-prompt \"{PROMPT}\" Begin.", true),
            ("pi -p --append-system-prompt \"{PROMPT}\" Begin.", true),
            ("pi -p --model \"{PROMPT}\" Begin.", false),
        ] {
            let mut argv = crate::execute_wrapped::build_argv(template, &prompt, &[]);
            let mut mounts = Vec::new();
            let c = carry_prompt_in_file_at(&root, "pi", &prompt, &mut argv, &mut mounts)
                .unwrap()
                .expect("carried");
            let value = &argv[3];
            assert!(!value.starts_with('@'), "{template}: {value}");
            if want_path {
                assert_eq!(value, &c.file.display().to_string(), "{template}");
            } else {
                assert!(
                    value.contains("Read that whole file"),
                    "{template}: {value}"
                );
            }
            assert_eq!(argv.last().unwrap(), "Begin.", "{template}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn pi_after_a_boolean_flag_still_gets_its_file() {
        let root = base();
        let prompt = big_prompt();
        let mut argv =
            crate::execute_wrapped::build_argv("pi -p --no-tools \"{PROMPT}\"", &prompt, &[]);
        let mut mounts = Vec::new();
        let c = carry_prompt_in_file_at(&root, "pi", &prompt, &mut argv, &mut mounts)
            .unwrap()
            .expect("carried");
        assert!(argv.contains(&format!("@{}", c.file.display())), "{argv:?}");
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
        let many = vec!["x".repeat(100 * 1024); 6];
        assert!(check_exec_args(&many).unwrap_err().contains("at most"));
        // Many tiny ones too: each also costs its pointer in the exec's argv table.
        let tiny = vec!["x".to_string(); 60_000];
        assert!(check_exec_args(&tiny).unwrap_err().contains("at most"));
        assert_eq!(
            crate::worker_sandbox::FloorUnarmed::ArgvTooLong.as_str(),
            "argv_too_long"
        );
    }
}
