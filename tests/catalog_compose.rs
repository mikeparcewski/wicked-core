//! Seam C1 (DES-TEAMING-002 §8.3, §11.2, §14): the phase catalog + `compose`.
//!
//! Acceptance (a): composing every §11.2 mapping yields a def whose per-phase
//! `(kind, role, gate, validator_pin, executes_code, executor, skill_ref, instructions, depends_on)`
//! equals TODAY's def except exactly §11.2's bold cells, one fixture per consumer.
//!
//! "Today's def" is read LIVE for every consumer core owns — `WorkflowRegistry::with_defaults()`
//! overlaid with the shipped `workflows/*.json`, which is what the engine resolves — and from
//! `tests/fixtures/catalog/crew-defs.json` for the consumers only crew defines (dumped from crew
//! `main` 4870c87 — see `tests/fixtures/catalog/README.md`). The steps and the bold cells are
//! `tests/fixtures/catalog/mappings.json`; the bold cells are fixed values, and the migration
//! cleanup cells are pinned again below in code.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::{json, Map, Value};
use wicked_core::{
    catalog, compose, plan_from_def, PlanRefusal, PlanSteps, WorkflowDef, WorkflowRegistry,
};

const EVIDENCE_FLOOR_PIN: &str = "e2e7af1db9e48454";

/// The acceptance tuple, in the DES's order.
const FIELDS: [&str; 9] = [
    "kind",
    "role",
    "gate",
    "validator_pin",
    "executes_code",
    "executor",
    "skill_ref",
    "instructions",
    "depends_on",
];

/// Every §11.2 consumer row.
const CONSUMERS: [&str; 19] = [
    "feature",
    "bug",
    "migration",
    "deliver",
    "chat",
    "onboarding",
    "survey-repo",
    "capture-learnings",
    "memories",
    "domain-graph-slice",
    "domain-extraction",
    "steering-author",
    "collab",
    "interactive-chat",
    "interactive-draft",
    "interactive-edit",
    "interactive-demo",
    "interactive-demo-reauthor",
    "qe-author-tests",
];

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/catalog")
}

fn read_json(path: PathBuf) -> Value {
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Today's def for every consumer: core's live defs, then crew's dumped ones.
fn today_defs() -> BTreeMap<String, WorkflowDef> {
    let mut reg = WorkflowRegistry::with_defaults();
    let loaded = reg
        .load_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("workflows"))
        .expect("the shipped workflows/ overlay loads");
    assert_eq!(
        loaded.len(),
        8,
        "every shipped workflow file loads: {loaded:?}"
    );
    let mut out = BTreeMap::new();
    for id in reg.ids() {
        out.insert(id.clone(), reg.get(&id).unwrap().clone());
    }
    let crew = read_json(fixtures().join("crew-defs.json"));
    for (consumer, def) in crew.as_object().expect("crew-defs.json is an object") {
        let def: WorkflowDef = serde_json::from_value(def.clone())
            .unwrap_or_else(|e| panic!("crew fixture {consumer}: {e}"));
        def.validate().unwrap();
        assert!(
            out.insert(consumer.clone(), def).is_none(),
            "{consumer} is defined by core — its fixture must come from core, not crew"
        );
    }
    out
}

/// One phase's acceptance tuple as a JSON object (field → value), normalised through serde so
/// defaults compare equal to their spelled-out form.
fn tuple(phase: &wicked_core::PhaseDef) -> Map<String, Value> {
    let v = serde_json::to_value(phase).unwrap();
    FIELDS
        .iter()
        .map(|f| (f.to_string(), v.get(*f).cloned().unwrap_or(Value::Null)))
        .collect()
}

fn mappings() -> Map<String, Value> {
    read_json(fixtures().join("mappings.json"))
        .as_object()
        .cloned()
        .expect("mappings.json is an object")
}

fn steps_of(mapping: &Value) -> PlanSteps {
    serde_json::from_value(json!({ "steps": mapping["steps"] })).expect("the fixture's steps parse")
}

/// C1 acceptance (a).
#[test]
fn compose_of_every_mapping_equals_todays_def_except_the_bold_cells() {
    let today = today_defs();
    let maps = mappings();
    let mut consumers: Vec<&str> = maps.keys().map(String::as_str).collect();
    consumers.sort();
    let mut want: Vec<&str> = CONSUMERS.to_vec();
    want.sort();
    assert_eq!(consumers, want, "one fixture per §11.2 consumer");

    let mut bold_total = 0;
    for consumer in CONSUMERS {
        let mapping = &maps[consumer];
        let before = today
            .get(consumer)
            .unwrap_or_else(|| panic!("no today def for {consumer}"));
        let after = compose(catalog(), &steps_of(mapping))
            .unwrap_or_else(|e| panic!("{consumer}: compose refused its §11.2 mapping: {e}"));

        let ids = |d: &WorkflowDef| d.phases.iter().map(|p| p.id.clone()).collect::<Vec<_>>();
        assert_eq!(
            ids(&after),
            ids(before),
            "{consumer}: same phases, same order"
        );

        let mut diff = Map::new();
        for (b, a) in before.phases.iter().zip(after.phases.iter()) {
            let (tb, ta) = (tuple(b), tuple(a));
            for f in FIELDS {
                if tb[f] != ta[f] {
                    diff.insert(format!("{}.{f}", a.id), ta[f].clone());
                }
            }
        }
        let bold = mapping["bold"].as_object().cloned().unwrap_or_default();
        assert_eq!(
            Value::Object(diff),
            Value::Object(bold.clone()),
            "{consumer}: compose must differ from today's def in exactly §11.2's bold cells"
        );
        bold_total += bold.len();
    }
    // feature 2 (test/review role), migration 5 (cutover pin+role, cleanup pin+code+role),
    // domain-extraction 1, steering-author 1, collab 2 (kind recon → build).
    assert_eq!(bold_total, 11, "§11.2 has eleven bold cells");
}

/// C1 acceptance (a), the named migration cell: cleanup maps to `build` (rev 10 decision).
#[test]
fn migration_cleanup_composes_as_build() {
    let def = compose(catalog(), &steps_of(&mappings()["migration"])).unwrap();
    let cleanup = def.phases.iter().find(|p| p.id == "cleanup").unwrap();
    let t = tuple(cleanup);
    assert_eq!(t["kind"], json!("build"));
    assert_eq!(t["role"], json!("creator"));
    assert_eq!(t["validator_pin"], json!(EVIDENCE_FLOOR_PIN));
    assert_eq!(t["executes_code"], json!(true));
    assert_eq!(t["gate"], json!("auto"));
    assert_eq!(t["depends_on"], json!(["verify"]));
    let cutover = def.phases.iter().find(|p| p.id == "cutover").unwrap();
    assert_eq!(
        tuple(cutover)["gate"],
        json!({"human_confirm": {"unconditional": true}}),
        "cutover keeps its unconditional human gate"
    );
}

fn refusal(steps: Value) -> PlanRefusal {
    let plan: PlanSteps = serde_json::from_value(json!({ "steps": steps })).unwrap();
    compose(catalog(), &plan).expect_err("the plan must be refused")
}

/// C1 acceptance (b): each weakening is refused with a named reason.
#[test]
fn a_step_that_weakens_its_entry_is_refused_with_a_named_reason() {
    let cases = [
        (
            json!([{"catalog": "test", "id": "t", "gate": "auto"}]),
            "gate_lowered",
        ),
        (
            json!([{"catalog": "build", "id": "b", "gate": {"human_confirm_if": "verdict_not_pass"}},
                   {"catalog": "review", "id": "r", "depends_on": ["b"], "gate": "auto"},
                   {"catalog": "understand", "id": "u", "gate": {"human_confirm": {"unconditional": true}}},
                   {"catalog": "understand", "id": "u2", "gate": {"human_confirm_if": "verdict_not_pass"}},
                   {"catalog": "test", "id": "t2", "gate": {"human_confirm": {"unconditional": false}}},
                   {"catalog": "build", "id": "b2", "gate": {"human_confirm": {"unconditional": true}}},
                   {"catalog": "build", "id": "b3", "gate": {"human_confirm": {"unconditional": false}}},
                   {"catalog": "test", "id": "t3", "gate": "auto"}]),
            "gate_lowered",
        ),
        (
            json!([{"catalog": "build", "id": "b", "validator_pin": null}]),
            "pin_removed",
        ),
        (
            json!([{"catalog": "produce", "id": "p", "role": "evaluator"}]),
            "role_changed",
        ),
        (
            json!([{"catalog": "test", "id": "t", "role": "creator"}]),
            "role_changed",
        ),
        (
            json!([{"catalog": "understand", "id": "u",
                    "executor": {"type": "tool", "cmd": ["echo", "hi"]}}]),
            "executor_not_allowed",
        ),
        (
            json!([{"catalog": "build", "id": "b", "executor": {"type": "agent"}}]),
            "executor_not_allowed",
        ),
        (
            json!([{"catalog": "build", "id": "b", "executes_code": false}]),
            "executes_code_lowered",
        ),
        (
            json!([{"catalog": "produce", "id": "p", "kind": "recon"}]),
            "kind_not_allowed",
        ),
        (
            json!([{"catalog": "run", "id": "r"}]),
            "tool_command_missing",
        ),
        (
            json!([{"catalog": "deliver", "id": "d", "executor": {"type": "tool", "cmd": []}}]),
            "tool_command_missing",
        ),
        (
            json!([{"catalog": "run", "id": "r", "executor": {"type": "agent"}}]),
            "tool_command_missing",
        ),
        (
            json!([{"catalog": "deploy", "id": "x"}]),
            "unknown_catalog_entry",
        ),
        (
            json!([{"catalog": "produce", "id": "p", "executes_code": true}]),
            "invalid_def",
        ),
        (json!([]), "invalid_def"),
        (
            json!([{"catalog": "understand", "id": "u", "depends_on": ["later"]},
                   {"catalog": "understand", "id": "later"}]),
            "invalid_def",
        ),
    ];
    for (steps, reason) in cases {
        let r = refusal(steps.clone());
        assert_eq!(r.reason(), reason, "{steps}: {r}");
        assert!(
            r.to_string().starts_with(reason),
            "the message names the reason: {r}"
        );
    }
}

/// The strengthenings §8.3/§11.2 allow are accepted, and a same-value `role` is not a change.
#[test]
fn a_step_that_strengthens_its_entry_is_accepted() {
    let plan: PlanSteps = serde_json::from_value(json!({ "steps": [
        {"catalog": "understand", "id": "u", "gate": {"human_confirm": {"unconditional": true}},
         "validator_pin": EVIDENCE_FLOOR_PIN, "owner": "team"},
        {"catalog": "produce", "id": "p", "executes_code": true, "validator_pin": EVIDENCE_FLOOR_PIN,
         "depends_on": ["u"], "role": "creator"},
        {"catalog": "test", "id": "t", "validator_pin": "bfe4020a365c598b", "depends_on": ["p"],
         "gate": {"human_confirm": {"unconditional": false}}},
        {"catalog": "run", "id": "r", "kind": "test", "depends_on": ["t"],
         "executor": {"type": "tool", "cmd": ["true"]}},
        {"catalog": "security_review", "id": "s", "depends_on": ["r"]}
    ]}))
    .unwrap();
    let def = compose(catalog(), &plan).unwrap();
    assert_eq!(def.id, wicked_core::COMPOSED_DEF_ID);
    let v = serde_json::to_value(&def).unwrap();
    assert_eq!(v["phases"][0]["owner"], json!("team"));
    assert_eq!(
        v["phases"][0]["gate"],
        json!({"human_confirm": {"unconditional": true}})
    );
    assert_eq!(v["phases"][1]["executes_code"], json!(true));
    assert_eq!(v["phases"][2]["validator_pin"], json!("bfe4020a365c598b"));
    assert_eq!(v["phases"][3]["kind"], json!("test"));
    assert_eq!(
        v["phases"][3]["executor"],
        json!({"type": "tool", "cmd": ["true"]})
    );
    assert_eq!(
        v["phases"][4]["skill_ref"],
        json!("wicked-garden-qe-security-test-engineer")
    );
    assert_eq!(v["phases"][4]["validator_pin"], json!(EVIDENCE_FLOOR_PIN));
    // The owner rides onto the unit, like role.
    let units = plan_from_def(&def, "x", "s");
    let u = serde_json::to_value(&units[0]).unwrap();
    assert_eq!(u["owner"], json!("team"));
    let u1 = serde_json::to_value(&units[1]).unwrap();
    assert!(
        u1.get("owner").is_none(),
        "the default owner is skipped: {u1}"
    );
}

/// C1 acceptance (c): a misspelled step key is refused at parse.
#[test]
fn a_misspelled_step_key_is_refused() {
    for bad in [
        json!({"steps": [{"catalog": "build", "id": "b", "validator_pinn": EVIDENCE_FLOOR_PIN}]}),
        json!({"steps": [{"catalog": "build", "id": "b", "onwer": "team"}]}),
        json!({"steps": [], "stpes": []}),
    ] {
        let err = serde_json::from_value::<PlanSteps>(bad.clone())
            .expect_err("deny_unknown_fields must refuse a misspelled key")
            .to_string();
        assert!(err.contains("unknown field"), "{bad}: {err}");
    }
}

/// C1 acceptance (d): an owner-omitted def serializes byte-identically — the shipped workflow
/// files that spell every field re-serialize to their exact bytes, no def or unit gains an
/// `owner` key, and an explicit `team` owner round-trips.
#[test]
fn an_owner_omitted_def_serializes_byte_identically() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("workflows");
    for name in ["feature", "bug", "migration"] {
        let raw = std::fs::read_to_string(dir.join(format!("{name}.json"))).unwrap();
        let def: WorkflowDef = serde_json::from_str(&raw).unwrap();
        let out = serde_json::to_string_pretty(&def).unwrap();
        assert_eq!(
            out.trim_end(),
            raw.trim_end(),
            "{name}.json re-serializes byte-identically"
        );
    }
    for (id, def) in today_defs() {
        let s = serde_json::to_string(&def).unwrap();
        assert!(!s.contains("\"owner\""), "{id} gained an owner key: {s}");
        let back: WorkflowDef = serde_json::from_str(&s).unwrap();
        assert_eq!(
            serde_json::to_string(&back).unwrap(),
            s,
            "{id} round-trips byte-identically"
        );
        for unit in plan_from_def(&def, "intent", "s") {
            let u = serde_json::to_string(&unit).unwrap();
            assert!(
                !u.contains("\"owner\""),
                "{id} unit gained an owner key: {u}"
            );
        }
    }
    let team: WorkflowDef =
        serde_json::from_value(json!({"id": "t", "phases": [{"id": "a", "owner": "team"}]}))
            .unwrap();
    let s = serde_json::to_value(&team).unwrap();
    assert_eq!(s["phases"][0]["owner"], json!("team"));
}

/// Arm the hermetic emit spool before `main` (core#311), as every test binary does: an emission a
/// test triggers must never spool to the operator's real replay queue.
///
/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only sets one process env var
/// via the std API — no allocator setup, no threads, no panics across the FFI boundary.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}
