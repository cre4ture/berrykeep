use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

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

impl GallerySummaryScope {
    fn is_capture_filtered(&self) -> bool {
        self.captured_from_unix.is_some() || self.captured_until_unix.is_some()
    }
}

#[derive(Debug)]
pub(crate) struct GalleryCaptureSummaryBusyError;

impl std::fmt::Display for GalleryCaptureSummaryBusyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("too many capture-filtered gallery summaries are being computed")
    }
}

impl std::error::Error for GalleryCaptureSummaryBusyError {}

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
#[derive(Debug)]
pub(crate) struct GallerySummaryProgress {
    tracker: Arc<GallerySummaryRefreshTracker>,
    _capture_refresh_permit: Option<OwnedSemaphorePermit>,
}

impl GallerySummaryProgress {
    /// `percent` is clamped below 100 because the refresh is only actually "done" once the
    /// cache is updated and the tracker is retired; a stray 100 would read as "refreshing but
    /// finished", which is not a state callers should see.
    pub(crate) fn report(&self, percent: u8) {
        self.tracker
            .percent
            .store(percent.min(99), Ordering::Relaxed);
    }
}

#[derive(Debug)]
pub(crate) struct GallerySummaryComputationPermit {
    _capture_miss_permit: Option<OwnedSemaphorePermit>,
    scope: GallerySummaryScope,
    in_flight_misses: Arc<Mutex<HashMap<GallerySummaryScope, Arc<Notify>>>>,
    completion: Arc<Notify>,
}

impl Drop for GallerySummaryComputationPermit {
    fn drop(&mut self) {
        let mut in_flight_misses = self.in_flight_misses.lock().unwrap();
        if in_flight_misses
            .get(&self.scope)
            .is_some_and(|current| Arc::ptr_eq(current, &self.completion))
        {
            in_flight_misses.remove(&self.scope);
        }
        drop(in_flight_misses);
        self.completion.notify_waiters();
    }
}

/// Claims a cache-miss computation for one summary scope. Followers wait for the leader rather
/// than consuming another global capture-range permit or running a duplicate aggregate query.
#[derive(Debug)]
pub(crate) enum GallerySummaryMiss {
    Leader(GallerySummaryComputationPermit),
    Follower(Arc<Notify>),
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
    in_flight_misses: Arc<Mutex<HashMap<GallerySummaryScope, Arc<Notify>>>>,
    capture_miss_permits: Arc<Semaphore>,
    capture_refresh_permits: Arc<Semaphore>,
}

/// The gallery controls expose one scope at a time. Media and privacy-label filters have a fixed
/// vocabulary, while capture-date ranges are user-selected. Keep the two classes in separate LRU
/// partitions so walking many arbitrary date ranges cannot evict hot unfiltered scopes.
const GALLERY_SUMMARY_CACHE_MAX_FIXED_SCOPES: usize = 64;
const GALLERY_SUMMARY_CACHE_MAX_CAPTURE_SCOPES: usize = 64;
const GALLERY_SUMMARY_MAX_CAPTURE_MISSES: usize = 2;
const GALLERY_SUMMARY_MAX_CAPTURE_REFRESHES: usize = 1;

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
            in_flight_misses: Arc::new(Mutex::new(HashMap::new())),
            capture_miss_permits: Arc::new(Semaphore::new(GALLERY_SUMMARY_MAX_CAPTURE_MISSES)),
            capture_refresh_permits: Arc::new(Semaphore::new(
                GALLERY_SUMMARY_MAX_CAPTURE_REFRESHES,
            )),
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

    /// Claims a synchronous cache-miss computation, coalescing concurrent requests for the same
    /// scope. Capture bounds are arbitrary client input, so distinct excess novel scopes are
    /// rejected instead of allowing requests to queue an unbounded number of whole-scope scans.
    pub(crate) fn try_start_summary_miss(
        &self,
        scope: &GallerySummaryScope,
    ) -> Result<GallerySummaryMiss, GalleryCaptureSummaryBusyError> {
        let mut in_flight_misses = self.in_flight_misses.lock().unwrap();
        if let Some(completion) = in_flight_misses.get(scope) {
            return Ok(GallerySummaryMiss::Follower(completion.clone()));
        }
        let capture_miss_permit = if scope.is_capture_filtered() {
            Some(
                self.capture_miss_permits
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| GalleryCaptureSummaryBusyError)?,
            )
        } else {
            None
        };
        let completion = Arc::new(Notify::new());
        in_flight_misses.insert(scope.clone(), completion.clone());
        Ok(GallerySummaryMiss::Leader(
            GallerySummaryComputationPermit {
                _capture_miss_permit: capture_miss_permit,
                scope: scope.clone(),
                in_flight_misses: self.in_flight_misses.clone(),
                completion,
            },
        ))
    }

    /// Waits only while this exact miss computation remains the leader for `scope`. Creating the
    /// notification future before checking the map avoids losing a completion that races with a
    /// follower arriving after the aggregate has already finished.
    pub(crate) async fn wait_for_summary_miss(
        &self,
        scope: &GallerySummaryScope,
        completion: &Arc<Notify>,
    ) {
        let notified = completion.notified();
        let still_running = self
            .in_flight_misses
            .lock()
            .unwrap()
            .get(scope)
            .is_some_and(|current| Arc::ptr_eq(current, completion));
        if still_running {
            notified.await;
        }
    }

    /// Claims the right to refresh `scope` in the background. Returns `None` if another task is
    /// already refreshing this scope or the capture-filtered computation limit is occupied, so
    /// callers never run duplicate or unbounded refreshes.
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
        let capture_refresh_permit = if scope.is_capture_filtered() {
            Some(
                self.capture_refresh_permits
                    .clone()
                    .try_acquire_owned()
                    .ok()?,
            )
        } else {
            None
        };
        let tracker = Arc::new(GallerySummaryRefreshTracker::default());
        tracker.running.store(true, Ordering::Release);
        trackers.insert(scope.clone(), tracker.clone());
        Some(GallerySummaryProgress {
            tracker,
            _capture_refresh_permit: capture_refresh_permit,
        })
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

    fn expect_miss_leader(miss: GallerySummaryMiss) -> GallerySummaryComputationPermit {
        match miss {
            GallerySummaryMiss::Leader(permit) => permit,
            GallerySummaryMiss::Follower(_) => panic!("new scope should elect a miss leader"),
        }
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
    fn independently_bounds_capture_misses_and_background_refreshes() {
        let cache = GallerySummaryCache::new();
        let miss_permits = (0..GALLERY_SUMMARY_MAX_CAPTURE_MISSES)
            .map(|index| {
                expect_miss_leader(
                    cache
                        .try_start_summary_miss(&capture_scope(index))
                        .expect("capture summary should be admitted below the limit"),
                )
            })
            .collect::<Vec<_>>();

        assert!(
            cache
                .try_start_summary_miss(&capture_scope(GALLERY_SUMMARY_MAX_CAPTURE_MISSES))
                .is_err()
        );
        let capture_refresh = cache
            .try_start_refresh(&capture_scope(GALLERY_SUMMARY_MAX_CAPTURE_MISSES))
            .expect("background work must not consume foreground miss permits");
        assert!(
            cache
                .try_start_refresh(&capture_scope(GALLERY_SUMMARY_MAX_CAPTURE_MISSES + 1))
                .is_none()
        );
        cache.finish_refresh(&capture_scope(GALLERY_SUMMARY_MAX_CAPTURE_MISSES));
        drop(capture_refresh);

        let next_refresh = cache
            .try_start_refresh(&capture_scope(GALLERY_SUMMARY_MAX_CAPTURE_MISSES + 1))
            .expect("released background permit should admit the next refresh");
        cache.finish_refresh(&capture_scope(GALLERY_SUMMARY_MAX_CAPTURE_MISSES + 1));
        drop(next_refresh);

        let fixed_miss = expect_miss_leader(
            cache
                .try_start_summary_miss(&scope(99))
                .expect("fixed scope misses should not use capture permits"),
        );
        let fixed_refresh = cache
            .try_start_refresh(&scope(99))
            .expect("fixed-scope refreshes should not use capture permits");
        cache.finish_refresh(&scope(99));
        drop(fixed_refresh);
        drop(fixed_miss);
        drop(miss_permits);
    }

    #[tokio::test]
    async fn coalesces_concurrent_capture_misses_by_scope() {
        let cache = GallerySummaryCache::new();
        let scope = capture_scope(0);
        let leader = expect_miss_leader(cache.try_start_summary_miss(&scope).unwrap());
        let completion = match cache.try_start_summary_miss(&scope).unwrap() {
            GallerySummaryMiss::Follower(completion) => completion,
            GallerySummaryMiss::Leader(_) => panic!("existing miss should have one leader"),
        };
        let second_scope_leader = expect_miss_leader(
            cache
                .try_start_summary_miss(&capture_scope(1))
                .expect("same-scope follower must not consume a capture permit"),
        );
        assert!(cache.try_start_summary_miss(&capture_scope(2)).is_err());

        let wait = cache.wait_for_summary_miss(&scope, &completion);
        tokio::pin!(wait);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), &mut wait)
                .await
                .is_err(),
            "the follower should wait while its scope's leader is still computing"
        );
        drop(leader);
        wait.await;
        let replacement_leader = expect_miss_leader(
            cache
                .try_start_summary_miss(&scope)
                .expect("a completed scope should accept a later cache miss"),
        );
        drop(replacement_leader);
        drop(second_scope_leader);
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
