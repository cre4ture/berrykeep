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

    pub fn defer(&mut self, error: String, now: u64, base_delay: u64) {
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

impl PersistentStore {
    pub(crate) async fn content_repair_tasks(&self) -> Result<Vec<ContentRepairTask>> {
        self.metadata_store.load_content_repair_tasks().await
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
        let path = self
            .storage_pool
            .content_path(StorageContentKind::Manifest, hash)?;
        let bytes = match fs::read(path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        validate_manifest(hash, &bytes)?;
        Ok(Some(bytes))
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

    pub(crate) async fn verify_recovered_content(&self, task: &ContentRepairTask) -> Result<()> {
        let payload = self
            .read_recovery_manifest(&task.reference.manifest_hash)
            .await?
            .context("repaired manifest missing")?;
        let manifest = validate_manifest(&task.reference.manifest_hash, &payload)?;
        {
            let chunks = if task.repair_chunks {
                &manifest.chunks
            } else {
                &task.chunks
            };
            for chunk in chunks {
                let payload = self
                    .read_chunk_payload(&chunk.hash)
                    .await?
                    .with_context(|| format!("repaired chunk missing: {}", chunk.hash))?;
                if payload.len() != chunk.size_bytes {
                    bail!("repaired chunk size mismatch: {}", chunk.hash);
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn verify_replication_source(&self, hash: &str, claim: bool) -> Result<()> {
        if hash == TOMBSTONE_MANIFEST_HASH {
            return Ok(());
        }
        let _guard = self.content_gc_gate.read().await;
        let task = ContentRepairTask::new(
            RetainedReference {
                key: None,
                object_id: None,
                version_id: None,
                manifest_hash: hash.to_string(),
                snapshot_only: true,
            },
            true,
        );
        self.verify_recovered_content(&task).await?;
        if claim {
            self.mark_manifest_locally_owned(hash).await?;
        }
        Ok(())
    }

    pub(crate) async fn finish_content_repair(&self, task: &ContentRepairTask) -> Result<()> {
        let _guard = self.content_gc_gate.read().await;
        self.verify_recovered_content(task).await?;
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
