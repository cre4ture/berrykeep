use super::*;
use crate::sync_work::{ContentAvailability, PlannedWork, WorkResult, WorkStore, now_ms};
use serde::{Deserialize, Serialize};

const SCOPE: &str = "folder-materialization";
const BATCH_LIMIT: usize = 64;

#[derive(Clone, Serialize, Deserialize)]
struct FolderWork {
    path: String,
    entry: Option<sync_core::NamespaceEntry>,
    startup: bool,
    availability: ContentAvailability,
    #[serde(default)]
    initial_local: Option<LocalEntryState>,
    #[serde(default)]
    precondition_recorded: bool,
    #[serde(default)]
    preserve: bool,
    #[serde(default)]
    remote_known: bool,
    #[serde(default)]
    initial_fingerprint: Option<String>,
    #[serde(default)]
    subtree: BTreeMap<String, (LocalEntryState, Option<String>)>,
}

fn journal(state: Option<&StartupStateStore>) -> Result<WorkStore> {
    match state {
        Some(state) => WorkStore::open(state.path.with_file_name("sync-work.sqlite")),
        // Callers without a persistent baseline are ephemeral/test backends.
        None => WorkStore::open(":memory:"),
    }
}

pub(super) fn pending(state: &StartupStateStore) -> Result<bool> {
    let status = journal(Some(state))?.status(SCOPE)?;
    Ok(status.pending != 0 || status.running != 0)
}

pub(super) fn folder_materialization_plan(
    snapshot: &SyncSnapshot,
    scope: &PathScope,
    changed: Option<&[String]>,
    previous_index: &RemoteTreeIndex,
    pending: &[PlannedWork],
) -> Result<Vec<PlannedWork>> {
    let entries = snapshot
        .remote
        .iter()
        .filter_map(|entry| {
            scope
                .remote_to_local(&entry.path)
                .filter(|path| !path.is_empty())
                .map(|path| (path, entry.clone()))
        })
        .collect::<BTreeMap<_, _>>();
    let mut selected = BTreeMap::new();
    for work in pending {
        let work: FolderWork = serde_json::from_value(work.payload.clone())?;
        if entries.contains_key(&work.path) || work.entry.is_none() {
            selected.insert(work.path, work.startup);
        }
    }
    match changed {
        None => selected.extend(entries.keys().cloned().map(|path| (path, true))),
        Some(paths) => {
            for path in paths {
                if let Some(path) = scope.remote_to_local(path).filter(|path| !path.is_empty()) {
                    selected.insert(path, false);
                }
            }
        }
    }
    // Pending creations removed by a later snapshot must not be replayed.
    // Remove any already materialized part through the normal guarded delete path.
    for path in previous_index
        .files
        .iter()
        .chain(previous_index.directories.iter())
    {
        if !entries.contains_key(path) {
            selected.insert(path.clone(), false);
        }
    }
    let mut work_by_path = BTreeMap::new();
    for (path, startup) in selected {
        let entry = entries.get(&path).cloned().or_else(|| {
            entries
                .keys()
                .any(|candidate| candidate.starts_with(&format!("{path}/")))
                .then(|| {
                    sync_core::NamespaceEntry::directory(
                        scope.local_to_remote(&path).unwrap_or_else(|| path.clone()),
                    )
                })
        });
        let prefix = match entry.as_ref().map(|entry| entry.kind) {
            Some(EntryKind::Directory) => "0",
            Some(EntryKind::File) => "1",
            None => "2",
        };
        let payload = FolderWork {
            path: path.clone(),
            entry,
            startup,
            availability: ContentAvailability::Complete,
            initial_local: None,
            precondition_recorded: false,
            preserve: false,
            remote_known: remote_index_contains_path_or_descendants(previous_index, &path),
            initial_fingerprint: None,
            subtree: BTreeMap::new(),
        };
        work_by_path.insert(
            path.clone(),
            PlannedWork::new(
                format!("{prefix}:{path}"),
                path,
                serde_json::to_value(payload)?,
            ),
        );
    }
    let keys = work_by_path
        .iter()
        .map(|(path, work)| (path.clone(), work.key.clone()))
        .collect::<BTreeMap<_, _>>();
    for (path, work) in &mut work_by_path {
        if !work.key.starts_with("2:") {
            work.depends_on = parent_directories(path)
                .into_iter()
                .filter_map(|parent| keys.get(&parent).cloned())
                .collect();
        }
    }
    Ok(work_by_path.into_values().collect())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn apply_remote_snapshot<B: FolderAgentLocalBackend>(
    backend: &mut B,
    options: &FolderAgentRuntimeOptions,
    client: &BerryKeepClient,
    snapshot: &SyncSnapshot,
    local_state: Option<&mut LocalTreeState>,
    changed_paths: Option<&[String]>,
    preserve_local_files: Option<&BTreeSet<String>>,
    matching_remote_files: Option<&BTreeSet<String>>,
    state_store: Option<&StartupStateStore>,
    scope: &PathScope,
    suppressed_uploads: &mut BTreeMap<String, LocalEntryState>,
    remote_index: &mut RemoteTreeIndex,
    modification_log: Option<&ModificationLogStore>,
    modification_context: Option<&ModificationLogContext>,
) -> Result<RemoteApplyOutcome> {
    let journal = journal(state_store)?;
    apply_remote_snapshot_with_journal(
        &journal,
        backend,
        options,
        client,
        snapshot,
        local_state,
        changed_paths,
        preserve_local_files,
        matching_remote_files,
        state_store,
        scope,
        suppressed_uploads,
        remote_index,
        modification_log,
        modification_context,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn apply_remote_snapshot_with_journal<B: FolderAgentLocalBackend>(
    journal: &WorkStore,
    backend: &mut B,
    options: &FolderAgentRuntimeOptions,
    client: &BerryKeepClient,
    snapshot: &SyncSnapshot,
    mut local_state: Option<&mut LocalTreeState>,
    changed_paths: Option<&[String]>,
    preserve_local_files: Option<&BTreeSet<String>>,
    matching_remote_files: Option<&BTreeSet<String>>,
    state_store: Option<&StartupStateStore>,
    scope: &PathScope,
    suppressed_uploads: &mut BTreeMap<String, LocalEntryState>,
    remote_index: &mut RemoteTreeIndex,
    modification_log: Option<&ModificationLogStore>,
    modification_context: Option<&ModificationLogContext>,
) -> Result<RemoteApplyOutcome> {
    let pending = journal.work(SCOPE)?;
    let mut work =
        folder_materialization_plan(snapshot, scope, changed_paths, remote_index, &pending)?;
    let previous = pending
        .into_iter()
        .map(|item| (item.key, item.payload))
        .collect::<BTreeMap<_, _>>();
    for item in &mut work {
        let mut payload: FolderWork = serde_json::from_value(item.payload.clone())?;
        if let Some(previous) = previous.get(&item.key) {
            let previous: FolderWork = serde_json::from_value(previous.clone())?;
            if previous.entry == payload.entry {
                payload.initial_local = previous.initial_local;
                payload.precondition_recorded = previous.precondition_recorded;
                payload.preserve = previous.preserve;
                payload.remote_known |= previous.remote_known;
                payload.initial_fingerprint = previous.initial_fingerprint;
                payload.subtree = previous.subtree;
            }
        }
        if !payload.precondition_recorded {
            payload.initial_local = backend.local_entry_state(options, &payload.path)?;
            if payload
                .initial_local
                .as_ref()
                .is_some_and(|entry| entry.kind == LocalEntryKind::File)
            {
                payload.initial_fingerprint =
                    Some(backend.file_content_fingerprint(options, &payload.path)?);
            }
            if payload
                .initial_local
                .as_ref()
                .is_some_and(|entry| entry.kind == LocalEntryKind::Directory)
                && payload
                    .entry
                    .as_ref()
                    .is_none_or(|entry| entry.kind != EntryKind::Directory)
            {
                let prefix = format!("{}/", payload.path);
                let tree = scan_local_tree_without_status(backend, options)?;
                for (path, state) in tree
                    .into_iter()
                    .filter(|(path, _)| path.starts_with(&prefix))
                {
                    let fingerprint = if state.kind == LocalEntryKind::File {
                        Some(backend.file_content_fingerprint(options, &path)?)
                    } else {
                        None
                    };
                    payload.subtree.insert(path, (state, fingerprint));
                }
            }
            payload.precondition_recorded = true;
        }
        payload.preserve |= preserve_local_files.is_some_and(|paths| paths.contains(&payload.path));
        item.payload = serde_json::to_value(payload)?;
    }
    journal.reconcile(SCOPE, &work)?;
    let mut outcome = RemoteApplyOutcome::default();
    let mut first_error = None;
    let limit = if options.run_once {
        usize::MAX
    } else {
        BATCH_LIMIT
    };
    for _ in 0..limit {
        let Some(lease) = journal.claim_next(SCOPE, now_ms(), 120_000)? else {
            break;
        };
        let work: FolderWork = serde_json::from_value(lease.work.payload.clone())?;
        let mut item_outcome = RemoteApplyOutcome::default();
        let (result, applied) = journal.execute(&lease, || {
            let mut conflict = false;
            let result: Result<()> = (|| {
                if work.availability != ContentAvailability::Complete {
                    anyhow::bail!("folder backend requires complete contents");
                }
                if work.preserve {
                    return Ok(());
                }
                let current = backend.local_entry_state(options, &work.path)?;
                if let Some(entry) = work
                    .entry
                    .as_ref()
                    .filter(|entry| entry.kind == EntryKind::File)
                {
                    let hash = entry
                        .content_fingerprint
                        .as_deref()
                        .or(entry.content_hash.as_deref());
                    if let Some(matching) = matching_local_entry_for_remote_file_change(
                        backend,
                        options,
                        &work.path,
                        hash,
                        state_store,
                    )? {
                        if let Some(state) = local_state.as_deref_mut() {
                            sync_local_state_path_and_parents(
                                backend,
                                options,
                                state,
                                &work.path,
                                Some(matching.clone()),
                            )?;
                        }
                        suppressed_uploads.insert(work.path.clone(), matching);
                        return Ok(());
                    }
                }
                let expected = work.initial_local.as_ref();
                let file_changed = if current
                    .as_ref()
                    .is_some_and(|entry| entry.kind == LocalEntryKind::File)
                {
                    match work.initial_fingerprint.as_ref() {
                        Some(expected) => {
                            backend.file_content_fingerprint(options, &work.path)? != *expected
                        }
                        None => false,
                    }
                } else {
                    false
                };
                let mut subtree_changed = false;
                if expected.is_some_and(|entry| entry.kind == LocalEntryKind::Directory)
                    && work
                        .entry
                        .as_ref()
                        .is_none_or(|entry| entry.kind != EntryKind::Directory)
                {
                    let prefix = format!("{}/", work.path);
                    for (path, state) in scan_local_tree_without_status(backend, options)?
                        .into_iter()
                        .filter(|(path, _)| path.starts_with(&prefix))
                    {
                        let fingerprint = if state.kind == LocalEntryKind::File {
                            Some(backend.file_content_fingerprint(options, &path)?)
                        } else {
                            None
                        };
                        if work.subtree.get(&path) != Some(&(state, fingerprint)) {
                            subtree_changed = true;
                            break;
                        }
                    }
                }
                let already_directory = work
                    .entry
                    .as_ref()
                    .is_some_and(|entry| entry.kind == EntryKind::Directory)
                    && current
                        .as_ref()
                        .is_some_and(|entry| entry.kind == LocalEntryKind::Directory);
                if (current.as_ref() != expected || file_changed || subtree_changed)
                    && !already_directory
                    && !(work.entry.is_none() && current.is_none())
                {
                    conflict = true;
                    anyhow::bail!(
                        "local path changed after remote work was scheduled: {}",
                        work.path
                    );
                }
                let item_snapshot = SyncSnapshot {
                    local: Vec::new(),
                    remote: work.entry.clone().into_iter().collect(),
                };
                let changed = [scope
                    .local_to_remote(&work.path)
                    .unwrap_or_else(|| work.path.clone())];
                let mut item_index = RemoteTreeIndex::default();
                if work.remote_known
                    || remote_index_contains_path_or_descendants(remote_index, &work.path)
                {
                    if remote_index.files.contains(&work.path) {
                        item_index.files.insert(work.path.clone());
                    } else {
                        item_index.directories.insert(work.path.clone());
                    }
                }
                item_outcome = apply_remote_snapshot_entry(
                    backend,
                    options,
                    client,
                    &item_snapshot,
                    local_state.as_deref_mut(),
                    if work.startup { None } else { Some(&changed) },
                    preserve_local_files,
                    matching_remote_files,
                    state_store,
                    scope,
                    suppressed_uploads,
                    &mut item_index,
                    modification_log,
                    modification_context,
                )?;
                Ok(())
            })();
            match result {
                Ok(()) => WorkResult::Complete,
                Err(error) => {
                    let message = format!("{error:#}");
                    tracing::warn!(
                        path = work.path,
                        error = message,
                        "remote materialization retained for retry"
                    );
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                    if conflict {
                        WorkResult::Conflict(message)
                    } else {
                        WorkResult::Retry(message)
                    }
                }
            }
        })?;
        if !applied {
            anyhow::bail!(
                "remote materialization generation was superseded while executing {}",
                work.path
            );
        }
        if matches!(result, WorkResult::Complete) {
            outcome.accumulate(item_outcome);
        }
    }
    if options.run_once
        && let Some(error) = first_error
    {
        return Err(error);
    }
    *remote_index = crate::build_remote_index(snapshot);
    // The shared index builder uses remote paths; folder profiles may select a prefix.
    remote_index.files = remote_index
        .files
        .iter()
        .filter_map(|path| scope.remote_to_local(path))
        .filter(|path| !path.is_empty())
        .collect();
    remote_index.directories = remote_index
        .directories
        .iter()
        .filter_map(|path| scope.remote_to_local(path))
        .filter(|path| !path.is_empty())
        .collect();
    let status = journal.status(SCOPE)?;
    if status.pending != 0 || status.conflicts != 0 {
        tracing::info!(
            pending = status.pending,
            conflicts = status.conflicts,
            "folder materialization work remaining"
        );
    }
    Ok(outcome)
}
