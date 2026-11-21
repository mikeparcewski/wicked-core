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
//! - When the probe is denied with `EACCES` and `CI` is unset (the seat forwards only `PATH`,
//!   `HOME`, `TMPDIR` and `LANG`), the test returns early after printing
//!   `SKIPPED (seat sandbox denies loopback bind|pty): <test>`, and [`skipped`] counts it.
//! - With `CI` set, or on any other probe error, the test panics, as it would have failed before.
//!
//! Usage, first line of the test:
//! `if crate::test_sandbox::skip(crate::test_sandbox::Need::Loopback, "<test name>") { return; }`
//! (integration tests spell it `wicked_core::test_sandbox::…`).

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
#[derive(Debug, Clone)]
pub struct ProbeError {
    pub denied: bool,
    pub detail: String,
}

static SKIPPED: AtomicUsize = AtomicUsize::new(0);

/// Whether the calling test must return early because the seat sandbox denies `need`. Panics
/// when the probe fails and the skip is not allowed (see the module doc).
pub fn skip(need: Need, test: &str) -> bool {
    decide(need, probe(need), ci_set(), test)
}

/// Tests skipped so far in this process.
pub fn skipped() -> usize {
    SKIPPED.load(Ordering::SeqCst)
}

fn ci_set() -> bool {
    std::env::var("CI").is_ok_and(|v| !v.is_empty() && v != "0" && v != "false")
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
    let io = |e: std::io::Error| ProbeError {
        denied: e.kind() == std::io::ErrorKind::PermissionDenied,
        detail: e.to_string(),
    };
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).map_err(io)?;
    let addr = listener.local_addr().map_err(io)?;
    std::net::TcpStream::connect(addr).map_err(io)?;
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
        Err(e) => {
            // portable-pty bails with `failed to openpty: Os { code: 13, kind: PermissionDenied, … }`.
            let detail = format!("{e:#}");
            let denied =
                detail.contains("PermissionDenied") || detail.contains("Permission denied");
            Err(ProbeError { denied, detail })
        }
    }
}

/// The decision, apart from the probe and the environment so it can be tested with injected
/// errors.
pub fn decide(need: Need, probe: &Result<(), ProbeError>, ci: bool, test: &str) -> bool {
    match probe {
        Ok(()) => false,
        Err(e) if e.denied && !ci => {
            SKIPPED.fetch_add(1, Ordering::SeqCst);
            eprintln!("SKIPPED (seat sandbox denies {}): {test}", need.label());
            true
        }
        Err(e) => panic!(
            "{test}: the {} probe failed ({}): {}",
            need.label(),
            if e.denied {
                "denied, and CI is set: CI is never a seat sandbox"
            } else {
                "not a sandbox denial"
            },
            e.detail
        ),
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
    fn a_denied_probe_without_ci_skips_and_is_counted() {
        for need in [Need::Loopback, Need::Pty] {
            let before = skipped();
            assert!(decide(need, &denied(), false, "injected"));
            assert!(skipped() > before, "the skip is counted");
        }
    }

    #[test]
    #[should_panic(expected = "CI is never a seat sandbox")]
    fn a_denied_probe_with_ci_set_fails_the_test() {
        decide(Need::Loopback, &denied(), true, "injected");
    }

    #[test]
    #[should_panic(expected = "not a sandbox denial")]
    fn any_other_probe_error_fails_the_test() {
        let other = Err(ProbeError {
            denied: false,
            detail: "Address not available".into(),
        });
        decide(Need::Pty, &other, false, "injected");
    }

    #[test]
    fn a_working_host_never_skips() {
        assert!(!decide(Need::Loopback, &Ok(()), false, "injected"));
        assert!(!decide(Need::Pty, &Ok(()), true, "injected"));
    }
}
