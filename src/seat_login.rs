//! Throwaway login homes -> broker credentials. No operator config home is opened.
//! Crew runs the printed interactive argv in its existing PTY, then calls ingest.
//! Login pods use the local Kubernetes cluster; the broker stores the resulting credentials.
//! Broker expiry is reported in Unix seconds (Claude stores milliseconds).

#![cfg_attr(not(unix), allow(dead_code))]
// agy lg-r1 A7

use std::path::Path;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::Serialize;
use serde_json::Value;

#[cfg(unix)]
use crate::container_floor::output_within;
#[cfg(all(unix, test))]
use crate::container_floor::Runtime;
#[cfg(all(unix, test))]
use crate::seat_gateway::{volumes, Gateway};
#[cfg(unix)]
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const USAGE: &str = "usage: wicked-core seat-login <argv|ingest|status|sign-out|refresh> <claude|codex|pi|opencode|copilot|agy>\n  Set WICKED_SEAT_GATEWAY_DIR to the daemon's absolute gateway directory.";
const GATEWAY_DIR_ENV: &str = "WICKED_SEAT_GATEWAY_DIR";
#[cfg(unix)]
const TIMEOUT: Duration = Duration::from_secs(15);
// Bound credential reads as well as runtime execution. No untrusted file can fill a pipe forever.
pub(crate) const MAX_FILE_BYTES: usize = 1024 * 1024;

fn supported_cli(cli: &str) -> bool {
    matches!(
        cli,
        "claude" | "codex" | "pi" | "opencode" | "copilot" | "agy"
    )
}

/// Per daemon (its gateway's name) and per agent: two daemons on one runtime never share a login.
fn login_volume(gateway: &str, cli: &str) -> String {
    format!(
        "{gateway}-login-{cli}-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    )
}

/// The agents whose one-time sign-in runs the HOST's own CLI (DES-SEAT-LOGIN-SUBSCRIPTIONS
/// "Host-side login"). Their browser logins listen on a loopback callback (codex and pi on
/// `127.0.0.1:1455`, pi's Claude login on `:53692`, opencode on `[::1]:1455`), which the
/// operator's browser reaches only when the login runs on the host; in a login container it was
/// unreachable, and the device-code fallbacks need an account that allows device codes. The CLI
/// runs with a throwaway home ([`host_login_argv`]); the seat still sees placeholders only.
/// claude (a paste-the-code flow) and copilot (GitHub's device flow, which every account has)
/// keep their login container.
pub(crate) fn host_login(cli: &str) -> bool {
    matches!(cli, "codex" | "pi" | "opencode")
}

/// The throwaway home of host login session `session` (a [`login_volume`] name): under the
/// daemon's gateway directory, which no seat ever sees (`cover_gateway_dir`).
fn login_home(dir: &Path, session: &str) -> std::path::PathBuf {
    dir.join("login").join(session)
}

/// The pid file of host login session `session` (written by the login's own shell).
fn login_pid_file(dir: &Path, session: &str) -> std::path::PathBuf {
    dir.join("login").join(format!("{session}.pid"))
}

/// The host login's own command, run in the throwaway home: it records its pid (`$2`, the
/// session's pid file; the PTY child leads its own process group, so this is also the group a
/// sign-out ends), shows the banner (`$1`), then becomes the CLI.
const HOST_LOGIN: &str = "cd \"$HOME\" || exit 1; printf '%s' \"$$\" > \"$2\" || exit 1; printf '%s\\n\\n' \"$1\"; shift 2; exec \"$@\"";

/// opencode's login: its own `auth login`, then (only if it succeeded) `auth export` of the
/// credential rows it stored (opencode 2.x keeps them in SQLite, never an `auth.json`) to the
/// file ingest reads, written whole or not at all (a temporary file renamed into place). A failed
/// login or export leaves no export and exits nonzero: ingest reports the sign-in missing, and the
/// user signs in again. `--standalone`: a private server inside this throwaway home, never the
/// operator's background opencode service (which holds the operator's own credentials).
const OPENCODE_HOST_LOGIN: &str = "cd \"$HOME\" || exit 1; printf '%s' \"$$\" > \"$2\" || exit 1; printf '%s\\n\\n' \"$1\"; oc=$3; d=\"$XDG_DATA_HOME/opencode\" && \"$oc\" auth login --standalone && umask 077 && mkdir -p \"$d\" && { \"$oc\" auth export --standalone > \"$d/wicked-auth.json.tmp\" && mv \"$d/wicked-auth.json.tmp\" \"$d/wicked-auth.json\" || { rm -f \"$d/wicked-auth.json.tmp\"; exit 1; }; }";

/// The daemon variables a host login keeps: the search path, locale and terminal, and the
/// network settings a corporate host needs to reach the vendor (proxy, CA bundle). Everything
/// else is dropped (`env -i`): no API key, token, or CLI home override of the operator's reaches
/// the login.
const HOST_LOGIN_PASSTHROUGH: &[&str] = &[
    "PATH",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "USER",
    "LOGNAME",
    "COLORTERM",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
];

/// The isolated environment of a host login in throwaway `home`: HOME, TMPDIR, every XDG base
/// directory and the CLI's own home variable all point inside it, so neither the operator's real
/// logins nor anything under the real `~` is read or written.
fn host_login_env(home: &Path, cli: &str) -> Vec<(String, String)> {
    let h = home.to_string_lossy();
    let mut env: Vec<(String, String)> = [
        ("HOME", String::new()),
        ("TMPDIR", "/tmp".into()),
        ("XDG_CONFIG_HOME", "/config".into()),
        ("XDG_DATA_HOME", "/data".into()),
        ("XDG_STATE_HOME", "/state".into()),
        ("XDG_CACHE_HOME", "/cache".into()),
    ]
    .into_iter()
    .map(|(k, sub)| (k.to_string(), format!("{h}{sub}")))
    .collect();
    for (var, dir) in login_homes(cli) {
        let value = format!("{h}/{dir}");
        match env.iter_mut().find(|(k, _)| k == var) {
            Some(slot) => slot.1 = value,
            None => env.push(((*var).to_string(), value)),
        }
    }
    env
}

/// The directories a host login's environment names (all created 0700 before the login runs:
/// codex refuses a `CODEX_HOME` that does not exist).
fn host_login_dirs(home: &Path, cli: &str) -> Vec<std::path::PathBuf> {
    host_login_env(home, cli)
        .into_iter()
        .map(|(_, v)| std::path::PathBuf::from(v))
        .filter(|p| p != home)
        .collect()
}

/// The host CLI's own login command (after the executable), forcing file storage where the CLI
/// could otherwise use the macOS keychain: codex's `cli_auth_credentials_store` (`auto` may pick
/// the keychain). pi writes `auth.json` and opencode its SQLite DB, both under the throwaway home.
/// pi's login is `/login` inside its TUI, so the TUI starts with no tools, extensions, MCP,
/// skills, context files, prompt templates or saved session: a sign-in terminal, not an agent.
fn host_login_command(cli: &str) -> &'static [&'static str] {
    match cli {
        "codex" => &["login", "-c", "cli_auth_credentials_store=\"file\""],
        "pi" => &[
            "--no-tools",
            "--no-extensions",
            "--no-mcp",
            "--no-skills",
            "--no-context-files",
            "--no-prompt-templates",
            "--no-session",
            "--no-approve",
        ],
        _ => &[],
    }
}

/// What the operator does in the host login terminal, shown first.
fn host_login_hint(cli: &str) -> &'static str {
    match cli {
        "codex" => "Your browser opens: sign in with ChatGPT there (no device code needed). If it does not open, open the URL below in a browser on this machine.",
        "pi" => "Type /login, choose \"Sign in with an account\", then Anthropic (Claude Pro/Max), GitHub Copilot, or \"OpenAI Codex (legacy)\" > Browser login (ChatGPT). Repeat /login for more providers, then exit pi (ctrl+d) to finish.",
        "opencode" => "Choose OpenAI > \"Codex browser (legacy)\" for ChatGPT, GitHub Copilot, or an API key. \"Sign in with ChatGPT\" is not supported for seats yet.",
        _ => "",
    }
}

/// The first `x.y.z` in a `--version` answer.
fn parse_version(text: &str) -> Option<String> {
    text.split(|c: char| c.is_whitespace() || c == 'v' || c == ',')
        .find(|w| {
            let parts: Vec<&str> = w.split('.').collect();
            parts.len() == 3
                && parts
                    .iter()
                    .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        })
        .map(str::to_string)
}

/// The version the seat image pins for `cli` (`@scope/pkg@x.y.z`).
fn pinned_version(cli: &str) -> Option<&'static str> {
    crate::container_floor::seat_package(cli).and_then(|p| p.version())
}

/// The banner of a host login: what runs, where the credential goes, the version check, and the
/// operator's next step. The credential format is what ingest validates; a host CLI that wrote
/// another shape is refused there by name, never half-imported.
fn host_login_banner(cli: &str, host: &str, pinned: Option<&str>) -> String {
    let check = match pinned {
        Some(p) if p != host => format!(
            "WARNING: the seats run {cli} {p}; if the sign-in is refused, install {cli} {p} on this host"
        ),
        Some(_) => "the version the seats run".to_string(),
        None => "the seat image is the operator's own".to_string(),
    };
    format!(
        "wicked: signing in {cli} for container seats with this host's {cli} {host} ({check}).\n\
         It runs in a throwaway home: your own {cli} sign-in is never read or changed, and the \
         home is deleted when this terminal ends.\n{}",
        host_login_hint(cli)
    )
}

/// The host executable named `cli` on `path` (the daemon's search path).
#[cfg(unix)]
pub(crate) fn find_on_path(
    cli: &str,
    path: Option<&std::ffi::OsStr>,
) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(path?)
        .filter(|d| d.is_absolute())
        .map(|d| d.join(cli))
        .find(|p| {
            std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// Pure argv of a host login: `env -i` with only [`HOST_LOGIN_PASSTHROUGH`] (from `daemon_env`)
/// and the throwaway home's variables, then `sh -c` running the CLI in that home.
pub(crate) fn host_login_argv(
    cli: &str,
    exe: &Path,
    home: &Path,
    pid_file: &Path,
    banner: &str,
    daemon_env: &[(String, String)],
) -> Vec<String> {
    let mut argv = vec!["/usr/bin/env".to_string(), "-i".to_string()];
    for name in HOST_LOGIN_PASSTHROUGH {
        if let Some((_, v)) = daemon_env.iter().find(|(k, _)| k == name) {
            argv.push(format!("{name}={v}"));
        }
    }
    argv.push("TERM=xterm-256color".into());
    for (k, v) in host_login_env(home, cli) {
        argv.push(format!("{k}={v}"));
    }
    let script = if cli == "opencode" {
        OPENCODE_HOST_LOGIN
    } else {
        HOST_LOGIN
    };
    argv.extend(["/bin/sh".into(), "-c".into(), script.into()]);
    argv.push("wicked-login".into());
    argv.push(banner.into());
    argv.push(pid_file.to_string_lossy().into_owned());
    argv.push(exe.to_string_lossy().into_owned());
    argv.extend(host_login_command(cli).iter().map(|s| (*s).to_string()));
    argv
}

/// The command a container-login agent's sign-in runs in its login container or pod (pure).
pub(crate) fn login_command(cli: &str) -> &'static [&'static str] {
    match cli {
        // A long-lived token the claude CLI mints for scripts (core#1024): its seats pass it
        // through, so the broker never renews a claude sign-in. The CLI prints the token and
        // keeps no copy; the login script reads it from claude's output (or, failing that, asks
        // for it pasted back unechoed) and it lands in the throwaway volume, where ingest reads it.
        "claude" => &["sh", "-c", CLAUDE_SETUP_TOKEN_LOGIN, "wicked-login"],
        // GitHub's device flow: copilot picks its loopback web flow unless it detects a headless
        // host, and a login container is not detected as one (its callback was unreachable).
        "copilot" => &["copilot", "login", "--device-code"],
        // agy's Gemini API key, pasted unechoed into the throwaway volume; agy itself never runs.
        "agy" => &["sh", "-c", AGY_API_KEY_LOGIN, "wicked-login"],
        _ => &[],
    }
}

/// Where agy's login leaves the Gemini API key the operator pasted.
const AGY_API_KEY_PATH: &str = "agy/wicked-gemini-key";

/// agy's API-key sign-in (the API-key path; its Google-account sign-in is agy's own login): the
/// operator pastes a Gemini API key (Google AI Studio) at a prompt that does not echo it, and it
/// lands, 0600, in the throwaway login volume, where ingest reads it. The script prints no part of
/// the key. Echo goes off BEFORE the prompt shows (a paste that follows the prompt at once was
/// echoed when it came first, found by the live test). It needs no network: the login container
/// gets none.
const AGY_API_KEY_LOGIN: &str = "trap 'stty echo 2>/dev/null' EXIT; trap 'exit 1' HUP INT TERM; \
printf '%s\\n' 'wicked: agy signs in with a Gemini API key (Google AI Studio, API keys).'; \
stty -echo 2>/dev/null; printf '%s' 'wicked: paste the key here (it is not shown) and press Enter: '; \
IFS= read -r k; r=$?; stty echo 2>/dev/null; printf '\\n'; \
[ \"$r\" -eq 0 ] && [ -n \"$k\" ] || { echo 'wicked: no key was pasted; sign agy in again' >&2; exit 1; }; \
(umask 077 && mkdir -p \"$WICKED_LOGIN_DIR\" && printf '%s' \"$k\" > \"$WICKED_LOGIN_DIR/wicked-gemini-key\") \
|| { echo 'wicked: the key could not be saved; sign agy in again' >&2; exit 1; }; \
echo 'wicked: key received; there is nothing more to type'";

/// The Gemini API key pasted at agy's sign-in (pure): the paste's terminal control sequences
/// dropped, the line trimmed, then one run of a key's characters. A refusal names the check that
/// failed and never repeats any part of the paste.
fn gemini_api_key(bytes: &[u8]) -> Result<String, String> {
    let text =
        std::str::from_utf8(bytes).map_err(|_| "agy: the pasted key is not text".to_string())?;
    let (text, finished) = strip_terminal_controls(text);
    if !finished {
        return Err(
            "agy: the paste of the Gemini API key did not finish; sign agy in again".into(),
        );
    }
    let key = text.trim();
    let why = if key.is_empty() {
        Some("no key was pasted")
    } else if key.len() > 256 {
        Some("the paste is longer than a key")
    } else if key.chars().any(char::is_whitespace) {
        Some("the paste has a space or a line break inside the key")
    } else if !key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        Some("the paste has characters a Gemini API key does not")
    } else if key.len() < 20 {
        Some("the paste is shorter than a key")
    } else {
        None
    };
    match why {
        Some(why) => Err(format!(
            "agy: {why}; sign agy in again and paste only the Gemini API key"
        )),
        None => Ok(key.to_string()),
    }
}

/// Where claude's login leaves the `claude setup-token` token: read from claude's own output, or
/// pasted back by the operator when it could not be read.
const CLAUDE_SETUP_TOKEN_PATH: &str = "claude/wicked-setup-token";

/// What claude's login prints the moment it has the token, before anything slow: ingest (and
/// the sign-in check) starts once the login terminal ends, and until then the operator only waits.
macro_rules! claude_token_received {
    () => {
        "wicked: token received \u{2014} checking it with Claude now, this can take up to a minute; there is nothing more to type"
    };
}

/// The python3 program that reads the `claude setup-token` token from a capture of claude's
/// terminal output: `python3 -c PROGRAM CAPTURE "ROWS COLS" DEST`. Exit 0 with the token, and
/// nothing else, written owner-only to DEST; any other exit leaves DEST untouched. The seat image
/// installs python3 (containers/seats/seat.Dockerfile).
///
/// util-linux `script` frames the capture with its own lines even under `-q` (`Script started on
/// …]` first, `\nScript done on …]` last); they are dropped first, or the footer's line break would
/// read as the blank row that ends a token. The capture is replayed onto a minimal screen of the
/// terminal's size. Escape sequences are
/// parsed as [`strip_terminal_controls`] parses them (CSI, OSC, DCS, SOS, PM, APC, SS3, two-byte;
/// a line break ends one); only cursor moves, erases, the alternate screen and autowrap act, and
/// every other sequence (SGR colours, modes) is dropped. A frame Ink erased and redrew so counts
/// once, and the alternate screen is read apart from the main screen. On each screen, the first
/// non-blank row after a row reading "Your OAuth token" must hold `sk-ant-oat` with only spaces
/// before it and only token characters (`A-Za-z0-9-_`) from there: that column is the token's.
/// After a row that ended exactly at the terminal width, the next row is either blank (the token
/// ends) or holds token characters alone starting at that same column (it is joined); anything
/// else there (text around a continuation, another column) is no token, never a truncated one.
/// After a shorter row, a blank row or a row of other text ("Store this token securely…") ends
/// the token, and a row of token characters is no token. Text around the token, two different
/// tokens, or none: no token (the script then asks for the paste). So is a full-width row the
/// capture ends on (its continuation never came; blank rows at the end of a screen, such as rows
/// a taller earlier frame left erased, do not count), and an unknown terminal size (`stty size`
/// failed): wrapping cannot be checked without the width. The token is held to the shape [`setup_token`] checks again
/// at ingest (one `sk-ant-oat` prefix, 20 to 4096 characters).
macro_rules! claude_token_extractor {
    () => {
        r#"import os, sys
T = set("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_")
P = "sk-ant-oat"
def screens(s, H, W):
    done, rows, alt = [], {}, None
    r = c = b = 0
    sv = (0, 0)
    top = lambda: max(0, b - H + 1) if H else 0
    i, n = 0, len(s)
    while i < n:
        ch = s[i]
        i += 1
        if ch == "\x1b":
            k = s[i] if i < n else ""
            if k == "[":
                i += 1
                j = i
                while i < n and " " <= s[i] <= "?":
                    i += 1
                q = s[j:i]
                if i >= n or not "\x40" <= s[i] <= "~":
                    continue
                f = s[i]
                i += 1
                pv = q[:1] in ("<", "=", ">", "?")
                v = [int(x) if x.isdigit() else 0 for x in q.lstrip("<=>?").split(";")]
                if pv:
                    if q[:1] == "?" and f in "hl" and {47, 1047, 1049} & set(v):
                        if f == "h" and alt is None:
                            alt, rows, r, c, b = (rows, r, c, b, sv), {}, 0, 0, 0
                        elif f == "l" and alt is not None:
                            done.append(rows)
                            rows, r, c, b, sv = alt
                            alt = None
                    continue
                m = v[0] or 1
                if W and f != "m":
                    c = min(c, W - 1)
                if f == "A":
                    r = max(top(), r - m)
                elif f in "Be":
                    r = min(r + m, top() + H - 1) if H else r + m
                elif f in "Ca":
                    c = min(c + m, W - 1) if W else c + m
                elif f == "D":
                    c = max(0, c - m)
                elif f == "E":
                    r, c = (min(r + m, top() + H - 1) if H else r + m), 0
                elif f == "F":
                    r, c = max(top(), r - m), 0
                elif f in "G`":
                    c = m - 1
                elif f in "Hf":
                    r, c = top() + m - 1, (v[1] if len(v) > 1 and v[1] else 1) - 1
                elif f == "d":
                    r = top() + m - 1
                elif f == "J":
                    e = v[0]
                    if e == 0:
                        rows = {y: l if y < r else l[:c] for y, l in rows.items() if y <= r}
                    elif e == 1:
                        rows = {y: l if y > r or y < top() else [" "] * (c + 1) + l[c + 1:] for y, l in rows.items() if y >= r or y < top()}
                    elif e == 2:
                        rows = {y: l for y, l in rows.items() if y < top()}
                    elif e == 3:
                        rows = {y: l for y, l in rows.items() if y >= top()}
                elif f == "K":
                    e, l = v[0], rows.get(r, [])
                    rows[r] = l[:c] if e == 0 else [" "] * (c + 1) + l[c + 1:] if e == 1 else []
                elif f == "s" and not q:
                    sv = (r, c)
                elif f == "u" and not q:
                    r, c = sv
            elif k in ("]", "P", "X", "^", "_"):
                i += 1
                while i < n and s[i] not in "\r\n":
                    i += 1
                    if s[i - 1] == "\x07":
                        break
                    if s[i - 1] == "\x1b":
                        if i < n and s[i] == "\\":
                            i += 1
                        else:
                            i -= 1
                        break
            elif k == "O":
                i += 1
                if i < n and " " <= s[i] <= "~":
                    i += 1
            elif k and " " <= k <= "~":
                i += 1
                c = min(c, W - 1) if W else c
                if k <= "/" and i < n:
                    i += 1
                if k == "7":
                    sv = (r, c)
                elif k == "8":
                    r, c = sv
                elif k in "DE":
                    r, c = r + 1, 0 if k == "E" else c
                elif k == "M":
                    r = max(top(), r - 1)
        elif ch == "\r":
            c = 0
        elif ch in "\n\x0b\x0c":
            r, c = r + 1, 0
        elif ch == "\x08":
            c = max(0, (min(c, W - 1) if W else c) - 1)
        elif ch == "\t":
            c = min((c // 8 + 1) * 8, W - 1) if W else (c // 8 + 1) * 8
        elif ch >= " " and ch != "\x7f":
            if W and c >= W:
                r, c = r + 1, 0
            l = rows.setdefault(r, [])
            l.extend(" " * (c + 1 - len(l)))
            l[c] = ch
            c += 1
        b = max(b, r)
    done.append(rows)
    if alt is not None:
        done.append(alt[0])
    out = []
    for d in done:
        lines = ["".join(d.get(y, [])).rstrip() for y in range(max(d, default=-1) + 1)]
        while lines and not lines[-1]:
            lines.pop()
        out.append(lines)
    return out
def tokens(lines, W):
    out = set()
    for i, line in enumerate(lines):
        if "Your OAuth token" not in line:
            continue
        j = i + 1
        while j < len(lines) and not lines[j].strip():
            j += 1
        row = lines[j] if j < len(lines) else ""
        col = row.find(P)
        if col < 0:
            continue
        tok = row[col:]
        if row[:col].strip() or not set(tok) <= T:
            return None
        end = len(row)
        closed = False
        for nxt in lines[j + 1:]:
            body = nxt.lstrip(" ")
            if not body:
                closed = True
                break
            if end != W:
                if set(body) <= T:
                    return None
                closed = True
                break
            if not set(body) <= T or len(nxt) - len(body) != col:
                return None
            tok, end = tok + body, len(nxt)
        if end == W and not closed:
            return None
        out.add(tok)
    return out
def main(path, size, dest):
    with open(path, "rb") as f:
        s = f.read(64 << 20).decode("utf-8", "replace")
    if s.startswith("Script started on "):
        s = s[s.find("\n") + 1:]
        e = s.rfind("\nScript done on ")
        s = s if e < 0 else s[:e]
    d = size.split()
    if len(d) != 2 or not d[0].isdigit() or not d[1].isdigit() or int(d[1]) == 0:
        return 1
    H, W = int(d[0]), int(d[1])
    found = set()
    for lines in screens(s, H, W):
        t = tokens(lines, W)
        if t is None:
            return 1
        found |= t
    if len(found) != 1:
        return 1
    tok = found.pop()
    if not (20 <= len(tok) <= 4096 and tok.startswith(P) and tok.count(P) == 1):
        return 1
    fd = os.open(dest, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | getattr(os, "O_NOFOLLOW", 0), 0o600)
    with os.fdopen(fd, "w") as f:
        f.write(tok)
    return 0
sys.exit(main(*sys.argv[1:4]))
"#
    };
}

/// claude's login: `claude setup-token` runs under util-linux `script`, so claude keeps a real
/// terminal for its own "Paste code here if prompted >" prompt (the browser's authorization code
/// is all the operator pastes) while its output is captured to an owner-only file in a fresh
/// `mktemp -d` directory on the login container's tmpfs `/tmp`: never in the login volume, so even
/// a killed container leaves no capture behind. [`claude_token_extractor`] reads the token claude
/// printed from it, at the terminal's size when claude exits (`stty size`), and writes only the
/// token (owner-only) beside the CLI's own files for ingest. The capture is deleted as soon as it
/// was read, and by a trap on exit, HUP, INT and TERM. The token is never printed or logged.
///
/// The capture needs a terminal on stdin (the login container's `-it` always has one): util-linux
/// `script` stops reading at an input EOF without passing it on, so a CLI waiting for input
/// would wait forever. Without one, claude runs directly and the paste is asked for.
///
/// When the token cannot be read (no util-linux `script` or python3, no token row, an ambiguous
/// one), the script says so in one line and asks for the paste as before. No token, no file:
/// ingest reports the sign-in missing and the operator signs in again.
///
/// The paste is read in line mode (`stty sane`: Enter ends a line even if the TUI left the
/// terminal raw) with bracketed paste on. The CLI wraps the long token across two rows, so a
/// copied token can carry a line break; inside the paste brackets lines keep being read until
/// the closing bracket. A terminal that does not bracket pastes ends the token with an empty
/// line (the script asks for one Enter more), so a wrapped row is never taken for the whole
/// token. Blank lines before any token are skipped, and input that ends first (the window
/// closed, EOF inside an open paste) saves nothing. What lands is the paste as typed, line
/// breaks and brackets included: [`setup_token`] joins and checks it.
///
/// claude turns xterm mouse reporting and its keyboard modes on (the kitty keyboard protocol,
/// modifyOtherKeys, synchronized output), and its `setup-token` exit leaves them on (as it
/// leaves the terminal raw): studio's terminal then sends each click into the paste as a mouse
/// report and a drag selects nothing, so the token cannot be copied. The script turns those
/// modes off after claude exits. It leaves the alternate screen alone: leaving it would clear the
/// token from view when claude printed it on the main screen. Whatever control sequences still
/// reach the paste (a terminal's answer to a query claude sent, a click before the modes were
/// off) are not token text: `bare` drops them before a line is tested, so a line of them alone
/// counts as an empty line, and [`setup_token`] drops them from what lands. (`bare` ends a CSI
/// at `A`-`~`: a terminal never sends one ending in `@`, and a login argv never carries an `@`.)
///
/// Once the token is saved, [`claude_token_received`] says the check has started and nothing is
/// left to type. Echo stays off, so keys pressed while ingest runs are never echoed.
const CLAUDE_SETUP_TOKEN_LOGIN: &str = concat!(
    "td=; trap '[ -z \"$td\" ] || rm -rf \"$td\"' EXIT; trap 'exit 1' HUP INT TERM; \
[ -t 0 ] && case $(script -V 2>/dev/null) in *util-linux*) td=$(umask 077 && mktemp -d \"${TMPDIR:-/tmp}/wicked-login.XXXXXX\") || td=;; esac; \
if [ -n \"$td\" ]; then (umask 077 && exec script -q -e -c 'claude setup-token' \"$td/claude.out\") || exit 1; \
else claude setup-token || exit 1; fi; stty sane -echo 2>/dev/null; \
printf '\\033[?1000l\\033[?1002l\\033[?1003l\\033[?1005l\\033[?1006l\\033[?1015l\\033[<u\\033[=0;1u\\033[>4;0m\\033[?2026l'; \
s=$(stty size 2>/dev/null); \
if [ -n \"$td\" ] && mkdir -p \"$CLAUDE_CONFIG_DIR\" && python3 -c '",
    claude_token_extractor!(),
    "' \"$td/claude.out\" \"$s\" \"$CLAUDE_CONFIG_DIR/wicked-setup-token\" 2>/dev/null; \
then rm -rf \"$td\"; td=; printf '\\n%s\\n' '",
    claude_token_received!(),
    "'; exit 0; fi; rm -rf \"$td\"; td=; \
printf '\\n%s' \"wicked: could not read the token from claude's output; paste it instead\"; \
e=$(printf '\\033'); b=$(printf '\\007'); \
bare() { printf '%s\\n' \"$1\" | LC_ALL=C sed -e \"s|$e\\[[0-?]*[ -/]*[A-~]||g\" -e \"s|$e[]P_^X][^$b$e]*$b||g\" \
-e \"s|$e[]P_^X][^$e]*$e.||g\" -e \"s|${e}O.||g\" -e \"s|$e.||g\" -e \"s|$e||g\"; }; \
o=$(printf '\\033[200~'); c=$(printf '\\033[201~'); p=; d=; t=; \
printf '\\033[?2004h\\n%s' 'wicked: copy the token shown above, paste it here (it is not shown) and press Enter: '; \
while IFS= read -r l; do p=\"$p$l\n\"; \
case \"$p\" in *\"$o\"*) case \"$p\" in *\"$c\"*) d=1; break;; esac; continue;; esac; \
l=$(bare \"$l\"); \
case \"$l\" in *[A-Za-z0-9_-]*) t=1; printf '\\r\\n%s' 'wicked: press Enter once more to finish (or paste the rest of the token): ';; \
*) [ -z \"$t\" ] || { d=1; break; };; esac; done; \
printf '\\033[?2004l\\n'; \
case $(bare \"$p\") in *[A-Za-z]*) ;; *) echo 'wicked: no token was pasted; sign claude in again' >&2; exit 1;; esac; \
[ -n \"$d\" ] || { echo 'wicked: the paste did not finish; sign claude in again' >&2; exit 1; }; \
mkdir -p \"$CLAUDE_CONFIG_DIR\" && (umask 077 && printf '%s' \"$p\" > \"$CLAUDE_CONFIG_DIR/wicked-setup-token\") \
|| { echo 'wicked: the token could not be saved; sign claude in again' >&2; exit 1; }; \
printf '%s\\n' '",
    claude_token_received!(),
    "'"
);

/// The `claude setup-token` token pasted back at sign-in (pure): the paste's terminal control
/// sequences dropped ([`strip_terminal_controls`]: its bracketed-paste markers, and the mouse
/// reports a click sends while mouse reporting is on), its lines (the CLI wraps the token across
/// rows, so a copy can carry a line break) trimmed and joined, then one run of a token's
/// characters with the prefix Anthropic's long-lived OAuth tokens carry. A refusal names the
/// check that failed and never repeats any part of the paste.
fn setup_token(bytes: &[u8]) -> Result<String, String> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| "claude: the pasted token is not text".to_string())?;
    let (text, finished) = strip_terminal_controls(text);
    // A paste cut short (its closing bracket never came) is a truncated token, never a token.
    if !finished {
        return Err(
            "claude: the paste of the `claude setup-token` token did not finish; sign claude in \
             again and paste the whole token it prints"
                .into(),
        );
    }
    // Whitespace inside a line is kept (and refused below): only a row break is joined.
    let text: String = text.split(['\r', '\n']).map(str::trim).collect();
    match setup_token_shape(&text) {
        Ok(()) => Ok(text),
        Err(why) => Err(format!(
            "claude: {why}; sign claude in again and paste only the token `claude setup-token` \
             prints"
        )),
    }
}

/// Which shape check a joined paste fails (pure), named by its category alone: never by any
/// part of the paste.
fn setup_token_shape(text: &str) -> Result<(), &'static str> {
    const PREFIX: &str = "sk-ant-oat";
    let token_char = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    let token = |t: &str| t.starts_with(PREFIX) && t.chars().all(token_char);
    if text.len() > 4096 {
        return Err("the paste is longer than a token");
    }
    match text.matches(PREFIX).count() {
        0 => return Err("the paste does not contain an sk-ant-oat… token"),
        1 => {}
        _ => return Err("the token appears more than once"),
    }
    if token(text) {
        return if text.len() < 20 {
            Err("the token in the paste is shorter than a token")
        } else {
            Ok(())
        };
    }
    let squeezed: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    if token(&squeezed) {
        return Err("the paste has a space inside the token");
    }
    Err("the paste had text before or after the token")
}

/// A paste with its terminal control sequences dropped (pure), and whether its bracketed-paste
/// markers balance (every `ESC [ 200 ~` closed by an `ESC [ 201 ~`). Dropped: CSI (`ESC [`,
/// parameter and intermediate bytes, one final byte; this covers the bracketed-paste markers and
/// the SGR mouse reports `ESC [ < b ; x ; y M|m` a click sends, a terminal's answer to a query,
/// and a key the kitty keyboard protocol or modifyOtherKeys encodes, `ESC [ 13 u` or
/// `ESC [ 27 ; 1 ; 13 ~`), OSC, DCS, SOS, PM and APC (`ESC ]`, `ESC P`, `ESC X`, `ESC ^`,
/// `ESC _`, up to BEL or `ESC \`), SS3 (`ESC O` and one byte) and any other two-byte escape.
/// A sequence the paste cuts short ends where it was cut: a line break ends any sequence and is
/// kept (an unterminated string never swallows the token's next row), and an ESC that is not
/// `ESC \` ends a string and starts the next sequence (a bracketed-paste marker is still counted).
fn strip_terminal_controls(text: &str) -> (String, bool) {
    let mut out = String::with_capacity(text.len());
    let (mut opened, mut closed) = (0usize, 0usize);
    let mut chars = text.chars().peekable();
    // An ESC already read that starts the next sequence (it ended a control string that was not
    // closed by ST).
    let mut esc = false;
    loop {
        if !esc {
            match chars.next() {
                None => break,
                Some('\u{1b}') => {}
                Some(c) => {
                    out.push(c);
                    continue;
                }
            }
        }
        esc = false;
        match chars.peek().copied() {
            Some('[') => {
                chars.next();
                let mut params = String::new();
                while let Some(&c) = chars.peek() {
                    match c {
                        ' '..='?' => params.push(c),
                        '@'..='~' => {
                            chars.next();
                            if c == '~' && params == "200" {
                                opened += 1;
                            } else if c == '~' && params == "201" {
                                closed += 1;
                            }
                            break;
                        }
                        _ => break,
                    }
                    chars.next();
                }
            }
            Some(']' | 'P' | 'X' | '^' | '_') => {
                chars.next();
                while let Some(&c) = chars.peek() {
                    match c {
                        '\r' | '\n' => break,
                        '\u{7}' => {
                            chars.next();
                            break;
                        }
                        '\u{1b}' => {
                            chars.next();
                            esc = chars.next_if_eq(&'\\').is_none();
                            break;
                        }
                        _ => {
                            chars.next();
                        }
                    }
                }
            }
            Some('O') => {
                chars.next();
                chars.next_if(|c| (' '..='~').contains(c));
            }
            Some(' '..='~') => {
                chars.next();
            }
            _ => {}
        }
    }
    (out, opened == closed)
}

/// Each CLI's configuration-home variable and its directory under the throwaway HOME. The init
/// helper creates every one of them before the login runs: codex refuses a `CODEX_HOME` that
/// does not exist ("Error loading configuration"), so its login exited before any sign-in and
/// ingest then found no `auth.json`.
pub(crate) fn login_homes(cli: &str) -> &'static [(&'static str, &'static str)] {
    match cli {
        "claude" => &[("CLAUDE_CONFIG_DIR", "claude")],
        "codex" => &[("CODEX_HOME", "codex")],
        "pi" => &[("PI_CODING_AGENT_DIR", "pi")],
        "opencode" => &[("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")],
        "copilot" => &[("COPILOT_HOME", "copilot")],
        // Not agy's own home (agy never runs at this sign-in): where its pasted key is saved.
        "agy" => &[("WICKED_LOGIN_DIR", "agy")],
        _ => &[],
    }
}

/// Paths under the throwaway HOME, aligned with login_argv. Bare names are accepted by the
/// pure extractor too, so callers may hand it files relative to a CLI's configuration home.
fn credential_paths(cli: &str) -> &'static [&'static str] {
    match cli {
        "claude" => &[
            CLAUDE_SETUP_TOKEN_PATH,
            "claude/.credentials.json",
            "claude/settings.json",
        ],
        "codex" => &["codex/auth.json"],
        "pi" => &["pi/auth.json", "pi/models.json"],
        // opencode 2.x keeps credentials in its SQLite DB; the login exports them here.
        "opencode" => &["data/opencode/wicked-auth.json"],
        // Written only because the init helper seeds `storeTokenPlaintext` (no keychain here).
        "copilot" => &["copilot/config.json"],
        "agy" => &[AGY_API_KEY_PATH],
        _ => &[],
    }
}

pub(crate) fn broker_files(cli: &str) -> Result<&'static [&'static str], String> {
    match cli {
        "claude" => Ok(&[
            "anthropic.token",
            "anthropic.key",
            "claude.credentials.json",
        ]),
        "codex" => Ok(&[
            "openai.token",
            "openai.account",
            "openai.key",
            "codex.auth.json",
        ]),
        // The provider key, the retained login record and each subscription's route files
        // (`seat_credentials::SUBSCRIPTION_ROUTES`).
        "pi" => Ok(&[
            "pi.token",
            "pi.auth.json",
            "pi.anthropic.token",
            "pi.chatgpt.token",
            "pi.chatgpt.account",
            "pi.copilot.token",
            "pi.copilot.host",
        ]),
        "opencode" => Ok(&[
            "opencode.token",
            "opencode.credentials.json",
            "opencode.chatgpt.token",
            "opencode.chatgpt.account",
            "opencode.copilot.token",
            "opencode.copilot.host",
            "opencode.anthropic.key",
        ]),
        // The GitHub token, and the account's Copilot API host and models the broker read with
        // it (`copilot_account`): identifiers beside the credential, emptied with it.
        "copilot" => Ok(&[
            "copilot.token",
            crate::copilot_account::HOST_FILE,
            crate::copilot_account::MODELS_FILE,
        ]),
        // The Gemini API key its route serves.
        "agy" => Ok(&[crate::seat_gateway::GEMINI_KEY_FILE]),
        _ => Err("unsupported login CLI".into()),
    }
}

fn input_file<'a>(
    cli: &str,
    files: &'a [(String, Vec<u8>)],
    path: &str,
) -> Result<Option<&'a [u8]>, String> {
    let bare = path.rsplit('/').next().unwrap_or(path);
    let mut found = files.iter().filter(|(p, _)| p == path || p == bare);
    let first = found.next();
    if found.next().is_some() {
        return Err(format!("{cli}: duplicate credential file {bare}"));
    }
    Ok(first.map(|(_, bytes)| bytes.as_slice()))
}

fn parse_json(cli: &str, bytes: &[u8]) -> Result<Value, String> {
    if bytes.len() > MAX_FILE_BYTES {
        return Err(format!("{cli}: credential file exceeds size limit"));
    }
    // Never forward serde diagnostics: an offending string may be a credential.
    serde_json::from_slice(bytes).map_err(|_| format!("{cli}: invalid credential JSON"))
}

pub(crate) fn nonempty(value: Option<&Value>) -> Option<&str> {
    // agy lg-r1 A5: one credential is one header value, never embedded whitespace/control.
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.chars().any(|c| c.is_control() || c.is_whitespace()))
}

fn validate_optional_token(value: Option<&Value>) -> Result<(), String> {
    if value.is_some_and(|v| !v.is_null()) && nonempty(value).is_none() {
        return Err("invalid credential token".into());
    }
    Ok(())
}

fn missing(cli: &str, field: &str) -> String {
    format!("{cli}: signed out; missing {field}")
}

fn token_file(name: &str, value: &str) -> (String, Vec<u8>) {
    (name.into(), value.as_bytes().to_vec())
}

/// Pure, complete extraction. Unknown files and fields never become broker credentials.
/// The whole original OAuth record is retained only for the broker's future refresher.
pub(crate) fn extract_tokens(
    cli: &str,
    files: &[(String, Vec<u8>)],
) -> Result<Vec<(String, Vec<u8>)>, String> {
    broker_files(cli)?;
    match cli {
        "claude" => {
            // The seat's pass-through token (core#1024). It is not an OAuth record: nothing to
            // renew, and it replaces any record an earlier `claude auth login` left.
            if let Some(bytes) = input_file(cli, files, CLAUDE_SETUP_TOKEN_PATH)? {
                return Ok(vec![token_file("anthropic.token", &setup_token(bytes)?)]);
            }
            for path in &credential_paths(cli)[1..] {
                let Some(bytes) = input_file(cli, files, path)? else {
                    continue;
                };
                let value = parse_json(cli, bytes)?;
                if value.get("claudeAiOauth").is_some_and(|v| !v.is_null()) {
                    let access = nonempty(value.pointer("/claudeAiOauth/accessToken"))
                        .ok_or_else(|| missing(cli, "claudeAiOauth.accessToken"))?;
                    validate_optional_token(value.pointer("/claudeAiOauth/refreshToken"))?;
                    credential_expiry(cli, &value)?;
                    return Ok(vec![
                        ("claude.credentials.json".into(), bytes.to_vec()),
                        token_file("anthropic.token", access),
                    ]);
                }
                // Claude's placeholder module confirms env-only API-key mode. Accept an
                // explicitly stored key if present; never invent or read a host env key.
                if let Some(key) = nonempty(value.get("ANTHROPIC_API_KEY"))
                    .or_else(|| nonempty(value.pointer("/env/ANTHROPIC_API_KEY")))
                {
                    return Ok(vec![token_file("anthropic.key", key)]);
                }
            }
            // No token and no credential record (a settings file without one does not count):
            // the token `claude setup-token` printed was neither read from its output nor pasted
            // back, or the window closed first.
            Err(missing(
                cli,
                "the `claude setup-token` token (it was neither read from claude's output nor \
                 pasted back); sign claude in again",
            ))
        }
        "codex" => {
            let bytes = input_file(cli, files, "codex/auth.json")?
                .ok_or_else(|| missing(cli, "auth.json"))?;
            let value = parse_json(cli, bytes)?;
            let mode = value.get("auth_mode").and_then(Value::as_str);
            if mode == Some("apikey")
                || (mode.is_none() && nonempty(value.get("OPENAI_API_KEY")).is_some())
            {
                let key = nonempty(value.get("OPENAI_API_KEY"))
                    .ok_or_else(|| missing(cli, "OPENAI_API_KEY"))?;
                return Ok(vec![
                    ("codex.auth.json".into(), bytes.to_vec()),
                    token_file("openai.key", key),
                ]);
            }
            if mode.is_some_and(|m| m != "chatgpt") {
                return Err("codex: unsupported auth_mode".into());
            }
            let token = nonempty(value.pointer("/tokens/access_token"))
                .ok_or_else(|| missing(cli, "tokens.access_token"))?;
            validate_optional_token(value.pointer("/tokens/refresh_token"))?;
            validate_optional_token(value.pointer("/tokens/id_token"))?;
            let account = nonempty(value.pointer("/tokens/account_id"))
                .ok_or_else(|| missing(cli, "tokens.account_id"))?;
            credential_expiry(cli, &value)?;
            // codex lg-r1 #8: reject header injection before touching the broker.
            crate::seat_gateway::validate_chatgpt_account(Some(account))?;
            Ok(vec![
                ("codex.auth.json".into(), bytes.to_vec()),
                token_file("openai.account", account),
                token_file("openai.token", token),
            ])
        }
        "pi" => extract_pi(files),
        "opencode" => extract_opencode(files),
        "copilot" => extract_copilot(files),
        "agy" => {
            let bytes = input_file(cli, files, AGY_API_KEY_PATH)?.ok_or_else(|| {
                missing(
                    cli,
                    "the Gemini API key (none was pasted); sign agy in again",
                )
            })?;
            Ok(vec![token_file(
                crate::seat_gateway::GEMINI_KEY_FILE,
                &gemini_api_key(bytes)?,
            )])
        }
        _ => Err("unsupported login CLI".into()),
    }
}

/// pi's provider ids the broker serves, and the subscription each OAuth sign-in rides.
const PI_OAUTH: &[(&str, crate::seat_credentials::Subscription)] = &[
    (
        "anthropic",
        crate::seat_credentials::Subscription::PiAnthropic,
    ),
    (
        "openai-codex",
        crate::seat_credentials::Subscription::PiChatgpt,
    ),
    (
        "github-copilot",
        crate::seat_credentials::Subscription::PiCopilot,
    ),
];

/// The field of the broker's copy of pi's Copilot entry that keeps the plan endpoint the
/// token exchange named (`endpoints.api`); pi itself never reads the broker's copy.
pub(crate) const COPILOT_ENDPOINT_FIELD: &str = "wickedApiEndpoint";

/// The Copilot API host a Copilot token names (`proxy-ep=proxy.<plan>.githubcopilot.com`, as pi
/// derives it), else the exchange's plan `endpoint`, else the individual plan's host. Validated
/// before it reaches a route.
fn copilot_host_from_token(token: &str, endpoint: Option<&str>) -> Result<String, String> {
    let host = token
        .split(';')
        .find_map(|kv| kv.strip_prefix("proxy-ep="))
        .map(|h| match h.strip_prefix("proxy.") {
            Some(rest) => format!("api.{rest}"),
            None => h.to_string(),
        })
        .or_else(|| {
            endpoint.map(|e| {
                e.strip_prefix("https://")
                    .unwrap_or(e)
                    .trim_end_matches('/')
                    .to_string()
            })
        })
        .unwrap_or_else(|| "api.individual.githubcopilot.com".into());
    crate::seat_gateway::validate_copilot_host(&host)?;
    Ok(host)
}

/// One pi OAuth entry's route files (pure; `value` is the provider's `auth.json` entry).
fn pi_oauth_files(
    sub: crate::seat_credentials::Subscription,
    value: &Value,
) -> Result<Vec<(String, Vec<u8>)>, String> {
    use crate::seat_credentials::Subscription;
    let route = sub.route();
    let access =
        nonempty(value.get("access")).ok_or_else(|| missing("pi", "the OAuth access token"))?;
    validate_optional_token(value.get("refresh"))?;
    pi_entry_expiry(value)?;
    let mut out = vec![token_file(route.file, access)];
    match sub {
        Subscription::PiAnthropic => {}
        Subscription::PiChatgpt => {
            let account = nonempty(value.get("accountId"))
                .ok_or_else(|| missing("pi", "openai-codex accountId"))?;
            crate::seat_gateway::validate_chatgpt_account(Some(account))?;
            out.push(token_file(route.account_file.unwrap_or_default(), account));
        }
        Subscription::PiCopilot => {
            if nonempty(value.get("enterpriseUrl")).is_some() {
                return Err("pi: GitHub Enterprise Copilot sign-ins are not supported yet".into());
            }
            // The GitHub token the broker re-exchanges for each new Copilot token.
            nonempty(value.get("refresh"))
                .ok_or_else(|| missing("pi", "github-copilot GitHub token"))?;
            let host =
                copilot_host_from_token(access, nonempty(value.get(COPILOT_ENDPOINT_FIELD)))?;
            out.push(token_file(route.host_file.unwrap_or_default(), &host));
        }
        _ => return Err("pi: not a pi subscription".into()),
    }
    Ok(out)
}

/// A pi or opencode entry's expiry in Unix seconds (both store milliseconds); `None` when it has
/// none (opencode writes `0` for a credential that does not expire).
fn pi_entry_expiry(value: &Value) -> Result<Option<u64>, String> {
    match value.get("expires").filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => v
            .as_u64()
            .map(|ms| (ms > 0).then_some(ms / 1000))
            .ok_or_else(|| "invalid credential expiry".into()),
    }
}

/// pi's providers a login stored that the broker cannot serve yet (named in the ingest report,
/// never silently dropped).
fn pi_skipped(value: &Value) -> Vec<String> {
    let Some(map) = value.as_object() else {
        return Vec::new();
    };
    map.iter()
        .filter(|(id, v)| {
            let kind = v.get("type").and_then(Value::as_str);
            let served = match id.as_str() {
                "openai" => kind == Some("api_key"),
                id => PI_OAUTH.iter().any(|(p, _)| *p == id) && kind == Some("oauth"),
            };
            !served
        })
        .map(|(id, v)| {
            format!(
                "{id} ({})",
                v.get("type").and_then(Value::as_str).unwrap_or("unknown")
            )
        })
        .collect()
}

/// pi: every provider the login stored that the broker can serve (DES-SEAT-LOGIN-SUBSCRIPTIONS
/// decision 1). The OpenAI API key keeps its route (`pi.token`); each OAuth sign-in gets its own;
/// the whole `auth.json` is retained for the refresher when any OAuth sign-in is present.
fn extract_pi(files: &[(String, Vec<u8>)]) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut out = Vec::new();
    let mut skipped = Vec::new();
    if let Some(bytes) = input_file("pi", files, "pi/auth.json")? {
        let value = parse_json("pi", bytes)?;
        if !value.is_object() {
            return Err("pi: auth.json is not an object".into());
        }
        if let Some(openai) = value.get("openai") {
            match openai.get("type").and_then(Value::as_str) {
                Some("api_key") | None => {
                    if let Some(key) = nonempty(openai.get("key")) {
                        out.push(token_file("pi.token", key));
                    }
                }
                _ => {}
            }
        }
        let mut oauth = false;
        for (id, sub) in PI_OAUTH {
            let Some(entry) = value.get(*id) else {
                continue;
            };
            if entry.get("type").and_then(Value::as_str) != Some("oauth") {
                continue;
            }
            out.extend(pi_oauth_files(*sub, entry)?);
            oauth = true;
        }
        if oauth {
            out.push(("pi.auth.json".into(), bytes.to_vec()));
        }
        skipped = pi_skipped(&value);
    }
    if !out.iter().any(|(n, _)| n == "pi.token") {
        if let Some(bytes) = input_file("pi", files, "pi/models.json")? {
            let value = parse_json("pi", bytes)?;
            if let Some(key) = nonempty(value.pointer("/providers/openai/apiKey")) {
                out.push(token_file("pi.token", key));
            }
        }
    }
    if out.is_empty() {
        let mut why = missing(
            "pi",
            "a supported sign-in (Anthropic Claude Pro/Max, OpenAI ChatGPT Plus/Pro, GitHub \
             Copilot, or an OpenAI API key)",
        );
        if !skipped.is_empty() {
            why.push_str(&format!("; not supported yet: {}", skipped.join(", ")));
        }
        return Err(why);
    }
    Ok(out)
}

/// opencode: the active credential of each integration its login exported, onto the route
/// files that integration's sign-in rides; the whole export is retained when a refreshable
/// (ChatGPT) sign-in is present.
fn extract_opencode(files: &[(String, Vec<u8>)]) -> Result<Vec<(String, Vec<u8>)>, String> {
    let cli = "opencode";
    let bytes = input_file(cli, files, "data/opencode/wicked-auth.json")?
        .ok_or_else(|| missing(cli, "the login's credential export"))?;
    let export = parse_json(cli, bytes)?;
    if !export.is_array() {
        return Err("opencode: the credential export is not a list".into());
    }
    let mut out = Vec::new();
    let mut retain = false;
    if let Some(value) = opencode_active(&export, "openai").map(|r| &r["value"]) {
        match (
            value.get("type").and_then(Value::as_str),
            value.get("methodID").and_then(Value::as_str),
        ) {
            (Some("key"), _) => {
                let key = nonempty(value.get("key")).ok_or_else(|| missing(cli, "openai key"))?;
                out.push(token_file("opencode.token", key));
            }
            (Some("oauth"), Some("chatgpt-browser" | "chatgpt-headless")) => {
                let access = nonempty(value.get("access"))
                    .ok_or_else(|| missing(cli, "the ChatGPT access token"))?;
                validate_optional_token(value.get("refresh"))?;
                pi_entry_expiry(value)?;
                let account = nonempty(value.pointer("/metadata/accountID"))
                    .ok_or_else(|| missing(cli, "the ChatGPT accountID"))?;
                crate::seat_gateway::validate_chatgpt_account(Some(account))?;
                out.push(token_file("opencode.chatgpt.token", access));
                out.push(token_file("opencode.chatgpt.account", account));
                retain = true;
            }
            _ => {}
        }
    }
    if let Some(value) = opencode_active(&export, "github-copilot").map(|r| &r["value"]) {
        if value.get("type").and_then(Value::as_str) == Some("oauth") {
            if nonempty(value.pointer("/metadata/enterpriseUrl")).is_some() {
                return Err(
                    "opencode: GitHub Enterprise Copilot sign-ins are not supported yet".into(),
                );
            }
            // opencode sends the GitHub token itself to the Copilot API (refresh = access).
            let token = nonempty(value.get("refresh"))
                .or_else(|| nonempty(value.get("access")))
                .ok_or_else(|| missing(cli, "the GitHub token"))?;
            let host = match nonempty(value.pointer("/metadata/apiEndpoint")) {
                Some(url) => url
                    .strip_prefix("https://")
                    .unwrap_or(url)
                    .trim_end_matches('/')
                    .to_string(),
                None => crate::seat_gateway::COPILOT_DEFAULT_HOST.to_string(),
            };
            crate::seat_gateway::validate_copilot_host(&host)?;
            out.push(token_file("opencode.copilot.token", token));
            out.push(token_file("opencode.copilot.host", &host));
        }
    }
    if let Some(value) = opencode_active(&export, "anthropic").map(|r| &r["value"]) {
        if value.get("type").and_then(Value::as_str) == Some("key") {
            let key = nonempty(value.get("key")).ok_or_else(|| missing(cli, "anthropic key"))?;
            out.push(token_file("opencode.anthropic.key", key));
        }
    }
    if retain {
        out.push(("opencode.credentials.json".into(), bytes.to_vec()));
    }
    if out.is_empty() {
        let mut why = missing(
            cli,
            "a supported sign-in (OpenAI ChatGPT via chatgpt-headless or chatgpt-browser, GitHub \
             Copilot, or an OpenAI or Anthropic API key)",
        );
        let skipped = opencode_skipped(&export);
        if !skipped.is_empty() {
            why.push_str(&format!("; not supported yet: {}", skipped.join(", ")));
        }
        return Err(why);
    }
    Ok(out)
}

/// copilot: the token of the last signed-in github.com user from `copilot/config.json` (JSON
/// behind a `//` comment header). GitHub Enterprise hosts are refused by name.
fn extract_copilot(files: &[(String, Vec<u8>)]) -> Result<Vec<(String, Vec<u8>)>, String> {
    let cli = "copilot";
    let bytes = input_file(cli, files, "copilot/config.json")?
        .ok_or_else(|| missing(cli, "config.json (did the login store its token?)"))?;
    // The file as copilot writes it: optional BOM, `//` header lines, then JSON.
    let text = std::str::from_utf8(bytes).map_err(|_| "copilot: invalid credential JSON")?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let body: String = text
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    let value = parse_json(cli, body.as_bytes())?;
    let host = nonempty(value.pointer("/lastLoggedInUser/host"))
        .ok_or_else(|| missing(cli, "lastLoggedInUser.host"))?;
    if host != "https://github.com" {
        return Err("copilot: GitHub Enterprise sign-ins are not supported yet".into());
    }
    let login = nonempty(value.pointer("/lastLoggedInUser/login"))
        .ok_or_else(|| missing(cli, "lastLoggedInUser.login"))?;
    let tokens = value
        .get("authTokens")
        .and_then(Value::as_object)
        .ok_or_else(|| missing(cli, "authTokens"))?;
    let token = [format!("{host}:{login}"), format!("{host}:{login}:github")]
        .iter()
        .find_map(|k| nonempty(tokens.get(k).and_then(|t| t.get("token"))))
        .ok_or_else(|| missing(cli, "the signed-in user's token"))?;
    Ok(vec![token_file("copilot.token", token)])
}

/// opencode's exported sign-ins the broker cannot serve yet, by integration and method only.
fn opencode_skipped(export: &Value) -> Vec<String> {
    let Some(rows) = export.as_array() else {
        return Vec::new();
    };
    let mut ids: Vec<&str> = rows
        .iter()
        .filter_map(|r| r.get("integrationID").and_then(Value::as_str))
        .collect();
    ids.sort();
    ids.dedup();
    ids.into_iter()
        .filter_map(|id| {
            let value = &opencode_active(export, id)?["value"];
            let kind = value
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let method = value.get("methodID").and_then(Value::as_str);
            let served = matches!(
                (id, kind, method),
                ("openai" | "anthropic", "key", _)
                    | (
                        "openai",
                        "oauth",
                        Some("chatgpt-browser" | "chatgpt-headless")
                    )
                    | ("github-copilot", "oauth", _)
            );
            (!served).then(|| format!("{id} ({})", method.unwrap_or(kind)))
        })
        .collect()
}

/// The providers of `cli`'s login that ingest stored nowhere (not supported yet), by name only.
pub(crate) fn skipped_providers(cli: &str, files: &[(String, Vec<u8>)]) -> Vec<String> {
    if cli == "opencode" {
        return input_file(cli, files, "data/opencode/wicked-auth.json")
            .ok()
            .flatten()
            .and_then(|b| parse_json(cli, b).ok())
            .map(|v| opencode_skipped(&v))
            .unwrap_or_default();
    }
    if cli != "pi" {
        return Vec::new();
    }
    input_file(cli, files, "pi/auth.json")
        .ok()
        .flatten()
        .and_then(|b| parse_json(cli, b).ok())
        .map(|v| pi_skipped(&v))
        .unwrap_or_default()
}

/// The broker file retaining `cli`'s whole login record (for its refresher and expiries).
fn retained_record(cli: &str) -> Option<&'static str> {
    match cli {
        "pi" => Some("pi.auth.json"),
        "opencode" => Some("opencode.credentials.json"),
        _ => None,
    }
}

/// The entry `key` of `cli`'s login record: pi's provider object; opencode's credential value
/// of that integration (its active one).
pub(crate) fn record_entry<'a>(cli: &str, record: &'a Value, key: &str) -> Option<&'a Value> {
    match cli {
        "pi" => record.get(key),
        "opencode" => opencode_active(record, key).map(|row| &row["value"]),
        _ => None,
    }
}

/// The active row of integration `id` in an `opencode auth export` (an array of credential
/// entries); with no row marked active, the only one.
pub(crate) fn opencode_active<'a>(export: &'a Value, id: &str) -> Option<&'a Value> {
    let rows: Vec<&Value> = export
        .as_array()?
        .iter()
        .filter(|r| r.get("integrationID").and_then(Value::as_str) == Some(id))
        .collect();
    rows.iter()
        .copied()
        .find(|r| r.get("active").and_then(Value::as_bool) == Some(true))
        .or_else(|| (rows.len() == 1).then(|| rows[0]))
}

/// `cli`'s signed-in subscriptions at `now` from the broker's route files and retained record:
/// `(valid, any_present, earliest valid expiry)`. A subscription whose record entry has expired
/// is not valid: its seat never gets that provider's placeholder (decision 7).
fn subscriptions(
    cli: &str,
    files: &[(String, Vec<u8>)],
    now: u64,
) -> Result<
    (
        Vec<crate::seat_credentials::Subscription>,
        bool,
        Option<u64>,
    ),
    String,
> {
    let present = |name: &str| files.iter().any(|(p, b)| p == name && !b.is_empty());
    let record = match retained_record(cli)
        .and_then(|name| files.iter().find(|(p, b)| p == name && !b.is_empty()))
    {
        Some((_, b)) => Some(parse_json(cli, b)?),
        None => None,
    };
    let (mut valid, mut any, mut earliest) = (Vec::new(), false, None::<u64>);
    for route in crate::seat_credentials::SUBSCRIPTION_ROUTES
        .iter()
        .filter(|r| r.cli == cli)
    {
        if !present(route.file) || route.account_file.is_some_and(|a| !present(a)) {
            continue;
        }
        any = true;
        let expiry = match record
            .as_ref()
            .and_then(|r| record_entry(cli, r, route.record_key))
        {
            Some(entry) => pi_entry_expiry(entry)?,
            None => None,
        };
        if expiry.is_some_and(|e| e <= now) {
            continue;
        }
        if let Some(e) = expiry {
            earliest = Some(earliest.map_or(e, |x: u64| x.min(e)));
        }
        valid.push(route.sub);
    }
    Ok((valid, any, earliest))
}

/// Only expiry is projected from retained credentials; no token is put in a status/report.
fn credential_expiry(cli: &str, value: &Value) -> Result<Option<u64>, String> {
    if cli == "codex" && value.get("auth_mode").and_then(Value::as_str) == Some("apikey") {
        return Ok(None);
    }
    if cli == "opencode" {
        let mut earliest = None::<u64>;
        for route in crate::seat_credentials::SUBSCRIPTION_ROUTES
            .iter()
            .filter(|r| r.cli == "opencode")
        {
            if let Some(e) = record_entry(cli, value, route.record_key)
                .filter(|e| e.get("type").and_then(Value::as_str) == Some("oauth"))
                .map(pi_entry_expiry)
                .transpose()?
                .flatten()
            {
                earliest = Some(earliest.map_or(e, |x| x.min(e)));
            }
        }
        return Ok(earliest);
    }
    if cli == "pi" {
        // The earliest of the record's OAuth sign-ins (the next one the refresher is due on).
        let mut earliest = None::<u64>;
        for (id, _) in PI_OAUTH {
            if let Some(entry) = value
                .get(*id)
                .filter(|e| e.get("type").and_then(Value::as_str) == Some("oauth"))
            {
                if let Some(e) = pi_entry_expiry(entry)? {
                    earliest = Some(earliest.map_or(e, |x| x.min(e)));
                }
            }
        }
        return Ok(earliest);
    }
    let (field, divisor) = match cli {
        "claude" => (value.pointer("/claudeAiOauth/expiresAt"), 1000),
        "codex" => (value.pointer("/tokens/expires_at"), 1),
        _ => return Ok(None),
    };
    if let Some(field) = field.filter(|v| !v.is_null()) {
        return field
            .as_u64()
            .map(|n| Some(n / divisor))
            .ok_or_else(|| format!("{cli}: invalid credential expiry"));
    }
    if cli == "codex" {
        // Codex auth.json usually has no expiry field; its access JWT can carry exp. This
        // reports local expiry only, and makes no claim about signature validity or revocation.
        if let Some(token) = nonempty(value.pointer("/tokens/access_token")) {
            let segments: Vec<_> = token.split('.').collect();
            if segments.len() == 3 {
                if let Ok(bytes) = URL_SAFE_NO_PAD.decode(segments[1]) {
                    if let Ok(claims) = serde_json::from_slice::<Value>(&bytes) {
                        if let Some(exp) = claims.get("exp") {
                            return exp
                                .as_u64()
                                .map(Some)
                                .ok_or_else(|| "codex: invalid credential expiry".into());
                        }
                    }
                }
            }
        }
    }
    Ok(None)
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub(crate) struct IngestReport {
    pub(crate) cli: String,
    pub(crate) files: Vec<String>,
    pub(crate) expires_at: Option<u64>,
    /// Providers the login stored that the broker cannot serve yet (by name, never a value).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) skipped: Vec<String>,
    /// The literals the agent's routes must carry: ChatGPT accounts, Copilot hosts.
    #[serde(skip)]
    pub(crate) literals: crate::seat_gateway::RouteLiterals,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "status", content = "detail", rename_all = "snake_case")]
pub(crate) enum BrokerStatus {
    SignedIn {
        expires_at: Option<u64>,
    },
    SignedOut,
    Expired,
    /// Signed in, but the broker's last refresh failed (DES §6: "Refresh failing").
    RefreshFailing {
        expires_at: Option<u64>,
        reason: String,
    },
    Unknown(String),
}

/// `status` folded with the broker's last refresh: a failed one is reported (never its answer).
pub(crate) fn with_last_refresh(
    status: BrokerStatus,
    last: Option<crate::seat_refresh::LastRefresh>,
) -> BrokerStatus {
    match (status, last) {
        (BrokerStatus::SignedIn { expires_at }, Some(l)) if !l.ok => BrokerStatus::RefreshFailing {
            expires_at,
            reason: l.error.unwrap_or_else(|| "the last refresh failed".into()),
        },
        (s, _) => s,
    }
}

/// The OAuth record's expiry in Unix seconds (for the broker's refresher).
pub(crate) fn record_expiry(cli: &str, record: &Value) -> Result<Option<u64>, String> {
    credential_expiry(cli, record)
}

/// The broker's retained OAuth record `file`, or `None` when absent or empty (signed out).
#[cfg(unix)]
pub(crate) fn read_broker_record(io: &dyn BrokerIo, file: &str) -> Result<Option<Vec<u8>>, String> {
    if !io.broker_present()? {
        return Ok(None);
    }
    let files = io.read_broker(&[file], false)?;
    Ok(files
        .into_iter()
        .next()
        .map(|(_, b)| b)
        .filter(|b| !b.is_empty()))
}

/// Pure status parser. Route entries need only nonempty presence markers; retained JSON is
/// read for expiry. Empty route files created at gateway initialization mean signed out.
pub(crate) fn status_from_files(cli: &str, files: &[(String, Vec<u8>)], now: u64) -> BrokerStatus {
    if let Err(e) = broker_files(cli) {
        return BrokerStatus::Unknown(e);
    }
    let present = |name: &str| files.iter().any(|(p, b)| p == name && !b.is_empty());
    if matches!(cli, "pi" | "opencode" | "copilot") {
        let key = match cli {
            "pi" => "pi.token",
            "opencode" => "opencode.token",
            _ => "copilot has no key route",
        };
        return match subscriptions(cli, files, now) {
            Err(e) => BrokerStatus::Unknown(e),
            Ok((valid, any, earliest)) => {
                if present(key) || !valid.is_empty() {
                    BrokerStatus::SignedIn {
                        expires_at: earliest,
                    }
                } else if any {
                    BrokerStatus::Expired
                } else {
                    BrokerStatus::SignedOut
                }
            }
        };
    }
    let archive = match cli {
        "claude" => Some("claude.credentials.json"),
        "codex" => Some("codex.auth.json"),
        _ => None,
    };
    let value = archive.and_then(|name| files.iter().find(|(p, b)| p == name && !b.is_empty()));
    let value = match value {
        Some((_, bytes)) => match parse_json(cli, bytes) {
            Ok(v) => Some(v),
            Err(e) => return BrokerStatus::Unknown(e),
        },
        None => None,
    };
    let codex_mode = value
        .as_ref()
        .and_then(|v| v.get("auth_mode"))
        .and_then(Value::as_str);
    let active = match cli {
        "claude" => present("anthropic.token") || present("anthropic.key"),
        // An API key: no expiry.
        "agy" => present(crate::seat_gateway::GEMINI_KEY_FILE),
        "codex" => match codex_mode {
            Some("apikey") => present("openai.key"),
            Some("chatgpt") => present("openai.token") && present("openai.account"),
            Some(_) => return BrokerStatus::Unknown("codex: unsupported auth_mode".into()),
            None => present("openai.key") || (present("openai.token") && present("openai.account")),
        },
        _ => false,
    };
    if !active {
        return BrokerStatus::SignedOut;
    }
    // An API key does not inherit an obsolete OAuth record's expiry.
    let oauth = match cli {
        "claude" => present("anthropic.token"),
        "codex" => match codex_mode {
            Some("chatgpt") => true,
            Some("apikey") => false,
            _ => present("openai.token") && !present("openai.key"),
        },
        _ => false,
    };
    let expiry = if oauth {
        value
            .as_ref()
            .map(|v| credential_expiry(cli, v))
            .unwrap_or(Ok(None))
    } else {
        Ok(None)
    };
    match expiry {
        Ok(Some(exp)) if exp <= now => BrokerStatus::Expired,
        Ok(expires_at) => BrokerStatus::SignedIn { expires_at },
        Err(e) => BrokerStatus::Unknown(e),
    }
}

/// The transport the sign-in state machine drives (k8s-swap B-4): the broker's storage, the
/// gateway's publication, the login stores and the account reads. The state machine (journals,
/// tombstones, sessions and their repair order) is one copy over this seam; the cluster's
/// transport is [`crate::k8s::login::K8sIo`], and the test-only Docker transport is kept
/// only for the legacy tests pending their port to MemIo.
///
/// The mutating methods are called with the broker's mutation lock held; a status read
/// (`broker_status`) and [`BrokerIo::prepare_login`] run without it.
#[cfg(unix)]
pub(crate) trait BrokerIo {
    /// Whether the broker's storage exists (none = nothing was ever signed in).
    fn broker_present(&self) -> Result<bool, String>;
    /// Provision the broker's storage on a first sign-in, never taking the daemon's ownership.
    fn init_broker(&self, dir: &Path) -> Result<(), String>;
    /// Read broker `paths` (a missing file is skipped). `presence`: a file that is not a
    /// retained record ([`is_archive`]) is read as a marker only (nonempty = present).
    fn read_broker(&self, paths: &[&str], presence: bool)
        -> Result<Vec<(String, Vec<u8>)>, String>;
    /// Replace broker `file` with `body`, whole or not at all.
    fn write_broker(&self, dir: &Path, cli: &str, file: &str, body: &[u8]) -> Result<(), String>;
    /// Empty broker `files` (signed out: a route names them, so they are never deleted).
    fn empty_broker(&self, dir: &Path, cli: &str, files: &[&str]) -> Result<(), String>;
    /// Remove broker `file` (journals and staged records only).
    fn remove_broker(&self, dir: &Path, cli: &str, file: &str) -> Result<(), String>;
    /// Settle what a dead owner's mutation left running.
    fn cleanup(&self, dir: &Path) -> Result<(), String>;
    /// Point `cli`'s routes at `literals` and publish the config.
    fn route_sign_in(
        &self,
        dir: &Path,
        cli: &str,
        literals: &crate::seat_gateway::RouteLiterals,
    ) -> Result<(), String>;
    /// Clear `cli`'s route literals and confirm the gateway applied the generation.
    fn confirm_sign_out(&self, dir: &Path, cli: &str) -> Result<(), String>;
    /// Whether login session `session`'s store (or its login) still exists.
    fn login_exists(&self, session: &str) -> Result<bool, String>;
    /// Create login session `session`'s store (empty), before its marker is written.
    fn create_login(&self, session: &str) -> Result<(), String>;
    /// Make `cli`'s configuration homes in the store (after the marker is written).
    fn init_login(&self, dir: &Path, cli: &str, session: &str) -> Result<(), String>;
    /// Everything a login of `cli` needs that may take long or must hold first (an image build, the
    /// cluster's isolation proof), done BEFORE the session transaction (review B-4 #3, #4).
    fn prepare_login(&self, _cli: &str) -> Result<(), String> {
        Ok(())
    }
    /// Start session `session`'s login and render the argv crew runs in its terminal (the caller
    /// holds the mutation lock, so no sign-out or newer sign-in can supersede it meanwhile).
    fn open_login(&self, cli: &str, session: &str) -> Result<Vec<String>, String>;
    /// Read credential files from the login store.
    fn read_login(&self, session: &str, paths: &[&str]) -> Result<Vec<(String, Vec<u8>)>, String>;
    /// End session `session`'s login (its terminal is over), keeping the store.
    fn stop_login(&self, session: &str) -> Result<(), String>;
    /// End the login and remove the store; one already gone counts as removed.
    fn remove_login(&self, session: &str) -> Result<(), String>;
    /// Make an account read from the gateway's network; the token rides stdin.
    fn send(
        &self,
        dir: &Path,
        cli: &str,
        request: &crate::seat_refresh::Request,
    ) -> Result<Value, String>;
}

/// Test transport only (k8s-final B-6): the state-machine tests below still drive the sign-in flow through it; port them to MemIo (src/seat_login_io_tests.rs) and delete it.
#[cfg(unix)]
#[derive(Clone)]
#[cfg(test)]
pub(crate) struct DockerIo {
    pub(crate) rt: Runtime,
    pub(crate) gw: Gateway,
}

#[cfg(unix)]
#[cfg(test)]
impl DockerIo {
    pub(crate) fn new(rt: &Runtime, gw: &Gateway) -> Self {
        Self {
            rt: rt.clone(),
            gw: gw.clone(),
        }
    }
}

#[cfg(unix)]
#[cfg(test)]
impl BrokerIo for DockerIo {
    fn broker_present(&self) -> Result<bool, String> {
        volume_exists(&self.rt, &volumes(&self.gw).1)
    }
    fn init_broker(&self, dir: &Path) -> Result<(), String> {
        crate::seat_gateway::init_gateway_volumes(&self.rt, &self.gw, dir)
    }
    fn read_broker(
        &self,
        paths: &[&str],
        presence: bool,
    ) -> Result<Vec<(String, Vec<u8>)>, String> {
        read_volume(&self.rt, &volumes(&self.gw).1, paths, presence)
    }
    fn write_broker(&self, dir: &Path, cli: &str, file: &str, body: &[u8]) -> Result<(), String> {
        crate::seat_gateway::set_broker_credential(&self.rt, &self.gw, dir, cli, file, body)
    }
    fn empty_broker(&self, dir: &Path, cli: &str, files: &[&str]) -> Result<(), String> {
        crate::seat_gateway::mutation_helper(
            &self.rt,
            dir,
            cli,
            &revoke_argv(&volumes(&self.gw).1, files),
            &[],
            TIMEOUT,
        )
        .map(|_| ())
    }
    fn remove_broker(&self, dir: &Path, cli: &str, file: &str) -> Result<(), String> {
        crate::seat_gateway::remove_broker_file(&self.rt, &self.gw, dir, cli, file)
    }
    fn cleanup(&self, dir: &Path) -> Result<(), String> {
        crate::seat_gateway::cleanup_mutations(&self.rt, dir)
    }
    fn route_sign_in(
        &self,
        dir: &Path,
        cli: &str,
        literals: &crate::seat_gateway::RouteLiterals,
    ) -> Result<(), String> {
        crate::seat_gateway::route_sign_in(&self.rt, &self.gw, dir, cli, literals)
    }
    fn confirm_sign_out(&self, dir: &Path, cli: &str) -> Result<(), String> {
        crate::seat_gateway::confirm_sign_out(&self.rt, &self.gw, dir, cli)
    }
    fn login_exists(&self, session: &str) -> Result<bool, String> {
        volume_exists(&self.rt, session)
    }
    fn create_login(&self, session: &str) -> Result<(), String> {
        checked(
            &self.rt,
            &["volume".into(), "create".into(), session.into()],
        )
    }
    fn init_login(&self, dir: &Path, cli: &str, session: &str) -> Result<(), String> {
        crate::seat_gateway::mutation_helper(
            &self.rt,
            dir,
            cli,
            &init_login_argv(session, cli),
            &[],
            TIMEOUT,
        )
        .map(|_| ())
    }
    fn open_login(&self, _cli: &str, _session: &str) -> Result<Vec<String>, String> {
        Err("the test transport does not launch sign-in processes".into())
    }
    fn read_login(&self, session: &str, paths: &[&str]) -> Result<Vec<(String, Vec<u8>)>, String> {
        read_volume(&self.rt, session, paths, false)
    }
    fn stop_login(&self, session: &str) -> Result<(), String> {
        stop_login_container(&self.rt, session)
    }
    fn remove_login(&self, session: &str) -> Result<(), String> {
        remove_login_volume(&self.rt, session)
    }
    fn send(
        &self,
        dir: &Path,
        cli: &str,
        request: &crate::seat_refresh::Request,
    ) -> Result<Value, String> {
        crate::seat_refresh::send(&self.rt, dir, cli, &self.gw.external_network, request)
    }
}

/// The daemon's gateway name for gateway dir `dir` (session and renewal-home names carry it).
pub(crate) fn gw_name(dir: &Path) -> String {
    crate::seat_gateway::gateway_name(dir)
}

/// Helper argv is pure; scripts and paths are fixed by this module, never supplied by the CLI.
#[cfg(test)]
fn helper_argv(volume: &str, readonly: bool, user: &str, script: String) -> Vec<String> {
    let mut argv: Vec<String> = [
        "run",
        "--rm",
        "--log-driver",
        "none",
        "--network",
        "none",
        "--user",
        user,
        "--read-only",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    if user == "0" {
        argv.extend([
            "--cap-add".into(),
            "CHOWN".into(),
            "--cap-add".into(),
            "FOWNER".into(),
        ]);
    }
    argv.extend([
        "-v".into(),
        format!("{volume}:/in{}", if readonly { ":ro" } else { "" }),
        crate::seat_gateway::HELPER_IMAGE.into(),
        "sh".into(),
        "-c".into(),
        script,
    ]);
    argv
}

#[cfg(test)]
fn read_argv(volume: &str, path: &str, presence_only: bool) -> Vec<String> {
    let mut script = String::from("set -e;");
    let mut prefix = String::from("/in");
    for part in path.split('/') {
        prefix.push('/');
        prefix.push_str(part);
        script.push_str(&format!(" [ ! -L {prefix} ] || exit 46;"));
    }
    script.push_str(&format!(
        " [ -e /in/{path} ] || exit 44; [ -f /in/{path} ] && [ -r /in/{path} ] || exit 45;"
    ));
    if presence_only {
        script.push_str(&format!(" if [ -s /in/{path} ]; then printf present; fi"));
    } else {
        script.push_str(&format!(" head -c {} /in/{path}", MAX_FILE_BYTES + 1));
    }
    helper_argv(volume, true, "10001", script)
}

/// Revoke by emptying, never deleting: the gateway's routes reference these files, and a
/// reload that names a missing file is refused, leaving the OLD credential served (found by the
/// live sign-out test). An empty file is the signed-out state the gateway starts with.
#[cfg(test)]
fn revoke_argv(volume: &str, files: &[&str]) -> Vec<String> {
    helper_argv(
        volume,
        false,
        "10001",
        files
            .iter()
            .map(|f| format!(": > /in/{f}"))
            .collect::<Vec<_>>()
            .join(" && "),
    )
}

/// Own the volume as the login user and create the CLI's configuration homes ([`login_homes`]).
/// The helper has CHOWN and FOWNER, never DAC_OVERRIDE or DAC_READ_SEARCH: the homes are made
/// while root still owns the volume, and the volume root is handed over LAST, since root can no
/// longer reach a child once `/in` is another user's 0700 directory.
#[cfg(test)]
fn init_login_argv(volume: &str, cli: &str) -> Vec<String> {
    let dirs: Vec<String> = login_homes(cli)
        .iter()
        .map(|(_, dir)| format!("/in/{dir}"))
        .chain(std::iter::once("/in".to_string()))
        .collect();
    let dirs = dirs.join(" ");
    let seed = match cli {
        // copilot stores its token in the system keychain, and with none (a container) only
        // after the user consents to plaintext; the throwaway volume's settings consent up
        // front, so the login writes `copilot/config.json`, which ingest moves to the broker.
        "copilot" => format!(
            " && printf '%s' '{COPILOT_SETTINGS}' > /in/copilot/settings.json \
             && chown 10001 /in/copilot/settings.json"
        ),
        _ => String::new(),
    };
    helper_argv(
        volume,
        false,
        "0",
        format!("mkdir -p {dirs}{seed} && chown 10001 {dirs} && chmod 700 {dirs}"),
    )
}

/// The copilot settings its login volume is seeded with (`storeTokenPlaintext`, see above).
pub(crate) const COPILOT_SETTINGS: &str = r#"{"storeTokenPlaintext":true}"#;

#[cfg(unix)]
#[cfg(test)]
fn execute(rt: &Runtime, argv: &[String]) -> Result<std::process::Output, String> {
    let mut cmd = rt.cmd();
    cmd.args(argv);
    output_within(cmd, rt.bound(TIMEOUT))
        .ok_or_else(|| "login runtime command could not start or timed out".into())
}

#[cfg(unix)]
#[cfg(test)]
fn checked(rt: &Runtime, argv: &[String]) -> Result<(), String> {
    let out = execute(rt, argv)?;
    if out.status.success() {
        Ok(())
    } else {
        // Runtime/helper output may contain credentials. Return only the exit status.
        Err(format!("login runtime command failed ({})", out.status))
    }
}

#[cfg(unix)]
#[cfg(test)]
fn volume_exists(rt: &Runtime, volume: &str) -> Result<bool, String> {
    let out = execute(
        rt,
        &[
            "volume".into(),
            "ls".into(),
            "--format".into(),
            "{{.Name}}".into(),
        ],
    )?;
    if !out.status.success() {
        return Err("could not list login/broker volumes".into());
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|name| name == volume))
}

/// Broker files read whole by a status reading (retained OAuth records and route literals);
/// every other broker file is read as a presence marker only.
pub(crate) fn is_archive(name: &str) -> bool {
    matches!(
        name,
        "claude.credentials.json"
            | "codex.auth.json"
            | "pi.auth.json"
            | "opencode.credentials.json"
    )
}

#[cfg(unix)]
#[cfg(test)]
fn read_volume(
    rt: &Runtime,
    volume: &str,
    paths: &[&str],
    broker: bool,
) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut files = Vec::new();
    for path in paths {
        let archive = is_archive(path);
        let out = execute(rt, &read_argv(volume, path, broker && !archive))?;
        if out.status.code() == Some(44) {
            continue;
        }
        if !out.status.success() {
            return Err(format!("could not read credential file {path}"));
        }
        if out.stdout.len() > MAX_FILE_BYTES {
            return Err(format!("credential file {path} exceeds size limit"));
        }
        files.push(((*path).into(), out.stdout));
    }
    Ok(files)
}

/// Extract everything before changing the broker; remove the login volume only after every
/// write succeeds. Failure retains the login volume for a retry. Writes are individually atomic
/// through set_broker_credential, but the runtime offers no multi-file transaction here.
#[cfg(unix)]
fn ingest(io: &dyn BrokerIo, dir: &Path, cli: &str, volume: &str) -> Result<IngestReport, String> {
    let owned = broker_files(cli)?;
    if !login_exists(io, dir, cli, volume)? {
        return Err(missing(
            cli,
            if host_login(cli) {
                "login home"
            } else {
                "login volume"
            },
        ));
    }
    // A host-login agent's session from before the upgrade is still a login volume.
    let from_home = host_login(cli) && std::fs::symlink_metadata(login_home(dir, volume)).is_ok();
    let read = if from_home {
        read_home(&login_home(dir, volume), credential_paths(cli))
    } else {
        io.read_login(volume, credential_paths(cli))
    };
    let (input, tokens) = match read.and_then(|input| {
        let tokens = extract_tokens(cli, &input)?;
        Ok((input, tokens))
    }) {
        Ok(read) => read,
        Err(e) => {
            // A host login that left nothing usable (cancelled, failed, or another shape) is
            // over: its throwaway home goes now, before any broker change, and the operator
            // signs in again. A login container's volume is kept for a retry, as before.
            if from_home {
                remove_login_home(dir, volume)?;
                crate::seat_refresh::remove_private_file(&session_path(dir, cli))?;
            }
            return Err(e);
        }
    };
    if !io.broker_present()? {
        // codex lg-r3 R3: first sign-in provisions storage without taking daemon ownership.
        io.init_broker(dir)?;
    }
    let skipped = skipped_providers(cli, &input);
    // codex lg-r2 C4: journal before any broker mutation, including stale-mode clearing.
    std::fs::create_dir_all(dir.join("refresh")).map_err(|e| e.to_string())?;
    let mut marker = std::fs::File::create(crate::seat_refresh::pending_path(dir, cli))
        .map_err(|e| e.to_string())?;
    use std::io::Write;
    marker
        .write_all(b"sign-in incomplete")
        .and_then(|()| marker.sync_all())
        .map_err(|e| e.to_string())?;
    let journal: Vec<_> = tokens
        .iter()
        .map(|(n, b)| (n, String::from_utf8_lossy(b).into_owned()))
        .collect();
    let staged = serde_json::to_vec(&journal).map_err(|_| "could not encode ingest journal")?;
    io.write_broker(dir, cli, &format!("{cli}.ingest.next"), &staged)?;
    publish_ingest(io, dir, cli, volume, &tokens, owned)?;
    broker_report(io, cli).map(|report| IngestReport { skipped, ..report })
}

#[cfg(unix)]
fn publish_ingest(
    io: &dyn BrokerIo,
    dir: &Path,
    cli: &str,
    volume: &str,
    tokens: &[(String, Vec<u8>)],
    owned: &[&str],
) -> Result<(), String> {
    for (name, bytes) in tokens {
        io.write_broker(dir, cli, name, bytes)?;
    }
    let stale: Vec<&str> = owned
        .iter()
        .copied()
        .filter(|f| !tokens.iter().any(|(n, _)| n == f))
        .collect();
    if !stale.is_empty() {
        io.empty_broker(dir, cli, &stale)?;
    }
    // codex lg-r2 C3: a retry must never replay this now-obsolete login generation.
    write_session_state(dir, cli, volume, true)
}

#[cfg(unix)]
fn broker_report(io: &dyn BrokerIo, cli: &str) -> Result<IngestReport, String> {
    let files = io.read_broker(broker_files(cli)?, false)?;
    let mut expires_at = None;
    for (name, bytes) in &files {
        if !bytes.is_empty() && is_archive(name) {
            expires_at = credential_expiry(cli, &parse_json(cli, bytes)?)?;
        }
    }
    let literals = route_literals(cli, &files)?;
    Ok(IngestReport {
        cli: cli.into(),
        files: files
            .iter()
            .filter(|(_, b)| !b.is_empty())
            .map(|(n, _)| n.clone())
            .collect(),
        expires_at,
        skipped: Vec::new(),
        literals,
    })
}

/// The route literals `cli`'s broker files name (pure): codex's ChatGPT account, and each
/// subscription route's account and Copilot host. Validated before they reach the gateway.
pub(crate) fn route_literals(
    cli: &str,
    files: &[(String, Vec<u8>)],
) -> Result<crate::seat_gateway::RouteLiterals, String> {
    let read = |name: &str| {
        files
            .iter()
            .find(|(n, b)| n == name && !b.is_empty())
            .map(|(_, b)| String::from_utf8_lossy(b).trim().to_string())
    };
    let mut literals = crate::seat_gateway::RouteLiterals::default();
    if cli == "codex" {
        if let Some(account) = read("openai.account") {
            literals.accounts.push(("openai.token".into(), account));
        }
    }
    for route in crate::seat_credentials::SUBSCRIPTION_ROUTES
        .iter()
        .filter(|r| r.cli == cli)
    {
        if let Some(account) = route.account_file.and_then(read) {
            literals.accounts.push((route.file.into(), account));
        }
        if let Some(host) = route.host_file.and_then(read) {
            literals.hosts.push((route.file.into(), host));
        }
    }
    if cli == "copilot" {
        let models = files
            .iter()
            .find(|(n, b)| n == crate::copilot_account::MODELS_FILE && !b.is_empty());
        if let Some((_, bytes)) = models {
            let account = crate::copilot_account::parse_models_file(bytes)
                .ok_or("copilot: the stored Copilot model list is unreadable")?;
            literals
                .models
                .push((crate::seat_gateway::COPILOT_CLI_FILE.into(), account));
        }
    }
    literals.validate(cli)?;
    Ok(literals)
}

/// Caller holds the broker lock. Repair sign-in before refresh checks expiry (codex lg-r2 C4).
#[cfg(unix)]
pub(crate) fn repair_ingest(io: &dyn BrokerIo, dir: &Path, cli: &str) -> Result<bool, String> {
    if complete_sign_out(io, dir, cli)? {
        return Ok(false);
    }
    if !signin_pending(dir, cli) {
        return Ok(false);
    }
    let volume = read_session(dir, cli)?.ok_or("incomplete sign-in has no session")?;
    if !session_committed(dir, cli)? {
        let Some(staged) = read_broker_record(io, &format!("{cli}.ingest.next"))? else {
            return Ok(false);
        };
        let journal: Vec<(String, String)> =
            serde_json::from_slice(&staged).map_err(|_| "invalid ingest journal")?;
        let tokens: Vec<_> = journal
            .into_iter()
            .map(|(n, b)| (n, b.into_bytes()))
            .collect();
        let owned = broker_files(cli)?;
        if tokens.iter().any(|(n, _)| !owned.contains(&n.as_str())) {
            return Err("invalid ingest journal file".into());
        }
        publish_ingest(io, dir, cli, &volume, &tokens, owned)?;
    }
    let report = broker_report(io, cli)?;
    io.route_sign_in(dir, cli, &report.literals)?;
    io.remove_broker(dir, cli, &format!("{cli}.ingest.next"))?;
    crate::seat_refresh::clear_pending(dir, cli)?;
    crate::seat_refresh::record_last(dir, cli, None);
    Ok(true)
}

pub(crate) fn signin_pending(dir: &Path, cli: &str) -> bool {
    std::fs::read(crate::seat_refresh::pending_path(dir, cli))
        .is_ok_and(|b| b == b"sign-in incomplete")
}

pub(crate) fn publication_status(dir: &Path, cli: &str, status: BrokerStatus) -> BrokerStatus {
    if signout_path(dir, cli).exists() {
        BrokerStatus::Unknown("sign-out incomplete".into())
    } else if crate::seat_gateway::unsettled_path(dir, cli).exists()
        || crate::seat_gateway::unsettled_path(dir, "broker").exists()
        || crate::seat_gateway::unsettled_path(dir, "config").exists()
    {
        BrokerStatus::Unknown("mutation helper cleanup incomplete".into())
    } else if signin_pending(dir, cli) {
        BrokerStatus::Unknown("sign-in incomplete".into())
    } else if crate::seat_refresh::pending_path(dir, cli).exists() {
        BrokerStatus::Unknown("credential publication pending".into())
    } else {
        status
    }
}

#[cfg(unix)]
pub(crate) fn broker_status(io: &dyn BrokerIo, cli: &str) -> BrokerStatus {
    broker_reading(io, cli).0
}

/// The broker's status for `cli` and the route files it was read from (presence markers, plus
/// the retained OAuth record) — one read for both the status and the seat's credential kind.
#[cfg(unix)]
fn broker_reading(io: &dyn BrokerIo, cli: &str) -> (BrokerStatus, Vec<(String, Vec<u8>)>) {
    let paths = match broker_files(cli) {
        Ok(paths) => paths,
        Err(e) => return (BrokerStatus::Unknown(e), Vec::new()),
    };
    match io.broker_present() {
        Ok(false) => return (BrokerStatus::SignedOut, Vec::new()),
        Err(e) => return (BrokerStatus::Unknown(e), Vec::new()),
        Ok(true) => {}
    }
    let files = match io.read_broker(paths, true) {
        Ok(files) => files,
        Err(e) => return (BrokerStatus::Unknown(e), Vec::new()),
    };
    let now = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(now) => now.as_secs(),
        Err(_) => {
            return (
                BrokerStatus::Unknown("system clock precedes Unix epoch".into()),
                files,
            )
        }
    };
    (status_from_files(cli, &files, now), files)
}

/// The credential a signed-in agent's broker holds, as the kind its seats must present: the
/// seat's placeholder takes that shape and its requests ride that credential's gateway route.
/// Pure over the broker's route files (nonempty = present, as [`status_from_files`] reads them);
/// `None` when no usable credential is present.
pub(crate) fn kind_from_files(
    cli: &str,
    files: &[(String, Vec<u8>)],
) -> Option<crate::seat_credentials::CredentialKind> {
    use crate::seat_credentials::CredentialKind;
    let present = |name: &str| files.iter().any(|(p, b)| p == name && !b.is_empty());
    match cli {
        // An OAuth sign-in wins, as the status's expiry reading does.
        "claude" if present("anthropic.token") => Some(CredentialKind::ClaudeOauth),
        "claude" if present("anthropic.key") => Some(CredentialKind::ClaudeApiKey),
        "codex" => {
            let mode = files
                .iter()
                .find(|(p, b)| p == "codex.auth.json" && !b.is_empty())
                .and_then(|(_, b)| serde_json::from_slice::<Value>(b).ok())
                .and_then(|v| {
                    v.get("auth_mode")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                });
            let chatgpt = present("openai.token") && present("openai.account");
            match mode.as_deref() {
                Some("apikey") if present("openai.key") => Some(CredentialKind::CodexApiKey),
                Some("chatgpt") if chatgpt => Some(CredentialKind::CodexChatgpt),
                None if present("openai.key") => Some(CredentialKind::CodexApiKey),
                None if chatgpt => Some(CredentialKind::CodexChatgpt),
                _ => None,
            }
        }
        "pi" if present("pi.token") => Some(CredentialKind::ProviderKey),
        "opencode" if present("opencode.token") => Some(CredentialKind::ProviderKey),
        "agy" if present(crate::seat_gateway::GEMINI_KEY_FILE) => {
            Some(CredentialKind::GeminiApiKey)
        }
        _ => None,
    }
}

/// What a signed-in agent's seat is armed with at `now` (pure over the broker's route files and
/// retained records): its native kind ([`kind_from_files`]) and every valid subscription. `None`
/// when neither is usable.
pub(crate) fn sign_in_from_files(
    cli: &str,
    files: &[(String, Vec<u8>)],
    now: u64,
) -> Option<crate::seat_credentials::SeatSignIn> {
    let kind = kind_from_files(cli, files);
    let subscriptions = match cli {
        "pi" | "opencode" | "copilot" => subscriptions(cli, files, now).ok()?.0,
        _ => Vec::new(),
    };
    (kind.is_some() || !subscriptions.is_empty()).then_some(crate::seat_credentials::SeatSignIn {
        kind,
        subscriptions,
    })
}

/// The sign-in a governed seat of `cli` runs on (DES-CONTAINER-FLOOR §6): the kind of the
/// credential the user signed in with in the agent's login container, read from the broker of
/// the daemon whose gateway dir is `dir`. Signed out, expired, mid-publication or unreadable =
/// the reason, and the unit is refused before its seat starts (never launched to fail upstream).
/// A failed refresh of a still-valid credential still runs (the status says so on the tab).
#[cfg(unix)]
pub(crate) fn seat_sign_in(
    io: &dyn BrokerIo,
    dir: &Path,
    cli: &str,
) -> Result<crate::seat_credentials::SeatSignIn, String> {
    // codex c6-r1: the broker's files are read one helper at a time; holding the mutation lock that
    // ingest, sign-out and refresh hold means the snapshot is never torn by one of them.
    let _lock = crate::seat_refresh::mutation_lock(dir)?;
    // agy c6-r1: an interrupted sign-in, sign-out or refresh is finished first, as ingest does,
    // so a crash mid-publication heals on the next unit instead of refusing every one after it.
    settle_broker(io, dir, cli)
        .map_err(|e| format!("{cli}'s sign-in could not be settled: {e}"))?;
    let (status, files) = broker_reading(io, cli);
    let status = publication_status(
        dir,
        cli,
        with_last_refresh(status, crate::seat_refresh::last_refresh(dir, cli)),
    );
    match status {
        BrokerStatus::SignedIn { .. } | BrokerStatus::RefreshFailing { .. } => {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| "system clock precedes Unix epoch")?
                .as_secs();
            sign_in_from_files(cli, &files, now)
                .ok_or_else(|| format!("{cli}: the broker holds no usable sign-in"))
        }
        BrokerStatus::SignedOut => Err(format!("{cli} is not signed in")),
        BrokerStatus::Expired => Err(format!("{cli}'s sign-in has expired")),
        BrokerStatus::Unknown(why) => Err(format!("{cli}'s sign-in could not be read: {why}")),
    }
}

/// The token a claude seat signed in with OAuth passes through (core#1024): the operator's own
/// `claude setup-token` token (or what an earlier `claude auth login` left, until it expires),
/// read from the broker under its lock. The seat's claude sends it upstream itself; the gateway
/// relays it unchanged.
#[cfg(unix)]
pub(crate) fn claude_seat_token(io: &dyn BrokerIo, dir: &Path) -> Result<String, String> {
    claude_broker_token(io, dir)?.ok_or_else(|| "claude is not signed in".into())
}

/// [`claude_seat_token`], with signed out as `Ok(None)`: the seat-secret guard asks the broker
/// which token a delivery must not carry (`seat_secret_guard::from_broker`).
#[cfg(unix)]
pub(crate) fn claude_broker_token(io: &dyn BrokerIo, dir: &Path) -> Result<Option<String>, String> {
    let _lock = crate::seat_refresh::mutation_lock(dir)?;
    let Some(bytes) = read_broker_record(io, "anthropic.token")? else {
        return Ok(None);
    };
    let token = String::from_utf8(bytes).map_err(|_| "claude: invalid broker token")?;
    let token = token.trim();
    if token.is_empty() || token.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("claude: invalid broker token".into());
    }
    Ok(Some(token.to_string()))
}

/// Finish whatever broker mutation of `cli` a crash interrupted (caller holds the mutation lock):
/// a helper's cleanup, a sign-out tombstone, an ingest journal, a refresh's staged record. The
/// same order `ingest_and_route` repairs in.
#[cfg(unix)]
fn settle_broker(io: &dyn BrokerIo, dir: &Path, cli: &str) -> Result<(), String> {
    io.cleanup(dir)?;
    complete_sign_out(io, dir, cli)?;
    if signin_pending(dir, cli) {
        repair_ingest(io, dir, cli)?;
    } else {
        crate::seat_refresh::repair_refresh(io, dir, cli)?;
    }
    // codex c6-r3: a published sign-in whose cleanup a crash interrupted still has its login
    // volume (a copy of the credential) and session marker; finish them as ingest_and_route does.
    // An uncommitted session is a login that may still be open: it is never touched here.
    if let Some(volume) = read_session(dir, cli)? {
        if session_committed(dir, cli)? {
            discard_login(io, dir, cli, &volume)?;
            crate::seat_refresh::remove_private_file(&session_path(dir, cli))?;
        }
    }
    Ok(())
}

/// Ingest, then point the gateway's routes at the new sign-in: codex's ChatGPT route carries the
/// account id as a literal header, so the served config is re-rendered and published.
#[cfg(unix)]
pub(crate) fn ingest_and_route(
    io: &dyn BrokerIo,
    dir: &Path,
    cli: &str,
) -> Result<IngestReport, String> {
    // codex lg-r1 #2: hold the same broker lock from reading through publication.
    let _lock = crate::seat_refresh::mutation_lock(dir)?;
    io.cleanup(dir)?;
    complete_sign_out(io, dir, cli)?;
    let volume = read_session(dir, cli)?.ok_or_else(|| missing(cli, "login session"))?;
    // The terminal has ended: its container is stopped before the volume is read, so whatever the
    // CLI wrote is final and nothing still holds the volume when it is removed. A host login's
    // process group was ended with its terminal.
    halt_login(io, dir, cli, &volume)?;
    repair_ingest(io, dir, cli)?;
    // codex lg-r3 R2: rescue or staging can survive even a failed pending-marker write.
    if !signin_pending(dir, cli) {
        crate::seat_refresh::repair_refresh(io, dir, cli)?;
    }
    let imported = !session_committed(dir, cli)?;
    let mut report = if imported {
        ingest(io, dir, cli, &volume)?
    } else {
        broker_report(io, cli)?
    };
    // copilot: the account's Copilot API host and the models it may use, read with the new
    // token (DES-SEAT-LOGIN-SUBSCRIPTIONS OD-1). A failed read does not undo the sign-in: it is
    // shown on the tab, its units are refused by name, and the refresh pass retries it.
    let mut account_error = None;
    if cli == "copilot" {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        match crate::seat_refresh::resolve_copilot(io, dir, now) {
            Ok(_) => report.literals = broker_report(io, cli)?.literals,
            Err(e) => account_error = Some((now, format!("reading the Copilot account: {e}"))),
        }
    }
    io.route_sign_in(dir, cli, &report.literals)?;
    io.remove_broker(dir, cli, &format!("{cli}.ingest.next"))?;
    crate::seat_refresh::clear_pending(dir, cli)?;
    // codex lg-r1 #8: the retry source survives every write/publish failure. codex lg-r4: the
    // session marker is the only record of the credential-bearing login volume, so it goes
    // LAST, after the volume is confirmed gone (a retry of a committed session whose volume is
    // already absent just finishes here).
    write_session_state(dir, cli, &volume, true)?;
    discard_login(io, dir, cli, &volume)?;
    match std::fs::remove_file(session_path(dir, cli)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.to_string()),
    }
    if let Some((at, error)) = account_error {
        crate::seat_refresh::record_last(
            dir,
            cli,
            Some(&crate::seat_refresh::LastRefresh {
                ok: false,
                at,
                error: Some(error),
            }),
        );
    } else if imported {
        crate::seat_refresh::record_last(dir, cli, None);
    }
    // codex lg-r3 R4: only a committed sign-in may supersede a sign-out intent.
    crate::seat_refresh::remove_private_file(&signout_path(dir, cli))?;
    Ok(report)
}

/// Sign out, then clear what the routes carried for this agent (codex's account header).
#[cfg(unix)]
pub(crate) fn sign_out_and_route(io: &dyn BrokerIo, dir: &Path, cli: &str) -> Result<(), String> {
    let _lock = crate::seat_refresh::mutation_lock(dir)?;
    io.cleanup(dir)?;
    broker_files(cli)?;
    let tombstone = signout_path(dir, cli);
    // codex lg-r3 R4: a crash after the first empty write must never authorize recovery.
    if !tombstone.try_exists().map_err(|e| e.to_string())? {
        crate::seat_refresh::create_private_file(&tombstone, b"sign-out")?;
    }
    complete_sign_out(io, dir, cli).map(|_| ())
}

fn signout_path(dir: &Path, cli: &str) -> std::path::PathBuf {
    dir.join("refresh").join(format!("{cli}.signout"))
}

/// Caller holds the mutation lock. Tombstones always win over publication journals.
#[cfg(unix)]
pub(crate) fn complete_sign_out(io: &dyn BrokerIo, dir: &Path, cli: &str) -> Result<bool, String> {
    let tombstone = signout_path(dir, cli);
    if !tombstone.try_exists().map_err(|e| e.to_string())? {
        return Ok(false);
    }
    // codex lg-r3 R4: retry even a prior tombstone creation whose fsync failed.
    crate::seat_refresh::sync_private_file(&tombstone)?;
    sign_out(io, dir, cli)?;
    if io.broker_present()? {
        for name in [
            format!("{cli}.ingest.next"),
            format!(
                "{}.next",
                crate::seat_refresh::record_file(cli).unwrap_or(cli)
            ),
        ] {
            io.remove_broker(dir, cli, &name)?;
        }
    }
    if let Some(volume) = read_session(dir, cli)? {
        // codex lg-r3 R4: retry a crash between removal of the volume and its session marker.
        if host_login(cli) {
            discard_login(io, dir, cli, &volume)?;
        } else if io.login_exists(&volume)? {
            io.remove_login(&volume)?;
        } else {
            io.stop_login(&volume)?;
        }
        crate::seat_refresh::remove_private_file(&session_path(dir, cli))?;
    }
    crate::seat_refresh::remove_private_file(&crate::seat_refresh::rescue_path(dir, cli))?;
    crate::seat_refresh::clear_pending(dir, cli)?;
    io.confirm_sign_out(dir, cli)?;
    crate::seat_refresh::record_last(dir, cli, None);
    crate::seat_refresh::remove_private_file(&tombstone)?;
    Ok(true)
}

#[cfg(unix)]
fn sign_out(io: &dyn BrokerIo, dir: &Path, cli: &str) -> Result<(), String> {
    let files = broker_files(cli)?;
    if !io.broker_present()? {
        return Ok(());
    }
    io.empty_broker(dir, cli, files)
}

fn session_path(dir: &Path, cli: &str) -> std::path::PathBuf {
    dir.join("login").join(format!("{cli}.session"))
}

// codex lg-r1 #10: never infer a volume from a CLI; ingest exactly the recorded session.
fn read_session(dir: &Path, cli: &str) -> Result<Option<String>, String> {
    let name = match std::fs::read_to_string(session_path(dir, cli)) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    let (name, state) = name.split_once('\n').unwrap_or((&name, ""));
    if !matches!(state, "" | "committed") {
        return Err("invalid login session state".into());
    }
    let prefix = format!("{}-login-{cli}-", gw_name(dir));
    if !name
        .strip_prefix(&prefix)
        .is_some_and(|n| n.len() == 8 && n.bytes().all(|c| c.is_ascii_hexdigit()))
    {
        return Err("invalid login session name".into());
    }
    Ok(Some(name.to_string()))
}

#[cfg(unix)]
fn new_session(io: &dyn BrokerIo, dir: &Path, cli: &str) -> Result<String, String> {
    let _lock = crate::seat_refresh::mutation_lock(dir)?;
    new_session_locked(io, dir, cli)
}

/// [`new_session`] for a caller that holds the mutation lock.
#[cfg(unix)]
fn new_session_locked(io: &dyn BrokerIo, dir: &Path, cli: &str) -> Result<String, String> {
    io.cleanup(dir)?;
    complete_sign_out(io, dir, cli)?;
    if signin_pending(dir, cli) && !repair_ingest(io, dir, cli)? {
        return Err("sign-in incomplete; retry ingest first".into());
    }
    let previous = read_session(dir, cli)?;
    if let Some(old) = &previous {
        discard_login(io, dir, cli, old)?;
        std::fs::remove_file(session_path(dir, cli)).map_err(|e| e.to_string())?;
    }
    if host_login(cli) {
        // A home whose session marker never got written (a crash between the two) is swept too.
        sweep_login_homes(dir, cli)?;
    }
    let volume = loop {
        let candidate = login_volume(&gw_name(dir), cli);
        if previous.as_ref() != Some(&candidate) && !login_exists(io, dir, cli, &candidate)? {
            break candidate;
        }
    };
    if host_login(cli) {
        create_login_home(dir, cli, &volume)?;
        write_session(dir, cli, &volume)?;
        return Ok(volume);
    }
    io.create_login(&volume)?;
    write_session(dir, cli, &volume)?;
    io.init_login(dir, cli, &volume)?;
    Ok(volume)
}

/// Create the throwaway home of host login session `session` and every directory its
/// environment names, each 0700 and new (an existing path is refused, never reused).
#[cfg(unix)]
fn create_login_home(dir: &Path, cli: &str, session: &str) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt;
    let root = dir.join("login");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&root)
        .map_err(|e| format!("could not create the login directory: {e}"))?;
    let home = login_home(dir, session);
    let made = std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&home)
        .and_then(|()| {
            host_login_dirs(&home, cli)
                .iter()
                .try_for_each(|d| std::fs::DirBuilder::new().mode(0o700).create(d))
        });
    if let Err(e) = made {
        let _ = remove_login_home(dir, session);
        return Err(format!("could not create the login home: {e}"));
    }
    Ok(())
}

/// The throwaway home name of one renewal of `cli`'s sign-in by its own CLI on this host
/// ([`crate::seat_refresh`]): per daemon and per agent, never a login session's name.
pub(crate) fn refresh_session(gateway: &str, cli: &str) -> String {
    format!(
        "{gateway}-refresh-{cli}-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    )
}

/// Create renewal `session`'s throwaway home (as a host login's: every directory its
/// environment names, each 0700 and new) and return its path.
#[cfg(unix)]
pub(crate) fn create_refresh_home(
    dir: &Path,
    cli: &str,
    session: &str,
) -> Result<std::path::PathBuf, String> {
    create_login_home(dir, cli, session)?;
    Ok(login_home(dir, session))
}

/// Remove renewal `session`'s throwaway home; one already gone counts as removed.
pub(crate) fn remove_refresh_home(dir: &Path, session: &str) -> Result<(), String> {
    remove_login_home(dir, session)
}

/// Remove every renewal home of `cli` under this daemon's gateway directory (a crash's leftover:
/// a copy of the credential). Callers hold the mutation lock, so none is in use.
#[cfg(unix)]
pub(crate) fn sweep_refresh_homes(dir: &Path, cli: &str) -> Result<(), String> {
    let prefix = format!("{}-refresh-{cli}-", gw_name(dir));
    let entries = match std::fs::read_dir(dir.join("login")) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.to_string()),
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name
            .strip_prefix(&prefix)
            .is_some_and(|n| n.len() == 8 && n.bytes().all(|c| c.is_ascii_hexdigit()))
        {
            remove_login_home(dir, &name)?;
        }
    }
    Ok(())
}

/// The whole environment a host CLI renewing its own sign-in runs with in throwaway `home`:
/// [`HOST_LOGIN_PASSTHROUGH`] from `daemon_env` (search path, locale, the proxy and CA settings a
/// corporate host needs) and the home's own variables ([`host_login_env`]). Nothing else of the
/// daemon's reaches it. opencode also reaches its own private server on loopback directly, never
/// through a proxy, and does not update itself.
pub(crate) fn host_cli_env(
    home: &Path,
    cli: &str,
    daemon_env: &[(String, String)],
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = HOST_LOGIN_PASSTHROUGH
        .iter()
        .filter_map(|name| daemon_env.iter().find(|(k, _)| k == name).cloned())
        .collect();
    if cli == "opencode" {
        for name in ["NO_PROXY", "no_proxy"] {
            let loopback = "127.0.0.1,localhost,::1";
            match env.iter_mut().find(|(k, _)| k == name) {
                Some(slot) => slot.1 = format!("{},{loopback}", slot.1),
                None => env.push((name.to_string(), loopback.to_string())),
            }
        }
        env.push(("OPENCODE_DISABLE_AUTOUPDATE".into(), "1".into()));
    }
    env.extend(host_login_env(home, cli));
    env
}

/// Write `bytes` to a NEW file at `path`, owner-only (0600), and sync it.
#[cfg(unix)]
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| format!("could not write the CLI's store: {}", e.kind()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| format!("could not write the CLI's store: {}", e.kind()))
}

/// The file at `path` under host home `home` ([`read_home`]'s checks), `None` when absent.
#[cfg(unix)]
pub(crate) fn read_home_file(home: &Path, path: &str) -> Result<Option<Vec<u8>>, String> {
    Ok(read_home(home, &[path])?.into_iter().next().map(|(_, b)| b))
}

/// Remove the throwaway home (and pid file) of host login session `session`; one already gone
/// counts as removed. `remove_dir_all` never follows a symlink out of the home. Callers end the
/// login's processes first ([`stop_host_login`]), so nothing recreates the home afterwards.
fn remove_login_home(dir: &Path, session: &str) -> Result<(), String> {
    match std::fs::remove_dir_all(login_home(dir, session)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("could not remove the login home: {e}")),
    }
    match std::fs::remove_file(login_pid_file(dir, session)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("could not remove the login pid file: {e}")),
    }
}

/// Seconds since the process started, from `ps -o etime=` (`[[dd-]hh:]mm:ss`).
#[cfg(unix)]
fn process_age(pid: i32) -> Option<u64> {
    use wicked_apps_core::HardenedCommand;
    let mut cmd = std::process::Command::new("ps");
    cmd.hardened();
    cmd.args(["-o", "etime=", "-p", &pid.to_string()])
        .stdin(std::process::Stdio::null());
    let out = output_within(cmd, TIMEOUT)?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let (days, clock) = match text.split_once('-') {
        Some((d, c)) => (d.parse::<u64>().ok()?, c.to_string()),
        None => (0, text),
    };
    let mut secs = 0u64;
    for part in clock.split(':') {
        secs = secs * 60 + part.parse::<u64>().ok()?;
    }
    Some(days * 86_400 + secs)
}

/// End the processes of host login session `session`, if its login still runs: SIGTERM to its
/// process group, then SIGKILL after a grace. A pid is ours only when it still leads its own
/// process group and started no later than its pid file was written (a recycled pid is a
/// process started after ours died, so after the file). A sign-out or a new sign-in while the
/// login terminal is open would otherwise leave the CLI running, free to write a credential
/// into a home nobody tracks any more.
#[cfg(unix)]
fn stop_host_login(dir: &Path, session: &str) -> Result<(), String> {
    let file = login_pid_file(dir, session);
    let (text, written) = match std::fs::read_to_string(&file)
        .and_then(|t| Ok((t, std::fs::metadata(&file)?.modified()?)))
    {
        Ok(read) => read,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("could not read the login pid file: {e}")),
    };
    let Some(pid) = text.trim().parse::<i32>().ok().filter(|p| *p > 1) else {
        return Ok(());
    };
    let alive = |pid: i32| unsafe { libc::kill(pid, 0) == 0 };
    // SAFETY: getpgid/kill/killpg take plain integers; no memory is shared.
    if !alive(pid) || unsafe { libc::getpgid(pid) } != pid {
        return Ok(());
    }
    let since_written = SystemTime::now()
        .duration_since(written)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    match process_age(pid) {
        Some(age) if age + 2 >= since_written => {}
        _ => return Ok(()),
    }
    unsafe { libc::killpg(pid, libc::SIGTERM) };
    let deadline = std::time::Instant::now() + LOGIN_STOP_WAIT;
    while alive(pid) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    if alive(pid) {
        unsafe { libc::killpg(pid, libc::SIGKILL) };
    }
    Ok(())
}

/// Remove every throwaway home of `cli`'s host logins under this daemon's gateway directory,
/// ending any login still running in one.
#[cfg(unix)]
fn sweep_login_homes(dir: &Path, cli: &str) -> Result<(), String> {
    let prefix = format!("{}-login-{cli}-", gw_name(dir));
    let entries = match std::fs::read_dir(dir.join("login")) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.to_string()),
    };
    let mut sessions: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let name = name
                .strip_suffix(".pid")
                .map(str::to_string)
                .unwrap_or(name);
            name.strip_prefix(&prefix)
                .is_some_and(|n| n.len() == 8 && n.bytes().all(|c| c.is_ascii_hexdigit()))
                .then_some(name)
        })
        .collect();
    sessions.sort();
    sessions.dedup();
    for session in sessions {
        stop_host_login(dir, &session)?;
        remove_login_home(dir, &session)?;
    }
    Ok(())
}

/// Read `paths` from host login home `home` as [`read_volume`] reads a login volume: a missing
/// file is skipped; a symlink anywhere on the path, a non-regular file or an oversized one is
/// refused. Every component is opened relative to its parent with `O_NOFOLLOW` (`openat`), and
/// the file itself non-blocking and checked with `fstat`, so no link or FIFO swapped in after a
/// check is ever followed or waited on.
#[cfg(unix)]
fn read_home(home: &Path, paths: &[&str]) -> Result<Vec<(String, Vec<u8>)>, String> {
    use std::ffi::CString;
    use std::io::Read;
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    let open = |at: Option<&OwnedFd>, name: &[u8], flags: i32| -> std::io::Result<OwnedFd> {
        use std::os::fd::AsRawFd;
        let c = CString::new(name).map_err(|_| std::io::ErrorKind::InvalidInput)?;
        let base = at.map_or(libc::AT_FDCWD, |f| f.as_raw_fd());
        // SAFETY: `c` is a valid C string; the returned fd is owned exactly once below.
        let fd =
            unsafe { libc::openat(base, c.as_ptr(), flags | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
        if fd < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        }
    };
    let dir_flags = libc::O_RDONLY | libc::O_DIRECTORY;
    let root = open(None, home.as_os_str().as_bytes(), dir_flags)
        .map_err(|_| "the login home is missing".to_string())?;
    let mut files = Vec::new();
    'paths: for path in paths {
        let refused = || format!("could not read credential file {path}");
        let parts: Vec<&str> = path.split('/').collect();
        let (name, parents) = parts.split_last().expect("a non-empty path");
        let mut held: Option<OwnedFd> = None;
        for part in parents {
            match open(
                Some(held.as_ref().unwrap_or(&root)),
                part.as_bytes(),
                dir_flags,
            ) {
                Ok(fd) => held = Some(fd),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue 'paths,
                Err(_) => return Err(refused()),
            }
        }
        let parent = held.as_ref().unwrap_or(&root);
        let fd = match open(
            Some(parent),
            name.as_bytes(),
            libc::O_RDONLY | libc::O_NONBLOCK,
        ) {
            Ok(fd) => fd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(refused()),
        };
        let file = std::fs::File::from(fd);
        if !file.metadata().is_ok_and(|m| m.is_file()) {
            return Err(refused());
        }
        let mut bytes = Vec::new();
        file.take(MAX_FILE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| refused())?;
        if bytes.len() > MAX_FILE_BYTES {
            return Err(format!("credential file {path} exceeds size limit"));
        }
        files.push(((*path).into(), bytes));
    }
    Ok(files)
}

/// Whether login session `session` still holds its store. For a host-login agent that is its
/// home, or a login volume a container session from before the upgrade left (the session
/// marker records no backend, so both are looked for).
#[cfg(unix)]
fn login_exists(io: &dyn BrokerIo, dir: &Path, cli: &str, session: &str) -> Result<bool, String> {
    if host_login(cli) && std::fs::symlink_metadata(login_home(dir, session)).is_ok() {
        return Ok(true);
    }
    io.login_exists(session)
}

/// End session `session`'s login: a host login's process group (normally gone with its
/// terminal) and any login container, including a pre-upgrade one of a host-login agent.
#[cfg(unix)]
fn halt_login(io: &dyn BrokerIo, dir: &Path, cli: &str, session: &str) -> Result<(), String> {
    if host_login(cli) {
        stop_host_login(dir, session)?;
        if !io.login_exists(session)? {
            return Ok(());
        }
    }
    io.stop_login(session)
}

/// Remove session `session`'s credential-bearing store after ending its login: the host home,
/// and a login volume (a host-login agent's pre-upgrade container session keeps its volume
/// until it is confirmed removed, never orphaned by the marker's removal).
#[cfg(unix)]
fn discard_login(io: &dyn BrokerIo, dir: &Path, cli: &str, session: &str) -> Result<(), String> {
    if host_login(cli) {
        stop_host_login(dir, session)?;
        remove_login_home(dir, session)?;
        if !io.login_exists(session)? {
            return Ok(());
        }
    }
    io.remove_login(session)
}

/// Stop and remove the login container of session `volume`, if it still runs. Its terminal is
/// over whenever this is called (ingest runs on the terminal's exit), but a terminal whose client
/// was killed leaves `docker run -it` behind, still holding the volume: every later volume
/// removal, and so every later sign-in, would fail (found on the proof daemon, C6).
#[cfg(unix)]
#[cfg(test)]
fn stop_login_container(rt: &Runtime, volume: &str) -> Result<(), String> {
    // agy c6-r1: a login that just exited is still being removed by its own `--rm` (`rm -f` then
    // answers "removal already in progress" and `ps -a` still lists it), so poll until it is gone.
    let deadline = std::time::Instant::now() + LOGIN_STOP_WAIT;
    loop {
        let names = login_containers_on(rt, volume)?;
        if names.is_empty() {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err("could not stop the login container".into());
        }
        for name in names {
            // Judged by the next listing: one that removed itself meanwhile makes `rm -f` fail.
            let _ = checked(rt, &["rm".into(), "-f".into(), name]);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// How long a login container's removal may take before stopping it counts as failed.
#[cfg(unix)]
const LOGIN_STOP_WAIT: Duration = Duration::from_secs(10);

/// The login containers that mount session `volume`: the one named after the session, and one an
/// earlier engine named `wicked-login-<cli>-<32 hex>` (codex c6-r1: an abandoned login from before
/// the upgrade still holds its volume). Nothing else that mounts the volume is ours to stop.
#[cfg(unix)]
#[cfg(test)]
fn login_containers_on(rt: &Runtime, volume: &str) -> Result<Vec<String>, String> {
    let out = execute(
        rt,
        &[
            "ps".into(),
            "-a".into(),
            "--filter".into(),
            format!("volume={volume}"),
            "--format".into(),
            "{{.Names}}".into(),
        ],
    )?;
    if !out.status.success() {
        return Err("could not list the login containers".into());
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|name| ours_on(volume, name))
        .map(str::to_string)
        .collect())
}

#[cfg(test)]
fn ours_on(volume: &str, name: &str) -> bool {
    name == volume
        || name.strip_prefix("wicked-login-").is_some_and(|rest| {
            rest.rsplit_once('-').is_some_and(|(cli, id)| {
                supported_cli(cli) && id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
            })
        })
}

/// Remove a login volume (stopping its login container first), treating one that is already
/// gone as removed (a crash after the removal and before its marker was unlinked).
#[cfg(unix)]
#[cfg(test)]
fn remove_login_volume(rt: &Runtime, volume: &str) -> Result<(), String> {
    stop_login_container(rt, volume)?;
    match checked(rt, &["volume".into(), "rm".into(), volume.into()]) {
        Ok(()) => Ok(()),
        Err(e) if !volume_exists(rt, volume)? => {
            let _ = e;
            Ok(())
        }
        Err(e) => Err(e),
    }
}

fn session_committed(dir: &Path, cli: &str) -> Result<bool, String> {
    std::fs::read_to_string(session_path(dir, cli))
        .map(|s| s.ends_with("\ncommitted"))
        .map_err(|e| e.to_string())
}

fn write_session(dir: &Path, cli: &str, volume: &str) -> Result<(), String> {
    write_session_state(dir, cli, volume, false)
}

fn write_session_state(dir: &Path, cli: &str, volume: &str, committed: bool) -> Result<(), String> {
    use std::io::Write;
    std::fs::create_dir_all(dir.join("login")).map_err(|e| e.to_string())?;
    let path = session_path(dir, cli);
    let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let result = (|| -> std::io::Result<()> {
        let mut f = opts.open(&temp)?;
        f.write_all(volume.as_bytes())?;
        if committed {
            f.write_all(b"\ncommitted")?;
        }
        f.sync_all()?;
        std::fs::rename(&temp, &path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    result.map_err(|e| e.to_string())
}

fn parse_args(args: &[String]) -> Result<(&str, &str), String> {
    if args.len() == 2
        && matches!(
            args[0].as_str(),
            "argv" | "ingest" | "status" | "sign-out" | "refresh"
        )
        && supported_cli(&args[1])
    {
        Ok((&args[0], &args[1]))
    } else {
        // Do not echo invalid arguments; they could be misplaced credentials.
        Err(USAGE.into())
    }
}

/// Identify the daemon without inspecting or creating its host state directory. Crew must
/// pass the same absolute directory spelling it uses to start the gateway.
fn gateway_for_dir(dir: Option<&Path>) -> Result<(), String> {
    let dir = dir.filter(|p| p.is_absolute()).ok_or_else(|| {
        format!("set {GATEWAY_DIR_ENV} to the daemon's absolute gateway directory")
    })?;
    let _ = dir;
    Ok(())
}

#[cfg(unix)]
fn prepare_login_argv(io: &dyn BrokerIo, dir: &Path, cli: &str) -> Result<Vec<String>, String> {
    // C6: an agent whose credential the broker cannot hold never opens a login (the user would
    // complete a real sign-in only for ingest to discard it).
    broker_files(cli).map_err(|e| format!("{cli} cannot sign in for container seats yet: {e}"))?;
    io.prepare_login(cli)?;
    // One transaction from the session to its started login (review B-4 #3): a sign-out or a newer
    // sign-in waits, so it can never finish before this login's pod exists and leave it behind.
    let _lock = crate::seat_refresh::mutation_lock(dir)?;
    let volume = new_session_locked(io, dir, cli)?;
    match io.open_login(cli, &volume) {
        Ok(argv) => Ok(argv),
        Err(e) => {
            // A login that did not start is discarded with its store, inside the transaction.
            // Its session marker goes only once that is done (codex b4-r2 #2): it is how a
            // sign-out or the next sign-in finds a pod and store a failed cleanup left.
            match discard_login(io, dir, cli, &volume) {
                Ok(()) => {
                    let _ = crate::seat_refresh::remove_private_file(&session_path(dir, cli));
                    Err(e)
                }
                Err(c) => Err(format!(
                    "{e}; its login could not be cleaned up ({c}), and is left for the next \
                     sign-in or sign-out to clear"
                )),
            }
        }
    }
}

/// The host CLI's `--version`, run as the login will run (isolated environment, in the
/// throwaway home). `None` when it cannot run or names no version: then `exe` is not the coding
/// agent the seat runs (e.g. another tool of that name earlier on PATH).
#[cfg(unix)]
fn host_version(
    exe: &Path,
    home: &Path,
    cli: &str,
    daemon_env: &[(String, String)],
) -> Option<String> {
    let argv = host_login_argv(cli, exe, home, Path::new("-"), "", daemon_env);
    // `env -i <vars> /bin/sh -c …`: keep the vars, run the executable directly.
    let vars = argv
        .iter()
        .skip(2)
        .take_while(|a| *a != "/bin/sh")
        .filter_map(|a| a.split_once('='));
    use wicked_apps_core::HardenedCommand;
    let mut cmd = std::process::Command::new(exe);
    cmd.hardened();
    cmd.arg("--version")
        .env_clear()
        .envs(vars)
        .current_dir(home)
        .stdin(std::process::Stdio::null());
    let out = output_within(cmd, TIMEOUT)?;
    if !out.status.success() {
        return None;
    }
    parse_version(&String::from_utf8_lossy(&out.stdout))
        .or_else(|| parse_version(&String::from_utf8_lossy(&out.stderr)))
}

/// Open a host login session for `cli` and render its argv (see [`host_login`]). `daemon_env` is
/// the daemon's environment, of which only [`HOST_LOGIN_PASSTHROUGH`] reaches the login.
#[cfg(unix)]
fn prepare_host_login_argv(
    io: &dyn BrokerIo,
    dir: &Path,
    cli: &str,
    daemon_env: &[(String, String)],
) -> Result<Vec<String>, String> {
    broker_files(cli).map_err(|e| format!("{cli} cannot sign in for container seats yet: {e}"))?;
    let pinned =
        if std::env::var_os(format!("WICKED_SEAT_IMAGE_{}", cli.to_ascii_uppercase())).is_some() {
            None
        } else {
            pinned_version(cli)
        };
    let path = daemon_env
        .iter()
        .find(|(k, _)| k == "PATH")
        .map(|(_, v)| std::ffi::OsStr::new(v.as_str()));
    let exe = find_on_path(cli, path).ok_or_else(|| {
        format!(
            "{cli} is not installed on this host: its sign-in runs this host's own {cli}{}. Install it, then sign in again",
            pinned.map(|v| format!(" (the seats run {cli} {v})")).unwrap_or_default()
        )
    })?;
    // The version probe runs in a throwaway home of its own, before any session exists: a
    // probe that fails touches no session marker (codex review: a failure after `new_session`
    // could delete a concurrent sign-in's marker outside the mutation lock). A probe home a
    // crash leaves behind matches the session pattern, so the next sign-in sweeps it.
    let probe = login_volume(&gw_name(dir), cli);
    create_login_home(dir, cli, &probe)?;
    let version = host_version(&exe, &login_home(dir, &probe), cli, daemon_env);
    remove_login_home(dir, &probe)?;
    let Some(version) = version else {
        return Err(format!(
            "could not run this host's {cli} ({}): it answered no version, so it may be another tool of that name",
            exe.display()
        ));
    };
    let session = new_session(io, dir, cli)?;
    let home = login_home(dir, &session);
    let banner = host_login_banner(cli, &version, pinned);
    Ok(host_login_argv(
        cli,
        &exe,
        &home,
        &login_pid_file(dir, &session),
        &banner,
        daemon_env,
    ))
}

#[cfg(unix)]
fn dispatch(verb: &str, cli: &str) -> Result<i32, String> {
    let dir = std::env::var_os(GATEWAY_DIR_ENV).map(std::path::PathBuf::from);
    gateway_for_dir(dir.as_deref())?;
    let dir = dir.as_deref().expect("validated by gateway_for_dir");
    // The seats run on the daemon's cluster (k8s-swap): so do their sign-ins. A status read never
    // starts anything; every other verb may provision the gateway's storage on a first sign-in,
    // never taking the daemon's ownership (`k8s::gw::attach`).
    let io = match crate::k8s::login::K8sIo::for_dir(dir, verb != "status") {
        Ok(io) => io,
        // A status read always answers a status (review B-4 pi #9): a cluster that cannot be read
        // now, or a gateway stopped after a revocation, is Unknown, never an error with no JSON.
        Err(e) if verb == "status" => {
            let status = publication_status(dir, cli, BrokerStatus::Unknown(e));
            println!(
                "{}",
                serde_json::to_string(&status).map_err(|_| "could not encode broker status")?
            );
            return Ok(1);
        }
        Err(e) => return Err(e),
    };
    let io: &dyn BrokerIo = &io;
    match verb {
        "argv" => {
            let argv = if host_login(cli) {
                let env: Vec<(String, String)> = std::env::vars_os()
                    .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
                    .collect();
                prepare_host_login_argv(io, dir, cli, &env)?
            } else {
                prepare_login_argv(io, dir, cli)?
            };
            println!(
                "{}",
                serde_json::to_string(&argv).map_err(|_| "could not encode login argv")?
            );
            Ok(0)
        }
        "ingest" => {
            let report = ingest_and_route(io, dir, cli)?;
            println!(
                "{}",
                serde_json::to_string(&report).map_err(|_| "could not encode ingest report")?
            );
            Ok(0)
        }
        "status" => {
            let status = with_last_refresh(
                broker_status(io, cli),
                crate::seat_refresh::last_refresh(dir, cli),
            );
            let status = publication_status(dir, cli, status);
            let rc = if matches!(status, BrokerStatus::Unknown(_)) {
                1
            } else {
                0
            };
            println!(
                "{}",
                serde_json::to_string(&status).map_err(|_| "could not encode broker status")?
            );
            Ok(rc)
        }
        "sign-out" => {
            sign_out_and_route(io, dir, cli)?;
            Ok(0)
        }
        "refresh" => {
            let outcome = crate::seat_refresh::refresh(io, dir, cli, true)?;
            println!(
                "{}",
                serde_json::to_string(&outcome).map_err(|_| "could not encode the outcome")?
            );
            Ok(0)
        }
        _ => unreachable!("validated by parse_args"),
    }
}

/// Container-only CLI entry point: no store and no actor. Bad input exits 2 before probing
/// a runtime. Native Windows reports the unsupported runtime explicitly and exits 1.
pub fn seat_login_main(args: &[String]) -> i32 {
    let (verb, cli) = match parse_args(args) {
        Ok(parsed) => parsed,
        Err(usage) => {
            eprintln!("{usage}");
            return 2;
        }
    };
    #[cfg(unix)]
    match dispatch(verb, cli) {
        Ok(rc) => rc,
        Err(e) => {
            eprintln!("wicked-core seat-login: {e}");
            1
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (verb, cli);
        eprintln!("wicked-core seat-login: a container runtime is required (Unix host)");
        1
    }
}

/// The state machine over its transport seam (k8s-swap B-4), with an in-memory transport.
#[cfg(all(test, unix))]
#[path = "seat_login_io_tests.rs"]
mod io_tests;

/// agy's API-key sign-in: the paste, the broker kind, the route, the seat (and LIVE, end to end).
#[cfg(all(test, unix))]
#[path = "seat_login_agy_tests.rs"]
mod agy_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[cfg(unix)]
    struct FakeRuntime {
        dir: std::path::PathBuf,
        rt: Runtime,
        gw: Gateway,
    }

    #[cfg(unix)]
    impl FakeRuntime {
        fn new() -> Self {
            let dir = std::env::temp_dir()
                .join(format!(".c4-login-test-{}", uuid::Uuid::new_v4().simple()));
            std::fs::create_dir_all(&dir).unwrap();
            let gw = Gateway::for_dir(&dir);
            let path = dir.join("runtime");
            // Offline stand-in: it records argv and stdin; it never invokes a container runtime.
            crate::test_env::install_stub(
                &path,
                r#"#!/bin/sh
base=${0%/*}
printf '%s\n' "$*" >> "$base/calls"
if [ "$1 $2" = 'image inspect' ]; then exit 0; fi
if [ "$1 $2" = 'container inspect' ]; then
  case "$3" in
    wicked-mutation-*) [ -e "$base/fail_cleanup" ] && { printf 'still live'; exit 0; }; printf 'No such container\n' >&2; exit 1;;
    *) case "$(cat "$base/state" 2>/dev/null)" in
      stopped) printf 'false\n';; absent) printf 'No such container\n' >&2; exit 1;; unknown) exit 1;; *) printf 'true\n';; esac; exit 0;;
  esac
fi
if [ "$1 $2" = 'rm --force' ]; then exit 0; fi
if [ "$1 $2" = 'volume ls' ]; then cat "$base/volumes"; exit 0; fi
if [ "$1 $2" = 'volume create' ]; then
  for arg do last=$arg; done
  printf '%s\n' "$last" >> "$base/volumes"; exit 0
fi
if [ "$1 $2" = 'volume rm' ]; then [ ! -e "$base/fail_remove" ]; exit $?; fi
for arg do last=$arg; done
case "$last" in
  *'chown 10001 /cfg /broker;'*)
    for word in $last; do
      case "$word" in /broker/*) name=${word#/broker/}; name=${name%;};
        [ -e "$base/broker/$name" ] || : > "$base/broker/$name";; esac
    done;;
  *'cat > /v/.config.json.new'*) cat > "$base/published"; [ ! -e "$base/fail_publish" ];;
  *'cat > /v/.'*)
    file=${last##* /v/}
    cat > "$base/broker/$file.new"
    [ -e "$base/fail_$file" ] && exit 1
    mv "$base/broker/$file.new" "$base/broker/$file";;
  *'head -c'*|*'printf present'*)
    case "$*" in
      *'-broker:/in:ro'*)
        file=${last##*/in/}; file=${file%% *}
        [ -f "$base/broker/$file" ] || exit 44
        case "$last" in *'head -c'*) cat "$base/broker/$file";; *) [ ! -s "$base/broker/$file" ] || printf present;; esac;;
      *wicked-setup-token*) [ -f "$base/setup-token" ] || exit 44; cat "$base/setup-token";;
      *) [ -f "$base/input" ] || exit 44; cat "$base/input";;
    esac;;
  *': > /in/'*)
    for word in $last; do case "$word" in /in/*) : > "$base/broker/${word#/in/}";; esac; done;;
  *'rm -f /v/'*) file=${last##*/v/};
    [ ! -e "$base/fail_remove_$file" ] || exit 1
    rm -f "$base/broker/$file";;
  *'read K;'*) cat > /dev/null; [ ! -e "$base/fail_probe" ];;
  *) exit 0;;
esac
"#,
            );
            std::fs::create_dir(dir.join("broker")).unwrap();
            std::fs::write(dir.join("volumes"), format!("{}\n", volumes(&gw).1)).unwrap();
            Self {
                dir,
                rt: Runtime::answering(path),
                gw,
            }
        }
        fn calls(&self) -> String {
            std::fs::read_to_string(self.dir.join("calls")).unwrap_or_default()
        }
    }

    /// Write `content` at `path` in `cli`'s open host login home (what the host CLI leaves).
    #[cfg(unix)]
    fn host_input(fake: &FakeRuntime, cli: &str, path: &str, content: &str) {
        let session = read_session(&fake.dir, cli)
            .unwrap()
            .expect("an open login session");
        let file = login_home(&fake.dir, &session).join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, content).unwrap();
    }

    #[cfg(unix)]
    impl Drop for FakeRuntime {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[cfg(unix)]
    #[test]
    fn first_ingest_preserves_live_keys_without_gateway_takeover() {
        if crate::validator::skip_when_nested(
            "first_ingest_preserves_live_keys_without_gateway_takeover",
        ) {
            return;
        }
        use crate::seat_gateway::{configure_gateway, register_unit, GatewayConfig, UnitKey};
        let fake = FakeRuntime::new();
        configure_gateway(&fake.dir, &GatewayConfig::default()).unwrap();
        let live = UnitKey::new("FAKE-run", "FAKE-unit", "claude");
        register_unit(&fake.dir, live.clone()).unwrap();
        let before = std::fs::read(fake.dir.join("config/units.json")).unwrap();
        // First ingest has no broker volume yet; initialization keeps existing files, too.
        std::fs::write(fake.dir.join("volumes"), "").unwrap();
        std::fs::write(fake.dir.join("broker/anthropic.token"), "FAKE-live-claude").unwrap();
        new_session(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "codex",
        )
        .unwrap();
        host_input(&fake, "codex", "codex/auth.json", &chatgpt().to_string());
        ingest_and_route(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "codex",
        )
        .unwrap();
        assert_eq!(
            std::fs::read(fake.dir.join("config/units.json")).unwrap(),
            before
        );
        assert_eq!(
            std::fs::read(fake.dir.join("broker/anthropic.token")).unwrap(),
            b"FAKE-live-claude"
        );
        let calls = fake.calls();
        assert!(calls.contains("chown 10001 /cfg /broker;"));
        assert!(
            calls.find("chown 10001 /cfg /broker;").unwrap()
                < calls.find("cat > /v/.codex.auth.json.new").unwrap()
        );
        assert!(std::fs::read_to_string(fake.dir.join("published"))
            .unwrap()
            .contains(&live.key));
        assert!(!fake.dir.join("config/revoking.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn refresh_recovery_prefers_rescue_and_refuses_an_unstored_rotation() {
        if crate::validator::skip_when_nested(
            "refresh_recovery_prefers_rescue_and_refuses_an_unstored_rotation",
        ) {
            return;
        }
        let fake = FakeRuntime::new();
        let _lock = crate::seat_refresh::mutation_lock(&fake.dir).unwrap();
        let old = oauth(1000);
        let mut staged = old.clone();
        staged["claudeAiOauth"]["refreshToken"] = json!("FAKE-R2");
        let mut rescued = old.clone();
        rescued["claudeAiOauth"]["refreshToken"] = json!("FAKE-R3");
        rescued["claudeAiOauth"]["accessToken"] = json!("FAKE-access-R3");
        let pending = crate::seat_refresh::pending_path(&fake.dir, "claude");
        std::fs::write(&pending, "rotated credential issued").unwrap();
        std::fs::write(
            fake.dir.join("broker/claude.credentials.json"),
            old.to_string(),
        )
        .unwrap();
        std::fs::write(
            fake.dir.join("broker/claude.credentials.json.next"),
            staged.to_string(),
        )
        .unwrap();
        let rescue = crate::seat_refresh::rescue_path(&fake.dir, "claude");
        crate::seat_refresh::create_private_file(&rescue, rescued.to_string().as_bytes()).unwrap();
        std::fs::write(fake.dir.join("fail_claude.credentials.json.next"), "").unwrap();
        assert!(crate::seat_refresh::repair_refresh(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "claude"
        )
        .is_err());
        assert!(rescue.exists());
        assert_eq!(
            std::fs::read_to_string(fake.dir.join("broker/claude.credentials.json")).unwrap(),
            old.to_string()
        );
        std::fs::remove_file(fake.dir.join("fail_claude.credentials.json.next")).unwrap();
        crate::seat_refresh::repair_refresh(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "claude",
        )
        .unwrap();
        assert!(!rescue.exists());
        assert!(!pending.exists());
        assert_eq!(
            std::fs::read_to_string(fake.dir.join("broker/claude.credentials.json")).unwrap(),
            rescued.to_string()
        );
        assert_eq!(
            std::fs::read(fake.dir.join("broker/anthropic.token")).unwrap(),
            b"FAKE-access-R3"
        );
        assert!(
            crate::seat_refresh::last_refresh(&fake.dir, "claude")
                .unwrap()
                .ok
        );

        // A pending issued generation without a recovery source cannot use the current record.
        for marker in ["", "rotated credential issued"] {
            std::fs::write(&pending, marker).unwrap();
            std::fs::write(fake.dir.join("calls"), "").unwrap();
            assert_eq!(
                crate::seat_refresh::repair_refresh(
                    &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
                    &fake.dir,
                    "claude"
                )
                .unwrap_err(),
                "rotated credential not yet stored in the broker"
            );
            assert!(pending.exists());
            assert!(
                !crate::seat_refresh::last_refresh(&fake.dir, "claude")
                    .unwrap()
                    .ok
            );
            assert!(!fake.calls().contains("cat > /v/"));
        }
        // Only a fully committed generation may recover from current after `.next` removal.
        std::fs::write(&pending, "rotated credential stored").unwrap();
        crate::seat_refresh::repair_refresh(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "claude",
        )
        .unwrap();
        assert!(!pending.exists());
        // codex lg-r4: a staged record with no pending marker can only be an orphan writer's
        // (a real one always writes the marker before staging): it is discarded, never served.
        let mut orphan = rescued.clone();
        orphan["claudeAiOauth"]["refreshToken"] = json!("FAKE-R4");
        std::fs::write(
            fake.dir.join("broker/claude.credentials.json.next"),
            orphan.to_string(),
        )
        .unwrap();
        crate::seat_refresh::repair_refresh(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "claude",
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(fake.dir.join("broker/claude.credentials.json")).unwrap(),
            rescued.to_string()
        );
        assert!(!pending.exists());
    }

    #[cfg(unix)]
    #[test]
    fn interrupted_sign_out_tombstone_wins_over_every_recovery_source() {
        if crate::validator::skip_when_nested(
            "interrupted_sign_out_tombstone_wins_over_every_recovery_source",
        ) {
            return;
        }
        for recovery in 0..3 {
            let fake = FakeRuntime::new();
            new_session(
                &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
                &fake.dir,
                "claude",
            )
            .unwrap();
            let record = oauth(9_000_000_000_000);
            let journal = vec![
                ("claude.credentials.json", record.to_string()),
                ("anthropic.token", "FAKE-replay".to_string()),
            ];
            for (name, bytes) in [
                ("claude.credentials.json", record.to_string()),
                ("claude.credentials.json.next", record.to_string()),
                (
                    "claude.ingest.next",
                    serde_json::to_string(&journal).unwrap(),
                ),
                ("anthropic.token", "FAKE-replay".to_string()),
            ] {
                std::fs::write(fake.dir.join("broker").join(name), bytes).unwrap();
            }
            std::fs::write(
                crate::seat_refresh::pending_path(&fake.dir, "claude"),
                if recovery == 1 {
                    "sign-in incomplete"
                } else {
                    "rotated credential issued"
                },
            )
            .unwrap();
            let rescue = crate::seat_refresh::rescue_path(&fake.dir, "claude");
            crate::seat_refresh::create_private_file(&rescue, record.to_string().as_bytes())
                .unwrap();
            std::fs::write(fake.dir.join("fail_remove_claude.ingest.next"), "").unwrap();
            assert!(sign_out_and_route(
                &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
                &fake.dir,
                "claude"
            )
            .is_err());
            assert!(signout_path(&fake.dir, "claude").exists());
            assert!(std::fs::read(fake.dir.join("broker/anthropic.token"))
                .unwrap()
                .is_empty());
            assert!(fake
                .dir
                .join("broker/claude.credentials.json.next")
                .exists());
            std::fs::remove_file(fake.dir.join("fail_remove_claude.ingest.next")).unwrap();
            std::fs::write(fake.dir.join("fail_probe"), "").unwrap();
            std::fs::write(fake.dir.join("calls"), "").unwrap();
            let repair = || -> Result<(), String> {
                match recovery {
                    0 => crate::seat_refresh::refresh(
                        &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
                        &fake.dir,
                        "claude",
                        false,
                    )
                    .map(|_| ()),
                    1 => {
                        let _lock = crate::seat_refresh::mutation_lock(&fake.dir)?;
                        repair_ingest(
                            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
                            &fake.dir,
                            "claude",
                        )
                        .map(|_| ())
                    }
                    _ => {
                        let _lock = crate::seat_refresh::mutation_lock(&fake.dir)?;
                        crate::seat_refresh::repair_refresh(
                            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
                            &fake.dir,
                            "claude",
                        )
                        .map(|_| ())
                    }
                }
            };
            assert!(repair().is_err());
            assert!(signout_path(&fake.dir, "claude").exists());
            std::fs::remove_file(fake.dir.join("fail_probe")).unwrap();
            repair().unwrap();
            assert!(!signout_path(&fake.dir, "claude").exists());
            for name in broker_files("claude").unwrap() {
                assert!(std::fs::read(fake.dir.join("broker").join(name))
                    .unwrap()
                    .is_empty());
            }
            assert!(!fake
                .dir
                .join("broker/claude.credentials.json.next")
                .exists());
            assert!(!fake.dir.join("broker/claude.ingest.next").exists());
            assert!(!rescue.exists());
            assert!(!crate::seat_refresh::pending_path(&fake.dir, "claude").exists());
            assert!(!session_path(&fake.dir, "claude").exists());
            assert!(!fake
                .calls()
                .contains("cat > /v/.claude.credentials.json.new"));
            assert!(!fake.calls().contains("cat > /v/.anthropic.token.new"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn ingest_validates_account_before_mutation_and_keeps_session_on_publish_failure() {
        if crate::validator::skip_when_nested(
            "ingest_validates_account_before_mutation_and_keeps_session_on_publish_failure",
        ) {
            return;
        }
        let fake = FakeRuntime::new();
        let session = new_session(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "codex",
        )
        .unwrap();
        std::fs::write(fake.dir.join("calls"), "").unwrap();
        let mut invalid = chatgpt();
        invalid["tokens"]["account_id"] = json!("FAKE\r\nx-evil: 1");
        host_input(&fake, "codex", "codex/auth.json", &invalid.to_string());
        assert!(ingest_and_route(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "codex"
        )
        .is_err());
        assert!(!fake.calls().contains("cat > /v/"));
        // Nothing usable: the host login is over and its home is gone.
        assert_eq!(read_session(&fake.dir, "codex").unwrap(), None);
        assert!(!login_home(&fake.dir, &session).exists());
        let session = new_session(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "codex",
        )
        .unwrap();
        host_input(&fake, "codex", "codex/auth.json", &chatgpt().to_string());
        std::fs::write(fake.dir.join("fail_publish"), "").unwrap();
        assert!(ingest_and_route(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "codex"
        )
        .is_err());
        assert!(fake.calls().contains("cat > /v/.openai.token.new"));
        assert_eq!(
            read_session(&fake.dir, "codex").unwrap(),
            Some(session.clone())
        );
        std::fs::remove_file(fake.dir.join("fail_publish")).unwrap();
        ingest_and_route(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "codex",
        )
        .unwrap();
        assert!(!login_home(&fake.dir, &session).exists());
        assert!(read_session(&fake.dir, "codex").unwrap().is_none());
        // A login container keeps its session (and volume) for a retry until removal succeeds.
        let session = new_session(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "claude",
        )
        .unwrap();
        std::fs::write(fake.dir.join("input"), oauth(9_000_000_000_000).to_string()).unwrap();
        std::fs::write(fake.dir.join("fail_remove"), "").unwrap();
        assert!(ingest_and_route(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "claude"
        )
        .is_err());
        assert_eq!(read_session(&fake.dir, "claude").unwrap(), Some(session));
        std::fs::remove_file(fake.dir.join("fail_remove")).unwrap();
        ingest_and_route(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "claude",
        )
        .unwrap();
        assert!(fake.calls().contains("volume rm"));
        assert!(read_session(&fake.dir, "claude").unwrap().is_none());
    }

    /// codex lg-r4: the session marker is the only record of the credential-bearing login
    /// volume, so a volume that is already gone (a crash after its removal) counts as removed,
    /// while one that is still there keeps the failure (and the marker).
    #[cfg(unix)]
    #[test]
    fn a_login_volume_already_gone_counts_as_removed() {
        let fake = FakeRuntime::new();
        std::fs::write(fake.dir.join("fail_remove"), "").unwrap();
        assert!(remove_login_volume(&fake.rt, "wicked-agw-x-login-codex-0000abcd").is_ok());
        std::fs::write(
            fake.dir.join("volumes"),
            "wicked-agw-x-login-codex-0000abcd\n",
        )
        .unwrap();
        assert!(remove_login_volume(&fake.rt, "wicked-agw-x-login-codex-0000abcd").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn new_login_sessions_remove_previous_volume_and_ingest_only_recorded_session() {
        use std::os::unix::fs::PermissionsExt;
        let fake = FakeRuntime::new();
        let first = new_session(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "claude",
        )
        .unwrap();
        let second = new_session(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "claude",
        )
        .unwrap();
        assert_ne!(first, second);
        let calls = fake.calls();
        assert!(
            calls.find(&format!("volume rm {first}")).unwrap()
                < calls.find(&format!("volume create {second}")).unwrap()
        );
        assert_eq!(
            read_session(&fake.dir, "claude").unwrap(),
            Some(second.clone())
        );
        assert_eq!(
            std::fs::metadata(session_path(&fake.dir, "claude"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        std::fs::write(fake.dir.join("input"), oauth(9_000_000_000_000).to_string()).unwrap();
        std::fs::write(fake.dir.join("calls"), "").unwrap();
        ingest_and_route(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "claude",
        )
        .unwrap();
        assert!(fake.calls().contains(&format!("{second}:/in:ro")));
        assert!(!fake.calls().contains(&first));
    }

    #[cfg(unix)]
    #[test]
    fn ingest_and_sign_out_block_behind_refresh_and_do_not_mutate_early() {
        if crate::validator::skip_when_nested(
            "ingest_and_sign_out_block_behind_refresh_and_do_not_mutate_early",
        ) {
            return;
        }
        for sign_out in [false, true] {
            let fake = FakeRuntime::new();
            new_session(
                &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
                &fake.dir,
                "codex",
            )
            .unwrap();
            host_input(&fake, "codex", "codex/auth.json", &chatgpt().to_string());
            std::fs::write(fake.dir.join("calls"), "").unwrap();
            let held = crate::seat_refresh::mutation_lock(&fake.dir).unwrap();
            let (tx, rx) = std::sync::mpsc::channel();
            let (rt, gw, dir) = (fake.rt.clone(), fake.gw.clone(), fake.dir.clone());
            let worker = std::thread::spawn(move || {
                tx.send(()).unwrap();
                if sign_out {
                    sign_out_and_route(&crate::seat_login::DockerIo::new(&rt, &gw), &dir, "codex")
                } else {
                    ingest_and_route(&crate::seat_login::DockerIo::new(&rt, &gw), &dir, "codex")
                        .map(|_| ())
                }
            });
            rx.recv().unwrap();
            // The actual cross-process lock remains busy for independent opens too.
            assert!(crate::seat_refresh::open_mutation_lock(&fake.dir)
                .unwrap()
                .try_lock()
                .is_err());
            assert!(fake.calls().is_empty());
            drop(held);
            worker.join().unwrap().unwrap();
            assert!(!fake.calls().is_empty());
        }
    }

    #[cfg(unix)]
    #[test]
    fn sign_out_returns_error_when_config_application_cannot_be_confirmed() {
        if crate::validator::skip_when_nested(
            "sign_out_returns_error_when_config_application_cannot_be_confirmed",
        ) {
            return;
        }
        let fake = FakeRuntime::new();
        std::fs::write(fake.dir.join("fail_probe"), "").unwrap();
        let error = sign_out_and_route(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "claude",
        )
        .unwrap_err();
        assert!(error.contains("sign-out could not confirm"));
        assert!(fake.calls().contains("cat > /v/.config.json.new"));
        assert!(fake.calls().contains("read K;"));
        std::fs::remove_file(fake.dir.join("fail_probe")).unwrap();
        sign_out_and_route(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "claude",
        )
        .unwrap();
    }

    /// codex c6-r3: arm finishes a published sign-in's interrupted cleanup (its login volume holds
    /// a copy of the credential), and never touches a session that may be a login still open.
    #[cfg(unix)]
    #[test]
    fn arm_finishes_a_committed_sign_ins_cleanup_and_leaves_an_open_login_alone() {
        let fake = FakeRuntime::new();
        std::fs::write(fake.dir.join("broker/pi.token"), "FAKE-key").unwrap();
        let open = new_session(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "pi",
        )
        .unwrap();
        assert_eq!(
            seat_sign_in(
                &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
                &fake.dir,
                "pi"
            )
            .unwrap(),
            crate::seat_credentials::CredentialKind::ProviderKey.into()
        );
        assert_eq!(read_session(&fake.dir, "pi").unwrap(), Some(open.clone()));
        assert!(login_home(&fake.dir, &open).exists());
        write_session_state(&fake.dir, "pi", &open, true).unwrap();
        assert!(seat_sign_in(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "pi"
        )
        .is_ok());
        assert_eq!(read_session(&fake.dir, "pi").unwrap(), None);
        assert!(!login_home(&fake.dir, &open).exists());
    }

    #[cfg(unix)]
    #[test]
    fn committed_ingest_retry_preserves_since_rotated_record() {
        let fake = FakeRuntime::new();
        new_session(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "codex",
        )
        .unwrap();
        host_input(&fake, "codex", "codex/auth.json", &chatgpt().to_string());
        std::fs::write(fake.dir.join("fail_publish"), "").unwrap();
        assert!(ingest_and_route(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "codex"
        )
        .is_err());
        assert!(session_committed(&fake.dir, "codex").unwrap());
        let mut rotated = chatgpt();
        rotated["tokens"]["refresh_token"] = json!("FAKE-rotated-R2");
        std::fs::write(fake.dir.join("broker/codex.auth.json"), rotated.to_string()).unwrap();
        std::fs::remove_file(fake.dir.join("fail_publish")).unwrap();
        std::fs::write(fake.dir.join("calls"), "").unwrap();
        ingest_and_route(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "codex",
        )
        .unwrap();
        assert!(!fake.calls().contains("cat > /v/.codex.auth.json.new"));
        let current: Value = serde_json::from_slice(
            &std::fs::read(fake.dir.join("broker/codex.auth.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(current["tokens"]["refresh_token"], "FAKE-rotated-R2");
    }

    #[cfg(unix)]
    #[test]
    fn partial_ingest_is_unknown_and_refresh_repairs_before_not_due() {
        let fake = FakeRuntime::new();
        new_session(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "claude",
        )
        .unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        std::fs::write(
            fake.dir.join("input"),
            oauth((now + 7200) * 1000).to_string(),
        )
        .unwrap();
        std::fs::write(fake.dir.join("fail_anthropic.token"), "").unwrap();
        assert!(ingest_and_route(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "claude"
        )
        .is_err());
        assert!(
            matches!(publication_status(&fake.dir, "claude", BrokerStatus::SignedOut), BrokerStatus::Unknown(ref reason) if reason == "sign-in incomplete")
        );
        std::fs::remove_file(fake.dir.join("fail_anthropic.token")).unwrap();
        assert!(matches!(
            crate::seat_refresh::refresh(
                &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
                &fake.dir,
                "claude",
                false
            )
            .unwrap(),
            crate::seat_refresh::Outcome::NotDue { .. }
        ));
        assert_eq!(
            std::fs::read(fake.dir.join("broker/anthropic.token")).unwrap(),
            b"FAKE-claude-access"
        );
        assert!(!crate::seat_refresh::pending_path(&fake.dir, "claude").exists());
    }

    #[cfg(unix)]
    #[test]
    fn sign_out_stopped_absent_and_unknown_gateway_never_mints_probe() {
        if crate::validator::skip_when_nested(
            "sign_out_stopped_absent_and_unknown_gateway_never_mints_probe",
        ) {
            return;
        }
        for state in ["stopped", "absent", "unknown"] {
            let fake = FakeRuntime::new();
            std::fs::write(fake.dir.join("state"), state).unwrap();
            crate::seat_refresh::record_last(
                &fake.dir,
                "codex",
                Some(&crate::seat_refresh::LastRefresh {
                    ok: false,
                    at: 1,
                    error: None,
                }),
            );
            let result = sign_out_and_route(
                &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
                &fake.dir,
                "codex",
            );
            assert_eq!(result.is_ok(), state != "unknown");
            assert!(!fake.calls().contains("read K;"));
            let revoking: Vec<String> = std::fs::read(fake.dir.join("config/revoking.json"))
                .ok()
                .map(|b| serde_json::from_slice(&b).unwrap())
                .unwrap_or_default();
            assert!(revoking.is_empty());
            if result.is_ok() {
                assert!(crate::seat_refresh::last_refresh(&fake.dir, "codex").is_none());
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn refresh_recovery_prefers_next_generation_and_status_becomes_signed_in() {
        if crate::validator::skip_when_nested(
            "refresh_recovery_prefers_next_generation_and_status_becomes_signed_in",
        ) {
            return;
        }
        let fake = FakeRuntime::new();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        std::fs::create_dir_all(fake.dir.join("refresh")).unwrap();
        std::fs::write(crate::seat_refresh::pending_path(&fake.dir, "claude"), "").unwrap();
        std::fs::write(
            fake.dir.join("broker/claude.credentials.json"),
            oauth(1000).to_string(),
        )
        .unwrap();
        let mut staged = oauth((now + 7200) * 1000);
        staged["claudeAiOauth"]["accessToken"] = json!("FAKE-new-access");
        staged["claudeAiOauth"]["refreshToken"] = json!("FAKE-rotated-R2");
        std::fs::write(
            fake.dir.join("broker/claude.credentials.json.next"),
            staged.to_string(),
        )
        .unwrap();
        crate::seat_refresh::record_last(
            &fake.dir,
            "claude",
            Some(&crate::seat_refresh::LastRefresh {
                ok: false,
                at: 1,
                error: Some("local write failed".into()),
            }),
        );
        assert!(matches!(
            crate::seat_refresh::refresh(
                &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
                &fake.dir,
                "claude",
                false
            )
            .unwrap(),
            crate::seat_refresh::Outcome::NotDue { .. }
        ));
        assert_eq!(
            std::fs::read_to_string(fake.dir.join("broker/claude.credentials.json")).unwrap(),
            staged.to_string()
        );
        assert_eq!(
            std::fs::read(fake.dir.join("broker/anthropic.token")).unwrap(),
            b"FAKE-new-access"
        );
        assert!(!fake
            .dir
            .join("broker/claude.credentials.json.next")
            .exists());
        assert!(matches!(
            with_last_refresh(
                broker_status(
                    &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
                    "claude"
                ),
                crate::seat_refresh::last_refresh(&fake.dir, "claude")
            ),
            BrokerStatus::SignedIn { .. }
        ));
        assert!(!fake.calls().contains(crate::seat_refresh::REFRESH_IMAGE));
    }

    #[cfg(unix)]
    #[test]
    fn sign_out_refuses_unsettled_writer_before_emptying_any_credentials() {
        if crate::validator::skip_when_nested(
            "sign_out_refuses_unsettled_writer_before_emptying_any_credentials",
        ) {
            return;
        }
        let fake = FakeRuntime::new();
        std::fs::create_dir_all(fake.dir.join("refresh")).unwrap();
        // Unique: cleanup `pkill -f`s / `pgrep -f`s the name host-wide, so a fixed one would hit
        // (or wait on) a parallel test's runtime calls carrying a name it prefixes.
        std::fs::write(
            crate::seat_gateway::unsettled_path(&fake.dir, "claude"),
            format!("wicked-mutation-FAKE{}", uuid::Uuid::new_v4().simple()),
        )
        .unwrap();
        std::fs::write(fake.dir.join("fail_cleanup"), "").unwrap();
        assert!(sign_out_and_route(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "claude"
        )
        .is_err());
        assert!(!fake.calls().contains(": > /in/"));
        assert!(crate::seat_gateway::unsettled_path(&fake.dir, "claude").exists());
        std::fs::remove_file(fake.dir.join("fail_cleanup")).unwrap();
        std::fs::write(fake.dir.join("state"), "stopped").unwrap();
        sign_out_and_route(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "claude",
        )
        .unwrap();
        assert!(!crate::seat_gateway::unsettled_path(&fake.dir, "claude").exists());
        assert!(fake.calls().contains(": > /in/anthropic.token"));
    }

    #[test]
    fn extraction_trims_and_rejects_malformed_tokens_keys_and_refresh_tokens() {
        for invalid in [
            "",
            "   ",
            "FAKE token",
            "FAKE\r\nheader",
            "FAKE\0token",
            "FAKE\ttoken",
        ] {
            let mut value = oauth(4_102_444_800_000);
            value["claudeAiOauth"]["accessToken"] = json!(invalid);
            assert!(extract_tokens("claude", &[file("claude/.credentials.json", value)]).is_err());
            let mut value = chatgpt();
            value["tokens"]["refresh_token"] = json!(invalid);
            assert!(extract_tokens("codex", &[file("codex/auth.json", value)]).is_err());
            assert!(extract_tokens(
                "codex",
                &[file(
                    "codex/auth.json",
                    json!({"auth_mode":"apikey","OPENAI_API_KEY":invalid})
                )]
            )
            .is_err());
        }
        let mut value = oauth(4_102_444_800_000);
        value["claudeAiOauth"]["accessToken"] = json!("  FAKE-trimmed  ");
        assert_eq!(
            bytes(
                &extract_tokens("claude", &[file("claude/.credentials.json", value)]).unwrap(),
                "anthropic.token"
            ),
            b"FAKE-trimmed"
        );
    }

    fn file(path: &str, value: Value) -> (String, Vec<u8>) {
        (path.into(), serde_json::to_vec(&value).unwrap())
    }

    fn bytes<'a>(files: &'a [(String, Vec<u8>)], name: &str) -> &'a [u8] {
        &files.iter().find(|(p, _)| p == name).unwrap().1
    }

    fn oauth(expiry: u64) -> Value {
        json!({"claudeAiOauth": {
            "accessToken": "FAKE-claude-access", "refreshToken": "FAKE-claude-refresh",
            "expiresAt": expiry, "scopes": ["user:inference"]
        }})
    }

    fn chatgpt() -> Value {
        json!({"auth_mode": "chatgpt", "OPENAI_API_KEY": null, "tokens": {
            "access_token": "FAKE-codex-access", "refresh_token": "FAKE-codex-refresh",
            "id_token": "FAKE-codex-id", "account_id": "FAKE-account-id"
        }, "last_refresh": "2026-01-01T00:00:00Z"})
    }

    fn jwt(expiry: Value) -> String {
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({"exp": expiry})).unwrap());
        format!("FAKE-header.{payload}.FAKE-signature")
    }

    /// core#1024: claude signs in with `claude setup-token` (the real CLI's own flow, run in the
    /// login container): the token it prints is pasted back unechoed and left for ingest, which
    /// moves it to the broker as the seat's pass-through token. No OAuth record is kept, an
    /// earlier one is emptied, and there is nothing for the broker to renew.
    #[cfg(unix)]
    #[test]
    fn a_claude_setup_token_sign_in_is_brokered_for_pass_through() {
        if crate::validator::skip_when_nested(
            "a_claude_setup_token_sign_in_is_brokered_for_pass_through",
        ) {
            return;
        }
        // Line mode with echo off before the prompt shows, so a paste typed at once is never
        // echoed and Enter ends the line whatever mode the TUI left the terminal in.
        assert!(CLAUDE_SETUP_TOKEN_LOGIN.contains("script -q -e -c 'claude setup-token'"));
        assert!(CLAUDE_SETUP_TOKEN_LOGIN
            .contains("else claude setup-token || exit 1; fi; stty sane -echo 2>/dev/null; "));
        assert!(CLAUDE_SETUP_TOKEN_LOGIN.contains("umask 077"));
        assert!(CLAUDE_SETUP_TOKEN_LOGIN.contains("> \"$CLAUDE_CONFIG_DIR/wicked-setup-token\""));
        assert_eq!(credential_paths("claude")[0], CLAUDE_SETUP_TOKEN_PATH);
        assert_eq!(
            setup_token(b"  sk-ant-oat01-FAKE-setup_token-0123\r\n").unwrap(),
            "sk-ant-oat01-FAKE-setup_token-0123"
        );
        assert_eq!(
            setup_token(b"\x1b[200~sk-ant-oat01-FAKE-setup_token-0123\x1b[201~\r").unwrap(),
            "sk-ant-oat01-FAKE-setup_token-0123",
            "a bracketed paste"
        );
        // The CLI wraps the token across two rows: a copy of both rows carries a line break
        // (and the second row's indent), CR- or LF-terminated, with blank lines around it.
        let token = fake_setup_token();
        let (row1, row2) = token.split_at(98);
        for pasted in [
            format!("\u{1b}[200~{row1}\n {row2}\u{1b}[201~\n"),
            format!("\u{1b}[200~ {row1}\r {row2}\u{1b}[201~\r"),
            format!("\n\n{token}\r\n"),
            format!("{token}\r"),
        ] {
            assert_eq!(setup_token(pasted.as_bytes()).unwrap(), token);
        }
        // A paste cut short (its closing bracket never came, or came alone) is refused: its
        // first row passes every other check, so it would land as a truncated token.
        for cut in [
            format!("\u{1b}[200~{row1}\n"),
            format!("{row1}\u{1b}[201~\n"),
            format!("\u{1b}[200~{row1}\n\u{1b}[200~{row2}\u{1b}[201~\n"),
        ] {
            let e = setup_token(cut.as_bytes()).unwrap_err();
            assert!(
                e.contains("did not finish") && !e.contains(&row1[13..40]),
                "{e}"
            );
        }
        // No pasted token and only a settings file without a credential record in it (claude
        // can write one): the operator is told the token was neither read nor pasted back.
        let err = extract_tokens(
            "claude",
            &[file("claude/settings.json", serde_json::json!({}))],
        )
        .unwrap_err();
        assert!(
            err.contains("signed out") && err.contains("nor pasted back"),
            "{err}"
        );
        for bad in [
            &b"sk-ant-api03-FAKE-not-a-setup-token"[..],
            b"sk-ant-oat01-FAKE token",
            b"sk-ant-oat01-FAKE\nx-evil: 1",
            b"https://claude.ai/oauth/FAKE",
            b"\x1b[200~\x1b[201~",
            b"",
        ] {
            let e = setup_token(bad).unwrap_err();
            assert!(!e.contains("FAKE"), "{e}");
        }
        let fake = FakeRuntime::new();
        // An earlier `claude auth login` record is in the broker.
        std::fs::write(
            fake.dir.join("broker/claude.credentials.json"),
            oauth(9_000_000_000_000).to_string(),
        )
        .unwrap();
        std::fs::write(fake.dir.join("broker/anthropic.token"), "FAKE-old-access").unwrap();
        new_session(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "claude",
        )
        .unwrap();
        std::fs::write(
            fake.dir.join("setup-token"),
            "sk-ant-oat01-FAKE-setup-0001\n",
        )
        .unwrap();
        let report = ingest_and_route(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "claude",
        )
        .unwrap();
        assert_eq!(report.files, vec!["anthropic.token".to_string()]);
        assert_eq!(report.expires_at, None);
        assert_eq!(
            std::fs::read(fake.dir.join("broker/anthropic.token")).unwrap(),
            b"sk-ant-oat01-FAKE-setup-0001"
        );
        assert!(
            std::fs::read(fake.dir.join("broker/claude.credentials.json"))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            broker_status(
                &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
                "claude"
            ),
            BrokerStatus::SignedIn { expires_at: None }
        );
        let sign_in = seat_sign_in(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "claude",
        )
        .unwrap();
        assert_eq!(
            sign_in.kind,
            Some(crate::seat_credentials::CredentialKind::ClaudeOauth)
        );
        assert_eq!(
            claude_seat_token(
                &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
                &fake.dir
            )
            .unwrap(),
            "sk-ant-oat01-FAKE-setup-0001"
        );
        assert_eq!(
            crate::seat_refresh::refresh(
                &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
                &fake.dir,
                "claude",
                true
            )
            .unwrap(),
            crate::seat_refresh::Outcome::NotRefreshable,
            "a pass-through token is never renewed by the broker"
        );
        // A bad paste is refused before the broker changes, and never repeated.
        new_session(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "claude",
        )
        .unwrap();
        std::fs::write(fake.dir.join("setup-token"), "FAKE-pasted-url").unwrap();
        let err = ingest_and_route(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "claude",
        )
        .unwrap_err();
        assert!(
            err.contains("claude setup-token") && !err.contains("FAKE"),
            "{err}"
        );
        assert_eq!(
            std::fs::read(fake.dir.join("broker/anthropic.token")).unwrap(),
            b"sk-ant-oat01-FAKE-setup-0001"
        );
        // Nothing pasted back (the window closed, or the script stopped): the error says so.
        new_session(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "claude",
        )
        .unwrap();
        std::fs::remove_file(fake.dir.join("setup-token")).unwrap();
        let err = ingest_and_route(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "claude",
        )
        .unwrap_err();
        assert!(
            err.contains("signed out") && err.contains("nor pasted back"),
            "{err}"
        );
    }

    /// A fake `claude setup-token` token, built at runtime (never a literal): its shape, length
    /// and alphabet are the real token's, so the CLI wraps it across rows the way it does.
    fn fake_setup_token() -> String {
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let body: String = (0..94u32)
            .map(|i| alphabet[(i * 37 % 64) as usize] as char)
            .collect();
        ["sk", "ant", "oat01", &body].join("-") + "AA"
    }

    /// claude leaves xterm mouse reporting on, so each click in studio's terminal sends an SGR
    /// mouse report (`ESC [ < b ; x ; y M`, then `m` on release) into the paste: before it,
    /// between its rows and after it. Those reports, like any other control sequence a terminal
    /// sends or a copy carries (a colour, a window title, a hyperlink), are dropped and the clean
    /// token remains.
    #[test]
    fn a_setup_token_paste_polluted_by_mouse_reports_is_the_clean_token() {
        let token = fake_setup_token();
        let (row1, row2) = token.split_at(98);
        let click = "\u{1b}[<0;12;5M\u{1b}[<0;12;5m";
        for pasted in [
            format!("{click}{token}{click}\r"),
            format!("{click}\u{1b}[200~{row1}\r{click} {row2}\u{1b}[201~{click}\r{click}"),
            format!("{click}\n{row1}{click}\n{row2}\n{click}\n"),
            format!("\u{1b}[<35;80;3M{row1}\u{1b}[<64;1;1M{row2}\u{1b}[<2;100;30m\r"),
            format!("\u{1b}]0;claude\u{7}\u{1b}[93m{row1}\u{1b}[0m\u{1b}]8;;\u{1b}\\{row2}\r"),
            format!("\u{1b}OA{token}\u{1b}=\r"),
        ] {
            assert_eq!(setup_token(pasted.as_bytes()).unwrap(), token, "{pasted:?}");
        }
        // A control string the paste cuts short (no BEL or ST) ends at the row break or at the
        // next ESC: it never swallows the token's next row (a truncated first row would pass
        // every other check) nor a bracketed-paste marker.
        for pasted in [
            format!("{row1}\u{1b}]11;rgb:1234\n{row2}\n\n"),
            format!("{row1}\u{1b}P>|xterm\r{row2}\r"),
            format!("\u{1b}]11;rgb:1e1e\u{1b}[200~{token}\u{1b}[201~\r"),
            format!("\u{1b}[200~{row1}\u{1b}_x\r{row2}\u{1b}[201~\r"),
        ] {
            assert_eq!(setup_token(pasted.as_bytes()).unwrap(), token, "{pasted:?}");
        }
        // The bracketed-paste markers are counted by the same stripper: a paste cut short is
        // still refused when clicks or an unterminated string surround it.
        for cut in [
            format!("{click}\u{1b}[200~{row1}{click}\r"),
            format!("\u{1b}]11;rgb:1e1e\u{1b}[200~{row1}\r"),
        ] {
            let e = setup_token(cut.as_bytes()).unwrap_err();
            assert!(e.contains("did not finish"), "{e}");
        }
    }

    /// A paste that is not one clean `claude setup-token` token is refused with the check it
    /// failed, named by category: never any part of the paste.
    #[test]
    fn each_setup_token_refusal_names_its_check_and_never_the_paste() {
        let token = fake_setup_token();
        let (row1, row2) = token.split_at(98);
        let cases = [
            (format!("Token: {token}"), "text before or after the token"),
            (format!("{token}."), "text before or after the token"),
            (format!("{row1} {row2}"), "a space inside the token"),
            (
                format!("sk-ant-api03-{}", &token[13..]),
                "does not contain an sk-ant-oat",
            ),
            (
                format!("{token}{}", "A".repeat(4096)),
                "longer than a token",
            ),
            (format!("{token}\r\n{token}"), "appears more than once"),
            ("sk-ant-oat01-A".to_string(), "shorter than a token"),
            (format!("\u{1b}[200~{row1}\r"), "did not finish"),
        ];
        let mut messages = std::collections::HashSet::new();
        for (pasted, check) in &cases {
            let e = setup_token(pasted.as_bytes()).unwrap_err();
            assert!(e.contains(check), "{check}: {e}");
            assert!(e.contains("sign claude in again"), "{check}: {e}");
            assert!(
                !e.contains(&token[13..40]) && !e.contains(&row1[70..95]) && !e.contains("AAAA"),
                "{check}: {e}"
            );
            messages.insert(e);
        }
        assert_eq!(messages.len(), cases.len() - 1, "one message per check");
    }

    /// The claude login script as the operator meets it (the sign-in that stayed signed out):
    /// run in a real PTY, after a stand-in `claude setup-token` that prints the token wrapped
    /// across two rows the way its TUI does and leaves the terminal raw, with the modes claude
    /// sets left on (mouse reporting, the kitty keyboard protocol, modifyOtherKeys, synchronized
    /// output; it leaves only the alternate screen). The script turns them off after it, so a
    /// drag selects again. The paste is sent the way studio's terminal (xterm.js) sends it: row
    /// breaks as CR, bracketed because the script turned bracketed paste on, then Enter as CR,
    /// with clicks and a terminal's answers to claude's queries around it. The whole token must
    /// land, every time, under `sh` and under `dash` (the seat image's `/bin/sh`) where it exists.
    #[cfg(unix)]
    #[test]
    fn the_claude_login_script_captures_a_wrapped_token_pasted_in_a_pty() {
        if crate::validator::skip_when_nested(
            "the_claude_login_script_captures_a_wrapped_token_pasted_in_a_pty",
        ) {
            return;
        }
        use portable_pty::{CommandBuilder, PtySize};
        use std::io::{Read, Write};
        let token = fake_setup_token();
        let (row1, row2) = token.split_at(98);
        let root = std::env::temp_dir().join(format!(
            "wicked-setup-token-pty-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let stub = bin.join("claude");
        std::fs::write(
            &stub,
            "#!/bin/sh\n[ \"$1\" = setup-token ] || exit 2\nstty raw -echo\n\
             printf '\\033[?1049h\\033[?1000h\\033[?1006h\\033[>1u\\033[>4;2m\\033[?2026h\\033[?2004h \\033[93m%s\\r\\033[1C\\033[1B%s\\r\\n\\033[?1049l' \"$ROW1\" \"$ROW2\"\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );

        // Each case: what the operator does at the prompt, and whether the whole token lands.
        // A terminal that does not bracket pastes ends the token with one Enter more; EOF (^D)
        // inside an open paste saves nothing.
        let cases: [(&str, String, Result<String, &str>); 12] = [
            (
                "the terminal's answers to claude's queries, then a bracketed paste",
                format!(
                    "\u{1b}[12;1R\u{1b}[?1;2c\u{1b}]11;rgb:1e1e/1e1e/1e1e\u{1b}\\\u{1b}[200~{token}\u{1b}[201~\r"
                ),
                Ok(token.clone()),
            ),
            (
                "a line of clicks alone ends an unbracketed paste",
                format!("{token}\r\u{1b}[<0;12;5M\u{1b}[<0;12;5m\r"),
                Ok(token.clone()),
            ),
            (
                "clicks around a bracketed paste",
                format!(
                    "\u{1b}[<0;12;5M\u{1b}[<0;12;5m\u{1b}[200~{row1}\r {row2}\u{1b}[201~\u{1b}[<0;3;7M\r"
                ),
                Ok(token.clone()),
            ),
            (
                "both rows copied, bracketed",
                format!("\u{1b}[200~{row1}\r {row2}\u{1b}[201~\r"),
                Ok(token.clone()),
            ),
            (
                "a blank Enter first, then the paste",
                format!("\r\u{1b}[200~{token}\u{1b}[201~\r"),
                Ok(token.clone()),
            ),
            (
                "the copy starts on the blank row above",
                format!("\u{1b}[200~\r{row1}\r {row2}\u{1b}[201~\r"),
                Ok(token.clone()),
            ),
            (
                "the copy ends with a line break, then Enter",
                format!("\u{1b}[200~{row1}\r {row2}\r\u{1b}[201~\r"),
                Ok(token.clone()),
            ),
            (
                "one row, unbracketed",
                format!("{token}\r\r"),
                Ok(token.clone()),
            ),
            (
                "both rows copied, unbracketed",
                format!("{row1}\r {row2}\r\r"),
                Ok(token.clone()),
            ),
            (
                "a middle row of digits and dashes only, unbracketed",
                format!("{row1}\r 0123-_\r {row2}\r\r"),
                Ok(format!("{row1}0123-_{row2}")),
            ),
            (
                "the input ends inside the paste",
                format!("\u{1b}[200~{row1}\r\u{4}"),
                Err("the paste did not finish"),
            ),
            (
                "a row of digits alone, then Enter, unbracketed",
                "0123-_\r\r".to_string(),
                Err("no token was pasted"),
            ),
        ];
        let shells: Vec<&str> = ["sh", "dash"]
            .into_iter()
            .filter(|sh| {
                *sh == "sh"
                    || ["/bin", "/usr/bin"]
                        .iter()
                        .any(|d| std::path::Path::new(d).join(sh).exists())
            })
            .collect();
        let runs = shells
            .iter()
            .flat_map(|sh| cases.iter().map(move |c| (*sh, c)));
        for (n, (shell, (case, paste, lands))) in runs.enumerate() {
            let case = format!("{shell}: {case}");
            let config = root.join(format!("home-{n}/claude"));
            let pair = portable_pty::native_pty_system()
                .openpty(PtySize {
                    rows: 30,
                    cols: 100,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .unwrap();
            let mut cmd = CommandBuilder::new(shell);
            cmd.args(["-c", CLAUDE_SETUP_TOKEN_LOGIN, "wicked-login"]);
            cmd.env("PATH", &path);
            cmd.env("CLAUDE_CONFIG_DIR", &config);
            cmd.env("ROW1", row1);
            cmd.env("ROW2", row2);
            let mut child = pair.slave.spawn_command(cmd).unwrap();
            drop(pair.slave);
            let mut reader = pair.master.try_clone_reader().unwrap();
            let mut writer = pair.master.take_writer().unwrap();
            let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = seen.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                while let Ok(n) = reader.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    sink.lock().unwrap().extend_from_slice(&buf[..n]);
                }
            });
            let shown = |what: &str| {
                // (core#1194) A failure guard: the wait is on the output (20 s ran out at load 20).
                let deadline = std::time::Instant::now() + crate::test_env::PATIENT;
                while std::time::Instant::now() < deadline {
                    if String::from_utf8_lossy(&seen.lock().unwrap()).contains(what) {
                        return true;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                false
            };
            assert!(shown("press Enter: "), "{case}: no prompt");
            // This stand-in prints no "Your OAuth token" screen: the script cannot read the token
            // from claude's output, says so, and asks for the paste (the fallback).
            assert!(
                String::from_utf8_lossy(&seen.lock().unwrap())
                    .contains("could not read the token from claude's output; paste it instead"),
                "{case}: the fallback did not say why"
            );
            // The script turned bracketed paste on with its prompt (the stand-in's own does not count).
            let out = String::from_utf8_lossy(&seen.lock().unwrap()).into_owned();
            assert!(
                out.contains("\u{1b}[?2004h\r\nwicked: copy"),
                "{case}: bracketed paste off"
            );
            // ...and turned the modes claude left on off after claude ran, before its prompt
            // (never the alternate screen: the stand-in left it itself).
            let mouse_off = out
                .find(
                    "\u{1b}[?1000l\u{1b}[?1002l\u{1b}[?1003l\u{1b}[?1005l\u{1b}[?1006l\
                     \u{1b}[?1015l\u{1b}[<u\u{1b}[=0;1u\u{1b}[>4;0m\u{1b}[?2026l",
                )
                .unwrap_or_else(|| panic!("{case}: claude's terminal modes left on"));
            assert_eq!(
                out.matches("\u{1b}[?1049l").count(),
                1,
                "{case}: left the alternate screen"
            );
            assert!(
                out.find(row2).unwrap() < mouse_off
                    && mouse_off < out.find("wicked: copy").unwrap(),
                "{case}: mouse reporting off out of order"
            );
            writer.write_all(paste.as_bytes()).unwrap();
            writer.flush().unwrap();
            let expected = match lands {
                Ok(expected) => expected,
                Err(refusal) => {
                    assert!(shown(refusal), "{case}: no refusal");
                    let status = child.wait().unwrap();
                    assert!(!status.success(), "{case}: {status:?}");
                    assert!(!config.join("wicked-setup-token").exists(), "{case}: saved");
                    continue;
                }
            };
            assert!(shown("token received"), "{case}: the script never finished");
            let status = child.wait().unwrap();
            assert!(status.success(), "{case}: {status:?}");
            let landed = std::fs::read(config.join("wicked-setup-token")).unwrap();
            assert_eq!(&setup_token(&landed).unwrap(), expected, "{case}");
            let out = String::from_utf8_lossy(&seen.lock().unwrap()).into_owned();
            assert!(
                !out[out.find("press Enter: ").unwrap()..].contains(&token[13..40]),
                "{case}: the paste was echoed"
            );
        }

        // Nothing pasted (the input ends): no file, and the script says so before it exits.
        let config = root.join("home-eof/claude");
        use wicked_apps_core::HardenedCommand;
        let out = std::process::Command::new("sh")
            .hardened()
            .args(["-c", CLAUDE_SETUP_TOKEN_LOGIN, "wicked-login"])
            .env("PATH", &path)
            .env("CLAUDE_CONFIG_DIR", &config)
            .env("ROW1", row1)
            .env("ROW2", row2)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("no token was pasted"));
        assert!(!config.join("wicked-setup-token").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// claude 2.1.287's closing screen of `claude setup-token`, on the main screen after its TUI
    /// exits (Ink Text rows; FAKE token): the token in yellow, wrapped at `cols` onto rows indented
    /// to the token's own column, each row ended by `nl`.
    #[cfg(unix)]
    fn claude_token_screen(token: &str, cols: usize, indent: usize, nl: &str) -> String {
        let pad = " ".repeat(indent);
        let mut lines = vec![
            format!("{pad}\u{1b}[32m\u{2713}\u{1b}[39m Long-lived authentication token created successfully!"),
            String::new(),
            format!("{pad}Your OAuth token (valid for 1 year):"),
            String::new(),
        ];
        lines.extend(token.as_bytes().chunks(cols - indent).map(|row| {
            format!(
                "{pad}\u{1b}[33m{}\u{1b}[39m",
                std::str::from_utf8(row).unwrap()
            )
        }));
        lines.extend([
            String::new(),
            format!("{pad}Store this token securely. You won't be able to see it again."),
            String::new(),
            format!("{pad}Use this token by setting: export CLAUDE_CODE_OAUTH_TOKEN=<token>"),
        ]);
        lines.join(nl) + nl
    }

    /// claude's TUI before the closing screen: the alternate screen with its modes on, Ink frames
    /// erased and redrawn by cursor moves, and its own prompt with the (FAKE) browser code typed.
    #[cfg(unix)]
    const CLAUDE_TUI: &str = "\u{1b}[?1049h\u{1b}[?1000h\u{1b}[?1006h\u{1b}[>1u\u{1b}[?2026h\
        \u{1b}[2J\u{1b}[HOpening your browser to sign in\u{2026}\r\nPaste code here if prompted > \
        \u{1b}[2K\u{1b}[1A\u{1b}[2K\u{1b}[GBrowser didn't open? Use the URL below\r\n\
        Paste code here if prompted > FAKE-browser-code\u{1b}[?2026l\r\n\u{1b}[?1049l";

    /// An Ink frame on the main screen (a spinner row, CR-redrawn) that the next frame erases the
    /// way Ink's log-update does (`ESC [ 2K ESC [ 1A ... ESC [ G`).
    #[cfg(unix)]
    fn erased_frame(next: &str) -> String {
        format!(
            "\u{1b}[33m\u{280b}\u{1b}[39m Creating your token\u{2026}\r\u{1b}[33m\u{2819}\u{1b}[39m Creating your token\u{2026}\r\n\
             \u{1b}[2K\u{1b}[1A\u{1b}[2K\u{1b}[G{next}"
        )
    }

    /// The lines util-linux `script -q` still writes around a capture (2.41.5 in the seat image).
    #[cfg(unix)]
    const SCRIPT_START: &str =
        "Script started on 2026-10-19 18:31:47+00:00 [COMMAND=\"claude setup-token\" \
        TERM=\"xterm-256color\" TTY=\"/dev/pts/0\" COLUMNS=\"100\" LINES=\"30\"]\n";
    #[cfg(unix)]
    const SCRIPT_DONE: &str =
        "\nScript done on 2026-10-19 18:31:48+00:00 [COMMAND_EXIT_CODE=\"0\"]\n";

    /// The extractor run on `capture` at terminal size `size` ("ROWS COLS"): the token it wrote, or
    /// `None` when it found none (and then it wrote nothing).
    #[cfg(unix)]
    fn extract_claude_token(capture: &str, size: &str) -> Option<String> {
        use wicked_apps_core::HardenedCommand;
        let dir = std::env::temp_dir().join(format!(
            "wicked-claude-extract-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let (capture_file, dest) = (dir.join("claude.out"), dir.join("wicked-setup-token"));
        std::fs::write(&capture_file, capture).unwrap();
        let status = std::process::Command::new("python3")
            .hardened()
            .args(["-c", claude_token_extractor!()])
            .arg(&capture_file)
            .arg(size)
            .arg(&dest)
            .status()
            .expect("python3 (the seat image installs it)");
        let token = if status.success() {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dest).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "the token file is owner-only");
            Some(std::fs::read_to_string(&dest).unwrap())
        } else {
            assert!(!dest.exists(), "a refusal wrote a token file");
            None
        };
        let _ = std::fs::remove_dir_all(&dir);
        token
    }

    /// The token is read from claude's own output, so the operator never pastes it.
    /// Realistic captures (claude's layout, ANSI colours, CR/LF or LF rows, Ink frames erased by
    /// cursor moves, the alternate screen): the exact token when it is wrapped at 80 and 100
    /// columns or unwrapped at 140, and no token (the paste prompt) whenever it is not certain.
    #[cfg(unix)]
    #[test]
    fn the_claude_token_is_read_from_claudes_own_output() {
        let token = fake_setup_token();
        let other = format!("{}xyz", &token[..token.len() - 3]);
        let screen = |cols, indent| claude_token_screen(&token, cols, indent, "\r\n");
        let reads: Vec<(&str, String, String)> = vec![
            ("wrapped at 80", format!("{CLAUDE_TUI}{}", erased_frame(&screen(80, 0))), "30 80".into()),
            ("wrapped at 80, indented", format!("{CLAUDE_TUI}{}", screen(80, 2)), "30 80".into()),
            ("wrapped at 100", format!("{CLAUDE_TUI}{}", erased_frame(&screen(100, 0))), "30 100".into()),
            ("wrapped at 100, indented", format!("{CLAUDE_TUI}{}", screen(100, 2)), "40 100".into()),
            ("unwrapped at 140", format!("{CLAUDE_TUI}{}", erased_frame(&screen(140, 2))), "30 140".into()),
            ("LF rows", claude_token_screen(&token, 80, 0, "\n"), "30 80".into()),
            ("three rows at 40", claude_token_screen(&token, 40, 2, "\r\n"), "30 40".into()),
            (
                "framed by util-linux script's own lines",
                format!("{SCRIPT_START}{CLAUDE_TUI}{}{SCRIPT_DONE}", screen(100, 0)),
                "30 100".into(),
            ),
            (
                "the screen drawn twice (a redraw that was not erased)",
                format!("{}{}", screen(100, 0), screen(100, 0)),
                "30 100".into(),
            ),
            (
                "the token fills its row exactly, a blank row below",
                format!("Your OAuth token (valid for 1 year):\r\n\r\n\u{1b}[33m{token}\u{1b}[39m\r\n\r\nStore this token securely.\r\n"),
                format!("30 {}", token.len()),
            ),
            (
                "the terminal wrapped it (autowrap, no row break)",
                format!("Your OAuth token (valid for 1 year):\r\n\r\n\u{1b}[33m{token}\u{1b}[39m\r\n\r\nStore this token securely.\r\n"),
                "30 80".into(),
            ),
        ];
        for (case, capture, size) in &reads {
            let read = extract_claude_token(capture, size);
            assert_eq!(read.as_deref(), Some(token.as_str()), "{case}");
            assert_eq!(
                setup_token(read.unwrap().as_bytes()).unwrap(),
                token,
                "{case}"
            );
        }

        let (row1, row2) = token.split_at(80);
        let exact = format!("30 {}", token.len());
        let none: Vec<(&str, String, &str)> = vec![
            ("no token on the screen", CLAUDE_TUI.into(), "30 80"),
            ("nothing captured", String::new(), "30 80"),
            (
                "two different tokens",
                format!(
                    "{}{}",
                    screen(140, 0),
                    claude_token_screen(&other, 140, 0, "\r\n")
                ),
                "30 140",
            ),
            // The continuation row is only joined at the width the token was wrapped at.
            ("a wrong terminal width", screen(80, 0), "30 120"),
            ("no terminal size", screen(80, 0), ""),
            (
                "the next row starts at another column",
                format!(
                    "Your OAuth token (valid for 1 year):\r\n\r\n  {}\r\n{row2}\r\n",
                    &row1[..78]
                ),
                "30 80",
            ),
            // After a full-width row only a blank row or a clean continuation may follow: anything
            // else is not taken to end the token (that would save a truncated one).
            (
                "it ends on a full-width row, then util-linux script's footer",
                format!("{SCRIPT_START}Your OAuth token (valid for 1 year):\r\n\r\n{row1}\r\n{SCRIPT_DONE}"),
                "30 80",
            ),
            (
                "it ends on a full-width row, below a taller Ink frame it erased",
                format!(
                    "{}{}Your OAuth token (valid for 1 year):\r\n\r\n{row1}\r\n",
                    "Paste code here if prompted >\r\n".repeat(8),
                    "\u{1b}[2K\u{1b}[1A".repeat(8) + "\u{1b}[2K\u{1b}[G"
                ),
                "30 80",
            ),
            (
                "the capture ends on a full-width row",
                format!("Your OAuth token (valid for 1 year):\r\n\r\n{row1}\r\n"),
                "30 80",
            ),
            (
                "an unwrapped token with no terminal size",
                screen(140, 0),
                "",
            ),
            (
                "text after the continuation",
                format!("Your OAuth token (valid for 1 year):\r\n\r\n{row1}\r\n{row2} (copy it)\r\n"),
                "30 80",
            ),
            (
                "text before the continuation",
                format!("Your OAuth token (valid for 1 year):\r\n\r\n{row1}\r\ntoken: {row2}\r\n"),
                "30 80",
            ),
            (
                "a full-width row, then \"Store this token\" right below",
                format!("Your OAuth token (valid for 1 year):\r\n\r\n{token}\r\nStore this token securely.\r\n"),
                exact.as_str(),
            ),
            (
                "text after the token on its row",
                format!(
                    "Your OAuth token (valid for 1 year):\r\n\r\n{} (copy it)\r\n",
                    &token[..60]
                ),
                "30 140",
            ),
            (
                "text before the token on its row",
                format!(
                    "Your OAuth token (valid for 1 year):\r\n\r\ntoken: {}\r\n",
                    &token[..60]
                ),
                "30 140",
            ),
            (
                "a row of token characters, not a token",
                "Your OAuth token (valid for 1 year):\r\n\r\nsk-ant-oat01-short\r\n".into(),
                "30 140",
            ),
        ];
        for (case, capture, size) in &none {
            assert_eq!(extract_claude_token(capture, size), None, "{case}");
        }
        // "Store this token…" is never joined, whatever the width: it is not token characters.
        let joined = extract_claude_token(
            &format!("Your OAuth token (valid for 1 year):\r\n\r\n{row1}\r\n{row2}\r\nStore this token securely.\r\n"),
            "30 80",
        );
        assert_eq!(joined.as_deref(), Some(token.as_str()));
    }

    /// The claude login as the operator meets it: a stand-in `claude setup-token`
    /// prints claude's closing screen in a real PTY, the login script reads the token from that
    /// output and saves exactly it, and the capture is gone; nothing asks for a paste. When the
    /// token cannot be read, the script says so and the paste prompt (unchanged) takes it. Under
    /// `sh` and `dash` (the seat image's `/bin/sh`) where it exists, and util-linux `script`
    /// (the seat image's). Where the host's `script` is BSD's (macOS), a stand-in with
    /// util-linux's arguments runs BSD's.
    #[cfg(unix)]
    #[test]
    fn the_claude_login_script_reads_the_token_from_claudes_output_in_a_pty() {
        if crate::validator::skip_when_nested(
            "the_claude_login_script_reads_the_token_from_claudes_output_in_a_pty",
        ) {
            return;
        }
        use portable_pty::{CommandBuilder, PtySize};
        use std::io::{Read, Write};
        use std::os::unix::fs::PermissionsExt;
        use wicked_apps_core::HardenedCommand;
        let token = fake_setup_token();
        let root = std::env::temp_dir().join(format!(
            "wicked-claude-autocapture-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let util_linux = std::process::Command::new("script")
            .hardened()
            .arg("-V")
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("util-linux"));
        if !util_linux {
            let bsd = std::path::Path::new("/usr/bin/script");
            assert!(bsd.exists(), "no script(1) on this host");
            std::fs::write(
                bin.join("script"),
                "#!/bin/sh\n# util-linux's `script -q -e -c CMD FILE` over BSD script (test only)\n\
                 [ \"$1\" = -V ] && { echo 'script from util-linux (BSD stand-in)'; exit 0; }\n\
                 /usr/bin/script -q \"$5.raw\" /bin/sh -c \"$4\"; rc=$?\n\
                 # util-linux frames the capture with its own lines even under -q: so does this.\n\
                 { printf 'Script started on 2026-10-19 [COMMAND=\"%s\"]\\n' \"$4\"; cat \"$5.raw\"; \
                 printf '\\nScript done on 2026-10-19 [COMMAND_EXIT_CODE=\"%s\"]\\n' \"$rc\"; } > \"$5\"\n\
                 rm -f \"$5.raw\"; exit $rc\n",
            )
            .unwrap();
            std::fs::set_permissions(bin.join("script"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        // The stand-in prints claude's closing screen at the terminal's width (FAKE token),
        // after a TUI on the alternate screen, and leaves the terminal raw with its modes on.
        std::fs::write(
            bin.join("claude-screen"),
            claude_token_screen(&token, 100, 0, "\r\n"),
        )
        .unwrap();
        std::fs::write(bin.join("claude-tui"), CLAUDE_TUI).unwrap();
        std::fs::write(
            bin.join("claude"),
            "#!/bin/sh\n[ \"$1\" = setup-token ] || exit 2\nstty raw -echo\n\
             d=$(dirname \"$0\"); cat \"$d/claude-tui\"\n\
             if [ -n \"$GARBLED\" ]; then printf 'Your OAuth token (valid for 1 year):\\r\\n\\r\\n  %s\\r\\n%s\\r\\n' \"$ROW1\" \"$ROW2\"; \
             elif [ -n \"$TRUNCATED\" ]; then printf 'Your OAuth token (valid for 1 year):\\r\\n\\r\\n%s\\r\\n' \"$ROW1\"; exit 0; \
             else cat \"$d/claude-screen\"; fi\nprintf '\\033[?1000h\\033[?1006h\\033[>1u'\n",
        )
        .unwrap();
        std::fs::set_permissions(bin.join("claude"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let shells: Vec<&str> = ["sh", "dash"]
            .into_iter()
            .filter(|sh| {
                *sh == "sh"
                    || ["/bin", "/usr/bin"]
                        .iter()
                        .any(|d| std::path::Path::new(d).join(sh).exists())
            })
            .collect();
        let (row1, row2) = token.split_at(98);
        // "read": claude's screen; "garbled": the last row at another column; "truncated": claude
        // stops after a full-width first row (through util-linux `script`, whose footer must not
        // read as the blank row that ends the token).
        for (n, (shell, mode)) in shells
            .iter()
            .flat_map(|sh| [(*sh, "read"), (*sh, "garbled"), (*sh, "truncated")])
            .enumerate()
        {
            let garbled = mode != "read";
            let case = format!("{shell}, {mode}");
            let config = root.join(format!("home-{n}/claude"));
            let tmp = root.join(format!("tmp-{n}"));
            std::fs::create_dir_all(&tmp).unwrap();
            let pair = portable_pty::native_pty_system()
                .openpty(PtySize {
                    rows: 30,
                    cols: 100,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .unwrap();
            let mut cmd = CommandBuilder::new(shell);
            cmd.args(["-c", CLAUDE_SETUP_TOKEN_LOGIN, "wicked-login"]);
            cmd.env("PATH", &path);
            cmd.env("CLAUDE_CONFIG_DIR", &config);
            cmd.env("TMPDIR", &tmp);
            match mode {
                "garbled" => {
                    cmd.env("GARBLED", "1");
                    cmd.env("ROW1", row1);
                    cmd.env("ROW2", row2);
                }
                "truncated" => {
                    cmd.env("TRUNCATED", "1");
                    cmd.env("ROW1", &token[..100]);
                }
                _ => {}
            }
            let mut child = pair.slave.spawn_command(cmd).unwrap();
            drop(pair.slave);
            let mut reader = pair.master.try_clone_reader().unwrap();
            let mut writer = pair.master.take_writer().unwrap();
            let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = seen.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                while let Ok(n) = reader.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    sink.lock().unwrap().extend_from_slice(&buf[..n]);
                }
            });
            let text = || String::from_utf8_lossy(&seen.lock().unwrap()).into_owned();
            let shown = |what: &str| {
                // (core#1194) A failure guard: the wait is on the output (20 s ran out at load 20).
                let deadline = std::time::Instant::now() + crate::test_env::PATIENT;
                while std::time::Instant::now() < deadline {
                    if text().contains(what) {
                        return true;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                false
            };
            if garbled {
                assert!(shown("press Enter: "), "{case}: no paste prompt");
                assert!(
                    text().contains("could not read the token from claude's output"),
                    "{case}: the fallback did not say why"
                );
                writer
                    .write_all(format!("\u{1b}[200~{token}\u{1b}[201~\r").as_bytes())
                    .unwrap();
                writer.flush().unwrap();
            }
            assert!(
                shown(claude_token_received!()),
                "{case}: the script never finished"
            );
            let status = child.wait().unwrap();
            assert!(status.success(), "{case}: {status:?}");
            let landed = std::fs::read(config.join("wicked-setup-token")).unwrap();
            assert_eq!(setup_token(&landed).unwrap(), token, "{case}");
            if !garbled {
                assert_eq!(landed, token.as_bytes(), "{case}: only the token is saved");
                assert!(
                    !text().contains("paste it here"),
                    "{case}: asked for a paste"
                );
            }
            // The capture never outlives the script, and the script never shows the token: the
            // only copy on screen is claude's own.
            assert_eq!(
                std::fs::read_dir(&tmp).unwrap().count(),
                0,
                "{case}: capture left"
            );
            assert_eq!(
                text().matches(&token[..60]).count(),
                1,
                "{case}: the token shown again"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// core#1024: a claude record from an earlier `claude auth login` sign-in is never renewed
    /// (the broker makes no token request): when it falls due, the refresh fails with a plain
    /// reason the status carries, and no request leaves the host.
    #[cfg(unix)]
    #[test]
    fn a_due_claude_oauth_record_fails_with_sign_in_again_and_no_request() {
        let fake = FakeRuntime::new();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        std::fs::write(
            fake.dir.join("broker/claude.credentials.json"),
            oauth((now + 300) * 1000).to_string(),
        )
        .unwrap();
        std::fs::write(
            fake.dir.join("broker/anthropic.token"),
            "FAKE-claude-access",
        )
        .unwrap();
        std::fs::write(fake.dir.join("calls"), "").unwrap();
        let err = crate::seat_refresh::refresh(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "claude",
            false,
        )
        .unwrap_err();
        assert_eq!(err, crate::seat_refresh::CLAUDE_SIGN_IN_AGAIN);
        assert!(matches!(
            with_last_refresh(
                broker_status(&crate::seat_login::DockerIo::new(&fake.rt, &fake.gw), "claude"),
                crate::seat_refresh::last_refresh(&fake.dir, "claude")
            ),
            BrokerStatus::RefreshFailing { ref reason, .. } if reason == crate::seat_refresh::CLAUDE_SIGN_IN_AGAIN
        ));
        assert!(!fake.calls().contains(crate::seat_refresh::REFRESH_IMAGE));
        assert_eq!(
            std::fs::read(fake.dir.join("broker/anthropic.token")).unwrap(),
            b"FAKE-claude-access",
            "the record is served until it expires"
        );
    }

    /// The host login argv (DES-SEAT-LOGIN-SUBSCRIPTIONS "Host-side login"): `env -i` keeps only
    /// the passthrough list, every home variable points into the throwaway home, the CLI runs in
    /// that home, and codex is told to store its credential in a file (never the keychain).
    #[cfg(unix)]
    #[test]
    fn host_login_argv_isolates_every_home_and_forwards_no_secret() {
        let daemon: Vec<(String, String)> = [
            ("PATH", "/opt/bin:/usr/bin"),
            ("LANG", "en_US.UTF-8"),
            ("HTTPS_PROXY", "http://proxy.example:3128"),
            ("HOME", "/FAKE-operator-home"),
            ("CODEX_HOME", "/FAKE-operator-home/.codex"),
            ("OPENAI_API_KEY", "FAKE-secret"),
            ("GH_TOKEN", "FAKE-secret"),
            ("ANTHROPIC_API_KEY", "FAKE-secret"),
            ("XDG_CONFIG_HOME", "/FAKE-operator-home/.config"),
            ("OPENCODE_DB", "/FAKE-operator-home/db"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let home = Path::new("/state/gateway/login/wicked-agw-x-login-codex-0000abcd");
        for (cli, homes, tail) in [
            (
                "codex",
                vec!["CODEX_HOME=/state/gateway/login/wicked-agw-x-login-codex-0000abcd/codex"],
                vec![
                    "/opt/bin/codex",
                    "login",
                    "-c",
                    "cli_auth_credentials_store=\"file\"",
                ],
            ),
            (
                "pi",
                vec![
                    "PI_CODING_AGENT_DIR=/state/gateway/login/wicked-agw-x-login-codex-0000abcd/pi",
                ],
                vec![
                    "/opt/bin/pi",
                    "--no-tools",
                    "--no-extensions",
                    "--no-mcp",
                    "--no-skills",
                    "--no-context-files",
                    "--no-prompt-templates",
                    "--no-session",
                    "--no-approve",
                ],
            ),
            ("opencode", vec![], vec!["/opt/bin/opencode"]),
        ] {
            let exe = Path::new("/opt/bin").join(cli);
            let argv = host_login_argv(cli, &exe, home, Path::new("/state/PID"), "BANNER", &daemon);
            assert_eq!(&argv[..2], ["/usr/bin/env", "-i"], "{cli}");
            let sh = argv.iter().position(|a| a == "/bin/sh").unwrap();
            let vars = &argv[2..sh];
            let h = home.display();
            for want in [
                "PATH=/opt/bin:/usr/bin".to_string(),
                "LANG=en_US.UTF-8".into(),
                "HTTPS_PROXY=http://proxy.example:3128".into(),
                "TERM=xterm-256color".into(),
                format!("HOME={h}"),
                format!("TMPDIR={h}/tmp"),
                format!("XDG_CONFIG_HOME={h}/config"),
                format!("XDG_DATA_HOME={h}/data"),
                format!("XDG_STATE_HOME={h}/state"),
                format!("XDG_CACHE_HOME={h}/cache"),
            ]
            .into_iter()
            .chain(homes.iter().map(|s| s.to_string()))
            {
                assert!(vars.contains(&want), "{cli}: {want} in {vars:?}");
            }
            // One value per variable: the operator's own homes never survive.
            for var in ["HOME=", "XDG_CONFIG_HOME=", "CODEX_HOME="] {
                assert!(
                    vars.iter().filter(|v| v.starts_with(var)).count() <= 1,
                    "{cli}: {var}"
                );
            }
            let encoded = serde_json::to_string(&argv).unwrap();
            for forbidden in [
                "FAKE-secret",
                "/FAKE-operator-home",
                "OPENCODE_DB",
                "GH_TOKEN",
            ] {
                assert!(!encoded.contains(forbidden), "{cli}: {forbidden}");
            }
            assert_eq!(argv[sh + 1], "-c");
            assert_eq!(argv[sh + 3], "wicked-login");
            assert_eq!(argv[sh + 4], "BANNER");
            assert_eq!(argv[sh + 5], "/state/PID");
            assert_eq!(&argv[sh + 6..], tail.as_slice(), "{cli}");
        }
        // opencode: its own private server, never the operator's background service.
        let oc = host_login_argv(
            "opencode",
            Path::new("/o"),
            home,
            Path::new("/p"),
            "B",
            &daemon,
        );
        let script = &oc[oc.iter().position(|a| a == "-c").unwrap() + 1];
        assert!(script.contains("\"$oc\" auth login --standalone &&"));
        assert!(script.contains("\"$oc\" auth export --standalone >"));
        assert!(script.starts_with("cd \"$HOME\" || exit 1;"));
    }

    #[test]
    fn host_login_banner_names_both_versions_and_the_next_step() {
        assert_eq!(parse_version("codex-cli 0.162.1\n"), Some("0.162.1".into()));
        assert_eq!(parse_version("opencode v2.0.25"), Some("2.0.25".into()));
        assert_eq!(parse_version("1.1.0"), Some("1.1.0".into()));
        assert_eq!(parse_version("copilot version: 1.34"), None);
        assert_eq!(pinned_version("codex"), Some("0.162.1"));
        let same = host_login_banner("codex", "0.162.1", Some("0.162.1"));
        assert!(
            same.contains("codex 0.162.1 (the version the seats run)"),
            "{same}"
        );
        let skew = host_login_banner("pi", "1.3.0", Some("1.1.0"));
        assert!(skew.contains("WARNING: the seats run pi 1.1.0"), "{skew}");
        assert!(skew.contains("/login"));
        assert!(host_login_banner("opencode", "2.0.25", None).contains("Codex browser (legacy)"));
        for cli in ["claude", "copilot"] {
            assert!(!host_login(cli), "{cli} keeps its login container");
        }
    }

    /// A host login session: a fresh 0700 throwaway home with every directory its environment
    /// names (codex refuses a `CODEX_HOME` that does not exist), under the gateway directory;
    /// a new session sweeps the previous one and any orphaned home; ingest reads only the
    /// recorded session's home and removes it.
    #[cfg(unix)]
    #[test]
    fn host_login_sessions_own_a_private_home_and_remove_it() {
        if crate::validator::skip_when_nested(
            "host_login_sessions_own_a_private_home_and_remove_it",
        ) {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let fake = FakeRuntime::new();
        let first = new_session(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "codex",
        )
        .unwrap();
        let home = login_home(&fake.dir, &first);
        for d in std::iter::once(home.clone()).chain(host_login_dirs(&home, "codex")) {
            let mode = std::fs::metadata(&d).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{}", d.display());
        }
        assert!(home.join("codex").is_dir());
        let orphan = login_home(&fake.dir, &format!("{}-login-codex-0000abcd", fake.gw.name));
        std::fs::create_dir_all(orphan.join("codex")).unwrap();
        let other = login_home(&fake.dir, &format!("{}-login-pi-0000abcd", fake.gw.name));
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(home.join("codex/auth.json"), chatgpt().to_string()).unwrap();
        let second = new_session(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "codex",
        )
        .unwrap();
        assert!(
            !home.exists() && !orphan.exists(),
            "previous and orphaned homes are swept"
        );
        assert!(other.exists(), "another agent's login is left alone");
        assert!(!fake.calls().contains("volume create"), "no login volume");
        host_input(&fake, "codex", "codex/auth.json", &chatgpt().to_string());
        ingest_and_route(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "codex",
        )
        .unwrap();
        assert_eq!(
            std::fs::read(fake.dir.join("broker/openai.token")).unwrap(),
            b"FAKE-codex-access"
        );
        assert!(!login_home(&fake.dir, &second).exists());
        assert_eq!(read_session(&fake.dir, "codex").unwrap(), None);
    }

    /// A cancelled or failed host login (nothing usable in its home) and a home that smuggles a
    /// symlink both end the session: the home is removed before any broker change.
    #[cfg(unix)]
    #[test]
    fn a_host_login_that_left_nothing_usable_is_cleaned_up_before_any_broker_change() {
        let fake = FakeRuntime::new();
        let session = new_session(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "pi",
        )
        .unwrap();
        std::fs::write(fake.dir.join("calls"), "").unwrap();
        let err = ingest_and_route(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "pi",
        )
        .unwrap_err();
        assert!(err.contains("pi"), "{err}");
        assert!(!login_home(&fake.dir, &session).exists());
        assert_eq!(read_session(&fake.dir, "pi").unwrap(), None);
        assert!(!fake.calls().contains("cat > /v/"), "no broker write");
        // A symlinked credential file is refused, never followed.
        let session = new_session(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "codex",
        )
        .unwrap();
        let elsewhere = fake.dir.join("elsewhere.json");
        std::fs::write(&elsewhere, chatgpt().to_string()).unwrap();
        std::os::unix::fs::symlink(
            &elsewhere,
            login_home(&fake.dir, &session).join("codex/auth.json"),
        )
        .unwrap();
        assert!(ingest_and_route(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "codex"
        )
        .is_err());
        assert!(!login_home(&fake.dir, &session).exists());
        assert!(elsewhere.exists(), "the link's target is never touched");
        assert!(!fake.calls().contains("cat > /v/"));
        // Sign-out removes an open host login's home too.
        let session = new_session(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "opencode",
        )
        .unwrap();
        sign_out_and_route(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "opencode",
        )
        .unwrap();
        assert!(!login_home(&fake.dir, &session).exists());
        assert_eq!(read_session(&fake.dir, "opencode").unwrap(), None);
    }

    /// The host CLI is found on the daemon's PATH and must answer a version; the session it
    /// opened is removed when it does not (another tool of that name, e.g. AWS's `copilot`).
    #[cfg(unix)]
    #[test]
    fn host_login_argv_needs_the_cli_on_path_answering_a_version() {
        if crate::validator::skip_when_nested(
            "host_login_argv_needs_the_cli_on_path_answering_a_version",
        ) {
            return;
        }
        let fake = FakeRuntime::new();
        let bin = fake.dir.join("bin");
        std::fs::create_dir(&bin).unwrap();
        let env = |path: &Path| vec![("PATH".to_string(), path.display().to_string())];
        let err = prepare_host_login_argv(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "codex",
            &env(&bin),
        )
        .unwrap_err();
        assert!(err.contains("codex is not installed on this host"), "{err}");
        assert_eq!(read_session(&fake.dir, "codex").unwrap(), None);
        // A stand-in that records where it ran and with which environment.
        crate::test_env::install_stub(
            &bin.join("codex"),
            "#!/bin/sh\nprintf '%s\\n' \"$PWD\" \"$HOME\" \"$CODEX_HOME\" \"${OPENAI_API_KEY:-unset}\" > \"$HOME/../probe\"\necho 'codex-cli 9.9.9'\n",
        );
        let argv = prepare_host_login_argv(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "codex",
            &env(&bin),
        )
        .unwrap();
        let session = read_session(&fake.dir, "codex").unwrap().unwrap();
        let home = login_home(&fake.dir, &session);
        assert_eq!(argv.last().unwrap(), "cli_auth_credentials_store=\"file\"");
        let banner = &argv[argv.iter().position(|a| a == "wicked-login").unwrap() + 1];
        assert!(
            banner.contains("codex 9.9.9") && banner.contains("WARNING"),
            "{banner}"
        );
        // The version probe ran isolated, in a home of its own that is already gone.
        let probe = std::fs::read_to_string(fake.dir.join("login/probe")).unwrap();
        let lines: Vec<String> = probe.lines().map(str::to_string).collect();
        let probe_home = Path::new(&lines[1]);
        assert_ne!(probe_home, home, "{probe}");
        assert!(probe_home.starts_with(fake.dir.join("login")), "{probe}");
        assert!(!probe_home.exists(), "the probe home is removed");
        assert_eq!(lines[2], format!("{}/codex", lines[1]));
        assert_eq!(lines[3], "unset");
        // The argv itself, run as the PTY runs it, executes the CLI in the throwaway home and
        // records its pid for sign-out.
        // spawn-audit: test-only — runs the printed argv as crew's PTY does (it is `env -i`).
        let out = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .output()
            .unwrap();
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains("wicked: signing in codex"));
        let probe = std::fs::read_to_string(fake.dir.join("login/probe")).unwrap();
        let h = home.display().to_string();
        let canon = home.canonicalize().unwrap().display().to_string();
        let lines: Vec<&str> = probe.lines().collect();
        assert!(lines[0] == h || lines[0] == canon, "{probe}");
        assert_eq!(
            &lines[1..],
            [h.clone(), format!("{h}/codex"), "unset".into()]
        );
        let pid = std::fs::read_to_string(login_pid_file(&fake.dir, &session)).unwrap();
        assert!(pid.parse::<i32>().is_ok_and(|p| p > 1), "{pid}");
        // Not a coding agent (no version): refused, and its session removed.
        crate::test_env::install_stub(&bin.join("pi"), "#!/bin/sh\necho 'not that tool'\n");
        let err = prepare_host_login_argv(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "pi",
            &env(&bin),
        )
        .unwrap_err();
        assert!(err.contains("answered no version"), "{err}");
        assert_eq!(read_session(&fake.dir, "pi").unwrap(), None);
    }

    /// codex review r1 #1: a sign-out (or a new sign-in) while the host login terminal is still
    /// open ends that login's process group before its home goes, so the CLI cannot write a
    /// credential into a home nobody tracks. A recycled pid (one that started after the pid
    /// file) is never signalled.
    #[cfg(unix)]
    #[test]
    fn sign_out_ends_a_still_running_host_login_before_removing_its_home() {
        if crate::validator::skip_when_nested(
            "sign_out_ends_a_still_running_host_login_before_removing_its_home",
        ) {
            return;
        }
        if crate::outstanding_work::tests::ps_exec_denied() {
            eprintln!("SKIP: `ps` cannot run here (EPERM — setuid binary under a sandbox)");
            return;
        }
        use std::os::unix::process::CommandExt;
        let fake = FakeRuntime::new();
        let session = new_session(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "pi",
        )
        .unwrap();
        let pid_file = login_pid_file(&fake.dir, &session);
        // spawn-audit: test-only — a stand-in login: records its pid as the login shell does.
        let mut login = std::process::Command::new("/bin/sh")
            .args(["-c", "printf '%s' \"$$\" > \"$1\"; exec sleep 600", "sh"])
            .arg(&pid_file)
            .process_group(0)
            .spawn()
            .unwrap();
        // (core#1194) Conditions, not clocks: the login records its pid before the sign-out
        // (bounded only as a failure guard), and both stand-ins live until signalled, so a slow
        // sign-out on a loaded host never reads a stand-in that simply ran out as "ended".
        let deadline = std::time::Instant::now() + crate::test_env::PATIENT;
        while !pid_file.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        sign_out_and_route(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "pi",
        )
        .unwrap();
        let status = login.wait().unwrap();
        assert!(!status.success(), "the login was ended: {status:?}");
        assert!(!login_home(&fake.dir, &session).exists());
        assert!(!pid_file.exists());
        // A pid file older than the process it names is a recycled pid: left alone.
        let session = new_session(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "pi",
        )
        .unwrap();
        let pid_file = login_pid_file(&fake.dir, &session);
        // spawn-audit: test-only — an unrelated process that happens to lead its group.
        let mut other = std::process::Command::new("sleep")
            .arg("600")
            .process_group(0)
            .spawn()
            .unwrap();
        std::fs::write(&pid_file, other.id().to_string()).unwrap();
        let old = SystemTime::now() - Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&pid_file)
            .unwrap()
            .set_modified(old)
            .unwrap();
        sign_out_and_route(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "pi",
        )
        .unwrap();
        assert!(
            other.try_wait().unwrap().is_none(),
            "a recycled pid is never signalled"
        );
        other.kill().unwrap();
        let _ = other.wait();
    }

    /// codex review r1 #2: a codex/pi/opencode login VOLUME a container session left from before
    /// the upgrade is still found, stopped and removed (never orphaned by its marker's removal).
    #[cfg(unix)]
    #[test]
    fn a_pre_upgrade_container_session_of_a_host_login_agent_is_still_removed() {
        if crate::validator::skip_when_nested(
            "a_pre_upgrade_container_session_of_a_host_login_agent_is_still_removed",
        ) {
            return;
        }
        let fake = FakeRuntime::new();
        let legacy = format!("{}-login-codex-0000abcd", fake.gw.name);
        let mut volumes = std::fs::read_to_string(fake.dir.join("volumes")).unwrap();
        volumes.push_str(&format!("{legacy}\n"));
        std::fs::write(fake.dir.join("volumes"), volumes).unwrap();
        write_session(&fake.dir, "codex", &legacy).unwrap();
        // Its credential is still ingested from the volume, as before the upgrade.
        std::fs::write(fake.dir.join("input"), chatgpt().to_string()).unwrap();
        ingest_and_route(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "codex",
        )
        .unwrap();
        assert!(fake.calls().contains(&format!("{legacy}:/in:ro")));
        assert!(fake.calls().contains(&format!("volume rm {legacy}")));
        assert_eq!(read_session(&fake.dir, "codex").unwrap(), None);
        // A sign-out of an open legacy session removes its volume too.
        std::fs::write(fake.dir.join("calls"), "").unwrap();
        write_session(&fake.dir, "codex", &legacy).unwrap();
        sign_out_and_route(
            &crate::seat_login::DockerIo::new(&fake.rt, &fake.gw),
            &fake.dir,
            "codex",
        )
        .unwrap();
        assert!(fake.calls().contains(&format!("volume rm {legacy}")));
        assert_eq!(read_session(&fake.dir, "codex").unwrap(), None);
    }

    /// codex review r1 #4: no component is followed through a symlink and no FIFO is waited on.
    #[cfg(unix)]
    #[test]
    fn read_home_follows_no_link_at_any_depth_and_never_blocks_on_a_fifo() {
        let dir = std::env::temp_dir().join(format!("wicked-rh-{}", uuid::Uuid::new_v4().simple()));
        let real = dir.join("real");
        std::fs::create_dir_all(real.join("codex")).unwrap();
        std::fs::write(real.join("codex/auth.json"), "FAKE").unwrap();
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::os::unix::fs::symlink(real.join("codex"), home.join("codex")).unwrap();
        assert!(
            read_home(&home, &["codex/auth.json"]).is_err(),
            "a linked directory"
        );
        std::fs::remove_file(home.join("codex")).unwrap();
        std::fs::create_dir(home.join("codex")).unwrap();
        std::os::unix::fs::symlink(real.join("codex/auth.json"), home.join("codex/auth.json"))
            .unwrap();
        assert!(
            read_home(&home, &["codex/auth.json"]).is_err(),
            "a linked file"
        );
        std::fs::remove_file(home.join("codex/auth.json")).unwrap();
        let fifo =
            std::ffi::CString::new(home.join("codex/auth.json").as_os_str().as_encoded_bytes())
                .unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(read_home(&home, &["codex/auth.json"]).is_err(), "a FIFO");
        std::fs::remove_file(home.join("codex/auth.json")).unwrap();
        assert_eq!(read_home(&home, &["codex/auth.json"]).unwrap(), Vec::new());
        std::fs::write(home.join("codex/auth.json"), "FAKE-ok").unwrap();
        assert_eq!(
            read_home(&home, &["codex/auth.json"]).unwrap(),
            vec![("codex/auth.json".to_string(), b"FAKE-ok".to_vec())]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn claude_oauth_extracts_access_and_preserves_original_refresh_record() {
        let input = vec![file("claude/.credentials.json", oauth(4102444800000))];
        let out = extract_tokens("claude", &input).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(bytes(&out, "anthropic.token"), b"FAKE-claude-access");
        assert_eq!(
            bytes(&out, "claude.credentials.json"),
            input[0].1.as_slice()
        );
        assert_eq!(
            credential_expiry("claude", &parse_json("claude", &input[0].1).unwrap()),
            Ok(Some(4102444800))
        );
    }

    #[test]
    fn claude_stored_api_key_maps_to_the_api_key_route() {
        for (path, value) in [
            (
                ".credentials.json",
                json!({"ANTHROPIC_API_KEY": "FAKE-anthropic-key"}),
            ),
            (
                "claude/settings.json",
                json!({"env": {"ANTHROPIC_API_KEY": "FAKE-anthropic-key"}}),
            ),
        ] {
            assert_eq!(
                extract_tokens("claude", &[file(path, value)]).unwrap(),
                vec![token_file("anthropic.key", "FAKE-anthropic-key")]
            );
        }
    }

    #[test]
    fn codex_chatgpt_extracts_token_and_account() {
        let input = vec![file("codex/auth.json", chatgpt())];
        let out = extract_tokens("codex", &input).unwrap();
        assert_eq!(out.len(), 3);
        assert_eq!(bytes(&out, "openai.token"), b"FAKE-codex-access");
        assert_eq!(bytes(&out, "openai.account"), b"FAKE-account-id");
        assert_eq!(bytes(&out, "codex.auth.json"), input[0].1.as_slice());
    }

    #[test]
    fn codex_api_key_respects_auth_mode_and_never_exports_stale_chatgpt_tokens() {
        let mut value = chatgpt();
        value["auth_mode"] = json!("apikey");
        value["OPENAI_API_KEY"] = json!("FAKE-openai-key");
        let out = extract_tokens("codex", &[file("auth.json", value)]).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(bytes(&out, "openai.key"), b"FAKE-openai-key");
        assert!(!out.iter().any(|(n, _)| n == "openai.token"));
        let legacy = json!({"OPENAI_API_KEY": "FAKE-legacy-key"});
        assert_eq!(
            bytes(
                &extract_tokens("codex", &[file("auth.json", legacy)]).unwrap(),
                "openai.key"
            ),
            b"FAKE-legacy-key"
        );
    }

    #[test]
    fn pi_provider_key_accepts_auth_and_models_shapes() {
        for (path, value) in [
            (
                "pi/auth.json",
                json!({"openai": {"type": "api_key", "key": "FAKE-pi-key"}}),
            ),
            (
                "models.json",
                json!({"providers": {"openai": {"apiKey": "FAKE-pi-key", "baseUrl": "http://example.invalid"}}}),
            ),
        ] {
            assert_eq!(
                extract_tokens("pi", &[file(path, value)]).unwrap(),
                vec![token_file("pi.token", "FAKE-pi-key")]
            );
        }
    }

    fn pi_jwt(account: &str) -> String {
        let e = |v: Value| URL_SAFE_NO_PAD.encode(serde_json::to_vec(&v).unwrap());
        format!(
            "{}.{}.FAKE-sig",
            e(json!({"alg": "none"})),
            e(json!({"https://api.openai.com/auth": {"chatgpt_account_id": account}}))
        )
    }

    /// The three subscription sign-ins pi's `/login` stores (FAKE values), plus an API key.
    fn pi_login() -> Value {
        json!({
            "openai": {"type": "api_key", "key": "FAKE-pi-key"},
            "anthropic": {"type": "oauth", "access": "sk-ant-oat01-FAKE-access",
                "refresh": "FAKE-ant-refresh", "expires": 4_000_000_000_000_u64},
            "openai-codex": {"type": "oauth", "access": pi_jwt("acct-FAKE-0002"),
                "refresh": "FAKE-oai-refresh", "expires": 4_100_000_000_000_u64,
                "accountId": "acct-FAKE-0002"},
            "github-copilot": {"type": "oauth",
                "access": "tid=FAKE;exp=1;proxy-ep=proxy.business.githubcopilot.com;",
                "refresh": "FAKE-gho-token", "expires": 3_900_000_000_000_u64},
            "xai": {"type": "oauth", "access": "FAKE-xai", "refresh": "FAKE-xai-r", "expires": 1}
        })
    }

    /// DES-SEAT-LOGIN-SUBSCRIPTIONS decision 1: every subscription a pi login stored gets its own
    /// route files, the whole record is retained for the refresher, and what the broker cannot
    /// serve yet is reported by name.
    #[test]
    fn pi_subscriptions_each_get_their_route_files_and_the_record_is_retained() {
        let login = pi_login();
        let input = vec![file("pi/auth.json", login.clone())];
        let files = extract_tokens("pi", &input).unwrap();
        let get = |n: &str| {
            files
                .iter()
                .find(|(f, _)| f == n)
                .map(|(_, b)| String::from_utf8(b.clone()).unwrap())
        };
        assert_eq!(get("pi.token").as_deref(), Some("FAKE-pi-key"));
        assert_eq!(
            get("pi.anthropic.token").as_deref(),
            Some("sk-ant-oat01-FAKE-access")
        );
        assert_eq!(get("pi.chatgpt.token"), Some(pi_jwt("acct-FAKE-0002")));
        assert_eq!(get("pi.chatgpt.account").as_deref(), Some("acct-FAKE-0002"));
        assert_eq!(
            get("pi.copilot.token").as_deref(),
            Some("tid=FAKE;exp=1;proxy-ep=proxy.business.githubcopilot.com;")
        );
        assert_eq!(
            get("pi.copilot.host").as_deref(),
            Some("api.business.githubcopilot.com")
        );
        let record: Value = serde_json::from_str(&get("pi.auth.json").unwrap()).unwrap();
        assert_eq!(record, login);
        for (name, _) in &files {
            assert!(
                broker_files("pi").unwrap().contains(&name.as_str()),
                "{name}"
            );
        }
        assert_eq!(
            skipped_providers("pi", &input),
            vec!["xai (oauth)".to_string()]
        );
        // The route literals the gateway carries come from the broker files alone.
        let literals = route_literals("pi", &files).unwrap();
        assert_eq!(
            literals.accounts,
            vec![("pi.chatgpt.token".into(), "acct-FAKE-0002".into())]
        );
        assert_eq!(
            literals.hosts,
            vec![(
                "pi.copilot.token".into(),
                "api.business.githubcopilot.com".into()
            )]
        );
        // The earliest OAuth expiry is the record's (the refresher's next due entry).
        assert_eq!(
            credential_expiry("pi", &login).unwrap(),
            Some(3_900_000_000)
        );
    }

    #[test]
    fn pi_refuses_inputs_that_would_reach_a_header_or_another_host() {
        let with = |id: &str, patch: Value| {
            let mut login = pi_login();
            for (k, v) in patch.as_object().unwrap() {
                login[id][k] = v.clone();
            }
            extract_tokens("pi", &[file("pi/auth.json", login)])
        };
        assert!(with("openai-codex", json!({"accountId": "FAKE\r\nx-evil: 1"})).is_err());
        assert!(with("openai-codex", json!({"accountId": null})).is_err());
        assert!(with(
            "github-copilot",
            json!({"access": "tid=x;proxy-ep=evil.example.com;"})
        )
        .is_err());
        assert!(
            with("github-copilot", json!({"enterpriseUrl": "corp.ghe.com"}))
                .unwrap_err()
                .contains("Enterprise")
        );
        assert!(with("github-copilot", json!({"refresh": null})).is_err());
        assert!(with("anthropic", json!({"access": "FAKE with space"})).is_err());
        assert!(with("anthropic", json!({"expires": "soon"})).is_err());
        // A Copilot token without a proxy endpoint rides the individual plan's host.
        let files = with("github-copilot", json!({"access": "tid=FAKE"})).unwrap();
        assert!(files
            .iter()
            .any(|(n, b)| n == "pi.copilot.host" && b == b"api.individual.githubcopilot.com"));
    }

    /// A Copilot token naming no `proxy-ep` takes the plan endpoint the exchange named (kept in
    /// the broker's copy of the record), never silently the individual plan's.
    #[test]
    fn pi_copilot_host_falls_back_to_the_exchanges_plan_endpoint() {
        let mut login = pi_login();
        login["github-copilot"]["access"] = json!("tid=FAKE-no-proxy");
        login["github-copilot"][COPILOT_ENDPOINT_FIELD] =
            json!("https://api.business.githubcopilot.com/");
        let files = extract_tokens("pi", &[file("pi/auth.json", login.clone())]).unwrap();
        assert!(files
            .iter()
            .any(|(n, b)| n == "pi.copilot.host" && b == b"api.business.githubcopilot.com"));
        login["github-copilot"][COPILOT_ENDPOINT_FIELD] = json!("https://evil.example.com");
        assert!(extract_tokens("pi", &[file("pi/auth.json", login)]).is_err());
    }

    /// Decision 7: an expired subscription is not placed in the seat; the agent is expired only
    /// when nothing valid is left, and a valid API key keeps it signed in.
    #[test]
    fn pi_status_and_seat_sign_in_count_only_valid_subscriptions() {
        use crate::seat_credentials::{CredentialKind, Subscription};
        let files = extract_tokens("pi", &[file("pi/auth.json", pi_login())]).unwrap();
        let now = 3_950_000_000; // copilot (3.9e9) expired; anthropic and chatgpt valid
        let sign_in = sign_in_from_files("pi", &files, now).unwrap();
        assert_eq!(sign_in.kind, Some(CredentialKind::ProviderKey));
        assert_eq!(
            sign_in.subscriptions,
            vec![Subscription::PiAnthropic, Subscription::PiChatgpt]
        );
        assert_eq!(
            status_from_files("pi", &files, now),
            BrokerStatus::SignedIn {
                expires_at: Some(4_000_000_000)
            }
        );
        let no_key: Vec<_> = files
            .iter()
            .filter(|(n, _)| n != "pi.token")
            .cloned()
            .collect();
        assert_eq!(
            status_from_files("pi", &no_key, 4_200_000_000),
            BrokerStatus::Expired
        );
        assert_eq!(sign_in_from_files("pi", &no_key, 4_200_000_000), None);
        assert_eq!(status_from_files("pi", &[], now), BrokerStatus::SignedOut);
        // Gateway init creates empty route files: empty = signed out.
        let empty: Vec<_> = files.iter().map(|(n, _)| (n.clone(), Vec::new())).collect();
        assert_eq!(
            status_from_files("pi", &empty, now),
            BrokerStatus::SignedOut
        );
    }

    /// What `copilot login` leaves with plaintext storage consented (FAKE values): JSON behind
    /// the CLI's `//` header, the token under `<host>:<login>` and `<host>:<login>:github`.
    fn copilot_config() -> String {
        "// User settings belong in settings.json.\n// This file is managed automatically.\n{\n  \"authTokens\": {\n    \"https://github.com:fake-user:github\": {\"token\": \"FAKE-gho-cp\"},\n    \"https://github.com:fake-user\": {\"token\": \"FAKE-gho-cp\"}\n  },\n  \"loggedInUsers\": [{\"host\": \"https://github.com\", \"login\": \"fake-user\", \"kind\": \"githubDotCom\"}],\n  \"lastLoggedInUser\": {\"host\": \"https://github.com\", \"login\": \"fake-user\"}\n}\n".into()
    }

    /// DES-SEAT-LOGIN-SUBSCRIPTIONS: copilot's login token (behind its JSONC header) is the
    /// broker's one file; GitHub Enterprise, a missing token and a missing user are refused.
    #[test]
    fn copilot_login_token_is_ingested_from_its_plaintext_config() {
        use crate::seat_credentials::Subscription;
        let cfg = |body: String| vec![("copilot/config.json".to_string(), body.into_bytes())];
        let files = extract_tokens("copilot", &cfg(copilot_config())).unwrap();
        assert_eq!(files, vec![token_file("copilot.token", "FAKE-gho-cp")]);
        let sign_in = sign_in_from_files("copilot", &files, 100).unwrap();
        assert_eq!(sign_in.kind, None);
        assert_eq!(sign_in.subscriptions, vec![Subscription::Copilot]);
        assert_eq!(
            status_from_files("copilot", &files, 100),
            BrokerStatus::SignedIn { expires_at: None }
        );
        assert_eq!(
            status_from_files("copilot", &[], 100),
            BrokerStatus::SignedOut
        );
        // OD-1: the account the broker read beside the token becomes the route's literals (its
        // host and models); the account files alone are no sign-in.
        let account = crate::copilot_account::account_models(
            &json!({"data": [{"id": "gpt-4.1", "capabilities": {"type": "chat"}}]}),
            9,
        )
        .unwrap();
        let mut with = files.clone();
        with.extend(
            crate::copilot_account::account_files("api.individual.githubcopilot.com", &account)
                .unwrap(),
        );
        assert_eq!(broker_files("copilot").unwrap().len(), 3);
        let literals = route_literals("copilot", &with).unwrap();
        assert_eq!(
            literals.hosts,
            vec![(
                "copilot.token".to_string(),
                "api.individual.githubcopilot.com".to_string()
            )]
        );
        assert_eq!(
            literals.models,
            vec![("copilot.token".to_string(), account)]
        );
        assert_eq!(
            status_from_files("copilot", &with[1..], 100),
            BrokerStatus::SignedOut
        );
        assert!(
            route_literals("copilot", &[("copilot.models".to_string(), b"{".to_vec())]).is_err()
        );
        let ghe = copilot_config().replace("https://github.com", "https://corp.ghe.com");
        assert!(extract_tokens("copilot", &cfg(ghe))
            .unwrap_err()
            .contains("Enterprise"));
        let no_token = copilot_config().replace("FAKE-gho-cp", "");
        let err = extract_tokens("copilot", &cfg(no_token)).unwrap_err();
        assert!(err.contains("token") && !err.contains("FAKE"), "{err}");
        // As copilot may write it: a BOM before the header, or before bare JSON; CRLF lines.
        for body in [
            format!("\u{feff}{}", copilot_config()),
            format!(
                "\u{feff}{}",
                copilot_config()
                    .lines()
                    .filter(|l| !l.starts_with("//"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
            copilot_config().replace('\n', "\r\n"),
        ] {
            assert_eq!(
                extract_tokens("copilot", &cfg(body)).unwrap(),
                vec![token_file("copilot.token", "FAKE-gho-cp")]
            );
        }
        let invalid = vec![("copilot/config.json".to_string(), vec![0xff, 0xfe, b'{'])];
        assert!(extract_tokens("copilot", &invalid).is_err());
        let no_user = copilot_config().replace("lastLoggedInUser", "somethingElse");
        assert!(extract_tokens("copilot", &cfg(no_user)).is_err());
        // The init helper seeds copilot's plaintext consent beside its home.
        let init = init_login_argv("vol", "copilot");
        assert!(init.last().unwrap().contains("/in/copilot/settings.json"));
        assert!(init
            .last()
            .unwrap()
            .contains(r#"{"storeTokenPlaintext":true}"#));
    }

    /// What opencode's login leaves after `auth export` (FAKE values): its ChatGPT sign-in, its
    /// Copilot device sign-in, an Anthropic key and an inactive older OpenAI key.
    fn opencode_login() -> Value {
        json!([
            {"id": "c0", "integrationID": "openai", "label": "API key", "active": false,
                "value": {"type": "key", "key": "FAKE-old-key"}},
            {"id": "c1", "integrationID": "openai", "label": "OAuth", "active": true,
                "value": {"type": "oauth", "methodID": "chatgpt-headless", "refresh": "FAKE-oc-r",
                    "access": "FAKE-oc-access", "expires": 4_000_000_000_000_u64,
                    "metadata": {"accountID": "acct-FAKE-oc"}}},
            {"id": "c2", "integrationID": "github-copilot", "label": "OAuth", "active": true,
                "value": {"type": "oauth", "methodID": "device", "refresh": "FAKE-gho-oc",
                    "access": "FAKE-gho-oc", "expires": 0,
                    "metadata": {"apiEndpoint": "https://api.business.githubcopilot.com"}}},
            {"id": "c3", "integrationID": "anthropic", "label": "API key", "active": true,
                "value": {"type": "key", "key": "FAKE-oc-ant"}}
        ])
    }

    /// The opencode login command: a failed login exports nothing and fails; a failed export
    /// leaves no export (whole or nothing) and fails; both succeeding leave the export, private.
    #[cfg(unix)]
    #[test]
    fn opencode_login_exports_only_after_a_successful_login_and_whole() {
        use std::os::unix::fs::PermissionsExt;
        let dir =
            std::env::temp_dir().join(format!("wicked-oc-login-{}", uuid::Uuid::new_v4().simple()));
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fake = bin.join("opencode");
        std::fs::write(
            &fake,
            "#!/bin/sh\n[ \"$3\" = --standalone ] || exit 9\ncase \"$2\" in\n  login) [ -e \"$FAKE_DIR/login_fails\" ] && exit 3; exit 0;;\n  export) printf '[{\"partial\":'; [ -e \"$FAKE_DIR/export_fails\" ] && exit 4; printf '1}]';;\nesac\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let run = |case: &str| {
            let data = dir.join(case);
            let _ = std::fs::remove_dir_all(&data);
            std::fs::create_dir_all(&data).unwrap();
            // spawn-audit: test-only — the engine's login command with a fake opencode.
            let out = std::process::Command::new("/bin/sh")
                .args(["-c", OPENCODE_HOST_LOGIN, "wicked-login", "BANNER"])
                .arg(dir.join(format!("{case}.pid")))
                .arg(&fake)
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", &data)
                .env("XDG_DATA_HOME", &data)
                .env("FAKE_DIR", &dir)
                .output()
                .unwrap();
            (out.status.success(), data.join("opencode"))
        };
        std::fs::write(dir.join("login_fails"), "").unwrap();
        let (ok, d) = run("a");
        assert!(!ok && !d.join("wicked-auth.json").exists());
        std::fs::remove_file(dir.join("login_fails")).unwrap();
        std::fs::write(dir.join("export_fails"), "").unwrap();
        let (ok, d) = run("b");
        assert!(!ok);
        assert!(!d.join("wicked-auth.json").exists() && !d.join("wicked-auth.json.tmp").exists());
        std::fs::remove_file(dir.join("export_fails")).unwrap();
        let (ok, d) = run("c");
        assert!(ok);
        assert_eq!(
            std::fs::read_to_string(d.join("wicked-auth.json")).unwrap(),
            "[{\"partial\":1}]"
        );
        let mode = std::fs::metadata(d.join("wicked-auth.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "the export is private");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// opencode 2.x keeps its sign-ins in SQLite; the login exports them, and ingest takes the
    /// ACTIVE credential of each integration onto its own route files (decision 1).
    #[test]
    fn opencode_export_maps_each_active_sign_in_to_its_route_files() {
        let login = opencode_login();
        let input = vec![file("data/opencode/wicked-auth.json", login.clone())];
        let files = extract_tokens("opencode", &input).unwrap();
        let get = |n: &str| {
            files
                .iter()
                .find(|(f, _)| f == n)
                .map(|(_, b)| String::from_utf8(b.clone()).unwrap())
        };
        assert_eq!(get("opencode.token"), None, "the inactive key is not used");
        assert_eq!(
            get("opencode.chatgpt.token").as_deref(),
            Some("FAKE-oc-access")
        );
        assert_eq!(
            get("opencode.chatgpt.account").as_deref(),
            Some("acct-FAKE-oc")
        );
        assert_eq!(
            get("opencode.copilot.token").as_deref(),
            Some("FAKE-gho-oc")
        );
        assert_eq!(
            get("opencode.copilot.host").as_deref(),
            Some("api.business.githubcopilot.com")
        );
        assert_eq!(
            get("opencode.anthropic.key").as_deref(),
            Some("FAKE-oc-ant")
        );
        let retained: Value =
            serde_json::from_str(&get("opencode.credentials.json").unwrap()).unwrap();
        assert_eq!(retained, login);
        for (name, _) in &files {
            assert!(
                broker_files("opencode").unwrap().contains(&name.as_str()),
                "{name}"
            );
        }
        // Only the ChatGPT sign-in expires; Copilot's `expires: 0` is none.
        assert_eq!(
            credential_expiry("opencode", &login).unwrap(),
            Some(4_000_000_000)
        );
        let literals = route_literals("opencode", &files).unwrap();
        assert_eq!(
            literals.accounts,
            vec![("opencode.chatgpt.token".into(), "acct-FAKE-oc".into())]
        );
        // A key-only login keeps the existing provider-key route.
        let key_only = json!([{"id": "k", "integrationID": "openai", "label": "API key",
            "active": true, "value": {"type": "key", "key": "FAKE-opencode-key"}}]);
        assert_eq!(
            extract_tokens("opencode", &[file("wicked-auth.json", key_only)]).unwrap(),
            vec![token_file("opencode.token", "FAKE-opencode-key")]
        );
        // The pre-2.x files are no longer read (opencode never writes them).
        assert!(extract_tokens(
            "opencode",
            &[file(
                "data/opencode/auth.json",
                json!({"openai": {"type": "api", "key": "FAKE"}})
            )]
        )
        .is_err());
    }

    #[test]
    fn opencode_refuses_header_shaped_accounts_foreign_hosts_and_enterprise() {
        let with = |idx: usize, pointer: &str, v: Value| {
            let mut login = opencode_login();
            *login[idx].pointer_mut(pointer).unwrap() = v;
            extract_tokens("opencode", &[file("wicked-auth.json", login)])
        };
        assert!(with(1, "/value/metadata/accountID", json!("FAKE\r\nx-evil: 1")).is_err());
        assert!(with(
            2,
            "/value/metadata/apiEndpoint",
            json!("https://evil.example.com")
        )
        .is_err());
        assert!(with(
            2,
            "/value/metadata",
            json!({"enterpriseUrl": "corp.ghe.com"})
        )
        .unwrap_err()
        .contains("Enterprise"));
        assert!(with(1, "/value/access", json!("FAKE with space")).is_err());
        assert!(extract_tokens("opencode", &[file("wicked-auth.json", json!({"a": 1}))]).is_err());
    }

    #[test]
    fn opencode_status_and_sign_in_count_only_valid_subscriptions() {
        use crate::seat_credentials::Subscription;
        let files =
            extract_tokens("opencode", &[file("wicked-auth.json", opencode_login())]).unwrap();
        let sign_in = sign_in_from_files("opencode", &files, 100).unwrap();
        assert_eq!(sign_in.kind, None);
        assert_eq!(
            sign_in.subscriptions,
            vec![
                Subscription::OpencodeChatgpt,
                Subscription::OpencodeCopilot,
                Subscription::OpencodeAnthropicKey
            ]
        );
        // The ChatGPT sign-in expired: Copilot and the Anthropic key still serve.
        let later = sign_in_from_files("opencode", &files, 4_100_000_000).unwrap();
        assert_eq!(
            later.subscriptions,
            vec![
                Subscription::OpencodeCopilot,
                Subscription::OpencodeAnthropicKey
            ]
        );
        assert_eq!(
            status_from_files("opencode", &files, 100),
            BrokerStatus::SignedIn {
                expires_at: Some(4_000_000_000)
            }
        );
        assert_eq!(
            status_from_files("opencode", &[], 100),
            BrokerStatus::SignedOut
        );
    }

    #[test]
    fn missing_credentials_name_the_required_field_and_never_return_a_partial_set() {
        for cli in ["claude", "codex", "pi", "opencode"] {
            let err = extract_tokens(cli, &[]).unwrap_err();
            assert!(err.contains(cli) && err.contains("signed out") && err.contains("missing"));
        }
        let mut value = chatgpt();
        value["tokens"]["account_id"] = Value::Null;
        let err = extract_tokens("codex", &[file("auth.json", value)]).unwrap_err();
        assert!(err.contains("tokens.account_id"));
        assert!(!err.contains("FAKE-codex-access"));
        let err = extract_tokens(
            "claude",
            &[file(
                ".credentials.json",
                json!({"claudeAiOauth": {"accessToken": " "}}),
            )],
        )
        .unwrap_err();
        assert!(err.contains("claudeAiOauth.accessToken"));
    }

    #[test]
    fn extraction_cannot_invent_a_secret_or_promote_an_unknown_file() {
        for (cli, input) in [
            (
                "claude",
                vec![file(".credentials.json", oauth(4102444800000))],
            ),
            ("codex", vec![file("auth.json", chatgpt())]),
            (
                "codex",
                vec![file(
                    "auth.json",
                    json!({"auth_mode": "apikey", "OPENAI_API_KEY": "FAKE-key"}),
                )],
            ),
            (
                "pi",
                vec![file(
                    "auth.json",
                    json!({"openai": {"type": "api_key", "key": "FAKE-key"}}),
                )],
            ),
            (
                "opencode",
                vec![file(
                    "wicked-auth.json",
                    json!([{"id": "c1", "integrationID": "openai", "label": "API key",
                        "active": true, "value": {"type": "key", "key": "FAKE-key"}}]),
                )],
            ),
        ] {
            for (_, output) in extract_tokens(cli, &input).unwrap() {
                assert!(
                    input
                        .iter()
                        .any(|(_, b)| b.windows(output.len()).any(|w| w == output.as_slice())),
                    "{cli}: output must come from input bytes"
                );
            }
            assert!(extract_tokens(
                cli,
                &[file(
                    "unrelated.json",
                    json!({"OPENAI_API_KEY": "FAKE-unknown-key"})
                )]
            )
            .is_err());
        }
    }

    #[test]
    fn unsupported_providers_oauth_and_copilot_fail_without_selecting_another_key() {
        // pi names what it cannot serve yet, and never selects another provider's key for it.
        let err = extract_tokens(
            "pi",
            &[file(
                "auth.json",
                json!({"anthropic": {"type": "api_key", "key": "FAKE-wrong-provider"},
                    "openai": {"type": "oauth", "access": "FAKE-oauth", "key": "FAKE-do-not-use"}}),
            )],
        )
        .unwrap_err();
        assert!(err.contains("supported sign-in"), "{err}");
        assert!(
            err.contains("anthropic (api_key)") && err.contains("openai (oauth)"),
            "{err}"
        );
        assert!(!err.contains("FAKE"), "{err}");
        // opencode names what it cannot serve yet (token sharing has no in-container callback).
        let err = extract_tokens(
            "opencode",
            &[file(
                "wicked-auth.json",
                json!([{"id": "c1", "integrationID": "openai", "label": "OAuth", "active": true,
                    "value": {"type": "oauth", "methodID": "chatgpt-token-sharing",
                        "access": "FAKE-ts", "refresh": "FAKE-ts-r", "expires": 1}},
                    {"id": "c2", "integrationID": "xai", "label": "API key", "active": true,
                    "value": {"type": "key", "key": "FAKE-xai"}}]),
            )],
        )
        .unwrap_err();
        assert!(err.contains("openai (chatgpt-token-sharing)"), "{err}");
        assert!(err.contains("xai (key)") && !err.contains("FAKE"), "{err}");
        assert!(extract_tokens("copilot", &[])
            .unwrap_err()
            .contains("config.json"));
    }

    #[test]
    fn malformed_duplicate_oversized_and_invalid_expiry_inputs_have_redacted_errors() {
        let bad = vec![(
            "auth.json".into(),
            b"{\"FAKE-secret-malformed\": BROKEN}".to_vec(),
        )];
        let err = extract_tokens("codex", &bad).unwrap_err();
        assert!(err.contains("invalid credential JSON") && !err.contains("FAKE-secret"));
        let duplicate = vec![
            file("auth.json", chatgpt()),
            file("codex/auth.json", chatgpt()),
        ];
        assert!(extract_tokens("codex", &duplicate)
            .unwrap_err()
            .contains("duplicate"));
        let large = vec![("auth.json".into(), vec![b'x'; MAX_FILE_BYTES + 1])];
        assert!(extract_tokens("codex", &large)
            .unwrap_err()
            .contains("size limit"));
        let mut bad = oauth(4102444800000);
        bad["claudeAiOauth"]["expiresAt"] = json!("FAKE-secret-expiry");
        let err = extract_tokens("claude", &[file(".credentials.json", bad)]).unwrap_err();
        assert!(err.contains("invalid credential expiry") && !err.contains("FAKE-secret"));
    }

    #[test]
    fn status_distinguishes_empty_active_expired_and_unknown_without_secret_output() {
        for cli in ["claude", "codex", "pi", "opencode"] {
            assert_eq!(status_from_files(cli, &[], 100), BrokerStatus::SignedOut);
            let empty: Vec<_> = broker_files(cli)
                .unwrap()
                .iter()
                .map(|p| ((*p).into(), vec![]))
                .collect();
            assert_eq!(status_from_files(cli, &empty, 100), BrokerStatus::SignedOut);
        }
        let mut files = vec![
            token_file("anthropic.token", "FAKE-claude-access"),
            file("claude.credentials.json", oauth(200000)),
        ];
        assert_eq!(
            status_from_files("claude", &files, 100),
            BrokerStatus::SignedIn {
                expires_at: Some(200)
            }
        );
        assert_eq!(
            status_from_files("claude", &files, 200),
            BrokerStatus::Expired
        );
        assert_eq!(
            status_from_files("claude", &files, 201),
            BrokerStatus::Expired
        );
        files[1].1 = b"FAKE-secret-invalid-json".to_vec();
        let unknown = status_from_files("claude", &files, 100);
        assert!(matches!(unknown, BrokerStatus::Unknown(_)));
        let encoded = serde_json::to_string(&unknown).unwrap();
        assert!(!encoded.contains("FAKE-secret"));
        assert!(encoded.contains("unknown"));
        assert_eq!(
            serde_json::to_value(BrokerStatus::SignedIn {
                expires_at: Some(200)
            })
            .unwrap(),
            json!({"status": "signed_in", "detail": {"expires_at": 200}})
        );
        for (cli, filename) in [
            ("claude", "anthropic.key"),
            ("pi", "pi.token"),
            ("opencode", "opencode.token"),
        ] {
            let status = status_from_files(cli, &[token_file(filename, "FAKE-secret-key")], 100);
            assert_eq!(status, BrokerStatus::SignedIn { expires_at: None });
            assert!(!serde_json::to_string(&status)
                .unwrap()
                .contains("FAKE-secret"));
        }
    }

    /// C6: a governed seat presents what the user signed in with, never a configured guess. Each
    /// sign-in the broker can hold maps to the one kind whose route serves that credential, and
    /// a broker the status would call signed out yields no kind (the seat is refused).
    #[test]
    fn the_seat_presents_the_kind_the_user_signed_in_with() {
        use crate::seat_credentials::CredentialKind as K;
        let f = |name: &str, v: &str| token_file(name, v);
        let codex_mode = |mode: &str| file("codex.auth.json", json!({"auth_mode": mode}));
        for (cli, files, want) in [
            (
                "claude",
                vec![
                    f("anthropic.token", "FAKE-access"),
                    file("claude.credentials.json", oauth(4102444800000)),
                ],
                Some(K::ClaudeOauth),
            ),
            (
                "claude",
                vec![f("anthropic.key", "FAKE-key")],
                Some(K::ClaudeApiKey),
            ),
            // OAuth wins, as the status's expiry reading does.
            (
                "claude",
                vec![
                    f("anthropic.key", "FAKE-key"),
                    f("anthropic.token", "FAKE-a"),
                ],
                Some(K::ClaudeOauth),
            ),
            (
                "codex",
                vec![
                    codex_mode("chatgpt"),
                    f("openai.token", "FAKE-a"),
                    f("openai.account", "FAKE-acct"),
                ],
                Some(K::CodexChatgpt),
            ),
            (
                "codex",
                vec![codex_mode("apikey"), f("openai.key", "FAKE-key")],
                Some(K::CodexApiKey),
            ),
            (
                "codex",
                vec![f("openai.key", "FAKE-key")],
                Some(K::CodexApiKey),
            ),
            // A ChatGPT sign-in without its account header cannot authenticate: no kind.
            (
                "codex",
                vec![codex_mode("chatgpt"), f("openai.token", "FAKE-a")],
                None,
            ),
            // A mode whose route file is empty (revoked) is signed out.
            (
                "codex",
                vec![codex_mode("apikey"), f("openai.key", "")],
                None,
            ),
            ("pi", vec![f("pi.token", "FAKE-key")], Some(K::ProviderKey)),
            (
                "opencode",
                vec![f("opencode.token", "FAKE-key")],
                Some(K::ProviderKey),
            ),
            ("copilot", vec![f("pi.token", "FAKE-key")], None),
            ("copilot", vec![f("copilot.token", "FAKE-gho")], None),
        ] {
            assert_eq!(kind_from_files(cli, &files), want, "{cli} {files:?}");
            // Whenever there is a kind, the status agrees the agent is signed in.
            if want.is_some() {
                assert!(
                    matches!(
                        status_from_files(cli, &files, 100),
                        BrokerStatus::SignedIn { .. }
                    ),
                    "{cli}"
                );
            }
        }
        for cli in ["claude", "codex", "pi", "opencode"] {
            let empty: Vec<_> = broker_files(cli)
                .unwrap()
                .iter()
                .map(|p| ((*p).into(), vec![]))
                .collect();
            assert_eq!(kind_from_files(cli, &[]), None, "{cli}");
            assert_eq!(kind_from_files(cli, &empty), None, "{cli}");
        }
    }

    #[test]
    fn codex_status_requires_route_account_and_uses_access_jwt_expiry() {
        let mut auth = chatgpt();
        auth["tokens"]["access_token"] = json!(jwt(json!(200)));
        let mut files = vec![
            token_file("openai.token", "FAKE-access"),
            file("codex.auth.json", auth),
        ];
        assert_eq!(
            status_from_files("codex", &files, 100),
            BrokerStatus::SignedOut
        );
        // The account the gateway's route carries completes a ChatGPT sign-in.
        files.push(token_file("openai.account", "FAKE-account"));
        assert_eq!(
            status_from_files("codex", &files, 100),
            BrokerStatus::SignedIn {
                expires_at: Some(200)
            }
        );
        assert_eq!(
            status_from_files("codex", &files, 200),
            BrokerStatus::Expired
        );
        files.push(token_file("openai.key", "FAKE-stale-key"));
        assert_eq!(
            status_from_files("codex", &files, 200),
            BrokerStatus::Expired,
            "the retained chatgpt mode must still report expiry when an old key remains"
        );
        assert_eq!(credential_expiry("codex", &chatgpt()), Ok(None));
        let mut invalid = chatgpt();
        invalid["tokens"]["access_token"] = json!(jwt(json!("FAKE-bad-expiry")));
        assert_eq!(
            credential_expiry("codex", &invalid),
            Err("codex: invalid credential expiry".into())
        );
        let key = vec![
            token_file("openai.key", "FAKE-key"),
            token_file("openai.token", "FAKE-stale-token"),
            file("codex.auth.json", json!({"auth_mode": "apikey"})),
        ];
        assert_eq!(
            status_from_files("codex", &key, 100),
            BrokerStatus::SignedIn { expires_at: None }
        );
    }

    #[test]
    fn ingest_report_serializes_only_cli_file_names_and_expiry() {
        let report = IngestReport {
            cli: "claude".into(),
            files: vec!["anthropic.token".into(), "claude.credentials.json".into()],
            expires_at: Some(200),
            skipped: Vec::new(),
            literals: crate::seat_gateway::RouteLiterals {
                accounts: vec![("openai.token".into(), "FAKE-account-id".into())],
                hosts: Vec::new(),
                models: Vec::new(),
            },
        };
        // The account id rides to the gateway config only; never into the report's output.
        assert_eq!(
            serde_json::to_value(&report).unwrap(),
            json!({"cli": "claude", "files": ["anthropic.token", "claude.credentials.json"], "expires_at": 200})
        );
    }

    #[test]
    fn gateway_identity_requires_explicit_absolute_directory_without_host_io() {
        assert!(gateway_for_dir(None).is_err());
        assert!(gateway_for_dir(Some(Path::new("relative/gateway"))).is_err());
        // Absolute on every platform (a bare `/x` has no drive on Windows); never opened.
        let dir = std::env::temp_dir().join("FAKE-never-opened-state/gateway");
        let dir = dir.as_path();
        gateway_for_dir(Some(dir)).unwrap();
        assert!(!dir.exists(), "validation does not create gateway state");
    }

    #[test]
    fn cli_bad_input_returns_usage_exit_two_before_any_runtime_call() {
        for input in [
            vec![],
            vec!["argv"],
            vec!["bogus", "claude"],
            vec!["status", "unknown"],
            vec!["ingest", "../claude"],
            vec!["sign-out", "claude", "extra"],
            vec!["argv", "claude", "FAKE-misplaced-secret"],
        ] {
            let args: Vec<String> = input.into_iter().map(str::to_string).collect();
            assert_eq!(parse_args(&args).unwrap_err(), USAGE);
            assert_eq!(seat_login_main(&args), 2);
        }
        for verb in ["argv", "ingest", "status", "sign-out"] {
            for cli in ["claude", "codex", "pi", "opencode", "copilot"] {
                let args = vec![verb.to_string(), cli.to_string()];
                assert_eq!(parse_args(&args).unwrap(), (verb, cli));
            }
        }
    }

    #[cfg(not(unix))]
    #[test]
    fn native_windows_dispatch_returns_runtime_required_exit_one() {
        assert_eq!(seat_login_main(&["status".into(), "claude".into()]), 1);
    }
}
