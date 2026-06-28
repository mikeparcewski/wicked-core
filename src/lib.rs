//! wicked-core — the in-process composition runtime for the wicked-estate core services.
//!
//! One thread (the [`actor`]) owns the writable estate store; everything else holds a clonable
//! [`Core`] handle and talks to it via commands + a live event stream. This separates the
//! system-of-record (SQLite, single writer) from the orchestration seam (a command API + events),
//! so consumers (agent, UI, MCP) stop re-opening and racing on the shared file. See `DESIGN.md`.
//!
//! P1 (this file): the actor + command/reply + event fan-out + a read path. The plan → distribute
//! → execute → evidence pipeline and the lifecycle commands land in P2 (see `DESIGN.md`).

mod actor;
mod command;
mod distribute;
mod domain;
mod event;
mod execute;
mod pipeline;
mod plan;
mod scope;

pub use domain::{
    all_sessions, get_session, get_work_output, put_node, session_units, AgentSession,
    HumanConfirm, SessionStatus, SessionView, UnitStatus, WorkUnit,
};
pub use event::CoreEvent;
pub use pipeline::SessionResult;
pub use scope::{resolve_scope, EntityMode};
pub use wicked_council::AgenticCli;

/// What to run: the problem to decompose, the council roster (`AgenticCli` seats), the scope toggle,
/// and a stable session id. The roster is passed explicitly so callers (tests, UI) control it; the
/// UI resolves it from the council registry.
pub struct LaunchSpec {
    pub problem: String,
    pub clis: Vec<AgenticCli>,
    pub entity_mode: EntityMode,
    pub session_id: String,
}

/// Resolve the council roster from the registry (built-ins merged with the user's
/// `~/.config/wicked-council/clis.toml`), keeping only council-enabled seats. This is what a
/// consumer passes as [`LaunchSpec::clis`] for a real run.
pub fn registry_roster() -> Vec<AgenticCli> {
    let user = std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".config/wicked-council/clis.toml"));
    wicked_council::registry::load(user.as_deref())
        .unwrap_or_default()
        .into_iter()
        .filter(|c| c.enabled_for_council)
        .collect()
}

use command::Command;
use std::sync::mpsc::{channel, Receiver, Sender};

/// A handle to the core runtime. Clone freely — every clone funnels into the single store-owning
/// actor thread, so callers compose the core services without contending on the SQLite file.
#[derive(Clone)]
pub struct Core {
    tx: Sender<Command>,
}

impl Core {
    /// Spawn the store actor over the estate store at `path`. The actor lives until every `Core`
    /// handle is dropped.
    pub fn spawn(path: impl Into<String>) -> Core {
        let (tx, rx) = channel();
        let path = path.into();
        std::thread::spawn(move || actor::run(path, rx));
        Core { tx }
    }

    /// Subscribe to the live event stream. Returns a receiver that gets every [`CoreEvent`] emitted
    /// after this call (the UI watches work happen instead of polling).
    pub fn subscribe(&self) -> Receiver<CoreEvent> {
        let (s, r) = channel();
        let _ = self.tx.send(Command::Subscribe(s));
        r
    }

    /// Liveness probe — emits a `Heartbeat` to subscribers and waits for the actor to ack.
    pub fn ping(&self) {
        let (reply, rx) = channel();
        if self.tx.send(Command::Ping(reply)).is_ok() {
            let _ = rx.recv();
        }
    }

    /// Launch a full governed session. Fire-and-forget: returns the session id immediately while the
    /// run proceeds on the actor thread, streaming progress (and any failure) as [`CoreEvent`]s.
    /// `subscribe()` BEFORE calling this to catch the whole sequence.
    pub fn launch(&self, mut spec: LaunchSpec) -> String {
        if spec.session_id.trim().is_empty() {
            spec.session_id = format!(
                "sess-{}",
                pipeline::deterministic_id(&[&spec.problem, &spec.clis.len().to_string()])
            );
        }
        let session_id = spec.session_id.clone();
        let _ = self.tx.send(Command::Launch(spec));
        session_id
    }

    /// The agent session ids currently on the store (lightweight; use [`sessions_detail`] for the
    /// full project list).
    pub fn sessions(&self) -> anyhow::Result<Vec<String>> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::Sessions(reply))
            .map_err(|_| anyhow::anyhow!("core actor stopped"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("core actor dropped the reply"))?
    }

    /// Every session + its ordered units — the read the UI builds its project list from.
    pub fn sessions_detail(&self) -> anyhow::Result<Vec<SessionView>> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::Projects(reply))
            .map_err(|_| anyhow::anyhow!("core actor stopped"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("core actor dropped the reply"))?
    }

    /// A unit's captured work output (the transcript), if any.
    pub fn work_output(&self, unit_id: &str) -> Option<String> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::WorkOutput(unit_id.to_string(), reply))
            .ok()?;
        rx.recv().ok().flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // Proves the P1 pattern end to end: one actor owns the store, serves a read, and fans events
    // out to subscribers — all in-process, no file polling, no second writer.
    #[test]
    fn actor_owns_store_serves_reads_and_emits_events() {
        let dir = std::env::temp_dir().join("wicked-core-test");
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("core-test.db");
        let _ = std::fs::remove_file(&db);

        let core = Core::spawn(db.display().to_string());
        let events = core.subscribe();

        // Read path: a fresh store has no agent sessions, and the read succeeds (actor owns it).
        let sessions = core.sessions().expect("sessions read should succeed");
        assert!(sessions.is_empty(), "a fresh store has no sessions");

        // Event path: ping emits a Heartbeat to the subscriber registered above (FIFO ordering on
        // the command channel guarantees Subscribe was processed before Ping).
        core.ping();
        let ev = events
            .recv_timeout(Duration::from_secs(2))
            .expect("a Heartbeat event should arrive");
        assert_eq!(ev, CoreEvent::Heartbeat);
    }

    // The whole point of COE: a launch composes plan → distribute (council) → execute (governance +
    // orchestration) and STREAMS the progress as live events, all through the single-writer actor.
    #[cfg(unix)]
    #[test]
    fn launch_runs_the_full_pipeline_and_streams_events() {
        use std::os::unix::fs::PermissionsExt;
        use wicked_council::types::{Category, Confidence, InputMode};

        let dir = std::env::temp_dir().join("wicked-core-launch-test");
        std::fs::create_dir_all(&dir).unwrap();
        let mk = |name: &str, key: &str| -> std::path::PathBuf {
            let p = dir.join(name);
            std::fs::write(
                &p,
                format!("#!/bin/sh\necho \"RECOMMENDATION: {key}\"\necho \"TOP_RISK: none\"\necho \"CHANGE_MY_MIND: no\"\necho \"DISQUALIFIER: None\"\nexit 0\n"),
            )
            .unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p
        };
        let cli = |key: &str, path: &std::path::Path| AgenticCli {
            key: key.to_string(),
            display_name: key.to_string(),
            binary: path.display().to_string(),
            headless_invocation: format!("{} {{PROMPT}}", path.display()),
            category: Category::default(),
            input_mode: InputMode::default(),
            version_probe: vec![],
            trust_flags: vec![],
            alt_binaries: vec![],
            confidence: Confidence::default(),
            enabled_for_council: true,
        };
        let clis = vec![
            cli("fake-a", &mk("fake-a.sh", "fake-a")),
            cli("fake-b", &mk("fake-b.sh", "fake-b")),
        ];

        let db = dir.join("launch.db");
        let _ = std::fs::remove_file(&db);
        let core = Core::spawn(db.display().to_string());
        let events = core.subscribe();

        let sid = core.launch(LaunchSpec {
            problem: "Do step one. Do step two".to_string(),
            clis,
            entity_mode: EntityMode::Shared,
            session_id: "test-launch".to_string(),
        });
        assert_eq!(sid, "test-launch");

        // Collect the live stream until the session completes.
        let mut got: Vec<CoreEvent> = Vec::new();
        loop {
            match events.recv_timeout(Duration::from_secs(20)) {
                Ok(ev) => {
                    let done = matches!(ev, CoreEvent::SessionCompleted { .. });
                    got.push(ev);
                    if done {
                        break;
                    }
                }
                Err(_) => panic!("timed out before SessionCompleted; got {got:?}"),
            }
        }

        let n = |pred: fn(&CoreEvent) -> bool| got.iter().filter(|e| pred(e)).count();
        assert_eq!(n(|e| matches!(e, CoreEvent::SessionStarted { .. })), 1);
        assert_eq!(
            n(|e| matches!(e, CoreEvent::UnitPlanned { .. })),
            2,
            "two units planned"
        );
        assert_eq!(n(|e| matches!(e, CoreEvent::UnitDistributed { .. })), 2);
        assert_eq!(n(|e| matches!(e, CoreEvent::GateDecided { .. })), 2);
        assert_eq!(
            n(|e| matches!(e, CoreEvent::UnitDone { .. })),
            2,
            "no deny policy registered ⇒ both units approve"
        );

        // Read back through COE's read API (exactly what the UI will use — no file polling).
        let detail = core.sessions_detail().expect("sessions_detail");
        assert_eq!(detail.len(), 1);
        assert_eq!(detail[0].session.id, "test-launch");
        assert_eq!(detail[0].units.len(), 2);
        assert!(detail[0].units.iter().all(|u| u.status == UnitStatus::Done));
        let out = core
            .work_output("test-launch:u1")
            .expect("unit 1 has captured output");
        assert!(out.contains("stub-output"), "transcript was: {out}");
    }
}
