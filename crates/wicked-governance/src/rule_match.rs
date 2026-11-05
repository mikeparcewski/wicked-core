//! RULE MATCH — which in-force rules a job must be checked against (DES-RULE-CHECK-001 stage 1, slice S1).
//!
//! Every rule handed in comes back with exactly ONE status and the facts behind it; none is dropped:
//!
//! * **not applicable**: a recorded fact rules it out. No step of the job selects it, with the gate's own semantics
//!   ([`crate::select_any`]: a step is selected when one of its selection tokens is in `applies_to`, literally, and
//!   none is in `excludes`; a step's tokens are its synthetic `unit-<n>`, its phase id and its catalog id, as
//!   `wicked_core::scope::phase_aliases` builds them); its `applies_to` names only tool tokens, which the MCP and
//!   editor gates judge per call; its `targets.project` names another project; a `targets.language` / `framework` /
//!   `layer` names none of the job's; or its `trigger.contains` matches nothing in the job's diff.
//! * **applies**: a fact selects it and none rules it out: its project is the job's, a step selects it, a facet
//!   matches, or its trigger matches the diff.
//! * **may apply**: no fact either way. It is checked, never dropped. Its facts say what is still open (a step scope
//!   whose steps are not known yet, or no scope at all).
//!
//! A fact that is not known yet stays out of the decision. At launch the job's steps are not known (a workflow run
//! may be planned into catalog steps whose ids are the planner's, as the end-to-end job 4ded4922 was), so a launch
//! passes no `steps` and step scopes decide nothing; at launch there is no diff either, so facets and triggers decide
//! nothing. An unreadable trigger regex rules nothing out. So "not applicable" is only ever a fact.
//!
//! Pure and deterministic: no store, no model. The order is the order a reader needs: applies, may apply, not
//! applicable; within each, rules whose steering type the job's kinds name first (`build` names `development`,
//! `test` names `testing`), then severity (critical first), then id.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::conformance::{ConfSeverity, ConformanceRule};

/// The three statuses (wire: snake_case).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchStatus {
    Applies,
    MayApply,
    NotApplicable,
}

/// One fact behind a status. `kind` is a stable token (`phase`, `tool`, `project`, `language`, `framework`, `layer`,
/// `trigger`, `retired`, `unscoped`); `detail` says it in words.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchFact {
    pub kind: String,
    pub detail: String,
}

/// A rule's match: its status and every fact that decided it (for `may_apply`, what is still open).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleMatch {
    pub rule_id: String,
    pub status: MatchStatus,
    pub facts: Vec<MatchFact>,
}

/// One step of the job as the gate sees it: every selection token it carries (its synthetic `unit-<n>`, its phase id,
/// its catalog id) and its kind (`build`, `test`, …).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobStep {
    pub tokens: Vec<String>,
    #[serde(default)]
    pub kind: Option<String>,
}

/// What is known about the job. `None` = not known yet (it decides nothing); `Some(empty)` = known to be none.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobFacts {
    /// The workflow id, for the words only.
    #[serde(default)]
    pub workflow: Option<String>,
    /// The job's steps with their selection tokens (review time). `None` (a launch, whose steps are planned later):
    /// `applies_to` / `excludes` decide nothing.
    #[serde(default)]
    pub steps: Option<Vec<JobStep>>,
    /// The step kinds the job is expected to run (`build`, `test`, …), for the reading order only.
    #[serde(default)]
    pub kinds: Option<Vec<String>>,
    /// The job's project (`None` = the job has none: a project-scoped rule does not apply, as in recall).
    #[serde(default)]
    pub project: Option<String>,
    /// The changed files' languages / frameworks / layers (review time).
    #[serde(default)]
    pub languages: Option<Vec<String>>,
    #[serde(default)]
    pub frameworks: Option<Vec<String>>,
    #[serde(default)]
    pub layers: Option<Vec<String>>,
    /// The diff text a trigger is matched against (review time).
    #[serde(default)]
    pub diff: Option<String>,
    /// The job's repository (its registered id or name, e.g. `tiny-math`): a rule that names it in its
    /// statement or its source document applies (E7 rerun: a tiny-math rule came back "may apply").
    #[serde(default)]
    pub repo: Option<String>,
    /// The changed files (review time): a rule whose statement names one applies.
    #[serde(default)]
    pub paths: Option<Vec<String>>,
}

/// The facets a match reads, so a caller holding a partial rule (crew's wire shape) can match it too.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MatchRule {
    pub id: String,
    #[serde(default = "default_severity")]
    pub severity: ConfSeverity,
    #[serde(default)]
    pub steering_type: Option<String>,
    #[serde(default)]
    pub applies_to: Vec<String>,
    #[serde(default)]
    pub excludes: Vec<String>,
    #[serde(default)]
    pub targets: MatchTargets,
    #[serde(default)]
    pub trigger: Option<MatchTrigger>,
    #[serde(default)]
    pub retired: bool,
    /// The rule's statement: a repository or a changed file it names is a fact that selects it.
    #[serde(default)]
    pub statement: String,
    /// Where the rule came from (`provenance.ref`, e.g. `tiny-math-tests.md@…#DEV-TINYMATH-2`).
    #[serde(default)]
    pub source_ref: Option<String>,
    /// The wire's own `provenance` (crew sends whole rules): its `ref` stands in for `source_ref`.
    #[serde(default, skip_serializing)]
    pub provenance: Option<MatchProvenance>,
}

/// A rule's `provenance`, as far as the match reads it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchProvenance {
    #[serde(default, rename = "ref")]
    pub reference: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchTargets {
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub layer: Option<String>,
    #[serde(default)]
    pub framework: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchTrigger {
    #[serde(default)]
    pub contains: Option<String>,
}

fn default_severity() -> ConfSeverity {
    ConfSeverity::Info
}

impl Default for MatchRule {
    fn default() -> Self {
        MatchRule {
            id: String::new(),
            severity: default_severity(),
            steering_type: None,
            applies_to: Vec::new(),
            excludes: Vec::new(),
            targets: MatchTargets::default(),
            trigger: None,
            retired: false,
            statement: String::new(),
            source_ref: None,
            provenance: None,
        }
    }
}

impl From<&ConformanceRule> for MatchRule {
    fn from(r: &ConformanceRule) -> Self {
        MatchRule {
            id: r.id.clone(),
            severity: r.severity,
            steering_type: Some(r.steering_type.clone()),
            applies_to: r.applies_to.clone(),
            excludes: r.excludes.clone(),
            targets: MatchTargets {
                language: r.targets.language.clone(),
                layer: r.targets.layer.clone(),
                framework: r.targets.framework.clone(),
                project: r.targets.project.clone(),
            },
            trigger: r.trigger.as_ref().map(|t| MatchTrigger {
                contains: t.contains.clone(),
            }),
            retired: r.retired,
            statement: r.statement.clone(),
            source_ref: r
                .provenance
                .reference
                .clone()
                .filter(|x| !x.trim().is_empty()),
            provenance: None,
        }
    }
}

/// A tool token (`mcp`, `mcp:<server>[/<tool>]`, `mcp-mode:<mode>`, `editor`, `editor:<id>[/<perm>]`,
/// `editor-perm:<perm>`, `editor-origin:<origin>`): the MCP gate and the editor grants gate judge these per call and
/// per grant, not a review.
fn tool_token(t: &str) -> bool {
    // The gates' own vocabulary (core `mcp_gate::subject_tokens`, `editor_gate::subject_tokens`), not a prefix guess:
    // a phase named `editor-build` is a phase (xreview r2 on S1).
    t == "mcp"
        || t == "editor"
        || [
            "mcp:",
            "mcp-mode:",
            "editor:",
            "editor-perm:",
            "editor-origin:",
        ]
        .iter()
        .any(|p| t.starts_with(p))
}

fn list(items: &[String]) -> String {
    if items.is_empty() {
        "none".to_string()
    } else {
        items.join(", ")
    }
}

fn eq_ci(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

/// A wildcard facet (`language` / `framework` / `layer`) against what the job is known to touch.
fn facet(
    name: &str,
    want: &Option<String>,
    have: &Option<Vec<String>>,
    na: &mut Vec<MatchFact>,
    yes: &mut Vec<MatchFact>,
) {
    let (Some(want), Some(have)) = (
        want.as_deref().filter(|w| !w.trim().is_empty()),
        have.as_ref(),
    ) else {
        return;
    };
    if have.iter().any(|h| eq_ci(h, want)) {
        yes.push(MatchFact {
            kind: name.to_string(),
            detail: format!("targets {name} {want}, which this job touches"),
        });
    } else {
        na.push(MatchFact {
            kind: name.to_string(),
            detail: format!("targets {name} {want}; this job touches {}", list(have)),
        });
    }
}

/// A step's name in words: its first token that is not the synthetic `unit-<n>` (else that one).
fn step_name(step: &JobStep) -> Option<String> {
    step.tokens
        .iter()
        .find(|t| !t.starts_with("unit-"))
        .or(step.tokens.first())
        .cloned()
}

/// Whether `text` names `token` as a whole word: not inside a longer identifier or path
/// (`tiny-math` in "tiny-math's src" or `tiny-math-tests.md`, never in `tiny-mathx`), case-insensitively.
fn names_token(text: &str, token: &str) -> bool {
    let (text, token) = (text.to_ascii_lowercase(), token.to_ascii_lowercase());
    if token.is_empty() {
        return false;
    }
    let ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut from = 0;
    while let Some(i) = text[from..].find(&token) {
        let at = from + i;
        let before = text[..at].chars().next_back();
        let after = text[at + token.len()..].chars().next();
        if !before.is_some_and(|c| ident(c) || c == '/') && !after.is_some_and(ident) {
            return true;
        }
        from = at + token.len().max(1);
    }
    false
}

/// Match ONE rule against the job.
pub fn match_rule(rule: &MatchRule, job: &JobFacts) -> RuleMatch {
    let mut na: Vec<MatchFact> = Vec::new();
    let mut yes: Vec<MatchFact> = Vec::new();
    let mut open: Vec<MatchFact> = Vec::new();

    if rule.retired {
        na.push(MatchFact {
            kind: "retired".into(),
            detail: "the rule is retired".into(),
        });
    }

    // Project: asymmetric, as in recall (a rule with a project applies to that project only).
    if let Some(p) = rule
        .targets
        .project
        .as_deref()
        .filter(|p| !p.trim().is_empty())
    {
        match job.project.as_deref() {
            Some(j) if j == p => yes.push(MatchFact {
                kind: "project".into(),
                detail: format!("scoped to this job's project {p}"),
            }),
            Some(j) => na.push(MatchFact {
                kind: "project".into(),
                detail: format!("scoped to project {p}; this job's project is {j}"),
            }),
            None => na.push(MatchFact {
                kind: "project".into(),
                detail: format!("scoped to project {p}; this job has no project"),
            }),
        }
    }

    // Tool tokens: the MCP and editor gates own these rules, call by call.
    let tool_only = !rule.applies_to.is_empty() && rule.applies_to.iter().all(|t| tool_token(t));
    if tool_only {
        na.push(MatchFact {
            kind: "tool".into(),
            detail: format!(
                "selected only for tool tokens ({}), which the MCP and editor gates judge on each call, not the review",
                list(&rule.applies_to)
            ),
        });
    }

    // Steps: the gate's own selection (select_any), token for token, literally.
    let scoped = !tool_only && (!rule.applies_to.is_empty() || !rule.excludes.is_empty());
    let scope_words = || {
        if rule.applies_to.is_empty() {
            format!("excluded from {}", list(&rule.excludes))
        } else if rule.excludes.is_empty() {
            format!("selected for {}", list(&rule.applies_to))
        } else {
            format!(
                "selected for {}, excluded from {}",
                list(&rule.applies_to),
                list(&rule.excludes)
            )
        }
    };
    if scoped {
        match job.steps.as_ref() {
            Some(steps) => {
                let selects = |step: &JobStep| {
                    let included = rule.applies_to.is_empty()
                        || step.tokens.iter().any(|t| rule.applies_to.contains(t));
                    included && !step.tokens.iter().any(|t| rule.excludes.contains(t))
                };
                let hits: Vec<String> = steps
                    .iter()
                    .filter(|s| selects(s))
                    .filter_map(step_name)
                    .collect();
                if !hits.is_empty() {
                    yes.push(MatchFact {
                        kind: "phase".into(),
                        detail: format!("selected for this job's {} step(s)", list(&hits)),
                    });
                } else {
                    let all: Vec<String> = steps.iter().filter_map(step_name).collect();
                    na.push(MatchFact {
                        kind: "phase".into(),
                        detail: format!("{}; this job's steps are {}", scope_words(), list(&all)),
                    });
                }
            }
            None => open.push(MatchFact {
                kind: "phase".into(),
                detail: format!(
                    "{}; this job's steps are set with its plan, so the review decides",
                    scope_words()
                ),
            }),
        }
    }

    // The job's repository, named by the rule (its statement, or its source document's name): it applies.
    if let Some(repo) = job.repo.as_deref().map(str::trim).filter(|r| !r.is_empty()) {
        let doc = rule
            .source_ref
            .as_deref()
            .or(rule
                .provenance
                .as_ref()
                .and_then(|p| p.reference.as_deref()))
            .map(|r| r.split(['@', '#']).next().unwrap_or(r))
            .map(|d| d.rsplit('/').next().unwrap_or(d))
            .unwrap_or("");
        if names_token(&rule.statement, repo) || names_token(doc, repo) {
            yes.push(MatchFact {
                kind: "repo".into(),
                detail: format!("it names this job's repository {repo}"),
            });
        }
    }
    // A changed file the rule names (review time): it applies.
    if let Some(paths) = job.paths.as_ref() {
        let named: Vec<&str> = paths
            .iter()
            .map(String::as_str)
            // A path, or a root file with an extension (`package.json`); never a bare word.
            .filter(|p| {
                (p.contains('/') || (p.contains('.') && p.len() >= 5))
                    && names_token(&rule.statement, p)
            })
            .collect();
        if !named.is_empty() {
            yes.push(MatchFact {
                kind: "path".into(),
                detail: format!("it names the changed file(s) {}", named.join(", ")),
            });
        }
    }

    facet(
        "language",
        &rule.targets.language,
        &job.languages,
        &mut na,
        &mut yes,
    );
    facet(
        "framework",
        &rule.targets.framework,
        &job.frameworks,
        &mut na,
        &mut yes,
    );
    facet("layer", &rule.targets.layer, &job.layers, &mut na, &mut yes);

    if let (Some(pattern), Some(diff)) = (
        rule.trigger.as_ref().and_then(|t| t.contains.as_deref()),
        job.diff.as_deref(),
    ) {
        // An unreadable regex decides nothing: the rule is checked, never ruled out by a typo.
        if let Ok(re) = regex::Regex::new(pattern) {
            if re.is_match(diff) {
                yes.push(MatchFact {
                    kind: "trigger".into(),
                    detail: format!("its trigger {pattern:?} matches the diff"),
                });
            } else {
                na.push(MatchFact {
                    kind: "trigger".into(),
                    detail: format!("its trigger {pattern:?} matches nothing in the diff"),
                });
            }
        }
    }

    let (status, facts) = if !na.is_empty() {
        (MatchStatus::NotApplicable, na)
    } else if !open.is_empty() {
        // An open step scope outranks a project or facet hit: the rule may still be selected for no step.
        (MatchStatus::MayApply, yes.into_iter().chain(open).collect())
    } else if !yes.is_empty() {
        (MatchStatus::Applies, yes)
    } else {
        (
            MatchStatus::MayApply,
            vec![MatchFact {
                kind: "unscoped".into(),
                detail: "no project, step, facet or trigger scopes it; the review checks it".into(),
            }],
        )
    };
    RuleMatch {
        rule_id: rule.id.clone(),
        status,
        facts,
    }
}

/// The steering types the job's step kinds name (a `build` step: development; a `test` step: testing).
fn kind_types(job: &JobFacts) -> BTreeSet<&'static str> {
    let kinds = job
        .kinds
        .iter()
        .flatten()
        .map(String::as_str)
        .chain(job.steps.iter().flatten().filter_map(|s| s.kind.as_deref()));
    let mut out = BTreeSet::new();
    for k in kinds {
        match k {
            "build" => {
                out.insert("development");
            }
            "test" => {
                out.insert("testing");
            }
            _ => {}
        }
    }
    out
}

/// Match every rule, one result each, in reading order (see the module docs). Never drops a rule.
pub fn match_rules(rules: &[MatchRule], job: &JobFacts) -> Vec<RuleMatch> {
    let types = kind_types(job);
    let mut rows: Vec<(&MatchRule, RuleMatch)> =
        rules.iter().map(|r| (r, match_rule(r, job))).collect();
    rows.sort_by(|(ra, a), (rb, b)| {
        let named = |r: &MatchRule| {
            !r.steering_type
                .as_deref()
                .is_some_and(|t| types.contains(t))
        };
        a.status
            .cmp(&b.status)
            .then_with(|| named(ra).cmp(&named(rb)))
            .then_with(|| rb.severity.rank().cmp(&ra.severity.rank()))
            .then_with(|| ra.id.cmp(&rb.id))
    });
    rows.into_iter().map(|(_, m)| m).collect()
}

#[derive(Deserialize)]
struct MatchArgs {
    rules: Vec<MatchRule>,
    #[serde(default)]
    job: JobFacts,
}

/// JSON entry (the core-ts `matchRules` binding): `{ rules: [rule…], job: {workflow?, steps?: [{tokens, kind?}],
/// kinds?, project?, languages?, frameworks?, layers?, diff?} }` → `{ matches: [{rule_id, status, facts: [{kind,
/// detail}]}] }`, one match per rule, in reading order.
pub fn match_rules_json(args_json: &str) -> anyhow::Result<String> {
    let args: MatchArgs =
        serde_json::from_str(args_json).map_err(|e| anyhow::anyhow!("bad_request: {e}"))?;
    let matches = match_rules(&args.rules, &args.job);
    Ok(serde_json::to_string(
        &serde_json::json!({ "matches": matches }),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(id: &str, sev: ConfSeverity, ty: &str, applies: &[&str]) -> MatchRule {
        MatchRule {
            id: id.into(),
            severity: sev,
            steering_type: Some(ty.into()),
            applies_to: applies.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    /// A bug launch: its steps are not known yet; its def's kinds are.
    fn bug_launch() -> JobFacts {
        JobFacts {
            workflow: Some("bug".into()),
            kinds: Some(vec![
                "recon".into(),
                "test".into(),
                "build".into(),
                "test".into(),
            ]),
            ..Default::default()
        }
    }

    /// The E7 job at review: the catalog plan it actually ran (unit-<n>, step id, catalog id).
    fn bug_review() -> JobFacts {
        let st = |n: u32, id: &str, cat: &str, kind: &str| JobStep {
            tokens: vec![format!("unit-{n}"), id.into(), cat.into()],
            kind: Some(kind.into()),
        };
        JobFacts {
            workflow: Some("bug".into()),
            steps: Some(vec![
                st(1, "pa-scope", "understand", "recon"),
                st(2, "understand", "understand", "recon"),
                st(3, "test_plan", "test_plan", "test"),
                st(4, "build", "build", "build"),
                st(5, "review", "review", "review"),
                st(6, "test", "test", "test"),
                st(7, "deliver", "deliver", "build"),
            ]),
            ..Default::default()
        }
    }

    const MCPS: &[&str] = &[
        "scope",
        "design",
        "build",
        "test",
        "security-review",
        "observability-review",
        "install",
    ];
    const TOOL_RULES: [&str; 11] = [
        "EDITOR-GRANTS",
        "MCP-FIRST-USE",
        "MCP-MODE-ASK-WRITE",
        "MCP-POSTURE-WRITE",
        "EDITOR-BUILTIN",
        "EDITOR-BUILTIN-DOC-MEDIA",
        "EDITOR-BUILTIN-PAGE-MEDIA",
        "EDITOR-BUILTIN-THEME",
        "EDITOR-OPEN-DEFAULTS",
        "MCP-POSTURE-READ",
        "MCP-POSTURE-READ-2",
    ];

    /// The rules in force on the test rig for the E7 job (4ded4922), with the facets they carry there (plus one more
    /// MCP read rule, to make the tool set 11).
    fn rig_rules() -> Vec<MatchRule> {
        use ConfSeverity::*;
        let mut v = vec![rule("MCPS-1004", Critical, "security", MCPS)];
        v.push(rule("EDITOR-GRANTS", Error, "security", &["editor"]));
        let mut first_use = rule("MCP-FIRST-USE", Error, "security", &["mcp"]);
        first_use.excludes = vec!["mcp:unit-convert/convert_length".into()];
        v.push(first_use);
        v.push(rule(
            "MCP-MODE-ASK-WRITE",
            Error,
            "security",
            &["mcp-mode:ask"],
        ));
        v.push(rule(
            "MCP-POSTURE-WRITE",
            Error,
            "security",
            &["mcp-mode:balanced"],
        ));
        for (id, ty) in [
            ("MCPS-1001", "architecture"),
            ("MCPS-1002", "operations"),
            ("MCPS-1003", "operations"),
            ("MCPS-1005", "security"),
            ("MCPS-1006", "testing"),
        ] {
            v.push(rule(id, Error, ty, MCPS));
        }
        v.push(rule("POL-2101", Error, "development", &["build", "review"]));
        v.push(rule(
            "RVWL-1001",
            Error,
            "testing",
            &[
                "review",
                "adversarial-review",
                "test",
                "verify",
                "security-review",
                "observability-review",
            ],
        ));
        v.push(rule(
            "RVWL-1002",
            Error,
            "development",
            &["build", "implement", "fix", "execute", "test", "verify"],
        ));
        v.push(rule(
            "RVWL-1003",
            Error,
            "operations",
            &[
                "build",
                "implement",
                "fix",
                "review",
                "adversarial-review",
                "test",
                "verify",
            ],
        ));
        v.push(rule("DEV-TINYMATH-1", Warn, "development", &[]));
        v.push(rule("DEV-TINYMATH-2", Warn, "development", &[]));
        v.push(rule("MCPS-1007", Warn, "compliance", MCPS));
        v.push(rule(
            "RVWL-1004",
            Warn,
            "operations",
            &["scope", "clarify", "plan", "design"],
        ));
        for (id, ty) in [
            ("learn-accessible", "design-ux"),
            ("learn-linked-issue", "development"),
            ("learn-no-secrets", "security"),
            ("learn-reversible-migrations", "operations"),
            ("learn-small-prs", "development"),
            ("learn-tests-with-fixes", "testing"),
        ] {
            v.push(rule(id, Warn, ty, &[]));
        }
        for id in [
            "EDITOR-BUILTIN",
            "EDITOR-BUILTIN-DOC-MEDIA",
            "EDITOR-BUILTIN-PAGE-MEDIA",
            "EDITOR-BUILTIN-THEME",
        ] {
            v.push(rule(id, Info, "security", &["editor-origin:first-party"]));
        }
        v.push(rule("EDITOR-OPEN-DEFAULTS", Info, "security", &["editor"]));
        v.push(rule("MCP-POSTURE-READ", Info, "security", &["mcp"]));
        v.push(rule("MCP-POSTURE-READ-2", Info, "security", &["mcp"]));
        v
    }

    fn status_of(out: &[RuleMatch], id: &str) -> MatchStatus {
        out.iter().find(|m| m.rule_id == id).unwrap().status
    }

    #[test]
    fn the_rig_bug_launch_lists_learn_tests_with_fixes_and_rules_out_only_the_tool_rules_by_fact() {
        let rules = rig_rules();
        let out = match_rules(&rules, &bug_launch());
        assert_eq!(out.len(), rules.len(), "no rule is dropped");
        assert_eq!(
            status_of(&out, "learn-tests-with-fixes"),
            MatchStatus::MayApply
        );
        assert_eq!(status_of(&out, "DEV-TINYMATH-2"), MatchStatus::MayApply);
        for id in TOOL_RULES {
            let m = out.iter().find(|m| m.rule_id == id).unwrap();
            assert_eq!(m.status, MatchStatus::NotApplicable, "{id}");
            assert_eq!(m.facts[0].kind, "tool");
            assert!(
                m.facts[0]
                    .detail
                    .contains("MCP and editor gates judge on each call"),
                "{m:?}"
            );
        }
        // At launch the steps are not known: a step-scoped rule (the MCP-server rules) is not ruled out, and says why.
        let mcps = out.iter().find(|m| m.rule_id == "MCPS-1004").unwrap();
        assert_eq!(mcps.status, MatchStatus::MayApply);
        assert!(
            mcps.facts[0].detail.contains("set with its plan"),
            "{mcps:?}"
        );
        assert_eq!(
            out.iter()
                .filter(|m| m.status == MatchStatus::NotApplicable)
                .count(),
            TOOL_RULES.len()
        );
        // Reading order: may apply before not applicable; the bug kinds' development/testing rules lead.
        assert_eq!(out[0].status, MatchStatus::MayApply);
        assert!(["testing", "development"].contains(
            &rules
                .iter()
                .find(|r| r.id == out[0].rule_id)
                .unwrap()
                .steering_type
                .as_deref()
                .unwrap()
        ));
    }

    #[test]
    fn at_review_the_steps_the_job_ran_decide_with_the_gates_own_tokens() {
        let out = match_rules(&rig_rules(), &bug_review());
        // The catalog plan ran build and test steps: the MCP-server rules ARE selected by the gate there.
        assert_eq!(status_of(&out, "MCPS-1004"), MatchStatus::Applies);
        assert_eq!(status_of(&out, "POL-2101"), MatchStatus::Applies);
        // RVWL-1004 names scope, clarify, plan, design: no step of this job carries one.
        let r = out.iter().find(|m| m.rule_id == "RVWL-1004").unwrap();
        assert_eq!(r.status, MatchStatus::NotApplicable);
        assert!(r.facts[0].detail.contains("this job's steps are pa-scope, understand, test_plan, build, review, test, deliver"), "{r:?}");
        assert_eq!(
            status_of(&out, "learn-tests-with-fixes"),
            MatchStatus::MayApply
        );
    }

    #[test]
    fn tokens_are_literal_and_exclusion_dominates_per_step_as_in_select_any() {
        let job = bug_review();
        // A padded token is not the step's token (select_any compares literally).
        assert_eq!(
            match_rule(
                &rule("P", ConfSeverity::Warn, "development", &[" build "]),
                &job
            )
            .status,
            MatchStatus::NotApplicable
        );
        // Included by `build` but excluded by the same step's catalog token: that step does not select it.
        let mut ex = rule("X", ConfSeverity::Warn, "development", &["build"]);
        ex.excludes = vec!["build".into()];
        assert_eq!(match_rule(&ex, &job).status, MatchStatus::NotApplicable);
        // An exclusion with a padded token excludes nothing.
        ex.excludes = vec![" build ".into()];
        assert_eq!(match_rule(&ex, &job).status, MatchStatus::Applies);
        // The synthetic unit token selects.
        assert_eq!(
            match_rule(
                &rule("U", ConfSeverity::Warn, "development", &["unit-4"]),
                &job
            )
            .status,
            MatchStatus::Applies
        );
        // excludes alone (no applies_to): excluded from every step = not applicable.
        let mut all = rule("A", ConfSeverity::Warn, "development", &[]);
        all.excludes = [
            "understand",
            "test_plan",
            "build",
            "review",
            "test",
            "deliver",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(match_rule(&all, &job).status, MatchStatus::NotApplicable);
    }

    #[test]
    fn only_the_gates_own_tool_vocabulary_is_a_tool_token() {
        // A phase that merely starts like a tool token is a phase: not ruled out at launch, selected at review.
        let mut job = bug_review();
        job.steps.as_mut().unwrap().push(JobStep {
            tokens: vec!["unit-8".into(), "editor-build".into()],
            kind: Some("build".into()),
        });
        for t in ["editor-build", "mcp-server-review"] {
            assert_eq!(
                match_rule(
                    &rule("P", ConfSeverity::Warn, "development", &[t]),
                    &bug_launch()
                )
                .status,
                MatchStatus::MayApply,
                "{t}"
            );
        }
        assert_eq!(
            match_rule(
                &rule("P", ConfSeverity::Warn, "development", &["editor-build"]),
                &job
            )
            .status,
            MatchStatus::Applies
        );
        for t in [
            "mcp",
            "mcp:jira",
            "mcp:jira/search",
            "mcp-mode:ask",
            "editor",
            "editor:x/read",
            "editor-perm:checks.read",
            "editor-origin:first-party",
        ] {
            assert_eq!(
                match_rule(
                    &rule("T", ConfSeverity::Warn, "security", &[t]),
                    &bug_launch()
                )
                .status,
                MatchStatus::NotApplicable,
                "{t}"
            );
        }
    }

    #[test]
    fn a_hundred_rules_come_back_as_a_hundred_matches_and_none_is_dropped() {
        let mut rules = Vec::new();
        for i in 0..100 {
            let applies: &[&str] = match i % 4 {
                0 => MCPS,
                1 => &["fix"],
                2 => &[],
                _ => &["mcp"],
            };
            rules.push(rule(
                &format!("R-{i:03}"),
                ConfSeverity::Warn,
                "development",
                applies,
            ));
        }
        for job in [bug_launch(), bug_review()] {
            let out = match_rules(&rules, &job);
            assert_eq!(out.len(), 100);
            let ids: BTreeSet<&str> = out.iter().map(|m| m.rule_id.as_str()).collect();
            assert_eq!(ids.len(), 100);
            assert!(out.iter().all(|m| !m.facts.is_empty()));
        }
        let launch = match_rules(&rules, &bug_launch());
        assert_eq!(
            launch
                .iter()
                .filter(|m| m.status == MatchStatus::NotApplicable)
                .count(),
            25,
            "only the tool rules"
        );
        let review = match_rules(&rules, &bug_review());
        assert_eq!(
            review
                .iter()
                .filter(|m| m.status == MatchStatus::Applies)
                .count(),
            25,
            "MCPS-scoped: build, test steps"
        );
        assert_eq!(
            review
                .iter()
                .filter(|m| m.status == MatchStatus::NotApplicable)
                .count(),
            50,
            "fix-scoped and tool rules"
        );
    }

    #[test]
    fn unknown_facts_decide_nothing_and_never_rule_a_rule_out() {
        // A language facet with no diff yet decides nothing; with the job's languages it does.
        let mut lang = rule("L", ConfSeverity::Warn, "development", &[]);
        lang.targets.language = Some("rust".into());
        assert_eq!(
            match_rule(&lang, &bug_launch()).status,
            MatchStatus::MayApply
        );
        let mut js = bug_launch();
        js.languages = Some(vec!["javascript".into()]);
        let m = match_rule(&lang, &js);
        assert_eq!(
            (m.status, m.facts[0].kind.as_str()),
            (MatchStatus::NotApplicable, "language")
        );
        js.languages = Some(vec!["Rust".into()]);
        assert_eq!(match_rule(&lang, &js).status, MatchStatus::Applies);
        // A facet hit with the steps still open stays may apply (the rule may be selected for no step).
        let mut both = rule("B", ConfSeverity::Warn, "development", &["build"]);
        both.targets.language = Some("rust".into());
        assert_eq!(match_rule(&both, &js).status, MatchStatus::MayApply);
    }

    #[test]
    fn project_and_trigger_are_facts() {
        let mut p = rule("P", ConfSeverity::Warn, "development", &[]);
        p.targets.project = Some("proj_a".into());
        let mut job = bug_launch();
        assert_eq!(
            match_rule(&p, &job).status,
            MatchStatus::NotApplicable,
            "a project rule on a job with no project"
        );
        job.project = Some("proj_a".into());
        let m = match_rule(&p, &job);
        assert_eq!(
            (m.status, m.facts[0].kind.as_str()),
            (MatchStatus::Applies, "project")
        );
        let mut t = rule("T", ConfSeverity::Warn, "security", &[]);
        t.trigger = Some(MatchTrigger {
            contains: Some(r"push\s+--force".into()),
        });
        let mut diff = bug_launch();
        diff.diff = Some("git push origin fix".into());
        assert_eq!(match_rule(&t, &diff).status, MatchStatus::NotApplicable);
        diff.diff = Some("git push --force".into());
        assert_eq!(match_rule(&t, &diff).status, MatchStatus::Applies);
        t.trigger = Some(MatchTrigger {
            contains: Some("(".into()),
        });
        assert_eq!(
            match_rule(&t, &diff).status,
            MatchStatus::MayApply,
            "an unreadable regex rules nothing out"
        );
    }

    #[test]
    fn the_json_entry_reads_a_partial_rule_and_answers_one_match_per_rule() {
        let out = match_rules_json(
            r#"{"rules":[{"id":"A","severity":"warn","applies_to":["install"]},{"id":"B"},{"id":"C","applies_to":["mcp"]}],
                "job":{"workflow":"bug","steps":[{"tokens":["unit-1","fix","build"],"kind":"build"}]}}"#,
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let m = v["matches"].as_array().unwrap();
        assert_eq!(m.len(), 3);
        assert_eq!(m[0]["rule_id"], "B");
        assert_eq!(m[0]["status"], "may_apply");
        assert_eq!(m[1]["status"], "not_applicable");
        assert_eq!(m[1]["facts"][0]["kind"], "phase");
        assert_eq!(m[2]["facts"][0]["kind"], "tool");
        assert!(match_rules_json("{")
            .unwrap_err()
            .to_string()
            .starts_with("bad_request:"));
    }

    /// E7 rerun (coord, 2026-11-05): a rule that names the job's repository, in its statement or its
    /// source document, applies; a changed file it names makes it apply at review; a longer name is
    /// not the repository.
    #[test]
    fn a_rule_naming_the_jobs_repo_or_a_changed_file_applies() {
        let tiny = MatchRule {
            id: "DEV-TINYMATH-2".into(),
            statement:
                "Every change to a function in tiny-math's src/math.js adds or updates a test"
                    .into(),
            ..Default::default()
        };
        let by_doc = MatchRule {
            id: "DOC".into(),
            statement: "Every exported function has a JSDoc block".into(),
            source_ref: Some("tiny-math-jsdoc.md@956a514#DEV-TINYMATH-1".into()),
            ..Default::default()
        };
        let other = MatchRule {
            id: "OTHER".into(),
            statement: "tiny-mathx keeps its cache warm".into(),
            ..Default::default()
        };
        let job = JobFacts {
            repo: Some("tiny-math".into()),
            ..Default::default()
        };
        let out = match_rules(&[tiny.clone(), by_doc, other.clone()], &job);
        let st = |id: &str| out.iter().find(|m| m.rule_id == id).unwrap().clone();
        assert_eq!(
            st("DEV-TINYMATH-2").status,
            MatchStatus::Applies,
            "{:?}",
            st("DEV-TINYMATH-2")
        );
        assert!(st("DEV-TINYMATH-2").facts[0]
            .detail
            .contains("names this job's repository tiny-math"));
        assert_eq!(st("DOC").status, MatchStatus::Applies);
        assert_eq!(st("OTHER").status, MatchStatus::MayApply);
        // No repo known: the same rule may apply (a fact not known decides nothing).
        let none = match_rules(&[tiny.clone()], &JobFacts::default());
        assert_eq!(none[0].status, MatchStatus::MayApply);
        // At review, a changed file the rule names.
        let mut by_path = tiny;
        by_path.statement = "Every change to src/math.js adds a test".into();
        let review = JobFacts {
            paths: Some(vec!["src/math.js".into()]),
            ..Default::default()
        };
        assert_eq!(
            match_rules(&[by_path], &review)[0].status,
            MatchStatus::Applies
        );
        // crew's wire sends the whole rule: `provenance.ref`, directory-qualified, names it too.
        let wire = r#"{"rules":[{"id":"D","statement":"JSDoc on every export","provenance":{"ref":"docs/tiny-math-jsdoc.md@956a#D"}}],"job":{"repo":"tiny-math"}}"#;
        assert!(match_rules_json(wire)
            .unwrap()
            .contains(r#""status":"applies""#));
        // A root file it names, at review.
        let mut root = MatchRule {
            id: "ROOT".into(),
            statement: "package.json keeps its engines field".into(),
            ..Default::default()
        };
        let pj = JobFacts {
            paths: Some(vec!["package.json".into()]),
            ..Default::default()
        };
        assert_eq!(
            match_rules(std::slice::from_ref(&root), &pj)[0].status,
            MatchStatus::Applies
        );
        root.statement = "keeps the package tidy".into();
        assert_eq!(match_rules(&[root], &pj)[0].status, MatchStatus::MayApply);
        // A rule out of scope by fact stays not applicable whatever it names.
        let mut tool = other;
        tool.statement = "tiny-math mcp calls are logged".into();
        tool.applies_to = vec!["mcp".into()];
        assert_eq!(
            match_rules(&[tool], &job)[0].status,
            MatchStatus::NotApplicable
        );
    }
}
