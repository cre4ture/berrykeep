use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use turso::transaction::{DropBehavior, Transaction, TransactionBehavior};
use turso::{Value, params_from_iter};
use uuid::Uuid;

use super::super::{
    CachedMediaMetadata, CurrentObjectEntry, FileVersionIndex,
    GALLERY_CAPTURE_FALLBACK_BACKFILL_KEY, GalleryDeltaChange, GalleryDeltaCursorError,
    GalleryDeltaKind, GalleryDeltaPage, GalleryDeltaScope, GalleryIndexCapturedSort,
    GalleryIndexEntry, GalleryIndexMediaFilter, GalleryIndexMediaSummary, GalleryIndexPage,
    GalleryIndexQuery, ManifestSummary, current_media_cache_metadata,
    effective_gallery_captured_at_unix, gallery_index_media_status,
    gallery_index_media_type_from_metadata, gallery_media_type_for_path,
    sqlite_like_prefix_pattern, version_created_at_unix_from_payload,
};
use super::{TursoMetadataStore, row_string, row_u64};

const GALLERY_CHANGE_LOG_RETENTION: u64 = 100_000;

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
                longitude REAL
            );

            CREATE TABLE IF NOT EXISTS gallery_changes (
                revision INTEGER PRIMARY KEY,
                key TEXT NOT NULL,
                change_kind TEXT NOT NULL CHECK(change_kind IN ('upsert', 'removal')),
                previous_inferred_media_type TEXT,
                previous_media_type TEXT,
                previous_latitude REAL,
                previous_longitude REAL
            );

            CREATE INDEX IF NOT EXISTS idx_gallery_objects_media_order
                ON gallery_objects(media_type, captured_at_unix DESC, key ASC);
            CREATE INDEX IF NOT EXISTS idx_gallery_objects_manifest_hash
                ON gallery_objects(manifest_hash);
            CREATE INDEX IF NOT EXISTS idx_gallery_objects_viewport
                ON gallery_objects(latitude, longitude, media_type, captured_at_unix DESC, key ASC);
            CREATE INDEX IF NOT EXISTS idx_manifest_summaries_content_fingerprint
                ON manifest_summaries(content_fingerprint);
            ",
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
                    previous_longitude
                )
                SELECT
                    CAST(value AS INTEGER),
                    NEW.key,
                    'upsert',
                    OLD.inferred_media_type,
                    OLD.media_type,
                    OLD.latitude,
                    OLD.longitude
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
                    previous_longitude
                )
                SELECT
                    CAST(value AS INTEGER),
                    OLD.key,
                    'removal',
                    OLD.inferred_media_type,
                    OLD.media_type,
                    OLD.latitude,
                    OLD.longitude
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
        let connection = self.gallery_read_connection().await;
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

    pub(super) async fn query_turso_gallery_delta(
        &self,
        history_id: &str,
        since_revision: u64,
        limit: usize,
        scope: &GalleryDeltaScope,
    ) -> Result<std::result::Result<GalleryDeltaPage, GalleryDeltaCursorError>> {
        let connection = self.gallery_read_connection().await;
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
                 captured_at_unix, media_status, geotagged, latitude, longitude
             ) VALUES (?1, ?2, ?3, ?4, ?4, 0, NULL, 0, NULL, NULL)
             ON CONFLICT(key) DO UPDATE SET
                 manifest_hash = excluded.manifest_hash,
                 object_id = excluded.object_id,
                 inferred_media_type = excluded.inferred_media_type,
                 media_type = excluded.inferred_media_type,
                 captured_at_unix = 0,
                 media_status = NULL,
                 geotagged = 0,
                 latitude = NULL,
                 longitude = NULL
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
            "SELECT gallery_objects.key, version_indexes.index_json
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
        ));
    }
    drop(rows);

    for (key, version_index_payload) in entries {
        let version_created_at_unix =
            version_created_at_unix_from_payload(version_index_payload.as_deref(), manifest_hash)?;
        let captured_at_unix = effective_gallery_captured_at_unix(
            &key,
            metadata.is_some(),
            metadata
                .as_ref()
                .and_then(|metadata| metadata.taken_at_unix),
            version_created_at_unix,
        );
        connection
            .execute(
                "UPDATE gallery_objects
                 SET media_type = COALESCE(?1, inferred_media_type),
                     captured_at_unix = ?2,
                     media_status = ?3,
                     geotagged = ?4,
                     latitude = ?5,
                     longitude = ?6
                 WHERE key = ?7",
                params_from_iter(vec![
                    optional_text_value(media_type),
                    Value::from(
                        i64::try_from(captured_at_unix).context("gallery capture time overflow")?,
                    ),
                    optional_text_value(media_status),
                    Value::from(i64::from(gps.is_some())),
                    gps.map(|gps| Value::from(gps.latitude))
                        .unwrap_or(Value::Null),
                    gps.map(|gps| Value::from(gps.longitude))
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
        )";
    let params = gallery_scope_values(&prefix, &prefix_pattern, depth, query);
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
         WHERE {scope}"
    );
    let mut summary_rows = connection
        .query(summary_sql, params_from_iter(params.clone()))
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
    let page_sql = format!(
        "SELECT
             gallery_objects.key,
             gallery_objects.manifest_hash,
             manifest_summaries.total_size_bytes,
             manifest_summaries.content_fingerprint,
             media_cache.metadata_json,
             version_indexes.index_json
         FROM gallery_objects
         LEFT JOIN manifest_summaries
           ON manifest_summaries.manifest_hash = gallery_objects.manifest_hash
         LEFT JOIN media_cache
           ON media_cache.content_fingerprint = manifest_summaries.content_fingerprint
         LEFT JOIN version_indexes
           ON version_indexes.object_id = gallery_objects.object_id
         WHERE {scope}
         ORDER BY gallery_objects.captured_at_unix {sort_direction}, gallery_objects.key ASC
         LIMIT ?9 OFFSET ?10"
    );
    let mut page_params = params;
    page_params.push(Value::from(
        i64::try_from(query.limit).context("gallery index limit overflow")?,
    ));
    page_params.push(Value::from(
        i64::try_from(query.offset).context("gallery index offset overflow")?,
    ));
    let mut rows = connection
        .query(page_sql, params_from_iter(page_params))
        .await?;
    let mut entries = Vec::new();
    while let Some(row) = rows.next().await? {
        entries.push(materialize_gallery_index_entry(
            row_string(&row, 0, "gallery_objects.key")?,
            row_string(&row, 1, "gallery_objects.manifest_hash")?,
            row_opt_i64(&row, 2, "manifest_summaries.total_size_bytes")?,
            row_opt_string(&row, 3, "manifest_summaries.content_fingerprint")?,
            row_opt_blob(&row, 4, "media_cache.metadata_json")?,
            row_opt_blob(&row, 5, "version_indexes.index_json")?,
        )?);
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
                 previous_longitude
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
    let mut rows = connection
        .query(
            "SELECT
                 gallery_objects.key,
                 gallery_objects.manifest_hash,
                 manifest_summaries.total_size_bytes,
                 manifest_summaries.content_fingerprint,
                 media_cache.metadata_json,
                 version_indexes.index_json,
                 gallery_objects.media_type,
                 gallery_objects.latitude,
                 gallery_objects.longitude
             FROM gallery_objects
             LEFT JOIN manifest_summaries
               ON manifest_summaries.manifest_hash = gallery_objects.manifest_hash
             LEFT JOIN media_cache
               ON media_cache.content_fingerprint = manifest_summaries.content_fingerprint
             LEFT JOIN version_indexes
               ON version_indexes.object_id = gallery_objects.object_id
             WHERE gallery_objects.key = ?1
               AND gallery_objects.inferred_media_type IS NOT NULL",
            (key,),
        )
        .await?;
    let Some(row) = rows.next().await? else {
        return Ok(None);
    };
    let key = row_string(&row, 0, "gallery_objects.key")?;
    let media_type = row_opt_string(&row, 6, "gallery_objects.media_type")?;
    let latitude = row_opt_f64(&row, 7, "gallery_objects.latitude")?;
    let longitude = row_opt_f64(&row, 8, "gallery_objects.longitude")?;
    if !gallery_entry_matches_delta_scope(&key, media_type.as_deref(), latitude, longitude, scope) {
        return Ok(None);
    }
    Ok(Some(materialize_gallery_index_entry(
        key,
        row_string(&row, 1, "gallery_objects.manifest_hash")?,
        row_opt_i64(&row, 2, "manifest_summaries.total_size_bytes")?,
        row_opt_string(&row, 3, "manifest_summaries.content_fingerprint")?,
        row_opt_blob(&row, 4, "media_cache.metadata_json")?,
        row_opt_blob(&row, 5, "version_indexes.index_json")?,
    )?))
}

fn gallery_scope_values(
    prefix: &str,
    prefix_pattern: &str,
    depth: i64,
    query: &GalleryIndexQuery,
) -> Vec<Value> {
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
    vec![
        Value::from(prefix),
        Value::from(prefix_pattern),
        Value::from(depth),
        optional_text_value(media_type),
        optional_real_value(south),
        optional_real_value(north),
        optional_real_value(west),
        optional_real_value(east),
    ]
}

fn materialize_gallery_index_entry(
    key: String,
    manifest_hash: String,
    size_bytes: Option<i64>,
    content_fingerprint: Option<String>,
    metadata_payload: Option<Vec<u8>>,
    version_index_payload: Option<Vec<u8>>,
) -> Result<GalleryIndexEntry> {
    let size_bytes = size_bytes
        .map(|value| u64::try_from(value).context("negative gallery entry size in Turso"))
        .transpose()?;
    let media_metadata = metadata_payload
        .and_then(|payload| serde_json::from_slice::<CachedMediaMetadata>(&payload).ok())
        .and_then(|metadata| current_media_cache_metadata(Some(metadata)));
    let modified_at_unix =
        version_created_at_unix_from_payload(version_index_payload.as_deref(), &manifest_hash)?;
    Ok(GalleryIndexEntry {
        key,
        manifest_hash,
        size_bytes,
        modified_at_unix,
        content_fingerprint,
        media_metadata,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{
        ClientCredentialState, MediaCacheStatus, MetadataStore, RepairAttemptRecord,
    };
    use std::time::{SystemTime, UNIX_EPOCH};

    fn turso_test_db_path(name: &str) -> std::path::PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "ironmesh-{name}-{}-{stamp}.turso.db",
            std::process::id()
        ))
    }

    async fn insert_gallery_fixture(
        connection: &turso::Connection,
        key: &str,
        media_type: &str,
        captured_at_unix: u64,
        latitude: Option<f64>,
        longitude: Option<f64>,
    ) {
        connection
            .execute(
                "INSERT INTO gallery_objects (
                     key, manifest_hash, object_id, inferred_media_type, media_type,
                     captured_at_unix, media_status, geotagged, latitude, longitude
                 ) VALUES (?1, ?2, ?3, ?4, ?4, ?5, 'ready', ?6, ?7, ?8)",
                params_from_iter(vec![
                    Value::from(key),
                    Value::from(format!("manifest-{key}")),
                    Value::from(format!("object-{key}")),
                    Value::from(media_type),
                    Value::from(i64::try_from(captured_at_unix).unwrap()),
                    Value::from(i64::from(latitude.is_some())),
                    latitude.map(Value::from).unwrap_or(Value::Null),
                    longitude.map(Value::from).unwrap_or(Value::Null),
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
        }
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
    async fn gallery_queries_use_pooled_turso_read_connections() {
        let metadata_db_path = turso_test_db_path("gallery-read-connection-pool");
        let store = TursoMetadataStore::open(&metadata_db_path)
            .await
            .expect("turso metadata store should open");
        assert_eq!(
            store.gallery_readers.len(),
            super::super::DEFAULT_TURSO_GALLERY_READ_CONNECTION_COUNT
        );
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
            offset: 0,
            limit: 10,
            viewport: None,
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
            gps: None,
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
}
