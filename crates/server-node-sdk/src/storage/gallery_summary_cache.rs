use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use super::{GalleryIndexMediaFilter, GalleryIndexMediaSummary};

/// Identifies one gallery map "scope" whose whole-library summary (total entry count and media
/// breakdown, independent of the current viewport) can be cached and refreshed independently of
/// the viewport-bounded cluster query.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct GallerySummaryScope {
    pub(crate) prefix: String,
    pub(crate) depth: usize,
    pub(crate) media_filter: GalleryIndexMediaFilter,
}

/// Client-visible status of a scope's background summary refresh, so a caller can show that the
/// numbers it just received may be a little behind and an update is on its way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct GallerySummaryRefreshStatus {
    pub(crate) refreshing: bool,
    /// Best-effort estimate in `0..=99`; only meaningful while `refreshing` is true. A refresh
    /// that just started or whose scope has never completed once has no estimate to report.
    pub(crate) progress_percent: Option<u8>,
}

#[derive(Debug, Clone)]
pub(crate) struct GallerySummaryCacheValue {
    pub(crate) history_id: String,
    pub(crate) revision: u64,
    pub(crate) total_entry_count: usize,
    pub(crate) media_summary: GalleryIndexMediaSummary,
}

/// Shared handle a background refresh uses to publish coarse progress while it works, and to
/// let other requests for the same scope see that a refresh is already under way.
#[derive(Debug, Default)]
struct GallerySummaryRefreshTracker {
    running: AtomicBool,
    percent: AtomicU8,
}

impl GallerySummaryRefreshTracker {
    fn snapshot(&self) -> GallerySummaryRefreshStatus {
        let refreshing = self.running.load(Ordering::Acquire);
        GallerySummaryRefreshStatus {
            refreshing,
            progress_percent: refreshing.then(|| self.percent.load(Ordering::Relaxed)),
        }
    }
}

/// Lets a background refresh report coarse progress without holding any cache lock.
#[derive(Debug, Clone)]
pub(crate) struct GallerySummaryProgress(Arc<GallerySummaryRefreshTracker>);

impl GallerySummaryProgress {
    /// `percent` is clamped below 100 because the refresh is only actually "done" once the
    /// cache is updated and the tracker is retired; a stray 100 would read as "refreshing but
    /// finished", which is not a state callers should see.
    pub(crate) fn report(&self, percent: u8) {
        self.0.percent.store(percent.min(99), Ordering::Relaxed);
    }
}

/// Caches the whole-scope gallery map summary (total entry count + media breakdown) that the
/// `map/clusters` endpoint used to recompute with an unbounded aggregate query on every single
/// viewport pan/zoom. A cached value is served immediately even once stale; staleness only
/// triggers a single background refresh per scope, never a synchronous recompute on the request
/// path. The very first request for a scope still pays for one synchronous computation, since
/// there is nothing to serve from cache yet.
pub(crate) struct GallerySummaryCache {
    values: Mutex<HashMap<GallerySummaryScope, GallerySummaryCacheValue>>,
    trackers: Mutex<HashMap<GallerySummaryScope, Arc<GallerySummaryRefreshTracker>>>,
}

impl GallerySummaryCache {
    pub(crate) fn new() -> Self {
        Self {
            values: Mutex::new(HashMap::new()),
            trackers: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn cached(&self, scope: &GallerySummaryScope) -> Option<GallerySummaryCacheValue> {
        self.values.lock().unwrap().get(scope).cloned()
    }

    pub(crate) fn store(&self, scope: GallerySummaryScope, value: GallerySummaryCacheValue) {
        self.values.lock().unwrap().insert(scope, value);
    }

    pub(crate) fn status(&self, scope: &GallerySummaryScope) -> GallerySummaryRefreshStatus {
        self.trackers
            .lock()
            .unwrap()
            .get(scope)
            .map(|tracker| tracker.snapshot())
            .unwrap_or_default()
    }

    /// Claims the right to refresh `scope` in the background. Returns `None` if another task is
    /// already refreshing this scope, so callers never run two refreshes for the same scope
    /// concurrently.
    pub(crate) fn try_start_refresh(
        &self,
        scope: &GallerySummaryScope,
    ) -> Option<GallerySummaryProgress> {
        let mut trackers = self.trackers.lock().unwrap();
        if let Some(existing) = trackers.get(scope)
            && existing.running.load(Ordering::Acquire)
        {
            return None;
        }
        let tracker = Arc::new(GallerySummaryRefreshTracker::default());
        tracker.running.store(true, Ordering::Release);
        trackers.insert(scope.clone(), tracker.clone());
        Some(GallerySummaryProgress(tracker))
    }

    /// Marks a claimed refresh as finished, regardless of whether it succeeded. Must be called
    /// exactly once for every `try_start_refresh` that returned `Some`.
    pub(crate) fn finish_refresh(&self, scope: &GallerySummaryScope) {
        if let Some(tracker) = self.trackers.lock().unwrap().get(scope) {
            tracker.running.store(false, Ordering::Release);
        }
    }
}
