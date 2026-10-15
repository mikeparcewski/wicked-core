//! Operator questions reuse the elicitation wire, but have no ACP turn or epoch to deliver to.
use std::collections::HashMap;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{event::CoreEvent, mcp_gate::McpUnit, Core};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AskRequest {
    pub question: String,
    pub choices: Option<Vec<String>>,
    pub recommended: Option<u32>,
}

impl AskRequest {
    pub(crate) fn validate(mut self) -> Result<Self, AskError> {
        if self.question.is_empty() || self.question.len() > 8192 {
            return Err(AskError::BadRequest(
                "question must contain 1..=8192 bytes".into(),
            ));
        }
        if let Some(choices) = &mut self.choices {
            if !(2..=100).contains(&choices.len()) {
                return Err(AskError::BadRequest(
                    "choices must contain 2..=100 entries".into(),
                ));
            }
            if choices.iter().any(|s| s.is_empty() || s.len() > 512) {
                return Err(AskError::BadRequest(
                    "choices entries must contain 1..=512 bytes".into(),
                ));
            }
            // The caller's index selects a choice in the original list, before deduplication.
            let recommended_choice = self
                .recommended
                .map(|i| {
                    choices.get(i as usize).cloned().ok_or_else(|| {
                        AskError::BadRequest("recommended out of range for choices".into())
                    })
                })
                .transpose()?;
            let mut seen = std::collections::HashSet::new();
            choices.retain(|s| seen.insert(s.clone()));
            if choices.len() < 2 {
                return Err(AskError::BadRequest(
                    "choices must contain at least 2 distinct entries".into(),
                ));
            }
            self.recommended = recommended_choice
                .and_then(|choice| choices.iter().position(|s| *s == choice))
                .map(|i| i as u32);
        } else if self.recommended.is_some() {
            return Err(AskError::BadRequest("recommended requires choices".into()));
        }
        Ok(self)
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AskTicket {
    pub run_id: String,
    pub elicitation_id: String,
    pub ord: u32,
    pub attempt: u32,
    pub seat: String,
    pub timeout_secs: u64,
}

#[derive(Debug)]
pub enum AskError {
    InvalidToken,
    BadRequest(String),
    RunEnded(String),
}
impl std::fmt::Display for AskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidToken => write!(f, "invalid_token: token is not bound to a unit"),
            Self::BadRequest(s) => write!(f, "bad_request: {s}"),
            Self::RunEnded(s) => write!(f, "run_ended: {s}"),
        }
    }
}
impl std::error::Error for AskError {}

struct Pending {
    run_id: String,
    ord: u32,
    attempt: u32,
    seat: String,
    request: AskRequest,
    opened: bool,
    arrival_seq: u64,
    opened_at: Instant,
}

#[derive(Default)]
pub(crate) struct Registry {
    entries: HashMap<String, Pending>,
    next_seq: u64,
}

pub(crate) fn timeout_secs() -> u64 {
    timeout_from(std::env::var("WICKED_ASK_TIMEOUT_SECS").ok().as_deref())
}
fn timeout_from(raw: Option<&str>) -> u64 {
    raw.and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(600)
        .clamp(10, 3600) as u64
}

impl Registry {
    /// Called by the actor: admission order is the order OpenAsk commands are handled.
    pub(crate) fn open(
        &mut self,
        unit: McpUnit,
        request: AskRequest,
    ) -> (AskTicket, Option<CoreEvent>) {
        let id = format!("ask-{}", uuid::Uuid::new_v4().simple());
        let opened = !self
            .entries
            .values()
            .any(|p| p.run_id == unit.run_id && p.opened);
        let arrival_seq = self.next_seq;
        self.next_seq = arrival_seq.checked_add(1).expect("ask sequence exhausted");
        let pending = Pending {
            run_id: unit.run_id,
            ord: unit.ord,
            attempt: unit.attempt,
            seat: unit.seat,
            request,
            opened,
            arrival_seq,
            opened_at: Instant::now(),
        };
        let ticket = AskTicket {
            run_id: pending.run_id.clone(),
            elicitation_id: id.clone(),
            ord: pending.ord,
            attempt: pending.attempt,
            seat: pending.seat.clone(),
            timeout_secs: timeout_secs(),
        };
        let event = opened.then(|| created(&id, &pending));
        self.entries.insert(id, pending);
        (ticket, event)
    }
}

fn created(id: &str, p: &Pending) -> CoreEvent {
    CoreEvent::ElicitationCreated {
        session: p.run_id.clone(),
        epoch: 0,
        elicitation_id: id.into(),
        message: p.request.question.clone(),
        options: p.request.choices.clone(),
        prop_type: Some("string".into()),
        recommended: p.request.recommended,
        source: Some("ask".into()),
        ord: Some(p.ord),
        attempt: Some(p.attempt),
        seat: Some(p.seat.clone()),
    }
}

fn promote(registry: &mut Registry, run: &str) -> Option<CoreEvent> {
    let (id, p) = registry
        .entries
        .iter_mut()
        .filter(|(_, p)| p.run_id == run && !p.opened)
        .min_by_key(|(_, p)| p.arrival_seq)?;
    p.opened = true;
    // heldMs measures the time shown to the operator, excluding time spent queued.
    p.opened_at = Instant::now();
    Some(created(id, p))
}

fn resolve_and_promote(
    registry: &mut Registry,
    id: &str,
    p: Pending,
    action: &str,
    reason: &str,
    answer: Option<String>,
) -> Vec<CoreEvent> {
    let run = p.run_id.clone();
    let mut events = vec![resolved(id, p, action, reason, answer)];
    events.extend(promote(registry, &run));
    events
}

fn resolved(id: &str, p: Pending, action: &str, reason: &str, answer: Option<String>) -> CoreEvent {
    CoreEvent::ElicitationResolved {
        session: p.run_id,
        elicitation_id: id.into(),
        action: action.into(),
        reason: reason.into(),
        answer,
        held_ms: Some(p.opened_at.elapsed().as_millis().min(u64::MAX as u128) as u64),
    }
}

impl Registry {
    #[cfg(test)]
    pub(crate) fn is_open(&self, id: &str) -> bool {
        self.entries.get(id).is_some_and(|p| p.opened)
    }

    /// Remove every ask for a terminal run, tearing down only the one shown to the operator.
    pub(crate) fn close_run(&mut self, run: &str) -> Vec<CoreEvent> {
        let ids: Vec<_> = self
            .entries
            .iter()
            .filter(|(_, p)| p.run_id == run)
            .map(|(id, _)| id.clone())
            .collect();
        ids.into_iter()
            .filter_map(|id| {
                let p = self.entries.remove(&id).expect("entry checked above");
                p.opened
                    .then(|| resolved(&id, p, "cancel", "teardown", None))
            })
            .collect()
    }

    /// End exactly one unit attempt. Drop its whole queue before promoting another attempt.
    pub(crate) fn close_attempt(&mut self, run: &str, ord: u32, attempt: u32) -> Vec<CoreEvent> {
        let ids: Vec<_> = self
            .entries
            .iter()
            .filter(|(_, p)| p.run_id == run && p.ord == ord && p.attempt == attempt)
            .map(|(id, _)| id.clone())
            .collect();
        let mut events = Vec::new();
        for id in ids {
            let p = self.entries.remove(&id).expect("entry checked above");
            if p.opened {
                events.push(resolved(&id, p, "cancel", "teardown", None));
            }
        }
        if !events.is_empty() {
            events.extend(promote(self, run));
        }
        events
    }

    /// Some(empty) means a queued ask was removed without ever being shown.
    pub(crate) fn close(&mut self, id: &str, reason: &str) -> Option<Vec<CoreEvent>> {
        if !matches!(reason, "timeout" | "teardown") {
            return None;
        }
        let p = self.entries.remove(id)?;
        Some(if p.opened {
            resolve_and_promote(self, id, p, "cancel", reason, None)
        } else {
            Vec::new()
        })
    }

    pub(crate) fn resolve(
        &mut self,
        run: &str,
        id: &str,
        action: &str,
        response: Option<&Value>,
    ) -> anyhow::Result<Option<Vec<CoreEvent>>> {
        if !self
            .entries
            .get(id)
            .is_some_and(|p| p.run_id == run && p.opened)
        {
            return Ok(None);
        }
        if !matches!(action, "accept" | "decline" | "cancel") {
            anyhow::bail!("invalid elicitation action");
        }
        let answer = if action == "accept" {
            Some(
                response
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("accept requires a string response"))?
                    .to_owned(),
            )
        } else {
            None
        };
        let p = self.entries.remove(id).expect("entry checked above");
        Ok(Some(resolve_and_promote(
            self, id, p, action, "human", answer,
        )))
    }

    /// Drain this actor's questions without promoting anything during teardown.
    pub(crate) fn drain(&mut self) -> Vec<CoreEvent> {
        self.entries
            .drain()
            .filter_map(|(id, p)| {
                p.opened
                    .then(|| resolved(&id, p, "cancel", "teardown", None))
            })
            .collect()
    }
}

/// The unit a token is bound to, as crew needs it BEFORE opening (identity and timeout): records and
/// emits nothing. `askUnit` in the napi layer.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AskUnit {
    pub run_id: String,
    pub ord: u32,
    pub attempt: u32,
    pub seat: String,
    pub timeout_secs: u64,
}

pub(crate) fn ask_unit_json(token: &str) -> Result<String, String> {
    let unit = crate::mcp_gate::resolve(token).ok_or_else(|| AskError::InvalidToken.to_string())?;
    serde_json::to_string(&AskUnit {
        run_id: unit.run_id,
        ord: unit.ord,
        attempt: unit.attempt,
        seat: unit.seat,
        timeout_secs: timeout_secs(),
    })
    .map_err(|e| AskError::BadRequest(e.to_string()).to_string())
}

pub(crate) fn open_ask_json(core: &Core, request_json: &str) -> Result<String, String> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Request {
        token: String,
        question: String,
        choices: Option<Vec<String>>,
        recommended: Option<u32>,
    }
    let r: Request = serde_json::from_str(request_json)
        .map_err(|e| AskError::BadRequest(e.to_string()).to_string())?;
    let ticket = core
        .open_ask(
            &r.token,
            AskRequest {
                question: r.question,
                choices: r.choices,
                recommended: r.recommended,
            },
        )
        .map_err(|e| e.to_string())?;
    serde_json::to_string(&ticket).map_err(|e| AskError::BadRequest(e.to_string()).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::Command;
    use crate::domain::{AgentSession, HumanConfirm, SessionStatus};
    use crate::mcp_gate::{McpMode, McpToken};
    use crate::workflow::PhaseRole;
    use crate::write_posture::WritePosture;
    use wicked_apps_core::{open_store, ToNode};

    fn request() -> AskRequest {
        AskRequest {
            question: "pick one".into(),
            choices: Some(vec!["A".into(), "B".into()]),
            recommended: Some(1),
        }
    }
    fn unit(run_id: &str) -> McpUnit {
        McpUnit {
            run_id: run_id.into(),
            attempt: 2,
            ord: 3,
            scope: "scope".into(),
            phase: "unit-3".into(),
            phase_id: "build".into(),
            catalog: String::new(),
            role: PhaseRole::Creator,
            posture: WritePosture::Full,
            seat: "claude".into(),
            mode: McpMode::Ask,
            decisions_path: "/unused".into(),
            db_path: String::new(),
        }
    }
    struct Fixture {
        core: Core,
        runner: std::sync::Arc<crate::AcpStepRunner>,
        dir: std::path::PathBuf,
        run: String,
    }
    impl Fixture {
        fn new(status: SessionStatus) -> Self {
            let dir =
                std::env::temp_dir().join(format!("wicked-ask-{}", uuid::Uuid::new_v4().simple()));
            std::fs::create_dir_all(&dir).unwrap();
            let run = format!("ask-test-{}", uuid::Uuid::new_v4().simple());
            let db = dir.join("core.db").to_string_lossy().into_owned();
            let session = AgentSession {
                id: run.clone(),
                workflow_id: "wf".into(),
                problem: "p".into(),
                entity_mode: crate::EntityMode::Shared,
                collection_scope: None,
                clis: vec![],
                status,
                human_confirm: HumanConfirm::All,
                auto_deliver: false,
                unit_ix: 0,
                attempt: 2,
                workdir: None,
                repo_ref: None,
                extra_write_roots: vec![],
                extra_read_roots: vec![],
                project_graph: None,
                project_id: None,
                archived_at: None,
                archive_note: None,
                verified_tree: None,
                run_branch: None,
                base_commit: None,
                finished_at: None,
                benched_seats: vec![],
                team: None,
                team_plan: None,
                exclude_seats: vec![],
                evidence_root: None,
                assurance: Default::default(),
                intent_amendments: vec![],
            };
            // Seed before the actor starts; never open a second writer beside it.
            {
                let mut store = open_store(Some(&db)).unwrap();
                crate::domain::put_node(&mut store, session.to_node()).unwrap();
            }
            // This rig has ACP maps: ask creations would be suppressed without the exemption.
            let (core, runner) = Core::spawn_with_acp_sessions(db);
            Self {
                core,
                runner,
                dir,
                run,
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            // Shutdown is queued by Core's last handle; unlink only after the test's reads.
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
    fn frame(events: &std::sync::mpsc::Receiver<CoreEvent>, kind: &str) -> Value {
        loop {
            let value = events
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap()
                .to_json();
            if value["type"] == kind {
                return value;
            }
        }
    }

    fn assert_frames(
        f: &Fixture,
        events: &std::sync::mpsc::Receiver<CoreEvent>,
        expected: &[(&str, &str)],
    ) {
        let live: Vec<_> = events.try_iter().map(|ev| ev.to_json()).collect();
        let records = f.core.run_events(&f.run);
        for frames in [&live, &records] {
            assert_eq!(frames.len(), expected.len(), "{frames:?}");
            for (frame, (kind, id)) in frames.iter().zip(expected) {
                assert_eq!(frame["type"], *kind);
                assert_eq!(frame["elicitationId"], *id);
            }
        }
    }

    #[test]
    fn ask_actor_queues_by_arrival_and_promotes_after_each_operator_answer() {
        let f = Fixture::new(SessionStatus::AwaitingHuman);
        let events = f.core.subscribe();
        let token = McpToken::mint();
        let _binding = token.bind(unit(&f.run));
        let tickets: Vec<_> = (0..4)
            .map(|i| {
                let mut r = request();
                r.question = format!("question {i}");
                f.core.open_ask(token.value(), r).unwrap()
            })
            .collect();
        let first = frame(&events, "elicitationCreated");
        assert_eq!(first["elicitationId"], tickets[0].elicitation_id);
        assert_eq!(first["message"], "question 0");
        assert!(events.try_recv().is_err());
        assert_eq!(f.core.run_events(&f.run).len(), 1);

        for (i, ticket) in tickets.iter().enumerate() {
            f.core
                .resolve_elicitation(
                    &f.run,
                    &ticket.elicitation_id,
                    "accept".into(),
                    Some(serde_json::json!("B")),
                )
                .unwrap();
            let resolved = frame(&events, "elicitationResolved");
            assert_eq!(resolved["elicitationId"], ticket.elicitation_id);
            assert_eq!(resolved["answer"], "B");
            if let Some(next) = tickets.get(i + 1) {
                let created = frame(&events, "elicitationCreated");
                assert_eq!(created["elicitationId"], next.elicitation_id);
                assert_eq!(created["message"], format!("question {}", i + 1));
                assert_eq!(created["options"], serde_json::json!(["A", "B"]));
                assert_eq!(created["recommended"], 1);
                assert_eq!(created["source"], "ask");
                assert_eq!(created["ord"], next.ord);
                assert_eq!(created["attempt"], next.attempt);
                assert_eq!(created["seat"], next.seat);
            }
            assert!(events.try_recv().is_err());
        }
        let records = f.core.run_events(&f.run);
        assert_eq!(records.len(), 8);
        for (pair, ticket) in records.as_chunks::<2>().0.iter().zip(&tickets) {
            assert_eq!(pair[0]["type"], "elicitationCreated");
            assert_eq!(pair[1]["type"], "elicitationResolved");
            assert_eq!(pair[0]["elicitationId"], ticket.elicitation_id);
            assert_eq!(pair[1]["elicitationId"], ticket.elicitation_id);
        }
    }

    #[test]
    fn ask_actor_closes_queued_questions_without_frames_or_promotion() {
        for reason in ["timeout", "teardown"] {
            let f = Fixture::new(SessionStatus::AwaitingHuman);
            let events = f.core.subscribe();
            let token = McpToken::mint();
            let _binding = token.bind(unit(&f.run));
            let first = f.core.open_ask(token.value(), request()).unwrap();
            let second = f.core.open_ask(token.value(), request()).unwrap();
            assert!(f.core.close_ask(&second.elicitation_id, reason));
            assert!(!f.core.close_ask(&second.elicitation_id, reason));
            f.core
                .resolve_elicitation(&f.run, &first.elicitation_id, "decline".into(), None)
                .unwrap();
            assert_frames(
                &f,
                &events,
                &[
                    ("elicitationCreated", &first.elicitation_id),
                    ("elicitationResolved", &first.elicitation_id),
                ],
            );
        }
    }

    #[test]
    fn ask_actor_close_of_open_question_promotes_and_resets_shown_time() {
        for reason in ["timeout", "teardown"] {
            let f = Fixture::new(SessionStatus::AwaitingHuman);
            let events = f.core.subscribe();
            let token = McpToken::mint();
            let _binding = token.bind(unit(&f.run));
            let first = f.core.open_ask(token.value(), request()).unwrap();
            let second = f.core.open_ask(token.value(), request()).unwrap();
            assert!(f.core.close_ask(&first.elicitation_id, reason));
            assert!(!f.core.close_ask(&first.elicitation_id, reason));
            assert!(f.core.close_ask(&second.elicitation_id, "timeout"));
            assert_frames(
                &f,
                &events,
                &[
                    ("elicitationCreated", &first.elicitation_id),
                    ("elicitationResolved", &first.elicitation_id),
                    ("elicitationCreated", &second.elicitation_id),
                    ("elicitationResolved", &second.elicitation_id),
                ],
            );
            let records = f.core.run_events(&f.run);
            assert_eq!(records[1]["reason"], reason);
            assert!(records[3]["heldMs"].is_u64());
        }
    }

    #[test]
    fn ask_promotion_resets_time_spent_queued() {
        let mut registry = Registry::default();
        let (first, _) = registry.open(unit("run"), request());
        let (second, _) = registry.open(unit("run"), request());
        registry
            .entries
            .get_mut(&second.elicitation_id)
            .unwrap()
            .opened_at = Instant::now() - std::time::Duration::from_secs(60);
        let promotion_start = Instant::now();
        registry.close(&first.elicitation_id, "timeout").unwrap();
        let pending = &registry.entries[&second.elicitation_id];
        assert!(pending.opened);
        assert!(pending.opened_at >= promotion_start);
    }

    #[test]
    fn ask_actor_shutdown_drops_queue_and_reopened_run_displays_next_ask_immediately() {
        let mut f = Fixture::new(SessionStatus::AwaitingHuman);
        let events = f.core.subscribe();
        let token = McpToken::mint();
        let _binding = token.bind(unit(&f.run));
        let first = f.core.open_ask(token.value(), request()).unwrap();
        let queued = f.core.open_ask(token.value(), request()).unwrap();
        assert_eq!(
            frame(&events, "elicitationCreated")["elicitationId"],
            first.elicitation_id
        );
        assert!(events.try_recv().is_err());
        f.core.tx.send(Command::Shutdown).unwrap();
        let resolved = frame(&events, "elicitationResolved");
        assert_eq!(resolved["elicitationId"], first.elicitation_id);
        assert_eq!(resolved["action"], "cancel");
        assert_eq!(resolved["reason"], "teardown");
        assert!(matches!(
            events.recv_timeout(std::time::Duration::from_secs(10)),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
        ));
        // Wait for the command receiver to drop too: the actor drops its store before its
        // receiver, so no second writer can overlap the old actor's final cleanup.
        assert!(f.core.sessions().is_err());
        // The persisted AwaitingHuman run is still resumable; no terminal event was invented.
        let (core, runner) = Core::spawn_with_acp_sessions(f.dir.join("core.db").to_string_lossy());
        f.core = core;
        f.runner = runner;
        assert_eq!(
            f.core.sessions_detail().unwrap()[0].session.status,
            SessionStatus::AwaitingHuman
        );
        let events = f.core.subscribe();
        assert!(!f.core.close_ask(&first.elicitation_id, "timeout"));
        assert!(!f.core.close_ask(&queued.elicitation_id, "timeout"));
        let next = f.core.open_ask(token.value(), request()).unwrap();
        assert_eq!(
            frame(&events, "elicitationCreated")["elicitationId"],
            next.elicitation_id
        );
        let records = f.core.run_events(&f.run);
        assert_eq!(records.len(), 3, "{records:?}");
        assert_eq!(records[1]["reason"], "teardown");
        assert_eq!(records[2]["elicitationId"], next.elicitation_id);
        assert!(!records
            .iter()
            .any(|event| event["elicitationId"] == queued.elicitation_id));
        assert!(f.core.close_ask(&next.elicitation_id, "teardown"));
    }

    #[test]
    fn ask_actor_rejects_queued_resolution_like_an_unknown_id() {
        let f = Fixture::new(SessionStatus::AwaitingHuman);
        let events = f.core.subscribe();
        let token = McpToken::mint();
        let _binding = token.bind(unit(&f.run));
        let first = f.core.open_ask(token.value(), request()).unwrap();
        let second = f.core.open_ask(token.value(), request()).unwrap();
        for action in ["accept", "decline", "cancel"] {
            let error = f
                .core
                .resolve_elicitation(
                    &f.run,
                    &second.elicitation_id,
                    action.into(),
                    Some(serde_json::json!("A")),
                )
                .unwrap_err();
            assert_eq!(
                error.to_string(),
                format!("elicitation not found: {}", second.elicitation_id)
            );
        }
        assert_eq!(f.core.run_events(&f.run).len(), 1);
        f.core
            .resolve_elicitation(&f.run, &first.elicitation_id, "decline".into(), None)
            .unwrap();
        f.core
            .resolve_elicitation(
                &f.run,
                &second.elicitation_id,
                "accept".into(),
                Some(serde_json::json!("A")),
            )
            .unwrap();
        assert_frames(
            &f,
            &events,
            &[
                ("elicitationCreated", &first.elicitation_id),
                ("elicitationResolved", &first.elicitation_id),
                ("elicitationCreated", &second.elicitation_id),
                ("elicitationResolved", &second.elicitation_id),
            ],
        );
    }

    #[test]
    fn ask_open_bound_token_and_actor_resolve_first_preserve_the_durable_answer() {
        let f = Fixture::new(SessionStatus::AwaitingHuman);
        let events = f.core.subscribe();
        let token = McpToken::mint();
        let _binding = token.bind(unit(&f.run));
        let ticket: Value = serde_json::from_str(
            &f.core
                .open_ask_json(
                    &serde_json::json!({
                        "token": token.value(), "question": "pick one", "choices": ["A", "B"]
                    })
                    .to_string(),
                )
                .unwrap(),
        )
        .unwrap();
        let id = ticket["elicitationId"].as_str().unwrap();
        assert!(id.starts_with("ask-"));
        assert_eq!(ticket["runId"], f.run);
        assert_eq!(ticket["ord"], 3);
        assert_eq!(ticket["attempt"], 2);
        assert_eq!(ticket["seat"], "claude");
        let created = frame(&events, "elicitationCreated");
        assert_eq!(created["source"], "ask");
        assert_eq!(created["epoch"], 0);
        assert_eq!(created["message"], "pick one");
        assert_eq!(created["options"], serde_json::json!(["A", "B"]));
        assert_eq!(created["ord"], 3);
        assert_eq!(created["attempt"], 2);
        // The frame names the seat that asked, as the ticket does.
        assert_eq!(created["seat"], "claude");
        assert!(f
            .core
            .resolve_elicitation(
                "wrong-run",
                id,
                "accept".into(),
                Some(serde_json::json!("A"))
            )
            .is_err());
        f.core
            .resolve_elicitation(&f.run, id, "accept".into(), Some(serde_json::json!("B")))
            .unwrap();
        let resolved = frame(&events, "elicitationResolved");
        assert_eq!(resolved["answer"], "B");
        assert_eq!(resolved["reason"], "human");
        assert!(resolved["heldMs"].is_u64());
        assert!(!f.core.close_ask(id, "timeout"));
        assert!(f
            .core
            .resolve_elicitation(&f.run, id, "accept".into(), Some(serde_json::json!("A")))
            .is_err());
        // The actor's normal event sink, not just the subscriber, carries the answer.
        assert!(f
            .core
            .run_events(&f.run)
            .iter()
            .any(|v| v["type"] == "elicitationResolved" && v["answer"] == "B"));
    }

    #[test]
    fn ask_bound_token_on_terminal_run_emits_nothing() {
        for status in [
            SessionStatus::Completed,
            SessionStatus::Cancelled,
            SessionStatus::Failed,
        ] {
            let f = Fixture::new(status);
            let events = f.core.subscribe();
            let token = McpToken::mint();
            let _binding = token.bind(unit(&f.run));
            assert!(matches!(
                f.core.open_ask(token.value(), request()),
                Err(AskError::RunEnded(_))
            ));
            assert!(f.core.run_events(&f.run).is_empty());
            assert!(events.try_recv().is_err());
        }
    }

    #[test]
    fn ask_actor_rechecks_a_revoked_binding() {
        let f = Fixture::new(SessionStatus::AwaitingHuman);
        let token = McpToken::mint();
        let binding = token.bind(unit(&f.run));
        // A caller's earlier resolution cannot authorize the later actor command.
        assert!(crate::mcp_gate::resolve(token.value()).is_some());
        drop(binding);
        let (reply, rx) = std::sync::mpsc::channel();
        f.core
            .tx
            .send(Command::OpenAsk {
                unit: token.value().into(),
                request: request(),
                reply,
            })
            .unwrap();
        assert!(matches!(rx.recv().unwrap(), Err(AskError::InvalidToken)));
        assert!(f.core.run_events(&f.run).is_empty());
    }

    #[test]
    fn ask_open_then_terminal_records_created_terminal_resolved_in_order() {
        let f = Fixture::new(SessionStatus::AwaitingHuman);
        let events = f.core.subscribe();
        let token = McpToken::mint();
        let _binding = token.bind(unit(&f.run));
        let (reply, opened) = std::sync::mpsc::channel();
        f.core
            .tx
            .send(Command::OpenAsk {
                unit: token.value().into(),
                request: request(),
                reply,
            })
            .unwrap();
        let (reply, queued) = std::sync::mpsc::channel();
        f.core
            .tx
            .send(Command::OpenAsk {
                unit: token.value().into(),
                request: request(),
                reply,
            })
            .unwrap();
        // Enqueue termination before waiting for creation, covering the old queued-EmitEvent hop.
        let (reply, cancelled) = std::sync::mpsc::channel();
        f.core
            .tx
            .send(Command::CancelRun {
                run_id: f.run.clone(),
                reply,
            })
            .unwrap();
        let ticket = opened.recv().unwrap().unwrap();
        let queued = queued.recv().unwrap().unwrap();
        assert_eq!(cancelled.recv().unwrap().unwrap(), SessionStatus::Cancelled);
        let live: Vec<_> = events.try_iter().map(|event| event.to_json()).collect();
        let records = f.core.run_events(&f.run);
        for frames in [&records, &live] {
            assert_eq!(frames.len(), 3, "{frames:?}");
            assert_eq!(frames[0]["type"], "elicitationCreated");
            assert_eq!(frames[1]["type"], "runCancelled");
            assert_eq!(frames[2]["type"], "elicitationResolved");
            assert_eq!(frames[0]["elicitationId"], ticket.elicitation_id);
            assert_eq!(frames[2]["elicitationId"], ticket.elicitation_id);
            assert_eq!(frames[2]["reason"], "teardown");
        }
        assert!(!f.core.close_ask(&ticket.elicitation_id, "timeout"));
        assert!(!f.core.close_ask(&queued.elicitation_id, "timeout"));
        assert!(matches!(
            f.core.open_ask(token.value(), request()),
            Err(AskError::RunEnded(_))
        ));
    }

    #[test]
    fn ask_invalid_accept_keeps_the_question_open_for_a_valid_answer() {
        let f = Fixture::new(SessionStatus::AwaitingHuman);
        let token = McpToken::mint();
        let _binding = token.bind(unit(&f.run));
        let ticket = f.core.open_ask(token.value(), request()).unwrap();
        for response in [
            None,
            Some(Value::Null),
            Some(serde_json::json!(42)),
            Some(serde_json::json!({"answer": "A"})),
        ] {
            assert!(f
                .core
                .resolve_elicitation(&f.run, &ticket.elicitation_id, "accept".into(), response)
                .is_err());
            assert_eq!(f.core.run_events(&f.run).len(), 1);
        }
        f.core
            .resolve_elicitation(
                &f.run,
                &ticket.elicitation_id,
                "accept".into(),
                Some(serde_json::json!("B")),
            )
            .unwrap();
        let records = f.core.run_events(&f.run);
        assert_eq!(records.len(), 2);
        assert_eq!(records[1]["answer"], "B");
    }

    #[test]
    fn ask_unit_names_the_bound_unit_and_emits_nothing() {
        let f = Fixture::new(SessionStatus::AwaitingHuman);
        let token = McpToken::mint();
        assert!(f
            .core
            .ask_unit_json(token.value())
            .unwrap_err()
            .starts_with("invalid_token:"));
        let binding = token.bind(unit(&f.run));
        let u: Value = serde_json::from_str(&f.core.ask_unit_json(token.value()).unwrap()).unwrap();
        assert_eq!(u["runId"], f.run);
        assert_eq!(u["ord"], 3);
        assert_eq!(u["attempt"], 2);
        assert_eq!(u["seat"], "claude");
        assert_eq!(u["timeoutSecs"], timeout_secs());
        assert!(f.core.run_events(&f.run).is_empty());
        drop(binding);
        assert!(f.core.ask_unit_json(token.value()).is_err());
    }

    #[test]
    fn ask_actor_suppression_uses_the_id_prefix_for_both_frames() {
        let f = Fixture::new(SessionStatus::AwaitingHuman);
        let events = f.core.subscribe();
        let mut registry = Registry::default();
        let (ticket, created) = registry.open(unit(&f.run), request());
        let mut created = created.unwrap();
        if let CoreEvent::ElicitationCreated { source, .. } = &mut created {
            *source = None;
        }
        f.core.tx.send(Command::EmitEvent(created.clone())).unwrap();
        assert_eq!(
            frame(&events, "elicitationCreated")["elicitationId"],
            ticket.elicitation_id
        );

        // Even an ACP suppression marker cannot consume an ask resolution, regardless of heldMs.
        f.runner
            .elicitation_maps()
            .lock()
            .unwrap()
            .mark_resolution_suppressed(&ticket.elicitation_id);
        let mut resolved = registry
            .resolve(&f.run, &ticket.elicitation_id, "decline", None)
            .unwrap()
            .unwrap()
            .remove(0);
        if let CoreEvent::ElicitationResolved { held_ms, .. } = &mut resolved {
            *held_ms = None;
        }
        f.core
            .tx
            .send(Command::EmitEvent(resolved.clone()))
            .unwrap();
        assert_eq!(
            frame(&events, "elicitationResolved")["elicitationId"],
            ticket.elicitation_id
        );

        // Metadata alone cannot make legacy frames bypass ACP suppression.
        let legacy_id = format!("legacy-{}", uuid::Uuid::new_v4().simple());
        if let CoreEvent::ElicitationCreated {
            elicitation_id,
            source,
            ..
        } = &mut created
        {
            *elicitation_id = legacy_id.clone();
            *source = Some("ask".into());
        }
        if let CoreEvent::ElicitationResolved {
            elicitation_id,
            held_ms,
            ..
        } = &mut resolved
        {
            *elicitation_id = legacy_id;
            *held_ms = Some(1);
        }
        f.core.tx.send(Command::EmitEvent(created)).unwrap();
        f.core.tx.send(Command::EmitEvent(resolved)).unwrap();
        f.core.sessions().unwrap();
        assert!(matches!(
            events.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        assert_eq!(f.core.run_events(&f.run).len(), 2);
    }

    #[test]
    fn ask_unbound_is_refused_and_close_only_emits_once() {
        let f = Fixture::new(SessionStatus::AwaitingHuman);
        let token = McpToken::mint();
        assert!(matches!(
            f.core.open_ask(token.value(), request()),
            Err(AskError::InvalidToken)
        ));
        let binding = token.bind(unit(&f.run));
        let events = f.core.subscribe();
        let t = f.core.open_ask(token.value(), request()).unwrap();
        frame(&events, "elicitationCreated");
        assert!(!f.core.close_ask(&t.elicitation_id, "invalid"));
        assert!(f.core.close_ask(&t.elicitation_id, "timeout"));
        assert!(!f.core.close_ask(&t.elicitation_id, "timeout"));
        let v = frame(&events, "elicitationResolved");
        assert_eq!(v["action"], "cancel");
        assert_eq!(v["reason"], "timeout");
        assert!(v.get("answer").is_none());
        assert!(v["heldMs"].is_u64());
        drop(binding);
        assert!(matches!(
            f.core.open_ask(token.value(), request()),
            Err(AskError::InvalidToken)
        ));
    }

    #[test]
    fn ask_validation_uses_byte_caps_and_dedupes() {
        let mut r = request();
        r.choices = Some(vec!["A".into(), "B".into(), "A".into()]);
        r.recommended = Some(2);
        let r = r.validate().unwrap();
        assert_eq!(r.choices.unwrap(), ["A", "B"]);
        assert_eq!(r.recommended, Some(0));
        for question in [String::new(), "é".repeat(4097)] {
            let mut r = request();
            r.question = question;
            assert!(matches!(r.validate(), Err(AskError::BadRequest(_))));
        }
        let mut r = request();
        r.question = "é".repeat(4096);
        assert!(r.validate().is_ok());
        for choices in [
            vec!["A".into()],
            vec!["A".into(); 101],
            vec!["".into(), "B".into()],
            vec!["é".repeat(257), "B".into()],
        ] {
            let mut r = request();
            r.choices = Some(choices);
            assert!(r.validate().is_err());
        }
        let mut r = request();
        r.choices = None;
        r.recommended = None;
        assert_eq!(r.validate().unwrap().recommended, None);
        assert_eq!(timeout_from(None), 600);
        assert_eq!(timeout_from(Some("bad")), 600);
        assert_eq!(timeout_from(Some("-2")), 10);
        assert_eq!(timeout_from(Some("9999")), 3600);
        assert_eq!(timeout_from(Some("70")), 70);
    }

    #[test]
    fn ask_validation_requires_two_distinct_choices() {
        let mut r = request();
        r.choices = Some(vec!["yes".into(), "yes".into()]);
        let err = r.validate().unwrap_err();
        assert!(matches!(err, AskError::BadRequest(_)));
        assert!(err.to_string().starts_with("bad_request:"));
    }

    #[test]
    fn ask_validation_remaps_recommended_to_the_original_choice() {
        for (original, deduped) in [(0, 0), (1, 0), (2, 1), (3, 2)] {
            let mut r = request();
            r.choices = Some(vec!["a".into(), "a".into(), "b".into(), "c".into()]);
            r.recommended = Some(original);
            let r = r.validate().unwrap();
            assert_eq!(r.choices.unwrap(), ["a", "b", "c"]);
            assert_eq!(r.recommended, Some(deduped));
        }
    }

    #[test]
    fn ask_validation_refuses_recommended_outside_the_callers_choices() {
        for choices in [None, Some(vec!["a".into(), "a".into(), "b".into()])] {
            for index in [3, u32::MAX] {
                let mut r = request();
                r.choices = choices.clone();
                r.recommended = Some(index);
                assert!(matches!(r.validate(), Err(AskError::BadRequest(_))));
            }
        }
        let mut r = request();
        r.choices = None;
        r.recommended = Some(0);
        assert!(matches!(r.validate(), Err(AskError::BadRequest(_))));
    }

    #[test]
    fn ask_close_races_human_resolution_with_one_winner() {
        let f = Fixture::new(SessionStatus::AwaitingHuman);
        let events = f.core.subscribe();
        let token = McpToken::mint();
        let _b = token.bind(unit(&f.run));
        let t = f.core.open_ask(token.value(), request()).unwrap();
        frame(&events, "elicitationCreated");
        let c = f.core.clone();
        let id = t.elicitation_id.clone();
        let closer = std::thread::spawn(move || c.close_ask(&id, "teardown"));
        let human = f
            .core
            .resolve_elicitation(&f.run, &t.elicitation_id, "decline".into(), None)
            .is_ok();
        assert_ne!(closer.join().unwrap(), human);
        frame(&events, "elicitationResolved");
        // An actor read flushes any queued close event before inspecting the durable trail.
        f.core.sessions().unwrap();
        assert_eq!(
            f.core
                .run_events(&f.run)
                .iter()
                .filter(|v| v["type"] == "elicitationResolved")
                .count(),
            1
        );
    }
}
