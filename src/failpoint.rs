//! Test-only crash points (DES-TEAMING-002 T3, round 10). A test ARMS a named point for one run;
//! when the actor reaches it for that run, the actor thread panics — a faithful crash: every write
//! the engine made so far stays, nothing after it happens, and the test restarts an engine over
//! the same store. Armed per `(point, run)` and consumed once, so parallel tests never collide. In
//! a non-test build every point is a no-op.

#[cfg(test)]
static ARMED: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

/// Arm `point` for `run_id` (tests only).
#[cfg(test)]
pub(crate) fn arm(point: &str, run_id: &str) {
    ARMED
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push((point.to_string(), run_id.to_string()));
}

/// A crash point: panics the calling (actor) thread when `point` is armed for `run_id`.
#[inline]
pub(crate) fn crash_point(point: &str, run_id: &str) {
    #[cfg(test)]
    {
        let hit = {
            let mut armed = ARMED.lock().unwrap_or_else(|p| p.into_inner());
            match armed.iter().position(|(p, r)| p == point && r == run_id) {
                Some(i) => {
                    armed.remove(i);
                    true
                }
                None => false,
            }
        };
        if hit {
            panic!("failpoint `{point}` for run {run_id}: simulated crash");
        }
    }
    #[cfg(not(test))]
    let _ = (point, run_id);
}
