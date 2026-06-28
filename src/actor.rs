//! The store actor: the ONE thread that owns the writable `SqliteStore`. Every command is handled
//! here, serially, so multiple in-process callers (agent, UI, MCP) never contend for the SQLite
//! writer lock or race a reader against a mid-batch write. This is the single-writer guarantee.

use crate::command::Command;
use crate::event::CoreEvent;
use crate::{pipeline, LaunchSpec};
use std::sync::mpsc::{Receiver, Sender};

use wicked_apps_core::{open_store, GraphRead, NodeKind, AGENT_SESSION};
use wicked_estate_core::SymbolQuery;

/// Run the actor loop until every [`crate::Core`] handle is dropped (the command channel closes).
/// Owns the store for its whole lifetime; nothing else opens it in-process.
pub(crate) fn run(path: String, rx: Receiver<Command>) {
    let mut store = match open_store(Some(&path)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("wicked-core: could not open store at {path}: {e}");
            return;
        }
    };
    // `store` is intentionally the only handle to the writable connection. Keep it `mut`: the P2
    // pipeline writes through it (plan/distribute/execute) on this same thread.
    let _ = &mut store;

    let mut subscribers: Vec<Sender<CoreEvent>> = Vec::new();

    while let Ok(cmd) = rx.recv() {
        match cmd {
            Command::Ping(reply) => {
                emit(&mut subscribers, CoreEvent::Heartbeat);
                let _ = reply.send(());
            }
            Command::Sessions(reply) => {
                let _ = reply.send(list_sessions(&store));
            }
            Command::Subscribe(sub) => subscribers.push(sub),
            Command::Launch(spec) => {
                let LaunchSpec {
                    problem,
                    clis,
                    entity_mode,
                    session_id,
                } = spec;
                // Runs on this (single-writer) thread, emitting CoreEvents to subscribers as it goes.
                let res = pipeline::run_session(
                    &mut store,
                    clis,
                    &problem,
                    entity_mode,
                    &session_id,
                    &mut |ev| emit(&mut subscribers, ev),
                );
                if let Err(e) = res {
                    emit(
                        &mut subscribers,
                        CoreEvent::Error {
                            session: Some(session_id),
                            message: e.to_string(),
                        },
                    );
                }
            }
        }
    }
}

/// Fan an event out to every live subscriber, dropping any whose receiver has hung up.
fn emit(subscribers: &mut Vec<Sender<CoreEvent>>, ev: CoreEvent) {
    subscribers.retain(|s| s.send(ev.clone()).is_ok());
}

/// Read the agent session ids on the store (by their session-node names).
fn list_sessions(store: &impl GraphRead) -> anyhow::Result<Vec<String>> {
    let query = SymbolQuery {
        kinds: vec![NodeKind::Other(AGENT_SESSION.to_string())],
        ..Default::default()
    };
    Ok(store
        .find_symbols(&query)?
        .into_iter()
        .map(|n| n.name)
        .collect())
}
