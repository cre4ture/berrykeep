use crate::bootstrap::ConnectionBootstrap;
use crate::ironmesh_client::{IronMeshClient, VersionConsistencyState};
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;
use sync_core::{EntryKind, NamespaceEntry, SyncSnapshot};
use transport_sdk::ClientIdentityMaterial;

const SNAPSHOT_BUILD_PROGRESS_STRIDE: u64 = 512;
const PREFERRED_SERVER_NOTIFICATION_WAIT_TIMEOUT: Duration = Duration::from_secs(2);
const STOP_CHECK_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy)]
pub struct RemoteSyncStrategy {
    mode: RemoteSyncMode,
}

#[derive(Debug, Clone, Copy)]
enum RemoteSyncMode {
    Polling {
        interval: Duration,
    },
    ServerNotifications {
        wait_timeout: Duration,
        retry_interval: Duration,
    },
}

impl RemoteSyncStrategy {
    pub fn polling(interval: Duration) -> Self {
        Self {
            mode: RemoteSyncMode::Polling {
                interval: interval.max(Duration::from_millis(250)),
            },
        }
    }

    pub fn server_notifications(wait_timeout: Duration, retry_interval: Duration) -> Self {
        Self {
            mode: RemoteSyncMode::ServerNotifications {
                wait_timeout: wait_timeout.max(Duration::from_millis(250)),
                retry_interval: retry_interval.max(Duration::from_millis(250)),
            },
        }
    }
}

impl Default for RemoteSyncStrategy {
    fn default() -> Self {
        Self::polling(Duration::from_secs(3))
    }
}

#[derive(Debug, Clone)]
pub struct RemoteSyncScheduler {
    strategy: RemoteSyncStrategy,
}

impl RemoteSyncScheduler {
    pub fn new(strategy: RemoteSyncStrategy) -> Self {
        Self { strategy }
    }

    pub fn wait_for_next_tick_blocking(&self, running: &AtomicBool) -> bool {
        sleep_until_or_stop(self.fallback_interval(), running)
    }

    pub fn spawn_loop<F>(&self, running: Arc<AtomicBool>, mut on_tick: F) -> JoinHandle<()>
    where
        F: FnMut() + Send + 'static,
    {
        let interval = self.fallback_interval();
        thread::spawn(move || {
            while running.load(Ordering::SeqCst) {
                if !sleep_until_or_stop(interval, &running) {
                    break;
                }
                if !running.load(Ordering::SeqCst) {
                    break;
                }
                on_tick();
            }
        })
    }

    fn fallback_interval(&self) -> Duration {
        match self.strategy.mode {
            RemoteSyncMode::Polling { interval } => interval,
            RemoteSyncMode::ServerNotifications { retry_interval, .. } => retry_interval,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RemoteSnapshotUpdate {
    pub snapshot: SyncSnapshot,
    pub changed_paths: Vec<String>,
    /// Deletions whose preferred remote tombstone directly proves the removed
    /// predecessor. Consumers must apply a deletion only after matching it to
    /// the entry they currently hold.
    pub deletions: Vec<RemoteDeletion>,
}

impl RemoteSnapshotUpdate {
    fn snapshot_only(snapshot: SyncSnapshot) -> Self {
        Self {
            snapshot,
            changed_paths: Vec::new(),
            deletions: Vec::new(),
        }
    }
}

/// The identity metadata needed to prove that a remote tombstone supersedes a
/// specific entry. A path without concrete identity metadata is never enough
/// to authorize deletion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEntryBaseline {
    pub path: String,
    pub version: Option<String>,
    pub content_hash: Option<String>,
    pub modified_at_unix: Option<u64>,
}

impl From<&NamespaceEntry> for RemoteEntryBaseline {
    fn from(entry: &NamespaceEntry) -> Self {
        Self {
            path: entry.path.clone(),
            version: entry.version.clone(),
            content_hash: entry.content_hash.clone(),
            modified_at_unix: entry.modified_at_unix,
        }
    }
}

/// A current, confirmed tombstone and the direct predecessor records it
/// supersedes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteDeletion {
    pub path: String,
    pub tombstone_version: String,
    pub tombstone_created_at_unix: u64,
    pub predecessors: Vec<RemoteEntryBaseline>,
}

impl RemoteDeletion {
    /// Returns true only when this deletion proves it supersedes `baseline`.
    ///
    /// A concrete revision is authoritative, but any available hash or time
    /// must agree as well. Without a concrete revision, both content hash and
    /// modification time are required. This deliberately never matches from a
    /// path or absence of metadata alone.
    pub fn matches_baseline(&self, baseline: &RemoteEntryBaseline) -> bool {
        self.path == baseline.path
            && self
                .predecessors
                .iter()
                .any(|predecessor| baseline_matches_predecessor(baseline, predecessor))
    }
}

/// Whether a revision is safe to use as an authoritative remote identity.
/// Legacy `server-head` placeholders are intentionally not concrete.
pub fn is_concrete_remote_revision(revision: Option<&str>) -> bool {
    revision
        .map(str::trim)
        .is_some_and(|revision| !revision.is_empty() && !revision.starts_with("server-head"))
}

fn baseline_matches_predecessor(
    baseline: &RemoteEntryBaseline,
    predecessor: &RemoteEntryBaseline,
) -> bool {
    if baseline.path != predecessor.path {
        return false;
    }

    if is_concrete_remote_revision(baseline.version.as_deref()) {
        if baseline.version != predecessor.version {
            return false;
        }
        return optional_values_do_not_conflict(&baseline.content_hash, &predecessor.content_hash)
            && optional_values_do_not_conflict(
                &baseline.modified_at_unix,
                &predecessor.modified_at_unix,
            );
    }

    baseline.content_hash.is_some()
        && baseline.modified_at_unix.is_some()
        && baseline.content_hash == predecessor.content_hash
        && baseline.modified_at_unix == predecessor.modified_at_unix
}

fn optional_values_do_not_conflict<T: Eq>(left: &Option<T>, right: &Option<T>) -> bool {
    left.as_ref()
        .zip(right.as_ref())
        .is_none_or(|(left, right)| left == right)
}

#[derive(Debug, Clone)]
pub struct RemoteSnapshotScope {
    pub prefix: Option<String>,
    pub depth: usize,
    pub snapshot: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteSnapshotFetchProgress {
    pub phase: String,
    pub entry_count: u64,
    pub processed_entry_count: u64,
    pub file_count: u64,
    pub directory_count: u64,
    pub current_path: Option<String>,
}

impl RemoteSnapshotScope {
    pub fn new(prefix: Option<String>, depth: usize, snapshot: Option<String>) -> Self {
        Self {
            prefix,
            depth: depth.max(1),
            snapshot,
        }
    }
}

#[derive(Clone)]
pub struct RemoteSnapshotFetcher {
    client: IronMeshClient,
    scope: RemoteSnapshotScope,
}

impl RemoteSnapshotFetcher {
    pub fn new(client: IronMeshClient, scope: RemoteSnapshotScope) -> Self {
        Self { client, scope }
    }

    pub fn from_bootstrap(
        bootstrap: &ConnectionBootstrap,
        identity: Option<&ClientIdentityMaterial>,
        prefix: Option<String>,
        depth: usize,
        snapshot: Option<String>,
    ) -> Result<Self> {
        let client = bootstrap.build_client_with_optional_identity(identity)?;
        let scope = RemoteSnapshotScope::new(prefix, depth, snapshot);
        Ok(Self::new(client, scope))
    }

    pub fn from_direct_base_url(
        base_url: impl Into<String>,
        prefix: Option<String>,
        depth: usize,
        snapshot: Option<String>,
    ) -> Self {
        let client = IronMeshClient::from_direct_base_url(base_url);
        let scope = RemoteSnapshotScope::new(prefix, depth, snapshot);
        Self::new(client, scope)
    }

    pub fn fetch_snapshot_blocking(&self) -> Result<SyncSnapshot> {
        self.client.load_snapshot_from_server_blocking(
            self.scope.prefix.as_deref(),
            self.scope.depth,
            self.scope.snapshot.as_deref(),
        )
    }

    /// Fetches the visible remote snapshot and confirms only candidate paths
    /// that are now missing through their version history. Passing all entries
    /// from the prior snapshot is efficient: history is queried only for paths
    /// absent from this newly fetched snapshot. Passing candidates at startup
    /// lets a caller validate persisted entries before it adopts a fresh index.
    pub fn fetch_snapshot_update_blocking(
        &self,
        baseline_candidates: &[RemoteEntryBaseline],
    ) -> Result<RemoteSnapshotUpdate> {
        let snapshot = self.fetch_snapshot_blocking()?;
        self.snapshot_update_for_snapshot_blocking(snapshot, baseline_candidates)
    }

    fn snapshot_update_for_snapshot_blocking(
        &self,
        snapshot: SyncSnapshot,
        baseline_candidates: &[RemoteEntryBaseline],
    ) -> Result<RemoteSnapshotUpdate> {
        let visible_paths: BTreeSet<&str> = snapshot
            .remote
            .iter()
            .map(|entry| entry.path.as_str())
            .collect();
        let missing_candidates: Vec<_> = baseline_candidates
            .iter()
            .filter(|candidate| !visible_paths.contains(candidate.path.as_str()))
            .cloned()
            .collect();
        let deletions = self.confirm_missing_deletions_blocking(&missing_candidates)?;

        Ok(RemoteSnapshotUpdate {
            snapshot,
            changed_paths: Vec::new(),
            deletions,
        })
    }

    fn confirm_missing_deletions_blocking(
        &self,
        candidates: &[RemoteEntryBaseline],
    ) -> Result<Vec<RemoteDeletion>> {
        let mut candidates_by_path: BTreeMap<&str, Vec<&RemoteEntryBaseline>> = BTreeMap::new();
        for candidate in candidates {
            candidates_by_path
                .entry(candidate.path.as_str())
                .or_default()
                .push(candidate);
        }

        let mut confirmed_deletions = Vec::new();
        for (path, path_candidates) in candidates_by_path {
            let graph = match self.client.list_versions_blocking(path) {
                Ok(Some(graph)) => graph,
                Ok(None) => continue,
                Err(error) => {
                    tracing::warn!(
                        "remote-refresh: unable to verify possible deletion for {path}; preserving the prior entry: {error:#}"
                    );
                    continue;
                }
            };
            let Some(preferred_head_version_id) = graph.preferred_head_version_id.as_deref() else {
                continue;
            };
            let Some(tombstone) = graph
                .versions
                .iter()
                .find(|version| version.version_id == preferred_head_version_id)
            else {
                continue;
            };
            if tombstone.entry_type != "tombstone"
                || tombstone.state != VersionConsistencyState::Confirmed
            {
                continue;
            }

            let predecessors: Vec<_> = tombstone
                .parent_version_ids
                .iter()
                .filter_map(|parent_version_id| {
                    graph.versions.iter().find(|version| {
                        version.version_id == *parent_version_id && version.entry_type == "key"
                    })
                })
                .map(|predecessor| RemoteEntryBaseline {
                    path: predecessor
                        .logical_path
                        .clone()
                        .unwrap_or_else(|| path.to_string()),
                    version: Some(predecessor.version_id.clone()),
                    content_hash: predecessor.content_hash.clone(),
                    modified_at_unix: Some(predecessor.created_at_unix),
                })
                .collect();
            if predecessors.is_empty() {
                continue;
            }

            let deletion = RemoteDeletion {
                path: path.to_string(),
                tombstone_version: tombstone.version_id.clone(),
                tombstone_created_at_unix: tombstone.created_at_unix,
                predecessors,
            };
            if path_candidates
                .iter()
                .any(|candidate| deletion.matches_baseline(candidate))
            {
                confirmed_deletions.push(deletion);
            }
        }

        Ok(confirmed_deletions)
    }

    pub fn fetch_snapshot_blocking_with_progress<F>(
        &self,
        mut on_progress: F,
    ) -> Result<SyncSnapshot>
    where
        F: FnMut(RemoteSnapshotFetchProgress),
    {
        on_progress(RemoteSnapshotFetchProgress {
            phase: "requesting-store-index".to_string(),
            ..RemoteSnapshotFetchProgress::default()
        });

        let response = self.client.store_index_blocking(
            self.scope.prefix.as_deref(),
            self.scope.depth,
            self.scope.snapshot.as_deref(),
        )?;

        let total_entries = response.entry_count.max(response.entries.len()) as u64;
        on_progress(RemoteSnapshotFetchProgress {
            phase: "received-store-index".to_string(),
            entry_count: total_entries,
            ..RemoteSnapshotFetchProgress::default()
        });

        let snapshot =
            snapshot_from_store_index_entries_with_progress(response.entries, |progress| {
                on_progress(progress);
            });

        let file_count = snapshot
            .remote
            .iter()
            .filter(|entry| entry.kind == EntryKind::File)
            .count() as u64;
        let directory_count = snapshot.remote.len() as u64 - file_count;
        on_progress(RemoteSnapshotFetchProgress {
            phase: "completed".to_string(),
            entry_count: snapshot.remote.len() as u64,
            processed_entry_count: total_entries,
            file_count,
            directory_count,
            current_path: None,
        });

        Ok(snapshot)
    }
}

fn snapshot_from_store_index_entries_with_progress<F>(
    entries: Vec<crate::ironmesh_client::StoreIndexEntry>,
    mut on_progress: F,
) -> SyncSnapshot
where
    F: FnMut(RemoteSnapshotFetchProgress),
{
    let total_entries = entries.len() as u64;
    let mut remote = Vec::with_capacity(entries.len());
    let mut file_count = 0u64;
    let mut directory_count = 0u64;

    for (index, entry) in entries.into_iter().enumerate() {
        if (entry.entry_type == "prefix") || entry.path.ends_with('/') {
            let directory_path = entry.path.trim_end_matches('/').to_string();
            if !directory_path.is_empty() {
                directory_count += 1;
                remote.push(NamespaceEntry::directory(directory_path));
            }
        } else {
            let version = entry.version.unwrap_or_else(|| "server-head".to_string());
            let content_hash = entry
                .content_hash
                .unwrap_or_else(|| format!("server-head:{}", entry.path));
            let mut remote_entry = NamespaceEntry::file_sized(
                entry.path.clone(),
                version,
                content_hash,
                entry.size_bytes,
            );
            remote_entry.content_fingerprint = entry.content_fingerprint;
            remote_entry.modified_at_unix = entry.modified_at_unix;
            remote_entry.media = entry
                .media
                .map(crate::ironmesh_client::namespace_media_metadata);
            file_count += 1;
            remote.push(remote_entry);
        }

        let processed_entry_count = (index + 1) as u64;
        if processed_entry_count == 1
            || processed_entry_count == total_entries
            || processed_entry_count.is_multiple_of(SNAPSHOT_BUILD_PROGRESS_STRIDE)
        {
            on_progress(RemoteSnapshotFetchProgress {
                phase: "building-snapshot".to_string(),
                entry_count: total_entries,
                processed_entry_count,
                file_count,
                directory_count,
                current_path: remote.last().map(|entry| entry.path.clone()),
            });
        }
    }

    SyncSnapshot {
        local: Vec::new(),
        remote,
    }
}

#[derive(Debug, Clone)]
pub struct RemoteSnapshotPoller {
    scheduler: RemoteSyncScheduler,
}

impl RemoteSnapshotPoller {
    pub fn polling(interval: Duration) -> Self {
        Self {
            scheduler: RemoteSyncScheduler::new(RemoteSyncStrategy::polling(interval)),
        }
    }

    pub fn prefer_server_notifications(retry_interval: Duration) -> Self {
        Self::server_notifications(
            preferred_server_notification_wait_timeout(retry_interval),
            retry_interval,
        )
    }

    pub fn server_notifications(wait_timeout: Duration, retry_interval: Duration) -> Self {
        Self {
            scheduler: RemoteSyncScheduler::new(RemoteSyncStrategy::server_notifications(
                wait_timeout,
                retry_interval,
            )),
        }
    }

    pub fn spawn_fetcher_loop<C>(
        &self,
        running: Arc<AtomicBool>,
        initial_snapshot: Option<SyncSnapshot>,
        fetcher: RemoteSnapshotFetcher,
        on_change: C,
    ) -> JoinHandle<()>
    where
        C: FnMut(RemoteSnapshotUpdate) + Send + 'static,
    {
        self.spawn_fetcher_loop_with_update_fetch(
            running,
            initial_snapshot,
            fetcher,
            |fetcher, baseline_candidates| {
                fetcher.fetch_snapshot_update_blocking(baseline_candidates)
            },
            on_change,
        )
    }

    pub fn spawn_fetcher_loop_with_fetch<F, C>(
        &self,
        running: Arc<AtomicBool>,
        initial_snapshot: Option<SyncSnapshot>,
        fetcher: RemoteSnapshotFetcher,
        mut fetch_snapshot: F,
        on_change: C,
    ) -> JoinHandle<()>
    where
        F: FnMut(&RemoteSnapshotFetcher) -> Result<SyncSnapshot> + Send + 'static,
        C: FnMut(RemoteSnapshotUpdate) + Send + 'static,
    {
        self.spawn_fetcher_loop_with_update_fetch(
            running,
            initial_snapshot,
            fetcher,
            move |fetcher, _baseline_candidates| {
                let snapshot = fetch_snapshot(fetcher)?;
                fetcher.snapshot_update_for_snapshot_blocking(snapshot, _baseline_candidates)
            },
            on_change,
        )
    }

    fn spawn_fetcher_loop_with_update_fetch<F, C>(
        &self,
        running: Arc<AtomicBool>,
        initial_snapshot: Option<SyncSnapshot>,
        fetcher: RemoteSnapshotFetcher,
        mut fetch_snapshot: F,
        mut on_change: C,
    ) -> JoinHandle<()>
    where
        F: FnMut(&RemoteSnapshotFetcher, &[RemoteEntryBaseline]) -> Result<RemoteSnapshotUpdate>
            + Send
            + 'static,
        C: FnMut(RemoteSnapshotUpdate) + Send + 'static,
    {
        match self.scheduler.strategy.mode {
            RemoteSyncMode::Polling { .. } => {
                let scheduler = self.scheduler.clone();
                thread::spawn(move || {
                    let mut current_snapshot = initial_snapshot;
                    while running.load(Ordering::SeqCst) {
                        if !scheduler.wait_for_next_tick_blocking(&running)
                            || !running.load(Ordering::SeqCst)
                        {
                            break;
                        }
                        let baseline_candidates = current_snapshot
                            .as_ref()
                            .map(remote_entry_baselines)
                            .unwrap_or_default();
                        let next_snapshot = match fetch_snapshot(&fetcher, &baseline_candidates) {
                            Ok(snapshot) => snapshot,
                            Err(error) => {
                                tracing::warn!("remote-refresh: snapshot polling error: {error:#}");
                                continue;
                            }
                        };
                        apply_snapshot_update(&mut current_snapshot, next_snapshot, &mut on_change);
                    }
                })
            }
            RemoteSyncMode::ServerNotifications {
                wait_timeout,
                retry_interval: _,
            } => {
                let scheduler = self.scheduler.clone();
                thread::spawn(move || {
                    let mut current_snapshot = initial_snapshot;
                    let mut last_sequence = 0u64;
                    let mut notifications_available = true;

                    while running.load(Ordering::SeqCst) {
                        let mut should_fetch = current_snapshot.is_none();
                        let mut observed_sequence = last_sequence;
                        let mut sequence_regressed = false;
                        if notifications_available {
                            match fetcher.client.wait_for_store_index_change_blocking(
                                last_sequence,
                                wait_timeout.as_millis() as u64,
                            ) {
                                Ok(response) => {
                                    observed_sequence = response.sequence;
                                    if response.sequence < last_sequence {
                                        tracing::warn!(
                                            "remote-refresh: server change sequence regressed from {last_sequence} to {}; reloading snapshot to rebaseline notification state",
                                            response.sequence
                                        );
                                        sequence_regressed = true;
                                        should_fetch = true;
                                    } else {
                                        should_fetch |= response.changed;
                                    }
                                }
                                Err(error) => {
                                    if should_fallback_to_polling_after_wait_error(&error) {
                                        tracing::warn!(
                                            "remote-refresh: server change wait unavailable, falling back to polling: {error:#}"
                                        );
                                        notifications_available = false;
                                    } else {
                                        tracing::warn!(
                                            "remote-refresh: server change wait failed, polling snapshot before retrying notifications: {error:#}"
                                        );
                                        if !scheduler.wait_for_next_tick_blocking(&running) {
                                            break;
                                        }
                                        should_fetch = true;
                                    }
                                }
                            }
                        }

                        if !notifications_available
                            && !scheduler.wait_for_next_tick_blocking(&running)
                        {
                            break;
                        }
                        if !notifications_available {
                            should_fetch = true;
                        }
                        if !running.load(Ordering::SeqCst) {
                            break;
                        }
                        if !should_fetch {
                            continue;
                        }

                        let baseline_candidates = current_snapshot
                            .as_ref()
                            .map(remote_entry_baselines)
                            .unwrap_or_default();
                        let mut next_snapshot = match fetch_snapshot(&fetcher, &baseline_candidates)
                        {
                            Ok(snapshot) => snapshot,
                            Err(error) => {
                                tracing::warn!("remote-refresh: snapshot refresh error: {error:#}");
                                continue;
                            }
                        };
                        if sequence_regressed {
                            // A regressed notification cannot establish that a
                            // tombstone is newer than the accepted snapshot.
                            // Rebaseline the sequence and accept only upserts.
                            next_snapshot.deletions.clear();
                        }
                        last_sequence = observed_sequence;
                        apply_snapshot_update(&mut current_snapshot, next_snapshot, &mut on_change);
                    }
                })
            }
        }
    }

    pub fn spawn_changed_paths_loop<F, C>(
        &self,
        running: Arc<AtomicBool>,
        mut current_snapshot: Option<SyncSnapshot>,
        mut fetch_snapshot: F,
        mut on_change: C,
    ) -> JoinHandle<()>
    where
        F: FnMut() -> Result<SyncSnapshot> + Send + 'static,
        C: FnMut(RemoteSnapshotUpdate) + Send + 'static,
    {
        let scheduler = self.scheduler.clone();
        thread::spawn(move || {
            while running.load(Ordering::SeqCst) {
                if !scheduler.wait_for_next_tick_blocking(&running) {
                    break;
                }
                if !running.load(Ordering::SeqCst) {
                    break;
                }

                let next_snapshot = match fetch_snapshot() {
                    Ok(snapshot) => snapshot,
                    Err(error) => {
                        tracing::warn!("remote-refresh: snapshot polling error: {error:#}");
                        continue;
                    }
                };
                apply_snapshot_update(
                    &mut current_snapshot,
                    RemoteSnapshotUpdate::snapshot_only(next_snapshot),
                    &mut on_change,
                );
            }
        })
    }
}

fn preferred_server_notification_wait_timeout(retry_interval: Duration) -> Duration {
    PREFERRED_SERVER_NOTIFICATION_WAIT_TIMEOUT.max(retry_interval)
}
fn should_fallback_to_polling_after_wait_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        let message = cause.to_string();
        message.contains("/store/index/changes/wait returned non-success status: 404")
            || message.contains("/store/index/changes/wait returned non-success status: 405")
            || message.contains("/store/index/changes/wait returned non-success status: 501")
    })
}

fn apply_snapshot_update<C>(
    current_snapshot: &mut Option<SyncSnapshot>,
    next_update: RemoteSnapshotUpdate,
    on_change: &mut C,
) where
    C: FnMut(RemoteSnapshotUpdate),
{
    if let Some(previous) = current_snapshot.as_ref() {
        let (merged_snapshot, deletions) = merge_snapshot_update(previous, next_update);
        let changed_paths = changed_paths_between(previous, &merged_snapshot);
        if !changed_paths.is_empty() {
            on_change(RemoteSnapshotUpdate {
                snapshot: merged_snapshot.clone(),
                changed_paths,
                deletions,
            });
        }
        *current_snapshot = Some(merged_snapshot);
        return;
    }

    *current_snapshot = Some(next_update.snapshot);
}

fn merge_snapshot_update(
    previous: &SyncSnapshot,
    next_update: RemoteSnapshotUpdate,
) -> (SyncSnapshot, Vec<RemoteDeletion>) {
    let mut merged_entries: BTreeMap<String, NamespaceEntry> = previous
        .remote
        .iter()
        .cloned()
        .map(|entry| (entry.path.clone(), entry))
        .collect();
    let next_paths: BTreeSet<String> = next_update
        .snapshot
        .remote
        .iter()
        .map(|entry| entry.path.clone())
        .collect();

    for entry in next_update.snapshot.remote {
        merged_entries.insert(entry.path.clone(), entry);
    }

    let mut applied_deletions = Vec::new();
    for deletion in next_update.deletions {
        if next_paths.contains(&deletion.path) {
            continue;
        }
        let Some(previous_entry) = previous
            .remote
            .iter()
            .find(|entry| entry.path == deletion.path)
        else {
            continue;
        };
        if deletion.matches_baseline(&RemoteEntryBaseline::from(previous_entry)) {
            merged_entries.remove(&deletion.path);
            applied_deletions.push(deletion);
        }
    }

    (
        SyncSnapshot {
            local: next_update.snapshot.local,
            remote: merged_entries.into_values().collect(),
        },
        applied_deletions,
    )
}

fn remote_entry_baselines(snapshot: &SyncSnapshot) -> Vec<RemoteEntryBaseline> {
    snapshot
        .remote
        .iter()
        .filter(|entry| entry.kind == EntryKind::File)
        .map(RemoteEntryBaseline::from)
        .collect()
}

pub fn changed_paths_between(previous: &SyncSnapshot, current: &SyncSnapshot) -> Vec<String> {
    let previous_index = remote_snapshot_index(previous);
    let current_index = remote_snapshot_index(current);

    let mut all_paths: BTreeSet<String> = previous_index.keys().cloned().collect();
    all_paths.extend(current_index.keys().cloned());

    let mut changed_paths = Vec::new();
    for path in all_paths {
        if previous_index.get(&path) != current_index.get(&path) {
            changed_paths.push(path);
        }
    }
    changed_paths
}

type RemoteSnapshotIndex = BTreeMap<
    String,
    (
        EntryKind,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<u64>,
        Option<u64>,
    ),
>;

fn remote_snapshot_index(snapshot: &SyncSnapshot) -> RemoteSnapshotIndex {
    let mut index = BTreeMap::new();
    for entry in &snapshot.remote {
        index.insert(
            entry.path.clone(),
            (
                entry.kind,
                entry.version.clone(),
                entry.content_hash.clone(),
                entry.content_fingerprint.clone(),
                entry.size_bytes,
                entry.modified_at_unix,
            ),
        );
    }
    index
}

fn sleep_until_or_stop(duration: Duration, running: &AtomicBool) -> bool {
    if duration.is_zero() {
        return running.load(Ordering::SeqCst);
    }

    let mut remaining = duration;
    let step = duration.min(STOP_CHECK_INTERVAL);
    while remaining > Duration::ZERO {
        if !running.load(Ordering::SeqCst) {
            return false;
        }
        let nap = remaining.min(step);
        thread::sleep(nap);
        remaining = remaining.saturating_sub(nap);
    }
    running.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::changed_paths_between;
    use super::*;
    use axum::{Json, Router, routing::get};
    use common::ClusterId;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;
    use sync_core::{NamespaceEntry, SyncSnapshot};
    use transport_sdk::{BootstrapEndpoint, BootstrapEndpointUse, BootstrapTrustRoots, RelayMode};

    fn sample_bootstrap(base_url: &str) -> ConnectionBootstrap {
        ConnectionBootstrap {
            version: transport_sdk::CLIENT_BOOTSTRAP_VERSION,
            cluster_id: ClusterId::now_v7(),
            rendezvous_urls: vec![base_url.to_string()],
            rendezvous_contact_list: None,
            rendezvous_mtls_required: false,
            direct_endpoints: vec![BootstrapEndpoint {
                url: base_url.to_string(),
                usage: Some(BootstrapEndpointUse::PublicApi),
                node_id: None,
                node_hostname: None,
            }],
            relay_mode: RelayMode::Disabled,
            trust_roots: BootstrapTrustRoots {
                cluster_ca_pem: None,
                public_api_ca_pem: None,
                rendezvous_ca_pem: None,
            },
            pairing_token: None,
            device_label: None,
            device_id: None,
            node_priority_overrides: BTreeMap::new(),
        }
    }

    fn spawn_test_server(
        router: Router,
    ) -> (
        std::net::SocketAddr,
        tokio::sync::oneshot::Sender<()>,
        std::thread::JoinHandle<()>,
    ) {
        let (addr_tx, addr_rx) = std::sync::mpsc::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let server = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime should build");
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("listener should bind");
                let addr = listener.local_addr().expect("listener addr");
                addr_tx.send(addr).expect("listener addr should send");
                axum::serve(listener, router)
                    .with_graceful_shutdown(async {
                        let _ = shutdown_rx.await;
                    })
                    .await
                    .expect("test server should run");
            });
        });

        (
            addr_rx.recv().expect("listener addr should arrive"),
            shutdown_tx,
            server,
        )
    }

    #[test]
    fn changed_paths_between_detects_add_update_delete() {
        let previous = SyncSnapshot {
            local: Vec::new(),
            remote: vec![
                NamespaceEntry::directory("docs"),
                NamespaceEntry::file("docs/readme.md", "v1", "h1"),
                NamespaceEntry::file("docs/old.txt", "v1", "h-old"),
            ],
        };
        let current = SyncSnapshot {
            local: Vec::new(),
            remote: vec![
                NamespaceEntry::directory("docs"),
                NamespaceEntry::file("docs/readme.md", "v2", "h2"),
                NamespaceEntry::file("docs/new.txt", "v1", "h-new"),
            ],
        };

        let changed = changed_paths_between(&previous, &current);

        assert_eq!(
            changed,
            vec![
                "docs/new.txt".to_string(),
                "docs/old.txt".to_string(),
                "docs/readme.md".to_string(),
            ],
        );
    }

    fn deletion_for(baseline: RemoteEntryBaseline) -> RemoteDeletion {
        RemoteDeletion {
            path: baseline.path.clone(),
            tombstone_version: "tombstone-9".to_string(),
            tombstone_created_at_unix: 9,
            predecessors: vec![baseline],
        }
    }

    #[test]
    fn validated_tombstone_removes_only_its_proven_predecessor() {
        let mut entry = NamespaceEntry::file("photos/newly-uploaded.jpg", "revision-2", "hash-2");
        entry.modified_at_unix = Some(2);
        let previous = SyncSnapshot {
            local: Vec::new(),
            remote: vec![entry.clone()],
        };
        let mut current_snapshot = Some(previous);
        let mut updates = Vec::new();

        apply_snapshot_update(
            &mut current_snapshot,
            RemoteSnapshotUpdate {
                snapshot: SyncSnapshot {
                    local: Vec::new(),
                    remote: Vec::new(),
                },
                changed_paths: Vec::new(),
                deletions: vec![deletion_for(RemoteEntryBaseline::from(&entry))],
            },
            &mut |update| updates.push(update),
        );

        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].changed_paths, vec![entry.path.clone()]);
        assert_eq!(updates[0].deletions.len(), 1);
        assert!(
            current_snapshot
                .expect("validated deletion is retained")
                .remote
                .is_empty()
        );
    }

    #[test]
    fn bare_or_mismatched_missing_snapshot_preserves_the_prior_entry() {
        let mut entry = NamespaceEntry::file("photos/newly-uploaded.jpg", "revision-2", "hash-2");
        entry.modified_at_unix = Some(2);
        let previous = SyncSnapshot {
            local: Vec::new(),
            remote: vec![entry.clone()],
        };
        let mut current_snapshot = Some(previous.clone());
        let mut updates = Vec::new();

        apply_snapshot_update(
            &mut current_snapshot,
            RemoteSnapshotUpdate::snapshot_only(SyncSnapshot {
                local: Vec::new(),
                remote: Vec::new(),
            }),
            &mut |update| updates.push(update),
        );
        assert!(updates.is_empty());
        assert_eq!(current_snapshot, Some(previous.clone()));

        let mismatched = RemoteEntryBaseline {
            version: Some("revision-1".to_string()),
            ..RemoteEntryBaseline::from(&entry)
        };
        apply_snapshot_update(
            &mut current_snapshot,
            RemoteSnapshotUpdate {
                snapshot: SyncSnapshot {
                    local: Vec::new(),
                    remote: Vec::new(),
                },
                changed_paths: Vec::new(),
                deletions: vec![deletion_for(mismatched)],
            },
            &mut |update| updates.push(update),
        );
        assert!(updates.is_empty());
        assert_eq!(current_snapshot, Some(previous));
    }

    #[test]
    fn deletion_matching_requires_consistent_revision_or_legacy_hash_and_time() {
        let baseline = RemoteEntryBaseline {
            path: "docs/readme.md".to_string(),
            version: Some("revision-2".to_string()),
            content_hash: Some("hash-2".to_string()),
            modified_at_unix: Some(2),
        };
        let contradictory = RemoteEntryBaseline {
            content_hash: Some("other-hash".to_string()),
            ..baseline.clone()
        };
        assert!(!deletion_for(contradictory).matches_baseline(&baseline));

        let legacy = RemoteEntryBaseline {
            version: Some("server-head:size=2".to_string()),
            ..baseline.clone()
        };
        assert!(deletion_for(legacy.clone()).matches_baseline(&legacy));
        let no_timestamp = RemoteEntryBaseline {
            modified_at_unix: None,
            ..legacy.clone()
        };
        assert!(!deletion_for(no_timestamp.clone()).matches_baseline(&no_timestamp));
        assert!(!is_concrete_remote_revision(Some("server-head")));
        assert!(is_concrete_remote_revision(Some("revision-2")));
    }

    #[test]
    fn duplicate_snapshot_does_not_emit_a_second_update() {
        let snapshot = SyncSnapshot {
            local: Vec::new(),
            remote: vec![NamespaceEntry::file(
                "photos/holiday.jpg",
                "revision-2",
                "hash-2",
            )],
        };
        let mut current_snapshot = Some(snapshot.clone());
        let mut updates = Vec::new();

        apply_snapshot_update(
            &mut current_snapshot,
            RemoteSnapshotUpdate::snapshot_only(snapshot.clone()),
            &mut |update| updates.push(update),
        );

        assert!(updates.is_empty());
        assert_eq!(current_snapshot, Some(snapshot));
    }

    #[test]
    fn changed_paths_between_detects_content_fingerprint_updates() {
        let mut previous_entry =
            NamespaceEntry::file_sized("docs/readme.txt", "v1", "h1", Some(42));
        previous_entry.content_fingerprint = None;
        let mut current_entry = NamespaceEntry::file_sized("docs/readme.txt", "v1", "h1", Some(42));
        current_entry.content_fingerprint = Some("cfp-readme".to_string());

        let previous = SyncSnapshot {
            local: Vec::new(),
            remote: vec![previous_entry],
        };
        let current = SyncSnapshot {
            local: Vec::new(),
            remote: vec![current_entry],
        };

        assert_eq!(
            changed_paths_between(&previous, &current),
            vec!["docs/readme.txt".to_string()]
        );
    }

    #[test]
    fn changed_paths_between_detects_remote_size_only_changes() {
        let previous = SyncSnapshot {
            local: Vec::new(),
            remote: vec![NamespaceEntry::file_sized(
                "docs/readme.md",
                "v1",
                "h1",
                Some(12),
            )],
        };
        let current = SyncSnapshot {
            local: Vec::new(),
            remote: vec![NamespaceEntry::file_sized(
                "docs/readme.md",
                "v1",
                "h1",
                Some(24),
            )],
        };

        let changed = changed_paths_between(&previous, &current);

        assert_eq!(changed, vec!["docs/readme.md".to_string()]);
    }

    #[test]
    fn remote_sync_strategy_polling_enforces_minimum_interval() {
        let strategy = RemoteSyncStrategy::polling(Duration::from_millis(10));

        match strategy.mode {
            RemoteSyncMode::Polling { interval } => {
                assert_eq!(interval, Duration::from_millis(250));
            }
            RemoteSyncMode::ServerNotifications { .. } => {
                panic!("polling strategy should stay in polling mode");
            }
        }

        let scheduler = RemoteSyncScheduler::new(strategy);
        assert_eq!(scheduler.fallback_interval(), Duration::from_millis(250));
    }

    #[test]
    fn remote_sync_strategy_server_notifications_enforces_minimum_intervals() {
        let strategy =
            RemoteSyncStrategy::server_notifications(Duration::from_millis(1), Duration::ZERO);

        match strategy.mode {
            RemoteSyncMode::Polling { .. } => {
                panic!("server notification strategy should not switch to polling mode");
            }
            RemoteSyncMode::ServerNotifications {
                wait_timeout,
                retry_interval,
            } => {
                assert_eq!(wait_timeout, Duration::from_millis(250));
                assert_eq!(retry_interval, Duration::from_millis(250));
            }
        }

        let scheduler = RemoteSyncScheduler::new(strategy);
        assert_eq!(scheduler.fallback_interval(), Duration::from_millis(250));
    }

    #[test]
    fn remote_sync_strategy_default_uses_three_second_polling() {
        match RemoteSyncStrategy::default().mode {
            RemoteSyncMode::Polling { interval } => {
                assert_eq!(interval, Duration::from_secs(3));
            }
            RemoteSyncMode::ServerNotifications { .. } => {
                panic!("default strategy should poll");
            }
        }
    }

    #[test]
    fn remote_snapshot_poller_prefers_server_notifications_with_reasonable_wait_timeout() {
        let fast_retry = RemoteSnapshotPoller::prefer_server_notifications(Duration::from_secs(1));
        match fast_retry.scheduler.strategy.mode {
            RemoteSyncMode::Polling { .. } => {
                panic!("preferred notification mode should not switch to polling");
            }
            RemoteSyncMode::ServerNotifications {
                wait_timeout,
                retry_interval,
            } => {
                assert_eq!(wait_timeout, Duration::from_secs(2));
                assert_eq!(retry_interval, Duration::from_secs(1));
            }
        }

        let slow_retry = RemoteSnapshotPoller::prefer_server_notifications(Duration::from_secs(5));
        match slow_retry.scheduler.strategy.mode {
            RemoteSyncMode::Polling { .. } => {
                panic!("preferred notification mode should not switch to polling");
            }
            RemoteSyncMode::ServerNotifications {
                wait_timeout,
                retry_interval,
            } => {
                assert_eq!(wait_timeout, Duration::from_secs(5));
                assert_eq!(retry_interval, Duration::from_secs(5));
            }
        }
    }

    #[test]
    fn remote_snapshot_scope_and_direct_fetcher_normalize_defaults() {
        let fetcher = RemoteSnapshotFetcher::from_direct_base_url(
            "http://127.0.0.1:1",
            Some("docs".to_string()),
            0,
            Some("snap-1".to_string()),
        );

        assert_eq!(fetcher.scope.prefix.as_deref(), Some("docs"));
        assert_eq!(fetcher.scope.depth, 1);
        assert_eq!(fetcher.scope.snapshot.as_deref(), Some("snap-1"));
    }

    #[test]
    fn sleep_until_or_stop_respects_zero_duration_and_stopped_state() {
        let running = AtomicBool::new(true);
        assert!(sleep_until_or_stop(Duration::ZERO, &running));

        running.store(false, Ordering::SeqCst);
        assert!(!sleep_until_or_stop(Duration::from_millis(1), &running));
    }

    #[test]
    fn polling_loop_emits_changed_paths_updates() {
        let poller = RemoteSnapshotPoller::polling(Duration::from_millis(1));
        let running = Arc::new(AtomicBool::new(true));
        let initial_snapshot = SyncSnapshot {
            local: Vec::new(),
            remote: vec![NamespaceEntry::file("docs/readme.md", "v1", "h1")],
        };
        let next_snapshot = SyncSnapshot {
            local: Vec::new(),
            remote: vec![
                NamespaceEntry::file("docs/readme.md", "v2", "h2"),
                NamespaceEntry::file("docs/new.txt", "v1", "h-new"),
            ],
        };
        let (tx, rx) = mpsc::channel();
        let running_for_callback = Arc::clone(&running);

        let handle = poller.spawn_changed_paths_loop(
            Arc::clone(&running),
            Some(initial_snapshot),
            move || Ok(next_snapshot.clone()),
            move |update| {
                running_for_callback.store(false, Ordering::SeqCst);
                tx.send(update).expect("update should send");
            },
        );

        let update = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("changed snapshot should produce an update");
        handle.join().expect("polling loop should stop cleanly");

        assert_eq!(
            update.changed_paths,
            vec!["docs/new.txt".to_string(), "docs/readme.md".to_string(),],
        );
        assert_eq!(update.snapshot.remote.len(), 2);
    }

    #[test]
    fn server_notification_loop_skips_fetch_without_remote_changes() {
        let router = Router::new().route(
            "/api/v1/store/index/changes/wait",
            get(|| async { Json(serde_json::json!({ "sequence": 0, "changed": false })) }),
        );
        let (addr, shutdown_tx, server) = spawn_test_server(router);

        let poller =
            RemoteSnapshotPoller::server_notifications(Duration::from_millis(250), Duration::ZERO);
        let running = Arc::new(AtomicBool::new(true));
        let fetcher =
            RemoteSnapshotFetcher::from_direct_base_url(format!("http://{addr}"), None, 1, None);
        let fetch_count = Arc::new(AtomicUsize::new(0));
        let fetch_count_for_loop = Arc::clone(&fetch_count);

        let handle = poller.spawn_fetcher_loop_with_fetch(
            Arc::clone(&running),
            Some(SyncSnapshot {
                local: Vec::new(),
                remote: vec![NamespaceEntry::file("docs/readme.md", "v1", "h1")],
            }),
            fetcher,
            move |_fetcher| {
                fetch_count_for_loop.fetch_add(1, Ordering::SeqCst);
                Ok(SyncSnapshot {
                    local: Vec::new(),
                    remote: vec![NamespaceEntry::file("docs/readme.md", "v1", "h1")],
                })
            },
            |_update| panic!("unchanged wait response should not emit an update"),
        );

        std::thread::sleep(Duration::from_millis(350));
        running.store(false, Ordering::SeqCst);
        handle
            .join()
            .expect("notification loop should stop cleanly");

        assert_eq!(fetch_count.load(Ordering::SeqCst), 0);

        let _ = shutdown_tx.send(());
        let _ = server.join();
    }

    #[test]
    fn server_notification_loop_falls_back_to_polling_when_wait_is_unavailable() {
        let (addr, shutdown_tx, server) = spawn_test_server(Router::new());

        let poller =
            RemoteSnapshotPoller::server_notifications(Duration::from_millis(250), Duration::ZERO);
        let running = Arc::new(AtomicBool::new(true));
        let fetcher =
            RemoteSnapshotFetcher::from_direct_base_url(format!("http://{addr}"), None, 1, None);
        let next_snapshot = SyncSnapshot {
            local: Vec::new(),
            remote: vec![
                NamespaceEntry::file("docs/readme.md", "v2", "h2"),
                NamespaceEntry::file("docs/new.txt", "v1", "h-new"),
            ],
        };
        let fetch_count = Arc::new(AtomicUsize::new(0));
        let fetch_count_for_loop = Arc::clone(&fetch_count);
        let (tx, rx) = mpsc::channel();
        let running_for_callback = Arc::clone(&running);

        let handle = poller.spawn_fetcher_loop_with_fetch(
            Arc::clone(&running),
            Some(SyncSnapshot {
                local: Vec::new(),
                remote: vec![NamespaceEntry::file("docs/readme.md", "v1", "h1")],
            }),
            fetcher,
            move |_fetcher| {
                fetch_count_for_loop.fetch_add(1, Ordering::SeqCst);
                Ok(next_snapshot.clone())
            },
            move |update| {
                running_for_callback.store(false, Ordering::SeqCst);
                tx.send(update).expect("update should send");
            },
        );

        let update = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("polling fallback should produce an update");
        handle
            .join()
            .expect("notification loop should stop cleanly");

        assert_eq!(fetch_count.load(Ordering::SeqCst), 1);
        assert_eq!(
            update.changed_paths,
            vec!["docs/new.txt".to_string(), "docs/readme.md".to_string()],
        );

        let _ = shutdown_tx.send(());
        let _ = server.join();
    }

    #[test]
    fn server_notification_loop_retries_transient_wait_errors_before_falling_back() {
        use axum::http::StatusCode;
        use axum::response::IntoResponse;

        let wait_attempts = Arc::new(AtomicUsize::new(0));
        let wait_attempts_for_route = Arc::clone(&wait_attempts);
        let router = Router::new().route(
            "/api/v1/store/index/changes/wait",
            get(move || {
                let wait_attempts = Arc::clone(&wait_attempts_for_route);
                async move {
                    let attempt = wait_attempts.fetch_add(1, Ordering::SeqCst);
                    if attempt == 0 {
                        StatusCode::SERVICE_UNAVAILABLE.into_response()
                    } else {
                        Json(serde_json::json!({ "sequence": 1, "changed": true })).into_response()
                    }
                }
            }),
        );
        let (addr, shutdown_tx, server) = spawn_test_server(router);

        let poller =
            RemoteSnapshotPoller::server_notifications(Duration::from_millis(250), Duration::ZERO);
        let running = Arc::new(AtomicBool::new(true));
        let fetcher =
            RemoteSnapshotFetcher::from_direct_base_url(format!("http://{addr}"), None, 1, None);
        let fetch_count = Arc::new(AtomicUsize::new(0));
        let fetch_count_for_loop = Arc::clone(&fetch_count);
        let (tx, rx) = mpsc::channel();
        let running_for_callback = Arc::clone(&running);

        let handle = poller.spawn_fetcher_loop_with_fetch(
            Arc::clone(&running),
            Some(SyncSnapshot {
                local: Vec::new(),
                remote: vec![NamespaceEntry::file("docs/readme.md", "v1", "h1")],
            }),
            fetcher,
            move |_fetcher| {
                fetch_count_for_loop.fetch_add(1, Ordering::SeqCst);
                Ok(SyncSnapshot {
                    local: Vec::new(),
                    remote: vec![NamespaceEntry::file("docs/readme.md", "v2", "h2")],
                })
            },
            move |update| {
                running_for_callback.store(false, Ordering::SeqCst);
                tx.send(update).expect("update should send");
            },
        );

        let update = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("notification retry should produce an update");
        handle
            .join()
            .expect("notification loop should stop cleanly");

        assert_eq!(fetch_count.load(Ordering::SeqCst), 1);
        assert_eq!(wait_attempts.load(Ordering::SeqCst), 1);
        assert_eq!(update.changed_paths, vec!["docs/readme.md".to_string()]);

        let _ = shutdown_tx.send(());
        let _ = server.join();
    }

    #[test]
    fn server_notification_loop_polls_snapshot_after_retryable_wait_errors() {
        use axum::http::StatusCode;
        use axum::response::IntoResponse;

        let wait_attempts = Arc::new(AtomicUsize::new(0));
        let wait_attempts_for_route = Arc::clone(&wait_attempts);
        let router = Router::new().route(
            "/api/v1/store/index/changes/wait",
            get(move || {
                let wait_attempts = Arc::clone(&wait_attempts_for_route);
                async move {
                    wait_attempts.fetch_add(1, Ordering::SeqCst);
                    StatusCode::SERVICE_UNAVAILABLE.into_response()
                }
            }),
        );
        let (addr, shutdown_tx, server) = spawn_test_server(router);

        let poller =
            RemoteSnapshotPoller::server_notifications(Duration::from_millis(250), Duration::ZERO);
        let running = Arc::new(AtomicBool::new(true));
        let fetcher =
            RemoteSnapshotFetcher::from_direct_base_url(format!("http://{addr}"), None, 1, None);
        let fetch_count = Arc::new(AtomicUsize::new(0));
        let fetch_count_for_loop = Arc::clone(&fetch_count);
        let (tx, rx) = mpsc::channel();
        let running_for_callback = Arc::clone(&running);

        let handle = poller.spawn_fetcher_loop_with_fetch(
            Arc::clone(&running),
            Some(SyncSnapshot {
                local: Vec::new(),
                remote: vec![NamespaceEntry::file("docs/readme.md", "v1", "h1")],
            }),
            fetcher,
            move |_fetcher| {
                fetch_count_for_loop.fetch_add(1, Ordering::SeqCst);
                Ok(SyncSnapshot {
                    local: Vec::new(),
                    remote: vec![NamespaceEntry::file("docs/readme.md", "v2", "h2")],
                })
            },
            move |update| {
                running_for_callback.store(false, Ordering::SeqCst);
                tx.send(update).expect("update should send");
            },
        );

        let update = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("retryable wait failures should still trigger a snapshot poll");
        handle
            .join()
            .expect("notification loop should stop cleanly");

        assert_eq!(fetch_count.load(Ordering::SeqCst), 1);
        assert!(wait_attempts.load(Ordering::SeqCst) >= 1);
        assert_eq!(update.changed_paths, vec!["docs/readme.md".to_string()]);

        let _ = shutdown_tx.send(());
        let _ = server.join();
    }

    #[test]
    fn server_notification_loop_rebases_after_sequence_regression() {
        let wait_attempts = Arc::new(AtomicUsize::new(0));
        let wait_attempts_for_route = Arc::clone(&wait_attempts);
        let router = Router::new().route(
            "/api/v1/store/index/changes/wait",
            get(move || {
                let wait_attempts = Arc::clone(&wait_attempts_for_route);
                async move {
                    let attempt = wait_attempts.fetch_add(1, Ordering::SeqCst);
                    if attempt == 0 {
                        Json(serde_json::json!({ "sequence": 5, "changed": true }))
                    } else {
                        Json(serde_json::json!({ "sequence": 0, "changed": false }))
                    }
                }
            }),
        );
        let (addr, shutdown_tx, server) = spawn_test_server(router);

        let poller =
            RemoteSnapshotPoller::server_notifications(Duration::from_millis(250), Duration::ZERO);
        let running = Arc::new(AtomicBool::new(true));
        let fetcher =
            RemoteSnapshotFetcher::from_direct_base_url(format!("http://{addr}"), None, 1, None);
        let fetch_attempts = Arc::new(AtomicUsize::new(0));
        let fetch_attempts_for_loop = Arc::clone(&fetch_attempts);
        let (tx, rx) = mpsc::channel();
        let running_for_callback = Arc::clone(&running);

        let handle = poller.spawn_fetcher_loop_with_fetch(
            Arc::clone(&running),
            Some(SyncSnapshot {
                local: Vec::new(),
                remote: vec![NamespaceEntry::file("docs/readme.md", "v1", "h1")],
            }),
            fetcher,
            move |_fetcher| {
                let attempt = fetch_attempts_for_loop.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    Ok(SyncSnapshot {
                        local: Vec::new(),
                        remote: vec![NamespaceEntry::file("docs/readme.md", "v1", "h1")],
                    })
                } else {
                    Ok(SyncSnapshot {
                        local: Vec::new(),
                        remote: vec![NamespaceEntry::file("docs/readme.md", "v2", "h2")],
                    })
                }
            },
            move |update| {
                running_for_callback.store(false, Ordering::SeqCst);
                tx.send(update).expect("update should send");
            },
        );

        let update = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("sequence regression should trigger a rebaseline snapshot fetch");
        handle
            .join()
            .expect("notification loop should stop cleanly");

        assert_eq!(wait_attempts.load(Ordering::SeqCst), 2);
        assert_eq!(fetch_attempts.load(Ordering::SeqCst), 2);
        assert_eq!(update.changed_paths, vec!["docs/readme.md".to_string()]);

        let _ = shutdown_tx.send(());
        let _ = server.join();
    }

    fn run_sequence_regression_scenario(
        second_update_timeout: Duration,
    ) -> (
        Result<RemoteSnapshotUpdate, mpsc::RecvTimeoutError>,
        Result<RemoteSnapshotUpdate, mpsc::RecvTimeoutError>,
        usize,
        usize,
    ) {
        let wait_attempts = Arc::new(AtomicUsize::new(0));
        let wait_attempts_for_route = Arc::clone(&wait_attempts);
        let router = Router::new().route(
            "/api/v1/store/index/changes/wait",
            get(move || {
                let wait_attempts = Arc::clone(&wait_attempts_for_route);
                async move {
                    let attempt = wait_attempts.fetch_add(1, Ordering::SeqCst);
                    if attempt == 0 {
                        Json(serde_json::json!({ "sequence": 5, "changed": true }))
                    } else {
                        Json(serde_json::json!({ "sequence": 0, "changed": false }))
                    }
                }
            }),
        );
        let (addr, shutdown_tx, server) = spawn_test_server(router);

        let poller =
            RemoteSnapshotPoller::server_notifications(Duration::from_millis(250), Duration::ZERO);
        let running = Arc::new(AtomicBool::new(true));
        let fetcher =
            RemoteSnapshotFetcher::from_direct_base_url(format!("http://{addr}"), None, 1, None);
        let fetch_attempts = Arc::new(AtomicUsize::new(0));
        let fetch_attempts_for_loop = Arc::clone(&fetch_attempts);
        let update_count = Arc::new(AtomicUsize::new(0));
        let update_count_for_callback = Arc::clone(&update_count);
        let (tx, rx) = mpsc::channel();
        let running_for_callback = Arc::clone(&running);

        let handle = poller.spawn_fetcher_loop_with_fetch(
            Arc::clone(&running),
            Some(SyncSnapshot {
                local: Vec::new(),
                remote: vec![NamespaceEntry::file(
                    "photos/holiday.jpg",
                    "revision-1",
                    "hash-1",
                )],
            }),
            fetcher,
            move |_fetcher| {
                let attempt = fetch_attempts_for_loop.fetch_add(1, Ordering::SeqCst);
                Ok(if attempt == 0 {
                    SyncSnapshot {
                        local: Vec::new(),
                        remote: vec![NamespaceEntry::file(
                            "photos/holiday.jpg",
                            "revision-2",
                            "hash-2",
                        )],
                    }
                } else {
                    SyncSnapshot {
                        local: Vec::new(),
                        remote: Vec::new(),
                    }
                })
            },
            move |update| {
                let count = update_count_for_callback.fetch_add(1, Ordering::SeqCst) + 1;
                tx.send(update).expect("update should send");
                if count == 2 {
                    running_for_callback.store(false, Ordering::SeqCst);
                }
            },
        );

        let first_update = rx.recv_timeout(Duration::from_secs(2));
        let second_update = rx.recv_timeout(second_update_timeout);
        running.store(false, Ordering::SeqCst);
        handle
            .join()
            .expect("notification loop should stop cleanly");

        let _ = shutdown_tx.send(());
        let _ = server.join();

        (
            first_update,
            second_update,
            wait_attempts.load(Ordering::SeqCst),
            fetch_attempts.load(Ordering::SeqCst),
        )
    }

    #[test]
    fn desired_behavior_sequence_regression_does_not_emit_a_stale_file_removal() {
        let (first_update, unexpected_removal, _, _) =
            run_sequence_regression_scenario(Duration::from_secs(2));

        let first_update = first_update.expect("first refresh should update the file revision");
        assert_eq!(
            first_update.changed_paths,
            vec!["photos/holiday.jpg".to_string()]
        );
        assert!(
            unexpected_removal.is_err(),
            "a sequence regression must not emit a removal from an older snapshot"
        );
    }

    #[test]
    fn server_notification_loop_retries_changed_snapshot_after_fetch_failure() {
        use axum::extract::Query;

        let router = Router::new().route(
            "/api/v1/store/index/changes/wait",
            get(
                |Query(params): Query<std::collections::HashMap<String, String>>| async move {
                    let since = params
                        .get("since")
                        .and_then(|value| value.parse::<u64>().ok())
                        .unwrap_or_default();
                    let changed = since < 1;
                    Json(serde_json::json!({ "sequence": 1, "changed": changed }))
                },
            ),
        );
        let (addr, shutdown_tx, server) = spawn_test_server(router);

        let poller =
            RemoteSnapshotPoller::server_notifications(Duration::from_millis(250), Duration::ZERO);
        let running = Arc::new(AtomicBool::new(true));
        let fetcher =
            RemoteSnapshotFetcher::from_direct_base_url(format!("http://{addr}"), None, 1, None);
        let fetch_attempts = Arc::new(AtomicUsize::new(0));
        let fetch_attempts_for_loop = Arc::clone(&fetch_attempts);
        let (tx, rx) = mpsc::channel();
        let running_for_callback = Arc::clone(&running);

        let handle = poller.spawn_fetcher_loop_with_fetch(
            Arc::clone(&running),
            Some(SyncSnapshot {
                local: Vec::new(),
                remote: vec![NamespaceEntry::file("docs/readme.md", "v1", "h1")],
            }),
            fetcher,
            move |_fetcher| {
                let attempt = fetch_attempts_for_loop.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    anyhow::bail!("simulated snapshot refresh failure");
                }
                Ok(SyncSnapshot {
                    local: Vec::new(),
                    remote: vec![NamespaceEntry::file("docs/readme.md", "v2", "h2")],
                })
            },
            move |update| {
                running_for_callback.store(false, Ordering::SeqCst);
                tx.send(update).expect("update should send");
            },
        );

        let update = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("changed wait should be retried after a fetch failure");
        handle
            .join()
            .expect("notification loop should stop cleanly");

        assert_eq!(fetch_attempts.load(Ordering::SeqCst), 2);
        assert_eq!(update.changed_paths, vec!["docs/readme.md".to_string()]);

        let _ = shutdown_tx.send(());
        let _ = server.join();
    }

    #[test]
    fn remote_snapshot_fetcher_from_bootstrap_builds_transport_aware_client() {
        async fn health() -> Json<serde_json::Value> {
            Json(serde_json::json!({ "ok": true }))
        }

        async fn store_index() -> Json<crate::ironmesh_client::StoreIndexResponse> {
            Json(crate::ironmesh_client::StoreIndexResponse {
                prefix: String::new(),
                depth: 1,
                entry_count: 0,
                total_entry_count: 0,
                offset: 0,
                limit: None,
                has_more: false,
                next_cursor: None,
                sync_token: None,
                consistency_token: None,
                media_summary: crate::ironmesh_client::StoreIndexMediaSummary::default(),
                entries: Vec::new(),
            })
        }

        let router = Router::new()
            .route("/api/v1/health", get(health))
            .route("/api/v1/store/index", get(store_index));
        let (addr_tx, addr_rx) = std::sync::mpsc::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let server = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime should build");
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("listener should bind");
                let addr = listener.local_addr().expect("listener addr");
                addr_tx.send(addr).expect("listener addr should send");
                axum::serve(listener, router)
                    .with_graceful_shutdown(async {
                        let _ = shutdown_rx.await;
                    })
                    .await
                    .expect("test server should run");
            });
        });
        let addr = addr_rx.recv().expect("listener addr should arrive");

        let bootstrap = sample_bootstrap(&format!("http://{addr}"));
        let fetcher = RemoteSnapshotFetcher::from_bootstrap(&bootstrap, None, None, 1, None)
            .expect("bootstrap-backed fetcher should build");

        let snapshot = fetcher
            .fetch_snapshot_blocking()
            .expect("bootstrap-backed fetcher should load snapshot");

        assert!(snapshot.remote.is_empty());

        let _ = shutdown_tx.send(());
        let _ = server.join();
    }

    #[test]
    fn remote_snapshot_fetcher_uses_store_index_sizes_without_per_file_heads() {
        async fn health() -> Json<serde_json::Value> {
            Json(serde_json::json!({ "ok": true }))
        }

        async fn store_index() -> Json<crate::ironmesh_client::StoreIndexResponse> {
            Json(crate::ironmesh_client::StoreIndexResponse {
                prefix: String::new(),
                depth: 1,
                entry_count: 1,
                total_entry_count: 1,
                offset: 0,
                limit: None,
                has_more: false,
                next_cursor: None,
                sync_token: None,
                consistency_token: None,
                media_summary: crate::ironmesh_client::StoreIndexMediaSummary::default(),
                entries: vec![crate::ironmesh_client::StoreIndexEntry {
                    path: "docs/readme.txt".to_string(),
                    entry_type: "key".to_string(),
                    labels: Vec::new(),
                    labels_resolved: false,
                    version: Some("v1".to_string()),
                    content_hash: Some("hash-1".to_string()),
                    size_bytes: Some(42),
                    modified_at_unix: None,
                    content_fingerprint: None,
                    media: None,
                }],
            })
        }

        let router = Router::new()
            .route("/api/v1/health", get(health))
            .route("/api/v1/store/index", get(store_index));
        let (addr_tx, addr_rx) = std::sync::mpsc::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let server = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime should build");
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("listener should bind");
                let addr = listener.local_addr().expect("listener addr");
                addr_tx.send(addr).expect("listener addr should send");
                axum::serve(listener, router)
                    .with_graceful_shutdown(async {
                        let _ = shutdown_rx.await;
                    })
                    .await
                    .expect("test server should run");
            });
        });
        let addr = addr_rx.recv().expect("listener addr should arrive");

        let bootstrap = sample_bootstrap(&format!("http://{addr}"));
        let fetcher = RemoteSnapshotFetcher::from_bootstrap(&bootstrap, None, None, 1, None)
            .expect("bootstrap-backed fetcher should build");

        let snapshot = fetcher
            .fetch_snapshot_blocking()
            .expect("snapshot fetch should use store index metadata only");

        assert_eq!(
            snapshot.remote,
            vec![
                NamespaceEntry::directory("docs"),
                NamespaceEntry::file_sized("docs/readme.txt", "v1", "hash-1", Some(42)),
            ]
        );

        let _ = shutdown_tx.send(());
        let _ = server.join();
    }
}
