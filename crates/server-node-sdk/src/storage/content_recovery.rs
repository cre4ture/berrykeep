//! Durable repair intent and GC-safe content installation. No namespace mutations.
use super::retained_content::RetainedReference;
use super::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ContentRepairTask {
    pub reference: RetainedReference,
    pub repair_chunks: bool,
    pub chunks: Vec<ReplicationChunkInfo>,
    pub attempts: u32,
    pub next_attempt_unix: u64,
    pub last_error: Option<String>,
    #[serde(default)]
    pub waiting_for_source: bool,
    #[serde(default)]
    pub source_fingerprint: String,
    #[serde(default)]
    pub recovered_chunks: usize,
}

impl ContentRepairTask {
    pub fn new(reference: RetainedReference, repair_chunks: bool) -> Self {
        Self {
            reference,
            repair_chunks,
            chunks: Vec::new(),
            attempts: 0,
            next_attempt_unix: 0,
            last_error: None,
            waiting_for_source: false,
            source_fingerprint: String::new(),
            recovered_chunks: 0,
        }
    }

    pub fn defer(&mut self, error: String, now: u64, base_delay: u64, made_progress: bool) {
        if made_progress {
            // A bounded repair pass can make durable progress before it runs
            // out of time or sources. Start its next pass at the base interval
            // instead of exponentially delaying an actively recovering object.
            self.attempts = 0;
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

async fn read_valid_manifest(
    storage_pool: &StoragePool,
    hash: &str,
) -> Result<Option<(Vec<u8>, ReplicationManifestPayload)>> {
    let path = storage_pool.content_path(StorageContentKind::Manifest, hash)?;
    let bytes = match fs::read(path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
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
    let manifest = match read_valid_manifest(storage_pool, hash).await {
        Ok(Some((_, manifest))) => manifest,
        Ok(None) => return Ok(false),
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

impl PersistentStore {
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

    /// A cache-repair task only needs to decide whether a chunk entry exists.
    /// Hashing and size validation happen once at durable-repair completion,
    /// where any invalid local entry is replaced from a verified peer response.
    pub(crate) async fn chunk_path_exists(&self, hash: &str) -> Result<bool> {
        let path = self
            .storage_pool
            .content_path(StorageContentKind::Chunk, hash)?;
        Ok(fs::try_exists(path).await?)
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
    #[cfg(test)]
    pub(crate) async fn manifest_is_fully_local(&self, hash: &str) -> Result<bool> {
        manifest_is_fully_local(&self.storage_pool, hash).await
    }

    pub(crate) async fn invalid_recovered_chunks(
        &self,
        task: &ContentRepairTask,
    ) -> Result<Vec<ReplicationChunkInfo>> {
        let payload = self
            .read_recovery_manifest(&task.reference.manifest_hash)
            .await?
            .context("repaired manifest missing")?;
        let manifest = validate_manifest(&task.reference.manifest_hash, &payload)?;
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

    pub(crate) async fn verify_recovered_content(&self, task: &ContentRepairTask) -> Result<()> {
        if let Some(chunk) = self.invalid_recovered_chunks(task).await?.first() {
            bail!("repaired chunk is missing or corrupt: {}", chunk.hash);
        }
        Ok(())
    }

    pub(crate) async fn check_owned_replica_presence(&self, hash: &str) -> Result<()> {
        if hash == TOMBSTONE_MANIFEST_HASH {
            return Ok(());
        }
        let _guard = self.content_gc_gate.read().await;
        if !self.manifest_is_owned(hash).await? {
            bail!("manifest is not an owned replica: {hash}");
        }
        if self.metadata_store.content_repair_pending(hash).await? {
            bail!("manifest has pending integrity findings: {hash}");
        }
        if !manifest_is_fully_local(&self.storage_pool, hash).await? {
            bail!("owned replica content is incomplete: {hash}");
        }
        Ok(())
    }

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

        assert_eq!(task.attempts, 0);
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
