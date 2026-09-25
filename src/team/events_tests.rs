//! DES-TEAMING-002 T1 acceptance: (a) four segments, (b) value-compared round trips, (c) `fold`
//! on DES-001's fixtures #8, #11, #15, #16 a–k, (d) `fold` under duplicates, (e) the §4.1 key
//! vectors, (g) the §6.1 identity rule over all 25 types plus the key-builder source guard.
//! Every expected value is fixed here; the keys were computed by an independent transcription of
//! the §4.1 algorithm, not by the code under test.

use serde_json::{json, Value};

use super::*;
use crate::bus::{deterministic_key, BusDb};
use crate::team::{FinalPass, FindingStatus, LedgerDelivery};

/// A wire token as its typed enum (panics on an unknown token: fixtures only).
fn tok<T: serde::de::DeserializeOwned>(s: &str) -> T {
    serde_json::from_value(json!(s)).unwrap_or_else(|e| panic!("token {s:?}: {e}"))
}

const FIXTURES: &str = include_str!("events_fixtures.json");

fn fixtures() -> Vec<(String, Value)> {
    let v: Value = serde_json::from_str(FIXTURES).unwrap();
    v.as_array()
        .unwrap()
        .iter()
        .map(|f| {
            (
                f["type"].as_str().unwrap().to_string(),
                f["payload"].clone(),
            )
        })
        .collect()
}

fn fixture(event_type: &str) -> Value {
    fixtures()
        .into_iter()
        .find(|(t, _)| t == event_type)
        .map(|(_, p)| p)
        .unwrap_or_else(|| panic!("no fixture for {event_type}"))
}

// ── (a) grammar and ownership ─────────────────────────────────────────────────────────────────────

#[test]
fn every_type_is_four_segments_under_wicked_team() {
    let mut seen = std::collections::BTreeSet::new();
    for t in ALL_TYPES {
        let segs: Vec<&str> = t.split('.').collect();
        assert_eq!(segs.len(), 4, "{t}");
        assert_eq!((segs[0], segs[1]), ("wicked", "team"), "{t}");
        assert!(
            segs.iter()
                .all(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_lowercase() || c == '_')),
            "{t}: WB-001 charset"
        );
        assert!(seen.insert(t), "{t} listed twice");
    }
    assert_eq!(seen.len(), 25);
}

/// DES-002 §7 assertion 1: each type has exactly one owner, as the matrix assigns it.
#[test]
fn every_type_has_the_one_owner_the_matrix_assigns() {
    use Owner::{Engine as E, Runner as R, Supervisor as S};
    let matrix = [
        E, E, E, E, E, E, S, S, R, R, S, R, R, R, S, S, R, R, S, S, S, S, E, E, E,
    ];
    for (t, want) in ALL_TYPES.iter().zip(matrix) {
        assert_eq!(owner(t), Some(want), "{t}");
    }
    assert_eq!(owner("wicked.crew.run.requested"), None);
}

// ── (b) round trips, compared by value ────────────────────────────────────────────────────────────

#[test]
fn every_fixture_round_trips_by_value() {
    let all = fixtures();
    let covered: std::collections::BTreeSet<&str> = all.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(covered.len(), 25, "a fixture per type");
    for (t, payload) in &all {
        let ev = TeamEvent::from_payload(t, payload).unwrap_or_else(|e| panic!("{t}: {e:#}"));
        assert_eq!(ev.event_type(), t.as_str());
        assert_eq!(&ev.to_payload().unwrap(), payload, "{t} round trip");
    }
}

#[test]
fn a_payload_missing_a_field_or_naming_an_unknown_type_is_refused() {
    let mut p = fixture(FINDING_RAISED);
    p.as_object_mut().unwrap().remove("raise_seq");
    assert!(TeamEvent::from_payload(FINDING_RAISED, &p).is_err());
    assert!(TeamEvent::from_payload("wicked.team.path.wandered", &fixture(PATH_STARTED)).is_err());
    let mut low = fixture(LEDGER_FOLDED);
    low["ledger"]["findings"][0]["severity"] = json!("low");
    assert!(
        TeamEvent::from_payload(LEDGER_FOLDED, &low).is_err(),
        "a ledger finding below the bar"
    );
}

// ── (e) the §4.1 key algorithm ────────────────────────────────────────────────────────────────────

#[test]
fn the_des_vectors_match_deterministic_key_byte_for_byte() {
    assert_eq!(
        deterministic_key(&[
            "team",
            "wicked.team.finding.raised",
            "run-1",
            "f-3fa9c2e1d0b4a7e6"
        ]),
        "f8289d402fc42823fd875fcf4456bd8f"
    );
    assert_eq!(
        deterministic_key(&["team", "wicked.team.path.started", "run-1"]),
        "25ea4932f42b6e22f1aacd16bc3dcdd9"
    );
    assert_eq!(
        key_path_started("run-1"),
        "25ea4932f42b6e22f1aacd16bc3dcdd9"
    );
    // The failure the vector exists to catch: a `\0`-join without the trailing NUL.
    use sha2::{Digest, Sha256};
    let joined = [
        "team",
        "wicked.team.finding.raised",
        "run-1",
        "f-3fa9c2e1d0b4a7e6",
    ]
    .join("\0");
    let wrong: String = Sha256::digest(joined.as_bytes())
        .iter()
        .take(16)
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(wrong, "fed61d5909428bb7aec506ad09dc86d1");
}

#[test]
fn every_fixture_keys_to_its_fixed_value() {
    let want = [
        (
            "wicked.team.path.started",
            "d47a180bea33813b621ae21635872c07",
        ),
        (
            "wicked.team.path.scored",
            "49cf8532bcd55d8613fe98e4909b616a",
        ),
        (
            "wicked.team.plan.proposed",
            "abcb4cad4ddddea1920b580a3f47e897",
        ),
        (
            "wicked.team.plan.revised",
            "ba2dac6452b8d4b4bca27ab4c4eea758",
        ),
        (
            "wicked.team.plan.accepted",
            "17305523b085e10692dc99691e34d4d8",
        ),
        (
            "wicked.team.plan.refused",
            "bf2c504130d772320a0ad921012c21c9",
        ),
        (
            "wicked.team.member.joined",
            "4d31d91a66891cf6ffb8489c95275951",
        ),
        (
            "wicked.team.member.left",
            "2ed493eac380400a34dd36321c8159e7",
        ),
        (
            "wicked.team.step.claimed",
            "7c68ff2c79f4bec348c53d1e689904fd",
        ),
        (
            "wicked.team.checkpoint.reached",
            "ea3c05d48212f591ea5fb9184f0497fd",
        ),
        (
            "wicked.team.finding.raised",
            "88796d03e5083035922afa595f7601a6",
        ),
        (
            "wicked.team.advice.delivered",
            "a98075c153327e4d23ac01d121c67d7c",
        ),
        (
            "wicked.team.advice.answered",
            "60f6be8bfd3e0ad6b36657142a523dac",
        ),
        (
            "wicked.team.help.requested",
            "f8cd6dff40853156099bcea2130ea3ce",
        ),
        (
            "wicked.team.help.answered",
            "453dd2cb14b57a48509ab0f5bb58383a",
        ),
        (
            "wicked.team.change.requested",
            "6b80d638ce979d7c6255fb9ea9b7abef",
        ),
        (
            "wicked.team.step.completed",
            "60389a008abcb3363ac57ff2c2ead046",
        ),
        (
            "wicked.team.step.reviewed",
            "0aea19144388cbdf69ef875b7903b665",
        ),
        (
            "wicked.team.finding.settled",
            "2cd8416f64142620632ff34f38bb60a4",
        ),
        (
            "wicked.team.council.called",
            "42861609786df934ff8b443786dad111",
        ),
        (
            "wicked.team.council.ruled",
            "5673facd459e952c91630af43e74ab7f",
        ),
        (
            "wicked.team.ledger.folded",
            "3cd042600abfa95e861a07361ff18478",
        ),
        (
            "wicked.team.gate.opened",
            "2ea3f95feb57f7679a1e15145a12ee63",
        ),
        (
            "wicked.team.gate.opened",
            "92b2c6be477ba34bde95e2fa31b371b4",
        ),
        (
            "wicked.team.gate.opened",
            "bf5d42dcb030bded0b3740751133ac53",
        ),
        (
            "wicked.team.gate.opened",
            "0bae9761177641edfae23fdee995e9e0",
        ),
        (
            "wicked.team.gate.decided",
            "8d1d1f6e960c61422d2c33cff742f6c5",
        ),
        (
            "wicked.team.gate.decided",
            "e4b13241087ea34f29f7508ab089b566",
        ),
        ("wicked.team.path.ended", "8741e4e2305194adf64fedb610c1d12a"),
    ];
    let all = fixtures();
    assert_eq!(all.len(), want.len());
    for ((t, payload), (wt, wk)) in all.iter().zip(want) {
        assert_eq!(t, wt);
        let ev = TeamEvent::from_payload(t, payload).unwrap();
        assert_eq!(ev.key().unwrap(), wk, "{t}");
        let row = ev.bus_emit().unwrap();
        assert_eq!(
            (
                row.event_type.as_str(),
                row.domain.as_str(),
                row.subdomain.as_str(),
                row.idempotency_key.as_deref()
            ),
            (wt, "wicked-core", "core.team", Some(wk))
        );
        assert_eq!(&row.payload, payload);
    }
}

#[test]
fn producer_assigned_ids_mint_to_fixed_values() {
    assert_eq!(
        mint_help_id("r1", 3, 1, "claude#1", 2),
        "h-173a7b6f9ad38fce2f578afd2aa1be38"
    );
    assert_eq!(
        mint_change_id("r1", 3, 1, "m1", 1),
        "c-9c29888aed8201cadb49ff6e7f4441aa"
    );
    assert_eq!(
        mint_proposal_id(
            "r1",
            "claude#1",
            &ProposalSource::Understand { ord: 2, attempt: 1 }
        ),
        "p-261ec7e69c0d38e55600d68144fae422"
    );
    assert_eq!(
        mint_proposal_id(
            "r1",
            "claude#1",
            &ProposalSource::PlanBlock {
                ord: 2,
                attempt: 1,
                plan_block_seq: 3
            }
        ),
        "p-942dd40506d967a712f963a4a1d195e7"
    );
    assert_eq!(score_source_diff(3, 1, 2), "diff:3:1:2");
    assert_eq!(score_source_intent("p-x"), "intent:p-x");
    assert_eq!(delivery_id_boundary("build", 2), "boundary:build:2");
    assert_eq!(delivery_id_end(2), "end:2");
    assert_eq!(answered_in("build", 1), "build:1");
    assert_eq!(subject_finding(4), "finding:4");
    assert_eq!(subject_step("test-plan", 1), "step:test-plan:1");
    assert_eq!(gate_id("r1", 3), "g-r1-3");
}

#[test]
fn a_keyed_type_with_a_null_ord_has_no_key() {
    let mut p = fixture(FINDING_RAISED);
    p["ord"] = Value::Null;
    // Refused at parse (it has no key); a hand-built one has no key either.
    assert!(TeamEvent::from_payload(FINDING_RAISED, &p).is_err());
    let mut ev = TeamEvent::from_payload(FINDING_RAISED, &fixture(FINDING_RAISED)).unwrap();
    ev.env.ord = None;
    assert!(ev.key().is_err());
}

// ── (g) the §6.1 identity rule, table-driven over all 25 types ───────────────────────────────────

/// Set the producer-assigned part of a fixture to the `n`th value. Returns the payload keys that
/// part lives in (every other key must be identical between two requests).
type Assign = fn(&mut Value, u32) -> &'static [&'static str];

fn identity_table() -> Vec<(&'static str, Assign)> {
    vec![
        (PATH_STARTED, |p, n| {
            p["run_id"] = json!(format!("r{n}"));
            &["run_id"]
        }),
        (PATH_SCORED, |p, n| {
            p["score_source"] = json!(score_source_diff(3, 1, n));
            &["score_source"]
        }),
        (PLAN_PROPOSED, |p, n| {
            let src = ProposalSource::PlanBlock {
                ord: 2,
                attempt: 1,
                plan_block_seq: n,
            };
            p["proposal_id"] = json!(mint_proposal_id("r1", "claude#1", &src));
            &["proposal_id"]
        }),
        (PLAN_REVISED, |p, n| {
            p["plan_rev"] = json!(n);
            &["plan_rev"]
        }),
        (PLAN_ACCEPTED, |p, n| {
            p["plan_rev"] = json!(n);
            &["plan_rev"]
        }),
        (PLAN_REFUSED, |p, n| {
            let src = ProposalSource::Edit {
                request_id: format!("req-{n}"),
            };
            p["proposal_id"] = json!(mint_proposal_id("r1", "human", &src));
            &["proposal_id"]
        }),
        (MEMBER_JOINED, |p, n| {
            p["open_seq"] = json!(n);
            &["open_seq"]
        }),
        (MEMBER_LEFT, |p, n| {
            p["open_seq"] = json!(n);
            &["open_seq"]
        }),
        (STEP_CLAIMED, |p, n| {
            p["attempt"] = json!(n);
            &["attempt"]
        }),
        (CHECKPOINT_REACHED, |p, n| {
            p["seq"] = json!(n);
            &["seq"]
        }),
        (FINDING_RAISED, |p, n| {
            p["raise_seq"] = json!(n);
            &["raise_seq"]
        }),
        (ADVICE_DELIVERED, |p, n| {
            p["delivery_id"] = json!(delivery_id_boundary("build", n));
            &["delivery_id"]
        }),
        (ADVICE_ANSWERED, |p, n| {
            p["answered_in"] = json!(answered_in("build", n));
            &["answered_in"]
        }),
        (HELP_REQUESTED, |p, n| {
            p["help_seq"] = json!(n);
            p["help_id"] = json!(mint_help_id("r1", 3, 1, "claude#1", n));
            &["help_seq", "help_id"]
        }),
        (HELP_ANSWERED, |p, n| {
            p["answer_id"] = json!(format!("t-{n}"));
            &["answer_id"]
        }),
        (CHANGE_REQUESTED, |p, n| {
            p["change_seq"] = json!(n);
            p["change_id"] = json!(mint_change_id("r1", 3, 1, "m1", n));
            &["change_seq", "change_id"]
        }),
        (STEP_COMPLETED, |p, n| {
            p["attempt"] = json!(n);
            &["attempt"]
        }),
        (STEP_REVIEWED, |p, n| {
            p["attempt"] = json!(n);
            &["attempt"]
        }),
        (FINDING_SETTLED, |p, n| {
            p["raise_seq"] = json!(n);
            &["raise_seq"]
        }),
        (COUNCIL_CALLED, |p, n| {
            p["subject"] = json!(subject_finding(n));
            &["subject"]
        }),
        (COUNCIL_RULED, |p, n| {
            p["subject"] = json!(subject_finding(n));
            &["subject"]
        }),
        (LEDGER_FOLDED, |p, n| {
            p["attempt"] = json!(n);
            &["attempt"]
        }),
        (GATE_OPENED, |p, n| {
            p["gate_id"] = json!(gate_id("r1", n));
            &["gate_id"]
        }),
        (GATE_DECIDED, |p, n| {
            p["gate_id"] = json!(gate_id("r1", n));
            &["gate_id"]
        }),
        (PATH_ENDED, |p, n| {
            p["run_id"] = json!(format!("r{n}"));
            &["run_id"]
        }),
    ]
}

fn tmp_bus(name: &str) -> BusDb {
    let dir = std::env::temp_dir().join(format!(
        "wicked-core-team-events-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    BusDb::shared(dir.join("bus.db").to_str().unwrap()).unwrap()
}

fn emit(bus: &BusDb, t: &str, payload: &Value) -> i64 {
    let ev = TeamEvent::from_payload(t, payload).unwrap_or_else(|e| panic!("{t}: {e:#}"));
    bus.emit(&ev.bus_emit().unwrap()).unwrap()
}

fn differing_keys(a: &Value, b: &Value) -> Vec<String> {
    let (a, b) = (a.as_object().unwrap(), b.as_object().unwrap());
    assert_eq!(
        a.keys().collect::<Vec<_>>(),
        b.keys().collect::<Vec<_>>(),
        "same fields"
    );
    a.iter()
        .filter(|(k, v)| b.get(*k) != Some(*v))
        .map(|(k, _)| k.clone())
        .collect()
}

#[test]
fn two_distinct_requests_are_two_rows_and_a_republish_is_one_for_every_type() {
    let table = identity_table();
    let types: Vec<&str> = table.iter().map(|(t, _)| *t).collect();
    assert_eq!(
        types,
        ALL_TYPES.to_vec(),
        "the table covers every type, in order"
    );
    let bus = tmp_bus("identity");
    for (t, assign) in table {
        let (mut a, mut b) = (fixture(t), fixture(t));
        let part = assign(&mut a, 1);
        assign(&mut b, 2);
        let mut diff = differing_keys(&a, &b);
        diff.sort();
        let mut want: Vec<String> = part.iter().map(|s| s.to_string()).collect();
        want.sort();
        assert_eq!(
            diff, want,
            "{t}: the two requests differ only in the producer part"
        );

        let first = emit(&bus, t, &a);
        let second = emit(&bus, t, &b);
        let again = emit(&bus, t, &a);
        assert_ne!(first, second, "{t}: two distinct requests, two rows");
        assert_eq!(
            again, first,
            "{t}: a re-publish resolves to the existing row"
        );
        let rows = bus.poll(t, 0, 100).unwrap();
        assert_eq!(rows.len(), 2, "{t}: exactly two rows on the bus");
    }
}

/// The named fixtures of §6.1: two `HELP:` lines in one output with the same question and
/// different context are two rows; two member change requests with the same text and different
/// steps are two rows.
#[test]
fn same_question_help_lines_and_same_text_change_requests_do_not_collapse() {
    let bus = tmp_bus("named");
    let mut ids = Vec::new();
    for (seq, context) in [(1, "retire.ts:41"), (2, "coverage.ts:12")] {
        let mut p = fixture(HELP_REQUESTED);
        p["question"] = json!("which table owns coverage?");
        p["context"] = json!(context);
        p["help_seq"] = json!(seq);
        p["help_id"] = json!(mint_help_id("r1", 3, 1, "claude#1", seq));
        ids.push(emit(&bus, HELP_REQUESTED, &p));
    }
    assert_ne!(ids[0], ids[1]);
    assert_eq!(bus.poll(HELP_REQUESTED, 0, 10).unwrap().len(), 2);

    let mut ids = Vec::new();
    for (seq, step) in [(1, "test-plan-2"), (2, "security-review")] {
        let mut p = fixture(CHANGE_REQUESTED);
        p["reason"] = json!("the migration has no test plan");
        p["steps"] = json!([{"catalog": "test_plan", "id": step}]);
        p["change_seq"] = json!(seq);
        p["change_id"] = json!(mint_change_id("r1", 3, 1, "m1", seq));
        ids.push(emit(&bus, CHANGE_REQUESTED, &p));
    }
    assert_ne!(ids[0], ids[1]);
    assert_eq!(bus.poll(CHANGE_REQUESTED, 0, 10).unwrap().len(), 2);
}

/// Payload text fields a key must never be derived from (DES-002 §6.1).
const FORBIDDEN: [&str; 5] = ["question", "claim", "evidence", "reason", "context"];

/// The parameter names of every `fn key_*` / `fn mint_*` in `src`.
fn key_builder_params(src: &str) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    let mut rest = src;
    while let Some(i) = rest.find("fn ") {
        let after = &rest[i + 3..];
        rest = after;
        let name: String = after
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if !(name.starts_with("key_") || name.starts_with("mint_")) {
            continue;
        }
        let open = after.find('(').unwrap();
        let close = after[open..].find(')').unwrap() + open;
        let params = after[open + 1..close]
            .split(',')
            .filter_map(|p| p.split(':').next())
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect();
        out.push((name, params));
    }
    out
}

fn forbidden_in(src: &str) -> Vec<String> {
    // A Windows checkout reads the source with CRLF line endings.
    let src = &src.replace("\r\n", "\n");
    let mut bad = Vec::new();
    for (name, params) in key_builder_params(src) {
        for p in params {
            if FORBIDDEN.contains(&p.as_str()) {
                bad.push(format!("{name}({p})"));
            }
        }
    }
    let dispatch = src
        .split("fn key_parts_of(")
        .nth(1)
        .and_then(|s| s.split("\n}\n").next())
        .unwrap_or("");
    for f in FORBIDDEN {
        if dispatch.contains(&format!(".{f}")) {
            bad.push(format!("key_parts_of reads .{f}"));
        }
    }
    bad
}

#[test]
fn no_key_builder_takes_a_payload_text_field() {
    let src = include_str!("events.rs");
    let builders = key_builder_params(src);
    assert!(
        builders.len() >= 28,
        "the scan found the builders: {builders:?}"
    );
    assert!(src.contains("fn key_parts_of("));
    assert_eq!(forbidden_in(src), Vec::<String>::new());
}

#[test]
fn the_key_builder_guard_catches_a_text_keyed_builder() {
    let bad = "pub fn key_help_requested(run_id: &str, question: &str) -> String { x }\n\
               fn key_parts_of(ev: &TeamEvent) -> Result<String> {\n    y(&b.claim)\n}\n";
    let want = vec![
        "key_help_requested(question)".to_string(),
        "key_parts_of reads .claim".to_string(),
    ];
    assert_eq!(forbidden_in(bad), want);
    // The same source as a Windows checkout reads it, and a clean one after the dispatch ends.
    assert_eq!(forbidden_in(&bad.replace('\n', "\r\n")), want);
    let clean = "fn key_parts_of(ev: &TeamEvent) -> Result<String> {\r\n    y(&b.help_id)\r\n}\r\n\
                 fn later() { z(&b.reason) }\r\n";
    assert_eq!(forbidden_in(clean), Vec::<String>::new());
}

// ── (c) + (d) fold ────────────────────────────────────────────────────────────────────────────────

const EVIDENCE: &str = "fetchCoverage(scope).then(setCount)";

struct Stream {
    rows: Vec<TeamRow>,
}

impl Stream {
    fn new() -> Self {
        Stream { rows: Vec::new() }
    }

    fn push(&mut self, by: &str, body: TeamBody) -> &mut Self {
        let id = 1000 + self.rows.len() as i64;
        self.rows.push(TeamRow {
            event_id: id,
            event: TeamEvent {
                env: Envelope {
                    run_id: "r1".into(),
                    ord: Some(3),
                    attempt: Some(1),
                    by: by.into(),
                    at: 1_758_700_000_000 + id,
                    re: None,
                },
                body,
            },
        });
        self
    }

    fn joined(&mut self, member: &str, seat: &str) -> &mut Self {
        self.push(
            "engine",
            TeamBody::MemberJoined(MemberJoined {
                member_id: member.into(),
                open_seq: 1,
                seat: seat.into(),
                role: tok("monitor"),
                status: tok("attached"),
                reason: "team plan monitors=1".into(),
                error: None,
            }),
        )
    }

    fn left(&mut self, member: &str, seat: &str, batches: u32) -> &mut Self {
        self.push(
            "engine",
            TeamBody::MemberLeft(MemberLeft {
                member_id: member.into(),
                open_seq: 1,
                seat: seat.into(),
                status: tok("completed"),
                batches,
                error: None,
            }),
        )
    }

    fn raised(&mut self, seq: u32, severity: &str) -> &mut Self {
        self.push(
            "claude#2",
            TeamBody::FindingRaised(FindingRaised {
                raise_seq: seq,
                finding_id: fid(seq),
                member_id: "m1".into(),
                line_key: None,
                anchor: None,
                anchor_source: None,
                severity: tok(severity),
                path: "src/retire.ts".into(),
                line: 40 + seq,
                evidence: EVIDENCE.into(),
                claim: "the fetch is never cancelled".into(),
                suggestion: None,
                tree: "t1".into(),
                in_diff: true,
                corroborated_by: vec![],
            }),
        )
    }

    fn delivered(
        &mut self,
        seq: u32,
        delivery_id: &str,
        channel: &str,
        outcome: &str,
    ) -> &mut Self {
        self.push(
            "engine",
            TeamBody::AdviceDelivered(AdviceDelivered {
                raise_seq: seq,
                finding_id: fid(seq),
                delivery_id: delivery_id.into(),
                steer_id: (channel == "acp_steering").then(|| delivery_id.to_string()),
                channel: tok(channel),
                outcome: tok(outcome),
                detail: None,
            }),
        )
    }

    fn injected(&mut self, seq: u32) -> &mut Self {
        self.delivered(seq, &format!("s-{seq}"), "acp_steering", "injected")
    }

    fn answered(&mut self, seq: u32, step: &str, disposition: &str, reason: &str) -> &mut Self {
        self.push(
            "claude#1",
            TeamBody::AdviceAnswered(AdviceAnswered {
                raise_seq: seq,
                answered_in: answered_in(step, 1),
                finding_id: fid(seq),
                disposition: tok(disposition),
                reason: reason.into(),
            }),
        )
    }

    fn settled(
        &mut self,
        seq: u32,
        status: &str,
        reason: &str,
        final_line: Option<u32>,
    ) -> &mut Self {
        self.push(
            "claude#2",
            TeamBody::FindingSettled(FindingSettled {
                raise_seq: seq,
                finding_id: fid(seq),
                status: tok(status),
                reason: reason.into(),
                final_line,
            }),
        )
    }

    fn ruled(&mut self, seq: u32, verdict: &str, reason: Option<&str>) -> &mut Self {
        let convened = verdict != "no_verdict";
        self.push(
            "council:task-9",
            TeamBody::CouncilRuled(CouncilRuled {
                subject: subject_finding(seq),
                verdict: tok(verdict),
                reason: reason.map(tok),
                task_id: convened.then(|| "task-9".to_string()),
                consensus: convened,
                agreement_pct: if convened { 67 } else { 0 },
                dissent: if convened {
                    vec!["the other side".into()]
                } else {
                    vec![]
                },
                returned: if convened { 3 } else { 0 },
                seated: if convened { 3 } else { 0 },
            }),
        )
    }

    fn completed(&mut self, status: &str) -> &mut Self {
        self.push(
            "claude#1",
            TeamBody::StepCompleted(StepCompleted {
                step_id: "build".into(),
                status: tok(status),
                tree: Some("t-final".into()),
                output_bytes: 10,
                output_ref: "unit:r1:3:1".into(),
            }),
        )
    }

    fn fold(&self) -> TeamLedger {
        fold(&self.rows)
    }

    fn fold_json(&self) -> Value {
        serde_json::to_value(self.fold()).unwrap()
    }
}

fn fid(seq: u32) -> String {
    format!("f-{seq:016x}")
}

/// A ledger finding as DES-001 §7 spells it.
#[allow(clippy::too_many_arguments)]
fn lf(
    seq: u32,
    severity: &str,
    final_line: Value,
    delivery: &str,
    status: &str,
    worker_reason: Value,
    monitor_reply: Value,
    dispute: Value,
) -> Value {
    json!({
        "findingId": fid(seq), "monitorId": "m1", "seat": "claude#2", "severity": severity,
        "path": "src/retire.ts", "line": 40 + seq, "evidence": EVIDENCE,
        "claim": "the fetch is never cancelled", "suggestion": null, "tree": "t1", "inDiff": true,
        "finalLine": final_line, "corroboratedBy": [], "delivery": delivery, "status": status,
        "workerReason": worker_reason, "monitorReply": monitor_reply, "dispute": dispute
    })
}

fn ledger(final_pass: &str, team_pause: bool, monitors: Value, findings: Vec<Value>) -> Value {
    json!({
        "finalPass": final_pass, "renderedToJudge": false, "teamPause": team_pause,
        "monitors": monitors, "findings": findings,
        "rejected": {"malformed": 0, "belowBar": 0, "unconfirmed": 0, "duplicate": 0}
    })
}

fn one_monitor(batches: u32) -> Value {
    json!([{"monitorId": "m1", "seat": "claude#2", "batches": batches, "status": "completed", "error": null}])
}

fn hold(reason: &str) -> Value {
    json!({"kind": "hold", "reason": reason})
}

fn dispute_yes_no(verdict: &str) -> Value {
    json!({"verdict": verdict, "agreementPct": 67, "dissent": 1, "seats": [], "reason": null})
}

fn no_verdict(reason: &str) -> Value {
    json!({"verdict": "no_verdict", "agreementPct": null, "dissent": null, "seats": [], "reason": reason})
}

/// DES-001 #8 (S2): a wrapped unit. No mid-turn delivery (R's attempt-end row says
/// `not_delivered` on channel `none`); the hold round's `WITHDRAW` settles it.
#[test]
fn fixture_8_wrapped_unit_final_pass_ledger() {
    let mut s = Stream::new();
    s.joined("m1", "claude#2")
        .raised(1, "high")
        .completed("ok")
        .delivered(1, &delivery_id_end(1), "none", "not_delivered")
        .settled(
            1,
            "withdrawn",
            "the handler is cancelled upstream",
            Some(41),
        )
        .left("m1", "claude#2", 1);
    let want = ledger(
        "completed",
        false,
        one_monitor(1),
        vec![lf(
            1,
            "high",
            json!(41),
            "not_delivered",
            "withdrawn",
            Value::Null,
            json!({"kind": "withdraw", "reason": "the handler is cancelled upstream"}),
            Value::Null,
        )],
    );
    assert_eq!(s.fold_json(), want);
    assert!(unresolved_highs(&s.fold()).is_empty());

    // The same fixture with the monitor holding takes the council path (#16 i).
    let mut held = Stream::new();
    held.joined("m1", "claude#2")
        .raised(1, "high")
        .completed("ok")
        .delivered(1, &delivery_id_end(1), "none", "not_delivered")
        .settled(1, "held", "still uncancelled", Some(41))
        .left("m1", "claude#2", 1);
    let l = held.fold();
    assert_eq!(
        unresolved_highs(&l)
            .iter()
            .map(|f| f.finding.finding_id.clone())
            .collect::<Vec<_>>(),
        vec![fid(1)]
    );
    assert!(l.team_pause, "no council verdict yet is not a YES");
}

/// DES-001 #8 (S3): a steer answered `promptRequired` is `turn_ended`; the finding is
/// `not_delivered`.
#[test]
fn fixture_8_prompt_required_steer_is_not_delivered() {
    let mut s = Stream::new();
    s.joined("m1", "claude#2")
        .raised(1, "high")
        .delivered(1, "s-1", "acp_steering", "turn_ended")
        .completed("ok")
        .left("m1", "claude#2", 1);
    let l = s.fold_json();
    assert_eq!(l["findings"][0]["delivery"], json!("not_delivered"));
    assert_eq!(l["findings"][0]["status"], json!("unanswered"));
}

/// DES-001 #11: `ADVICE` lines become dispositions with their reasons; a delivered id with no
/// line ends `unanswered`; a missing hold-round reply records a HOLD.
#[test]
fn fixture_11_advice_lines_and_the_hold_round() {
    let mut s = Stream::new();
    s.joined("m1", "claude#2")
        .raised(1, "high")
        .raised(2, "high")
        .raised(3, "high")
        .injected(1)
        .injected(2)
        .injected(3)
        .completed("ok")
        .answered(
            1,
            "build",
            "declined",
            "campaign.rs:325 documents the exclusion",
        )
        .answered(2, "build", "accepted", "added AbortController")
        .settled(1, "held", "the exclusion does not cover retire", Some(41))
        .settled(3, "held", "no reply (counted as hold)", Some(43))
        .left("m1", "claude#2", 2);
    let want = ledger(
        "completed",
        true,
        one_monitor(2),
        vec![
            lf(
                1,
                "high",
                json!(41),
                "injected",
                "declined",
                json!("campaign.rs:325 documents the exclusion"),
                hold("the exclusion does not cover retire"),
                Value::Null,
            ),
            lf(
                2,
                "high",
                Value::Null,
                "injected",
                "accepted",
                json!("added AbortController"),
                Value::Null,
                Value::Null,
            ),
            lf(
                3,
                "high",
                json!(43),
                "injected",
                "unanswered",
                Value::Null,
                hold("no reply (counted as hold)"),
                Value::Null,
            ),
        ],
    );
    assert_eq!(s.fold_json(), want);
    let l = s.fold();
    // Only the unaccepted findings go to the hold round.
    assert_eq!(
        l.findings.iter().map(is_unaccepted).collect::<Vec<_>>(),
        vec![true, false, true]
    );
}

fn base_high(status: Option<(&str, &str)>, settle: Option<&str>) -> Stream {
    let mut s = Stream::new();
    s.joined("m1", "claude#2")
        .raised(1, "high")
        .injected(1)
        .completed("ok");
    if let Some((d, r)) = status {
        s.answered(1, "build", d, r);
    }
    match settle {
        Some("held") => s.settled(1, "held", "the finding stands", Some(41)),
        Some(other) => s.settled(1, other, "text gone or withdrawn", None),
        None => &mut s,
    };
    s.left("m1", "claude#2", 1);
    s
}

fn council_count(s: &Stream) -> usize {
    unresolved_highs(&s.fold()).len()
}

/// DES-001 #15: exactly one council for a HIGH that is unaccepted and held; none when any one
/// of the three is removed.
#[test]
fn fixture_15_council_trigger() {
    let declined = Some(("declined", "documented"));
    assert_eq!(council_count(&base_high(declined, Some("held"))), 1);
    // Held by silence: no hold-round reply counts as HOLD.
    assert_eq!(council_count(&base_high(declined, None)), 1);
    // Unanswered (injected, no ADVICE line) and not delivered: each convenes one.
    assert_eq!(council_count(&base_high(None, Some("held"))), 1);
    let mut undelivered = Stream::new();
    undelivered
        .joined("m1", "claude#2")
        .raised(1, "high")
        .delivered(1, &delivery_id_end(1), "none", "not_delivered")
        .settled(1, "held", "the finding stands", Some(41));
    assert_eq!(council_count(&undelivered), 1);

    // Remove one condition at a time.
    let mut medium = Stream::new();
    medium
        .joined("m1", "claude#2")
        .raised(1, "medium")
        .injected(1)
        .answered(1, "build", "declined", "documented")
        .settled(1, "held", "the finding stands", Some(41));
    assert_eq!(council_count(&medium), 0, "MEDIUM");
    assert_eq!(
        council_count(&base_high(Some(("accepted", "fixed")), Some("held"))),
        0,
        "accepted"
    );
    assert_eq!(
        council_count(&base_high(declined, Some("withdrawn"))),
        0,
        "WITHDRAW"
    );
    assert_eq!(
        council_count(&base_high(declined, Some("superseded"))),
        0,
        "superseded"
    );
}

fn with_ruling(mut s: Stream, verdict: &str, reason: Option<&str>) -> Stream {
    s.ruled(1, verdict, reason);
    s
}

/// DES-001 #16: continue on a council YES only; every other outcome pauses.
#[test]
fn fixture_16_continue_or_pause() {
    let declined = || base_high(Some(("declined", "documented")), Some("held"));

    // (a) YES → no pause.
    let a = with_ruling(declined(), "yes", None).fold();
    assert!(!a.team_pause);
    assert_eq!(
        serde_json::to_value(&a.findings[0].dispute).unwrap(),
        dispute_yes_no("yes")
    );
    assert!(!gate_pauses(true, &a));

    // (b) NO → human pause.
    let b = with_ruling(declined(), "no", None).fold();
    assert!(b.team_pause);
    assert_eq!(
        serde_json::to_value(&b.findings[0].dispute).unwrap(),
        dispute_yes_no("no")
    );
    assert!(gate_pauses(true, &b));

    // (c) no verdict, every reason → the same pause as (b).
    for reason in ["no_quorum", "seats_benched", "error", "timeout", "cap"] {
        let c = with_ruling(declined(), "no_verdict", Some(reason)).fold();
        assert!(c.team_pause, "{reason}");
        assert_eq!(
            serde_json::to_value(&c.findings[0].dispute).unwrap(),
            no_verdict(reason)
        );
        assert!(gate_pauses(true, &c), "{reason}");
    }

    // (d) A skipped judge approves (`combine_verdict(true, None)`); the fold reads no judge, so
    // the ledger is identical and the gate still pauses on (b) and (c).
    assert!(gate_pauses(true, &b));

    // (e) The floor failing denies the unit: no team_dispute pause.
    assert!(!gate_pauses(false, &b));

    // (h) Unanswered (injected, no ADVICE line), held or silent.
    for settle in [Some("held"), None] {
        let yes = with_ruling(base_high(None, settle), "yes", None).fold();
        assert!(!yes.team_pause, "(h) yes, {settle:?}");
        let no = with_ruling(base_high(None, settle), "no", None).fold();
        assert!(no.team_pause, "(h) no, {settle:?}");
        for reason in ["no_quorum", "seats_benched", "error", "timeout", "cap"] {
            let nv = with_ruling(base_high(None, settle), "no_verdict", Some(reason)).fold();
            assert!(nv.team_pause, "(h) {reason}, {settle:?}");
        }
    }

    // (i) Not delivered: no steering channel at all, and a steer answered promptRequired.
    let undelivered = |outcome: &str| {
        let mut s = Stream::new();
        s.joined("m1", "claude#2").raised(1, "high");
        if outcome == "turn_ended" {
            s.delivered(1, "s-1", "acp_steering", "turn_ended");
        }
        s.delivered(1, &delivery_id_end(1), "none", "not_delivered")
            .completed("ok")
            .settled(1, "held", "the finding stands", Some(41));
        s
    };
    for outcome in ["none", "turn_ended"] {
        let yes = with_ruling(undelivered(outcome), "yes", None).fold();
        assert_eq!(yes.findings[0].delivery, LedgerDelivery::NotDelivered);
        assert!(!yes.team_pause, "(i) yes {outcome}");
        assert!(
            with_ruling(undelivered(outcome), "no", None)
                .fold()
                .team_pause
        );
        assert!(
            with_ruling(undelivered(outcome), "no_verdict", Some("timeout"))
                .fold()
                .team_pause
        );
    }

    // (j) Final-pass timeout: no hold reply, no council. Synthesized fail-closed; recorded
    // results kept; MEDIUM untouched.
    let mut s = Stream::new();
    s.joined("m1", "claude#2")
        .raised(1, "high")
        .raised(2, "medium")
        .raised(3, "high")
        .injected(1)
        .injected(3)
        .answered(1, "build", "declined", "documented")
        .answered(3, "build", "declined", "documented")
        .settled(3, "held", "the finding stands", Some(43))
        .ruled(3, "yes", None);
    let j = synthesize_timeout(s.fold());
    let want = ledger(
        "timed_out",
        true,
        json!([{"monitorId": "m1", "seat": "claude#2", "batches": 0, "status": "timed_out",
                "error": "no member.left for this opening"}]),
        vec![
            lf(
                1,
                "high",
                Value::Null,
                "injected",
                "declined",
                json!("documented"),
                hold("no reply (final pass timed out)"),
                no_verdict("timeout"),
            ),
            lf(
                2,
                "medium",
                Value::Null,
                "not_delivered",
                "unanswered",
                Value::Null,
                Value::Null,
                Value::Null,
            ),
            lf(
                3,
                "high",
                json!(43),
                "injected",
                "declined",
                json!("documented"),
                hold("the finding stands"),
                dispute_yes_no("yes"),
            ),
        ],
    );
    assert_eq!(serde_json::to_value(&j).unwrap(), want);
    assert!(gate_pauses(true, &j), "a timeout pauses; never unitDone");

    // (f), (g), (k) are the pipeline's and the actor's (combine_verdict's inputs, confirm_gate's
    // approve and amend arms). The fold's part is the ledger they start from: (b)'s, with
    // teamPause set, which the assertions above pin.
}

#[test]
fn a_failed_step_skips_the_final_pass() {
    let mut s = Stream::new();
    s.joined("m1", "claude#2")
        .raised(1, "medium")
        .completed("failed");
    assert_eq!(s.fold().final_pass, FinalPass::Skipped);
}

#[test]
fn the_latest_advice_line_wins() {
    let mut s = Stream::new();
    s.joined("m1", "claude#2")
        .raised(1, "high")
        .injected(1)
        .answered(1, "build", "declined", "first thought")
        .answered(1, "review", "accepted", "fixed after all");
    let l = s.fold();
    assert_eq!(
        (
            l.findings[0].status.as_str(),
            l.findings[0].worker_reason.as_deref()
        ),
        ("accepted", Some("fixed after all"))
    );
}

/// (d) Re-delivered rows, a re-published row (same key, same event id) and any delivery order
/// fold to the same ledger.
#[test]
fn fold_is_idempotent_under_duplicates_and_order() {
    let mut s = Stream::new();
    s.joined("m1", "claude#2")
        .raised(1, "high")
        .raised(2, "medium")
        .injected(1)
        .answered(1, "build", "declined", "documented")
        .completed("ok")
        .settled(1, "held", "the finding stands", Some(41))
        .ruled(1, "no", None)
        .left("m1", "claude#2", 3);
    let once = s.fold();

    let mut twice = s.rows.clone();
    twice.extend(s.rows.iter().rev().cloned());
    assert_eq!(fold(&twice), once);

    let mut reversed = s.rows.clone();
    reversed.reverse();
    assert_eq!(fold(&reversed), once);

    // A second copy of a fact under a later event id (a replay that bypassed the bus's dedup)
    // folds once: rows are deduplicated by key, the first row wins.
    let mut replayed = s.rows.clone();
    let mut late = s.rows[4].clone();
    late.event_id = 9_999;
    if let TeamBody::AdviceAnswered(b) = &mut late.event.body {
        b.reason = "a replayed copy".into();
    }
    replayed.push(late);
    assert_eq!(fold(&replayed), once);

    assert_eq!(
        fold(&[]),
        TeamLedger {
            final_pass: FinalPass::Completed,
            rendered_to_judge: false,
            monitors: vec![],
            findings: vec![],
            rejected: Default::default(),
            team_pause: false,
        }
    );
}

// ── Fail-closed parsing and folding (review of #616) ─────────────────────────────────────────────

/// A severity the bar does not know is refused at parse: it can never reach the fold as a
/// finding that silently vanishes.
#[test]
fn an_unknown_severity_is_rejected_at_parse() {
    for bad in ["hihg", "low", "HIGH", ""] {
        let mut p = fixture(FINDING_RAISED);
        p["severity"] = json!(bad);
        assert!(
            TeamEvent::from_payload(FINDING_RAISED, &p).is_err(),
            "severity {bad:?} must not parse"
        );
    }
}

/// Every enum §6 documents is a closed set on the wire: an unknown token is refused at parse.
#[test]
fn an_unknown_token_in_any_enum_field_is_rejected_at_parse() {
    let cases: [(&str, &str); 24] = [
        (PATH_STARTED, "selection"),
        (PATH_SCORED, "basis"),
        (PLAN_PROPOSED, "kind"),
        (PLAN_REVISED, "reason"),
        (PLAN_ACCEPTED, "mode"),
        (MEMBER_JOINED, "role"),
        (MEMBER_JOINED, "status"),
        (MEMBER_LEFT, "status"),
        (CHECKPOINT_REACHED, "status"),
        (FINDING_RAISED, "severity"),
        (ADVICE_DELIVERED, "channel"),
        (ADVICE_DELIVERED, "outcome"),
        (ADVICE_ANSWERED, "disposition"),
        (STEP_COMPLETED, "status"),
        (STEP_REVIEWED, "verdict"),
        (STEP_REVIEWED, "to"),
        (FINDING_SETTLED, "status"),
        (COUNCIL_CALLED, "trigger"),
        (COUNCIL_RULED, "verdict"),
        (LEDGER_FOLDED, "final_pass"),
        (LEDGER_FOLDED, "transport"),
        (GATE_DECIDED, "kind"),
        (GATE_DECIDED, "decision"),
        (PATH_ENDED, "status"),
    ];
    for (t, field) in cases {
        let mut p = fixture(t);
        assert!(p.get(field).is_some(), "{t}.{field} exists");
        p[field] = json!("not_a_token");
        assert!(
            TeamEvent::from_payload(t, &p).is_err(),
            "{t}.{field} = \"not_a_token\" must not parse"
        );
    }
    let nested: [(&str, &[&str]); 8] = [
        (PATH_SCORED, &["plan", "depth"]),
        (PLAN_PROPOSED, &["steps", "2", "owner"]),
        (PLAN_ACCEPTED, &["steps", "0", "added_by"]),
        (GATE_OPENED, &["ledger_source"]),
        (LEDGER_FOLDED, &["ledger", "findings", "0", "status"]),
        (LEDGER_FOLDED, &["ledger", "findings", "0", "delivery"]),
        (LEDGER_FOLDED, &["ledger", "monitors", "0", "status"]),
        (
            LEDGER_FOLDED,
            &["ledger", "findings", "0", "dispute", "verdict"],
        ),
    ];
    for (t, path) in nested {
        let mut p = fixture(t);
        let mut slot = &mut p;
        for seg in path {
            slot = match seg.parse::<usize>() {
                Ok(i) => &mut slot[i],
                Err(_) => &mut slot[*seg],
            };
        }
        assert!(!slot.is_null(), "{t}.{path:?} exists");
        *slot = json!("not_a_token");
        assert!(
            TeamEvent::from_payload(t, &p).is_err(),
            "{t}.{path:?} = \"not_a_token\" must not parse"
        );
    }
}

/// A row that names a finding the stream never raised is not dropped: the record is incomplete,
/// so the fold says `stream_gap` and pauses (DES-002 §4.7: an incomplete team record goes to a
/// human, never auto-approved). Every raised HIGH is in the ledger.
#[test]
fn a_high_never_disappears_and_an_orphan_row_is_a_stream_gap() {
    let mut s = Stream::new();
    s.joined("m1", "claude#2")
        .raised(1, "high")
        .raised(2, "high")
        .injected(2)
        .answered(2, "build", "accepted", "fixed")
        // raise_seq 7 was never raised on this stream (lost, or aged out):
        .answered(7, "build", "declined", "documented")
        .completed("ok")
        .left("m1", "claude#2", 1);
    let l = s.fold_json();
    assert_eq!(
        l["findings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["findingId"].clone())
            .collect::<Vec<_>>(),
        vec![json!(fid(1)), json!(fid(2))],
        "every raised HIGH is in the ledger"
    );
    assert_eq!(l["finalPass"], json!("stream_gap"));
    assert_eq!(l["teamPause"], json!(true));

    // Each orphan kind is a gap: a delivery, a settle and a council ruling on an unraised finding.
    for orphan in ["delivered", "settled", "ruled"] {
        let mut s = Stream::new();
        s.joined("m1", "claude#2")
            .raised(1, "medium")
            .completed("ok");
        match orphan {
            "delivered" => s.injected(9),
            "settled" => s.settled(9, "withdrawn", "gone", None),
            _ => s.ruled(9, "yes", None),
        };
        let l = s.fold_json();
        assert_eq!(l["finalPass"], json!("stream_gap"), "{orphan}");
        assert_eq!(l["teamPause"], json!(true), "{orphan}");
    }
}

// ── Rows must name the finding they claim to be about (review of #616, round 2) ──────────────────

/// codex's scenario 1: an `advice.answered{raise_seq:1, finding_id:"f-other", accepted}` must not
/// be applied to the real HIGH raised at `raise_seq` 1. A row whose `finding_id` is not the raised
/// finding's is a stream gap, and the HIGH keeps the ledger paused.
#[test]
fn a_row_naming_another_finding_id_is_not_applied_and_is_a_gap() {
    // The honest stream; for `settled` it carries no settle of its own, so the forged row is the
    // finding's only one (a same-key settle would be deduplicated, first row wins).
    let base = |settle: bool| {
        let mut s = Stream::new();
        s.joined("m1", "claude#2")
            .raised(1, "high")
            .injected(1)
            .answered(1, "build", "declined", "documented");
        if settle {
            s.settled(1, "held", "the finding stands", Some(41));
        }
        s
    };
    assert!(base(true).fold().team_pause, "the real HIGH is unresolved");
    assert!(base(false).fold().team_pause, "held by silence");

    // The same stream plus a row that claims raise_seq 1 but names another finding.
    for kind in ["answered", "delivered", "settled"] {
        let mut s = base(kind != "settled");
        match kind {
            "answered" => s.answered(1, "review", "accepted", "fixed"),
            "delivered" => s.delivered(1, "s-other", "acp_steering", "injected"),
            _ => s.settled(1, "withdrawn", "not mine", None),
        };
        let last = s.rows.last_mut().unwrap();
        match &mut last.event.body {
            TeamBody::AdviceAnswered(b) => b.finding_id = "f-other".into(),
            TeamBody::AdviceDelivered(b) => b.finding_id = "f-other".into(),
            TeamBody::FindingSettled(b) => b.finding_id = "f-other".into(),
            _ => unreachable!(),
        }
        let l = s.fold();
        let f = &l.findings[0];
        assert_eq!(f.finding.finding_id, fid(1), "{kind}");
        assert_eq!(
            (f.status, f.worker_reason.as_deref()),
            (FindingStatus::Declined, Some("documented")),
            "{kind}: the mismatched row is not applied"
        );
        assert_eq!(l.final_pass, FinalPass::StreamGap, "{kind}");
        assert!(l.team_pause, "{kind}: the pause stands");
    }
}

/// codex's scenario 2: a keyed row with a `null` `ord` (its key cannot be built) is refused at
/// parse, and a row that reaches the fold without a key is a stream gap that is never applied.
#[test]
fn a_keyless_row_is_refused_at_parse_and_never_applied_by_the_fold() {
    let mut p = fixture(ADVICE_ANSWERED);
    p["ord"] = Value::Null;
    assert!(
        TeamEvent::from_payload(ADVICE_ANSWERED, &p).is_err(),
        "advice.answered with ord:null has no key: refused"
    );
    for t in ALL_TYPES {
        if matches!(t, PATH_STARTED | PATH_ENDED) {
            continue;
        }
        let mut p = fixture(t);
        if p["ord"].is_null() {
            continue; // keyed on run-level ids only
        }
        p["ord"] = Value::Null;
        p["attempt"] = Value::Null;
        let parsed = TeamEvent::from_payload(t, &p);
        if let Ok(ev) = &parsed {
            assert!(ev.key().is_ok(), "{t}: a row that parses has a key");
        }
    }

    let mut s = Stream::new();
    s.joined("m1", "claude#2")
        .raised(1, "high")
        .injected(1)
        .answered(1, "build", "declined", "documented")
        .settled(1, "held", "the finding stands", Some(41))
        // A second answer that would clear the pause, built past the parser with no key.
        .answered(1, "review", "accepted", "fixed");
    s.rows.last_mut().unwrap().event.env.ord = None;
    assert!(s.rows.last().unwrap().event.key().is_err());
    let l = s.fold();
    assert_eq!(
        (l.findings[0].status, l.findings[0].worker_reason.as_deref()),
        (FindingStatus::Declined, Some("documented")),
        "the keyless row is not applied"
    );
    assert_eq!(l.final_pass, FinalPass::StreamGap);
    assert!(l.team_pause);
}

/// `finding_id` is a content hash, so attempt 2's copy of a finding has the same id: a row of
/// another attempt that reuses the `raise_seq` is not about this attempt's finding.
#[test]
fn a_row_from_another_attempt_is_not_applied_and_is_a_gap() {
    let mut s = Stream::new();
    s.joined("m1", "claude#2")
        .raised(1, "high")
        .injected(1)
        .answered(1, "build", "declined", "documented")
        .settled(1, "held", "the finding stands", Some(41))
        .ruled(1, "no", None)
        .ruled(1, "yes", None);
    // The YES belongs to attempt 2 (same subject, same finding id).
    s.rows.last_mut().unwrap().event.env.attempt = Some(2);
    let l = s.fold();
    assert_eq!(
        l.findings[0].dispute.as_ref().map(|d| d.verdict),
        Some(crate::team::Verdict::No),
        "attempt 2's ruling is not applied"
    );
    assert_eq!(l.final_pass, FinalPass::StreamGap);
    assert!(l.team_pause);
}
