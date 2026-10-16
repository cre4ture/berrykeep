use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use axum::Json;
use axum::extract::{MatchedPath, Request, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use common::range_chunk_cache::RangeChunkCache;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    OperatorConfigPersistenceSource, ServerState, append_admin_audit, authorize_admin_request,
    operator_config_persistence_source, persist_cluster_nodes_state,
    update_persisted_node_enrollment_if_possible,
};

pub(crate) const MULTIMEDIA_ENABLED_LABEL: &str = "multimedia_enabled";
pub(crate) const MULTIMEDIA_ENABLED_ENV: &str = "BERRYKEEP_NODE_MULTIMEDIA_ENABLED";
pub(crate) const MULTIMEDIA_DISABLED_CODE: &str = "node_multimedia_disabled";

/// Canonical audit identities shared by handlers and the pre-extraction gate.
#[derive(Clone, Copy)]
pub(super) enum AdminMultimediaAction {
    Thumbnail,
    CacheRetry,
    MapClusters,
    MapEntries,
    MapConfigGet,
    MapConfigPut,
    MapImportStatus,
    MapImportRun,
    NaturalEarthStatus,
    NaturalEarthRun,
    OperationRun,
}

impl AdminMultimediaAction {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Thumbnail => "auth/media/thumbnail/get",
            Self::CacheRetry => "auth/media/cache/retry",
            Self::MapClusters => "auth/store/map/clusters/get",
            Self::MapEntries => "auth/store/map/cluster-entries/get",
            Self::MapConfigGet => "auth/maps/config/get",
            Self::MapConfigPut => "auth/maps/config/put",
            Self::MapImportStatus => "auth/maps/import/status",
            Self::MapImportRun => "auth/maps/import/run",
            Self::NaturalEarthStatus => "auth/maps/import/natural-earth/status",
            Self::NaturalEarthRun => "auth/maps/import/natural-earth/run",
            Self::OperationRun => "auth/operations/run",
        }
    }

    fn from_route(method: &Method, path: &str) -> Option<Self> {
        // Axum also dispatches HEAD requests through GET handlers.
        let method = if method == Method::HEAD {
            "GET"
        } else {
            method.as_str()
        };
        Some(match (method, path) {
            ("GET", "/auth/media/thumbnail") => Self::Thumbnail,
            ("POST", "/auth/media/cache/retry") => Self::CacheRetry,
            ("GET", "/auth/store/map/clusters" | "/auth/gallery/map/clusters") => Self::MapClusters,
            ("GET", "/auth/store/map/cluster-entries" | "/auth/gallery/map/cluster-entries") => {
                Self::MapEntries
            }
            ("GET", "/auth/maps/config") => Self::MapConfigGet,
            ("PUT", "/auth/maps/config") => Self::MapConfigPut,
            ("GET", "/auth/maps/import") => Self::MapImportStatus,
            ("POST", "/auth/maps/import") => Self::MapImportRun,
            ("GET", "/auth/maps/import/natural-earth") => Self::NaturalEarthStatus,
            ("POST", "/auth/maps/import/natural-earth") => Self::NaturalEarthRun,
            ("POST", "/auth/operations/{operation_id}/runs") => Self::OperationRun,
            _ => return None,
        })
    }
}

#[derive(Debug)]
pub(crate) struct MultimediaDisabled;

impl std::fmt::Display for MultimediaDisabled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("multimedia functionality is disabled on this server node")
    }
}

impl std::error::Error for MultimediaDisabled {}

/// Required by every optional whole-original recovery path. The private field
/// ensures callers obtain admission from the node's shared control.
#[derive(Debug)]
pub(crate) struct MediaSourceRecoveryPermit {
    _permit: tokio::sync::OwnedSemaphorePermit,
}

/// Shared by the request layer and every media worker, including existing clones.
#[derive(Clone)]
pub(crate) struct MultimediaControl {
    enabled: Arc<AtomicBool>,
    enabled_notification: Arc<tokio::sync::Notify>,
    source_recovery_slots: Arc<tokio::sync::Semaphore>,
    source_recovery_retries: Arc<Mutex<MediaRecoveryBackoffs>>,
    storage_only_sources: Arc<Mutex<RangeChunkCache<String, u64>>>,
}

#[derive(Clone, Copy, Default)]
struct MediaRecoveryBackoff {
    source_retry_after_unix: u64,
    holder_recheck_after_unix: u64,
}

const MEDIA_RECOVERY_BACKOFF_CAPACITY: usize = 1024;
const MAX_CONCURRENT_MEDIA_SOURCE_RECOVERIES: usize = 2;

/// Retry deadlines are admission state: active entries must never be evicted.
/// At capacity, new manifests wait until an existing entry fully expires.
#[derive(Default)]
struct MediaRecoveryBackoffs {
    entries: HashMap<String, MediaRecoveryBackoff>,
    next_expiration_unix: Option<u64>,
}

impl MediaRecoveryBackoff {
    fn expires_at(self) -> u64 {
        self.source_retry_after_unix
            .max(self.holder_recheck_after_unix)
    }
}

impl MediaRecoveryBackoffs {
    fn expire(&mut self, now_unix: u64) {
        if self
            .next_expiration_unix
            .is_some_and(|expiration| expiration <= now_unix)
        {
            self.entries
                .retain(|_, backoff| backoff.expires_at() > now_unix);
            self.next_expiration_unix = self
                .entries
                .values()
                .map(|backoff| backoff.expires_at())
                .min();
        }
    }

    fn can_admit(&mut self, manifest_hash: &str, now_unix: u64, kind: &MediaRetryKind) -> bool {
        self.expire(now_unix);
        self.entries.get(manifest_hash).map_or(
            self.entries.len() < MEDIA_RECOVERY_BACKOFF_CAPACITY,
            |backoff| match kind {
                MediaRetryKind::SourceRecovery => backoff.source_retry_after_unix <= now_unix,
                MediaRetryKind::HolderRecheck => backoff.holder_recheck_after_unix <= now_unix,
            },
        )
    }

    fn admit(&mut self, manifest_hash: &str, now_unix: u64, kind: MediaRetryKind) -> bool {
        if !self.can_admit(manifest_hash, now_unix, &kind) {
            return false;
        }
        let backoff = self.entries.entry(manifest_hash.to_string()).or_default();
        match kind {
            MediaRetryKind::SourceRecovery => {
                backoff.source_retry_after_unix = now_unix.saturating_add(10 * 60)
            }
            MediaRetryKind::HolderRecheck => {
                backoff.holder_recheck_after_unix = now_unix.saturating_add(30)
            }
        }
        let expiration = backoff.expires_at();
        // A stale earlier minimum is harmless: expire() will rescan once when
        // that boundary arrives, rather than scanning a full table on each miss.
        self.next_expiration_unix = Some(
            self.next_expiration_unix
                .map_or(expiration, |next| next.min(expiration)),
        );
        true
    }
}

enum MediaRetryKind {
    SourceRecovery,
    HolderRecheck,
}

impl Default for MultimediaControl {
    fn default() -> Self {
        Self {
            enabled: Arc::new(AtomicBool::new(true)),
            enabled_notification: Arc::new(tokio::sync::Notify::new()),
            source_recovery_slots: Arc::new(tokio::sync::Semaphore::new(
                MAX_CONCURRENT_MEDIA_SOURCE_RECOVERIES,
            )),
            source_recovery_retries: Arc::new(Mutex::new(MediaRecoveryBackoffs::default())),
            storage_only_sources: Arc::new(Mutex::new(RangeChunkCache::new(1024))),
        }
    }
}

impl MultimediaControl {
    pub(crate) fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    pub(crate) fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
        if enabled {
            self.enabled_notification.notify_waiters();
        }
    }

    /// Park a resumable worker without losing its ownership or polling the flag.
    pub(crate) async fn wait_until_enabled(&self) {
        loop {
            let notification = self.enabled_notification.notified();
            tokio::pin!(notification);
            // Register before checking the flag so a concurrent enable cannot
            // be lost between observing disabled and awaiting the notification.
            notification.as_mut().enable();
            if self.enabled() {
                return;
            }
            notification.await;
        }
    }

    pub(crate) fn ensure_enabled(&self) -> Result<()> {
        if !self.enabled() {
            return Err(MultimediaDisabled.into());
        }
        Ok(())
    }

    pub(crate) fn source_recovery_due(&self, manifest_hash: &str, now_unix: u64) -> bool {
        if !self.enabled() || self.source_recovery_slots.available_permits() == 0 {
            return false;
        }
        self.source_recovery_retries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .can_admit(manifest_hash, now_unix, &MediaRetryKind::SourceRecovery)
    }

    pub(crate) fn storage_only_source_observed(&self, manifest_hash: &str, now_unix: u64) -> bool {
        self.storage_only_sources
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&manifest_hash.to_string())
            .is_some_and(|until| *until > now_unix)
    }

    pub(crate) fn note_storage_only_source(&self, manifest_hash: &str, now_unix: u64) {
        self.storage_only_sources
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(manifest_hash.to_string(), now_unix.saturating_add(10 * 60));
    }

    pub(crate) fn admit_holder_recheck(&self, manifest_hash: &str, now_unix: u64) -> bool {
        self.admit_retry(manifest_hash, now_unix, MediaRetryKind::HolderRecheck)
    }

    /// Legacy profile hydration shares the same global limit without changing
    /// its existing retry policy.
    pub(crate) fn try_source_recovery_slot(&self) -> Result<Option<MediaSourceRecoveryPermit>> {
        self.ensure_enabled()?;
        Ok(self
            .source_recovery_slots
            .clone()
            .try_acquire_owned()
            .ok()
            .map(|permit| MediaSourceRecoveryPermit { _permit: permit }))
    }

    /// Bound transfer and derivation together without queueing gallery requests.
    /// Reserve the source cooldown only after a global slot is available.
    pub(crate) fn admit_source_recovery(
        &self,
        manifest_hash: &str,
        now_unix: u64,
    ) -> Result<Option<MediaSourceRecoveryPermit>> {
        let Some(permit) = self.try_source_recovery_slot()? else {
            return Ok(None);
        };
        if !self.reserve_source_recovery_backoff(manifest_hash, now_unix) {
            return Ok(None);
        }
        Ok(Some(permit))
    }

    /// Reserve backoff only once a foreground request has an actual source holder
    /// and needs chunk recovery; artifact imports and missing holders use no slot.
    fn reserve_source_recovery_backoff(&self, manifest_hash: &str, now_unix: u64) -> bool {
        self.admit_retry(manifest_hash, now_unix, MediaRetryKind::SourceRecovery)
    }

    fn admit_retry(&self, manifest_hash: &str, now_unix: u64, kind: MediaRetryKind) -> bool {
        self.source_recovery_retries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .admit(manifest_hash, now_unix, kind)
    }
}

pub(crate) fn enabled_from_labels(labels: &HashMap<String, String>) -> Result<bool> {
    labels
        .get(MULTIMEDIA_ENABLED_LABEL)
        .map_or(Ok(true), |value| {
            value
                .parse()
                .context("multimedia_enabled must be true or false")
        })
}

pub(crate) fn enabled_from_environment(raw: Option<&str>) -> Result<Option<bool>> {
    raw.map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| match value.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Ok(true),
            "false" | "0" | "no" | "off" => Ok(false),
            _ => bail!("invalid BERRYKEEP_NODE_MULTIMEDIA_ENABLED: expected true/false, 1/0, yes/no, or on/off"),
        })
        .transpose()
}

fn set_enabled_label(labels: &mut HashMap<String, String>, enabled: bool) {
    if enabled {
        labels.remove(MULTIMEDIA_ENABLED_LABEL);
    } else {
        labels.insert(MULTIMEDIA_ENABLED_LABEL.to_string(), "false".to_string());
    }
}

#[derive(Serialize)]
struct NodeMultimediaView {
    enabled: bool,
    persistence_source: OperatorConfigPersistenceSource,
    persisted: bool,
}

#[derive(Deserialize)]
pub(super) struct UpdateNodeMultimediaRequest {
    enabled: bool,
}

fn view(state: &ServerState, persisted: bool) -> NodeMultimediaView {
    NodeMultimediaView {
        enabled: state.multimedia.enabled(),
        persistence_source: operator_config_persistence_source(state),
        persisted,
    }
}

pub(super) async fn get_config(State(state): State<ServerState>, headers: HeaderMap) -> Response {
    let action = "auth/node-multimedia/get";
    let authz = match authorize_admin_request(&state, &headers, action, true, true, json!({})).await
    {
        Ok(authz) => authz,
        Err(status) => return status.into_response(),
    };
    let view = view(&state, state.network.node_enrollment_path.is_some());
    append_admin_audit(
        &state,
        action,
        &authz,
        true,
        true,
        true,
        "success",
        json!({ "enabled": view.enabled }),
    )
    .await;
    Json(view).into_response()
}

pub(super) async fn update_config(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(request): Json<UpdateNodeMultimediaRequest>,
) -> Response {
    let action = "auth/node-multimedia/update";
    let details = json!({ "enabled": request.enabled });
    let authz =
        match authorize_admin_request(&state, &headers, action, false, true, details.clone()).await
        {
            Ok(authz) => authz,
            Err(status) => return status.into_response(),
        };
    // Serialize with certificate renewal so a settings write keeps the latest enrollment material.
    let _guard = state.network.node_enrollment_renewal_lock.lock().await;
    let result = apply_config(&state, request.enabled).await;
    match result {
        Ok(persisted) => {
            append_admin_audit(
                &state, action, &authz, true, false, true, "success", details,
            )
            .await;
            Json(view(&state, persisted)).into_response()
        }
        Err(error) => {
            tracing::error!(%error, "failed updating node multimedia configuration");
            append_admin_audit(
                &state,
                action,
                &authz,
                true,
                false,
                true,
                "error",
                json!({ "error": error.to_string() }),
            )
            .await;
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": error.to_string() })),
            )
                .into_response()
        }
    }
}

async fn apply_config(state: &ServerState, enabled: bool) -> Result<bool> {
    // Validate before writing, but leave heartbeats and replication free to run
    // while the enrollment file is read and written on a blocking worker.
    let original_descriptor = state
        .cluster
        .lock()
        .await
        .node(state.node_id)
        .cloned()
        .context("local node descriptor is unavailable")?;
    let persistence_state = state.clone();
    let persisted = tokio::task::spawn_blocking(move || {
        update_persisted_node_enrollment_if_possible(&persistence_state, |package| {
            set_enabled_label(&mut package.bootstrap.labels, enabled);
        })
    })
    .await
    .context("node multimedia persistence worker failed")??;
    {
        let mut cluster = state.cluster.lock().await;
        // Preserve concurrent heartbeat/storage updates. The node owns its local
        // descriptor, so restore the validated one if membership was reloaded.
        let mut descriptor = cluster
            .node(state.node_id)
            .cloned()
            .unwrap_or(original_descriptor);
        set_enabled_label(&mut descriptor.labels, enabled);
        cluster.register_node(descriptor);
    }
    let was_enabled = state.multimedia.enabled();
    state.multimedia.set_enabled(enabled);
    if enabled && !was_enabled {
        super::spawn_media_metadata_backfill(state.clone(), "multimedia_enabled");
        super::map_dataset_import::spawn_resume_if_needed(state.clone()).await;
    }
    // Runtime and enrollment already agree. A cluster-cache write failure must
    // not skip enable side effects, and an identical PUT must retry that write.
    persist_cluster_nodes_state(state).await?;
    Ok(persisted)
}

pub(super) fn unavailable_response() -> Response {
    (StatusCode::NOT_IMPLEMENTED, [(axum::http::header::CACHE_CONTROL, "no-store")], Json(json!({
        "error": "Multimedia functionality is disabled on this server node. Use a multimedia-enabled node.",
        "code": MULTIMEDIA_DISABLED_CODE,
    }))).into_response()
}

/// Inspect matched route templates so object keys resembling convenience routes stay accessible.
fn is_multimedia_route(method: &Method, matched_path: &str) -> bool {
    let path = matched_path
        .strip_prefix(super::PUBLIC_API_V1_PREFIX)
        .unwrap_or(matched_path);
    let path = path.strip_prefix("/auth").unwrap_or(path);
    let path = path.strip_prefix("/api").unwrap_or(path);
    path.starts_with("/maps/")
        || path.starts_with("/gallery/map/")
        || matches!(
            path,
            "/store/map/clusters"
                | "/store/map/cluster-entries"
                | "/media/thumbnail"
                | "/media/cache/retry"
                | "/cluster/media/artifact"
        )
        || (method == Method::POST && path == "/operations/{operation_id}/runs")
}

pub(super) async fn require_multimedia(
    State(state): State<ServerState>,
    request: Request,
    next: Next,
) -> Response {
    if let Some(path) = request.extensions().get::<MatchedPath>()
        && is_multimedia_route(request.method(), path.as_str())
    {
        // Client/map routers and internal transports authenticate in their outer
        // middleware. Admin handlers authenticate internally, so do their check
        // here before either capability rejection or request-body extraction.
        let normalized_path = path
            .as_str()
            .strip_prefix(super::PUBLIC_API_V1_PREFIX)
            .unwrap_or(path.as_str());
        let admin_action = AdminMultimediaAction::from_route(request.method(), normalized_path);
        if normalized_path.starts_with("/auth/") {
            let action = admin_action
                .map(AdminMultimediaAction::as_str)
                .unwrap_or("node_multimedia/access");
            let uri = request
                .extensions()
                .get::<axum::extract::OriginalUri>()
                .map(|original| &original.0)
                .unwrap_or_else(|| request.uri());
            let query = axum::extract::Query::<HashMap<String, String>>::try_from_uri(uri)
                .ok()
                .map(|query| query.0);
            if let Err(status) = authorize_admin_request(
                &state,
                request.headers(),
                action,
                true,
                true,
                json!({ "path": uri.path(), "method": request.method().as_str(), "query": query }),
            )
            .await
            {
                return status.into_response();
            }
        }
        let import_status = matches!(
            admin_action,
            Some(
                AdminMultimediaAction::MapImportStatus | AdminMultimediaAction::NaturalEarthStatus
            )
        );
        if !state.multimedia.enabled() && !import_status {
            return unavailable_response();
        }
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn multimedia_resumable_workers_wake_after_reenable() {
        let control = MultimediaControl::default();
        control.set_enabled(false);
        let waiting = control.wait_until_enabled();
        tokio::pin!(waiting);
        assert!(futures_util::poll!(waiting.as_mut()).is_pending());
        control.clone().set_enabled(true);
        tokio::time::timeout(std::time::Duration::from_secs(1), waiting)
            .await
            .expect("an already parked worker must wake when a cloned control enables multimedia");
        // Enabling before a worker subscribes must also avoid a missed wakeup.
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            control.wait_until_enabled(),
        )
        .await
        .unwrap();
    }

    #[test]
    fn multimedia_source_recovery_admission_is_globally_bounded() {
        let control = MultimediaControl::default();
        let first = control
            .admit_source_recovery("first", 100)
            .unwrap()
            .unwrap();
        let second = control.clone().try_source_recovery_slot().unwrap().unwrap();
        assert!(
            !control.source_recovery_due("third", 100),
            "distinct manifests must share a bounded number of active recoveries"
        );
        assert!(
            control
                .admit_source_recovery("third", 100)
                .unwrap()
                .is_none()
        );
        drop(first);
        assert!(
            control.source_recovery_due("third", 101),
            "a busy request must not consume its per-manifest recovery cooldown"
        );
        let third = control
            .admit_source_recovery("third", 101)
            .unwrap()
            .unwrap();
        assert!(!control.source_recovery_due("fourth", 101));
        drop(second);
        drop(third);
        assert!(
            !control.source_recovery_due("first", 101),
            "releasing concurrency admission must retain its retry deadline"
        );
        assert!(control.source_recovery_due("fourth", 101));
        assert!(
            control
                .admit_source_recovery("first", 101)
                .unwrap()
                .is_none()
        );
        control.set_enabled(false);
        assert!(
            control
                .admit_source_recovery("fourth", 101)
                .unwrap_err()
                .is::<MultimediaDisabled>()
        );
        control.set_enabled(true);
        assert!(
            control
                .admit_source_recovery("fourth", 101)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn multimedia_retry_backoffs_survive_capacity_pressure() {
        let control = MultimediaControl::default();
        assert!(control.admit_holder_recheck("first", 100));
        assert!(control.reserve_source_recovery_backoff("first", 100));
        for index in 1..1024 {
            assert!(control.reserve_source_recovery_backoff(&format!("manifest-{index}"), 100));
        }
        assert!(!control.source_recovery_due("overflow", 101));
        assert!(
            !control.reserve_source_recovery_backoff("overflow", 101),
            "new manifests must wait instead of evicting an active recovery deadline"
        );
        assert!(!control.admit_holder_recheck("overflow", 101));
        assert!(!control.source_recovery_due("first", 101));
        assert!(!control.admit_holder_recheck("first", 101));
        assert!(!control.reserve_source_recovery_backoff("first", 699));
        assert!(
            control.reserve_source_recovery_backoff("overflow", 700),
            "expired entries must release admission capacity"
        );
        assert!(control.reserve_source_recovery_backoff("first", 700));
    }

    #[test]
    fn multimedia_retry_backoffs_survive_observation_churn_and_partial_expiry() {
        let control = MultimediaControl::default();
        assert!(control.admit_holder_recheck("first", 100));
        assert!(control.reserve_source_recovery_backoff("first", 101));
        for index in 0..2048 {
            control.note_storage_only_source(&format!("observed-{index}"), 101);
        }
        assert!(
            !control.source_recovery_due("first", 102),
            "capability observations must not evict recovery admission state"
        );
        assert!(!control.admit_holder_recheck("first", 102));
        for index in 1..1024 {
            assert!(control.admit_holder_recheck(&format!("holder-{index}"), 101));
        }
        assert!(
            control.reserve_source_recovery_backoff("new", 131),
            "expired holder-only entries must be reclaimed"
        );
        assert!(
            !control.reserve_source_recovery_backoff("first", 131),
            "expiry of the holder deadline must retain the later source deadline"
        );
        assert!(control.admit_holder_recheck("first", 131));
        assert!(!control.reserve_source_recovery_backoff("first", 700));
        assert!(control.reserve_source_recovery_backoff("first", 701));
    }

    #[test]
    fn multimedia_storage_only_observations_expire_without_spending_retry_budget() {
        let control = MultimediaControl::default();
        assert!(!control.storage_only_source_observed("manifest", 100));
        control.note_storage_only_source("manifest", 100);
        assert!(
            control
                .clone()
                .storage_only_source_observed("manifest", 101)
        );
        assert!(control.source_recovery_due("manifest", 101));
        assert!(control.admit_holder_recheck("manifest", 101));
        assert!(control.reserve_source_recovery_backoff("manifest", 101));
        assert!(control.storage_only_source_observed("manifest", 699));
        assert!(!control.storage_only_source_observed("manifest", 700));
    }

    #[test]
    fn multimedia_holder_rechecks_are_throttled_independently_of_source_recovery() {
        let control = MultimediaControl::default();
        assert!(control.admit_holder_recheck("manifest", 100));
        assert!(!control.clone().admit_holder_recheck("manifest", 101));
        assert!(control.source_recovery_due("manifest", 101));
        assert!(control.reserve_source_recovery_backoff("manifest", 101));
        assert!(!control.admit_holder_recheck("manifest", 129));
        assert!(control.admit_holder_recheck("manifest", 130));
        assert!(!control.source_recovery_due("manifest", 130));
    }

    #[test]
    fn multimedia_source_recovery_backoff_is_shared_and_expires() {
        let control = MultimediaControl::default();
        assert!(control.source_recovery_due("manifest-a", 100));
        assert!(control.clone().source_recovery_due("manifest-a", 100));
        assert!(control.reserve_source_recovery_backoff("manifest-a", 100));
        assert!(
            !control
                .clone()
                .reserve_source_recovery_backoff("manifest-a", 101)
        );
        assert!(control.reserve_source_recovery_backoff("manifest-b", 101));
        assert!(!control.reserve_source_recovery_backoff("manifest-a", 699));
        assert!(control.reserve_source_recovery_backoff("manifest-a", 700));
    }

    #[test]
    fn multimedia_environment_accepts_empty_as_unset_but_rejects_invalid_values() {
        for raw in [None, Some(""), Some("  ")] {
            assert_eq!(enabled_from_environment(raw).unwrap(), None);
        }
        for raw in [" false ", "FALSE", "0", "no", "No", "off", "OFF"] {
            assert_eq!(
                enabled_from_environment(Some(raw)).unwrap(),
                Some(false),
                "{raw}"
            );
        }
        for raw in ["true", "TRUE", "1", "yes", "Yes", "on", " ON "] {
            assert_eq!(
                enabled_from_environment(Some(raw)).unwrap(),
                Some(true),
                "{raw}"
            );
        }
        for raw in ["unknown", "2", "disabled"] {
            assert!(enabled_from_environment(Some(raw)).is_err());
        }
    }

    #[test]
    fn multimedia_labels_default_to_enabled_and_reject_invalid_values() {
        let mut labels = HashMap::new();
        assert!(enabled_from_labels(&labels).unwrap());
        for (raw, expected) in [("false", false), ("true", true)] {
            labels.insert(MULTIMEDIA_ENABLED_LABEL.to_string(), raw.to_string());
            assert_eq!(enabled_from_labels(&labels).unwrap(), expected);
        }
        for raw in ["", "yes", "FALSE", "0"] {
            labels.insert(MULTIMEDIA_ENABLED_LABEL.to_string(), raw.to_string());
            assert!(enabled_from_labels(&labels).is_err());
        }
    }

    #[test]
    fn multimedia_routes_do_not_capture_object_keys_or_storage_maintenance() {
        for path in [
            "/api/v1/store/{key}",
            "/auth/store/{key}",
            "/store/index",
            "/cluster/v2/replication/chunk/{hash}",
            "/auth/scrub/run",
            "/auth/node-multimedia",
            "/auth/media/cache/clear",
        ] {
            assert!(!is_multimedia_route(&Method::GET, path), "{path}");
        }
        assert!(is_multimedia_route(
            &Method::POST,
            "/auth/operations/{operation_id}/runs"
        ));
        assert!(!is_multimedia_route(
            &Method::GET,
            "/auth/operations/{operation_id}/runs/{run_id}"
        ));
    }
}
