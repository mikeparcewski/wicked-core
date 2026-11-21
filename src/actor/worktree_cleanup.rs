//! (core#1434) A cancel's worktree cleanup stays on the record until its outcome is said.
//!
//! A cancel reaps its run's worktree off the actor (core#1429), and since core#1409 it does not
//! wait for the reap at all: the outcome (the tree went, or it was kept because it holds
//! uncommitted work, which `worktreeRetained` says) is posted back as
//! [`Command::WorktreeCleanupDone`] once the reap ends, after `runCancelled`. A posted command
//! alone is lost when the actor stops first (the last `Core` handle dropped, a daemon restart),
//! and the operator never learns the tree was kept.
//!
//! So the cancel writes a [`PendingCleanup`] in the same batch as the `Cancelled` session. The
//! continuation ([`done`]) says the outcome and resolves the record. The next boot
//! ([`resume_pending`]) reaps every record still pending, off the actor and through the same
//! continuation, and keeps those trees out of the boot reaper's synchronous pass.

use std::collections::HashSet;
use std::sync::mpsc::Sender;

use serde::{Deserialize, Serialize};
use wicked_apps_core::{
    synthetic_symbol, GraphRead, GraphStore, Language, Location, Node, NodeKind, Span,
    SYMBOL_SCHEME,
};
use wicked_estate_core::SymbolQuery;

use crate::command::Command;
use crate::domain::SessionStatus;
use crate::event::CoreEvent;

/// The node kind of a cancel's cleanup record.
pub(crate) const WORKTREE_CLEANUP: &str = "worktree_cleanup";

/// One cancel's worktree cleanup: what to reap, and what to say when the tree is kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PendingCleanup {
    pub run_id: String,
    pub repo_root: String,
    /// The kept tree's path, as `worktreeRetained` names it.
    pub path: String,
    /// Why the tree is kept, as `worktreeRetained` says it.
    pub reason: String,
    /// `true` until the outcome is said.
    pub pending: bool,
}

impl PendingCleanup {
    pub(crate) fn to_node(&self) -> Node {
        let mut node = Node::new(
            synthetic_symbol(WORKTREE_CLEANUP, &self.run_id),
            NodeKind::Other(WORKTREE_CLEANUP.to_string()),
            self.run_id.clone(),
            Language::new(SYMBOL_SCHEME),
            Location::new(format!("{WORKTREE_CLEANUP}/{}", self.run_id), Span::ZERO),
        );
        if let Ok(serde_json::Value::Object(map)) = serde_json::to_value(self) {
            node.metadata = map;
        }
        node
    }

    fn from_node(node: &Node) -> Option<Self> {
        match &node.kind {
            NodeKind::Other(k) if k == WORKTREE_CLEANUP => {}
            _ => return None,
        }
        serde_json::from_value(serde_json::Value::Object(node.metadata.clone())).ok()
    }

    /// The `worktreeRetained` this cleanup says when its tree is kept.
    pub(crate) fn retained_event(&self) -> CoreEvent {
        CoreEvent::WorktreeRetained {
            session: self.run_id.clone(),
            path: self.path.clone(),
            reason: self.reason.clone(),
        }
    }
}

/// The cleanup record of `run_id`, pending or resolved.
pub(crate) fn get(store: &dyn GraphRead, run_id: &str) -> anyhow::Result<Option<PendingCleanup>> {
    Ok(store
        .get_node(&synthetic_symbol(WORKTREE_CLEANUP, run_id))?
        .as_ref()
        .and_then(PendingCleanup::from_node))
}

/// Whether `run_id`'s cancel cleanup is still going (its outcome not said yet).
pub(crate) fn is_pending(store: &dyn GraphRead, run_id: &str) -> anyhow::Result<bool> {
    Ok(get(store, run_id)?.is_some_and(|c| c.pending))
}

/// Every cleanup whose outcome was never said.
fn all_pending(store: &dyn GraphRead) -> anyhow::Result<Vec<PendingCleanup>> {
    let query = SymbolQuery {
        kinds: vec![NodeKind::Other(WORKTREE_CLEANUP.to_string())],
        ..Default::default()
    };
    Ok(store
        .find_symbols(&query)?
        .iter()
        .filter_map(PendingCleanup::from_node)
        .filter(|c| c.pending)
        .collect())
}

/// Reap `cleanup`'s tree off the actor, in the place `ticket` holds in the tree's queue; the
/// outcome comes back as [`Command::WorktreeCleanupDone`].
pub(crate) fn start(
    self_tx: &Sender<Command>,
    ticket: crate::repo::TreeTicket,
    cleanup: &PendingCleanup,
) {
    let (root, rid, tx) = (
        cleanup.repo_root.clone(),
        cleanup.run_id.clone(),
        self_tx.clone(),
    );
    crate::repo::spawn_bound(move || {
        let reaped = crate::repo::reap_worktree_with(ticket, &root, &rid);
        // An actor that stopped meanwhile never hears it: the record stays pending, and the next
        // boot reaps again (`resume_pending`).
        let _ = tx.send(Command::WorktreeCleanupDone {
            run_id: rid,
            reaped,
        });
    });
}

/// On the actor: the reap of `run_id`'s tree ended. A kept tree is said (`worktreeRetained`), then
/// the record is resolved. A record already resolved says nothing again.
pub(crate) fn done(
    store: &mut dyn GraphStore,
    subscribers: &mut crate::event_log::EventSink,
    run_id: &str,
    reaped: bool,
) {
    let mut cleanup = match get(&*store, run_id) {
        Ok(Some(c)) if c.pending => c,
        Ok(_) => return,
        Err(e) => {
            eprintln!("wicked-core: run {run_id}'s worktree cleanup record could not be read: {e}");
            return;
        }
    };
    if !reaped {
        subscribers.emit(cleanup.retained_event());
    }
    cleanup.pending = false;
    if let Err(e) = crate::domain::put_node(store, cleanup.to_node()) {
        eprintln!(
            "wicked-core: run {run_id}'s worktree cleanup could not be marked done ({e}); the \
             next boot reaps the tree again"
        );
    }
}

/// At boot, before the orphan reaper: every cleanup an earlier life of this daemon never heard the
/// end of is reaped again, off the actor, through the same continuation. Returns the run ids
/// handed off; the boot reaper keeps its hands off those trees. A record whose run is no longer
/// terminal (relaunched since) is resolved without a word: its tree is the new run's.
pub(crate) fn resume_pending(
    store: &mut dyn GraphStore,
    self_tx: &Sender<Command>,
) -> HashSet<String> {
    let mut handed = HashSet::new();
    let records = match all_pending(&*store) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("wicked-core: pending worktree cleanups could not be read at boot: {e}");
            return handed;
        }
    };
    for mut cleanup in records {
        let terminal = matches!(
            crate::domain::get_session(&*store, &cleanup.run_id),
            Ok(Some(s)) if matches!(
                s.status,
                SessionStatus::Cancelled | SessionStatus::Completed | SessionStatus::Failed
            )
        );
        if !terminal {
            cleanup.pending = false;
            let _ = crate::domain::put_node(store, cleanup.to_node());
            continue;
        }
        eprintln!(
            "wicked-core: run {}'s worktree cleanup did not finish before the last stop; reaping \
             it again (core#1434)",
            cleanup.run_id
        );
        let ticket = crate::repo::reserve_tree(&cleanup.repo_root, &cleanup.run_id);
        start(self_tx, ticket, &cleanup);
        handed.insert(cleanup.run_id.clone());
    }
    handed
}
