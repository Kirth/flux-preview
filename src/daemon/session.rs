use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// The id of the file currently being previewed (ZAP §3).
///
/// Bumped on every show/step/hide. Every async hop checks it (before work, after decoding,
/// on arrival in the UI) and drops its result when it no longer matches, so holding an
/// arrow key never queues up stale decodes or flashes old content.
#[derive(Clone, Default)]
pub struct Session(Arc<AtomicU64>);

impl Session {
    pub fn bump(&self) -> u64 {
        self.0.fetch_add(1, Ordering::AcqRel) + 1
    }

    pub fn is_current(&self, id: u64) -> bool {
        self.0.load(Ordering::Acquire) == id
    }
}
