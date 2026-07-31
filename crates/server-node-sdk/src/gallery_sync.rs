use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::storage;

use super::{StoreIndexMediaFilter, StoreIndexSortOrder};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(super) struct GallerySyncViewport {
    south: f64,
    west: f64,
    north: f64,
    east: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(super) struct GallerySyncScope {
    pub(super) prefix: String,
    pub(super) depth: usize,
    pub(super) media_filter: StoreIndexMediaFilter,
    pub(super) captured_sort: StoreIndexSortOrder,
    pub(super) viewport: Option<GallerySyncViewport>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(super) struct GallerySyncTokenPayload {
    pub(super) history_id: String,
    pub(super) revision: u64,
    pub(super) scope: GallerySyncScope,
}

pub(super) fn encode_gallery_sync_token(payload: &GallerySyncTokenPayload) -> String {
    let encoded = serde_json::to_vec(payload)
        .map(|payload| BASE64_URL_SAFE_NO_PAD.encode(payload))
        .expect("validated gallery sync token payload should serialize");
    format!("g2_{encoded}")
}

pub(super) fn decode_gallery_sync_token(token: &str) -> Option<GallerySyncTokenPayload> {
    if token.len() > 65_536 {
        return None;
    }
    let encoded = token.strip_prefix("g2_")?;
    let bytes = BASE64_URL_SAFE_NO_PAD.decode(encoded).ok()?;
    let payload = serde_json::from_slice::<GallerySyncTokenPayload>(&bytes).ok()?;
    if Uuid::parse_str(&payload.history_id).ok()?.to_string() != payload.history_id
        || payload.scope.depth == 0
        || payload.scope.prefix != payload.scope.prefix.trim().trim_matches('/')
        || !matches!(
            payload.scope.captured_sort,
            StoreIndexSortOrder::CapturedAsc | StoreIndexSortOrder::CapturedDesc
        )
        || !gallery_sync_viewport_is_valid(payload.scope.viewport.as_ref())
    {
        return None;
    }
    Some(payload)
}

pub(super) fn gallery_delta_scope_from_sync(
    scope: &GallerySyncScope,
) -> storage::GalleryDeltaScope {
    storage::GalleryDeltaScope {
        prefix: scope.prefix.clone(),
        depth: scope.depth,
        media_filter: match scope.media_filter {
            StoreIndexMediaFilter::All => storage::GalleryIndexMediaFilter::All,
            StoreIndexMediaFilter::Image => storage::GalleryIndexMediaFilter::Image,
            StoreIndexMediaFilter::Video => storage::GalleryIndexMediaFilter::Video,
        },
        captured_sort: match scope.captured_sort {
            StoreIndexSortOrder::CapturedAsc => storage::GalleryIndexCapturedSort::Asc,
            StoreIndexSortOrder::CapturedDesc => storage::GalleryIndexCapturedSort::Desc,
            _ => unreachable!("gallery sync token sort was validated"),
        },
        viewport: scope
            .viewport
            .as_ref()
            .map(|viewport| storage::GalleryViewportBounds {
                south: viewport.south,
                west: viewport.west,
                north: viewport.north,
                east: viewport.east,
            }),
    }
}

pub(super) fn gallery_sync_scope_from_query(
    query: &storage::GalleryIndexQuery,
) -> GallerySyncScope {
    GallerySyncScope {
        prefix: query.prefix.trim().trim_matches('/').to_string(),
        depth: query.depth,
        media_filter: match query.media_filter {
            storage::GalleryIndexMediaFilter::All => StoreIndexMediaFilter::All,
            storage::GalleryIndexMediaFilter::Image => StoreIndexMediaFilter::Image,
            storage::GalleryIndexMediaFilter::Video => StoreIndexMediaFilter::Video,
        },
        captured_sort: match query.captured_sort {
            storage::GalleryIndexCapturedSort::Asc => StoreIndexSortOrder::CapturedAsc,
            storage::GalleryIndexCapturedSort::Desc => StoreIndexSortOrder::CapturedDesc,
        },
        viewport: query.viewport.map(|viewport| GallerySyncViewport {
            south: canonical_gallery_bound(viewport.south),
            west: canonical_gallery_bound(viewport.west),
            north: canonical_gallery_bound(viewport.north),
            east: canonical_gallery_bound(viewport.east),
        }),
    }
}

fn gallery_sync_viewport_is_valid(viewport: Option<&GallerySyncViewport>) -> bool {
    let Some(viewport) = viewport else {
        return true;
    };
    viewport.south.is_finite()
        && viewport.west.is_finite()
        && viewport.north.is_finite()
        && viewport.east.is_finite()
        && (-90.0..=90.0).contains(&viewport.south)
        && (-90.0..=90.0).contains(&viewport.north)
        && viewport.south <= viewport.north
        && (-180.0..=180.0).contains(&viewport.west)
        && (-180.0..=180.0).contains(&viewport.east)
        && !(viewport.south == 0.0 && viewport.south.is_sign_negative())
        && !(viewport.west == 0.0 && viewport.west.is_sign_negative())
        && !(viewport.north == 0.0 && viewport.north.is_sign_negative())
        && !(viewport.east == 0.0 && viewport.east.is_sign_negative())
}

fn canonical_gallery_bound(value: f64) -> f64 {
    if value == 0.0 { 0.0 } else { value }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token_payload() -> GallerySyncTokenPayload {
        GallerySyncTokenPayload {
            history_id: Uuid::new_v4().to_string(),
            revision: 42,
            scope: GallerySyncScope {
                prefix: "gallery".to_string(),
                depth: 64,
                media_filter: StoreIndexMediaFilter::Image,
                captured_sort: StoreIndexSortOrder::CapturedDesc,
                viewport: None,
            },
        }
    }

    #[test]
    fn gallery_sync_token_rejects_unknown_fields() {
        let mut value = serde_json::to_value(token_payload()).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("unexpected".to_string(), serde_json::Value::Bool(true));
        let token = format!(
            "g2_{}",
            BASE64_URL_SAFE_NO_PAD.encode(serde_json::to_vec(&value).unwrap())
        );
        assert!(decode_gallery_sync_token(&token).is_none());
    }

    #[test]
    fn gallery_sync_scope_canonicalizes_negative_zero() {
        let query = |zero| storage::GalleryIndexQuery {
            prefix: "/gallery/".to_string(),
            depth: 64,
            media_filter: storage::GalleryIndexMediaFilter::Image,
            captured_sort: storage::GalleryIndexCapturedSort::Desc,
            offset: 0,
            limit: 100,
            viewport: Some(storage::GalleryViewportBounds {
                south: zero,
                west: zero,
                north: 10.0,
                east: 20.0,
            }),
        };
        let negative = gallery_sync_scope_from_query(&query(-0.0));
        let positive = gallery_sync_scope_from_query(&query(0.0));
        assert_eq!(negative, positive);
        assert_eq!(negative.prefix, "gallery");
        assert!(!negative.viewport.unwrap().south.is_sign_negative());
    }
}
