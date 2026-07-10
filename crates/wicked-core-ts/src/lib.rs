//! Node / TypeScript bindings (napi-rs) for **wicked-core** — drive the in-process composition /
//! orchestration runtime from JS/TS.
//!
//! ```js
//! const { Core } = require('wicked-core-ts')     // (or `./index.js` in-tree) — the napi loader
//! const core = Core.spawnStub('/tmp/core.db')   // stub engine: deterministic, no real LLM CLI
//! const sub = core.subscribe((err, json) => console.log(JSON.parse(json)))  // live CoreEvent stream
//! // ... later: sub.close()                      // stop delivery + tear the pump/callback down
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
//! Build: `npm run build` (`napi build --platform --release`) from this directory emits the
//! platform-suffixed addon (`wicked-core-ts.<triple>.node`) plus the generated loader `index.js` and
//! `index.d.ts`. A plain `cargo build -p wicked-core-ts` still links the cdylib too — the
//! `.cargo/config.toml` here injects the macOS `dynamic_lookup` linker flags — for a napi-CLI-free
//! dev/IDE loop.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use napi::bindgen_prelude::{AsyncTask, Buffer};
use napi::threadsafe_function::{
    ErrorStrategy, ThreadSafeCallContext, ThreadsafeFunction, ThreadsafeFunctionCallMode,
};
use napi::{Env, JsFunction, Task};
use napi_derive::napi;

/// Bound on the live-event [`ThreadsafeFunction`] queue (NIT: backpressure). In napi `0` means an
/// UNLIMITED queue; a positive bound caps buffered events if a subscriber's JS callback stalls
/// (excess events are dropped by the NonBlocking `.call()` rather than growing memory unbounded).
/// Sized well above any single run's event count so a normal stream is never truncated.
const EVENT_QUEUE_BOUND: usize = 1024;

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
        // Campaign DAG scheduler (DES-CAMPAIGN-001). Additive tagged-JSON mappings — the studio
        // ignores unknown event types, so these never disturb existing consumers. The full campaign
        // binding surface (launchCampaign etc.) is a separate follow-on task.
        CoreEvent::CampaignLaunched { campaign } => {
            json!({ "type": "campaignLaunched", "campaign": campaign })
        }
        CoreEvent::CampaignNodeReady { campaign, node } => {
            json!({ "type": "campaignNodeReady", "campaign": campaign, "node": node })
        }
        CoreEvent::CampaignNodeStarted {
            campaign,
            node,
            run_id,
        } => json!({ "type": "campaignNodeStarted", "campaign": campaign, "node": node, "runId": run_id }),
        CoreEvent::CampaignNodeAwaitingHuman {
            campaign,
            node,
            run_id,
            prompt,
        } => json!({ "type": "campaignNodeAwaitingHuman", "campaign": campaign, "node": node, "runId": run_id, "prompt": prompt }),
        CoreEvent::CampaignNodeCompleted { campaign, node } => {
            json!({ "type": "campaignNodeCompleted", "campaign": campaign, "node": node })
        }
        CoreEvent::CampaignNodeFailed { campaign, node } => {
            json!({ "type": "campaignNodeFailed", "campaign": campaign, "node": node })
        }
        CoreEvent::CampaignNodeBlocked { campaign, node } => {
            json!({ "type": "campaignNodeBlocked", "campaign": campaign, "node": node })
        }
        CoreEvent::CampaignPaused { campaign } => {
            json!({ "type": "campaignPaused", "campaign": campaign })
        }
        CoreEvent::CampaignCompleted { campaign } => {
            json!({ "type": "campaignCompleted", "campaign": campaign })
        }
        CoreEvent::CampaignFailed { campaign } => {
            json!({ "type": "campaignFailed", "campaign": campaign })
        }
        CoreEvent::CampaignCancelled { campaign } => {
            json!({ "type": "campaignCancelled", "campaign": campaign })
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
    /// A registered `WorkflowDef` id (`feature` | `bug` | `migration` or a drop-in). When set, planning
    /// is data-driven from the def's phases; omit for the free-text planner.
    pub workflow: Option<String>,
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
        workflow: o.workflow,
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

    /// Subscribe to the live [`CoreEvent`] stream. `callback` follows the Node error-first
    /// convention — `(err, eventJson)` — and is invoked once per event with the event serialized as
    /// a JSON string (`{ type, ...fields }`); parse it in JS. Events arrive in emission order. Call
    /// this BEFORE `launchRun` to catch the whole sequence, and HOLD the returned [`Subscription`]:
    /// `close()` / `unsubscribe()` stops delivery and tears the pump thread + callback down.
    ///
    /// NOTE (ordering): async methods resolve their Promise off a libuv worker thread, while events
    /// are delivered from a separate pump thread — so an event emitted by a call MAY be observed by
    /// this callback slightly AFTER that call's Promise resolves. Await the event you need (as the
    /// smoke does) rather than assuming it precedes the method's resolution.
    #[napi(ts_args_type = "callback: (err: Error | null, eventJson: string) => void")]
    pub fn subscribe(&self, env: Env, callback: JsFunction) -> napi::Result<Subscription> {
        // SIG-1 containment: in napi-rs 2.16 a *throw* inside a ThreadsafeFunction callback escalates
        // to `napi_fatal_exception` (→ uncaughtException → process death) under BOTH
        // `ErrorStrategy::Fatal` AND `CalleeHandled` — the `.call()` "Direct" variant routes a pending
        // exception through `handle_call_js_cb_status` regardless of strategy. So we wrap the user's
        // callback in a JS try/catch shim: the function we hand napi never throws, so a throwing
        // subscriber is contained (swallowed) instead of killing the process. `CalleeHandled` (used
        // for the tsfn + `.call(Ok(..))` below) additionally routes value-conversion failures to the
        // callback's `err` argument instead of aborting.
        let factory: JsFunction =
            env.run_script("(function(cb){return function(err,v){try{cb(err,v)}catch(_e){}}})")?;
        let wrapped: JsFunction = factory.call(None, &[callback])?.try_into()?;

        let mut tsfn: ThreadsafeFunction<String, ErrorStrategy::CalleeHandled> = wrapped
            .create_threadsafe_function(EVENT_QUEUE_BOUND, |ctx: ThreadSafeCallContext<String>| {
                Ok(vec![ctx.value])
            })?;
        // SIG-2: unref the tsfn (via the Env) so the pump does NOT hold the libuv loop open — a normal
        // `main()` return lets Node exit on its own, no `process.exit()` needed. `unref` acts on the
        // shared handle, so the pump thread's clone is unref'd too.
        tsfn.unref(&env)?;

        let stop = Arc::new(AtomicBool::new(false));
        let rx = self.inner.subscribe();
        let pump_tsfn = tsfn.clone();
        let pump_stop = stop.clone();
        // SIG-3: one dedicated FIFO pump thread. `recv_timeout` lets it observe the stop flag and
        // exit cleanly on `close()` (instead of blocking on `recv` forever); it also ends when the
        // actor drops the sender (last Core handle gone). On exit it drops `rx`, so the actor prunes
        // this subscriber on its next emit (retain-on-send) — re-subscribing never leaves a second
        // live pump or a duplicated stream.
        let join = std::thread::spawn(move || loop {
            if pump_stop.load(Ordering::SeqCst) {
                break;
            }
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(ev) => {
                    let json = event_to_json(&ev).to_string();
                    let _ = pump_tsfn.call(Ok(json), ThreadsafeFunctionCallMode::NonBlocking);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        });

        Ok(Subscription {
            stop,
            join: Mutex::new(Some(join)),
            tsfn: Mutex::new(Some(tsfn)),
        })
    }

    /// Liveness probe — emits a `Heartbeat` to subscribers and resolves once the actor acks (`"ok"`).
    #[napi(ts_return_type = "Promise<string>")]
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
    #[napi(ts_return_type = "Promise<string>")]
    pub fn launch_run(&self, opts: LaunchOptions) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let spec = build_spec(opts)?;
            core.launch_run(spec).map_err(err)
        })
    }

    /// Resume an interactive run from its persisted cursor (after a pause, crash, or fresh process).
    /// Resolves to the resulting status token.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn resume_run(&self, run_id: String) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || core.resume_run(&run_id).map(status_token).map_err(err))
    }

    /// Resolve a human-confirm gate on a PAUSED run. `approve=true` proceeds (optionally applying
    /// `amend` to the next unit's instruction); `approve=false` rejects → cancels the run. Resolves
    /// to the resulting status token. Rejects if the run is not paused at a gate.
    #[napi(ts_return_type = "Promise<string>")]
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
    #[napi(ts_return_type = "Promise<string>")]
    pub fn cancel_run(&self, run_id: String) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || core.cancel_run(&run_id).map(status_token).map_err(err))
    }

    // NOTE: there is intentionally no `pauseRun`. wicked-core has no imperative pause — a run pauses
    // ONLY at a declared human-confirm gate (set `humanConfirm` to `all` / `before:<ord>` at launch).
    // Exposing a fake `pauseRun` would misrepresent the engine, so it is omitted (see the report).

    /// The agent session ids currently on the store, as a JSON array of strings.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn sessions(&self) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let ids = core.sessions().map_err(err)?;
            serde_json::to_string(&ids).map_err(err)
        })
    }

    /// Every session + its ordered units, as a JSON array of `{ session, units }` objects (the read
    /// a UI builds its project list from).
    #[napi(ts_return_type = "Promise<string>")]
    pub fn sessions_detail(&self) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let views = core.sessions_detail().map_err(err)?;
            // NIT: surface a serialize failure as a napi error instead of silently substituting
            // `null` (which would hand the UI a malformed row it can't distinguish from real data).
            let mut arr: Vec<serde_json::Value> = Vec::with_capacity(views.len());
            for v in &views {
                arr.push(serde_json::json!({
                    "session": serde_json::to_value(&v.session).map_err(err)?,
                    "units": serde_json::to_value(&v.units).map_err(err)?,
                }));
            }
            serde_json::to_string(&arr).map_err(err)
        })
    }

    /// A unit's captured work output (transcript), as a JSON value — a string, or `null` if none.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn work_output(&self, unit_id: String) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let out = core.work_output(&unit_id);
            serde_json::to_string(&out).map_err(err)
        })
    }

    /// Register a git repository the orchestrator can run within. Validates it is a git repo with
    /// ≥1 commit; resolves to the persisted `RepoEntry` as a JSON object.
    #[napi(ts_return_type = "Promise<string>")]
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
    #[napi(ts_return_type = "Promise<string>")]
    pub fn list_repos(&self) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let repos = core.list_repos().map_err(err)?;
            serde_json::to_string(&repos).map_err(err)
        })
    }

    // ── PTY terminal sessions (DES-TERMINAL-001) ────────────────────────────────
    // Each method runs its (potentially blocking) Core call on a libuv worker thread via the SAME
    // `CoreTask`/`AsyncTask` pattern as every other method — the Node event loop is never blocked on
    // PTY open/write/resize/close. Terminal *events* (`terminalOpened` / `terminalOutput` with a
    // base64 `bytesB64` / `terminalExited`) arrive on the `subscribe` stream; `subscribe()` BEFORE
    // `openTerminal` to catch the whole sequence.

    /// Open a PTY terminal session running `cmd` (or the login shell if omitted) in `cwd`, sized
    /// `cols`x`rows`. `governed=false` is a loud, opt-in UNGOVERNED operator shell that bypasses the
    /// gate-hook (DES §7); pass `true` for the governed default. Resolves the new terminal id. Output
    /// arrives as `terminalOutput` events, so `subscribe()` FIRST to catch `terminalOpened` + bytes.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn open_terminal(
        &self,
        cwd: String,
        cmd: Option<Vec<String>>,
        cols: u16,
        rows: u16,
        governed: bool,
    ) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            core.open_terminal(cwd, cmd, cols, rows, governed)
                .map_err(err)
        })
    }

    /// Write raw input bytes (keystrokes) to a terminal. The `bytes` Buffer is copied to an owned
    /// `Vec<u8>` on the Node thread (a cheap memcpy — keystroke payloads are tiny) so the blocking
    /// write can run off-thread without moving a JS-owned Buffer across threads. Resolves `"ok"`;
    /// rejects if the terminal id is unknown or the write fails.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn write_terminal(&self, id: String, bytes: Buffer) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        let data: Vec<u8> = bytes.to_vec();
        task(move || {
            core.write_terminal(&id, &data).map_err(err)?;
            Ok("ok".to_string())
        })
    }

    /// Resize a terminal's PTY to `cols`x`rows`. Resolves `"ok"`; rejects if the terminal id is
    /// unknown or the resize fails.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn resize_terminal(&self, id: String, cols: u16, rows: u16) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            core.resize_terminal(&id, cols, rows).map_err(err)?;
            Ok("ok".to_string())
        })
    }

    /// Close a terminal: the actor kills the child, joins the reader thread, and drops the registry +
    /// I/O entries (no orphaned process/thread). Resolves `"ok"` once teardown completes; a
    /// `terminalExited` event is emitted. Rejects on an unknown id.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn close_terminal(&self, id: String) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            core.close_terminal(&id).map_err(err)?;
            Ok("ok".to_string())
        })
    }
}

// ── the live-event subscription handle ─────────────────────────────────────────

/// A live event subscription returned by [`Core::subscribe`]. Owns the FIFO pump thread + its
/// [`ThreadsafeFunction`]. `close()` / `unsubscribe()` tears both down deterministically (set the
/// stop flag → join the pump, which drops the event `Receiver` so the actor prunes its sender →
/// abort the tsfn). Idempotent; dropping the JS handle without an explicit close also stops the pump.
#[napi]
pub struct Subscription {
    stop: Arc<AtomicBool>,
    join: Mutex<Option<JoinHandle<()>>>,
    tsfn: Mutex<Option<ThreadsafeFunction<String, ErrorStrategy::CalleeHandled>>>,
}

#[napi]
impl Subscription {
    /// Stop delivering events and release the pump thread + `ThreadsafeFunction`. Idempotent and
    /// safe on a normal shutdown path; after it returns the pump is joined and the tsfn aborted, so
    /// the callback will not fire again. This is the teardown that makes re-subscribe leak-free and
    /// lets a plain `main()` return promptly.
    #[napi]
    pub fn close(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.join.lock().ok().and_then(|mut g| g.take()) {
            let _ = handle.join();
        }
        if let Some(tsfn) = self.tsfn.lock().ok().and_then(|mut g| g.take()) {
            // abort() releases with `abort` mode + flips the shared `aborted` flag, so any call the
            // pump had queued/attempts is a no-op (never a use-after-free on env teardown).
            let _ = tsfn.abort();
        }
    }

    /// Alias for [`Subscription::close`].
    #[napi]
    pub fn unsubscribe(&self) {
        self.close();
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        // If the JS handle is GC'd without an explicit close(), still signal the pump to stop so it
        // can't run (and re-deliver) forever. Detach rather than join — Drop may run on the JS
        // thread, and the pump exits within one `recv_timeout` tick, dropping its `Receiver` + tsfn
        // clone (whose Drop then releases the tsfn, as it was never aborted).
        self.stop.store(true, Ordering::SeqCst);
    }
}
