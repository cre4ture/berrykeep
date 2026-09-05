use std::collections::{HashMap, HashSet};
use std::os::windows::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::adapter::{CfapiAction, CfapiActionPlan};
use crate::auth::is_internal_client_identity_relative_path;
use crate::cfapi::{
    cf_dehydrate_placeholder_with_oplock, cf_get_placeholder_standard_info,
    cf_get_placeholder_standard_info_with_identity, cf_hydrate_placeholder, cf_set_in_sync,
    cf_set_in_sync_with_usn, cf_set_not_in_sync, describe_path_state, open_sync_path,
    path_is_placeholder, path_placeholder_state, try_convert_materialized_file,
};
use crate::cfapi_safe_wrap::local_file_identity_for_path;
use crate::connection_config::is_internal_connection_bootstrap_relative_path;
use crate::helpers::{
    PlaceholderFileIdentity, decode_placeholder_file_identity, error_chain_has_win32_hresult,
    normalize_path, path_to_relative,
};
use crate::hydration_control::is_active_hydration_marked;
use crate::placeholder_metadata::{
    RemoteObjectReconcileReport, promote_remote_to_in_sync_content_baseline,
    record_in_sync_local_file_state, record_uploaded_object_state,
};
#[cfg(test)]
use crate::runtime::UploadReceipt;
use crate::runtime::{ObjectRenameReceipt, Uploader, is_remote_mutation_conflict};
use crate::snapshot_cache::is_internal_remote_snapshot_relative_path;
use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;
use windows_sys::Win32::Storage::CloudFilters::{
    CF_IN_SYNC_STATE_IN_SYNC, CF_PIN_STATE_PINNED, CF_PIN_STATE_UNPINNED,
    CF_PLACEHOLDER_STATE_NO_STATES, CF_PLACEHOLDER_STATE_PARTIAL,
};
use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_PINNED, FILE_ATTRIBUTE_UNPINNED};

const DEHYDRATE_SHARING_VIOLATION_MAX_RETRIES: usize = 8;
const DEHYDRATE_SHARING_VIOLATION_RETRY_DELAY_MS: u64 = 250;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct LocalFileIdentity {
    volume_serial_number: u32,
    file_index: u64,
}

impl LocalFileIdentity {
    fn from_path(path: &std::path::Path) -> Option<Self> {
        let (volume_serial_number, file_index) = local_file_identity_for_path(path).ok()?;
        Some(Self {
            volume_serial_number,
            file_index,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SeenEntry {
    is_dir: bool,
    file_attributes: u32,
    local_file_identity: Option<LocalFileIdentity>,
    placeholder_identity_path: Option<String>,
    placeholder_object_id: Option<String>,
    placeholder_revision: Option<String>,
    placeholder_state: Option<PlaceholderSnapshot>,
    provider_hydration_active: bool,
}

impl SeenEntry {
    fn has_pinned_attribute(&self) -> bool {
        (self.file_attributes & FILE_ATTRIBUTE_PINNED) != 0
    }

    fn has_unpinned_attribute(&self) -> bool {
        (self.file_attributes & FILE_ATTRIBUTE_UNPINNED) != 0
    }

    fn to_log_string(&self) -> String {
        let placeholder_state = self
            .placeholder_state
            .map(|state| state.to_log_string())
            .unwrap_or_else(|| String::from("none"));
        let local_file_identity = self
            .local_file_identity
            .map(|identity| {
                format!(
                    "{:08x}:{:016x}",
                    identity.volume_serial_number, identity.file_index
                )
            })
            .unwrap_or_else(|| String::from("none"));
        let placeholder_identity_path = self
            .placeholder_identity_path
            .clone()
            .unwrap_or_else(|| String::from("none"));
        let placeholder_object_id = self.placeholder_object_id.as_deref().unwrap_or("none");
        let placeholder_revision = self.placeholder_revision.as_deref().unwrap_or("none");
        format!(
            "dir={} attrs=0x{:08x} pinned_attr={} unpinned_attr={} file_id={} placeholder_path={} object_id={} revision={} placeholder_probe={} hydration_active={}",
            self.is_dir,
            self.file_attributes,
            self.has_pinned_attribute(),
            self.has_unpinned_attribute(),
            local_file_identity,
            placeholder_identity_path,
            placeholder_object_id,
            placeholder_revision,
            placeholder_state,
            self.provider_hydration_active,
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LocalRenamePair {
    from_path: String,
    to_path: String,
    detection: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PlaceholderSnapshot {
    on_disk_data_size: i64,
    modified_data_size: i64,
    in_sync_state: i32,
    pin_state: i32,
    is_partial: bool,
}

impl PlaceholderSnapshot {
    fn from_path(path: &std::path::Path, placeholder_state_bits: u32) -> Option<Self> {
        let file = match open_sync_path(path, false) {
            Ok(file) => file,
            Err(err) => {
                tracing::info!(
                    "monitor: dehydrate probe open failed path={} error={} state={}",
                    path.display(),
                    err,
                    describe_path_state(path)
                );
                return None;
            }
        };
        let info = match cf_get_placeholder_standard_info(&file) {
            Ok(info) => info,
            Err(err) => {
                tracing::info!(
                    "monitor: dehydrate probe placeholder-info failed path={} error={} state={}",
                    path.display(),
                    err,
                    describe_path_state(path)
                );
                return None;
            }
        };
        Some(Self {
            on_disk_data_size: info.OnDiskDataSize,
            modified_data_size: info.ModifiedDataSize,
            in_sync_state: info.InSyncState,
            pin_state: info.PinState,
            is_partial: (placeholder_state_bits & CF_PLACEHOLDER_STATE_PARTIAL) != 0,
        })
    }

    fn should_dehydrate(self) -> bool {
        self.pin_state == CF_PIN_STATE_UNPINNED
            && self.in_sync_state == CF_IN_SYNC_STATE_IN_SYNC
            && self.modified_data_size == 0
            && self.on_disk_data_size > 0
    }

    fn should_hydrate(self) -> bool {
        self.pin_state == CF_PIN_STATE_PINNED
            && self.in_sync_state == CF_IN_SYNC_STATE_IN_SYNC
            && self.modified_data_size == 0
            && self.is_partial
    }

    fn block_reason(self) -> &'static str {
        if self.pin_state != CF_PIN_STATE_UNPINNED {
            "pin-state-not-unpinned"
        } else if self.in_sync_state != CF_IN_SYNC_STATE_IN_SYNC {
            "not-in-sync"
        } else if self.modified_data_size != 0 {
            "modified-data-present"
        } else if self.on_disk_data_size <= 0 {
            "already-dehydrated"
        } else {
            "eligible"
        }
    }

    fn to_log_string(self) -> String {
        format!(
            "on_disk={} modified={} in_sync={} pin={} partial={}",
            self.on_disk_data_size,
            self.modified_data_size,
            self.in_sync_state,
            self.pin_state,
            self.is_partial
        )
    }
}

fn should_schedule_placeholder_hydration(
    previous_entry: Option<&SeenEntry>,
    entry: &SeenEntry,
    placeholder_state: PlaceholderSnapshot,
) -> bool {
    if !entry.has_pinned_attribute()
        || !placeholder_state.should_hydrate()
        || entry.provider_hydration_active
    {
        return false;
    }

    let Some(previous_entry) = previous_entry else {
        return true;
    };

    if previous_entry == entry {
        return false;
    }

    let previous_was_hydrate_eligible = previous_entry.has_pinned_attribute()
        && previous_entry
            .placeholder_state
            .is_some_and(PlaceholderSnapshot::should_hydrate);

    !previous_was_hydrate_eligible
        || previous_entry.provider_hydration_active
        || previous_entry
            .placeholder_state
            .filter(|previous| previous.should_hydrate())
            .is_some_and(|previous| previous != placeholder_state)
}

fn should_skip_clean_placeholder_content_upload(
    previous_entry: Option<&SeenEntry>,
    entry: &SeenEntry,
) -> bool {
    let Some(previous_entry) = previous_entry else {
        return false;
    };
    let Some(placeholder_state) = entry.placeholder_state else {
        return false;
    };
    if placeholder_state.modified_data_size != 0 {
        return false;
    }

    previous_entry.placeholder_state.is_some()
        || previous_entry.local_file_identity == entry.local_file_identity
}

pub struct SyncRootMonitor {
    name: String,
    sync_root: PathBuf,
    provider_instance_id: uuid::Uuid,
    uploader: Arc<dyn Uploader>,
    seen: HashMap<String, SeenEntry>,
    pending_object_renames: Vec<LocalRenamePair>,
    dehydrations_in_flight: Arc<Mutex<HashSet<String>>>,
    hydrations_in_flight: Arc<Mutex<HashSet<String>>>,
    remote_applied_tracker: RemoteAppliedTracker,
    refresh_gate: Arc<Mutex<()>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DehydrateScanSummary {
    total_entries: usize,
    pinned_attribute_count: usize,
    unpinned_attribute_count: usize,
    probed_placeholder_count: usize,
    hydrate_eligible_count: usize,
    eligible_count: usize,
}

#[derive(Debug, Default)]
struct SnapshotEntries {
    entries: HashMap<String, SeenEntry>,
    walk_error_count: usize,
    walk_error_samples: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct RemoteAppliedTracker {
    directories: Arc<Mutex<HashSet<String>>>,
    removed_files: Arc<Mutex<HashSet<String>>>,
    created_files: Arc<Mutex<HashSet<String>>>,
}

impl RemoteAppliedTracker {
    pub fn record_plan(&self, plan: &CfapiActionPlan) {
        let mut directories = self
            .directories
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        for action in &plan.actions {
            match action {
                CfapiAction::EnsureDirectory { path, .. } => {
                    record_remote_applied_directory(path, &mut directories);
                }
                CfapiAction::EnsurePlaceholder { path, .. }
                | CfapiAction::HydrateOnDemand { path, .. } => {
                    for parent in parent_directories_for_path(path) {
                        record_remote_applied_directory(&parent, &mut directories);
                    }
                }
                CfapiAction::QueueUploadOnClose { .. } | CfapiAction::MarkConflict { .. } => {}
            }
        }
    }

    pub fn record_reconcile_report(&self, report: &RemoteObjectReconcileReport) {
        let mut removed_files = self
            .removed_files
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        removed_files.extend(report.deleted_paths.iter().cloned());
        removed_files.extend(report.renamed_paths.keys().cloned());

        let mut created_files = self
            .created_files
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        created_files.extend(report.renamed_paths.values().cloned());
    }

    fn take_directory_suppression(&self, path: &str) -> bool {
        let normalized = normalize_monitor_relative_path(path);
        if normalized.is_empty() {
            return false;
        }

        self.directories
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&normalized)
    }

    fn take_file_removal_suppression(&self, path: &str) -> bool {
        self.removed_files
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&normalize_monitor_relative_path(path))
    }

    fn take_file_creation_suppression(&self, path: &str) -> bool {
        self.created_files
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&normalize_monitor_relative_path(path))
    }
}

impl SyncRootMonitor {
    pub fn new(
        name: &str,
        sync_root: PathBuf,
        provider_instance_id: uuid::Uuid,
        uploader: Arc<dyn Uploader>,
    ) -> Self {
        Self {
            name: name.to_string(),
            sync_root,
            provider_instance_id,
            uploader,
            seen: HashMap::new(),
            pending_object_renames: Vec::new(),
            dehydrations_in_flight: Arc::new(Mutex::new(HashSet::new())),
            hydrations_in_flight: Arc::new(Mutex::new(HashSet::new())),
            remote_applied_tracker: RemoteAppliedTracker::default(),
            refresh_gate: Arc::new(Mutex::new(())),
        }
    }

    pub fn remote_applied_tracker(&self) -> RemoteAppliedTracker {
        self.remote_applied_tracker.clone()
    }

    pub fn refresh_gate(&self) -> Arc<Mutex<()>> {
        self.refresh_gate.clone()
    }

    pub fn run(&mut self) {
        loop {
            self.walk();
            std::thread::sleep(Duration::from_secs(5));
        }
    }

    pub fn seed_seen(&mut self) {
        let snapshot = self.snapshot_entries();
        if snapshot.walk_error_count != 0 {
            tracing::info!(
                "{}: seed_seen captured partial snapshot due to {} walk errors sample={:?}",
                self.name,
                snapshot.walk_error_count,
                snapshot.walk_error_samples
            );
        }
        self.seen = snapshot.entries;
    }

    pub fn seed_remote_entries(&mut self, plan: &CfapiActionPlan) {
        self.seed_remote_entries_with_suppressed_paths(plan, &std::collections::BTreeSet::new());
    }

    pub fn seed_remote_entries_with_suppressed_paths(
        &mut self,
        plan: &CfapiActionPlan,
        suppressed_paths: &std::collections::BTreeSet<String>,
    ) {
        let mut seeded = HashMap::new();
        for action in &plan.actions {
            match action {
                CfapiAction::EnsureDirectory { path, .. } => {
                    self.seed_existing_entry(&mut seeded, path, true);
                }
                CfapiAction::EnsurePlaceholder { path, .. }
                | CfapiAction::HydrateOnDemand { path, .. } => {
                    self.seed_existing_entry(&mut seeded, path, false);
                    for parent in parent_directories_for_path(path) {
                        self.seed_existing_entry(&mut seeded, &parent, true);
                    }
                }
                CfapiAction::QueueUploadOnClose { .. } | CfapiAction::MarkConflict { .. } => {}
            }
        }
        for path in suppressed_paths {
            self.seed_existing_entry(&mut seeded, path, false);
            for parent in parent_directories_for_path(path) {
                self.seed_existing_entry(&mut seeded, &parent, true);
            }
        }
        self.seen = seeded;
    }

    pub fn walk(&mut self) {
        let refresh_gate = self.refresh_gate.clone();
        let _refresh_gate = refresh_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let SnapshotEntries {
            entries: current,
            walk_error_count,
            walk_error_samples,
        } = self.snapshot_entries();
        let mut current = current;
        let handled_renames = self.handle_local_object_renames(&mut current);
        let dehydrate_summary = summarize_dehydrate_scan(&current);
        let paths = current.keys().cloned().collect::<Vec<_>>();
        for rel_path in paths {
            if handled_renames.contains(&rel_path) {
                continue;
            }
            let path = self
                .sync_root
                .join(rel_path.replace('/', std::path::MAIN_SEPARATOR.to_string().as_str()));
            self.handle_entry(&path, rel_path, &mut current);
        }

        self.log_dehydrate_scan_summary(dehydrate_summary);
        if walk_error_count == 0 {
            self.handle_deleted_entries(&current, &handled_renames);
        } else {
            let preserved_count = self
                .preserve_missing_entries_after_incomplete_snapshot(&mut current, &handled_renames);
            tracing::info!(
                "{}: snapshot scan encountered {} walk errors; suppressing delete detection for this pass and preserving {} prior entries sample={:?}",
                self.name,
                walk_error_count,
                preserved_count,
                walk_error_samples
            );
        }
        self.seen = current;
    }

    fn handle_local_object_renames(
        &mut self,
        current: &mut HashMap<String, SeenEntry>,
    ) -> std::collections::HashSet<String> {
        let mut rename_pairs = detect_local_object_renames(&self.seen, current);
        let detected_pairs = rename_pairs
            .iter()
            .map(|rename| (rename.from_path.clone(), rename.to_path.clone()))
            .collect::<std::collections::HashSet<_>>();
        for mut pending in std::mem::take(&mut self.pending_object_renames) {
            let key = (pending.from_path.clone(), pending.to_path.clone());
            if detected_pairs.contains(&key) {
                continue;
            }
            let retry_is_still_safe = !current.contains_key(&pending.from_path)
                && current.get(&pending.to_path).is_some_and(|entry| {
                    entry.placeholder_identity_path.as_deref() == Some(pending.from_path.as_str())
                        && entry.placeholder_object_id.is_some()
                        && entry.placeholder_revision.is_some()
                });
            if retry_is_still_safe {
                pending.detection = "retry-after-transient-error";
                rename_pairs.push(pending);
            }
        }
        rename_pairs.sort_by(|left, right| left.from_path.cmp(&right.from_path));
        let mut handled_paths = std::collections::HashSet::new();

        for rename in rename_pairs {
            let full_path = self.sync_root.join(
                rename
                    .to_path
                    .replace('/', std::path::MAIN_SEPARATOR.to_string().as_str()),
            );
            tracing::info!(
                "{}: detected local object rename {} -> {} detection={} state_before={} state_after={}",
                self.name,
                rename.from_path,
                rename.to_path,
                rename.detection,
                self.seen
                    .get(&rename.from_path)
                    .map(|entry| entry.to_log_string())
                    .unwrap_or_else(|| String::from("<missing>")),
                current
                    .get(&rename.to_path)
                    .map(|entry| entry.to_log_string())
                    .unwrap_or_else(|| String::from("<missing>")),
            );

            let entry = current
                .get(&rename.to_path)
                .expect("object-id rename candidates must exist in the current snapshot");
            let object_id = entry
                .placeholder_object_id
                .as_deref()
                .expect("object-id rename candidates must carry object_id")
                .to_string();
            let expected_revision = entry
                .placeholder_revision
                .as_deref()
                .expect("object-id rename candidates must carry a revision")
                .to_string();
            let is_dir = entry.is_dir;
            let remote_destination = remote_rename_destination(&rename.to_path, is_dir);
            match self
                .uploader
                .rename_object(&object_id, &expected_revision, &remote_destination)
            {
                Ok(receipt) => {
                    handled_paths.insert(rename.from_path.clone());
                    handled_paths.insert(rename.to_path.clone());
                    if receipt.object_id != object_id || receipt.remote_version.trim().is_empty() {
                        tracing::error!(
                            "{}: remote rename returned an invalid post-mutation identity for {} -> {}; preserving local move as a conflict",
                            self.name,
                            rename.from_path,
                            rename.to_path
                        );
                        mark_local_rename_conflict(&full_path);
                        continue;
                    }
                    tracing::info!(
                        "{}: remote rename applied {} -> {} raw_state={}",
                        self.name,
                        rename.from_path,
                        rename.to_path,
                        describe_path_state(&full_path)
                    );
                    if let Err(err) = repair_locally_renamed_object(
                        &self.sync_root,
                        &full_path,
                        &rename.to_path,
                        self.provider_instance_id,
                        is_dir,
                        &receipt,
                    ) {
                        tracing::info!(
                            "{}: failed to repair local renamed file {} after remote rename: {:#} state={}",
                            self.name,
                            rename.to_path,
                            err,
                            describe_path_state(&full_path)
                        );
                        mark_local_rename_conflict(&full_path);
                    } else {
                        self.seed_existing_entry(current, &rename.to_path, is_dir);
                    }
                }
                Err(err) if is_remote_mutation_conflict(&err) => {
                    handled_paths.insert(rename.from_path.clone());
                    handled_paths.insert(rename.to_path.clone());
                    tracing::info!(
                        "{}: remote rename conflicted {} -> {}: {:#}; preserving the local move without upload/delete fallback",
                        self.name,
                        rename.from_path,
                        rename.to_path,
                        err
                    );
                    mark_local_rename_conflict(&full_path);
                }
                Err(err) => {
                    // A transport failure says nothing about the remote object
                    // state. Keep the pre-rename entry in the next baseline so
                    // this same object-id rename is detected and retried on
                    // the next scan, while suppressing path-based delete or
                    // upload fallbacks for the current pass.
                    self.pending_object_renames.push(rename.clone());
                    handled_paths.insert(rename.from_path.clone());
                    handled_paths.insert(rename.to_path.clone());
                    tracing::warn!(
                        "{}: transient remote rename failure {} -> {}: {:#}; retaining the object-id rename for retry",
                        self.name,
                        rename.from_path,
                        rename.to_path,
                        err
                    );
                }
            }
        }

        handled_paths
    }

    fn log_dehydrate_scan_summary(&self, summary: DehydrateScanSummary) {
        let hydrate_in_flight_count = self
            .hydrations_in_flight
            .lock()
            .expect("hydrations_in_flight lock poisoned")
            .len();
        let in_flight_count = self
            .dehydrations_in_flight
            .lock()
            .expect("dehydrations_in_flight lock poisoned")
            .len();
        if summary.pinned_attribute_count == 0
            && summary.unpinned_attribute_count == 0
            && summary.probed_placeholder_count == 0
            && summary.hydrate_eligible_count == 0
            && summary.eligible_count == 0
            && hydrate_in_flight_count == 0
            && in_flight_count == 0
        {
            return;
        }

        tracing::info!(
            "{}: dehydrate-scan total_entries={} pinned_attr={} unpinned_attr={} probed_placeholders={} hydrate_eligible={} eligible={} hydrate_in_flight={} in_flight={}",
            self.name,
            summary.total_entries,
            summary.pinned_attribute_count,
            summary.unpinned_attribute_count,
            summary.probed_placeholder_count,
            summary.hydrate_eligible_count,
            summary.eligible_count,
            hydrate_in_flight_count,
            in_flight_count
        );
    }

    fn snapshot_entries(&self) -> SnapshotEntries {
        let mut snapshot = SnapshotEntries::default();
        let walker = walkdir::WalkDir::new(&self.sync_root).into_iter();
        for entry in walker {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    snapshot.walk_error_count += 1;
                    if snapshot.walk_error_samples.len() < 8 {
                        let location = err
                            .path()
                            .map(|path| {
                                let rel_path =
                                    path_to_relative(&self.sync_root, &path.to_string_lossy());
                                if rel_path.is_empty() {
                                    path.display().to_string()
                                } else {
                                    rel_path
                                }
                            })
                            .unwrap_or_else(|| String::from("<unknown>"));
                        snapshot
                            .walk_error_samples
                            .push(format!("{location}: {err}"));
                    }
                    continue;
                }
            };

            let path = entry.path();
            let rel_path = path_to_relative(&self.sync_root, &path.to_string_lossy());
            if rel_path.is_empty()
                || is_internal_client_identity_relative_path(&rel_path)
                || is_internal_connection_bootstrap_relative_path(&rel_path)
                || is_internal_remote_snapshot_relative_path(&rel_path)
            {
                continue;
            }

            snapshot.entries.insert(
                rel_path.clone(),
                snapshot_entry(&self.sync_root, &rel_path, path, entry.file_type().is_dir()),
            );
        }

        snapshot
    }

    fn preserve_missing_entries_after_incomplete_snapshot(
        &self,
        current: &mut HashMap<String, SeenEntry>,
        handled_renames: &HashSet<String>,
    ) -> usize {
        let mut preserved_count = 0;
        for (path, entry) in &self.seen {
            if current.contains_key(path) || handled_renames.contains(path) {
                continue;
            }
            current.insert(path.clone(), entry.clone());
            preserved_count += 1;
        }
        preserved_count
    }

    fn seed_existing_entry(
        &self,
        seeded: &mut HashMap<String, SeenEntry>,
        rel_path: &str,
        is_dir_hint: bool,
    ) {
        let normalized = rel_path.trim_matches(['/', '\\']).replace('\\', "/");
        if normalized.is_empty()
            || is_internal_client_identity_relative_path(&normalized)
            || is_internal_connection_bootstrap_relative_path(&normalized)
            || is_internal_remote_snapshot_relative_path(&normalized)
        {
            return;
        }

        let full_path = self
            .sync_root
            .join(normalized.replace('/', std::path::MAIN_SEPARATOR.to_string().as_str()));
        let metadata = match std::fs::metadata(&full_path) {
            Ok(metadata) => metadata,
            Err(_) => return,
        };
        seeded.insert(
            normalized,
            snapshot_entry(
                &self.sync_root,
                rel_path,
                &full_path,
                metadata.is_dir() || is_dir_hint,
            ),
        );
    }

    fn handle_entry(
        &mut self,
        path: &std::path::Path,
        rel_path: String,
        current: &mut HashMap<String, SeenEntry>,
    ) {
        if rel_path.is_empty() {
            return;
        }
        if is_internal_client_identity_relative_path(&rel_path)
            || is_internal_connection_bootstrap_relative_path(&rel_path)
            || is_internal_remote_snapshot_relative_path(&rel_path)
        {
            return;
        }
        let entry = match current.get(&rel_path) {
            Some(entry) => entry,
            None => return,
        };
        let previous_entry = self.seen.get(&rel_path);
        let entry_unchanged = previous_entry == Some(entry);

        if !entry_unchanged
            && (entry.has_unpinned_attribute()
                || previous_entry.is_some_and(SeenEntry::has_unpinned_attribute)
                || entry.placeholder_state.is_some()
                || previous_entry
                    .and_then(|value| value.placeholder_state)
                    .is_some())
        {
            tracing::info!(
                "{}: path-state changed path={} previous={} current={} raw_state={}",
                self.name,
                rel_path,
                previous_entry
                    .map(|value| value.to_log_string())
                    .unwrap_or_else(|| String::from("<new>")),
                entry.to_log_string(),
                describe_path_state(path)
            );
        }

        self.maybe_schedule_placeholder_hydrate(path, &rel_path, previous_entry, entry);
        self.maybe_schedule_placeholder_dehydrate(path, &rel_path, previous_entry, entry);

        if entry_unchanged {
            return;
        }

        if self
            .remote_applied_tracker
            .take_file_creation_suppression(&rel_path)
        {
            tracing::info!(
                "{}: suppressing local upload for remote-renamed path {}",
                self.name,
                rel_path
            );
            return;
        }

        if entry.is_dir {
            if self
                .remote_applied_tracker
                .take_directory_suppression(&rel_path)
            {
                tracing::info!(
                    "{}: suppressing local upload for remote-applied directory {}",
                    self.name,
                    rel_path
                );
                return;
            }
            tracing::info!("{}: detected new directory {}", self.name, rel_path);
            let mut cursor = std::io::Cursor::new(b"<DIR>".to_vec());
            let remote_path = directory_marker_path(&rel_path);
            if entry.placeholder_object_id.is_some() != entry.placeholder_revision.is_some() {
                tracing::warn!(
                    "{}: refusing ambiguous directory upload for {}; object_id and expected_revision are incomplete",
                    self.name,
                    rel_path
                );
                return;
            }
            match self.uploader.upload_reader_for_object(
                &remote_path,
                entry.placeholder_object_id.as_deref(),
                entry.placeholder_revision.as_deref(),
                &mut cursor,
                b"<DIR>".len() as u64,
            ) {
                Ok(receipt) => {
                    let Some((object_id, revision)) = receipt
                        .object_id
                        .as_deref()
                        .zip(receipt.remote_version.as_deref())
                    else {
                        tracing::warn!(
                            "{}: directory upload returned no stable object identity/revision for {}",
                            self.name,
                            rel_path
                        );
                        self.mark_entry_for_retry(current, &rel_path, previous_entry);
                        return;
                    };
                    if let Err(err) = record_uploaded_object_state(
                        &self.sync_root,
                        &rel_path,
                        self.provider_instance_id,
                        object_id,
                        revision,
                        None,
                    ) {
                        tracing::info!(
                            "{}: failed to record uploaded directory state for {}: {:#}",
                            self.name,
                            rel_path,
                            err
                        );
                        self.mark_entry_for_retry(current, &rel_path, previous_entry);
                    } else {
                        // The upload receipt advances the placeholder's
                        // revision. Refresh this walk's snapshot so that
                        // revision becomes the next monitor baseline instead
                        // of looking like a new local change on every scan.
                        self.seed_existing_entry(current, &rel_path, true);
                    }
                }
                Err(err) => {
                    tracing::info!(
                        "{}: failed to upload directory marker {}: {}",
                        self.name,
                        rel_path,
                        err
                    );
                    self.mark_entry_for_retry(current, &rel_path, previous_entry);
                }
            }
        } else {
            let metadata = match std::fs::metadata(path) {
                Ok(m) => m,
                Err(_) => return,
            };
            if path.exists() {
                let upload_snapshot = (metadata.len(), metadata.modified().ok());
                let was_placeholder = path_is_placeholder(path);
                if was_placeholder {
                    if should_skip_clean_placeholder_content_upload(previous_entry, entry) {
                        tracing::info!(
                            "{}: skipping upload for clean placeholder {} after metadata-only change snapshot={} raw_state={}",
                            self.name,
                            rel_path,
                            entry.to_log_string(),
                            describe_path_state(path)
                        );
                        return;
                    }
                    tracing::info!(
                        "{}: uploading placeholder-backed file {} after local change detection",
                        self.name,
                        rel_path
                    );
                }
                let mut file = match std::fs::File::open(path) {
                    Ok(file) => file,
                    Err(err) => {
                        tracing::info!(
                            "{}: failed to reopen file {} for upload: {}",
                            self.name,
                            rel_path,
                            err
                        );
                        self.mark_entry_for_retry(current, &rel_path, previous_entry);
                        return;
                    }
                };
                if entry.placeholder_object_id.is_some() != entry.placeholder_revision.is_some() {
                    tracing::warn!(
                        "{}: refusing ambiguous upload for {}; object_id and expected_revision are incomplete",
                        self.name,
                        rel_path
                    );
                    return;
                }
                match self.uploader.upload_reader_for_object(
                    &rel_path,
                    entry.placeholder_object_id.as_deref(),
                    entry.placeholder_revision.as_deref(),
                    &mut file,
                    metadata.len(),
                ) {
                    Ok(receipt) => {
                        let content_unchanged = std::fs::metadata(path)
                            .map(|after| (after.len(), after.modified().ok()) == upload_snapshot)
                            .unwrap_or(false);
                        if !was_placeholder {
                            try_convert_materialized_file(path, &rel_path, &metadata);
                        }
                        let uploaded_identity = receipt
                            .object_id
                            .as_deref()
                            .zip(receipt.remote_version.as_deref());
                        let Some((object_id, revision)) = uploaded_identity else {
                            tracing::warn!(
                                "{}: upload for {} returned no stable object identity/revision; leaving local entry dirty",
                                self.name,
                                rel_path
                            );
                            self.mark_entry_for_retry(current, &rel_path, previous_entry);
                            return;
                        };
                        if let Err(err) = record_uploaded_object_state(
                            &self.sync_root,
                            &rel_path,
                            self.provider_instance_id,
                            object_id,
                            revision,
                            receipt.in_sync_content_fingerprint.as_deref(),
                        ) {
                            tracing::info!(
                                "{}: failed to record uploaded object state for {}: {:#}",
                                self.name,
                                rel_path,
                                err
                            );
                            self.mark_entry_for_retry(current, &rel_path, previous_entry);
                            return;
                        }
                        if !content_unchanged {
                            tracing::warn!(
                                "{}: {} changed while its upload was in flight; retained object_id={} revision={} and left it dirty for a guarded retry",
                                self.name,
                                rel_path,
                                object_id,
                                revision
                            );
                            self.mark_entry_for_retry(current, &rel_path, previous_entry);
                            return;
                        }
                        match open_sync_path(path, true).and_then(|file| {
                            let mut upload_usn = cf_set_not_in_sync(&file)
                                .map_err(std::io::Error::other)?;
                            let final_snapshot = file
                                .metadata()
                                .map(|current| (current.len(), current.modified().ok()));
                            if !matches!(final_snapshot, Ok(snapshot) if snapshot == upload_snapshot)
                            {
                                return Err(std::io::Error::other(
                                    "file changed before the upload could be committed in sync",
                                ));
                            }
                            cf_set_in_sync_with_usn(&file, &mut upload_usn)
                                .map_err(std::io::Error::other)
                        }) {
                            Ok(()) => {}
                            Err(err) => {
                                tracing::info!(
                                    "{}: failed to mark uploaded file {} in sync: {:#}",
                                    self.name,
                                    rel_path,
                                    err
                                );
                                self.mark_entry_for_retry(current, &rel_path, previous_entry);
                                return;
                            }
                        }
                        self.seed_existing_entry(current, &rel_path, false);
                        tracing::info!("{}: uploaded file {}", self.name, rel_path);
                    }
                    Err(err) => {
                        tracing::info!(
                            "{}: failed to upload file {}: {}",
                            self.name,
                            rel_path,
                            err
                        );
                        if is_remote_mutation_conflict(&err) {
                            tracing::warn!(
                                "{}: preserving unsynchronized local change for {} after object mutation conflict",
                                self.name,
                                rel_path
                            );
                        } else {
                            self.mark_entry_for_retry(current, &rel_path, previous_entry);
                        }
                    }
                }
            } else {
                // File does not exist, create placeholder
                use crate::runtime::create_placeholder;
                if let Err(e) =
                    create_placeholder(&self.sync_root, &rel_path, self.provider_instance_id)
                {
                    tracing::info!(
                        "{}: failed to create placeholder for {}: {}",
                        self.name,
                        rel_path,
                        e
                    );
                    self.mark_entry_for_retry(current, &rel_path, previous_entry);
                } else {
                    tracing::info!("{}: created placeholder for {}", self.name, rel_path);
                }
            }
        }
    }

    fn mark_entry_for_retry(
        &self,
        current: &mut HashMap<String, SeenEntry>,
        rel_path: &str,
        previous_entry: Option<&SeenEntry>,
    ) {
        if let Some(previous_entry) = previous_entry {
            current.insert(rel_path.to_string(), previous_entry.clone());
        } else {
            current.remove(rel_path);
        }
    }

    fn maybe_schedule_placeholder_dehydrate(
        &self,
        path: &std::path::Path,
        rel_path: &str,
        previous_entry: Option<&SeenEntry>,
        entry: &SeenEntry,
    ) {
        if !entry.has_unpinned_attribute() {
            return;
        }

        let Some(placeholder_state) = entry.placeholder_state else {
            if previous_entry != Some(entry) {
                tracing::info!(
                    "{}: dehydrate candidate missing placeholder probe path={} entry={} raw_state={}",
                    self.name,
                    rel_path,
                    entry.to_log_string(),
                    describe_path_state(path)
                );
            }
            return;
        };
        if !placeholder_state.should_dehydrate() {
            if previous_entry != Some(entry) {
                tracing::info!(
                    "{}: dehydrate candidate rejected path={} snapshot={} reason={} raw_state={}",
                    self.name,
                    rel_path,
                    placeholder_state.to_log_string(),
                    placeholder_state.block_reason(),
                    describe_path_state(path)
                );
            }
            return;
        }

        if previous_entry != Some(entry) {
            tracing::info!(
                "{}: dehydrate candidate accepted path={} snapshot={} raw_state={} action=request-provider-dehydrate",
                self.name,
                rel_path,
                placeholder_state.to_log_string(),
                describe_path_state(path),
            );
        }

        {
            let mut in_flight = self
                .dehydrations_in_flight
                .lock()
                .expect("dehydrations_in_flight lock poisoned");
            if !in_flight.insert(rel_path.to_string()) {
                if previous_entry != Some(entry) {
                    tracing::info!(
                        "{}: dehydrate already in flight for {} snapshot={} raw_state={}",
                        self.name,
                        rel_path,
                        placeholder_state.to_log_string(),
                        describe_path_state(path)
                    );
                }
                return;
            }
        }

        let rel_path = rel_path.to_string();
        let full_path = path.to_path_buf();
        let monitor_name = self.name.clone();
        let in_flight = self.dehydrations_in_flight.clone();
        std::thread::spawn(move || {
            tracing::info!(
                "{}: dehydrating unpinned placeholder {} state_before={} snapshot={}",
                monitor_name,
                rel_path,
                describe_path_state(&full_path),
                placeholder_state.to_log_string()
            );

            let mut attempt = 0usize;
            loop {
                match cf_dehydrate_placeholder_with_oplock(&full_path, &rel_path) {
                    Ok(()) => {
                        tracing::info!(
                            "{}: dehydrated placeholder {} state_after={}",
                            monitor_name,
                            rel_path,
                            describe_path_state(&full_path)
                        );
                        break;
                    }
                    Err(err)
                        if error_chain_has_win32_hresult(&err, ERROR_SHARING_VIOLATION)
                            && attempt < DEHYDRATE_SHARING_VIOLATION_MAX_RETRIES =>
                    {
                        attempt += 1;
                        tracing::info!(
                            "{}: dehydrate placeholder {} hit sharing violation; retrying attempt={}/{} after {} ms error={:#} state_now={}",
                            monitor_name,
                            rel_path,
                            attempt,
                            DEHYDRATE_SHARING_VIOLATION_MAX_RETRIES,
                            DEHYDRATE_SHARING_VIOLATION_RETRY_DELAY_MS,
                            err,
                            describe_path_state(&full_path)
                        );
                        std::thread::sleep(Duration::from_millis(
                            DEHYDRATE_SHARING_VIOLATION_RETRY_DELAY_MS,
                        ));
                    }
                    Err(err) => {
                        tracing::info!(
                            "{}: failed to dehydrate placeholder {}: {:#} state_after={}",
                            monitor_name,
                            rel_path,
                            err,
                            describe_path_state(&full_path)
                        );
                        break;
                    }
                }
            }

            in_flight
                .lock()
                .expect("dehydrations_in_flight lock poisoned")
                .remove(&rel_path);
            tracing::info!(
                "{}: dehydrate worker finished for {} current_state={}",
                monitor_name,
                rel_path,
                describe_path_state(&full_path)
            );
        });
    }

    fn maybe_schedule_placeholder_hydrate(
        &self,
        path: &std::path::Path,
        rel_path: &str,
        previous_entry: Option<&SeenEntry>,
        entry: &SeenEntry,
    ) {
        if !entry.has_pinned_attribute() {
            return;
        }

        let Some(placeholder_state) = entry.placeholder_state else {
            if previous_entry != Some(entry) {
                tracing::info!(
                    "{}: hydrate candidate missing placeholder probe path={} entry={} raw_state={}",
                    self.name,
                    rel_path,
                    entry.to_log_string(),
                    describe_path_state(path)
                );
            }
            return;
        };
        if !placeholder_state.should_hydrate() {
            if previous_entry != Some(entry) {
                let reason = if placeholder_state.pin_state != CF_PIN_STATE_PINNED {
                    "pin-state-not-pinned"
                } else if placeholder_state.in_sync_state != CF_IN_SYNC_STATE_IN_SYNC {
                    "not-in-sync"
                } else if placeholder_state.modified_data_size != 0 {
                    "modified-data-present"
                } else if !placeholder_state.is_partial {
                    "already-fully-hydrated"
                } else {
                    "not-eligible"
                };
                tracing::info!(
                    "{}: hydrate candidate rejected path={} snapshot={} reason={} raw_state={}",
                    self.name,
                    rel_path,
                    placeholder_state.to_log_string(),
                    reason,
                    describe_path_state(path)
                );
            }
            return;
        }

        if entry.provider_hydration_active {
            if previous_entry != Some(entry) {
                tracing::info!(
                    "{}: hydrate candidate deferred path={} snapshot={} reason=provider-hydration-active raw_state={}",
                    self.name,
                    rel_path,
                    placeholder_state.to_log_string(),
                    describe_path_state(path)
                );
            }
            return;
        }

        if !should_schedule_placeholder_hydration(previous_entry, entry, placeholder_state) {
            return;
        }

        if previous_entry != Some(entry) {
            tracing::info!(
                "{}: hydrate candidate accepted path={} snapshot={} raw_state={} action=request-provider-hydrate",
                self.name,
                rel_path,
                placeholder_state.to_log_string(),
                describe_path_state(path),
            );
        }

        {
            let mut in_flight = self
                .hydrations_in_flight
                .lock()
                .expect("hydrations_in_flight lock poisoned");
            if !in_flight.insert(rel_path.to_string()) {
                if previous_entry != Some(entry) {
                    tracing::info!(
                        "{}: hydrate already in flight for {} snapshot={} raw_state={}",
                        self.name,
                        rel_path,
                        placeholder_state.to_log_string(),
                        describe_path_state(path)
                    );
                }
                return;
            }
        }

        let rel_path = rel_path.to_string();
        let full_path = path.to_path_buf();
        let sync_root = self.sync_root.clone();
        let monitor_name = self.name.clone();
        let in_flight = self.hydrations_in_flight.clone();
        std::thread::spawn(move || {
            tracing::info!(
                "{}: hydrating pinned placeholder {} state_before={} snapshot={}",
                monitor_name,
                rel_path,
                describe_path_state(&full_path),
                placeholder_state.to_log_string()
            );

            if is_active_hydration_marked(&sync_root, &rel_path) {
                tracing::info!(
                    "{}: deferring pinned placeholder hydrate {} because provider hydration is active state_before={} snapshot={}",
                    monitor_name,
                    rel_path,
                    describe_path_state(&full_path),
                    placeholder_state.to_log_string()
                );
            } else {
                let result = open_sync_path(&full_path, true)
                    .map_err(anyhow::Error::from)
                    .and_then(|file| cf_hydrate_placeholder(&file));

                match result {
                    Ok(()) => {
                        tracing::info!(
                            "{}: hydrated pinned placeholder {} state_after={}",
                            monitor_name,
                            rel_path,
                            describe_path_state(&full_path)
                        );
                    }
                    Err(err) => {
                        tracing::info!(
                            "{}: failed to hydrate pinned placeholder {}: {:#} state_after={}",
                            monitor_name,
                            rel_path,
                            err,
                            describe_path_state(&full_path)
                        );
                    }
                }
            }

            in_flight
                .lock()
                .expect("hydrations_in_flight lock poisoned")
                .remove(&rel_path);
            tracing::info!(
                "{}: hydrate worker finished for {} current_state={}",
                monitor_name,
                rel_path,
                describe_path_state(&full_path)
            );
        });
    }

    fn handle_deleted_entries(
        &self,
        current: &HashMap<String, SeenEntry>,
        handled_renames: &std::collections::HashSet<String>,
    ) {
        let mut deleted_paths = self
            .seen
            .iter()
            .filter_map(|(path, entry)| {
                if current.contains_key(path) {
                    None
                } else {
                    Some((path.as_str(), entry.clone()))
                }
            })
            .collect::<Vec<_>>();
        deleted_paths.sort_by(|(left_path, _), (right_path, _)| right_path.cmp(left_path));

        for (path, entry) in deleted_paths {
            if is_internal_client_identity_relative_path(path)
                || is_internal_connection_bootstrap_relative_path(path)
                || is_internal_remote_snapshot_relative_path(path)
            {
                continue;
            }
            if handled_renames.contains(path) {
                tracing::info!(
                    "{}: skipping delete handling for renamed path {}",
                    self.name,
                    path
                );
                continue;
            }
            if self
                .remote_applied_tracker
                .take_file_removal_suppression(path)
            {
                tracing::info!(
                    "{}: suppressing remote-applied local removal for {}",
                    self.name,
                    path
                );
                continue;
            }
            let (Some(object_id), Some(expected_revision)) = (
                entry.placeholder_object_id.as_deref(),
                entry.placeholder_revision.as_deref(),
            ) else {
                tracing::warn!(
                    "{}: refusing path-only remote delete for {}; placeholder identity is incomplete",
                    self.name,
                    path
                );
                continue;
            };
            let object_kind = if entry.is_dir { "directory" } else { "file" };
            tracing::info!(
                "{}: detected deleted {} {} object_id={} expected_revision={}",
                self.name,
                object_kind,
                path,
                object_id,
                expected_revision
            );
            if let Err(err) = self.uploader.delete_object(object_id, expected_revision) {
                tracing::info!(
                    "{}: failed to delete remote {} {} object_id={}: {}",
                    self.name,
                    object_kind,
                    path,
                    object_id,
                    err
                );
            }
        }
    }
}

fn normalize_monitor_relative_path(path: &str) -> String {
    path.trim_matches(['/', '\\']).replace('\\', "/")
}

fn directory_marker_path(path: &str) -> String {
    let trimmed = normalize_monitor_relative_path(path);
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}/")
    }
}

fn record_remote_applied_directory(path: &str, directories: &mut HashSet<String>) {
    let normalized = normalize_monitor_relative_path(path);
    if normalized.is_empty() {
        return;
    }

    directories.insert(normalized.clone());
    for parent in parent_directories_for_path(&normalized) {
        directories.insert(parent);
    }
}

fn placeholder_identity_for_entry(
    path: &std::path::Path,
    is_placeholder: bool,
) -> Option<PlaceholderFileIdentity> {
    if !is_placeholder {
        return None;
    }

    let file = open_sync_path(path, false).ok()?;
    let info = cf_get_placeholder_standard_info_with_identity(&file).ok()?;
    let file_identity = info.file_identity();
    if file_identity.is_empty() {
        return None;
    }

    decode_placeholder_file_identity(file_identity)
}

fn repair_locally_renamed_object(
    sync_root: &std::path::Path,
    path: &std::path::Path,
    rel_path: &str,
    provider_instance_id: uuid::Uuid,
    is_dir: bool,
    receipt: &ObjectRenameReceipt,
) -> anyhow::Result<()> {
    let is_placeholder = path_is_placeholder(path);
    tracing::info!(
        "monitor: repairing local renamed object {} mode={} entry_snapshot={} state_before={}",
        rel_path,
        if is_placeholder {
            "placeholder-metadata-only"
        } else {
            "materialized-convert-and-fingerprint"
        },
        format!(
            "object_id={} revision={}",
            receipt.object_id, receipt.remote_version
        ),
        describe_path_state(path)
    );

    if is_dir {
        record_uploaded_object_state(
            sync_root,
            rel_path,
            provider_instance_id,
            &receipt.object_id,
            &receipt.remote_version,
            None,
        )?;
        promote_remote_to_in_sync_content_baseline(sync_root, rel_path, provider_instance_id)?;
        let file = open_sync_path(path, true)?;
        cf_set_in_sync(&file)?;
        return Ok(());
    }

    if is_placeholder {
        // Renamed placeholders must be repaired without reading file content, or the
        // fingerprinting path will implicitly hydrate them. Repoint the stored
        // FileIdentity metadata to the new relative path and restore the in-sync
        // content baseline as a metadata-only operation.
        record_uploaded_object_state(
            sync_root,
            rel_path,
            provider_instance_id,
            &receipt.object_id,
            &receipt.remote_version,
            None,
        )?;
        promote_remote_to_in_sync_content_baseline(sync_root, rel_path, provider_instance_id)?;
        let file = open_sync_path(path, true)?;
        cf_set_in_sync(&file)?;
        tracing::info!(
            "monitor: repaired local renamed object {} mode=placeholder-metadata-only state_after={}",
            rel_path,
            describe_path_state(path)
        );
        return Ok(());
    }

    let metadata = std::fs::metadata(path)?;
    try_convert_materialized_file(path, rel_path, &metadata);

    record_uploaded_object_state(
        sync_root,
        rel_path,
        provider_instance_id,
        &receipt.object_id,
        &receipt.remote_version,
        None,
    )?;
    let file = open_sync_path(path, true)?;
    cf_set_in_sync(&file)?;
    record_in_sync_local_file_state(sync_root, rel_path, provider_instance_id)?;
    tracing::info!(
        "monitor: repaired local renamed file {} mode=materialized-convert-and-fingerprint state_after={}",
        rel_path,
        describe_path_state(path)
    );
    Ok(())
}

fn mark_local_rename_conflict(path: &std::path::Path) {
    let result = match open_sync_path(path, true) {
        Ok(file) => cf_set_not_in_sync(&file).map(|_| ()),
        Err(err) => Err(err.into()),
    };
    if let Err(err) = result {
        tracing::info!(
            "monitor: failed to mark conflicted local rename not-in-sync at {}: {:#}",
            path.display(),
            err
        );
    }
}

fn detect_local_object_renames(
    previous: &HashMap<String, SeenEntry>,
    current: &HashMap<String, SeenEntry>,
) -> Vec<LocalRenamePair> {
    let mut pairs = Vec::new();
    let mut matched_sources = std::collections::HashSet::new();
    let mut matched_destinations = std::collections::HashSet::new();

    for (to_path, entry) in current {
        if previous.contains_key(to_path) {
            continue;
        }
        let Some(from_path) = entry.placeholder_identity_path.as_deref() else {
            continue;
        };
        let (Some(object_id), Some(_revision)) = (
            entry.placeholder_object_id.as_deref(),
            entry.placeholder_revision.as_deref(),
        ) else {
            continue;
        };
        if from_path == to_path
            || matched_sources.contains(from_path)
            || matched_destinations.contains(to_path)
            || current.contains_key(from_path)
        {
            continue;
        }
        if previous.get(from_path).is_some_and(|candidate| {
            candidate.is_dir == entry.is_dir
                && candidate.placeholder_object_id.as_deref() == Some(object_id)
        }) {
            matched_sources.insert(from_path.to_string());
            matched_destinations.insert(to_path.clone());
            pairs.push(LocalRenamePair {
                from_path: from_path.to_string(),
                to_path: to_path.clone(),
                detection: "placeholder-identity",
            });
        }
    }

    pairs.sort_by(|left, right| left.from_path.cmp(&right.from_path));
    pairs
}

fn remote_rename_destination(relative_path: &str, is_directory: bool) -> String {
    let normalized = normalize_path(relative_path);
    if is_directory {
        let marker_path = normalized.trim_end_matches('/');
        if marker_path.is_empty() {
            String::new()
        } else {
            format!("{marker_path}/")
        }
    } else {
        normalized
    }
}

fn snapshot_entry(
    sync_root: &std::path::Path,
    rel_path: &str,
    path: &std::path::Path,
    is_dir: bool,
) -> SeenEntry {
    let metadata = std::fs::metadata(path).ok();
    let file_attributes = metadata
        .as_ref()
        .map(|metadata| metadata.file_attributes())
        .unwrap_or_default();
    let is_placeholder = path_is_placeholder(path);
    let placeholder_identity = placeholder_identity_for_entry(path, is_placeholder);
    let should_probe = !is_dir
        && ((file_attributes & FILE_ATTRIBUTE_UNPINNED) != 0
            || (file_attributes & FILE_ATTRIBUTE_PINNED) != 0);
    SeenEntry {
        is_dir,
        file_attributes,
        local_file_identity: (!is_dir)
            .then(|| LocalFileIdentity::from_path(path))
            .flatten(),
        placeholder_identity_path: placeholder_identity
            .as_ref()
            .map(|identity| identity.path.clone()),
        placeholder_object_id: placeholder_identity
            .as_ref()
            .and_then(|identity| identity.object_id.clone()),
        placeholder_revision: placeholder_identity
            .as_ref()
            .and_then(|identity| identity.remote_version.clone()),
        placeholder_state: if !should_probe {
            None
        } else {
            let placeholder_state_bits =
                path_placeholder_state(path).unwrap_or(CF_PLACEHOLDER_STATE_NO_STATES);
            PlaceholderSnapshot::from_path(path, placeholder_state_bits)
        },
        provider_hydration_active: should_probe && is_active_hydration_marked(sync_root, rel_path),
    }
}

fn summarize_dehydrate_scan(entries: &HashMap<String, SeenEntry>) -> DehydrateScanSummary {
    let mut summary = DehydrateScanSummary {
        total_entries: entries.len(),
        ..Default::default()
    };
    for entry in entries.values() {
        if entry.has_pinned_attribute() {
            summary.pinned_attribute_count += 1;
        }
        if entry.has_unpinned_attribute() {
            summary.unpinned_attribute_count += 1;
        }
        if entry.placeholder_state.is_some() {
            summary.probed_placeholder_count += 1;
        }
        if entry
            .placeholder_state
            .is_some_and(PlaceholderSnapshot::should_hydrate)
        {
            summary.hydrate_eligible_count += 1;
        }
        if entry
            .placeholder_state
            .is_some_and(PlaceholderSnapshot::should_dehydrate)
        {
            summary.eligible_count += 1;
        }
    }
    summary
}

fn parent_directories_for_path(path: &str) -> Vec<String> {
    let normalized = path.trim_matches(['/', '\\']).replace('\\', "/");
    let segments = normalized
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if segments.len() < 2 {
        return Vec::new();
    }

    let mut parents = Vec::with_capacity(segments.len().saturating_sub(1));
    for index in 1..segments.len() {
        parents.push(segments[..index].join("/"));
    }
    parents
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::placeholder_metadata::reconcile_existing_placeholders;
    use crate::runtime::{
        SyncRootRegistration, apply_action_plan, register_sync_root, unregister_sync_root,
    };
    use std::io::Read;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use sync_core::{NamespaceEntry, SyncSnapshot};
    use windows_sys::Win32::Storage::CloudFilters::{
        CF_IN_SYNC_STATE_NOT_IN_SYNC, CF_PIN_STATE_PINNED, CF_PIN_STATE_UNSPECIFIED,
    };

    #[derive(Default)]
    struct MockUploader {
        uploads: Mutex<Vec<String>>,
        deletes: Mutex<Vec<String>>,
        delete_revisions: Mutex<Vec<String>>,
    }

    #[derive(Default)]
    struct FailOnceUploader {
        attempts: Mutex<HashMap<String, usize>>,
        uploads: Mutex<Vec<String>>,
    }

    #[derive(Default)]
    struct TransientRenameUploader {
        rename_attempts: Mutex<usize>,
    }

    /// A real Windows CFAPI registration so the monitor observes the same placeholder kind that
    /// remote reconciliation removes in production.
    struct RegisteredMonitorTestSyncRoot {
        root_path: PathBuf,
        _registration_lock: std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for RegisteredMonitorTestSyncRoot {
        fn drop(&mut self) {
            let _ = unregister_sync_root(&self.root_path);
            let _ = std::fs::remove_dir_all(&self.root_path);
        }
    }

    impl Uploader for MockUploader {
        fn upload_reader(
            &self,
            path: &str,
            reader: &mut dyn Read,
            _length: u64,
        ) -> anyhow::Result<UploadReceipt> {
            let mut sink = Vec::new();
            let _ = reader.read_to_end(&mut sink)?;
            self.uploads
                .lock()
                .expect("uploads lock poisoned")
                .push(path.to_string());
            Ok(UploadReceipt {
                object_id: Some(format!("object:{path}")),
                remote_version: Some("revision-uploaded".to_string()),
                in_sync_content_fingerprint: None,
            })
        }

        fn delete_object(&self, object_id: &str, expected_revision: &str) -> anyhow::Result<()> {
            self.deletes
                .lock()
                .expect("deletes lock poisoned")
                .push(object_id.to_string());
            self.delete_revisions
                .lock()
                .expect("delete_revisions lock poisoned")
                .push(expected_revision.to_string());
            Ok(())
        }
    }

    impl Uploader for FailOnceUploader {
        fn upload_reader(
            &self,
            path: &str,
            reader: &mut dyn Read,
            _length: u64,
        ) -> anyhow::Result<UploadReceipt> {
            let mut sink = Vec::new();
            let _ = reader.read_to_end(&mut sink)?;

            let attempt = {
                let mut attempts = self.attempts.lock().expect("attempts lock poisoned");
                let entry = attempts.entry(path.to_string()).or_insert(0);
                *entry += 1;
                *entry
            };

            if attempt == 1 {
                anyhow::bail!("injected upload failure for {path}");
            }

            self.uploads
                .lock()
                .expect("uploads lock poisoned")
                .push(path.to_string());
            Ok(UploadReceipt {
                object_id: Some(format!("object:{path}")),
                remote_version: Some("revision-uploaded".to_string()),
                in_sync_content_fingerprint: None,
            })
        }
    }

    impl Uploader for TransientRenameUploader {
        fn upload_reader(
            &self,
            _path: &str,
            _reader: &mut dyn Read,
            _length: u64,
        ) -> anyhow::Result<UploadReceipt> {
            anyhow::bail!("unexpected upload while testing rename retry")
        }

        fn rename_object(
            &self,
            _object_id: &str,
            _expected_revision: &str,
            _to_path: &str,
        ) -> anyhow::Result<ObjectRenameReceipt> {
            *self
                .rename_attempts
                .lock()
                .expect("rename_attempts lock poisoned") += 1;
            anyhow::bail!("injected transient rename transport failure")
        }
    }

    fn seen_entry(is_dir: bool) -> SeenEntry {
        SeenEntry {
            is_dir,
            file_attributes: 0,
            local_file_identity: None,
            placeholder_identity_path: None,
            placeholder_object_id: None,
            placeholder_revision: None,
            placeholder_state: None,
            provider_hydration_active: false,
        }
    }

    fn registered_monitor_test_sync_root(
        test_name: &str,
    ) -> (RegisteredMonitorTestSyncRoot, uuid::Uuid) {
        let registration_lock = crate::lock_sync_root_registration_tests();
        let unique = uuid::Uuid::new_v4();
        let root_path = std::env::temp_dir().join(format!("ironmesh-monitor-{test_name}-{unique}"));
        let registration = SyncRootRegistration::new(
            format!("test-monitor-{test_name}-{unique}"),
            "Ironmesh Monitor Test",
            &root_path,
            uuid::Uuid::new_v4(),
            None,
        );
        let identity =
            register_sync_root(&registration).expect("test sync root registration should succeed");

        (
            RegisteredMonitorTestSyncRoot {
                root_path,
                _registration_lock: registration_lock,
            },
            identity.provider_instance_id,
        )
    }

    fn create_clean_provider_placeholder(
        sync_root: &std::path::Path,
        provider_instance_id: uuid::Uuid,
        path: &str,
    ) {
        apply_action_plan(
            sync_root,
            &CfapiActionPlan {
                actions: vec![CfapiAction::EnsurePlaceholder {
                    object_id: Some("test-object".to_string()),
                    path: path.to_string(),
                    remote_version: "newer-revision".to_string(),
                    remote_content_hash: "newer-content-hash".to_string(),
                    remote_size: Some(1_024),
                    remote_content_fingerprint: Some("newer-fingerprint".to_string()),
                    remote_modified_at_unix: Some(1_725_000_001),
                    remote_media: None,
                }],
            },
            provider_instance_id,
            true,
        )
        .expect("clean provider placeholder should be created");
    }

    fn stale_snapshot_without_removed_path() -> SyncSnapshot {
        SyncSnapshot {
            local: Vec::new(),
            remote: vec![NamespaceEntry::file(
                "small-unrelated-change.txt",
                "unrelated-revision",
                "unrelated-hash",
            )],
        }
    }

    #[test]
    fn seed_seen_makes_startup_walk_passive_for_existing_entries() {
        let unique = uuid::Uuid::new_v4();
        let sync_root = std::env::temp_dir().join(format!("ironmesh-monitor-seed-seen-{unique}"));
        std::fs::create_dir_all(sync_root.join("docs")).expect("failed to create sync root");
        std::fs::write(sync_root.join("docs").join("readme.txt"), b"hello")
            .expect("failed to seed existing file");

        let uploader = Arc::new(MockUploader::default());
        let mut monitor = SyncRootMonitor::new(
            "monitor-test",
            sync_root.clone(),
            uuid::Uuid::nil(),
            uploader.clone(),
        );
        monitor.seed_seen();
        monitor.walk();

        assert!(
            uploader
                .uploads
                .lock()
                .expect("uploads lock poisoned")
                .is_empty(),
            "startup walk should not upload pre-existing entries after seed_seen"
        );
        assert!(
            uploader
                .deletes
                .lock()
                .expect("deletes lock poisoned")
                .is_empty(),
            "startup walk should not emit deletes after seed_seen"
        );

        let _ = std::fs::remove_dir_all(sync_root);
    }

    #[test]
    fn seed_remote_entries_keeps_local_only_files_pending_for_upload() {
        let unique = uuid::Uuid::new_v4();
        let sync_root = std::env::temp_dir().join(format!("ironmesh-monitor-remote-seed-{unique}"));
        std::fs::create_dir_all(sync_root.join("docs")).expect("failed to create sync root");
        std::fs::write(
            sync_root.join("docs").join("readme.txt"),
            b"remote baseline",
        )
        .expect("failed to seed remote placeholder stand-in");
        std::fs::write(sync_root.join("local-only.txt"), b"local upload")
            .expect("failed to seed local-only file");

        let uploader = Arc::new(MockUploader::default());
        let mut monitor = SyncRootMonitor::new(
            "monitor-test",
            sync_root.clone(),
            uuid::Uuid::nil(),
            uploader.clone(),
        );
        monitor.seed_remote_entries(&CfapiActionPlan {
            actions: vec![CfapiAction::EnsurePlaceholder {
                object_id: Some("obj-readme".to_string()),
                path: "docs/readme.txt".to_string(),
                remote_version: "v1".to_string(),
                remote_content_hash: "h1".to_string(),
                remote_size: None,
                remote_content_fingerprint: None,
                remote_modified_at_unix: None,
                remote_media: None,
            }],
        });
        monitor.walk();

        let uploads = uploader
            .uploads
            .lock()
            .expect("uploads lock poisoned")
            .clone();
        assert!(
            uploads.iter().any(|path| path == "local-only.txt"),
            "startup walk should still upload pre-existing local-only files"
        );
        assert!(
            uploads
                .iter()
                .all(|path| path != "docs/" && path != "docs/readme.txt"),
            "startup walk should not re-upload remote-seeded entries"
        );

        let _ = std::fs::remove_dir_all(sync_root);
    }

    #[test]
    fn failed_upload_is_retried_on_next_walk_for_local_file() {
        let unique = uuid::Uuid::new_v4();
        let sync_root =
            std::env::temp_dir().join(format!("ironmesh-monitor-retry-local-file-{unique}"));
        std::fs::create_dir_all(&sync_root).expect("failed to create sync root");

        let uploader = Arc::new(FailOnceUploader::default());
        let mut monitor = SyncRootMonitor::new(
            "monitor-test",
            sync_root.clone(),
            uuid::Uuid::nil(),
            uploader.clone(),
        );
        monitor.seed_seen();

        std::fs::write(sync_root.join("retry.txt"), b"retry me")
            .expect("failed to create local retry file");

        monitor.walk();
        monitor.walk();

        let attempts = uploader
            .attempts
            .lock()
            .expect("attempts lock poisoned")
            .get("retry.txt")
            .copied()
            .unwrap_or(0);
        assert_eq!(
            attempts, 2,
            "local file should be retried after an upload failure"
        );
        assert_eq!(
            uploader
                .uploads
                .lock()
                .expect("uploads lock poisoned")
                .as_slice(),
            ["retry.txt"],
            "second walk should complete the upload"
        );

        let _ = std::fs::remove_dir_all(sync_root);
    }

    #[test]
    fn remote_applied_directory_is_suppressed_but_later_local_directory_uploads() {
        let unique = uuid::Uuid::new_v4();
        let sync_root =
            std::env::temp_dir().join(format!("ironmesh-monitor-remote-apply-{unique}"));
        std::fs::create_dir_all(&sync_root).expect("failed to create sync root");

        let uploader = Arc::new(MockUploader::default());
        let mut monitor = SyncRootMonitor::new(
            "monitor-test",
            sync_root.clone(),
            uuid::Uuid::nil(),
            uploader.clone(),
        );
        monitor.seed_seen();
        let remote_applied = monitor.remote_applied_tracker();

        std::fs::create_dir_all(sync_root.join("docs")).expect("failed to create remote directory");
        remote_applied.record_plan(&CfapiActionPlan {
            actions: vec![CfapiAction::EnsureDirectory {
                object_id: None,
                path: "docs".to_string(),
                remote_version: None,
            }],
        });
        monitor.walk();

        assert!(
            uploader
                .uploads
                .lock()
                .expect("uploads lock poisoned")
                .is_empty(),
            "remote-applied directory should not be echoed back as local upload"
        );

        std::fs::create_dir_all(sync_root.join("local")).expect("failed to create local directory");
        monitor.walk();

        let uploads = uploader
            .uploads
            .lock()
            .expect("uploads lock poisoned")
            .clone();
        assert!(
            uploads.iter().any(|path| path == "local/"),
            "later local directory should still upload normally, uploads={uploads:?}"
        );
        assert!(
            uploads.iter().all(|path| path != "docs/"),
            "remote-applied directory should remain suppressed, uploads={uploads:?}"
        );

        let _ = std::fs::remove_dir_all(sync_root);
    }

    #[test]
    fn successful_directory_upload_refreshes_the_seen_revision() {
        let (sync_root, provider_instance_id) =
            registered_monitor_test_sync_root("directory-upload-seen-revision");
        let uploader = Arc::new(MockUploader::default());
        let mut monitor = SyncRootMonitor::new(
            "monitor-test",
            sync_root.root_path.clone(),
            provider_instance_id,
            uploader.clone(),
        );
        monitor.seed_seen();
        std::fs::create_dir_all(sync_root.root_path.join("docs"))
            .expect("local directory should be created");

        monitor.walk();
        monitor.walk();

        let uploads = uploader
            .uploads
            .lock()
            .expect("uploads lock poisoned")
            .clone();
        assert_eq!(
            uploads,
            vec!["docs/"],
            "the receipt revision must become the next monitor baseline instead of re-uploading the unchanged directory"
        );
    }

    #[test]
    fn successful_local_placeholder_rename_records_the_post_rename_revision() {
        let (sync_root, provider_instance_id) =
            registered_monitor_test_sync_root("rename-post-mutation-revision");
        let old_path = "docs/before.txt";
        let new_path = "archive/after.txt";
        create_clean_provider_placeholder(&sync_root.root_path, provider_instance_id, old_path);

        let old_full_path = sync_root.root_path.join("docs\\before.txt");
        let new_full_path = sync_root.root_path.join("archive\\after.txt");
        std::fs::create_dir_all(
            new_full_path
                .parent()
                .expect("renamed placeholder should have a parent"),
        )
        .expect("rename target parent should be created");
        std::fs::rename(&old_full_path, &new_full_path)
            .expect("placeholder should be renamed locally");

        repair_locally_renamed_object(
            &sync_root.root_path,
            &new_full_path,
            new_path,
            provider_instance_id,
            false,
            &ObjectRenameReceipt {
                object_id: "test-object".to_string(),
                remote_version: "revision-after-rename".to_string(),
            },
        )
        .expect("successful rename should update placeholder metadata");

        let file = open_sync_path(&new_full_path, false)
            .expect("renamed placeholder should remain accessible");
        let info = cf_get_placeholder_standard_info_with_identity(&file)
            .expect("renamed placeholder identity should be readable");
        let identity = decode_placeholder_file_identity(info.file_identity())
            .expect("renamed placeholder identity should decode");
        assert_eq!(identity.object_id.as_deref(), Some("test-object"));
        assert_eq!(
            identity.remote_version.as_deref(),
            Some("revision-after-rename")
        );
        assert_eq!(identity.path, new_path);
    }

    #[test]
    fn incomplete_snapshot_preserves_missing_paths_and_skips_delete_emission() {
        let unique = uuid::Uuid::new_v4();
        let sync_root =
            std::env::temp_dir().join(format!("ironmesh-monitor-partial-scan-{unique}"));
        std::fs::create_dir_all(&sync_root).expect("failed to create sync root");

        let uploader = Arc::new(MockUploader::default());
        let mut monitor = SyncRootMonitor::new(
            "monitor-test",
            sync_root.clone(),
            uuid::Uuid::nil(),
            uploader.clone(),
        );
        monitor
            .seen
            .insert(String::from("docs/keep.txt"), seen_entry(false));
        monitor
            .seen
            .insert(String::from("docs/old-name.txt"), seen_entry(false));

        let mut current = HashMap::from([(String::from("docs/new-name.txt"), seen_entry(false))]);
        let handled_renames = HashSet::from([
            String::from("docs/old-name.txt"),
            String::from("docs/new-name.txt"),
        ]);

        let preserved_count = monitor
            .preserve_missing_entries_after_incomplete_snapshot(&mut current, &handled_renames);

        assert_eq!(preserved_count, 1);
        assert!(current.contains_key("docs/keep.txt"));
        assert!(!current.contains_key("docs/old-name.txt"));

        monitor.handle_deleted_entries(&current, &handled_renames);

        assert!(
            uploader
                .deletes
                .lock()
                .expect("deletes lock poisoned")
                .is_empty(),
            "incomplete snapshots should not emit deletes for omitted entries"
        );

        let _ = std::fs::remove_dir_all(sync_root);
    }

    #[test]
    fn monitor_does_not_turn_snapshot_absence_into_remote_delete() {
        let (sync_root, provider_instance_id) =
            registered_monitor_test_sync_root("reconciler-removal-delete-echo");
        let path = "photos/after-upload.jpg";
        create_clean_provider_placeholder(&sync_root.root_path, provider_instance_id, path);

        let uploader = Arc::new(MockUploader::default());
        let mut monitor = SyncRootMonitor::new(
            "monitor-test",
            sync_root.root_path.clone(),
            provider_instance_id,
            uploader.clone(),
        );
        monitor.seed_seen();
        assert!(
            monitor
                .seen
                .get(path)
                .and_then(|entry| entry.placeholder_identity_path.as_deref())
                .is_some_and(|identity_path| identity_path == path),
            "the monitor has observed the real provider placeholder before reconciliation"
        );

        let report = reconcile_existing_placeholders(
            &sync_root.root_path,
            &stale_snapshot_without_removed_path(),
            provider_instance_id,
        )
        .expect("current reconciliation should complete");
        assert!(report.deleted_paths.is_empty());
        assert!(report.preserved_paths.contains(path));
        monitor.walk();

        assert!(
            uploader
                .deletes
                .lock()
                .expect("deletes lock poisoned")
                .is_empty()
        );
    }

    #[test]
    fn monitor_user_file_delete_is_propagated_exactly_once() {
        // Control behavior: later origin tracking must preserve propagation of genuine user
        // deletions, while suppressing only adapter-applied removals.
        let (sync_root, provider_instance_id) =
            registered_monitor_test_sync_root("user-delete-count");
        let path = "documents/user-deleted.txt";
        create_clean_provider_placeholder(&sync_root.root_path, provider_instance_id, path);
        let removed_path = sync_root.root_path.join("documents\\user-deleted.txt");

        let uploader = Arc::new(MockUploader::default());
        let mut monitor = SyncRootMonitor::new(
            "monitor-test",
            sync_root.root_path.clone(),
            provider_instance_id,
            uploader.clone(),
        );
        monitor.seed_seen();
        std::fs::remove_file(&removed_path).expect("failed to simulate user deletion");
        monitor.walk();
        monitor.walk();

        assert_eq!(
            uploader
                .deletes
                .lock()
                .expect("deletes lock poisoned")
                .as_slice(),
            ["test-object"],
            "a user file deletion is emitted exactly once after the monitor advances its seen set"
        );
    }

    #[test]
    fn monitor_directory_marker_delete_uses_object_identity_and_revision() {
        let uploader = Arc::new(MockUploader::default());
        let mut monitor = SyncRootMonitor::new(
            "monitor-test",
            std::env::temp_dir().join(format!(
                "ironmesh-monitor-directory-delete-{}",
                uuid::Uuid::new_v4()
            )),
            uuid::Uuid::nil(),
            uploader.clone(),
        );
        let mut directory = seen_entry(true);
        directory.placeholder_object_id = Some("directory-marker-object".to_string());
        directory.placeholder_revision = Some("directory-marker-revision".to_string());
        monitor.seen.insert("documents".to_string(), directory);

        monitor.handle_deleted_entries(&HashMap::new(), &HashSet::new());

        assert_eq!(
            uploader
                .deletes
                .lock()
                .expect("deletes lock poisoned")
                .as_slice(),
            ["directory-marker-object"],
            "an explicit directory marker is deleted by its stable object identity"
        );
        assert_eq!(
            uploader
                .delete_revisions
                .lock()
                .expect("delete_revisions lock poisoned")
                .as_slice(),
            ["directory-marker-revision"],
            "the deletion retains the marker's expected revision as its CAS precondition"
        );
    }

    #[test]
    fn local_rename_detection_requires_same_placeholder_object_id() {
        let mut previous_entry = seen_entry(false);
        previous_entry.placeholder_identity_path = Some("docs/old.txt".to_string());
        previous_entry.placeholder_object_id = Some("obj-document".to_string());
        previous_entry.placeholder_revision = Some("revision-3".to_string());
        let current_entry = previous_entry.clone();

        let pairs = detect_local_object_renames(
            &HashMap::from([("docs/old.txt".to_string(), previous_entry)]),
            &HashMap::from([("archive/new.txt".to_string(), current_entry)]),
        );

        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].from_path, "docs/old.txt");
        assert_eq!(pairs[0].to_path, "archive/new.txt");
        assert_eq!(pairs[0].detection, "placeholder-identity");
    }

    #[test]
    fn local_directory_rename_detection_requires_same_placeholder_object_id() {
        let mut previous_entry = seen_entry(true);
        previous_entry.placeholder_identity_path = Some("docs".to_string());
        previous_entry.placeholder_object_id = Some("obj-directory".to_string());
        previous_entry.placeholder_revision = Some("revision-3".to_string());
        let current_entry = previous_entry.clone();

        let pairs = detect_local_object_renames(
            &HashMap::from([("docs".to_string(), previous_entry)]),
            &HashMap::from([("archive".to_string(), current_entry)]),
        );

        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].from_path, "docs");
        assert_eq!(pairs[0].to_path, "archive");
    }

    #[test]
    fn transient_object_id_rename_failure_is_retried_after_seen_advances() {
        let unique = uuid::Uuid::new_v4();
        let sync_root =
            std::env::temp_dir().join(format!("ironmesh-monitor-rename-retry-{unique}"));
        let uploader = Arc::new(TransientRenameUploader::default());
        let mut monitor = SyncRootMonitor::new(
            "monitor-test",
            sync_root,
            uuid::Uuid::nil(),
            uploader.clone(),
        );

        let mut old_entry = seen_entry(false);
        old_entry.placeholder_identity_path = Some("docs/old.txt".to_string());
        old_entry.placeholder_object_id = Some("obj-document".to_string());
        old_entry.placeholder_revision = Some("revision-3".to_string());
        let current_entry = old_entry.clone();
        monitor.seen.insert("docs/old.txt".to_string(), old_entry);
        let mut current = HashMap::from([("archive/new.txt".to_string(), current_entry)]);

        let first_handled = monitor.handle_local_object_renames(&mut current);
        assert!(first_handled.contains("docs/old.txt"));
        assert!(first_handled.contains("archive/new.txt"));
        assert_eq!(monitor.pending_object_renames.len(), 1);

        // This models the end of a scan: only the destination remains in the
        // baseline, so a retry cannot rely on the removed source path.
        monitor.seen = current.clone();
        let second_handled = monitor.handle_local_object_renames(&mut current);

        assert!(second_handled.contains("docs/old.txt"));
        assert!(second_handled.contains("archive/new.txt"));
        assert_eq!(monitor.pending_object_renames.len(), 1);
        assert_eq!(
            *uploader
                .rename_attempts
                .lock()
                .expect("rename_attempts lock poisoned"),
            2,
            "the pending object-id rename must be retried rather than falling back to path operations"
        );
    }

    #[test]
    fn remote_directory_rename_destination_retains_marker_suffix() {
        assert_eq!(remote_rename_destination("archive", true), "archive/");
        assert_eq!(remote_rename_destination("archive/", true), "archive/");
        assert_eq!(
            remote_rename_destination("archive/readme.txt", false),
            "archive/readme.txt"
        );
    }

    #[test]
    fn local_rename_detection_rejects_path_history_with_different_object_id() {
        let mut previous_entry = seen_entry(false);
        previous_entry.placeholder_identity_path = Some("docs/old.txt".to_string());
        previous_entry.placeholder_object_id = Some("obj-old".to_string());
        previous_entry.placeholder_revision = Some("revision-3".to_string());
        let mut current_entry = previous_entry.clone();
        current_entry.placeholder_object_id = Some("obj-new".to_string());

        let pairs = detect_local_object_renames(
            &HashMap::from([("docs/old.txt".to_string(), previous_entry)]),
            &HashMap::from([("archive/new.txt".to_string(), current_entry)]),
        );

        assert!(pairs.is_empty());
    }

    #[test]
    fn desired_behavior_monitor_does_not_echo_provider_applied_file_removal_as_server_delete() {
        // Desired behavior: a reconciler-originated local removal should be observed by the
        // monitor but attributed to the adapter, rather than becoming a new server deletion.
        // This fails today because RemoteAppliedTracker tracks directories only.
        let (sync_root, provider_instance_id) =
            registered_monitor_test_sync_root("reconciler-removal-no-delete-echo");
        let path = "photos/remote-tombstone.jpg";
        create_clean_provider_placeholder(&sync_root.root_path, provider_instance_id, path);

        let uploader = Arc::new(MockUploader::default());
        let mut monitor = SyncRootMonitor::new(
            "monitor-test",
            sync_root.root_path.clone(),
            provider_instance_id,
            uploader.clone(),
        );
        monitor.seed_seen();
        std::fs::remove_file(sync_root.root_path.join("photos\\remote-tombstone.jpg"))
            .expect("remote-applied removal should succeed");
        let mut report = RemoteObjectReconcileReport::default();
        report.deleted_paths.insert(path.to_string());
        monitor
            .remote_applied_tracker()
            .record_reconcile_report(&report);
        monitor.walk();

        assert!(
            uploader
                .deletes
                .lock()
                .expect("deletes lock poisoned")
                .is_empty(),
            "a confirmed remote-applied removal must not be echoed back to the server"
        );
    }

    #[test]
    fn reconcile_rename_suppresses_both_source_removal_and_destination_upload() {
        let tracker = RemoteAppliedTracker::default();
        let mut report = RemoteObjectReconcileReport::default();
        report.renamed_paths.insert(
            "docs/source.txt".to_string(),
            "archive/destination.txt".to_string(),
        );

        tracker.record_reconcile_report(&report);

        assert!(tracker.take_file_removal_suppression("docs/source.txt"));
        assert!(tracker.take_file_creation_suppression("archive/destination.txt"));
        assert!(
            !tracker.take_file_creation_suppression("archive/destination.txt"),
            "the destination suppression must be consumed after the monitor observes it"
        );
    }

    #[test]
    fn placeholder_snapshot_only_dehydrates_clean_unpinned_hydrated_file() {
        let eligible = PlaceholderSnapshot {
            on_disk_data_size: 1024,
            modified_data_size: 0,
            in_sync_state: CF_IN_SYNC_STATE_IN_SYNC,
            pin_state: CF_PIN_STATE_UNPINNED,
            is_partial: false,
        };
        assert!(eligible.should_dehydrate());

        let pinned = PlaceholderSnapshot {
            pin_state: CF_PIN_STATE_PINNED,
            ..eligible
        };
        assert!(!pinned.should_dehydrate());

        let unspecified = PlaceholderSnapshot {
            pin_state: CF_PIN_STATE_UNSPECIFIED,
            ..eligible
        };
        assert!(!unspecified.should_dehydrate());

        let dirty = PlaceholderSnapshot {
            modified_data_size: 1,
            ..eligible
        };
        assert!(!dirty.should_dehydrate());

        let not_in_sync = PlaceholderSnapshot {
            in_sync_state: CF_IN_SYNC_STATE_NOT_IN_SYNC,
            ..eligible
        };
        assert!(!not_in_sync.should_dehydrate());

        let already_dehydrated = PlaceholderSnapshot {
            on_disk_data_size: 0,
            ..eligible
        };
        assert!(!already_dehydrated.should_dehydrate());

        let partial_pinned = PlaceholderSnapshot {
            on_disk_data_size: 1024,
            modified_data_size: 0,
            in_sync_state: CF_IN_SYNC_STATE_IN_SYNC,
            pin_state: CF_PIN_STATE_PINNED,
            is_partial: true,
        };
        assert!(partial_pinned.should_hydrate());

        let fully_hydrated_pinned = PlaceholderSnapshot {
            is_partial: false,
            ..partial_pinned
        };
        assert!(!fully_hydrated_pinned.should_hydrate());
    }

    #[test]
    fn startup_seed_does_not_schedule_hydration_for_unchanged_pinned_placeholder() {
        let pinned_partial = PlaceholderSnapshot {
            on_disk_data_size: 0,
            modified_data_size: 0,
            in_sync_state: CF_IN_SYNC_STATE_IN_SYNC,
            pin_state: CF_PIN_STATE_PINNED,
            is_partial: true,
        };
        let entry = SeenEntry {
            is_dir: false,
            file_attributes: FILE_ATTRIBUTE_PINNED,
            local_file_identity: None,
            placeholder_identity_path: Some("movies/example.mp4".to_string()),
            placeholder_object_id: Some("obj-example".to_string()),
            placeholder_revision: Some("revision-example".to_string()),
            placeholder_state: Some(pinned_partial),
            provider_hydration_active: false,
        };

        assert!(
            !should_schedule_placeholder_hydration(Some(&entry), &entry, pinned_partial),
            "seeded startup snapshot should not auto-hydrate an unchanged pinned placeholder",
        );
    }

    #[test]
    fn monitor_only_schedules_hydration_when_entry_newly_becomes_eligible() {
        let pinned_partial = PlaceholderSnapshot {
            on_disk_data_size: 0,
            modified_data_size: 0,
            in_sync_state: CF_IN_SYNC_STATE_IN_SYNC,
            pin_state: CF_PIN_STATE_PINNED,
            is_partial: true,
        };
        let previous = SeenEntry {
            is_dir: false,
            file_attributes: 0,
            local_file_identity: None,
            placeholder_identity_path: Some("movies/example.mp4".to_string()),
            placeholder_object_id: Some("obj-example".to_string()),
            placeholder_revision: Some("revision-example".to_string()),
            placeholder_state: None,
            provider_hydration_active: false,
        };
        let current = SeenEntry {
            is_dir: false,
            file_attributes: FILE_ATTRIBUTE_PINNED,
            local_file_identity: None,
            placeholder_identity_path: Some("movies/example.mp4".to_string()),
            placeholder_object_id: Some("obj-example".to_string()),
            placeholder_revision: Some("revision-example".to_string()),
            placeholder_state: Some(pinned_partial),
            provider_hydration_active: false,
        };

        assert!(
            should_schedule_placeholder_hydration(Some(&previous), &current, pinned_partial),
            "a placeholder that newly becomes pinned and partially hydrated should be eligible",
        );
        assert!(
            !should_schedule_placeholder_hydration(Some(&current), &current, pinned_partial),
            "already-eligible placeholders should not reschedule hydration on every walk",
        );
    }

    #[test]
    fn monitor_defers_hydration_while_provider_hydration_is_active() {
        let pinned_partial = PlaceholderSnapshot {
            on_disk_data_size: 0,
            modified_data_size: 0,
            in_sync_state: CF_IN_SYNC_STATE_IN_SYNC,
            pin_state: CF_PIN_STATE_PINNED,
            is_partial: true,
        };
        let entry = SeenEntry {
            is_dir: false,
            file_attributes: FILE_ATTRIBUTE_PINNED,
            local_file_identity: None,
            placeholder_identity_path: Some("movies/example.mp4".to_string()),
            placeholder_object_id: Some("obj-example".to_string()),
            placeholder_revision: Some("revision-example".to_string()),
            placeholder_state: Some(pinned_partial),
            provider_hydration_active: true,
        };

        assert!(
            !should_schedule_placeholder_hydration(None, &entry, pinned_partial),
            "provider-owned hydration should suppress overlapping explicit hydrates",
        );
    }

    #[test]
    fn monitor_retries_hydration_after_provider_hydration_clears() {
        let pinned_partial = PlaceholderSnapshot {
            on_disk_data_size: 0,
            modified_data_size: 0,
            in_sync_state: CF_IN_SYNC_STATE_IN_SYNC,
            pin_state: CF_PIN_STATE_PINNED,
            is_partial: true,
        };
        let previous = SeenEntry {
            is_dir: false,
            file_attributes: FILE_ATTRIBUTE_PINNED,
            local_file_identity: None,
            placeholder_identity_path: Some("movies/example.mp4".to_string()),
            placeholder_object_id: Some("obj-example".to_string()),
            placeholder_revision: Some("revision-example".to_string()),
            placeholder_state: Some(pinned_partial),
            provider_hydration_active: true,
        };
        let current = SeenEntry {
            provider_hydration_active: false,
            ..previous.clone()
        };

        assert!(
            should_schedule_placeholder_hydration(Some(&previous), &current, pinned_partial),
            "a stuck partial placeholder should retry once provider-owned hydration clears",
        );
    }

    #[test]
    fn monitor_retries_hydration_after_partial_progress_changes() {
        let previous_partial = PlaceholderSnapshot {
            on_disk_data_size: 0,
            modified_data_size: 0,
            in_sync_state: CF_IN_SYNC_STATE_IN_SYNC,
            pin_state: CF_PIN_STATE_PINNED,
            is_partial: true,
        };
        let current_partial = PlaceholderSnapshot {
            on_disk_data_size: 3_883_008,
            ..previous_partial
        };
        let previous = SeenEntry {
            is_dir: false,
            file_attributes: FILE_ATTRIBUTE_PINNED,
            local_file_identity: None,
            placeholder_identity_path: Some("movies/example.mp4".to_string()),
            placeholder_object_id: Some("obj-example".to_string()),
            placeholder_revision: Some("revision-example".to_string()),
            placeholder_state: Some(previous_partial),
            provider_hydration_active: false,
        };
        let current = SeenEntry {
            placeholder_state: Some(current_partial),
            ..previous.clone()
        };

        assert!(
            should_schedule_placeholder_hydration(Some(&previous), &current, current_partial),
            "partial hydration progress should allow one more explicit hydrate pass",
        );
    }

    #[test]
    fn monitor_skips_upload_for_existing_clean_placeholder_state_changes() {
        let previous = SeenEntry {
            is_dir: false,
            file_attributes: FILE_ATTRIBUTE_UNPINNED,
            local_file_identity: Some(LocalFileIdentity {
                volume_serial_number: 7,
                file_index: 11,
            }),
            placeholder_identity_path: Some("movies/example.mp4".to_string()),
            placeholder_object_id: Some("obj-example".to_string()),
            placeholder_revision: Some("revision-example".to_string()),
            placeholder_state: Some(PlaceholderSnapshot {
                on_disk_data_size: 4096,
                modified_data_size: 0,
                in_sync_state: CF_IN_SYNC_STATE_IN_SYNC,
                pin_state: CF_PIN_STATE_UNPINNED,
                is_partial: false,
            }),
            provider_hydration_active: false,
        };
        let current = SeenEntry {
            placeholder_state: Some(PlaceholderSnapshot {
                on_disk_data_size: 0,
                ..previous.placeholder_state.expect("placeholder snapshot")
            }),
            ..previous.clone()
        };

        assert!(
            should_skip_clean_placeholder_content_upload(Some(&previous), &current),
            "clean placeholder state transitions should not trigger a content upload",
        );
    }

    #[test]
    fn monitor_keeps_dirty_placeholder_uploads_enabled() {
        let previous = SeenEntry {
            is_dir: false,
            file_attributes: FILE_ATTRIBUTE_UNPINNED,
            local_file_identity: Some(LocalFileIdentity {
                volume_serial_number: 7,
                file_index: 11,
            }),
            placeholder_identity_path: Some("movies/example.mp4".to_string()),
            placeholder_object_id: Some("obj-example".to_string()),
            placeholder_revision: Some("revision-example".to_string()),
            placeholder_state: Some(PlaceholderSnapshot {
                on_disk_data_size: 4096,
                modified_data_size: 0,
                in_sync_state: CF_IN_SYNC_STATE_IN_SYNC,
                pin_state: CF_PIN_STATE_UNPINNED,
                is_partial: false,
            }),
            provider_hydration_active: false,
        };
        let current = SeenEntry {
            placeholder_state: Some(PlaceholderSnapshot {
                modified_data_size: 512,
                ..previous.placeholder_state.expect("placeholder snapshot")
            }),
            ..previous.clone()
        };

        assert!(
            !should_skip_clean_placeholder_content_upload(Some(&previous), &current),
            "dirty placeholders must still be eligible for upload",
        );
    }

    #[test]
    fn monitor_skips_upload_after_materialized_file_converts_to_clean_placeholder() {
        let file_identity = Some(LocalFileIdentity {
            volume_serial_number: 7,
            file_index: 11,
        });
        let previous = SeenEntry {
            is_dir: false,
            file_attributes: 0,
            local_file_identity: file_identity,
            placeholder_identity_path: None,
            placeholder_object_id: None,
            placeholder_revision: None,
            placeholder_state: None,
            provider_hydration_active: false,
        };
        let current = SeenEntry {
            is_dir: false,
            file_attributes: FILE_ATTRIBUTE_UNPINNED,
            local_file_identity: file_identity,
            placeholder_identity_path: Some("movies/example.mp4".to_string()),
            placeholder_object_id: Some("obj-example".to_string()),
            placeholder_revision: Some("revision-example".to_string()),
            placeholder_state: Some(PlaceholderSnapshot {
                on_disk_data_size: 0,
                modified_data_size: 0,
                in_sync_state: CF_IN_SYNC_STATE_IN_SYNC,
                pin_state: CF_PIN_STATE_UNPINNED,
                is_partial: false,
            }),
            provider_hydration_active: false,
        };

        assert!(
            should_skip_clean_placeholder_content_upload(Some(&previous), &current),
            "post-upload placeholder conversion should not cause a second upload",
        );
    }
}
