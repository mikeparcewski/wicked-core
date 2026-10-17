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
//! the relay maps its exit code to the hook's JSON answer: 0 → its own stdout (or `{}`),
//! 2 → deny with its stderr as the reason, anything else (a crash, a timeout, an unknown token)
//! → deny. A token is minted per unit, known only to that seat and the relay, and revoked when
//! the unit ends.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
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
    units: Mutex<HashMap<String, HookContext>>,
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
/// panic while the seat runs), so a token never outlives its unit.
#[derive(Debug)]
pub(crate) struct HookRegistration(String);

impl Drop for HookRegistration {
    fn drop(&mut self) {
        revoke(&self.0);
    }
}

/// Bind `token` to the context the unit's gate hook runs with; `None` when no relay runs.
pub(crate) fn register(token: &str, ctx: HookContext) -> Option<HookRegistration> {
    let r = relay()?;
    let mut m = r.units.lock().ok()?;
    m.insert(token.to_string(), ctx);
    Some(HookRegistration(token.to_string()))
}

/// Revoke `token` (the unit ended).
pub(crate) fn revoke(token: &str) {
    if let Some(r) = relay() {
        if let Ok(mut m) = r.units.lock() {
            m.remove(token);
        }
    }
}

/// claude's HTTP-hook answer for one gate-hook outcome.
pub(crate) fn answer(code: Option<i32>, stdout: &[u8], stderr: &[u8]) -> String {
    let deny = |reason: &str| {
        serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
            }
        })
        .to_string()
    };
    match code {
        Some(0) => {
            let out = String::from_utf8_lossy(stdout);
            let out = out.trim();
            if out.is_empty() {
                "{}".into()
            } else if serde_json::from_str::<serde_json::Value>(out).is_ok_and(|v| v.is_object()) {
                out.to_string()
            } else {
                // A command hook's plain stdout is a transcript note, not a decision.
                "{}".into()
            }
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
fn is_tool_call(body: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(body).is_ok_and(|v| {
        v.get("tool_name")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|t| !t.trim().is_empty())
    })
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
    let mut length: Option<usize> = None;
    let mut chunked = false;
    let mut headers = 0;
    loop {
        let Some(h) = bounded_line(&mut reader, &mut budget) else {
            return respond(conn, "431 Request Header Fields Too Large", "{}");
        };
        if h == "\r\n" || h.is_empty() {
            break;
        }
        headers += 1;
        if headers > MAX_HEADERS {
            return respond(conn, "431 Request Header Fields Too Large", "{}");
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
    let token = match route(&line) {
        Some(Route::Ping) => return respond(conn, "200 OK", "{\"pong\":true}"),
        Some(Route::Hook(t)) => t,
        None => return respond(conn, "404 Not Found", "{}"),
    };
    let deny = |conn, why: &[u8]| respond(conn, "200 OK", &answer(Some(1), b"", why));
    let body = if chunked {
        match read_chunked(&mut reader) {
            Some(b) => b,
            None => return deny(conn, b"unreadable or oversized chunked hook payload"),
        }
    } else {
        match length {
            Some(n) if n > MAX_BODY => return deny(conn, b"hook payload too large"),
            Some(n) => {
                let mut body = vec![0u8; n];
                if reader.read_exact(&mut body).is_err() {
                    return deny(conn, b"truncated hook payload");
                }
                body
            }
            None => return deny(conn, b"hook payload without a length"),
        }
    };
    if !is_tool_call(&body) {
        return deny(conn, b"the hook payload is not a tool call");
    }
    let ctx = relay().and_then(|r| r.units.lock().ok().and_then(|m| m.get(&token).cloned()));
    let Some(ctx) = ctx else {
        return deny(conn, b"unknown or revoked hook token");
    };
    let (code, out, err) = run_gate_hook(&ctx, &body);
    respond(conn, "200 OK", &answer(code, &out, &err));
}

/// Run the host gate hook with the unit's env and `payload` on stdin, bounded.
fn run_gate_hook(ctx: &HookContext, payload: &[u8]) -> (Option<i32>, Vec<u8>, Vec<u8>) {
    let exe = crate::execute_wrapped::resolve_wicked_core_exe();
    let mut cmd = Command::new(&exe);
    cmd.hardened()
        .arg("gate-hook")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in &ctx.env {
        match v {
            Some(v) => cmd.env(k, v),
            None => cmd.env_remove(k),
        };
    }
    if let Some(cwd) = &ctx.cwd {
        cmd.current_dir(cwd);
    }
    let Ok(mut child) = cmd.spawn() else {
        return (
            Some(1),
            Vec::new(),
            b"could not start the gate hook".to_vec(),
        );
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(payload);
    }
    let drain = |p: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut b = Vec::new();
            if let Some(mut p) = p {
                let _ = p.read_to_end(&mut b);
            }
            b
        })
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
    let start = Instant::now();
    let code = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s.code().or(Some(1)),
            Ok(None) if start.elapsed() < HOOK_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(20))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    (
        code,
        out.join().unwrap_or_default(),
        err.join().unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_answer_maps_the_gate_hooks_exit_to_claudes_hook_json() {
        assert_eq!(answer(Some(0), b"", b""), "{}");
        assert_eq!(answer(Some(0), b"{\"a\":1}", b""), "{\"a\":1}");
        assert_eq!(answer(Some(0), b"note text", b""), "{}");
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
        assert_eq!(post(&allow), serde_json::json!({}));
        let d = post(&deny);
        assert_eq!(d["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(
            d["hookSpecificOutput"]["permissionDecisionReason"],
            "denied by test policy"
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
        assert_eq!(raw(chunked), serde_json::json!({}));
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
             const cmds=['echo allowed > ALLOWED.txt','echo denied > DENIED.txt'];\
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
                "--dangerously-skip-permissions",
                "--settings",
                &settings,
            ]
            .map(String::from)
            .to_vec();
            let armed = arm(&inputs, &argv, "run-988", ord, &[], None, Some(&dir))
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
}
