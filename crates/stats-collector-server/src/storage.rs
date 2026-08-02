//! Append-only raw storage for ingested hardware-reliability payloads.
//!
//! Per `docs/server-node-hardware-reliability-telemetry-strategy.md` Section 5.3, raw ingested
//! batches are stored separately from the public aggregate view: the public
//! `GET /v1/stats/summary` is computed via [`crate::aggregate`], while direct access to this raw
//! table is admin-token-guarded (the GDPR access/erasure endpoints in `lib.rs`).
//!
//! Backed by [`turso`], a pure-Rust, embedded SQLite-compatible engine — no C toolchain or
//! bundled `libsqlite3` is required to build this crate. This mirrors the pattern already used by
//! `server-node-sdk`'s optional Turso metadata backend
//! (`crates/server-node-sdk/src/storage/turso_impl.rs`): no external synchronization wrapper is
//! needed around [`turso::Connection`] (unlike `rusqlite::Connection`, it is safely shared across
//! concurrent async callers on its own).

use anyhow::{Context, Result, bail};

/// A single stored ingestion row, as inserted by [`IngestStorage::insert`].
///
/// `country_code` is whatever the configured [`crate::country::CountryResolver`] derived from the
/// request source IP at ingest time (doc Section 4.2). It is `None` with the default no-op
/// resolver; the raw source IP itself is never stored (see the struct docs below).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredRecord {
    pub id: i64,
    pub received_at_unix: i64,
    pub telemetry_subject_id: String,
    pub schema_version: u32,
    pub country_code: Option<String>,
    pub raw_payload_json: String,
}

/// Turso-backed append-only store for the `hardware_reliability_ingest` table.
///
/// Deliberately does *not* have a column for the request's source IP address: the project's
/// general "no IP addresses" data-minimization stance (Section 2.6) applies here too. The source
/// IP is only ever used transiently, in memory, for per-IP rate limiting (see `rate_limit.rs`) and
/// is never written to this table, logged, or otherwise persisted.
pub struct IngestStorage {
    _database: turso::Database,
    connection: turso::Connection,
}

impl IngestStorage {
    /// Opens (creating if necessary) the Turso database at `path` and ensures the ingestion table
    /// exists.
    pub async fn open(path: &str) -> Result<Self> {
        let database = turso::Builder::new_local(path)
            .build()
            .await
            .with_context(|| format!("failed to open turso db at {path}"))?;
        Self::from_database(database).await
    }

    /// Opens an in-memory database, primarily for tests.
    pub async fn open_in_memory() -> Result<Self> {
        let database = turso::Builder::new_local(":memory:")
            .build()
            .await
            .context("failed to open in-memory turso db")?;
        Self::from_database(database).await
    }

    async fn from_database(database: turso::Database) -> Result<Self> {
        let connection = database
            .connect()
            .context("failed to connect to turso db")?;
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS hardware_reliability_ingest (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    received_at_unix INTEGER NOT NULL,
                    telemetry_subject_id TEXT NOT NULL,
                    schema_version INTEGER NOT NULL,
                    country_code TEXT,
                    raw_payload_json TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_hardware_reliability_ingest_subject
                    ON hardware_reliability_ingest(telemetry_subject_id);
                CREATE TABLE IF NOT EXISTS ingestion_tokens (
                    telemetry_subject_id TEXT PRIMARY KEY,
                    token TEXT NOT NULL,
                    created_at_unix INTEGER NOT NULL
                );
                ",
            )
            .await
            .context("failed to initialize hardware_reliability_ingest schema")?;
        Ok(Self {
            _database: database,
            connection,
        })
    }

    /// Appends one raw ingestion record.
    ///
    /// `country_code` is the server-derived country (doc Section 4.2), or `None` when unknown / no
    /// resolver is configured. The request's raw source IP is deliberately never a parameter here.
    pub async fn insert(
        &self,
        received_at_unix: i64,
        telemetry_subject_id: &str,
        schema_version: u32,
        country_code: Option<&str>,
        raw_payload_json: &str,
    ) -> Result<i64> {
        self.connection
            .execute(
                "INSERT INTO hardware_reliability_ingest
                    (received_at_unix, telemetry_subject_id, schema_version, country_code, raw_payload_json)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                (
                    received_at_unix,
                    telemetry_subject_id,
                    i64::from(schema_version),
                    country_code,
                    raw_payload_json,
                ),
            )
            .await
            .context("failed to insert ingestion record")?;
        Ok(self.connection.last_insert_rowid())
    }

    /// Returns every stored row, most recent first. Used by the aggregation step (which dedupes to
    /// one current record per subject). Fine for the small fleet this service targets; a future
    /// large-scale deployment would replace this with a windowed/streamed aggregation query.
    pub async fn all_records(&self) -> Result<Vec<StoredRecord>> {
        let mut rows = self
            .connection
            .query(
                "SELECT id, received_at_unix, telemetry_subject_id, schema_version, country_code, raw_payload_json
                 FROM hardware_reliability_ingest
                 ORDER BY id DESC",
                (),
            )
            .await
            .context("failed to read ingestion records")?;
        let mut records = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .context("failed to read ingestion records")?
        {
            records.push(stored_record_from_row(&row)?);
        }
        Ok(records)
    }

    /// Deletes every row for a given `telemetry_subject_id` (GDPR erasure, doc Section 4.5).
    /// Returns the number of rows removed.
    ///
    /// Also revokes any registered ingestion token for this subject id (see `ingestion_tokens`
    /// below): once a subject's telemetry content is erased, there is no remaining purpose in
    /// keeping its registration credential around, and an operator who erases a subject and later
    /// re-registers under the same pseudonym (unlikely, but possible if they reused a salt) gets a
    /// fresh token rather than silently resuming the old one.
    pub async fn delete_subject(&self, telemetry_subject_id: &str) -> Result<usize> {
        let removed = self
            .connection
            .execute(
                "DELETE FROM hardware_reliability_ingest WHERE telemetry_subject_id = ?1",
                (telemetry_subject_id,),
            )
            .await
            .context("failed to delete records for subject")?;
        self.connection
            .execute(
                "DELETE FROM ingestion_tokens WHERE telemetry_subject_id = ?1",
                (telemetry_subject_id,),
            )
            .await
            .context("failed to revoke ingestion token for subject")?;
        usize::try_from(removed).context("deleted row count overflow")
    }

    /// Idempotently issues (or returns the already-registered) ingestion token for
    /// `telemetry_subject_id` (doc Section 5.2/8 registration handshake). `candidate_token` is
    /// only actually stored if no row exists yet for this subject id - the
    /// `INSERT ... ON CONFLICT DO NOTHING` followed by a `SELECT` makes concurrent first-time
    /// registrations for the same subject id race-safe: whichever insert wins, every caller reads
    /// back that same winning token, so a re-run operator or a node restarting before its first
    /// successful ingest never gets a silently invalidated token.
    pub async fn get_or_create_ingestion_token(
        &self,
        telemetry_subject_id: &str,
        created_at_unix: i64,
        candidate_token: &str,
    ) -> Result<String> {
        self.connection
            .execute(
                "INSERT INTO ingestion_tokens (telemetry_subject_id, token, created_at_unix)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(telemetry_subject_id) DO NOTHING",
                (telemetry_subject_id, candidate_token, created_at_unix),
            )
            .await
            .context("failed to insert ingestion token")?;
        let mut rows = self
            .connection
            .query(
                "SELECT token FROM ingestion_tokens WHERE telemetry_subject_id = ?1",
                (telemetry_subject_id,),
            )
            .await
            .context("failed to read ingestion token after insert")?;
        let row = rows
            .next()
            .await
            .context("failed to read ingestion token after insert")?
            .context("ingestion token missing immediately after insert")?;
        row_string(&row, 0, "ingestion_tokens.token")
    }

    /// Looks up the registered ingestion token for `telemetry_subject_id`, if any. Used by the
    /// ingest handler to validate an `X-Ironmesh-Ingestion-Token` header (doc Section 5.2/8).
    /// Never exposed via any admin/raw-record view (see the `ingestion_tokens` table doc comment
    /// above) - it is a bearer secret, not telemetry content.
    pub async fn token_for_subject(&self, telemetry_subject_id: &str) -> Result<Option<String>> {
        let mut rows = self
            .connection
            .query(
                "SELECT token FROM ingestion_tokens WHERE telemetry_subject_id = ?1",
                (telemetry_subject_id,),
            )
            .await
            .context("failed to read ingestion token")?;
        let Some(row) = rows
            .next()
            .await
            .context("failed to read ingestion token")?
        else {
            return Ok(None);
        };
        Ok(Some(row_string(&row, 0, "ingestion_tokens.token")?))
    }

    /// Deletes raw rows older than `cutoff_unix` (retention enforcement, doc Section 4.6). Returns
    /// the number of rows removed.
    pub async fn delete_older_than(&self, cutoff_unix: i64) -> Result<usize> {
        let removed = self
            .connection
            .execute(
                "DELETE FROM hardware_reliability_ingest WHERE received_at_unix < ?1",
                (cutoff_unix,),
            )
            .await
            .context("failed to prune expired records")?;
        usize::try_from(removed).context("pruned row count overflow")
    }

    /// Returns the total number of stored rows. Primarily useful for tests.
    pub async fn count(&self) -> Result<i64> {
        let mut rows = self
            .connection
            .query("SELECT COUNT(*) FROM hardware_reliability_ingest", ())
            .await
            .context("failed to count ingestion records")?;
        let row = rows
            .next()
            .await
            .context("failed to count ingestion records")?
            .context("count query returned no rows")?;
        row_i64(&row, 0, "COUNT(*)")
    }

    /// Fetches all rows for a given `telemetry_subject_id`, most recent first. Primarily useful
    /// for tests and for a future admin/erasure endpoint (Section 4.5).
    pub async fn records_for_subject(
        &self,
        telemetry_subject_id: &str,
    ) -> Result<Vec<StoredRecord>> {
        let mut rows = self
            .connection
            .query(
                "SELECT id, received_at_unix, telemetry_subject_id, schema_version, country_code, raw_payload_json
                 FROM hardware_reliability_ingest
                 WHERE telemetry_subject_id = ?1
                 ORDER BY id DESC",
                (telemetry_subject_id,),
            )
            .await
            .context("failed to read ingestion records")?;
        let mut records = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .context("failed to read ingestion records")?
        {
            records.push(stored_record_from_row(&row)?);
        }
        Ok(records)
    }
}

fn stored_record_from_row(row: &turso::Row) -> Result<StoredRecord> {
    Ok(StoredRecord {
        id: row_i64(row, 0, "hardware_reliability_ingest.id")?,
        received_at_unix: row_i64(row, 1, "hardware_reliability_ingest.received_at_unix")?,
        telemetry_subject_id: row_string(
            row,
            2,
            "hardware_reliability_ingest.telemetry_subject_id",
        )?,
        schema_version: u32::try_from(row_i64(
            row,
            3,
            "hardware_reliability_ingest.schema_version",
        )?)
        .context("schema_version out of range")?,
        country_code: row_opt_string(row, 4, "hardware_reliability_ingest.country_code")?,
        raw_payload_json: row_string(row, 5, "hardware_reliability_ingest.raw_payload_json")?,
    })
}

fn row_string(row: &turso::Row, idx: usize, label: &str) -> Result<String> {
    match row.get_value(idx)? {
        turso::Value::Text(value) => Ok(value),
        turso::Value::Blob(value) => {
            String::from_utf8(value).with_context(|| format!("invalid utf-8 in {label}"))
        }
        other => bail!("expected text value for {label}, got {other:?}"),
    }
}

fn row_opt_string(row: &turso::Row, idx: usize, label: &str) -> Result<Option<String>> {
    match row.get_value(idx)? {
        turso::Value::Null => Ok(None),
        turso::Value::Text(value) => Ok(Some(value)),
        turso::Value::Blob(value) => Ok(Some(
            String::from_utf8(value).with_context(|| format!("invalid utf-8 in {label}"))?,
        )),
        other => bail!("expected optional text value for {label}, got {other:?}"),
    }
}

fn row_i64(row: &turso::Row, idx: usize, label: &str) -> Result<i64> {
    match row.get_value(idx)? {
        turso::Value::Integer(value) => Ok(value),
        other => bail!("expected integer value for {label}, got {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn insert_and_count_round_trips() {
        let storage = IngestStorage::open_in_memory()
            .await
            .expect("storage should open");
        assert_eq!(storage.count().await.expect("count should succeed"), 0);

        storage
            .insert(
                1_752_912_000,
                "subject-a",
                1,
                None,
                "{\"schema_version\":1}",
            )
            .await
            .expect("insert should succeed");
        assert_eq!(storage.count().await.expect("count should succeed"), 1);

        let records = storage
            .records_for_subject("subject-a")
            .await
            .expect("query should succeed");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].telemetry_subject_id, "subject-a");
        assert_eq!(records[0].schema_version, 1);
        assert_eq!(records[0].country_code, None);
    }

    #[tokio::test]
    async fn insert_persists_resolved_country_code() {
        let storage = IngestStorage::open_in_memory()
            .await
            .expect("storage should open");
        storage
            .insert(1_752_912_000, "subject-de", 1, Some("DE"), "{}")
            .await
            .expect("insert should succeed");
        let records = storage
            .records_for_subject("subject-de")
            .await
            .expect("query should succeed");
        assert_eq!(records[0].country_code.as_deref(), Some("DE"));
    }

    #[tokio::test]
    async fn delete_subject_removes_only_that_subject() {
        let storage = IngestStorage::open_in_memory()
            .await
            .expect("storage should open");
        storage.insert(1, "keep", 1, None, "{}").await.unwrap();
        storage.insert(2, "erase", 1, None, "{}").await.unwrap();
        storage.insert(3, "erase", 1, None, "{}").await.unwrap();

        let removed = storage
            .delete_subject("erase")
            .await
            .expect("delete should succeed");
        assert_eq!(removed, 2);
        assert_eq!(storage.count().await.unwrap(), 1);
        assert!(
            storage
                .records_for_subject("erase")
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(storage.records_for_subject("keep").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn delete_older_than_prunes_by_timestamp() {
        let storage = IngestStorage::open_in_memory()
            .await
            .expect("storage should open");
        storage.insert(100, "old", 1, None, "{}").await.unwrap();
        storage.insert(200, "new", 1, None, "{}").await.unwrap();

        let removed = storage
            .delete_older_than(150)
            .await
            .expect("prune should succeed");
        assert_eq!(removed, 1);
        assert_eq!(storage.count().await.unwrap(), 1);
        assert!(storage.records_for_subject("old").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn get_or_create_ingestion_token_is_idempotent() {
        let storage = IngestStorage::open_in_memory()
            .await
            .expect("storage should open");
        let first = storage
            .get_or_create_ingestion_token("subject-a", 1_000, "candidate-1")
            .await
            .expect("first registration should succeed");
        assert_eq!(first, "candidate-1");

        // A second registration attempt for the same subject id must return the *same* token,
        // even though it offers a different candidate - an operator re-running setup, or a node
        // restarting before its first successful ingest, must not get a silently invalidated
        // token.
        let second = storage
            .get_or_create_ingestion_token("subject-a", 2_000, "candidate-2")
            .await
            .expect("second registration should succeed");
        assert_eq!(second, first);
    }

    #[tokio::test]
    async fn get_or_create_ingestion_token_is_independent_per_subject() {
        let storage = IngestStorage::open_in_memory()
            .await
            .expect("storage should open");
        let a = storage
            .get_or_create_ingestion_token("subject-a", 1_000, "token-a")
            .await
            .unwrap();
        let b = storage
            .get_or_create_ingestion_token("subject-b", 1_000, "token-b")
            .await
            .unwrap();
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn token_for_subject_returns_none_when_unregistered() {
        let storage = IngestStorage::open_in_memory()
            .await
            .expect("storage should open");
        assert_eq!(
            storage.token_for_subject("never-registered").await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn token_for_subject_returns_the_registered_token() {
        let storage = IngestStorage::open_in_memory()
            .await
            .expect("storage should open");
        storage
            .get_or_create_ingestion_token("subject-a", 1_000, "token-a")
            .await
            .unwrap();
        assert_eq!(
            storage.token_for_subject("subject-a").await.unwrap(),
            Some("token-a".to_string())
        );
    }

    #[tokio::test]
    async fn delete_subject_also_revokes_the_ingestion_token() {
        let storage = IngestStorage::open_in_memory()
            .await
            .expect("storage should open");
        storage.insert(1, "subject-a", 1, None, "{}").await.unwrap();
        storage
            .get_or_create_ingestion_token("subject-a", 1_000, "token-a")
            .await
            .unwrap();
        assert!(
            storage
                .token_for_subject("subject-a")
                .await
                .unwrap()
                .is_some()
        );

        storage.delete_subject("subject-a").await.unwrap();

        assert_eq!(storage.token_for_subject("subject-a").await.unwrap(), None);
    }
}
