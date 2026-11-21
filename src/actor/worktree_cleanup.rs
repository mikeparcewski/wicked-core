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
//!
//! Each cancel's record carries a `cancel_id` its continuation echoes, so the outcome of an earlier
//! cancel of the same run id (cancel, relaunch, cancel again inside the first reap) never resolves
//! the later cancel's record. The outcome is said at least once: a stop between the event and the
//! record's resolve says it again at the next boot, and the per-run event log
//! (`crate::event_log`) keeps every emitted event for a consumer that was not subscribed.

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
    /// Which cancel of `run_id` this is: its continuation names it, and only the matching record
    /// is resolved.
    #[serde(default)]
    pub cancel_id: u64,
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

/// A fresh `cancel_id`: the wall clock in nanoseconds. Two cancels of one run id are a relaunch
/// apart, never inside one nanosecond.
pub(crate) fn new_cancel_id() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or_default()
}

/// Off the actor: wait out the grace of the tool groups a cancel SIGTERMed (`signalled`) and
/// SIGKILL the survivors, then reap `cleanup`'s tree (when there is one) in the place `ticket`
/// holds in the tree's queue. The tree is never inspected or removed while a signalled child may
/// still write to it. The outcome comes back as [`Command::WorktreeCleanupDone`].
pub(crate) fn start(
    self_tx: &Sender<Command>,
    signalled: Vec<i32>,
    reap: Option<(crate::repo::TreeTicket, &PendingCleanup)>,
) {
    if signalled.is_empty() && reap.is_none() {
        return;
    }
    let reap = reap.map(|(ticket, c)| (ticket, c.repo_root.clone(), c.run_id.clone(), c.cancel_id));
    let tx = self_tx.clone();
    crate::repo::spawn_bound(move || {
        #[cfg(unix)]
        {
            if !signalled.is_empty() {
                let _ = crate::validator::escalate_groups_after_term(&signalled);
            }
        }
        #[cfg(not(unix))]
        let _ = signalled;
        let Some((ticket, root, rid, cancel_id)) = reap else {
            return;
        };
        let reaped = crate::repo::reap_worktree_with(ticket, &root, &rid);
        // An actor that stopped meanwhile never hears it: the record stays pending, and the next
        // boot reaps again (`resume_pending`).
        let _ = tx.send(Command::WorktreeCleanupDone {
            run_id: rid,
            cancel_id,
            reaped,
        });
    });
}

/// On the actor: the reap of `run_id`'s tree for its cancel `cancel_id` ended. A kept tree is said
/// (`worktreeRetained`) while the run is still that cancel's (not relaunched since), then the
/// record is resolved. A record already resolved, or one a later cancel wrote, is left as it is.
pub(crate) fn done(
    store: &mut dyn GraphStore,
    subscribers: &mut crate::event_log::EventSink,
    run_id: &str,
    cancel_id: u64,
    reaped: bool,
) {
    let mut cleanup = match get(&*store, run_id) {
        Ok(Some(c)) if c.pending && c.cancel_id == cancel_id => c,
        Ok(_) => return,
        Err(e) => {
            eprintln!("wicked-core: run {run_id}'s worktree cleanup record could not be read: {e}");
            return;
        }
    };
    let still_cancelled = !matches!(
        crate::domain::get_session(&*store, run_id),
        Ok(Some(s)) if s.status != SessionStatus::Cancelled
    );
    if !reaped && still_cancelled {
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
/// end of is reaped again, off the actor, through the same continuation. Returns the run ids whose
/// trees the boot reaper must keep its hands off, or `None` when the records cannot be read (the
/// boot reaper then skips its pass: it could force-remove a tree a pending cleanup protects).
///
/// - The run is still `Cancelled`, or its session is gone: reaped again (clean-only) and said.
/// - The run's session cannot be read: left pending, and its tree is kept for this boot.
/// - The run was relaunched since (any other status): resolved without a word. Its tree is the
///   new run's, and that run's own end applies its own retention.
pub(crate) fn resume_pending(
    store: &mut dyn GraphStore,
    self_tx: &Sender<Command>,
) -> Option<HashSet<String>> {
    let mut handed = HashSet::new();
    let records = match all_pending(&*store) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("wicked-core: pending worktree cleanups could not be read at boot: {e}");
            return None;
        }
    };
    for mut cleanup in records {
        match crate::domain::get_session(&*store, &cleanup.run_id) {
            Ok(Some(s)) if s.status != SessionStatus::Cancelled => {
                cleanup.pending = false;
                let _ = crate::domain::put_node(store, cleanup.to_node());
                continue;
            }
            Err(e) => {
                eprintln!(
                    "wicked-core: run {}'s session could not be read at boot ({e}); its pending \
                     worktree cleanup waits for the next boot",
                    cleanup.run_id
                );
                handed.insert(cleanup.run_id.clone());
                continue;
            }
            Ok(_) => {}
        }
        eprintln!(
            "wicked-core: run {}'s worktree cleanup did not finish before the last stop; reaping \
             it again (core#1434)",
            cleanup.run_id
        );
        let ticket = crate::repo::reserve_tree(&cleanup.repo_root, &cleanup.run_id);
        start(self_tx, Vec::new(), Some((ticket, &cleanup)));
        handed.insert(cleanup.run_id.clone());
    }
    Some(handed)
}
