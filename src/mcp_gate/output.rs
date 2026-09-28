//! The broker's OUTPUT decision for one brokered MCP call (DES-MCP-TOOLS-001 §6 step 8, slice S3).
//!
//! After an upstream answers, crew's broker scrubs the result of every secret it injected (D-2) and
//! then asks core, through the `evaluateMcpOutput` binding, whether the unit may SEE that result.
//! This is the same `select_any` + `decide` the call itself went through ([`super::evaluate`]), over
//! the same context plus `raw` (the scrubbed result text), so an operator's output policy is one
//! more steering rule: `applies_to: [mcp:jira]`, `trigger.contains` over the result, `effect: deny`.
//!
//! - A **deny** withholds the result. It is recorded like any MCP deny, as the ADVISORY claim class
//!   `mcp-deny:` (blocked and disclosed, the unit continues), naming the rules that denied. A deny
//!   that cannot be recorded is a [`McpCallError::GuardError`]: the broker refuses (D-3).
//! - Anything else is **allow**, and records nothing: the call's own allow is already in the unit's
//!   decisions log, and one call is never claimed twice as allowed (I4).
//! - The engine gates (D-1, D-5, first use) are not re-run: they decided before anything ran.

use serde::{Deserialize, Serialize};
use wicked_apps_core::{open_store_ro, Decision, GraphRead};

use super::{classify, context, deny, resolve, subject_tokens, McpCall, McpCallError, McpUnit};

/// The output verdict. `decision` is `allow` | `deny`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpOutputVerdict {
    pub decision: &'static str,
    pub subject: String,
    /// The rules that denied (deny), or every rule that fired (allow).
    pub rule_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remedy: Option<String>,
    /// The recorded deny's claim id; `None` for an allow (nothing new is recorded).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claim_id: Option<String>,
}

/// Judge the (already scrubbed) result `raw` of `call` for `unit`. Pure: returns the claim to
/// record for a deny, and records nothing.
pub(crate) fn evaluate_output(
    store: &dyn GraphRead,
    unit: &McpUnit,
    call: &McpCall,
    raw: &str,
    evaluated_at: i64,
) -> Result<(McpOutputVerdict, Option<wicked_apps_core::ConformanceClaim>), McpCallError> {
    if !super::valid_name(&call.server) || !super::valid_name(&call.tool) {
        return Err(McpCallError::BadRequest(format!(
            "server and tool must be non-empty names of [A-Za-z0-9_.-] (got {:?} / {:?})",
            call.server, call.tool
        )));
    }
    let class = classify(call.annotations.as_ref(), call.class_override);
    let subject = super::subject_of(&call.server, &call.tool);
    let mut ctx = context(unit, call, class);
    ctx["raw"] = serde_json::Value::String(raw.to_string());

    let mcp_tokens = subject_tokens(&call.server, &call.tool, unit.mode);
    let mut phases: Vec<&str> =
        crate::scope::phase_aliases(&unit.phase, Some(&unit.phase_id), Some(&unit.catalog));
    phases.extend(mcp_tokens.iter().map(String::as_str));
    let selected = wicked_governance::select_any(store, &unit.scope, &phases, &ctx)
        .map_err(|e| McpCallError::GuardError(format!("output policy select failed: {e}")))?;
    let claim = wicked_governance::decide_as(
        &selected,
        &unit.scope,
        &unit.phase,
        &ctx,
        evaluated_at,
        super::MCP_EVALUATOR,
    );
    if claim.decision != Decision::Deny {
        return Ok((
            McpOutputVerdict {
                decision: "allow",
                subject,
                rule_ids: claim.policy_ids,
                reason: None,
                remedy: None,
                claim_id: None,
            },
            None,
        ));
    }
    let denying: Vec<String> = claim
        .policy_ids
        .iter()
        .filter(|id| {
            selected
                .iter()
                .any(|p| &p.id == *id && p.effect == wicked_governance::Effect::Deny)
        })
        .cloned()
        .collect();
    let denying = if denying.is_empty() {
        claim.policy_ids.clone()
    } else {
        denying
    };
    let reason = format!(
        "mcp: the result of `{subject}` is withheld by policy {}",
        denying.join(", ")
    );
    let remedy = "the call ran but its result is not shown; recall the policy by its rule id";
    let (verdict, claim) = deny(unit, &subject, class, denying, reason, remedy, evaluated_at);
    Ok((
        McpOutputVerdict {
            decision: "deny",
            subject: verdict.subject,
            rule_ids: verdict.rule_ids,
            reason: verdict.reason,
            remedy: verdict.remedy,
            claim_id: Some(verdict.claim_id),
        },
        Some(claim),
    ))
}

/// Judge and (for a deny) RECORD the output decision of one brokered call. A deny that cannot be
/// recorded refuses (D-3).
pub fn evaluate_mcp_output(
    token: &str,
    call: &McpCall,
    raw: &str,
) -> Result<McpOutputVerdict, McpCallError> {
    let unit = resolve(token).ok_or(McpCallError::InvalidToken)?;
    let store = open_store_ro(Some(&unit.db_path))
        .map_err(|e| McpCallError::GuardError(format!("policy store open failed: {e}")))?;
    let (verdict, claim) = evaluate_output(&store, &unit, call, raw, crate::clock::eval_now())?;
    if let Some(claim) = claim {
        crate::gate_hook::append_annotated_claim_checked(
            &unit.decisions_path.to_string_lossy(),
            &unit.phase,
            &verdict.subject,
            &claim,
        )
        .map_err(|e| McpCallError::GuardError(format!("could not record the decision: {e}")))?;
    }
    Ok(verdict)
}

/// The JSON face of [`evaluate_mcp_output`] for the core-ts binding: `{token, call, raw}` in, the
/// verdict out; an error string is `<code>: <message>`.
pub fn evaluate_mcp_output_json(request_json: &str) -> Result<String, String> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Request {
        token: String,
        call: McpCall,
        raw: String,
    }
    let req: Request = serde_json::from_str(request_json)
        .map_err(|e| McpCallError::BadRequest(format!("request: {e}")).to_string())?;
    let verdict =
        evaluate_mcp_output(&req.token, &req.call, &req.raw).map_err(|e| e.to_string())?;
    serde_json::to_string(&verdict).map_err(|e| format!("guard_error: {e}"))
}

#[cfg(test)]
mod tests {
    use super::super::{
        default_carrier, default_kind, seed_mcp_defaults, McpAnnotations, McpMode, McpToken,
    };
    use super::*;
    use crate::workflow::PhaseRole;
    use crate::write_posture::WritePosture;
    use std::path::PathBuf;
    use wicked_apps_core::{open_store, ConformanceClaim};

    fn unit() -> McpUnit {
        McpUnit {
            run_id: "mcp-out".to_string(),
            attempt: 0,
            ord: 3,
            scope: "wicked-agent/mcp-out/shared".to_string(),
            phase: "unit-3".to_string(),
            phase_id: "build".to_string(),
            catalog: String::new(),
            role: PhaseRole::Creator,
            posture: WritePosture::Full,
            seat: "codex".to_string(),
            mode: McpMode::Balanced,
            decisions_path: PathBuf::from("/nonexistent"),
            db_path: String::new(),
        }
    }

    fn call() -> McpCall {
        McpCall {
            server: "jira".to_string(),
            tool: "get_issue".to_string(),
            args: serde_json::json!({"key": "ABC-1"}),
            annotations: Some(McpAnnotations {
                read_only_hint: Some(true),
                ..Default::default()
            }),
            class_override: None,
            registered: true,
            kind: default_kind(),
            carrier: default_carrier(),
        }
    }

    /// An output policy: withhold any jira result that carries an AWS access key id.
    fn output_rule() -> wicked_governance::ConformanceRule {
        let rule: wicked_governance::ConformanceRule = serde_json::from_value(serde_json::json!({
            "id": "OUT-NO-AWS-KEYS", "rule_type": "policy", "statement": "no AWS keys in results",
            "severity": "critical", "confidence": 1.0, "steering_type": "security",
            "applies_to": ["mcp:jira"], "effect": "deny",
            "trigger": {"contains": "AKIA[0-9A-Z]{16}"},
            "provenance": {"source": "ui", "ref": "test", "source_kinds": ["doc"]}
        }))
        .unwrap();
        rule.validate().unwrap();
        rule
    }

    fn store_with_rule() -> wicked_apps_core::SqliteStore {
        let mut store = open_store(Some(":memory:")).unwrap();
        seed_mcp_defaults(&mut store).unwrap();
        wicked_governance::register_rule(&mut store, &output_rule()).unwrap();
        store
    }

    /// PROVING TEST (S3 output decide): a result the output policy matches is denied, naming the
    /// rule, as an advisory `mcp-deny:` claim; a clean result is allowed and records nothing.
    #[test]
    fn an_output_policy_withholds_a_matching_result_and_passes_a_clean_one() {
        let store = store_with_rule();
        let u = unit();
        let (v, claim) = evaluate_output(
            &store,
            &u,
            &call(),
            r#"{"content":[{"type":"text","text":"key AKIAABCDEFGHIJKLMNOP"}]}"#,
            1,
        )
        .unwrap();
        assert_eq!(v.decision, "deny");
        assert_eq!(v.rule_ids, vec!["OUT-NO-AWS-KEYS".to_string()]);
        let claim = claim.expect("a deny carries its claim");
        assert!(crate::gate_hook::is_advisory_deny(&claim), "{claim:?}");
        assert_eq!(claim.obligations[1], "mcp:jira/get_issue");

        let (v, claim) = evaluate_output(
            &store,
            &u,
            &call(),
            r#"{"content":[{"type":"text","text":"all clear"}]}"#,
            1,
        )
        .unwrap();
        assert_eq!(v.decision, "allow");
        assert!(claim.is_none(), "an allowed output records nothing new");
        assert!(v.claim_id.is_none());
    }

    /// The policy matches the RESULT, not only the args: the same call with the key in its args
    /// but a clean result is allowed at output (the call-time gate judged the args).
    #[test]
    fn the_output_decision_reads_the_result_text() {
        let store = store_with_rule();
        let (v, _) = evaluate_output(&store, &unit(), &call(), "nothing here", 1).unwrap();
        assert_eq!(v.decision, "allow");
    }

    /// Recorded on deny, refused when the deny cannot be recorded (D-3), and the JSON face speaks
    /// camelCase with error codes.
    #[test]
    fn a_withheld_result_is_recorded_and_an_unrecordable_one_is_refused() {
        let tid = format!("{:?}", std::thread::current().id()).replace(['(', ')'], "");
        let base =
            std::env::temp_dir().join(format!("wicked-mcp-output-{}-{tid}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let db = base.join("policy.db");
        {
            let mut store = open_store(Some(&db.to_string_lossy())).unwrap();
            seed_mcp_defaults(&mut store).unwrap();
            wicked_governance::register_rule(&mut store, &output_rule()).unwrap();
        }
        let mut u = unit();
        u.db_path = db.to_string_lossy().into_owned();
        u.decisions_path = base.join("attempt-0").join("decisions.ndjson");
        let token = McpToken::mint();
        let binding = token.bind(u.clone());
        let req = serde_json::json!({
            "token": token.value(),
            "call": {"server": "jira", "tool": "get_issue", "registered": true,
                     "annotations": {"readOnlyHint": true}},
            "raw": "AKIAABCDEFGHIJKLMNOP",
        });
        let out: serde_json::Value =
            serde_json::from_str(&evaluate_mcp_output_json(&req.to_string()).unwrap()).unwrap();
        assert_eq!(out["decision"], "deny");
        assert_eq!(out["ruleIds"][0], "OUT-NO-AWS-KEYS");
        assert!(out["claimId"].as_str().unwrap().starts_with("mcp-deny:"));
        let log = std::fs::read_to_string(&u.decisions_path).unwrap();
        let claim: ConformanceClaim = serde_json::from_str(log.lines().last().unwrap()).unwrap();
        assert!(crate::gate_hook::is_advisory_deny(&claim));
        drop(binding);

        let blocked = base.join("blocked");
        std::fs::write(&blocked, b"not a dir").unwrap();
        u.decisions_path = blocked.join("decisions.ndjson");
        let _b = token.bind(u);
        let err = evaluate_mcp_output_json(&req.to_string()).unwrap_err();
        assert!(err.starts_with("guard_error:"), "{err}");

        let err = evaluate_mcp_output_json(
            r#"{"token":"wmt_nope","call":{"server":"jira","tool":"t","registered":true},"raw":""}"#,
        )
        .unwrap_err();
        assert!(err.starts_with("invalid_token"), "{err}");
        let err = evaluate_mcp_output_json(r#"{"token":"x","call":{}}"#).unwrap_err();
        assert!(err.starts_with("bad_request:"), "{err}");
        let _ = std::fs::remove_dir_all(&base);
    }
}
