//! The gate hook for a claude seat in the container floor (slice C3).
//!
//! A governed claude unit is judged before every tool call by `wicked-core gate-hook`, a HOST
//! binary claude runs as a command hook. Inside a Linux seat container that command does not
//! exist, so the seat's settings name an HTTP hook instead: claude POSTs the same PreToolUse
//! payload to `http://agw:<HOOK_PORT>/hook/<token>`, the daemon's gateway routes it to this
//! relay on the host's loopback, and the relay runs the unchanged host `gate-hook` with exactly
//! the environment the seat's command would have carried, then answers in claude's hook JSON.
//!
//! **Glue only.** The judgement, the decisions log and the store are the existing gate hook's;
//! the relay maps its exit code to the HTTP hook's JSON answer: 0 → its own stdout (or `{}`),
//! 2 → deny with its stderr as the reason, anything else (a crash, a timeout, an unknown token)
//! → deny. A token is minted per unit, known only to that seat and the relay, and revoked when
//! the unit ends.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use wicked_apps_core::HardenedCommand;

/// The gateway port the seat's HTTP hook calls (not a browser-blocked port).
pub(crate) const HOOK_PORT: u16 = 8711;
/// How long one tool call's judgement may take before it is denied.
const HOOK_TIMEOUT: Duration = Duration::from_secs(60);
/// The largest hook payload the relay reads (a tool call's input).
const MAX_BODY: usize = 8 * 1024 * 1024;

struct Relay {
    port: u16,
    units: Mutex<HashMap<String, Vec<(OsString, OsString)>>>,
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
                        std::thread::spawn(move || serve(conn));
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

/// The HTTP hook URL a seat holding `token` calls.
pub(crate) fn hook_url(token: &str) -> String {
    format!("http://agw:{HOOK_PORT}/hook/{token}")
}

/// Bind `token` to the environment the unit's gate hook runs with (the seat command's explicit
/// variables, as a command hook would have inherited them).
pub(crate) fn register(token: &str, env: Vec<(OsString, OsString)>) -> bool {
    match relay() {
        Some(r) => {
            if let Ok(mut m) = r.units.lock() {
                m.insert(token.to_string(), env);
                return true;
            }
            false
        }
        None => false,
    }
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

fn serve(conn: TcpStream) {
    let _ = conn.set_read_timeout(Some(Duration::from_secs(30)));
    let Ok(read_half) = conn.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let mut length = 0usize;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).is_err() || h == "\r\n" || h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                length = v.trim().parse().unwrap_or(0);
            }
        }
    }
    let token = match route(&line) {
        Some(Route::Ping) => return respond(conn, "200 OK", "{\"pong\":true}"),
        Some(Route::Hook(t)) => t,
        None => return respond(conn, "404 Not Found", "{}"),
    };
    if length > MAX_BODY {
        return respond(
            conn,
            "200 OK",
            &answer(Some(1), b"", b"hook payload too large"),
        );
    }
    let mut body = vec![0u8; length];
    if reader.read_exact(&mut body).is_err() {
        return respond(
            conn,
            "200 OK",
            &answer(Some(1), b"", b"truncated hook payload"),
        );
    }
    let env = relay().and_then(|r| r.units.lock().ok().and_then(|m| m.get(&token).cloned()));
    let Some(env) = env else {
        return respond(
            conn,
            "200 OK",
            &answer(Some(1), b"", b"unknown or revoked hook token"),
        );
    };
    let (code, out, err) = run_gate_hook(&env, &body);
    respond(conn, "200 OK", &answer(code, &out, &err));
}

/// Run the host gate hook with the unit's env and `payload` on stdin, bounded.
fn run_gate_hook(env: &[(OsString, OsString)], payload: &[u8]) -> (Option<i32>, Vec<u8>, Vec<u8>) {
    let exe = crate::execute_wrapped::resolve_wicked_core_exe();
    let mut cmd = Command::new(&exe);
    cmd.hardened()
        .arg("gate-hook")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
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
            "#!/bin/sh\ncat > /dev/null\n[ \"$WICKED_TEST_VERDICT\" = deny ] && { echo \"denied by test policy\" >&2; exit 2; }\nexit 0\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let prev = std::env::var_os("WICKED_CORE_EXE");
        std::env::set_var("WICKED_CORE_EXE", &fake);
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
        register(&allow, vec![("WICKED_TEST_VERDICT".into(), "allow".into())]);
        register(&deny, vec![("WICKED_TEST_VERDICT".into(), "deny".into())]);
        assert_eq!(post(&allow), serde_json::json!({}));
        let d = post(&deny);
        assert_eq!(d["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(
            d["hookSpecificOutput"]["permissionDecisionReason"],
            "denied by test policy"
        );
        revoke(&allow);
        assert_eq!(
            post(&allow)["hookSpecificOutput"]["permissionDecision"],
            "deny",
            "revoked token"
        );
        match prev {
            Some(v) => std::env::set_var("WICKED_CORE_EXE", v),
            None => std::env::remove_var("WICKED_CORE_EXE"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
