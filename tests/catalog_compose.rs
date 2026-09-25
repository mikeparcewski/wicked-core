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
/// `COVERAGE_VALIDATOR_PIN`: shipped and APPROVED (the `domain_coverage` entry's pin).
const COVERAGE_PIN: &str = "bfe4020a365c598b";

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

/// domain-extraction's coverage maps onto `domain_coverage`, which carries the coverage pin as
/// data: the mapping restates it (a no-op), no §11.2 step swaps a pin, and the composed phase is
/// today's in every acceptance field and still satisfies `BINARY_PINNED_PHASES`.
#[test]
fn domain_extraction_coverage_composes_from_its_own_pinned_entry() {
    let maps = mappings();
    for consumer in CONSUMERS {
        for step in maps[consumer]["steps"].as_array().unwrap() {
            let Some(pin) = step.get("validator_pin") else {
                continue;
            };
            let entry = wicked_core::catalog_entry(step["catalog"].as_str().unwrap()).unwrap();
            if let Some(own) = entry.validator_pin.as_deref() {
                assert_eq!(
                    pin,
                    &json!(own),
                    "{consumer}/{}: swaps its entry's pin",
                    step["id"]
                );
            }
        }
    }
    let def = compose(catalog(), &steps_of(&maps["domain-extraction"])).unwrap();
    let today = today_defs();
    let before = today["domain-extraction"]
        .phases
        .iter()
        .find(|p| p.id == "coverage")
        .unwrap();
    let after = def.phases.iter().find(|p| p.id == "coverage").unwrap();
    assert_eq!(tuple(after), tuple(before), "coverage is unchanged");
    assert_eq!(after.verified_evidence, before.verified_evidence);
    assert_eq!(after.required_deliverables, before.required_deliverables);
    // `BINARY_PINNED_PHASES` is `[("domain-extraction", "coverage", COVERAGE_VALIDATOR_PIN)]`.
    assert_eq!(
        after.validator_pin.as_deref(),
        Some(wicked_core::COVERAGE_VALIDATOR_PIN)
    );
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

/// Operator decision (follow-up to #615): a step may ADD a pin to an unpinned entry, but never
/// SWAP the pin an entry carries — not even for another APPROVED pin. Here `build`'s evidence
/// floor is swapped for the shipped, approved coverage pin: refused as `pin_changed`.
#[test]
fn a_step_cannot_swap_its_entrys_pin_for_another_approved_pin() {
    let r = refusal(json!([{"catalog": "build", "id": "b", "validator_pin": COVERAGE_PIN}]));
    assert_eq!(r.reason(), "pin_changed", "{r}");
    assert!(r.to_string().starts_with("pin_changed"), "{r}");
    // Restating the entry's own pin is a no-op, and removal stays `pin_removed`.
    let same: PlanSteps = serde_json::from_value(json!({ "steps": [
        {"catalog": "build", "id": "b", "validator_pin": EVIDENCE_FLOOR_PIN}
    ]}))
    .unwrap();
    let def = compose(catalog(), &same).expect("restating the entry's pin is a no-op");
    assert_eq!(
        def.phases[0].validator_pin.as_deref(),
        Some(EVIDENCE_FLOOR_PIN)
    );
    let gone = refusal(json!([{"catalog": "build", "id": "b", "validator_pin": null}]));
    assert_eq!(gone.reason(), "pin_removed", "{gone}");
}

/// The strengthenings §8.3/§11.2 allow are accepted, and a same-value `role` is not a change.
#[test]
fn a_step_that_strengthens_its_entry_is_accepted() {
    let plan: PlanSteps = serde_json::from_value(json!({ "steps": [
        {"catalog": "understand", "id": "u", "gate": {"human_confirm": {"unconditional": true}},
         "validator_pin": EVIDENCE_FLOOR_PIN, "owner": "team"},
        {"catalog": "produce", "id": "p", "executes_code": true, "validator_pin": EVIDENCE_FLOOR_PIN,
         "depends_on": ["u"], "role": "creator"},
        {"catalog": "critique", "id": "t", "validator_pin": COVERAGE_PIN, "depends_on": ["p"],
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
    // An ADD on the unpinned `critique` entry (a swap on a pinned entry is `pin_changed`).
    assert_eq!(v["phases"][2]["validator_pin"], json!(COVERAGE_PIN));
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

/// Review finding on #615 (codex, HIGH): a step replaced a catalog entry's `skill_ref`, so the
/// phase called `security_review` ran a non-security skill. Every field that could loosen an
/// entry is refused with a named reason; the same value is a no-op.
#[test]
fn a_step_cannot_replace_what_its_entry_already_fixes() {
    let cases = [
        // skill_ref: set-only-if-unset.
        (
            json!([{"catalog": "security_review", "id": "s", "skill_ref": "wicked-garden-domain"}]),
            "skill_ref_changed",
        ),
        // required_deliverables: tighten-only (the entry's list may only grow).
        (
            json!([{"catalog": "understand", "id": "u", "required_deliverables": ["a.json"]},
                   {"catalog": "understand", "id": "v", "depends_on": ["u"]},
                   {"catalog": "security_review", "id": "s2", "required_deliverables": []}]),
            "ok",
        ),
    ];
    for (steps, reason) in cases {
        let plan: PlanSteps = serde_json::from_value(json!({ "steps": steps })).unwrap();
        match (compose(catalog(), &plan), reason) {
            (Ok(_), "ok") => {}
            (Ok(def), _) => panic!("{steps}: accepted, want {reason}: {def:?}"),
            (Err(r), want) => assert_eq!(r.reason(), want, "{steps}: {r}"),
        }
    }
    // The same value is a no-op, not a refusal.
    let same: PlanSteps = serde_json::from_value(json!({ "steps": [
        {"catalog": "security_review", "id": "s",
         "skill_ref": "wicked-garden-qe-security-test-engineer", "role": "evaluator",
         "kind": "review", "validator_pin": EVIDENCE_FLOOR_PIN, "executes_code": false,
         "gate": "auto"}
    ]}))
    .unwrap();
    let def = compose(catalog(), &same).expect("restating the entry is not a change");
    assert_eq!(
        def.phases[0].skill_ref.as_deref(),
        Some("wicked-garden-qe-security-test-engineer")
    );
}

/// The field table (`STEP_FIELD_RULES`) is exhaustive and enforced: every `PlanStep` field has a
/// row, and every field whose rule is not `Free` refuses a loosening step with its named reason
/// while accepting the entry's own value. Run against a synthetic entry that sets EVERY field, so
/// no rule is vacuous because the shipped catalog happens to leave a field empty.
#[test]
fn every_step_field_is_classified_and_every_loosening_is_refused() {
    use wicked_core::{FieldRule, PhaseDef, STEP_FIELD_RULES};
    let entries: Vec<PhaseDef> = serde_json::from_value(json!([
        {"id": "full", "kind": "build", "role": "creator", "instructions": "own",
         "gate_type": "execution", "gate": {"human_confirm": {"unconditional": false}},
         "executes_code": true, "required_deliverables": ["r.json"], "skill_ref": "own-skill",
         "allowed_skills": ["a"], "validator_pin": EVIDENCE_FLOOR_PIN},
        {"id": "tool", "kind": "recon", "executor": {"type": "tool", "cmd": []}}
    ]))
    .unwrap();

    // A fully populated step serializes every field: the table must name exactly those.
    let full_step = json!({"catalog": "full", "id": "x", "instructions": "own", "gate": "auto",
        "gate_type": "value", "validator_pin": "p", "executes_code": true, "skill_ref": "s",
        "allowed_skills": [], "required_deliverables": [], "depends_on": [],
        "executor": {"type": "agent"}, "owner": "team", "kind": "build", "role": "creator",
        "added_by": "floor", "floor_reason": "band 20-39 requires build"});
    let parsed: wicked_core::PlanStep = serde_json::from_value(full_step).unwrap();
    let mut keys: Vec<String> = serde_json::to_value(&parsed)
        .unwrap()
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    keys.sort();
    let mut table: Vec<String> = STEP_FIELD_RULES
        .iter()
        .map(|(f, _)| f.to_string())
        .collect();
    table.sort();
    assert_eq!(
        keys, table,
        "STEP_FIELD_RULES must classify every PlanStep field"
    );

    let compose_one = |entry: &str, extra: Value| {
        let mut step = json!({"catalog": entry, "id": "x"});
        for (k, v) in extra.as_object().unwrap() {
            step[k] = v.clone();
        }
        let plan: PlanSteps = serde_json::from_value(json!({ "steps": [step] })).unwrap();
        compose(&entries, &plan)
    };
    // (field → the loosening steps and the reason each must be refused with, plus the entry's
    // own value, which must be accepted).
    // (catalog entry, step fields, expected refusal reason)
    type Loosening<'a> = Vec<(&'a str, Value, &'a str)>;
    // (catalog entry, step fields) restating the entry's own value
    type Restate<'a> = (&'a str, Value);
    let loosen: BTreeMap<&str, (Loosening, Restate)> = BTreeMap::from([
        (
            "role",
            (
                vec![("full", json!({"role": "evaluator"}), "role_changed")],
                ("full", json!({"role": "creator"})),
            ),
        ),
        (
            "kind",
            (
                vec![("full", json!({"kind": "recon"}), "kind_not_allowed")],
                ("full", json!({"kind": "build"})),
            ),
        ),
        (
            "gate",
            (
                vec![
                    ("full", json!({"gate": "auto"}), "gate_lowered"),
                    (
                        "full",
                        json!({"gate": {"human_confirm_if": "verdict_not_pass"}}),
                        "gate_lowered",
                    ),
                ],
                (
                    "full",
                    json!({"gate": {"human_confirm": {"unconditional": false}}}),
                ),
            ),
        ),
        (
            "validator_pin",
            (
                vec![
                    ("full", json!({"validator_pin": null}), "pin_removed"),
                    (
                        "full",
                        json!({"validator_pin": COVERAGE_PIN}),
                        "pin_changed",
                    ),
                ],
                ("full", json!({"validator_pin": EVIDENCE_FLOOR_PIN})),
            ),
        ),
        (
            "executes_code",
            (
                vec![(
                    "full",
                    json!({"executes_code": false}),
                    "executes_code_lowered",
                )],
                ("full", json!({"executes_code": true})),
            ),
        ),
        (
            "required_deliverables",
            (
                vec![
                    (
                        "full",
                        json!({"required_deliverables": []}),
                        "deliverable_removed",
                    ),
                    (
                        "full",
                        json!({"required_deliverables": ["other.json"]}),
                        "deliverable_removed",
                    ),
                ],
                (
                    "full",
                    json!({"required_deliverables": ["r.json", "more.json"]}),
                ),
            ),
        ),
        (
            "executor",
            (
                vec![
                    (
                        "full",
                        json!({"executor": {"type": "tool", "cmd": ["sh"]}}),
                        "executor_not_allowed",
                    ),
                    (
                        "full",
                        json!({"executor": {"type": "agent"}}),
                        "executor_not_allowed",
                    ),
                    (
                        "tool",
                        json!({"executor": {"type": "agent"}}),
                        "tool_command_missing",
                    ),
                    (
                        "tool",
                        json!({"executor": {"type": "tool", "cmd": []}}),
                        "tool_command_missing",
                    ),
                    ("tool", json!({}), "tool_command_missing"),
                ],
                (
                    "tool",
                    json!({"executor": {"type": "tool", "cmd": ["true"]}}),
                ),
            ),
        ),
        (
            "instructions",
            (
                vec![(
                    "full",
                    json!({"instructions": "other"}),
                    "instructions_changed",
                )],
                ("full", json!({"instructions": "own"})),
            ),
        ),
        (
            "skill_ref",
            (
                vec![(
                    "full",
                    json!({"skill_ref": "wicked-garden-domain"}),
                    "skill_ref_changed",
                )],
                ("full", json!({"skill_ref": "own-skill"})),
            ),
        ),
        (
            "allowed_skills",
            (
                vec![
                    (
                        "full",
                        json!({"allowed_skills": ["b"]}),
                        "allowed_skills_changed",
                    ),
                    (
                        "full",
                        json!({"allowed_skills": []}),
                        "allowed_skills_changed",
                    ),
                ],
                ("full", json!({"allowed_skills": ["a"]})),
            ),
        ),
    ]);
    for (field, rule) in STEP_FIELD_RULES {
        match rule {
            FieldRule::Identity => {}
            // A record of how the step entered the plan: compose ignores it (T2, §8.5).
            FieldRule::Record => {
                let record = json!({"added_by": "floor", "floor_reason": "band 20-39 requires x"});
                let def = compose_one("full", record).unwrap_or_else(|e| panic!("{field}: {e}"));
                assert_eq!(def, compose_one("full", json!({})).unwrap(), "{field}");
            }
            FieldRule::Free => {
                let change = match field {
                    "gate_type" => json!({"gate_type": null}),
                    "depends_on" => json!({"depends_on": []}),
                    "owner" => json!({"owner": "team"}),
                    other => panic!("no free-field case for {other}"),
                };
                compose_one("full", change).unwrap_or_else(|e| panic!("free {field}: {e}"));
            }
            _ => {
                let (cases, same) = loosen
                    .get(field)
                    .unwrap_or_else(|| panic!("{field} ({rule:?}) has no loosening case"));
                for (entry, step, reason) in cases {
                    let r = compose_one(entry, step.clone())
                        .expect_err(&format!("{field}: {step} must be refused"));
                    assert_eq!(r.reason(), *reason, "{field}: {step}: {r}");
                }
                compose_one(same.0, same.1.clone())
                    .unwrap_or_else(|e| panic!("{field}: the entry's own value is a no-op: {e}"));
            }
        }
    }
    // Setting a SetIfUnset field on an entry that leaves it unset is allowed.
    let def = compose_one(
        "tool",
        json!({"executor": {"type": "tool", "cmd": ["true"]}, "skill_ref": "s",
               "instructions": "i", "allowed_skills": ["a"], "validator_pin": COVERAGE_PIN}),
    )
    .unwrap();
    assert_eq!(def.phases[0].skill_ref.as_deref(), Some("s"));
    assert_eq!(def.phases[0].validator_pin.as_deref(), Some(COVERAGE_PIN));
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
        // Compare the committed bytes: a Windows checkout may turn LF into CRLF (core.autocrlf).
        let raw = std::fs::read_to_string(dir.join(format!("{name}.json")))
            .unwrap()
            .replace("\r\n", "\n");
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
