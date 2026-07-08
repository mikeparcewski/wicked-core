//! DES-TERMINAL-001 — PTY terminal-session capability.
//!
//! Proves the round-trip + lifecycle a happy-path can't fake: open a PTY, write input, observe the
//! echoed output over the CoreEvent stream, close it, and confirm `TerminalExited` fires exactly once
//! — and that closing actually reaps the child (no orphan). Unix-only: relies on `cat` + PTY line
//! echo; Windows ConPTY parity is a follow-up (DES §9 R3).
#![cfg(unix)]

use std::time::Duration;

use base64::Engine as _;
use wicked_core::{Core, CoreEvent};

fn unique_db() -> String {
    // A per-process atomic counter is the real uniqueness guarantee: the three tests in this file
    // share one process id and run on parallel threads, so a timestamp alone can collide within a
    // single clock tick — two `Core`s then open the SAME sqlite file and one gets `database is
    // locked`. `fetch_add` can never hand out the same value twice, so no two paths collide. (pid +
    // timestamp are retained only to keep names distinct across separate binary runs.)
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!(
        "wicked-core-term-{}-{}-{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        seq
    ));
    p.to_string_lossy().into_owned()
}

/// Block until an event matches `pred` (or fail after 5s).
fn wait_for(
    events: &std::sync::mpsc::Receiver<CoreEvent>,
    label: &str,
    mut pred: impl FnMut(&CoreEvent) -> bool,
) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        match events.recv_timeout(Duration::from_millis(200)) {
            Ok(ev) => {
                if pred(&ev) {
                    return;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => break,
        }
    }
    panic!("timed out waiting for: {label}");
}

/// Accumulate `TerminalOutput` for terminal `id` (up to 5s) and return the integer that follows the
/// first occurrence of `marker` (e.g. `"BGPID="` ⇒ the backgrounded child's pid). `None` on timeout.
fn capture_marker_pid(
    events: &std::sync::mpsc::Receiver<CoreEvent>,
    id: &str,
    marker: &str,
) -> Option<u32> {
    let mut acc = String::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        match events.recv_timeout(Duration::from_millis(200)) {
            Ok(CoreEvent::TerminalOutput { id: i, bytes_b64, .. }) if i == id => {
                if let Ok(b) = base64::engine::general_purpose::STANDARD.decode(&bytes_b64) {
                    acc.push_str(&String::from_utf8_lossy(&b));
                }
                if let Some(pos) = acc.find(marker) {
                    let digits: String = acc[pos + marker.len()..]
                        .chars()
                        .skip_while(|c| !c.is_ascii_digit())
                        .take_while(|c| c.is_ascii_digit())
                        .collect();
                    if !digits.is_empty() {
                        // Ensure we captured the WHOLE number (a trailing non-digit followed it).
                        let tail = &acc[pos + marker.len()..];
                        if tail.chars().skip_while(|c| !c.is_ascii_digit()).nth(digits.len())
                            .map(|c| !c.is_ascii_digit())
                            .unwrap_or(false)
                        {
                            return digits.parse().ok();
                        }
                    }
                }
            }
            Ok(_) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => break,
        }
    }
    None
}

/// Best-effort liveness check for a pid via `kill -0` (cross-unix; no libc dep in the test). True iff
/// the process still exists.
fn pid_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Open a terminal, retrying a few times on a TRANSIENT spawn failure (e.g. `posix_spawn`/`fork`
/// EAGAIN under heavy parallel-test load). The SUT correctly SURFACES the OS error (rather than
/// hanging), so this only hardens the test itself against a loaded machine; it panics only if the
/// open never succeeds.
fn open_retry(core: &Core, cmd: Vec<String>) -> String {
    let mut last = String::new();
    for attempt in 0..5u64 {
        match core.open_terminal(std::env::temp_dir(), Some(cmd.clone()), 80, 24, false) {
            Ok(id) => return id,
            Err(e) => {
                last = e.to_string();
                std::thread::sleep(Duration::from_millis(100 * (attempt + 1)));
            }
        }
    }
    panic!("open_terminal failed after retries: {last}");
}

#[test]
fn pty_roundtrip_and_reap() {
    // Keep the memory engine offline in CI (no model download).
    std::env::set_var("WICKED_MEMORY_EMBEDDER", "hash");

    let core = Core::spawn(unique_db());
    let events = core.subscribe();

    // Open a PTY running `cat` (echoes stdin → stdout; PTY line-discipline also echoes input).
    let id = core
        .open_terminal(std::env::temp_dir(), Some(vec!["cat".to_string()]), 80, 24, false)
        .expect("open_terminal");

    wait_for(&events, "TerminalOpened", |e| {
        matches!(e, CoreEvent::TerminalOpened { id: i, .. } if *i == id)
    });

    // Write input; assert the echoed bytes come back over TerminalOutput (base64-decoded).
    core.write_terminal(&id, b"hello-pty\n").expect("write_terminal");
    wait_for(&events, "TerminalOutput containing 'hello-pty'", |e| match e {
        CoreEvent::TerminalOutput { id: i, bytes_b64, .. } if *i == id => base64::engine::general_purpose::STANDARD
            .decode(bytes_b64)
            .map(|b| String::from_utf8_lossy(&b).contains("hello-pty"))
            .unwrap_or(false),
        _ => false,
    });

    // Resize must not error while the session is live.
    core.resize_terminal(&id, 100, 40).expect("resize_terminal");

    // Close → child reaped, reader joined, TerminalExited fires (once).
    core.close_terminal(&id).expect("close_terminal");
    wait_for(&events, "TerminalExited", |e| {
        matches!(e, CoreEvent::TerminalExited { id: i, .. } if *i == id)
    });

    // No second TerminalExited for the same id (idempotent finish — a late TerminalReaderDone
    // after CloseTerminal must be a no-op).
    let mut extra_exit = false;
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    while std::time::Instant::now() < deadline {
        match events.recv_timeout(Duration::from_millis(100)) {
            Ok(CoreEvent::TerminalExited { id: i, .. }) if i == id => {
                extra_exit = true;
                break;
            }
            Ok(_) => continue,
            Err(_) => continue,
        }
    }
    assert!(!extra_exit, "TerminalExited double-fired for the same terminal id");
}

/// CRIT-1 (the load-bearing test the `cat` round-trip can't surface): a child that leaves a
/// backgrounded DESCENDANT holding the PTY slave. `Child::kill` sends SIGHUP to the direct child
/// only, so the descendant keeps the master open, the reader's `read()` never EOFs, and an untimed
/// `join()` would wedge the actor FOREVER (and orphan the descendant). With the process-GROUP kill +
/// bounded join, `close_terminal` must (a) return promptly and (b) leave no orphan.
#[test]
fn close_terminal_kills_process_group_no_orphan_no_hang() {
    std::env::set_var("WICKED_MEMORY_EMBEDDER", "hash");
    let core = Core::spawn(unique_db());
    let events = core.subscribe();

    // `trap "" HUP` makes the shell (and everything it forks/execs — SIG_IGN survives exec) IGNORE
    // SIGHUP. `sleep 60 &` backgrounds a descendant holding the PTY slave; we print its pid; then
    // `exec sleep 60` replaces the shell so the direct child also blocks holding the slave. Because
    // both now ignore SIGHUP, `Child::kill` (SIGHUP to the direct child only) would leave BOTH alive
    // — the master never EOFs and the old untimed teardown wedges forever. Job control is off under
    // `sh -c`, so both stay in the child's process group, so the killpg SIGTERM→SIGKILL escalation
    // (which they can't ignore) reaps them.
    let id = open_retry(
        &core,
        vec![
            "sh".to_string(),
            "-c".to_string(),
            "trap \"\" HUP; sleep 60 & echo BGPID=$!; exec sleep 60".to_string(),
        ],
    );

    wait_for(&events, "TerminalOpened", |e| {
        matches!(e, CoreEvent::TerminalOpened { id: i, .. } if *i == id)
    });

    // Grab the backgrounded descendant's pid so we can assert it's dead after close.
    let bg_pid = capture_marker_pid(&events, &id, "BGPID=");

    // THE assertion: close must return promptly — not hang on a wedged reader join.
    let t0 = std::time::Instant::now();
    core.close_terminal(&id).expect("close_terminal");
    let elapsed = t0.elapsed();
    assert!(
        elapsed < Duration::from_secs(8),
        "close_terminal hung ({elapsed:?}) — the reader join was not bounded / the group was not killed"
    );

    wait_for(&events, "TerminalExited", |e| {
        matches!(e, CoreEvent::TerminalExited { id: i, .. } if *i == id)
    });

    // No orphan: the backgrounded descendant must die (killpg hit the whole group). Best-effort —
    // poll briefly for init to reap it after SIGKILL.
    if let Some(pid) = bg_pid {
        let mut dead = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            if !pid_alive(pid) {
                dead = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(dead, "backgrounded descendant pid {pid} survived close_terminal (orphan)");
    } else {
        eprintln!("warning: could not capture BGPID from PTY output — skipping orphan check (no-hang still asserted)");
    }
}

/// SIG-2: a stuck write (child not draining its stdin) on terminal A must not block close/resize on a
/// DIFFERENT terminal B. With the per-session writer lock (not the global map lock) the stuck write
/// holds only A's writer, so B's close completes promptly.
#[test]
fn stuck_write_does_not_block_other_terminals() {
    std::env::set_var("WICKED_MEMORY_EMBEDDER", "hash");
    let core = Core::spawn(unique_db());
    let events = core.subscribe();

    // A: a child that never reads stdin, so a large write fills the PTY input buffer and BLOCKS.
    let a = open_retry(&core, vec!["sleep".to_string(), "60".to_string()]);
    wait_for(&events, "A opened", |e| {
        matches!(e, CoreEvent::TerminalOpened { id: i, .. } if *i == a)
    });

    // B: the terminal we'll close; must not be blocked by A's stuck write.
    let b = open_retry(&core, vec!["cat".to_string()]);
    wait_for(&events, "B opened", |e| {
        matches!(e, CoreEvent::TerminalOpened { id: i, .. } if *i == b)
    });

    // Kick off a large blocking write to A on another thread (wedges on the full PTY input buffer).
    let core2 = core.clone();
    let a2 = a.clone();
    let writer = std::thread::spawn(move || {
        let blob = vec![b'x'; 2 * 1024 * 1024]; // 2 MiB — far exceeds any PTY input buffer (KB-scale)
        let _ = core2.write_terminal(&a2, &blob); // expected to block until A is closed/killed
    });

    // Give the write time to actually block on the full buffer.
    std::thread::sleep(Duration::from_millis(400));

    // Close B: must complete promptly despite A's stuck write (SIG-2).
    let t0 = std::time::Instant::now();
    core.close_terminal(&b).expect("close B");
    let elapsed = t0.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "close_terminal(B) was blocked ({elapsed:?}) by the stuck write on A — the map lock was held across the write"
    );
    wait_for(&events, "B exited", |e| {
        matches!(e, CoreEvent::TerminalExited { id: i, .. } if *i == b)
    });

    // Cleanup: closing A kills the child, which unblocks (errors) the stuck write so the thread ends.
    let _ = core.close_terminal(&a);
    let _ = writer.join();
}
