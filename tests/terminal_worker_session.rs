//! Proves that the PTY terminal infrastructure can drive a PERSISTENT, MULTI-TURN worker session —
//! the foundation for wicked-core#13 (persistent PTY worker sessions to replace one-shot `-p` runs).
//!
//! The key invariant: a single process started in a PTY can receive N input turns and produce N
//! observable output turns WITHOUT restarting. This is what makes prompt-cache reuse possible:
//! the CLI process (claude, codex, or any interactive agent CLI) stays warm across governance-gated
//! turns; the engine writes each turn's prompt to stdin and reads the response from the event stream.
//!
//! Unix-only: relies on PTY line discipline + portable-pty.
#![cfg(unix)]

use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use base64::Engine as _;
use wicked_core::{Core, CoreEvent};

fn unique_db() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!(
        "wicked-core-wkr-{}-{}.db",
        std::process::id(),
        seq
    ));
    p.to_string_lossy().into_owned()
}

fn wait_for(events: &Receiver<CoreEvent>, label: &str, mut pred: impl FnMut(&CoreEvent) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match events.recv_timeout(Duration::from_millis(100)) {
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

/// Drain `TerminalOutput` events for `id` until `marker` appears in the accumulated text, or
/// `timeout` elapses. Returns the accumulated text either way — the caller asserts the marker.
fn collect_until(events: &Receiver<CoreEvent>, id: &str, marker: &str, timeout: Duration) -> String {
    let mut acc = String::new();
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match events.recv_timeout(Duration::from_millis(50)) {
            Ok(CoreEvent::TerminalOutput {
                id: i, bytes_b64, ..
            }) if i == id => {
                if let Ok(b) = base64::engine::general_purpose::STANDARD.decode(&bytes_b64) {
                    acc.push_str(&String::from_utf8_lossy(&b));
                }
                if acc.contains(marker) {
                    return acc;
                }
            }
            Ok(_) => continue,
            Err(_) => continue,
        }
    }
    acc
}

/// Core proof: a single PTY session drives multiple turns of I/O without restarting the process.
///
/// Uses a shell read-loop as a stand-in for any interactive agent CLI. The `WKRTURN:` prefix
/// makes each turn's output unambiguously recognizable in the byte stream, the same way a real
/// CLI's response text would be read from PTY events by the session manager.
#[test]
fn persistent_session_drives_multiple_turns() {
    std::env::set_var("WICKED_MEMORY_EMBEDDER", "hash");

    let core = Core::spawn(unique_db());
    let events = core.subscribe();

    // Interactive process: reads lines from stdin, emits a tagged response per line.
    // Same shape as `claude`, `codex`, or any agent CLI started in interactive mode.
    let id = open_retry(
        &core,
        vec![
            "sh".to_string(),
            "-c".to_string(),
            "while IFS= read -r line; do printf 'WKRTURN:%s\\n' \"$line\"; done".to_string(),
        ],
    );

    wait_for(
        &events,
        "TerminalOpened",
        |e| matches!(e, CoreEvent::TerminalOpened { id: i, .. } if *i == id),
    );

    // Turn 1 — send a prompt, observe the response.
    core.write_terminal(&id, b"alpha\n").expect("write turn 1");
    let out1 = collect_until(&events, &id, "WKRTURN:alpha", Duration::from_secs(5));
    assert!(
        out1.contains("WKRTURN:alpha"),
        "turn 1 response not in output; got: {out1:?}"
    );

    // Turn 2 — same session, NO process restart.
    core.write_terminal(&id, b"beta\n").expect("write turn 2");
    let out2 = collect_until(&events, &id, "WKRTURN:beta", Duration::from_secs(5));
    assert!(
        out2.contains("WKRTURN:beta"),
        "turn 2 response not in output; got: {out2:?}"
    );

    // Turn 3 — prove it holds across N turns, not just 2.
    core.write_terminal(&id, b"gamma\n").expect("write turn 3");
    let out3 = collect_until(&events, &id, "WKRTURN:gamma", Duration::from_secs(5));
    assert!(
        out3.contains("WKRTURN:gamma"),
        "turn 3 response not in output; got: {out3:?}"
    );

    // Session closes cleanly after all turns complete.
    core.close_terminal(&id).expect("close session");
    wait_for(
        &events,
        "TerminalExited",
        |e| matches!(e, CoreEvent::TerminalExited { id: i, .. } if *i == id),
    );
}

/// Queued writes: all turns can be written to stdin BEFORE reading any output.
/// Proves the CLI's input queue (PTY buffer) absorbs them in order — the engine
/// can pipeline prompts without waiting for each response first.
#[test]
fn queued_writes_are_processed_in_order() {
    std::env::set_var("WICKED_MEMORY_EMBEDDER", "hash");

    let core = Core::spawn(unique_db());
    let events = core.subscribe();

    let id = open_retry(
        &core,
        vec![
            "sh".to_string(),
            "-c".to_string(),
            "while IFS= read -r line; do printf 'WKRTURN:%s\\n' \"$line\"; done".to_string(),
        ],
    );

    wait_for(
        &events,
        "TerminalOpened",
        |e| matches!(e, CoreEvent::TerminalOpened { id: i, .. } if *i == id),
    );

    // Write all three turns back-to-back before reading any output.
    core.write_terminal(&id, b"first\n").expect("queue first");
    core.write_terminal(&id, b"second\n").expect("queue second");
    core.write_terminal(&id, b"third\n").expect("queue third");

    // All three must appear in the output stream, in order.
    let out = collect_until(&events, &id, "WKRTURN:third", Duration::from_secs(5));
    let pos_first = out.find("WKRTURN:first").expect("first turn in output");
    let pos_second = out.find("WKRTURN:second").expect("second turn in output");
    let pos_third = out.find("WKRTURN:third").expect("third turn in output");
    assert!(
        pos_first < pos_second && pos_second < pos_third,
        "turns arrived out of order in output stream"
    );

    core.close_terminal(&id).expect("close");
    wait_for(
        &events,
        "TerminalExited",
        |e| matches!(e, CoreEvent::TerminalExited { id: i, .. } if *i == id),
    );
}
