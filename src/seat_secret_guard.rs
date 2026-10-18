//! SEAT-SECRET GUARD (core#1024 follow-up): a credential the platform put into a seat never leaves
//! with the work.
//!
//! ## The exposure
//!
//! core#1024 moved the claude seat to pass-through auth: the seat's own claude sends the operator's
//! long-lived `claude setup-token` token, which reaches the seat container as
//! `CLAUDE_CODE_OAUTH_TOKEN`. That is inherent to pass-through: the unit's own code can read its
//! environment, and a token it writes into the worktree (a debug dump, a `.env`, a test fixture, a
//! commit message) would ride the run branch to the remote at delivery.
//!
//! ## What the guard does
//!
//! Two checks, both over what the work ADDS: every git object reachable from the work's `HEAD` and
//! its current tree (tracked + untracked-not-ignored content, the worktree guard's own snapshot)
//! that is not already reachable from the base. That covers file contents (blobs), file and
//! directory names (trees), and commit messages, including a commit a later commit "removed" the
//! token from, which a push would still ship.
//!
//! 1. **Each unit's settled diff** (`cli_runner::run_unit_and_judge_on`, after the final worktree
//!    guard): new objects since the unit's dispatch baseline. A match FAILS the unit.
//! 2. **Delivery** (`deliver_lift::lift_and_reverify`, before the deliver command runs): new objects
//!    since the run's base, excluding everything the remote already has (`--remotes`). A match
//!    REFUSES the delivery; nothing is pushed.
//!
//! ## Which values it looks for
//!
//! Every value the seat launcher injects is registered here at the moment it is handed to a seat
//! ([`register`], from `container_floor::arm`), so a unit's settled diff is checked against what
//! THIS process put into seats. A delivery also asks the broker for the token it holds now
//! ([`from_broker`]), because the units that produced the work may have run before a daemon
//! restart. Only real credentials count: the per-unit gateway keys a seat also holds are revoked when
//! the unit's lease drops, and they authorize nothing outside the gateway.
//!
//! ## Never the value
//!
//! The comparison is an exact substring search in this process ([`memchr::memmem`]); the value is
//! never put on an argv, in an environment, an error, a log line, an event or a test snapshot. A
//! refusal names the place (path, or which commit's message) and the credential's KIND. The raw
//! value and its base64 cores (each of the three alignments, standard and URL-safe alphabets) are
//! searched, so `Authorization: Basic …`-style encodings are caught too; [`SeatSecret`]'s `Debug`
//! prints the kind alone.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
#[cfg(not(test))]
use std::sync::Mutex;

use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD};
use base64::Engine as _;
use wicked_apps_core::spawn::HardenedCommand;

/// The marker every refusal starts with: the deliver refusal is `deliver: SEAT-CREDENTIAL — …`,
/// the unit failure `SEAT-CREDENTIAL — …`. Crew's deliver triage keys on it.
pub(crate) const SEAT_CREDENTIAL_MARKER: &str = "SEAT-CREDENTIAL";

/// Shorter than this is not a credential the launcher hands out (a setup token is ~100 chars), and
/// a short needle would match by chance.
const MIN_SECRET_LEN: usize = 16;
/// A base64 core shorter than this is skipped (chance matches).
const MIN_ENCODED_LEN: usize = 20;
/// The registry keeps this many distinct values (a re-sign-in adds one; the oldest goes first).
const REGISTRY_CAP: usize = 32;

/// What a credential is, named in a refusal instead of its value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SecretKind {
    /// The operator's `claude setup-token` token a claude seat passes through (core#1024).
    ClaudeSetupToken,
}

impl SecretKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            SecretKind::ClaudeSetupToken => {
                "the claude seat's sign-in token (`claude setup-token`, CLAUDE_CODE_OAUTH_TOKEN)"
            }
        }
    }
}

/// How the value appeared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Encoding {
    Raw,
    Base64,
}

/// One injected credential's search needles. `Debug` prints the kind only, never a needle.
#[derive(Clone)]
pub(crate) struct SeatSecret {
    kind: SecretKind,
    needles: Vec<(Encoding, Vec<u8>)>,
}

impl std::fmt::Debug for SeatSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SeatSecret")
            .field("kind", &self.kind)
            .field("needles", &"<redacted>")
            .finish()
    }
}

impl SeatSecret {
    /// The needles for `value`: the raw bytes, then the base64 core of each alignment (the value
    /// starting 0, 1 or 2 bytes into a 3-byte group), in both alphabets. `None` for a value too
    /// short to be a credential.
    pub(crate) fn new(kind: SecretKind, value: &str) -> Option<Self> {
        let raw = value.trim().as_bytes();
        if raw.len() < MIN_SECRET_LEN {
            return None;
        }
        let mut needles = vec![(Encoding::Raw, raw.to_vec())];
        for skip in 0..3 {
            let rest = &raw[skip..];
            let whole = rest.len() / 3 * 3;
            for engine in [&STANDARD_NO_PAD, &URL_SAFE_NO_PAD] {
                let core = engine.encode(&rest[..whole]).into_bytes();
                if core.len() >= MIN_ENCODED_LEN && !needles.iter().any(|(_, n)| *n == core) {
                    needles.push((Encoding::Base64, core));
                }
            }
        }
        Some(Self { kind, needles })
    }

    fn same_value(&self, other: &SeatSecret) -> bool {
        self.needles.first() == other.needles.first()
    }

    /// The first way this secret appears in `bytes`, if it does.
    fn found_in(&self, bytes: &[u8]) -> Option<Encoding> {
        self.needles
            .iter()
            .find(|(_, n)| memchr::memmem::find(bytes, n).is_some())
            .map(|(e, _)| *e)
    }
}

/// The process-wide registry. Under `cfg(test)` each test thread has its own, so one test's
/// registered value never reaches another test's scan.
#[cfg(not(test))]
static INJECTED: Mutex<Vec<SeatSecret>> = Mutex::new(Vec::new());

#[cfg(not(test))]
fn with_registry<R>(f: impl FnOnce(&mut Vec<SeatSecret>) -> R) -> R {
    f(&mut INJECTED.lock().unwrap_or_else(|p| p.into_inner()))
}

#[cfg(test)]
thread_local! {
    static INJECTED: std::cell::RefCell<Vec<SeatSecret>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn with_registry<R>(f: impl FnOnce(&mut Vec<SeatSecret>) -> R) -> R {
    INJECTED.with(|r| f(&mut r.borrow_mut()))
}

/// Record a credential the launcher is handing to a seat. Called where the value is injected, so
/// every unit's settled diff is checked against everything this process put into seats.
pub(crate) fn register(kind: SecretKind, value: &str) {
    let Some(secret) = SeatSecret::new(kind, value) else {
        return;
    };
    with_registry(|reg| {
        if reg.iter().any(|s| s.same_value(&secret)) {
            return;
        }
        if reg.len() >= REGISTRY_CAP {
            reg.remove(0);
        }
        reg.push(secret);
    });
}

/// Every credential this process has put into a seat.
pub(crate) fn injected() -> Vec<SeatSecret> {
    with_registry(|reg| reg.clone())
}

/// The registry plus `more`, without duplicates.
pub(crate) fn with_injected(more: Vec<SeatSecret>) -> Vec<SeatSecret> {
    let mut all = injected();
    for s in more {
        if !all.iter().any(|a| a.same_value(&s)) {
            all.push(s);
        }
    }
    all
}

/// The credentials the broker under `gateway_dir` holds for seats right now (the claude
/// setup-token). `Ok(empty)` when nothing is signed in or the daemon has never brokered a seat
/// (no gateway dir); `Err` (a reason, never a value) when the broker could not be read.
pub(crate) fn from_broker(gateway_dir: Option<&Path>) -> Result<Vec<SeatSecret>, String> {
    let Some(dir) = gateway_dir.filter(|d| d.is_dir()) else {
        return Ok(Vec::new());
    };
    broker_token(dir).map(|t| {
        t.and_then(|t| SeatSecret::new(SecretKind::ClaudeSetupToken, &t))
            .into_iter()
            .collect()
    })
}

#[cfg(unix)]
fn broker_token(dir: &Path) -> Result<Option<String>, String> {
    let rt = crate::container_floor::runtime()
        .map_err(|w| format!("no container runtime answered ({w:?})"))?;
    let gw = crate::seat_gateway::Gateway::for_dir(dir);
    crate::seat_login::claude_broker_token(&rt, &gw, dir)
}

#[cfg(not(unix))]
fn broker_token(_dir: &Path) -> Result<Option<String>, String> {
    // Native Windows has no seat sign-ins (the container floor needs a Unix host).
    Ok(None)
}

/// Where a credential was found. Names the place and the kind, never the value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Leak {
    pub(crate) kind: SecretKind,
    pub(crate) encoding: Encoding,
    pub(crate) location: String,
}

impl Leak {
    /// `` `path` holds the claude seat's sign-in token (…), base64-encoded ``.
    pub(crate) fn describe(&self) -> String {
        format!(
            "{} holds {}{}",
            self.location,
            self.kind.label(),
            match self.encoding {
                Encoding::Raw => "",
                Encoding::Base64 => ", base64-encoded",
            }
        )
    }
}

/// What a delivery or a unit adds, for [`scan_new_objects`]: everything reachable from `tips`
/// (commits and trees) that is not reachable from `exclude` (or, with `exclude_remotes`, from any
/// remote-tracking ref).
pub(crate) struct Scope<'a> {
    pub(crate) cwd: &'a Path,
    /// The pinned git dir (`GIT_DIR`), never the worktree's own `.git` when one is known.
    pub(crate) git_dir: Option<&'a Path>,
    pub(crate) tips: Vec<String>,
    pub(crate) exclude: Vec<String>,
    pub(crate) exclude_remotes: bool,
}

impl Scope<'_> {
    fn env(&self) -> Vec<(&'static str, &Path)> {
        match self.git_dir {
            Some(d) => vec![("GIT_DIR", d), ("GIT_WORK_TREE", self.cwd)],
            None => Vec::new(),
        }
    }
}

/// Search every object the scope adds for any of `secrets`. `Ok(None)` = clean, `Ok(Some)` = the
/// first place a credential appears, `Err` = the scan could not complete (callers fail closed).
pub(crate) fn scan_new_objects(
    scope: &Scope<'_>,
    secrets: &[SeatSecret],
) -> Result<Option<Leak>, String> {
    if secrets.is_empty() {
        return Ok(None);
    }
    let env = scope.env();
    // An exclusion git cannot resolve only widens the scan; never let it fail the scan.
    let exclude: Vec<&str> = scope
        .exclude
        .iter()
        .map(String::as_str)
        .filter(|x| {
            !x.is_empty()
                && crate::worktree_guard::git(
                    scope.cwd,
                    &["rev-parse", "--verify", "-q", &format!("{x}^{{commit}}")],
                    &env,
                )
                .is_ok()
        })
        .collect();
    // `--objects-edge-aggressive`, not `--objects`: a bare TREE tip (the uncommitted work's
    // snapshot) is otherwise listed whole, the base's unchanged files included. The edge lines it
    // adds (`-<commit>`) are dropped below.
    let mut args: Vec<&str> = vec!["rev-list", "--objects-edge-aggressive"];
    args.extend(scope.tips.iter().map(String::as_str));
    args.push("--not");
    args.extend(exclude);
    if scope.exclude_remotes {
        args.push("--remotes");
    }
    let listed = crate::worktree_guard::git(scope.cwd, &args, &env)
        .map_err(|e| format!("could not list the work's objects: {e}"))?;
    let objects: Vec<(String, Option<String>)> = String::from_utf8_lossy(&listed)
        .lines()
        .filter(|l| !l.starts_with('-'))
        .filter_map(|l| {
            let (id, path) = match l.split_once(' ') {
                Some((id, p)) => (id, Some(p.to_string())),
                None => (l, None),
            };
            (!id.is_empty()).then(|| (id.to_string(), path))
        })
        .collect();
    if objects.is_empty() {
        return Ok(None);
    }
    read_objects(scope, &env, &objects, secrets)
}

/// Stream `objects` through one `git cat-file --batch` and search each as it arrives.
fn read_objects(
    scope: &Scope<'_>,
    env: &[(&'static str, &Path)],
    objects: &[(String, Option<String>)],
    secrets: &[SeatSecret],
) -> Result<Option<Leak>, String> {
    // spawn-audit: hardened — git plumbing over the run's own object store; reads no engine state.
    let mut cmd = Command::new("git");
    cmd.hardened()
        .args(["cat-file", "--batch"])
        .current_dir(scope.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("could not spawn `git cat-file`: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("`git cat-file` has no stdin")?;
    let ids: String = objects.iter().map(|(id, _)| format!("{id}\n")).collect();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(ids.as_bytes());
    });
    let stdout = child.stdout.take().ok_or("`git cat-file` has no stdout")?;
    let found = search_batch(BufReader::new(stdout), objects, secrets);
    // Stop early on a hit: the rest of the batch is not needed.
    let _ = child.kill();
    let _ = child.wait();
    let _ = writer.join();
    found
}

fn search_batch(
    mut out: impl BufRead,
    objects: &[(String, Option<String>)],
    secrets: &[SeatSecret],
) -> Result<Option<Leak>, String> {
    let mut header = String::new();
    for (id, path) in objects {
        header.clear();
        out.read_line(&mut header)
            .map_err(|e| format!("could not read `git cat-file`: {e}"))?;
        let mut parts = header.split_whitespace();
        let (Some(_), Some(kind), Some(size)) = (parts.next(), parts.next(), parts.next()) else {
            return Err(format!(
                "object {} could not be read ({})",
                short(id),
                header.trim()
            ));
        };
        let size: usize = size
            .parse()
            .map_err(|_| format!("object {} has an unreadable size", short(id)))?;
        let mut body = vec![0u8; size];
        out.read_exact(&mut body)
            .map_err(|e| format!("object {} could not be read: {e}", short(id)))?;
        let mut nl = [0u8; 1];
        out.read_exact(&mut nl)
            .map_err(|e| format!("object {} could not be read: {e}", short(id)))?;
        for s in secrets {
            if let Some(encoding) = s.found_in(&body) {
                return Ok(Some(Leak {
                    kind: s.kind,
                    encoding,
                    location: location(kind, id, path.as_deref()),
                }));
            }
        }
    }
    Ok(None)
}

fn short(id: &str) -> &str {
    &id[..id.len().min(10)]
}

fn location(kind: &str, id: &str, path: Option<&str>) -> String {
    match (kind, path.filter(|p| !p.is_empty())) {
        ("blob", Some(p)) => format!("`{p}`"),
        ("blob", None) => format!("a file (blob {})", short(id)),
        ("tree", Some(p)) => format!("a file or directory name in `{p}/`"),
        ("tree", None) => "a file or directory name at the repository root".to_string(),
        ("commit", _) => format!("the message of commit {}", short(id)),
        (other, _) => format!("{other} object {}", short(id)),
    }
}

/// The deliver refusal: names where and what, says nothing was pushed and how to retry.
pub(crate) fn deliver_refusal(leak: &Leak) -> String {
    format!(
        "deliver: {SEAT_CREDENTIAL_MARKER} — the work carries a credential the platform put into a \
         seat: {}. Nothing was pushed — the deliver gate never ships a seat's credential. Remove it \
         from the run branch (the file, and any commit that carried it) and approve to retry. The \
         value is never shown.",
        leak.describe()
    )
}

/// The unit failure: names where and what; the work cannot carry it forward.
pub(crate) fn unit_refusal(ord: u32, leak: &Leak) -> String {
    format!(
        "{SEAT_CREDENTIAL_MARKER} — unit {ord} left a credential the platform put into its seat in \
         the work: {}. The unit failed so the work cannot carry it on to delivery. Remove it (the \
         file, and any commit that carried it) before retrying. The value is never shown.",
        leak.describe()
    )
}

/// A unit's settled diff: everything the unit added since its dispatch baseline, against what this
/// process put into seats. `Ok(None)` when nothing was injected (no scan at all), or the work is
/// clean.
pub(crate) fn unit_leak(
    workdir: &Path,
    baseline: &crate::worktree_guard::WorktreeSnapshot,
) -> Result<Option<Leak>, String> {
    let secrets = injected();
    if secrets.is_empty() {
        return Ok(None);
    }
    let git_dir = baseline.git_dir.as_deref().map(PathBuf::from);
    let now = match git_dir.as_deref() {
        Some(d) => crate::worktree_guard::snapshot_through(workdir, d),
        None => crate::worktree_guard::snapshot_through(workdir, &own_git_dir(workdir)?),
    }
    .map_err(|e| format!("the worktree could not be snapshotted: {e}"))?;
    scan_new_objects(
        &Scope {
            cwd: workdir,
            git_dir: git_dir.as_deref(),
            tips: vec![now.head, now.tree],
            exclude: vec![baseline.head.clone()],
            exclude_remotes: false,
        },
        &secrets,
    )
}

/// A baseline recorded before the git dir was pinned on it: the worktree's own git dir.
fn own_git_dir(workdir: &Path) -> Result<PathBuf, String> {
    let dir = crate::worktree_guard::git_string(workdir, &["rev-parse", "--absolute-git-dir"], &[])
        .map_err(|e| e.to_string())?;
    Ok(PathBuf::from(dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake setup-token-shaped value, built at runtime so no bearer-looking literal sits in the
    /// source (GitGuardian flags those; core#1024).
    fn fake_token(seed: &str) -> String {
        let body: String = (0..90)
            .map(|i| {
                let c = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-";
                c[(i * 7 + seed.len() * 3) % c.len()] as char
            })
            .collect();
        format!("{}-{}-{}{seed}-{body}", "sk", "ant", "oat01")
    }

    fn git_ok(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git") // spawn-audit: test-only — a git fixture; reads no engine state.
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {:?}", out);
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn init(dir: &Path) {
        git_ok(dir, &["init", "-q", "-b", "main"]);
        git_ok(dir, &["config", "commit.gpgsign", "false"]);
        git_ok(dir, &["config", "core.autocrlf", "false"]);
    }

    /// A scratch directory, unique per test thread; `d.path()` reads like a tempdir's.
    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "wicked-seat-secret-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn repo() -> (Scratch, String) {
        let d = Scratch::new();
        init(d.path());
        std::fs::write(d.path().join("README.md"), "base\n").unwrap();
        git_ok(d.path(), &["add", "-A"]);
        git_ok(d.path(), &["commit", "-q", "-m", "base"]);
        let base = git_ok(d.path(), &["rev-parse", "HEAD"]);
        (d, base)
    }

    fn scan(dir: &Path, base: &str, secrets: &[SeatSecret]) -> Option<Leak> {
        let snap = crate::worktree_guard::snapshot_through(
            dir,
            &PathBuf::from(git_ok(dir, &["rev-parse", "--absolute-git-dir"])),
        )
        .unwrap();
        scan_new_objects(
            &Scope {
                cwd: dir,
                git_dir: None,
                tips: vec![snap.head, snap.tree],
                exclude: vec![base.to_string()],
                exclude_remotes: true,
            },
            secrets,
        )
        .unwrap()
    }

    #[test]
    fn an_injected_token_in_an_untracked_file_is_found_by_path_and_kind_never_by_value() {
        let token = fake_token("a");
        let secret = SeatSecret::new(SecretKind::ClaudeSetupToken, &token).unwrap();
        let (d, base) = repo();
        std::fs::create_dir_all(d.path().join("debug")).unwrap();
        std::fs::write(
            d.path().join("debug/env.txt"),
            format!("HOME=/x\nCLAUDE_CODE_OAUTH_TOKEN={token}\n"),
        )
        .unwrap();
        let leak = scan(d.path(), &base, std::slice::from_ref(&secret)).expect("found");
        assert_eq!(leak.kind, SecretKind::ClaudeSetupToken);
        assert_eq!(leak.encoding, Encoding::Raw);
        assert_eq!(leak.location, "`debug/env.txt`");
        for text in [
            deliver_refusal(&leak),
            unit_refusal(3, &leak),
            format!("{leak:?}"),
            format!("{secret:?}"),
        ] {
            assert!(!text.contains(&token), "the value leaked into: {text}");
            assert!(!text.contains(&token[10..40]));
        }
        assert!(deliver_refusal(&leak).starts_with("deliver: SEAT-CREDENTIAL — "));
        assert!(deliver_refusal(&leak).contains("`debug/env.txt`"));
        assert!(deliver_refusal(&leak).contains("claude setup-token"));
    }

    #[test]
    fn a_base64_encoding_at_any_alignment_is_found() {
        let token = fake_token("b");
        let secret = SeatSecret::new(SecretKind::ClaudeSetupToken, &token).unwrap();
        for prefix in ["", "u", "us", "user:"] {
            for engine in [
                &base64::engine::general_purpose::STANDARD,
                &base64::engine::general_purpose::URL_SAFE,
            ] {
                let encoded = engine.encode(format!("{prefix}{token}"));
                let (d, base) = repo();
                std::fs::write(
                    d.path().join("auth.json"),
                    format!("{{\"b\":\"{encoded}\"}}"),
                )
                .unwrap();
                let leak = scan(d.path(), &base, std::slice::from_ref(&secret))
                    .unwrap_or_else(|| panic!("prefix {prefix:?} not found"));
                assert_eq!(leak.encoding, Encoding::Base64);
                assert_eq!(leak.location, "`auth.json`");
            }
        }
    }

    #[test]
    fn a_commit_that_carried_the_token_is_found_even_after_a_later_commit_removed_it() {
        let token = fake_token("c");
        let secret = SeatSecret::new(SecretKind::ClaudeSetupToken, &token).unwrap();
        let (d, base) = repo();
        std::fs::write(d.path().join("leak.txt"), &token).unwrap();
        git_ok(d.path(), &["add", "-A"]);
        git_ok(d.path(), &["commit", "-q", "-m", "oops"]);
        git_ok(d.path(), &["rm", "-q", "leak.txt"]);
        git_ok(d.path(), &["commit", "-q", "-m", "remove"]);
        let leak = scan(d.path(), &base, std::slice::from_ref(&secret)).expect("history scanned");
        assert_eq!(leak.location, "`leak.txt`");
        // …and in a commit message.
        let (d, base) = repo();
        git_ok(
            d.path(),
            &[
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                &format!("token {token}"),
            ],
        );
        let leak = scan(d.path(), &base, &[secret]).expect("message scanned");
        assert!(leak.location.starts_with("the message of commit "));
    }

    #[test]
    fn clean_work_unrelated_values_and_the_bases_own_content_pass() {
        let token = fake_token("d");
        let secret = SeatSecret::new(SecretKind::ClaudeSetupToken, &token).unwrap();
        // The base already holds it (the remote has it): not the work's to answer for.
        let d = Scratch::new();
        init(d.path());
        std::fs::write(d.path().join("old.txt"), &token).unwrap();
        git_ok(d.path(), &["add", "-A"]);
        git_ok(d.path(), &["commit", "-q", "-m", "base"]);
        let base = git_ok(d.path(), &["rev-parse", "HEAD"]);
        std::fs::write(d.path().join("new.txt"), "work\n").unwrap();
        assert_eq!(scan(d.path(), &base, std::slice::from_ref(&secret)), None);
        // Another value of the same shape is not this one.
        std::fs::write(d.path().join("other.txt"), fake_token("zz")).unwrap();
        assert_eq!(scan(d.path(), &base, std::slice::from_ref(&secret)), None);
        // Nothing to look for: no scan at all.
        assert_eq!(scan(d.path(), &base, &[]), None);
    }

    #[test]
    fn short_values_are_not_registered_and_the_registry_dedupes() {
        assert!(SeatSecret::new(SecretKind::ClaudeSetupToken, "short").is_none());
        let token = fake_token("e");
        register(SecretKind::ClaudeSetupToken, &token);
        register(SecretKind::ClaudeSetupToken, &format!("  {token}\n"));
        let mine = SeatSecret::new(SecretKind::ClaudeSetupToken, &token).unwrap();
        assert_eq!(injected().iter().filter(|s| s.same_value(&mine)).count(), 1);
        assert_eq!(
            with_injected(vec![mine.clone()])
                .iter()
                .filter(|s| s.same_value(&mine))
                .count(),
            1
        );
    }

    #[test]
    fn no_gateway_dir_means_nothing_brokered() {
        assert!(from_broker(None).unwrap().is_empty());
        let d = Scratch::new();
        assert!(from_broker(Some(&d.path().join("absent")))
            .unwrap()
            .is_empty());
    }
}
