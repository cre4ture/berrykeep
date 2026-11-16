use super::*;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use sync_agent_core::sync_work::{ContentAvailability, PlannedWork, WorkResult, WorkStore, now_ms};

pub(super) const LOCAL_SCOPE: &str = "cfapi-local-intent";
const MATERIALIZATION_SCOPE: &str = "cfapi-materialization";
const BASELINE_SCOPE: &str = "cfapi-namespace";
const FENCE_SCOPE: &str = "cfapi-completed-mutations";
const UPLOAD_SCOPE: &str = "cfapi-upload";
const RECEIPT_SCOPE: &str = "cfapi-upload-receipt";

#[derive(Clone, Serialize, Deserialize)]
struct ReceiptWork {
    receipt: crate::runtime::UploadReceipt,
    source_object_id: Option<String>,
    source_revision: Option<String>,
    is_dir: bool,
}

#[derive(Clone, Serialize, Deserialize)]
struct Materialization {
    actions: Vec<CfapiAction>,
    availability: ContentAvailability,
}

#[derive(Clone, Serialize, Deserialize)]
struct RevisionFence {
    object_id: String,
    revision: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) enum LocalMutation {
    Delete {
        path: String,
        object_id: String,
        revision: String,
        is_dir: bool,
    },
    Rename {
        from: String,
        to: String,
        object_id: String,
        revision: String,
        is_dir: bool,
    },
}

impl LocalMutation {
    pub(super) fn object_id(&self) -> &str {
        match self {
            Self::Delete { object_id, .. } | Self::Rename { object_id, .. } => object_id,
        }
    }
    pub(super) fn paths(&self) -> Vec<&str> {
        match self {
            Self::Delete { path, .. } => vec![path],
            Self::Rename { from, to, .. } => vec![from, to],
        }
    }
}

#[derive(Clone)]
pub(crate) struct CfapiWork {
    pub(super) store: WorkStore,
    root: PathBuf,
    provider: uuid::Uuid,
    tracker: RemoteAppliedTracker,
    inventory: Arc<Mutex<BTreeMap<String, BTreeSet<String>>>>,
    baseline: Arc<Mutex<HashMap<String, PriorPath>>>,
    fences: Arc<Mutex<HashMap<String, RevisionFence>>>,
    wake: Arc<(Mutex<u64>, std::sync::Condvar)>,
}

impl CfapiWork {
    pub(crate) fn wake_epoch(&self) -> u64 {
        *self.wake.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn notify(&self) {
        let mut epoch = self.wake.0.lock().unwrap_or_else(|e| e.into_inner());
        *epoch = epoch.wrapping_add(1);
        self.wake.1.notify_all();
    }

    pub(crate) fn wait_for_work(&self, observed: u64, timeout: std::time::Duration) {
        let epoch = self.wake.0.lock().unwrap_or_else(|e| e.into_inner());
        let _ = self
            .wake
            .1
            .wait_timeout_while(epoch, timeout, |epoch| *epoch == observed)
            .unwrap_or_else(|e| e.into_inner());
    }

    pub(crate) fn next_execution_at(&self) -> Result<Option<i64>> {
        Ok([RECEIPT_SCOPE, MATERIALIZATION_SCOPE]
            .into_iter()
            .map(|scope| self.store.next_ready_at(scope))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .min())
    }

    pub(super) fn pending_uploads(&self) -> Result<HashSet<String>> {
        Ok(self
            .store
            .work(UPLOAD_SCOPE)?
            .into_iter()
            .map(|work| work.key.trim_end_matches('/').to_owned())
            .collect())
    }
    pub(super) fn cancel_missing_uploads(
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

    pub(crate) fn status(&self) -> Result<sync_agent_core::sync_work::WorkStatus> {
        let mut result = sync_agent_core::sync_work::WorkStatus::default();
        for scope in [
            MATERIALIZATION_SCOPE,
            LOCAL_SCOPE,
            UPLOAD_SCOPE,
            RECEIPT_SCOPE,
        ] {
            let status = self.store.status(scope)?;
            result.pending += status.pending;
            result.running += status.running;
            result.conflicts += status.conflicts;
            result.completed += status.completed;
        }
        Ok(result)
    }

    pub(super) fn new(
        root: PathBuf,
        provider: uuid::Uuid,
        store: WorkStore,
        tracker: RemoteAppliedTracker,
    ) -> Result<Self> {
        let baseline = store.path_observations(BASELINE_SCOPE)?;
        let fences = store.path_observations(FENCE_SCOPE)?;
        let inventory = crate::placeholder_metadata::local_placeholder_paths_by_object_id(&root);
        Ok(Self {
            store,
            root,
            provider,
            tracker,
            inventory: Arc::new(Mutex::new(inventory)),
            baseline: Arc::new(Mutex::new(baseline)),
            fences: Arc::new(Mutex::new(fences)),
            wake: Arc::default(),
        })
    }

    pub(super) fn prior(&self) -> HashMap<String, PriorPath> {
        self.baseline
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
    }

    pub(super) fn persist_prior(
        &self,
        previous: &HashMap<String, PriorPath>,
        current: &HashMap<String, PriorPath>,
    ) -> Result<()> {
        let receipts = self
            .store
            .work(RECEIPT_SCOPE)?
            .into_iter()
            .map(|job| Ok((job.key, serde_json::from_value::<ReceiptWork>(job.payload)?)))
            .collect::<Result<HashMap<_, _>>>()?;
        let mut baseline = self.baseline.lock().unwrap_or_else(|err| err.into_inner());
        // Receipt callbacks can advance identities while a monitor scan is running.
        // Never overwrite or remove an observation newer than the scan's baseline.
        let changed = current
            .iter()
            .filter(|(path, value)| {
                baseline.get(*path) == previous.get(*path)
                    && baseline.get(*path) != Some(*value)
                    && !receipts.get(*path).is_some_and(|receipt| {
                        value.is_dir == receipt.is_dir
                            && ((value.object_id == receipt.source_object_id
                                && value.revision == receipt.source_revision)
                                || (value.object_id.is_none() && value.revision.is_none()))
                    })
            })
            .map(|(path, value)| (path.clone(), value.clone()))
            .collect::<Vec<_>>();
        let removed = previous
            .keys()
            .filter(|path| {
                !current.contains_key(*path) && baseline.get(*path) == previous.get(*path)
            })
            .cloned()
            .collect::<Vec<_>>();
        self.store
            .observe_paths(BASELINE_SCOPE, &changed, &removed)?;
        baseline.extend(changed);
        for path in removed {
            baseline.remove(&path);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn record_receipt(
        &self,
        path: &str,
        receipt: &crate::runtime::UploadReceipt,
    ) -> Result<()> {
        self.record_accepted_receipt(path, None, None, receipt)
    }

    fn record_accepted_receipt(
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

    pub(super) fn recover_upload_observations(&self) -> Result<()> {
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
                if !full.exists() {
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

    pub(super) fn local_intents(&self) -> Result<Vec<LocalMutation>> {
        self.store
            .work(LOCAL_SCOPE)?
            .into_iter()
            .map(|work| serde_json::from_value(work.payload).map_err(Into::into))
            .collect()
    }

    pub(crate) fn protected_object_ids(&self) -> Result<BTreeSet<String>> {
        Ok(self
            .local_intents()?
            .iter()
            .map(|intent| intent.object_id().to_owned())
            .collect())
    }

    pub(crate) fn protected_snapshot_ids(
        &self,
        snapshot: &sync_core::SyncSnapshot,
    ) -> Result<BTreeSet<String>> {
        let mut ids = self.protected_object_ids()?;
        let fences = self.fences.lock().unwrap_or_else(|err| err.into_inner());
        for entry in &snapshot.remote {
            if fence_lookup_key(entry.object_id.as_deref(), entry.version.as_deref())
                .and_then(|key| fences.get(&key))
                .is_some_and(|intent| {
                    fence_suppresses(
                        intent,
                        entry.object_id.as_deref(),
                        entry.version.as_deref(),
                        &entry.path,
                    )
                })
                && let Some(id) = entry.object_id.as_ref()
            {
                ids.insert(id.clone());
            }
        }
        Ok(ids)
    }

    fn suppressed_by_fence(&self, action: &CfapiAction) -> bool {
        let fences = self.fences.lock().unwrap_or_else(|err| err.into_inner());
        fence_lookup_key(action_object_id(action), action_revision(action))
            .and_then(|key| fences.get(&key))
            .is_some_and(|intent| {
                fence_suppresses(
                    intent,
                    action_object_id(action),
                    action_revision(action),
                    action_path(action),
                )
            })
    }

    fn completed_mutation(&self, intent: &LocalMutation) -> WorkResult {
        let revision = match intent {
            LocalMutation::Delete { revision, .. } | LocalMutation::Rename { revision, .. } => {
                revision
            }
        };
        let key = fence_lookup_key(Some(intent.object_id()), Some(revision))
            .expect("complete mutation identity");
        match self.record_fence(
            key,
            RevisionFence {
                object_id: intent.object_id().to_owned(),
                revision: revision.to_owned(),
            },
        ) {
            Ok(()) => WorkResult::Complete,
            Err(error) => {
                WorkResult::Retry(format!("persisting completed mutation fence: {error:#}"))
            }
        }
    }

    fn record_fence(&self, key: String, fence: RevisionFence) -> Result<()> {
        self.store
            .observe_paths(FENCE_SCOPE, &[(key.clone(), fence.clone())], &[])?;
        self.fences
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .insert(key, fence);
        Ok(())
    }

    pub(super) fn protected_paths(&self) -> Result<BTreeSet<String>> {
        Ok(self
            .local_intents()?
            .iter()
            .flat_map(|intent| intent.paths().into_iter().map(ToOwned::to_owned))
            .collect())
    }

    pub(super) fn enqueue_intent(&self, intent: &LocalMutation) -> Result<()> {
        self.store.enqueue(
            LOCAL_SCOPE,
            &[PlannedWork::new(
                intent.object_id(),
                intent.object_id(),
                serde_json::to_value(intent)?,
            )],
        )?;
        self.notify();
        Ok(())
    }

    pub(super) fn enqueue_deletes(&self, intents: &[LocalMutation]) -> Result<()> {
        let mut jobs = Vec::new();
        for intent in intents {
            let mut job = PlannedWork::new(
                intent.object_id(),
                intent.object_id(),
                serde_json::to_value(intent)?,
            );
            if let LocalMutation::Delete {
                path, is_dir: true, ..
            } = intent
            {
                let prefix = format!("{path}/");
                job.depends_on = intents
                    .iter()
                    .filter_map(|child| match child {
                        LocalMutation::Delete {
                            path, object_id, ..
                        } if path.starts_with(&prefix) => Some(object_id.clone()),
                        _ => None,
                    })
                    .collect();
            }
            jobs.push(job);
        }
        self.store.enqueue(LOCAL_SCOPE, &jobs)?;
        self.notify();
        Ok(())
    }

    pub(super) fn drain_mutations(&self, uploader: &dyn Uploader) -> Result<()> {
        for _ in 0..64 {
            let Some(lease) = self.store.claim_next(LOCAL_SCOPE, now_ms(), 120_000)? else {
                break;
            };
            let intent: LocalMutation = serde_json::from_value(lease.work.payload.clone())?;
            let (result, applied) = self
                .store
                .execute(&lease, || self.apply_mutation(uploader, &intent))?;
            if !applied {
                tracing::debug!(
                    work = lease.work.key,
                    "CFAPI local intent superseded; newer work retained"
                );
                continue;
            }
            if matches!(result, WorkResult::Conflict(_))
                && let LocalMutation::Rename { to, .. } = &intent
            {
                mark_local_rename_conflict(&self.root.join(to.replace('/', "\\")));
            }
            if !matches!(result, WorkResult::Complete) {
                tracing::warn!(?intent,?result,journal=%self.store.path().display(),"CFAPI local mutation retained");
            }
        }
        Ok(())
    }

    fn apply_mutation(&self, uploader: &dyn Uploader, intent: &LocalMutation) -> WorkResult {
        use crate::placeholder_metadata::RemoteObjectResolution;
        let gates = intent
            .paths()
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|path| crate::path_operations::gate(&self.root.join(path.replace('/', "\\"))))
            .collect::<Vec<_>>();
        let _path_operations = gates
            .iter()
            .map(|gate| gate.lock().unwrap_or_else(|e| e.into_inner()))
            .collect::<Vec<_>>();
        match intent {
            LocalMutation::Delete {
                path,
                object_id,
                revision,
                ..
            } => {
                let full = self.root.join(path.replace('/', "\\"));
                if full.exists() {
                    let entry = snapshot_entry(&self.root, path, &full, full.is_dir());
                    if entry.placeholder_object_id.as_deref() == Some(object_id) {
                        return WorkResult::Conflict(format!(
                            "deleted object was restored locally at {path}"
                        ));
                    }
                }
                match uploader.delete_object(object_id, revision) {
                    Ok(()) => self.completed_mutation(intent),
                    Err(error) => match uploader.resolve_remote_object(object_id) {
                        Ok(RemoteObjectResolution::Tombstone(_)) => self.completed_mutation(intent),
                        Ok(RemoteObjectResolution::Present(remote))
                            if remote.revision.as_deref() != Some(revision) =>
                        {
                            WorkResult::Conflict(format!(
                                "delete expected {revision}, remote revision is {:?}",
                                remote.revision
                            ))
                        }
                        _ if is_remote_mutation_conflict(&error) => {
                            WorkResult::Conflict(format!("{error:#}"))
                        }
                        _ => WorkResult::Retry(format!("{error:#}")),
                    },
                }
            }
            LocalMutation::Rename {
                from,
                to,
                object_id,
                revision,
                is_dir,
            } => {
                let full = self.root.join(to.replace('/', "\\"));
                let entry = snapshot_entry(&self.root, to, &full, *is_dir);
                if entry.placeholder_object_id.as_deref() != Some(object_id) {
                    return WorkResult::Conflict(format!(
                        "rename destination identity changed at {to}"
                    ));
                }
                let receipt = match uploader.rename_object(
                    object_id,
                    revision,
                    &remote_rename_destination(to, *is_dir),
                ) {
                    Ok(receipt) => receipt,
                    Err(error) => match uploader.resolve_remote_object(object_id) {
                        Ok(RemoteObjectResolution::Present(remote))
                            if normalize_path(&remote.path).trim_end_matches('/') == to
                                && remote
                                    .revision
                                    .as_ref()
                                    .is_some_and(|revision| !revision.is_empty()) =>
                        {
                            crate::runtime::ObjectRenameReceipt {
                                object_id: remote.object_id,
                                remote_version: remote.revision.unwrap(),
                            }
                        }
                        Ok(RemoteObjectResolution::Present(remote))
                            if normalize_path(&remote.path).trim_end_matches('/') != from
                                || remote.revision.as_deref() != Some(revision) =>
                        {
                            return WorkResult::Conflict(format!(
                                "rename object changed remotely: {remote:?}"
                            ));
                        }
                        _ if is_remote_mutation_conflict(&error) => {
                            return WorkResult::Conflict(format!("{error:#}"));
                        }
                        _ => return WorkResult::Retry(format!("{error:#}")),
                    },
                };
                if receipt.object_id != *object_id || receipt.remote_version.trim().is_empty() {
                    return WorkResult::Conflict("invalid rename receipt identity".into());
                }
                if let Err(error) = repair_locally_renamed_object(
                    &self.root,
                    &full,
                    to,
                    self.provider,
                    *is_dir,
                    &receipt,
                ) {
                    return WorkResult::Retry(format!("repairing rename receipt: {error:#}"));
                }
                let entry = PriorPath {
                    is_dir: *is_dir,
                    object_id: Some(object_id.clone()),
                    revision: Some(receipt.remote_version),
                };
                let mut baseline = self.baseline.lock().unwrap_or_else(|err| err.into_inner());
                if let Err(error) = self.store.observe_paths(
                    BASELINE_SCOPE,
                    &[(to.clone(), entry.clone())],
                    std::slice::from_ref(from),
                ) {
                    return WorkResult::Retry(format!("persisting rename receipt: {error:#}"));
                }
                baseline.remove(from);
                baseline.insert(to.clone(), entry);
                self.completed_mutation(intent)
            }
        }
    }

    pub(crate) fn schedule(
        &self,
        plan: &CfapiActionPlan,
        inventory: &BTreeMap<String, BTreeSet<String>>,
    ) -> Result<()> {
        *self.inventory.lock().unwrap_or_else(|err| err.into_inner()) = inventory.clone();
        let plan = CfapiActionPlan {
            actions: plan
                .actions
                .iter()
                .filter(|action| !self.suppressed_by_fence(action))
                .cloned()
                .collect(),
        };
        self.store.reconcile(
            MATERIALIZATION_SCOPE,
            &materialization_plan(&plan, &self.protected_paths()?)?,
        )?;
        self.notify();
        Ok(())
    }

    pub(crate) fn drain_materialization(&self) -> Result<()> {
        for _ in 0..8 {
            let Some(lease) = self
                .store
                .claim_next(MATERIALIZATION_SCOPE, now_ms(), 120_000)?
            else {
                break;
            };
            let work: Materialization = serde_json::from_value(lease.work.payload.clone())?;
            let (result, applied) =
                self.store
                    .execute(&lease, || match self.apply_materialization(&work) {
                        Ok(result) => result,
                        Err(error) => WorkResult::Retry(format!("{error:#}")),
                    })?;
            if !applied {
                tracing::debug!(
                    work = lease.work.key,
                    "CFAPI materialization superseded; newer work retained"
                );
                continue;
            }
            if !matches!(result, WorkResult::Complete) {
                tracing::warn!(
                    work = lease.work.key,
                    ?result,
                    "CFAPI materialization retained"
                );
            }
        }
        let status = self.store.status(MATERIALIZATION_SCOPE)?;
        if status.conflicts > 0 {
            tracing::warn!(conflicts=status.conflicts,journal=%self.store.path().display(),"CFAPI materialization conflicts require reconciliation");
        }
        Ok(())
    }

    fn apply_materialization(&self, work: &Materialization) -> Result<WorkResult> {
        if work.availability != ContentAvailability::MetadataOnly {
            anyhow::bail!("CFAPI namespace executor requires metadata availability");
        }
        let gates = work
            .actions
            .iter()
            .map(action_path)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|path| crate::path_operations::gate(&self.root.join(path.replace('/', "\\"))))
            .collect::<Vec<_>>();
        let _path_operations = gates
            .iter()
            .map(|gate| gate.lock().unwrap_or_else(|e| e.into_inner()))
            .collect::<Vec<_>>();
        let protected = self.protected_paths()?;
        let mut conflict = None;
        let mut waiting = None;
        let actions = work
            .actions
            .iter()
            .filter(|action| {
                !is_protected(action_path(action), &protected) && !self.suppressed_by_fence(action)
            })
            .filter_map(|action| {
                let path = action_path(action);
                let full = self.root.join(path.replace('/', "\\"));
                if !full.exists()
                    && self
                        .baseline
                        .lock()
                        .unwrap_or_else(|err| err.into_inner())
                        .contains_key(path)
                {
                    waiting = Some(format!(
                        "awaiting complete local topology reconciliation for missing {path}"
                    ));
                    return None;
                }
                if full.exists()
                    && !matches!(
                        action,
                        CfapiAction::EnsureDirectory {
                            object_id: None,
                            ..
                        }
                    )
                {
                    let entry = snapshot_entry(&self.root, path, &full, full.is_dir());
                    if let Some(error) = entry.placeholder_inspection_error.as_ref() {
                        waiting = Some(error.clone());
                        return None;
                    }
                    if !entry.is_placeholder
                        || entry.placeholder_object_id.as_deref() != action_object_id(action)
                    {
                        conflict = Some(format!("local object identity differs at {path}"));
                        return None;
                    }
                    if entry
                        .placeholder_state
                        .is_some_and(placeholder_has_uploadable_local_content)
                    {
                        // Metadata already represents this revision; modified local bytes
                        // remain for the independent guarded upload path.
                        if entry.placeholder_revision.as_deref() != action_revision(action) {
                            conflict = Some(format!("locally modified placeholder at {path}"));
                        }
                        return None;
                    }
                }
                Some(action.clone())
            })
            .collect::<Vec<_>>();
        let mut inventory = self.inventory.lock().unwrap_or_else(|err| err.into_inner());
        let plan = CfapiActionPlan {
            actions: actions.clone(),
        };
        if crate::runtime::apply_action_plan_with_observed_placeholder_paths(
            &self.root,
            &plan,
            self.provider,
            true,
            Some(&inventory),
        )
        .is_err()
        {
            // A locked first entry must not prevent the remainder of its batch
            // from materializing. Successful entries are recognized on replay.
            for action in &actions {
                let _ = crate::runtime::apply_action_plan_with_observed_placeholder_paths(
                    &self.root,
                    &CfapiActionPlan {
                        actions: vec![action.clone()],
                    },
                    self.provider,
                    true,
                    Some(&inventory),
                );
            }
        }
        let mut observed = Vec::new();
        let mut retry = waiting;
        for action in &actions {
            let path = action_path(action);
            let full = self.root.join(path.replace('/', "\\"));
            if !full.exists() {
                retry = Some(format!("placeholder still absent at {path}"));
                continue;
            }
            let entry = snapshot_entry(&self.root, path, &full, full.is_dir());
            if let Some(error) = entry.placeholder_inspection_error.as_ref() {
                retry = Some(error.clone());
                continue;
            }
            if action_object_id(action).is_some()
                && (entry.placeholder_object_id.as_deref() != action_object_id(action)
                    || entry.placeholder_revision.as_deref() != action_revision(action))
            {
                retry = Some(format!(
                    "placeholder identity/revision not applied at {path}"
                ));
                continue;
            }
            if let Some(object_id) = entry.placeholder_object_id.as_ref() {
                inventory
                    .entry(object_id.clone())
                    .or_default()
                    .insert(path.to_owned());
            }
            observed.push((path.to_owned(), PriorPath::from_observed(&entry)));
            self.tracker.record_plan(&CfapiActionPlan {
                actions: vec![action.clone()],
            });
            for parent in parent_directories_for_path(path) {
                let full = self.root.join(parent.replace('/', "\\"));
                if full.is_dir() {
                    observed.push((
                        parent.clone(),
                        PriorPath::from_observed(&snapshot_entry(&self.root, &parent, &full, true)),
                    ));
                }
            }
        }
        self.store.observe_paths(BASELINE_SCOPE, &observed, &[])?;
        self.baseline
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .extend(observed);
        Ok(match (retry, conflict) {
            (Some(error), _) => WorkResult::Retry(error),
            (None, Some(error)) => WorkResult::Conflict(error),
            _ => WorkResult::Complete,
        })
    }
}

/// The existing content-transfer executor supplies bytes and its CFAPI safety
/// checks. This boundary persists upload intent and accepted namespace identity.
pub(super) struct JournaledUploader {
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
        if let Some(observed) = observed
            && observed.object_id.as_deref().is_some()
            && (observed.object_id.as_deref() != object_id
                || observed.revision.as_deref() != revision)
        {
            anyhow::bail!(
                "upload identity advanced; reconcile current receipt before retrying {path}"
            );
        }
        let job = PlannedWork::new(
            path,
            format!("upload:{}", object_id.unwrap_or(path)),
            serde_json::json!({"path":path,"object_id":object_id,"revision":revision,"length":length}),
        );
        self.work.store.enqueue(UPLOAD_SCOPE, &[job])?;
        self.work.notify();
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

fn is_protected(path: &str, protected: &BTreeSet<String>) -> bool {
    protected.iter().any(|ancestor| {
        path == ancestor
            || path
                .strip_prefix(ancestor)
                .is_some_and(|suffix| suffix.starts_with('/'))
    })
}

fn fence_lookup_key(object_id: Option<&str>, revision: Option<&str>) -> Option<String> {
    object_id
        .zip(revision)
        .map(|(id, revision)| format!("{}:{id}:{revision}", id.len()))
}

fn fence_suppresses(
    fence: &RevisionFence,
    object_id: Option<&str>,
    revision: Option<&str>,
    _path: &str,
) -> bool {
    object_id == Some(fence.object_id.as_str()) && revision == Some(fence.revision.as_str())
}

fn action_path(action: &CfapiAction) -> &str {
    match action {
        CfapiAction::EnsureDirectory { path, .. }
        | CfapiAction::EnsurePlaceholder { path, .. }
        | CfapiAction::HydrateOnDemand { path, .. }
        | CfapiAction::QueueUploadOnClose { path, .. }
        | CfapiAction::MarkConflict { path, .. } => path,
    }
}
fn action_object_id(action: &CfapiAction) -> Option<&str> {
    match action {
        CfapiAction::EnsureDirectory { object_id, .. }
        | CfapiAction::EnsurePlaceholder { object_id, .. }
        | CfapiAction::HydrateOnDemand { object_id, .. }
        | CfapiAction::MarkConflict { object_id, .. } => object_id.as_deref(),
        _ => None,
    }
}
fn action_revision(action: &CfapiAction) -> Option<&str> {
    match action {
        CfapiAction::EnsureDirectory { remote_version, .. }
        | CfapiAction::MarkConflict { remote_version, .. } => remote_version.as_deref(),
        CfapiAction::EnsurePlaceholder { remote_version, .. }
        | CfapiAction::HydrateOnDemand { remote_version, .. } => Some(remote_version),
        _ => None,
    }
}

fn materialization_plan(
    plan: &CfapiActionPlan,
    protected: &BTreeSet<String>,
) -> Result<Vec<PlannedWork>> {
    let mut directories = BTreeMap::new();
    let mut files: BTreeMap<String, Vec<CfapiAction>> = BTreeMap::new();
    for action in &plan.actions {
        let path = action_path(action);
        if is_protected(path, protected) || matches!(action, CfapiAction::QueueUploadOnClose { .. })
        {
            continue;
        }
        for parent in parent_directories_for_path(path) {
            directories
                .entry(parent.clone())
                .or_insert(CfapiAction::EnsureDirectory {
                    object_id: None,
                    path: parent,
                    remote_version: None,
                });
        }
        match action {
            CfapiAction::EnsureDirectory { .. } => {
                directories.insert(path.to_owned(), action.clone());
            }
            _ => files
                .entry(
                    path.rsplit_once('/')
                        .map_or("", |(parent, _)| parent)
                        .to_owned(),
                )
                .or_default()
                .push(action.clone()),
        }
    }
    let mut jobs = Vec::new();
    for (path, action) in &directories {
        let mut job = PlannedWork::new(
            format!("directory:{path}"),
            format!("path:{path}"),
            serde_json::to_value(Materialization {
                actions: vec![action.clone()],
                availability: ContentAvailability::MetadataOnly,
            })?,
        );
        job.depends_on = parent_directories_for_path(path)
            .into_iter()
            .filter(|parent| directories.contains_key(parent))
            .map(|parent| format!("directory:{parent}"))
            .collect();
        jobs.push(job);
    }
    for (parent, mut actions) in files {
        actions.sort_by(|left, right| action_path(left).cmp(action_path(right)));
        for (index, chunk) in actions.chunks(64).enumerate() {
            let mut job = PlannedWork::new(
                format!("files:{parent}:{index:08}"),
                format!("directory-children:{parent}"),
                serde_json::to_value(Materialization {
                    actions: chunk.to_vec(),
                    availability: ContentAvailability::MetadataOnly,
                })?,
            );
            if !parent.is_empty() {
                job.depends_on.push(format!("directory:{parent}"));
            }
            jobs.push(job);
        }
    }
    Ok(jobs)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct RecordingUploader {
        deletes: Mutex<Vec<(String, String)>>,
        uploads: Mutex<usize>,
    }
    impl Uploader for RecordingUploader {
        fn upload_reader(
            &self,
            _path: &str,
            _reader: &mut dyn std::io::Read,
            _length: u64,
        ) -> Result<crate::runtime::UploadReceipt> {
            anyhow::bail!("unexpected path-only upload")
        }
        fn upload_reader_for_object(
            &self,
            _path: &str,
            _object_id: Option<&str>,
            _revision: Option<&str>,
            _reader: &mut dyn std::io::Read,
            _length: u64,
        ) -> Result<crate::runtime::UploadReceipt> {
            *self.uploads.lock().unwrap() += 1;
            Ok(crate::runtime::UploadReceipt {
                object_id: Some("test-object".into()),
                remote_version: Some("accepted-next".into()),
                in_sync_content_fingerprint: None,
            })
        }
        fn delete_object(&self, object_id: &str, revision: &str) -> Result<()> {
            self.deletes
                .lock()
                .unwrap()
                .push((object_id.into(), revision.into()));
            Ok(())
        }
    }
    fn root() -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("berrykeep-cfapi-work-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }
    fn file(path: &str) -> CfapiAction {
        CfapiAction::EnsurePlaceholder {
            object_id: Some(format!("object-{path}")),
            path: path.into(),
            remote_version: "r1".into(),
            remote_content_hash: "hash".into(),
            remote_size: Some(1),
            remote_content_fingerprint: None,
            remote_modified_at_unix: None,
            remote_media: None,
        }
    }
    #[test]
    fn idle_work_wait_has_no_deadline_and_scheduling_cannot_lose_wakeup() {
        let work = CfapiWork::new(
            root(),
            uuid::Uuid::nil(),
            WorkStore::open(":memory:").unwrap(),
            RemoteAppliedTracker::default(),
        )
        .unwrap();
        assert_eq!(work.next_execution_at().unwrap(), None);
        let epoch = work.wake_epoch();
        work.schedule(
            &CfapiActionPlan {
                actions: vec![file("wake.txt")],
            },
            &BTreeMap::new(),
        )
        .unwrap();
        let before = std::time::Instant::now();
        work.wait_for_work(epoch, std::time::Duration::from_secs(5));
        assert!(
            before.elapsed() < std::time::Duration::from_secs(1),
            "enqueue before wait must wake immediately"
        );
        assert!(work.next_execution_at().unwrap().is_some());
    }

    #[test]
    fn large_tree_plan_is_bounded_and_parents_gate_children() {
        let mut actions = vec![CfapiAction::EnsureDirectory {
            object_id: None,
            path: "docs".into(),
            remote_version: None,
        }];
        for index in 0..1000 {
            actions.push(file(&format!("docs/file-{index:04}")));
        }
        let jobs = materialization_plan(
            &CfapiActionPlan { actions },
            &std::collections::BTreeSet::new(),
        )
        .unwrap();
        assert!(jobs.iter().all(|job| {
            serde_json::from_value::<Materialization>(job.payload.clone())
                .unwrap()
                .actions
                .len()
                <= 64
        }));
        let root = jobs.iter().find(|job| job.key == "directory:docs").unwrap();
        assert!(
            jobs.iter()
                .filter(|job| job.key != root.key)
                .all(|job| job.depends_on.contains(&root.key))
        );
    }
    #[test]
    fn pending_local_directory_intent_suppresses_remote_descendants() {
        let jobs = materialization_plan(
            &CfapiActionPlan {
                actions: vec![file("docs/a"), file("other/b")],
            },
            &std::collections::BTreeSet::from(["docs".into()]),
        )
        .unwrap();
        let paths = jobs
            .iter()
            .flat_map(|job| {
                serde_json::from_value::<Materialization>(job.payload.clone())
                    .unwrap()
                    .actions
            })
            .map(|action| action_path(&action).to_string())
            .collect::<Vec<_>>();
        assert!(!paths.iter().any(|path| path.starts_with("docs")));
        assert!(paths.iter().any(|path| path == "other/b"));
    }

    #[test]
    fn completed_delete_fence_survives_restart_and_rejects_pre_replay_snapshot() {
        let root = root();
        let journal = root.join("state/work.sqlite");
        let work = CfapiWork::new(
            root.clone(),
            uuid::Uuid::nil(),
            WorkStore::open(&journal).unwrap(),
            RemoteAppliedTracker::default(),
        )
        .unwrap();
        let intent = LocalMutation::Delete {
            path: "a.txt".into(),
            object_id: "object-a.txt".into(),
            revision: "r1".into(),
            is_dir: false,
        };
        work.enqueue_intent(&intent).unwrap();
        let uploader = RecordingUploader::default();
        work.drain_mutations(&uploader).unwrap();
        assert!(work.local_intents().unwrap().is_empty());
        let reopened = CfapiWork::new(
            root,
            uuid::Uuid::nil(),
            WorkStore::open(journal).unwrap(),
            RemoteAppliedTracker::default(),
        )
        .unwrap();
        reopened
            .schedule(
                &CfapiActionPlan {
                    actions: vec![file("a.txt")],
                },
                &BTreeMap::new(),
            )
            .unwrap();
        assert_eq!(
            reopened.status().unwrap().pending,
            0,
            "a snapshot fetched before local delete replay must not recreate the old revision"
        );
        let mut restored = file("a.txt");
        if let CfapiAction::EnsurePlaceholder { remote_version, .. } = &mut restored {
            *remote_version = "restored-r2".into();
        }
        reopened
            .schedule(
                &CfapiActionPlan {
                    actions: vec![restored],
                },
                &BTreeMap::new(),
            )
            .unwrap();
        assert_eq!(
            reopened.status().unwrap().pending,
            1,
            "a deliberate newer restore remains eligible"
        );
    }

    #[test]
    fn accepted_receipt_is_not_erased_by_an_older_scan_and_drives_immediate_delete() {
        let root = root();
        let journal = root.join("state/work.sqlite");
        let work = CfapiWork::new(
            root.clone(),
            uuid::Uuid::nil(),
            WorkStore::open(&journal).unwrap(),
            RemoteAppliedTracker::default(),
        )
        .unwrap();
        work.record_receipt(
            "gone.txt",
            &crate::runtime::UploadReceipt {
                object_id: Some("uploaded-object".into()),
                remote_version: Some("uploaded-r1".into()),
                in_sync_content_fingerprint: None,
            },
        )
        .unwrap();
        work.persist_prior(&HashMap::new(), &HashMap::new())
            .unwrap();
        assert_eq!(
            work.prior()["gone.txt"].object_id.as_deref(),
            Some("uploaded-object")
        );
        let uploader = Arc::new(RecordingUploader::default());
        let mut monitor = SyncRootMonitor::from_store(
            "test",
            root,
            uuid::Uuid::nil(),
            uploader.clone(),
            WorkStore::open(journal).unwrap(),
        )
        .unwrap();
        monitor.prepare_startup().unwrap();
        assert_eq!(
            *uploader.deletes.lock().unwrap(),
            vec![("uploaded-object".into(), "uploaded-r1".into())]
        );
    }

    #[test]
    fn accepted_upload_before_identity_repair_recovers_own_revision_after_restart() {
        let (sync_root, provider) =
            crate::monitor::tests::registered_monitor_test_sync_root("accepted-upload-recovery");
        let path = "a.txt";
        crate::monitor::tests::create_clean_provider_placeholder(
            &sync_root.root_path,
            provider,
            path,
        );
        let initial = snapshot_entry_with_placeholder_state(
            &sync_root.root_path,
            path,
            &sync_root.root_path.join(path),
            false,
            Ok(CF_PLACEHOLDER_STATE_NO_STATES),
        );
        assert!(
            initial.is_placeholder,
            "initial={}",
            initial.to_log_string()
        );
        assert!(initial.placeholder_state.unwrap().is_partial);
        assert!(initial.materialized_file.is_none());
        let state = root().join("work.sqlite");
        let work = CfapiWork::new(
            sync_root.root_path.clone(),
            provider,
            WorkStore::open(&state).unwrap(),
            RemoteAppliedTracker::default(),
        )
        .unwrap();
        work.record_accepted_receipt(
            path,
            Some("test-object"),
            Some("newer-revision"),
            &crate::runtime::UploadReceipt {
                object_id: Some("test-object".into()),
                remote_version: Some("accepted-r2".into()),
                in_sync_content_fingerprint: None,
            },
        )
        .unwrap();
        let reopened = CfapiWork::new(
            sync_root.root_path.clone(),
            provider,
            WorkStore::open(state).unwrap(),
            RemoteAppliedTracker::default(),
        )
        .unwrap();
        reopened.recover_upload_observations().unwrap();
        reopened.drain_upload_receipts().unwrap();
        let entry = snapshot_entry(
            &sync_root.root_path,
            path,
            &sync_root.root_path.join(path),
            false,
        );
        assert_eq!(
            entry.placeholder_revision.as_deref(),
            Some("accepted-r2"),
            "state={} status={:?}",
            entry.to_log_string(),
            reopened.status().unwrap()
        );
        assert_eq!(
            entry.placeholder_state.unwrap().on_disk_data_size,
            0,
            "receipt repair must not hydrate a placeholder"
        );
        let inner = Arc::new(RecordingUploader::default());
        let uploader = JournaledUploader {
            inner: inner.clone(),
            work: reopened,
        };
        uploader
            .upload_reader_for_object(
                path,
                Some("test-object"),
                Some("accepted-r2"),
                &mut std::io::Cursor::new(Vec::<u8>::new()),
                0,
            )
            .unwrap();
        assert_eq!(
            *inner.uploads.lock().unwrap(),
            1,
            "own accepted revision must not permanently reject later local uploads"
        );
    }

    #[test]
    fn accepted_receipt_repair_waits_through_close_upload_finalization() {
        use crate::close_upload::{
            UploadConcurrencyGate, UploadWorkerContext, process_close_upload_for_test,
        };
        use std::sync::mpsc;
        use std::time::Duration;

        struct UploadWithConcurrentRepair {
            work: CfapiWork,
            start: mpsc::Sender<()>,
            finished: Mutex<mpsc::Receiver<()>>,
        }
        impl Uploader for UploadWithConcurrentRepair {
            fn upload_reader(
                &self,
                path: &str,
                _reader: &mut dyn std::io::Read,
                _length: u64,
            ) -> Result<crate::runtime::UploadReceipt> {
                let receipt = crate::runtime::UploadReceipt {
                    object_id: Some("test-object".into()),
                    remote_version: Some("accepted-r2".into()),
                    in_sync_content_fingerprint: None,
                };
                // Simulate the accepted receipt boundary while the close callback
                // still owns its pre-finalization snapshot and USN.
                self.work
                    .record_accepted_receipt(path, None, None, &receipt)?;
                self.start.send(()).unwrap();
                assert!(
                    matches!(
                        self.finished
                            .lock()
                            .unwrap()
                            .recv_timeout(Duration::from_millis(100)),
                        Err(mpsc::RecvTimeoutError::Timeout)
                    ),
                    "receipt repair ran inside the close-upload finalization window"
                );
                Ok(receipt)
            }
        }

        let (sync_root, provider) =
            super::super::tests::registered_monitor_test_sync_root("serialized-receipt");
        std::fs::write(sync_root.root_path.join("a.txt"), b"local upload").unwrap();
        let work = CfapiWork::new(
            sync_root.root_path.clone(),
            provider,
            WorkStore::open(root().join("work.sqlite")).unwrap(),
            RemoteAppliedTracker::default(),
        )
        .unwrap();
        let (start, started) = mpsc::channel();
        let (done, finished) = mpsc::channel();
        let inner = Arc::new(UploadWithConcurrentRepair {
            work: work.clone(),
            start,
            finished: Mutex::new(finished),
        });
        let repair_work = work.clone();
        let repair = std::thread::spawn(move || {
            started.recv().unwrap();
            repair_work.drain_upload_receipts().unwrap();
            done.send(()).unwrap();
        });
        let worker = UploadWorkerContext {
            sync_root: sync_root.root_path.clone(),
            provider_instance_id: provider,
            runtime: Arc::new(crate::runtime::CfapiRuntime::default()),
            uploader: Arc::new(JournaledUploader {
                inner: inner.clone(),
                work,
            }),
            upload_gate: Arc::new(UploadConcurrencyGate::new(1)),
        };
        assert!(process_close_upload_for_test(&worker, "a.txt"));
        inner
            .finished
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        repair.join().unwrap();
        let info = crate::cfapi::cf_get_placeholder_standard_info(
            &std::fs::File::open(sync_root.root_path.join("a.txt")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            info.InSyncState,
            windows::Win32::Storage::CloudFilters::CF_IN_SYNC_STATE_IN_SYNC.0
        );
    }

    #[test]
    fn interrupted_partial_placeholder_batch_resumes_without_another_snapshot() {
        let (sync_root, provider) =
            crate::monitor::tests::registered_monitor_test_sync_root("partial-batch-resume");
        let state = root().join("work.sqlite");
        let work = CfapiWork::new(
            sync_root.root_path.clone(),
            provider,
            WorkStore::open(&state).unwrap(),
            RemoteAppliedTracker::default(),
        )
        .unwrap();
        let actions = (0..100)
            .map(|index| file(&format!("docs/file-{index:03}")))
            .collect();
        work.schedule(&CfapiActionPlan { actions }, &BTreeMap::new())
            .unwrap();
        let directory = work
            .store
            .claim_next(MATERIALIZATION_SCOPE, 0, 1)
            .unwrap()
            .unwrap();
        let payload: Materialization =
            serde_json::from_value(directory.work.payload.clone()).unwrap();
        assert!(matches!(
            work.apply_materialization(&payload).unwrap(),
            WorkResult::Complete
        ));
        work.store
            .finish(&directory, WorkResult::Complete, 0)
            .unwrap();
        let batch = work
            .store
            .claim_next(MATERIALIZATION_SCOPE, 0, 1)
            .unwrap()
            .unwrap();
        let mut payload: Materialization =
            serde_json::from_value(batch.work.payload.clone()).unwrap();
        payload.actions.truncate(32);
        assert!(matches!(
            work.apply_materialization(&payload).unwrap(),
            WorkResult::Complete
        ));
        // No acknowledgement: a new process sees the partially applied running batch.
        let reopened = CfapiWork::new(
            sync_root.root_path.clone(),
            provider,
            WorkStore::open(state).unwrap(),
            RemoteAppliedTracker::default(),
        )
        .unwrap();
        reopened.drain_materialization().unwrap();
        assert_eq!(reopened.status().unwrap().pending, 0);
        for index in 0..100 {
            let path = format!("docs/file-{index:03}");
            let entry = snapshot_entry(
                &sync_root.root_path,
                &path,
                &sync_root.root_path.join(&path),
                false,
            );
            assert_eq!(
                entry.placeholder_object_id.as_deref(),
                Some(format!("object-{path}").as_str())
            );
            assert_eq!(entry.placeholder_state.unwrap().on_disk_data_size, 0);
        }
    }
}
