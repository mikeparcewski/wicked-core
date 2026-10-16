//! The broker's ONE refresher (DES-CONTAINER-FLOOR §6, item 2): the only place a signed-in
//! agent's OAuth credential is refreshed. Seats never refresh (their placeholder expiry is far in
//! the future), so parallel seats never race a refresh.
//!
//! **Glue.** The request is built from the broker's retained OAuth record, POSTed to the
//! provider's token endpoint by a short-lived pinned curl container on the gateway's external
//! bridge (the body on stdin, the answer on stdout; never argv), and the answer is folded back
//! into the record, which then goes through the same extraction as a sign-in
//! ([`crate::seat_login::extract_tokens`]). agentgateway watches its `backendAuth` file, so the
//! new token is served without a restart. The record is written before the token file: a
//! provider that rotates its refresh token must never leave the broker holding only the old one.
//!
//! **One per gateway.** A pass holds an exclusive lock file in the daemon's gateway directory
//! (a second pass skips); the daemon starts one background loop per gateway dir.
//!
//! The endpoints and client ids are the ones the pinned CLIs themselves use (read from the
//! seat images' binaries: claude 2.1.296, codex 0.162.1). A real provider refresh needs a real
//! sign-in, which this engine never performs on anyone's behalf: the flow is proved against a
//! stub endpoint.

// Native Windows has no container floor: only the refresher's pure half compiles there.
#![cfg_attr(not(unix), allow(dead_code))]

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Refresh this long before the credential expires.
pub(crate) const REFRESH_MARGIN_SECS: u64 = 15 * 60;
/// How often the daemon's loop looks.
#[cfg(unix)]
const PASS_EVERY: std::time::Duration = std::time::Duration::from_secs(5 * 60);
/// The pinned OSS image the token request runs in (curl verifies TLS against its CA bundle).
pub(crate) const REFRESH_IMAGE: &str = "curlimages/curl:8.11.1";
/// The agents whose sign-in is an OAuth record the broker can refresh.
pub(crate) const REFRESHABLE: &[&str] = &["claude", "codex", "pi"];

/// One provider's refresh flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Flow {
    pub(crate) url: String,
    pub(crate) client_id: &'static str,
}

pub(crate) fn flow(cli: &str) -> Option<Flow> {
    let (url, client_id) = match cli {
        "claude" => (
            "https://platform.claude.com/v1/oauth/token",
            "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
        ),
        "codex" => (
            "https://auth.openai.com/oauth/token",
            "app_EMoamEEZ73f0CkXaXp7hrann",
        ),
        _ => return None,
    };
    #[cfg(test)]
    if let Some(stub) = TEST_URL.lock().ok().and_then(|u| u.clone()) {
        return Some(Flow {
            url: stub,
            client_id,
        });
    }
    Some(Flow {
        url: url.into(),
        client_id,
    })
}

/// The stub endpoint under test; `None` in production (a refresh token only goes to its provider).
fn test_url() -> Option<String> {
    #[cfg(test)]
    if let Some(stub) = TEST_URL.lock().ok().and_then(|u| u.clone()) {
        return Some(stub);
    }
    None
}

/// (tests) The stub token endpoint the live test points every flow at. No production override:
/// a refresh token only ever goes to the provider.
#[cfg(test)]
pub(crate) static TEST_URL: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// The broker file that retains `cli`'s whole OAuth record.
pub(crate) fn record_file(cli: &str) -> Option<&'static str> {
    match cli {
        "claude" => Some("claude.credentials.json"),
        "codex" => Some("codex.auth.json"),
        "pi" => Some("pi.auth.json"),
        _ => None,
    }
}

/// The login-volume spelling of the record, for [`crate::seat_login::extract_tokens`].
fn login_path(cli: &str) -> Option<&'static str> {
    match cli {
        "claude" => Some("claude/.credentials.json"),
        "codex" => Some("codex/auth.json"),
        "pi" => Some("pi/auth.json"),
        _ => None,
    }
}

/// One token request the refresher makes for a record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Request {
    /// An OAuth refresh: a JSON POST to the provider's token endpoint.
    Post { url: String, body: Value },
    /// Copilot's token exchange: a GET authorized by the GitHub token (sent on stdin as a
    /// header, never on a command line), with the client's identifying headers.
    BearerGet {
        url: String,
        token: String,
        headers: Vec<(&'static str, &'static str)>,
    },
}

/// GitHub's Copilot token exchange (the endpoint pi's own refresh calls).
const COPILOT_EXCHANGE_URL: &str = "https://api.github.com/copilot_internal/v2/token";
/// The identifying headers pi sends with the exchange (`github-copilot.js` COPILOT_HEADERS).
const PI_COPILOT_HEADERS: &[(&str, &str)] = &[
    ("accept", "application/json"),
    ("user-agent", "GitHubCopilotChat/0.35.0"),
    ("editor-version", "vscode/1.107.0"),
    ("editor-plugin-version", "copilot-chat/0.35.0"),
    ("copilot-integration-id", "vscode-chat"),
];

/// pi's refreshable entries, in no particular order: provider id → the flow its tokens renew by.
const PI_ENTRIES: &[&str] = &["anthropic", "openai-codex", "github-copilot"];

/// The next refresh of `record` at `now`: the entry it renews (pi: a provider id; `None` for
/// the single-record agents) and its request. pi renews the entry expiring first, one entry per
/// committed generation (DES-SEAT-LOGIN-SUBSCRIPTIONS decision 8); `None` = nothing refreshable
/// (or, unless `force`, nothing due).
pub(crate) fn next_refresh(
    cli: &str,
    record: &Value,
    now: u64,
    force: bool,
) -> Option<(Option<String>, Request)> {
    if cli != "pi" {
        let flow = flow(cli)?;
        let body = request_body(cli, record, &flow)?;
        let expires_at = crate::seat_login::record_expiry(cli, record).ok()?;
        if !force && !due(expires_at, now) {
            return None;
        }
        return Some((
            None,
            Request::Post {
                url: flow.url,
                body,
            },
        ));
    }
    let id = due_entries(cli, record, now, force).into_iter().next()?;
    let request = entry_request(cli, record, &id)?;
    Some((Some(id), request))
}

/// The entries of a multi-entry record that a pass renews at `now`, soonest-expiring first:
/// those due (or, with `force`, every renewable one). Each is tried once per pass.
pub(crate) fn due_entries(cli: &str, record: &Value, now: u64, force: bool) -> Vec<String> {
    if cli != "pi" {
        return Vec::new();
    }
    let mut candidates: Vec<(u64, &str)> = PI_ENTRIES
        .iter()
        .filter_map(|id| {
            let entry = record.get(*id)?;
            (entry.get("type").and_then(Value::as_str) == Some("oauth")).then_some(())?;
            str_at(entry, "/refresh")?;
            // An entry with no expiry is renewable but never due on its own: only `force` takes
            // it (last).
            let exp = entry
                .get("expires")
                .and_then(Value::as_u64)
                .filter(|ms| *ms > 0)
                .map(|ms| ms / 1000);
            Some((exp.unwrap_or(u64::MAX), *id))
        })
        .filter(|(exp, _)| force || (*exp != u64::MAX && due(Some(*exp), now)))
        .collect();
    candidates.sort();
    candidates
        .into_iter()
        .map(|(_, id)| id.to_string())
        .collect()
}

/// The token request that renews entry `id` of `cli`'s record.
pub(crate) fn entry_request(cli: &str, record: &Value, id: &str) -> Option<Request> {
    let _ = cli;
    let entry = record.get(id)?;
    let refresh = str_at(entry, "/refresh")?.to_string();
    let request = match id {
        "anthropic" => {
            let flow = flow("claude")?;
            Request::Post {
                url: flow.url,
                body: json!({"grant_type": "refresh_token", "refresh_token": refresh,
                    "client_id": flow.client_id}),
            }
        }
        "openai-codex" => {
            let flow = flow("codex")?;
            Request::Post {
                url: flow.url,
                body: json!({"grant_type": "refresh_token", "refresh_token": refresh,
                    "client_id": flow.client_id, "scope": "openid profile email"}),
            }
        }
        _ => Request::BearerGet {
            url: test_url().unwrap_or_else(|| COPILOT_EXCHANGE_URL.into()),
            token: refresh,
            headers: PI_COPILOT_HEADERS.to_vec(),
        },
    };
    Some(request)
}

/// Fold a token answer into pi's entry `id` of `record` (at `now`, Unix seconds).
pub(crate) fn apply_pi_answer(
    record: &Value,
    id: &str,
    answer: &Value,
    now: u64,
) -> Result<Value, String> {
    let mut out = record.clone();
    let entry = out
        .get_mut(id)
        .and_then(Value::as_object_mut)
        .ok_or_else(|| format!("pi: the record has no {id} entry"))?;
    if id == "github-copilot" {
        let token =
            str_at(answer, "/token").ok_or("pi: the Copilot token exchange returned no token")?;
        let expires_at = answer
            .get("expires_at")
            .and_then(Value::as_u64)
            .ok_or("pi: the Copilot token exchange returned no expires_at")?;
        entry.insert("access".into(), json!(token));
        entry.insert("expires".into(), json!(expires_at.saturating_mul(1000)));
        // The plan endpoint THIS exchange names, kept (in the broker's copy only) for a token
        // that names no `proxy-ep`; an answer naming none drops an earlier generation's.
        match str_at(answer, "/endpoints/api") {
            Some(api) => {
                entry.insert(crate::seat_login::COPILOT_ENDPOINT_FIELD.into(), json!(api));
            }
            None => {
                entry.remove(crate::seat_login::COPILOT_ENDPOINT_FIELD);
            }
        }
        return Ok(out);
    }
    let access =
        str_at(answer, "/access_token").ok_or("pi: the token endpoint returned no access_token")?;
    if answer.get("refresh_token").is_some() && str_at(answer, "/refresh_token").is_none() {
        return Err("pi: invalid refresh_token in token answer".into());
    }
    let expires_in = answer
        .get("expires_in")
        .and_then(Value::as_u64)
        .ok_or("pi: the token endpoint returned no expires_in")?;
    entry.insert("access".into(), json!(access));
    if let Some(r) = str_at(answer, "/refresh_token") {
        entry.insert("refresh".into(), json!(r));
    }
    entry.insert(
        "expires".into(),
        json!(now.saturating_add(expires_in).saturating_mul(1000)),
    );
    Ok(out)
}

fn str_at<'a>(v: &'a Value, p: &str) -> Option<&'a str> {
    crate::seat_login::nonempty(v.pointer(p))
}

/// The token request body for `record`; `None` when the record is not a refreshable OAuth
/// sign-in (an API key, a record without a refresh token).
pub(crate) fn request_body(cli: &str, record: &Value, flow: &Flow) -> Option<Value> {
    match cli {
        "claude" => {
            let rt = str_at(record, "/claudeAiOauth/refreshToken")?;
            let mut body = json!({
                "grant_type": "refresh_token",
                "refresh_token": rt,
                "client_id": flow.client_id,
            });
            let scopes: Vec<&str> = record
                .pointer("/claudeAiOauth/scopes")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            if !scopes.is_empty() {
                body["scope"] = json!(scopes.join(" "));
            }
            Some(body)
        }
        "codex" => {
            if record.get("auth_mode").and_then(Value::as_str) == Some("apikey") {
                return None;
            }
            let rt = str_at(record, "/tokens/refresh_token")?;
            Some(json!({
                "client_id": flow.client_id,
                "grant_type": "refresh_token",
                "refresh_token": rt,
                "scope": "openid profile email",
            }))
        }
        _ => None,
    }
}

/// Fold a token endpoint's answer into `record` (at `now`, Unix seconds). The provider may or
/// may not rotate the refresh token; an absent one keeps the old.
pub(crate) fn apply_answer(
    cli: &str,
    record: &Value,
    answer: &Value,
    now: u64,
) -> Result<Value, String> {
    let access = str_at(answer, "/access_token")
        .ok_or_else(|| format!("{cli}: the token endpoint returned no access_token"))?;
    // agy lg-r1 A5: absent rotation keeps the old token; a malformed rotation is an error.
    for key in ["refresh_token", "id_token"] {
        if answer.get(key).is_some() && str_at(answer, &format!("/{key}")).is_none() {
            return Err(format!("{cli}: invalid {key} in token answer"));
        }
    }
    let mut out = record.clone();
    match cli {
        "claude" => {
            let o = out
                .get_mut("claudeAiOauth")
                .and_then(Value::as_object_mut)
                .ok_or("claude: the record has no claudeAiOauth")?;
            o.insert("accessToken".into(), json!(access));
            if let Some(r) = str_at(answer, "/refresh_token") {
                o.insert("refreshToken".into(), json!(r));
            }
            let expires_in = answer
                .get("expires_in")
                .and_then(Value::as_u64)
                .ok_or("claude: the token endpoint returned no expires_in")?;
            o.insert(
                "expiresAt".into(),
                json!(now.saturating_add(expires_in).saturating_mul(1000)),
            );
        }
        "codex" => {
            let t = out
                .get_mut("tokens")
                .and_then(Value::as_object_mut)
                .ok_or("codex: the record has no tokens")?;
            t.insert("access_token".into(), json!(access));
            for k in ["refresh_token", "id_token"] {
                if let Some(v) = str_at(answer, &format!("/{k}")) {
                    t.insert(k.into(), json!(v));
                }
            }
            // codex's own expiry is the access JWT's `exp` (read by the broker status); an
            // explicit expires_in is recorded too when the endpoint gives one.
            match answer.get("expires_in").and_then(Value::as_u64) {
                Some(s) => {
                    t.insert("expires_at".into(), json!(now.saturating_add(s)));
                }
                None => {
                    t.remove("expires_at");
                }
            }
            out["last_refresh"] = json!(rfc3339(now));
        }
        _ => return Err(format!("{cli}: not refreshable")),
    }
    Ok(out)
}

/// `now` as `YYYY-MM-DDTHH:MM:SSZ` (codex's `last_refresh` spelling).
fn rfc3339(now: u64) -> String {
    let days = now / 86_400;
    let secs = now % 86_400;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        (secs / 60) % 60,
        secs % 60
    )
}

/// Whether a credential expiring at `expires_at` is due at `now`. An unknown expiry is never due.
pub(crate) fn due(expires_at: Option<u64>, now: u64) -> bool {
    expires_at.is_some_and(|e| e <= now.saturating_add(REFRESH_MARGIN_SECS))
}

/// What one refresh did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(crate) enum Outcome {
    /// Not signed in with a refreshable OAuth record (signed out, an API key).
    NotRefreshable,
    NotDue {
        expires_at: Option<u64>,
    },
    Refreshed {
        expires_at: Option<u64>,
    },
    /// Another pass holds the lock.
    Busy,
}

/// The last refresh's result, kept host-side in the gateway dir (no credential in it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LastRefresh {
    pub(crate) ok: bool,
    pub(crate) at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
}

fn last_path(dir: &Path, cli: &str) -> PathBuf {
    dir.join("refresh").join(format!("{cli}.json"))
}

/// The last recorded refresh of `cli` under gateway dir `dir`.
pub(crate) fn last_refresh(dir: &Path, cli: &str) -> Option<LastRefresh> {
    serde_json::from_slice(&std::fs::read(last_path(dir, cli)).ok()?).ok()
}

/// Record (or, with `None`, clear: a fresh sign-in or a sign-out) the last refresh.
pub(crate) fn record_last(dir: &Path, cli: &str, last: Option<&LastRefresh>) {
    let p = last_path(dir, cli);
    match last {
        None => {
            let _ = std::fs::remove_file(&p);
        }
        Some(l) => {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let tmp = p.with_extension("json.new");
            if serde_json::to_vec(l)
                .ok()
                .and_then(|b| std::fs::write(&tmp, b).ok())
                .is_some()
            {
                let _ = std::fs::rename(&tmp, &p);
            }
        }
    }
}

// codex lg-r1 #2: every broker mutation uses this file, not a process-local mutex.
pub(crate) fn open_mutation_lock(dir: &Path) -> Result<std::fs::File, String> {
    std::fs::create_dir_all(dir.join("refresh")).map_err(|e| e.to_string())?;
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("refresh/lock"))
        .map_err(|e| e.to_string())
}

/// The held broker mutation lock. Dropping it UNLOCKS before it closes: an `flock` belongs to the
/// open file description, and a child that another thread forks while the lock is held shares
/// that description until its exec, so a bare close could leave the lock held after its owner
/// is done (a concurrent refresher then saw `Busy` for a free broker). An explicit unlock
/// releases it for every descriptor sharing the description.
pub(crate) struct MutationLock(std::fs::File);

impl Drop for MutationLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

pub(crate) fn mutation_lock(dir: &Path) -> Result<MutationLock, String> {
    let file = open_mutation_lock(dir)?;
    file.lock().map_err(|e| e.to_string())?;
    Ok(MutationLock(file))
}

/// [`mutation_lock`] without waiting: `None` when it cannot be taken now (another owner holds it).
pub(crate) fn try_mutation_lock(dir: &Path) -> Result<Option<MutationLock>, String> {
    let file = open_mutation_lock(dir)?;
    Ok(file.try_lock().ok().map(|()| MutationLock(file)))
}

pub(crate) fn pending_path(dir: &Path, cli: &str) -> PathBuf {
    dir.join("refresh").join(format!("{cli}.pending"))
}

// codex lg-r3 R2/R4/R5: journal entries must reach disk before their mutation can start.
pub(crate) fn create_private_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path).map_err(|e| e.to_string())?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| e.to_string())?;
    sync_parent(path)
}

fn sync_parent(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    std::fs::File::open(path.parent().ok_or("journal has no parent")?)
        .and_then(|f| f.sync_all())
        .map_err(|e| e.to_string())?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

pub(crate) fn sync_private_file(path: &Path) -> Result<(), String> {
    std::fs::File::open(path)
        .and_then(|f| f.sync_all())
        .map_err(|e| e.to_string())?;
    sync_parent(path)
}

pub(crate) fn remove_private_file(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => sync_parent(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

fn write_pending_state(dir: &Path, cli: &str, state: &[u8]) -> Result<(), String> {
    let path = pending_path(dir, cli);
    let tmp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
    let result = (|| {
        create_private_file(&tmp, state)?;
        std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
        sync_parent(&path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    result
}

/// The marker of a route-literal publication not yet confirmed (removed once it succeeds).
fn literals_pending_path(dir: &Path, cli: &str) -> PathBuf {
    dir.join("refresh").join(format!("{cli}.literals.pending"))
}

pub(crate) fn rescue_path(dir: &Path, cli: &str) -> PathBuf {
    dir.join("refresh").join(format!("{cli}.rescue"))
}

const ROTATION_ISSUED: &[u8] = b"rotated credential issued";
const ROTATION_STORED: &[u8] = b"rotated credential stored";
const ROTATION_UNSTORED: &str = "rotated credential not yet stored in the broker";

fn rotation_unstored(dir: &Path, cli: &str) -> String {
    record_last(
        dir,
        cli,
        Some(&LastRefresh {
            ok: false,
            at: unix_now(),
            error: Some(ROTATION_UNSTORED.into()),
        }),
    );
    ROTATION_UNSTORED.into()
}

fn stage_rotated(
    next: &str,
    bytes: &[u8],
    write: &mut impl FnMut(&str, &[u8]) -> Result<(), String>,
) -> Result<(), String> {
    // codex lg-r3 R2: retry the returned generation, never the invalidated refresh token.
    for _ in 0..3 {
        if write(next, bytes).is_ok() {
            return Ok(());
        }
    }
    Err(ROTATION_UNSTORED.into())
}

pub(crate) fn clear_pending(dir: &Path, cli: &str) -> Result<(), String> {
    remove_private_file(&pending_path(dir, cli))
}

// codex lg-r1 #7: a flag survives a partial commit; it NEVER holds record bytes.
fn commit_refresh(
    dir: &Path,
    cli: &str,
    record: &str,
    files: &[(String, Vec<u8>)],
    mut write: impl FnMut(&str, &[u8]) -> Result<(), String>,
    mut remove: impl FnMut(&str) -> Result<(), String>,
) -> Result<(), String> {
    std::fs::create_dir_all(dir.join("refresh")).map_err(|e| e.to_string())?;
    let retained = files
        .iter()
        .find(|(n, _)| n == record)
        .ok_or("refresh record missing")?;
    // codex lg-r2 C2: retain the rotated generation before replacing the current record.
    let next = format!("{record}.next");
    if write_pending_state(dir, cli, ROTATION_ISSUED)
        .and_then(|()| stage_rotated(&next, &retained.1, &mut write))
        .is_err()
    {
        // codex lg-r3 R2: the covered gateway dir is never mounted into a seat. Exclusive
        // creation avoids overwriting a rescue generation that has not yet been recovered.
        let saved = create_private_file(&rescue_path(dir, cli), &retained.1);
        let error = rotation_unstored(dir, cli);
        return saved.and(Err(error));
    }
    write(&retained.0, &retained.1)?;
    for (name, bytes) in files.iter().filter(|(n, _)| n != record) {
        write(name, bytes)?;
    }
    write_pending_state(dir, cli, ROTATION_STORED)?;
    remove(&next)?;
    clear_pending(dir, cli)
}

fn repair_pending(
    dir: &Path,
    cli: &str,
    bytes: Option<&[u8]>,
    mut write: impl FnMut(&str, &[u8]) -> Result<(), String>,
    mut remove: impl FnMut(&str) -> Result<(), String>,
) -> Result<bool, String> {
    if !pending_path(dir, cli)
        .try_exists()
        .map_err(|e| e.to_string())?
    {
        return Ok(false);
    }
    let record = record_file(cli).ok_or("not refreshable")?;
    let bytes = bytes.ok_or("pending refresh has no recovery record")?;
    let files = crate::seat_login::extract_tokens(
        cli,
        &[(
            login_path(cli).ok_or("not refreshable")?.into(),
            bytes.to_vec(),
        )],
    )?;
    for (name, bytes) in files {
        write(&name, &bytes)?;
    }
    write_pending_state(dir, cli, ROTATION_STORED)?;
    remove(&format!("{record}.next"))?;
    clear_pending(dir, cli)?;
    // agy lg-r1 A4: successful repair supersedes the failed local publication.
    record_last(
        dir,
        cli,
        Some(&LastRefresh {
            ok: true,
            at: unix_now(),
            error: None,
        }),
    );
    Ok(true)
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn encode_request(body: &Value) -> Result<Vec<u8>, String> {
    let bytes = serde_json::to_vec(body).map_err(|_| "could not encode the request")?;
    // codex lg-r2 C6: an accepted record cannot stream an unbounded token request.
    if bytes.len() > 64 * 1024 {
        return Err("token request exceeds 64 KiB".into());
    }
    Ok(bytes)
}

#[cfg(unix)]
pub(crate) fn repair_refresh(
    rt: &crate::container_floor::Runtime,
    gw: &crate::seat_gateway::Gateway,
    dir: &Path,
    cli: &str,
) -> Result<Option<Vec<u8>>, String> {
    if crate::seat_login::complete_sign_out(rt, gw, dir, cli)? {
        return Ok(None);
    }
    let Some(file) = record_file(cli) else {
        return Ok(None);
    };
    if crate::seat_login::signin_pending(dir, cli) {
        return Err("sign-in incomplete".into());
    }
    let rescue = match std::fs::read(rescue_path(dir, cli)) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.to_string()),
    };
    let bytes = if let Some(bytes) = rescue {
        write_pending_state(dir, cli, ROTATION_ISSUED)?;
        stage_rotated(&format!("{file}.next"), &bytes, &mut |name, body| {
            crate::seat_gateway::set_broker_credential(rt, gw, dir, cli, name, body)
        })
        .map_err(|_| rotation_unstored(dir, cli))?;
        remove_private_file(&rescue_path(dir, cli))?;
        Some(bytes)
    } else {
        let pending = pending_path(dir, cli)
            .try_exists()
            .map_err(|e| e.to_string())?;
        let staged = crate::seat_login::read_broker_record(rt, gw, &format!("{file}.next"))?;
        match staged {
            // codex lg-r4: a staged record is trusted only under its pending marker (written
            // before staging); one without it is an orphan writer's and is discarded.
            Some(_) if !pending => {
                crate::seat_gateway::remove_broker_file(rt, gw, dir, cli, &format!("{file}.next"))?;
                crate::seat_login::read_broker_record(rt, gw, file)?
            }
            Some(bytes) => Some(bytes),
            None if !pending
                || std::fs::read(pending_path(dir, cli)).map_err(|e| e.to_string())?
                    == ROTATION_STORED =>
            {
                crate::seat_login::read_broker_record(rt, gw, file)?
            }
            // codex lg-r3 R2: no evidence the issued generation reached the broker.
            None => return Err(rotation_unstored(dir, cli)),
        }
    };
    repair_pending(
        dir,
        cli,
        bytes.as_deref(),
        |name, body| crate::seat_gateway::set_broker_credential(rt, gw, dir, cli, name, body),
        |name| crate::seat_gateway::remove_broker_file(rt, gw, dir, cli, name),
    )?;
    Ok(bytes)
}

#[cfg(unix)]
mod live {
    use super::*;
    use crate::container_floor::{output_within, Runtime};
    use crate::seat_gateway::Gateway;
    use std::time::Duration;

    const POST_TIMEOUT: Duration = Duration::from_secs(45);
    /// The largest token answer read back.
    const MAX_ANSWER: usize = 256 * 1024;

    /// POST `body` to `url` from a short-lived hardened curl container on `network`; the body
    /// rides stdin and the answer stdout. Errors name the status only, never the answer.
    #[cfg(test)]
    pub(crate) fn post_json(
        rt: &Runtime,
        dir: &Path,
        cli: &str,
        network: &str,
        url: &str,
        body: &Value,
    ) -> Result<Value, String> {
        send(
            rt,
            dir,
            cli,
            network,
            &Request::Post {
                url: url.into(),
                body: body.clone(),
            },
        )
    }

    /// The curl argv and stdin of `request` (pure): a POST's body and a GET's credential header
    /// both ride stdin, so no credential is ever a command-line argument.
    pub(crate) fn request_argv(request: &Request) -> Result<(Vec<String>, Vec<u8>), String> {
        let mut argv: Vec<String> = ["-sS", "--max-time", "30", "--fail"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let stdin = match request {
            Request::Post { url, body } => {
                argv.extend(
                    [
                        "-X",
                        "POST",
                        "-H",
                        "content-type: application/json",
                        "-H",
                        "accept: application/json",
                        "--data-binary",
                        "@-",
                        url,
                    ]
                    .map(str::to_string),
                );
                encode_request(body)?
            }
            Request::BearerGet {
                url,
                token,
                headers,
            } => {
                if token.chars().any(|c| c.is_control() || c.is_whitespace()) {
                    return Err("invalid credential token".into());
                }
                for (name, value) in headers {
                    argv.extend(["-H".to_string(), format!("{name}: {value}")]);
                }
                argv.extend(["-H".to_string(), "@-".to_string(), url.clone()]);
                format!("authorization: Bearer {token}\n").into_bytes()
            }
        };
        Ok((argv, stdin))
    }

    /// Make `request` from a short-lived hardened curl container on `network`; the answer comes
    /// back on stdout. Errors name the status only, never the answer.
    pub(crate) fn send(
        rt: &Runtime,
        dir: &Path,
        cli: &str,
        network: &str,
        request: &Request,
    ) -> Result<Value, String> {
        let (curl, bytes) = request_argv(request)?;
        if !crate::container_floor::image_present(rt, REFRESH_IMAGE) {
            let mut pull = rt.cmd();
            pull.args(["pull", "-q", REFRESH_IMAGE]);
            match output_within(pull, POST_TIMEOUT) {
                Some(o) if o.status.success() => {}
                _ => {
                    return Err(format!(
                        "could not pull the refresher image {REFRESH_IMAGE}"
                    ))
                }
            }
        }
        let argv = [
            "run",
            "--rm",
            "--log-driver",
            "none",
            "-i",
            "--network",
            network,
            "--read-only",
            "--cap-drop",
            "ALL",
            "--security-opt",
            "no-new-privileges",
            "--pids-limit",
            "16",
            "--memory",
            "64m",
            REFRESH_IMAGE,
        ]
        .into_iter()
        .map(str::to_string)
        .chain(curl)
        .collect::<Vec<_>>();
        let out = crate::seat_gateway::mutation_helper(rt, dir, cli, &argv, &bytes, POST_TIMEOUT)?;
        if out.len() > MAX_ANSWER {
            return Err("the token answer exceeds its size limit".into());
        }
        serde_json::from_slice(&out).map_err(|_| "the token answer is not JSON".into())
    }

    /// Refresh `cli` when its credential is due (or `force`). One pass per gateway dir at a time.
    pub(crate) fn refresh(
        rt: &Runtime,
        gw: &Gateway,
        dir: &Path,
        cli: &str,
        force: bool,
    ) -> Result<Outcome, String> {
        let Some(_lock) = try_mutation_lock(dir)? else {
            return Ok(Outcome::Busy);
        };
        crate::seat_gateway::cleanup_mutations(rt, dir)?;
        if crate::seat_login::complete_sign_out(rt, gw, dir, cli)? {
            return Ok(Outcome::NotRefreshable);
        }
        let (Some(flow), Some(file)) = (
            flow(if cli == "pi" { "claude" } else { cli }),
            record_file(cli),
        ) else {
            return Ok(Outcome::NotRefreshable);
        };
        crate::seat_login::repair_ingest(rt, gw, dir, cli)?;
        if crate::seat_login::signin_pending(dir, cli) {
            return Err("sign-in incomplete; retry ingest first".into());
        }
        // Re-read only after acquiring the broker lock (codex lg-r1 #2).
        let bytes = repair_refresh(rt, gw, dir, cli)?;
        let Some(bytes) = bytes else {
            return Ok(Outcome::NotRefreshable);
        };
        let mut record: Value =
            serde_json::from_slice(&bytes).map_err(|_| format!("{cli}: unreadable record"))?;
        if let Some(body) = request_body(cli, &record, &flow) {
            encode_request(&body)?; // codex lg-r2 C6: reject oversized records even before NotDue.
        }
        let now = unix_now();
        let multi = cli == "pi";
        // A multi-entry record renews each eligible entry once per pass, one committed generation
        // at a time, so a rotated refresh token is stored before the next entry's request is made
        // (DES-SEAT-LOGIN-SUBSCRIPTIONS decision 8). One entry's refused request never strands the
        // others; a failure to store a generation stops the pass.
        let plan: Vec<Option<String>> = if multi {
            due_entries(cli, &record, now, force)
                .into_iter()
                .map(Some)
                .collect()
        } else if next_refresh(cli, &record, now, force).is_some() {
            vec![None]
        } else {
            Vec::new()
        };
        let refreshable = if multi {
            !due_entries(cli, &record, now, true).is_empty()
        } else {
            request_body(cli, &record, &flow).is_some()
        };
        let mut refreshed = false;
        let mut failures: Vec<String> = Vec::new();
        for entry in &plan {
            let request = match entry {
                Some(id) => entry_request(cli, &record, id),
                None => next_refresh(cli, &record, now, force).map(|(_, r)| r),
            };
            let Some(request) = request else {
                continue;
            };
            let answered =
                send(rt, dir, cli, &gw.external_network, &request).and_then(|answer| match entry {
                    Some(id) => apply_pi_answer(&record, id, &answer, now),
                    None => apply_answer(cli, &record, &answer, now),
                });
            let new = match answered {
                Ok(new) => new,
                Err(e) if multi => {
                    failures.push(format!("{}: {e}", entry.as_deref().unwrap_or(cli)));
                    continue;
                }
                Err(e) => {
                    failures.push(e);
                    break;
                }
            };
            let committed = (|| {
                let new_bytes =
                    serde_json::to_vec(&new).map_err(|_| "could not encode the record")?;
                let files = crate::seat_login::extract_tokens(
                    cli,
                    &[(login_path(cli).unwrap_or_default().into(), new_bytes)],
                )?;
                commit_refresh(
                    dir,
                    cli,
                    file,
                    &files,
                    |name, b| crate::seat_gateway::set_broker_credential(rt, gw, dir, cli, name, b),
                    |name| crate::seat_gateway::remove_broker_file(rt, gw, dir, cli, name),
                )
            })();
            if let Err(e) = committed {
                failures.push(e);
                break;
            }
            record = new;
            refreshed = true;
        }
        // The route literals follow the stored record on every pass (a renewed Copilot token can
        // name another plan host; a crash between a commit and its publication heals here).
        // A publication that failed (or was interrupted) is retried until it succeeds: its
        // marker is written before and removed only after the gateway has the literals.
        if multi && failures.is_empty() {
            let published = (|| {
                let bytes =
                    serde_json::to_vec(&record).map_err(|_| "could not encode the record")?;
                let files = crate::seat_login::extract_tokens(
                    cli,
                    &[(login_path(cli).unwrap_or_default().into(), bytes)],
                )?;
                let literals = crate::seat_login::route_literals(cli, &files)?;
                let marker = literals_pending_path(dir, cli);
                if marker.exists()
                    || !crate::seat_gateway::route_literals_current(dir, cli, &literals)?
                {
                    if !marker.exists() {
                        create_private_file(&marker, b"route literals unpublished")?;
                    }
                    crate::seat_gateway::route_sign_in(rt, gw, dir, cli, &literals)?;
                    remove_private_file(&marker)?;
                }
                Ok::<(), String>(())
            })();
            if let Err(e) = published {
                failures.push(format!("route literals: {e}"));
            }
        }
        if !plan.is_empty() || !failures.is_empty() {
            record_last(
                dir,
                cli,
                Some(&LastRefresh {
                    ok: failures.is_empty(),
                    at: now,
                    error: (!failures.is_empty()).then(|| failures.join("; ")),
                }),
            );
        }
        if !failures.is_empty() {
            return Err(failures.join("; "));
        }
        let expires_at = crate::seat_login::record_expiry(cli, &record)?;
        if refreshed {
            Ok(Outcome::Refreshed { expires_at })
        } else if !refreshable {
            Ok(Outcome::NotRefreshable)
        } else {
            Ok(Outcome::NotDue { expires_at })
        }
    }

    /// Start the daemon's refresher loop for gateway dir `dir`, once per process: a first pass
    /// runs now (a credential that expired between runs is refreshed before the seat starts),
    /// then one every [`PASS_EVERY`].
    pub(crate) fn start(rt: &Runtime, gw: &Gateway, dir: &Path) {
        static STARTED: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());
        {
            let mut s = STARTED.lock().unwrap_or_else(|p| p.into_inner());
            if s.iter().any(|d| d == dir) {
                return;
            }
            s.push(dir.to_path_buf());
        }
        let pass = {
            let (rt, gw, dir) = (rt.clone(), gw.clone(), dir.to_path_buf());
            move || {
                for cli in REFRESHABLE {
                    if let Err(e) = refresh(&rt, &gw, &dir, cli, false) {
                        eprintln!("wicked-core: refreshing the {cli} sign-in failed: {e}");
                    }
                }
            }
        };
        pass();
        let _ = std::thread::Builder::new()
            .name("wicked-broker-refresher".into())
            .spawn(move || loop {
                std::thread::sleep(PASS_EVERY);
                pass();
            });
    }
}

#[cfg(all(unix, test))]
pub(crate) use live::request_argv;
#[cfg(unix)]
pub(crate) use live::{refresh, start};

#[cfg(test)]
mod tests {
    use super::*;

    fn pi_record() -> Value {
        json!({
            "openai": {"type": "api_key", "key": "FAKE-key"},
            "anthropic": {"type": "oauth", "access": "sk-ant-oat01-FAKE-a",
                "refresh": "FAKE-ant-r", "expires": 2_000_000_000_000_u64},
            "openai-codex": {"type": "oauth", "access": "FAKE.jwt.x", "refresh": "FAKE-oai-r",
                "expires": 1_000_000_000_000_u64, "accountId": "acct-FAKE"},
            "github-copilot": {"type": "oauth", "access": "tid=FAKE", "refresh": "FAKE-gho",
                "expires": 1_500_000_000_000_u64}
        })
    }

    /// Decision 8: pi renews the entry expiring first, with that provider's own flow.
    #[test]
    fn pi_refreshes_the_earliest_due_entry_with_its_providers_flow() {
        let record = pi_record();
        assert_eq!(next_refresh("pi", &record, 100, false), None, "nothing due");
        let (entry, request) = next_refresh("pi", &record, 1_000_000_000, false).unwrap();
        assert_eq!(entry.as_deref(), Some("openai-codex"));
        let Request::Post { url, body } = request else {
            panic!("an OAuth refresh is a POST")
        };
        assert_eq!(url, "https://auth.openai.com/oauth/token");
        assert_eq!(body["refresh_token"], "FAKE-oai-r");
        assert_eq!(body["client_id"], "app_EMoamEEZ73f0CkXaXp7hrann");
        // Once codex's entry is renewed, Copilot's exchange is next.
        let renewed = apply_pi_answer(
            &record,
            "openai-codex",
            &json!({"access_token": "FAKE-new-a", "refresh_token": "FAKE-new-r", "expires_in": 3600}),
            1_600_000_000,
        )
        .unwrap();
        assert_eq!(renewed["openai-codex"]["refresh"], "FAKE-new-r");
        assert_eq!(renewed["openai-codex"]["expires"], 1_600_003_600_000_u64);
        assert_eq!(renewed["openai-codex"]["accountId"], "acct-FAKE");
        let (entry, request) = next_refresh("pi", &renewed, 1_500_000_000, false).unwrap();
        assert_eq!(entry.as_deref(), Some("github-copilot"));
        let Request::BearerGet {
            url,
            token,
            headers,
        } = request.clone()
        else {
            panic!("Copilot's exchange is a GET")
        };
        assert_eq!(url, "https://api.github.com/copilot_internal/v2/token");
        assert_eq!(token, "FAKE-gho");
        assert!(headers.contains(&("copilot-integration-id", "vscode-chat")));
        let exchanged = apply_pi_answer(
            &renewed,
            "github-copilot",
            &json!({"token": "tid=FAKE-2;proxy-ep=proxy.individual.githubcopilot.com", "expires_at": 1_500_001_800_u64}),
            1_500_000_000,
        )
        .unwrap();
        assert_eq!(
            exchanged["github-copilot"]["refresh"], "FAKE-gho",
            "the GitHub token stays"
        );
        assert_eq!(
            exchanged["github-copilot"]["expires"],
            1_500_001_800_000_u64
        );
        // A forced refresh takes the earliest entry; Anthropic's renews by claude's flow.
        let (entry, _) = next_refresh("pi", &exchanged, 0, true).unwrap();
        assert_eq!(entry.as_deref(), Some("github-copilot"));
        let mut only_anthropic = exchanged.clone();
        only_anthropic["github-copilot"]["type"] = json!("api_key");
        only_anthropic["openai-codex"]
            .as_object_mut()
            .unwrap()
            .remove("refresh");
        let (entry, request) = next_refresh("pi", &only_anthropic, 0, true).unwrap();
        assert_eq!(entry.as_deref(), Some("anthropic"));
        assert!(
            matches!(request, Request::Post { ref url, .. } if url.contains("platform.claude.com"))
        );
        // A pass plans every renewable entry when forced (each tried once), only the due ones
        // otherwise, and none for a record without refresh tokens (not refreshable).
        assert_eq!(
            due_entries("pi", &record, 0, true),
            vec!["openai-codex", "github-copilot", "anthropic"]
        );
        assert_eq!(
            due_entries("pi", &record, 1_499_999_500, false),
            vec!["openai-codex", "github-copilot"]
        );
        let mut no_refresh = record.clone();
        for id in ["anthropic", "openai-codex", "github-copilot"] {
            no_refresh[id].as_object_mut().unwrap().remove("refresh");
        }
        assert!(due_entries("pi", &no_refresh, 0, true).is_empty());
        // An entry with no expiry is renewable when forced, never due by itself.
        let mut no_expiry = record.clone();
        no_expiry["anthropic"]
            .as_object_mut()
            .unwrap()
            .remove("expires");
        assert!(due_entries("pi", &no_expiry, 3_000_000_000, false)
            .iter()
            .all(|e| e != "anthropic"));
        assert_eq!(
            due_entries("pi", &no_expiry, 0, true)
                .last()
                .map(String::as_str),
            Some("anthropic")
        );
        // The exchange's plan endpoint is kept for a token that names none.
        let with_endpoint = apply_pi_answer(
            &record,
            "github-copilot",
            &json!({"token": "tid=FAKE-3", "expires_at": 1_u64,
                "endpoints": {"api": "https://api.business.githubcopilot.com"}}),
            0,
        )
        .unwrap();
        assert_eq!(
            with_endpoint["github-copilot"][crate::seat_login::COPILOT_ENDPOINT_FIELD],
            "https://api.business.githubcopilot.com"
        );
        // A later exchange naming no endpoint drops the earlier generation's.
        let without = apply_pi_answer(
            &with_endpoint,
            "github-copilot",
            &json!({"token": "tid=FAKE-4", "expires_at": 2_u64}),
            0,
        )
        .unwrap();
        assert!(without["github-copilot"]
            .get(crate::seat_login::COPILOT_ENDPOINT_FIELD)
            .is_none());
        // A refresh answer must carry what the CLI needs.
        assert!(apply_pi_answer(&record, "anthropic", &json!({"access_token": "x"}), 1).is_err());
        assert!(apply_pi_answer(&record, "github-copilot", &json!({"token": "x"}), 1).is_err());
        assert!(apply_pi_answer(
            &record,
            "xai",
            &json!({"access_token": "x", "expires_in": 1}),
            1
        )
        .is_err());
    }

    /// The exchange's GitHub token rides stdin as a header, never a command-line argument.
    #[cfg(unix)]
    #[test]
    fn a_bearer_get_keeps_its_token_off_the_command_line() {
        let request = Request::BearerGet {
            url: "https://api.github.com/copilot_internal/v2/token".into(),
            token: "FAKE-gho-secret".into(),
            headers: PI_COPILOT_HEADERS.to_vec(),
        };
        let (argv, stdin) = request_argv(&request).unwrap();
        assert!(!argv.iter().any(|a| a.contains("FAKE-gho-secret")));
        assert_eq!(stdin, b"authorization: Bearer FAKE-gho-secret\n");
        assert!(argv.windows(2).any(|w| w == ["-H", "@-"]));
        let injected = Request::BearerGet {
            url: "u".into(),
            token: "FAKE\r\nx-evil: 1".into(),
            headers: Vec::new(),
        };
        assert!(request_argv(&injected).is_err());
        let (argv, stdin) = request_argv(&Request::Post {
            url: "u".into(),
            body: json!({"refresh_token": "FAKE-r"}),
        })
        .unwrap();
        assert!(!argv.iter().any(|a| a.contains("FAKE-r")));
        assert!(String::from_utf8(stdin).unwrap().contains("FAKE-r"));
    }

    #[test]
    fn first_staging_failure_retries_and_exhaustion_rescues_rotated_record_exclusively() {
        for failures in [1, 3] {
            let dir =
                std::env::temp_dir().join(format!("c4-stage-{}", uuid::Uuid::new_v4().simple()));
            let new = apply_answer(
                "claude",
                &claude_record(),
                &json!({"access_token":"FAKE-new", "refresh_token":"FAKE-R2", "expires_in":3600}),
                2_000_000_000,
            )
            .unwrap();
            let record = serde_json::to_vec(&new).unwrap();
            let files = crate::seat_login::extract_tokens(
                "claude",
                &[("claude/.credentials.json".into(), record.clone())],
            )
            .unwrap();
            let mut attempts = 0;
            let mut writes = Vec::new();
            let result = commit_refresh(
                &dir,
                "claude",
                "claude.credentials.json",
                &files,
                |name, bytes| {
                    if name.ends_with(".next") {
                        attempts += 1;
                        assert_eq!(bytes, record.as_slice());
                        if attempts <= failures {
                            return Err("injected staging failure".into());
                        }
                    }
                    writes.push(name.to_string());
                    Ok(())
                },
                |_| Ok(()),
            );
            if failures == 1 {
                result.unwrap();
                assert_eq!(attempts, 2);
                assert!(writes.iter().any(|n| n == "anthropic.token"));
                assert!(!rescue_path(&dir, "claude").exists());
                assert!(!pending_path(&dir, "claude").exists());
            } else {
                assert_eq!(result.unwrap_err(), ROTATION_UNSTORED);
                assert_eq!(attempts, 3);
                assert!(writes.is_empty());
                let rescue = rescue_path(&dir, "claude");
                assert_eq!(std::fs::read(&rescue).unwrap(), record);
                assert!(create_private_file(&rescue, b"FAKE-older").is_err());
                assert_eq!(std::fs::read(&rescue).unwrap(), record);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    assert_eq!(
                        std::fs::metadata(rescue).unwrap().permissions().mode() & 0o777,
                        0o600
                    );
                }
                assert_eq!(
                    std::fs::read(pending_path(&dir, "claude")).unwrap(),
                    ROTATION_ISSUED
                );
                let last = last_refresh(&dir, "claude").unwrap();
                assert!(!last.ok);
                assert_eq!(last.error.as_deref(), Some(ROTATION_UNSTORED));
            }
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn token_answer_uses_attached_stdio_without_runtime_logging() {
        let dir = std::env::temp_dir().join(format!("c4-post-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let runtime = dir.join("runtime");
        crate::test_env::install_stub(
            &runtime,
            r#"#!/bin/sh
base=${0%/*}
if [ "$1 $2" = 'image inspect' ]; then exit 0; fi
[ "$1" = run ] || exit 1
printf '%s\n' "$*" > "$base/argv"
cat > "$base/request"
printf '{"access_token":"FAKE-answer","refresh_token":"FAKE-R2"}'
"#,
        );
        let rt = crate::container_floor::Runtime { cli: runtime };
        let _lock = mutation_lock(&dir).unwrap();
        let answer = live::post_json(
            &rt,
            &dir,
            "claude",
            "FAKE-ext",
            "https://FAKE.invalid/token",
            &json!({"refresh_token":"FAKE-R1"}),
        )
        .unwrap();
        assert_eq!(answer["access_token"], "FAKE-answer");
        let argv = std::fs::read_to_string(dir.join("argv")).unwrap();
        assert!(argv.contains("--log-driver none"));
        assert!(argv.contains("--data-binary @-"));
        assert!(!argv.contains("FAKE-R1"));
        assert_eq!(
            serde_json::from_slice::<Value>(&std::fs::read(dir.join("request")).unwrap()).unwrap(),
            json!({"refresh_token":"FAKE-R1"})
        );
        drop(_lock);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn partial_refresh_republishes_retained_generation_before_not_due() {
        let dir =
            std::env::temp_dir().join(format!("c4-refresh-{}", uuid::Uuid::new_v4().simple()));
        let record = serde_json::to_vec(&apply_answer("claude", &claude_record(),
            &json!({"access_token":"FAKE-new", "refresh_token":"FAKE-rotated", "expires_in":3600}), 2_000_000_000).unwrap()).unwrap();
        let files = crate::seat_login::extract_tokens(
            "claude",
            &[("claude/.credentials.json".into(), record.clone())],
        )
        .unwrap();
        let mut writes = Vec::new();
        let mut retained = Vec::new();
        let result = commit_refresh(
            &dir,
            "claude",
            "claude.credentials.json",
            &files,
            |name, bytes| {
                assert!(pending_path(&dir, "claude").exists());
                assert_eq!(
                    std::fs::read(pending_path(&dir, "claude")).unwrap(),
                    ROTATION_ISSUED
                );
                writes.push(name.to_string());
                if name.ends_with(".next") {
                    Ok(())
                } else if name == "claude.credentials.json" {
                    retained = bytes.to_vec();
                    Ok(())
                } else {
                    Err("injected token write failure".into())
                }
            },
            |_| Ok(()),
        );
        assert!(result.is_err());
        assert_eq!(
            writes,
            [
                "claude.credentials.json.next",
                "claude.credentials.json",
                "anthropic.token"
            ]
        );
        assert_eq!(retained, record);
        let expiry =
            crate::seat_login::record_expiry("claude", &serde_json::from_slice(&retained).unwrap())
                .unwrap();
        assert!(!due(expiry, 2_000_000_000));
        let mut served = Vec::new();
        repair_pending(
            &dir,
            "claude",
            Some(&retained),
            |name, bytes| {
                served.push((name.to_string(), bytes.to_vec()));
                Ok(())
            },
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(
            served,
            vec![
                ("claude.credentials.json".into(), record),
                ("anthropic.token".into(), b"FAKE-new".to_vec())
            ]
        );
        assert!(!pending_path(&dir, "claude").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn refresher_skips_a_broker_mutation_without_reading_a_record() {
        let dir =
            std::env::temp_dir().join(format!("c4-refresh-lock-{}", uuid::Uuid::new_v4().simple()));
        let lock = mutation_lock(&dir).unwrap();
        let rt = crate::container_floor::Runtime {
            cli: "/nonexistent-FAKE-runtime".into(),
        };
        let gw = crate::seat_gateway::Gateway::for_dir(&dir);
        assert_eq!(
            refresh(&rt, &gw, &dir, "claude", true).unwrap(),
            Outcome::Busy
        );
        drop(lock);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// core-ci-red: a child forked by another thread while the lock is held shares its open file
    /// description until its exec. The owner's drop must free the broker anyway; a bare close
    /// left the `flock` held, and a concurrent refresh read `Busy` (an `Ok`) for a free broker.
    #[cfg(unix)]
    #[test]
    fn a_dropped_mutation_lock_is_free_while_a_forked_child_shares_its_descriptor() {
        let dir =
            std::env::temp_dir().join(format!("c4-lock-share-{}", uuid::Uuid::new_v4().simple()));
        let lock = mutation_lock(&dir).unwrap();
        // What the forked child holds: a duplicate of the same open file description.
        let inherited = lock.0.try_clone().unwrap();
        assert!(try_mutation_lock(&dir).unwrap().is_none());
        drop(lock);
        assert!(try_mutation_lock(&dir).unwrap().is_some());
        drop(inherited);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rotated_record_survives_first_record_failure_and_repair_clears_last_failure() {
        let dir = std::env::temp_dir().join(format!(
            ".c4-refresh-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let record = serde_json::to_vec(
            &apply_answer(
                "claude",
                &claude_record(),
                &json!({"access_token":"FAKE-new", "refresh_token":"FAKE-R2", "expires_in":3600}),
                2_000_000_000,
            )
            .unwrap(),
        )
        .unwrap();
        let files = crate::seat_login::extract_tokens(
            "claude",
            &[("claude/.credentials.json".into(), record.clone())],
        )
        .unwrap();
        let mut broker = std::collections::BTreeMap::new();
        assert!(commit_refresh(
            &dir,
            "claude",
            "claude.credentials.json",
            &files,
            |name, bytes| {
                if name == "claude.credentials.json" {
                    return Err("injected first record failure".into());
                }
                broker.insert(name.to_string(), bytes.to_vec());
                Ok(())
            },
            |_| Ok(())
        )
        .is_err());
        assert_eq!(broker["claude.credentials.json.next"], record);
        record_last(
            &dir,
            "claude",
            Some(&LastRefresh {
                ok: false,
                at: 1,
                error: Some("local write failed".into()),
            }),
        );
        let staged = broker["claude.credentials.json.next"].clone();
        let mut removed = Vec::new();
        assert!(repair_pending(
            &dir,
            "claude",
            Some(&staged),
            |name, bytes| {
                broker.insert(name.to_string(), bytes.to_vec());
                Ok(())
            },
            |name| {
                removed.push(name.to_string());
                Ok(())
            }
        )
        .unwrap());
        assert_eq!(broker["claude.credentials.json"], record);
        assert_eq!(broker["anthropic.token"], b"FAKE-new");
        assert_eq!(removed, ["claude.credentials.json.next"]);
        assert!(last_refresh(&dir, "claude").unwrap().ok);
        assert!(!pending_path(&dir, "claude").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn refresh_answer_rejects_invalid_rotation_and_trims_tokens() {
        for cli in ["claude", "codex"] {
            let record = if cli == "claude" {
                claude_record()
            } else {
                codex_record()
            };
            for bad in ["", "   ", "FAKE token", "FAKE\nheader", "FAKE\0token"] {
                for field in ["access_token", "refresh_token"] {
                    let mut answer = json!({"access_token":"FAKE-valid", "expires_in":3600});
                    answer[field] = json!(bad);
                    assert!(apply_answer(cli, &record, &answer, 1).is_err());
                }
            }
            let answer = json!({"access_token":"  FAKE-access  ","refresh_token":" FAKE-refresh ","expires_in":3600});
            let new = apply_answer(cli, &record, &answer, 1).unwrap();
            let pointer = if cli == "claude" {
                "/claudeAiOauth/refreshToken"
            } else {
                "/tokens/refresh_token"
            };
            assert_eq!(new.pointer(pointer).unwrap(), "FAKE-refresh");
        }
    }

    #[test]
    fn refresh_request_is_bounded_before_runtime_spawn() {
        assert!(encode_request(&json!({"refresh_token":"FAKE-small"})).is_ok());
        assert!(encode_request(&json!({"refresh_token":"F".repeat(64 * 1024)})).is_err());
    }

    fn claude_record() -> Value {
        json!({"claudeAiOauth": {
            "accessToken": "FAKE-old-access", "refreshToken": "FAKE-old-refresh",
            "expiresAt": 1_000_000_000_000u64, "scopes": ["user:inference", "user:profile"],
            "subscriptionType": "max"
        }})
    }

    fn codex_record() -> Value {
        json!({"auth_mode": "chatgpt", "OPENAI_API_KEY": null, "tokens": {
            "id_token": "FAKE.id.token", "access_token": "FAKE.access.token",
            "refresh_token": "FAKE-old-refresh", "account_id": "acct-FAKE-1"
        }, "last_refresh": "2026-01-01T00:00:00Z"})
    }

    #[test]
    fn the_request_carries_the_refresh_token_client_and_scopes() {
        let f = flow("claude").unwrap();
        let b = request_body("claude", &claude_record(), &f).unwrap();
        assert_eq!(b["grant_type"], "refresh_token");
        assert_eq!(b["refresh_token"], "FAKE-old-refresh");
        assert_eq!(b["client_id"], f.client_id);
        assert_eq!(b["scope"], "user:inference user:profile");
        let f = flow("codex").unwrap();
        let b = request_body("codex", &codex_record(), &f).unwrap();
        assert_eq!(b["refresh_token"], "FAKE-old-refresh");
        assert_eq!(b["client_id"], "app_EMoamEEZ73f0CkXaXp7hrann");
        // Not refreshable: an API key, a record without a refresh token, another agent.
        assert!(request_body("codex", &json!({"auth_mode": "apikey"}), &f).is_none());
        assert!(request_body("claude", &json!({"claudeAiOauth": {}}), &f).is_none());
        assert!(flow("pi").is_none());
    }

    #[test]
    fn an_answer_is_folded_into_the_record_and_a_missing_refresh_token_keeps_the_old() {
        let now = 1_800_000_000;
        let new = apply_answer(
            "claude",
            &claude_record(),
            &json!({"access_token": "FAKE-new-access", "refresh_token": "FAKE-new-refresh", "expires_in": 3600}),
            now,
        )
        .unwrap();
        assert_eq!(new["claudeAiOauth"]["accessToken"], "FAKE-new-access");
        assert_eq!(new["claudeAiOauth"]["refreshToken"], "FAKE-new-refresh");
        assert_eq!(new["claudeAiOauth"]["expiresAt"], (now + 3600) * 1000);
        assert_eq!(new["claudeAiOauth"]["subscriptionType"], "max");
        let kept = apply_answer(
            "codex",
            &codex_record(),
            &json!({"access_token": "FAKE.new.access"}),
            now,
        )
        .unwrap();
        assert_eq!(kept["tokens"]["refresh_token"], "FAKE-old-refresh");
        assert_eq!(kept["tokens"]["access_token"], "FAKE.new.access");
        assert_eq!(kept["tokens"]["account_id"], "acct-FAKE-1");
        assert_eq!(kept["last_refresh"], "2027-01-15T08:00:00Z");
        assert!(apply_answer(
            "claude",
            &claude_record(),
            &json!({"error": "invalid_grant"}),
            now
        )
        .is_err());
        // The folded record is a sign-in the broker's extraction accepts.
        let files = crate::seat_login::extract_tokens(
            "claude",
            &[(
                "claude/.credentials.json".into(),
                serde_json::to_vec(&new).unwrap(),
            )],
        )
        .unwrap();
        assert!(files
            .iter()
            .any(|(n, b)| n == "anthropic.token" && b == b"FAKE-new-access"));
    }

    #[test]
    fn only_a_credential_inside_the_margin_is_due() {
        let now = 1_000_000;
        assert!(due(Some(now + 60), now));
        assert!(due(Some(now - 1), now));
        assert!(!due(Some(now + REFRESH_MARGIN_SECS + 1), now));
        assert!(!due(None, now));
    }

    #[test]
    fn the_last_refresh_is_kept_without_a_credential() {
        let dir = std::env::temp_dir().join(format!("wicked-refresh-{}", uuid::Uuid::new_v4()));
        assert_eq!(last_refresh(&dir, "claude"), None);
        let failed = LastRefresh {
            ok: false,
            at: 5,
            error: Some("the token endpoint refused the refresh (curl exit 22)".into()),
        };
        record_last(&dir, "claude", Some(&failed));
        assert_eq!(last_refresh(&dir, "claude"), Some(failed));
        record_last(&dir, "claude", None);
        assert_eq!(last_refresh(&dir, "claude"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
