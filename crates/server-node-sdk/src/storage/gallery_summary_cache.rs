use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use super::{GalleryIndexMediaFilter, GalleryIndexMediaSummary, GalleryLabelFilter};

/// Identifies one gallery map "scope" whose whole-library summary (total entry count and media
/// breakdown, independent of the current viewport) can be cached and refreshed independently of
/// the viewport-bounded cluster query.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct GallerySummaryScope {
    pub(crate) prefix: String,
    pub(crate) depth: usize,
    pub(crate) media_filter: GalleryIndexMediaFilter,
    pub(crate) captured_from_unix: Option<u64>,
    pub(crate) captured_until_unix: Option<u64>,
    /// The label filter is part of the aggregate's identity: a cached
    /// unfiltered summary must never be reused for a restricted map view.
    pub(crate) label_filter: GalleryLabelFilter,
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

/// The gallery controls expose one scope at a time. Media and privacy-label filters have a fixed
/// vocabulary, while capture-date ranges are user-selected. Keep the two classes in separate LRU
/// partitions so walking many arbitrary date ranges cannot evict hot unfiltered scopes.
const GALLERY_SUMMARY_CACHE_MAX_FIXED_SCOPES: usize = 64;
const GALLERY_SUMMARY_CACHE_MAX_CAPTURE_SCOPES: usize = 16;

#[derive(Default)]
struct GallerySummaryCacheValues {
    entries: HashMap<GallerySummaryScope, GallerySummaryCacheValue>,
    fixed_least_recently_used: VecDeque<GallerySummaryScope>,
    capture_least_recently_used: VecDeque<GallerySummaryScope>,
}

impl GallerySummaryCacheValues {
    fn touch(&mut self, scope: &GallerySummaryScope) {
        let least_recently_used =
            if scope.captured_from_unix.is_some() || scope.captured_until_unix.is_some() {
                &mut self.capture_least_recently_used
            } else {
                &mut self.fixed_least_recently_used
            };
        if let Some(index) = least_recently_used
            .iter()
            .position(|existing| existing == scope)
        {
            least_recently_used.remove(index);
        }
        least_recently_used.push_back(scope.clone());
    }

    fn evict_excess_entries(&mut self) {
        while self.fixed_least_recently_used.len() > GALLERY_SUMMARY_CACHE_MAX_FIXED_SCOPES {
            let scope = self
                .fixed_least_recently_used
                .pop_front()
                .expect("fixed gallery summary LRU exceeded its capacity");
            self.entries.remove(&scope);
        }
        while self.capture_least_recently_used.len() > GALLERY_SUMMARY_CACHE_MAX_CAPTURE_SCOPES {
            let scope = self
                .capture_least_recently_used
                .pop_front()
                .expect("capture-filtered gallery summary LRU exceeded its capacity");
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
            captured_from_unix: None,
            captured_until_unix: None,
            label_filter: GalleryLabelFilter::default(),
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

    fn capture_scope(index: usize) -> GallerySummaryScope {
        let mut scope = scope(index);
        scope.captured_from_unix = Some(index as u64);
        scope.captured_until_unix = Some(index as u64 + 1);
        scope
    }

    #[test]
    fn bounds_fixed_scopes_and_evicts_the_least_recently_used_value() {
        let cache = GallerySummaryCache::new();
        for index in 0..=GALLERY_SUMMARY_CACHE_MAX_FIXED_SCOPES {
            cache.store(scope(index), value(index));
        }

        assert!(cache.cached(&scope(0)).is_none());
        assert_eq!(
            cache
                .cached(&scope(GALLERY_SUMMARY_CACHE_MAX_FIXED_SCOPES))
                .expect("newest scope should remain cached")
                .revision,
            GALLERY_SUMMARY_CACHE_MAX_FIXED_SCOPES as u64
        );
    }

    #[test]
    fn capture_ranges_cannot_evict_unfiltered_scopes() {
        let cache = GallerySummaryCache::new();
        cache.store(scope(0), value(0));
        for index in 0..=GALLERY_SUMMARY_CACHE_MAX_CAPTURE_SCOPES {
            cache.store(capture_scope(index), value(index));
        }

        assert!(cache.cached(&scope(0)).is_some());
        assert!(cache.cached(&capture_scope(0)).is_none());
        assert!(
            cache
                .cached(&capture_scope(GALLERY_SUMMARY_CACHE_MAX_CAPTURE_SCOPES))
                .is_some()
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
