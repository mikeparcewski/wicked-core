//! (core#820) The write plan a `consent_before` gate offers.
//!
//! A consent gate asks before a phase with effects outside the run (the `mcp-server` install
//! writes CLI configurations). The operator can only consent to what they are shown, so the
//! workflow runs a DRY-RUN Tool phase just before the gated one: the gated unit `depends_on` it,
//! and its stdout carries one compact JSON line, the plan:
//!
//! ```json
//! {"dry_run": true,
//!  "choices": [
//!    {"id": "worker", "label": "Install for workers", "default": true,
//!     "writes": [{"path": "/h/.wicked-worker/claude/.claude.json", "what": "claude MCP config",
//!                 "cli": "claude", "operator_owned": false}]},
//!    {"id": "operator", "label": "Also install into my CLIs",
//!     "writes": [{"path": "/h/.claude.json", "what": "claude MCP config", "cli": "claude",
//!                 "operator_owned": true}]}],
//!  "skipped": [{"cli": "pi", "why": "unsupported"}]}
//! ```
//!
//! The engine never runs the installer itself and never interprets a path: the plan is data the
//! dry run produced on this host, under this run's environment. Each choice becomes an answer token
//! `consent:<id>` beside `reject` (decline). The answer rides the gated Tool unit as
//! [`CONSENT_CHOICE_ENV`], so the real step writes exactly what the chosen row listed. No plan, or
//! a plan with no choices, leaves the gate a plain approve / reject, and the prompt says the write
//! targets were not listed.

use serde::{Deserialize, Serialize};

/// The variable the gated Tool unit receives: the id of the choice the operator approved.
pub const CONSENT_CHOICE_ENV: &str = "WICKED_CONSENT_CHOICE";

/// The answer-token prefix of a consent choice (`consent:<id>`).
pub const CHOICE_PREFIX: &str = "consent:";

/// One file or directory a choice would write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteTarget {
    pub path: String,
    #[serde(default)]
    pub what: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli: Option<String>,
    /// The path is the OPERATOR's own (outside every program-owned root), as the installer judged.
    #[serde(default)]
    pub operator_owned: bool,
}

/// One answer the gate offers, with everything it would write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanChoice {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub default: bool,
    #[serde(default)]
    pub writes: Vec<WriteTarget>,
}

/// A CLI the plan leaves out, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skipped {
    pub cli: String,
    #[serde(default)]
    pub why: String,
}

/// The parsed dry-run plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WritePlan {
    #[serde(default)]
    pub choices: Vec<PlanChoice>,
    #[serde(default)]
    pub skipped: Vec<Skipped>,
}

/// What a consent pause offers: the plan's choices, from which unit, or why there is none.
#[derive(Debug, Clone, PartialEq)]
pub struct ConsentOffer {
    /// The ord of the dry-run unit the plan came from.
    pub plan_ord: Option<u32>,
    pub plan: Option<WritePlan>,
    /// Set when no usable plan exists: the sentence the prompt and the event carry.
    pub missing: Option<String>,
}

impl WritePlan {
    /// The choice an answer token names (`consent:<id>`), or `None`.
    pub fn choice_for_token(&self, token: &str) -> Option<&PlanChoice> {
        let id = token.strip_prefix(CHOICE_PREFIX)?;
        self.choices.iter().find(|c| c.id == id)
    }

    /// The default choice: the one marked `default`, else the first.
    pub fn default_choice(&self) -> Option<&PlanChoice> {
        self.choices
            .iter()
            .find(|c| c.default)
            .or_else(|| self.choices.first())
    }
}

/// The plan in a dry-run unit's output: the LAST line that parses as an object with
/// `"dry_run": true` (a Tool unit's output is its stdout, then its stderr). A choice id must be a
/// short token (`[A-Za-z0-9_-]`, ≤ 32) and ids must be distinct, or the plan is refused: an id
/// rides an environment variable and an answer token.
pub fn parse_plan(output: &str) -> Option<WritePlan> {
    let plan = output.lines().rev().find_map(|line| {
        let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
        if v.get("dry_run").and_then(|d| d.as_bool()) != Some(true) {
            return None;
        }
        serde_json::from_value::<WritePlan>(v).ok()
    })?;
    let ok_id = |id: &str| {
        !id.is_empty()
            && id.len() <= 32
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    };
    let mut ids: Vec<&str> = plan.choices.iter().map(|c| c.id.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    if ids.len() != plan.choices.len() || !plan.choices.iter().all(|c| ok_id(&c.id)) {
        return None;
    }
    Some(plan)
}

/// Shorten a path under `home` to `~/…` for a human sentence (the event keeps the full path).
fn display_path(path: &str, home: Option<&str>) -> String {
    match home.filter(|h| !h.is_empty()) {
        Some(h) => match path.strip_prefix(h) {
            Some(rest) if rest.starts_with('/') || rest.starts_with('\\') => format!("~{rest}"),
            _ => path.to_string(),
        },
        None => path.to_string(),
    }
}

/// The prompt's sentence for the offer: every choice with what it writes, or why nothing is listed.
pub fn offer_sentence(offer: &ConsentOffer, home: Option<&str>) -> String {
    let Some(plan) = offer.plan.as_ref().filter(|p| !p.choices.is_empty()) else {
        return format!(
            " The write targets were not listed: {}.",
            offer
                .missing
                .as_deref()
                .unwrap_or("no dry-run plan ran before this gate")
        );
    };
    let mut s = String::from(" Choose one:");
    for c in &plan.choices {
        let writes = if c.writes.is_empty() {
            "writes nothing".to_string()
        } else {
            let items: Vec<String> = c
                .writes
                .iter()
                .map(|w| {
                    let p = display_path(&w.path, home);
                    let p = if w.operator_owned {
                        format!("your own {p}")
                    } else {
                        p
                    };
                    if w.what.is_empty() {
                        p
                    } else {
                        format!("{p} ({})", w.what)
                    }
                })
                .collect();
            format!("writes {}", items.join(", "))
        };
        let default = if c.default { " (default)" } else { "" };
        s.push_str(&format!(" \u{2022} {}{default} — {writes}.", c.label));
    }
    s.push_str(" \u{2022} Decline — writes nothing and cancels the run.");
    if !plan.skipped.is_empty() {
        let skipped: Vec<String> = plan
            .skipped
            .iter()
            .map(|k| {
                if k.why.is_empty() {
                    k.cli.clone()
                } else {
                    format!("{} ({})", k.cli, k.why)
                }
            })
            .collect();
        s.push_str(&format!(" Not installed for: {}.", skipped.join(", ")));
    }
    s
}

/// The event fields an offer adds to `awaitingHuman`: `choices` (the answer tokens, `reject` last),
/// `recommended` (the default's index), `choiceLabels`, `writeTargets` (per token, `reject` → `[]`),
/// `writePlanOrd`, `writeTargetsSkipped`; or only `writeTargetsMissing` when there is no plan.
pub fn event_fields(offer: &ConsentOffer) -> serde_json::Map<String, serde_json::Value> {
    use serde_json::json;
    let mut m = serde_json::Map::new();
    let Some(plan) = offer.plan.as_ref().filter(|p| !p.choices.is_empty()) else {
        m.insert(
            "writeTargetsMissing".into(),
            json!(offer
                .missing
                .as_deref()
                .unwrap_or("no dry-run plan ran before this gate")),
        );
        return m;
    };
    let mut choices: Vec<String> = Vec::new();
    let mut labels = serde_json::Map::new();
    let mut targets = serde_json::Map::new();
    for c in &plan.choices {
        let token = format!("{CHOICE_PREFIX}{}", c.id);
        labels.insert(token.clone(), json!(c.label));
        let writes: Vec<serde_json::Value> = c
            .writes
            .iter()
            .map(|w| {
                let mut o =
                    json!({ "path": w.path, "what": w.what, "operatorOwned": w.operator_owned });
                if let Some(cli) = &w.cli {
                    o["cli"] = json!(cli);
                }
                o
            })
            .collect();
        targets.insert(token.clone(), json!(writes));
        choices.push(token);
    }
    labels.insert("reject".into(), json!("Decline"));
    targets.insert("reject".into(), json!([]));
    let recommended = plan
        .default_choice()
        .and_then(|d| plan.choices.iter().position(|c| c.id == d.id))
        .unwrap_or(0);
    choices.push("reject".into());
    m.insert("choices".into(), json!(choices));
    m.insert("recommended".into(), json!(recommended));
    m.insert("choiceLabels".into(), serde_json::Value::Object(labels));
    m.insert("writeTargets".into(), serde_json::Value::Object(targets));
    if let Some(o) = offer.plan_ord {
        m.insert("writePlanOrd".into(), json!(o));
    }
    if !plan.skipped.is_empty() {
        m.insert("writeTargetsSkipped".into(), json!(plan.skipped));
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAN: &str = r#"{"dry_run":true,"choices":[{"id":"worker","label":"Install for workers","default":true,"writes":[{"path":"/h/.wicked/mcp-servers/giphy/current","what":"install root"}]},{"id":"operator","label":"Also install into my CLIs","writes":[{"path":"/h/.codex/config.toml","what":"codex MCP config","cli":"codex","operator_owned":true}]}],"skipped":[{"cli":"pi","why":"unsupported"}]}"#;

    #[test]
    fn the_plan_is_the_last_dry_run_line_and_its_ids_are_tokens() {
        let out = format!("building…\n{PLAN}\n[stderr] npm notice\n");
        let plan = parse_plan(&out).expect("a plan");
        assert_eq!(plan.choices.len(), 2);
        assert_eq!(plan.default_choice().unwrap().id, "worker");
        assert_eq!(
            plan.choice_for_token("consent:operator").unwrap().label,
            "Also install into my CLIs"
        );
        assert!(
            plan.choice_for_token("operator").is_none(),
            "needs the prefix"
        );
        assert!(plan.choice_for_token("consent:root").is_none());
        // Not a dry run, an unsafe id, or duplicate ids: no plan.
        assert!(parse_plan(r#"{"choices":[]}"#).is_none());
        assert!(parse_plan(&PLAN.replace("\"operator\"", "\"op erator\"")).is_none());
        assert!(parse_plan(&PLAN.replace("\"operator\"", "\"worker\"")).is_none());
    }

    #[test]
    fn the_offer_names_every_choice_and_its_writes() {
        let offer = ConsentOffer {
            plan_ord: Some(9),
            plan: parse_plan(PLAN),
            missing: None,
        };
        let s = offer_sentence(&offer, Some("/h"));
        assert!(s.contains("Install for workers (default) — writes ~/.wicked/mcp-servers/giphy/current (install root)."), "{s}");
        assert!(
            s.contains("writes your own ~/.codex/config.toml (codex MCP config)"),
            "{s}"
        );
        assert!(s.contains("Decline — writes nothing"), "{s}");
        assert!(s.contains("Not installed for: pi (unsupported)."), "{s}");
        let f = event_fields(&offer);
        assert_eq!(
            f["choices"],
            serde_json::json!(["consent:worker", "consent:operator", "reject"])
        );
        assert_eq!(f["recommended"], serde_json::json!(0));
        assert_eq!(
            f["writeTargets"]["consent:operator"][0]["operatorOwned"],
            serde_json::json!(true)
        );
        assert_eq!(f["writeTargets"]["reject"], serde_json::json!([]));
        assert_eq!(f["writePlanOrd"], serde_json::json!(9));
        assert!(!f.contains_key("writeTargetsMissing"));

        let none = ConsentOffer {
            plan_ord: None,
            plan: None,
            missing: None,
        };
        assert!(offer_sentence(&none, None).contains("The write targets were not listed"));
        let f = event_fields(&none);
        assert!(f.contains_key("writeTargetsMissing") && !f.contains_key("choices"));
    }
}
