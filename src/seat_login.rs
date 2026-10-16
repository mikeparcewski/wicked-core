//! Throwaway login homes -> broker credentials. No operator config home is opened.
//! Crew runs the printed interactive argv in its existing PTY, then calls ingest.
//! Login uses the gateway's external bridge; a login auth-host allowlist is a later slice.
//! Broker expiry is reported in Unix seconds (Claude stores milliseconds).

#![cfg_attr(not(unix), allow(dead_code))] // agy lg-r1 A7

use std::path::Path;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::Serialize;
use serde_json::Value;

#[cfg(unix)]
use crate::container_floor::{output_within, Runtime};
#[cfg(unix)]
use crate::seat_gateway::{volumes, Gateway};
#[cfg(unix)]
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const USAGE: &str = "usage: wicked-core seat-login <argv|ingest|status|sign-out|refresh> <claude|codex|pi|opencode|copilot>\n  Set WICKED_SEAT_GATEWAY_DIR to the daemon's absolute gateway directory.";
const GATEWAY_DIR_ENV: &str = "WICKED_SEAT_GATEWAY_DIR";
#[cfg(unix)]
const TIMEOUT: Duration = Duration::from_secs(15);
// Bound credential reads as well as runtime execution. No untrusted file can fill a pipe forever.
const MAX_FILE_BYTES: usize = 1024 * 1024;

fn supported_cli(cli: &str) -> bool {
    matches!(cli, "claude" | "codex" | "pi" | "opencode" | "copilot")
}

/// Per daemon (its gateway's name) and per agent: two daemons on one runtime never share a login.
fn login_volume(gateway: &str, cli: &str) -> String {
    format!(
        "{gateway}-login-{cli}-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    )
}

/// The login container of the session whose volume is `login_volume`: the same name (containers
/// and volumes are separate namespaces), so the session marker records both.
fn login_container(login_volume: &str) -> String {
    login_volume.to_string()
}

/// opencode's login command: its own `auth login`, then `auth export` of the credential rows it
/// stored (opencode 2.x keeps them in SQLite, never an `auth.json`) to the file ingest reads.
const OPENCODE_LOGIN: &str = "opencode auth login; umask 077; mkdir -p \"$XDG_DATA_HOME/opencode\" && opencode auth export > \"$XDG_DATA_HOME/opencode/wicked-auth.json\"";

/// Pure argv rendering: no host environment or credentials are forwarded. Callers validate CLI
/// names before using the returned argv.
pub(crate) fn login_argv(
    rt: &Path,
    cli: &str,
    image: &str,
    login_volume: &str,
    network: &str,
) -> Vec<String> {
    let mut argv = vec![
        rt.to_string_lossy().into_owned(),
        "run".into(),
        "-it".into(),
        "--rm".into(),
        // codex lg-r3 R1: PTY output can include credentials; keep it out of runtime logs.
        "--log-driver".into(),
        "none".into(),
        // Named after its session's volume (C6): a terminal whose client died (a closed tab, a
        // dropped socket) leaves this container running on the volume, and only a known name lets
        // ingest, the next sign-in and sign-out stop it.
        "--name".into(),
        login_container(login_volume),
        "--user".into(),
        "10001".into(),
        "--read-only".into(),
        "--tmpfs".into(),
        "/tmp:rw,exec,mode=1777".into(),
        "--cap-drop".into(),
        "ALL".into(),
        "--security-opt".into(),
        "no-new-privileges".into(),
        "--network".into(),
        network.into(),
        "-v".into(),
        format!("{login_volume}:/home/login"),
        "-e".into(),
        "HOME=/home/login".into(),
    ];
    for (var, dir) in login_homes(cli) {
        argv.extend(["-e".into(), format!("{var}=/home/login/{dir}")]);
    }
    argv.push(image.into());
    let command: &[&str] = match cli {
        "claude" => &["claude", "auth", "login"],
        "codex" => &["codex", "login", "--device-auth"],
        // The login, then its own export of what it stored, into the same throwaway volume.
        "opencode" => &["sh", "-c", OPENCODE_LOGIN],
        "pi" => &["pi"], // /login is entered inside the TUI.
        "copilot" => &["copilot", "login"],
        _ => &[],
    };
    argv.extend(command.iter().map(|s| (*s).to_string()));
    argv
}

/// Each CLI's configuration-home variable and its directory under the throwaway HOME. The init
/// helper creates every one of them before the login runs: codex refuses a `CODEX_HOME` that
/// does not exist ("Error loading configuration"), so its login exited before any sign-in and
/// ingest then found no `auth.json`.
fn login_homes(cli: &str) -> &'static [(&'static str, &'static str)] {
    match cli {
        "claude" => &[("CLAUDE_CONFIG_DIR", "claude")],
        "codex" => &[("CODEX_HOME", "codex")],
        "pi" => &[("PI_CODING_AGENT_DIR", "pi")],
        "opencode" => &[("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")],
        // COPILOT_HOME is unconfirmed. HOME alone confines its default files to the volume.
        _ => &[],
    }
}

/// Paths under the throwaway HOME, aligned with login_argv. Bare names are accepted by the
/// pure extractor too, so callers may hand it files relative to a CLI's configuration home.
fn credential_paths(cli: &str) -> &'static [&'static str] {
    match cli {
        "claude" => &["claude/.credentials.json", "claude/settings.json"],
        "codex" => &["codex/auth.json"],
        "pi" => &["pi/auth.json", "pi/models.json"],
        // opencode 2.x keeps credentials in its SQLite DB; the login exports them here.
        "opencode" => &["data/opencode/wicked-auth.json"],
        _ => &[],
    }
}

fn broker_files(cli: &str) -> Result<&'static [&'static str], String> {
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
        "copilot" => Err("copilot credential path and gateway route are not configured".into()),
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
            for path in credential_paths(cli) {
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
            Err(missing(
                cli,
                "claudeAiOauth.accessToken or stored ANTHROPIC_API_KEY",
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
pub(crate) fn read_broker_record(
    rt: &Runtime,
    gw: &Gateway,
    file: &str,
) -> Result<Option<Vec<u8>>, String> {
    let volume = volumes(gw).1;
    if !volume_exists(rt, &volume)? {
        return Ok(None);
    }
    let files = read_volume(rt, &volume, &[file], false)?;
    Ok(files
        .into_iter()
        .next()
        .map(|(_, b)| b)
        .filter(|b| !b.is_empty()))
}

/// Pure status parser. Route entries need only nonempty presence markers; retained JSON is
/// read for expiry. Empty route files created at gateway initialization mean signed out.
fn status_from_files(cli: &str, files: &[(String, Vec<u8>)], now: u64) -> BrokerStatus {
    if let Err(e) = broker_files(cli) {
        return BrokerStatus::Unknown(e);
    }
    let present = |name: &str| files.iter().any(|(p, b)| p == name && !b.is_empty());
    if matches!(cli, "pi" | "opencode") {
        let key = if cli == "pi" {
            "pi.token"
        } else {
            "opencode.token"
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

/// Helper argv is pure; scripts and paths are fixed by this module, never supplied by the CLI.
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
fn init_login_argv(volume: &str, cli: &str) -> Vec<String> {
    let dirs: Vec<String> = login_homes(cli)
        .iter()
        .map(|(_, dir)| format!("/in/{dir}"))
        .chain(std::iter::once("/in".to_string()))
        .collect();
    let dirs = dirs.join(" ");
    helper_argv(
        volume,
        false,
        "0",
        format!("mkdir -p {dirs} && chown 10001 {dirs} && chmod 700 {dirs}"),
    )
}

#[cfg(unix)]
fn execute(rt: &Runtime, argv: &[String]) -> Result<std::process::Output, String> {
    let mut cmd = rt.cmd();
    cmd.args(argv);
    output_within(cmd, TIMEOUT)
        .ok_or_else(|| "login runtime command could not start or timed out".into())
}

#[cfg(unix)]
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
fn is_archive(name: &str) -> bool {
    matches!(
        name,
        "claude.credentials.json"
            | "codex.auth.json"
            | "pi.auth.json"
            | "opencode.credentials.json"
    )
}

#[cfg(unix)]
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
fn ingest(
    rt: &Runtime,
    gw: &Gateway,
    dir: &Path,
    cli: &str,
    volume: &str,
) -> Result<IngestReport, String> {
    let owned = broker_files(cli)?;
    if !volume_exists(rt, volume)? {
        return Err(missing(cli, "login volume"));
    }
    if !volume_exists(rt, &volumes(gw).1)? {
        // codex lg-r3 R3: first sign-in provisions storage without taking daemon ownership.
        crate::seat_gateway::init_gateway_volumes(rt, gw, dir)?;
    }
    let input = read_volume(rt, volume, credential_paths(cli), false)?;
    let tokens = extract_tokens(cli, &input)?;
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
    crate::seat_gateway::set_broker_credential(
        rt,
        gw,
        dir,
        cli,
        &format!("{cli}.ingest.next"),
        &staged,
    )?;
    publish_ingest(rt, gw, dir, cli, volume, &tokens, owned)?;
    broker_report(rt, gw, cli).map(|report| IngestReport { skipped, ..report })
}

#[cfg(unix)]
fn publish_ingest(
    rt: &Runtime,
    gw: &Gateway,
    dir: &Path,
    cli: &str,
    volume: &str,
    tokens: &[(String, Vec<u8>)],
    owned: &[&str],
) -> Result<(), String> {
    for (name, bytes) in tokens {
        crate::seat_gateway::set_broker_credential(rt, gw, dir, cli, name, bytes)?;
    }
    let stale: Vec<&str> = owned
        .iter()
        .copied()
        .filter(|f| !tokens.iter().any(|(n, _)| n == f))
        .collect();
    if !stale.is_empty() {
        crate::seat_gateway::mutation_helper(
            rt,
            dir,
            cli,
            &revoke_argv(&volumes(gw).1, &stale),
            &[],
            TIMEOUT,
        )?;
    }
    // codex lg-r2 C3: a retry must never replay this now-obsolete login generation.
    write_session_state(dir, cli, volume, true)
}

#[cfg(unix)]
fn broker_report(rt: &Runtime, gw: &Gateway, cli: &str) -> Result<IngestReport, String> {
    let files = read_volume(rt, &volumes(gw).1, broker_files(cli)?, false)?;
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
    literals.validate(cli)?;
    Ok(literals)
}

/// Caller holds the broker lock. Repair sign-in before refresh checks expiry (codex lg-r2 C4).
#[cfg(unix)]
pub(crate) fn repair_ingest(
    rt: &Runtime,
    gw: &Gateway,
    dir: &Path,
    cli: &str,
) -> Result<bool, String> {
    if complete_sign_out(rt, gw, dir, cli)? {
        return Ok(false);
    }
    if !signin_pending(dir, cli) {
        return Ok(false);
    }
    let volume = read_session(dir, gw, cli)?.ok_or("incomplete sign-in has no session")?;
    if !session_committed(dir, cli)? {
        let Some(staged) = read_broker_record(rt, gw, &format!("{cli}.ingest.next"))? else {
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
        publish_ingest(rt, gw, dir, cli, &volume, &tokens, owned)?;
    }
    let report = broker_report(rt, gw, cli)?;
    crate::seat_gateway::route_sign_in(rt, gw, dir, cli, &report.literals)?;
    crate::seat_gateway::remove_broker_file(rt, gw, dir, cli, &format!("{cli}.ingest.next"))?;
    crate::seat_refresh::clear_pending(dir, cli)?;
    crate::seat_refresh::record_last(dir, cli, None);
    Ok(true)
}

pub(crate) fn signin_pending(dir: &Path, cli: &str) -> bool {
    std::fs::read(crate::seat_refresh::pending_path(dir, cli))
        .is_ok_and(|b| b == b"sign-in incomplete")
}

fn publication_status(dir: &Path, cli: &str, status: BrokerStatus) -> BrokerStatus {
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
pub(crate) fn broker_status(rt: &Runtime, gw: &Gateway, cli: &str) -> BrokerStatus {
    broker_reading(rt, gw, cli).0
}

/// The broker's status for `cli` and the route files it was read from (presence markers, plus
/// the retained OAuth record) — one read for both the status and the seat's credential kind.
#[cfg(unix)]
fn broker_reading(rt: &Runtime, gw: &Gateway, cli: &str) -> (BrokerStatus, Vec<(String, Vec<u8>)>) {
    let paths = match broker_files(cli) {
        Ok(paths) => paths,
        Err(e) => return (BrokerStatus::Unknown(e), Vec::new()),
    };
    let volume = volumes(gw).1;
    match volume_exists(rt, &volume) {
        Ok(false) => return (BrokerStatus::SignedOut, Vec::new()),
        Err(e) => return (BrokerStatus::Unknown(e), Vec::new()),
        Ok(true) => {}
    }
    let files = match read_volume(rt, &volume, paths, true) {
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
        "pi" | "opencode" => subscriptions(cli, files, now).ok()?.0,
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
    rt: &Runtime,
    gw: &Gateway,
    dir: &Path,
    cli: &str,
) -> Result<crate::seat_credentials::SeatSignIn, String> {
    // codex c6-r1: the broker's files are read one helper at a time; holding the mutation lock that
    // ingest, sign-out and refresh hold means the snapshot is never torn by one of them.
    let _lock = crate::seat_refresh::mutation_lock(dir)?;
    // agy c6-r1: an interrupted sign-in, sign-out or refresh is finished first, as ingest does,
    // so a crash mid-publication heals on the next unit instead of refusing every one after it.
    settle_broker(rt, gw, dir, cli)
        .map_err(|e| format!("{cli}'s sign-in could not be settled: {e}"))?;
    let (status, files) = broker_reading(rt, gw, cli);
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

/// Finish whatever broker mutation of `cli` a crash interrupted (caller holds the mutation lock):
/// a helper's cleanup, a sign-out tombstone, an ingest journal, a refresh's staged record. The
/// same order `ingest_and_route` repairs in.
#[cfg(unix)]
fn settle_broker(rt: &Runtime, gw: &Gateway, dir: &Path, cli: &str) -> Result<(), String> {
    crate::seat_gateway::cleanup_mutations(rt, dir)?;
    complete_sign_out(rt, gw, dir, cli)?;
    if signin_pending(dir, cli) {
        repair_ingest(rt, gw, dir, cli)?;
    } else {
        crate::seat_refresh::repair_refresh(rt, gw, dir, cli)?;
    }
    // codex c6-r3: a published sign-in whose cleanup a crash interrupted still has its login
    // volume (a copy of the credential) and session marker; finish them as ingest_and_route does.
    // An uncommitted session is a login that may still be open: it is never touched here.
    if let Some(volume) = read_session(dir, gw, cli)? {
        if session_committed(dir, cli)? {
            remove_login_volume(rt, &volume)?;
            crate::seat_refresh::remove_private_file(&session_path(dir, cli))?;
        }
    }
    Ok(())
}

/// Native Windows has no seat sign-ins (the container floor needs a Unix host).
#[cfg(not(unix))]
pub(crate) fn seat_sign_in(
    _rt: &crate::container_floor::Runtime,
    _gw: &crate::seat_gateway::Gateway,
    _dir: &Path,
    _cli: &str,
) -> Result<crate::seat_credentials::SeatSignIn, String> {
    Err("seat sign-ins need a Unix host".into())
}

/// Ingest, then point the gateway's routes at the new sign-in: codex's ChatGPT route carries the
/// account id as a literal header, so the served config is re-rendered and published.
#[cfg(unix)]
pub(crate) fn ingest_and_route(
    rt: &Runtime,
    gw: &Gateway,
    dir: &Path,
    cli: &str,
) -> Result<IngestReport, String> {
    // codex lg-r1 #2: hold the same broker lock from reading through publication.
    let _lock = crate::seat_refresh::mutation_lock(dir)?;
    crate::seat_gateway::cleanup_mutations(rt, dir)?;
    complete_sign_out(rt, gw, dir, cli)?;
    let volume = read_session(dir, gw, cli)?.ok_or_else(|| missing(cli, "login session"))?;
    // The terminal has ended: its container is stopped before the volume is read, so whatever the
    // CLI wrote is final and nothing still holds the volume when it is removed.
    stop_login_container(rt, &volume)?;
    repair_ingest(rt, gw, dir, cli)?;
    // codex lg-r3 R2: rescue or staging can survive even a failed pending-marker write.
    if !signin_pending(dir, cli) {
        crate::seat_refresh::repair_refresh(rt, gw, dir, cli)?;
    }
    let imported = !session_committed(dir, cli)?;
    let report = if imported {
        ingest(rt, gw, dir, cli, &volume)?
    } else {
        broker_report(rt, gw, cli)?
    };
    crate::seat_gateway::route_sign_in(rt, gw, dir, cli, &report.literals)?;
    crate::seat_gateway::remove_broker_file(rt, gw, dir, cli, &format!("{cli}.ingest.next"))?;
    crate::seat_refresh::clear_pending(dir, cli)?;
    // codex lg-r1 #8: the retry source survives every write/publish failure. codex lg-r4: the
    // session marker is the only record of the credential-bearing login volume, so it goes
    // LAST, after the volume is confirmed gone (a retry of a committed session whose volume is
    // already absent just finishes here).
    write_session_state(dir, cli, &volume, true)?;
    remove_login_volume(rt, &volume)?;
    match std::fs::remove_file(session_path(dir, cli)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.to_string()),
    }
    if imported {
        crate::seat_refresh::record_last(dir, cli, None);
    }
    // codex lg-r3 R4: only a committed sign-in may supersede a sign-out intent.
    crate::seat_refresh::remove_private_file(&signout_path(dir, cli))?;
    Ok(report)
}

/// Sign out, then clear what the routes carried for this agent (codex's account header).
#[cfg(unix)]
pub(crate) fn sign_out_and_route(
    rt: &Runtime,
    gw: &Gateway,
    dir: &Path,
    cli: &str,
) -> Result<(), String> {
    let _lock = crate::seat_refresh::mutation_lock(dir)?;
    crate::seat_gateway::cleanup_mutations(rt, dir)?;
    broker_files(cli)?;
    let tombstone = signout_path(dir, cli);
    // codex lg-r3 R4: a crash after the first empty write must never authorize recovery.
    if !tombstone.try_exists().map_err(|e| e.to_string())? {
        crate::seat_refresh::create_private_file(&tombstone, b"sign-out")?;
    }
    complete_sign_out(rt, gw, dir, cli).map(|_| ())
}

fn signout_path(dir: &Path, cli: &str) -> std::path::PathBuf {
    dir.join("refresh").join(format!("{cli}.signout"))
}

/// Caller holds the mutation lock. Tombstones always win over publication journals.
#[cfg(unix)]
pub(crate) fn complete_sign_out(
    rt: &Runtime,
    gw: &Gateway,
    dir: &Path,
    cli: &str,
) -> Result<bool, String> {
    let tombstone = signout_path(dir, cli);
    if !tombstone.try_exists().map_err(|e| e.to_string())? {
        return Ok(false);
    }
    // codex lg-r3 R4: retry even a prior tombstone creation whose fsync failed.
    crate::seat_refresh::sync_private_file(&tombstone)?;
    sign_out(rt, gw, dir, cli)?;
    if volume_exists(rt, &volumes(gw).1)? {
        for name in [
            format!("{cli}.ingest.next"),
            format!(
                "{}.next",
                crate::seat_refresh::record_file(cli).unwrap_or(cli)
            ),
        ] {
            crate::seat_gateway::remove_broker_file(rt, gw, dir, cli, &name)?;
        }
    }
    if let Some(volume) = read_session(dir, gw, cli)? {
        // codex lg-r3 R4: retry a crash between removal of the volume and its session marker.
        if volume_exists(rt, &volume)? {
            remove_login_volume(rt, &volume)?;
        } else {
            stop_login_container(rt, &volume)?;
        }
        crate::seat_refresh::remove_private_file(&session_path(dir, cli))?;
    }
    crate::seat_refresh::remove_private_file(&crate::seat_refresh::rescue_path(dir, cli))?;
    crate::seat_refresh::clear_pending(dir, cli)?;
    crate::seat_gateway::confirm_sign_out(rt, gw, dir, cli)?;
    crate::seat_refresh::record_last(dir, cli, None);
    crate::seat_refresh::remove_private_file(&tombstone)?;
    Ok(true)
}

#[cfg(unix)]
fn sign_out(rt: &Runtime, gw: &Gateway, dir: &Path, cli: &str) -> Result<(), String> {
    let files = broker_files(cli)?;
    let volume = volumes(gw).1;
    if !volume_exists(rt, &volume)? {
        return Ok(());
    }
    crate::seat_gateway::mutation_helper(rt, dir, cli, &revoke_argv(&volume, files), &[], TIMEOUT)
        .map(|_| ())
}

fn session_path(dir: &Path, cli: &str) -> std::path::PathBuf {
    dir.join("login").join(format!("{cli}.session"))
}

// codex lg-r1 #10: never infer a volume from a CLI; ingest exactly the recorded session.
fn read_session(
    dir: &Path,
    gw: &crate::seat_gateway::Gateway,
    cli: &str,
) -> Result<Option<String>, String> {
    let name = match std::fs::read_to_string(session_path(dir, cli)) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    let (name, state) = name.split_once('\n').unwrap_or((&name, ""));
    if !matches!(state, "" | "committed") {
        return Err("invalid login session state".into());
    }
    let prefix = format!("{}-login-{cli}-", gw.name);
    if !name
        .strip_prefix(&prefix)
        .is_some_and(|n| n.len() == 8 && n.bytes().all(|c| c.is_ascii_hexdigit()))
    {
        return Err("invalid login session name".into());
    }
    Ok(Some(name.to_string()))
}

#[cfg(unix)]
fn new_session(rt: &Runtime, gw: &Gateway, dir: &Path, cli: &str) -> Result<String, String> {
    let _lock = crate::seat_refresh::mutation_lock(dir)?;
    crate::seat_gateway::cleanup_mutations(rt, dir)?;
    complete_sign_out(rt, gw, dir, cli)?;
    if signin_pending(dir, cli) && !repair_ingest(rt, gw, dir, cli)? {
        return Err("sign-in incomplete; retry ingest first".into());
    }
    let previous = read_session(dir, gw, cli)?;
    if let Some(old) = &previous {
        remove_login_volume(rt, old)?;
        std::fs::remove_file(session_path(dir, cli)).map_err(|e| e.to_string())?;
    }
    let volume = loop {
        let candidate = login_volume(&gw.name, cli);
        if previous.as_ref() != Some(&candidate) && !volume_exists(rt, &candidate)? {
            break candidate;
        }
    };
    checked(rt, &["volume".into(), "create".into(), volume.clone()])?;
    write_session(dir, cli, &volume)?;
    crate::seat_gateway::mutation_helper(
        rt,
        dir,
        cli,
        &init_login_argv(&volume, cli),
        &[],
        TIMEOUT,
    )?;
    Ok(volume)
}

/// Stop and remove the login container of session `volume`, if it still runs. Its terminal is
/// over whenever this is called (ingest runs on the terminal's exit), but a terminal whose client
/// was killed leaves `docker run -it` behind, still holding the volume: every later volume
/// removal, and so every later sign-in, would fail (found on the proof daemon, C6).
#[cfg(unix)]
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

fn ours_on(volume: &str, name: &str) -> bool {
    name == login_container(volume)
        || name.strip_prefix("wicked-login-").is_some_and(|rest| {
            rest.rsplit_once('-').is_some_and(|(cli, id)| {
                supported_cli(cli) && id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
            })
        })
}

/// Remove a login volume (stopping its login container first), treating one that is already
/// gone as removed (a crash after the removal and before its marker was unlinked).
#[cfg(unix)]
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
fn gateway_for_dir(dir: Option<&Path>) -> Result<crate::seat_gateway::Gateway, String> {
    let dir = dir.filter(|p| p.is_absolute()).ok_or_else(|| {
        format!("set {GATEWAY_DIR_ENV} to the daemon's absolute gateway directory")
    })?;
    let name = crate::seat_gateway::gateway_name(dir);
    Ok(crate::seat_gateway::Gateway {
        external_network: format!("{name}-ext"),
        name,
    })
}

#[cfg(unix)]
fn prepare_login_argv(
    rt: &Runtime,
    gw: &Gateway,
    dir: &Path,
    cli: &str,
    image: &str,
) -> Result<Vec<String>, String> {
    // C6: an agent whose credential the broker cannot hold never opens a login (the user would
    // complete a real sign-in only for ingest to discard it).
    broker_files(cli).map_err(|e| format!("{cli} cannot sign in for container seats yet: {e}"))?;
    // codex lg-r3 R3: only the daemon may take ownership and revoke stale unit keys.
    crate::seat_gateway::ensure_external_bridge(rt, gw)?;
    let volume = new_session(rt, gw, dir, cli)?;
    Ok(login_argv(
        &rt.cli,
        cli,
        image,
        &volume,
        &gw.external_network,
    ))
}

#[cfg(unix)]
fn dispatch(verb: &str, cli: &str) -> Result<i32, String> {
    let dir = std::env::var_os(GATEWAY_DIR_ENV).map(std::path::PathBuf::from);
    let gw = gateway_for_dir(dir.as_deref())?;
    let rt = crate::container_floor::runtime()
        .map_err(|why| format!("a container runtime is required: {}", why.describe()))?;
    match verb {
        "argv" => {
            let image = crate::container_floor::ensure_seat_image(&rt, cli)?;
            let argv = prepare_login_argv(
                &rt,
                &gw,
                dir.as_deref().expect("validated directory"),
                cli,
                &image,
            )?;
            println!(
                "{}",
                serde_json::to_string(&argv).map_err(|_| "could not encode login argv")?
            );
            Ok(0)
        }
        "ingest" => {
            let dir = dir.as_deref().expect("validated by gateway_for_dir");
            let report = ingest_and_route(&rt, &gw, dir, cli)?;
            println!(
                "{}",
                serde_json::to_string(&report).map_err(|_| "could not encode ingest report")?
            );
            Ok(0)
        }
        "status" => {
            let status = with_last_refresh(
                broker_status(&rt, &gw, cli),
                dir.as_deref()
                    .and_then(|d| crate::seat_refresh::last_refresh(d, cli)),
            );
            let status =
                publication_status(dir.as_deref().expect("validated directory"), cli, status);
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
            let dir = dir.as_deref().expect("validated by gateway_for_dir");
            sign_out_and_route(&rt, &gw, dir, cli)?;
            Ok(0)
        }
        "refresh" => {
            let dir = dir.as_deref().expect("validated by gateway_for_dir");
            let outcome = crate::seat_refresh::refresh(&rt, &gw, dir, cli, true)?;
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
if [ "$1 $2" = 'network create' ]; then
  [ ! -e "$base/network" ] || exit 1
  printf 'bridge false\n' > "$base/network"; exit 0
fi
if [ "$1 $2" = 'network inspect' ]; then cat "$base/network"; exit $?; fi
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
      *) cat "$base/input";;
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
                rt: Runtime { cli: path },
                gw,
            }
        }
        fn calls(&self) -> String {
            std::fs::read_to_string(self.dir.join("calls")).unwrap_or_default()
        }
    }

    #[cfg(unix)]
    impl Drop for FakeRuntime {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[cfg(unix)]
    #[test]
    fn credential_containers_all_disable_runtime_logging_and_keep_stdio() {
        for cli in ["claude", "codex", "pi", "opencode", "copilot"] {
            let mut argvs = vec![
                login_argv(
                    Path::new("/FAKE-runtime"),
                    cli,
                    "FAKE-image",
                    "FAKE-login",
                    "FAKE-ext",
                ),
                init_login_argv("FAKE-login", cli),
            ];
            for path in credential_paths(cli) {
                argvs.push(read_argv("FAKE-login", path, false));
            }
            if let Ok(files) = broker_files(cli) {
                argvs.push(revoke_argv("FAKE-broker", files));
                for file in files {
                    argvs.push(read_argv("FAKE-broker", file, true));
                    argvs.push(read_argv("FAKE-broker", file, false));
                }
            }
            for argv in argvs {
                assert!(
                    argv.windows(2).any(|w| w == ["--log-driver", "none"]),
                    "{argv:?}"
                );
            }
        }
        let fake = FakeRuntime::new();
        let _lock = crate::seat_refresh::mutation_lock(&fake.dir).unwrap();
        crate::seat_gateway::init_gateway_volumes(&fake.rt, &fake.gw, &fake.dir).unwrap();
        crate::seat_gateway::set_broker_credential(
            &fake.rt,
            &fake.gw,
            &fake.dir,
            "claude",
            "claude.credentials.json",
            b"FAKE-record",
        )
        .unwrap();
        assert_eq!(
            read_broker_record(&fake.rt, &fake.gw, "claude.credentials.json")
                .unwrap()
                .unwrap(),
            b"FAKE-record"
        );
        crate::seat_gateway::remove_broker_file(
            &fake.rt,
            &fake.gw,
            &fake.dir,
            "claude",
            "claude.credentials.json.next",
        )
        .unwrap();
        for line in fake.calls().lines().filter(|l| l.starts_with("run ")) {
            assert!(line.contains("--log-driver none"), "{line}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn login_bridge_and_first_ingest_preserve_live_keys_without_gateway_takeover() {
        use crate::seat_gateway::{configure_gateway, register_unit, GatewayConfig, UnitKey};
        let fake = FakeRuntime::new();
        configure_gateway(&fake.dir, &GatewayConfig::default()).unwrap();
        let live = UnitKey::new("FAKE-run", "FAKE-unit", "claude");
        register_unit(&fake.dir, live.clone()).unwrap();
        let before = std::fs::read(fake.dir.join("config/units.json")).unwrap();
        let argv =
            prepare_login_argv(&fake.rt, &fake.gw, &fake.dir, "codex", "FAKE-image").unwrap();
        assert!(argv
            .windows(2)
            .any(|w| w[0] == "--network" && w[1] == fake.gw.external_network));
        prepare_login_argv(&fake.rt, &fake.gw, &fake.dir, "codex", "FAKE-image").unwrap();
        assert_eq!(
            std::fs::read(fake.dir.join("config/units.json")).unwrap(),
            before
        );
        assert!(!fake.calls().contains("container inspect"));
        assert!(!fake.calls().contains("cat > /v/"));
        std::fs::write(fake.dir.join("network"), "bridge true").unwrap();
        assert!(prepare_login_argv(&fake.rt, &fake.gw, &fake.dir, "codex", "FAKE-image").is_err());
        std::fs::write(fake.dir.join("network"), "overlay false").unwrap();
        assert!(prepare_login_argv(&fake.rt, &fake.gw, &fake.dir, "codex", "FAKE-image").is_err());

        // First ingest has no broker volume yet; initialization keeps existing files, too.
        std::fs::write(fake.dir.join("volumes"), "").unwrap();
        std::fs::write(fake.dir.join("broker/anthropic.token"), "FAKE-live-claude").unwrap();
        new_session(&fake.rt, &fake.gw, &fake.dir, "codex").unwrap();
        std::fs::write(fake.dir.join("input"), chatgpt().to_string()).unwrap();
        ingest_and_route(&fake.rt, &fake.gw, &fake.dir, "codex").unwrap();
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
        assert!(!calls.contains(crate::seat_gateway::GATEWAY_IMAGE));
        assert!(std::fs::read_to_string(fake.dir.join("published"))
            .unwrap()
            .contains(&live.key));
        assert!(!fake.dir.join("config/revoking.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn refresh_recovery_prefers_rescue_and_refuses_an_unstored_rotation() {
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
        assert!(
            crate::seat_refresh::repair_refresh(&fake.rt, &fake.gw, &fake.dir, "claude").is_err()
        );
        assert!(rescue.exists());
        assert_eq!(
            std::fs::read_to_string(fake.dir.join("broker/claude.credentials.json")).unwrap(),
            old.to_string()
        );
        std::fs::remove_file(fake.dir.join("fail_claude.credentials.json.next")).unwrap();
        crate::seat_refresh::repair_refresh(&fake.rt, &fake.gw, &fake.dir, "claude").unwrap();
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
                crate::seat_refresh::repair_refresh(&fake.rt, &fake.gw, &fake.dir, "claude")
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
        crate::seat_refresh::repair_refresh(&fake.rt, &fake.gw, &fake.dir, "claude").unwrap();
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
        crate::seat_refresh::repair_refresh(&fake.rt, &fake.gw, &fake.dir, "claude").unwrap();
        assert_eq!(
            std::fs::read_to_string(fake.dir.join("broker/claude.credentials.json")).unwrap(),
            rescued.to_string()
        );
        assert!(!pending.exists());
    }

    #[cfg(unix)]
    #[test]
    fn interrupted_sign_out_tombstone_wins_over_every_recovery_source() {
        for recovery in 0..3 {
            let fake = FakeRuntime::new();
            new_session(&fake.rt, &fake.gw, &fake.dir, "claude").unwrap();
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
            assert!(sign_out_and_route(&fake.rt, &fake.gw, &fake.dir, "claude").is_err());
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
                    0 => {
                        crate::seat_refresh::refresh(&fake.rt, &fake.gw, &fake.dir, "claude", false)
                            .map(|_| ())
                    }
                    1 => {
                        let _lock = crate::seat_refresh::mutation_lock(&fake.dir)?;
                        repair_ingest(&fake.rt, &fake.gw, &fake.dir, "claude").map(|_| ())
                    }
                    _ => {
                        let _lock = crate::seat_refresh::mutation_lock(&fake.dir)?;
                        crate::seat_refresh::repair_refresh(&fake.rt, &fake.gw, &fake.dir, "claude")
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
        let fake = FakeRuntime::new();
        let session = new_session(&fake.rt, &fake.gw, &fake.dir, "codex").unwrap();
        std::fs::write(fake.dir.join("calls"), "").unwrap();
        let mut invalid = chatgpt();
        invalid["tokens"]["account_id"] = json!("FAKE\r\nx-evil: 1");
        std::fs::write(fake.dir.join("input"), invalid.to_string()).unwrap();
        assert!(ingest_and_route(&fake.rt, &fake.gw, &fake.dir, "codex").is_err());
        assert!(!fake.calls().contains("cat > /v/"));
        assert_eq!(
            read_session(&fake.dir, &fake.gw, "codex").unwrap(),
            Some(session.clone())
        );
        std::fs::write(fake.dir.join("input"), chatgpt().to_string()).unwrap();
        std::fs::write(fake.dir.join("fail_publish"), "").unwrap();
        assert!(ingest_and_route(&fake.rt, &fake.gw, &fake.dir, "codex").is_err());
        assert!(fake.calls().contains("cat > /v/.openai.token.new"));
        assert!(!fake.calls().contains("volume rm"));
        assert_eq!(
            read_session(&fake.dir, &fake.gw, "codex").unwrap(),
            Some(session)
        );
        std::fs::remove_file(fake.dir.join("fail_publish")).unwrap();
        std::fs::write(fake.dir.join("fail_remove"), "").unwrap();
        assert!(ingest_and_route(&fake.rt, &fake.gw, &fake.dir, "codex").is_err());
        assert!(read_session(&fake.dir, &fake.gw, "codex")
            .unwrap()
            .is_some());
        std::fs::remove_file(fake.dir.join("fail_remove")).unwrap();
        ingest_and_route(&fake.rt, &fake.gw, &fake.dir, "codex").unwrap();
        assert!(fake.calls().contains("volume rm"));
        assert!(read_session(&fake.dir, &fake.gw, "codex")
            .unwrap()
            .is_none());
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
        let first = new_session(&fake.rt, &fake.gw, &fake.dir, "codex").unwrap();
        let second = new_session(&fake.rt, &fake.gw, &fake.dir, "codex").unwrap();
        assert_ne!(first, second);
        let calls = fake.calls();
        assert!(
            calls.find(&format!("volume rm {first}")).unwrap()
                < calls.find(&format!("volume create {second}")).unwrap()
        );
        assert_eq!(
            read_session(&fake.dir, &fake.gw, "codex").unwrap(),
            Some(second.clone())
        );
        assert_eq!(
            std::fs::metadata(session_path(&fake.dir, "codex"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        std::fs::write(fake.dir.join("input"), chatgpt().to_string()).unwrap();
        std::fs::write(fake.dir.join("calls"), "").unwrap();
        ingest_and_route(&fake.rt, &fake.gw, &fake.dir, "codex").unwrap();
        assert!(fake.calls().contains(&format!("{second}:/in:ro")));
        assert!(!fake.calls().contains(&first));
    }

    #[cfg(unix)]
    #[test]
    fn ingest_and_sign_out_block_behind_refresh_and_do_not_mutate_early() {
        for sign_out in [false, true] {
            let fake = FakeRuntime::new();
            new_session(&fake.rt, &fake.gw, &fake.dir, "codex").unwrap();
            std::fs::write(fake.dir.join("input"), chatgpt().to_string()).unwrap();
            std::fs::write(fake.dir.join("calls"), "").unwrap();
            let held = crate::seat_refresh::mutation_lock(&fake.dir).unwrap();
            let (tx, rx) = std::sync::mpsc::channel();
            let (rt, gw, dir) = (fake.rt.clone(), fake.gw.clone(), fake.dir.clone());
            let worker = std::thread::spawn(move || {
                tx.send(()).unwrap();
                if sign_out {
                    sign_out_and_route(&rt, &gw, &dir, "codex")
                } else {
                    ingest_and_route(&rt, &gw, &dir, "codex").map(|_| ())
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
        let fake = FakeRuntime::new();
        std::fs::write(fake.dir.join("fail_probe"), "").unwrap();
        let error = sign_out_and_route(&fake.rt, &fake.gw, &fake.dir, "claude").unwrap_err();
        assert!(error.contains("sign-out could not confirm"));
        assert!(fake.calls().contains("cat > /v/.config.json.new"));
        assert!(fake.calls().contains("read K;"));
        std::fs::remove_file(fake.dir.join("fail_probe")).unwrap();
        sign_out_and_route(&fake.rt, &fake.gw, &fake.dir, "claude").unwrap();
    }

    /// codex c6-r3: arm finishes a published sign-in's interrupted cleanup (its login volume holds
    /// a copy of the credential), and never touches a session that may be a login still open.
    #[cfg(unix)]
    #[test]
    fn arm_finishes_a_committed_sign_ins_cleanup_and_leaves_an_open_login_alone() {
        let fake = FakeRuntime::new();
        std::fs::write(fake.dir.join("broker/pi.token"), "FAKE-key").unwrap();
        let open = new_session(&fake.rt, &fake.gw, &fake.dir, "pi").unwrap();
        assert_eq!(
            seat_sign_in(&fake.rt, &fake.gw, &fake.dir, "pi").unwrap(),
            crate::seat_credentials::CredentialKind::ProviderKey.into()
        );
        assert_eq!(
            read_session(&fake.dir, &fake.gw, "pi").unwrap(),
            Some(open.clone())
        );
        assert!(!fake.calls().contains(&format!("volume rm {open}")));
        write_session_state(&fake.dir, "pi", &open, true).unwrap();
        assert!(seat_sign_in(&fake.rt, &fake.gw, &fake.dir, "pi").is_ok());
        assert_eq!(read_session(&fake.dir, &fake.gw, "pi").unwrap(), None);
        assert!(fake.calls().contains(&format!("volume rm {open}")));
    }

    #[cfg(unix)]
    #[test]
    fn committed_ingest_retry_preserves_since_rotated_record() {
        let fake = FakeRuntime::new();
        new_session(&fake.rt, &fake.gw, &fake.dir, "codex").unwrap();
        std::fs::write(fake.dir.join("input"), chatgpt().to_string()).unwrap();
        std::fs::write(fake.dir.join("fail_publish"), "").unwrap();
        assert!(ingest_and_route(&fake.rt, &fake.gw, &fake.dir, "codex").is_err());
        assert!(session_committed(&fake.dir, "codex").unwrap());
        let mut rotated = chatgpt();
        rotated["tokens"]["refresh_token"] = json!("FAKE-rotated-R2");
        std::fs::write(fake.dir.join("broker/codex.auth.json"), rotated.to_string()).unwrap();
        std::fs::remove_file(fake.dir.join("fail_publish")).unwrap();
        std::fs::write(fake.dir.join("calls"), "").unwrap();
        ingest_and_route(&fake.rt, &fake.gw, &fake.dir, "codex").unwrap();
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
        new_session(&fake.rt, &fake.gw, &fake.dir, "claude").unwrap();
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
        assert!(ingest_and_route(&fake.rt, &fake.gw, &fake.dir, "claude").is_err());
        assert!(
            matches!(publication_status(&fake.dir, "claude", BrokerStatus::SignedOut), BrokerStatus::Unknown(ref reason) if reason == "sign-in incomplete")
        );
        std::fs::remove_file(fake.dir.join("fail_anthropic.token")).unwrap();
        assert!(matches!(
            crate::seat_refresh::refresh(&fake.rt, &fake.gw, &fake.dir, "claude", false).unwrap(),
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
            let result = sign_out_and_route(&fake.rt, &fake.gw, &fake.dir, "codex");
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
            crate::seat_refresh::refresh(&fake.rt, &fake.gw, &fake.dir, "claude", false).unwrap(),
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
                broker_status(&fake.rt, &fake.gw, "claude"),
                crate::seat_refresh::last_refresh(&fake.dir, "claude")
            ),
            BrokerStatus::SignedIn { .. }
        ));
        assert!(!fake.calls().contains(crate::seat_refresh::REFRESH_IMAGE));
    }

    #[cfg(unix)]
    #[test]
    fn sign_out_refuses_unsettled_writer_before_emptying_any_credentials() {
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
        assert!(sign_out_and_route(&fake.rt, &fake.gw, &fake.dir, "claude").is_err());
        assert!(!fake.calls().contains(": > /in/"));
        assert!(crate::seat_gateway::unsettled_path(&fake.dir, "claude").exists());
        std::fs::remove_file(fake.dir.join("fail_cleanup")).unwrap();
        std::fs::write(fake.dir.join("state"), "stopped").unwrap();
        sign_out_and_route(&fake.rt, &fake.gw, &fake.dir, "claude").unwrap();
        assert!(!crate::seat_gateway::unsettled_path(&fake.dir, "claude").exists());
        assert!(fake.calls().contains(": > /in/anthropic.token"));
    }

    #[cfg(unix)]
    #[test]
    fn sign_out_absent_volumes_succeeds_and_existing_config_volume_is_published() {
        for config_exists in [false, true] {
            let fake = FakeRuntime::new();
            std::fs::write(fake.dir.join("state"), "absent").unwrap();
            std::fs::write(
                fake.dir.join("volumes"),
                if config_exists {
                    format!("{}\n{}\n", volumes(&fake.gw).0, volumes(&fake.gw).1)
                } else {
                    String::new()
                },
            )
            .unwrap();
            sign_out_and_route(&fake.rt, &fake.gw, &fake.dir, "codex").unwrap();
            assert_eq!(
                fake.calls().contains("cat > /v/.config.json.new"),
                config_exists
            );
            assert!(!fake.calls().contains("read K;"));
        }
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

    #[test]
    fn windows_login_helpers_are_available_without_dead_code_warnings() {
        // agy lg-r1 A7: this is a build-configuration contract, also asserted on Unix lanes.
        assert!(include_str!("seat_login.rs")
            .split("use std")
            .next()
            .unwrap()
            .contains("#![cfg_attr(not(unix), allow(dead_code))]"));
        assert!(include_str!("seat_gateway.rs")
            .split("use std")
            .next()
            .unwrap()
            .contains("#![cfg_attr(not(unix), allow(dead_code))]"));
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

    #[test]
    fn login_argv_per_cli_has_hardening_isolated_home_and_native_command() {
        for (cli, command, config_env) in [
            (
                "claude",
                vec!["claude", "auth", "login"],
                vec!["CLAUDE_CONFIG_DIR=/home/login/claude"],
            ),
            (
                "codex",
                vec!["codex", "login", "--device-auth"],
                vec!["CODEX_HOME=/home/login/codex"],
            ),
            ("pi", vec!["pi"], vec!["PI_CODING_AGENT_DIR=/home/login/pi"]),
            (
                "opencode",
                vec!["sh", "-c", OPENCODE_LOGIN],
                vec![
                    "XDG_CONFIG_HOME=/home/login/config",
                    "XDG_DATA_HOME=/home/login/data",
                ],
            ),
            ("copilot", vec!["copilot", "login"], vec![]),
        ] {
            let volume = login_volume("wicked-agw-test", cli);
            let argv = login_argv(
                Path::new("/runtime"),
                cli,
                "fake-image:1",
                &volume,
                "fake-gateway-ext",
            );
            assert_eq!(&argv[..4], ["/runtime", "run", "-it", "--rm"], "{cli}");
            let pair =
                |flag: &str, value: &str| argv.windows(2).any(|w| w[0] == flag && w[1] == value);
            for (flag, value) in [
                ("--user", "10001"),
                ("--tmpfs", "/tmp:rw,exec,mode=1777"),
                ("--cap-drop", "ALL"),
                ("--security-opt", "no-new-privileges"),
                ("--network", "fake-gateway-ext"),
                ("-e", "HOME=/home/login"),
            ] {
                assert!(pair(flag, value), "{cli}: {flag}");
            }
            assert!(argv.iter().any(|a| a == "--read-only"));
            assert!(pair("-v", &format!("{volume}:/home/login")));
            let name = &argv[argv.iter().position(|a| a == "--name").unwrap() + 1];
            assert_eq!(name, &volume, "the container is named after its session");
            assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
            for env in config_env {
                assert!(pair("-e", env), "{cli}: {env}");
            }
            let image = argv.iter().position(|a| a == "fake-image:1").unwrap();
            assert_eq!(&argv[image + 1..], command.as_slice());
            let encoded = serde_json::to_string(&argv).unwrap();
            for forbidden in [
                "ANTHROPIC_API_KEY",
                "OPENAI_API_KEY",
                "COPILOT_HOME",
                "/broker",
                "@",
                "FAKE-secret",
            ] {
                assert!(!encoded.contains(forbidden), "{cli}: {forbidden}");
            }
            assert_ne!(
                name,
                &login_argv(
                    Path::new("/runtime"),
                    cli,
                    "fake-image:1",
                    &volume,
                    "fake-gateway-ext"
                )[5]
            );
        }
    }

    /// Regression (codex sign-in on the container floor): the login container's configuration
    /// homes exist before the CLI starts. `codex login` exits at config load when `CODEX_HOME`
    /// is missing, so the login wrote nothing and ingest reported "missing auth.json".
    #[test]
    fn init_creates_every_configuration_home_the_login_names() {
        for cli in ["claude", "codex", "pi", "opencode", "copilot"] {
            let argv = login_argv(Path::new("/rt"), cli, "img", "vol", "net");
            let script = init_login_argv("vol", cli).last().unwrap().clone();
            let (mkdir, rest) = script.split_once(" && ").unwrap();
            let created: Vec<&str> = mkdir
                .strip_prefix("mkdir -p ")
                .unwrap()
                .split(' ')
                .collect();
            assert_eq!(
                created.last(),
                Some(&"/in"),
                "{cli}: the volume root, handed over last"
            );
            assert_eq!(
                rest,
                format!("chown 10001 {0} && chmod 700 {0}", created.join(" ")),
                "{cli}: every created directory is the login user's, private"
            );
            let homes: Vec<String> = argv
                .windows(2)
                .filter(|w| w[0] == "-e")
                .filter_map(|w| w[1].split_once("=/home/login/"))
                .map(|(_, dir)| format!("/in/{dir}"))
                .collect();
            assert_eq!(homes.len(), login_homes(cli).len(), "{cli}");
            for home in &homes {
                assert!(created.contains(&home.as_str()), "{cli}: {home} is created");
            }
        }
        let codex = init_login_argv("vol", "codex");
        assert!(codex
            .last()
            .unwrap()
            .starts_with("mkdir -p /in/codex /in && "));
    }

    /// The same regression against the real codex seat image: after the init helper, the
    /// login's `CODEX_HOME` loads (signed out), where before it failed at config load.
    #[cfg(unix)]
    #[test]
    fn live_codex_login_home_exists_before_the_login_runs() {
        let _env = crate::test_env::ENV_LOCK
            .read()
            .unwrap_or_else(|p| p.into_inner());
        if std::env::var("WICKED_CONTAINER_LIVE").ok().as_deref() != Some("1") {
            eprintln!("skipped: set WICKED_CONTAINER_LIVE=1 with a container runtime to run");
            return;
        }
        let rt = crate::container_floor::runtime().expect("a container runtime answers");
        let image = crate::container_floor::ensure_seat_image(&rt, "codex").unwrap();
        let volume = login_volume("wicked-agw-livehome", "codex");
        let _ = execute(&rt, &["volume".into(), "rm".into(), volume.clone()]);
        checked(&rt, &["volume".into(), "create".into(), volume.clone()]).unwrap();
        let status = |init: bool| {
            if init {
                checked(&rt, &init_login_argv(&volume, "codex")).unwrap();
            }
            let mut argv = login_argv(&rt.cli, "codex", &image, &volume, "none");
            argv.retain(|a| a != "-it");
            let at = argv.iter().position(|a| *a == image).unwrap();
            argv.truncate(at + 1);
            argv.extend(["codex", "login", "status"].map(String::from));
            let out = execute(&rt, &argv[1..]).unwrap();
            String::from_utf8_lossy(&out.stdout).into_owned()
                + &String::from_utf8_lossy(&out.stderr)
        };
        let before = status(false);
        let after = status(true);
        let _ = execute(&rt, &["volume".into(), "rm".into(), volume.clone()]);
        assert!(
            before.contains("does not exist"),
            "the bug reproduces: {before}"
        );
        assert!(!after.contains("does not exist"), "{after}");
        assert!(after.contains("Not logged in"), "{after}");
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
            .contains("not configured"));
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
    fn helper_argv_bounds_reads_and_never_sends_credentials_or_host_mounts() {
        for cli in ["claude", "codex", "pi", "opencode"] {
            for path in credential_paths(cli) {
                let volume = login_volume("wicked-agw-test", cli);
                assert!(volume.starts_with(&format!("wicked-agw-test-login-{cli}-")));
                let argv = read_argv(&volume, path, false);
                assert!(argv.windows(2).any(|w| w == ["--network", "none"]));
                assert!(argv.windows(2).any(|w| w == ["--user", "10001"]));
                assert!(argv.iter().any(|s| s == &format!("{volume}:/in:ro")));
                assert!(argv.iter().any(|s| s == crate::seat_gateway::HELPER_IMAGE));
                assert!(argv
                    .last()
                    .unwrap()
                    .contains(&format!("head -c {}", MAX_FILE_BYTES + 1)));
                assert!(argv.last().unwrap().contains("exit 44"));
                assert!(argv.last().unwrap().contains("! -L"));
                assert!(!serde_json::to_string(&argv)
                    .unwrap()
                    .contains("FAKE-secret"));
            }
        }
        let presence = read_argv("fake-broker", "anthropic.token", true);
        assert!(presence.last().unwrap().contains("printf present"));
        assert!(!presence.last().unwrap().contains("head -c"));
        let init = init_login_argv("wicked-login-claude", "claude");
        assert!(init
            .last()
            .unwrap()
            .contains("chown 10001 /in/claude /in && chmod 700 /in/claude /in"));
        assert!(init.windows(2).any(|w| w == ["--cap-add", "CHOWN"]));
    }

    #[test]
    fn sign_out_argv_empties_all_cli_files_and_preserves_other_agents() {
        for cli in ["claude", "codex", "pi", "opencode"] {
            let owned = broker_files(cli).unwrap();
            let argv = revoke_argv("fake-broker", owned);
            let script = argv.last().unwrap();
            assert!(script.starts_with(": > /in/") && !script.contains("rm "));
            for filename in owned {
                assert!(script
                    .split_whitespace()
                    .any(|p| p == format!("/in/{filename}")));
            }
            for other in ["claude", "codex", "pi", "opencode"]
                .into_iter()
                .filter(|c| *c != cli)
            {
                for filename in broker_files(other).unwrap() {
                    assert!(!script
                        .split_whitespace()
                        .any(|p| p == format!("/in/{filename}")));
                }
            }
        }
    }

    /// codex c6-r1: only the session's own login container, or one an earlier engine named
    /// `wicked-login-<cli>-<32 hex>`, is stopped on its volume's behalf.
    #[test]
    fn only_login_containers_are_stopped_for_a_session_volume() {
        let v = "wicked-agw-0123456789ab-login-claude-0a1b2c3d";
        assert!(ours_on(v, v));
        assert!(ours_on(
            v,
            &format!("wicked-login-claude-{}", "a".repeat(32))
        ));
        assert!(ours_on(
            v,
            &format!("wicked-login-opencode-{}", "0".repeat(32))
        ));
        for other in [
            "wicked-seat-claude-run-1",
            "wicked-login-claude-short",
            &format!("wicked-login-bogus-{}", "a".repeat(32)),
            &format!("wicked-login-claude-{}", "g".repeat(32)),
            "wicked-agw-0123456789ab-login-claude-0a1b2c3e",
            "some-user-container",
        ] {
            assert!(!ours_on(v, other), "{other}");
        }
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
        let gw = gateway_for_dir(Some(dir)).unwrap();
        assert_eq!(gw.name, crate::seat_gateway::gateway_name(dir));
        assert_eq!(gw.external_network, format!("{}-ext", gw.name));
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

    /// LIVE (`WICKED_CONTAINER_LIVE=1` + a runtime; skipped otherwise): a codex ChatGPT sign-in,
    /// end to end with FAKE credentials. A login volume holding `codex/auth.json` is ingested into
    /// the broker; the seat (placeholder bearer = its unit key, placeholder account header) reaches
    /// a stub upstream only through the gateway, which sends the broker's token and the signed-in
    /// account; status says signed in. Sign-out removes both: the upstream sees neither again and
    /// status says signed out. Neither the token nor the key reaches the gateway's logs.
    #[cfg(unix)]
    #[test]
    fn live_a_codex_sign_in_reaches_the_upstream_through_the_broker_and_sign_out_removes_it() {
        use crate::seat_gateway::{
            attach, configure_gateway, ensure_gateway, register_live, GatewayConfig, ModelRoute,
            Provider, UnitKey,
        };
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        if std::env::var("WICKED_CONTAINER_LIVE").ok().as_deref() != Some("1") {
            eprintln!("skipped: set WICKED_CONTAINER_LIVE=1 with a container runtime to run");
            return;
        }
        let rt = crate::container_floor::runtime().expect("a container runtime answers");
        let image = crate::container_floor::ensure_seat_image(&rt, "claude").unwrap();
        let dir = std::env::temp_dir().join(format!(
            "wicked-login-live-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(dir.join("stub")).unwrap();
        let gw = Gateway::for_dir(&dir);
        let run = |argv: &[&str]| {
            let mut c = rt.cmd();
            c.args(argv);
            output_within(c, Duration::from_secs(60)).unwrap()
        };
        let stub = format!("{}-stub", gw.name);
        // The stub echoes the two headers the ChatGPT backend authenticates by.
        std::fs::write(
            dir.join("stub/stub.js"),
            "const h=require('http');h.createServer((q,r)=>{q.resume();q.on('end',()=>{\
             r.setHeader('content-type','application/json');\
             r.end(JSON.stringify({auth:q.headers['authorization']||'',\
             acct:q.headers['chatgpt-account-id']||''}))})}).listen(8080);\
             h.createServer((q,r)=>{let b='';q.on('data',d=>b+=d);q.on('end',()=>{\
             console.log('TOKEN-REQUEST '+q.url+' '+b);\
             r.setHeader('content-type','application/json');\
             if(q.url!=='/token'){r.statusCode=400;return r.end('{\"error\":\"invalid_grant\"}')}\
             r.end(JSON.stringify({access_token:'FAKE-codex-access-v2',expires_in:3600}))})}).listen(8081)",
        )
        .unwrap();
        configure_gateway(
            &dir,
            &GatewayConfig {
                units: vec![],
                providers: vec![ModelRoute {
                    port: 4002,
                    provider: Provider::ChatGpt,
                    upstream_host: format!("{stub}:8080"),
                    tls: false,
                    credential_file: std::path::PathBuf::from("/broker/openai.token"),
                    kind: Some(crate::seat_credentials::CredentialKind::CodexChatgpt),
                    unit_header: None,
                }],
                egress_hosts: vec![],
                hook_upstream: None,
                mcp_upstream: None,
                chatgpt_accounts: Default::default(),
                copilot_hosts: Default::default(),
                legacy_chatgpt_account: None,
            },
        )
        .unwrap();
        let gw = ensure_gateway(&rt, &dir).expect("the gateway starts");
        let stub_dir = format!(
            "type=bind,src={},dst=/s,readonly",
            dir.join("stub").display()
        );
        let o = run(&[
            "run",
            "-d",
            "--name",
            &stub,
            "--network",
            &gw.external_network,
            "--mount",
            &stub_dir,
            &image,
            "node",
            "/s/stub.js",
        ]);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        let seat_net = format!("{}-seat", gw.name);
        run(&["network", "rm", &seat_net]);
        assert!(run(&["network", "create", "--internal", &seat_net])
            .status
            .success());
        attach(&rt, &gw, &seat_net).expect("attached as agw");
        let key = UnitKey::new("run-login", "run-login#1", "codex");
        register_live(&rt, &gw, &dir, key.clone()).unwrap();
        // What the user's login container leaves behind (FAKE values).
        let volume = login_volume(&gw.name, "codex");
        run(&["volume", "rm", &volume]);
        assert!(run(&["volume", "create", &volume]).status.success());
        checked(&rt, &init_login_argv(&volume, "codex")).unwrap();
        let auth = serde_json::to_string(&chatgpt()).unwrap();
        let write = helper_argv(
            &volume,
            false,
            "10001",
            format!("printf '%s' '{auth}' > /in/codex/auth.json"),
        );
        checked(&rt, &write).unwrap();
        let ask = |want: &dyn Fn(&str) -> bool| {
            let bearer = format!("authorization: Bearer {}", key.bearer);
            let mut last = String::new();
            for _ in 0..30 {
                let o = run(&[
                    "run",
                    "--rm",
                    "--network",
                    &seat_net,
                    &image,
                    "curl",
                    "-s",
                    "--max-time",
                    "4",
                    "-w",
                    "\n%{http_code}",
                    "-H",
                    &bearer,
                    "-H",
                    "chatgpt-account-id: acct-PLACEHOLDER",
                    "-H",
                    "content-type: application/json",
                    "-d",
                    "{}",
                    "http://agw:4002/backend-api/codex/responses",
                ]);
                last = String::from_utf8_lossy(&o.stdout).to_string();
                if want(&last) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            last
        };
        write_session(&dir, "codex", &volume).unwrap();
        let report = ingest_and_route(&rt, &gw, &dir, "codex").expect("ingested");
        let signed_in = ask(&|b| b.contains("FAKE-codex-access") && b.contains("FAKE-account-id"));
        let status_in = broker_status(&rt, &gw, "codex");
        let login_gone = !volume_exists(&rt, &volume).unwrap();
        // The broker's one refresher: a forced refresh against the stub token endpoint; the
        // gateway serves the new token with no restart (it watches its backendAuth file).
        *crate::seat_refresh::TEST_URL.lock().unwrap() = Some(format!("http://{stub}:8081/token"));
        let refreshed = crate::seat_refresh::refresh(&rt, &gw, &dir, "codex", true);
        let after_refresh = ask(&|b| b.contains("FAKE-codex-access-v2"));
        let not_due = crate::seat_refresh::refresh(&rt, &gw, &dir, "codex", false);
        // A refusing endpoint: the refresh fails, the status says so, the served token stays.
        *crate::seat_refresh::TEST_URL.lock().unwrap() = Some(format!("http://{stub}:8081/bad"));
        let refused = crate::seat_refresh::refresh(&rt, &gw, &dir, "codex", true);
        let status_failing = with_last_refresh(
            broker_status(&rt, &gw, "codex"),
            crate::seat_refresh::last_refresh(&dir, "codex"),
        );
        *crate::seat_refresh::TEST_URL.lock().unwrap() = None;
        let stub_logs = run(&["logs", &stub]);
        let token_requests = String::from_utf8_lossy(&stub_logs.stdout).to_string();
        sign_out_and_route(&rt, &gw, &dir, "codex").expect("signed out");
        let signed_out =
            ask(&|b| !b.contains("FAKE-codex-access") && !b.contains("FAKE-account-id"));
        let status_out = broker_status(&rt, &gw, "codex");
        let logs = run(&["logs", &gw.name]);
        let all_logs = format!(
            "{}{}",
            String::from_utf8_lossy(&logs.stdout),
            String::from_utf8_lossy(&logs.stderr)
        );
        run(&["rm", "-f", &stub, &gw.name]);
        run(&["network", "rm", &seat_net, &gw.external_network]);
        let (cfg, broker) = crate::seat_gateway::volumes(&gw);
        run(&["volume", "rm", &cfg, &broker, &volume]);
        let _ = std::fs::remove_dir_all(&dir);
        eprintln!("signed_in={signed_in:?}\nsigned_out={signed_out:?}\nreport={report:?}");
        assert!(
            signed_in.contains("\"auth\":\"Bearer FAKE-codex-access\""),
            "{signed_in}"
        );
        assert!(
            signed_in.contains("\"acct\":\"FAKE-account-id\""),
            "{signed_in}"
        );
        assert!(!signed_in.contains("PLACEHOLDER") && !signed_in.contains(&key.bearer));
        assert!(
            matches!(status_in, BrokerStatus::SignedIn { .. }),
            "{status_in:?}"
        );
        assert!(
            login_gone,
            "the throwaway login volume is removed after ingest"
        );
        assert!(
            !signed_out.contains("FAKE-codex-access") && !signed_out.contains("FAKE-account-id"),
            "{signed_out}"
        );
        assert_eq!(status_out, BrokerStatus::SignedOut);
        eprintln!("refreshed={refreshed:?}\nnot_due={not_due:?}\nrefused={refused:?}\nstatus_failing={status_failing:?}");
        assert!(
            matches!(
                refreshed,
                Ok(crate::seat_refresh::Outcome::Refreshed { .. })
            ),
            "{refreshed:?}"
        );
        assert!(
            after_refresh.contains("\"auth\":\"Bearer FAKE-codex-access-v2\""),
            "{after_refresh}"
        );
        assert!(
            after_refresh.contains("\"acct\":\"FAKE-account-id\""),
            "{after_refresh}"
        );
        assert!(
            token_requests.contains("\"refresh_token\":\"FAKE-codex-refresh\"")
                && token_requests.contains("app_EMoamEEZ73f0CkXaXp7hrann"),
            "{token_requests}"
        );
        assert!(
            matches!(not_due, Ok(crate::seat_refresh::Outcome::NotDue { .. })),
            "a fresh credential is not refreshed again: {not_due:?}"
        );
        assert!(refused.is_err(), "{refused:?}");
        assert!(
            matches!(status_failing, BrokerStatus::RefreshFailing { .. }),
            "{status_failing:?}"
        );
        assert!(!all_logs.contains("FAKE-codex-access-v2"));
        assert!(!all_logs.contains("FAKE-codex-access") && !all_logs.contains(&key.bearer));
    }

    /// LIVE (`WICKED_CONTAINER_LIVE=1`): pi's subscription sign-ins end to end with FAKE
    /// credentials (DES-SEAT-LOGIN-SUBSCRIPTIONS). A login volume holding pi's `auth.json` with
    /// Anthropic, ChatGPT and Copilot sign-ins is ingested; the real pi CLI, run in its seat image
    /// with only the placeholder files `seat_files` renders, reaches a stub upstream through each
    /// route, and the stub sees the broker's token (and the ChatGPT account), never the unit key
    /// or a placeholder. Another agent's unit key is refused on pi's routes; the Copilot token is
    /// renewed by the broker's exchange; sign-out empties every route.
    #[cfg(unix)]
    #[test]
    fn live_pi_subscriptions_reach_the_upstream_only_through_their_routes() {
        use crate::seat_credentials::{seat_files, Subscription};
        use crate::seat_gateway::{
            attach, configure_gateway, ensure_gateway, register_live, GatewayConfig, UnitKey,
        };
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        if std::env::var("WICKED_CONTAINER_LIVE").ok().as_deref() != Some("1") {
            eprintln!("skipped: set WICKED_CONTAINER_LIVE=1 with a container runtime to run");
            return;
        }
        let rt = crate::container_floor::runtime().expect("a container runtime answers");
        let image = crate::container_floor::ensure_seat_image(&rt, "pi").unwrap();
        let dir =
            std::env::temp_dir().join(format!("wicked-pi-live-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join("stub")).unwrap();
        std::fs::create_dir_all(dir.join("seat")).unwrap();
        let gw = Gateway::for_dir(&dir);
        let run = |argv: &[&str]| {
            let mut c = rt.cmd();
            c.args(argv);
            output_within(c, Duration::from_secs(90)).unwrap()
        };
        let stub = format!("{}-stub", gw.name);
        // 8080: the model upstream, logging what each request carried; 8081: the token endpoints.
        std::fs::write(
            dir.join("stub/stub.js"),
            "const h=require('http');h.createServer((q,r)=>{q.resume();q.on('end',()=>{\
             console.log('UPSTREAM '+JSON.stringify({url:q.url,auth:q.headers['authorization']||'',\
             acct:q.headers['chatgpt-account-id']||'',unit:q.headers['x-wicked-unit']||''}));\
             r.statusCode=401;r.end('{}')})}).listen(8080);\
             h.createServer((q,r)=>{q.resume();q.on('end',()=>{console.log('TOKEN '+q.method+' '+(q.headers['authorization']||''));\
             r.setHeader('content-type','application/json');\
             if(q.method==='GET')return r.end(JSON.stringify({token:'tid=FAKE-pi-copilot-v2;proxy-ep=proxy.individual.githubcopilot.com',\
             expires_at:Math.floor(Date.now()/1000)+1800}));\
             r.end(JSON.stringify({access_token:'FAKE-pi-renewed',expires_in:3600}))})}).listen(8081)",
        )
        .unwrap();
        let mut config = GatewayConfig::default();
        config.providers.retain(|r| r.owner() == Some("pi"));
        for route in &mut config.providers {
            route.upstream_host = format!("{stub}:8080");
            route.tls = false;
        }
        configure_gateway(&dir, &config).unwrap();
        let gw = ensure_gateway(&rt, &dir).expect("the gateway starts");
        let stub_dir = format!(
            "type=bind,src={},dst=/s,readonly",
            dir.join("stub").display()
        );
        let o = run(&[
            "run",
            "-d",
            "--name",
            &stub,
            "--network",
            &gw.external_network,
            "--mount",
            &stub_dir,
            &image,
            "node",
            "/s/stub.js",
        ]);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        let seat_net = format!("{}-seat", gw.name);
        run(&["network", "rm", &seat_net]);
        assert!(run(&["network", "create", "--internal", &seat_net])
            .status
            .success());
        attach(&rt, &gw, &seat_net).expect("attached as agw");
        let key = UnitKey::new("run-pi", "run-pi#1", "pi");
        let codex = UnitKey::new("run-pi", "run-pi#2", "codex");
        register_live(&rt, &gw, &dir, key.clone()).unwrap();
        register_live(&rt, &gw, &dir, codex.clone()).unwrap();
        // What pi's `/login` leaves in the login volume (FAKE values; Copilot due soonest).
        let mut login = pi_login();
        login["github-copilot"]["expires"] = json!(4_000_000_000_000_u64 - 1);
        login["github-copilot"]["access"] =
            json!("tid=FAKE-pi-copilot;proxy-ep=proxy.individual.githubcopilot.com");
        let volume = login_volume(&gw.name, "pi");
        run(&["volume", "rm", &volume]);
        assert!(run(&["volume", "create", &volume]).status.success());
        checked(&rt, &init_login_argv(&volume, "pi")).unwrap();
        let write = helper_argv(
            &volume,
            false,
            "10001",
            format!("printf '%s' '{login}' > /in/pi/auth.json"),
        );
        checked(&rt, &write).unwrap();
        write_session(&dir, "pi", &volume).unwrap();
        let report = ingest_and_route(&rt, &gw, &dir, "pi").expect("ingested");
        let sign_in = seat_sign_in(&rt, &gw, &dir, "pi").expect("pi is signed in");
        let upstream = || {
            let o = run(&["logs", &stub]);
            format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            )
        };
        // A raw request on each route, as pi's unit and as codex's unit.
        let curl = |port: u16, unit: &str| {
            let header = format!("x-wicked-unit: {unit}");
            let url = format!("http://agw:{port}/probe-{port}");
            let o = run(&[
                "run",
                "--rm",
                "--network",
                &seat_net,
                &image,
                "curl",
                "-s",
                "--max-time",
                "6",
                "-o",
                "/dev/null",
                "-w",
                "%{http_code}",
                "-H",
                &header,
                "-H",
                "authorization: Bearer PLACEHOLDER-not-a-token",
                "-d",
                "{}",
                &url,
            ]);
            String::from_utf8_lossy(&o.stdout).to_string()
        };
        let foreign: Vec<String> = [4006, 4007, 4008]
            .iter()
            .map(|p| curl(*p, &codex.key))
            .collect();
        let absent: Vec<String> = [4006, 4007, 4008]
            .iter()
            .map(|p| curl(*p, "wk-unknown"))
            .collect();
        // The real pi CLI with only its placeholder files, one model per subscription.
        // The same rendering arming does (`container_floor::placeholder_handoff`).
        let url = crate::seat_gateway::credential_gateway_url(
            "pi",
            crate::seat_credentials::CredentialKind::ProviderKey,
        );
        let files = seat_files("pi", &sign_in, &key.key, &url);
        for (name, body) in &files {
            std::fs::write(dir.join("seat").join(name), body).unwrap();
        }
        let seat_dir = format!(
            "type=bind,src={},dst=/p,readonly",
            dir.join("seat").display()
        );
        for model in [
            "anthropic/claude-sonnet-5",
            "openai-codex/gpt-5.5",
            "github-copilot/gpt-5.5",
        ] {
            let script = format!(
                "mkdir -p /tmp/pi && cp /p/* /tmp/pi/ && PI_CODING_AGENT_DIR=/tmp/pi HOME=/tmp \
                 timeout 60 pi -p --mode json --no-extensions --model {model} hi </dev/null 2>&1 | tail -c 600; true"
            );
            let o = run(&[
                "run",
                "--rm",
                "--network",
                &seat_net,
                "--user",
                "10001",
                "--read-only",
                "--tmpfs",
                "/tmp:rw,exec",
                "--mount",
                &seat_dir,
                &image,
                "sh",
                "-c",
                &script,
            ]);
            eprintln!(
                "pi {model}: {}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
        }
        // `docker logs` can trail the container's writes: wait for all three routes' lines.
        let mut seen = String::new();
        for _ in 0..40 {
            seen = upstream();
            if seen.contains("/v1/messages")
                && seen.contains("/codex/responses")
                && seen.contains("tid=")
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        *crate::seat_refresh::TEST_URL.lock().unwrap() = Some(format!("http://{stub}:8081/token"));
        let refreshed = crate::seat_refresh::refresh(&rt, &gw, &dir, "pi", true);
        *crate::seat_refresh::TEST_URL.lock().unwrap() = None;
        let after_refresh = {
            let mut last = String::new();
            for _ in 0..30 {
                curl(4008, &key.key);
                last = upstream();
                if last.contains("FAKE-pi-copilot-v2") {
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            last
        };
        let status_in = broker_status(&rt, &gw, "pi");
        sign_out_and_route(&rt, &gw, &dir, "pi").expect("signed out");
        let before_out = upstream().len();
        for port in [4006, 4007, 4008] {
            curl(port, &key.key);
        }
        let after_out = upstream()[before_out..].to_string();
        let status_out = broker_status(&rt, &gw, "pi");
        let logs = run(&["logs", &gw.name]);
        let gw_logs = format!(
            "{}{}",
            String::from_utf8_lossy(&logs.stdout),
            String::from_utf8_lossy(&logs.stderr)
        );
        run(&["rm", "-f", &stub, &gw.name]);
        run(&["network", "rm", &seat_net, &gw.external_network]);
        let (cfg, broker) = crate::seat_gateway::volumes(&gw);
        run(&["volume", "rm", &cfg, &broker, &volume]);
        let _ = std::fs::remove_dir_all(&dir);
        for line in gw_logs.lines().filter(|l| {
            !l.contains("DirectResponse")
                && (l.contains("\"scope\":\"request\"")
                    || l.contains("error")
                    || l.contains("warn"))
        }) {
            eprintln!("GW {}", &line[..line.len().min(700)]);
        }
        eprintln!("report={report:?}\nsign_in={sign_in:?}\nforeign={foreign:?} absent={absent:?}\nseen={seen}\nrefreshed={refreshed:?}");
        assert_eq!(report.skipped, vec!["xai (oauth)".to_string()]);
        assert_eq!(
            sign_in.subscriptions,
            vec![
                Subscription::PiAnthropic,
                Subscription::PiChatgpt,
                Subscription::PiCopilot
            ]
        );
        assert!(
            foreign.iter().all(|c| c == "401"),
            "codex's unit on pi's routes: {foreign:?}"
        );
        // The refusals were the gateway's: no foreign or unknown probe reached the upstream (the
        // stub answers 401 too). `seen` predates every probe pi's own unit makes.
        assert!(
            !seen.contains("/probe-"),
            "a refused probe reached the upstream: {seen}"
        );
        assert!(absent.iter().all(|c| c == "401"), "{absent:?}");
        let upstream_lines: Vec<Value> = seen
            .lines()
            .filter_map(|l| l.strip_prefix("UPSTREAM "))
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let by = |path: &str| {
            upstream_lines
                .iter()
                .find(|v| v["url"].as_str().unwrap().contains(path))
        };
        let anthropic = by("/v1/messages").expect("pi reached the anthropic route");
        assert_eq!(anthropic["auth"], "Bearer sk-ant-oat01-FAKE-access");
        let chatgpt = by("/codex/responses").expect("pi reached the ChatGPT route");
        assert_eq!(
            chatgpt["auth"],
            format!("Bearer {}", pi_jwt("acct-FAKE-0002"))
        );
        assert_eq!(chatgpt["acct"], "acct-FAKE-0002");
        let copilot = upstream_lines.iter().find(|v| v["url"] == "/responses");
        assert_eq!(
            copilot.map(|v| v["auth"].clone()),
            Some(json!(
                "Bearer tid=FAKE-pi-copilot;proxy-ep=proxy.individual.githubcopilot.com"
            )),
            "{seen}"
        );
        for line in &upstream_lines {
            assert_eq!(line["unit"], "", "the unit header never leaves the gateway");
            assert!(
                !line.to_string().contains(&key.key) && !line.to_string().contains("PLACEHOLDER")
            );
        }
        assert!(
            matches!(
                refreshed,
                Ok(crate::seat_refresh::Outcome::Refreshed { .. })
            ),
            "{refreshed:?}"
        );
        assert!(
            seen.contains("TOKEN GET Bearer FAKE-gho-token")
                || after_refresh.contains("TOKEN GET Bearer FAKE-gho-token")
        );
        assert!(
            after_refresh.contains("Bearer tid=FAKE-pi-copilot-v2"),
            "{after_refresh}"
        );
        assert!(
            matches!(status_in, BrokerStatus::SignedIn { .. }),
            "{status_in:?}"
        );
        assert!(!after_out.contains("FAKE"), "{after_out}");
        assert_eq!(status_out, BrokerStatus::SignedOut);
        // Every synthetic credential, original and renewed, stays out of the gateway's log and
        // out of what ingest reports.
        let chatgpt_jwt = pi_jwt("acct-FAKE-0002");
        let report_text = format!("{report:?}");
        for secret in [
            "sk-ant-oat01-FAKE-access",
            chatgpt_jwt.as_str(),
            "FAKE-ant-refresh",
            "FAKE-oai-refresh",
            "FAKE-gho-token",
            "FAKE-pi-copilot",
            "FAKE-pi-key",
            "FAKE-pi-renewed",
            &key.key,
        ] {
            assert!(!gw_logs.contains(secret), "{secret} in the gateway log");
            assert!(
                !report_text.contains(secret),
                "{secret} in the ingest report"
            );
        }
    }

    /// LIVE (`WICKED_CONTAINER_LIVE=1`): opencode's sign-ins end to end with FAKE credentials
    /// (DES-SEAT-LOGIN-SUBSCRIPTIONS). Its login export (ChatGPT, Copilot, an Anthropic key) is
    /// ingested; the real opencode CLI, launched through the engine's pinned-server script with
    /// only the placeholder files `seat_files` renders (the script imports the constant credential
    /// rows), reaches a stub upstream through each route, which sees the broker's credential and
    /// never the unit key, a placeholder or `x-wicked-unit`. pi's unit is refused on opencode's
    /// routes; sign-out empties them.
    #[cfg(unix)]
    #[test]
    fn live_opencode_sign_ins_reach_the_upstream_only_through_their_routes() {
        use crate::seat_credentials::seat_files;
        use crate::seat_gateway::{
            attach, configure_gateway, ensure_gateway, register_live, GatewayConfig, UnitKey,
        };
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        if std::env::var("WICKED_CONTAINER_LIVE").ok().as_deref() != Some("1") {
            eprintln!("skipped: set WICKED_CONTAINER_LIVE=1 with a container runtime to run");
            return;
        }
        let rt = crate::container_floor::runtime().expect("a container runtime answers");
        let image = crate::container_floor::ensure_seat_image(&rt, "opencode").unwrap();
        let dir =
            std::env::temp_dir().join(format!("wicked-oc-live-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join("stub")).unwrap();
        std::fs::create_dir_all(dir.join("seat")).unwrap();
        let gw = Gateway::for_dir(&dir);
        let run = |argv: &[&str]| {
            let mut c = rt.cmd();
            c.args(argv);
            output_within(c, Duration::from_secs(120)).unwrap()
        };
        let stub = format!("{}-stub", gw.name);
        std::fs::write(
            dir.join("stub/stub.js"),
            "const h=require('http');h.createServer((q,r)=>{q.resume();q.on('end',()=>{\
             console.log('UPSTREAM '+JSON.stringify({url:q.url,auth:q.headers['authorization']||'',\
             key:q.headers['x-api-key']||'',acct:q.headers['chatgpt-account-id']||'',\
             unit:q.headers['x-wicked-unit']||''}));r.statusCode=401;r.end('{}')})}).listen(8080);",
        )
        .unwrap();
        let mut config = GatewayConfig::default();
        config.providers.retain(|r| r.owner() == Some("opencode"));
        for route in &mut config.providers {
            route.upstream_host = format!("{stub}:8080");
            route.tls = false;
        }
        configure_gateway(&dir, &config).unwrap();
        let gw = ensure_gateway(&rt, &dir).expect("the gateway starts");
        let stub_dir = format!(
            "type=bind,src={},dst=/s,readonly",
            dir.join("stub").display()
        );
        let o = run(&[
            "run",
            "-d",
            "--name",
            &stub,
            "--network",
            &gw.external_network,
            "--mount",
            &stub_dir,
            &image,
            "sh",
            "-c",
            "exec node /s/stub.js",
        ]);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        let seat_net = format!("{}-seat", gw.name);
        run(&["network", "rm", &seat_net]);
        assert!(run(&["network", "create", "--internal", &seat_net])
            .status
            .success());
        attach(&rt, &gw, &seat_net).expect("attached as agw");
        let key = UnitKey::new("run-oc", "run-oc#1", "opencode");
        let pi = UnitKey::new("run-oc", "run-oc#2", "pi");
        register_live(&rt, &gw, &dir, key.clone()).unwrap();
        register_live(&rt, &gw, &dir, pi.clone()).unwrap();
        let login = opencode_login();
        let volume = login_volume(&gw.name, "opencode");
        run(&["volume", "rm", &volume]);
        assert!(run(&["volume", "create", &volume]).status.success());
        checked(&rt, &init_login_argv(&volume, "opencode")).unwrap();
        let write = helper_argv(
            &volume,
            false,
            "10001",
            format!("mkdir -p /in/data/opencode && printf '%s' '{login}' > /in/data/opencode/wicked-auth.json"),
        );
        checked(&rt, &write).unwrap();
        write_session(&dir, "opencode", &volume).unwrap();
        let report = ingest_and_route(&rt, &gw, &dir, "opencode").expect("ingested");
        let sign_in = seat_sign_in(&rt, &gw, &dir, "opencode").expect("opencode is signed in");
        let upstream = || {
            let o = run(&["logs", &stub]);
            format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            )
        };
        let curl = |port: u16, unit: &str| {
            let header = format!("x-wicked-unit: {unit}");
            let url = format!("http://agw:{port}/probe-{port}");
            let o = run(&[
                "run",
                "--rm",
                "--network",
                &seat_net,
                &image,
                "curl",
                "-s",
                "--max-time",
                "6",
                "-o",
                "/dev/null",
                "-w",
                "%{http_code}",
                "-H",
                &header,
                "-d",
                "{}",
                &url,
            ]);
            String::from_utf8_lossy(&o.stdout).to_string()
        };
        let foreign: Vec<String> = [4009, 4010, 4011]
            .iter()
            .map(|p| curl(*p, &pi.key))
            .collect();
        // The unit's files, as arming renders them, in the per-unit dir OPENCODE_CONFIG names.
        let url = crate::seat_gateway::credential_gateway_url(
            "opencode",
            crate::seat_credentials::CredentialKind::ProviderKey,
        );
        for (name, body) in seat_files("opencode", &sign_in, &key.key, &url) {
            std::fs::write(dir.join("seat").join(name), body).unwrap();
        }
        let env = crate::seat_gateway::seat_env_for_sign_in("opencode", &sign_in, &key.key);
        let logout = env
            .iter()
            .find(|(k, _)| k == crate::seat_gateway::OPENCODE_LOGOUT_ENV)
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        let seat_dir = format!(
            "type=bind,src={},dst=/u,readonly",
            dir.join("seat").display()
        );
        for model in [
            "openai/gpt-5.5",
            "github-copilot/gpt-5.5",
            "anthropic/claude-sonnet-5",
        ] {
            let argv: Vec<String> = ["opencode", "run", "--format", "json", "-m", model, "hi"]
                .map(String::from)
                .to_vec();
            let pinned = crate::worker_sandbox::opencode_pinned_server_argv(&argv, 4567).unwrap();
            let mut docker: Vec<String> = [
                "run",
                "--rm",
                "--network",
                &seat_net,
                "--user",
                "10001",
                "--read-only",
                "--tmpfs",
                "/tmp:rw,exec",
                "-w",
                "/tmp",
                "-e",
                "TMPDIR=/tmp",
                "--mount",
                &seat_dir,
                "-e",
                "HOME=/tmp",
                "-e",
                "XDG_CONFIG_HOME=/tmp/config",
                "-e",
                "XDG_DATA_HOME=/tmp/data",
                "-e",
                "OPENCODE_CONFIG=/u/opencode.json",
                "-e",
                "OPENCODE_SERVER_PASSWORD=nonce-FAKE",
                "-e",
                "OPENCODE_PASSWORD=nonce-FAKE",
                "-e",
                "OPENCODE_DISABLE_AUTOUPDATE=1",
            ]
            .map(String::from)
            .to_vec();
            docker.extend(["-e".into(), format!("WICKED_OPENCODE_LOGOUT={logout}")]);
            docker.push(image.clone());
            docker.extend(["timeout".into(), "90".into()]);
            docker.extend(pinned);
            let refs: Vec<&str> = docker.iter().map(String::as_str).collect();
            let o = run(&refs);
            eprintln!(
                "opencode {model}: {}",
                String::from_utf8_lossy(&o.stderr)
                    .chars()
                    .rev()
                    .take(400)
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect::<String>()
            );
        }
        let mut seen = String::new();
        for _ in 0..40 {
            seen = upstream();
            if seen.contains("/backend-api/codex/responses")
                && seen.contains("FAKE-gho-oc")
                && seen.contains("/v1/messages")
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        let status_in = broker_status(&rt, &gw, "opencode");
        sign_out_and_route(&rt, &gw, &dir, "opencode").expect("signed out");
        let before_out = upstream().len();
        for port in [4009, 4010, 4011] {
            curl(port, &key.key);
        }
        std::thread::sleep(Duration::from_secs(1));
        let after_out = upstream()[before_out..].to_string();
        let status_out = broker_status(&rt, &gw, "opencode");
        let logs = run(&["logs", &gw.name]);
        let gw_logs = format!(
            "{}{}",
            String::from_utf8_lossy(&logs.stdout),
            String::from_utf8_lossy(&logs.stderr)
        );
        run(&["rm", "-f", &stub, &gw.name]);
        run(&["network", "rm", &seat_net, &gw.external_network]);
        let (cfg, broker) = crate::seat_gateway::volumes(&gw);
        run(&["volume", "rm", &cfg, &broker, &volume]);
        let _ = std::fs::remove_dir_all(&dir);
        eprintln!("report={report:?}\nsign_in={sign_in:?}\nforeign={foreign:?}\nseen={seen}");
        assert!(
            foreign.iter().all(|c| c == "401"),
            "pi's unit on opencode's routes: {foreign:?}"
        );
        assert!(
            !seen.contains("/probe-"),
            "a refused probe reached the upstream: {seen}"
        );
        let lines: Vec<Value> = seen
            .lines()
            .filter_map(|l| l.strip_prefix("UPSTREAM "))
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let find = |pred: &dyn Fn(&Value) -> bool| lines.iter().find(|v| pred(v)).cloned();
        let chatgpt = find(&|v| v["url"] == "/backend-api/codex/responses" && v["auth"] != "")
            .expect("opencode reached the ChatGPT route");
        assert_eq!(chatgpt["auth"], "Bearer FAKE-oc-access");
        assert_eq!(chatgpt["acct"], "acct-FAKE-oc");
        let copilot = find(&|v| v["auth"] == "Bearer FAKE-gho-oc");
        assert!(
            copilot.is_some(),
            "opencode reached the Copilot route: {seen}"
        );
        let anthropic = find(&|v| v["url"].as_str().unwrap_or("").starts_with("/v1/messages"))
            .expect("opencode reached the Anthropic route");
        assert_eq!(anthropic["key"], "FAKE-oc-ant");
        for line in &lines {
            assert_eq!(line["unit"], "", "the unit header never leaves the gateway");
            let text = line.to_string();
            assert!(
                !text.contains(&key.key) && !text.contains("PLACEHOLDER"),
                "{text}"
            );
        }
        assert!(
            matches!(status_in, BrokerStatus::SignedIn { .. }),
            "{status_in:?}"
        );
        assert!(!after_out.contains("FAKE"), "{after_out}");
        assert_eq!(status_out, BrokerStatus::SignedOut);
        let report_text = format!("{report:?}");
        for secret in [
            "FAKE-oc-access",
            "FAKE-oc-r",
            "FAKE-gho-oc",
            "FAKE-oc-ant",
            &key.key,
        ] {
            assert!(!gw_logs.contains(secret), "{secret} in the gateway log");
            assert!(
                !report_text.contains(secret),
                "{secret} in the ingest report"
            );
        }
    }

    /// The FAKE claude the C6 live test builds into a seat image: no account, no network auth.
    /// `claude auth login` shows what the real subscription login shows in a terminal with no
    /// browser (a link, then a code to paste) and writes a FAKE OAuth record; a headless turn sends
    /// one model request with whatever credential its seat holds and prints what came back.
    #[cfg(unix)]
    const FAKE_CLAUDE: &str = r#"#!/bin/sh
cfg="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
if [ "$1 $2" = "auth login" ]; then
  echo "Browser didn't open? Use the url below to sign in:"
  echo "https://claude.example.invalid/oauth/authorize?code=true"
  printf 'Paste code here if prompted > '
  read code
  [ "$code" = "FAKE-CODE-1234" ] || { echo "Invalid code"; exit 1; }
  mkdir -p "$cfg" && umask 077
  printf '%s' '{"claudeAiOauth":{"accessToken":"sk-ant-oat01-FAKE-c6-access","refreshToken":"sk-ant-ort01-FAKE-c6-refresh","expiresAt":4102444800000,"scopes":["user:inference"]}}' > "$cfg/.credentials.json"
  echo "Login successful."
  exit 0
fi
tok=$(jq -r '.claudeAiOauth.accessToken // empty' "$cfg/.credentials.json" 2>/dev/null)
curl -s --max-time 8 -H "authorization: Bearer $tok" -H 'content-type: application/json' \
  -d '{"model":"x","max_tokens":1,"messages":[]}' "$ANTHROPIC_BASE_URL/v1/messages" || echo CURL_FAILED
"#;

    /// Run a login argv in the engine's own PTY (`terminal::spawn_pty`, the one behind crew's
    /// `/terminals`) and, at the CLI's code prompt, type `answer` — or, with `None`, kill the
    /// terminal there, as crew does when its socket drops. Returns the transcript and exit code.
    #[cfg(unix)]
    fn login_terminal(argv: &[String], answer: Option<&str>) -> (String, Option<i32>) {
        let map = crate::terminal::new_map();
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = crate::terminal::spawn_pty(
            "c6-login",
            &std::env::temp_dir(),
            Some(argv.to_vec()),
            100,
            30,
            &map,
            tx,
        )
        .expect("the login terminal opens");
        let mut transcript = String::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(90);
        let mut typed = false;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(left) {
                Ok(crate::command::Command::TerminalChunk { bytes, .. }) => {
                    spawned
                        .in_flight
                        .fetch_sub(bytes.len(), std::sync::atomic::Ordering::AcqRel);
                    transcript.push_str(&String::from_utf8_lossy(&bytes));
                    if !typed && transcript.contains("Paste code") {
                        typed = true;
                        let Some(answer) = answer else { break };
                        use std::io::Write;
                        let terminals = crate::terminal::lock(&map);
                        let mut w = terminals["c6-login"].writer.lock().unwrap();
                        w.write_all(format!("{answer}\r").as_bytes()).unwrap();
                    }
                }
                Ok(crate::command::Command::TerminalReaderDone { .. }) => break,
                Ok(_) => {}
                Err(e) => panic!("the login terminal did not finish ({e}): {transcript}"),
            }
        }
        let mut session = crate::terminal::lock(&map).remove("c6-login").unwrap();
        let exit = crate::terminal::reap_session(&mut session, answer.is_none());
        (transcript, exit)
    }

    /// LIVE (`WICKED_CONTAINER_LIVE=1` + a runtime; skipped otherwise) — C6, the sign-in journey
    /// end to end with a FAKE claude (no account, no real credential anywhere):
    /// 1. before any sign-in, the agent's governed seat is refused (`agent_signed_out`);
    /// 2. a sign-in abandoned mid-login (the terminal killed, as a closed tab does) leaves no
    ///    container behind once ingested and does not block the next one; then
    ///    the login argv crew runs for Agents → Sign in, run in the engine's own PTY (the one
    ///    behind crew's `/terminals`): the user sees the link, types the code, the CLI writes its
    ///    credential into the throwaway login volume;
    /// 3. ingest moves it into the broker and removes the login volume;
    /// 4. a governed seat of that agent, armed by the container floor, holds only a placeholder,
    ///    yet its model request reaches the (stub) upstream carrying the user's credential, through
    ///    the route of the kind the user signed in with;
    /// 5. after sign-out the seat is refused again.
    #[cfg(unix)]
    #[test]
    fn live_a_login_container_sign_in_carries_to_the_agents_governed_seat() {
        use crate::container_floor::{arm, build_image, ensure_seat_image, finalize};
        use crate::seat_gateway::{configure_gateway, ensure_gateway, GatewayConfig};
        use crate::worker_sandbox::{FloorInputs, FloorUnarmed};
        use base64::engine::general_purpose::STANDARD;
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        if std::env::var("WICKED_CONTAINER_LIVE").ok().as_deref() != Some("1") {
            eprintln!("skipped: set WICKED_CONTAINER_LIVE=1 with a container runtime to run");
            return;
        }
        const IMAGE_ENV: &str = "WICKED_SEAT_IMAGE_CLAUDE";
        // Restored however the test ends (agy c6-r2 P3): later tests in this binary read them.
        struct Restore(Vec<(&'static str, Option<std::ffi::OsString>)>);
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
            [IMAGE_ENV, "WICKED_WORKER_HOME"]
                .into_iter()
                .map(|k| (k, std::env::var_os(k)))
                .collect(),
        );
        std::env::remove_var(IMAGE_ENV);
        let rt = crate::container_floor::runtime().expect("a container runtime answers");
        let pinned = ensure_seat_image(&rt, "claude").expect("the claude seat image");
        let fake = format!("wicked-c6-fake-claude:{}", uuid::Uuid::new_v4().simple());
        build_image(
            &rt,
            &format!(
                "FROM {pinned}\nRUN rm -f /usr/local/bin/claude && echo {} | base64 -d > \
                 /usr/local/bin/claude && chmod 755 /usr/local/bin/claude\n",
                STANDARD.encode(FAKE_CLAUDE)
            ),
            &fake,
            &[],
        )
        .expect("the fake claude image");
        let base = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("wicked-c6-live-{}", uuid::Uuid::new_v4().simple()));
        let dir = base.join("state/gateway");
        let worker = base.join("worker");
        let seat_home = worker.join("seat-claude");
        for d in [&dir, &seat_home, &base.join("stub")] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::env::set_var(IMAGE_ENV, &fake);
        std::env::set_var("WICKED_WORKER_HOME", &worker);
        let run = |argv: &[&str]| {
            let mut c = rt.cmd();
            c.args(argv);
            output_within(c, Duration::from_secs(60)).unwrap()
        };
        let gw = Gateway::for_dir(&dir);
        let stub = format!("{}-stub", gw.name);
        // The stub upstream answers with what authenticated the request.
        std::fs::write(
            base.join("stub/stub.js"),
            "require('http').createServer((q,r)=>{q.resume();q.on('end',()=>{\
             r.setHeader('content-type','application/json');\
             r.end(JSON.stringify({path:q.url,auth:q.headers['authorization']||'',\
             key:q.headers['x-api-key']||''}))})}).listen(8080)",
        )
        .unwrap();
        let mut config = GatewayConfig::default();
        for route in &mut config.providers {
            route.upstream_host = format!("{stub}:8080");
            route.tls = false;
        }
        configure_gateway(&dir, &config).unwrap();
        let gw = ensure_gateway(&rt, &dir).expect("the gateway starts");
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
            &pinned,
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
        let argv: Vec<String> = ["claude", "-p", "hello"].map(String::from).to_vec();
        let seat = |ord: u32| {
            arm(
                &inputs,
                &argv,
                "run-c6",
                ord,
                &[],
                Some("hello"),
                Some(&dir),
            )
        };
        // 1. Signed out: refused before any seat container starts.
        let before = seat(1).map(|_| ()).unwrap_err();
        let running = |name: &str| {
            run(&[
                "container",
                "inspect",
                "--format",
                "{{.State.Running}}",
                name,
            ])
            .stdout
            .starts_with(b"true")
        };
        // 2a. A sign-in whose tab closed mid-login: crew kills the terminal's client, which leaves
        // `docker run -it` running on the login volume. Ingest (crew runs it on the terminal's
        // exit) stops it; nothing was signed in; the next sign-in opens normally.
        let abandoned = prepare_login_argv(&rt, &gw, &dir, "claude", &fake).expect("login argv");
        let abandoned_volume = read_session(&dir, &gw, "claude").unwrap().unwrap();
        let _ = login_terminal(&abandoned, None);
        let lingered = running(&abandoned_volume);
        let abandoned_ingest = ingest_and_route(&rt, &gw, &dir, "claude").map(|r| r.files);
        let stopped = !running(&abandoned_volume);
        // 2a'. One an earlier engine left (named `wicked-login-<cli>-<hex>`, not after its session).
        prepare_login_argv(&rt, &gw, &dir, "claude", &fake).expect("argv");
        let legacy_volume = read_session(&dir, &gw, "claude").unwrap().unwrap();
        let legacy = format!("wicked-login-claude-{}", uuid::Uuid::new_v4().simple());
        let mount = format!("{legacy_volume}:/home/login");
        let o = run(&[
            "run", "-d", "--name", &legacy, "-v", &mount, &fake, "sleep", "300",
        ]);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        let legacy_ingest = ingest_and_route(&rt, &gw, &dir, "claude").map(|r| r.files);
        let legacy_stopped = !running(&legacy);
        // 2b. Agents -> Sign in: the login argv crew runs, in the engine's PTY.
        let login = prepare_login_argv(&rt, &gw, &dir, "claude", &fake).expect("the login argv");
        let volume = read_session(&dir, &gw, "claude")
            .unwrap()
            .expect("a login session");
        let abandoned_gone = !volume_exists(&rt, &abandoned_volume).unwrap()
            && !volume_exists(&rt, &legacy_volume).unwrap();
        let (transcript, exit) = login_terminal(&login, Some("FAKE-CODE-1234"));
        // 3. Crew's ingest on the terminal's exit.
        let report = ingest_and_route(&rt, &gw, &dir, "claude").expect("ingested");
        let login_gone = !volume_exists(&rt, &volume).unwrap();
        // 4. The agent's governed seat.
        let armed = seat(2).expect("a signed-in agent's seat arms");
        let base_url = armed
            .env
            .iter()
            .find(|(k, _)| k == "ANTHROPIC_BASE_URL")
            .map(|(_, v)| v.clone());
        // The command execute_wrapped builds: the floor's wrapper, then the seat's argv.
        let full: Vec<String> = armed.wrapper.iter().chain(&armed.argv).cloned().collect();
        let mut cmd = crate::execute_wrapped::build_worker_command(&full, None);
        cmd.env("CLAUDE_CONFIG_DIR", &seat_home);
        for (k, v) in &armed.env {
            cmd.env(k, v);
        }
        crate::container_floor::write_unit_seat_files(&mut cmd, "claude", &armed.files)
            .expect("placeholder files");
        let placeholder =
            std::fs::read_to_string(seat_home.join(".credentials.json")).unwrap_or_default();
        let out = output_within(finalize(cmd), Duration::from_secs(90)).expect("the seat ran");
        let seat_out = String::from_utf8_lossy(&out.stdout).to_string();
        let seat_err = String::from_utf8_lossy(&out.stderr).to_string();
        if let Some(lease) = armed.lease {
            let _ = lease.release();
        }
        drop(armed.cleanup);
        // 5. Sign out: refused again.
        sign_out_and_route(&rt, &gw, &dir, "claude").expect("signed out");
        let after = seat(3).map(|_| ()).unwrap_err();
        let logs = run(&["logs", &gw.name]);
        let gw_logs = format!(
            "{}{}",
            String::from_utf8_lossy(&logs.stdout),
            String::from_utf8_lossy(&logs.stderr)
        );
        run(&["rm", "-f", &stub, &gw.name]);
        run(&["network", "rm", &gw.external_network]);
        let (cfg, broker) = crate::seat_gateway::volumes(&gw);
        run(&["volume", "rm", &cfg, &broker, &volume]);
        run(&["rm", "-f", &abandoned_volume, &volume, &legacy]);
        run(&["volume", "rm", &abandoned_volume, &legacy_volume]);
        run(&["image", "rm", &fake]);
        let _ = std::fs::remove_dir_all(&base);
        eprintln!(
            "before={before:?}\ntranscript={transcript}\nexit={exit:?}\nreport={report:?}\n\
             base_url={base_url:?}\nseat_out={seat_out}\nseat_err={seat_err}\nafter={after:?}"
        );
        assert!(
            lingered,
            "a killed terminal client leaves its login container running"
        );
        assert!(
            abandoned_ingest.is_err(),
            "nothing was signed in: {abandoned_ingest:?}"
        );
        assert!(stopped, "ingest stops the abandoned login container");
        assert!(legacy_ingest.is_err(), "{legacy_ingest:?}");
        assert!(
            legacy_stopped,
            "an earlier engine's login container is stopped too"
        );
        assert!(
            abandoned_gone,
            "the next sign-in removes the abandoned session's volume"
        );
        assert_eq!(before.0, FloorUnarmed::AgentSignedOut, "{before:?}");
        assert!(before.1.contains("claude is not signed in"), "{before:?}");
        assert!(
            transcript.contains("https://claude.example.invalid/oauth/authorize")
                && transcript.contains("Login successful."),
            "the user sees the link and the result: {transcript}"
        );
        assert_eq!(exit, Some(0), "{transcript}");
        assert!(
            login_gone,
            "the throwaway login volume is removed after ingest"
        );
        assert_eq!(
            report.files,
            vec![
                "anthropic.token".to_string(),
                "claude.credentials.json".into()
            ]
        );
        // The OAuth sign-in's route, and only a placeholder in the seat.
        assert_eq!(base_url.as_deref(), Some("http://agw:4005"));
        assert!(placeholder.contains("claudeAiOauth"), "{placeholder}");
        assert!(!placeholder.contains("FAKE-c6-access"), "{placeholder}");
        assert!(out.status.success(), "{seat_out}\n{seat_err}");
        assert!(
            seat_out.contains("\"auth\":\"Bearer sk-ant-oat01-FAKE-c6-access\""),
            "the upstream receives the user's credential: {seat_out}\n{seat_err}"
        );
        assert!(seat_out.contains("\"path\":\"/v1/messages\""), "{seat_out}");
        assert_eq!(after.0, FloorUnarmed::AgentSignedOut, "{after:?}");
        assert!(
            !gw_logs.contains("FAKE-c6-access"),
            "no credential in the gateway log"
        );
    }
}
