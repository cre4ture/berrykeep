//! Durable repair intent and GC-safe content installation. No namespace mutations.
use super::retained_content::RetainedReference;
use super::*;

#[cfg(test)]
use std::sync::Mutex as StdMutex;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(test)]
use tokio::sync::Notify;

#[cfg(test)]
pub(crate) struct RecoveryVerificationBlocker {
    started: Arc<Notify>,
    release: Arc<Notify>,
    released: std::sync::atomic::AtomicBool,
    calls: AtomicUsize,
}

#[cfg(test)]
impl RecoveryVerificationBlocker {
    pub(crate) async fn wait_until_started(&self) {
        self.started.notified().await;
    }

    pub(crate) fn release(&self) {
        self.released.store(true, Ordering::Release);
        self.release.notify_waiters();
    }

    pub(crate) fn calls(&self) -> usize {
        self.calls.load(Ordering::Acquire)
    }
}

#[cfg(test)]
fn recovery_verification_blockers()
-> &'static StdMutex<HashMap<String, Arc<RecoveryVerificationBlocker>>> {
    static BLOCKERS: std::sync::OnceLock<
        StdMutex<HashMap<String, Arc<RecoveryVerificationBlocker>>>,
    > = std::sync::OnceLock::new();
    BLOCKERS.get_or_init(|| StdMutex::new(HashMap::new()))
}

#[cfg(test)]
pub(crate) fn block_recovery_verification_for_test(
    manifest_hash: &str,
) -> Arc<RecoveryVerificationBlocker> {
    let blocker = Arc::new(RecoveryVerificationBlocker {
        started: Arc::new(Notify::new()),
        release: Arc::new(Notify::new()),
        released: std::sync::atomic::AtomicBool::new(false),
        calls: AtomicUsize::new(0),
    });
    recovery_verification_blockers()
        .lock()
        .expect("recovery verification blocker lock should not be poisoned")
        .insert(manifest_hash.to_string(), blocker.clone());
    blocker
}

#[cfg(test)]
pub(crate) fn clear_recovery_verification_blocker_for_test(manifest_hash: &str) {
    recovery_verification_blockers()
        .lock()
        .expect("recovery verification blocker lock should not be poisoned")
        .remove(manifest_hash);
}

#[cfg(test)]
async fn wait_for_recovery_verification_test_blocker(manifest_hash: &str) {
    let blocker = recovery_verification_blockers()
        .lock()
        .expect("recovery verification blocker lock should not be poisoned")
        .get(manifest_hash)
        .cloned();
    if let Some(blocker) = blocker {
        blocker.calls.fetch_add(1, Ordering::AcqRel);
        blocker.started.notify_one();
        loop {
            let notified = blocker.release.notified();
            if blocker.released.load(Ordering::Acquire) {
                break;
            }
            notified.await;
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PendingReplicationImport {
    key: String,
    object_id: Option<String>,
    version_id: Option<String>,
    logical_path: Option<String>,
    parent_version_ids: Vec<String>,
    state: VersionConsistencyState,
    created_at_unix: Option<u64>,
    copied_from_object_id: Option<String>,
    copied_from_version_id: Option<String>,
    copied_from_path: Option<String>,
    selected_is_preferred_head: bool,
}

impl PendingReplicationImport {
    pub(crate) fn from_bundle(bundle: &ReplicationExportBundle) -> Self {
        Self {
            key: bundle.key.clone(),
            object_id: bundle.object_id.clone(),
            version_id: bundle.version_id.clone(),
            logical_path: bundle.logical_path.clone(),
            parent_version_ids: bundle.parent_version_ids.clone(),
            state: bundle.state.clone(),
            created_at_unix: bundle.created_at_unix,
            copied_from_object_id: bundle.copied_from_object_id.clone(),
            copied_from_version_id: bundle.copied_from_version_id.clone(),
            copied_from_path: bundle.copied_from_path.clone(),
            selected_is_preferred_head: bundle.selected_is_preferred_head,
        }
    }

    pub(crate) fn restore_bundle(
        &self,
        manifest_hash: String,
        manifest_bytes: Vec<u8>,
    ) -> Result<ReplicationExportBundle> {
        let manifest = validate_manifest(&manifest_hash, &manifest_bytes)?;
        Ok(ReplicationExportBundle {
            key: self.key.clone(),
            object_id: self.object_id.clone(),
            version_id: self.version_id.clone(),
            logical_path: self.logical_path.clone(),
            parent_version_ids: self.parent_version_ids.clone(),
            state: self.state.clone(),
            created_at_unix: self.created_at_unix,
            copied_from_object_id: self.copied_from_object_id.clone(),
            copied_from_version_id: self.copied_from_version_id.clone(),
            copied_from_path: self.copied_from_path.clone(),
            selected_is_preferred_head: self.selected_is_preferred_head,
            manifest_hash,
            manifest_bytes,
            manifest,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ContentRepairTask {
    pub reference: RetainedReference,
    pub repair_chunks: bool,
    pub chunks: Vec<ReplicationChunkInfo>,
    pub attempts: u32,
    pub next_attempt_unix: u64,
    #[serde(default)]
    pub last_attempt_unix: u64,
    pub last_error: Option<String>,
    #[serde(default)]
    pub waiting_for_source: bool,
    #[serde(default)]
    pub source_fingerprint: String,
    #[serde(default)]
    pub recovered_chunks: usize,
    /// A replication pull persists this intent before downloading chunks. If
    /// the foreground pull is cancelled, the durable worker can finish both
    /// content recovery and the namespace import instead of treating the pin
    /// as stale merely because the version is not locally referenced yet.
    #[serde(default)]
    pub pending_replication_import: Option<Box<PendingReplicationImport>>,
}

impl ContentRepairTask {
    pub fn new(reference: RetainedReference, repair_chunks: bool) -> Self {
        Self {
            reference,
            repair_chunks,
            chunks: Vec::new(),
            attempts: 0,
            next_attempt_unix: 0,
            last_attempt_unix: 0,
            last_error: None,
            waiting_for_source: false,
            source_fingerprint: String::new(),
            recovered_chunks: 0,
            pending_replication_import: None,
        }
    }

    pub fn defer(&mut self, error: String, now: u64, base_delay: u64, made_progress: bool) {
        if made_progress {
            // A bounded repair pass can make durable progress before it runs
            // out of time or sources. Start its next pass at the base interval
            // instead of exponentially delaying an actively recovering object,
            // but retain the failure history so recurring verification errors
            // cannot turn into an unbounded full-object retry loop.
            self.next_attempt_unix = now.saturating_add(base_delay.max(1));
            self.last_error = Some(error);
            return;
        }
        self.attempts = self.attempts.saturating_add(1);
        let delay = base_delay
            .max(1)
            .saturating_mul(1u64 << self.attempts.min(6))
            .min(3600);
        self.next_attempt_unix = now.saturating_add(delay);
        self.last_error = Some(error);
    }
}

pub(crate) fn validate_manifest(hash: &str, payload: &[u8]) -> Result<ReplicationManifestPayload> {
    if hash_hex(payload) != hash {
        bail!("manifest hash mismatch: expected={hash}");
    }
    let manifest: ObjectManifest =
        serde_json::from_slice(payload).context("invalid recovery manifest")?;
    let mut total = 0usize;
    let mut sizes = HashMap::new();
    for chunk in &manifest.chunks {
        if !manifest_hash_looks_safe_filename(&chunk.hash) {
            bail!("invalid chunk hash in manifest={hash}");
        }
        if sizes
            .insert(&chunk.hash, chunk.size_bytes)
            .is_some_and(|old| old != chunk.size_bytes)
        {
            bail!("inconsistent sizes for chunk={}", chunk.hash);
        }
        total = total
            .checked_add(chunk.size_bytes)
            .context("manifest size overflow")?;
    }
    if total != manifest.total_size_bytes {
        bail!("manifest size mismatch: hash={hash}");
    }
    Ok(ReplicationManifestPayload {
        key: manifest.key,
        total_size_bytes: manifest.total_size_bytes,
        chunks: manifest
            .chunks
            .into_iter()
            .map(|c| ReplicationChunkInfo {
                hash: c.hash,
                size_bytes: c.size_bytes,
            })
            .collect(),
    })
}

async fn read_manifest_bytes(storage_pool: &StoragePool, hash: &str) -> Result<Option<Vec<u8>>> {
    let path = storage_pool.content_path(StorageContentKind::Manifest, hash)?;
    let bytes = match fs::read(path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    Ok(Some(bytes))
}

async fn read_valid_manifest(
    storage_pool: &StoragePool,
    hash: &str,
) -> Result<Option<(Vec<u8>, ReplicationManifestPayload)>> {
    let Some(bytes) = read_manifest_bytes(storage_pool, hash).await? else {
        return Ok(None);
    };
    let manifest = validate_manifest(hash, &bytes)?;
    Ok(Some((bytes, manifest)))
}

/// Cheap presence contract shared by availability and ordinary replication.
/// Full chunk hashing belongs to scrub, actual transfers and repair completion.
pub(super) async fn manifest_is_fully_local(
    storage_pool: &StoragePool,
    hash: &str,
) -> Result<bool> {
    if hash == TOMBSTONE_MANIFEST_HASH {
        return Ok(true);
    }
    let Some(bytes) = read_manifest_bytes(storage_pool, hash).await? else {
        return Ok(false);
    };
    let manifest = match validate_manifest(hash, &bytes) {
        Ok(manifest) => manifest,
        Err(error) => {
            warn!(manifest_hash = hash, error = %error, "invalid manifest is not locally available");
            return Ok(false);
        }
    };
    for chunk in &manifest.chunks {
        let path = storage_pool.content_path(StorageContentKind::Chunk, &chunk.hash)?;
        let metadata = match fs::metadata(path).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_file() || metadata.len() != chunk.size_bytes as u64 {
            return Ok(false);
        }
    }
    Ok(true)
}

#[derive(Clone)]
pub(crate) struct ContentRecoveryInspector {
    storage_pool: StoragePool,
}

impl ContentRecoveryInspector {
    pub(crate) async fn chunk_path_exists(&self, hash: &str) -> Result<bool> {
        let path = self
            .storage_pool
            .content_path(StorageContentKind::Chunk, hash)?;
        Ok(fs::try_exists(path).await?)
    }

    pub(crate) async fn invalid_recovered_chunks(
        &self,
        task: &ContentRepairTask,
    ) -> Result<Vec<ReplicationChunkInfo>> {
        #[cfg(test)]
        wait_for_recovery_verification_test_blocker(&task.reference.manifest_hash).await;
        let (_, manifest) = read_valid_manifest(&self.storage_pool, &task.reference.manifest_hash)
            .await?
            .context("repaired manifest missing")?;
        let chunks = if task.repair_chunks {
            &manifest.chunks
        } else {
            &task.chunks
        };
        let mut invalid = Vec::new();
        for chunk in chunks {
            let path = self
                .storage_pool
                .content_path(StorageContentKind::Chunk, &chunk.hash)?;
            match fs::read(path).await {
                Ok(payload)
                    if payload.len() == chunk.size_bytes && hash_hex(&payload) == chunk.hash => {}
                Ok(_) => {
                    invalid.push(chunk.clone());
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    invalid.push(chunk.clone());
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(invalid)
    }
}

impl PersistentStore {
    pub(crate) fn content_recovery_inspector(&self) -> ContentRecoveryInspector {
        ContentRecoveryInspector {
            storage_pool: self.storage_pool.clone(),
        }
    }

    #[cfg(test)]
    pub(crate) async fn content_repair_tasks(&self) -> Result<Vec<ContentRepairTask>> {
        self.metadata_store.load_content_repair_tasks().await
    }

    pub(crate) async fn content_repair_tasks_for_manifests(
        &self,
        manifest_hashes: &[String],
    ) -> Result<Vec<ContentRepairTask>> {
        self.metadata_store
            .load_content_repair_tasks_for_manifests(manifest_hashes)
            .await
    }

    pub(crate) async fn content_repair_task_hashes(&self) -> Result<Vec<String>> {
        self.metadata_store.content_repair_task_hashes().await
    }

    pub(crate) async fn filter_locally_owned_manifests(
        &self,
        manifest_hashes: &[String],
    ) -> Result<HashSet<String>> {
        self.metadata_store
            .filter_locally_owned_manifests(manifest_hashes)
            .await
    }

    pub(crate) async fn due_content_repair_task_hashes(
        &self,
        now_unix: u64,
        source_fingerprint: &str,
        limit: usize,
    ) -> Result<Vec<String>> {
        self.metadata_store
            .due_content_repair_task_hashes(now_unix, source_fingerprint, limit)
            .await
    }

    pub(crate) async fn chunk_path_matches_size(&self, hash: &str, size: usize) -> Result<bool> {
        let path = self
            .storage_pool
            .content_path(StorageContentKind::Chunk, hash)?;
        let metadata = match fs::metadata(path).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        Ok(metadata.is_file() && metadata.len() == size as u64)
    }

    pub(crate) async fn persist_content_repair_task(&self, task: &ContentRepairTask) -> Result<()> {
        // Registration must precede the next GC snapshot. Transfers need not hold
        // this lock while waiting for network I/O: the durable task pins content.
        let _guard = self.content_gc_gate.read().await;
        self.metadata_store.persist_content_repair_task(task).await
    }

    /// Persists the immutable manifest and its pending import intent under one
    /// GC gate. A cancelled replication pull can then be resumed without
    /// fetching the manifest again, and cleanup cannot observe the manifest
    /// without the task that protects its partially recovered chunks.
    pub(crate) async fn install_recovery_manifest_and_persist_content_repair_task(
        &self,
        task: &ContentRepairTask,
        manifest_bytes: &[u8],
    ) -> Result<()> {
        validate_manifest(&task.reference.manifest_hash, manifest_bytes)?;
        let _guard = self.content_gc_gate.read().await;
        persist_storage_content(
            &self.storage_pool,
            self.metadata_store.as_ref(),
            StorageContentKind::Manifest,
            &task.reference.manifest_hash,
            manifest_bytes,
        )
        .await?;
        self.metadata_store.persist_content_repair_task(task).await
    }

    /// Registers a newly discovered repair task together with any chunks named
    /// by an already-local manifest. Holding the GC gate across both steps
    /// closes the enqueue-to-worker window: cache bytes that can be reused by
    /// the first repair pass are pinned before cleanup can reclaim them.
    pub(crate) async fn prepare_and_persist_content_repair_task(
        &self,
        task: &mut ContentRepairTask,
    ) -> Result<()> {
        let _guard = self.content_gc_gate.read().await;
        if task.chunks.is_empty() {
            match read_valid_manifest(&self.storage_pool, &task.reference.manifest_hash).await {
                Ok(Some((_, manifest))) => task.chunks = manifest.chunks,
                Ok(None) => {}
                Err(error) => {
                    warn!(
                        manifest_hash = %task.reference.manifest_hash,
                        error = %error,
                        "could not pin chunks for a locally unreadable repair manifest"
                    );
                }
            }
        }
        self.metadata_store.persist_content_repair_task(task).await
    }

    pub(crate) async fn discard_content_repair_task(&self, hash: &str) -> Result<()> {
        let _guard = self.content_gc_gate.read().await;
        self.metadata_store.delete_content_repair_task(hash).await
    }

    pub(crate) async fn read_recovery_manifest(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        Ok(read_valid_manifest(&self.storage_pool, hash)
            .await?
            .map(|(bytes, _)| bytes))
    }

    pub(crate) async fn install_recovery_manifest(&self, hash: &str, payload: &[u8]) -> Result<()> {
        validate_manifest(hash, payload)?;
        let _guard = self.content_gc_gate.read().await;
        persist_storage_content(
            &self.storage_pool,
            self.metadata_store.as_ref(),
            StorageContentKind::Manifest,
            hash,
            payload,
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn manifest_is_owned(&self, hash: &str) -> Result<bool> {
        Ok(self
            .metadata_store
            .filter_locally_owned_manifests(&[hash.to_string()])
            .await?
            .contains(hash))
    }

    /// Confirms a retained manifest and all of its chunks are present with the
    /// cheap metadata contract used by availability and ordinary replication.
    pub(crate) async fn manifest_is_fully_local(&self, hash: &str) -> Result<bool> {
        let _guard = self.content_gc_gate.read().await;
        manifest_is_fully_local(&self.storage_pool, hash).await
    }

    #[cfg(test)]
    pub(crate) async fn invalid_recovered_chunks(
        &self,
        task: &ContentRepairTask,
    ) -> Result<Vec<ReplicationChunkInfo>> {
        self.content_recovery_inspector()
            .invalid_recovered_chunks(task)
            .await
    }

    #[cfg(test)]
    pub(crate) async fn verify_recovered_content(&self, task: &ContentRepairTask) -> Result<()> {
        if let Some(chunk) = self.invalid_recovered_chunks(task).await?.first() {
            bail!("repaired chunk is missing or corrupt: {}", chunk.hash);
        }
        Ok(())
    }

    /// Returns whether this is a healthy owned replica. Operational failures
    /// remain errors so callers do not mistake a transient store failure for
    /// missing content and schedule destructive-looking repair work.
    pub(crate) async fn check_owned_replica_presence(&self, hash: &str) -> Result<bool> {
        if hash == TOMBSTONE_MANIFEST_HASH {
            return Ok(true);
        }
        let _guard = self.content_gc_gate.read().await;
        if !self.manifest_is_owned(hash).await? {
            return Ok(false);
        }
        if self.metadata_store.content_repair_pending(hash).await? {
            return Ok(false);
        }
        manifest_is_fully_local(&self.storage_pool, hash).await
    }

    #[cfg(test)]
    pub(crate) async fn finish_content_repair(&self, task: &ContentRepairTask) -> Result<()> {
        let _guard = self.content_gc_gate.read().await;
        self.verify_recovered_content(task).await?;
        self.finish_verified_content_repair_locked(task).await
    }

    /// Completes a repair after the caller has already validated every chunk.
    /// This is reserved for the replication pull path, which checks existing
    /// bytes and every received response before it calls this method.
    pub(crate) async fn finish_verified_content_repair(
        &self,
        task: &ContentRepairTask,
    ) -> Result<()> {
        let _guard = self.content_gc_gate.read().await;
        self.finish_verified_content_repair_locked(task).await
    }

    async fn finish_verified_content_repair_locked(&self, task: &ContentRepairTask) -> Result<()> {
        // Establish permanent protection before removing the temporary repair pin.
        if task.repair_chunks {
            self.mark_manifest_locally_owned(&task.reference.manifest_hash)
                .await?;
        }
        self.metadata_store
            .delete_content_repair_task(&task.reference.manifest_hash)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task() -> ContentRepairTask {
        ContentRepairTask::new(
            RetainedReference {
                key: Some("progress.bin".to_string()),
                object_id: None,
                version_id: Some("v1".to_string()),
                manifest_hash: "manifest".to_string(),
                snapshot_only: false,
            },
            true,
        )
    }

    #[test]
    fn productive_deferred_repair_returns_to_the_base_interval() {
        let mut task = task();
        task.attempts = 6;

        task.defer("remaining chunk unavailable".to_string(), 100, 30, true);

        assert_eq!(task.attempts, 6);
        assert_eq!(task.next_attempt_unix, 130);
        assert_eq!(
            task.last_error.as_deref(),
            Some("remaining chunk unavailable")
        );
    }

    #[test]
    fn unproductive_deferred_repair_keeps_exponential_backoff() {
        let mut task = task();

        task.defer("source unavailable".to_string(), 100, 30, false);

        assert_eq!(task.attempts, 1);
        assert_eq!(task.next_attempt_unix, 160);
    }
}
