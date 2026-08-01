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
//! - `seq` — a per-process monotonic counter. Millisecond stamps collide freely (a burst of events
//!   from one actor turn shares a millisecond), so `ts` alone cannot order a run. `seq` is the tiebreak
//!   and makes the total order recoverable.
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

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::event::CoreEvent;

/// Process-wide monotonic sequence. Shared across runs on purpose: it makes the interleaving of
/// concurrent runs recoverable too, which a per-run counter would lose.
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
/// Most variants carry `session`; campaign node events carry the node's `run_id`. Deriving the key
/// from the emitted object means a NEW variant that follows the convention is logged automatically —
/// a second hand-written 125-arm match would be a second thing to forget. `None` ⇒ not run-scoped
/// (chat, terminal, campaign-level), so not part of any run's evidence.
pub fn run_key(json: &serde_json::Value) -> Option<&str> {
    json.get("session")
        .and_then(|v| v.as_str())
        .or_else(|| json.get("run_id").and_then(|v| v.as_str()))
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

/// Queue one event for its run's log under `root`. Returns whether a record was ENQUEUED — the write
/// itself happens on the writer thread, so this is not a durability acknowledgement (use [`flush`]).
///
/// `false` means one of three things: the event was a declared streaming exclusion, it was not
/// run-scoped, or the writer channel is gone. Best-effort throughout: a full disk or a permissions
/// problem costs the record, never the run and never the live fanout.
pub fn append(root: &Path, ev: &CoreEvent) -> bool {
    // Cheapest test first, and deliberately BEFORE `to_json`: the excluded variants outnumber
    // everything else by orders of magnitude, and this runs on the actor thread.
    if is_high_volume(ev) {
        return false;
    }
    let mut json = ev.to_json();
    let Some(run_id) = run_key(&json).map(str::to_string) else {
        return false;
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
    }
    writer()
        .send(LogMsg::Record {
            path: run_log_path(root, &run_id),
            line: format!("{json}\n"),
        })
        .is_ok()
}

/// Read a run's recorded events, oldest first. Missing log ⇒ empty (a run that never emitted, or one
/// from before this existed — not an error). Unparseable lines are skipped rather than failing the
/// read: a torn final line from a crash mid-append must not make the preceding history unreadable.
pub fn read_run(root: &Path, run_id: &str) -> Vec<serde_json::Value> {
    // Drain the writer first: without this a caller could read back a history missing the events it
    // just emitted, purely because they were still in the queue.
    flush();
    let path = run_log_path(root, run_id);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let mut out: Vec<serde_json::Value> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    // Append order already IS emit order for a single writer, but sorting by the envelope makes the
    // read robust to a future concurrent writer without changing anything for today's single actor.
    out.sort_by_key(|v| v.get("seq").and_then(|s| s.as_u64()).unwrap_or(0));
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
}

impl EventSink {
    /// A sink that both fans out and records, under `root` (see [`log_root`]).
    pub fn persistent(root: PathBuf) -> Self {
        Self {
            subscribers: Vec::new(),
            root: Some(root),
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
            append(root, &ev);
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
}
