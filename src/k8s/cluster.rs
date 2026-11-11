//! The cluster's lifecycle for one daemon: the probe and its honest failure kinds, the one-time
//! install (the pinned agent-sandbox controller, this daemon's namespace and a ServiceAccount
//! scoped to its own objects), the daemon's gateway, a run's namespace, and the sweep.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::render::{self, GatewaySpec, HostPort, RunBudget};
use super::{
    daemon_namespace_name, label_value, run_namespace_name, Kube, AGENT_SANDBOX_IMAGE,
    AGENT_SANDBOX_NAMESPACE, AGENT_SANDBOX_SHA256, OWNER_LABEL, OWN_CONTEXT, ROLE_LABEL, RUN_LABEL,
};
use crate::worker_sandbox::FloorUnarmed;

/// The pinned agent-sandbox release manifest, vendored byte for byte.
pub(crate) const AGENT_SANDBOX_MANIFEST: &str =
    include_str!("../../containers/k8s/agent-sandbox-v1.0.10.yaml");

/// The engine's ServiceAccount (in the daemon namespace).
pub(crate) const ENGINE_SA: &str = "wicked-engine";

/// `Err` unless `manifest` is the pinned release byte for byte.
pub(crate) fn check_manifest(manifest: &str) -> Result<(), String> {
    let got: String = Sha256::digest(manifest.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if got == AGENT_SANDBOX_SHA256 {
        Ok(())
    } else {
        Err(format!(
            "the agent-sandbox manifest is not the pinned release (sha256 {got}, pinned \
             {AGENT_SANDBOX_SHA256}); not applied"
        ))
    }
}

/// Whether the cluster can run seats now: the API server answers, the controller runs the
/// pinned image, and its CRDs are present. The `Err` is the refusal kind and its detail.
pub(crate) fn probe(kube: &Kube) -> Result<(), (FloorUnarmed, String)> {
    if !kube.answers() {
        return Err((
            FloorUnarmed::ClusterUnreachable,
            format!("context {} did not answer", kube.context),
        ));
    }
    controller_ready(kube).map_err(|e| (FloorUnarmed::ControllerMissing, e))
}

/// The controller's state, judged from its Deployment and CRD.
pub(crate) fn controller_ready(kube: &Kube) -> Result<(), String> {
    let dep = kube
        .get_json(
            Some(AGENT_SANDBOX_NAMESPACE),
            "deployment",
            "agent-sandbox-controller",
        )?
        .ok_or("the agent-sandbox controller is not installed")?;
    judge_controller(&dep)?;
    kube.get_json(None, "crd", "sandboxes.agents.x-k8s.io")?
        .ok_or("the agent-sandbox CRDs are not installed")?;
    Ok(())
}

/// Pure half of [`controller_ready`]: the Deployment runs the pinned image and is available.
pub(crate) fn judge_controller(dep: &Value) -> Result<(), String> {
    let images: Vec<&str> = dep
        .pointer("/spec/template/spec/containers")
        .and_then(Value::as_array)
        .map(|cs| {
            cs.iter()
                .filter_map(|c| c.get("image").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    if !images.contains(&AGENT_SANDBOX_IMAGE) {
        return Err(format!(
            "the agent-sandbox controller runs {images:?}, not the pinned {AGENT_SANDBOX_IMAGE}"
        ));
    }
    let available = dep
        .pointer("/status/availableReplicas")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if available == 0 {
        return Err("the agent-sandbox controller is not available yet".into());
    }
    Ok(())
}

/// The engine ServiceAccount's username, as the API server sees it.
pub(crate) fn engine_username(owner: &str) -> String {
    format!(
        "system:serviceaccount:{}:{ENGINE_SA}",
        daemon_namespace_name(owner)
    )
}

/// The objects `cluster install` applies for one daemon (with the admin context): its
/// namespace, the engine's ServiceAccount and token, the role it is bound to, and an admission
/// policy that refuses that account any create, update or delete of an object that does not
/// carry this daemon's owner label (D5): the account cannot touch what it does not own, even by
/// a bug.
pub(crate) fn install_docs(owner: &str) -> Vec<Value> {
    let ns = daemon_namespace_name(owner);
    let labels = render::labels(owner, &[]);
    let rules = json!([
        {"apiGroups": [""], "resources": ["namespaces"],
         "verbs": ["get", "list", "watch", "create", "patch", "delete"]},
        {"apiGroups": [""],
         "resources": ["pods", "pods/log", "persistentvolumeclaims", "services", "resourcequotas",
                       "limitranges", "configmaps", "events"],
         "verbs": ["get", "list", "watch", "create", "patch", "delete"]},
        {"apiGroups": [""], "resources": ["pods/exec"], "verbs": ["create", "get"]},
        {"apiGroups": [""], "resources": ["persistentvolumes"],
         "verbs": ["get", "list", "watch", "create", "patch", "delete"]},
        {"apiGroups": ["networking.k8s.io"], "resources": ["networkpolicies"],
         "verbs": ["get", "list", "watch", "create", "patch", "delete"]},
        {"apiGroups": ["apps"], "resources": ["deployments"],
         "verbs": ["get", "list", "watch", "create", "patch", "delete"]},
        {"apiGroups": ["batch"], "resources": ["jobs"],
         "verbs": ["get", "list", "watch", "create", "patch", "delete"]},
        {"apiGroups": ["agents.x-k8s.io"], "resources": ["sandboxes"],
         "verbs": ["get", "list", "watch", "create", "patch", "delete"]},
        {"apiGroups": ["extensions.agents.x-k8s.io"],
         "resources": ["sandboxclaims", "sandboxtemplates", "sandboxwarmpools"],
         "verbs": ["get", "list", "watch", "create", "patch", "delete"]},
        {"apiGroups": ["apiextensions.k8s.io"], "resources": ["customresourcedefinitions"],
         "verbs": ["get"]},
    ]);
    let user = engine_username(owner);
    let cel_owner = format!(
        "has(o.metadata.labels) && '{OWNER_LABEL}' in o.metadata.labels && \
         o.metadata.labels['{OWNER_LABEL}'] == '{owner}'"
    );
    vec![
        render::namespace(&ns, owner, &[(ROLE_LABEL, "daemon")]),
        json!({"apiVersion": "v1", "kind": "ServiceAccount",
               "metadata": {"name": ENGINE_SA, "namespace": ns, "labels": labels},
               "automountServiceAccountToken": false}),
        json!({"apiVersion": "v1", "kind": "Secret", "type": "kubernetes.io/service-account-token",
               "metadata": {"name": format!("{ENGINE_SA}-token"), "namespace": ns, "labels": labels,
                            "annotations": {"kubernetes.io/service-account.name": ENGINE_SA}}}),
        json!({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "ClusterRole",
               "metadata": {"name": format!("wicked-engine-{owner}"), "labels": labels},
               "rules": rules}),
        json!({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "ClusterRoleBinding",
               "metadata": {"name": format!("wicked-engine-{owner}"), "labels": labels},
               "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole",
                           "name": format!("wicked-engine-{owner}")},
               "subjects": [{"kind": "ServiceAccount", "name": ENGINE_SA, "namespace": ns}]}),
        json!({"apiVersion": "admissionregistration.k8s.io/v1", "kind": "ValidatingAdmissionPolicy",
               "metadata": {"name": format!("wicked-engine-owns-{owner}"), "labels": labels},
               "spec": {
                   "failurePolicy": "Fail",
                   "matchConstraints": {"resourceRules": [{
                       "apiGroups": ["*"], "apiVersions": ["*"],
                       "operations": ["CREATE", "UPDATE", "DELETE"], "resources": ["*"],
                   }]},
                   "matchConditions": [{"name": "the-engine-account",
                                        "expression": format!("request.userInfo.username == '{user}'")}],
                   "variables": [{"name": "o",
                                  "expression": "request.operation == 'DELETE' ? oldObject : object"}],
                   "validations": [{
                       "expression": format!(
                           "request.subResource != '' || variables.o == null || ({})",
                           cel_owner.replace("o.metadata", "variables.o.metadata")),
                       "message": format!("the wicked engine may only change objects labelled {OWNER_LABEL}={owner}"),
                   }],
               }}),
        json!({"apiVersion": "admissionregistration.k8s.io/v1", "kind": "ValidatingAdmissionPolicyBinding",
               "metadata": {"name": format!("wicked-engine-owns-{owner}"), "labels": labels},
               "spec": {"policyName": format!("wicked-engine-owns-{owner}"),
                        "validationActions": ["Deny"]}}),
    ]
}

/// The kubeconfig the engine runs on: the admin context's cluster (server and CA), the engine
/// account's token, and one context named [`OWN_CONTEXT`]. Pure.
pub(crate) fn engine_kubeconfig(
    server: &str,
    ca_data: Option<&str>,
    insecure: bool,
    token: &str,
) -> Value {
    let mut cluster = json!({"server": server});
    if let Some(ca) = ca_data {
        cluster["certificate-authority-data"] = json!(ca);
    }
    if insecure {
        cluster["insecure-skip-tls-verify"] = json!(true);
    }
    json!({
        "apiVersion": "v1",
        "kind": "Config",
        "clusters": [{"name": OWN_CONTEXT, "cluster": cluster}],
        "users": [{"name": OWN_CONTEXT, "user": {"token": token}}],
        "contexts": [{"name": OWN_CONTEXT, "context": {"cluster": OWN_CONTEXT, "user": OWN_CONTEXT}}],
        "current-context": OWN_CONTEXT,
    })
}

/// `cluster install`: with the operator's admin context, apply the pinned controller and this
/// daemon's scoped account, then write the engine's kubeconfig under `home` (mode 0600).
pub(crate) fn install(admin: &Kube, home: &Path) -> Result<PathBuf, String> {
    check_manifest(AGENT_SANDBOX_MANIFEST)?;
    let r = admin.call(
        None,
        &[
            "apply",
            "--server-side",
            "--force-conflicts",
            "--field-manager",
            super::MANAGED_BY,
            "-f",
            "-",
        ],
        Some(AGENT_SANDBOX_MANIFEST.as_bytes()),
        Duration::from_secs(180),
    );
    if !r.ok() {
        return Err(format!("the agent-sandbox manifest did not apply: {}", r.err_text()));
    }
    let rollout = admin.call(
        Some(AGENT_SANDBOX_NAMESPACE),
        &[
            "rollout",
            "status",
            "deployment/agent-sandbox-controller",
            "--timeout=150s",
        ],
        None,
        Duration::from_secs(170),
    );
    if !rollout.ok() {
        return Err(format!(
            "the agent-sandbox controller did not become available: {}",
            rollout.err_text()
        ));
    }
    admin.apply(&install_docs(&admin.owner))?;
    let ns = daemon_namespace_name(&admin.owner);
    let mut token = String::new();
    for _ in 0..40 {
        if let Some(sec) = admin.get_json(Some(&ns), "secret", &format!("{ENGINE_SA}-token"))? {
            if let Some(t) = sec.pointer("/data/token").and_then(Value::as_str) {
                token = decode_b64(t)?;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    if token.is_empty() {
        return Err("the engine account's token was not issued".into());
    }
    let view = admin.call(
        None,
        &["config", "view", "--raw", "--minify", "-o", "json"],
        None,
        super::CALL_TIMEOUT,
    );
    if !view.ok() {
        return Err(view.err_text());
    }
    let v: Value = serde_json::from_slice(&view.stdout).map_err(|e| e.to_string())?;
    let cluster = v
        .pointer("/clusters/0/cluster")
        .ok_or("the admin context names no cluster")?;
    let server = cluster
        .get("server")
        .and_then(Value::as_str)
        .ok_or("the admin context's cluster has no server")?;
    let cfg = engine_kubeconfig(
        server,
        cluster
            .get("certificate-authority-data")
            .and_then(Value::as_str),
        cluster
            .get("insecure-skip-tls-verify")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        &token,
    );
    let path = super::own_kubeconfig(home);
    let body = serde_json::to_vec_pretty(&cfg).map_err(|e| e.to_string())?;
    write_private(&path, &body)?;
    Ok(path)
}

fn decode_b64(s: &str) -> Result<String, String> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(s.trim())
        .map_err(|e| e.to_string())?;
    String::from_utf8(bytes).map_err(|e| e.to_string())
}

fn write_private(path: &Path, body: &[u8]) -> Result<(), String> {
    use std::io::Write as _;
    let dir = path.parent().ok_or("no parent dir")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let tmp = dir.join(format!(".kubeconfig-{}", uuid::Uuid::new_v4().simple()));
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut f = opts.open(&tmp).map_err(|e| e.to_string())?;
        f.write_all(body).map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
    }
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/// `cluster uninstall`: delete every object labelled with this daemon's owner (its run
/// namespaces, its own namespace, its cluster-scoped role, binding, policy and volumes).
/// The shared agent-sandbox controller stays unless `controller` is set. Never touches an
/// object without the label.
pub(crate) fn uninstall(admin: &Kube, controller: bool) -> Result<Vec<String>, String> {
    let mut gone = Vec::new();
    let sel = super::owner_selector(&admin.owner, &[]);
    for kind in [
        "namespaces",
        "clusterrolebindings",
        "clusterroles",
        "validatingadmissionpolicybindings",
        "validatingadmissionpolicies",
        "persistentvolumes",
    ] {
        let r = admin.call(
            None,
            &["delete", kind, "-l", &sel, "--ignore-not-found", "--wait=true", "-o", "name"],
            None,
            Duration::from_secs(300),
        );
        if !r.ok() {
            return Err(format!("{kind}: {}", r.err_text()));
        }
        gone.extend(
            String::from_utf8_lossy(&r.stdout)
                .lines()
                .map(str::to_string),
        );
    }
    if controller {
        let r = admin.call(
            None,
            &["delete", "--ignore-not-found", "--wait=true", "-f", "-"],
            Some(AGENT_SANDBOX_MANIFEST.as_bytes()),
            Duration::from_secs(300),
        );
        if !r.ok() {
            return Err(format!("the controller: {}", r.err_text()));
        }
        gone.push("agent-sandbox controller".into());
    }
    Ok(gone)
}

/// The daemon's gateway, as the seats reach it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DaemonGateway {
    pub(crate) namespace: String,
    /// The `agw` Service's cluster IP: a seat's `hostAliases` entry.
    pub(crate) service_ip: String,
    /// The running gateway pod (the writer sidecar is exec'd into).
    pub(crate) pod: String,
}

/// Everything of the daemon namespace besides what `install` made: its policies, the gateway's
/// volumes, Deployment and Service.
pub(crate) fn daemon_docs(g: &GatewaySpec, host: &[HostPort]) -> Vec<Value> {
    let ports = render::seat_gateway_ports();
    vec![
        render::default_deny(&g.namespace, &g.owner),
        render::gateway_ingress(&g.namespace, &g.owner, ports),
        render::gateway_egress(&g.namespace, &g.owner, host),
        render::fill_egress(&g.namespace, &g.owner),
        render::pvc(&g.namespace, &g.owner, render::GATEWAY_CONFIG_PVC, "64Mi"),
        render::pvc(&g.namespace, &g.owner, render::GATEWAY_BROKER_PVC, "64Mi"),
        render::gateway_deployment(g),
        render::gateway_service(g, ports),
    ]
}

/// Apply the daemon's gateway and wait until its pod runs; answer how seats reach it.
pub(crate) fn ensure_gateway(
    kube: &Kube,
    g: &GatewaySpec,
    host: &[HostPort],
) -> Result<DaemonGateway, String> {
    kube.apply(&daemon_docs(g, host))?;
    let rollout = kube.call(
        Some(&g.namespace),
        &[
            "rollout",
            "status",
            &format!("deployment/{}", render::GATEWAY_NAME),
            "--timeout=90s",
        ],
        None,
        Duration::from_secs(100),
    );
    if !rollout.ok() {
        return Err(format!(
            "the gateway did not become ready: {}",
            rollout.err_text()
        ));
    }
    let svc = kube
        .get_json(Some(&g.namespace), "service", render::GATEWAY_NAME)?
        .ok_or("the gateway Service is missing")?;
    let service_ip = svc
        .pointer("/spec/clusterIP")
        .and_then(Value::as_str)
        .filter(|ip| !ip.is_empty() && *ip != "None")
        .ok_or("the gateway Service has no cluster IP")?
        .to_string();
    let pods = kube.list_owned(Some(&g.namespace), "pods", &[(ROLE_LABEL, "gateway")])?;
    let pod = running_pod(&pods).ok_or("no gateway pod is running")?;
    Ok(DaemonGateway {
        namespace: g.namespace.clone(),
        service_ip,
        pod,
    })
}

/// The name of the first pod of `pods` that is Running with every container ready and is not
/// being deleted.
pub(crate) fn running_pod(pods: &[Value]) -> Option<String> {
    pods.iter()
        .find(|p| {
            p.pointer("/metadata/deletionTimestamp").is_none()
                && p.pointer("/status/phase").and_then(Value::as_str) == Some("Running")
                && p.pointer("/status/containerStatuses")
                    .and_then(Value::as_array)
                    .is_some_and(|cs| {
                        !cs.is_empty()
                            && cs
                                .iter()
                                .all(|c| c.get("ready").and_then(Value::as_bool) == Some(true))
                    })
        })
        .and_then(|p| p.pointer("/metadata/name").and_then(Value::as_str))
        .map(str::to_string)
}

/// A run's namespace for this daemon: created with its quota, policies and replica volume
/// (idempotent). Answers the namespace name.
pub(crate) fn ensure_run(
    kube: &Kube,
    run_id: &str,
    budget: &RunBudget,
    replica_size: &str,
) -> Result<String, String> {
    let ns = run_namespace_name(&kube.owner, run_id);
    let docs = render::run_namespace_docs(
        &ns,
        &kube.owner,
        &label_value(run_id),
        &daemon_namespace_name(&kube.owner),
        budget,
        replica_size,
    );
    kube.apply(&docs)?;
    Ok(ns)
}

/// Delete a run's namespace (and everything in it) if it is this daemon's.
pub(crate) fn delete_run(kube: &Kube, run_id: &str, wait: bool) -> Result<bool, String> {
    let ns = run_namespace_name(&kube.owner, run_id);
    kube.delete_owned(None, "namespace", &ns, wait)
}

/// What a sweep does with each of this daemon's run namespaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SweepAction {
    /// The run is over (or unknown to this engine): its namespace goes.
    DeleteNamespace(String),
    /// The run is live: only the pods an earlier engine process created go.
    ReapStalePods(String),
}

/// Pure half of [`sweep`]: decide per namespace. `live` answers whether a run id is still
/// executing in this engine. A namespace without this daemon's labels is never in the list.
pub(crate) fn sweep_plan(namespaces: &[Value], owner: &str, live: &dyn Fn(&str) -> bool) -> Vec<SweepAction> {
    namespaces
        .iter()
        .filter(|n| super::is_owned_by(n, owner))
        .filter(|n| {
            n.pointer("/metadata/labels")
                .and_then(|l| l.get(ROLE_LABEL))
                .and_then(Value::as_str)
                == Some("run")
        })
        .filter_map(|n| {
            let name = n.pointer("/metadata/name").and_then(Value::as_str)?;
            let run = n
                .pointer("/metadata/labels")
                .and_then(|l| l.get(RUN_LABEL))
                .and_then(Value::as_str)
                .unwrap_or("");
            Some(if live(run) {
                SweepAction::ReapStalePods(name.to_string())
            } else {
                SweepAction::DeleteNamespace(name.to_string())
            })
        })
        .collect()
}

/// The boot sweep: terminal runs' namespaces are deleted; in a live run, sandboxes an earlier
/// engine process created (another [`super::BOOT_LABEL`]) are deleted. Answers what it did.
pub(crate) fn sweep(kube: &Kube, live: &dyn Fn(&str) -> bool) -> Result<Vec<String>, String> {
    let namespaces = kube.list_owned(None, "namespaces", &[(ROLE_LABEL, "run")])?;
    let mut done = Vec::new();
    for action in sweep_plan(&namespaces, &kube.owner, live) {
        match action {
            SweepAction::DeleteNamespace(ns) => {
                if kube.delete_owned(None, "namespace", &ns, false)? {
                    done.push(format!("namespace/{ns}"));
                }
            }
            SweepAction::ReapStalePods(ns) => {
                let selector = format!(
                    "{},{}!={}",
                    super::owner_selector(&kube.owner, &[]),
                    super::BOOT_LABEL,
                    super::boot_id()
                );
                let r = kube.call(
                    Some(&ns),
                    &[
                        "delete",
                        "sandboxes.agents.x-k8s.io",
                        "-l",
                        &selector,
                        "--ignore-not-found",
                        "--wait=false",
                        "-o",
                        "name",
                    ],
                    None,
                    super::CALL_TIMEOUT,
                );
                if !r.ok() {
                    return Err(r.err_text());
                }
                done.extend(
                    String::from_utf8_lossy(&r.stdout)
                        .lines()
                        .map(|l| format!("{ns}/{l}")),
                );
            }
        }
    }
    Ok(done)
}

/// How long a pids-limit verdict is trusted before the probe runs again.
const PIDS_PROBE_EVERY: Duration = Duration::from_secs(60 * 60);

/// The last pids-limit verdict per cluster context, with when it was taken.
static PIDS_VERDICT: std::sync::Mutex<Option<(String, std::time::Instant, u32)>> =
    std::sync::Mutex::new(None);

/// Whether pods on this cluster are held to a pids limit (PARITY B2): measured by the probe pod
/// ([`render::pids_probe_pod`]) in the daemon namespace, cached for an hour per context. Only a
/// pass is cached, so a fixed cluster is seen at the next arm. `Err` = refuse every seat with
/// [`FloorUnarmed::PidsUnlimited`].
pub(crate) fn ensure_pids_limited(kube: &Kube, floor_image: &str) -> Result<u32, (FloorUnarmed, String)> {
    if let Some((ctx, at, n)) = PIDS_VERDICT.lock().ok().and_then(|g| g.clone()) {
        if ctx == kube.context && at.elapsed() < PIDS_PROBE_EVERY {
            return Ok(n);
        }
    }
    let ns = daemon_namespace_name(&kube.owner);
    let name = format!("pids-probe-{}", super::boot_id());
    let infra = |e: String| (FloorUnarmed::PolicyRefused, e);
    let _ = kube.delete_owned(Some(&ns), "pod", &name, true);
    kube.apply(&[render::pids_probe_pod(&ns, &kube.owner, &name, floor_image)])
        .map_err(infra)?;
    let start = std::time::Instant::now();
    let phase = loop {
        let pod = kube.get_json(Some(&ns), "pod", &name).map_err(infra)?;
        let phase = pod
            .as_ref()
            .and_then(|p| p.pointer("/status/phase"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if phase == "Succeeded" || phase == "Failed" {
            break phase;
        }
        if start.elapsed() > Duration::from_secs(120) {
            let _ = kube.delete_owned(Some(&ns), "pod", &name, false);
            return Err(infra(format!(
                "the pids-limit probe did not finish (phase {phase:?})"
            )));
        }
        std::thread::sleep(Duration::from_millis(500));
    };
    let logs = kube.call(Some(&ns), &["logs", &name], None, super::CALL_TIMEOUT);
    let _ = kube.delete_owned(Some(&ns), "pod", &name, false);
    if !logs.ok() {
        return Err(infra(format!(
            "the pids-limit probe's log could not be read ({phase}): {}",
            logs.err_text()
        )));
    }
    let n = render::pids_probe_verdict(&String::from_utf8_lossy(&logs.stdout))
        .map_err(|e| (FloorUnarmed::PidsUnlimited, e))?;
    if let Ok(mut g) = PIDS_VERDICT.lock() {
        *g = Some((kube.context.clone(), std::time::Instant::now(), n));
    }
    Ok(n)
}
