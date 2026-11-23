//! The sign-in state machine over its transport seam (k8s-swap B-4): one copy of the journals,
//! tombstones and sessions drives any [`BrokerIo`]. An in-memory transport proves the whole
//! container-login flow (open, ingest, the seat's read, sign-out) and the C4 settle on the seat's
//! read, with no runtime or cluster.

use super::*;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

/// Failures at the BrokerIo boundary, replacing the old shell stub's failure files.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Fail {
    RemoveLogin,
    RemoveBroker(String),
    Probe,
    Write(String),
    Publish,
    Cleanup,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GatewayState {
    #[default]
    Running,
    Stopped,
    Absent,
    Unknown,
}

#[derive(Default)]
pub(crate) struct MemIo {
    broker: RefCell<Option<BTreeMap<String, Vec<u8>>>>,
    stores: RefCell<BTreeMap<String, BTreeMap<String, Vec<u8>>>>,
    pods: RefCell<BTreeSet<String>>,
    pub(crate) log: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    fail: RefCell<BTreeSet<Fail>>,
    pub(crate) gateway_state: RefCell<GatewayState>,
    pub(crate) broker_missing: RefCell<bool>,
    pub(crate) published: RefCell<Vec<u8>>,
    /// When set: the gateway dir whose mutation lock must be held at `open_login` and free at
    /// `prepare_login` (review B-4 #3).
    lock_dir: RefCell<Option<std::path::PathBuf>>,
    /// When set: `open_login` starts the pod, then fails; `remove_login` fails (codex b4-r2 #2).
    fail_open_and_remove: RefCell<bool>,
}

impl MemIo {
    fn note(&self, s: String) {
        self.log.lock().unwrap().push(s);
    }
    pub(crate) fn broker_file(&self, f: &str) -> Option<Vec<u8>> {
        self.broker.borrow().as_ref()?.get(f).cloned()
    }
    pub(crate) fn broker_text(&self, file: &str) -> Option<String> {
        self.broker_file(file)
            .map(|v| String::from_utf8(v).unwrap())
    }
    pub(crate) fn seed_broker(&self, file: &str, bytes: impl AsRef<[u8]>) {
        self.broker
            .borrow_mut()
            .get_or_insert_with(BTreeMap::new)
            .insert(file.into(), bytes.as_ref().to_vec());
    }
    pub(crate) fn seed_login(&self, dir: &Path, cli: &str, file: &str, bytes: impl AsRef<[u8]>) {
        let session = read_session(dir, cli).unwrap().expect("an open session");
        self.stores
            .borrow_mut()
            .entry(session)
            .or_default()
            .insert(file.into(), bytes.as_ref().to_vec());
    }
    pub(crate) fn remove_login_file(&self, dir: &Path, cli: &str, file: &str) {
        let session = read_session(dir, cli).unwrap().expect("an open session");
        self.stores
            .borrow_mut()
            .entry(session)
            .or_default()
            .remove(file);
    }
    pub(crate) fn fail(&self, failure: Fail) {
        self.fail.borrow_mut().insert(failure);
    }
    pub(crate) fn recover(&self, failure: Fail) {
        assert!(
            self.fail.borrow_mut().remove(&failure),
            "failure was injected"
        );
    }
    pub(crate) fn calls(&self) -> String {
        self.log.lock().unwrap().join("\n")
    }
    pub(crate) fn clear_calls(&self) {
        self.log.lock().unwrap().clear();
    }
    fn check(&self, failure: Fail) -> Result<(), String> {
        if self.fail.borrow().contains(&failure) {
            Err(format!("injected {failure:?}"))
        } else {
            Ok(())
        }
    }
    fn publish(&self, dir: &Path) -> Result<(), String> {
        self.note("publish".into());
        *self.published.borrow_mut() =
            std::fs::read(dir.join("config/config.json")).map_err(|e| e.to_string())?;
        self.check(Fail::Publish)
    }
}

impl BrokerIo for MemIo {
    fn broker_present(&self) -> Result<bool, String> {
        self.note("broker present".into());
        Ok(!*self.broker_missing.borrow() && self.broker.borrow().is_some())
    }
    fn init_broker(&self, dir: &Path) -> Result<(), String> {
        self.note("init broker".into());
        *self.broker_missing.borrow_mut() = false;
        let mut config = crate::seat_gateway::load_config(dir)?;
        crate::seat_gateway::reconcile_routes(&mut config);
        let mut broker = self.broker.borrow_mut();
        let broker = broker.get_or_insert_with(BTreeMap::new);
        for route in config.providers {
            let file = route
                .credential_file
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();
            broker.entry(file).or_default();
        }
        Ok(())
    }
    fn read_broker(
        &self,
        paths: &[&str],
        presence: bool,
    ) -> Result<Vec<(String, Vec<u8>)>, String> {
        self.note("read broker".into());
        let b = self.broker.borrow();
        let b = b.as_ref().ok_or("no broker")?;
        Ok(paths
            .iter()
            .filter_map(|p| {
                let v = b.get(*p)?;
                let v = if presence && !is_archive(p) {
                    if v.is_empty() {
                        Vec::new()
                    } else {
                        b"1".to_vec()
                    }
                } else {
                    v.clone()
                };
                Some(((*p).to_string(), v))
            })
            .collect())
    }
    fn write_broker(&self, _d: &Path, _c: &str, file: &str, body: &[u8]) -> Result<(), String> {
        self.note(format!("write {file}"));
        self.check(Fail::Write(file.into()))?;
        self.broker
            .borrow_mut()
            .get_or_insert_with(BTreeMap::new)
            .insert(file.into(), body.to_vec());
        Ok(())
    }
    fn empty_broker(&self, _d: &Path, _c: &str, files: &[&str]) -> Result<(), String> {
        let mut b = self.broker.borrow_mut();
        let b = b.get_or_insert_with(BTreeMap::new);
        for f in files {
            self.note(format!("empty {f}"));
            b.insert((*f).into(), Vec::new());
        }
        Ok(())
    }
    fn remove_broker(&self, _d: &Path, _c: &str, file: &str) -> Result<(), String> {
        self.note(format!("remove broker {file}"));
        self.check(Fail::RemoveBroker(file.into()))?;
        if let Some(b) = self.broker.borrow_mut().as_mut() {
            b.remove(file);
        }
        Ok(())
    }
    fn cleanup(&self, dir: &Path) -> Result<(), String> {
        self.note("cleanup".into());
        self.check(Fail::Cleanup)?;
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
        self.note(format!("route {cli}"));
        crate::seat_gateway::route_sign_in_with(dir, cli, literals, || self.publish(dir))
    }
    fn confirm_sign_out(&self, dir: &Path, cli: &str) -> Result<(), String> {
        self.note(format!("confirm {cli}"));
        let serving = match *self.gateway_state.borrow() {
            GatewayState::Running => Some(true),
            GatewayState::Stopped | GatewayState::Absent => Some(false),
            GatewayState::Unknown => None,
        };
        crate::seat_gateway::confirm_sign_out_with(
            dir,
            cli,
            serving,
            serving == Some(false),
            || self.publish(dir),
            |_, accepted| {
                self.note(format!("probe {accepted}"));
                self.check(Fail::Probe)
            },
        )
    }
    fn login_exists(&self, session: &str) -> Result<bool, String> {
        self.note(format!("login exists {session}"));
        Ok(self.stores.borrow().contains_key(session) || self.pods.borrow().contains(session))
    }
    fn create_login(&self, session: &str) -> Result<(), String> {
        self.note(format!("create login {session}"));
        self.stores
            .borrow_mut()
            .insert(session.into(), BTreeMap::new());
        Ok(())
    }
    fn init_login(&self, _d: &Path, cli: &str, session: &str) -> Result<(), String> {
        self.note(format!("init {cli} {session}"));
        if cli == "copilot" {
            self.stores.borrow_mut().get_mut(session).unwrap().insert(
                "copilot/settings.json".into(),
                COPILOT_SETTINGS.as_bytes().to_vec(),
            );
        }
        Ok(())
    }
    fn prepare_login(&self, cli: &str) -> Result<(), String> {
        if let Some(d) = self.lock_dir.borrow().as_ref() {
            assert!(
                crate::seat_refresh::try_mutation_lock(d).unwrap().is_some(),
                "a login is prepared outside the transaction"
            );
        }
        self.note(format!("prepare {cli}"));
        Ok(())
    }
    fn open_login(&self, cli: &str, session: &str) -> Result<Vec<String>, String> {
        if let Some(d) = self.lock_dir.borrow().as_ref() {
            assert!(
                crate::seat_refresh::try_mutation_lock(d).unwrap().is_none(),
                "a login starts inside the session's transaction"
            );
        }
        self.pods.borrow_mut().insert(session.into());
        if *self.fail_open_and_remove.borrow() {
            return Err("injected: the login pod did not become ready".into());
        }
        Ok(vec!["exec".into(), session.into(), cli.into()])
    }
    fn read_login(&self, session: &str, paths: &[&str]) -> Result<Vec<(String, Vec<u8>)>, String> {
        assert!(
            !self.pods.borrow().contains(session),
            "a store is read only after its login pod is gone"
        );
        self.note(format!("read login {session}"));
        let s = self.stores.borrow();
        let s = s.get(session).ok_or("no store")?;
        Ok(paths
            .iter()
            .filter_map(|p| s.get(*p).map(|v| ((*p).to_string(), v.clone())))
            .collect())
    }
    fn stop_login(&self, session: &str) -> Result<(), String> {
        self.note(format!("stop login {session}"));
        self.pods.borrow_mut().remove(session);
        Ok(())
    }
    fn remove_login(&self, session: &str) -> Result<(), String> {
        self.note(format!("remove login {session}"));
        // A missing store counts as removed even if the remove operation fails.
        if self.login_exists(session)? {
            self.check(Fail::RemoveLogin)?;
        }
        if *self.fail_open_and_remove.borrow() {
            return Err("injected: the cluster is unreachable".into());
        }
        self.pods.borrow_mut().remove(session);
        self.stores.borrow_mut().remove(session);
        Ok(())
    }
    fn send(
        &self,
        _d: &Path,
        cli: &str,
        _r: &crate::seat_refresh::Request,
    ) -> Result<Value, String> {
        self.note(format!("send {cli}"));
        self.check(Fail::Probe)?;
        Err("no account reads here".into())
    }
}

fn temp_dir() -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("wicked-io-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn token() -> String {
    format!("sk-ant-oat01-{}", "A".repeat(40))
}

/// Open a claude sign-in, let its login leave a setup token in the store, and ingest it.
fn sign_claude_in(io: &MemIo, dir: &Path) {
    let argv = prepare_login_argv(io, dir, "claude").unwrap();
    let session = read_session(dir, "claude").unwrap().expect("a session");
    assert_eq!(
        argv,
        vec!["exec".to_string(), session.clone(), "claude".into()]
    );
    assert!(io
        .log
        .lock()
        .unwrap()
        .contains(&format!("init claude {session}")));
    io.stores
        .borrow_mut()
        .get_mut(&session)
        .unwrap()
        .insert(CLAUDE_SETUP_TOKEN_PATH.into(), token().into_bytes());
    let report = ingest_and_route(io, dir, "claude").unwrap();
    assert!(
        report.files.contains(&"anthropic.token".to_string()),
        "{:?}",
        report.files
    );
}

/// B-4: the whole container-login flow runs over the transport seam: the login opens a session
/// (store, then marker, then homes, then the pod), ingest stops the pod BEFORE reading the store,
/// publishes the route, and removes the store and the marker; the seat reads the same sign-in
/// and token; sign-out empties the route files and confirms the generation.
#[test]
fn a_container_login_signs_in_and_out_over_any_transport() {
    let (io, dir) = (MemIo::default(), temp_dir());
    sign_claude_in(&io, &dir);
    assert_eq!(
        io.broker_file("anthropic.token"),
        Some(token().into_bytes())
    );
    assert!(io.log.lock().unwrap().contains(&"route claude".to_string()));
    assert!(
        io.stores.borrow().is_empty(),
        "the credential-bearing store is gone"
    );
    assert!(io.pods.borrow().is_empty());
    assert_eq!(read_session(&dir, "claude").unwrap(), None);
    let seat = seat_sign_in(&io, &dir, "claude").unwrap();
    assert_eq!(
        seat.kind,
        Some(crate::seat_credentials::CredentialKind::ClaudeOauth)
    );
    assert_eq!(claude_seat_token(&io, &dir).unwrap(), token());
    assert!(matches!(
        broker_status(&io, "claude"),
        BrokerStatus::SignedIn { .. }
    ));
    sign_out_and_route(&io, &dir, "claude").unwrap();
    assert_eq!(io.broker_file("anthropic.token"), Some(Vec::new()));
    assert!(io
        .log
        .lock()
        .unwrap()
        .contains(&"confirm claude".to_string()));
    assert_eq!(broker_status(&io, "claude"), BrokerStatus::SignedOut);
    assert!(seat_sign_in(&io, &dir, "claude")
        .unwrap_err()
        .contains("not signed in"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// B-4 / C4: a seat's sign-in read settles an interrupted sign-out first (B-2's cluster read did
/// not, so a crash mid-sign-out kept refusing every seat with "sign-out incomplete" and the
/// credential stayed served until the operator signed out again).
#[test]
fn a_seats_sign_in_read_finishes_an_interrupted_sign_out() {
    let (io, dir) = (MemIo::default(), temp_dir());
    sign_claude_in(&io, &dir);
    // A sign-out that crashed right after its tombstone: nothing emptied yet.
    crate::seat_refresh::create_private_file(&signout_path(&dir, "claude"), b"sign-out").unwrap();
    assert_eq!(
        io.broker_file("anthropic.token"),
        Some(token().into_bytes())
    );
    let err = seat_sign_in(&io, &dir, "claude").unwrap_err();
    assert!(err.contains("not signed in"), "{err}");
    assert_eq!(io.broker_file("anthropic.token"), Some(Vec::new()));
    assert!(io
        .log
        .lock()
        .unwrap()
        .contains(&"confirm claude".to_string()));
    assert!(!signout_path(&dir, "claude").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

/// B-4: a new sign-in discards an abandoned one (crew's `rm -f` does nothing for a pod): its pod
/// and its credential-bearing store are gone before the new session exists.
#[test]
fn a_new_sign_in_discards_an_abandoned_login_pod_and_store() {
    let (io, dir) = (MemIo::default(), temp_dir());
    prepare_login_argv(&io, &dir, "claude").unwrap();
    let first = read_session(&dir, "claude").unwrap().unwrap();
    io.stores
        .borrow_mut()
        .get_mut(&first)
        .unwrap()
        .insert(CLAUDE_SETUP_TOKEN_PATH.into(), token().into_bytes());
    prepare_login_argv(&io, &dir, "claude").unwrap();
    let second = read_session(&dir, "claude").unwrap().unwrap();
    assert_ne!(first, second);
    assert!(!io.pods.borrow().contains(&first));
    assert!(!io.stores.borrow().contains_key(&first));
    assert!(io.pods.borrow().contains(&second));
    let _ = std::fs::remove_dir_all(&dir);
}

/// B-4 review #3: the login is prepared (image, isolation proof) outside the transaction, and the
/// session through its started pod is one transaction, so no sign-out or newer sign-in can finish
/// in between and leave the pod behind.
#[test]
fn a_login_starts_inside_its_sessions_transaction() {
    let (io, dir) = (MemIo::default(), temp_dir());
    *io.lock_dir.borrow_mut() = Some(dir.clone());
    prepare_login_argv(&io, &dir, "claude").unwrap();
    assert!(io
        .log
        .lock()
        .unwrap()
        .contains(&"prepare claude".to_string()));
    let _ = std::fs::remove_dir_all(&dir);
}

/// codex b4-r2 #2: a login that failed to start and whose cleanup failed too keeps its session
/// marker, so the next sign-in still finds and clears its pod and store.
#[test]
fn a_failed_login_whose_cleanup_fails_keeps_its_session_marker() {
    let (io, dir) = (MemIo::default(), temp_dir());
    *io.fail_open_and_remove.borrow_mut() = true;
    let e = prepare_login_argv(&io, &dir, "claude").unwrap_err();
    assert!(e.contains("could not be cleaned up"), "{e}");
    let left = read_session(&dir, "claude")
        .unwrap()
        .expect("the marker stays");
    assert!(io.pods.borrow().contains(&left));
    *io.fail_open_and_remove.borrow_mut() = false;
    prepare_login_argv(&io, &dir, "claude").unwrap();
    assert!(
        !io.pods.borrow().contains(&left),
        "the next sign-in cleared it"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// #1438: a failed journal removal must retain it for the state machine's next repair.
#[test]
fn a_failed_broker_remove_retains_the_file_until_retry() {
    let io = MemIo::default();
    let file = "claude.ingest.next";
    io.seed_broker(file, b"journal");
    io.fail(Fail::RemoveBroker(file.into()));
    assert!(io
        .remove_broker(Path::new("/unused"), "claude", file)
        .is_err());
    assert_eq!(io.broker_file(file), Some(b"journal".to_vec()));
    io.recover(Fail::RemoveBroker(file.into()));
    io.remove_broker(Path::new("/unused"), "claude", file)
        .unwrap();
    assert_eq!(io.broker_file(file), None);
    // Already removed is harmless, too.
    io.remove_broker(Path::new("/unused"), "claude", file)
        .unwrap();
}

/// core#1673 (crew#1523): cancelling a sign-in ends exactly the session the last `argv` opened:
/// its login pod stops and its credential-bearing store goes, then its marker, while the
/// credential the broker already holds stays. A second abandon is a no-op.
#[test]
fn abandon_ends_the_open_login_pod_and_store_and_keeps_the_broker_credential() {
    let (io, dir) = (MemIo::default(), temp_dir());
    sign_claude_in(&io, &dir);
    prepare_login_argv(&io, &dir, "claude").unwrap();
    let open = read_session(&dir, "claude")
        .unwrap()
        .expect("an open sign-in");
    // The person typed half a sign-in: the store already holds a (newer) token.
    io.seed_login(&dir, "claude", CLAUDE_SETUP_TOKEN_PATH, b"half-finished");
    assert!(io.pods.borrow().contains(&open));
    assert!(abandon_login(&io, &dir, "claude").unwrap());
    assert!(!io.pods.borrow().contains(&open), "the login pod is gone");
    assert!(!io.stores.borrow().contains_key(&open), "its store is gone");
    assert_eq!(read_session(&dir, "claude").unwrap(), None);
    assert_eq!(
        io.broker_file("anthropic.token"),
        Some(token().into_bytes()),
        "the broker's credential is left alone"
    );
    assert!(!io.calls().contains("confirm claude"), "no sign-out ran");
    // Idempotent: nothing open now.
    assert!(!abandon_login(&io, &dir, "claude").unwrap());
    // The cancelled sign-in is over: a late ingest finds no session to import.
    let err = ingest_and_route(&io, &dir, "claude").unwrap_err();
    assert!(err.contains("login session"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// core#1673: a store whose removal fails keeps its session marker, so the next abandon (or
/// sign-in, or sign-out) still finds and clears it.
#[test]
fn a_failed_abandon_keeps_the_session_marker_for_a_retry() {
    let (io, dir) = (MemIo::default(), temp_dir());
    prepare_login_argv(&io, &dir, "claude").unwrap();
    let open = read_session(&dir, "claude").unwrap().unwrap();
    io.fail(Fail::RemoveLogin);
    assert!(abandon_login(&io, &dir, "claude").is_err());
    assert_eq!(read_session(&dir, "claude").unwrap(), Some(open.clone()));
    assert!(io.stores.borrow().contains_key(&open));
    io.recover(Fail::RemoveLogin);
    assert!(abandon_login(&io, &dir, "claude").unwrap());
    assert!(!io.stores.borrow().contains_key(&open));
    assert_eq!(read_session(&dir, "claude").unwrap(), None);
    let _ = std::fs::remove_dir_all(&dir);
}

/// core#1673: an ingest whose publication was interrupted is finished before anything is
/// abandoned (its session marker is how that repair finds it), as a new sign-in does.
#[test]
fn abandon_finishes_an_interrupted_publication_first() {
    let (io, dir) = (MemIo::default(), temp_dir());
    prepare_login_argv(&io, &dir, "claude").unwrap();
    io.seed_login(&dir, "claude", CLAUDE_SETUP_TOKEN_PATH, token());
    io.fail(Fail::Publish);
    assert!(ingest_and_route(&io, &dir, "claude").is_err());
    assert!(signin_pending(&dir, "claude"));
    io.recover(Fail::Publish);
    assert!(abandon_login(&io, &dir, "claude").unwrap());
    assert!(!signin_pending(&dir, "claude"));
    assert_eq!(
        io.broker_file("anthropic.token"),
        Some(token().into_bytes()),
        "the interrupted sign-in was published, not dropped"
    );
    assert!(io.stores.borrow().is_empty());
    assert_eq!(read_session(&dir, "claude").unwrap(), None);
    let _ = std::fs::remove_dir_all(&dir);
}
