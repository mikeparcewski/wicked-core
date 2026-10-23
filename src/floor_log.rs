//! FLOOR LOGS — every stream a repo-checks floor process writes, stored ONCE, content-addressed
//! (DES-FLOOR-V2 §6).
//!
//! A floor process's stdout and stderr are streamed straight to a blob in the run's log store
//! (`<worker home>/floor-logs/<run key>/<sha256>`) while being hashed; the blob's name IS its sha256,
//! so the hash on the gate receipt ([`LogRef`]) makes any range of it verifiable on its own. There is
//! no ring buffer as a source of truth: the 4 KB tails a [`crate::repo_checks::CheckRun`] still
//! carries are a display cut of the blob. A stream longer than [`MAX_STREAM_BYTES`] keeps its first
//! bytes and is marked `truncated` — the floor reads a truncated inventory as "cannot separate"
//! (fail-closed), never as complete.
//!
//! The store lives OUTSIDE every write root the floor's sandbox arms (the worktree, the check
//! scratch leaf, the private `TMPDIR`), so a check cannot rewrite a blob, and a reader verifies the
//! hash anyway ([`read_range`]). The line TOC is lazy: computed from the blob on request
//! ([`toc`]), never stored. The store is garbage-collected by AGE, as sccache's and Bazel's disk
//! caches are ([`gc`], at the start of every floor): no lease, no marker — a blob a live floor
//! or a receipt still uses is younger than any expiry.

use crate::repo_checks::hex12;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The largest stream one floor process may leave; past it the blob keeps the first
/// `MAX_STREAM_BYTES` bytes and the ref is `truncated`.
pub const MAX_STREAM_BYTES: u64 = 64 * 1024 * 1024;
/// The log store's directory under the worker home.
pub const LOG_SUBDIR: &str = "floor-logs";
/// A finished blob older than this is collected ([`gc`]).
pub const FLOOR_LOG_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
/// An unfinished `.part-*` older than this is collected: no capture runs a day.
const PART_TTL: Duration = Duration::from_secs(24 * 3600);
/// Nothing modified within this is ever evicted by the byte cap.
const CAP_SPARES: Duration = Duration::from_secs(24 * 3600);
/// The log store's total byte cap, in GB (default [`DEFAULT_FLOOR_LOG_CAP_GB`]); oldest first.
pub const FLOOR_LOG_CAP_ENV: &str = "WICKED_FLOOR_LOG_CAP_GB";
/// The default of [`FLOOR_LOG_CAP_ENV`].
pub const DEFAULT_FLOOR_LOG_CAP_GB: u64 = 5;
/// How much of a stream's end a check run still carries inline (`stdoutTail` / `stderrTail`).
const TAIL: usize = crate::repo_checks::TAIL_BYTES;

/// One stored stream of one floor process, as the gate receipt names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogRef {
    /// Which process of the check: `head`, `excluded`, `retry`, `base`, `toc-base`, `toc-head`.
    pub attempt: String,
    /// `stdout` | `stderr`.
    pub stream: String,
    /// The blob's sha256 (lowercase hex) — also its file name in the run's log store.
    pub sha256: String,
    /// The blob's length in bytes.
    pub bytes: u64,
    /// The process wrote more than [`MAX_STREAM_BYTES`], or its pipe was still held open when the
    /// floor stopped reading: the blob is a prefix of the stream.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// The run's log store: `<root>/r-<12 hex of sha256(run id)>`, or `t-<…of the tree path>` for a
/// floor that belongs to no run.
pub(crate) fn run_dir(root: &Path, run_id: Option<&str>, worktree: &Path) -> PathBuf {
    let key = match run_id {
        Some(id) => format!("r-{}", hex12(id.as_bytes())),
        None => format!("t-{}", hex12(worktree.as_os_str().as_encoded_bytes())),
    };
    root.join(key)
}

/// Restart a blob's age (best-effort).
fn touch(path: &Path) {
    let _ = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|f| f.set_modified(std::time::SystemTime::now()));
}

/// (codex r12) A receipt about to be reused (a cached base record): its blob must still be there,
/// whole ([`read_blob`]), and its age restarts. `Err` — the record is no longer evidence.
pub(crate) fn refresh(dir: &Path, r: &LogRef) -> Result<(), String> {
    read_blob(dir, r)?;
    touch(&dir.join(&r.sha256));
    Ok(())
}

/// [`FLOOR_LOG_CAP_ENV`] in bytes.
pub(crate) fn log_cap_bytes() -> u64 {
    std::env::var(FLOOR_LOG_CAP_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_FLOOR_LOG_CAP_GB)
        .saturating_mul(1_000_000_000)
}

/// Best-effort age GC of the log store at `root` (every run's directory): a finished blob older
/// than [`FLOOR_LOG_TTL`] and a `.part-*` older than [`PART_TTL`] go; then, over `cap` bytes, the
/// oldest blobs go first, never one modified within [`CAP_SPARES`]. Files are told apart by NAME
/// only (`Path::file_name`), so separators never matter. Directories stay (a capture may be about
/// to create its blob in one). Returns the removed paths.
pub(crate) fn gc(root: &Path, now: std::time::SystemTime, cap: u64) -> Vec<PathBuf> {
    let age = |m: &std::fs::Metadata| {
        m.modified()
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .unwrap_or_default()
    };
    let mut removed = Vec::new();
    let mut kept: Vec<(Duration, PathBuf, u64)> = Vec::new();
    let runs = std::fs::read_dir(root).into_iter().flatten().flatten();
    for e in runs.flat_map(|r| std::fs::read_dir(r.path()).into_iter().flatten().flatten()) {
        let (path, Ok(m)) = (e.path(), e.metadata()) else {
            continue;
        };
        let name = e.file_name();
        let name = name.to_string_lossy();
        let ttl = if name.starts_with(".part-") {
            PART_TTL
        } else {
            FLOOR_LOG_TTL
        };
        if !m.is_file() {
            continue;
        }
        if age(&m) > ttl {
            if std::fs::remove_file(&path).is_ok() {
                removed.push(path);
            }
        } else {
            kept.push((age(&m), path, m.len()));
        }
    }
    let mut total: u64 = kept.iter().map(|k| k.2).sum();
    kept.sort_by_key(|k| std::cmp::Reverse(k.0));
    for (age, path, bytes) in kept {
        if total <= cap || age <= CAP_SPARES {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= bytes;
            removed.push(path);
        }
    }
    removed
}

/// The finished capture of one stream.
#[derive(Debug, Default)]
pub(crate) struct Captured {
    pub(crate) sha256: String,
    pub(crate) bytes: u64,
    pub(crate) truncated: bool,
    /// The last [`TAIL`] bytes (display only).
    pub(crate) tail: Vec<u8>,
    /// Why the blob could not be stored, when it could not.
    pub(crate) error: Option<String>,
}

impl Captured {
    pub(crate) fn log_ref(&self, attempt: &str, stream: &str) -> LogRef {
        LogRef {
            attempt: attempt.to_string(),
            stream: stream.to_string(),
            sha256: self.sha256.clone(),
            bytes: self.bytes,
            truncated: self.truncated,
        }
    }
}

struct State {
    file: Option<std::fs::File>,
    tmp: PathBuf,
    hasher: Sha256,
    bytes: u64,
    cap: u64,
    truncated: bool,
    tail: Vec<u8>,
    closed: bool,
    error: Option<String>,
}

impl State {
    fn take(&mut self, chunk: &[u8]) {
        if self.closed {
            return;
        }
        let room = self.cap.saturating_sub(self.bytes) as usize;
        let kept = &chunk[..chunk.len().min(room)];
        if kept.len() < chunk.len() {
            self.truncated = true;
        }
        if !kept.is_empty() {
            self.hasher.update(kept);
            self.bytes += kept.len() as u64;
            if let Some(f) = self.file.as_mut() {
                if let Err(e) = f.write_all(kept) {
                    self.error = Some(format!("the floor log could not be written: {e}"));
                    self.file = None;
                    let _ = std::fs::remove_file(&self.tmp);
                }
            }
        }
        self.tail.extend_from_slice(chunk);
        if self.tail.len() > TAIL * 2 {
            let cut = self.tail.len() - TAIL;
            self.tail.drain(..cut);
        }
    }
}

/// A stream being captured into `dir` by a reader thread.
pub(crate) struct Capture {
    state: Arc<Mutex<State>>,
    dir: PathBuf,
    done: std::sync::mpsc::Receiver<()>,
}

/// Start capturing `r` into the store at `dir` (created when missing), keeping at most `cap` bytes.
pub(crate) fn capture<R: Read + Send + 'static>(mut r: R, dir: &Path, cap: u64) -> Capture {
    let tmp = dir.join(format!(".part-{}", uuid::Uuid::new_v4().simple()));
    let (file, error) = match std::fs::create_dir_all(dir).and_then(|()| {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
    }) {
        Ok(f) => (Some(f), None),
        Err(e) => (
            None,
            Some(format!(
                "the floor log store `{}` could not be written: {e}",
                dir.display()
            )),
        ),
    };
    let state = Arc::new(Mutex::new(State {
        file,
        tmp,
        hasher: Sha256::new(),
        bytes: 0,
        cap,
        truncated: false,
        tail: Vec::with_capacity(TAIL * 2),
        closed: false,
        error,
    }));
    let (done_tx, done) = std::sync::mpsc::channel();
    let shared = Arc::clone(&state);
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match r.read(&mut chunk) {
                Ok(0) => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                // Any other read error leaves the stream unread past this point: a prefix.
                Err(_) => {
                    shared.lock().unwrap_or_else(|p| p.into_inner()).truncated = true;
                    break;
                }
                Ok(n) => shared
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .take(&chunk[..n]),
            }
        }
        let _ = done_tx.send(());
    });
    Capture {
        state,
        dir: dir.to_path_buf(),
        done,
    }
}

impl Capture {
    /// Stop capturing — at EOF, or after `wait` (a detached descendant can hold the pipe open
    /// forever; what was read is the blob, marked `truncated`) — and store the blob under its hash.
    pub(crate) fn finish(self, wait: Duration) -> Captured {
        let eof = self.done.recv_timeout(wait).is_ok();
        let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        s.closed = true;
        if !eof {
            s.truncated = true;
        }
        let sha256 = format!("{:x}", s.hasher.clone().finalize());
        let mut error = s.error.take();
        if let Some(mut f) = s.file.take() {
            let dest = self.dir.join(&sha256);
            let stored = f.flush().and_then(|()| {
                drop(f);
                if dest.is_file() {
                    // (codex r12) A dedup is a fresh use: the blob's age restarts, so the age GC
                    // keeps what this receipt names.
                    touch(&dest);
                    std::fs::remove_file(&s.tmp)
                } else {
                    std::fs::rename(&s.tmp, &dest)
                }
            });
            if let Err(e) = stored {
                let _ = std::fs::remove_file(&s.tmp);
                error = Some(format!("the floor log could not be stored: {e}"));
            }
        }
        let mut tail = std::mem::take(&mut s.tail);
        if tail.len() > TAIL {
            tail.drain(..tail.len() - TAIL);
        }
        Captured {
            sha256,
            bytes: s.bytes,
            truncated: s.truncated,
            tail,
            error,
        }
    }
}

/// The whole blob `r` names, verified against its hash and length.
pub fn read_blob(dir: &Path, r: &LogRef) -> Result<Vec<u8>, String> {
    if r.sha256.len() != 64 || !r.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("`{}` is not a sha256", r.sha256));
    }
    let path = dir.join(&r.sha256);
    let meta = std::fs::symlink_metadata(&path)
        .map_err(|e| format!("log {} is not in the store: {e}", r.sha256))?;
    if !meta.is_file() || meta.len() != r.bytes || r.bytes > MAX_STREAM_BYTES {
        return Err(format!(
            "log {} is not the {}-byte blob the receipt names",
            r.sha256, r.bytes
        ));
    }
    let bytes = std::fs::read(&path).map_err(|e| format!("log {}: {e}", r.sha256))?;
    let got = format!("{:x}", Sha256::digest(&bytes));
    if got != r.sha256 {
        return Err(format!(
            "log {} does not match its hash (the blob hashes to {got}) — refused",
            r.sha256
        ));
    }
    Ok(bytes)
}

/// Bytes `start..end` of the blob `r` names (clamped to its length), after verifying the whole
/// blob against the receipt's hash.
pub fn read_range(dir: &Path, r: &LogRef, start: u64, end: u64) -> Result<Vec<u8>, String> {
    let blob = read_blob(dir, r)?;
    let end = (end as usize).min(blob.len());
    let start = (start as usize).min(end);
    Ok(blob[start..end].to_vec())
}

/// A log's lazy table of contents: the byte range of every line (newline excluded), and the ranges
/// of the lines that carry an annotated id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogToc {
    pub lines: Vec<(u64, u64)>,
    /// `(id, start, end)`: a line that contains `id`.
    pub marks: Vec<(String, u64, u64)>,
}

/// The TOC of the blob `r` names, marking the lines that contain any of `ids`.
pub fn toc(dir: &Path, r: &LogRef, ids: &[String]) -> Result<LogToc, String> {
    let blob = read_blob(dir, r)?;
    let mut lines = Vec::new();
    let mut marks = Vec::new();
    let mut start = 0usize;
    for (i, b) in blob.iter().enumerate() {
        if *b == b'\n' {
            lines.push((start as u64, i as u64));
            start = i + 1;
        }
    }
    if start < blob.len() {
        lines.push((start as u64, blob.len() as u64));
    }
    for &(s, e) in &lines {
        let line = String::from_utf8_lossy(&blob[s as usize..e as usize]);
        for id in ids
            .iter()
            .filter(|id| !id.is_empty() && line.contains(id.as_str()))
        {
            marks.push((id.clone(), s, e));
        }
    }
    Ok(LogToc { lines, marks })
}

/// The text of a stored stream, lossily decoded, for the floor's own readers (the base's failing
/// ids, an inventory). Empty when the blob cannot be read.
pub(crate) fn text(dir: &Path, r: &LogRef) -> String {
    read_blob(dir, r)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "wicked-floorlog-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// A file `age` old (by mtime, relative to `now`) of `bytes` bytes.
    fn aged(path: &Path, now: std::time::SystemTime, age: Duration, bytes: usize) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![b'x'; bytes]).unwrap();
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(now - age).unwrap();
    }

    const H: Duration = Duration::from_secs(3600);

    /// (round 11) Age GC: a blob past [`FLOOR_LOG_TTL`] goes, a recent one stays; a `.part-*`
    /// younger than a day survives (a capture may be writing it), an older one goes.
    #[test]
    fn gc_removes_old_blobs_and_stale_parts_and_keeps_recent_ones() {
        let root = store("gc-age");
        let now = std::time::SystemTime::now();
        let run = root.join("r-0123456789ab");
        aged(&run.join("old-blob"), now, FLOOR_LOG_TTL + H, 4);
        aged(&run.join("new-blob"), now, H, 4);
        aged(&run.join(".part-young"), now, 20 * H, 4);
        aged(&run.join(".part-stale"), now, 25 * H, 4);
        let removed = gc(&root, now, u64::MAX);
        assert_eq!(removed.len(), 2, "{removed:?}");
        assert!(!run.join("old-blob").exists() && !run.join(".part-stale").exists());
        assert!(run.join("new-blob").exists() && run.join(".part-young").exists());
        assert!(
            run.is_dir(),
            "a run's directory stays for a capture about to write into it"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// (round 11) Over the byte cap the oldest blobs go first, and nothing touched in the last 24
    /// hours is ever evicted, even when the store stays over the cap.
    #[test]
    fn the_byte_cap_evicts_oldest_first_and_spares_the_last_day() {
        let root = store("gc-cap");
        let now = std::time::SystemTime::now();
        for (run, name, age) in [
            ("r-a", "five-days", 120 * H),
            ("r-b", "three-days", 72 * H),
            ("r-a", "one-hour", H),
        ] {
            aged(&root.join(run).join(name), now, age, 10);
        }
        // 30 bytes over a 15-byte cap: the five-day blob goes, then the three-day one.
        let removed = gc(&root, now, 15);
        let names: Vec<String> = removed
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["five-days", "three-days"]);
        // Still over a zero cap: the hour-old blob is spared.
        assert!(gc(&root, now, 0).is_empty());
        assert!(root.join("r-a").join("one-hour").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// (round 11) Windows: the store's paths are built by `Path::join` and files told apart by
    /// file NAME, so a root with spaces works with either separator, and a run key holds only
    /// characters every filesystem accepts.
    #[test]
    fn the_log_store_paths_hold_on_windows() {
        let root = store("gc paths").join("floor logs");
        let now = std::time::SystemTime::now();
        let run = run_dir(
            &root,
            Some("run:1/with\\separators"),
            Path::new("C:\\w\\tree"),
        );
        let key = run.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
            "{key}"
        );
        assert_eq!(run.parent(), Some(root.as_path()));
        aged(&run.join(".part-x"), now, 25 * H, 1);
        assert_eq!(gc(&root, now, u64::MAX), vec![run.join(".part-x")]);
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    /// (codex r12) A capture identical to an aged blob dedups onto it and restarts its age: GC over
    /// the cap then spares it, and the new receipt still reads.
    #[test]
    fn a_dedup_restarts_the_blob_age_so_its_new_receipt_survives_gc() {
        let root = store("gc-dedup");
        let dir = root.join("r-0123456789ab");
        let take = || {
            capture(
                std::io::Cursor::new(b"same output\n".to_vec()),
                &dir,
                MAX_STREAM_BYTES,
            )
            .finish(Duration::from_secs(5))
        };
        let first = take();
        let now = std::time::SystemTime::now();
        std::fs::OpenOptions::new()
            .write(true)
            .open(dir.join(&first.sha256))
            .unwrap()
            .set_modified(now - 48 * H)
            .unwrap();
        let again = take();
        assert_eq!(again.sha256, first.sha256, "content-addressed: one blob");
        assert!(gc(&root, std::time::SystemTime::now(), 0).is_empty());
        assert!(read_blob(&dir, &again.log_ref("head", "stdout")).is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// DONE WHEN 6: a range fetched by ref verifies against the receipt hash, a 40 KB line comes
    /// back whole (no 4 KB ring buffer cuts it), and a tampered blob is refused.
    #[test]
    fn a_range_by_ref_verifies_and_a_40_kb_line_comes_back_whole() {
        let dir = store("range");
        let long = "x".repeat(40 * 1024);
        let body = format!("first\n{long}\ntest t::a ... FAILED\nlast");
        let c = capture(
            std::io::Cursor::new(body.clone().into_bytes()),
            &dir,
            MAX_STREAM_BYTES,
        )
        .finish(Duration::from_secs(5));
        assert!(c.error.is_none() && !c.truncated, "{c:?}");
        let r = c.log_ref("head", "stdout");
        assert_eq!(r.bytes, body.len() as u64);
        assert_eq!(r.sha256, format!("{:x}", Sha256::digest(body.as_bytes())));
        assert!(
            c.tail.len() <= TAIL,
            "the inline tail is only a display cut"
        );
        let t = toc(&dir, &r, &["t::a".to_string()]).unwrap();
        assert_eq!(t.lines.len(), 4);
        let (s, e) = t.lines[1];
        let line = read_range(&dir, &r, s, e).unwrap();
        assert_eq!(line.len(), 40 * 1024, "the 40 KB line comes back whole");
        assert_eq!(String::from_utf8(line).unwrap(), long);
        assert_eq!(t.marks.len(), 1);
        let (_, ms, me) = &t.marks[0];
        assert_eq!(
            read_range(&dir, &r, *ms, *me).unwrap(),
            b"test t::a ... FAILED".to_vec()
        );
        // The same bytes again are the same blob (stored once).
        let again = capture(
            std::io::Cursor::new(body.clone().into_bytes()),
            &dir,
            MAX_STREAM_BYTES,
        )
        .finish(Duration::from_secs(5));
        assert_eq!(again.sha256, r.sha256);
        let blobs = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(
            blobs, 1,
            "content-addressed: one blob, no leftover part file"
        );
        // A blob rewritten after the receipt was taken is refused, whatever range is asked for.
        std::fs::write(dir.join(&r.sha256), body.replace("FAILED", "ok....")).unwrap();
        let err = read_range(&dir, &r, 0, 5).unwrap_err();
        assert!(err.contains("does not match its hash"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The per-stream cap fails closed: the blob is the prefix, marked truncated, and verifies.
    #[test]
    fn a_stream_past_the_cap_is_a_truncated_prefix() {
        let dir = store("cap");
        let c = capture(std::io::Cursor::new(vec![b'a'; 10_000]), &dir, 1000)
            .finish(Duration::from_secs(5));
        assert!(c.truncated && c.bytes == 1000, "{c:?}");
        let r = c.log_ref("base", "stderr");
        assert_eq!(read_blob(&dir, &r).unwrap().len(), 1000);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
