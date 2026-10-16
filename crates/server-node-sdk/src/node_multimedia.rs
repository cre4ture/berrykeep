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

#[derive(Debug)]
pub(crate) struct MultimediaDisabled;

impl std::fmt::Display for MultimediaDisabled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("multimedia functionality is disabled on this server node")
    }
}

impl std::error::Error for MultimediaDisabled {}

/// Shared by the request layer and every media worker, including existing clones.
#[derive(Clone)]
pub(crate) struct MultimediaControl {
    enabled: Arc<AtomicBool>,
    source_recovery_retries: Arc<Mutex<RangeChunkCache<String, u64>>>,
}

impl Default for MultimediaControl {
    fn default() -> Self {
        Self {
            enabled: Arc::new(AtomicBool::new(true)),
            source_recovery_retries: Arc::new(Mutex::new(RangeChunkCache::new(1024))),
        }
    }
}

impl MultimediaControl {
    pub(crate) fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    pub(crate) fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
    }

    pub(crate) fn ensure_enabled(&self) -> Result<()> {
        if !self.enabled() {
            return Err(MultimediaDisabled.into());
        }
        Ok(())
    }

    /// A foreground request may bypass metadata-only backoff once, but repeated
    /// source-recovery failures must not create an unbounded retry or state queue.
    pub(crate) fn admit_source_recovery(&self, manifest_hash: &str, now_unix: u64) -> bool {
        let mut retries = self
            .source_recovery_retries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let key = manifest_hash.to_string();
        if retries
            .get(&key)
            .is_some_and(|retry_at| *retry_at > now_unix)
        {
            return false;
        }
        retries.insert(key, now_unix.saturating_add(10 * 60));
        true
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
        .map(|value| {
            value
                .parse()
                .context("invalid BERRYKEEP_NODE_MULTIMEDIA_ENABLED: expected true or false")
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
    if let Err(status) = authorize_admin_request(
        &state,
        &headers,
        "auth/node-multimedia/get",
        true,
        true,
        json!({}),
    )
    .await
    {
        return status.into_response();
    }
    Json(view(&state, state.network.node_enrollment_path.is_some())).into_response()
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
    let persisted = update_persisted_node_enrollment_if_possible(state, |package| {
        set_enabled_label(&mut package.bootstrap.labels, enabled);
    })?;
    let changed = {
        let mut cluster = state.cluster.lock().await;
        let Some(mut descriptor) = cluster
            .list_nodes()
            .into_iter()
            .find(|node| node.node_id == state.node_id)
        else {
            bail!("local node descriptor is unavailable");
        };
        set_enabled_label(&mut descriptor.labels, enabled);
        cluster.register_node(descriptor)
    };
    let was_enabled = state.multimedia.enabled();
    state.multimedia.set_enabled(enabled);
    if changed {
        persist_cluster_nodes_state(state).await?;
    }
    if enabled && !was_enabled {
        super::spawn_media_metadata_backfill(state.clone(), "multimedia_enabled");
        super::map_dataset_import::spawn_resume_if_needed(state.clone()).await;
    }
    Ok(persisted)
}

pub(super) fn unavailable_response() -> Response {
    (StatusCode::NOT_IMPLEMENTED, Json(json!({
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
    if !state.multimedia.enabled()
        && request
            .extensions()
            .get::<MatchedPath>()
            .is_some_and(|path| is_multimedia_route(request.method(), path.as_str()))
    {
        return unavailable_response();
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multimedia_source_recovery_backoff_is_shared_and_expires() {
        let control = MultimediaControl::default();
        assert!(control.admit_source_recovery("manifest-a", 100));
        assert!(!control.clone().admit_source_recovery("manifest-a", 101));
        assert!(control.admit_source_recovery("manifest-b", 101));
        assert!(!control.admit_source_recovery("manifest-a", 699));
        assert!(control.admit_source_recovery("manifest-a", 700));
    }

    #[test]
    fn multimedia_environment_accepts_empty_as_unset_but_rejects_invalid_values() {
        for raw in [None, Some(""), Some("  ")] {
            assert_eq!(enabled_from_environment(raw).unwrap(), None);
        }
        assert_eq!(
            enabled_from_environment(Some(" false ")).unwrap(),
            Some(false)
        );
        assert_eq!(enabled_from_environment(Some("true")).unwrap(), Some(true));
        for raw in ["0", "FALSE", "disabled"] {
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
