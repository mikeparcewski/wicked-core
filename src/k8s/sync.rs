//! The replica sync (PARITY B11, D1): how a unit's host paths reach its run's replica volume and
//! how a creator's changes come back.
//!
//! **One stream format, both directions.** The engine never parses a tar header written by a
//! seat. Both directions use one framed stream: [`MAGIC`], then roots ([`ROOT`] marker: the
//! root's replica path and its mode), each followed by its entries (a kind byte, a length-prefixed
//! path relative to the root, a length-prefixed body), then [`END`]. A stream without its end
//! marker is truncated and refused whole. The engine side is [`encode_root`] / [`decode`]; the
//! pod side is [`POD_CODEC`], a fixed Python program run with `python3 -I` in the run's sync pod
//! (the checks image), which only reads and writes bytes: it never runs repository code.
//!
//! **Modes.** `tree` = a unit's worktree: the files git lists (tracked, plus untracked and not
//! ignored), never anything under `.git`. `git` = the replica's own `.git`, rebuilt by the engine
//! at every copy-in (a seat's hooks or config never reach a later unit). `plain` = any other
//! root, copied whole. `seed` = a cache, copied only when the replica has none yet.
//!
//! **Write-back is untrusted input.** [`check_entries`] refuses absolute paths, `..`, a `.git`
//! component (in any case or spelling a case-insensitive file system folds), anything but regular
//! files and symlinks, and symlinks whose target leaves the root; [`apply`] writes on the host
//! with a no-follow walk (`openat(O_NOFOLLOW)`, component by component), so a directory the seat
//! replaced with a link is never followed, and replaces each file by rename, so no partial file
//! is ever visible.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// The stream's first bytes.
pub(crate) const MAGIC: &[u8] = b"WKS1\n";
/// Kind byte: a root marker (path = the root's replica path, body = its mode).
pub(crate) const ROOT: u8 = b'R';
/// Kind byte: the end of the stream.
pub(crate) const END: u8 = b'E';
/// The most bytes one stream may carry (a write-back larger than this is refused, not cut).
pub(crate) const MAX_STREAM: u64 = 512 << 20;
/// The most entries one stream may carry.
pub(crate) const MAX_ENTRIES: usize = 400_000;

/// A root's sync mode (see the module doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Tree,
    Git,
    Plain,
    Seed,
    /// Single files of a directory, named one by one (bound files: launch settings, a prompt).
    Files,
}

impl Mode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Mode::Tree => "tree",
            Mode::Git => "git",
            Mode::Plain => "plain",
            Mode::Seed => "seed",
            Mode::Files => "files",
        }
    }
    fn parse(s: &[u8]) -> Option<Self> {
        match s {
            b"tree" => Some(Mode::Tree),
            b"git" => Some(Mode::Git),
            b"plain" => Some(Mode::Plain),
            b"seed" => Some(Mode::Seed),
            b"files" => Some(Mode::Files),
            _ => None,
        }
    }
}

/// What an entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A regular file.
    File,
    /// A regular file with its execute bit.
    Exec,
    /// A symlink (the body is its target).
    Link,
    /// Anything else the pod found (a fifo, a socket, a device): always refused on write-back.
    Other,
}

impl Kind {
    fn byte(self) -> u8 {
        match self {
            Kind::File => b'f',
            Kind::Exec => b'x',
            Kind::Link => b'l',
            Kind::Other => b'o',
        }
    }
    fn parse(b: u8) -> Option<Self> {
        match b {
            b'f' => Some(Kind::File),
            b'x' => Some(Kind::Exec),
            b'l' => Some(Kind::Link),
            b'o' => Some(Kind::Other),
            _ => None,
        }
    }
}

/// One file of a root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) kind: Kind,
    /// Relative to its root, `/`-separated, UTF-8.
    pub(crate) path: String,
    pub(crate) data: Vec<u8>,
}

/// One root of a stream and its entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Root {
    /// The root's path in the replica: its host path without the leading `/`.
    pub(crate) replica: String,
    pub(crate) mode: Mode,
    pub(crate) entries: Vec<Entry>,
}

fn put(out: &mut dyn Write, kind: u8, path: &[u8], data: &[u8]) -> std::io::Result<()> {
    out.write_all(&[kind])?;
    out.write_all(&(path.len() as u32).to_be_bytes())?;
    out.write_all(path)?;
    out.write_all(&(data.len() as u64).to_be_bytes())?;
    out.write_all(data)
}

/// Write one root and its entries.
pub(crate) fn encode_root(out: &mut dyn Write, root: &Root) -> std::io::Result<()> {
    put(
        out,
        ROOT,
        root.replica.as_bytes(),
        root.mode.as_str().as_bytes(),
    )?;
    for e in &root.entries {
        put(out, e.kind.byte(), e.path.as_bytes(), &e.data)?;
    }
    Ok(())
}

/// A whole stream of `roots`.
pub(crate) fn encode(roots: &[Root]) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    for r in roots {
        encode_root(&mut out, r).expect("writing to a Vec");
    }
    put(&mut out, END, b"", b"").expect("writing to a Vec");
    out
}

fn read_exact_or(r: &mut dyn Read, buf: &mut [u8], what: &str) -> Result<(), String> {
    r.read_exact(buf)
        .map_err(|_| format!("the sync stream was truncated ({what})"))
}

/// Read a whole stream: every root and entry, or `Err` (truncated, oversized, malformed). A
/// stream is never applied in part.
pub(crate) fn decode(r: &mut dyn Read, limit: u64) -> Result<Vec<Root>, String> {
    let mut magic = [0u8; 5];
    read_exact_or(r, &mut magic, "magic")?;
    if magic != MAGIC {
        return Err("the sync stream does not start with its magic".into());
    }
    let mut roots: Vec<Root> = Vec::new();
    let mut total: u64 = 0;
    let mut count = 0usize;
    loop {
        let mut kind = [0u8; 1];
        read_exact_or(r, &mut kind, "kind")?;
        let mut len4 = [0u8; 4];
        read_exact_or(r, &mut len4, "path length")?;
        let plen = u32::from_be_bytes(len4) as u64;
        if plen > 64 * 1024 {
            return Err("a sync path is longer than 64 KiB".into());
        }
        let mut path = vec![0u8; plen as usize];
        read_exact_or(r, &mut path, "path")?;
        let mut len8 = [0u8; 8];
        read_exact_or(r, &mut len8, "body length")?;
        let dlen = u64::from_be_bytes(len8);
        total = total.saturating_add(dlen).saturating_add(plen + 13);
        if total > limit {
            return Err(format!("the sync stream is larger than {limit} bytes"));
        }
        let mut data = vec![0u8; dlen as usize];
        read_exact_or(r, &mut data, "body")?;
        if kind[0] == END {
            if !path.is_empty() || !data.is_empty() {
                return Err("a malformed end marker".into());
            }
            return Ok(roots);
        }
        let path = String::from_utf8(path).map_err(|_| "a sync path is not UTF-8".to_string())?;
        if kind[0] == ROOT {
            let mode = Mode::parse(&data).ok_or("a sync root has an unknown mode")?;
            roots.push(Root {
                replica: path,
                mode,
                entries: Vec::new(),
            });
            continue;
        }
        let kind = Kind::parse(kind[0]).ok_or("a sync entry has an unknown kind")?;
        count += 1;
        if count > MAX_ENTRIES {
            return Err(format!(
                "the sync stream has more than {MAX_ENTRIES} entries"
            ));
        }
        let root = roots.last_mut().ok_or("a sync entry precedes every root")?;
        root.entries.push(Entry { kind, path, data });
    }
}

/// Characters a case-insensitive, normalizing file system ignores in a name (HFS+), so `.g\u{200c}it`
/// would land as `.git` (the class of CVE-2014-9390).
fn ignorable(c: char) -> bool {
    matches!(c as u32, 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x206F | 0xFEFF)
}

/// Whether a path component names a git dir on any host file system.
pub(crate) fn is_git_name(c: &str) -> bool {
    let folded: String = c
        .chars()
        .filter(|c| !ignorable(*c))
        .collect::<String>()
        .trim_end_matches(['.', ' '])
        .to_ascii_lowercase();
    folded == ".git" || folded == "git~1"
}

/// A relative path's components, or `Err` when it is absolute, empty or walks up.
pub(crate) fn components(rel: &str) -> Result<Vec<&str>, String> {
    if rel.is_empty() || rel.starts_with('/') || rel.contains('\0') || rel.contains('\\') {
        return Err(format!("`{rel}` is not a relative path"));
    }
    let parts: Vec<&str> = rel.split('/').collect();
    if parts
        .iter()
        .any(|c| c.is_empty() || *c == "." || *c == "..")
    {
        return Err(format!("`{rel}` is not a plain relative path"));
    }
    Ok(parts)
}

/// Whether `target` (a symlink's text at `rel`) stays inside the root, read lexically.
pub(crate) fn link_stays_inside(rel: &str, target: &[u8]) -> bool {
    let Ok(t) = std::str::from_utf8(target) else {
        return false;
    };
    if t.is_empty() || t.starts_with('/') || t.contains('\0') {
        return false;
    }
    let mut depth: Vec<&str> = rel.split('/').collect();
    depth.pop();
    for c in t.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                if depth.pop().is_none() {
                    return false;
                }
            }
            c => depth.push(c),
        }
    }
    true
}

/// A path as a case-insensitive, normalizing host file system compares it (review r1: a mask
/// for `.env` must also cover `.ENV`): ignorable characters dropped, lowercased.
pub(crate) fn fold(p: &str) -> String {
    p.chars()
        .filter(|c| !ignorable(*c))
        .collect::<String>()
        .to_lowercase()
}

/// Whether `rel` (under a root) is `prefix` or lies under it, compared folded ([`fold`]).
pub(crate) fn under(rel: &str, prefix: &str) -> bool {
    let (rel, prefix) = (fold(rel), fold(prefix));
    rel == prefix || rel.starts_with(&format!("{prefix}/"))
}

/// Every link among `entries` resolves inside the root, following the other links of the set
/// on the way (review r1: `a -> .` and `b -> a/../out` are each lexically inside, together not).
pub(crate) fn links_stay_inside(entries: &[Entry]) -> Result<(), String> {
    links_stay_inside_on(None, entries)
}

/// [`links_stay_inside`], also following the links already on the host under `root` that the
/// write-back keeps (review r2: an ignored host link `a -> .` the seat never saw).
pub(crate) fn links_stay_inside_on(root: Option<&Path>, entries: &[Entry]) -> Result<(), String> {
    let links: std::collections::HashMap<String, String> = entries
        .iter()
        .filter(|e| e.kind == Kind::Link)
        .map(|e| (fold(&e.path), String::from_utf8_lossy(&e.data).into_owned()))
        .collect();
    for e in entries.iter().filter(|e| e.kind == Kind::Link) {
        let mut stack: Vec<String> = e.path.split('/').map(str::to_string).collect();
        stack.pop();
        let mut todo: std::collections::VecDeque<String> = String::from_utf8_lossy(&e.data)
            .split('/')
            .map(str::to_string)
            .collect();
        let mut steps = 0;
        while let Some(c) = todo.pop_front() {
            steps += 1;
            if steps > 4096 {
                return Err(format!("`{}`: its links do not resolve", e.path));
            }
            match c.as_str() {
                "" | "." => {}
                ".." => {
                    if stack.pop().is_none() {
                        return Err(format!("`{}` resolves outside its root", e.path));
                    }
                }
                _ => {
                    if c.starts_with('/') {
                        return Err(format!("`{}` resolves outside its root", e.path));
                    }
                    stack.push(c);
                    let host_link = || {
                        let p = root?.join(stack.join("/"));
                        std::fs::symlink_metadata(&p)
                            .ok()
                            .filter(|m| m.file_type().is_symlink())?;
                        std::fs::read_link(&p)
                            .ok()
                            .map(|t| t.to_string_lossy().into_owned())
                    };
                    let found = links
                        .get(&fold(&stack.join("/")))
                        .cloned()
                        .or_else(host_link);
                    if let Some(t) = found.as_ref() {
                        if t.starts_with('/') {
                            return Err(format!("`{}` resolves outside its root", e.path));
                        }
                        stack.pop();
                        for (i, part) in t.split('/').enumerate() {
                            todo.insert(i, part.to_string());
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// A content digest of one entry (its kind and its bytes), for the copy-in baseline.
pub(crate) fn digest(e: &Entry) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update([e.kind.byte()]);
    h.update(&e.data);
    h.finalize().into()
}

/// The write-back's checks on one root's entries (untrusted: they came from a seat's replica).
/// `skip` = relative paths the seat could never write on the host (masks, nested roots): their
/// entries are dropped, never applied. `Err` names the first refused entry.
pub(crate) fn check_entries(root: &Root, skip: &[String]) -> Result<Vec<Entry>, String> {
    let mut out = Vec::with_capacity(root.entries.len());
    let mut seen = std::collections::HashSet::new();
    for e in &root.entries {
        let parts = components(&e.path)?;
        if parts.iter().any(|c| is_git_name(c)) {
            return Err(format!("`{}` is inside a git dir", e.path));
        }
        if !seen.insert(e.path.to_ascii_lowercase()) {
            return Err(format!(
                "`{}` appears twice (or twice in different case)",
                e.path
            ));
        }
        match e.kind {
            Kind::Other => return Err(format!("`{}` is not a regular file or a symlink", e.path)),
            Kind::Link if !link_stays_inside(&e.path, &e.data) => {
                return Err(format!("`{}` is a symlink that leaves its root", e.path))
            }
            _ => {}
        }
        if skip.iter().any(|s| under(&e.path, s)) {
            continue;
        }
        out.push(e.clone());
    }
    links_stay_inside(&out)?;
    // A path cannot be both a file and the parent of another entry.
    let mut paths: Vec<String> = out.iter().map(|e| fold(&e.path)).collect();
    paths.sort_unstable();
    for w in paths.windows(2) {
        if w[1].starts_with(&format!("{}/", w[0])) {
            return Err(format!("`{}` is a file and a directory", w[0]));
        }
    }
    Ok(out)
}

/// The host files of a root that the write-back deletes: those `listed` on the host and absent
/// from the replica's `entries`, except what the seat could never see (`skip`).
pub(crate) fn deletions(listed: &[String], entries: &[Entry], skip: &[String]) -> Vec<String> {
    let have: std::collections::HashSet<&str> = entries.iter().map(|e| e.path.as_str()).collect();
    listed
        .iter()
        .filter(|p| !have.contains(p.as_str()))
        .filter(|p| !skip.iter().any(|s| under(p, s)))
        .filter(|p| components(p).is_ok_and(|c| !c.iter().any(|c| is_git_name(c))))
        .cloned()
        .collect()
}

/// The files of a host `root` git lists (tracked, plus untracked and not ignored), relative.
/// Run with the engine's hardened git and no fsmonitor; `Err` when git fails.
pub(crate) fn host_tree_listing(root: &Path) -> Result<Vec<String>, String> {
    // Not a repository (an engine session's scratch tree): every file but a `.git`.
    let inside = git(root)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .is_ok_and(|o| o.status.success());
    if !inside {
        return Ok(host_plain_listing(root, &[".git".to_string()]));
    }
    let out = git(root)
        .args(["ls-files", "-z", "-c", "-o", "--exclude-standard"])
        .output()
        .map_err(|e| format!("git ls-files: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git ls-files in {} failed: {}",
            root.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let mut v: Vec<String> = out
        .stdout
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .filter_map(|s| String::from_utf8(s.to_vec()).ok())
        .collect();
    v.sort();
    v.dedup();
    Ok(v)
}

/// The engine's git for a host worktree: no system or global configuration, no fsmonitor, no
/// prompts (the worktree's own repository configuration is the engine's clone's).
pub(crate) fn git(dir: &Path) -> std::process::Command {
    use wicked_apps_core::HardenedCommand;
    let mut c = std::process::Command::new("git");
    c.hardened()
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.untrackedCache=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_CONFIG_COUNT")
        .stdin(std::process::Stdio::null());
    c
}

/// Read one host file or link for the copy-in, never following a link. `None` = gone or not a
/// file or link (left out).
pub(crate) fn read_host_entry(root: &Path, rel: &str) -> Option<Entry> {
    let p = root.join(rel);
    let md = std::fs::symlink_metadata(&p).ok()?;
    let ft = md.file_type();
    if ft.is_symlink() {
        let t = std::fs::read_link(&p).ok()?;
        return Some(Entry {
            kind: Kind::Link,
            path: rel.to_string(),
            data: t.to_str()?.as_bytes().to_vec(),
        });
    }
    if !ft.is_file() {
        return None;
    }
    // Every ancestor must be a real directory: a link on the way would read another file.
    let mut cur = root.to_path_buf();
    for c in components(rel)
        .ok()?
        .iter()
        .take(rel.split('/').count() - 1)
    {
        cur.push(c);
        if !std::fs::symlink_metadata(&cur).ok()?.file_type().is_dir() {
            return None;
        }
    }
    let data = std::fs::read(&p).ok()?;
    #[cfg(unix)]
    let exec = {
        use std::os::unix::fs::PermissionsExt;
        md.permissions().mode() & 0o111 != 0
    };
    #[cfg(not(unix))]
    let exec = false;
    Some(Entry {
        kind: if exec { Kind::Exec } else { Kind::File },
        path: rel.to_string(),
        data,
    })
}

/// Every file and link under a host `root` (no link followed), relative, except under `skip`.
pub(crate) fn host_plain_listing(root: &Path, skip: &[String]) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, skip: &[String], out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            let Some(rel) = p
                .strip_prefix(base)
                .ok()
                .and_then(|r| r.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            if skip.iter().any(|s| under(&rel, s)) {
                continue;
            }
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                walk(base, &p, skip, out);
            } else if ft.is_file() || ft.is_symlink() {
                out.push(rel);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, skip, &mut out);
    out.sort();
    out
}

/// Paths of `others` that lie strictly under `root`, relative to it (nested roots and masks).
pub(crate) fn nested(root: &Path, others: &[PathBuf]) -> Vec<String> {
    others
        .iter()
        .filter(|o| o.as_path() != root)
        .filter_map(|o| o.strip_prefix(root).ok())
        .filter_map(|r| r.to_str())
        .filter(|r| !r.is_empty())
        .map(str::to_string)
        .collect()
}

/// The replica path of a host path: the path without its leading `/`.
pub(crate) fn replica_path(p: &Path) -> Option<String> {
    super::render::replica_sub_path(p)
}

/// What one root's write-back did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Applied {
    pub(crate) written: usize,
    pub(crate) unchanged: usize,
    pub(crate) deleted: usize,
}

/// Apply checked `entries` and `delete` to the host `root` with a no-follow walk: deletions
/// first, then each file or link replaced by rename. Never follows a link on the way.
#[cfg(unix)]
// `mode_t` is u16 on macOS and u32 on Linux: the casts are needed on one of them.
#[allow(clippy::unnecessary_cast)]
pub(crate) fn apply(
    root: &Path,
    entries: &[Entry],
    delete: &[String],
    guard: &[String],
) -> Result<Applied, String> {
    // The paths the seat may never write, as the host's own file system resolves them (review
    // r2: on APFS `.ſsh` names `.ssh`; no case or normalization rule of ours can know that, the
    // inode can). An entry or a deletion whose path, or any ancestor of it, is one of them is
    // refused.
    let guarded: std::collections::HashSet<(u64, u64)> = guard
        .iter()
        .filter_map(|g| std::fs::symlink_metadata(root.join(g)).ok())
        .map(|m| {
            use std::os::unix::fs::MetadataExt;
            (m.dev(), m.ino())
        })
        .collect();
    let hits_guard = |rel: &str| -> bool {
        use std::os::unix::fs::MetadataExt;
        let mut cur = root.to_path_buf();
        rel.split('/').any(|c| {
            cur.push(c);
            std::fs::symlink_metadata(&cur).is_ok_and(|m| guarded.contains(&(m.dev(), m.ino())))
        })
    };
    if let Some(p) = entries
        .iter()
        .map(|e| e.path.as_str())
        .chain(delete.iter().map(String::as_str))
        .find(|p| hits_guard(p))
    {
        return Err(format!(
            "`{p}` is a path the seat may not write (it names a masked path on this host)"
        ));
    }
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = |s: &str| CString::new(s).map_err(|_| format!("`{s}` has a NUL"));
    let root_c = CString::new(root.as_os_str().as_bytes()).map_err(|_| "root has a NUL")?;
    // SAFETY: plain libc calls on owned, NUL-terminated strings; every fd opened is closed.
    let open_dir = |at: i32, name: &CString| unsafe {
        libc::openat(
            at,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    let root_fd = unsafe {
        libc::open(
            root_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if root_fd < 0 {
        return Err(format!(
            "{} is not a directory the write-back can open",
            root.display()
        ));
    }
    struct Fd(i32);
    impl Drop for Fd {
        fn drop(&mut self) {
            unsafe { libc::close(self.0) };
        }
    }
    let root_fd = Fd(root_fd);
    let kind_at = |dir: i32, name: &CString| -> Option<u32> {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let r = unsafe { libc::fstatat(dir, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) };
        (r == 0).then_some(st.st_mode as u32 & libc::S_IFMT as u32)
    };
    // The parent directory of `rel`, walked without following links; `create` makes missing
    // directories and replaces a file or link standing where a directory belongs.
    let parent = |rel: &str, create: bool| -> Result<Option<(Fd, CString)>, String> {
        let parts = components(rel)?;
        let mut fd = Fd(unsafe { libc::dup(root_fd.0) });
        for comp in &parts[..parts.len() - 1] {
            let name = c(comp)?;
            let mut next = open_dir(fd.0, &name);
            if next < 0 {
                if !create {
                    return Ok(None);
                }
                match kind_at(fd.0, &name) {
                    None => {}
                    Some(k) if k == libc::S_IFDIR as u32 => {
                        return Err(format!("`{rel}`: a directory on the way cannot be opened"))
                    }
                    Some(_) if unsafe { libc::unlinkat(fd.0, name.as_ptr(), 0) } != 0 => {
                        return Err(format!("`{rel}`: could not replace a file on the way"));
                    }
                    Some(_) => {}
                }
                if unsafe { libc::mkdirat(fd.0, name.as_ptr(), 0o755) } != 0 {
                    return Err(format!("`{rel}`: could not create a directory on the way"));
                }
                next = open_dir(fd.0, &name);
                if next < 0 {
                    return Err(format!("`{rel}`: could not open a directory on the way"));
                }
            }
            fd = Fd(next);
        }
        Ok(Some((fd, c(parts[parts.len() - 1])?)))
    };
    let mut done = Applied::default();
    for rel in delete {
        let Some((dir, name)) = parent(rel, false)? else {
            continue;
        };
        match kind_at(dir.0, &name) {
            None => {}
            Some(k) if k == libc::S_IFDIR as u32 => {}
            Some(_) => {
                if unsafe { libc::unlinkat(dir.0, name.as_ptr(), 0) } != 0 {
                    return Err(format!("could not delete `{rel}`"));
                }
                done.deleted += 1;
            }
        }
    }
    for e in entries {
        let (dir, name) = parent(&e.path, true)?.expect("created");
        let existing = kind_at(dir.0, &name);
        if existing == Some(libc::S_IFREG as u32) && e.kind != Kind::Link {
            // Unchanged content and mode: left alone (no mtime churn for build tools).
            let fd = unsafe {
                libc::openat(
                    dir.0,
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd >= 0 {
                use std::os::unix::io::FromRawFd;
                let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
                let mut cur = Vec::new();
                let mode = f
                    .metadata()
                    .map(|m| std::os::unix::fs::PermissionsExt::mode(&m.permissions()))
                    .unwrap_or(0);
                if f.read_to_end(&mut cur).is_ok()
                    && cur == e.data
                    && ((mode & 0o111 != 0) == (e.kind == Kind::Exec))
                {
                    done.unchanged += 1;
                    continue;
                }
            }
        }
        if existing == Some(libc::S_IFDIR as u32) && !remove_empty_dirs(dir.0, &name) {
            return Err(format!(
                "`{}` replaces a directory that is not empty",
                e.path
            ));
        }
        let tmp = c(&format!(".wicked-wb-{}", uuid::Uuid::new_v4().simple()))?;
        match e.kind {
            Kind::Link => {
                let target = CString::new(e.data.clone()).map_err(|_| "a link target has a NUL")?;
                if unsafe { libc::symlinkat(target.as_ptr(), dir.0, tmp.as_ptr()) } != 0 {
                    return Err(format!("could not write the link `{}`", e.path));
                }
            }
            _ => {
                let mode = if e.kind == Kind::Exec { 0o755 } else { 0o644 };
                let fd = unsafe {
                    libc::openat(
                        dir.0,
                        tmp.as_ptr(),
                        libc::O_WRONLY
                            | libc::O_CREAT
                            | libc::O_EXCL
                            | libc::O_NOFOLLOW
                            | libc::O_CLOEXEC,
                        mode as libc::c_uint,
                    )
                };
                if fd < 0 {
                    return Err(format!("could not write `{}`", e.path));
                }
                use std::os::unix::io::FromRawFd;
                let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
                let wrote = f.write_all(&e.data).and_then(|()| f.sync_data());
                drop(f);
                if wrote.is_err() {
                    unsafe { libc::unlinkat(dir.0, tmp.as_ptr(), 0) };
                    return Err(format!("could not write `{}`", e.path));
                }
            }
        }
        if unsafe { libc::renameat(dir.0, tmp.as_ptr(), dir.0, name.as_ptr()) } != 0 {
            unsafe { libc::unlinkat(dir.0, tmp.as_ptr(), 0) };
            return Err(format!("could not put `{}` in place", e.path));
        }
        done.written += 1;
    }
    Ok(done)
}

/// Remove the directory `name` of `dir` and the empty directories under it, never following a
/// link; `false` when anything but an empty directory is left in it (review r1: a deletion
/// empties a nested tree, its directories go before a file takes its place).
#[cfg(unix)]
fn remove_empty_dirs(dir: i32, name: &std::ffi::CStr) -> bool {
    // SAFETY: fd-relative libc calls on NUL-terminated names; every fd opened is closed.
    unsafe {
        if libc::unlinkat(dir, name.as_ptr(), libc::AT_REMOVEDIR) == 0 {
            return true;
        }
        let fd = libc::openat(
            dir,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        );
        if fd < 0 {
            return false;
        }
        let dup = libc::dup(fd);
        let d = libc::fdopendir(dup);
        if d.is_null() {
            libc::close(dup);
            libc::close(fd);
            return false;
        }
        let mut subdirs: Vec<std::ffi::CString> = Vec::new();
        loop {
            let ent = libc::readdir(d);
            if ent.is_null() {
                break;
            }
            let n = std::ffi::CStr::from_ptr((*ent).d_name.as_ptr());
            if n.to_bytes() == b"." || n.to_bytes() == b".." {
                continue;
            }
            subdirs.push(n.to_owned());
        }
        libc::closedir(d);
        let ok = subdirs.iter().all(|s| remove_empty_dirs(fd, s));
        libc::close(fd);
        ok && libc::unlinkat(dir, name.as_ptr(), libc::AT_REMOVEDIR) == 0
    }
}

#[cfg(not(unix))]
pub(crate) fn apply(
    _root: &Path,
    _entries: &[Entry],
    _delete: &[String],
    _guard: &[String],
) -> Result<Applied, String> {
    Err("the Kubernetes write-back needs a Unix host".into())
}

/// The sync pod's half of the stream (run as `python3 -I -c POD_CODEC <op> <plan-json>`): reads
/// and writes bytes only, with the same no-follow walk, and runs git only as `ls-files`, `bundle`
/// and `rev-parse` with fsmonitor and hooks off (a seat-written `.git/config` cannot make it run
/// anything). Ops:
/// - `unpack <plan>`: the stream on stdin into `/replica`, root by root; `tree` and `plain` roots
///   are mirrored (a replica file the stream does not carry is deleted; for `tree` only files git
///   lists, so ignored build output stays), `git` replaces the root whole, `seed` writes only when
///   the root does not exist yet.
/// - `pack <plan>`: the roots of the plan (JSON `[[replica, mode, [skip...]], ...]`) as a stream on
///   stdout: `tree` = what git lists, `plain` = every file and link.
/// - `bundle <tree> <base>`: the commits on the tree's HEAD since `base`, as a git bundle on
///   stdout (empty output = none).
pub(crate) const POD_CODEC: &str = r#"
import errno, json, os, shutil, stat, struct, subprocess, sys
R = os.environ.get("WICKED_SYNC_ROOT", "/replica")
def die(m):
    sys.stderr.write("wicked-sync: " + m + "\n"); sys.exit(3)
def git(root, *args):
    env = {"PATH": "/usr/bin:/bin", "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null",
           "HOME": "/tmp", "GIT_TERMINAL_PROMPT": "0"}
    return subprocess.run(["git", "-C", root, "-c", "core.fsmonitor=false",
                           "-c", "core.untrackedCache=false", "-c", "core.hooksPath=/dev/null",
                           "-c", "safe.directory=*"] + list(args),
                          env=env, stdin=subprocess.DEVNULL, capture_output=True)
def comps(rel):
    if not rel or rel.startswith("/") or "\0" in rel: die("bad path " + repr(rel))
    p = rel.split("/")
    if any(c in ("", ".", "..") for c in p): die("bad path " + repr(rel))
    return p
IGN = set(range(0x200B, 0x2010)) | set(range(0x202A, 0x202F)) | set(range(0x2060, 0x2070)) | {0xFEFF}
def fold(p): return "".join(c for c in p if ord(c) not in IGN).lower()
def under(rel, pre):
    rel, pre = fold(rel), fold(pre)
    return rel == pre or rel.startswith(pre + "/")
skip = []
def opendir(fd, name): return os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
def remove_any(fd, name):
    st = os.stat(name, dir_fd=fd, follow_symlinks=False)
    if stat.S_ISDIR(st.st_mode): shutil.rmtree(name, dir_fd=fd)
    else: os.unlink(name, dir_fd=fd)
def parent(rootfd, rel, create):
    p = comps(rel); fd = os.dup(rootfd)
    for c in p[:-1]:
        try:
            n = opendir(fd, c)
        except FileNotFoundError:
            if not create: os.close(fd); return None, None
            os.mkdir(c, 0o755, dir_fd=fd); n = opendir(fd, c)
        except OSError as e:
            if e.errno not in (errno.ENOTDIR, errno.ELOOP): raise
            if not create: os.close(fd); return None, None
            remove_any(fd, c); os.mkdir(c, 0o755, dir_fd=fd); n = opendir(fd, c)
        os.close(fd); fd = n
    return fd, p[-1]
def openroot(rel, create):
    fd = os.open(R, os.O_RDONLY | os.O_DIRECTORY)
    for c in comps(rel):
        try:
            n = opendir(fd, c)
        except FileNotFoundError:
            if not create: os.close(fd); return None
            os.mkdir(c, 0o755, dir_fd=fd); n = opendir(fd, c)
        except OSError as e:
            if e.errno not in (errno.ENOTDIR, errno.ELOOP): raise
            if not create: os.close(fd); return None
            remove_any(fd, c); os.mkdir(c, 0o755, dir_fd=fd); n = opendir(fd, c)
        os.close(fd); fd = n
    return fd
def rd(f, n):
    b = f.read(n)
    if len(b) != n: die("truncated stream")
    return b
def entries(f):
    if rd(f, 5) != b"WKS1\n": die("bad magic")
    while True:
        k = rd(f, 1); pl = struct.unpack(">I", rd(f, 4))[0]; p = rd(f, pl)
        dl = struct.unpack(">Q", rd(f, 8))[0]; d = rd(f, dl)
        if k == b"E": return
        yield k, p.decode("utf-8"), d
def tree_list(root):
    if not os.path.isdir(root + "/.git") or os.path.islink(root + "/.git"):
        fd = os.open(root, os.O_RDONLY | os.O_DIRECTORY)
        try: return [p for p in plain_list(fd) if not under(p, ".git")]
        finally: os.close(fd)
    r = git(root, "ls-files", "-z", "-c", "-o", "--exclude-standard")
    if r.returncode != 0: die("git ls-files failed: " + r.stderr.decode(errors="replace").strip())
    return sorted(set(x.decode("utf-8", "surrogateescape") for x in r.stdout.split(b"\0") if x))
def plain_list(rootfd):
    out = []
    def walk(fd, pre):
        for name in os.listdir(fd):
            st = os.stat(name, dir_fd=fd, follow_symlinks=False)
            rel = pre + name
            if stat.S_ISDIR(st.st_mode):
                n = opendir(fd, name)
                try: walk(n, rel + "/")
                finally: os.close(n)
            else: out.append(rel)
    walk(rootfd, "")
    return sorted(out)
def put(out, k, p, d):
    p = p.encode("utf-8", "surrogateescape")
    out.write(k + struct.pack(">I", len(p)) + p + struct.pack(">Q", len(d)) + d)
def emit(out, rootfd, rel):
    fd, name = parent(rootfd, rel, False)
    if fd is None: return
    try:
        try: st = os.stat(name, dir_fd=fd, follow_symlinks=False)
        except FileNotFoundError: return
        if stat.S_ISLNK(st.st_mode):
            put(out, b"l", rel, os.readlink(name, dir_fd=fd).encode("utf-8", "surrogateescape"))
        elif stat.S_ISREG(st.st_mode):
            h = os.open(name, os.O_RDONLY | os.O_NOFOLLOW, dir_fd=fd)
            with os.fdopen(h, "rb") as fh: d = fh.read()
            put(out, b"x" if st.st_mode & 0o111 else b"f", rel, d)
        elif stat.S_ISDIR(st.st_mode): return
        else: put(out, b"o", rel, b"")
    finally: os.close(fd)
def unpack(plan):
    f = sys.stdin.buffer; cur = None; rootfd = None; wrote = set(); mode = None
    def finish():
        if cur is None: return
        r, m, skip = cur
        # A masked path the replica holds (a seat planted it, or it predates the mask) goes: a
        # mask is the absence of the path (review r1).
        masks = plan[r][2]
        if masks:
            for rel in plain_list(rootfd):
                if any(under(rel, mk) for mk in masks):
                    fd, name = parent(rootfd, rel, False)
                    if fd is None: continue
                    try: os.unlink(name, dir_fd=fd)
                    except FileNotFoundError: pass
                    finally: os.close(fd)
        if m == "git" and not wrote:
            os.close(rootfd)
            pfd = openroot(os.path.dirname(r), False) if "/" in r else os.open(R, os.O_RDONLY)
            if pfd is not None:
                try: remove_any(pfd, os.path.basename(r))
                except FileNotFoundError: pass
                finally: os.close(pfd)
            return
        if m in ("tree", "plain"):
            have = tree_list(R + "/" + r) if m == "tree" else plain_list(rootfd)
            have = [h for h in have if not (m == "tree" and under(h, ".git"))]
            for rel in have:
                if rel in wrote or any(under(rel, s) for s in skip): continue
                if m == "tree" and any(c == ".git" for c in rel.split("/")): continue
                fd, name = parent(rootfd, rel, False)
                if fd is None: continue
                try:
                    st = os.stat(name, dir_fd=fd, follow_symlinks=False)
                    if not stat.S_ISDIR(st.st_mode): os.unlink(name, dir_fd=fd)
                except FileNotFoundError: pass
                finally: os.close(fd)
        os.close(rootfd)
    plan = {p[0]: (p[1], p[2], p[3] if len(p) > 3 else []) for p in plan}
    for k, p, d in entries(f):
        if k == b"R":
            finish(); cur = None; wrote = set(); m = d.decode()
            if p not in plan or plan[p][0] != m: die("root not in plan: " + p)
            skip = plan[p][1]
            if m == "git":
                pfd = openroot(os.path.dirname(p), True) if "/" in p else os.open(R, os.O_RDONLY)
                try:
                    try: remove_any(pfd, os.path.basename(p))
                    except FileNotFoundError: pass
                finally: os.close(pfd)
            if m == "seed" and openroot(p, False) is not None:
                cur = None; rootfd = None; mode = "skip"; continue
            rootfd = openroot(p, True); cur = (p, m, skip); mode = m; continue
        if mode == "skip": continue
        if cur is None: die("entry before root")
        rel = p
        if mode == "tree" and any(c == ".git" for c in comps(rel)): die("tree entry in .git: " + rel)
        if mode == "files" and rel not in skip: die("file not in plan: " + rel)
        fd, name = parent(rootfd, rel, True)
        try:
            try: remove_any(fd, name)
            except FileNotFoundError: pass
            if k == b"l":
                os.symlink(d.decode("utf-8", "surrogateescape"), name, dir_fd=fd)
            else:
                h = os.open(name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                            0o755 if k == b"x" else 0o644, dir_fd=fd)
                with os.fdopen(h, "wb") as fh: fh.write(d)
        finally: os.close(fd)
        wrote.add(rel)
    finish()
def pack(plan):
    out = sys.stdout.buffer; out.write(b"WKS1\n")
    for item in plan:
        r, m, skip = item[0], item[1], item[2]
        rootfd = openroot(r, False)
        put(out, b"R", r, m.encode())
        if rootfd is None: continue
        try:
            if m == "files":
                for rel in skip: emit(out, rootfd, rel)
                continue
            rels = tree_list(R + "/" + r) if m == "tree" else plain_list(rootfd)
            for rel in rels:
                if any(under(rel, s) for s in skip): continue
                if m == "tree" and any(c == ".git" for c in rel.split("/")): continue
                emit(out, rootfd, rel)
        finally: os.close(rootfd)
    put(out, b"E", "", b""); out.flush()
def bundle(tree, base):
    root = R + "/" + tree
    head = git(root, "rev-parse", "--verify", "-q", "HEAD^{commit}")
    if head.returncode != 0: die("the replica has no HEAD commit")
    if head.stdout.decode().strip() == base: return
    path = "/tmp/wicked-unit-%d-%s.bundle" % (os.getpid(), os.urandom(8).hex())
    try:
        b = git(root, "bundle", "create", path, "HEAD", "^" + base)
        if b.returncode != 0: die("git bundle failed: " + b.stderr.decode(errors="replace").strip())
        with open(path, "rb") as fh: shutil.copyfileobj(fh, sys.stdout.buffer)
    finally:
        for f in (path, path + ".lock"):
            try: os.unlink(f)
            except FileNotFoundError: pass
op = sys.argv[1]
if op == "unpack": unpack(json.loads(sys.argv[2]))
elif op == "pack": pack(json.loads(sys.argv[2]))
elif op == "bundle": bundle(sys.argv[2], sys.argv[3])
else: die("unknown op")
"#;
