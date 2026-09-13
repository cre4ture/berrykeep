#![cfg(windows)]

use std::path::{Path, PathBuf};

const LOCAL_STATE_ROOT_DIR: &str = "BerryKeep";
const LEGACY_LOCAL_STATE_ROOT_DIR: &str = "Ironmesh";
const LOCAL_STATE_SYNC_ROOTS_DIR: &str = "sync-roots";
const LOCAL_STATE_CONNECTION_BOOTSTRAP_FILE_NAME: &str = "connection-bootstrap.json";
const LOCAL_STATE_CLIENT_IDENTITY_FILE_NAME: &str = "client-identity.json";
const LOCAL_STATE_DESKTOP_STATUS_FILE_NAME: &str = "desktop-status.json";

pub(crate) fn local_appdata_sync_root_state_dir(sync_root_path: &Path) -> PathBuf {
    local_appdata_sync_root_state_dir_in(local_appdata_base_dir(), sync_root_path)
}

fn local_appdata_sync_root_state_dir_in(
    local_appdata_base_dir: PathBuf,
    sync_root_path: &Path,
) -> PathBuf {
    let state_label = sync_root_state_label(sync_root_path);
    let canonical_state_dir = local_appdata_root(&local_appdata_base_dir, LOCAL_STATE_ROOT_DIR)
        .join(LOCAL_STATE_SYNC_ROOTS_DIR)
        .join(&state_label);
    let legacy_state_dir = local_appdata_root(&local_appdata_base_dir, LEGACY_LOCAL_STATE_ROOT_DIR)
        .join(LOCAL_STATE_SYNC_ROOTS_DIR)
        .join(state_label);
    if canonical_state_dir.exists() || !legacy_state_dir.exists() {
        return canonical_state_dir;
    }

    let Some(canonical_parent) = canonical_state_dir.parent() else {
        return legacy_state_dir;
    };
    if std::fs::create_dir_all(canonical_parent).is_ok()
        && std::fs::rename(&legacy_state_dir, &canonical_state_dir).is_ok()
    {
        canonical_state_dir
    } else {
        // Keep the existing state usable when a filesystem prevents the
        // one-shot migration; a later invocation can retry the move.
        legacy_state_dir
    }
}

pub(crate) fn local_appdata_connection_bootstrap_path(sync_root_path: &Path) -> PathBuf {
    local_appdata_sync_root_state_dir(sync_root_path)
        .join(LOCAL_STATE_CONNECTION_BOOTSTRAP_FILE_NAME)
}

pub(crate) fn local_appdata_client_identity_path(sync_root_path: &Path) -> PathBuf {
    local_appdata_sync_root_state_dir(sync_root_path).join(LOCAL_STATE_CLIENT_IDENTITY_FILE_NAME)
}

pub(crate) fn local_appdata_desktop_status_path(sync_root_path: &Path) -> PathBuf {
    local_appdata_sync_root_state_dir(sync_root_path).join(LOCAL_STATE_DESKTOP_STATUS_FILE_NAME)
}

fn local_appdata_base_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

fn local_appdata_root(local_appdata_base_dir: &Path, product_directory: &str) -> PathBuf {
    local_appdata_base_dir.join(product_directory)
}

fn sync_root_state_label(sync_root_path: &Path) -> String {
    let normalized = sync_root_path
        .to_string_lossy()
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_ascii_lowercase();
    let hash = blake3::hash(normalized.as_bytes()).to_hex().to_string();
    let leaf = sync_root_path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("sync-root");
    let sanitized_leaf = leaf
        .chars()
        .map(|value| {
            if value.is_ascii_alphanumeric() {
                value.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .to_string();
    let label = if sanitized_leaf.is_empty() {
        "sync_root".to_string()
    } else {
        sanitized_leaf
    };
    format!("{label}-{hash}")
}

#[cfg(test)]
mod tests {
    use super::{
        local_appdata_client_identity_path, local_appdata_connection_bootstrap_path,
        local_appdata_desktop_status_path,
    };
    use std::path::Path;

    #[test]
    fn local_appdata_state_paths_are_stable_for_sync_root() {
        let sync_root = Path::new(r"C:\Users\Example\BerryKeep\Wiz3");
        let bootstrap = local_appdata_connection_bootstrap_path(sync_root);
        let identity = local_appdata_client_identity_path(sync_root);

        assert_eq!(
            bootstrap.file_name().and_then(|value| value.to_str()),
            Some("connection-bootstrap.json")
        );
        assert_eq!(
            identity.file_name().and_then(|value| value.to_str()),
            Some("client-identity.json")
        );
        assert_eq!(
            local_appdata_desktop_status_path(sync_root)
                .file_name()
                .and_then(|value| value.to_str()),
            Some("desktop-status.json")
        );
        assert_eq!(bootstrap.parent(), identity.parent());
    }

    #[test]
    fn local_appdata_state_migrates_the_legacy_sync_root_directory() {
        let temporary_root = std::env::temp_dir().join(format!(
            "berrykeep-windows-local-state-{}",
            uuid::Uuid::now_v7()
        ));
        let sync_root = Path::new(r"C:\\Users\\Example\\BerryKeep\\Wiz3");
        let state_label = super::sync_root_state_label(sync_root);
        let legacy_dir = temporary_root
            .join(super::LEGACY_LOCAL_STATE_ROOT_DIR)
            .join(super::LOCAL_STATE_SYNC_ROOTS_DIR)
            .join(&state_label);
        std::fs::create_dir_all(&legacy_dir).expect("legacy state directory should exist");
        std::fs::write(legacy_dir.join("state-marker"), b"existing state")
            .expect("legacy state should be written");

        let state_dir =
            super::local_appdata_sync_root_state_dir_in(temporary_root.clone(), sync_root);

        assert!(state_dir.starts_with(temporary_root.join(super::LOCAL_STATE_ROOT_DIR)));
        assert_eq!(
            std::fs::read(state_dir.join("state-marker")).expect("state should migrate"),
            b"existing state"
        );
        assert!(!legacy_dir.exists());

        let _ = std::fs::remove_dir_all(temporary_root);
    }
}
