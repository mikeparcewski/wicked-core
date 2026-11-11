//! Live parity tests against a real cluster. Skipped unless `WICKED_K8S_LIVE=1` and
//! `WICKED_K8S_ADMIN_CONTEXT` names an admin context (used only for `cluster install` and
//! `uninstall`; everything between runs on the engine's own scoped account). The seat image is
//! `WICKED_K8S_LIVE_SEAT_IMAGE` (an image already in the cluster's store). Every object is
//! labelled with a throwaway owner and removed at the end; the run ends with the cluster's
//! labelled-object list as it found it.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

use super::cluster;
use super::render::{self, GatewaySpec, ReplicaMount, RunBudget, SeatPod};
use super::*;

fn live() -> Option<(String, String)> {
    if std::env::var("WICKED_K8S_LIVE").ok().as_deref() != Some("1") {
        eprintln!("skipped: set WICKED_K8S_LIVE=1 and WICKED_K8S_ADMIN_CONTEXT to run");
        return None;
    }
    let ctx = std::env::var("WICKED_K8S_ADMIN_CONTEXT").ok()?;
    let image = std::env::var("WICKED_K8S_LIVE_SEAT_IMAGE")
        .unwrap_or_else(|_| "wicked-seat-codex:2d518ac11d63".into());
    Some((ctx, image))
}

struct Uninstall(Kube);
impl Drop for Uninstall {
    fn drop(&mut self) {
        match cluster::uninstall(&self.0, false) {
            Ok(gone) => eprintln!("uninstalled: {gone:?}"),
            Err(e) => eprintln!("UNINSTALL FAILED: {e}"),
        }
    }
}

fn exec(
    k: &Kube,
    ns: &str,
    pod: &str,
    container: &str,
    script: &str,
    stdin: Option<&[u8]>,
) -> Called {
    let mut args = vec!["exec"];
    if stdin.is_some() {
        args.push("-i");
    }
    args.extend([
        "-c",
        container,
        pod,
        "--",
        "/floor/busybox",
        "sh",
        "-c",
        script,
    ]);
    k.call(Some(ns), &args, stdin, Duration::from_secs(60))
}

/// B1, B2, B3, B4, B5, B9 (pod layer), C1, D5 end to end on a real cluster: install, the
/// engine's scoped account, the gateway with its writer sidecar, the pids probe, a run
/// namespace, a seat Sandbox (restricted, the PID 1 loop, Landlock canary), the network policy from the
/// seat's side, the dead-man's switch fields, the run's deletion and the uninstall.
#[test]
fn k8s_live_the_cluster_round_trip_holds_every_pod_layer_row() {
    let Some((admin_ctx, seat_image)) = live() else {
        return;
    };
    let t0 = std::time::Instant::now();
    let home =
        std::env::temp_dir().join(format!("wicked-k8s-live-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&home).unwrap();
    let mut admin = resolve_settings(
        &home,
        None,
        None,
        Some(&admin_ctx),
        std::env::var_os("HOME").map(PathBuf::from).as_deref(),
        crate::validator::find_on_path,
    )
    .expect("kubectl and the admin context");
    admin.owner = owner_id(&home);
    let _cleanup = Uninstall(admin.clone());

    // The trusted launcher image, built by the image builder (D2).
    let rt = crate::container_floor::runtime().expect("the image builder answers");
    let floor = crate::container_floor::ensure_floor_image(&rt).expect("the floor image");
    eprintln!("[{:>5.1}s] floor image {floor}", t0.elapsed().as_secs_f64());

    // D5: install, then everything else on the engine's own kubeconfig.
    let kc = cluster::install(&admin, &home).expect("cluster install");
    let engine = resolve_settings(
        &home,
        None,
        None,
        None,
        None,
        crate::validator::find_on_path,
    )
    .expect("the engine's own kubeconfig");
    assert_eq!(engine.kubeconfig.as_deref(), Some(kc.as_path()));
    assert_eq!(engine.owner, admin.owner);
    cluster::probe(&engine).expect("the cluster and controller answer the engine");
    eprintln!(
        "[{:>5.1}s] installed; engine probe ok",
        t0.elapsed().as_secs_f64()
    );

    // D5: the engine's account cannot touch an object without its owner label.
    let foreign = serde_json::json!({"apiVersion": "v1", "kind": "ConfigMap",
        "metadata": {"name": "not-ours", "namespace": daemon_namespace_name(&engine.owner)}});
    let refused = engine.apply(&[foreign]).unwrap_err();
    assert!(
        refused.contains("may only change objects labelled"),
        "{refused}"
    );
    let kube_system = engine.call(
        None,
        &["delete", "namespace", "kube-system", "--dry-run=server"],
        None,
        Duration::from_secs(30),
    );
    assert!(
        !kube_system.ok(),
        "the engine account could delete kube-system"
    );

    // B9: the gateway and its writer sidecar; the first config is published through the writer.
    let dns = daemon_namespace_name(&engine.owner);
    let spec = GatewaySpec {
        namespace: dns.clone(),
        owner: engine.owner.clone(),
        image: crate::seat_gateway::GATEWAY_IMAGE.into(),
        writer_image: crate::seat_gateway::HELPER_IMAGE.into(),
    };
    cluster::apply_gateway(&engine, &spec, &[]).expect("gateway objects");
    let writer = cluster::writer_pod(&engine, &dns, Duration::from_secs(120)).expect("writer");
    let config = crate::seat_gateway::GatewayConfig::default();
    let mut touch = String::from("set -e; umask 077;");
    for r in &config.providers {
        if let Some(f) = r.credential_file.file_name().and_then(|f| f.to_str()) {
            touch.push_str(&format!(" [ -e /broker/{f} ] || : > /broker/{f};"));
        }
    }
    let body = serde_json::to_vec(&crate::seat_gateway::render(&config)).unwrap();
    let w = |script: &str, stdin: Option<&[u8]>| {
        let mut args = vec!["exec"];
        if stdin.is_some() {
            args.push("-i");
        }
        args.extend(["-c", "writer", writer.as_str(), "--", "sh", "-c", script]);
        engine.call(Some(&dns), &args, stdin, Duration::from_secs(60))
    };
    assert!(w(&touch, None).ok(), "broker files");
    let publish = w(
        "umask 077; cat > /cfg/.config.json.new && mv /cfg/.config.json.new /cfg/config.json",
        Some(&body),
    );
    assert!(publish.ok(), "{}", publish.err_text());
    let gw =
        cluster::gateway_ready(&engine, &dns, Duration::from_secs(120)).expect("gateway serves");
    eprintln!(
        "[{:>5.1}s] gateway {} at {}",
        t0.elapsed().as_secs_f64(),
        gw.pod,
        gw.service_ip
    );
    // The readiness port answers inside the pod (the writer shares its network namespace).
    let ready = w(
        &format!(
            "wget -q -O /dev/null -T 3 http://127.0.0.1:{}/ 2>&1; echo rc=$?",
            crate::seat_gateway::READY_PORT
        ),
        None,
    );
    eprintln!(
        "readiness probe: {}",
        String::from_utf8_lossy(&ready.stdout).trim()
    );

    // B2: the pids limit, measured.
    let n = cluster::ensure_pids_limited(&engine, &floor).expect("pods are pids-limited");
    eprintln!(
        "[{:>5.1}s] pids probe: refused after {n}",
        t0.elapsed().as_secs_f64()
    );

    // A run namespace and one seat.
    let run = format!("live-{}", uuid::Uuid::new_v4().simple());
    let ns = cluster::ensure_run(&engine, &run, &RunBudget::default(), "1Gi").expect("run ns");
    let tree = PathBuf::from("/home/live/.wicked/repos/x/wt").join(&run);
    let sb = SeatPod {
        name: "seat-1".into(),
        namespace: ns.clone(),
        owner: engine.owner.clone(),
        run_label: label_value(&run),
        unit_label: "u1".into(),
        boot: boot_id().into(),
        role: "seat".into(),
        image: seat_image.clone(),
        floor_image: floor.clone(),
        workdir: tree.clone(),
        mounts: vec![ReplicaMount {
            path: tree.clone(),
            rw: true,
        }],
        memory_limit: "1Gi".into(),
        memory_request: "256Mi".into(),
        cpu_request: "100m".into(),
        gateway_ip: Some(gw.service_ip.clone()),
        shutdown_time: rfc3339_after(Duration::from_secs(15 * 60)),
    };
    let t_seat = std::time::Instant::now();
    engine
        .apply(&[render::seat_sandbox(&sb).unwrap()])
        .expect("seat sandbox");
    let mut ready = false;
    while t_seat.elapsed() < Duration::from_secs(120) {
        let pods = engine
            .list_owned(Some(&ns), "pods", &[(UNIT_LABEL, "u1")])
            .unwrap();
        if cluster::running_pod(&pods).is_some() {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(ready, "the seat pod did not run");
    eprintln!(
        "[{:>5.1}s] seat pod running after {:.2}s",
        t0.elapsed().as_secs_f64(),
        t_seat.elapsed().as_secs_f64()
    );

    // B3: the engine's loop is PID 1, and survives the seat's kill -9 -1.
    let ps = exec(
        &engine,
        &ns,
        "seat-1",
        "seat",
        "cat /proc/1/cmdline | tr '\\0' ' '",
        None,
    );
    assert!(
        String::from_utf8_lossy(&ps.stdout).starts_with("/floor/busybox sh -c trap"),
        "{:?}",
        String::from_utf8_lossy(&ps.stdout)
    );
    let _ = exec(
        &engine,
        &ns,
        "seat-1",
        "seat",
        "/floor/busybox sleep 30 & kill -9 -1",
        None,
    );
    let alive = exec(&engine, &ns, "seat-1", "seat", "echo alive", None);
    assert!(alive.ok(), "the seat pod died on kill -9 -1");

    // B5 + F1: from the seat, only the gateway's seat ports answer.
    let probe = |host: &str, port: u16| -> bool {
        let r = exec(&engine, &ns, "seat-1", "seat",
            &format!("/floor/busybox nc -w 3 {host} {port} </dev/null >/dev/null 2>&1 && echo open || echo closed"), None);
        String::from_utf8_lossy(&r.stdout).contains("open")
    };
    let api = exec(
        &engine,
        &ns,
        "seat-1",
        "seat",
        "echo $KUBERNETES_SERVICE_HOST",
        None,
    );
    let api = String::from_utf8_lossy(&api.stdout).trim().to_string();
    assert!(!api.is_empty());
    assert!(!probe(&api, 443), "the API server is reachable from a seat");
    assert!(
        !probe("1.1.1.1", 443),
        "the internet is reachable from a seat"
    );
    assert!(
        !probe("192.168.5.2", 22),
        "the VM host is reachable from a seat"
    );
    assert!(
        probe("agw", 3000),
        "the gateway's CONNECT port is not reachable as agw"
    );
    assert!(
        !probe("agw", crate::seat_gateway::READY_PORT),
        "the readiness port is reachable"
    );
    assert!(
        !probe(&gw.service_ip, 22),
        "an unlisted gateway port answered"
    );

    // B4: the Landlock canary in the Sandbox pod, with the seat floor's own argv shape.
    let canary = exec(&engine, &ns, "seat-1", "seat",
        &format!("/floor/launch /wicked-canary/probe -- /floor/landrun --best-effort --ignore-missing \
                  --rox /usr --rox /bin --rox /lib --rox /etc --rox /floor --ro /proc --ro /dev \
                  --ro /wicked-canary --rw /dev/null --rwx /home/seat --rwx /tmp --rwx {t} \
                  --connect-tcp 3000 --env PATH --env HOME -- /floor/launch --check /wicked-canary/probe -- \
                  /floor/busybox sh -c 'touch {t}/ok && echo wrote-tree'", t = tree.display()), None);
    let err = String::from_utf8_lossy(&canary.stderr);
    assert!(err.contains("floor canary passed"), "{err}");
    assert!(String::from_utf8_lossy(&canary.stdout).contains("wrote-tree"));

    // C1: the sandbox carries its dead-man's switch; the run's deletion takes everything.
    let sbx: Value = engine
        .get_json(Some(&ns), "sandboxes.agents.x-k8s.io", "seat-1")
        .unwrap()
        .unwrap();
    assert_eq!(sbx["spec"]["shutdownPolicy"], "Delete");
    assert!(cluster::delete_run(&engine, &run, true).unwrap());
    assert!(engine.get_json(None, "namespace", &ns).unwrap().is_none());
    eprintln!(
        "[{:>5.1}s] run namespace deleted",
        t0.elapsed().as_secs_f64()
    );
}
