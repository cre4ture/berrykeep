use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use common::NodeId;
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use tokio_rusqlite::Connection as TokioConnection;
use tracing::warn;
use uuid::Uuid;

use crate::cluster::NodeDescriptor;

use super::{
    ActiveSnapshotBatch, AdminAuditEvent, CachedChunkRecord, CachedMediaMetadata,
    ClientCredentialState, CurrentObjectEntry, CurrentState, DataChangeEvent, DataChangeEventQuery,
    DataScrubRunRecord, FileVersionIndex, GALLERY_CAPTURE_FALLBACK_BACKFILL_KEY,
    GALLERY_LABELS_COLUMN, GALLERY_LABELS_COLUMN_DEFINITION, GalleryDeltaChange,
    GalleryDeltaCursorError, GalleryDeltaKind, GalleryDeltaPage, GalleryDeltaScope,
    GalleryIndexCapturedSort, GalleryIndexEntry, GalleryIndexMediaSummary, GalleryIndexPage,
    GalleryIndexQuery, METADATA_SCHEMA_VERSION_CURRENT, ManifestSummary,
    ManualRepairActionRunRecord, MetadataDbLogicalProgress, MetadataDbLogicalProgressCallback,
    MetadataDbTableLogicalBreakdown, MetadataStore, ObjectVersionMetadataRecord, ReconcileMarker,
    RepairAttemptRecord, RepairRunRecord, S3AccessKeyRecord, S3BucketRecord,
    S3BucketVersioningStatus, S3ControlPlaneState, S3ObjectVersionRecord, SnapshotInfo,
    SnapshotManifest, StorageContentKind, StorageLocationRecord, StorageLocationState,
    StorageStatsSample, StorageStatsState, compress_snapshot_json, current_media_cache_metadata,
    decode_gallery_labels, decompress_snapshot_json, effective_gallery_captured_at_unix,
    encode_gallery_labels, gallery_index_media_status, gallery_index_media_type_from_metadata,
    gallery_label_predicates, gallery_media_type_for_path, metadata_db_logical_summary_query,
    metadata_db_logical_table_specs, sqlite_like_prefix_pattern,
    version_created_at_unix_from_payload,
};

const SQLITE_METADATA_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_SQLITE_READ_CONNECTION_COUNT: usize = 4;
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

pub(super) struct SqliteMetadataStore {
    metadata_db_path: PathBuf,
    writer: TokioConnection,
    readers: Vec<TokioConnection>,
    next_reader: AtomicUsize,
}

impl SqliteMetadataStore {
    pub(super) async fn open(metadata_db_path: &Path) -> Result<Self> {
        let metadata_db_path = metadata_db_path.to_path_buf();
        let writer = open_sqlite_writer_connection(&metadata_db_path).await?;
        let mut readers = Vec::with_capacity(sqlite_read_connection_count());
        for _ in 0..sqlite_read_connection_count() {
            readers.push(open_sqlite_reader_connection(&metadata_db_path).await?);
        }
        let store = Self {
            metadata_db_path,
            writer,
            readers,
            next_reader: AtomicUsize::new(0),
        };
        store.backfill_gallery_objects().await?;
        Ok(store)
    }

    fn read_connection(&self) -> TokioConnection {
        let index = self.next_reader.fetch_add(1, Ordering::Relaxed) % self.readers.len();
        self.readers[index].clone()
    }

    async fn read<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        self.read_connection()
            .call(move |db| Ok(f(db)))
            .await
            .map_err(map_tokio_rusqlite_error)?
    }

    async fn write<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        self.writer
            .call(move |db| Ok(f(db)))
            .await
            .map_err(map_tokio_rusqlite_error)?
    }

    async fn write_tx<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        self.write(move |db| {
            db.execute_batch("BEGIN IMMEDIATE")?;
            match f(db) {
                Ok(value) => {
                    db.execute_batch("COMMIT")?;
                    Ok(value)
                }
                Err(err) => {
                    let _ = db.execute_batch("ROLLBACK");
                    Err(err)
                }
            }
        })
        .await
    }

    fn metadata_conn(&self) -> Result<Connection> {
        let db = Connection::open(&self.metadata_db_path)
            .with_context(|| format!("failed to open {}", self.metadata_db_path.display()))?;
        db.busy_timeout(SQLITE_METADATA_BUSY_TIMEOUT)
            .with_context(|| {
                format!(
                    "failed to configure sqlite busy timeout for {}",
                    self.metadata_db_path.display()
                )
            })?;
        Ok(db)
    }

    async fn in_metadata_tx<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T>,
    {
        let db = self.metadata_conn()?;
        db.execute_batch("BEGIN IMMEDIATE")?;
        match f(&db) {
            Ok(value) => {
                db.execute_batch("COMMIT")?;
                Ok(value)
            }
            Err(err) => {
                let _ = db.execute_batch("ROLLBACK");
                Err(err)
            }
        }
    }

    async fn delete_invalid_media_cache_row_if_payload_matches(
        &self,
        content_fingerprint: String,
        payload: Vec<u8>,
    ) -> Result<bool> {
        self.write_tx(move |db| {
            let deleted = db.execute(
                "DELETE FROM media_cache
                 WHERE content_fingerprint = ?1
                   AND metadata_json = ?2",
                params![content_fingerprint, payload],
            )?;
            if deleted > 0 {
                let unchanged_projection_keys =
                    refresh_gallery_objects_for_content_fingerprint_and_collect_unchanged_keys(
                        db,
                        &content_fingerprint,
                    )?;
                record_gallery_upserts_for_keys(db, &unchanged_projection_keys)?;
            }
            Ok(deleted > 0)
        })
        .await
    }

    async fn backfill_gallery_objects(&self) -> Result<()> {
        self.write_tx(|db| {
            let capture_fallback_backfill_needed = db
                .query_row(
                    "SELECT 1 FROM metadata_meta WHERE key = ?1",
                    params![GALLERY_CAPTURE_FALLBACK_BACKFILL_KEY],
                    |_row| Ok(()),
                )
                .optional()?
                .is_none();
            let mut statement = db.prepare(
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
            )?;
            let rows = statement.query_map(
                params![i64::from(capture_fallback_backfill_needed)],
                |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    CurrentObjectEntry {
                        manifest_hash: row.get(1)?,
                        object_id: row.get(2)?,
                    },
                ))
                },
            )?;
            let stale = rows.collect::<std::result::Result<Vec<_>, _>>()?;
            drop(statement);
            for (key, entry) in stale {
                upsert_gallery_object(db, &key, &entry)?;
            }
            if capture_fallback_backfill_needed {
                db.execute(
                    "INSERT INTO metadata_meta(key, value) VALUES(?1, 'complete')
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    params![GALLERY_CAPTURE_FALLBACK_BACKFILL_KEY],
                )?;
            }
            Ok(())
        })
        .await
    }
}

fn upsert_gallery_object(db: &Connection, key: &str, entry: &CurrentObjectEntry) -> Result<()> {
    let inferred_media_type = gallery_media_type_for_path(key);
    db.execute(
        "INSERT INTO gallery_objects (
             key,
             manifest_hash,
             object_id,
             inferred_media_type,
             media_type,
             captured_at_unix,
             media_status,
             geotagged,
             latitude,
             longitude
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
        params![
            key,
            entry.manifest_hash,
            entry.object_id,
            inferred_media_type,
        ],
    )?;
    refresh_gallery_objects_for_manifest(db, &entry.manifest_hash)
}

fn refresh_gallery_objects_for_manifest(db: &Connection, manifest_hash: &str) -> Result<()> {
    let metadata = db
        .query_row(
            "SELECT media_cache.metadata_json
             FROM manifest_summaries
             LEFT JOIN media_cache
                ON media_cache.content_fingerprint = manifest_summaries.content_fingerprint
             WHERE manifest_summaries.manifest_hash = ?1",
            params![manifest_hash],
            |row| row.get::<_, Option<Vec<u8>>>(0),
        )
        .optional()?
        .flatten()
        .and_then(|payload| serde_json::from_slice::<CachedMediaMetadata>(&payload).ok())
        .and_then(|metadata| current_media_cache_metadata(Some(metadata)));
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
    let mut statement = db.prepare(
        "SELECT gallery_objects.key, version_indexes.index_json
         FROM gallery_objects
         LEFT JOIN version_indexes
           ON version_indexes.object_id = gallery_objects.object_id
         WHERE gallery_objects.manifest_hash = ?1",
    )?;
    let entries = statement
        .query_map(params![manifest_hash], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<Vec<u8>>>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(statement);

    for (key, version_index_payload) in entries {
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
        db.execute(
            "UPDATE gallery_objects
             SET media_type = COALESCE(?1, inferred_media_type),
                 captured_at_unix = ?2,
                 media_status = ?3,
                 geotagged = ?4,
                 latitude = ?5,
                 longitude = ?6
             WHERE key = ?7",
            params![
                media_type,
                u64_to_i64(captured_at_unix)?,
                media_status,
                if gps.is_some() { 1i64 } else { 0i64 },
                gps.map(|gps| gps.latitude),
                gps.map(|gps| gps.longitude),
                key,
            ],
        )?;
    }
    Ok(())
}

fn refresh_gallery_objects_for_content_fingerprint(
    db: &Connection,
    content_fingerprint: &str,
) -> Result<()> {
    let mut statement = db.prepare(
        "SELECT manifest_hash
         FROM manifest_summaries
         WHERE content_fingerprint = ?1",
    )?;
    let manifest_hashes = statement
        .query_map(params![content_fingerprint], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for manifest_hash in manifest_hashes {
        refresh_gallery_objects_for_manifest(db, &manifest_hash)?;
    }
    Ok(())
}

fn refresh_gallery_objects_for_content_fingerprint_and_collect_unchanged_keys(
    db: &Connection,
    content_fingerprint: &str,
) -> Result<Vec<String>> {
    let before = gallery_projection_states_for_content_fingerprint(db, content_fingerprint)?;
    refresh_gallery_objects_for_content_fingerprint(db, content_fingerprint)?;
    let after = gallery_projection_states_for_content_fingerprint(db, content_fingerprint)?;
    Ok(unchanged_gallery_projection_keys(before, after))
}

fn refresh_gallery_objects_for_manifest_and_collect_unchanged_keys(
    db: &Connection,
    manifest_hash: &str,
) -> Result<Vec<String>> {
    let before = gallery_projection_states_for_manifest(db, manifest_hash)?;
    refresh_gallery_objects_for_manifest(db, manifest_hash)?;
    let after = gallery_projection_states_for_manifest(db, manifest_hash)?;
    Ok(unchanged_gallery_projection_keys(before, after))
}

fn refresh_gallery_objects_for_object_id_and_collect_unchanged_keys(
    db: &Connection,
    object_id: &str,
) -> Result<Vec<String>> {
    let before = gallery_projection_states_for_object_id(db, object_id)?;
    let mut statement =
        db.prepare("SELECT DISTINCT manifest_hash FROM gallery_objects WHERE object_id = ?1")?;
    let manifest_hashes = statement
        .query_map(params![object_id], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(statement);
    for manifest_hash in manifest_hashes {
        refresh_gallery_objects_for_manifest(db, &manifest_hash)?;
    }
    let after = gallery_projection_states_for_object_id(db, object_id)?;
    Ok(unchanged_gallery_projection_keys(before, after))
}

fn gallery_projection_states_for_content_fingerprint(
    db: &Connection,
    content_fingerprint: &str,
) -> Result<Vec<GalleryProjectionState>> {
    gallery_projection_states_for_query(
        db,
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
}

fn gallery_projection_states_for_manifest(
    db: &Connection,
    manifest_hash: &str,
) -> Result<Vec<GalleryProjectionState>> {
    gallery_projection_states_for_query(
        db,
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
}

fn gallery_projection_states_for_object_id(
    db: &Connection,
    object_id: &str,
) -> Result<Vec<GalleryProjectionState>> {
    gallery_projection_states_for_query(
        db,
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
}

fn gallery_projection_states_for_query(
    db: &Connection,
    sql: &str,
    parameter: &str,
) -> Result<Vec<GalleryProjectionState>> {
    let mut statement = db.prepare(sql)?;
    let states = statement
        .query_map(params![parameter], |row| {
            Ok(GalleryProjectionState {
                key: row.get(0)?,
                media_type: row.get(1)?,
                captured_at_unix: row.get(2)?,
                media_status: row.get(3)?,
                geotagged: row.get(4)?,
                latitude: row.get(5)?,
                longitude: row.get(6)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
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

fn record_gallery_upserts_for_keys(db: &Connection, keys: &[String]) -> Result<()> {
    for key in keys {
        record_gallery_change(db, key, "upsert")?;
    }
    Ok(())
}

fn record_gallery_change(db: &Connection, key: &str, change_kind: &str) -> Result<()> {
    db.execute(
        "UPDATE metadata_meta
         SET value = CAST(value AS INTEGER) + 1
         WHERE key = 'gallery_revision'",
        [],
    )?;
    let revision = current_gallery_revision_from_db(db)?;
    db.execute(
        "INSERT INTO gallery_changes(revision, key, change_kind) VALUES (?1, ?2, ?3)",
        params![revision, key, change_kind],
    )?;
    db.execute(
        "DELETE FROM gallery_changes WHERE revision <= ?1",
        params![revision.saturating_sub(GALLERY_CHANGE_LOG_RETENTION)],
    )?;
    Ok(())
}

fn query_gallery_index_from_db(
    db: &Connection,
    query: &GalleryIndexQuery,
) -> Result<GalleryIndexPage> {
    let transaction = db.unchecked_transaction()?;
    // Reading the revision first establishes the SQLite snapshot. Any write that
    // commits afterwards is therefore guaranteed to be available from the delta
    // feed rather than being hidden behind a newer token paired with stale rows.
    let history_id = current_gallery_history_id_from_db(&transaction)?;
    let revision = current_gallery_revision_from_db(&transaction)?;
    let page = query_gallery_index_in_transaction(&transaction, query, history_id, revision)?;
    transaction.commit()?;
    Ok(page)
}

/// Binds the shared gallery scope parameters in the order `scope` expects them,
/// so that label predicates can be appended with the placeholders that follow.
fn gallery_scope_values(
    prefix: &str,
    prefix_pattern: &str,
    depth: i64,
    media_type: Option<&str>,
    viewport: (Option<f64>, Option<f64>, Option<f64>, Option<f64>),
) -> Vec<Value> {
    let (south, north, west, east) = viewport;
    let real = |value: Option<f64>| value.map_or(Value::Null, Value::Real);
    vec![
        Value::Text(prefix.to_string()),
        Value::Text(prefix_pattern.to_string()),
        Value::Integer(depth),
        media_type.map_or(Value::Null, |value| Value::Text(value.to_string())),
        real(south),
        real(north),
        real(west),
        real(east),
    ]
}

fn query_gallery_index_in_transaction(
    db: &Connection,
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
    let media_type = query.media_filter.media_type();
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
    let scope_values = gallery_scope_values(
        &prefix,
        &prefix_pattern,
        depth,
        media_type,
        (south, north, west, east),
    );
    let (summary_label_sql, summary_label_values) =
        gallery_label_predicates(&query.label_filter, scope_values.len() + 1)?;
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
    let mut summary_values = scope_values.clone();
    summary_values.extend(summary_label_values.into_iter().map(Value::Text));
    let (total_entry_count, media_summary) =
        db.query_row(&summary_sql, params_from_iter(summary_values), |row| {
            let count = |index| {
                row.get::<_, i64>(index).and_then(|value| {
                    usize::try_from(value).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            index,
                            rusqlite::types::Type::Integer,
                            Box::new(error),
                        )
                    })
                })
            };
            Ok((
                count(0)?,
                GalleryIndexMediaSummary {
                    ready_count: count(1)?,
                    pending_count: count(2)?,
                    incomplete_count: count(3)?,
                    image_count: count(4)?,
                    video_count: count(5)?,
                    geotagged_count: count(6)?,
                },
            ))
        })?;
    let sort_direction = match query.captured_sort {
        GalleryIndexCapturedSort::Asc => "ASC",
        GalleryIndexCapturedSort::Desc => "DESC",
    };
    let limit = i64::try_from(query.limit).context("gallery index limit overflow")?;
    let offset = i64::try_from(query.offset).context("gallery index offset overflow")?;
    // The page statement binds the scope, then LIMIT/OFFSET, so its label
    // placeholders continue after those two.
    let (page_label_sql, page_label_values) =
        gallery_label_predicates(&query.label_filter, scope_values.len() + 3)?;
    let page_sql = format!(
        "SELECT
             gallery_objects.key,
             gallery_objects.manifest_hash,
             manifest_summaries.total_size_bytes,
             manifest_summaries.content_fingerprint,
             media_cache.metadata_json,
             version_indexes.index_json,
             gallery_objects.labels_json
         FROM gallery_objects
         LEFT JOIN manifest_summaries
           ON manifest_summaries.manifest_hash = gallery_objects.manifest_hash
         LEFT JOIN media_cache
           ON media_cache.content_fingerprint = manifest_summaries.content_fingerprint
         LEFT JOIN version_indexes
           ON version_indexes.object_id = gallery_objects.object_id
         WHERE {scope}{page_label_sql}
         ORDER BY gallery_objects.captured_at_unix {sort_direction}, gallery_objects.key ASC
         LIMIT ?9 OFFSET ?10"
    );
    let mut statement = db.prepare(&page_sql)?;
    let mut page_values = scope_values;
    page_values.push(Value::Integer(limit));
    page_values.push(Value::Integer(offset));
    page_values.extend(page_label_values.into_iter().map(Value::Text));
    let rows = statement.query_map(params_from_iter(page_values), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<i64>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<Vec<u8>>>(4)?,
            row.get::<_, Option<Vec<u8>>>(5)?,
            row.get::<_, String>(6)?,
        ))
    })?;
    let mut entries = Vec::new();
    for row in rows {
        let (
            key,
            manifest_hash,
            size_bytes,
            content_fingerprint,
            metadata_payload,
            version_index_payload,
            labels_json,
        ) = row?;
        entries.push(materialize_gallery_index_entry(
            key,
            manifest_hash,
            size_bytes,
            content_fingerprint,
            metadata_payload,
            version_index_payload,
            labels_json,
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

fn materialize_gallery_index_entry(
    key: String,
    manifest_hash: String,
    size_bytes: Option<i64>,
    content_fingerprint: Option<String>,
    metadata_payload: Option<Vec<u8>>,
    version_index_payload: Option<Vec<u8>>,
    labels_json: String,
) -> Result<GalleryIndexEntry> {
    let size_bytes = size_bytes
        .map(|value| u64::try_from(value).context("negative gallery entry size in sqlite"))
        .transpose()?;
    let media_metadata = metadata_payload
        .and_then(|payload| serde_json::from_slice::<CachedMediaMetadata>(&payload).ok())
        .and_then(|metadata| current_media_cache_metadata(Some(metadata)));
    let modified_at_unix =
        version_created_at_unix_from_payload(version_index_payload.as_deref(), &manifest_hash)?;
    let labels = decode_gallery_labels(&labels_json)?;
    Ok(GalleryIndexEntry {
        key,
        manifest_hash,
        size_bytes,
        modified_at_unix,
        content_fingerprint,
        media_metadata,
        labels,
    })
}

fn query_gallery_entry_from_db(
    db: &Connection,
    key: &str,
    scope: &GalleryDeltaScope,
) -> Result<Option<GalleryIndexEntry>> {
    // Filtering here rather than against the change log is what makes labelling
    // a photo `private` reach the client as a removal: the entry stops
    // resolving, and the caller falls through to its removal branch.
    let (label_sql, label_values) = gallery_label_predicates(&scope.label_filter, 2)?;
    let mut values = vec![Value::Text(key.to_string())];
    values.extend(label_values.into_iter().map(Value::Text));
    let row = db
        .query_row(
            &format!(
                "SELECT
                 gallery_objects.key,
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
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<Vec<u8>>>(4)?,
                    row.get::<_, Option<Vec<u8>>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<f64>>(7)?,
                    row.get::<_, Option<f64>>(8)?,
                    row.get::<_, String>(9)?,
                ))
            },
        )
        .optional()?;
    row.filter(|(key, _, _, _, _, _, media_type, latitude, longitude, _)| {
        gallery_entry_matches_delta_scope(key, media_type.as_deref(), *latitude, *longitude, scope)
    })
    .map(
        |(key, manifest_hash, size, fingerprint, metadata, versions, _, _, _, labels_json)| {
            materialize_gallery_index_entry(
                key,
                manifest_hash,
                size,
                fingerprint,
                metadata,
                versions,
                labels_json,
            )
        },
    )
    .transpose()
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
        super::GalleryIndexMediaFilter::All => true,
        super::GalleryIndexMediaFilter::Image => media_type == Some("image"),
        super::GalleryIndexMediaFilter::Video => media_type == Some("video"),
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

fn current_gallery_revision_from_db(db: &Connection) -> Result<u64> {
    let raw = db.query_row(
        "SELECT value FROM metadata_meta WHERE key = 'gallery_revision'",
        [],
        |row| row.get::<_, String>(0),
    )?;
    raw.parse::<u64>()
        .with_context(|| format!("invalid persisted gallery revision: {raw}"))
}

fn current_gallery_history_id_from_db(db: &Connection) -> Result<String> {
    let history_id = db.query_row(
        "SELECT value FROM metadata_meta WHERE key = 'gallery_history_id'",
        [],
        |row| row.get::<_, String>(0),
    )?;
    Uuid::parse_str(&history_id)
        .with_context(|| format!("invalid persisted gallery history id: {history_id}"))?;
    Ok(history_id)
}

fn query_gallery_delta_from_db(
    db: &Connection,
    history_id: &str,
    since_revision: u64,
    limit: usize,
    scope: &GalleryDeltaScope,
) -> Result<std::result::Result<GalleryDeltaPage, GalleryDeltaCursorError>> {
    let transaction = db.unchecked_transaction()?;
    let page =
        query_gallery_delta_in_transaction(&transaction, history_id, since_revision, limit, scope)?;
    transaction.commit()?;
    Ok(page)
}

fn query_gallery_delta_in_transaction(
    db: &Connection,
    history_id: &str,
    since_revision: u64,
    limit: usize,
    scope: &GalleryDeltaScope,
) -> Result<std::result::Result<GalleryDeltaPage, GalleryDeltaCursorError>> {
    let current_history_id = current_gallery_history_id_from_db(db)?;
    let current_revision = current_gallery_revision_from_db(db)?;
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
    let oldest_revision = db
        .query_row("SELECT MIN(revision) FROM gallery_changes", [], |row| {
            row.get::<_, Option<u64>>(0)
        })?
        .unwrap_or(current_revision.saturating_add(1));
    if since_revision.saturating_add(1) < oldest_revision {
        return Ok(Err(GalleryDeltaCursorError::Expired {
            history_id: current_history_id,
            current_revision,
        }));
    }

    let sql_limit =
        i64::try_from(limit.saturating_add(1)).context("gallery delta limit overflow")?;
    let mut statement = db.prepare(
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
    )?;
    let rows = statement.query_map(params![since_revision, sql_limit], |row| {
        Ok((
            row.get::<_, u64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<f64>>(4)?,
            row.get::<_, Option<f64>>(5)?,
        ))
    })?;
    let mut raw_changes = rows.collect::<std::result::Result<Vec<_>, _>>()?;
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
        let entry = query_gallery_entry_from_db(db, &key, scope)?;
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

async fn open_sqlite_writer_connection(metadata_db_path: &Path) -> Result<TokioConnection> {
    let connection = TokioConnection::open(metadata_db_path)
        .await
        .with_context(|| format!("failed to open {}", metadata_db_path.display()))?;
    connection
        .call(|db| {
            init_metadata_db(db).map_err(|err| {
                tokio_rusqlite::Error::Other(Box::new(std::io::Error::other(format!("{err:#}"))))
            })
        })
        .await
        .map_err(map_tokio_rusqlite_error)
        .with_context(|| format!("failed to initialize {}", metadata_db_path.display()))?;
    Ok(connection)
}

async fn open_sqlite_reader_connection(metadata_db_path: &Path) -> Result<TokioConnection> {
    let connection = TokioConnection::open(metadata_db_path)
        .await
        .with_context(|| format!("failed to open {}", metadata_db_path.display()))?;
    connection
        .call(|db| {
            configure_read_only_metadata_db_connection(db).map_err(|err| {
                tokio_rusqlite::Error::Other(Box::new(std::io::Error::other(format!("{err:#}"))))
            })
        })
        .await
        .map_err(map_tokio_rusqlite_error)
        .with_context(|| {
            format!(
                "failed to configure sqlite read connection for {}",
                metadata_db_path.display()
            )
        })?;
    Ok(connection)
}

fn sqlite_read_connection_count() -> usize {
    std::env::var("IRONMESH_SQLITE_READ_CONNECTIONS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .map(|value| value.max(1))
        .unwrap_or(DEFAULT_SQLITE_READ_CONNECTION_COUNT)
}

fn map_tokio_rusqlite_error(error: tokio_rusqlite::Error) -> anyhow::Error {
    match error {
        tokio_rusqlite::Error::ConnectionClosed => {
            anyhow!("sqlite metadata connection closed unexpectedly")
        }
        tokio_rusqlite::Error::Close((_, err)) => err.into(),
        tokio_rusqlite::Error::Rusqlite(err) => err.into(),
        tokio_rusqlite::Error::Other(err) => anyhow!("sqlite metadata worker error: {err}"),
        _ => anyhow!("sqlite metadata worker returned an unsupported error variant"),
    }
}

#[async_trait]
impl MetadataStore for SqliteMetadataStore {
    async fn load_current_state(&self) -> Result<CurrentState> {
        self.read(|db| load_current_state_from_db(db)).await
    }

    async fn get_current_object(&self, key: &str) -> Result<Option<CurrentObjectEntry>> {
        let key = key.to_string();
        self.read(move |db| {
            db.query_row(
                "SELECT manifest_hash, object_id FROM current_objects WHERE key = ?1",
                params![key],
                |row| {
                    Ok(CurrentObjectEntry {
                        manifest_hash: row.get(0)?,
                        object_id: row.get(1)?,
                    })
                },
            )
            .optional()
            .context("failed to query current object")
        })
        .await
    }

    async fn upsert_current_object(&self, key: &str, entry: &CurrentObjectEntry) -> Result<()> {
        let key = key.to_string();
        let entry = entry.clone();
        self.write_tx(move |db| {
            db.execute(
                "INSERT INTO current_objects (key, manifest_hash, object_id)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(key) DO UPDATE SET
                    manifest_hash = excluded.manifest_hash,
                    object_id = excluded.object_id",
                params![key, entry.manifest_hash, entry.object_id],
            )?;
            upsert_gallery_object(db, &key, &entry)
        })
        .await
    }

    async fn remove_current_object(&self, key: &str) -> Result<()> {
        let key = key.to_string();
        self.write_tx(move |db| {
            db.execute("DELETE FROM current_objects WHERE key = ?1", params![key])?;
            db.execute("DELETE FROM gallery_objects WHERE key = ?1", params![key])?;
            Ok(())
        })
        .await
    }

    async fn set_gallery_object_labels(&self, key: &str, labels: &[String]) -> Result<()> {
        let key = key.to_string();
        let labels_json = encode_gallery_labels(labels)?;
        self.write_tx(move |db| {
            db.execute(
                "UPDATE gallery_objects SET labels_json = ?2 WHERE key = ?1",
                params![key, labels_json],
            )?;
            Ok(())
        })
        .await
    }

    async fn query_gallery_index(
        &self,
        query: &GalleryIndexQuery,
    ) -> Result<Option<GalleryIndexPage>> {
        let query = query.clone();
        self.read(move |db| query_gallery_index_from_db(db, &query).map(Some))
            .await
    }

    async fn query_gallery_delta(
        &self,
        history_id: &str,
        since_revision: u64,
        limit: usize,
        scope: &GalleryDeltaScope,
    ) -> Result<Option<std::result::Result<GalleryDeltaPage, GalleryDeltaCursorError>>> {
        let history_id = history_id.to_string();
        let scope = scope.clone();
        self.read(move |db| {
            query_gallery_delta_from_db(db, &history_id, since_revision, limit, &scope).map(Some)
        })
        .await
    }

    async fn count_current_objects(&self) -> Result<usize> {
        self.read(|db| {
            let count: i64 =
                db.query_row("SELECT COUNT(*) FROM current_objects", [], |row| row.get(0))?;
            Ok(usize::try_from(count).unwrap_or(0))
        })
        .await
    }

    async fn list_current_object_keys(&self) -> Result<Vec<String>> {
        self.read(|db| {
            let mut stmt = db.prepare("SELECT key FROM current_objects")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            let mut keys = Vec::new();
            for row in rows {
                keys.push(row?);
            }
            Ok(keys)
        })
        .await
    }

    async fn list_keys_for_object_id(&self, object_id: &str) -> Result<Vec<String>> {
        let object_id = object_id.to_string();
        self.read(move |db| {
            let mut stmt = db.prepare("SELECT key FROM current_objects WHERE object_id = ?1")?;
            let rows = stmt.query_map(params![object_id], |row| row.get::<_, String>(0))?;
            let mut keys = Vec::new();
            for row in rows {
                keys.push(row?);
            }
            Ok(keys)
        })
        .await
    }

    async fn load_repair_attempts(
        &self,
    ) -> Result<std::collections::HashMap<String, RepairAttemptRecord>> {
        self.read(|db| {
            let mut stmt = db.prepare(
                "SELECT subject, attempts, last_failure_unix
                 FROM repair_attempts",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    RepairAttemptRecord {
                        attempts: row.get::<_, u32>(1)?,
                        last_failure_unix: row.get::<_, u64>(2)?,
                    },
                ))
            })?;

            let mut attempts = std::collections::HashMap::new();
            for row in rows {
                let (subject, record) = row?;
                attempts.insert(subject, record);
            }
            Ok(attempts)
        })
        .await
    }

    async fn persist_repair_attempts(
        &self,
        attempts: &std::collections::HashMap<String, RepairAttemptRecord>,
    ) -> Result<()> {
        let attempts = attempts.clone();
        self.write_tx(move |db| {
            db.execute("DELETE FROM repair_attempts", [])?;
            let mut stmt = db.prepare(
                "INSERT INTO repair_attempts (subject, attempts, last_failure_unix)
                 VALUES (?1, ?2, ?3)",
            )?;
            for (subject, record) in attempts {
                stmt.execute(params![
                    subject,
                    record.attempts,
                    u64_to_i64(record.last_failure_unix)?
                ])?;
            }
            Ok(())
        })
        .await
    }

    async fn list_repair_run_history(
        &self,
        limit: Option<usize>,
        finished_since_unix: Option<u64>,
    ) -> Result<Vec<RepairRunRecord>> {
        self.read(move |db| {
            let mut query =
                String::from("SELECT record_json\n             FROM repair_run_history");
            let mut conditions = Vec::new();
            if finished_since_unix.is_some() {
                conditions.push("finished_at_unix >= ?1");
            }
            if !conditions.is_empty() {
                query.push_str(" WHERE ");
                query.push_str(&conditions.join(" AND "));
            }
            query.push_str(" ORDER BY finished_at_unix DESC, run_id DESC");
            if limit.is_some() {
                query.push_str(" LIMIT ?");
                query.push_str(if finished_since_unix.is_some() {
                    "2"
                } else {
                    "1"
                });
            }

            let mut stmt = db.prepare(&query)?;
            let mut records = Vec::new();
            match (finished_since_unix, limit) {
                (Some(finished_since_unix), Some(limit)) => {
                    let rows = stmt.query_map(
                        params![u64_to_i64(finished_since_unix)?, usize_to_i64(limit)?],
                        |row| row.get::<_, Vec<u8>>(0),
                    )?;
                    for row in rows {
                        let payload = row?;
                        records.push(
                            serde_json::from_slice::<RepairRunRecord>(&payload)
                                .context("invalid repair run history record in sqlite")?,
                        );
                    }
                }
                (Some(finished_since_unix), None) => {
                    let rows = stmt
                        .query_map(params![u64_to_i64(finished_since_unix)?], |row| {
                            row.get::<_, Vec<u8>>(0)
                        })?;
                    for row in rows {
                        let payload = row?;
                        records.push(
                            serde_json::from_slice::<RepairRunRecord>(&payload)
                                .context("invalid repair run history record in sqlite")?,
                        );
                    }
                }
                (None, Some(limit)) => {
                    let rows = stmt.query_map(params![usize_to_i64(limit)?], |row| {
                        row.get::<_, Vec<u8>>(0)
                    })?;
                    for row in rows {
                        let payload = row?;
                        records.push(
                            serde_json::from_slice::<RepairRunRecord>(&payload)
                                .context("invalid repair run history record in sqlite")?,
                        );
                    }
                }
                (None, None) => {
                    let rows = stmt.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
                    for row in rows {
                        let payload = row?;
                        records.push(
                            serde_json::from_slice::<RepairRunRecord>(&payload)
                                .context("invalid repair run history record in sqlite")?,
                        );
                    }
                }
            }

            Ok(records)
        })
        .await
    }

    async fn persist_repair_run_record(&self, record: &RepairRunRecord) -> Result<()> {
        let payload = serde_json::to_vec_pretty(record)?;
        let run_id = record.run_id.clone();
        let finished_at_unix = record.finished_at_unix;
        self.write(move |db| {
            db.execute(
                "INSERT INTO repair_run_history (run_id, finished_at_unix, record_json)\n             VALUES (?1, ?2, ?3)\n             ON CONFLICT(run_id) DO UPDATE SET\n                 finished_at_unix = excluded.finished_at_unix,\n                 record_json = excluded.record_json",
                params![run_id, u64_to_i64(finished_at_unix)?, payload],
            )?;
            Ok(())
        })
        .await
    }

    async fn prune_repair_run_history_before(&self, finished_before_unix: u64) -> Result<()> {
        self.write(move |db| {
            db.execute(
                "DELETE FROM repair_run_history\n             WHERE finished_at_unix < ?1",
                params![u64_to_i64(finished_before_unix)?],
            )?;
            Ok(())
        })
        .await
    }

    async fn list_manual_repair_action_run_history(
        &self,
        limit: Option<usize>,
        finished_since_unix: Option<u64>,
    ) -> Result<Vec<ManualRepairActionRunRecord>> {
        self.read(move |db| {
            let mut query = String::from(
                "SELECT record_json\n             FROM manual_repair_action_run_history",
            );
            let mut conditions = Vec::new();
            if finished_since_unix.is_some() {
                conditions.push("finished_at_unix >= ?1");
            }
            if !conditions.is_empty() {
                query.push_str(" WHERE ");
                query.push_str(&conditions.join(" AND "));
            }
            query.push_str(" ORDER BY finished_at_unix DESC, run_id DESC");
            if limit.is_some() {
                query.push_str(" LIMIT ?");
                query.push_str(if finished_since_unix.is_some() {
                    "2"
                } else {
                    "1"
                });
            }

            let mut stmt = db.prepare(&query)?;
            let mut records = Vec::new();
            match (finished_since_unix, limit) {
                (Some(finished_since_unix), Some(limit)) => {
                    let rows = stmt.query_map(
                        params![u64_to_i64(finished_since_unix)?, usize_to_i64(limit)?],
                        |row| row.get::<_, Vec<u8>>(0),
                    )?;
                    for row in rows {
                        let payload = row?;
                        records.push(
                            serde_json::from_slice::<ManualRepairActionRunRecord>(&payload)
                                .context("invalid manual repair action history record in sqlite")?,
                        );
                    }
                }
                (Some(finished_since_unix), None) => {
                    let rows = stmt
                        .query_map(params![u64_to_i64(finished_since_unix)?], |row| {
                            row.get::<_, Vec<u8>>(0)
                        })?;
                    for row in rows {
                        let payload = row?;
                        records.push(
                            serde_json::from_slice::<ManualRepairActionRunRecord>(&payload)
                                .context("invalid manual repair action history record in sqlite")?,
                        );
                    }
                }
                (None, Some(limit)) => {
                    let rows = stmt.query_map(params![usize_to_i64(limit)?], |row| {
                        row.get::<_, Vec<u8>>(0)
                    })?;
                    for row in rows {
                        let payload = row?;
                        records.push(
                            serde_json::from_slice::<ManualRepairActionRunRecord>(&payload)
                                .context("invalid manual repair action history record in sqlite")?,
                        );
                    }
                }
                (None, None) => {
                    let rows = stmt.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
                    for row in rows {
                        let payload = row?;
                        records.push(
                            serde_json::from_slice::<ManualRepairActionRunRecord>(&payload)
                                .context("invalid manual repair action history record in sqlite")?,
                        );
                    }
                }
            }

            Ok(records)
        })
        .await
    }

    async fn persist_manual_repair_action_run_record(
        &self,
        record: &ManualRepairActionRunRecord,
    ) -> Result<()> {
        let payload = serde_json::to_vec_pretty(record)?;
        let run_id = record.run_id.clone();
        let finished_at_unix = record.finished_at_unix;
        self.write(move |db| {
            db.execute(
                "INSERT INTO manual_repair_action_run_history (run_id, finished_at_unix, record_json)\n             VALUES (?1, ?2, ?3)\n             ON CONFLICT(run_id) DO UPDATE SET\n                 finished_at_unix = excluded.finished_at_unix,\n                 record_json = excluded.record_json",
                params![run_id, u64_to_i64(finished_at_unix)?, payload],
            )?;
            Ok(())
        })
        .await
    }

    async fn prune_manual_repair_action_run_history_before(
        &self,
        finished_before_unix: u64,
    ) -> Result<()> {
        self.write(move |db| {
            db.execute(
                "DELETE FROM manual_repair_action_run_history\n             WHERE finished_at_unix < ?1",
                params![u64_to_i64(finished_before_unix)?],
            )?;
            Ok(())
        })
        .await
    }

    async fn list_data_scrub_run_history(
        &self,
        limit: Option<usize>,
        finished_since_unix: Option<u64>,
    ) -> Result<Vec<DataScrubRunRecord>> {
        self.read(move |db| {
            let mut query =
                String::from("SELECT record_json\n             FROM data_scrub_run_history");
            let mut conditions = Vec::new();
            if finished_since_unix.is_some() {
                conditions.push("finished_at_unix >= ?1");
            }
            if !conditions.is_empty() {
                query.push_str(" WHERE ");
                query.push_str(&conditions.join(" AND "));
            }
            query.push_str(" ORDER BY finished_at_unix DESC, run_id DESC");
            if limit.is_some() {
                query.push_str(" LIMIT ?");
                query.push_str(if finished_since_unix.is_some() {
                    "2"
                } else {
                    "1"
                });
            }

            let mut stmt = db.prepare(&query)?;
            let mut records = Vec::new();
            match (finished_since_unix, limit) {
                (Some(finished_since_unix), Some(limit)) => {
                    let rows = stmt.query_map(
                        params![u64_to_i64(finished_since_unix)?, usize_to_i64(limit)?],
                        |row| row.get::<_, Vec<u8>>(0),
                    )?;
                    for row in rows {
                        let payload = row?;
                        records.push(
                            serde_json::from_slice::<DataScrubRunRecord>(&payload)
                                .context("invalid data scrub run history record in sqlite")?,
                        );
                    }
                }
                (Some(finished_since_unix), None) => {
                    let rows = stmt
                        .query_map(params![u64_to_i64(finished_since_unix)?], |row| {
                            row.get::<_, Vec<u8>>(0)
                        })?;
                    for row in rows {
                        let payload = row?;
                        records.push(
                            serde_json::from_slice::<DataScrubRunRecord>(&payload)
                                .context("invalid data scrub run history record in sqlite")?,
                        );
                    }
                }
                (None, Some(limit)) => {
                    let rows = stmt.query_map(params![usize_to_i64(limit)?], |row| {
                        row.get::<_, Vec<u8>>(0)
                    })?;
                    for row in rows {
                        let payload = row?;
                        records.push(
                            serde_json::from_slice::<DataScrubRunRecord>(&payload)
                                .context("invalid data scrub run history record in sqlite")?,
                        );
                    }
                }
                (None, None) => {
                    let rows = stmt.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
                    for row in rows {
                        let payload = row?;
                        records.push(
                            serde_json::from_slice::<DataScrubRunRecord>(&payload)
                                .context("invalid data scrub run history record in sqlite")?,
                        );
                    }
                }
            }

            Ok(records)
        })
        .await
    }

    async fn persist_data_scrub_run_record(&self, record: &DataScrubRunRecord) -> Result<()> {
        let payload = serde_json::to_vec_pretty(record)?;
        let run_id = record.run_id.clone();
        let finished_at_unix = record.finished_at_unix;
        self.write(move |db| {
            db.execute(
                "INSERT INTO data_scrub_run_history (run_id, finished_at_unix, record_json)\n             VALUES (?1, ?2, ?3)\n             ON CONFLICT(run_id) DO UPDATE SET\n                 finished_at_unix = excluded.finished_at_unix,\n                 record_json = excluded.record_json",
                params![run_id, u64_to_i64(finished_at_unix)?, payload],
            )?;
            Ok(())
        })
        .await
    }

    async fn prune_data_scrub_run_history_before(&self, finished_before_unix: u64) -> Result<()> {
        self.write(move |db| {
            db.execute(
                "DELETE FROM data_scrub_run_history\n             WHERE finished_at_unix < ?1",
                params![u64_to_i64(finished_before_unix)?],
            )?;
            Ok(())
        })
        .await
    }

    async fn load_cluster_nodes(&self) -> Result<Vec<NodeDescriptor>> {
        self.read(|db| {
            let mut stmt = db.prepare(
                "SELECT node_id, descriptor_json
                 FROM cluster_nodes
                 ORDER BY node_id",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?;

            let mut nodes = Vec::new();
            for row in rows {
                let (node_id, payload) = row?;
                let descriptor =
                    serde_json::from_slice::<NodeDescriptor>(&payload).with_context(|| {
                        format!("invalid cluster node descriptor in sqlite for {node_id}")
                    })?;
                if descriptor.node_id.to_string() != node_id {
                    anyhow::bail!(
                        "cluster node descriptor id mismatch in sqlite: row={node_id} payload={}",
                        descriptor.node_id
                    );
                }
                nodes.push(descriptor);
            }
            Ok(nodes)
        })
        .await
    }

    async fn persist_cluster_nodes(&self, nodes: &[NodeDescriptor]) -> Result<()> {
        let nodes = nodes.to_vec();
        self.write_tx(move |db| {
            db.execute("DELETE FROM cluster_nodes", [])?;
            let mut stmt = db.prepare(
                "INSERT INTO cluster_nodes (node_id, descriptor_json)
                 VALUES (?1, ?2)",
            )?;
            for node in nodes {
                stmt.execute(params![
                    node.node_id.to_string(),
                    serde_json::to_vec_pretty(&node)?
                ])?;
            }
            Ok(())
        })
        .await
    }

    async fn load_cluster_replicas(
        &self,
    ) -> Result<std::collections::HashMap<String, Vec<NodeId>>> {
        self.read(|db| {
            let mut stmt = db.prepare(
                "SELECT subject, node_id
                 FROM cluster_replicas
                 ORDER BY subject, node_id",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;

            let mut replicas: std::collections::HashMap<String, Vec<NodeId>> =
                std::collections::HashMap::new();
            for row in rows {
                let (subject, node_id) = row?;
                let node_id = node_id
                    .parse::<NodeId>()
                    .with_context(|| format!("invalid node id in cluster replicas: {node_id}"))?;
                replicas.entry(subject).or_default().push(node_id);
            }
            Ok(replicas)
        })
        .await
    }

    async fn persist_cluster_replicas(
        &self,
        replicas: &std::collections::HashMap<String, Vec<NodeId>>,
    ) -> Result<()> {
        let replicas = replicas.clone();
        self.write_tx(move |db| {
            db.execute("DELETE FROM cluster_replicas", [])?;
            let mut stmt = db.prepare(
                "INSERT INTO cluster_replicas (subject, node_id)
                 VALUES (?1, ?2)",
            )?;
            for (subject, nodes) in replicas {
                for node_id in nodes {
                    stmt.execute(params![subject, node_id.to_string()])?;
                }
            }
            Ok(())
        })
        .await
    }

    async fn load_client_credential_state(&self) -> Result<ClientCredentialState> {
        let payload = self
            .read(|db| {
                db.query_row(
                    "SELECT state_json FROM client_credential_state WHERE singleton = 1",
                    [],
                    |row| row.get::<_, Vec<u8>>(0),
                )
                .optional()
                .map_err(Into::into)
            })
            .await?;

        match payload {
            Some(payload) => serde_json::from_slice::<ClientCredentialState>(&payload)
                .context("invalid client credential state in sqlite"),
            None => Ok(ClientCredentialState::default()),
        }
    }

    async fn persist_client_credential_state(&self, state: &ClientCredentialState) -> Result<()> {
        let payload = serde_json::to_vec_pretty(state)?;
        self.write(move |db| {
            db.execute(
                "INSERT INTO client_credential_state (singleton, state_json)
                 VALUES (1, ?1)
                 ON CONFLICT(singleton) DO UPDATE SET state_json = excluded.state_json",
                params![payload],
            )?;
            Ok(())
        })
        .await
    }

    async fn load_s3_control_plane_state(&self) -> Result<S3ControlPlaneState> {
        let db = self.metadata_conn()?;

        let mut bucket_stmt = db.prepare(
            "SELECT bucket_name, root_prefix, versioning_status, read_only,
                    created_at_unix, updated_at_unix, created_by, deleted_at_unix
             FROM s3_buckets
             ORDER BY bucket_name",
        )?;
        let bucket_rows = bucket_stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, bool>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<i64>>(7)?,
            ))
        })?;

        let mut buckets = Vec::new();
        for row in bucket_rows {
            let (
                bucket_name,
                root_prefix,
                versioning_status,
                read_only,
                created_at_unix,
                updated_at_unix,
                created_by,
                deleted_at_unix,
            ) = row?;
            buckets.push(S3BucketRecord {
                bucket_name,
                root_prefix,
                versioning_status: S3BucketVersioningStatus::parse(&versioning_status)
                    .with_context(|| {
                        format!(
                            "invalid S3 bucket versioning status in sqlite: {versioning_status}"
                        )
                    })?,
                read_only,
                created_at_unix: u64::try_from(created_at_unix)
                    .context("negative s3 bucket created_at_unix in sqlite")?,
                updated_at_unix: u64::try_from(updated_at_unix)
                    .context("negative s3 bucket updated_at_unix in sqlite")?,
                created_by,
                deleted_at_unix: deleted_at_unix
                    .map(|value| {
                        u64::try_from(value).context("negative s3 bucket deleted_at_unix in sqlite")
                    })
                    .transpose()?,
            });
        }

        let mut access_key_stmt = db.prepare(
            "SELECT access_key_id, secret_material, description, bucket_scope_json,
                    prefix_scope_json, allow_list, allow_read, allow_write,
                    allow_delete, allow_manage, created_at_unix, updated_at_unix,
                    last_used_at_unix, revoked_at_unix
                 FROM s3_access_keys
                 ORDER BY access_key_id",
        )?;
        let access_key_rows = access_key_stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, Vec<u8>>(4)?,
                row.get::<_, bool>(5)?,
                row.get::<_, bool>(6)?,
                row.get::<_, bool>(7)?,
                row.get::<_, bool>(8)?,
                row.get::<_, bool>(9)?,
                row.get::<_, i64>(10)?,
                row.get::<_, i64>(11)?,
                row.get::<_, Option<i64>>(12)?,
                row.get::<_, Option<i64>>(13)?,
            ))
        })?;

        let mut access_keys = Vec::new();
        for row in access_key_rows {
            let (
                access_key_id,
                secret_material,
                description,
                bucket_scope_json,
                prefix_scope_json,
                allow_list,
                allow_read,
                allow_write,
                allow_delete,
                allow_manage,
                created_at_unix,
                updated_at_unix,
                last_used_at_unix,
                revoked_at_unix,
            ) = row?;
            access_keys.push(S3AccessKeyRecord {
                access_key_id,
                secret_material,
                description,
                bucket_scope: serde_json::from_slice(&bucket_scope_json)
                    .context("invalid s3 access key bucket_scope_json in sqlite")?,
                prefix_scope: serde_json::from_slice(&prefix_scope_json)
                    .context("invalid s3 access key prefix_scope_json in sqlite")?,
                allow_list,
                allow_read,
                allow_write,
                allow_delete,
                allow_manage,
                created_at_unix: u64::try_from(created_at_unix)
                    .context("negative s3 access key created_at_unix in sqlite")?,
                updated_at_unix: u64::try_from(updated_at_unix)
                    .context("negative s3 access key updated_at_unix in sqlite")?,
                last_used_at_unix: last_used_at_unix
                    .map(|value| {
                        u64::try_from(value)
                            .context("negative s3 access key last_used_at_unix in sqlite")
                    })
                    .transpose()?,
                revoked_at_unix: revoked_at_unix
                    .map(|value| {
                        u64::try_from(value)
                            .context("negative s3 access key revoked_at_unix in sqlite")
                    })
                    .transpose()?,
            });
        }

        Ok(S3ControlPlaneState {
            buckets,
            access_keys,
        })
    }

    async fn persist_s3_control_plane_state(&self, state: &S3ControlPlaneState) -> Result<()> {
        self.in_metadata_tx(|db| {
            db.execute("DELETE FROM s3_buckets", [])?;
            db.execute("DELETE FROM s3_access_keys", [])?;

            let mut bucket_stmt = db.prepare(
                "INSERT INTO s3_buckets (
                     bucket_name,
                     root_prefix,
                     versioning_status,
                     read_only,
                     created_at_unix,
                     updated_at_unix,
                     created_by,
                     deleted_at_unix
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for bucket in &state.buckets {
                bucket_stmt.execute(params![
                    bucket.bucket_name.as_str(),
                    bucket.root_prefix.as_str(),
                    bucket.versioning_status.as_str(),
                    bucket.read_only,
                    u64_to_i64(bucket.created_at_unix)?,
                    u64_to_i64(bucket.updated_at_unix)?,
                    bucket.created_by.clone(),
                    bucket.deleted_at_unix.map(u64_to_i64).transpose()?,
                ])?;
            }

            let mut access_key_stmt = db.prepare(
                "INSERT INTO s3_access_keys (
                     access_key_id,
                     secret_material,
                     description,
                     bucket_scope_json,
                     prefix_scope_json,
                     allow_list,
                     allow_read,
                     allow_write,
                     allow_delete,
                     allow_manage,
                     created_at_unix,
                     updated_at_unix,
                     last_used_at_unix,
                     revoked_at_unix
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            )?;
            for access_key in &state.access_keys {
                access_key_stmt.execute(params![
                    access_key.access_key_id.as_str(),
                    access_key.secret_material.as_str(),
                    access_key.description.clone(),
                    serde_json::to_vec(&access_key.bucket_scope)?,
                    serde_json::to_vec(&access_key.prefix_scope)?,
                    access_key.allow_list,
                    access_key.allow_read,
                    access_key.allow_write,
                    access_key.allow_delete,
                    access_key.allow_manage,
                    u64_to_i64(access_key.created_at_unix)?,
                    u64_to_i64(access_key.updated_at_unix)?,
                    access_key.last_used_at_unix.map(u64_to_i64).transpose()?,
                    access_key.revoked_at_unix.map(u64_to_i64).transpose()?,
                ])?;
            }

            Ok(())
        })
        .await
    }

    async fn load_snapshot_manifest(&self, snapshot_id: &str) -> Result<Option<SnapshotManifest>> {
        let snapshot_id = snapshot_id.to_string();
        let payload = self
            .read(move |db| {
                db.query_row(
                    "SELECT snapshot_json FROM snapshots WHERE snapshot_id = ?1",
                    params![snapshot_id],
                    |row| row.get::<_, Vec<u8>>(0),
                )
                .optional()
                .map_err(Into::into)
            })
            .await?;
        match payload {
            Some(payload) => {
                let payload = decompress_snapshot_json(&payload)?;
                serde_json::from_slice::<SnapshotManifest>(&payload)
                    .map(Some)
                    .context("invalid snapshot manifest in sqlite")
            }
            None => Ok(None),
        }
    }

    async fn load_snapshot_batch_state(&self) -> Result<Option<ActiveSnapshotBatch>> {
        let payload = self
            .read(|db| {
                db.query_row(
                    "SELECT state_json
                     FROM snapshot_batch_state
                     WHERE singleton = 1",
                    [],
                    |row| row.get::<_, Vec<u8>>(0),
                )
                .optional()
                .map_err(Into::into)
            })
            .await?;
        match payload {
            Some(payload) => serde_json::from_slice::<ActiveSnapshotBatch>(&payload)
                .map(Some)
                .context("invalid snapshot batch state in sqlite"),
            None => Ok(None),
        }
    }

    async fn persist_snapshot_batch_state(
        &self,
        state: Option<&ActiveSnapshotBatch>,
    ) -> Result<()> {
        let state = state.cloned();
        self.write(move |db| {
            match state {
                Some(state) => {
                    let payload = serde_json::to_vec_pretty(&state)?;
                    db.execute(
                        "INSERT INTO snapshot_batch_state (singleton, state_json)
                         VALUES (1, ?1)
                         ON CONFLICT(singleton) DO UPDATE SET state_json = excluded.state_json",
                        params![payload],
                    )?;
                }
                None => {
                    db.execute("DELETE FROM snapshot_batch_state WHERE singleton = 1", [])?;
                }
            }
            Ok(())
        })
        .await
    }

    async fn load_cached_media_metadata(
        &self,
        content_fingerprint: &str,
    ) -> Result<Option<CachedMediaMetadata>> {
        let content_fingerprint_owned = content_fingerprint.to_string();
        let payload = self
            .read({
                let content_fingerprint = content_fingerprint_owned.clone();
                move |db| {
                    db.query_row(
                        "SELECT metadata_json FROM media_cache WHERE content_fingerprint = ?1",
                        params![content_fingerprint],
                        |row| row.get::<_, Vec<u8>>(0),
                    )
                    .optional()
                    .map_err(Into::into)
                }
            })
            .await?;
        match payload {
            Some(payload) => match serde_json::from_slice::<CachedMediaMetadata>(&payload) {
                Ok(metadata) => Ok(Some(metadata)),
                Err(err) => {
                    let deleted = self
                        .delete_invalid_media_cache_row_if_payload_matches(
                            content_fingerprint_owned.clone(),
                            payload,
                        )
                        .await
                        .with_context(|| {
                            format!(
                                "failed to delete invalid media metadata row for {content_fingerprint_owned}"
                            )
                        })?;
                    if deleted {
                        warn!(
                            content_fingerprint = %content_fingerprint,
                            error = %err,
                            "deleted invalid cached media metadata row from sqlite"
                        );
                    }
                    Ok(None)
                }
            },
            None => Ok(None),
        }
    }

    async fn load_cached_media_metadata_many(
        &self,
        content_fingerprints: &[String],
    ) -> Result<std::collections::HashMap<String, CachedMediaMetadata>> {
        const SQLITE_MEDIA_CACHE_QUERY_BATCH_SIZE: usize = 500;
        let content_fingerprints = content_fingerprints.to_vec();
        let (metadata_by_content_fingerprint, invalid_rows) = self
            .read(move |db| {
                let mut metadata_by_content_fingerprint =
                    std::collections::HashMap::with_capacity(content_fingerprints.len());
                let mut invalid_rows = Vec::new();

                for chunk in content_fingerprints.chunks(SQLITE_MEDIA_CACHE_QUERY_BATCH_SIZE) {
                    if chunk.is_empty() {
                        continue;
                    }

                    let placeholders = std::iter::repeat_n("?", chunk.len())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let query = format!(
                        "SELECT content_fingerprint, metadata_json
                         FROM media_cache
                         WHERE content_fingerprint IN ({placeholders})"
                    );
                    let mut stmt = db.prepare(&query)?;
                    let rows = stmt.query_map(params_from_iter(chunk.iter()), |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
                    })?;

                    for row in rows {
                        let (content_fingerprint, payload) = row?;
                        match serde_json::from_slice::<CachedMediaMetadata>(&payload) {
                            Ok(metadata) => {
                                metadata_by_content_fingerprint
                                    .insert(content_fingerprint, metadata);
                            }
                            Err(err) => {
                                invalid_rows.push((content_fingerprint, payload, err.to_string()));
                            }
                        }
                    }
                }

                Ok((metadata_by_content_fingerprint, invalid_rows))
            })
            .await?;

        for (content_fingerprint, payload, error) in invalid_rows {
            let deleted = self
                .delete_invalid_media_cache_row_if_payload_matches(
                    content_fingerprint.clone(),
                    payload,
                )
                .await
                .with_context(|| {
                    format!("failed to delete invalid media metadata row for {content_fingerprint}")
                })?;
            if deleted {
                warn!(
                    content_fingerprint = %content_fingerprint,
                    error,
                    "deleted invalid cached media metadata row from sqlite"
                );
            }
        }

        Ok(metadata_by_content_fingerprint)
    }

    async fn persist_media_cache_record(&self, metadata: &CachedMediaMetadata) -> Result<()> {
        let payload = serde_json::to_vec_pretty(metadata)?;
        let content_fingerprint = metadata.content_fingerprint.clone();
        self.write_tx(move |db| {
            let changed = db.execute(
                "INSERT INTO media_cache (content_fingerprint, metadata_json)
                 VALUES (?1, ?2)
                 ON CONFLICT(content_fingerprint) DO UPDATE
                 SET metadata_json = excluded.metadata_json
                 WHERE media_cache.metadata_json != excluded.metadata_json",
                params![content_fingerprint, payload],
            )?;
            if changed > 0 {
                let unchanged_projection_keys =
                    refresh_gallery_objects_for_content_fingerprint_and_collect_unchanged_keys(
                        db,
                        &content_fingerprint,
                    )?;
                record_gallery_upserts_for_keys(db, &unchanged_projection_keys)?;
            }
            Ok(())
        })
        .await
    }

    #[cfg(test)]
    async fn has_media_cache_record(&self, content_fingerprint: &str) -> Result<bool> {
        let content_fingerprint = content_fingerprint.to_string();
        self.read(move |db| {
            let exists = db.query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM media_cache WHERE content_fingerprint = ?1
                 )",
                params![content_fingerprint],
                |row| row.get::<_, i64>(0),
            )?;
            Ok(exists != 0)
        })
        .await
    }

    async fn delete_media_cache_record(&self, content_fingerprint: &str) -> Result<()> {
        let content_fingerprint = content_fingerprint.to_string();
        self.write_tx(move |db| {
            let deleted = db.execute(
                "DELETE FROM media_cache WHERE content_fingerprint = ?1",
                params![content_fingerprint],
            )?;
            if deleted > 0 {
                let unchanged_projection_keys =
                    refresh_gallery_objects_for_content_fingerprint_and_collect_unchanged_keys(
                        db,
                        &content_fingerprint,
                    )?;
                record_gallery_upserts_for_keys(db, &unchanged_projection_keys)?;
            }
            Ok(())
        })
        .await
    }

    async fn list_snapshot_infos(&self) -> Result<Vec<SnapshotInfo>> {
        self.read(|db| {
            let mut snapshots = Vec::new();
            let mut stmt = db.prepare(
                "SELECT snapshot_id, created_at_unix, object_count
                 FROM snapshots
                 ORDER BY created_at_unix DESC, snapshot_id DESC",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok(SnapshotInfo {
                    id: row.get(0)?,
                    created_at_unix: row.get::<_, u64>(1)?,
                    object_count: row.get::<_, i64>(2)? as usize,
                })
            })?;
            for row in rows {
                snapshots.push(row?);
            }
            Ok(snapshots)
        })
        .await
    }

    async fn list_data_change_events(
        &self,
        query: &DataChangeEventQuery,
    ) -> Result<Vec<DataChangeEvent>> {
        let limit = query
            .limit
            .map(usize_to_i64)
            .transpose()?
            .unwrap_or(i64::MAX);
        let action_filter = query.action.map(|action| action.as_str().to_string());
        let path_filter = query
            .path_prefix
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| format!("{value}%"));
        let actor_filter = query
            .actor_query
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| format!("%{value}%"));
        let before_created_at_unix = query
            .before
            .as_ref()
            .map(|cursor| u64_to_i64(cursor.created_at_unix))
            .transpose()?;
        let before_event_id = query.before.as_ref().map(|cursor| cursor.event_id.clone());

        self.read(move |db| {
            let mut stmt = db.prepare(
                "SELECT event_json
                 FROM data_change_events
                 WHERE (?1 IS NULL OR action = ?1)
                   AND (?2 IS NULL OR path LIKE ?2 OR COALESCE(from_path, '') LIKE ?2 OR COALESCE(to_path, '') LIKE ?2)
                   AND (?3 IS NULL OR COALESCE(actor_id, '') LIKE ?3 OR COALESCE(actor_label, '') LIKE ?3 OR COALESCE(actor_credential_fingerprint, '') LIKE ?3)
                                 AND (?4 IS NULL OR created_at_unix < ?4 OR (created_at_unix = ?4 AND event_id < ?5))
                             ORDER BY created_at_unix DESC, event_id DESC
                             LIMIT ?6",
            )?;
            let rows = stmt.query_map(
                params![
                    action_filter,
                    path_filter,
                    actor_filter,
                    before_created_at_unix,
                    before_event_id,
                    limit
                ],
                |row| row.get::<_, Vec<u8>>(0),
            )?;

            let mut events = Vec::new();
            for row in rows {
                let payload = row?;
                events.push(
                    serde_json::from_slice::<DataChangeEvent>(&payload)
                        .context("invalid data change event in sqlite")?,
                );
            }

            Ok(events)
        })
        .await
    }

    async fn append_admin_audit_event(&self, event: &AdminAuditEvent) -> Result<()> {
        let payload = serde_json::to_vec(event)?;
        let event_id = event.event_id.clone();
        let created_at_unix = event.created_at_unix;
        self.write(move |db| {
            db.execute(
                "INSERT INTO admin_audit_events (event_id, created_at_unix, event_json)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(event_id) DO UPDATE
                 SET created_at_unix = excluded.created_at_unix,
                     event_json = excluded.event_json",
                params![event_id, u64_to_i64(created_at_unix)?, payload],
            )?;
            Ok(())
        })
        .await
    }

    async fn append_data_change_event(&self, event: &DataChangeEvent) -> Result<()> {
        let payload = serde_json::to_vec(event)?;
        let event_id = event.event_id.clone();
        let created_at_unix = event.created_at_unix;
        let action = event.action.as_str().to_string();
        let path = event.path.clone();
        let from_path = event.from_path.clone();
        let to_path = event.to_path.clone();
        let actor_kind = event.actor_kind.as_str().to_string();
        let actor_id = event.actor_id.clone();
        let actor_label = event.actor_label.clone();
        let actor_credential_fingerprint = event.actor_credential_fingerprint.clone();
        self.write(move |db| {
            db.execute(
                "INSERT INTO data_change_events (
                     event_id,
                     created_at_unix,
                     action,
                     path,
                     from_path,
                     to_path,
                     actor_kind,
                     actor_id,
                     actor_label,
                     actor_credential_fingerprint,
                     event_json
                 )
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(event_id) DO UPDATE
                 SET created_at_unix = excluded.created_at_unix,
                     action = excluded.action,
                     path = excluded.path,
                     from_path = excluded.from_path,
                     to_path = excluded.to_path,
                     actor_kind = excluded.actor_kind,
                     actor_id = excluded.actor_id,
                     actor_label = excluded.actor_label,
                     actor_credential_fingerprint = excluded.actor_credential_fingerprint,
                     event_json = excluded.event_json",
                params![
                    event_id,
                    u64_to_i64(created_at_unix)?,
                    action,
                    path,
                    from_path,
                    to_path,
                    actor_kind,
                    actor_id,
                    actor_label,
                    actor_credential_fingerprint,
                    payload,
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn load_version_index_by_object_id(
        &self,
        object_id: &str,
    ) -> Result<Option<FileVersionIndex>> {
        let object_id = object_id.to_string();
        let payload = self
            .read(move |db| {
                db.query_row(
                    "SELECT index_json FROM version_indexes WHERE object_id = ?1",
                    params![object_id],
                    |row| row.get::<_, Vec<u8>>(0),
                )
                .optional()
                .map_err(Into::into)
            })
            .await?;

        match payload {
            Some(payload) => serde_json::from_slice::<FileVersionIndex>(&payload)
                .map(Some)
                .context("invalid version index in sqlite"),
            None => Ok(None),
        }
    }

    async fn load_manifest_summaries(
        &self,
        manifest_hashes: &[String],
    ) -> Result<std::collections::HashMap<String, ManifestSummary>> {
        const SQLITE_SUMMARY_QUERY_BATCH_SIZE: usize = 500;
        let manifest_hashes = manifest_hashes.to_vec();
        self.read(move |db| {
            let mut summaries = std::collections::HashMap::with_capacity(manifest_hashes.len());
            for chunk in manifest_hashes.chunks(SQLITE_SUMMARY_QUERY_BATCH_SIZE) {
                if chunk.is_empty() {
                    continue;
                }

                let placeholders = std::iter::repeat_n("?", chunk.len())
                    .collect::<Vec<_>>()
                    .join(", ");
                let query = format!(
                    "SELECT manifest_hash, total_size_bytes, content_fingerprint
                     FROM manifest_summaries
                     WHERE manifest_hash IN ({placeholders})"
                );
                let mut stmt = db.prepare(&query)?;
                let rows = stmt.query_map(params_from_iter(chunk.iter()), |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?;

                for row in rows {
                    let (manifest_hash, total_size_bytes, content_fingerprint) = row?;
                    summaries.insert(
                        manifest_hash,
                        ManifestSummary {
                            total_size_bytes: u64::try_from(total_size_bytes)
                                .context("negative manifest summary size in sqlite")?,
                            content_fingerprint,
                        },
                    );
                }
            }

            Ok(summaries)
        })
        .await
    }

    async fn load_object_version_metadata(
        &self,
        version_id: &str,
    ) -> Result<Option<ObjectVersionMetadataRecord>> {
        let db = self.metadata_conn()?;
        let row = db
            .query_row(
                "SELECT content_type, content_encoding, content_language,
                        cache_control, content_disposition, user_metadata_json,
                        checksum_sha256, checksum_crc32c, updated_at_unix
                 FROM object_version_metadata
                 WHERE version_id = ?1",
                params![version_id],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Vec<u8>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, Option<String>>(7)?,
                        row.get::<_, i64>(8)?,
                    ))
                },
            )
            .optional()?;

        match row {
            Some((
                content_type,
                content_encoding,
                content_language,
                cache_control,
                content_disposition,
                user_metadata_json,
                checksum_sha256,
                checksum_crc32c,
                updated_at_unix,
            )) => Ok(Some(ObjectVersionMetadataRecord {
                version_id: version_id.to_string(),
                content_type,
                content_encoding,
                content_language,
                cache_control,
                content_disposition,
                user_metadata: serde_json::from_slice(&user_metadata_json)
                    .context("invalid object version user_metadata_json in sqlite")?,
                checksum_sha256,
                checksum_crc32c,
                updated_at_unix: u64::try_from(updated_at_unix)
                    .context("negative object version updated_at_unix in sqlite")?,
            })),
            None => Ok(None),
        }
    }

    async fn persist_object_version_metadata(
        &self,
        metadata: &ObjectVersionMetadataRecord,
    ) -> Result<()> {
        let db = self.metadata_conn()?;
        db.execute(
            "INSERT INTO object_version_metadata (
                 version_id,
                 content_type,
                 content_encoding,
                 content_language,
                 cache_control,
                 content_disposition,
                 user_metadata_json,
                 checksum_sha256,
                 checksum_crc32c,
                 updated_at_unix
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(version_id) DO UPDATE
             SET content_type = excluded.content_type,
                 content_encoding = excluded.content_encoding,
                 content_language = excluded.content_language,
                 cache_control = excluded.cache_control,
                 content_disposition = excluded.content_disposition,
                 user_metadata_json = excluded.user_metadata_json,
                 checksum_sha256 = excluded.checksum_sha256,
                 checksum_crc32c = excluded.checksum_crc32c,
                 updated_at_unix = excluded.updated_at_unix",
            params![
                metadata.version_id,
                metadata.content_type,
                metadata.content_encoding,
                metadata.content_language,
                metadata.cache_control,
                metadata.content_disposition,
                serde_json::to_vec(&metadata.user_metadata)?,
                metadata.checksum_sha256,
                metadata.checksum_crc32c,
                u64_to_i64(metadata.updated_at_unix)?,
            ],
        )?;
        Ok(())
    }

    async fn delete_object_version_metadata(&self, version_id: &str) -> Result<()> {
        let db = self.metadata_conn()?;
        db.execute(
            "DELETE FROM object_version_metadata WHERE version_id = ?1",
            params![version_id],
        )?;
        Ok(())
    }

    async fn load_s3_object_version(
        &self,
        bucket_name: &str,
        version_id: &str,
    ) -> Result<Option<S3ObjectVersionRecord>> {
        let db = self.metadata_conn()?;
        let row = db
            .query_row(
                "SELECT ironmesh_key, etag, multipart_part_count, created_at_unix
                 FROM s3_object_versions
                 WHERE bucket_name = ?1 AND version_id = ?2",
                params![bucket_name, version_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?;

        match row {
            Some((ironmesh_key, etag, multipart_part_count, created_at_unix)) => {
                Ok(Some(S3ObjectVersionRecord {
                    bucket_name: bucket_name.to_string(),
                    ironmesh_key,
                    version_id: version_id.to_string(),
                    etag,
                    multipart_part_count: multipart_part_count
                        .map(|value| {
                            u32::try_from(value)
                                .context("negative or overflowing multipart_part_count in sqlite")
                        })
                        .transpose()?,
                    created_at_unix: u64::try_from(created_at_unix)
                        .context("negative s3 object version created_at_unix in sqlite")?,
                }))
            }
            None => Ok(None),
        }
    }

    async fn list_s3_object_versions_for_key(
        &self,
        bucket_name: &str,
        ironmesh_key: &str,
    ) -> Result<Vec<S3ObjectVersionRecord>> {
        let db = self.metadata_conn()?;
        let mut stmt = db.prepare(
            "SELECT version_id, etag, multipart_part_count, created_at_unix
             FROM s3_object_versions
             WHERE bucket_name = ?1 AND ironmesh_key = ?2
             ORDER BY created_at_unix DESC, version_id DESC",
        )?;
        let rows = stmt.query_map(params![bucket_name, ironmesh_key], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;

        let mut records = Vec::new();
        for row in rows {
            let (version_id, etag, multipart_part_count, created_at_unix) = row?;
            records.push(S3ObjectVersionRecord {
                bucket_name: bucket_name.to_string(),
                ironmesh_key: ironmesh_key.to_string(),
                version_id,
                etag,
                multipart_part_count: multipart_part_count
                    .map(|value| {
                        u32::try_from(value)
                            .context("negative or overflowing multipart_part_count in sqlite")
                    })
                    .transpose()?,
                created_at_unix: u64::try_from(created_at_unix)
                    .context("negative s3 object version created_at_unix in sqlite")?,
            });
        }
        Ok(records)
    }

    async fn list_s3_object_versions(
        &self,
        bucket_name: &str,
        ironmesh_key_prefix: Option<&str>,
    ) -> Result<Vec<S3ObjectVersionRecord>> {
        let db = self.metadata_conn()?;
        let mut records = Vec::new();
        if let Some(prefix) = ironmesh_key_prefix {
            let like_pattern = super::sqlite_like_prefix_pattern(prefix);
            let mut stmt = db.prepare(
                "SELECT ironmesh_key, version_id, etag, multipart_part_count, created_at_unix
                 FROM s3_object_versions
                 WHERE bucket_name = ?1 AND ironmesh_key LIKE ?2 ESCAPE '\\'
                 ORDER BY ironmesh_key ASC, created_at_unix DESC, version_id DESC",
            )?;
            let rows = stmt.query_map(params![bucket_name, like_pattern], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })?;

            for row in rows {
                let (ironmesh_key, version_id, etag, multipart_part_count, created_at_unix) = row?;
                records.push(S3ObjectVersionRecord {
                    bucket_name: bucket_name.to_string(),
                    ironmesh_key,
                    version_id,
                    etag,
                    multipart_part_count: multipart_part_count
                        .map(|value| {
                            u32::try_from(value)
                                .context("negative or overflowing multipart_part_count in sqlite")
                        })
                        .transpose()?,
                    created_at_unix: u64::try_from(created_at_unix)
                        .context("negative s3 object version created_at_unix in sqlite")?,
                });
            }
        } else {
            let mut stmt = db.prepare(
                "SELECT ironmesh_key, version_id, etag, multipart_part_count, created_at_unix
                 FROM s3_object_versions
                 WHERE bucket_name = ?1
                 ORDER BY ironmesh_key ASC, created_at_unix DESC, version_id DESC",
            )?;
            let rows = stmt.query_map(params![bucket_name], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })?;

            for row in rows {
                let (ironmesh_key, version_id, etag, multipart_part_count, created_at_unix) = row?;
                records.push(S3ObjectVersionRecord {
                    bucket_name: bucket_name.to_string(),
                    ironmesh_key,
                    version_id,
                    etag,
                    multipart_part_count: multipart_part_count
                        .map(|value| {
                            u32::try_from(value)
                                .context("negative or overflowing multipart_part_count in sqlite")
                        })
                        .transpose()?,
                    created_at_unix: u64::try_from(created_at_unix)
                        .context("negative s3 object version created_at_unix in sqlite")?,
                });
            }
        }
        Ok(records)
    }

    async fn persist_s3_object_version(&self, record: &S3ObjectVersionRecord) -> Result<()> {
        let db = self.metadata_conn()?;
        db.execute(
            "INSERT INTO s3_object_versions (
                 bucket_name,
                 ironmesh_key,
                 version_id,
                 etag,
                 multipart_part_count,
                 created_at_unix
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(bucket_name, version_id) DO UPDATE
             SET ironmesh_key = excluded.ironmesh_key,
                 etag = excluded.etag,
                 multipart_part_count = excluded.multipart_part_count,
                 created_at_unix = excluded.created_at_unix",
            params![
                record.bucket_name,
                record.ironmesh_key,
                record.version_id,
                record.etag,
                record.multipart_part_count.map(i64::from),
                u64_to_i64(record.created_at_unix)?,
            ],
        )?;
        Ok(())
    }

    async fn delete_s3_object_version(&self, bucket_name: &str, version_id: &str) -> Result<()> {
        let db = self.metadata_conn()?;
        db.execute(
            "DELETE FROM s3_object_versions
             WHERE bucket_name = ?1 AND version_id = ?2",
            params![bucket_name, version_id],
        )?;
        Ok(())
    }

    async fn persist_manifest_summary(
        &self,
        manifest_hash: &str,
        summary: &ManifestSummary,
    ) -> Result<()> {
        let manifest_hash = manifest_hash.to_string();
        let total_size_bytes = summary.total_size_bytes;
        let content_fingerprint = summary.content_fingerprint.clone();
        self.write_tx(move |db| {
            let changed = db.execute(
                "INSERT INTO manifest_summaries (manifest_hash, total_size_bytes, content_fingerprint)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(manifest_hash) DO UPDATE
                 SET total_size_bytes = excluded.total_size_bytes,
                     content_fingerprint = excluded.content_fingerprint
                 WHERE manifest_summaries.total_size_bytes != excluded.total_size_bytes
                    OR manifest_summaries.content_fingerprint != excluded.content_fingerprint",
                params![
                    manifest_hash,
                    u64_to_i64(total_size_bytes)?,
                    content_fingerprint
                ],
            )?;
            if changed > 0 {
                let unchanged_projection_keys =
                    refresh_gallery_objects_for_manifest_and_collect_unchanged_keys(
                        db,
                        &manifest_hash,
                    )?;
                record_gallery_upserts_for_keys(db, &unchanged_projection_keys)?;
            }
            Ok(())
        })
        .await
    }

    async fn persist_version_index_by_object_id(
        &self,
        object_id: &str,
        index: &FileVersionIndex,
    ) -> Result<()> {
        let payload = serde_json::to_vec_pretty(index)?;
        let object_id = object_id.to_string();
        self.write_tx(move |db| {
            let changed = db.execute(
                "INSERT INTO version_indexes (object_id, index_json)
                 VALUES (?1, ?2)
                 ON CONFLICT(object_id) DO UPDATE SET index_json = excluded.index_json
                 WHERE version_indexes.index_json != excluded.index_json",
                params![object_id, payload],
            )?;
            if changed > 0 {
                let unchanged_projection_keys =
                    refresh_gallery_objects_for_object_id_and_collect_unchanged_keys(
                        db, &object_id,
                    )?;
                record_gallery_upserts_for_keys(db, &unchanged_projection_keys)?;
            }
            Ok(())
        })
        .await
    }

    async fn load_all_version_indexes(&self) -> Result<Vec<FileVersionIndex>> {
        self.read(|db| {
            let mut stmt = db.prepare(
                "SELECT index_json
                 FROM version_indexes
                 ORDER BY object_id",
            )?;
            let rows = stmt.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
            let mut indexes = Vec::new();
            for row in rows {
                let payload = row?;
                indexes.push(
                    serde_json::from_slice::<FileVersionIndex>(&payload)
                        .context("invalid version index in sqlite")?,
                );
            }
            Ok(indexes)
        })
        .await
    }

    async fn list_version_index_object_ids(&self) -> Result<Vec<String>> {
        self.read(|db| {
            let mut stmt =
                db.prepare("SELECT object_id FROM version_indexes ORDER BY object_id")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            let mut ids = Vec::new();
            for row in rows {
                ids.push(row?);
            }
            Ok(ids)
        })
        .await
    }

    async fn persist_snapshot_manifest(&self, manifest: &SnapshotManifest) -> Result<()> {
        let payload = compress_snapshot_json(&serde_json::to_vec_pretty(manifest)?)?;
        let manifest_id = manifest.id.clone();
        let created_at_unix = manifest.created_at_unix;
        let object_count = manifest.objects.len();
        self.write(move |db| {
            db.execute(
                "INSERT INTO snapshots (snapshot_id, created_at_unix, object_count, snapshot_json)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(snapshot_id) DO UPDATE SET
                     created_at_unix = excluded.created_at_unix,
                     object_count = excluded.object_count,
                     snapshot_json = excluded.snapshot_json",
                params![
                    manifest_id,
                    u64_to_i64(created_at_unix)?,
                    i64::try_from(object_count).context("snapshot object count overflow")?,
                    payload
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn load_all_snapshots(&self) -> Result<Vec<SnapshotManifest>> {
        self.read(|db| {
            let mut stmt = db.prepare(
                "SELECT snapshot_json
                 FROM snapshots
                 ORDER BY created_at_unix DESC, snapshot_id DESC",
            )?;
            let rows = stmt.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
            let mut snapshots = Vec::new();
            for row in rows {
                let payload = row?;
                let payload = decompress_snapshot_json(&payload)?;
                snapshots.push(
                    serde_json::from_slice::<SnapshotManifest>(&payload)
                        .context("invalid snapshot manifest in sqlite")?,
                );
            }
            Ok(snapshots)
        })
        .await
    }

    async fn load_snapshot_by_id(&self, snapshot_id: &str) -> Result<Option<SnapshotManifest>> {
        let snapshot_id = snapshot_id.to_string();
        let payload = self
            .read(move |db| {
                db.query_row(
                    "SELECT snapshot_json FROM snapshots WHERE snapshot_id = ?1",
                    params![snapshot_id],
                    |row| row.get::<_, Vec<u8>>(0),
                )
                .optional()
                .map_err(Into::into)
            })
            .await?;
        match payload {
            Some(payload) => {
                let payload = decompress_snapshot_json(&payload)?;
                serde_json::from_slice::<SnapshotManifest>(&payload)
                    .map(Some)
                    .context("invalid snapshot manifest in sqlite")
            }
            None => Ok(None),
        }
    }

    async fn list_uncompressed_snapshot_ids(&self) -> Result<Vec<String>> {
        const ZSTD_MAGIC: &[u8] = &[0x28, 0xB5, 0x2F, 0xFD];
        self.read(|db| {
            let mut stmt = db.prepare("SELECT snapshot_id, snapshot_json FROM snapshots")?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?;
            let mut ids = Vec::new();
            for row in rows {
                let (id, payload) = row?;
                if !payload.starts_with(ZSTD_MAGIC) {
                    ids.push(id);
                }
            }
            Ok(ids)
        })
        .await
    }

    async fn delete_snapshots_by_id(&self, snapshot_ids: &[String]) -> Result<()> {
        const SQLITE_SNAPSHOT_DELETE_BATCH_SIZE: usize = 500;

        if snapshot_ids.is_empty() {
            return Ok(());
        }

        let snapshot_ids = snapshot_ids.to_vec();
        self.write_tx(move |db| {
            for chunk in snapshot_ids.chunks(SQLITE_SNAPSHOT_DELETE_BATCH_SIZE) {
                let placeholders = std::iter::repeat_n("?", chunk.len())
                    .collect::<Vec<_>>()
                    .join(", ");
                let query = format!("DELETE FROM snapshots WHERE snapshot_id IN ({placeholders})");
                db.execute(&query, params_from_iter(chunk.iter()))?;
            }
            Ok(())
        })
        .await
    }

    async fn vacuum_metadata_store(&self) -> Result<bool> {
        self.write(|db| {
            db.execute_batch("VACUUM")?;
            Ok(true)
        })
        .await
    }

    async fn load_storage_stats_state(&self) -> Result<Option<StorageStatsState>> {
        let payload = self
            .read(|db| {
                db.query_row(
                    "SELECT state_json
                     FROM storage_stats_state
                     WHERE singleton = 1",
                    [],
                    |row| row.get::<_, Vec<u8>>(0),
                )
                .optional()
                .map_err(Into::into)
            })
            .await?;
        match payload {
            Some(payload) => serde_json::from_slice::<StorageStatsState>(&payload)
                .map(Some)
                .context("invalid storage stats state in sqlite"),
            None => Ok(None),
        }
    }

    async fn persist_storage_stats_state(&self, state: &StorageStatsState) -> Result<()> {
        let payload = serde_json::to_vec_pretty(state)?;
        self.write_tx(move |db| {
            db.execute(
                "INSERT INTO storage_stats_state (singleton, state_json)
                 VALUES (1, ?1)
                 ON CONFLICT(singleton) DO UPDATE SET state_json = excluded.state_json",
                params![payload],
            )?;
            Ok(())
        })
        .await
    }

    async fn load_storage_locations(&self) -> Result<Vec<StorageLocationRecord>> {
        self.read(|db| {
            let mut statement = db.prepare(
                "SELECT content_kind, content_hash, path_id, size_bytes, state
                 FROM storage_locations",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, u64>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?;
            let mut locations = Vec::new();
            for row in rows {
                let (kind, hash, path_id, size_bytes, state) = row?;
                locations.push(StorageLocationRecord {
                    kind: StorageContentKind::from_str(&kind)?,
                    hash,
                    path_id,
                    size_bytes,
                    state: StorageLocationState::from_str(&state)?,
                });
            }
            Ok(locations)
        })
        .await
    }

    async fn persist_storage_location(&self, location: &StorageLocationRecord) -> Result<()> {
        let location = location.clone();
        self.write(move |db| {
            db.execute(
                "INSERT INTO storage_locations
                     (content_kind, content_hash, path_id, size_bytes, state)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(content_kind, content_hash) DO UPDATE SET
                     path_id = excluded.path_id,
                     size_bytes = excluded.size_bytes,
                     state = excluded.state",
                params![
                    location.kind.as_str(),
                    location.hash,
                    location.path_id,
                    location.size_bytes,
                    location.state.as_str(),
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn delete_storage_location(&self, kind: StorageContentKind, hash: &str) -> Result<()> {
        let hash = hash.to_string();
        self.write(move |db| {
            db.execute(
                "DELETE FROM storage_locations WHERE content_kind = ?1 AND content_hash = ?2",
                params![kind.as_str(), hash],
            )?;
            Ok(())
        })
        .await
    }

    async fn load_cached_chunk_record(&self, hash: &str) -> Result<Option<CachedChunkRecord>> {
        let hash = hash.to_string();
        let payload = self
            .read(move |db| {
                db.query_row(
                    "SELECT record_json
                     FROM cached_chunks
                     WHERE hash = ?1",
                    params![hash],
                    |row| row.get::<_, Vec<u8>>(0),
                )
                .optional()
                .map_err(Into::into)
            })
            .await?;
        match payload {
            Some(payload) => serde_json::from_slice::<CachedChunkRecord>(&payload)
                .map(Some)
                .context("invalid cached chunk record in sqlite"),
            None => Ok(None),
        }
    }

    async fn persist_cached_chunk_record(&self, record: &CachedChunkRecord) -> Result<()> {
        let payload = serde_json::to_vec_pretty(record)?;
        let hash = record.hash.clone();
        self.write(move |db| {
            db.execute(
                "INSERT INTO cached_chunks (hash, record_json)
                 VALUES (?1, ?2)
                 ON CONFLICT(hash) DO UPDATE SET record_json = excluded.record_json",
                params![hash, payload],
            )?;
            Ok(())
        })
        .await
    }

    async fn delete_cached_chunk_record(&self, hash: &str) -> Result<()> {
        let hash = hash.to_string();
        self.write(move |db| {
            db.execute("DELETE FROM cached_chunks WHERE hash = ?1", params![hash])?;
            Ok(())
        })
        .await
    }

    async fn list_cached_chunk_records(&self) -> Result<Vec<CachedChunkRecord>> {
        self.read(|db| {
            let mut stmt = db.prepare(
                "SELECT record_json
                 FROM cached_chunks
                 ORDER BY hash ASC",
            )?;
            let rows = stmt.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
            let mut records = Vec::new();
            for row in rows {
                let payload = row?;
                records.push(
                    serde_json::from_slice::<CachedChunkRecord>(&payload)
                        .context("invalid cached chunk record in sqlite")?,
                );
            }
            Ok(records)
        })
        .await
    }

    async fn mark_manifest_locally_owned(
        &self,
        manifest_hash: &str,
        owned_at_unix: u64,
    ) -> Result<()> {
        let manifest_hash = manifest_hash.to_string();
        self.write(move |db| {
            db.execute(
                "INSERT INTO locally_owned_manifests (manifest_hash, owned_at_unix)
                 VALUES (?1, ?2)
                 ON CONFLICT(manifest_hash) DO UPDATE SET owned_at_unix = excluded.owned_at_unix",
                params![manifest_hash, u64_to_i64(owned_at_unix)?],
            )?;
            Ok(())
        })
        .await
    }

    async fn delete_locally_owned_manifest(&self, manifest_hash: &str) -> Result<()> {
        let manifest_hash = manifest_hash.to_string();
        self.write(move |db| {
            db.execute(
                "DELETE FROM locally_owned_manifests WHERE manifest_hash = ?1",
                params![manifest_hash],
            )?;
            Ok(())
        })
        .await
    }

    async fn list_locally_owned_manifests(&self) -> Result<Vec<String>> {
        self.read(|db| {
            let mut stmt = db.prepare(
                "SELECT manifest_hash
                 FROM locally_owned_manifests
                 ORDER BY manifest_hash ASC",
            )?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            let mut manifests = Vec::new();
            for row in rows {
                manifests.push(row?);
            }
            Ok(manifests)
        })
        .await
    }

    async fn filter_locally_owned_manifests(
        &self,
        manifest_hashes: &[String],
    ) -> Result<std::collections::HashSet<String>> {
        const SQLITE_LOCALLY_OWNED_QUERY_BATCH_SIZE: usize = 500;

        if manifest_hashes.is_empty() {
            return Ok(std::collections::HashSet::new());
        }

        let manifest_hashes = manifest_hashes.to_vec();
        self.read(move |db| {
            let mut owned = std::collections::HashSet::with_capacity(manifest_hashes.len());
            for chunk in manifest_hashes.chunks(SQLITE_LOCALLY_OWNED_QUERY_BATCH_SIZE) {
                if chunk.is_empty() {
                    continue;
                }

                let placeholders = std::iter::repeat_n("?", chunk.len())
                    .collect::<Vec<_>>()
                    .join(", ");
                let query = format!(
                    "SELECT manifest_hash
                     FROM locally_owned_manifests
                     WHERE manifest_hash IN ({placeholders})"
                );
                let mut stmt = db.prepare(&query)?;
                let rows = stmt.query_map(params_from_iter(chunk.iter()), |row| {
                    row.get::<_, String>(0)
                })?;

                for row in rows {
                    owned.insert(row?);
                }
            }

            Ok(owned)
        })
        .await
    }

    async fn load_current_storage_stats(&self) -> Result<Option<StorageStatsSample>> {
        let payload = self
            .read(|db| {
                db.query_row(
                    "SELECT sample_json
                     FROM storage_stats_current
                     WHERE singleton = 1",
                    [],
                    |row| row.get::<_, Vec<u8>>(0),
                )
                .optional()
                .map_err(Into::into)
            })
            .await?;
        match payload {
            Some(payload) => serde_json::from_slice::<StorageStatsSample>(&payload)
                .map(Some)
                .context("invalid current storage stats in sqlite"),
            None => Ok(None),
        }
    }

    async fn list_storage_stats_history(
        &self,
        limit: Option<usize>,
        collected_since_unix: Option<u64>,
    ) -> Result<Vec<StorageStatsSample>> {
        let mut query = String::from(
            "SELECT sample_json
             FROM storage_stats_history",
        );
        let mut query_params = Vec::<Value>::new();
        if let Some(collected_since_unix) = collected_since_unix {
            query.push_str("\n             WHERE collected_at_unix >= ?1");
            query_params.push(Value::Integer(
                u64_to_i64(collected_since_unix)
                    .context("storage stats since timestamp overflow")?,
            ));
        }
        query.push_str("\n             ORDER BY collected_at_unix DESC, rowid DESC");
        if let Some(limit) = limit {
            let placeholder_index = query_params.len() + 1;
            query.push_str(&format!("\n             LIMIT ?{placeholder_index}"));
            query_params.push(Value::Integer(
                i64::try_from(limit).context("storage stats history limit overflow")?,
            ));
        }

        self.read(move |db| {
            let mut stmt = db.prepare(&query)?;
            let rows = stmt.query_map(params_from_iter(query_params), |row| {
                row.get::<_, Vec<u8>>(0)
            })?;
            let mut samples = Vec::new();
            for row in rows {
                let payload = row?;
                samples.push(
                    serde_json::from_slice::<StorageStatsSample>(&payload)
                        .context("invalid storage stats history sample in sqlite")?,
                );
            }
            Ok(samples)
        })
        .await
    }

    async fn load_metadata_db_logical_breakdown(
        &self,
        progress: Option<MetadataDbLogicalProgressCallback>,
    ) -> Result<Vec<MetadataDbTableLogicalBreakdown>> {
        let metadata_db_path = self.metadata_db_path.clone();
        tokio::task::spawn_blocking(move || -> Result<Vec<MetadataDbTableLogicalBreakdown>> {
            let db = Connection::open(&metadata_db_path).with_context(|| {
                format!(
                    "failed to open logical distribution sqlite connection for {}",
                    metadata_db_path.display()
                )
            })?;
            db.busy_timeout(SQLITE_METADATA_BUSY_TIMEOUT)
                .with_context(|| {
                    format!(
                        "failed to configure logical distribution sqlite busy timeout for {}",
                        metadata_db_path.display()
                    )
                })?;
            load_metadata_db_logical_breakdown_from_db(&db, progress)
        })
        .await
        .context("logical distribution sqlite worker join failure")?
    }

    async fn persist_storage_stats_sample(&self, sample: &StorageStatsSample) -> Result<()> {
        let payload = serde_json::to_vec_pretty(sample)?;
        let collected_at_unix = sample.collected_at_unix;
        self.write_tx(move |db| {
            db.execute(
                "INSERT INTO storage_stats_current (singleton, sample_json)
                 VALUES (1, ?1)
                 ON CONFLICT(singleton) DO UPDATE SET sample_json = excluded.sample_json",
                params![payload.clone()],
            )?;
            db.execute(
                "INSERT INTO storage_stats_history (collected_at_unix, sample_json)
                 VALUES (?1, ?2)",
                params![u64_to_i64(collected_at_unix)?, payload],
            )?;
            Ok(())
        })
        .await
    }

    async fn prune_storage_stats_history_before(&self, collected_before_unix: u64) -> Result<()> {
        self.write_tx(move |db| {
            db.execute(
                "DELETE FROM storage_stats_history
                 WHERE collected_at_unix < ?1",
                params![u64_to_i64(collected_before_unix)?],
            )?;
            Ok(())
        })
        .await
    }

    async fn has_version_index(&self, object_id: &str) -> Result<bool> {
        let object_id = object_id.to_string();
        self.read(move |db| {
            Ok(db
                .query_row(
                    "SELECT 1 FROM version_indexes WHERE object_id = ?1",
                    params![object_id],
                    |_row| Ok(()),
                )
                .optional()?
                .is_some())
        })
        .await
    }

    async fn delete_version_index_by_object_id(&self, object_id: &str) -> Result<()> {
        let object_id = object_id.to_string();
        self.write(move |db| {
            db.execute(
                "DELETE FROM version_indexes WHERE object_id = ?1",
                params![object_id],
            )?;
            Ok(())
        })
        .await
    }

    async fn list_media_cache_fingerprints(&self) -> Result<Vec<String>> {
        self.read(|db| {
            let mut stmt = db.prepare(
                "SELECT content_fingerprint
                 FROM media_cache
                 ORDER BY content_fingerprint",
            )?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            let mut fingerprints = Vec::new();
            for row in rows {
                fingerprints.push(row?);
            }
            Ok(fingerprints)
        })
        .await
    }

    async fn has_reconcile_marker(
        &self,
        source_node_id: &str,
        key: &str,
        source_version_id: &str,
    ) -> Result<bool> {
        let source_node_id = source_node_id.to_string();
        let key = key.to_string();
        let source_version_id = source_version_id.to_string();
        self.read(move |db| {
            Ok(db
                .query_row(
                    "SELECT 1
                     FROM reconcile_markers
                     WHERE source_node_id = ?1 AND key = ?2 AND source_version_id = ?3",
                    params![source_node_id, key, source_version_id],
                    |_row| Ok(()),
                )
                .optional()?
                .is_some())
        })
        .await
    }

    async fn mark_reconciled(&self, marker: &ReconcileMarker) -> Result<()> {
        let source_node_id = marker.source_node_id.clone();
        let key = marker.key.clone();
        let source_version_id = marker.source_version_id.clone();
        let local_version_id = marker.local_version_id.clone();
        let imported_at_unix = marker.imported_at_unix;
        self.write(move |db| {
            db.execute(
                "INSERT INTO reconcile_markers (
                     source_node_id,
                     key,
                     source_version_id,
                     local_version_id,
                     imported_at_unix
                 ) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(source_node_id, key, source_version_id) DO UPDATE
                 SET local_version_id = excluded.local_version_id,
                     imported_at_unix = excluded.imported_at_unix",
                params![
                    source_node_id,
                    key,
                    source_version_id,
                    local_version_id,
                    u64_to_i64(imported_at_unix)?
                ],
            )?;
            Ok(())
        })
        .await
    }
}

fn configure_metadata_db_connection(db: &Connection) -> Result<()> {
    db.busy_timeout(SQLITE_METADATA_BUSY_TIMEOUT)
        .context("failed to configure sqlite metadata busy timeout")?;
    db.execute_batch(
        "
        PRAGMA synchronous = NORMAL;
        PRAGMA foreign_keys = ON;
        ",
    )?;
    Ok(())
}

fn configure_read_only_metadata_db_connection(db: &mut Connection) -> Result<()> {
    configure_metadata_db_connection(db)?;
    db.execute_batch("PRAGMA query_only = ON;")?;
    Ok(())
}

fn init_metadata_db(db: &Connection) -> Result<()> {
    configure_metadata_db_connection(db)?;
    db.execute_batch(
        "
        PRAGMA journal_mode = WAL;
        PRAGMA query_only = OFF;

        CREATE TABLE IF NOT EXISTS metadata_meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS current_objects (
            key TEXT PRIMARY KEY,
            manifest_hash TEXT NOT NULL,
            object_id TEXT NOT NULL
        );

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
            labels_json TEXT NOT NULL DEFAULT '[]'
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

        CREATE TABLE IF NOT EXISTS version_indexes (
            object_id TEXT PRIMARY KEY,
            index_json BLOB NOT NULL
        );

        CREATE TABLE IF NOT EXISTS snapshots (
            snapshot_id TEXT PRIMARY KEY,
            created_at_unix INTEGER NOT NULL,
            object_count INTEGER NOT NULL,
            snapshot_json BLOB NOT NULL
        );

        CREATE TABLE IF NOT EXISTS snapshot_batch_state (
            singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
            state_json BLOB NOT NULL
        );

        CREATE TABLE IF NOT EXISTS storage_stats_current (
            singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
            sample_json BLOB NOT NULL
        );

        CREATE TABLE IF NOT EXISTS storage_stats_state (
            singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
            state_json BLOB NOT NULL
        );

        CREATE TABLE IF NOT EXISTS storage_stats_history (
            collected_at_unix INTEGER NOT NULL,
            sample_json BLOB NOT NULL
        );

        CREATE TABLE IF NOT EXISTS storage_locations (
            content_kind TEXT NOT NULL,
            content_hash TEXT NOT NULL,
            path_id TEXT NOT NULL,
            size_bytes INTEGER NOT NULL,
            state TEXT NOT NULL,
            PRIMARY KEY (content_kind, content_hash)
        );

        CREATE TABLE IF NOT EXISTS repair_attempts (
            subject TEXT PRIMARY KEY,
            attempts INTEGER NOT NULL,
            last_failure_unix INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS repair_run_history (
            run_id TEXT PRIMARY KEY,
            finished_at_unix INTEGER NOT NULL,
            record_json BLOB NOT NULL
        );

        CREATE TABLE IF NOT EXISTS manual_repair_action_run_history (
            run_id TEXT PRIMARY KEY,
            finished_at_unix INTEGER NOT NULL,
            record_json BLOB NOT NULL
        );

        CREATE TABLE IF NOT EXISTS data_scrub_run_history (
            run_id TEXT PRIMARY KEY,
            finished_at_unix INTEGER NOT NULL,
            record_json BLOB NOT NULL
        );

        CREATE TABLE IF NOT EXISTS cluster_nodes (
            node_id TEXT PRIMARY KEY,
            descriptor_json BLOB NOT NULL
        );

        CREATE TABLE IF NOT EXISTS cluster_replicas (
            subject TEXT NOT NULL,
            node_id TEXT NOT NULL,
            PRIMARY KEY(subject, node_id)
        );

        CREATE TABLE IF NOT EXISTS client_credential_state (
            singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
            state_json BLOB NOT NULL
        );

        CREATE TABLE IF NOT EXISTS s3_buckets (
            bucket_name TEXT PRIMARY KEY,
            root_prefix TEXT NOT NULL,
            versioning_status TEXT NOT NULL,
            read_only INTEGER NOT NULL,
            created_at_unix INTEGER NOT NULL,
            updated_at_unix INTEGER NOT NULL,
            created_by TEXT,
            deleted_at_unix INTEGER
        );

        CREATE TABLE IF NOT EXISTS s3_access_keys (
            access_key_id TEXT PRIMARY KEY,
            secret_material TEXT NOT NULL,
            description TEXT,
            bucket_scope_json BLOB NOT NULL,
            prefix_scope_json BLOB NOT NULL,
            allow_list INTEGER NOT NULL,
            allow_read INTEGER NOT NULL,
            allow_write INTEGER NOT NULL,
            allow_delete INTEGER NOT NULL,
            allow_manage INTEGER NOT NULL DEFAULT 0,
            created_at_unix INTEGER NOT NULL,
            updated_at_unix INTEGER NOT NULL,
            last_used_at_unix INTEGER,
            revoked_at_unix INTEGER
        );

        CREATE TABLE IF NOT EXISTS object_version_metadata (
            version_id TEXT PRIMARY KEY,
            content_type TEXT,
            content_encoding TEXT,
            content_language TEXT,
            cache_control TEXT,
            content_disposition TEXT,
            user_metadata_json BLOB NOT NULL,
            checksum_sha256 TEXT,
            checksum_crc32c TEXT,
            updated_at_unix INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS s3_object_versions (
            bucket_name TEXT NOT NULL,
            ironmesh_key TEXT NOT NULL,
            version_id TEXT NOT NULL,
            etag TEXT NOT NULL,
            multipart_part_count INTEGER,
            created_at_unix INTEGER NOT NULL,
            PRIMARY KEY(bucket_name, version_id)
        );

        CREATE TABLE IF NOT EXISTS admin_audit_events (
            event_id TEXT PRIMARY KEY,
            created_at_unix INTEGER NOT NULL,
            event_json BLOB NOT NULL
        );

        CREATE TABLE IF NOT EXISTS data_change_events (
            event_id TEXT PRIMARY KEY,
            created_at_unix INTEGER NOT NULL,
            action TEXT NOT NULL,
            path TEXT NOT NULL,
            from_path TEXT,
            to_path TEXT,
            actor_kind TEXT NOT NULL,
            actor_id TEXT,
            actor_label TEXT,
            actor_credential_fingerprint TEXT,
            event_json BLOB NOT NULL
        );

        CREATE TABLE IF NOT EXISTS media_cache (
            content_fingerprint TEXT PRIMARY KEY,
            metadata_json BLOB NOT NULL
        );

        CREATE TABLE IF NOT EXISTS cached_chunks (
            hash TEXT PRIMARY KEY,
            record_json BLOB NOT NULL
        );

        CREATE TABLE IF NOT EXISTS manifest_summaries (
            manifest_hash TEXT PRIMARY KEY,
            total_size_bytes INTEGER NOT NULL,
            content_fingerprint TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS locally_owned_manifests (
            manifest_hash TEXT PRIMARY KEY,
            owned_at_unix INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS reconcile_markers (
            source_node_id TEXT NOT NULL,
            key TEXT NOT NULL,
            source_version_id TEXT NOT NULL,
            local_version_id TEXT,
            imported_at_unix INTEGER NOT NULL,
            PRIMARY KEY(source_node_id, key, source_version_id)
        );

        CREATE INDEX IF NOT EXISTS idx_current_objects_object_id
            ON current_objects(object_id);
        CREATE INDEX IF NOT EXISTS idx_gallery_objects_media_order
            ON gallery_objects(media_type, captured_at_unix DESC, key ASC);
        CREATE INDEX IF NOT EXISTS idx_gallery_objects_manifest_hash
            ON gallery_objects(manifest_hash);
        CREATE INDEX IF NOT EXISTS idx_manifest_summaries_content_fingerprint
            ON manifest_summaries(content_fingerprint);
        CREATE INDEX IF NOT EXISTS idx_snapshots_created
            ON snapshots(created_at_unix DESC, snapshot_id DESC);
        CREATE INDEX IF NOT EXISTS idx_storage_stats_history_collected
            ON storage_stats_history(collected_at_unix DESC);
        CREATE INDEX IF NOT EXISTS idx_repair_run_history_finished
            ON repair_run_history(finished_at_unix DESC, run_id DESC);
        CREATE INDEX IF NOT EXISTS idx_manual_repair_action_run_history_finished
            ON manual_repair_action_run_history(finished_at_unix DESC, run_id DESC);
        CREATE INDEX IF NOT EXISTS idx_data_scrub_run_history_finished
            ON data_scrub_run_history(finished_at_unix DESC, run_id DESC);
        CREATE INDEX IF NOT EXISTS idx_admin_audit_created
            ON admin_audit_events(created_at_unix DESC, event_id DESC);
        CREATE INDEX IF NOT EXISTS idx_data_change_events_created
            ON data_change_events(created_at_unix DESC, event_id DESC);
        CREATE INDEX IF NOT EXISTS idx_data_change_events_action
            ON data_change_events(action, created_at_unix DESC, event_id DESC);
        CREATE INDEX IF NOT EXISTS idx_data_change_events_path
            ON data_change_events(path);
        CREATE INDEX IF NOT EXISTS idx_data_change_events_actor_id
            ON data_change_events(actor_id);
        CREATE INDEX IF NOT EXISTS idx_cluster_replicas_subject
            ON cluster_replicas(subject);
        CREATE INDEX IF NOT EXISTS idx_s3_object_versions_key
            ON s3_object_versions(bucket_name, ironmesh_key, created_at_unix DESC, version_id DESC);
        ",
    )?;
    add_sqlite_column_if_missing(db, "gallery_objects", "latitude", "REAL")?;
    add_sqlite_column_if_missing(db, "gallery_objects", "longitude", "REAL")?;
    add_sqlite_column_if_missing(
        db,
        "gallery_objects",
        GALLERY_LABELS_COLUMN,
        GALLERY_LABELS_COLUMN_DEFINITION,
    )?;
    add_sqlite_column_if_missing(
        db,
        "gallery_changes",
        "previous_inferred_media_type",
        "TEXT",
    )?;
    add_sqlite_column_if_missing(db, "gallery_changes", "previous_media_type", "TEXT")?;
    add_sqlite_column_if_missing(db, "gallery_changes", "previous_latitude", "REAL")?;
    add_sqlite_column_if_missing(db, "gallery_changes", "previous_longitude", "REAL")?;
    db.execute(
        "CREATE INDEX IF NOT EXISTS idx_gallery_objects_viewport
         ON gallery_objects(latitude, longitude, media_type, captured_at_unix DESC, key ASC)",
        [],
    )?;
    init_gallery_change_log(db)?;
    if let Err(err) = db.execute(
        "ALTER TABLE s3_access_keys ADD COLUMN allow_manage INTEGER NOT NULL DEFAULT 0",
        [],
    ) {
        let duplicate_column = err
            .to_string()
            .contains("duplicate column name: allow_manage");
        if !duplicate_column {
            return Err(err).context("failed to migrate sqlite s3_access_keys.allow_manage");
        }
    }

    let stored_version = db
        .query_row(
            "SELECT value FROM metadata_meta WHERE key = ?1",
            ["schema_version"],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .context("failed to read sqlite metadata schema version")?;

    let schema_version = match stored_version {
        Some(raw) => raw
            .parse::<i64>()
            .with_context(|| format!("invalid sqlite metadata schema version: {raw}"))?,
        None => METADATA_SCHEMA_VERSION_CURRENT,
    };

    if schema_version != METADATA_SCHEMA_VERSION_CURRENT {
        anyhow::bail!(
            "unsupported sqlite metadata schema version: {} (current={})",
            schema_version,
            METADATA_SCHEMA_VERSION_CURRENT
        );
    }

    db.execute(
        "INSERT INTO metadata_meta(key, value) VALUES(?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![
            "schema_version",
            METADATA_SCHEMA_VERSION_CURRENT.to_string()
        ],
    )
    .context("failed to persist sqlite metadata schema version")?;

    Ok(())
}

fn add_sqlite_column_if_missing(
    db: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<()> {
    let sql = format!("ALTER TABLE {table} ADD COLUMN {column} {definition}");
    if let Err(err) = db.execute(&sql, [])
        && !err.to_string().contains("duplicate column name")
    {
        return Err(err).with_context(|| format!("failed to add {table}.{column}"));
    }
    Ok(())
}

fn init_gallery_change_log(db: &Connection) -> Result<()> {
    db.execute(
        "INSERT INTO metadata_meta(key, value) VALUES('gallery_history_id', ?1)
         ON CONFLICT(key) DO NOTHING",
        params![Uuid::new_v4().to_string()],
    )?;
    db.execute(
        "INSERT INTO metadata_meta(key, value) VALUES('gallery_revision', '0')
         ON CONFLICT(key) DO NOTHING",
        [],
    )?;
    db.execute_batch(
        "DROP TRIGGER IF EXISTS gallery_objects_change_insert;
         DROP TRIGGER IF EXISTS gallery_objects_change_update;
         DROP TRIGGER IF EXISTS gallery_objects_change_delete;",
    )?;
    let trigger_sql = format!(
        "
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
        "
    );
    db.execute_batch(&trigger_sql)
        .context("failed to initialize sqlite gallery change log")
}

fn load_current_state_from_db(db: &Connection) -> Result<CurrentState> {
    let mut stmt = db.prepare(
        "SELECT key, manifest_hash, object_id
         FROM current_objects",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;

    let mut state = CurrentState::default();
    for row in rows {
        let (key, manifest_hash, object_id) = row?;
        state.objects.insert(key.clone(), manifest_hash);
        state.object_ids.insert(key, object_id);
    }
    Ok(state)
}

fn load_metadata_db_logical_breakdown_from_db(
    db: &Connection,
    progress: Option<MetadataDbLogicalProgressCallback>,
) -> Result<Vec<MetadataDbTableLogicalBreakdown>> {
    let specs = metadata_db_logical_table_specs();
    let mut tables = Vec::with_capacity(specs.len());
    for (index, spec) in specs.iter().enumerate() {
        if let Some(progress) = progress.as_ref() {
            progress(MetadataDbLogicalProgress {
                total_tables: specs.len(),
                completed_tables: index,
                current_table: Some(spec.table.to_string()),
            });
        }
        let query = metadata_db_logical_summary_query(*spec);
        let (row_count, tracked_value_bytes) = db.query_row(&query, [], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })?;
        let row_count = u64::try_from(row_count)
            .with_context(|| format!("negative row count reported for {}", spec.table))?;
        let tracked_value_bytes = u64::try_from(tracked_value_bytes)
            .with_context(|| format!("negative tracked value bytes reported for {}", spec.table))?;
        tables.push(MetadataDbTableLogicalBreakdown {
            table: spec.table.to_string(),
            row_count,
            tracked_value_bytes,
            average_tracked_value_bytes: tracked_value_bytes.checked_div(row_count),
            tracked_columns: spec
                .tracked_columns
                .iter()
                .map(|column| (*column).to_string())
                .collect(),
        });
    }

    if let Some(progress) = progress.as_ref() {
        progress(MetadataDbLogicalProgress {
            total_tables: specs.len(),
            completed_tables: specs.len(),
            current_table: None,
        });
    }

    Ok(tables)
}

fn u64_to_i64(value: u64) -> Result<i64> {
    i64::try_from(value).context("integer overflow converting u64 to i64")
}

fn usize_to_i64(value: usize) -> Result<i64> {
    i64::try_from(value).context("integer overflow converting usize to i64")
}

#[cfg(test)]
#[path = "sqlite_impl/gallery_tests.rs"]
mod gallery_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::MediaCacheStatus;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn sqlite_test_db_path(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "ironmesh-{name}-{}-{stamp}.sqlite",
            std::process::id()
        ))
    }

    #[test]
    fn init_metadata_db_persists_schema_version() {
        let db = Connection::open_in_memory().expect("in-memory sqlite should open");

        init_metadata_db(&db).expect("metadata schema should initialize");

        let schema_version: String = db
            .query_row(
                "SELECT value FROM metadata_meta WHERE key = ?1",
                ["schema_version"],
                |row| row.get(0),
            )
            .expect("schema version should persist");
        assert_eq!(schema_version, METADATA_SCHEMA_VERSION_CURRENT.to_string());
    }

    #[test]
    fn init_metadata_db_accepts_missing_legacy_schema_version() {
        let db = Connection::open_in_memory().expect("in-memory sqlite should open");

        init_metadata_db(&db).expect("metadata schema should initialize");
        db.execute(
            "DELETE FROM metadata_meta WHERE key = ?1",
            ["schema_version"],
        )
        .expect("schema version row should delete");

        init_metadata_db(&db).expect("legacy metadata schema should be accepted");

        let schema_version: String = db
            .query_row(
                "SELECT value FROM metadata_meta WHERE key = ?1",
                ["schema_version"],
                |row| row.get(0),
            )
            .expect("schema version should be restored");
        assert_eq!(schema_version, METADATA_SCHEMA_VERSION_CURRENT.to_string());
    }

    #[test]
    fn init_metadata_db_rejects_future_schema_version() {
        let db = Connection::open_in_memory().expect("in-memory sqlite should open");

        init_metadata_db(&db).expect("metadata schema should initialize");
        db.execute(
            "UPDATE metadata_meta SET value = ?2 WHERE key = ?1",
            params!["schema_version", "99"],
        )
        .expect("future schema version should write");

        let err = init_metadata_db(&db).expect_err("future schema version should fail");
        assert!(
            err.to_string()
                .contains("unsupported sqlite metadata schema version: 99")
        );
    }

    #[tokio::test]
    async fn conditional_invalid_media_cleanup_preserves_rewritten_row() {
        let metadata_db_path = sqlite_test_db_path("conditional-invalid-media-cleanup");
        let store = SqliteMetadataStore::open(&metadata_db_path)
            .await
            .expect("sqlite metadata store should open");
        let content_fingerprint = "fingerprint-1".to_string();
        let invalid_payload = br#"{"broken":true}"#.to_vec();
        let valid_payload = serde_json::to_vec_pretty(&CachedMediaMetadata {
            schema_version: crate::storage::media_cache::MEDIA_CACHE_SCHEMA_VERSION,
            content_fingerprint: content_fingerprint.clone(),
            source_manifest_hash: "manifest-1".to_string(),
            status: MediaCacheStatus::Ready,
            media_type: Some("image".to_string()),
            mime_type: Some("image/jpeg".to_string()),
            width: Some(64),
            height: Some(48),
            orientation: Some(1),
            taken_at_unix: Some(1),
            date_encoded_unix: None,
            duration_millis: None,
            frame_rate_millihertz: None,
            total_bitrate_bps: None,
            codec_name: None,
            codec_fourcc: None,
            gps: None,
            photo: None,
            thumbnail: None,
            source_size_bytes: 1024,
            generated_at_unix: 2,
            retry_after_unix: None,
            error: None,
        })
        .expect("valid media metadata should serialize");

        store
            .write({
                let content_fingerprint = content_fingerprint.clone();
                let invalid_payload = invalid_payload.clone();
                move |db| {
                    db.execute(
                        "INSERT INTO media_cache (content_fingerprint, metadata_json)
                         VALUES (?1, ?2)",
                        params![content_fingerprint, invalid_payload],
                    )?;
                    Ok(())
                }
            })
            .await
            .expect("invalid row should persist");

        let observed_payload = store
            .read({
                let content_fingerprint = content_fingerprint.clone();
                move |db| {
                    db.query_row(
                        "SELECT metadata_json FROM media_cache WHERE content_fingerprint = ?1",
                        params![content_fingerprint],
                        |row| row.get::<_, Vec<u8>>(0),
                    )
                    .map_err(Into::into)
                }
            })
            .await
            .expect("invalid row should load");
        assert!(
            serde_json::from_slice::<CachedMediaMetadata>(&observed_payload).is_err(),
            "fixture payload should remain invalid"
        );

        store
            .write({
                let content_fingerprint = content_fingerprint.clone();
                let valid_payload = valid_payload.clone();
                move |db| {
                    db.execute(
                        "INSERT INTO media_cache (content_fingerprint, metadata_json)
                         VALUES (?1, ?2)
                         ON CONFLICT(content_fingerprint) DO UPDATE SET metadata_json = excluded.metadata_json",
                        params![content_fingerprint, valid_payload],
                    )?;
                    Ok(())
                }
            })
            .await
            .expect("valid row should replace invalid row");

        let deleted = store
            .delete_invalid_media_cache_row_if_payload_matches(
                content_fingerprint.clone(),
                observed_payload,
            )
            .await
            .expect("conditional cleanup should succeed");
        assert!(!deleted, "cleanup should not delete rewritten row");

        let remaining_payload = store
            .read(move |db| {
                db.query_row(
                    "SELECT metadata_json FROM media_cache WHERE content_fingerprint = ?1",
                    params![content_fingerprint],
                    |row| row.get::<_, Vec<u8>>(0),
                )
                .map_err(Into::into)
            })
            .await
            .expect("rewritten row should remain present");
        assert_eq!(remaining_payload, valid_payload);

        let _ = std::fs::remove_file(metadata_db_path);
    }
}
