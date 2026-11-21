//! TEST SUPPORT (core#1068): tests that need a loopback listener or a PTY, run inside the seat
//! sandbox.
//!
//! The container seat floor admits `--bind-tcp` only for the seat's own ports and grants
//! `/dev/ptmx` and `/dev/pts` to agy alone (`container_floor::landrun_prefix`, `seat_devices`). A
//! seat running this crate's tests inside it gets `EACCES` on every loopback bind and every
//! `openpty`, so about 50 tests failed before any change. The floor stays as it is; the tests say
//! so instead:
//!
//! - [`skip`] probes once per process (bind `127.0.0.1:0` and connect to it; open a PTY).
//! - When the probe fails with `EACCES` (Unix; Landlock's denial) and `CI` is not in the
//!   environment at all (the seat forwards only `PATH`, `HOME`, `TMPDIR` and `LANG`), the test
//!   returns early after writing `SKIPPED (seat sandbox denies loopback bind|pty): <test>` straight
//!   to the process's stderr, past the test harness's capture, and [`skipped`] counts it.
//! - With `CI` present (any value), or on any other probe error (`EPERM` included), the test
//!   panics, as it would have failed before.
//!
//! Usage, first line of the test:
//! `if crate::test_sandbox::skip(crate::test_sandbox::Need::Loopback, "<test name>") { return; }`
//! (integration tests spell it `wicked_core::test_sandbox::…`).

use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

/// What a test needs from the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Need {
    /// A listener on 127.0.0.1 that the test can connect to.
    Loopback,
    /// A pseudo-terminal pair.
    Pty,
}

impl Need {
    fn label(self) -> &'static str {
        match self {
            Need::Loopback => "loopback bind",
            Need::Pty => "pty",
        }
    }
}

/// Why a probe failed: `denied` is the seat sandbox's `EACCES`; anything else is a real failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeError {
    pub denied: bool,
    pub detail: String,
}

/// What a guarded test does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Run,
    /// Return early; the notice to print.
    Skip(String),
    /// Fail the test with this message.
    Fail(String),
}

static SKIPPED: AtomicUsize = AtomicUsize::new(0);

/// Whether the calling test must return early because the seat sandbox denies `need`. Panics
/// when the probe fails and the skip is not allowed (see the module doc).
pub fn skip(need: Need, test: &str) -> bool {
    let verdict = decide(need, probe(need), std::env::var_os("CI").is_some(), test);
    apply(verdict, &SKIPPED, &mut std::io::stderr().lock())
}

/// Tests skipped so far in this process.
pub fn skipped() -> usize {
    SKIPPED.load(Ordering::SeqCst)
}

/// The decision, apart from the probe and the environment, so it is tested with injected errors.
pub fn decide(need: Need, probe: &Result<(), ProbeError>, ci: bool, test: &str) -> Verdict {
    match probe {
        Ok(()) => Verdict::Run,
        Err(e) if e.denied && !ci => Verdict::Skip(format!(
            "SKIPPED (seat sandbox denies {}): {test}",
            need.label()
        )),
        Err(e) => Verdict::Fail(format!(
            "{test}: the {} probe failed ({}): {}",
            need.label(),
            if e.denied {
                "EACCES, and CI is set: CI is never a seat sandbox"
            } else {
                "not the seat sandbox's EACCES"
            },
            e.detail
        )),
    }
}

/// Carry out a verdict: count and announce a skip, panic on a failure. The notice is written to
/// `out` directly: `std::io::stderr()` is not the harness-captured `eprintln!`, so a passing,
/// skipped test still shows it.
pub fn apply(verdict: Verdict, counter: &AtomicUsize, out: &mut dyn Write) -> bool {
    match verdict {
        Verdict::Run => false,
        Verdict::Skip(notice) => {
            counter.fetch_add(1, Ordering::SeqCst);
            let _ = writeln!(out, "{notice}");
            true
        }
        Verdict::Fail(message) => panic!("{message}"),
    }
}

/// Landlock's denial: `EACCES`, never `EPERM` or any other error. No seat sandbox exists off Unix.
fn is_eacces(code: Option<i32>) -> bool {
    #[cfg(unix)]
    {
        code == Some(libc::EACCES)
    }
    #[cfg(not(unix))]
    {
        let _ = code;
        false
    }
}

fn io_error(e: std::io::Error) -> ProbeError {
    ProbeError {
        denied: is_eacces(e.raw_os_error()),
        detail: e.to_string(),
    }
}

/// portable-pty bails with `failed to openpty: Os { code: 13, kind: PermissionDenied, … }`: the
/// formatted `std::io::Error` is all it keeps, so its `code` is read back from the text.
fn pty_error(detail: String) -> ProbeError {
    let code = detail
        .split("code: ")
        .nth(1)
        .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|digits| digits.parse().ok());
    ProbeError {
        denied: is_eacces(code),
        detail,
    }
}

fn probe(need: Need) -> &'static Result<(), ProbeError> {
    static LOOPBACK: OnceLock<Result<(), ProbeError>> = OnceLock::new();
    static PTY: OnceLock<Result<(), ProbeError>> = OnceLock::new();
    match need {
        Need::Loopback => LOOPBACK.get_or_init(probe_loopback),
        Need::Pty => PTY.get_or_init(probe_pty),
    }
}

fn probe_loopback() -> Result<(), ProbeError> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).map_err(io_error)?;
    let addr = listener.local_addr().map_err(io_error)?;
    std::net::TcpStream::connect(addr).map_err(io_error)?;
    Ok(())
}

fn probe_pty() -> Result<(), ProbeError> {
    let size = portable_pty::PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    };
    match portable_pty::native_pty_system().openpty(size) {
        Ok(_pair) => Ok(()),
        Err(e) => Err(pty_error(format!("{e:#}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn denied() -> Result<(), ProbeError> {
        Err(ProbeError {
            denied: true,
            detail: "Permission denied (os error 13)".into(),
        })
    }

    #[test]
    fn a_denied_probe_without_ci_skips_and_the_skip_is_counted_and_shown() {
        let verdict = decide(Need::Loopback, &denied(), false, "injected");
        assert_eq!(
            verdict,
            Verdict::Skip("SKIPPED (seat sandbox denies loopback bind): injected".into())
        );
        // A counter and sink of its own: the process's real count stays the real one.
        let counter = AtomicUsize::new(0);
        let mut out = Vec::new();
        assert!(apply(verdict, &counter, &mut out));
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "SKIPPED (seat sandbox denies loopback bind): injected\n"
        );
        assert!(matches!(
            decide(Need::Pty, &denied(), false, "injected"),
            Verdict::Skip(n) if n == "SKIPPED (seat sandbox denies pty): injected"
        ));
    }

    #[test]
    #[should_panic(expected = "CI is never a seat sandbox")]
    fn a_denied_probe_with_ci_set_fails_the_test() {
        let verdict = decide(Need::Loopback, &denied(), true, "injected");
        apply(verdict, &AtomicUsize::new(0), &mut Vec::new());
    }

    #[test]
    #[should_panic(expected = "not the seat sandbox's EACCES")]
    fn any_other_probe_error_fails_the_test() {
        let other = Err(ProbeError {
            denied: false,
            detail: "Address not available".into(),
        });
        apply(
            decide(Need::Pty, &other, false, "injected"),
            &AtomicUsize::new(0),
            &mut Vec::new(),
        );
    }

    #[test]
    fn a_working_host_never_skips() {
        let counter = AtomicUsize::new(0);
        for ci in [false, true] {
            let verdict = decide(Need::Loopback, &Ok(()), ci, "injected");
            assert!(!apply(verdict, &counter, &mut Vec::new()));
        }
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    /// Only Landlock's EACCES counts as the seat sandbox; EPERM and the rest fail as before.
    #[cfg(unix)]
    #[test]
    fn only_eacces_classifies_as_the_seat_sandbox() {
        let io = |code| io_error(std::io::Error::from_raw_os_error(code)).denied;
        assert!(io(libc::EACCES));
        assert!(!io(libc::EPERM));
        assert!(!io(libc::EADDRNOTAVAIL));
        // portable-pty's text, as `{e:#}` renders it.
        let pty = |code: i32, kind: &str| {
            pty_error(format!(
                "failed to openpty: Os {{ code: {code}, kind: {kind}, message: \"x\" }}"
            ))
            .denied
        };
        assert!(pty(libc::EACCES, "PermissionDenied"));
        assert!(!pty(libc::EPERM, "PermissionDenied"));
        assert!(!pty(libc::ENOENT, "NotFound"));
        assert!(!pty_error("failed to openpty: no code".into()).denied);
    }
}
