use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::storage;

use super::StoreIndexMediaFilter;

const GALLERY_MAP_TOKEN_PREFIX: &str = "gm1_";
const MAX_GALLERY_MAP_TOKEN_LENGTH: usize = 65_536;
const MAX_GALLERY_MAP_RESOLUTION: u32 = 1 << 22;

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
        || !payload.resolution.is_power_of_two()
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

pub(super) fn gallery_map_resolution_for_zoom(zoom: u8) -> u32 {
    1u32 << u32::from(zoom.min(20).saturating_add(2))
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
    fn map_query_token_rejects_non_power_of_two_resolution() {
        let mut payload = token_payload();
        payload.resolution = 3;
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
}
