//! The CONTAINER checks floor (DES-FLOOR-CONTAINER): the repository's own checks
//! ([`crate::repo_checks`]) run in a fresh Linux container per floor side instead of inside the
//! host's OS sandbox (`sandbox-exec` on macOS).
//!
//! ## Why
//!
//! On a macOS host every process a check spawns pays for endpoint-security scanning and the
//! sandbox profile. The same crew test file ran in 17 s in a Linux container and 260 s on the macOS
//! floor; crew's whole suite took 2.5 min in a container against 8.6-18 min on the macOS floor, and
//! a power run's three full-suite floors took 79-123 min each under host load (re-time 3).
//!
//! ## What a container floor is
//!
//! * **One fresh container per floor side.** The head's checks run in one container, started at
//!   the floor's first head check. The base comparison runs in a container of its own, made fresh
//!   for every base export the floor takes, so nothing the head ran can reach the base's tree,
//!   dependencies or build output. Every check is a `docker exec` into its side's container
//!   ([`ChecksFloor::command`]); the floor's decision logic (base comparison, retries, exclusions)
//!   is unchanged: only the spawn differs.
//! * **The tree is a copy, streamed in.** The engine writes a tar of the floor's tree itself, from
//!   `git ls-tree` and `git cat-file --batch` over the pinned git dir ([`write_tree_tar`]): no bind
//!   mount of the worktree (a mount would be a write path back to the host), and not `git archive`,
//!   which runs the smudge filters a repository's configuration names. The head is the tree the
//!   floor judges (a guarded checkout's tree, else the worktree's content snapshot); the base is
//!   the run base commit. A check's writes land in the container and go with it.
//! * **The copy is verified after every check** ([`ChecksFloor::after`]): every tracked file must
//!   still hold the bytes (or link target) that were copied in. A check that rewrote one has run
//!   the later checks on another tree, so its result is no evidence for this one (the host floor's
//!   worktree guard makes the same comparison).
//! * **No network while checks run** (`--network none`). Dependencies come from cache volumes that
//!   an install container fills: it alone has the network, and it runs a fixed
//!   `npm ci --ignore-scripts` or a `cargo fetch --locked` with the repository's `.cargo`
//!   configuration left out of its copy and every compiler wrapper overridden, so it executes no
//!   repository code.
//! * **Hardened**: a read-only root filesystem, every capability dropped, `no-new-privileges`, an
//!   init, a pids limit and a memory cap (`--memory` = `--memory-swap`, [`MEMORY_ENV`]). The checks
//!   run as uid 1000; the container's own idle process runs as another uid, so the engine can end
//!   everything a check left running (`kill -9 -1` as the checks' user) without stopping the
//!   container.
//! * **An OOM kill is floor infrastructure.** The container's cgroup `memory.events` `oom_kill`
//!   count is read after every check; a check during which it rose is `could_not_run` with the
//!   reason, whatever its exit code, never a failure of the change (so it is never retried as a
//!   failing set or compared with the base). A check can only turn its own result into that
//!   denial; it can never turn it into a pass.
//! * **Never a seat container.** Its own image ([`DOCKERFILE`], `wicked-checks-floor:<hash>`), its
//!   own label ([`LABEL`]), no seat home, cache, network or gateway.
//!
//! ## Caches
//!
//! | Cache | Volume | Filled by | Mounted in the checks |
//! |---|---|---|---|
//! | `node_modules` | `wfloor-nm-<key>`, key = sha256 of the image, the package dir, the lockfile, every `package.json` / `.npmrc` / `npm-shrinkwrap.json` of the tree (and the whole tree when the lockfile installs a local `file:` package) | an install container, `npm ci --ignore-scripts` | read-only, in place at `<side>/<dir>/node_modules`; nested workspace `node_modules` unpacked from a tar in the volume |
//! | cargo registry | `wfloor-cargo-home` (shared) | `cargo fetch --locked`, once per workspace lockfile | read-only, `CARGO_NET_OFFLINE=true` |
//! | cargo target | `wfloor-target-<run leaf>-<side>` | the floor itself | read-write, removed with the run's check-scratch leaf |
//!
//! A target volume is per check-scratch leaf and side (core#480's reasons: cargo decides
//! freshness by mtime): warm across the floors of one leaf (a live worktree), cold for each fresh
//! guarded checkout, exactly as the host floor's leaf. The copy-in keeps each file's mtime from the
//! leaf's previous copy when its blob is unchanged and stamps a changed one later than any earlier
//! build could have run ([`Mtimes`]), so cargo rebuilds what changed and nothing reads as fresh
//! that is not. `node_modules` and target volumes are held to the check cache's caps against the
//! runtime VM's disk ([`evict`]); a volume a floor holds is never evicted (a lease, taken under
//! the same lock the eviction takes). The shared cargo registry is outside those caps.
//!
//! ## Which floor runs ([`RUNNER_ENV`])
//!
//! `WICKED_FLOOR_RUNNER=auto|container|sandbox`, default `auto`. `auto` runs the container floor
//! on a macOS host whose container runtime answers and whose floor can be prepared, and otherwise
//! the host sandbox floor, with the reason on the report (`floorRunner.fallbackReason`). It falls
//! back only for what the container cannot do (no runtime, no image, an install it cannot fill
//! offline, a tool the image lacks, a check that needs `.git` or writes a results file); a
//! dependency install that FAILS on the repository's own inputs is not a reason to pick another
//! floor, and stays the floor's evidence. On a Linux host `auto` keeps the native `bwrap` floor:
//! the macOS costs this removes do not exist there. `container` refuses the floor (fail-closed)
//! instead of falling back; `sandbox` never tries the container. A test harness (this crate's
//! unit tests, and any test binary that armed the hermetic emit spool) runs the host floor unless
//! the switch is set. Which one ran is on every report and `repoChecksEvaluated`
//! (`floorRunner.runner`: `container` | `macos-sandbox` | `linux-bwrap` | `none`).
//!
//! ## What the container floor does not run
//!
//! Inside the container the checks see [`MARKER_ENV`]`=container`. A test that needs an OS write
//! boundary armed INSIDE the floor (`bwrap` cannot arm under the runtime's default seccomp, and the
//! floor does not loosen it) or the network leaves itself out under it with a disclosed line; the
//! report says so ([`PLATFORM_SCOPE`]). The repository's CI covers them.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use wicked_apps_core::HardenedCommand;

use crate::container_floor::{output_within, Runtime};
use crate::repo_checks::{RepoCheck, Tree};

/// Which floor runs the checks: `auto` (default), `container` or `sandbox`.
pub(crate) const RUNNER_ENV: &str = "WICKED_FLOOR_RUNNER";
/// The floor container's memory cap (a runtime size: `6g`, `8192m`); default [`DEFAULT_MEMORY`].
pub(crate) const MEMORY_ENV: &str = "WICKED_FLOOR_MEMORY";
/// Set to `container` inside the floor container, so a check can tell where it runs.
pub(crate) const MARKER_ENV: &str = "WICKED_CHECKS_FLOOR";
/// The label on every floor container and volume.
pub(crate) const LABEL: &str = "wicked.checks-floor";
/// The checks floor image, built by the engine from this file alone.
pub(crate) const DOCKERFILE: &str = include_str!("../containers/checks/node22-rust.Dockerfile");
/// The default memory cap.
pub(crate) const DEFAULT_MEMORY: &str = "6g";
/// The disclosure every container floor report carries.
pub(crate) const PLATFORM_SCOPE: &str = "the checks ran in a Linux container with no network: \
     macOS-only code paths did not run here, and tests that need an OS write boundary or the \
     network inside the floor leave themselves out (WICKED_CHECKS_FLOOR=container); the \
     repository's CI covers them on its own platforms";

/// The uid:gid the checks run as (the image's `node` user).
const CHECKS_USER: &str = "1000:1000";
/// The uid:gid of the container's own idle process: not the checks' user, so the checks' user's
/// `kill -9 -1` never reaches it.
const IDLE_USER: &str = "65534:65534";
const HOME: &str = "/home/floor";
const TARGET_DIR: &str = "/wicked/target";
const CARGO_HOME_DIR: &str = "/wicked/cargo-home";
const FILL_DIR: &str = "/wicked/vol";
const CARGO_HOME_VOLUME: &str = "wfloor-cargo-home";
const COMPLETE: &str = ".wicked-complete";
const NESTED_TAR: &str = ".wicked-nested.tar";
const INSTALL_LOG: &str = ".wicked-install.log";
/// The install every `node_modules` volume is filled with, whatever argv the repository detected:
/// a fixed, trusted command (a repository argv like `--ignore-scripts=false` never reaches it).
const NPM_FILL: &[&str] = &["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund"];
pub(crate) const PATH_IN: &str =
    "/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const PROBE: Duration = Duration::from_secs(15);
const START: Duration = Duration::from_secs(120);
const COPY: Duration = Duration::from_secs(300);
const VERIFY: Duration = Duration::from_secs(120);
const FILL: Duration = crate::repo_checks::INSTALL_TIMEOUT;
const REAP: Duration = Duration::from_secs(20);
/// How often the eviction and sweep pass runs at most (per process).
const MAINTAIN_EVERY: Duration = Duration::from_secs(10 * 60);
/// node_modules volumes kept per repository, newest first, whatever the caps.
const NM_KEEP: usize = 3;
/// A changed file's mtime is stamped this far past the copy, so it is later than any build an
/// earlier copy of the leaf could have run (the VM's clock may trail the host's a little).
const MTIME_MARGIN_S: u64 = 2;

/// The operator's choice of floor ([`RUNNER_ENV`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Auto,
    Container,
    Sandbox,
}

/// Parse [`RUNNER_ENV`]. Unset or empty is `auto`; an unknown value is an error (a typo must not
/// silently pick a floor).
pub(crate) fn mode_from(v: Option<&str>) -> Result<Mode, String> {
    match v.map(str::trim).unwrap_or("") {
        "" | "auto" => Ok(Mode::Auto),
        "container" => Ok(Mode::Container),
        "sandbox" | "host" => Ok(Mode::Sandbox),
        other => Err(format!(
            "{RUNNER_ENV}={other} is not one of auto, container, sandbox"
        )),
    }
}

/// The floor mode for this process. This crate's own unit tests run the host floor unless a test
/// asks for the container (`test_seam`), and so does any test binary that armed the hermetic emit
/// spool (every integration test of this crate does) unless [`RUNNER_ENV`] is set: the floor
/// tests are about the floor's decisions, not the runtime, and a developer host or CI runner with
/// a container runtime must not change what they exercise.
pub(crate) fn mode() -> Result<Mode, String> {
    #[cfg(test)]
    {
        return Ok(test_seam::mode().unwrap_or(Mode::Sandbox));
    }
    #[allow(unreachable_code)]
    let set = std::env::var(RUNNER_ENV).ok();
    if set.is_none() && test_harness() {
        return Ok(Mode::Sandbox);
    }
    mode_from(set.as_deref())
}

/// Whether this process is a test harness: the hermetic emit spool is armed (its outbox path).
fn test_harness() -> bool {
    std::env::var(wicked_apps_core::emit::DEADLETTER_ENV)
        .is_ok_and(|p| p.contains("wicked-apps-test-outbox-"))
}

/// Whether `auto` tries the container on this host: macOS only (see the module doc).
pub(crate) fn auto_tries_container() -> bool {
    cfg!(target_os = "macos")
}

/// The wire record of which floor ran ([`crate::repo_checks::RepoChecksReport::runner`], the
/// `floorRunner` of `repoChecksEvaluated`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FloorRunner {
    /// `container` | `macos-sandbox` | `linux-bwrap` | `none` (no boundary: the operator's opt-in,
    /// none could be armed, or the runner was refused and nothing ran).
    pub runner: String,
    /// Why the container floor did not run, when it was wanted: `auto` fell back to the host floor,
    /// or (`runner: none`) `container` was asked for and refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<String>,
    /// The image the checks ran in (`tag@id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// The runtime CLI and its server version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    /// `os/arch` the checks ran on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// The container's memory cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_cap: Option<String>,
    /// `CARGO_BUILD_JOBS` inside, sized to the memory cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_jobs: Option<u32>,
    /// The checks' network: `none` in the container, `open` on the host floors.
    pub network: String,
    /// The tree copied in for the head (a git tree id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree: Option<String>,
    /// Each cache the floor used and whether it was already filled.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub caches: Vec<CacheUse>,
    /// The containers this floor started (head, then one per base export).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub containers: Vec<String>,
    /// Time spent preparing (image, volumes, fills) before the first check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prepare_ms: Option<u64>,
    /// Time spent starting containers and copying trees in, all sides together.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy_in_ms: Option<u64>,
    /// What the container floor leaves to CI ([`PLATFORM_SCOPE`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform_scope: Option<String>,
}

/// One cache volume a container floor used.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheUse {
    /// `node_modules` | `cargo-registry` | `cargo-target`.
    pub kind: String,
    pub volume: String,
    /// `hit` | `filled` (an install container filled it for this floor) | `failed` (the install
    /// failed on the repository's own inputs; its log is the install check's output).
    pub state: String,
    /// The fill's duration, when this floor filled it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill_ms: Option<u64>,
}

impl FloorRunner {
    /// The record of a host floor: `level` is the report's `sandbox_level`, `wrapper` the launcher.
    pub(crate) fn host(level: &str, wrapper: &[String], fallback: Option<String>) -> Self {
        let runner = if level == "sandboxed" {
            match wrapper.first().map(|w| {
                Path::new(w)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            }) {
                Some(n) if n == "sandbox-exec" => "macos-sandbox",
                Some(n) if n == "bwrap" => "linux-bwrap",
                _ => "host-sandbox",
            }
        } else {
            "none"
        };
        FloorRunner {
            runner: runner.to_string(),
            fallback_reason: fallback,
            network: "open".to_string(),
            ..Default::default()
        }
    }

    /// The record of a floor whose runner was refused: nothing ran.
    pub(crate) fn refused(why: String) -> Self {
        FloorRunner {
            runner: "none".to_string(),
            fallback_reason: Some(why),
            network: "none".to_string(),
            ..Default::default()
        }
    }
}

/// The memory cap and the cargo jobs that fit it: one job per 2 GB, at least one.
pub(crate) fn memory_and_jobs(v: Option<&str>, cpus: usize) -> Result<(String, u64, u32), String> {
    let spelled = v
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_MEMORY)
        .to_ascii_lowercase();
    let (num, unit) = spelled.split_at(
        spelled
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(spelled.len()),
    );
    let n: u64 = num
        .parse()
        .map_err(|_| format!("{MEMORY_ENV}={spelled} is not a size like 6g"))?;
    let bytes = match unit {
        "g" | "gb" => n << 30,
        "m" | "mb" => n << 20,
        _ => return Err(format!("{MEMORY_ENV}={spelled} is not a size like 6g")),
    };
    if bytes < (1 << 30) {
        return Err(format!("{MEMORY_ENV}={spelled} is under 1g"));
    }
    let jobs = ((bytes >> 30) / 2).clamp(1, cpus.max(1) as u64) as u32;
    Ok((spelled, bytes, jobs))
}

/// 16 hex of sha256 over `parts`, NUL-separated.
pub(crate) fn key16(parts: &[&[u8]]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
        h.update([0u8]);
    }
    h.finalize()[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The image's tag: `wicked-checks-floor:<12 hex of sha256(Dockerfile)>`.
pub(crate) fn image_tag() -> String {
    format!(
        "wicked-checks-floor:{}",
        &key16(&[DOCKERFILE.as_bytes()])[..12]
    )
}

/// What an `npm ci` in a package dir of a tree installs from, beyond its lockfile: every manifest
/// and npm configuration file of the tree (a workspace member's `package.json` decides its links,
/// an `.npmrc` the registry and install mode), and — when the lockfile installs a local `file:`
/// package, whose bytes are the tree's — the tree itself.
pub(crate) fn npm_inputs(entries: &[Entry], lock: &[u8], tree: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut linked = false;
    for e in entries {
        let base = e.path.rsplit('/').next().unwrap_or("");
        if matches!(
            base,
            "package.json" | ".npmrc" | "npm-shrinkwrap.json" | "package-lock.json"
        ) {
            out.extend_from_slice(e.path.as_bytes());
            out.push(0);
            out.extend_from_slice(e.oid.as_bytes());
            out.push(0);
            // A linked input's bytes are its referent's, which npm follows: the tree decides.
            linked |= e.is_link();
        }
    }
    if linked || installs_local_bytes(lock) {
        out.extend_from_slice(b"tree\0");
        out.extend_from_slice(tree.as_bytes());
    }
    out
}

/// Whether a lockfile installs bytes from the tree itself: any package whose `resolved` or (v1)
/// `version` is a `file:` spec, read structurally (any JSON spelling), or a lockfile that does not
/// parse (unknown: assume it does).
pub(crate) fn installs_local_bytes(lock: &[u8]) -> bool {
    let lock = lock.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(lock);
    fn walk(v: &serde_json::Value) -> bool {
        match v {
            serde_json::Value::Object(m) => m.iter().any(|(k, v)| {
                (matches!(k.as_str(), "resolved" | "version")
                    && v.as_str()
                        .is_some_and(|s| s.trim_start().starts_with("file:")))
                    || walk(v)
            }),
            serde_json::Value::Array(a) => a.iter().any(walk),
            _ => false,
        }
    }
    serde_json::from_slice::<serde_json::Value>(lock).map_or(true, |v| walk(&v))
}

/// The first git dependency a lockfile installs (`resolved` / v1 `version` naming a git source),
/// read structurally; a lockfile that does not parse names none here (npm refuses it anyway).
pub(crate) fn git_dependency(lock: &[u8]) -> Option<String> {
    fn walk(v: &serde_json::Value) -> Option<String> {
        match v {
            serde_json::Value::Object(m) => m.iter().find_map(|(k, v)| {
                v.as_str()
                    .filter(|s| {
                        let s = s.trim_start().to_ascii_lowercase();
                        matches!(k.as_str(), "resolved" | "version")
                            && ["git+", "git:", "github:", "gitlab:", "bitbucket:", "gist:"]
                                .iter()
                                .any(|p| s.starts_with(p))
                    })
                    .map(str::to_string)
                    .or_else(|| walk(v))
            }),
            serde_json::Value::Array(a) => a.iter().find_map(walk),
            _ => None,
        }
    }
    // npm strips a byte-order mark before parsing; a lockfile that still does not parse is not
    // read as "no git dependency" (it fails closed).
    let lock = lock.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(lock);
    match serde_json::from_slice::<serde_json::Value>(lock) {
        Ok(v) => walk(&v),
        Err(_) => Some("a lockfile that does not parse".to_string()),
    }
}

/// The cargo inputs a fetch resolves from beyond its lockfile: every `Cargo.toml` of the tree (a
/// manifest decides which locked packages are reachable, and a `[patch]` adds sources).
pub(crate) fn cargo_inputs(entries: &[Entry], tree: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut linked = false;
    for e in entries.iter().filter(|e| {
        matches!(
            e.path.rsplit('/').next(),
            Some("Cargo.toml") | Some("Cargo.lock")
        )
    }) {
        out.extend_from_slice(e.path.as_bytes());
        out.push(0);
        out.extend_from_slice(e.oid.as_bytes());
        out.push(0);
        // A linked manifest or lockfile reads as its referent: the tree decides.
        linked |= e.is_link();
    }
    if linked {
        out.extend_from_slice(b"tree\0");
        out.extend_from_slice(tree.as_bytes());
    }
    out
}

/// The `node_modules` volume for an `npm ci` in `dir` over `lock` and the tree's `inputs`
/// ([`npm_inputs`]).
pub(crate) fn nm_volume(image: &str, dir: &str, lock: &[u8], inputs: &[u8]) -> String {
    let argv = NPM_FILL.join("\u{1f}");
    format!(
        "wfloor-nm-{}",
        key16(&[
            image.as_bytes(),
            dir.as_bytes(),
            argv.as_bytes(),
            lock,
            inputs
        ])
    )
}

/// The cargo target volume of one run leaf and side.
pub(crate) fn target_volume(leaf: &Path, tree: Tree) -> String {
    format!("wfloor-target-{}-{}", leaf_key(leaf), side(tree))
}

fn leaf_key(leaf: &Path) -> String {
    key16(&[leaf.to_string_lossy().as_bytes()])
}

fn side(tree: Tree) -> &'static str {
    match tree {
        Tree::Head => "head",
        Tree::Base => "base",
    }
}

/// The argv of an `npm ci` install the container floor can fill from a lockfile, or `None` for an
/// install it cannot (pnpm, yarn, `npm install` with no lockfile): those need the network while
/// they run, which the floor does not have.
pub(crate) fn npm_ci(check: &RepoCheck) -> Option<&[String]> {
    let a = &check.argv;
    (a.len() >= 2 && a[0] == "npm" && a[1] == "ci").then_some(a.as_slice())
}

// ---------------------------------------------------------------------------------------------
// The tree copy: a tar the engine writes from git's object store.
// ---------------------------------------------------------------------------------------------

/// One entry of a tree to copy in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) mode: u32,
    pub(crate) oid: String,
    pub(crate) path: String,
}

impl Entry {
    fn is_link(&self) -> bool {
        self.mode & 0o170000 == 0o120000
    }
}

/// Parse `git ls-tree -r -z --full-tree` output: blobs and links (submodules are skipped: the
/// floor never fetches them, as the macOS floor's worktree has none checked out by the engine). A
/// path that could leave the tree (absolute, `..`) or that holds a control character (a newline
/// would forge a PAX record) refuses the copy.
pub(crate) fn parse_ls_tree(raw: &[u8]) -> Result<Vec<Entry>, String> {
    let mut out = Vec::new();
    for rec in raw.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let tab = rec
            .iter()
            .position(|b| *b == b'\t')
            .ok_or("a `git ls-tree` record has no path")?;
        let head = std::str::from_utf8(&rec[..tab]).map_err(|e| e.to_string())?;
        let path = String::from_utf8(rec[tab + 1..].to_vec())
            .map_err(|_| "a path in the tree is not UTF-8".to_string())?;
        let mut f = head.split(' ');
        let (Some(mode), Some(kind), Some(oid)) = (f.next(), f.next(), f.next()) else {
            return Err(format!("a `git ls-tree` record is malformed: {head}"));
        };
        if kind != "blob" {
            continue;
        }
        let mode = u32::from_str_radix(mode, 8).map_err(|e| e.to_string())?;
        if path.starts_with('/')
            || path.split('/').any(|c| c == ".." || c.is_empty())
            || path.chars().any(char::is_control)
        {
            return Err(format!("the tree holds an unsafe path {path:?}"));
        }
        out.push(Entry {
            mode,
            oid: oid.to_string(),
            path,
        });
    }
    Ok(out)
}

/// The mtimes a side's copies used, per path: `(blob id, mtime)`. A blob unchanged since the
/// leaf's last copy keeps its mtime; anything else is stamped [`MTIME_MARGIN_S`] past now, later
/// than any build an earlier copy could have run. Kept in the run's check-scratch leaf.
#[derive(Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Mtimes(pub(crate) std::collections::BTreeMap<String, (String, u64)>);

impl Mtimes {
    pub(crate) fn load(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// The mtimes for `entries` copied at `now`, and the map to keep for the next copy.
    pub(crate) fn assign(&self, entries: &[Entry], now: u64) -> (Vec<u64>, Mtimes) {
        let stamp = now + MTIME_MARGIN_S;
        let mut next = std::collections::BTreeMap::new();
        let times = entries
            .iter()
            .map(|e| {
                let t = match self.0.get(&e.path) {
                    Some((oid, t)) if *oid == e.oid && *t <= stamp => *t,
                    // Unchanged bytes with a recorded time ahead of now (the clock stepped back).
                    Some((oid, _)) if *oid == e.oid => stamp,
                    // Changed: strictly later than the mtime cargo recorded for the old bytes.
                    Some((_, t)) => stamp.max(t.saturating_add(1)),
                    None => stamp,
                };
                next.insert(e.path.clone(), (e.oid.clone(), t));
                t
            })
            .collect();
        (times, Mtimes(next))
    }

    pub(crate) fn save(&self, path: &Path) {
        if let Ok(b) = serde_json::to_vec(self) {
            let tmp = path.with_extension("tmp");
            if std::fs::write(&tmp, b).is_ok() {
                let _ = std::fs::rename(&tmp, path);
            }
        }
    }
}

/// A PAX record `"<len> <key>=<value>\n"`, `len` counting itself.
fn pax_record(key: &str, value: &str) -> Vec<u8> {
    let body = format!(" {key}={value}\n");
    let mut len = body.len() + 1;
    loop {
        let candidate = format!("{len}{body}");
        if candidate.len() == len {
            return candidate.into_bytes();
        }
        len = candidate.len();
    }
}

fn octal(field: &mut [u8], v: u64) {
    let s = format!("{:0width$o}", v, width = field.len() - 1);
    let b = s.as_bytes();
    let b = &b[b.len().saturating_sub(field.len() - 1)..];
    field[..b.len()].copy_from_slice(b);
    field[field.len() - 1] = 0;
}

/// One ustar header block.
fn ustar(name: &str, kind: u8, mode: u32, size: u64, mtime: u64, link: &str) -> [u8; 512] {
    let mut h = [0u8; 512];
    let n = name.as_bytes();
    h[..n.len().min(100)].copy_from_slice(&n[..n.len().min(100)]);
    octal(&mut h[100..108], u64::from(mode & 0o7777));
    octal(&mut h[108..116], 1000);
    octal(&mut h[116..124], 1000);
    octal(&mut h[124..136], size);
    octal(&mut h[136..148], mtime);
    h[156] = kind;
    let l = link.as_bytes();
    h[157..157 + l.len().min(100)].copy_from_slice(&l[..l.len().min(100)]);
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    h[148..156].copy_from_slice(b"        ");
    let sum: u64 = h.iter().map(|b| u64::from(*b)).sum();
    let s = format!("{sum:06o}\0 ");
    h[148..156].copy_from_slice(s.as_bytes());
    h
}

/// Write one tar member (a PAX header first when a name or link does not fit ustar). A link
/// target with a control character is refused (it would forge a PAX record).
pub(crate) fn tar_member(
    out: &mut impl Write,
    path: &str,
    mode: u32,
    mtime: u64,
    body: &[u8],
) -> std::io::Result<()> {
    let link = mode & 0o170000 == 0o120000;
    let link_target = if link {
        String::from_utf8(body.to_vec()).map_err(|_| {
            std::io::Error::other(format!(
                "the link `{path}` names a target that is not UTF-8"
            ))
        })?
    } else {
        String::new()
    };
    if link_target.chars().any(char::is_control) {
        return Err(std::io::Error::other(format!(
            "the link `{path}` names a target with a control character"
        )));
    }
    let needs_pax = path.len() > 100 || link_target.len() > 100;
    if needs_pax {
        let mut rec = pax_record("path", path);
        if link_target.len() > 100 {
            rec.extend(pax_record("linkpath", &link_target));
        }
        out.write_all(&ustar(
            "././@PaxHeader",
            b'x',
            0o644,
            rec.len() as u64,
            mtime,
            "",
        ))?;
        out.write_all(&rec)?;
        pad(out, rec.len())?;
    }
    let short: String = path.chars().take(100).collect();
    if link {
        out.write_all(&ustar(&short, b'2', 0o777, 0, mtime, &link_target))
    } else {
        let m = if mode & 0o111 != 0 { 0o755 } else { 0o644 };
        out.write_all(&ustar(&short, b'0', m, body.len() as u64, mtime, ""))?;
        out.write_all(body)?;
        pad(out, body.len())
    }
}

fn pad(out: &mut impl Write, len: usize) -> std::io::Result<()> {
    let rem = len % 512;
    if rem != 0 {
        out.write_all(&[0u8; 512][..512 - rem])?;
    }
    Ok(())
}

/// A git command over the pinned git dir, with no system or global configuration.
fn git_cmd(git_dir: &Path, args: &[&str]) -> Command {
    // spawn-audit: hardened — git plumbing over the run's pinned git dir (object reads only).
    let mut c = Command::new("git");
    c.hardened()
        .args(args)
        .env("GIT_DIR", git_dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .current_dir(std::env::temp_dir());
    c
}

/// The entries of `tree` (a tree or commit id) in `git_dir`.
pub(crate) fn ls_tree(git_dir: &Path, tree: &str) -> Result<Vec<Entry>, String> {
    let mut c = git_cmd(git_dir, &["ls-tree", "-r", "-z", "--full-tree", tree]);
    c.stdin(Stdio::null());
    let out = output_within(c, COPY).ok_or("`git ls-tree` did not finish")?;
    if !out.status.success() {
        return Err(format!(
            "`git ls-tree {tree}` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    parse_ls_tree(&out.stdout)
}

/// One blob of `id` at `rel`, or `None` when it is not there.
fn read_blob(git_dir: &Path, id: &str, rel: &str) -> Option<Vec<u8>> {
    let spec = format!("{id}:{rel}");
    let mut c = git_cmd(git_dir, &["cat-file", "blob", &spec]);
    c.stdin(Stdio::null());
    output_within(c, PROBE)
        .filter(|o| o.status.success())
        .map(|o| o.stdout)
}

/// What a copied file must still hold after a check: its bytes' sha256, or its link target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) enum Expect {
    /// The bytes' sha256 and whether the file is executable.
    #[serde(rename = "f")]
    File(String, bool),
    #[serde(rename = "l")]
    Link(String),
}

/// Write `entries` (with `mtimes`) as a tar to `out`, reading each blob through one
/// `git cat-file --batch` (raw object bytes: no filter, no eol conversion, no hook). Returns what
/// each copied path holds, for the after-check comparison.
pub(crate) fn write_tree_tar(
    git_dir: &Path,
    entries: &[Entry],
    mtimes: &[u64],
    out: &mut dyn Write,
) -> Result<Vec<(String, Expect)>, String> {
    use sha2::{Digest, Sha256};
    let mut c = git_cmd(git_dir, &["cat-file", "--batch"]);
    c.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = c
        .spawn()
        .map_err(|e| format!("`git cat-file --batch`: {e}"))?;
    let mut tx = child.stdin.take().ok_or("no stdin")?;
    let mut rx = BufReader::new(child.stdout.take().ok_or("no stdout")?);
    let mut held = Vec::with_capacity(entries.len());
    let result = (|| -> Result<(), String> {
        let mut line = String::new();
        let mut out = out;
        for (e, t) in entries.iter().zip(mtimes) {
            writeln!(tx, "{}", e.oid).map_err(|e| e.to_string())?;
            tx.flush().map_err(|e| e.to_string())?;
            line.clear();
            rx.read_line(&mut line).map_err(|e| e.to_string())?;
            let mut f = line.trim_end().split(' ');
            let (Some(_), Some(kind), Some(size)) = (f.next(), f.next(), f.next()) else {
                return Err(format!("`{}`: {}", e.path, line.trim_end()));
            };
            if kind != "blob" {
                return Err(format!("`{}` is not a blob ({kind})", e.path));
            }
            let size: usize = size.parse().map_err(|_| "a bad blob size".to_string())?;
            let mut body = vec![0u8; size];
            rx.read_exact(&mut body).map_err(|e| e.to_string())?;
            let mut nl = [0u8; 1];
            rx.read_exact(&mut nl).map_err(|e| e.to_string())?;
            tar_member(&mut out, &e.path, e.mode, *t, &body)
                .map_err(|err| format!("writing `{}` into the container: {err}", e.path))?;
            let expect = if e.is_link() {
                Expect::Link(String::from_utf8_lossy(&body).into_owned())
            } else {
                Expect::File(
                    Sha256::digest(&body)
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect(),
                    e.mode & 0o111 != 0,
                )
            };
            held.push((e.path.clone(), expect));
        }
        out.write_all(&[0u8; 1024]).map_err(|e| e.to_string())?;
        out.flush().map_err(|e| e.to_string())
    })();
    drop(tx);
    let _ = child.kill();
    let _ = child.wait();
    result.map(|()| held)
}

// ---------------------------------------------------------------------------------------------
// Running the runtime's CLI: bounded, streamed both ways.
// ---------------------------------------------------------------------------------------------

/// What [`run_fed`] observed.
struct Fed {
    /// `None` when the command ran past its deadline (it was killed).
    status: Option<ExitStatus>,
    out: Vec<u8>,
    err: Vec<u8>,
    /// The feeder's own result (a broken pipe when the command died early).
    fed: Result<(), String>,
}

/// Run `cmd` to completion within `limit` while `feed` writes its stdin: stdout and stderr are
/// drained on their own threads (a full pipe never stalls it) and a watchdog kills its process
/// group at the deadline, so neither the feed nor the wait can block past it.
fn run_fed(
    mut cmd: Command,
    limit: Duration,
    feed: impl FnOnce(&mut dyn Write) -> Result<(), String>,
) -> Result<Fed, String> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|e| format!("could not start: {e}"))?;
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
    let done = Arc::new(AtomicBool::new(false));
    let fired = Arc::new(AtomicBool::new(false));
    {
        let (done, fired, pid) = (done.clone(), fired.clone(), child.id());
        std::thread::spawn(move || {
            let start = Instant::now();
            while !done.load(Ordering::Relaxed) {
                if start.elapsed() >= limit {
                    fired.store(true, Ordering::Relaxed);
                    #[cfg(unix)]
                    // SAFETY: killpg on the child's own process group (pgid == its pid).
                    unsafe {
                        libc::killpg(pid as libc::pid_t, libc::SIGKILL);
                    }
                    #[cfg(not(unix))]
                    let _ = pid;
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
    }
    let fed = match child.stdin.take() {
        Some(s) => {
            let mut w = std::io::BufWriter::with_capacity(1 << 20, s);
            let r = feed(&mut w);
            r.and_then(|()| w.flush().map_err(|e| e.to_string()))
        }
        None => Err("no stdin".into()),
    };
    let waited = child.wait();
    done.store(true, Ordering::Relaxed);
    let grace = Duration::from_secs(5);
    let out = out.recv_timeout(grace).unwrap_or_default();
    let err = err.recv_timeout(grace).unwrap_or_default();
    let status = if fired.load(Ordering::Relaxed) {
        None
    } else {
        waited.ok()
    };
    Ok(Fed {
        status,
        out,
        err,
        fed,
    })
}

/// Run a runtime command bounded; `Ok(stdout)` on success.
fn rt_run(rt: &Runtime, args: &[&str], limit: Duration) -> Result<String, String> {
    let mut c = rt.cmd();
    c.args(args);
    let o = output_within(c, rt.bound(limit))
        .ok_or_else(|| format!("`{} {}` did not answer", rt.cli.display(), args[0]))?;
    if o.status.success() {
        Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
    } else {
        Err(format!(
            "`{} {}` failed: {}",
            rt.cli.display(),
            args.join(" "),
            String::from_utf8_lossy(&o.stderr).trim()
        ))
    }
}

fn tail(bytes: &[u8], lines: usize) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut t: Vec<&str> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .rev()
        .take(lines)
        .collect();
    t.reverse();
    t.join(" | ")
}

// ---------------------------------------------------------------------------------------------
// Volume locks, leases and use stamps.
// ---------------------------------------------------------------------------------------------

/// Where every engine on this host takes its volume locks and leases. Volume names are global to
/// the runtime, so two daemons with different worker homes must share these (the user's temp dir:
/// the runtime is the user's).
fn lock_dir() -> PathBuf {
    std::env::temp_dir().join("wicked-checks-floor-locks")
}

/// An exclusive lock on `volume` for this process: a fill, a lease, or an eviction of it.
struct VolumeLock(#[allow(dead_code)] std::fs::File);

fn lock_volume(volume: &str) -> Result<VolumeLock, String> {
    let dir = lock_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(format!("{volume}.lock")))
        .map_err(|e| e.to_string())?;
    f.lock().map_err(|e| e.to_string())?;
    Ok(VolumeLock(f))
}

/// Take a lease on `volume` for this floor (call under its [`lock_volume`]): the eviction pass,
/// which takes the same lock, never removes a leased volume.
fn lease(volume: &str, held: &Mutex<Vec<PathBuf>>) {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let path = lock_dir().join(format!(
        "{volume}.lease-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    if std::fs::write(&path, b"").is_ok() {
        if let Ok(mut h) = held.lock() {
            h.push(path);
        }
    }
}

/// Whether a live process holds a lease on `volume` (dead holders' leases are cleared).
fn leased(volume: &str) -> bool {
    let prefix = format!("{volume}.lease-");
    let Ok(rd) = std::fs::read_dir(lock_dir()) else {
        return false;
    };
    let mut live = false;
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        let Some(rest) = n.strip_prefix(&prefix) else {
            continue;
        };
        let pid = rest.split('-').next().and_then(|p| p.parse::<i32>().ok());
        match pid {
            Some(p) if pid_alive(p) => live = true,
            _ => {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    live
}

/// Where the engine keeps the container floor's use stamps: a dot directory of the check cache
/// holding files only (the cap pass treats a directory there as a repository group and its
/// subdirectories as run leaves; this one has none).
fn state_dir(cache: &Path) -> PathBuf {
    cache.join(".checks-floor")
}

/// Mark `volume` used now (its LRU order for [`evict`]).
fn stamp(cache: &Path, volume: &str, repo: &str) {
    let dir = state_dir(cache);
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join(format!("{volume}.used")), repo);
}

// ---------------------------------------------------------------------------------------------
// The floor.
// ---------------------------------------------------------------------------------------------

/// One `node_modules` volume and where it goes in a side's tree.
#[derive(Debug, Clone)]
struct NmMount {
    dir: String,
    volume: String,
    /// The install failed on the repository's own inputs: its log, shown by the install check.
    failed: Option<String>,
}

/// What one side's container is made from.
#[derive(Debug, Clone)]
struct SidePlan {
    entries: Vec<Entry>,
    nm: Vec<NmMount>,
    cargo: bool,
}

/// One side's running container.
#[derive(Debug)]
struct Live {
    name: String,
    /// The `oom_kill` count read after the last check.
    oom_kills: u64,
    /// Which base export this container holds (the export directory's identity).
    export: Option<(u64, u64)>,
    /// Set when the container can no longer give evidence (it stopped answering, or a check
    /// rewrote the copied tree): every later check there could not run.
    dead: Option<String>,
    /// What every copied path holds, for the after-check comparison.
    expect: Vec<(String, Expect)>,
}

/// Why a container floor could not be prepared: `auto` falls back on these only.
#[derive(Debug)]
pub(crate) struct Unavailable(pub(crate) String);

/// What a fill did.
#[derive(Debug)]
enum Fill {
    Hit,
    Filled,
    /// The repository's own install failed (its log).
    Failed(String),
}

impl Fill {
    fn state(&self) -> &'static str {
        match self {
            Fill::Hit => "hit",
            Fill::Filled => "filled",
            Fill::Failed(_) => "failed",
        }
    }
}

/// A container floor for one floor run: the head prepared, containers started on first use.
#[derive(Debug)]
pub(crate) struct ChecksFloor {
    rt: Runtime,
    tag: String,
    image_id: String,
    git_dir: PathBuf,
    head_root: PathBuf,
    head_tree: String,
    base_commit: Option<String>,
    memory: String,
    jobs: u32,
    run_label: String,
    leaf: PathBuf,
    cache: PathBuf,
    repo: String,
    dirs: Vec<String>,
    plans: Mutex<[Option<Result<SidePlan, String>>; 2]>,
    seq: AtomicU32,
    live: Mutex<[Option<Live>; 2]>,
    record: Arc<Mutex<FloorRunner>>,
    leases: Mutex<Vec<PathBuf>>,
}

fn idx(tree: Tree) -> usize {
    match tree {
        Tree::Head => 0,
        Tree::Base => 1,
    }
}

impl ChecksFloor {
    /// Prepare a container floor for `worktree`: the runtime answers, the image is there (built on
    /// first use), the head's tree is known (`tree`: a guarded checkout's tree id; else the
    /// worktree's content snapshot), the image holds every tool the checks name, and every cache
    /// volume the HEAD needs is filled. The base is prepared only when a base check first needs
    /// it. `Err` is what the container cannot do here (`auto` falls back with it).
    pub(crate) fn prepare(
        worktree: &Path,
        ctx: &crate::repo_checks::FloorContext,
        tree: Option<&str>,
        leaf: &Path,
        cache: &Path,
        detected: &[RepoCheck],
    ) -> Result<Self, Unavailable> {
        let no = Unavailable;
        let started = Instant::now();
        let git_dir = ctx
            .git_dir
            .clone()
            .ok_or_else(|| no("the floor has no pinned git dir to copy the tree from".into()))?;
        // What the container cannot do, decided from the inputs before anything starts.
        if let Some(c) = detected
            .iter()
            .find(|c| c.name == "install" && npm_ci(c).is_none())
        {
            return Err(no(format!(
                "the container floor fills node_modules only from an npm lockfile (`npm ci`); \
                 this install is `{}`",
                c.argv.join(" ")
            )));
        }
        if let Some(base) = ctx.base_head.as_deref().filter(|b| b.len() >= 7) {
            if let Some(c) = detected
                .iter()
                .find(|c| c.argv.iter().any(|a| a.contains(base)))
            {
                return Err(no(format!(
                    "`{}` names the base commit, which needs the repository's history; the \
                     container's copy has none",
                    c.name
                )));
            }
        }
        if let Some(Ok(Some(guide))) = ctx
            .base_head
            .as_deref()
            .map(|base| crate::floor_guide::load_base(worktree, &git_dir, base))
        {
            if let Some(g) = guide.checks.iter().find(|g| g.results.how.path.is_some()) {
                return Err(no(format!(
                    "the build guide's `{}` reads its results from a file; the container floor \
                     reads results from the output only",
                    g.name
                )));
            }
        }
        let rt = crate::container_floor::runtime().map_err(|e| no(e.describe().to_string()))?;
        let server = rt_run(
            &rt,
            &[
                "version",
                "--format",
                "{{.Server.Os}}/{{.Server.Arch}} {{.Server.Version}}",
            ],
            PROBE,
        )
        .map_err(no)?;
        let platform = server.split(' ').next().unwrap_or("").to_string();
        if !platform.starts_with("linux/") {
            return Err(no(format!(
                "the container runtime's server is `{server}`, not linux"
            )));
        }
        let cpus = crate::container_floor::seat_cpus();
        let (memory, _, jobs) =
            memory_and_jobs(std::env::var(MEMORY_ENV).ok().as_deref(), cpus).map_err(no)?;
        let tag = image_tag();
        if !crate::container_floor::image_present(&rt, &tag) {
            static BUILD: Mutex<()> = Mutex::new(());
            let _one = BUILD.lock();
            if !crate::container_floor::image_present(&rt, &tag) {
                eprintln!("wicked-core: building the checks floor image {tag} (first use)");
                crate::container_floor::build_image(&rt, DOCKERFILE, &tag, &[]).map_err(no)?;
            }
        }
        let image_id = rt_run(
            &rt,
            &["image", "inspect", "--format", "{{.Id}}", &tag],
            PROBE,
        )
        .map_err(no)?;
        // Every tool a check names by bare name must be in the image (a repository whose checks
        // run `pytest` or `go` keeps the host floor under `auto`), and the runtime must count OOM
        // kills (cgroup v2 `memory.events` or v1 `memory.oom_control`): without that count an OOM
        // kill under a check that exits 0 could not be told from a pass.
        let mut tools: Vec<&str> = detected
            .iter()
            .filter_map(|c| c.argv.first().map(String::as_str))
            .filter(|b| !b.contains('/') && !b.is_empty())
            .collect();
        tools.sort_unstable();
        tools.dedup();
        let mut cmd: Vec<&str> = vec![
            "sh",
            "-c",
            "[ -r /sys/fs/cgroup/memory.events ] || [ -r /sys/fs/cgroup/memory/memory.oom_control ] \
             || echo '<no OOM accounting>'; for b do command -v \"$b\" >/dev/null 2>&1 || echo \"$b\"; done",
            "sh",
        ];
        cmd.extend(tools.iter().copied());
        let (_, missing) = probe_run(&rt, &image_id, &[], &cmd).map_err(no)?;
        if missing.contains("<no OOM accounting>") {
            return Err(no(
                "the container runtime exposes no OOM kill count (cgroup memory events)".into(),
            ));
        }
        if !missing.trim().is_empty() {
            return Err(no(format!(
                "the checks floor image has no {}",
                missing.split_whitespace().collect::<Vec<_>>().join(", ")
            )));
        }
        let head_tree = match tree {
            Some(t) if !t.is_empty() => t.to_string(),
            _ => {
                crate::worktree_guard::snapshot_through(worktree, &git_dir)
                    .map_err(|e| no(format!("the head tree could not be snapshotted: {e}")))?
                    .tree
            }
        };
        let run_label = ctx.run_id.clone().unwrap_or_else(|| "no-run".to_string());
        let repo = leaf
            .parent()
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        maintain(&rt, cache, &image_id);
        // The package dirs whose lockfile the floor mounts a node_modules for, and the cargo
        // workspaces it fetches: the root and every detected check's directory.
        let mut dirs: Vec<String> = vec![String::new()];
        for c in detected {
            if !dirs.contains(&c.dir) {
                dirs.push(c.dir.clone());
            }
        }
        let runtime = format!("{} ({server})", rt.cli.display());
        let floor = ChecksFloor {
            rt,
            tag: tag.clone(),
            image_id: image_id.clone(),
            git_dir,
            head_root: worktree.to_path_buf(),
            head_tree: head_tree.clone(),
            base_commit: ctx.base_head.clone(),
            memory: memory.clone(),
            jobs,
            run_label,
            leaf: leaf.to_path_buf(),
            cache: cache.to_path_buf(),
            repo,
            dirs,
            plans: Mutex::new([None, None]),
            seq: AtomicU32::new(0),
            live: Mutex::new([None, None]),
            record: Arc::new(Mutex::new(FloorRunner {
                runner: "container".into(),
                fallback_reason: None,
                image: Some(format!("{tag}@{image_id}")),
                runtime: Some(runtime),
                platform: Some(platform),
                memory_cap: Some(memory),
                build_jobs: None,
                network: "none".into(),
                tree: Some(head_tree),
                caches: Vec::new(),
                containers: Vec::new(),
                prepare_ms: None,
                copy_in_ms: Some(0),
                platform_scope: Some(PLATFORM_SCOPE.to_string()),
            })),
            leases: Mutex::new(Vec::new()),
        };
        // The head's caches now: what the container cannot fill is a reason to fall back; what
        // fails on the repository's own inputs is the floor's evidence (`SidePlan` keeps it).
        let head = floor.plan_side(Tree::Head).map_err(no)?;
        if let Ok(mut p) = floor.plans.lock() {
            p[0] = Some(Ok(head));
        }
        if let Ok(mut r) = floor.record.lock() {
            r.prepare_ms = Some(started.elapsed().as_millis() as u64);
        }
        Ok(floor)
    }

    /// The runtime CLI (the report's launcher).
    pub(crate) fn cli(&self) -> String {
        self.rt.cli.to_string_lossy().into_owned()
    }

    /// The report's record of this floor.
    pub(crate) fn record(&self) -> FloorRunner {
        self.record.lock().map(|r| r.clone()).unwrap_or_default()
    }

    /// The record, readable after the floor is gone (the scratch that owns the floor drops it,
    /// with its containers and leases, before its leaf can be retired).
    pub(crate) fn record_handle(&self) -> Arc<Mutex<FloorRunner>> {
        self.record.clone()
    }

    /// The identity a reusable result of this floor carries (creator records, journals, base
    /// records): a result taken in this image is never stood on by another runner, nor the reverse.
    pub(crate) fn posture(&self) -> String {
        format!("container:{}", self.image_id)
    }

    /// The side's plan, made (and its caches filled) on first use.
    fn plan(&self, tree: Tree) -> Result<SidePlan, String> {
        if let Some(p) = self.plans.lock().ok().and_then(|p| p[idx(tree)].clone()) {
            return p;
        }
        let made = self.plan_side(tree);
        if let Ok(mut p) = self.plans.lock() {
            p[idx(tree)] = Some(made.clone());
        }
        made
    }

    /// Read `tree`'s entries and fill every cache volume it needs. `Err` is infrastructure (the
    /// runtime, the network, the object store), never the repository's own install failing.
    fn plan_side(&self, tree: Tree) -> Result<SidePlan, String> {
        let id = match tree {
            Tree::Head => self.head_tree.clone(),
            Tree::Base => self
                .base_commit
                .clone()
                .ok_or("the floor has no run base to copy in")?,
        };
        let entries = ls_tree(&self.git_dir, &id)?;
        let has = |rel: &str| entries.iter().any(|e| e.path == rel);
        let rel = |dir: &str, f: &str| {
            if dir.is_empty() {
                f.to_string()
            } else {
                format!("{dir}/{f}")
            }
        };
        let mut nm = Vec::new();
        let mut cargo = false;
        let mut caches = Vec::new();
        for dir in &self.dirs {
            // A lockfile with no manifest beside it is no npm package (the host floor's detection
            // reads the manifest too): nothing to install there.
            // npm's own choice of lockfile: a shrinkwrap wins over package-lock.json.
            let lock_name = ["npm-shrinkwrap.json", "package-lock.json"]
                .into_iter()
                .find(|f| has(&rel(dir, f)));
            if let Some(lock_name) = lock_name.filter(|_| has(&rel(dir, "package.json"))) {
                if entries
                    .iter()
                    .any(|e| e.path == rel(dir, lock_name) && e.is_link())
                {
                    // Its bytes are its referent's, which the floor would have to resolve the way
                    // npm does before it could vet them.
                    return Err(format!(
                        "`{}` is a link; the container floor reads only a lockfile it can vet",
                        rel(dir, lock_name)
                    ));
                }
                let lock = read_blob(&self.git_dir, &id, &rel(dir, lock_name))
                    .ok_or("the lockfile could not be read")?;
                if let Some(dep) = git_dependency(&lock) {
                    // npm runs a git dependency's `prepare` even with scripts off, and the
                    // installer has the network: not something the container floor runs.
                    return Err(format!(
                        "`{}` installs a git dependency ({dep}), whose `prepare` script npm runs \
                         even with scripts off; the container floor's networked installer does not \
                         run repository-chosen code",
                        rel(dir, lock_name)
                    ));
                }
                let volume = nm_volume(&self.tag, dir, &lock, &npm_inputs(&entries, &lock, &id));
                let t = Instant::now();
                let filled = self.fill_nm(&entries, dir, &volume)?;
                caches.push(CacheUse {
                    kind: "node_modules".into(),
                    volume: volume.clone(),
                    state: filled.state().into(),
                    fill_ms: (!matches!(filled, Fill::Hit)).then(|| t.elapsed().as_millis() as u64),
                });
                nm.push(NmMount {
                    dir: dir.clone(),
                    volume,
                    failed: match filled {
                        Fill::Failed(log) => Some(log),
                        _ => None,
                    },
                });
            }
            let locked_here = has(&rel(dir, "Cargo.lock"));
            if !locked_here && has(&rel(dir, "Cargo.toml")) && !has("Cargo.lock") {
                // A cargo package with no lockfile resolves on the network, which the checks do
                // not have.
                return Err(format!(
                    "`{}` has no Cargo.lock to fetch from; the container floor's checks have no \
                     network to resolve one",
                    rel(dir, "Cargo.toml")
                ));
            }
            if locked_here {
                cargo = true;
                let lock = read_blob(&self.git_dir, &id, &rel(dir, "Cargo.lock"))
                    .ok_or("Cargo.lock could not be read")?;
                let marker = key16(&[
                    self.tag.as_bytes(),
                    dir.as_bytes(),
                    &lock,
                    &cargo_inputs(&entries, &id),
                ]);
                let t = Instant::now();
                let filled = self.fill_cargo(&entries, dir, &marker)?;
                let ms = (!matches!(filled, Fill::Hit)).then(|| t.elapsed().as_millis() as u64);
                // One registry entry per side: the worst state of its workspaces' fetches.
                match caches
                    .iter_mut()
                    .find(|c: &&mut CacheUse| c.kind == "cargo-registry")
                {
                    Some(c) => {
                        if c.state == "hit" || filled.state() == "failed" {
                            c.state = filled.state().into();
                        }
                        if let Some(ms) = ms {
                            c.fill_ms = Some(c.fill_ms.unwrap_or(0) + ms);
                        }
                    }
                    None => caches.push(CacheUse {
                        kind: "cargo-registry".into(),
                        volume: CARGO_HOME_VOLUME.into(),
                        state: filled.state().into(),
                        fill_ms: ms,
                    }),
                }
            }
        }
        if let Ok(mut r) = self.record.lock() {
            r.caches.extend(caches);
            if cargo {
                // What the checks really get: an operator's own value wins, as for the seats.
                r.build_jobs = Some(
                    std::env::var("CARGO_BUILD_JOBS")
                        .ok()
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(self.jobs),
                );
            }
        }
        Ok(SidePlan { entries, nm, cargo })
    }

    /// The environment every check of a side sees inside, before a guide's own.
    fn env(&self, cargo: bool) -> Vec<(String, String)> {
        let mut env: Vec<(String, String)> = [
            ("PATH", PATH_IN),
            ("HOME", HOME),
            ("TMPDIR", "/tmp"),
            ("TMP", "/tmp"),
            ("TEMP", "/tmp"),
            ("XDG_CONFIG_HOME", "/home/floor/.config"),
            ("XDG_CACHE_HOME", "/home/floor/.cache"),
            ("npm_config_cache", "/home/floor/.npm"),
            ("npm_config_update_notifier", "false"),
            ("npm_config_fund", "false"),
            ("npm_config_audit", "false"),
            ("npm_config_offline", "true"),
            ("CI", "1"),
            ("NO_COLOR", "1"),
            ("FORCE_COLOR", "0"),
            ("PYTHONDONTWRITEBYTECODE", "1"),
            ("LANG", "C.UTF-8"),
            ("USER", "node"),
            ("LOGNAME", "node"),
            (MARKER_ENV, "container"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        if cargo {
            for (k, v) in [
                ("CARGO_HOME", CARGO_HOME_DIR),
                ("CARGO_TARGET_DIR", TARGET_DIR),
                ("CARGO_NET_OFFLINE", "true"),
            ] {
                env.push((k.into(), v.into()));
            }
            // The seat build table (debuginfo 0, incremental off) with jobs sized to the memory
            // cap; an operator's own value in the daemon's environment wins, as for the seats.
            let jobs = self.jobs as usize;
            for (k, v) in crate::worker_sandbox::floor_build_env(|| jobs, |k| std::env::var(k).ok())
            {
                let v = if k == "CARGO_BUILD_JOBS" && std::env::var(k).is_err() {
                    jobs.to_string()
                } else {
                    v
                };
                env.push((k.to_string(), v));
            }
        }
        env
    }

    fn work(tree: Tree) -> String {
        format!("/work/{}", side(tree))
    }

    /// Start `tree`'s container and copy its tree in.
    fn start(&self, tree: Tree, export: Option<(u64, u64)>) -> Result<Live, String> {
        let plan = self.plan(tree).map_err(|e| match tree {
            Tree::Head => e,
            Tree::Base => format!("the base could not be prepared: {e}"),
        })?;
        let t0 = Instant::now();
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        let run8: String = self
            .run_label
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .take(8)
            .collect();
        let name = format!(
            "wfloor-{run8}-{}-{}-{n}",
            side(tree),
            &key16(&[
                self.leaf.to_string_lossy().as_bytes(),
                format!("{:?}{}", SystemTime::now(), std::process::id()).as_bytes()
            ])[..6]
        );
        let work = Self::work(tree);
        if plan.cargo {
            // Made with its labels (a volume a mount creates has none), leased, stamped.
            let target = target_volume(&self.leaf, tree);
            let _lock = lock_volume(&target)?;
            let warm = volume_exists(&self.rt, &target);
            rt_run(
                &self.rt,
                &[
                    "volume",
                    "create",
                    "--label",
                    &format!("{LABEL}.kind=target"),
                    "--label",
                    &format!("{LABEL}.leaf={}", leaf_key(&self.leaf)),
                    &target,
                ],
                PROBE,
            )?;
            lease(&target, &self.leases);
            if let Ok(mut r) = self.record.lock() {
                r.caches.push(CacheUse {
                    kind: "cargo-target".into(),
                    volume: target.clone(),
                    state: if warm { "hit" } else { "created" }.into(),
                    fill_ms: None,
                });
            }
            stamp(&self.cache, &target, &self.repo);
        }
        let mut args: Vec<String> = ["run", "-d", "--rm", "--name", &name]
            .map(String::from)
            .to_vec();
        args.extend([
            "--label".into(),
            format!("{LABEL}={}", self.run_label),
            "--label".into(),
            format!("{LABEL}.pid={}", std::process::id()),
            "--read-only".into(),
            "--network".into(),
            "none".into(),
            "--cap-drop".into(),
            "ALL".into(),
            "--security-opt".into(),
            "no-new-privileges".into(),
            "--init".into(),
            "--pids-limit".into(),
            "8192".into(),
            "--user".into(),
            IDLE_USER.into(),
            "--memory".into(),
            self.memory.clone(),
            "--memory-swap".into(),
            self.memory.clone(),
            "--tmpfs".into(),
            // 0755: the engine's root exec (no capabilities) must traverse it to open the parents
            // the runtime made for a subdirectory package's mount point.
            format!("{work}:rw,exec,uid=1000,gid=1000,mode=0755,size=8g"),
            "--tmpfs".into(),
            "/tmp:rw,exec,mode=1777,size=8g".into(),
            "--tmpfs".into(),
            format!("{HOME}:rw,exec,uid=1000,gid=1000,mode=0700,size=2g"),
        ]);
        // Every node_modules in place (relative workspace links resolve against its package).
        for m in plan.nm.iter().filter(|m| m.failed.is_none()) {
            args.extend([
                "--mount".into(),
                format!(
                    "type=volume,src={},dst={},readonly",
                    m.volume,
                    nm_path(&work, &m.dir)
                ),
            ]);
        }
        if plan.cargo {
            args.extend([
                "--mount".into(),
                format!("type=volume,src={CARGO_HOME_VOLUME},dst={CARGO_HOME_DIR},readonly"),
                "--mount".into(),
                format!(
                    "type=volume,src={},dst={TARGET_DIR}",
                    target_volume(&self.leaf, tree)
                ),
            ]);
        }
        for (k, v) in self.env(plan.cargo) {
            args.extend(["-e".into(), format!("{k}={v}")]);
        }
        args.extend([self.image_id.clone(), "sleep".into(), "infinity".into()]);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        if let Err(e) = rt_run(&self.rt, &refs, START) {
            // A start whose answer was lost may still have made the container: it is removed by
            // its name either way.
            self.remove(&name);
            return Err(format!("the floor container did not start: {e}"));
        }
        // From here on the container is ours to remove, whatever fails.
        let copied = (|| {
            // The runtime made a subdirectory package's mount point's parents as root: open them
            // to the checks' user (an owner may chmod without any capability), or the copy cannot
            // write into them.
            let parents = nested_parents(&work, &plan.nm);
            if !parents.is_empty() {
                let mut a: Vec<&str> = vec!["exec", "-u", "0", &name, "chmod", "0777"];
                a.extend(parents.iter().map(String::as_str));
                rt_run(&self.rt, &a, REAP)?;
            }
            self.copy_in(tree, &name, &plan)
        })();
        let expect = match copied {
            Ok(x) => x,
            Err(e) => {
                self.remove(&name);
                return Err(e);
            }
        };
        if let Ok(mut r) = self.record.lock() {
            r.containers.push(name.clone());
            r.copy_in_ms = Some(r.copy_in_ms.unwrap_or(0) + t0.elapsed().as_millis() as u64);
        }
        Ok(Live {
            name,
            oom_kills: 0,
            export,
            dead: None,
            expect,
        })
    }

    /// Stream the side's files into `name` (every path under a mounted node_modules left out),
    /// then unpack each node_modules volume's nested tar. Returns what the copy holds.
    fn copy_in(
        &self,
        tree: Tree,
        name: &str,
        plan: &SidePlan,
    ) -> Result<Vec<(String, Expect)>, String> {
        let live_nm: Vec<&NmMount> = plan.nm.iter().filter(|m| m.failed.is_none()).collect();
        let mounted: Vec<String> = live_nm.iter().map(|m| nm_rel(&m.dir)).collect();
        // Nothing of the tree is written where a node_modules volume is mounted (read-only): not
        // a file under it, nor a tracked link in its place.
        let under = |p: &str, m: &str| p == m || p.starts_with(&format!("{m}/"));
        let entries: Vec<Entry> = plan
            .entries
            .iter()
            .filter(|e| !mounted.iter().any(|m| under(&e.path, m)))
            .cloned()
            .collect();
        let manifest = self
            .leaf
            .join(format!("container-mtimes-{}.json", side(tree)));
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let (times, next) = Mtimes::load(&manifest).assign(&entries, now);
        let work = Self::work(tree);
        let mut script = format!("set -e; tar -x -f - -C {work}");
        for m in &live_nm {
            let d = if m.dir.is_empty() {
                work.clone()
            } else {
                format!("{work}/{}", m.dir)
            };
            let nested = sh_quote(&format!("{d}/node_modules/{NESTED_TAR}"));
            // Another package's mounted node_modules below this one is never overwritten.
            let mut excludes = String::new();
            for o in live_nm.iter().filter(|o| o.dir != m.dir) {
                let rel = if m.dir.is_empty() {
                    Some(o.dir.as_str())
                } else {
                    o.dir.strip_prefix(&format!("{}/", m.dir))
                };
                if let Some(rel) = rel {
                    excludes.push_str(&format!(
                        " --exclude={} --exclude={}",
                        sh_quote(&format!("./{rel}/node_modules")),
                        sh_quote(&format!("./{rel}/node_modules/*"))
                    ));
                }
            }
            script.push_str(&format!(
                "; if [ -f {nested} ]; then tar -x -f {nested}{excludes} -C {}; fi",
                sh_quote(&d)
            ));
        }
        let mut c = self.rt.cmd();
        c.args(["exec", "-i", "-u", CHECKS_USER, name, "sh", "-c", &script]);
        let mut held = Vec::new();
        let fed = run_fed(c, self.rt.bound(COPY), |w| {
            held = write_tree_tar(&self.git_dir, &entries, &times, w)?;
            Ok(())
        })
        .map_err(|e| format!("the tree copy-in: {e}"))?;
        match fed.status {
            Some(s) if s.success() && fed.fed.is_ok() => {}
            None => {
                return Err(format!(
                    "the tree copy-in ran past {:?}",
                    self.rt.bound(COPY)
                ))
            }
            Some(s) => {
                return Err(format!(
                    "the tree copy-in failed ({s}{}): {}",
                    fed.fed.err().map(|e| format!("; {e}")).unwrap_or_default(),
                    tail(&fed.err, 6)
                ))
            }
        }
        next.save(&manifest);
        // The nested node_modules the volumes unpacked are writable by the checks' user like the
        // tree, so they are compared after every check too: what they hold now joins the copy's
        // record, in place of any tracked file they replaced.
        if !live_nm.is_empty() {
            let (dirs, files) = self.hash_nested(name, tree, &live_nm)?;
            held.retain(|(p, _)| !dirs.iter().any(|d| under(p, d)));
            held.extend(files);
        }
        Ok(held)
    }

    /// The nested node_modules directories the volumes' archives unpacked (relative to the side's
    /// root) and what every file under them holds, read inside the container.
    #[allow(clippy::type_complexity)]
    fn hash_nested(
        &self,
        name: &str,
        tree: Tree,
        nm: &[&NmMount],
    ) -> Result<(Vec<String>, Vec<(String, Expect)>), String> {
        const SCRIPT: &str = r#"import sys,json,hashlib,os,stat,tarfile
root=sys.argv[1]; dirs=[]; files=[]
for pkg in sys.argv[2:]:
    base=os.path.join(root,pkg) if pkg else root
    t=os.path.join(base,'node_modules','.wicked-nested.tar')
    if not os.path.isfile(t):
        continue
    with tarfile.open(t) as tf:
        for n in tf.getnames():
            parts=[x for x in n.split('/') if x not in ('','.')]
            if 'node_modules' in parts:
                d='/'.join(parts[:parts.index('node_modules')+1])
                d=(pkg+'/'+d) if pkg else d
                if d not in dirs:
                    dirs.append(d)
for d in dirs:
    top=os.path.join(root,d)
    if os.path.ismount(top) or not os.path.isdir(top) or os.path.islink(top):
        continue
    for cur,subs,names in os.walk(top):
        subs[:]=[s for s in subs if not os.path.ismount(os.path.join(cur,s))]
        for x in names+[s for s in subs if os.path.islink(os.path.join(cur,s))]:
            f=os.path.join(cur,x); rel=os.path.relpath(f,root); st=os.lstat(f)
            if stat.S_ISLNK(st.st_mode):
                files.append([rel,{'l':os.readlink(f)}])
            elif stat.S_ISREG(st.st_mode):
                h=hashlib.sha256()
                with open(f,'rb') as fh:
                    for b in iter(lambda: fh.read(1<<20), b''):
                        h.update(b)
                files.append([rel,{'f':[h.hexdigest(),bool(st.st_mode & 0o111)]}])
print(json.dumps({'dirs':dirs,'files':files}))
"#;
        let work = Self::work(tree);
        let mut c = self.rt.cmd();
        c.args([
            "exec",
            "-i",
            "-u",
            CHECKS_USER,
            name,
            "python3",
            "-I",
            "-c",
            SCRIPT,
            &work,
        ]);
        c.args(nm.iter().map(|m| m.dir.as_str()));
        let fed = run_fed(c, self.rt.bound(VERIFY), |_| Ok(()))?;
        if !fed.status.is_some_and(|s| s.success()) {
            return Err(format!(
                "the unpacked node_modules could not be read: {}",
                tail(&fed.err, 4)
            ));
        }
        #[derive(Deserialize)]
        struct Out {
            dirs: Vec<String>,
            files: Vec<(String, serde_json::Value)>,
        }
        let out: Out = serde_json::from_slice(&fed.out).map_err(|e| e.to_string())?;
        let files = out
            .files
            .into_iter()
            .filter_map(|(p, v)| {
                if let Some(l) = v.get("l").and_then(|l| l.as_str()) {
                    return Some((p, Expect::Link(l.to_string())));
                }
                let f = v.get("f")?.as_array()?;
                Some((
                    p,
                    Expect::File(f.first()?.as_str()?.to_string(), f.get(1)?.as_bool()?),
                ))
            })
            .collect();
        Ok((out.dirs, files))
    }

    fn remove(&self, name: &str) {
        let _ = rt_run(&self.rt, &["rm", "--force", name], REAP);
    }

    /// The `docker exec` for one check: `root` must be the floor's head worktree (a head check) or
    /// the base export (a base check, whose identity picks the base container: a fresh export is a
    /// fresh container). `Err` is a reason the check could not run.
    pub(crate) fn command(
        &self,
        tree: Tree,
        root: &Path,
        check: &str,
        dir: &str,
        argv: &[String],
        extra_env: &[(String, String)],
    ) -> Result<Command, String> {
        if argv.is_empty() {
            return Err("empty argv".into());
        }
        let export = match tree {
            Tree::Head => {
                if !same_dir(root, &self.head_root) {
                    return Err(format!(
                        "the container floor runs head checks only in the floor's worktree, not \
                         `{}`",
                        root.display()
                    ));
                }
                None
            }
            Tree::Base => Some(dir_identity(root)?),
        };
        let mut live = self
            .live
            .lock()
            .map_err(|_| "the container floor's state is poisoned".to_string())?;
        let slot = &mut live[idx(tree)];
        let stale = slot.as_ref().is_some_and(|l| l.export != export);
        if stale {
            if let Some(old) = slot.take() {
                self.remove(&old.name);
            }
        }
        if slot.is_none() {
            *slot = Some(self.start(tree, export)?);
        }
        let l = slot.as_ref().expect("started above");
        if let Some(why) = &l.dead {
            return Err(why.clone());
        }
        let cwd = if dir.is_empty() {
            Self::work(tree)
        } else {
            format!("{}/{dir}", Self::work(tree))
        };
        // The floor's own install step is the cache volume an install container filled for this
        // side: the checks have no network and node_modules is mounted read-only, so the step
        // checks the fill is complete and shows its log (or the failed install's), instead of
        // running npm again.
        let fill = (check == "install" && argv.len() >= 2 && argv[0] == "npm" && argv[1] == "ci")
            .then(|| {
                self.plan(tree)
                    .ok()
                    .and_then(|p| p.nm.into_iter().find(|m| m.dir == dir))
            })
            .flatten();
        let argv: Vec<String> = match fill {
            Some(NmMount {
                failed: Some(log), ..
            }) => vec![
                "sh".into(),
                "-c".into(),
                "printf '%s\\n' \"$1\"; exit 1".into(),
                "install".into(),
                format!(
                    "wicked-core: `{}` failed in the install container:\n{log}",
                    NPM_FILL.join(" ")
                ),
            ],
            Some(m) => vec![
                "sh".into(),
                "-c".into(),
                format!(
                    "echo 'wicked-core: node_modules is the floor cache volume {vol} (filled by \
                     `{cmd}` in an install container with the network; mounted read-only here)'; \
                     test -f node_modules/{COMPLETE} && cat node_modules/{INSTALL_LOG}",
                    vol = m.volume,
                    cmd = NPM_FILL.join(" "),
                ),
            ],
            None => argv.to_vec(),
        };
        let mut c = self.rt.cmd();
        c.args(["exec", "-u", CHECKS_USER, "-w", &cwd]);
        for (k, v) in extra_env {
            c.args(["-e", &format!("{k}={v}")]);
        }
        c.arg(&l.name).args(reaping(&argv));
        Ok(c)
    }

    /// After a check in `tree`'s container: end everything it left running, then say whether it
    /// can stand as evidence. `Some(reason)` when the container ran out of memory during it,
    /// stopped answering, or the copied tree no longer holds what was copied in — whatever the
    /// check's exit code.
    pub(crate) fn after(&self, tree: Tree) -> Option<String> {
        let Ok(mut live) = self.live.lock() else {
            return Some("floor infrastructure: the container floor's state is poisoned".into());
        };
        let l = live[idx(tree)].as_mut()?;
        if let Some(why) = &l.dead {
            return Some(why.clone());
        }
        let _ = rt_run(
            &self.rt,
            &[
                "exec",
                "-u",
                CHECKS_USER,
                &l.name,
                "sh",
                "-c",
                "kill -9 -1 2>/dev/null; exit 0",
            ],
            REAP,
        );
        // The OOM counter (cgroup v2 `memory.events`, else v1 `memory.oom_control`; preparing the
        // floor refused a runtime with neither); an exec that fails is a container that stopped
        // answering.
        let events = match rt_run(
            &self.rt,
            &["exec", "-u", CHECKS_USER, &l.name, "sh", "-c", OOM_READ],
            REAP,
        ) {
            Ok(e) => e,
            Err(e) => {
                let why = format!(
                    "floor infrastructure: the checks container stopped answering ({e}) — not \
                     a failure of the change"
                );
                l.dead = Some(why.clone());
                return Some(why);
            }
        };
        let kills = oom_kills(&events);
        let oom = (kills > l.oom_kills).then(|| {
            format!(
                "floor infrastructure: the checks container ran out of memory (cap {}, \
                 {MEMORY_ENV}) and the kernel killed a process of this check — not a failure of \
                 the change",
                self.memory
            )
        });
        l.oom_kills = kills;
        // The copy is compared whatever else happened: a later check never runs on a rewritten
        // tree, and a rewrite is never blamed on the next check.
        let changed = match self.verify(&l.name, tree, &l.expect) {
            Ok(changed) if changed.is_empty() => None,
            Ok(changed) => Some(format!(
                "the check changed tracked files of the tree it was judging ({}): what ran after \
                 it ran on another tree, so the floor stands on nothing here",
                changed.join(", ")
            )),
            Err(e) => Some(format!(
                "floor infrastructure: the copied tree could not be compared after the check ({e})"
            )),
        };
        if let Some(why) = &changed {
            l.dead = Some(why.clone());
        }
        changed.or(oom)
    }

    /// The paths of the copy that no longer hold what was copied in (at most 20 named, then a
    /// count), compared inside the container by its own python, isolated (`-I`).
    fn verify(
        &self,
        name: &str,
        tree: Tree,
        expect: &[(String, Expect)],
    ) -> Result<Vec<String>, String> {
        // A raw string: a `\` line continuation would strip the indentation python needs.
        const SCRIPT: &str = r#"import sys,json,hashlib,os,stat
root=sys.argv[1]; m=json.load(sys.stdin); bad=[]; dirs={}
def real_dir(d):
    # Every parent is walked without following a link: a check that swapped a directory for a
    # link to a copy elsewhere is a change even when the bytes there match.
    if d in dirs:
        return dirs[d]
    try:
        st=os.lstat(os.path.join(root,d)) if d else os.lstat(root)
        ok=stat.S_ISDIR(st.st_mode)
    except OSError:
        ok=False
    if ok and d:
        ok=real_dir(os.path.dirname(d))
    dirs[d]=ok
    return ok
for p,e in m:
    k,v=next(iter(e.items()))
    f=os.path.join(root,p)
    try:
        if not real_dir(os.path.dirname(p)):
            ok=False
        elif k=='l':
            ok=os.path.islink(f) and os.readlink(f)==v
        else:
            st=os.lstat(f)
            ok=stat.S_ISREG(st.st_mode) and bool(st.st_mode & 0o111)==v[1]
            if ok:
                h=hashlib.sha256()
                with open(f,'rb') as fh:
                    for b in iter(lambda: fh.read(1<<20), b''):
                        h.update(b)
                ok=h.hexdigest()==v[0]
    except OSError:
        ok=False
    if not ok:
        bad.append(p)
print(json.dumps({'n':len(bad),'paths':bad[:20]}))
"#;
        let body = serde_json::to_vec(expect).map_err(|e| e.to_string())?;
        let work = Self::work(tree);
        let mut c = self.rt.cmd();
        c.args([
            "exec",
            "-i",
            "-u",
            CHECKS_USER,
            name,
            "python3",
            "-I",
            "-c",
            SCRIPT,
            &work,
        ]);
        let fed = run_fed(c, self.rt.bound(VERIFY), |w| {
            w.write_all(&body).map_err(|e| e.to_string())
        })?;
        if !fed.status.is_some_and(|s| s.success()) {
            return Err(format!(
                "the comparison did not finish: {}",
                tail(&fed.err, 4)
            ));
        }
        let v: serde_json::Value = serde_json::from_slice(&fed.out).map_err(|e| e.to_string())?;
        let n = v["n"].as_u64().unwrap_or(0) as usize;
        let mut paths: Vec<String> = v["paths"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|p| p.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if n > paths.len() {
            paths.push(format!("{} more", n - paths.len()));
        }
        Ok(paths)
    }

    /// Whether `url` answers over HTTPS from a container on the runtime's default network (what an
    /// install container has): asked of the network, in the image, by a named throwaway container.
    fn reachable(&self, url: &str) -> bool {
        let name = unique_name("netprobe");
        let pid_label = format!("{LABEL}.pid={}", std::process::id());
        let probe = format!(
            "fetch({url:?},{{signal:AbortSignal.timeout(15000)}}).then(r=>process.exit(r.status<500?0:1),()=>process.exit(1))"
        );
        let ok = rt_run(
            &self.rt,
            &[
                "run",
                "--rm",
                "--name",
                &name,
                "--read-only",
                "--cap-drop",
                "ALL",
                "--user",
                CHECKS_USER,
                "--label",
                LABEL,
                "--label",
                &pid_label,
                &self.image_id,
                "node",
                "-e",
                &probe,
            ],
            START,
        )
        .is_ok();
        let _ = rt_run(&self.rt, &["rm", "--force", &name], REAP);
        ok
    }

    /// Fill `volume` with the `node_modules` of [`NPM_FILL`] in `dir` (once per key), and lease it
    /// for this floor.
    fn fill_nm(&self, entries: &[Entry], dir: &str, volume: &str) -> Result<Fill, String> {
        let _lock = lock_volume(volume)?;
        lease(volume, &self.leases);
        stamp(&self.cache, volume, &self.repo);
        let exists = volume_exists(&self.rt, volume);
        if exists && volume_has(&self.rt, &self.image_id, volume, COMPLETE)? {
            return Ok(Fill::Hit);
        }
        // A volume without its marker is a fill that died: start again from an empty one. One
        // the runtime will not remove (a container still mounts it) is never filled in place.
        if exists {
            rt_run(&self.rt, &["volume", "rm", "--force", volume], REAP)
                .map_err(|e| format!("an unfinished {volume} could not be removed: {e}"))?;
        }
        rt_run(
            &self.rt,
            &[
                "volume",
                "create",
                "--label",
                &format!("{LABEL}.kind=nm"),
                "--label",
                &format!("{LABEL}.repo={}", self.repo),
                volume,
            ],
            PROBE,
        )?;
        let pkg = sh_quote(&if dir.is_empty() {
            "/work/t".to_string()
        } else {
            format!("/work/t/{dir}")
        });
        let cmd = NPM_FILL.join(" ");
        // A node_modules the tree tracks is not the install's: removed before it, so the nested
        // archive holds npm's output only. Nested dirs are archived NUL-delimited, verbatim.
        let script = format!(
            "set -e; mkdir -p /work/t; tar -x -f - -C /work/t; cd {pkg}; \
             find . -mindepth 1 -name node_modules -prune -exec rm -rf {{}} +; \
             echo WICKED-FILL-START; \
             if ! {cmd} > {FILL_DIR}/{INSTALL_LOG} 2>&1; then tail -n 60 {FILL_DIR}/{INSTALL_LOG}; exit 3; fi; \
             mkdir -p node_modules; cp -a node_modules/. {FILL_DIR}/; \
             find . -path ./node_modules -prune -o -type d -name node_modules -prune -print0 \
               | tar --null -T - -c -f {FILL_DIR}/{NESTED_TAR}; \
             {OOM_BEFORE_MARK} touch {FILL_DIR}/{COMPLETE}"
        );
        match self.run_install(
            entries,
            &[format!("type=volume,src={volume},dst={FILL_DIR}")],
            &[
                // Nothing the repository's `.npmrc` names runs here: scripts off, and the programs
                // npm may start pinned to the image's own (environment values win over `.npmrc`).
                // A git dependency (whose `prepare` npm runs regardless) was refused above.
                "npm_config_ignore_scripts=true".to_string(),
                "npm_config_git=/usr/bin/git".to_string(),
                "npm_config_script_shell=/bin/sh".to_string(),
                "npm_config_shell=/bin/sh".to_string(),
            ],
            &script,
        )? {
            None => Ok(Fill::Filled),
            // Whether the network failed is asked of the network, never read from the install's
            // log (repository-controlled text).
            Some(log) if !self.reachable("https://registry.npmjs.org/") => Err(format!(
                "the npm registry could not be reached from the install container: {}",
                tail(log.as_bytes(), 6)
            )),
            // The repository's own install failed: evidence, not a reason to change floors. The
            // volume stays unmarked, so the next floor tries again.
            Some(log) => Ok(Fill::Failed(log)),
        }
    }

    /// Fetch `dir`'s Cargo.lock into the shared cargo home (once per lockfile, `marker`). The copy
    /// leaves out every `.cargo` directory and `rust-toolchain` file, and every compiler wrapper is
    /// overridden, so cargo runs no repository-configured program while it has the network and the
    /// shared cargo home.
    fn fill_cargo(&self, entries: &[Entry], dir: &str, marker: &str) -> Result<Fill, String> {
        let _lock = lock_volume(CARGO_HOME_VOLUME)?;
        let file = format!(".wicked-fetched/{marker}");
        if volume_exists(&self.rt, CARGO_HOME_VOLUME)
            && volume_has(&self.rt, &self.image_id, CARGO_HOME_VOLUME, &file)?
        {
            return Ok(Fill::Hit);
        }
        if !volume_exists(&self.rt, CARGO_HOME_VOLUME) {
            rt_run(
                &self.rt,
                &[
                    "volume",
                    "create",
                    "--label",
                    &format!("{LABEL}.kind=cargo-home"),
                    CARGO_HOME_VOLUME,
                ],
                PROBE,
            )?;
        }
        let pkg = sh_quote(&if dir.is_empty() {
            "/work/t".to_string()
        } else {
            format!("/work/t/{dir}")
        });
        let script = format!(
            "set -e; mkdir -p /work/t; tar -x -f - -C /work/t; cd {pkg}; echo WICKED-FILL-START; \
             if ! cargo fetch --locked --config 'build.rustc=\"rustc\"' \
               --config 'build.rustc-wrapper=\"\"' --config 'build.rustc-workspace-wrapper=\"\"' \
               > /tmp/fetch.log 2>&1; then tail -n 60 /tmp/fetch.log; exit 3; fi; \
             {OOM_BEFORE_MARK} mkdir -p {FILL_DIR}/.wicked-fetched; touch {FILL_DIR}/{file}"
        );
        let filtered = cargo_fill_entries(entries);
        match self.run_install(
            &filtered,
            &[format!(
                "type=volume,src={CARGO_HOME_VOLUME},dst={FILL_DIR}"
            )],
            &[format!("CARGO_HOME={FILL_DIR}")],
            &script,
        )? {
            None => Ok(Fill::Filled),
            Some(log) if !self.reachable("https://index.crates.io/config.json") => Err(format!(
                "the cargo registry could not be reached from the install container: {}",
                tail(log.as_bytes(), 6)
            )),
            // The lockfile does not resolve (or another input of the repository's): the checks
            // then fail offline on it, which is the floor's evidence.
            Some(log) => Ok(Fill::Failed(log)),
        }
    }

    /// Run an install container over `entries` with `script`, the tree streamed in: `Ok(None)` on
    /// success, `Ok(Some(log))` when the install itself failed (the script's exit 3, after its
    /// `WICKED-FILL-START` line), `Err` for anything else (the runtime, the copy, the deadline).
    /// The container is named and labelled, and removed on every path but success.
    fn run_install(
        &self,
        entries: &[Entry],
        mounts: &[String],
        env: &[String],
        script: &str,
    ) -> Result<Option<String>, String> {
        let name = unique_name("install");
        let mut args: Vec<String> = [
            "run",
            "--rm",
            "-i",
            "--name",
            &name,
            "--read-only",
            "--cap-drop",
            "ALL",
            "--security-opt",
            "no-new-privileges",
            "--init",
            "--pids-limit",
            "8192",
            "--user",
            CHECKS_USER,
            "--memory",
            &self.memory,
            "--memory-swap",
            &self.memory,
            "--tmpfs",
            "/work:rw,exec,uid=1000,gid=1000,mode=0700,size=8g",
            "--tmpfs",
            "/tmp:rw,exec,mode=1777,size=4g",
            "--tmpfs",
            "/home/floor:rw,exec,uid=1000,gid=1000,mode=0700,size=2g",
            "-e",
            "HOME=/home/floor",
            "-e",
            "TMPDIR=/tmp",
            "-e",
            "CI=1",
            "-e",
            "NO_COLOR=1",
            "-e",
            "npm_config_cache=/home/floor/.npm",
            "-e",
            "npm_config_update_notifier=false",
        ]
        .map(String::from)
        .to_vec();
        args.extend([
            "--label".into(),
            format!("{LABEL}=install"),
            "--label".into(),
            format!("{LABEL}.pid={}", std::process::id()),
        ]);
        for m in mounts {
            args.extend(["--mount".into(), m.clone()]);
        }
        for e in env {
            args.extend(["-e".into(), e.clone()]);
        }
        // The installer's own OOM count, read before it exits: an install that ran out of memory
        // is infrastructure whether npm then exited 0 or not.
        let wrapped = format!(
            "( {script} ); rc=$?; echo \"WICKED-OOM $( (cat /sys/fs/cgroup/memory.events \
             || cat /sys/fs/cgroup/memory/memory.oom_control) 2>/dev/null | sed -n 's/^oom_kill //p')\"; \
             exit $rc"
        );
        args.extend([self.image_id.clone(), "sh".into(), "-c".into(), wrapped]);
        let times = vec![0u64; entries.len()];
        let mut c = self.rt.cmd();
        c.args(&args);
        let fed = match run_fed(c, self.rt.bound(FILL), |w| {
            write_tree_tar(&self.git_dir, entries, &times, w).map(drop)
        }) {
            Ok(f) => f,
            Err(e) => {
                self.remove(&name);
                return Err(format!("the install container: {e}"));
            }
        };
        let out = String::from_utf8_lossy(&fed.out).into_owned();
        let oom = out
            .lines()
            .rev()
            .find_map(|l| l.strip_prefix("WICKED-OOM "))
            .and_then(|n| n.trim().parse::<u64>().ok())
            .unwrap_or(0);
        if oom > 0 {
            self.remove(&name);
            return Err(format!(
                "the install container ran out of memory (cap {}, {MEMORY_ENV})",
                self.memory
            ));
        }
        match fed.status {
            Some(s) if s.success() => fed.fed.map(|()| None),
            Some(s) if s.code() == Some(3) && out.contains("WICKED-FILL-START") => {
                self.remove(&name);
                let log = out
                    .split_once("WICKED-FILL-START")
                    .map(|(_, l)| l.trim().to_string())
                    .unwrap_or_default();
                Ok(Some(log))
            }
            None => {
                self.remove(&name);
                Err(format!(
                    "the install container ran past {:?}",
                    self.rt.bound(FILL)
                ))
            }
            Some(s) => {
                self.remove(&name);
                Err(format!(
                    "the install container failed ({s}): {}",
                    tail(
                        format!("{out}\n{}", String::from_utf8_lossy(&fed.err)).as_bytes(),
                        8
                    )
                ))
            }
        }
    }
}

impl Drop for ChecksFloor {
    fn drop(&mut self) {
        if let Ok(mut live) = self.live.lock() {
            for l in live.iter_mut().filter_map(Option::take) {
                self.remove(&l.name);
            }
        }
        if let Ok(mut leases) = self.leases.lock() {
            for p in leases.drain(..) {
                let _ = std::fs::remove_file(p);
            }
        }
        // The leaf was retired while this floor held its target volumes: they go now.
        if std::fs::symlink_metadata(&self.leaf).is_err() {
            remove_leaf_volumes(&self.leaf);
        }
    }
}

/// The entries a cargo fetch is given: no `.cargo` directory (its `config.toml` can name a
/// `build.rustc` or wrapper that cargo would run) and no `rust-toolchain` file.
pub(crate) fn cargo_fill_entries(entries: &[Entry]) -> Vec<Entry> {
    entries
        .iter()
        .filter(|e| {
            !e.path.split('/').any(|c| c == ".cargo")
                && !e
                    .path
                    .rsplit('/')
                    .next()
                    .is_some_and(|b| b.starts_with("rust-toolchain"))
        })
        .cloned()
        .collect()
}

/// A package's node_modules, relative to the side's root.
fn nm_rel(dir: &str) -> String {
    if dir.is_empty() {
        "node_modules".to_string()
    } else {
        format!("{dir}/node_modules")
    }
}

/// The path a package's node_modules is mounted at in a side's tree.
fn nm_path(work: &str, dir: &str) -> String {
    if dir.is_empty() {
        format!("{work}/node_modules")
    } else {
        format!("{work}/{dir}/node_modules")
    }
}

/// The directories the runtime creates (as root) for the subdirectory packages' mount points:
/// every parent of each mount point below the side's root, outermost first.
fn nested_parents(work: &str, nm: &[NmMount]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for m in nm
        .iter()
        .filter(|m| m.failed.is_none() && !m.dir.is_empty())
    {
        let mut acc = work.to_string();
        for c in m.dir.split('/') {
            acc = format!("{acc}/{c}");
            if !out.contains(&acc) {
                out.push(acc.clone());
            }
        }
    }
    out
}

/// The check's argv inside a shell that, once the check's own process exits, ends everything
/// else the checks' user runs in the container and exits with the check's code. The runtime's
/// `exec` returns only when its output streams close, and a process a test left behind (holding
/// stdout) would otherwise hold the check open long after it finished — the host floor ends the
/// check's process group at its exit for the same reason. `kill -1` never reaches the shell
/// itself or the container's idle process (another user).
pub(crate) fn reaping(argv: &[String]) -> Vec<String> {
    let mut v: Vec<String> = [
        "sh",
        "-c",
        "\"$@\"; rc=$?; kill -9 -1 2>/dev/null; exit $rc",
        "wicked-check",
    ]
    .map(String::from)
    .to_vec();
    v.extend(argv.iter().cloned());
    v
}

/// In a fill script, before its completion marker: an install that saw an OOM kill marks
/// nothing complete (exit 4: infrastructure), even when npm or cargo exited 0.
const OOM_BEFORE_MARK: &str = "n=$( (cat /sys/fs/cgroup/memory.events \
     || cat /sys/fs/cgroup/memory/memory.oom_control) 2>/dev/null | sed -n 's/^oom_kill //p'); \
     [ \"${n:-0}\" = 0 ] || { echo 'the install ran out of memory'; exit 4; };";

/// Reads the container's OOM counter: cgroup v2 `memory.events`, else v1 `memory.oom_control`.
const OOM_READ: &str = "cat /sys/fs/cgroup/memory.events 2>/dev/null \
     || cat /sys/fs/cgroup/memory/memory.oom_control 2>/dev/null; exit 0";

/// The `oom_kill` count of a cgroup v2 `memory.events` (or a v1 `memory.oom_control`, which
/// spells it the same way).
pub(crate) fn oom_kills(events: &str) -> u64 {
    events
        .lines()
        .find_map(|l| l.strip_prefix("oom_kill "))
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(0)
}

/// Whether a check's run is the runtime's failure, not the check's: the runtime's own exec exit
/// codes (125 its own error, 126 not executable, 127 not found) together with its own first line.
/// A check that prints such a line and exits so can only turn its result into a `could_not_run`
/// denial, never into a pass.
pub(crate) fn launcher_failure(first_line: &str, exit: Option<i32>) -> Option<String> {
    let l = first_line.trim_end();
    (matches!(exit, Some(125..=127))
        && (l.starts_with("OCI runtime exec failed")
            || l.starts_with("Error response from daemon")))
    .then(|| format!("the floor container could not run the check: {l}"))
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

#[cfg(unix)]
fn dir_identity(p: &Path) -> Result<(u64, u64), String> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::symlink_metadata(p)
        .map_err(|e| format!("the base export `{}`: {e}", p.display()))?;
    Ok((m.dev(), m.ino()))
}

#[cfg(not(unix))]
fn dir_identity(p: &Path) -> Result<(u64, u64), String> {
    let m = std::fs::symlink_metadata(p)
        .map_err(|e| format!("the base export `{}`: {e}", p.display()))?;
    Ok((
        0,
        m.modified()
            .ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos() as u64),
    ))
}

/// A unique name for a container this process starts (process-wide, never reused).
fn unique_name(kind: &str) -> String {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!(
        "wfloor-{kind}-{}-{}-{nanos:08x}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// Run `cmd` in a throwaway, hardened, offline container of `image` (named, pid-labelled, removed
/// whatever happens): `Ok((exit code, stdout))` when the runtime ran it, `Err` when it did not.
fn probe_run(
    rt: &Runtime,
    image: &str,
    mounts: &[String],
    cmd: &[&str],
) -> Result<(i32, String), String> {
    let name = unique_name("probe");
    let pid_label = format!("{LABEL}.pid={}", std::process::id());
    let mut args: Vec<&str> = vec![
        "run",
        "--rm",
        "--name",
        &name,
        "--network",
        "none",
        "--read-only",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        "--user",
        CHECKS_USER,
        "--label",
        LABEL,
        "--label",
        &pid_label,
    ];
    for m in mounts {
        args.extend(["--mount", m.as_str()]);
    }
    args.push(image);
    args.extend(cmd);
    let mut c = rt.cmd();
    c.args(&args);
    let out = output_within(c, rt.bound(START));
    let _ = rt_run(rt, &["rm", "--force", &name], REAP);
    let o = out.ok_or_else(|| format!("a probe container did not answer within {START:?}"))?;
    match o.status.code() {
        // The runtime's own failure (the container never ran the command).
        Some(125) | None => Err(format!(
            "a probe container did not start: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Some(code) => Ok((code, String::from_utf8_lossy(&o.stdout).trim().to_string())),
    }
}

/// Whether `volume` holds `file` (a complete fill's marker), read through a throwaway container:
/// `Err` when the runtime could not say (never read as "missing").
fn volume_has(rt: &Runtime, image: &str, volume: &str, file: &str) -> Result<bool, String> {
    let mount = format!("type=volume,src={volume},dst={FILL_DIR},readonly");
    let path = format!("{FILL_DIR}/{file}");
    match probe_run(rt, image, &[mount], &["test", "-f", &path])? {
        (0, _) => Ok(true),
        (1, _) => Ok(false),
        (code, _) => Err(format!("reading {volume} exited {code}")),
    }
}

fn volume_exists(rt: &Runtime, volume: &str) -> bool {
    rt_run(
        rt,
        &["volume", "inspect", "--format", "{{.Name}}", volume],
        PROBE,
    )
    .is_ok()
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// ---------------------------------------------------------------------------------------------
// Maintenance: the sweep of containers a dead engine left, and volume eviction.
// ---------------------------------------------------------------------------------------------

/// At most once per [`MAINTAIN_EVERY`]: remove floor containers whose engine process is gone, and
/// hold the floor's volumes to the check cache's caps.
fn maintain(rt: &Runtime, cache: &Path, image: &str) {
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    {
        let Ok(mut last) = LAST.lock() else { return };
        if last.is_some_and(|t| t.elapsed() < MAINTAIN_EVERY) {
            return;
        }
        *last = Some(Instant::now());
    }
    sweep(rt);
    evict(rt, cache, image, crate::repo_checks::CacheCaps::from_env());
}

/// Remove floor containers (checks, install and probe containers alike) labelled with an engine
/// pid that is no longer alive.
fn sweep(rt: &Runtime) {
    let Ok(list) = rt_run(
        rt,
        &[
            "ps",
            "-a",
            "--filter",
            &format!("label={LABEL}.pid"),
            "--format",
            &format!("{{{{.Names}}}}\t{{{{.Label \"{LABEL}.pid\"}}}}"),
        ],
        PROBE,
    ) else {
        return;
    };
    for line in list.lines() {
        let mut f = line.split('\t');
        let (Some(name), Some(pid)) = (f.next(), f.next()) else {
            continue;
        };
        let Ok(pid) = pid.trim().parse::<i32>() else {
            continue;
        };
        if pid as u32 == std::process::id() || pid_alive(pid) {
            continue;
        }
        eprintln!("wicked-core: removing checks floor container {name} (its engine {pid} is gone)");
        let _ = rt_run(rt, &["rm", "--force", name], REAP);
    }
}

#[cfg(unix)]
fn pid_alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe {
        libc::kill(pid, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

#[cfg(not(unix))]
fn pid_alive(_pid: i32) -> bool {
    true
}

/// One floor volume the eviction pass may remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VolumeUse {
    pub(crate) name: String,
    pub(crate) repo: String,
    pub(crate) bytes: u64,
    pub(crate) used: SystemTime,
}

/// The volumes to remove, oldest use first: every one idle past the TTL; `node_modules` volumes
/// beyond the newest [`NM_KEEP`] of a repository; then by least recent use until the total fits
/// `total_cap` and each repository fits `repo_cap`. The shared cargo home is never on the list,
/// nor a `protected` volume (a live floor leases it): it still counts against the caps, so the
/// pass takes the next candidate instead.
pub(crate) fn eviction_plan(
    mut vols: Vec<VolumeUse>,
    now: SystemTime,
    ttl: Duration,
    total_cap: u64,
    repo_cap: u64,
    protected: &std::collections::HashSet<String>,
) -> Vec<String> {
    vols.retain(|v| v.name != CARGO_HOME_VOLUME);
    vols.sort_by_key(|v| std::cmp::Reverse(v.used));
    let mut out: Vec<String> = Vec::new();
    let mut nm_seen: std::collections::HashMap<String, usize> = Default::default();
    for v in &vols {
        let idle = now.duration_since(v.used).unwrap_or_default() > ttl;
        let over_keep = v.name.starts_with("wfloor-nm-") && {
            let n = nm_seen.entry(v.repo.clone()).or_default();
            *n += 1;
            *n > NM_KEEP
        };
        if (idle || over_keep) && !protected.contains(&v.name) {
            out.push(v.name.clone());
        }
    }
    let kept: Vec<&VolumeUse> = vols.iter().filter(|v| !out.contains(&v.name)).collect();
    let mut total: u64 = kept.iter().map(|v| v.bytes).sum();
    let mut per_repo: std::collections::HashMap<&str, u64> = Default::default();
    for v in &kept {
        *per_repo.entry(v.repo.as_str()).or_default() += v.bytes;
    }
    for v in kept.iter().rev() {
        let repo_over = per_repo.get(v.repo.as_str()).copied().unwrap_or(0) > repo_cap;
        if (total <= total_cap && !repo_over) || protected.contains(&v.name) {
            continue;
        }
        out.push(v.name.clone());
        total = total.saturating_sub(v.bytes);
        if let Some(r) = per_repo.get_mut(v.repo.as_str()) {
            *r = r.saturating_sub(v.bytes);
        }
    }
    out
}

/// Hold the floor's volumes to the caps, measured in the runtime VM ([`eviction_plan`]). Each
/// removal is taken under the volume's lock and skipped while any live floor leases it.
fn evict(rt: &Runtime, cache: &Path, image: &str, caps: crate::repo_checks::CacheCaps) {
    let dir = state_dir(cache);
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut stamps: Vec<(String, String, SystemTime)> = Vec::new();
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        let Some(vol) = n.strip_suffix(".used") else {
            continue;
        };
        let used = e
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let repo = std::fs::read_to_string(e.path()).unwrap_or_default();
        stamps.push((vol.to_string(), repo, used));
    }
    if stamps.is_empty() {
        return;
    }
    let sizes = volume_sizes(rt);
    let vols: Vec<VolumeUse> = stamps
        .iter()
        .filter(|(n, ..)| sizes.contains_key(n))
        .map(|(n, r, u)| VolumeUse {
            name: n.clone(),
            repo: r.clone(),
            bytes: sizes[n],
            used: *u,
        })
        .collect();
    // The VM's own disk, read inside a throwaway container (its root is on the VM's disk).
    let space = probe_run(rt, image, &[], &["df", "-B1", "--output=size,avail", "/"])
        .ok()
        .filter(|(code, _)| *code == 0)
        .and_then(|(_, o)| {
            let l = o.lines().nth(1)?;
            let mut f = l.split_whitespace();
            Some(crate::repo_checks::VolumeSpace {
                total: f.next()?.parse().ok()?,
                free: f.next()?.parse().ok()?,
            })
        });
    let bytes: u64 = vols.iter().map(|v| v.bytes).sum();
    let total_cap = crate::repo_checks::effective_total_cap(caps, bytes, space);
    let protected: std::collections::HashSet<String> = vols
        .iter()
        .filter(|v| leased(&v.name))
        .map(|v| v.name.clone())
        .collect();
    let plan = eviction_plan(
        vols,
        SystemTime::now(),
        crate::repo_checks::CHECK_CACHE_IDLE_TTL,
        total_cap,
        caps.repo,
        &protected,
    );
    for v in plan {
        let Ok(_lock) = lock_volume(&v) else { continue };
        if leased(&v) {
            continue;
        }
        if rt_run(rt, &["volume", "rm", &v], REAP).is_ok() {
            eprintln!("wicked-core: checks floor — removed cache volume {v} (cap / idle)");
            let _ = std::fs::remove_file(dir.join(format!("{v}.used")));
        }
    }
    // Stamps of volumes the runtime no longer has, a day after their last use (a stamp can be
    // written before its volume is made).
    let day = Duration::from_secs(24 * 60 * 60);
    for (n, _, used) in stamps {
        let old = SystemTime::now().duration_since(used).unwrap_or_default() > day;
        if old && !sizes.contains_key(&n) && !volume_exists(rt, &n) {
            let _ = std::fs::remove_file(dir.join(format!("{n}.used")));
        }
    }
}

/// The floor volumes' sizes, from `system df -v`.
fn volume_sizes(rt: &Runtime) -> std::collections::HashMap<String, u64> {
    let mut out = std::collections::HashMap::new();
    let Ok(raw) = rt_run(
        rt,
        &["system", "df", "-v", "--format", "{{json .Volumes}}"],
        START,
    ) else {
        return out;
    };
    let Ok(list) = serde_json::from_str::<Vec<serde_json::Value>>(&raw) else {
        return out;
    };
    for v in list {
        let Some(name) = v.get("Name").and_then(|n| n.as_str()) else {
            continue;
        };
        if !name.starts_with("wfloor-") {
            continue;
        }
        let size = v
            .get("Size")
            .and_then(|s| s.as_str())
            .map(parse_size)
            .unwrap_or(0);
        out.insert(name.to_string(), size);
    }
    out
}

/// A runtime's human size (`13.2GB`, `336MB`, `5.4GiB`, `0B`) in bytes.
pub(crate) fn parse_size(s: &str) -> u64 {
    let s = s.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (n, unit) = s.split_at(split);
    let n: f64 = n.parse().unwrap_or(0.0);
    let mul = match unit.trim().to_ascii_uppercase().as_str() {
        "B" | "" => 1.0,
        "KB" | "K" => 1e3,
        "MB" | "M" => 1e6,
        "GB" | "G" => 1e9,
        "TB" | "T" => 1e12,
        "KIB" => 1024.0,
        "MIB" => 1024.0 * 1024.0,
        "GIB" => 1024.0 * 1024.0 * 1024.0,
        "TIB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => 1.0,
    };
    (n * mul) as u64
}

/// Remove the cargo target volumes of a check-scratch leaf that is being removed (the leaf
/// removal [`crate::repo_checks`] makes on a run's end, a guarded checkout dropped, or the cap
/// pass). Only when a container floor ever stamped them, so a host with no container floor never
/// probes a runtime here. A volume a live floor leases (or still mounts) is kept, its stamp left
/// for the idle pass.
pub(crate) fn remove_leaf_volumes(leaf: &Path) {
    let Some(cache) = leaf.parent().and_then(Path::parent) else {
        return;
    };
    let dir = state_dir(cache);
    let names: Vec<String> = [Tree::Head, Tree::Base]
        .into_iter()
        .map(|t| target_volume(leaf, t))
        .filter(|v| dir.join(format!("{v}.used")).exists())
        .collect();
    if names.is_empty() {
        return;
    }
    let Ok(rt) = crate::container_floor::runtime() else {
        return;
    };
    for v in names {
        let Ok(_lock) = lock_volume(&v) else { continue };
        if leased(&v) {
            continue;
        }
        if rt_run(&rt, &["volume", "rm", "--force", &v], REAP).is_ok() {
            let _ = std::fs::remove_file(dir.join(format!("{v}.used")));
        }
    }
}

/// `wicked-core floor run --repo <worktree> --base <commit> [--stage creator|verify] [--run <id>]
/// [--full]`: run the repository-checks floor once over a checkout, with whichever runner
/// [`RUNNER_ENV`] picks, and print the report as JSON (stdout). For an operator or a timing run;
/// a governed run's floor is the engine's own. Exit 0 when the floor passed, 1 when it did not,
/// 2 on a usage error. A run id it makes up itself (no `--run`) has its check scratch and target
/// volumes removed at the end.
pub fn run_cli(args: &[String]) -> i32 {
    let arg = |k: &str| {
        args.iter()
            .position(|a| a == k)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let (Some(repo), Some(base)) = (arg("--repo"), arg("--base")) else {
        eprintln!(
            "usage: wicked-core floor run --repo <worktree> --base <commit> \
             [--stage creator|verify] [--run <id>] [--full]"
        );
        return 2;
    };
    let worktree = PathBuf::from(&repo);
    let git_dir =
        match crate::worktree_guard::git(&worktree, &["rev-parse", "--absolute-git-dir"], &[]) {
            Ok(o) => PathBuf::from(String::from_utf8_lossy(&o).trim()),
            Err(e) => {
                eprintln!("{e}");
                return 2;
            }
        };
    let stage = match arg("--stage").as_deref() {
        None | Some("creator") => crate::repo_checks::FloorStage::Creator,
        Some("verify") => crate::repo_checks::FloorStage::Verify,
        Some(o) => {
            eprintln!("--stage {o}: creator or verify");
            return 2;
        }
    };
    let made_up = arg("--run").is_none();
    let run_id = arg("--run").unwrap_or_else(|| format!("floor-run-{}", std::process::id()));
    let ctx = crate::repo_checks::FloorContext {
        stage,
        base_head: Some(base),
        git_dir: Some(git_dir),
        run_id: Some(run_id.clone()),
        ..Default::default()
    };
    // `--full`: the full suite, as a power run at high rigor hands off (DES-POWER-MODE).
    let full = args.iter().any(|a| a == "--full");
    if full {
        crate::power::set_full_suite(&run_id, true);
    }
    let t = Instant::now();
    let report = crate::repo_checks::run_floor(&worktree, &ctx);
    eprintln!(
        "wicked-core floor run: {} in {:.1}s",
        if report.passed { "PASSED" } else { "FAILED" },
        t.elapsed().as_secs_f64()
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&report).unwrap_or_default()
    );
    if full {
        crate::power::set_full_suite(&run_id, false);
    }
    if made_up {
        crate::repo_checks::remove_check_scratch(&worktree, &run_id);
    }
    if report.passed {
        0
    } else {
        1
    }
}

#[cfg(test)]
pub(crate) mod test_seam {
    use super::Mode;
    use std::cell::Cell;
    thread_local! {
        static MODE: Cell<Option<Mode>> = const { Cell::new(None) };
    }
    pub(crate) fn mode() -> Option<Mode> {
        MODE.with(Cell::get)
    }
    #[allow(dead_code)]
    pub(crate) fn set_mode(m: Option<Mode>) {
        MODE.with(|c| c.set(m));
    }
}

#[cfg(test)]
#[path = "checks_container_tests.rs"]
mod tests;
