//! Generic, persistent server operations and multimedia geolocation inference.
//!
//! Operations deliberately keep durable run state separate from their result
//! chunks.  That lets a long-running producer publish meaningful review data
//! incrementally instead of accumulating one large report in memory.

use std::collections::BTreeMap;
use std::f64::consts::PI;

use serde::{Deserialize, Serialize};

pub(crate) const GEOLOCATION_PROPOSE_OPERATION_ID: &str = "multimedia.geolocation.propose";
pub(crate) const GEOLOCATION_APPLY_OPERATION_ID: &str = "multimedia.geolocation.apply";

/// Process-local scheduling state. Durable state is always kept in
/// [`OperationRun`]; this only serializes live work and is intentionally reset
/// by a restart.
#[derive(Debug, Default)]
pub(crate) struct OperationActivityRuntime {
    pub(crate) multimedia_scan_run_id: Option<String>,
    pub(crate) multimedia_apply_run_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct OperationDescriptor {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) description: String,
    pub(crate) category: OperationCategory,
    pub(crate) requires_prefix: bool,
    pub(crate) supports_review: bool,
    pub(crate) priority: OperationPriority,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OperationCategory {
    Repair,
    Multimedia,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OperationPriority {
    Background,
    Maintenance,
    Repair,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OperationRunStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Interrupted,
}

impl OperationRunStatus {
    pub(crate) fn is_unfinished(self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct OperationProgress {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) completed: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) total: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct OperationRun {
    pub(crate) run_id: String,
    pub(crate) operation_id: String,
    pub(crate) status: OperationRunStatus,
    pub(crate) priority: OperationPriority,
    pub(crate) created_at_unix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) started_at_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) finished_at_unix: Option<u64>,
    #[serde(default)]
    pub(crate) progress: OperationProgress,
    #[serde(default)]
    pub(crate) input: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) summary: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) termination_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct OperationResultChunk {
    pub(crate) run_id: String,
    pub(crate) chunk_id: String,
    pub(crate) result_type: String,
    pub(crate) created_at_unix: u64,
    pub(crate) payload: serde_json::Value,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GeoCaptureTimeBasis {
    /// The timestamp was normalized using an explicit metadata offset.
    UtcNormalized,
    /// A camera or filename supplied a local wall-clock value without an offset.
    /// It is intentionally never sorted into a UTC-normalized segment.
    FloatingLocal,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GeoCaptureTimeSource {
    EmbeddedMetadata,
    Filename,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct GeoCaptureTime {
    pub(crate) unix: u64,
    pub(crate) source: GeoCaptureTimeSource,
    pub(crate) basis: GeoCaptureTimeBasis,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub(crate) struct GeoCoordinate {
    pub(crate) latitude: f64,
    pub(crate) longitude: f64,
}

impl GeoCoordinate {
    pub(crate) fn valid(self) -> bool {
        self.latitude.is_finite()
            && (-90.0..=90.0).contains(&self.latitude)
            && self.longitude.is_finite()
            && (-180.0..=180.0).contains(&self.longitude)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GeoAnalysisMedia {
    pub(crate) path: String,
    pub(crate) object_id: String,
    pub(crate) manifest_hash: String,
    pub(crate) content_fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) capture_time: Option<GeoCaptureTime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) gps: Option<GeoCoordinate>,
    /// Any existing GPS metadata, including an incomplete or unparseable XMP
    /// coordinate. Such media is never a proposal target; only a valid,
    /// non-inferred coordinate may be an inference anchor.
    #[serde(default)]
    pub(crate) gps_is_present: bool,
    /// A location written by a prior BerryKeep inference still means the media
    /// is geotagged, but must not become an anchor for later inferences.
    #[serde(default)]
    pub(crate) gps_is_inferred: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub(crate) struct GeoInferenceConfig {
    pub(crate) max_anchor_time_delta_seconds: u64,
    pub(crate) segment_gap_seconds: u64,
    pub(crate) max_anchor_speed_kmh: f64,
}

impl Default for GeoInferenceConfig {
    fn default() -> Self {
        Self {
            max_anchor_time_delta_seconds: 5 * 60,
            segment_gap_seconds: 5 * 60,
            max_anchor_speed_kmh: 50.0,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GeoInferenceMethod {
    Interpolation,
    NearestAnchor,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GeoProposalAnchor {
    pub(crate) path: String,
    pub(crate) object_id: String,
    pub(crate) capture_time: GeoCaptureTime,
    pub(crate) coordinate: GeoCoordinate,
    pub(crate) distance_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GeoProposal {
    pub(crate) id: String,
    pub(crate) media_path: String,
    pub(crate) object_id: String,
    pub(crate) manifest_hash: String,
    pub(crate) content_fingerprint: String,
    pub(crate) capture_time: GeoCaptureTime,
    pub(crate) proposed: GeoCoordinate,
    pub(crate) method: GeoInferenceMethod,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) previous_anchor: Option<GeoProposalAnchor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) next_anchor: Option<GeoProposalAnchor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) estimated_anchor_speed_kmh: Option<f64>,
    #[serde(default)]
    pub(crate) warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GeoProposalChunk {
    pub(crate) id: String,
    pub(crate) analysis_run_id: String,
    pub(crate) folder: String,
    pub(crate) time_range_start: GeoCaptureTime,
    pub(crate) time_range_end: GeoCaptureTime,
    pub(crate) item_count: usize,
    pub(crate) status: GeoProposalChunkStatus,
    pub(crate) proposals: Vec<GeoProposal>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GeoProposalChunkStatus {
    Ready,
}

/// Produces semantic chunks for a single analysis run.  Folder and time-basis
/// boundaries are never crossed; the latter keeps floating local wall-clock
/// time away from explicitly offset timestamps.
pub(crate) fn infer_geolocation_proposals(
    analysis_run_id: &str,
    media: impl IntoIterator<Item = GeoAnalysisMedia>,
    config: GeoInferenceConfig,
) -> Vec<GeoProposalChunk> {
    let mut by_folder_and_basis =
        BTreeMap::<(String, GeoCaptureTimeBasis), Vec<GeoAnalysisMedia>>::new();
    for item in media {
        let Some(capture_time) = item.capture_time else {
            continue;
        };
        let folder = item
            .path
            .rsplit_once('/')
            .map(|(folder, _)| folder.to_string())
            .unwrap_or_default();
        by_folder_and_basis
            .entry((folder, capture_time.basis))
            .or_default()
            .push(item);
    }

    let mut chunks = Vec::new();
    for ((folder, _basis), mut folder_media) in by_folder_and_basis {
        folder_media.sort_by(|left, right| {
            left.capture_time
                .expect("capture time was grouped above")
                .unix
                .cmp(
                    &right
                        .capture_time
                        .expect("capture time was grouped above")
                        .unix,
                )
                .then_with(|| left.path.cmp(&right.path))
        });

        let mut segment_start = 0usize;
        for index in 1..=folder_media.len() {
            let is_end = index == folder_media.len();
            let gap_exceeded = !is_end
                && folder_media[index]
                    .capture_time
                    .expect("capture time was grouped above")
                    .unix
                    .saturating_sub(
                        folder_media[index - 1]
                            .capture_time
                            .expect("capture time was grouped above")
                            .unix,
                    )
                    > config.segment_gap_seconds;
            if !is_end && !gap_exceeded {
                continue;
            }

            let segment = &folder_media[segment_start..index];
            if let Some(chunk) = infer_segment(analysis_run_id, &folder, segment, config) {
                chunks.push(chunk);
            }
            segment_start = index;
        }
    }
    chunks
}

fn infer_segment(
    analysis_run_id: &str,
    folder: &str,
    segment: &[GeoAnalysisMedia],
    config: GeoInferenceConfig,
) -> Option<GeoProposalChunk> {
    let first = segment.first()?.capture_time?;
    let last = segment.last()?.capture_time?;
    let mut proposals = Vec::new();

    for (target_index, target) in segment.iter().enumerate() {
        let target_time = target.capture_time?;
        if target.gps_is_present {
            continue;
        }

        let previous = (0..target_index).rev().find_map(|index| {
            let candidate = &segment[index];
            let coordinate = candidate
                .gps
                .filter(|value| value.valid() && !candidate.gps_is_inferred)?;
            let capture_time = candidate.capture_time?;
            Some((candidate, capture_time, coordinate))
        });
        let next = ((target_index + 1)..segment.len()).find_map(|index| {
            let candidate = &segment[index];
            let coordinate = candidate
                .gps
                .filter(|value| value.valid() && !candidate.gps_is_inferred)?;
            let capture_time = candidate.capture_time?;
            Some((candidate, capture_time, coordinate))
        });

        let previous = previous.and_then(|(item, capture_time, coordinate)| {
            let distance_seconds = target_time.unix.saturating_sub(capture_time.unix);
            (distance_seconds > 0 && distance_seconds <= config.max_anchor_time_delta_seconds)
                .then_some((
                    anchor_from(item, capture_time, coordinate, distance_seconds),
                    coordinate,
                ))
        });
        let next = next.and_then(|(item, capture_time, coordinate)| {
            let distance_seconds = capture_time.unix.saturating_sub(target_time.unix);
            (distance_seconds > 0 && distance_seconds <= config.max_anchor_time_delta_seconds)
                .then_some((
                    anchor_from(item, capture_time, coordinate, distance_seconds),
                    coordinate,
                ))
        });

        let proposal = match (previous, next) {
            (
                Some((previous_anchor, previous_coordinate)),
                Some((next_anchor, next_coordinate)),
            ) => {
                let anchor_interval_seconds = next_anchor
                    .capture_time
                    .unix
                    .saturating_sub(previous_anchor.capture_time.unix);
                if anchor_interval_seconds == 0 {
                    None
                } else {
                    let speed_kmh = geo_distance_km(previous_coordinate, next_coordinate)
                        / (anchor_interval_seconds as f64 / 3_600.0);
                    if !speed_kmh.is_finite() || speed_kmh > config.max_anchor_speed_kmh {
                        None
                    } else {
                        let fraction = target_time
                            .unix
                            .saturating_sub(previous_anchor.capture_time.unix)
                            as f64
                            / anchor_interval_seconds as f64;
                        Some(GeoProposal {
                            id: proposal_id(analysis_run_id, &target.path),
                            media_path: target.path.clone(),
                            object_id: target.object_id.clone(),
                            manifest_hash: target.manifest_hash.clone(),
                            content_fingerprint: target.content_fingerprint.clone(),
                            capture_time: target_time,
                            proposed: spherical_interpolate(
                                previous_coordinate,
                                next_coordinate,
                                fraction,
                            ),
                            method: GeoInferenceMethod::Interpolation,
                            previous_anchor: Some(previous_anchor),
                            next_anchor: Some(next_anchor),
                            estimated_anchor_speed_kmh: Some(speed_kmh),
                            warnings: Vec::new(),
                        })
                    }
                }
            }
            (Some((anchor, coordinate)), None) => Some(nearest_anchor_proposal(
                analysis_run_id,
                target,
                target_time,
                Some(anchor),
                None,
                coordinate,
            )),
            (None, Some((anchor, coordinate))) => Some(nearest_anchor_proposal(
                analysis_run_id,
                target,
                target_time,
                None,
                Some(anchor),
                coordinate,
            )),
            (None, None) => None,
        };
        if let Some(proposal) = proposal {
            proposals.push(proposal);
        }
    }

    (!proposals.is_empty()).then(|| GeoProposalChunk {
        id: chunk_id(analysis_run_id, folder, first, last),
        analysis_run_id: analysis_run_id.to_string(),
        folder: folder.to_string(),
        time_range_start: first,
        time_range_end: last,
        item_count: segment.len(),
        status: GeoProposalChunkStatus::Ready,
        proposals,
    })
}

fn anchor_from(
    item: &GeoAnalysisMedia,
    capture_time: GeoCaptureTime,
    coordinate: GeoCoordinate,
    distance_seconds: u64,
) -> GeoProposalAnchor {
    GeoProposalAnchor {
        path: item.path.clone(),
        object_id: item.object_id.clone(),
        capture_time,
        coordinate,
        distance_seconds,
    }
}

fn nearest_anchor_proposal(
    analysis_run_id: &str,
    target: &GeoAnalysisMedia,
    capture_time: GeoCaptureTime,
    previous_anchor: Option<GeoProposalAnchor>,
    next_anchor: Option<GeoProposalAnchor>,
    proposed: GeoCoordinate,
) -> GeoProposal {
    GeoProposal {
        id: proposal_id(analysis_run_id, &target.path),
        media_path: target.path.clone(),
        object_id: target.object_id.clone(),
        manifest_hash: target.manifest_hash.clone(),
        content_fingerprint: target.content_fingerprint.clone(),
        capture_time,
        proposed,
        method: GeoInferenceMethod::NearestAnchor,
        previous_anchor,
        next_anchor,
        estimated_anchor_speed_kmh: None,
        warnings: Vec::new(),
    }
}

pub(crate) fn geo_distance_km(left: GeoCoordinate, right: GeoCoordinate) -> f64 {
    const EARTH_RADIUS_KM: f64 = 6_371.008_8;
    let latitude_delta = (right.latitude - left.latitude).to_radians();
    let longitude_delta = (right.longitude - left.longitude).to_radians();
    let a = (latitude_delta / 2.0).sin().powi(2)
        + left.latitude.to_radians().cos()
            * right.latitude.to_radians().cos()
            * (longitude_delta / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_KM * a.sqrt().atan2((1.0 - a).sqrt())
}

/// Great-circle interpolation.  Interpolating unit vectors rather than latitude
/// and longitude separately correctly takes the short route across the date line.
pub(crate) fn spherical_interpolate(
    start: GeoCoordinate,
    end: GeoCoordinate,
    fraction: f64,
) -> GeoCoordinate {
    let fraction = fraction.clamp(0.0, 1.0);
    let start_vector = geographic_unit_vector(start);
    let end_vector = geographic_unit_vector(end);
    let dot = (start_vector.0 * end_vector.0
        + start_vector.1 * end_vector.1
        + start_vector.2 * end_vector.2)
        .clamp(-1.0, 1.0);
    let omega = dot.acos();
    let (mut x, mut y, mut z) = if omega.abs() < 1e-12 {
        (
            start_vector.0 + (end_vector.0 - start_vector.0) * fraction,
            start_vector.1 + (end_vector.1 - start_vector.1) * fraction,
            start_vector.2 + (end_vector.2 - start_vector.2) * fraction,
        )
    } else {
        let sin_omega = omega.sin();
        let left_weight = ((1.0 - fraction) * omega).sin() / sin_omega;
        let right_weight = (fraction * omega).sin() / sin_omega;
        (
            left_weight * start_vector.0 + right_weight * end_vector.0,
            left_weight * start_vector.1 + right_weight * end_vector.1,
            left_weight * start_vector.2 + right_weight * end_vector.2,
        )
    };
    let magnitude = (x * x + y * y + z * z).sqrt();
    if magnitude > 0.0 {
        x /= magnitude;
        y /= magnitude;
        z /= magnitude;
    }
    let longitude = y.atan2(x).to_degrees();
    GeoCoordinate {
        latitude: z.atan2((x * x + y * y).sqrt()).to_degrees(),
        longitude: if longitude > 180.0 {
            longitude - 360.0
        } else if longitude <= -180.0 {
            longitude + 360.0
        } else {
            longitude
        },
    }
}

fn geographic_unit_vector(coordinate: GeoCoordinate) -> (f64, f64, f64) {
    let latitude = coordinate.latitude * PI / 180.0;
    let longitude = coordinate.longitude * PI / 180.0;
    (
        latitude.cos() * longitude.cos(),
        latitude.cos() * longitude.sin(),
        latitude.sin(),
    )
}

fn proposal_id(run_id: &str, path: &str) -> String {
    stable_identifier("proposal", &[run_id, path])
}

fn chunk_id(run_id: &str, folder: &str, start: GeoCaptureTime, end: GeoCaptureTime) -> String {
    stable_identifier(
        "chunk",
        &[
            run_id,
            folder,
            &start.unix.to_string(),
            &end.unix.to_string(),
            match start.basis {
                GeoCaptureTimeBasis::UtcNormalized => "utc",
                GeoCaptureTimeBasis::FloatingLocal => "floating",
            },
        ],
    )
}

fn stable_identifier(kind: &str, values: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(kind.as_bytes());
    for value in values {
        hasher.update(&[0]);
        hasher.update(value.as_bytes());
    }
    format!("{kind}-{}", &hasher.finalize().to_hex()[..20])
}

use anyhow::{Context, Result, bail};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use common::xmp::XmpGeoInference;
use serde_json::json;
use tokio::time::{Duration, sleep};
use tracing::{info, warn};
use uuid::Uuid;

use crate::storage;

use super::{
    DataChangeAction, DataChangeUploadMode, PendingDataChangeEvent, ServerState,
    authorize_admin_request, enqueue_autonomous_post_write_replication, lock_store,
    publish_namespace_change, read_store, record_data_change_event,
    request_local_availability_refresh, should_trigger_autonomous_post_write_replication,
};

const MULTIMEDIA_OPERATION_BATCH_SIZE: usize = 64;
const MULTIMEDIA_OPERATION_YIELD_MILLIS: u64 = 100;
const OPERATION_RUN_PERSIST_ATTEMPTS: usize = 3;
const OPERATION_RUN_PERSIST_RETRY_MILLIS: u64 = 100;
const OPERATION_RESULT_TYPE_GEO_PROPOSAL_CHUNK: &str = "multimedia.geolocation.proposal_chunk";
const OPERATION_RESULT_TYPE_GEO_APPLY_ITEM: &str = "multimedia.geolocation.apply_item";

#[derive(Debug, Deserialize)]
pub(super) struct OperationRunStartRequest {
    #[serde(default)]
    prefix: Option<String>,
    #[serde(default)]
    max_anchor_time_delta_seconds: Option<u64>,
    #[serde(default)]
    segment_gap_seconds: Option<u64>,
    #[serde(default)]
    max_anchor_speed_kmh: Option<f64>,
    #[serde(default)]
    analysis_run_id: Option<String>,
    #[serde(default)]
    proposal_chunk_ids: Vec<String>,
    #[serde(default)]
    proposal_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct OperationRunHistoryQuery {
    #[serde(default)]
    operation_id: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub(super) struct OperationResultQuery {
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    offset: Option<usize>,
}

#[derive(Debug, Serialize)]
struct OperationCatalogResponse {
    operations: Vec<OperationDescriptor>,
}

#[derive(Debug, Serialize)]
struct OperationRunStartResponse {
    run: OperationRun,
}

#[derive(Debug, Serialize)]
struct OperationRunHistoryResponse {
    runs: Vec<OperationRun>,
}

#[derive(Debug, Serialize)]
struct OperationRunResultsResponse {
    run_id: String,
    chunks: Vec<OperationResultChunk>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_offset: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GeoProposalRunInput {
    prefix: String,
    config: GeoInferenceConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GeoApplyRunInput {
    analysis_run_id: String,
    proposal_chunk_ids: Vec<String>,
    proposal_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GeoApplyItemResult {
    proposal_id: String,
    media_path: String,
    #[serde(rename = "status")]
    outcome: GeoApplyItemOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum GeoApplyItemOutcome {
    Applied,
    AlreadyHasGps,
    SkippedStale,
    Failed,
}

pub(super) async fn list_operations(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> Response {
    if let Err(status) = authorize_admin_request(
        &state,
        &headers,
        "auth/operations/list",
        true,
        true,
        json!({}),
    )
    .await
    {
        return status.into_response();
    }
    (
        StatusCode::OK,
        Json(OperationCatalogResponse {
            operations: multimedia_operation_descriptors(),
        }),
    )
        .into_response()
}

pub(super) async fn start_operation_run(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(operation_id): Path<String>,
    Json(request): Json<OperationRunStartRequest>,
) -> Response {
    let start_is_apply = operation_id == GEOLOCATION_APPLY_OPERATION_ID;
    if operation_id != GEOLOCATION_PROPOSE_OPERATION_ID && !start_is_apply {
        return StatusCode::NOT_FOUND.into_response();
    }
    let authorization = authorize_admin_request(
        &state,
        &headers,
        "auth/operations/run",
        !start_is_apply,
        true,
        json!({ "operation_id": operation_id, "prefix": request.prefix }),
    )
    .await;
    if let Err(status) = authorization {
        return status.into_response();
    }

    let (input, priority, task) = match operation_id.as_str() {
        GEOLOCATION_PROPOSE_OPERATION_ID => match validated_geo_proposal_input(&request) {
            Ok(input) => (
                serde_json::to_value(&input).expect("serializable geo proposal input"),
                OperationPriority::Background,
                OperationTask::GeoProposal(input),
            ),
            Err(error) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": error.to_string() })),
                )
                    .into_response();
            }
        },
        GEOLOCATION_APPLY_OPERATION_ID => match validated_geo_apply_input(&request) {
            Ok(input) => (
                serde_json::to_value(&input).expect("serializable geo apply input"),
                OperationPriority::Background,
                OperationTask::GeoApply(input),
            ),
            Err(error) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": error.to_string() })),
                )
                    .into_response();
            }
        },
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    let run = OperationRun {
        run_id: Uuid::now_v7().to_string(),
        operation_id,
        status: OperationRunStatus::Queued,
        priority,
        created_at_unix: super::unix_ts(),
        started_at_unix: None,
        finished_at_unix: None,
        progress: OperationProgress {
            phase: Some("queued".to_string()),
            message: Some("Waiting for the multimedia operation slot.".to_string()),
            ..OperationProgress::default()
        },
        input,
        summary: None,
        error: None,
        termination_reason: None,
    };
    if let Err(error) = persist_operation_run_with_retry(&state, &run, "queue operation").await {
        warn!(error = %error, operation_id = %run.operation_id, "failed to persist queued operation");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    let run_for_task = run.clone();
    let state_for_task = state.clone();
    tokio::spawn(async move {
        match task {
            OperationTask::GeoProposal(input) => {
                run_geo_proposal(state_for_task, run_for_task, input).await
            }
            OperationTask::GeoApply(input) => {
                run_geo_apply(state_for_task, run_for_task, input).await
            }
        }
    });
    (
        StatusCode::ACCEPTED,
        Json(OperationRunStartResponse { run }),
    )
        .into_response()
}

pub(super) async fn get_operation_run(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(run_id): Path<String>,
) -> Response {
    if let Err(status) = authorize_admin_request(
        &state,
        &headers,
        "auth/operation-runs/get",
        true,
        true,
        json!({ "run_id": run_id }),
    )
    .await
    {
        return status.into_response();
    }
    let run = {
        let store = read_store(&state, "operations.load_run").await;
        store.load_operation_run(&run_id).await
    };
    match run {
        Ok(Some(run)) => (StatusCode::OK, Json(run)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => {
            warn!(error = %error, run_id, "failed to load operation run");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub(super) async fn get_operation_run_results(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(run_id): Path<String>,
    Query(query): Query<OperationResultQuery>,
) -> Response {
    if let Err(status) = authorize_admin_request(
        &state,
        &headers,
        "auth/operation-runs/results",
        true,
        true,
        json!({ "run_id": run_id, "limit": query.limit, "offset": query.offset }),
    )
    .await
    {
        return status.into_response();
    }
    let limit = query
        .limit
        .map(|limit| limit.clamp(1, 1_000))
        .unwrap_or(100);
    let offset = query.offset.unwrap_or_default();
    let chunks = {
        let store = read_store(&state, "operations.load_results").await;
        store
            .list_operation_result_chunks(&run_id, Some(limit.saturating_add(1)), offset)
            .await
    };
    match chunks {
        Ok(mut chunks) => {
            let next_offset = (chunks.len() > limit).then(|| {
                chunks.pop();
                offset.saturating_add(limit)
            });
            (
                StatusCode::OK,
                Json(OperationRunResultsResponse {
                    run_id,
                    chunks,
                    next_offset,
                }),
            )
                .into_response()
        }
        Err(error) => {
            warn!(error = %error, "failed to load operation result chunks");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub(super) async fn get_operation_run_history(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Query(query): Query<OperationRunHistoryQuery>,
) -> Response {
    if let Err(status) = authorize_admin_request(
        &state,
        &headers,
        "auth/operation-runs/history",
        true,
        true,
        json!({ "operation_id": query.operation_id, "limit": query.limit }),
    )
    .await
    {
        return status.into_response();
    }
    let limit = query.limit.map(|limit| limit.clamp(1, 200)).or(Some(40));
    let runs = {
        let store = read_store(&state, "operations.load_history").await;
        store
            .list_operation_runs(query.operation_id.as_deref(), limit)
            .await
    };
    match runs {
        Ok(runs) => (StatusCode::OK, Json(OperationRunHistoryResponse { runs })).into_response(),
        Err(error) => {
            warn!(error = %error, "failed to load operation run history");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub(super) async fn interrupt_runs_after_server_restart(state: &ServerState) {
    let interrupted_at_unix = super::unix_ts();
    let result = {
        let store = lock_store(state, "operations.restart_interrupt").await;
        store
            .interrupt_unfinished_operation_runs(interrupted_at_unix, "server_restart")
            .await
    };
    match result {
        Ok(count) if count > 0 => info!(
            count,
            "marked unfinished operation runs interrupted after restart"
        ),
        Ok(_) => {}
        Err(error) => {
            warn!(error = %error, "failed to interrupt unfinished operation runs after restart")
        }
    }
    prune_operation_run_history_with_retention(state, interrupted_at_unix).await;
}

fn multimedia_operation_descriptors() -> Vec<OperationDescriptor> {
    vec![
        OperationDescriptor {
            id: GEOLOCATION_PROPOSE_OPERATION_ID.to_string(),
            label: "Propose missing media locations".to_string(),
            description: "Find media without GPS in one selected folder and propose locations from nearby geotagged files.".to_string(),
            category: OperationCategory::Multimedia,
            requires_prefix: true,
            supports_review: true,
            priority: OperationPriority::Background,
        },
        OperationDescriptor {
            id: GEOLOCATION_APPLY_OPERATION_ID.to_string(),
            label: "Apply confirmed media locations".to_string(),
            description: "Write selected geolocation proposals to XMP sidecars after revalidating every media object.".to_string(),
            category: OperationCategory::Multimedia,
            requires_prefix: false,
            supports_review: false,
            priority: OperationPriority::Background,
        },
    ]
}

fn validated_geo_proposal_input(request: &OperationRunStartRequest) -> Result<GeoProposalRunInput> {
    let prefix = request
        .prefix
        .as_deref()
        .map(str::trim)
        .map(|prefix| prefix.trim_matches('/'))
        .filter(|prefix| !prefix.is_empty())
        .map(|prefix| format!("{prefix}/"))
        .context("a non-empty folder/store prefix is required")?;
    let mut config = GeoInferenceConfig::default();
    if let Some(value) = request.max_anchor_time_delta_seconds {
        config.max_anchor_time_delta_seconds = value;
    }
    if let Some(value) = request.segment_gap_seconds {
        config.segment_gap_seconds = value;
    }
    if let Some(value) = request.max_anchor_speed_kmh {
        config.max_anchor_speed_kmh = value;
    }
    if config.max_anchor_time_delta_seconds == 0
        || config.segment_gap_seconds == 0
        || !config.max_anchor_speed_kmh.is_finite()
        || config.max_anchor_speed_kmh <= 0.0
    {
        bail!("geolocation timing and speed limits must be positive");
    }
    Ok(GeoProposalRunInput { prefix, config })
}

fn validated_geo_apply_input(request: &OperationRunStartRequest) -> Result<GeoApplyRunInput> {
    let analysis_run_id = request
        .analysis_run_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .context("analysis_run_id is required")?
        .to_string();
    let proposal_chunk_ids = normalized_ids(&request.proposal_chunk_ids);
    let proposal_ids = normalized_ids(&request.proposal_ids);
    if proposal_chunk_ids.is_empty() && proposal_ids.is_empty() {
        bail!("select at least one proposal chunk or proposal");
    }
    Ok(GeoApplyRunInput {
        analysis_run_id,
        proposal_chunk_ids,
        proposal_ids,
    })
}

fn normalized_ids(values: &[String]) -> Vec<String> {
    let mut values = values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    values.sort();
    values.dedup();
    values
}

enum OperationTask {
    GeoProposal(GeoProposalRunInput),
    GeoApply(GeoApplyRunInput),
}

async fn run_geo_proposal(state: ServerState, mut run: OperationRun, input: GeoProposalRunInput) {
    if let Err(error) = acquire_multimedia_slot(&state, &run.run_id, true).await {
        finish_failed_operation(&state, &mut run, error).await;
        return;
    }
    run.status = OperationRunStatus::Running;
    run.started_at_unix = Some(super::unix_ts());
    run.progress = OperationProgress {
        phase: Some("scanning".to_string()),
        message: Some("Collecting media metadata in batches.".to_string()),
        ..OperationProgress::default()
    };
    if let Err(error) =
        persist_operation_run_with_retry(&state, &run, "start geolocation proposal").await
    {
        finish_failed_operation(&state, &mut run, error).await;
        release_multimedia_slot(&state, true).await;
        return;
    }

    let result = async {
        let (worker, object_hashes, object_ids) = {
            let store = read_store(&state, "operations.geo_proposal.snapshot").await;
            let inspector = store.store_index_inspector().await?;
            let worker = store.media_cache_worker();
            let hashes = inspector.current_object_hashes();
            let ids = inspector.current_object_ids();
            (worker, hashes, ids)
        };
        let mut candidates = object_hashes
            .into_iter()
            .filter(|(path, _)| path.starts_with(&input.prefix))
            .filter(|(path, _)| storage::gallery_media_type_for_path(path).is_some())
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| left.0.cmp(&right.0));
        let total = candidates.len();
        let mut media = Vec::with_capacity(total);
        let mut metadata_error_count = 0usize;
        for (batch_index, batch) in candidates
            .chunks(MULTIMEDIA_OPERATION_BATCH_SIZE)
            .enumerate()
        {
            wait_for_higher_priority_work(&state).await;
            for (path, manifest_hash) in batch {
                let metadata = match worker.ensure_media_metadata(manifest_hash).await {
                    Ok(metadata) => metadata,
                    Err(error) => {
                        metadata_error_count += 1;
                        warn!(
                            error = %error,
                            media_path = %path,
                            "skipping media with unreadable metadata during geolocation proposal scan"
                        );
                        continue;
                    }
                };
                let Some(metadata) = metadata else {
                    continue;
                };
                let sidecar_gps = {
                    let store = read_store(&state, "operations.geo_proposal.sidecar_gps").await;
                    match store.media_sidecar_gps_overlay(path).await {
                        Ok(overlay) => overlay,
                        Err(error) => {
                            warn!(
                                error = %error,
                                media_path = %path,
                                "ignoring unreadable XMP sidecar during geolocation proposal scan"
                            );
                            storage::MediaSidecarGpsOverlay::default()
                        }
                    }
                };
                let capture_time = capture_time_for_geolocation(path, &metadata);
                let embedded_gps = metadata.gps.as_ref().map(|value| GeoCoordinate {
                    latitude: value.latitude,
                    longitude: value.longitude,
                });
                // A measured embedded location takes precedence over an
                // inferred sidecar. Otherwise preserve the sidecar location
                // and its trust flag: it suppresses a duplicate proposal but
                // cannot act as a future inference anchor.
                let gps_is_present = embedded_gps.is_some() || sidecar_gps.has_geo_location_properties;
                let (gps, gps_is_inferred) = match (sidecar_gps.location, embedded_gps) {
                    (Some(sidecar), Some(embedded)) if sidecar.inferred_by_berrykeep => {
                        (Some(embedded), false)
                    }
                    (Some(sidecar), _) => (
                        Some(GeoCoordinate {
                            latitude: sidecar.latitude,
                            longitude: sidecar.longitude,
                        }),
                        sidecar.inferred_by_berrykeep,
                    ),
                    (None, Some(embedded)) => (Some(embedded), false),
                    (None, None) => (None, false),
                };
                media.push(GeoAnalysisMedia {
                    path: path.clone(),
                    object_id: object_ids.get(path).cloned().unwrap_or_default(),
                    manifest_hash: manifest_hash.clone(),
                    content_fingerprint: metadata.content_fingerprint.clone(),
                    capture_time,
                    gps,
                    gps_is_present,
                    gps_is_inferred,
                });
            }
            run.progress = OperationProgress {
                phase: Some("scanning".to_string()),
                completed: Some(((batch_index + 1) * MULTIMEDIA_OPERATION_BATCH_SIZE).min(total)),
                total: Some(total),
                message: Some("Scanning media metadata.".to_string()),
            };
            persist_operation_run(&state, &run).await?;
            tokio::task::yield_now().await;
        }

        run.progress = OperationProgress {
            phase: Some("proposing".to_string()),
            completed: Some(total),
            total: Some(total),
            message: Some("Building semantic folder/time-segment proposals.".to_string()),
        };
        persist_operation_run(&state, &run).await?;
        let chunks = infer_geolocation_proposals(&run.run_id, media, input.config);
        let proposal_count = chunks
            .iter()
            .map(|chunk| chunk.proposals.len())
            .sum::<usize>();
        for (index, chunk) in chunks.iter().enumerate() {
            let result = OperationResultChunk {
                run_id: run.run_id.clone(),
                chunk_id: chunk.id.clone(),
                result_type: OPERATION_RESULT_TYPE_GEO_PROPOSAL_CHUNK.to_string(),
                created_at_unix: super::unix_ts(),
                payload: serde_json::to_value(chunk)?,
            };
            persist_operation_result_chunk(&state, &result).await?;
            run.progress = OperationProgress {
                phase: Some("persisting_results".to_string()),
                completed: Some(index + 1),
                total: Some(chunks.len()),
                message: Some("Persisting proposal chunks for review.".to_string()),
            };
            persist_operation_run(&state, &run).await?;
            tokio::task::yield_now().await;
        }
        Ok::<_, anyhow::Error>((chunks.len(), proposal_count, total, metadata_error_count))
    }
    .await;

    match result {
        Ok((chunk_count, proposal_count, media_count, metadata_error_count)) => {
            run.status = OperationRunStatus::Completed;
            run.finished_at_unix = Some(super::unix_ts());
            run.progress = OperationProgress {
                phase: Some("completed".to_string()),
                completed: Some(media_count),
                total: Some(media_count),
                message: Some("Proposal scan completed.".to_string()),
            };
            run.summary = Some(json!({
                "media_scanned": media_count,
                "proposal_chunk_count": chunk_count,
                "proposal_count": proposal_count,
                "metadata_error_count": metadata_error_count,
            }));
            if let Err(error) = persist_terminal_operation_run_with_retention(
                &state,
                &run,
                "complete geolocation proposal",
            )
            .await
            {
                finish_interrupted_operation(&state, &mut run, "persistence_failure", error).await;
            }
        }
        Err(error) => finish_failed_operation(&state, &mut run, error).await,
    }
    release_multimedia_slot(&state, true).await;
}

async fn run_geo_apply(state: ServerState, mut run: OperationRun, input: GeoApplyRunInput) {
    if let Err(error) = acquire_multimedia_slot(&state, &run.run_id, false).await {
        finish_failed_operation(&state, &mut run, error).await;
        return;
    }
    run.status = OperationRunStatus::Running;
    run.started_at_unix = Some(super::unix_ts());
    run.progress = OperationProgress {
        phase: Some("loading_selection".to_string()),
        message: Some("Loading confirmed geolocation proposals.".to_string()),
        ..OperationProgress::default()
    };
    if let Err(error) =
        persist_operation_run_with_retry(&state, &run, "start geolocation apply").await
    {
        finish_failed_operation(&state, &mut run, error).await;
        release_multimedia_slot(&state, false).await;
        return;
    }

    let result = async {
        let analysis = {
            let store = read_store(&state, "operations.geo_apply.analysis_run").await;
            store.load_operation_run(&input.analysis_run_id).await?
        }
        .filter(|analysis| analysis.operation_id == GEOLOCATION_PROPOSE_OPERATION_ID)
        .context("referenced analysis run does not exist or is not a geolocation proposal run")?;
        if !matches!(
            analysis.status,
            OperationRunStatus::Completed | OperationRunStatus::Interrupted
        ) {
            bail!("referenced analysis run has not produced reviewable results yet");
        }
        let chunks = {
            let store = read_store(&state, "operations.geo_apply.load_proposals").await;
            store
                .list_operation_result_chunks(&input.analysis_run_id, None, 0)
                .await?
        };
        let proposals = selected_proposals(&input, chunks)?;
        let total = proposals.len();
        let worker = {
            let store = read_store(&state, "operations.geo_apply.worker").await;
            store.media_cache_worker()
        };
        let mut counters = GeoApplyCounters::default();
        for (index, proposal) in proposals.iter().enumerate() {
            wait_for_higher_priority_work(&state).await;
            let item_result =
                apply_one_geo_proposal(&state, &worker, &input.analysis_run_id, proposal).await;
            counters.record(item_result.outcome);
            let chunk = OperationResultChunk {
                run_id: run.run_id.clone(),
                chunk_id: format!("apply-{}", item_result.proposal_id),
                result_type: OPERATION_RESULT_TYPE_GEO_APPLY_ITEM.to_string(),
                created_at_unix: super::unix_ts(),
                payload: serde_json::to_value(&item_result)?,
            };
            persist_operation_result_chunk(&state, &chunk).await?;
            run.progress = OperationProgress {
                phase: Some("applying".to_string()),
                completed: Some(index + 1),
                total: Some(total),
                message: Some("Revalidating and applying selected sidecar updates.".to_string()),
            };
            persist_operation_run(&state, &run).await?;
            tokio::task::yield_now().await;
        }
        Ok::<_, anyhow::Error>((total, counters))
    }
    .await;

    match result {
        Ok((total, counters)) => {
            run.status = OperationRunStatus::Completed;
            run.finished_at_unix = Some(super::unix_ts());
            run.progress = OperationProgress {
                phase: Some("completed".to_string()),
                completed: Some(total),
                total: Some(total),
                message: Some("Geolocation apply run completed.".to_string()),
            };
            run.summary = Some(json!({
                "selected": total,
                "applied": counters.applied,
                "already_has_gps": counters.already_has_gps,
                "skipped_stale": counters.skipped_stale,
                "failed": counters.failed,
            }));
            if let Err(error) = persist_terminal_operation_run_with_retention(
                &state,
                &run,
                "complete geolocation apply",
            )
            .await
            {
                finish_interrupted_operation(&state, &mut run, "persistence_failure", error).await;
            }
        }
        Err(error) => finish_failed_operation(&state, &mut run, error).await,
    }
    release_multimedia_slot(&state, false).await;
}

fn capture_time_for_geolocation(
    path: &str,
    metadata: &storage::CachedMediaMetadata,
) -> Option<GeoCaptureTime> {
    if let Some(unix) = metadata.taken_at_unix {
        return Some(GeoCaptureTime {
            unix,
            source: GeoCaptureTimeSource::EmbeddedMetadata,
            basis: if metadata.taken_at_timezone_known == Some(true) {
                GeoCaptureTimeBasis::UtcNormalized
            } else {
                GeoCaptureTimeBasis::FloatingLocal
            },
        });
    }
    storage::filename_captured_at_unix(path).map(|unix| GeoCaptureTime {
        unix,
        source: GeoCaptureTimeSource::Filename,
        basis: GeoCaptureTimeBasis::FloatingLocal,
    })
}

fn selected_proposals(
    input: &GeoApplyRunInput,
    result_chunks: Vec<OperationResultChunk>,
) -> Result<Vec<GeoProposal>> {
    let selected_chunk_ids = input
        .proposal_chunk_ids
        .iter()
        .collect::<std::collections::BTreeSet<_>>();
    let selected_proposal_ids = input
        .proposal_ids
        .iter()
        .collect::<std::collections::BTreeSet<_>>();
    let mut proposals = Vec::new();
    for result_chunk in result_chunks {
        if result_chunk.result_type != OPERATION_RESULT_TYPE_GEO_PROPOSAL_CHUNK {
            continue;
        }
        let chunk: GeoProposalChunk = serde_json::from_value(result_chunk.payload)
            .context("invalid persisted geolocation proposal chunk")?;
        if chunk.analysis_run_id != input.analysis_run_id {
            bail!("proposal chunk does not belong to the referenced analysis run");
        }
        let whole_chunk_selected = selected_chunk_ids.contains(&chunk.id);
        for proposal in chunk.proposals {
            if whole_chunk_selected || selected_proposal_ids.contains(&proposal.id) {
                proposals.push(proposal);
            }
        }
    }
    proposals.sort_by(|left, right| left.id.cmp(&right.id));
    proposals.dedup_by(|left, right| left.id == right.id);
    if proposals.is_empty() {
        bail!("the selected proposal IDs do not exist in the referenced analysis run");
    }
    Ok(proposals)
}

#[derive(Default)]
struct GeoApplyCounters {
    applied: usize,
    already_has_gps: usize,
    skipped_stale: usize,
    failed: usize,
}

impl GeoApplyCounters {
    fn record(&mut self, outcome: GeoApplyItemOutcome) {
        match outcome {
            GeoApplyItemOutcome::Applied => self.applied += 1,
            GeoApplyItemOutcome::AlreadyHasGps => self.already_has_gps += 1,
            GeoApplyItemOutcome::SkippedStale => self.skipped_stale += 1,
            GeoApplyItemOutcome::Failed => self.failed += 1,
        }
    }
}

async fn apply_one_geo_proposal(
    state: &ServerState,
    worker: &storage::MediaCacheWorker,
    analysis_run_id: &str,
    proposal: &GeoProposal,
) -> GeoApplyItemResult {
    let stale = |detail: String| GeoApplyItemResult {
        proposal_id: proposal.id.clone(),
        media_path: proposal.media_path.clone(),
        outcome: GeoApplyItemOutcome::SkippedStale,
        detail: Some(detail),
    };
    let failure = |detail: String| GeoApplyItemResult {
        proposal_id: proposal.id.clone(),
        media_path: proposal.media_path.clone(),
        outcome: GeoApplyItemOutcome::Failed,
        detail: Some(detail),
    };

    let identity = {
        let store = read_store(state, "operations.geo_apply.revalidate_identity").await;
        let inspector = match store.store_index_inspector().await {
            Ok(inspector) => inspector,
            Err(error) => {
                return failure(format!("failed reading current object state: {error:#}"));
            }
        };
        (
            inspector
                .current_object_hashes()
                .get(&proposal.media_path)
                .cloned(),
            inspector
                .current_object_ids()
                .get(&proposal.media_path)
                .cloned(),
        )
    };
    let (Some(manifest_hash), Some(object_id)) = identity else {
        return stale("media object no longer exists".to_string());
    };
    if manifest_hash != proposal.manifest_hash || object_id != proposal.object_id {
        return stale(
            "media object identity or current version changed since analysis".to_string(),
        );
    }
    let metadata = match worker.ensure_media_metadata(&manifest_hash).await {
        Ok(Some(metadata)) => metadata,
        Ok(None) => return stale("media metadata is no longer available".to_string()),
        Err(error) => return failure(format!("failed reading current media metadata: {error:#}")),
    };
    if metadata.content_fingerprint != proposal.content_fingerprint {
        return stale("media content fingerprint changed since analysis".to_string());
    }
    if capture_time_for_geolocation(&proposal.media_path, &metadata) != Some(proposal.capture_time)
    {
        return stale("capture-time data changed since analysis".to_string());
    }
    if metadata.gps.is_some() {
        return GeoApplyItemResult {
            proposal_id: proposal.id.clone(),
            media_path: proposal.media_path.clone(),
            outcome: GeoApplyItemOutcome::AlreadyHasGps,
            detail: Some("media metadata already has GPS".to_string()),
        };
    }
    let existing_sidecar_gps = {
        let store = read_store(state, "operations.geo_apply.revalidate_sidecar").await;
        match store.media_sidecar_gps_overlay(&proposal.media_path).await {
            Ok(value) => value,
            Err(error) => return failure(format!("failed reading XMP sidecar: {error:#}")),
        }
    };
    if existing_sidecar_gps.has_geo_location_properties {
        return GeoApplyItemResult {
            proposal_id: proposal.id.clone(),
            media_path: proposal.media_path.clone(),
            outcome: GeoApplyItemOutcome::AlreadyHasGps,
            detail: Some("XMP sidecar already has GPS".to_string()),
        };
    }
    let inference = xmp_geo_inference(analysis_run_id, proposal);
    let write = {
        let mut store = lock_store(state, "operations.geo_apply.write_sidecar").await;
        store
            .set_media_geolocation(&proposal.media_path, inference)
            .await
    };
    let result = match write {
        Ok(storage::MediaGeolocationWrite::Applied(result)) => result,
        Ok(storage::MediaGeolocationWrite::AlreadyHasGps) => {
            return GeoApplyItemResult {
                proposal_id: proposal.id.clone(),
                media_path: proposal.media_path.clone(),
                outcome: GeoApplyItemOutcome::AlreadyHasGps,
                detail: Some("XMP sidecar already has GPS".to_string()),
            };
        }
        Err(error) => return failure(format!("failed writing XMP sidecar: {error:#}")),
    };

    let sidecar_key = common::xmp::sidecar_key_for_media(&proposal.media_path);
    publish_namespace_change(state);
    request_local_availability_refresh(state);
    if should_trigger_autonomous_post_write_replication(
        state.autonomous_replication_on_put_enabled,
        false,
    ) {
        enqueue_autonomous_post_write_replication(
            state,
            super::autonomous_post_write_replication_subjects(&sidecar_key, &result.version_id),
        )
        .await;
    }
    record_data_change_event(
        state,
        PendingDataChangeEvent {
            action: DataChangeAction::Upload,
            actor: None,
            path: sidecar_key,
            from_path: None,
            to_path: None,
            recursive: false,
            affected_path_count: 1,
            total_size_bytes: None,
            version_id: Some(result.version_id),
            snapshot_id: Some(result.snapshot_id),
            upload_mode: Some(DataChangeUploadMode::Direct),
        },
    )
    .await;
    GeoApplyItemResult {
        proposal_id: proposal.id.clone(),
        media_path: proposal.media_path.clone(),
        outcome: GeoApplyItemOutcome::Applied,
        detail: None,
    }
}

fn xmp_geo_inference(analysis_run_id: &str, proposal: &GeoProposal) -> XmpGeoInference {
    let (
        reference_distance_seconds,
        previous_anchor_distance_seconds,
        next_anchor_distance_seconds,
    ) = match proposal.method {
        GeoInferenceMethod::NearestAnchor => (
            proposal
                .previous_anchor
                .as_ref()
                .or(proposal.next_anchor.as_ref())
                .map(|anchor| anchor.distance_seconds),
            None,
            None,
        ),
        GeoInferenceMethod::Interpolation => (
            None,
            proposal
                .previous_anchor
                .as_ref()
                .map(|anchor| anchor.distance_seconds),
            proposal
                .next_anchor
                .as_ref()
                .map(|anchor| anchor.distance_seconds),
        ),
    };
    let confidence = match proposal.method {
        GeoInferenceMethod::NearestAnchor => format!(
            "reference_distance={}s",
            reference_distance_seconds.unwrap_or_default()
        ),
        GeoInferenceMethod::Interpolation => format!(
            "previous={}s; next={}s; estimated_speed={:.1}km/h",
            previous_anchor_distance_seconds.unwrap_or_default(),
            next_anchor_distance_seconds.unwrap_or_default(),
            proposal.estimated_anchor_speed_kmh.unwrap_or_default(),
        ),
    };
    XmpGeoInference {
        latitude: proposal.proposed.latitude,
        longitude: proposal.proposed.longitude,
        method: match proposal.method {
            GeoInferenceMethod::Interpolation => "interpolation".to_string(),
            GeoInferenceMethod::NearestAnchor => "nearest-anchor".to_string(),
        },
        run_id: analysis_run_id.to_string(),
        confidence,
        reference_distance_seconds,
        previous_anchor_distance_seconds,
        next_anchor_distance_seconds,
        estimated_speed_kmh: proposal.estimated_anchor_speed_kmh,
    }
}

async fn persist_operation_run(state: &ServerState, run: &OperationRun) -> Result<()> {
    let store = lock_store(state, "operations.persist_run").await;
    store.persist_operation_run(run).await
}

async fn persist_operation_run_with_retry(
    state: &ServerState,
    run: &OperationRun,
    action: &str,
) -> Result<()> {
    for attempt in 1..=OPERATION_RUN_PERSIST_ATTEMPTS {
        match persist_operation_run(state, run).await {
            Ok(()) => return Ok(()),
            Err(error) if attempt == OPERATION_RUN_PERSIST_ATTEMPTS => return Err(error),
            Err(error) => {
                warn!(
                    error = %error,
                    run_id = %run.run_id,
                    attempt,
                    action,
                    "retrying operation-run persistence after a transient failure"
                );
                sleep(Duration::from_millis(OPERATION_RUN_PERSIST_RETRY_MILLIS)).await;
            }
        }
    }
    unreachable!("operation-run persistence attempts are non-empty")
}

async fn persist_terminal_operation_run_with_retention(
    state: &ServerState,
    run: &OperationRun,
    action: &str,
) -> Result<()> {
    persist_operation_run_with_retry(state, run, action).await?;
    prune_operation_run_history_with_retention(
        state,
        run.finished_at_unix.unwrap_or_else(super::unix_ts),
    )
    .await;
    Ok(())
}

async fn prune_operation_run_history_with_retention(state: &ServerState, reference_unix: u64) {
    let retention_cutoff =
        reference_unix.saturating_sub(state.maintenance.repair_run_history_retention_secs);
    let result = {
        let store = lock_store(state, "operations.prune_history").await;
        store
            .prune_operation_run_history_before(retention_cutoff)
            .await
    };
    if let Err(error) = result {
        warn!(
            error = %error,
            retention_cutoff,
            "failed to prune generic operation run history"
        );
    }
}

async fn persist_operation_result_chunk(
    state: &ServerState,
    chunk: &OperationResultChunk,
) -> Result<()> {
    let store = lock_store(state, "operations.persist_result_chunk").await;
    store.persist_operation_result_chunk(chunk).await
}

async fn finish_failed_operation(
    state: &ServerState,
    run: &mut OperationRun,
    error: anyhow::Error,
) {
    run.status = OperationRunStatus::Failed;
    run.finished_at_unix = Some(super::unix_ts());
    run.error = Some(error.to_string());
    run.progress.phase = Some("failed".to_string());
    run.progress.message = Some("Operation failed; see error for details.".to_string());
    if let Err(persist_error) =
        persist_terminal_operation_run_with_retention(state, run, "persist failed operation").await
    {
        warn!(error = %persist_error, run_id = %run.run_id, "failed to persist failed operation");
    }
    warn!(error = %error, run_id = %run.run_id, operation_id = %run.operation_id, "operation failed");
}

async fn finish_interrupted_operation(
    state: &ServerState,
    run: &mut OperationRun,
    reason: &str,
    error: anyhow::Error,
) {
    run.status = OperationRunStatus::Interrupted;
    run.finished_at_unix = Some(super::unix_ts());
    run.error = Some(error.to_string());
    run.termination_reason = Some(reason.to_string());
    run.progress.phase = Some("interrupted".to_string());
    run.progress.message =
        Some("Operation interrupted; persisted results remain reviewable.".to_string());
    if let Err(persist_error) =
        persist_terminal_operation_run_with_retention(state, run, "persist interrupted operation")
            .await
    {
        warn!(error = %persist_error, run_id = %run.run_id, "failed to persist interrupted operation");
    }
}

async fn acquire_multimedia_slot(state: &ServerState, run_id: &str, is_scan: bool) -> Result<()> {
    loop {
        if higher_priority_work_active(state).await {
            sleep(Duration::from_millis(MULTIMEDIA_OPERATION_YIELD_MILLIS)).await;
            continue;
        }
        let acquired = {
            let mut activity = state.maintenance.operations_activity.lock().await;
            let active = if is_scan {
                &mut activity.multimedia_scan_run_id
            } else {
                &mut activity.multimedia_apply_run_id
            };
            if active.is_none() {
                *active = Some(run_id.to_string());
                true
            } else {
                false
            }
        };
        if acquired {
            return Ok(());
        }
        sleep(Duration::from_millis(MULTIMEDIA_OPERATION_YIELD_MILLIS)).await;
    }
}

async fn release_multimedia_slot(state: &ServerState, is_scan: bool) {
    let mut activity = state.maintenance.operations_activity.lock().await;
    if is_scan {
        activity.multimedia_scan_run_id = None;
    } else {
        activity.multimedia_apply_run_id = None;
    }
}

async fn wait_for_higher_priority_work(state: &ServerState) {
    while higher_priority_work_active(state).await {
        sleep(Duration::from_millis(MULTIMEDIA_OPERATION_YIELD_MILLIS)).await;
    }
}

async fn higher_priority_work_active(state: &ServerState) -> bool {
    if !state
        .maintenance
        .repair_activity
        .lock()
        .await
        .active_runs
        .is_empty()
    {
        return true;
    }
    if !state
        .maintenance
        .manual_repair_activity
        .lock()
        .await
        .active_runs
        .is_empty()
    {
        return true;
    }
    !state
        .maintenance
        .data_scrub_activity
        .lock()
        .await
        .active_runs
        .is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cached_metadata(
        taken_at_unix: Option<u64>,
        taken_at_timezone_known: Option<bool>,
        date_encoded_unix: Option<u64>,
    ) -> storage::CachedMediaMetadata {
        storage::CachedMediaMetadata {
            schema_version: 1,
            content_fingerprint: "fingerprint".to_string(),
            source_manifest_hash: "manifest".to_string(),
            status: storage::MediaCacheStatus::Ready,
            media_type: Some("image".to_string()),
            mime_type: Some("image/jpeg".to_string()),
            width: None,
            height: None,
            orientation: None,
            taken_at_unix,
            taken_at_timezone_known,
            date_encoded_unix,
            duration_millis: None,
            frame_rate_millihertz: None,
            total_bitrate_bps: None,
            codec_name: None,
            codec_fourcc: None,
            gps: None,
            photo: None,
            thumbnail: None,
            source_size_bytes: 0,
            generated_at_unix: 0,
            retry_after_unix: None,
            error: None,
        }
    }

    fn media(path: &str, time: Option<u64>, gps: Option<(f64, f64)>) -> GeoAnalysisMedia {
        let gps_is_present = gps.is_some();
        GeoAnalysisMedia {
            path: path.to_string(),
            object_id: format!("object-{path}"),
            manifest_hash: format!("manifest-{path}"),
            content_fingerprint: format!("fingerprint-{path}"),
            capture_time: time.map(|unix| GeoCaptureTime {
                unix,
                source: GeoCaptureTimeSource::EmbeddedMetadata,
                basis: GeoCaptureTimeBasis::UtcNormalized,
            }),
            gps: gps.map(|(latitude, longitude)| GeoCoordinate {
                latitude,
                longitude,
            }),
            gps_is_present,
            gps_is_inferred: false,
        }
    }

    fn proposals(media: Vec<GeoAnalysisMedia>, config: GeoInferenceConfig) -> Vec<GeoProposal> {
        infer_geolocation_proposals("run", media, config)
            .into_iter()
            .flat_map(|chunk| chunk.proposals)
            .collect()
    }

    #[test]
    fn interpolates_between_two_anchors() {
        let result = proposals(
            vec![
                media("trip/a.jpg", Some(0), Some((0.0, 0.0))),
                media("trip/target.jpg", Some(60), None),
                media("trip/b.jpg", Some(120), Some((0.0, 0.01))),
            ],
            GeoInferenceConfig::default(),
        );
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].method, GeoInferenceMethod::Interpolation);
        assert!((result[0].proposed.longitude - 0.005).abs() < 0.000_1);
        assert_eq!(
            result[0].previous_anchor.as_ref().unwrap().distance_seconds,
            60
        );
        assert_eq!(result[0].next_anchor.as_ref().unwrap().distance_seconds, 60);
    }

    #[test]
    fn uses_nearest_anchor_on_either_side() {
        let before = proposals(
            vec![
                media("trip/a.jpg", Some(0), Some((10.0, 20.0))),
                media("trip/target.jpg", Some(90), None),
            ],
            GeoInferenceConfig::default(),
        );
        assert_eq!(before[0].method, GeoInferenceMethod::NearestAnchor);
        assert!(before[0].previous_anchor.is_some());

        let after = proposals(
            vec![
                media("trip/target.jpg", Some(90), None),
                media("trip/b.jpg", Some(120), Some((10.0, 20.0))),
            ],
            GeoInferenceConfig::default(),
        );
        assert_eq!(after[0].method, GeoInferenceMethod::NearestAnchor);
        assert!(after[0].next_anchor.is_some());
    }

    #[test]
    fn respects_five_minute_anchor_boundary() {
        let result = proposals(
            vec![
                media("trip/a.jpg", Some(0), Some((0.0, 0.0))),
                media("trip/target.jpg", Some(301), None),
            ],
            GeoInferenceConfig {
                segment_gap_seconds: 1_000,
                ..GeoInferenceConfig::default()
            },
        );
        assert!(result.is_empty());
    }

    #[test]
    fn rejects_anchor_pairs_over_speed_limit_unless_configured_higher() {
        let source = vec![
            media("trip/a.jpg", Some(0), Some((0.0, 0.0))),
            media("trip/target.jpg", Some(60), None),
            media("trip/b.jpg", Some(120), Some((1.0, 0.0))),
        ];
        assert!(proposals(source.clone(), GeoInferenceConfig::default()).is_empty());
        let result = proposals(
            source,
            GeoInferenceConfig {
                max_anchor_speed_kmh: 10_000.0,
                ..GeoInferenceConfig::default()
            },
        );
        assert_eq!(result[0].method, GeoInferenceMethod::Interpolation);
    }

    #[test]
    fn segment_gaps_create_distinct_semantic_chunks() {
        let chunks = infer_geolocation_proposals(
            "run",
            vec![
                media("trip/a.jpg", Some(0), Some((0.0, 0.0))),
                media("trip/b.jpg", Some(30), None),
                media("trip/c.jpg", Some(60), Some((0.0, 0.005))),
                media("trip/d.jpg", Some(1_000), Some((1.0, 1.0))),
                media("trip/e.jpg", Some(1_030), None),
            ],
            GeoInferenceConfig::default(),
        );
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].folder, "trip");
        assert_eq!(chunks[0].item_count, 3);
        assert_eq!(chunks[1].item_count, 2);
    }

    #[test]
    fn omits_media_without_capture_time_and_never_uses_version_time() {
        let result = proposals(
            vec![
                media("trip/a.jpg", Some(0), Some((0.0, 0.0))),
                media("trip/unknown.jpg", None, None),
                media("trip/b.jpg", Some(60), Some((0.0, 0.01))),
            ],
            GeoInferenceConfig::default(),
        );
        assert!(result.is_empty());
    }

    #[test]
    fn same_timestamps_do_not_create_unsafe_interpolation() {
        let result = proposals(
            vec![
                media("trip/a.jpg", Some(0), Some((0.0, 0.0))),
                media("trip/target.jpg", Some(0), None),
                media("trip/b.jpg", Some(0), Some((0.0, 1.0))),
            ],
            GeoInferenceConfig::default(),
        );
        assert!(result.is_empty());
    }

    #[test]
    fn interpolation_crosses_the_antimeridian_on_the_short_arc() {
        let value = spherical_interpolate(
            GeoCoordinate {
                latitude: 0.0,
                longitude: 179.0,
            },
            GeoCoordinate {
                latitude: 0.0,
                longitude: -179.0,
            },
            0.5,
        );
        assert!(value.longitude.abs() > 179.5);
    }

    #[test]
    fn keeps_floating_times_separate_from_utc_times() {
        let mut anchor = media("trip/a.jpg", Some(0), Some((0.0, 0.0)));
        anchor.capture_time.as_mut().unwrap().basis = GeoCaptureTimeBasis::FloatingLocal;
        let result = proposals(
            vec![anchor, media("trip/target.jpg", Some(10), None)],
            GeoInferenceConfig::default(),
        );
        assert!(result.is_empty());
    }

    #[test]
    fn inferred_locations_are_not_anchors_but_remain_geotagged() {
        let mut inferred = media("trip/inferred.jpg", Some(0), Some((0.0, 0.0)));
        inferred.gps_is_inferred = true;
        let result = proposals(
            vec![
                inferred,
                media("trip/target.jpg", Some(60), None),
                media("trip/measured.jpg", Some(120), Some((0.0, 0.01))),
            ],
            GeoInferenceConfig::default(),
        );

        assert_eq!(
            result.len(),
            1,
            "the already geotagged item is not proposed again"
        );
        assert_eq!(result[0].media_path, "trip/target.jpg");
        assert_eq!(result[0].method, GeoInferenceMethod::NearestAnchor);
        assert!(result[0].previous_anchor.is_none());
        assert_eq!(
            result[0]
                .next_anchor
                .as_ref()
                .map(|anchor| anchor.path.as_str()),
            Some("trip/measured.jpg")
        );
    }

    #[test]
    fn capture_time_prefers_embedded_metadata_and_marks_its_time_basis() {
        let metadata = cached_metadata(Some(1_700_000_000), Some(true), Some(1_600_000_000));
        assert_eq!(
            capture_time_for_geolocation("trip/IMG_20200101_000000.jpg", &metadata),
            Some(GeoCaptureTime {
                unix: 1_700_000_000,
                source: GeoCaptureTimeSource::EmbeddedMetadata,
                basis: GeoCaptureTimeBasis::UtcNormalized,
            })
        );
    }

    #[test]
    fn capture_time_falls_back_to_filename_but_not_date_encoded_or_version_time() {
        let metadata = cached_metadata(None, None, Some(1_700_000_000));
        let fallback = capture_time_for_geolocation("trip/IMG_20240102_030405.jpg", &metadata)
            .expect("the filename timestamp is an allowed fallback");
        assert_eq!(fallback.source, GeoCaptureTimeSource::Filename);
        assert_eq!(fallback.basis, GeoCaptureTimeBasis::FloatingLocal);

        // `date_encoded_unix` is deliberately populated, yet a name without a
        // timestamp still cannot become time-usable for geo inference. Version
        // creation time is not part of CachedMediaMetadata or this function at
        // all, so it cannot silently enter the decision path.
        assert_eq!(
            capture_time_for_geolocation("trip/no-time.jpg", &metadata),
            None
        );
    }
}
