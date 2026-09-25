//! DES-TEAMING-002 P1 — `TeamBus` over a real bus db: the outbox, the bounded drain, FIFO, the
//! supersede rule and the replay. Every test uses its own temp bus and temp outbox: nothing here
//! can write the operator's `emit-outbox.ndjson` or any file under a home directory.
//!
//! "The bus refuses" is a SQLite trigger on the test bus (`RAISE(ABORT)` for the chosen team
//! types): deterministic, per type, and no busy-timeout wait. P1 (a)/(c) also hold a real
//! EXCLUSIVE lock, the unwritable-bus case the acceptance names.

use super::*;
use crate::team::events::{self as tev, TeamEvent};

const FIXTURES: &str = include_str!("events_fixtures.json");

pub(crate) struct Rig {
    pub dir: PathBuf,
    pub bus: String,
    pub outbox: PathBuf,
}

pub(crate) fn rig(name: &str) -> Rig {
    let dir = std::env::temp_dir().join(format!(
        "wicked-core-p1-{name}-{}-{}",
        std::process::id(),
        now_ms()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let bus = dir.join("bus.db").to_string_lossy().into_owned();
    // Create the bus through the engine's own shared handle, as a daemon would.
    BusDb::shared(&bus).expect("bus opens");
    Rig {
        outbox: dir.join(TEAM_OUTBOX_FILE),
        bus,
        dir,
    }
}

impl Rig {
    pub fn team_bus(&self) -> TeamBus {
        TeamBus::new(
            self.bus.clone(),
            self.outbox.clone(),
            Duration::from_millis(30),
        )
    }

    fn conn(&self) -> rusqlite::Connection {
        let c = rusqlite::Connection::open(&self.bus).unwrap();
        c.busy_timeout(Duration::from_secs(5)).unwrap();
        c
    }

    /// Make the bus refuse these team types (all of `core.team` for `&[]`).
    pub fn refuse(&self, types: &[&str]) {
        let when = if types.is_empty() {
            "NEW.subdomain = 'core.team'".to_string()
        } else {
            let list: Vec<String> = types.iter().map(|t| format!("'{t}'")).collect();
            format!("NEW.event_type IN ({})", list.join(","))
        };
        self.conn()
            .execute_batch(&format!(
                "DROP TRIGGER IF EXISTS p1_refuse; CREATE TRIGGER p1_refuse BEFORE INSERT ON \
                 events WHEN {when} BEGIN SELECT RAISE(ABORT, 'p1 test: bus refuses'); END;"
            ))
            .unwrap();
    }

    /// The bus takes everything again.
    pub fn allow(&self) {
        self.conn()
            .execute_batch("DROP TRIGGER IF EXISTS p1_refuse;")
            .unwrap();
    }

    /// `(event_id, event_type, idempotency_key)` of every team row of `run`, in bus order.
    pub fn rows(&self, run: &str) -> Vec<(i64, String, String)> {
        let c = self.conn();
        let mut st = c
            .prepare(
                "SELECT event_id, event_type, idempotency_key, payload FROM events \
                 WHERE subdomain = 'core.team' ORDER BY event_id",
            )
            .unwrap();
        st.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .filter(|(_, _, _, p)| {
            serde_json::from_str::<Value>(p)
                .ok()
                .and_then(|v| v.get("run_id").and_then(Value::as_str).map(str::to_string))
                .as_deref()
                == Some(run)
        })
        .map(|(id, t, k, _)| (id, t, k))
        .collect()
    }

    pub fn types(&self, run: &str) -> Vec<String> {
        self.rows(run).into_iter().map(|(_, t, _)| t).collect()
    }

    pub fn outbox_lines(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.outbox)
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The `nth` T1 fixture of `event_type`, re-homed on `run` (the key is rebuilt from the run).
pub(crate) fn fixture(event_type: &str, nth: usize, run: &str) -> TeamEvent {
    fixture_with(event_type, nth, run, |_| {})
}

pub(crate) fn fixture_with(
    event_type: &str,
    nth: usize,
    run: &str,
    edit: impl FnOnce(&mut Value),
) -> TeamEvent {
    let all: Vec<Value> = serde_json::from_str(FIXTURES).unwrap();
    let mut payload = all
        .iter()
        .filter(|f| f["type"] == event_type)
        .nth(nth)
        .unwrap_or_else(|| panic!("no fixture #{nth} for {event_type}"))["payload"]
        .clone();
    payload["run_id"] = json!(run);
    edit(&mut payload);
    TeamEvent::from_payload(event_type, &payload).expect("fixture parses")
}

fn gate_opened_kind(kind: &str) -> usize {
    let all: Vec<Value> = serde_json::from_str(FIXTURES).unwrap();
    all.iter()
        .filter(|f| f["type"] == tev::GATE_OPENED)
        .position(|f| f["payload"]["kind"] == kind)
        .unwrap_or_else(|| panic!("no gate.opened fixture of kind {kind}"))
}

fn published(o: &PublishOutcome) -> bool {
    matches!(o, PublishOutcome::Published(_))
}

// ── P1 (a): the bus is unwritable, every fact spools, and lands once when it returns ─────────────

/// P1 (a) — with the bus db held EXCLUSIVE (unwritable), every team fact lands in the team
/// outbox (the emit-outbox record plus key, run and owner); once the bus returns, a drain
/// publishes each exactly once, and a second drain publishes nothing more.
#[test]
fn p1_a_unwritable_bus_every_fact_spools_and_lands_once_when_the_bus_returns() {
    let rig = rig("a");
    let tb = rig.team_bus();
    let run = "run-a";
    let facts = [
        fixture(tev::PATH_STARTED, 0, run),
        fixture(tev::PLAN_ACCEPTED, 0, run),
        fixture(tev::GATE_OPENED, gate_opened_kind("team_transport"), run),
    ];
    let holder = rig.conn();
    holder.execute_batch("BEGIN EXCLUSIVE;").unwrap();
    for f in &facts {
        let o = tb.publish(f).unwrap();
        assert!(matches!(o, PublishOutcome::Spooled(_)), "{o:?}");
    }
    let lines = rig.outbox_lines();
    assert_eq!(lines.len(), 3, "{lines:#?}");
    // The first was refused by the bus; the later ones queued behind it (FIFO).
    assert!(lines[0]["deadletter_reason"]
        .as_str()
        .unwrap()
        .contains("bus write failed"));
    assert!(lines[2]["deadletter_reason"]
        .as_str()
        .unwrap()
        .contains("FIFO"));
    for (line, f) in lines.iter().zip(&facts) {
        assert_eq!(line["type"], f.event_type());
        assert_eq!(line["idempotency_key"], f.key().unwrap());
        assert_eq!(line["run_id"], run);
        assert_eq!(line["owner"], "engine");
        assert_eq!(line["domain"], crate::bus::CORE_DOMAIN);
        assert_eq!(line["subdomain"], tev::TEAM_SUBDOMAIN);
        assert!(!line["deadletter_reason"].as_str().unwrap().is_empty());
        assert!(line["ts"].as_i64().is_some() && line["pid"].as_u64().is_some());
    }
    holder.execute_batch("COMMIT;").unwrap();
    let r = tb.drain_all();
    assert_eq!(r.published.len(), 3, "{r:?}");
    assert_eq!(r.remaining, 0);
    let rows = rig.rows(run);
    assert_eq!(
        rows.iter().map(|r| r.2.clone()).collect::<Vec<_>>(),
        facts.iter().map(|f| f.key().unwrap()).collect::<Vec<_>>(),
        "one row per fact, in emit order"
    );
    assert!(
        rig.outbox_lines().is_empty(),
        "published lines are compacted"
    );
    assert_eq!(tb.drain_all().published.len(), 0);
    assert_eq!(rig.rows(run).len(), 3);
}

/// P1 (a) — the spool announces itself: a `DEADLETTER_MARKER` line on stderr for every spooled
/// fact. Runs the spool in a child copy of this test binary and reads the child's stderr.
#[test]
fn p1_a_a_spooled_fact_writes_a_deadletter_marker_on_stderr() {
    if std::env::var_os("WICKED_P1_SPOOL_CHILD").is_some() {
        return;
    }
    let exe = std::env::current_exe().unwrap();
    let out = std::process::Command::new(exe)
        .args([
            "--exact",
            "team::publish::tests::p1_child_spools_one_fact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("WICKED_P1_SPOOL_CHILD", "1")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "child failed: {stderr}");
    let marked: Vec<&str> = stderr
        .lines()
        .filter(|l| l.starts_with(wicked_apps_core::emit::DEADLETTER_MARKER))
        .collect();
    assert!(
        marked.iter().any(|l| l.contains(tev::PATH_STARTED)),
        "no DEADLETTER_MARKER line for the spooled fact in: {stderr}"
    );
}

/// The child half of the marker test (a no-op unless run by it).
#[test]
fn p1_child_spools_one_fact() {
    if std::env::var_os("WICKED_P1_SPOOL_CHILD").is_none() {
        return;
    }
    let rig = rig("marker");
    rig.refuse(&[]);
    let o = rig
        .team_bus()
        .publish(&fixture(tev::PATH_STARTED, 0, "run-marker"))
        .unwrap();
    assert!(matches!(o, PublishOutcome::Spooled(_)));
}

// ── P1 (d): replay is idempotent ─────────────────────────────────────────────────────────────────

/// P1 (d) — `replay_team_outbox` replays leftover lines idempotently: the same line present
/// twice (a crash between the bus write and the compaction) and replayed twice lands once.
#[test]
fn p1_d_a_line_replayed_twice_lands_once() {
    let rig = rig("d");
    let tb = rig.team_bus();
    rig.refuse(&[]);
    let f = fixture(tev::PATH_STARTED, 0, "run-d");
    assert!(matches!(
        tb.publish(&f).unwrap(),
        PublishOutcome::Spooled(_)
    ));
    let line = std::fs::read_to_string(&rig.outbox).unwrap();
    std::fs::write(&rig.outbox, format!("{line}{line}")).unwrap();
    rig.allow();
    let first = tb.drain_all();
    let second = tb.drain_all();
    assert_eq!(rig.rows("run-d").len(), 1, "{first:?} {second:?}");
    assert!(second.published.is_empty());
    // A copy of the line appended after it landed resolves to the same row.
    std::fs::write(&rig.outbox, &line).unwrap();
    let third = tb.drain_all();
    assert_eq!(
        third.published.len(),
        1,
        "dedup resolves to the existing row"
    );
    assert_eq!(rig.rows("run-d").len(), 1);
}

// ── P1 (g): per-run FIFO ─────────────────────────────────────────────────────────────────────────

/// P1 (g) — with `gate.opened` failing and `gate.decided` queued behind it, the bus never holds
/// a `gate.decided` whose `gate.opened` is absent: not in live draining, not after replay; once
/// `gate.opened` lands, both land in order.
#[test]
fn p1_g_gate_decided_never_lands_before_its_gate_opened() {
    let rig = rig("g");
    let tb = rig.team_bus();
    let run = "run-g";
    let opened = fixture(tev::GATE_OPENED, gate_opened_kind("team_transport"), run);
    let decided = fixture_with(tev::GATE_DECIDED, 0, run, |p| {
        p["gate_id"] = opened.to_payload().unwrap()["gate_id"].clone();
    });
    rig.refuse(&[tev::GATE_OPENED]);
    assert!(matches!(
        tb.publish(&opened).unwrap(),
        PublishOutcome::Spooled(_)
    ));
    // The bus would take gate.decided, but FIFO holds it behind the unpublished gate.opened.
    let o = tb.publish(&decided).unwrap();
    assert!(matches!(o, PublishOutcome::Spooled(_)), "{o:?}");
    assert!(rig.types(run).is_empty(), "live: {:?}", rig.types(run));
    let r = tb.drain_all();
    assert!(r.published.is_empty(), "replay: {r:?}");
    assert!(rig.types(run).is_empty());
    rig.allow();
    tb.drain_all();
    assert_eq!(rig.types(run), vec![tev::GATE_OPENED, tev::GATE_DECIDED]);
}

// ── The supersede rule ───────────────────────────────────────────────────────────────────────────

/// §4.1: a run tombstone supersedes the run's lines wherever they sit, and a live publish of a
/// later fact of the run, except `path.ended` appended after it (the rejected run's goodbye).
#[test]
fn a_run_tombstone_supersedes_every_line_of_the_run_but_a_later_path_ended() {
    let rig = rig("tomb");
    let tb = rig.team_bus();
    let run = "run-t";
    rig.refuse(&[]);
    tb.publish(&fixture(tev::PATH_STARTED, 0, run)).unwrap();
    tb.publish(&fixture(tev::PLAN_ACCEPTED, 0, run)).unwrap();
    tb.supersede_run(run, None, None, tev::PLAN_ACCEPTED, "rejected")
        .unwrap();
    assert_eq!(
        tb.publish(&fixture(tev::GATE_OPENED, 0, run)).unwrap(),
        PublishOutcome::Superseded
    );
    let ended = fixture(tev::PATH_ENDED, 0, run);
    assert!(matches!(
        tb.publish(&ended).unwrap(),
        PublishOutcome::Spooled(_)
    ));
    rig.allow();
    let r = tb.drain_all();
    assert_eq!(rig.types(run), vec![tev::PATH_ENDED], "{r:?}");
    assert_eq!(r.superseded, 2);
    // Another run is untouched.
    assert!(published(
        &tb.publish(&fixture(tev::PATH_STARTED, 0, "run-other"))
            .unwrap()
    ));
}

/// Computed fields are never read from input: a replayed line whose stored key is not its
/// payload's key is refused (kept for the operator, never published) and blocks nothing.
#[test]
fn a_line_whose_key_disagrees_with_its_payload_is_refused_on_replay() {
    let rig = rig("mis");
    let tb = rig.team_bus();
    rig.refuse(&[]);
    tb.publish(&fixture(tev::PATH_STARTED, 0, "run-m")).unwrap();
    tb.publish(&fixture(tev::PATH_STARTED, 0, "run-n")).unwrap();
    let text = std::fs::read_to_string(&rig.outbox).unwrap();
    let mut lines: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    lines[0]["idempotency_key"] = json!("0000000000000000000000000000beef");
    let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(&rig.outbox, body).unwrap();
    rig.allow();
    let r = tb.drain_all();
    assert_eq!(r.invalid, 1, "{r:?}");
    assert!(rig.types("run-m").is_empty());
    assert_eq!(rig.types("run-n"), vec![tev::PATH_STARTED]);
    assert_eq!(
        rig.outbox_lines().len(),
        1,
        "the refused line stays for the operator"
    );
}

// ── §4.8, one named fixture per row (acceptance (i)). Rows needing the actor are in
//    `actor/team_gate_tests.rs`; these are the rows whose four columns live in the outbox/bus. ──

/// §4.8 row 5 — `step.claimed` fails (R): (a) the attempt's snapshot is un-teamed (T5's worker
/// seam stamps it; here the attempt tombstone that precedes it); (b) nothing from R for that
/// attempt; (c) none; (d) the attempt tombstone ⇒ a replay publishes nothing for the attempt,
/// while another attempt of the run publishes normally.
#[test]
fn row5_step_claimed_fails_the_attempt_publishes_nothing_ever() {
    let rig = rig("row5");
    let tb = rig.team_bus();
    let run = "run-5";
    let claimed = fixture(tev::STEP_CLAIMED, 0, run);
    let (ord, attempt) = (claimed.env.ord.unwrap(), claimed.env.attempt.unwrap());
    rig.refuse(&[]);
    assert!(matches!(
        tb.publish(&claimed).unwrap(),
        PublishOutcome::Spooled(_)
    ));
    tb.supersede_run(
        run,
        Some(tev::Owner::Runner),
        Some((ord, attempt)),
        tev::STEP_CLAIMED,
        "step.claimed past the bound",
    )
    .unwrap();
    let checkpoint = fixture_with(tev::CHECKPOINT_REACHED, 0, run, |p| {
        p["ord"] = json!(ord);
        p["attempt"] = json!(attempt);
    });
    assert_eq!(tb.publish(&checkpoint).unwrap(), PublishOutcome::Superseded);
    rig.allow();
    tb.drain_all();
    assert!(rig.types(run).is_empty(), "{:?}", rig.types(run));
    let next = fixture_with(tev::STEP_CLAIMED, 0, run, |p| {
        p["attempt"] = json!(attempt + 1)
    });
    assert!(published(&tb.publish(&next).unwrap()));
    assert_eq!(rig.types(run), vec![tev::STEP_CLAIMED]);
}

/// §4.8 row 7 / P1 (h) — final-pass timeout: (b) `gate.opened{ledger_ref: null, ledger_source:
/// "synthesized"}` is on the bus and no `ledger.folded` for the attempt; (c) `ledger_ref` null ⇒
/// consumers read the persisted snapshot; (d) S's spooled fold is tombstoned at its deadline, so
/// neither a replay nor S's late live publish ever puts it on the bus.
#[test]
fn row7_final_pass_timeout_the_late_fold_is_never_published() {
    let rig = rig("row7");
    let tb = rig.team_bus();
    let run = "run-7";
    let fold = fixture(tev::LEDGER_FOLDED, 0, run);
    rig.refuse(&[tev::LEDGER_FOLDED]);
    assert!(matches!(
        tb.publish(&fold).unwrap(),
        PublishOutcome::Spooled(_)
    ));
    // The fold's deadline passes: S tombstones its own line.
    tb.supersede_fact(&fold.key().unwrap(), "fold past its deadline")
        .unwrap();
    let opened = fixture_with(
        tev::GATE_OPENED,
        gate_opened_kind("unit_review"),
        run,
        |p| {
            p["ledger_ref"] = Value::Null;
            p["ledger_source"] = json!("synthesized");
        },
    );
    assert!(published(&tb.publish(&opened).unwrap()));
    rig.allow();
    tb.drain_all();
    assert_eq!(
        tb.publish(&fold).unwrap(),
        PublishOutcome::Superseded,
        "S's late fold after the deadline"
    );
    tb.drain_all();
    assert_eq!(rig.types(run), vec![tev::GATE_OPENED]);
    let row = rig.conn().query_row(
        "SELECT payload FROM events WHERE event_type = ?1",
        [tev::GATE_OPENED],
        |r| r.get::<_, String>(0),
    );
    let p: Value = serde_json::from_str(&row.unwrap()).unwrap();
    assert_eq!(p["ledger_ref"], Value::Null);
    assert_eq!(p["ledger_source"], "synthesized");
}

/// §4.8 row 8 — rows deleted by the 24 h retention: (a) the persisted state stays; (b) recent rows
/// only; (c) a reference to a deleted row is "aged out"; (d) compaction removed the published
/// line, so no drain can re-publish the deleted row's fact.
#[test]
fn row8_retention_deleted_rows_are_never_republished() {
    let rig = rig("row8");
    let tb = rig.team_bus();
    let run = "run-8";
    rig.refuse(&[]);
    tb.publish(&fixture(tev::PATH_STARTED, 0, run)).unwrap();
    rig.allow();
    assert_eq!(tb.drain_all().published.len(), 1);
    assert!(rig.outbox_lines().is_empty(), "compacted once published");
    rig.conn()
        .execute("DELETE FROM events WHERE subdomain = 'core.team'", [])
        .unwrap();
    assert!(tb.drain_all().published.is_empty());
    assert!(
        rig.types(run).is_empty(),
        "the aged-out fact is not re-published"
    );
}

/// §4.8 row 9 — restart mid-step: (b) the dead attempt's rows are on the bus; (d) the outbox
/// drains in order after the restart (a fresh `TeamBus` on the same files, as a new daemon) and
/// nothing is published twice.
#[test]
fn row9_restart_mid_step_the_outbox_drains_in_order_once() {
    let rig = rig("row9");
    let run = "run-9";
    let before = rig.team_bus();
    rig.refuse(&[tev::PLAN_ACCEPTED]);
    assert!(published(
        &before.publish(&fixture(tev::PATH_STARTED, 0, run)).unwrap()
    ));
    before
        .publish(&fixture(tev::PLAN_ACCEPTED, 0, run))
        .unwrap();
    before
        .publish(&fixture(
            tev::GATE_OPENED,
            gate_opened_kind("team_transport"),
            run,
        ))
        .unwrap();
    drop(before);
    rig.allow();
    let after = rig.team_bus();
    let r = after.drain_all();
    assert_eq!(r.published.len(), 2, "{r:?}");
    assert_eq!(
        rig.types(run),
        vec![tev::PATH_STARTED, tev::PLAN_ACCEPTED, tev::GATE_OPENED]
    );
    assert!(after.drain_all().published.is_empty());
    let keys: Vec<String> = rig.rows(run).into_iter().map(|r| r.2).collect();
    let mut dedup = keys.clone();
    dedup.dedup();
    assert_eq!(keys, dedup);
}

/// §4.8 row 10 — restart with the stream gone (`path.started` aged out): (b) nothing for the gap;
/// (d) nothing to replay for the gap — the outbox holds no line for it, and a drain adds none.
/// (The supervisor's `stream_gap` fold and its team pause are T6's.)
#[test]
fn row10_stream_gone_nothing_to_replay_for_the_gap() {
    let rig = rig("row10");
    let tb = rig.team_bus();
    let run = "run-10";
    assert!(published(
        &tb.publish(&fixture(tev::PATH_STARTED, 0, run)).unwrap()
    ));
    rig.conn()
        .execute("DELETE FROM events WHERE subdomain = 'core.team'", [])
        .unwrap();
    let r = tb.drain_all();
    assert!(r.published.is_empty() && r.remaining == 0, "{r:?}");
    assert!(rig.types(run).is_empty());
}

/// §4.8 row 11 — a council with no verdict: (b) `council.called`, `council.ruled{no_verdict}`,
/// `ledger.folded` (S) and `gate.opened{team_dispute}` (E) all reach the bus; (c) all present;
/// (d) nothing special — S's facts, refused at first, drain in S's order once the bus takes
/// them, independent of E's lane.
#[test]
fn row11_council_no_verdict_facts_drain_in_order() {
    let rig = rig("row11");
    let tb = rig.team_bus();
    let run = "run-11";
    let s_facts = [
        fixture(tev::COUNCIL_CALLED, 0, run),
        fixture_with(tev::COUNCIL_RULED, 0, run, |p| {
            p["verdict"] = json!("no_verdict");
            p["reason"] = json!("no_quorum");
        }),
        fixture(tev::LEDGER_FOLDED, 0, run),
    ];
    rig.refuse(&[tev::COUNCIL_CALLED]);
    for f in &s_facts {
        assert!(matches!(tb.publish(f).unwrap(), PublishOutcome::Spooled(_)));
    }
    let dispute = fixture(tev::GATE_OPENED, gate_opened_kind("team_dispute"), run);
    assert!(
        published(&tb.publish(&dispute).unwrap()),
        "E's lane is its own FIFO"
    );
    rig.allow();
    tb.drain_all();
    assert_eq!(
        rig.types(run),
        vec![
            tev::GATE_OPENED,
            tev::COUNCIL_CALLED,
            tev::COUNCIL_RULED,
            tev::LEDGER_FOLDED
        ]
    );
}

/// The production team outbox lives under the store's state home, never under a home directory
/// the test did not choose (an earlier seam leaked lines into the operator's real emit outbox).
#[test]
fn the_team_outbox_is_under_the_store_state_home() {
    let rig = rig("home");
    let db = rig.dir.join("core.db");
    let cfg = TeamConfig::for_store(db.to_str().unwrap());
    let outbox = cfg.outbox.clone().expect("a file store has a state home");
    assert_eq!(outbox.file_name().unwrap(), TEAM_OUTBOX_FILE);
    assert_eq!(
        std::fs::canonicalize(outbox.parent().unwrap()).unwrap(),
        std::fs::canonicalize(&rig.dir).unwrap()
    );
    assert_eq!(
        cfg.bound(),
        Duration::from_secs(31),
        "5 attempts, 1+2+4+8+16 s"
    );
    assert!(TeamConfig::for_store(":memory:").outbox.is_none());
}
