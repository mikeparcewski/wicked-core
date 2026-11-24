//! (DES-j6-build D1, seam C0) A Tool step's model calls.
//!
//! A host Tool step (`domain-run`) makes prompt-in, text-out model calls, and a host process
//! reaches no seat on its own. Every generic Tool unit is therefore armed with a channel back to
//! the daemon that runs it: a unix socket in a fresh private directory, named to the command by
//! [`SOCK_ENV`] and [`TOKEN_ENV`], and closed when the command exits ([`Channel`]'s drop). Each
//! request runs as ONE seat call through the engine's own one-shot carrier
//! ([`crate::decision::SeatBallots`], the council's ballot path) on the daemon's step runner: under
//! the container floor it is a seat-container call through the gateway, and under the host floor
//! the same runner the engine's judges use. The prompt goes to the seat byte-exact (no run-context
//! block). The client is `wicked-core model-call` ([`call_from_env`]).
//!
//! The wire is one JSON line each way: a [`Request`] in, a [`Reply`] out.
//!
//! Unix only: elsewhere [`arm`] refuses and the client says so.
#![cfg_attr(not(unix), allow(dead_code, unused_imports))]

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The socket path a Tool command reads to reach its daemon's model-call channel.
pub const SOCK_ENV: &str = "WICKED_MODEL_CALL_SOCK";
/// The channel's per-unit token: a request that does not carry it is refused.
pub const TOKEN_ENV: &str = "WICKED_MODEL_CALL_TOKEN";

/// The model families a Tool step may ask for (DES-j6-build D2: Claude Code writes, Codex checks).
/// Each is the registry key of its seat.
pub const FAMILIES: &[&str] = &["claude", "codex"];

/// A call's budget when the request names none: the proof's own (`calls.py`, 1500 s).
pub const DEFAULT_TIMEOUT_S: u64 = 1500;
/// The longest budget a request may ask for.
pub const MAX_TIMEOUT_S: u64 = 3600;
/// The most answer text one call keeps.
const STDOUT_CAP: usize = 4 * 1024 * 1024;
/// The largest request line the channel reads (the proof's largest prompt was 269 KB).
const MAX_REQUEST_BYTES: u64 = 8 * 1024 * 1024;
/// How often the listener looks for a stop between connections.
const POLL: Duration = Duration::from_millis(100);

/// One model call, as the client sends it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub token: String,
    pub family: String,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_s: Option<u64>,
}

/// The channel's answer: the seat's text, or why there is none.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    /// The seat that answered (its registry key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Reply {
    fn refused(why: impl Into<String>) -> Self {
        Reply {
            ok: false,
            cli: None,
            text: None,
            error: Some(why.into()),
        }
    }
}

/// What one call runs on: the daemon's step runner, the seat registry it was armed with, and the
/// run that asks (the seat call's parent run, core#1413).
struct Seats {
    runner: Arc<dyn crate::workflow::StepRunner>,
    registry: Vec<crate::AgenticCli>,
    run_id: String,
    ord: u32,
    token: String,
}

impl Seats {
    fn answer(&self, req: &Request) -> Reply {
        if !same_token(&req.token, &self.token) {
            return Reply::refused("the request does not carry this channel's token");
        }
        if !FAMILIES.contains(&req.family.as_str()) {
            return Reply::refused(format!(
                "family {:?} is not one of {}",
                req.family,
                FAMILIES.join(", ")
            ));
        }
        let Some(cli) = self.registry.iter().find(|c| c.key == req.family) else {
            return Reply::refused(format!(
                "no `{}` seat is registered on this daemon",
                req.family
            ));
        };
        if let Some(h) = cli.health.as_ref().filter(|h| !h.usable) {
            return Reply::refused(format!(
                "the `{}` seat is not usable: {}",
                cli.key,
                h.reason.as_deref().unwrap_or("no reason given")
            ));
        }
        let secs = req
            .timeout_s
            .unwrap_or(DEFAULT_TIMEOUT_S)
            .clamp(1, MAX_TIMEOUT_S);
        let Some(cli) = no_tools(cli) else {
            return Reply::refused(format!(
                "the `{}` seat's invocation cannot run as a tool-free model call",
                cli.key
            ));
        };
        let cli = &cli;
        use wicked_council::types::BallotCarrier;
        let carrier = crate::decision::SeatBallots::new(self.runner.clone(), Some(&self.run_id));
        let task = format!("model-call-{}-{}", self.run_id, self.ord);
        match carrier.run_ballot(
            cli,
            &task,
            &req.prompt,
            Duration::from_secs(secs),
            None,
            STDOUT_CAP,
        ) {
            Ok(run) if run.exit_ok => Reply {
                ok: true,
                cli: Some(cli.key.clone()),
                text: Some(run.stdout),
                error: None,
            },
            Ok(run) => Reply {
                ok: false,
                cli: Some(cli.key.clone()),
                text: None,
                error: Some(format!(
                    "the `{}` seat failed: {}",
                    cli.key,
                    clip(run.stdout.trim(), 2000)
                )),
            },
            Err(f) => Reply {
                ok: false,
                cli: Some(cli.key.clone()),
                text: None,
                error: Some(format!(
                    "the `{}` seat failed ({}): {}",
                    cli.key,
                    f.kind.as_str(),
                    clip(
                        [f.detail.trim(), f.stderr.trim(), f.stdout.trim()]
                            .into_iter()
                            .find(|s| !s.is_empty())
                            .unwrap_or(""),
                        2000
                    )
                )),
            },
        }
    }
}

/// (coord fold 1) The shipped preset steps whose Tool command may make model calls, as
/// `(preset, step id)`. Only these are armed ([`shipped_step`]); a repo-defined Tool step never is.
pub(crate) const MODEL_CALL_STEPS: &[(&str, &str)] = &[("domain-extraction", "domain-run")];

/// Whether `unit` of a run of `preset` is a declared model-call step of a SHIPPED preset: the run's
/// preset and the unit's step are declared in [`MODEL_CALL_STEPS`], and the unit's command is
/// byte-for-byte the built-in step's command. A repo or user preset that reuses the name with any
/// other command, or a registered workflow's Tool step, is not armed.
pub(crate) fn shipped_step(preset: Option<&str>, unit: &crate::domain::WorkUnit) -> bool {
    let declared = |p: &str| MODEL_CALL_STEPS.iter().any(|(dp, _)| *dp == p);
    if !preset.is_some_and(declared) {
        return false;
    }
    declared_in(
        MODEL_CALL_STEPS,
        &crate::catalog::builtin_presets(),
        preset,
        unit,
    )
}

fn declared_in(
    decl: &[(&str, &str)],
    shipped: &[(&'static str, Vec<crate::plan::PlanStep>)],
    preset: Option<&str>,
    unit: &crate::domain::WorkUnit,
) -> bool {
    let (Some(preset), Some(step), Some(cmd)) = (preset, unit.phase_id(), unit.tool_cmd.as_ref())
    else {
        return false;
    };
    if !decl.iter().any(|(p, s)| *p == preset && *s == step) {
        return false;
    }
    shipped
        .iter()
        .find(|(name, _)| *name == preset)
        .and_then(|(_, steps)| steps.iter().find(|s| s.id == step))
        .and_then(|s| match &s.executor {
            Some(crate::workflow::PhaseExecutor::Tool { cmd }) => Some(cmd),
            _ => None,
        })
        .is_some_and(|shipped_cmd| shipped_cmd == cmd)
}

/// The seat as a model call runs it: prompt in, text out, no tools (DES-j6-build D1, as the J6 v3 proof
/// ran it). Claude gets `--tools ""`; Codex gets `-s read-only` (an invocation that already names a
/// sandbox is refused, never run with it).
/// Both run in the call's own empty scratch tree (the unit has no workdir). Under the container
/// floor the floor turns codex's own sandbox off (it cannot run in a seat container,
/// `worker_sandbox::codex_sandbox_off_under_floor`), and the container is the boundary. `None` for
/// an invocation with no `{PROMPT}` token to place the flags before, or one this cannot make tool-free.
pub(crate) fn no_tools(cli: &crate::AgenticCli) -> Option<crate::AgenticCli> {
    let inv = cli.headless_invocation.as_str();
    let at = inv.find("\"{PROMPT}\"").or_else(|| inv.find("{PROMPT}"))?;
    let flags = match cli.key.as_str() {
        "claude" => "--tools \"\" ",
        "codex" if inv.contains(" -s ") || inv.contains("--sandbox") => return None,
        "codex" => "-s read-only ",
        _ => return None,
    };
    let mut out = cli.clone();
    out.headless_invocation = format!("{}{flags}{}", &inv[..at], &inv[at..]);
    Some(out)
}

/// Compare two tokens without stopping at the first difference.
fn same_token(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

fn clip(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// An armed channel. It lives as long as the Tool command; dropping it stops the listener and
/// removes the socket's directory. A call already running finishes on its own thread (its seat is
/// bounded by the call's budget); its answer then has no one to read it.
pub(crate) struct Channel {
    dir: PathBuf,
    sock: PathBuf,
    token: String,
    stop: Arc<AtomicBool>,
}

impl Channel {
    /// The variables the Tool command is handed.
    pub(crate) fn env(&self) -> Vec<(String, String)> {
        vec![
            (
                SOCK_ENV.to_string(),
                self.sock.to_string_lossy().into_owned(),
            ),
            (TOKEN_ENV.to_string(), self.token.clone()),
        ]
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Arm a channel for unit `ord` of `run_id`: a private directory (mode 0700) with one socket in
/// it, and a listener thread serving each connection on its own thread.
#[cfg(unix)]
pub(crate) fn arm(
    runner: Arc<dyn crate::workflow::StepRunner>,
    registry: Vec<crate::AgenticCli>,
    run_id: &str,
    ord: u32,
) -> std::io::Result<Channel> {
    use std::os::unix::fs::DirBuilderExt;
    use std::os::unix::net::UnixListener;
    let id = uuid::Uuid::new_v4().simple().to_string();
    // Short: a socket path must fit `sun_path` (104 bytes on macOS).
    let dir = std::env::temp_dir().join(format!("wmc-{}", &id[..12]));
    std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    let sock = dir.join("s");
    let listener = match UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e);
        }
    };
    if let Err(e) = listener.set_nonblocking(true) {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(e);
    }
    let token = uuid::Uuid::new_v4().simple().to_string();
    let stop = Arc::new(AtomicBool::new(false));
    let seats = Arc::new(Seats {
        runner,
        registry,
        run_id: run_id.to_string(),
        ord,
        token: token.clone(),
    });
    let stop2 = stop.clone();
    std::thread::Builder::new()
        .name(format!("model-call-{run_id}-{ord}"))
        .spawn(move || {
            while !stop2.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let seats = seats.clone();
                        std::thread::spawn(move || serve_one(stream, &seats));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(POLL)
                    }
                    Err(_) => std::thread::sleep(POLL),
                }
            }
        })?;
    Ok(Channel {
        dir,
        sock,
        token,
        stop,
    })
}

#[cfg(not(unix))]
pub(crate) fn arm(
    _runner: Arc<dyn crate::workflow::StepRunner>,
    _registry: Vec<crate::AgenticCli>,
    _run_id: &str,
    _ord: u32,
) -> std::io::Result<Channel> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "a Tool step's model-call channel needs a unix socket",
    ))
}

#[cfg(unix)]
fn serve_one(stream: std::os::unix::net::UnixStream, seats: &Seats) {
    let _ = stream.set_nonblocking(false);
    let reply = match read_request(&stream) {
        Ok(req) => seats.answer(&req),
        Err(why) => Reply::refused(why),
    };
    let mut out = &stream;
    if let Ok(mut line) = serde_json::to_string(&reply) {
        line.push('\n');
        let _ = out.write_all(line.as_bytes());
        let _ = out.flush();
    }
}

#[cfg(unix)]
fn read_request(stream: &std::os::unix::net::UnixStream) -> Result<Request, String> {
    let mut line = String::new();
    let mut reader = BufReader::new(stream.take(MAX_REQUEST_BYTES + 1));
    reader
        .read_line(&mut line)
        .map_err(|e| format!("could not read the request: {e}"))?;
    if line.len() as u64 > MAX_REQUEST_BYTES {
        return Err(format!(
            "the request is over {MAX_REQUEST_BYTES} bytes: not sent"
        ));
    }
    serde_json::from_str(line.trim_end()).map_err(|e| format!("the request is not valid: {e}"))
}

/// The client side (`wicked-core model-call`): send `prompt` to the `family` seat through the
/// channel the Tool step was armed with, and return the seat's text.
#[cfg(unix)]
pub fn call_from_env(family: &str, prompt: &str, timeout_s: Option<u64>) -> Result<String, String> {
    let sock = std::env::var(SOCK_ENV)
        .ok()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            format!("{SOCK_ENV} is not set: model-call runs only inside a Tool step of a run")
        })?;
    let token = std::env::var(TOKEN_ENV)
        .ok()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("{TOKEN_ENV} is not set"))?;
    call_at(&sock, &token, family, prompt, timeout_s)
}

#[cfg(not(unix))]
pub fn call_from_env(
    _family: &str,
    _prompt: &str,
    _timeout_s: Option<u64>,
) -> Result<String, String> {
    Err("model-call needs a unix socket".to_string())
}

#[cfg(unix)]
fn call_at(
    sock: &str,
    token: &str,
    family: &str,
    prompt: &str,
    timeout_s: Option<u64>,
) -> Result<String, String> {
    use std::os::unix::net::UnixStream;
    let stream =
        UnixStream::connect(sock).map_err(|e| format!("could not reach the channel: {e}"))?;
    // The reader waits past the call's own budget, so the seat's timeout is the one reported.
    let wait = timeout_s
        .unwrap_or(DEFAULT_TIMEOUT_S)
        .clamp(1, MAX_TIMEOUT_S)
        + 120;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(wait)));
    let req = Request {
        token: token.to_string(),
        family: family.to_string(),
        prompt: prompt.to_string(),
        timeout_s,
    };
    let mut line = serde_json::to_string(&req).map_err(|e| e.to_string())?;
    line.push('\n');
    (&stream)
        .write_all(line.as_bytes())
        .map_err(|e| format!("could not send the request: {e}"))?;
    let mut answer = String::new();
    BufReader::new(&stream)
        .read_line(&mut answer)
        .map_err(|e| format!("no answer from the channel: {e}"))?;
    let reply: Reply = serde_json::from_str(answer.trim_end())
        .map_err(|e| format!("the channel's answer is not valid: {e}"))?;
    match reply {
        Reply {
            ok: true,
            text: Some(text),
            ..
        } => Ok(text),
        r => Err(r
            .error
            .unwrap_or_else(|| "the channel answered without text".to_string())),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use wicked_council::types::{Category, Confidence, InputMode, SeatHealth};

    fn seat(key: &str) -> crate::AgenticCli {
        crate::AgenticCli {
            key: key.into(),
            display_name: key.into(),
            binary: "unused".into(),
            headless_invocation: match key {
                "claude" => "claude -p \"{PROMPT}\"".to_string(),
                "codex" => "codex exec --skip-git-repo-check \"{PROMPT}\"".to_string(),
                _ => format!("run-{key} {{PROMPT}}"),
            },
            category: Category::default(),
            input_mode: InputMode::default(),
            version_probe: vec![],
            trust_flags: vec![],
            alt_binaries: vec![],
            confidence: Confidence::default(),
            enabled_for_council: true,
            seat_eligible_for_work: true,
            acp: None,
            capabilities: None,
            login_invocation: None,
            logout_invocation: None,
            governance_class: None,
            credential: None,
            free_tier: None,
            health: None,
        }
    }

    /// What a seat was handed: (assigned cli, asking run, prompt, invocation).
    type Handed = (Option<String>, Option<String>, String, Option<String>);

    /// A seat runner that answers `<cli>: <prompt>` and records what it was handed.
    #[derive(Default)]
    struct Echo {
        seen: Mutex<Vec<Handed>>,
        fail: bool,
    }

    impl crate::workflow::StepRunner for Echo {
        fn run_unit(&self, i: &crate::workflow::StepInput) -> crate::workflow::StepOutput {
            self.seen.lock().unwrap().push((
                i.unit.assigned_cli.clone(),
                i.unit.parent_run_id.clone(),
                i.unit.description.clone(),
                i.unit.assigned_invocation.clone(),
            ));
            crate::workflow::StepOutput {
                run_id: i.run_id.clone(),
                unit_ix: 0,
                attempt: 0,
                output: format!(
                    "{}: {}",
                    i.unit.assigned_cli.clone().unwrap_or_default(),
                    i.unit.description
                ),
                status: if self.fail {
                    crate::workflow::StepStatus::Failed
                } else {
                    crate::workflow::StepStatus::Ok
                },
                usage: None,
                files: Vec::new(),
                tools: Vec::new(),
                governed: false,
            }
        }
    }

    fn sock_and_token(ch: &Channel) -> (String, String) {
        let env = ch.env();
        let get = |k: &str| env.iter().find(|(n, _)| n == k).unwrap().1.clone();
        (get(SOCK_ENV), get(TOKEN_ENV))
    }

    /// The C0 shape: one claude and one codex call through one armed channel each reach their own
    /// seat, under the asking run, with the prompt byte-exact (newlines and a long body included).
    #[test]
    fn a_tool_step_gets_one_claude_and_one_codex_answer() {
        let echo = Arc::new(Echo::default());
        let ch = arm(
            echo.clone(),
            vec![seat("claude"), seat("codex")],
            "run-1",
            2,
        )
        .unwrap();
        let (sock, token) = sock_and_token(&ch);
        let long = format!("line one\nline \"two\"\n{}", "x".repeat(300 * 1024));
        let a = call_at(&sock, &token, "claude", "write it", Some(30)).unwrap();
        let b = call_at(&sock, &token, "codex", &long, Some(30)).unwrap();
        assert_eq!(a, "claude: write it");
        assert_eq!(b, format!("codex: {long}"));
        let seen = echo.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].0.as_deref(), Some("claude"));
        assert_eq!(seen[1].0.as_deref(), Some("codex"));
        assert!(seen.iter().all(|s| s.1.as_deref() == Some("run-1")));
        assert_eq!(seen[1].2, long);
        // (coord fold 2) No tools: the proof's `claude --tools ""` and `codex -s read-only`.
        assert_eq!(
            seen[0].3.as_deref(),
            Some("claude -p --tools \"\" \"{PROMPT}\"")
        );
        assert_eq!(
            seen[1].3.as_deref(),
            Some("codex exec --skip-git-repo-check -s read-only \"{PROMPT}\"")
        );
    }

    fn unit(step: &str, cmd: &[String]) -> crate::domain::WorkUnit {
        let mut u = crate::domain::WorkUnit::pending(format!("r:{step}"), "r", 1, "tool");
        u.tool_cmd = Some(cmd.to_vec());
        u
    }

    fn shipped_cmd(preset: &str, step: &str) -> Vec<String> {
        crate::catalog::builtin_presets()
            .into_iter()
            .find(|(n, _)| *n == preset)
            .and_then(|(_, steps)| steps.into_iter().find(|s| s.id == step))
            .and_then(|s| match s.executor {
                Some(crate::workflow::PhaseExecutor::Tool { cmd }) => Some(cmd),
                _ => None,
            })
            .expect("a shipped Tool step")
    }

    /// (coord fold 1) A declared step of a shipped preset with the shipped command is armed; the
    /// same step under a repo preset name, a changed command, an undeclared step, or a run with no
    /// preset is not. A real shipped Tool step (`extract-prepare`) stands in for the declaration.
    #[test]
    fn only_a_declared_step_of_a_shipped_preset_is_armed() {
        let step = crate::domain_extraction::EXTRACT_PREPARE_STEP;
        let cmd = shipped_cmd("domain-extraction", step);
        let decl = [("domain-extraction", step)];
        let shipped = crate::catalog::builtin_presets();
        let armed = |preset: Option<&str>, u: &crate::domain::WorkUnit| {
            declared_in(&decl, &shipped, preset, u)
        };
        assert!(armed(Some("domain-extraction"), &unit(step, &cmd)));
        // a repo-defined preset or workflow with the same step and command
        assert!(!armed(Some("my-repo-flow"), &unit(step, &cmd)));
        assert!(!armed(None, &unit(step, &cmd)));
        // the shipped preset's name with another command (a repo preset shadowing the name)
        let mut other = cmd.clone();
        other.push("--and-more".into());
        assert!(!armed(Some("domain-extraction"), &unit(step, &other)));
        // a shipped Tool step that is not declared
        assert!(!armed(
            Some("domain-extraction"),
            &unit("domain-graph", &cmd)
        ));
        // production: nothing in today's presets is declared yet (C1 adds `domain-run`)
        assert!(!shipped_step(Some("domain-extraction"), &unit(step, &cmd)));
    }

    /// A seat that cannot be made tool-free is refused, never run with its tools.
    #[test]
    fn a_seat_that_cannot_run_tool_free_is_refused() {
        let mut sandboxed = seat("codex");
        sandboxed.headless_invocation = "codex exec -s workspace-write \"{PROMPT}\"".into();
        assert!(no_tools(&sandboxed).is_none());
        let echo = Arc::new(Echo::default());
        let ch = arm(echo.clone(), vec![sandboxed], "run-1", 2).unwrap();
        let (sock, token) = sock_and_token(&ch);
        let err = call_at(&sock, &token, "codex", "p", None).unwrap_err();
        assert!(err.contains("tool-free"), "{err}");
        assert!(echo.seen.lock().unwrap().is_empty());
    }

    /// A wrong token, an unknown family, an unregistered or benched seat: refused, no seat runs.
    #[test]
    fn a_call_the_channel_cannot_honour_is_refused_before_any_seat_runs() {
        let echo = Arc::new(Echo::default());
        let mut benched = seat("codex");
        benched.health = Some(SeatHealth {
            usable: false,
            reason: Some("signed out".into()),
        });
        let ch = arm(echo.clone(), vec![benched], "run-1", 2).unwrap();
        let (sock, token) = sock_and_token(&ch);
        let bad_token = call_at(&sock, "nope", "codex", "p", None).unwrap_err();
        assert!(bad_token.contains("token"), "{bad_token}");
        let unknown = call_at(&sock, &token, "pi", "p", None).unwrap_err();
        assert!(unknown.contains("not one of"), "{unknown}");
        let missing = call_at(&sock, &token, "claude", "p", None).unwrap_err();
        assert!(missing.contains("no `claude` seat"), "{missing}");
        let off = call_at(&sock, &token, "codex", "p", None).unwrap_err();
        assert!(off.contains("signed out"), "{off}");
        assert!(echo.seen.lock().unwrap().is_empty());
    }

    /// A seat that fails is reported as a failure with its words, never as an answer.
    #[test]
    fn a_failed_seat_is_an_error_not_an_answer() {
        let echo = Arc::new(Echo {
            fail: true,
            ..Default::default()
        });
        let ch = arm(echo, vec![seat("claude")], "run-1", 2).unwrap();
        let (sock, token) = sock_and_token(&ch);
        let err = call_at(&sock, &token, "claude", "p", None).unwrap_err();
        assert!(err.contains("the `claude` seat failed"), "{err}");
    }

    /// Dropping the channel (the Tool command exited) removes its socket: no later call lands.
    #[test]
    fn the_channel_closes_with_its_command() {
        let echo = Arc::new(Echo::default());
        let ch = arm(echo, vec![seat("claude")], "run-1", 2).unwrap();
        let (sock, token) = sock_and_token(&ch);
        let dir = std::path::Path::new(&sock).parent().unwrap().to_path_buf();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        drop(ch);
        assert!(!dir.exists());
        assert!(call_at(&sock, &token, "claude", "p", None).is_err());
    }

    /// The client outside a Tool step says so instead of hanging or guessing.
    #[test]
    fn the_client_outside_a_tool_step_names_the_missing_channel() {
        // Read-only on the process env: the test harness never sets the channel variables.
        if std::env::var_os(SOCK_ENV).is_none() {
            let err = call_from_env("claude", "p", None).unwrap_err();
            assert!(err.contains(SOCK_ENV), "{err}");
        }
    }
}
