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

/// One run's session on one seat, in one write posture.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct SessionKey {
    pub(crate) run_id: String,
    pub(crate) cli_key: String,
    /// The unit's write posture (its `Debug` spelling): a read-only or deliverable-roots unit
    /// never shares a session with a writer, nor with each other (codex hl-r1).
    pub(crate) posture: String,
}

#[derive(Debug, Default)]
struct Entry {
    /// The session the next turn may resume; `None` while a turn holds it, or after a failure.
    id: Option<String>,
    /// The turn that holds the key now: only it may record the outcome.
    owner: u64,
    /// The newest launch (the actor's `launch_seq`) admitted on this key: an older launch
    /// arriving late is superseded and never takes the session (codex hl-r2).
    seq: u64,
}

/// How many ended runs are remembered so a late worker of one cannot recreate its entries.
const ENDED_CAP: usize = 4096;

#[derive(Debug, Default)]
struct Registry {
    next_owner: u64,
    keys: HashMap<SessionKey, Entry>,
    ended: std::collections::VecDeque<String>,
    /// A reactivated run's floor: launches at or below it belong to its failed lifecycle.
    floors: HashMap<String, u64>,
}

/// The sessions ONE wrapped runner can resume, shared only by that runner (codex hl-r1: two
/// engines in one process never see each other's runs).
#[derive(Debug, Default)]
pub(crate) struct Sessions {
    inner: Mutex<Registry>,
}

/// One turn's hold on its key. A later turn of the same key takes the hold over (a reassigned
/// unit's replacement), and the superseded turn's outcome is then ignored; a turn never resumes
/// a session another turn still holds (it starts fresh instead).
#[derive(Debug)]
pub(crate) struct Turn {
    key: SessionKey,
    owner: u64,
    /// The session this turn resumes, when an earlier turn completed one.
    pub(crate) prior: Option<String>,
}

impl Sessions {
    /// Take `key`'s session for launch `launch_seq`. A launch of an ended run, or one older than
    /// the newest launch already admitted on the key, gets a turn that owns nothing: it starts
    /// fresh and its outcome is ignored.
    pub(crate) fn begin(&self, key: SessionKey, launch_seq: u64) -> Turn {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let dead = Turn {
            key: key.clone(),
            owner: 0,
            prior: None,
        };
        if g.ended.contains(&key.run_id)
            || g.floors.get(&key.run_id).is_some_and(|&f| launch_seq <= f)
        {
            return dead;
        }
        g.next_owner += 1;
        let owner = g.next_owner;
        let e = g.keys.entry(key.clone()).or_default();
        if launch_seq < e.seq {
            return dead;
        }
        e.seq = launch_seq;
        let prior = e.id.take();
        e.owner = owner;
        Turn { key, owner, prior }
    }

    /// Record `turn`'s outcome: `Some(id)` is resumable by the next turn, `None` starts it
    /// fresh. Ignored when a later turn took the key over or the run ended meanwhile.
    pub(crate) fn finish(&self, turn: Turn, id: Option<String>) {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(e) = g.keys.get_mut(&turn.key) {
            if turn.owner != 0 && e.owner == turn.owner {
                e.id = id;
            }
        }
    }

    /// Forget every session of `run_id` (the run ended or was cancelled) and refuse its late
    /// launches, so the map holds only live runs.
    pub(crate) fn forget_run(&self, run_id: &str) {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.keys.retain(|k, _| k.run_id != run_id);
        g.floors.remove(run_id);
        if !g.ended.iter().any(|r| r == run_id) {
            if g.ended.len() >= ENDED_CAP {
                g.ended.pop_front();
            }
            g.ended.push_back(run_id.to_string());
        }
    }

    /// Admit `run_id`'s launches again (a failed run was reactivated: codex hl-r4).
    /// `min_launch_seq` is the failed lifecycle's last launch: nothing at or below it is admitted.
    pub(crate) fn reopen_run(&self, run_id: &str, min_launch_seq: u64) {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.ended.retain(|r| r != run_id);
        g.floors.insert(run_id.to_string(), min_launch_seq);
    }

    /// Forget `run_id`'s sessions on seat `cli_key` (the unit moved to another seat).
    pub(crate) fn forget_seat(&self, run_id: &str, cli_key: &str) {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.keys
            .retain(|k, _| k.run_id != run_id || k.cli_key != cli_key);
    }
}

/// Whether `id` is a session id in the spelling `cli` uses: a UUID (claude, codex, pi) or
/// `ses_<alphanumerics>` (opencode). A stream-supplied id becomes an argv element on the next
/// turn, so nothing else is ever remembered (codex hl-r1: `--last` would select another run's
/// session).
pub(crate) fn valid_session_id(cli: SeatCli, id: &str) -> bool {
    match cli {
        // agy's `conversation_id` is a UUID too.
        SeatCli::Claude | SeatCli::Codex | SeatCli::Pi | SeatCli::Agy => {
            uuid::Uuid::parse_str(id).is_ok()
        }
        SeatCli::Opencode => id
            .strip_prefix("ses_")
            .is_some_and(|r| !r.is_empty() && r.chars().all(|c| c.is_ascii_alphanumeric())),
        _ => false,
    }
}

/// A fresh engine-chosen session id in the spelling `cli` accepts, or `None` when the CLI picks
/// its own (codex; agy, whose `--conversation` resumes only a conversation it created).
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

/// Whether the template already chose a session flag (the engine never overrides one). The
/// spellings are per CLI (codex hl-r3): `-c` is claude's, pi's and opencode's `--continue` but
/// codex's config flag, and `-s` is opencode's `--session` but codex's sandbox.
fn names_a_session(cli: SeatCli, argv: &[String]) -> bool {
    // Only the CLI's own arguments: the governed opencode wrapper's `/bin/sh -c SCRIPT tag oc
    // port` prefix is not opencode's (its `-c` is the shell's).
    let own = if argv.get(3).map(String::as_str) == Some("wicked-opencode-floor") {
        argv.get(6..).unwrap_or_default()
    } else {
        argv.get(1..).unwrap_or_default()
    };
    let flags = &own[..own.iter().position(|a| a == "--").unwrap_or(own.len())];
    // codex's own `exec resume <id>` subcommand is a template-chosen session (codex hl-r4).
    if cli == SeatCli::Codex && flags.iter().any(|a| a == "resume") {
        return true;
    }
    let short: &[&str] = match cli {
        SeatCli::Codex => &[],
        SeatCli::Opencode => &["-c", "-s"],
        // agy's `-c` is `--continue`; it has no `-r`.
        SeatCli::Agy => &["-c"],
        _ => &["-c", "-r"],
    };
    flags.iter().any(|a| {
        let f = a.split('=').next().unwrap_or(a);
        matches!(
            f,
            "--session-id" | "--resume" | "--continue" | "--session" | "--conversation"
        ) || short.contains(&f)
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
    if argv.is_empty() || names_a_session(cli, argv) {
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
            // Only `opencode run` takes a session: as launched, or inside the governed floor's
            // pinned-server wrapper, which forwards its trailing arguments to `opencode run`
            // (`worker_sandbox::opencode_pinned_server_argv`; agy hl-r1: resume was dead there).
            let run = argv.get(1).map(String::as_str) == Some("run")
                || argv.get(3).map(String::as_str) == Some("wicked-opencode-floor");
            if !run {
                return None;
            }
            let id = prior.or(fresh)?;
            insert_flag(argv, "--session", id);
            Some(id.to_string())
        }
        SeatCli::Agy => {
            // agy picks the conversation id (its `init` event names it); a later turn resumes it
            // with `--conversation <id>`, right after the binary like every engine flag on an agy
            // argv (never after the glued `-p=<prompt>`, `execute_wrapped::inject_agy_headless`).
            let id = prior?;
            argv.splice(1..1, ["--conversation".to_string(), id.to_string()]);
            Some(id.to_string())
        }
        SeatCli::Codex => {
            let id = prior?;
            // `codex exec [opts] [--] <prompt>` → `codex exec [opts] resume <id> [--] <prompt>`:
            // every option stays in `exec`'s scope, before the subcommand (codex hl-r1: `-C` and
            // `--add-dir` are not `resume` options); only the id and the prompt follow it.
            if argv.get(1).map(String::as_str) != Some("exec") {
                return None;
            }
            let guard = argv.iter().position(|a| a == "--");
            // Without a `--` guard the prompt's position is unknown (flags may follow it: agy
            // hl-r1), so such a template starts a fresh session instead of resuming.
            let g = guard?;
            let (opts, prompt) = (argv[2..g].to_vec(), argv[g..].to_vec());
            let mut out = vec![argv[0].clone(), "exec".into()];
            out.extend(opts);
            out.push("resume".into());
            out.push(id.to_string());
            out.extend(prompt);
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
            // The LAST turn of the invocation is judged (codex hl-r2: a prompt after a
            // successful bootstrap prompt may still fail): a turn start resets it, its terminal
            // event decides it.
            if s.completed.is_some() {
                if let Some(done) = crate::execute_wrapped::headless_turn_event(self.cli, line) {
                    s.completed = Some(done);
                }
            }
            // opencode has no end-of-turn event, but an `error` event fails the turn even when
            // the process exits 0 (agy hl-r1).
            if self.cli == SeatCli::Opencode
                && crate::execute_wrapped::opencode_error(line).is_some()
            {
                s.completed = Some(false);
            }
        }
        self.inner.on_line(line)
    }

    fn finish(&mut self) -> AdapterOut {
        self.inner.finish()
    }
}

/// The id to remember after a turn that exited 0: the engine's own id when it chose one (a
/// stream that names another is not trusted), else the stream's (codex), only in the CLI's
/// grammar; only when the CLI's terminal event arrived (or it has none). `None` = start fresh.
pub(crate) fn session_after_turn(
    cli: SeatCli,
    engine_id: Option<&str>,
    seen: &TurnSession,
) -> Option<String> {
    if seen.completed == Some(false) {
        return None;
    }
    // Only codex and agy pick their own ids; for the others an id the engine did not inject is
    // never adopted from the stream (agy hl-r1).
    let id = match (engine_id, cli) {
        (Some(e), _) => e.to_string(),
        (None, SeatCli::Codex | SeatCli::Agy) => seen.id.clone()?,
        (None, _) => return None,
    };
    valid_session_id(cli, &id).then_some(id)
}

pub(crate) fn settle_cut_off_turn(seen: &TurnSession, code: &mut i32, out: &mut String, cli: &str) {
    if seen.completed == Some(false) && *code == 0 {
        *code = 1;
        out.push_str(&format!(
            "\nwicked-core: the {cli} turn ended without completing (no end-of-turn event, or an \
             error event: cancelled, cut off or failed); it is not counted as done"
        ));
    }
}

/// An agy turn its own `--print-timeout` cut off: agy still exits 0 with a `SUCCESS` result, so
/// only its stderr notice says the turn did not finish ([`crate::execute_wrapped::agy_print_timed_out`]).
/// Such a turn is not done: exit 1, with the reason.
pub(crate) fn settle_agy_print_timeout(stderr: &str, code: &mut i32, out: &mut String) {
    if *code == 0 && crate::execute_wrapped::agy_print_timed_out(stderr) {
        *code = 1;
        out.push_str(
            "\nwicked-core: the agy turn reached its --print-timeout before it finished (agy \
             returned its partial output); it is not counted as done",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Null;
    impl OutputAdapter for Null {
        fn on_line(&mut self, _: &str) -> AdapterOut {
            AdapterOut::default()
        }
    }

    #[test]
    fn a_turn_that_exits_zero_without_its_terminal_event_is_not_done() {
        let capture = include_str!("../tests/fixtures/headless/codex-resume-t1.ndjson");
        let lines: Vec<&str> = capture.lines().collect();
        let at = lines
            .iter()
            .position(|l| l.contains("\"turn.completed\""))
            .expect("the capture completes");
        for (feed, done) in [(&lines[..], true), (&lines[..at], false)] {
            let (mut tap, seen) = SessionTap::new(Box::new(Null), SeatCli::Codex);
            for l in feed {
                tap.on_line(l);
            }
            let (mut code, mut out) = (0, String::from("partial"));
            settle_cut_off_turn(&seen.lock().unwrap(), &mut code, &mut out, "codex");
            assert_eq!(code == 0, done, "{out}");
            assert_eq!(out.contains("not counted as done"), !done);
        }
        // A failed exit keeps its own code; opencode (no terminal event) is judged by exit code.
        let (_tap, seen) = SessionTap::new(Box::new(Null), SeatCli::Codex);
        let (mut code, mut out) = (7, String::new());
        settle_cut_off_turn(&seen.lock().unwrap(), &mut code, &mut out, "codex");
        assert_eq!((code, out.as_str()), (7, ""));
        let (_tap, seen) = SessionTap::new(Box::new(Null), SeatCli::Opencode);
        let (mut code, mut out) = (0, String::new());
        settle_cut_off_turn(&seen.lock().unwrap(), &mut code, &mut out, "opencode");
        assert_eq!(code, 0);
    }

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
        // The governed floor's pinned-server wrapper forwards its tail to `opencode run`.
        let mut wrapped = s(&[
            "/bin/sh",
            "-c",
            "script",
            "wicked-opencode-floor",
            "opencode",
            "4242",
            "--format",
            "json",
            "--",
            "x",
        ]);
        assert_eq!(
            inject_session(SeatCli::Opencode, &mut wrapped, Some("ses_b"), None).as_deref(),
            Some("ses_b")
        );
        assert_eq!(
            &wrapped[6..],
            &s(&["--format", "json", "--session", "ses_b", "--", "x"])[..]
        );
        let mut serve = s(&["opencode", "serve"]);
        assert!(inject_session(SeatCli::Opencode, &mut serve, None, Some("ses_a")).is_none());
        assert!(fresh_id(SeatCli::Opencode).unwrap().starts_with("ses_"));
        assert!(fresh_id(SeatCli::Codex).is_none());
    }

    #[test]
    fn codex_resumes_by_its_thread_id_with_every_option_in_execs_scope() {
        let mut first = s(&["codex", "exec", "--json", "-s", "danger-full-access", "x"]);
        assert!(inject_session(SeatCli::Codex, &mut first, None, None).is_none());
        assert_eq!(first[1], "exec");
        let t = "01a13334-cda7-7b73-aa5d-346bc3b141d4";
        let mut next = s(&[
            "codex",
            "exec",
            "--json",
            "-s",
            "danger-full-access",
            "--",
            "y",
        ]);
        inject_session(SeatCli::Codex, &mut next, Some(t), None);
        assert_eq!(
            next,
            s(&[
                "codex",
                "exec",
                "--json",
                "-s",
                "danger-full-access",
                "resume",
                t,
                "--",
                "y"
            ])
        );
        // agy hl-r1: with no guard the prompt's place is unknown (a flag may follow it): fresh.
        let mut unguarded = s(&["codex", "exec", "-m", "o3", "fix it", "--json"]);
        assert!(inject_session(SeatCli::Codex, &mut unguarded, Some(t), None).is_none());
        assert_eq!(
            unguarded,
            s(&["codex", "exec", "-m", "o3", "fix it", "--json"])
        );
        // codex hl-r1: the fenced launch's `-C` / `--add-dir` stay before `resume`.
        let mut fenced = s(&[
            "codex",
            "exec",
            "--json",
            "-C",
            "/scratch",
            "--add-dir",
            "/notes",
            "--",
            "z",
        ]);
        inject_session(SeatCli::Codex, &mut fenced, Some(t), None);
        assert_eq!(
            fenced,
            s(&[
                "codex",
                "exec",
                "--json",
                "-C",
                "/scratch",
                "--add-dir",
                "/notes",
                "resume",
                t,
                "--",
                "z"
            ])
        );
    }

    /// codex hl-r3: `-c` is pi's (and claude's) `--continue`, which pi refuses beside
    /// `--session-id`; codex's `-c`/`-s` (config, sandbox) still allow injection.
    #[test]
    fn session_flags_are_read_per_cli() {
        let mut p = s(&["pi", "-p", "-c", "--", "x"]);
        assert!(inject_session(SeatCli::Pi, &mut p, None, Some("p-1")).is_none());
        assert_eq!(p, s(&["pi", "-p", "-c", "--", "x"]));
        let mut c = s(&["claude", "-p", "-c", "--", "x"]);
        assert!(inject_session(SeatCli::Claude, &mut c, None, Some("u")).is_none());
        let mut o = s(&["opencode", "run", "-s", "ses_mine", "x"]);
        assert!(inject_session(SeatCli::Opencode, &mut o, None, Some("ses_a")).is_none());
        let t = "01a13334-cda7-7b73-aa5d-346bc3b141d4";
        let mut x = s(&["codex", "exec", "-c", "k=v", "-s", "read-only", "--", "x"]);
        assert!(inject_session(SeatCli::Codex, &mut x, Some(t), None).is_some());
        // codex hl-r4: an explicit `exec resume <id>` template is left alone.
        let mut r = s(&["codex", "exec", "--json", "resume", t, "--", "x"]);
        assert!(inject_session(SeatCli::Codex, &mut r, Some(t), None).is_none());
        assert_eq!(r, s(&["codex", "exec", "--json", "resume", t, "--", "x"]));
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
            session_after_turn(SeatCli::Codex, None, &got).as_deref(),
            Some("01a13334-cda7-7b73-aa5d-346bc3b141d4")
        );
        // Cut before the terminal event: a cancelled turn is not resumed.
        let (mut tap, seen) = SessionTap::new(Box::new(Null), SeatCli::Codex);
        for l in codex.lines().take(2) {
            tap.on_line(l);
        }
        assert_eq!(
            session_after_turn(SeatCli::Codex, None, &seen.lock().unwrap()),
            None
        );
        // opencode has no terminal event: the engine's id stands.
        let (_tap, seen) = SessionTap::new(Box::new(Null), SeatCli::Opencode);
        assert_eq!(
            session_after_turn(SeatCli::Opencode, Some("ses_x"), &seen.lock().unwrap()).as_deref(),
            Some("ses_x")
        );
    }

    #[test]
    fn sessions_are_kept_per_run_seat_and_posture() {
        let k = |run: &str, posture: &str| SessionKey {
            run_id: run.into(),
            cli_key: "claude".into(),
            posture: posture.into(),
        };
        let reg = Sessions::default();
        let t = reg.begin(k("r1", "Full"), 1);
        assert_eq!(t.prior, None);
        reg.finish(t, Some("a".into()));
        let t = reg.begin(k("r1", "Full"), 1);
        assert_eq!(t.prior.as_deref(), Some("a"));
        // Another posture (codex hl-r1: deliverable roots vs read-only too) never resumes it.
        assert_eq!(reg.begin(k("r1", "ReadOnly"), 1).prior, None);
        assert_eq!(reg.begin(k("r1", "DeliverableRoots"), 1).prior, None);
        // A failed turn forgets it.
        reg.finish(t, None);
        assert_eq!(reg.begin(k("r1", "Full"), 1).prior, None);
        // Another runner (another engine in the process) never sees this one's sessions.
        let other = Sessions::default();
        let t = reg.begin(k("r2", "Full"), 1);
        reg.finish(t, Some("b".into()));
        assert_eq!(other.begin(k("r2", "Full"), 1).prior, None);
        // The run ended: its late outcome is ignored, nothing is resurrected.
        let late = reg.begin(k("r2", "Full"), 1);
        assert_eq!(late.prior.as_deref(), Some("b"));
        reg.forget_run("r2");
        reg.finish(late, Some("c".into()));
        assert_eq!(reg.begin(k("r2", "Full"), 1).prior, None);
    }

    /// codex hl-r1: a reassigned unit's replacement takes the hold; it never resumes a session
    /// the superseded turn still holds, and the superseded turn's outcome is ignored.
    #[test]
    fn a_superseded_turn_never_overwrites_its_replacement() {
        let k = SessionKey {
            run_id: "r".into(),
            cli_key: "codex".into(),
            posture: "Full".into(),
        };
        let reg = Sessions::default();
        let t = reg.begin(k.clone(), 1);
        reg.finish(t, Some("s".into()));
        let old = reg.begin(k.clone(), 1);
        assert_eq!(old.prior.as_deref(), Some("s"));
        let new = reg.begin(k.clone(), 1);
        assert_eq!(new.prior, None, "no overlapping resume of one session");
        reg.finish(new, Some("n".into()));
        reg.finish(old, None);
        assert_eq!(reg.begin(k.clone(), 1).prior.as_deref(), Some("n"));
        reg.forget_seat("r", "codex");
        assert_eq!(reg.begin(k, 1).prior, None);
    }

    /// codex hl-r2: an invocation's LAST turn is judged: a bootstrap prompt that succeeded never
    /// masks a later prompt that aborted, or one cut off before its end.
    #[test]
    fn the_last_turn_of_an_invocation_is_judged() {
        let end = |stop: &str| {
            format!(
                r#"{{"type":"agent_end","messages":[{{"role":"assistant","stopReason":"{stop}"}}]}}"#
            )
        };
        let start = r#"{"type":"agent_start"}"#;
        for (feed, done) in [
            (vec![start.to_string(), end("stop")], true),
            (
                vec![
                    start.to_string(),
                    end("stop"),
                    start.to_string(),
                    end("aborted"),
                ],
                false,
            ),
            (
                vec![start.to_string(), end("stop"), start.to_string()],
                false,
            ),
            (
                vec![
                    start.to_string(),
                    end("error"),
                    start.to_string(),
                    end("stop"),
                ],
                true,
            ),
        ] {
            let (mut tap, seen) = SessionTap::new(Box::new(Null), SeatCli::Pi);
            for l in &feed {
                tap.on_line(l);
            }
            assert_eq!(seen.lock().unwrap().completed, Some(done), "{feed:?}");
        }
    }

    /// codex hl-r2: a launch older than the newest one admitted on its key (a superseded worker
    /// arriving late) and a launch of an ended run own nothing: they start fresh and their
    /// outcome is ignored, so neither clears nor recreates a session.
    #[test]
    fn a_late_or_ended_launch_owns_nothing() {
        let k = SessionKey {
            run_id: "r".into(),
            cli_key: "codex".into(),
            posture: "Full".into(),
        };
        let reg = Sessions::default();
        let b = reg.begin(k.clone(), 5);
        reg.finish(b, Some("s".into()));
        let late = reg.begin(k.clone(), 4);
        assert_eq!(late.prior, None);
        reg.finish(late, None);
        assert_eq!(reg.begin(k.clone(), 6).prior.as_deref(), Some("s"));
        reg.forget_run("r");
        let after = reg.begin(k.clone(), 7);
        assert_eq!(after.prior, None);
        reg.finish(after, Some("x".into()));
        assert_eq!(
            reg.begin(k.clone(), 8).prior,
            None,
            "an ended run is never recreated"
        );
        // codex hl-r4: a failed run reactivated by ResumeRun keeps sessions again.
        reg.reopen_run("r", 9);
        // codex hl-r5: an obsolete launch of the failed lifecycle owns nothing after reopen.
        let obsolete = reg.begin(k.clone(), 9);
        reg.finish(obsolete, Some("obsolete".into()));
        let t = reg.begin(k.clone(), 10);
        assert_eq!(
            t.prior, None,
            "the obsolete launch never seeds the new lifecycle"
        );
        reg.finish(t, Some("y".into()));
        assert_eq!(reg.begin(k, 11).prior.as_deref(), Some("y"));
    }

    /// codex hl-r1: only an id in the CLI's own grammar is ever remembered, and the engine's id
    /// wins over a stream that names another.
    #[test]
    fn only_a_well_formed_id_is_remembered() {
        let done = TurnSession {
            id: Some("--last".into()),
            completed: Some(true),
        };
        assert_eq!(session_after_turn(SeatCli::Codex, None, &done), None);
        let u = "01a13334-cda7-7b73-aa5d-346bc3b141d4";
        let done = TurnSession {
            id: Some(u.into()),
            completed: Some(true),
        };
        assert_eq!(
            session_after_turn(SeatCli::Codex, None, &done).as_deref(),
            Some(u)
        );
        let e = "11111111-2222-3333-4444-555555555555";
        assert_eq!(
            session_after_turn(SeatCli::Claude, Some(e), &done).as_deref(),
            Some(e)
        );
        // An engine-picking CLI never adopts a stream id the engine did not inject.
        assert_eq!(session_after_turn(SeatCli::Claude, None, &done), None);
        // An opencode `error` event fails the turn even on exit 0.
        let (mut tap, seen) = SessionTap::new(Box::new(Null), SeatCli::Opencode);
        tap.on_line(r#"{"type":"step_start","sessionID":"ses_a","part":{}}"#);
        tap.on_line(r#"{"type":"error","sessionID":"ses_a","error":{"name":"APIError","data":{"message":"rate limit"}}}"#);
        let (mut code, mut out) = (0, String::new());
        settle_cut_off_turn(&seen.lock().unwrap(), &mut code, &mut out, "opencode");
        assert_eq!(code, 1, "{out}");
        assert!(valid_session_id(SeatCli::Opencode, "ses_abc123"));
        for bad in ["ses_", "ses_a-b", "-r", "ses_a b"] {
            assert!(!valid_session_id(SeatCli::Opencode, bad), "{bad}");
        }
    }
}
