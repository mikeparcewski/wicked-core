//! Live parity tests against a real cluster. Skipped unless `WICKED_K8S_LIVE=1` and
//! `WICKED_K8S_ADMIN_CONTEXT` names an admin context (used only for `cluster install` and
//! `uninstall`; everything between runs on the engine's own scoped account). The seat image is
//! `WICKED_K8S_LIVE_SEAT_IMAGE` (an image already in the cluster's store). Every object is
//! labelled with a throwaway owner and removed at the end; the run ends with the cluster's
//! labelled-object list as it found it.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;
use wicked_apps_core::HardenedCommand as _;

use super::cluster;
use super::render::{self, GatewaySpec, ReplicaMount, RunBudget, SeatPod};
use super::*;

fn live() -> Option<(String, String)> {
    if std::env::var("WICKED_K8S_LIVE").ok().as_deref() != Some("1") {
        eprintln!("skipped: set WICKED_K8S_LIVE=1 and WICKED_K8S_ADMIN_CONTEXT to run");
        return None;
    }
    // Review r1 P2: once live testing is asked for, a missing context fails, never skips.
    let ctx = std::env::var("WICKED_K8S_ADMIN_CONTEXT")
        .expect("WICKED_K8S_LIVE=1 needs WICKED_K8S_ADMIN_CONTEXT");
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
    // Review r2: the engine renews its own token as itself (TokenRequest on its own account).
    assert!(cluster::renew_token_below(&engine, u64::MAX).expect("self-renewal"));
    cluster::probe(&engine).expect("the renewed token works");
    eprintln!(
        "[{:>5.1}s] installed; engine probe ok",
        t0.elapsed().as_secs_f64()
    );

    // D5: the engine's account cannot touch an object without its owner label.
    let foreign = serde_json::json!({"apiVersion": "v1", "kind": "ConfigMap",
        "metadata": {"name": "not-ours", "namespace": daemon_namespace_name(&engine.owner)}});
    let refused = engine.apply(&[foreign]).unwrap_err();
    assert!(
        refused.contains("may only write objects labelled"),
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
        boot: "b1-live".into(),
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
    assert!(w("printf b1-live > /cfg/boot", None).ok(), "the boot token");
    let gw =
        cluster::gateway_ready(&engine, &dns, Duration::from_secs(120)).expect("gateway serves");
    eprintln!(
        "[{:>5.1}s] gateway {} at {}",
        t0.elapsed().as_secs_f64(),
        gw.pod,
        gw.service_ip
    );
    // The readiness port answers inside the pod (the writer shares its network namespace), and
    // refuses a call with no unit key: the gateway serves its policy (review r1 P2).
    let ready = w(
        &format!(
            "wget -S -q -O /dev/null -T 3 http://127.0.0.1:{}/ 2>&1; true",
            crate::seat_gateway::READY_PORT
        ),
        None,
    );
    let ready = String::from_utf8_lossy(&ready.stdout).to_string();
    assert!(
        ready.contains(" 401 "),
        "the readiness listener did not refuse a keyless call: {ready}"
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
        deadline_secs: 15 * 60,
        scope: None,
        view: None,
        deps: None,
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

    // B5 + F1: from the seat, only the gateway's seat ports answer. Every target below is a
    // LISTENING port (review r1 P2: a closed port proves nothing), and an exec that fails is an
    // error, never "closed".
    let probe = |host: &str, port: u16| -> bool {
        let r = exec(&engine, &ns, "seat-1", "seat",
            &format!("if /floor/busybox nc -w 3 {host} {port} </dev/null >/dev/null 2>&1; then echo open; else echo closed; fi"), None);
        let out = String::from_utf8_lossy(&r.stdout).trim().to_string();
        assert!(
            r.ok() && (out == "open" || out == "closed"),
            "probe {host}:{port} did not run: {}",
            r.err_text()
        );
        out == "open"
    };
    let env = |name: &str| {
        let r = exec(
            &engine,
            &ns,
            "seat-1",
            "seat",
            &format!("echo ${name}"),
            None,
        );
        String::from_utf8_lossy(&r.stdout).trim().to_string()
    };
    let api = env("KUBERNETES_SERVICE_HOST");
    assert!(!api.is_empty());
    let pod_json = engine
        .get_json(Some(&ns), "pod", "seat-1")
        .unwrap()
        .unwrap();
    let node_ip = pod_json["status"]["hostIP"].as_str().unwrap().to_string();
    let gw_pod = engine
        .get_json(Some(&dns), "pod", &gw.pod)
        .unwrap()
        .unwrap();
    let gw_ip = gw_pod["status"]["podIP"].as_str().unwrap().to_string();
    // The same targets answer from the gateway's own pod (so they listen) ...
    let listens = |host: &str, port: u16| {
        let r = w(&format!("if nc -w 3 {host} {port} </dev/null >/dev/null 2>&1; then echo open; else echo closed; fi"), None);
        String::from_utf8_lossy(&r.stdout).trim() == "open"
    };
    assert!(
        listens(&gw_ip, crate::seat_gateway::READY_PORT),
        "the readiness port does not listen"
    );
    eprintln!(
        "from the gateway pod: node {node_ip}:10250 open={}",
        listens(&node_ip, 10250)
    );
    // ... and are refused from the seat.
    assert!(!probe(&api, 443), "the API server is reachable from a seat");
    assert!(
        !probe(&node_ip, 10250),
        "the node's kubelet is reachable from a seat"
    );
    assert!(
        !probe(&node_ip, 6443),
        "the node's API port is reachable from a seat"
    );
    assert!(
        !probe("1.1.1.1", 443),
        "the internet is reachable from a seat"
    );
    assert!(
        !probe(&gw_ip, crate::seat_gateway::READY_PORT),
        "the gateway's readiness port is reachable from a seat"
    );
    assert!(
        probe("agw", 3000),
        "the gateway's CONNECT port is not reachable as agw"
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

/// Run a finalized seat command to its end, as `run_bounded` does (stdin closed).
fn run_seat(cmd: std::process::Command) -> (i32, String, String) {
    let mut cmd = cmd;
    cmd.stdin(std::process::Stdio::null());
    let out = cmd.output().expect("the kubectl client ran");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The seat argv a unit runs: the pod wrapper, the launcher under Landlock over `mounts`, then
/// `script` in the seat image's own shell.
fn seat_cmd(
    wrapper: &[String],
    cwd: &std::path::Path,
    script: &str,
    env: &[(&str, &str)],
) -> std::process::Command {
    let mut c = std::process::Command::new(&wrapper[0]);
    c.hardened()
        .args(&wrapper[1..])
        .args(["/bin/sh", "-c", script])
        .current_dir(cwd);
    for (k, v) in env {
        c.env(k, v);
    }
    c
}

/// B-2 end to end on a real cluster (PARITY A2, A3, A12, B5, B8, B9, B11, B15, B17): the gateway on
/// its writer sidecar (publish, a unit key served then refused); a creator seat pod armed from a
/// real git worktree, its unit env over stdin (never on the argv), its change, deletion and
/// commit written back byte-identical with the host HEAD fast-forwarded; a planted `.git` hook
/// never coming back; a read-only unit's writes succeeding in the pod and vanishing; and a lost
/// exec stream read as no result, never an exit code.
#[test]
fn k8s_live_a_seat_pod_round_trip_writes_back_only_what_it_may() {
    let Some((admin_ctx, seat_image)) = live() else {
        return;
    };
    let t0 = std::time::Instant::now();
    let say = |m: &str| eprintln!("[{:>5.1}s] {m}", t0.elapsed().as_secs_f64());
    let home = std::env::temp_dir().join(format!(
        "wicked-k8s-live2-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&home).unwrap();
    let home = home.canonicalize().unwrap();
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
    let rt = crate::container_floor::runtime().expect("the image builder answers");
    let floor = crate::container_floor::ensure_floor_image(&rt).expect("the floor image");
    let sync_image = crate::checks_container::image_tag();
    if !crate::container_floor::image_present(&rt, &sync_image) {
        crate::container_floor::build_image(
            &rt,
            crate::checks_container::DOCKERFILE,
            &sync_image,
            &[],
        )
        .expect("the sync image");
    }
    cluster::install(&admin, &home).expect("cluster install");
    let engine = resolve_settings(
        &home,
        None,
        None,
        None,
        None,
        crate::validator::find_on_path,
    )
    .expect("the engine's own kubeconfig");
    cluster::probe_cached(&engine).expect("probe");
    cluster::ensure_pids_limited(&engine, &floor).expect("pids");
    say("installed");

    // B8, B9: the gateway on its writer; a unit key is served before a seat starts, refused after.
    let gdir = home.join("gateway");
    std::fs::create_dir_all(&gdir).unwrap();
    let gw = super::gw::ensure(&engine, &gdir).expect("the gateway serves");
    say(&format!("gateway {} at {}", gw.pod, gw.service_ip));
    let key = crate::seat_gateway::UnitKey::new("live2", "live2#1", "codex");
    let t_key = std::time::Instant::now();
    super::gw::register_live(&gw, key.clone()).expect("the key is served");
    say(&format!(
        "key register -> served in {:.2}s",
        t_key.elapsed().as_secs_f64()
    ));
    super::gw::deregister_live(&gw, &key.key).expect("the key is refused");
    gw.write_file(super::gw::Volume::Broker, "live-probe", b"secret-bytes")
        .expect("a broker write");
    let back = gw
        .read_broker(&[("live-probe", false), ("missing-file", true)])
        .expect("a broker read");
    assert_eq!(
        back,
        vec![("live-probe".to_string(), b"secret-bytes".to_vec())]
    );

    // A real worktree: one commit, a file to change, one to delete, an ignored build dir.
    let tree = home.join("repo");
    std::fs::create_dir_all(&tree).unwrap();
    let git = |args: &[&str]| {
        let o = super::sync::git(&tree).args(args).output().unwrap();
        assert!(
            o.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(tree.join(".gitignore"), "target/\n").unwrap();
    std::fs::write(tree.join("a.txt"), "before\n").unwrap();
    std::fs::write(tree.join("gone.txt"), "x\n").unwrap();
    std::fs::write(tree.join(".env"), "HOST_SECRET=1\n").unwrap();
    git(&["add", "-A"]);
    git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-qm",
        "base",
    ]);
    let base = git(&["rev-parse", "HEAD"]);
    let landrun = |rw: bool| -> Vec<String> {
        let mut v: Vec<String> = vec![
            "/floor/launch".into(),
            "/wicked-canary/probe".into(),
            "--".into(),
        ];
        v.extend(crate::container_floor::landrun_prefix(
            &[crate::container_floor::Mount::Bind {
                path: tree.clone(),
                rw,
            }],
            crate::container_floor::GATEWAY_PORTS,
            &[],
        ));
        v
    };
    let mounts = vec![
        super::seat::PlanMount::Bind {
            path: tree.clone(),
            rw: true,
        },
        super::seat::PlanMount::Mask {
            path: tree.join(".env"),
        },
    ];
    let spec = |view: bool, run: &'static str, ord: u32| super::seat::ArmSpec {
        run_id: run,
        ord,
        cli: "codex",
        image: &seat_image,
        floor_image: &floor,
        sync_image: &sync_image,
        workdir: &tree,
        tree: &tree,
        view,
        mounts: &mounts,
        protected: &[],
        write_roots: if view {
            &[]
        } else {
            std::slice::from_ref(&tree)
        },
        seat_dirs: &[],
        scratch: None,
        seed_base: None,
        memory: "1Gi",
        gateway_ip: Some(&gw.service_ip),
        deadline: Duration::from_secs(600),
        role: "seat",
        deps: None,
    };

    // B11: a creator unit.
    let t_arm = std::time::Instant::now();
    let (seat, wrapper) = super::seat::arm(&engine, &spec(false, "live2-run", 1), landrun(true))
        .expect("the seat arms");
    say(&format!(
        "creator seat armed in {:.2}s (namespace, sync pod, copy-in, pod, deny check)",
        t_arm.elapsed().as_secs_f64()
    ));
    let script = "set -e; test ! -e .env; test \"$UNIT_SECRET\" = s3cr3t; \
                  echo after > a.txt; rm gone.txt; echo new > new.txt; mkdir -p target; echo o > target/out.o; \
                  printf '#!/bin/sh\\necho pwned\\n' > .git/hooks/post-commit; chmod +x .git/hooks/post-commit; \
                  git add -A; git commit -qm unit; echo > .env; echo done";
    // Like spawn hardening: an already resolved, seat-owned copy outside the mounted tree,
    // supplied explicitly on the carrier. The pod must receive its identity without rediscovering it.
    let global = home.join("gitconfig-seat-copy");
    let system = home.join("gitconfig-system");
    std::fs::write(
        &global,
        "[user]\n\tname = Op\n\temail = op@users.noreply.example\n",
    )
    .unwrap();
    std::fs::write(&system, "").unwrap();
    let mut cmd = seat_cmd(&wrapper, &tree, script, &[("UNIT_SECRET", "s3cr3t")]);
    cmd.env("GIT_CONFIG_GLOBAL", &global)
        .env("GIT_CONFIG_SYSTEM", &system)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env(
            "GIT_SSH_COMMAND",
            wicked_apps_core::spawn::NOPUSH_SSH_COMMAND,
        )
        .envs(wicked_apps_core::spawn::nopush_git_config_env());
    let (fin, seat2) = super::seat::finalize(&cmd)
        .expect("finalize")
        .expect("a pod seat");
    let argv: Vec<String> = fin
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert!(
        !argv.iter().any(|a| a.contains("s3cr3t")),
        "a secret rode the exec argv"
    );
    assert!(
        fin.get_envs().all(|(k, _)| k != "UNIT_SECRET"),
        "a secret is in kubectl's env"
    );
    assert_eq!(seat2.id, seat.id);
    let (code, out, err) = run_seat(fin);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(err.contains("floor canary passed"), "{err}");
    let code = super::seat::settle(&seat, super::seat::Ending::Exited(code)).expect("settled");
    assert_eq!(code, 0);
    say("creator unit settled");
    assert_eq!(
        std::fs::read_to_string(tree.join("a.txt")).unwrap(),
        "after\n"
    );
    assert_eq!(
        std::fs::read_to_string(tree.join("new.txt")).unwrap(),
        "new\n"
    );
    assert!(!tree.join("gone.txt").exists(), "a deleted file came back");
    assert!(
        !tree.join("target").exists(),
        "ignored build output came back"
    );
    assert_eq!(
        std::fs::read_to_string(tree.join(".env")).unwrap(),
        "HOST_SECRET=1\n",
        "a mask was written"
    );
    assert!(
        !tree.join(".git/hooks/post-commit").exists(),
        "a planted hook came back"
    );
    let head = git(&["rev-parse", "HEAD"]);
    assert_ne!(head, base);
    assert_eq!(
        git(&["log", "-1", "--format=%an <%ae>|%cn <%ce>"]),
        "Op <op@users.noreply.example>|Op <op@users.noreply.example>",
        "the landed commit must carry the operator's identity for author and committer"
    );
    assert_eq!(
        git(&["rev-parse", "HEAD^"]),
        base,
        "not a fast-forward of the base"
    );
    assert_eq!(
        git(&["status", "--porcelain"]),
        "",
        "the host index does not match the commit"
    );
    let pods = engine
        .list_owned(Some(&seat.ns), "pods", &[(UNIT_LABEL, &seat.unit_label)])
        .unwrap();
    assert!(pods.is_empty(), "the seat pod outlived its settle");

    // B17: a read-only unit writes freely in its view; nothing comes back.
    let before = git(&["rev-parse", "HEAD^{tree}"]);
    let (ro, wrapper) = super::seat::arm(&engine, &spec(true, "live2-run", 2), landrun(true))
        .expect("the view arms");
    // Phase C: an evaluator's git works in its view (the tree root is the seat's own, never
    // "dubious ownership"), and sees the change it made there.
    let cmd = seat_cmd(&wrapper, &tree, "set -e; touch x; echo z > a.txt; python3 -c 'open(\"p\",\"w\").write(\"p\")' 2>/dev/null || true; cat a.txt; \
                  echo LOG=$(git log --format=%H -1); git diff --stat; git status --porcelain", &[]);
    let (fin, _) = super::seat::finalize(&cmd).unwrap().unwrap();
    let (code, out, err) = run_seat(fin);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(out.contains('z'), "the view was not writable: {out}");
    assert!(
        out.contains(&format!("LOG={head}")) && out.contains("a.txt |"),
        "git log/diff in a read-only unit: {out}\n{err}"
    );
    assert!(!err.contains("dubious ownership"), "{err}");
    super::seat::settle(&ro, super::seat::Ending::Exited(code)).expect("settled");
    assert_eq!(
        std::fs::read_to_string(tree.join("a.txt")).unwrap(),
        "after\n"
    );
    assert!(!tree.join("x").exists() && !tree.join("p").exists());
    assert_eq!(git(&["rev-parse", "HEAD^{tree}"]), before);
    assert_eq!(git(&["status", "--porcelain"]), "");
    say("read-only unit settled; host unchanged");

    // A3: a stream lost before the CLI ends (the client killed) is no result.
    let (lost, wrapper) =
        super::seat::arm(&engine, &spec(false, "live2-run", 3), landrun(true)).expect("arms");
    let cmd = seat_cmd(&wrapper, &tree, "sleep 30", &[]);
    let (mut fin, _) = super::seat::finalize(&cmd).unwrap().unwrap();
    fin.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = fin.spawn().unwrap();
    std::thread::sleep(Duration::from_secs(3));
    let _ = child.kill();
    let code = child.wait().unwrap().code().unwrap_or(-1);
    let r = super::seat::settle(&lost, super::seat::Ending::Exited(code));
    assert!(
        r.as_ref().is_err_and(|e| e.starts_with("exec_stream_lost")),
        "{r:?}"
    );
    say("a lost stream read as no result");
    assert!(cluster::delete_run(&engine, "live2-run", true).unwrap());
}
