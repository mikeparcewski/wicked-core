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
//! `listExecuting`), `Core::session_detail`, `Core::run_team` and (core#1795) `Core::gate_actions`,
//! the gate card's list of answers. Transcripts stay on the actor: their store helpers fold a
//! read error into "no record", which this path could not tell from a real answer.
//!
//! Each read takes a connection of its own from a small pool (core#1795), so a slow read, or one
//! waiting for a quiet moment, never holds another read up.
//!
//! What it is NOT: an input to any decision. Gates, reassigns, delivery, the hook-decision fold and
//! every deny are decided on the actor from the actor's own store. `gate_actions` lists the answers
//! a gate takes by running the decision path's own pre-checks on this read; the answer itself is
//! checked again on the actor when it arrives. A read here may show a state a
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

/// (core#1795) How many idle read connections a core keeps. Each read takes a connection of its
/// own (an idle one, or a new one), so reads never queue behind each other: a slow read (a gate
/// read's worktree snapshot) or one waiting for a quiet moment holds only its own connection.
/// Past this many idle connections, a finished read's connection is closed.
const IDLE_CONNECTIONS: usize = 4;

pub(crate) struct ReadModel {
    /// The store file, or `None` when this store has no read model (in-memory, Postgres).
    path: Option<String>,
    /// Idle read-only connections ([`IDLE_CONNECTIONS`] at most). The lock is held only to take
    /// or return one, never across a read.
    idle: Mutex<Vec<SqliteStore>>,
    /// The actor's command counters: where "between commands" is read from.
    actor: Arc<crate::actor_stats::ActorStats>,
    /// How long a read waits for a between-commands moment ([`QUIET_BUDGET`]; tests widen it so a
    /// loaded host cannot turn an interleaving test into a budget test).
    budget: Duration,
    /// TEST-ONLY: runs once, between a read's quiet moment and its snapshot pin.
    #[cfg(test)]
    after_quiet: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// TEST-ONLY: runs once, right after a read pins its snapshot.
    #[cfg(test)]
    after_pin: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// TEST-ONLY: snapshots given up because a command began before the pin.
    #[cfg(test)]
    retries: std::sync::atomic::AtomicUsize,
}

/// Whether a store spec can be read through a read-only SQLite connection.
fn readable_spec(spec: &str) -> bool {
    // A SQLite `file:` URI keeps its own semantics (a shared in-memory db, query parameters): the
    // read-only opener takes a plain path, so those stores are read through the actor.
    !(spec.is_empty()
        || spec == ":memory:"
        || spec.starts_with("file:")
        || spec.starts_with("postgres://")
        || spec.starts_with("postgresql://"))
}

/// The store spec a core runs on, resolved once at spawn: a SQLite file path is made absolute, so the
/// actor (which opens it on its own thread, a moment later) and the read model open the same file
/// whatever the process's working directory does afterwards. In-memory and Postgres specs are
/// returned as given.
pub(crate) fn resolve_spec(spec: String) -> String {
    if !readable_spec(&spec) {
        return spec;
    }
    // A path that does not resolve to UTF-8 (a non-UTF-8 working directory) is kept as given, so
    // the actor opens what it always did; the read model then reads that same spec.
    std::path::absolute(&spec)
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or(spec)
}

/// The path the read model opens for `spec`: an ABSOLUTE SQLite file path only. A path that could
/// not be resolved (a non-UTF-8 working directory) would follow the process's working directory and
/// could open another store, so such a store is read through the actor.
fn read_path(spec: &str) -> Option<String> {
    if !readable_spec(spec) {
        return None;
    }
    let resolved = resolve_spec(spec.to_string());
    std::path::Path::new(&resolved)
        .is_absolute()
        .then_some(resolved)
}

impl ReadModel {
    /// The read model for the store the actor opens at `spec`.
    pub(crate) fn for_store(spec: &str, actor: Arc<crate::actor_stats::ActorStats>) -> Self {
        let off =
            std::env::var("WICKED_READ_MODEL").is_ok_and(|v| v.trim().eq_ignore_ascii_case("off"));
        Self {
            path: (!off).then(|| read_path(spec)).flatten(),
            idle: Mutex::new(Vec::new()),
            actor,
            budget: QUIET_BUDGET,
            #[cfg(test)]
            after_quiet: Mutex::new(None),
            #[cfg(test)]
            after_pin: Mutex::new(None),
            #[cfg(test)]
            retries: Default::default(),
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
        // A stopped actor answers every read with its own error ("core actor stopped"), as before;
        // the connections go with it, so nothing of this core holds the store open after it.
        if self.actor.is_stopped() {
            self.close_idle();
            return None;
        }
        // (core#1795) This read's own connection: the pool's lock is released before the read
        // waits or reads, so concurrent reads run side by side. A connection that fails is
        // dropped (never returned), and the next read opens a fresh one.
        let taken = self.idle.lock().unwrap_or_else(|p| p.into_inner()).pop();
        let mut store = match taken {
            Some(store) => store,
            None => wicked_apps_core::open_store_ro(Some(path)).ok()?,
        };
        let out = self.read_on(&mut store, f)?;
        if self.actor.is_stopped() {
            self.close_idle();
            return None;
        }
        let mut idle = self.idle.lock().unwrap_or_else(|p| p.into_inner());
        if idle.len() < IDLE_CONNECTIONS {
            idle.push(store);
        }
        Some(out)
    }

    /// Close every idle connection (the actor stopped).
    fn close_idle(&self) {
        self.idle.lock().unwrap_or_else(|p| p.into_inner()).clear();
    }

    /// [`ReadModel::read`] on one connection: `None` when the read must go to the actor, and the
    /// connection is then not reused.
    fn read_on<T>(
        &self,
        store: &mut SqliteStore,
        f: impl FnOnce(&SqliteStore) -> anyhow::Result<T>,
    ) -> Option<T> {
        let deadline = Instant::now() + self.budget;
        loop {
            let quiet = self
                .actor
                .between_commands(deadline.saturating_duration_since(Instant::now()));
            #[cfg(test)]
            if let Some(hook) = self.after_quiet.lock().unwrap().take() {
                hook();
            }
            // `begin_batch`, then one SELECT: the snapshot everything `f` reads is pinned here.
            // Estate 0.35 begins `IMMEDIATE` (estate#532); on this read-only connection to a WAL
            // store that is still a read transaction and takes no write lock, so the actor keeps
            // writing (pinned by `one_read_sees_one_committed_state_even_when_a_write_lands_midway`).
            if store.begin_batch().is_err() || store.meta_get("history_enabled").is_err() {
                let _ = store.commit_batch();
                return None;
            }
            #[cfg(test)]
            if let Some(hook) = self.after_pin.lock().unwrap().take() {
                hook();
            }
            // An actor that took `Shutdown` while this read waited answers it itself (stopped).
            if self.actor.is_stopped() {
                let _ = store.commit_batch();
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
            #[cfg(test)]
            self.retries
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if store.commit_batch().is_err() {
                return None;
            }
        }
        let out = f(store);
        // `COMMIT` ends the read transaction; a connection that cannot end it is not reused, and
        // what it read is not trusted either: the actor answers instead.
        let ended = store.commit_batch().is_ok();
        if !ended || out.is_err() || self.actor.is_stopped() {
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
        assert!(
            read_path("relative/core.db").is_some_and(|p| std::path::Path::new(&p).is_absolute())
        );
        assert_eq!(read_path(":memory:"), None);
        assert!(!readable_spec("file:review?mode=memory&cache=shared"));
        assert!(!readable_spec("file:/state/core.db"));
        assert_eq!(
            resolve_spec("file:review?mode=memory&cache=shared".into()),
            "file:review?mode=memory&cache=shared",
            "a SQLite URI is handed to the actor untouched"
        );
        assert!(!readable_spec(":memory:"));
        assert!(!readable_spec(""));
        assert!(!readable_spec("postgres://h/db"));
        assert!(!readable_spec("postgresql://h/db"));
        assert!(!ReadModel::for_store(":memory:", Default::default()).applies());
        let rel = ReadModel::for_store("relative/core.db", Default::default());
        assert!(
            rel.path
                .as_deref()
                .is_some_and(|p| std::path::Path::new(p).is_absolute()),
            "a relative store is resolved at spawn: {:?}",
            rel.path
        );
    }

    #[test]
    fn a_command_that_begins_before_the_pin_is_waited_out_and_its_whole_write_read() {
        use std::sync::mpsc::channel;
        use wicked_apps_core::ToNode;
        let dir = std::env::temp_dir().join(format!(
            "wicked-read-model-gen-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("core.db");
        let path = db.to_str().unwrap().to_string();
        let session = crate::actor::sessions_detail_fold_tests::session;
        {
            let mut w = wicked_apps_core::open_store(Some(&path)).unwrap();
            crate::domain::put_nodes(&mut w, &[session("run-a").to_node()]).unwrap();
        }
        let actor = Arc::new(crate::actor_stats::ActorStats::starting());
        actor.started_up();
        let mut rm = ReadModel::for_store(&path, actor.clone());
        rm.budget = Duration::from_secs(30);
        // The schedule, forced step by step: the read sees the actor quiet; THEN a command begins
        // and commits half its write (run-b); the read pins on that; THEN the command commits the
        // rest (run-c) and ends. Every wait is bounded, so a broken read fails rather than hangs.
        let bound = Duration::from_secs(20);
        let (go_tx, go) = channel::<()>();
        let (half_tx, half) = channel::<()>();
        let (pinned_tx, pinned) = channel::<()>();
        let (a2, p2) = (actor.clone(), path.clone());
        let writer = std::thread::spawn(move || {
            let mut w = wicked_apps_core::open_store(Some(&p2)).unwrap();
            go.recv_timeout(bound).expect("go");
            let _cmd = crate::actor_stats::Timed::start(&a2, "Test", 0, Duration::from_secs(3600));
            crate::domain::put_nodes(&mut w, &[session("run-b").to_node()]).unwrap();
            half_tx.send(()).unwrap();
            let _ = pinned.recv_timeout(bound);
            crate::domain::put_nodes(&mut w, &[session("run-c").to_node()]).unwrap();
        });
        *rm.after_quiet.lock().unwrap() = Some(Box::new(move || {
            go_tx.send(()).unwrap();
            half.recv_timeout(bound).expect("half written");
        }));
        *rm.after_pin.lock().unwrap() = Some(Box::new(move || {
            let _ = pinned_tx.send(());
        }));
        let n = rm
            .read(|s| Ok(crate::domain::all_sessions(s)?.len()))
            .expect("read");
        writer.join().unwrap();
        assert_eq!(
            rm.retries.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the snapshot pinned mid-command was given up"
        );
        assert_eq!(
            n, 3,
            "never the half-written state: the read waited the command out"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (core#1795) A slow read holds only its own connection: a second read answers while the
    /// first is still inside its read transaction, and both connections stay for the next reads.
    #[test]
    fn a_slow_read_does_not_hold_another_read_up() {
        use std::sync::mpsc::channel;
        let dir = std::env::temp_dir().join(format!(
            "wicked-read-model-pool-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("core.db");
        drop(wicked_apps_core::open_store(Some(db.to_str().unwrap())).unwrap());
        let actor = Arc::new(crate::actor_stats::ActorStats::starting());
        actor.started_up();
        let rm = Arc::new(ReadModel::for_store(db.to_str().unwrap(), actor));
        let bound = Duration::from_secs(20);
        let (inside_tx, inside) = channel::<()>();
        let (release_tx, release) = channel::<()>();
        let slow = {
            let rm = rm.clone();
            std::thread::spawn(move || {
                rm.read(|_| {
                    inside_tx.send(()).unwrap();
                    release.recv_timeout(bound).expect("released");
                    Ok("slow")
                })
            })
        };
        inside
            .recv_timeout(bound)
            .expect("the slow read is inside its read");
        let (fast_tx, fast) = channel();
        {
            let rm = rm.clone();
            std::thread::spawn(move || {
                let _ = fast_tx.send(rm.read(|_| Ok("fast")));
            });
        }
        let got = fast.recv_timeout(Duration::from_secs(5));
        release_tx.send(()).unwrap();
        assert_eq!(
            got.expect("the second read answered while the first was still reading"),
            Some("fast")
        );
        assert_eq!(slow.join().unwrap(), Some("slow"));
        assert_eq!(
            rm.idle.lock().unwrap().len(),
            2,
            "both connections are kept"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_actor_that_stops_while_a_read_waits_answers_it_itself() {
        let dir = std::env::temp_dir().join(format!(
            "wicked-read-model-stop-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("core.db");
        drop(wicked_apps_core::open_store(Some(db.to_str().unwrap())).unwrap());
        let actor = Arc::new(crate::actor_stats::ActorStats::starting());
        actor.started_up();
        let rm = ReadModel::for_store(db.to_str().unwrap(), actor.clone());
        assert_eq!(rm.read(|_| Ok(1)), Some(1));
        // The actor takes Shutdown while the read runs: the read defers to the actor.
        assert_eq!(
            rm.read(|_| {
                actor.mark_stopped();
                Ok(2)
            }),
            None
        );
        assert_eq!(rm.read(|_| Ok(3)), None, "and every read after it");
        let _ = std::fs::remove_dir_all(&dir);
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
