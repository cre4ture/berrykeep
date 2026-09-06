use std::io::{BufRead, Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use axum::Router;
use axum::extract::State;
use berry_thumbnailer::error::ThumbError as BerryThumbnailError;
use berry_thumbnailer::{ThumbnailOptions, ThumbnailSize, create_thumbnails_with_options};
use exif::{In, Reader as ExifReader, Tag, Value};
use image::codecs::jpeg::JpegEncoder;
use image::metadata::Orientation;
use image::{DynamicImage, ImageFormat, ImageReader};
use imagesize::{Compression as HeifCompression, ImageType as SizeImageType};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};
use tokio::fs;
use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::task;
use tokio::time::{Duration, Instant, timeout};
use tracing::{info, warn};
use uuid::Uuid;

use super::manifest_reader::ManifestReader;
use super::media_tools::{
    FFMPEG_TIMEOUT_SECS, FFPROBE_TIMEOUT_SECS, FfprobeOutput, FfprobeTags, MediaToolPaths,
    VIDEO_THUMBNAIL_SEEK_FRACTION, VIDEO_THUMBNAIL_SEEK_MAX_SECS, VIDEO_THUMBNAIL_SEEK_MIN_SECS,
    VIDEO_THUMBNAIL_UNKNOWN_DURATION_SEEK_SECS,
};
use super::{
    MetadataStore, ObjectManifest, StorageContentKind, StoragePool, TOMBSTONE_MANIFEST_HASH,
    content_fingerprint_from_manifest, hash_hex, unix_ts, write_atomic,
};

pub(super) const MEDIA_CACHE_SCHEMA_VERSION: u32 = 11;
pub(super) const MEDIA_CACHE_INCOMPLETE_RETRY_SECS: u64 = 10 * 60;
const MEDIA_CACHE_INCOMPLETE_RETRY_SECS_ENV: &str = "IRONMESH_MEDIA_CACHE_INCOMPLETE_RETRY_SECS";
const MEDIA_CACHE_BUILD_TOTAL_PERMITS_ENV: &str = "IRONMESH_MEDIA_CACHE_BUILD_TOTAL_PERMITS";
const MEDIA_CACHE_BUILD_BYTES_PER_PERMIT_ENV: &str = "IRONMESH_MEDIA_CACHE_BUILD_BYTES_PER_PERMIT";
const MEDIA_CACHE_IMAGE_MAX_DIMENSION_ENV: &str = "IRONMESH_MEDIA_CACHE_IMAGE_MAX_DIMENSION";
const MEDIA_CACHE_IMAGE_MAX_PIXELS_ENV: &str = "IRONMESH_MEDIA_CACHE_IMAGE_MAX_PIXELS";
const MEDIA_CACHE_IMAGE_MAX_DECODE_BYTES_ENV: &str = "IRONMESH_MEDIA_CACHE_IMAGE_MAX_DECODE_BYTES";
const DEFAULT_MEDIA_CACHE_BUILD_TOTAL_PERMITS: u32 = 8;
const DEFAULT_MEDIA_CACHE_BUILD_BYTES_PER_PERMIT: u64 = 16 * 1024 * 1024;
const DEFAULT_MEDIA_CACHE_IMAGE_MAX_DIMENSION: u32 = 12_288;
const DEFAULT_MEDIA_CACHE_IMAGE_MAX_PIXELS: u64 = 40_000_000;
const DEFAULT_MEDIA_CACHE_IMAGE_MAX_DECODE_BYTES: u64 = 256 * 1024 * 1024;
const IMAGE_DECODE_ESTIMATED_BYTES_PER_PIXEL: u64 = 4;
pub(super) const GRID_THUMBNAIL_MAX_DIMENSION: u32 = 256;
pub(super) const GRID_THUMBNAIL_PROFILE: &str = "grid";
pub(super) const MOBILE_VIEWER_THUMBNAIL_MAX_DIMENSION: u32 = 1536;
pub(super) const MOBILE_VIEWER_THUMBNAIL_PROFILE: &str = "mobile_viewer";
pub(super) const MEDIA_FORMAT_SNIFF_BYTES: usize = 64 * 1024;
pub(super) const SLOW_MEDIA_CACHE_GENERATION_LOG_THRESHOLD_MS: u128 = 20000;
const GRID_THUMBNAIL_JPEG_QUALITY: u8 = 82;
const MOBILE_VIEWER_THUMBNAIL_JPEG_QUALITY: u8 = 84;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MediaImageFormat {
    Standard(ImageFormat),
    Heif,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ThumbnailProfileSpec {
    pub(crate) name: &'static str,
    pub(crate) max_dimension: u32,
    jpeg_quality: u8,
}

pub(crate) fn grid_thumbnail_profile() -> ThumbnailProfileSpec {
    ThumbnailProfileSpec {
        name: GRID_THUMBNAIL_PROFILE,
        max_dimension: GRID_THUMBNAIL_MAX_DIMENSION,
        jpeg_quality: GRID_THUMBNAIL_JPEG_QUALITY,
    }
}

pub(crate) fn mobile_viewer_thumbnail_profile() -> ThumbnailProfileSpec {
    ThumbnailProfileSpec {
        name: MOBILE_VIEWER_THUMBNAIL_PROFILE,
        max_dimension: MOBILE_VIEWER_THUMBNAIL_MAX_DIMENSION,
        jpeg_quality: MOBILE_VIEWER_THUMBNAIL_JPEG_QUALITY,
    }
}

pub(crate) fn thumbnail_profile_from_query(profile: Option<&str>) -> Option<ThumbnailProfileSpec> {
    let profile = profile.unwrap_or(GRID_THUMBNAIL_PROFILE).trim();
    match profile {
        GRID_THUMBNAIL_PROFILE => Some(grid_thumbnail_profile()),
        MOBILE_VIEWER_THUMBNAIL_PROFILE => Some(mobile_viewer_thumbnail_profile()),
        _ => None,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MediaCacheStatus {
    Ready,
    Incomplete,
    Unsupported,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaGpsCoordinates {
    pub latitude: f64,
    pub longitude: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedThumbnailInfo {
    pub profile: String,
    pub format: String,
    pub width: u32,
    pub height: u32,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CachedPhotoMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera_manufacturer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lens_manufacturer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lens_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iso_speed: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exposure_time_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub f_number: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focal_length_mm: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flash: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub white_balance: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedMediaMetadata {
    pub schema_version: u32,
    pub content_fingerprint: String,
    pub source_manifest_hash: String,
    pub status: MediaCacheStatus,
    pub media_type: Option<String>,
    pub mime_type: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub orientation: Option<u16>,
    pub taken_at_unix: Option<u64>,
    /// Whether `taken_at_unix` came with an explicit UTC offset. A missing
    /// offset is a floating local wall-clock value, not a trustworthy UTC time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub taken_at_timezone_known: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date_encoded_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_millis: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_rate_millihertz: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_bitrate_bps: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec_fourcc: Option<String>,
    pub gps: Option<MediaGpsCoordinates>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub photo: Option<CachedPhotoMetadata>,
    pub thumbnail: Option<CachedThumbnailInfo>,
    pub source_size_bytes: usize,
    pub generated_at_unix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_unix: Option<u64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MediaCacheLookup {
    pub content_fingerprint: String,
    pub metadata: Option<CachedMediaMetadata>,
    /// Path-scoped GPS supplied by an XMP sidecar projection. Unlike cached
    /// metadata, this must not be shared by byte-identical objects at other
    /// paths.
    pub gps_override: Option<MediaGpsCoordinates>,
}

struct RenderedThumbnail {
    payload: Vec<u8>,
    width: u32,
    height: u32,
}

struct DerivedMediaCacheArtifact {
    metadata: CachedMediaMetadata,
    thumbnail_payload: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub(super) struct MediaCacheBuildConfig {
    pub(super) total_permits: u32,
    bytes_per_permit: u64,
    image_limits: MediaCacheImageLimits,
}

#[derive(Debug, Clone)]
pub(super) struct MediaCacheImageLimits {
    pub(super) max_dimension: u32,
    pub(super) max_pixels: u64,
    pub(super) max_decode_bytes: u64,
}

impl Default for MediaCacheImageLimits {
    fn default() -> Self {
        Self {
            max_dimension: positive_env_u32(
                MEDIA_CACHE_IMAGE_MAX_DIMENSION_ENV,
                DEFAULT_MEDIA_CACHE_IMAGE_MAX_DIMENSION,
            ),
            max_pixels: positive_env_u64(
                MEDIA_CACHE_IMAGE_MAX_PIXELS_ENV,
                DEFAULT_MEDIA_CACHE_IMAGE_MAX_PIXELS,
            ),
            max_decode_bytes: positive_env_u64(
                MEDIA_CACHE_IMAGE_MAX_DECODE_BYTES_ENV,
                DEFAULT_MEDIA_CACHE_IMAGE_MAX_DECODE_BYTES,
            ),
        }
    }
}

impl Default for MediaCacheBuildConfig {
    fn default() -> Self {
        Self {
            total_permits: positive_env_u32(
                MEDIA_CACHE_BUILD_TOTAL_PERMITS_ENV,
                DEFAULT_MEDIA_CACHE_BUILD_TOTAL_PERMITS,
            ),
            bytes_per_permit: positive_env_u64(
                MEDIA_CACHE_BUILD_BYTES_PER_PERMIT_ENV,
                DEFAULT_MEDIA_CACHE_BUILD_BYTES_PER_PERMIT,
            ),
            image_limits: MediaCacheImageLimits::default(),
        }
    }
}

impl MediaCacheBuildConfig {
    pub(super) fn permits_for_source_size(&self, source_size_bytes: usize) -> u32 {
        let source_size_bytes = u64::try_from(source_size_bytes).unwrap_or(u64::MAX);
        self.permits_for_estimated_bytes(source_size_bytes)
    }

    fn permits_for_estimated_bytes(&self, estimated_bytes: u64) -> u32 {
        let permits = if estimated_bytes == 0 {
            1
        } else {
            estimated_bytes.div_ceil(self.bytes_per_permit)
        };
        permits.clamp(1, u64::from(self.total_permits)) as u32
    }

    fn image_limits(&self) -> &MediaCacheImageLimits {
        &self.image_limits
    }

    #[cfg(test)]
    pub(super) fn with_image_limits_for_test(
        mut self,
        image_limits: MediaCacheImageLimits,
    ) -> Self {
        self.image_limits = image_limits;
        self
    }
}

impl From<CachedMediaMetadata> for DerivedMediaCacheArtifact {
    fn from(metadata: CachedMediaMetadata) -> Self {
        Self {
            metadata,
            thumbnail_payload: None,
        }
    }
}

#[derive(Clone)]
pub(crate) struct MediaCacheWorker {
    pub(super) storage_pool: StoragePool,
    pub(super) media_thumbnails_dir: PathBuf,
    pub(super) media_cache_build_permits: Arc<Semaphore>,
    pub(super) media_cache_build_config: MediaCacheBuildConfig,
    pub(super) metadata_store: Arc<dyn MetadataStore>,
    pub(super) media_tools: MediaToolPaths,
}

impl MediaCacheWorker {
    pub(super) fn new(
        storage_pool: StoragePool,
        media_thumbnails_dir: PathBuf,
        media_cache_build_permits: Arc<Semaphore>,
        media_cache_build_config: MediaCacheBuildConfig,
        metadata_store: Arc<dyn MetadataStore>,
        media_tools: MediaToolPaths,
    ) -> Self {
        Self {
            storage_pool,
            media_thumbnails_dir,
            media_cache_build_permits,
            media_cache_build_config,
            metadata_store,
            media_tools,
        }
    }

    pub(crate) async fn ensure_media_metadata(
        &self,
        manifest_hash: &str,
    ) -> Result<Option<CachedMediaMetadata>> {
        self.ensure_media_artifact(manifest_hash, false).await
    }

    pub(crate) async fn ensure_media_cache(
        &self,
        manifest_hash: &str,
    ) -> Result<Option<CachedMediaMetadata>> {
        self.ensure_media_artifact(manifest_hash, true).await
    }

    pub(crate) async fn ensure_image_thumbnail_profile_payload(
        &self,
        manifest_hash: &str,
        profile: ThumbnailProfileSpec,
    ) -> Result<Option<Vec<u8>>> {
        let Some(manifest) = self.load_manifest_by_hash(manifest_hash).await? else {
            return Ok(None);
        };

        if !manifest_chunks_are_locally_complete(&manifest, &self.storage_pool).await? {
            return Ok(None);
        }

        let content_fingerprint = content_fingerprint_from_manifest(&manifest);
        let thumbnail_path = self
            .media_thumbnails_dir
            .join(&content_fingerprint)
            .join(format!("{}.jpg", profile.name));
        if fs::try_exists(&thumbnail_path).await? {
            return Ok(Some(fs::read(&thumbnail_path).await?));
        }

        let sniff_bytes = read_object_prefix_from_manifest(
            &manifest,
            &self.storage_pool,
            MEDIA_FORMAT_SNIFF_BYTES,
        )
        .await?;
        let format = match image_format_from_prefix(&sniff_bytes) {
            Ok(format) => format,
            Err(_) => return Ok(None),
        };
        if image_format_mime_type(format).is_none() {
            return Ok(None);
        }

        let build_permits =
            self.media_cache_build_config
                .permits_for_estimated_bytes(estimated_image_build_bytes(
                    manifest.total_size_bytes,
                    image_dimensions(&sniff_bytes, format).ok(),
                    Some(format),
                    true,
                    self.media_cache_build_config.image_limits(),
                ));
        let build_permit = self
            .media_cache_build_permits
            .clone()
            .acquire_many_owned(build_permits)
            .await
            .expect("media cache build semaphore should remain open");

        let manifest_owned = manifest.clone();
        let storage_pool = self.storage_pool.clone();
        let image_limits = self.media_cache_build_config.image_limits().clone();
        let rendered = match task::spawn_blocking(move || {
            let _build_permit = build_permit;
            render_image_thumbnail_payload(&manifest_owned, &storage_pool, profile, &image_limits)
        })
        .await
        {
            Ok(Ok(Some(payload))) => payload,
            Ok(Ok(None)) => return Ok(None),
            Ok(Err(err)) => return Err(err),
            Err(err) => bail!("image thumbnail profile task failed: {err}"),
        };

        write_atomic(&thumbnail_path, &rendered).await?;
        Ok(Some(rendered))
    }

    async fn ensure_media_artifact(
        &self,
        manifest_hash: &str,
        include_thumbnail: bool,
    ) -> Result<Option<CachedMediaMetadata>> {
        let ensure_started_at = Instant::now();
        let now_unix = unix_ts();
        if manifest_hash == TOMBSTONE_MANIFEST_HASH {
            return Ok(None);
        }

        let Some(manifest) = self.load_manifest_by_hash(manifest_hash).await? else {
            return Ok(None);
        };
        let content_fingerprint = content_fingerprint_from_manifest(&manifest);
        let existing = current_media_cache_metadata(
            self.load_cached_media_metadata(&content_fingerprint)
                .await?,
        );
        if let Some(existing) = existing.as_ref() {
            let retry_due = media_cache_retry_due(existing, now_unix);
            let cache_satisfies_request = if retry_due {
                false
            } else {
                !include_thumbnail
                    || existing.thumbnail.is_some()
                    || existing.status != MediaCacheStatus::Ready
            };
            if !cache_satisfies_request {
                // Fall through and rebuild the artifact with a thumbnail.
            } else {
                let total_ms = ensure_started_at.elapsed().as_millis();
                if total_ms >= SLOW_MEDIA_CACHE_GENERATION_LOG_THRESHOLD_MS {
                    warn!(
                        manifest_hash,
                        content_fingerprint = %content_fingerprint,
                        total_ms,
                        cache_hit = true,
                        include_thumbnail,
                        status = ?existing.status,
                        has_thumbnail = existing.thumbnail.is_some(),
                        "slow media cache ensure"
                    );
                }
                return Ok(Some(existing.clone()));
            }
        }

        info!(
            manifest_hash,
            content_fingerprint = %content_fingerprint,
            source_size_bytes = manifest.total_size_bytes,
            include_thumbnail,
            metadata_present = existing.is_some(),
            "media cache build requested"
        );

        let build_started_at = Instant::now();
        let derived = self
            .build_media_cache_artifact(
                &manifest,
                manifest_hash,
                &content_fingerprint,
                include_thumbnail,
            )
            .await;
        let build_record_ms = build_started_at.elapsed().as_millis();
        if include_thumbnail
            && let Some(existing) = existing.as_ref()
            && existing.status == MediaCacheStatus::Ready
            && existing.thumbnail.is_none()
            && (derived.metadata.status != MediaCacheStatus::Ready
                || derived.metadata.thumbnail.is_none())
        {
            let merged = merge_cached_media_metadata_without_thumbnail(existing, &derived.metadata);
            let derived = DerivedMediaCacheArtifact {
                metadata: merged,
                thumbnail_payload: None,
            };
            let persist_started_at = Instant::now();
            self.persist_media_cache_record(&derived).await?;
            let persist_ms = persist_started_at.elapsed().as_millis();
            let total_ms = ensure_started_at.elapsed().as_millis();
            let metadata = &derived.metadata;
            if matches!(
                metadata.status,
                MediaCacheStatus::Failed | MediaCacheStatus::Unsupported
            ) {
                warn!(
                    manifest_hash,
                    content_fingerprint = %content_fingerprint,
                    include_thumbnail,
                    status = ?metadata.status,
                    error = metadata.error.as_deref().unwrap_or(""),
                    "media thumbnail build failed after metadata-only cache"
                );
            }
            info!(
                manifest_hash,
                content_fingerprint = %content_fingerprint,
                total_ms,
                build_record_ms,
                persist_ms,
                include_thumbnail,
                status = ?metadata.status,
                has_thumbnail = metadata.thumbnail.is_some(),
                error = metadata.error.as_deref().unwrap_or(""),
                "media cache build finished"
            );
            if total_ms >= SLOW_MEDIA_CACHE_GENERATION_LOG_THRESHOLD_MS {
                warn!(
                    manifest_hash,
                    content_fingerprint = %content_fingerprint,
                    total_ms,
                    build_record_ms,
                    persist_ms,
                    include_thumbnail,
                    status = ?metadata.status,
                    has_thumbnail = metadata.thumbnail.is_some(),
                    error = metadata.error.as_deref().unwrap_or(""),
                    "slow media cache build"
                );
            }
            return Ok(Some(metadata.clone()));
        }

        let persist_started_at = Instant::now();
        self.persist_media_cache_record(&derived).await?;
        let persist_ms = persist_started_at.elapsed().as_millis();
        let total_ms = ensure_started_at.elapsed().as_millis();
        let metadata = &derived.metadata;
        info!(
            manifest_hash,
            content_fingerprint = %content_fingerprint,
            total_ms,
            build_record_ms,
            persist_ms,
            include_thumbnail,
            status = ?metadata.status,
            has_thumbnail = metadata.thumbnail.is_some(),
            error = metadata.error.as_deref().unwrap_or(""),
            "media cache build finished"
        );
        if total_ms >= SLOW_MEDIA_CACHE_GENERATION_LOG_THRESHOLD_MS {
            warn!(
                manifest_hash,
                content_fingerprint = %content_fingerprint,
                total_ms,
                build_record_ms,
                persist_ms,
                include_thumbnail,
                status = ?metadata.status,
                has_thumbnail = metadata.thumbnail.is_some(),
                error = metadata.error.as_deref().unwrap_or(""),
                "slow media cache build"
            );
        }
        Ok(Some(metadata.clone()))
    }

    pub(crate) async fn import_media_cache_artifact(
        &self,
        mut metadata: CachedMediaMetadata,
        thumbnail_payload: Option<Vec<u8>>,
    ) -> Result<CachedMediaMetadata> {
        if metadata.status != MediaCacheStatus::Ready || thumbnail_payload.is_none() {
            metadata.thumbnail = None;
        }
        metadata.retry_after_unix = None;

        let derived = DerivedMediaCacheArtifact {
            metadata: metadata.clone(),
            thumbnail_payload,
        };
        self.persist_media_cache_record(&derived).await?;
        Ok(metadata)
    }

    async fn load_manifest_by_hash(&self, manifest_hash: &str) -> Result<Option<ObjectManifest>> {
        let manifest_path = self
            .storage_pool
            .content_path(StorageContentKind::Manifest, manifest_hash)?;
        if !fs::try_exists(&manifest_path).await? {
            return Ok(None);
        }

        let payload = fs::read(&manifest_path).await?;
        let manifest = serde_json::from_slice::<ObjectManifest>(&payload)
            .with_context(|| format!("invalid manifest {}", manifest_path.display()))?;
        Ok(Some(manifest))
    }

    async fn load_cached_media_metadata(
        &self,
        content_fingerprint: &str,
    ) -> Result<Option<CachedMediaMetadata>> {
        self.metadata_store
            .load_cached_media_metadata(content_fingerprint)
            .await
    }

    async fn build_media_cache_artifact(
        &self,
        manifest: &ObjectManifest,
        manifest_hash: &str,
        content_fingerprint: &str,
        include_thumbnail: bool,
    ) -> DerivedMediaCacheArtifact {
        let generated_at_unix = unix_ts();

        match manifest_chunks_are_locally_complete(manifest, &self.storage_pool).await {
            Ok(true) => {}
            Ok(false) => {
                return incomplete_media_cache_artifact(
                    manifest_hash,
                    content_fingerprint,
                    manifest.total_size_bytes,
                    generated_at_unix,
                    "media source is incomplete locally; one or more chunks are missing or have the wrong size",
                );
            }
            Err(err) => {
                return failed_media_cache_artifact(
                    manifest_hash,
                    content_fingerprint,
                    manifest.total_size_bytes,
                    generated_at_unix,
                    err.to_string(),
                );
            }
        }

        let sniff_bytes = match read_object_prefix_from_manifest(
            manifest,
            &self.storage_pool,
            MEDIA_FORMAT_SNIFF_BYTES,
        )
        .await
        {
            Ok(bytes) => bytes,
            Err(err) => {
                return failed_media_cache_artifact(
                    manifest_hash,
                    content_fingerprint,
                    manifest.total_size_bytes,
                    generated_at_unix,
                    err.to_string(),
                );
            }
        };
        let format = image_format_from_prefix(&sniff_bytes).ok();

        if let Some(format) = format {
            if image_format_mime_type(format).is_none() {
                return unsupported_media_cache_artifact(
                    manifest_hash,
                    content_fingerprint,
                    manifest.total_size_bytes,
                    generated_at_unix,
                    "media format is not supported for thumbnail extraction",
                );
            }

            let build_permits = self.media_cache_build_config.permits_for_estimated_bytes(
                estimated_image_build_bytes(
                    manifest.total_size_bytes,
                    image_dimensions(&sniff_bytes, format).ok(),
                    Some(format),
                    include_thumbnail,
                    self.media_cache_build_config.image_limits(),
                ),
            );
            let build_permit = self
                .media_cache_build_permits
                .clone()
                .acquire_many_owned(build_permits)
                .await
                .expect("media cache build semaphore should remain open");

            let manifest_owned = manifest.clone();
            let storage_pool = self.storage_pool.clone();
            let manifest_hash_owned = manifest_hash.to_string();
            let content_fingerprint_owned = content_fingerprint.to_string();
            let image_limits = self.media_cache_build_config.image_limits().clone();

            return match task::spawn_blocking(move || {
                let _build_permit = build_permit;
                build_image_media_cache_blocking(
                    &manifest_hash_owned,
                    &content_fingerprint_owned,
                    &manifest_owned,
                    &storage_pool,
                    include_thumbnail,
                    &image_limits,
                )
            })
            .await
            {
                Ok(Ok(derived)) => derived,
                Ok(Err(err)) => failed_media_cache_artifact(
                    manifest_hash,
                    content_fingerprint,
                    manifest.total_size_bytes,
                    generated_at_unix,
                    err.to_string(),
                ),
                Err(err) => failed_media_cache_artifact(
                    manifest_hash,
                    content_fingerprint,
                    manifest.total_size_bytes,
                    generated_at_unix,
                    format!("image media cache task failed: {err}"),
                ),
            };
        }

        let _build_permit = self
            .media_cache_build_permits
            .clone()
            .acquire_many_owned(
                self.media_cache_build_config
                    .permits_for_source_size(manifest.total_size_bytes),
            )
            .await
            .expect("media cache build semaphore should remain open");

        match derive_video_media_cache(
            manifest_hash,
            content_fingerprint,
            manifest.total_size_bytes,
            manifest,
            &self.storage_pool,
            &self.media_tools,
            include_thumbnail,
        )
        .await
        {
            Ok(derived) => derived,
            Err(err) => failed_media_cache_artifact(
                manifest_hash,
                content_fingerprint,
                manifest.total_size_bytes,
                generated_at_unix,
                err.to_string(),
            ),
        }
    }

    async fn persist_media_cache_record(&self, derived: &DerivedMediaCacheArtifact) -> Result<()> {
        persist_media_cache_record_with_payload(
            &self.media_thumbnails_dir,
            self.metadata_store.as_ref(),
            &derived.metadata,
            derived.thumbnail_payload.as_deref(),
        )
        .await
    }
}

fn base_media_metadata(
    manifest_hash: &str,
    content_fingerprint: &str,
    source_size_bytes: usize,
    generated_at_unix: u64,
) -> CachedMediaMetadata {
    CachedMediaMetadata {
        schema_version: MEDIA_CACHE_SCHEMA_VERSION,
        content_fingerprint: content_fingerprint.to_string(),
        source_manifest_hash: manifest_hash.to_string(),
        status: MediaCacheStatus::Failed,
        media_type: None,
        mime_type: None,
        width: None,
        height: None,
        orientation: None,
        taken_at_unix: None,
        taken_at_timezone_known: None,
        date_encoded_unix: None,
        duration_millis: None,
        frame_rate_millihertz: None,
        total_bitrate_bps: None,
        codec_name: None,
        codec_fourcc: None,
        gps: None,
        photo: None,
        thumbnail: None,
        source_size_bytes,
        generated_at_unix,
        retry_after_unix: None,
        error: None,
    }
}

pub fn media_cache_incomplete_retry_after_unix(now_unix: u64) -> u64 {
    now_unix.saturating_add(media_cache_incomplete_retry_secs())
}

fn media_cache_incomplete_retry_secs() -> u64 {
    std::env::var(MEDIA_CACHE_INCOMPLETE_RETRY_SECS_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(MEDIA_CACHE_INCOMPLETE_RETRY_SECS)
}

fn incomplete_media_cache_artifact(
    manifest_hash: &str,
    content_fingerprint: &str,
    source_size_bytes: usize,
    generated_at_unix: u64,
    error: impl Into<String>,
) -> DerivedMediaCacheArtifact {
    DerivedMediaCacheArtifact {
        metadata: CachedMediaMetadata {
            status: MediaCacheStatus::Incomplete,
            retry_after_unix: Some(media_cache_incomplete_retry_after_unix(generated_at_unix)),
            error: Some(error.into()),
            ..base_media_metadata(
                manifest_hash,
                content_fingerprint,
                source_size_bytes,
                generated_at_unix,
            )
        },
        thumbnail_payload: None,
    }
}

pub fn promote_cached_media_metadata_to_incomplete(
    metadata: &CachedMediaMetadata,
    generated_at_unix: u64,
    error: impl Into<String>,
) -> CachedMediaMetadata {
    preserve_cached_media_metadata_status(
        metadata,
        MediaCacheStatus::Incomplete,
        generated_at_unix,
        error,
        Some(media_cache_incomplete_retry_after_unix(generated_at_unix)),
    )
}

fn preserve_cached_media_metadata_status(
    metadata: &CachedMediaMetadata,
    status: MediaCacheStatus,
    generated_at_unix: u64,
    error: impl Into<String>,
    retry_after_unix: Option<u64>,
) -> CachedMediaMetadata {
    let mut next = metadata.clone();
    next.status = status;
    next.thumbnail = None;
    next.generated_at_unix = generated_at_unix;
    next.retry_after_unix = retry_after_unix;
    next.error = Some(error.into());
    next
}

fn merge_cached_media_metadata_without_thumbnail(
    existing: &CachedMediaMetadata,
    derived: &CachedMediaMetadata,
) -> CachedMediaMetadata {
    let (status, retry_after_unix, fallback_error) = match derived.status {
        MediaCacheStatus::Incomplete => (
            MediaCacheStatus::Incomplete,
            Some(media_cache_incomplete_retry_after_unix(
                derived.generated_at_unix,
            )),
            "media source is incomplete locally; one or more chunks are missing or have the wrong size",
        ),
        MediaCacheStatus::Unsupported => (
            MediaCacheStatus::Unsupported,
            None,
            "media format is not supported for thumbnail extraction",
        ),
        MediaCacheStatus::Failed => (
            MediaCacheStatus::Failed,
            None,
            "thumbnail generation failed",
        ),
        MediaCacheStatus::Ready => (
            MediaCacheStatus::Failed,
            None,
            "media thumbnail build finished without producing a thumbnail",
        ),
    };

    preserve_cached_media_metadata_status(
        existing,
        status,
        derived.generated_at_unix,
        derived
            .error
            .clone()
            .unwrap_or_else(|| fallback_error.to_string()),
        retry_after_unix,
    )
}

pub fn media_cache_retry_due(metadata: &CachedMediaMetadata, now_unix: u64) -> bool {
    metadata.status == MediaCacheStatus::Incomplete
        && metadata
            .retry_after_unix
            .map(|retry_after_unix| retry_after_unix <= now_unix)
            .unwrap_or(true)
}

fn positive_env_u32(key: &str, default: u32) -> u32 {
    std::env::var(key)
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn positive_env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn failed_media_cache_artifact(
    manifest_hash: &str,
    content_fingerprint: &str,
    source_size_bytes: usize,
    generated_at_unix: u64,
    error: impl Into<String>,
) -> DerivedMediaCacheArtifact {
    DerivedMediaCacheArtifact {
        metadata: CachedMediaMetadata {
            status: MediaCacheStatus::Failed,
            error: Some(error.into()),
            ..base_media_metadata(
                manifest_hash,
                content_fingerprint,
                source_size_bytes,
                generated_at_unix,
            )
        },
        thumbnail_payload: None,
    }
}

fn unsupported_media_cache_artifact(
    manifest_hash: &str,
    content_fingerprint: &str,
    source_size_bytes: usize,
    generated_at_unix: u64,
    error: impl Into<String>,
) -> DerivedMediaCacheArtifact {
    DerivedMediaCacheArtifact {
        metadata: CachedMediaMetadata {
            status: MediaCacheStatus::Unsupported,
            error: Some(error.into()),
            ..base_media_metadata(
                manifest_hash,
                content_fingerprint,
                source_size_bytes,
                generated_at_unix,
            )
        },
        thumbnail_payload: None,
    }
}

pub(super) async fn persist_media_cache_record_with_payload(
    media_thumbnails_dir: &Path,
    metadata_store: &dyn MetadataStore,
    metadata: &CachedMediaMetadata,
    thumbnail_payload: Option<&[u8]>,
) -> Result<()> {
    if let (Some(thumbnail), Some(payload)) = (&metadata.thumbnail, thumbnail_payload) {
        let thumbnail_path = media_thumbnails_dir
            .join(&metadata.content_fingerprint)
            .join(format!("{}.jpg", thumbnail.profile));
        write_atomic(&thumbnail_path, payload).await?;
    }
    metadata_store.persist_media_cache_record(metadata).await
}

async fn manifest_chunks_are_locally_complete(
    manifest: &ObjectManifest,
    storage_pool: &StoragePool,
) -> Result<bool> {
    for chunk in &manifest.chunks {
        let chunk_path = storage_pool.content_path(StorageContentKind::Chunk, &chunk.hash)?;
        let metadata = match fs::metadata(&chunk_path).await {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(err) => return Err(err.into()),
        };
        if metadata.len() != chunk.size_bytes as u64 {
            return Ok(false);
        }
    }

    Ok(true)
}

async fn read_object_prefix_from_manifest(
    manifest: &ObjectManifest,
    storage_pool: &StoragePool,
    max_bytes: usize,
) -> Result<Vec<u8>> {
    let target_len = std::cmp::min(manifest.total_size_bytes, max_bytes);
    let mut prefix = Vec::with_capacity(target_len);

    for chunk in &manifest.chunks {
        if prefix.len() >= target_len {
            break;
        }

        let chunk_path = storage_pool.content_path(StorageContentKind::Chunk, &chunk.hash)?;
        let payload = fs::read(&chunk_path)
            .await
            .with_context(|| format!("failed reading chunk {}", chunk.hash))?;
        if payload.len() != chunk.size_bytes {
            bail!(
                "size mismatch for chunk hash={} expected={} actual={}",
                chunk.hash,
                chunk.size_bytes,
                payload.len()
            );
        }
        let actual_hash = hash_hex(&payload);
        if actual_hash != chunk.hash {
            bail!(
                "hash mismatch for chunk expected={} actual={}",
                chunk.hash,
                actual_hash
            );
        }

        let remaining = target_len.saturating_sub(prefix.len());
        prefix.extend_from_slice(&payload[..remaining.min(payload.len())]);
    }

    Ok(prefix)
}

async fn collect_local_chunk_paths(
    manifest: &ObjectManifest,
    storage_pool: &StoragePool,
) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::with_capacity(manifest.chunks.len());
    for chunk in &manifest.chunks {
        let chunk_path = storage_pool.content_path(StorageContentKind::Chunk, &chunk.hash)?;
        let metadata = fs::metadata(&chunk_path)
            .await
            .with_context(|| format!("missing chunk {}", chunk.hash))?;
        if metadata.len() != chunk.size_bytes as u64 {
            bail!(
                "size mismatch for chunk hash={} expected={} actual={}",
                chunk.hash,
                chunk.size_bytes,
                metadata.len()
            );
        }
        paths.push(chunk_path);
    }
    Ok(paths)
}

/// Holds the chunk hashes and precomputed byte offsets for a virtual video file.
/// Paths are resolved from the in-memory storage-location index at read time.
struct ChunkVideoIndex {
    storage_pool: StoragePool,
    hashes: Vec<String>,
    /// Byte offset of each chunk's first byte in the virtual file.
    offsets: Vec<u64>,
    total_size: u64,
}

impl ChunkVideoIndex {
    fn new(storage_pool: StoragePool, manifest: &ObjectManifest) -> Self {
        let mut offsets = Vec::with_capacity(manifest.chunks.len());
        let mut offset = 0u64;
        for chunk in &manifest.chunks {
            offsets.push(offset);
            offset += chunk.size_bytes as u64;
        }
        let hashes = manifest.chunks.iter().map(|c| c.hash.clone()).collect();
        Self {
            storage_pool,
            hashes,
            offsets,
            total_size: offset,
        }
    }

    async fn read_range(&self, start: u64, end: u64) -> Result<Vec<u8>> {
        let mut result = Vec::new();
        for i in 0..self.hashes.len() {
            let chunk_start = self.offsets[i];
            let chunk_size = if i + 1 < self.offsets.len() {
                self.offsets[i + 1] - chunk_start
            } else {
                self.total_size - chunk_start
            };
            let chunk_end = chunk_start + chunk_size.saturating_sub(1);
            if chunk_end < start {
                continue;
            }
            if chunk_start > end {
                break;
            }
            let read_from = start.saturating_sub(chunk_start) as usize;
            let read_to = (end.min(chunk_end) - chunk_start) as usize;
            let path = self
                .storage_pool
                .content_path(StorageContentKind::Chunk, &self.hashes[i])?;
            let data = fs::read(&path).await?;
            result.extend_from_slice(&data[read_from..=read_to]);
        }
        Ok(result)
    }
}

async fn serve_video_range(
    State(index): State<Arc<ChunkVideoIndex>>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    use axum::http::{StatusCode, header};
    use axum::response::IntoResponse;

    let total = index.total_size;
    let (start, end, partial) = match headers.get(header::RANGE) {
        Some(v) => match parse_http_byte_range(v.to_str().unwrap_or(""), total) {
            Some((s, e)) => (s, e, true),
            None => {
                return (
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    [(
                        header::CONTENT_RANGE,
                        format!("bytes */{total}")
                            .parse::<axum::http::HeaderValue>()
                            .unwrap(),
                    )],
                    axum::body::Body::empty(),
                )
                    .into_response();
            }
        },
        None => (0, total.saturating_sub(1), false),
    };

    match index.read_range(start, end).await {
        Ok(data) => {
            let len = data.len();
            let status = if partial {
                StatusCode::PARTIAL_CONTENT
            } else {
                StatusCode::OK
            };
            let mut builder = axum::response::Response::builder()
                .status(status)
                .header(header::CONTENT_TYPE, "application/octet-stream")
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::CONTENT_LENGTH, len);
            if partial {
                builder = builder.header(
                    header::CONTENT_RANGE,
                    format!("bytes {start}-{end}/{total}"),
                );
            }
            builder
                .body(axum::body::Body::from(data))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        Err(err) => {
            warn!(?err, "chunk video server: range read failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn parse_http_byte_range(range: &str, total: u64) -> Option<(u64, u64)> {
    let s = range.strip_prefix("bytes=")?;
    if let Some(suffix) = s.strip_prefix('-') {
        let n: u64 = suffix.trim().parse().ok()?;
        if n == 0 || total == 0 {
            return None;
        }
        Some((total.saturating_sub(n), total - 1))
    } else {
        let (a, b) = s.split_once('-')?;
        let start: u64 = a.trim().parse().ok()?;
        let end = if b.trim().is_empty() {
            total.saturating_sub(1)
        } else {
            b.trim().parse::<u64>().ok()?.min(total.saturating_sub(1))
        };
        (start <= end && start < total).then_some((start, end))
    }
}

/// Starts a local HTTP server on 127.0.0.1 that presents the chunked video
/// as a single seekable file via Range requests.
///
/// A random UUID token is embedded as a literal route segment so that other
/// local processes cannot reach the endpoint without knowing the token.
/// The server is torn down (task aborted) as soon as ffprobe/ffmpeg finish.
async fn start_chunk_video_server(
    index: Arc<ChunkVideoIndex>,
    _temp_dir: &Path,
) -> Result<(tokio::task::JoinHandle<()>, String)> {
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("failed to bind local TCP listener for chunk video server")?;
    let port = listener.local_addr()?.port();
    let token = Uuid::new_v4().to_string();
    let route = format!("/{token}/video");

    let app = Router::new()
        .route(&route, axum::routing::get(serve_video_range))
        .with_state(index);

    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let url = format!("http://127.0.0.1:{port}/{token}/video");
    Ok((handle, url))
}

async fn derive_video_media_cache(
    manifest_hash: &str,
    content_fingerprint: &str,
    source_size_bytes: usize,
    manifest: &ObjectManifest,
    storage_pool: &StoragePool,
    media_tools: &MediaToolPaths,
    include_thumbnail: bool,
) -> Result<DerivedMediaCacheArtifact> {
    let generated_at_unix = unix_ts();
    // Validate all chunks are present and have the expected size.
    collect_local_chunk_paths(manifest, storage_pool).await?;
    let chunk_index = Arc::new(ChunkVideoIndex::new(storage_pool.clone(), manifest));

    let temp_dir = std::env::temp_dir().join(format!("ironmesh-media-cache-{}", Uuid::new_v4()));
    fs::create_dir_all(&temp_dir)
        .await
        .with_context(|| format!("failed to create temp dir {}", temp_dir.display()))?;

    let (server_task, video_url) = match start_chunk_video_server(chunk_index, &temp_dir).await {
        Ok(v) => v,
        Err(err) => {
            let _ = fs::remove_dir_all(&temp_dir).await;
            return Err(err);
        }
    };

    let derived = async {
        let mut ffprobe = Command::new(&media_tools.ffprobe);
        ffprobe
            .arg("-v")
            .arg("error")
            .arg("-select_streams")
            .arg("v:0")
            .arg("-show_entries")
            .arg(
                "stream=width,height,codec_name,codec_tag_string,avg_frame_rate,bit_rate:stream_tags=creation_time,com.apple.quicktime.creationdate,location,location-eng,com.apple.quicktime.location.ISO6709:format=format_name,duration,bit_rate:format_tags=creation_time,com.apple.quicktime.creationdate,location,location-eng,com.apple.quicktime.location.ISO6709",
            )
            .arg("-of")
            .arg("json")
            .arg(&video_url);
        let probe_output = run_media_tool(&mut ffprobe, FFPROBE_TIMEOUT_SECS, "ffprobe").await?;
        let probe: FfprobeOutput = serde_json::from_slice(&probe_output.stdout)
            .context("failed to parse ffprobe JSON output")?;
        let Some(stream) = probe.streams.first() else {
            return Ok::<DerivedMediaCacheArtifact, anyhow::Error>(
                unsupported_media_cache_artifact(
                    manifest_hash,
                    content_fingerprint,
                    source_size_bytes,
                    generated_at_unix,
                    "unsupported media format",
                ),
            );
        };

        let mime_type = video_mime_type_for_format_name(
            probe
                .format
                .as_ref()
                .and_then(|format| format.format_name.as_deref()),
        );
        let duration_secs = probe
            .format
            .as_ref()
            .and_then(|format| format.duration.as_deref())
            .and_then(parse_positive_f64);
        // ffprobe turns QuickTime's integer `mvhd` creation time into an RFC
        // 3339 `creation_time`, including a synthetic trailing `Z`. Cameras
        // often store local wall-clock time there, so it is not UTC-safe on
        // its own. Prefer Apple's offset-bearing creation-date tag when it is
        // available; otherwise retain creation_time as a floating local time.
        let (embedded_capture_time_unix, taken_at_timezone_known) = ffprobe_capture_time(
            probe.format.as_ref().map(|format| &format.tags),
            &stream.tags,
        );
        let total_bitrate_bps = probe
            .format
            .as_ref()
            .and_then(|format| format.bit_rate.as_deref())
            .or(stream.bit_rate.as_deref())
            .and_then(parse_positive_u64);
        let gps = probe
            .format
            .as_ref()
            .and_then(|format| ffprobe_gps(&format.tags))
            .or_else(|| ffprobe_gps(&stream.tags));
        let metadata = CachedMediaMetadata {
            status: MediaCacheStatus::Ready,
            media_type: Some("video".to_string()),
            mime_type,
            width: stream.width,
            height: stream.height,
            taken_at_unix: embedded_capture_time_unix,
            taken_at_timezone_known,
            date_encoded_unix: embedded_capture_time_unix,
            duration_millis: duration_secs.and_then(seconds_to_millis),
            frame_rate_millihertz: stream
                .avg_frame_rate
                .as_deref()
                .and_then(parse_frame_rate_millihertz),
            total_bitrate_bps,
            codec_name: trimmed_metadata_string(stream.codec_name.as_deref()),
            codec_fourcc: trimmed_metadata_string(stream.codec_tag_string.as_deref()),
            gps,
            ..base_media_metadata(
                manifest_hash,
                content_fingerprint,
                source_size_bytes,
                generated_at_unix,
            )
        };

        if !include_thumbnail {
            return Ok(DerivedMediaCacheArtifact {
                metadata,
                thumbnail_payload: None,
            });
        }

        let mut ffmpeg = Command::new(&media_tools.ffmpeg);
        ffmpeg.arg("-v").arg("error").arg("-nostdin");
        if let Some(seek_time) = preferred_video_seek_time(duration_secs) {
            ffmpeg.arg("-ss").arg(seek_time);
        }
        ffmpeg
            .arg("-i")
            .arg(&video_url)
            .arg("-an")
            .arg("-sn")
            .arg("-dn")
            .arg("-vf")
            .arg(format!(
                "thumbnail=100,scale={0}:{0}:force_original_aspect_ratio=decrease",
                GRID_THUMBNAIL_MAX_DIMENSION
            ))
            .arg("-frames:v")
            .arg("1")
            .arg("-f")
            .arg("image2pipe")
            .arg("-vcodec")
            .arg("mjpeg")
            .arg("pipe:1");
        let ffmpeg_output = run_media_tool(&mut ffmpeg, FFMPEG_TIMEOUT_SECS, "ffmpeg").await?;
        let rendered = image::load_from_memory(&ffmpeg_output.stdout)
            .context("failed to decode ffmpeg thumbnail output")?;

        Ok(DerivedMediaCacheArtifact {
            metadata: CachedMediaMetadata {
                thumbnail: Some(CachedThumbnailInfo {
                    profile: GRID_THUMBNAIL_PROFILE.to_string(),
                    format: "jpeg".to_string(),
                    width: rendered.width(),
                    height: rendered.height(),
                    size_bytes: ffmpeg_output.stdout.len() as u64,
                }),
                ..metadata
            },
            thumbnail_payload: Some(ffmpeg_output.stdout),
        })
    }
    .await;

    server_task.abort();
    let _ = fs::remove_dir_all(&temp_dir).await;
    derived
}

async fn run_media_tool(
    command: &mut Command,
    timeout_secs: u64,
    tool_name: &str,
) -> Result<std::process::Output> {
    command.kill_on_drop(true);
    match timeout(Duration::from_secs(timeout_secs), command.output()).await {
        Ok(Ok(output)) if output.status.success() => Ok(output),
        Ok(Ok(output)) => {
            bail!(
                "{tool_name} exited with status {}: {}",
                output.status,
                trimmed_command_output(&output.stderr)
            )
        }
        Ok(Err(err)) => Err(err).with_context(|| format!("failed to spawn {tool_name}")),
        Err(_) => bail!("{tool_name} timed out after {timeout_secs}s"),
    }
}

fn trimmed_command_output(stderr: &[u8]) -> String {
    let value = String::from_utf8_lossy(stderr).trim().to_string();
    if value.len() > 400 {
        format!("{}...", &value[..400])
    } else if value.is_empty() {
        "<no stderr output>".to_string()
    } else {
        value
    }
}

fn parse_positive_f64(value: &str) -> Option<f64> {
    value
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && *value > 0.0)
}

fn parse_positive_u64(value: &str) -> Option<u64> {
    value.trim().parse::<u64>().ok().filter(|value| *value > 0)
}

fn seconds_to_millis(seconds: f64) -> Option<u64> {
    let millis = seconds * 1_000.0;
    (millis.is_finite() && millis > 0.0 && millis <= u64::MAX as f64).then(|| millis.round() as u64)
}

pub(super) fn parse_frame_rate_millihertz(value: &str) -> Option<u32> {
    let value = value.trim();
    let frames_per_second = match value.split_once('/') {
        Some((numerator, denominator)) => {
            let numerator = numerator.trim().parse::<f64>().ok()?;
            let denominator = denominator.trim().parse::<f64>().ok()?;
            (denominator != 0.0).then_some(numerator / denominator)?
        }
        None => value.parse::<f64>().ok()?,
    };
    let millihertz = frames_per_second * 1_000.0;
    (millihertz.is_finite() && millihertz > 0.0 && millihertz <= u32::MAX as f64)
        .then(|| millihertz.round() as u32)
}

pub(super) fn parse_ffprobe_timestamp(value: &str) -> Option<u64> {
    let value = value.trim();
    let timestamp = OffsetDateTime::parse(value, &Rfc3339)
        .or_else(|_| parse_ffprobe_basic_offset_timestamp(value))
        .ok()?
        .unix_timestamp();
    u64::try_from(timestamp).ok()
}

/// ffprobe normally emits RFC 3339 offsets with a colon, but Apple QuickTime
/// creation-date tags also commonly use ISO 8601's compact `+HHMM` form.
fn parse_ffprobe_basic_offset_timestamp(value: &str) -> Result<OffsetDateTime, time::error::Parse> {
    let offset_start = value.len().saturating_sub(5);
    let Some(offset) = value.get(offset_start..) else {
        return OffsetDateTime::parse(value, &Rfc3339);
    };
    if offset.len() != 5
        || !matches!(offset.as_bytes().first(), Some(b'+' | b'-'))
        || !offset.as_bytes()[1..].iter().all(u8::is_ascii_digit)
    {
        return OffsetDateTime::parse(value, &Rfc3339);
    }
    let normalized = format!(
        "{}{}:{}",
        &value[..offset_start],
        &offset[..3],
        &offset[3..]
    );
    OffsetDateTime::parse(&normalized, &Rfc3339)
}

fn ffprobe_capture_time(
    format_tags: Option<&FfprobeTags>,
    stream_tags: &FfprobeTags,
) -> (Option<u64>, Option<bool>) {
    let offset_bearing = [
        format_tags.and_then(|tags| tags.quicktime_creation_date.as_deref()),
        stream_tags.quicktime_creation_date.as_deref(),
    ]
    .into_iter()
    .flatten()
    .find_map(parse_ffprobe_timestamp);
    if let Some(timestamp) = offset_bearing {
        return (Some(timestamp), Some(true));
    }

    let floating_local = [
        format_tags.and_then(|tags| tags.creation_time.as_deref()),
        stream_tags.creation_time.as_deref(),
    ]
    .into_iter()
    .flatten()
    .find_map(parse_ffprobe_timestamp);
    (floating_local, floating_local.map(|_| false))
}

fn ffprobe_gps(tags: &super::media_tools::FfprobeTags) -> Option<MediaGpsCoordinates> {
    [
        tags.quicktime_location_iso6709.as_deref(),
        tags.location.as_deref(),
        tags.location_eng.as_deref(),
    ]
    .into_iter()
    .flatten()
    .find_map(parse_iso6709_location)
}

/// Parses the latitude/longitude portion of ISO 6709 values commonly exposed
/// by ffprobe for QuickTime location atoms, e.g. `+47.3769+008.5417/`.
pub(super) fn parse_iso6709_location(value: &str) -> Option<MediaGpsCoordinates> {
    let value = value.trim().strip_suffix('/').unwrap_or(value.trim());
    let first = value.as_bytes().first().copied()?;
    if !matches!(first, b'+' | b'-') {
        return None;
    }
    let longitude_start = value
        .char_indices()
        .skip(1)
        .find_map(|(index, character)| matches!(character, '+' | '-').then_some(index))?;
    let latitude = value[..longitude_start].parse::<f64>().ok()?;
    let longitude_end = value
        .char_indices()
        .skip(longitude_start + 1)
        .find_map(|(index, character)| matches!(character, '+' | '-').then_some(index))
        .unwrap_or(value.len());
    let longitude = value[longitude_start..longitude_end].parse::<f64>().ok()?;
    (latitude.is_finite()
        && longitude.is_finite()
        && (-90.0..=90.0).contains(&latitude)
        && (-180.0..=180.0).contains(&longitude))
    .then_some(MediaGpsCoordinates {
        latitude,
        longitude,
    })
}

fn trimmed_metadata_string(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    Some(value.chars().take(256).collect())
}

pub(super) fn preferred_video_seek_time(duration_secs: Option<f64>) -> Option<String> {
    let seek = match duration_secs {
        Some(duration_secs) => (duration_secs * VIDEO_THUMBNAIL_SEEK_FRACTION)
            .clamp(VIDEO_THUMBNAIL_SEEK_MIN_SECS, VIDEO_THUMBNAIL_SEEK_MAX_SECS)
            .min(duration_secs),
        None => VIDEO_THUMBNAIL_UNKNOWN_DURATION_SEEK_SECS,
    };
    Some(format!("{seek:.3}"))
}

fn video_mime_type_for_format_name(format_name: Option<&str>) -> Option<String> {
    let format_name = format_name?;
    if format_name.contains("webm") {
        return Some("video/webm".to_string());
    }
    if format_name.contains("matroska") {
        return Some("video/x-matroska".to_string());
    }
    if format_name.contains("mov") || format_name.contains("mp4") || format_name.contains("3gp") {
        return Some("video/mp4".to_string());
    }
    if format_name.contains("avi") {
        return Some("video/x-msvideo".to_string());
    }
    if format_name.contains("flv") {
        return Some("video/x-flv".to_string());
    }
    if format_name.contains("mpegts") || format_name == "ts" {
        return Some("video/mp2t".to_string());
    }
    if format_name.contains("ogg") {
        return Some("video/ogg".to_string());
    }
    if format_name.contains("mpeg") {
        return Some("video/mpeg".to_string());
    }
    None
}

fn build_image_media_cache_blocking(
    manifest_hash: &str,
    content_fingerprint: &str,
    manifest: &ObjectManifest,
    storage_pool: &StoragePool,
    include_thumbnail: bool,
    image_limits: &MediaCacheImageLimits,
) -> Result<DerivedMediaCacheArtifact> {
    let mut reader = ManifestReader::new(manifest, storage_pool)
        .context("failed to create manifest-backed image reader")?;
    let derived = derive_image_media_cache_from_reader(
        manifest_hash,
        content_fingerprint,
        manifest.total_size_bytes,
        &mut reader,
        include_thumbnail,
        image_limits,
    )?;
    reader
        .verify_all()
        .context("failed to verify manifest-backed image input")?;
    Ok(derived)
}

#[cfg(test)]
fn derive_image_media_cache(
    manifest_hash: &str,
    content_fingerprint: &str,
    source_size_bytes: usize,
    payload: &[u8],
    include_thumbnail: bool,
    image_limits: &MediaCacheImageLimits,
) -> Result<DerivedMediaCacheArtifact> {
    let mut reader = Cursor::new(payload);
    derive_image_media_cache_from_reader(
        manifest_hash,
        content_fingerprint,
        source_size_bytes,
        &mut reader,
        include_thumbnail,
        image_limits,
    )
}

fn derive_image_media_cache_from_reader<R: BufRead + Seek>(
    manifest_hash: &str,
    content_fingerprint: &str,
    source_size_bytes: usize,
    reader: &mut R,
    include_thumbnail: bool,
    image_limits: &MediaCacheImageLimits,
) -> Result<DerivedMediaCacheArtifact> {
    let generated_at_unix = unix_ts();
    let format = match guess_image_format(reader) {
        Ok(format) => format,
        Err(_) => {
            return Ok(unsupported_media_cache_artifact(
                manifest_hash,
                content_fingerprint,
                source_size_bytes,
                generated_at_unix,
                "unsupported media format",
            ));
        }
    };

    let mime_type = match image_format_mime_type(format) {
        Some(value) => value.to_string(),
        None => {
            return Ok(unsupported_media_cache_artifact(
                manifest_hash,
                content_fingerprint,
                source_size_bytes,
                generated_at_unix,
                "media format is not supported for thumbnail extraction",
            ));
        }
    };

    let (width, height) = image_dimensions_from_reader(reader, format)?;
    let (orientation, gps, taken_at_unix, taken_at_timezone_known, photo) =
        extract_exif_fields_from_reader(reader);
    let mut metadata = CachedMediaMetadata {
        status: MediaCacheStatus::Ready,
        media_type: Some("image".to_string()),
        mime_type: Some(mime_type),
        width: Some(width),
        height: Some(height),
        orientation,
        taken_at_unix,
        taken_at_timezone_known,
        gps,
        photo,
        ..base_media_metadata(
            manifest_hash,
            content_fingerprint,
            source_size_bytes,
            generated_at_unix,
        )
    };

    if !include_thumbnail {
        return Ok(DerivedMediaCacheArtifact {
            metadata,
            thumbnail_payload: None,
        });
    }

    if let Some(error) = validate_image_decode_limits(
        format,
        width,
        height,
        grid_thumbnail_profile(),
        image_limits,
    ) {
        metadata.status = MediaCacheStatus::Unsupported;
        metadata.error = Some(error);
        return Ok(DerivedMediaCacheArtifact {
            metadata,
            thumbnail_payload: None,
        });
    }

    let rendered_thumbnail = match render_thumbnail_from_reader(
        reader,
        format,
        orientation,
        grid_thumbnail_profile(),
        image_limits,
    ) {
        Ok(rendered) => rendered,
        Err(error) if is_thumbnail_limit_error(&error) => {
            metadata.status = MediaCacheStatus::Unsupported;
            metadata.error = Some(format!("{error:#}"));
            return Ok(DerivedMediaCacheArtifact {
                metadata,
                thumbnail_payload: None,
            });
        }
        Err(error) => return Err(error),
    };

    metadata.thumbnail = Some(CachedThumbnailInfo {
        profile: grid_thumbnail_profile().name.to_string(),
        format: "jpeg".to_string(),
        width: rendered_thumbnail.width,
        height: rendered_thumbnail.height,
        size_bytes: rendered_thumbnail.payload.len() as u64,
    });

    Ok(DerivedMediaCacheArtifact {
        metadata,
        thumbnail_payload: Some(rendered_thumbnail.payload),
    })
}

fn image_format_mime_type(format: MediaImageFormat) -> Option<&'static str> {
    match format {
        MediaImageFormat::Standard(ImageFormat::Bmp) => Some("image/bmp"),
        MediaImageFormat::Standard(ImageFormat::Gif) => Some("image/gif"),
        MediaImageFormat::Standard(ImageFormat::Jpeg) => Some("image/jpeg"),
        MediaImageFormat::Standard(ImageFormat::Png) => Some("image/png"),
        MediaImageFormat::Standard(ImageFormat::WebP) => Some("image/webp"),
        MediaImageFormat::Heif => Some("image/heic"),
        _ => None,
    }
}

pub(crate) fn current_media_cache_metadata(
    metadata: Option<CachedMediaMetadata>,
) -> Option<CachedMediaMetadata> {
    metadata.filter(|metadata| metadata.schema_version == MEDIA_CACHE_SCHEMA_VERSION)
}

fn image_dimensions(payload: &[u8], format: MediaImageFormat) -> Result<(u32, u32)> {
    match format {
        MediaImageFormat::Standard(format) => {
            ImageReader::with_format(Cursor::new(payload), format)
                .into_dimensions()
                .context("failed to inspect image dimensions")
        }
        MediaImageFormat::Heif => {
            let dimensions =
                imagesize::blob_size(payload).context("failed to inspect HEIF image dimensions")?;
            Ok((
                u32::try_from(dimensions.width).context("HEIF image width exceeds u32")?,
                u32::try_from(dimensions.height).context("HEIF image height exceeds u32")?,
            ))
        }
    }
}

fn image_format_from_prefix(prefix: &[u8]) -> Result<MediaImageFormat> {
    if let Ok(format) = image::guess_format(prefix) {
        return Ok(MediaImageFormat::Standard(format));
    }
    match imagesize::image_type(prefix) {
        Ok(SizeImageType::Heif(HeifCompression::Hevc)) => Ok(MediaImageFormat::Heif),
        _ => bail!("unsupported media format"),
    }
}

fn guess_image_format<R: Read + Seek>(reader: &mut R) -> Result<MediaImageFormat> {
    reader
        .seek(SeekFrom::Start(0))
        .context("failed to rewind image input for format detection")?;
    let mut prefix = Vec::with_capacity(MEDIA_FORMAT_SNIFF_BYTES);
    reader
        .take(MEDIA_FORMAT_SNIFF_BYTES as u64)
        .read_to_end(&mut prefix)
        .context("failed to read image format prefix")?;
    reader
        .seek(SeekFrom::Start(0))
        .context("failed to rewind image input after format detection")?;
    image_format_from_prefix(&prefix)
}

fn image_dimensions_from_reader<R: BufRead + Seek>(
    reader: &mut R,
    format: MediaImageFormat,
) -> Result<(u32, u32)> {
    reader
        .seek(SeekFrom::Start(0))
        .context("failed to rewind image input for dimension inspection")?;
    match format {
        MediaImageFormat::Standard(format) => ImageReader::with_format(&mut *reader, format)
            .into_dimensions()
            .context("failed to inspect image dimensions"),
        MediaImageFormat::Heif => {
            let dimensions = imagesize::reader_size(&mut *reader)
                .context("failed to inspect HEIF image dimensions")?;
            Ok((
                u32::try_from(dimensions.width).context("HEIF image width exceeds u32")?,
                u32::try_from(dimensions.height).context("HEIF image height exceeds u32")?,
            ))
        }
    }
}

fn estimated_image_build_bytes(
    source_size_bytes: usize,
    dimensions: Option<(u32, u32)>,
    format: Option<MediaImageFormat>,
    include_thumbnail: bool,
    image_limits: &MediaCacheImageLimits,
) -> u64 {
    let source_size_bytes = u64::try_from(source_size_bytes).unwrap_or(u64::MAX);
    if !include_thumbnail {
        return source_size_bytes;
    }

    // Exact HEIF admission depends on embedded-thumbnail and grid metadata that
    // is not available in the format-sniff prefix. A successful optimized
    // decode is bounded by max_decode_bytes; the decoder performs the exact
    // SPS/tile/source/output estimate before allocating pixel buffers.
    if format == Some(MediaImageFormat::Heif) {
        return image_limits.max_decode_bytes;
    }

    let decode_bytes = dimensions
        .map(|(width, height)| {
            u64::from(width)
                .saturating_mul(u64::from(height))
                .saturating_mul(IMAGE_DECODE_ESTIMATED_BYTES_PER_PIXEL)
        })
        .unwrap_or_else(|| {
            image_limits.max_decode_bytes.max(
                image_limits
                    .max_pixels
                    .saturating_mul(IMAGE_DECODE_ESTIMATED_BYTES_PER_PIXEL),
            )
        });
    decode_bytes.max(source_size_bytes)
}

fn validate_image_decode_limits(
    format: MediaImageFormat,
    width: u32,
    height: u32,
    profile: ThumbnailProfileSpec,
    image_limits: &MediaCacheImageLimits,
) -> Option<String> {
    // HEIF admission is based on the selected embedded thumbnail or the
    // largest concurrently live grid tile, not the primary canvas. The
    // thumbnailer validates coded SPS dimensions and its complete peak-memory
    // estimate immediately before decode.
    if format == MediaImageFormat::Heif {
        return None;
    }

    let (decode_width, decode_height) = if format == MediaImageFormat::Standard(ImageFormat::Jpeg) {
        jpeg_scaled_dimensions(width, height, profile.max_dimension)
    } else {
        (width, height)
    };

    if decode_width > image_limits.max_dimension || decode_height > image_limits.max_dimension {
        return Some(format!(
            "image thumbnail generation rejected: decoded dimensions {}x{} exceed limit {}px",
            decode_width, decode_height, image_limits.max_dimension
        ));
    }

    let pixel_count = u64::from(decode_width).saturating_mul(u64::from(decode_height));
    if pixel_count > image_limits.max_pixels {
        return Some(format!(
            "image thumbnail generation rejected: decoded pixel count {} exceeds limit {}",
            pixel_count, image_limits.max_pixels
        ));
    }

    let estimated_decode_bytes = pixel_count.saturating_mul(IMAGE_DECODE_ESTIMATED_BYTES_PER_PIXEL);
    if estimated_decode_bytes > image_limits.max_decode_bytes {
        return Some(format!(
            "image thumbnail generation rejected: estimated decode footprint {} bytes exceeds limit {} bytes",
            estimated_decode_bytes, image_limits.max_decode_bytes
        ));
    }

    None
}

fn jpeg_scaled_dimensions(width: u32, height: u32, target: u32) -> (u32, u32) {
    for scale in [1, 2, 4, 8] {
        let scaled_width = width.saturating_mul(scale).div_ceil(8);
        let scaled_height = height.saturating_mul(scale).div_ceil(8);
        if scaled_width >= target || scaled_height >= target || scale == 8 {
            return (scaled_width, scaled_height);
        }
    }

    unreachable!("the full-size JPEG scale always matches")
}

fn render_thumbnail_from_reader<R: BufRead + Seek>(
    reader: &mut R,
    format: MediaImageFormat,
    orientation: Option<u16>,
    profile: ThumbnailProfileSpec,
    image_limits: &MediaCacheImageLimits,
) -> Result<RenderedThumbnail> {
    let mime_type = image_format_mime_type(format)
        .context("image format is not supported for thumbnail extraction")?
        .parse::<mime::Mime>()
        .context("failed to construct image MIME type")?;
    reader
        .seek(SeekFrom::Start(0))
        .context("failed to rewind image input for thumbnail generation")?;
    let mut thumbnails = create_thumbnails_with_options(
        &mut *reader,
        mime_type,
        [ThumbnailSize::Custom((
            profile.max_dimension,
            profile.max_dimension,
        ))],
        ThumbnailOptions::new()
            .with_max_decoded_bytes(image_limits.max_decode_bytes)
            .with_max_progressive_jpeg_pixels(image_limits.max_pixels),
    )
    .context("failed to generate image thumbnail")?;
    let image = thumbnails
        .pop()
        .context("thumbnail generator returned no image")?
        .into_image();
    // berry-thumbnailer normalizes HEIF container transforms and falls back
    // to embedded EXIF orientation, so applying it again would rotate twice.
    let orientation = match format {
        MediaImageFormat::Heif => None,
        MediaImageFormat::Standard(_) => orientation,
    };
    encode_thumbnail(image, orientation, profile)
}

fn is_thumbnail_limit_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<BerryThumbnailError>()
        .is_some_and(|error| matches!(error, BerryThumbnailError::LimitExceeded(_)))
}

fn encode_thumbnail(
    mut thumbnail: DynamicImage,
    orientation: Option<u16>,
    profile: ThumbnailProfileSpec,
) -> Result<RenderedThumbnail> {
    apply_exif_orientation(&mut thumbnail, orientation);
    let mut encoded = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut encoded, profile.jpeg_quality);
    encoder
        .encode_image(&thumbnail)
        .context("failed to encode thumbnail")?;
    Ok(RenderedThumbnail {
        payload: encoded,
        width: thumbnail.width(),
        height: thumbnail.height(),
    })
}

fn render_image_thumbnail_payload(
    manifest: &ObjectManifest,
    storage_pool: &StoragePool,
    profile: ThumbnailProfileSpec,
    image_limits: &MediaCacheImageLimits,
) -> Result<Option<Vec<u8>>> {
    let mut reader = ManifestReader::new(manifest, storage_pool)
        .context("failed to create manifest-backed image reader")?;
    let format = guess_image_format(&mut reader)?;
    let (width, height) = image_dimensions_from_reader(&mut reader, format)?;
    if let Some(error) = validate_image_decode_limits(format, width, height, profile, image_limits)
    {
        tracing::debug!(
            profile = profile.name,
            width,
            height,
            error = %error,
            "skipping thumbnail profile generation because image exceeds decode limits"
        );
        reader
            .verify_all()
            .context("failed to verify manifest-backed image input")?;
        return Ok(None);
    }

    let (orientation, _, _, _, _) = extract_exif_fields_from_reader(&mut reader);
    let rendered =
        match render_thumbnail_from_reader(&mut reader, format, orientation, profile, image_limits)
        {
            Ok(rendered) => rendered,
            Err(error) if is_thumbnail_limit_error(&error) => {
                tracing::debug!(
                    profile = profile.name,
                    width,
                    height,
                    error = %error,
                    "skipping thumbnail profile generation because decoder limits were exceeded"
                );
                reader
                    .verify_all()
                    .context("failed to verify manifest-backed image input")?;
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
    reader
        .verify_all()
        .context("failed to verify manifest-backed image input")?;
    Ok(Some(rendered.payload))
}

fn apply_exif_orientation(image: &mut DynamicImage, orientation: Option<u16>) {
    let Some(orientation) = orientation
        .and_then(|value| u8::try_from(value).ok())
        .and_then(Orientation::from_exif)
    else {
        return;
    };
    image.apply_orientation(orientation);
}

type ExifExtraction = (
    Option<u16>,
    Option<MediaGpsCoordinates>,
    Option<u64>,
    Option<bool>,
    Option<CachedPhotoMetadata>,
);

fn extract_exif_fields_from_reader<R: BufRead + Seek>(reader: &mut R) -> ExifExtraction {
    if reader.seek(SeekFrom::Start(0)).is_err() {
        return (None, None, None, None, None);
    }
    let exif = match ExifReader::new().read_from_container(reader) {
        Ok(value) => value,
        Err(_) => return (None, None, None, None, None),
    };

    let orientation = exif
        .get_field(Tag::Orientation, In::PRIMARY)
        .and_then(|field| field.value.get_uint(0))
        .and_then(|value| u16::try_from(value).ok());

    let latitude = exif
        .get_field(Tag::GPSLatitude, In::PRIMARY)
        .and_then(|field| exif_gps_coordinate(&field.value))
        .map(
            |value| match exif_ascii_ref(exif.get_field(Tag::GPSLatitudeRef, In::PRIMARY)) {
                Some('S') | Some('s') => -value,
                _ => value,
            },
        );
    let longitude = exif
        .get_field(Tag::GPSLongitude, In::PRIMARY)
        .and_then(|field| exif_gps_coordinate(&field.value))
        .map(
            |value| match exif_ascii_ref(exif.get_field(Tag::GPSLongitudeRef, In::PRIMARY)) {
                Some('W') | Some('w') => -value,
                _ => value,
            },
        );

    let gps = match (latitude, longitude) {
        (Some(latitude), Some(longitude)) => Some(MediaGpsCoordinates {
            latitude,
            longitude,
        }),
        _ => None,
    };

    let (taken_at_unix, taken_at_timezone_known) = exif_capture_time(&exif)
        .map(|(unix, timezone_known)| (Some(unix), Some(timezone_known)))
        .unwrap_or((None, None));
    let photo = exif_photo_metadata(&exif);

    (
        orientation,
        gps,
        taken_at_unix,
        taken_at_timezone_known,
        photo,
    )
}

fn exif_photo_metadata(exif: &exif::Exif) -> Option<CachedPhotoMetadata> {
    let metadata = CachedPhotoMetadata {
        camera_manufacturer: exif_metadata_string(exif, Tag::Make),
        camera_model: exif_metadata_string(exif, Tag::Model),
        lens_manufacturer: exif_metadata_string(exif, Tag::LensMake),
        lens_model: exif_metadata_string(exif, Tag::LensModel),
        iso_speed: exif
            .get_field(Tag::ISOSpeed, In::PRIMARY)
            .or_else(|| exif.get_field(Tag::PhotographicSensitivity, In::PRIMARY))
            .and_then(|field| field.value.get_uint(0)),
        exposure_time_seconds: exif_first_f64(exif, Tag::ExposureTime),
        f_number: exif_first_f64(exif, Tag::FNumber),
        focal_length_mm: exif_first_f64(exif, Tag::FocalLength),
        flash: exif
            .get_field(Tag::Flash, In::PRIMARY)
            .and_then(|field| field.value.get_uint(0))
            .and_then(|value| u16::try_from(value).ok()),
        white_balance: exif
            .get_field(Tag::WhiteBalance, In::PRIMARY)
            .and_then(|field| field.value.get_uint(0))
            .and_then(|value| u16::try_from(value).ok()),
    };

    (!metadata.is_empty()).then_some(metadata)
}

impl CachedPhotoMetadata {
    fn is_empty(&self) -> bool {
        self.camera_manufacturer.is_none()
            && self.camera_model.is_none()
            && self.lens_manufacturer.is_none()
            && self.lens_model.is_none()
            && self.iso_speed.is_none()
            && self.exposure_time_seconds.is_none()
            && self.f_number.is_none()
            && self.focal_length_mm.is_none()
            && self.flash.is_none()
            && self.white_balance.is_none()
    }
}

fn exif_metadata_string(exif: &exif::Exif, tag: Tag) -> Option<String> {
    trimmed_metadata_string(exif_ascii_string(exif.get_field(tag, In::PRIMARY)))
}

fn exif_first_f64(exif: &exif::Exif, tag: Tag) -> Option<f64> {
    let value = &exif.get_field(tag, In::PRIMARY)?.value;
    let value = match value {
        Value::Rational(values) => values.first()?.to_f64(),
        Value::SRational(values) => values.first()?.to_f64(),
        _ => return None,
    };
    (value.is_finite() && value > 0.0).then_some(value)
}

fn exif_capture_time(exif: &exif::Exif) -> Option<(u64, bool)> {
    [
        (
            exif_ascii_string(exif.get_field(Tag::DateTimeOriginal, In::PRIMARY)),
            exif_ascii_string(exif.get_field(Tag::OffsetTimeOriginal, In::PRIMARY))
                .or_else(|| exif_ascii_string(exif.get_field(Tag::OffsetTime, In::PRIMARY))),
        ),
        (
            exif_ascii_string(exif.get_field(Tag::DateTimeDigitized, In::PRIMARY)),
            exif_ascii_string(exif.get_field(Tag::OffsetTimeDigitized, In::PRIMARY))
                .or_else(|| exif_ascii_string(exif.get_field(Tag::OffsetTime, In::PRIMARY))),
        ),
        (
            exif_ascii_string(exif.get_field(Tag::DateTime, In::PRIMARY)),
            exif_ascii_string(exif.get_field(Tag::OffsetTime, In::PRIMARY)),
        ),
    ]
    .into_iter()
    .find_map(|(datetime, offset)| {
        parse_exif_taken_at(datetime, offset)
            .map(|unix| (unix, offset.and_then(parse_exif_offset).is_some()))
    })
}

pub(super) fn parse_exif_taken_at(datetime: Option<&str>, offset: Option<&str>) -> Option<u64> {
    let date_time = parse_exif_datetime(datetime?)?;
    let timestamp = match offset.and_then(parse_exif_offset) {
        Some(offset) => date_time.assume_offset(offset).unix_timestamp(),
        None => date_time.assume_utc().unix_timestamp(),
    };
    u64::try_from(timestamp).ok()
}

fn parse_exif_datetime(value: &str) -> Option<PrimitiveDateTime> {
    let value = value.get(..19)?;
    if !matches!(value.as_bytes().get(4), Some(b':'))
        || !matches!(value.as_bytes().get(7), Some(b':'))
        || !matches!(value.as_bytes().get(10), Some(b' '))
        || !matches!(value.as_bytes().get(13), Some(b':'))
        || !matches!(value.as_bytes().get(16), Some(b':'))
    {
        return None;
    }

    let year = value.get(0..4)?.parse::<i32>().ok()?;
    let month = value.get(5..7)?.parse::<u8>().ok()?;
    let day = value.get(8..10)?.parse::<u8>().ok()?;
    let hour = value.get(11..13)?.parse::<u8>().ok()?;
    let minute = value.get(14..16)?.parse::<u8>().ok()?;
    let second = value.get(17..19)?.parse::<u8>().ok()?;

    let month = Month::try_from(month).ok()?;
    let date = Date::from_calendar_date(year, month, day).ok()?;
    let time = Time::from_hms(hour, minute, second).ok()?;
    Some(PrimitiveDateTime::new(date, time))
}

fn parse_exif_offset(value: &str) -> Option<UtcOffset> {
    let value = value.get(..6)?;
    if !matches!(value.as_bytes().first(), Some(b'+') | Some(b'-'))
        || !matches!(value.as_bytes().get(3), Some(b':'))
    {
        return None;
    }

    let sign = if value.starts_with('-') { -1 } else { 1 };
    let hours = value.get(1..3)?.parse::<i8>().ok()?;
    let minutes = value.get(4..6)?.parse::<i8>().ok()?;
    UtcOffset::from_hms(sign * hours, sign * minutes, 0).ok()
}

fn exif_ascii_string(field: Option<&exif::Field>) -> Option<&str> {
    match &field?.value {
        Value::Ascii(values) => {
            let value = values.first()?;
            let value = std::str::from_utf8(value).ok()?;
            let value = value.trim_matches(char::from(0)).trim();
            if value.is_empty() { None } else { Some(value) }
        }
        _ => None,
    }
}

fn exif_ascii_ref(field: Option<&exif::Field>) -> Option<char> {
    exif_ascii_string(field)?.chars().next()
}

pub(super) fn exif_gps_coordinate(value: &Value) -> Option<f64> {
    match value {
        Value::Rational(values) if values.len() >= 3 => {
            let degrees = values[0].to_f64();
            let minutes = values[1].to_f64();
            let seconds = values[2].to_f64();
            if !degrees.is_finite() || !minutes.is_finite() || !seconds.is_finite() {
                return None;
            }
            let total = degrees + (minutes / 60.0) + (seconds / 3600.0);
            total.is_finite().then_some(total)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_png_bytes() -> Vec<u8> {
        let image = image::DynamicImage::new_rgba8(4, 3);
        let mut cursor = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut cursor, image::ImageFormat::Png)
            .unwrap();
        cursor.into_inner()
    }

    fn sample_wide_jpeg_bytes() -> Vec<u8> {
        let image = image::RgbImage::from_pixel(2048, 512, image::Rgb([40, 90, 180]));
        let mut payload = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut payload, 85)
            .encode_image(&image)
            .unwrap();
        payload
    }

    fn sample_heic_bytes() -> Vec<u8> {
        // Lossless x265 grid fixture generated from the permissively licensed
        // heif-oxide test bitstream.
        let hex: String = include_str!("../../testdata/test-grid.heic.hex")
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect();
        (0..hex.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).unwrap())
            .collect()
    }

    fn sample_exif_oriented_heic_bytes() -> Vec<u8> {
        let hex: String = include_str!("../../testdata/test-exif-orientation.heic.hex")
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect();
        (0..hex.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn media_cache_build_config_scales_permits_by_source_size() {
        let config = MediaCacheBuildConfig {
            total_permits: 20,
            bytes_per_permit: 16 * 1024 * 1024,
            image_limits: MediaCacheImageLimits::default(),
        };

        assert_eq!(config.permits_for_source_size(0), 1);
        assert_eq!(config.permits_for_source_size(1), 1);
        assert_eq!(config.permits_for_source_size(16 * 1024 * 1024), 1);
        assert_eq!(config.permits_for_source_size((16 * 1024 * 1024) + 1), 2);
        assert_eq!(
            config.permits_for_source_size(usize::MAX),
            config.total_permits
        );
    }

    #[test]
    fn derive_image_media_cache_only_applies_decode_limits_for_thumbnail_builds() {
        let payload = sample_png_bytes();
        let image_limits = MediaCacheImageLimits {
            max_dimension: 10,
            max_pixels: 11,
            max_decode_bytes: 1024 * 1024,
        };

        let metadata_only = derive_image_media_cache(
            "manifest",
            "fingerprint",
            payload.len(),
            &payload,
            false,
            &image_limits,
        )
        .unwrap();
        assert_eq!(metadata_only.metadata.status, MediaCacheStatus::Ready);
        assert_eq!(metadata_only.metadata.width, Some(4));
        assert_eq!(metadata_only.metadata.height, Some(3));
        assert!(metadata_only.metadata.thumbnail.is_none());

        let with_thumbnail = derive_image_media_cache(
            "manifest",
            "fingerprint",
            payload.len(),
            &payload,
            true,
            &image_limits,
        )
        .unwrap();
        assert_eq!(
            with_thumbnail.metadata.status,
            MediaCacheStatus::Unsupported
        );
        assert_eq!(with_thumbnail.metadata.width, Some(4));
        assert_eq!(with_thumbnail.metadata.height, Some(3));
        assert!(with_thumbnail.metadata.thumbnail.is_none());
        assert!(with_thumbnail.thumbnail_payload.is_none());
        assert!(
            with_thumbnail
                .metadata
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("pixel count")
        );
    }

    #[test]
    fn derive_image_media_cache_supports_oriented_heic_grids() {
        let payload = sample_heic_bytes();

        let derived = derive_image_media_cache(
            "manifest",
            "fingerprint",
            payload.len(),
            &payload,
            true,
            &MediaCacheImageLimits::default(),
        )
        .unwrap();

        assert_eq!(derived.metadata.status, MediaCacheStatus::Ready);
        assert_eq!(derived.metadata.mime_type.as_deref(), Some("image/heic"));
        assert_eq!(
            (derived.metadata.width, derived.metadata.height),
            (Some(50), Some(100))
        );
        assert_eq!(derived.metadata.orientation, None);
        let thumbnail = derived.metadata.thumbnail.unwrap();
        assert_eq!((thumbnail.width, thumbnail.height), (128, 256));
        assert!(derived.thumbnail_payload.is_some());
    }

    #[test]
    fn derive_image_media_cache_supports_exif_oriented_heic_grids() {
        let payload = sample_exif_oriented_heic_bytes();

        let derived = derive_image_media_cache(
            "manifest",
            "fingerprint",
            payload.len(),
            &payload,
            true,
            &MediaCacheImageLimits::default(),
        )
        .unwrap();

        assert_eq!(derived.metadata.status, MediaCacheStatus::Ready);
        assert_eq!(derived.metadata.mime_type.as_deref(), Some("image/heic"));
        assert_eq!(
            (derived.metadata.width, derived.metadata.height),
            (Some(100), Some(50))
        );
        assert_eq!(derived.metadata.orientation, Some(6));
        let thumbnail = derived.metadata.thumbnail.unwrap();
        assert_eq!((thumbnail.width, thumbnail.height), (128, 256));
        assert!(derived.thumbnail_payload.is_some());
    }

    #[test]
    fn heic_codestream_limit_is_reported_as_unsupported_media() {
        let payload = sample_heic_bytes();
        let image_limits = MediaCacheImageLimits {
            max_dimension: 12_288,
            max_pixels: 40_000_000,
            max_decode_bytes: 150_000,
        };

        let derived = derive_image_media_cache(
            "manifest",
            "fingerprint",
            payload.len(),
            &payload,
            true,
            &image_limits,
        )
        .unwrap();

        assert_eq!(derived.metadata.status, MediaCacheStatus::Unsupported);
        assert!(derived.metadata.thumbnail.is_none());
        assert!(derived.thumbnail_payload.is_none());
        assert!(
            derived
                .metadata
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("target-sized HEIF decode")
        );
    }

    #[test]
    fn heif_primary_canvas_does_not_preempt_optimized_decoder_admission() {
        assert_eq!(
            validate_image_decode_limits(
                MediaImageFormat::Heif,
                60_000,
                20_000,
                grid_thumbnail_profile(),
                &MediaCacheImageLimits::default(),
            ),
            None
        );
    }

    #[test]
    fn baseline_jpeg_uses_scaled_decode_instead_of_full_source_limits() {
        let payload = sample_wide_jpeg_bytes();
        let image_limits = MediaCacheImageLimits {
            max_dimension: 300,
            max_pixels: 20_000,
            max_decode_bytes: 64 * 1024,
        };

        let derived = derive_image_media_cache(
            "manifest",
            "fingerprint",
            payload.len(),
            &payload,
            true,
            &image_limits,
        )
        .unwrap();

        assert_eq!(derived.metadata.status, MediaCacheStatus::Ready);
        let thumbnail = derived.metadata.thumbnail.unwrap();
        assert_eq!((thumbnail.width, thumbnail.height), (256, 64));
        assert!(derived.thumbnail_payload.is_some());
    }

    #[test]
    fn jpeg_limits_apply_to_the_native_scaled_decode() {
        let profile = grid_thumbnail_profile();
        assert_eq!(
            jpeg_scaled_dimensions(13_616, 3_616, profile.max_dimension),
            (1_702, 452)
        );

        let image_limits = MediaCacheImageLimits {
            max_dimension: 12_288,
            max_pixels: 40_000_000,
            max_decode_bytes: 256 * 1024 * 1024,
        };
        let error = validate_image_decode_limits(
            MediaImageFormat::Standard(ImageFormat::Jpeg),
            u16::MAX.into(),
            u16::MAX.into(),
            profile,
            &image_limits,
        )
        .expect("an oversized scaled JPEG buffer must be rejected locally");

        assert!(error.contains("decoded pixel count"));
    }

    #[test]
    fn thumbnail_limit_errors_remain_classifiable_through_context() {
        let error =
            anyhow::Error::new(BerryThumbnailError::LimitExceeded("test limit".to_string()))
                .context("thumbnail generation failed");

        assert!(is_thumbnail_limit_error(&error));
    }

    #[test]
    fn parses_quicktime_iso6709_video_locations() {
        let location = parse_iso6709_location("+47.3769+008.5417+00450.0/")
            .expect("ISO 6709 location should parse");
        assert!((location.latitude - 47.3769).abs() < 0.000_001);
        assert!((location.longitude - 8.5417).abs() < 0.000_001);
        assert!(parse_iso6709_location("not-a-location").is_none());
        assert!(parse_iso6709_location("+91.0+008.5/").is_none());
    }

    #[test]
    fn ffprobe_capture_time_requires_an_explicit_quicktime_offset_for_utc_basis() {
        let floating_stream_tags = FfprobeTags {
            creation_time: Some("2024-03-04T05:06:07Z".to_string()),
            ..FfprobeTags::default()
        };
        assert_eq!(
            ffprobe_capture_time(None, &floating_stream_tags),
            (Some(1_709_528_767), Some(false))
        );

        let offset_format_tags = FfprobeTags {
            creation_time: Some("2024-03-04T05:06:07Z".to_string()),
            quicktime_creation_date: Some("2024-03-04T06:06:07+0100".to_string()),
            ..FfprobeTags::default()
        };
        assert_eq!(
            ffprobe_capture_time(Some(&offset_format_tags), &floating_stream_tags),
            (Some(1_709_528_767), Some(true))
        );
    }

    #[test]
    fn estimated_image_build_bytes_uses_conservative_budget_without_dimensions() {
        let image_limits = MediaCacheImageLimits {
            max_dimension: 4_096,
            max_pixels: 40_000_000,
            max_decode_bytes: 256 * 1024 * 1024,
        };

        assert_eq!(
            estimated_image_build_bytes(32 * 1024, None, None, true, &image_limits),
            image_limits
                .max_decode_bytes
                .max(image_limits.max_pixels * IMAGE_DECODE_ESTIMATED_BYTES_PER_PIXEL)
        );
    }

    #[test]
    fn estimated_image_build_bytes_reserves_the_heif_decoder_budget() {
        let source_size = 3 * 1024 * 1024;
        let dimensions = (4_032, 3_024);

        assert_eq!(
            estimated_image_build_bytes(
                source_size,
                Some(dimensions),
                Some(MediaImageFormat::Heif),
                true,
                &MediaCacheImageLimits::default(),
            ),
            MediaCacheImageLimits::default().max_decode_bytes
        );
    }
}
