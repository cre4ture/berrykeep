//! One transaction spans the process check, launch/stop, and launch-report write.
//! A file lock also protects against a second config-app process using the same store.

use std::fs::{self, File, OpenOptions, TryLockError};

use super::{ApiError, ManagedPaths};

pub(super) fn acquire(paths: &ManagedPaths) -> Result<File, ApiError> {
    let path = paths
        .launch_report_path()
        .with_extension("service-action.lock");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| ApiError::internal(error.into()))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|error| ApiError::internal(error.into()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(ApiError {
            status: axum::http::StatusCode::CONFLICT,
            message:
                "Another service action is in progress. Wait for it to finish before trying again."
                    .to_string(),
            code: Some("service_action_in_progress"),
            path_preparation: None,
        }),
        Err(TryLockError::Error(error)) => Err(ApiError::internal(error.into())),
    }
}
