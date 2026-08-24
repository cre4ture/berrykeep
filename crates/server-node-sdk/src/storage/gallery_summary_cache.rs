use std::collections::{HashMap, VecDeque};
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
    values: Mutex<GallerySummaryCacheValues>,
    trackers: Mutex<HashMap<GallerySummaryScope, Arc<GallerySummaryRefreshTracker>>>,
}

/// The gallery controls expose one scope at a time and only offer a small fixed set of media
/// filters. This LRU prevents request parameters from growing the process memory without bound
/// while retaining the scopes a user is most likely to revisit.
const GALLERY_SUMMARY_CACHE_MAX_SCOPES: usize = 64;

#[derive(Default)]
struct GallerySummaryCacheValues {
    entries: HashMap<GallerySummaryScope, GallerySummaryCacheValue>,
    least_recently_used: VecDeque<GallerySummaryScope>,
}

impl GallerySummaryCacheValues {
    fn touch(&mut self, scope: &GallerySummaryScope) {
        if let Some(index) = self
            .least_recently_used
            .iter()
            .position(|existing| existing == scope)
        {
            self.least_recently_used.remove(index);
        }
        self.least_recently_used.push_back(scope.clone());
    }

    fn evict_excess_entries(&mut self) {
        while self.entries.len() > GALLERY_SUMMARY_CACHE_MAX_SCOPES {
            let Some(scope) = self.least_recently_used.pop_front() else {
                break;
            };
            self.entries.remove(&scope);
        }
    }
}

impl GallerySummaryCache {
    pub(crate) fn new() -> Self {
        Self {
            values: Mutex::new(GallerySummaryCacheValues::default()),
            trackers: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn cached(&self, scope: &GallerySummaryScope) -> Option<GallerySummaryCacheValue> {
        let mut values = self.values.lock().unwrap();
        let value = values.entries.get(scope).cloned();
        if value.is_some() {
            values.touch(scope);
        }
        value
    }

    pub(crate) fn store(&self, scope: GallerySummaryScope, value: GallerySummaryCacheValue) {
        let mut values = self.values.lock().unwrap();
        values.entries.insert(scope.clone(), value);
        values.touch(&scope);
        values.evict_excess_entries();
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
        if let Some(tracker) = self.trackers.lock().unwrap().remove(scope) {
            tracker.running.store(false, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(index: usize) -> GallerySummaryScope {
        GallerySummaryScope {
            prefix: format!("gallery/{index}"),
            depth: 1,
            media_filter: GalleryIndexMediaFilter::All,
        }
    }

    fn value(index: usize) -> GallerySummaryCacheValue {
        GallerySummaryCacheValue {
            history_id: "history".to_string(),
            revision: index as u64,
            total_entry_count: index,
            media_summary: GalleryIndexMediaSummary::default(),
        }
    }

    #[test]
    fn bounds_scopes_and_evicts_the_least_recently_used_value() {
        let cache = GallerySummaryCache::new();
        for index in 0..=GALLERY_SUMMARY_CACHE_MAX_SCOPES {
            cache.store(scope(index), value(index));
        }

        assert!(cache.cached(&scope(0)).is_none());
        assert_eq!(
            cache
                .cached(&scope(GALLERY_SUMMARY_CACHE_MAX_SCOPES))
                .expect("newest scope should remain cached")
                .revision,
            GALLERY_SUMMARY_CACHE_MAX_SCOPES as u64
        );
    }

    #[test]
    fn retires_refresh_trackers_when_the_refresh_finishes() {
        let cache = GallerySummaryCache::new();
        let scope = scope(0);
        assert!(cache.try_start_refresh(&scope).is_some());
        assert!(cache.status(&scope).refreshing);

        cache.finish_refresh(&scope);

        assert!(!cache.status(&scope).refreshing);
        assert!(cache.try_start_refresh(&scope).is_some());
    }
}
