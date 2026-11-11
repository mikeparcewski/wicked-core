//! (core#1390) The engine's READ model: run reads answered from a read-only SQLite connection on
//! the caller's thread, never queued behind the single-writer actor.
//!
//! Why: the actor runs one command at a time. A read waiting behind a long command (a worktree
//! snapshot on a large tree took 30–45 s on the dev host) answered in tens of seconds although it
//! costs about 30 ms. A read-only connection rides the store's WAL: every read below runs inside one
//! read transaction, so it sees one committed state of the store — never a half-written row, and
//! never a state older than the last write the actor committed before the read began. A command's
//! reply is sent after its writes commit, so a caller that wrote through the actor reads its write.
//!
//! Between commands, where it can: a read first waits (up to [`QUIET_BUDGET`]) for a moment
//! when the actor is between two commands, pins its snapshot there, and checks no command began
//! before the pin — so a read issued after a command's event sees that whole command, as a read
//! queued behind it on the actor did. Only when the actor stays busy past the budget (a long
//! command) does the read go ahead on the last committed state, which may then be one the running
//! command is midway through.
//!
//! Served here: `Core::sessions`, `Core::sessions_detail` (crew's GET /runs, the stall watchdog's
//! `listExecuting`) and `Core::run_team`. Transcripts stay on the actor: their store helpers fold a
//! read error into "no record", which this path could not tell from a real answer.
//!
//! What it is NOT: an input to any decision. Gates, reassigns, delivery, the hook-decision fold and
//! every deny are decided on the actor from the actor's own store. A read here may show a state a
//! command is midway through (it committed the session, not yet the units); the actor re-validates
//! every answer sent on such a read (`check_gate_expect`, the reassign's cursor check), exactly as
//! it refuses an answer sent on a read that is a second old. The guard test
//! `read_model_serves_reads_only` pins which `Core` methods may use it.
//!
//! `WICKED_READ_MODEL=off` (read when a `Core` spawns) turns it off: every read then goes through
//! the actor, as before core#1390 — the operator's way back, and the load harness's baseline.
//!
//! When it does not apply — an in-memory store, a `postgres://` store (the read-only opener is
//! SQLite-only, core#30), a store file the actor has not created yet, or any error on the read
//! connection — the read goes through the actor exactly as before, and a failed connection is
//! reopened on the next read.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use wicked_apps_core::{GraphWrite, SqliteStore};

/// The longest a read waits for the actor to be between commands before reading the last
/// committed state anyway.
pub(crate) const QUIET_BUDGET: Duration = Duration::from_millis(100);

pub(crate) struct ReadModel {
    /// The store file, or `None` when this store has no read model (in-memory, Postgres).
    path: Option<String>,
    conn: Mutex<Option<SqliteStore>>,
    /// The actor's command counters: where "between commands" is read from.
    actor: Arc<crate::actor_stats::ActorStats>,
}

/// Whether a store spec can be read through a read-only SQLite connection.
fn readable_spec(spec: &str) -> bool {
    !(spec.is_empty()
        || spec == ":memory:"
        || spec.starts_with("postgres://")
        || spec.starts_with("postgresql://"))
}

impl ReadModel {
    /// The read model for the store the actor opens at `spec`.
    pub(crate) fn for_store(spec: &str, actor: Arc<crate::actor_stats::ActorStats>) -> Self {
        let off =
            std::env::var("WICKED_READ_MODEL").is_ok_and(|v| v.trim().eq_ignore_ascii_case("off"));
        Self {
            path: (!off && readable_spec(spec)).then(|| spec.to_string()),
            conn: Mutex::new(None),
            actor,
        }
    }

    /// Whether reads can be served here at all (false: every read goes through the actor).
    #[cfg(test)]
    pub(crate) fn applies(&self) -> bool {
        self.path.is_some()
    }

    /// Run `f` inside one read transaction on the read-only connection. `None` means "ask the
    /// actor": this store has no read model, the connection could not be opened, or the read
    /// failed (the connection is then dropped and reopened next time).
    pub(crate) fn read<T>(&self, f: impl FnOnce(&SqliteStore) -> anyhow::Result<T>) -> Option<T> {
        let path = self.path.as_deref()?;
        let mut slot = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        // A stopped actor answers every read with its own error ("core actor stopped"), as before;
        // the connection goes with it, so nothing of this core holds the store open after it.
        if self.actor.is_stopped() {
            *slot = None;
            return None;
        }
        if slot.is_none() {
            *slot = wicked_apps_core::open_store_ro(Some(path)).ok();
        }
        let store = slot.as_mut()?;
        let deadline = Instant::now() + QUIET_BUDGET;
        loop {
            let quiet = self
                .actor
                .between_commands(deadline.saturating_duration_since(Instant::now()));
            // `BEGIN` (deferred), then one SELECT: the snapshot everything `f` reads is pinned here.
            if store.begin_batch().is_err() || store.meta_get("history_enabled").is_err() {
                let _ = store.commit_batch();
                *slot = None;
                return None;
            }
            match quiet {
                // Pinned while no command had begun since the quiet moment: a between-commands
                // state. Or the budget ran out: the last committed state, never another try.
                Some(generation)
                    if self.actor.generation() != generation && Instant::now() < deadline => {}
                _ => break,
            }
            // A command began before the pin: let it finish and pin again (within the budget).
            if store.commit_batch().is_err() {
                *slot = None;
                return None;
            }
        }
        let out = f(store);
        // `COMMIT` ends the read transaction; a connection that cannot end it is not reused, and
        // what it read is not trusted either: the actor answers instead.
        let ended = store.commit_batch().is_ok();
        if !ended || out.is_err() {
            *slot = None;
            return None;
        }
        out.ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_sqlite_file_has_a_read_model() {
        assert!(readable_spec("/state/core.db"));
        assert!(readable_spec("relative/core.db"));
        assert!(!readable_spec(":memory:"));
        assert!(!readable_spec(""));
        assert!(!readable_spec("postgres://h/db"));
        assert!(!readable_spec("postgresql://h/db"));
        assert!(!ReadModel::for_store(":memory:", Default::default()).applies());
    }

    #[test]
    fn one_read_sees_one_committed_state_even_when_a_write_lands_midway() {
        use wicked_apps_core::{GraphStore, ToNode};
        let dir = std::env::temp_dir().join(format!(
            "wicked-read-model-snap-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("core.db");
        let mut writer = wicked_apps_core::open_store(Some(db.to_str().unwrap())).unwrap();
        let session = crate::actor::sessions_detail_fold_tests::session;
        crate::domain::put_nodes(&mut writer, &[session("run-a").to_node()]).unwrap();
        let actor = Arc::new(crate::actor_stats::ActorStats::starting());
        actor.started_up();
        let rm = ReadModel::for_store(db.to_str().unwrap(), actor);
        let (before, after) = rm
            .read(|s| {
                let before = crate::domain::all_sessions(s)?.len();
                // A writer commits between the two reads of one fold.
                let w: &mut dyn GraphStore = &mut writer;
                crate::domain::put_nodes(w, &[session("run-b").to_node()])?;
                let after = crate::domain::all_sessions(s)?.len();
                Ok((before, after))
            })
            .expect("read");
        assert_eq!((before, after), (1, 1), "the read's snapshot does not move");
        assert_eq!(
            rm.read(|s| Ok(crate::domain::all_sessions(s)?.len())),
            Some(2),
            "the next read sees the commit"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_store_with_no_file_yet_defers_to_the_actor_and_is_read_once_it_exists() {
        let dir = std::env::temp_dir().join(format!(
            "wicked-read-model-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("core.db");
        let rm = ReadModel::for_store(db.to_str().unwrap(), Default::default());
        assert!(rm.applies());
        assert_eq!(rm.read(|_| Ok(1)), None, "no file yet: the actor answers");
        drop(wicked_apps_core::open_store(Some(db.to_str().unwrap())).unwrap());
        assert_eq!(rm.read(|_| Ok(2)), Some(2), "reopened once the file exists");
        assert_eq!(
            rm.read(|_| -> anyhow::Result<i32> { anyhow::bail!("x") }),
            None,
            "a failed read defers to the actor"
        );
        assert_eq!(
            rm.read(|_| Ok(3)),
            Some(3),
            "and the connection is reopened"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
