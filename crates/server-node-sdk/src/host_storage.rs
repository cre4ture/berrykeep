use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sysinfo::Disks;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

/// Inventory supplied by the host-storage agent that runs within the node process.
///
/// It deliberately exposes only mounted filesystems. Mounting, formatting and service
/// lifecycle operations remain the responsibility of the host administrator.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct HostStorageInventoryResponse {
    pub platform: &'static str,
    pub volumes: Vec<HostStorageVolume>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct HostStorageVolume {
    pub name: String,
    pub mount_path: String,
    pub file_system: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub removable: bool,
    pub read_only: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PrepareHostStorageDirectoryRequest {
    pub mount_path: String,
    pub directory_name: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct PrepareHostStorageDirectoryResponse {
    pub mount_path: String,
    pub path: String,
    pub directory_created: bool,
    pub write_check: HostStorageWriteCheck,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HostStorageWriteCheck {
    Passed,
}

pub(crate) fn inventory() -> HostStorageInventoryResponse {
    let disks = Disks::new_with_refreshed_list();
    let mut volumes = disks
        .list()
        .iter()
        .map(|disk| HostStorageVolume {
            name: disk.name().to_string_lossy().into_owned(),
            mount_path: disk.mount_point().display().to_string(),
            file_system: disk.file_system().to_string_lossy().into_owned(),
            total_bytes: disk.total_space(),
            available_bytes: disk.available_space(),
            removable: disk.is_removable(),
            read_only: disk.is_read_only(),
        })
        .collect::<Vec<_>>();
    volumes.sort_by(|left, right| left.mount_path.cmp(&right.mount_path));

    HostStorageInventoryResponse {
        platform: std::env::consts::OS,
        volumes,
    }
}

pub(crate) async fn prepare_directory(
    request: &PrepareHostStorageDirectoryRequest,
) -> Result<PrepareHostStorageDirectoryResponse> {
    let volume_inventory = inventory();
    prepare_directory_from_volumes(&volume_inventory.volumes, request).await
}

async fn prepare_directory_from_volumes(
    volumes: &[HostStorageVolume],
    request: &PrepareHostStorageDirectoryRequest,
) -> Result<PrepareHostStorageDirectoryResponse> {
    let mount_path = PathBuf::from(request.mount_path.trim());
    if mount_path.as_os_str().is_empty() || !mount_path.is_absolute() {
        bail!("select a mounted volume reported by the host storage agent");
    }
    let Some(volume) = volumes
        .iter()
        .find(|volume| Path::new(&volume.mount_path) == mount_path)
    else {
        bail!(
            "the selected volume is no longer mounted; refresh the volume list and select it again"
        );
    };
    if volume.read_only {
        bail!("the selected volume is read-only");
    }

    let directory_name = validate_directory_name(&request.directory_name)?;
    let target_path = mount_path.join(directory_name);
    let directory_created = match fs::try_exists(&target_path).await? {
        true => {
            let metadata = fs::symlink_metadata(&target_path).await.with_context(|| {
                format!(
                    "failed reading selected storage directory {}",
                    target_path.display()
                )
            })?;
            if metadata.file_type().is_symlink() {
                bail!(
                    "the selected storage path {} must not be a symbolic link",
                    target_path.display()
                );
            }
            if !metadata.is_dir() {
                bail!(
                    "the selected storage path {} already exists but is not a directory",
                    target_path.display()
                );
            }
            false
        }
        false => {
            fs::create_dir(&target_path).await.with_context(|| {
                format!(
                    "failed creating storage directory {} as the IronMesh service account",
                    target_path.display()
                )
            })?;
            true
        }
    };

    if let Err(error) = verify_target_is_within_mount(&mount_path, &target_path).await {
        if directory_created {
            let _ = fs::remove_dir(&target_path).await;
        }
        return Err(error);
    }
    if let Err(error) = write_probe(&target_path).await {
        if directory_created {
            let _ = fs::remove_dir(&target_path).await;
        }
        return Err(error);
    }

    Ok(PrepareHostStorageDirectoryResponse {
        mount_path: volume.mount_path.clone(),
        path: target_path.display().to_string(),
        directory_created,
        write_check: HostStorageWriteCheck::Passed,
    })
}

fn validate_directory_name(value: &str) -> Result<&str> {
    let directory_name = value.trim();
    if directory_name.is_empty()
        || directory_name.len() > 64
        || matches!(directory_name, "." | "..")
        || !directory_name.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
    {
        bail!(
            "the storage directory name must be 1–64 ASCII letters, digits, '.', '_' or '-' and cannot be '.' or '..'"
        );
    }
    Ok(directory_name)
}

async fn verify_target_is_within_mount(mount_path: &Path, target_path: &Path) -> Result<()> {
    let canonical_mount = fs::canonicalize(mount_path).await.with_context(|| {
        format!(
            "failed resolving selected mounted volume {}",
            mount_path.display()
        )
    })?;
    let canonical_target = fs::canonicalize(target_path).await.with_context(|| {
        format!(
            "failed resolving selected storage directory {}",
            target_path.display()
        )
    })?;
    if !canonical_target.starts_with(&canonical_mount) {
        bail!(
            "the selected storage directory {} resolves outside the mounted volume",
            target_path.display()
        );
    }
    Ok(())
}

async fn write_probe(target_path: &Path) -> Result<()> {
    let probe_path = target_path.join(format!(".ironmesh-write-check-{}.tmp", Uuid::new_v4()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe_path)
        .await
        .with_context(|| {
            format!(
                "the IronMesh service account cannot create a write-check file in {}",
                target_path.display()
            )
        })?;
    file.write_all(b"ironmesh storage preflight\n")
        .await
        .with_context(|| {
            format!(
                "failed writing the storage check in {}",
                target_path.display()
            )
        })?;
    file.sync_data().await.with_context(|| {
        format!(
            "failed syncing the storage check in {}",
            target_path.display()
        )
    })?;
    drop(file);
    fs::remove_file(&probe_path).await.with_context(|| {
        format!(
            "failed removing the storage check in {}",
            target_path.display()
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        HostStorageVolume, PrepareHostStorageDirectoryRequest, prepare_directory_from_volumes,
    };
    use std::path::PathBuf;
    use uuid::Uuid;

    fn test_volume(root: &std::path::Path) -> HostStorageVolume {
        HostStorageVolume {
            name: "test volume".to_string(),
            mount_path: root.display().to_string(),
            file_system: "testfs".to_string(),
            total_bytes: 1,
            available_bytes: 1,
            removable: false,
            read_only: false,
        }
    }

    #[tokio::test]
    async fn prepares_a_safe_child_directory_and_checks_writes() {
        let root = std::env::temp_dir().join(format!("ironmesh-host-storage-{}", Uuid::new_v4()));
        tokio::fs::create_dir(&root).await.unwrap();
        let request = PrepareHostStorageDirectoryRequest {
            mount_path: root.display().to_string(),
            directory_name: "ironmesh-data".to_string(),
        };

        let response = prepare_directory_from_volumes(&[test_volume(&root)], &request)
            .await
            .unwrap();

        assert!(response.directory_created);
        assert_eq!(
            response.path,
            root.join("ironmesh-data").display().to_string()
        );
        assert!(
            tokio::fs::try_exists(PathBuf::from(&response.path))
                .await
                .unwrap()
        );
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn rejects_a_path_that_is_not_a_reported_mount() {
        let root = std::env::temp_dir().join(format!("ironmesh-host-storage-{}", Uuid::new_v4()));
        tokio::fs::create_dir(&root).await.unwrap();
        let request = PrepareHostStorageDirectoryRequest {
            mount_path: root.join("other").display().to_string(),
            directory_name: "ironmesh-data".to_string(),
        };

        let error = prepare_directory_from_volumes(&[test_volume(&root)], &request)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("no longer mounted"));
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn rejects_a_directory_name_that_could_escape_the_mount() {
        let root = std::env::temp_dir().join(format!("ironmesh-host-storage-{}", Uuid::new_v4()));
        tokio::fs::create_dir(&root).await.unwrap();
        let request = PrepareHostStorageDirectoryRequest {
            mount_path: root.display().to_string(),
            directory_name: "../outside".to_string(),
        };

        let error = prepare_directory_from_volumes(&[test_volume(&root)], &request)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("storage directory name"));
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}
