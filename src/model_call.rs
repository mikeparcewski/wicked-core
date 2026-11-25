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

/// (core#1905) Which seat answers each family on this run, decided once when the channel is armed:
/// `Ok` is the seat instance that answers, `Err` names every instance of the family and why it is
/// out. Built by [`admission`].
pub(crate) type Admission = std::collections::BTreeMap<String, Result<crate::AgenticCli, String>>;

/// (core#1905) The seats a model call may use: the run's own LAUNCH roster (the instances the
/// launcher configured, with their invocations and the launcher's health probe), never the
/// daemon's whole registry. An instance is out when it, or its CLI, is benched for this run, or
/// when the launcher's probe found it unusable. The benches are the ballot rule's (a model call is
/// an ungoverned engine call, like a council ballot): the run's own (`run_bench`, its
/// `benched_seats`) and its launch benches (`launch_bench`), each less its governed-only entries
/// ([`crate::domain::ungoverned_bench`]). A registry seat that is not on the launch roster is never
/// used. Per family, the first eligible instance in roster order answers (`claude#2` answers
/// `claude` when it is the one configured).
pub(crate) fn admission(
    roster: &[crate::AgenticCli],
    run_bench: &[crate::domain::BenchedSeat],
    launch_bench: &[crate::domain::BenchedSeat],
) -> Admission {
    let mut bench = crate::domain::ungoverned_bench(run_bench);
    bench.extend(crate::domain::ungoverned_bench(launch_bench));
    FAMILIES
        .iter()
        .map(|family| {
            let mut why = Vec::new();
            let mut pick = None;
            for c in roster
                .iter()
                .filter(|c| crate::distribute::model_of(&c.key) == *family)
            {
                let model = crate::distribute::model_of(&c.key);
                if let Some(b) = bench.iter().find(|b| b.cli == c.key || b.cli == model) {
                    why.push(format!(
                        "the `{}` seat is benched for this run: {} ({})",
                        c.key, b.reason, b.source
                    ));
                } else if let Some(h) = c.health.as_ref().filter(|h| !h.usable) {
                    why.push(format!(
                        "the `{}` seat is not usable: {}",
                        c.key,
                        h.reason.as_deref().unwrap_or("no reason given")
                    ));
                } else {
                    pick = Some(c.clone());
                    break;
                }
            }
            let verdict = match pick {
                Some(c) => Ok(c),
                None if why.is_empty() => {
                    Err(format!("no `{family}` seat is on this run's roster"))
                }
                None => Err(why.join("; ")),
            };
            (family.to_string(), verdict)
        })
        .collect()
}

/// (core#1906) The calls a channel has started, and whether its step has ended. A call is admitted
/// under the lock before its seat runs and released under it after; closing marks the calls still
/// live as cancelled and hands back their ids to cancel. So a call admitted after the close never
/// runs, a call live at the close is cancelled and answers "cancelled: the step ended" whatever its
/// seat returned, and a call released before the close is never cancelled (a cancel would leave a
/// tombstone in the runner for the daemon's life).
#[derive(Default)]
pub(crate) struct StepCalls(std::sync::Mutex<StepCallState>);

#[derive(Default)]
struct StepCallState {
    closed: bool,
    /// Live call id -> whether the close cancelled it.
    live: std::collections::HashMap<String, bool>,
}

/// What a call whose step ended answers.
pub(crate) const STEP_ENDED: &str = "cancelled: the step ended";

impl StepCalls {
    fn state(&self) -> std::sync::MutexGuard<'_, StepCallState> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Admit call `id` before its seat runs: `false` once the step has ended.
    pub(crate) fn admit(&self, id: &str) -> bool {
        let mut st = self.state();
        if st.closed {
            return false;
        }
        st.live.insert(id.to_string(), false);
        true
    }

    /// Whether call `id`, still live, was cancelled by the close.
    pub(crate) fn cancelled(&self, id: &str) -> bool {
        self.state().live.get(id).copied().unwrap_or(false)
    }

    /// Release call `id` after its seat returned: whether the close cancelled it.
    pub(crate) fn release(&self, id: &str) -> bool {
        self.state().live.remove(id).unwrap_or(false)
    }

    /// End the step: no call is admitted from here, and every call still live is marked cancelled.
    /// The ids returned are the ones to cancel (none on a second close).
    pub(crate) fn close(&self) -> Vec<String> {
        let mut st = self.state();
        if st.closed {
            return Vec::new();
        }
        st.closed = true;
        st.live
            .iter_mut()
            .map(|(id, cancelled)| {
                *cancelled = true;
                id.clone()
            })
            .collect()
    }
}

/// What one call runs on: the daemon's step runner, the seats the run admits ([`admission`]), and
/// the run that asks (the seat call's parent run, core#1413).
struct Seats {
    runner: Arc<dyn crate::workflow::StepRunner>,
    admit: Admission,
    calls: Arc<StepCalls>,
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
        let cli = match self.admit.get(&req.family) {
            Some(Ok(cli)) => cli,
            Some(Err(why)) => return Reply::refused(why.clone()),
            None => {
                return Reply::refused(format!("no `{}` seat is on this run's roster", req.family))
            }
        };
        let secs = req
            .timeout_s
            .unwrap_or(DEFAULT_TIMEOUT_S)
            .clamp(1, MAX_TIMEOUT_S);
        if let Some(n) = req.max_output_tokens {
            if crate::distribute::model_of(&cli.key) != "claude" {
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
            .answer_cap(req.max_output_tokens)
            .step_calls(self.calls.clone());
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
        if carrier.step_ended() {
            return Reply {
                ok: false,
                cli: Some(cli.key.clone()),
                text: None,
                error: Some(STEP_ENDED.to_string()),
            };
        }
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
    repo: impl FnOnce() -> Option<crate::repo::RepoEntry>,
) -> bool {
    declared_in(
        MODEL_CALL_STEPS,
        &crate::catalog::builtin_presets,
        preset,
        unit,
        repo,
    )
}

/// (C1) The preset and step are checked first; the run's repo is looked up (`repo`, a store read)
/// only for a declared step, and the shipped presets are built (`shipped`) only then.
fn declared_in(
    decl: &[(&str, &str)],
    shipped: &dyn Fn() -> Vec<(&'static str, Vec<crate::plan::PlanStep>)>,
    preset: Option<&str>,
    unit: &crate::domain::WorkUnit,
    repo: impl FnOnce() -> Option<crate::repo::RepoEntry>,
) -> bool {
    let (Some(preset), Some(step), Some(cmd)) = (preset, unit.phase_id(), unit.tool_cmd.as_ref())
    else {
        return false;
    };
    if !decl.iter().any(|(p, s)| *p == preset && *s == step) {
        return false;
    }
    let Some(template) = shipped()
        .into_iter()
        .find(|(name, _)| *name == preset)
        .and_then(|(_, steps)| steps.into_iter().find(|s| s.id == step))
        .and_then(|s| match s.executor {
            Some(crate::workflow::PhaseExecutor::Tool { cmd }) => Some(cmd),
            _ => None,
        })
    else {
        return false;
    };
    let mut bound = crate::domain::WorkUnit::pending(unit.id.clone(), "", unit.ord, "");
    bound.tool_cmd = Some(template);
    if let Some(repo) = repo() {
        crate::plan::bind_repo_paths(std::slice::from_mut(&mut bound), &repo);
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
pub(crate) fn tool_free(unit: &mut crate::domain::WorkUnit, seat: &str) {
    if crate::distribute::model_of(seat) == "claude" {
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

/// An armed channel. It lives as long as the Tool command: [`Channel::close`] (run when the
/// command's own process exits, and again on drop) stops the listener, refuses every later call,
/// and cancels the calls still running (core#1906); dropping it also removes the socket's
/// directory.
pub(crate) struct Channel {
    dir: PathBuf,
    sock: PathBuf,
    token: String,
    stop: Arc<AtomicBool>,
    calls: Arc<StepCalls>,
    runner: Arc<dyn crate::workflow::StepRunner>,
    /// This engine's own binary ([`crate::execute_wrapped::resolve_wicked_core_exe_opt`]), so the
    /// command's `wicked-core model-call` reaches this daemon's build, not whichever one PATH finds.
    core_exe: Option<String>,
}

impl Channel {
    /// The variables the Tool command is handed: the socket, its token, and (when this engine's
    /// binary is found) `WICKED_CORE_EXE`, the one name the command resolves `wicked-core` by.
    pub(crate) fn env(&self) -> Vec<(String, String)> {
        let mut env = vec![
            (
                SOCK_ENV.to_string(),
                self.sock.to_string_lossy().into_owned(),
            ),
            (TOKEN_ENV.to_string(), self.token.clone()),
        ];
        if let Some(exe) = &self.core_exe {
            env.push((
                crate::gate_hook::WICKED_CORE_EXE_ENV.to_string(),
                exe.clone(),
            ));
        }
        env
    }

    /// (core#1906) End the step's calls: no connection starts a call from here, and each call
    /// still running has its seat cancelled (the runner's own cancel of that call's run id, the
    /// one a timed-out call gets). Idempotent.
    pub(crate) fn close(&self) {
        (self.closer())()
    }

    /// A handle that runs [`Channel::close`] from elsewhere (the Tool runner, when the command's
    /// own process exits).
    pub(crate) fn closer(&self) -> impl Fn() + Send + 'static {
        let (stop, calls, runner) = (self.stop.clone(), self.calls.clone(), self.runner.clone());
        move || {
            stop.store(true, Ordering::SeqCst);
            for id in calls.close() {
                runner.on_run_complete(&id);
            }
        }
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        self.close();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The prefix of a channel's directory under the temp dir: `wmc-<daemon pid>-<12 hex>`.
const DIR_PREFIX: &str = "wmc-";

/// Arm a channel for unit `ord` of `run_id`: a private directory (mode 0700) with one socket in
/// it, and a listener thread serving each connection on its own thread. `admit` is the run's
/// seats per family ([`admission`]).
#[cfg(unix)]
pub(crate) fn arm(
    runner: Arc<dyn crate::workflow::StepRunner>,
    admit: Admission,
    run_id: &str,
    ord: u32,
) -> std::io::Result<Channel> {
    use std::os::unix::fs::DirBuilderExt;
    use std::os::unix::net::UnixListener;
    let id = uuid::Uuid::new_v4().simple().to_string();
    // Short: a socket path must fit `sun_path` (104 bytes on macOS). The daemon's pid names the
    // owner, so a later daemon's sweep removes it only once this process is gone ([`sweep_stale`]).
    let dir =
        std::env::temp_dir().join(format!("{DIR_PREFIX}{}-{}", std::process::id(), &id[..12]));
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
    let calls = Arc::new(StepCalls::default());
    // The channel owns the directory from here: an early return below removes it.
    let channel = Channel {
        dir,
        sock,
        token: token.clone(),
        stop: Arc::new(AtomicBool::new(false)),
        calls: calls.clone(),
        runner: runner.clone(),
        core_exe: crate::execute_wrapped::resolve_wicked_core_exe_opt(),
    };
    let seats = Arc::new(Seats {
        runner,
        admit,
        calls,
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
                            // Refused by name, here on the listener: one short write to an empty
                            // socket, no read, so a burst of these never holds up the next accept.
                            refuse_now(&stream, &over_capacity());
                            continue;
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
    _admit: Admission,
    _run_id: &str,
    _ord: u32,
) -> std::io::Result<Channel> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "a Tool step's model-call channel needs a unix socket",
    ))
}

/// What a connection past [`MAX_CONNECTIONS`] is told.
fn over_capacity() -> String {
    format!("the channel is serving {MAX_CONNECTIONS} calls; this one was not started")
}

/// Answer `why` on `stream` and close it, without reading the request. The client reads this
/// answer even when its own send then fails ([`call_at`]).
#[cfg(unix)]
fn refuse_now(stream: &std::os::unix::net::UnixStream, why: &str) {
    let _ = stream.set_nonblocking(true);
    if let Ok(mut line) = serde_json::to_string(&Reply::refused(why)) {
        line.push('\n');
        let mut out = stream;
        let _ = out.write_all(line.as_bytes());
    }
    let _ = stream.shutdown(std::net::Shutdown::Write);
}

#[cfg(unix)]
fn serve_one(stream: std::os::unix::net::UnixStream, seats: &Seats, stop: &AtomicBool) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(IO_DEADLINE));
    let _ = stream.set_write_timeout(Some(IO_DEADLINE));
    let reply = match read_request(&stream) {
        // A connection opened while the step ran starts no call once the step has ended (a fast
        // path: the admission under [`StepCalls`] is the authority).
        Ok(_) if stop.load(Ordering::SeqCst) => Reply::refused(STEP_ENDED),
        Ok(req) => seats.answer(&req),
        Err(why) => Reply::refused(why),
    };
    let mut out = &stream;
    if let Ok(mut line) = serde_json::to_string(&reply) {
        line.push('\n');
        let _ = out.write_all(line.as_bytes());
        let _ = out.flush();
    }
    let _ = stream.shutdown(std::net::Shutdown::Write);
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

/// (C1) Remove the channel directories a daemon that is gone left under the temp dir (a crash or a
/// kill skips [`Channel`]'s drop). A directory goes only when its name parses as
/// `wmc-<pid>-<12 hex>`, it is a real directory (not a symlink) owned by this user, and its pid is
/// provably gone (`kill(pid, 0)` answers ESRCH). Everything else stays: a live or reused pid, a
/// probe that answers anything else, another user's directory, and the older unnamed `wmc-<12 hex>`
/// directories, whose owner cannot be proven gone. Run once when the actor starts.
#[cfg(unix)]
pub(crate) fn sweep_stale() {
    sweep_stale_in(&std::env::temp_dir(), |pid| {
        // SAFETY: signal 0 sends nothing; it only reports whether `pid` exists.
        if unsafe { libc::kill(pid, 0) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
        }
    });
}

#[cfg(not(unix))]
pub(crate) fn sweep_stale() {}

/// The pid a channel directory's name gives, when the name is exactly `wmc-<pid>-<12 hex>`.
#[cfg(unix)]
fn owner_pid(name: &str) -> Option<libc::pid_t> {
    let rest = name.strip_prefix(DIR_PREFIX)?;
    let (pid, id) = rest.split_once('-')?;
    let well_formed = !pid.is_empty()
        && pid.bytes().all(|b| b.is_ascii_digit())
        && id.len() == 12
        && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    let pid: libc::pid_t = pid.parse().ok().filter(|_| well_formed)?;
    (pid > 1).then_some(pid)
}

#[cfg(unix)]
fn sweep_stale_in(dir: &std::path::Path, probe: impl Fn(libc::pid_t) -> Result<(), i32>) {
    use std::os::unix::fs::MetadataExt;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    // SAFETY: geteuid has no preconditions.
    let me = unsafe { libc::geteuid() };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(pid) = name.to_str().and_then(owner_pid) else {
            continue;
        };
        let path = e.path();
        let Ok(md) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if sweepable(md.file_type().is_dir(), md.uid(), me, || probe(pid)) {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// Whether a parsed channel directory goes: a real directory (not a symlink) of this user whose
/// owner is provably gone. The probe runs only for such a directory.
#[cfg(unix)]
fn sweepable(real_dir: bool, uid: u32, me: u32, probe: impl FnOnce() -> Result<(), i32>) -> bool {
    real_dir && uid == me && probe() == Err(libc::ESRCH)
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
    // A channel that refuses early (over capacity, a request over its limit) answers and closes
    // before reading all of it, so this send can fail; its answer is still there to read, and it
    // says why.
    let sent = (&stream).write_all(line.as_bytes());
    let mut answer = String::new();
    let read = BufReader::new(&stream).read_line(&mut answer);
    match (sent, read) {
        (_, Ok(n)) if n > 0 => {}
        (Err(e), _) => return Err(format!("could not send the request: {e}")),
        (Ok(()), Err(e)) => return Err(format!("no answer from the channel: {e}")),
        (Ok(()), Ok(_)) => return Err("no answer from the channel: it closed".to_string()),
    }
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

    /// The seats of a run whose launch roster is `roster`, with no bench.
    fn adm(roster: Vec<crate::AgenticCli>) -> Admission {
        admission(&roster, &[], &[])
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
            adm(vec![seat("claude"), seat("codex")]),
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
        let ch = arm(
            echo.clone(),
            adm(vec![seat("claude"), seat("codex")]),
            "run-c",
            1,
        )
        .unwrap();
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
            declared_in(&decl, &|| shipped.clone(), preset, u, || Some(repo.clone()))
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
        assert!(!shipped_step(Some("domain-extraction"), &real, || Some(
            repo.clone()
        )));
    }

    /// A connection opened while the step ran starts no call after the step ends.
    #[test]
    fn a_connection_left_open_starts_no_call_after_the_step_ends() {
        use std::os::unix::net::UnixStream;
        let echo = Arc::new(Echo::default());
        let ch = arm(echo.clone(), adm(vec![seat("claude")]), "run-1", 2).unwrap();
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
        assert_eq!(reply.error.as_deref(), Some(STEP_ENDED));
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
        let ch = arm(echo.clone(), adm(vec![benched]), "run-1", 2).unwrap();
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
        let ch = arm(echo, adm(vec![seat("claude")]), "run-1", 2).unwrap();
        let (sock, token) = sock_and_token(&ch);
        let err = call_at(&sock, &token, "claude", "p", None, None).unwrap_err();
        assert!(err.contains("the `claude` seat failed"), "{err}");
    }

    /// Dropping the channel (the Tool command exited) removes its socket: no later call lands.
    #[test]
    fn the_channel_closes_with_its_command() {
        let echo = Arc::new(Echo::default());
        let ch = arm(echo, adm(vec![seat("claude")]), "run-1", 2).unwrap();
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
            let err = call_from_env("claude", "p", None, None).unwrap_err();
            assert!(err.contains(SOCK_ENV), "{err}");
        }
    }

    // ---- C1a (core#1905, core#1906, the bounds, the sweep) ----

    fn bench(cli: &str, reason: &str, governed_only: bool) -> crate::domain::BenchedSeat {
        crate::domain::BenchedSeat {
            cli: cli.into(),
            reason: reason.into(),
            source: "launcher".into(),
            governed_only,
            benched_until: None,
            strikes: 0,
            replaced: None,
        }
    }

    fn unusable(key: &str, why: &str) -> crate::AgenticCli {
        let mut c = seat(key);
        c.health = Some(SeatHealth {
            usable: false,
            reason: Some(why.into()),
        });
        c
    }

    /// (core#1905) The run's own seats decide: a benched codex, an unusable claude and a family
    /// the launch roster lacks are each refused by name before any seat runs; a governed-only
    /// bench (the host floor's, which binds governed work only) does not keep a seat from a call.
    #[test]
    fn a_benched_unusable_or_absent_seat_is_refused_by_name() {
        let roster = [seat("claude"), seat("codex")];
        let a = admission(&roster, &[bench("codex", "signed out", false)], &[]);
        assert!(a["claude"].is_ok());
        let e = a["codex"].clone().unwrap_err();
        assert!(
            e.contains("`codex` seat is benched for this run: signed out (launcher)"),
            "{e}"
        );
        // the launch bench counts too
        let a = admission(&roster, &[], &[bench("codex", "worktree", false)]);
        assert!(a["codex"].is_err());
        // a governed-only bench does not bind an ungoverned call
        let a = admission(&roster, &[bench("codex", "host floor", true)], &[]);
        assert!(a["codex"].is_ok());
        let a = admission(&[unusable("claude", "quota"), seat("codex")], &[], &[]);
        let e = a["claude"].clone().unwrap_err();
        assert!(e.contains("`claude` seat is not usable: quota"), "{e}");
        let a = admission(&[seat("claude")], &[], &[]);
        assert_eq!(
            a["codex"].clone().unwrap_err(),
            "no `codex` seat is on this run's roster"
        );
        // through the channel: refused by name, and no seat runs
        let echo = Arc::new(Echo::default());
        let ch = arm(
            echo.clone(),
            admission(&roster, &[bench("codex", "signed out", false)], &[]),
            "run-b",
            1,
        )
        .unwrap();
        let (sock, token) = sock_and_token(&ch);
        let e = call_at(&sock, &token, "codex", "p", None, None).unwrap_err();
        assert!(e.contains("benched for this run: signed out"), "{e}");
        assert!(echo.seen.lock().unwrap().is_empty());
    }

    /// (core#1905) A run configured with a seat INSTANCE (`claude#2`) gets its calls on that
    /// instance, with its own invocation: the family, the answer cap and the tool-free posture
    /// read its CLI. A bench of the instance benches only it; a bench of the CLI benches every
    /// instance; an unusable first instance gives way to the next.
    #[test]
    fn a_seat_instance_answers_for_its_family() {
        let mut two = seat("claude#2");
        two.headless_invocation = "claude -p --profile two \"{PROMPT}\"".into();
        let echo = Arc::new(Echo::default());
        let ch = arm(echo.clone(), adm(vec![two.clone()]), "run-i", 1).unwrap();
        let (sock, token) = sock_and_token(&ch);
        assert_eq!(
            call_at(&sock, &token, "claude", "w", None, Some(64000)).unwrap(),
            "claude#2: w"
        );
        let seen = echo.seen.lock().unwrap();
        assert_eq!(seen[0].0.as_deref(), Some("claude#2"));
        assert_eq!(
            seen[0].3.as_deref(),
            Some("claude -p --profile two \"{PROMPT}\"")
        );
        assert_eq!(seen[0].4, crate::write_posture::WritePosture::ToolLess);
        assert_eq!(*echo.launch.lock().unwrap(), vec![(Some(64000), true)]);
        drop(seen);
        let roster = [unusable("claude", "signed out"), two.clone()];
        let a = admission(&roster, &[], &[]);
        assert_eq!(a["claude"].as_ref().unwrap().key, "claude#2");
        let a = admission(&roster, &[bench("claude#2", "quota", false)], &[]);
        let e = a["claude"].clone().unwrap_err();
        assert!(
            e.contains("`claude` seat is not usable") && e.contains("`claude#2` seat is benched"),
            "{e}"
        );
        let a = admission(
            &[seat("claude"), two],
            &[bench("claude", "quota", false)],
            &[],
        );
        assert!(a["claude"].is_err(), "a CLI bench benches its instances");
    }

    /// (C1) The run's repo is read only for a declared step of a shipped preset: never for a
    /// repo-defined preset, a run with no preset, or an undeclared step.
    #[test]
    fn the_repo_is_looked_up_only_for_a_declared_step() {
        let looked = std::cell::Cell::new(0);
        let repo = || {
            looked.set(looked.get() + 1);
            None
        };
        let u = unit("domain-run", &["true".to_string()]);
        assert!(!shipped_step(Some("my-flow"), &u, repo));
        assert!(!shipped_step(None, &u, repo));
        assert!(!shipped_step(
            Some("domain-extraction"),
            &unit("domain-graph", &["true".to_string()]),
            repo
        ));
        assert_eq!(looked.get(), 0);
        let decl = [("p", "s")];
        let shipped = || {
            vec![(
                "p",
                vec![crate::plan::PlanStep {
                    catalog: "run".into(),
                    id: "s".into(),
                    executor: Some(crate::workflow::PhaseExecutor::Tool {
                        cmd: vec!["true".into()],
                    }),
                    ..Default::default()
                }],
            )]
        };
        assert!(declared_in(
            &decl,
            &shipped,
            Some("p"),
            &unit("s", &["true".to_string()]),
            repo
        ));
        assert_eq!(looked.get(), 1);
    }

    /// A seat runner that holds each call until released or until its own run id is cancelled,
    /// and records every cancel it is handed.
    #[derive(Default)]
    struct Hold {
        st: Mutex<HoldState>,
        cv: std::sync::Condvar,
        /// Ignores its cancels: it stops only when released (a seat slow to stop).
        deaf: bool,
    }

    #[derive(Default)]
    struct HoldState {
        entered: Vec<String>,
        cancelled: Vec<String>,
        open: bool,
    }

    impl crate::workflow::StepRunner for Hold {
        fn run_unit(&self, i: &crate::workflow::StepInput) -> crate::workflow::StepOutput {
            let id = i.run_id.clone();
            let mut st = self.st.lock().unwrap();
            st.entered.push(id.clone());
            self.cv.notify_all();
            let st = self
                .cv
                .wait_while(st, |s| !s.open && (self.deaf || !s.cancelled.contains(&id)))
                .unwrap();
            let cancelled = st.cancelled.contains(&id);
            drop(st);
            crate::workflow::StepOutput {
                run_id: id,
                unit_ix: 0,
                attempt: 0,
                output: "held answer".into(),
                status: if cancelled {
                    crate::workflow::StepStatus::Cancelled
                } else {
                    crate::workflow::StepStatus::Ok
                },
                usage: None,
                files: Vec::new(),
                tools: Vec::new(),
                governed: false,
            }
        }

        fn on_run_complete(&self, run_id: &str) {
            self.st.lock().unwrap().cancelled.push(run_id.to_string());
            self.cv.notify_all();
        }
    }

    impl Hold {
        fn wait_entered(&self, n: usize) {
            let st = self.st.lock().unwrap();
            let (st, t) = self
                .cv
                .wait_timeout_while(st, Duration::from_secs(20), |s| s.entered.len() < n)
                .unwrap();
            assert!(
                !t.timed_out(),
                "{} of {n} calls reached the seat",
                st.entered.len()
            );
        }

        fn release(&self) {
            self.st.lock().unwrap().open = true;
            self.cv.notify_all();
        }

        fn cancelled(&self) -> Vec<String> {
            self.st.lock().unwrap().cancelled.clone()
        }

        fn entered(&self) -> Vec<String> {
            self.st.lock().unwrap().entered.clone()
        }
    }

    fn call_in_thread(
        sock: &str,
        token: &str,
        prompt: String,
    ) -> std::thread::JoinHandle<Result<String, String>> {
        let (sock, token) = (sock.to_string(), token.to_string());
        std::thread::spawn(move || call_at(&sock, &token, "claude", &prompt, Some(60), None))
    }

    /// (core#1906) Ending the step cancels a call still running: its seat gets the runner's
    /// cancel of that call's own run id (never the asking run's), the call answers "the step
    /// ended" whatever its seat then returned, and nothing is left running.
    #[test]
    fn the_steps_end_cancels_a_running_call() {
        let hold = Arc::new(Hold::default());
        let ch = arm(hold.clone(), adm(vec![seat("claude")]), "run-x", 1).unwrap();
        let (sock, token) = sock_and_token(&ch);
        let call = call_in_thread(&sock, &token, "p".into());
        hold.wait_entered(1);
        let started = std::time::Instant::now();
        ch.close();
        let err = call.join().unwrap().unwrap_err();
        assert_eq!(err, STEP_ENDED);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(hold.cancelled(), hold.entered(), "the call's own id");
        assert!(hold.cancelled()[0].starts_with("council-modelcallrunx1-"));
        // a second close cancels nothing more
        ch.close();
        drop(ch);
        assert_eq!(hold.cancelled().len(), 1);
    }

    /// (core#1906) A call that answered before the step ended is never cancelled (a cancel would
    /// leave a tombstone in the runner for the daemon's life).
    #[test]
    fn a_call_done_before_the_steps_end_is_not_cancelled() {
        let hold = Arc::new(Hold::default());
        hold.release();
        let ch = arm(hold.clone(), adm(vec![seat("claude")]), "run-d", 1).unwrap();
        let (sock, token) = sock_and_token(&ch);
        assert_eq!(
            call_at(&sock, &token, "claude", "p", None, None).unwrap(),
            "held answer"
        );
        drop(ch);
        assert!(hold.cancelled().is_empty());
    }

    /// (core#1906) The admission is the authority: a call admitted after the step ended (a
    /// connection that passed the listener's stop check just before the end) never reaches its
    /// seat, and says why.
    #[test]
    fn a_call_admitted_after_the_steps_end_never_runs() {
        use wicked_council::types::BallotCarrier;
        let hold = Arc::new(Hold::default());
        let calls = Arc::new(StepCalls::default());
        assert!(calls.close().is_empty());
        let carrier = crate::decision::SeatBallots::new(hold.clone(), Some("run-a"))
            .model_calls()
            .step_calls(calls);
        let out = carrier.run_ballot(&seat("claude"), "t", "p", Duration::from_secs(5), None, 64);
        assert!(out.is_err());
        assert!(carrier.step_ended());
        assert!(hold.entered().is_empty());
    }

    /// (core#1906) A call past its budget whose seat has not stopped (held by the reaper) is still
    /// the step's: a close then cancels it, and the call answers "the step ended".
    #[test]
    fn a_call_past_its_budget_whose_seat_still_runs_is_ended_by_the_close() {
        use wicked_council::types::BallotCarrier;
        let hold = Arc::new(Hold {
            deaf: true,
            ..Default::default()
        });
        let calls = Arc::new(StepCalls::default());
        let carrier = crate::decision::SeatBallots::new(hold.clone(), Some("run-t"))
            .model_calls()
            .step_calls(calls.clone())
            .with_drain(Duration::from_millis(50));
        let out = carrier.run_ballot(
            &seat("claude"),
            "t",
            "p",
            Duration::from_millis(200),
            None,
            64,
        );
        assert!(out.is_err(), "past its budget");
        assert!(!carrier.step_ended());
        assert_eq!(
            carrier.reaper().reap(Duration::from_millis(10)),
            1,
            "still running"
        );
        // the close still finds it, and cancels it by its own id
        let ids = calls.close();
        assert_eq!(ids, hold.entered());
        assert!(carrier.step_ended());
        hold.release();
        assert_eq!(carrier.reaper().reap(Duration::from_secs(5)), 0);
        assert!(
            carrier.step_ended(),
            "released after the close: still ended"
        );
    }

    /// (core#1906) Two channels of one run are separate: ending one step leaves the other's call
    /// running, and it answers.
    #[test]
    fn ending_one_step_leaves_another_steps_call_running() {
        let hold = Arc::new(Hold::default());
        let a = arm(hold.clone(), adm(vec![seat("claude")]), "run-2", 1).unwrap();
        let b = arm(hold.clone(), adm(vec![seat("claude")]), "run-2", 2).unwrap();
        let (sa, ta) = sock_and_token(&a);
        let (sb, tb) = sock_and_token(&b);
        let ca = call_in_thread(&sa, &ta, "a".into());
        hold.wait_entered(1);
        let cb = call_in_thread(&sb, &tb, "b".into());
        hold.wait_entered(2);
        drop(a);
        assert_eq!(ca.join().unwrap().unwrap_err(), STEP_ENDED);
        assert_eq!(hold.cancelled().len(), 1);
        hold.release();
        assert_eq!(cb.join().unwrap().unwrap(), "held answer");
        drop(b);
        assert_eq!(hold.cancelled().len(), 1);
    }

    /// Bounds, through the client the `model-call` command runs (its `call_at`): a request over
    /// 8 MiB is refused naming the limit, and a connection that sends nothing holds up no other.
    #[test]
    fn an_oversized_request_is_refused_and_a_silent_one_holds_up_nothing() {
        use std::os::unix::net::UnixStream;
        let hold = Arc::new(Hold::default());
        hold.release();
        let ch = arm(hold.clone(), adm(vec![seat("claude")]), "run-o", 1).unwrap();
        let (sock, token) = sock_and_token(&ch);
        let big = "x".repeat(40 * 1024 * 1024);
        let e = call_at(&sock, &token, "claude", &big, None, None).unwrap_err();
        assert!(
            e.contains(&format!("over {MAX_REQUEST_BYTES} bytes")),
            "{e}"
        );
        let silent = UnixStream::connect(&sock).unwrap();
        let started = std::time::Instant::now();
        assert_eq!(
            call_at(&sock, &token, "claude", "p", None, None).unwrap(),
            "held answer"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        drop(silent);
        assert_eq!(
            hold.entered().len(),
            1,
            "the oversized request reached no seat"
        );
    }

    /// With 16 calls held, a 17th (with a 4 MiB prompt) and a burst of 32 more are refused by
    /// name, promptly, and the held 16 then answer.
    #[test]
    fn calls_past_the_connection_limit_are_refused_by_name_and_the_held_ones_answer() {
        let hold = Arc::new(Hold::default());
        let ch = arm(hold.clone(), adm(vec![seat("claude")]), "run-c", 1).unwrap();
        let (sock, token) = sock_and_token(&ch);
        let held: Vec<_> = (0..MAX_CONNECTIONS)
            .map(|i| call_in_thread(&sock, &token, format!("h{i}")))
            .collect();
        hold.wait_entered(MAX_CONNECTIONS);
        let started = std::time::Instant::now();
        let four = "y".repeat(4 * 1024 * 1024);
        let e = call_at(&sock, &token, "claude", &four, None, None).unwrap_err();
        assert_eq!(e, over_capacity());
        let burst: Vec<_> = (0..32)
            .map(|i| call_in_thread(&sock, &token, format!("b{i}")))
            .collect();
        for b in burst {
            assert_eq!(b.join().unwrap().unwrap_err(), over_capacity());
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        hold.release();
        for h in held {
            assert_eq!(h.join().unwrap().unwrap(), "held answer");
        }
        assert_eq!(hold.entered().len(), MAX_CONNECTIONS);
    }

    /// A seat runner over the real wrapped runner's cancel registry: each call waits for the
    /// test's go, then registers its cancel token the way a wrapped worker does, records whether it
    /// was born cancelled, and returns a SUCCESS either way.
    #[derive(Default)]
    struct LateRegistration {
        wrapped: crate::execute_wrapped::WrappedCliStepRunner,
        st: Mutex<(usize, bool, Vec<bool>)>,
        cv: std::sync::Condvar,
    }

    impl crate::workflow::StepRunner for LateRegistration {
        fn run_unit(&self, i: &crate::workflow::StepInput) -> crate::workflow::StepOutput {
            let st = self.st.lock().unwrap();
            let mut st = st;
            st.0 += 1;
            self.cv.notify_all();
            let mut st = self.cv.wait_while(st, |s| !s.1).unwrap();
            let token = self.wrapped.register_token(i);
            st.2.push(token.load(std::sync::atomic::Ordering::Relaxed));
            drop(st);
            self.wrapped.unregister_token(i, &token);
            crate::workflow::StepOutput {
                run_id: i.run_id.clone(),
                unit_ix: 0,
                attempt: 0,
                output: "a late answer".into(),
                status: crate::workflow::StepStatus::Ok,
                usage: None,
                files: Vec::new(),
                tools: Vec::new(),
                governed: false,
            }
        }

        fn on_run_complete(&self, run_id: &str) {
            crate::workflow::StepRunner::on_run_complete(&self.wrapped, run_id);
        }
    }

    /// (core#1906) The step ends after a call was admitted and before its seat registered with the
    /// wrapped runner: the late registration under the call's own id is born cancelled (the
    /// runner's tombstone), and the call answers "the step ended" although its seat then returned
    /// a success (the close wins over a completion not yet released).
    #[test]
    fn a_call_admitted_before_the_steps_end_and_registered_after_is_born_cancelled() {
        let late = Arc::new(LateRegistration::default());
        let ch = arm(late.clone(), adm(vec![seat("claude")]), "run-r", 1).unwrap();
        let (sock, token) = sock_and_token(&ch);
        let call = call_in_thread(&sock, &token, "p".into());
        {
            let st = late.st.lock().unwrap();
            let (_st, t) = late
                .cv
                .wait_timeout_while(st, Duration::from_secs(20), |s| s.0 < 1)
                .unwrap();
            assert!(!t.timed_out());
        }
        ch.close();
        late.st.lock().unwrap().1 = true;
        late.cv.notify_all();
        assert_eq!(call.join().unwrap().unwrap_err(), STEP_ENDED);
        assert_eq!(late.st.lock().unwrap().2, vec![true], "born cancelled");
    }

    /// (core#1906) The channel ends with the Tool command's own process, through the hook the arm
    /// site sets: while a descendant in its own session (out of the group kill's reach) still holds
    /// the command's stdout, a running call is cancelled and a later call is refused.
    #[test]
    fn the_channel_ends_with_its_command_though_a_descendant_holds_its_pipe() {
        let hold = Arc::new(Hold::default());
        let ch = arm(hold.clone(), adm(vec![seat("claude")]), "run-l", 1).unwrap();
        let (sock, token) = sock_and_token(&ch);
        let live = call_in_thread(&sock, &token, "live".into());
        hold.wait_entered(1);
        let ready = std::env::temp_dir().join(format!(
            "wmc-life-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        // The descendant leaves the group, says so, then holds stdout for 4 s; the leader waits
        // for the word, so the group kill at its exit cannot reach the descendant.
        let script = format!(
            "perl -e 'use POSIX; POSIX::setsid() or die; open(my $f, \">\", $ARGV[0]) or die; \
             close $f; sleep 4' '{r}' & while [ ! -e '{r}' ]; do sleep 0.05; done; echo started",
            r = ready.display()
        );
        let closer = ch.closer();
        let started = std::time::Instant::now();
        let tool = std::thread::spawn(move || {
            let _hook = crate::actor::ChildExitHook::set(Box::new(closer));
            let cmd = vec!["sh".to_string(), "-c".to_string(), script];
            crate::actor::run_tool_cmd(&cmd, None, &[], &|| None, &|_| {}, &|_| {})
        });
        assert_eq!(live.join().unwrap().unwrap_err(), STEP_ENDED);
        let ended = started.elapsed();
        assert!(call_at(&sock, &token, "claude", "later", None, None).is_err());
        let (out, status, _) = tool.join().unwrap();
        let _ = std::fs::remove_file(&ready);
        assert_eq!(status, crate::workflow::StepStatus::Ok, "{out}");
        assert!(
            started.elapsed() >= Duration::from_secs(3),
            "the descendant held the pipe (the premise): {:?}",
            started.elapsed()
        );
        assert!(ended < Duration::from_millis(2500), "{ended:?}");
        assert_eq!(hold.cancelled().len(), 1);
    }

    /// (C1) Only a directory named `wmc-<pid>-<12 hex>` gives an owner pid.
    #[test]
    fn a_channel_directory_name_parses_strictly() {
        assert_eq!(owner_pid("wmc-4242-0123456789ab"), Some(4242));
        for bad in [
            "wmc-0123456789ab",
            "wmc-1-0123456789ab",
            "wmc-0-0123456789ab",
            "wmc--0123456789ab",
            "wmc-+42-0123456789ab",
            "wmc-42-0123456789AB",
            "wmc-42-0123456789a",
            "wmc-42-0123456789abc",
            "wmc-99999999999999999999-0123456789ab",
            "xmc-42-0123456789ab",
        ] {
            assert_eq!(owner_pid(bad), None, "{bad}");
        }
    }

    /// (C1) A channel directory goes only when it is a real directory of this user and its pid is
    /// provably gone; a live pid, any other probe answer, a symlink and another user's directory
    /// stay, and the probe is never run for those last two.
    #[test]
    fn only_a_gone_daemons_own_directory_is_swept() {
        let gone = || Err(libc::ESRCH);
        assert!(sweepable(true, 7, 7, gone));
        assert!(!sweepable(true, 7, 7, || Ok(())));
        assert!(!sweepable(true, 7, 7, || Err(libc::EPERM)));
        assert!(!sweepable(true, 7, 7, || Err(0)));
        let never = || -> Result<(), i32> { panic!("probed") };
        assert!(!sweepable(false, 7, 7, never));
        assert!(!sweepable(true, 8, 7, never));
    }

    #[test]
    fn the_sweep_removes_only_dead_daemons_channel_directories() {
        let base = std::env::temp_dir().join(format!(
            "wmc-sweep-test-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir(&base).unwrap();
        let d = base.as_path();
        let mk = |n: &str| {
            std::fs::create_dir(d.join(n)).unwrap();
            std::fs::write(d.join(n).join("s"), "").unwrap();
        };
        mk("wmc-111-0123456789ab"); // dead
        mk("wmc-222-0123456789ab"); // alive
        mk("wmc-333-0123456789ab"); // EPERM
        mk("wmc-0123456789ab"); // legacy
        mk("wmc-x1-0123456789ab"); // malformed
        mk("other");
        std::fs::create_dir(d.join("target")).unwrap();
        std::os::unix::fs::symlink(d.join("target"), d.join("wmc-444-0123456789ab")).unwrap();
        sweep_stale_in(d, |pid| match pid {
            111 | 444 => Err(libc::ESRCH),
            333 => Err(libc::EPERM),
            _ => Ok(()),
        });
        let mut left: Vec<_> = std::fs::read_dir(d)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "other",
                "target",
                "wmc-0123456789ab",
                "wmc-222-0123456789ab",
                "wmc-333-0123456789ab",
                "wmc-444-0123456789ab",
                "wmc-x1-0123456789ab"
            ]
        );
        std::fs::remove_dir_all(d).unwrap();
    }

    /// (C1) A channel is named for the daemon that armed it, so a later daemon's sweep can tell,
    /// and hands its command this engine's binary.
    #[test]
    fn a_channel_directory_names_its_daemon() {
        // other tests set WICKED_CORE_EXE under this lock; hold it across both resolutions
        let _env = crate::test_env::ENV_LOCK
            .read()
            .unwrap_or_else(|p| p.into_inner());
        let ch = arm(Arc::new(Echo::default()), adm(vec![]), "run-n", 1).unwrap();
        let name = ch.dir.file_name().unwrap().to_str().unwrap().to_string();
        assert_eq!(owner_pid(&name), Some(std::process::id() as libc::pid_t));
        // the command resolves `wicked-core` to this engine's own binary
        let exe = ch
            .env()
            .into_iter()
            .find(|(k, _)| k == crate::gate_hook::WICKED_CORE_EXE_ENV)
            .map(|(_, v)| v);
        assert_eq!(exe, crate::execute_wrapped::resolve_wicked_core_exe_opt());
        assert!(exe.is_some());
    }
}
