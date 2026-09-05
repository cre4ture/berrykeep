#![cfg(windows)]

use crate::auth::is_internal_client_identity_relative_path;
use crate::cfapi::{
    cf_ensure_placeholder_identity, cf_get_placeholder_standard_info_with_identity, cf_set_in_sync,
    cf_update_placeholder_file_identity, cf_update_placeholder_file_identity_with_oplock,
    cf_update_placeholder_metadata_and_identity,
    cf_update_placeholder_metadata_and_identity_with_oplock, open_sync_path, path_is_placeholder,
};
use crate::connection_config::is_internal_connection_bootstrap_relative_path;
use crate::content_fingerprint::file_content_fingerprint;
use crate::helpers::{
    PlaceholderFileIdentity, decode_placeholder_file_identity, normalize_path, path_to_relative,
    unix_seconds_to_windows_file_time,
};
use crate::snapshot_cache::is_internal_remote_snapshot_relative_path;
use anyhow::{Context, Result};
use client_sdk::remote_sync::RemoteObjectChange;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::Path;
use sync_core::{NamespaceMediaMetadata, SyncSnapshot};
use uuid::Uuid;
use walkdir::WalkDir;
use windows_sys::Win32::Storage::CloudFilters::{CF_FS_METADATA, CF_IN_SYNC_STATE_IN_SYNC};
use windows_sys::Win32::Storage::FileSystem::FILE_BASIC_INFO;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RemoteObjectReconcileReport {
    pub deleted_paths: BTreeSet<String>,
    pub renamed_paths: BTreeMap<String, String>,
    pub migrated_paths: BTreeSet<String>,
    pub conflicted_paths: BTreeSet<String>,
    pub preserved_paths: BTreeSet<String>,
    pub suppressed_startup_paths: BTreeSet<String>,
    /// Stable object IDs observed before this reconciliation pass. Runtime
    /// action application reuses this inventory instead of opening every
    /// placeholder again solely to check for object-ID collisions.
    pub observed_placeholder_paths_by_object_id: BTreeMap<String, BTreeSet<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRemoteObject {
    pub object_id: String,
    pub path: String,
    pub revision: Option<String>,
}

pub trait RemoteObjectResolver: Send + Sync {
    fn resolve_object(&self, object_id: &str) -> Result<Option<ResolvedRemoteObject>>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteFileMetadataPolicy {
    ApplyAndMarkInSync,
    ApplyAfterConfirmedMove,
    PreserveLocalConflict,
}

#[derive(Debug, Clone, Copy)]
pub struct RemotePlaceholderState<'a> {
    pub object_id: Option<&'a str>,
    pub remote_version: Option<&'a str>,
    pub remote_content_hash: Option<&'a str>,
    pub remote_size_bytes: Option<u64>,
    pub remote_content_fingerprint: Option<&'a str>,
    pub remote_modified_at_unix: Option<u64>,
    pub remote_media: Option<&'a NamespaceMediaMetadata>,
}

pub fn record_in_sync_local_file_state(
    sync_root_path: &Path,
    relative_path: &str,
    provider_instance_id: Uuid,
) -> Result<()> {
    let normalized = normalize_path(relative_path);
    if normalized.is_empty() || is_internal_sync_root_relative_path(&normalized) {
        return Ok(());
    }

    let full_path = sync_root_path.join(normalized.replace('/', "\\"));
    let metadata = fs::metadata(&full_path)
        .with_context(|| format!("failed to inspect {}", full_path.display()))?;
    if metadata.is_dir() {
        return Ok(());
    }
    let local_content_fingerprint = file_content_fingerprint(&full_path)?;
    record_in_sync_content_baseline(
        sync_root_path,
        &normalized,
        provider_instance_id,
        &local_content_fingerprint,
    )
}

pub fn record_in_sync_content_baseline(
    sync_root_path: &Path,
    relative_path: &str,
    provider_instance_id: Uuid,
    in_sync_content_fingerprint: &str,
) -> Result<()> {
    let normalized = normalize_path(relative_path);
    if normalized.is_empty() || is_internal_sync_root_relative_path(&normalized) {
        return Ok(());
    }

    mutate_placeholder_identity_for_path(sync_root_path, &normalized, None, true, |identity| {
        identity.path = normalized.clone();
        identity.provider_instance_id = Some(provider_instance_id);
        identity.set_in_sync_content_baseline(in_sync_content_fingerprint);
    })
}

pub fn promote_remote_to_in_sync_content_baseline(
    sync_root_path: &Path,
    relative_path: &str,
    provider_instance_id: Uuid,
) -> Result<()> {
    let normalized = normalize_path(relative_path);
    if normalized.is_empty() || is_internal_sync_root_relative_path(&normalized) {
        return Ok(());
    }

    mutate_placeholder_identity_for_path(sync_root_path, &normalized, None, true, |identity| {
        identity.path = normalized.clone();
        identity.provider_instance_id = Some(provider_instance_id);
        identity.promote_remote_to_in_sync_content_baseline();
    })
}

pub fn refresh_remote_placeholder_state(
    sync_root_path: &Path,
    relative_path: &str,
    provider_instance_id: Uuid,
    remote: RemotePlaceholderState<'_>,
) -> Result<()> {
    refresh_remote_placeholder_state_with_policy(
        sync_root_path,
        relative_path,
        provider_instance_id,
        remote,
        RemoteFileMetadataPolicy::ApplyAndMarkInSync,
    )
}

/// Bind the first remote baseline to a placeholder that was just created by a
/// remote action.  Callers must not use this for a pre-existing path: the
/// relaxed dirty-state check is safe only before a user could have changed the
/// local object.
pub fn initialize_new_remote_placeholder_state(
    sync_root_path: &Path,
    relative_path: &str,
    provider_instance_id: Uuid,
    remote: RemotePlaceholderState<'_>,
) -> Result<()> {
    refresh_remote_placeholder_state_with_policy(
        sync_root_path,
        relative_path,
        provider_instance_id,
        remote,
        RemoteFileMetadataPolicy::ApplyAfterConfirmedMove,
    )
}

pub fn refresh_remote_conflict_identity(
    sync_root_path: &Path,
    relative_path: &str,
    provider_instance_id: Uuid,
    remote: RemotePlaceholderState<'_>,
) -> Result<()> {
    refresh_remote_placeholder_state_with_policy(
        sync_root_path,
        relative_path,
        provider_instance_id,
        remote,
        RemoteFileMetadataPolicy::PreserveLocalConflict,
    )
}

fn refresh_remote_placeholder_state_with_policy(
    sync_root_path: &Path,
    relative_path: &str,
    provider_instance_id: Uuid,
    remote: RemotePlaceholderState<'_>,
    file_metadata_policy: RemoteFileMetadataPolicy,
) -> Result<()> {
    let RemotePlaceholderState {
        object_id,
        remote_version,
        remote_content_hash,
        remote_size_bytes,
        remote_content_fingerprint,
        remote_modified_at_unix,
        remote_media,
    } = remote;
    let normalized = normalize_path(relative_path);
    if normalized.is_empty() || is_internal_sync_root_relative_path(&normalized) {
        return Ok(());
    }

    let full_path = sync_root_path.join(normalized.replace('/', "\\"));
    let metadata = match fs::metadata(&full_path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(err).with_context(|| format!("failed to inspect {}", full_path.display()));
        }
    };

    let existing_placeholder = open_sync_path(&full_path, false)
        .ok()
        .and_then(|file| cf_get_placeholder_standard_info_with_identity(&file).ok())
        .and_then(|info| {
            let modified_data_size = info.info().ModifiedDataSize;
            let in_sync_state = info.info().InSyncState;
            decode_placeholder_file_identity(info.file_identity())
                .map(|identity| (identity, modified_data_size, in_sync_state))
        });
    let normalized_object_id = object_id.map(str::trim).filter(|value| !value.is_empty());
    if let Some((existing_identity, modified_data_size, in_sync_state)) =
        existing_placeholder.as_ref()
    {
        if let (Some(expected), Some(actual)) =
            (normalized_object_id, existing_identity.object_id.as_deref())
            && actual != expected
        {
            anyhow::bail!(
                "object identity conflict at {normalized}: local object_id={actual} remote object_id={expected}"
            );
        }
        if normalized_object_id.is_some()
            && existing_identity.object_id.is_none()
            && identity_has_remote_baseline(existing_identity)
            && !legacy_identity_matches_remote_baseline(
                existing_identity,
                remote_version,
                remote_content_hash,
                remote_content_fingerprint,
            )
        {
            anyhow::bail!(
                "legacy placeholder at {normalized} cannot be safely bound to this remote baseline"
            );
        }
        let local_state_is_dirty = file_metadata_policy
            != RemoteFileMetadataPolicy::ApplyAfterConfirmedMove
            && (*modified_data_size != 0
                || *in_sync_state != CF_IN_SYNC_STATE_IN_SYNC
                || normalize_path(&existing_identity.path) != normalized);
        if local_state_is_dirty
            && file_metadata_policy != RemoteFileMetadataPolicy::PreserveLocalConflict
        {
            if existing_identity.object_id.as_deref() != normalized_object_id
                || existing_identity.remote_version.as_deref() != remote_version
            {
                anyhow::bail!(
                    "local and remote changes conflict at {normalized}; preserving local dirty data"
                );
            }
            return Ok(());
        }
    } else if normalized_object_id.is_some() {
        anyhow::bail!(
            "existing file at {normalized} has no verifiable placeholder identity; preserving it"
        );
    }

    let file_size = remote_size_bytes
        .and_then(|value| i64::try_from(value).ok())
        .or_else(|| i64::try_from(metadata.len()).ok())
        .unwrap_or_default();
    let fs_metadata = (!metadata.is_dir())
        .then(|| {
            remote_file_system_metadata(file_metadata_policy, remote_modified_at_unix, file_size)
        })
        .flatten();
    let fs_metadata_is_current = match file_metadata_policy {
        RemoteFileMetadataPolicy::ApplyAndMarkInSync
        | RemoteFileMetadataPolicy::ApplyAfterConfirmedMove => {
            metadata.is_dir()
                || remote_file_system_metadata_is_current(
                    &metadata,
                    remote_modified_at_unix,
                    remote_size_bytes,
                )
        }
        RemoteFileMetadataPolicy::PreserveLocalConflict => true,
    };

    mutate_placeholder_identity_for_path(
        sync_root_path,
        &normalized,
        fs_metadata,
        fs_metadata_is_current,
        |identity| {
            identity.object_id = normalized_object_id.map(ToString::to_string);
            identity.path = normalized.clone();
            identity.provider_instance_id = Some(provider_instance_id);
            identity.remote_version = remote_version
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string);
            identity.remote_content_hash = remote_content_hash
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string);
            identity.remote_content_fingerprint = remote_content_fingerprint
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string);
            identity.remote_size_bytes = remote_size_bytes;
            identity.remote_modified_at_unix = remote_modified_at_unix;
            identity.set_remote_media(remote_media.cloned());
        },
    )
}

fn remote_file_system_metadata(
    file_metadata_policy: RemoteFileMetadataPolicy,
    remote_modified_at_unix: Option<u64>,
    file_size: i64,
) -> Option<CF_FS_METADATA> {
    match file_metadata_policy {
        RemoteFileMetadataPolicy::ApplyAndMarkInSync
        | RemoteFileMetadataPolicy::ApplyAfterConfirmedMove => remote_modified_at_unix
            .and_then(unix_seconds_to_windows_file_time)
            .map(|last_write_time| CF_FS_METADATA {
                BasicInfo: FILE_BASIC_INFO {
                    LastWriteTime: last_write_time,
                    ..Default::default()
                },
                FileSize: file_size,
            }),
        RemoteFileMetadataPolicy::PreserveLocalConflict => None,
    }
}

fn remote_file_system_metadata_is_current(
    metadata: &fs::Metadata,
    remote_modified_at_unix: Option<u64>,
    remote_size_bytes: Option<u64>,
) -> bool {
    let size_is_current = remote_size_bytes.is_none_or(|size| metadata.len() == size);
    let modified_at_is_current = remote_modified_at_unix.is_none_or(|modified_at| {
        metadata
            .modified()
            .ok()
            .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
            .is_some_and(|value| value.as_secs() == modified_at)
    });
    size_is_current && modified_at_is_current
}

pub fn reconcile_existing_placeholders(
    sync_root_path: &Path,
    current_snapshot: &SyncSnapshot,
    provider_instance_id: Uuid,
) -> Result<RemoteObjectReconcileReport> {
    reconcile_remote_object_state(
        sync_root_path,
        current_snapshot,
        &[],
        provider_instance_id,
        None,
    )
}

/// Reconcile placeholders at startup using an object resolver to confirm
/// deletions that happened while the adapter was offline.  Snapshot absence by
/// itself remains non-authoritative: a placeholder is treated as a tombstone
/// only when its stable object ID cannot be resolved and it has a known remote
/// revision to compare before removal.
pub fn reconcile_existing_placeholders_with_resolver(
    sync_root_path: &Path,
    current_snapshot: &SyncSnapshot,
    provider_instance_id: Uuid,
    resolver: &dyn RemoteObjectResolver,
) -> Result<RemoteObjectReconcileReport> {
    let remote_object_ids = current_snapshot
        .remote
        .iter()
        .filter_map(|entry| nonempty(entry.object_id.as_deref()))
        .collect::<BTreeSet<_>>();
    let tombstones = scan_local_placeholders(sync_root_path)
        .unique
        .iter()
        .filter_map(|(object_id, local)| {
            if remote_object_ids.contains(object_id.as_str())
                || nonempty(local.identity.remote_version.as_deref()).is_none()
            {
                return None;
            }
            match resolver.resolve_object(object_id) {
                Ok(None) => {
                    let revision = local.identity.remote_version.as_deref()?;
                    let mut previous = if local.full_path.is_dir() {
                        sync_core::NamespaceEntry::directory(&local.relative_path)
                    } else {
                        sync_core::NamespaceEntry::file(&local.relative_path, revision, "")
                    };
                    previous.object_id = Some(object_id.clone());
                    previous.version = Some(revision.to_string());
                    Some(RemoteObjectChange::Deleted { previous })
                }
                Ok(Some(_)) => None,
                Err(error) => {
                    tracing::warn!(
                        object_id,
                        path = %local.relative_path,
                        "startup deletion could not be confirmed by object ID: {error:#}"
                    );
                    None
                }
            }
        })
        .collect::<Vec<_>>();

    reconcile_remote_object_state(
        sync_root_path,
        current_snapshot,
        &tombstones,
        provider_instance_id,
        Some(resolver),
    )
}

pub fn record_uploaded_object_state(
    sync_root_path: &Path,
    relative_path: &str,
    provider_instance_id: Uuid,
    object_id: &str,
    revision: &str,
    in_sync_content_fingerprint: Option<&str>,
) -> Result<()> {
    let normalized = normalize_path(relative_path);
    let object_id = object_id.trim();
    let revision = revision.trim();
    if normalized.is_empty() || object_id.is_empty() || revision.is_empty() {
        anyhow::bail!("uploaded object state requires path, object_id, and revision");
    }
    let full_path = sync_root_path.join(normalized.replace('/', "\\"));
    if let Ok(file) = open_sync_path(&full_path, false)
        && let Ok(info) = cf_get_placeholder_standard_info_with_identity(&file)
        && let Some(identity) = decode_placeholder_file_identity(info.file_identity())
        && identity
            .object_id
            .as_deref()
            .is_some_and(|existing| existing != object_id)
    {
        anyhow::bail!(
            "refusing to replace object identity at {normalized}: existing={:?} uploaded={object_id}",
            identity.object_id
        );
    }

    mutate_placeholder_identity_for_path(sync_root_path, &normalized, None, true, |identity| {
        identity.object_id = Some(object_id.to_string());
        identity.path = normalized.clone();
        identity.remote_version = Some(revision.to_string());
        identity.provider_instance_id = Some(provider_instance_id);
        if let Some(fingerprint) = in_sync_content_fingerprint {
            identity.set_in_sync_content_baseline(fingerprint);
        }
    })
}

pub fn reconcile_remote_object_state(
    sync_root_path: &Path,
    current_snapshot: &SyncSnapshot,
    object_changes: &[RemoteObjectChange],
    provider_instance_id: Uuid,
    resolver: Option<&dyn RemoteObjectResolver>,
) -> Result<RemoteObjectReconcileReport> {
    Ok(reconcile_remote_object_state_best_effort(
        sync_root_path,
        current_snapshot,
        object_changes,
        provider_instance_id,
        resolver,
    ))
}

/// Applies each independently safe remote reconciliation action and returns
/// its complete report. Per-path filesystem failures are represented as
/// preserved conflicts in the report so callers can retain monitor suppression
/// for mutations that already completed earlier in the same pass.
pub(crate) fn reconcile_remote_object_state_best_effort(
    sync_root_path: &Path,
    current_snapshot: &SyncSnapshot,
    object_changes: &[RemoteObjectChange],
    provider_instance_id: Uuid,
    resolver: Option<&dyn RemoteObjectResolver>,
) -> RemoteObjectReconcileReport {
    let mut report = RemoteObjectReconcileReport::default();
    let mut local_placeholders = scan_local_placeholders(sync_root_path);
    report
        .preserved_paths
        .extend(local_placeholders.undecodable_paths.iter().cloned());
    // A rename delta is authoritative for this pass. If it cannot be safely
    // applied, restart reconciliation must not reinterpret the snapshot and
    // bypass the delta's resolver/revision guards.
    let renamed_object_ids = object_changes
        .iter()
        .filter_map(|change| match change {
            RemoteObjectChange::Renamed { current, .. } => {
                nonempty(current.object_id.as_deref()).map(ToString::to_string)
            }
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    // A directory rename changes the namespace of every descendant.  A
    // snapshot captured while those individual mutations are still arriving
    // is not authoritative for any path in either subtree, even when a child
    // does not yet have its own rename delta.  Preserve those paths for this
    // pass rather than moving a locally renamed descendant back to its stale
    // source location.
    let renamed_directory_subtrees = object_changes
        .iter()
        .filter_map(|change| match change {
            RemoteObjectChange::Renamed { previous, current }
                if previous.kind == sync_core::EntryKind::Directory
                    && current.kind == sync_core::EntryKind::Directory =>
            {
                let previous_path = normalize_path(&previous.path);
                let current_path = normalize_path(&current.path);
                (!previous_path.is_empty() && !current_path.is_empty())
                    .then_some((previous_path, current_path))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    for change in object_changes
        .iter()
        .filter(|change| matches!(change, RemoteObjectChange::Deleted { .. }))
    {
        let RemoteObjectChange::Deleted { previous } = change else {
            continue;
        };
        let Some(object_id) = nonempty(previous.object_id.as_deref()) else {
            continue;
        };
        let confirmed_tombstone = resolver
            .and_then(|resolver| resolver.resolve_object(object_id).ok())
            .is_some_and(|resolved| resolved.is_none());
        if !confirmed_tombstone {
            local_placeholders.record_conflict(object_id, &mut report);
            continue;
        }
        let Some(local) = local_placeholders.get_unique(object_id).cloned() else {
            local_placeholders.record_conflict(object_id, &mut report);
            continue;
        };
        let revision_matches = nonempty(local.identity.remote_version.as_deref())
            .zip(nonempty(previous.version.as_deref()))
            .is_some_and(|(local_revision, previous_revision)| local_revision == previous_revision);
        if !local.is_clean_for_confirmed_deletion(provider_instance_id) || !revision_matches {
            report.conflicted_paths.insert(local.relative_path.clone());
            report.preserved_paths.insert(local.relative_path.clone());
            let remote_path_is_vacant = !current_snapshot.remote.iter().any(|entry| {
                canonical_object_path(&entry.path) == canonical_object_path(&local.relative_path)
            });
            if remote_path_is_vacant && local.has_unsynced_local_content(provider_instance_id) {
                if let Err(error) =
                    detach_confirmed_tombstoned_identity(sync_root_path, &local, object_id)
                {
                    tracing::warn!(
                        object_id,
                        path = %local.relative_path,
                        "failed to detach the confirmed tombstoned identity from local conflict data: {error:#}"
                    );
                } else {
                    // The old object no longer exists and there is no new
                    // remote object at this path. Preserve the dirty bytes as
                    // a new local object rather than retrying a stale CAS
                    // mutation against the deleted object ID.
                    local_placeholders.unique.remove(object_id);
                }
            }
            continue;
        }

        let remove_result = if local.full_path.is_dir() {
            fs::remove_dir(&local.full_path)
        } else {
            fs::remove_file(&local.full_path)
        };
        match remove_result {
            Ok(()) => {
                report.deleted_paths.insert(local.relative_path.clone());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                report
                    .suppressed_startup_paths
                    .insert(local.relative_path.clone());
            }
        }
        local_placeholders.unique.remove(object_id);
    }

    let mut rename_changes = object_changes
        .iter()
        .filter_map(|change| match change {
            RemoteObjectChange::Renamed { previous, current } => Some((previous, current)),
            _ => None,
        })
        .collect::<Vec<_>>();
    // Process descendants before their parent directories. A child moved by a
    // remote delta creates the destination hierarchy; each parent can then
    // safely attach its own identity to that confirmed hierarchy instead of
    // colliding with it or reconstructing the old path.
    rename_changes.sort_by(|(left_previous, _), (right_previous, _)| {
        path_depth(&right_previous.path)
            .cmp(&path_depth(&left_previous.path))
            .then_with(|| left_previous.path.cmp(&right_previous.path))
    });

    for (previous, current) in rename_changes {
        let Some(object_id) = nonempty(current.object_id.as_deref()) else {
            continue;
        };
        let confirmed_remote = resolver
            .and_then(|resolver| resolver.resolve_object(object_id).ok())
            .flatten()
            .is_some_and(|resolved| {
                canonical_object_path(&resolved.path) == canonical_object_path(&current.path)
                    && nonempty(resolved.revision.as_deref())
                        == nonempty(current.version.as_deref())
            });
        if resolver.is_some() && !confirmed_remote {
            local_placeholders.record_conflict(object_id, &mut report);
            continue;
        }
        let Some(local) = local_placeholders.get_unique(object_id).cloned() else {
            local_placeholders.record_conflict(object_id, &mut report);
            continue;
        };
        let revision_matches = nonempty(local.identity.remote_version.as_deref())
            == nonempty(previous.version.as_deref());
        if !local.is_clean(provider_instance_id) || !revision_matches {
            report.conflicted_paths.insert(local.relative_path.clone());
            report.preserved_paths.insert(local.relative_path.clone());
            continue;
        }
        match move_remote_placeholder(sync_root_path, &local, current, provider_instance_id) {
            Ok(true) => {
                report
                    .renamed_paths
                    .insert(local.relative_path.clone(), normalize_path(&current.path));
            }
            Ok(false) => {
                report.conflicted_paths.insert(local.relative_path.clone());
                report.preserved_paths.insert(local.relative_path.clone());
            }
            Err(error) => {
                tracing::warn!(
                    object_id,
                    source_path = %local.relative_path,
                    target_path = %current.path,
                    "failed to apply confirmed remote rename; preserving the local placeholder and its reconciliation report: {error:#}"
                );
                report.conflicted_paths.insert(local.relative_path.clone());
                report.preserved_paths.insert(local.relative_path.clone());
            }
        }
    }

    reconcile_restart_placeholders(
        sync_root_path,
        current_snapshot,
        provider_instance_id,
        &local_placeholders.placeholders(),
        &renamed_object_ids,
        &renamed_directory_subtrees,
        &mut report,
    );

    report.observed_placeholder_paths_by_object_id = local_placeholders.paths_by_object_id();
    report
}

fn path_depth(path: &str) -> usize {
    normalize_path(path)
        .split('/')
        .filter(|segment| !segment.is_empty())
        .count()
}

fn canonical_object_path(path: &str) -> String {
    normalize_path(path).trim_end_matches('/').to_string()
}

#[derive(Clone)]
struct LocalPlaceholder {
    relative_path: String,
    full_path: std::path::PathBuf,
    identity: PlaceholderFileIdentity,
    on_disk_data_size: i64,
    modified_data_size: i64,
    in_sync_state: i32,
}

impl LocalPlaceholder {
    fn is_clean(&self, provider_instance_id: Uuid) -> bool {
        self.identity.provider_instance_id == Some(provider_instance_id)
            && normalize_path(&self.identity.path) == self.relative_path
            && self.modified_data_size == 0
            && self.in_sync_state == CF_IN_SYNC_STATE_IN_SYNC
            && identity_has_remote_baseline(&self.identity)
    }

    /// CFAPI state alone is insufficient after the provider was stopped: an
    /// application can change an already hydrated placeholder while no
    /// callback worker is active.  For a confirmed remote deletion, compare
    /// hydrated bytes with the persisted in-sync baseline before removing it.
    /// A dehydrated placeholder has no local content to protect, while a
    /// missing baseline or a fingerprinting error is treated conservatively as
    /// a conflict.
    fn is_clean_for_confirmed_deletion(&self, provider_instance_id: Uuid) -> bool {
        if !self.is_clean(provider_instance_id) {
            return false;
        }
        if self.full_path.is_dir() {
            return true;
        }
        if self.on_disk_data_size <= 0 {
            return true;
        }
        let Some(baseline) = self.identity.in_sync_content_fingerprint.as_deref() else {
            return false;
        };
        file_content_fingerprint(&self.full_path)
            .ok()
            .is_some_and(|current| current == baseline)
    }

    fn has_unsynced_local_content(&self, provider_instance_id: Uuid) -> bool {
        if self.full_path.is_dir()
            || self.identity.provider_instance_id != Some(provider_instance_id)
            || normalize_path(&self.identity.path) != self.relative_path
            || (self.modified_data_size == 0 && self.in_sync_state == CF_IN_SYNC_STATE_IN_SYNC)
        {
            return false;
        }
        let Some(baseline) = self.identity.in_sync_content_fingerprint.as_deref() else {
            return false;
        };
        if self.on_disk_data_size <= 0 {
            return self.modified_data_size != 0;
        }
        file_content_fingerprint(&self.full_path)
            .ok()
            .is_some_and(|current| current != baseline)
    }
}

fn detach_confirmed_tombstoned_identity(
    sync_root_path: &Path,
    local: &LocalPlaceholder,
    expected_object_id: &str,
) -> Result<()> {
    mutate_placeholder_identity_for_path(
        sync_root_path,
        &local.relative_path,
        None,
        true,
        |identity| {
            // This runs during startup before the callback connection is made.
            // Keep the object/path check nonetheless, so an unexpected identity
            // replacement cannot turn into a path-based overwrite.
            if identity.object_id.as_deref() != Some(expected_object_id) {
                return;
            }
            identity.object_id = None;
            identity.remote_version = None;
            identity.remote_content_hash = None;
            identity.remote_content_fingerprint = None;
            identity.remote_size_bytes = None;
            identity.remote_modified_at_unix = None;
            identity.remote_media = None;
            identity.remote_media_absent = false;
            identity.in_sync_content_fingerprint = None;
        },
    )
}

#[derive(Default)]
struct LocalPlaceholderIndex {
    unique: HashMap<String, LocalPlaceholder>,
    ambiguous: HashMap<String, Vec<LocalPlaceholder>>,
    unbound: Vec<LocalPlaceholder>,
    undecodable_paths: BTreeSet<String>,
}

impl LocalPlaceholderIndex {
    fn insert(&mut self, placeholder: LocalPlaceholder) {
        let Some(object_id) = nonempty(placeholder.identity.object_id.as_deref()) else {
            self.unbound.push(placeholder);
            return;
        };
        let object_id = object_id.to_string();
        if let Some(duplicates) = self.ambiguous.get_mut(&object_id) {
            duplicates.push(placeholder);
        } else if let Some(first) = self.unique.remove(&object_id) {
            self.ambiguous.insert(object_id, vec![first, placeholder]);
        } else {
            self.unique.insert(object_id, placeholder);
        }
    }

    fn get_unique(&self, object_id: &str) -> Option<&LocalPlaceholder> {
        self.unique.get(object_id)
    }

    fn record_conflict(&self, object_id: &str, report: &mut RemoteObjectReconcileReport) {
        let Some(duplicates) = self.ambiguous.get(object_id) else {
            return;
        };
        for placeholder in duplicates {
            report
                .conflicted_paths
                .insert(placeholder.relative_path.clone());
            report
                .preserved_paths
                .insert(placeholder.relative_path.clone());
        }
    }

    fn paths_by_object_id(&self) -> BTreeMap<String, BTreeSet<String>> {
        let mut paths = BTreeMap::new();
        for (object_id, placeholder) in &self.unique {
            paths
                .entry(object_id.clone())
                .or_insert_with(BTreeSet::new)
                .insert(placeholder.relative_path.clone());
        }
        for (object_id, placeholders) in &self.ambiguous {
            let object_paths = paths.entry(object_id.clone()).or_insert_with(BTreeSet::new);
            object_paths.extend(
                placeholders
                    .iter()
                    .map(|placeholder| placeholder.relative_path.clone()),
            );
        }
        paths
    }

    fn placeholders(&self) -> Vec<LocalPlaceholder> {
        let mut placeholders = self.unbound.clone();
        placeholders.extend(self.unique.values().cloned());
        placeholders.extend(self.ambiguous.values().flatten().cloned());
        placeholders
    }
}

fn scan_local_placeholders(sync_root_path: &Path) -> LocalPlaceholderIndex {
    let mut placeholders = LocalPlaceholderIndex::default();
    for entry in WalkDir::new(sync_root_path)
        .min_depth(1)
        .into_iter()
        .flatten()
    {
        if !entry.file_type().is_file() && !entry.file_type().is_dir() {
            continue;
        }
        let relative_path = path_to_relative(sync_root_path, &entry.path().to_string_lossy());
        if relative_path.is_empty() || is_internal_sync_root_relative_path(&relative_path) {
            continue;
        }
        let Ok(file) = open_sync_path(entry.path(), false) else {
            continue;
        };
        let Ok(info) = cf_get_placeholder_standard_info_with_identity(&file) else {
            continue;
        };
        let Some(identity) = decode_placeholder_file_identity(info.file_identity()) else {
            placeholders.undecodable_paths.insert(relative_path);
            continue;
        };
        placeholders.insert(LocalPlaceholder {
            relative_path,
            full_path: entry.into_path(),
            identity,
            on_disk_data_size: info.info().OnDiskDataSize,
            modified_data_size: info.info().ModifiedDataSize,
            in_sync_state: info.info().InSyncState,
        });
    }
    placeholders
}

pub(crate) fn local_placeholder_paths_by_object_id(
    sync_root_path: &Path,
) -> BTreeMap<String, BTreeSet<String>> {
    scan_local_placeholders(sync_root_path).paths_by_object_id()
}

fn reconcile_restart_placeholders(
    sync_root_path: &Path,
    current_snapshot: &SyncSnapshot,
    provider_instance_id: Uuid,
    local_placeholders: &[LocalPlaceholder],
    renamed_object_ids: &BTreeSet<String>,
    renamed_directory_subtrees: &[(String, String)],
    report: &mut RemoteObjectReconcileReport,
) {
    let remote_by_object_id = unique_remote_files_by_object_id(current_snapshot);
    let remote_by_path = current_snapshot
        .remote
        .iter()
        .map(|entry| (normalize_path(&entry.path), entry))
        .collect::<HashMap<_, _>>();

    for local in local_placeholders {
        let relative_path = local.relative_path.clone();
        let identity = &local.identity;

        if renamed_directory_subtrees
            .iter()
            .any(|(previous, current)| {
                path_is_same_or_descendant(&relative_path, previous)
                    || path_is_same_or_descendant(&relative_path, current)
            })
        {
            report.preserved_paths.insert(relative_path);
            continue;
        }

        if let Some(object_id) = nonempty(identity.object_id.as_deref()) {
            if renamed_object_ids.contains(object_id) {
                report.preserved_paths.insert(relative_path);
                continue;
            }
            let Some(remote) = remote_by_object_id.get(object_id) else {
                // Absence is not a tombstone. Keep the placeholder until a
                // confirmed object-id deletion delta arrives.
                report.preserved_paths.insert(relative_path);
                continue;
            };
            if normalize_path(&remote.path) != relative_path
                && !report.renamed_paths.contains_key(&relative_path)
            {
                // A restart snapshot can lag a local rename that the server
                // already accepted. A remote move is safe to apply only when
                // its revision proves the remote state advanced beyond the
                // local placeholder's last confirmed version. Otherwise
                // preserve the local location rather than moving it back to a
                // stale path.
                let remote_rename_is_newer =
                    nonempty(remote.version.as_deref()).is_some_and(|remote_revision| {
                        nonempty(identity.remote_version.as_deref()) != Some(remote_revision)
                    });
                if local.is_clean(provider_instance_id) && remote_rename_is_newer {
                    match move_remote_placeholder(
                        sync_root_path,
                        local,
                        remote,
                        provider_instance_id,
                    ) {
                        Ok(true) => {
                            report
                                .renamed_paths
                                .insert(relative_path, normalize_path(&remote.path));
                        }
                        Ok(false) => {
                            report.conflicted_paths.insert(relative_path.clone());
                            report.preserved_paths.insert(relative_path);
                        }
                        Err(error) => {
                            tracing::warn!(
                                object_id,
                                source_path = %relative_path,
                                target_path = %remote.path,
                                "failed to apply restart remote rename; preserving the local placeholder and its reconciliation report: {error:#}"
                            );
                            report.conflicted_paths.insert(relative_path.clone());
                            report.preserved_paths.insert(relative_path);
                        }
                    }
                } else {
                    report.conflicted_paths.insert(relative_path.clone());
                    report.preserved_paths.insert(relative_path);
                }
            }
            continue;
        }

        let Some(remote) = remote_by_path.get(&relative_path) else {
            report.preserved_paths.insert(relative_path);
            continue;
        };
        let safe_legacy_match = local.is_clean(provider_instance_id)
            && nonempty(remote.object_id.as_deref()).is_some()
            && legacy_identity_matches_remote_baseline(
                identity,
                remote.version.as_deref(),
                remote.content_hash.as_deref(),
                remote.content_fingerprint.as_deref(),
            );
        if !safe_legacy_match {
            report.conflicted_paths.insert(relative_path.clone());
            report.preserved_paths.insert(relative_path);
            continue;
        }
        if let Err(error) = refresh_remote_placeholder_state(
            sync_root_path,
            &relative_path,
            provider_instance_id,
            remote_placeholder_state(remote),
        ) {
            tracing::warn!(
                path = %relative_path,
                "failed to migrate the legacy placeholder; preserving the local placeholder and its reconciliation report: {error:#}"
            );
            report.conflicted_paths.insert(relative_path.clone());
            report.preserved_paths.insert(relative_path);
        } else {
            report.migrated_paths.insert(relative_path);
        }
    }
}

fn path_is_same_or_descendant(path: &str, ancestor: &str) -> bool {
    path == ancestor
        || path
            .strip_prefix(ancestor)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn unique_remote_files_by_object_id(
    snapshot: &SyncSnapshot,
) -> HashMap<String, &sync_core::NamespaceEntry> {
    let mut unique = HashMap::new();
    let mut ambiguous = BTreeSet::new();
    for entry in &snapshot.remote {
        let Some(object_id) = nonempty(entry.object_id.as_deref()) else {
            continue;
        };
        if ambiguous.contains(object_id) {
            continue;
        }
        if unique.insert(object_id.to_string(), entry).is_some() {
            unique.remove(object_id);
            ambiguous.insert(object_id.to_string());
        }
    }
    unique
}

fn move_remote_placeholder(
    sync_root_path: &Path,
    local: &LocalPlaceholder,
    remote: &sync_core::NamespaceEntry,
    provider_instance_id: Uuid,
) -> Result<bool> {
    let target_relative_path = normalize_path(&remote.path);
    if target_relative_path == local.relative_path {
        return Ok(true);
    }
    let target_path = sync_root_path.join(target_relative_path.replace('/', "\\"));
    if target_path.exists() {
        if local.full_path.is_dir() && target_path.is_dir() {
            return merge_remote_directory_placeholder(
                sync_root_path,
                local,
                remote,
                provider_instance_id,
                &target_path,
            );
        }
        return Ok(false);
    }
    if let Some(parent) = target_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(&local.full_path, &target_path).with_context(|| {
        format!(
            "failed to move remote placeholder {} -> {}",
            local.full_path.display(),
            target_path.display()
        )
    })?;
    if let Err(error) = write_remote_move_identity(
        sync_root_path,
        &target_relative_path,
        provider_instance_id,
        remote,
    ) {
        return match fs::rename(&target_path, &local.full_path) {
            Ok(()) => Err(error.context(format!(
                "failed to update remote move identity after moving {}; restored the source path",
                target_path.display()
            ))),
            Err(rollback_error) => Err(error.context(format!(
                "failed to update remote move identity after moving {}; rollback to {} also failed: {rollback_error}",
                target_path.display(),
                local.full_path.display()
            ))),
        };
    }
    let moved_file = open_sync_path(&target_path, true)?;
    cf_set_in_sync(&moved_file)?;
    Ok(true)
}

fn write_remote_move_identity(
    sync_root_path: &Path,
    target_relative_path: &str,
    provider_instance_id: Uuid,
    remote: &sync_core::NamespaceEntry,
) -> Result<()> {
    let target_relative_path = normalize_path(target_relative_path);
    mutate_placeholder_identity_for_path(
        sync_root_path,
        &target_relative_path,
        None,
        true,
        |identity| {
            identity.object_id = remote
                .object_id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string);
            identity.path = target_relative_path.clone();
            identity.provider_instance_id = Some(provider_instance_id);
            identity.remote_version = remote
                .version
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string);
            identity.remote_content_hash = remote
                .content_hash
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string);
            identity.remote_content_fingerprint = remote
                .content_fingerprint
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string);
            identity.remote_size_bytes = remote.size_bytes;
            identity.remote_modified_at_unix = remote.modified_at_unix;
            identity.set_remote_media(remote.media.clone());
        },
    )
}

/// A remote directory rename can arrive after one of its descendants has
/// already been moved. In that case the target directory exists only as the
/// provider-created parent of clean, confirmed placeholders. We may attach the
/// directory object identity to it after removing an empty clean source. Any
/// local data or unknown target entry remains a conflict instead.
fn merge_remote_directory_placeholder(
    sync_root_path: &Path,
    local: &LocalPlaceholder,
    remote: &sync_core::NamespaceEntry,
    provider_instance_id: Uuid,
    target_path: &Path,
) -> Result<bool> {
    let source_is_empty = fs::read_dir(&local.full_path)?.next().is_none();
    let target_is_confirmed = directory_has_only_clean_remote_placeholders(
        sync_root_path,
        target_path,
        provider_instance_id,
    );
    if !source_is_empty || !target_is_confirmed {
        return Ok(false);
    }
    if let Err(error) = fs::remove_dir(&local.full_path) {
        return if error.kind() == std::io::ErrorKind::NotFound {
            Ok(false)
        } else {
            Err(error).with_context(|| {
                format!(
                    "failed to remove subsumed remote directory {}",
                    local.full_path.display()
                )
            })
        };
    }

    let target_relative_path = normalize_path(&remote.path);
    let target_file = open_sync_path(target_path, true)?;
    cf_ensure_placeholder_identity(&target_file, &target_relative_path)?;
    drop(target_file);
    refresh_remote_placeholder_state_with_policy(
        sync_root_path,
        &target_relative_path,
        provider_instance_id,
        remote_placeholder_state(remote),
        RemoteFileMetadataPolicy::ApplyAfterConfirmedMove,
    )?;
    let target_file = open_sync_path(target_path, true)?;
    cf_set_in_sync(&target_file)?;
    Ok(true)
}

fn directory_has_only_clean_remote_placeholders(
    sync_root_path: &Path,
    directory_path: &Path,
    provider_instance_id: Uuid,
) -> bool {
    let mut saw_placeholder = false;
    for entry in WalkDir::new(directory_path)
        .min_depth(1)
        .into_iter()
        .flatten()
    {
        if !entry.file_type().is_file() && !entry.file_type().is_dir() {
            continue;
        }
        let relative_path = path_to_relative(sync_root_path, &entry.path().to_string_lossy());
        let Ok(file) = open_sync_path(entry.path(), false) else {
            return false;
        };
        let Ok(info) = cf_get_placeholder_standard_info_with_identity(&file) else {
            return false;
        };
        let Some(identity) = decode_placeholder_file_identity(info.file_identity()) else {
            return false;
        };
        let local = LocalPlaceholder {
            relative_path,
            full_path: entry.into_path(),
            identity,
            on_disk_data_size: info.info().OnDiskDataSize,
            modified_data_size: info.info().ModifiedDataSize,
            in_sync_state: info.info().InSyncState,
        };
        if nonempty(local.identity.object_id.as_deref()).is_none()
            || !local.is_clean(provider_instance_id)
        {
            return false;
        }
        saw_placeholder = true;
    }
    saw_placeholder
}

fn remote_placeholder_state(entry: &sync_core::NamespaceEntry) -> RemotePlaceholderState<'_> {
    RemotePlaceholderState {
        object_id: entry.object_id.as_deref(),
        remote_version: entry.version.as_deref(),
        remote_content_hash: entry.content_hash.as_deref(),
        remote_size_bytes: entry.size_bytes,
        remote_content_fingerprint: entry.content_fingerprint.as_deref(),
        remote_modified_at_unix: entry.modified_at_unix,
        remote_media: entry.media.as_ref(),
    }
}

fn nonempty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

/// Legacy v2 placeholders predate server-provided revision IDs.  They may
/// carry the synthetic `server-head` selector instead of a concrete revision.
/// At the unchanged path that selector can be promoted only when the recorded
/// content baseline also matches the current remote object; otherwise the
/// placeholder remains unbound for a later conservative reconciliation.
fn legacy_identity_matches_remote_baseline(
    identity: &PlaceholderFileIdentity,
    remote_revision: Option<&str>,
    remote_content_hash: Option<&str>,
    remote_content_fingerprint: Option<&str>,
) -> bool {
    let local_revision = nonempty(identity.remote_version.as_deref());
    let remote_revision = nonempty(remote_revision);
    if remote_revision.is_some() && local_revision == remote_revision {
        return true;
    }

    let is_synthetic_legacy_revision =
        local_revision.is_some_and(|revision| revision.starts_with("server-head"));
    let same_content_hash = nonempty(identity.remote_content_hash.as_deref())
        .zip(nonempty(remote_content_hash))
        .is_some_and(|(local, remote)| local == remote);
    let same_content_fingerprint = nonempty(identity.remote_content_fingerprint.as_deref())
        .zip(nonempty(remote_content_fingerprint))
        .is_some_and(|(local, remote)| local == remote);
    is_synthetic_legacy_revision
        && remote_revision.is_some()
        && (same_content_hash || same_content_fingerprint)
}

fn mutate_placeholder_identity_for_path(
    sync_root_path: &Path,
    relative_path: &str,
    fs_metadata: Option<CF_FS_METADATA>,
    fs_metadata_is_current: bool,
    mutator: impl FnOnce(&mut PlaceholderFileIdentity),
) -> Result<()> {
    let full_path = sync_root_path.join(relative_path.replace('/', "\\"));
    let file = open_sync_path(&full_path, false).with_context(|| {
        format!(
            "failed to open {} for placeholder metadata read",
            full_path.display()
        )
    })?;

    let mut identity = match cf_get_placeholder_standard_info_with_identity(&file) {
        Ok(info) => decode_placeholder_file_identity(info.file_identity())
            .unwrap_or_else(|| PlaceholderFileIdentity::new(relative_path)),
        Err(_) => PlaceholderFileIdentity::new(relative_path),
    };
    let original_identity = identity.clone();
    mutator(&mut identity);
    let fs_metadata_needs_update = fs_metadata.is_some() && !fs_metadata_is_current;
    // Avoid reopening unchanged placeholders, while still repairing a stale
    // LastWriteTime or file size even when the encoded identity already matches.
    if identity == original_identity && !fs_metadata_needs_update {
        return Ok(());
    }
    let encoded = identity.encoded();

    let oplock_result = match fs_metadata.as_ref() {
        Some(metadata) => {
            cf_update_placeholder_metadata_and_identity_with_oplock(&full_path, metadata, &encoded)
        }
        None => cf_update_placeholder_file_identity_with_oplock(&full_path, &encoded),
    };
    match oplock_result {
        Ok(()) => Ok(()),
        Err(oplock_err) => {
            if path_is_placeholder(&full_path) {
                return Err(oplock_err).with_context(|| {
                    format!(
                        "refusing to reopen existing placeholder {} with generic write access after oplock metadata update failed",
                        full_path.display()
                    )
                });
            }
            let writable_file = open_sync_path(&full_path, true).with_context(|| {
                format!(
                    "failed to reopen {} for placeholder metadata update fallback",
                    full_path.display()
                )
            })?;
            cf_ensure_placeholder_identity(&writable_file, relative_path)?;
            match fs_metadata.as_ref() {
                Some(metadata) => {
                    cf_update_placeholder_metadata_and_identity(&writable_file, metadata, &encoded)
                }
                None => cf_update_placeholder_file_identity(&writable_file, &encoded),
            }
        }
    }
}

fn identity_has_remote_baseline(identity: &PlaceholderFileIdentity) -> bool {
    identity.remote_version.is_some()
        || identity.remote_content_hash.is_some()
        || identity.remote_content_fingerprint.is_some()
        || identity.remote_size_bytes.is_some()
        || identity.remote_modified_at_unix.is_some()
        || identity.remote_media.is_some()
        || identity.remote_media_absent
        || identity.in_sync_content_fingerprint.is_some()
}

fn is_internal_sync_root_relative_path(path: &str) -> bool {
    is_internal_client_identity_relative_path(path)
        || is_internal_connection_bootstrap_relative_path(path)
        || is_internal_remote_snapshot_relative_path(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{CfapiAction, CfapiActionPlan};
    use crate::runtime::{
        SyncRootRegistration, apply_action_plan, register_sync_root, unregister_sync_root,
    };
    use std::path::PathBuf;
    use std::time::{Duration, UNIX_EPOCH};
    use sync_core::NamespaceEntry;

    #[derive(Default)]
    struct StaticResolver {
        objects: HashMap<String, ResolvedRemoteObject>,
    }

    impl RemoteObjectResolver for StaticResolver {
        fn resolve_object(&self, object_id: &str) -> Result<Option<ResolvedRemoteObject>> {
            Ok(self.objects.get(object_id).cloned())
        }
    }

    fn remote_file(path: &str, object_id: &str, revision: &str, hash: &str) -> NamespaceEntry {
        NamespaceEntry::file(path, revision, hash).with_object_id(object_id)
    }

    #[test]
    fn canonical_object_path_ignores_directory_marker_suffix() {
        assert_eq!(canonical_object_path("archive/"), "archive");
        assert_eq!(canonical_object_path("archive/nested/"), "archive/nested");
    }

    /// Registers a real Windows CFAPI root so reconciliation sees the same
    /// provider-managed placeholders as it does in production.
    struct RegisteredTestSyncRoot {
        root_path: PathBuf,
        _registration_lock: std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for RegisteredTestSyncRoot {
        fn drop(&mut self) {
            let _ = unregister_sync_root(&self.root_path);
            let _ = fs::remove_dir_all(&self.root_path);
        }
    }

    fn registered_test_sync_root(test_name: &str) -> (RegisteredTestSyncRoot, Uuid) {
        let registration_lock = crate::lock_sync_root_registration_tests();
        let unique = Uuid::new_v4();
        let root_path = std::env::temp_dir().join(format!(
            "ironmesh-placeholder-metadata-{test_name}-{unique}"
        ));
        let registration = SyncRootRegistration::new(
            format!("test-placeholder-metadata-{test_name}-{unique}"),
            "Ironmesh Placeholder Metadata Test",
            &root_path,
            Uuid::new_v4(),
            None,
        );
        let identity =
            register_sync_root(&registration).expect("test sync root registration should succeed");

        (
            RegisteredTestSyncRoot {
                root_path,
                _registration_lock: registration_lock,
            },
            identity.provider_instance_id,
        )
    }

    fn create_clean_provider_placeholder(
        sync_root: &Path,
        provider_instance_id: Uuid,
        path: &str,
        remote_version: &str,
        remote_content_hash: &str,
        remote_modified_at_unix: u64,
    ) {
        apply_action_plan(
            sync_root,
            &CfapiActionPlan {
                actions: vec![CfapiAction::EnsurePlaceholder {
                    object_id: Some(format!("object-{remote_content_hash}")),
                    path: path.to_string(),
                    remote_version: remote_version.to_string(),
                    remote_content_hash: remote_content_hash.to_string(),
                    remote_size: Some(1_024),
                    remote_content_fingerprint: Some(format!("fingerprint-{remote_content_hash}")),
                    remote_modified_at_unix: Some(remote_modified_at_unix),
                    remote_media: None,
                }],
            },
            provider_instance_id,
            true,
        )
        .expect("clean provider placeholder should be created");
    }

    fn remote_snapshot_with_small_unrelated_change() -> SyncSnapshot {
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
    fn conflict_refresh_preserves_local_file_metadata_and_sync_state() {
        assert!(
            remote_file_system_metadata(
                RemoteFileMetadataPolicy::PreserveLocalConflict,
                Some(1_723_456_789),
                42,
            )
            .is_none()
        );
        assert!(
            remote_file_system_metadata(
                RemoteFileMetadataPolicy::ApplyAndMarkInSync,
                Some(1_723_456_789),
                42,
            )
            .is_some()
        );
    }

    #[test]
    fn conflict_refresh_records_remote_identity_without_clearing_local_dirty_state() {
        let (sync_root, provider_instance_id) = registered_test_sync_root("conflict-identity");
        let path = "docs/conflicted.txt";
        let object_id = "object-conflict-hash";
        create_clean_provider_placeholder(
            &sync_root.root_path,
            provider_instance_id,
            path,
            "revision-1",
            "conflict-hash",
            1_725_100_000,
        );
        let full_path = sync_root.root_path.join("docs\\conflicted.txt");
        let file = open_sync_path(&full_path, true).expect("placeholder should open");
        crate::cfapi::cf_set_not_in_sync(&file).expect("placeholder should become dirty");
        drop(file);

        refresh_remote_conflict_identity(
            &sync_root.root_path,
            path,
            provider_instance_id,
            RemotePlaceholderState {
                object_id: Some(object_id),
                remote_version: Some("revision-2"),
                remote_content_hash: Some("remote-conflict-hash"),
                remote_size_bytes: Some(2_048),
                remote_content_fingerprint: Some("remote-conflict-fingerprint"),
                remote_modified_at_unix: Some(1_725_100_001),
                remote_media: None,
            },
        )
        .expect("conflict refresh should retain the remote side of the conflict");

        let file = open_sync_path(&full_path, false).expect("placeholder should remain");
        let info = cf_get_placeholder_standard_info_with_identity(&file)
            .expect("updated conflict identity should be readable");
        let identity = decode_placeholder_file_identity(info.file_identity())
            .expect("updated conflict identity should decode");
        assert_eq!(identity.object_id.as_deref(), Some(object_id));
        assert_eq!(identity.remote_version.as_deref(), Some("revision-2"));
        assert_ne!(info.info().InSyncState, CF_IN_SYNC_STATE_IN_SYNC);
    }

    #[test]
    fn remote_metadata_comparison_detects_stale_timestamp_and_size() {
        let root =
            std::env::temp_dir().join(format!("ironmesh-placeholder-meta-{}", Uuid::now_v7()));
        fs::create_dir_all(&root).expect("test directory should exist");
        let path = root.join("photo.jpg");
        fs::write(&path, vec![0_u8; 42]).expect("test file should be written");
        let modified_at = 1_723_456_789;
        fs::File::options()
            .write(true)
            .open(&path)
            .expect("test file should open")
            .set_modified(UNIX_EPOCH + Duration::from_secs(modified_at))
            .expect("test timestamp should be set");
        let metadata = fs::metadata(&path).expect("test metadata should load");

        assert!(remote_file_system_metadata_is_current(
            &metadata,
            Some(modified_at),
            Some(42),
        ));
        assert!(!remote_file_system_metadata_is_current(
            &metadata,
            Some(modified_at + 1),
            Some(42),
        ));
        assert!(!remote_file_system_metadata_is_current(
            &metadata,
            Some(modified_at),
            Some(43),
        ));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn remote_modify_keeps_object_id_and_advances_revision() {
        let (sync_root, provider_instance_id) = registered_test_sync_root("remote-modify");
        let path = "docs/modified.txt";
        let object_id = "object-modify-hash";
        create_clean_provider_placeholder(
            &sync_root.root_path,
            provider_instance_id,
            path,
            "revision-1",
            "modify-hash",
            1_725_100_000,
        );

        refresh_remote_placeholder_state(
            &sync_root.root_path,
            path,
            provider_instance_id,
            RemotePlaceholderState {
                object_id: Some(object_id),
                remote_version: Some("revision-2"),
                remote_content_hash: Some("modify-hash-2"),
                remote_size_bytes: Some(2_048),
                remote_content_fingerprint: Some("fingerprint-modify-2"),
                remote_modified_at_unix: Some(1_725_100_001),
                remote_media: None,
            },
        )
        .expect("clean remote modification should refresh the placeholder");

        let file = open_sync_path(&sync_root.root_path.join("docs/modified.txt"), false)
            .expect("modified placeholder should open");
        let info = cf_get_placeholder_standard_info_with_identity(&file)
            .expect("modified identity should be readable");
        let identity = decode_placeholder_file_identity(info.file_identity())
            .expect("modified identity should decode");
        assert_eq!(identity.object_id.as_deref(), Some(object_id));
        assert_eq!(identity.remote_version.as_deref(), Some("revision-2"));
    }

    #[test]
    fn remote_rename_moves_existing_placeholder_by_object_id() {
        let (sync_root, provider_instance_id) = registered_test_sync_root("remote-rename");
        let old_path = "docs/old-name.txt";
        let new_path = "archive/new-name.txt";
        let object_id = "object-rename-hash";
        create_clean_provider_placeholder(
            &sync_root.root_path,
            provider_instance_id,
            old_path,
            "revision-1",
            "rename-hash",
            1_725_100_001,
        );
        let previous = remote_file(old_path, object_id, "revision-1", "rename-hash");
        let current = remote_file(new_path, object_id, "revision-2", "rename-hash");
        let resolver = StaticResolver {
            objects: HashMap::from([(
                object_id.to_string(),
                ResolvedRemoteObject {
                    object_id: object_id.to_string(),
                    path: new_path.to_string(),
                    revision: Some("revision-2".to_string()),
                },
            )]),
        };

        let report = reconcile_remote_object_state(
            &sync_root.root_path,
            &SyncSnapshot {
                local: Vec::new(),
                remote: vec![current.clone()],
            },
            &[RemoteObjectChange::Renamed { previous, current }],
            provider_instance_id,
            Some(&resolver),
        )
        .expect("remote rename should reconcile");

        assert_eq!(
            report.renamed_paths.get(old_path).map(String::as_str),
            Some(new_path)
        );
        assert!(!sync_root.root_path.join("docs\\old-name.txt").exists());
        let new_full_path = sync_root.root_path.join("archive\\new-name.txt");
        let file = open_sync_path(&new_full_path, false).expect("renamed placeholder should exist");
        let info = cf_get_placeholder_standard_info_with_identity(&file)
            .expect("renamed placeholder identity should be readable");
        let identity = decode_placeholder_file_identity(info.file_identity())
            .expect("renamed placeholder identity should decode");
        assert_eq!(identity.object_id.as_deref(), Some(object_id));
        assert_eq!(identity.remote_version.as_deref(), Some("revision-2"));
        assert_eq!(identity.path, new_path);
    }

    #[test]
    fn rejected_rename_delta_is_not_reapplied_by_restart_reconciliation() {
        let (sync_root, provider_instance_id) = registered_test_sync_root("rename-suppression");
        let old_path = "docs/original.txt";
        let new_path = "archive/remote.txt";
        let object_id = "object-rename-suppression";
        create_clean_provider_placeholder(
            &sync_root.root_path,
            provider_instance_id,
            old_path,
            "revision-1",
            "rename-suppression",
            1_725_100_001,
        );
        let previous = remote_file(old_path, object_id, "revision-1", "rename-suppression");
        let current = remote_file(new_path, object_id, "revision-2", "rename-suppression");

        let report = reconcile_remote_object_state(
            &sync_root.root_path,
            &SyncSnapshot {
                local: Vec::new(),
                remote: vec![current.clone()],
            },
            &[RemoteObjectChange::Renamed { previous, current }],
            provider_instance_id,
            Some(&StaticResolver::default()),
        )
        .expect("rejected rename should be classified without a move");

        assert!(report.preserved_paths.contains(old_path));
        assert!(sync_root.root_path.join("docs\\original.txt").exists());
        assert!(!sync_root.root_path.join("archive\\remote.txt").exists());
    }

    #[test]
    fn confirmed_tombstone_deletes_only_matching_object_id_and_revision() {
        let (sync_root, provider_instance_id) = registered_test_sync_root("remote-tombstone");
        let path = "docs/deleted.txt";
        let object_id = "object-delete-hash";
        create_clean_provider_placeholder(
            &sync_root.root_path,
            provider_instance_id,
            path,
            "revision-7",
            "delete-hash",
            1_725_100_002,
        );
        let previous = remote_file(path, object_id, "revision-7", "delete-hash");

        let report = reconcile_remote_object_state(
            &sync_root.root_path,
            &SyncSnapshot::default(),
            &[RemoteObjectChange::Deleted { previous }],
            provider_instance_id,
            Some(&StaticResolver::default()),
        )
        .expect("confirmed tombstone should reconcile");

        assert!(report.deleted_paths.contains(path));
        assert!(!sync_root.root_path.join("docs\\deleted.txt").exists());
    }

    #[test]
    fn startup_reconciliation_deletes_clean_placeholder_only_after_object_id_confirmation() {
        let (sync_root, provider_instance_id) =
            registered_test_sync_root("startup-confirmed-object-delete");
        let path = "docs/deleted-while-offline.txt";
        create_clean_provider_placeholder(
            &sync_root.root_path,
            provider_instance_id,
            path,
            "revision-7",
            "offline-delete",
            1_725_100_012,
        );

        let report = reconcile_existing_placeholders_with_resolver(
            &sync_root.root_path,
            &SyncSnapshot::default(),
            provider_instance_id,
            &StaticResolver::default(),
        )
        .expect("confirmed missing object should be reconciled at startup");

        assert!(report.deleted_paths.contains(path));
        assert!(
            !sync_root
                .root_path
                .join("docs\\deleted-while-offline.txt")
                .exists()
        );
    }

    #[test]
    fn delete_and_recreate_same_path_installs_new_object_identity() {
        let (sync_root, provider_instance_id) = registered_test_sync_root("replace-same-path");
        let path = "docs/replaced.txt";
        let old_object_id = "object-replace-old";
        let new_object_id = "object-replace-new";
        create_clean_provider_placeholder(
            &sync_root.root_path,
            provider_instance_id,
            path,
            "revision-old",
            "replace-old",
            1_725_100_007,
        );
        let previous = remote_file(path, old_object_id, "revision-old", "replace-old");
        let current = remote_file(path, new_object_id, "revision-new", "replace-new");

        let report = reconcile_remote_object_state(
            &sync_root.root_path,
            &SyncSnapshot {
                local: Vec::new(),
                remote: vec![current.clone()],
            },
            &[
                RemoteObjectChange::Deleted { previous },
                RemoteObjectChange::Created {
                    current: current.clone(),
                },
            ],
            provider_instance_id,
            Some(&StaticResolver::default()),
        )
        .expect("old tombstone should remove only the old object");
        assert!(report.deleted_paths.contains(path));

        apply_action_plan(
            &sync_root.root_path,
            &CfapiActionPlan {
                actions: vec![CfapiAction::EnsurePlaceholder {
                    object_id: Some(new_object_id.to_string()),
                    path: path.to_string(),
                    remote_version: "revision-new".to_string(),
                    remote_content_hash: "replace-new".to_string(),
                    remote_size: Some(128),
                    remote_content_fingerprint: Some("fingerprint-new".to_string()),
                    remote_modified_at_unix: Some(1_725_100_008),
                    remote_media: None,
                }],
            },
            provider_instance_id,
            true,
        )
        .expect("replacement placeholder should be created");

        let file = open_sync_path(&sync_root.root_path.join("docs\\replaced.txt"), false)
            .expect("replacement placeholder should exist");
        let info = cf_get_placeholder_standard_info_with_identity(&file)
            .expect("replacement identity should be readable");
        let identity = decode_placeholder_file_identity(info.file_identity())
            .expect("replacement identity should decode");
        assert_eq!(identity.object_id.as_deref(), Some(new_object_id));
        assert_ne!(identity.object_id.as_deref(), Some(old_object_id));
    }

    #[test]
    fn stale_tombstone_revision_is_a_conflict_not_a_delete() {
        let (sync_root, provider_instance_id) =
            registered_test_sync_root("stale-tombstone-revision");
        let path = "docs/changed.txt";
        let object_id = "object-stale-hash";
        create_clean_provider_placeholder(
            &sync_root.root_path,
            provider_instance_id,
            path,
            "revision-2",
            "stale-hash",
            1_725_100_003,
        );
        let stale_previous = remote_file(path, object_id, "revision-1", "stale-hash");

        let report = reconcile_remote_object_state(
            &sync_root.root_path,
            &SyncSnapshot::default(),
            &[RemoteObjectChange::Deleted {
                previous: stale_previous,
            }],
            provider_instance_id,
            Some(&StaticResolver::default()),
        )
        .expect("stale tombstone should be handled conservatively");

        assert!(report.deleted_paths.is_empty());
        assert!(report.conflicted_paths.contains(path));
        assert!(sync_root.root_path.join("docs\\changed.txt").exists());
    }

    #[test]
    fn restart_reconciliation_moves_placeholder_by_object_id() {
        let (sync_root, provider_instance_id) = registered_test_sync_root("restart-rename");
        let old_path = "docs/before-restart.txt";
        let new_path = "docs/after-restart.txt";
        let object_id = "object-restart-hash";
        create_clean_provider_placeholder(
            &sync_root.root_path,
            provider_instance_id,
            old_path,
            "revision-1",
            "restart-hash",
            1_725_100_004,
        );

        let report = reconcile_existing_placeholders(
            &sync_root.root_path,
            &SyncSnapshot {
                local: Vec::new(),
                remote: vec![remote_file(
                    new_path,
                    object_id,
                    "revision-2",
                    "restart-hash",
                )],
            },
            provider_instance_id,
        )
        .expect("restart reconciliation should succeed");

        assert_eq!(
            report.renamed_paths.get(old_path).map(String::as_str),
            Some(new_path)
        );
        assert!(sync_root.root_path.join("docs\\after-restart.txt").exists());
    }

    #[test]
    fn restart_reconciliation_does_not_undo_an_accepted_local_rename_from_a_stale_snapshot() {
        let (sync_root, provider_instance_id) = registered_test_sync_root("stale-local-rename");
        let old_path = "docs/before-rename.txt";
        let new_path = "archive/after-rename.txt";
        let object_id = "object-local-rename";
        create_clean_provider_placeholder(
            &sync_root.root_path,
            provider_instance_id,
            old_path,
            "revision-1",
            "local-rename",
            1_725_100_004,
        );

        let old_full_path = sync_root.root_path.join(old_path.replace('/', "\\"));
        let new_full_path = sync_root.root_path.join(new_path.replace('/', "\\"));
        fs::create_dir_all(
            new_full_path
                .parent()
                .expect("renamed placeholder should have a parent"),
        )
        .expect("destination parent should be created");
        fs::rename(&old_full_path, &new_full_path).expect("local rename should succeed");
        promote_remote_to_in_sync_content_baseline(
            &sync_root.root_path,
            new_path,
            provider_instance_id,
        )
        .expect("accepted local rename should persist its destination path");
        let renamed_file = open_sync_path(&new_full_path, true)
            .expect("renamed placeholder should remain openable");
        cf_set_in_sync(&renamed_file).expect("renamed placeholder should remain in sync");

        let report = reconcile_existing_placeholders(
            &sync_root.root_path,
            &SyncSnapshot {
                local: Vec::new(),
                // The rename has already been accepted by the server, but
                // this startup snapshot still reflects its old state.
                remote: vec![remote_file(
                    old_path,
                    object_id,
                    "revision-1",
                    "local-rename",
                )],
            },
            provider_instance_id,
        )
        .expect("stale restart reconciliation should succeed conservatively");

        assert!(report.conflicted_paths.contains(new_path));
        assert!(report.preserved_paths.contains(new_path));
        assert!(new_full_path.exists());
        assert!(!old_full_path.exists());
    }

    #[test]
    fn legacy_placeholder_is_migrated_only_on_exact_path_and_revision_match() {
        let (sync_root, provider_instance_id) = registered_test_sync_root("legacy-migration");
        let path = "docs/legacy.txt";
        let object_id = "object-legacy-hash";
        create_clean_provider_placeholder(
            &sync_root.root_path,
            provider_instance_id,
            path,
            "revision-1",
            "legacy-hash",
            1_725_100_005,
        );
        let full_path = sync_root.root_path.join("docs\\legacy.txt");
        let file = open_sync_path(&full_path, true).expect("legacy placeholder should open");
        let legacy_identity = format!(
            "v=2\np={path}\nrv=revision-1\nrh=legacy-hash\ncf=legacy-fingerprint\npi={provider_instance_id}"
        );
        cf_update_placeholder_file_identity(&file, legacy_identity.as_bytes())
            .expect("test should install a legacy identity");
        drop(file);

        let report = reconcile_existing_placeholders(
            &sync_root.root_path,
            &SyncSnapshot {
                local: Vec::new(),
                remote: vec![remote_file(path, object_id, "revision-1", "legacy-hash")],
            },
            provider_instance_id,
        )
        .expect("legacy placeholder should reconcile");

        assert!(report.migrated_paths.contains(path));
        let file = open_sync_path(&full_path, false).expect("migrated placeholder should open");
        let info = cf_get_placeholder_standard_info_with_identity(&file)
            .expect("migrated identity should be readable");
        let identity = decode_placeholder_file_identity(info.file_identity())
            .expect("migrated identity should decode");
        assert_eq!(identity.object_id.as_deref(), Some(object_id));
    }

    #[test]
    fn legacy_server_head_placeholder_migrates_when_its_content_baseline_matches() {
        let (sync_root, provider_instance_id) =
            registered_test_sync_root("legacy-server-head-migration");
        let path = "docs/legacy-server-head.txt";
        let object_id = "object-legacy-server-head";
        create_clean_provider_placeholder(
            &sync_root.root_path,
            provider_instance_id,
            path,
            "revision-1",
            "legacy-server-head-hash",
            1_725_100_005,
        );
        let full_path = sync_root.root_path.join(path.replace('/', "\\"));
        let file = open_sync_path(&full_path, true).expect("legacy placeholder should open");
        let legacy_identity = format!(
            "v=2\np={path}\nrv=server-head\nrh=legacy-server-head-hash\ncf=legacy-fingerprint\npi={provider_instance_id}"
        );
        cf_update_placeholder_file_identity(&file, legacy_identity.as_bytes())
            .expect("test should install a synthetic legacy identity");
        drop(file);

        let report = reconcile_existing_placeholders(
            &sync_root.root_path,
            &SyncSnapshot {
                local: Vec::new(),
                remote: vec![remote_file(
                    path,
                    object_id,
                    "revision-2",
                    "legacy-server-head-hash",
                )],
            },
            provider_instance_id,
        )
        .expect("synthetic legacy placeholder should reconcile by content baseline");

        assert!(report.migrated_paths.contains(path));
        let file = open_sync_path(&full_path, false).expect("migrated placeholder should open");
        let info = cf_get_placeholder_standard_info_with_identity(&file)
            .expect("migrated identity should be readable");
        let identity = decode_placeholder_file_identity(info.file_identity())
            .expect("migrated identity should decode");
        assert_eq!(identity.object_id.as_deref(), Some(object_id));
        assert_eq!(identity.remote_version.as_deref(), Some("revision-2"));
    }

    #[test]
    fn legacy_placeholder_with_mismatched_revision_is_preserved_unbound() {
        let (sync_root, provider_instance_id) =
            registered_test_sync_root("legacy-migration-conflict");
        let path = "docs/legacy-conflict.txt";
        create_clean_provider_placeholder(
            &sync_root.root_path,
            provider_instance_id,
            path,
            "revision-1",
            "legacy-conflict-hash",
            1_725_100_005,
        );
        let full_path = sync_root.root_path.join("docs/legacy-conflict.txt");
        let file = open_sync_path(&full_path, true).expect("legacy placeholder should open");
        let legacy_identity = format!(
            "v=2\np={path}\nrv=revision-1\nrh=legacy-conflict-hash\ncf=legacy-fingerprint\npi={provider_instance_id}"
        );
        cf_update_placeholder_file_identity(&file, legacy_identity.as_bytes())
            .expect("test should install a legacy identity");
        drop(file);

        let report = reconcile_existing_placeholders(
            &sync_root.root_path,
            &SyncSnapshot {
                local: Vec::new(),
                remote: vec![remote_file(
                    path,
                    "object-legacy-conflict",
                    "revision-2",
                    "legacy-conflict-hash",
                )],
            },
            provider_instance_id,
        )
        .expect("unsafe legacy binding should be preserved as a conflict");

        assert!(report.conflicted_paths.contains(path));
        let file = open_sync_path(&full_path, false).expect("legacy placeholder should remain");
        let info = cf_get_placeholder_standard_info_with_identity(&file)
            .expect("legacy identity should remain readable");
        let identity = decode_placeholder_file_identity(info.file_identity())
            .expect("legacy identity should decode");
        assert!(identity.object_id.is_none());
        assert_eq!(identity.remote_version.as_deref(), Some("revision-1"));
    }

    #[test]
    fn simultaneous_local_and_remote_change_preserves_local_placeholder() {
        let (sync_root, provider_instance_id) = registered_test_sync_root("concurrent-change");
        let old_path = "docs/local-change.txt";
        let new_path = "archive/remote-change.txt";
        let object_id = "object-concurrent-hash";
        create_clean_provider_placeholder(
            &sync_root.root_path,
            provider_instance_id,
            old_path,
            "revision-1",
            "concurrent-hash",
            1_725_100_006,
        );
        let local_file = open_sync_path(&sync_root.root_path.join("docs\\local-change.txt"), true)
            .expect("local placeholder should open");
        crate::cfapi::cf_set_not_in_sync(&local_file).expect("test should mark local data dirty");
        drop(local_file);
        let previous = remote_file(old_path, object_id, "revision-1", "concurrent-hash");
        let current = remote_file(new_path, object_id, "revision-2", "remote-hash");
        let resolver = StaticResolver {
            objects: HashMap::from([(
                object_id.to_string(),
                ResolvedRemoteObject {
                    object_id: object_id.to_string(),
                    path: new_path.to_string(),
                    revision: Some("revision-2".to_string()),
                },
            )]),
        };

        let report = reconcile_remote_object_state(
            &sync_root.root_path,
            &SyncSnapshot {
                local: Vec::new(),
                remote: vec![current.clone()],
            },
            &[RemoteObjectChange::Renamed { previous, current }],
            provider_instance_id,
            Some(&resolver),
        )
        .expect("concurrent changes should be classified");

        assert!(report.conflicted_paths.contains(old_path));
        assert!(sync_root.root_path.join("docs\\local-change.txt").exists());
        assert!(
            !sync_root
                .root_path
                .join("archive\\remote-change.txt")
                .exists()
        );
    }

    #[test]
    fn reconcile_remote_delete_preserves_local_only_plain_files() {
        let sync_root =
            std::env::temp_dir().join(format!("ironmesh-placeholder-meta-{}", Uuid::now_v7()));
        fs::create_dir_all(&sync_root).expect("sync root should exist");
        let full_path = sync_root.join("notes.txt");
        fs::write(&full_path, b"offline local").expect("local file should exist");

        let report = reconcile_existing_placeholders(
            &sync_root,
            &SyncSnapshot {
                local: Vec::new(),
                remote: vec![NamespaceEntry::file("other.txt", "v1", "h1")],
            },
            Uuid::now_v7(),
        )
        .expect("reconcile should succeed");

        assert!(report.deleted_paths.is_empty());
        assert!(full_path.exists());

        let _ = fs::remove_dir_all(sync_root);
    }

    #[test]
    fn reconcile_absence_without_tombstone_preserves_clean_placeholder() {
        let (sync_root, provider_instance_id) =
            registered_test_sync_root("stale-snapshot-removes-newer-placeholder");
        let path = "holiday/newer-photo.jpg";
        let remote_version = "newer-revision";
        let remote_content_hash = "newer-content-hash";
        let remote_modified_at_unix = 1_725_000_001;
        create_clean_provider_placeholder(
            &sync_root.root_path,
            provider_instance_id,
            path,
            remote_version,
            remote_content_hash,
            remote_modified_at_unix,
        );

        let full_path = sync_root.root_path.join("holiday\\newer-photo.jpg");
        let file = open_sync_path(&full_path, false).expect("placeholder should be reopenable");
        let info = cf_get_placeholder_standard_info_with_identity(&file)
            .expect("placeholder identity should be available");
        let identity = decode_placeholder_file_identity(info.file_identity())
            .expect("placeholder identity should decode");
        assert_eq!(identity.remote_version.as_deref(), Some(remote_version));
        assert_eq!(
            identity.remote_content_hash.as_deref(),
            Some(remote_content_hash)
        );
        assert_eq!(
            identity.remote_modified_at_unix,
            Some(remote_modified_at_unix)
        );

        // This models a remote listing fetched before the local upload created
        // `path`. It has an unrelated current change, but no tombstone for this
        // placeholder's revision, hash, or timestamp.
        let report = reconcile_existing_placeholders(
            &sync_root.root_path,
            &remote_snapshot_with_small_unrelated_change(),
            provider_instance_id,
        )
        .expect("current reconciliation should complete");

        assert!(report.deleted_paths.is_empty());
        assert!(report.preserved_paths.contains(path));
        assert!(full_path.exists());
    }

    #[test]
    fn reconcile_does_not_scan_global_absence_for_small_unrelated_change() {
        let (sync_root, provider_instance_id) =
            registered_test_sync_root("global-absence-deletes-unrelated-paths");
        let first_path = "holiday/first-newer-file.jpg";
        let second_path = "holiday/second-newer-file.mp4";
        create_clean_provider_placeholder(
            &sync_root.root_path,
            provider_instance_id,
            first_path,
            "first-revision",
            "first-hash",
            1_725_000_002,
        );
        create_clean_provider_placeholder(
            &sync_root.root_path,
            provider_instance_id,
            second_path,
            "second-revision",
            "second-hash",
            1_725_000_003,
        );

        let report = reconcile_existing_placeholders(
            &sync_root.root_path,
            &remote_snapshot_with_small_unrelated_change(),
            provider_instance_id,
        )
        .expect("current reconciliation should complete");

        assert!(report.deleted_paths.is_empty());
        assert!(
            sync_root
                .root_path
                .join("holiday\\first-newer-file.jpg")
                .exists()
        );
        assert!(
            sync_root
                .root_path
                .join("holiday\\second-newer-file.mp4")
                .exists()
        );
    }

    #[test]
    fn desired_behavior_reconcile_remote_delete_preserves_newer_placeholder_when_stale_snapshot_has_no_tombstone()
     {
        let (sync_root, provider_instance_id) =
            registered_test_sync_root("stale-snapshot-must-preserve-placeholder");
        let path = "holiday/newer-photo.jpg";
        create_clean_provider_placeholder(
            &sync_root.root_path,
            provider_instance_id,
            path,
            "newer-revision",
            "newer-content-hash",
            1_725_000_004,
        );
        let full_path = sync_root.root_path.join("holiday\\newer-photo.jpg");

        let report = reconcile_existing_placeholders(
            &sync_root.root_path,
            &remote_snapshot_with_small_unrelated_change(),
            provider_instance_id,
        )
        .expect("reconciliation should complete");

        assert!(full_path.exists(), "stale absence must preserve local data");
        assert!(report.deleted_paths.is_empty());
    }

    #[test]
    fn desired_behavior_reconcile_remote_delete_preserves_paths_unrelated_to_the_remote_change_set()
    {
        let (sync_root, provider_instance_id) =
            registered_test_sync_root("small-change-set-must-not-delete-unrelated-paths");
        let first_path = "holiday/first-newer-file.jpg";
        let second_path = "holiday/second-newer-file.mp4";
        create_clean_provider_placeholder(
            &sync_root.root_path,
            provider_instance_id,
            first_path,
            "first-revision",
            "first-hash",
            1_725_000_005,
        );
        create_clean_provider_placeholder(
            &sync_root.root_path,
            provider_instance_id,
            second_path,
            "second-revision",
            "second-hash",
            1_725_000_006,
        );

        let report = reconcile_existing_placeholders(
            &sync_root.root_path,
            &remote_snapshot_with_small_unrelated_change(),
            provider_instance_id,
        )
        .expect("reconciliation should complete");

        let first_path_exists = sync_root
            .root_path
            .join("holiday\\first-newer-file.jpg")
            .exists();
        let second_path_exists = sync_root
            .root_path
            .join("holiday\\second-newer-file.mp4")
            .exists();
        assert!(
            first_path_exists && second_path_exists && report.deleted_paths.is_empty(),
            "paths unrelated to a remote change must remain locally available"
        );
    }
}
