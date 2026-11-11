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

    fn tag(self) -> &'static str {
        match self {
            Volume::Config => "cfg",
            Volume::Broker => "broker",
        }
    }
}

/// The `$0` marker every write of `<v>/<file>` runs under (followed by a unique suffix): the
/// next write of that file finds an earlier one still running by it ([`write_script`]).
pub(crate) fn write_marker(v: Volume, file: &str) -> String {
    format!("wicked-writer-W-{}-{file}-", v.tag())
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
        "{} t={d}/.{file}.$$; trap 'rm -f \"$t\"' EXIT; cat > \"$t\"; \
         [ \"$(sha256sum \"$t\" | cut -d' ' -f1)\" = {digest} ] || {{ echo 'wicked-writer: the body did not arrive whole' >&2; exit 65; }}; \
         mv -f \"$t\" {d}/{file}; trap - EXIT",
        fence(v, file)
    ))
}

/// The writer script that removes `<dir>/<file>` behind the same fence as a write of it (B-4: a
/// journal or a staged record, which no route names).
pub(crate) fn remove_script(v: Volume, file: &str) -> Result<String, String> {
    plain_name(file)?;
    Ok(format!("{} rm -f {}/{file}", fence(v, file), v.dir()))
}

/// The fence every write or removal of `<v>/<file>` runs behind (see [`write_script`]), ending
/// with the file's kernel lock held on fd 9.
fn fence(v: Volume, file: &str) -> String {
    let d = v.dir();
    let m = write_marker(v, file);
    let first = &m[..1];
    let rest = &m[1..];
    format!(
        // The fence (review r3): an earlier write of this file whose client timed out may still
        // be running in the writer (exec does not end it), and could rename a stale body over
        // this one after it lands. Every such write is killed first (`$0` is its marker; `[x]y`
        // keeps the pattern from matching this script's own text), and any rename it already
        // started is waited out, before this body is taken. The kernel lock is the barrier (review
        // r5): every write holds it from before its body to after its rename (busybox `flock` has no
        // `-w`: a bounded `-n` retry, 20 s, found by the B-4 live run), and a rename child
        // an old write forked holds it too, so no stale rename can land after this one. A killed
        // write's body (a credential, codex b2-gw #7 / b4-r2 #3) is removed under the lock: no
        // earlier write is alive by then to rename it.
        "set -e; umask 077; \
         for p in $(pgrep -f '[{first}]{rest}'); do [ \"$p\" = \"$$\" ] || kill -9 \"$p\" 2>/dev/null || :; done; \
         i=0; while pgrep -f '[m]v -f {d}/.{file}.' >/dev/null; do i=$((i+1)); [ $i -lt 200 ] || exit 68; sleep 0.05; done; \
         exec 9>{d}/.{file}.lock; i=0; until flock -n 9; do i=$((i+1)); [ $i -lt 400 ] || {{ echo 'wicked-writer: an earlier write still holds the file' >&2; exit 69; }}; sleep 0.05; done; \
         rm -f {d}/.{file}.[0-9]*;"
    )
}

/// The readiness probe script: the key on stdin, then up to 15 s until the gateway answers it
/// with `want` (` 200 ` served, ` 401 ` refused) on its readiness listener.
pub(crate) fn confirm_script(accepted: bool) -> String {
    let want = if accepted { "HTTP/1.1 200" } else { " 401 " };
    format!(
        "read K; i=0; while [ $i -lt 60 ]; do \
         o=$(wget -T 2 -S -O /dev/null --header \"x-api-key: $K\" http://127.0.0.1:{}/ 2>&1); \
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
        self.exec_marked("wicked-writer-", script, stdin, limit)
    }

    /// [`Gw::exec`] under `$0` = `prefix` + a unique suffix.
    fn exec_marked(
        &self,
        prefix: &str,
        script: &str,
        stdin: Option<&[u8]>,
        limit: Duration,
    ) -> Called {
        let marker = format!("{prefix}{}", uuid::Uuid::new_v4().simple());
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
        // One write at a time from this engine: the fence in the script ends any earlier write
        // of the same file, which must only ever be an orphan, never a sibling of ours.
        let _one = WRITES.lock().unwrap_or_else(|p| p.into_inner());
        let r = self.exec_marked(&write_marker(v, file), &script, Some(body), EXEC_TIMEOUT);
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

    /// Remove `<volume>/<file>` behind the write fence (a journal or a staged record).
    pub(crate) fn remove_file(&self, v: Volume, file: &str) -> Result<(), String> {
        let script = remove_script(v, file)?;
        let _one = WRITES.lock().unwrap_or_else(|p| p.into_inner());
        let r = self.exec_marked(&write_marker(v, file), &script, None, EXEC_TIMEOUT);
        if r.ok() {
            Ok(())
        } else {
            Err(format!(
                "the gateway writer could not remove {file}: {}",
                r.err_text()
            ))
        }
    }

    /// Whether the gateway Deployment is scaled to zero (no gateway starts on its own); `false`
    /// when it cannot be read.
    pub(crate) fn scaled_down(&self) -> bool {
        self.kube
            .get_json(Some(&self.ns), "deployment", render::GATEWAY_NAME)
            .ok()
            .flatten()
            .and_then(|d| {
                d.pointer("/spec/replicas")
                    .and_then(serde_json::Value::as_u64)
            })
            == Some(0)
    }

    /// Whether a gateway serves now, for a sign-out's confirmation ([`serving_state`]).
    pub(crate) fn serving_state(&self) -> Option<bool> {
        self.kube
            .list_owned(Some(&self.ns), "pods", &[(super::ROLE_LABEL, "gateway")])
            .ok()
            .and_then(|pods| serving_state(&pods))
    }

    /// One exec of `argv` in the gateway pod's `fetch` sidecar (the account reader), with
    /// `stdin`, its stdout capped at `cap`.
    pub(crate) fn fetch(&self, argv: &[String], stdin: &[u8], limit: Duration, cap: u64) -> Called {
        let mut args: Vec<&str> = vec!["exec", "-i", "-c", "fetch", self.pod.as_str(), "--"];
        args.extend(argv.iter().map(String::as_str));
        self.kube
            .call_capped(Some(&self.ns), &args, Some(stdin), limit, cap)
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

    /// This gateway as it runs now (review r6 #15): the pod is looked up again by its labels (a
    /// quarantine or an eviction replaces it), the serving pod first, else the one whose writer
    /// runs; unchanged when none can be read.
    pub(crate) fn current(&self) -> Gw {
        let pods =
            match self
                .kube
                .list_owned(Some(&self.ns), "pods", &[(super::ROLE_LABEL, "gateway")])
            {
                Ok(p) => p,
                Err(_) => return self.clone(),
            };
        match cluster::running_pod(&pods).or_else(|| cluster::writer_running(&pods)) {
            Some(pod) => Gw {
                pod,
                ..self.clone()
            },
            None => self.clone(),
        }
    }

    /// The gateway's access-log records for `unit` since `since` (unix seconds).
    pub(crate) fn records(&self, since: &str, unit: &str) -> Vec<GatewayRecord> {
        let since = since
            .parse::<u64>()
            .map(super::rfc3339)
            .unwrap_or_else(|_| super::rfc3339(0));
        let t = format!("--since-time={since}");
        // The container's previous run too (codex b2-gw #8): a gateway restart mid-unit keeps
        // the records before it. A pod replaced mid-unit still loses its predecessor's.
        let mut out = Vec::new();
        for previous in [true, false] {
            let mut a = vec!["logs", "-c", "gateway", self.pod.as_str(), t.as_str()];
            if previous {
                a.push("--previous");
            }
            let r = self.kube.call(Some(&self.ns), &a, None, EXEC_TIMEOUT);
            if !r.ok() {
                continue;
            }
            out.extend(crate::seat_gateway::parse_records(
                &String::from_utf8_lossy(&r.stdout),
                unit,
            ));
            out.extend(crate::seat_gateway::parse_records(
                &String::from_utf8_lossy(&r.stderr),
                unit,
            ));
        }
        out
    }
}

/// Whether a gateway serves (pure, review B-4 #5): `Some(true)` a pod serves (a generation can be
/// confirmed); `Some(false)` no gateway process is alive in any pod (none, or every gateway
/// container waiting or ended: nothing can still accept a pending key); `None` a gateway process
/// runs but is not ready (alive, so a pending revocation is never dropped on its account).
pub(crate) fn serving_state(pods: &[serde_json::Value]) -> Option<bool> {
    if cluster::running_pod(pods).is_some() {
        return Some(true);
    }
    let alive = pods.iter().any(|p| {
        p.pointer("/status/containerStatuses")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|cs| {
                cs.iter().any(|c| {
                    c.get("name").and_then(serde_json::Value::as_str) == Some("gateway")
                        && c.pointer("/state/running").is_some()
                })
            })
    });
    if alive {
        None
    } else {
        Some(false)
    }
}

/// Serializes this engine's writer writes ([`Gw::write_file`]).
static WRITES: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
    let cluster_pending = cluster_rotation_pending(
        kube.get_json(Some(&ns), "deployment", render::GATEWAY_NAME)
            .map_err(|e| (FloorUnarmed::ClusterUnreachable, e))?
            .as_ref(),
    );
    let spec = GatewaySpec {
        namespace: ns.clone(),
        owner: kube.owner.clone(),
        image: crate::seat_gateway::GATEWAY_IMAGE.into(),
        writer_image: crate::seat_gateway::HELPER_IMAGE.into(),
        // A quarantine whose rotation did not land is finished first (review r2): the
        // gateway never starts again on the token it was stopped under.
        boot: boot_for_start(dir, cluster_pending).map_err(gwerr)?,
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
    if cluster_pending {
        // The Deployment now runs on the rotated token: the owed rotation is paid.
        clear_cluster_rotation(&Gw {
            kube: kube.clone(),
            ns: ns.clone(),
            pod: pod.clone(),
            service_ip: String::new(),
            dir: dir.to_path_buf(),
        });
    }
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

/// The daemon's gateway for a sign-in (the `seat-login` CLI, B-4), never taking the daemon's
/// ownership: no config or boot token is published, no unit key is touched, and a quarantined
/// gateway is never restarted (its rotation is the daemon's to finish). `provision`: a first
/// sign-in creates the daemon's objects as [`ensure`] would (same boot token, the host ports the
/// egress policy already grants) when there is no gateway yet, so the broker's storage exists
/// before any run; the gateway container keeps waiting for the daemon's first config. An
/// existing gateway is never re-applied (a quarantine landing meanwhile stands). Without `provision`, `Ok(None)` when
/// the daemon has no gateway yet (nothing was ever signed in on this cluster).
pub(crate) fn attach(
    kube: &Kube,
    dir: &Path,
    provision: bool,
) -> Result<Option<Gw>, (FloorUnarmed, String)> {
    let gwerr = |e: String| (FloorUnarmed::GatewayUnavailable, e);
    let unreachable = |e: String| (FloorUnarmed::ClusterUnreachable, e);
    if !dir.is_absolute() {
        return Err(gwerr("the gateway directory must be absolute".into()));
    }
    let ns = daemon_namespace_name(&kube.owner);
    let quarantined = |dep: &Option<serde_json::Value>| {
        rotation_pending(dir)
            || cluster_rotation_pending(dep.as_ref())
            || dep
                .as_ref()
                .and_then(|d| d.pointer("/spec/replicas"))
                .and_then(serde_json::Value::as_u64)
                == Some(0)
    };
    let refused = || {
        gwerr(
            "the gateway is stopped after a revocation it could not confirm; the daemon \
             restarts it before its next seat; start a run, then try again"
                .into(),
        )
    };
    let dep = kube
        .get_json(Some(&ns), "deployment", render::GATEWAY_NAME)
        .map_err(unreachable)?;
    if quarantined(&dep) {
        return Err(refused());
    }
    if provision {
        let _broker = crate::seat_refresh::mutation_lock(dir).map_err(gwerr)?;
        let _transaction = crate::seat_gateway::config_transaction(dir).map_err(gwerr)?;
        let dep = kube
            .get_json(Some(&ns), "deployment", render::GATEWAY_NAME)
            .map_err(unreachable)?;
        if quarantined(&dep) {
            return Err(refused());
        }
        if let Some(d) = &dep {
            // Never re-applied here (review B-4 #2): only the daemon changes a gateway that
            // exists, so a quarantine that lands meanwhile is never undone by a sign-in.
            if !has_login_stores(d) {
                return Err(gwerr(
                    "the daemon's gateway predates sign-ins on the cluster; start a run so the \
                     daemon updates it, then sign in again"
                        .into(),
                ));
            }
        } else {
            let existing = kube
                .get_json(Some(&ns), "networkpolicy", "gateway-egress")
                .map_err(unreachable)?;
            let host: Vec<HostPort> = existing.as_ref().map(render_host_ports).unwrap_or_default();
            let spec = GatewaySpec {
                namespace: ns.clone(),
                owner: kube.owner.clone(),
                image: crate::seat_gateway::GATEWAY_IMAGE.into(),
                writer_image: crate::seat_gateway::HELPER_IMAGE.into(),
                boot: boot_token(dir, false).map_err(gwerr)?,
            };
            let (deployment, rest): (Vec<_>, Vec<_>) = cluster::daemon_docs(&spec, &host)
                .into_iter()
                .partition(|d| d["kind"] == "Deployment");
            kube.apply(&rest)
                .map_err(|e| (FloorUnarmed::PolicyRefused, e))?;
            // Created, never applied: one the daemon made (or quarantined) meanwhile stands.
            let body =
                serde_json::to_vec(&render::list(&deployment)).map_err(|e| gwerr(e.to_string()))?;
            let r = kube.call(
                None,
                &["create", "-f", "-"],
                Some(&body),
                super::CALL_TIMEOUT,
            );
            if !r.ok()
                && !r.err_text().contains("AlreadyExists")
                && !r.err_text().contains("already exists")
            {
                return Err((FloorUnarmed::PolicyRefused, r.err_text()));
            }
            let dep = kube
                .get_json(Some(&ns), "deployment", render::GATEWAY_NAME)
                .map_err(unreachable)?;
            if quarantined(&dep) {
                return Err(refused());
            }
        }
    } else if dep.is_none() {
        return Ok(None);
    }
    let pod = cluster::writer_pod(kube, &ns, READY_TIMEOUT).map_err(gwerr)?;
    Ok(Some(Gw {
        kube: kube.clone(),
        ns,
        pod,
        service_ip: String::new(),
        dir: dir.to_path_buf(),
    }))
}

/// Whether a gateway Deployment mounts the login stores (pure).
pub(crate) fn has_login_stores(dep: &serde_json::Value) -> bool {
    dep.pointer("/spec/template/spec/volumes")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|v| {
            v.iter().any(|x| {
                x.pointer("/persistentVolumeClaim/claimName")
                    .and_then(serde_json::Value::as_str)
                    == Some(render::GATEWAY_LOGINS_PVC)
            })
        })
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
    // Stopped first, whatever the host state allows (review r2, r4): the rotation owed is
    // recorded on the host when it can be (review r3) AND on the Deployment itself, in the same
    // patch that scales it to none, so an engine that dies (or a host that cannot write) still
    // leaves the next start owing it. A patch of the Deployment, never `scale`: the engine's
    // account holds no `deployments/scale` (review r4).
    let _ = begin_rotation(&gw.dir);
    let body = quarantine_patch().to_string();
    let r = gw.kube.call(
        Some(&gw.ns),
        &[
            "patch",
            "deployment",
            render::GATEWAY_NAME,
            "--type",
            "merge",
            "-p",
            &body,
        ],
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
                    let _ = end_rotation(&gw.dir);
                    clear_cluster_rotation(gw);
                }
                return true;
            }
            _ => std::thread::sleep(Duration::from_millis(500)),
        }
    }
    false
}

/// The marker of a boot-token rotation a quarantine owes (`config/rotate-pending` of the gateway
/// dir): [`ensure`] rotates first, and refuses to start the gateway until it can. On disk, not in
/// the process (review r3): it outlives an engine that died mid-quarantine.
fn rotation_marker(dir: &Path) -> PathBuf {
    dir.join("config/rotate-pending")
}

/// The annotation a quarantine leaves on the gateway Deployment while its token rotation is owed.
pub(crate) const ROTATE_ANNOTATION: &str = "wicked.dev/rotate-pending";

/// The merge patch that stops the gateway and records the rotation it owes (pure).
pub(crate) fn quarantine_patch() -> serde_json::Value {
    serde_json::json!({
        "metadata": {"annotations": {ROTATE_ANNOTATION: "1"}},
        "spec": {"replicas": 0},
    })
}

/// Whether the gateway Deployment `dep` carries an owed rotation (pure).
pub(crate) fn cluster_rotation_pending(dep: Option<&serde_json::Value>) -> bool {
    dep.and_then(|d| d.pointer("/metadata/annotations"))
        .and_then(|a| a.get(ROTATE_ANNOTATION))
        .is_some()
}

/// Drop the Deployment's owed-rotation annotation (the new token is on disk). A failure leaves
/// it, and the next start rotates once more: never fewer rotations, only more.
fn clear_cluster_rotation(gw: &Gw) {
    let body = serde_json::json!({"metadata": {"annotations": {ROTATE_ANNOTATION: null}}});
    let _ = gw.kube.call(
        Some(&gw.ns),
        &[
            "patch",
            "deployment",
            render::GATEWAY_NAME,
            "--type",
            "merge",
            "-p",
            &body.to_string(),
        ],
        None,
        EXEC_TIMEOUT,
    );
}

/// The boot token a gateway (re)starts under: a fresh one when a quarantine still owes its
/// rotation (whichever engine began it; recorded on the host or, `cluster_pending`, on the
/// Deployment), else the current one.
pub(crate) fn boot_for_start(dir: &Path, cluster_pending: bool) -> Result<String, String> {
    let pending = rotation_pending(dir) || cluster_pending;
    let t = boot_token(dir, pending)?;
    if pending {
        end_rotation(dir)?;
    }
    Ok(t)
}

/// Whether a quarantine of `dir`'s gateway still owes its token rotation.
pub(crate) fn rotation_pending(dir: &Path) -> bool {
    std::fs::symlink_metadata(rotation_marker(dir)).is_ok()
}

/// Record that `dir`'s gateway owes a rotation (before it is stopped).
pub(crate) fn begin_rotation(dir: &Path) -> Result<(), String> {
    state_write(dir, &rotation_marker(dir), b"quarantined")
}

/// The gateway dir's `config` dir: a real directory (made when absent), never a link
/// (codex b2-gw #1).
fn state_dir(dir: &Path) -> Result<PathBuf, String> {
    let c = dir.join("config");
    std::fs::create_dir_all(&c).map_err(|e| e.to_string())?;
    match std::fs::symlink_metadata(&c) {
        Ok(m) if m.file_type().is_dir() => Ok(c),
        _ => Err(format!("{} is not a directory", c.display())),
    }
}

/// Replace one gateway state file: an owner-only file created new beside it, renamed over it
/// (a link planted at the name is replaced, never followed).
fn state_write(dir: &Path, path: &Path, body: &[u8]) -> Result<(), String> {
    let c = state_dir(dir)?;
    let tmp = c.join(format!(".state-{}", uuid::Uuid::new_v4().simple()));
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let wrote = o
        .open(&tmp)
        .and_then(|mut f| std::io::Write::write_all(&mut f, body))
        .and_then(|()| std::fs::rename(&tmp, path));
    if let Err(e) = wrote {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("{}: {e}", path.display()));
    }
    Ok(())
}

/// A small gateway state file, only when it is a regular file (no link followed), bounded.
fn state_read(path: &Path) -> Option<String> {
    use std::io::Read as _;
    std::fs::symlink_metadata(path)
        .ok()
        .filter(|m| m.file_type().is_file() && m.len() <= 256)?;
    let mut s = String::new();
    std::fs::File::open(path)
        .ok()?
        .take(256)
        .read_to_string(&mut s)
        .ok()?;
    Some(s)
}

/// The rotation is done (the new token is on disk).
pub(crate) fn end_rotation(dir: &Path) -> Result<(), String> {
    match std::fs::remove_file(rotation_marker(dir)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
        _ => Ok(()),
    }
}

/// The gateway's boot token (`config/boot-token` of the gateway dir), created when absent or when
/// `fresh` (a quarantine).
pub(crate) fn boot_token(dir: &Path, fresh: bool) -> Result<String, String> {
    let path = dir.join("config/boot-token");
    if !fresh {
        if let Some(t) = state_read(&path) {
            let t = t.trim();
            // Only a token this engine wrote (a uuid's 32 hex digits).
            if t.len() == 32 && t.chars().all(|c| c.is_ascii_hexdigit()) {
                return Ok(t.to_string());
            }
        }
    }
    let t = uuid::Uuid::new_v4().simple().to_string();
    state_write(dir, &path, t.as_bytes())?;
    Ok(t)
}

/// The sign-in a governed seat of `cli` runs on, read through the writer by the one sign-in
/// state machine (`seat_login::seat_sign_in`): an interrupted sign-in, sign-out or refresh is
/// settled first (C4). Signed out, expired or unreadable = the reason.
#[cfg(unix)]
pub(crate) fn seat_sign_in(
    gw: &Gw,
    cli: &str,
) -> Result<crate::seat_credentials::SeatSignIn, String> {
    crate::seat_login::seat_sign_in(&super::login::K8sIo::of(gw), &gw.dir, cli)
}

/// The claude setup-token a claude seat signed in with OAuth passes through (core#1024).
#[cfg(unix)]
pub(crate) fn claude_seat_token(gw: &Gw) -> Result<String, String> {
    crate::seat_login::claude_seat_token(&super::login::K8sIo::of(gw), &gw.dir)
}

/// Native Windows runs no seats (the seat runner needs a Unix host).
#[cfg(not(unix))]
pub(crate) fn seat_sign_in(
    _gw: &Gw,
    _cli: &str,
) -> Result<crate::seat_credentials::SeatSignIn, String> {
    Err("seat sign-ins need a Unix host".into())
}

/// Native Windows runs no seats (the seat runner needs a Unix host).
#[cfg(not(unix))]
pub(crate) fn claude_seat_token(_gw: &Gw) -> Result<String, String> {
    Err("seat sign-ins need a Unix host".into())
}
