use super::*;

fn run(name: &str, exit_code: i32, duration_ms: u64) -> CheckRun {
    CheckRun {
        failures_complete_in: None,
        name: name.into(),
        argv: vec!["sh".into()],
        source: "t".into(),
        exit_code: Some(exit_code),
        timed_out: false,
        spawn_error: None,
        duration_ms,
        stdout_tail: String::new(),
        stderr_tail: String::new(),
        bound_s: 1,
        bound_note: None,
        failure_ids: Vec::new(),
        flaky_ids: Vec::new(),
        scope: None,
        retry: None,
        self_retry: false,
        classification: None,
        pre_existing: Vec::new(),
        regressions: Vec::new(),
        base: None,
        env_cannot_run: Vec::new(),
        reruns: Vec::new(),
        earlier_passes: Vec::new(),
        selection: None,
        decision: None,
        logs: Vec::new(),
        load_band: None,
        reused_from: None,
        left_outputs: Vec::new(),
        delta: None,
    }
}

#[test]
fn the_summary_counts_every_pass_the_verdict_cost() {
    let mut r = run("test_targeted", 1, 1_977_600);
    assert_eq!(r.other_passes(), "", "a single pass adds nothing");
    assert_eq!(r.summary(), "test_targeted: exit 1 (1977.6s)");

    r.reruns.push(run("test_targeted", 1, 1_890_200));
    r.base = Some(Box::new(BaseRun {
        head: "562a49e".into(),
        cached: false,
        run: Some(run("test_targeted", 1, 1_900_000)),
        error: None,
    }));
    assert_eq!(
        r.summary(),
        "test_targeted: exit 1 (1977.6s; + re-run 1890.2s, base 1900.0s = 5767.8s over 3 passes)"
    );

    // A failed `affected` pre-pass that handed over to the full check counts too (review on #1190).
    let mut full = run("test", 0, 600_000);
    full.earlier_passes.push(("affected".into(), 1_800_000));
    assert_eq!(
        full.summary(),
        "test: exit 0 (600.0s; + affected 1800.0s = 2400.0s over 2 passes)"
    );

    // A base read back from the cache cost this floor nothing.
    r.base.as_mut().unwrap().cached = true;
    assert_eq!(
        r.other_passes(),
        "; + re-run 1890.2s = 3867.8s over 2 passes"
    );
}

#[test]
fn a_floor_slot_records_each_pass_and_names_the_latest() {
    let now: FloorNow = Default::default();
    assert_eq!(
        now.lock().unwrap().describe(),
        None,
        "nothing before a pass"
    );

    // Outside a slot, a pass is not recorded anywhere.
    note_pass("test", Tree::Head);
    assert_eq!(now.lock().unwrap().describe(), None);

    let got = with_floor_now(&now, || {
        note_pass("test", Tree::Head);
        let first = now.lock().unwrap().describe();
        note_pass("test", Tree::Base);
        let base = now.lock().unwrap().describe();
        note_pass("test", Tree::Head);
        let again = now.lock().unwrap().describe();
        (first, base, again)
    });
    assert_eq!(
        got.0.as_deref(),
        Some("now `test` on the change (pass 1 of `test`), 0 min in")
    );
    assert_eq!(
        got.1.as_deref(),
        Some("now `test` on the base (pass 2 of `test`), 0 min in")
    );
    assert_eq!(
        got.2.as_deref(),
        Some("now `test` on the change again (pass 3 of `test`), 0 min in")
    );

    // A guide probe under the check's name is not a pass of it (review on #1190).
    let probed = with_floor_now(&now, || {
        as_probe(|| note_pass("test", Tree::Head));
        now.lock().unwrap().describe()
    });
    assert_eq!(
        probed.as_deref(),
        Some("now `test` on the change again (pass 3 of `test`), 0 min in"),
        "the probe added no pass"
    );

    // The slot is removed when the work returns: a later pass is not recorded into it.
    note_pass("lint", Tree::Head);
    assert!(now.lock().unwrap().describe().unwrap().contains("`test`"));
}
