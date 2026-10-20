//! The gate hook for a claude seat in the container floor (slice C3).
//!
//! A governed claude unit is judged before every tool call by `wicked-core gate-hook`, a HOST
//! binary claude runs as a command hook. Inside a Linux seat container that binary does not
//! exist, so the seat's settings name a small command hook instead ([`hook_command`]): it posts
//! the same PreToolUse payload with the floor's own static busybox (`/floor/busybox wget`, from
//! the engine's read-only floor image) to `http://agw:<HOOK_PORT>/hook/<token>`, the daemon's
//! gateway routes it to this relay on the host's loopback, and the relay runs the unchanged host
//! `gate-hook` with exactly the environment the seat's command would have carried, then answers
//! in claude's hook JSON, which the command prints as its own answer.
//!
//! Why a command and not claude's `type: "http"` hook (core#988): Claude Code refuses an HTTP
//! hook whose host resolves to a private or link-local address (only loopback is exempt), and
//! `agw` is the seat network's private address, so every call was blocked before it reached the
//! relay. A command hook is outside that guard; it still rides the one egress the seat has (the
//! gateway's hook route) and, with `onFailure: "block"`, any failure (an unreachable relay, a
//! non-2xx answer, a timeout) denies the call.
//!
//! **Glue only.** The judgement, the decisions log and the store are the existing gate hook's;
//! the relay maps its exit code to the hook's JSON answer: 0 → claude's allow decision (its own
//! stdout's object, if any, rides along; core#1027),
//! 2 → deny with its stderr as the reason (the gate hook recorded that decision itself). Anything
//! else is no verdict and the relay refuses the call itself: the gate hook failed (another exit
//! code), could not start, ran past the call's budget, or the request is not one it can judge (an
//! unknown token, a payload that is not a tool call). Every such refusal names its reason in the
//! answer and in the daemon's log, and is recorded in the unit's decisions log as an infra deny,
//! which the fold replays as a `governanceHookFired` deny and which denies the unit (core#997:
//! a refusal used to read only "the gate hook failed (exit 1)", with nothing logged or recorded).
//! A gate hook ended by a signal before it answered is run once more, but only when it
//! demonstrably left no verdict (the unit's decisions log did not change while it ran); one that
//! may have recorded a decision, or a second such ending, is refused. One deadline per call covers
//! every run's payload write, wait and output collection ([`judge`]). A hook request whose headers
//! fail is refused and recorded against the unit its token names. A token is minted per unit,
//! known only to that seat and the relay, and revoked when the unit ends; revocation waits for the
//! unit's calls still being judged, a gate-hook run starts and an allow is committed only while the
//! unit is live and before the call's deadline (both decided under the unit's lifecycle lock), and
//! a refusal that cannot be recorded in the unit's decisions log fails the unit at its end.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use wicked_apps_core::HardenedCommand;

/// The gateway port the seat's gate hook posts to (not a browser-blocked port).
pub(crate) const HOOK_PORT: u16 = 8711;
/// How long one tool call's judgement may take before it is denied.
const HOOK_TIMEOUT: Duration = Duration::from_secs(60);
/// The largest hook payload the relay reads (a tool call's input).
const MAX_BODY: usize = 8 * 1024 * 1024;

struct Relay {
    port: u16,
    units: Mutex<HashMap<String, Registered>>,
}

static RELAY: OnceLock<Option<Relay>> = OnceLock::new();

/// The relay's loopback port, starting it on first use. `None` when it cannot listen.
pub(crate) fn port() -> Option<u16> {
    relay().map(|r| r.port)
}

fn relay() -> Option<&'static Relay> {
    RELAY
        .get_or_init(|| {
            let listener = TcpListener::bind(("127.0.0.1", 0)).ok()?;
            let port = listener.local_addr().ok()?.port();
            std::thread::Builder::new()
                .name("wicked-hook-relay".into())
                .spawn(move || {
                    for conn in listener.incoming().flatten() {
                        // Bounded: a flood of connections is closed, never a thread each.
                        let Some(slot) = Slot::take() else {
                            overflow(conn);
                            continue;
                        };
                        std::thread::spawn(move || {
                            let _slot = slot;
                            serve(conn)
                        });
                    }
                })
                .ok()?;
            Some(Relay {
                port,
                units: Mutex::new(HashMap::new()),
            })
        })
        .as_ref()
}

/// A fresh per-unit token (the URL path segment the seat's hook calls).
pub(crate) fn mint_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// The relay URL a seat holding `token` posts its tool calls to (through the gateway).
pub(crate) fn hook_url(token: &str) -> String {
    format!("http://agw:{HOOK_PORT}/hook/{token}")
}

/// What a container seat's gate hook prints when the relay cannot answer (its stderr, which
/// claude shows as the deny reason).
pub(crate) const UNREACHABLE: &str =
    "the gate hook could not reach the engine relay; the call is denied (fail closed)";
/// What it prints when a 2xx answer is not a hook decision (a JSON object).
pub(crate) const NOT_A_DECISION: &str =
    "the relay answer is not a hook decision; the call is denied (fail closed)";

/// The command hook a container seat runs for every tool call (core#988): the payload claude
/// writes on its stdin is posted straight from that stdin (`--post-file -`, never a file another
/// process in the seat could rewrite between the write and the post) with the floor's busybox,
/// never a tool from the seat image. A failed post (no route, a non-2xx answer, a timeout) exits
/// 2, claude's blocking code, with [`UNREACHABLE`] as the reason; a 2xx answer is printed as the
/// hook's JSON only when it is a JSON object (the relay always answers one), else it exits 2
/// with [`NOT_A_DECISION`] (claude reads plain stdout as no decision, which would allow).
/// `onFailure: "block"` on the hook covers whatever this command itself cannot (it is killed, or
/// the shell is missing). `-Y off`: the seat's proxy variables never apply to the relay. `None`
/// when `token` is not the hex a relay token is (it is spliced into the command).
pub(crate) fn hook_command(token: &str) -> Option<String> {
    if token.is_empty() || !token.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let bb = crate::container_floor::FLOOR_BUSYBOX;
    let url = hook_url(token);
    let wait = HOOK_TIMEOUT.as_secs() + 25;
    Some(format!(
        "out=$({bb} wget -q -Y off -T {wait} --header 'Content-Type: application/json' \
         --post-file - -O - {url}) || {{ echo '{UNREACHABLE}' >&2; exit 2; }}; \
         case \"$out\" in '{{'*) printf '%s' \"$out\" ;; \
         *) echo '{NOT_A_DECISION}' >&2; exit 2 ;; esac"
    ))
}

/// What one unit's gate hook runs with: the seat command's explicit variables and its working
/// directory, as a command hook would have inherited them (relative tool paths and phase fences
/// resolve against the unit's tree, never the daemon's cwd). Engine-set, never seat-supplied.
#[derive(Debug, Clone)]
pub(crate) struct HookContext {
    /// `None` = removed from the seat command's env (agy gw-r1 F-03): removed for the hook too.
    pub(crate) env: Vec<(OsString, Option<OsString>)>,
    pub(crate) cwd: Option<std::path::PathBuf>,
}

/// A live registration: dropping it revokes the token, on every exit path (an early refusal, a
/// panic while the seat runs), so a token never outlives its unit. Revoking waits for the unit's
/// calls still being judged ([`Call`], each bounded by its deadline), so no gate-hook run of the
/// unit is in flight, or appends to its decisions log, once revocation returns.
#[derive(Debug)]
pub(crate) struct HookRegistration(String);

impl HookRegistration {
    /// Revoke the token now (the unit ended), and whether a refusal the relay made for the unit
    /// could not be recorded in its decisions log: a unit with one must not pass, whatever its
    /// log shows.
    pub(crate) fn finish(mut self) -> bool {
        revoke(&std::mem::take(&mut self.0))
    }
}

impl Drop for HookRegistration {
    fn drop(&mut self) {
        if !self.0.is_empty() {
            revoke(&self.0);
        }
    }
}

/// Bind `token` to the context the unit's gate hook runs with; `None` when no relay runs.
pub(crate) fn register(token: &str, ctx: HookContext) -> Option<HookRegistration> {
    let r = relay()?;
    let mut m = r.units.lock().ok()?;
    m.insert(
        token.to_string(),
        Registered {
            ctx,
            life: Arc::new(Life::default()),
        },
    );
    Some(HookRegistration(token.to_string()))
}

/// Revoke `token` (the unit ended): no new call of it is judged, and this waits (bounded) for the
/// calls already being judged. Returns whether a refusal for the unit went unrecorded.
pub(crate) fn revoke(token: &str) -> bool {
    let reg = relay().and_then(|r| r.units.lock().ok().and_then(|mut m| m.remove(token)));
    reg.is_some_and(|reg| reg.life.revoke_and_wait())
}

/// One registered unit: its hook context and its lifecycle.
#[derive(Clone)]
struct Registered {
    ctx: HookContext,
    life: Arc<Life>,
}

/// A registered unit's lifecycle, shared by its calls and its revocation.
#[derive(Default)]
struct Life {
    state: Mutex<LifeState>,
    idle: std::sync::Condvar,
    /// Held by the one judgement (or refusal record) of the unit in progress.
    judging: Mutex<()>,
}

#[derive(Default)]
struct LifeState {
    revoked: bool,
    /// Calls of the unit being judged now.
    calls: usize,
    /// A relay refusal for the unit could not be recorded in its decisions log.
    unrecorded: bool,
}

impl Life {
    /// Start judging one call of a live unit; `None` once it is revoked.
    fn enter(self: &Arc<Self>) -> Option<Call> {
        let mut s = self.state.lock().ok()?;
        if s.revoked {
            return None;
        }
        s.calls += 1;
        Some(Call(self.clone()))
    }

    fn revoke_and_wait(&self) -> bool {
        // Every call ends by its deadline; the margin covers its refusal's record.
        self.revoke_and_wait_within(HOOK_TIMEOUT + Duration::from_secs(10))
    }

    /// Revoke, wait up to `limit` for the calls being judged, and whether the unit must fail:
    /// a refusal went unrecorded, or a call was still active when the wait gave up (its refusal
    /// may still be pending, so the unit's log cannot be trusted to hold it).
    fn revoke_and_wait_within(&self, limit: Duration) -> bool {
        let Ok(mut s) = self.state.lock() else {
            return true;
        };
        s.revoked = true;
        let until = Instant::now() + limit;
        while s.calls > 0 {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            s = match self.idle.wait_timeout(s, left) {
                Ok((s, _)) => s,
                Err(_) => return true,
            };
        }
        s.unrecorded || s.calls > 0
    }

    /// The unit's judging turn, waited for until `deadline`; `None` past it.
    fn turn(&self, deadline: Instant) -> Option<std::sync::MutexGuard<'_, ()>> {
        loop {
            match self.judging.try_lock() {
                Ok(g) => return Some(g),
                Err(std::sync::TryLockError::Poisoned(p)) => return Some(p.into_inner()),
                Err(std::sync::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(std::sync::TryLockError::WouldBlock) => return None,
            }
        }
    }

    fn mark_unrecorded(&self) {
        if let Ok(mut s) = self.state.lock() {
            s.unrecorded = true;
        }
    }
}

/// One call being judged: it holds off its unit's revocation until it is answered.
struct Call(Arc<Life>);

impl Call {
    /// `f`'s answer while the unit is still live, decided under its lifecycle lock (so the unit is
    /// not revoked between the check and the decision); `None` once it is revoked.
    fn while_live<T>(&self, f: impl FnOnce() -> T) -> Option<T> {
        let s = self.0.state.lock().ok()?;
        (!s.revoked).then(f)
    }
}

impl Drop for Call {
    fn drop(&mut self) {
        if let Ok(mut s) = self.0.state.lock() {
            s.calls = s.calls.saturating_sub(1);
        }
        self.0.idle.notify_all();
    }
}

/// claude's PreToolUse hook answer for one gate-hook outcome.
///
/// An allowed call (exit 0) answers claude's documented allow decision
/// (`hookSpecificOutput.permissionDecision: "allow"`), never `{}` (core#1027): a command hook
/// that exits 0 with no decision sends the call through claude's own permission flow, and under
/// the engine's `--permission-mode acceptEdits` in a `-p` session that flow refuses every Bash
/// command it does not auto-approve (`node`, `env`, a compound command) and every read outside
/// the working tree ("This command requires approval", measured on the pinned 2.1.296 seat), with
/// nobody there to approve. The gate is the judge on this floor, so its allow is authoritative;
/// claude's own deny rules (the `--disallowedTools` fence) still block an allowed call, and
/// deny stays deny. A JSON object the gate hook printed rides along; a decision it already
/// states is kept, never upgraded.
pub(crate) fn answer(code: Option<i32>, stdout: &[u8], stderr: &[u8]) -> String {
    let deny = deny_answer;
    match code {
        Some(0) => {
            let out = String::from_utf8_lossy(stdout);
            // A command hook's plain stdout is a transcript note, not a decision.
            let mut v = match serde_json::from_str::<serde_json::Value>(out.trim()) {
                Ok(v @ serde_json::Value::Object(_)) => v,
                _ => serde_json::json!({}),
            };
            let obj = v.as_object_mut().expect("an object");
            let hso = obj
                .entry("hookSpecificOutput")
                .or_insert_with(|| serde_json::json!({}));
            if !hso.is_object() {
                *hso = serde_json::json!({});
            }
            let hso = hso.as_object_mut().expect("an object");
            hso.insert("hookEventName".into(), "PreToolUse".into());
            hso.entry("permissionDecision")
                .or_insert_with(|| "allow".into());
            v.to_string()
        }
        Some(2) => {
            let why = String::from_utf8_lossy(stderr);
            deny(why.trim())
        }
        Some(c) => deny(&format!(
            "the gate hook failed (exit {c}): {}; the call is denied (fail closed)",
            String::from_utf8_lossy(stderr).trim()
        )),
        None => deny("the gate hook did not finish in time; the call is denied (fail closed)"),
    }
}

/// claude's PreToolUse deny answer with `reason`.
fn deny_answer(reason: &str) -> String {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    })
    .to_string()
}

/// The token in a request line `POST /hook/<token> HTTP/1.1`, and whether it is a liveness probe.
fn route(request_line: &str) -> Option<Route> {
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?;
    let path = parts.next()?;
    match (method, path) {
        ("GET", "/ping") => Some(Route::Ping),
        ("POST", p) => {
            let token = p.strip_prefix("/hook/")?;
            (!token.is_empty() && token.chars().all(|c| c.is_ascii_hexdigit()))
                .then(|| Route::Hook(token.to_string()))
        }
        _ => None,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Route {
    Ping,
    Hook(String),
}

fn respond(mut conn: TcpStream, status: &str, body: &str) {
    let _ = write!(
        conn,
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
         connection: close\r\n\r\n{body}",
        body.len()
    );
}

/// The most a request line plus headers may take, and how many headers (agy gw-r1 F-06: a seat
/// reaches the relay through the gateway and must not grow the daemon's memory without bound).
const MAX_HEAD: usize = 64 * 1024;
const MAX_HEADERS: usize = 100;
/// Concurrent connections the relay serves; more are closed at once.
const MAX_CONNS: usize = 32;

static ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// One served connection, counted while it lives (a panic included).
struct Slot;

impl Slot {
    fn take() -> Option<Slot> {
        if ACTIVE.fetch_add(1, Ordering::SeqCst) >= MAX_CONNS {
            ACTIVE.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(Slot)
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A connection past [`MAX_CONNS`]: never served, and never read on the accepting thread. A
/// bounded pool of [`MAX_OVERFLOW`] short-lived workers reads its request line within one
/// absolute deadline ([`OVERFLOW_READ`]) and, when it names a live unit's token, marks that unit
/// to fail closed at its end: its call was refused (the seat's hook sees the close and denies) and
/// the refusal is not in its decisions log. Past the pool the connection is closed unread (a
/// flood that size cannot be attributed; each of its calls is still denied at the seat).
fn overflow(mut conn: TcpStream) {
    let Some(worker) = OverflowWorker::take() else {
        return;
    };
    std::thread::spawn(move || {
        let _worker = worker;
        let Some(token) = request_token_within(&mut conn, OVERFLOW_READ) else {
            return;
        };
        // Marked while holding a `Call`, so the unit's end waits for the mark (or, past its wait,
        // fails closed anyway); a unit that already ended is not live.
        let live = unit(&token).and_then(|reg| reg.life.enter().map(|call| (reg, call)));
        if let Some((reg, call)) = live {
            reg.life.mark_unrecorded();
            drop(call);
            // Closed only once its unit is marked.
            drop(conn);
            eprintln!(
                "wicked-core: the gate hook relay is at its connection limit; a call of a live \
                 unit was refused unrecorded, and the unit will fail closed"
            );
        }
    });
}

/// The overflow workers alive at once.
const MAX_OVERFLOW: usize = 8;
/// How long an overflow worker reads a request line, in all.
const OVERFLOW_READ: Duration = Duration::from_millis(500);

static OVERFLOW: AtomicUsize = AtomicUsize::new(0);

struct OverflowWorker;

impl OverflowWorker {
    fn take() -> Option<Self> {
        if OVERFLOW.fetch_add(1, Ordering::SeqCst) >= MAX_OVERFLOW {
            OVERFLOW.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(OverflowWorker)
    }
}

impl Drop for OverflowWorker {
    fn drop(&mut self) {
        OVERFLOW.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The hook token of `conn`'s request line, read within `limit` in all (the socket timeout is
/// recomputed before every read, so a client dripping bytes cannot extend it), at most 1 KiB.
fn request_token_within(conn: &mut TcpStream, limit: Duration) -> Option<String> {
    let deadline = Instant::now() + limit;
    let mut line = Vec::new();
    let mut buf = [0u8; 256];
    while !line.contains(&b'\n') {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || line.len() > 1024 {
            return None;
        }
        conn.set_read_timeout(Some(left)).ok()?;
        match conn.read(&mut buf) {
            Ok(0) | Err(_) => return None,
            Ok(n) => line.extend_from_slice(&buf[..n]),
        }
    }
    let end = line.iter().position(|b| *b == b'\n')?;
    match route(std::str::from_utf8(&line[..end]).ok()?) {
        Some(Route::Hook(t)) => Some(t),
        _ => None,
    }
}

/// Read one CRLF line of at most `budget` bytes; `None` past the budget or on a read error.
fn bounded_line(reader: &mut impl BufRead, budget: &mut usize) -> Option<String> {
    let mut line = Vec::new();
    let n = reader
        .take(*budget as u64 + 1)
        .read_until(b'\n', &mut line)
        .ok()?;
    if n > *budget {
        return None;
    }
    *budget -= n;
    String::from_utf8(line).ok()
}

/// A chunked body (a proxy may re-frame the hook's request), at most [`MAX_BODY`] bytes.
fn read_chunked(reader: &mut impl BufRead) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let mut budget = 1024;
        let size_line = bounded_line(reader, &mut budget)?;
        let size = usize::from_str_radix(size_line.trim().split(';').next()?.trim(), 16).ok()?;
        if size == 0 {
            // Trailers (none expected) up to the blank line.
            loop {
                let mut budget = 1024;
                let l = bounded_line(reader, &mut budget)?;
                if l == "\r\n" || l.is_empty() {
                    return Some(body);
                }
            }
        }
        if body.len() + size > MAX_BODY {
            return None;
        }
        let at = body.len();
        body.resize(at + size, 0);
        reader.read_exact(&mut body[at..]).ok()?;
        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf).ok()?;
    }
}

/// The payload is a PreToolUse tool call: a JSON object naming its tool. Anything else (an empty
/// or non-JSON body) is never handed to the gate hook, which would judge it as no call at all
/// (agy gw-r1 F-01: an empty body was allowed).
/// The tool it names, when it is one.
fn tool_call(body: &[u8]) -> Option<String> {
    let v = serde_json::from_slice::<serde_json::Value>(body).ok()?;
    let tool = v.get("tool_name")?.as_str()?.trim();
    (!tool.is_empty()).then(|| tool.to_string())
}

fn serve(conn: TcpStream) {
    let _ = conn.set_read_timeout(Some(Duration::from_secs(30)));
    let Ok(read_half) = conn.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(read_half);
    let mut budget = MAX_HEAD;
    let Some(line) = bounded_line(&mut reader, &mut budget) else {
        return respond(conn, "431 Request Header Fields Too Large", "{}");
    };
    // The route, and the token of a hook request, come from the request line alone, so a hook
    // request whose headers fail is a refusal like any other: answered with its reason, logged,
    // and recorded in the decisions log of the unit its token names. The unit is looked up when it
    // is needed (a refusal, the judgement), never held from here: a token revoked while the
    // request was still arriving names no unit.
    let route = route(&line);
    let token = match &route {
        Some(Route::Hook(t)) => Some(t.clone()),
        _ => None,
    };
    // A refusal on a live unit's token is recorded while it holds a `Call`, so the unit's end
    // (`HookRegistration::finish`) waits for the record and sees it if it failed; a token revoked
    // first names no unit (logged only).
    let refuse = |conn, why: &str, tool: &str| {
        let live = token
            .as_deref()
            .and_then(unit)
            .and_then(|reg| reg.life.enter().map(|call| (reg, call)));
        let reply = match &live {
            Some((reg, _)) => match reg.life.turn(Instant::now() + HOOK_TIMEOUT) {
                Some(_turn) => refusal(Some(reg), tool, why),
                None => {
                    reg.life.mark_unrecorded();
                    refusal(None, tool, why)
                }
            },
            None => refusal(None, tool, why),
        };
        drop(live);
        respond(conn, "200 OK", &reply);
    };
    let bad_headers = |conn, why: &str| match &route {
        Some(Route::Hook(_)) => refuse(conn, why, UNKNOWN_TOOL),
        _ => respond(conn, "431 Request Header Fields Too Large", "{}"),
    };
    let mut length: Option<usize> = None;
    let mut chunked = false;
    let mut headers = 0;
    loop {
        let Some(h) = bounded_line(&mut reader, &mut budget) else {
            return bad_headers(
                conn,
                "the hook request's headers are oversized or unreadable",
            );
        };
        if h == "\r\n" || h.is_empty() {
            break;
        }
        headers += 1;
        if headers > MAX_HEADERS {
            return bad_headers(conn, "the hook request has too many headers");
        }
        if let Some((k, v)) = h.split_once(':') {
            let k = k.trim();
            if k.eq_ignore_ascii_case("content-length") {
                length = v.trim().parse().ok();
            } else if k.eq_ignore_ascii_case("transfer-encoding") {
                chunked = v.to_ascii_lowercase().contains("chunked");
            }
        }
    }
    match route {
        Some(Route::Ping) => return respond(conn, "200 OK", "{\"pong\":true}"),
        Some(Route::Hook(_)) => {}
        None => return respond(conn, "404 Not Found", "{}"),
    }
    let body = if chunked {
        match read_chunked(&mut reader) {
            Some(b) => b,
            None => {
                return refuse(
                    conn,
                    "unreadable or oversized chunked hook payload",
                    UNKNOWN_TOOL,
                )
            }
        }
    } else {
        match length {
            Some(n) if n > MAX_BODY => return refuse(conn, "hook payload too large", UNKNOWN_TOOL),
            Some(n) => {
                let mut body = vec![0u8; n];
                if reader.read_exact(&mut body).is_err() {
                    return refuse(conn, "truncated hook payload", UNKNOWN_TOOL);
                }
                body
            }
            None => return refuse(conn, "hook payload without a length", UNKNOWN_TOOL),
        }
    };
    let Some(tool) = tool_call(&body) else {
        return refuse(conn, "the hook payload is not a tool call", UNKNOWN_TOOL);
    };
    // The call holds off its unit's revocation until it is answered (`Call`).
    let live = token.as_deref().and_then(unit);
    let Some((reg, call)) = live.and_then(|reg| reg.life.enter().map(|call| (reg, call))) else {
        return refuse(conn, "unknown or revoked hook token", &tool);
    };
    respond(
        conn,
        "200 OK",
        &judge(&reg, &call, &tool, &body, HOOK_TIMEOUT),
    );
    drop(call);
}

/// The live unit `token` names, if any.
fn unit(token: &str) -> Option<Registered> {
    relay().and_then(|r| r.units.lock().ok().and_then(|m| m.get(token).cloned()))
}

/// Judge one `tool` call of `ctx`'s unit with the host gate hook, within ONE `budget` that covers
/// every run's payload write, wait and output collection: the answer for claude.
///
/// The gate hook's own allow (0) and deny (2) are answered as they are. A gate hook ended by a
/// signal before it answered (core#997's run) is run once more within what is left of the budget,
/// but only when it demonstrably left no verdict: the unit's decisions log is exactly as long as
/// before it ran. A run that wrote anything there may have recorded a decision for the call (a
/// deny it recorded before it was killed must not become an allow), and a unit whose log the relay
/// cannot read cannot show that; both are refused. Everything else is refused ([`refusal`]).
/// A unit whose token was revoked while its call was judged gets neither a second run nor an
/// allow, and an allow is committed only before the deadline, checked after every step that can
/// block (the run's spawn and the commit both under the unit's lifecycle lock).
fn judge(reg: &Registered, call: &Call, tool: &str, body: &[u8], budget: Duration) -> String {
    let ctx = &reg.ctx;
    let deadline = Instant::now() + budget;
    // One judgement of the unit at a time, so what its decisions log gains while this one runs is
    // this call's alone (the checks below read it); waiting for the turn counts against the call.
    let Some(_turn) = reg.life.turn(deadline) else {
        reg.life.mark_unrecorded();
        return refusal(
            None,
            tool,
            "the unit's earlier calls were still being judged at the call's deadline",
        );
    };
    // Recorded under the unit (this call still holds off its end): a call refused because the
    // unit ended while it was judged is an unjudged call, and the unit's fold must deny it.
    let ended = |tool: &str| {
        refusal(
            Some(reg),
            tool,
            "the unit ended while its call was being judged (its hook token was revoked)",
        )
    };
    let log = env_of(ctx, crate::gate_hook::DECISIONS_PATH_ENV).map(std::path::PathBuf::from);
    let log_len = |p: &std::path::Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    let before = log.as_deref().map(log_len);
    let mut note = "";
    // One id for this call's runs, stamped by the gate hook into every claim it records for it.
    let invocation = uuid::Uuid::new_v4().simple().to_string();
    let answered = match run_gate_hook(ctx, call, &invocation, body, deadline) {
        HookRun::Killed { signal, stderr } => {
            let untouched = before.is_some() && log.as_deref().map(log_len) == before;
            let time_left = Instant::now() < deadline;
            eprintln!(
                "wicked-core: the gate hook was killed by {} before it answered a {tool} call; {}",
                signal_name(signal),
                match (untouched, time_left) {
                    (true, true) => "judging it again if its unit is still live",
                    (true, false) => "no time is left to judge it again",
                    (false, _) => "it may have recorded a decision, so it is not judged again",
                }
            );
            match (untouched, time_left) {
                // The second run starts only while the unit is live and before the deadline,
                // both checked where it is spawned ([`run_gate_hook`]).
                (true, true) => {
                    note = " (a first run was killed before it answered too)";
                    run_gate_hook(ctx, call, &invocation, body, deadline)
                }
                (true, false) => {
                    note = "; no time was left to judge it again";
                    HookRun::Killed { signal, stderr }
                }
                (false, _) => {
                    note = "; it may have recorded a decision for the call, so it is not judged \
                            again";
                    HookRun::Killed { signal, stderr }
                }
            }
        }
        other => other,
    };
    match answered {
        // Allowed (0) by the gate hook: committed only while the unit is live and before the
        // deadline, both checked under its lifecycle lock, after every step that can block.
        HookRun::Exited {
            code: 0,
            stdout,
            stderr,
        } => match call
            .while_live(|| (Instant::now() < deadline).then(|| answer(Some(0), &stdout, &stderr)))
        {
            Some(Some(allow)) => allow,
            Some(None) => refusal(Some(reg), tool, &HookRun::TimedOut.why(budget)),
            None => ended(tool),
        },
        // Denied (2) by the gate hook. Deny stays deny; when the gate hook recorded nothing (it
        // denies when it cannot write its own record), the relay records the refusal itself, or
        // marks the unit when it cannot either.
        HookRun::Exited {
            code: 2,
            stdout,
            stderr,
        } => {
            // A deny claim among what the log gained during this run (this call's alone: the unit's
            // turn is held), not just any growth: the gate hook writes its liveness sentinel before
            // it judges, and can then fail to write its decision.
            let recorded = match (log.as_deref(), before) {
                (Some(log), Some(from)) => records_a_deny_since(log, from, &invocation),
                _ => false,
            };
            if recorded {
                answer(Some(2), &stdout, &stderr)
            } else {
                let why = String::from_utf8_lossy(&stderr).trim().to_string();
                refusal(
                    Some(reg),
                    tool,
                    &format!("the gate hook denied the call but recorded nothing: {why}"),
                )
            }
        }
        HookRun::Ended => ended(tool),
        failed => refusal(Some(reg), tool, &format!("{}{note}", failed.why(budget))),
    }
}

/// Whether `log` gained, past byte `from`, a deny claim the gate hook recorded for THIS call: one
/// whose tool-call annotation carries `invocation` ([`crate::gate_hook::HOOK_INVOCATION_KEY`]). A
/// claim another recorder wrote meanwhile (the MCP broker's, say) is not this call's record.
/// Unreadable = no.
fn records_a_deny_since(log: &std::path::Path, from: u64, invocation: &str) -> bool {
    use std::io::{Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(log) else {
        return false;
    };
    let mut tail = String::new();
    if f.seek(SeekFrom::Start(from)).is_err() || f.read_to_string(&mut tail).is_err() {
        return false;
    }
    let mut ours = false;
    for line in tail.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            ours = false;
            continue;
        };
        if v.get("claim_id").is_some_and(serde_json::Value::is_string) {
            if ours && v.get("decision").and_then(serde_json::Value::as_str) == Some("deny") {
                return true;
            }
            ours = false;
        } else if v.get(crate::gate_hook::TOOL_CALL_KEY).is_some() {
            ours = v
                .get(crate::gate_hook::HOOK_INVOCATION_KEY)
                .and_then(|x| x.as_str())
                == Some(invocation);
        }
    }
    false
}

/// The value `ctx` gives `key` (non-empty), as its hook would have inherited it.
fn env_of(ctx: &HookContext, key: &str) -> Option<String> {
    ctx.env
        .iter()
        .find(|(n, _)| n.as_os_str() == key)
        .and_then(|(_, v)| v.as_ref())
        .map(|v| v.to_string_lossy().into_owned())
        .filter(|v| !v.is_empty())
}

/// The tool name a refusal is recorded under when the payload names none.
const UNKNOWN_TOOL: &str = "(unknown)";

/// A call the relay refuses itself (the gate hook gave no verdict, or the request is not one it
/// can judge): the deny answer carries the reason, the daemon's log names it, and the unit's
/// decisions log records it as an infra deny ([`crate::gate_hook::append_infra_deny_checked`],
/// replayed as a `governanceHookFired` deny), so a refusal is never silent. When it cannot be
/// recorded there (the log is unwritable, or the unit names none), the unit is marked so its end
/// fails it ([`HookRegistration::finish`]): an unrecorded refusal never leaves a unit able to pass.
/// A request whose token names no live unit has no decisions log; its refusal is logged only.
fn refusal(reg: Option<&Registered>, tool: &str, why: &str) -> String {
    let reason =
        format!("the gate hook relay refused the call: {why}; the call is denied (fail closed)");
    eprintln!("wicked-core: {reason} (tool: {tool})");
    if let Some(reg) = reg {
        let var = |k: &str| env_of(&reg.ctx, k);
        let recorded = match (
            var(crate::gate_hook::DECISIONS_PATH_ENV),
            var(crate::gate_hook::GATE_SCOPE_ENV),
            var(crate::gate_hook::GATE_PHASE_ENV),
        ) {
            (Some(log), Some(scope), Some(phase)) => {
                crate::gate_hook::append_infra_deny_checked(&log, &scope, &phase, tool, &reason)
                    .map_err(|e| e.to_string())
            }
            _ => Err("the unit names no decisions log".to_string()),
        };
        if let Err(e) = recorded {
            eprintln!(
                "wicked-core: the refusal could not be recorded in the unit's decisions log ({e}); \
                 the unit will fail closed"
            );
            reg.life.mark_unrecorded();
        }
    }
    deny_answer(&reason)
}

/// How one run of the host gate hook ended.
#[derive(Debug)]
enum HookRun {
    /// It exited with `code`: 0 = allow, 2 = deny, anything else = it failed.
    Exited {
        code: i32,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    /// It was ended by `signal` before it exited (no verdict). Unix only: elsewhere an exit
    /// status always carries a code.
    #[cfg_attr(not(unix), allow(dead_code))]
    Killed { signal: i32, stderr: Vec<u8> },
    /// It could not be started, or its status could not be read (what failed).
    Lost(String),
    /// It was not started: the unit ended (its token was revoked) first.
    Ended,
    /// It did not answer (exit and hand over its output) by the call's deadline; it was stopped.
    TimedOut,
}

impl HookRun {
    /// Why this run is no verdict (a refusal's reason).
    fn why(&self, budget: Duration) -> String {
        let tail = |b: &[u8]| {
            let t = String::from_utf8_lossy(b).trim().to_string();
            if t.is_empty() {
                "no output".to_string()
            } else {
                t
            }
        };
        match self {
            HookRun::Exited { code, stderr, .. } => {
                format!("the gate hook failed (exit {code}): {}", tail(stderr))
            }
            HookRun::Killed { signal, stderr } => format!(
                "the gate hook was killed by {} before it answered: {}",
                signal_name(*signal),
                tail(stderr)
            ),
            HookRun::Lost(e) => format!("the gate hook {e}"),
            HookRun::Ended => "the gate hook was not run: the unit ended".to_string(),
            HookRun::TimedOut => format!(
                "the gate hook did not answer within the call's {}s",
                budget.as_secs_f32()
            ),
        }
    }
}

/// A signal's name for the log and the reason (`signal 9 (SIGKILL)`).
fn signal_name(signal: i32) -> String {
    #[cfg(unix)]
    let name = match signal {
        libc::SIGKILL => Some("SIGKILL"),
        libc::SIGTERM => Some("SIGTERM"),
        libc::SIGINT => Some("SIGINT"),
        libc::SIGHUP => Some("SIGHUP"),
        libc::SIGSEGV => Some("SIGSEGV"),
        libc::SIGBUS => Some("SIGBUS"),
        libc::SIGABRT => Some("SIGABRT"),
        libc::SIGPIPE => Some("SIGPIPE"),
        _ => None,
    };
    #[cfg(not(unix))]
    let name: Option<&str> = None;
    match name {
        Some(n) => format!("signal {signal} ({n})"),
        None => format!("signal {signal}"),
    }
}

/// How an exit status ended: its code, or the signal that ended it.
fn status_run(status: std::process::ExitStatus, stdout: Vec<u8>, stderr: Vec<u8>) -> HookRun {
    if let Some(code) = status.code() {
        return HookRun::Exited {
            code,
            stdout,
            stderr,
        };
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return HookRun::Killed { signal, stderr };
        }
    }
    HookRun::Exited {
        code: -1,
        stdout,
        stderr,
    }
}

/// Run the host gate hook with the unit's env and `payload` on stdin, ending it at `deadline`:
/// the payload is written from its own thread (a hook that never reads a payload larger than the
/// pipe cannot block the relay), and its output is collected only until the deadline (a
/// descendant holding the pipes cannot either). Whatever it did, a run that has not answered
/// (exited and handed over its output) by the deadline is [`HookRun::TimedOut`]; its process group
/// is then killed, which closes the pipes its descendants held. The I/O threads hold a
/// connection slot ([`InFlight`]) for as long as they live, so a hook whose descendants escape
/// the group and keep the pipes open uses up the relay's slots (new calls are refused, fail
/// closed) instead of growing the daemon's threads without bound.
fn run_gate_hook(
    ctx: &HookContext,
    call: &Call,
    invocation: &str,
    payload: &[u8],
    deadline: Instant,
) -> HookRun {
    let exe = crate::execute_wrapped::resolve_wicked_core_exe();
    let mut cmd = Command::new(&exe);
    cmd.hardened()
        .arg("gate-hook")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Its own group, so the deadline ends its descendants too.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    for (k, v) in &ctx.env {
        match v {
            Some(v) => cmd.env(k, v),
            None => cmd.env_remove(k),
        };
    }
    cmd.env(crate::gate_hook::HOOK_INVOCATION_ENV, invocation);
    if let Some(cwd) = &ctx.cwd {
        cmd.current_dir(cwd);
    }
    if IO_ACTIVE.load(Ordering::SeqCst) + 3 > MAX_IO {
        return HookRun::Lost(
            "could not be started: earlier gate-hook runs still hold the relay's I/O threads"
                .into(),
        );
    }
    // Started only while the unit is live and before the deadline, decided under its lifecycle
    // lock: no run of a revoked unit ever starts.
    let mut child = match call.while_live(|| (Instant::now() < deadline).then(|| cmd.spawn())) {
        Some(Some(Ok(c))) => c,
        Some(Some(Err(e))) => return HookRun::Lost(format!("could not be started: {e}")),
        Some(None) => return HookRun::TimedOut,
        None => return HookRun::Ended,
    };
    #[cfg(unix)]
    let group = child.id();
    if let Some(mut stdin) = child.stdin.take() {
        let payload = payload.to_vec();
        let held = InFlight::new();
        // Ends when the hook reads it all, exits, or is killed (the write then fails).
        std::thread::spawn(move || {
            let _held = held;
            let _ = stdin.write_all(&payload);
        });
    }
    let drain = |p: Option<Box<dyn Read + Send>>| {
        let (tx, rx) = std::sync::mpsc::channel();
        let held = InFlight::new();
        std::thread::spawn(move || {
            let _held = held;
            let mut b = Vec::new();
            if let Some(mut p) = p {
                let _ = p.read_to_end(&mut b);
            }
            let _ = tx.send(b);
        });
        rx
    };
    let out = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let err = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let end_group = || {
        #[cfg(unix)]
        // SAFETY: plain integers. The group is the one this child leads (`process_group(0)`), and
        // the child is never reaped before this runs (its pid, so its group id, stays reserved).
        unsafe {
            libc::killpg(group as libc::pid_t, libc::SIGKILL);
        }
    };
    // Exit is observed WITHOUT reaping (`has_exited_unreaped`): the leader stays a zombie until
    // the `wait` below, so ending its group at any point before that can never reach a stranger
    // that reused the pid.
    let exited = loop {
        match crate::validator::has_exited_unreaped(&mut child) {
            Ok(true) => break Ok(()),
            Ok(false) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            other => {
                end_group();
                let _ = child.kill();
                let _ = child.wait();
                break Err(other.err());
            }
        }
    };
    match exited {
        Ok(()) => {}
        Err(None) => return HookRun::TimedOut,
        Err(Some(e)) => return HookRun::Lost(format!("could not be waited for: {e}")),
    }
    let collect = |rx: std::sync::mpsc::Receiver<Vec<u8>>| {
        rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .ok()
    };
    let (out, err) = (collect(out), collect(err));
    // An answer is one given by the deadline: a run that exited across it, or whose output came
    // in only after it (a descendant held the pipes), is a timeout like any other.
    let in_time = out.is_some() && err.is_some() && Instant::now() < deadline;
    if !in_time {
        end_group();
    }
    let status = match child.wait() {
        Ok(s) => s,
        Err(e) => return HookRun::Lost(format!("could not be waited for: {e}")),
    };
    match (out, err) {
        (Some(out), Some(err)) if in_time => status_run(status, out, err),
        _ => HookRun::TimedOut,
    }
}

/// One relay I/O thread still running (a gate-hook run's stdin writer or output reader),
/// counted apart from connections: a run is not started while [`MAX_IO`] are, so descendants that
/// keep pipes open bound the threads instead of growing them, and never take connection slots.
struct InFlight;

/// The most relay I/O threads alive at once (three per gate-hook run).
const MAX_IO: usize = 96;

static IO_ACTIVE: AtomicUsize = AtomicUsize::new(0);

impl InFlight {
    fn new() -> Self {
        IO_ACTIVE.fetch_add(1, Ordering::SeqCst);
        InFlight
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        IO_ACTIVE.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_answer_maps_the_gate_hooks_exit_to_claudes_hook_json() {
        // core#1027: an allowed call answers claude's documented allow decision, never `{}`
        // (which hands the call to the permission mode, and acceptEdits refuses it in `-p`).
        let allow = serde_json::json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse", "permissionDecision": "allow"}});
        let json = |s: String| serde_json::from_str::<serde_json::Value>(&s).unwrap();
        assert_eq!(json(answer(Some(0), b"", b"")), allow);
        assert_eq!(json(answer(Some(0), b"note text", b"")), allow);
        assert_eq!(json(answer(Some(0), b"[1]", b"")), allow);
        // The gate hook's own object rides along with the decision added.
        let mut with_extra = allow.clone();
        with_extra["a"] = 1.into();
        assert_eq!(json(answer(Some(0), b"{\"a\":1}", b"")), with_extra);
        let mut ctx = allow.clone();
        ctx["hookSpecificOutput"]["additionalContext"] = "note".into();
        assert_eq!(
            json(answer(
                Some(0),
                br#"{"hookSpecificOutput":{"additionalContext":"note"}}"#,
                b""
            )),
            ctx
        );
        // A decision the gate hook states itself is kept, never upgraded to allow.
        let ask =
            br#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"ask"}}"#;
        assert_eq!(
            json(answer(Some(0), ask, b""))["hookSpecificOutput"]["permissionDecision"],
            "ask"
        );
        for (code, err) in [
            (Some(2), "policy X denies Bash"),
            (Some(7), "boom"),
            (None, ""),
        ] {
            let v: serde_json::Value =
                serde_json::from_str(&answer(code, b"", err.as_bytes())).unwrap();
            assert_eq!(
                v["hookSpecificOutput"]["permissionDecision"], "deny",
                "{code:?}"
            );
            assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
            assert!(v["hookSpecificOutput"]["permissionDecisionReason"]
                .as_str()
                .unwrap()
                .contains(err));
        }
    }

    #[test]
    fn only_a_hex_token_path_or_the_ping_is_routed() {
        assert_eq!(route("GET /ping HTTP/1.1\r\n"), Some(Route::Ping));
        assert_eq!(
            route("POST /hook/ab12 HTTP/1.1\r\n"),
            Some(Route::Hook("ab12".into()))
        );
        assert_eq!(route("POST /hook/../etc HTTP/1.1"), None);
        assert_eq!(route("POST /hook/ HTTP/1.1"), None);
        assert_eq!(route("GET /hook/ab12 HTTP/1.1"), None);
    }

    /// agy gw-r1 F-06: a header line past the head budget is refused, never buffered whole.
    #[test]
    fn a_header_past_the_budget_is_refused() {
        let mut budget = 16;
        let mut r = std::io::Cursor::new(b"short\r\nthis line is far too long\r\n".to_vec());
        assert_eq!(
            bounded_line(&mut r, &mut budget).as_deref(),
            Some("short\r\n")
        );
        assert_eq!(budget, 9);
        assert_eq!(bounded_line(&mut r, &mut budget), None);
        let mut r = std::io::Cursor::new(b"4\r\nabcd\r\n0\r\n\r\n".to_vec());
        assert_eq!(read_chunked(&mut r).as_deref(), Some(&b"abcd"[..]));
        let mut r = std::io::Cursor::new(format!("{:x}\r\n", MAX_BODY + 1).into_bytes());
        assert_eq!(read_chunked(&mut r), None);
    }

    /// The relay end to end on loopback with a fake `gate-hook` (a script that denies when the
    /// unit's env says so): an unknown token is denied, a registered one is judged with ITS env,
    /// a revoked one is denied again.
    #[cfg(unix)]
    #[test]
    fn a_registered_token_is_judged_with_its_units_env_and_a_revoked_one_is_denied() {
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!("wicked-relay-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("fake-wicked-core");
        std::fs::write(
            &fake,
            "#!/bin/sh\ncat > /dev/null\n[ -n \"$WICKED_TEST_GONE\" ] && { echo \"removed var leaked\" >&2; exit 2; }\n[ \"$WICKED_TEST_VERDICT\" = deny ] && { echo \"denied by test policy\" >&2; exit 2; }\n[ \"$(pwd -P)\" = \"$WICKED_TEST_CWD\" ] || { echo \"wrong cwd $(pwd -P)\" >&2; exit 2; }\nexit 0\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let prev = std::env::var_os("WICKED_CORE_EXE");
        std::env::set_var("WICKED_CORE_EXE", &fake);
        // Inherited by the hook unless its context removes it.
        std::env::set_var("WICKED_TEST_GONE", "leak");
        let port = port().expect("the relay listens");
        let post = |token: &str| -> serde_json::Value {
            let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
            let body = "{\"tool_name\":\"Bash\",\"tool_input\":{\"command\":\"ls\"}}";
            write!(
                s,
                "POST /hook/{token} HTTP/1.1\r\nhost: x\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            let mut resp = String::new();
            s.read_to_string(&mut resp).unwrap();
            serde_json::from_str(resp.split("\r\n\r\n").nth(1).unwrap()).unwrap()
        };
        let allow = mint_token();
        let deny = mint_token();
        assert_eq!(
            post(&allow)["hookSpecificOutput"]["permissionDecision"],
            "deny",
            "unknown token"
        );
        // The hook runs in the UNIT's tree (relative tool paths), not the daemon's cwd.
        let tree = dir.join("tree");
        std::fs::create_dir_all(&tree).unwrap();
        let tree = tree.canonicalize().unwrap();
        let ctx = |verdict: &str| HookContext {
            env: vec![
                ("WICKED_TEST_VERDICT".into(), Some(verdict.into())),
                (
                    "WICKED_TEST_CWD".into(),
                    Some(tree.clone().into_os_string()),
                ),
                // A variable the seat command removed is removed for its hook too.
                ("WICKED_TEST_GONE".into(), None),
            ],
            cwd: Some(tree.clone()),
        };
        let allow_reg = register(&allow, ctx("allow")).expect("registered");
        let deny_reg = register(&deny, ctx("deny")).expect("registered");
        assert_eq!(
            post(&allow)["hookSpecificOutput"]["permissionDecision"],
            "allow"
        );
        let d = post(&deny);
        assert_eq!(d["hookSpecificOutput"]["permissionDecision"], "deny");
        // Its unit names no decisions log, so the relay cannot see the deny recorded and refuses
        // the call itself, with the gate hook's reason.
        assert!(
            d["hookSpecificOutput"]["permissionDecisionReason"]
                .as_str()
                .unwrap()
                .contains("denied by test policy"),
            "{d}"
        );
        // agy gw-r1 F-01: a payload that is not a tool call (empty, no length, not JSON) is
        // denied without running the hook; a chunked one is decoded and judged.
        let raw = |req: String| -> serde_json::Value {
            let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
            s.write_all(req.as_bytes()).unwrap();
            let mut resp = String::new();
            s.read_to_string(&mut resp).unwrap();
            serde_json::from_str(resp.split("\r\n\r\n").nth(1).unwrap()).unwrap()
        };
        for req in [
            format!("POST /hook/{allow} HTTP/1.1\r\nhost: x\r\ncontent-length: 0\r\n\r\n"),
            format!("POST /hook/{allow} HTTP/1.1\r\nhost: x\r\n\r\n"),
            format!("POST /hook/{allow} HTTP/1.1\r\ncontent-length: 2\r\n\r\n{{}}"),
        ] {
            assert_eq!(
                raw(req.clone())["hookSpecificOutput"]["permissionDecision"],
                "deny",
                "{req}"
            );
        }
        let body = "{\"tool_name\":\"Bash\",\"tool_input\":{\"command\":\"ls\"}}";
        let chunked = format!(
            "POST /hook/{allow} HTTP/1.1\r\ntransfer-encoding: chunked\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n",
            body.len()
        );
        assert_eq!(
            raw(chunked)["hookSpecificOutput"]["permissionDecision"],
            "allow"
        );
        // Dropping the registration revokes it (every exit path, a panic included).
        drop(allow_reg);
        assert_eq!(
            post(&allow)["hookSpecificOutput"]["permissionDecision"],
            "deny",
            "revoked token"
        );
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _held = deny_reg;
            panic!("the seat's output sink panicked");
        }));
        assert!(unwound.is_err());
        assert!(
            post(&deny)["hookSpecificOutput"]["permissionDecisionReason"]
                .as_str()
                .unwrap()
                .contains("unknown or revoked")
        );
        std::env::remove_var("WICKED_TEST_GONE");
        match prev {
            Some(v) => std::env::set_var("WICKED_CORE_EXE", v),
            None => std::env::remove_var("WICKED_CORE_EXE"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// core#997's run: a claude seat's `cargo test` loop was refused with "the gate hook failed
    /// (exit 1)" and nothing else: no reason, no line in the daemon's log, no deny in the unit's
    /// decisions log. The gate hook had not judged the call (its log has no hook-fired line for
    /// it); the relay read a signal death as `exit 1`. Against a host gate hook that is killed by
    /// SIGKILL on its first run (the way it died there), the same payload is judged again and
    /// allowed when the policy allows it. A gate hook that keeps dying, exits 1, or a payload
    /// that is not a tool call is refused fail closed, with the reason in the answer AND as an
    /// infra deny in the unit's decisions log (replayed as a `governanceHookFired` deny). A
    /// policy deny (exit 2) is the gate hook's own record, never a second one from the relay.
    #[cfg(unix)]
    #[test]
    fn a_killed_gate_hook_is_judged_again_and_every_relay_refusal_is_recorded() {
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!("wicked-relay-997-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let fake = dir.join("fake-wicked-core");
        // MODE: `kill-once` dies by SIGKILL on its first run (a marker), `kill` always, `exit1`
        // exits 1 with no output, `deny` is a policy deny, `deny-then-kill` appends a deny claim
        // to the unit's decisions log and dies on its first run (allows after), `stall` never
        // reads its payload, `linger` exits 0 while a child holds its output; every run that
        // reads its payload is counted.
        std::fs::write(
            &fake,
            "#!/bin/sh\n[ \"$WICKED_TEST_MODE\" = stall ] && exec sleep 8\n\
             [ \"$WICKED_TEST_MODE\" = linger ] && { sleep 8 & echo $! > \"$WICKED_TEST_DIR/linger.pid\"; \
             cat > /dev/null; exit 0; }\n\
             cat > /dev/null\necho run >> \"$WICKED_TEST_DIR/runs-$WICKED_TEST_MODE\"\n\
             case \"$WICKED_TEST_MODE\" in\n\
             revoke-during) [ -e \"$WICKED_TEST_DIR/started\" ] && exit 0; : > \"$WICKED_TEST_DIR/started\"; \
             while [ ! -e \"$WICKED_TEST_DIR/revoked\" ]; do sleep 0.1; done; kill -9 $$ ;;\n\
             deny-then-kill) [ -e \"$WICKED_TEST_DIR/recorded\" ] || { : > \"$WICKED_TEST_DIR/recorded\"; \
             printf '%s\\n' '{\"claim_id\":\"infra-deny:unit-5\",\"scope\":\"s\",\"phase\":\"unit-5\",\"policy_ids\":[\"engine:governance-infra\"],\"decision\":\"deny\",\"obligations\":[\"store\"],\"evaluated_context_ref\":\"sha256:infra\",\"criteria\":\"c\",\"evaluator_identity\":\"wicked-governance-infra\",\"evaluated_at\":1792454400}' \
             >> \"$WICKED_DECISIONS_PATH\"; kill -9 $$; } ;;\n\
             kill-once) [ -e \"$WICKED_TEST_DIR/killed\" ] || { : > \"$WICKED_TEST_DIR/killed\"; kill -9 $$; } ;;\n\
             kill) kill -9 $$ ;;\n\
             exit1|unwritable) exit 1 ;;\n\
             deny) printf '%s\\n' \"{\\\"_wicked_tool_call\\\":\\\"Bash\\\",\\\"_wicked_tool_phase\\\":\\\"unit-5\\\",\\\"_wicked_hook_invocation\\\":\\\"$WICKED_HOOK_INVOCATION\\\"}\" >> \"$WICKED_DECISIONS_PATH\"; \
             printf '%s\\n' '{\"claim_id\":\"policy-deny:unit-5\",\"scope\":\"s\",\"phase\":\"unit-5\",\"policy_ids\":[\"test-policy\"],\"decision\":\"deny\",\"obligations\":[\"denied by test policy\"],\"evaluated_context_ref\":\"sha256:p\",\"criteria\":\"c\",\"evaluator_identity\":\"wicked-governance\",\"evaluated_at\":1792454400}' >> \"$WICKED_DECISIONS_PATH\"; \
             echo 'denied by test policy' >&2; exit 2 ;;\n\
             deny-unrecorded) echo 'DENY (could not write hook-fired sentinel)' >&2; exit 2 ;;\n\
             foreign-deny) printf '%s\\n' '{\"_wicked_tool_call\":\"mcp__wicked__write\",\"_wicked_tool_phase\":\"unit-5\"}' \
             '{\"claim_id\":\"mcp-deny:unit-5\",\"scope\":\"s\",\"phase\":\"unit-5\",\"policy_ids\":[\"engine:mcp\"],\"decision\":\"deny\",\"obligations\":[\"mcp\"],\"evaluated_context_ref\":\"sha256:m\",\"criteria\":\"c\",\"evaluator_identity\":\"wicked-mcp\",\"evaluated_at\":1792454400}' \
             >> \"$WICKED_DECISIONS_PATH\"; echo 'DENY (could not record the decision)' >&2; exit 2 ;;\n\
             sentinel-only) printf '%s\\n' '{\"_wicked_hook_fired\":\"unit-5\"}' >> \"$WICKED_DECISIONS_PATH\"; \
             echo 'DENY (could not record the decision)' >&2; exit 2 ;;\n\
             esac\nexit 0\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let prev = std::env::var_os("WICKED_CORE_EXE");
        std::env::set_var("WICKED_CORE_EXE", &fake);
        let port = port().expect("the relay listens");
        // The call the run's seat was refused: the module-by-module `cargo test` loop.
        let command = "cd /wt; export CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0; for m in \
                       seat_login::tests seat_gateway::tests hook_relay::tests; do echo \"== $m\"; \
                       timeout 280 cargo test -j2 --lib $m 2>&1 | grep -E \"^test .*FAILED|test \
                       result|panicked\" | head -30; done";
        let payload = serde_json::json!({
            "session_id": "s", "hook_event_name": "PreToolUse", "tool_name": "Bash",
            "tool_input": {"command": command, "timeout": 900000},
        })
        .to_string();
        let post = |token: &str, body: &str| -> serde_json::Value {
            let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
            write!(
                s,
                "POST /hook/{token} HTTP/1.1\r\nhost: x\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            let mut resp = String::new();
            s.read_to_string(&mut resp).unwrap();
            serde_json::from_str(resp.split("\r\n\r\n").nth(1).unwrap()).unwrap()
        };
        let decision = |v: &serde_json::Value| {
            v["hookSpecificOutput"]["permissionDecision"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        };
        let reason = |v: &serde_json::Value| {
            v["hookSpecificOutput"]["permissionDecisionReason"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        };
        let runs = |mode: &str| {
            std::fs::read_to_string(dir.join(format!("runs-{mode}")))
                .unwrap_or_default()
                .lines()
                .count()
        };
        // One unit per mode, each with its own decisions log, as the launcher arms it.
        let unit = |mode: &str| {
            let log = dir.join(format!("decisions-{mode}.ndjson"));
            let ctx = HookContext {
                env: vec![
                    ("WICKED_TEST_MODE".into(), Some(mode.into())),
                    ("WICKED_TEST_DIR".into(), Some(dir.clone().into_os_string())),
                    (
                        crate::gate_hook::DECISIONS_PATH_ENV.into(),
                        Some(log.clone().into_os_string()),
                    ),
                    (
                        crate::gate_hook::GATE_SCOPE_ENV.into(),
                        Some("wicked-agent/run-997/shared".into()),
                    ),
                    (
                        crate::gate_hook::GATE_PHASE_ENV.into(),
                        Some("unit-5".into()),
                    ),
                ],
                cwd: Some(dir.clone()),
            };
            let token = mint_token();
            let reg = register(&token, ctx).expect("registered");
            (token, reg, log)
        };
        let records =
            |log: &std::path::Path| crate::gate_hook::collect_hook_decisions_in(log, "unit-5");

        // Killed once, judged again: allowed, two runs, nothing refused or recorded by the relay.
        let (t, _r1, log) = unit("kill-once");
        let a = post(&t, &payload);
        assert_eq!(decision(&a), "allow", "{a}");
        assert_eq!(runs("kill-once"), 2);
        assert!(records(&log).is_empty(), "{:?}", records(&log));

        // Killed every time: refused, its reason names the signal, recorded as a deny.
        let (t, _r2, log) = unit("kill");
        let d = post(&t, &payload);
        assert_eq!(decision(&d), "deny", "{d}");
        assert!(reason(&d).contains("SIGKILL"), "{d}");
        assert_eq!(runs("kill"), 2, "judged once more, then refused");
        let rec = records(&log);
        assert_eq!(rec.len(), 1, "{rec:?}");
        assert_eq!(rec[0].decision, "deny");
        assert_eq!(rec[0].tool_name, "Bash");
        assert_eq!(
            rec[0].denying_policy.as_deref(),
            Some(crate::gate_hook::INFRA_RULE_ID)
        );
        assert_eq!(rec[0].obligations.first(), Some(&reason(&d)));

        // A gate hook that fails (exit 1, no output) is refused at once, and recorded.
        let (t, _r3, log) = unit("exit1");
        let d = post(&t, &payload);
        assert_eq!(decision(&d), "deny", "{d}");
        assert!(reason(&d).contains("exit 1"), "{d}");
        assert_eq!(runs("exit1"), 1, "a failed gate hook is not run again");
        let rec = records(&log);
        assert_eq!(rec.len(), 1, "{rec:?}");
        assert_eq!(rec[0].obligations.first(), Some(&reason(&d)));

        // A payload the relay cannot judge, on a live unit's token: refused and recorded.
        let bad = post(&t, "{\"not\":\"a tool call\"}");
        assert_eq!(decision(&bad), "deny");
        assert!(reason(&bad).contains("not a tool call"), "{bad}");
        assert_eq!(records(&log).len(), 2);

        // A policy deny is the gate hook's own decision and record: no second one from the relay.
        let (t, _r4, log) = unit("deny");
        let d = post(&t, &payload);
        assert_eq!(decision(&d), "deny");
        assert_eq!(reason(&d), "denied by test policy");
        let rec = records(&log);
        assert_eq!(rec.len(), 1, "{rec:?}");
        assert_eq!(rec[0].claim_id, "policy-deny:unit-5");
        // A deny the gate hook could not record (it denies when its own write fails, exit 2) is
        // recorded by the relay (core#1055 review round 9).
        let (t, _r4b, log) = unit("deny-unrecorded");
        let d = post(&t, &payload);
        assert_eq!(decision(&d), "deny");
        assert!(reason(&d).contains("recorded nothing"), "{d}");
        assert!(reason(&d).contains("could not write"), "{d}");
        let rec = records(&log);
        assert_eq!(rec.len(), 1, "{rec:?}");
        assert_eq!(
            rec[0].denying_policy.as_deref(),
            Some(crate::gate_hook::INFRA_RULE_ID)
        );
        // …and one whose only new deny is another recorder's, for another call (round 11).
        let (t, _r4d, log) = unit("foreign-deny");
        let d = post(&t, &payload);
        assert!(reason(&d).contains("recorded nothing"), "{d}");
        let rec = records(&log);
        assert_eq!(rec.len(), 2, "{rec:?}");
        assert_eq!(
            rec[1].denying_policy.as_deref(),
            Some(crate::gate_hook::INFRA_RULE_ID)
        );
        // Every claim the gate hook records for a relayed call carries the call's id.
        {
            let p = dir.join("annotated.ndjson");
            std::env::set_var(crate::gate_hook::HOOK_INVOCATION_ENV, "inv-1");
            crate::gate_hook::append_infra_deny_checked(
                p.to_str().unwrap(),
                "s",
                "unit-5",
                "Bash",
                "r",
            )
            .unwrap();
            std::env::remove_var(crate::gate_hook::HOOK_INVOCATION_ENV);
            let first = std::fs::read_to_string(&p).unwrap();
            let ann: serde_json::Value =
                serde_json::from_str(first.lines().next().unwrap()).unwrap();
            assert_eq!(ann[crate::gate_hook::HOOK_INVOCATION_KEY], "inv-1");
            assert!(records_a_deny_since(&p, 0, "inv-1"));
            assert!(!records_a_deny_since(&p, 0, "inv-2"));
        }
        // …and so is one that wrote only its liveness sentinel before failing (round 10).
        let (t, _r4c, log) = unit("sentinel-only");
        let d = post(&t, &payload);
        assert!(reason(&d).contains("recorded nothing"), "{d}");
        let rec = records(&log);
        assert_eq!(rec.len(), 1, "{rec:?}");
        assert_eq!(rec[0].decision, "deny");

        // core#1055 security review P1: a gate hook that records a deny and is then killed is
        // never run again (a second run could allow): the call is refused, and its deny stands.
        let (t, _r5, log) = unit("deny-then-kill");
        let d = post(&t, &payload);
        assert_eq!(decision(&d), "deny", "{d}");
        assert_eq!(
            runs("deny-then-kill"),
            1,
            "not judged again once it wrote a decision"
        );
        assert!(reason(&d).contains("may have recorded a decision"), "{d}");
        let rec = records(&log);
        assert!(
            rec.iter().all(|r| r.decision == "deny") && rec.len() == 2,
            "its own deny and the relay's: {rec:?}"
        );

        // P2: one deadline per call, across the payload write, the wait and the output. A hook
        // that never reads a payload larger than the pipe, and one that exits while a child still
        // holds its output, are both refused within the budget.
        let big = serde_json::json!({
            "hook_event_name": "PreToolUse", "tool_name": "Write",
            "tool_input": {"file_path": "/wt/big.txt", "content": "x".repeat(1 << 20)},
        })
        .to_string();
        let baseline = IO_ACTIVE.load(Ordering::SeqCst);
        let settles = |what: &dyn Fn() -> bool| {
            let until = std::time::Instant::now() + Duration::from_secs(5);
            while !what() && std::time::Instant::now() < until {
                std::thread::sleep(Duration::from_millis(50));
            }
            what()
        };
        for mode in ["stall", "linger", "late"] {
            let (t, _r, log) = unit(mode);
            let reg = super::unit(&t).expect("registered");
            let call = reg.life.enter().expect("live");
            let started = std::time::Instant::now();
            // `late` is a hook that allows at once, on a budget already spent: an answer after
            // the deadline is a timeout too, never an allow.
            let budget = if mode == "late" {
                Duration::ZERO
            } else {
                Duration::from_secs(2)
            };
            let a: serde_json::Value =
                serde_json::from_str(&judge(&reg, &call, "Write", big.as_bytes(), budget)).unwrap();
            let took = started.elapsed();
            assert_eq!(decision(&a), "deny", "{mode}: {a}");
            assert!(reason(&a).contains("did not answer within"), "{mode}: {a}");
            // The budget plus a loaded host's scheduling slack, far under the 8 s the hook holds.
            assert!(
                took < budget + Duration::from_secs(4),
                "{mode}: refused in {took:?}"
            );
            assert_eq!(records(&log).len(), 1, "{mode}: {:?}", records(&log));
            drop(call);
            // Nothing of the run outlives its refusal: its group (the lingering child included)
            // is killed, and its I/O threads end and give their slots back.
            assert!(
                settles(&|| IO_ACTIVE.load(Ordering::SeqCst) <= baseline),
                "{mode}: relay I/O threads still running"
            );
        }
        // The unit's lifecycle lock held past the deadline (a contended registry): an allow
        // the gate hook would give is not committed after the deadline.
        {
            let (t, _r, log) = unit("held");
            let reg = super::unit(&t).expect("registered");
            let call = reg.life.enter().expect("live");
            let (locked_tx, locked_rx) = std::sync::mpsc::channel();
            let life = reg.life.clone();
            let holder = std::thread::spawn(move || {
                let _held = life.state.lock().unwrap();
                locked_tx.send(()).unwrap();
                std::thread::sleep(Duration::from_millis(1500));
            });
            locked_rx.recv().unwrap();
            let a: serde_json::Value = serde_json::from_str(&judge(
                &reg,
                &call,
                "Bash",
                payload.as_bytes(),
                Duration::from_secs(1),
            ))
            .unwrap();
            holder.join().unwrap();
            assert_eq!(decision(&a), "deny", "{a}");
            assert!(reason(&a).contains("did not answer within"), "{a}");
            assert_eq!(records(&log).len(), 1, "{:?}", records(&log));
        }
        let lingering = std::fs::read_to_string(dir.join("linger.pid")).unwrap();
        let alive = || {
            // spawn-audit: test-only — probes a pid.
            std::process::Command::new("kill")
                .args(["-0", lingering.trim()])
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        };
        assert!(
            settles(&|| !alive()),
            "the hook's lingering child was killed"
        );

        // P2: a hook request on a live unit's token whose headers fail is a recorded refusal.
        let (t, _r6, log) = unit("headers");
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let mut req = format!("POST /hook/{t} HTTP/1.1\r\n");
        for i in 0..=MAX_HEADERS {
            req.push_str(&format!("x-h{i}: v\r\n"));
        }
        req.push_str("content-length: 2\r\n\r\n{}");
        s.write_all(req.as_bytes()).unwrap();
        let mut resp = String::new();
        let _ = s.read_to_string(&mut resp);
        let h = records(&log);
        assert_eq!(h.len(), 1, "{h:?} / {resp}");
        assert_eq!(h[0].decision, "deny");
        assert!(
            h[0].obligations[0].contains("too many headers"),
            "{:?}",
            h[0].obligations
        );
        assert_eq!(runs("headers"), 0, "the gate hook never ran");

        // A token revoked while its request is still arriving names no unit when the call is
        // judged: refused, its gate hook never runs, and nothing is appended to the ended unit's log.
        let (t, r7, log) = unit("revoked");
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(format!("POST /hook/{t} HTTP/1.1\r\n").as_bytes())
            .unwrap();
        // The relay reads the request line before the unit ends.
        std::thread::sleep(Duration::from_millis(300));
        drop(r7);
        write!(
            s,
            "host: x\r\ncontent-length: {}\r\n\r\n{payload}",
            payload.len()
        )
        .unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(resp.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(decision(&v), "deny", "{v}");
        assert!(reason(&v).contains("unknown or revoked"), "{v}");
        assert_eq!(runs("revoked"), 0, "the gate hook never ran");
        assert!(records(&log).is_empty(), "{:?}", records(&log));

        // A unit that ends while its gate hook is running (here the hook is then killed, leaving
        // no decision, and a second run would allow) gets neither a second run nor an allow, and
        // the refused call is recorded as a deny in its log before its end returns.
        let (t, r8, log) = unit("revoke-during");
        let answered = std::thread::scope(|sc| {
            let h = sc.spawn(|| post(&t, &payload));
            let started = dir.join("started");
            assert!(settles(&|| started.exists()), "the gate hook started");
            // Revocation waits for the call being judged, so it ends from its own thread.
            let revoking = sc.spawn(move || r8.finish());
            assert!(
                settles(&|| super::unit(&t).is_none()),
                "the token is revoked at once"
            );
            std::fs::write(dir.join("revoked"), "").unwrap();
            let answered = h.join().unwrap();
            assert!(!revoking.join().unwrap(), "the refusal was recorded");
            answered
        });
        assert_eq!(decision(&answered), "deny", "{answered}");
        assert!(reason(&answered).contains("unit ended"), "{answered}");
        assert_eq!(runs("revoke-during"), 1, "never judged again");
        // The unjudged call is recorded as a deny before the unit's end returns, so its fold denies.
        let rec = records(&log);
        assert_eq!(rec.len(), 1, "{rec:?}");
        assert_eq!(rec[0].decision, "deny");
        assert_eq!(
            rec[0].denying_policy.as_deref(),
            Some(crate::gate_hook::INFRA_RULE_ID)
        );
        assert!(rec[0].obligations[0].contains("unit ended"), "{rec:?}");

        // A refusal that cannot be recorded (the unit's decisions log is unwritable) marks the
        // unit, and its end reports it, so the unit fails closed (`execute_wrapped`); a unit
        // whose refusals were recorded reports nothing.
        {
            use std::os::unix::fs::PermissionsExt;
            let (t, r9, log) = unit("unwritable");
            std::fs::write(&log, "").unwrap();
            std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o444)).unwrap();
            let d = post(&t, &payload);
            assert_eq!(decision(&d), "deny", "{d}");
            assert!(records(&log).is_empty());
            assert!(
                r9.finish(),
                "the unrecorded refusal is reported at the unit's end"
            );
            std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o644)).unwrap();
            let (t, r10, _log) = unit("exit1");
            let _ = post(&t, &payload);
            assert!(!r10.finish(), "a recorded refusal is not reported");
        }

        // A malformed request's refusal being recorded when the unit ends: the end waits for the
        // record, so a failed append is reported (core#1055 review round 6). Its decisions log is
        // read-only and its append lock held, so the recorder sits in its lock wait.
        {
            use std::os::unix::fs::PermissionsExt;
            let (t, r11, log) = unit("overlap");
            std::fs::write(&log, "").unwrap();
            std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o444)).unwrap();
            let lock = log.with_extension("lock");
            std::fs::write(&lock, "").unwrap();
            let answered = std::thread::scope(|sc| {
                let h = sc.spawn(|| post(&t, "{}"));
                // The relay reads the request and enters the recorder's (~200 ms) lock wait.
                std::thread::sleep(Duration::from_millis(60));
                let finished = r11.finish();
                (h.join().unwrap(), finished)
            });
            let (d, unrecorded) = answered;
            let _ = std::fs::remove_file(&lock);
            std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert_eq!(decision(&d), "deny", "{d}");
            // Either the refusal was in flight (the end waited and saw its failed record), or the
            // unit ended first and the request named no live unit: never a missed failure.
            assert!(
                unrecorded || reason(&d).contains("unknown or revoked"),
                "the unit's end missed an unrecorded refusal: {d}"
            );
        }

        // A revocation that gives up waiting while a call is still active (its refusal may be
        // blocked, e.g. on the daemon's log) fails the unit rather than reporting it clean.
        {
            let life = Arc::new(Life::default());
            let call = life.enter().expect("live");
            assert!(
                life.revoke_and_wait_within(Duration::from_millis(100)),
                "an active call at the wait limit fails the unit"
            );
            drop(call);
            let idle = Arc::new(Life::default());
            assert!(!idle.revoke_and_wait_within(Duration::from_millis(100)));
        }

        // A call past the relay's connection limit is closed unserved; the live unit its request
        // line names is marked, so its end fails it (core#1055 review round 9).
        {
            let (t, r12, _log) = unit("overflow");
            ACTIVE.fetch_add(MAX_CONNS, Ordering::SeqCst);
            let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
            s.write_all(format!("POST /hook/{t} HTTP/1.1\r\n").as_bytes())
                .unwrap();
            let mut rest = Vec::new();
            let _ = s.read_to_end(&mut rest);
            ACTIVE.fetch_sub(MAX_CONNS, Ordering::SeqCst);
            assert!(rest.is_empty(), "closed unserved");
            assert!(r12.finish(), "the overflowed unit fails closed");
            // An overflow client dripping its request line never holds up the accepting thread
            // (core#1055 review round 11): the relay answers a ping while it drips.
            ACTIVE.fetch_add(MAX_CONNS, Ordering::SeqCst);
            let mut drip = TcpStream::connect(("127.0.0.1", port)).unwrap();
            drip.write_all(b"POST /hook/").unwrap();
            std::thread::sleep(Duration::from_millis(50));
            ACTIVE.fetch_sub(MAX_CONNS, Ordering::SeqCst);
            let started = std::time::Instant::now();
            let mut ping = TcpStream::connect(("127.0.0.1", port)).unwrap();
            ping.write_all(b"GET /ping HTTP/1.1\r\nhost: x\r\n\r\n")
                .unwrap();
            let mut pong = String::new();
            ping.read_to_string(&mut pong).unwrap();
            assert!(pong.contains("pong"), "{pong}");
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "the ping waited {:?}",
                started.elapsed()
            );
            drop(drip);
        }

        // An unknown token has no unit to record in: refused with its reason.
        let d = post(&mint_token(), &payload);
        assert_eq!(decision(&d), "deny");
        assert!(reason(&d).contains("unknown or revoked"), "{d}");

        match prev {
            Some(v) => std::env::set_var("WICKED_CORE_EXE", v),
            None => std::env::remove_var("WICKED_CORE_EXE"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// LIVE (`WICKED_CONTAINER_LIVE=1`), core#988: a governed claude unit on the container floor
    /// has every tool call judged by the engine's relay. The real claude seat (its pinned CLI, the
    /// floor's launcher, Landlock, the netns sidecar) runs with the engine's settings: its command
    /// hook posts each call through the real gateway's hook route to this process's relay, which
    /// runs the (fake) host gate hook. The model (a stub behind the Anthropic route) asks for two
    /// Bash calls: the gate allows the first (it runs) and denies the second (it never runs, and
    /// claude reports the gate's reason). The same seat with a revoked token has its call denied.
    /// On main (an HTTP hook to `agw`) claude refused every call: "HTTP hook blocked".
    #[cfg(unix)]
    #[test]
    fn live_a_container_claude_units_calls_are_judged_through_the_relay() {
        use crate::container_floor::{arm, finalize, output_within};
        use crate::seat_gateway::{configure_gateway, ensure_gateway, Gateway, GatewayConfig};
        use crate::worker_sandbox::FloorInputs;
        use std::path::Path;
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        if std::env::var("WICKED_CONTAINER_LIVE").ok().as_deref() != Some("1") {
            eprintln!("skipped: set WICKED_CONTAINER_LIVE=1 with a container runtime to run");
            return;
        }
        struct Restore(Vec<(&'static str, Option<OsString>)>);
        impl Drop for Restore {
            fn drop(&mut self) {
                for (k, v) in self.0.drain(..) {
                    match v {
                        Some(v) => std::env::set_var(k, v),
                        None => std::env::remove_var(k),
                    }
                }
            }
        }
        let _restore = Restore(
            [
                "WICKED_WORKER_HOME",
                "WICKED_CORE_EXE",
                "WICKED_SEAT_IMAGE_CLAUDE",
            ]
            .into_iter()
            .map(|k| (k, std::env::var_os(k)))
            .collect(),
        );
        std::env::remove_var("WICKED_SEAT_IMAGE_CLAUDE");
        let rt = crate::container_floor::runtime().expect("a container runtime answers");
        let image = crate::container_floor::ensure_seat_image(&rt, "claude").unwrap();
        let base = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("wicked-988-live-{}", uuid::Uuid::new_v4().simple()));
        let dir = base.join("state/gateway");
        let worker = base.join("worker");
        let seat_home = worker.join("seat-claude");
        for d in [&dir, &seat_home, &base.join("stub")] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::env::set_var("WICKED_WORKER_HOME", &worker);
        // The host gate hook the relay runs (as `wicked-core gate-hook`): records each payload,
        // denies the call that writes DENIED.txt, allows the rest.
        let calls = base.join("hook-calls.ndjson");
        let fake = base.join("gate-hook.sh");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\n[ \"$1\" = gate-hook ] || exit 3\np=$(cat)\nprintf '%s\\n' \"$p\" >> '{}'\n\
                 case \"$p\" in *DENIED.txt*) echo 'DENIED-BY-THE-RELAYED-GATE' >&2; exit 2;; esac\n\
                 exit 0\n",
                calls.display()
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::env::set_var("WICKED_CORE_EXE", &fake);
        let run = |argv: &[&str]| {
            let mut c = rt.cmd();
            c.args(argv);
            output_within(c, Duration::from_secs(60)).unwrap()
        };
        let gw = Gateway::for_dir(&dir);
        let stub = format!("{}-stub", gw.name);
        // The Anthropic stub: two Bash calls, then text; it logs each request's last tool result.
        std::fs::write(
            base.join("stub/stub.js"),
            "require('http').createServer((q,r)=>{let b='';q.on('data',d=>b+=d);q.on('end',()=>{\
             let j={};try{j=JSON.parse(b)}catch{}\
             const res=(j.messages||[]).flatMap(m=>Array.isArray(m.content)?\
             m.content.filter(c=>c.type==='tool_result'):[]);\
             const main=(j.tools||[]).some(t=>t.name==='Bash');const step=res.length;\
             console.log('UPSTREAM '+JSON.stringify({url:q.url,main,step,last:res[step-1]||null}));\
             const cmds=['node -e \"require(\\'fs\\').writeFileSync(\\'ALLOWED.txt\\',\\'x\\')\"','echo denied > DENIED.txt'];\
             const tool=main&&step<cmds.length;\
             const content=tool?[{type:'tool_use',id:'toolu_'+step,name:'Bash',\
             input:{command:cmds[step],description:'probe'}}]:[{type:'text',text:'STUB-DONE'}];\
             const stop=tool?'tool_use':'end_turn';\
             const msg={id:'msg_'+step,type:'message',role:'assistant',model:j.model||'m',\
             content:[],stop_reason:null,stop_sequence:null,usage:{input_tokens:1,output_tokens:1}};\
             if(!j.stream){r.setHeader('content-type','application/json');\
             return r.end(JSON.stringify({...msg,content,stop_reason:stop}))}\
             r.setHeader('content-type','text/event-stream');\
             const ev=(t,d)=>r.write('event: '+t+'\\ndata: '+JSON.stringify({type:t,...d})+'\\n\\n');\
             ev('message_start',{message:msg});content.forEach((c,i)=>{if(c.type==='text'){\
             ev('content_block_start',{index:i,content_block:{type:'text',text:''}});\
             ev('content_block_delta',{index:i,delta:{type:'text_delta',text:c.text}})}else{\
             ev('content_block_start',{index:i,content_block:{...c,input:{}}});\
             ev('content_block_delta',{index:i,delta:{type:'input_json_delta',\
             partial_json:JSON.stringify(c.input)}})}ev('content_block_stop',{index:i})});\
             ev('message_delta',{delta:{stop_reason:stop,stop_sequence:null},usage:{output_tokens:1}});\
             ev('message_stop',{});r.end()})}).listen(8080)",
        )
        .unwrap();
        let mut config = GatewayConfig::default();
        for route in &mut config.providers {
            route.upstream_host = format!("{stub}:8080");
            route.tls = false;
        }
        configure_gateway(&dir, &config).unwrap();
        let gw = ensure_gateway(&rt, &dir).expect("the gateway starts");
        assert!(
            crate::seat_gateway::hook_route_ready(),
            "the gateway reaches this process's relay"
        );
        crate::seat_gateway::set_broker_credential(
            &rt,
            &gw,
            &dir,
            "claude",
            "anthropic.key",
            b"FAKE-988-broker-key",
        )
        .unwrap();
        let stub_mount = format!(
            "type=bind,src={},dst=/s,readonly",
            base.join("stub").display()
        );
        let o = run(&[
            "run",
            "-d",
            "--name",
            &stub,
            "--network",
            &gw.external_network,
            "--mount",
            &stub_mount,
            &image,
            "node",
            "/s/stub.js",
        ]);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        // A real linked run worktree with the engine's pin, as a governed unit has.
        let repo = base.join("repo");
        let wt = base.join("wt");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |cwd: &Path, args: &[&str]| {
            // spawn-audit: test-only — local git over a temp repo.
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
        };
        git(&repo, &["init", "-q"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "i"]);
        git(&repo, &["worktree", "add", "-q", wt.to_str().unwrap()]);
        let pin = crate::worktree_guard::pinned_git_dir(&wt, &repo).unwrap();
        let inputs = FloorInputs {
            cwd: wt.clone(),
            write_roots: vec![wt.clone()],
            read_only: false,
            seat_dirs: vec![seat_home.clone()],
            unmask: vec![],
            pinned: vec![],
            net: vec![],
            home: None,
            trusted_git_dir: Some(pin),
        };
        // One governed claude unit with the engine's container settings for `token`.
        let unit = |ord: u32, token: &str| {
            let settings = serde_json::json!({
                "disableAllHooks": false,
                "hooks": {"PreToolUse": [{"matcher": "*", "hooks": [
                    crate::execute_wrapped::container_gate_hook(token).unwrap()
                ]}]}
            })
            .to_string();
            let argv: Vec<String> = [
                "claude",
                "-p",
                "run the two commands",
                "--output-format",
                "stream-json",
                "--verbose",
                "--model",
                "claude-sonnet-4-5",
                // The engine's own posture (core#1027): no ambient settings and acceptEdits, NOT
                // `--dangerously-skip-permissions`, which hid that an allow answered `{}` left
                // the call to the mode, and acceptEdits refuses `node` in a `-p` session.
                "--setting-sources",
                "",
                "--permission-mode",
                "acceptEdits",
                "--settings",
                &settings,
            ]
            .map(String::from)
            .to_vec();
            let armed = arm(&inputs, &argv, "run-988", ord, &[], None, None, Some(&dir))
                .expect("a signed-in claude seat arms");
            let full: Vec<String> = armed.wrapper.iter().chain(&armed.argv).cloned().collect();
            let mut cmd = crate::execute_wrapped::build_worker_command(&full, None);
            cmd.env("CLAUDE_CONFIG_DIR", &seat_home);
            for (k, v) in &armed.env {
                cmd.env(k, v);
            }
            crate::container_floor::write_unit_seat_files(&mut cmd, "claude", &armed.files)
                .expect("placeholder files");
            let out = output_within(finalize(cmd), Duration::from_secs(150)).expect("the seat ran");
            if let Some(lease) = armed.lease {
                let _ = lease.release();
            }
            drop(armed.cleanup);
            (
                out.status.code(),
                format!(
                    "{}{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                ),
            )
        };
        let token = mint_token();
        let reg = register(
            &token,
            HookContext {
                env: vec![],
                cwd: Some(wt.clone()),
            },
        )
        .expect("the relay runs");
        let (code, judged) = unit(1, &token);
        let allowed = wt.join("ALLOWED.txt").exists();
        let denied = wt.join("DENIED.txt").exists();
        let judged_calls = std::fs::read_to_string(&calls).unwrap_or_default();
        // The same seat once its token is revoked (the unit ended): its first call is denied.
        drop(reg);
        let _ = std::fs::remove_file(wt.join("ALLOWED.txt"));
        let (_, revoked) = unit(2, &token);
        let revoked_ran = wt.join("ALLOWED.txt").exists();
        let upstream = String::from_utf8_lossy(&run(&["logs", &stub]).stdout).to_string();
        run(&["rm", "-f", &stub, &gw.name]);
        run(&["network", "rm", &gw.external_network]);
        let (cfg, broker) = crate::seat_gateway::volumes(&gw);
        run(&["volume", "rm", &cfg, &broker]);
        let _ = std::fs::remove_dir_all(&base);
        let tail = |s: &str| {
            let c: Vec<char> = s.chars().collect();
            c[c.len().saturating_sub(2000)..].iter().collect::<String>()
        };
        eprintln!(
            "code={code:?}\njudged={}\nrevoked={}\ncalls={judged_calls}\nupstream={upstream}",
            tail(&judged),
            tail(&revoked)
        );
        assert!(!judged.contains("HTTP hook blocked"), "{}", tail(&judged));
        assert!(
            judged_calls.contains("ALLOWED.txt") && judged_calls.contains("DENIED.txt"),
            "both calls reached the host gate hook through the relay: {judged_calls}"
        );
        assert!(allowed, "the allowed call ran: {}", tail(&judged));
        assert!(!denied, "the denied call never ran: {}", tail(&judged));
        assert!(
            judged.contains("DENIED-BY-THE-RELAYED-GATE"),
            "claude reports the gate's reason: {}",
            tail(&judged)
        );
        assert!(judged.contains("STUB-DONE"), "{}", tail(&judged));
        assert_eq!(code, Some(0), "{}", tail(&judged));
        assert!(
            !revoked_ran,
            "a revoked token's call never ran: {}",
            tail(&revoked)
        );
        assert!(
            revoked.contains("unknown or revoked hook token"),
            "{}",
            tail(&revoked)
        );
    }

    /// LIVE (`WICKED_CONTAINER_LIVE=1`), core#997: the shape of the refused call in a real claude
    /// container seat, through the real gateway's hook route and this process's relay. The model
    /// (a stub behind the Anthropic route) asks for three Bash calls: a long `cargo test`-like
    /// loop (child processes, thousands of output lines, ~45 s), then a call whose host gate hook
    /// is SIGKILLed on its first run (how the run's gate hook died), then one whose gate hook dies
    /// every time. The loop is judged and runs; the killed call is judged again and runs; the
    /// third is refused fail closed, claude reports the relay's reason (the signal), and the
    /// unit's decisions log holds the refusal as an infra deny for the fold to replay.
    #[cfg(unix)]
    #[test]
    fn live_a_long_loop_and_a_killed_gate_hook_in_a_container_claude_seat() {
        use crate::container_floor::{arm, finalize, output_within};
        use crate::seat_gateway::{configure_gateway, ensure_gateway, Gateway, GatewayConfig};
        use crate::worker_sandbox::FloorInputs;
        use std::path::Path;
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        if std::env::var("WICKED_CONTAINER_LIVE").ok().as_deref() != Some("1") {
            eprintln!("skipped: set WICKED_CONTAINER_LIVE=1 with a container runtime to run");
            return;
        }
        struct Restore(Vec<(&'static str, Option<OsString>)>);
        impl Drop for Restore {
            fn drop(&mut self) {
                for (k, v) in self.0.drain(..) {
                    match v {
                        Some(v) => std::env::set_var(k, v),
                        None => std::env::remove_var(k),
                    }
                }
            }
        }
        let _restore = Restore(
            [
                "WICKED_WORKER_HOME",
                "WICKED_CORE_EXE",
                "WICKED_SEAT_IMAGE_CLAUDE",
            ]
            .into_iter()
            .map(|k| (k, std::env::var_os(k)))
            .collect(),
        );
        std::env::remove_var("WICKED_SEAT_IMAGE_CLAUDE");
        let rt = crate::container_floor::runtime().expect("a container runtime answers");
        let image = crate::container_floor::ensure_seat_image(&rt, "claude").unwrap();
        let base = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("wicked-997-live-{}", uuid::Uuid::new_v4().simple()));
        let dir = base.join("state/gateway");
        let worker = base.join("worker");
        let seat_home = worker.join("seat-claude");
        for d in [&dir, &seat_home, &base.join("stub")] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::env::set_var("WICKED_WORKER_HOME", &worker);
        // The host gate hook: records each payload; SIGKILLed on the first run of the call that
        // writes AFTER-KILL.txt, and on every run of the one that writes FORCED.txt.
        let calls = base.join("hook-calls.ndjson");
        let fake = base.join("gate-hook.sh");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\n[ \"$1\" = gate-hook ] || exit 3\np=$(cat)\nprintf '%s\\n' \"$p\" >> '{c}'\n\
                 case \"$p\" in\n\
                 *AFTER-KILL.txt*) [ -e '{k}' ] || {{ : > '{k}'; kill -9 $$; }} ;;\n\
                 *FORCED.txt*) kill -9 $$ ;;\n\
                 esac\nexit 0\n",
                c = calls.display(),
                k = base.join("killed").display(),
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::env::set_var("WICKED_CORE_EXE", &fake);
        let run = |argv: &[&str]| {
            let mut c = rt.cmd();
            c.args(argv);
            output_within(c, Duration::from_secs(60)).unwrap()
        };
        let gw = Gateway::for_dir(&dir);
        let stub = format!("{}-stub", gw.name);
        // The `cargo test` loop of the run, with a stand-in for cargo (thousands of result lines
        // from a child shell per module, and a wait), then the two calls whose hook is killed.
        let cmds = [
            "for m in seat_login::tests seat_gateway::tests hook_relay::tests; do echo \"== $m\"; \
             sh -c 'i=0; while [ $i -lt 4000 ]; do echo \"test $0::t$i ... ok\"; i=$((i+1)); done; \
             sleep 15; echo \"test result: ok. 4000 passed\"' \"$m\" 2>&1 | \
             grep -E \"^test .*FAILED|test result|panicked\" | head -30; done; echo done > LONG.txt",
            "for m in a b; do echo \"== $m\"; done; echo judged > AFTER-KILL.txt",
            "echo forced > FORCED.txt",
        ];
        let cmds_json = serde_json::to_string(&cmds).unwrap();
        std::fs::write(
            base.join("stub/stub.js"),
            format!(
                "const cmds={cmds_json};\
                 require('http').createServer((q,r)=>{{let b='';q.on('data',d=>b+=d);q.on('end',()=>{{\
                 let j={{}};try{{j=JSON.parse(b)}}catch{{}}\
                 const res=(j.messages||[]).flatMap(m=>Array.isArray(m.content)?\
                 m.content.filter(c=>c.type==='tool_result'):[]);\
                 const main=(j.tools||[]).some(t=>t.name==='Bash');const step=res.length;\
                 console.log('UPSTREAM '+JSON.stringify({{url:q.url,main,step,last:res[step-1]||null}}));\
                 const tool=main&&step<cmds.length;\
                 const content=tool?[{{type:'tool_use',id:'toolu_'+step,name:'Bash',\
                 input:{{command:cmds[step],description:'probe',timeout:300000}}}}]:\
                 [{{type:'text',text:'STUB-DONE'}}];\
                 const stop=tool?'tool_use':'end_turn';\
                 const msg={{id:'msg_'+step,type:'message',role:'assistant',model:j.model||'m',\
                 content:[],stop_reason:null,stop_sequence:null,usage:{{input_tokens:1,output_tokens:1}}}};\
                 if(!j.stream){{r.setHeader('content-type','application/json');\
                 return r.end(JSON.stringify({{...msg,content,stop_reason:stop}}))}}\
                 r.setHeader('content-type','text/event-stream');\
                 const ev=(t,d)=>r.write('event: '+t+'\\ndata: '+JSON.stringify({{type:t,...d}})+'\\n\\n');\
                 ev('message_start',{{message:msg}});content.forEach((c,i)=>{{if(c.type==='text'){{\
                 ev('content_block_start',{{index:i,content_block:{{type:'text',text:''}}}});\
                 ev('content_block_delta',{{index:i,delta:{{type:'text_delta',text:c.text}}}})}}else{{\
                 ev('content_block_start',{{index:i,content_block:{{...c,input:{{}}}}}});\
                 ev('content_block_delta',{{index:i,delta:{{type:'input_json_delta',\
                 partial_json:JSON.stringify(c.input)}}}})}}ev('content_block_stop',{{index:i}})}});\
                 ev('message_delta',{{delta:{{stop_reason:stop,stop_sequence:null}},usage:{{output_tokens:1}}}});\
                 ev('message_stop',{{}});r.end()}})}}).listen(8080)"
            ),
        )
        .unwrap();
        let mut config = GatewayConfig::default();
        for route in &mut config.providers {
            route.upstream_host = format!("{stub}:8080");
            route.tls = false;
        }
        configure_gateway(&dir, &config).unwrap();
        let gw = ensure_gateway(&rt, &dir).expect("the gateway starts");
        assert!(crate::seat_gateway::hook_route_ready());
        crate::seat_gateway::set_broker_credential(
            &rt,
            &gw,
            &dir,
            "claude",
            "anthropic.key",
            b"FAKE-997-broker-key",
        )
        .unwrap();
        let stub_mount = format!(
            "type=bind,src={},dst=/s,readonly",
            base.join("stub").display()
        );
        let o = run(&[
            "run",
            "-d",
            "--name",
            &stub,
            "--network",
            &gw.external_network,
            "--mount",
            &stub_mount,
            &image,
            "node",
            "/s/stub.js",
        ]);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        let repo = base.join("repo");
        let wt = base.join("wt");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |cwd: &Path, args: &[&str]| {
            // spawn-audit: test-only — local git over a temp repo.
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
        };
        git(&repo, &["init", "-q"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "i"]);
        git(&repo, &["worktree", "add", "-q", wt.to_str().unwrap()]);
        let pin = crate::worktree_guard::pinned_git_dir(&wt, &repo).unwrap();
        let inputs = FloorInputs {
            cwd: wt.clone(),
            write_roots: vec![wt.clone()],
            read_only: false,
            seat_dirs: vec![seat_home.clone()],
            unmask: vec![],
            pinned: vec![],
            net: vec![],
            home: None,
            trusted_git_dir: Some(pin),
        };
        let token = mint_token();
        let settings = serde_json::json!({
            "disableAllHooks": false,
            "hooks": {"PreToolUse": [{"matcher": "*", "hooks": [
                crate::execute_wrapped::container_gate_hook(&token).unwrap()
            ]}]}
        })
        .to_string();
        let argv: Vec<String> = [
            "claude",
            "-p",
            "run the three commands",
            "--output-format",
            "stream-json",
            "--verbose",
            "--model",
            "claude-sonnet-4-5",
            "--setting-sources",
            "",
            "--permission-mode",
            "acceptEdits",
            "--settings",
            &settings,
        ]
        .map(String::from)
        .to_vec();
        // The unit's decisions log, scope and phase, as the launcher hands them to its hook.
        let log = base.join("decisions.ndjson");
        let reg = register(
            &token,
            HookContext {
                env: vec![
                    (
                        crate::gate_hook::DECISIONS_PATH_ENV.into(),
                        Some(log.clone().into_os_string()),
                    ),
                    (
                        crate::gate_hook::GATE_SCOPE_ENV.into(),
                        Some("wicked-agent/run-997/shared".into()),
                    ),
                    (
                        crate::gate_hook::GATE_PHASE_ENV.into(),
                        Some("unit-1".into()),
                    ),
                ],
                cwd: Some(wt.clone()),
            },
        )
        .expect("the relay runs");
        let armed = arm(&inputs, &argv, "run-997", 1, &[], None, None, Some(&dir))
            .expect("a signed-in claude seat arms");
        let full: Vec<String> = armed.wrapper.iter().chain(&armed.argv).cloned().collect();
        let mut cmd = crate::execute_wrapped::build_worker_command(&full, None);
        cmd.env("CLAUDE_CONFIG_DIR", &seat_home);
        for (k, v) in &armed.env {
            cmd.env(k, v);
        }
        crate::container_floor::write_unit_seat_files(&mut cmd, "claude", &armed.files)
            .expect("placeholder files");
        let out = output_within(finalize(cmd), Duration::from_secs(400)).expect("the seat ran");
        if let Some(lease) = armed.lease {
            let _ = lease.release();
        }
        drop(armed.cleanup);
        drop(reg);
        let judged = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let long = wt.join("LONG.txt").exists();
        let after_kill = wt.join("AFTER-KILL.txt").exists();
        let forced = wt.join("FORCED.txt").exists();
        let judged_calls = std::fs::read_to_string(&calls).unwrap_or_default();
        let records = crate::gate_hook::collect_hook_decisions_in(&log, "unit-1");
        let upstream = String::from_utf8_lossy(&run(&["logs", &stub]).stdout).to_string();
        run(&["rm", "-f", &stub, &gw.name]);
        run(&["network", "rm", &gw.external_network]);
        let (cfg, broker) = crate::seat_gateway::volumes(&gw);
        run(&["volume", "rm", &cfg, &broker]);
        let _ = std::fs::remove_dir_all(&base);
        let tail = |s: &str| {
            let c: Vec<char> = s.chars().collect();
            c[c.len().saturating_sub(3000)..].iter().collect::<String>()
        };
        eprintln!(
            "code={:?}\njudged={}\ncalls={}\nrecords={records:?}\nupstream={}",
            out.status.code(),
            tail(&judged),
            judged_calls.lines().count(),
            tail(&upstream)
        );
        assert!(
            long,
            "the long cargo-test loop was allowed and ran: {}",
            tail(&judged)
        );
        assert!(
            after_kill,
            "the call whose gate hook was killed was judged again and ran: {}",
            tail(&judged)
        );
        assert_eq!(
            judged_calls.matches("AFTER-KILL.txt").count(),
            2,
            "killed once, judged twice"
        );
        assert!(!forced, "the call whose gate hook kept dying never ran");
        assert!(
            judged.contains("SIGKILL"),
            "claude reports the relay's reason: {}",
            tail(&judged)
        );
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0].decision, "deny");
        assert_eq!(records[0].tool_name, "Bash");
        assert!(records[0]
            .obligations
            .first()
            .is_some_and(|r| r.contains("SIGKILL")));
        assert!(judged.contains("STUB-DONE"), "{}", tail(&judged));
        assert_eq!(out.status.code(), Some(0), "{}", tail(&judged));
    }
}
