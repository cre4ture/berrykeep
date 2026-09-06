use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use common::xmp::XmpGeoLocation;
use tracing::warn;
use turso::transaction::{DropBehavior, Transaction, TransactionBehavior};
use turso::{Value, params_from_iter};
use uuid::Uuid;

use super::super::{
    CachedMediaMetadata, CurrentObjectEntry, FileVersionIndex,
    GALLERY_CAPTURE_FALLBACK_BACKFILL_KEY, GALLERY_LABELS_COLUMN, GALLERY_LABELS_COLUMN_DEFINITION,
    GalleryDeltaChange, GalleryDeltaCursorError, GalleryDeltaKind, GalleryDeltaPage,
    GalleryDeltaScope, GalleryIndexCapturedSort, GalleryIndexEntry, GalleryIndexMediaFilter,
    GalleryIndexMediaSummary, GalleryIndexPage, GalleryIndexQuery, GalleryMapCluster,
    GalleryMapClusterEntriesQuery, GalleryMapClusterPage, GalleryMapClusterQuery,
    GallerySummaryCacheValue, GallerySummaryMiss, GallerySummaryProgress,
    GallerySummaryRefreshStatus, GallerySummaryScope, GalleryViewportBounds, ManifestSummary,
    MediaGpsCoordinates, current_media_cache_metadata, decode_gallery_labels,
    effective_gallery_captured_at_unix, effective_gallery_gps, encode_gallery_labels,
    gallery_index_media_status, gallery_index_media_type_from_metadata,
    gallery_label_filter_matches_json, gallery_label_predicates, gallery_map_bounded_resolution,
    gallery_media_type_for_path, gallery_web_mercator_position, sqlite_like_prefix_pattern,
    version_created_at_unix_from_payload, version_index_head_projection,
};
#[cfg(test)]
use super::turso_test_db_path;
use super::{TursoMetadataStore, row_string, row_u64, upsert_version_index_head_projection};

const GALLERY_CHANGE_LOG_RETENTION: u64 = 100_000;
const GALLERY_SPATIAL_BACKFILL_CHUNK_ROWS: i64 = 1_000;

#[derive(Debug, PartialEq)]
struct GalleryProjectionState {
    key: String,
    media_type: Option<String>,
    captured_at_unix: u64,
    media_status: Option<String>,
    geotagged: u64,
    latitude: Option<f64>,
    longitude: Option<f64>,
}

pub(super) async fn init_gallery_projection(connection: &turso::Connection) -> Result<()> {
    connection
        .execute_batch(
            "
            CREATE TABLE IF NOT EXISTS gallery_objects (
                key TEXT PRIMARY KEY,
                manifest_hash TEXT NOT NULL,
                object_id TEXT NOT NULL,
                inferred_media_type TEXT,
                media_type TEXT,
                captured_at_unix INTEGER NOT NULL DEFAULT 0,
                media_status TEXT,
                geotagged INTEGER NOT NULL DEFAULT 0,
                latitude REAL,
                longitude REAL,
                sidecar_latitude REAL,
                sidecar_longitude REAL,
                sidecar_inferred_by_berrykeep INTEGER NOT NULL DEFAULT 0,
                spatial_x REAL,
                spatial_y REAL,
                labels_json TEXT NOT NULL DEFAULT '[]'
            );

            CREATE TABLE IF NOT EXISTS gallery_changes (
                revision INTEGER PRIMARY KEY,
                key TEXT NOT NULL,
                change_kind TEXT NOT NULL CHECK(change_kind IN ('upsert', 'removal')),
                previous_inferred_media_type TEXT,
                previous_media_type TEXT,
                previous_latitude REAL,
                previous_longitude REAL,
                previous_labels_json TEXT NOT NULL DEFAULT '[]'
            );

            CREATE INDEX IF NOT EXISTS idx_gallery_objects_media_order
                ON gallery_objects(media_type, captured_at_unix DESC, key ASC);
            CREATE INDEX IF NOT EXISTS idx_gallery_objects_manifest_hash
                ON gallery_objects(manifest_hash);
            CREATE INDEX IF NOT EXISTS idx_gallery_objects_viewport
                ON gallery_objects(latitude, longitude, media_type, captured_at_unix DESC, key ASC);
            CREATE INDEX IF NOT EXISTS idx_gallery_changes_key_revision
                ON gallery_changes(key, revision DESC);
            CREATE INDEX IF NOT EXISTS idx_manifest_summaries_content_fingerprint
                ON manifest_summaries(content_fingerprint);
            ",
        )
        .await?;
    super::add_column_if_missing(
        connection,
        "gallery_objects",
        "object_id",
        "TEXT NOT NULL DEFAULT ''",
    )
    .await?;
    add_gallery_projection_column(connection, "spatial_x", "REAL").await?;
    add_gallery_projection_column(connection, "spatial_y", "REAL").await?;
    add_gallery_projection_column(connection, "sidecar_latitude", "REAL").await?;
    add_gallery_projection_column(connection, "sidecar_longitude", "REAL").await?;
    add_gallery_projection_column(
        connection,
        "sidecar_inferred_by_berrykeep",
        "INTEGER NOT NULL DEFAULT 0",
    )
    .await?;
    connection
        .execute(
            "CREATE INDEX IF NOT EXISTS idx_gallery_objects_spatial
             ON gallery_objects(spatial_y, spatial_x, media_type, key)",
            (),
        )
        .await?;
    connection
        .execute(
            "CREATE INDEX IF NOT EXISTS idx_gallery_objects_spatial_backfill
             ON gallery_objects(key)
             WHERE geotagged != 0
               AND latitude IS NOT NULL
               AND longitude IS NOT NULL
               AND (spatial_x IS NULL OR spatial_y IS NULL)",
            (),
        )
        .await?;
    backfill_gallery_spatial_positions(connection).await?;
    super::add_column_if_missing(
        connection,
        "gallery_objects",
        GALLERY_LABELS_COLUMN,
        GALLERY_LABELS_COLUMN_DEFINITION,
    )
    .await?;
    // Chunked summary refreshes need `key` for stable pagination. Keep large label JSON out of
    // this otherwise covering index: label-filtered summaries can fetch it from the table.
    connection
        .execute(
            "CREATE INDEX IF NOT EXISTS idx_gallery_objects_capture_summary
             ON gallery_objects(
                 captured_at_unix,
                 inferred_media_type,
                 media_type,
                 media_status,
                 geotagged,
                 key
             )",
            (),
        )
        .await?;
    super::add_column_if_missing(
        connection,
        "gallery_changes",
        "previous_labels_json",
        "TEXT NOT NULL DEFAULT '[]'",
    )
    .await?;
    connection
        .execute(
            "INSERT INTO metadata_meta(key, value) VALUES('gallery_history_id', ?1)
             ON CONFLICT(key) DO NOTHING",
            (Uuid::new_v4().to_string(),),
        )
        .await?;
    connection
        .execute(
            "INSERT INTO metadata_meta(key, value) VALUES('gallery_revision', '0')
             ON CONFLICT(key) DO NOTHING",
            (),
        )
        .await?;
    let trigger_sql = format!(
        "
            DROP TRIGGER IF EXISTS gallery_objects_change_insert;
            DROP TRIGGER IF EXISTS gallery_objects_change_update;
            DROP TRIGGER IF EXISTS gallery_objects_change_delete;

            CREATE TRIGGER gallery_objects_change_insert
            AFTER INSERT ON gallery_objects
            BEGIN
                UPDATE metadata_meta
                   SET value = CAST(value AS INTEGER) + 1
                 WHERE key = 'gallery_revision';
                INSERT INTO gallery_changes(revision, key, change_kind)
                SELECT CAST(value AS INTEGER), NEW.key, 'upsert'
                  FROM metadata_meta WHERE key = 'gallery_revision';
                DELETE FROM gallery_changes
                 WHERE revision <= CAST((SELECT value FROM metadata_meta WHERE key = 'gallery_revision') AS INTEGER) - {GALLERY_CHANGE_LOG_RETENTION};
            END;

            CREATE TRIGGER gallery_objects_change_update
            AFTER UPDATE ON gallery_objects
            WHEN OLD.manifest_hash IS NOT NEW.manifest_hash
              OR OLD.object_id IS NOT NEW.object_id
              OR OLD.inferred_media_type IS NOT NEW.inferred_media_type
              OR OLD.media_type IS NOT NEW.media_type
              OR OLD.captured_at_unix IS NOT NEW.captured_at_unix
              OR OLD.media_status IS NOT NEW.media_status
              OR OLD.geotagged IS NOT NEW.geotagged
              OR OLD.latitude IS NOT NEW.latitude
              OR OLD.longitude IS NOT NEW.longitude
              OR OLD.labels_json IS NOT NEW.labels_json
            BEGIN
                UPDATE metadata_meta
                   SET value = CAST(value AS INTEGER) + 1
                 WHERE key = 'gallery_revision';
                INSERT INTO gallery_changes(
                    revision,
                    key,
                    change_kind,
                    previous_inferred_media_type,
                    previous_media_type,
                    previous_latitude,
                    previous_longitude,
                    previous_labels_json
                )
                SELECT
                    CAST(value AS INTEGER),
                    NEW.key,
                    'upsert',
                    OLD.inferred_media_type,
                    OLD.media_type,
                    OLD.latitude,
                    OLD.longitude,
                    OLD.labels_json
                  FROM metadata_meta WHERE key = 'gallery_revision';
                DELETE FROM gallery_changes
                 WHERE revision <= CAST((SELECT value FROM metadata_meta WHERE key = 'gallery_revision') AS INTEGER) - {GALLERY_CHANGE_LOG_RETENTION};
            END;

            CREATE TRIGGER gallery_objects_change_delete
            AFTER DELETE ON gallery_objects
            BEGIN
                UPDATE metadata_meta
                   SET value = CAST(value AS INTEGER) + 1
                 WHERE key = 'gallery_revision';
                INSERT INTO gallery_changes(
                    revision,
                    key,
                    change_kind,
                    previous_inferred_media_type,
                    previous_media_type,
                    previous_latitude,
                    previous_longitude,
                    previous_labels_json
                )
                SELECT
                    CAST(value AS INTEGER),
                    OLD.key,
                    'removal',
                    OLD.inferred_media_type,
                    OLD.media_type,
                    OLD.latitude,
                    OLD.longitude,
                    OLD.labels_json
                  FROM metadata_meta WHERE key = 'gallery_revision';
                DELETE FROM gallery_changes
                 WHERE revision <= CAST((SELECT value FROM metadata_meta WHERE key = 'gallery_revision') AS INTEGER) - {GALLERY_CHANGE_LOG_RETENTION};
            END;
            ",
    );
    connection
        .execute_batch(&trigger_sql)
        .await
        .context("failed to initialize Turso gallery change log")
}

async fn add_gallery_projection_column(
    connection: &turso::Connection,
    column: &str,
    column_type: &str,
) -> Result<()> {
    let sql = format!("ALTER TABLE gallery_objects ADD COLUMN {column} {column_type}");
    if let Err(error) = connection.execute(&sql, ()).await
        && !error.to_string().contains("duplicate column name")
    {
        return Err(error).with_context(|| format!("failed adding gallery_objects.{column}"));
    }
    Ok(())
}

async fn backfill_gallery_spatial_positions(connection: &turso::Connection) -> Result<()> {
    let transaction =
        Transaction::new_unchecked(connection, TransactionBehavior::Immediate).await?;
    let result = async {
        let mut cursor: Option<String> = None;
        loop {
            let cursor_value = cursor.clone().map(Value::from).unwrap_or(Value::Null);
            let mut rows = transaction
                .query(
                    "SELECT key, latitude, longitude
                     FROM gallery_objects
                     WHERE geotagged != 0
                       AND latitude IS NOT NULL
                       AND longitude IS NOT NULL
                       AND (spatial_x IS NULL OR spatial_y IS NULL)
                       AND (?1 IS NULL OR key > ?1)
                     ORDER BY key
                     LIMIT ?2",
                    params_from_iter([
                        cursor_value,
                        Value::from(GALLERY_SPATIAL_BACKFILL_CHUNK_ROWS),
                    ]),
                )
                .await?;
            let mut positions = Vec::new();
            while let Some(row) = rows.next().await? {
                positions.push((
                    row_string(&row, 0, "gallery_objects.key")?,
                    row_f64(&row, 1, "gallery_objects.latitude")?,
                    row_f64(&row, 2, "gallery_objects.longitude")?,
                ));
            }
            drop(rows);
            if positions.is_empty() {
                break;
            }
            let next_cursor = positions.last().map(|(key, _, _)| key.clone());
            let completed = positions.len() < GALLERY_SPATIAL_BACKFILL_CHUNK_ROWS as usize;
            for (key, latitude, longitude) in positions {
                let Some((spatial_x, spatial_y)) =
                    gallery_web_mercator_position(latitude, longitude)
                else {
                    continue;
                };
                transaction
                    .execute(
                        "UPDATE gallery_objects SET spatial_x = ?1, spatial_y = ?2 WHERE key = ?3",
                        (spatial_x, spatial_y, key),
                    )
                    .await?;
            }
            cursor = next_cursor;
            if completed {
                break;
            }
        }
        Ok(())
    }
    .await;
    finish_gallery_transaction(transaction, result).await
}

impl TursoMetadataStore {
    pub(super) async fn backfill_gallery_objects(&self) -> Result<()> {
        let _writer = self.writer_lock.lock().await;
        let connection = &self.connection;
        let transaction =
            Transaction::new_unchecked(connection, TransactionBehavior::Immediate).await?;
        let result = async {
            let mut marker_rows = connection
                .query(
                    "SELECT 1 FROM metadata_meta WHERE key = ?1",
                    (GALLERY_CAPTURE_FALLBACK_BACKFILL_KEY,),
                )
                .await?;
            let capture_fallback_backfill_needed = marker_rows.next().await?.is_none();
            drop(marker_rows);
            let mut rows = connection
                .query(
                    "SELECT current_objects.key, current_objects.manifest_hash, current_objects.object_id
                     FROM current_objects
                     LEFT JOIN gallery_objects ON gallery_objects.key = current_objects.key
                     WHERE gallery_objects.key IS NULL
                        OR gallery_objects.manifest_hash != current_objects.manifest_hash
                        OR gallery_objects.object_id != current_objects.object_id
                        OR (
                            gallery_objects.geotagged != 0
                            AND (gallery_objects.latitude IS NULL OR gallery_objects.longitude IS NULL)
                        )
                        OR (?1 != 0 AND gallery_objects.captured_at_unix = 0)",
                    (i64::from(capture_fallback_backfill_needed),),
                )
                .await?;
            let mut entries = Vec::new();
            while let Some(row) = rows.next().await? {
                entries.push((
                    row_string(&row, 0, "current_objects.key")?,
                    CurrentObjectEntry {
                        manifest_hash: row_string(&row, 1, "current_objects.manifest_hash")?,
                        object_id: row_string(&row, 2, "current_objects.object_id")?,
                    },
                ));
            }
            for (key, entry) in entries {
                upsert_gallery_object(connection, &key, &entry).await?;
            }
            if capture_fallback_backfill_needed {
                connection
                    .execute(
                        "INSERT INTO metadata_meta(key, value) VALUES(?1, 'complete')
                         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                        (GALLERY_CAPTURE_FALLBACK_BACKFILL_KEY,),
                    )
                    .await?;
            }
            Ok(())
        }
        .await;
        finish_gallery_transaction(transaction, result).await
    }

    pub(super) async fn upsert_current_object_with_gallery(
        &self,
        key: &str,
        entry: &CurrentObjectEntry,
    ) -> Result<()> {
        let _writer = self.writer_lock.lock().await;
        let connection = &self.connection;
        let transaction =
            Transaction::new_unchecked(connection, TransactionBehavior::Immediate).await?;
        let result = async {
            connection
                .execute(
                    "INSERT INTO current_objects (key, manifest_hash, object_id)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT(key) DO UPDATE SET
                        manifest_hash = excluded.manifest_hash,
                        object_id = excluded.object_id",
                    (key, entry.manifest_hash.as_str(), entry.object_id.as_str()),
                )
                .await?;
            upsert_gallery_object(connection, key, entry).await
        }
        .await;
        finish_gallery_transaction(transaction, result).await
    }

    pub(super) async fn remove_current_object_with_gallery(&self, key: &str) -> Result<()> {
        let _writer = self.writer_lock.lock().await;
        let connection = &self.connection;
        let transaction =
            Transaction::new_unchecked(connection, TransactionBehavior::Immediate).await?;
        let result = async {
            connection
                .execute("DELETE FROM current_objects WHERE key = ?1", (key,))
                .await?;
            connection
                .execute("DELETE FROM gallery_objects WHERE key = ?1", (key,))
                .await?;
            Ok(())
        }
        .await;
        finish_gallery_transaction(transaction, result).await
    }

    /// Replaces the user labels the projection holds for `key`.
    ///
    /// Runs inside the gallery transaction so the change log trigger observes the
    /// label update and assigns it a revision, which is what lets delta sync
    /// propagate label changes to clients.
    pub(super) async fn store_gallery_object_labels(
        &self,
        key: &str,
        labels: &[String],
    ) -> Result<()> {
        let labels_json = encode_gallery_labels(labels)?;
        let _writer = self.writer_lock.lock().await;
        let connection = &self.connection;
        let transaction =
            Transaction::new_unchecked(connection, TransactionBehavior::Immediate).await?;
        let result = async {
            connection
                .execute(
                    "UPDATE gallery_objects SET labels_json = ?2 WHERE key = ?1",
                    (key, labels_json.as_str()),
                )
                .await?;
            Ok(())
        }
        .await;
        finish_gallery_transaction(transaction, result).await
    }

    /// Stores a GPS overlay belonging to one XMP sidecar. This is deliberately
    /// path-scoped: the media cache is keyed by content fingerprint and may be
    /// shared by byte-identical objects at unrelated paths.
    pub(super) async fn store_gallery_object_sidecar_gps(
        &self,
        key: &str,
        location: Option<XmpGeoLocation>,
    ) -> Result<()> {
        let _writer = self.writer_lock.lock().await;
        let connection = &self.connection;
        let transaction =
            Transaction::new_unchecked(connection, TransactionBehavior::Immediate).await?;
        let result = async {
            let mut rows = connection
                .query(
                    "SELECT manifest_hash, sidecar_latitude, sidecar_longitude,
                            sidecar_inferred_by_berrykeep
                     FROM gallery_objects WHERE key = ?1",
                    (key,),
                )
                .await?;
            let (
                manifest_hash,
                existing_latitude,
                existing_longitude,
                existing_inferred_by_berrykeep,
            ) = match rows.next().await? {
                Some(row) => (
                    row_string(&row, 0, "gallery_objects.manifest_hash")?,
                    row_opt_f64(&row, 1, "gallery_objects.sidecar_latitude")?,
                    row_opt_f64(&row, 2, "gallery_objects.sidecar_longitude")?,
                    row_u64(&row, 3, "gallery_objects.sidecar_inferred_by_berrykeep")?,
                ),
                None => return Ok(()),
            };
            drop(rows);
            let latitude = location.as_ref().map(|location| location.latitude);
            let longitude = location.as_ref().map(|location| location.longitude);
            let inferred_by_berrykeep = location
                .as_ref()
                .is_some_and(|location| location.inferred_by_berrykeep);
            if existing_latitude == latitude
                && existing_longitude == longitude
                && existing_inferred_by_berrykeep == u64::from(inferred_by_berrykeep)
            {
                return Ok(());
            }
            connection
                .execute(
                    "UPDATE gallery_objects
                     SET sidecar_latitude = ?2, sidecar_longitude = ?3,
                         sidecar_inferred_by_berrykeep = ?4
                     WHERE key = ?1",
                    params_from_iter(vec![
                        Value::from(key),
                        optional_real_value(latitude),
                        optional_real_value(longitude),
                        Value::from(i64::from(inferred_by_berrykeep)),
                    ]),
                )
                .await?;
            refresh_gallery_objects_for_manifest(connection, &manifest_hash).await
        }
        .await;
        finish_gallery_transaction(transaction, result).await
    }

    pub(super) async fn persist_media_cache_record_with_gallery(
        &self,
        metadata: &CachedMediaMetadata,
    ) -> Result<()> {
        let payload = serde_json::to_vec_pretty(metadata)?;
        let content_fingerprint = metadata.content_fingerprint.clone();
        let _writer = self.writer_lock.lock().await;
        let connection = &self.connection;
        let transaction =
            Transaction::new_unchecked(connection, TransactionBehavior::Immediate).await?;
        let result = async {
            let changed = connection
                .execute(
                    "INSERT INTO media_cache (content_fingerprint, metadata_json)
                     VALUES (?1, ?2)
                     ON CONFLICT(content_fingerprint) DO UPDATE
                     SET metadata_json = excluded.metadata_json
                     WHERE media_cache.metadata_json != excluded.metadata_json",
                    (content_fingerprint.as_str(), payload),
                )
                .await?;
            if changed > 0 {
                let unchanged_projection_keys =
                    refresh_gallery_objects_for_content_fingerprint_and_collect_unchanged_keys(
                        connection,
                        &content_fingerprint,
                    )
                    .await?;
                record_gallery_upserts_for_keys(connection, &unchanged_projection_keys).await?;
            }
            Ok(())
        }
        .await;
        finish_gallery_transaction(transaction, result).await
    }

    pub(super) async fn delete_media_cache_record_with_gallery(
        &self,
        content_fingerprint: &str,
    ) -> Result<()> {
        let _writer = self.writer_lock.lock().await;
        let connection = &self.connection;
        let transaction =
            Transaction::new_unchecked(connection, TransactionBehavior::Immediate).await?;
        let result = async {
            let deleted = connection
                .execute(
                    "DELETE FROM media_cache WHERE content_fingerprint = ?1",
                    (content_fingerprint,),
                )
                .await?;
            if deleted > 0 {
                let unchanged_projection_keys =
                    refresh_gallery_objects_for_content_fingerprint_and_collect_unchanged_keys(
                        connection,
                        content_fingerprint,
                    )
                    .await?;
                record_gallery_upserts_for_keys(connection, &unchanged_projection_keys).await?;
            }
            Ok(())
        }
        .await;
        finish_gallery_transaction(transaction, result).await
    }

    pub(super) async fn delete_invalid_media_cache_record_if_payload_matches(
        &self,
        content_fingerprint: &str,
        payload: &[u8],
    ) -> Result<bool> {
        let _writer = self.writer_lock.lock().await;
        let connection = &self.connection;
        let transaction =
            Transaction::new_unchecked(connection, TransactionBehavior::Immediate).await?;
        let result = async {
            let deleted = connection
                .execute(
                    "DELETE FROM media_cache
                     WHERE content_fingerprint = ?1
                       AND metadata_json = ?2",
                    (content_fingerprint, payload.to_vec()),
                )
                .await?;
            if deleted > 0 {
                let unchanged_projection_keys =
                    refresh_gallery_objects_for_content_fingerprint_and_collect_unchanged_keys(
                        connection,
                        content_fingerprint,
                    )
                    .await?;
                record_gallery_upserts_for_keys(connection, &unchanged_projection_keys).await?;
            }
            Ok(deleted > 0)
        }
        .await;
        finish_gallery_transaction(transaction, result).await
    }

    pub(super) async fn persist_manifest_summary_with_gallery(
        &self,
        manifest_hash: &str,
        summary: &ManifestSummary,
    ) -> Result<()> {
        let total_size_bytes =
            i64::try_from(summary.total_size_bytes).context("manifest summary size overflow")?;
        let _writer = self.writer_lock.lock().await;
        let connection = &self.connection;
        let transaction =
            Transaction::new_unchecked(connection, TransactionBehavior::Immediate).await?;
        let result = async {
            let changed = connection
                .execute(
                    "INSERT INTO manifest_summaries (manifest_hash, total_size_bytes, content_fingerprint)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT(manifest_hash) DO UPDATE
                     SET total_size_bytes = excluded.total_size_bytes,
                         content_fingerprint = excluded.content_fingerprint
                     WHERE manifest_summaries.total_size_bytes != excluded.total_size_bytes
                        OR manifest_summaries.content_fingerprint != excluded.content_fingerprint",
                    (manifest_hash, total_size_bytes, summary.content_fingerprint.as_str()),
            )
            .await?;
            if changed > 0 {
                let unchanged_projection_keys =
                    refresh_gallery_objects_for_manifest_and_collect_unchanged_keys(
                        connection,
                        manifest_hash,
                    )
                    .await?;
                record_gallery_upserts_for_keys(connection, &unchanged_projection_keys)
                    .await?;
            }
            Ok(())
        }
        .await;
        finish_gallery_transaction(transaction, result).await
    }

    pub(super) async fn persist_version_index_with_gallery(
        &self,
        object_id: &str,
        index: &FileVersionIndex,
    ) -> Result<()> {
        let payload = serde_json::to_vec_pretty(index)?;
        let head_projection = version_index_head_projection(object_id, index);
        let _writer = self.writer_lock.lock().await;
        let connection = &self.connection;
        let transaction =
            Transaction::new_unchecked(connection, TransactionBehavior::Immediate).await?;
        let result = async {
            let changed = connection
                .execute(
                    "INSERT INTO version_indexes (object_id, index_json)
                     VALUES (?1, ?2)
                     ON CONFLICT(object_id) DO UPDATE SET index_json = excluded.index_json
                     WHERE version_indexes.index_json != excluded.index_json",
                    (object_id, payload),
                )
                .await?;
            upsert_version_index_head_projection(connection, &head_projection).await?;
            if changed > 0 {
                let unchanged_projection_keys =
                    refresh_gallery_objects_for_object_id_and_collect_unchanged_keys(
                        connection, object_id,
                    )
                    .await?;
                record_gallery_upserts_for_keys(connection, &unchanged_projection_keys).await?;
            }
            Ok(())
        }
        .await;
        finish_gallery_transaction(transaction, result).await
    }

    pub(super) async fn query_turso_gallery_index(
        &self,
        query: &GalleryIndexQuery,
    ) -> Result<GalleryIndexPage> {
        let connection = self.gallery_read_connection().await?;
        let transaction =
            Transaction::new_unchecked(&connection, TransactionBehavior::Deferred).await?;
        let result = async {
            let history_id = current_gallery_history_id(&transaction).await?;
            let revision = current_gallery_revision(&transaction).await?;
            query_gallery_index(&transaction, query, history_id, revision).await
        }
        .await;
        finish_gallery_read_transaction(transaction, result).await
    }

    pub(super) async fn query_turso_gallery_map_clusters(
        &self,
        query: &GalleryMapClusterQuery,
    ) -> Result<GalleryMapClusterPage> {
        let scope = GallerySummaryScope {
            prefix: query.prefix.trim().trim_matches('/').to_string(),
            depth: query.depth,
            media_filter: query.media_filter,
            captured_from_unix: query.captured_from_unix,
            captured_until_unix: query.captured_until_unix,
            label_filter: query.label_filter.clone(),
        };
        let cached_summary = self.gallery_map_summary_cache.cached(&scope);
        let (history_id, cache_revision, revision, resolution, visible_geotagged_count, clusters) = {
            let connection = self.gallery_read_connection().await?;
            let transaction =
                Transaction::new_unchecked(&connection, TransactionBehavior::Deferred).await?;
            let result = async {
                let history_id = current_gallery_history_id(&transaction).await?;
                let cache_revision = current_gallery_revision(&transaction).await?;
                let revision = current_gallery_scope_revision(
                    &transaction,
                    query.prefix.trim().trim_matches('/'),
                    query.depth,
                )
                .await?;
                let (resolution, visible_geotagged_count, clusters) =
                    query_gallery_map_cluster_cells(&transaction, query).await?;
                Ok((
                    history_id,
                    cache_revision,
                    revision,
                    resolution,
                    visible_geotagged_count,
                    clusters,
                ))
            }
            .await;
            finish_gallery_read_transaction(transaction, result).await?
        };

        let (total_entry_count, media_summary, summary_status) = self
            .gallery_map_summary(scope, &history_id, cache_revision, cached_summary)
            .await?;

        Ok(GalleryMapClusterPage {
            history_id,
            revision,
            total_entry_count,
            media_summary,
            visible_geotagged_count,
            resolution,
            clusters,
            summary_status,
        })
    }

    /// Returns the whole-scope gallery map summary for `scope`, serving a cached (possibly
    /// stale) value immediately rather than blocking the caller on the underlying aggregate
    /// query. A cold cache is computed by one leader while callers for the same scope wait for
    /// its result. See `SqliteMetadataStore::gallery_map_summary` for the SQLite policy.
    async fn gallery_map_summary(
        &self,
        scope: GallerySummaryScope,
        history_id: &str,
        revision: u64,
        cached: Option<GallerySummaryCacheValue>,
    ) -> Result<(usize, GalleryIndexMediaSummary, GallerySummaryRefreshStatus)> {
        let mut cached_snapshot = cached;
        let mut summary_miss = None;
        loop {
            // Prefer a value populated while the viewport query was running, but retain the
            // preflight snapshot in case that scope was evicted in the meantime.
            let cached = self
                .gallery_map_summary_cache
                .cached(&scope)
                .or(cached_snapshot.take());
            if let Some(cached) = cached {
                if cached.history_id == history_id && cached.revision == revision {
                    return Ok((
                        cached.total_entry_count,
                        cached.media_summary,
                        GallerySummaryRefreshStatus::default(),
                    ));
                }
                if let Some(progress) = self.gallery_map_summary_cache.try_start_refresh(&scope) {
                    let connections = self.gallery_summary_read_connection_factory();
                    let cache = self.gallery_map_summary_cache.clone();
                    let refresh_scope = scope.clone();
                    let estimate = Some(cached.total_entry_count);
                    tokio::spawn(async move {
                        let result = match connections.open().await {
                            Ok(connection) => {
                                let result = query_gallery_map_summary(
                                    &connection,
                                    &refresh_scope,
                                    estimate,
                                    Some(&progress),
                                )
                                .await;
                                drop(connection);
                                result
                            }
                            Err(error) => Err(error),
                        };
                        match result {
                            Ok(value) => cache.store(refresh_scope.clone(), value),
                            Err(error) => {
                                warn!(error = %error, "failed to refresh gallery map summary in background")
                            }
                        }
                        cache.finish_refresh(&refresh_scope);
                    });
                }
                let status = self.gallery_map_summary_cache.status(&scope);
                return Ok((cached.total_entry_count, cached.media_summary, status));
            }

            match summary_miss.take() {
                Some(GallerySummaryMiss::Follower(completion)) => {
                    self.gallery_map_summary_cache
                        .wait_for_summary_miss(&scope, &completion)
                        .await;
                }
                Some(GallerySummaryMiss::Leader(_computation)) => {
                    let connection = self.gallery_read_connection().await?;
                    let value = query_gallery_map_summary(&connection, &scope, None, None).await?;
                    drop(connection);
                    self.gallery_map_summary_cache.store(scope, value.clone());
                    return Ok((
                        value.total_entry_count,
                        value.media_summary,
                        GallerySummaryRefreshStatus::default(),
                    ));
                }
                None => {
                    summary_miss = Some(
                        self.gallery_map_summary_cache
                            .try_start_summary_miss(&scope)?,
                    );
                }
            }
        }
    }

    pub(super) async fn query_turso_gallery_map_cluster_entries(
        &self,
        query: &GalleryMapClusterEntriesQuery,
    ) -> Result<GalleryIndexPage> {
        let connection = self.gallery_read_connection().await?;
        let transaction =
            Transaction::new_unchecked(&connection, TransactionBehavior::Deferred).await?;
        let result = async {
            let history_id = current_gallery_history_id(&transaction).await?;
            let revision = current_gallery_scope_revision(
                &transaction,
                query.prefix.trim().trim_matches('/'),
                query.depth,
            )
            .await?;
            query_gallery_map_cluster_entries(&transaction, query, history_id, revision).await
        }
        .await;
        finish_gallery_read_transaction(transaction, result).await
    }

    pub(super) async fn query_turso_gallery_delta(
        &self,
        history_id: &str,
        since_revision: u64,
        limit: usize,
        scope: &GalleryDeltaScope,
    ) -> Result<std::result::Result<GalleryDeltaPage, GalleryDeltaCursorError>> {
        let connection = self.gallery_read_connection().await?;
        let transaction =
            Transaction::new_unchecked(&connection, TransactionBehavior::Deferred).await?;
        let result =
            query_gallery_delta(&transaction, history_id, since_revision, limit, scope).await;
        finish_gallery_read_transaction(transaction, result).await
    }
}

async fn finish_gallery_transaction<T>(
    transaction: Transaction<'_>,
    result: Result<T>,
) -> Result<T> {
    match result {
        Ok(value) => {
            transaction.commit().await?;
            Ok(value)
        }
        Err(error) => {
            let _ = transaction.rollback().await;
            Err(error)
        }
    }
}

async fn finish_gallery_read_transaction<T>(
    mut transaction: Transaction<'_>,
    result: Result<T>,
) -> Result<T> {
    match result {
        Ok(value) => {
            transaction.set_drop_behavior(DropBehavior::Commit);
            transaction.finish().await?;
            Ok(value)
        }
        Err(error) => {
            let _ = transaction.rollback().await;
            Err(error)
        }
    }
}

async fn upsert_gallery_object(
    connection: &turso::Connection,
    key: &str,
    entry: &CurrentObjectEntry,
) -> Result<()> {
    let inferred_media_type = gallery_media_type_for_path(key);
    connection
        .execute(
            "INSERT INTO gallery_objects (
                 key, manifest_hash, object_id, inferred_media_type, media_type,
                 captured_at_unix, media_status, geotagged, latitude, longitude,
                 sidecar_latitude, sidecar_longitude, sidecar_inferred_by_berrykeep,
                 spatial_x, spatial_y
             ) VALUES (?1, ?2, ?3, ?4, ?4, 0, NULL, 0, NULL, NULL, NULL, NULL, 0, NULL, NULL)
             ON CONFLICT(key) DO UPDATE SET
                 manifest_hash = excluded.manifest_hash,
                 object_id = excluded.object_id,
                 inferred_media_type = excluded.inferred_media_type,
                 media_type = excluded.inferred_media_type,
                 captured_at_unix = 0,
                 media_status = NULL,
                 geotagged = 0,
                 latitude = NULL,
                 longitude = NULL,
                 sidecar_latitude = NULL,
                 sidecar_longitude = NULL,
                 sidecar_inferred_by_berrykeep = 0,
                 spatial_x = NULL,
                 spatial_y = NULL
             WHERE gallery_objects.manifest_hash != excluded.manifest_hash
                OR gallery_objects.object_id != excluded.object_id
                OR gallery_objects.inferred_media_type IS NOT excluded.inferred_media_type",
            params_from_iter(vec![
                Value::from(key),
                Value::from(entry.manifest_hash.clone()),
                Value::from(entry.object_id.clone()),
                optional_text_value(inferred_media_type),
            ]),
        )
        .await?;
    refresh_gallery_objects_for_manifest(connection, &entry.manifest_hash).await
}

async fn refresh_gallery_objects_for_manifest(
    connection: &turso::Connection,
    manifest_hash: &str,
) -> Result<()> {
    let mut rows = connection
        .query(
            "SELECT media_cache.metadata_json
             FROM manifest_summaries
             LEFT JOIN media_cache
                ON media_cache.content_fingerprint = manifest_summaries.content_fingerprint
             WHERE manifest_summaries.manifest_hash = ?1",
            (manifest_hash,),
        )
        .await?;
    let metadata = match rows.next().await? {
        Some(row) => row_opt_blob(&row, 0, "media_cache.metadata_json")?,
        None => None,
    }
    .and_then(|payload| serde_json::from_slice::<CachedMediaMetadata>(&payload).ok())
    .and_then(|metadata| current_media_cache_metadata(Some(metadata)));
    drop(rows);
    let media_type = gallery_index_media_type_from_metadata(metadata.as_ref());
    let media_status = gallery_index_media_status(metadata.as_ref());
    let gps = metadata
        .as_ref()
        .and_then(|metadata| metadata.gps.as_ref())
        .filter(|gps| {
            gps.latitude.is_finite()
                && (-90.0..=90.0).contains(&gps.latitude)
                && gps.longitude.is_finite()
                && (-180.0..=180.0).contains(&gps.longitude)
        });
    let mut rows = connection
        .query(
            "SELECT
                 gallery_objects.key,
                 version_indexes.index_json,
                 gallery_objects.sidecar_latitude,
                 gallery_objects.sidecar_longitude,
                 gallery_objects.sidecar_inferred_by_berrykeep
             FROM gallery_objects
             LEFT JOIN version_indexes
               ON version_indexes.object_id = gallery_objects.object_id
             WHERE gallery_objects.manifest_hash = ?1",
            (manifest_hash,),
        )
        .await?;
    let mut entries = Vec::new();
    while let Some(row) = rows.next().await? {
        entries.push((
            row_string(&row, 0, "gallery_objects.key")?,
            row_opt_blob(&row, 1, "version_indexes.index_json")?,
            row_opt_f64(&row, 2, "gallery_objects.sidecar_latitude")?,
            row_opt_f64(&row, 3, "gallery_objects.sidecar_longitude")?,
            row_u64(&row, 4, "gallery_objects.sidecar_inferred_by_berrykeep")?,
        ));
    }
    drop(rows);

    for (
        key,
        version_index_payload,
        sidecar_latitude,
        sidecar_longitude,
        sidecar_inferred_by_berrykeep,
    ) in entries
    {
        let version_created_at_unix =
            version_created_at_unix_from_payload(version_index_payload.as_deref(), manifest_hash)?;
        let captured_at_unix = effective_gallery_captured_at_unix(
            &key,
            media_status,
            metadata
                .as_ref()
                .and_then(|metadata| metadata.taken_at_unix),
            version_created_at_unix,
        );
        let sidecar_gps = match (sidecar_latitude, sidecar_longitude) {
            (Some(latitude), Some(longitude))
                if latitude.is_finite()
                    && (-90.0..=90.0).contains(&latitude)
                    && longitude.is_finite()
                    && (-180.0..=180.0).contains(&longitude) =>
            {
                Some(MediaGpsCoordinates {
                    latitude,
                    longitude,
                })
            }
            _ => None,
        };
        let effective_gps = effective_gallery_gps(
            gps,
            sidecar_gps.as_ref(),
            sidecar_inferred_by_berrykeep != 0,
        );
        let spatial_position = effective_gps
            .and_then(|gps| gallery_web_mercator_position(gps.latitude, gps.longitude));
        connection
            .execute(
                "UPDATE gallery_objects
                 SET media_type = COALESCE(?1, inferred_media_type),
                     captured_at_unix = ?2,
                     media_status = ?3,
                     geotagged = ?4,
                     latitude = ?5,
                     longitude = ?6,
                     spatial_x = ?7,
                     spatial_y = ?8
                 WHERE key = ?9",
                params_from_iter(vec![
                    optional_text_value(media_type),
                    Value::from(
                        i64::try_from(captured_at_unix).context("gallery capture time overflow")?,
                    ),
                    optional_text_value(media_status),
                    Value::from(i64::from(effective_gps.is_some())),
                    effective_gps
                        .map(|gps| Value::from(gps.latitude))
                        .unwrap_or(Value::Null),
                    effective_gps
                        .map(|gps| Value::from(gps.longitude))
                        .unwrap_or(Value::Null),
                    spatial_position
                        .map(|position| Value::from(position.0))
                        .unwrap_or(Value::Null),
                    spatial_position
                        .map(|position| Value::from(position.1))
                        .unwrap_or(Value::Null),
                    Value::from(key),
                ]),
            )
            .await?;
    }
    Ok(())
}

async fn refresh_gallery_objects_for_content_fingerprint(
    connection: &turso::Connection,
    content_fingerprint: &str,
) -> Result<()> {
    let mut rows = connection
        .query(
            "SELECT manifest_hash FROM manifest_summaries WHERE content_fingerprint = ?1",
            (content_fingerprint,),
        )
        .await?;
    let mut manifests = Vec::new();
    while let Some(row) = rows.next().await? {
        manifests.push(row_string(&row, 0, "manifest_summaries.manifest_hash")?);
    }
    for manifest_hash in manifests {
        refresh_gallery_objects_for_manifest(connection, &manifest_hash).await?;
    }
    Ok(())
}

async fn refresh_gallery_objects_for_content_fingerprint_and_collect_unchanged_keys(
    connection: &turso::Connection,
    content_fingerprint: &str,
) -> Result<Vec<String>> {
    let before =
        gallery_projection_states_for_content_fingerprint(connection, content_fingerprint).await?;
    refresh_gallery_objects_for_content_fingerprint(connection, content_fingerprint).await?;
    let after =
        gallery_projection_states_for_content_fingerprint(connection, content_fingerprint).await?;
    Ok(unchanged_gallery_projection_keys(before, after))
}

async fn refresh_gallery_objects_for_manifest_and_collect_unchanged_keys(
    connection: &turso::Connection,
    manifest_hash: &str,
) -> Result<Vec<String>> {
    let before = gallery_projection_states_for_manifest(connection, manifest_hash).await?;
    refresh_gallery_objects_for_manifest(connection, manifest_hash).await?;
    let after = gallery_projection_states_for_manifest(connection, manifest_hash).await?;
    Ok(unchanged_gallery_projection_keys(before, after))
}

async fn refresh_gallery_objects_for_object_id_and_collect_unchanged_keys(
    connection: &turso::Connection,
    object_id: &str,
) -> Result<Vec<String>> {
    let before = gallery_projection_states_for_object_id(connection, object_id).await?;
    let mut rows = connection
        .query(
            "SELECT DISTINCT manifest_hash FROM gallery_objects WHERE object_id = ?1",
            (object_id,),
        )
        .await?;
    let mut manifest_hashes = Vec::new();
    while let Some(row) = rows.next().await? {
        manifest_hashes.push(row_string(&row, 0, "gallery_objects.manifest_hash")?);
    }
    drop(rows);
    for manifest_hash in manifest_hashes {
        refresh_gallery_objects_for_manifest(connection, &manifest_hash).await?;
    }
    let after = gallery_projection_states_for_object_id(connection, object_id).await?;
    Ok(unchanged_gallery_projection_keys(before, after))
}

async fn gallery_projection_states_for_content_fingerprint(
    connection: &turso::Connection,
    content_fingerprint: &str,
) -> Result<Vec<GalleryProjectionState>> {
    gallery_projection_states_for_query(
        connection,
        "SELECT
             gallery_objects.key,
             gallery_objects.media_type,
             gallery_objects.captured_at_unix,
             gallery_objects.media_status,
             gallery_objects.geotagged,
             gallery_objects.latitude,
             gallery_objects.longitude
         FROM gallery_objects
         JOIN manifest_summaries
           ON manifest_summaries.manifest_hash = gallery_objects.manifest_hash
         WHERE manifest_summaries.content_fingerprint = ?1",
        content_fingerprint,
    )
    .await
}

async fn gallery_projection_states_for_manifest(
    connection: &turso::Connection,
    manifest_hash: &str,
) -> Result<Vec<GalleryProjectionState>> {
    gallery_projection_states_for_query(
        connection,
        "SELECT
             key,
             media_type,
             captured_at_unix,
             media_status,
             geotagged,
             latitude,
             longitude
         FROM gallery_objects
         WHERE manifest_hash = ?1",
        manifest_hash,
    )
    .await
}

async fn gallery_projection_states_for_object_id(
    connection: &turso::Connection,
    object_id: &str,
) -> Result<Vec<GalleryProjectionState>> {
    gallery_projection_states_for_query(
        connection,
        "SELECT
             key,
             media_type,
             captured_at_unix,
             media_status,
             geotagged,
             latitude,
             longitude
         FROM gallery_objects
         WHERE object_id = ?1",
        object_id,
    )
    .await
}

async fn gallery_projection_states_for_query(
    connection: &turso::Connection,
    sql: &str,
    parameter: &str,
) -> Result<Vec<GalleryProjectionState>> {
    let mut rows = connection.query(sql, (parameter,)).await?;
    let mut states = Vec::new();
    while let Some(row) = rows.next().await? {
        states.push(GalleryProjectionState {
            key: row_string(&row, 0, "gallery_objects.key")?,
            media_type: row_opt_string(&row, 1, "gallery_objects.media_type")?,
            captured_at_unix: row_u64(&row, 2, "gallery_objects.captured_at_unix")?,
            media_status: row_opt_string(&row, 3, "gallery_objects.media_status")?,
            geotagged: row_u64(&row, 4, "gallery_objects.geotagged")?,
            latitude: row_opt_f64(&row, 5, "gallery_objects.latitude")?,
            longitude: row_opt_f64(&row, 6, "gallery_objects.longitude")?,
        });
    }
    Ok(states)
}

fn unchanged_gallery_projection_keys(
    before: Vec<GalleryProjectionState>,
    after: Vec<GalleryProjectionState>,
) -> Vec<String> {
    let after_by_key = after
        .into_iter()
        .map(|state| (state.key.clone(), state))
        .collect::<HashMap<_, _>>();
    before
        .into_iter()
        .filter_map(|state| (after_by_key.get(&state.key) == Some(&state)).then_some(state.key))
        .collect()
}

async fn record_gallery_upserts_for_keys(
    connection: &turso::Connection,
    keys: &[String],
) -> Result<()> {
    for key in keys {
        record_gallery_change(connection, key, "upsert").await?;
    }
    Ok(())
}

async fn record_gallery_change(
    connection: &turso::Connection,
    key: &str,
    change_kind: &str,
) -> Result<()> {
    connection
        .execute(
            "UPDATE metadata_meta
             SET value = CAST(value AS INTEGER) + 1
             WHERE key = 'gallery_revision'",
            (),
        )
        .await?;
    let revision = current_gallery_revision(connection).await?;
    connection
        .execute(
            "INSERT INTO gallery_changes(revision, key, change_kind) VALUES (?1, ?2, ?3)",
            (
                i64::try_from(revision).context("gallery revision overflow")?,
                key,
                change_kind,
            ),
        )
        .await?;
    connection
        .execute(
            "DELETE FROM gallery_changes WHERE revision <= ?1",
            (
                i64::try_from(revision.saturating_sub(GALLERY_CHANGE_LOG_RETENTION))
                    .context("gallery change retention revision overflow")?,
            ),
        )
        .await?;
    Ok(())
}

async fn query_gallery_index(
    connection: &turso::Connection,
    query: &GalleryIndexQuery,
    history_id: String,
    revision: u64,
) -> Result<GalleryIndexPage> {
    let prefix = query.prefix.trim().trim_matches('/').to_string();
    let prefix_pattern = if prefix.is_empty() {
        "%".to_string()
    } else {
        sqlite_like_prefix_pattern(&format!("{prefix}/"))
    };
    let depth = i64::try_from(query.depth).context("gallery index depth overflow")?;
    let scope = "
        (?1 = '' OR gallery_objects.key = ?1 OR gallery_objects.key LIKE ?2 ESCAPE '\\')
        AND CASE
            WHEN ?1 = '' THEN CASE
                WHEN trim(gallery_objects.key, '/') = '' THEN 0
                ELSE length(trim(gallery_objects.key, '/'))
                     - length(replace(trim(gallery_objects.key, '/'), '/', '')) + 1
            END
            WHEN gallery_objects.key = ?1 THEN 0
            ELSE length(substr(gallery_objects.key, length(?1) + 2))
                 - length(replace(substr(gallery_objects.key, length(?1) + 2), '/', '')) + 1
        END <= ?3
        AND gallery_objects.inferred_media_type IS NOT NULL
        AND (?4 IS NULL OR gallery_objects.media_type = ?4)
        AND (
            ?5 IS NULL
            OR (
                gallery_objects.latitude BETWEEN ?5 AND ?6
                AND (
                    (?7 <= ?8 AND gallery_objects.longitude BETWEEN ?7 AND ?8)
                    OR (?7 > ?8 AND (gallery_objects.longitude >= ?7 OR gallery_objects.longitude <= ?8))
                )
            )
        )
        AND (?9 IS NULL OR gallery_objects.captured_at_unix >= ?9)
        AND (?10 IS NULL OR gallery_objects.captured_at_unix < ?10)";
    let params = gallery_scope_values(&prefix, &prefix_pattern, depth, query)?;
    let (summary_label_sql, summary_label_values) =
        gallery_label_predicates(&query.label_filter, params.len() + 1)?;
    let summary_sql = format!(
        "SELECT
             COUNT(*),
             COALESCE(SUM(CASE WHEN media_status = 'ready' THEN 1 ELSE 0 END), 0),
             COALESCE(SUM(CASE WHEN media_status IS NULL THEN 1 ELSE 0 END), 0),
             COALESCE(SUM(CASE WHEN media_status = 'incomplete' THEN 1 ELSE 0 END), 0),
             COALESCE(SUM(CASE WHEN media_type = 'image' THEN 1 ELSE 0 END), 0),
             COALESCE(SUM(CASE WHEN media_type = 'video' THEN 1 ELSE 0 END), 0),
             COALESCE(SUM(geotagged), 0)
         FROM gallery_objects
         WHERE {scope}{summary_label_sql}"
    );
    let mut summary_values = params.clone();
    summary_values.extend(summary_label_values.into_iter().map(Value::Text));
    let mut summary_rows = connection
        .query(summary_sql, params_from_iter(summary_values))
        .await?;
    let summary = summary_rows
        .next()
        .await?
        .ok_or_else(|| anyhow::anyhow!("gallery index summary query returned no row"))?;
    let count = |index, label| -> Result<usize> {
        usize::try_from(row_u64(&summary, index, label)?)
            .with_context(|| format!("gallery index count overflow for {label}"))
    };
    let total_entry_count = count(0, "gallery summary count")?;
    let media_summary = GalleryIndexMediaSummary {
        ready_count: count(1, "gallery summary ready")?,
        pending_count: count(2, "gallery summary pending")?,
        incomplete_count: count(3, "gallery summary incomplete")?,
        image_count: count(4, "gallery summary image")?,
        video_count: count(5, "gallery summary video")?,
        geotagged_count: count(6, "gallery summary geotagged")?,
    };
    let sort_direction = match query.captured_sort {
        GalleryIndexCapturedSort::Asc => "ASC",
        GalleryIndexCapturedSort::Desc => "DESC",
    };
    // The page statement binds the scope, then LIMIT/OFFSET, so its label
    // placeholders continue after those two.
    let (page_label_sql, page_label_values) =
        gallery_label_predicates(&query.label_filter, params.len() + 3)?;
    let limit_parameter = params.len() + 1;
    let offset_parameter = limit_parameter + 1;
    let page_sql = format!(
        "SELECT
             gallery_objects.key,
             gallery_objects.object_id,
             gallery_objects.manifest_hash,
             manifest_summaries.total_size_bytes,
             manifest_summaries.content_fingerprint,
             media_cache.metadata_json,
             version_indexes.index_json,
             gallery_objects.labels_json,
             gallery_objects.latitude,
             gallery_objects.longitude
         FROM gallery_objects
         LEFT JOIN manifest_summaries
           ON manifest_summaries.manifest_hash = gallery_objects.manifest_hash
         LEFT JOIN media_cache
           ON media_cache.content_fingerprint = manifest_summaries.content_fingerprint
         LEFT JOIN version_indexes
           ON version_indexes.object_id = gallery_objects.object_id
         WHERE {scope}{page_label_sql}
         ORDER BY gallery_objects.captured_at_unix {sort_direction}, gallery_objects.key ASC
         LIMIT ?{limit_parameter} OFFSET ?{offset_parameter}"
    );
    let mut page_params = params;
    page_params.push(Value::from(
        i64::try_from(query.limit).context("gallery index limit overflow")?,
    ));
    page_params.push(Value::from(
        i64::try_from(query.offset).context("gallery index offset overflow")?,
    ));
    page_params.extend(page_label_values.into_iter().map(Value::Text));
    let mut rows = connection
        .query(page_sql, params_from_iter(page_params))
        .await?;
    let mut entries = Vec::new();
    while let Some(row) = rows.next().await? {
        entries.push(materialize_gallery_index_entry(GalleryIndexEntrySource {
            key: row_string(&row, 0, "gallery_objects.key")?,
            object_id: row_string(&row, 1, "gallery_objects.object_id")?,
            manifest_hash: row_string(&row, 2, "gallery_objects.manifest_hash")?,
            size_bytes: row_opt_i64(&row, 3, "manifest_summaries.total_size_bytes")?,
            content_fingerprint: row_opt_string(&row, 4, "manifest_summaries.content_fingerprint")?,
            metadata_payload: row_opt_blob(&row, 5, "media_cache.metadata_json")?,
            version_index_payload: row_opt_blob(&row, 6, "version_indexes.index_json")?,
            labels_json: row_string(&row, 7, "gallery_objects.labels_json")?,
            gallery_latitude: row_opt_f64(&row, 8, "gallery_objects.latitude")?,
            gallery_longitude: row_opt_f64(&row, 9, "gallery_objects.longitude")?,
        })?);
    }
    Ok(GalleryIndexPage {
        history_id,
        revision,
        total_entry_count,
        media_summary,
        entries,
    })
}

const GALLERY_MAP_SCOPE_SQL: &str = "
    (?1 = '' OR gallery_objects.key = ?1 OR gallery_objects.key LIKE ?2 ESCAPE '\\')
    AND CASE
        WHEN ?1 = '' THEN CASE
            WHEN trim(gallery_objects.key, '/') = '' THEN 0
            ELSE length(trim(gallery_objects.key, '/'))
                 - length(replace(trim(gallery_objects.key, '/'), '/', '')) + 1
        END
        WHEN gallery_objects.key = ?1 THEN 0
        ELSE length(substr(gallery_objects.key, length(?1) + 2))
             - length(replace(substr(gallery_objects.key, length(?1) + 2), '/', '')) + 1
    END <= ?3
    AND gallery_objects.inferred_media_type IS NOT NULL
    AND (?4 IS NULL OR gallery_objects.media_type = ?4)";

const GALLERY_MAP_OPTIONAL_CAPTURE_SQL: &str = "
    AND (?5 IS NULL OR gallery_objects.captured_at_unix >= ?5)
    AND (?6 IS NULL OR gallery_objects.captured_at_unix < ?6)";

const GALLERY_MAP_CAPTURE_NONE_SQL: &str = "
    AND ?5 IS NULL
    AND ?6 IS NULL";
const GALLERY_MAP_CAPTURE_FROM_SQL: &str = "
    AND gallery_objects.captured_at_unix >= ?5
    AND ?6 IS NULL";
const GALLERY_MAP_CAPTURE_UNTIL_SQL: &str = "
    AND ?5 IS NULL
    AND gallery_objects.captured_at_unix < ?6";
const GALLERY_MAP_CAPTURE_RANGE_SQL: &str = "
    AND gallery_objects.captured_at_unix >= ?5
    AND gallery_objects.captured_at_unix < ?6";

fn gallery_map_summary_capture_sql(has_from: bool, has_until: bool) -> &'static str {
    match (has_from, has_until) {
        (false, false) => GALLERY_MAP_CAPTURE_NONE_SQL,
        (true, false) => GALLERY_MAP_CAPTURE_FROM_SQL,
        (false, true) => GALLERY_MAP_CAPTURE_UNTIL_SQL,
        (true, true) => GALLERY_MAP_CAPTURE_RANGE_SQL,
    }
}

const GALLERY_MAP_VIEWPORT_SQL: &str = "
    gallery_objects.latitude BETWEEN ?7 AND ?8
    AND (
        (?9 <= ?10 AND gallery_objects.longitude BETWEEN ?9 AND ?10)
        OR (?9 > ?10 AND (gallery_objects.longitude >= ?9 OR gallery_objects.longitude <= ?10))
    )
    AND gallery_objects.spatial_y BETWEEN ?11 AND ?12
    AND (
        (?13 <= ?14 AND gallery_objects.spatial_x BETWEEN ?13 AND ?14)
        OR (?13 > ?14 AND (gallery_objects.spatial_x >= ?13 OR gallery_objects.spatial_x <= ?14))
    )";

fn turso_gallery_map_scope_values(
    prefix: &str,
    prefix_pattern: &str,
    depth: usize,
    media_filter: GalleryIndexMediaFilter,
    captured_from_unix: Option<u64>,
    captured_until_unix: Option<u64>,
    viewport: GalleryViewportBounds,
) -> Result<Vec<Value>> {
    let (spatial_west, spatial_south) =
        gallery_web_mercator_position(viewport.south, viewport.west)
            .context("validated gallery map southwest bound should project")?;
    let (spatial_east, spatial_north) =
        gallery_web_mercator_position(viewport.north, viewport.east)
            .context("validated gallery map northeast bound should project")?;
    Ok(vec![
        Value::from(prefix),
        Value::from(prefix_pattern),
        Value::from(i64::try_from(depth).context("gallery map depth overflow")?),
        optional_text_value(media_filter.media_type()),
        optional_integer_value(captured_from_unix, "gallery map capture-time lower bound")?,
        optional_integer_value(captured_until_unix, "gallery map capture-time upper bound")?,
        Value::from(viewport.south),
        Value::from(viewport.north),
        Value::from(viewport.west),
        Value::from(viewport.east),
        Value::from(spatial_north),
        Value::from(spatial_south),
        Value::from(spatial_west),
        Value::from(spatial_east),
    ])
}

async fn gallery_map_summary_query(
    connection: &turso::Connection,
    scope_values: &[Value],
    capture_sql: &str,
    label_filter: &super::super::GalleryLabelFilter,
) -> Result<(usize, GalleryIndexMediaSummary)> {
    let (label_sql, label_values) = gallery_label_predicates(label_filter, scope_values.len() + 1)?;
    let summary_sql = format!(
        "SELECT
             COUNT(*),
             COALESCE(SUM(CASE WHEN media_status = 'ready' THEN 1 ELSE 0 END), 0),
             COALESCE(SUM(CASE WHEN media_status IS NULL THEN 1 ELSE 0 END), 0),
             COALESCE(SUM(CASE WHEN media_status = 'incomplete' THEN 1 ELSE 0 END), 0),
             COALESCE(SUM(CASE WHEN media_type = 'image' THEN 1 ELSE 0 END), 0),
             COALESCE(SUM(CASE WHEN media_type = 'video' THEN 1 ELSE 0 END), 0),
             COALESCE(SUM(geotagged), 0)
         FROM gallery_objects
         WHERE {GALLERY_MAP_SCOPE_SQL}{capture_sql}{label_sql}"
    );
    let mut values = scope_values.to_vec();
    values.extend(label_values.into_iter().map(Value::Text));
    let mut summary_rows = connection
        .query(summary_sql, params_from_iter(values))
        .await?;
    let summary = summary_rows
        .next()
        .await?
        .ok_or_else(|| anyhow::anyhow!("gallery map summary query returned no row"))?;
    let count = |index, label| -> Result<usize> {
        usize::try_from(row_u64(&summary, index, label)?)
            .with_context(|| format!("gallery map count overflow for {label}"))
    };
    let total_entry_count = count(0, "gallery map total")?;
    let media_summary = GalleryIndexMediaSummary {
        ready_count: count(1, "gallery map ready")?,
        pending_count: count(2, "gallery map pending")?,
        incomplete_count: count(3, "gallery map incomplete")?,
        image_count: count(4, "gallery map images")?,
        video_count: count(5, "gallery map videos")?,
        geotagged_count: count(6, "gallery map geotagged")?,
    };
    drop(summary_rows);
    Ok((total_entry_count, media_summary))
}

const GALLERY_MAP_SUMMARY_CHUNK_ROWS: i64 = 5_000;

/// Same result as [`gallery_map_summary_query`], but computed in small ordered chunks so a
/// caller can report coarse progress on a long-running background refresh instead of blocking on
/// one unbounded aggregate query. `total_estimate` (typically the previous cached count for this
/// scope) only turns "rows scanned so far" into a percentage; it does not affect the exact
/// result.
async fn gallery_map_summary_chunked_query(
    connection: &turso::Connection,
    scope_values: &[Value],
    capture_sql: &str,
    label_filter: &super::super::GalleryLabelFilter,
    total_estimate: Option<usize>,
    progress: &GallerySummaryProgress,
) -> Result<(usize, GalleryIndexMediaSummary)> {
    let (label_sql, label_values) = gallery_label_predicates(label_filter, scope_values.len() + 1)?;
    let cursor_parameter = scope_values.len() + label_values.len() + 1;
    let limit_parameter = cursor_parameter + 1;
    let sql = format!(
        "SELECT gallery_objects.key, gallery_objects.media_status, gallery_objects.media_type,
                gallery_objects.geotagged
           FROM gallery_objects
          WHERE {GALLERY_MAP_SCOPE_SQL}{capture_sql}{label_sql}
            AND (?{cursor_parameter} IS NULL OR gallery_objects.key > ?{cursor_parameter})
          ORDER BY gallery_objects.key
          LIMIT ?{limit_parameter}"
    );
    let mut cursor: Option<String> = None;
    let mut total = 0usize;
    let mut summary = GalleryIndexMediaSummary::default();
    loop {
        let mut values = scope_values.to_vec();
        values.extend(label_values.iter().cloned().map(Value::Text));
        values.push(optional_text_value(cursor.as_deref()));
        values.push(Value::from(GALLERY_MAP_SUMMARY_CHUNK_ROWS));
        let mut rows = connection.query(&sql, params_from_iter(values)).await?;
        let mut chunk_len = 0usize;
        let mut last_key = None;
        while let Some(row) = rows.next().await? {
            let key = row_string(&row, 0, "gallery_objects.key")?;
            let media_status = row_opt_string(&row, 1, "gallery_objects.media_status")?;
            let media_type = row_opt_string(&row, 2, "gallery_objects.media_type")?;
            let geotagged = row_u64(&row, 3, "gallery_objects.geotagged")?;
            chunk_len += 1;
            total += 1;
            match media_status.as_deref() {
                Some("ready") => summary.ready_count += 1,
                None => summary.pending_count += 1,
                Some("incomplete") => summary.incomplete_count += 1,
                _ => {}
            }
            match media_type.as_deref() {
                Some("image") => summary.image_count += 1,
                Some("video") => summary.video_count += 1,
                _ => {}
            }
            if geotagged != 0 {
                summary.geotagged_count += 1;
            }
            last_key = Some(key);
        }
        if let Some(estimate) = total_estimate.filter(|value| *value > 0) {
            let percent = ((total as f64 / estimate as f64) * 100.0).min(99.0) as u8;
            progress.report(percent);
        }
        if chunk_len < GALLERY_MAP_SUMMARY_CHUNK_ROWS as usize {
            break;
        }
        cursor = last_key;
    }
    Ok((total, summary))
}

async fn gallery_map_cluster_cells_query(
    connection: &turso::Connection,
    base_values: &[Value],
    requested_resolution: u32,
    max_clusters: usize,
    label_filter: &super::super::GalleryLabelFilter,
) -> Result<(u32, Vec<GalleryMapCluster>)> {
    let max_clusters = max_clusters.max(1);
    let mut resolution = requested_resolution.max(1);
    let clusters = loop {
        let resolution_parameter = base_values.len() + 1;
        let (label_sql, label_values) =
            gallery_label_predicates(label_filter, base_values.len() + 2)?;
        let limit_parameter = base_values.len() + 2 + label_values.len();
        let sql = format!(
            "SELECT
                 CAST(gallery_objects.spatial_x * ?{resolution_parameter} AS INTEGER),
                 CAST(gallery_objects.spatial_y * ?{resolution_parameter} AS INTEGER),
                 COUNT(*),
                 AVG(gallery_objects.latitude),
                 AVG(gallery_objects.longitude),
                 MIN(gallery_objects.latitude),
                 MAX(gallery_objects.latitude),
                 MIN(gallery_objects.longitude),
                 MAX(gallery_objects.longitude),
                 MIN(gallery_objects.key),
                 MIN(gallery_objects.object_id),
                 MIN(gallery_objects.manifest_hash),
                 MIN(manifest_summaries.total_size_bytes),
                 MIN(manifest_summaries.content_fingerprint),
                 MIN(media_cache.metadata_json),
                 MIN(version_indexes.index_json),
                 MIN(gallery_objects.labels_json),
                 MIN(gallery_objects.latitude),
                 MIN(gallery_objects.longitude)
             FROM gallery_objects
             LEFT JOIN manifest_summaries
               ON manifest_summaries.manifest_hash = gallery_objects.manifest_hash
             LEFT JOIN media_cache
               ON media_cache.content_fingerprint = manifest_summaries.content_fingerprint
             LEFT JOIN version_indexes
               ON version_indexes.object_id = gallery_objects.object_id
             WHERE {GALLERY_MAP_SCOPE_SQL}{GALLERY_MAP_OPTIONAL_CAPTURE_SQL}
               AND {GALLERY_MAP_VIEWPORT_SQL}{label_sql}
             GROUP BY 1, 2
             ORDER BY 2 ASC, 1 ASC
             LIMIT ?{limit_parameter}"
        );
        let mut values = base_values.to_vec();
        values.push(Value::from(i64::from(resolution)));
        values.extend(label_values.into_iter().map(Value::Text));
        values.push(Value::from(
            i64::try_from(max_clusters.saturating_add(1))
                .context("gallery map cluster limit overflow")?,
        ));
        let mut rows = connection.query(sql, params_from_iter(values)).await?;
        let mut clusters = Vec::new();
        while let Some(row) = rows.next().await? {
            let cell_x = u32::try_from(row_u64(&row, 0, "gallery map cell x")?)
                .context("gallery map cell x overflow")?;
            let cell_y = u32::try_from(row_u64(&row, 1, "gallery map cell y")?)
                .context("gallery map cell y overflow")?;
            let cluster_count = usize::try_from(row_u64(&row, 2, "gallery map cluster count")?)
                .context("gallery map cluster count overflow")?;
            let entry = if cluster_count == 1 {
                Some(materialize_gallery_index_entry(GalleryIndexEntrySource {
                    key: row_string(&row, 9, "gallery_objects.key")?,
                    object_id: row_string(&row, 10, "gallery_objects.object_id")?,
                    manifest_hash: row_string(&row, 11, "gallery_objects.manifest_hash")?,
                    size_bytes: row_opt_i64(&row, 12, "manifest_summaries.total_size_bytes")?,
                    content_fingerprint: row_opt_string(
                        &row,
                        13,
                        "manifest_summaries.content_fingerprint",
                    )?,
                    metadata_payload: row_opt_blob(&row, 14, "media_cache.metadata_json")?,
                    version_index_payload: row_opt_blob(&row, 15, "version_indexes.index_json")?,
                    labels_json: row_string(&row, 16, "gallery_objects.labels_json")?,
                    gallery_latitude: row_opt_f64(&row, 17, "gallery_objects.latitude")?,
                    gallery_longitude: row_opt_f64(&row, 18, "gallery_objects.longitude")?,
                })?)
            } else {
                None
            };
            clusters.push(GalleryMapCluster {
                cell_x,
                cell_y,
                count: cluster_count,
                latitude: row_f64(&row, 3, "gallery map latitude")?,
                longitude: row_f64(&row, 4, "gallery map longitude")?,
                bounds: GalleryViewportBounds {
                    south: row_f64(&row, 5, "gallery map south")?,
                    north: row_f64(&row, 6, "gallery map north")?,
                    west: row_f64(&row, 7, "gallery map west")?,
                    east: row_f64(&row, 8, "gallery map east")?,
                },
                entry,
            });
        }
        if clusters.len() <= max_clusters || resolution == 1 {
            break clusters;
        }
        resolution = (resolution / 2).max(1);
    };
    Ok((resolution, clusters))
}

/// Computes viewport clusters and the whole-scope summary together in one shot. Production code
/// no longer calls this directly (see `TursoMetadataStore::query_turso_gallery_map_clusters`,
/// which serves the summary from `GallerySummaryCache` instead); it is kept as the ground-truth
/// reference implementation exercised by the gallery map tests.
#[cfg(test)]
async fn query_gallery_map_clusters(
    connection: &turso::Connection,
    query: &GalleryMapClusterQuery,
    history_id: String,
    revision: u64,
) -> Result<GalleryMapClusterPage> {
    let prefix = query.prefix.trim().trim_matches('/').to_string();
    let prefix_pattern = if prefix.is_empty() {
        "%".to_string()
    } else {
        sqlite_like_prefix_pattern(&format!("{prefix}/"))
    };
    let base_values = turso_gallery_map_scope_values(
        &prefix,
        &prefix_pattern,
        query.depth,
        query.media_filter,
        query.captured_from_unix,
        query.captured_until_unix,
        query.viewport,
    )?;
    let (total_entry_count, media_summary) = gallery_map_summary_query(
        connection,
        &base_values[..6],
        gallery_map_summary_capture_sql(
            query.captured_from_unix.is_some(),
            query.captured_until_unix.is_some(),
        ),
        &query.label_filter,
    )
    .await?;
    let (resolution, clusters) = gallery_map_cluster_cells_query(
        connection,
        &base_values,
        gallery_map_bounded_resolution(
            query.requested_resolution,
            query.viewport,
            query.max_clusters,
        ),
        query.max_clusters,
        &query.label_filter,
    )
    .await?;
    let visible_geotagged_count = clusters.iter().map(|cluster| cluster.count).sum();
    Ok(GalleryMapClusterPage {
        history_id,
        revision,
        total_entry_count,
        media_summary,
        visible_geotagged_count,
        resolution,
        clusters,
        summary_status: GallerySummaryRefreshStatus::default(),
    })
}

/// Like the (test-only) full [`query_gallery_map_clusters`], but skips the whole-scope summary
/// aggregation entirely. Used on the hot request path, where the summary is served from
/// `GallerySummaryCache` instead so a map pan/zoom never pays for an unbounded aggregate query.
pub(super) async fn query_gallery_map_cluster_cells(
    connection: &turso::Connection,
    query: &GalleryMapClusterQuery,
) -> Result<(u32, usize, Vec<GalleryMapCluster>)> {
    let prefix = query.prefix.trim().trim_matches('/').to_string();
    let prefix_pattern = if prefix.is_empty() {
        "%".to_string()
    } else {
        sqlite_like_prefix_pattern(&format!("{prefix}/"))
    };
    let base_values = turso_gallery_map_scope_values(
        &prefix,
        &prefix_pattern,
        query.depth,
        query.media_filter,
        query.captured_from_unix,
        query.captured_until_unix,
        query.viewport,
    )?;
    let (resolution, clusters) = gallery_map_cluster_cells_query(
        connection,
        &base_values,
        gallery_map_bounded_resolution(
            query.requested_resolution,
            query.viewport,
            query.max_clusters,
        ),
        query.max_clusters,
        &query.label_filter,
    )
    .await?;
    let visible_geotagged_count = clusters.iter().map(|cluster| cluster.count).sum();
    Ok((resolution, visible_geotagged_count, clusters))
}

/// Computes the whole-scope summary for `scope`, either exactly (for the first, synchronous
/// computation of a never-before-seen scope) or in progress-reporting chunks (for a background
/// refresh of a scope that is already cached but stale). Manages its own transaction and reads
/// `history_id`/`revision` alongside the summary so the cached value can be compared for
/// staleness later.
pub(super) async fn query_gallery_map_summary(
    connection: &turso::Connection,
    scope: &GallerySummaryScope,
    total_estimate: Option<usize>,
    progress: Option<&GallerySummaryProgress>,
) -> Result<GallerySummaryCacheValue> {
    let transaction = Transaction::new_unchecked(connection, TransactionBehavior::Deferred).await?;
    let result = async {
        let history_id = current_gallery_history_id(&transaction).await?;
        let revision = current_gallery_revision(&transaction).await?;
        let prefix_pattern = if scope.prefix.is_empty() {
            "%".to_string()
        } else {
            sqlite_like_prefix_pattern(&format!("{}/", scope.prefix))
        };
        let depth = i64::try_from(scope.depth).context("gallery map summary depth overflow")?;
        let capture_sql = gallery_map_summary_capture_sql(
            scope.captured_from_unix.is_some(),
            scope.captured_until_unix.is_some(),
        );
        let scope_values = vec![
            Value::from(scope.prefix.as_str()),
            Value::from(prefix_pattern.as_str()),
            Value::from(depth),
            optional_text_value(scope.media_filter.media_type()),
            optional_integer_value(
                scope.captured_from_unix,
                "gallery map capture-time lower bound",
            )?,
            optional_integer_value(
                scope.captured_until_unix,
                "gallery map capture-time upper bound",
            )?,
        ];
        let (total_entry_count, media_summary) = match progress {
            Some(progress) => {
                gallery_map_summary_chunked_query(
                    &transaction,
                    &scope_values,
                    capture_sql,
                    &scope.label_filter,
                    total_estimate,
                    progress,
                )
                .await?
            }
            None => {
                gallery_map_summary_query(
                    &transaction,
                    &scope_values,
                    capture_sql,
                    &scope.label_filter,
                )
                .await?
            }
        };
        Ok(GallerySummaryCacheValue {
            history_id,
            revision,
            total_entry_count,
            media_summary,
        })
    }
    .await;
    finish_gallery_read_transaction(transaction, result).await
}

async fn query_gallery_map_cluster_entries(
    connection: &turso::Connection,
    query: &GalleryMapClusterEntriesQuery,
    history_id: String,
    revision: u64,
) -> Result<GalleryIndexPage> {
    let prefix = query.prefix.trim().trim_matches('/').to_string();
    let prefix_pattern = if prefix.is_empty() {
        "%".to_string()
    } else {
        sqlite_like_prefix_pattern(&format!("{prefix}/"))
    };
    let mut values = turso_gallery_map_scope_values(
        &prefix,
        &prefix_pattern,
        query.depth,
        query.media_filter,
        query.captured_from_unix,
        query.captured_until_unix,
        query.viewport,
    )?;
    let resolution_parameter = values.len() + 1;
    let cell_x_parameter = resolution_parameter + 1;
    let cell_y_parameter = resolution_parameter + 2;
    values.push(Value::from(i64::from(query.resolution.max(1))));
    values.push(Value::from(i64::from(query.cell_x)));
    values.push(Value::from(i64::from(query.cell_y)));
    let (label_sql, label_values) =
        gallery_label_predicates(&query.label_filter, values.len() + 1)?;
    values.extend(label_values.into_iter().map(Value::Text));
    let limit_parameter = values.len() + 1;
    let offset_parameter = limit_parameter + 1;
    let cell_scope = format!(
        "{GALLERY_MAP_SCOPE_SQL}{GALLERY_MAP_OPTIONAL_CAPTURE_SQL}
         AND {GALLERY_MAP_VIEWPORT_SQL}
         AND CAST(gallery_objects.spatial_x * ?{resolution_parameter} AS INTEGER) = ?{cell_x_parameter}
         AND CAST(gallery_objects.spatial_y * ?{resolution_parameter} AS INTEGER) = ?{cell_y_parameter}{label_sql}"
    );
    let summary_sql = format!(
        "SELECT
             COUNT(*),
             COALESCE(SUM(CASE WHEN media_status = 'ready' THEN 1 ELSE 0 END), 0),
             COALESCE(SUM(CASE WHEN media_status IS NULL THEN 1 ELSE 0 END), 0),
             COALESCE(SUM(CASE WHEN media_status = 'incomplete' THEN 1 ELSE 0 END), 0),
             COALESCE(SUM(CASE WHEN media_type = 'image' THEN 1 ELSE 0 END), 0),
             COALESCE(SUM(CASE WHEN media_type = 'video' THEN 1 ELSE 0 END), 0),
             COUNT(*)
         FROM gallery_objects
         WHERE {cell_scope}"
    );
    let mut summary_rows = connection
        .query(summary_sql, params_from_iter(values.iter().cloned()))
        .await?;
    let summary = summary_rows
        .next()
        .await?
        .ok_or_else(|| anyhow::anyhow!("gallery map cluster summary returned no row"))?;
    let count = |index, label| -> Result<usize> {
        usize::try_from(row_u64(&summary, index, label)?)
            .with_context(|| format!("gallery cluster entry count overflow for {label}"))
    };
    let total_entry_count = count(0, "gallery cluster total")?;
    let media_summary = GalleryIndexMediaSummary {
        ready_count: count(1, "gallery cluster ready")?,
        pending_count: count(2, "gallery cluster pending")?,
        incomplete_count: count(3, "gallery cluster incomplete")?,
        image_count: count(4, "gallery cluster images")?,
        video_count: count(5, "gallery cluster videos")?,
        geotagged_count: count(6, "gallery cluster geotagged")?,
    };
    drop(summary_rows);
    let page_sql = format!(
        "SELECT
             gallery_objects.key,
             gallery_objects.object_id,
             gallery_objects.manifest_hash,
             manifest_summaries.total_size_bytes,
             manifest_summaries.content_fingerprint,
             media_cache.metadata_json,
             version_indexes.index_json,
             gallery_objects.labels_json,
             gallery_objects.latitude,
             gallery_objects.longitude
         FROM gallery_objects
         LEFT JOIN manifest_summaries
           ON manifest_summaries.manifest_hash = gallery_objects.manifest_hash
         LEFT JOIN media_cache
           ON media_cache.content_fingerprint = manifest_summaries.content_fingerprint
         LEFT JOIN version_indexes
           ON version_indexes.object_id = gallery_objects.object_id
         WHERE {cell_scope}
         ORDER BY gallery_objects.captured_at_unix DESC, gallery_objects.key ASC
         LIMIT ?{limit_parameter} OFFSET ?{offset_parameter}"
    );
    values.push(Value::from(
        i64::try_from(query.limit.max(1)).context("gallery map entry limit overflow")?,
    ));
    values.push(Value::from(
        i64::try_from(query.offset).context("gallery map entry offset overflow")?,
    ));
    let mut rows = connection.query(page_sql, params_from_iter(values)).await?;
    let mut entries = Vec::new();
    while let Some(row) = rows.next().await? {
        entries.push(materialize_gallery_index_entry(GalleryIndexEntrySource {
            key: row_string(&row, 0, "gallery_objects.key")?,
            object_id: row_string(&row, 1, "gallery_objects.object_id")?,
            manifest_hash: row_string(&row, 2, "gallery_objects.manifest_hash")?,
            size_bytes: row_opt_i64(&row, 3, "manifest_summaries.total_size_bytes")?,
            content_fingerprint: row_opt_string(&row, 4, "manifest_summaries.content_fingerprint")?,
            metadata_payload: row_opt_blob(&row, 5, "media_cache.metadata_json")?,
            version_index_payload: row_opt_blob(&row, 6, "version_indexes.index_json")?,
            labels_json: row_string(&row, 7, "gallery_objects.labels_json")?,
            gallery_latitude: row_opt_f64(&row, 8, "gallery_objects.latitude")?,
            gallery_longitude: row_opt_f64(&row, 9, "gallery_objects.longitude")?,
        })?);
    }
    Ok(GalleryIndexPage {
        history_id,
        revision,
        total_entry_count,
        media_summary,
        entries,
    })
}

async fn query_gallery_delta(
    connection: &turso::Connection,
    history_id: &str,
    since_revision: u64,
    limit: usize,
    scope: &GalleryDeltaScope,
) -> Result<std::result::Result<GalleryDeltaPage, GalleryDeltaCursorError>> {
    let current_history_id = current_gallery_history_id(connection).await?;
    let current_revision = current_gallery_revision(connection).await?;
    if history_id != current_history_id {
        return Ok(Err(GalleryDeltaCursorError::HistoryMismatch {
            history_id: current_history_id,
            current_revision,
        }));
    }
    if since_revision > current_revision {
        return Ok(Err(GalleryDeltaCursorError::Ahead {
            history_id: current_history_id,
            current_revision,
        }));
    }
    let mut oldest_rows = connection
        .query("SELECT MIN(revision) FROM gallery_changes", ())
        .await?;
    let oldest_revision = match oldest_rows.next().await? {
        Some(row) => row_opt_u64(&row, 0, "gallery_changes.oldest_revision")?,
        None => None,
    }
    .unwrap_or(current_revision.saturating_add(1));
    if since_revision.saturating_add(1) < oldest_revision {
        return Ok(Err(GalleryDeltaCursorError::Expired {
            history_id: current_history_id,
            current_revision,
        }));
    }
    let sql_limit =
        i64::try_from(limit.saturating_add(1)).context("gallery delta limit overflow")?;
    let mut rows = connection
        .query(
            "SELECT
                 revision,
                 key,
                 previous_inferred_media_type,
                 previous_media_type,
                 previous_latitude,
                 previous_longitude,
                 previous_labels_json
             FROM gallery_changes
             WHERE revision > ?1
             ORDER BY revision ASC
             LIMIT ?2",
            (
                i64::try_from(since_revision).context("gallery delta revision overflow")?,
                sql_limit,
            ),
        )
        .await?;
    let mut raw_changes = Vec::new();
    while let Some(row) = rows.next().await? {
        raw_changes.push((
            row_u64(&row, 0, "gallery_changes.revision")?,
            row_string(&row, 1, "gallery_changes.key")?,
            row_opt_string(&row, 2, "gallery_changes.previous_inferred_media_type")?,
            row_opt_string(&row, 3, "gallery_changes.previous_media_type")?,
            row_opt_f64(&row, 4, "gallery_changes.previous_latitude")?,
            row_opt_f64(&row, 5, "gallery_changes.previous_longitude")?,
            row_string(&row, 6, "gallery_changes.previous_labels_json")?,
        ));
    }
    let has_more = raw_changes.len() > limit;
    raw_changes.truncate(limit);
    let next_revision = raw_changes
        .last()
        .map(|(revision, ..)| *revision)
        .unwrap_or(current_revision);
    let mut changes = Vec::with_capacity(raw_changes.len());
    for (
        _revision,
        key,
        previous_inferred_media_type,
        previous_media_type,
        previous_latitude,
        previous_longitude,
        previous_labels_json,
    ) in raw_changes
    {
        let entry = query_gallery_entry(connection, &key, scope).await?;
        if entry.is_some() {
            changes.push(GalleryDeltaChange {
                key,
                kind: GalleryDeltaKind::Upsert,
                entry,
            });
        } else if previous_inferred_media_type.is_some()
            && gallery_entry_matches_delta_scope(
                &key,
                previous_media_type.as_deref(),
                previous_latitude,
                previous_longitude,
                scope,
            )
            && gallery_label_filter_matches_json(&previous_labels_json, &scope.label_filter)?
        {
            changes.push(GalleryDeltaChange {
                key,
                kind: GalleryDeltaKind::Removal,
                entry: None,
            });
        }
    }
    Ok(Ok(GalleryDeltaPage {
        history_id: current_history_id,
        next_revision,
        has_more,
        changes,
    }))
}

async fn query_gallery_entry(
    connection: &turso::Connection,
    key: &str,
    scope: &GalleryDeltaScope,
) -> Result<Option<GalleryIndexEntry>> {
    // Filtering here rather than against the change log is what makes labelling
    // a photo `private` reach the client as a removal: the entry stops
    // resolving, and the caller falls through to its removal branch.
    let (label_sql, label_values) = gallery_label_predicates(&scope.label_filter, 2)?;
    let mut values = vec![Value::from(key)];
    values.extend(label_values.into_iter().map(Value::Text));
    let mut rows = connection
        .query(
            &format!(
                "SELECT
                 gallery_objects.key,
                 gallery_objects.object_id,
                 gallery_objects.manifest_hash,
                 manifest_summaries.total_size_bytes,
                 manifest_summaries.content_fingerprint,
                 media_cache.metadata_json,
                 version_indexes.index_json,
                 gallery_objects.media_type,
                 gallery_objects.latitude,
                 gallery_objects.longitude,
                 gallery_objects.labels_json
             FROM gallery_objects
             LEFT JOIN manifest_summaries
               ON manifest_summaries.manifest_hash = gallery_objects.manifest_hash
             LEFT JOIN media_cache
               ON media_cache.content_fingerprint = manifest_summaries.content_fingerprint
             LEFT JOIN version_indexes
               ON version_indexes.object_id = gallery_objects.object_id
             WHERE gallery_objects.key = ?1
               AND gallery_objects.inferred_media_type IS NOT NULL{label_sql}"
            ),
            params_from_iter(values),
        )
        .await?;
    let Some(row) = rows.next().await? else {
        return Ok(None);
    };
    let key = row_string(&row, 0, "gallery_objects.key")?;
    let media_type = row_opt_string(&row, 7, "gallery_objects.media_type")?;
    let latitude = row_opt_f64(&row, 8, "gallery_objects.latitude")?;
    let longitude = row_opt_f64(&row, 9, "gallery_objects.longitude")?;
    if !gallery_entry_matches_delta_scope(&key, media_type.as_deref(), latitude, longitude, scope) {
        return Ok(None);
    }
    Ok(Some(materialize_gallery_index_entry(
        GalleryIndexEntrySource {
            key,
            object_id: row_string(&row, 1, "gallery_objects.object_id")?,
            manifest_hash: row_string(&row, 2, "gallery_objects.manifest_hash")?,
            size_bytes: row_opt_i64(&row, 3, "manifest_summaries.total_size_bytes")?,
            content_fingerprint: row_opt_string(&row, 4, "manifest_summaries.content_fingerprint")?,
            metadata_payload: row_opt_blob(&row, 5, "media_cache.metadata_json")?,
            version_index_payload: row_opt_blob(&row, 6, "version_indexes.index_json")?,
            labels_json: row_string(&row, 10, "gallery_objects.labels_json")?,
            gallery_latitude: latitude,
            gallery_longitude: longitude,
        },
    )?))
}

fn gallery_scope_values(
    prefix: &str,
    prefix_pattern: &str,
    depth: i64,
    query: &GalleryIndexQuery,
) -> Result<Vec<Value>> {
    let media_type = query.media_filter.media_type();
    let (south, north, west, east) = query
        .viewport
        .map(|bounds| {
            (
                Some(bounds.south),
                Some(bounds.north),
                Some(bounds.west),
                Some(bounds.east),
            )
        })
        .unwrap_or((None, None, None, None));
    Ok(vec![
        Value::from(prefix),
        Value::from(prefix_pattern),
        Value::from(depth),
        optional_text_value(media_type),
        optional_real_value(south),
        optional_real_value(north),
        optional_real_value(west),
        optional_real_value(east),
        optional_integer_value(query.captured_from_unix, "gallery capture-time lower bound")?,
        optional_integer_value(
            query.captured_until_unix,
            "gallery capture-time upper bound",
        )?,
    ])
}

struct GalleryIndexEntrySource {
    key: String,
    object_id: String,
    manifest_hash: String,
    size_bytes: Option<i64>,
    content_fingerprint: Option<String>,
    metadata_payload: Option<Vec<u8>>,
    version_index_payload: Option<Vec<u8>>,
    labels_json: String,
    gallery_latitude: Option<f64>,
    gallery_longitude: Option<f64>,
}

fn materialize_gallery_index_entry(
    GalleryIndexEntrySource {
        key,
        object_id,
        manifest_hash,
        size_bytes,
        content_fingerprint,
        metadata_payload,
        version_index_payload,
        labels_json,
        gallery_latitude,
        gallery_longitude,
    }: GalleryIndexEntrySource,
) -> Result<GalleryIndexEntry> {
    let size_bytes = size_bytes
        .map(|value| u64::try_from(value).context("negative gallery entry size in Turso"))
        .transpose()?;
    let gallery_gps = match (gallery_latitude, gallery_longitude) {
        (Some(latitude), Some(longitude))
            if latitude.is_finite()
                && (-90.0..=90.0).contains(&latitude)
                && longitude.is_finite()
                && (-180.0..=180.0).contains(&longitude) =>
        {
            Some(MediaGpsCoordinates {
                latitude,
                longitude,
            })
        }
        _ => None,
    };
    let mut media_metadata = metadata_payload
        .and_then(|payload| serde_json::from_slice::<CachedMediaMetadata>(&payload).ok())
        .and_then(|metadata| current_media_cache_metadata(Some(metadata)));
    if let (Some(gallery_gps), Some(metadata)) = (&gallery_gps, media_metadata.as_mut()) {
        metadata.gps = Some(gallery_gps.clone());
    }
    let modified_at_unix =
        version_created_at_unix_from_payload(version_index_payload.as_deref(), &manifest_hash)?;
    let labels = decode_gallery_labels(&labels_json)?;
    Ok(GalleryIndexEntry {
        key,
        object_id,
        manifest_hash,
        size_bytes,
        modified_at_unix,
        content_fingerprint,
        media_metadata,
        gps_override: gallery_gps,
        labels,
    })
}

fn gallery_entry_matches_delta_scope(
    key: &str,
    media_type: Option<&str>,
    latitude: Option<f64>,
    longitude: Option<f64>,
    scope: &GalleryDeltaScope,
) -> bool {
    let prefix = scope.prefix.trim().trim_matches('/');
    let relative_key = if prefix.is_empty() {
        Some(key)
    } else if key == prefix {
        Some("")
    } else {
        key.strip_prefix(prefix)
            .and_then(|suffix| suffix.strip_prefix('/'))
    };
    let Some(relative_key) = relative_key else {
        return false;
    };
    let relative_depth = if relative_key.is_empty() {
        0
    } else {
        relative_key.bytes().filter(|byte| *byte == b'/').count() + 1
    };
    if relative_depth > scope.depth {
        return false;
    }
    let media_matches = match scope.media_filter {
        GalleryIndexMediaFilter::All => true,
        GalleryIndexMediaFilter::Image => media_type == Some("image"),
        GalleryIndexMediaFilter::Video => media_type == Some("video"),
    };
    if !media_matches {
        return false;
    }
    let Some(viewport) = scope.viewport else {
        return true;
    };
    let (Some(latitude), Some(longitude)) = (latitude, longitude) else {
        return false;
    };
    let longitude_matches = if viewport.west <= viewport.east {
        (viewport.west..=viewport.east).contains(&longitude)
    } else {
        longitude >= viewport.west || longitude <= viewport.east
    };
    (viewport.south..=viewport.north).contains(&latitude) && longitude_matches
}

async fn current_gallery_revision(connection: &turso::Connection) -> Result<u64> {
    let mut rows = connection
        .query(
            "SELECT value FROM metadata_meta WHERE key = 'gallery_revision'",
            (),
        )
        .await?;
    let row = rows
        .next()
        .await?
        .ok_or_else(|| anyhow::anyhow!("missing persisted Turso gallery revision"))?;
    let raw = row_string(&row, 0, "metadata_meta.gallery_revision")?;
    raw.parse::<u64>()
        .with_context(|| format!("invalid persisted Turso gallery revision: {raw}"))
}

/// Returns the most recent retained mutation that can affect a gallery map scope. Query tokens
/// use this scoped revision rather than the library-wide revision so media ingestion elsewhere in
/// a library does not repeatedly invalidate an open cluster or its pagination.
async fn current_gallery_scope_revision(
    connection: &turso::Connection,
    prefix: &str,
    depth: usize,
) -> Result<u64> {
    let prefix_pattern = if prefix.is_empty() {
        "%".to_string()
    } else {
        sqlite_like_prefix_pattern(&format!("{prefix}/"))
    };
    let depth = i64::try_from(depth).context("gallery map scope depth overflow")?;
    let mut rows = connection
        .query(
            "SELECT COALESCE(MAX(revision), 0)
             FROM gallery_changes
             WHERE (?1 = '' OR gallery_changes.key = ?1 OR gallery_changes.key LIKE ?2 ESCAPE '\\')
               AND CASE
                   WHEN ?1 = '' THEN CASE
                       WHEN trim(gallery_changes.key, '/') = '' THEN 0
                       ELSE length(trim(gallery_changes.key, '/'))
                            - length(replace(trim(gallery_changes.key, '/'), '/', '')) + 1
                   END
                   WHEN gallery_changes.key = ?1 THEN 0
                   ELSE length(substr(gallery_changes.key, length(?1) + 2))
                        - length(replace(substr(gallery_changes.key, length(?1) + 2), '/', '')) + 1
               END <= ?3",
            params_from_iter([
                Value::from(prefix.to_string()),
                Value::from(prefix_pattern),
                Value::from(depth),
            ]),
        )
        .await?;
    let row = rows
        .next()
        .await?
        .ok_or_else(|| anyhow::anyhow!("gallery map scope revision query returned no row"))?;
    row_u64(&row, 0, "gallery map scope revision")
}

async fn current_gallery_history_id(connection: &turso::Connection) -> Result<String> {
    let mut rows = connection
        .query(
            "SELECT value FROM metadata_meta WHERE key = 'gallery_history_id'",
            (),
        )
        .await?;
    let row = rows
        .next()
        .await?
        .ok_or_else(|| anyhow::anyhow!("missing persisted Turso gallery history id"))?;
    let history_id = row_string(&row, 0, "metadata_meta.gallery_history_id")?;
    Uuid::parse_str(&history_id)
        .with_context(|| format!("invalid persisted Turso gallery history id: {history_id}"))?;
    Ok(history_id)
}

fn optional_text_value(value: Option<&str>) -> Value {
    value.map(Value::from).unwrap_or(Value::Null)
}

fn optional_real_value(value: Option<f64>) -> Value {
    value.map(Value::from).unwrap_or(Value::Null)
}

fn optional_integer_value(value: Option<u64>, label: &str) -> Result<Value> {
    match value {
        Some(value) => Ok(Value::from(
            i64::try_from(value).with_context(|| format!("{label} overflow"))?,
        )),
        None => Ok(Value::Null),
    }
}

fn row_opt_string(row: &turso::Row, idx: usize, label: &str) -> Result<Option<String>> {
    match row.get_value(idx)? {
        Value::Null => Ok(None),
        Value::Text(value) => Ok(Some(value)),
        Value::Blob(value) => Ok(Some(
            String::from_utf8(value).with_context(|| format!("invalid utf-8 in {label}"))?,
        )),
        other => bail!("expected optional text value for {label}, got {other:?}"),
    }
}

fn row_opt_blob(row: &turso::Row, idx: usize, label: &str) -> Result<Option<Vec<u8>>> {
    match row.get_value(idx)? {
        Value::Null => Ok(None),
        Value::Blob(value) => Ok(Some(value)),
        Value::Text(value) => Ok(Some(value.into_bytes())),
        other => bail!("expected optional blob value for {label}, got {other:?}"),
    }
}

fn row_opt_i64(row: &turso::Row, idx: usize, label: &str) -> Result<Option<i64>> {
    match row.get_value(idx)? {
        Value::Null => Ok(None),
        Value::Integer(value) => Ok(Some(value)),
        other => bail!("expected optional integer value for {label}, got {other:?}"),
    }
}

fn row_opt_u64(row: &turso::Row, idx: usize, label: &str) -> Result<Option<u64>> {
    row_opt_i64(row, idx, label)?
        .map(|value| u64::try_from(value).with_context(|| format!("negative integer for {label}")))
        .transpose()
}

fn row_opt_f64(row: &turso::Row, idx: usize, label: &str) -> Result<Option<f64>> {
    match row.get_value(idx)? {
        Value::Null => Ok(None),
        Value::Real(value) => Ok(Some(value)),
        Value::Integer(value) => Ok(Some(value as f64)),
        other => bail!("expected optional real value for {label}, got {other:?}"),
    }
}

fn row_f64(row: &turso::Row, idx: usize, label: &str) -> Result<f64> {
    row_opt_f64(row, idx, label)?.ok_or_else(|| anyhow::anyhow!("missing real value for {label}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{
        ClientCredentialState, MediaCacheStatus, MetadataStore, RepairAttemptRecord,
    };

    async fn insert_gallery_fixture(
        connection: &turso::Connection,
        key: &str,
        media_type: &str,
        captured_at_unix: u64,
        latitude: Option<f64>,
        longitude: Option<f64>,
    ) {
        let spatial_position = latitude
            .zip(longitude)
            .and_then(|(latitude, longitude)| gallery_web_mercator_position(latitude, longitude));
        connection
            .execute(
                "INSERT INTO gallery_objects (
                     key, manifest_hash, object_id, inferred_media_type, media_type,
                     captured_at_unix, media_status, geotagged, latitude, longitude,
                     spatial_x, spatial_y
                 ) VALUES (?1, ?2, ?3, ?4, ?4, ?5, 'ready', ?6, ?7, ?8, ?9, ?10)",
                params_from_iter(vec![
                    Value::from(key),
                    Value::from(format!("manifest-{key}")),
                    Value::from(format!("object-{key}")),
                    Value::from(media_type),
                    Value::from(i64::try_from(captured_at_unix).unwrap()),
                    Value::from(i64::from(latitude.is_some())),
                    latitude.map(Value::from).unwrap_or(Value::Null),
                    longitude.map(Value::from).unwrap_or(Value::Null),
                    spatial_position
                        .map(|position| Value::from(position.0))
                        .unwrap_or(Value::Null),
                    spatial_position
                        .map(|position| Value::from(position.1))
                        .unwrap_or(Value::Null),
                ]),
            )
            .await
            .expect("gallery fixture should insert");
    }

    fn gallery_delta_scope() -> GalleryDeltaScope {
        GalleryDeltaScope {
            prefix: "gallery".to_string(),
            depth: 64,
            media_filter: GalleryIndexMediaFilter::All,
            captured_sort: GalleryIndexCapturedSort::Desc,
            viewport: None,
            label_filter: Default::default(),
        }
    }

    fn gallery_map_query(
        viewport: GalleryViewportBounds,
        requested_resolution: u32,
        max_clusters: usize,
    ) -> GalleryMapClusterQuery {
        GalleryMapClusterQuery {
            prefix: "gallery".to_string(),
            depth: 64,
            media_filter: GalleryIndexMediaFilter::Image,
            captured_from_unix: None,
            captured_until_unix: None,
            viewport,
            requested_resolution,
            max_clusters,
            label_filter: Default::default(),
        }
    }

    #[tokio::test]
    async fn gallery_map_clusters_are_bounded_in_turso() {
        let metadata_db_path = turso_test_db_path("gallery-map-bounded");
        let database = turso::Builder::new_local(&metadata_db_path.to_string_lossy())
            .build()
            .await
            .expect("turso should open");
        let connection = database.connect().expect("turso should connect");
        super::super::init_metadata_db(&connection)
            .await
            .expect("metadata schema should initialize");
        insert_gallery_fixture(
            &connection,
            "gallery/zurich.jpg",
            "image",
            30,
            Some(47.4),
            Some(8.5),
        )
        .await;
        insert_gallery_fixture(
            &connection,
            "gallery/new-york.jpg",
            "image",
            20,
            Some(40.7),
            Some(-74.0),
        )
        .await;
        insert_gallery_fixture(
            &connection,
            "gallery/sydney.jpg",
            "image",
            10,
            Some(-33.9),
            Some(151.2),
        )
        .await;
        insert_gallery_fixture(&connection, "gallery/no-gps.jpg", "image", 40, None, None).await;

        let page = query_gallery_map_clusters(
            &connection,
            &gallery_map_query(
                GalleryViewportBounds {
                    south: -90.0,
                    west: -180.0,
                    north: 90.0,
                    east: 180.0,
                },
                4096,
                1,
            ),
            current_gallery_history_id(&connection).await.unwrap(),
            current_gallery_revision(&connection).await.unwrap(),
        )
        .await
        .expect("gallery clusters should load");

        assert_eq!(page.total_entry_count, 4);
        assert_eq!(page.media_summary.geotagged_count, 3);
        assert_eq!(page.visible_geotagged_count, 3);
        assert_eq!(page.clusters.len(), 1);
        assert!(page.resolution < 4096);
        assert_eq!(page.clusters[0].count, 3);

        drop(connection);
        drop(database);
        let _ = std::fs::remove_file(metadata_db_path);
    }

    #[tokio::test]
    async fn gallery_map_summary_cache_serves_stale_value_and_refreshes_in_background_turso() {
        let metadata_db_path = turso_test_db_path("gallery-map-summary-cache");
        let store = TursoMetadataStore::open(&metadata_db_path)
            .await
            .expect("turso metadata store should open");
        insert_gallery_fixture(
            &store.connection,
            "gallery/a.jpg",
            "image",
            1,
            Some(47.4),
            Some(8.5),
        )
        .await;

        let viewport = GalleryViewportBounds {
            south: -90.0,
            west: -180.0,
            north: 90.0,
            east: 180.0,
        };
        let query = gallery_map_query(viewport, 1024, 512);

        // Cold cache: computed synchronously, so the very first caller still gets a real answer.
        let first = store
            .query_gallery_map_clusters(&query)
            .await
            .unwrap()
            .expect("gallery map clusters should be available");
        assert_eq!(first.total_entry_count, 1);
        assert!(!first.summary_status.refreshing);

        insert_gallery_fixture(
            &store.connection,
            "gallery/b.jpg",
            "image",
            2,
            Some(47.4),
            Some(8.5),
        )
        .await;

        // Warm but stale cache: the (now outdated) cached summary is served immediately rather
        // than blocking this request on a recompute, while a background refresh is kicked off.
        let second = store
            .query_gallery_map_clusters(&query)
            .await
            .unwrap()
            .expect("gallery map clusters should be available");
        assert_eq!(second.total_entry_count, 1);
        assert!(second.summary_status.refreshing);
        // The viewport-bounded clusters themselves are never served from the summary cache.
        assert_eq!(second.visible_geotagged_count, 2);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let refreshed = store
                .query_gallery_map_clusters(&query)
                .await
                .unwrap()
                .expect("gallery map clusters should be available");
            if refreshed.total_entry_count == 2 && !refreshed.summary_status.refreshing {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "background gallery map summary refresh did not complete in time"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        drop(store);
        let _ = std::fs::remove_file(metadata_db_path);
    }

    #[tokio::test]
    async fn gallery_map_cluster_entries_are_paginated_in_turso() {
        let metadata_db_path = turso_test_db_path("gallery-map-entry-pages");
        let database = turso::Builder::new_local(&metadata_db_path.to_string_lossy())
            .build()
            .await
            .expect("turso should open");
        let connection = database.connect().expect("turso should connect");
        super::super::init_metadata_db(&connection)
            .await
            .expect("metadata schema should initialize");
        insert_gallery_fixture(
            &connection,
            "gallery/older.jpg",
            "image",
            10,
            Some(47.4),
            Some(8.5),
        )
        .await;
        insert_gallery_fixture(
            &connection,
            "gallery/newer.jpg",
            "image",
            20,
            Some(47.4),
            Some(8.5),
        )
        .await;
        let viewport = GalleryViewportBounds {
            south: 45.0,
            west: 5.0,
            north: 49.0,
            east: 11.0,
        };
        let history_id = current_gallery_history_id(&connection).await.unwrap();
        let revision = current_gallery_revision(&connection).await.unwrap();
        let clusters = query_gallery_map_clusters(
            &connection,
            &gallery_map_query(viewport, 1024, 512),
            history_id.clone(),
            revision,
        )
        .await
        .expect("gallery clusters should load");
        assert_eq!(clusters.clusters.len(), 1);
        let cluster = &clusters.clusters[0];
        assert_eq!(cluster.count, 2);

        let entries = query_gallery_map_cluster_entries(
            &connection,
            &GalleryMapClusterEntriesQuery {
                prefix: "gallery".to_string(),
                depth: 64,
                media_filter: GalleryIndexMediaFilter::Image,
                captured_from_unix: None,
                captured_until_unix: None,
                viewport,
                resolution: clusters.resolution,
                cell_x: cluster.cell_x,
                cell_y: cluster.cell_y,
                offset: 0,
                limit: 1,
                label_filter: Default::default(),
            },
            history_id,
            revision,
        )
        .await
        .expect("cluster entries should load");
        assert_eq!(entries.total_entry_count, 2);
        assert_eq!(entries.entries.len(), 1);
        assert_eq!(entries.entries[0].key, "gallery/newer.jpg");

        drop(connection);
        drop(database);
        let _ = std::fs::remove_file(metadata_db_path);
    }

    #[tokio::test]
    async fn gallery_spatial_backfill_restores_missing_positions_in_turso() {
        let metadata_db_path = turso_test_db_path("gallery-spatial-backfill");
        let store = TursoMetadataStore::open(&metadata_db_path)
            .await
            .expect("turso metadata store should open");
        insert_gallery_fixture(
            &store.connection,
            "gallery/missing-position.jpg",
            "image",
            1,
            Some(47.4),
            Some(8.5),
        )
        .await;
        store
            .connection
            .execute(
                "UPDATE gallery_objects SET spatial_x = NULL, spatial_y = NULL WHERE key = ?1",
                ("gallery/missing-position.jpg",),
            )
            .await
            .expect("gallery fixture position should clear");

        backfill_gallery_spatial_positions(&store.connection)
            .await
            .expect("spatial positions should backfill");
        let mut rows = store
            .connection
            .query(
                "SELECT spatial_x, spatial_y FROM gallery_objects WHERE key = ?1",
                ("gallery/missing-position.jpg",),
            )
            .await
            .expect("backfilled gallery fixture should query");
        let row = rows
            .next()
            .await
            .expect("backfilled gallery fixture should load")
            .expect("backfilled gallery fixture should exist");
        assert!(
            row_opt_f64(&row, 0, "gallery_objects.spatial_x")
                .expect("spatial x should decode")
                .is_some()
        );
        assert!(
            row_opt_f64(&row, 1, "gallery_objects.spatial_y")
                .expect("spatial y should decode")
                .is_some()
        );

        drop(rows);
        drop(store);
        let _ = std::fs::remove_file(metadata_db_path);
    }

    #[tokio::test]
    async fn gallery_map_cluster_tokens_ignore_changes_outside_their_scope_in_turso() {
        let metadata_db_path = turso_test_db_path("gallery-map-token-scope");
        let store = TursoMetadataStore::open(&metadata_db_path)
            .await
            .expect("turso metadata store should open");
        insert_gallery_fixture(
            &store.connection,
            "gallery/a.jpg",
            "image",
            10,
            Some(47.4),
            Some(8.5),
        )
        .await;
        insert_gallery_fixture(
            &store.connection,
            "gallery/b.jpg",
            "image",
            20,
            Some(47.4),
            Some(8.5),
        )
        .await;

        let viewport = GalleryViewportBounds {
            south: 45.0,
            west: 5.0,
            north: 49.0,
            east: 11.0,
        };
        let clusters = store
            .query_gallery_map_clusters(&gallery_map_query(viewport, 1024, 512))
            .await
            .expect("gallery map clusters should load")
            .expect("turso should support gallery map clusters");
        let cluster = clusters
            .clusters
            .first()
            .expect("fixture should produce one map cluster");

        insert_gallery_fixture(
            &store.connection,
            "elsewhere/noise.jpg",
            "image",
            30,
            Some(40.7),
            Some(-74.0),
        )
        .await;

        let page = store
            .query_gallery_map_cluster_entries(&GalleryMapClusterEntriesQuery {
                prefix: "gallery".to_string(),
                depth: 64,
                media_filter: GalleryIndexMediaFilter::Image,
                captured_from_unix: None,
                captured_until_unix: None,
                viewport,
                resolution: clusters.resolution,
                cell_x: cluster.cell_x,
                cell_y: cluster.cell_y,
                offset: 0,
                limit: 100,
                label_filter: Default::default(),
            })
            .await
            .expect("cluster page should load after unrelated ingest")
            .expect("turso should support cluster pages");
        assert_eq!(page.history_id, clusters.history_id);
        assert_eq!(page.revision, clusters.revision);

        store
            .connection
            .execute(
                "UPDATE gallery_objects SET captured_at_unix = 40 WHERE key = 'gallery/a.jpg'",
                (),
            )
            .await
            .expect("in-scope fixture should update");
        let changed_page = store
            .query_gallery_map_cluster_entries(&GalleryMapClusterEntriesQuery {
                prefix: "gallery".to_string(),
                depth: 64,
                media_filter: GalleryIndexMediaFilter::Image,
                captured_from_unix: None,
                captured_until_unix: None,
                viewport,
                resolution: clusters.resolution,
                cell_x: cluster.cell_x,
                cell_y: cluster.cell_y,
                offset: 0,
                limit: 100,
                label_filter: Default::default(),
            })
            .await
            .expect("cluster page should load after in-scope change")
            .expect("turso should support cluster pages");
        assert_ne!(changed_page.revision, clusters.revision);

        drop(store);
        let _ = std::fs::remove_file(metadata_db_path);
    }

    #[tokio::test]
    async fn gallery_revision_and_delta_survive_turso_restart_and_track_removals() {
        let metadata_db_path = turso_test_db_path("gallery-delta-restart");
        let history_id;
        {
            let database = turso::Builder::new_local(&metadata_db_path.to_string_lossy())
                .build()
                .await
                .expect("turso should open");
            let connection = database.connect().expect("turso should connect");
            super::super::init_metadata_db(&connection)
                .await
                .expect("metadata schema should initialize");
            history_id = current_gallery_history_id(&connection).await.unwrap();
            insert_gallery_fixture(&connection, "gallery/cat.jpg", "image", 10, None, None).await;
            assert_eq!(current_gallery_revision(&connection).await.unwrap(), 1);
        }

        let database = turso::Builder::new_local(&metadata_db_path.to_string_lossy())
            .build()
            .await
            .expect("turso should reopen");
        let connection = database.connect().expect("turso should reconnect");
        super::super::init_metadata_db(&connection)
            .await
            .expect("metadata schema should reopen");
        assert_eq!(
            current_gallery_history_id(&connection).await.unwrap(),
            history_id
        );
        assert_eq!(current_gallery_revision(&connection).await.unwrap(), 1);

        connection
            .execute(
                "UPDATE gallery_objects
                 SET captured_at_unix = 20, geotagged = 1, latitude = 47.4, longitude = 8.5
                 WHERE key = 'gallery/cat.jpg'",
                (),
            )
            .await
            .unwrap();
        let scope = gallery_delta_scope();
        let page = query_gallery_delta(&connection, &history_id, 1, 10, &scope)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(page.next_revision, 2);
        assert_eq!(page.changes.len(), 1);
        assert_eq!(page.changes[0].kind, GalleryDeltaKind::Upsert);
        assert_eq!(page.changes[0].key, "gallery/cat.jpg");

        connection
            .execute(
                "DELETE FROM gallery_objects WHERE key = 'gallery/cat.jpg'",
                (),
            )
            .await
            .unwrap();
        let removal = query_gallery_delta(&connection, &history_id, 2, 10, &scope)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(removal.next_revision, 3);
        assert_eq!(removal.changes[0].kind, GalleryDeltaKind::Removal);
        assert!(removal.changes[0].entry.is_none());

        let first_page = query_gallery_delta(&connection, &history_id, 0, 1, &scope)
            .await
            .unwrap()
            .unwrap();
        assert!(first_page.has_more);
        assert_eq!(first_page.next_revision, 1);
        assert!(first_page.changes.is_empty());
        let second_page = query_gallery_delta(
            &connection,
            &history_id,
            first_page.next_revision,
            1,
            &scope,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(second_page.has_more);
        assert_eq!(second_page.next_revision, 2);
        assert_eq!(second_page.changes[0].kind, GalleryDeltaKind::Removal);
        let final_page = query_gallery_delta(
            &connection,
            &history_id,
            second_page.next_revision,
            1,
            &scope,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!final_page.has_more);
        assert_eq!(final_page.next_revision, 3);
        assert_eq!(final_page.changes[0].kind, GalleryDeltaKind::Removal);

        let ahead = query_gallery_delta(&connection, &history_id, 4, 10, &scope)
            .await
            .unwrap();
        assert!(matches!(
            ahead,
            Err(GalleryDeltaCursorError::Ahead {
                current_revision: 3,
                ..
            })
        ));
        let foreign = query_gallery_delta(&connection, &Uuid::new_v4().to_string(), 3, 10, &scope)
            .await
            .unwrap();
        assert!(matches!(
            foreign,
            Err(GalleryDeltaCursorError::HistoryMismatch {
                current_revision: 3,
                ..
            })
        ));
        connection
            .execute("DELETE FROM gallery_changes WHERE revision < 3", ())
            .await
            .unwrap();
        let expired = query_gallery_delta(&connection, &history_id, 0, 10, &scope)
            .await
            .unwrap();
        assert!(matches!(
            expired,
            Err(GalleryDeltaCursorError::Expired {
                current_revision: 3,
                ..
            })
        ));

        drop(connection);
        drop(database);
        let _ = std::fs::remove_file(metadata_db_path);
    }

    #[tokio::test]
    async fn gallery_backfill_migrates_zero_capture_time_to_version_creation() {
        let metadata_db_path = turso_test_db_path("gallery-capture-fallback-backfill");
        let store = TursoMetadataStore::open(&metadata_db_path)
            .await
            .expect("turso metadata store should open");
        let version_index_payload = serde_json::to_vec(&serde_json::json!({
            "object_id": "object-pending",
            "versions": {
                "version-pending": {
                    "version_id": "version-pending",
                    "object_id": "object-pending",
                    "manifest_hash": "manifest-pending",
                    "logical_path": "gallery/pending.jpg",
                    "parent_version_ids": [],
                    "state": "confirmed",
                    "created_at_unix": 1_800_000_000u64,
                    "copied_from_object_id": null,
                    "copied_from_version_id": null,
                    "copied_from_path": null
                }
            },
            "head_version_ids": ["version-pending"],
            "preferred_head_version_id": "version-pending"
        }))
        .unwrap();
        store
            .connection
            .execute_batch(
                "INSERT INTO current_objects (key, manifest_hash, object_id)
                 VALUES ('gallery/pending.jpg', 'manifest-pending', 'object-pending');
                 INSERT INTO gallery_objects (
                     key, manifest_hash, object_id, inferred_media_type, media_type,
                     captured_at_unix, media_status, geotagged, latitude, longitude
                 ) VALUES (
                     'gallery/pending.jpg', 'manifest-pending', 'object-pending', 'image', 'image',
                     0, NULL, 0, NULL, NULL
                 );
                 DELETE FROM metadata_meta WHERE key = 'gallery_capture_fallback_v1';",
            )
            .await
            .expect("legacy gallery projection should persist");
        store
            .connection
            .execute(
                "INSERT INTO version_indexes (object_id, index_json) VALUES (?1, ?2)",
                ("object-pending", version_index_payload),
            )
            .await
            .expect("version index should persist");
        drop(store);

        let reopened = TursoMetadataStore::open(&metadata_db_path)
            .await
            .expect("turso metadata store should reopen");
        let mut rows = reopened
            .connection
            .query(
                "SELECT captured_at_unix FROM gallery_objects WHERE key = 'gallery/pending.jpg'",
                (),
            )
            .await
            .expect("backfilled gallery capture time should query");
        let row = rows.next().await.unwrap().unwrap();
        assert_eq!(
            row_u64(&row, 0, "gallery_objects.captured_at_unix").unwrap(),
            1_800_000_000
        );
        drop(rows);

        drop(reopened);
        let _ = std::fs::remove_file(metadata_db_path);
    }

    #[tokio::test]
    async fn gallery_backfill_skips_current_entries_with_a_fresh_projection() {
        let metadata_db_path = turso_test_db_path("gallery-backfill-stale-filter");
        let store = TursoMetadataStore::open(&metadata_db_path)
            .await
            .expect("turso metadata store should open");
        let entry = CurrentObjectEntry {
            manifest_hash: "manifest-fresh".to_string(),
            object_id: "object-fresh".to_string(),
        };
        store
            .upsert_current_object_with_gallery("gallery/fresh.jpg", &entry)
            .await
            .expect("current object should persist");
        store
            .connection
            .execute_batch(
                "
                CREATE TABLE gallery_backfill_updates (marker INTEGER NOT NULL);
                CREATE TRIGGER gallery_backfill_updates_marker
                AFTER UPDATE ON gallery_objects
                BEGIN
                    INSERT INTO gallery_backfill_updates(marker) VALUES(1);
                END;
                ",
            )
            .await
            .expect("backfill marker should initialize");

        store
            .backfill_gallery_objects()
            .await
            .expect("fresh projection backfill should succeed");
        let mut rows = store
            .connection
            .query("SELECT COUNT(*) FROM gallery_backfill_updates", ())
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        assert_eq!(
            row_u64(&row, 0, "gallery_backfill_updates.count").unwrap(),
            0
        );
        drop(rows);

        store
            .connection
            .execute(
                "UPDATE gallery_objects
                 SET geotagged = 1, latitude = NULL, longitude = NULL
                 WHERE key = 'gallery/fresh.jpg'",
                (),
            )
            .await
            .unwrap();
        store
            .connection
            .execute("DELETE FROM gallery_backfill_updates", ())
            .await
            .unwrap();
        store
            .backfill_gallery_objects()
            .await
            .expect("stale projection backfill should succeed");
        let mut rows = store
            .connection
            .query("SELECT COUNT(*) FROM gallery_backfill_updates", ())
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        assert_eq!(
            row_u64(&row, 0, "gallery_backfill_updates.count").unwrap(),
            1
        );

        drop(store);
        let _ = std::fs::remove_file(metadata_db_path);
    }

    #[tokio::test]
    async fn gallery_queries_use_dedicated_turso_read_connections() {
        let metadata_db_path = turso_test_db_path("gallery-read-connection-pool");
        let store = TursoMetadataStore::open(&metadata_db_path)
            .await
            .expect("turso metadata store should open");
        store
            .connection
            .execute_batch("BEGIN")
            .await
            .expect("primary Turso connection transaction should begin");

        let query = GalleryIndexQuery {
            prefix: "gallery".to_string(),
            depth: 64,
            media_filter: GalleryIndexMediaFilter::All,
            captured_sort: GalleryIndexCapturedSort::Desc,
            captured_from_unix: None,
            captured_until_unix: None,
            offset: 0,
            limit: 10,
            viewport: None,
            label_filter: Default::default(),
        };
        let (first, second) = tokio::join!(
            store.query_turso_gallery_index(&query),
            store.query_turso_gallery_index(&query),
        );
        assert_eq!(
            first
                .expect("first gallery query should not share the primary connection transaction")
                .total_entry_count,
            0
        );
        assert_eq!(
            second
                .expect("second gallery query should not share the primary connection transaction")
                .total_entry_count,
            0
        );

        store
            .connection
            .execute_batch("ROLLBACK")
            .await
            .expect("primary Turso connection transaction should roll back");
        drop(store);
        let _ = std::fs::remove_file(metadata_db_path);
    }

    /// Turso 0.6 connections break after a handful of `BEGIN`/`COMMIT` cycles ("cannot commit -
    /// no transaction is active"), which is why gallery reads open a fresh connection per
    /// transaction instead of reusing pooled ones. More iterations than the read concurrency
    /// limit, so this also covers permits being released rather than leaked.
    #[tokio::test]
    async fn repeated_gallery_reads_survive_more_transactions_than_the_concurrency_limit() {
        let metadata_db_path = turso_test_db_path("gallery-repeated-read-transactions");
        let store = TursoMetadataStore::open(&metadata_db_path)
            .await
            .expect("turso metadata store should open");
        let query = GalleryIndexQuery {
            prefix: "gallery".to_string(),
            depth: 64,
            media_filter: GalleryIndexMediaFilter::All,
            captured_sort: GalleryIndexCapturedSort::Desc,
            captured_from_unix: None,
            captured_until_unix: None,
            offset: 0,
            limit: 10,
            viewport: None,
            label_filter: Default::default(),
        };
        for iteration in 0..16 {
            store
                .query_turso_gallery_index(&query)
                .await
                .unwrap_or_else(|error| {
                    panic!("gallery read {iteration} should succeed, got: {error}")
                });
        }
        drop(store);
        let _ = std::fs::remove_file(metadata_db_path);
    }

    #[tokio::test]
    async fn gallery_writes_are_serialized_on_the_primary_turso_connection() {
        let metadata_db_path = turso_test_db_path("gallery-primary-writer-lock");
        let store = TursoMetadataStore::open(&metadata_db_path)
            .await
            .expect("turso metadata store should open");
        let first_entry = CurrentObjectEntry {
            manifest_hash: "manifest-first".to_string(),
            object_id: "object-first".to_string(),
        };
        let second_entry = CurrentObjectEntry {
            manifest_hash: "manifest-second".to_string(),
            object_id: "object-second".to_string(),
        };

        let (first, second) = tokio::join!(
            store.upsert_current_object_with_gallery("gallery/first.jpg", &first_entry),
            store.upsert_current_object_with_gallery("gallery/second.jpg", &second_entry),
        );
        first.expect("first gallery write should commit");
        second.expect("second gallery write should commit");
        let mut rows = store
            .connection
            .query("SELECT COUNT(*) FROM current_objects", ())
            .await
            .expect("current object count should query");
        let row = rows
            .next()
            .await
            .expect("current object count should load")
            .expect("current object count should contain a row");
        assert_eq!(
            row_u64(&row, 0, "current_objects.count").unwrap(),
            2,
            "both writes should remain committed"
        );

        drop(store);
        let _ = std::fs::remove_file(metadata_db_path);
    }

    #[tokio::test]
    async fn gallery_transaction_helpers_roll_back_errors() {
        let metadata_db_path = turso_test_db_path("gallery-transaction-rollback");
        let database = turso::Builder::new_local(&metadata_db_path.to_string_lossy())
            .build()
            .await
            .expect("Turso test database should open");
        let connection = database
            .connect()
            .expect("Turso test database should connect");
        super::super::init_metadata_db(&connection)
            .await
            .expect("metadata schema should initialize");

        let write_transaction =
            Transaction::new_unchecked(&connection, TransactionBehavior::Immediate)
                .await
                .expect("gallery write transaction should begin");
        write_transaction
            .execute(
                "INSERT INTO current_objects (key, manifest_hash, object_id)
                 VALUES (?1, ?2, ?3)",
                (
                    "gallery/rollback.jpg",
                    "manifest-rollback",
                    "object-rollback",
                ),
            )
            .await
            .expect("transactional fixture should insert");
        let write_error = finish_gallery_transaction::<()>(
            write_transaction,
            Err(anyhow::anyhow!("forced gallery write failure")),
        )
        .await
        .expect_err("gallery write error should propagate");
        assert!(
            write_error
                .to_string()
                .contains("forced gallery write failure")
        );

        let mut rows = connection
            .query(
                "SELECT COUNT(*) FROM current_objects WHERE key = ?1",
                ("gallery/rollback.jpg",),
            )
            .await
            .expect("rolled back row count should query");
        let row = rows
            .next()
            .await
            .expect("rolled back row count should load")
            .expect("rolled back row count should contain a row");
        assert_eq!(row_u64(&row, 0, "current_objects.count").unwrap(), 0);
        drop(rows);

        let read_transaction =
            Transaction::new_unchecked(&connection, TransactionBehavior::Deferred)
                .await
                .expect("gallery read transaction should begin");
        let read_error = finish_gallery_read_transaction::<()>(
            read_transaction,
            Err(anyhow::anyhow!("forced gallery read failure")),
        )
        .await
        .expect_err("gallery read error should propagate");
        assert!(
            read_error
                .to_string()
                .contains("forced gallery read failure")
        );
        connection
            .query("SELECT 1", ())
            .await
            .expect("connection should remain usable after read rollback");

        drop(connection);
        drop(database);
        let _ = std::fs::remove_file(metadata_db_path);
    }

    #[tokio::test]
    async fn gallery_writes_share_the_turso_writer_with_other_transactions() {
        let metadata_db_path = turso_test_db_path("gallery-shared-writer-lock");
        let store = TursoMetadataStore::open(&metadata_db_path)
            .await
            .expect("turso metadata store should open");
        let entry = CurrentObjectEntry {
            manifest_hash: "manifest-shared-writer".to_string(),
            object_id: "object-shared-writer".to_string(),
        };
        let attempts = std::collections::HashMap::from([(
            "gallery/shared-writer.jpg".to_string(),
            RepairAttemptRecord {
                attempts: 1,
                last_failure_unix: 2,
            },
        )]);
        let credentials = ClientCredentialState::default();

        let (gallery, repair, credentials) = tokio::join!(
            store.upsert_current_object_with_gallery("gallery/shared-writer.jpg", &entry),
            store.persist_repair_attempts(&attempts),
            store.persist_client_credential_state(&credentials),
        );
        gallery.expect("gallery write should commit");
        repair.expect("repair attempt write should commit");
        credentials.expect("client credential write should commit");

        drop(store);
        let _ = std::fs::remove_file(metadata_db_path);
    }

    #[tokio::test]
    async fn gallery_media_delta_tracks_each_shared_content_fingerprint_entry() {
        let metadata_db_path = turso_test_db_path("gallery-media-delta-shared-fingerprint");
        let store = TursoMetadataStore::open(&metadata_db_path)
            .await
            .expect("turso metadata store should open");
        store
            .connection
            .execute_batch(
                "
                INSERT INTO manifest_summaries (manifest_hash, total_size_bytes, content_fingerprint)
                VALUES
                    ('manifest-a', 100, 'fingerprint-shared'),
                    ('manifest-b', 100, 'fingerprint-shared');
                INSERT INTO gallery_objects (
                    key, manifest_hash, object_id, inferred_media_type, media_type,
                    captured_at_unix, media_status, geotagged, latitude, longitude
                ) VALUES
                    ('gallery/a.jpg', 'manifest-a', 'object-a', 'image', 'image', 10, 'ready', 0, NULL, NULL),
                    ('gallery/b.jpg', 'manifest-b', 'object-b', 'image', 'image', 10, 'ready', 0, NULL, NULL);
                ",
            )
            .await
            .expect("gallery fixtures should persist");

        let media_metadata = |width| CachedMediaMetadata {
            schema_version: crate::storage::media_cache::MEDIA_CACHE_SCHEMA_VERSION,
            content_fingerprint: "fingerprint-shared".to_string(),
            source_manifest_hash: "manifest-a".to_string(),
            status: MediaCacheStatus::Ready,
            media_type: Some("image".to_string()),
            mime_type: Some("image/jpeg".to_string()),
            width: Some(width),
            height: Some(48),
            orientation: Some(1),
            taken_at_unix: Some(10),
            taken_at_timezone_known: Some(true),
            date_encoded_unix: None,
            duration_millis: None,
            frame_rate_millihertz: None,
            total_bitrate_bps: None,
            codec_name: None,
            codec_fourcc: None,
            gps: None,
            photo: None,
            thumbnail: None,
            source_size_bytes: 100,
            generated_at_unix: 2,
            retry_after_unix: None,
            error: None,
        };
        store
            .persist_media_cache_record_with_gallery(&media_metadata(64))
            .await
            .expect("initial media metadata should persist");
        store
            .connection
            .execute(
                "UPDATE gallery_objects SET captured_at_unix = 0 WHERE key = 'gallery/a.jpg'",
                (),
            )
            .await
            .expect("first gallery entry should become stale");
        let history_id = current_gallery_history_id(&store.connection)
            .await
            .expect("gallery history should load");
        let revision = current_gallery_revision(&store.connection)
            .await
            .expect("gallery revision should load");

        store
            .persist_media_cache_record_with_gallery(&media_metadata(128))
            .await
            .expect("changed media metadata should persist");
        let page = store
            .query_turso_gallery_delta(&history_id, revision, 10, &gallery_delta_scope())
            .await
            .expect("gallery delta should load")
            .expect("gallery token should remain current");
        assert_eq!(page.changes.len(), 2);
        let mut changed_keys = page
            .changes
            .iter()
            .map(|change| change.key.as_str())
            .collect::<Vec<_>>();
        changed_keys.sort_unstable();
        assert_eq!(changed_keys, ["gallery/a.jpg", "gallery/b.jpg"]);
        assert!(page.changes.iter().all(|change| {
            change.kind == GalleryDeltaKind::Upsert
                && change
                    .entry
                    .as_ref()
                    .and_then(|entry| entry.media_metadata.as_ref())
                    .and_then(|metadata| metadata.width)
                    == Some(128)
        }));

        drop(store);
        let _ = std::fs::remove_file(metadata_db_path);
    }

    /// Labels live outside the content-addressed media bytes, so re-uploading an
    /// image must not discard them. The upsert deliberately omits `labels_json`
    /// from its conflict clause; this pins that behaviour down.
    #[tokio::test]
    async fn gallery_labels_survive_a_projection_refresh() {
        let metadata_db_path = turso_test_db_path("gallery-labels-refresh");
        let store = TursoMetadataStore::open(&metadata_db_path)
            .await
            .expect("turso metadata store should open");
        let key = "gallery/labelled.jpg";
        store
            .upsert_current_object_with_gallery(
                key,
                &CurrentObjectEntry {
                    manifest_hash: "manifest-first".to_string(),
                    object_id: "object-first".to_string(),
                },
            )
            .await
            .expect("current object should persist");
        store
            .store_gallery_object_labels(key, &["private".to_string()])
            .await
            .expect("labels should persist");

        store
            .upsert_current_object_with_gallery(
                key,
                &CurrentObjectEntry {
                    manifest_hash: "manifest-second".to_string(),
                    object_id: "object-first".to_string(),
                },
            )
            .await
            .expect("re-uploaded object should persist");

        let mut rows = store
            .connection
            .query(
                "SELECT labels_json FROM gallery_objects WHERE key = ?1",
                (key,),
            )
            .await
            .expect("labels should be queryable");
        let row = rows.next().await.unwrap().expect("row should exist");
        assert_eq!(
            row_string(&row, 0, "gallery_objects.labels_json").unwrap(),
            "[\"private\"]"
        );

        drop(store);
        let _ = std::fs::remove_file(metadata_db_path);
    }

    /// Databases created before the label column exists must gain it through the
    /// additive migration rather than needing a projection rebuild.
    #[tokio::test]
    async fn gallery_labels_column_is_added_to_preexisting_databases() {
        let metadata_db_path = turso_test_db_path("gallery-labels-migration");
        let database = turso::Builder::new_local(&metadata_db_path.to_string_lossy())
            .build()
            .await
            .expect("turso should open");
        let connection = database.connect().expect("turso should connect");
        connection
            .execute(
                "CREATE TABLE gallery_objects (
                     key TEXT PRIMARY KEY,
                     manifest_hash TEXT NOT NULL,
                     object_id TEXT NOT NULL,
                     inferred_media_type TEXT,
                     media_type TEXT,
                     captured_at_unix INTEGER NOT NULL DEFAULT 0,
                     media_status TEXT,
                     geotagged INTEGER NOT NULL DEFAULT 0,
                     latitude REAL,
                     longitude REAL
                 )",
                (),
            )
            .await
            .expect("legacy projection table should be created");

        super::super::init_metadata_db(&connection)
            .await
            .expect("metadata schema should migrate");

        insert_gallery_fixture(&connection, "gallery/legacy.jpg", "image", 10, None, None).await;
        let mut rows = connection
            .query(
                "SELECT labels_json FROM gallery_objects WHERE key = ?1",
                ("gallery/legacy.jpg",),
            )
            .await
            .expect("migrated column should be queryable");
        let row = rows.next().await.unwrap().expect("row should exist");
        assert_eq!(
            row_string(&row, 0, "gallery_objects.labels_json").unwrap(),
            "[]",
            "migrated rows should default to no labels"
        );

        drop(rows);
        drop(connection);
        let _ = std::fs::remove_file(metadata_db_path);
    }
}
