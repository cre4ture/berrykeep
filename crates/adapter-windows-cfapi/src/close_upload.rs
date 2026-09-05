use crate::cfapi::{
    cf_ensure_placeholder_identity, cf_get_placeholder_standard_info,
    cf_get_placeholder_standard_info_with_identity, cf_set_in_sync_with_usn, cf_set_not_in_sync,
    cf_update_placeholder_file_identity, describe_path_state,
};
use crate::helpers::{PlaceholderFileIdentity, decode_placeholder_file_identity};
use crate::runtime::{
    CfapiRuntime, UploadReceipt, Uploader, is_remote_mutation_conflict,
    reconcile_ancestor_directory_sync_states,
};
use anyhow::{Context, Result, bail};
use std::collections::HashSet;
use std::io::Write;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

const CLOSE_UPLOAD_QUIET_PERIOD: Duration = Duration::from_millis(750);
const CLOSE_UPLOAD_RETRY_DELAY: Duration = Duration::from_millis(1000);
const CLOSE_UPLOAD_PERMIT_WAIT_LOG_THRESHOLD: Duration = Duration::from_secs(1);
const CLOSE_UPLOAD_TRACE_FILE_ENV: &str = "IRONMESH_CFAPI_CLOSE_UPLOAD_TRACE_FILE";

pub(crate) struct UploadWorkerContext {
    pub(crate) sync_root: PathBuf,
    pub(crate) provider_instance_id: uuid::Uuid,
    pub(crate) runtime: Arc<CfapiRuntime>,
    pub(crate) uploader: Arc<dyn Uploader>,
    pub(crate) upload_gate: Arc<UploadConcurrencyGate>,
}

pub(crate) fn close_upload_max_concurrency_from_env() -> Result<usize> {
    const ENV_NAME: &str = "IRONMESH_CFAPI_CLOSE_UPLOAD_MAX_CONCURRENCY";

    match std::env::var(ENV_NAME) {
        Ok(value) => {
            let parsed = value
                .parse::<usize>()
                .with_context(|| format!("failed parsing {ENV_NAME}={value} as usize"))?;
            if parsed == 0 {
                bail!("{ENV_NAME} must be greater than zero");
            }
            Ok(parsed)
        }
        Err(std::env::VarError::NotPresent) => Ok(default_close_upload_max_concurrency()),
        Err(err) => Err(err).with_context(|| format!("failed reading {ENV_NAME}")),
    }
}

pub(crate) fn default_close_upload_max_concurrency() -> usize {
    std::thread::available_parallelism()
        .map(|parallelism| parallelism.get().clamp(4, 8))
        .unwrap_or(8)
}

pub(crate) struct UploadConcurrencyGate {
    max_active: usize,
    active: Mutex<usize>,
    ready: Condvar,
}

impl UploadConcurrencyGate {
    pub(crate) fn new(max_active: usize) -> Self {
        Self {
            max_active,
            active: Mutex::new(0),
            ready: Condvar::new(),
        }
    }

    fn acquire(self: &Arc<Self>, relative_path: &str) -> UploadConcurrencyPermit {
        let started = Instant::now();
        let mut active = self.active.lock().expect("upload gate lock poisoned");
        while *active >= self.max_active {
            active = self
                .ready
                .wait(active)
                .expect("upload gate lock poisoned while waiting");
        }
        *active += 1;
        let active_now = *active;
        drop(active);

        let waited = started.elapsed();
        if waited >= CLOSE_UPLOAD_PERMIT_WAIT_LOG_THRESHOLD {
            tracing::info!(
                "close-completion: acquired upload permit for {} after {:?} (active={} max_active={})",
                relative_path,
                waited,
                active_now,
                self.max_active
            );
        }

        UploadConcurrencyPermit {
            gate: Arc::clone(self),
        }
    }
}

struct UploadConcurrencyPermit {
    gate: Arc<UploadConcurrencyGate>,
}

impl Drop for UploadConcurrencyPermit {
    fn drop(&mut self) {
        let mut active = self
            .gate
            .active
            .lock()
            .expect("upload gate lock poisoned on release");
        *active = active.saturating_sub(1);
        self.gate.ready.notify_one();
    }
}

#[derive(Default)]
pub(crate) struct UploadDebounceState {
    pending_generations: Mutex<std::collections::HashMap<String, u64>>,
    uploads_in_flight: Mutex<HashSet<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UploadDebounceSnapshot {
    pub pending_count: usize,
    pub uploads_in_flight_count: usize,
    pub path_generation: Option<u64>,
    pub path_in_flight: bool,
    pub pending_paths_sample: Vec<String>,
    pub uploads_in_flight_sample: Vec<String>,
}

impl UploadDebounceSnapshot {
    pub(crate) fn to_log_string(&self) -> String {
        format!(
            "pending_count={} in_flight_count={} path_generation={:?} path_in_flight={} pending_sample={:?} in_flight_sample={:?}",
            self.pending_count,
            self.uploads_in_flight_count,
            self.path_generation,
            self.path_in_flight,
            self.pending_paths_sample,
            self.uploads_in_flight_sample
        )
    }
}

impl UploadDebounceState {
    pub(crate) fn has_in_flight_upload_for_path(&self, relative_path: &str) -> bool {
        self.uploads_in_flight
            .lock()
            .expect("uploads_in_flight lock poisoned")
            .contains(relative_path)
    }

    pub(crate) fn debug_snapshot_for_path(
        &self,
        relative_path: &str,
        sample_limit: usize,
    ) -> UploadDebounceSnapshot {
        let sample_limit = sample_limit.max(1);
        let (pending_count, path_generation, pending_paths_sample) = {
            let pending = self
                .pending_generations
                .lock()
                .expect("pending upload generations lock poisoned");
            let mut pending_paths = pending.keys().cloned().collect::<Vec<_>>();
            pending_paths.sort();
            (
                pending.len(),
                pending.get(relative_path).copied(),
                pending_paths
                    .into_iter()
                    .take(sample_limit)
                    .collect::<Vec<_>>(),
            )
        };
        let (uploads_in_flight_count, path_in_flight, uploads_in_flight_sample) = {
            let uploads_in_flight = self
                .uploads_in_flight
                .lock()
                .expect("uploads_in_flight lock poisoned");
            let mut in_flight_paths = uploads_in_flight.iter().cloned().collect::<Vec<_>>();
            in_flight_paths.sort();
            (
                uploads_in_flight.len(),
                uploads_in_flight.contains(relative_path),
                in_flight_paths
                    .into_iter()
                    .take(sample_limit)
                    .collect::<Vec<_>>(),
            )
        };

        UploadDebounceSnapshot {
            pending_count,
            uploads_in_flight_count,
            path_generation,
            path_in_flight,
            pending_paths_sample,
            uploads_in_flight_sample,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LocalFileSnapshot {
    len: u64,
    modified: Option<SystemTime>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UploadAttemptOutcome {
    Settled,
    Retry,
    Conflict,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct UploadObjectContext {
    object_id: Option<String>,
    expected_revision: Option<String>,
}

pub(crate) fn schedule_debounced_close_upload(
    worker: Arc<UploadWorkerContext>,
    debounce: Arc<UploadDebounceState>,
    relative_path: String,
) {
    let generation = {
        let mut pending = debounce
            .pending_generations
            .lock()
            .expect("pending upload generations lock poisoned");
        let entry = pending.entry(relative_path.clone()).or_insert(0);
        *entry += 1;
        *entry
    };
    let snapshot = debounce.debug_snapshot_for_path(&relative_path, 8);

    tracing::info!(
        "close-completion: scheduled upload for {} after {:?} quiet period (generation {}, {})",
        relative_path,
        CLOSE_UPLOAD_QUIET_PERIOD,
        generation,
        snapshot.to_log_string()
    );
    close_upload_trace_event(format!(
        "scheduled path={} generation={} delay_ms={} {}",
        relative_path,
        generation,
        CLOSE_UPLOAD_QUIET_PERIOD.as_millis(),
        snapshot.to_log_string()
    ));

    spawn_debounced_close_upload(
        worker,
        debounce,
        relative_path,
        generation,
        CLOSE_UPLOAD_QUIET_PERIOD,
    );
}

fn spawn_debounced_close_upload(
    worker: Arc<UploadWorkerContext>,
    debounce: Arc<UploadDebounceState>,
    relative_path: String,
    generation: u64,
    delay: Duration,
) {
    std::thread::spawn(move || {
        std::thread::sleep(delay);

        let is_latest = {
            let pending = debounce
                .pending_generations
                .lock()
                .expect("pending upload generations lock poisoned");
            pending.get(&relative_path).copied() == Some(generation)
        };
        if !is_latest {
            let snapshot = debounce.debug_snapshot_for_path(&relative_path, 8);
            tracing::info!(
                "close-completion: skipping stale upload worker for {} generation {} ({})",
                relative_path,
                generation,
                snapshot.to_log_string()
            );
            close_upload_trace_event(format!(
                "stale-worker-skip path={} generation={} {}",
                relative_path,
                generation,
                snapshot.to_log_string()
            ));
            return;
        }

        {
            let mut uploads_in_flight = debounce
                .uploads_in_flight
                .lock()
                .expect("uploads_in_flight lock poisoned");
            if !uploads_in_flight.insert(relative_path.clone()) {
                drop(uploads_in_flight);
                let snapshot = debounce.debug_snapshot_for_path(&relative_path, 8);
                tracing::info!(
                    "close-completion: upload already in flight for {} generation {} ({})",
                    relative_path,
                    generation,
                    snapshot.to_log_string()
                );
                close_upload_trace_event(format!(
                    "already-in-flight path={} generation={} {}",
                    relative_path,
                    generation,
                    snapshot.to_log_string()
                ));
                return;
            }
        }

        let _upload_permit = worker.upload_gate.acquire(&relative_path);
        tracing::info!(
            "close-completion: starting upload worker for {} generation {} after {:?}",
            relative_path,
            generation,
            delay
        );
        close_upload_trace_event(format!(
            "worker-start path={} generation={} delay_ms={}",
            relative_path,
            generation,
            delay.as_millis()
        ));
        let outcome = match panic::catch_unwind(AssertUnwindSafe(|| {
            process_debounced_close_upload(worker.as_ref(), &relative_path)
        })) {
            Ok(outcome) => outcome,
            Err(payload) => {
                tracing::error!(
                    "close-completion: upload worker panicked for {} generation {}: {}",
                    relative_path,
                    generation,
                    panic_payload_message(payload.as_ref())
                );
                close_upload_trace_event(format!(
                    "worker-panic path={} generation={} panic={}",
                    relative_path,
                    generation,
                    panic_payload_message(payload.as_ref())
                ));
                UploadAttemptOutcome::Retry
            }
        };

        debounce
            .uploads_in_flight
            .lock()
            .expect("uploads_in_flight lock poisoned")
            .remove(&relative_path);

        let latest_generation = {
            let pending = debounce
                .pending_generations
                .lock()
                .expect("pending upload generations lock poisoned");
            pending.get(&relative_path).copied()
        };
        let snapshot = debounce.debug_snapshot_for_path(&relative_path, 8);
        tracing::info!(
            "close-completion: worker finished for {} generation {} outcome={:?} latest_generation={:?} ({})",
            relative_path,
            generation,
            outcome,
            latest_generation,
            snapshot.to_log_string()
        );
        close_upload_trace_event(format!(
            "worker-finish path={} generation={} outcome={:?} latest_generation={:?} {}",
            relative_path,
            generation,
            outcome,
            latest_generation,
            snapshot.to_log_string()
        ));

        if let Some(latest) = latest_generation {
            if latest != generation {
                let follow_up_delay = match outcome {
                    UploadAttemptOutcome::Retry => CLOSE_UPLOAD_RETRY_DELAY,
                    UploadAttemptOutcome::Settled | UploadAttemptOutcome::Conflict => {
                        CLOSE_UPLOAD_QUIET_PERIOD
                    }
                };
                tracing::info!(
                    "close-completion: newer generation {} is pending for {}; scheduling follow-up after {:?} ({})",
                    latest,
                    relative_path,
                    follow_up_delay,
                    snapshot.to_log_string()
                );
                close_upload_trace_event(format!(
                    "worker-follow-up path={} generation={} latest_generation={} delay_ms={} {}",
                    relative_path,
                    generation,
                    latest,
                    follow_up_delay.as_millis(),
                    snapshot.to_log_string()
                ));
                spawn_debounced_close_upload(
                    worker,
                    debounce,
                    relative_path,
                    latest,
                    follow_up_delay,
                );
                return;
            }

            match outcome {
                UploadAttemptOutcome::Retry => {
                    tracing::info!(
                        "close-completion: retrying upload for {} generation {} after {:?} ({})",
                        relative_path,
                        latest,
                        CLOSE_UPLOAD_RETRY_DELAY,
                        snapshot.to_log_string()
                    );
                    close_upload_trace_event(format!(
                        "worker-retry path={} generation={} delay_ms={} {}",
                        relative_path,
                        latest,
                        CLOSE_UPLOAD_RETRY_DELAY.as_millis(),
                        snapshot.to_log_string()
                    ));
                    spawn_debounced_close_upload(
                        worker,
                        debounce,
                        relative_path,
                        latest,
                        CLOSE_UPLOAD_RETRY_DELAY,
                    );
                }
                UploadAttemptOutcome::Settled | UploadAttemptOutcome::Conflict => {
                    close_upload_trace_event(format!(
                        "worker-finished path={} generation={} outcome={outcome:?}",
                        relative_path, latest,
                    ));
                    debounce
                        .pending_generations
                        .lock()
                        .expect("pending upload generations lock poisoned")
                        .remove(&relative_path);
                }
            }
        }
    });
}

fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

fn close_upload_trace_event(message: String) {
    let Some(path) = std::env::var_os(CLOSE_UPLOAD_TRACE_FILE_ENV).map(PathBuf::from) else {
        return;
    };

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let timestamp = match SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(elapsed) => format!("{}.{}", elapsed.as_secs(), elapsed.subsec_millis()),
        Err(_) => "time-error".to_string(),
    };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{} {}", timestamp, message);
    }
}

fn process_debounced_close_upload(
    worker: &UploadWorkerContext,
    relative_path: &str,
) -> UploadAttemptOutcome {
    let full_path = worker.sync_root.join(relative_path);

    tracing::info!(
        "close-completion: checking upload for {} state_before={}",
        relative_path,
        describe_path_state(&full_path)
    );

    let metadata = match std::fs::metadata(&full_path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(
                "close-completion: path disappeared before upload check, skipping {}",
                relative_path
            );
            return UploadAttemptOutcome::Settled;
        }
        Err(err) => {
            tracing::info!(
                "close-completion: metadata error for {}: {}",
                full_path.display(),
                err
            );
            close_upload_trace_event(format!(
                "metadata-error path={} error={}",
                relative_path, err
            ));
            return UploadAttemptOutcome::Retry;
        }
    };

    if metadata.is_dir() {
        tracing::debug!("close-completion: skipping directory {}", relative_path);
        return UploadAttemptOutcome::Settled;
    }

    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&full_path)
    {
        Ok(f) => f,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(
                "close-completion: file disappeared before open, skipping {}",
                relative_path
            );
            return UploadAttemptOutcome::Settled;
        }
        Err(err) => {
            tracing::info!(
                "cfapi close-completion open error: path={} error={}",
                full_path.display(),
                err
            );
            close_upload_trace_event(format!("open-error path={} error={}", relative_path, err));
            return UploadAttemptOutcome::Retry;
        }
    };

    match cf_get_placeholder_standard_info(&file) {
        Ok(placeholder_info) if placeholder_info.ModifiedDataSize == 0 => {
            tracing::info!(
                "close-completion: {} was already scheduled for upload but ModifiedDataSize is zero; continuing upload to preserve retry semantics state={}",
                relative_path,
                describe_path_state(&full_path)
            );
            close_upload_trace_event(format!(
                "modified-data-zero-continue path={}",
                relative_path
            ));
        }
        Ok(_) => {}
        Err(err) => {
            tracing::info!(
                "close-completion: placeholder info unavailable for {}, treating as modified: {}",
                relative_path,
                err
            );
        }
    }

    let snapshot_before = match capture_file_snapshot(&file) {
        Ok(snapshot) => snapshot,
        Err(err) => {
            tracing::info!(
                "close-completion: failed to snapshot {} before upload: {}",
                relative_path,
                err
            );
            close_upload_trace_event(format!(
                "snapshot-before-error path={} error={}",
                relative_path, err
            ));
            return UploadAttemptOutcome::Retry;
        }
    };

    let (mut upload_usn, object_context) = match prepare_file_for_upload(&file, relative_path) {
        Ok(prepared) => prepared,
        Err(err) => {
            tracing::info!(
                "close-completion: failed to prepare {} for upload: {:#}",
                relative_path,
                err
            );
            close_upload_trace_event(format!(
                "prepare-error path={} error={:#}",
                relative_path, err
            ));
            return UploadAttemptOutcome::Retry;
        }
    };
    tracing::info!(
        "close-completion: prepared {} for upload snapshot_before={:?} upload_usn={} state={}",
        relative_path,
        snapshot_before,
        upload_usn,
        describe_path_state(&full_path)
    );
    reconcile_ancestor_directory_sync_states(&worker.sync_root, relative_path);

    let upload_receipt = match upload_file_on_close(
        worker,
        relative_path,
        &object_context,
        snapshot_before.len,
        &mut file,
    ) {
        Ok(receipt) => receipt,
        Err(err) => {
            tracing::info!(
                "cfapi upload error: path={} bytes={} error={:#}",
                relative_path,
                snapshot_before.len,
                err
            );
            close_upload_trace_event(format!(
                "upload-error path={} bytes={} error={:#}",
                relative_path, snapshot_before.len, err
            ));
            if is_remote_mutation_conflict(&err) {
                tracing::warn!(
                    "close-completion: preserving unsynchronized local change after remote mutation conflict for {}",
                    relative_path
                );
                return UploadAttemptOutcome::Conflict;
            }
            return UploadAttemptOutcome::Retry;
        }
    };

    let snapshot_after = match capture_path_snapshot(&full_path) {
        Ok(snapshot) => snapshot,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(
                "close-completion: {} was removed after upload; waiting for follow-up event",
                relative_path
            );
            return UploadAttemptOutcome::Settled;
        }
        Err(err) => {
            tracing::info!(
                "close-completion: failed to snapshot {} after upload: {}",
                relative_path,
                err
            );
            if let Err(receipt_error) = record_placeholder_upload_receipt(
                &file,
                relative_path,
                &upload_receipt,
                worker.provider_instance_id,
                false,
            ) {
                tracing::warn!(
                    "close-completion: failed recording the accepted server revision after a snapshot error for {}: {receipt_error:#}",
                    relative_path
                );
            }
            close_upload_trace_event(format!(
                "snapshot-after-error path={} error={}",
                relative_path, err
            ));
            return UploadAttemptOutcome::Retry;
        }
    };

    if snapshot_after != snapshot_before {
        if let Err(err) = record_upload_receipt_after_concurrent_change(
            &file,
            relative_path,
            &upload_receipt,
            worker.provider_instance_id,
        ) {
            tracing::warn!(
                "close-completion: failed recording the accepted server revision for concurrently changed {}: {err:#}",
                relative_path
            );
        }
        tracing::info!(
            "close-completion: {} changed during upload, scheduling retry snapshot_before={:?} snapshot_after={:?} state={}",
            relative_path,
            snapshot_before,
            snapshot_after,
            describe_path_state(&full_path)
        );
        close_upload_trace_event(format!(
            "snapshot-changed path={} snapshot_before={:?} snapshot_after={:?}",
            relative_path, snapshot_before, snapshot_after
        ));
        return UploadAttemptOutcome::Retry;
    }
    tracing::info!(
        "close-completion: upload finished for {} snapshot_after={:?} upload_usn={} state_before_in_sync={}",
        relative_path,
        snapshot_after,
        upload_usn,
        describe_path_state(&full_path)
    );

    if let Err(err) = finalize_placeholder_after_upload(
        &file,
        relative_path,
        &upload_receipt,
        worker.provider_instance_id,
        &mut upload_usn,
    ) {
        tracing::info!(
            "close-completion: failed to mark {} in sync after upload: {:#}",
            relative_path,
            err
        );
        close_upload_trace_event(format!(
            "mark-in-sync-error path={} error={:#}",
            relative_path, err
        ));
        return UploadAttemptOutcome::Retry;
    }

    reconcile_ancestor_directory_sync_states(&worker.sync_root, relative_path);
    tracing::info!(
        "cfapi uploaded local file: path={} bytes={} final_state={}",
        relative_path,
        snapshot_before.len,
        describe_path_state(&full_path)
    );
    close_upload_trace_event(format!(
        "upload-success path={} bytes={}",
        relative_path, snapshot_before.len
    ));
    UploadAttemptOutcome::Settled
}

fn prepare_file_for_upload(
    file: &std::fs::File,
    relative_path: &str,
) -> Result<(i64, UploadObjectContext)> {
    cf_ensure_placeholder_identity(file, relative_path)?;
    let info = cf_get_placeholder_standard_info_with_identity(file)?;
    let identity = decode_placeholder_file_identity(info.file_identity()).ok_or_else(|| {
        anyhow::anyhow!("placeholder identity is not decodable for {relative_path}")
    })?;
    let object_id = identity.object_id.filter(|value| !value.trim().is_empty());
    let expected_revision = identity
        .remote_version
        .filter(|value| !value.trim().is_empty());
    if object_id.is_some() != expected_revision.is_some() {
        bail!(
            "refusing ambiguous upload for {relative_path}: object_id and expected_revision must either both be present or both be absent"
        );
    }
    let upload_usn = cf_set_not_in_sync(file)?;
    Ok((
        upload_usn,
        UploadObjectContext {
            object_id,
            expected_revision,
        },
    ))
}

fn finalize_placeholder_after_upload(
    file: &std::fs::File,
    relative_path: &str,
    receipt: &UploadReceipt,
    provider_instance_id: uuid::Uuid,
    upload_usn: &mut i64,
) -> Result<()> {
    record_placeholder_upload_receipt(file, relative_path, receipt, provider_instance_id, true)?;
    // Updating the identity advances the file's USN. Capture a fresh value after
    // that write so CFAPI accepts the following in-sync transition.
    *upload_usn = cf_set_not_in_sync(file)?;
    cf_set_in_sync_with_usn(file, upload_usn)
}

fn record_placeholder_upload_receipt(
    file: &std::fs::File,
    relative_path: &str,
    receipt: &UploadReceipt,
    provider_instance_id: uuid::Uuid,
    update_content_baseline: bool,
) -> Result<()> {
    cf_ensure_placeholder_identity(file, relative_path)?;
    let info = cf_get_placeholder_standard_info_with_identity(file)?;
    let mut identity = decode_placeholder_file_identity(info.file_identity())
        .unwrap_or_else(|| PlaceholderFileIdentity::new(relative_path));
    let receipt_object_id = receipt
        .object_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("upload returned no object_id for {relative_path}"))?;
    if identity
        .object_id
        .as_deref()
        .is_some_and(|object_id| object_id != receipt_object_id)
    {
        bail!("upload changed object identity unexpectedly for {relative_path}");
    }
    let remote_version = receipt
        .remote_version
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("upload returned no revision for {relative_path}"))?;
    identity.object_id = Some(receipt_object_id.to_string());
    identity.path = relative_path.replace('\\', "/");
    identity.remote_version = Some(remote_version.to_string());
    identity.provider_instance_id = Some(provider_instance_id);
    if update_content_baseline
        && let Some(fingerprint) = receipt.in_sync_content_fingerprint.as_deref()
    {
        identity.set_in_sync_content_baseline(fingerprint);
    }
    cf_update_placeholder_file_identity(file, &identity.encoded())
}

fn record_upload_receipt_after_concurrent_change(
    file: &std::fs::File,
    relative_path: &str,
    receipt: &UploadReceipt,
    provider_instance_id: uuid::Uuid,
) -> Result<()> {
    record_placeholder_upload_receipt(file, relative_path, receipt, provider_instance_id, false)
}

fn capture_file_snapshot(file: &std::fs::File) -> Result<LocalFileSnapshot> {
    let metadata = file.metadata()?;
    Ok(LocalFileSnapshot {
        len: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

fn capture_path_snapshot(path: &Path) -> std::io::Result<LocalFileSnapshot> {
    let metadata = std::fs::metadata(path)?;
    Ok(LocalFileSnapshot {
        len: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

fn upload_file_on_close(
    worker: &UploadWorkerContext,
    relative_path: &str,
    object_context: &UploadObjectContext,
    metadata_len: u64,
    file: &mut std::fs::File,
) -> Result<UploadReceipt> {
    tracing::info!(
        "close-completion: uploading {} ({} bytes)",
        relative_path,
        metadata_len
    );

    let upload_receipt = worker.uploader.upload_reader_for_object(
        relative_path,
        object_context.object_id.as_deref(),
        object_context.expected_revision.as_deref(),
        file,
        metadata_len,
    )?;
    if let Some(version) = upload_receipt.remote_version.clone() {
        worker.runtime.set_remote_version(relative_path, version);
    }
    Ok(upload_receipt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use std::path::Path;
    use std::sync::Mutex as StdMutex;
    use std::time::UNIX_EPOCH;

    #[derive(Default)]
    struct RecordingUploader {
        uploads: StdMutex<Vec<(String, Vec<u8>, u64)>>,
        fail: bool,
    }

    impl Uploader for RecordingUploader {
        fn upload_reader(
            &self,
            path: &str,
            reader: &mut dyn std::io::Read,
            length: u64,
        ) -> Result<UploadReceipt> {
            if self.fail {
                return Err(anyhow!("simulated upload failure"));
            }
            let mut payload = Vec::new();
            reader.read_to_end(&mut payload)?;
            self.uploads
                .lock()
                .expect("upload record lock poisoned")
                .push((path.to_string(), payload, length));
            Ok(UploadReceipt {
                object_id: Some("obj-upload".to_string()),
                remote_version: Some(format!("version:size={length}")),
                in_sync_content_fingerprint: Some(format!("cfp-upload-{length}")),
            })
        }
    }

    fn make_test_file(payload: &[u8]) -> (std::path::PathBuf, std::fs::File) {
        let path = std::env::temp_dir().join(format!(
            "ironmesh-cfapi-close-upload-test-{}-{}.bin",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time should be after epoch")
                .as_nanos()
        ));
        std::fs::write(&path, payload).expect("failed to write temp test file");
        let file = std::fs::File::open(&path).expect("failed to reopen temp test file");
        (path, file)
    }

    fn test_upload_worker_context(uploader: Arc<dyn Uploader>) -> UploadWorkerContext {
        UploadWorkerContext {
            sync_root: Path::new("C:/ironmesh-test").to_path_buf(),
            provider_instance_id: uuid::Uuid::nil(),
            runtime: Arc::new(CfapiRuntime::default()),
            uploader,
            upload_gate: Arc::new(UploadConcurrencyGate::new(1)),
        }
    }

    #[test]
    fn upload_file_on_close_updates_runtime_after_successful_upload() {
        let uploader = Arc::new(RecordingUploader::default());
        let worker = test_upload_worker_context(uploader.clone());
        let payload = b"cfapi-upload-payload";
        let (path, mut file) = make_test_file(payload);

        let receipt = upload_file_on_close(
            &worker,
            "docs/photo.jpg",
            &UploadObjectContext {
                object_id: None,
                expected_revision: None,
            },
            payload.len() as u64,
            &mut file,
        )
        .expect("upload should succeed");

        let uploads = uploader
            .uploads
            .lock()
            .expect("upload record lock poisoned");
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].0, "docs/photo.jpg");
        assert_eq!(uploads[0].1, payload);
        assert_eq!(uploads[0].2, payload.len() as u64);
        drop(uploads);

        let hydrated = worker
            .runtime
            .handle_fetch_data("docs/photo.jpg", &crate::runtime::DemoHydrator)
            .expect("remote version should still be updated after upload");
        let hydrated_text = String::from_utf8(hydrated).expect("demo hydrator emits utf8 payload");
        assert!(hydrated_text.contains(&format!("version:size={}", payload.len())));
        let expected_fingerprint = format!("cfp-upload-{}", payload.len());
        assert_eq!(
            receipt.in_sync_content_fingerprint.as_deref(),
            Some(expected_fingerprint.as_str())
        );

        let _ = std::fs::remove_file(path);
    }
}
