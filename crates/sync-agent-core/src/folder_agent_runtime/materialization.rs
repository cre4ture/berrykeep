use super::*;
use crate::sync_work::{ContentAvailability, PlannedWork, WorkResult, WorkStore, now_ms};
use serde::{Deserialize, Serialize};
mod resolution;
pub(crate) use resolution::authorize_queued_conflict;

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
    #[serde(default)]
    baseline_conflict: bool,
    #[serde(default)]
    existing_parent_directories: Vec<String>,
    #[serde(default)]
    resolution: Option<crate::ConflictResolutionStrategy>,
    #[serde(default)]
    delete_conflict_copies: bool,
}

#[cfg(test)]
fn journal(state: Option<&StartupStateStore>) -> Result<WorkStore> {
    match state {
        Some(state) => WorkStore::open(state.path.with_file_name("sync-work.sqlite")),
        // Callers without a persistent baseline are ephemeral/test backends.
        None => WorkStore::open(":memory:"),
    }
}

pub(super) fn open(state: &StartupStateStore) -> Result<WorkStore> {
    let journal = WorkStore::open_for_executor(state.path.with_file_name("sync-work.sqlite"))?;
    journal.maintain(now_ms())?;
    Ok(journal)
}

pub(super) fn next_deadline(journal: &WorkStore) -> Result<Option<Instant>> {
    if let Some(at) = journal.next_ready_at(SCOPE)? {
        return Ok(Some(
            Instant::now() + Duration::from_millis(at.saturating_sub(now_ms()).max(0) as u64),
        ));
    }
    // Existing shutdown polling supplies a bounded wake while conflicts await an
    // explicit resolution from another connection; empty journals have no timer.
    Ok((journal.status(SCOPE)?.conflicts > 0).then(|| Instant::now() + Duration::from_secs(1)))
}

pub(super) fn status_message(journal: &WorkStore, fallback: String) -> Result<String> {
    let status = journal.status(SCOPE)?;
    let remaining = status.pending + status.running;
    Ok(if remaining > 0 || status.conflicts > 0 {
        format!(
            "Sync has {remaining} item(s) remaining and {} conflict(s)",
            status.conflicts
        )
    } else {
        fallback
    })
}

pub(super) fn blocked_local_paths(journal: &WorkStore) -> Result<BTreeSet<String>> {
    Ok(journal.active_resources(SCOPE)?.into_iter().collect())
}
pub(super) fn publish_conflicts(journal: &WorkStore, state: &StartupStateStore) -> Result<()> {
    for job in journal.conflicts(SCOPE)? {
        resolution::publish_conflict(state, &job)?;
    }
    Ok(())
}

pub(super) fn resolution_paths(journal: &WorkStore) -> Result<BTreeSet<String>> {
    Ok(journal
        .work(SCOPE)?
        .into_iter()
        .filter(|job| {
            job.payload
                .get("resolution")
                .is_some_and(|value| !value.is_null())
        })
        .filter_map(|job| {
            job.payload
                .get("path")
                .and_then(|value| value.as_str())
                .map(ToOwned::to_owned)
        })
        .collect())
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
    // Only explicit remote diffs authorize deletion. The mutable upload-known
    // index is not proof of absence in an authoritative remote namespace.
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
            baseline_conflict: false,
            existing_parent_directories: Vec::new(),
            resolution: None,
            delete_conflict_copies: false,
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

#[cfg(test)]
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
    if changed_paths.is_none_or(|paths| !paths.is_empty()) {
        schedule_remote_snapshot(
            journal,
            backend,
            options,
            snapshot,
            changed_paths,
            preserve_local_files,
            state_store,
            scope,
            remote_index,
        )?;
    }
    let outcome = drain_remote_work(
        journal,
        backend,
        options,
        client,
        local_state,
        state_store,
        scope,
        suppressed_uploads,
        remote_index,
        modification_log,
        modification_context,
    )?;
    update_remote_index(snapshot, changed_paths, scope, remote_index);
    let _ = matching_remote_files;
    Ok(outcome)
}

#[allow(clippy::too_many_arguments)]
fn schedule_remote_snapshot<B: FolderAgentLocalBackend>(
    journal: &WorkStore,
    backend: &mut B,
    options: &FolderAgentRuntimeOptions,
    snapshot: &SyncSnapshot,
    changed_paths: Option<&[String]>,
    preserve_local_files: Option<&BTreeSet<String>>,
    state_store: Option<&StartupStateStore>,
    scope: &PathScope,
    remote_index: &RemoteTreeIndex,
) -> Result<()> {
    let active = journal.work(SCOPE)?;
    let all = journal.all_work(SCOPE)?;
    let mut pending = all
        .iter()
        .filter(|job| {
            job.payload
                .get("entry")
                .is_some_and(|entry| !entry.is_null())
        })
        .cloned()
        .collect::<Vec<_>>();
    pending.extend(
        active
            .into_iter()
            .filter(|job| job.payload.get("entry").is_none_or(|entry| entry.is_null())),
    );
    let mut work =
        folder_materialization_plan(snapshot, scope, changed_paths, remote_index, &pending)?;
    let previous = all
        .into_iter()
        .map(|item| (item.key, item.payload))
        .collect::<BTreeMap<_, _>>();
    let baseline = state_store
        .map(StartupStateStore::load_local_baseline)
        .transpose()?;
    let baseline_hashes = state_store
        .map(StartupStateStore::load_local_baseline_hashes)
        .transpose()?
        .unwrap_or_default();
    let mut captured_tree = None;
    let mut fingerprints: BTreeMap<String, String> = BTreeMap::new();
    for item in &mut work {
        let mut payload: FolderWork = serde_json::from_value(item.payload.clone())?;
        if let Some(previous) = previous.get(&item.key) {
            let previous_work: FolderWork = serde_json::from_value(previous.clone())?;
            if previous_work.entry == payload.entry {
                item.payload = previous.clone();
                continue;
            }
        }
        if !payload.precondition_recorded {
            if payload.entry.is_none()
                && let Some(baseline) = baseline.as_ref()
            {
                // A delayed echo of our own delete can arrive after the user
                // creates a new local path. Absence alone grants no permission
                // to delete it or block its upload. Retain actual prior remote
                // or synced-baseline evidence for genuine deletion retries.
                payload.remote_known |= baseline.contains_key(&payload.path);
                payload.preserve |= !payload.remote_known;
            }
            payload.initial_local = backend.local_entry_state(options, &payload.path)?;
            if payload
                .initial_local
                .as_ref()
                .is_some_and(|entry| entry.kind == LocalEntryKind::File)
            {
                payload.initial_fingerprint =
                    Some(backend.file_content_fingerprint(options, &payload.path)?);
            }
            // An unchanged remote echo never authorizes filesystem work. Persist
            // that decision even when the local path still matches here: the user
            // may edit, remove or replace it before the job is executed.
            if payload.resolution.is_none()
                && let Some(baseline) = baseline.as_ref()
                && let Some(synced) = baseline.get(&payload.path)
                && let Some(remote) = payload.entry.as_ref()
            {
                let same_remote_file = synced.kind == LocalEntryKind::File
                    && remote.kind == EntryKind::File
                    && remote
                        .content_fingerprint
                        .as_ref()
                        .or(remote.content_hash.as_ref())
                        .is_some_and(|hash| baseline_hashes.get(&payload.path) == Some(hash));
                let same_remote_directory =
                    synced.kind == LocalEntryKind::Directory && remote.kind == EntryKind::Directory;
                payload.preserve |= same_remote_file || same_remote_directory;
            }
            if payload
                .initial_local
                .as_ref()
                .is_some_and(|entry| entry.kind == LocalEntryKind::Directory)
                && payload
                    .entry
                    .as_ref()
                    .is_none_or(|entry| entry.kind != EntryKind::Directory)
                && !payload.preserve
            {
                let prefix = format!("{}/", payload.path);
                if captured_tree.is_none() {
                    captured_tree = Some(scan_local_tree_without_status(backend, options)?);
                }
                for (path, state) in captured_tree
                    .as_ref()
                    .expect("captured tree")
                    .range(prefix.clone()..)
                    .take_while(|(path, _)| path.starts_with(&prefix))
                    .map(|(path, state)| (path.clone(), state.clone()))
                {
                    let fingerprint = if state.kind == LocalEntryKind::File {
                        Some(if let Some(hash) = fingerprints.get(&path) {
                            hash.clone()
                        } else {
                            let hash = backend.file_content_fingerprint(options, &path)?;
                            fingerprints.insert(path.clone(), hash.clone());
                            hash
                        })
                    } else {
                        None
                    };
                    payload.subtree.insert(path, (state, fingerprint));
                }
            }
            payload.precondition_recorded = true;
            if let Some(baseline) = baseline.as_ref()
                && let Some(current) = payload.initial_local.as_ref()
                && !payload.preserve
            {
                let same_kind_directory = current.kind == LocalEntryKind::Directory
                    && payload
                        .entry
                        .as_ref()
                        .is_some_and(|entry| entry.kind == EntryKind::Directory);
                let metadata_changed =
                    !same_kind_directory && baseline.get(&payload.path) != Some(current);
                let content_changed = payload
                    .initial_fingerprint
                    .as_ref()
                    .is_some_and(|hash| baseline_hashes.get(&payload.path) != Some(hash));
                let subtree_changed = payload.subtree.iter().any(|(path, (state, hash))| {
                    (if state.kind == LocalEntryKind::Directory {
                        baseline
                            .get(path)
                            .is_none_or(|entry| entry.kind != LocalEntryKind::Directory)
                    } else {
                        baseline.get(path) != Some(state)
                    }) || hash
                        .as_ref()
                        .is_some_and(|hash| baseline_hashes.get(path) != Some(hash))
                });
                payload.baseline_conflict = metadata_changed || content_changed || subtree_changed;
            }
        }
        if payload.entry.is_some()
            && !payload.preserve
            && let Some(baseline) = baseline.as_ref()
        {
            for parent in parent_directories(&payload.path) {
                if baseline
                    .get(&parent)
                    .is_some_and(|entry| entry.kind == LocalEntryKind::Directory)
                {
                    payload.existing_parent_directories.push(parent.clone());
                    if backend
                        .local_entry_state(options, &parent)?
                        .is_none_or(|entry| entry.kind != LocalEntryKind::Directory)
                    {
                        payload.baseline_conflict = true;
                    }
                }
            }
        }
        payload.preserve |= preserve_local_files.is_some_and(|paths| paths.contains(&payload.path));
        item.payload = serde_json::to_value(payload)?;
    }
    journal.reconcile(SCOPE, &work)?;
    if let Some(state) = state_store {
        for job in journal.conflicts(SCOPE)? {
            resolution::publish_conflict(state, &job)?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn drain_remote_work<B: FolderAgentLocalBackend>(
    journal: &WorkStore,
    backend: &mut B,
    options: &FolderAgentRuntimeOptions,
    client: &BerryKeepClient,
    mut local_state: Option<&mut LocalTreeState>,
    state_store: Option<&StartupStateStore>,
    scope: &PathScope,
    suppressed_uploads: &mut BTreeMap<String, LocalEntryState>,
    remote_index: &mut RemoteTreeIndex,
    modification_log: Option<&ModificationLogStore>,
    modification_context: Option<&ModificationLogContext>,
) -> Result<RemoteApplyOutcome> {
    let mut current_tree = None;
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
        let mut work: FolderWork = serde_json::from_value(lease.work.payload.clone())?;
        let mut item_outcome = RemoteApplyOutcome::default();
        let (result, applied) = journal.execute(&lease, || {
            let mut conflict = false;
            let result: Result<()> = (|| {
                if work.availability != ContentAvailability::Complete {
                    anyhow::bail!("folder backend requires complete contents");
                }
                if matches!(
                    work.resolution,
                    Some(crate::ConflictResolutionStrategy::KeepLocal)
                ) {
                    resolution::keep_local(
                        backend,
                        options,
                        client,
                        &work,
                        local_state.as_deref_mut(),
                        state_store,
                        scope,
                        suppressed_uploads,
                        remote_index,
                        modification_log,
                        modification_context,
                    )?;
                    return Ok(());
                }
                if work.preserve {
                    return Ok(());
                }
                for parent in &work.existing_parent_directories {
                    if backend
                        .local_entry_state(options, parent)?
                        .is_none_or(|entry| entry.kind != LocalEntryKind::Directory)
                    {
                        conflict = true;
                        anyhow::bail!(
                            "parent directory {parent} was removed or replaced; restore it before applying remote work for {}",
                            work.path
                        );
                    }
                }
                let mut current = backend.local_entry_state(options, &work.path)?;
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
                if matches!(
                    work.resolution,
                    Some(crate::ConflictResolutionStrategy::KeepRemote)
                ) {
                    backend.preserve_local_conflict(options, &work.path)?;
                    if let Some(state) = local_state.as_deref_mut() {
                        remove_local_state_path_and_descendants(state, &work.path);
                    }
                    if let Some(state) = state_store {
                        state.remove_baseline_subtree(&work.path)?;
                    }
                    current = backend.local_entry_state(options, &work.path)?;
                    work.initial_local = current.clone();
                    work.initial_fingerprint = if current
                        .as_ref()
                        .is_some_and(|entry| entry.kind == LocalEntryKind::File)
                    {
                        Some(backend.file_content_fingerprint(options, &work.path)?)
                    } else {
                        None
                    };
                    work.subtree.clear();
                    work.baseline_conflict = false;
                    work.startup = false;
                    work.remote_known = true;
                    current_tree = None;
                }
                let expected = work.initial_local.as_ref();
                if work.baseline_conflict {
                    conflict = true;
                    anyhow::bail!(
                        "local content diverged from the synced baseline before scheduling {}",
                        work.path
                    );
                }
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
                    if current_tree.is_none() {
                        current_tree = Some(scan_local_tree_without_status(backend, options)?);
                    }
                    for (path, state) in current_tree
                        .as_ref()
                        .expect("current tree")
                        .range(prefix.clone()..)
                        .take_while(|(path, _)| path.starts_with(&prefix))
                        .map(|(path, state)| (path.clone(), state.clone()))
                    {
                        let fingerprint = if state.kind == LocalEntryKind::File {
                            Some(backend.file_content_fingerprint(options, &path)?)
                        } else {
                            None
                        };
                        let changed = match work.subtree.get(&path) {
                            Some((previous, _hash)) if state.kind == LocalEntryKind::Directory => {
                                previous.kind != LocalEntryKind::Directory
                            }
                            Some((previous, hash)) => previous != &state || hash != &fingerprint,
                            None => true,
                        };
                        if changed {
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
                let same_directory =
                    current
                        .as_ref()
                        .zip(expected)
                        .is_some_and(|(current, expected)| {
                            current.kind == LocalEntryKind::Directory
                                && expected.kind == LocalEntryKind::Directory
                        });
                if ((current.as_ref() != expected && !same_directory)
                    || file_changed
                    || subtree_changed)
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
                    None,
                    None,
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
            tracing::warn!(
                path = work.path,
                "remote work lease was superseded; retaining earlier progress and deferring replay"
            );
            continue;
        }
        if matches!(result, WorkResult::Complete) {
            if let Some(state) = state_store {
                resolution::clear_resolved_conflict(state, &work.path)?;
            }
            if work.delete_conflict_copies {
                let _ = crate::delete_conflict_copies(&options.root_dir, &work.path)?;
            }
            item_outcome.completed_work_count = 1;
            outcome.accumulate(item_outcome);
            if let Some(tree) = current_tree.as_mut() {
                if work
                    .entry
                    .as_ref()
                    .is_none_or(|entry| entry.kind == EntryKind::File)
                {
                    remove_local_state_path_and_descendants(tree, &work.path);
                }
                if let Some(entry) = backend.local_entry_state(options, &work.path)? {
                    tree.insert(work.path.clone(), entry);
                }
            }
        }
        if matches!(result, WorkResult::Conflict(_))
            && let Some(state) = state_store
        {
            resolution::publish_conflict(state, &lease.work)?;
        }
    }
    if options.run_once
        && let Some(error) = first_error
    {
        return Err(error);
    }
    let status = journal.status(SCOPE)?;
    if options.run_once && (status.pending != 0 || status.running != 0 || status.conflicts != 0) {
        anyhow::bail!(
            "one-shot sync is incomplete: {} pending, {} running, {} conflicts; unfinished work is retained for retry",
            status.pending,
            status.running,
            status.conflicts
        );
    }
    if status.pending != 0 || status.conflicts != 0 {
        tracing::info!(
            pending = status.pending,
            conflicts = status.conflicts,
            "folder materialization work remaining"
        );
    }
    Ok(outcome)
}

fn update_remote_index(
    snapshot: &SyncSnapshot,
    changed: Option<&[String]>,
    scope: &PathScope,
    index: &mut RemoteTreeIndex,
) {
    if changed.is_none() {
        let full = crate::build_remote_index(snapshot);
        index.files = full
            .files
            .iter()
            .filter_map(|path| scope.remote_to_local(path))
            .filter(|path| !path.is_empty())
            .collect();
        index.directories = full
            .directories
            .iter()
            .filter_map(|path| scope.remote_to_local(path))
            .filter(|path| !path.is_empty())
            .collect();
        return;
    }
    let entries = snapshot
        .remote
        .iter()
        .filter_map(|entry| {
            scope
                .remote_to_local(&entry.path)
                .map(|path| (path, entry.kind))
        })
        .collect::<BTreeMap<_, _>>();
    for path in changed
        .into_iter()
        .flatten()
        .filter_map(|path| scope.remote_to_local(path))
        .filter(|path| !path.is_empty())
    {
        match entries.get(&path) {
            Some(EntryKind::File) => {
                remove_remote_directory_subtree_from_index(index, &path);
                index.files.insert(path);
            }
            Some(EntryKind::Directory) => {
                index.files.remove(&path);
                index.directories.insert(path);
            }
            None => {
                index.files.remove(&path);
                remove_remote_directory_subtree_from_index(index, &path);
            }
        }
    }
}
