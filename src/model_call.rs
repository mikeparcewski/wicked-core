//! (DES-j6-build D1, seam C0) A Tool step's model calls.
//!
//! A host Tool step (`domain-run`) makes prompt-in, text-out model calls, and a host process
//! reaches no seat on its own. A Tool unit of a step a SHIPPED preset declares
//! ([`MODEL_CALL_STEPS`], [`shipped_step`]) is therefore armed with a channel back to the daemon
//! that runs it: a unix socket in a fresh private directory, named to the command by
//! [`SOCK_ENV`] and [`TOKEN_ENV`], and closed when the command exits ([`Channel`]'s drop). No other
//! Tool unit is armed. Each request runs as ONE seat call through the engine's own one-shot carrier
//! ([`crate::decision::SeatBallots`], the council's ballot path) on the daemon's step runner: under
//! the container floor it is a seat-container call through the gateway, and under the host floor
//! the same runner the engine's judges use. The call runs tool-free under the engine's own
//! postures ([`tool_free`]); the prompt is the unit's whole task (no run-context block), and the
//! runner adds only its posture's instruction. The client is `wicked-core model-call`
//! ([`call_from_env`]).
//!
//! The prompt reaches the seat on stdin (C0a, [`crate::execute_wrapped::with_prompt_on_stdin`]),
//! byte-exact at any size on both floors. A claude call may cap its answer
//! ([`Request::max_output_tokens`]): the seat runs with `CLAUDE_CODE_MAX_OUTPUT_TOKENS` set to it
//! ([`crate::execute_wrapped::with_answer_cap`]), as the J6 v3 proof ran its writer.
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
/// The largest answer cap a claude call may ask for, in output tokens.
pub const MAX_OUTPUT_TOKENS: u32 = 128_000;
/// The most answer text one call keeps.
const STDOUT_CAP: usize = 4 * 1024 * 1024;
/// The largest request line the channel reads (the proof's largest prompt was 269 KB).
const MAX_REQUEST_BYTES: u64 = 8 * 1024 * 1024;
/// How often the listener looks for a stop between connections.
const POLL: Duration = Duration::from_millis(100);
/// How long a connection may take to send its request, and the reader to take the reply.
const IO_DEADLINE: Duration = Duration::from_secs(60);
/// The most connections one channel serves at once (a Tool step makes its calls a few at a time).
const MAX_CONNECTIONS: usize = 16;
/// How long a timed-out call's seat is waited for before its connection is answered.
const REAP_BOUND: Duration = Duration::from_secs(30);

/// One model call, as the client sends it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub token: String,
    pub family: String,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_s: Option<u64>,
    /// A claude call's answer cap in output tokens (1..=[`MAX_OUTPUT_TOKENS`]); none = the seat's
    /// own default. A codex call that names one is refused (codex has no such setting here).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
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
        if let Some(n) = req.max_output_tokens {
            if cli.key != "claude" {
                return Reply::refused(format!(
                    "an answer cap (max_output_tokens) is a claude setting; the `{}` seat takes none",
                    cli.key
                ));
            }
            if !(1..=MAX_OUTPUT_TOKENS).contains(&n) {
                return Reply::refused(format!(
                    "max_output_tokens {n} is not 1..={MAX_OUTPUT_TOKENS}"
                ));
            }
        }
        use wicked_council::types::BallotCarrier;
        let carrier = crate::decision::SeatBallots::new(self.runner.clone(), Some(&self.run_id))
            .model_calls()
            .answer_cap(req.max_output_tokens);
        let task = format!("model-call-{}-{}", self.run_id, self.ord);
        let out = carrier.run_ballot(
            cli,
            &task,
            &req.prompt,
            Duration::from_secs(secs),
            None,
            STDOUT_CAP,
        );
        // A seat that timed out and has not stopped is waited for here, so this connection's
        // answer comes after the seat's end (or after the bound).
        carrier.reaper().reap(REAP_BOUND);
        match out {
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

/// Whether `unit` of a run of `preset` against `repo` is a declared model-call step of a SHIPPED
/// preset: the run's preset and the unit's step are declared in [`MODEL_CALL_STEPS`], and the
/// unit's command is the built-in step's command bound to the run's repo by the planner's own
/// binder ([`crate::plan::bind_repo_paths`]), byte for byte. A preset that reuses the name with any
/// other command, or a registered workflow's Tool step, is not armed. (A project preset that
/// shadows the name keeping the exact command runs the shipped program, which the engine hands
/// the garden root; it makes no call the shipped preset would not.)
pub(crate) fn shipped_step(
    preset: Option<&str>,
    unit: &crate::domain::WorkUnit,
    repo: Option<&crate::repo::RepoEntry>,
) -> bool {
    let declared = |p: &str| MODEL_CALL_STEPS.iter().any(|(dp, _)| *dp == p);
    if !preset.is_some_and(declared) {
        return false;
    }
    declared_in(
        MODEL_CALL_STEPS,
        &crate::catalog::builtin_presets(),
        preset,
        unit,
        repo,
    )
}

fn declared_in(
    decl: &[(&str, &str)],
    shipped: &[(&'static str, Vec<crate::plan::PlanStep>)],
    preset: Option<&str>,
    unit: &crate::domain::WorkUnit,
    repo: Option<&crate::repo::RepoEntry>,
) -> bool {
    let (Some(preset), Some(step), Some(cmd)) = (preset, unit.phase_id(), unit.tool_cmd.as_ref())
    else {
        return false;
    };
    if !decl.iter().any(|(p, s)| *p == preset && *s == step) {
        return false;
    }
    let Some(template) = shipped
        .iter()
        .find(|(name, _)| *name == preset)
        .and_then(|(_, steps)| steps.iter().find(|s| s.id == step))
        .and_then(|s| match &s.executor {
            Some(crate::workflow::PhaseExecutor::Tool { cmd }) => Some(cmd.clone()),
            _ => None,
        })
    else {
        return false;
    };
    let mut bound = crate::domain::WorkUnit::pending(unit.id.clone(), "", unit.ord, "");
    bound.tool_cmd = Some(template);
    if let Some(repo) = repo {
        crate::plan::bind_repo_paths(std::slice::from_mut(&mut bound), repo);
    }
    bound.tool_cmd.as_ref() == Some(cmd)
}

/// (coord fold 2) A model call runs tool-free, as the J6 v3 proof ran it, under the engine's own
/// postures ([`crate::write_posture::WritePosture::of`]) so the runner reconciles them with the
/// seat's invocation and resolved trust flags at its one launch boundary:
/// - claude is an answer-only unit, the tool-less posture: `--tools ""` (core#1257);
/// - codex is a read-only evaluator: on the host floor its `--sandbox workspace-write` rooted at a
///   per-call check scratch (`-C`), the tree denied by the kernel (F-036, core#503); under the
///   container floor the floor turns codex's own sandbox off, and the seat container is the
///   boundary.
pub(crate) fn tool_free(unit: &mut crate::domain::WorkUnit, family: &str) {
    if family == "claude" {
        unit.answer_only = true;
    } else {
        unit.role = crate::workflow::PhaseRole::Evaluator;
        unit.worktree_guarded = true;
    }
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

/// An armed channel. It lives as long as the Tool command; dropping it stops the listener, removes
/// the socket's directory, and stops a connection already open from starting a call. A call
/// already running finishes on its own thread (its seat is bounded by the call's budget); its
/// answer then has no one to read it.
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
    // The channel owns the directory from here: an early return below removes it.
    let channel = Channel {
        dir,
        sock,
        token: token.clone(),
        stop: Arc::new(AtomicBool::new(false)),
    };
    let seats = Arc::new(Seats {
        runner,
        registry,
        run_id: run_id.to_string(),
        ord,
        token,
    });
    let stop = channel.stop.clone();
    let open = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    std::thread::Builder::new()
        .name(format!("model-call-{run_id}-{ord}"))
        .spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if open.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
                            open.fetch_sub(1, Ordering::SeqCst);
                            continue; // dropped: the client sees the connection close
                        }
                        let (seats, halt, held) = (seats.clone(), stop.clone(), open.clone());
                        let served = std::thread::Builder::new().spawn(move || {
                            serve_one(stream, &seats, &halt);
                            held.fetch_sub(1, Ordering::SeqCst);
                        });
                        if served.is_err() {
                            // the stream went with the closure; the count goes back here
                            open.fetch_sub(1, Ordering::SeqCst);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(POLL)
                    }
                    Err(_) => std::thread::sleep(POLL),
                }
            }
        })?;
    Ok(channel)
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
fn serve_one(stream: std::os::unix::net::UnixStream, seats: &Seats, stop: &AtomicBool) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(IO_DEADLINE));
    let _ = stream.set_write_timeout(Some(IO_DEADLINE));
    let reply = match read_request(&stream) {
        // A connection opened while the step ran starts no call once the step has ended.
        Ok(_) if stop.load(Ordering::SeqCst) => {
            Reply::refused("the Tool step that armed this channel has ended")
        }
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
pub fn call_from_env(
    family: &str,
    prompt: &str,
    timeout_s: Option<u64>,
    max_output_tokens: Option<u32>,
) -> Result<String, String> {
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
    call_at(&sock, &token, family, prompt, timeout_s, max_output_tokens)
}

#[cfg(not(unix))]
pub fn call_from_env(
    _family: &str,
    _prompt: &str,
    _timeout_s: Option<u64>,
    _max_output_tokens: Option<u32>,
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
    max_output_tokens: Option<u32>,
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
        max_output_tokens,
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

    /// What a seat was handed: (assigned cli, asking run, prompt, invocation, the write posture
    /// the runner derives for the unit on an unbound call).
    type Handed = (
        Option<String>,
        Option<String>,
        String,
        Option<String>,
        crate::write_posture::WritePosture,
    );

    /// A seat runner that answers `<cli>: <prompt>` and records what it was handed.
    #[derive(Default)]
    struct Echo {
        seen: Mutex<Vec<Handed>>,
        /// What each run's launch thread was set to: (its claude answer cap, prompt on stdin).
        launch: Mutex<Vec<(Option<u32>, bool)>>,
        fail: bool,
    }

    impl crate::workflow::StepRunner for Echo {
        fn run_unit(&self, i: &crate::workflow::StepInput) -> crate::workflow::StepOutput {
            self.seen.lock().unwrap().push((
                i.unit.assigned_cli.clone(),
                i.unit.parent_run_id.clone(),
                i.unit.description.clone(),
                i.unit.assigned_invocation.clone(),
                crate::write_posture::WritePosture::of(&i.unit, i.workdir.is_some()),
            ));
            self.launch.lock().unwrap().push((
                crate::execute_wrapped::answer_cap(),
                crate::execute_wrapped::prompt_on_stdin(),
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
        let a = call_at(&sock, &token, "claude", "write it", Some(30), None).unwrap();
        let b = call_at(&sock, &token, "codex", &long, Some(30), None).unwrap();
        assert_eq!(a, "claude: write it");
        assert_eq!(b, format!("codex: {long}"));
        let seen = echo.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].0.as_deref(), Some("claude"));
        assert_eq!(seen[1].0.as_deref(), Some("codex"));
        assert!(seen.iter().all(|s| s.1.as_deref() == Some("run-1")));
        assert_eq!(seen[1].2, long);
        // (coord fold 2) No tools, through the engine's own postures: claude is tool-less (the
        // runner's `--tools ""`), codex a read-only evaluator (its `--sandbox workspace-write` rooted
        // at a check scratch, the tree denied). The seat's own invocation is handed on unchanged; the runner applies
        // the posture to it and to the seat's resolved trust flags at its launch boundary.
        use crate::write_posture::WritePosture;
        assert_eq!(seen[0].4, WritePosture::ToolLess);
        assert_eq!(seen[1].4, WritePosture::ReadOnly);
        assert_eq!(seen[0].3.as_deref(), Some("claude -p \"{PROMPT}\""));
        assert!(seen.iter().all(|s| s.4 != WritePosture::Full));
    }

    /// The runner's tool-less claude argv for a model call's posture: the lever is the engine's
    /// own (`apply_tool_less_posture`), placed before an option guard and over any allowlist.
    #[test]
    fn a_model_call_claude_argv_is_tool_less_even_past_an_option_guard() {
        let mut argv: Vec<String> = ["claude", "-p", "--tools", "Bash", "--", "the prompt"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        crate::execute_wrapped::apply_tool_less_posture(&mut argv, Vec::new(), true, false)
            .unwrap();
        let guard = argv.iter().position(|a| a == "--").unwrap();
        let tools = argv.iter().position(|a| a == "--tools").unwrap();
        assert!(tools < guard && argv[tools + 1].is_empty(), "{argv:?}");
        assert_eq!(argv.iter().filter(|a| *a == "--tools").count(), 1);
    }

    /// (C0a, C0) Every model call's prompt rides stdin; (coord round 2 on garden#1557) a claude
    /// call's answer cap reaches its launch thread, and nothing else carries one: a codex call
    /// that names a cap, and a cap out of range, are refused before any seat runs.
    #[test]
    fn a_claude_answer_cap_reaches_its_launch_and_every_prompt_rides_stdin() {
        let echo = Arc::new(Echo::default());
        let ch = arm(echo.clone(), vec![seat("claude"), seat("codex")], "run-c", 1).unwrap();
        let (sock, token) = sock_and_token(&ch);
        assert_eq!(
            call_at(&sock, &token, "claude", "w", None, Some(64000)).unwrap(),
            "claude: w"
        );
        call_at(&sock, &token, "claude", "w", None, None).unwrap();
        call_at(&sock, &token, "codex", "c", None, None).unwrap();
        let codex_cap = call_at(&sock, &token, "codex", "c", None, Some(64000)).unwrap_err();
        assert!(codex_cap.contains("claude setting"), "{codex_cap}");
        for bad in [0, MAX_OUTPUT_TOKENS + 1] {
            let e = call_at(&sock, &token, "claude", "w", None, Some(bad)).unwrap_err();
            assert!(e.contains("max_output_tokens"), "{e}");
        }
        assert_eq!(
            *echo.launch.lock().unwrap(),
            vec![(Some(64000), true), (None, true), (None, true)]
        );
        // the cap is the launch thread's alone: none is left behind on this one
        assert_eq!(crate::execute_wrapped::answer_cap(), None);
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

    fn repo_at(id: &str, root: &str) -> crate::repo::RepoEntry {
        crate::repo::RepoEntry {
            id: id.to_string(),
            name: id.to_string(),
            root_path: root.to_string(),
            default_branch: "main".to_string(),
            registered_at: 0,
            code_graph_db: format!("/graphs/{id}/estate.db"),
            graph_key: id.to_string(),
            findings: Vec::new(),
        }
    }

    /// The unit the planner dispatches for a shipped step: its template bound to the run's repo by
    /// the planner's own binder.
    fn planned(
        step: &str,
        cmd: &[String],
        repo: &crate::repo::RepoEntry,
    ) -> crate::domain::WorkUnit {
        let mut u = unit(step, cmd);
        crate::plan::bind_repo_paths(std::slice::from_mut(&mut u), repo);
        u
    }

    /// (coord fold 1) A declared step of a shipped preset with the shipped command, bound to the
    /// run's repo, is armed; the same step under a repo preset name, a changed command, a command
    /// bound to another repo, an undeclared step, or a run with no preset is not. A real shipped
    /// Tool step (`extract-prepare`, whose command carries both repo placeholders) stands in for the
    /// declaration.
    #[test]
    fn only_a_declared_step_of_a_shipped_preset_is_armed() {
        let step = crate::domain_extraction::EXTRACT_PREPARE_STEP;
        let cmd = shipped_cmd("domain-extraction", step);
        assert!(cmd.iter().any(|a| a == crate::workflow::REPO_ROOT_TOKEN));
        let repo = repo_at("app", "/src/app");
        let decl = [("domain-extraction", step)];
        let shipped = crate::catalog::builtin_presets();
        let armed = |preset: Option<&str>, u: &crate::domain::WorkUnit| {
            declared_in(&decl, &shipped, preset, u, Some(&repo))
        };
        let real = planned(step, &cmd, &repo);
        assert_ne!(
            real.tool_cmd.as_ref(),
            Some(&cmd),
            "the planner bound the repo"
        );
        assert!(armed(Some("domain-extraction"), &real));
        // a repo-defined preset or workflow with the same step and command
        assert!(!armed(Some("my-repo-flow"), &real));
        assert!(!armed(None, &real));
        // the shipped preset's name with another command (a preset shadowing the name)
        let mut other = cmd.clone();
        other.push("--and-more".into());
        assert!(!armed(
            Some("domain-extraction"),
            &planned(step, &other, &repo)
        ));
        // the shipped command bound to a repo other than the run's
        let elsewhere = planned(step, &cmd, &repo_at("other", "/src/other"));
        assert!(!armed(Some("domain-extraction"), &elsewhere));
        // a shipped Tool step that is not declared
        assert!(!armed(
            Some("domain-extraction"),
            &planned("domain-graph", &cmd, &repo)
        ));
        // production: nothing in today's presets is declared yet (C1 adds `domain-run`)
        assert!(!shipped_step(Some("domain-extraction"), &real, Some(&repo)));
    }

    /// A connection opened while the step ran starts no call after the step ends.
    #[test]
    fn a_connection_left_open_starts_no_call_after_the_step_ends() {
        use std::os::unix::net::UnixStream;
        let echo = Arc::new(Echo::default());
        let ch = arm(echo.clone(), vec![seat("claude")], "run-1", 2).unwrap();
        let (sock, token) = sock_and_token(&ch);
        let stream = UnixStream::connect(&sock).unwrap();
        // let the listener accept it while the channel is armed
        std::thread::sleep(POLL * 3);
        drop(ch);
        let req = Request {
            token,
            family: "claude".into(),
            prompt: "late".into(),
            timeout_s: None,
            max_output_tokens: None,
        };
        let mut line = serde_json::to_string(&req).unwrap();
        line.push('\n');
        (&stream).write_all(line.as_bytes()).unwrap();
        let mut answer = String::new();
        BufReader::new(&stream).read_line(&mut answer).unwrap();
        let reply: Reply = serde_json::from_str(answer.trim_end()).unwrap();
        assert!(!reply.ok);
        assert!(reply.error.unwrap().contains("has ended"));
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
        let bad_token = call_at(&sock, "nope", "codex", "p", None, None).unwrap_err();
        assert!(bad_token.contains("token"), "{bad_token}");
        let unknown = call_at(&sock, &token, "pi", "p", None, None).unwrap_err();
        assert!(unknown.contains("not one of"), "{unknown}");
        let missing = call_at(&sock, &token, "claude", "p", None, None).unwrap_err();
        assert!(missing.contains("no `claude` seat"), "{missing}");
        let off = call_at(&sock, &token, "codex", "p", None, None).unwrap_err();
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
        let err = call_at(&sock, &token, "claude", "p", None, None).unwrap_err();
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
        assert!(call_at(&sock, &token, "claude", "p", None, None).is_err());
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
