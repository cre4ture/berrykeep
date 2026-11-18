use super::*;
mod controls;
mod materialization;
mod namespace;
#[cfg(test)]
mod tests;
mod uploads;
use anyhow::Result;
#[cfg(test)]
use materialization::materialization_plan;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use sync_agent_core::sync_work::{ContentAvailability, PlannedWork, WorkResult, WorkStore, now_ms};
pub(super) use uploads::JournaledUploader;

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
        #[serde(default)]
        renamed_from: Option<String>,
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
            Self::Delete {
                path, renamed_from, ..
            } => std::iter::once(path.as_str())
                .chain(renamed_from.as_deref())
                .collect(),
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
