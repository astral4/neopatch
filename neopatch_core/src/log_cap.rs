//! Bounded log-site counter.

use std::num::NonZero;
use std::sync::atomic::{AtomicU32, Ordering};

/// Gates a log site so events from it don't flood the log. The first `limit` calls to `tick`
/// return `Some(n)` (0-indexed); subsequent calls return `None`.
pub(crate) struct LogCap {
    count: AtomicU32,
    limit: NonZero<u32>,
}

impl LogCap {
    #[must_use]
    pub(crate) const fn new(limit: NonZero<u32>) -> Self {
        Self {
            count: AtomicU32::new(0),
            limit,
        }
    }

    pub(crate) fn tick(&self) -> Option<u32> {
        // Early-return via `load` introduces a race window, but the window
        // can leak at most one extra increment past the limit, which is harmless.
        let limit = self.limit.get();
        if self.count.load(Ordering::Relaxed) >= limit {
            return None;
        }
        let n = self.count.fetch_add(1, Ordering::Relaxed);
        if n < limit { Some(n) } else { None }
    }
}
