//! The daemon's model gateway on the cluster (PARITY B8, B9, C4; core#1394): every operation the
//! Docker gateway ran as a helper container is an exec into the gateway pod's long-lived
//! `writer` sidecar, the only container that mounts the config and broker volumes read-write.
//! **0 container lifecycles per seat start.**
//!
//! The host-side state machine is [`crate::seat_gateway`]'s own and unchanged: the persisted
//! routes and units (`config/gateway.json`, `config/units.json`), the pending revocations
//! (`config/revoking.json`), the rendered config, the locks. Only the transport is new.
//!
//! **Secrets ride stdin, never argv.** An exec's command is a URL query on the API server (in its
//! audit log and the kubelet's), so a credential or a unit key is always on stdin, and every
//! write is `cat > tmp`, then a sha256 check against the digest on the argv, then `mv`: a client
//! killed mid-write can never commit a partial credential.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use super::render::{self, GatewaySpec, HostPort};
use super::{cluster, daemon_namespace_name, Called, Kube};
use crate::seat_gateway::{GatewayRecord, UnitKey};
use crate::worker_sandbox::FloorUnarmed;

const EXEC_TIMEOUT: Duration = Duration::from_secs(60);
const READY_TIMEOUT: Duration = Duration::from_secs(120);

/// The daemon's gateway as the engine drives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Gw {
    pub(crate) kube: Kube,
    /// The daemon namespace.
    pub(crate) ns: String,
    /// The gateway pod whose writer is exec'd into.
    pub(crate) pod: String,
    /// The `agw` Service's cluster IP (a seat's `hostAliases` entry).
    pub(crate) service_ip: String,
    /// The daemon's gateway directory (the host-side state).
    pub(crate) dir: PathBuf,
}

/// Where a writer write lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Volume {
    Config,
    Broker,
}

impl Volume {
    fn dir(self) -> &'static str {
        match self {
            Volume::Config => "/cfg",
            Volume::Broker => "/broker",
        }
    }
}

/// A plain file name a writer script may carry (no path, quote or shell character).
pub(crate) fn plain_name(file: &str) -> Result<(), String> {
    if file.is_empty()
        || file.starts_with('.')
        || !file
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
    {
        return Err(format!("`{file}` is not a gateway file name"));
    }
    Ok(())
}

/// The writer script that replaces `<dir>/<file>` with stdin only when stdin's sha256 is
/// `digest` (pure: the argv carries the name and the digest, never the body).
pub(crate) fn write_script(v: Volume, file: &str, digest: &str) -> Result<String, String> {
    plain_name(file)?;
    if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("not a sha256 digest".into());
    }
    let d = v.dir();
    Ok(format!(
        "set -e; umask 077; t={d}/.{file}.$$; trap 'rm -f \"$t\"' EXIT; cat > \"$t\"; \
         [ \"$(sha256sum \"$t\" | cut -d' ' -f1)\" = {digest} ] || {{ echo 'wicked-writer: the body did not arrive whole' >&2; exit 65; }}; \
         mv -f \"$t\" {d}/{file}; trap - EXIT"
    ))
}

/// The readiness probe script: the key on stdin, then up to 15 s until the gateway answers it
/// with `want` (` 200 ` served, ` 401 ` refused) on its readiness listener.
pub(crate) fn confirm_script(accepted: bool) -> String {
    let want = if accepted { "HTTP/1.1 200" } else { " 401 " };
    format!(
        "read K; i=0; while [ $i -lt 60 ]; do \
         o=$(wget -S -O /dev/null --header \"x-api-key: $K\" http://127.0.0.1:{}/ 2>&1); \
         case \"$o\" in *'{want}'*) exit 0;; esac; i=$((i+1)); sleep 0.25; done; exit 1",
        crate::seat_gateway::READY_PORT
    )
}

/// The broker-read script: for each named file that exists and is a regular file (no link on
/// the way), `name size` and the body (`presence` names are read as a marker only: `1` when
/// nonempty).
pub(crate) fn read_script(files: &[(&str, bool)]) -> Result<String, String> {
    let mut s = String::from("set -e;");
    for (f, presence) in files {
        plain_name(f)?;
        let p = format!("/broker/{f}");
        s.push_str(&format!(
            " if [ -f {p} ] && [ ! -L {p} ]; then n=$(wc -c < {p}); if [ $n -gt {max} ]; then exit 66; fi; "
            , max = crate::seat_login::MAX_FILE_BYTES
        ));
        if *presence {
            s.push_str(&format!(
                "if [ -s {p} ]; then printf '%s 1\\n1' {f}; else printf '%s 0\\n' {f}; fi; fi;"
            ));
        } else {
            s.push_str(&format!("printf '%s %s\\n' {f} $n; cat {p}; fi;"));
        }
    }
    Ok(s)
}

/// Parse [`read_script`]'s output (pure). `Err` on anything malformed.
pub(crate) fn parse_read(out: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut files = Vec::new();
    let mut rest = out;
    while !rest.is_empty() {
        let nl = rest
            .iter()
            .position(|b| *b == b'\n')
            .ok_or("a malformed broker read")?;
        let head = std::str::from_utf8(&rest[..nl]).map_err(|_| "a malformed broker read")?;
        let (name, size) = head.split_once(' ').ok_or("a malformed broker read")?;
        let size: usize = size.trim().parse().map_err(|_| "a malformed broker read")?;
        rest = &rest[nl + 1..];
        if rest.len() < size {
            return Err("a truncated broker read".into());
        }
        files.push((name.to_string(), rest[..size].to_vec()));
        rest = &rest[size..];
    }
    Ok(files)
}

impl Gw {
    /// One exec into the writer: `sh -c script` with `$0` = a unique marker (a dead engine's
    /// leftover is findable by it), stdin when given.
    pub(crate) fn exec(&self, script: &str, stdin: Option<&[u8]>, limit: Duration) -> Called {
        let marker = format!("wicked-writer-{}", uuid::Uuid::new_v4().simple());
        let mut args = vec!["exec"];
        if stdin.is_some() {
            args.push("-i");
        }
        args.extend([
            "-c",
            "writer",
            self.pod.as_str(),
            "--",
            "sh",
            "-c",
            script,
            &marker,
        ]);
        self.kube.call(Some(&self.ns), &args, stdin, limit)
    }

    /// Replace `<volume>/<file>` with `body`, whole or not at all.
    pub(crate) fn write_file(&self, v: Volume, file: &str, body: &[u8]) -> Result<(), String> {
        let digest = super::hex(&Sha256::digest(body));
        let script = write_script(v, file, &digest)?;
        let r = self.exec(&script, Some(body), EXEC_TIMEOUT);
        if r.ok() {
            Ok(())
        } else {
            // The writer's stderr names no credential (the script prints none).
            Err(format!(
                "the gateway writer could not write {file}: {}",
                r.err_text()
            ))
        }
    }

    /// Empty a broker file (signed out): the gateway's routes reference it, so it is never
    /// deleted while a route names it.
    pub(crate) fn empty_broker_file(&self, file: &str) -> Result<(), String> {
        self.write_file(Volume::Broker, file, b"")
    }

    /// Read broker files (see [`read_script`]).
    pub(crate) fn read_broker(
        &self,
        files: &[(&str, bool)],
    ) -> Result<Vec<(String, Vec<u8>)>, String> {
        let r = self.exec(&read_script(files)?, None, EXEC_TIMEOUT);
        if !r.ok() {
            return Err(format!("could not read the broker: {}", r.err_text()));
        }
        parse_read(&r.stdout)
    }

    /// Publish the rendered `config/config.json` (the gateway's watcher reloads on the rename).
    pub(crate) fn publish(&self) -> Result<(), String> {
        let body = std::fs::read(self.dir.join("config/config.json"))
            .map_err(|e| format!("could not read the rendered gateway config: {e}"))?;
        self.write_file(Volume::Config, "config.json", &body)
    }

    /// Wait until the gateway serves (`accepted`) or refuses `key`; the key rides stdin.
    pub(crate) fn confirm_key(&self, key: &str, accepted: bool) -> Result<(), String> {
        let r = self.exec(
            &confirm_script(accepted),
            Some(format!("{key}\n").as_bytes()),
            EXEC_TIMEOUT,
        );
        if r.ok() {
            Ok(())
        } else {
            Err(format!(
                "the gateway did not {} the unit key in time (its config reload was not applied)",
                if accepted { "accept" } else { "revoke" }
            ))
        }
    }

    /// The gateway's access-log records for `unit` since `since` (unix seconds).
    pub(crate) fn records(&self, since: &str, unit: &str) -> Vec<GatewayRecord> {
        let since = since
            .parse::<u64>()
            .map(super::rfc3339)
            .unwrap_or_else(|_| super::rfc3339(0));
        let t = format!("--since-time={since}");
        let r = self.kube.call(
            Some(&self.ns),
            &["logs", "-c", "gateway", self.pod.as_str(), &t],
            None,
            EXEC_TIMEOUT,
        );
        if !r.ok() {
            return Vec::new();
        }
        let mut out = crate::seat_gateway::parse_records(&String::from_utf8_lossy(&r.stdout), unit);
        out.extend(crate::seat_gateway::parse_records(
            &String::from_utf8_lossy(&r.stderr),
            unit,
        ));
        out
    }
}

/// The empty credential file of every route (a reload naming a missing file is refused).
fn init_broker_script(config: &crate::seat_gateway::GatewayConfig) -> String {
    let mut s = String::from("set -e; umask 077;");
    for r in &config.providers {
        if let Some(f) = r.credential_file.file_name().and_then(|f| f.to_str()) {
            if plain_name(f).is_ok() {
                s.push_str(&format!(" [ -e /broker/{f} ] || : > /broker/{f};"));
            }
        }
    }
    s
}

/// The host's loopback as pods reach it: the first host alias the cluster's DNS resolves, from
/// the writer (`nslookup`), as an IP.
fn host_ip(gw: &Gw) -> Option<String> {
    crate::seat_gateway::HOST_ALIASES.iter().find_map(|alias| {
        let r = gw.exec(
            &format!("nslookup {alias} 2>/dev/null"),
            None,
            Duration::from_secs(15),
        );
        parse_nslookup(&String::from_utf8_lossy(&r.stdout))
    })
}

/// The answer address of busybox `nslookup` output (the last `Address:` line after `Name:`).
pub(crate) fn parse_nslookup(out: &str) -> Option<String> {
    let after = out.split_once("Name:")?.1;
    after
        .lines()
        .filter_map(|l| {
            l.trim()
                .strip_prefix("Address:")
                .or_else(|| l.trim().strip_prefix("Address 1:"))
        })
        .map(|a| a.split_whitespace().next().unwrap_or("").to_string())
        .find(|a| a.parse::<std::net::Ipv4Addr>().is_ok())
}

/// The host ports the gateway must reach: the gate-hook relay and crew's loopback broker.
fn host_ports() -> Vec<u16> {
    let mut v = Vec::new();
    if let Some(p) = crate::hook_relay::port() {
        v.push(p);
    }
    if let Some(p) = crate::mcp_gate::daemon_crew_url()
        .as_deref()
        .and_then(crate::seat_gateway::crew_port)
    {
        v.push(p);
    }
    v
}

/// Probe the hook relay and the MCP broker from the gateway's own network namespace and record
/// the routes the next publish carries (once found, kept for the process).
fn probe_routes(gw: &Gw, ip: &str) {
    if let Some(port) = crate::hook_relay::port() {
        let up = format!("{ip}:{port}");
        crate::seat_gateway::probe_once(&crate::seat_gateway::HOOK_UPSTREAM, || {
            // A fresh egress rule applies within seconds (F1): retried briefly.
            (0..20).find_map(|_| {
                let r = gw.exec(
                    &format!("wget -q -T 2 -O - http://{up}/ping 2>/dev/null"),
                    None,
                    Duration::from_secs(10),
                );
                if r.ok() && String::from_utf8_lossy(&r.stdout).contains("pong") {
                    Some(up.clone())
                } else {
                    std::thread::sleep(Duration::from_millis(500));
                    None
                }
            })
        });
    }
    if let Some(port) = crate::mcp_gate::daemon_crew_url()
        .as_deref()
        .and_then(crate::seat_gateway::crew_port)
    {
        let up = format!("{ip}:{port}");
        crate::seat_gateway::probe_once(&crate::seat_gateway::MCP_UPSTREAM, || {
            (0..20).find_map(|_| {
                let r = gw.exec(
                    &format!("wget -S -T 2 -O /dev/null http://{up}/api/v1/health 2>&1"),
                    None,
                    Duration::from_secs(10),
                );
                if String::from_utf8_lossy(&r.stdout).contains("HTTP/1.") {
                    Some(up.clone())
                } else {
                    std::thread::sleep(Duration::from_millis(500));
                    None
                }
            })
        });
    }
}

/// The daemon's gateway, started or taken over, serving its current config (the cluster half
/// of `seat_gateway::ensure_gateway`): keys an earlier engine left are confirmed refused before
/// any new unit runs.
pub(crate) fn ensure(kube: &Kube, dir: &Path) -> Result<Gw, (FloorUnarmed, String)> {
    let gwerr = |e: String| (FloorUnarmed::GatewayUnavailable, e);
    if !dir.is_absolute() {
        return Err(gwerr("the gateway directory must be absolute".into()));
    }
    let _broker = crate::seat_refresh::mutation_lock(dir).map_err(gwerr)?;
    // `config_transaction` holds the publish lock (review r1: taking it here too deadlocked).
    let _transaction = crate::seat_gateway::config_transaction(dir).map_err(gwerr)?;
    let ns = daemon_namespace_name(&kube.owner);
    let spec = GatewaySpec {
        namespace: ns.clone(),
        owner: kube.owner.clone(),
        image: crate::seat_gateway::GATEWAY_IMAGE.into(),
        writer_image: crate::seat_gateway::HELPER_IMAGE.into(),
        // A quarantine whose rotation did not land is finished first (review r2): the
        // gateway never starts again on the token it was stopped under.
        boot: {
            let pending = ROTATE.lock().is_ok_and(|q| q.iter().any(|d| d == dir));
            let t = boot_token(dir, pending).map_err(gwerr)?;
            if pending {
                if let Ok(mut q) = ROTATE.lock() {
                    q.retain(|d| d != dir);
                }
            }
            t
        },
    };
    // Applied with the host ports already granted (idempotent; it also re-creates a gateway a
    // quarantine stopped, under its new boot token), then again once the host's address is
    // resolved from inside, when a port is missing.
    let existing = kube
        .get_json(Some(&ns), "networkpolicy", "gateway-egress")
        .map_err(|e| (FloorUnarmed::ClusterUnreachable, e))?;
    let mut host: Vec<HostPort> = existing.as_ref().map(render_host_ports).unwrap_or_default();
    let want = host_ports();
    cluster::apply_gateway(kube, &spec, &host).map_err(|e| (FloorUnarmed::PolicyRefused, e))?;
    let pod = cluster::writer_pod(kube, &ns, READY_TIMEOUT).map_err(gwerr)?;
    let mut gw = Gw {
        kube: kube.clone(),
        ns: ns.clone(),
        pod,
        service_ip: String::new(),
        dir: dir.to_path_buf(),
    };
    let ip = host_ip(&gw);
    if let Some(ip) = &ip {
        let missing: Vec<u16> = want
            .iter()
            .copied()
            .filter(|p| !host.iter().any(|h| &h.ip == ip && h.port == *p))
            .collect();
        if !missing.is_empty() {
            host.retain(|h| &h.ip == ip);
            host.extend(missing.into_iter().map(|port| HostPort {
                ip: ip.clone(),
                port,
            }));
            cluster::apply_gateway(kube, &spec, &host)
                .map_err(|e| (FloorUnarmed::PolicyRefused, e))?;
        }
        probe_routes(&gw, ip);
    }
    let mut config =
        crate::seat_gateway::provision_config(dir, crate::seat_gateway::egress_hosts_from_env())
            .map_err(gwerr)?;
    let r = gw.exec(&init_broker_script(&config), None, EXEC_TIMEOUT);
    if !r.ok() {
        return Err(gwerr(format!(
            "could not initialize the broker: {}",
            r.err_text()
        )));
    }
    let owned = crate::seat_gateway::OWNED
        .lock()
        .is_ok_and(|o| o.iter().any(|d| d == dir));
    if !owned && !config.units.is_empty() {
        // An earlier engine's keys: pending first, so a failure below still knows them.
        crate::seat_gateway::mark_revoking(dir, config.units.iter().map(|u| u.key.clone()))
            .map_err(gwerr)?;
        config.units.clear();
    }
    crate::seat_gateway::persist_units(dir, &config).map_err(gwerr)?;
    gw.publish().map_err(gwerr)?;
    // Only now may the gateway container start (a fresh pod waits for this token).
    gw.write_file(Volume::Config, "boot", spec.boot.as_bytes())
        .map_err(gwerr)?;
    let served = cluster::gateway_ready(kube, &ns, READY_TIMEOUT).map_err(gwerr)?;
    gw.pod = served.pod;
    gw.service_ip = served.service_ip;
    settle_revoking_locked(&gw).map_err(gwerr)?;
    if let Ok(mut o) = crate::seat_gateway::OWNED.lock() {
        if !o.iter().any(|d| d == dir) {
            o.push(dir.to_path_buf());
        }
    }
    Ok(gw)
}

/// The host ports an existing gateway egress policy names (pure).
pub(crate) fn render_host_ports(policy: &serde_json::Value) -> Vec<HostPort> {
    let mut out = Vec::new();
    for rule in policy
        .pointer("/spec/egress")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        let cidr = rule
            .pointer("/to/0/ipBlock/cidr")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let Some(ip) = cidr.strip_suffix("/32") else {
            continue;
        };
        for p in rule
            .get("ports")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(port) = p.get("port").and_then(serde_json::Value::as_u64) {
                out.push(HostPort {
                    ip: ip.to_string(),
                    port: port as u16,
                });
            }
        }
    }
    out
}

fn settle_revoking_locked(gw: &Gw) -> Result<(), String> {
    let pending = crate::seat_gateway::rerender_without_pending(&gw.dir)?;
    if pending.is_empty() {
        return Ok(());
    }
    gw.publish()?;
    for key in &pending {
        gw.confirm_key(key, false)?;
    }
    crate::seat_gateway::atomic_json(
        &crate::seat_gateway::revoking_path(&gw.dir),
        &Vec::<String>::new(),
    )
}

/// Register `unit` and publish; the seat starts only once the gateway serves its key.
pub(crate) fn register_live(gw: &Gw, unit: UnitKey) -> Result<(), String> {
    let _broker = crate::seat_refresh::mutation_lock(&gw.dir)?;
    let _transaction = crate::seat_gateway::config_transaction(&gw.dir)?;
    let key = unit.key.clone();
    crate::seat_gateway::register_unit_locked(&gw.dir, unit)?;
    gw.publish()?;
    gw.confirm_key(&key, true)
}

/// Revoke `key`: complete only when the gateway refuses it (and every key still pending).
pub(crate) fn deregister_live(gw: &Gw, key: &str) -> Result<(), String> {
    let _broker = crate::seat_refresh::mutation_lock(&gw.dir)?;
    let _transaction = crate::seat_gateway::config_transaction(&gw.dir)?;
    crate::seat_gateway::deregister_unit_locked(&gw.dir, key)?;
    settle_revoking_locked(gw)
}

/// A revocation the gateway never confirmed: the gateway is stopped (scaled to none, its pods
/// waited out) and its boot token replaced, so no served policy outlives a key the engine meant
/// to revoke: the next [`ensure`] re-creates it, and its gateway container starts only after the
/// writer has published a config without the key and the new token (review r1). `true` when no
/// gateway pod is left.
pub(crate) fn quarantine(gw: &Gw) -> bool {
    let _publishing = crate::seat_gateway::PUBLISH_LOCK.lock();
    if let Ok(mut o) = crate::seat_gateway::OWNED.lock() {
        o.retain(|d| d != &gw.dir);
    }
    // Stopped first, whatever the host state allows (review r2): the token is rotated after.
    if let Ok(mut q) = ROTATE.lock() {
        q.push(gw.dir.clone());
    }
    let r = gw.kube.call(
        Some(&gw.ns),
        &["scale", "deployment", render::GATEWAY_NAME, "--replicas=0"],
        None,
        EXEC_TIMEOUT,
    );
    if !r.ok() {
        return false;
    }
    let start = Instant::now();
    while start.elapsed() < READY_TIMEOUT {
        match gw
            .kube
            .list_owned(Some(&gw.ns), "pods", &[(super::ROLE_LABEL, "gateway")])
        {
            Ok(p) if p.is_empty() => {
                if boot_token(&gw.dir, true).is_ok() {
                    if let Ok(mut q) = ROTATE.lock() {
                        q.retain(|d| d != &gw.dir);
                    }
                }
                return true;
            }
            _ => std::thread::sleep(Duration::from_millis(500)),
        }
    }
    false
}

/// Gateway dirs whose boot token a quarantine has not yet rotated: [`ensure`] rotates it first,
/// and refuses to start the gateway until it can.
static ROTATE: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

/// The gateway's boot token (`config/boot-token` of the gateway dir), created when absent or when
/// `fresh` (a quarantine).
pub(crate) fn boot_token(dir: &Path, fresh: bool) -> Result<String, String> {
    let path = dir.join("config/boot-token");
    if !fresh {
        if let Ok(t) = std::fs::read_to_string(&path) {
            let t = t.trim();
            if !t.is_empty() && t.chars().all(|c| c.is_ascii_alphanumeric()) {
                return Ok(t.to_string());
            }
        }
    }
    let t = uuid::Uuid::new_v4().simple().to_string();
    std::fs::create_dir_all(dir.join("config")).map_err(|e| e.to_string())?;
    std::fs::write(&path, &t).map_err(|e| e.to_string())?;
    Ok(t)
}

/// The broker files a status read of `cli` needs: retained records whole, the rest as presence.
fn status_files(cli: &str) -> Result<Vec<(&'static str, bool)>, String> {
    Ok(crate::seat_login::broker_files(cli)?
        .iter()
        .map(|f| (*f, !crate::seat_login::is_archive(f)))
        .collect())
}

/// The sign-in a governed seat of `cli` runs on, read from the broker through the writer (the
/// cluster half of `seat_login::seat_sign_in`). Signed out, expired or unreadable = the reason.
pub(crate) fn seat_sign_in(
    gw: &Gw,
    cli: &str,
) -> Result<crate::seat_credentials::SeatSignIn, String> {
    use crate::seat_login::BrokerStatus;
    let _lock = crate::seat_refresh::mutation_lock(&gw.dir)?;
    let files = gw.read_broker(&status_files(cli)?)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| "system clock precedes Unix epoch")?
        .as_secs();
    let status = crate::seat_login::status_from_files(cli, &files, now);
    let status = crate::seat_login::publication_status(
        &gw.dir,
        cli,
        crate::seat_login::with_last_refresh(
            status,
            crate::seat_refresh::last_refresh(&gw.dir, cli),
        ),
    );
    match status {
        BrokerStatus::SignedIn { .. } | BrokerStatus::RefreshFailing { .. } => {
            crate::seat_login::sign_in_from_files(cli, &files, now)
                .ok_or_else(|| format!("{cli}: the broker holds no usable sign-in"))
        }
        BrokerStatus::SignedOut => Err(format!("{cli} is not signed in")),
        BrokerStatus::Expired => Err(format!("{cli}'s sign-in has expired")),
        BrokerStatus::Unknown(why) => Err(format!("{cli}'s sign-in could not be read: {why}")),
    }
}

/// The claude setup-token a claude seat signed in with OAuth passes through (core#1024).
pub(crate) fn claude_seat_token(gw: &Gw) -> Result<String, String> {
    let _lock = crate::seat_refresh::mutation_lock(&gw.dir)?;
    let files = gw.read_broker(&[("anthropic.token", false)])?;
    let token = files
        .into_iter()
        .next()
        .map(|(_, b)| b)
        .filter(|b| !b.is_empty())
        .ok_or("claude is not signed in")?;
    let token = String::from_utf8(token).map_err(|_| "claude: invalid broker token")?;
    let token = token.trim();
    if token.is_empty() || token.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("claude: invalid broker token".into());
    }
    Ok(token.to_string())
}
