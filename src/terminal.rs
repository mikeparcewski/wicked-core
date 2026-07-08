//! Off-actor PTY I/O for the terminal-session capability (DES-TERMINAL-001).
//!
//! The single-writer split (DES §4): the actor owns the small session *registry* (id → status +
//! per-terminal `seq`), but PTY byte-I/O runs OFF the actor so a chatty process can never starve the
//! store writer. Concretely:
//!
//!  * **Writers/masters/children live here**, in a shared `Mutex<HashMap>` — NOT on the actor. Input
//!    (`Core::write_terminal`) and resize (`Core::resize_terminal`) lock this map directly and act on
//!    it, never round-tripping the store-writer actor.
//!  * **A reader thread per terminal** drains the master in bounded (≤16 KB) chunks and posts each
//!    back to the actor's single emit point as [`Command::TerminalChunk`] — exactly mirroring how a
//!    worker streams `CliOutputDelta` (see `actor::dispatch_unit`), so output stays globally ordered
//!    with a per-terminal `seq` assigned on the one actor thread. On EOF it posts
//!    [`Command::TerminalReaderDone`] so the actor can reap + emit `TerminalExited`.
//!
//! Lifecycle / no orphans (DES §5, R1): the child + reader-thread handle live in [`PtySession`] so
//! the actor can kill the child and join the thread on `CloseTerminal` and on shutdown. See
//! `actor::finish_terminal`.

use std::collections::HashMap;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

use crate::command::Command;

/// Bounded read-chunk cap — mirrors the CliOutputDelta path's backpressure posture (DES §6): never
/// unbounded-buffer a chatty PTY; drain at most this many bytes per read → per event.
const READ_CHUNK: usize = 16 * 1024;

/// The off-actor PTY I/O for one open terminal. All fields are `Send` so the whole map can be shared
/// across the actor thread (open/close/shutdown) and caller threads (write/resize) behind a mutex.
pub(crate) struct PtySession {
    /// The PTY master writer — keystrokes IN. Written by `Core::write_terminal` (off-actor).
    pub(crate) writer: Box<dyn Write + Send>,
    /// The PTY master — resized by `Core::resize_terminal` (off-actor). Kept so resize works after open.
    pub(crate) master: Box<dyn portable_pty::MasterPty + Send>,
    /// The child process — killed + reaped on close/shutdown (no orphaned process, DES R1).
    pub(crate) child: Box<dyn portable_pty::Child + Send + Sync>,
    /// The reader thread — joined on close/shutdown (no leaked thread, DES R1).
    pub(crate) reader: Option<JoinHandle<()>>,
}

/// The shared off-actor writer/master/child map (DES §4 "off-actor `Mutex<HashMap>`"). Held by both
/// the actor and every [`crate::Core`] handle.
pub(crate) type PtyMap = Arc<Mutex<HashMap<String, PtySession>>>;

/// A fresh, empty PTY map.
pub(crate) fn new_map() -> PtyMap {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Lock the PTY map, recovering from poisoning (a panic while holding it must not wedge the actor —
/// the guarded map is just handle bookkeeping, always safe to keep using).
pub(crate) fn lock(map: &PtyMap) -> MutexGuard<'_, HashMap<String, PtySession>> {
    map.lock().unwrap_or_else(|p| p.into_inner())
}

/// Mint a process-unique terminal id (no `uuid` dep): a monotonic counter salted with a nanosecond
/// timestamp. Unique within a process, which is all a [`crate::Core`] instance needs.
pub(crate) fn new_id() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("term-{ts}-{n}")
}

/// Spawn a PTY running `cmd` (or the login shell if `None`/empty) in `cwd`, register its
/// writer/master/child in `map`, and start the reader thread. The reader posts each ≤16 KB chunk to
/// the actor via `to_actor` as [`Command::TerminalChunk`], then [`Command::TerminalReaderDone`] on
/// EOF. Returns `Err` (nothing registered) if the PTY can't be opened or the command can't be spawned.
pub(crate) fn spawn_pty(
    id: &str,
    cwd: &Path,
    cmd: Option<Vec<String>>,
    cols: u16,
    rows: u16,
    map: &PtyMap,
    to_actor: Sender<Command>,
) -> anyhow::Result<()> {
    use portable_pty::{CommandBuilder, PtySize};

    let size = PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    };
    let pair = portable_pty::native_pty_system()
        .openpty(size)
        .map_err(|e| anyhow::anyhow!("openpty failed: {e}"))?;

    // Build the command: an explicit argv, or the user's login shell (DES §3: `cmd: None` ⇒ shell).
    let mut builder = match cmd {
        Some(argv) if !argv.is_empty() => {
            let mut b = CommandBuilder::new(&argv[0]);
            for a in &argv[1..] {
                b.arg(a);
            }
            b
        }
        _ => {
            #[cfg(windows)]
            let shell = std::env::var("COMSPEC").unwrap_or_else(|_| "powershell.exe".to_string());
            #[cfg(not(windows))]
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
            CommandBuilder::new(shell)
        }
    };
    builder.cwd(cwd.as_os_str());
    builder.env("TERM", "xterm-256color");

    let child = pair
        .slave
        .spawn_command(builder)
        .map_err(|e| anyhow::anyhow!("spawn in pty failed: {e}"))?;
    drop(pair.slave); // only ever drive the child via the master

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| anyhow::anyhow!("clone pty reader failed: {e}"))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| anyhow::anyhow!("take pty writer failed: {e}"))?;

    // Reader thread: drain the master OFF the actor in bounded chunks, posting each back to the
    // actor's single emit point. Exits on EOF (child closed the slave — natural exit or a kill).
    let rid = id.to_string();
    let handle = std::thread::spawn(move || {
        let mut buf = vec![0u8; READ_CHUNK];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break, // EOF or error → PTY closed
                Ok(n) => {
                    let cmd = Command::TerminalChunk {
                        id: rid.clone(),
                        bytes: buf[..n].to_vec(),
                    };
                    if to_actor.send(cmd).is_err() {
                        return; // actor gone; stop draining (no ReaderDone — nobody to hear it)
                    }
                }
            }
        }
        // EOF: tell the actor so it reaps the child, joins us, and emits `TerminalExited` once.
        let _ = to_actor.send(Command::TerminalReaderDone { id: rid });
    });

    lock(map).insert(
        id.to_string(),
        PtySession {
            writer,
            master: pair.master,
            child,
            reader: Some(handle),
        },
    );
    Ok(())
}
