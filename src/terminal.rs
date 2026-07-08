//! Off-actor PTY I/O for the terminal-session capability (DES-TERMINAL-001).
//!
//! The single-writer split (DES §4): the actor owns the small session *registry* (id → status +
//! per-terminal `seq`), but PTY byte-I/O runs OFF the actor so a chatty process can never starve the
//! store writer. Concretely:
//!
//!  * **Writers/masters/children live here**, in a shared `Mutex<HashMap>` — NOT on the actor. Input
//!    (`Core::write_terminal`) and resize (`Core::resize_terminal`) lock the map only long enough to
//!    clone out a *per-session* `Arc<Mutex<_>>` handle, then release it and do the (possibly blocking)
//!    I/O under that per-session lock — so a stuck write on one terminal never wedges close/open/resize
//!    or I/O on OTHER terminals (SIG-2).
//!  * **A reader thread per terminal** drains the master in bounded (≤16 KB) chunks. Output is
//!    back-pressured (SIG-1 / DES §5,§6,R2): the reader parks bytes in a bounded, **drop-oldest**
//!    [`TerminalOutputBuffer`] and only forwards to the actor up to an in-flight byte cap, so a chatty
//!    TUI can't grow the command channel without bound (OOM). Each forwarded chunk is posted to the
//!    actor's single emit point as [`Command::TerminalChunk`] — mirroring how a worker streams
//!    `CliOutputDelta` (see `actor::dispatch_unit`) — so output stays globally ordered with a
//!    per-terminal `seq` assigned on the one actor thread. On EOF it posts
//!    [`Command::TerminalReaderDone`] so the actor can reap + emit `TerminalExited`.
//!
//! Lifecycle / no orphans (DES §5, R1): the child + reader-thread handle live in [`PtySession`] so the
//! actor can kill + reap on `CloseTerminal` and on shutdown. Teardown ([`reap_session`]) kills the
//! child's whole **process group** on unix (not just the direct child — CRIT-1) and BOUNDED-joins the
//! reader so the single actor thread can NEVER block indefinitely on close/shutdown.

use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::command::Command;

/// Bounded read-chunk cap — mirrors the CliOutputDelta path's backpressure posture (DES §6): never
/// unbounded-buffer a chatty PTY; drain at most this many bytes per read → per event.
const READ_CHUNK: usize = 16 * 1024;

/// Backpressure caps per terminal (SIG-1 / DES §5,§6, R2). `CAP_BYTES` bounds BOTH the reader's local
/// drop-oldest buffer AND the reader→actor in-flight window, so the pending output per terminal stays
/// ≤ ~2·`CAP_BYTES` no matter how chatty the child is. `CAP_CHUNKS` is a parallel guard on chunk count.
const CAP_BYTES: usize = 2 * 1024 * 1024; // 2 MiB
const CAP_CHUNKS: usize = 4096;

// ── SIG-1: bounded, drop-oldest terminal-output buffer ──────────────────────────────────────────

/// A bounded FIFO of pending PTY output chunks with **drop-oldest** overflow (DES §5/§6, R2). Owned by
/// the reader thread: when the actor can't keep up (its in-flight backlog hits [`CAP_BYTES`]) the
/// reader parks bytes here and, on overflow, discards the OLDEST chunks — bounding memory instead of
/// letting the command channel grow without limit (OOM on a chatty TUI). Dropped bytes are counted
/// into a shared `dropped_total` so the actor can emit a degraded marker to the consumer, and a
/// sticky `degraded` flag records that this terminal lost output.
pub(crate) struct TerminalOutputBuffer {
    queue: VecDeque<Vec<u8>>,
    buffered: usize,
    cap_bytes: usize,
    cap_chunks: usize,
    degraded: bool,
    dropped_total: Arc<AtomicU64>,
}

impl TerminalOutputBuffer {
    pub(crate) fn new(cap_bytes: usize, cap_chunks: usize, dropped_total: Arc<AtomicU64>) -> Self {
        Self {
            queue: VecDeque::new(),
            buffered: 0,
            cap_bytes,
            cap_chunks,
            degraded: false,
            dropped_total,
        }
    }

    /// Enqueue `chunk`, then shed the OLDEST chunks while over either cap. Each shed sets the sticky
    /// degraded flag and adds to the shared dropped-byte total (what the actor reports to the
    /// consumer). We never shed the LAST remaining chunk (`len > 1` guard): a single chunk that alone
    /// exceeds the cap is still delivered, so the buffer always makes forward progress rather than
    /// discarding everything.
    pub(crate) fn push(&mut self, chunk: Vec<u8>) {
        self.buffered += chunk.len();
        self.queue.push_back(chunk);
        while (self.buffered > self.cap_bytes || self.queue.len() > self.cap_chunks)
            && self.queue.len() > 1
        {
            match self.queue.pop_front() {
                Some(old) => {
                    self.buffered -= old.len();
                    self.degraded = true;
                    self.dropped_total.fetch_add(old.len() as u64, Ordering::AcqRel);
                }
                None => break, // unreachable while len > 1 — guard against an infinite loop
            }
        }
    }

    /// Remove + return the oldest queued chunk (FIFO delivery order), or `None` if empty.
    pub(crate) fn pop_oldest(&mut self) -> Option<Vec<u8>> {
        let c = self.queue.pop_front()?;
        self.buffered -= c.len();
        Some(c)
    }

    /// Whether this terminal has EVER shed output (sticky). Read by the SIG-1 unit test; production
    /// surfaces degradation via the shared `dropped_total` counter (see the actor), hence `allow`.
    #[allow(dead_code)]
    pub(crate) fn degraded(&self) -> bool {
        self.degraded
    }

    /// Cumulative dropped bytes (reads the shared counter the actor also observes). Used by the SIG-1
    /// unit test; production reads the `Arc<AtomicU64>` directly in the actor, hence `allow`.
    #[allow(dead_code)]
    pub(crate) fn dropped_total(&self) -> u64 {
        self.dropped_total.load(Ordering::Acquire)
    }
}

/// The off-actor PTY I/O for one open terminal. All fields are `Send` so the whole map can be shared
/// across the actor thread (open/close/shutdown) and caller threads (write/resize) behind a mutex.
pub(crate) struct PtySession {
    /// The PTY master writer — keystrokes IN. Behind a PER-SESSION mutex (SIG-2) so a stuck write
    /// (child not draining stdin) holds only THIS lock, never the shared map lock — close/open/resize
    /// and other terminals stay responsive. `Core::write_terminal` clones this Arc out of the map,
    /// releases the map lock, then does the blocking write here.
    pub(crate) writer: Arc<Mutex<Box<dyn Write + Send>>>,
    /// The PTY master — resized off-actor. Behind its own mutex + Arc so `Core::resize_terminal`
    /// clones it out of the map and resizes without holding the map lock.
    pub(crate) master: Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>,
    /// The child process — killed (whole process GROUP on unix, CRIT-1) + reaped on close/shutdown
    /// (no orphaned process/descendant, DES R1).
    pub(crate) child: Box<dyn portable_pty::Child + Send + Sync>,
    /// The reader thread handle — BOUNDED-joined on close/shutdown (CRIT-1); detached if it somehow
    /// doesn't exit within the grace window (killpg has already closed the fds, so that's not
    /// expected — this is the belt-and-suspenders the old untimed join lacked).
    pub(crate) reader: Option<JoinHandle<()>>,
    /// Disconnects when the reader thread's closure returns (it moves — and thus drops — the paired
    /// sender). Lets teardown wait for reader completion with a TIMEOUT instead of an unbounded
    /// `join()` that a wedged `read()` would block forever (CRIT-1).
    pub(crate) reader_done: Receiver<()>,
    /// The child's pid, cached at spawn. On unix the child is a `setsid` session leader ⇒ its pgid ==
    /// pid, so this is ALSO the process-group id we signal with `killpg` (CRIT-1). Only read on unix
    /// (the Windows teardown path uses `Child::kill`), hence the non-unix `allow`.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) child_pid: Option<u32>,
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

/// The shared backpressure handles the actor keeps in its registry after a successful [`spawn_pty`]:
/// the in-flight byte gauge (which the reader reads to pace itself, SIG-1) and the cumulative
/// dropped-byte counter (which the actor reads to emit the degraded marker).
pub(crate) struct SpawnedTerminal {
    pub(crate) in_flight: Arc<AtomicUsize>,
    pub(crate) dropped_total: Arc<AtomicU64>,
}

/// Spawn a PTY running `cmd` (or the login shell if `None`/empty) in `cwd`, register its
/// writer/master/child in `map`, and start the reader thread. The reader parks output in a bounded
/// drop-oldest buffer and forwards it to the actor (up to the in-flight cap) as
/// [`Command::TerminalChunk`], then [`Command::TerminalReaderDone`] on EOF. Returns the shared
/// backpressure handles on success, or `Err` (nothing registered, child killed+reaped) on failure.
pub(crate) fn spawn_pty(
    id: &str,
    cwd: &Path,
    cmd: Option<Vec<String>>,
    cols: u16,
    rows: u16,
    map: &PtyMap,
    to_actor: Sender<Command>,
) -> anyhow::Result<SpawnedTerminal> {
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

    let mut child = pair
        .slave
        .spawn_command(builder)
        .map_err(|e| anyhow::anyhow!("spawn in pty failed: {e}"))?;
    drop(pair.slave); // only ever drive the child via the master
    // Cache the pid up front: on unix it's the process-GROUP id we killpg on teardown (CRIT-1).
    let child_pid = child.process_id();

    // Minor: on any post-spawn setup failure, KILL + REAP the child before returning `Err` — dropping
    // it silently would leak a zombie (and orphan its PTY).
    let mut reader = match pair.master.try_clone_reader() {
        Ok(r) => r,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow::anyhow!("clone pty reader failed: {e}"));
        }
    };
    let writer = match pair.master.take_writer() {
        Ok(w) => w,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow::anyhow!("take pty writer failed: {e}"));
        }
    };

    // Per-terminal backpressure state (SIG-1): the in-flight gauge paces the reader; the dropped
    // counter tells the actor when to emit a degraded marker.
    let in_flight = Arc::new(AtomicUsize::new(0));
    let dropped_total = Arc::new(AtomicU64::new(0));
    // Reader-exit signal (CRIT-1): the reader moves `done_tx` into its closure; when the closure
    // returns, `done_tx` drops and `done_rx` disconnects — a timeout-able "reader has exited" signal.
    let (done_tx, done_rx) = channel::<()>();

    let rid = id.to_string();
    let reader_in_flight = in_flight.clone();
    let reader_dropped = dropped_total.clone();
    let handle = std::thread::spawn(move || {
        let _done = done_tx; // dropped when this closure returns ⇒ signals teardown the reader exited
        let mut buf = vec![0u8; READ_CHUNK];
        let mut out = TerminalOutputBuffer::new(CAP_BYTES, CAP_CHUNKS, reader_dropped);
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break, // EOF or error → PTY closed (natural exit, or a kill)
                Ok(n) => {
                    out.push(buf[..n].to_vec()); // bounded drop-oldest (SIG-1)
                    // Forward toward the actor only up to the in-flight cap: when the actor is busy
                    // the gauge stays high, the reader stops sending, and `push` sheds the oldest —
                    // so the command channel can't grow without bound.
                    while reader_in_flight.load(Ordering::Acquire) < CAP_BYTES {
                        let Some(chunk) = out.pop_oldest() else { break };
                        reader_in_flight.fetch_add(chunk.len(), Ordering::AcqRel);
                        let cmd = Command::TerminalChunk {
                            id: rid.clone(),
                            bytes: chunk,
                        };
                        if to_actor.send(cmd).is_err() {
                            return; // actor gone; stop draining (no ReaderDone — nobody to hear it)
                        }
                    }
                }
            }
        }
        // EOF: flush whatever remains (bounded by the cap) so trailing output isn't lost, then tell
        // the actor so it reaps the child, joins us, and emits `TerminalExited` once.
        while let Some(chunk) = out.pop_oldest() {
            reader_in_flight.fetch_add(chunk.len(), Ordering::AcqRel);
            if to_actor
                .send(Command::TerminalChunk {
                    id: rid.clone(),
                    bytes: chunk,
                })
                .is_err()
            {
                return;
            }
        }
        let _ = to_actor.send(Command::TerminalReaderDone { id: rid });
    });

    lock(map).insert(
        id.to_string(),
        PtySession {
            writer: Arc::new(Mutex::new(writer)),
            master: Arc::new(Mutex::new(pair.master)),
            child,
            reader: Some(handle),
            reader_done: done_rx,
            child_pid,
        },
    );
    Ok(SpawnedTerminal {
        in_flight,
        dropped_total,
    })
}

// ── CRIT-1: process-GROUP kill (not just the direct child) + BOUNDED reader join ─────────────────

#[cfg(unix)]
mod sig {
    //! Minimal direct FFI into libc (always linked on unix) so we can kill the child's whole PROCESS
    //! GROUP, not just the direct child. We declare `killpg` here rather than take a `libc` crate dep
    //! (it's only a transitive dep of portable-pty, and this change is scoped to three source files).
    //!
    //! `Child::kill` sends SIGHUP to the DIRECT child pid only; a descendant that inherited a slave
    //! fd then keeps the PTY master open, so the reader's `read()` never EOFs and an untimed `join()`
    //! wedges the single actor thread forever (and the descendant is orphaned). portable-pty makes
    //! each child a `setsid` session leader (unix.rs), so the child's pgid == its pid; signalling that
    //! group with `killpg(pid, ..)` reaches the child AND every descendant still in the group.
    //! SIGTERM(15)/SIGKILL(9) are identical across Linux, macOS and the BSDs.
    pub const SIGTERM: i32 = 15;
    pub const SIGKILL: i32 = 9;
    extern "C" {
        pub fn killpg(pgrp: i32, sig: i32) -> i32;
    }
}

/// Wait up to `dur` for the reader thread to have EXITED (its `reader_done` sender dropped, or a value
/// arrived). Returns `true` if it exited within the window, `false` on timeout. Bounded by `dur`.
fn reader_exited(s: &PtySession, dur: Duration) -> bool {
    match s.reader_done.recv_timeout(dur) {
        Ok(()) | Err(RecvTimeoutError::Disconnected) => true,
        Err(RecvTimeoutError::Timeout) => false,
    }
}

/// Tear down one live session's OS resources: (optionally) kill the child's process GROUP, reap the
/// child, and BOUNDED-join the reader thread. This NEVER blocks the caller indefinitely (CRIT-1):
///
///  * on unix we `killpg` the group — SIGTERM, a brief grace, then SIGKILL — so descendants die and
///    the PTY master EOFs, unwedging a reader stuck in `read()`;
///  * the join is BOUNDED: we wait for the reader's exit signal with a timeout and, if it still
///    hasn't exited, DETACH the thread (drop the handle without joining) and proceed.
///
/// `kill=true` for an operator close / shutdown; `kill=false` for a natural EOF (the child already
/// exited — we just reap + join). Returns the child's exit code when known.
pub(crate) fn reap_session(s: &mut PtySession, kill: bool) -> Option<i32> {
    let mut exited = false;
    if kill {
        #[cfg(unix)]
        {
            if let Some(pid) = s.child_pid {
                let pgid = pid as i32;
                // Ask the whole group to terminate, then give it a brief grace to exit cleanly.
                unsafe { sig::killpg(pgid, sig::SIGTERM) };
                exited = reader_exited(s, Duration::from_millis(500));
                if !exited {
                    // Still holding the PTY open — force the group down and wait a little longer.
                    unsafe { sig::killpg(pgid, sig::SIGKILL) };
                    exited = reader_exited(s, Duration::from_millis(1500));
                }
            } else {
                // No pid (shouldn't happen on unix) — fall back to the direct-child kill.
                let _ = s.child.kill();
            }
        }
        #[cfg(not(unix))]
        {
            // Windows: SIGHUP-equivalent to the DIRECT child only. Killing the whole descendant
            // process tree (a Job Object) is the ConPTY parity follow-up (DES §9, R3).
            let _ = s.child.kill();
        }
    }
    // Reap the direct child (no zombie). After a kill it is dead (killpg/kill) so this returns
    // promptly; on a natural EOF it has already exited.
    let status = s.child.wait().ok().map(|st| st.exit_code() as i32);
    // Make sure we've observed the reader's exit (the no-kill / windows / no-pid paths may not have
    // waited yet). Bounded, so the actor can never hang here.
    if !exited {
        exited = reader_exited(s, Duration::from_millis(2000));
    }
    // Join only if the reader actually exited (then it's instant); otherwise DETACH it — drop the
    // handle without joining so the actor proceeds. killpg has already closed the fds, so a
    // still-running reader is not expected in practice.
    match s.reader.take() {
        Some(h) if exited => {
            let _ = h.join();
        }
        Some(_detached) => { /* dropped without join — never block the single actor thread */ }
        None => {}
    }
    status
}

/// Drop-guarded last-resort reaper (Minor): kill + reap every PTY still in the map when the actor
/// loop exits — on a CLEAN `Shutdown` OR a handler PANIC. On a clean shutdown the `Shutdown` arm has
/// already reaped each terminal (map empty ⇒ no-op); on a panic THIS is what prevents a child/thread
/// leak (the old straight-line end-of-`run` drain ran only on normal loop exit, so a panic leaked
/// them — the exact failure DES R1 forbids). Holds its own map clone so drop-order is irrelevant.
pub(crate) struct PtyReaper {
    map: PtyMap,
}

impl PtyReaper {
    pub(crate) fn new(map: PtyMap) -> Self {
        Self { map }
    }
}

impl Drop for PtyReaper {
    fn drop(&mut self) {
        let leftovers: Vec<PtySession> = lock(&self.map).drain().map(|(_, v)| v).collect();
        for mut s in leftovers {
            let _ = reap_session(&mut s, true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // SIG-1: a synthetic burst exceeding the cap drops the OLDEST chunks, flags degraded, and counts
    // the dropped bytes (what the actor reads to emit the consumer-facing degraded marker).
    #[test]
    fn output_buffer_drops_oldest_and_flags_degraded() {
        let dropped = Arc::new(AtomicU64::new(0));
        // 100-byte / 1000-chunk cap; the byte cap binds first here.
        let mut b = TerminalOutputBuffer::new(100, 1000, dropped.clone());

        // Under cap: nothing dropped, not degraded.
        b.push(vec![1u8; 40]);
        b.push(vec![2u8; 40]);
        assert!(!b.degraded(), "must not be degraded before any overflow");
        assert_eq!(b.dropped_total(), 0);

        // Overflow the 100-byte cap (40+40+40 = 120): the OLDEST 40-byte chunk (the 1s) is shed.
        b.push(vec![3u8; 40]);
        assert!(b.degraded(), "degraded flag set after overflow");
        assert_eq!(b.dropped_total(), 40, "shed exactly the oldest 40-byte chunk");
        assert_eq!(dropped.load(Ordering::Acquire), 40, "shared counter reflects the drop");

        // The survivors are the NEWER chunks, still in FIFO order; the oldest is gone.
        assert_eq!(b.pop_oldest(), Some(vec![2u8; 40]));
        assert_eq!(b.pop_oldest(), Some(vec![3u8; 40]));
        assert_eq!(b.pop_oldest(), None);

        // A single chunk larger than the whole cap is kept (we never drop the ONLY chunk to zero the
        // buffer) but is itself over-cap — the next push will shed it.
        b.push(vec![9u8; 250]);
        assert_eq!(b.dropped_total(), 40, "an only-chunk over cap is not shed on its own push");
        b.push(vec![8u8; 10]);
        assert!(b.dropped_total() >= 40 + 250, "pushing after an over-cap only-chunk sheds it");
    }
}
