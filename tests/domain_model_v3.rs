//! J6 C2 (DES-C2-v2): the 1.2.0 domain model, built from the rows garden's `domain-run` really
//! writes. Every store here is a real-shape fixture: `tests/fixtures/domain_v3/<tree>.sql` is the
//! estate store after garden's own route (`run.run` → `store.rows_for` → `wicked-estate
//! write-batch`) wrote one run over the tree beside it; its header names the garden commit, the
//! estate version, the generator and the tree hashes. No annotation here is hand-written except a
//! ruling (core's own writer) and the tombstones and old-loop rows the policy tests add, each in the
//! shape its writer uses (`store.py:373-379`; `Annotation::new` as the old loop did).

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use wicked_apps_core::{GraphRead, GraphWrite};
use wicked_governance::{
    build_and_persist, build_extract_model, statement_key, write_ruling, ExtractIndex,
    RulingRequest,
};

const BIN: &str = env!("CARGO_BIN_EXE_wicked-core");

/// Every emission this binary triggers spools into the per-process test outbox (core#311, core#749).
///
/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only sets one process env var
/// via the std API — no allocator setup, no threads, no panics across the FFI boundary.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}

const SCHEMA: &str = include_str!("../crates/wicked-governance/schemas/domain-model.schema.json");
const READ_PARA: &str = "ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#READ-PARA().";
const MAIN_PARA: &str = "ts-cobol . . . app/cbl/PAYCALC1/PAYCALC1#MAIN-PARA().";
const PROG: &str = "app/cbl/PAYCALC1.cbl";

/// A scratch copy of fixture `name`: its tree and its store (the SQL dump executed into a new file).
struct Fixture {
    dir: PathBuf,
    tree: PathBuf,
    db: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let p = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &p);
        } else {
            std::fs::copy(e.path(), p).unwrap();
        }
    }
}

fn fixture(name: &str, tag: &str) -> Fixture {
    fixture_of(name, name, tag)
}

/// The store dump `store.sql` over a copy of the tree `name` (one tree, several runs over it).
fn fixture_of(name: &str, store: &str, tag: &str) -> Fixture {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/domain_v3");
    let dir = std::env::temp_dir().join(format!("wicked-core-dm3-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let tree = dir.join(name);
    copy_dir(&src.join(name), &tree);
    let db = dir.join("estate.db");
    let sql = std::fs::read_to_string(src.join(format!("{store}.sql"))).unwrap();
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch(&sql)
        .unwrap();
    Fixture { dir, tree, db }
}

fn open(f: &Fixture) -> wicked_apps_core::SqliteStore {
    wicked_apps_core::open_store(Some(f.db.to_str().unwrap())).unwrap()
}

fn build(f: &Fixture) -> Value {
    let store = open(f);
    serde_json::to_value(build_extract_model(&store, &f.tree, 7).unwrap()).unwrap()
}

fn validates(doc: &Value) {
    let schema: Value = serde_json::from_str(SCHEMA).unwrap();
    let compiled = jsonschema::JSONSchema::compile(&schema).unwrap();
    let errors: Vec<String> = match compiled.validate(doc) {
        Ok(()) => Vec::new(),
        Err(e) => e.map(|e| format!("{e} @ {}", e.instance_path)).collect(),
    };
    assert!(errors.is_empty(), "{}\n{doc:#}", errors.join("\n"));
}

/// Every statement of the model, by id: the requirements' and `metadata.statements` (as crew
/// reads them, `domainModelOf`). An id held twice fails here.
fn rules(doc: &Value) -> std::collections::BTreeMap<String, Value> {
    let mut out = std::collections::BTreeMap::new();
    for b in held_rules(doc).into_iter().chain(
        doc["metadata"]["statements"]
            .as_array()
            .cloned()
            .unwrap_or_default(),
    ) {
        let id = b["id"].as_str().unwrap().to_string();
        assert!(out.insert(id.clone(), b).is_none(), "{id} is held twice");
    }
    out
}

/// The statements the requirements hold.
fn held_rules(doc: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    for d in doc["domains"].as_object().unwrap().values() {
        for r in d["requirements"].as_object().unwrap().values() {
            out.extend(r["business_rules"].as_array().unwrap().iter().cloned());
        }
    }
    out
}

/// The ids of the statements the requirements hold, and of those in `metadata.statements`.
fn split(doc: &Value) -> (Vec<String>, Vec<String>) {
    let ids = |v: Vec<Value>| -> Vec<String> {
        v.iter()
            .map(|b| b["id"].as_str().unwrap().to_string())
            .collect()
    };
    (
        ids(held_rules(doc)),
        ids(doc["metadata"]["statements"]
            .as_array()
            .cloned()
            .unwrap_or_default()),
    )
}

fn sid(n: u32) -> String {
    format!("{READ_PARA}#{n}")
}

fn target_of(doc: &Value, id: &str) -> Value {
    doc["metadata"]["targets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == id)
        .cloned()
        .unwrap_or_else(|| panic!("no target {id}"))
}

/// A ruling on statement `n` as a client makes it, from the document alone: the key it read (on
/// the statement, or on its target's `rejected` list once rejected) and the target's hash.
fn rule_on(f: &Fixture, n: u32, action: &str, note: Option<&str>) -> Result<Value, String> {
    let doc = build(f);
    let key = rules(&doc)
        .get(&sid(n))
        .map(|r| r["statement_key"].clone())
        .or_else(|| {
            target_of(&doc, READ_PARA)["rejected"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["id"] == sid(n))
                .map(|r| r["statement_key"].clone())
        })
        .and_then(|k| k.as_str().map(String::from))
        .expect("the document names the statement's key");
    let req = RulingRequest {
        statement: Some(sid(n)),
        expect_key: Some(key),
        expect_target_hash: target_of(&doc, READ_PARA)["target_hash"]
            .as_str()
            .unwrap()
            .to_string(),
        action: action.into(),
        note: note.map(String::from),
        by: "ana".into(),
        ..Default::default()
    };
    let mut store = open(f);
    write_ruling(&mut store, &f.tree, &req, 100)
        .unwrap()
        .map_err(|r| format!("{}: {}", r.code, r.message))
}

/// The real run's model: its four READ-PARA statements with their statuses and claims, its group
/// with the detail the prose check withheld, its four follow-ups in the writer's order, its four
/// targets and the accounting; the document validates against schema 1.2.0.
#[test]
fn the_real_runs_store_builds_its_statements_groups_followups_and_targets() {
    let f = fixture("mini-cobol", "real");
    let doc = build(&f);
    validates(&doc);
    assert_eq!(doc["metadata"]["schema_version"], "1.2.0");
    assert_eq!(doc["metadata"]["built_at"], 7);
    // One file domain, labelled by its group's area; one requirement on READ-PARA.
    let domains = doc["domains"].as_object().unwrap();
    assert_eq!(domains.keys().collect::<Vec<_>>(), vec![PROG]);
    assert_eq!(domains[PROG]["description"], "PAYCALC1");
    let reqs = domains[PROG]["requirements"].as_object().unwrap();
    assert_eq!(reqs.len(), 1);
    let req = reqs.values().next().unwrap();
    assert_eq!(req["title"], "Overtime pay");
    // garden#1545: the requirement holds the statement the business rule cites; the three open
    // statements no sentence cites are the model's other statements (each has its follow-up).
    assert_eq!(split(&doc), (vec![sid(0)], vec![sid(1), sid(2), sid(3)]));
    let rs = rules(&doc);
    let status = |n| rs[&sid(n)]["status"].as_str().unwrap().to_string();
    assert_eq!(
        (status(0), status(1), status(2), status(3)),
        (
            "Checked".into(),
            "open".into(),
            "open".into(),
            "open".into()
        )
    );
    let checked = &rs[&sid(0)];
    assert_eq!(checked["source_ref"], format!("{PROG}#L20-L29"));
    assert_eq!(checked["checked_by"], "codex");
    assert_eq!(checked["confidence"], 0.9);
    assert_eq!(checked["located"], true);
    assert_eq!(
        checked["claims"][1],
        json!({"i": 1, "type": "effect", "text": "`PAY-AMOUNT` gets the overtime rate",
               "when": [0], "ranges": [{"file": PROG, "from": 25, "to": 26}]})
    );
    assert!(rs[&sid(1)].get("checked_by").is_none());
    assert!(checked["statement_key"]
        .as_str()
        .unwrap()
        .starts_with("v1:"));
    // The group: its rule cites the Checked statement; the detail the checker flagged stays withheld.
    let groups = doc["metadata"]["rule_groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["rule"]["cites"], json!([sid(0)]));
    assert_eq!(groups[0]["details"], json!([]));
    assert_eq!(groups[0]["withheld"][0]["id"], "g1.d1");
    assert_eq!(groups[0]["stale"], false);
    assert!(groups[0]["id"].as_str().unwrap().len() == 16);
    // Follow-ups in the writer's order, with their store keys.
    let fu = doc["metadata"]["followups"].as_array().unwrap();
    let kinds: Vec<&str> = fu.iter().map(|r| r["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, vec!["flagged", "open", "open", "refused"]);
    assert!(fu
        .iter()
        .all(|r| r["id"].as_str().is_some_and(|i| i.len() == 16)));
    // Targets: the two JCL ones are accounted (no decision); READ-PARA has open statements;
    // MAIN-PARA's lines are not accounted for.
    let acc = &doc["metadata"]["accounting"];
    assert_eq!(
        acc,
        &json!({"targets": 4, "accounted": 2, "open": 3, "stale": 0})
    );
    assert_eq!(target_of(&doc, READ_PARA)["accounted"], false);
    let main = target_of(&doc, MAIN_PARA);
    assert_eq!(main["accounted"], false);
    assert!(main["unaccounted_reasons"][0]
        .as_str()
        .unwrap()
        .starts_with("M7"));
    assert_eq!(
        doc["metadata"]["skipped"],
        json!({"retired": 0, "malformed": 0, "old_loop": 0})
    );
}

/// TypeScript: a file domain labelled by its groups' area, the business group before the
/// housekeeping group the writer numbered first.
#[test]
fn a_typescript_run_orders_housekeeping_last() {
    let f = fixture("mini-ts", "ts");
    let doc = build(&f);
    validates(&doc);
    let d = &doc["domains"]["src/core/launch-posture.ts"];
    assert_eq!(d["description"], "launch-posture");
    let groups = doc["metadata"]["rule_groups"].as_array().unwrap();
    let kinds: Vec<(&str, i64)> = groups
        .iter()
        .map(|g| (g["kind"].as_str().unwrap(), g["g"].as_i64().unwrap()))
        .collect();
    assert_eq!(kinds, vec![("business", 2), ("housekeeping", 1)]);
    assert_eq!(doc["metadata"]["accounting"]["accounted"], 1);
}

/// garden#1545, on the train-16 cert's shape (`mini-cobol-cert.sql`, gen_cert.py): MAIN-PARA's
/// file open is cited only by a housekeeping rule the prose check withheld (the cert's 22 open and
/// close paragraphs, g15.r/g16.r), its file close only by a shown housekeeping rule, READ-PARA's
/// open statement by nothing. None of them is a requirement; each stays readable by id, so every
/// group cite and every follow-up resolves.
#[test]
fn a_statement_is_a_requirement_only_when_a_shown_business_sentence_cites_it() {
    let f = fixture_of("mini-cobol", "mini-cobol-cert", "cert");
    let doc = build(&f);
    validates(&doc);
    let main = |n: u32| format!("{MAIN_PARA}#{n}");
    let groups = doc["metadata"]["rule_groups"].as_array().unwrap();
    let shape: Vec<(&str, bool, usize)> = groups
        .iter()
        .map(|g| {
            (
                g["kind"].as_str().unwrap(),
                g["rule"].is_null(),
                g["withheld"].as_array().unwrap().len(),
            )
        })
        .collect();
    assert_eq!(
        shape,
        vec![
            ("business", false, 0),
            ("housekeeping", true, 1),
            ("housekeeping", false, 0)
        ],
        "the store holds the cert's shape"
    );
    assert_eq!(groups[1]["withheld"][0]["cites"], json!([main(0)]));
    assert_eq!(groups[2]["rule"]["cites"], json!([main(1)]));
    // One requirement: READ-PARA's, holding the statement the business rule cites.
    let reqs = doc["domains"][PROG]["requirements"].as_object().unwrap();
    assert_eq!(reqs.len(), 1, "{reqs:#?}");
    let req = reqs.values().next().unwrap();
    assert_eq!(req["title"], "Overtime pay");
    assert_eq!(doc["domains"][PROG]["description"], "PAYCALC1");
    assert_eq!(
        split(&doc),
        (vec![sid(0)], vec![main(0), main(1), sid(1)]),
        "the open and close are not requirements; they and the open statement are the others"
    );
    // Each keeps its status, key and claims where crew reads it.
    let rs = rules(&doc);
    assert_eq!(
        [&rs[&main(0)], &rs[&main(1)], &rs[&sid(1)]].map(|r| r["status"].as_str().unwrap()),
        ["Checked", "Checked", "open"]
    );
    assert!(rs
        .values()
        .all(|r| r["statement_key"].as_str().unwrap().starts_with("v1:")));
    // Every cite of every sentence, shown or withheld, and every follow-up's statement resolves.
    for g in groups {
        for s in std::iter::once(&g["rule"])
            .filter(|r| !r.is_null())
            .chain(g["details"].as_array().unwrap())
            .chain(g["withheld"].as_array().unwrap())
        {
            for c in s["cites"].as_array().unwrap() {
                assert!(rs.contains_key(c.as_str().unwrap()), "cite {c} resolves");
            }
        }
    }
    for fu in doc["metadata"]["followups"].as_array().unwrap() {
        if let Some(s) = fu["statement"].as_str() {
            assert!(rs.contains_key(s), "follow-up statement {s} resolves");
        }
    }
    // The open statement still takes a ruling from what the document says, and stays where it is.
    rule_on(&f, 1, "confirm", None).unwrap();
    let after = build(&f);
    assert_eq!(split(&after), split(&doc));
    assert_eq!(rules(&after)[&sid(1)]["status"], "confirmed");
    // The persisted model reads back whole, its other statements included; one requirement node.
    let mut store = open(&f);
    let (model, outcome) = build_and_persist(&mut store, &f.tree, 9).unwrap();
    assert_eq!(outcome.requirements, 1);
    let built = serde_json::to_value(&model).unwrap();
    let (read, _, _, _) = wicked_governance::load_domain_graph_with_units(&store)
        .unwrap()
        .unwrap();
    assert_eq!(read["metadata"], built["metadata"]);
    assert_eq!(read["metadata"]["statements"].as_array().unwrap().len(), 3);
    validates(&read);
}

/// The truth table (§3.2, R10): a confirm settles only its statement; the target is accounted once
/// every statement is settled; a reject keeps the target open and the statement out of the model
/// (it stays on the target with its note); a confirm on an unaccounted target accounts for its lines.
#[test]
fn rulings_settle_what_they_name_and_the_target_follows() {
    let f = fixture("mini-cobol", "truth");
    rule_on(&f, 1, "confirm", None).unwrap();
    let doc = build(&f);
    // A confirm moves nothing between the requirements and the other statements (garden#1545):
    // only a business sentence makes a requirement, and the next run's prose writes those.
    assert_eq!(split(&doc), (vec![sid(0)], vec![sid(1), sid(2), sid(3)]));
    let rs = rules(&doc);
    assert_eq!(rs[&sid(1)]["status"], "confirmed");
    assert_eq!(rs[&sid(1)]["checked_by"], "you");
    assert_eq!(rs[&sid(1)]["confidence"], 1.0);
    assert_eq!(rs[&sid(1)]["ruling"]["by"], "ana");
    let fu = doc["metadata"]["followups"].as_array().unwrap();
    let row = fu.iter().find(|r| r["statement"] == sid(1)).unwrap();
    assert_eq!(row["ruling"]["action"], "confirm");
    // two statements still open: the target is not accounted yet
    assert_eq!(target_of(&doc, READ_PARA)["accounted"], false);
    assert_eq!(doc["metadata"]["accounting"]["open"], 2);
    rule_on(&f, 2, "confirm", None).unwrap();
    rule_on(&f, 3, "reject", Some("the amount is never zero")).unwrap();
    let doc = build(&f);
    let rs = rules(&doc);
    assert!(
        !rs.contains_key(&sid(3)),
        "a rejected statement is not in the model"
    );
    let t = target_of(&doc, READ_PARA);
    assert_eq!(t["rejected"][0]["id"], sid(3));
    assert_eq!(
        t["rejected"][0]["ruling"]["note"],
        "the amount is never zero"
    );
    assert_eq!(t["accounted"], false, "a reject keeps its target open");
    assert_eq!(doc["metadata"]["accounting"]["open"], 1);
    // a later ruling on the same statement replaces the earlier one
    rule_on(&f, 3, "confirm", None).unwrap();
    let doc = build(&f);
    assert_eq!(rules(&doc)[&sid(3)]["status"], "confirmed");
    assert_eq!(target_of(&doc, READ_PARA)["accounted"], true);
    // the refused target: a confirm says its lines make no business decision
    let main = target_of(&doc, MAIN_PARA);
    let mut store = open(&f);
    write_ruling(
        &mut store,
        &f.tree,
        &RulingRequest {
            target: Some(MAIN_PARA.into()),
            expect_target_hash: main["target_hash"].as_str().unwrap().into(),
            action: "confirm".into(),
            by: "ana".into(),
            ..Default::default()
        },
        101,
    )
    .unwrap()
    .unwrap();
    let doc = build(&f);
    assert_eq!(target_of(&doc, MAIN_PARA)["accounted"], true);
    assert_eq!(
        doc["metadata"]["accounting"],
        json!({"targets": 4, "accounted": 4, "open": 0, "stale": 0})
    );
    let refused = doc["metadata"]["followups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["kind"] == "refused")
        .unwrap()
        .clone();
    assert_eq!(refused["ruling"]["action"], "confirm");
    validates(&doc);
}

/// R04: the key includes each claim's index, so swapping which claim a guard points at is a
/// different statement, and a ruling made on the old one no longer applies.
#[test]
fn a_ruling_does_not_follow_a_statement_whose_guard_moved() {
    let a = json!({"anchor": {"file": "f"}, "claims": [
        {"i": 0, "type": "condition", "text": "A"}, {"i": 1, "type": "condition", "text": "B"},
        {"i": 2, "type": "effect", "text": "E", "when": [0]}]});
    let mut b = a.clone();
    b["claims"][0]["i"] = json!(1);
    b["claims"][1]["i"] = json!(0);
    assert_ne!(statement_key(&a), statement_key(&b));
    // lines are not part of it: a statement that only moved keeps its key
    let mut moved = a.clone();
    moved["anchor"]["lines"] = json!([5, 9]);
    assert_eq!(statement_key(&a), statement_key(&moved));

    // In the store: a ruling written for another key is ignored and the row shows open again.
    let f = fixture("mini-cobol", "key");
    let doc = build(&f);
    let th = target_of(&doc, READ_PARA)["target_hash"].clone();
    let mut store = open(&f);
    let sym = wicked_apps_core::SymbolId::from(READ_PARA);
    let ruling = json!({"subject": "statement", "id": sid(1), "action": "confirm", "by": "ana",
                        "at": 1, "statement_key": statement_key(&b), "target_hash": th});
    store
        .annotate(
            &sym,
            wicked_estate_core::Annotation::new(
                "rule_ruling",
                format!("rule_ruling:statement:{}", sid(1)),
                ruling.to_string(),
            ),
        )
        .unwrap();
    drop(store);
    let doc = build(&f);
    assert_eq!(rules(&doc)[&sid(1)]["status"], "open");
}

/// F07/R07: an edit to a file the batch read makes its targets stale: nothing on them shows as
/// checked, the sentence citing a stale statement is withheld (R11), and a ruling is refused.
#[test]
fn a_changed_checkout_makes_its_targets_stale() {
    let f = fixture("mini-cobol", "stale");
    let before = build(&f);
    let key1 = rules(&before)[&sid(1)]["statement_key"].clone();
    let th = target_of(&before, READ_PARA)["target_hash"].clone();
    let p = f.tree.join(PROG);
    let mut text = std::fs::read(&p).unwrap();
    text.extend_from_slice(b"      * edited\n");
    std::fs::write(&p, text).unwrap();
    let doc = build(&f);
    validates(&doc);
    let rs = rules(&doc);
    assert!(rs.values().all(|r| r["status"] == "stale"), "{rs:#?}");
    // The edit withholds the business rule as changed, yet its statement stays the requirement,
    // shown stale, instead of leaving the Requirements tab until the next run (garden#1545).
    assert_eq!(split(&doc), (vec![sid(0)], vec![sid(1), sid(2), sid(3)]));
    assert!(rs.values().all(|r| r.get("checked_by").is_none()));
    assert_eq!(rs[&sid(0)]["confidence"], 0.4);
    let g = &doc["metadata"]["rule_groups"][0];
    assert!(g["rule"].is_null());
    assert_eq!(g["stale"], true);
    assert!(g["withheld"]
        .as_array()
        .unwrap()
        .iter()
        .any(|w| w["reason"] == "its statements changed since it was written"));
    // the batch read all three files, so every target of it is stale
    assert_eq!(doc["metadata"]["accounting"]["stale"], 4);
    assert_eq!(doc["metadata"]["accounting"]["accounted"], 0);
    let mut store = open(&f);
    let refused = write_ruling(
        &mut store,
        &f.tree,
        &RulingRequest {
            statement: Some(sid(1)),
            expect_key: key1.as_str().map(String::from),
            expect_target_hash: th.as_str().unwrap().into(),
            action: "confirm".into(),
            by: "ana".into(),
            ..Default::default()
        },
        1,
    )
    .unwrap()
    .unwrap_err();
    assert_eq!(refused.code, "stale_code");
}

/// §4: every refusal names why, and none writes anything.
#[test]
fn a_ruling_is_refused_by_name_and_writes_nothing() {
    let f = fixture("mini-cobol", "refuse");
    let doc = build(&f);
    let rs = rules(&doc);
    let th = target_of(&doc, READ_PARA)["target_hash"]
        .as_str()
        .unwrap()
        .to_string();
    let key = |n| rs[&sid(n)]["statement_key"].as_str().map(String::from);
    let base = RulingRequest {
        statement: Some(sid(1)),
        expect_key: key(1),
        expect_target_hash: th.clone(),
        action: "confirm".into(),
        by: "ana".into(),
        ..Default::default()
    };
    let cases = [
        (
            RulingRequest {
                statement: Some(format!("{READ_PARA}#9")),
                ..base.clone()
            },
            "not_found",
        ),
        (
            RulingRequest {
                statement: Some(sid(0)),
                expect_key: key(0),
                ..base.clone()
            },
            "not_open",
        ),
        (
            RulingRequest {
                expect_key: key(2),
                ..base.clone()
            },
            "changed",
        ),
        (
            RulingRequest {
                expect_target_hash: "x".into(),
                ..base.clone()
            },
            "changed",
        ),
        (
            RulingRequest {
                action: "reject".into(),
                ..base.clone()
            },
            "note_required",
        ),
        (
            RulingRequest {
                action: "reject".into(),
                note: Some("  ".into()),
                ..base.clone()
            },
            "note_required",
        ),
        (
            RulingRequest {
                target: Some(MAIN_PARA.into()),
                ..base.clone()
            },
            "bad_request",
        ),
        (
            RulingRequest {
                action: "approve".into(),
                ..base.clone()
            },
            "bad_request",
        ),
        (
            RulingRequest {
                by: " ".into(),
                ..base.clone()
            },
            "bad_request",
        ),
        (
            RulingRequest {
                statement: None,
                target: Some("app/jcl/PAYJOB.jcl:L1-L1".into()),
                ..base.clone()
            },
            "not_open",
        ),
    ];
    for (req, code) in cases {
        let mut store = open(&f);
        let got = write_ruling(&mut store, &f.tree, &req, 1)
            .unwrap()
            .unwrap_err();
        assert_eq!(got.code, code, "{req:?}: {}", got.message);
    }
    let store = open(&f);
    assert!(store.annotations_by_type("rule_ruling").unwrap().is_empty());
}

/// §1.1 and R22: retired, malformed and old-loop rows are never read as statements, and the
/// coverage report counts a v3 statement by its effective status (never its confidence column,
/// which the writer leaves at 1.0): open or stale never resolve, a retired row is not counted.
#[test]
fn the_live_record_policy_holds_in_the_model_and_the_coverage_report() {
    use wicked_governance::domain_extract::RuleCount;
    let f = fixture("mini-cobol", "policy");
    let report = |f: &Fixture| {
        let cfg = wicked_governance::CoverageConfig {
            repo_root: Some(f.tree.clone()),
            ..Default::default()
        };
        wicked_governance::recompute_front_half_coverage_with(&open(f), &cfg).unwrap()
    };
    let unaccounted = |r: &wicked_governance::CoverageReport, sym: &str| {
        r.unaccounted_nodes.iter().any(|n| n.symbol_id == sym)
    };
    // How each business_rule row on READ-PARA counts.
    let counts = |f: &Fixture| -> Vec<RuleCount> {
        let store = open(f);
        let idx = ExtractIndex::load(&store, Some(&f.tree)).unwrap();
        store
            .annotations(&wicked_apps_core::SymbolId::from(READ_PARA))
            .unwrap()
            .iter()
            .filter(|a| a.r#type == "business_rule")
            .map(|a| idx.rule_counts(&a.key, &a.value))
            .collect()
    };
    assert_eq!(
        counts(&f),
        vec![
            RuleCount::Resolved(0.9),
            RuleCount::Unresolved,
            RuleCount::Unresolved,
            RuleCount::Unresolved
        ]
    );
    let fresh = report(&f);
    assert!(!unaccounted(&fresh, READ_PARA), "{fresh:#?}");
    assert!(unaccounted(&fresh, MAIN_PARA), "a target row is not a rule");

    // Retire the Checked statement (garden's tombstone shape, store.py:373-379), add an old-loop
    // rule on MAIN-PARA (no contract, its confidence in the column) and a malformed follow-up.
    let mut store = open(&f);
    let ann =
        |ty: &str, key: String, value: String| wicked_estate_core::Annotation::new(ty, key, value);
    store
        .annotate(
            &wicked_apps_core::SymbolId::from(READ_PARA),
            ann(
                "business_rule",
                format!("business_rule:{}", sid(0)),
                json!({"status": "retired"}).to_string(),
            ),
        )
        .unwrap();
    store
        .annotate(
            &wicked_apps_core::SymbolId::from(MAIN_PARA),
            ann("business_rule", "r".into(), "amount > 0".into()).with_confidence(0.9),
        )
        .unwrap();
    store
        .annotate(
            &wicked_apps_core::SymbolId::from(MAIN_PARA),
            ann(
                "domain_followup",
                "domain_followup:x".into(),
                "not json".into(),
            ),
        )
        .unwrap();
    drop(store);
    let doc = build(&f);
    assert_eq!(
        doc["metadata"]["skipped"],
        json!({"retired": 1, "malformed": 1, "old_loop": 1})
    );
    assert!(!rules(&doc).contains_key(&sid(0)));
    assert!(
        doc["metadata"]["rule_groups"][0]["rule"].is_null(),
        "its sentence is withheld"
    );
    validates(&doc);
    // The superseded Checked row and its tombstone are not counted; the open ones stay unresolved.
    assert_eq!(
        counts(&f),
        vec![
            RuleCount::Ignored,
            RuleCount::Unresolved,
            RuleCount::Unresolved,
            RuleCount::Unresolved,
            RuleCount::Ignored
        ]
    );
    let after = report(&f);
    assert!(
        !unaccounted(&after, READ_PARA),
        "open statements still account for it"
    );
    // (core#1955 R2-2) The store holds v3 rows, so the old-loop rule is not counted, as the model
    // skips it (`skipped.old_loop`): the report and the model tell one story.
    assert!(
        unaccounted(&after, MAIN_PARA),
        "the old-loop rule is not counted beside v3 rows"
    );
    assert_eq!(
        after.resolved + 1,
        fresh.resolved,
        "READ-PARA no longer resolves, and MAIN-PARA's old-loop rule (0.9) does not either"
    );

    // A changed checkout: READ-PARA's Checked statement is stale, so it no longer resolves.
    let g = fixture("mini-cobol", "policy-stale");
    std::fs::write(g.tree.join(PROG), b"edited\n").unwrap();
    let stale = report(&g);
    assert_eq!(counts(&g)[0], RuleCount::Unresolved);
    assert!(!unaccounted(&stale, READ_PARA));
    assert_eq!(stale.resolved + 1, fresh.resolved);
}

/// The governance coverage report on fixture `f`'s store, judged against its checkout.
fn coverage(f: &Fixture) -> wicked_governance::CoverageReport {
    let cfg = wicked_governance::CoverageConfig {
        repo_root: Some(f.tree.clone()),
        ..Default::default()
    };
    wicked_governance::recompute_front_half_coverage_with(&open(f), &cfg).unwrap()
}

fn unaccounted_in(r: &wicked_governance::CoverageReport, sym: &str) -> bool {
    r.unaccounted_nodes.iter().any(|n| n.symbol_id == sym)
}

/// An old-loop rule on MAIN-PARA, as the old loop wrote one (no contract, its confidence in the
/// column; `Annotation::new` as the old loop did).
fn add_old_loop_rule(f: &Fixture) {
    let mut store = open(f);
    store
        .annotate(
            &wicked_apps_core::SymbolId::from(MAIN_PARA),
            wicked_estate_core::Annotation::new("business_rule", "r", "amount > 0")
                .with_confidence(0.9),
        )
        .unwrap();
}

/// (core#1955 R2-2, codex round 1) A store stays a v3 store when every v3 row in it is retired (a
/// run that found no rules retires them all): an old-loop rule there is still not counted, as the
/// model skips it. Fails when "a v3 store" means "a store with a live v3 row".
#[test]
fn an_old_loop_rule_is_not_counted_once_every_v3_row_is_retired() {
    let f = fixture("mini-cobol", "all-retired");
    let mut store = open(&f);
    for ty in [
        "business_rule",
        "domain_target",
        "rule_group",
        "domain_followup",
    ] {
        // garden's tombstone shape (store.py:373-379) over every row of every v3 type.
        for (sym, a) in store.annotations_by_type(ty).unwrap() {
            store
                .annotate(
                    &sym,
                    wicked_estate_core::Annotation::new(
                        ty,
                        a.key,
                        json!({"status": "retired"}).to_string(),
                    ),
                )
                .unwrap();
        }
    }
    drop(store);
    add_old_loop_rule(&f);
    let idx = ExtractIndex::load(&open(&f), Some(&f.tree)).unwrap();
    assert!(idx.is_empty(), "no live v3 row is left");
    assert_eq!(
        build(&f)["metadata"]["skipped"]["old_loop"],
        1,
        "the model skips it"
    );
    assert!(
        unaccounted_in(&coverage(&f), MAIN_PARA),
        "the report skips it too"
    );
}

/// (core#1955 R2-2) A store the old loop alone wrote counts its rules the 1.1.0 way (its stored
/// 1.1.0 graph keeps its label) until a 1.2.0 build is persisted over it. From then on the report
/// follows that model, which skips them. The store is the fixture's indexed nodes with no row.
#[test]
fn an_old_loop_store_counts_the_old_way_until_a_1_2_0_build_is_persisted() {
    let f = fixture("mini-cobol", "old-loop-only");
    rusqlite::Connection::open(&f.db)
        .unwrap()
        .execute_batch("DELETE FROM annotations")
        .unwrap();
    add_old_loop_rule(&f);
    let before = coverage(&f);
    assert!(
        !unaccounted_in(&before, MAIN_PARA),
        "the old loop's rule counts"
    );
    let mut store = open(&f);
    build_and_persist(&mut store, &f.tree, 9).unwrap();
    drop(store);
    let after = coverage(&f);
    assert!(
        unaccounted_in(&after, MAIN_PARA),
        "the 1.2.0 model skips it, and so does the report"
    );
    assert_eq!(after.resolved + 1, before.resolved);
}

/// (core#1955 R2-1(d)) A build that another build overtakes is refused at the persist, and writes
/// nothing: A is assembled (its ids numbered against the store's build), B builds and commits, then
/// A's persist is refused and the store still holds B's model at B's generation. Fails without the
/// generation the persist expects: A would write its older model over B's.
#[test]
fn a_build_another_build_overtook_is_refused_and_writes_nothing() {
    let f = fixture("mini-cobol", "overtaken");
    let mut store = open(&f);
    let a = wicked_governance::assemble_build(&store, &f.tree, 9).unwrap();
    let (b, b_outcome) = build_and_persist(&mut store, &f.tree, 10).unwrap();
    let b = serde_json::to_value(b).unwrap();
    assert_ne!(
        serde_json::to_value(a.model()).unwrap()["metadata"],
        b["metadata"],
        "A and B differ, so a write of A shows"
    );
    let err = wicked_governance::persist_build(&mut store, a)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("another requirements-graph build committed")
            && err.contains("nothing was written"),
        "{err}"
    );
    let (read, build, _, _) = wicked_governance::load_domain_graph_with_units(&store)
        .unwrap()
        .unwrap();
    assert_eq!(build.generation, b_outcome.generation, "no new generation");
    assert_eq!(read["metadata"], b["metadata"]);
    assert_eq!(read["domains"], b["domains"]);
}

/// F18/F13/R16: the model persists and reads back whole (lists included), and a later build with
/// fewer statements never carries an absent requirement back.
#[test]
fn the_model_round_trips_through_the_store_and_carries_nothing_absent() {
    let f = fixture("mini-cobol", "trip");
    let mut store = open(&f);
    let (model, outcome) = build_and_persist(&mut store, &f.tree, 9).unwrap();
    assert_eq!(outcome.requirements, 1);
    let built = serde_json::to_value(&model).unwrap();
    let (read, build, _, links) = wicked_governance::load_domain_graph_with_units(&store)
        .unwrap()
        .unwrap();
    assert_eq!(read["metadata"], built["metadata"]);
    assert_eq!(read["domains"], built["domains"]);
    assert_eq!(build.generation, outcome.generation);
    let link = &links[PROG].values().next().unwrap()[0];
    assert_eq!(link.lines, vec![[20, 29]]);
    validates(&read);
    // Retire every statement: a follow-ups-only model; the requirement is gone, not carried.
    for n in 0..4 {
        store
            .annotate(
                &wicked_apps_core::SymbolId::from(READ_PARA),
                wicked_estate_core::Annotation::new(
                    "business_rule",
                    format!("business_rule:{}", sid(n)),
                    json!({"status": "retired"}).to_string(),
                ),
            )
            .unwrap();
    }
    let (model, outcome) = build_and_persist(&mut store, &f.tree, 10).unwrap();
    assert_eq!(outcome.requirements, 0);
    assert!(model.domains.is_empty());
    let (read, _, _, _) = wicked_governance::load_domain_graph_with_units(&store)
        .unwrap()
        .unwrap();
    assert_eq!(read["domains"], json!({}));
    assert_eq!(read["metadata"]["followups"].as_array().unwrap().len(), 4);
    validates(&read);

    // The anchor's file leaves the index: its rows are orphans (not read), and the requirement a
    // carrying build would restore for a node that is gone stays gone.
    let g = fixture("mini-cobol", "trip-gone");
    let mut store = open(&g);
    build_and_persist(&mut store, &g.tree, 9).unwrap();
    store.begin_batch().unwrap();
    store.remove_file(PROG).unwrap();
    store.commit_batch().unwrap();
    let (model, outcome) = build_and_persist(&mut store, &g.tree, 10).unwrap();
    assert_eq!(outcome.requirements, 0);
    assert!(model.domains.is_empty());
    let (read, _, _, _) = wicked_governance::load_domain_graph_with_units(&store)
        .unwrap()
        .unwrap();
    assert_eq!(read["domains"], json!({}), "{read:#}");
    let targets: Vec<&str> = read["metadata"]["targets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect();
    assert!(
        !targets.contains(&READ_PARA),
        "the orphan target is not read: {targets:?}"
    );
}

/// §4: the command's one JSON line and exit code: a ruling saved and the model rebuilt (its
/// accounting moved), and a refusal that names its code and writes nothing.
#[test]
fn domain_ruling_answers_one_line_and_rebuilds() {
    let f = fixture("mini-cobol", "cmd");
    let doc = build(&f);
    let key = rules(&doc)[&sid(1)]["statement_key"].clone();
    let th = target_of(&doc, READ_PARA)["target_hash"].clone();
    let req = RulingRequest {
        statement: Some(sid(1)),
        expect_key: key.as_str().map(String::from),
        expect_target_hash: th.as_str().unwrap().into(),
        action: "confirm".into(),
        by: "ana".into(),
        ..Default::default()
    };
    let (line, code) = wicked_core::domain_ruling(&f.db, &f.tree, &req);
    assert_eq!(code, 0, "{line}");
    let v: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["ruling"]["action"], "confirm");
    assert_eq!(v["build"]["ok"], true);
    assert_eq!(
        v["build"]["accounting"],
        json!({"targets": 4, "accounted": 2, "open": 2, "stale": 0})
    );
    // The rebuilt model is the one the store now holds.
    let (read, _, _, _) = wicked_governance::load_domain_graph_with_units(&open(&f))
        .unwrap()
        .unwrap();
    assert_eq!(rules(&read)[&sid(1)]["status"], "confirmed");

    let (line, code) = wicked_core::domain_ruling(
        &f.db,
        &f.tree,
        &RulingRequest {
            statement: Some(sid(0)),
            ..req
        },
    );
    assert_eq!(code, 2, "{line}");
    let v: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(
        (v["ok"].clone(), v["code"].clone()),
        (json!(false), json!("not_open"))
    );
    assert_eq!(
        open(&f).annotations_by_type("rule_ruling").unwrap().len(),
        1,
        "the refusal wrote nothing"
    );
}

/// The command itself: one JSON line on stdout and the exit code (§4); a missing checkout and a
/// store that does not exist are refused the same way, with nothing created.
#[test]
fn the_domain_ruling_command_answers_by_exit_code() {
    let f = fixture("mini-cobol", "bin");
    let doc = build(&f);
    let key = rules(&doc)[&sid(1)]["statement_key"]
        .as_str()
        .unwrap()
        .to_string();
    let th = target_of(&doc, READ_PARA)["target_hash"]
        .as_str()
        .unwrap()
        .to_string();
    let run = |args: &[&str]| {
        let out = std::process::Command::new(BIN)
            .arg("domain-ruling")
            .args(args)
            .env("HOME", &f.dir)
            .output()
            .unwrap();
        let line: Value = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stderr)));
        (out.status.code(), line)
    };
    let (db, tree, id) = (f.db.to_str().unwrap(), f.tree.to_str().unwrap(), sid(1));
    let ask = |db: &str, action: &str| {
        run(&[
            "--db",
            db,
            "--repo-root",
            tree,
            "--statement",
            &id,
            "--expect",
            &key,
            "--expect-target",
            &th,
            "--by",
            "ana",
            "--action",
            action,
        ])
    };
    let (code, v) = ask(db, "reject");
    assert_eq!(
        (code, v["code"].clone()),
        (Some(2), json!("note_required")),
        "{v}"
    );
    let (code, v) = run(&[
        "--db",
        db,
        "--statement",
        "x",
        "--action",
        "confirm",
        "--by",
        "ana",
    ]);
    assert_eq!(
        (code, v["code"].clone()),
        (Some(2), json!("bad_request")),
        "{v}"
    );
    let missing = f.dir.join("none.db");
    let (code, v) = ask(missing.to_str().unwrap(), "confirm");
    assert_eq!(
        (code, v["code"].clone()),
        (Some(2), json!("not_found")),
        "{v}"
    );
    assert!(!missing.exists(), "a refusal creates no store");
    let (code, v) = ask(db, "confirm");
    assert_eq!(code, Some(0), "{v}");
    assert_eq!(v["build"]["accounting"]["open"], 2, "{v}");
}

/// garden names a repaired statement `<id>~r<k>` (check.py:339): the schema takes it; and a 1.2.0
/// statement without its key does not validate.
#[test]
fn a_repaired_statement_id_is_valid_and_a_keyless_statement_is_not() {
    let f = fixture("mini-cobol", "repair");
    let mut doc = build(&f);
    let first = |doc: &mut Value| -> Value {
        let reqs = doc["domains"][PROG]["requirements"]
            .as_object_mut()
            .unwrap();
        reqs.values_mut().next().unwrap()["business_rules"][0].take()
    };
    let mut rule = first(&mut doc);
    rule["id"] = json!(format!("{}~r0", sid(0)));
    let put = |doc: &mut Value, rule: Value| {
        let reqs = doc["domains"][PROG]["requirements"]
            .as_object_mut()
            .unwrap();
        reqs.values_mut().next().unwrap()["business_rules"][0] = rule;
    };
    put(&mut doc, rule.clone());
    validates(&doc);
    rule.as_object_mut().unwrap().remove("statement_key");
    put(&mut doc, rule);
    let schema: Value = serde_json::from_str(SCHEMA).unwrap();
    let compiled = jsonschema::JSONSchema::compile(&schema).unwrap();
    assert!(compiled.validate(&doc).is_err());
}

/// Rewrite the live row of `key` in place (the store a later write, or a read between two writes,
/// leaves behind), by the row's own value.
fn rewrite(f: &Fixture, key: &str, edit: impl FnOnce(&mut Value)) {
    let conn = rusqlite::Connection::open(&f.db).unwrap();
    let (id, raw): (i64, String) = conn
        .query_row(
            "SELECT id, value FROM annotations WHERE key = ?1 ORDER BY id DESC LIMIT 1",
            [key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap_or_else(|e| panic!("no row {key}: {e}"));
    let mut v: Value = serde_json::from_str(&raw).unwrap();
    edit(&mut v);
    conn.execute(
        "UPDATE annotations SET value = ?1 WHERE id = ?2",
        rusqlite::params![v.to_string(), id],
    )
    .unwrap();
}

fn rule_key(n: u32) -> String {
    format!("business_rule:{}", sid(n))
}

fn group_key(f: &Fixture) -> String {
    rusqlite::Connection::open(&f.db)
        .unwrap()
        .query_row(
            "SELECT key FROM annotations WHERE type = 'rule_group' ORDER BY id LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

/// R2-1 (a), the C2 round-1 P1: a statement row and its target's row from two different runs (a
/// read between two writes) never show the old statement as Checked: it reads stale.
#[test]
fn a_statement_and_its_target_from_two_runs_read_stale() {
    let f = fixture("mini-cobol", "two-runs");
    assert_eq!(rules(&build(&f))[&sid(0)]["status"], "Checked");
    for field in ["batch_hash", "target_hash"] {
        let g = fixture("mini-cobol", &format!("two-runs-{field}"));
        rewrite(&g, &rule_key(0), |v| v[field] = json!("0".repeat(64)));
        let doc = build(&g);
        let rs = rules(&doc);
        assert_eq!(rs[&sid(0)]["status"], "stale", "{field}");
        assert!(rs[&sid(0)]["checked_by"].is_null(), "{field}");
        assert_eq!(
            rs[&sid(1)]["status"],
            "open",
            "{field}: the other statements are of one run"
        );
        assert!(
            target_of(&doc, READ_PARA)["stale"] == json!(false),
            "{field}: the target itself is fresh"
        );
        // A missing hash on the statement is not a match either.
        let h = fixture("mini-cobol", &format!("two-runs-{field}-none"));
        rewrite(&h, &rule_key(0), |v| {
            v.as_object_mut().unwrap().remove(field);
        });
        assert_eq!(
            rules(&build(&h))[&sid(0)]["status"],
            "stale",
            "{field} absent"
        );
    }
}

/// R2-1 (b): an anchor outside the repository, or with a line 0, leaves the statement unlocated.
#[test]
fn an_unusable_anchor_leaves_the_statement_unlocated() {
    let f = fixture("mini-cobol", "anchor");
    assert_eq!(rules(&build(&f))[&sid(1)]["located"], true);
    for (tag, file, lines) in [
        ("up", "../outside.cbl", json!([20, 29])),
        ("abs", "/srv/app/PAYCALC1.cbl", json!([20, 29])),
        ("zero", PROG, json!([0, 29])),
        ("short", PROG, json!([20])),
    ] {
        let g = fixture("mini-cobol", &format!("anchor-{tag}"));
        rewrite(&g, &rule_key(1), |v| {
            v["contract"]["anchor"]["file"] = json!(file);
            v["contract"]["anchor"]["lines"] = lines.clone();
        });
        let doc = build(&g);
        assert_eq!(rules(&doc)[&sid(1)]["located"], false, "{tag}");
        validates(&doc);
    }
}

/// R2-1 (c): a group sentence rests on its statements as they read now. A stored input hash that
/// differs from the Checked statement's text, or none at all, withholds the sentence.
#[test]
fn a_group_sentence_whose_input_changed_or_is_missing_is_withheld() {
    let f = fixture("mini-cobol", "inputs");
    assert!(!build(&f)["metadata"]["rule_groups"][0]["rule"].is_null());
    let gk = group_key(&f);
    for (tag, input) in [("changed", Some(json!("0".repeat(64)))), ("missing", None)] {
        let g = fixture("mini-cobol", &format!("inputs-{tag}"));
        rewrite(&g, &gk, |v| match &input {
            Some(h) => v["inputs"][sid(0)] = h.clone(),
            None => {
                v["inputs"].as_object_mut().unwrap().remove(&sid(0));
            }
        });
        let doc = build(&g);
        assert_eq!(
            rules(&doc)[&sid(0)]["status"],
            "Checked",
            "{tag}: the statement itself stands"
        );
        let group = &doc["metadata"]["rule_groups"][0];
        assert!(group["rule"].is_null(), "{tag}: {group:#}");
        assert!(
            group["withheld"]
                .as_array()
                .unwrap()
                .iter()
                .any(|w| w["id"] == "g1.r"
                    && w["reason"] == "its statements changed since it was written"),
            "{tag}: {group:#}"
        );
        validates(&doc);
    }
}

/// R2-1 (e): a group row with no `group` object and a statement row with no text are malformed:
/// counted, and not read.
#[test]
fn a_group_without_its_group_and_a_statement_without_text_are_malformed() {
    let f = fixture("mini-cobol", "malformed");
    assert_eq!(build(&f)["metadata"]["skipped"]["malformed"], 0);
    let gk = group_key(&f);
    rewrite(&f, &gk, |v| {
        v.as_object_mut().unwrap().remove("group");
    });
    rewrite(&f, &rule_key(3), |v| v["statement"] = json!("  "));
    let doc = build(&f);
    assert_eq!(doc["metadata"]["skipped"]["malformed"], 2);
    assert!(doc["metadata"]["rule_groups"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(!rules(&doc).contains_key(&sid(3)));
    validates(&doc);
}
