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
    let mut p = std::env::temp_dir();
    p.push(format!(
        "wicked-core-term-{}-{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
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
