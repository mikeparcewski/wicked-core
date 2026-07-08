//! Sortable task ids — a tiny stand-in for ULID kept dependency-free.
//!
//! The task id is a sortable string. We encode `<millis_since_epoch>-<process_counter>`
//! in a lexicographically sortable, zero-padded form. Monotonic within a process even when
//! two ids are minted in the same millisecond (the counter breaks the tie).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Mint a new sortable task id, e.g. `01718000000000-0000000000000042`.
pub fn new_task_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    // 14 digits covers millis well past year 5000; 16 digits for the counter.
    format!("{millis:014}-{seq:016}")
}
