//! The cluster's lifecycle for one daemon: the probe and its honest failure kinds, the one-time
//! install (the pinned agent-sandbox controller, this daemon's namespace and a ServiceAccount
//! scoped to its own objects), the daemon's gateway, a run's namespace, and the sweep.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::render::{self, GatewaySpec, HostPort, RunBudget};
use super::{
    daemon_namespace_name, run_namespace_name, Kube, AGENT_SANDBOX_IMAGE, AGENT_SANDBOX_NAMESPACE,
    AGENT_SANDBOX_SHA256, OWNER_LABEL, OWN_CONTEXT, ROLE_LABEL,
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

/// [`probe`], with a pass kept for a minute per context (a seat arm probes the cluster once,
/// not once per call). A failure is never kept.
pub(crate) fn probe_cached(kube: &Kube) -> Result<(), (FloorUnarmed, String)> {
    static LAST: std::sync::Mutex<Option<(Kube, std::time::Instant)>> = std::sync::Mutex::new(None);
    if let Ok(last) = LAST.lock() {
        if let Some((k, at)) = last.as_ref() {
            if k == kube && at.elapsed() < Duration::from_secs(60) {
                return Ok(());
            }
        }
    }
    probe(kube)?;
    // The engine's own token is renewed while the cluster answers (B-5): a 30-day TokenRequest,
    // renewed inside its last week. Only the engine's own context (an operator's kubeconfig is
    // never rewritten); a failed renewal is reported and the old token used until it expires.
    if kube.context == super::OWN_CONTEXT {
        if let Err(e) = renew_token_if_due(kube) {
            eprintln!("wicked-core: the engine's cluster token was not renewed: {e}");
        }
    }
    if let Ok(mut last) = LAST.lock() {
        *last = Some((kube.clone(), std::time::Instant::now()));
    }
    Ok(())
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
    // The pinned template must be the one rolled out: observed, every replica updated and
    // available (an old ReplicaSet's available pods are not the pinned release, review r1 P2).
    let n = |p: &str| dep.pointer(p).and_then(Value::as_u64).unwrap_or(0);
    let want = dep
        .pointer("/spec/replicas")
        .and_then(Value::as_u64)
        .unwrap_or(1);
    if n("/status/observedGeneration") < n("/metadata/generation") {
        return Err("the agent-sandbox controller's rollout is not observed yet".into());
    }
    // ... and no old replica left (kubectl's own rollout-complete test, review r2 P2).
    if n("/status/updatedReplicas") < want
        || n("/status/availableReplicas") < want
        || n("/status/replicas") > n("/status/updatedReplicas")
    {
        return Err("the agent-sandbox controller's rollout is not complete".into());
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

/// The ClusterRole naming the streaming verbs (exec, logs): never bound cluster-wide, only by a
/// RoleBinding inside a namespace this daemon owns ([`exec_role_binding`]).
pub(crate) fn exec_role_name(owner: &str) -> String {
    format!("wicked-engine-exec-{owner}")
}

/// The RoleBinding that lets the engine exec into and read logs of pods in `ns` (one of its
/// own namespaces): exec reaches only pods of namespaces this daemon created (review r1 P1).
pub(crate) fn exec_role_binding(ns: &str, owner: &str) -> Value {
    json!({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "RoleBinding",
           "metadata": {"name": "wicked-engine-exec", "namespace": ns, "labels": render::labels(owner, &[])},
           "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole",
                       "name": exec_role_name(owner)},
           "subjects": [{"kind": "ServiceAccount", "name": ENGINE_SA,
                         "namespace": daemon_namespace_name(owner)}]})
}

/// The admission rule for one daemon's engine account, as CEL over the request (pure, so the
/// rule's shape is pinned): every create, update and delete it makes must
/// - be of an object carrying its owner label (the new object on create and update, the old one
///   on update and delete: a relabel cannot take a foreign object over), and
/// - for a namespaced object, sit in a namespace carrying its owner label (no workload in
///   another daemon's namespace or `kube-system`).
pub(crate) fn admission_validations(owner: &str) -> Vec<Value> {
    let owned = |v: &str| {
        format!(
            "({v} != null && has({v}.metadata.labels) && '{OWNER_LABEL}' in {v}.metadata.labels \
             && {v}.metadata.labels['{OWNER_LABEL}'] == '{owner}')"
        )
    };
    vec![
        json!({"expression": format!(
                   "request.operation == 'DELETE' || {}", owned("object")),
               "message": format!("the wicked engine may only write objects labelled {OWNER_LABEL}={owner}")}),
        json!({"expression": format!(
                   "request.operation == 'CREATE' || {}", owned("oldObject")),
               "message": format!("the wicked engine may only change or delete objects labelled {OWNER_LABEL}={owner}")}),
        json!({"expression": format!(
                   "request.kind.kind == 'Namespace' || !has(request.namespace) || request.namespace == '' || {}",
                   owned("namespaceObject")),
               "message": format!("the wicked engine may only act inside namespaces labelled {OWNER_LABEL}={owner}")}),
    ]
}

/// The objects `cluster install` applies for one daemon (with the admin context): its
/// namespace, the engine's ServiceAccount, its roles, and an admission policy over every create,
/// update and delete the account makes ([`admission_validations`]), applied BEFORE the binding
/// that grants the role (D5, review r1). No long-lived token Secret: the engine's credential is a
/// TokenRequest token kept only in its kubeconfig ([`install`]). No PersistentVolume verb: a PV
/// can name any node path (review r1 P1); caches get their design in B-3.
pub(crate) fn install_docs(owner: &str) -> Vec<Value> {
    let ns = daemon_namespace_name(owner);
    let labels = render::labels(owner, &[]);
    let crud = ["get", "list", "watch", "create", "patch", "delete"];
    let rules = json!([
        {"apiGroups": [""], "resources": ["namespaces"], "verbs": crud},
        {"apiGroups": [""],
         "resources": ["pods", "persistentvolumeclaims", "services", "resourcequotas",
                       "limitranges", "configmaps"],
         "verbs": crud},
        {"apiGroups": [""], "resources": ["nodes"], "verbs": ["get", "list"]},
        {"apiGroups": ["networking.k8s.io"], "resources": ["networkpolicies"], "verbs": crud},
        {"apiGroups": ["apps"], "resources": ["deployments"], "verbs": crud},
        {"apiGroups": ["batch"], "resources": ["jobs"], "verbs": crud},
        {"apiGroups": ["rbac.authorization.k8s.io"], "resources": ["rolebindings"], "verbs": crud},
        {"apiGroups": ["rbac.authorization.k8s.io"], "resources": ["clusterroles"],
         "verbs": ["bind"], "resourceNames": [exec_role_name(owner)]},
        {"apiGroups": ["agents.x-k8s.io"], "resources": ["sandboxes"], "verbs": crud},
        {"apiGroups": ["extensions.agents.x-k8s.io"],
         "resources": ["sandboxclaims", "sandboxtemplates", "sandboxwarmpools"], "verbs": crud},
        {"apiGroups": ["apiextensions.k8s.io"], "resources": ["customresourcedefinitions"],
         "verbs": ["get"]},
        {"apiGroups": ["apps"], "resources": ["deployments"], "verbs": ["get"]},
    ]);
    let user = engine_username(owner);
    let policy = format!("wicked-engine-owns-{owner}");
    vec![
        render::namespace(&ns, owner, &[(ROLE_LABEL, "daemon")]),
        json!({"apiVersion": "admissionregistration.k8s.io/v1", "kind": "ValidatingAdmissionPolicy",
        "metadata": {"name": policy, "labels": labels},
        "spec": {
            "failurePolicy": "Fail",
            "matchConstraints": {"resourceRules": [{
                "apiGroups": ["*"], "apiVersions": ["*"],
                "operations": ["CREATE", "UPDATE", "DELETE"], "resources": ["*"],
            }]},
            "matchConditions": [{"name": "the-engine-account",
                                 "expression": format!("request.userInfo.username == '{user}'")}],
            "validations": admission_validations(owner),
        }}),
        json!({"apiVersion": "admissionregistration.k8s.io/v1", "kind": "ValidatingAdmissionPolicyBinding",
               "metadata": {"name": policy, "labels": labels},
               "spec": {"policyName": policy, "validationActions": ["Deny"]}}),
        json!({"apiVersion": "v1", "kind": "ServiceAccount",
               "metadata": {"name": ENGINE_SA, "namespace": ns, "labels": labels},
               "automountServiceAccountToken": false}),
        json!({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "ClusterRole",
               "metadata": {"name": exec_role_name(owner), "labels": labels},
               "rules": [{"apiGroups": [""], "resources": ["pods/exec", "pods/log"],
                          "verbs": ["create", "get"]},
                         {"apiGroups": [""], "resources": ["serviceaccounts/token"],
                          "verbs": ["create"], "resourceNames": [ENGINE_SA]}]}),
        exec_role_binding(&ns, owner),
        json!({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "ClusterRole",
               "metadata": {"name": format!("wicked-engine-{owner}"), "labels": labels},
               "rules": rules}),
        json!({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "ClusterRoleBinding",
               "metadata": {"name": format!("wicked-engine-{owner}"), "labels": labels},
               "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole",
                           "name": format!("wicked-engine-{owner}")},
               "subjects": [{"kind": "ServiceAccount", "name": ENGINE_SA, "namespace": ns}]}),
    ]
}

/// The kubeconfig the engine runs on: the admin context's cluster (server and CA), the engine
/// account's token, and one context named [`OWN_CONTEXT`]. Pure.
pub(crate) fn engine_kubeconfig(cluster: Value, token: &str) -> Value {
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
        return Err(format!(
            "the agent-sandbox manifest did not apply: {}",
            r.err_text()
        ));
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
    let token = mint_token(admin, &ns)?;
    let view = admin.call(
        None,
        &[
            "config",
            "view",
            "--raw",
            "--minify",
            "--flatten",
            "-o",
            "json",
        ],
        None,
        super::CALL_TIMEOUT,
    );
    if !view.ok() {
        return Err(view.err_text());
    }
    let v: Value = serde_json::from_slice(&view.stdout).map_err(|e| e.to_string())?;
    // The admin context's whole cluster entry (flattened: a CA file becomes its data; the
    // server name, proxy and TLS settings are kept as they are), review r1 P2.
    let cluster = v
        .pointer("/clusters/0/cluster")
        .cloned()
        .ok_or("the admin context names no cluster")?;
    if cluster.get("server").and_then(Value::as_str).is_none() {
        return Err("the admin context's cluster has no server".into());
    }
    let cfg = engine_kubeconfig(cluster, &token);
    let path = super::own_kubeconfig(home);
    let body = serde_json::to_vec_pretty(&cfg).map_err(|e| e.to_string())?;
    // Verified from a private staging file; the discoverable path is written only once the
    // policy is seen enforcing (review r2 P2).
    let staging = path.with_file_name(format!(
        ".kubeconfig-verify-{}",
        uuid::Uuid::new_v4().simple()
    ));
    write_private(&staging, &body)?;
    // The credential is handed over only once the admission policy is seen ENFORCING on it
    // (policy propagation can lag the create, review r1 P2): a foreign write must be refused.
    let engine = Kube {
        kubectl: admin.kubectl.clone(),
        kubeconfig: Some(staging.clone()),
        context: OWN_CONTEXT.to_string(),
        owner: admin.owner.clone(),
    };
    let start = std::time::Instant::now();
    loop {
        match admission_enforced(&engine) {
            Ok(()) => {
                std::fs::rename(&staging, &path).map_err(|e| e.to_string())?;
                return Ok(path);
            }
            Err(e) if start.elapsed() > Duration::from_secs(60) => {
                let _ = std::fs::remove_file(&staging);
                return Err(format!(
                    "the engine account's admission policy did not enforce: {e}; no credential written"
                ));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(500)),
        }
    }
}

/// `Ok` when the engine account's write of an object WITHOUT its owner label is refused by its
/// admission policy (a server-side dry run: nothing is created).
pub(crate) fn admission_enforced(engine: &Kube) -> Result<(), String> {
    let ns = daemon_namespace_name(&engine.owner);
    let probe = json!({"apiVersion": "v1", "kind": "ConfigMap",
                       "metadata": {"name": "wicked-admission-probe", "namespace": ns}});
    let body = serde_json::to_vec(&probe).map_err(|e| e.to_string())?;
    let r = engine.call(
        None,
        &["create", "--dry-run=server", "-f", "-", "-o", "name"],
        Some(&body),
        super::CALL_TIMEOUT,
    );
    if r.ok() {
        return Err("an unlabelled object was admitted".into());
    }
    let err = r.err_text();
    if err.contains("may only write objects labelled") {
        Ok(())
    } else {
        Err(err)
    }
}

/// How long an engine token lives, and when it is renewed.
pub(crate) const TOKEN_LIFETIME: &str = "720h";
const TOKEN_RENEW_BELOW_SECS: u64 = 7 * 24 * 3600;

/// A TokenRequest token for the engine account (never stored in the cluster's datastore).
fn mint_token(kube: &Kube, ns: &str) -> Result<String, String> {
    let r = kube.call(
        Some(ns),
        &[
            "create",
            "token",
            ENGINE_SA,
            &format!("--duration={TOKEN_LIFETIME}"),
        ],
        None,
        super::CALL_TIMEOUT,
    );
    let t = String::from_utf8_lossy(&r.stdout).trim().to_string();
    if r.ok() && !t.is_empty() {
        Ok(t)
    } else {
        Err(format!("the engine token was not issued: {}", r.err_text()))
    }
}

/// The `exp` claim of a JWT (unverified: only to decide when to renew the engine's own token).
pub(crate) fn token_expiry(token: &str) -> Option<u64> {
    use base64::Engine as _;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    serde_json::from_slice::<Value>(&bytes)
        .ok()?
        .get("exp")?
        .as_u64()
}

/// Renew the engine's own token (a TokenRequest it may make for itself only) when less than a
/// week is left; the kubeconfig is rewritten in place. `Ok(false)` = nothing to do.
pub(crate) fn renew_token_if_due(engine: &Kube) -> Result<bool, String> {
    renew_token_below(engine, TOKEN_RENEW_BELOW_SECS)
}

/// [`renew_token_if_due`] with its threshold (a test renews at once with `u64::MAX`).
pub(crate) fn renew_token_below(engine: &Kube, below_secs: u64) -> Result<bool, String> {
    let Some(path) = engine.kubeconfig.clone() else {
        return Ok(false);
    };
    let raw = std::fs::read(&path).map_err(|e| e.to_string())?;
    let mut cfg: Value = serde_json::from_slice(&raw).map_err(|e| e.to_string())?;
    let Some(token) = cfg.pointer("/users/0/user/token").and_then(Value::as_str) else {
        return Ok(false);
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if token_expiry(token).is_some_and(|exp| exp > now.saturating_add(below_secs)) {
        return Ok(false);
    }
    let fresh = mint_token(engine, &daemon_namespace_name(&engine.owner))?;
    cfg["users"][0]["user"]["token"] = json!(fresh);
    write_private(
        &path,
        &serde_json::to_vec_pretty(&cfg).map_err(|e| e.to_string())?,
    )?;
    Ok(true)
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
            &[
                "delete",
                kind,
                "-l",
                &sel,
                "--ignore-not-found",
                "--wait=true",
                "-o",
                "name",
            ],
            None,
            Duration::from_secs(300),
        );
        if !r.ok() {
            return Err(format!("{kind}: {}", r.err_text()));
        }
        gone.extend(
            String::from_utf8_lossy(&r.stdout)
                .lines()
                .filter(|l| !l.trim().is_empty() && !l.starts_with("No resources"))
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
        render::login_egress(&g.namespace, &g.owner),
        render::pvc(&g.namespace, &g.owner, render::GATEWAY_CONFIG_PVC, "64Mi"),
        render::pvc(&g.namespace, &g.owner, render::GATEWAY_BROKER_PVC, "64Mi"),
        render::pvc(&g.namespace, &g.owner, render::GATEWAY_LOGINS_PVC, "64Mi"),
        render::gateway_deployment(g),
        render::gateway_service(g, ports),
    ]
}

/// Apply the daemon's gateway objects (idempotent). The gateway container waits for its first
/// config: publish it through [`writer_pod`], then [`gateway_ready`].
pub(crate) fn apply_gateway(kube: &Kube, g: &GatewaySpec, host: &[HostPort]) -> Result<(), String> {
    kube.apply(&daemon_docs(g, host))
}

/// The gateway pod whose writer sidecar is running (waited for, bounded): the pod every config
/// publish, credential write and probe is exec'd into.
pub(crate) fn writer_pod(kube: &Kube, ns: &str, limit: Duration) -> Result<String, String> {
    let start = std::time::Instant::now();
    loop {
        let pods = kube.list_owned(Some(ns), "pods", &[(ROLE_LABEL, "gateway")])?;
        if let Some(p) = writer_running(&pods) {
            return Ok(p);
        }
        if start.elapsed() > limit {
            return Err("the gateway's writer did not start".into());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Pure half of [`writer_pod`]: the first undeleted pod whose `writer` init container runs.
pub(crate) fn writer_running(pods: &[Value]) -> Option<String> {
    pods.iter()
        .find(|p| {
            p.pointer("/metadata/deletionTimestamp").is_none()
                && p.pointer("/status/initContainerStatuses")
                    .and_then(Value::as_array)
                    .is_some_and(|cs| {
                        cs.iter().any(|c| {
                            c.get("name").and_then(Value::as_str) == Some("writer")
                                && c.pointer("/state/running").is_some()
                        })
                    })
        })
        .and_then(|p| p.pointer("/metadata/name").and_then(Value::as_str))
        .map(str::to_string)
}

/// Wait until the gateway serves (its pod Running and every container ready) and answer how
/// seats reach it.
pub(crate) fn gateway_ready(
    kube: &Kube,
    ns: &str,
    limit: Duration,
) -> Result<DaemonGateway, String> {
    let svc = kube
        .get_json(Some(ns), "service", render::GATEWAY_NAME)?
        .ok_or("the gateway Service is missing")?;
    let service_ip = svc
        .pointer("/spec/clusterIP")
        .and_then(Value::as_str)
        .filter(|ip| !ip.is_empty() && *ip != "None")
        .ok_or("the gateway Service has no cluster IP")?
        .to_string();
    let start = std::time::Instant::now();
    loop {
        let pods = kube.list_owned(Some(ns), "pods", &[(ROLE_LABEL, "gateway")])?;
        if let Some(pod) = running_pod(&pods) {
            return Ok(DaemonGateway {
                namespace: ns.to_string(),
                service_ip,
                pod,
            });
        }
        if start.elapsed() > limit {
            return Err("the gateway did not become ready".into());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// The name of the first pod of `pods` that is Running with every container ready and is not
/// being deleted.
pub(crate) fn running_pod(pods: &[Value]) -> Option<String> {
    pods.iter()
        .find(|p| {
            p.pointer("/metadata/deletionTimestamp").is_none()
                && p.pointer("/status/phase").and_then(Value::as_str) == Some("Running")
                && p.pointer("/status/conditions")
                    .and_then(Value::as_array)
                    .is_some_and(|cs| {
                        cs.iter().any(|c| {
                            c.get("type").and_then(Value::as_str) == Some("Ready")
                                && c.get("status").and_then(Value::as_str) == Some("True")
                        })
                    })
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
    let mut docs = render::run_namespace_docs(
        &ns,
        &kube.owner,
        run_id,
        &daemon_namespace_name(&kube.owner),
        budget,
        replica_size,
    );
    docs.push(exec_role_binding(&ns, &kube.owner));
    kube.apply(&docs)?;
    Ok(ns)
}

/// Delete a run's namespace (and everything in it) if it is this daemon's.
pub(crate) fn delete_run(kube: &Kube, run_id: &str, wait: bool) -> Result<bool, String> {
    let ns = run_namespace_name(&kube.owner, run_id);
    delete_namespace_owned(kube, &ns, wait)
}

/// Delete namespace `ns` only if it is this daemon's, with the UID and resourceVersion read at
/// the ownership check as preconditions: a namespace replaced or relabelled in between is not
/// deleted (review r1 P2).
pub(crate) fn delete_namespace_owned(kube: &Kube, ns: &str, wait: bool) -> Result<bool, String> {
    let Some(obj) = kube.get_json(None, "namespace", ns)? else {
        return Ok(false);
    };
    if !super::is_owned_by(&obj, &kube.owner) {
        return Err(format!("namespace/{ns} is not this daemon's; not deleted"));
    }
    let body = serde_json::to_vec(&delete_preconditions(&obj)?).map_err(|e| e.to_string())?;
    let url = format!("/api/v1/namespaces/{ns}");
    let r = kube.call(
        None,
        &["delete", "--raw", &url, "-f", "-"],
        Some(&body),
        super::CALL_TIMEOUT,
    );
    if !r.ok() {
        let e = r.err_text();
        if e.contains("NotFound") || e.contains("not found") {
            return Ok(false);
        }
        return Err(e);
    }
    if wait {
        let start = std::time::Instant::now();
        while kube.get_json(None, "namespace", ns)?.is_some() {
            if start.elapsed() > Duration::from_secs(180) {
                return Err(format!("namespace/{ns} was not gone in time"));
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }
    Ok(true)
}

/// The DeleteOptions that bind a delete to the exact object read (pure).
pub(crate) fn delete_preconditions(obj: &Value) -> Result<Value, String> {
    let uid = obj
        .pointer("/metadata/uid")
        .and_then(Value::as_str)
        .ok_or("the object has no uid")?;
    let rv = obj
        .pointer("/metadata/resourceVersion")
        .and_then(Value::as_str)
        .ok_or("the object has no resourceVersion")?;
    Ok(json!({"apiVersion": "v1", "kind": "DeleteOptions",
              "preconditions": {"uid": uid, "resourceVersion": rv}}))
}

/// What a sweep does with each of this daemon's run namespaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SweepAction {
    /// The run is over (or unknown to this engine): its namespace goes.
    DeleteNamespace(String),
    /// The run is live: only the pods an earlier engine process created go.
    ReapStalePods(String),
}

/// Whether namespace `n` was created before `at` (pure; RFC 3339 UTC strings compare in order).
/// No bound, or no readable creation time, counts as before.
pub(crate) fn before(n: &Value, at: Option<&str>) -> bool {
    match (
        at,
        n.pointer("/metadata/creationTimestamp")
            .and_then(Value::as_str),
    ) {
        (Some(at), Some(created)) => created < at,
        _ => true,
    }
}

/// Pure half of [`sweep`]: decide per namespace. `live` answers whether a run id is still
/// executing in this engine. A namespace without this daemon's labels is never in the list.
pub(crate) fn sweep_plan(
    namespaces: &[Value],
    owner: &str,
    live: &dyn Fn(&str) -> bool,
) -> Vec<SweepAction> {
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
            // The exact run id (the label may be sanitized); a namespace without it cannot be
            // matched to a run, so it is never deleted by a sweep (review r1 P1).
            let run = n
                .pointer("/metadata/annotations")
                .and_then(|a| a.get(render::RUN_ID_ANNOTATION))
                .and_then(Value::as_str)?;
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
/// `created_before` (RFC 3339): a namespace created at or after it (this engine's own, made
/// while the sweep ran) is left alone (B-5).
pub(crate) fn sweep(
    kube: &Kube,
    live: &dyn Fn(&str) -> bool,
    created_before: Option<&str>,
) -> Result<Vec<String>, String> {
    let namespaces: Vec<Value> = kube
        .list_owned(None, "namespaces", &[(ROLE_LABEL, "run")])?
        .into_iter()
        .filter(|n| before(n, created_before))
        .collect();
    let mut done = Vec::new();
    for action in sweep_plan(&namespaces, &kube.owner, live) {
        match action {
            SweepAction::DeleteNamespace(ns) => {
                if delete_namespace_owned(kube, &ns, false)? {
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
                // Standalone pods an earlier engine started (the sync pod) go too (review r2 P2).
                let r = kube.call(
                    Some(&ns),
                    &[
                        "delete",
                        "pods",
                        "-l",
                        &selector,
                        "--ignore-not-found",
                        "--wait=false",
                        "--grace-period=0",
                        "-o",
                        "name",
                    ],
                    None,
                    super::CALL_TIMEOUT,
                );
                if !r.ok() {
                    return Err(r.err_text());
                }
                // A termination barrier (review r1 P2): the sweep succeeds only once no pod an
                // earlier engine started is left running in the live run.
                let start = std::time::Instant::now();
                loop {
                    let pods = kube.list_owned(Some(&ns), "pods", &[])?;
                    let stale = pods.iter().any(|p| {
                        p.pointer("/metadata/labels")
                            .and_then(|l| l.get(super::BOOT_LABEL))
                            .and_then(Value::as_str)
                            .is_some_and(|b| b != super::boot_id())
                    });
                    if !stale {
                        break;
                    }
                    if start.elapsed() > Duration::from_secs(90) {
                        return Err(format!("an earlier engine's pods in {ns} did not stop"));
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        }
    }
    Ok(done)
}

/// How long a pids-limit verdict is trusted before the probe runs again.
const PIDS_PROBE_EVERY: Duration = Duration::from_secs(60 * 60);

/// The last verdict: (cluster key, when, the children count). Held for the whole probe, so
/// concurrent arms never race two probes (review r1 P2).
static PIDS_VERDICT: std::sync::Mutex<Option<(String, std::time::Instant, u32)>> =
    std::sync::Mutex::new(None);

/// The one node this runner certifies: v1 runs on a single-node local cluster, and a verdict
/// for one node never approves another (review r1 P2).
pub(crate) fn single_node(kube: &Kube) -> Result<(String, String), (FloorUnarmed, String)> {
    let r = kube.call(
        None,
        &["get", "nodes", "-o", "json"],
        None,
        super::CALL_TIMEOUT,
    );
    if !r.ok() {
        return Err((FloorUnarmed::ClusterUnreachable, r.err_text()));
    }
    let v: Value = serde_json::from_slice(&r.stdout)
        .map_err(|e| (FloorUnarmed::ClusterUnreachable, e.to_string()))?;
    // (name, uid): the uid is the node's incarnation, so a reset cluster whose node keeps its
    // name is probed again (review r2 P2).
    let names: Vec<(String, String)> = v
        .get("items")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|n| {
                    Some((
                        n.pointer("/metadata/name")?.as_str()?.to_string(),
                        n.pointer("/metadata/uid")?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    match names.as_slice() {
        [one] => Ok(one.clone()),
        _ => Err((
            FloorUnarmed::PolicyRefused,
            format!(
                "the runner certifies a single-node local cluster; this one has {} nodes",
                names.len()
            ),
        )),
    }
}

/// Whether this cluster's pods are held to a pids limit and its NetworkPolicy is enforced
/// (PARITY B2, B5): measured by the probe pod ([`render::pids_probe_pod`]) on the one node, in
/// the daemon namespace (deny-all), cached for an hour per (kubeconfig, context, node). Only a
/// pass is cached. `Err` = refuse every seat.
pub(crate) fn ensure_pids_limited(
    kube: &Kube,
    floor_image: &str,
) -> Result<u32, (FloorUnarmed, String)> {
    let mut cache = PIDS_VERDICT.lock().map_err(|_| {
        (
            FloorUnarmed::PolicyRefused,
            "the pids probe lock is poisoned".to_string(),
        )
    })?;
    let (node, node_uid) = single_node(kube)?;
    let key = format!("{}|{node}|{node_uid}", kube.head(None).join(" "));
    if let Some((k, at, n)) = cache.as_ref() {
        if *k == key && at.elapsed() < PIDS_PROBE_EVERY {
            return Ok(*n);
        }
    }
    let ns = daemon_namespace_name(&kube.owner);
    // The probe proves enforcement against its own namespace's deny-all: applied here, so the
    // first arm on a fresh install (before the gateway applies the daemon's policies) does not
    // read a namespace with no policy yet as a cluster that enforces none (B-2 live).
    kube.apply(&[render::default_deny(&ns, &kube.owner)])
        .map_err(|e| (FloorUnarmed::PolicyRefused, e))?;
    let name = format!("pids-probe-{}", super::boot_id());
    let infra = |e: String| (FloorUnarmed::PolicyRefused, e);
    let _ = kube.delete_owned(Some(&ns), "pod", &name, true);
    kube.apply(&[render::pids_probe_pod(
        &ns,
        &kube.owner,
        &name,
        floor_image,
        &node,
    )])
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
    let log = String::from_utf8_lossy(&logs.stdout);
    let n = render::pids_probe_verdict(&log).map_err(|e| {
        let kind = if e.contains("NetworkPolicy") {
            FloorUnarmed::PolicyRefused
        } else {
            FloorUnarmed::PidsUnlimited
        };
        (kind, e)
    })?;
    *cache = Some((key, std::time::Instant::now(), n));
    Ok(n)
}
