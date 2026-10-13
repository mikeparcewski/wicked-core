//! Headless multi-turn (slice C2): the ACP-free carrier keeps each CLI's OWN session across the
//! units of a run, the way the ACP carrier kept one session process per `(run, seat)`.
//!
//! A seat runs in a fresh container per unit (the container floor), but its configuration home —
//! where every CLI stores its sessions — is the seat's worker home on the host, mounted at the
//! same path, so turn 2 can resume turn 1 from a new container (proved per CLI by the container
//! floor spike: one model request carrying turn 1's tool result). The engine chooses the session
//! id up front where the CLI accepts one (claude `--session-id`, pi `--session-id`, opencode
//! `--session`), and reads it from the stream where it does not (codex `thread.started`), then
//! resumes it on the next unit of the same run, seat and posture (a read-only unit never resumes
//! a writer's session, the ACP carrier's F-036 rule).
//!
//! Only in the container floor ([`crate::container_floor::container_mode`]): on the host the
//! wrapped carrier stays single-shot, as before.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use wicked_apps_core::spawn::SeatCli;

use crate::execute_wrapped::{AdapterOut, OutputAdapter};

/// One run's session on one seat, in one posture.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct SessionKey {
    pub(crate) run_id: String,
    pub(crate) cli_key: String,
    /// Whether the unit's writes are fenced (read-only / deliverable roots): never shares a
    /// session with a write-posture unit.
    pub(crate) fenced: bool,
}

/// The sessions the carrier can resume, process-wide (the engine has one wrapped carrier).
static SESSIONS: Mutex<Option<HashMap<SessionKey, String>>> = Mutex::new(None);

/// The session to resume for `key`, when an earlier unit completed one.
pub(crate) fn resumable(key: &SessionKey) -> Option<String> {
    SESSIONS
        .lock()
        .ok()
        .and_then(|g| g.as_ref().and_then(|m| m.get(key).cloned()))
}

/// Remember `id` as `key`'s session (after a completed turn).
pub(crate) fn remember(key: SessionKey, id: String) {
    if let Ok(mut g) = SESSIONS.lock() {
        g.get_or_insert_with(HashMap::new).insert(key, id);
    }
}

/// Forget `key`'s session (a failed or cancelled turn: the next unit starts fresh).
pub(crate) fn forget(key: &SessionKey) {
    if let Ok(mut g) = SESSIONS.lock() {
        if let Some(m) = g.as_mut() {
            m.remove(key);
        }
    }
}

/// Forget every session of `run_id` (the run ended or was cancelled).
pub(crate) fn forget_run(run_id: &str) {
    if let Ok(mut g) = SESSIONS.lock() {
        if let Some(m) = g.as_mut() {
            m.retain(|k, _| k.run_id != run_id);
        }
    }
}

/// A fresh engine-chosen session id in the spelling `cli` accepts, or `None` when the CLI picks
/// its own (codex).
pub(crate) fn fresh_id(cli: SeatCli) -> Option<String> {
    let u = uuid::Uuid::new_v4();
    match cli {
        SeatCli::Claude | SeatCli::Pi => Some(u.to_string()),
        // opencode session ids are `ses_`-prefixed.
        SeatCli::Opencode => Some(format!("ses_{}", &u.simple().to_string()[..26])),
        _ => None,
    }
}

/// Insert `flag value` before a `--` guard (or at the end).
fn insert_flag(argv: &mut Vec<String>, flag: &str, value: &str) {
    let at = argv.iter().position(|a| a == "--").unwrap_or(argv.len());
    argv.insert(at, value.to_string());
    argv.insert(at, flag.to_string());
}

/// Whether the template already chose a session flag (the engine never overrides one).
fn names_a_session(argv: &[String]) -> bool {
    let flags = &argv[..argv.iter().position(|a| a == "--").unwrap_or(argv.len())];
    // (`-c`/`-s` are NOT session flags here: codex spells config and sandbox with them.)
    flags.iter().any(|a| {
        matches!(
            a.split('=').next().unwrap_or(a),
            "--session-id" | "--resume" | "-r" | "--continue" | "--session"
        )
    })
}

/// Put `cli`'s session on `argv`: resume `prior` when there is one, else start the engine's
/// `fresh` id. Returns the id the turn runs under when the engine knows it up front (`None` for
/// codex's first turn, whose id is read from its stream, and for a CLI or argv shape this does
/// not handle — which runs exactly as before).
pub(crate) fn inject_session(
    cli: SeatCli,
    argv: &mut Vec<String>,
    prior: Option<&str>,
    fresh: Option<&str>,
) -> Option<String> {
    if argv.is_empty() || names_a_session(argv) {
        return None;
    }
    match cli {
        SeatCli::Claude => match (prior, fresh) {
            (Some(id), _) => {
                insert_flag(argv, "--resume", id);
                Some(id.to_string())
            }
            (None, Some(id)) => {
                insert_flag(argv, "--session-id", id);
                Some(id.to_string())
            }
            _ => None,
        },
        SeatCli::Pi => {
            let id = prior.or(fresh)?;
            insert_flag(argv, "--session-id", id);
            Some(id.to_string())
        }
        SeatCli::Opencode => {
            // Only `opencode run` takes a session.
            if argv.get(1).map(String::as_str) != Some("run") {
                return None;
            }
            let id = prior.or(fresh)?;
            insert_flag(argv, "--session", id);
            Some(id.to_string())
        }
        SeatCli::Codex => {
            let id = prior?;
            // `codex exec [opts] <prompt>` → `codex exec resume <id> [opts] <prompt>`; the
            // resume subcommand takes no `--sandbox`/`-s`, so the sandbox mode rides `-c`.
            if argv.get(1).map(String::as_str) != Some("exec") {
                return None;
            }
            let mut rest: Vec<String> = Vec::new();
            let mut i = 2;
            while i < argv.len() {
                let a = &argv[i];
                if (a == "-s" || a == "--sandbox") && i + 1 < argv.len() {
                    rest.push("-c".into());
                    rest.push(format!("sandbox_mode=\"{}\"", argv[i + 1]));
                    i += 2;
                    continue;
                }
                if let Some(mode) = a.strip_prefix("--sandbox=") {
                    rest.push("-c".into());
                    rest.push(format!("sandbox_mode=\"{mode}\""));
                    i += 1;
                    continue;
                }
                rest.push(a.clone());
                i += 1;
            }
            let mut out = vec![argv[0].clone(), "exec".into(), "resume".into()];
            // Options before the id would be read as the id by some versions: id first.
            out.push(id.to_string());
            out.extend(rest);
            *argv = out;
            Some(id.to_string())
        }
        _ => None,
    }
}

/// What one turn's stream said about its session.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct TurnSession {
    /// The session id the CLI reported (codex `thread_id`, claude `session_id`, ...).
    pub(crate) id: Option<String>,
    /// Whether the CLI's terminal event arrived (`None`: the CLI has none — opencode).
    pub(crate) completed: Option<bool>,
}

/// An adapter wrapper that reads the session id and the terminal event off the raw stream and
/// otherwise forwards every line unchanged.
pub(crate) struct SessionTap {
    inner: Box<dyn OutputAdapter>,
    cli: SeatCli,
    seen: Arc<Mutex<TurnSession>>,
}

impl SessionTap {
    pub(crate) fn new(
        inner: Box<dyn OutputAdapter>,
        cli: SeatCli,
    ) -> (Self, Arc<Mutex<TurnSession>>) {
        let seen = Arc::new(Mutex::new(TurnSession {
            id: None,
            completed: crate::execute_wrapped::headless_turn_completed(cli, &[]),
        }));
        (
            Self {
                inner,
                cli,
                seen: Arc::clone(&seen),
            },
            seen,
        )
    }
}

impl OutputAdapter for SessionTap {
    fn on_line(&mut self, line: &str) -> AdapterOut {
        if let Ok(mut s) = self.seen.lock() {
            if s.id.is_none() {
                s.id = crate::execute_wrapped::headless_session_id(self.cli, line);
            }
            if s.completed == Some(false)
                && crate::execute_wrapped::headless_turn_completed(self.cli, &[line]) == Some(true)
            {
                s.completed = Some(true);
            }
        }
        self.inner.on_line(line)
    }

    fn finish(&mut self) -> AdapterOut {
        self.inner.finish()
    }
}

/// The id to remember after a turn that exited 0: the stream's id, else the engine's; only when
/// the CLI's terminal event arrived (or it has none). `None` = start fresh next time.
pub(crate) fn session_after_turn(engine_id: Option<&str>, seen: &TurnSession) -> Option<String> {
    if seen.completed == Some(false) {
        return None;
    }
    seen.id.clone().or_else(|| engine_id.map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn claude_starts_an_engine_session_then_resumes_it() {
        let mut a = s(&["claude", "-p", "--", "do it"]);
        assert_eq!(
            inject_session(SeatCli::Claude, &mut a, None, Some("u-1")).as_deref(),
            Some("u-1")
        );
        assert_eq!(
            a,
            s(&["claude", "-p", "--session-id", "u-1", "--", "do it"])
        );
        let mut b = s(&["claude", "-p", "--", "next"]);
        inject_session(SeatCli::Claude, &mut b, Some("u-1"), Some("u-2"));
        assert_eq!(b, s(&["claude", "-p", "--resume", "u-1", "--", "next"]));
    }

    #[test]
    fn pi_and_opencode_reuse_one_engine_id() {
        let mut a = s(&["pi", "-p", "--mode", "json", "--", "x"]);
        inject_session(SeatCli::Pi, &mut a, Some("p-1"), Some("p-2"));
        assert_eq!(
            a,
            s(&[
                "pi",
                "-p",
                "--mode",
                "json",
                "--session-id",
                "p-1",
                "--",
                "x"
            ])
        );
        let mut o = s(&["opencode", "run", "--format", "json", "x"]);
        inject_session(SeatCli::Opencode, &mut o, None, Some("ses_a"));
        assert_eq!(
            o,
            s(&[
                "opencode",
                "run",
                "--format",
                "json",
                "x",
                "--session",
                "ses_a"
            ])
        );
        let mut serve = s(&["opencode", "serve"]);
        assert!(inject_session(SeatCli::Opencode, &mut serve, None, Some("ses_a")).is_none());
        assert!(fresh_id(SeatCli::Opencode).unwrap().starts_with("ses_"));
        assert!(fresh_id(SeatCli::Codex).is_none());
    }

    #[test]
    fn codex_resumes_by_its_thread_id_with_the_sandbox_on_c() {
        let mut first = s(&["codex", "exec", "--json", "-s", "danger-full-access", "x"]);
        assert!(inject_session(SeatCli::Codex, &mut first, None, None).is_none());
        assert_eq!(first[1], "exec");
        let mut next = s(&["codex", "exec", "--json", "-s", "danger-full-access", "y"]);
        inject_session(SeatCli::Codex, &mut next, Some("t-1"), None);
        assert_eq!(
            next,
            s(&[
                "codex",
                "exec",
                "resume",
                "t-1",
                "--json",
                "-c",
                "sandbox_mode=\"danger-full-access\"",
                "y"
            ])
        );
    }

    #[test]
    fn a_template_that_names_a_session_is_left_alone() {
        let mut a = s(&["claude", "-p", "--resume", "mine", "--", "x"]);
        assert!(inject_session(SeatCli::Claude, &mut a, Some("u"), Some("v")).is_none());
        assert_eq!(a, s(&["claude", "-p", "--resume", "mine", "--", "x"]));
        // A prompt word after `--` is not a flag.
        let mut b = s(&["claude", "-p", "--", "--resume"]);
        assert!(inject_session(SeatCli::Claude, &mut b, None, Some("v")).is_some());
    }

    #[test]
    fn the_tap_reads_the_id_and_terminal_event_from_real_streams() {
        struct Null;
        impl OutputAdapter for Null {
            fn on_line(&mut self, _: &str) -> AdapterOut {
                AdapterOut::default()
            }
        }
        let codex = include_str!("../tests/fixtures/headless/codex-resume-t1.ndjson");
        let (mut tap, seen) = SessionTap::new(Box::new(Null), SeatCli::Codex);
        for l in codex.lines() {
            tap.on_line(l);
        }
        let got = seen.lock().unwrap().clone();
        assert_eq!(
            got.id.as_deref(),
            Some("01a13334-cda7-7b73-aa5d-346bc3b141d4")
        );
        assert_eq!(got.completed, Some(true));
        assert_eq!(
            session_after_turn(None, &got).as_deref(),
            Some("01a13334-cda7-7b73-aa5d-346bc3b141d4")
        );
        // Cut before the terminal event: a cancelled turn is not resumed.
        let (mut tap, seen) = SessionTap::new(Box::new(Null), SeatCli::Codex);
        for l in codex.lines().take(2) {
            tap.on_line(l);
        }
        assert_eq!(session_after_turn(None, &seen.lock().unwrap()), None);
        // opencode has no terminal event: the engine's id stands.
        let (_tap, seen) = SessionTap::new(Box::new(Null), SeatCli::Opencode);
        assert_eq!(
            session_after_turn(Some("ses_x"), &seen.lock().unwrap()).as_deref(),
            Some("ses_x")
        );
    }

    #[test]
    fn sessions_are_kept_per_run_seat_and_posture() {
        let k = |run: &str, fenced| SessionKey {
            run_id: run.into(),
            cli_key: "claude".into(),
            fenced,
        };
        remember(k("r-hs-1", false), "a".into());
        assert_eq!(resumable(&k("r-hs-1", false)).as_deref(), Some("a"));
        assert_eq!(
            resumable(&k("r-hs-1", true)),
            None,
            "a fenced unit never resumes a writer"
        );
        forget(&k("r-hs-1", false));
        assert_eq!(resumable(&k("r-hs-1", false)), None);
        remember(k("r-hs-2", true), "b".into());
        forget_run("r-hs-2");
        assert_eq!(resumable(&k("r-hs-2", true)), None);
    }
}
