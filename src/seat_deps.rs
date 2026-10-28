//! SEAT DEPENDENCIES (core#1155): a JS run's worktree carries its dependencies before a seat works
//! in it, installed by the ENGINE for the platform the seat runs on.
//!
//! ## The defect this closes
//!
//! Since core#1144 the creator floor runs on a clean checkout of the seat's tree, so its install
//! lands in that checkout and never in the run's worktree. The seats work in the worktree, and
//! none of them could install there: a container seat's only egress is the gateway's CONNECT
//! allowlist (`WICKED_SEAT_EGRESS_HOSTS`, which names no npm registry on the rig, so `npm ci` saw
//! its TLS handshake cut), and a read-only seat may not install at all (governed-worker E2). On rig
//! run 115fabdb (wicked-crew) the evaluator's `vitest` / `tsc` / `eslint` exited 127, the isolated
//! QE reviewer recorded INCONCLUSIVE, and delivery was refused. Before core#1144 the creator floor
//! installed into the live worktree, for the HOST: a container seat then found a `node_modules`
//! whose native bindings were the host's (`rolldown native binding is missing`, run d3a380b0).
//!
//! ## What the engine does
//!
//! Before a seat's turn on a unit with a worktree ([`provision`]), when the worktree's root has a
//! `package.json` and a `package-lock.json`:
//!
//! * it runs `npm ci --ignore-scripts --no-audit --no-fund` IN the worktree, under the floor's own
//!   OS write boundary and isolated homes ([`crate::repo_checks::floor_sandbox`],
//!   [`crate::repo_checks::CheckScratch`]): the same command, sandbox and minimal environment the
//!   floor's `install` check uses, on the host, which has the network;
//! * for a container seat ([`crate::container_floor::container_mode`]) it adds `--os=linux
//!   --cpu=<host arch> --libc=glibc`, so npm installs the optional native packages of the seat
//!   image (Debian, the host's architecture) instead of the host's;
//! * it writes a marker, `node_modules/.wicked-seat-deps.json`, naming that platform and the
//!   lockfile's digest. A later unit of the run finds the marker matching and installs nothing; a
//!   changed lockfile, another platform, a missing marker (a seat's own `npm ci` removes
//!   `node_modules` first) or a hollow tree installs again.
//!
//! Nothing is installed when a `node_modules/` (at the root or beside any tracked `package.json`)
//! is not git-ignored (it would ride into the change), when anything tracked lives under one (npm
//! ci removes them) or one is a link, when the repository uses pnpm or yarn (no platform override
//! here yet), when the unit carries no pinned git dir, or when the floor's sandbox cannot arm. Each
//! of these is said on the unit's stream, and the seat works as before. Every git command goes
//! through the run's PINNED git dir and runs no hook or worktree filter.
//!
//! The install must leave the reviewed tree as it found it. The tree is snapshotted (the worktree
//! guard's own snapshot, content and modes) before and after; the one change npm ci makes there,
//! the executable bit of a linked package's bin, is put back on the file. Anything else is
//! reported, never repaired by a checkout.
//!
//! The floor's own install is unchanged: it still installs in its own checkout. A floor that runs
//! in the LIVE worktree (a creator floor whose clean checkout could not be made) reads a marker
//! naming another platform as a hollow tree ([`foreign_platform_gap`]) and installs for the host.
//!
//! Ignored files are outside the worktree guard's snapshot by construction
//! ([`crate::worktree_guard`]), so the install is never a seat's mutation.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::repo_checks::{CheckScratch, ExecOpts, RepoCheck, Tree};

/// The label of the context block a seat is handed when its dependencies are in place.
pub(crate) const CONTEXT_LABEL: &str = "[engine — dependencies]";

/// The marker the engine writes inside `node_modules/` once the install for a platform succeeded.
pub(crate) const MARKER: &str = ".wicked-seat-deps.json";

/// The install's bound before the floor's load factor: a cold `npm ci` of wicked-crew takes about
/// 80 s on the rig.
const INSTALL_TIMEOUT_S: u64 = 900;

/// The npm platform a seat runs on: `os` / `cpu` as npm spells them, and `libc` on Linux.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SeatPlatform {
    pub(crate) os: String,
    pub(crate) cpu: String,
    pub(crate) libc: Option<String>,
    /// Whether this is not the host's own platform (npm needs the override flags).
    pub(crate) foreign: bool,
}

impl SeatPlatform {
    /// `linux-arm64-glibc`, `darwin-arm64`: the marker's and the stream's spelling.
    pub(crate) fn label(&self) -> String {
        match &self.libc {
            Some(l) => format!("{}-{}-{l}", self.os, self.cpu),
            None => format!("{}-{}", self.os, self.cpu),
        }
    }

    /// The host's own platform.
    pub(crate) fn host() -> Self {
        SeatPlatform {
            os: npm_os(std::env::consts::OS).to_string(),
            cpu: npm_cpu(std::env::consts::ARCH).to_string(),
            libc: host_libc().map(str::to_string),
            foreign: false,
        }
    }

    /// The seat image's platform: Debian (glibc) Linux on the host's architecture. Foreign
    /// unless the host is that platform already.
    pub(crate) fn container() -> Self {
        let mut c = SeatPlatform {
            os: "linux".into(),
            cpu: npm_cpu(std::env::consts::ARCH).to_string(),
            libc: Some("glibc".into()),
            foreign: true,
        };
        c.foreign = c.label() != Self::host().label();
        c
    }

    /// The platform this daemon's seats run on.
    pub(crate) fn for_seats() -> Self {
        if crate::container_floor::container_mode() {
            Self::container()
        } else {
            Self::host()
        }
    }

    fn npm_flags(&self) -> Vec<String> {
        if !self.foreign {
            return Vec::new();
        }
        let mut flags = vec![format!("--os={}", self.os), format!("--cpu={}", self.cpu)];
        if let Some(l) = &self.libc {
            flags.push(format!("--libc={l}"));
        }
        flags
    }
}

/// The host's C library on Linux, as npm's `libc` field spells it; `None` elsewhere.
fn host_libc() -> Option<&'static str> {
    if cfg!(all(target_os = "linux", target_env = "gnu")) {
        Some("glibc")
    } else if cfg!(all(target_os = "linux", target_env = "musl")) {
        Some("musl")
    } else {
        None
    }
}

fn npm_os(os: &str) -> &str {
    match os {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

fn npm_cpu(arch: &str) -> &str {
    match arch {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        "x86" => "ia32",
        other => other,
    }
}

/// What [`provision`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Provisioned {
    /// No `package.json` at the worktree root: nothing to install.
    NotNode,
    /// The marker matches this platform and lockfile, and every declared dependency is present.
    AlreadyFor(String),
    /// Installed now.
    Installed {
        platform: String,
        argv: Vec<String>,
        duration_ms: u64,
        /// Tracked bins whose mode npm changed and the engine put back (their `.bin` link will not
        /// run until the repository commits them executable).
        restored: Vec<String>,
    },
    /// Not installed, on purpose; the reason is said on the unit's stream.
    Skipped(String),
    /// The install ran and failed.
    Failed(String),
}

impl Provisioned {
    /// The line the unit's stream carries, or `None` when there is nothing to say.
    pub(crate) fn note(&self) -> Option<String> {
        match self {
            Provisioned::NotNode | Provisioned::AlreadyFor(_) => None,
            Provisioned::Installed {
                platform,
                argv,
                duration_ms,
                restored,
            } => Some(format!(
                "dependencies: the engine installed the worktree's node_modules for the seats \
                 ({platform}) with `{}` in {} s{}",
                argv.join(" "),
                duration_ms / 1000,
                if restored.is_empty() {
                    String::new()
                } else {
                    format!(
                        "; npm made {} tracked bin(s) executable and the engine put their mode \
                         back, so their node_modules/.bin link will not run: {}",
                        restored.len(),
                        restored.join(", ")
                    )
                }
            )),
            Provisioned::Skipped(why) => Some(format!(
                "dependencies: the engine did not install the worktree's node_modules ({why})"
            )),
            Provisioned::Failed(why) => Some(format!(
                "dependencies: the engine's install of the worktree's node_modules failed ({why}); \
                 the seat may not be able to run the repository's checks"
            )),
        }
    }

    /// The context the seat is handed, when its dependencies are in place.
    pub(crate) fn seat_context(&self) -> Option<String> {
        let platform = match self {
            Provisioned::AlreadyFor(p) => p,
            Provisioned::Installed { platform, .. } => platform,
            _ => return None,
        };
        Some(format!(
            "The worktree's node_modules is installed for this seat's platform ({platform}) by the \
             engine (npm ci --ignore-scripts). Run the repository's scripts and tests directly. Do \
             not run `npm ci` or `npm install`: npm ci deletes node_modules first, and the seat may \
             have no route to the package registry."
        ))
    }
}

#[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq, Debug)]
struct Marker {
    platform: String,
    lock: String,
}

/// The marker's platform label, when `node_modules/` carries one the engine wrote.
pub(crate) fn marker_platform(worktree: &Path) -> Option<String> {
    read_marker(worktree).map(|m| m.platform)
}

fn read_marker(worktree: &Path) -> Option<Marker> {
    let path = worktree.join("node_modules").join(MARKER);
    let meta = std::fs::symlink_metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > 4096 {
        return None;
    }
    serde_json::from_slice(&std::fs::read(&path).ok()?).ok()
}

/// (for the floor) Why a live worktree's `node_modules` does not provision checks that run on
/// THIS host: the engine installed it for the seats' platform, not the host's.
pub(crate) fn foreign_platform_gap(worktree: &Path) -> Option<String> {
    let installed = marker_platform(worktree)?;
    let host = SeatPlatform::host().label();
    (installed != host).then(|| {
        format!(
            "node_modules was installed for the seats' platform ({installed}), not this host's \
             ({host})"
        )
    })
}

fn lock_digest(worktree: &Path) -> Result<String, String> {
    let bytes = std::fs::read(worktree.join("package-lock.json"))
        .map_err(|e| format!("package-lock.json could not be read: {e}"))?;
    Ok(crate::repo_checks::hex12(&Sha256::digest(&bytes)))
}

fn is_link(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink())
}

/// Git through the run's PINNED git dir (`GIT_DIR` / `GIT_WORK_TREE`), never the worktree's own
/// `.git` file, which a seat can rewrite: the same plumbing the worktree guard uses. Only
/// commands that run no hook and no worktree filter are issued here (`ls-files`, `check-ignore`,
/// `diff-tree` over two tree objects); the snapshots go through the guard itself.
fn pinned_git(worktree: &Path, git_dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let env: [(&str, &Path); 2] = [("GIT_DIR", git_dir), ("GIT_WORK_TREE", worktree)];
    crate::worktree_guard::git(worktree, args, &env).map_err(|e| e.to_string())
}

/// The preflight: `node_modules/` at the root and beside every tracked `package.json` is
/// git-ignored, and nothing tracked lives under any `node_modules` (npm ci removes those
/// directories, workspace members' included). Any git error refuses (fail toward not installing).
fn install_may_touch_only_ignored(worktree: &Path, git_dir: &Path) -> Result<(), String> {
    let listed = pinned_git(worktree, git_dir, &["ls-files", "-z"])
        .map_err(|e| format!("git could not list the tracked files: {e}"))?;
    let mut package_dirs: Vec<String> = vec![String::new()];
    for path in listed.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        let path = String::from_utf8_lossy(path);
        if path.split('/').any(|seg| seg == "node_modules") {
            return Err(format!(
                "the repository tracks `{path}` under a node_modules directory, which npm ci \
                 would remove"
            ));
        }
        if let Some(dir) = path.strip_suffix("/package.json") {
            package_dirs.push(format!("{dir}/"));
        }
    }
    for dir in package_dirs {
        let nm = format!("{dir}node_modules/");
        if is_link(&worktree.join(&nm)) {
            return Err(format!("`{nm}` is a link (never followed)"));
        }
        // `check-ignore -q` exits 0 when ignored, 1 when not.
        if pinned_git(worktree, git_dir, &["check-ignore", "-q", "--", &nm]).is_err() {
            return Err(format!(
                "`{nm}` is not git-ignored, so an install would ride into the change"
            ));
        }
    }
    Ok(())
}

/// One tracked entry's change between two snapshot trees: `(old mode, new mode, old blob, new
/// blob, path)`, from `git diff-tree -r -z` over the two tree objects (no worktree read).
type TreeChange = (String, String, String, String, String);

fn tree_changes(
    worktree: &Path,
    git_dir: &Path,
    before: &str,
    after: &str,
) -> Result<Vec<TreeChange>, String> {
    let raw = pinned_git(
        worktree,
        git_dir,
        &["diff-tree", "-r", "-z", "--no-renames", before, after],
    )?;
    let mut out = Vec::new();
    let mut parts = raw.split(|b| *b == 0).filter(|p| !p.is_empty());
    while let (Some(meta), Some(path)) = (parts.next(), parts.next()) {
        // `:<old mode> <new mode> <old sha> <new sha> <status>`
        let meta = String::from_utf8_lossy(meta);
        let f: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        if f.len() < 4 {
            return Err(format!("git diff-tree printed an unexpected line `{meta}`"));
        }
        out.push((
            f[0].to_string(),
            f[1].to_string(),
            f[2].to_string(),
            f[3].to_string(),
            String::from_utf8_lossy(path).into_owned(),
        ));
    }
    Ok(out)
}

/// Put back what the install changed in the reviewed tree. The ONE change npm ci makes there is
/// the executable bit of a linked package's bin (a workspace member, a `file:` dependency): a
/// mode-only change, restored here on the file itself (never through git, whose checkout runs
/// hooks and filters). The comparison is between two snapshots of the tree's real content and
/// modes, so a file a seat had already edited is judged by what the install did to it. Anything
/// else the install changed is reported, not repaired. Returns the restored paths.
fn restore_modes(
    worktree: &Path,
    git_dir: &Path,
    before: &str,
    after: &str,
) -> Result<Vec<String>, String> {
    let mut restored = Vec::new();
    let mut left = Vec::new();
    for (old_mode, new_mode, old_blob, new_blob, path) in
        tree_changes(worktree, git_dir, before, after)?
    {
        let mode_only = old_blob == new_blob
            && matches!(
                (old_mode.as_str(), new_mode.as_str()),
                ("100644", "100755") | ("100755", "100644")
            );
        let fixed = mode_only
            && crate::repo_checks::confined(worktree, &path)
                .ok()
                .and_then(|p| {
                    let meta = std::fs::symlink_metadata(&p).ok()?;
                    meta.is_file().then_some((p, meta))
                })
                .is_some_and(|(p, meta)| set_exec(&p, &meta, old_mode == "100755"));
        if fixed {
            restored.push(path);
        } else {
            left.push(path);
        }
    }
    if left.is_empty() {
        Ok(restored)
    } else {
        Err(format!(
            "the install changed the reviewed tree beyond a bin's mode, and it was left as it is: {}",
            left.join(", ")
        ))
    }
}

/// Set or clear a file's executable bits (a mode-only restore); `false` where modes do not exist.
#[cfg(unix)]
fn set_exec(p: &Path, meta: &std::fs::Metadata, exec: bool) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let mode = meta.permissions().mode();
    let mode = if exec { mode | 0o111 } else { mode & !0o111 };
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).is_ok()
}

#[cfg(not(unix))]
fn set_exec(_p: &Path, _meta: &std::fs::Metadata, _exec: bool) -> bool {
    false
}

/// The install argv for `platform`.
pub(crate) fn install_argv(platform: &SeatPlatform) -> Vec<String> {
    let mut argv: Vec<String> = ["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    argv.extend(platform.npm_flags());
    argv
}

/// Make `worktree`'s dependencies present for `platform` (see the module doc). `git_dir` is the
/// run's pinned git dir (the unit's worktree baseline carries it); `None` installs nothing. Runs
/// the install on the calling thread; call it off the actor.
pub(crate) fn provision(
    worktree: &Path,
    git_dir: Option<&Path>,
    run_id: &str,
    platform: &SeatPlatform,
) -> Provisioned {
    match std::fs::symlink_metadata(worktree.join("package.json")) {
        Ok(m) if m.is_file() => {}
        _ => return Provisioned::NotNode,
    }
    if worktree.join("pnpm-lock.yaml").exists() || worktree.join("yarn.lock").exists() {
        return Provisioned::Skipped(
            "the repository uses pnpm or yarn; the engine installs npm lockfiles only".into(),
        );
    }
    if !std::fs::symlink_metadata(worktree.join("package-lock.json")).is_ok_and(|m| m.is_file()) {
        return Provisioned::Skipped("no package-lock.json to install from".into());
    }
    let nm = worktree.join("node_modules");
    if is_link(&nm) {
        return Provisioned::Skipped("node_modules is a link (never followed)".into());
    }
    let lock = match lock_digest(worktree) {
        Ok(d) => d,
        Err(e) => return Provisioned::Skipped(e),
    };
    let label = platform.label();
    let want = Marker {
        platform: label.clone(),
        lock,
    };
    if read_marker(worktree).as_ref() == Some(&want)
        && crate::repo_checks::declared_deps_present(worktree).unwrap_or(false)
    {
        return Provisioned::AlreadyFor(label);
    }
    let Some(git_dir) = git_dir else {
        return Provisioned::Skipped(
            "the unit carries no pinned git dir to check the install against".into(),
        );
    };
    if let Err(why) = install_may_touch_only_ignored(worktree, git_dir) {
        return Provisioned::Skipped(why);
    }
    let scratch = match CheckScratch::prepare_for(worktree, Some(run_id)) {
        Ok(s) => s,
        Err(e) => return Provisioned::Skipped(format!("the install scratch: {e}")),
    };
    let sandbox = crate::repo_checks::floor_sandbox(&[
        worktree.to_path_buf(),
        scratch.tmp_path().to_path_buf(),
        scratch.root().to_path_buf(),
    ]);
    if sandbox.level != crate::validator::SandboxLevel::Sandboxed {
        return Provisioned::Skipped(format!(
            "the floor's sandbox does not arm on this host ({})",
            sandbox
                .downgrade_reason
                .as_deref()
                .unwrap_or("no boundary tool")
        ));
    }
    let before = match crate::worktree_guard::snapshot_through(worktree, git_dir) {
        Ok(s) => s.tree,
        Err(e) => return Provisioned::Skipped(format!("the tree could not be snapshotted: {e}")),
    };
    // The marker goes before the install (npm ci removes node_modules) and is written only after
    // it succeeded, so a failed install never leaves one.
    let marker_path = nm.join(MARKER);
    let _ = std::fs::remove_file(&marker_path);
    let check = RepoCheck {
        name: "seat-deps".into(),
        argv: install_argv(platform),
        source: format!("package-lock.json (seat dependencies, {label})"),
        timeout_s: Some(INSTALL_TIMEOUT_S),
        dir: String::new(),
    };
    let run = crate::repo_checks::run_one_with(
        worktree,
        None,
        &check,
        &ExecOpts::default(),
        &sandbox,
        &scratch,
        Tree::Head,
    );
    // The install must leave the reviewed tree as it found it, whether or not it succeeded.
    let restored = match crate::worktree_guard::snapshot_through(worktree, git_dir) {
        Ok(after) if after.tree == before => Ok(Vec::new()),
        Ok(after) => restore_modes(worktree, git_dir, &before, &after.tree),
        Err(e) => Err(format!(
            "the tree could not be snapshotted after the install: {e}"
        )),
    };
    if run.exit_code != Some(0) {
        let why = if let Some(e) = &run.spawn_error {
            e.clone()
        } else if run.timed_out {
            format!("timed out after {} s", run.bound_s)
        } else {
            let tail = if run.stderr_tail.trim().is_empty() {
                &run.stdout_tail
            } else {
                &run.stderr_tail
            };
            let last: Vec<&str> = tail.lines().rev().take(3).collect();
            format!(
                "exit {}: {}",
                run.exit_code.map_or("none".into(), |c| c.to_string()),
                last.into_iter().rev().collect::<Vec<_>>().join(" / ")
            )
        };
        return Provisioned::Failed(why);
    }
    let restored = match restored {
        Ok(r) => r,
        Err(why) => return Provisioned::Failed(why),
    };
    // A lockfile with no dependencies installs no node_modules at all.
    if let Err(e) = std::fs::create_dir_all(&nm) {
        return Provisioned::Failed(format!("node_modules could not be created: {e}"));
    }
    let body = serde_json::to_vec(&want).unwrap_or_default();
    if let Err(e) = write_new(&marker_path, &body) {
        return Provisioned::Failed(format!("the marker could not be written: {e}"));
    }
    Provisioned::Installed {
        platform: label,
        argv: check.argv,
        duration_ms: run.duration_ms,
        restored,
    }
}

fn write_new(path: &PathBuf, body: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    f.write_all(body)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        // spawn-audit: test-only — the fixture's git.
        let ok = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    #[cfg(unix)]
    fn chmod(p: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(unix)]
    fn mode_of(p: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    /// The fixture's pinned git dir (a plain checkout: its own `.git`).
    pub(crate) fn git_dir_of(wt: &Path) -> PathBuf {
        wt.join(".git")
    }

    /// A crew-shaped repository: a root `package.json` with scripts and a dev dependency, an npm
    /// lockfile, `node_modules/` git-ignored. No network: the lockfile pins a local `file:`
    /// dependency, so `npm ci` installs from the tree itself. The dependency's bin is committed
    /// `bin_mode`.
    pub(crate) fn crew_shaped_with(name: &str, bin_mode: u32) -> (PathBuf, PathBuf) {
        let tmp = crate::repo_checks::tests::scratch(name);
        let wt = tmp.join("wt");
        std::fs::create_dir_all(wt.join("vendor/fake-vitest/bin")).unwrap();
        std::fs::write(
            wt.join("vendor/fake-vitest/package.json"),
            r#"{"name":"fake-vitest","version":"1.0.0","bin":{"vitest":"bin/vitest.js"}}"#,
        )
        .unwrap();
        let bin = wt.join("vendor/fake-vitest/bin/vitest.js");
        std::fs::write(&bin, "#!/usr/bin/env node\nconsole.log('ran');\n").unwrap();
        #[cfg(unix)]
        chmod(&bin, bin_mode);
        #[cfg(not(unix))]
        let _ = bin_mode;
        std::fs::write(
            wt.join("package.json"),
            r#"{"name":"crew-shaped","version":"0.0.0","private":true,
"scripts":{"typecheck":"tsc --noEmit","lint":"eslint .","test":"vitest run"},
"devDependencies":{"fake-vitest":"file:vendor/fake-vitest"}}"#,
        )
        .unwrap();
        std::fs::write(wt.join(".gitignore"), "node_modules/\ntmp/\n").unwrap();
        git(&wt, &["init", "-q"]);
        // The lockfile npm itself writes for this manifest (no registry entry in it).
        // spawn-audit: test-only — npm writes the fixture's lockfile.
        let st = Command::new("npm")
            .args([
                "install",
                "--package-lock-only",
                "--ignore-scripts",
                "--no-audit",
                "--no-fund",
                "--offline",
            ])
            .current_dir(&wt)
            .env("npm_config_cache", tmp.join("npm-cache"))
            .output()
            .unwrap();
        assert!(
            st.status.success(),
            "{}",
            String::from_utf8_lossy(&st.stderr)
        );
        git(&wt, &["add", "-A"]);
        git(&wt, &["commit", "-qm", "base"]);
        (tmp, wt)
    }

    pub(crate) fn crew_shaped(name: &str) -> (PathBuf, PathBuf) {
        crew_shaped_with(name, 0o755)
    }

    pub(crate) fn npm_available() -> bool {
        // spawn-audit: test-only — is npm on this host.
        Command::new("npm")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    pub(crate) fn sandbox_arms() -> bool {
        crate::repo_checks::floor_probe().is_ok()
    }

    fn installs_here() -> bool {
        let ok = npm_available() && sandbox_arms();
        if !ok {
            eprintln!("skipped: npm or the floor sandbox is not available on this host");
        }
        ok
    }

    fn tree_of(wt: &Path) -> String {
        crate::worktree_guard::snapshot_through(wt, &git_dir_of(wt))
            .unwrap()
            .tree
    }

    #[test]
    fn the_platform_labels_and_flags_follow_npm_spelling() {
        let c = SeatPlatform::container();
        assert_eq!(c.os, "linux");
        assert_eq!(c.libc.as_deref(), Some("glibc"));
        assert!(c.label().starts_with("linux-") && c.label().ends_with("-glibc"));
        let h = SeatPlatform::host();
        // A container seat on another platform installs with the override; on the seat image's
        // own platform it needs none.
        assert_eq!(c.foreign, c.label() != h.label());
        let argv = install_argv(&c);
        assert_eq!(
            &argv[..5],
            ["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund"]
        );
        if c.foreign {
            assert!(argv.contains(&"--os=linux".to_string()), "{argv:?}");
            assert!(argv.contains(&format!("--cpu={}", c.cpu)), "{argv:?}");
            assert!(argv.contains(&"--libc=glibc".to_string()), "{argv:?}");
        }
        // The host installs with the floor's own argv, no override.
        assert_eq!(
            install_argv(&h),
            ["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund"]
        );
        assert_eq!(npm_cpu("aarch64"), "arm64");
        assert_eq!(npm_cpu("x86_64"), "x64");
        assert_eq!(npm_os("macos"), "darwin");
        let forced = SeatPlatform {
            os: "linux".into(),
            cpu: "arm64".into(),
            libc: Some("glibc".into()),
            foreign: true,
        };
        assert_eq!(
            install_argv(&forced)[5..],
            ["--os=linux", "--cpu=arm64", "--libc=glibc"]
        );
    }

    #[test]
    fn a_tree_without_a_manifest_is_not_node_and_pnpm_is_said() {
        let tmp = crate::repo_checks::tests::scratch("seat-deps-none");
        assert_eq!(
            provision(&tmp, None, "r", &SeatPlatform::host()),
            Provisioned::NotNode
        );
        std::fs::write(tmp.join("package.json"), "{}").unwrap();
        std::fs::write(tmp.join("pnpm-lock.yaml"), "").unwrap();
        assert!(matches!(
            provision(&tmp, None, "r", &SeatPlatform::host()),
            Provisioned::Skipped(w) if w.contains("pnpm")
        ));
    }

    /// core#1155 DONE WHEN 1: a crew-shaped worktree has its dependencies after provisioning,
    /// for the seats' platform; the installed command RUNS, the marker names the platform, a
    /// second unit installs nothing, and the worktree guard does not see the install.
    #[test]
    fn a_crew_shaped_worktree_is_provisioned_once_for_the_seat_platform() {
        if !installs_here() {
            return;
        }
        let (_tmp, wt) = crew_shaped("seat-deps-crew");
        let gd = git_dir_of(&wt);
        let before = tree_of(&wt);
        // The host's own platform, so the installed command can run here.
        let platform = SeatPlatform::host();
        let got = provision(&wt, Some(&gd), "run-1155", &platform);
        let Provisioned::Installed {
            platform: p,
            restored,
            ..
        } = &got
        else {
            panic!("expected an install, got {got:?}");
        };
        assert_eq!(p, &platform.label());
        assert!(restored.is_empty(), "{restored:?}");
        assert!(wt.join("node_modules/fake-vitest/package.json").is_file());
        // spawn-audit: test-only — the installed bin runs.
        let ran = Command::new(wt.join("node_modules/.bin/vitest"))
            .current_dir(&wt)
            .output()
            .unwrap();
        assert!(ran.status.success(), "{ran:?}");
        assert_eq!(String::from_utf8_lossy(&ran.stdout).trim(), "ran");
        assert_eq!(
            marker_platform(&wt).as_deref(),
            Some(platform.label().as_str())
        );
        assert!(got.seat_context().unwrap().contains("Do not run `npm ci`"));
        assert_eq!(
            before,
            tree_of(&wt),
            "the install left the reviewed tree unchanged"
        );
        // A later unit of the run installs nothing.
        assert_eq!(
            provision(&wt, Some(&gd), "run-1155", &platform),
            Provisioned::AlreadyFor(platform.label())
        );
        assert!(
            foreign_platform_gap(&wt).is_none(),
            "the host's own install"
        );
        // A seat's own `npm ci` removed node_modules: the next unit installs again.
        std::fs::remove_dir_all(wt.join("node_modules")).unwrap();
        assert!(matches!(
            provision(&wt, Some(&gd), "run-1155", &platform),
            Provisioned::Installed { .. }
        ));
        // A changed lockfile installs again too.
        let lock = std::fs::read_to_string(wt.join("package-lock.json")).unwrap();
        std::fs::write(wt.join("package-lock.json"), format!("{lock}\n")).unwrap();
        assert!(matches!(
            provision(&wt, Some(&gd), "run-1155", &platform),
            Provisioned::Installed { .. }
        ));
        // Another platform's install: the marker says so, and the host floor sees a gap.
        let foreign = SeatPlatform {
            os: "linux".into(),
            cpu: "arm64".into(),
            libc: Some("glibc".into()),
            foreign: true,
        };
        if foreign.label() != platform.label() {
            assert!(matches!(
                provision(&wt, Some(&gd), "run-1155", &foreign),
                Provisioned::Installed { .. }
            ));
            assert!(foreign_platform_gap(&wt).is_some());
        }
        // No pinned git dir: nothing is installed.
        std::fs::remove_dir_all(wt.join("node_modules")).unwrap();
        assert!(matches!(
            provision(&wt, None, "run-1155", &platform),
            Provisioned::Skipped(w) if w.contains("pinned git dir")
        ));
    }

    /// (xreview on #1158) npm makes a linked package's tracked bin executable. The engine puts
    /// the mode back on the file (no checkout, no hook) and says so, judged by the tree's real
    /// content and modes: a bin a seat had already edited keeps the seat's edit.
    #[cfg(unix)]
    #[test]
    fn a_linked_bins_mode_is_put_back_even_on_a_file_a_seat_had_edited() {
        if !installs_here() {
            return;
        }
        let (_tmp, wt) = crew_shaped_with("seat-deps-mode", 0o644);
        let gd = git_dir_of(&wt);
        let bin = wt.join("vendor/fake-vitest/bin/vitest.js");
        // A seat's uncommitted edit to the bin, before the install.
        std::fs::write(&bin, "#!/usr/bin/env node\nconsole.log('edited');\n").unwrap();
        let before = tree_of(&wt);
        let got = provision(&wt, Some(&gd), "run-mode", &SeatPlatform::host());
        let Provisioned::Installed { restored, .. } = &got else {
            panic!("expected an install, got {got:?}");
        };
        assert_eq!(
            restored,
            &vec!["vendor/fake-vitest/bin/vitest.js".to_string()]
        );
        assert_eq!(mode_of(&bin) & 0o111, 0, "the mode is back");
        assert!(std::fs::read_to_string(&bin).unwrap().contains("edited"));
        assert_eq!(
            before,
            tree_of(&wt),
            "the reviewed tree is as the seat left it"
        );
        assert!(got.note().unwrap().contains("put their mode back"));
    }

    #[test]
    fn a_node_modules_that_is_not_ignored_or_holds_tracked_files_is_never_installed() {
        if !npm_available() {
            return;
        }
        let (_tmp, wt) = crew_shaped("seat-deps-unignored");
        let gd = git_dir_of(&wt);
        std::fs::write(wt.join(".gitignore"), "tmp/\n").unwrap();
        let got = provision(&wt, Some(&gd), "r", &SeatPlatform::host());
        assert!(
            matches!(&got, Provisioned::Skipped(w) if w.contains("not git-ignored")),
            "{got:?}"
        );
        assert!(!wt.join("node_modules").exists());
        // A workspace member's node_modules: ignored at the root only, not beside the member.
        std::fs::write(wt.join(".gitignore"), "/node_modules/\ntmp/\n").unwrap();
        std::fs::create_dir_all(wt.join("packages/member")).unwrap();
        std::fs::write(wt.join("packages/member/package.json"), "{}").unwrap();
        git(&wt, &["add", "-A"]);
        git(&wt, &["commit", "-qm", "member"]);
        let got = provision(&wt, Some(&gd), "r", &SeatPlatform::host());
        assert!(
            matches!(&got, Provisioned::Skipped(w) if w.contains("packages/member/node_modules/")),
            "{got:?}"
        );
        // A tracked file under a member's node_modules: npm ci would remove it.
        std::fs::write(wt.join(".gitignore"), "node_modules/\ntmp/\n").unwrap();
        std::fs::create_dir_all(wt.join("packages/member/node_modules/vendored")).unwrap();
        std::fs::write(
            wt.join("packages/member/node_modules/vendored/keep.txt"),
            "x",
        )
        .unwrap();
        git(&wt, &["add", "-A"]);
        git(
            &wt,
            &[
                "add",
                "-f",
                "packages/member/node_modules/vendored/keep.txt",
            ],
        );
        git(&wt, &["commit", "-qm", "vendored"]);
        let got = provision(&wt, Some(&gd), "r", &SeatPlatform::host());
        assert!(
            matches!(&got, Provisioned::Skipped(w) if w.contains("keep.txt")),
            "{got:?}"
        );
        assert!(wt
            .join("packages/member/node_modules/vendored/keep.txt")
            .is_file());
    }

    /// (xreview on #1158) A lockfile with no dependencies installs no node_modules: the marker
    /// still lands, so the next unit does not install again.
    #[test]
    fn a_lockfile_with_no_dependencies_still_gets_its_marker() {
        if !installs_here() {
            return;
        }
        let tmp = crate::repo_checks::tests::scratch("seat-deps-empty");
        let wt = tmp.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(
            wt.join("package.json"),
            r#"{"name":"empty","version":"0.0.0","private":true,"scripts":{"test":"node --test"}}"#,
        )
        .unwrap();
        std::fs::write(
            wt.join("package-lock.json"),
            r#"{"name":"empty","version":"0.0.0","lockfileVersion":3,"requires":true,"packages":{"":{"name":"empty","version":"0.0.0"}}}"#,
        )
        .unwrap();
        std::fs::write(wt.join(".gitignore"), "node_modules/\n").unwrap();
        git(&wt, &["init", "-q"]);
        git(&wt, &["add", "-A"]);
        git(&wt, &["commit", "-qm", "base"]);
        let gd = git_dir_of(&wt);
        let got = provision(&wt, Some(&gd), "r", &SeatPlatform::host());
        assert!(matches!(got, Provisioned::Installed { .. }), "{got:?}");
        assert!(matches!(
            provision(&wt, Some(&gd), "r", &SeatPlatform::host()),
            Provisioned::AlreadyFor(_)
        ));
    }
}
