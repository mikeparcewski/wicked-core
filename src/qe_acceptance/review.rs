//! (DES-QE-EVERY-SEAT, operator ruling 2026-10-12 "QE should be everyone") The run's QE unit's
//! ISOLATED REVIEWER, run by the engine as its own fresh worker session inside the QE unit's
//! attempt — so QE acceptance works on every seat, including ones with no sub-agent (pi, codex).
//!
//! 1. The executor turn (the unit's own seat) runs garden's `accept --phase evidence` and names the
//!    evidence dir on a `QE-EVIDENCE:` line ([`parse_marker`]); it writes no QE verdict.
//! 2. The engine BINDS that dir to this run and attempt ([`bind_evidence`]).
//! 3. A reviewer turn on a run seat ([`candidates`], stamped at dispatch) runs on a NEW input
//!    ([`reviewer_input`]): no prior outputs, no executor directive, only the evidence path, and a
//!    nonce its child alone receives as [`REVIEW_SESSION_ENV`].
//! 4. The engine BINDS the verdict the reviewer recorded ([`bind_verdict`]): the newest live
//!    verdict of that QE run carries the nonce, this crew run, and the reviewer's own token.
//!
//! The ledger is read the way crew's acceptance reader reads it — canonical JSON, never a
//! `DomainStore` (opening one writes). Every unreadable or mismatched input is `invalid`
//! (fail-closed); the fold turns the outcome into a denial ([`denial`]).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::domain::{AgentSession, QeReview, QeReviewAttempt, QeReviewerSeat, WorkUnit};
use crate::workflow::{DeltaSink, StepInput, StepOutput, StepRunner, StepStatus};

/// The line the executor turn names its evidence dir on.
pub(crate) const EVIDENCE_MARKER: &str = "QE-EVIDENCE:";
/// The variable ONLY the reviewer turn's child receives: the nonce it stamps on its verdict
/// (`review_session`), and garden's licence to review inline (this session was started for the
/// review alone). In `ENGINE_INTERNAL_ENV`, so no other child ever inherits it.
pub(crate) const REVIEW_SESSION_ENV: &str = "WICKED_QE_REVIEW_SESSION";

pub(crate) const OUTCOME_PASS: &str = "pass";
pub(crate) const OUTCOME_FAIL: &str = "fail";
pub(crate) const OUTCOME_INVALID: &str = "invalid";
pub(crate) const OUTCOME_UNAVAILABLE: &str = "unavailable";
/// (j4-gates) The reviewer recorded a non-PASS verdict and said it could not run the checks it was
/// asked to judge (a missing tool, binding or runtime on its seat): no product failure was found,
/// and none was ruled out. Not a FAIL: the gate keeps the operator's judgment, as for `unavailable`.
pub(crate) const OUTCOME_INCONCLUSIVE: &str = "inconclusive";

/// (j4-gates) The line the reviewer names what could not run on, before its VERDICT line.
pub(crate) const INCONCLUSIVE_MARKER: &str = "QE-INCONCLUSIVE:";
/// The longest "what could not run" the record keeps.
const INCONCLUSIVE_WHAT_MAX: usize = 300;

pub(crate) const BASIS_DISTINCT: &str = "distinct";
pub(crate) const BASIS_EXECUTOR: &str = "executor_seat";
pub(crate) const BASIS_REDUCED: &str = "reduced_assurance_creator";

/// The fold's denial source for a reviewer `fail` / `invalid` (class `verdict_not_pass`).
pub(crate) const DENIAL_SOURCE_QE_REVIEW: &str = "qe_review";
/// The fold's denial source for `unavailable` (class `judge_unavailable`: the operator's approve
/// is the judgment, labelled).
pub const DENIAL_SOURCE_QE_REVIEWER_UNAVAILABLE: &str = "qe_reviewer_unavailable";
/// (j4-gates) The fold's denial source for `inconclusive` (class `judge_unavailable`): the reviewer
/// could not run its checks. It never outranks another layer, and it does not refuse the operator's
/// judgment over a failed floor (QE acceptance stays required at delivery).
pub const DENIAL_SOURCE_QE_INCONCLUSIVE: &str = "qe_inconclusive";

/// (j4-gates) Whether a denial is the QE reviewer's "could not judge" record: no seat could review
/// (`qe_reviewer_unavailable`) or the reviewer could not run its checks (`qe_inconclusive`). Neither
/// is a verdict on the work.
pub(crate) fn could_not_judge(d: &crate::domain::UnitDenial) -> bool {
    d.source == DENIAL_SOURCE_QE_REVIEWER_UNAVAILABLE || d.source == DENIAL_SOURCE_QE_INCONCLUSIVE
}

/// (j4-gates) Every denial source the isolated QE reviewer's record produces.
pub(crate) fn is_qe_denial(d: &crate::domain::UnitDenial) -> bool {
    d.source == DENIAL_SOURCE_QE_REVIEW || could_not_judge(d)
}

/// Whether this unit's attempt runs the engine's reviewer: the run's QE unit carrying its ledger
/// root, with a `required` decision — and never the reviewer turn itself.
pub(crate) fn applies(unit: &WorkUnit) -> bool {
    super::carries_ledger_root(unit)
        && unit.qe_review_session.is_none()
        && unit.qe_acceptance.as_ref().is_some_and(|q| q.required())
}

/// (§3.7) The evidence dir a same-tree retry RESUMES: the unit's last bound QE run, when this
/// dispatch starts on the tree that run was taken on. `None` otherwise (a fresh QE run).
pub(crate) fn resume_dir(unit: &WorkUnit) -> Option<&str> {
    let review = unit.qe_review.as_ref()?;
    let tree = unit.worktree_baseline.as_ref().map(|b| b.tree.as_str())?;
    review.qe_run_id.as_ref()?;
    (review.tree.as_deref() == Some(tree))
        .then_some(review.evidence_dir.as_deref())
        .flatten()
}

/// The executor turn's directive (rides the unit prompt; short — the PTY line budget).
pub(crate) fn evidence_directive(skill: &str, resume: Option<&str>) -> String {
    let resume = resume.map(|d| format!(" --resume {d}")).unwrap_or_default();
    format!(
        " QE ACCEPTANCE IS REQUIRED: run \"{skill}\" accept --phase evidence{resume} (writer and \
         executor only; no review, no QE verdict) on a scenario written from this run's \
         acceptance list. Its ledger is ${}, outside the worktree (never write .wicked-qe into \
         the tree). Put the line `{EVIDENCE_MARKER} <evidence dir>` before your VERDICT line: \
         the engine then runs the isolated reviewer in a fresh session, and delivery is refused \
         without its PASS.",
        super::LEDGER_DIR_ENV
    )
}

/// The reviewer turn's whole task: the evidence path, nothing else — and (core#928) the absolute
/// scratch dir it may stage a helper file in ([`crate::write_posture::unit_scratch_tmp`]): spelled
/// out, because the shell fence resolves no `$TMPDIR` in a write target.
pub(crate) fn reviewer_brief(dir: &str, scratch: Option<&Path>) -> String {
    let stage = scratch
        .map(|t| {
            format!(
                " Stage any helper file in {} (spell that absolute path; /tmp and the worktree \
                 are refused).",
                t.display()
            )
        })
        .unwrap_or_default();
    format!(
        "Run its accept action as the ISOLATED REVIEWER: accept --phase review --evidence {dir}. \
         wicked-core started this session for this review alone, so you hold no plan writing or \
         test execution: open the acceptance reviewer here, judge only the scenario, plan and \
         evidence files in that directory, record the verdict in the QE ledger (stamped with \
         ${REVIEW_SESSION_ENV} and WICKED_RUN_ID), and change nothing else. If a check \
         could not run here, put `{INCONCLUSIVE_MARKER} <what, why>` before VERDICT.{stage}"
    )
}

/// The reviewer unit: the QE unit's role, read-only tree posture and ledger root (so the boundary
/// and its writable ledger hold), the QE skill as its method (admission and invocation judge what
/// it really runs), the brief as its whole task — and nothing of the executor's: no directive,
/// base skill, validator, deliverables, team or floor report.
pub(crate) fn reviewer_unit(
    unit: &WorkUnit,
    seat: &QeReviewerSeat,
    dir: &str,
    nonce: Option<&str>,
) -> WorkUnit {
    let mut u = unit.clone();
    u.description = reviewer_brief(dir, crate::write_posture::unit_scratch_tmp(unit).as_deref());
    if u.assigned_cli.as_deref() != Some(seat.seat.as_str()) {
        u.assigned_cli = Some(seat.seat.clone());
        u.assigned_invocation = seat.invocation.clone();
    }
    u.skill_ref = Some(crate::execute_wrapped::QE_SKILL.to_string());
    u.allowed_skills.clear();
    u.base_skill_ref = None;
    u.qe_acceptance = None;
    u.validator = None;
    u.required_deliverables.clear();
    u.team = None;
    u.member_step = None;
    u.repo_checks = None;
    u.instructions = None;
    u.amendments.clear();
    u.rework_amendment = None;
    u.structured_description = false;
    u.qe_reviewer_seats.clear();
    u.qe_review = None;
    u.qe_review_session = nonce.map(str::to_string);
    u
}

/// The reviewer turn's input: a NEW `StepInput` — no prior outputs (the executor's reply, the
/// team boundary, nothing), the reviewer unit, the run's own identity and boundary.
pub(crate) fn reviewer_input(
    input: &StepInput,
    seat: &QeReviewerSeat,
    dir: &str,
    nonce: &str,
) -> StepInput {
    StepInput {
        unit: reviewer_unit(&input.unit, seat, dir, Some(nonce)),
        prior_outputs: Vec::new(),
        ..input.clone()
    }
}

/// (§3.2) The seats the reviewer may run on, from the run's OWN seats only, in order. Absolute
/// exclusions (never relaxed): off-roster, benched, launch-excluded, and a seat whose skills do
/// not admit the reviewer. Authors (`work_authors` ∪ the unit's own `authored_by`, compared by
/// seat INSTANCE) are relaxed only in a reduced-assurance run, labelled.
pub(crate) fn candidates(
    session: &AgentSession,
    units: &[WorkUnit],
    unit: &WorkUnit,
) -> Vec<QeReviewerSeat> {
    use crate::team::runner::seat_key;
    let mut authors = crate::pipeline::work_authors(units, unit.ord);
    for a in &unit.authored_by {
        if !authors.contains(a) {
            authors.push(a.clone());
        }
    }
    let excluded = |k: &str| {
        session
            .exclude_seats
            .iter()
            .any(|e| e == k || e == seat_key(k))
    };
    // (codex r1 on the code) The launch roster's template for a seat, read off a unit the roster
    // seated there — what the reviewer turn will actually run, and what admission judges.
    let invocation = |k: &str| -> Option<String> {
        if unit.assigned_cli.as_deref() == Some(k) {
            return unit.assigned_invocation.clone();
        }
        units
            .iter()
            .filter(|u| u.assigned_cli.as_deref() == Some(k))
            .find_map(|u| u.assigned_invocation.clone())
    };
    let shape = reviewer_unit(
        unit,
        &QeReviewerSeat {
            seat: String::new(),
            basis: String::new(),
            same_cli: false,
            invocation: None,
        },
        "",
        None,
    );
    let pool: Vec<&String> = session
        .clis
        .iter()
        .filter(|k| !session.benched_seats.iter().any(|b| &b.cli == *k))
        .filter(|k| !excluded(k))
        .filter(|k| crate::distribute::seat_admits_unit(&shape, k, invocation(k)))
        .collect();
    let is_author = |k: &str| authors.iter().any(|a| a == k);
    let same_cli = |k: &str| authors.iter().any(|a| a != k && seat_key(a) == seat_key(k));
    let executor = unit.assigned_cli.as_deref();
    let seat = |k: &str, basis: &str| QeReviewerSeat {
        seat: k.to_string(),
        basis: basis.to_string(),
        same_cli: same_cli(k),
        invocation: invocation(k),
    };
    let mut out: Vec<QeReviewerSeat> = pool
        .iter()
        .filter(|k| !is_author(k) && Some(k.as_str()) != executor)
        .map(|k| seat(k, BASIS_DISTINCT))
        .collect();
    if let Some(e) = executor.filter(|e| !is_author(e) && pool.iter().any(|k| k == e)) {
        out.push(seat(e, BASIS_EXECUTOR));
    }
    if session.assurance.reduced() {
        for k in pool.iter().filter(|k| is_author(k)) {
            if !out.iter().any(|c| &c.seat == *k) {
                out.push(seat(k, BASIS_REDUCED));
            }
        }
    }
    out
}

/// The LAST `QE-EVIDENCE:` line's path (decoration — bullets, emphasis, code ticks — tolerated).
pub(crate) fn parse_marker(output: &str) -> Option<String> {
    output.lines().rev().find_map(|l| {
        let at = l.find(EVIDENCE_MARKER)?;
        let rest = l[at + EVIDENCE_MARKER.len()..]
            .trim()
            .trim_matches(|c: char| c == '`' || c == '*' || c == '"' || c == '\'')
            .trim();
        (!rest.is_empty()).then(|| rest.to_string())
    })
}

/// (j4-gates) What the reviewer said it could NOT run, when its reply says its verdict is
/// inconclusive: the LAST [`INCONCLUSIVE_MARKER`] line's text (the engine's own contract, in the
/// reviewer brief), else garden's convention (the acceptance reviewer's `reason: inconclusive — …`):
/// the last line whose first word, past decoration, a `reason:` label, a `FAIL` token, `[tag]`s and
/// separators, is `inconclusive`. `None` for every other reply — a reply that only MENTIONS the word
/// mid-sentence never counts. Capped at [`INCONCLUSIVE_WHAT_MAX`] chars.
pub(crate) fn parse_inconclusive(output: &str) -> Option<String> {
    let cap = |t: &str| -> String {
        let t = t.trim();
        if t.chars().count() <= INCONCLUSIVE_WHAT_MAX {
            t.to_string()
        } else {
            format!(
                "{}…",
                t.chars().take(INCONCLUSIVE_WHAT_MAX).collect::<String>()
            )
        }
    };
    let decoration = |c: char| c.is_whitespace() || "*-_>#`\"'".contains(c);
    if let Some(what) = output.lines().rev().find_map(|l| {
        let at = l.find(INCONCLUSIVE_MARKER)?;
        let rest = l[at + INCONCLUSIVE_MARKER.len()..].trim_matches(decoration);
        (!rest.is_empty()).then(|| cap(rest))
    }) {
        return Some(what);
    }
    output.lines().rev().find_map(|l| {
        let mut t = l.trim_start_matches(decoration);
        loop {
            let before = t;
            for label in ["reason:", "verdict:", "fail"] {
                if t.get(..label.len())
                    .is_some_and(|h| h.eq_ignore_ascii_case(label))
                {
                    let rest = &t[label.len()..];
                    // A whole word only: `FAILED` / `failure` are not the token.
                    if rest.chars().next().is_none_or(|c| !c.is_alphanumeric()) {
                        t = rest;
                    }
                }
            }
            if let Some(rest) = t.strip_prefix('[') {
                if let Some(end) = rest.find(']') {
                    t = &rest[end + 1..];
                }
            }
            t = t.trim_start_matches(|c: char| {
                decoration(c) || matches!(c, ':' | '—' | '–' | '-' | '=' | '(' | ')')
            });
            if t == before {
                break;
            }
        }
        let word = "inconclusive";
        (t.get(..word.len())
            .is_some_and(|h| h.eq_ignore_ascii_case(word))
            && t[word.len()..]
                .chars()
                .next()
                .is_none_or(|c| !c.is_alphanumeric()))
        .then(|| cap(t))
    })
}

/// Read one canonical ledger record (`<root>/<table>/<id>.json`).
/// A record without a non-empty string `id` is refused, as crew's reader refuses it.
fn read_record(path: &Path) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("{} could not be read: {e}", path.display()))?;
    match serde_json::from_str::<serde_json::Value>(&raw) {
        Ok(serde_json::Value::Object(m)) if field(&m, "id").is_some() => Ok(m),
        Ok(serde_json::Value::Object(_)) => Err(format!("{} has no record id", path.display())),
        Ok(_) => Err(format!("{} is not a record object", path.display())),
        Err(e) => Err(format!("{} is not valid JSON: {e}", path.display())),
    }
}

fn field<'a>(rec: &'a serde_json::Map<String, serde_json::Value>, k: &str) -> Option<&'a str> {
    rec.get(k)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
}

/// Soft-deleted, read the way crew's reader reads it: any truthy `deleted` (JS truthiness).
fn deleted(rec: &serde_json::Map<String, serde_json::Value>) -> bool {
    use serde_json::Value as V;
    match rec.get("deleted") {
        None | Some(V::Null) => false,
        Some(V::Bool(b)) => *b,
        Some(V::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(V::String(s)) => !s.is_empty(),
        Some(V::Array(_)) | Some(V::Object(_)) => true,
    }
}

/// Unix millis of an ISO-8601 UTC timestamp as `Date.prototype.toISOString` writes it
/// (`YYYY-MM-DDTHH:MM:SS[.fff]Z`, year 1970–9999); `None` for anything else — an impossible date
/// included.
pub(crate) fn iso_millis(s: &str) -> Option<i64> {
    let digits = |p: &str, n: usize| -> Option<i64> {
        (p.len() == n && p.bytes().all(|b| b.is_ascii_digit()))
            .then(|| p.parse().ok())
            .flatten()
    };
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let d: Vec<&str> = date.split('-').collect();
    let [y, m, day] = d.as_slice() else {
        return None;
    };
    let (y, m, day) = (digits(y, 4)?, digits(m, 2)?, digits(day, 2)?);
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if !(1970..=9999).contains(&y) || !(1..=12).contains(&m) {
        return None;
    }
    if day < 1 || day > month_days[(m - 1) as usize] {
        return None;
    }
    let (hms, frac) = time.split_once('.').unwrap_or((time, "000"));
    let t: Vec<&str> = hms.split(':').collect();
    let [hh, mm, ss] = t.as_slice() else {
        return None;
    };
    let (hh, mm, ss) = (digits(hh, 2)?, digits(mm, 2)?, digits(ss, 2)?);
    if hh > 23 || mm > 59 || ss > 59 || frac.is_empty() || frac.len() > 9 {
        return None;
    }
    if !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let ms: i64 = format!("{frac:0<3}")[..3].parse().ok()?;
    // Days from the civil date (H. Hinnant's algorithm); every input is range-checked above.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 24 + hh) * 60 + mm) * 60_000 + ss * 1000 + ms)
}

/// (§3.1 step 2) Bind the executor's marker to THIS run and attempt: exactly
/// `<ledger_root>/evidence/<uuid>` (canonically — no link anywhere on the way), holding the plan and
/// the scenario copy, whose run record is live, stamped with this crew run, points at this dir,
/// and was created in this attempt (`not_before`, unix millis) — or is the run this attempt was
/// told to RESUME. Returns the QE run id and the dir.
pub(crate) fn bind_evidence(
    ledger_root: &Path,
    marker: &str,
    crew_run: &str,
    not_before: i64,
    resume: Option<&str>,
) -> Result<(String, PathBuf), String> {
    let dir = Path::new(marker);
    if !dir.is_absolute() {
        return Err(format!(
            "the evidence dir `{marker}` is not an absolute path"
        ));
    }
    let id = dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("the evidence dir `{marker}` has no run id"))?
        .to_string();
    uuid::Uuid::parse_str(&id)
        .map_err(|_| format!("the evidence dir's run id `{id}` is not a QE run id (a UUID)"))?;
    let meta = std::fs::symlink_metadata(dir)
        .map_err(|e| format!("the evidence dir `{marker}` could not be read: {e}"))?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(format!(
            "the evidence dir `{marker}` is not a plain directory"
        ));
    }
    let root = std::fs::canonicalize(ledger_root)
        .map_err(|e| format!("the run's QE ledger could not be resolved: {e}"))?;
    let real = std::fs::canonicalize(dir)
        .map_err(|e| format!("the evidence dir `{marker}` could not be resolved: {e}"))?;
    if real != root.join("evidence").join(&id) {
        return Err(format!(
            "the evidence dir `{marker}` is not this run's ledger evidence dir ({})",
            root.join("evidence").join(&id).display()
        ));
    }
    for f in ["test-plan.md", "scenario.md"] {
        if !real.join(f).is_file() {
            return Err(format!("the evidence dir holds no `{f}`"));
        }
    }
    let run = read_record(&root.join("runs").join(format!("{id}.json")))
        .map_err(|e| format!("the QE run record: {e}"))?;
    if deleted(&run) {
        return Err(format!("QE run {id} is deleted"));
    }
    if field(&run, "crew_run_id") != Some(crew_run) {
        return Err(format!("QE run {id} is not stamped with this run's id"));
    }
    let points_here = field(&run, "evidence_path")
        .and_then(|p| std::fs::canonicalize(p).ok())
        .is_some_and(|p| p == real);
    if !points_here {
        return Err(format!(
            "QE run {id}'s evidence_path is not this evidence dir"
        ));
    }
    let resumed = resume
        .and_then(|r| std::fs::canonicalize(r).ok())
        .is_some_and(|r| r == real);
    if !resumed {
        let at = field(&run, "created_at").and_then(iso_millis);
        if !at.is_some_and(|t| t >= not_before) {
            return Err(format!(
                "QE run {id} was not created in this attempt (an earlier run's evidence)"
            ));
        }
    }
    Ok((id, real))
}

/// (§3.1 step 4) Bind the reviewer's verdict: the NEWEST live verdict of QE run `id` (the one crew
/// reads as that run's judgment) must carry this reviewer turn's nonce, this crew run, and the
/// token the reviewer's own reply ended on. Returns that verdict.
pub(crate) fn bind_verdict(
    ledger_root: &Path,
    id: &str,
    crew_run: &str,
    nonce: &str,
    reply_token: &str,
) -> Result<String, String> {
    let dir = ledger_root.join("verdicts");
    let names = std::fs::read_dir(&dir).map_err(|e| {
        format!("the QE ledger's verdicts could not be read ({e}): the reviewer recorded none")
    })?;
    let mut newest: Option<(String, serde_json::Map<String, serde_json::Value>)> = None;
    for entry in names {
        let entry =
            entry.map_err(|e| format!("the QE ledger's verdicts could not be read: {e}"))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".json") {
            continue;
        }
        let rec = read_record(&entry.path())?;
        if deleted(&rec) || field(&rec, "run_id") != Some(id) {
            continue;
        }
        let at = field(&rec, "created_at").unwrap_or_default().to_string();
        if iso_millis(&at).is_none() {
            return Err(format!(
                "verdict {} on QE run {id} has no valid created_at",
                field(&rec, "id").unwrap_or("?")
            ));
        }
        if newest.as_ref().is_none_or(|(n, _)| at > *n) {
            newest = Some((at, rec));
        }
    }
    let Some((_, rec)) = newest else {
        return Err(format!("the reviewer recorded no verdict on QE run {id}"));
    };
    if field(&rec, "review_session") != Some(nonce) {
        return Err(format!(
            "the newest verdict on QE run {id} was not recorded by the isolated reviewer session"
        ));
    }
    if field(&rec, "crew_run_id") != Some(crew_run) {
        return Err(format!(
            "the newest verdict on QE run {id} is not stamped with this run's id"
        ));
    }
    let verdict = field(&rec, "verdict").unwrap_or_default().to_uppercase();
    if verdict != reply_token {
        return Err(format!(
            "the recorded verdict ({verdict}) is not the reviewer's own ({reply_token})"
        ));
    }
    Ok(verdict)
}

/// A reviewer turn that failed AS A SEAT — a FAILED launch or turn whose output the judge
/// rotation's classifiers name (sign-in, quota, not installed, could not start). The only ending
/// that moves on to the next candidate: a timeout, a cancel, an unclassified failure and a turn
/// that completed but broke its contract are `invalid` (codex r1 on the code).
fn seat_failure(out: &StepOutput) -> Option<&'static str> {
    use wicked_council::types::SeatFailureReason;
    if out.status != StepStatus::Failed {
        return None;
    }
    let nonzero = crate::execute_wrapped::wrapped_exit_code(&out.output).is_some_and(|c| c != 0);
    SeatFailureReason::classify_refusal(&out.output, nonzero)
        .or_else(|| {
            crate::execute_wrapped::spawn_failure_detail(&out.output)
                .and_then(SeatFailureReason::classify_spawn_detail)
        })
        .map(|r| r.as_str())
}

/// (§3.1) Run the reviewer for an executor turn that folded Ok, or `None` when this unit's attempt
/// runs none. `not_before` is the attempt's dispatch time (unix millis). Each reviewer turn is
/// recorded with its nonce and governance flags: its own decisions log is folded as its own
/// (`gate_hook::qe_review_decisions_path`), so the unit's output envelope stays the executor's.
pub(crate) fn run(
    runner: &Arc<dyn StepRunner>,
    input: &StepInput,
    executor: &StepOutput,
    not_before: i64,
    emit: &DeltaSink,
) -> Option<QeReview> {
    if !applies(&input.unit) || executor.status != StepStatus::Ok {
        return None;
    }
    let tree = input
        .unit
        .worktree_baseline
        .as_ref()
        .map(|b| b.tree.clone());
    // (§3.7, codex r1 on the code) A same-tree retry keeps the QE run it was told to resume even
    // when this attempt fails to bind, so the next retry still resumes it (its newest verdict
    // governs it in crew's reader) instead of minting a run beside a FAIL that never retires.
    let lineage = resume_dir(&input.unit).and(input.unit.qe_review.as_ref());
    let mut review = QeReview {
        outcome: OUTCOME_INVALID.to_string(),
        reason: String::new(),
        evidence_dir: lineage.and_then(|r| r.evidence_dir.clone()),
        qe_run_id: lineage.and_then(|r| r.qe_run_id.clone()),
        tree,
        verdict: None,
        attempts: Vec::new(),
    };
    let ledger = input.unit.qe_ledger_root.as_deref().map(Path::new)?;
    let Some(marker) = parse_marker(&executor.output) else {
        review.reason = format!(
            "the QE executor turn named no evidence (no `{EVIDENCE_MARKER}` line; wicked-garden \
             may be older than the engine-run reviewer)"
        );
        return Some(review);
    };
    let (id, dir) = match bind_evidence(
        ledger,
        &marker,
        &input.run_id,
        not_before,
        resume_dir(&input.unit),
    ) {
        Ok(b) => b,
        Err(why) => {
            review.reason = format!("the QE evidence could not be bound: {why}");
            return Some(review);
        }
    };
    let dir_s = dir.to_string_lossy().into_owned();
    review.evidence_dir = Some(dir_s.clone());
    review.qe_run_id = Some(id.clone());
    let seats = &input.unit.qe_reviewer_seats;
    if seats.is_empty() {
        review.outcome = OUTCOME_UNAVAILABLE.to_string();
        review.reason = "no run seat can review the QE evidence in isolation: every seat is \
                         benched, excluded, an author of the work, or does not admit the QE skill"
            .to_string();
        return Some(review);
    }
    for c in seats {
        let nonce = uuid::Uuid::new_v4().to_string();
        emit(&format!(
            "QE reviewer: seat {} ({}, fresh session) reviews {dir_s}",
            c.seat, c.basis
        ));
        let out = runner.run_unit_streaming(&reviewer_input(input, c, &dir_s, &nonce), emit);
        let attempt = |status: String| QeReviewAttempt {
            seat: c.seat.clone(),
            basis: c.basis.clone(),
            same_cli: c.same_cli,
            status,
            session: nonce.clone(),
            governed: out.governed,
            tool_activity: !out.tools.is_empty(),
            attempt: input.attempt,
        };
        if let Some(why) = seat_failure(&out) {
            review.attempts.push(attempt(format!("seat_failure:{why}")));
            continue;
        }
        if out.status != StepStatus::Ok {
            review.attempts.push(attempt(OUTCOME_INVALID.to_string()));
            review.reason = format!(
                "the reviewer turn on `{}` ended {:?} without a seat failure the engine can \
                 classify",
                c.seat, out.status
            );
            return Some(review);
        }
        let Some(token) = crate::validator::parse_evaluator_verdict(&out.output).token else {
            review.attempts.push(attempt(OUTCOME_INVALID.to_string()));
            review.reason = format!("the reviewer on `{}` ended with no VERDICT line", c.seat);
            return Some(review);
        };
        match bind_verdict(ledger, &id, &input.run_id, &nonce, &token) {
            Ok(v) => {
                review.attempts.push(attempt("verdict".to_string()));
                // (j4-gates) A non-PASS the reviewer itself calls inconclusive — it could not run
                // the checks it judges on this seat — is no FAIL of the work: it names what could
                // not run, and the gate keeps the operator's judgment. So is a ledger verdict that
                // means "not evaluated" (INCONCLUSIVE / N-A / SKIP).
                let not_evaluated = matches!(v.as_str(), "INCONCLUSIVE" | "N-A" | "SKIP");
                let inconclusive = (v != "PASS")
                    .then(|| parse_inconclusive(&out.output))
                    .flatten()
                    .or_else(|| {
                        not_evaluated.then(|| format!("the reviewer recorded {v} (not evaluated)"))
                    });
                review.outcome = match (&inconclusive, v == "PASS") {
                    (_, true) => OUTCOME_PASS,
                    (Some(_), false) => OUTCOME_INCONCLUSIVE,
                    (None, false) => OUTCOME_FAIL,
                }
                .to_string();
                review.reason = match &inconclusive {
                    Some(what) => format!(
                        "the isolated QE reviewer on `{}` ({}) recorded {v} on QE run {id} as \
                         INCONCLUSIVE — it could not run its checks: {what}",
                        c.seat, c.basis
                    ),
                    None => format!(
                        "the isolated QE reviewer on `{}` ({}) recorded {v} on QE run {id}",
                        c.seat, c.basis
                    ),
                };
                review.verdict = Some(v);
            }
            Err(why) => {
                review.attempts.push(attempt(OUTCOME_INVALID.to_string()));
                review.reason = format!("the reviewer's verdict could not be bound: {why}");
            }
        }
        emit(&format!("QE reviewer: {}", review.reason));
        return Some(review);
    }
    review.outcome = OUTCOME_UNAVAILABLE.to_string();
    review.reason = format!(
        "every reviewer seat failed as a seat ({})",
        review
            .attempts
            .iter()
            .map(|a| format!("{}: {}", a.seat, a.status))
            .collect::<Vec<_>>()
            .join(", ")
    );
    Some(review)
}

/// (§3.6) A floor re-run's record: no reviewer turn runs (the seat does not run again). The last
/// record is carried only when this dispatch starts on the tree it was taken on — after a floor
/// fix too, when the fix made no edit (core#1153); otherwise it is `invalid`.
pub(crate) fn on_floor_rerun(unit: &WorkUnit, fix: bool) -> Option<QeReview> {
    if !applies(unit) {
        return None;
    }
    let tree = unit.worktree_baseline.as_ref().map(|b| b.tree.clone());
    let invalid = |reason: &str| QeReview {
        outcome: OUTCOME_INVALID.to_string(),
        reason: reason.to_string(),
        evidence_dir: unit.qe_review.as_ref().and_then(|r| r.evidence_dir.clone()),
        qe_run_id: unit.qe_review.as_ref().and_then(|r| r.qe_run_id.clone()),
        tree: tree.clone(),
        verdict: None,
        attempts: Vec::new(),
    };
    match unit.qe_review.as_ref() {
        // (core#1153) A floor fix that made no edit (`NO CHANGE NEEDED`) left the tree the review
        // judged: its record stands, as for a plain re-run. `unit` is the fixed input, re-baselined
        // on the tree the fix left.
        Some(r) if tree.is_some() && r.tree == tree => Some(r.clone()),
        _ if fix => Some(invalid(
            "the floor fix changed the tree after the QE evidence was taken; approve to re-run QE \
             on the fixed tree",
        )),
        _ => Some(invalid(
            "the floor re-run is not on the tree the last QE review judged; approve to re-run QE",
        )),
    }
}

/// The fold's denial for a reviewer record: `fail` / `invalid` → [`DENIAL_SOURCE_QE_REVIEW`] (a
/// substantive veto), `unavailable` → [`DENIAL_SOURCE_QE_REVIEWER_UNAVAILABLE`] and (j4-gates)
/// `inconclusive` → [`DENIAL_SOURCE_QE_INCONCLUSIVE`] (the operator's judgment gate). `None` for `pass`.
pub(crate) fn denial(review: &QeReview) -> Option<crate::domain::UnitDenial> {
    let source = match review.outcome.as_str() {
        OUTCOME_PASS => return None,
        OUTCOME_UNAVAILABLE => DENIAL_SOURCE_QE_REVIEWER_UNAVAILABLE,
        OUTCOME_INCONCLUSIVE => DENIAL_SOURCE_QE_INCONCLUSIVE,
        _ => DENIAL_SOURCE_QE_REVIEW,
    };
    Some(crate::domain::UnitDenial::new(
        source,
        format!("QE acceptance: {}", review.reason),
    ))
}

/// (§3.4.1) The QE decision an operator's approve at a judge-unavailable gate records, when the
/// unit's last reviewer record says no isolated reviewer could run: `skipped` by the operator,
/// with "judged by operator" and the evidence in its reason. `None` for any other record.
pub(crate) fn operator_judged(unit: &WorkUnit, by: &str) -> Option<crate::assurance::QeAcceptance> {
    use crate::assurance as a;
    let review = unit
        .qe_review
        .as_ref()
        .filter(|r| r.outcome == OUTCOME_UNAVAILABLE || r.outcome == OUTCOME_INCONCLUSIVE)?;
    let what = if review.outcome == OUTCOME_INCONCLUSIVE {
        "the QE reviewer could not run its checks"
    } else {
        "no isolated QE reviewer could run"
    };
    Some(a::QeAcceptance {
        status: a::QE_SKIPPED.to_string(),
        basis: a::QE_BASIS_OPERATOR.to_string(),
        score: None,
        threshold: crate::review_scale::THRESHOLDS.qe_waiver_max_score,
        reason: format!(
            "judged by operator ({by}): {what} ({}); evidence at {}",
            review.reason,
            review.evidence_dir.as_deref().unwrap_or("(none bound)")
        ),
        reasons: Vec::new(),
        ord: Some(unit.ord),
        tree: review.tree.clone(),
    })
}

#[cfg(test)]
mod tests;
