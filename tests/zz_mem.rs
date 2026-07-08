#[test]
fn measure_rank_cost() {
    let db = "/Users/michael.parcewski/.wicked/sources/AutoGOT/.wicked/code-graph.db";
    if !std::path::Path::new(db).exists() {
        eprintln!("skip");
        return;
    }
    let t = std::time::Instant::now();
    let top = wicked_core::rank_symbols(db, 14).expect("rank");
    eprintln!(
        "RANKDONE rank_symbols(14) AutoGOT: {} results in {:?}",
        top.len(),
        t.elapsed()
    );
}
