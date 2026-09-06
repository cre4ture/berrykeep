//! Generic, persistent server operations and multimedia geolocation inference.
//!
//! Operations deliberately keep durable run state separate from their result
//! chunks.  That lets a long-running producer publish meaningful review data
//! incrementally instead of accumulating one large report in memory.

use std::collections::BTreeMap;
use std::f64::consts::PI;
use std::sync::Arc;

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
    /// Number of proposals across the complete semantic folder/time segment.
    /// A technically paginated result repeats this value on each page.
    #[serde(default)]
    pub(crate) proposal_count: usize,
    /// Zero-based page within a technically paginated semantic chunk.
    #[serde(default)]
    pub(crate) proposal_page: usize,
    #[serde(default = "one_geo_proposal_page")]
    pub(crate) proposal_page_count: usize,
    pub(crate) status: GeoProposalChunkStatus,
    pub(crate) proposals: Vec<GeoProposal>,
}

const fn one_geo_proposal_page() -> usize {
    1
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GeoProposalChunkStatus {
    Ready,
}

#[derive(Debug)]
struct GeoInferenceSegment {
    folder: String,
    media: Vec<GeoAnalysisMedia>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GeoScanCandidate {
    path: String,
    manifest_hash: String,
    object_id: Option<String>,
}

/// Collects a scope-limited, deterministic media candidate list without
/// cloning the entire store index. A run fails rather than silently truncating
/// its review data when the chosen prefix is too broad.
fn geo_scan_candidates<'a>(
    entries: impl IntoIterator<Item = (&'a str, &'a str, Option<&'a str>)>,
    prefix: &str,
    maximum: usize,
) -> Result<Vec<GeoScanCandidate>> {
    let mut candidates = Vec::new();
    for (path, manifest_hash, object_id) in entries {
        if !path.starts_with(prefix) || storage::gallery_media_type_for_path(path).is_none() {
            continue;
        }
        if candidates.len() == maximum {
            bail!(
                "geolocation scan scope contains more than {maximum} media files; choose a narrower prefix"
            );
        }
        candidates.push(GeoScanCandidate {
            path: path.to_string(),
            manifest_hash: manifest_hash.to_string(),
            object_id: object_id.map(str::to_string),
        });
    }
    candidates.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(candidates)
}

/// Produces semantic chunks for a single analysis run.  Folder and time-basis
/// boundaries are never crossed; the latter keeps floating local wall-clock
/// time away from explicitly offset timestamps.
#[cfg(test)]
pub(crate) fn infer_geolocation_proposals(
    analysis_run_id: &str,
    media: impl IntoIterator<Item = GeoAnalysisMedia>,
    config: GeoInferenceConfig,
) -> Vec<GeoProposalChunk> {
    semantic_geo_inference_segments(media, config)
        .into_iter()
        .flat_map(|segment| infer_segment(analysis_run_id, &segment.folder, &segment.media, config))
        .collect()
}

/// Groups media into the logical review units used by both the synchronous
/// inference helper and the background worker.  A worker can persist each
/// returned segment independently without changing the semantics seen by the
/// reviewer.
fn semantic_geo_inference_segments(
    media: impl IntoIterator<Item = GeoAnalysisMedia>,
    config: GeoInferenceConfig,
) -> Vec<GeoInferenceSegment> {
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

    let mut segments = Vec::new();
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

        let mut current_segment = Vec::new();
        for item in folder_media {
            let gap_exceeded = current_segment
                .last()
                .is_some_and(|previous: &GeoAnalysisMedia| {
                    item.capture_time
                        .expect("capture time was grouped above")
                        .unix
                        .saturating_sub(
                            previous
                                .capture_time
                                .expect("capture time was grouped above")
                                .unix,
                        )
                        > config.segment_gap_seconds
                });
            if gap_exceeded {
                segments.push(GeoInferenceSegment {
                    folder: folder.clone(),
                    media: std::mem::take(&mut current_segment),
                });
            }
            current_segment.push(item);
        }
        if !current_segment.is_empty() {
            segments.push(GeoInferenceSegment {
                folder,
                media: current_segment,
            });
        }
    }
    segments
}

#[cfg(test)]
fn infer_segment(
    analysis_run_id: &str,
    folder: &str,
    segment: &[GeoAnalysisMedia],
    config: GeoInferenceConfig,
) -> Vec<GeoProposalChunk> {
    let (previous_anchor_indexes, next_anchor_indexes) = nearest_anchor_indexes(segment);
    let mut proposals = Vec::new();

    for target_index in 0..segment.len() {
        if let Some(proposal) = infer_target_proposal(
            analysis_run_id,
            segment,
            target_index,
            previous_anchor_indexes[target_index],
            next_anchor_indexes[target_index],
            config,
        ) {
            proposals.push(proposal);
        }
    }

    proposal_chunk_pages(analysis_run_id, folder, segment, proposals)
}

/// The worker variant keeps a very large logical segment intact for review,
/// while yielding between fixed-size inference batches.  Repair and scrubbing
/// work can therefore take precedence without inventing arbitrary proposal
/// chunk boundaries.
async fn infer_and_persist_segment(
    state: &ServerState,
    run_id: &str,
    run: &mut OperationRun,
    analysis_run_id: &str,
    folder: &str,
    segment: &[GeoAnalysisMedia],
    config: GeoInferenceConfig,
) -> Result<(usize, usize)> {
    let (previous_anchor_indexes, next_anchor_indexes) = nearest_anchor_indexes(segment);
    let mut proposals = Vec::new();
    for target_index in 0..segment.len() {
        if target_index > 0 && target_index.is_multiple_of(MULTIMEDIA_OPERATION_BATCH_SIZE) {
            wait_for_multimedia_turn(state, run).await?;
            tokio::task::yield_now().await;
        }
        if let Some(proposal) = infer_target_proposal(
            analysis_run_id,
            segment,
            target_index,
            previous_anchor_indexes[target_index],
            next_anchor_indexes[target_index],
            config,
        ) {
            proposals.push(proposal);
        }
    }
    let proposal_count = proposals.len();
    if proposal_count == 0 {
        return Ok((0, 0));
    }

    // Page metadata contains the logical proposal total. The scan already
    // retains this segment in memory, so materializing its small proposal
    // records once avoids a second inference pass and its repeated clones.
    let proposal_page_count = proposal_count.div_ceil(GEO_PROPOSAL_RESULT_PAGE_SIZE);
    let mut proposal_pages = proposals.into_iter();
    for proposal_page in 0..proposal_page_count {
        wait_for_multimedia_turn(state, run).await?;
        let page_proposals = proposal_pages
            .by_ref()
            .take(GEO_PROPOSAL_RESULT_PAGE_SIZE)
            .collect();
        persist_geo_proposal_page(
            state,
            run_id,
            proposal_chunk_page(
                analysis_run_id,
                folder,
                segment,
                page_proposals,
                proposal_count,
                proposal_page,
                proposal_page_count,
            )
            .context("geolocation segment lost its capture-time bounds")?,
        )
        .await?;
    }
    Ok((proposal_count, proposal_page_count))
}

fn nearest_anchor_indexes(
    segment: &[GeoAnalysisMedia],
) -> (Vec<Option<usize>>, Vec<Option<usize>>) {
    let mut previous_anchor_indexes = vec![None; segment.len()];
    let mut previous_time_anchor = None;
    let mut current_time = None;
    let mut current_time_anchor = None;
    for (index, item) in segment.iter().enumerate() {
        let capture_time = item
            .capture_time
            .expect("all media are grouped by capture time");
        if current_time != Some(capture_time.unix) {
            if current_time_anchor.is_some() {
                previous_time_anchor = current_time_anchor;
            }
            current_time = Some(capture_time.unix);
            current_time_anchor = None;
        }
        previous_anchor_indexes[index] = previous_time_anchor;
        if valid_anchor(item).is_some() {
            current_time_anchor = Some(index);
        }
    }

    let mut next_anchor_indexes = vec![None; segment.len()];
    let mut next_time_anchor = None;
    let mut current_time = None;
    let mut current_time_anchor = None;
    for (index, item) in segment.iter().enumerate().rev() {
        let capture_time = item
            .capture_time
            .expect("all media are grouped by capture time");
        if current_time != Some(capture_time.unix) {
            if current_time_anchor.is_some() {
                next_time_anchor = current_time_anchor;
            }
            current_time = Some(capture_time.unix);
            current_time_anchor = None;
        }
        next_anchor_indexes[index] = next_time_anchor;
        if valid_anchor(item).is_some() {
            current_time_anchor = Some(index);
        }
    }
    (previous_anchor_indexes, next_anchor_indexes)
}

fn valid_anchor(item: &GeoAnalysisMedia) -> Option<(GeoCaptureTime, GeoCoordinate)> {
    let coordinate = item
        .gps
        .filter(|value| value.valid() && !item.gps_is_inferred)?;
    Some((item.capture_time?, coordinate))
}

fn infer_target_proposal(
    analysis_run_id: &str,
    segment: &[GeoAnalysisMedia],
    target_index: usize,
    previous_anchor_index: Option<usize>,
    next_anchor_index: Option<usize>,
    config: GeoInferenceConfig,
) -> Option<GeoProposal> {
    let target = segment.get(target_index)?;
    let target_time = target.capture_time?;
    if target.gps_is_present {
        return None;
    }

    let previous = previous_anchor_index.and_then(|index| {
        let item = &segment[index];
        let (capture_time, coordinate) = valid_anchor(item)?;
        let distance_seconds = target_time.unix.saturating_sub(capture_time.unix);
        (distance_seconds > 0 && distance_seconds <= config.max_anchor_time_delta_seconds)
            .then_some((
                anchor_from(item, capture_time, coordinate, distance_seconds),
                coordinate,
            ))
    });
    let next = next_anchor_index.and_then(|index| {
        let item = &segment[index];
        let (capture_time, coordinate) = valid_anchor(item)?;
        let distance_seconds = capture_time.unix.saturating_sub(target_time.unix);
        (distance_seconds > 0 && distance_seconds <= config.max_anchor_time_delta_seconds)
            .then_some((
                anchor_from(item, capture_time, coordinate, distance_seconds),
                coordinate,
            ))
    });

    match (previous, next) {
        (Some((previous_anchor, previous_coordinate)), Some((next_anchor, next_coordinate))) => {
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
    }
}

#[cfg(test)]
fn proposal_chunk_pages(
    analysis_run_id: &str,
    folder: &str,
    segment: &[GeoAnalysisMedia],
    proposals: Vec<GeoProposal>,
) -> Vec<GeoProposalChunk> {
    if proposals.is_empty() {
        return Vec::new();
    }
    let proposal_count = proposals.len();
    let proposal_page_count = proposal_count.div_ceil(GEO_PROPOSAL_RESULT_PAGE_SIZE);
    proposals
        .chunks(GEO_PROPOSAL_RESULT_PAGE_SIZE)
        .enumerate()
        .map(|(proposal_page, proposals)| {
            proposal_chunk_page(
                analysis_run_id,
                folder,
                segment,
                proposals.to_vec(),
                proposal_count,
                proposal_page,
                proposal_page_count,
            )
            .expect("semantic segments always have capture times")
        })
        .collect()
}

fn proposal_chunk_page(
    analysis_run_id: &str,
    folder: &str,
    segment: &[GeoAnalysisMedia],
    proposals: Vec<GeoProposal>,
    proposal_count: usize,
    proposal_page: usize,
    proposal_page_count: usize,
) -> Option<GeoProposalChunk> {
    let first = segment.first()?.capture_time?;
    let last = segment.last()?.capture_time?;
    Some(GeoProposalChunk {
        id: chunk_id(analysis_run_id, folder, first, last),
        analysis_run_id: analysis_run_id.to_string(),
        folder: folder.to_string(),
        time_range_start: first,
        time_range_end: last,
        item_count: segment.len(),
        proposal_count,
        proposal_page,
        proposal_page_count,
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
use tokio::time::{Duration, sleep, timeout};
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
const OPERATION_RESULT_SELECTION_BATCH_SIZE: usize = 64;
/// A result row remains transportable and bounded even when one logical
/// folder/time segment contains a very large number of proposals.
const GEO_PROPOSAL_RESULT_PAGE_SIZE: usize = 64;
const GEO_PROPOSAL_RESULT_PAGE_INDEX_WIDTH: usize = 6;
/// The public results endpoint pages generic result chunks, whose payload
/// sizes are operation-defined. Keep both the number of chunks and the
/// serialized payload bounded so a single administrative request cannot load
/// an unbounded report into memory.
const DEFAULT_OPERATION_RESULT_CHUNK_LIMIT: usize = 10;
const MAX_OPERATION_RESULT_CHUNKS_PER_RESPONSE: usize = 20;
const MAX_OPERATION_RESULT_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
/// Geolocation inference assumes a short, local time window. Larger values
/// would turn one folder into an impractically broad temporal segment.
const MAX_GEO_TIME_WINDOW_SECONDS: u64 = 60 * 60;
/// A scan must stay bounded even when a broad folder contains an unexpectedly
/// large media library. The server fails clearly before producing partial
/// proposals so an administrator can choose a narrower prefix.
const MAX_GEOLOCATION_SCAN_MEDIA: usize = 50_000;
/// Apply selections are persisted in the durable run input and rewritten with
/// progress updates, so bound both their count and individual identifier size.
const MAX_GEO_APPLY_SELECTION_IDS: usize = 2_000;
const MAX_GEO_APPLY_SELECTION_ID_BYTES: usize = 256;
const MULTIMEDIA_OPERATION_YIELD_MILLIS: u64 = 100;
/// Multimedia work gives repair/scrub activity a bounded priority window at
/// every batch boundary. It must not reserve the only slot of its kind forever.
const MULTIMEDIA_OPERATION_PRIORITY_WAIT_SECONDS: u64 = 5 * 60;
const HIGHER_PRIORITY_WORK_TIMEOUT_REASON: &str = "higher_priority_work_timeout";
const OPERATION_RUN_PERSIST_ATTEMPTS: usize = 3;
const OPERATION_RUN_PERSIST_RETRY_MILLIS: u64 = 100;
const OPERATION_RESULT_TYPE_GEO_PROPOSAL_CHUNK: &str = "multimedia.geolocation.proposal_chunk";
const OPERATION_RESULT_TYPE_GEO_APPLY_ITEM: &str = "multimedia.geolocation.apply_item";

#[derive(Debug, Deserialize)]
pub(super) struct OperationRunStartRequest {
    #[serde(default)]
    approve: Option<bool>,
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
    // Every operation start persists a run and may trigger background metadata
    // work, so it is never an audit dry-run. Proposing has no separate review
    // confirmation; the explicit start request is its approval, whereas apply
    // requires its request-body confirmation.
    let approval_granted = !start_is_apply || request.approve.unwrap_or(false);
    let authorization = authorize_admin_request(
        &state,
        &headers,
        "auth/operations/run",
        false,
        approval_granted,
        json!({
            "operation_id": operation_id,
            "prefix": request.prefix,
            "approve": approval_granted,
        }),
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
            message: Some(
                "Waiting for the multimedia operation slot and higher-priority maintenance work."
                    .to_string(),
            ),
            ..OperationProgress::default()
        },
        input,
        summary: None,
        error: None,
        termination_reason: None,
    };
    // Reserve the per-kind slot before persisting/spawning the task.  This is
    // admission control rather than a retry queue: a second click must not
    // create an unbounded number of durable queued runs while one scan/apply
    // is waiting for higher-priority maintenance work.
    if !try_reserve_multimedia_slot(&state, &run.run_id, !start_is_apply).await {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "A multimedia operation of this kind is already queued or running."
            })),
        )
            .into_response();
    }
    if let Err(error) = persist_operation_run_with_retry(&state, &run, "queue operation").await {
        warn!(error = %error, operation_id = %run.operation_id, "failed to persist queued operation");
        release_multimedia_slot(&state, &run.run_id, !start_is_apply).await;
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    let run_for_task = run.clone();
    let state_for_task = state.clone();
    let slot_guard = MultimediaOperationSlotGuard::new(
        Arc::clone(&state_for_task.maintenance.operations_activity),
        run_for_task.run_id.clone(),
        !start_is_apply,
    );
    tokio::spawn(async move {
        let _slot_guard = slot_guard;
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
        .map(|limit| limit.clamp(1, MAX_OPERATION_RESULT_CHUNKS_PER_RESPONSE))
        .unwrap_or(DEFAULT_OPERATION_RESULT_CHUNK_LIMIT);
    let offset = query.offset.unwrap_or_default();
    let chunks = {
        let store = read_store(&state, "operations.load_results").await;
        store
            .list_operation_result_chunks(&run_id, Some(limit.saturating_add(1)), offset)
            .await
    };
    match chunks {
        Ok(chunks) => match bounded_operation_result_page(chunks, limit, offset) {
            Ok((chunks, next_offset)) => (
                StatusCode::OK,
                Json(OperationRunResultsResponse {
                    run_id,
                    chunks,
                    next_offset,
                }),
            )
                .into_response(),
            Err(error) => {
                warn!(error = %error, "failed to bound operation result response");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        },
        Err(error) => {
            warn!(error = %error, "failed to load operation result chunks");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn bounded_operation_result_page(
    fetched_chunks: Vec<OperationResultChunk>,
    limit: usize,
    offset: usize,
) -> Result<(Vec<OperationResultChunk>, Option<usize>)> {
    debug_assert!(limit > 0);
    let has_chunk_after_requested_limit = fetched_chunks.len() > limit;
    let mut response_bytes = 0usize;
    // `limit` is clamped by the HTTP handler, but retain the fixed capacity
    // here as well so this helper cannot allocate based on an external value.
    let mut chunks = Vec::with_capacity(MAX_OPERATION_RESULT_CHUNKS_PER_RESPONSE);
    let mut stopped_for_byte_budget = false;

    for chunk in fetched_chunks.into_iter().take(limit) {
        let chunk_bytes = serde_json::to_vec(&chunk)?.len();
        // Always return the first chunk. This preserves forward progress even
        // for a future operation with one exceptionally large result row.
        if !chunks.is_empty()
            && response_bytes.saturating_add(chunk_bytes) > MAX_OPERATION_RESULT_RESPONSE_BYTES
        {
            stopped_for_byte_budget = true;
            break;
        }
        response_bytes = response_bytes.saturating_add(chunk_bytes);
        chunks.push(chunk);
    }

    let next_offset = (stopped_for_byte_budget || has_chunk_after_requested_limit)
        .then(|| offset.saturating_add(chunks.len()));
    Ok((chunks, next_offset))
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
    if config.max_anchor_time_delta_seconds > MAX_GEO_TIME_WINDOW_SECONDS
        || config.segment_gap_seconds > MAX_GEO_TIME_WINDOW_SECONDS
    {
        bail!("geolocation timing limits must not exceed {MAX_GEO_TIME_WINDOW_SECONDS} seconds");
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
    let selection_id_count = request
        .proposal_chunk_ids
        .len()
        .saturating_add(request.proposal_ids.len());
    if selection_id_count > MAX_GEO_APPLY_SELECTION_IDS {
        bail!(
            "select at most {MAX_GEO_APPLY_SELECTION_IDS} proposal chunks or proposals per apply run"
        );
    }
    if request
        .proposal_chunk_ids
        .iter()
        .chain(&request.proposal_ids)
        .any(|id| id.len() > MAX_GEO_APPLY_SELECTION_ID_BYTES)
    {
        bail!(
            "proposal chunk and proposal IDs must not exceed {MAX_GEO_APPLY_SELECTION_ID_BYTES} bytes"
        );
    }
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

fn analysis_run_has_reviewable_results(status: OperationRunStatus) -> bool {
    matches!(
        status,
        OperationRunStatus::Completed
            | OperationRunStatus::Interrupted
            | OperationRunStatus::Failed
    )
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
    if let Err(error) = wait_for_multimedia_turn(&state, &mut run).await {
        finish_failed_operation(&state, &mut run, error).await;
        release_multimedia_slot(&state, &run.run_id, true).await;
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
        release_multimedia_slot(&state, &run.run_id, true).await;
        return;
    }

    let result = async {
        let (worker, candidates) = {
            let store = read_store(&state, "operations.geo_proposal.snapshot").await;
            let inspector = store.store_index_inspector().await?;
            let worker = store.media_cache_worker();
            let candidates = geo_scan_candidates(
                inspector.current_object_entries(),
                &input.prefix,
                MAX_GEOLOCATION_SCAN_MEDIA,
            )?;
            (worker, candidates)
        };
        let total = candidates.len();
        let mut media = Vec::new();
        let mut metadata_error_count = 0usize;
        let mut sidecar_error_count = 0usize;
        for (batch_index, batch) in candidates
            .chunks(MULTIMEDIA_OPERATION_BATCH_SIZE)
            .enumerate()
        {
            wait_for_multimedia_turn(&state, &mut run).await?;
            for candidate in batch {
                let metadata = match worker.ensure_media_metadata(&candidate.manifest_hash).await {
                    Ok(metadata) => metadata,
                    Err(error) => {
                        metadata_error_count += 1;
                        warn!(
                            error = %error,
                            media_path = %candidate.path,
                            "skipping media with unreadable metadata during geolocation proposal scan"
                        );
                        continue;
                    }
                };
                let Some(metadata) = metadata else {
                    continue;
                };
                if !media_metadata_is_ready_for_geolocation(&metadata) {
                    // An incomplete, unsupported, or failed cache record
                    // does not establish that embedded GPS is absent. Do not
                    // infer a competing sidecar until the original metadata
                    // can be read and revalidated.
                    metadata_error_count += 1;
                    continue;
                }
                let sidecar_gps = {
                    let store = read_store(&state, "operations.geo_proposal.sidecar_gps").await;
                    match store.media_sidecar_gps_overlay(&candidate.path).await {
                        Ok(overlay) => overlay,
                        Err(error) => {
                            sidecar_error_count += 1;
                            warn!(
                                error = %error,
                                media_path = %candidate.path,
                                "skipping media with unreadable XMP sidecar during geolocation proposal scan"
                            );
                            // An unreadable sidecar may contain user-owned
                            // coordinates. Treat it as GPS-present so this
                            // scan cannot propose an update it cannot safely
                            // revalidate or apply.
                            storage::MediaSidecarGpsOverlay {
                                location: None,
                                has_geo_location_properties: true,
                            }
                        }
                    }
                };
                let capture_time = capture_time_for_geolocation(&candidate.path, &metadata);
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
                    path: candidate.path.clone(),
                    object_id: candidate.object_id.clone().unwrap_or_default(),
                    manifest_hash: candidate.manifest_hash.clone(),
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
            completed: Some(0),
            total: None,
            message: Some("Building semantic folder/time-segment proposals.".to_string()),
        };
        persist_operation_run(&state, &run).await?;
        let segments = semantic_geo_inference_segments(media, input.config);
        let segment_count = segments.len();
        let mut chunk_count = 0usize;
        let mut proposal_count = 0usize;
        for (index, segment) in segments.iter().enumerate() {
            wait_for_multimedia_turn(&state, &mut run).await?;
            let run_id = run.run_id.clone();
            let (segment_proposal_count, persisted_page_count) = infer_and_persist_segment(
                &state,
                &run_id,
                &mut run,
                &run_id,
                &segment.folder,
                &segment.media,
                input.config,
            )
            .await?;
            proposal_count += segment_proposal_count;
            if persisted_page_count > 0 {
                chunk_count += 1;
            }
            run.progress = OperationProgress {
                phase: Some("proposing".to_string()),
                completed: Some(index + 1),
                total: Some(segment_count),
                message: Some("Building and publishing proposal chunks for review.".to_string()),
            };
            persist_operation_run(&state, &run).await?;
            tokio::task::yield_now().await;
        }
        Ok::<_, anyhow::Error>(
            (
                chunk_count,
                proposal_count,
                total,
                metadata_error_count,
                sidecar_error_count,
            ),
        )
    }
    .await;

    match result {
        Ok((
            chunk_count,
            proposal_count,
            media_count,
            metadata_error_count,
            sidecar_error_count,
        )) => {
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
                "sidecar_error_count": sidecar_error_count,
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
    release_multimedia_slot(&state, &run.run_id, true).await;
}

async fn run_geo_apply(state: ServerState, mut run: OperationRun, input: GeoApplyRunInput) {
    if let Err(error) = wait_for_multimedia_turn(&state, &mut run).await {
        finish_failed_operation(&state, &mut run, error).await;
        release_multimedia_slot(&state, &run.run_id, false).await;
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
        release_multimedia_slot(&state, &run.run_id, false).await;
        return;
    }

    let result = async {
        let analysis = {
            let store = read_store(&state, "operations.geo_apply.analysis_run").await;
            store.load_operation_run(&input.analysis_run_id).await?
        }
        .filter(|analysis| analysis.operation_id == GEOLOCATION_PROPOSE_OPERATION_ID)
        .context("referenced analysis run does not exist or is not a geolocation proposal run")?;
        if !analysis_run_has_reviewable_results(analysis.status) {
            bail!("referenced analysis run has not produced reviewable results yet");
        }
        let worker = {
            let store = read_store(&state, "operations.geo_apply.worker").await;
            store.media_cache_worker()
        };
        let selected_chunk_ids = input
            .proposal_chunk_ids
            .iter()
            .collect::<std::collections::BTreeSet<_>>();
        let selected_proposal_ids = input
            .proposal_ids
            .iter()
            .collect::<std::collections::BTreeSet<_>>();
        let mut counters = GeoApplyCounters::default();
        let mut selected_count = 0usize;
        let mut result_offset = 0usize;
        loop {
            let result_chunks = {
                let store = read_store(&state, "operations.geo_apply.load_proposal_page").await;
                store
                    .list_operation_result_chunks(
                        &input.analysis_run_id,
                        Some(OPERATION_RESULT_SELECTION_BATCH_SIZE),
                        result_offset,
                    )
                    .await?
            };
            if result_chunks.is_empty() {
                break;
            }
            let page_len = result_chunks.len();
            result_offset += page_len;
            for result_chunk in result_chunks {
                if result_chunk.result_type != OPERATION_RESULT_TYPE_GEO_PROPOSAL_CHUNK {
                    continue;
                }
                let proposal_chunk: GeoProposalChunk = serde_json::from_value(result_chunk.payload)
                    .context("invalid persisted geolocation proposal chunk")?;
                if proposal_chunk.analysis_run_id != input.analysis_run_id {
                    bail!("proposal chunk does not belong to the referenced analysis run");
                }
                let whole_chunk_selected = selected_chunk_ids.contains(&proposal_chunk.id);
                for proposal in proposal_chunk.proposals {
                    if !whole_chunk_selected && !selected_proposal_ids.contains(&proposal.id) {
                        continue;
                    }
                    wait_for_multimedia_turn(&state, &mut run).await?;
                    let item_result =
                        apply_one_geo_proposal(&state, &worker, &input.analysis_run_id, &proposal)
                            .await;
                    counters.record(item_result.outcome);
                    let chunk = OperationResultChunk {
                        run_id: run.run_id.clone(),
                        chunk_id: format!("apply-{}", item_result.proposal_id),
                        result_type: OPERATION_RESULT_TYPE_GEO_APPLY_ITEM.to_string(),
                        created_at_unix: super::unix_ts(),
                        payload: serde_json::to_value(&item_result)?,
                    };
                    persist_operation_result_chunk(&state, &chunk).await?;
                    selected_count += 1;
                    run.progress = OperationProgress {
                        phase: Some("applying".to_string()),
                        completed: Some(selected_count),
                        total: None,
                        message: Some(
                            "Revalidating and applying selected sidecar updates.".to_string(),
                        ),
                    };
                    // Each item outcome is durable on its own. Persist the
                    // derived run counter in batches so a large apply does
                    // not acquire the global store write lock twice for
                    // every sidecar update.
                    if selected_count.is_multiple_of(MULTIMEDIA_OPERATION_BATCH_SIZE) {
                        persist_operation_run(&state, &run).await?;
                    }
                    tokio::task::yield_now().await;
                }
            }
            if page_len < OPERATION_RESULT_SELECTION_BATCH_SIZE {
                break;
            }
        }
        if selected_count == 0 {
            bail!("the selected proposal IDs do not exist in the referenced analysis run");
        }
        Ok::<_, anyhow::Error>((selected_count, counters))
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
    release_multimedia_slot(&state, &run.run_id, false).await;
}

fn media_metadata_is_ready_for_geolocation(metadata: &storage::CachedMediaMetadata) -> bool {
    metadata.status == storage::MediaCacheStatus::Ready
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
    storage::filename_capture_time(path)
        .filter(|capture_time| capture_time.has_time_of_day)
        .map(|capture_time| GeoCaptureTime {
            unix: capture_time.unix,
            source: GeoCaptureTimeSource::Filename,
            basis: GeoCaptureTimeBasis::FloatingLocal,
        })
}

#[cfg(test)]
pub(crate) async fn run_geo_apply_for_test(
    state: ServerState,
    run: OperationRun,
    analysis_run_id: &str,
    proposal_id: &str,
) {
    run_geo_apply(
        state,
        run,
        GeoApplyRunInput {
            analysis_run_id: analysis_run_id.to_string(),
            proposal_chunk_ids: Vec::new(),
            proposal_ids: vec![proposal_id.to_string()],
        },
    )
    .await;
}

#[cfg(test)]
pub(crate) fn capture_time_for_geolocation_for_test(
    path: &str,
    metadata: &storage::CachedMediaMetadata,
) -> Option<GeoCaptureTime> {
    capture_time_for_geolocation(path, metadata)
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
        match store.current_object_identity(&proposal.media_path).await {
            Ok(identity) => identity,
            Err(error) => {
                return failure(format!("failed reading current object state: {error:#}"));
            }
        }
    };
    let Some((manifest_hash, object_id)) = identity else {
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
    if !media_metadata_is_ready_for_geolocation(&metadata) {
        return stale("media metadata is no longer ready for revalidation".to_string());
    }
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

async fn persist_geo_proposal_page(
    state: &ServerState,
    run_id: &str,
    chunk: GeoProposalChunk,
) -> Result<()> {
    let result = OperationResultChunk {
        run_id: run_id.to_string(),
        // `GeoProposalChunk::id` stays the semantic folder/time-segment ID;
        // only persistence pages receive an implementation-specific suffix.
        chunk_id: geo_proposal_page_result_chunk_id(&chunk.id, chunk.proposal_page),
        result_type: OPERATION_RESULT_TYPE_GEO_PROPOSAL_CHUNK.to_string(),
        created_at_unix: super::unix_ts(),
        payload: serde_json::to_value(&chunk)?,
    };
    persist_operation_result_chunk(state, &result).await
}

fn geo_proposal_page_result_chunk_id(semantic_chunk_id: &str, proposal_page: usize) -> String {
    format!(
        "{semantic_chunk_id}-page-{proposal_page:0width$}",
        width = GEO_PROPOSAL_RESULT_PAGE_INDEX_WIDTH,
    )
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

struct MultimediaOperationSlotGuard {
    activity: Arc<tokio::sync::Mutex<OperationActivityRuntime>>,
    run_id: String,
    is_scan: bool,
}

impl MultimediaOperationSlotGuard {
    fn new(
        activity: Arc<tokio::sync::Mutex<OperationActivityRuntime>>,
        run_id: String,
        is_scan: bool,
    ) -> Self {
        Self {
            activity,
            run_id,
            is_scan,
        }
    }
}

impl Drop for MultimediaOperationSlotGuard {
    fn drop(&mut self) {
        // Operation workers are Tokio tasks. A detached cleanup task also runs
        // when the worker is aborted or panics, which prevents stale admission
        // control from blocking all later runs of this operation kind.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let activity = Arc::clone(&self.activity);
        let run_id = self.run_id.clone();
        let is_scan = self.is_scan;
        drop(handle.spawn(async move {
            release_multimedia_slot_activity(&activity, &run_id, is_scan).await;
        }));
    }
}

fn multimedia_slot(activity: &mut OperationActivityRuntime, is_scan: bool) -> &mut Option<String> {
    if is_scan {
        &mut activity.multimedia_scan_run_id
    } else {
        &mut activity.multimedia_apply_run_id
    }
}

pub(crate) async fn try_reserve_multimedia_slot(
    state: &ServerState,
    run_id: &str,
    is_scan: bool,
) -> bool {
    let mut activity = state.maintenance.operations_activity.lock().await;
    let active = multimedia_slot(&mut activity, is_scan);
    if active.is_some() {
        return false;
    }
    *active = Some(run_id.to_string());
    true
}

pub(crate) async fn release_multimedia_slot(state: &ServerState, run_id: &str, is_scan: bool) {
    release_multimedia_slot_activity(&state.maintenance.operations_activity, run_id, is_scan).await;
}

async fn release_multimedia_slot_activity(
    activity: &tokio::sync::Mutex<OperationActivityRuntime>,
    run_id: &str,
    is_scan: bool,
) {
    let mut activity = activity.lock().await;
    let active = multimedia_slot(&mut activity, is_scan);
    if active.as_deref() == Some(run_id) {
        *active = None;
    }
}

/// Waits for an uninterrupted higher-priority maintenance phase at any
/// multimedia batch boundary. Timeout is a terminal run failure because the
/// serialized slot has no user-visible cancellation mechanism in V1.
async fn wait_for_multimedia_turn(state: &ServerState, run: &mut OperationRun) -> Result<()> {
    if timeout(
        Duration::from_secs(MULTIMEDIA_OPERATION_PRIORITY_WAIT_SECONDS),
        wait_for_higher_priority_work(state),
    )
    .await
    .is_err()
    {
        run.termination_reason = Some(HIGHER_PRIORITY_WORK_TIMEOUT_REASON.to_string());
        bail!(
            "higher-priority repair or scrub work did not become idle within {MULTIMEDIA_OPERATION_PRIORITY_WAIT_SECONDS} seconds"
        );
    }
    Ok(())
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
    fn geolocation_requires_ready_media_metadata() {
        let mut metadata = cached_metadata(Some(1_700_000_000), Some(true), None);
        assert!(media_metadata_is_ready_for_geolocation(&metadata));

        for status in [
            storage::MediaCacheStatus::Incomplete,
            storage::MediaCacheStatus::Unsupported,
            storage::MediaCacheStatus::Failed,
        ] {
            metadata.status = status;
            assert!(!media_metadata_is_ready_for_geolocation(&metadata));
        }
    }

    #[test]
    fn operation_result_responses_stop_at_the_serialized_byte_budget() {
        let large_chunk = |index| OperationResultChunk {
            run_id: "run".to_string(),
            chunk_id: format!("chunk-{index}"),
            result_type: "test".to_string(),
            created_at_unix: 0,
            payload: json!({ "contents": "x".repeat(MAX_OPERATION_RESULT_RESPONSE_BYTES / 2 + 1) }),
        };

        let (chunks, next_offset) = bounded_operation_result_page(
            vec![large_chunk(1), large_chunk(2), large_chunk(3)],
            3,
            12,
        )
        .expect("result page is serializable");

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk_id, "chunk-1");
        assert_eq!(next_offset, Some(13));
    }

    #[test]
    fn operation_result_responses_keep_chunk_limit_pagination() {
        let chunk = |index| OperationResultChunk {
            run_id: "run".to_string(),
            chunk_id: format!("chunk-{index}"),
            result_type: "test".to_string(),
            created_at_unix: 0,
            payload: json!({ "index": index }),
        };

        let (chunks, next_offset) =
            bounded_operation_result_page(vec![chunk(1), chunk(2), chunk(3)], 2, 4)
                .expect("result page is serializable");

        assert_eq!(chunks.len(), 2);
        assert_eq!(next_offset, Some(6));
    }

    #[tokio::test]
    async fn multimedia_slot_guard_releases_a_slot_when_its_worker_ends() {
        let activity = std::sync::Arc::new(tokio::sync::Mutex::new(OperationActivityRuntime {
            multimedia_scan_run_id: Some("run-1".to_string()),
            multimedia_apply_run_id: None,
        }));
        {
            let _guard = MultimediaOperationSlotGuard::new(
                std::sync::Arc::clone(&activity),
                "run-1".to_string(),
                true,
            );
        }

        for _ in 0..32 {
            let released = {
                let mut current = activity.lock().await;
                multimedia_slot(&mut current, true).is_none()
            };
            if released {
                return;
            }
            tokio::task::yield_now().await;
        }

        panic!("the dropped operation worker did not release its multimedia slot");
    }

    #[test]
    fn geo_scan_candidates_are_bounded_and_sorted_without_partial_results() {
        let candidates = geo_scan_candidates(
            [
                ("album/b.jpg", "hash-b", Some("object-b")),
                ("album/a.jpg", "hash-a", Some("object-a")),
                ("album/readme.txt", "hash-text", Some("object-text")),
                ("other/c.jpg", "hash-c", Some("object-c")),
            ],
            "album/",
            2,
        )
        .expect("two media candidates fit the scope limit");
        assert_eq!(
            candidates,
            vec![
                GeoScanCandidate {
                    path: "album/a.jpg".to_string(),
                    manifest_hash: "hash-a".to_string(),
                    object_id: Some("object-a".to_string()),
                },
                GeoScanCandidate {
                    path: "album/b.jpg".to_string(),
                    manifest_hash: "hash-b".to_string(),
                    object_id: Some("object-b".to_string()),
                },
            ]
        );

        let error = geo_scan_candidates(
            [
                ("album/a.jpg", "hash-a", None),
                ("album/b.jpg", "hash-b", None),
            ],
            "album/",
            1,
        )
        .expect_err("an oversized scope must fail rather than truncate proposals");
        assert!(error.to_string().contains("choose a narrower prefix"));
    }

    #[test]
    fn geo_proposal_page_result_chunk_ids_sort_by_numeric_page_order() {
        let mut chunk_ids = (0..12)
            .rev()
            .map(|page| geo_proposal_page_result_chunk_id("semantic-chunk", page))
            .collect::<Vec<_>>();
        chunk_ids.sort();

        assert_eq!(
            chunk_ids,
            (0..12)
                .map(|page| geo_proposal_page_result_chunk_id("semantic-chunk", page))
                .collect::<Vec<_>>(),
        );
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
    fn large_semantic_chunk_is_persisted_as_bounded_technical_pages() {
        let mut source = vec![media("trip/a.jpg", Some(0), Some((0.0, 0.0)))];
        source.extend((1..=GEO_PROPOSAL_RESULT_PAGE_SIZE + 1).map(|second| {
            media(
                &format!("trip/target-{second:03}.jpg"),
                Some(second as u64),
                None,
            )
        }));
        source.push(media(
            "trip/b.jpg",
            Some((GEO_PROPOSAL_RESULT_PAGE_SIZE + 2) as u64),
            Some((0.0, 0.001)),
        ));

        let chunks = infer_geolocation_proposals(
            "run",
            source,
            GeoInferenceConfig {
                max_anchor_time_delta_seconds: 1_000,
                segment_gap_seconds: 1_000,
                ..GeoInferenceConfig::default()
            },
        );

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].id, chunks[1].id);
        assert_eq!(chunks[0].proposal_count, GEO_PROPOSAL_RESULT_PAGE_SIZE + 1);
        assert_eq!(chunks[0].proposal_page, 0);
        assert_eq!(chunks[1].proposal_page, 1);
        assert_eq!(chunks[0].proposal_page_count, 2);
        assert_eq!(chunks[1].proposal_page_count, 2);
        assert_eq!(chunks[0].proposals.len(), GEO_PROPOSAL_RESULT_PAGE_SIZE);
        assert_eq!(chunks[1].proposals.len(), 1);
    }

    #[test]
    fn proposal_time_windows_are_bounded_at_the_api_boundary() {
        let request = OperationRunStartRequest {
            approve: None,
            prefix: Some("trip".to_string()),
            max_anchor_time_delta_seconds: Some(MAX_GEO_TIME_WINDOW_SECONDS + 1),
            segment_gap_seconds: None,
            max_anchor_speed_kmh: None,
            analysis_run_id: None,
            proposal_chunk_ids: Vec::new(),
            proposal_ids: Vec::new(),
        };

        let error = validated_geo_proposal_input(&request)
            .expect_err("unbounded geolocation timing input must be rejected");
        assert!(
            error
                .to_string()
                .contains("geolocation timing limits must not exceed")
        );
    }

    #[test]
    fn apply_selection_inputs_are_bounded_before_they_become_durable_run_input() {
        let mut request = OperationRunStartRequest {
            approve: Some(true),
            prefix: None,
            max_anchor_time_delta_seconds: None,
            segment_gap_seconds: None,
            max_anchor_speed_kmh: None,
            analysis_run_id: Some("analysis-run".to_string()),
            proposal_chunk_ids: Vec::new(),
            proposal_ids: vec!["proposal".to_string(); MAX_GEO_APPLY_SELECTION_IDS + 1],
        };
        let error = validated_geo_apply_input(&request)
            .expect_err("oversized proposal selection must be rejected");
        assert!(error.to_string().contains("select at most"));

        request.proposal_ids = vec!["x".repeat(MAX_GEO_APPLY_SELECTION_ID_BYTES + 1)];
        let error = validated_geo_apply_input(&request)
            .expect_err("oversized proposal identifier must be rejected");
        assert!(error.to_string().contains("must not exceed"));
    }

    #[test]
    fn terminal_analysis_runs_with_persisted_results_remain_reviewable() {
        assert!(analysis_run_has_reviewable_results(
            OperationRunStatus::Completed
        ));
        assert!(analysis_run_has_reviewable_results(
            OperationRunStatus::Interrupted
        ));
        assert!(analysis_run_has_reviewable_results(
            OperationRunStatus::Failed
        ));
        assert!(!analysis_run_has_reviewable_results(
            OperationRunStatus::Queued
        ));
        assert!(!analysis_run_has_reviewable_results(
            OperationRunStatus::Running
        ));
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
    fn same_timestamp_anchor_does_not_hide_usable_neighbouring_anchors() {
        let result = proposals(
            vec![
                media("trip/a.jpg", Some(0), Some((0.0, 0.0))),
                media("trip/same-time-anchor.jpg", Some(60), Some((0.0, 0.005))),
                media("trip/target.jpg", Some(60), None),
                media("trip/b.jpg", Some(120), Some((0.0, 0.01))),
            ],
            GeoInferenceConfig::default(),
        );

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].method, GeoInferenceMethod::Interpolation);
        assert_eq!(
            result[0]
                .previous_anchor
                .as_ref()
                .map(|anchor| anchor.path.as_str()),
            Some("trip/a.jpg")
        );
        assert_eq!(
            result[0]
                .next_anchor
                .as_ref()
                .map(|anchor| anchor.path.as_str()),
            Some("trip/b.jpg")
        );
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
    fn unparseable_existing_gps_is_not_an_inference_target() {
        let mut existing_but_unparseable = media("trip/target.jpg", Some(120), None);
        existing_but_unparseable.gps_is_present = true;
        let result = proposals(
            vec![
                media("trip/before.jpg", Some(0), Some((47.0, 8.0))),
                existing_but_unparseable,
                media("trip/after.jpg", Some(240), Some((47.0005, 8.0005))),
            ],
            GeoInferenceConfig::default(),
        );
        assert!(result.is_empty());
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

        assert_eq!(
            capture_time_for_geolocation("trip/IMG-20240102-WA0001.jpg", &metadata),
            None,
            "a date-only filename has no safe time-of-day for geo inference"
        );

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
