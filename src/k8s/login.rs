//! The sign-in transport on the cluster (k8s-swap B-4, PARITY C4): the one sign-in state machine
//! of [`crate::seat_login`] and [`crate::seat_refresh`] (journals, tombstones, sessions, repair
//! order) drives the daemon's gateway pod instead of Docker helpers and volumes.
//!
//! - **Broker** reads, writes and removals are execs into the gateway's `writer` sidecar
//!   ([`Gw`]): every write is whole or nothing (sha256-checked, renamed), behind the per-file
//!   fence and kernel lock.
//! - **Login stores** are directories of the `gateway-logins` claim (`/logins/<session>` in the
//!   writer). A container-login agent's sign-in runs in a **login pod** of the daemon namespace
//!   (its seat image, its store as HOME, internet egress, or none for agy's pasted key), started
//!   when crew asks for the argv; crew's terminal runs `kubectl exec -it` into it, and ingest,
//!   sign-out or the next sign-in delete it. A login nobody finishes ends at the pod's kubelet
//!   deadline even with no engine left.
//! - **Account reads** (copilot's API host and models) are `curl` in the gateway pod's `fetch`
//!   sidecar, on the gateway's network, with the token on exec stdin.
//!
//! Nothing secret rides an exec argv (it is a URL query on the API server): credentials and
//! tokens are on stdin or stay in the writer.

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::gw::{self, Gw, Volume};
use super::{cluster, render, Kube};
use crate::seat_login::BrokerIo;
use crate::worker_sandbox::FloorUnarmed;

/// How long a login pod may take to start.
const LOGIN_START: Duration = Duration::from_secs(120);
/// One writer exec of the login store.
const STORE_TIMEOUT: Duration = Duration::from_secs(60);
/// The largest account answer read back from the fetch sidecar (the parser refuses above 256 KiB).
const FETCH_CAP: u64 = 512 * 1024;

/// The cluster's sign-in transport for one daemon. `gw` is `None` only for a status read of a
/// daemon that has no gateway yet (then nothing was ever signed in on this cluster).
pub(crate) struct K8sIo {
    gw: Option<Gw>,
    /// The seat image the login pod runs, built by [`BrokerIo::prepare_login`].
    image: std::sync::Mutex<Option<String>>,
}

/// A refusal of the cluster, as the `seat-login` CLI reports it: the stable kind first (crew
/// matches on it), then the reason.
fn described((why, e): (FloorUnarmed, String)) -> String {
    format!("{}: {e}", why.as_str())
}

impl K8sIo {
    /// The transport of a running gateway (the daemon's seats).
    pub(crate) fn of(gw: &Gw) -> Self {
        Self {
            gw: Some(gw.clone()),
            image: std::sync::Mutex::new(None),
        }
    }

    /// The transport for gateway dir `dir` (the `seat-login` CLI): the daemon's cluster from its
    /// state home (the dir's parent) and its gateway, provisioned on a first sign-in when
    /// `provision` ([`gw::attach`]).
    pub(crate) fn for_dir(dir: &Path, provision: bool) -> Result<Self, String> {
        let home = dir
            .parent()
            .filter(|_| dir.is_absolute())
            .ok_or("the gateway directory must be absolute")?;
        let kube = super::kube_for(home).map_err(described)?;
        Self::attach(&kube, dir, provision)
    }

    /// [`K8sIo::for_dir`] on a known cluster (the daemon's refresher, once per pass).
    pub(crate) fn attach(kube: &Kube, dir: &Path, provision: bool) -> Result<Self, String> {
        let gw = gw::attach(kube, dir, provision).map_err(described)?;
        Ok(Self {
            gw,
            image: std::sync::Mutex::new(None),
        })
    }

    fn gw(&self) -> Result<&Gw, String> {
        self.gw
            .as_ref()
            .ok_or_else(|| "the daemon has no gateway on its cluster yet".into())
    }

    /// One writer exec of a login-store script; `Ok(code)` = its exit status.
    fn store(&self, script: &str) -> Result<super::Called, String> {
        Ok(self.gw()?.exec(script, None, STORE_TIMEOUT))
    }

    /// A store script that must succeed. Its diagnostics are never returned (review B-4 #7): they
    /// name paths inside a store a vendor CLI wrote, and a path can carry a credential.
    fn store_ok(&self, script: &str, what: &str) -> Result<(), String> {
        let r = self.store(script)?;
        if r.ok() {
            Ok(())
        } else {
            Err(format!("could not {what} (exit {:?})", r.code))
        }
    }
}

/// A login session's name as a store directory and pod name: a DNS label (the shape
/// `seat_login` gives it, `<gateway>-login-<cli>-<8 hex>`), so it is safe in a path, a script
/// and an object name.
pub(crate) fn session_name(session: &str) -> Result<(), String> {
    let ok = !session.is_empty()
        && session.len() <= 63
        && session
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && session.as_bytes()[0].is_ascii_alphanumeric()
        && session.as_bytes()[session.len() - 1].is_ascii_alphanumeric();
    if ok {
        Ok(())
    } else {
        Err("invalid login session name".into())
    }
}

/// A credential path relative to the login HOME (`claude/.credentials.json`): plain components
/// only, never empty, `.` or `..`.
pub(crate) fn store_path(path: &str) -> Result<(), String> {
    let ok = !path.is_empty()
        && path.split('/').all(|c| {
            !c.is_empty()
                && c != "."
                && c != ".."
                && c.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        });
    if ok {
        Ok(())
    } else {
        Err(format!("`{path}` is not a login store path"))
    }
}

/// The writer script that reads `paths` from session `session`'s store (pure): for each path
/// whose every component is not a link and which is a regular file, `path size` and its body; a
/// missing path prints nothing; a link on the way exits 46, a non-regular file 45, an oversized
/// one 66. The store is read only after its login pod is gone (no writer races it).
pub(crate) fn read_store_script(session: &str, paths: &[&str]) -> Result<String, String> {
    read_store_script_at(render::LOGINS_DIR, session, paths)
}

/// [`read_store_script`] under store root `root` (the tests run it against a local dir).
pub(crate) fn read_store_script_at(
    root: &str,
    session: &str,
    paths: &[&str],
) -> Result<String, String> {
    session_name(session)?;
    let base = format!("{root}/{session}");
    let mut s = format!("set -e; [ -d {base} ] && [ ! -L {base} ] || exit 46;");
    for p in paths {
        store_path(p)?;
        let mut prefix = base.clone();
        let mut gone = Vec::new();
        for c in p.split('/') {
            prefix.push('/');
            prefix.push_str(c);
            gone.push(prefix.clone());
        }
        let file = format!("{base}/{p}");
        // Each prefix: a link is refused; a missing one means the file is missing.
        let mut checks = String::new();
        for g in &gone {
            checks.push_str(&format!(
                " if [ -L {g} ]; then exit 46; fi; if [ ! -e {g} ]; then m=1; fi;"
            ));
        }
        s.push_str(&format!(
            " m=0;{checks} if [ $m = 0 ]; then [ -f {file} ] && [ -r {file} ] || exit 45; \
             n=$(wc -c < {file}); if [ $n -gt {max} ]; then exit 66; fi; \
             printf '%s %s\\n' {p} $n; cat {file}; fi;",
            max = crate::seat_login::MAX_FILE_BYTES
        ));
    }
    Ok(s)
}

/// The writer script that removes session `session`'s store under `root` (pure). A link is
/// removed, never followed; every directory is first made traversable by its owner (a vendor
/// CLI that `chmod 000`ed one cannot keep its credential there, review B-4 #6), without
/// following a link (`find` does not, and `-type d` is never a link).
pub(crate) fn remove_store_script(root: &str, session: &str) -> Result<String, String> {
    session_name(session)?;
    let p = format!("{root}/{session}");
    Ok(format!(
        "set -e; if [ -L {p} ]; then rm -f {p}; elif [ -e {p} ]; then \
         find {p} -type d -exec chmod u+rwx {{}} \\; ; rm -rf {p}; fi"
    ))
}

/// The writer script that makes `cli`'s configuration homes in session `session`'s store (each
/// 0700), seeding copilot's settings (pure).
pub(crate) fn init_store_script(cli: &str, session: &str) -> Result<String, String> {
    session_name(session)?;
    let base = format!("{}/{session}", render::LOGINS_DIR);
    let mut s = format!("set -e; umask 077; [ -d {base} ] && [ ! -L {base} ] || exit 46;");
    for (_, dir) in crate::seat_login::login_homes(cli) {
        store_path(dir)?;
        s.push_str(&format!(" mkdir -p {base}/{dir}; chmod 700 {base}/{dir};"));
    }
    if cli == "copilot" {
        // See `seat_login::init_login_argv`: no keychain here, so the token is stored plain.
        s.push_str(&format!(
            " printf '%s' '{}' > {base}/copilot/settings.json;",
            crate::seat_login::COPILOT_SETTINGS
        ));
    }
    s.push_str(&format!(" chmod 700 {base}"));
    Ok(s)
}

/// The argv crew's terminal runs for session `session` (pure): `kubectl exec -it` into its
/// login pod, running the agent's login command. No credential is on it.
pub(crate) fn login_exec_argv(kube: &Kube, ns: &str, cli: &str, session: &str) -> Vec<String> {
    // The terminal's own `KUBECONFIG` is dropped, as every engine call drops it (review B-4 #1):
    // with only a context pinned, it could name another cluster with a context of that name.
    let mut argv: Vec<String> = ["/usr/bin/env", "-u", "KUBECONFIG"]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    argv.extend(kube.head(Some(ns)));
    argv.extend(
        ["exec", "-it", session, "-c", "login", "--"]
            .iter()
            .map(|s| (*s).to_string()),
    );
    argv.extend(
        crate::seat_login::login_command(cli)
            .iter()
            .map(|s| (*s).to_string()),
    );
    argv
}

/// The login pod of `cli`'s session (pure).
pub(crate) fn login_pod_spec(gw: &Gw, cli: &str, session: &str, image: &str) -> render::LoginPod {
    render::LoginPod {
        name: session.to_string(),
        namespace: gw.ns.clone(),
        owner: gw.kube.owner.clone(),
        image: image.to_string(),
        // agy's API-key sign-in only reads a paste: no network at all.
        online: cli != "agy",
        homes: crate::seat_login::login_homes(cli)
            .iter()
            .map(|(var, dir)| ((*var).to_string(), (*dir).to_string()))
            .collect(),
    }
}

impl BrokerIo for K8sIo {
    fn broker_present(&self) -> Result<bool, String> {
        Ok(self.gw.is_some())
    }

    fn init_broker(&self, _dir: &Path) -> Result<(), String> {
        // `attach` provisioned the claims before any write.
        self.gw().map(|_| ())
    }

    fn read_broker(
        &self,
        paths: &[&str],
        presence: bool,
    ) -> Result<Vec<(String, Vec<u8>)>, String> {
        let files: Vec<(&str, bool)> = paths
            .iter()
            .map(|p| (*p, presence && !crate::seat_login::is_archive(p)))
            .collect();
        self.gw()?.read_broker(&files)
    }

    fn write_broker(&self, _dir: &Path, _cli: &str, file: &str, body: &[u8]) -> Result<(), String> {
        self.gw()?.write_file(Volume::Broker, file, body)
    }

    fn empty_broker(&self, _dir: &Path, _cli: &str, files: &[&str]) -> Result<(), String> {
        let gw = self.gw()?;
        for f in files {
            gw.empty_broker_file(f)?;
        }
        Ok(())
    }

    fn remove_broker(&self, _dir: &Path, _cli: &str, file: &str) -> Result<(), String> {
        self.gw()?.remove_file(Volume::Broker, file)
    }

    fn cleanup(&self, dir: &Path) -> Result<(), String> {
        // Writer writes need no settling (the per-file fence ends an orphan's write). What can be
        // left is the container floor's journal of a Docker helper, which writes only a Docker
        // volume this broker never reads: it is dropped, so it never reads as a mutation in
        // progress here.
        for scope in [
            "claude", "codex", "pi", "opencode", "copilot", "agy", "broker", "config",
        ] {
            crate::seat_refresh::remove_private_file(&crate::seat_gateway::unsettled_path(
                dir, scope,
            ))?;
        }
        Ok(())
    }

    fn route_sign_in(
        &self,
        dir: &Path,
        cli: &str,
        literals: &crate::seat_gateway::RouteLiterals,
    ) -> Result<(), String> {
        let gw = self.gw()?;
        crate::seat_gateway::route_sign_in_with(dir, cli, literals, || gw.publish())
    }

    fn confirm_sign_out(&self, dir: &Path, cli: &str) -> Result<(), String> {
        let gw = self.gw()?;
        crate::seat_gateway::confirm_sign_out_with(
            dir,
            cli,
            gw.serving_state(),
            gw.scaled_down(),
            || gw.publish(),
            |key, accepted| gw.confirm_key(key, accepted),
        )
    }

    fn login_exists(&self, session: &str) -> Result<bool, String> {
        session_name(session)?;
        let gw = self.gw()?;
        if gw.kube.get_json(Some(&gw.ns), "pod", session)?.is_some() {
            return Ok(true);
        }
        let p = format!("{}/{session}", render::LOGINS_DIR);
        let r = self.store(&format!(
            "if [ -e {p} ] || [ -L {p} ]; then exit 0; else exit 3; fi"
        ))?;
        match r.code {
            Some(0) => Ok(true),
            Some(3) => Ok(false),
            _ => Err(format!(
                "could not look for the login store: {}",
                r.err_text()
            )),
        }
    }

    fn create_login(&self, session: &str) -> Result<(), String> {
        session_name(session)?;
        self.store_ok(
            &format!("set -e; umask 077; mkdir {}/{session}", render::LOGINS_DIR),
            "create the login store",
        )
    }

    fn init_login(&self, _dir: &Path, cli: &str, session: &str) -> Result<(), String> {
        self.store_ok(&init_store_script(cli, session)?, "prepare the login store")
    }

    fn prepare_login(&self, cli: &str) -> Result<(), String> {
        let gw = self.gw()?;
        // The container CLI stays the image builder (PARITY D2); the cluster runs what it built.
        let rt = crate::container_floor::runtime()
            .map_err(|why| format!("an image builder is required: {}", why.describe()))?;
        let image = crate::container_floor::ensure_seat_image(&rt, cli)?;
        // No vendor CLI starts on a cluster that has not proved it holds pods (review B-4 #4):
        // the same pids-limit and NetworkPolicy-enforcement probe a seat arm runs.
        let floor = crate::container_floor::ensure_floor_image(&rt)?;
        super::cluster::ensure_pids_limited(&gw.kube, &floor).map_err(described)?;
        *self.image.lock().unwrap_or_else(|p| p.into_inner()) = Some(image);
        Ok(())
    }

    fn open_login(&self, cli: &str, session: &str) -> Result<Vec<String>, String> {
        session_name(session)?;
        let gw = self.gw()?;
        let image = self
            .image
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .ok_or("the login was not prepared")?;
        let doc = render::login_pod(&login_pod_spec(gw, cli, session, &image));
        if let Err(e) = gw.kube.apply(&[doc]) {
            let _ = self.stop_login(session);
            return Err(format!("the login pod was not admitted: {e}"));
        }
        let start = Instant::now();
        loop {
            match gw.kube.get_json(Some(&gw.ns), "pod", session) {
                Ok(Some(p)) => {
                    if cluster::running_pod(std::slice::from_ref(&p)).is_some() {
                        break;
                    }
                    if let Some(why) = super::floor::stuck(&p) {
                        let _ = self.stop_login(session);
                        return Err(format!("the login pod cannot start: {why}"));
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    let _ = self.stop_login(session);
                    return Err(format!("the login pod could not be read: {e}"));
                }
            }
            if start.elapsed() > LOGIN_START {
                let _ = self.stop_login(session);
                return Err(format!(
                    "the login pod did not start within {LOGIN_START:?}"
                ));
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        Ok(login_exec_argv(&gw.kube, &gw.ns, cli, session))
    }

    fn read_login(&self, session: &str, paths: &[&str]) -> Result<Vec<(String, Vec<u8>)>, String> {
        let r = self.store(&read_store_script(session, paths)?)?;
        match r.code {
            Some(0) => gw::parse_read(&r.stdout),
            Some(45) | Some(46) => Err("a login file is not a regular file".into()),
            Some(66) => Err("a login file exceeds its size limit".into()),
            // Never the output: it can hold a credential.
            _ => Err(format!(
                "could not read the login store (exit {:?})",
                r.code
            )),
        }
    }

    fn stop_login(&self, session: &str) -> Result<(), String> {
        session_name(session)?;
        let gw = self.gw()?;
        gw.kube
            .delete_owned(Some(&gw.ns), "pod", session, true)
            .map(|_| ())
            .map_err(|e| format!("could not stop the login pod: {e}"))
    }

    fn remove_login(&self, session: &str) -> Result<(), String> {
        self.stop_login(session)?;
        self.store_ok(
            &remove_store_script(render::LOGINS_DIR, session)?,
            "remove the login store",
        )
    }

    fn send(
        &self,
        _dir: &Path,
        _cli: &str,
        request: &crate::seat_refresh::Request,
    ) -> Result<Value, String> {
        let (args, stdin) = crate::seat_refresh::request_argv(request)?;
        // `-q` first: no `.curlrc` is read.
        let argv: Vec<String> = ["curl".to_string(), "-q".to_string()]
            .into_iter()
            .chain(args)
            .collect();
        let r = self.gw()?.fetch(
            &argv,
            &stdin,
            crate::seat_refresh::ACCOUNT_TIMEOUT,
            FETCH_CAP,
        );
        if !r.ok() {
            // Never the output: it can hold the answer.
            return Err(format!(
                "the account read did not complete (exit {:?})",
                r.code
            ));
        }
        crate::seat_refresh::answer(&r.stdout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn kube() -> Kube {
        Kube {
            kubectl: PathBuf::from("/opt/k/kubectl"),
            kubeconfig: Some(PathBuf::from("/state/k8s/kubeconfig")),
            context: "wicked".into(),
            owner: "wabc123def456".into(),
        }
    }

    fn gw() -> Gw {
        Gw {
            kube: kube(),
            ns: "wicked-sys-wabc123def456".into(),
            pod: "agw-1".into(),
            service_ip: String::new(),
            dir: PathBuf::from("/state/gateway"),
        }
    }

    const SESSION: &str = "wicked-agw-0123456789ab-login-claude-0a1b2c3d";

    fn spec() -> render::GatewaySpec {
        render::GatewaySpec {
            namespace: "wicked-sys-wabc123def456".into(),
            owner: "wabc123def456".into(),
            image: crate::seat_gateway::GATEWAY_IMAGE.into(),
            writer_image: "busybox:1.37".into(),
            boot: "boot-1".into(),
        }
    }

    /// B-4: a login pod is restricted like every engine pod, carries no token or service link,
    /// mounts only its own session's store (as HOME) and memory scratch, and ends at a kubelet
    /// deadline even if no engine is left. Its environment names homes, never a credential.
    #[test]
    fn k8s_a_login_pod_is_restricted_scoped_to_its_session_and_ends_by_itself() {
        let pod = render::login_pod(&login_pod_spec(&gw(), "claude", SESSION, "seat:claude"));
        let spec = &pod["spec"];
        assert_eq!(spec["automountServiceAccountToken"], json!(false));
        assert_eq!(spec["enableServiceLinks"], json!(false));
        assert_eq!(spec["hostNetwork"], json!(false));
        assert_eq!(spec["restartPolicy"], json!("Never"));
        assert_eq!(
            spec["activeDeadlineSeconds"],
            json!(render::LOGIN_DEADLINE_SECS)
        );
        assert_eq!(spec["securityContext"]["runAsNonRoot"], json!(true));
        let c = &spec["containers"][0];
        assert_eq!(c["securityContext"], render::hardened_context());
        assert_eq!(c["imagePullPolicy"], json!("Never"));
        assert_eq!(pod["metadata"]["name"], json!(SESSION));
        assert_eq!(
            pod["metadata"]["labels"][super::super::ROLE_LABEL],
            json!(render::LOGIN_ROLE)
        );
        assert_eq!(
            pod["metadata"]["labels"][super::super::OWNER_LABEL],
            json!("wabc123def456")
        );
        let mounts = c["volumeMounts"].as_array().unwrap();
        assert_eq!(mounts.len(), 2, "{mounts:?}");
        assert_eq!(
            mounts[0],
            json!({"name": "logins", "mountPath": render::LOGIN_HOME, "subPath": SESSION})
        );
        assert_eq!(mounts[1]["mountPath"], json!("/tmp"));
        let env = c["env"].to_string().to_ascii_lowercase();
        assert!(env.contains("claude_config_dir"), "{env}");
        assert!(!env.contains("token") && !env.contains("key"), "{env}");
        // agy reads a paste only: a role no egress policy selects.
        let agy = render::login_pod(&login_pod_spec(&gw(), "agy", SESSION, "seat:agy"));
        assert_eq!(
            agy["metadata"]["labels"][super::super::ROLE_LABEL],
            json!(render::LOGIN_OFFLINE_ROLE)
        );
    }

    /// B-4: the daemon objects carry the login store and the login egress; the egress is the
    /// internet (never a private range) for `login` pods only; only the writer mounts the
    /// stores, and the account reader mounts nothing.
    #[test]
    fn k8s_the_daemon_carries_login_stores_and_only_the_writer_mounts_them() {
        let docs = cluster::daemon_docs(&spec(), &[]);
        let names: Vec<String> = docs
            .iter()
            .map(|d| {
                format!(
                    "{}/{}",
                    d["kind"].as_str().unwrap(),
                    d["metadata"]["name"].as_str().unwrap()
                )
            })
            .collect();
        assert!(
            names.contains(&format!(
                "PersistentVolumeClaim/{}",
                render::GATEWAY_LOGINS_PVC
            )),
            "{names:?}"
        );
        let egress = docs
            .iter()
            .find(|d| d["metadata"]["name"] == json!("login-egress-internet"))
            .expect("login egress");
        assert_eq!(
            egress["spec"]["podSelector"],
            json!({"matchLabels": {super::super::ROLE_LABEL: render::LOGIN_ROLE}})
        );
        assert_eq!(
            egress["spec"]["egress"][0]["to"][0]["ipBlock"]["except"],
            json!(render::PRIVATE_RANGES)
        );
        // No policy in the daemon namespace selects an offline login.
        assert!(!serde_json::to_string(&docs)
            .unwrap()
            .contains(render::LOGIN_OFFLINE_ROLE));
        let dep = render::gateway_deployment(&spec());
        let init = dep["spec"]["template"]["spec"]["initContainers"]
            .as_array()
            .unwrap();
        let writer = init.iter().find(|c| c["name"] == json!("writer")).unwrap();
        assert!(writer["volumeMounts"]
            .as_array()
            .unwrap()
            .contains(&json!({"name": "logins", "mountPath": render::LOGINS_DIR})));
        let fetch = init.iter().find(|c| c["name"] == json!("fetch")).unwrap();
        assert!(fetch.get("volumeMounts").is_none(), "{fetch}");
        assert_eq!(fetch["securityContext"], render::hardened_context());
        assert_eq!(fetch["image"], json!(crate::seat_refresh::REFRESH_IMAGE));
        let gateway = &dep["spec"]["template"]["spec"]["containers"][0];
        assert!(!gateway["volumeMounts"].to_string().contains("logins"));
    }

    /// B-4: a store read checks every component for a link, refuses non-regular and oversized
    /// files, and accepts only plain session names and paths.
    #[test]
    fn k8s_a_login_store_read_refuses_links_and_odd_names() {
        let s = read_store_script(SESSION, &["claude/.credentials.json"]).unwrap();
        let base = format!("/logins/{SESSION}");
        assert!(s.contains(&format!("[ ! -L {base} ]")), "{s}");
        assert!(
            s.contains(&format!("if [ -L {base}/claude ]; then exit 46")),
            "{s}"
        );
        assert!(
            s.contains(&format!(
                "if [ -L {base}/claude/.credentials.json ]; then exit 46"
            )),
            "{s}"
        );
        assert!(s.contains("exit 45") && s.contains("exit 66"), "{s}");
        for bad in ["", "../x", "a/b", "A-1", "x;rm", "-a", "a-"] {
            assert!(read_store_script(bad, &["claude/x"]).is_err(), "{bad}");
        }
        for bad in [
            "", "a//b", "../x", "a/../b", "./a", "a b", "a;b", "$(x)", "a/",
        ] {
            assert!(read_store_script(SESSION, &[bad]).is_err(), "{bad}");
        }
        // The output parses as the broker read does.
        let out = b"claude/x 3\nabc".to_vec();
        assert_eq!(
            gw::parse_read(&out).unwrap(),
            vec![("claude/x".to_string(), b"abc".to_vec())]
        );
    }

    #[test]
    fn k8s_a_login_store_is_prepared_with_the_clis_homes() {
        let s = init_store_script("copilot", SESSION).unwrap();
        assert!(
            s.contains(&format!("mkdir -p /logins/{SESSION}/copilot")),
            "{s}"
        );
        assert!(s.contains("storeTokenPlaintext"), "{s}");
        assert!(s.contains("umask 077"), "{s}");
        assert!(init_store_script("claude", "../etc").is_err());
    }

    /// B-4: crew's terminal runs a pinned `kubectl exec -it` into the session's pod; the login
    /// command is the agent's own, and nothing secret is on it.
    #[test]
    fn k8s_the_login_argv_is_a_pinned_exec_into_the_sessions_pod() {
        let argv = login_exec_argv(&kube(), "wicked-sys-wabc123def456", "copilot", SESSION);
        // B-4 review #1: the terminal's KUBECONFIG never picks the cluster.
        assert_eq!(
            argv[..3],
            ["/usr/bin/env", "-u", "KUBECONFIG"].map(String::from)
        );
        let argv = &argv[3..];
        let head = kube().head(Some("wicked-sys-wabc123def456"));
        assert_eq!(argv[..head.len()], head[..]);
        assert_eq!(
            argv[head.len()..head.len() + 6],
            ["exec", "-it", SESSION, "-c", "login", "--"].map(String::from)
        );
        assert_eq!(
            argv[head.len() + 6..],
            ["copilot", "login", "--device-code"].map(String::from)
        );
    }

    /// B-4: the container floor's helper journals cannot settle on the cluster (their helpers
    /// wrote Docker volumes this broker never reads): the cluster transport drops them, so a
    /// sign-in never reads as "mutation helper cleanup incomplete" forever after the swap.
    #[test]
    fn k8s_a_docker_helper_journal_never_blocks_a_cluster_sign_in() {
        let dir = std::env::temp_dir().join(format!(
            "wicked-k8s-login-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(dir.join("refresh")).unwrap();
        std::fs::write(
            crate::seat_gateway::unsettled_path(&dir, "claude"),
            b"wicked-mutation-0123",
        )
        .unwrap();
        let before = crate::seat_login::publication_status(
            &dir,
            "claude",
            crate::seat_login::BrokerStatus::SignedOut,
        );
        assert!(matches!(
            before,
            crate::seat_login::BrokerStatus::Unknown(_)
        ));
        K8sIo {
            gw: None,
            image: std::sync::Mutex::new(None),
        }
        .cleanup(&dir)
        .unwrap();
        assert_eq!(
            crate::seat_login::publication_status(
                &dir,
                "claude",
                crate::seat_login::BrokerStatus::SignedOut
            ),
            crate::seat_login::BrokerStatus::SignedOut
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn sh(script: &str) -> std::process::Output {
        use wicked_apps_core::HardenedCommand as _;
        std::process::Command::new("/bin/sh")
            .hardened()
            .args(["-c", script])
            .output()
            .unwrap()
    }

    fn store_root() -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("wicked-store-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(d.join(SESSION)).unwrap();
        d
    }

    /// B-4 review #8: the store read runs for real (as the writer runs it): a plain file is read,
    /// a missing one skipped, a link on the way and a fifo refused.
    #[test]
    fn k8s_a_login_store_read_runs_and_refuses_links_and_fifos() {
        let root = store_root();
        let base = root.join(SESSION);
        std::fs::create_dir_all(base.join("claude")).unwrap();
        std::fs::write(base.join("claude/tok"), "abc").unwrap();
        let r = root.to_str().unwrap();
        let out = sh(&read_store_script_at(r, SESSION, &["claude/tok", "claude/none"]).unwrap());
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            gw::parse_read(&out.stdout).unwrap(),
            vec![("claude/tok".to_string(), b"abc".to_vec())]
        );
        let outside = root.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("tok"), "secret").unwrap();
        std::os::unix::fs::symlink(&outside, base.join("copilot")).unwrap();
        let out = sh(&read_store_script_at(r, SESSION, &["copilot/tok"]).unwrap());
        assert_eq!(out.status.code(), Some(46));
        assert!(out.stdout.is_empty());
        let fifo = base.join("claude/fifo");
        assert!(std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success());
        let out = sh(&read_store_script_at(r, SESSION, &["claude/fifo"]).unwrap());
        assert_eq!(out.status.code(), Some(45));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// B-4 review #6: a store a vendor CLI locked (`chmod 000` dirs) is still removed, and a store
    /// that is a link is removed as a link, its target untouched.
    #[test]
    fn k8s_a_locked_login_store_is_removed_and_a_link_never_followed() {
        let root = store_root();
        let base = root.join(SESSION);
        std::fs::create_dir_all(base.join("a/b")).unwrap();
        std::fs::write(base.join("a/b/tok"), "secret").unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(base.join("a/b"), std::fs::Permissions::from_mode(0o000)).unwrap();
        std::fs::set_permissions(base.join("a"), std::fs::Permissions::from_mode(0o000)).unwrap();
        let r = root.to_str().unwrap();
        let out = sh(&remove_store_script(r, SESSION).unwrap());
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!base.exists());
        let target = root.join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("keep"), "k").unwrap();
        std::os::unix::fs::symlink(&target, &base).unwrap();
        let out = sh(&remove_store_script(r, SESSION).unwrap());
        assert!(out.status.success());
        assert!(std::fs::symlink_metadata(&base).is_err());
        assert!(target.join("keep").exists());
        // Removing what is gone is done.
        assert!(sh(&remove_store_script(r, SESSION).unwrap())
            .status
            .success());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// B-4 review #5: a gateway process that runs but is not ready can still accept a pending key:
    /// unknown, never "not serving". Only no process alive is "not serving".
    #[test]
    fn k8s_an_unready_gateway_is_never_read_as_stopped() {
        let pod = |state: Value, ready: bool| {
            json!({"metadata": {"name": "agw-1"}, "status": {"phase": "Running",
                "conditions": [{"type": "Ready", "status": if ready {"True"} else {"False"}}],
                "containerStatuses": [{"name": "gateway", "ready": ready, "state": state}]}})
        };
        assert_eq!(gw::serving_state(&[]), Some(false));
        assert_eq!(
            gw::serving_state(&[pod(json!({"running": {}}), true)]),
            Some(true)
        );
        assert_eq!(
            gw::serving_state(&[pod(json!({"running": {}}), false)]),
            None
        );
        assert_eq!(
            gw::serving_state(&[pod(
                json!({"waiting": {"reason": "PodInitializing"}}),
                false
            )]),
            Some(false)
        );
        assert_eq!(
            gw::serving_state(&[pod(json!({"terminated": {}}), false)]),
            Some(false)
        );
    }

    /// B-4 review #2: a sign-in never re-applies an existing gateway; one without login stores is
    /// refused with what to do.
    #[test]
    fn k8s_a_gateway_without_login_stores_is_named() {
        let dep = render::gateway_deployment(&spec());
        assert!(gw::has_login_stores(&dep));
        let mut old = dep.clone();
        old["spec"]["template"]["spec"]["volumes"] = json!([]);
        assert!(!gw::has_login_stores(&old));
    }
}
