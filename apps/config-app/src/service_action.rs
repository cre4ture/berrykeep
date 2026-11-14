//! One transaction spans the process check, launch/stop, and launch-report write.
//! A file lock also protects against a second config-app process using the same store.

use std::fs::{self, File, OpenOptions, TryLockError};

use anyhow::Context;

use super::{ApiError, ManagedPaths};

pub(super) fn acquire(paths: &ManagedPaths) -> Result<File, ApiError> {
    let path = paths
        .launch_report_path()
        .with_extension("service-action.lock");
    acquire_lock(
        &path,
        "service_action_in_progress",
        "Another service action is in progress. Wait for it to finish before trying again.",
    )
}

pub(super) fn acquire_store(paths: &ManagedPaths) -> Result<File, ApiError> {
    let path = paths
        .instance_store_path()
        .with_extension("store-write.lock");
    acquire_lock(
        &path,
        "configuration_update_in_progress",
        "Another configuration update is in progress. Please retry.",
    )
}

fn acquire_lock(
    path: &std::path::Path,
    code: &'static str,
    message: &'static str,
) -> Result<File, ApiError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| {
                format!(
                    "failed creating service action directory {}",
                    parent.display()
                )
            })
            .map_err(ApiError::internal)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("failed opening service action lock {}", path.display()))
        .map_err(ApiError::internal)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(ApiError {
            status: axum::http::StatusCode::CONFLICT,
            message: message.to_string(),
            code: Some(code),
            path_preparation: None,
        }),
        Err(TryLockError::Error(error)) => {
            Err(ApiError::internal(anyhow::Error::new(error).context(
                format!("failed locking service actions at {}", path.display()),
            )))
        }
    }
}
