use super::*;

impl CfapiWork {
    pub(in crate::monitor) fn pending_uploads(&self) -> Result<HashSet<String>> {
        Ok(self
            .store
            .work(UPLOAD_SCOPE)?
            .into_iter()
            .map(|work| work.key.trim_end_matches('/').to_owned())
            .collect())
    }

    pub(in crate::monitor) fn enqueue_upload(
        &self,
        path: &str,
        object_id: Option<&str>,
        revision: Option<&str>,
        length: u64,
    ) -> Result<()> {
        self.store.enqueue(UPLOAD_SCOPE, &[PlannedWork::new(path, format!("upload:{}", object_id.unwrap_or(path)),
            serde_json::json!({"path":path,"object_id":object_id,"revision":revision,"length":length}))])?;
        self.notify();
        Ok(())
    }

    pub(in crate::monitor) fn cancel_missing_uploads(
        &self,
        current: &HashMap<String, ObservedEntry>,
    ) -> Result<()> {
        for job in self.store.work(UPLOAD_SCOPE)? {
            if !current.contains_key(job.key.trim_end_matches('/')) {
                self.store.cancel(UPLOAD_SCOPE, &job.key)?;
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(in crate::monitor) fn record_receipt(
        &self,
        path: &str,
        receipt: &crate::runtime::UploadReceipt,
    ) -> Result<()> {
        self.record_accepted_receipt(path, None, None, receipt)
    }

    pub(in crate::monitor) fn record_accepted_receipt(
        &self,
        path: &str,
        source_object_id: Option<&str>,
        source_revision: Option<&str>,
        receipt: &crate::runtime::UploadReceipt,
    ) -> Result<()> {
        let is_dir = path.ends_with('/');
        let normalized = normalize_path(path);
        let path = normalized.trim_end_matches('/');
        let (Some(object_id), Some(revision)) = (&receipt.object_id, &receipt.remote_version)
        else {
            anyhow::bail!("upload receipt lacks object identity/revision for {path}");
        };
        self.store.enqueue(
            RECEIPT_SCOPE,
            &[PlannedWork::new(
                path,
                format!("receipt:{object_id}"),
                serde_json::to_value(ReceiptWork {
                    receipt: receipt.clone(),
                    source_object_id: source_object_id.map(ToOwned::to_owned),
                    source_revision: source_revision.map(ToOwned::to_owned),
                    is_dir,
                })?,
            )],
        )?;
        if let Some((source_id, source_revision)) = source_object_id.zip(source_revision)
            && source_id == object_id
            && source_revision != revision
        {
            self.record_fence(
                fence_lookup_key(Some(source_id), Some(source_revision)).expect("source identity"),
                RevisionFence {
                    object_id: source_id.to_owned(),
                    revision: source_revision.to_owned(),
                },
            )?;
        }
        let entry = PriorPath {
            is_dir,
            object_id: Some(object_id.clone()),
            revision: Some(revision.clone()),
        };
        let mut baseline = self.baseline.lock().unwrap_or_else(|err| err.into_inner());
        self.store
            .observe_paths(BASELINE_SCOPE, &[(path.to_owned(), entry.clone())], &[])?;
        baseline.insert(path.to_owned(), entry);
        self.notify();
        Ok(())
    }

    pub(in crate::monitor) fn recover_upload_observations(&self) -> Result<()> {
        for job in self.store.work(RECEIPT_SCOPE)? {
            let work: ReceiptWork = serde_json::from_value(job.payload)?;
            if let Some((id, revision)) = work
                .source_object_id
                .as_deref()
                .zip(work.source_revision.as_deref())
            {
                self.record_fence(
                    fence_lookup_key(Some(id), Some(revision)).expect("receipt source identity"),
                    RevisionFence {
                        object_id: id.to_owned(),
                        revision: revision.to_owned(),
                    },
                )?;
            }
            let Some(object_id) = work.receipt.object_id.as_ref() else {
                continue;
            };
            let Some(revision) = work.receipt.remote_version.as_ref() else {
                continue;
            };
            let mut baseline = self.baseline.lock().unwrap_or_else(|err| err.into_inner());
            let previous = baseline.get(&job.key);
            if previous.is_none()
                || previous.is_some_and(|previous| {
                    previous.object_id == work.source_object_id
                        && previous.revision == work.source_revision
                })
            {
                let entry = PriorPath {
                    is_dir: work.is_dir,
                    object_id: Some(object_id.clone()),
                    revision: Some(revision.clone()),
                };
                self.store.observe_paths(
                    BASELINE_SCOPE,
                    &[(job.key.clone(), entry.clone())],
                    &[],
                )?;
                baseline.insert(job.key, entry);
            }
        }
        Ok(())
    }

    pub(crate) fn drain_upload_receipts(&self) -> Result<()> {
        for _ in 0..64 {
            let Some(lease) = self.store.claim_next(RECEIPT_SCOPE, now_ms(), 120_000)? else {
                break;
            };
            let work: ReceiptWork = serde_json::from_value(lease.work.payload.clone())?;
            let path = &lease.work.key;
            let (result, applied) = self.store.execute(&lease, || {
                let full = self.root.join(path.replace('/', "\\"));
                let gate = crate::path_operations::gate(&full);
                let _path_operation = gate.lock().unwrap_or_else(|e| e.into_inner());
                let metadata = match std::fs::metadata(&full) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        return WorkResult::Complete;
                    }
                    Err(error) => {
                        return WorkResult::Retry(format!(
                            "receipt target metadata unavailable: {error}"
                        ));
                    }
                };
                if metadata.is_dir() != work.is_dir {
                    // The accepted object was replaced by another kind. Retain
                    // its accepted namespace observation for guarded deletion;
                    // never bind that old identity onto the replacement.
                    return WorkResult::Complete;
                }
                let entry = snapshot_entry(&self.root, path, &full, work.is_dir);
                if entry.placeholder_object_id == work.receipt.object_id
                    && entry.placeholder_revision == work.receipt.remote_version
                {
                    return WorkResult::Complete;
                }
                if entry.placeholder_inspection_error.is_some() {
                    return WorkResult::Retry("current placeholder identity unavailable".into());
                }
                if entry.is_placeholder
                    && (entry.placeholder_object_id != work.source_object_id
                        || entry.placeholder_revision != work.source_revision)
                {
                    return WorkResult::Conflict(format!(
                        "upload receipt cannot replace an unrelated identity at {path}"
                    ));
                }
                let result = (|| -> Result<()> {
                    record_uploaded_object_state(
                        &self.root,
                        path,
                        self.provider,
                        work.receipt
                            .object_id
                            .as_deref()
                            .ok_or_else(|| anyhow::anyhow!("receipt lacks object ID"))?,
                        work.receipt
                            .remote_version
                            .as_deref()
                            .ok_or_else(|| anyhow::anyhow!("receipt lacks revision"))?,
                        None,
                    )?;
                    // The server accepted these bytes, but the local file may have
                    // changed meanwhile. Advance our own CAS identity without
                    // declaring unknown current contents synchronized.
                    if !work.is_dir {
                        cf_set_not_in_sync_metadata_only(&full)?;
                    }
                    Ok(())
                })();
                match result {
                    Ok(()) => WorkResult::Complete,
                    Err(error) => WorkResult::Retry(format!("{error:#}")),
                }
            })?;
            if !applied {
                tracing::debug!(
                    path,
                    "upload receipt superseded while applying; newer work retained"
                );
                continue;
            }
            if !matches!(result, WorkResult::Complete) {
                tracing::warn!(
                    path,
                    ?result,
                    "CFAPI accepted upload receipt retained for repair"
                );
            }
        }
        Ok(())
    }
}

/// The existing content-transfer executor supplies bytes and its CFAPI safety
/// checks. This boundary persists upload intent and accepted namespace identity.
pub(in crate::monitor) struct JournaledUploader {
    pub inner: Arc<dyn Uploader>,
    pub work: CfapiWork,
}

impl Uploader for JournaledUploader {
    fn upload_reader(
        &self,
        path: &str,
        reader: &mut dyn std::io::Read,
        length: u64,
    ) -> Result<crate::runtime::UploadReceipt> {
        self.upload_reader_for_object(path, None, None, reader, length)
    }

    fn upload_reader_for_object(
        &self,
        path: &str,
        object_id: Option<&str>,
        revision: Option<&str>,
        reader: &mut dyn std::io::Read,
        length: u64,
    ) -> Result<crate::runtime::UploadReceipt> {
        let observed = self
            .work
            .baseline
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .get(path.trim_end_matches('/'))
            .cloned();
        if observed.as_ref().is_some_and(|old| {
            old.is_dir != path.ends_with('/') && old.object_id.is_some() && old.revision.is_some()
        }) {
            self.work
                .enqueue_upload(path, object_id, revision, length)?;
            self.work
                .stage_kind_replacement(path, path.ends_with('/'))?;
            anyhow::bail!("kind replacement is waiting for guarded namespace removal: {path}");
        }
        if is_protected(path.trim_end_matches('/'), &self.work.protected_paths()?) {
            self.work
                .enqueue_upload(path, object_id, revision, length)?;
            anyhow::bail!("upload is waiting for its pending namespace operation: {path}");
        }
        if let Some(observed) = observed
            && observed.object_id.as_deref().is_some()
            && (observed.object_id.as_deref() != object_id
                || observed.revision.as_deref() != revision)
        {
            anyhow::bail!(
                "upload identity advanced; reconcile current receipt before retrying {path}"
            );
        }
        self.work
            .enqueue_upload(path, object_id, revision, length)?;
        self.work.store.rearm_completed(UPLOAD_SCOPE, path)?;
        let Some(lease) = self
            .work
            .store
            .claim_key(UPLOAD_SCOPE, path, now_ms(), 120_000)?
        else {
            anyhow::bail!("upload is leased or waiting for retry: {path}");
        };
        let mut receipt = None;
        let mut upload_error = None;
        let (_, applied) = self.work.store.execute(&lease, || {
            match self
                .inner
                .upload_reader_for_object(path, object_id, revision, reader, length)
                .and_then(|result| {
                    self.work
                        .record_accepted_receipt(path, object_id, revision, &result)?;
                    receipt = Some(result);
                    Ok(())
                }) {
                Ok(()) => WorkResult::Complete,
                Err(error) => {
                    let message = format!("{error:#}");
                    let conflict = is_remote_mutation_conflict(&error);
                    upload_error = Some(error);
                    if conflict {
                        WorkResult::Conflict(message)
                    } else {
                        WorkResult::Retry(message)
                    }
                }
            }
        })?;
        self.work.notify();
        if !applied {
            anyhow::bail!("upload work was superseded while executing {path}");
        }
        if let Some(error) = upload_error {
            return Err(error);
        }
        receipt.ok_or_else(|| anyhow::anyhow!("upload produced no receipt for {path}"))
    }

    fn delete_object(&self, object_id: &str, revision: &str) -> Result<()> {
        self.inner.delete_object(object_id, revision)
    }
    fn rename_object(
        &self,
        object_id: &str,
        revision: &str,
        to: &str,
    ) -> Result<crate::runtime::ObjectRenameReceipt> {
        self.inner.rename_object(object_id, revision, to)
    }
    fn resolve_remote_object(
        &self,
        object_id: &str,
    ) -> Result<crate::placeholder_metadata::RemoteObjectResolution> {
        self.inner.resolve_remote_object(object_id)
    }
}
