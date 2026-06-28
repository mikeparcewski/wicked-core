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
mod event;

pub use event::CoreEvent;

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

    /// The agent session ids currently on the store (P1 read path; richer `ProjectView`s in P2).
    pub fn sessions(&self) -> anyhow::Result<Vec<String>> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::Sessions(reply))
            .map_err(|_| anyhow::anyhow!("core actor stopped"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("core actor dropped the reply"))?
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
}
