//! wicked-core — the in-process composition runtime for the wicked-estate core services.
//!
//! One thread (the [`actor`]) owns the writable estate store; everything else holds a clonable
//! [`Core`] handle and talks to it via commands + a live event stream. This separates the
//! system-of-record (SQLite, single writer) from the orchestration seam (a command API + events),
//! so consumers (agent, UI, MCP) stop re-opening and racing on the shared file. See `DESIGN.md`.
//!
//! Built: the actor + command/reply + event fan-out, the full plan → distribute → execute →
//! evidence pipeline ([`Core::launch`], stub execute path), and the read API
//! ([`Core::sessions_detail`], [`Core::work_output`]). Remaining (see `DESIGN.md`): the wrapped-CLI
//! execute backend (real subprocess + gate-hook), migrating the GUI onto `Core`, and deleting the
//! `wicked-agent` crate.

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

    // The whole point of COE: the pipeline composes plan → distribute (council synthesis) → execute
    // (governance + orchestration) → evidence, and STREAMS the progress as live events. Uses a STUB
    // dispatcher so the council runs its real synthesis over deterministic votes — NO subprocess, so
    // the test is reliable (the real-subprocess dispatch is wicked-council's own concern).
    #[test]
    fn pipeline_composes_and_streams_events_deterministically() {
        use std::sync::Arc;
        use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
        use wicked_council::CouncilTask;

        struct Stub;
        impl Dispatcher for Stub {
            fn dispatch(&self, cli: &AgenticCli, _task: &CouncilTask) -> Option<Vote> {
                Some(Vote {
                    cli: cli.key.clone(),
                    recommendation: "fake-a".into(),
                    top_risk: "none".into(),
                    change_my_mind: "no".into(),
                    disqualifier: None,
                    confidence: Confidence::default(),
                    provenance: "stub".into(),
                })
            }
        }
        let cli = |key: &str| AgenticCli {
            key: key.into(),
            display_name: key.into(),
            binary: "unused".into(),
            headless_invocation: "unused {PROMPT}".into(),
            category: Category::default(),
            input_mode: InputMode::default(),
            version_probe: vec![],
            trust_flags: vec![],
            alt_binaries: vec![],
            confidence: Confidence::default(),
            enabled_for_council: true,
        };

        let dir = std::env::temp_dir().join("wicked-core-pipeline-test");
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("pipeline.db");
        let _ = std::fs::remove_file(&db);
        let mut store = wicked_apps_core::open_store(Some(db.to_str().unwrap())).unwrap();

        let mut events: Vec<CoreEvent> = Vec::new();
        let result = crate::pipeline::run_session(
            &mut store,
            vec![cli("fake-a"), cli("fake-b")],
            "Do step one. Do step two",
            EntityMode::Shared,
            "test-pipeline",
            Arc::new(Stub),
            &mut |ev| events.push(ev),
        )
        .expect("run_session");

        // Composition result.
        assert_eq!(result.units.len(), 2);
        assert_eq!(result.approved, 2, "no deny policy ⇒ both approve");
        assert_eq!(result.rejected, 0);

        // Live event sequence — emitted in order, bookended by Started/Completed.
        let n = |pred: fn(&CoreEvent) -> bool| events.iter().filter(|e| pred(e)).count();
        assert_eq!(n(|e| matches!(e, CoreEvent::SessionStarted { .. })), 1);
        assert_eq!(n(|e| matches!(e, CoreEvent::UnitPlanned { .. })), 2);
        assert_eq!(n(|e| matches!(e, CoreEvent::UnitDistributed { .. })), 2);
        assert_eq!(n(|e| matches!(e, CoreEvent::GateDecided { .. })), 2);
        assert_eq!(n(|e| matches!(e, CoreEvent::UnitDone { .. })), 2);
        assert!(matches!(
            events.first(),
            Some(CoreEvent::SessionStarted { .. })
        ));
        assert!(matches!(
            events.last(),
            Some(CoreEvent::SessionCompleted { .. })
        ));

        // Persisted + readable through the same domain the read API serves.
        let units = session_units(&store, "test-pipeline").unwrap();
        assert_eq!(units.len(), 2);
        assert!(units.iter().all(|u| u.status == UnitStatus::Done));
        let out = get_work_output(&store, "test-pipeline:u1").expect("unit 1 output");
        assert!(out.contains("stub-output"), "transcript: {out}");
    }
}
