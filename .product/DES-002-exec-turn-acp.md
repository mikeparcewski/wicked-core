# Purpose

`exec_turn_acp` — the turn loop, elicitation/create arm, dual-poll loop, Phase 1/2 write protocol, outer-loop bounded poll, and `exec_turn` match block.

Lock-poison recovery: `unwrap_or_else(|p| p.into_inner())` at every `Mutex::lock()` call — documented in DES-002-elicitation-maps.md, applied uniformly throughout.

---

## `rpc_respond` helper (src/acp_runner.rs)

```rust
/// Send a JSON-RPC 2.0 response frame for an inbound request.
/// Echoes `id` verbatim as Value — handles both numeric and string ids.
/// Generalized over W: Write so tests can pass Cursor<Vec<u8>> (F18).
fn rpc_respond<W: Write>(
    writer: &mut W,
    id:     &Value,
    result: Value,
) -> anyhow::Result<()> {
    let msg = json!({"jsonrpc": "2.0", "id": id, "result": result});
    writeln!(writer, "{msg}")?;
    writer.flush()?;
    Ok(())
}
```

`rpc_expect` is also generalized:
```rust
fn rpc_expect<W: Write>(
    rx:          &Receiver<String>,  // borrow — called twice on the same receiver
    id:          u64,
    timeout:     Duration,
    writer:      &mut W,
    write_lock:  &Arc<Mutex<()>>,
    kill_handle: &Arc<KillHandle>,
) -> anyhow::Result<Value>
```

Both callers in `start_acp_process` pass `&mut stdin` (local `BufWriter<ChildStdin>` — `AcpProcess` not yet constructed). See §Handshake-phase guard in DES-002-actor-teardown.md.

---

## `exec_turn_acp` signature

```rust
fn exec_turn_acp(
    proc:                &mut AcpProcess,
    prompt:              &str,
    prior_outputs:       &[PriorUnitOutput],
    emit:                &DeltaSink,
    timeout:             Duration,
    // ── new params ──
    run_id:              &str,
    elicitation_maps:    &Arc<Mutex<ElicitationMaps>>,
    emit_ev:             &dyn Fn(CoreEvent) -> bool,  // returns false if actor channel closed
    elicitation_enabled: bool,   // reflects form_enabled (global flag AND adapter allowlist)
    elicitation_shared:  bool,   // true ↔ runner's maps Arc == actor's maps Arc
    run_epoch:           u64,    // dispatch-time epoch; 0 = disabled
    mut epoch_guard:     Option<&mut EpochCleanup>,  // None when epoch=0
                                                      // MUST be `mut` — if let Some(ref mut g)
                                                      // requires mutably reborrowing the Option (E0596)
) -> anyhow::Result<TurnResult>
```

`emit_ev` closure in `AcpStepRunner::exec_turn`:
```rust
let tx = &self.tx;
let emit_ev = |ev: CoreEvent| -> bool {
    tx.send(Command::EmitEvent(ev)).is_ok()
};
```

### `elicitation_enabled` and `form_enabled`

`elicitation_enabled` reflects the per-session `form_enabled` decision computed in `start_acp_process` — global flag AND adapter allowlist check — not just whether the caller is `exec_turn` vs `chat_turn`.

```rust
const ELICITATION_VERIFIED_ADAPTERS: &[&str] = &["claude-agent-acp", "codex-acp"];
let form_enabled = elicitation_enabled_global_flag
    && ELICITATION_VERIFIED_ADAPTERS.contains(&adapter_name)
    && !is_chat_session;  // chat_turn rejects elicitation at runtime; don't advertise it
```

Pass `form_enabled` into both `exec_turn` and `exec_turn_acp`. When `WICKED_ELICITATION_ENABLED=false`, the adapter is not allowlisted, or the session is a chat session, the elicitation arm cancels inbound requests immediately — even if a misbehaving adapter sends `elicitation/create` despite the capability being omitted.

`elicitation_enabled=false` for `AcpStepRunner::chat_turn` until OQ-R-7 (chat run_id → session routing contract) is verified. The `!is_chat_session` gate in the capability advertisement ensures that even when the global flag is on and the adapter is allowlisted, chat sessions do not advertise `{"form":{}}` — eliminating spurious elicitation/create requests that would be immediately cancelled.

---

## Five new variables before the outer loop

```rust
let mut found                  = false;
let mut timed_out              = false;  // ordinary deadline / stopReason:cancelled
let mut elicitation_timed_out  = false;  // deadline or teardown within the 'elicit arm
let mut elicitation_teardown   = false;  // run-cancel/disconnect teardown (not deadline expiry)
let mut cancelled              = false;  // human explicitly dismissed the elicitation
let mut dead_session           = false;  // cancel-response write failed (broken stdin)
let mut prompt_done_path       = false;  // session/prompt won the elicitation race
let mut prompt_error           = false;  // session/prompt won but carried a JSON-RPC error
// NOTE: prompt_done_path and prompt_error are declared here (turn scope) so they remain
// accessible in the final status construction below the elicitation/create arm.
// prompt_error=true means prompt_done_path=true AND the prompt response was an error —
// final status must be Failed, not Ok (test 19 asserts this path).
```

---

## Outer-loop bounded poll

When `run_epoch > 0`, the outer turn loop uses a 500 ms bounded poll instead of the full remaining budget. This prevents a CancelRun that arrives between a one-shot tombstone check and `recv_timeout` from blocking the worker for the residual 7200-second budget (because `maps.remove()` already cleared the sender, so `cancel_epoch` cannot wake the receive).

```rust
const ELICITATION_OUTER_POLL_MS: u64 = 500;

'outer: loop {
    let remaining = deadline.checked_duration_since(Instant::now()).unwrap_or_default();
    if remaining.is_zero() { timed_out = true; break 'outer; }

    let poll = if run_epoch > 0 {
        remaining.min(Duration::from_millis(ELICITATION_OUTER_POLL_MS))
    } else {
        remaining
    };

    match line_rx.recv_timeout(poll) {
        Ok(line) => {
            // Check tombstone BEFORE dispatching this frame.
            // If CancelRun arrives while the adapter streams session/update faster than
            // the 500ms interval, recv_timeout always returns Ok and the Timeout arm never
            // fires. The per-frame check closes this window.
            if run_epoch > 0 {
                let maps = elicitation_maps.lock().unwrap_or_else(|p| p.into_inner());
                if maps.is_epoch_cancelled(run_id, run_epoch) {
                    elicitation_timed_out = true;
                    break 'outer;
                }
            }
            /* existing frame dispatch — elicitation/create arm, response arm */
        }
        Err(RecvTimeoutError::Timeout) => {
            if run_epoch > 0 {
                let maps = elicitation_maps.lock().unwrap_or_else(|p| p.into_inner());
                if maps.is_epoch_cancelled(run_id, run_epoch) {
                    elicitation_timed_out = true;
                    break 'outer;
                }
            }
            continue 'outer;
        }
        Err(RecvTimeoutError::Disconnected) => {
            // Adapter crash — check tombstone to distinguish from CancelRun teardown.
            if run_epoch > 0 {
                let maps = elicitation_maps.lock().unwrap_or_else(|p| p.into_inner());
                if maps.is_epoch_cancelled(run_id, run_epoch) {
                    elicitation_timed_out = true;
                    break 'outer;
                }
            }
            // Adapter crash with no tombstone → SESSION_DIED fallback_with_warning.
            break 'outer;
        }
    }
}
```

---

## Critical ordering: method guard on response branch

The `elicitation/create` arm must be checked before the `id`-match response branch. Add a `"method": null` guard to the response branch:

```rust
// Response to our session/prompt request — ONLY if no "method" field.
if v.get("method").is_none() && v.get("id").and_then(Value::as_u64) == Some(id) {
    // ... existing found/timed_out/usage logic ...
    break;
}
```

---

## Full elicitation/create arm

```rust
if v.get("method").and_then(Value::as_str) == Some("elicitation/create") {
    let elicitation_req_id = v["id"].clone();
    if elicitation_req_id.is_null() { continue; }  // malformed notification; ignore

    // ── FIRST: disabled/epoch-zero check ─────────────────────────────────────
    // Epoch 0 is treated identically to elicitation_enabled=false: legacy bus tasks
    // deserialized with epoch 0 have no active entry in ElicitationMaps.
    if !elicitation_enabled || run_epoch == 0 {
        // Return Ok(TurnResult { dead_session: true }) on write failure (transport fault).
        // 'elicit is not in scope here (declared below); use return not break.
        let write_ok = {
            let _wg = proc.write_lock.lock().unwrap_or_else(|p| p.into_inner());
            let watchdog = WriteWatchdog::new(Arc::clone(&proc.kill_handle), WRITE_WATCHDOG_MS);
            let r = rpc_respond(&mut proc.stdin, &elicitation_req_id, json!({"action": "cancel"}));
            let fired = watchdog.complete();
            !fired && r.is_ok()
        };
        if !write_ok {
            return Ok(TurnResult { dead_session: true, ..TurnResult::default_at(output, usage, files) });
        }
        continue;
    }

    // ── SECOND: release-build guard ───────────────────────────────────────────
    if !elicitation_shared {
        tracing::error!(run_id = %run_id,
            "BUG: elicitation arm reached on a non-shared runner; aborting");
        elicitation_timed_out = true;
        elicitation_teardown  = true;
        break;
    }

    // ── THIRD: validate params.mode ───────────────────────────────────────────
    let mode = v["params"]["mode"].as_str().unwrap_or("");
    if mode != "form" {
        tracing::warn!(req_id = ?elicitation_req_id, mode, "elicitation: unsupported mode; cancelling");
        let write_ok = {
            let _wg = proc.write_lock.lock().unwrap_or_else(|p| p.into_inner());
            let watchdog = WriteWatchdog::new(Arc::clone(&proc.kill_handle), WRITE_WATCHDOG_MS);
            let r = rpc_respond(&mut proc.stdin, &elicitation_req_id, json!({"action": "cancel"}));
            let fired = watchdog.complete();
            !fired && r.is_ok()
        };
        if !write_ok {
            return Ok(TurnResult { dead_session: true, ..TurnResult::default_at(output, usage, files) });
        }
        continue;
    }

    // ── Extract message ───────────────────────────────────────────────────────
    const MSG_CAP: usize = 8 * 1024;
    let raw_msg: &str = v["params"]["message"].as_str().unwrap_or("");
    if raw_msg.is_empty() {
        tracing::warn!(req_id = ?elicitation_req_id,
            "elicitation/create has empty message; params.message may be mis-pathed");
    }
    let message: String = if raw_msg.len() > MSG_CAP {
        let marker = "[truncated]";
        let cap = MSG_CAP - marker.len();
        let end = raw_msg.char_indices()
            .take_while(|(i, c)| *i + c.len_utf8() <= cap)
            .last().map(|(i, c)| i + c.len_utf8()).unwrap_or(0);
        format!("{}{marker}", &raw_msg[..end])
    } else {
        raw_msg.to_string()
    };

    // ── FOURTH: schema parsing ────────────────────────────────────────────────
    // Five cases: (a) single-property supported type → proceed; (b) unsupported type → cancel;
    // (c) key > 256 bytes → cancel; (d) multi-property → cancel; (e) absent schema → free-text.
    // Mode "form" with absent requestedSchema → cancel immediately (no free-text fallback).
    const PROP_KEY_CAP: usize = 256;
    let schema_present = !v["params"]["requestedSchema"].is_null();
    let root_schema = &v["params"]["requestedSchema"];

    // Root type validation: only "object" or absent/null allowed.
    let root_type_ok = match root_schema.get("type") {
        None => true,
        Some(v) if v.is_null() => true,
        Some(v) => v.as_str() == Some("object"),
    };

    // Root constraint allowlist — cancel on unsupported structural constraints.
    let root_constraints_ok = root_type_ok && {
        let required_ok = true; // re-validated post-parse against prop_key
        let additional_ok = true; // any additionalProperties value is safe for single-property responses
        const ALLOWED: &[&str] = &[
            "type", "properties", "required", "additionalProperties",
            "$schema", "title", "description",
        ];
        let all_root_keys_supported = root_schema.as_object().map_or(true, |obj| {
            obj.keys().all(|k| ALLOWED.contains(&k.as_str()))
        });
        required_ok && additional_ok && all_root_keys_supported
    };

    let props = if root_constraints_ok {
        v["params"]["requestedSchema"]["properties"].as_object()
    } else {
        None
    };

    // Returns Some((prop_key, options, prop_type)) to proceed, or None to cancel.
    let schema_result: Option<(String, Option<Vec<String>>, Option<String>)> = match props {
        Some(m) if m.len() == 1 => {
            let (raw_key, schema) = m.iter().next().unwrap();
            if raw_key.len() > PROP_KEY_CAP {
                tracing::warn!(req_id = ?elicitation_req_id, key_len = raw_key.len(),
                    "elicitation: property key exceeds 256 bytes; cancelling");
                None
            } else if schema.as_bool() == Some(false) {
                tracing::warn!(req_id = ?elicitation_req_id,
                    "elicitation: property schema is boolean `false`; cancelling");
                None
            } else if schema.as_bool() == Some(true) {
                Some((raw_key.clone(), None, None))
            } else if !schema.is_object() {
                tracing::warn!(req_id = ?elicitation_req_id,
                    "elicitation: property schema is not an object or boolean; cancelling");
                None
            } else {
                let key = raw_key.clone();
                let type_val = schema.get("type");
                let raw_type_str = type_val.and_then(|v| v.as_str());
                let pty = raw_type_str.filter(|t| *t == "string").map(str::to_string);
                let type_is_unsupported = type_val.map_or(false, |v| {
                    v.as_str().map_or(!v.is_null(), |s| s != "string")
                });
                if type_is_unsupported {
                    tracing::warn!(req_id = ?elicitation_req_id, schema_type = ?type_val,
                        "elicitation: unsupported property type; cancelling");
                    None
                } else {
                    // Check for unenforceable constraints (string/numeric) and unknown keywords.
                    let present = |k: &str| schema.get(k).is_some_and(|v| !v.is_null());
                    let has_string_constraints = matches!(pty.as_deref(), Some("string") | None)
                        && (present("minLength") || present("maxLength") || present("pattern")
                            || present("format") || present("const"));
                    let allowed_keys = ["type", "title", "description", "default", "examples",
                                        "$comment", "oneOf", "enum"];
                    let has_unknown_keyword = schema.as_object()
                        .map(|obj| obj.keys().any(|k| !allowed_keys.contains(&k.as_str())))
                        .unwrap_or(false);
                    if has_string_constraints || has_unknown_keyword {
                        tracing::warn!(req_id = ?elicitation_req_id,
                            "elicitation: schema has unenforceable constraints; cancelling");
                        None
                    } else {
                        // Parse oneOf/enum options with intersection when both present.
                        const OPT_CAP: usize = 512;
                        const ARRAY_CAP: usize = 512;
                        let one_of_present = present("oneOf");
                        let enum_present   = present("enum");
                        let one_of_len = schema["oneOf"].as_array().map_or(0, |a| a.len());
                        let enum_len   = schema["enum"].as_array().map_or(0, |a| a.len());
                        if one_of_len > ARRAY_CAP || enum_len > ARRAY_CAP {
                            tracing::warn!(req_id = ?elicitation_req_id,
                                one_of_len, enum_len, "elicitation: source array exceeds cap; cancelling");
                            None
                        } else {
                            let one_of_opts: Option<Vec<String>> = schema["oneOf"].as_array().map(|arr| {
                                let all_const_only = arr.iter().all(|v| {
                                    v.as_object().map_or(false, |m| m.len() == 1 && m.contains_key("const"))
                                });
                                if !all_const_only { return vec![]; } // sentinel → cancel
                                let collected: Vec<String> = arr.iter()
                                    .filter_map(|v| v["const"].as_str())
                                    .filter(|s| !s.is_empty() && s.len() <= OPT_CAP)
                                    .map(str::to_string).collect();
                                let unique_len = {
                                    let mut seen = std::collections::HashSet::new();
                                    collected.iter().filter(|s| seen.insert(s.as_str())).count()
                                };
                                if unique_len < collected.len() { vec![] } else { collected }
                            });
                            let enum_opts: Option<Vec<String>> = schema["enum"].as_array().map(|arr| {
                                arr.iter().filter_map(|x| x.as_str())
                                    .filter(|s| !s.is_empty())
                                    .filter(|s| { if s.len() > OPT_CAP {
                                        tracing::warn!(req_id = ?elicitation_req_id,
                                            "elicitation: dropping option string exceeding 512 bytes");
                                        false } else { true } })
                                    .map(str::to_string).collect()
                            });
                            // Intersect when both oneOf and enum are present.
                            let opts = if one_of_present && enum_present {
                                let enum_set: std::collections::HashSet<&str> = enum_opts
                                    .as_deref().into_iter().flatten().map(String::as_str).collect();
                                one_of_opts.map(|v| {
                                    v.into_iter().filter(|s| enum_set.contains(s.as_str())).collect::<Vec<_>>()
                                })
                            } else if one_of_present { one_of_opts } else { enum_opts };
                            let opts = opts.and_then(|v| if v.is_empty() { None } else { Some(v) });
                            // Apply 100-option cap once on the final (post-intersection) list.
                            let opts = opts.map(|mut v| { v.truncate(100); v });
                            // F16: selection constraint present but no representable choices → cancel.
                            if (one_of_present || enum_present) && opts.is_none() {
                                tracing::warn!(req_id = ?elicitation_req_id,
                                    "elicitation: all choices non-representable; cancelling");
                                None
                            } else {
                                Some((key.clone(), opts, pty))
                            }
                        }
                    }
                }
            }
        }
        Some(m) if m.len() > 1 => {
            tracing::warn!(req_id = ?elicitation_req_id, prop_count = m.len(),
                "elicitation: multi-field schema unsupported; cancelling");
            None
        }
        _ => {
            if schema_present {
                tracing::warn!(req_id = ?elicitation_req_id,
                    "elicitation: schema present but properties absent/empty; cancelling");
            } else {
                // Mode "form" with absent requestedSchema → cancel immediately.
                tracing::warn!(req_id = ?elicitation_req_id,
                    "elicitation: mode=form but requestedSchema absent; cancelling");
            }
            None
        }
    };

    let (prop_key, options, prop_type) = match schema_result {
        Some(r) => r,
        None => {
            let write_ok = {
                let _wg = proc.write_lock.lock().unwrap_or_else(|p| p.into_inner());
                let watchdog = WriteWatchdog::new(Arc::clone(&proc.kill_handle), WRITE_WATCHDOG_MS);
                let r = rpc_respond(&mut proc.stdin, &elicitation_req_id, json!({"action": "cancel"}));
                let fired = watchdog.complete();
                !fired && r.is_ok()
            };
            if !write_ok {
                return Ok(TurnResult { dead_session: true, ..TurnResult::default_at(output, usage, files) });
            }
            continue;
        }
    };

    // ── Post-parse required validation ────────────────────────────────────────
    if let Some(req) = root_schema.get("required").filter(|v| !v.is_null()) {
        let required_ok = req.as_array().map_or(false, |arr| {
            arr.is_empty() || (arr.len() == 1 && arr[0].as_str() == Some(prop_key.as_str()))
        });
        if !required_ok {
            tracing::warn!(req_id = ?elicitation_req_id, required = ?req, prop_key = %prop_key,
                "elicitation: `required` does not match single parsed property key; cancelling");
            let write_ok = {
                let _wg = proc.write_lock.lock().unwrap_or_else(|p| p.into_inner());
                let watchdog = WriteWatchdog::new(Arc::clone(&proc.kill_handle), WRITE_WATCHDOG_MS);
                let r = rpc_respond(&mut proc.stdin, &elicitation_req_id, json!({"action": "cancel"}));
                let fired = watchdog.complete();
                !fired && r.is_ok()
            };
            if !write_ok {
                return Ok(TurnResult { dead_session: true, ..TurnResult::default_at(output, usage, files) });
            }
            continue;
        }
    }

    // ── FIFTH: register (atomic) ───────────────────────────────────────────────
    let elicitation_id = uuid::Uuid::new_v4().to_string();
    let rx_res = {
        let mut maps = elicitation_maps.lock().unwrap_or_else(|e| e.into_inner());
        maps.register(run_id, &elicitation_id, run_epoch, options.clone())
    };
    let rx_res = match rx_res {
        Some(rx) => rx,
        None => {
            tracing::warn!(run_id = %run_id, epoch = run_epoch,
                "elicitation: epoch cancelled before registration; aborting");
            elicitation_timed_out = true;
            elicitation_teardown  = true;
            break;
        }
    };

    // Set in_flight_id immediately after successful registration (panic bridge).
    if let Some(ref mut g) = epoch_guard { g.in_flight_id = Some(elicitation_id.clone()); }

    // Emit. Returns false if actor channel closed → break immediately.
    if !emit_ev(CoreEvent::ElicitationCreated {
        session:         run_id.to_string(),
        epoch:           run_epoch,
        elicitation_id:  elicitation_id.clone(),
        message,
        options:         options.clone(),
        prop_type:       prop_type.clone(),
    }) {
        elicitation_maps.lock().unwrap_or_else(|e| e.into_inner())
            .remove(run_id, &elicitation_id);
        elicitation_timed_out = true;
        break;
    }
    tracing::info!(run_id = %run_id, elicitation_id = %elicitation_id,
        option_count = options.as_ref().map_or(0, |v| v.len()), "elicitation.created");

    // ── Dual-poll loop ─────────────────────────────────────────────────────────
    const ELICITATION_POLL_MS: u64 = 50;
    let mut result = 'elicit: loop {
        // Check deadline BEFORE polling — prevents accepting an answer at the boundary
        // that should be elicitation_timed_out.
        {
            let remaining = deadline.checked_duration_since(Instant::now()).unwrap_or_default();
            if remaining.is_zero() {
                elicitation_timed_out = true;
                tracing::warn!(run_id = %run_id, elicitation_id = %elicitation_id,
                    "elicitation.timed_out (pre-recv deadline guard)");
                break 'elicit ElicitationResult { action: "cancel".into(), response: None };
            }
        }
        // 1. Check resolution (non-blocking).
        match rx_res.try_recv() {
            Ok(r) => {
                // P2b: ResolveElicitation-before-CancelRun race — check tombstone explicitly.
                let is_cancelled = {
                    let maps = elicitation_maps.lock().unwrap_or_else(|e| e.into_inner());
                    maps.is_epoch_cancelled(run_id, run_epoch)
                };
                if is_cancelled {
                    elicitation_timed_out = true;
                    elicitation_teardown  = true;
                    tracing::warn!(run_id = %run_id, elicitation_id = %elicitation_id,
                        "elicitation: epoch cancelled after deliver; treating as teardown (P2b)");
                    break 'elicit ElicitationResult { action: "cancel".into(), response: None };
                }
                break 'elicit r;
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                elicitation_timed_out = true;
                elicitation_teardown  = true;
                tracing::info!(run_id = %run_id, elicitation_id = %elicitation_id,
                    reason = "teardown", "elicitation.cancelled");
                break 'elicit ElicitationResult { action: "cancel".into(), response: None };
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
        // 2. Check deadline.
        let remaining = deadline.checked_duration_since(Instant::now()).unwrap_or_default();
        if remaining.is_zero() {
            elicitation_timed_out = true;
            tracing::warn!(run_id = %run_id, elicitation_id = %elicitation_id,
                overrun_ms = deadline.elapsed().as_millis() as u64, "elicitation.timed_out");
            break 'elicit ElicitationResult { action: "cancel".into(), response: None };
        }
        // 3. Drain stdout for up to ELICITATION_POLL_MS.
        let poll = remaining.min(Duration::from_millis(ELICITATION_POLL_MS));
        match proc.line_rx.recv_timeout(poll) {
            Ok(line) => {
                // Frame byte cap — defence-in-depth (primary cap is in the stdout reader thread).
                // FRAME_BYTE_CAP = MAX_OUT * 7 (56 MiB) — same value as start_acp_process.
                if line.len() > FRAME_BYTE_CAP {
                    tracing::warn!(frame_len = line.len(),
                        "elicitation: inbound frame exceeds FRAME_BYTE_CAP; dropping");
                    continue 'elicit;
                }
                let lv: Value = match serde_json::from_str(&line) {
                    Ok(v) => v, Err(_) => continue 'elicit
                };
                if lv.get("method").and_then(Value::as_str) == Some("session/update") {
                    handle_update(&lv, emit, &mut output, &mut usage, &mut files, MAX_OUT);
                    continue 'elicit;
                }
                // Second concurrent elicitation/create: cancel immediately (first-held wins).
                if lv.get("method").and_then(Value::as_str) == Some("elicitation/create") {
                    if !lv["id"].is_null() {
                        let write_result = {
                            let _wg = proc.write_lock.lock().unwrap_or_else(|p| p.into_inner());
                            let watchdog = WriteWatchdog::new(Arc::clone(&proc.kill_handle), WRITE_WATCHDOG_MS);
                            let r = rpc_respond(&mut proc.stdin, &lv["id"], json!({"action": "cancel"}));
                            let fired = watchdog.complete();
                            if fired { Err(anyhow::anyhow!("watchdog fired")) } else { r }
                        };
                        if write_result.is_err() {
                            {
                                let mut maps = elicitation_maps.lock().unwrap_or_else(|p| p.into_inner());
                                maps.remove(run_id, &elicitation_id);
                            }
                            elicitation_timed_out = true;
                            elicitation_teardown = true;
                            break 'elicit ElicitationResult { action: "cancel".to_string(), response: None };
                        }
                        tracing::info!(run_id = %run_id, reason = "superseded",
                            second_req_id = ?lv["id"], "elicitation.cancelled");
                    }
                    continue 'elicit;
                }
                // Unhandled inbound frame — log and drop.
                if let Some(method) = lv.get("method").and_then(Value::as_str) {
                    tracing::warn!(elicitation_id = %elicitation_id, method,
                        "elicitation: unhandled inbound frame dropped; adapter may hang");
                    continue 'elicit;
                }
                // session/prompt response arrived before elicitation resolved.
                if lv.get("method").is_none() && lv.get("id").and_then(Value::as_u64) == Some(id) {
                    if let Some(u) = parse_result_usage(&lv["result"]["usage"]) {
                        let cost = usage.as_ref().and_then(|ex| ex.cost_usd);
                        usage = Some(Usage { cost_usd: cost.or(u.cost_usd), ..u });
                    }
                    if lv.get("error").is_some() {
                        // Case (a): JSON-RPC error → Failed path (not human-dismiss).
                        // Set prompt_error=true so the final status construction returns Failed,
                        // not Ok. prompt_done_path alone (without this flag) would route to Ok
                        // and contradict test 19's expected Failed result.
                        prompt_error = true;
                        break 'elicit ElicitationResult { action: "prompt_done".into(), response: None };
                    }
                    let stop = lv["result"]["stopReason"].as_str().unwrap_or("end_turn");
                    found = stop != "cancelled";
                    if !found { timed_out = true; }
                    break 'elicit ElicitationResult { action: "prompt_done".into(), response: None };
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // Adapter stdout pipe closed — distinct from human-dismiss.
                elicitation_timed_out = true;
                elicitation_teardown  = true;
                tracing::warn!(elicitation_id = %elicitation_id,
                    "elicitation: adapter stdout disconnected mid-suspend; mapping to Failed");
                break 'elicit ElicitationResult { action: "cancel".into(), response: None };
            }
        }
    };

    // ── Post-deadline drain + cleanup — under a single lock hold ──────────────
    prompt_done_path = result.action == "prompt_done";  // assign turn-level flag (declared before outer loop)
    {
        let mut maps = elicitation_maps.lock().unwrap_or_else(|e| e.into_inner());
        // Unconditional drain: if an answer arrived, forward it to the adapter.
        // Tombstone re-check while holding the lock closes the deliver-interleave window.
        if let Ok(late) = rx_res.try_recv() {
            if late.action != "cancel" {
                if !maps.is_epoch_cancelled(run_id, run_epoch) {
                    result = late;
                } else {
                    tracing::warn!(run_id = %run_id, elicitation_id = %elicitation_id,
                        "elicitation: drain discarding late answer — epoch cancelled (P1b)");
                    elicitation_teardown = true;
                }
            }
        }
        // Unconditional tombstone re-check — covers the case where the initial try_recv
        // already consumed the answer but cancel_epoch ran after that recv.
        if maps.is_epoch_cancelled(run_id, run_epoch) {
            tracing::warn!(run_id = %run_id, elicitation_id = %elicitation_id,
                "elicitation: epoch tombstoned after initial recv; overriding to teardown");
            elicitation_teardown  = true;
            elicitation_timed_out = true;
            result = ElicitationResult { action: "cancel".into(), response: None };
        }
        // maps.remove() inside the drain lock closes the deliver() race.
        // After this call, deliver() finds "not found" for any concurrent call.
        maps.remove(run_id, &elicitation_id);
    }

    // ── Pre-write cancellation gate ───────────────────────────────────────────
    if !elicitation_timed_out && !elicitation_teardown {
        let pre_write_cancelled = {
            let maps = elicitation_maps.lock().unwrap_or_else(|p| p.into_inner());
            maps.is_epoch_cancelled(run_id, run_epoch)
        };
        if pre_write_cancelled {
            tracing::warn!(run_id = %run_id, elicitation_id = %elicitation_id,
                "elicitation: epoch tombstoned before adapter write; overriding to cancel");
            elicitation_teardown = true;
            elicitation_timed_out = true;
            result = ElicitationResult { action: "cancel".into(), response: None };
        }
    }

    // ── Build wire action ─────────────────────────────────────────────────────
    let wire_action: &str = match result.action.as_str() {
        "prompt_done" => "cancel",
        other => other,
    };
    // content belongs ONLY to accept — ACP elicitation results are discriminated by action.
    // decline/cancel with a non-null response (e.g., stale UI value) must not emit content:
    // it would produce an invalid adapter frame and is ignored by the protocol.
    let resp_result = if wire_action == "accept" {
        match result.response.as_ref() {
            Some(r) => json!({"action": wire_action, "content": {&prop_key: r}}),
            None    => json!({"action": wire_action}),  // free-text accept with no schema
        }
    } else {
        json!({"action": wire_action})  // decline/cancel: no content field
    };

    // ── Phase 1: decide final action under maps lock (no I/O) ─────────────────
    let final_action: &str = {
        let maps = elicitation_maps.lock().unwrap_or_else(|p| p.into_inner());
        let is_cancelled = maps.is_epoch_cancelled(run_id, run_epoch);
        if is_cancelled && !elicitation_timed_out && !elicitation_teardown {
            elicitation_teardown = true;
            elicitation_timed_out = true;
            "cancel"
        } else {
            wire_action
        }
    };

    // ── Phase 2: acquire write_lock, re-check tombstone, write ────────────────
    // FRAME_BYTE_CAP = MAX_OUT * 7 (56 MiB). Defined at module scope.
    let (write_err, final_action, deliberate_kill) = {
        let _write_guard = proc.write_lock.lock().unwrap_or_else(|p| p.into_inner());
        let final_action = {
            let maps = elicitation_maps.lock().unwrap_or_else(|p| p.into_inner());
            let newly_cancelled = maps.is_epoch_cancelled(run_id, run_epoch)
                && !elicitation_timed_out && !elicitation_teardown;
            if newly_cancelled {
                elicitation_teardown = true;
                elicitation_timed_out = true;
                "cancel"
            } else { final_action }
        };
        let final_result = match final_action {
            "cancel" => json!({"action": "cancel"}),
            _ => resp_result,
        };
        // Kill before write on the teardown path only.
        // On a pure timeout path with a late human answer, do NOT kill first —
        // the adapter is still alive and WriteWatchdog bounds the write.
        let mut deliberate_kill = false;
        if elicitation_teardown || (elicitation_timed_out && final_action == "cancel") {
            deliberate_kill = true;
            proc.kill_handle.signal();
        }
        // Store effective outcome in EpochCleanup BEFORE the write so drop() can emit the
        // correct action/reason on panic. On the success path in_flight_id is cleared at the
        // emit_ev call below, so drop() will not emit again. Pre-write panic is still "cancel"
        // because set_in_flight_outcome has not yet been called.
        let pre_write_reason = if elicitation_teardown { "teardown" }
            else if elicitation_timed_out { "timeout" }
            else if prompt_done_path { "session_prompt" }
            else { "human" };
        if let Some(ref mut g) = epoch_guard { g.set_in_flight_outcome(final_action, pre_write_reason); }
        let watchdog = WriteWatchdog::new(Arc::clone(&proc.kill_handle), WRITE_WATCHDOG_MS);
        let err = rpc_respond(&mut proc.stdin, &elicitation_req_id, final_result).err();
        let watchdog_fired = watchdog.complete();
        let err = if watchdog_fired && err.is_none() {
            Some(anyhow::anyhow!("write watchdog fired after rpc_respond returned"))
        } else { err };
        // Post-write cancellation check — still inside write lock scope.
        // shared_run_terminal uses try_lock and does NOT block on _write_guard; it may have
        // tombstoned the epoch and published RunCancelled during the write above.
        // Checking here (still holding _write_guard) serializes with any future try_lock:
        // if we detect the tombstone, the child is already signalled and the epoch is dead.
        // Override final_action to "cancel" so ElicitationResolved emits action="cancel"
        // with reason="teardown" — the adapter may have received the accept wire frame,
        // but from the session's perspective the accept is voided by concurrent cancellation.
        // (Acknowledged residual race: if CancelRun tombstones after we release _write_guard,
        //  the outer post-write check at the call site catches it; in that case the accept
        //  was delivered before cancel, so action="accept",reason="teardown" is emitted —
        //  an accepted-then-cancelled sequence that is correct and observable in the audit log.)
        let (final_action, deliberate_kill) = {
            let maps = elicitation_maps.lock().unwrap_or_else(|p| p.into_inner());
            let mid_write_cancelled = maps.is_epoch_cancelled(run_id, run_epoch)
                && !elicitation_timed_out && !elicitation_teardown;
            if mid_write_cancelled {
                elicitation_teardown = true;
                elicitation_timed_out = true;
                ("cancel", true)  // override: cancellation arrived during write
            } else {
                (final_action, deliberate_kill)
            }
        };
        (err, final_action, deliberate_kill)
    };

    // ── Post-write tombstone check ────────────────────────────────────────────
    if !elicitation_timed_out && !elicitation_teardown {
        let post_write_cancelled = {
            let maps = elicitation_maps.lock().unwrap_or_else(|p| p.into_inner());
            maps.is_epoch_cancelled(run_id, run_epoch)
        };
        if post_write_cancelled {
            elicitation_teardown = true;
            elicitation_timed_out = true;
        }
    }

    // ── Emit ElicitationResolved ──────────────────────────────────────────────
    let (event_action, event_reason) = match (&write_err, deliberate_kill) {
        (Some(_), false) if !elicitation_teardown => {
            // Unexpected transport failure (not our own kill, not actor-initiated).
            // Use final_action (the attempted wire action — accept, decline, or cancel)
            // rather than a synthetic "cancel". No cancel frame was written; replacing
            // the action with "cancel" violates the effective-wire-action contract and
            // causes audit consumers to record a human cancellation for what was actually
            // a failed accept or decline. reason="adapter_write_failure" conveys delivery
            // failure independently of which action was attempted.
            (final_action.to_string(), "adapter_write_failure".to_string())
        }
        _ => {
            let reason = if elicitation_teardown { "teardown" }
                else if elicitation_timed_out { "timeout" }
                else if prompt_done_path { "session_prompt" }
                else { "human" };
            (final_action.to_string(), reason.to_string())
        }
    };
    // Overwrite the pre-write snapshot with the authoritative outcome. The pre-write call to
    // set_in_flight_outcome used approximate flags; event_action/event_reason now reflect the
    // actual write result (write error, post-write cancellation, etc.). If a panic occurs
    // between here and in_flight_id being cleared below, drop() emits the correct values.
    if let Some(ref mut g) = epoch_guard { g.set_in_flight_outcome(&event_action, &event_reason); }
    emit_ev(CoreEvent::ElicitationResolved {
        session:         run_id.to_string(),
        elicitation_id:  elicitation_id.clone(),
        action:          event_action.clone(),
        reason:          event_reason.clone(),
    });
    tracing::info!(run_id = %run_id, elicitation_id = %elicitation_id,
        action = %event_action, reason = %event_reason, "elicitation.resolved");
    // Clear in_flight_id — the resolved event is enqueued; panic after this is safe.
    if let Some(ref mut g) = epoch_guard { g.in_flight_id = None; }

    // ── Propagate write failure ───────────────────────────────────────────────
    let silently_ignore_write_failure =
        result.action == "cancel" || prompt_done_path || elicitation_timed_out;
    if !silently_ignore_write_failure {
        if write_err.is_some() {
            // Accept/decline write failed. exec_turn_acp is a free function (no self, no cli_key);
            // signal via write_failed_terminal flag so exec_turn can call drop_session_gen.
            return Ok(TurnResult { write_failed_terminal: true, ..TurnResult::default_at(output, usage, files) });
        }
    } else if write_err.is_some() {
        dead_session = true;
    }

    // ── Route outer loop ──────────────────────────────────────────────────────
    if found || elicitation_timed_out { break; }
    if result.action == "cancel" && !prompt_done_path { cancelled = true; break; }
    if prompt_done_path { break; }
    // Per-iteration tombstone check before re-entering the outer loop.
    // Combined with the 500ms bounded poll (above), this closes the post-remove CancelRun window.
    if {
        let maps = elicitation_maps.lock().unwrap_or_else(|p| p.into_inner());
        maps.is_epoch_cancelled(run_id, run_epoch)
    } {
        elicitation_timed_out = true;
        break;
    }
    continue; // "decline": re-enter outer loop
} // end elicitation/create arm

// ── Final status construction after 'outer loop ───────────────────────────────
// Status mapping after 'outer loop exits — exit flags are checked in priority order:
//
//   1. found=true                         → human resolved (accept/decline)   → Ok
//   2. cancelled / timed_out / elicitation_timed_out                          → Cancelled
//      - cancelled=true        → human explicitly dismissed (action="cancel", !prompt_done_path)
//      - timed_out=true        → ordinary turn deadline (outer loop)
//      - elicitation_timed_out → epoch tear-down or inner deadline
//        (exec_turn arm 1 also catches elicitation_timed_out and overrides to ElicitationFailed)
//   3. prompt_done_path=true && !prompt_error → session/prompt won cleanly    → Ok
//   4. prompt_error=true (→ prompt_done_path=true too, but error wins)        → Failed
//      JSON-RPC error in the session/prompt response; test 19 asserts Failed.
//   5. else → Failed (unreachable in normal operation; surfaces logic gaps)
let status = if found {
    StepStatus::Ok
} else if cancelled || elicitation_timed_out || timed_out {
    StepStatus::Cancelled
} else if prompt_done_path && !prompt_error {
    StepStatus::Ok
} else {
    StepStatus::Failed  // prompt_error=true or unreachable gap
};
return Ok(TurnResult {
    status,
    cancelled,
    elicitation_timed_out,
    dead_session,
    write_failed_terminal: false,
    ..TurnResult::default_at(output, usage, files)
});
```

---

## `exec_turn` match block (`AcpStepRunner::exec_turn`)

The match on `exec_turn_acp` result uses this arm order — the first matching arm wins:

```rust
match exec_turn_acp(..., _epoch_guard.as_mut()) {
    // FIRST: elicitation_timed_out — must precede Ok/Cancelled.
    // elicitation_timed_out can be true alongside any status; placing after Ok would
    // let the Ok arm fire, bypassing the session drop and pipe unblock.
    Ok(result) if result.elicitation_timed_out => {
        drop(proc);
        self.drop_session_gen(&run_id, cli_key, my_session_gen);
        StepOutput { status: StepStatus::ElicitationFailed, ..result.into_step_output(input) }
    }

    // SECOND: write_failed_terminal (before dead_session).
    Ok(result) if result.write_failed_terminal => {
        self.drop_session_gen(&run_id, cli_key, my_session_gen);
        return StepOutput { status: StepStatus::ElicitationFailed, ..result.into_step_output(input) };
    }

    // THIRD: dead_session (before status==Ok).
    // Scenario: session/prompt arrives during elicitation (found=true → status=Ok)
    // and the cancel-response write fails. Without eviction, the session is retained
    // with broken stdin.
    Ok(result) if result.dead_session => {
        self.drop_session_gen(&run_id, cli_key, my_session_gen);
        match result.status {
            StepStatus::Ok | StepStatus::Cancelled =>
                StepOutput { ..result.into_step_output(input) },
            _ => {
                // status==Failed (or ElicitationFailed) with dead_session → fallback.
                // Before launching the fallback CLI, check if the run was cancelled or shutdown
                // raced the failing write. The same cancellation check used by the Err/startup
                // branches applies here — without it, the fallback executes work after CancelRun.
                let cancelled = {
                    let m = self.elicitation_maps.lock().unwrap_or_else(|p| p.into_inner());
                    m.shutdown_flag() || m.is_epoch_cancelled(&run_id, run_epoch)
                };  // lock released
                if cancelled {
                    return StepOutput {
                        status: StepStatus::ElicitationFailed,
                        ..input.blank_step_output()
                    };
                }
                fallback_with_warning(run_id, unit, input, "dead_session")
            }
        }
    }

    // Normal paths.
    Ok(result) if result.status == StepStatus::Ok => { /* Ok output */ }
    Ok(result) if result.status == StepStatus::Cancelled => {
        // Handles both cancelled=true (human dismiss) and ordinary timed_out.
        // Does NOT match when elicitation_timed_out=true — caught in the first arm.
        /* drop session, return Cancelled */
    }
    Ok(_) => {
        // SESSION_DIED → fallback_with_warning (existing; unchanged for non-elicitation).
    }

    // Err arm: exec_turn_acp itself failed (e.g. rpc_expect Err during the turn).
    // Check epoch/shutdown tombstone BEFORE fallback in BOTH the Err arm AND the
    // startup-error branch (start_acp_process fails before exec_turn_acp is called).
    Err(e) => {
        // Compute cancelled in a nested scope so the lock is released before fallback.
        // Without this, `m` would be held across the entire blocking fallback_with_warning
        // call, preventing CancelRun, Shutdown, reassignment, and elicitation delivery
        // from acquiring the maps lock for the fallback CLI's full runtime.
        let cancelled = {
            // elicitation_maps is Arc<Mutex<ElicitationMaps>> (NOT Option).
            let m = self.elicitation_maps.lock().unwrap_or_else(|p| p.into_inner());
            m.shutdown_flag() || m.is_epoch_cancelled(&run_id, run_epoch)
        };  // lock released here — fallback proceeds without holding the maps mutex
        if cancelled {
            // exec_turn returns StepOutput (NOT Result<StepOutput>).
            return StepOutput {
                status: StepStatus::ElicitationFailed,
                ..input.blank_step_output()
            };
        }
        // Not cancelled — genuine turn error (e.g. rpc_expect Err on a dead child,
        // or initial session/prompt write failing before exec_turn_acp returns).
        // Evict the broken process guard BEFORE fallback; otherwise the dead process
        // remains cached and every subsequent unit reuses it, fails immediately, and
        // falls back again — burning retries until session_gen is eventually rotated.
        // This mirrors the existing eviction in the Ok(result) branches.
        self.drop_session_gen(&run_id, cli_key, my_session_gen);
        // Lock is not held during fallback.
        fallback_with_warning(run_id, unit, input, &e.to_string())
    }
}

// Startup-error branch (start_acp_process fails BEFORE exec_turn_acp is called):
let proc = match start_acp_process(...) {
    Ok(p) => p,
    Err(e) => {
        // Compute cancelled in a nested scope so the lock is released before fallback.
        // Without this, `m` would be held across the entire blocking fallback_with_warning
        // call (same hazard as the exec_turn_acp Err arm), preventing CancelRun, Shutdown,
        // reassignment, and elicitation delivery from acquiring the maps lock.
        let cancelled = {
            let m = self.elicitation_maps.lock().unwrap_or_else(|p| p.into_inner());
            m.shutdown_flag() || m.is_epoch_cancelled(&run_id, run_epoch)
        };  // lock released here before fallback
        if cancelled {
            return StepOutput {
                status: StepStatus::ElicitationFailed,
                ..input.blank_step_output()
            };
        }
        return fallback_with_warning(run_id, unit, input, &e.to_string());
    }
};
```

---

## Frame byte cap (`src/acp_runner.rs` stdout reader thread)

```rust
const MAX_OUT: usize = 8 * 1024 * 1024;    // 8 MiB accumulated output budget
const FRAME_BYTE_CAP: usize = MAX_OUT * 7;  // 56 MiB; worst-case 6× JSON-string expansion + envelope
```

The 7× multiplier covers worst-case 6× JSON-string expansion (non-ASCII/control bytes → `\uXXXX`) plus envelope overhead. Using MAX_OUT (8 MiB) or 2× terminates valid sessions with heavily-escaped content.

The stdout reader thread uses `limited_read_until` (bounded read to cap + 1 bytes) to prevent unbounded allocation. `BufRead::lines()` allocates the entire frame before yielding; a post-yield length check does not prevent the allocation.

On frame exceeding cap: signal the kill_handle and return `Err` — the SESSION_DIED path handles recovery. Silently dropping the oversized frame is unsafe for handshake and prompt responses.
