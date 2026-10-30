//! The engine's bus mirror of a run's lifecycle (program DES-PLUGIN-EVENTS §3.1; DES-POWER-MODE
//! §6): a fixed set of CoreEvents, re-published on wicked-bus under the designed namespace
//! `wicked.run.<noun>.<verb>` (4 segments, `domain=wicked-core`, subdomain `run`), so a plugin or
//! studio reads ONE durable, ordered, cursorable stream. Each row's payload is the `/ws` frame
//! without `type` and `session`, plus `runId`, `seq` (the run log's own seq of the event it
//! mirrors) and `at`; its idempotency key is `run:<run>:<seq>`, so a re-emit is a dedup.
//!
//! Lifecycle only: output deltas, terminal bytes and heartbeats are never mirrored. Bulky fields
//! (`output`, `log`, `stdout`, `stderr`) are dropped from a payload. Best effort and off the actor
//! thread: one writer thread owns the queue and opens the bus through the process-wide handle; a
//! failure costs the mirror row, never the run, and is logged once per minute at most. It runs
//! only with `WICKED_RUN_MIRROR=1` and a `WICKED_BUS_DB`.

use std::sync::mpsc::{channel, Sender};
use std::sync::{Mutex, OnceLock};

/// The bus type a CoreEvent `type` mirrors to, `None` when it is not mirrored.
pub fn bus_type(event_type: &str) -> Option<&'static str> {
    Some(match event_type {
        "powerIntakePlanned" => "wicked.run.intake.planned",
        "rigorRescored" => "wicked.run.rigor.rescored",
        "powerCheckpointReached" => "wicked.run.checkpoint.reached",
        "powerTurnStarted" => "wicked.run.turn.started",
        "powerTurnCompleted" => "wicked.run.turn.completed",
        "powerHandoffStepped" => "wicked.run.handoff.stepped",
        "powerToggled" => "wicked.run.power.toggled",
        "powerActivity" => "wicked.run.activity.recorded",
        "powerWorkingNotes" => "wicked.run.notes.recorded",
        "unitExecuting" => "wicked.run.unit.started",
        "unitDone" | "unitDenied" | "stepFailed" => "wicked.run.unit.finished",
        "repoChecksEvaluated" => "wicked.run.checks.evaluated",
        "awaitingHuman" => "wicked.run.human.awaited",
        "qeAcceptanceDecided" => "wicked.run.qe.decided",
        "workerMessageQueued" => "wicked.run.steer.queued",
        "workerMessageInjected" => "wicked.run.steer.delivered",
        _ => return None,
    })
}

const BULKY: [&str; 4] = ["output", "log", "stdout", "stderr"];

fn strip(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(m) => {
            m.retain(|k, _| !BULKY.contains(&k.as_str()));
            for x in m.values_mut() {
                strip(x);
            }
        }
        serde_json::Value::Array(a) => a.iter_mut().for_each(strip),
        _ => {}
    }
}

/// The mirror row for one recorded event (`json` is [`crate::event::CoreEvent::to_json`]),
/// `None` when the event is not mirrored or carries no run.
pub fn row(json: &serde_json::Value, seq: u64, at: u64) -> Option<crate::bus::BusEmit> {
    let ty = json.get("type")?.as_str()?;
    // A live tool frame mirrors by its state (power-mode-ui contract).
    let bus = if ty == "toolActivity" {
        match json.get("state").and_then(|s| s.as_str()) {
            Some("started") => "wicked.run.tool.started",
            Some("finished") => "wicked.run.tool.finished",
            _ => return None,
        }
    } else {
        bus_type(ty)?
    };
    let run_id = json.get("session")?.as_str()?.to_string();
    let mut payload = json.clone();
    if let Some(m) = payload.as_object_mut() {
        m.remove("type");
        m.remove("session");
        if ty == "unitDone" || ty == "unitDenied" || ty == "stepFailed" {
            m.insert(
                "outcome".into(),
                serde_json::json!(match ty {
                    "unitDone" => "done",
                    "unitDenied" => "denied",
                    _ => "failed",
                }),
            );
        }
    }
    strip(&mut payload);
    if let Some(m) = payload.as_object_mut() {
        m.insert("runId".into(), serde_json::json!(run_id));
        m.insert("seq".into(), serde_json::json!(seq));
        m.insert("at".into(), serde_json::json!(at));
    }
    Some(
        crate::bus::BusEmit::new(bus, crate::bus::CORE_DOMAIN, "run", payload)
            .with_key(format!("run:{run_id}:{seq}")),
    )
}

fn writer() -> Option<&'static Mutex<Sender<crate::bus::BusEmit>>> {
    static W: OnceLock<Option<Mutex<Sender<crate::bus::BusEmit>>>> = OnceLock::new();
    W.get_or_init(|| {
        // Opt-in: the daemon that serves plugins turns it on (`WICKED_RUN_MIRROR=1`, crew sets
        // it at boot); an embedder, a test process or a scratch bus is never written behind its
        // back. Unit tests never mirror.
        if cfg!(test) || std::env::var("WICKED_RUN_MIRROR").ok().as_deref() != Some("1") {
            return None;
        }
        let path = std::env::var("WICKED_BUS_DB")
            .ok()
            .filter(|p| !p.is_empty())?;
        let (tx, rx) = channel::<crate::bus::BusEmit>();
        std::thread::Builder::new()
            .name("wicked-core-run-mirror".into())
            .spawn(move || {
                let mut last_warn: Option<std::time::Instant> = None;
                let mut db: Option<crate::bus::BusDb> = None;
                for ev in rx {
                    if db.is_none() {
                        db = crate::bus::BusDb::shared(&path).ok();
                    }
                    let res = match &db {
                        Some(d) => d
                            .emit_bounded(&ev, std::time::Duration::from_secs(5))
                            .map(|_| ()),
                        None => Err(anyhow::anyhow!("the bus at {path} could not be opened")),
                    };
                    if let Err(e) = res {
                        if last_warn.is_none_or(|t| t.elapsed().as_secs() >= 60) {
                            eprintln!(
                                "wicked-core: run mirror: {} not mirrored ({e:#})",
                                ev.event_type
                            );
                            last_warn = Some(std::time::Instant::now());
                        }
                    }
                }
            })
            .ok()?;
        Some(Mutex::new(tx))
    })
    .as_ref()
}

/// Mirror one recorded event (called at the single emit point, after the run log stamped it).
pub fn mirror(json: &serde_json::Value, seq: u64, at: u64) {
    let Some(r) = row(json, seq, at) else {
        return;
    };
    if let Some(w) = writer() {
        let _ = w.lock().unwrap_or_else(|p| p.into_inner()).send(r);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mirrored_type_has_four_segments_in_the_run_domain() {
        for t in [
            "powerIntakePlanned",
            "rigorRescored",
            "powerCheckpointReached",
            "powerTurnStarted",
            "powerTurnCompleted",
            "powerHandoffStepped",
            "powerToggled",
            "powerActivity",
            "unitExecuting",
            "unitDone",
            "unitDenied",
            "stepFailed",
            "repoChecksEvaluated",
            "awaitingHuman",
            "qeAcceptanceDecided",
            "workerMessageQueued",
            "workerMessageInjected",
        ] {
            let b = bus_type(t).unwrap();
            assert_eq!(b.split('.').count(), 4, "{b}");
            assert!(b.starts_with("wicked.run."), "{b}");
        }
        assert!(bus_type("unitOutputDelta").is_none());
        assert!(bus_type("heartbeat").is_none());
    }

    #[test]
    fn a_row_carries_run_seq_and_the_frame_without_bulk() {
        let json = serde_json::json!({
            "type": "repoChecksEvaluated", "session": "r1", "ord": 3, "passed": true,
            "checks": [{"name": "test", "output": "x".repeat(10)}],
        });
        let r = row(&json, 42, 7).unwrap();
        assert_eq!(r.event_type, "wicked.run.checks.evaluated");
        assert_eq!(r.idempotency_key.as_deref(), Some("run:r1:42"));
        assert_eq!(r.payload["runId"], "r1");
        assert_eq!(r.payload["seq"], 42);
        assert_eq!(r.payload["ord"], 3);
        assert!(r.payload.get("type").is_none());
        assert!(r.payload["checks"][0].get("output").is_none());
        let done = row(
            &serde_json::json!({"type": "unitDenied", "session": "r1", "ord": 1}),
            1,
            1,
        )
        .unwrap();
        assert_eq!(done.payload["outcome"], "denied");
        assert!(row(&serde_json::json!({"type": "unitDone"}), 1, 1).is_none());
    }

    /// A live tool frame mirrors by its state; the notes frame has its own type.
    #[test]
    fn tool_frames_mirror_as_started_or_finished_and_notes_as_recorded() {
        let f = |state: &str| {
            serde_json::json!({"type": "toolActivity", "session": "r1", "ord": 3, "state": state,
                "id": "t1", "kind": "read", "tool": "Read"})
        };
        assert_eq!(
            row(&f("started"), 1, 1).unwrap().event_type,
            "wicked.run.tool.started"
        );
        assert_eq!(
            row(&f("finished"), 2, 1).unwrap().event_type,
            "wicked.run.tool.finished"
        );
        assert!(row(&f("weird"), 3, 1).is_none());
        let n = serde_json::json!({"type": "powerWorkingNotes", "session": "r1", "ord": 3});
        assert_eq!(
            row(&n, 4, 1).unwrap().event_type,
            "wicked.run.notes.recorded"
        );
    }
}
