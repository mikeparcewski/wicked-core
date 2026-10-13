//! The model gateway: one agentgateway container per daemon, attached as `agw` to each seat's
//! own internal network. Its external bridge is the seats' only route out; the CONNECT listener
//! admits only the configured SNI hostnames. Host-loopback routes belong to a later slice.
//!
//! **Glue, not a runtime.** Pure functions render the v1.6.0 config and the runtime CLI argv;
//! bounded child processes apply them. The broker's credentials stay in `/broker`, mounted only
//! into the gateway. Seats receive random, revocable unit keys, never the provider's key.
//!
//! **Reload by rename.** `config/gateway.json` persists the model routes and hostname allowlist;
//! `config/units.json` owns the live unit keys. Every change replaces `config/config.json` in the
//! same directory, so agentgateway's watcher sees a complete document. A process-wide lock
//! serializes provisioning, startup and key changes. The daemon must pass the same directory
//! throughout its lifetime (the directory is also the input to [`gateway_name`]).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::container_floor::{output_within, Runtime};
use std::process::{Output, Stdio};
use std::time::{Duration, Instant};

/// The OSS gateway image whose config shape the fixture proves.
pub(crate) const GATEWAY_IMAGE: &str = "ghcr.io/agentgateway/agentgateway:v1.6.0";
/// The gateway label, distinct from the seat label so a seat sweep cannot reap it.
pub(crate) const GATEWAY_LABEL: &str = "wicked.gateway";

static CONFIG_LOCK: Mutex<()> = Mutex::new(());
const CLI_TIMEOUT: Duration = Duration::from_secs(15);
const START_TIMEOUT: Duration = Duration::from_secs(60);
/// The engine's readiness listener: the served key list, answered by the gateway itself (no
/// upstream). Never a seat port; probed from inside the gateway's own network namespace.
pub(crate) const READY_PORT: u16 = 4999;

/// The config input, independent of the runtime CLI and agentgateway's wire representation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GatewayConfig {
    pub(crate) units: Vec<UnitKey>,
    pub(crate) providers: Vec<ModelRoute>,
    pub(crate) egress_hosts: Vec<String>,
    /// `host:port` of the engine's gate-hook relay as the gateway reaches it (the host's
    /// loopback through the runtime's host alias); `None` = no hook route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) hook_upstream: Option<String>,
    /// `host:port` of crew's MCP broker as the gateway reaches it; `None` = no MCP route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) mcp_upstream: Option<String>,
}

/// A seat's revocable placeholder key and the identity authenticated requests carry.
/// Its `Debug` implementation redacts the key so diagnostics cannot print it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct UnitKey {
    pub(crate) key: String,
    pub(crate) run: String,
    pub(crate) unit: String,
    pub(crate) seat: String,
}

impl std::fmt::Debug for UnitKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnitKey")
            .field("key", &"[redacted]")
            .field("run", &self.run)
            .field("unit", &self.unit)
            .field("seat", &self.seat)
            .finish()
    }
}

impl UnitKey {
    /// Mint a fresh `wk-` + 32 hex key for one governed unit.
    pub(crate) fn new(run: &str, unit: &str, seat: &str) -> Self {
        Self {
            key: format!("wk-{}", uuid::Uuid::new_v4().simple()),
            run: run.into(),
            unit: unit.into(),
            seat: seat.into(),
        }
    }
}

/// One model listener and its broker-owned provider credential path inside the gateway.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ModelRoute {
    pub(crate) port: u16,
    pub(crate) provider: Provider,
    /// Empty = the provider's own endpoint (agentgateway's `ai` backend defaults it, with TLS);
    /// otherwise a `host:port` override (a stub, a regional or private endpoint).
    pub(crate) upstream_host: String,
    /// TLS to an overridden upstream (`backendTLS: {}`; agentgateway
    /// examples/traffic-cross-app-access/okta-auth0/gateway.yaml:31-35).
    pub(crate) tls: bool,
    pub(crate) credential_file: PathBuf,
}

/// The native API each model listener preserves. The seat-env helper uses the standard ports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Provider {
    Anthropic,
    OpenAI,
}

impl Provider {
    pub(crate) fn port(self) -> u16 {
        match self {
            Self::Anthropic => 4000,
            Self::OpenAI => 4001,
        }
    }

    /// The provider's variant name in agentgateway's config (`openAI`, not `openai`).
    fn config_key(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAI => "openAI",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAI => "openai",
        }
    }
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            units: Vec::new(),
            providers: [Provider::Anthropic, Provider::OpenAI]
                .into_iter()
                .map(|provider| ModelRoute {
                    port: provider.port(),
                    provider,
                    upstream_host: String::new(),
                    tls: true,
                    credential_file: PathBuf::from(format!("/broker/{}.key", provider.name())),
                })
                .collect(),
            // `WICKED_SEAT_EGRESS_HOSTS`: the hostnames (comma-separated) seats may reach
            // through CONNECT, e.g. package registries. None by default.
            egress_hosts: std::env::var("WICKED_SEAT_EGRESS_HOSTS")
                .unwrap_or_default()
                .split(',')
                .map(|h| h.trim().to_string())
                .filter(|h| !h.is_empty())
                .collect(),
            hook_upstream: None,
            mcp_upstream: None,
        }
    }
}

/// Render JSON (also valid YAML), preserving the proven Anthropic fixture's policies and body
/// routes. Empty keys still render `strict`: no model request becomes anonymous at idle.
pub(crate) fn render(config: &GatewayConfig) -> Value {
    let keys: Vec<Value> = config
        .units
        .iter()
        .map(|u| {
            json!({
                "key": u.key,
                "metadata": {
                    "name": format!("unit-{}", u.unit),
                    "run": u.run, "unit": u.unit, "seat": u.seat
                }
            })
        })
        .collect();
    let mut binds: Vec<Value> = config
        .providers
        .iter()
        .map(|route| {
            let routes = match route.provider {
                Provider::Anthropic => json!({
                    "/v1/messages": "messages",
                    "/v1/messages/count_tokens": "anthropicTokenCount",
                    "*": "passthrough"
                }),
                Provider::OpenAI => json!({
                    "/v1/chat/completions": "completions",
                    "/v1/responses": "responses",
                    "*": "passthrough"
                }),
            };
            let mut provider = serde_json::Map::new();
            provider.insert(route.provider.config_key().into(), json!({}));
            let location = match route.provider {
                Provider::Anthropic => json!({"header": {"name": "x-api-key"}}),
                // v1.6.0 schema/config.json: LocalAPIKeys.location / AuthorizationLocation.
                // Native OpenAI clients send Authorization: Bearer <placeholder>.
                Provider::OpenAI => {
                    json!({"header": {"name": "authorization", "prefix": "Bearer "}})
                }
            };
            let mut policies = json!({
                "apiKey": {
                    "mode": "strict",
                    "location": location,
                    "keys": keys
                },
                "ai": {"routes": routes}
            });
            // The broker file always exists (created empty at gateway start); until the agent is
            // signed in it is empty, and the provider answers with its own 401.
            policies["backendAuth"] = json!({"key": {"file": route.credential_file}});
            let mut backend = json!({"name": route.provider.name(), "provider": provider});
            if !route.upstream_host.is_empty() {
                backend["hostOverride"] = json!(route.upstream_host);
                if route.tls {
                    policies["backendTLS"] = json!({});
                }
            }
            json!({
                "port": route.port,
                "listeners": [{ "routes": [{
                    "policies": policies,
                    "backends": [{"ai": backend}]
                }]}]
            })
        })
        .collect();
    binds.push(json!({"port": 3000, "tunnelProtocol": "connect", "listeners": []}));
    // Crew's MCP broker: exactly its two unit-token routes (the token in the body is the
    // credential, verified by the engine); every other broker route stays unreachable.
    if let Some(up) = &config.mcp_upstream {
        binds.push(json!({
            "port": 8700,
            "listeners": [{ "routes": [{
                "matches": [
                    {"path": {"exact": "/api/v1/mcp/call"}, "method": "POST"},
                    {"path": {"exact": "/api/v1/mcp/tools"}, "method": "POST"}
                ],
                "backends": [{"host": up}]
            }]}]
        }));
    }
    // Readiness: the same strict key list, answered directly (agentgateway applies apiKey before
    // directResponse, proxy/httpproxy.rs), so a 200 for a key proves the served config has it.
    binds.push(json!({
        "port": READY_PORT,
        "listeners": [{ "routes": [{ "policies": {
            "apiKey": {"mode": "strict", "location": {"header": {"name": "x-api-key"}}, "keys": keys},
            "directResponse": {"status": 200, "body": "ready"}
        }}]}]
    }));
    // The seat's gate hook: `/hook/<token>` to the engine's relay; the token in the path is the
    // relay's credential (unknown tokens are denied there).
    if let Some(up) = &config.hook_upstream {
        binds.push(json!({
            "port": crate::hook_relay::HOOK_PORT,
            "listeners": [{ "routes": [{
                "matches": [{"path": {"pathPrefix": "/hook/"}}],
                "backends": [{"host": up}]
            }]}]
        }));
    }
    json!({
        "config": {
            "adminAddr": "off",
            "sensitiveHeaders": ["x-api-key", "authorization"],
            "logging": {"level": "info", "format": "json"}
        },
        "frontendPolicies": {"accessLog": {"add": {
            "wicked_unit": "default(apiKey.unit, \"none\")",
            "wicked_run": "default(apiKey.run, \"none\")",
            "wicked_seat": "default(apiKey.seat, \"none\")",
            "llm_model": "llm.requestModel",
            "llm_tools": "llm.toolCalls",
            "llm_text": "llm.completion",
            "llm_in": "llm.inputTokens",
            "llm_out": "llm.outputTokens"
        }}},
        "binds": binds,
        "gateways": {"secure": {"port": 443, "listeners": [{
            "name": "public-egress", "hostname": "*", "protocol": "TLS"
        }]}},
        // An empty hostname list can mean match-all to a router. Omit the route entirely when
        // nothing is allowed, keeping the idle CONNECT listener closed.
        "tcpRoutes": if config.egress_hosts.is_empty() { vec![] } else { vec![json!({
            "name": "egress-allowlist",
            "gateways": "secure/public-egress",
            "hostnames": config.egress_hosts,
            "backends": [{"dynamic": {}}]
        })] }
    })
}

/// A gateway error line, kept only when it carries no credential shape.
fn redact_tail(line: &str) -> String {
    if redact_check(line) {
        line.chars().take(400).collect()
    } else {
        "(redacted)".into()
    }
}

/// A stable container name for the daemon's explicit state directory, without reading it.
pub(crate) fn gateway_name(state_home: &Path) -> String {
    let hash = Sha256::digest(state_home.as_os_str().as_encoded_bytes());
    let hex: String = hash[..6].iter().map(|b| format!("{b:02x}")).collect();
    format!("wicked-agw-{hex}")
}

/// The gateway and its normal (non-internal) external bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Gateway {
    pub(crate) name: String,
    pub(crate) external_network: String,
}

impl Gateway {
    fn for_dir(dir: &Path) -> Self {
        let name = gateway_name(dir);
        Self {
            external_network: format!("{name}-ext"),
            name,
        }
    }
}

/// The detached gateway argv, with only the config and broker directories mounted; no port is
/// published on the host. Like the seat renderer, the first element is the runtime CLI.
pub(crate) fn run_argv(rt: &Path, gw: &Gateway) -> Vec<String> {
    vec![
        rt.to_string_lossy().into_owned(),
        "run".into(),
        "-d".into(),
        "--name".into(),
        gw.name.clone(),
        "--network".into(),
        gw.external_network.clone(),
        "--label".into(),
        format!("{GATEWAY_LABEL}=1"),
        "--user".into(),
        "10001".into(),
        "--read-only".into(),
        "--cap-drop".into(),
        "ALL".into(),
        "--security-opt".into(),
        "no-new-privileges".into(),
        "--pids-limit".into(),
        "512".into(),
        "--memory".into(),
        "512m".into(),
        "--mount".into(),
        format!("type=volume,src={},dst=/cfg,readonly", volumes(gw).0),
        "--mount".into(),
        format!("type=volume,src={},dst=/broker,readonly", volumes(gw).1),
        GATEWAY_IMAGE.into(),
        "-f".into(),
        "/cfg/config.json".into(),
    ]
}

/// Attach/detach argv are pure too: each seat keeps its own network and resolves only `agw`.
pub(crate) fn attach_argv(gw: &Gateway, network: &str) -> Vec<String> {
    ["network", "connect", "--alias", "agw", network, &gw.name]
        .into_iter()
        .map(str::to_string)
        .collect()
}

/// Only model proxy values and the placeholder key enter the seat: 4000 (Anthropic) / 4001
/// (OpenAI).
pub(crate) fn seat_env(provider: Provider, key: &str) -> Vec<(String, String)> {
    env_at_port(provider, provider.port(), key)
}

fn env_at_port(provider: Provider, port: u16, key: &str) -> Vec<(String, String)> {
    let (base, api_key, url) = match provider {
        Provider::Anthropic => (
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_API_KEY",
            format!("http://agw:{port}"),
        ),
        Provider::OpenAI => (
            "OPENAI_BASE_URL",
            "OPENAI_API_KEY",
            format!("http://agw:{port}/v1"),
        ),
    };
    vec![
        (base.into(), url),
        (api_key.into(), key.into()),
        ("HTTPS_PROXY".into(), "http://agw:3000".into()),
        ("HTTP_PROXY".into(), "http://agw:3000".into()),
        ("NO_PROXY".into(), "agw".into()),
    ]
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, String> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|e| {
            // Deserializer diagnostics can contain the offending string (a unit key).
            format!(
                "invalid JSON in {} at {}:{}",
                path.display(),
                e.line(),
                e.column()
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("could not read {}: {e}", path.display())),
    }
}

/// Replace a whole file by a same-directory rename; never truncate the gateway's watched file.
fn atomic_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let parent = path.parent().ok_or("config file has no parent")?;
    fs::create_dir_all(parent)
        .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // The gateway mounts this directory directly, so the daemon state directory may stay
        // private. A restrictive daemon umask must not deny uid 10001 the mounted config.
        fs::set_permissions(parent, fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("could not make gateway config directory readable: {e}"))?;
    }
    let bytes =
        serde_json::to_vec_pretty(value).map_err(|_| "could not serialize gateway config")?;
    let temp = parent.join(format!(".gateway-{}.tmp", uuid::Uuid::new_v4().simple()));
    let replace = || -> std::io::Result<()> {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        // The gateway's unprivileged uid must read the config; this directory is never mounted
        // into a seat. Broker credential modes are untouched.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o644);
        }
        let mut f = opts.open(&temp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = if path.file_name() == Some(std::ffi::OsStr::new("units.json"))
                || path.file_name() == Some(std::ffi::OsStr::new("revoking.json"))
            {
                0o600
            } else {
                0o644
            };
            f.set_permissions(fs::Permissions::from_mode(mode))?;
        }
        f.write_all(&bytes)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&temp, path)
    };
    let result = replace().map_err(|e| format!("could not replace {}: {e}", path.display()));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn load_config(dir: &Path) -> Result<GatewayConfig, String> {
    let mut config: GatewayConfig =
        read_json(&dir.join("config/gateway.json"))?.unwrap_or_default();
    config.units = read_json(&dir.join("config/units.json"))?.unwrap_or_default();
    Ok(config)
}

/// `config` as served: with this process's gate-hook route when the relay was reached.
fn effective(config: &GatewayConfig) -> GatewayConfig {
    let mut c = config.clone();
    if c.hook_upstream.is_none() {
        c.hook_upstream = HOOK_UPSTREAM.lock().ok().and_then(|c| c.clone());
    }
    if c.mcp_upstream.is_none() {
        c.mcp_upstream = MCP_UPSTREAM.lock().ok().and_then(|c| c.clone());
    }
    c
}

/// Crew's MCP broker as this process's gateway reaches it, probed once (see [`probe_mcp`]).
static MCP_UPSTREAM: Mutex<Option<String>> = Mutex::new(None);

/// A route probe whose FOUND address is kept for the process; a miss is not (agy gw-r1 F-05: a
/// transient failure, a cold helper image, must not refuse every later claude unit).
fn probe_once(
    cell: &Mutex<Option<String>>,
    probe: impl FnOnce() -> Option<String>,
) -> Option<String> {
    if let Some(up) = cell.lock().ok().and_then(|c| c.clone()) {
        return Some(up);
    }
    let found = probe();
    if let (Some(up), Ok(mut c)) = (&found, cell.lock()) {
        *c = Some(up.clone());
    }
    found
}

/// The MCP broker URL a container seat is handed (crew's two unit-token routes only).
pub(crate) const MCP_URL: &str = "http://agw:8700";

/// The port of the daemon's broker URL (`WICKED_CREW_URL`), when it names a loopback host: the
/// gateway reaches the host's loopback only through the runtime's host alias.
fn crew_port(url: &str) -> Option<u16> {
    let rest = url.trim().strip_prefix("http://")?;
    let authority = rest.split('/').next()?;
    let (host, port) = authority.rsplit_once(':')?;
    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
        .then(|| port.parse().ok())
        .flatten()
}

/// Find which host alias reaches crew's broker from the gateway's network (any HTTP answer from
/// `/api/v1/health`), once per process. `None` when the daemon has no loopback broker URL.
fn probe_mcp(rt: &Runtime, gw: &Gateway) -> Option<String> {
    probe_once(&MCP_UPSTREAM, || {
        let port = crew_port(&crate::mcp_gate::daemon_crew_url()?)?;
        HOST_ALIASES.iter().find_map(|alias| {
            let up = format!("{alias}:{port}");
            let argv: Vec<String> = [
                "run",
                "--rm",
                "--network",
                &gw.external_network,
                "--cap-drop",
                "ALL",
                HELPER_IMAGE,
                "sh",
                "-c",
                &format!("wget -S -T 3 -O /dev/null http://{up}/api/v1/health 2>&1"),
            ]
            .into_iter()
            .map(str::to_string)
            .collect();
            cli(rt, &argv, CLI_TIMEOUT)
                .filter(|o| String::from_utf8_lossy(&o.stdout).contains("HTTP/1."))
                .map(|_| up)
        })
    })
}

/// Whether this process's gateway carries the MCP broker route.
pub(crate) fn mcp_route_ready() -> bool {
    MCP_UPSTREAM.lock().is_ok_and(|c| c.is_some())
}

/// The relay's address as this process's gateway reaches it, probed once (see [`probe_hook`]).
static HOOK_UPSTREAM: Mutex<Option<String>> = Mutex::new(None);

/// The host aliases the container desktops give for the host's loopback, in probe order.
const HOST_ALIASES: &[&str] = &[
    "host.lima.internal",
    "host.docker.internal",
    "host.containers.internal",
    "host.rancher-desktop.internal",
];

/// Find which host alias reaches the engine's gate-hook relay from the gateway's network (a
/// `GET /ping` from the helper image), once per process. `None` when no alias answers.
fn probe_hook(rt: &Runtime, gw: &Gateway) -> Option<String> {
    probe_once(&HOOK_UPSTREAM, || {
        let port = crate::hook_relay::port()?;
        HOST_ALIASES.iter().find_map(|alias| {
            let up = format!("{alias}:{port}");
            let argv: Vec<String> = [
                "run",
                "--rm",
                "--network",
                &gw.external_network,
                "--cap-drop",
                "ALL",
                HELPER_IMAGE,
                "wget",
                "-q",
                "-T",
                "3",
                "-O",
                "-",
                &format!("http://{up}/ping"),
            ]
            .into_iter()
            .map(str::to_string)
            .collect();
            cli(rt, &argv, CLI_TIMEOUT)
                .filter(|o| {
                    o.status.success() && String::from_utf8_lossy(&o.stdout).contains("pong")
                })
                .map(|_| up)
        })
    })
}

/// Whether this process's gateway carries the gate-hook route (the relay answered).
pub(crate) fn hook_route_ready() -> bool {
    HOOK_UPSTREAM.lock().is_ok_and(|c| c.is_some())
}

fn persist_units(dir: &Path, config: &GatewayConfig) -> Result<(), String> {
    // Authoritative units first; a failed config replacement is reported and a retry re-renders
    // from this list. Successful deregistration guarantees the served config no longer has it.
    atomic_json(&dir.join("config/units.json"), &config.units)?;
    atomic_json(&dir.join("config/config.json"), &render(&effective(config)))
}

/// The env a seat needs to reach every model route and the CONNECT allowlist through `agw`,
/// holding only its placeholder `key` (one entry per variable).
pub(crate) fn seat_env_all(key: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for p in [Provider::Anthropic, Provider::OpenAI] {
        for (k, v) in seat_env(p, key) {
            if !out.iter().any(|(e, _)| *e == k) {
                out.push((k, v));
            }
        }
    }
    out
}

/// (tests) Provision routes and allowlisted hosts without losing live registrations. Only the routes
/// and hosts from `config` are used; keys are owned by register/deregister, never provisioning.
#[cfg(test)]
pub(crate) fn configure_gateway(dir: &Path, config: &GatewayConfig) -> Result<(), String> {
    let _one = CONFIG_LOCK
        .lock()
        .map_err(|_| "gateway config lock poisoned")?;
    let units = read_json(&dir.join("config/units.json"))?.unwrap_or_default();
    let mut settings = config.clone();
    settings.units.clear();
    atomic_json(&dir.join("config/gateway.json"), &settings)?;
    settings.units = units;
    persist_units(dir, &settings)
}

/// Add (or replace) one unit key and atomically publish the new strict auth policy.
pub(crate) fn register_unit(dir: &Path, unit: UnitKey) -> Result<(), String> {
    let _one = CONFIG_LOCK
        .lock()
        .map_err(|_| "gateway config lock poisoned")?;
    let mut config = load_config(dir)?;
    config.units.retain(|u| u.key != unit.key);
    config.units.push(unit);
    persist_units(dir, &config)
}

/// Revoke one key. Repeating a revocation is harmless; all other keys and routes survive. The key
/// is recorded as PENDING revocation (`config/revoking.json`) before it leaves the served list,
/// and stays there until the gateway is seen refusing it ([`settle_revoking`]) or the gateway
/// container is gone (codex gw-r4 P2: a failed publish, a crash or a failed quarantine must not
/// lose the one record of a key the gateway may still accept).
pub(crate) fn deregister_unit(dir: &Path, key: &str) -> Result<(), String> {
    let _one = CONFIG_LOCK
        .lock()
        .map_err(|_| "gateway config lock poisoned")?;
    let mut config = load_config(dir)?;
    mark_revoking(dir, [key.to_string()])?;
    config.units.retain(|u| u.key != key);
    persist_units(dir, &config)
}

fn revoking_path(dir: &Path) -> PathBuf {
    dir.join("config/revoking.json")
}

fn load_revoking(dir: &Path) -> Result<Vec<String>, String> {
    Ok(read_json(&revoking_path(dir))?.unwrap_or_default())
}

/// Add `keys` to the pending revocations (callers hold `CONFIG_LOCK`).
fn mark_revoking(dir: &Path, keys: impl IntoIterator<Item = String>) -> Result<(), String> {
    let mut pending = load_revoking(dir)?;
    for k in keys {
        if !pending.contains(&k) {
            pending.push(k);
        }
    }
    atomic_json(&revoking_path(dir), &pending)
}

/// Re-render the served config from the authoritative state with every pending key left out,
/// returning the pending keys (callers hold `CONFIG_LOCK`). A crash between the two writes of
/// [`persist_units`] can leave a rendered config that still authorizes a pending key (codex
/// gw-r5 P2): it is never republished as found.
fn rerender_without_pending(dir: &Path) -> Result<Vec<String>, String> {
    let pending = load_revoking(dir)?;
    if pending.is_empty() {
        return Ok(pending);
    }
    let mut config = load_config(dir)?;
    config.units.retain(|u| !pending.contains(&u.key));
    persist_units(dir, &config)?;
    Ok(pending)
}

/// Publish the served config and confirm the gateway refuses every pending revocation; only
/// then are they cleared. `config_locked`: the caller already holds `CONFIG_LOCK`. Callers hold
/// `PUBLISH_LOCK`.
fn settle_revoking(
    rt: &Runtime,
    gw: &Gateway,
    dir: &Path,
    config_locked: bool,
) -> Result<(), String> {
    let lock = || -> Result<Option<std::sync::MutexGuard<'static, ()>>, String> {
        if config_locked {
            Ok(None)
        } else {
            CONFIG_LOCK
                .lock()
                .map(Some)
                .map_err(|_| "gateway config lock poisoned".to_string())
        }
    };
    let pending = {
        let _one = lock()?;
        rerender_without_pending(dir)?
    };
    if pending.is_empty() {
        return Ok(());
    }
    publish(rt, gw, dir)?;
    for key in &pending {
        confirm_key(rt, gw, key, false)?;
    }
    let _one = lock()?;
    let mut now = load_revoking(dir)?;
    now.retain(|k| !pending.contains(k));
    atomic_json(&revoking_path(dir), &now)
}

/// The pinned OSS helper image that writes into the gateway's volumes from inside the runtime's
/// VM. A write from the macOS side of a bind mount never reaches the gateway's file watcher
/// (inotify does not cross virtiofs), so every config change is published from in there.
pub(crate) const HELPER_IMAGE: &str = "busybox:1.37";

/// The gateway's named volumes: its served config and the credential broker.
pub(crate) fn volumes(gw: &Gateway) -> (String, String) {
    (format!("{}-cfg", gw.name), format!("{}-broker", gw.name))
}

/// Publish `dir/config/config.json` (the rendered config) into the gateway's config volume by
/// an atomic rename inside the VM, which the gateway's watcher sees and reloads.
pub(crate) fn publish(rt: &Runtime, gw: &Gateway, dir: &Path) -> Result<(), String> {
    let body = fs::read(dir.join("config/config.json"))
        .map_err(|e| format!("could not read the rendered gateway config: {e}"))?;
    write_into_volume(rt, &volumes(gw).0, "config.json", &body)
}

/// Store a provider credential in the broker (`/broker/<file>`), the only place it lives: the
/// gateway reads it to authenticate upstream; no seat ever mounts the broker.
#[cfg_attr(any(not(test), not(unix)), allow(dead_code))] // the C4 sign-in flow is its production caller; its tests are unix-only
pub(crate) fn set_broker_credential(
    rt: &Runtime,
    gw: &Gateway,
    file: &str,
    secret: &[u8],
) -> Result<(), String> {
    if file.is_empty()
        || !file
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
    {
        return Err(format!("`{file}` is not a broker file name"));
    }
    write_into_volume(rt, &volumes(gw).1, file, secret)
}

/// Write `body` to `<volume>/<file>` from inside the runtime's VM (stdin, never argv), by an
/// atomic rename the gateway's watcher sees. `file` is a plain name (checked by the callers).
fn write_into_volume(rt: &Runtime, volume: &str, file: &str, body: &[u8]) -> Result<(), String> {
    use std::io::Write as _;
    let mut cmd = rt.cmd();
    cmd.args([
        "run",
        "--rm",
        "-i",
        "--network",
        "none",
        "--user",
        "10001",
        "--cap-drop",
        "ALL",
        "-v",
        &format!("{volume}:/v"),
        HELPER_IMAGE,
        "sh",
        "-c",
        &format!("umask 077; cat > /v/.{file}.new && mv /v/.{file}.new /v/{file}"),
    ]);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("could not start the config publisher: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(body)
            .map_err(|e| format!("could not stream into the gateway volume: {e}"))?;
    }
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(st)) if st.success() => return Ok(()),
            Ok(Some(st)) => return Err(format!("writing {file} into {volume} failed ({st})")),
            Ok(None) if start.elapsed() < START_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(50))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("writing {file} into {volume} timed out"));
            }
        }
    }
}

/// Serializes register/deregister WITH their publish, so the served config is never older than
/// the last change (two units racing could otherwise publish out of order and drop a key).
static PUBLISH_LOCK: Mutex<()> = Mutex::new(());

/// Register `unit` and publish the new config to the running gateway.
pub(crate) fn register_live(
    rt: &Runtime,
    gw: &Gateway,
    dir: &Path,
    unit: UnitKey,
) -> Result<(), String> {
    let _one = PUBLISH_LOCK
        .lock()
        .map_err(|_| "gateway publish lock poisoned")?;
    let key = unit.key.clone();
    register_unit(dir, unit)?;
    publish(rt, gw, dir)?;
    // The seat starts only once the gateway SERVES its key (the reload is asynchronous).
    confirm_key(rt, gw, &key, true)
}

/// Revoke `key` and publish the new config to the running gateway.
pub(crate) fn deregister_live(
    rt: &Runtime,
    gw: &Gateway,
    dir: &Path,
    key: &str,
) -> Result<(), String> {
    let _one = PUBLISH_LOCK
        .lock()
        .map_err(|_| "gateway publish lock poisoned")?;
    deregister_unit(dir, key)?;
    // Revocation is complete only when the gateway refuses the key (and any key still pending).
    settle_revoking(rt, gw, dir, false)
}

/// Wait (bounded) until the running gateway accepts (`accepted`) or refuses `key` on its
/// readiness listener, probed from inside its own network namespace; the key rides stdin.
pub(crate) fn confirm_key(
    rt: &Runtime,
    gw: &Gateway,
    key: &str,
    accepted: bool,
) -> Result<(), String> {
    use std::io::Write as _;
    let want = if accepted { "HTTP/1.1 200" } else { " 401 " };
    let script = format!(
        "read K; i=0; while [ $i -lt 60 ]; do \
         o=$(wget -S -O /dev/null --header \"x-api-key: $K\" http://127.0.0.1:{READY_PORT}/ 2>&1); \
         case \"$o\" in *'{want}'*) exit 0;; esac; i=$((i+1)); sleep 0.25; done; exit 1"
    );
    let mut cmd = rt.cmd();
    cmd.args([
        "run",
        "--rm",
        "-i",
        "--network",
        &format!("container:{}", gw.name),
        "--user",
        "10001",
        "--read-only",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        HELPER_IMAGE,
        "sh",
        "-c",
        &script,
    ]);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("could not start the gateway readiness probe: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = writeln!(stdin, "{key}");
    }
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(st)) if st.success() => return Ok(()),
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < START_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(50))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
        }
    }
    Err(format!(
        "the gateway did not {} the unit key in time (its config reload was not applied)",
        if accepted { "accept" } else { "revoke" }
    ))
}

/// Create the gateway's volumes owned by its user, with an (empty) credential file per route
/// so the served config never names a missing file. Existing credentials are kept.
fn init_volumes(rt: &Runtime, gw: &Gateway, config: &GatewayConfig) -> Result<(), String> {
    let (cfg, broker) = volumes(gw);
    for v in [&cfg, &broker] {
        checked(
            rt,
            &[
                "volume".into(),
                "create".into(),
                "--label".into(),
                format!("{GATEWAY_LABEL}=1"),
                v.clone(),
            ],
        )?;
    }
    let mut script = String::from("set -e; chown 10001 /cfg /broker;");
    for r in &config.providers {
        if let Some(f) = r.credential_file.file_name().and_then(|f| f.to_str()) {
            if f.chars()
                .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
            {
                script.push_str(&format!(
                    " [ -e /broker/{f} ] || : > /broker/{f}; chown 10001 /broker/{f}; chmod 600 /broker/{f};"
                ));
            }
        }
    }
    // One-off, offline, root only for chown (a fresh volume is root-owned).
    checked(
        rt,
        &[
            "run".into(),
            "--rm".into(),
            "--network".into(),
            "none".into(),
            "--user".into(),
            "0".into(),
            "-v".into(),
            format!("{cfg}:/cfg"),
            "-v".into(),
            format!("{broker}:/broker"),
            HELPER_IMAGE.into(),
            "sh".into(),
            "-c".into(),
            script,
        ],
    )
}

fn cli(rt: &Runtime, argv: &[String], limit: Duration) -> Option<Output> {
    let mut cmd = rt.cmd();
    cmd.args(argv);
    output_within(cmd, limit)
}

fn checked(rt: &Runtime, argv: &[String]) -> Result<(), String> {
    match cli(rt, argv, CLI_TIMEOUT) {
        Some(o) if o.status.success() => Ok(()),
        // Do not relay runtime/config diagnostics that might contain credentials.
        Some(o) => Err(format!(
            "gateway runtime command `{}` failed ({})",
            argv.join(" "),
            o.status
        )),
        None => Err(format!(
            "gateway runtime command `{}` could not start or timed out",
            argv.join(" ")
        )),
    }
}

/// The gateway container's state as the runtime reports it. `Unknown` (the inspection failed or
/// timed out) is never read as stopped: a reset clears every live key (codex gw-r2 P2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GwState {
    Running,
    /// The container exists and is not running.
    Stopped,
    /// No such container.
    Absent,
    Unknown,
}

fn gw_state(rt: &Runtime, gw: &Gateway, limit: Duration) -> GwState {
    let Some(o) = cli(rt, &inspect_argv(gw), limit) else {
        return GwState::Unknown;
    };
    classify_inspect(
        o.status.success(),
        &String::from_utf8_lossy(&o.stdout),
        &String::from_utf8_lossy(&o.stderr),
    )
}

/// `container inspect --format {{.State.Running}}`: `true` / `false` on success; a failure is
/// `Absent` only when the runtime says so (docker, podman and nerdctl all print "no such").
fn classify_inspect(ok: bool, stdout: &str, stderr: &str) -> GwState {
    match (ok, stdout.trim()) {
        (true, "true") => GwState::Running,
        (true, "false") => GwState::Stopped,
        (false, _) if stderr.to_ascii_lowercase().contains("no such") => GwState::Absent,
        _ => GwState::Unknown,
    }
}

fn running(rt: &Runtime, gw: &Gateway, limit: Duration) -> bool {
    gw_state(rt, gw, limit) == GwState::Running
}

fn inspect_argv(gw: &Gateway) -> Vec<String> {
    [
        "container",
        "inspect",
        "--format",
        "{{.State.Running}}",
        &gw.name,
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

fn external_network_argv(gw: &Gateway) -> Vec<String> {
    [
        "network",
        "create",
        "--driver",
        "bridge",
        "--label",
        &format!("{GATEWAY_LABEL}=1"),
        &gw.external_network,
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

/// Reuse a running gateway; otherwise clear stale unit keys, create its external bridge and run
/// the hardened container. `dir` is the daemon's explicit gateway state directory. Provision
/// routes from `config/gateway.json` when present, else the defaults (provider endpoints, broker
/// credentials used only when present).
pub(crate) fn ensure_gateway(rt: &Runtime, dir: &Path) -> Result<Gateway, String> {
    // A reset clears and republishes the keys: serialized with every register/deregister and
    // its publish (lock order PUBLISH_LOCK -> CONFIG_LOCK, as in `register_live`).
    let _publishing = PUBLISH_LOCK
        .lock()
        .map_err(|_| "gateway publish lock poisoned")?;
    let _one = CONFIG_LOCK
        .lock()
        .map_err(|_| "gateway config lock poisoned")?;
    // Validate before rendering comma-separated --mount values, as the seat floor does.
    if !dir.is_absolute()
        || !dir
            .to_str()
            .is_some_and(|s| !s.contains([',', '"', '\'', '\n', '=']))
    {
        return Err("gateway directory must be an absolute UTF-8 mountable path".into());
    }
    let gw = Gateway::for_dir(dir);
    // Whose keys the served config holds: this process takes the gateway over once, and keys an
    // earlier engine left (it crashed with units live) are revoked before any new unit runs.
    // Ownership is recorded only once a takeover or a fresh start has SUCCEEDED (codex gw-r3
    // P2): a failed first attempt leaves the stale keys pending for the next one.
    let owned = || OWNED.lock().is_ok_and(|o| o.iter().any(|d| d == dir));
    let own = || {
        if let Ok(mut o) = OWNED.lock() {
            if !o.iter().any(|d| d == dir) {
                o.push(dir.to_path_buf());
            }
        }
    };
    // A gateway whose revocation was never confirmed is removed before anything reuses it
    // (codex gw-r3 P2); until the runtime confirms the removal, no seat is armed on it.
    let quarantined = QUARANTINE.lock().is_ok_and(|q| q.contains(&gw.name));
    if quarantined {
        if !remove_gateway(rt, &gw) {
            return Err(format!(
                "gateway {} is quarantined (a unit key it served was never confirmed revoked) and could not be removed",
                gw.name
            ));
        }
        OWNED.lock().map(|mut o| o.retain(|d| d != dir)).ok();
    }
    // The helper runs every probe and publish: pulled first, under the pull's own bound, so a
    // cold image never times a route probe out (agy gw-r1 F-05).
    if !crate::container_floor::image_present(rt, HELPER_IMAGE) {
        match cli(rt, &["pull".into(), HELPER_IMAGE.into()], START_TIMEOUT) {
            Some(o) if o.status.success() => {}
            _ => {
                return Err(format!(
                    "could not pull the gateway helper image {HELPER_IMAGE}"
                ))
            }
        }
    }
    let state = gw_state(rt, &gw, CLI_TIMEOUT);
    if state == GwState::Unknown {
        return Err(format!(
            "could not tell whether gateway {} is running (the runtime did not answer)",
            gw.name
        ));
    }
    if state == GwState::Running {
        // A gateway an earlier engine started: this process's relay listens on a new port, so
        // probe it now; the next publish (the first unit's key) carries the fresh route.
        let _ = probe_hook(rt, &gw);
        let _ = probe_mcp(rt, &gw);
        if !owned() {
            let mut config = load_config(dir)?;
            if !config.units.is_empty() {
                // Pending first: if the publish or a confirmation below fails, the next attempt
                // (or the next daemon) still knows which keys must be seen refused.
                mark_revoking(dir, config.units.iter().map(|u| u.key.clone()))?;
                config.units.clear();
                persist_units(dir, &config)?;
            }
        }
        // Every pending revocation (a takeover's stale keys, a revocation an earlier attempt or
        // daemon never confirmed) is confirmed before this gateway serves another seat.
        settle_revoking(rt, &gw, dir, true)?;
        own();
        return Ok(gw);
    }
    // A stopped container still owns its name: it must be gone before the keys are reset (a
    // removal that fails leaves the old served config in place). Never `--force`: a gateway that
    // started meanwhile is not removed. The live units of a stopped gateway lost their route
    // with it (their sidecars pin its old address), so clearing their keys below loses nothing.
    if state == GwState::Stopped {
        checked(rt, &["rm".into(), gw.name.clone()])?;
    }
    if checked(rt, &external_network_argv(&gw)).is_err() {
        // An existing normal bridge is reusable, but an internal network must be refused.
        let argv = [
            "network",
            "inspect",
            "--format",
            "{{.Driver}} {{.Internal}}",
            &gw.external_network,
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
        let normal = cli(rt, &argv, CLI_TIMEOUT).is_some_and(|o| {
            o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "bridge false"
        });
        if !normal {
            return Err("could not create or reuse the gateway's external bridge".into());
        }
    }
    let _ = probe_hook(rt, &gw);
    let _ = probe_mcp(rt, &gw);
    let mut config = load_config(dir)?;
    config.units.clear();
    persist_units(dir, &config)?;
    // The container that served the pending keys is gone (absent, or removed above): a fresh
    // gateway never accepted them.
    atomic_json(&revoking_path(dir), &Vec::<String>::new())?;
    init_volumes(rt, &gw, &config)?;
    publish(rt, &gw, dir)?;
    let argv = run_argv(&rt.cli, &gw);
    match cli(rt, &argv[1..], START_TIMEOUT) {
        Some(o) if o.status.success() => {}
        Some(o) => {
            return Err(format!(
                "starting gateway {} failed ({})",
                gw.name, o.status
            ))
        }
        None => {
            return Err(format!(
                "starting gateway {} could not start or timed out",
                gw.name
            ))
        }
    }
    let start = Instant::now();
    while start.elapsed() < START_TIMEOUT {
        let remaining = START_TIMEOUT.saturating_sub(start.elapsed());
        if running(rt, &gw, CLI_TIMEOUT.min(remaining)) {
            own();
            return Ok(gw);
        }
        // Exited (a config it refused): say why now, from its last log line (the gateway logs
        // no credential: sensitive headers are redacted, and the config holds file paths only).
        let state = cli(
            rt,
            &[
                "container".into(),
                "inspect".into(),
                "--format".into(),
                "{{.State.Status}}".into(),
                gw.name.clone(),
            ],
            CLI_TIMEOUT,
        )
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
        if state == "exited" {
            let why = cli(
                rt,
                &["logs".into(), "--tail".into(), "1".into(), gw.name.clone()],
                CLI_TIMEOUT,
            )
            .map(|o| String::from_utf8_lossy(&o.stderr).trim().to_string())
            .unwrap_or_default();
            return Err(format!("gateway {} exited: {}", gw.name, redact_tail(&why)));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Err(format!(
        "gateway {} did not become running within {START_TIMEOUT:?}",
        gw.name
    ))
}

/// Remove the gateway container (a revocation it never confirmed): whatever policy it serves
/// goes with it. `true` when the runtime confirms the removal.
pub(crate) fn quarantine(rt: &Runtime, gw: &Gateway) -> bool {
    let _publishing = PUBLISH_LOCK.lock();
    // Marked BEFORE the removal: if the runtime refuses it, `ensure_gateway` keeps refusing to
    // reuse this gateway until a removal succeeds. A restarted daemon has lost this mark, but
    // the key is still in `config/revoking.json`, so its takeover confirms the refusal first.
    if let Ok(mut q) = QUARANTINE.lock() {
        if !q.contains(&gw.name) {
            q.push(gw.name.clone());
        }
    }
    remove_gateway(rt, gw)
}

/// The gateways (by name) this process quarantined and has not yet seen removed.
static QUARANTINE: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// The daemons' gateway dirs this process has taken over or started (their keys are its own).
static OWNED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// `rm --force` the gateway; on success its quarantine mark is cleared. Callers hold
/// `PUBLISH_LOCK`.
fn remove_gateway(rt: &Runtime, gw: &Gateway) -> bool {
    let gone = checked(rt, &["rm".into(), "--force".into(), gw.name.clone()]).is_ok();
    if gone {
        if let Ok(mut q) = QUARANTINE.lock() {
            q.retain(|n| n != &gw.name);
        }
    }
    gone
}

/// Attach the gateway endpoint under `agw` on a seat's own internal network.
pub(crate) fn attach(rt: &Runtime, gw: &Gateway, seat_network: &str) -> Result<(), String> {
    checked(rt, &attach_argv(gw, seat_network))
}

/// A credential-free model request record (the access-log tool arguments are not retained).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct GatewayRecord {
    pub(crate) status: u16,
    pub(crate) unit: String,
    pub(crate) run: String,
    pub(crate) model: Option<String>,
    pub(crate) tools: Vec<String>,
    pub(crate) input_tokens: Option<u64>,
    pub(crate) output_tokens: Option<u64>,
    pub(crate) text: Vec<String>,
}

/// True only when the line has no provider/unit key shape. Conservatively reject even partial
/// `sk-` / `wk-` tokens followed by a key character; leaked keys must never become records.
pub(crate) fn redact_check(line: &str) -> bool {
    !["sk-", "wk-"].iter().any(|prefix| {
        line.match_indices(*prefix).any(|(at, _)| {
            line.as_bytes()
                .get(at + prefix.len())
                .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'-' || *c == b'_')
        })
    })
}

/// Parse request-scoped JSON lines for a unit id or its metadata name (`unit-<id>`). Unscoped,
/// malformed and credential-bearing lines are dropped; unauthenticated `none` is a separate unit.
pub(crate) fn parse_records(log: &str, unit_key_name_or_unit: &str) -> Vec<GatewayRecord> {
    log.lines()
        .filter_map(|line| {
            if !redact_check(line) {
                return None;
            }
            let v: Value = serde_json::from_str(line).ok()?;
            // Decode JSON escapes before checking again: a key with \u0073 must not evade redaction.
            if !redact_check(&v.to_string()) || v["scope"].as_str()? != "request" {
                return None;
            }
            let unit = v["wicked_unit"].as_str().unwrap_or("none");
            if unit != unit_key_name_or_unit && format!("unit-{unit}") != unit_key_name_or_unit {
                return None;
            }
            let strings = |field: &str| -> Vec<String> {
                match &v[field] {
                    Value::Array(a) => a
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect(),
                    Value::String(s) => vec![s.clone()],
                    _ => Vec::new(),
                }
            };
            Some(GatewayRecord {
                status: u16::try_from(v["http.status"].as_u64()?).ok()?,
                unit: unit.into(),
                run: v["wicked_run"].as_str().unwrap_or("none").into(),
                model: v["llm_model"]
                    .as_str()
                    .or_else(|| v["gen_ai.request.model"].as_str())
                    .map(str::to_string),
                tools: v["llm_tools"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|tool| tool["name"].as_str())
                    .map(str::to_string)
                    .collect(),
                input_tokens: v["llm_in"]
                    .as_u64()
                    .or_else(|| v["gen_ai.usage.input_tokens"].as_u64()),
                output_tokens: v["llm_out"]
                    .as_u64()
                    .or_else(|| v["gen_ai.usage.output_tokens"].as_u64()),
                text: strings("llm_text"),
            })
        })
        .collect()
}

/// Append `records` to the unit's gateway evidence file (one JSON object per line), best
/// effort: the evidence is a record, never a gate input that could fail the unit.
pub(crate) fn write_records(path: &Path, records: &[GatewayRecord]) {
    if records.is_empty() {
        return;
    }
    let mut body = String::new();
    for r in records {
        if let Ok(line) = serde_json::to_string(r) {
            body.push_str(&line);
            body.push('\n');
        }
    }
    let written = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| f.write_all(body.as_bytes()));
    if let Err(e) = written {
        eprintln!(
            "wicked-core: gateway records not written to {}: {e}",
            path.display()
        );
    }
}

pub(crate) fn logs_argv(gw: &Gateway, since: &str) -> Vec<String> {
    ["logs", "--since", since, &gw.name]
        .into_iter()
        .map(str::to_string)
        .collect()
}

/// Collect bounded runtime logs since an RFC3339 timestamp, keeping only this unit's safe
/// request records. Some runtimes put container stderr on the CLI's stderr, so parse both.
pub(crate) fn collect_unit_records(
    rt: &Runtime,
    gw: &Gateway,
    since: &str,
    unit: &str,
) -> Vec<GatewayRecord> {
    let Some(o) = cli(rt, &logs_argv(gw, since), CLI_TIMEOUT).filter(|o| o.status.success()) else {
        return Vec::new();
    };
    let mut records = parse_records(&String::from_utf8_lossy(&o.stdout), unit);
    records.extend(parse_records(&String::from_utf8_lossy(&o.stderr), unit));
    records
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROVEN: &str = include_str!("../tests/fixtures/gateway/proven-config.yaml");
    const ACCESS_LOG: &str = include_str!("../tests/fixtures/gateway/access-log.ndjson");

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir()
                .join(format!("wicked-agw-test-{}", uuid::Uuid::new_v4().simple()));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Codex gw-r2 (P2): only a runtime's own answer counts as stopped or absent; a failed or
    /// garbled inspection is unknown, and a reset (which clears every live key) never follows it.
    #[test]
    fn only_a_confirmed_state_counts_as_stopped() {
        assert_eq!(classify_inspect(true, "true\n", ""), GwState::Running);
        assert_eq!(classify_inspect(true, "false\n", ""), GwState::Stopped);
        for err in [
            "Error: No such container: wicked-agw-1",
            "Error: no such container wicked-agw-1",
        ] {
            assert_eq!(classify_inspect(false, "", err), GwState::Absent, "{err}");
        }
        assert_eq!(
            classify_inspect(false, "", "Cannot connect to the Docker daemon"),
            GwState::Unknown
        );
        assert_eq!(classify_inspect(true, "", ""), GwState::Unknown);
    }

    fn unit(id: &str) -> UnitKey {
        UnitKey::new("run-a", id, "claude")
    }

    fn plain_config() -> GatewayConfig {
        GatewayConfig {
            units: vec![UnitKey {
                key: "wk-unit-PLACEHOLDER-1".into(),
                run: "run-a".into(),
                unit: "1".into(),
                seat: "claude".into(),
            }],
            providers: vec![ModelRoute {
                port: 4000,
                provider: Provider::Anthropic,
                upstream_host: "gw1-stub:8080".into(),
                tls: false,
                credential_file: PathBuf::from("/broker/anthropic.key"),
            }],
            egress_hosts: vec!["registry.npmjs.org".into()],
            hook_upstream: None,
            mcp_upstream: None,
        }
    }

    /// LIVE (`WICKED_CONTAINER_LIVE=1` + a runtime; skipped otherwise): the real lifecycle. The
    /// gateway starts with no unit keys; a seat's key registered AFTER start is honoured (the
    /// config reload reaches the gateway), the request reaches a stub upstream only through
    /// `agw`, the gateway injects the broker's credential, the lease's records carry the unit's
    /// status and usage, and revoking the key makes the same request 401.
    #[cfg(unix)]
    #[test]
    fn live_a_seat_reaches_the_model_only_through_the_gateway_with_its_unit_key() {
        // The process-wide hook route (set by the live test's probe) must not change mid-test.
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        if std::env::var("WICKED_CONTAINER_LIVE").ok().as_deref() != Some("1") {
            eprintln!("skipped: set WICKED_CONTAINER_LIVE=1 with a container runtime to run");
            return;
        }
        let rt = crate::container_floor::runtime().expect("a container runtime answers");
        let image = crate::container_floor::ensure_seat_image(&rt, "claude").unwrap();
        let temp = TempDir::new();
        let dir = temp.0.clone();
        let gw = Gateway::for_dir(&dir);
        let run = |argv: &[&str]| {
            let mut c = rt.cmd();
            c.args(argv);
            crate::container_floor::output_within(c, Duration::from_secs(60)).unwrap()
        };
        // Clean leftovers of an earlier failed run of this test.
        let stub = format!("{}-stub", gw.name);
        run(&["rm", "-f", &gw.name, &stub]);
        // The provider stub on the gateway's external bridge: answers /v1/messages with a native
        // Anthropic body and echoes the credential it received in a header we can read back.
        fs::create_dir_all(dir.join("stub")).unwrap();
        fs::write(
            dir.join("stub/stub.js"),
            "require('http').createServer((q,r)=>{let b='';q.on('data',d=>b+=d);q.on('end',()=>{\
             r.setHeader('content-type','application/json');\
             r.end(JSON.stringify({id:'msg_1',type:'message',role:'assistant',model:'stub',\
             stop_reason:'end_turn',content:[{type:'text',text:'auth='+(q.headers['x-api-key']===\
             'broker-secret'?'injected':'missing')}],usage:{input_tokens:5,output_tokens:3}}))})})\
             .listen(8080)",
        )
        .unwrap();
        configure_gateway(
            &dir,
            &GatewayConfig {
                units: vec![],
                providers: vec![ModelRoute {
                    port: 4000,
                    provider: Provider::Anthropic,
                    upstream_host: format!("{stub}:8080"),
                    tls: false,
                    credential_file: PathBuf::from("/broker/anthropic.key"),
                }],
                egress_hosts: vec![],
                hook_upstream: None,
                mcp_upstream: None,
            },
        )
        .unwrap();
        let gw = ensure_gateway(&rt, &dir).expect("the gateway starts");
        set_broker_credential(&rt, &gw, "anthropic.key", b"broker-secret").unwrap();
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
        let key = UnitKey::new("run-live", "run-live#1", "claude");
        register_live(&rt, &gw, &dir, key.clone()).unwrap();
        let curl = |k: &str| {
            let hdr = format!("x-api-key: {k}");
            // A few tries: the reload is asynchronous.
            let mut last = String::new();
            for _ in 0..20 {
                let o = run(&[
                    "run", "--rm", "--network", &seat_net, &image, "curl", "-s", "--max-time",
                    "4", "-w", "\n%{http_code}", "-H", &hdr, "-H",
                    "anthropic-version: 2023-06-01", "-H", "content-type: application/json",
                    "-d", "{\"model\":\"stub\",\"max_tokens\":5,\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}]}",
                    "http://agw:4000/v1/messages",
                ]);
                last = String::from_utf8_lossy(&o.stdout).to_string();
                if last.ends_with("200") || last.ends_with("401") && k != key.key {
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            last
        };
        let ok = curl(&key.key);
        let direct = run(&[
            "run",
            "--rm",
            "--network",
            &seat_net,
            &image,
            "curl",
            "-s",
            "--max-time",
            "3",
            &format!("http://{stub}:8080/"),
        ]);
        let since = "1970-01-01T00:00:00Z";
        let records = collect_unit_records(&rt, &gw, since, "run-live#1");
        deregister_live(&rt, &gw, &dir, &key.key).unwrap();
        std::thread::sleep(Duration::from_secs(3));
        let revoked = curl(&key.key);
        let logs = run(&["logs", &gw.name]);
        let all_logs = format!(
            "{}{}",
            String::from_utf8_lossy(&logs.stdout),
            String::from_utf8_lossy(&logs.stderr)
        );
        run(&["rm", "-f", &stub, &gw.name]);
        run(&["network", "rm", &seat_net, &gw.external_network]);
        let (cfg, broker) = volumes(&gw);
        run(&["volume", "rm", &cfg, &broker]);
        eprintln!("ok={ok:?}\nrevoked={revoked:?}\nrecords={records:?}");
        assert!(ok.ends_with("200") && ok.contains("auth=injected"), "{ok}");
        assert!(
            !direct.status.success(),
            "the upstream is unreachable from the seat directly"
        );
        assert!(
            records
                .iter()
                .any(|r| r.status == 200 && r.input_tokens == Some(5)),
            "{records:?}"
        );
        assert!(
            revoked.ends_with("401"),
            "a revoked key is refused: {revoked}"
        );
        assert!(!all_logs.contains("broker-secret") && !all_logs.contains(&key.key));
    }

    /// Codex gw-r1 (P2): the MCP listener exists only with a reachable broker, and passes exactly
    /// crew's two unit-token routes (never a prefix: the operator's broker routes stay unreachable).
    #[test]
    fn the_mcp_route_passes_only_the_two_unit_token_routes() {
        assert!(!render(&plain_config())["binds"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["port"] == 8700));
        let cfg = GatewayConfig {
            mcp_upstream: Some("host.lima.internal:7701".into()),
            ..plain_config()
        };
        let r = render(&cfg);
        let bind = r["binds"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["port"] == 8700)
            .expect("the MCP bind");
        let route = &bind["listeners"][0]["routes"][0];
        assert_eq!(
            route["matches"],
            json!([
                {"path": {"exact": "/api/v1/mcp/call"}, "method": "POST"},
                {"path": {"exact": "/api/v1/mcp/tools"}, "method": "POST"}
            ])
        );
        assert_eq!(route["backends"][0]["host"], "host.lima.internal:7701");
        assert_eq!(crew_port("http://127.0.0.1:7701"), Some(7701));
        assert_eq!(crew_port("http://localhost:61771/"), Some(61771));
        assert_eq!(crew_port("http://example.com:80"), None);
        assert_eq!(crew_port("https://127.0.0.1:7701"), None);
    }

    /// Codex gw-r1 (P2): the readiness listener answers from the gateway itself for exactly the
    /// served keys, and is not a seat port.
    #[test]
    fn the_readiness_listener_holds_the_served_keys_and_is_not_a_seat_port() {
        let r = render(&plain_config());
        let bind = r["binds"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["port"] == READY_PORT)
            .expect("the readiness bind");
        let policies = &bind["listeners"][0]["routes"][0]["policies"];
        assert_eq!(policies["apiKey"]["mode"], "strict");
        assert_eq!(
            policies["apiKey"]["keys"],
            r["binds"][0]["listeners"][0]["routes"][0]["policies"]["apiKey"]["keys"]
        );
        assert_eq!(policies["directResponse"]["status"], 200);
        assert!(!bind["listeners"][0]["routes"][0]
            .as_object()
            .unwrap()
            .contains_key("backends"));
        assert!(!crate::container_floor::GATEWAY_PORTS.contains(&READY_PORT));
    }

    #[test]
    fn the_hook_route_renders_only_when_the_relay_is_reachable() {
        let none = render(&plain_config());
        assert!(!none["binds"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["port"] == crate::hook_relay::HOOK_PORT));
        let cfg = GatewayConfig {
            hook_upstream: Some("host.lima.internal:51234".into()),
            ..plain_config()
        };
        let r = render(&cfg);
        let hook = r["binds"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["port"] == crate::hook_relay::HOOK_PORT)
            .expect("the hook bind");
        let route = &hook["listeners"][0]["routes"][0];
        assert_eq!(route["matches"][0]["path"]["pathPrefix"], "/hook/");
        assert_eq!(route["backends"][0]["host"], "host.lima.internal:51234");
    }

    /// LIVE (`WICKED_CONTAINER_LIVE=1`): the per-seat firewall sidecar. A seat in its namespace
    /// reaches the gateway (`agw:4000` answers 401 to no key) and nothing else: not the internal
    /// bridge's host address (the VM host's services), not the internet; and the seat, with no
    /// capabilities, cannot flush the rules.
    #[cfg(unix)]
    #[test]
    fn live_the_seat_sidecar_admits_only_the_gateway() {
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        if std::env::var("WICKED_CONTAINER_LIVE").ok().as_deref() != Some("1") {
            eprintln!("skipped: set WICKED_CONTAINER_LIVE=1 with a container runtime to run");
            return;
        }
        let rt = crate::container_floor::runtime().expect("a container runtime answers");
        let image = crate::container_floor::ensure_seat_image(&rt, "claude").unwrap();
        let temp = TempDir::new();
        let dir = temp.0.clone();
        let run = |argv: &[&str]| {
            let mut c = rt.cmd();
            c.args(argv);
            crate::container_floor::output_within(c, Duration::from_secs(60)).unwrap()
        };
        let gw = ensure_gateway(&rt, &dir).expect("the gateway starts");
        let seat = format!("{}-sc", gw.name);
        run(&["rm", "-f", &crate::container_floor::netns_name(&seat)]);
        run(&["network", "rm", &seat]);
        assert!(run(&["network", "create", "--internal", &seat])
            .status
            .success());
        attach(&rt, &gw, &seat).unwrap();
        let ip = crate::container_floor::gateway_ip(&rt, &gw.name, &seat).unwrap();
        crate::container_floor::start_netns_sidecar(&rt, &seat, &seat, &ip).expect("sidecar up");
        let bridge = String::from_utf8_lossy(
            &run(&[
                "network",
                "inspect",
                "--format",
                "{{(index .IPAM.Config 0).Gateway}}",
                &seat,
            ])
            .stdout,
        )
        .trim()
        .to_string();
        let ns = format!("container:{}", crate::container_floor::netns_name(&seat));
        let probe = |url: &str| -> String {
            let o = run(&[
                "run",
                "--rm",
                "--network",
                &ns,
                "--user",
                "502:20",
                "--cap-drop",
                "ALL",
                &image,
                "curl",
                "-s",
                "-o",
                "/dev/null",
                "-w",
                "%{http_code}",
                "--max-time",
                "3",
                url,
            ]);
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        };
        let to_gw = probe("http://agw:4000/v1/messages");
        let to_bridge = probe(&format!("http://{bridge}:4000/"));
        let to_bridge_k8s = probe(&format!("https://{bridge}:6443/"));
        let to_net = probe("http://1.1.1.1/");
        let flush = run(&[
            "run",
            "--rm",
            "--network",
            &ns,
            "--cap-drop",
            "ALL",
            "--user",
            "0",
            &crate::container_floor::ensure_netns_image(&rt).unwrap(),
            "iptables",
            "-F",
        ]);
        run(&[
            "rm",
            "-f",
            &crate::container_floor::netns_name(&seat),
            &gw.name,
        ]);
        run(&["network", "rm", &seat, &gw.external_network]);
        let (cfg, broker) = volumes(&gw);
        run(&["volume", "rm", &cfg, &broker]);
        eprintln!(
            "gw={to_gw} bridge={to_bridge} bridge6443={to_bridge_k8s} net={to_net} flush={:?}",
            flush.status
        );
        assert_eq!(to_gw, "401", "the gateway answers (no key)");
        assert_eq!(to_bridge, "000", "the VM host is unreachable");
        assert_eq!(to_bridge_k8s, "000", "the k3s API is unreachable");
        assert_eq!(to_net, "000", "the internet is unreachable");
        assert!(
            !flush.status.success(),
            "a seat without capabilities cannot flush the rules"
        );
    }

    fn json_file(path: &Path) -> Value {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn anthropic_render_matches_the_proven_fixture() {
        // serde_yaml already belongs to the root crate; it is only used here to read the
        // checked-in proof. The production model, renderer and persistence are all JSON.
        let fixture: Value = serde_yaml::from_str(PROVEN).unwrap();
        let rendered = render(&plain_config());
        assert_eq!(rendered["config"], fixture["config"]);
        let mut expected_log = fixture["frontendPolicies"].clone();
        expected_log["accessLog"]["add"]["wicked_seat"] = json!("default(apiKey.seat, \"none\")");
        assert_eq!(rendered["frontendPolicies"], expected_log);
        let mut expected_bind = fixture["binds"][0].clone();
        expected_bind["listeners"][0]["routes"][0]["policies"]["apiKey"]["keys"][0]["metadata"]
            ["seat"] = json!("claude");
        // Whole-bind equality pins the exact apiKey location/metadata, backendAuth file,
        // native messages/count_tokens/passthrough routes, provider and plain hostOverride.
        assert_eq!(rendered["binds"][0], expected_bind);
    }

    #[test]
    fn openai_routes_and_every_units_metadata_are_rendered_per_bind() {
        let config = GatewayConfig {
            units: vec![unit("1"), unit("2")],
            ..GatewayConfig::default()
        };
        let rendered = render(&config);
        assert_eq!(rendered["binds"][1]["port"], 4001);
        let openai = &rendered["binds"][1]["listeners"][0]["routes"][0];
        assert_eq!(
            openai["policies"]["ai"]["routes"],
            json!({
                "/v1/chat/completions": "completions", "/v1/responses": "responses", "*": "passthrough"
            })
        );
        assert_eq!(
            openai["policies"]["backendAuth"],
            json!({"key": {"file": "/broker/openai.key"}})
        );
        assert_eq!(
            openai["backends"][0]["ai"]["provider"],
            json!({"openAI": {}})
        );
        assert_eq!(
            openai["policies"]["apiKey"]["location"],
            json!({"header": {"name": "authorization", "prefix": "Bearer "}})
        );
        for bind in &rendered["binds"].as_array().unwrap()[..2] {
            let auth = &bind["listeners"][0]["routes"][0]["policies"]["apiKey"];
            assert_eq!(auth["mode"], "strict");
            assert_eq!(auth["keys"].as_array().unwrap().len(), 2);
            for (index, unit) in config.units.iter().enumerate() {
                assert_eq!(auth["keys"][index]["key"], unit.key);
                assert_eq!(
                    auth["keys"][index]["metadata"],
                    json!({
                        "name": format!("unit-{}", unit.unit), "unit": unit.unit,
                        "run": unit.run, "seat": unit.seat
                    })
                );
            }
        }
    }

    #[test]
    fn zero_units_stays_strict_and_empty_egress_has_no_route() {
        let rendered = render(&GatewayConfig::default());
        for bind in &rendered["binds"].as_array().unwrap()[..2] {
            let auth = &bind["listeners"][0]["routes"][0]["policies"]["apiKey"];
            assert_eq!(auth["mode"], "strict");
            assert_eq!(auth["keys"], json!([]));
        }
        assert_eq!(rendered["tcpRoutes"], json!([]));
    }

    #[test]
    fn connect_render_matches_the_proven_allowlist_shape() {
        let rendered = render(&plain_config());
        assert_eq!(
            rendered["binds"][1],
            json!({"port": 3000, "tunnelProtocol": "connect", "listeners": []})
        );
        assert_eq!(
            rendered["gateways"],
            json!({"secure": {"port": 443, "listeners": [{
                "name": "public-egress", "hostname": "*", "protocol": "TLS"
            }]}})
        );
        assert_eq!(
            rendered["tcpRoutes"],
            json!([{
                "name": "egress-allowlist", "gateways": "secure/public-egress",
                "hostnames": ["registry.npmjs.org"], "backends": [{"dynamic": {}}]
            }])
        );
    }

    #[test]
    fn an_overridden_upstream_gets_backend_tls_and_the_default_endpoint_needs_none() {
        let mut config = plain_config();
        config.providers[0].tls = true;
        let r = render(&config);
        let route = &r["binds"][0]["listeners"][0]["routes"][0];
        assert_eq!(route["policies"]["backendTLS"], json!({}));
        assert!(route["backends"][0]["ai"]["hostOverride"].is_string());
        // The provider's own endpoint: no override, no explicit TLS (agentgateway defaults both).
        let d = render(&GatewayConfig::default());
        let route = &d["binds"][0]["listeners"][0]["routes"][0];
        assert!(route["backends"][0]["ai"].get("hostOverride").is_none());
        assert!(route["policies"].get("backendTLS").is_none());
        assert_eq!(
            route["backends"][0]["ai"]["provider"],
            json!({"anthropic": {}})
        );
    }

    #[test]
    fn unit_keys_are_unique_uuid_hex_and_debug_is_redacted() {
        let a = unit("1");
        let b = unit("1");
        assert_ne!(a.key, b.key);
        assert!(a.key.starts_with("wk-"));
        assert_eq!(a.key.len(), 35);
        assert!(a.key[3..].bytes().all(|c| c.is_ascii_hexdigit()));
        assert!(!format!("{a:?}").contains(&a.key));
        assert_eq!(
            (a.run.as_str(), a.unit.as_str(), a.seat.as_str()),
            ("run-a", "1", "claude")
        );
    }

    fn broker_key(dir: &Path) {
        fs::create_dir_all(dir.join("broker")).unwrap();
        fs::write(dir.join("broker/anthropic.key"), "test-only").unwrap();
    }

    /// Codex gw-r4 (P2): a revoked key stays recorded as pending (private, 0600) until the
    /// gateway is seen refusing it, so a failed publish, a crash or a failed quarantine cannot
    /// lose the one record that it must still be confirmed refused.
    #[test]
    fn a_revoked_key_stays_pending_until_the_gateway_refuses_it() {
        let _env = crate::test_env::ENV_LOCK
            .read()
            .unwrap_or_else(|p| p.into_inner());
        let temp = TempDir::new();
        broker_key(&temp.0);
        configure_gateway(&temp.0, &plain_config()).unwrap();
        let a = unit("1");
        register_unit(&temp.0, a.clone()).unwrap();
        assert!(load_revoking(&temp.0).unwrap().is_empty());
        deregister_unit(&temp.0, &a.key).unwrap();
        deregister_unit(&temp.0, &a.key).unwrap();
        assert_eq!(load_revoking(&temp.0).unwrap(), vec![a.key.clone()]);
        assert!(load_config(&temp.0).unwrap().units.is_empty());
        // Codex gw-r5 (P2): a crash between `units.json` and `config.json` leaves a rendered
        // config that still serves a pending key; the settle re-renders before publishing.
        let b = unit("2");
        register_unit(&temp.0, b.clone()).unwrap();
        mark_revoking(&temp.0, [b.key.clone()]).unwrap();
        atomic_json(&temp.0.join("config/units.json"), &Vec::<UnitKey>::new()).unwrap();
        let served = |dir: &Path| fs::read_to_string(dir.join("config/config.json")).unwrap();
        assert!(served(&temp.0).contains(&b.key), "the crash left it served");
        let pending = rerender_without_pending(&temp.0).unwrap();
        assert!(pending.contains(&b.key));
        assert!(!served(&temp.0).contains(&b.key));
        assert!(!served(&temp.0).contains(&a.key));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(revoking_path(&temp.0))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn registration_and_revocation_atomically_replace_config_and_preserve_routes() {
        // The process-wide hook route (set by the live test's probe) must not change mid-test.
        let _env = crate::test_env::ENV_LOCK
            .read()
            .unwrap_or_else(|p| p.into_inner());
        let temp = TempDir::new();
        broker_key(&temp.0);
        configure_gateway(&temp.0, &plain_config()).unwrap();
        let config_path = temp.0.join("config/config.json");
        let initial = json_file(&config_path);
        assert_eq!(
            initial["binds"][0]["listeners"][0]["routes"][0]["policies"]["apiKey"]["keys"],
            json!([])
        );
        let a = unit("1");
        let b = unit("2");
        #[cfg(unix)]
        let mut old_handle = fs::File::open(&config_path).unwrap();
        register_unit(&temp.0, a.clone()).unwrap();
        #[cfg(unix)]
        {
            use std::io::Read;
            let mut old_bytes = String::new();
            old_handle.read_to_string(&mut old_bytes).unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&old_bytes).unwrap(),
                initial,
                "an open handle still sees the complete previous inode"
            );
            use std::os::unix::fs::MetadataExt;
            assert_ne!(
                old_handle.metadata().unwrap().ino(),
                fs::metadata(&config_path).unwrap().ino()
            );
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&config_path).unwrap().permissions().mode() & 0o777,
                0o644
            );
            assert_eq!(
                fs::metadata(temp.0.join("config/units.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        register_unit(&temp.0, b.clone()).unwrap();
        register_unit(&temp.0, b.clone()).unwrap();
        let persisted: Vec<UnitKey> = read_json(&temp.0.join("config/units.json"))
            .unwrap()
            .unwrap();
        assert_eq!(
            persisted,
            vec![a.clone(), b.clone()],
            "registration is idempotent by key"
        );
        let mut changed_routes = plain_config();
        changed_routes.egress_hosts.push("example.com".into());
        configure_gateway(&temp.0, &changed_routes).unwrap();
        assert_eq!(
            load_config(&temp.0).unwrap().units,
            persisted,
            "provisioning preserves live keys"
        );
        deregister_unit(&temp.0, &a.key).unwrap();
        deregister_unit(&temp.0, &a.key).unwrap();
        let config = load_config(&temp.0).unwrap();
        assert_eq!(config.units, vec![b.clone()]);
        assert_eq!(config.providers, changed_routes.providers);
        assert_eq!(config.egress_hosts, changed_routes.egress_hosts);
        assert_eq!(json_file(&config_path), render(&effective(&config)));
        deregister_unit(&temp.0, &b.key).unwrap();
        assert!(load_config(&temp.0).unwrap().units.is_empty());
        assert_eq!(
            json_file(&config_path),
            initial_with_hosts(initial, &changed_routes.egress_hosts)
        );
        assert!(fs::read_dir(temp.0.join("config")).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
    }

    fn initial_with_hosts(mut initial: Value, hosts: &[String]) -> Value {
        initial["tcpRoutes"][0]["hostnames"] = json!(hosts);
        initial
    }

    #[test]
    fn concurrent_registration_does_not_lose_keys() {
        // The process-wide hook route (set by the live test's probe) must not change mid-test.
        let _env = crate::test_env::ENV_LOCK
            .read()
            .unwrap_or_else(|p| p.into_inner());
        let temp = TempDir::new();
        broker_key(&temp.0);
        configure_gateway(&temp.0, &plain_config()).unwrap();
        let keys: Vec<UnitKey> = (0..8).map(|n| unit(&n.to_string())).collect();
        let handles: Vec<_> = keys
            .iter()
            .cloned()
            .map(|key| {
                let dir = temp.0.clone();
                std::thread::spawn(move || register_unit(&dir, key).unwrap())
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        let config = load_config(&temp.0).unwrap();
        assert_eq!(config.units.len(), keys.len());
        assert!(keys.iter().all(|key| config.units.contains(key)));
        assert_eq!(
            json_file(&temp.0.join("config/config.json")),
            render(&effective(&config))
        );
    }

    #[test]
    fn malformed_unit_state_is_refused_without_overwriting_served_config() {
        let temp = TempDir::new();
        configure_gateway(&temp.0, &plain_config()).unwrap();
        let config_path = temp.0.join("config/config.json");
        let original = fs::read(&config_path).unwrap();
        fs::write(temp.0.join("config/units.json"), "[broken").unwrap();
        assert!(register_unit(&temp.0, unit("1")).is_err());
        assert!(deregister_unit(&temp.0, "absent").is_err());
        assert_eq!(fs::read(config_path).unwrap(), original);
    }

    #[test]
    fn seat_env_uses_native_model_urls_and_the_connect_proxy() {
        for (provider, base, api_key, url) in [
            (
                Provider::Anthropic,
                "ANTHROPIC_BASE_URL",
                "ANTHROPIC_API_KEY",
                "http://agw:4000",
            ),
            (
                Provider::OpenAI,
                "OPENAI_BASE_URL",
                "OPENAI_API_KEY",
                "http://agw:4001/v1",
            ),
        ] {
            let env = seat_env(provider, "placeholder");
            assert_eq!(
                env,
                vec![
                    (base.into(), url.into()),
                    (api_key.into(), "placeholder".into()),
                    ("HTTPS_PROXY".into(), "http://agw:3000".into()),
                    ("HTTP_PROXY".into(), "http://agw:3000".into()),
                    ("NO_PROXY".into(), "agw".into()),
                ]
            );
        }
    }

    #[test]
    fn gateway_names_are_stable_sha256_prefixes_and_distinct_by_state_directory() {
        assert_eq!(gateway_name(Path::new("")), "wicked-agw-e3b0c44298fc");
        let a = gateway_name(Path::new("/state/one"));
        assert_eq!(a, gateway_name(Path::new("/state/one")));
        assert_ne!(a, gateway_name(Path::new("/state/two")));
        assert_eq!(a.len(), "wicked-agw-".len() + 12);
        assert!(a["wicked-agw-".len()..]
            .bytes()
            .all(|c| c.is_ascii_hexdigit()));
        let gw = Gateway::for_dir(Path::new("/state/one"));
        assert_eq!(gw.external_network, format!("{a}-ext"));
    }

    #[test]
    fn runtime_argv_pins_hardening_mounts_and_network_membership() {
        let gw = Gateway::for_dir(Path::new("/state/gateway"));
        let argv = run_argv(Path::new("/usr/bin/docker"), &gw);
        let has = |pair: &[&str]| argv.windows(pair.len()).any(|w| w == pair);
        assert_eq!(argv[0], "/usr/bin/docker");
        assert!(has(&["run", "-d"]));
        assert!(has(&["--name", &gw.name]));
        assert!(has(&["--network", &gw.external_network]));
        for pair in [
            ["--user", "10001"],
            ["--label", "wicked.gateway=1"],
            ["--cap-drop", "ALL"],
            ["--security-opt", "no-new-privileges"],
            ["--pids-limit", "512"],
            ["--memory", "512m"],
        ] {
            assert!(has(&pair), "missing {pair:?}");
        }
        // Named volumes, never host binds (a host-side write never reaches the watcher).
        let (cfg, broker) = volumes(&gw);
        for pair in [
            [
                "--mount",
                &format!("type=volume,src={cfg},dst=/cfg,readonly"),
            ],
            [
                "--mount",
                &format!("type=volume,src={broker},dst=/broker,readonly"),
            ],
        ] {
            assert!(has(&pair), "missing {pair:?}");
        }
        assert!(argv.contains(&"--read-only".into()));
        assert_eq!(
            &argv[argv.len() - 3..],
            &[GATEWAY_IMAGE, "-f", "/cfg/config.json"]
        );
        assert!(!argv.iter().any(|v| [
            "--privileged",
            "--publish",
            "-p",
            "--internal",
            "--cap-add"
        ]
        .contains(&v.as_str())));
        assert_eq!(
            external_network_argv(&gw),
            vec![
                "network",
                "create",
                "--driver",
                "bridge",
                "--label",
                "wicked.gateway=1",
                &gw.external_network
            ]
        );
        assert_eq!(
            attach_argv(&gw, "seat-1"),
            vec!["network", "connect", "--alias", "agw", "seat-1", &gw.name]
        );
        assert_eq!(
            logs_argv(&gw, "2026-10-13T04:25:00Z"),
            vec!["logs", "--since", "2026-10-13T04:25:00Z", &gw.name]
        );
        assert_eq!(
            inspect_argv(&gw),
            vec![
                "container",
                "inspect",
                "--format",
                "{{.State.Running}}",
                &gw.name
            ]
        );
    }

    #[test]
    fn real_access_log_attributes_tools_and_usage_to_the_authenticated_unit() {
        let records = parse_records(ACCESS_LOG, "1");
        assert_eq!(
            records,
            vec![GatewayRecord {
                status: 200,
                unit: "1".into(),
                run: "run-a".into(),
                model: Some("claude-proof".into()),
                tools: vec!["Bash".into()],
                input_tokens: Some(14),
                output_tokens: Some(7),
                text: vec![],
            }]
        );
        assert_eq!(parse_records(ACCESS_LOG, "unit-1"), records);
        let rejected = parse_records(ACCESS_LOG, "none");
        assert_eq!(rejected.len(), 1);
        assert_eq!(rejected[0].status, 401);
        assert_eq!(
            (rejected[0].unit.as_str(), rejected[0].run.as_str()),
            ("none", "none")
        );
        assert_eq!(rejected[0].input_tokens, None);
        assert!(parse_records(ACCESS_LOG, "2").is_empty());
    }

    #[test]
    fn credential_bearing_lines_are_dropped_even_with_json_escapes() {
        let clean = ACCESS_LOG.lines().next().unwrap();
        assert!(redact_check(clean));
        for key in [
            "sk-ant-test-provider-key",
            "wk-0123456789abcdef0123456789abcdef",
        ] {
            let mut line: Value = serde_json::from_str(clean).unwrap();
            line["llm_text"] = json!([format!("leaked {key}")]);
            let raw = line.to_string();
            assert!(!redact_check(&raw));
            assert!(parse_records(&format!("{raw}\n{clean}"), "1").len() == 1);
        }
        let mut line: Value = serde_json::from_str(clean).unwrap();
        line["llm_text"] = json!(["sk-secret"]);
        let escaped = line.to_string().replace("sk-secret", "\\u0073k-secret");
        assert!(redact_check(&escaped), "the raw bytes have no key prefix");
        assert!(
            parse_records(&escaped, "1").is_empty(),
            "the decoded value is also checked"
        );
    }

    #[test]
    fn parser_skips_nonrequests_and_bad_lines_and_reads_native_usage_fallbacks() {
        let log = [
            "not JSON".into(), json!({"scope": "startup", "wicked_unit": "1", "http.status": 200}).to_string(),
            json!({"scope": "request", "wicked_unit": "1", "http.status": 70000}).to_string(),
            json!({"scope": "request", "wicked_unit": "1", "http.status": 200,
                "gen_ai.request.model": "model", "gen_ai.usage.input_tokens": 0,
                "gen_ai.usage.output_tokens": 9, "llm_tools": [{"name": "Read"}, {"id": "missing-name"}],
                "llm_text": ["hello", 42, "world"]
            }).to_string(),
        ].join("\n");
        assert_eq!(
            parse_records(&log, "1"),
            vec![GatewayRecord {
                status: 200,
                unit: "1".into(),
                run: "none".into(),
                model: Some("model".into()),
                tools: vec!["Read".into()],
                input_tokens: Some(0),
                output_tokens: Some(9),
                text: vec!["hello".into(), "world".into()],
            }]
        );
    }
}
