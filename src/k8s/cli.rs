//! `wicked-core cluster <install|uninstall|status>` (k8s-swap B-5): the operator's handle on the
//! daemon's cluster. No store, no actor.
//!
//! - `install --admin-context <ctx>`: the pinned agent-sandbox controller, the daemon's scoped
//!   engine account and its kubeconfig (under the daemon's state home), then the cluster's own
//!   proof that it can hold seats: a pod pids limit and an enforced NetworkPolicy (the deny probe).
//!   A cluster that fails it is refused: the engine's objects and kubeconfig are removed again.
//! - `uninstall --admin-context <ctx> [--controller]`: every object of this daemon (and the
//!   controller with `--controller`).
//! - `status [--json]`: what crew's diagnostics show (the cluster, the controller, the gateway,
//!   run namespaces, pending pods), read with the engine's own account.
//!
//! The operator's current kubectl context is never used: the admin context is named.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::{cluster, daemon_namespace_name, render, Kube, ROLE_LABEL};

pub(crate) const USAGE: &str = "usage: wicked-core cluster install --admin-context <ctx> [--admin-kubeconfig <file>] [--home <dir>]\n\
       wicked-core cluster uninstall --admin-context <ctx> [--admin-kubeconfig <file>] [--home <dir>] [--controller]\n\
       wicked-core cluster status [--json] [--home <dir>]\n  \
--home is the daemon's state home (default: the parent of WICKED_SEAT_GATEWAY_DIR).";

/// Parsed arguments (pure).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Args {
    pub(crate) verb: String,
    pub(crate) admin_context: Option<String>,
    pub(crate) admin_kubeconfig: Option<String>,
    pub(crate) home: Option<PathBuf>,
    pub(crate) controller: bool,
    pub(crate) json: bool,
}

pub(crate) fn parse(args: &[String]) -> Result<Args, String> {
    let verb = args.first().ok_or(USAGE)?.clone();
    if !matches!(verb.as_str(), "install" | "uninstall" | "status") {
        return Err(USAGE.into());
    }
    let mut a = Args {
        verb,
        admin_context: None,
        admin_kubeconfig: None,
        home: None,
        controller: false,
        json: false,
    };
    let mut it = args[1..].iter();
    while let Some(arg) = it.next() {
        let mut value = || it.next().cloned().ok_or_else(|| USAGE.to_string());
        match arg.as_str() {
            "--admin-context" => a.admin_context = Some(value()?),
            "--admin-kubeconfig" => a.admin_kubeconfig = Some(value()?),
            "--home" => a.home = Some(PathBuf::from(value()?)),
            "--controller" if a.verb == "uninstall" => a.controller = true,
            "--json" if a.verb == "status" => a.json = true,
            _ => return Err(USAGE.into()),
        }
    }
    if a.verb != "status" && a.admin_context.as_deref().is_none_or(str::is_empty) {
        return Err(format!(
            "{USAGE}\n  --admin-context is required: the current context is never used"
        ));
    }
    if a.home.as_ref().is_some_and(|h| !h.is_absolute()) {
        return Err("--home must be an absolute path".into());
    }
    Ok(a)
}

fn home_of(a: &Args) -> Result<PathBuf, String> {
    if let Some(h) = &a.home {
        return Ok(h.clone());
    }
    std::env::var_os("WICKED_SEAT_GATEWAY_DIR")
        .map(PathBuf::from)
        .filter(|d| d.is_absolute())
        .and_then(|d| d.parent().map(Path::to_path_buf))
        .ok_or_else(|| "name the daemon's state home with --home".to_string())
}

fn admin(a: &Args, home: &Path) -> Result<Kube, String> {
    let user_home = std::env::var_os("HOME").map(PathBuf::from);
    super::resolve_settings(
        home,
        std::env::var(super::KUBECTL_ENV).ok().as_deref(),
        a.admin_kubeconfig.as_deref(),
        a.admin_context.as_deref(),
        user_home.as_deref(),
        crate::validator::find_on_path,
    )
    .map_err(|(_, e)| e)
}

/// What `status` reports (pure over what was read): `gateway` is `serving`, `starting` (pods but
/// none serving), `stopped` (the Deployment at no replicas: a quarantine) or `absent`.
pub(crate) fn status_doc(
    context: &str,
    controller: Result<(), String>,
    deployment: Option<&Value>,
    gateway_pods: &[Value],
    run_namespaces: &[Value],
    pods: &[Value],
) -> Value {
    let gateway = if cluster::running_pod(gateway_pods).is_some() {
        "serving"
    } else if deployment
        .and_then(|d| d.pointer("/spec/replicas"))
        .and_then(Value::as_u64)
        == Some(0)
    {
        "stopped"
    } else if deployment.is_some() {
        "starting"
    } else {
        "absent"
    };
    let pending = pods
        .iter()
        .filter(|p| p.pointer("/status/phase").and_then(Value::as_str) == Some("Pending"))
        .count();
    json!({
        "configured": true,
        "reachable": true,
        "context": context,
        "controller": match &controller {
            Ok(()) => json!({"ready": true}),
            Err(e) => json!({"ready": false, "detail": e}),
        },
        "gateway": gateway,
        "controllerVersion": super::AGENT_SANDBOX_VERSION,
        "runNamespaces": run_namespaces.len(),
        "pendingPods": pending,
    })
}

fn status(home: &Path) -> (Value, i32) {
    let kube = match super::kube_for(home) {
        Ok(k) => k,
        Err((why, e)) => {
            return (
                json!({"configured": false, "reachable": false, "kind": why.as_str(), "detail": e}),
                1,
            )
        }
    };
    if !kube.answers() {
        return (
            json!({"configured": true, "reachable": false, "kind": "cluster_unreachable",
                   "context": kube.context}),
            1,
        );
    }
    let ns = daemon_namespace_name(&kube.owner);
    let read = || -> Result<Value, String> {
        let dep = kube.get_json(Some(&ns), "deployment", render::GATEWAY_NAME)?;
        let gw_pods = kube.list_owned(Some(&ns), "pods", &[(ROLE_LABEL, "gateway")])?;
        let runs = kube.list_owned(None, "namespaces", &[(ROLE_LABEL, "run")])?;
        // Every owned namespace's pods (codex b5 #15): no namespace means the context's default
        // one, where none of this daemon's pods live.
        let mut pods = kube.list_owned(Some(&ns), "pods", &[])?;
        for r in &runs {
            if let Some(n) = r.pointer("/metadata/name").and_then(Value::as_str) {
                pods.extend(kube.list_owned(Some(n), "pods", &[])?);
            }
        }
        Ok(status_doc(
            &kube.context,
            cluster::controller_ready(&kube),
            dep.as_ref(),
            &gw_pods,
            &runs,
            &pods,
        ))
    };
    match read() {
        Ok(v) => (v, 0),
        Err(e) => (
            json!({"configured": true, "reachable": true, "context": kube.context,
                   "detail": e}),
            1,
        ),
    }
}

/// Install, then prove the cluster can hold seats (pids limit + enforced NetworkPolicy); a
/// cluster that cannot is refused and left as it was found (the controller stays: it is shared).
/// A daemon already installed there (its credential or namespace exists) is never torn down by
/// a failed reinstall (codex b5 #4): its runs, claims and gateway stay, and the failure says so.
fn install(a: &Args, home: &Path) -> Result<Value, String> {
    // The engine's credential dir is the state home's own: a link there is refused, never
    // followed (codex b5 #2).
    let dir = home.join("k8s");
    if std::fs::symlink_metadata(&dir).is_ok_and(|m| !m.file_type().is_dir()) {
        return Err(format!(
            "{} is not a directory (a link is refused); remove it and install again",
            dir.display()
        ));
    }
    let admin = admin(a, home)?;
    let own = super::own_kubeconfig(home);
    let existed = own.is_file()
        || admin
            .get_json(None, "namespace", &daemon_namespace_name(&admin.owner))
            .ok()
            .flatten()
            .is_some();
    // Undo a fresh install; report what could not be undone (codex b5 #6).
    let undo = |why: String| -> String {
        if existed {
            return format!("{why}; the existing installation was left as it was");
        }
        let mut left = Vec::new();
        if let Err(e) = cluster::uninstall(&admin, false) {
            left.push(format!("cluster objects ({e})"));
        }
        match std::fs::remove_file(&own) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                left.push(format!("{} ({e})", own.display()))
            }
            _ => {}
        }
        if left.is_empty() {
            format!("{why}; nothing of this daemon was left on the cluster")
        } else {
            format!("{why}; could not remove: {}", left.join(", "))
        }
    };
    let path = cluster::install(&admin, home).map_err(undo)?;
    // The proof runs on the credential just written, whatever the environment names (codex b5
    // #7): the cluster certified is the one installed into.
    let engine = super::resolve_settings(
        home,
        std::env::var(super::KUBECTL_ENV).ok().as_deref(),
        Some(&path.to_string_lossy()),
        Some(super::OWN_CONTEXT),
        std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .as_deref(),
        crate::validator::find_on_path,
    )
    .map_err(|(_, e)| undo(e))?;
    let rt = crate::container_floor::runtime()
        .map_err(|w| undo(format!("an image builder is required: {}", w.describe())))?;
    let floor = crate::container_floor::ensure_floor_image(&rt).map_err(undo)?;
    let pids = cluster::ensure_pids_limited(&engine, &floor).map_err(|(why, e)| {
        undo(format!(
            "{}: {e}; this cluster cannot hold governed seats",
            why.as_str()
        ))
    })?;
    Ok(json!({"installed": true, "kubeconfig": path, "podPidsLimit": pids}))
}

/// [`install`] with no admin choice (tests: refusals before any cluster call).
#[cfg(test)]
pub(crate) fn install_for_test(home: &Path) -> Result<Value, String> {
    let a = parse(&["install", "--admin-context", "none"].map(String::from))?;
    install(&a, home)
}

/// After a successful uninstall: this daemon's own credential goes with its cluster objects
/// (codex b5 #8), and its dir when empty; an admin config named on the command line is never
/// touched.
fn forget_credential(home: &Path) -> Result<(), String> {
    let own = super::own_kubeconfig(home);
    match std::fs::remove_file(&own) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(format!("{}: {e}", own.display()))
        }
        _ => {}
    }
    if let Some(d) = own.parent() {
        let _ = std::fs::remove_dir(d);
    }
    Ok(())
}

/// The CLI entry: exit 0 done, 1 refused or failed, 2 bad arguments.
pub fn cluster_main(args: &[String]) -> i32 {
    let a = match parse(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let home = match home_of(&a) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("wicked-core cluster: {e}");
            return 2;
        }
    };
    match a.verb.as_str() {
        "status" => {
            let (v, rc) = status(&home);
            if a.json {
                println!("{v}");
            } else {
                println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            }
            rc
        }
        "install" => match install(&a, &home) {
            Ok(v) => {
                println!("{v}");
                0
            }
            Err(e) => {
                eprintln!("wicked-core cluster install: {e}");
                1
            }
        },
        _ => match admin(&a, &home)
            .and_then(|k| cluster::uninstall(&k, a.controller))
            .and_then(|done| forget_credential(&home).map(|()| done))
        {
            Ok(done) => {
                println!("{}", json!({"removed": done}));
                0
            }
            Err(e) => {
                eprintln!("wicked-core cluster uninstall: {e}");
                1
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn k8s_cluster_cli_never_uses_the_current_context() {
        assert!(parse(&s(&["install"]))
            .unwrap_err()
            .contains("--admin-context is required"));
        assert!(parse(&s(&["uninstall", "--controller"])).is_err());
        let a = parse(&s(&[
            "install",
            "--admin-context",
            "rancher-desktop",
            "--home",
            "/s",
        ]))
        .unwrap();
        assert_eq!(a.admin_context.as_deref(), Some("rancher-desktop"));
        assert_eq!(a.home, Some(PathBuf::from("/s")));
        assert!(parse(&s(&["install", "--admin-context", "x", "--home", "rel"])).is_err());
        assert!(parse(&s(&["status", "--controller"])).is_err());
        assert!(parse(&s(&["status", "--json"])).unwrap().json);
        assert!(parse(&s(&["delete"])).is_err());
        assert!(
            parse(&s(&["uninstall", "--admin-context", "x", "--controller"]))
                .unwrap()
                .controller
        );
    }

    #[test]
    fn k8s_cluster_status_names_the_gateway_state_and_pending_pods() {
        let serving = json!({"metadata": {"name": "agw-1"}, "status": {"phase": "Running",
            "conditions": [{"type": "Ready", "status": "True"}],
            "containerStatuses": [{"name": "gateway", "ready": true}]}});
        let pending = json!({"metadata": {"name": "p"}, "status": {"phase": "Pending"}});
        let dep = json!({"spec": {"replicas": 1}});
        let v = status_doc(
            "wicked",
            Ok(()),
            Some(&dep),
            std::slice::from_ref(&serving),
            &[json!({}), json!({})],
            &[serving.clone(), pending.clone()],
        );
        assert_eq!(v["gateway"], "serving");
        assert_eq!(v["runNamespaces"], 2);
        assert_eq!(v["pendingPods"], 1);
        assert_eq!(v["controller"]["ready"], true);
        let stopped = json!({"spec": {"replicas": 0}});
        let v = status_doc(
            "wicked",
            Err("missing".into()),
            Some(&stopped),
            &[],
            &[],
            &[pending],
        );
        assert_eq!(v["gateway"], "stopped");
        assert_eq!(v["controller"]["detail"], "missing");
        assert_eq!(
            status_doc("wicked", Ok(()), None, &[], &[], &[])["gateway"],
            "absent"
        );
        assert_eq!(
            status_doc("wicked", Ok(()), Some(&dep), &[], &[], &[])["gateway"],
            "starting"
        );
    }

    /// B-5: the boot sweep never deletes a namespace created after this boot (this engine's own,
    /// made while the sweep ran), and the shared session namespace is always live.
    #[test]
    fn k8s_the_boot_sweep_spares_new_namespaces_and_the_session_namespace() {
        let ns = |t: &str| json!({"metadata": {"creationTimestamp": t}});
        let at = Some("2026-11-11T07:00:00Z");
        assert!(cluster::before(&ns("2026-11-11T06:59:59Z"), at));
        assert!(!cluster::before(&ns("2026-11-11T07:00:00Z"), at));
        assert!(!cluster::before(&ns("2026-11-11T07:00:01Z"), at));
        assert!(cluster::before(&json!({}), at));
        assert!(cluster::before(&ns("2026-11-11T07:00:01Z"), None));
        let owner = "wabc123def456";
        let run_ns = |name: &str, run: &str| {
            json!({"metadata": {"name": name,
                "labels": {super::super::OWNER_LABEL: owner, ROLE_LABEL: "run"},
                "annotations": {render::RUN_ID_ANNOTATION: run}}})
        };
        let live = std::collections::HashSet::from(["r-live".to_string()]);
        let is_live = |r: &str| r == super::super::SESSIONS_RUN || live.contains(r);
        let plan = cluster::sweep_plan(
            &[
                run_ns("a", "r-live"),
                run_ns("b", "r-done"),
                run_ns("c", super::super::SESSIONS_RUN),
            ],
            owner,
            &is_live,
        );
        assert_eq!(
            plan,
            vec![
                cluster::SweepAction::ReapStalePods("a".into()),
                cluster::SweepAction::DeleteNamespace("b".into()),
                cluster::SweepAction::ReapStalePods("c".into()),
            ]
        );
    }
}
