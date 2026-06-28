//! Thin operator CLI over the COE library — the replacement entry point for the retired
//! `wicked-agent` binary. All composition lives in `wicked_core`; this is just argv + printing.
//!
//!   wicked-core status                          # list sessions + units on the store
//!   wicked-core launch --problem "Do X. Do Y"   # run a governed session, streaming events
//!   [--db <path>]                               # else $WICKED_ESTATE_DB, else ./wicked-estate.db

use std::time::Duration;
use wicked_core::{registry_roster, Core, CoreEvent, EntityMode, LaunchSpec};

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn store_path(args: &[String]) -> String {
    flag(args, "--db")
        .or_else(|| {
            std::env::var("WICKED_ESTATE_DB")
                .ok()
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "wicked-estate.db".to_string())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let core = Core::spawn(store_path(&args));

    match args.get(1).map(String::as_str) {
        Some("status") => match core.sessions_detail() {
            Ok(views) if views.is_empty() => println!("(no sessions)"),
            Ok(views) => {
                for v in views {
                    let done = v
                        .units
                        .iter()
                        .filter(|u| matches!(u.status, wicked_core::UnitStatus::Done))
                        .count();
                    println!(
                        "{} [{:?}] {}/{} units done",
                        v.session.id,
                        v.session.status,
                        done,
                        v.units.len()
                    );
                }
            }
            Err(e) => {
                eprintln!("status failed: {e}");
                std::process::exit(1);
            }
        },
        Some("launch") => {
            let Some(problem) = flag(&args, "--problem") else {
                eprintln!("launch requires --problem \"...\"");
                std::process::exit(2);
            };
            let roster = registry_roster();
            let events = core.subscribe();
            let sid = core.launch(LaunchSpec {
                problem,
                clis: roster,
                entity_mode: EntityMode::Shared,
                session_id: String::new(),
            });
            println!("launched {sid}");
            loop {
                match events.recv_timeout(Duration::from_secs(300)) {
                    Ok(ev) => {
                        println!("  {ev:?}");
                        if matches!(
                            ev,
                            CoreEvent::SessionCompleted { .. } | CoreEvent::Error { .. }
                        ) {
                            break;
                        }
                    }
                    Err(_) => {
                        eprintln!("timed out waiting for the session to finish");
                        std::process::exit(1);
                    }
                }
            }
        }
        _ => {
            eprintln!("usage: wicked-core <status | launch --problem \"...\"> [--db <path>]");
            std::process::exit(2);
        }
    }
}
