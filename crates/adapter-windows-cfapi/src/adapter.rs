use std::collections::HashMap;

use sync_core::{
    NamespaceMediaMetadata, SyncOperation, SyncPlan, SyncPolicy, SyncSnapshot, plan_sync,
};

#[derive(Debug, Clone, PartialEq, Default)]
pub struct RemoteFileMetadata {
    pub object_id: Option<String>,
    pub size_bytes: Option<u64>,
    pub content_fingerprint: Option<String>,
    pub modified_at_unix: Option<u64>,
    pub media: Option<NamespaceMediaMetadata>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsCfapiAdapter {
    pub sync_root_name: String,
}

impl WindowsCfapiAdapter {
    pub fn new(sync_root_name: impl Into<String>) -> Self {
        Self {
            sync_root_name: sync_root_name.into(),
        }
    }

    pub fn plan_actions(&self, snapshot: &SyncSnapshot, policy: &SyncPolicy) -> CfapiActionPlan {
        let sync_plan = plan_sync(snapshot, policy);
        let remote_metadata_by_path = snapshot
            .remote
            .iter()
            .filter(|entry| entry.kind == sync_core::EntryKind::File)
            .map(|entry| {
                (
                    entry.path.clone(),
                    RemoteFileMetadata {
                        object_id: entry.object_id.clone(),
                        size_bytes: entry.size_bytes,
                        content_fingerprint: entry.content_fingerprint.clone(),
                        modified_at_unix: entry.modified_at_unix,
                        media: entry.media.clone(),
                    },
                )
            })
            .collect::<HashMap<_, _>>();
        map_sync_plan_to_cfapi_actions(&sync_plan, &remote_metadata_by_path)
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct CfapiActionPlan {
    pub actions: Vec<CfapiAction>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CfapiAction {
    EnsureDirectory {
        path: String,
    },
    EnsurePlaceholder {
        object_id: Option<String>,
        path: String,
        remote_version: String,
        remote_content_hash: String,
        remote_size: Option<u64>,
        remote_content_fingerprint: Option<String>,
        remote_modified_at_unix: Option<u64>,
        remote_media: Option<NamespaceMediaMetadata>,
    },
    HydrateOnDemand {
        object_id: Option<String>,
        path: String,
        remote_version: String,
        remote_content_hash: String,
        remote_size: Option<u64>,
        remote_content_fingerprint: Option<String>,
        remote_modified_at_unix: Option<u64>,
        remote_media: Option<NamespaceMediaMetadata>,
    },
    QueueUploadOnClose {
        path: String,
        local_version: Option<String>,
    },
    MarkConflict {
        object_id: Option<String>,
        path: String,
        local_version: Option<String>,
        remote_version: Option<String>,
        remote_content_hash: Option<String>,
        remote_size: Option<u64>,
        remote_content_fingerprint: Option<String>,
        remote_modified_at_unix: Option<u64>,
        remote_media: Option<NamespaceMediaMetadata>,
    },
}

pub fn map_sync_plan_to_cfapi_actions(
    sync_plan: &SyncPlan,
    remote_metadata_by_path: &HashMap<String, RemoteFileMetadata>,
) -> CfapiActionPlan {
    let mut actions = Vec::with_capacity(sync_plan.operations.len());

    for operation in &sync_plan.operations {
        let mapped = match operation {
            SyncOperation::CreateDirectory { path } => {
                CfapiAction::EnsureDirectory { path: path.clone() }
            }
            SyncOperation::EnsurePlaceholder {
                path,
                remote_version,
                remote_content_hash,
            } => CfapiAction::EnsurePlaceholder {
                object_id: remote_metadata_by_path
                    .get(path)
                    .and_then(|metadata| metadata.object_id.clone()),
                path: path.clone(),
                remote_version: remote_version.clone(),
                remote_content_hash: remote_content_hash.clone(),
                remote_size: remote_metadata_by_path
                    .get(path)
                    .and_then(|metadata| metadata.size_bytes),
                remote_content_fingerprint: remote_metadata_by_path
                    .get(path)
                    .and_then(|metadata| metadata.content_fingerprint.clone()),
                remote_modified_at_unix: remote_metadata_by_path
                    .get(path)
                    .and_then(|metadata| metadata.modified_at_unix),
                remote_media: remote_metadata_by_path
                    .get(path)
                    .and_then(|metadata| metadata.media.clone()),
            },
            SyncOperation::Hydrate {
                path,
                remote_version,
                remote_content_hash,
            } => CfapiAction::HydrateOnDemand {
                object_id: remote_metadata_by_path
                    .get(path)
                    .and_then(|metadata| metadata.object_id.clone()),
                path: path.clone(),
                remote_version: remote_version.clone(),
                remote_content_hash: remote_content_hash.clone(),
                remote_size: remote_metadata_by_path
                    .get(path)
                    .and_then(|metadata| metadata.size_bytes),
                remote_content_fingerprint: remote_metadata_by_path
                    .get(path)
                    .and_then(|metadata| metadata.content_fingerprint.clone()),
                remote_modified_at_unix: remote_metadata_by_path
                    .get(path)
                    .and_then(|metadata| metadata.modified_at_unix),
                remote_media: remote_metadata_by_path
                    .get(path)
                    .and_then(|metadata| metadata.media.clone()),
            },
            SyncOperation::Upload {
                path,
                local_version,
            } => CfapiAction::QueueUploadOnClose {
                path: path.clone(),
                local_version: local_version.clone(),
            },
            SyncOperation::Conflict {
                path,
                local_version,
                remote_version,
                remote_content_hash,
            } => CfapiAction::MarkConflict {
                object_id: remote_metadata_by_path
                    .get(path)
                    .and_then(|metadata| metadata.object_id.clone()),
                path: path.clone(),
                local_version: local_version.clone(),
                remote_version: remote_version.clone(),
                remote_content_hash: remote_content_hash.clone(),
                remote_size: remote_metadata_by_path
                    .get(path)
                    .and_then(|metadata| metadata.size_bytes),
                remote_content_fingerprint: remote_metadata_by_path
                    .get(path)
                    .and_then(|metadata| metadata.content_fingerprint.clone()),
                remote_modified_at_unix: remote_metadata_by_path
                    .get(path)
                    .and_then(|metadata| metadata.modified_at_unix),
                remote_media: remote_metadata_by_path
                    .get(path)
                    .and_then(|metadata| metadata.media.clone()),
            },
        };

        actions.push(mapped);
    }

    CfapiActionPlan { actions }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sync_core::{HydrationState, LocalEntry, NamespaceEntry, PinState};

    #[test]
    fn adapter_maps_remote_only_file_to_placeholder_action() {
        let adapter = WindowsCfapiAdapter::new("Ironmesh");
        let snapshot = SyncSnapshot {
            local: vec![],
            remote: vec![
                NamespaceEntry::file("docs/readme.md", "v1", "h1").with_object_id("obj-readme"),
            ],
        };

        let plan = adapter.plan_actions(&snapshot, &SyncPolicy::default());

        assert_eq!(
            plan.actions,
            vec![CfapiAction::EnsurePlaceholder {
                object_id: Some("obj-readme".to_string()),
                path: "docs/readme.md".to_string(),
                remote_version: "v1".to_string(),
                remote_content_hash: "h1".to_string(),
                remote_size: None,
                remote_content_fingerprint: None,
                remote_modified_at_unix: None,
                remote_media: None,
            }],
        );
    }

    #[test]
    fn adapter_maps_local_only_file_to_upload_on_close() {
        let adapter = WindowsCfapiAdapter::new("Ironmesh");
        let snapshot = SyncSnapshot {
            local: vec![LocalEntry::new(
                NamespaceEntry::file("notes/task.txt", "v-local", "h-local"),
                PinState::Pinned,
                HydrationState::Hydrated,
            )],
            remote: vec![],
        };

        let plan = adapter.plan_actions(&snapshot, &SyncPolicy::default());

        assert_eq!(
            plan.actions,
            vec![CfapiAction::QueueUploadOnClose {
                path: "notes/task.txt".to_string(),
                local_version: Some("v-local".to_string()),
            }],
        );
    }

    #[test]
    fn adapter_maps_divergence_to_conflict_action() {
        let adapter = WindowsCfapiAdapter::new("Ironmesh");
        let snapshot = SyncSnapshot {
            local: vec![LocalEntry::new(
                NamespaceEntry::file("report.csv", "v-local", "h1"),
                PinState::Pinned,
                HydrationState::Hydrated,
            )],
            remote: vec![
                NamespaceEntry::file("report.csv", "v-remote", "h2").with_object_id("obj-report"),
            ],
        };

        let plan = adapter.plan_actions(&snapshot, &SyncPolicy::default());

        assert_eq!(
            plan.actions,
            vec![CfapiAction::MarkConflict {
                object_id: Some("obj-report".to_string()),
                path: "report.csv".to_string(),
                local_version: Some("v-local".to_string()),
                remote_version: Some("v-remote".to_string()),
                remote_content_hash: Some("h2".to_string()),
                remote_size: None,
                remote_content_fingerprint: None,
                remote_modified_at_unix: None,
                remote_media: None,
            }],
        );
    }

    #[test]
    fn adapter_carries_remote_metadata_for_file_actions() {
        let adapter = WindowsCfapiAdapter::new("Ironmesh");
        let mut remote = NamespaceEntry::file_sized("docs/readme.md", "v1", "h1", Some(42));
        remote.object_id = Some("obj-readme".to_string());
        remote.modified_at_unix = Some(1_723_456_789);
        remote.media = Some(NamespaceMediaMetadata {
            media_type: Some("image".to_string()),
            mime_type: Some("image/jpeg".to_string()),
            width: Some(4_032),
            height: Some(3_024),
            ..Default::default()
        });
        let snapshot = SyncSnapshot {
            local: vec![],
            remote: vec![remote],
        };

        let plan = adapter.plan_actions(&snapshot, &SyncPolicy::default());

        assert_eq!(
            plan.actions,
            vec![CfapiAction::EnsurePlaceholder {
                object_id: Some("obj-readme".to_string()),
                path: "docs/readme.md".to_string(),
                remote_version: "v1".to_string(),
                remote_content_hash: "h1".to_string(),
                remote_size: Some(42),
                remote_content_fingerprint: None,
                remote_modified_at_unix: Some(1_723_456_789),
                remote_media: Some(NamespaceMediaMetadata {
                    media_type: Some("image".to_string()),
                    mime_type: Some("image/jpeg".to_string()),
                    width: Some(4_032),
                    height: Some(3_024),
                    ..Default::default()
                }),
            }],
        );
    }

    #[test]
    fn adapter_maps_remote_directory_to_ensure_directory() {
        let adapter = WindowsCfapiAdapter::new("Ironmesh");
        let snapshot = SyncSnapshot {
            local: vec![],
            remote: vec![NamespaceEntry::directory("nested/dir")],
        };

        let plan = adapter.plan_actions(&snapshot, &SyncPolicy::default());

        assert_eq!(
            plan.actions,
            vec![CfapiAction::EnsureDirectory {
                path: "nested/dir".to_string(),
            }],
        );
    }
}
