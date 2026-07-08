//! Node / TypeScript bindings (napi-rs) for **wicked-core** — drive the in-process composition /
//! orchestration runtime from JS/TS.
//!
//! ```js
//! const { Core } = require('./wicked-core.node')
//! const core = Core.spawnStub('/tmp/core.db')   // stub engine: deterministic, no real LLM CLI
//! core.subscribe((json) => console.log(JSON.parse(json)))     // live CoreEvent stream
//! const runId = await core.launchRun({
//!   problem: 'Do step one. Do step two',
//!   sessionId: 'demo',
//!   clisJson: JSON.stringify([{ key: 'a', display_name: 'A', binary: 'a', headless_invocation: 'a {PROMPT}' }]),
//!   humanConfirm: 'before:1',                    // pause before unit 1
//! })
//! // ... on an `awaitingHuman` event:
//! await core.confirmGate(runId, true)            // Approve → run advances to completion
//! ```
//!
//! ## Async shape
//! `wicked_core::Core` is a **sync-blocking** handle: each method sends a `Command` to the store
//! actor and blocks on a oneshot reply (`std::sync::mpsc`, NOT tokio). `Core` is `Send + Sync` and
//! cheap to `Clone`, so every binding method clones the handle into a napi [`AsyncTask`] whose
//! `compute()` runs on a libuv worker thread — the Node event loop is never blocked on the actor
//! round-trip. The live event stream ([`Core::subscribe`]) moves the `Receiver<CoreEvent>` into a
//! dedicated pump thread that forwards each event (as a JSON string) through a
//! [`ThreadsafeFunction`], preserving emission order.
//!
//! Build: `cargo build -p wicked-core-ts` from this directory (produces the cdylib), then rename /
//! copy the artifact to `wicked-core.node` for Node to `require()`. The `.cargo/config.toml` here
//! injects the macOS `dynamic_lookup` linker flags so a plain `cargo build` links the addon without
//! the full `napi build` CLI.

use std::sync::Arc;

use napi::bindgen_prelude::AsyncTask;
use napi::threadsafe_function::{
    ErrorStrategy, ThreadSafeCallContext, ThreadsafeFunction, ThreadsafeFunctionCallMode,
};
use napi::{Env, JsFunction, Task};
use napi_derive::napi;

use wicked_council::types::{Confidence, CouncilTask, Dispatcher, Vote};
use wicked_council::AgenticCli;
use wicked_core::{
    CoreEvent, EntityMode, HumanConfirm, HumanDecision, LaunchSpec, RepoSpec, SessionStatus,
    StubStepRunner,
};

// ── helpers ──────────────────────────────────────────────────────────────────

/// Map any displayable error onto a napi error (mirrors wicked-memory-ts's `err`).
fn err<E: std::fmt::Display>(e: E) -> napi::Error {
    napi::Error::from_reason(e.to_string())
}

/// Wall-clock now in unix seconds (for repo registration timestamps).
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The snake_case wire token for a [`SessionStatus`] (matches its serde representation).
fn status_token(s: SessionStatus) -> String {
    match s {
        SessionStatus::Planning => "planning",
        SessionStatus::Distributing => "distributing",
        SessionStatus::Executing => "executing",
        SessionStatus::AwaitingHuman => "awaiting_human",
        SessionStatus::Completed => "completed",
        SessionStatus::Cancelled => "cancelled",
        SessionStatus::Failed => "failed",
    }
    .to_string()
}

/// Parse the human-confirm gate policy from a wire token: `none` | `all` | `before:<ord>`.
fn parse_human_confirm(raw: Option<&str>) -> HumanConfirm {
    match raw.map(str::trim) {
        Some("all") => HumanConfirm::All,
        Some(s) if s.starts_with("before:") => s["before:".len()..]
            .trim()
            .parse::<u32>()
            .map(HumanConfirm::Before)
            .unwrap_or(HumanConfirm::None),
        _ => HumanConfirm::None,
    }
}

/// Serialize one [`CoreEvent`] to a tagged JSON object (`{ type, ...fields }`) for the JS callback.
/// `CoreEvent` is not `serde::Serialize`, so this maps every variant by hand.
fn event_to_json(ev: &CoreEvent) -> serde_json::Value {
    use serde_json::json;
    match ev {
        CoreEvent::Heartbeat => json!({ "type": "heartbeat" }),
        CoreEvent::SessionStarted { session, problem } => {
            json!({ "type": "sessionStarted", "session": session, "problem": problem })
        }
        CoreEvent::UnitPlanned {
            session,
            ord,
            description,
        } => json!({ "type": "unitPlanned", "session": session, "ord": ord, "description": description }),
        CoreEvent::UnitDistributed { session, ord, cli } => {
            json!({ "type": "unitDistributed", "session": session, "ord": ord, "cli": cli })
        }
        CoreEvent::UnitExecuting { session, ord } => {
            json!({ "type": "unitExecuting", "session": session, "ord": ord })
        }
        CoreEvent::CliOutputDelta {
            session,
            ord,
            chunk,
        } => json!({ "type": "cliOutputDelta", "session": session, "ord": ord, "chunk": chunk }),
        CoreEvent::GateDecided {
            session,
            ord,
            allow,
        } => json!({ "type": "gateDecided", "session": session, "ord": ord, "allow": allow }),
        CoreEvent::UnitDone { session, ord } => {
            json!({ "type": "unitDone", "session": session, "ord": ord })
        }
        CoreEvent::UnitDenied { session, ord } => {
            json!({ "type": "unitDenied", "session": session, "ord": ord })
        }
        CoreEvent::AwaitingHuman {
            session,
            ord,
            prompt,
        } => json!({ "type": "awaitingHuman", "session": session, "ord": ord, "prompt": prompt }),
        CoreEvent::Resumed { session, ord } => {
            json!({ "type": "resumed", "session": session, "ord": ord })
        }
        CoreEvent::RunCancelled { session } => json!({ "type": "runCancelled", "session": session }),
        CoreEvent::SessionFailed { session, ord } => {
            json!({ "type": "sessionFailed", "session": session, "ord": ord })
        }
        CoreEvent::RepoRegistered { repo_ref } => {
            json!({ "type": "repoRegistered", "repoRef": repo_ref })
        }
        CoreEvent::SessionCompleted { session } => {
            json!({ "type": "sessionCompleted", "session": session })
        }
        CoreEvent::Error { session, message } => {
            json!({ "type": "error", "session": session, "message": message })
        }
        // PTY terminal sessions (DES-TERMINAL-001). Mapped minimally to keep this exhaustive match
        // compiling now that core carries the terminal capability; the full TS surface (openTerminal
        // etc.) is a separate follow-on task.
        CoreEvent::TerminalOpened { id, cwd } => {
            json!({ "type": "terminalOpened", "id": id, "cwd": cwd })
        }
        CoreEvent::TerminalOutput { id, seq, bytes_b64 } => {
            json!({ "type": "terminalOutput", "id": id, "seq": seq, "bytesB64": bytes_b64 })
        }
        CoreEvent::TerminalExited { id, status } => {
            json!({ "type": "terminalExited", "id": id, "status": status })
        }
    }
}

// ── the AsyncTask that runs one blocking Core call off the Node loop ──────────

/// A single blocking Core call, run on a libuv worker thread. Holds a boxed closure so one Task type
/// serves every method; every result is marshalled as a `String` (a plain value, a status token, or
/// a JSON document the caller parses).
pub struct CoreTask {
    work: Option<Box<dyn FnOnce() -> napi::Result<String> + Send>>,
}

impl Task for CoreTask {
    type Output = String;
    type JsValue = String;

    fn compute(&mut self) -> napi::Result<String> {
        let work = self
            .work
            .take()
            .ok_or_else(|| err("wicked-core-ts: task polled twice"))?;
        work()
    }

    fn resolve(&mut self, _env: Env, output: String) -> napi::Result<String> {
        Ok(output)
    }
}

/// Wrap a blocking closure in an [`AsyncTask`] → a JS `Promise<string>`.
fn task<F>(f: F) -> AsyncTask<CoreTask>
where
    F: FnOnce() -> napi::Result<String> + Send + 'static,
{
    AsyncTask::new(CoreTask {
        work: Some(Box::new(f)),
    })
}

// ── the stub engine for deterministic, no-real-LLM runs ───────────────────────

/// A deterministic council dispatcher: every seat votes for the first roster option, so the council
/// reaches a clean consensus without spawning any subprocess. Pairs with [`StubStepRunner`] (which
/// returns fixed text) to drive a full run offline — the engine seams tests inject.
struct StubDispatcher;

impl Dispatcher for StubDispatcher {
    fn dispatch(&self, cli: &AgenticCli, task: &CouncilTask) -> Option<Vote> {
        Some(Vote {
            cli: cli.key.clone(),
            recommendation: task
                .options
                .first()
                .cloned()
                .unwrap_or_else(|| cli.key.clone()),
            top_risk: "none".into(),
            change_my_mind: "no".into(),
            disqualifier: None,
            confidence: Confidence::default(),
            provenance: "wicked-core-ts stub".into(),
        })
    }
}

// ── the launch spec, as a JS object ───────────────────────────────────────────

/// Options for [`Core::launch_run`]. `clisJson` is a JSON array of `AgenticCli` seats (the council
/// roster); `Core.registryRoster()` returns the production roster ready to pass here.
#[napi(object)]
pub struct LaunchOptions {
    /// The free-text problem to decompose into ordered work units.
    pub problem: String,
    /// A stable session/run id (empty → the caller must supply one; Core requires an explicit id here).
    pub session_id: String,
    /// JSON array of `AgenticCli` seats — the council roster for this run.
    pub clis_json: String,
    /// `shared` (default) | `isolated` — the collection-scope mode.
    pub entity_mode: Option<String>,
    /// Human-confirm gate policy: `none` (default) | `all` | `before:<ord>`.
    pub human_confirm: Option<String>,
    /// The id of a registered repo to run within (creates an isolated worktree). Omit for a repo-less run.
    pub repo_ref: Option<String>,
}

fn build_spec(o: LaunchOptions) -> napi::Result<LaunchSpec> {
    let clis: Vec<AgenticCli> = serde_json::from_str(&o.clis_json)
        .map_err(|e| err(format!("clisJson is not a valid AgenticCli array: {e}")))?;
    Ok(LaunchSpec {
        problem: o.problem,
        clis,
        entity_mode: o
            .entity_mode
            .as_deref()
            .map(EntityMode::parse)
            .unwrap_or(EntityMode::Shared),
        session_id: o.session_id,
        human_confirm: parse_human_confirm(o.human_confirm.as_deref()),
        repo_ref: o.repo_ref,
    })
}

// ── the binding surface ────────────────────────────────────────────────────────

/// A handle to a wicked-core runtime. Construct with [`Core::spawn`] (production engine: real
/// council + wrapped-CLI subprocesses) or [`Core::spawn_stub`] (deterministic offline engine).
#[napi]
pub struct Core {
    inner: wicked_core::Core,
}

#[napi]
impl Core {
    /// Spawn the store actor over the estate db at `path` with the PRODUCTION engine (real council
    /// dispatcher + real wrapped-CLI step runner — runs actual agentic CLIs). The actor lives until
    /// every handle is dropped.
    #[napi(factory)]
    pub fn spawn(path: String) -> Core {
        Core {
            inner: wicked_core::Core::spawn(path),
        }
    }

    /// Spawn the store actor with the STUB engine — a deterministic council dispatcher +
    /// `StubStepRunner`, no subprocesses. For tests / offline runs that must not touch a real LLM.
    #[napi(factory)]
    pub fn spawn_stub(path: String) -> Core {
        let dispatcher: Arc<dyn Dispatcher + Send + Sync> = Arc::new(StubDispatcher);
        let runner = Arc::new(StubStepRunner);
        Core {
            inner: wicked_core::Core::spawn_with_engine(path, dispatcher, runner),
        }
    }

    /// The production council roster (built-ins ∪ the user's `~/.config/wicked-council/clis.toml`),
    /// as a JSON array of `AgenticCli` — pass straight into `launchRun`'s `clisJson`.
    #[napi]
    pub fn registry_roster() -> napi::Result<String> {
        serde_json::to_string(&wicked_core::registry_roster()).map_err(err)
    }

    /// Subscribe to the live [`CoreEvent`] stream. `callback` is invoked once per event with the
    /// event serialized as a JSON string (`{ type, ...fields }`); parse it in JS. Events arrive in
    /// emission order. Call this BEFORE `launchRun` to catch the whole sequence.
    #[napi]
    pub fn subscribe(&self, callback: JsFunction) -> napi::Result<()> {
        let tsfn: ThreadsafeFunction<String, ErrorStrategy::Fatal> = callback
            .create_threadsafe_function(0, |ctx: ThreadSafeCallContext<String>| Ok(vec![ctx.value]))?;
        let rx = self.inner.subscribe();
        std::thread::spawn(move || {
            // One reader thread → FIFO calls into the tsfn preserve CoreEvent emission order. The
            // loop ends when the actor drops the sender (last Core handle gone), releasing the tsfn.
            while let Ok(ev) = rx.recv() {
                let json = event_to_json(&ev).to_string();
                tsfn.call(json, ThreadsafeFunctionCallMode::NonBlocking);
            }
        });
        Ok(())
    }

    /// Liveness probe — emits a `Heartbeat` to subscribers and resolves once the actor acks (`"ok"`).
    #[napi]
    pub fn ping(&self) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            core.ping();
            Ok("ok".to_string())
        })
    }

    /// Launch an interactive, resumable run: plans + distributes, then executes each unit off-thread
    /// (or pauses at a human-confirm gate). Resolves to the run id. Progress arrives as `CoreEvent`s
    /// — `subscribe()` first. Rejects with a busy error if a run with that id is already in flight.
    #[napi]
    pub fn launch_run(&self, opts: LaunchOptions) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let spec = build_spec(opts)?;
            core.launch_run(spec).map_err(err)
        })
    }

    /// Resume an interactive run from its persisted cursor (after a pause, crash, or fresh process).
    /// Resolves to the resulting status token.
    #[napi]
    pub fn resume_run(&self, run_id: String) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || core.resume_run(&run_id).map(status_token).map_err(err))
    }

    /// Resolve a human-confirm gate on a PAUSED run. `approve=true` proceeds (optionally applying
    /// `amend` to the next unit's instruction); `approve=false` rejects → cancels the run. Resolves
    /// to the resulting status token. Rejects if the run is not paused at a gate.
    #[napi]
    pub fn confirm_gate(
        &self,
        run_id: String,
        approve: bool,
        amend: Option<String>,
    ) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let decision = if approve {
                HumanDecision::Approve { amend }
            } else {
                HumanDecision::Reject
            };
            core.confirm_gate(&run_id, decision)
                .map(status_token)
                .map_err(err)
        })
    }

    /// Cancel a run — mark it terminally `Cancelled` and stop advancing it. Resolves to the status
    /// token. Safe whether the run is executing or paused.
    #[napi]
    pub fn cancel_run(&self, run_id: String) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || core.cancel_run(&run_id).map(status_token).map_err(err))
    }

    // NOTE: there is intentionally no `pauseRun`. wicked-core has no imperative pause — a run pauses
    // ONLY at a declared human-confirm gate (set `humanConfirm` to `all` / `before:<ord>` at launch).
    // Exposing a fake `pauseRun` would misrepresent the engine, so it is omitted (see the report).

    /// The agent session ids currently on the store, as a JSON array of strings.
    #[napi]
    pub fn sessions(&self) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let ids = core.sessions().map_err(err)?;
            serde_json::to_string(&ids).map_err(err)
        })
    }

    /// Every session + its ordered units, as a JSON array of `{ session, units }` objects (the read
    /// a UI builds its project list from).
    #[napi]
    pub fn sessions_detail(&self) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let views = core.sessions_detail().map_err(err)?;
            let arr: Vec<serde_json::Value> = views
                .iter()
                .map(|v| {
                    serde_json::json!({
                        "session": serde_json::to_value(&v.session).unwrap_or(serde_json::Value::Null),
                        "units": serde_json::to_value(&v.units).unwrap_or(serde_json::Value::Null),
                    })
                })
                .collect();
            serde_json::to_string(&arr).map_err(err)
        })
    }

    /// A unit's captured work output (transcript), as a JSON value — a string, or `null` if none.
    #[napi]
    pub fn work_output(&self, unit_id: String) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let out = core.work_output(&unit_id);
            serde_json::to_string(&out).map_err(err)
        })
    }

    /// Register a git repository the orchestrator can run within. Validates it is a git repo with
    /// ≥1 commit; resolves to the persisted `RepoEntry` as a JSON object.
    #[napi]
    pub fn register_repo(&self, name: String, root_path: String) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let spec = RepoSpec {
                name,
                root_path,
                registered_at: now_secs(),
            };
            let entry = core.register_repo(spec).map_err(err)?;
            serde_json::to_string(&entry).map_err(err)
        })
    }

    /// List every registered repository, as a JSON array of `RepoEntry` objects.
    #[napi]
    pub fn list_repos(&self) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let repos = core.list_repos().map_err(err)?;
            serde_json::to_string(&repos).map_err(err)
        })
    }
}
