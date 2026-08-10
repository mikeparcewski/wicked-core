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

use wicked_core::{
    CoreEvent, EntityMode, HumanConfirm, HumanDecision, LaunchSpec, RepoSpec, SessionStatus,
    StubStepRunner,
};
use wicked_council::types::{Confidence, CouncilTask, Dispatcher, Vote};
use wicked_council::AgenticCli;

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

// Human-confirm parsing lives in wicked_core::HumanConfirm::parse — the ONE parser every entry point
// shares, so the napi layer cannot disagree with the bus/CLI/HTTP paths (FINDING-019).

/// Serialize one [`CoreEvent`] to its tagged JSON object for the JS callback.
///
/// The mapping itself lives in core ([`CoreEvent::to_json`]) so the `/ws` stream and core's durable
/// event log speak ONE vocabulary (FINDING-014) — when it lived here, core could not name its own
/// events and the daemon invented substitutes. Kept as a named wrapper because the tests below pin
/// the shape through this seam.
fn event_to_json(ev: &CoreEvent) -> serde_json::Value {
    ev.to_json()
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
        // The ONE canonical parser (FINDING-019): fail CLOSED on a typo instead of the old
        // silent downgrade to None, so a JS caller that mistypes humanConfirm gets a thrown error,
        // not an unattended run.
        human_confirm: HumanConfirm::parse(o.human_confirm.as_deref()).map_err(err)?,
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
    /// The estate db path — held so governance read methods can open a read-only connection
    /// without going through the single-writer actor (crew#40).
    db_path: String,
}

#[napi]
impl Core {
    /// Spawn the store actor over the estate db at `path` with the PRODUCTION engine (real council
    /// dispatcher + real wrapped-CLI step runner — runs actual agentic CLIs). The actor lives until
    /// every handle is dropped.
    #[napi(factory)]
    pub fn spawn(path: String) -> Core {
        Core {
            inner: wicked_core::Core::spawn(path.clone()),
            db_path: path,
        }
    }

    /// Spawn the store actor with the STUB engine — a deterministic council dispatcher +
    /// `StubStepRunner`, no subprocesses. For tests / offline runs that must not touch a real LLM.
    #[napi(factory)]
    pub fn spawn_stub(path: String) -> Core {
        let dispatcher: Arc<dyn Dispatcher + Send + Sync> = Arc::new(StubDispatcher);
        let runner = Arc::new(StubStepRunner);
        Core {
            inner: wicked_core::Core::spawn_with_engine(path.clone(), dispatcher, runner),
            db_path: path,
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
            .create_threadsafe_function(
                EVENT_QUEUE_BOUND,
                |ctx: ThreadSafeCallContext<String>| Ok(vec![ctx.value]),
            )?;
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

    /// Open a chat: eagerly warm one ACP session per seat (crew#165 / core#13). Resolves to a
    /// JSON array of per-seat outcomes `[{cliKey, ok, error?}]`; `chatSessionReady`/`chatSessionFailed`
    /// also stream to subscribers. Blocking handshakes run on the task pool, not the JS thread.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn chat_open(
        &self,
        chat_id: String,
        clis_json: String,
        cwd: Option<String>,
    ) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let clis: Vec<String> = serde_json::from_str(&clis_json).map_err(err)?;
            let outcomes = core
                .chat_open(&chat_id, &clis, cwd.map(std::path::PathBuf::from))
                .map_err(err)?;
            let arr: Vec<serde_json::Value> = outcomes
                .into_iter()
                .map(|(cli, r)| match r {
                    Ok(()) => serde_json::json!({ "cliKey": cli, "ok": true }),
                    Err(e) => serde_json::json!({ "cliKey": cli, "ok": false, "error": e }),
                })
                .collect();
            serde_json::to_string(&arr).map_err(err)
        })
    }

    /// Fan a message out to the chat's warm seats (all, or `targets_json` subset). Ack-fast:
    /// resolves to the JSON array of seats targeted; replies stream as `chatDelta`/`chatReply`.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn chat_send(
        &self,
        chat_id: String,
        text: String,
        targets_json: Option<String>,
        cwd: Option<String>,
    ) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let targets: Option<Vec<String>> = match targets_json {
                Some(t) => Some(serde_json::from_str(&t).map_err(err)?),
                None => None,
            };
            let seats = core
                .chat_send(&chat_id, &text, targets, cwd.map(std::path::PathBuf::from))
                .map_err(err)?;
            serde_json::to_string(&seats).map_err(err)
        })
    }

    /// The seats currently warm for a chat — JSON array of cli keys.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn chat_seats(&self, chat_id: String) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let seats = core.chat_seats(&chat_id).map_err(err)?;
            serde_json::to_string(&seats).map_err(err)
        })
    }

    /// Every chat currently holding pool state — JSON array of
    /// `[{chatId, seats, idleSecs}]`, sorted by id.
    ///
    /// Each warm seat pins an ACP bridge plus an agent child (~520 MB resident) and clients mint
    /// chat ids freely, so without this an accumulation is invisible until the host runs out of
    /// memory (FINDING-027). `idleSecs` is seconds since the chat's last open/ensure/turn, or
    /// `null` when no activity was ever recorded — which the reaper treats as idle-since-forever.
    ///
    /// `null` rather than the `u64::MAX` the Rust side uses for that case: a JS `number` is an
    /// f64, so `u64::MAX` arrives as `18446744073709552000` and no consumer can test for the
    /// sentinel by equality. `null` is checkable, and it stops a caller from doing arithmetic on a
    /// value that never meant a duration.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn chat_list(&self) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let chats = core.chat_list().map_err(err)?;
            let arr: Vec<serde_json::Value> = chats
                .into_iter()
                .map(|c| {
                    serde_json::json!({
                        "chatId": c.chat_id,
                        "seats": c.seats,
                        "idleSecs": (c.idle_secs != u64::MAX).then_some(c.idle_secs),
                    })
                })
                .collect();
            serde_json::to_string(&arr).map_err(err)
        })
    }

    /// Close a chat's warm sessions (idempotent); emits
    /// `chatClosed` with `reason: "requested"`.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn chat_close(&self, chat_id: String) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            core.chat_close(&chat_id).map_err(err)?;
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

    /// A run's recorded event history, oldest first, as a JSON array. Each entry is the SAME tagged
    /// object the `/ws` stream carries ([`CoreEvent::to_json`]) plus a capture-time `ts` (epoch millis)
    /// and an ordering `seq`.
    ///
    /// The read half of FINDING-014: an evidence bundle assembled after a run must read what actually
    /// happened rather than re-derive pseudo-events from unit records, which cannot recover what it
    /// never saw and invents its own type names doing it. Because the log and the socket serialize
    /// through one mapping, an event named here is the event named live.
    ///
    /// Empty array for an unknown run, one that emitted nothing, or one predating the log — an absent
    /// history is not an error. Streaming chunk events (`cliOutputDelta`, `chatDelta`,
    /// `terminalOutput`, `heartbeat`) are excluded by design.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn run_events(&self, run_id: String) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let events = core.run_events(&run_id);
            serde_json::to_string(&events).map_err(err)
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

    // ── Governance reads (crew#40) ──────────────────────────────────────────────
    // Each method opens a fresh READ-ONLY connection (open_store_ro) so it never
    // races with the single-writer actor. The actor continues to hold the one writable
    // handle; readonly connections are safe to open concurrently on SQLite WAL mode.

    /// All registered governance policies, as a JSON array of `Policy` objects.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn list_policies(&self) -> AsyncTask<CoreTask> {
        let db_path = self.db_path.clone();
        task(move || {
            use wicked_apps_core::{open_store_ro, FromNode, GraphRead, NodeKind};
            use wicked_estate_core::SymbolQuery;
            use wicked_governance::Policy;
            let store = open_store_ro(Some(db_path.as_str())).map_err(err)?;
            let query = SymbolQuery {
                kinds: vec![NodeKind::Other("policy".to_string())],
                ..Default::default()
            };
            let mut policies: Vec<Policy> = Vec::new();
            for node in store.find_symbols(&query).map_err(err)? {
                match Policy::from_node(&node) {
                    Ok(p) => policies.push(p),
                    Err(e) => eprintln!(
                        "wicked-core-ts: policy node '{}' failed to parse: {e}",
                        node.symbol
                    ),
                }
            }
            policies.sort_by(|a, b| a.id.cmp(&b.id));
            serde_json::to_string(&policies).map_err(err)
        })
    }

    /// All conformance rules on the store (Pattern + Policy types), as a JSON array.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn list_conformance_rules(&self) -> AsyncTask<CoreTask> {
        let db_path = self.db_path.clone();
        task(move || {
            use wicked_apps_core::open_store_ro;
            use wicked_governance::{recall_rules, RuleQuery};
            let store = open_store_ro(Some(db_path.as_str())).map_err(err)?;
            let rules = recall_rules(&store, &RuleQuery::default()).map_err(err)?;
            serde_json::to_string(&rules).map_err(err)
        })
    }

    /// All conformance claims (governance decisions) on the store, as a JSON array.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn list_conformance_claims(&self) -> AsyncTask<CoreTask> {
        let db_path = self.db_path.clone();
        task(move || {
            use wicked_apps_core::{
                open_store_ro, ConformanceClaim, GraphRead, NodeKind, CONFORMANCE_CLAIM,
            };
            use wicked_estate_core::SymbolQuery;
            use wicked_governance::claim_from_node;
            let store = open_store_ro(Some(db_path.as_str())).map_err(err)?;
            let query = SymbolQuery {
                kinds: vec![NodeKind::Other(CONFORMANCE_CLAIM.to_string())],
                ..Default::default()
            };
            let mut claims: Vec<ConformanceClaim> = Vec::new();
            for node in store.find_symbols(&query).map_err(err)? {
                match claim_from_node(&node) {
                    Ok(c) => claims.push(c),
                    Err(e) => eprintln!(
                        "wicked-core-ts: claim node '{}' failed to parse: {e}",
                        node.symbol
                    ),
                }
            }
            serde_json::to_string(&claims).map_err(err)
        })
    }

    // ── Governance writes (crew#42) ─────────────────────────────────────────────

    /// Upsert a governance policy. `policy_json` is a JSON-serialized `Policy` object
    /// (fields: id, kind, applies_to, effect, trigger, severity, criteria, rule, obligations).
    /// Validates server-side (fails closed on deny_unknown_fields + required fields). Idempotent
    /// on stable id — calling twice with the same id and payload is a no-op.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn upsert_policy(&self, policy_json: String) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            core.upsert_policy(policy_json).map_err(err)?;
            Ok(String::new())
        })
    }

    /// Upsert a conformance rule. `rule_json` is a JSON-serialized `ConformanceRule` object
    /// (fields: id, rule_type, statement, severity, confidence, targets, provenance).
    /// Validates server-side (INV-C1/C2/C4). Idempotent on stable id.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn upsert_conformance_rule(&self, rule_json: String) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            core.upsert_conformance_rule(rule_json).map_err(err)?;
            Ok(String::new())
        })
    }

    /// Withdraw a governance policy from enforcement (FINDING-038 — governance state was otherwise
    /// append-only, so a mis-authored policy denied forever).
    ///
    /// Retire, not delete: the node stays readable so a past decision citing this id can still be
    /// explained, but SELECT stops returning it, so it can never decide another gate.
    ///
    /// Resolves to a JSON-encoded boolean: the four characters `true` if a policy with that id
    /// existed, the five characters `false` if none did. Like every method here it hands JS a
    /// `Promise<string>` carrying JSON, so it must be `JSON.parse`d — a bare truthiness test
    /// passes on BOTH values and would read a miss as a hit, losing exactly the 200-vs-404
    /// distinction this return value exists to carry.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn retire_policy(&self, id: String) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let found = core.retire_policy(&id).map_err(err)?;
            serde_json::to_string(&found).map_err(err)
        })
    }

    /// Withdraw a conformance rule from recall. Same retire-not-delete contract as
    /// [`Core::retire_policy`], and the same JSON-encoded `true`/`false` reply that must be
    /// parsed rather than tested for truthiness.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn retire_conformance_rule(&self, id: String) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let found = core.retire_conformance_rule(&id).map_err(err)?;
            serde_json::to_string(&found).map_err(err)
        })
    }

    /// Register (or replace) a workflow definition in the actor's runtime registry. `json` is a
    /// JSON-serialised `WorkflowDef` object (fields: id, description, phases — see the wicked-core
    /// workflow schema). Validates server-side (id + ≥1 phase required); rejects invalid JSON or a
    /// structurally invalid def. Returns the registered workflow id. Idempotent on id — calling
    /// twice replaces the first registration. The def is immediately visible to the next `launchRun`
    /// call; no process restart required.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn register_workflow(&self, json: String) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || core.register_workflow(json).map_err(err))
    }

    /// Recall which conformance rules apply to the given `query_json` (a JSON-serialized
    /// `RuleQuery` — fields: language, layer, framework, severity, rule_type; all optional).
    /// An empty or whitespace `query_json` is treated as an all-rules query (no facet filters).
    /// Opens a read-only connection — does not block the single-writer actor. Returns a JSON
    /// array of `ConformanceRule` objects, severity-first then id.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn recall_rules_preview(&self, query_json: String) -> AsyncTask<CoreTask> {
        let db_path = self.db_path.clone();
        task(move || {
            use wicked_apps_core::open_store_ro;
            use wicked_governance::{recall_rules, RuleQuery};
            let store = open_store_ro(Some(db_path.as_str())).map_err(err)?;
            let query: RuleQuery = if query_json.trim().is_empty() {
                RuleQuery::default()
            } else {
                serde_json::from_str(&query_json).map_err(err)?
            };
            let rules = recall_rules(&store, &query).map_err(err)?;
            serde_json::to_string(&rules).map_err(err)
        })
    }

    /// Front-half coverage gate report — JSON-serialized `CoverageReport`, or the JSON literal
    /// `null` when the store has no domain-model nodes yet. Opens a read-only connection so it
    /// never blocks the single-writer actor.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn get_coverage_report(&self) -> AsyncTask<CoreTask> {
        let db_path = self.db_path.clone();
        task(move || {
            use wicked_apps_core::open_store_ro;
            use wicked_governance::recompute_front_half_coverage;
            let store = open_store_ro(Some(db_path.as_str())).map_err(err)?;
            match recompute_front_half_coverage(&store) {
                Ok(report) => serde_json::to_string(&report).map_err(err),
                Err(e) => {
                    eprintln!("wicked-core-ts: getCoverageReport failed: {e}");
                    Ok("null".to_string())
                }
            }
        })
    }

    /// Coverage for ONE registered repo, computed over that repo's OWN code graph — not the daemon's
    /// bookkeeping store (FINDING-009). `get_coverage_report` above reads `self.db_path` (the daemon
    /// `core.db`), which holds run/governance nodes but none of a repo's domain/requirement nodes, so
    /// it reports a vacuous `coverage: 1.0` over an empty denominator and cannot name a repo. This
    /// resolves the repo from the registry, opens its `code_graph_db` (`<root>/.codegraph/estate.db`,
    /// the one spelling every consumer shares), and recomputes over it. An unknown `repo_ref` is an
    /// ERROR, never a silent vacuous report — the caller must name a real repo.
    #[napi]
    pub fn get_coverage_report_for_repo(&self, repo_ref: String) -> AsyncTask<CoreTask> {
        let db_path = self.db_path.clone();
        task(move || {
            use wicked_apps_core::open_store_ro;
            let daemon = open_store_ro(Some(db_path.as_str())).map_err(err)?;
            // The resolve-repo → open-its-store → recompute logic lives in wicked-core so it is
            // unit-testable there (this napi layer stays thin glue). An unknown repo errors.
            let report = wicked_core::coverage_report_for_repo(&daemon, &repo_ref).map_err(err)?;
            serde_json::to_string(&report).map_err(err)
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

    /// Inject an operator message into one or all active PTY workers for a run.
    ///
    /// `target` is either `"all"` (write to every PTY session for the run) or a CLI key string
    /// (write only to that CLI's session). ACP-backed sessions have no PTY and are skipped with a
    /// warning. Fires [`CoreEvent::WorkerMessageInjected`] for each successful write.
    #[napi]
    pub fn inject_worker_message(
        &self,
        run_id: String,
        message: String,
        target: String,
    ) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            let t = if target == "all" {
                wicked_core::InjectTarget::All
            } else {
                wicked_core::InjectTarget::Cli(target)
            };
            core.inject_worker_message(&run_id, &message, t)
                .map_err(err)?;
            Ok("ok".to_string())
        })
    }

    /// Stop the current worker for `ord` in run `run_id` and re-dispatch it.
    ///
    /// `newCli` is either a CLI key string (re-dispatch immediately to that CLI) or `null` (re-run
    /// the council and let it pick). Returns `"ok"` when the command has been queued; the
    /// [`CoreEvent::UnitReassigned`] event confirms the reassignment.
    #[napi]
    pub fn reassign_unit(
        &self,
        run_id: String,
        ord: u32,
        new_cli: Option<String>,
    ) -> AsyncTask<CoreTask> {
        let core = self.inner.clone();
        task(move || {
            core.reassign_unit(&run_id, ord, new_cli).map_err(err)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    // Only the tests name a failure kind now that the CoreEvent → JSON mapping lives in core.
    use serde_json::Value;
    use wicked_core::StepFailureKind;

    /// Assert one hand-mapped variant: its `type` tag and the EXACT set of JSON keys it emits. This
    /// pins the hand-written `CoreEvent → JSON` mapping (`event_to_json` — the studio's only view of
    /// the stream) so a renamed key or a wrong tag fails CI instead of silently drifting.
    fn check(ev: CoreEvent, expected_type: &str, expected_keys: &[&str]) {
        let v: Value = event_to_json(&ev);
        let obj = v.as_object().expect("mapping emits a JSON object");
        assert_eq!(
            obj.get("type").and_then(Value::as_str),
            Some(expected_type),
            "wrong type tag for {expected_type}"
        );
        let mut got: Vec<&str> = obj.keys().map(String::as_str).collect();
        got.sort_unstable();
        let mut want: Vec<&str> = expected_keys.to_vec();
        want.sort_unstable();
        assert_eq!(got, want, "key set drift for {expected_type}");
    }

    /// Pin EVERY mapped `CoreEvent` variant's tag (camelCase) + exact key set. Because `CoreEvent` is
    /// `#[non_exhaustive]`, a NEW variant no longer breaks the build (the defensive `_` arm catches
    /// it) — so this test is the tripwire: a new variant with no explicit arm falls through to
    /// `{"type":"unknown"}` and any missing pin here signals the arm was never added.
    #[test]
    fn every_mapped_variant_has_stable_tag_and_keys() {
        let s = || "s".to_string();
        check(CoreEvent::Heartbeat, "heartbeat", &["type"]);
        check(
            CoreEvent::ChatSessionReady {
                chat: "c".into(),
                cli_key: "claude".into(),
            },
            "chatSessionReady",
            &["type", "chat", "cliKey"],
        );
        check(
            CoreEvent::ChatSessionFailed {
                chat: "c".into(),
                cli_key: "claude".into(),
                reason: "boom".into(),
            },
            "chatSessionFailed",
            &["type", "chat", "cliKey", "reason"],
        );
        check(
            CoreEvent::ChatDelta {
                chat: "c".into(),
                cli_key: "claude".into(),
                text: "t".into(),
            },
            "chatDelta",
            &["type", "chat", "cliKey", "text"],
        );
        check(
            CoreEvent::ChatReply {
                chat: "c".into(),
                cli_key: "claude".into(),
                text: "t".into(),
                ok: true,
            },
            "chatReply",
            &["type", "chat", "cliKey", "text", "ok"],
        );
        check(
            CoreEvent::ChatClosed {
                chat: "c".into(),
                reason: "idle".into(),
            },
            "chatClosed",
            &["type", "chat", "reason"],
        );
        check(
            CoreEvent::SessionStarted {
                session: s(),
                problem: s(),
                workflow_id: None,
                cli_count: 1,
                governed: false,
                entity_mode: s(),
            },
            "sessionStarted",
            &[
                "type",
                "session",
                "problem",
                "workflowId",
                "cliCount",
                "governed",
                "entityMode",
            ],
        );
        check(
            CoreEvent::UnitPlanned {
                session: s(),
                ord: 1,
                description: s(),
                stage: s(),
                role: s(),
                gate: s(),
                skill_ref: None,
                has_validator_pin: false,
                executor_type: s(),
            },
            "unitPlanned",
            &[
                "type",
                "session",
                "ord",
                "description",
                "stage",
                "role",
                "gate",
                "skillRef",
                "hasValidatorPin",
                "executorType",
            ],
        );
        check(
            CoreEvent::UnitDistributed {
                session: s(),
                ord: 1,
                cli: s(),
                routing_method: s(),
                agreement_pct: None,
                returned: None,
                seated: None,
                dissent: None,
                degraded_reason: None,
            },
            "unitDistributed",
            &[
                "type",
                "session",
                "ord",
                "cli",
                "routingMethod",
                "agreementPct",
                "returned",
                // The quorum denominator `returned` must be read against. Emitted unconditionally
                // (null when unknown) so a consumer never has to guess whether its absence means
                // "one-seat council" or "field not sent" (FINDING-026 D).
                "seated",
                "dissent",
                "degradedReason",
            ],
        );
        check(
            CoreEvent::UnitExecuting {
                session: s(),
                ord: 1,
            },
            "unitExecuting",
            &["type", "session", "ord"],
        );
        check(
            CoreEvent::CliOutputDelta {
                session: s(),
                ord: 1,
                chunk: s(),
            },
            "cliOutputDelta",
            &["type", "session", "ord", "chunk"],
        );
        check(
            CoreEvent::GateDecided {
                session: s(),
                ord: 1,
                allow: true,
            },
            "gateDecided",
            &["type", "session", "ord", "allow"],
        );
        // ── DES-STUDIO-COCKPIT-001 §3 B-events (the 4 new insight variants) ──
        check(
            CoreEvent::GateEvaluated {
                session: s(),
                ord: 1,
                criterion: Some(s()),
                has_deterministic_floor: true,
                deterministic_pass: true,
                agent_verdict: Some(s()),
                agent_reasoning: Some(s()),
                evaluator_pass: Some(true),
                evaluator_policies: vec![s()],
                denial_reason: None,
                combined: true,
            },
            "gateEvaluated",
            &[
                "type",
                "session",
                "ord",
                "criterion",
                "hasDeterministicFloor",
                "deterministicPass",
                "agentVerdict",
                "agentReasoning",
                "evaluatorPass",
                "evaluatorPolicies",
                "denialReason",
                "combined",
            ],
        );
        check(
            CoreEvent::UnitDispatched {
                session: s(),
                ord: 1,
                attempt: 0,
            },
            "unitDispatched",
            &["type", "session", "ord", "attempt"],
        );
        check(
            CoreEvent::CliUsage {
                session: s(),
                ord: 1,
                attempt: 0,
                input_tokens: 10,
                output_tokens: 20,
                cache_read_tokens: 6,
                cache_creation_tokens: 2,
                cost_usd: Some(0.4),
            },
            "cliUsage",
            &[
                "type",
                "session",
                "ord",
                "attempt",
                "inputTokens",
                "outputTokens",
                "cacheReadTokens",
                "cacheCreationTokens",
                "costUsd",
            ],
        );
        check(
            CoreEvent::DataUsed {
                session: s(),
                ord: 1,
                files: vec![s()],
            },
            "dataUsed",
            &["type", "session", "ord", "files"],
        );
        check(
            CoreEvent::ToolInvoked {
                session: s(),
                ord: 1,
                attempt: 0,
                tools: vec![s()],
            },
            "toolInvoked",
            &["type", "session", "ord", "attempt", "tools"],
        );
        // ── remaining variants ──
        check(
            CoreEvent::UnitDone {
                session: s(),
                ord: 1,
            },
            "unitDone",
            &["type", "session", "ord"],
        );
        check(
            CoreEvent::UnitDenied {
                session: s(),
                ord: 1,
            },
            "unitDenied",
            &["type", "session", "ord"],
        );
        check(
            CoreEvent::AwaitingHuman {
                session: s(),
                ord: 1,
                reviewing_ord: Some(1),
                prompt: s(),
            },
            "awaitingHuman",
            &["type", "session", "ord", "reviewingOrd", "prompt"],
        );
        check(
            CoreEvent::Resumed {
                session: s(),
                ord: 1,
            },
            "resumed",
            &["type", "session", "ord"],
        );
        check(
            CoreEvent::RunCancelled { session: s() },
            "runCancelled",
            &["type", "session"],
        );
        check(
            CoreEvent::SessionFailed {
                session: s(),
                ord: 1,
            },
            "sessionFailed",
            &["type", "session", "ord"],
        );
        check(
            CoreEvent::RepoRegistered { repo_ref: s() },
            "repoRegistered",
            &["type", "repoRef"],
        );
        check(
            CoreEvent::SessionCompleted { session: s() },
            "sessionCompleted",
            &["type", "session"],
        );
        check(
            CoreEvent::WorkerMessageInjected {
                session: s(),
                message: s(),
                target: s(),
            },
            "workerMessageInjected",
            &["type", "session", "message", "target"],
        );
        check(
            CoreEvent::AssumptionRecorded {
                session: s(),
                ord: 1,
                kind: s(),
                library: s(),
                transform: s(),
                known: true,
                detail: s(),
            },
            "assumptionRecorded",
            &[
                "type",
                "session",
                "ord",
                "kind",
                "library",
                "transform",
                "known",
                "detail",
            ],
        );
        check(
            CoreEvent::WorkerMessageQueued {
                session: s(),
                message: s(),
                target: s(),
            },
            "workerMessageQueued",
            &["type", "session", "message", "target"],
        );
        check(
            CoreEvent::UnitReassigned {
                session: s(),
                ord: 1,
                attempt: 1,
                previous_cli: s(),
                new_cli: Some(s()),
            },
            "unitReassigned",
            &["type", "session", "ord", "attempt", "previousCli", "newCli"],
        );
        check(
            CoreEvent::Error {
                session: Some(s()),
                message: s(),
            },
            "error",
            &["type", "session", "message"],
        );
        check(
            CoreEvent::TerminalOpened { id: s(), cwd: s() },
            "terminalOpened",
            &["type", "id", "cwd"],
        );
        check(
            CoreEvent::TerminalOutput {
                id: s(),
                seq: 7,
                bytes_b64: s(),
            },
            "terminalOutput",
            &["type", "id", "seq", "bytesB64"],
        );
        check(
            CoreEvent::TerminalExited {
                id: s(),
                status: Some(0),
            },
            "terminalExited",
            &["type", "id", "status"],
        );
        check(
            CoreEvent::CampaignLaunched { campaign: s() },
            "campaignLaunched",
            &["type", "campaign"],
        );
        check(
            CoreEvent::CampaignNodeReady {
                campaign: s(),
                node: s(),
            },
            "campaignNodeReady",
            &["type", "campaign", "node"],
        );
        check(
            CoreEvent::CampaignNodeStarted {
                campaign: s(),
                node: s(),
                run_id: s(),
            },
            "campaignNodeStarted",
            &["type", "campaign", "node", "runId"],
        );
        check(
            CoreEvent::CampaignNodeAwaitingHuman {
                campaign: s(),
                node: s(),
                run_id: s(),
                prompt: s(),
            },
            "campaignNodeAwaitingHuman",
            &["type", "campaign", "node", "runId", "prompt"],
        );
        check(
            CoreEvent::CampaignNodeCompleted {
                campaign: s(),
                node: s(),
            },
            "campaignNodeCompleted",
            &["type", "campaign", "node"],
        );
        check(
            CoreEvent::CampaignNodeFailed {
                campaign: s(),
                node: s(),
            },
            "campaignNodeFailed",
            &["type", "campaign", "node"],
        );
        check(
            CoreEvent::CampaignNodeBlocked {
                campaign: s(),
                node: s(),
            },
            "campaignNodeBlocked",
            &["type", "campaign", "node"],
        );
        check(
            CoreEvent::CampaignPaused { campaign: s() },
            "campaignPaused",
            &["type", "campaign"],
        );
        check(
            CoreEvent::CampaignCompleted { campaign: s() },
            "campaignCompleted",
            &["type", "campaign"],
        );
        check(
            CoreEvent::CampaignFailed { campaign: s() },
            "campaignFailed",
            &["type", "campaign"],
        );
        check(
            CoreEvent::CampaignCancelled { campaign: s() },
            "campaignCancelled",
            &["type", "campaign"],
        );
        check(
            CoreEvent::StepFailed {
                session: s(),
                ord: 1,
                attempt: 0,
                detail: s(),
                failure_kind: StepFailureKind::WorkerError,
            },
            "stepFailed",
            &["type", "session", "ord", "attempt", "detail", "failureKind"],
        );
        check(
            CoreEvent::CrashRecoveryRedrive {
                session: s(),
                ord: 1,
                attempt: 1,
            },
            "crashRecoveryRedrive",
            &["type", "session", "ord", "attempt"],
        );
        check(
            CoreEvent::WorkerSessionStarted {
                session: s(),
                terminal_id: s(),
                cli_key: s(),
            },
            "workerSessionStarted",
            &["type", "session", "terminalId", "cliKey"],
        );
        check(
            CoreEvent::AcpSessionStarted {
                session: s(),
                cli_key: s(),
                acp_session_id: s(),
            },
            "acpSessionStarted",
            &["type", "session", "cliKey", "acpSessionId"],
        );
        check(
            CoreEvent::AcpFallback {
                session: s(),
                cli_key: s(),
                reason: s(),
                fallback_kind: s(),
            },
            "acpFallback",
            &["type", "session", "cliKey", "reason", "fallbackKind"],
        );
        // P2 observability events — worker-lifecycle wave (EVT-003, EVT-004, EVT-007).
        check(
            CoreEvent::WorkerSessionReused {
                session: s(),
                terminal_id: s(),
                ord: 2,
            },
            "workerSessionReused",
            &["type", "session", "terminalId", "ord"],
        );
        check(
            CoreEvent::WorkerSessionClosed {
                session: s(),
                terminal_id: s(),
                reason: s(),
            },
            "workerSessionClosed",
            &["type", "session", "terminalId", "reason"],
        );
        check(
            CoreEvent::UnitContextInjected {
                session: s(),
                ord: 2,
                recipient_cli: s(),
                prior_units: vec![wicked_core::InjectedContext {
                    ord: 1,
                    label: s(),
                    output_bytes: 42,
                }],
            },
            "unitContextInjected",
            &["type", "session", "ord", "recipientCli", "priorUnits"],
        );
        // P2 governance-deep wave (EVT-008, EVT-009, EVT-010, EVT-011, EVT-016).
        check(
            CoreEvent::GovernanceHookFired {
                session: s(),
                ord: 1,
                attempt: 0,
                tool_name: s(),
                decision: "allow".to_string(),
                denying_policy: None,
            },
            "governanceHookFired",
            &[
                "type",
                "session",
                "ord",
                "attempt",
                "toolName",
                "decision",
                "denyingPolicy",
            ],
        );
        check(
            CoreEvent::ValidationPinAttached {
                session: s(),
                ord: 1,
                pin: s(),
                criterion: s(),
            },
            "validationPinAttached",
            &["type", "session", "ord", "pin", "criterion"],
        );
        check(
            CoreEvent::GateEscalated {
                session: s(),
                ord: 1,
                condition: "verdict_not_pass".to_string(),
                verdict_summary: s(),
            },
            "gateEscalated",
            &["type", "session", "ord", "condition", "verdictSummary"],
        );
        check(
            CoreEvent::ToolExecutorDispatched {
                session: s(),
                ord: 1,
                cmd: vec!["echo".to_string(), "hello".to_string()],
                workdir: None,
            },
            "toolExecutorDispatched",
            &["type", "session", "ord", "cmd", "workdir"],
        );
        check(
            CoreEvent::GovernanceContextArmed {
                session: s(),
                ord: 1,
                attempt: 0,
                path: "wrapped_cli".to_string(),
                db_path: s(),
            },
            "governanceContextArmed",
            &["type", "session", "ord", "attempt", "path", "dbPath"],
        );
        check(
            CoreEvent::GovernanceUnenforced {
                session: s(),
                ord: 4,
                attempt: 0,
                cli: s(),
                reason: s(),
            },
            "governanceUnenforced",
            &["type", "session", "ord", "attempt", "cli", "reason"],
        );
        // P2 decisions-full wave (EVT-001, EVT-012, EVT-013).
        check(
            CoreEvent::WorkflowSelected {
                session: s(),
                workflow_id: "feature".to_string(),
                unit_count: 2,
            },
            "workflowSelected",
            &["type", "session", "workflowId", "unitCount"],
        );
        check(
            CoreEvent::UnitReworkAmended {
                session: s(),
                ord: 1,
                amendment: "add error handling".to_string(),
                updated_description: s(),
            },
            "unitReworkAmended",
            &["type", "session", "ord", "amendment", "updatedDescription"],
        );
        check(
            CoreEvent::UnitOutputCaptured {
                session: s(),
                ord: 1,
                attempt: 0,
                output_bytes: 512,
                step_status: "ok".to_string(),
                governed: false,
            },
            "unitOutputCaptured",
            &[
                "type",
                "session",
                "ord",
                "attempt",
                "outputBytes",
                "stepStatus",
                "governed",
            ],
        );
    }
}
