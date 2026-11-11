//! The Kubernetes runner (core#1383, k8s-swap): governed seats, the checks floor and the model
//! gateway run as pods on a local cluster through kubernetes-sigs/agent-sandbox, in place of
//! per-seat Docker containers and networks.
//!
//! **Glue, not a runtime.** As with the container floor before it, the engine does not speak a
//! cluster API. It renders manifests as data ([`render`]) and runs `kubectl` with the kubeconfig,
//! the context and the namespace pinned on every argv ([`Kube`]). A seat is still an ordinary
//! child process of the engine (`kubectl exec -i`): stdio streaming, the exit code, the deadline
//! and the cancel kill stay where they were. A pod outlives a killed client, so every seat is
//! reaped by deleting its Sandbox.
//!
//! **Never the operator's current context.** The engine talks only to the cluster it is told
//! about: its own kubeconfig written by `wicked-core cluster install` (a ServiceAccount scoped to
//! this daemon's objects), or [`CONTEXT_ENV`]. A current-context that happens to point at another
//! cluster is never used.
//!
//! **Ownership.** Everything the engine creates carries [`OWNER_LABEL`] (this daemon's id,
//! [`owner_id`]) and [`MANAGED_LABEL`]. A run's pods, claims and volumes live in the run's own
//! namespace ([`run_namespace_name`]); deleting the namespace is the run's cleanup, and a delete
//! is refused for any object whose owner label is not this daemon's ([`Kube::delete_owned`]).
//!
//! The parity contract this module is held to is `PARITY.md` of the k8s-swap lane, mirrored in
//! the module tests (each test names its row).

pub(crate) mod cli;
pub(crate) mod cluster;
pub(crate) mod floor;
pub(crate) mod gw;
// The sign-in transport (B-4); seat sign-ins need a Unix host, as the seat runner does.
#[cfg(unix)]
pub(crate) mod login;
pub(crate) mod render;
pub(crate) mod seat;
pub(crate) mod sync;

#[cfg(test)]
mod live;
// The seat runner is Unix-only (no Windows host runs seats), and its fixtures are Unix paths.
#[cfg(all(test, unix))]
mod seat_tests;
#[cfg(test)]
mod tests;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest, Sha256};
use wicked_apps_core::HardenedCommand;

use crate::worker_sandbox::FloorUnarmed;

/// The kubectl to run (a name on `PATH` or an absolute path); otherwise the first found on
/// `PATH` or in a desktop's install dir ([`kubectl_candidates`]).
pub(crate) const KUBECTL_ENV: &str = "WICKED_KUBECTL";
/// The kubeconfig the engine uses. Default: the one `cluster install` wrote under the daemon's
/// state home ([`own_kubeconfig`]); otherwise kubectl's own default file, with [`CONTEXT_ENV`]
/// naming the context.
pub(crate) const KUBECONFIG_ENV: &str = "WICKED_K8S_KUBECONFIG";
/// The context to use. Required unless the engine's own kubeconfig is present: the current
/// context is never used implicitly.
pub(crate) const CONTEXT_ENV: &str = "WICKED_K8S_CONTEXT";
/// The context name inside the kubeconfig `cluster install` writes.
pub(crate) const OWN_CONTEXT: &str = "wicked";

/// The label naming the daemon that owns an object (its [`owner_id`]).
pub(crate) const OWNER_LABEL: &str = "wicked.dev/owner";
/// The label naming the run an object belongs to (a sanitized run id, [`label_value`]).
pub(crate) const RUN_LABEL: &str = "wicked.dev/run";
/// The label naming an object's role: `seat`, `judge`, `floor`, `sync`, `fill`, `gateway`.
pub(crate) const ROLE_LABEL: &str = "wicked.dev/role";
/// The label naming the engine process that created a pod, so a restarted engine can tell its
/// predecessor's leftovers in a live run ([`boot_id`]).
pub(crate) const BOOT_LABEL: &str = "wicked.dev/boot";
/// The label naming the unit execution a seat pod serves (its claim identity, PARITY B15).
pub(crate) const UNIT_LABEL: &str = "wicked.dev/unit";
/// The standard managed-by label, so `kubectl get -l` finds every engine object.
pub(crate) const MANAGED_LABEL: &str = "app.kubernetes.io/managed-by";
pub(crate) const MANAGED_BY: &str = "wicked-core";

/// The agent-sandbox release the runner is pinned to, and the sha256 of its release manifest
/// (`sandbox-with-extensions.yaml`), checked before every apply.
pub(crate) const AGENT_SANDBOX_VERSION: &str = "v1.0.10";
pub(crate) const AGENT_SANDBOX_SHA256: &str =
    "7cc1eca2fb45844b0ec0c35fc975340dc01af2994301ffa8bf05db1ad1e397ee";
/// The controller image the pinned manifest runs: the version check reads it back.
pub(crate) const AGENT_SANDBOX_IMAGE: &str =
    "registry.k8s.io/agent-sandbox/agent-sandbox-controller:v1.0.10";
/// The namespace the agent-sandbox manifest installs its controller in.
pub(crate) const AGENT_SANDBOX_NAMESPACE: &str = "agent-sandbox-system";

const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
pub(crate) const CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// A daemon's id: a stable hash of its operational home (the gateway dir's parent), so two
/// daemons on one cluster never share or delete each other's objects. DNS-label safe.
pub(crate) fn owner_id(home: &Path) -> String {
    let canon = crate::worker_sandbox::canon_lenient(home).unwrap_or_else(|| home.to_path_buf());
    let digest = Sha256::digest(canon.to_string_lossy().as_bytes());
    format!("w{}", &hex(&digest)[..12])
}

/// This engine process's id ([`BOOT_LABEL`]): random per process.
pub(crate) fn boot_id() -> &'static str {
    static BOOT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    BOOT.get_or_init(|| uuid::Uuid::new_v4().simple().to_string()[..12].to_string())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `raw` as a label value: `[A-Za-z0-9._-]`, at most 63, starting and ending alphanumeric. A
/// value that had to change gets a short digest suffix. A label is a selector, never an
/// identity: two ids CAN map to one label (`"a b"` and the literal `"a-b-c8687a08"`), so every
/// identity decision reads the exact id (a run's [`render::RUN_ID_ANNOTATION`]), review r2 P3.
pub(crate) fn label_value(raw: &str) -> String {
    let clean: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = clean.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    if trimmed == raw && !raw.is_empty() && raw.len() <= 63 {
        return raw.to_string();
    }
    let digest = &hex(&Sha256::digest(raw.as_bytes()))[..8];
    let head: String = trimmed.chars().take(63 - 9).collect();
    let head = head.trim_end_matches(|c: char| !c.is_ascii_alphanumeric());
    if head.is_empty() {
        format!("x{digest}")
    } else {
        format!("{head}-{digest}")
    }
}

/// The daemon's own namespace (its gateway, broker and cache fills).
pub(crate) fn daemon_namespace_name(owner: &str) -> String {
    format!("wicked-sys-{owner}")
}

/// A run's namespace: `wicked-run-<owner>-<digest of the run id>`, a DNS label (≤ 63).
pub(crate) fn run_namespace_name(owner: &str, run_id: &str) -> String {
    let digest = &hex(&Sha256::digest(run_id.as_bytes()))[..10];
    format!("wicked-run-{owner}-{digest}")
}

/// The kubectl candidates, in order: the operator's choice (the only one when set), then `PATH`,
/// then the desktops' install dirs (a daemon's `PATH` often lacks them).
pub(crate) fn kubectl_candidates(
    choice: Option<&str>,
    home: Option<&Path>,
    on_path: impl Fn(&str) -> Option<PathBuf>,
) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        if !out.contains(&p) {
            out.push(p);
        }
    };
    if let Some(c) = choice.map(str::trim).filter(|c| !c.is_empty()) {
        let p = Path::new(c);
        if p.is_absolute() {
            if p.is_file() {
                push(p.to_path_buf());
            }
        } else if let Some(found) = on_path(c) {
            push(found);
        }
        return out;
    }
    if let Some(found) = on_path("kubectl") {
        push(found);
    }
    if let Some(h) = home {
        let p = h.join(".rd/bin/kubectl");
        if p.is_file() {
            push(p);
        }
    }
    for abs in [
        "/usr/local/bin/kubectl",
        "/opt/homebrew/bin/kubectl",
        "/Applications/Docker.app/Contents/Resources/bin/kubectl",
    ] {
        let p = PathBuf::from(abs);
        if p.is_file() {
            push(p);
        }
    }
    out
}

/// The kubeconfig `cluster install` writes for a daemon: `<home>/k8s/kubeconfig`.
pub(crate) fn own_kubeconfig(home: &Path) -> PathBuf {
    home.join("k8s").join("kubeconfig")
}

/// Which cluster the engine talks to, and how: every argv pins all three.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Kube {
    pub(crate) kubectl: PathBuf,
    /// `None` = kubectl's default kubeconfig (with an explicit context).
    pub(crate) kubeconfig: Option<PathBuf>,
    pub(crate) context: String,
    /// This daemon's id ([`owner_id`]).
    pub(crate) owner: String,
}

/// Where the engine's cluster settings come from, resolved without running anything.
pub(crate) fn resolve_settings(
    home: &Path,
    kubectl_choice: Option<&str>,
    kubeconfig_choice: Option<&str>,
    context_choice: Option<&str>,
    user_home: Option<&Path>,
    on_path: impl Fn(&str) -> Option<PathBuf>,
) -> Result<Kube, (FloorUnarmed, String)> {
    let kubectl = kubectl_candidates(kubectl_choice, user_home, on_path)
        .into_iter()
        .next()
        .ok_or_else(|| {
            (
                FloorUnarmed::NoCluster,
                format!("no kubectl was found (set {KUBECTL_ENV} to its path)"),
            )
        })?;
    let context_choice = context_choice.map(str::trim).filter(|c| !c.is_empty());
    let kubeconfig_choice = kubeconfig_choice
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(PathBuf::from);
    let own = own_kubeconfig(home);
    let (kubeconfig, context) = match (kubeconfig_choice, context_choice) {
        (Some(k), Some(c)) => (Some(k), c.to_string()),
        (Some(k), None) => (Some(k), OWN_CONTEXT.to_string()),
        (None, Some(c)) => (None, c.to_string()),
        (None, None) if own.is_file() => (Some(own), OWN_CONTEXT.to_string()),
        (None, None) => {
            return Err((
                FloorUnarmed::NoCluster,
                format!(
                    "no cluster is configured for this daemon: run `wicked-core cluster install` \
                     or set {CONTEXT_ENV} (the current kubectl context is never used implicitly)"
                ),
            ))
        }
    };
    Ok(Kube {
        kubectl,
        kubeconfig,
        context,
        owner: owner_id(home),
    })
}

/// The operational home the daemon's actor runs on (first noted wins: one daemon per process).
static ENGINE_HOME: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// Note the daemon's operational home (the actor, at start), for engine work that has no runner
/// at hand: the checks floor's pods must carry the same owner as the seats' (B-3).
pub(crate) fn note_engine_home(home: &Path) {
    if let Ok(mut g) = ENGINE_HOME.lock() {
        if g.is_none() {
            *g = Some(home.to_path_buf());
        }
    }
}

/// Forget the noted home (live tests: each installs into its own throwaway home).
#[cfg(test)]
pub(crate) fn forget_engine_home_for_test() {
    if let Ok(mut g) = ENGINE_HOME.lock() {
        *g = None;
    }
}

/// The daemon's operational home as noted by its actor, else the worker home base (a floor run
/// outside a daemon).
pub(crate) fn engine_home() -> Option<PathBuf> {
    ENGINE_HOME
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .or_else(|| wicked_apps_core::spawn::worker_home_base().ok())
}

/// The engine's cluster for the daemon whose operational home is `home`, from the environment.
pub(crate) fn kube_for(home: &Path) -> Result<Kube, (FloorUnarmed, String)> {
    let user_home = std::env::var_os("HOME").map(PathBuf::from);
    resolve_settings(
        home,
        std::env::var(KUBECTL_ENV).ok().as_deref(),
        std::env::var(KUBECONFIG_ENV).ok().as_deref(),
        std::env::var(CONTEXT_ENV).ok().as_deref(),
        user_home.as_deref(),
        crate::validator::find_on_path,
    )
}

/// A run reached a terminal state (B-5; called off the actor thread with every terminal
/// transition): its namespace goes, with its pods, replica and per-run cache claims. A resumed
/// run gets a new one. Nothing happens when the daemon has no cluster configured.
pub(crate) fn release_run(run_id: &str) {
    let Some(home) = engine_home() else {
        return;
    };
    let Ok(kube) = kube_for(&home) else {
        return;
    };
    if !kube.kubeconfig.as_ref().is_some_and(|k| k.is_file()) && std::env::var(CONTEXT_ENV).is_err()
    {
        return;
    }
    if let Err(e) = cluster::delete_run(&kube, run_id, false) {
        eprintln!("wicked-core: run {run_id}'s namespace was not deleted: {e}");
    }
}

/// The daemon's boot sweep (B-5), off the actor thread: the namespaces of runs that are not
/// `live` (terminal, or unknown to the store) go; a live run keeps its namespace and loses only
/// the pods an earlier engine left. A namespace created after this boot is never touched. The
/// daemon's shared session namespace is live.
pub(crate) fn boot_sweep(live: std::collections::HashSet<String>) {
    let started = rfc3339(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    );
    let _ = std::thread::Builder::new()
        .name("wicked-k8s-boot-sweep".into())
        .spawn(move || {
            let Some(home) = engine_home() else {
                return;
            };
            let Ok(kube) = kube_for(&home) else {
                return;
            };
            if cluster::probe_cached(&kube).is_err() {
                return;
            }
            let is_live = |run: &str| run == SESSIONS_RUN || live.contains(run);
            match cluster::sweep(&kube, &is_live, Some(&started)) {
                Ok(done) if !done.is_empty() => {
                    eprintln!("wicked-core: the boot sweep removed {}", done.join(", "))
                }
                Ok(_) => {}
                Err(e) => eprintln!("wicked-core: the cluster boot sweep did not finish: {e}"),
            }
        });
}

/// The run id of the daemon's shared one-shot session namespace.
pub(crate) const SESSIONS_RUN: &str = "sessions";

/// One kubectl call's result.
#[derive(Debug)]
pub(crate) struct Called {
    pub(crate) code: Option<i32>,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

impl Called {
    pub(crate) fn ok(&self) -> bool {
        self.code == Some(0)
    }
    pub(crate) fn err_text(&self) -> String {
        let e = String::from_utf8_lossy(&self.stderr).trim().to_string();
        if e.is_empty() {
            match self.code {
                Some(c) => format!("kubectl exited {c}"),
                None => "kubectl timed out".to_string(),
            }
        } else {
            e
        }
    }
}

impl Kube {
    /// The pinned argv head: kubectl, then the kubeconfig, the context and (when given) the
    /// namespace. Pure, so the pinning is a tested property.
    pub(crate) fn head(&self, ns: Option<&str>) -> Vec<String> {
        let mut v = vec![self.kubectl.to_string_lossy().into_owned()];
        if let Some(k) = &self.kubeconfig {
            v.push("--kubeconfig".into());
            v.push(k.to_string_lossy().into_owned());
        }
        v.push("--context".into());
        v.push(self.context.clone());
        if let Some(ns) = ns {
            v.push("--namespace".into());
            v.push(ns.to_string());
        }
        v
    }

    /// A hardened kubectl [`Command`] with the pinned head (stdin closed unless the caller
    /// replaces it). `KUBECONFIG` is removed, so only the pinned file is read.
    pub(crate) fn cmd(&self, ns: Option<&str>) -> Command {
        let head = self.head(ns);
        let mut c = Command::new(&head[0]);
        c.hardened().stdin(Stdio::null()).args(&head[1..]);
        c.env_remove("KUBECONFIG");
        if let Some(dir) = self.kubectl.parent().filter(|d| d.is_absolute()) {
            let mut dirs = vec![dir.to_path_buf()];
            if let Some(p) = std::env::var_os("PATH") {
                dirs.extend(std::env::split_paths(&p).filter(|d| d != dir));
            }
            if let Ok(p) = std::env::join_paths(dirs) {
                c.env("PATH", p);
            }
        }
        c
    }

    /// Run kubectl with `args` (after the pinned head), feeding `stdin` when given, bounded by
    /// `limit` (the whole group is killed past it). Never fails: the result says what happened.
    pub(crate) fn call(
        &self,
        ns: Option<&str>,
        args: &[&str],
        stdin: Option<&[u8]>,
        limit: Duration,
    ) -> Called {
        let mut cmd = self.cmd(ns);
        cmd.args(args);
        run_bounded(cmd, stdin, limit)
    }

    /// [`Kube::call`] with stdout capped at `cap` bytes ([`run_capped`]).
    pub(crate) fn call_capped(
        &self,
        ns: Option<&str>,
        args: &[&str],
        stdin: Option<&[u8]>,
        limit: Duration,
        cap: u64,
    ) -> Called {
        let mut cmd = self.cmd(ns);
        cmd.args(args);
        run_capped(cmd, stdin, limit, cap)
    }

    /// `kubectl apply --server-side` of `docs` (a JSON `List`): idempotent, and large CRDs fit
    /// (a client-side apply's annotation would not).
    pub(crate) fn apply(&self, docs: &[Value]) -> Result<(), String> {
        let body = serde_json::to_vec(&render::list(docs)).map_err(|e| e.to_string())?;
        let r = self.call(
            None,
            &[
                "apply",
                "--server-side",
                "--force-conflicts",
                "--field-manager",
                MANAGED_BY,
                "-f",
                "-",
            ],
            Some(&body),
            CALL_TIMEOUT,
        );
        if r.ok() {
            Ok(())
        } else {
            Err(r.err_text())
        }
    }

    /// The object `kind/name` as JSON, `Ok(None)` when it does not exist.
    pub(crate) fn get_json(
        &self,
        ns: Option<&str>,
        kind: &str,
        name: &str,
    ) -> Result<Option<Value>, String> {
        let r = self.call(
            ns,
            &["get", kind, name, "--ignore-not-found", "-o", "json"],
            None,
            CALL_TIMEOUT,
        );
        if !r.ok() {
            return Err(r.err_text());
        }
        if r.stdout.iter().all(u8::is_ascii_whitespace) {
            return Ok(None);
        }
        serde_json::from_slice(&r.stdout)
            .map(Some)
            .map_err(|e| e.to_string())
    }

    /// The objects of `kind` carrying this daemon's owner label (plus `extra` selectors).
    pub(crate) fn list_owned(
        &self,
        ns: Option<&str>,
        kind: &str,
        extra: &[(&str, &str)],
    ) -> Result<Vec<Value>, String> {
        let selector = owner_selector(&self.owner, extra);
        let r = self.call(
            ns,
            &["get", kind, "-l", &selector, "-o", "json"],
            None,
            CALL_TIMEOUT,
        );
        if !r.ok() {
            return Err(r.err_text());
        }
        let v: Value = serde_json::from_slice(&r.stdout).map_err(|e| e.to_string())?;
        Ok(v.get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// Delete `kind/name` only when it carries this daemon's owner label: an object the engine
    /// did not create is never deleted, whatever its name. `Ok(false)` = there was nothing of
    /// ours to delete.
    pub(crate) fn delete_owned(
        &self,
        ns: Option<&str>,
        kind: &str,
        name: &str,
        wait: bool,
    ) -> Result<bool, String> {
        let Some(obj) = self.get_json(ns, kind, name)? else {
            return Ok(false);
        };
        if !is_owned_by(&obj, &self.owner) {
            return Err(format!(
                "{kind}/{name} is not this daemon's (its {OWNER_LABEL} label is not {}); not deleted",
                self.owner
            ));
        }
        let wait_flag = format!("--wait={wait}");
        let r = self.call(
            ns,
            &[
                "delete",
                kind,
                name,
                "--ignore-not-found",
                &wait_flag,
                "--grace-period=0",
            ],
            None,
            if wait {
                Duration::from_secs(180)
            } else {
                CALL_TIMEOUT
            },
        );
        if r.ok() {
            Ok(true)
        } else {
            Err(r.err_text())
        }
    }

    /// Whether the API server answers now (`kubectl version` reaches the server).
    pub(crate) fn answers(&self) -> bool {
        self.call(None, &["version", "-o", "json"], None, PROBE_TIMEOUT)
            .ok()
    }
}

/// The label selector for this daemon's objects, with `extra` `key=value` pairs.
pub(crate) fn owner_selector(owner: &str, extra: &[(&str, &str)]) -> String {
    let mut s = format!("{OWNER_LABEL}={owner}");
    for (k, v) in extra {
        s.push(',');
        s.push_str(k);
        s.push('=');
        s.push_str(v);
    }
    s
}

/// Whether `obj`'s metadata carries `owner` in [`OWNER_LABEL`].
pub(crate) fn is_owned_by(obj: &Value, owner: &str) -> bool {
    obj.pointer("/metadata/labels")
        .and_then(|l| l.get(OWNER_LABEL))
        .and_then(Value::as_str)
        == Some(owner)
}

/// Run `cmd` to completion within `limit`, feeding `stdin` on its own thread, draining both
/// pipes; past the deadline the child's whole process group is killed.
pub(crate) fn run_bounded(cmd: Command, stdin: Option<&[u8]>, limit: Duration) -> Called {
    run_capped(cmd, stdin, limit, u64::MAX)
}

/// [`run_bounded`], killing the call when its stdout passes `cap` bytes (review r1: a seat's
/// replica must not stream more into the engine than the write-back will ever accept). A capped
/// call answers no exit code and says so on stderr.
pub(crate) fn run_capped(
    mut cmd: Command,
    stdin: Option<&[u8]>,
    limit: Duration,
    cap: u64,
) -> Called {
    use std::io::Read;
    let over = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    if stdin.is_some() {
        cmd.stdin(Stdio::piped());
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Called {
                code: None,
                stdout: Vec::new(),
                stderr: format!("kubectl could not start: {e}").into_bytes(),
            }
        }
    };
    // The feeder reports on a channel, so the deadline holds while it writes: a descendant that
    // keeps the stdin pipe open can never block this call past `limit` (review r2 P2).
    let feeder = stdin.map(|body| {
        let body = body.to_vec();
        let pipe = child.stdin.take();
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            if let Some(mut p) = pipe {
                let _ = p.write_all(&body);
            }
            let _ = tx.send(());
        });
        rx
    });
    let drain = |p: Option<Box<dyn Read + Send>>,
                 cap: u64,
                 over: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(p) = p {
                let _ = p.take(cap.saturating_add(1)).read_to_end(&mut buf);
            }
            if buf.len() as u64 > cap {
                if let Some(o) = over {
                    o.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                buf.clear();
            }
            let _ = tx.send(buf);
        });
        rx
    };
    let out = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
        cap,
        Some(std::sync::Arc::clone(&over)),
    );
    let err = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
        1 << 20,
        None,
    );
    let start = Instant::now();
    let code = loop {
        match child.try_wait() {
            Ok(Some(s)) if !over.load(std::sync::atomic::Ordering::SeqCst) => {
                break Some(s.code().unwrap_or(-1))
            }
            Ok(None)
                if start.elapsed() < limit && !over.load(std::sync::atomic::Ordering::SeqCst) =>
            {
                std::thread::sleep(Duration::from_millis(25))
            }
            _ => {
                #[cfg(unix)]
                // SAFETY: killpg on the child's own process group (pgid == its pid).
                unsafe {
                    libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
                }
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let grace = Duration::from_secs(2);
    if let Some(f) = feeder {
        let left = limit.saturating_sub(start.elapsed()).max(grace);
        if f.recv_timeout(left).is_err() {
            #[cfg(unix)]
            // SAFETY: killpg on the child's own process group (pgid == its pid).
            unsafe {
                libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
            }
        }
    }
    let stdout = out.recv_timeout(grace).unwrap_or_default();
    if over.load(std::sync::atomic::Ordering::SeqCst) {
        return Called {
            code: None,
            stdout: Vec::new(),
            stderr: format!("the output passed its {cap}-byte cap; the call was stopped")
                .into_bytes(),
        };
    }
    Called {
        code,
        stdout,
        stderr: err.recv_timeout(grace).unwrap_or_default(),
    }
}

/// `now + after` as an RFC 3339 UTC timestamp (a Sandbox's `shutdownTime`).
pub(crate) fn rfc3339_after(after: Duration) -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        + after.as_secs();
    rfc3339(secs)
}

/// Unix seconds as `YYYY-MM-DDTHH:MM:SSZ` (Howard Hinnant's civil-from-days).
pub(crate) fn rfc3339(unix: u64) -> String {
    let days = (unix / 86_400) as i64;
    let rem = unix % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}
