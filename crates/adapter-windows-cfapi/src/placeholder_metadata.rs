#![cfg(windows)]

use crate::auth::is_internal_client_identity_relative_path;
use crate::cfapi::{
    cf_ensure_placeholder_identity, cf_get_placeholder_standard_info_with_identity,
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
use client_sdk::{RemoteDeletion, RemoteEntryBaseline};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use sync_core::NamespaceMediaMetadata;
use uuid::Uuid;
use walkdir::WalkDir;
use windows_sys::Win32::Storage::CloudFilters::CF_FS_METADATA;
use windows_sys::Win32::Storage::FileSystem::FILE_BASIC_INFO;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RemoteDeleteReconcileReport {
    pub deleted_paths: BTreeSet<String>,
    /// Directory removals are kept separate so the monitor can suppress their
    /// corresponding local delete event without treating them like file
    /// placeholder removals.
    pub deleted_directory_paths: BTreeSet<String>,
    pub preserved_paths: BTreeSet<String>,
    pub suppressed_startup_paths: BTreeSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteFileMetadataPolicy {
    ApplyAndMarkInSync,
    PreserveLocalConflict,
}

#[derive(Debug, Clone, Copy)]
pub struct RemotePlaceholderState<'a> {
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

pub fn record_uploaded_remote_state(
    sync_root_path: &Path,
    relative_path: &str,
    provider_instance_id: Uuid,
    remote_version: Option<&str>,
    remote_content_hash: Option<&str>,
    in_sync_content_fingerprint: Option<&str>,
) -> Result<()> {
    let normalized = normalize_path(relative_path);
    if normalized.is_empty() || is_internal_sync_root_relative_path(&normalized) {
        return Ok(());
    }

    mutate_placeholder_identity_for_path(sync_root_path, &normalized, None, true, |identity| {
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
        // Upload responses currently provide a version and manifest hash, not
        // a remote timestamp, size, or media payload. Keeping those values
        // from the predecessor would make a valid exact-version tombstone
        // appear contradictory after a fast delete.
        identity.remote_modified_at_unix = None;
        identity.remote_size_bytes = None;
        identity.remote_media = None;
        identity.remote_media_absent = false;
        if let Some(fingerprint) = in_sync_content_fingerprint
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            identity.remote_content_fingerprint = Some(fingerprint.to_string());
            identity.in_sync_content_fingerprint = Some(fingerprint.to_string());
        }
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
    if metadata.is_dir() {
        return Ok(());
    }

    let file_size = remote_size_bytes
        .and_then(|value| i64::try_from(value).ok())
        .or_else(|| i64::try_from(metadata.len()).ok())
        .unwrap_or_default();
    let fs_metadata =
        remote_file_system_metadata(file_metadata_policy, remote_modified_at_unix, file_size);
    let fs_metadata_is_current = match file_metadata_policy {
        RemoteFileMetadataPolicy::ApplyAndMarkInSync => remote_file_system_metadata_is_current(
            &metadata,
            remote_modified_at_unix,
            remote_size_bytes,
        ),
        RemoteFileMetadataPolicy::PreserveLocalConflict => true,
    };

    mutate_placeholder_identity_for_path(
        sync_root_path,
        &normalized,
        fs_metadata,
        fs_metadata_is_current,
        |identity| {
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
        RemoteFileMetadataPolicy::ApplyAndMarkInSync => remote_modified_at_unix
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

/// Collects persisted provider file identities only for local paths absent
/// from the already fetched remote snapshot. This avoids opening every CFAPI
/// placeholder during startup when the normal case is that most files remain
/// visible remotely.
pub fn collect_provider_file_baselines_missing_from_remote(
    sync_root_path: &Path,
    provider_instance_id: Uuid,
    visible_remote_paths: &BTreeSet<String>,
) -> Vec<RemoteEntryBaseline> {
    let mut baselines = Vec::new();
    for entry in WalkDir::new(sync_root_path)
        .min_depth(1)
        .into_iter()
        .flatten()
    {
        if entry.file_type().is_dir() {
            continue;
        }
        let relative_path = path_to_relative(sync_root_path, &entry.path().to_string_lossy());
        if relative_path.is_empty() || is_internal_sync_root_relative_path(&relative_path) {
            continue;
        }
        if visible_remote_paths.contains(&relative_path) {
            continue;
        }
        let Ok(file) = open_sync_path(entry.path(), false) else {
            continue;
        };
        let Ok(info) = cf_get_placeholder_standard_info_with_identity(&file) else {
            continue;
        };
        let Some(identity) = decode_placeholder_file_identity(info.file_identity()) else {
            continue;
        };
        if identity.path != relative_path
            || identity.provider_instance_id != Some(provider_instance_id)
            || !identity_has_remote_baseline(&identity)
        {
            continue;
        }
        baselines.push(remote_entry_baseline(&identity));
    }
    baselines.sort_by(|left, right| left.path.cmp(&right.path));
    baselines
}

/// Applies only server-confirmed deletions.  A missing path in a snapshot is
/// never enough: the tombstone must prove it supersedes the local identity.
pub fn reconcile_remote_delete_state(
    sync_root_path: &Path,
    deletions: &[RemoteDeletion],
    provider_instance_id: Uuid,
    prior_remote_directory_baselines: &BTreeMap<String, RemoteEntryBaseline>,
) -> Result<RemoteDeleteReconcileReport> {
    let mut report = RemoteDeleteReconcileReport::default();
    let mut explicit_deletions = deletions.iter().collect::<Vec<_>>();
    explicit_deletions.sort_by(|left, right| right.path.cmp(&left.path));

    for deletion in explicit_deletions {
        let relative_path = normalize_path(&deletion.path);
        if relative_path.is_empty() || is_internal_sync_root_relative_path(&relative_path) {
            continue;
        }
        let full_path = sync_root_path.join(relative_path.replace('/', "\\"));
        let metadata = match fs::metadata(&full_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                report.preserved_paths.insert(relative_path);
                continue;
            }
        };
        if metadata.is_dir() {
            // Directories have no CFAPI file identity.  We may remove one only
            // when this process previously observed the concrete directory
            // marker revision and the server tombstone proves it supersedes
            // that exact marker.  `remove_dir` below additionally refuses to
            // remove a directory with any remaining local contents.
            let Some(baseline) = prior_remote_directory_baselines.get(&relative_path) else {
                report.preserved_paths.insert(relative_path);
                continue;
            };
            if !deletion.matches_baseline(baseline) {
                report.preserved_paths.insert(relative_path);
                continue;
            }
            match fs::remove_dir(&full_path) {
                Ok(()) => {
                    report.deleted_directory_paths.insert(relative_path);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => {
                    report.preserved_paths.insert(relative_path);
                }
            }
            continue;
        }
        let file = match open_sync_path(&full_path, false) {
            Ok(file) => file,
            Err(_) => {
                report.preserved_paths.insert(relative_path);
                continue;
            }
        };
        let placeholder_info = match cf_get_placeholder_standard_info_with_identity(&file) {
            Ok(info) => info,
            Err(_) => {
                report.preserved_paths.insert(relative_path);
                continue;
            }
        };
        let Some(identity) = decode_placeholder_file_identity(placeholder_info.file_identity())
        else {
            report.preserved_paths.insert(relative_path);
            continue;
        };
        if identity.path != relative_path
            || identity.provider_instance_id != Some(provider_instance_id)
            || !deletion.matches_baseline(&remote_entry_baseline(&identity))
        {
            report.preserved_paths.insert(relative_path);
            continue;
        }

        let matches_in_sync_baseline = if placeholder_info.info().ModifiedDataSize == 0 {
            true
        } else if let Some(in_sync_content_fingerprint) =
            identity.in_sync_content_fingerprint.as_deref()
        {
            file_content_fingerprint(&full_path)
                .map(|current_fingerprint| current_fingerprint == in_sync_content_fingerprint)
                .unwrap_or(false)
        } else {
            false
        };
        if !matches_in_sync_baseline {
            report.preserved_paths.insert(relative_path);
            continue;
        }

        match fs::remove_file(&full_path) {
            Ok(()) => {
                report.deleted_paths.insert(relative_path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                report.suppressed_startup_paths.insert(relative_path);
            }
        }
    }

    Ok(report)
}

fn remote_entry_baseline(identity: &PlaceholderFileIdentity) -> RemoteEntryBaseline {
    RemoteEntryBaseline {
        path: identity.path.clone(),
        version: identity.remote_version.clone(),
        content_hash: identity.remote_content_hash.clone(),
        modified_at_unix: identity.remote_modified_at_unix,
    }
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
    fn reconcile_remote_delete_preserves_local_only_plain_files() {
        let sync_root =
            std::env::temp_dir().join(format!("ironmesh-placeholder-meta-{}", Uuid::now_v7()));
        fs::create_dir_all(&sync_root).expect("sync root should exist");
        let full_path = sync_root.join("notes.txt");
        fs::write(&full_path, b"offline local").expect("local file should exist");

        let report =
            reconcile_remote_delete_state(&sync_root, &[], Uuid::now_v7(), &BTreeMap::new())
                .expect("reconcile should succeed");

        assert!(report.deleted_paths.is_empty());
        assert!(full_path.exists());

        let _ = fs::remove_dir_all(sync_root);
    }

    #[test]
    fn collect_provider_file_baselines_only_reads_missing_remote_paths() {
        let (sync_root, provider_instance_id) =
            registered_test_sync_root("collect-missing-provider-baselines");
        let visible_path = "holiday/visible.jpg";
        let missing_path = "holiday/missing.jpg";
        create_clean_provider_placeholder(
            &sync_root.root_path,
            provider_instance_id,
            visible_path,
            "visible-revision",
            "visible-hash",
            1_725_000_001,
        );
        create_clean_provider_placeholder(
            &sync_root.root_path,
            provider_instance_id,
            missing_path,
            "missing-revision",
            "missing-hash",
            1_725_000_002,
        );

        let visible_remote_paths = BTreeSet::from([visible_path.to_string()]);
        let baselines = collect_provider_file_baselines_missing_from_remote(
            &sync_root.root_path,
            provider_instance_id,
            &visible_remote_paths,
        );

        assert_eq!(baselines.len(), 1);
        assert_eq!(baselines[0].path, missing_path);
        assert_eq!(baselines[0].version.as_deref(), Some("missing-revision"));
    }

    #[test]
    fn reconcile_remote_delete_requires_a_matching_tombstone_predecessor() {
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

        let report = reconcile_remote_delete_state(
            &sync_root.root_path,
            &[RemoteDeletion {
                path: path.to_string(),
                tombstone_version: "delete-revision".to_string(),
                tombstone_created_at_unix: remote_modified_at_unix + 1,
                predecessors: vec![RemoteEntryBaseline {
                    path: path.to_string(),
                    version: Some(remote_version.to_string()),
                    content_hash: Some(remote_content_hash.to_string()),
                    modified_at_unix: Some(remote_modified_at_unix),
                }],
            }],
            provider_instance_id,
            &BTreeMap::new(),
        )
        .expect("current reconciliation should complete");

        assert!(report.deleted_paths.contains(path));
        assert!(!full_path.exists());
    }

    #[test]
    fn reconcile_remote_delete_does_not_scan_paths_without_explicit_tombstones() {
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

        let report = reconcile_remote_delete_state(
            &sync_root.root_path,
            &[],
            provider_instance_id,
            &BTreeMap::new(),
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
    fn reconcile_remote_delete_preserves_newer_placeholder_without_tombstone() {
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

        let report = reconcile_remote_delete_state(
            &sync_root.root_path,
            &[],
            provider_instance_id,
            &BTreeMap::new(),
        )
        .expect("reconciliation should complete");

        assert!(full_path.exists(), "stale absence must preserve local data");
        assert!(report.deleted_paths.is_empty());
    }

    #[test]
    fn reconcile_remote_delete_removes_only_empty_directories_with_matching_prior_marker() {
        let root = std::env::temp_dir().join(format!(
            "ironmesh-placeholder-directory-delete-{}",
            Uuid::new_v4()
        ));
        let root_path = "remote-folder-move/from";
        let nested_path = "remote-folder-move/from/nested";
        fs::create_dir_all(root.join(nested_path.replace('/', "\\")))
            .expect("remote directory tree should exist");

        let baselines = BTreeMap::from([
            (
                root_path.to_string(),
                RemoteEntryBaseline {
                    path: root_path.to_string(),
                    version: Some("root-marker-revision".to_string()),
                    content_hash: Some("root-marker-hash".to_string()),
                    modified_at_unix: Some(1_725_000_010),
                },
            ),
            (
                nested_path.to_string(),
                RemoteEntryBaseline {
                    path: nested_path.to_string(),
                    version: Some("nested-marker-revision".to_string()),
                    content_hash: Some("nested-marker-hash".to_string()),
                    modified_at_unix: Some(1_725_000_011),
                },
            ),
        ]);
        let deletion = |path: &str, version: &str, hash: &str, modified_at_unix| RemoteDeletion {
            path: path.to_string(),
            tombstone_version: format!("tombstone-{version}"),
            tombstone_created_at_unix: modified_at_unix + 1,
            predecessors: vec![RemoteEntryBaseline {
                path: path.to_string(),
                version: Some(version.to_string()),
                content_hash: Some(hash.to_string()),
                modified_at_unix: Some(modified_at_unix),
            }],
        };

        let report = reconcile_remote_delete_state(
            &root,
            &[
                deletion(
                    root_path,
                    "root-marker-revision",
                    "root-marker-hash",
                    1_725_000_010,
                ),
                deletion(
                    nested_path,
                    "nested-marker-revision",
                    "nested-marker-hash",
                    1_725_000_011,
                ),
            ],
            Uuid::now_v7(),
            &baselines,
        )
        .expect("directory reconciliation should complete");

        assert_eq!(
            report.deleted_directory_paths,
            BTreeSet::from([root_path.to_string(), nested_path.to_string()]),
            "the reverse path ordering must remove the empty child before its parent"
        );
        assert!(
            !root.join(root_path.replace('/', "\\")).exists(),
            "a confirmed marker tombstone must not leave the renamed source directory behind"
        );

        fs::create_dir_all(root.join(root_path.replace('/', "\\")))
            .expect("local directory should be recreated for the safety check");
        let report = reconcile_remote_delete_state(
            &root,
            &[deletion(
                root_path,
                "root-marker-revision",
                "root-marker-hash",
                1_725_000_010,
            )],
            Uuid::now_v7(),
            &BTreeMap::new(),
        )
        .expect("directory reconciliation without provenance should complete");
        assert!(root.join(root_path.replace('/', "\\")).exists());
        assert!(report.deleted_directory_paths.is_empty());

        let local_file = root
            .join(root_path.replace('/', "\\"))
            .join("local-only.txt");
        fs::write(&local_file, b"must survive").expect("local-only content should be created");
        let report = reconcile_remote_delete_state(
            &root,
            &[deletion(
                root_path,
                "root-marker-revision",
                "root-marker-hash",
                1_725_000_010,
            )],
            Uuid::now_v7(),
            &baselines,
        )
        .expect("non-empty directory reconciliation should complete");
        assert!(
            local_file.exists(),
            "local-only content must never be removed"
        );
        assert!(report.deleted_directory_paths.is_empty());
        assert!(report.preserved_paths.contains(root_path));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn empty_upload_receipt_downgrades_stale_remote_identity() {
        let (sync_root, provider_instance_id) =
            registered_test_sync_root("empty-upload-receipt-downgrades-baseline");
        let path = "holiday/newer-photo.jpg";
        create_clean_provider_placeholder(
            &sync_root.root_path,
            provider_instance_id,
            path,
            "old-revision",
            "old-content-hash",
            1_725_000_007,
        );

        record_uploaded_remote_state(
            &sync_root.root_path,
            path,
            provider_instance_id,
            None,
            None,
            Some("new-local-content-fingerprint"),
        )
        .expect("identity should be safely downgraded after empty upload receipt");

        let full_path = sync_root.root_path.join("holiday\\newer-photo.jpg");
        let file = open_sync_path(&full_path, false).expect("placeholder should be reopenable");
        let info = cf_get_placeholder_standard_info_with_identity(&file)
            .expect("placeholder identity should be available");
        let identity = decode_placeholder_file_identity(info.file_identity())
            .expect("placeholder identity should decode");
        assert!(identity.remote_version.is_none());
        assert!(identity.remote_content_hash.is_none());
        assert!(identity.remote_modified_at_unix.is_none());
        assert_eq!(
            identity.remote_content_fingerprint.as_deref(),
            Some("new-local-content-fingerprint")
        );
        assert_eq!(
            identity.in_sync_content_fingerprint.as_deref(),
            Some("new-local-content-fingerprint")
        );
    }
}
