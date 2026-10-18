//! The broker's ONE refresher (DES-CONTAINER-FLOOR §6, item 2): the only place a signed-in
//! agent's sign-in is renewed. Seats never refresh (their placeholder expiry is far in the
//! future), so parallel seats never race a refresh.
//!
//! **The broker renews nothing itself (core#1024).** It makes no token request and never
//! presents itself as any vendor's client. A sign-in is renewed by the agent's own CLI, the way
//! that CLI renews itself, run on this host in a throwaway home under the daemon's gateway
//! directory (the isolation of its host sign-in, core#986: an `env -i` environment whose HOME,
//! XDG directories and CLI home all point inside it). The broker writes the CLI's own credential
//! record there, runs one command of the CLI's that makes it renew its own credential, reads
//! the CLI's store back, and deletes the home:
//!
//! - **codex**: `codex app-server` (its own JSON-RPC server) asked `account/read` with
//!   `refreshToken: true`, its documented proactive refresh; `auth.json` is then read back.
//! - **pi**: `pi auth print-bearer-token --provider <id> --min-expiry <m>`, its own request-auth
//!   path, which renews and persists an OAuth entry with less validity left (the printed token
//!   goes to /dev/null). One entry per run, each committed before the next (decision 8).
//! - **opencode**: `opencode auth import` of its own export, `opencode models` (loading its
//!   providers resolves the active ChatGPT sign-in, which opencode renews within five minutes of
//!   its expiry), then `opencode auth export`. All three on a private `--standalone` server.
//! - **claude** seats pass the operator's own long-lived `claude setup-token` token through
//!   (the gateway relays the client's Authorization unchanged), so there is nothing to renew. A
//!   record from an earlier `claude auth login` sign-in is never renewed: when it falls due, the
//!   refresh fails with a plain reason, and the operator signs in again.
//! - **copilot**: its GitHub token does not expire; the pass re-reads the account's Copilot API
//!   host and models with it ([`copilot_account_request`]).
//!
//! A CLI that cannot renew (not installed, a refused refresh, an exit without a new record) is a
//! failed refresh recorded with its reason (never the CLI's output, which can hold a token); the
//! credential is served until it expires, and then the seat is signed out until the operator
//! signs in again.
//!
//! **Glue.** The CLI's new record goes through the same extraction as a sign-in
//! ([`crate::seat_login::extract_tokens`]). agentgateway watches its `backendAuth` file, so the
//! new token is served without a restart. The record is written before the token file: a CLI
//! that rotates its refresh token must never leave the broker holding only the old one.
//!
//! **One per gateway.** A pass holds an exclusive lock file in the daemon's gateway directory
//! (a second pass skips); the daemon starts one background loop per gateway dir.

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
/// The pinned OSS image the copilot account reads run in (curl verifies TLS against its CA
/// bundle).
pub(crate) const REFRESH_IMAGE: &str = "curlimages/curl:8.11.1";
/// The agents whose sign-in the broker's pass keeps current: an OAuth record their own CLI
/// renews, a claude record it reports on, or (copilot) the account's Copilot API host and
/// models it re-reads with the GitHub token.
pub(crate) const REFRESHABLE: &[&str] = &["claude", "codex", "pi", "opencode", "copilot"];

/// Why a claude record is never renewed: its seats pass a `claude setup-token` token through.
pub(crate) const CLAUDE_SIGN_IN_AGAIN: &str = "claude seats now sign in with a long-lived token \
     from `claude setup-token`, which the broker does not renew; sign claude in again";

/// Whether `cli`'s own CLI renews its sign-in on this host (the agents that sign in there).
pub(crate) fn renews_on_host(cli: &str) -> bool {
    matches!(cli, "codex" | "pi" | "opencode")
}

/// (tests) The directory a stub of each renewing CLI is found in, instead of the daemon's PATH.
#[cfg(test)]
pub(crate) static TEST_CLI_DIR: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// (tests) The stub origin serving both copilot account reads (`/copilot_internal/user`,
/// `/models`). No production override: the GitHub token only goes to GitHub and the account's
/// own validated Copilot API host.
#[cfg(test)]
pub(crate) static COPILOT_TEST_ORIGIN: std::sync::Mutex<Option<String>> =
    std::sync::Mutex::new(None);

/// One account read the copilot pass makes: a GET authorized by the GitHub token (sent on stdin
/// as a header, never on a command line). It carries no client identity of ours: curl's own
/// User-Agent, the accept/API-version headers GitHub documents, and, on the model listing, the
/// copilot CLI's own integration id ([`crate::copilot_account::MODELS_HEADERS`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Request {
    pub(crate) url: String,
    pub(crate) token: String,
    pub(crate) headers: Vec<(&'static str, &'static str)>,
}

/// The two reads that resolve a copilot sign-in's account (DES-SEAT-LOGIN-SUBSCRIPTIONS OD-1),
/// as the copilot CLI makes them: GitHub names the account's Copilot API, which lists the
/// models. `host` = `None` builds the first; `Some(host)` the second, at that validated host.
pub(crate) fn copilot_account_request(token: &str, host: Option<&str>) -> Request {
    use crate::copilot_account::{MODELS_HEADERS, USER_HEADERS, USER_URL};
    #[cfg(test)]
    let origin = COPILOT_TEST_ORIGIN.lock().ok().and_then(|o| o.clone());
    #[cfg(not(test))]
    let origin: Option<String> = None;
    let (url, headers) = match (host, origin) {
        (None, Some(o)) => (format!("{o}/copilot_internal/user"), USER_HEADERS),
        (None, None) => (USER_URL.to_string(), USER_HEADERS),
        (Some(_), Some(o)) => (format!("{o}/models"), MODELS_HEADERS),
        (Some(h), None) => (format!("https://{h}/models"), MODELS_HEADERS),
    };
    Request {
        url,
        token: token.to_string(),
        headers: headers.to_vec(),
    }
}

/// The broker file that retains `cli`'s whole OAuth record (claude's only from an earlier
/// `claude auth login` sign-in).
pub(crate) fn record_file(cli: &str) -> Option<&'static str> {
    match cli {
        "claude" => Some("claude.credentials.json"),
        "codex" => Some("codex.auth.json"),
        "pi" => Some("pi.auth.json"),
        "opencode" => Some("opencode.credentials.json"),
        _ => None,
    }
}

/// The login-volume spelling of the record, for [`crate::seat_login::extract_tokens`].
pub(crate) fn login_path(cli: &str) -> Option<&'static str> {
    match cli {
        "claude" => Some("claude/.credentials.json"),
        "codex" => Some("codex/auth.json"),
        "pi" => Some("pi/auth.json"),
        "opencode" => Some("data/opencode/wicked-auth.json"),
        _ => None,
    }
}

/// Agents whose record holds several sign-ins, each renewed on its own (an entry at a time).
fn multi_entry(cli: &str) -> bool {
    matches!(cli, "pi" | "opencode")
}

/// pi's renewable entries, in no particular order: the provider ids its own CLI renews.
const PI_ENTRIES: &[&str] = &["anthropic", "openai-codex", "github-copilot"];

/// Whether `cli`'s single record (claude, codex) is a renewable OAuth sign-in: one with a
/// refresh token, not an API key.
pub(crate) fn renewable(cli: &str, record: &Value) -> bool {
    match cli {
        "claude" => str_at(record, "/claudeAiOauth/refreshToken").is_some(),
        "codex" => {
            record.get("auth_mode").and_then(Value::as_str) != Some("apikey")
                && str_at(record, "/tokens/refresh_token").is_some()
        }
        _ => false,
    }
}

/// Whether `cli`'s single record is due at `now` (or, with `force`, renewable at all).
pub(crate) fn record_due(cli: &str, record: &Value, now: u64, force: bool) -> bool {
    renewable(cli, record)
        && (force || crate::seat_login::record_expiry(cli, record).is_ok_and(|e| due(e, now)))
}

/// The entries of a multi-entry record that a pass renews at `now`, soonest-expiring first:
/// those due (or, with `force`, every renewable one). Each is tried once per pass.
pub(crate) fn due_entries(cli: &str, record: &Value, now: u64, force: bool) -> Vec<String> {
    // opencode's only renewable sign-in is its ChatGPT one; Copilot's GitHub token does not
    // expire.
    let entries: &[&str] = match cli {
        "pi" => PI_ENTRIES,
        "opencode" => &["openai"],
        _ => return Vec::new(),
    };
    let mut candidates: Vec<(u64, &str)> = entries
        .iter()
        .filter_map(|id| {
            let entry = crate::seat_login::record_entry(cli, record, id)?;
            (entry.get("type").and_then(Value::as_str) == Some("oauth")).then_some(())?;
            if cli == "opencode"
                && !matches!(
                    entry.get("methodID").and_then(Value::as_str),
                    Some("chatgpt-browser" | "chatgpt-headless")
                )
            {
                return None;
            }
            str_at(entry, "/refresh")?;
            // An entry with no expiry is renewable but never due on its own: only `force` takes
            // it (last).
            Some((entry_expiry(entry).unwrap_or(u64::MAX), *id))
        })
        .filter(|(exp, _)| force || (*exp != u64::MAX && due(Some(*exp), now)))
        .collect();
    candidates.sort();
    candidates
        .into_iter()
        .map(|(_, id)| id.to_string())
        .collect()
}

/// A pi or opencode entry's expiry in Unix seconds (both store milliseconds; `0` = none).
fn entry_expiry(entry: &Value) -> Option<u64> {
    entry
        .get("expires")
        .and_then(Value::as_u64)
        .filter(|ms| *ms > 0)
        .map(|ms| ms / 1000)
}

fn str_at<'a>(v: &'a Value, p: &str) -> Option<&'a str> {
    crate::seat_login::nonempty(v.pointer(p))
}

/// What one run of a CLI's own renewal did, as the broker judges it from the CLI's store.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Renewal {
    /// The CLI stored a new record (renewed, or at least rotated): the broker takes it.
    Renewed(Value),
    /// The CLI left its record as it was and the credential is still valid: by the CLI's own
    /// rule it was not due yet (opencode renews within five minutes of expiry).
    Unchanged,
}

/// Judge one run of `cli`'s own renewal (pure): `old` is the record the broker handed it,
/// `stored` what the CLI's store holds afterwards, `ran` whether the CLI's command succeeded
/// (`Err` = how it failed, never its output), `entry` the entry it was asked to renew. A store
/// the CLI changed is taken even when its command then failed: a rotated refresh token must
/// never be dropped. Errors name the failure only.
pub(crate) fn judge(
    cli: &str,
    entry: Option<&str>,
    old: &Value,
    stored: Option<&[u8]>,
    ran: Result<(), String>,
    now: u64,
) -> Result<Renewal, String> {
    let stored = stored.ok_or_else(|| format!("{cli} left no sign-in in its store"))?;
    let new: Value = serde_json::from_slice(stored)
        .map_err(|_| format!("{cli} left an unreadable sign-in in its store"))?;
    if &new != old {
        crate::seat_login::extract_tokens(
            cli,
            &[(login_path(cli).unwrap_or_default().into(), stored.to_vec())],
        )
        .map_err(|e| format!("{cli}'s renewed sign-in was refused: {e}"))?;
        return Ok(Renewal::Renewed(new));
    }
    ran.map_err(|e| format!("{cli} could not renew its sign-in ({e})"))?;
    let expiry = match entry {
        Some(id) => crate::seat_login::record_entry(cli, &new, id).and_then(entry_expiry),
        None => crate::seat_login::record_expiry(cli, &new).ok().flatten(),
    };
    if expiry.is_some_and(|e| e <= now) {
        return Err(format!("{cli} did not renew its expired sign-in"));
    }
    Ok(Renewal::Unchanged)
}

/// pi's `--min-expiry`: renew an entry with less than this much validity left. A forced
/// renewal asks for more validity than any token has, so pi always renews.
pub(crate) fn pi_min_expiry(force: bool) -> String {
    if force {
        "100000h".into()
    } else {
        format!("{}m", REFRESH_MARGIN_SECS / 60 + 1)
    }
}

/// The JSON-RPC lines that make `codex app-server` renew its own sign-in: `initialize` (naming
/// this engine as the client, as the protocol asks), `initialized`, then `account/read` with
/// `refreshToken: true` (id 2), codex's proactive refresh.
pub(crate) fn codex_requests() -> Vec<u8> {
    let lines = [
        json!({"id": 1, "method": "initialize", "params": {"clientInfo": {
            "name": "wicked_core", "title": "wicked-core broker",
            "version": env!("CARGO_PKG_VERSION")}}}),
        json!({"method": "initialized"}),
        json!({"id": 2, "method": "account/read", "params": {"refreshToken": true}}),
    ];
    lines
        .iter()
        .flat_map(|l| format!("{l}\n").into_bytes())
        .collect()
}

/// Whether one line codex's server wrote answers request id 2: `Some(true)` a result,
/// `Some(false)` an error, `None` another message.
pub(crate) fn codex_answer(line: &[u8]) -> Option<bool> {
    let v: Value = serde_json::from_slice(line).ok()?;
    (v.get("id") == Some(&json!(2))).then(|| v.get("result").is_some())
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

/// curl's `-w` format: the answer's HTTP status on a last line of its own (curl expands the `\n`).
const STATUS_FORMAT: &str = "\\n%{http_code}";

/// Split curl's output into the answer's body and the HTTP status [`STATUS_FORMAT`] appended:
/// a last line of exactly three digits after a newline. Output without one is all body, the
/// status unknown (`None`).
fn split_status(out: &[u8]) -> (&[u8], Option<u16>) {
    match out.iter().rposition(|b| *b == b'\n') {
        Some(nl) if out.len() - nl == 4 && out[nl + 1..].iter().all(u8::is_ascii_digit) => {
            let code = std::str::from_utf8(&out[nl + 1..])
                .ok()
                .and_then(|c| c.parse().ok());
            (&out[..nl], code)
        }
        _ => (out, None),
    }
}

/// The error a refused request reports (core#1011): its HTTP status and, when the body is an
/// OAuth or API error, the error's CODE (`invalid_grant`, `rate_limit_error`), never the body
/// itself, its description or anything a credential could be in.
fn refusal(status: u16, body: &[u8]) -> String {
    let code = serde_json::from_slice::<Value>(body).ok().and_then(|v| {
        let code = match v.get("error") {
            Some(Value::String(c)) => c.clone(),
            Some(e) => e.get("type")?.as_str()?.to_string(),
            None => return None,
        };
        // A code is a short token; anything else (free text) is not repeated.
        let token = !code.is_empty()
            && code.len() <= 64
            && code
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
        token.then_some(code)
    });
    match code {
        Some(code) => format!("the endpoint answered HTTP {status} ({code})"),
        None => format!("the endpoint answered HTTP {status}"),
    }
}

#[cfg(unix)]
mod live {
    use super::*;
    use crate::container_floor::{output_within, Runtime};
    use crate::seat_gateway::Gateway;
    use std::time::{Duration, Instant};
    use wicked_apps_core::HardenedCommand;

    const POST_TIMEOUT: Duration = Duration::from_secs(45);
    /// The largest account answer read back.
    const MAX_ANSWER: usize = 256 * 1024;
    /// How long one command of a CLI's own renewal may run.
    const CLI_STEP_TIMEOUT: Duration = Duration::from_secs(90);
    /// The largest store a CLI's export may print.
    const MAX_STORE: usize = 1024 * 1024;

    /// The curl argv and stdin of an account read (pure): its credential header rides stdin, so
    /// no credential is ever a command-line argument. The answer's HTTP status follows its body
    /// on a last line of its own ([`STATUS_FORMAT`], read back by [`split_status`]); no
    /// `--fail`, so a refusal names its status, not "curl exit 22". No `-A`: curl's own
    /// User-Agent, never a vendor client's.
    pub(crate) fn request_argv(request: &Request) -> Result<(Vec<String>, Vec<u8>), String> {
        let mut argv: Vec<String> = ["-sS", "--max-time", "30", "-w", STATUS_FORMAT]
            .into_iter()
            .map(str::to_string)
            .collect();
        if request
            .token
            .chars()
            .any(|c| c.is_control() || c.is_whitespace())
        {
            return Err("invalid credential token".into());
        }
        for (name, value) in &request.headers {
            argv.extend(["-H".to_string(), format!("{name}: {value}")]);
        }
        argv.extend(["-H".to_string(), "@-".to_string(), request.url.clone()]);
        Ok((
            argv,
            format!("authorization: Bearer {}\n", request.token).into_bytes(),
        ))
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
                        "could not pull the account reader image {REFRESH_IMAGE}"
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
        let (out, status) = split_status(&out);
        if let Some(code) = status.filter(|c| !(200..300).contains(c)) {
            return Err(refusal(code, out));
        }
        if out.len() > MAX_ANSWER {
            return Err("the account answer exceeds its size limit".into());
        }
        serde_json::from_slice(out).map_err(|_| "the account answer is not JSON".into())
    }

    /// The host executable of `cli`'s renewing CLI, on the daemon's search path.
    fn renewing_exe(cli: &str, daemon_env: &[(String, String)]) -> Result<PathBuf, String> {
        #[cfg(test)]
        if let Some(d) = TEST_CLI_DIR.lock().ok().and_then(|d| d.clone()) {
            return Ok(d.join(cli));
        }
        let path = daemon_env
            .iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| std::ffi::OsStr::new(v.as_str()));
        crate::seat_login::find_on_path(cli, path).ok_or_else(|| {
            format!(
                "{cli} is not installed on this host: its sign-in is renewed by this host's own \
                 {cli}. Install it, or sign {cli} in again when the sign-in expires"
            )
        })
    }

    /// Run one command of `exe` in throwaway `home` with `env` (and nothing else): `stdin` is
    /// written then closed; stdout is kept only when `keep_stdout` (bounded), else discarded,
    /// and stderr always is (either can hold a credential). The command leads its own process
    /// group, which is ended afterwards, so a server it started never outlives it. `Err` names
    /// how it failed (an exit status or a timeout), never what it printed.
    fn run_step(
        exe: &Path,
        args: &[String],
        home: &Path,
        env: &[(String, String)],
        stdin: &[u8],
        keep_stdout: bool,
    ) -> (Result<(), String>, Vec<u8>) {
        use std::io::{Read, Write};
        use std::os::unix::process::CommandExt;
        use std::process::Stdio;
        let mut cmd = std::process::Command::new(exe);
        cmd.hardened();
        cmd.args(args)
            .env_clear()
            .envs(env.iter().map(|(k, v)| (k, v)))
            .current_dir(home)
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(if keep_stdout {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stderr(Stdio::null());
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(_) => return (Err("it could not start".into()), Vec::new()),
        };
        let pgid = child.id() as i32;
        if let Some(mut input) = child.stdin.take() {
            let bytes = stdin.to_vec();
            std::thread::spawn(move || {
                let _ = input.write_all(&bytes);
            });
        }
        let reader = child.stdout.take().map(|out| {
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                let _ = out.take(MAX_STORE as u64 + 1).read_to_end(&mut bytes);
                bytes
            })
        });
        let start = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if start.elapsed() < CLI_STEP_TIMEOUT => {
                    std::thread::sleep(Duration::from_millis(50))
                }
                _ => break None,
            }
        };
        // SAFETY: killpg takes plain integers; the group is the one this child leads.
        unsafe { libc::killpg(pgid, libc::SIGKILL) };
        if status.is_none() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let out = reader.and_then(|r| r.join().ok()).unwrap_or_default();
        let ran = match status {
            None => Err("it timed out".into()),
            Some(s) if s.success() => Ok(()),
            Some(s) => Err(format!("it exited with {s}")),
        };
        if out.len() > MAX_STORE {
            return (Err("its output exceeds the size limit".into()), Vec::new());
        }
        (ran, out)
    }

    /// `codex app-server`: send [`codex_requests`], wait for the answer to request 2 (or the
    /// step timeout), then close its stdin so it exits. `Ok` = codex answered with a result.
    fn run_codex(exe: &Path, home: &Path, env: &[(String, String)]) -> Result<(), String> {
        use std::io::{BufRead, Write};
        use std::os::unix::process::CommandExt;
        use std::process::Stdio;
        let mut cmd = std::process::Command::new(exe);
        cmd.hardened();
        cmd.args(["app-server", "-c", "cli_auth_credentials_store=\"file\""])
            .env_clear()
            .envs(env.iter().map(|(k, v)| (k, v)))
            .current_dir(home)
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = cmd.spawn().map_err(|_| "it could not start".to_string())?;
        let pgid = child.id() as i32;
        let mut input = child.stdin.take();
        let (tx, rx) = std::sync::mpsc::channel();
        if let Some(out) = child.stdout.take() {
            std::thread::spawn(move || {
                let mut out = std::io::BufReader::new(out);
                let mut line = Vec::new();
                // Each line is judged and dropped: an answer can carry a token.
                while out
                    .read_until(b'\n', &mut line)
                    .is_ok_and(|n| n > 0 && line.len() <= MAX_STORE)
                {
                    if let Some(ok) = codex_answer(&line) {
                        let _ = tx.send(ok);
                        return;
                    }
                    line.clear();
                }
            });
        }
        let sent = input
            .as_mut()
            .map(|i| i.write_all(&codex_requests()).and_then(|()| i.flush()));
        let answer = match sent {
            Some(Ok(())) => rx.recv_timeout(CLI_STEP_TIMEOUT).ok(),
            _ => None,
        };
        // Closing stdin ends the server; give it a moment to finish writing its store.
        drop(input);
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            if let Ok(Some(_)) = child.try_wait() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        // SAFETY: killpg takes plain integers; the group is the one this child leads.
        unsafe { libc::killpg(pgid, libc::SIGKILL) };
        let _ = child.kill();
        let _ = child.wait();
        match answer {
            Some(true) => Ok(()),
            Some(false) => Err("its account/read answered an error".into()),
            None => Err("its account/read did not answer".into()),
        }
    }

    /// Run `cli`'s own renewal of `record` (its `entry`, for pi and opencode) on this host, in a
    /// throwaway home that is removed afterwards whatever happened, and judge what the CLI
    /// stored ([`judge`]). The record is written into the CLI's own store (0600 under 0700
    /// directories) and read back from it; it never reaches an argument, the environment or a
    /// log.
    pub(crate) fn renew_on_host(
        dir: &Path,
        gw: &Gateway,
        cli: &str,
        record: &Value,
        entry: Option<&str>,
        force: bool,
        now: u64,
    ) -> Result<Renewal, String> {
        let daemon_env: Vec<(String, String)> = std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
            .collect();
        let exe = renewing_exe(cli, &daemon_env)?;
        let bytes = serde_json::to_vec(record).map_err(|_| "could not encode the record")?;
        let session = crate::seat_login::refresh_session(&gw.name, cli);
        let home = crate::seat_login::create_refresh_home(dir, cli, &session)?;
        let env = crate::seat_login::host_cli_env(&home, cli, &daemon_env);
        let result = (|| {
            let (ran, stored) = match cli {
                "codex" => {
                    crate::seat_login::write_private(&home.join("codex/auth.json"), &bytes)?;
                    let ran = run_codex(&exe, &home, &env);
                    (
                        ran,
                        crate::seat_login::read_home_file(&home, "codex/auth.json")?,
                    )
                }
                "pi" => {
                    crate::seat_login::write_private(&home.join("pi/auth.json"), &bytes)?;
                    let id = entry.ok_or("pi renews one entry at a time")?;
                    let args: Vec<String> = [
                        "auth",
                        "print-bearer-token",
                        "--provider",
                        id,
                        "--min-expiry",
                        &pi_min_expiry(force),
                    ]
                    .into_iter()
                    .map(str::to_string)
                    .collect();
                    let (ran, _) = run_step(&exe, &args, &home, &env, &[], false);
                    (
                        ran,
                        crate::seat_login::read_home_file(&home, "pi/auth.json")?,
                    )
                }
                "opencode" => {
                    let step = |args: &[&str], stdin: &[u8], keep: bool| {
                        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
                        run_step(&exe, &args, &home, &env, stdin, keep)
                    };
                    step(&["auth", "import", "--standalone"], &bytes, false)
                        .0
                        .map_err(|e| format!("opencode could not take its sign-in ({e})"))?;
                    let (ran, _) = step(&["models", "--standalone"], &[], false);
                    let (exported, out) = step(&["auth", "export", "--standalone"], &[], true);
                    exported.map_err(|e| format!("opencode could not export its sign-in ({e})"))?;
                    (ran, Some(out))
                }
                _ => return Err(format!("{cli} has no renewal of its own on this host")),
            };
            judge(cli, entry, record, stored.as_deref(), ran, now)
        })();
        crate::seat_login::remove_refresh_home(dir, &session)?;
        result
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
        if cli == "copilot" {
            return copilot_pass(rt, gw, dir, force);
        }
        let Some(file) = record_file(cli) else {
            return Ok(Outcome::NotRefreshable);
        };
        crate::seat_login::repair_ingest(rt, gw, dir, cli)?;
        if crate::seat_login::signin_pending(dir, cli) {
            return Err("sign-in incomplete; retry ingest first".into());
        }
        // A crash can leave a renewal's throwaway home (a copy of the credential): gone first.
        crate::seat_login::sweep_refresh_homes(dir, gw, cli)?;
        // Re-read only after acquiring the broker lock (codex lg-r1 #2).
        let bytes = repair_refresh(rt, gw, dir, cli)?;
        let Some(bytes) = bytes else {
            return Ok(Outcome::NotRefreshable);
        };
        let mut record: Value =
            serde_json::from_slice(&bytes).map_err(|_| format!("{cli}: unreadable record"))?;
        let now = unix_now();
        let multi = multi_entry(cli);
        // A multi-entry record renews each eligible entry once per pass, one committed generation
        // at a time, so a rotated refresh token is stored before the next entry is renewed
        // (DES-SEAT-LOGIN-SUBSCRIPTIONS decision 8). One entry's failed renewal never strands
        // the others; a failure to store a generation stops the pass.
        let plan: Vec<Option<String>> = if multi {
            due_entries(cli, &record, now, force)
                .into_iter()
                .map(Some)
                .collect()
        } else if record_due(cli, &record, now, force) {
            vec![None]
        } else {
            Vec::new()
        };
        let refreshable = if multi {
            !due_entries(cli, &record, now, true).is_empty()
        } else {
            renewable(cli, &record)
        };
        let mut refreshed = false;
        let mut failures: Vec<String> = Vec::new();
        for entry in &plan {
            let renewed = if renews_on_host(cli) {
                renew_on_host(dir, gw, cli, &record, entry.as_deref(), force, now)
            } else {
                // claude: an earlier `claude auth login` record, never renewed (its seats now
                // pass a `claude setup-token` token through).
                Err(CLAUDE_SIGN_IN_AGAIN.to_string())
            };
            let new = match renewed {
                Ok(Renewal::Renewed(new)) => new,
                Ok(Renewal::Unchanged) => continue,
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

    /// copilot's pass (caller holds the mutation lock): re-read the account when it was never
    /// read, or was read [`RESOLVE_EVERY_SECS`](crate::copilot_account::RESOLVE_EVERY_SECS) ago,
    /// or with `force`. A failed read keeps what was published before and is shown on the tab.
    fn copilot_pass(
        rt: &Runtime,
        gw: &Gateway,
        dir: &Path,
        force: bool,
    ) -> Result<Outcome, String> {
        let cli = "copilot";
        crate::seat_login::repair_ingest(rt, gw, dir, cli)?;
        if crate::seat_login::signin_pending(dir, cli) {
            return Err("sign-in incomplete; retry ingest first".into());
        }
        let now = unix_now();
        let fresh = crate::seat_gateway::copilot_account(dir).is_some_and(|a| {
            a.resolved_at <= now
                && now
                    < a.resolved_at
                        .saturating_add(crate::copilot_account::RESOLVE_EVERY_SECS)
        });
        // An account whose publication did not complete is read and published again, however
        // recent its `resolved_at` (codex review: the literal is persisted before the gateway
        // has it).
        if fresh && !force && !literals_pending_path(dir, cli).exists() {
            return Ok(Outcome::NotDue { expires_at: None });
        }
        match resolve_copilot(rt, gw, dir, now) {
            Ok(true) => {
                record_last(
                    dir,
                    cli,
                    Some(&LastRefresh {
                        ok: true,
                        at: now,
                        error: None,
                    }),
                );
                Ok(Outcome::Refreshed { expires_at: None })
            }
            Ok(false) => Ok(Outcome::NotRefreshable),
            Err(e) => {
                let e = format!("reading the Copilot account: {e}");
                record_last(
                    dir,
                    cli,
                    Some(&LastRefresh {
                        ok: false,
                        at: now,
                        error: Some(e.clone()),
                    }),
                );
                Err(e)
            }
        }
    }

    /// Read the copilot sign-in's account with the brokered GitHub token, keep it beside the
    /// token and publish it on the route (caller holds the mutation lock). `Ok(false)` = signed
    /// out. Errors name the step and status only, never an answer or the token.
    pub(crate) fn resolve_copilot(
        rt: &Runtime,
        gw: &Gateway,
        dir: &Path,
        now: u64,
    ) -> Result<bool, String> {
        let cli = "copilot";
        let Some(token) =
            crate::seat_login::read_broker_record(rt, gw, crate::seat_gateway::COPILOT_CLI_FILE)?
        else {
            return Ok(false);
        };
        let token = String::from_utf8(token).map_err(|_| "invalid credential token")?;
        let token = token.trim();
        let user = send(
            rt,
            dir,
            cli,
            &gw.external_network,
            &copilot_account_request(token, None),
        )
        .map_err(|e| format!("copilot_internal/user: {e}"))?;
        let host = crate::copilot_account::api_host(&user)?;
        let list = send(
            rt,
            dir,
            cli,
            &gw.external_network,
            &copilot_account_request(token, Some(&host)),
        )
        .map_err(|e| format!("{host}/models: {e}"))?;
        let account = crate::copilot_account::account_models(&list, now)?;
        let files = crate::copilot_account::account_files(&host, &account)?;
        for (name, bytes) in &files {
            crate::seat_gateway::set_broker_credential(rt, gw, dir, cli, name, bytes)?;
        }
        let literals = crate::seat_login::route_literals(cli, &files)?;
        // Journal the publication: its marker outlives a failed or interrupted publish, so the
        // next pass publishes again instead of trusting the persisted `resolved_at`.
        let marker = literals_pending_path(dir, cli);
        if !marker.exists() {
            create_private_file(&marker, b"route literals unpublished")?;
        }
        crate::seat_gateway::route_sign_in(rt, gw, dir, cli, &literals)?;
        remove_private_file(&marker)?;
        Ok(true)
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

#[cfg(unix)]
pub(crate) use live::{refresh, resolve_copilot, start};
#[cfg(all(unix, test))]
pub(crate) use live::{renew_on_host, request_argv};

/// (tests) FAKE renewing CLIs: shell stubs of codex, pi and opencode that renew the way the real
/// ones do (in their own store, in the throwaway home they are run in) by rewriting each `from`
/// of `renames` to its `to`. Each appends its argv and the NAMES of its environment to
/// `<gateway dir>/stub-cli.log`; a `<gateway dir>/refuse-<cli>` file makes it fail without
/// touching its store.
#[cfg(all(unix, test))]
pub(crate) fn install_stub_clis(bin: &Path, renames: &[(&str, &str)]) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(bin).unwrap();
    let sed: String = renames
        .iter()
        .map(|(from, to)| format!(" -e 's|{from}|{to}|g'"))
        .collect();
    let common = format!(
        "#!/bin/sh\nd=\"$HOME/../..\"\n\
         printf '%s %s | %s\\n' \"${{0##*/}}\" \"$*\" \"$(env | cut -d= -f1 | sort | tr '\\n' ' ')\" >> \"$d/stub-cli.log\"\n\
         renew() {{ sed{sed} \"$1\" > \"$1.new\" && mv \"$1.new\" \"$1\"; }}\n"
    );
    let codex = format!(
        "{common}while IFS= read -r line; do case \"$line\" in *'\"id\":2'*)\n\
         if [ -f \"$d/refuse-codex\" ]; then echo '{{\"id\":2,\"error\":{{\"code\":-32603,\"message\":\"FAKE refused\"}}}}';\n\
         else renew \"$CODEX_HOME/auth.json\" && echo '{{\"id\":2,\"result\":{{\"account\":null}}}}'; fi;; esac; done\n"
    );
    let pi = format!(
        "{common}[ -f \"$d/refuse-pi\" ] && exit 1\n\
         [ \"$1 $2 $3\" = 'auth print-bearer-token --provider' ] || exit 2\n\
         renew \"$PI_CODING_AGENT_DIR/auth.json\" && echo FAKE-printed-token\n"
    );
    let opencode = format!(
        "{common}s=\"$XDG_DATA_HOME/stub-store.json\"\n\
         case \"$1 $2\" in\n\
         'auth import') cat > \"$s\";;\n\
         'models --standalone') [ -f \"$d/refuse-opencode\" ] && exit 1; renew \"$s\";;\n\
         'auth export') cat \"$s\";;\n\
         *) exit 2;; esac\n"
    );
    for (name, body) in [("codex", codex), ("pi", pi), ("opencode", opencode)] {
        let p = bin.join(name);
        std::fs::write(&p, body).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

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

    fn claude_record(access: &str, refresh: &str, expires_ms: u64) -> Value {
        json!({"claudeAiOauth": {
            "accessToken": access, "refreshToken": refresh,
            "expiresAt": expires_ms, "scopes": ["user:inference", "user:profile"],
            "subscriptionType": "max"
        }})
    }

    fn codex_record() -> Value {
        json!({"auth_mode": "chatgpt", "OPENAI_API_KEY": null, "tokens": {
            "id_token": "FAKE.id.token", "access_token": "FAKE.access.token",
            "refresh_token": "FAKE-old-refresh", "account_id": "acct-FAKE-1"
        }, "last_refresh": "2026-01-01T00:00:00Z"})
    }

    /// Decision 8: pi plans its due entries soonest-expiring first, each renewed on its own; a
    /// forced pass takes every renewable entry, a record without refresh tokens none.
    #[test]
    fn pi_plans_its_due_entries_soonest_first() {
        let record = pi_record();
        assert!(
            due_entries("pi", &record, 100, false).is_empty(),
            "nothing due"
        );
        assert_eq!(
            due_entries("pi", &record, 1_000_000_000, false),
            vec!["openai-codex"]
        );
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
    }

    /// opencode renews only its ChatGPT sign-in; its Copilot GitHub token does not expire.
    #[test]
    fn opencode_plans_only_its_chatgpt_sign_in() {
        let export = json!([
            {"id": "old", "integrationID": "openai", "label": "OAuth", "active": false,
                "value": {"type": "oauth", "methodID": "chatgpt-headless", "refresh": "FAKE-old-r",
                    "access": "FAKE-old-a", "expires": 1_000_u64}},
            {"id": "cur", "integrationID": "openai", "label": "OAuth", "active": true,
                "value": {"type": "oauth", "methodID": "chatgpt-headless", "refresh": "FAKE-r",
                    "access": "FAKE-a", "expires": 2_000_000_000_000_u64}},
            {"id": "gh", "integrationID": "github-copilot", "label": "OAuth", "active": true,
                "value": {"type": "oauth", "methodID": "device", "refresh": "FAKE-gho",
                    "access": "FAKE-gho", "expires": 0}}
        ]);
        assert!(due_entries("opencode", &export, 0, false).is_empty());
        assert_eq!(due_entries("opencode", &export, 0, true), vec!["openai"]);
        assert_eq!(
            due_entries("opencode", &export, 2_000_000_000, false),
            vec!["openai"],
            "the active row decides"
        );
    }

    /// claude and codex: a single record is due inside the margin, renewable only as an OAuth
    /// sign-in with a refresh token.
    #[test]
    fn a_single_record_is_due_only_as_a_renewable_oauth_sign_in() {
        let claude = claude_record("FAKE-a", "FAKE-r", 1_000_000_000_000);
        assert!(record_due("claude", &claude, 1_000_000_000 - 60, false));
        assert!(!record_due("claude", &claude, 0, false));
        assert!(record_due("claude", &claude, 0, true));
        assert!(!renewable("claude", &json!({"claudeAiOauth": {}})));
        assert!(renewable("codex", &codex_record()));
        assert!(!renewable("codex", &json!({"auth_mode": "apikey"})));
        assert!(!renewable("pi", &pi_record()), "pi renews per entry");
    }

    /// core#1024: the broker judges a CLI's own renewal by its store alone. A changed store is
    /// taken (even when the command then failed: a rotated refresh token is never dropped); an
    /// unchanged one is "not due by the CLI's own rule" while valid, a failure once expired or
    /// when the command failed; an unusable store is refused. No error repeats a credential.
    #[test]
    fn a_clis_own_renewal_is_judged_by_its_store() {
        let now = 1_900_000_000;
        let old = codex_record();
        let mut renewed = old.clone();
        renewed["tokens"]["access_token"] = json!("FAKE.renewed.access");
        renewed["tokens"]["refresh_token"] = json!("FAKE-rotated-refresh");
        let bytes = serde_json::to_vec(&renewed).unwrap();
        assert_eq!(
            judge("codex", None, &old, Some(&bytes), Ok(()), now).unwrap(),
            Renewal::Renewed(renewed.clone())
        );
        assert_eq!(
            judge(
                "codex",
                None,
                &old,
                Some(&bytes),
                Err("it exited with 1".into()),
                now
            )
            .unwrap(),
            Renewal::Renewed(renewed),
            "a rotated store is kept whatever the command said"
        );
        let same = serde_json::to_vec(&old).unwrap();
        assert_eq!(
            judge("codex", None, &old, Some(&same), Ok(()), now).unwrap(),
            Renewal::Unchanged
        );
        let failed = judge(
            "codex",
            None,
            &old,
            Some(&same),
            Err("it timed out".into()),
            now,
        )
        .unwrap_err();
        assert_eq!(failed, "codex could not renew its sign-in (it timed out)");
        // An expired entry the CLI left as it was is a failed renewal.
        let mut expired = pi_record();
        expired["anthropic"]["expires"] = json!(1_000_u64);
        let same = serde_json::to_vec(&expired).unwrap();
        assert_eq!(
            judge("pi", Some("anthropic"), &expired, Some(&same), Ok(()), now).unwrap_err(),
            "pi did not renew its expired sign-in"
        );
        expired["github-copilot"]["expires"] = json!(3_000_000_000_000_u64);
        let same = serde_json::to_vec(&expired).unwrap();
        assert_eq!(
            judge(
                "pi",
                Some("github-copilot"),
                &expired,
                Some(&same),
                Ok(()),
                now
            )
            .unwrap(),
            Renewal::Unchanged,
            "a valid entry pi left alone was not due by pi's own rule"
        );
        // A store the CLI emptied, or one the broker cannot serve, is refused by name.
        assert!(judge("codex", None, &old, None, Ok(()), now).is_err());
        assert!(judge("codex", None, &old, Some(b"not json"), Ok(()), now).is_err());
        let mut broken = old.clone();
        broken["tokens"]["account_id"] = json!("FAKE\r\nx-evil: 1");
        let err = judge(
            "codex",
            None,
            &old,
            Some(&serde_json::to_vec(&broken).unwrap()),
            Ok(()),
            now,
        )
        .unwrap_err();
        assert!(
            err.starts_with("codex's renewed sign-in was refused"),
            "{err}"
        );
        for e in [failed, err] {
            assert!(!e.contains("FAKE"), "{e}");
        }
    }

    /// codex's own proactive refresh: `account/read` with `refreshToken: true`, after the
    /// protocol's handshake, naming this engine (never another client) as the client.
    #[test]
    fn codex_is_asked_for_its_own_proactive_refresh() {
        let lines: Vec<Value> = String::from_utf8(codex_requests())
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines[0]["method"], "initialize");
        assert_eq!(lines[0]["params"]["clientInfo"]["name"], "wicked_core");
        assert_eq!(lines[1]["method"], "initialized");
        assert_eq!(
            lines[2],
            json!({"id": 2, "method": "account/read", "params": {"refreshToken": true}})
        );
        assert_eq!(
            codex_answer(br#"{"id":2,"result":{"account":null}}"#),
            Some(true)
        );
        assert_eq!(
            codex_answer(br#"{"id":2,"error":{"code":-32603,"message":"x"}}"#),
            Some(false)
        );
        assert_eq!(codex_answer(br#"{"id":1,"result":{}}"#), None);
        assert_eq!(codex_answer(br#"{"method":"configWarning"}"#), None);
        assert_eq!(pi_min_expiry(false), "16m");
        assert_eq!(pi_min_expiry(true), "100000h");
    }

    /// core#1024: the broker's own requests (the copilot account reads, its only ones) carry no
    /// vendor identity: no User-Agent of any client but curl's own, no editor or app headers.
    /// The GitHub token rides stdin as a header, never a command-line argument.
    #[cfg(unix)]
    #[test]
    fn the_brokers_own_requests_carry_no_vendor_identity_and_no_token_on_argv() {
        for host in [None, Some("api.individual.githubcopilot.com")] {
            let request = copilot_account_request("FAKE-gho-secret", host);
            let (argv, stdin) = request_argv(&request).unwrap();
            assert!(!argv.iter().any(|a| a.contains("FAKE-gho-secret")));
            assert_eq!(stdin, b"authorization: Bearer FAKE-gho-secret\n");
            assert!(argv.windows(2).any(|w| w == ["-H", "@-"]));
            let agent = ["-", "A"].concat();
            assert!(
                !argv.iter().any(|a| *a == agent || a == "--user-agent"),
                "{argv:?}"
            );
            for header in argv.iter().map(|a| a.to_ascii_lowercase()) {
                for vendor in ["user-agent:", "editor-", "x-app:"] {
                    assert!(!header.starts_with(vendor), "{header}");
                }
            }
            assert!(!argv.iter().any(|a| a == "--fail"), "{argv:?}");
            assert!(argv.windows(2).any(|w| w == ["-w", STATUS_FORMAT]));
        }
        let injected = Request {
            url: "u".into(),
            token: "FAKE\r\nx-evil: 1".into(),
            headers: Vec::new(),
        };
        assert!(request_argv(&injected).is_err());
    }

    /// core#1024: no client identity of a vendor's CLI is written anywhere in the broker or the
    /// gateway (the strings are split here so this file does not match itself).
    #[test]
    fn no_vendor_client_identity_is_spelled_in_the_broker_or_the_gateway() {
        let sources = [
            include_str!("seat_refresh.rs"),
            include_str!("seat_gateway.rs"),
            include_str!("seat_login.rs"),
            include_str!("copilot_account.rs"),
            include_str!("container_floor.rs"),
        ];
        let needles = [
            ["claude", "-cli/"].concat(),
            ["codex", "_cli_rs"].concat(),
            ["GitHub", "CopilotChat"].concat(),
            ["editor", "-version"].concat(),
            ["copilot-integration-id", "\", \"vscode"].concat(),
            ["\"-", "A\""].concat(),
        ];
        for source in sources {
            for needle in &needles {
                assert!(!source.contains(needle.as_str()), "{needle} is spelled");
            }
        }
    }

    /// core#1024 (no runtime): codex, pi and opencode each renew their own sign-in on this host,
    /// in a throwaway home that is gone afterwards; the broker hands the record over in the CLI's
    /// own store, never on argv, and the CLI's environment holds nothing of the daemon's beyond
    /// the pass-through list.
    #[cfg(unix)]
    #[test]
    fn each_cli_renews_its_own_sign_in_in_a_throwaway_home() {
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!("c4-renew-{}", uuid::Uuid::new_v4().simple()));
        let bin = dir.join("bin");
        install_stub_clis(
            &bin,
            &[
                ("FAKE.access.token", "FAKE.renewed.access"),
                ("FAKE-ant-r", "FAKE-ant-r2"),
                ("FAKE-r\"", "FAKE-r2\""),
            ],
        );
        *TEST_CLI_DIR.lock().unwrap() = Some(bin.clone());
        let gw = crate::seat_gateway::Gateway::for_dir(&dir);
        let now = 1_900_000_000;
        let codex = renew_on_host(&dir, &gw, "codex", &codex_record(), None, false, now);
        let pi = renew_on_host(&dir, &gw, "pi", &pi_record(), Some("anthropic"), true, now);
        let export = json!([{"id": "cur", "integrationID": "openai", "label": "OAuth",
            "active": true, "value": {"type": "oauth", "methodID": "chatgpt-headless",
            "refresh": "FAKE-r", "access": "FAKE-a", "expires": 1_000_u64,
            "metadata": {"accountID": "acct-FAKE"}}}]);
        let opencode = renew_on_host(&dir, &gw, "opencode", &export, Some("openai"), false, now);
        std::fs::write(dir.join("refuse-codex"), "").unwrap();
        let refused = renew_on_host(&dir, &gw, "codex", &codex_record(), None, true, now);
        let mut stale = export.clone();
        stale[0]["value"]["refresh"] = json!("FAKE-q");
        let expired = renew_on_host(&dir, &gw, "opencode", &stale, Some("openai"), false, now);
        std::fs::write(dir.join("refuse-opencode"), "").unwrap();
        let failed = renew_on_host(&dir, &gw, "opencode", &export, Some("openai"), false, now);
        *TEST_CLI_DIR.lock().unwrap() = Some(dir.join("nothing-here"));
        let missing = renew_on_host(&dir, &gw, "codex", &codex_record(), None, true, now);
        *TEST_CLI_DIR.lock().unwrap() = None;
        let log = std::fs::read_to_string(dir.join("stub-cli.log")).unwrap();
        let homes: Vec<_> = std::fs::read_dir(dir.join("login"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        std::fs::remove_dir_all(&dir).unwrap();
        match codex.unwrap() {
            Renewal::Renewed(r) => {
                assert_eq!(r["tokens"]["access_token"], "FAKE.renewed.access");
                assert_eq!(r["tokens"]["refresh_token"], "FAKE-old-refresh");
            }
            other => panic!("{other:?}"),
        }
        match pi.unwrap() {
            Renewal::Renewed(r) => assert_eq!(r["anthropic"]["refresh"], "FAKE-ant-r2"),
            other => panic!("{other:?}"),
        }
        match opencode.unwrap() {
            Renewal::Renewed(r) => assert_eq!(r[0]["value"]["refresh"], "FAKE-r2"),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            refused.unwrap_err(),
            "codex could not renew its sign-in (its account/read answered an error)"
        );
        assert_eq!(
            expired.unwrap_err(),
            "opencode did not renew its expired sign-in"
        );
        assert_eq!(
            failed.unwrap_err(),
            "opencode could not renew its sign-in (it exited with exit status: 1)"
        );
        assert_eq!(
            missing.unwrap_err(),
            "codex could not renew its sign-in (it could not start)"
        );
        assert!(homes.is_empty(), "every throwaway home is gone: {homes:?}");
        // The CLIs' own commands, and nothing of the record on any command line.
        assert!(log.contains("codex app-server -c cli_auth_credentials_store=\"file\" |"));
        assert!(
            log.contains("pi auth print-bearer-token --provider anthropic --min-expiry 100000h |")
        );
        assert!(log.contains("opencode auth import --standalone |"));
        assert!(log.contains("opencode models --standalone |"));
        assert!(log.contains("opencode auth export --standalone |"));
        for secret in [
            "FAKE-old-refresh",
            "FAKE-ant-r",
            "FAKE-r",
            "FAKE.access.token",
        ] {
            assert!(
                !log.lines()
                    .any(|l| l.split(" | ").next().unwrap().contains(secret)),
                "{secret} on a command line: {log}"
            );
        }
        // An env -i environment: the throwaway home's own variables and the pass-through list.
        for line in log.lines() {
            let names = line.split(" | ").nth(1).unwrap();
            for name in names.split_whitespace() {
                let allowed = crate::seat_login::host_cli_env(
                    Path::new("/h"),
                    line.split_whitespace().next().unwrap(),
                    &std::env::vars().collect::<Vec<_>>(),
                )
                .iter()
                .any(|(k, _)| k == name)
                    || ["PWD", "SHLVL", "_", "OLDPWD"].contains(&name);
                assert!(allowed, "{name} reached a renewing CLI: {line}");
            }
        }
    }

    #[test]
    fn first_staging_failure_retries_and_exhaustion_rescues_rotated_record_exclusively() {
        for failures in [1, 3] {
            let dir =
                std::env::temp_dir().join(format!("c4-stage-{}", uuid::Uuid::new_v4().simple()));
            let new = claude_record("FAKE-new", "FAKE-R2", 2_003_600_000_000);
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

    /// The copilot account reads ride attached stdio with no runtime logging; a refused read
    /// names its status and error code, never the body's detail.
    #[cfg(unix)]
    #[test]
    fn account_reads_use_attached_stdio_without_runtime_logging() {
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
case "$*" in
  *refused*) printf '{"message":"FAKE-detail","error":{"type":"rate_limit_error"}}\n429' ;;
  *) printf '{"endpoints":{"api":"https://FAKE.invalid"}}\n200' ;;
esac
"#,
        );
        let rt = crate::container_floor::Runtime { cli: runtime };
        let _lock = mutation_lock(&dir).unwrap();
        let read = |url: &str| {
            live::send(
                &rt,
                &dir,
                "copilot",
                "FAKE-ext",
                &Request {
                    url: url.into(),
                    token: "FAKE-gho-R1".into(),
                    headers: crate::copilot_account::USER_HEADERS.to_vec(),
                },
            )
        };
        let answer = read("https://FAKE.invalid/copilot_internal/user").unwrap();
        assert_eq!(answer["endpoints"]["api"], "https://FAKE.invalid");
        let argv = std::fs::read_to_string(dir.join("argv")).unwrap();
        assert!(argv.contains("--log-driver none"));
        assert!(!argv.contains("FAKE-gho-R1"));
        assert!(!argv.contains(" -A "), "{argv}");
        let refused = read("https://FAKE.invalid/refused").unwrap_err();
        assert_eq!(refused, "the endpoint answered HTTP 429 (rate_limit_error)");
        assert_eq!(
            std::fs::read_to_string(dir.join("request")).unwrap(),
            "authorization: Bearer FAKE-gho-R1\n"
        );
        drop(_lock);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn partial_refresh_republishes_retained_generation_before_not_due() {
        let dir =
            std::env::temp_dir().join(format!("c4-refresh-{}", uuid::Uuid::new_v4().simple()));
        let record = serde_json::to_vec(&claude_record(
            "FAKE-new",
            "FAKE-rotated",
            2_003_600_000_000,
        ))
        .unwrap();
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
        let record =
            serde_json::to_vec(&claude_record("FAKE-new", "FAKE-R2", 2_003_600_000_000)).unwrap();
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

    /// The status line curl appends is split off the body; a refusal reports the status and the
    /// error's code only (core#1011).
    #[test]
    fn a_refused_answer_names_its_status_and_code_not_its_body() {
        assert_eq!(
            split_status(b"{\"a\":1}\n200"),
            (&b"{\"a\":1}"[..], Some(200))
        );
        assert_eq!(split_status(b"\n429"), (&b""[..], Some(429)));
        assert_eq!(split_status(b"{\"a\":1}"), (&b"{\"a\":1}"[..], None));
        assert_eq!(split_status(b"{}\n20"), (&b"{}\n20"[..], None));
        assert_eq!(
            refusal(
                400,
                br#"{"error":"invalid_grant","error_description":"FAKE-secret-detail"}"#
            ),
            "the endpoint answered HTTP 400 (invalid_grant)"
        );
        assert_eq!(
            refusal(
                429,
                br#"{"error":{"type":"rate_limit_error","message":"FAKE-detail"}}"#
            ),
            "the endpoint answered HTTP 429 (rate_limit_error)"
        );
        assert_eq!(
            refusal(403, b"<html>FAKE</html>"),
            "the endpoint answered HTTP 403"
        );
        assert_eq!(
            refusal(400, br#"{"error":"bad grant; FAKE token=abc"}"#),
            "the endpoint answered HTTP 400",
            "free text in the code's place is not repeated"
        );
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
            error: Some(CLAUDE_SIGN_IN_AGAIN.into()),
        };
        record_last(&dir, "claude", Some(&failed));
        assert_eq!(last_refresh(&dir, "claude"), Some(failed));
        record_last(&dir, "claude", None);
        assert_eq!(last_refresh(&dir, "claude"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
