//! Where a seat's arm spends its time (k8s-final, phase C's 51 s dispatch-to-pod gap).
//!
//! The engine arms a seat on the thread that dispatches its unit, with no await in between, so a
//! thread-local clock sees every phase: [`begin`] before the arm, [`mark`] at each phase's end
//! (the time since the previous mark is the phase's), [`take`] after it. A mark with no clock
//! running (a live test, a check pod) costs nothing and records nothing.

use std::cell::RefCell;
use std::time::Instant;

pub(crate) use crate::event::ArmPhase as Phase;

struct Clock {
    start: Instant,
    last: Instant,
    phases: Vec<Phase>,
}

thread_local! {
    static CLOCK: RefCell<Option<Clock>> = const { RefCell::new(None) };
}

/// Start timing an arm on this thread; a clock left running by an earlier arm is dropped.
pub(crate) fn begin() {
    let now = Instant::now();
    CLOCK.with(|c| {
        *c.borrow_mut() = Some(Clock {
            start: now,
            last: now,
            phases: Vec::new(),
        })
    });
}

/// End phase `name`: the time since the previous mark (or [`begin`]) is charged to it.
pub(crate) fn mark(name: &str) {
    mark_with(name, None);
}

/// [`mark`], with what the phase moved.
pub(crate) fn mark_with(name: &str, detail: Option<String>) {
    let now = Instant::now();
    CLOCK.with(|c| {
        if let Some(clock) = c.borrow_mut().as_mut() {
            let ms = now.duration_since(clock.last).as_millis() as u64;
            clock.last = now;
            clock.phases.push(Phase {
                name: name.to_string(),
                ms,
                detail,
            });
        }
    });
}

/// The phases so far and the arm's whole time (ms), ending the clock. `None` when none ran.
pub(crate) fn take() -> Option<(Vec<Phase>, u64)> {
    CLOCK.with(|c| {
        c.borrow_mut().take().map(|clock| {
            let total = clock.start.elapsed().as_millis() as u64;
            (clock.phases, total)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn k8s_the_arm_clock_charges_each_phase_since_the_previous_mark() {
        // No clock: marks are free and nothing is recorded.
        mark("orphan");
        assert!(take().is_none());
        begin();
        std::thread::sleep(std::time::Duration::from_millis(20));
        mark("first");
        mark_with("second", Some("3 entries".into()));
        let (phases, total) = take().expect("a clock ran");
        assert_eq!(
            phases.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            vec!["first", "second"]
        );
        assert!(phases[0].ms >= 20, "{phases:?}");
        assert!(phases[1].ms < phases[0].ms, "{phases:?}");
        assert_eq!(phases[1].detail.as_deref(), Some("3 entries"));
        assert!(total >= phases[0].ms + phases[1].ms);
        // Taken once: the clock ended.
        assert!(take().is_none());
        // Another thread's arm never sees this one's clock.
        begin();
        std::thread::spawn(|| {
            mark("elsewhere");
            assert!(take().is_none());
        })
        .join()
        .unwrap();
        assert!(take().unwrap().0.is_empty());
    }
}
