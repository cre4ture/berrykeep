use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::storage;

use super::StoreIndexMediaFilter;

const GALLERY_MAP_TOKEN_PREFIX: &str = "gm1_";
const MAX_GALLERY_MAP_TOKEN_LENGTH: usize = 65_536;
const MAX_GALLERY_MAP_RESOLUTION: u32 = 1 << 25;
const MAPLIBRE_WORLD_SIZE_AT_ZOOM_ZERO_PX: f64 = 512.0;
const DEFAULT_GALLERY_MAP_CLUSTER_CELL_WIDTH_PX: f64 = 32.0;
const GALLERY_MAP_CLUSTER_CELL_WIDTH_OPTIONS_PX: [f64; 5] = [16.0, 24.0, 32.0, 48.0, 64.0];
const GALLERY_MAP_CLUSTER_ZOOM_STEP: f64 = 0.5;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(super) struct GalleryMapQueryTokenPayload {
    pub(super) history_id: String,
    pub(super) revision: u64,
    pub(super) prefix: String,
    pub(super) depth: usize,
    pub(super) media_filter: StoreIndexMediaFilter,
    pub(super) viewport: GalleryMapViewport,
    pub(super) resolution: u32,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(super) struct GalleryMapViewport {
    pub(super) south: f64,
    pub(super) west: f64,
    pub(super) north: f64,
    pub(super) east: f64,
}

pub(super) fn encode_gallery_map_query_token(payload: &GalleryMapQueryTokenPayload) -> String {
    let encoded = serde_json::to_vec(payload)
        .map(|payload| BASE64_URL_SAFE_NO_PAD.encode(payload))
        .expect("validated gallery map token payload should serialize");
    format!("{GALLERY_MAP_TOKEN_PREFIX}{encoded}")
}

pub(super) fn decode_gallery_map_query_token(token: &str) -> Option<GalleryMapQueryTokenPayload> {
    if token.len() > MAX_GALLERY_MAP_TOKEN_LENGTH {
        return None;
    }
    let encoded = token.strip_prefix(GALLERY_MAP_TOKEN_PREFIX)?;
    let bytes = BASE64_URL_SAFE_NO_PAD.decode(encoded).ok()?;
    let payload = serde_json::from_slice::<GalleryMapQueryTokenPayload>(&bytes).ok()?;
    if Uuid::parse_str(&payload.history_id).ok()?.to_string() != payload.history_id
        || payload.depth == 0
        || payload.prefix != payload.prefix.trim().trim_matches('/')
        || payload.resolution == 0
        || payload.resolution > MAX_GALLERY_MAP_RESOLUTION
        || !gallery_map_viewport_is_valid(payload.viewport)
    {
        return None;
    }
    Some(payload)
}

pub(super) fn gallery_map_viewport_is_valid(viewport: GalleryMapViewport) -> bool {
    viewport.south.is_finite()
        && viewport.west.is_finite()
        && viewport.north.is_finite()
        && viewport.east.is_finite()
        && (-90.0..=90.0).contains(&viewport.south)
        && (-90.0..=90.0).contains(&viewport.north)
        && viewport.south <= viewport.north
        && (-180.0..=180.0).contains(&viewport.west)
        && (-180.0..=180.0).contains(&viewport.east)
}

/// Returns whether `inner` is fully covered by `outer`, including wrapped longitude intervals.
pub(super) fn gallery_map_viewport_contains(
    outer: GalleryMapViewport,
    inner: GalleryMapViewport,
) -> bool {
    outer.south <= inner.south
        && inner.north <= outer.north
        && gallery_map_longitude_interval_contains(outer, inner)
}

fn gallery_map_longitude_interval_contains(
    outer: GalleryMapViewport,
    inner: GalleryMapViewport,
) -> bool {
    let outer_span = gallery_map_longitude_span(outer);
    let inner_span = gallery_map_longitude_span(inner);
    if outer_span >= 360.0 {
        return true;
    }
    if inner_span > outer_span {
        return false;
    }

    let outer_start = gallery_map_normalized_longitude(outer.west);
    let mut inner_start = gallery_map_normalized_longitude(inner.west);
    if inner_start < outer_start {
        inner_start += 360.0;
    }
    inner_start + inner_span <= outer_start + outer_span
}

fn gallery_map_longitude_span(viewport: GalleryMapViewport) -> f64 {
    let raw_span = viewport.east - viewport.west;
    if raw_span.abs() >= 360.0 {
        360.0
    } else {
        raw_span.rem_euclid(360.0)
    }
}

fn gallery_map_normalized_longitude(longitude: f64) -> f64 {
    (longitude + 180.0).rem_euclid(360.0)
}

pub(super) fn gallery_map_cluster_cell_width_px(
    requested_cell_width_px: Option<f64>,
) -> Result<f64, &'static str> {
    let Some(requested_cell_width_px) = requested_cell_width_px else {
        return Ok(DEFAULT_GALLERY_MAP_CLUSTER_CELL_WIDTH_PX);
    };
    if !requested_cell_width_px.is_finite() {
        return Err("cluster_cell_size_px must be a finite number");
    }

    let min_cell_width_px = GALLERY_MAP_CLUSTER_CELL_WIDTH_OPTIONS_PX[0];
    let max_cell_width_px = *GALLERY_MAP_CLUSTER_CELL_WIDTH_OPTIONS_PX
        .last()
        .expect("gallery map cluster cell width options must not be empty");
    let bounded_cell_width_px = requested_cell_width_px.clamp(min_cell_width_px, max_cell_width_px);
    Ok(*GALLERY_MAP_CLUSTER_CELL_WIDTH_OPTIONS_PX
        .iter()
        .min_by(|left, right| {
            (f64::abs(**left - bounded_cell_width_px))
                .total_cmp(&f64::abs(**right - bounded_cell_width_px))
        })
        .expect("gallery map cluster cell width options must not be empty"))
}

pub(super) fn gallery_map_resolution_for_zoom(zoom: f64, cell_width_px: f64) -> u32 {
    let zoom = if zoom.is_finite() {
        zoom.clamp(0.0, 20.0)
    } else {
        0.0
    };
    // A continuous resolution moves every grid boundary at every camera update. Round upward
    // to half zoom levels so cells remain no wider than the client-selected CSS-pixel target
    // while small zoom changes keep the same cluster grid.
    let grid_zoom = (zoom / GALLERY_MAP_CLUSTER_ZOOM_STEP).ceil() * GALLERY_MAP_CLUSTER_ZOOM_STEP;
    let resolution =
        (MAPLIBRE_WORLD_SIZE_AT_ZOOM_ZERO_PX * 2.0_f64.powf(grid_zoom) / cell_width_px).ceil();

    resolution.clamp(1.0, f64::from(MAX_GALLERY_MAP_RESOLUTION)) as u32
}

pub(super) fn storage_media_filter(
    media_filter: StoreIndexMediaFilter,
) -> storage::GalleryIndexMediaFilter {
    match media_filter {
        StoreIndexMediaFilter::All => storage::GalleryIndexMediaFilter::All,
        StoreIndexMediaFilter::Image => storage::GalleryIndexMediaFilter::Image,
        StoreIndexMediaFilter::Video => storage::GalleryIndexMediaFilter::Video,
    }
}

pub(super) fn storage_viewport(viewport: GalleryMapViewport) -> storage::GalleryViewportBounds {
    storage::GalleryViewportBounds {
        south: canonical_bound(viewport.south),
        west: canonical_bound(viewport.west),
        north: canonical_bound(viewport.north),
        east: canonical_bound(viewport.east),
    }
}

pub(super) fn encode_gallery_map_cluster_id(cell_x: u32, cell_y: u32) -> String {
    format!("{cell_x}_{cell_y}")
}

pub(super) fn decode_gallery_map_cluster_id(
    cluster_id: &str,
    resolution: u32,
) -> Option<(u32, u32)> {
    if cluster_id.len() > 64 {
        return None;
    }
    let (cell_x, cell_y) = cluster_id.split_once('_')?;
    let cell_x = cell_x.parse::<u32>().ok()?;
    let cell_y = cell_y.parse::<u32>().ok()?;
    (cell_x < resolution && cell_y < resolution).then_some((cell_x, cell_y))
}

fn canonical_bound(value: f64) -> f64 {
    if value == 0.0 { 0.0 } else { value }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token_payload() -> GalleryMapQueryTokenPayload {
        GalleryMapQueryTokenPayload {
            history_id: Uuid::new_v4().to_string(),
            revision: 42,
            prefix: "gallery".to_string(),
            depth: 64,
            media_filter: StoreIndexMediaFilter::Image,
            viewport: GalleryMapViewport {
                south: -85.0,
                west: 170.0,
                north: 85.0,
                east: -170.0,
            },
            resolution: 1024,
        }
    }

    #[test]
    fn map_query_token_round_trips_antimeridian_scope() {
        let payload = token_payload();
        assert_eq!(
            decode_gallery_map_query_token(&encode_gallery_map_query_token(&payload)),
            Some(payload)
        );
    }

    #[test]
    fn map_query_token_rejects_out_of_range_resolution() {
        let mut payload = token_payload();
        payload.resolution = MAX_GALLERY_MAP_RESOLUTION + 1;
        assert!(
            decode_gallery_map_query_token(&encode_gallery_map_query_token(&payload)).is_none()
        );
    }

    #[test]
    fn map_cluster_id_is_bounded_by_resolution() {
        assert_eq!(decode_gallery_map_cluster_id("7_3", 8), Some((7, 3)));
        assert_eq!(decode_gallery_map_cluster_id("8_3", 8), None);
        assert_eq!(decode_gallery_map_cluster_id("7_8", 8), None);
    }

    #[test]
    fn map_viewport_containment_preserves_antimeridian_intervals() {
        let world = GalleryMapViewport {
            south: -90.0,
            west: -180.0,
            north: 90.0,
            east: 180.0,
        };
        let prefetched_antimeridian_viewport = GalleryMapViewport {
            south: -10.0,
            west: 160.0,
            north: 10.0,
            east: -160.0,
        };
        let visible_antimeridian_viewport = GalleryMapViewport {
            south: -5.0,
            west: 170.0,
            north: 5.0,
            east: -170.0,
        };
        let outside_viewport = GalleryMapViewport {
            south: -5.0,
            west: 150.0,
            north: 5.0,
            east: 170.0,
        };

        assert!(gallery_map_viewport_contains(
            world,
            prefetched_antimeridian_viewport
        ));
        assert!(gallery_map_viewport_contains(
            prefetched_antimeridian_viewport,
            visible_antimeridian_viewport
        ));
        assert!(!gallery_map_viewport_contains(
            prefetched_antimeridian_viewport,
            outside_viewport
        ));
    }

    #[test]
    fn map_grid_resolution_quantizes_fractional_zoom_at_the_client_cell_width() {
        let cell_width_px = gallery_map_cluster_cell_width_px(None).unwrap();
        for zoom in [0.0, 0.25, 1.2, 3.75, 12.4, 20.0] {
            let world_size = MAPLIBRE_WORLD_SIZE_AT_ZOOM_ZERO_PX * 2.0_f64.powf(zoom);
            let resolution = f64::from(gallery_map_resolution_for_zoom(zoom, cell_width_px));

            assert!(
                world_size / resolution <= cell_width_px,
                "zoom level {zoom} has an oversized cluster cell"
            );
        }

        assert_eq!(gallery_map_resolution_for_zoom(1.0, cell_width_px), 1 << 5);
        assert_eq!(gallery_map_resolution_for_zoom(3.0, cell_width_px), 1 << 7);
        assert_eq!(
            gallery_map_resolution_for_zoom(3.01, cell_width_px),
            gallery_map_resolution_for_zoom(3.49, cell_width_px)
        );
        assert_ne!(
            gallery_map_resolution_for_zoom(3.49, cell_width_px),
            gallery_map_resolution_for_zoom(3.51, cell_width_px)
        );
    }

    #[test]
    fn map_cluster_cell_width_is_bounded_and_quantized() {
        assert_eq!(
            gallery_map_cluster_cell_width_px(None),
            Ok(DEFAULT_GALLERY_MAP_CLUSTER_CELL_WIDTH_PX)
        );
        assert_eq!(gallery_map_cluster_cell_width_px(Some(8.0)), Ok(16.0));
        assert_eq!(gallery_map_cluster_cell_width_px(Some(28.0)), Ok(24.0));
        assert_eq!(gallery_map_cluster_cell_width_px(Some(49.0)), Ok(48.0));
        assert_eq!(gallery_map_cluster_cell_width_px(Some(128.0)), Ok(64.0));
        assert_eq!(
            gallery_map_cluster_cell_width_px(Some(f64::NAN)),
            Err("cluster_cell_size_px must be a finite number")
        );
    }

    #[test]
    fn map_grid_resolution_reflects_the_client_cell_width() {
        assert_eq!(
            gallery_map_resolution_for_zoom(
                1.0,
                gallery_map_cluster_cell_width_px(Some(16.0)).unwrap()
            ),
            1 << 6
        );
        assert_eq!(
            gallery_map_resolution_for_zoom(
                1.0,
                gallery_map_cluster_cell_width_px(Some(64.0)).unwrap()
            ),
            1 << 4
        );
        assert_eq!(
            gallery_map_resolution_for_zoom(
                20.0,
                gallery_map_cluster_cell_width_px(Some(16.0)).unwrap()
            ),
            1 << 25
        );
    }
}
