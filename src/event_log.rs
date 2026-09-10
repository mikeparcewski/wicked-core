//! DURABLE PER-RUN EVENT LOG (FINDING-014) — the audit trail behind a run's evidence packet.
//!
//! Before this, [`CoreEvent`]s were fanned out to live subscribers and then DROPPED. A run's event
//! history existed only for whoever happened to be holding a socket at the time, so an evidence
//! bundle assembled after the fact had nothing to read: the daemon re-DERIVED a couple of pseudo-event
//! types from unit records and shipped those instead, which is how a 49-event run exported as "6
//! events" of two invented types. Evidence that only exists if a client was watching is not evidence.
//!
//! ## Shape
//!
//! One append-only NDJSON file per run, at [`run_log_path`]. Each line is the event's own tagged JSON
//! ([`CoreEvent::to_json`] — the SAME object the `/ws` stream carries, so the log and the socket cannot
//! drift) plus two envelope fields this module owns:
//!
//! - `ts` — capture-time epoch millis. Nothing in the domain model carries a time value (`WorkUnit`,
//!   `AgentSession` and `Node` are all timeless), so ordering and duration were previously
//!   unrecoverable after the fact. Stamped here, at the single emit point, rather than inferred later.
//! - `seq` — a monotonic counter. Millisecond stamps collide freely (a burst of events from one actor
//!   turn shares a millisecond), so `ts` alone cannot order a run. `seq` is the tiebreak and makes the
//!   total order recoverable. It is strictly increasing within a run for the run's WHOLE life — across
//!   daemon restarts, not just within one process (core#408): the counter is process-wide, but the
//!   first record a process writes for a run continues from the largest `seq` already in that run's
//!   log. Before that seed the counter restarted at 0 with the daemon, so a resumed run's new events
//!   sorted BEFORE its old ones and every "latest event" consumer (the studio now-bar, crew's relays,
//!   the acceptance poller) read the pre-restart tail as current — at exactly the moment an operator
//!   most needed the truth.
//! - `daemonRestarted: true` — on ONE record per restart: the first one a fresh engine writes for a
//!   run that already had history. A consumer keeping per-run state across the gap can see the
//!   boundary rather than infer it from a `seq` jump. Absent everywhere else (never `false`).
//!
//! All three are envelope fields of the durable log only. The live `/ws` frame is the bare
//! [`CoreEvent::to_json`] object and carries none of them.
//!
//! ## Where it lives: beside the store, not in a global directory
//!
//! The root is [`log_root`] — `<store-path>.events/`, the same sidecar convention the actor already
//! uses for `<store>.mem` and `<store>.knowledge`. A run's evidence belongs next to the store that
//! holds the run.
//!
//! This is deliberately NOT a process-global path such as `~/.wicked/runs`. Anchoring to the store
//! means a `Core` opened against a scratch database keeps its logs in that scratch directory, so two
//! `Core`s cannot interleave into one tree and `cargo test` cannot deposit run logs in a developer's
//! home directory. (It did, before this: a full suite run left 76 logs and 1.6 MB in `~/.wicked`, and
//! the resulting contention was measurable — see the note on [`is_high_volume`].) It is also NOT the
//! governance root, which a fresh re-launch of a run id deliberately wipes and the OS clears; an audit
//! trail has to outlive both.
//!
//! ## Why a file and not the store
//!
//! Core's store is single-writer by design: the actor thread owns it, which is what keeps SQLite free
//! of races. The emit point cannot borrow it — call sites already pass `&mut store` and the emit sink
//! as separate arguments to the same call, so a sink that captured the store would not borrow-check.
//! An independently-owned file handle sidesteps that entirely, and the codebase already has the
//! precedent: the gate-hook decisions log ([`crate::gate_hook`]) is append-only NDJSON written by
//! out-of-process hooks with no store handle at all.
//!
//! wicked-bus is deliberately NOT the home for this despite being the ecosystem's event substrate. It
//! is a DELIVERY fabric with TTLs — an audit trail has to outlive delivery, so it belongs next to the
//! run it documents.
//!
//! ## What is not logged
//!
//! Streaming variants (`cliOutputDelta`, `chatDelta`, `terminalOutput`) and `heartbeat` are skipped —
//! see [`is_high_volume`]. They are chunk-level transport, not run history; their content is already
//! persisted as captured work output. This is a deliberate, named exclusion rather than a silent one:
//! every other variant is recorded, so a missing event in the log means a missing event, not a filter.
//!
//! ## Retention — this grows without bound, on purpose
//!
//! Nothing here prunes. A run's log is on the order of 20–80 KB (roughly 50–200 records with the
//! streaming variants excluded), so a host that has executed ten thousand runs holds a few hundred MB.
//! That is real growth and it is stated here rather than discovered later.
//!
//! Deleting it is an OPERATOR action, and the alternatives are worse. A per-run size cap would silently
//! truncate a long run's evidence — turning a complete record into a partial one with no marker, which
//! is the same class of defect as the re-derivation this replaces. A global age-based sweep would
//! delete the audit trail of exactly the old runs an audit is most likely to ask about. If retention
//! policy is wanted it belongs above this module, as an explicit operator-facing prune with its own
//! record of what it removed — not as an implicit default that quietly loses evidence.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::event::CoreEvent;

/// Process-wide monotonic sequence. Shared across runs on purpose: it makes the interleaving of
/// concurrent runs recoverable too, which a per-run counter would lose.
///
/// Starts at 0 with the process and is raised (`fetch_max`) past a run's persisted history the first
/// time this process records for that run — see [`persisted_max_seq`] and the module docs. Raising
/// rather than assigning keeps it monotonic for every OTHER run already in flight: a seed can only
/// move the counter forward.
static SEQ: AtomicU64 = AtomicU64::new(0);

/// Path-escape a run id injectively so it can name a file. Same scheme as [`crate::gate_hook`]'s:
/// alphanumerics and `-` survive, everything else becomes `_<hex>`. Injective, so two run ids can
/// never collide onto one log, and `..` / `/` cannot escape the root.
fn encode_run_id(run_id: &str) -> String {
    let enc: String = run_id
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b == b'-' {
                (b as char).to_string()
            } else {
                format!("_{b:02x}")
            }
        })
        .collect();
    if enc.is_empty() {
        "_empty".to_string()
    } else {
        enc
    }
}

/// The log directory for a core whose store is at `store_path` — `<store_path>.events`.
///
/// Matches the actor's existing sidecar convention (`<store>.mem`, `<store>.knowledge`). Anchoring to
/// the store rather than a global directory is what makes each `Core` self-contained; see the module
/// docs for why that matters.
pub fn log_root(store_path: &str) -> PathBuf {
    PathBuf::from(format!("{store_path}.events"))
}

/// `<root>/<encoded-run-id>.ndjson`.
pub fn run_log_path(root: &Path, run_id: &str) -> PathBuf {
    root.join(format!("{}.ndjson", encode_run_id(run_id)))
}

/// Chunk-level streaming events, excluded from the log (see the module docs).
///
/// Matched on the ENUM, before any serialization. These variants are the overwhelming majority of all
/// emissions — a CLI's stdout arrives as thousands of `CliOutputDelta` chunks per run — so encoding one
/// to JSON just to read its `type` and throw it away is work done on the single-writer actor thread,
/// the thread that drives every run in the process. Doing exactly that was a measured regression: a
/// governed-run test blew its 8s budget and its binary went 20s → 47s. Deciding on the variant costs a
/// discriminant compare.
///
/// [`high_volume_type_names`] holds the same set as the `type` strings the log stores, and
/// `the_two_spellings_of_the_exclusion_set_agree` pins them together.
fn is_high_volume(ev: &CoreEvent) -> bool {
    matches!(
        ev,
        CoreEvent::CliOutputDelta { .. }
            | CoreEvent::ChatDelta { .. }
            | CoreEvent::TerminalOutput { .. }
            | CoreEvent::Heartbeat
    )
}

/// The exclusion set spelled as the tagged `type` names that appear in the log — the vocabulary a
/// reader of the NDJSON sees, and what the docs and tests talk in.
#[cfg(test)]
const fn high_volume_type_names() -> [&'static str; 4] {
    ["cliOutputDelta", "chatDelta", "terminalOutput", "heartbeat"]
}

/// The run a tagged event belongs to, read out of the JSON rather than re-matched per variant.
///
/// Most variants carry `session`; campaign node events carry the node's run as `runId`. Deriving
/// the key from the emitted object means a NEW variant that follows the convention is logged
/// automatically — a second hand-written 125-arm match would be a second thing to forget.
/// `None` ⇒ not run-scoped (chat, terminal, campaign-level), so not part of any run's evidence.
///
/// The key is `runId`, not `run_id`, because the emitted JSON is camelCase throughout. An earlier
/// cut of this function looked for `run_id` — a key `to_json` emits nowhere — so both campaign node
/// events were dropped from every run's history while the code read as though it handled them.
/// `campaign_node_events_are_routed_to_their_run` pins the live spelling.
pub fn run_key(json: &serde_json::Value) -> Option<&str> {
    json.get("session")
        .and_then(|v| v.as_str())
        .or_else(|| json.get("runId").and_then(|v| v.as_str()))
}

/// One unit of work for the writer thread: a fully-resolved destination and the exact line to append.
///
/// The PATH is resolved on the calling thread, not the writer's, so a single shared writer can serve
/// any number of `Core`s with different log roots.
enum LogMsg {
    Record {
        path: PathBuf,
        line: String,
    },
    /// Round-trip barrier: everything queued before this has been written when the reply lands.
    Flush(std::sync::mpsc::Sender<()>),
}

/// Handle to the single background writer thread, started on first use.
static WRITER: std::sync::OnceLock<std::sync::mpsc::Sender<LogMsg>> = std::sync::OnceLock::new();

/// How many per-run file handles the writer keeps open before dropping the cache. Bounded because a
/// daemon outlives thousands of runs; dropped wholesale rather than LRU-evicted because reopening is
/// cheap and the access pattern (a handful of concurrent runs) makes a precise policy pointless.
const HANDLE_CACHE_CAP: usize = 64;

/// The writer thread's sender, starting it on first use.
///
/// Why a thread at all: `emit` runs on the SINGLE-WRITER ACTOR, the thread that owns the store and
/// drives every run in the process. Doing `create_dir_all` + `open` + `write` + `close` inline there
/// put filesystem latency on the critical path of every event and serialized all runs behind it — a
/// measured regression (one governed-run test's 8s budget started blowing) and not a cost an audit
/// trail is allowed to impose. Handing the line to a channel keeps the actor's work to a JSON encode
/// and a send.
///
/// Ordering survives the move: `ts` and `seq` are stamped on the ACTOR thread before the send, and the
/// channel is FIFO, so the file's order is the emission order.
fn writer() -> &'static std::sync::mpsc::Sender<LogMsg> {
    WRITER.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<LogMsg>();
        std::thread::Builder::new()
            .name("wicked-event-log".into())
            .spawn(move || {
                // `None` = this path was tried and could not be opened. Remembering the FAILURE
                // matters as much as remembering the handle: a log root that is unwritable (read-only
                // volume, wrong ownership, sandbox) is unwritable for the whole run, and without this
                // every event pays a fresh `create_dir_all` + `open` that is going to fail again.
                let mut open: std::collections::HashMap<PathBuf, Option<std::fs::File>> =
                    std::collections::HashMap::new();
                while let Ok(msg) = rx.recv() {
                    match msg {
                        LogMsg::Flush(reply) => {
                            let _ = reply.send(());
                        }
                        LogMsg::Record { path, line } => {
                            if open.len() >= HANDLE_CACHE_CAP {
                                open.clear();
                            }
                            let slot = match open.entry(path.clone()) {
                                std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                                std::collections::hash_map::Entry::Vacant(e) => {
                                    // Best-effort: an unwritable log costs the record, never the run.
                                    let opened = path.parent().and_then(|parent| {
                                        std::fs::create_dir_all(parent).ok()?;
                                        std::fs::OpenOptions::new()
                                            .create(true)
                                            .append(true)
                                            .open(&path)
                                            .ok()
                                    });
                                    e.insert(opened)
                                }
                            };
                            if let Some(f) = slot {
                                // The whole `line + '\n'` in ONE `write_all`: a lone small append is
                                // atomic on POSIX (`O_APPEND`) and Windows (`FILE_APPEND_DATA`), so no
                                // concurrent writer can interleave a partial line.
                                let _ = f.write_all(line.as_bytes());
                            }
                        }
                    }
                }
            })
            .expect("spawn event-log writer");
        tx
    })
}

/// Block until every record queued so far has hit disk. Called before a read so a caller cannot
/// observe a history that is missing events it just emitted.
pub fn flush() {
    let (tx, rx) = std::sync::mpsc::channel();
    if writer().send(LogMsg::Flush(tx)).is_ok() {
        let _ = rx.recv_timeout(std::time::Duration::from_secs(5));
    }
}

/// The envelope `seq` of a recorded line, if it carries one.
fn seq_of(v: &serde_json::Value) -> Option<u64> {
    v.get("seq").and_then(|s| s.as_u64())
}

/// Every parseable record in a raw NDJSON log, in file order. Unparseable lines are skipped rather
/// than failing the read: a torn final line from a crash mid-append must not make the preceding
/// history unreadable — and must not stop a restarted engine from seeding its counter off it.
fn parse_log(raw: &str) -> Vec<serde_json::Value> {
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// What a fresh engine finds in a run's log before writing its first record for that run.
struct PriorHistory {
    /// The largest `seq` already recorded — where the counter must continue from. `None` when the
    /// log is absent or holds no parseable record, i.e. the run has no history to continue.
    max_seq: Option<u64>,
    /// The file ends mid-line: a crash mid-append left a fragment with no terminating newline.
    torn_tail: bool,
}

/// A run's log as text, tolerant of a torn multi-byte character.
///
/// A crash mid-append can cut a UTF-8 sequence in the final line, leaving the file invalid UTF-8.
/// `read_to_string` refuses the WHOLE file for that one byte — a complete history would read as
/// empty, and a restarted engine, finding "no history", would seed nothing and restart `seq` at 0:
/// the very failure this module fixes, back through a side door. Lossy decoding confines the damage
/// to the torn line, which is unparseable either way and already skipped. `None` ⇒ no log.
fn read_log(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Inspect a run's log once, on the first record this engine writes for it.
///
/// Reads the whole file rather than seeking to its tail: a torn trailing line (crash mid-append) or
/// a log a pre-#408 engine wrote across a restart (a second `seq` run starting at 0 AFTER the higher
/// values) both defeat "the last line is the max". It runs once per run per engine lifetime — a
/// per-run file of tens of KB — not once per event, so the cost stays off the emission path.
fn inspect_log(path: &Path) -> PriorHistory {
    let Some(raw) = read_log(path) else {
        return PriorHistory {
            max_seq: None,
            torn_tail: false,
        };
    };
    PriorHistory {
        max_seq: parse_log(&raw).iter().filter_map(seq_of).max(),
        torn_tail: !raw.is_empty() && !raw.ends_with('\n'),
    }
}

/// The largest `seq` already recorded in the log at `path` — where a fresh engine's counter must
/// continue from for that run. `None` when the log is absent or holds no parseable record.
pub fn persisted_max_seq(path: &Path) -> Option<u64> {
    inspect_log(path).max_seq
}

/// Queue one event for its run's log under `root`. Returns whether a record was ENQUEUED — the write
/// itself happens on the writer thread, so this is not a durability acknowledgement (use [`flush`]).
///
/// `continued` is the set of runs this engine has already recorded for. The FIRST record for a run
/// is where the persisted history is consulted: the process-wide counter is raised past the run's
/// largest recorded `seq`, so a daemon restart cannot make the new events sort before the old ones
/// (core#408), and — when there WAS history — the record is stamped `daemonRestarted: true`. Every
/// later record for that run is a plain append. Any sink that can write must pass its own set; a
/// path that skipped this would reintroduce the restart at 0, which is why this is not a bare
/// `(root, event)` call.
///
/// `false` means one of three things: the event was a declared streaming exclusion, it was not
/// run-scoped, or the writer channel is gone. Best-effort throughout: a full disk or a permissions
/// problem costs the record, never the run and never the live fanout.
fn append(root: &Path, ev: &CoreEvent, continued: &mut HashSet<String>) -> bool {
    // Cheapest test first, and deliberately BEFORE `to_json`: the excluded variants outnumber
    // everything else by orders of magnitude, and this runs on the actor thread.
    if is_high_volume(ev) {
        return false;
    }
    let mut json = ev.to_json();
    let Some(run_id) = run_key(&json).map(str::to_string) else {
        return false;
    };
    let path = run_log_path(root, &run_id);
    // First record THIS engine writes for the run: continue from wherever the run's history already
    // reached. No flush of the writer queue is needed here — anything still queued for this run was
    // stamped by this same process's counter, which is already past it; only a PREVIOUS process's
    // records (on disk) can be ahead of the counter.
    let (restarted, torn_tail) = if continued.insert(run_id.clone()) {
        let prior = inspect_log(&path);
        if let Some(max) = prior.max_seq {
            SEQ.fetch_max(max.saturating_add(1), Ordering::Relaxed);
        }
        (prior.max_seq.is_some(), prior.torn_tail)
    } else {
        (false, false)
    };
    // Stamped HERE, on the emitting thread, not on the writer: `ts` must be capture time and `seq`
    // must reflect emission order, neither of which survives being assigned after a queue hop.
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    if let Some(obj) = json.as_object_mut() {
        obj.insert("ts".to_string(), serde_json::json!(ts));
        obj.insert("seq".to_string(), serde_json::json!(seq));
        if restarted {
            obj.insert("daemonRestarted".to_string(), serde_json::json!(true));
        }
    }
    // A torn trailing line (the crash that precedes a restart) has no newline. Appending straight
    // after it would glue THIS record — the seeded, marked first one after the restart — onto the
    // fragment, and the reader would drop both as one unparseable line. Start on a fresh line
    // instead: the fragment stays its own unparseable line, which the reader already skips, so the
    // crash still costs exactly the torn record and nothing after it.
    let line = if torn_tail {
        format!("\n{json}\n")
    } else {
        format!("{json}\n")
    };
    writer().send(LogMsg::Record { path, line }).is_ok()
}

/// Read a run's recorded events, oldest first. Missing log ⇒ empty (a run that never emitted, or one
/// from before this existed — not an error). Unparseable lines are skipped rather than failing the
/// read: a torn final line from a crash mid-append must not make the preceding history unreadable.
///
/// Order is by the envelope `seq` — the contract consumers read the tail of as "latest" — wherever
/// `seq` IS an order. A log a pre-#408 engine wrote across a daemon restart holds a second `seq` run
/// starting at 0 (the counter restarted with the process), and sorting THAT by `seq` is precisely
/// the interleaving the finding reports: the post-restart events land ahead of the older ones and
/// the tail is a stale `awaitingHuman`. A repeated `seq` is proof the counter restarted, and for an
/// append-only single-writer log the file order is the emission order — so when `seq` repeats, file
/// order stands and the sort is skipped. Logs written since the fix never repeat a `seq`, so for
/// them the two agree and the sort is a no-op that keeps the read robust to a future concurrent
/// writer.
pub fn read_run(root: &Path, run_id: &str) -> Vec<serde_json::Value> {
    // Drain the writer first: without this a caller could read back a history missing the events it
    // just emitted, purely because they were still in the queue.
    flush();
    let Some(raw) = read_log(&run_log_path(root, run_id)) else {
        return Vec::new();
    };
    let mut out = parse_log(&raw);
    let seqs: Vec<u64> = out.iter().map(|v| seq_of(v).unwrap_or(0)).collect();
    let distinct: HashSet<u64> = seqs.iter().copied().collect();
    if distinct.len() == seqs.len() {
        // Stable, so records without a `seq` (none are written today) keep their file order.
        out.sort_by_key(|v| seq_of(v).unwrap_or(0));
    }
    out
}

/// The live subscriber list PLUS the durable log, bundled so the actor's single emit point reaches
/// both through one `&mut` argument.
///
/// This exists because of a borrow, not a taxonomy: the emit closure is passed alongside `&mut store`
/// to the same call, so the sink cannot capture the store — but it CAN own a log root that needs no
/// store. Bundling here also means every one of the actor's emissions is recorded by construction;
/// there is no second path an event could take to the socket while skipping the log.
#[derive(Default)]
pub struct EventSink {
    subscribers: Vec<std::sync::mpsc::Sender<CoreEvent>>,
    /// Where this sink records. `None` ⇒ fan out only, for embedders and tests that want no
    /// filesystem writes.
    root: Option<PathBuf>,
    /// Runs this sink has already recorded for. A sink lives as long as the engine that owns it (in
    /// production, the daemon process), so "not yet in here" means "this engine's first record for
    /// the run" — the point where the persisted history seeds the counter and the restart marker is
    /// stamped (core#408). Grows by one id per run the engine touches; bounded by the same thing the
    /// writer's handle cache is, and a few dozen bytes per entry.
    continued: HashSet<String>,
}

impl EventSink {
    /// A sink that both fans out and records, under `root` (see [`log_root`]).
    pub fn persistent(root: PathBuf) -> Self {
        Self {
            subscribers: Vec::new(),
            root: Some(root),
            continued: HashSet::new(),
        }
    }

    /// Register a live subscriber.
    pub fn push(&mut self, s: std::sync::mpsc::Sender<CoreEvent>) {
        self.subscribers.push(s);
    }

    /// Record then fan out, dropping subscribers whose receiver has hung up.
    ///
    /// Recording happens FIRST and unconditionally: the whole point of FINDING-014 is that the trail
    /// must not depend on anyone listening, so a run with zero subscribers still produces complete
    /// evidence.
    pub fn emit(&mut self, ev: CoreEvent) {
        if let Some(root) = &self.root {
            append(root, &ev, &mut self.continued);
        }
        self.subscribers.retain(|s| s.send(ev.clone()).is_ok());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch log root. No env var and no thread-local: the root is a plain argument now, so tests
    /// are isolated by construction and can run in parallel with nothing to restore.
    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wicked-evlog-test-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The whole point of the finding: a run with NO live subscriber must still leave a complete,
    /// ordered, timestamped trail. Under the old fanout-only `emit` this recorded nothing at all.
    ///
    /// Also pins the two properties an evidence reader depends on and that the old path could not
    /// offer: every record carries a capture-time `ts` (no time value exists anywhere in the domain
    /// model to recover it from later), and `seq` totally orders events that share a millisecond.
    #[test]
    fn unwatched_run_still_leaves_an_ordered_timestamped_trail() {
        let root = tmp("unwatched");
        let mut sink = EventSink::persistent(root.clone());
        assert!(sink.subscribers.is_empty(), "no listener, by construction");
        for ord in 0..25u32 {
            sink.emit(CoreEvent::UnitDone {
                session: "run-a".to_string(),
                ord,
            });
        }
        let got = read_run(&root, "run-a");
        assert_eq!(
            got.len(),
            25,
            "every emission recorded with nobody watching"
        );
        for (i, v) in got.iter().enumerate() {
            assert_eq!(v["type"], "unitDone");
            assert_eq!(v["ord"], i as u64, "read back in emit order");
            assert!(
                v["ts"].as_u64().unwrap_or(0) > 1_600_000_000_000,
                "capture-time epoch millis, not a placeholder: {v}"
            );
        }
        let seqs: Vec<u64> = got.iter().map(|v| v["seq"].as_u64().unwrap()).collect();
        let mut sorted = seqs.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(seqs, sorted, "seq is strictly increasing — a total order");
        // Deliberately NOT asserted: that this burst shares a millisecond. It usually does — which is
        // the reason `seq` exists at all, since `ts` alone cannot order a run — but it is a timing
        // incident, not a property, and asserting it would make the suite flaky on a slow filesystem.
        // The `seq` assertion above is the one that has to hold.
    }

    /// Two concurrent runs must not contaminate each other's evidence — the leakage property, at the
    /// log level. `run_key` routes by the event's OWN session id, and `encode_run_id` is injective, so
    /// no pair of run ids can land in one file.
    #[test]
    fn runs_are_isolated_and_non_run_events_are_not_attributed_to_any_run() {
        let root = tmp("isolation");
        let mut sink = EventSink::persistent(root.clone());
        sink.emit(CoreEvent::UnitDone {
            session: "org-a/repo".to_string(),
            ord: 1,
        });
        sink.emit(CoreEvent::UnitDone {
            session: "org-b/repo".to_string(),
            ord: 2,
        });
        // Not run-scoped: must not be filed under either run.
        sink.emit(CoreEvent::ChatClosed {
            chat: "c1".to_string(),
            reason: "requested".to_string(),
        });
        sink.emit(CoreEvent::RepoRegistered {
            repo_ref: "org-a/repo".to_string(),
        });
        let a = read_run(&root, "org-a/repo");
        let b = read_run(&root, "org-b/repo");
        assert_eq!(a.len(), 1, "run a sees only its own event: {a:?}");
        assert_eq!(b.len(), 1, "run b sees only its own event: {b:?}");
        assert_eq!(a[0]["ord"], 1);
        assert_eq!(b[0]["ord"], 2);
        assert_ne!(
            run_log_path(&root, "org-a/repo"),
            run_log_path(&root, "org-b/repo"),
            "distinct run ids ⇒ distinct files"
        );
        // A `/`-bearing run id must stay inside the root rather than escaping via the path.
        assert!(
            run_log_path(&root, "../../etc/passwd").starts_with(&root),
            "run ids are path-escaped, not interpolated"
        );
    }

    /// Two `Core`s on different stores must not share a log tree. This is the property that anchoring
    /// the root to the store buys, and the reason a full test run no longer writes into `~/.wicked`.
    #[test]
    fn separate_stores_get_separate_logs() {
        let a = tmp("store-a");
        let b = tmp("store-b");
        let mut sink_a = EventSink::persistent(a.clone());
        let mut sink_b = EventSink::persistent(b.clone());
        // SAME run id in both — only the root distinguishes them.
        sink_a.emit(CoreEvent::UnitDone {
            session: "shared-id".to_string(),
            ord: 7,
        });
        sink_b.emit(CoreEvent::UnitDone {
            session: "shared-id".to_string(),
            ord: 9,
        });
        let ra = read_run(&a, "shared-id");
        let rb = read_run(&b, "shared-id");
        assert_eq!(ra.len(), 1, "store a holds only its own: {ra:?}");
        assert_eq!(rb.len(), 1, "store b holds only its own: {rb:?}");
        assert_eq!(ra[0]["ord"], 7);
        assert_eq!(rb[0]["ord"], 9);
    }

    /// Streaming chunks are excluded, and the exclusion list is the whole of it — anything not on it is
    /// recorded. This is the test that would fail if someone widened the filter and quietly dropped
    /// real history.
    #[test]
    fn only_the_named_streaming_variants_are_excluded() {
        let root = tmp("filter");
        let mut sink = EventSink::persistent(root.clone());
        sink.emit(CoreEvent::CliOutputDelta {
            session: "r".to_string(),
            ord: 0,
            chunk: "noise".to_string(),
        });
        assert_eq!(
            read_run(&root, "r").len(),
            0,
            "delta chunks are not run history"
        );
        sink.emit(CoreEvent::UnitContextInjected {
            session: "r".to_string(),
            ord: 1,
            recipient_cli: "claude".to_string(),
            prior_units: vec![],
        });
        sink.emit(CoreEvent::SessionCompleted {
            session: "r".to_string(),
        });
        let got = read_run(&root, "r");
        assert_eq!(got.len(), 2, "everything else is recorded: {got:?}");
        assert_eq!(got[0]["type"], "unitContextInjected");
        assert_eq!(got[1]["type"], "sessionCompleted");
    }

    /// The exclusion set exists twice — as an enum match (what [`append`] actually filters on, chosen
    /// so no delta is ever serialized) and as `type` strings (what the module docs promise and what a
    /// reader of the log sees). Two spellings of one rule drift; this makes them fail loudly instead.
    #[test]
    fn the_two_spellings_of_the_exclusion_set_agree() {
        let excluded = [
            CoreEvent::CliOutputDelta {
                session: "r".to_string(),
                ord: 0,
                chunk: String::new(),
            },
            CoreEvent::ChatDelta {
                chat: "c".to_string(),
                cli_key: "claude".to_string(),
                text: String::new(),
            },
            CoreEvent::TerminalOutput {
                id: "t".to_string(),
                seq: 0,
                bytes_b64: String::new(),
            },
            CoreEvent::Heartbeat,
        ];
        let mut names: Vec<String> = Vec::new();
        for ev in &excluded {
            assert!(
                is_high_volume(ev),
                "{} is a documented exclusion but append would record it",
                ev.to_json()["type"]
            );
            names.push(ev.to_json()["type"].as_str().unwrap().to_string());
        }
        names.sort();
        let mut declared: Vec<String> = high_volume_type_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        declared.sort();
        assert_eq!(
            names, declared,
            "the variants append filters and the type names the docs/log use have diverged"
        );

        // And nothing load-bearing gets caught by the filter.
        for ev in [
            CoreEvent::SessionCompleted {
                session: "r".to_string(),
            },
            CoreEvent::UnitExecuting {
                session: "r".to_string(),
                ord: 0,
            },
        ] {
            assert!(
                !is_high_volume(&ev),
                "{} must never be filtered out of the evidence trail",
                ev.to_json()["type"]
            );
        }
    }

    /// Campaign node events name their run as `runId`, not `session`. They are the ONLY variants
    /// routed by the second key, so a wrong spelling there costs an entire class of history while
    /// every other test still passes — which is exactly what happened: `run_key` first looked for
    /// `run_id`, a key nothing emits, and both variants were dropped.
    #[test]
    fn campaign_node_events_are_routed_to_their_run() {
        let events = [
            CoreEvent::CampaignNodeStarted {
                campaign: "c1".to_string(),
                node: "n1".to_string(),
                run_id: "run-7".to_string(),
            },
            CoreEvent::CampaignNodeAwaitingHuman {
                campaign: "c1".to_string(),
                node: "n1".to_string(),
                run_id: "run-7".to_string(),
                prompt: "approve?".to_string(),
            },
        ];
        for ev in &events {
            let json = ev.to_json();
            assert_eq!(
                run_key(&json),
                Some("run-7"),
                "{} carries a run but was not routed to it",
                json["type"]
            );
        }

        // And end-to-end through the sink, so the routing key and the file it lands in agree.
        let root = tmp("campaign-node");
        let mut sink = EventSink::persistent(root.clone());
        for ev in events {
            sink.emit(ev);
        }
        let recorded = read_run(&root, "run-7");
        assert_eq!(
            recorded.len(),
            2,
            "campaign node events must appear in their run's history, got {recorded:?}"
        );
    }

    /// A crash mid-append leaves a torn final line. That must cost the torn record only — not the
    /// history in front of it, which is the part an operator is reading the log for.
    #[test]
    fn a_torn_trailing_line_does_not_destroy_the_history_before_it() {
        let root = tmp("torn");
        let mut sink = EventSink::persistent(root.clone());
        for ord in 0..3u32 {
            sink.emit(CoreEvent::UnitDone {
                session: "t".to_string(),
                ord,
            });
        }
        // Reaching around `read_run` to corrupt the file, so drain the writer by hand first —
        // `append` only ENQUEUES, and the file need not exist yet.
        flush();
        let path = run_log_path(&root, "t");
        let mut raw = std::fs::read_to_string(&path).unwrap();
        raw.push_str("{\"type\":\"unitDone\",\"sess");
        std::fs::write(&path, raw).unwrap();
        let got = read_run(&root, "t");
        assert_eq!(got.len(), 3, "three intact records survive the torn tail");
        assert_eq!(got[2]["ord"], 2);
    }

    /// Reading a run that never existed is an empty history, not an error — an evidence bundle for a
    /// pre-existing run (or one that emitted nothing) must still assemble.
    #[test]
    fn absent_log_reads_as_empty() {
        assert!(read_run(&tmp("absent"), "never-ran").is_empty());
    }

    /// The root is derived from the store path, so it inherits the store's durability. Pin both halves:
    /// it sits beside the store (the `.mem` / `.knowledge` sidecar convention), and it does NOT live
    /// under the temp-dir governance root, which a run's fresh re-launch wipes and the OS clears —
    /// either would silently destroy the trail this module exists to keep.
    #[test]
    fn the_log_root_is_a_sidecar_of_the_store_not_the_wiped_governance_root() {
        let root = log_root("/var/lib/wicked/core.db");
        assert_eq!(root, PathBuf::from("/var/lib/wicked/core.db.events"));
        assert_eq!(
            root.parent().unwrap(),
            Path::new("/var/lib/wicked"),
            "sibling of the store, like <store>.mem and <store>.knowledge"
        );
        assert!(
            !root.starts_with(std::env::temp_dir().join("wicked-core-gov")),
            "must not live under the gov root, which a fresh re-launch wipes: {root:?}"
        );
        let p = run_log_path(&root, "run-x");
        assert_eq!(p.file_name().unwrap(), "run-x.ndjson");
    }

    /// A sink with no root writes nothing — the default used by embedders and tests.
    #[test]
    fn rootless_sink_writes_nothing() {
        let root = tmp("nonpersist");
        let mut sink = EventSink::default();
        sink.emit(CoreEvent::UnitDone {
            session: "np".to_string(),
            ord: 0,
        });
        assert!(read_run(&root, "np").is_empty());
        assert!(!run_log_path(&root, "np").exists());
    }

    // ── core#408: `seq` survives a daemon restart ─────────────────────────────────────────────

    /// One recorded line for run `r`, as a log left by a PREVIOUS process would hold it. The only
    /// thing one test process cannot produce for real is history stamped by another process's
    /// counter, so these tests write that history by hand.
    fn recorded_line(ty: &str, ord: u32, ts: u64, seq: u64) -> String {
        format!("{{\"type\":\"{ty}\",\"session\":\"r\",\"ord\":{ord},\"ts\":{ts},\"seq\":{seq}}}\n")
    }

    fn write_log(root: &Path, run_id: &str, raw: &str) -> PathBuf {
        let path = run_log_path(root, run_id);
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(&path, raw).unwrap();
        path
    }

    /// The finding: a fresh engine over a run that already has history must CONTINUE the run's
    /// `seq`, not restart it. The counter is process-wide and this binary is one process, so the
    /// history is given a `seq` far beyond anything this process has stamped — the exact relation a
    /// restarted daemon (counter at 0) has to the log it finds on disk. Without the seed the new
    /// records would carry a small `seq`, sort BEFORE the history, and the tail — what every
    /// "latest event" consumer reads — would be the stale pre-restart `awaitingHuman`.
    #[test]
    fn a_fresh_engine_continues_a_runs_seq_from_its_persisted_log() {
        let root = tmp("restart-seed");
        const PRIOR_MAX: u64 = 1 << 40;
        let mut raw = String::new();
        raw.push_str(&recorded_line(
            "sessionStarted",
            0,
            1_700_000_000_000,
            PRIOR_MAX - 2,
        ));
        raw.push_str(&recorded_line(
            "unitDone",
            1,
            1_700_000_000_001,
            PRIOR_MAX - 1,
        ));
        raw.push_str(&recorded_line(
            "awaitingHuman",
            2,
            1_700_000_000_002,
            PRIOR_MAX,
        ));
        write_log(&root, "r", &raw);

        // A new sink is a new engine — in production, the restarted daemon.
        let mut sink = EventSink::persistent(root.clone());
        sink.emit(CoreEvent::Resumed {
            session: "r".to_string(),
            ord: 2,
        });
        sink.emit(CoreEvent::UnitExecuting {
            session: "r".to_string(),
            ord: 2,
        });

        let got = read_run(&root, "r");
        let types: Vec<&str> = got.iter().map(|v| v["type"].as_str().unwrap()).collect();
        assert_eq!(
            types,
            [
                "sessionStarted",
                "unitDone",
                "awaitingHuman",
                "resumed",
                "unitExecuting"
            ],
            "history first, then the resumption — the tail is the latest event"
        );
        let seqs: Vec<u64> = got.iter().map(|v| v["seq"].as_u64().unwrap()).collect();
        assert!(
            seqs.windows(2).all(|w| w[0] < w[1]),
            "strictly increasing across the restart: {seqs:?}"
        );
        assert!(
            seqs[3] > PRIOR_MAX,
            "the first post-restart seq continues past the persisted max, not from this \
             process's counter: {seqs:?}"
        );

        // The boundary is marked exactly once, on the first post-restart record.
        assert_eq!(got[3]["daemonRestarted"], serde_json::json!(true));
        let marked: Vec<usize> = got
            .iter()
            .enumerate()
            .filter(|(_, v)| v.get("daemonRestarted").is_some())
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            marked,
            vec![3],
            "one marker per restart, on the first record after it: {got:#?}"
        );
    }

    /// A run with no history is simply a new run: nothing to continue from and no restart to mark.
    /// A marker here would make every run read as resumed and the field useless.
    #[test]
    fn a_run_without_prior_history_is_not_marked_as_restarted() {
        let root = tmp("fresh-run");
        let mut sink = EventSink::persistent(root.clone());
        for ord in 0..3u32 {
            sink.emit(CoreEvent::UnitDone {
                session: "r".to_string(),
                ord,
            });
        }
        let got = read_run(&root, "r");
        assert_eq!(got.len(), 3);
        assert!(
            got.iter().all(|v| v.get("daemonRestarted").is_none()),
            "a fresh run carries no restart marker: {got:#?}"
        );
    }

    /// The seed reads past a torn trailing line — the crash that most plausibly precedes a restart.
    /// The max is taken over every parseable record, not read off the last line. And the first
    /// post-restart record starts on a fresh line: appended straight after the newline-less
    /// fragment it would glue onto it and both would read as one unparseable line — losing the very
    /// record that carries the seeded `seq` and the restart marker.
    #[test]
    fn the_seed_survives_a_torn_trailing_line() {
        let root = tmp("restart-torn");
        const PRIOR_MAX: u64 = 1 << 41;
        let mut raw = recorded_line("unitDone", 0, 1_700_000_000_000, PRIOR_MAX);
        raw.push_str("{\"type\":\"unitDone\",\"sess");
        let path = write_log(&root, "r", &raw);
        assert_eq!(persisted_max_seq(&path), Some(PRIOR_MAX));

        let mut sink = EventSink::persistent(root.clone());
        sink.emit(CoreEvent::UnitDone {
            session: "r".to_string(),
            ord: 1,
        });
        let got = read_run(&root, "r");
        assert_eq!(got.len(), 2, "the intact record plus the new one: {got:#?}");
        assert!(got[1]["seq"].as_u64().unwrap() > PRIOR_MAX);
        assert_eq!(got[1]["daemonRestarted"], serde_json::json!(true));
    }

    /// Copilot on #420: the crash can cut a MULTI-BYTE character in the torn line, leaving the file
    /// invalid UTF-8. `read_to_string` would then refuse the whole file — the history would read as
    /// empty, and the restarted engine, finding "no history", would seed nothing and restart `seq`
    /// at 0: the finding, back through a side door. The torn line must cost only itself, for the
    /// seed AND for the read.
    #[test]
    fn a_torn_multibyte_character_costs_only_the_torn_line() {
        let root = tmp("restart-torn-utf8");
        const PRIOR_MAX: u64 = 1 << 42;
        let mut bytes = recorded_line("unitDone", 0, 1_700_000_000_000, PRIOR_MAX).into_bytes();
        // "—" is E2 80 94; the crash landed after its second byte.
        bytes.extend_from_slice(
            b"{\"type\":\"error\",\"session\":\"r\",\"message\":\"gate \xE2\x80",
        );
        assert!(
            std::str::from_utf8(&bytes).is_err(),
            "the fixture really is invalid UTF-8"
        );
        let path = run_log_path(&root, "r");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&path, &bytes).unwrap();

        assert_eq!(persisted_max_seq(&path), Some(PRIOR_MAX));
        assert_eq!(
            read_run(&root, "r").len(),
            1,
            "the intact record survives the torn one"
        );

        let mut sink = EventSink::persistent(root.clone());
        sink.emit(CoreEvent::UnitDone {
            session: "r".to_string(),
            ord: 1,
        });
        let got = read_run(&root, "r");
        assert_eq!(got.len(), 2, "{got:#?}");
        assert!(got[1]["seq"].as_u64().unwrap() > PRIOR_MAX);
        assert_eq!(got[1]["daemonRestarted"], serde_json::json!(true));
    }

    /// An absent or record-less log has no max — the fresh-run case, spelled at the helper.
    #[test]
    fn no_history_has_no_max_seq() {
        let root = tmp("no-max");
        assert_eq!(persisted_max_seq(&run_log_path(&root, "never")), None);
        let path = write_log(&root, "empty", "\n\n");
        assert_eq!(persisted_max_seq(&path), None);
    }

    /// What a pre-#408 engine left on disk: `seq` 0..=2 before the restart, then 0..=1 again after
    /// it. File order is emission order (append-only, single writer); sorting by the REPEATED `seq`
    /// is what put the post-restart `resumed` ahead of `sessionStarted` and left `awaitingHuman` at
    /// the tail. The reader must hand such a log back in file order.
    #[test]
    fn a_log_written_across_a_pre_fix_restart_reads_back_in_emission_order() {
        let root = tmp("legacy-restart");
        let mut raw = String::new();
        raw.push_str(&recorded_line("sessionStarted", 0, 1_000, 0));
        raw.push_str(&recorded_line("unitPlanned", 1, 1_000, 1));
        raw.push_str(&recorded_line("awaitingHuman", 1, 1_001, 2));
        // — daemon restart: the old counter started over —
        raw.push_str(&recorded_line("resumed", 1, 5_000, 0));
        raw.push_str(&recorded_line("unitExecuting", 1, 5_001, 1));
        write_log(&root, "r", &raw);

        let types: Vec<String> = read_run(&root, "r")
            .iter()
            .map(|v| v["type"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            types,
            [
                "sessionStarted",
                "unitPlanned",
                "awaitingHuman",
                "resumed",
                "unitExecuting"
            ],
            "a repeated seq means the counter restarted; file order is the emission order"
        );
    }

    /// The robustness the sort exists for is kept: when `seq` IS an order (no repeats), a file whose
    /// line order disagrees with it reads back by `seq`.
    #[test]
    fn distinct_seqs_still_order_a_file_whose_line_order_disagrees() {
        let root = tmp("shuffled");
        let mut raw = String::new();
        raw.push_str(&recorded_line("unitDone", 2, 1_000, 2));
        raw.push_str(&recorded_line("unitDone", 0, 1_000, 0));
        raw.push_str(&recorded_line("unitDone", 1, 1_000, 1));
        write_log(&root, "r", &raw);
        let ords: Vec<u64> = read_run(&root, "r")
            .iter()
            .map(|v| v["ord"].as_u64().unwrap())
            .collect();
        assert_eq!(ords, [0, 1, 2]);
    }
}
