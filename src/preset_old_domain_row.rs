//! (J6 C1b) The OLD 9-step `domain-extraction` built-in, as wicked-core main 2b34bc9 shipped it:
//! `domain_extraction_preset` is copied verbatim from `src/catalog.rs:1304-1407` at that commit, and
//! the names it used from `src/domain_extraction.rs` (`:57-133`, `:302`, `:309`) and
//! `src/catalog.rs:52` are copied beside it. Only the imports are adjusted. The old-row seed test
//! serializes it through the store's own writer (`Preset::to_node`), so the stored row is the one
//! a store seeded before J6 v3 holds. Test-only.

use crate::domain::StageKind;
use crate::plan::PlanStep;
use crate::workflow::{GateSpec, GateType, PhaseExecutor};

const EXTRACT_PREPARE_STEP: &str = "extract-prepare";
const EXTRACT_APPLY_STEP: &str = "extract-apply";
const EXTRACT_NEXT_STEP: &str = "extract-next";
const WORKLIST_REL: &str = ".wicked/domain/worklist.json";
const RULES_REL: &str = ".wicked/domain/rules.json";
const EXTRACTOR_SKILL: &str = "wicked-garden-domain-extractor";
const RULES_REVIEW_INSTRUCTIONS: &str =
    "Review the rules the extract step stated in `.wicked/domain/rules.json` against the units of \
`.wicked/domain/worklist.json`. This run maps the repository one page at a time: the worklist is \
the current page, hub-first, of product-code units not yet resolved to a requirement (unmapped \
first, then placeholders an earlier page left empty or under the confidence threshold). \
Completeness is not your question. The host measures the share of real requirements after each \
page and maps the next page until the target is reached; the requirements graph is built in the \
domain-graph step. When the rules file carries `business_domains` (the repository's business \
domains, proposed on its first page), judge each domain too: it names an area by what the code \
does for its users, never a directory, package or module name; its purpose is one line; and its \
evidence cites real code (entry points, routes, CLI verbs, public-API nouns) that shows that \
area. Judge each rule's `domain` pick, and each of the file's `assignments`, as part of the \
work: the area the rule serves, not its file's directory. A domain the code does not show, a \
catalog of directory names, or picks that mostly follow the directory instead of the rule are \
problems you list as `ITEM n: FAIL — business_domains — <why>`. Judge each stated rule: it \
names a unit of the worklist, it states what the \
business requires rather than describing the code, it is grounded in that unit's source, and its \
confidence is not inflated. An empty statement is right where the source supports no rule: pure \
plumbing such as a re-export, a getter or a pass-through wrapper. Most product code does encode \
a rule (a check, a refusal, a limit, a default, an ordering, a permission, a state change, a \
field a caller must supply), so an empty statement on a unit whose source shows one is a MISSED \
rule, and a problem you list. Units in test, data, manifest or document files are not product \
requirements: the host records whatever is stated for them as unconfirmed, never as a validated \
requirement, so they are not a reason to fail the page; judge the page on its product-code \
units, and PASS a page that has none. An extracted rule (kind `Rule`: a Salesforce Flow, a cloud \
policy) is product behaviour whatever its file's extension, outside test and fixture files: \
judge it like code. A page's change should hold only this run's own files \
(`.wicked/domain/worklist.json`, `.wicked/domain/rules.json`, `.wicked/domain/apply-report.json`, \
`coverage-report.json`): when it does, a break that the repository's existing code already had is \
not this page's to keep, so a rule it breaks is `RULE <id>: N/A — pre-existing: <file:line> is not \
in this change`, never an ITEM or a FAIL (wicked-core#1339); a rule the page's own files break \
(in the rules file, say) is judged as usual. When the change touches any other file, list that \
file as a problem and judge the rules against it as usual. You judge work another seat did; \
change no file. List each problem as `ITEM \
n: FAIL — <symbol> — <why>` (for a missed rule, `missed: <the rule its source shows>`). Judge \
the PAGE: a weak or code-describing statement is a problem you list, and the host records every \
statement under its confidence threshold as a risk, never as a validated requirement. End with \
`VERDICT: PASS` when the page is sound as a whole (most stated rules are grounded in their \
unit's source and name worklist units, and the units whose source shows a rule got one), or \
`VERDICT: FAIL` when it is not: rules are invented, name symbols outside the worklist, were \
composed by a script from names rather than read from the source, most of them describe code \
instead of requirements, or most sourced product-code units were left empty while their source \
shows a rule (a page that states almost nothing is not sound, however correct its few rules \
are), or its `business_domains` hold a domain the cited code does not show or one named after a \
directory (a stored catalog is never replaced, so an unsound one must not pass).";
const COVERAGE_REPORT_FILE: &str = "coverage-report.json";
const COVERAGE_VALIDATOR_PIN: &str = "9a2c71609c3e3ced";
const DOMAIN_COVERAGE_SKILL: &str = "wicked-garden-domain-coverage";

pub(super) fn domain_extraction_preset() -> Vec<PlanStep> {
    use crate::domain_extraction::garden_domain_step_cmd;
    use crate::workflow::{CODE_GRAPH_DB_TOKEN, REPO_ROOT_TOKEN};
    let step = |catalog: &str, id: &str, after: Option<&str>| PlanStep {
        catalog: catalog.to_string(),
        id: id.to_string(),
        depends_on: after.map(|a| vec![a.to_string()]),
        ..PlanStep::default()
    };
    vec![
        PlanStep {
            gate_type: Some(None),
            skill_ref: Some("wicked-garden-domain".to_string()),
            ..step("understand", "survey", None)
        },
        PlanStep {
            gate_type: Some(None),
            skill_ref: Some("wicked-garden-domain".to_string()),
            ..step("understand", "analyze", Some("survey"))
        },
        // wicked-garden#1362: the seat cannot reach a store or the engine's binaries, so the host
        // prepares its work list and applies what it states — files in the worktree between them.
        PlanStep {
            gate_type: Some(None),
            executor: Some(PhaseExecutor::Tool {
                cmd: garden_domain_step_cmd(
                    EXTRACT_PREPARE_STEP,
                    "scripts/domain/prepare_worklist.py",
                    &format!("--out {WORKLIST_REL}"),
                ),
            }),
            ..step("run", EXTRACT_PREPARE_STEP, Some("analyze"))
        },
        PlanStep {
            skill_ref: Some(EXTRACTOR_SKILL.to_string()),
            required_deliverables: Some(vec![RULES_REL.to_string()]),
            ..step("produce", "extract", Some(EXTRACT_PREPARE_STEP))
        },
        // The page's own review (a `critique`, so the floor adds no generic one that judges a page
        // as an incomplete model): before the host writes anything.
        PlanStep {
            instructions: Some(RULES_REVIEW_INSTRUCTIONS.to_string()),
            ..step("critique", "critique", Some("extract"))
        },
        PlanStep {
            gate_type: Some(None),
            executor: Some(PhaseExecutor::Tool {
                cmd: garden_domain_step_cmd(
                    EXTRACT_APPLY_STEP,
                    "scripts/domain/apply_rules.py",
                    &format!("--rules {RULES_REL} --coverage-out {COVERAGE_REPORT_FILE}"),
                ),
            }),
            ..step("run", EXTRACT_APPLY_STEP, Some("critique"))
        },
        // domain-graph-3: the generation keeps going until the repository's target or the
        // operator stops it — this host step decides each page's next (another page from the
        // frontier, the budget's ask, or done), a bounded loop back to `extract-prepare`.
        PlanStep {
            gate_type: Some(None),
            executor: Some(PhaseExecutor::Tool {
                cmd: garden_domain_step_cmd(EXTRACT_NEXT_STEP, "scripts/domain/next_page.py", ""),
            }),
            repeat_from: Some(EXTRACT_PREPARE_STEP.to_string()),
            ..step("run", EXTRACT_NEXT_STEP, Some(EXTRACT_APPLY_STEP))
        },
        PlanStep {
            executes_code: Some(true),
            required_deliverables: Some(vec!["coverage-report.json".to_string()]),
            skill_ref: Some(DOMAIN_COVERAGE_SKILL.to_string()),
            validator_pin: Some(Some(COVERAGE_VALIDATOR_PIN.to_string())),
            ..step("domain_coverage", "coverage", Some(EXTRACT_NEXT_STEP))
        },
        PlanStep {
            kind: Some(StageKind::Build),
            gate_type: Some(Some(GateType::Strategy)),
            gate: Some(GateSpec::HumanConfirm {
                unconditional: false,
            }),
            executor: Some(PhaseExecutor::Tool {
                cmd: [
                    "wicked-core",
                    "domain-graph",
                    "--db",
                    CODE_GRAPH_DB_TOKEN,
                    // The graph is saved into the repo's estate store (`--db`), where crew reads
                    // it (`requirements_graph_for_repo`); no file is written into the checkout
                    // (operator ruling 2026-10-31). `--repo-root` names the checkout whose
                    // tracked files the corroboration reads. The coverage minimum reaches the
                    // step through the daemon's environment
                    // (`WICKED_REQUIREMENTS_MIN_COVERAGE_PCT`, crew's setting).
                    "--repo-root",
                    REPO_ROOT_TOKEN,
                ]
                .map(str::to_string)
                .to_vec(),
            }),
            ..step("run", "domain-graph", Some("coverage"))
        },
    ]
}
