#![cfg(windows)]

pub mod adapter;
pub mod auth;
pub mod cfapi;
pub(crate) mod cfapi_safe_wrap;
pub mod cli;
pub(crate) mod close_upload;
pub mod connection_config;
pub(crate) mod content_fingerprint;
pub mod helpers;
pub mod hydration_control;
pub mod live;
pub(crate) mod local_state;
pub mod monitor;
pub mod placeholder_metadata;
pub mod register;
pub mod runtime;
pub mod snapshot_cache;
pub mod sync_root_identity;
pub(crate) mod windows_status;

#[cfg(test)]
static SYNC_ROOT_REGISTRATION_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub(crate) fn lock_sync_root_registration_tests() -> std::sync::MutexGuard<'static, ()> {
    SYNC_ROOT_REGISTRATION_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
