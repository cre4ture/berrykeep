use super::*;

const CONFLICT_REASON: &str = "queued_remote_materialization";

pub(super) fn publish_conflict(state: &StartupStateStore, job: &PlannedWork) -> Result<()> {
    let work: FolderWork = serde_json::from_value(job.payload.clone())?;
    state.upsert_conflict(&work.path, CONFLICT_REASON, &serde_json::to_string(job)?)
}

pub(super) fn clear_resolved_conflict(state: &StartupStateStore, path: &str) -> Result<()> {
    if state
        .load_conflict(path)?
        .is_some_and(|conflict| conflict.reason == CONFLICT_REASON)
    {
        state.remove_conflict(path)?;
    }
    Ok(())
}

pub(crate) fn authorize_queued_conflict(
    state: &StartupStateStore,
    conflict: &crate::StoredConflict,
    strategy: crate::ConflictResolutionStrategy,
    delete_copies: bool,
) -> Result<()> {
    let job: PlannedWork = serde_json::from_str(&conflict.details_json)?;
    let mut work: FolderWork = serde_json::from_value(job.payload.clone())?;
    if work.path != conflict.path {
        anyhow::bail!("queued conflict path does not match the reviewed operation");
    }
    work.resolution = Some(strategy);
    work.preserve = false;
    work.delete_conflict_copies = delete_copies;
    let journal = WorkStore::open(state.path.with_file_name("sync-work.sqlite"))?;
    if !journal.authorize_conflict(SCOPE, &job, &serde_json::to_value(work)?)? {
        anyhow::bail!("queued conflict changed; refresh the conflict before choosing a resolution");
    }
    Ok(())
}

/// Execute the user's explicit local choice through the same folder transfer
/// and logging implementation, scoped to the reviewed path and its descendants.
#[allow(clippy::too_many_arguments)]
pub(super) fn keep_local<B: FolderAgentLocalBackend>(
    backend: &mut B,
    options: &FolderAgentRuntimeOptions,
    client: &BerryKeepClient,
    work: &FolderWork,
    local_state: Option<&mut LocalTreeState>,
    state_store: Option<&StartupStateStore>,
    scope: &PathScope,
    suppressed: &mut BTreeMap<String, LocalEntryState>,
    index: &mut RemoteTreeIndex,
    log: Option<&ModificationLogStore>,
    context: Option<&ModificationLogContext>,
) -> Result<()> {
    let current = backend.local_entry_state(options, &work.path)?;
    if current.is_none() {
        let directory = work
            .initial_local
            .as_ref()
            .is_some_and(|entry| entry.kind == LocalEntryKind::Directory)
            || work
                .entry
                .as_ref()
                .is_some_and(|entry| entry.kind == EntryKind::Directory);
        if let Some(mut remote) = scope.local_to_remote(&work.path) {
            if directory {
                remote.push('/');
            }
            delete_remote_path_with_logging(client, &work.path, &remote, log, context)?;
        }
        if let Some(state) = state_store {
            state.remove_baseline_subtree(&work.path)?;
        }
        if let Some(state) = local_state {
            remove_local_state_path_and_descendants(state, &work.path);
        }
        remove_remote_directory_subtree_from_index(index, &work.path);
        index.files.remove(&work.path);
        remove_suppressed_upload_path_and_descendants(suppressed, &work.path);
        return Ok(());
    }
    if let Some(entry) = work.entry.as_ref() {
        match entry.kind {
            EntryKind::File => {
                index.files.insert(work.path.clone());
            }
            EntryKind::Directory => {
                index.directories.insert(work.path.clone());
            }
        }
    }
    let mut scoped = ResolutionBackend {
        inner: backend,
        path: &work.path,
    };
    let mut selected_state = LocalTreeState::new();
    sync_local_changes(
        &mut scoped,
        options,
        client,
        &mut selected_state,
        state_store,
        scope,
        index,
        suppressed,
        log,
        context,
        None,
        None,
        None,
    )?;
    if let Some(state) = local_state {
        remove_local_state_path_and_descendants(state, &work.path);
        state.extend(selected_state);
    }
    Ok(())
}

struct ResolutionBackend<'a, B> {
    inner: &'a mut B,
    path: &'a str,
}
impl<B: FolderAgentLocalBackend> FolderAgentLocalBackend for ResolutionBackend<'_, B> {
    fn storage_mode_label(&self, options: &FolderAgentRuntimeOptions) -> &'static str {
        self.inner.storage_mode_label(options)
    }
    fn watch_mode_label(&self, options: &FolderAgentRuntimeOptions) -> &'static str {
        self.inner.watch_mode_label(options)
    }
    fn state_identity_root(&self, options: &FolderAgentRuntimeOptions) -> Result<PathBuf> {
        self.inner.state_identity_root(options)
    }
    fn prepare(&mut self, options: &FolderAgentRuntimeOptions) -> Result<()> {
        self.inner.prepare(options)
    }
    fn scan_local_tree_with_progress(
        &mut self,
        options: &FolderAgentRuntimeOptions,
        progress: &mut dyn FnMut(&LocalTreeScanProgress),
    ) -> Result<LocalTreeState> {
        let mut tree = self
            .inner
            .scan_local_tree_with_progress(options, progress)?;
        let prefix = format!("{}/", self.path);
        tree.retain(|path, _| path == self.path || path.starts_with(&prefix));
        Ok(tree)
    }
    fn local_entry_state(
        &mut self,
        options: &FolderAgentRuntimeOptions,
        path: &str,
    ) -> Result<Option<LocalEntryState>> {
        self.inner.local_entry_state(options, path)
    }
    fn file_content_fingerprint(
        &mut self,
        options: &FolderAgentRuntimeOptions,
        path: &str,
    ) -> Result<String> {
        self.inner.file_content_fingerprint(options, path)
    }
    fn ensure_local_directory(
        &mut self,
        options: &FolderAgentRuntimeOptions,
        path: &str,
    ) -> Result<()> {
        self.inner.ensure_local_directory(options, path)
    }
    fn upload_local_file(
        &mut self,
        options: &FolderAgentRuntimeOptions,
        client: &BerryKeepClient,
        scope: &PathScope,
        path: &str,
        size: u64,
    ) -> Result<String> {
        self.inner
            .upload_local_file(options, client, scope, path, size)
    }
    fn download_remote_file(
        &mut self,
        options: &FolderAgentRuntimeOptions,
        client: &BerryKeepClient,
        path: &str,
        remote: &str,
    ) -> Result<()> {
        self.inner
            .download_remote_file(options, client, path, remote)
    }
    fn remove_local_path(&mut self, options: &FolderAgentRuntimeOptions, path: &str) -> Result<()> {
        self.inner.remove_local_path(options, path)
    }
    fn start_local_change_monitor(
        &mut self,
        _options: &FolderAgentRuntimeOptions,
        _wake: FolderAgentWakeCallback,
    ) -> Result<()> {
        Ok(())
    }
}
