use super::*;
use bytes::BytesMut;
use reqwest::Url;

const MAP_IMPORT_PATH: &str = "state/map_dataset_import.json";
const MAP_IMPORT_STORAGE_ROOT: &str = "sys/maps";
const MAP_IMPORT_MIN_PART_SIZE_BYTES: u64 = 256 * 1024 * 1024;
const MAP_IMPORT_MAX_PART_SIZE_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const MAP_IMPORT_INGEST_CHUNK_BYTES: usize = 8 * 1024 * 1024;
const MAP_IMPORT_CHECKPOINT_BYTES: u64 = 64 * 1024 * 1024;
const MAP_IMPORT_RETRY_MAX_DELAY_SECS: u64 = 60;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AdminMapDatasetImportState {
    Running,
    Failed,
    Completed,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AdminMapDatasetImportStatusResponse {
    pub(crate) active_job: Option<AdminMapDatasetImportJobView>,
    pub(crate) can_start_new: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AdminMapDatasetImportJobView {
    pub(crate) job_id: String,
    pub(crate) state: AdminMapDatasetImportState,
    pub(crate) dataset_filename: String,
    pub(crate) source_display: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) variant_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) asset: Option<map_config::MapVariantAssetKind>,
    pub(crate) logical_key: String,
    pub(crate) manifest_key: String,
    pub(crate) part_size_bytes: u64,
    pub(crate) total_size_bytes: u64,
    pub(crate) total_parts: usize,
    pub(crate) completed_parts: usize,
    pub(crate) completed_bytes: u64,
    pub(crate) current_part_index: Option<usize>,
    pub(crate) current_part_id: Option<String>,
    pub(crate) current_part_key: Option<String>,
    pub(crate) current_part_size_bytes: Option<u64>,
    pub(crate) current_part_completed_bytes: u64,
    pub(crate) manifest_uploaded: bool,
    pub(crate) retry_count: u32,
    pub(crate) next_retry_at_unix: Option<u64>,
    pub(crate) last_error: Option<String>,
    pub(crate) started_at_unix: u64,
    pub(crate) updated_at_unix: u64,
    pub(crate) finished_at_unix: Option<u64>,
    pub(crate) progress_percent: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct StartAdminMapDatasetImportRequest {
    source: String,
    part_size_bytes: u64,
    /// The cluster-configured map variant whose artifact is replaced.  Omitted
    /// only for compatibility with pre-variant import clients.
    #[serde(default)]
    variant_id: Option<String>,
    #[serde(default)]
    asset: Option<map_config::MapVariantAssetKind>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct StartAdminMapDatasetImportResponse {
    pub(crate) started: bool,
    pub(crate) status: AdminMapDatasetImportStatusResponse,
}

#[derive(Debug)]
pub(crate) struct MapDatasetImportRuntime {
    pub(crate) path: PathBuf,
    job: Option<MapDatasetImportJob>,
    worker_running: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MapDatasetImportStateFile {
    #[serde(default)]
    job: Option<MapDatasetImportJob>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MapDatasetImportJob {
    job_id: String,
    state: AdminMapDatasetImportState,
    source_url: String,
    #[serde(default)]
    representation_validator: Option<String>,
    #[serde(default)]
    variant_id: Option<String>,
    #[serde(default)]
    asset: Option<map_config::MapVariantAssetKind>,
    dataset_filename: String,
    logical_key: String,
    manifest_key: String,
    part_size_bytes: u64,
    total_size_bytes: u64,
    total_parts: usize,
    completed_parts: usize,
    completed_bytes: u64,
    current_part_index: usize,
    current_part_downloaded_bytes: u64,
    #[serde(default)]
    current_part_chunk_refs: Vec<UploadChunkRef>,
    manifest_uploaded: bool,
    retry_count: u32,
    next_retry_at_unix: Option<u64>,
    last_error: Option<String>,
    started_at_unix: u64,
    updated_at_unix: u64,
    finished_at_unix: Option<u64>,
}

#[derive(Debug, Clone)]
struct MapDatasetProbe {
    source_url: String,
    representation_validator: Option<String>,
    dataset_filename: String,
    total_size_bytes: u64,
}

#[derive(Debug, Clone)]
struct MapDatasetPartSpec {
    index: usize,
    part_id: String,
    key: String,
    offset_bytes: u64,
    size_bytes: u64,
}

#[derive(Debug)]
enum ImportAdvanceError {
    Fatal(String),
    Transient(String),
}

#[derive(Debug, Serialize)]
struct SplitFileManifestDocument {
    manifest_version: u32,
    #[serde(rename = "type")]
    manifest_type: String,
    logical_format: String,
    logical_key: String,
    manifest_key: String,
    storage_root: String,
    logical_size_bytes: u64,
    last_part_size_bytes: u64,
    parts_count: usize,
    parts: Vec<SplitFileManifestPartDocument>,
}

#[derive(Debug, Serialize)]
struct SplitFileManifestPartDocument {
    part_id: String,
    key: String,
    offset_bytes: u64,
    size_bytes: u64,
}

impl MapDatasetImportRuntime {
    pub(crate) fn empty(path: PathBuf) -> Self {
        Self {
            path,
            job: None,
            worker_running: false,
        }
    }

    fn status_view(&self) -> AdminMapDatasetImportStatusResponse {
        AdminMapDatasetImportStatusResponse {
            active_job: self.job.as_ref().map(MapDatasetImportJob::view),
            can_start_new: !self.worker_running
                && !matches!(
                    self.job.as_ref().map(|job| job.state),
                    Some(AdminMapDatasetImportState::Running)
                ),
        }
    }
}

pub(crate) async fn has_running_job(state: &ServerState) -> bool {
    let runtime = state.storage.map_dataset_import.lock().await;
    !runtime.status_view().can_start_new
}

impl MapDatasetImportJob {
    fn view(&self) -> AdminMapDatasetImportJobView {
        let current_part = current_part_spec(self).ok().flatten();
        let downloaded_bytes = self
            .completed_bytes
            .saturating_add(self.current_part_downloaded_bytes)
            .min(self.total_size_bytes);
        let progress_percent = if self.total_size_bytes == 0 {
            0.0
        } else {
            (downloaded_bytes as f64 / self.total_size_bytes as f64) * 100.0
        };

        AdminMapDatasetImportJobView {
            job_id: self.job_id.clone(),
            state: self.state,
            dataset_filename: self.dataset_filename.clone(),
            source_display: source_display_url(&self.source_url),
            variant_id: self.variant_id.clone(),
            asset: self.asset,
            logical_key: self.logical_key.clone(),
            manifest_key: self.manifest_key.clone(),
            part_size_bytes: self.part_size_bytes,
            total_size_bytes: self.total_size_bytes,
            total_parts: self.total_parts,
            completed_parts: self.completed_parts,
            completed_bytes: self.completed_bytes,
            current_part_index: current_part.as_ref().map(|part| part.index),
            current_part_id: current_part.as_ref().map(|part| part.part_id.clone()),
            current_part_key: current_part.as_ref().map(|part| part.key.clone()),
            current_part_size_bytes: current_part.as_ref().map(|part| part.size_bytes),
            current_part_completed_bytes: self.current_part_downloaded_bytes,
            manifest_uploaded: self.manifest_uploaded,
            retry_count: self.retry_count,
            next_retry_at_unix: self.next_retry_at_unix,
            last_error: self.last_error.clone(),
            started_at_unix: self.started_at_unix,
            updated_at_unix: self.updated_at_unix,
            finished_at_unix: self.finished_at_unix,
            progress_percent,
        }
    }
}

pub(crate) async fn load_runtime(data_dir: &FsPath) -> Result<MapDatasetImportRuntime> {
    let path = data_dir.join(MAP_IMPORT_PATH);
    if !tokio::fs::try_exists(&path).await? {
        return Ok(MapDatasetImportRuntime::empty(path));
    }

    let payload = tokio::fs::read(&path)
        .await
        .with_context(|| format!("failed reading {}", path.display()))?;
    let parsed = serde_json::from_slice::<MapDatasetImportStateFile>(&payload)
        .with_context(|| format!("failed parsing {}", path.display()))?;
    Ok(MapDatasetImportRuntime {
        path,
        job: parsed.job,
        worker_running: false,
    })
}

pub(crate) async fn spawn_resume_if_needed(state: ServerState) {
    spawn_worker_if_needed(&state).await;
}

pub(crate) async fn admin_status(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let action = "auth/maps/import/status";
    if let Err(status) =
        authorize_admin_request(&state, &headers, action, true, true, json!({})).await
    {
        return status.into_response();
    }

    let status = {
        let runtime = state.storage.map_dataset_import.lock().await;
        runtime.status_view()
    };
    (StatusCode::OK, Json(status)).into_response()
}

pub(crate) async fn start_admin_import(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(request): Json<StartAdminMapDatasetImportRequest>,
) -> impl IntoResponse {
    let action = "auth/maps/import/run";
    let authz = match authorize_admin_request(
        &state,
        &headers,
        action,
        true,
        true,
        json!({
            "part_size_bytes": request.part_size_bytes,
            "variant_id": request.variant_id,
            "asset": request.asset,
        }),
    )
    .await
    {
        Ok(request) => request,
        Err(status) => return status.into_response(),
    };

    let start_result = match start_import_job(&state, request).await {
        Ok(result) => result,
        Err(status) => return status.into_response(),
    };

    append_admin_audit(
        &state,
        action,
        &authz,
        true,
        true,
        true,
        if start_result.started {
            "started"
        } else {
            "already_running"
        },
        json!({
            "started": start_result.started,
            "active_job": start_result.status.active_job.as_ref().map(|job| json!({
                "job_id": job.job_id,
                "variant_id": job.variant_id,
                "asset": job.asset,
                "logical_key": job.logical_key,
                "manifest_key": job.manifest_key,
                "part_size_bytes": job.part_size_bytes,
                "total_size_bytes": job.total_size_bytes,
            })),
        }),
    )
    .await;

    (StatusCode::ACCEPTED, Json(start_result)).into_response()
}

async fn start_import_job(
    state: &ServerState,
    request: StartAdminMapDatasetImportRequest,
) -> std::result::Result<StartAdminMapDatasetImportResponse, StatusCode> {
    if natural_earth_import::is_running(state).await {
        return Err(StatusCode::CONFLICT);
    }
    let source_url = match extract_source_url(&request.source) {
        Ok(url) => url,
        Err(_) => return Err(StatusCode::BAD_REQUEST),
    };
    if request.part_size_bytes < MAP_IMPORT_MIN_PART_SIZE_BYTES
        || request.part_size_bytes > MAP_IMPORT_MAX_PART_SIZE_BYTES
    {
        return Err(StatusCode::BAD_REQUEST);
    }

    {
        let runtime = state.storage.map_dataset_import.lock().await;
        if !runtime.status_view().can_start_new {
            return Ok(StartAdminMapDatasetImportResponse {
                started: false,
                status: runtime.status_view(),
            });
        }
    }

    let configured_target = match (request.variant_id.as_deref(), request.asset) {
        (Some(variant_id), Some(asset)) => {
            let configuration = map_config::load_current_configuration(state)
                .await
                .map_err(|err| {
                    warn!(error = %err, variant_id, "failed loading map configuration for import");
                    StatusCode::INTERNAL_SERVER_ERROR
                })?;
            map_config::resolve_import_target(&configuration.configuration, variant_id, asset)
                .map(Some)
                .map_err(|err| {
                    warn!(error = %err, variant_id, ?asset, "invalid configured map import target");
                    StatusCode::BAD_REQUEST
                })?
        }
        (None, None) => None,
        _ => return Err(StatusCode::BAD_REQUEST),
    };
    let http = build_map_import_http_client().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let probe = probe_download_source(
        &http,
        &source_url,
        configured_target
            .as_ref()
            .map(|target| target.dataset_filename.as_str()),
    )
    .await
    .map_err(import_error_status)?;
    let now = unix_ts();
    let dataset_filename = configured_target
        .as_ref()
        .map(|target| target.dataset_filename.clone())
        .unwrap_or(probe.dataset_filename.clone());
    let logical_key = configured_target
        .as_ref()
        .map(|target| target.logical_key.clone())
        .unwrap_or_else(|| format!("{MAP_IMPORT_STORAGE_ROOT}/{dataset_filename}"));
    let manifest_key = configured_target
        .as_ref()
        .map(|target| target.manifest_key.clone())
        .unwrap_or_else(|| format!("{MAP_IMPORT_STORAGE_ROOT}/{dataset_filename}.manifest.json"));
    let job = MapDatasetImportJob {
        job_id: Uuid::now_v7().to_string(),
        state: AdminMapDatasetImportState::Running,
        source_url: probe.source_url,
        representation_validator: probe.representation_validator,
        variant_id: configured_target
            .as_ref()
            .map(|target| target.variant_id.clone()),
        asset: configured_target.as_ref().map(|target| target.asset),
        dataset_filename: dataset_filename.clone(),
        logical_key,
        manifest_key,
        part_size_bytes: request.part_size_bytes,
        total_size_bytes: probe.total_size_bytes,
        total_parts: part_count_for_size(probe.total_size_bytes, request.part_size_bytes)
            .map_err(|_| StatusCode::BAD_REQUEST)?,
        completed_parts: 0,
        completed_bytes: 0,
        current_part_index: 0,
        current_part_downloaded_bytes: 0,
        current_part_chunk_refs: Vec::new(),
        manifest_uploaded: false,
        retry_count: 0,
        next_retry_at_unix: None,
        last_error: None,
        started_at_unix: now,
        updated_at_unix: now,
        finished_at_unix: None,
    };

    let status = {
        let mut runtime = state.storage.map_dataset_import.lock().await;
        // A second request may have completed its range probe while this request
        // was probing. Keep the persisted job and single worker authoritative.
        if !runtime.status_view().can_start_new {
            return Ok(StartAdminMapDatasetImportResponse {
                started: false,
                status: runtime.status_view(),
            });
        }
        runtime.job = Some(job);
        runtime.status_view()
    };
    persist_runtime(state)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    spawn_worker_if_needed(state).await;

    Ok(StartAdminMapDatasetImportResponse {
        started: true,
        status,
    })
}

async fn spawn_worker_if_needed(state: &ServerState) {
    let should_spawn = {
        let mut runtime = state.storage.map_dataset_import.lock().await;
        if runtime.worker_running {
            false
        } else if matches!(
            runtime.job.as_ref().map(|job| job.state),
            Some(AdminMapDatasetImportState::Running)
        ) {
            runtime.worker_running = true;
            true
        } else {
            false
        }
    };

    if !should_spawn {
        return;
    }

    let state = state.clone();
    tokio::spawn(async move {
        run_worker(state.clone()).await;
        let mut runtime = state.storage.map_dataset_import.lock().await;
        runtime.worker_running = false;
    });
}

async fn run_worker(state: ServerState) {
    let http = match build_map_import_http_client() {
        Ok(client) => client,
        Err(err) => {
            let _ = fail_job(&state, err.to_string()).await;
            return;
        }
    };

    loop {
        let job = {
            let runtime = state.storage.map_dataset_import.lock().await;
            match runtime.job.clone() {
                Some(job) if job.state == AdminMapDatasetImportState::Running => job,
                _ => return,
            }
        };
        if let Err(error) = validate_persisted_job_checkpoint(&job) {
            let _ = fail_job(&state, error).await;
            return;
        }

        let outcome = if job.completed_parts >= job.total_parts {
            if job.manifest_uploaded {
                complete_job(&state).await
            } else {
                finalize_manifest(&state, &job).await
            }
        } else {
            advance_part(&state, &http, &job).await
        };

        match outcome {
            Ok(()) => continue,
            Err(ImportAdvanceError::Fatal(err)) => {
                let _ = fail_job(&state, err).await;
                return;
            }
            Err(ImportAdvanceError::Transient(err)) => {
                let retry_delay = transient_retry_delay_secs(&job);
                let _ = note_transient_failure(&state, err, retry_delay).await;
                tokio::time::sleep(Duration::from_secs(retry_delay)).await;
            }
        }
    }
}

async fn advance_part(
    state: &ServerState,
    http: &reqwest::Client,
    job: &MapDatasetImportJob,
) -> std::result::Result<(), ImportAdvanceError> {
    let part = current_part_spec(job)?
        .ok_or_else(|| ImportAdvanceError::Fatal("missing current import part".to_string()))?;

    if job.current_part_downloaded_bytes > part.size_bytes {
        return Err(ImportAdvanceError::Fatal(format!(
            "current part progress exceeds declared part size for {}",
            part.key
        )));
    }

    if job.current_part_downloaded_bytes < part.size_bytes {
        download_part_bytes(state, http, job, &part).await?;
    }

    let refreshed_job = {
        let runtime = state.storage.map_dataset_import.lock().await;
        runtime.job.clone().ok_or_else(|| {
            ImportAdvanceError::Fatal("import job disappeared during processing".to_string())
        })?
    };
    let refreshed_part = current_part_spec(&refreshed_job)?
        .ok_or_else(|| ImportAdvanceError::Fatal("current part vanished".to_string()))?;

    if refreshed_job.current_part_downloaded_bytes < refreshed_part.size_bytes {
        return Ok(());
    }

    finalize_part_object(state, &refreshed_job, &refreshed_part).await
}

async fn download_part_bytes(
    state: &ServerState,
    http: &reqwest::Client,
    job: &MapDatasetImportJob,
    part: &MapDatasetPartSpec,
) -> std::result::Result<(), ImportAdvanceError> {
    download_part_bytes_with_limits(
        state,
        http,
        job,
        part,
        MAP_IMPORT_INGEST_CHUNK_BYTES,
        MAP_IMPORT_CHECKPOINT_BYTES,
    )
    .await
}

async fn download_part_bytes_with_limits(
    state: &ServerState,
    http: &reqwest::Client,
    job: &MapDatasetImportJob,
    part: &MapDatasetPartSpec,
    ingest_chunk_bytes: usize,
    checkpoint_bytes_limit: u64,
) -> std::result::Result<(), ImportAdvanceError> {
    if ingest_chunk_bytes == 0 || checkpoint_bytes_limit == 0 {
        return Err(ImportAdvanceError::Fatal(
            "map import download limits must be greater than zero".to_string(),
        ));
    }
    let representation_validator =
        persisted_representation_validator(job.representation_validator.as_deref())
            .map_err(ImportAdvanceError::Fatal)?;
    let absolute_start = part
        .offset_bytes
        .saturating_add(job.current_part_downloaded_bytes);
    let absolute_end = part
        .offset_bytes
        .saturating_add(part.size_bytes)
        .saturating_sub(1);
    let range_header = format!("bytes={absolute_start}-{absolute_end}");
    let request = http
        .get(job.source_url.clone())
        .header(reqwest::header::RANGE, range_header)
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .header(reqwest::header::IF_RANGE, representation_validator);
    let response = request
        .send()
        .await
        .map_err(|err| ImportAdvanceError::Transient(err.to_string()))?;
    if response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        let status = response.status();
        let message = format!(
            "range download for {} returned unexpected status {status}",
            part.key
        );
        return Err(classify_http_status(status, message));
    }
    validate_content_range(
        response.headers(),
        absolute_start,
        absolute_end,
        job.total_size_bytes,
        &part.key,
    )?;
    validate_representation_validator(response.headers(), representation_validator, &part.key)?;
    let remaining_bytes = part
        .size_bytes
        .saturating_sub(job.current_part_downloaded_bytes);
    if response
        .content_length()
        .is_some_and(|content_length| content_length > remaining_bytes)
    {
        return Err(ImportAdvanceError::Fatal(format!(
            "range download for {} exceeded the declared remaining byte range",
            part.key
        )));
    }

    let mut downloaded_bytes = job.current_part_downloaded_bytes;
    let mut chunk_refs = job.current_part_chunk_refs.clone();
    let mut buffer = BytesMut::new();
    let mut checkpoint_bytes = 0u64;

    let mut response = response;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|err| ImportAdvanceError::Transient(err.to_string()))?
    {
        let received_bytes = downloaded_bytes
            .saturating_add(buffer.len() as u64)
            .saturating_add(chunk.len() as u64);
        if received_bytes > part.size_bytes {
            return Err(ImportAdvanceError::Fatal(format!(
                "range download for {} exceeded the declared remaining byte range",
                part.key
            )));
        }
        buffer.extend_from_slice(chunk.as_ref());
        while buffer.len() >= ingest_chunk_bytes {
            let ingest = buffer.split_to(ingest_chunk_bytes).freeze();
            let chunk_ref = ingest_import_chunk(state, ingest.as_ref()).await?;
            downloaded_bytes = downloaded_bytes.saturating_add(chunk_ref.size_bytes as u64);
            chunk_refs.push(chunk_ref);
            checkpoint_bytes = checkpoint_bytes.saturating_add(ingest.len() as u64);
            if checkpoint_bytes >= checkpoint_bytes_limit {
                update_running_part_checkpoint(
                    state,
                    job.job_id.as_str(),
                    downloaded_bytes,
                    &chunk_refs,
                    None,
                    None,
                    true,
                )
                .await
                .map_err(|err| ImportAdvanceError::Fatal(err.to_string()))?;
                checkpoint_bytes = 0;
            }
        }
    }

    if !buffer.is_empty() {
        let tail = buffer.split().freeze();
        let chunk_ref = ingest_import_chunk(state, tail.as_ref()).await?;
        downloaded_bytes = downloaded_bytes.saturating_add(chunk_ref.size_bytes as u64);
        chunk_refs.push(chunk_ref);
    }

    update_running_part_checkpoint(
        state,
        job.job_id.as_str(),
        downloaded_bytes,
        &chunk_refs,
        None,
        None,
        true,
    )
    .await
    .map_err(|err| ImportAdvanceError::Fatal(err.to_string()))?;

    if downloaded_bytes > part.size_bytes {
        return Err(ImportAdvanceError::Fatal(format!(
            "downloaded {} bytes for {}, expected at most {}",
            downloaded_bytes, part.key, part.size_bytes
        )));
    }
    if downloaded_bytes < part.size_bytes {
        return Err(ImportAdvanceError::Transient(format!(
            "download for {} ended after {} of {} bytes; retrying remaining range",
            part.key, downloaded_bytes, part.size_bytes
        )));
    }

    Ok(())
}

async fn finalize_part_object(
    state: &ServerState,
    job: &MapDatasetImportJob,
    part: &MapDatasetPartSpec,
) -> std::result::Result<(), ImportAdvanceError> {
    let current_chunk_bytes = chunk_refs_size_bytes(&job.current_part_chunk_refs);
    if current_chunk_bytes != part.size_bytes {
        return Err(ImportAdvanceError::Fatal(format!(
            "part {} has {} bytes across chunk refs but expected {}",
            part.key, current_chunk_bytes, part.size_bytes
        )));
    }

    let put_result = {
        let mut store = lock_store(state, "map_dataset_import.finalize_part").await;
        store
            .put_object_from_chunks(
                &part.key,
                part.size_bytes as usize,
                &job.current_part_chunk_refs,
                PutOptions {
                    parent_version_ids: Vec::new(),
                    state: VersionConsistencyState::Confirmed,
                    inherit_preferred_parent: true,
                    create_snapshot: true,
                    explicit_version_id: None,
                },
            )
            .await
            .map_err(|err| ImportAdvanceError::Fatal(err.to_string()))?
    };

    register_put_outcome(state, &part.key, &put_result.version_id).await;
    let next_completed_parts = job.completed_parts.saturating_add(1);
    let next_completed_bytes = job.completed_bytes.saturating_add(part.size_bytes);
    let next_part_index = next_completed_parts;

    {
        let mut runtime = state.storage.map_dataset_import.lock().await;
        let Some(active_job) = runtime.job.as_mut() else {
            return Err(ImportAdvanceError::Fatal(
                "import job vanished during part finalization".to_string(),
            ));
        };
        if active_job.job_id != job.job_id {
            return Err(ImportAdvanceError::Fatal(
                "import job changed during part finalization".to_string(),
            ));
        }
        active_job.completed_parts = next_completed_parts;
        active_job.completed_bytes = next_completed_bytes;
        active_job.current_part_index = next_part_index;
        active_job.current_part_downloaded_bytes = 0;
        active_job.current_part_chunk_refs.clear();
        active_job.retry_count = 0;
        active_job.next_retry_at_unix = None;
        active_job.last_error = None;
        active_job.updated_at_unix = unix_ts();
    }
    persist_runtime(state)
        .await
        .map_err(|err| ImportAdvanceError::Fatal(err.to_string()))?;
    Ok(())
}

async fn finalize_manifest(
    state: &ServerState,
    job: &MapDatasetImportJob,
) -> std::result::Result<(), ImportAdvanceError> {
    validate_manifest_checkpoint(job)?;
    let manifest_bytes = build_split_manifest_bytes(job)
        .map_err(|err| ImportAdvanceError::Fatal(err.to_string()))?;
    let put_result = {
        let mut store = lock_store(state, "map_dataset_import.finalize_manifest").await;
        store
            .put_object_versioned(
                &job.manifest_key,
                Bytes::from(manifest_bytes),
                PutOptions {
                    parent_version_ids: Vec::new(),
                    state: VersionConsistencyState::Confirmed,
                    inherit_preferred_parent: true,
                    create_snapshot: true,
                    explicit_version_id: None,
                },
            )
            .await
            .map_err(|err| ImportAdvanceError::Fatal(err.to_string()))?
    };

    register_put_outcome(state, &job.manifest_key, &put_result.version_id).await;
    invalidate_cached_mbtiles_source(state, &job.manifest_key).await;

    {
        let mut runtime = state.storage.map_dataset_import.lock().await;
        let Some(active_job) = runtime.job.as_mut() else {
            return Err(ImportAdvanceError::Fatal(
                "import job vanished during manifest finalization".to_string(),
            ));
        };
        if active_job.job_id != job.job_id {
            return Err(ImportAdvanceError::Fatal(
                "import job changed during manifest finalization".to_string(),
            ));
        }
        active_job.manifest_uploaded = true;
        active_job.updated_at_unix = unix_ts();
    }
    persist_runtime(state)
        .await
        .map_err(|err| ImportAdvanceError::Fatal(err.to_string()))?;
    complete_job(state).await
}

async fn complete_job(state: &ServerState) -> std::result::Result<(), ImportAdvanceError> {
    {
        let mut runtime = state.storage.map_dataset_import.lock().await;
        let Some(job) = runtime.job.as_mut() else {
            return Err(ImportAdvanceError::Fatal(
                "import job disappeared before completion".to_string(),
            ));
        };
        job.state = AdminMapDatasetImportState::Completed;
        job.retry_count = 0;
        job.next_retry_at_unix = None;
        job.last_error = None;
        job.updated_at_unix = unix_ts();
        job.finished_at_unix = Some(job.updated_at_unix);
    }
    persist_runtime(state)
        .await
        .map_err(|err| ImportAdvanceError::Fatal(err.to_string()))?;
    Ok(())
}

async fn fail_job(state: &ServerState, error: String) -> Result<()> {
    {
        let mut runtime = state.storage.map_dataset_import.lock().await;
        if let Some(job) = runtime.job.as_mut() {
            job.state = AdminMapDatasetImportState::Failed;
            job.last_error = Some(error);
            job.next_retry_at_unix = None;
            job.updated_at_unix = unix_ts();
            job.finished_at_unix = Some(job.updated_at_unix);
        }
    }
    persist_runtime(state).await
}

async fn note_transient_failure(
    state: &ServerState,
    error: String,
    retry_delay_secs: u64,
) -> Result<()> {
    {
        let mut runtime = state.storage.map_dataset_import.lock().await;
        if let Some(job) = runtime.job.as_mut() {
            job.retry_count = job.retry_count.saturating_add(1);
            job.last_error = Some(error);
            job.next_retry_at_unix = Some(unix_ts().saturating_add(retry_delay_secs));
            job.updated_at_unix = unix_ts();
        }
    }
    persist_runtime(state).await
}

async fn update_running_part_checkpoint(
    state: &ServerState,
    job_id: &str,
    downloaded_bytes: u64,
    chunk_refs: &[UploadChunkRef],
    retry_count: Option<u32>,
    last_error: Option<Option<String>>,
    force_persist: bool,
) -> Result<()> {
    {
        let mut runtime = state.storage.map_dataset_import.lock().await;
        let Some(job) = runtime.job.as_mut() else {
            bail!("import job disappeared while updating checkpoint");
        };
        if job.job_id != job_id {
            bail!("import job changed while updating checkpoint");
        }
        job.current_part_downloaded_bytes = downloaded_bytes;
        job.current_part_chunk_refs = chunk_refs.to_vec();
        if let Some(retry_count) = retry_count {
            job.retry_count = retry_count;
        }
        if let Some(last_error) = last_error {
            job.last_error = last_error;
        }
        job.next_retry_at_unix = None;
        job.updated_at_unix = unix_ts();
    }

    if force_persist {
        persist_runtime(state).await?;
    }
    Ok(())
}

async fn persist_runtime(state: &ServerState) -> Result<()> {
    let (path, job) = {
        let runtime = state.storage.map_dataset_import.lock().await;
        (runtime.path.clone(), runtime.job.clone())
    };
    let payload = serde_json::to_vec_pretty(&MapDatasetImportStateFile { job })
        .context("failed encoding map dataset import state")?;
    write_json_atomic(&path, &payload).await
}

async fn ingest_import_chunk(
    state: &ServerState,
    payload: &[u8],
) -> std::result::Result<UploadChunkRef, ImportAdvanceError> {
    let (hash, _) = state
        .storage
        .upload_chunk_ingestor
        .ingest_chunk_auto(payload)
        .await
        .map_err(|err| ImportAdvanceError::Fatal(err.to_string()))?;
    Ok(UploadChunkRef {
        hash,
        size_bytes: payload.len(),
    })
}

pub(crate) async fn register_put_outcome(state: &ServerState, key: &str, version_id: &str) {
    register_cluster_object_put_outcome(state, key, version_id).await;
}

pub(crate) async fn invalidate_cached_mbtiles_source(state: &ServerState, manifest_key: &str) {
    {
        let mut sources = state.storage.mbtiles_sources.write().await;
        sources.remove(manifest_key);
    }
    super::web_maps::clear_mbtiles_source_initialization_failure(state, manifest_key).await;
}

fn build_map_import_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .build()
        .context("failed building map dataset import HTTP client")
}

async fn probe_download_source(
    http: &reqwest::Client,
    source_url: &str,
    configured_dataset_filename: Option<&str>,
) -> std::result::Result<MapDatasetProbe, ImportAdvanceError> {
    let parsed = Url::parse(source_url)
        .map_err(|err| ImportAdvanceError::Fatal(format!("invalid source URL: {err}")))?;
    let response = http
        .get(parsed.clone())
        .header(reqwest::header::RANGE, "bytes=0-0")
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .send()
        .await
        .map_err(|err| ImportAdvanceError::Transient(err.to_string()))?;

    if response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(classify_http_status(
            response.status(),
            format!(
                "source {} does not support resumable range requests",
                source_display_url(parsed.as_str())
            ),
        ));
    }

    let content_range = response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| {
            ImportAdvanceError::Fatal("range probe response was missing Content-Range".to_string())
        })?;
    let (start, end, total_size_bytes) = parse_content_range(content_range).ok_or_else(|| {
        ImportAdvanceError::Fatal(format!("invalid Content-Range header: {content_range}"))
    })?;
    if start != 0 || end != 0 || total_size_bytes == 0 {
        return Err(ImportAdvanceError::Fatal(format!(
            "range probe returned unexpected Content-Range: {content_range}"
        )));
    }

    let dataset_filename = probe_dataset_filename(response.url(), configured_dataset_filename)?;
    let representation_validator =
        representation_validator(response.headers()).ok_or_else(|| {
            ImportAdvanceError::Fatal(
                "range probe response was missing a strong ETag or Last-Modified validator"
                    .to_string(),
            )
        })?;
    Ok(MapDatasetProbe {
        // Keep the stable user-provided URL. Redirect targets may be short-lived
        // signed CDN URLs and must be re-resolved after a restart.
        source_url: source_url.to_string(),
        representation_validator: Some(representation_validator),
        dataset_filename,
        total_size_bytes,
    })
}

fn strong_etag(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(reqwest::header::ETAG)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.starts_with("W/"))
        .filter(|value| value.len() > 2 && value.starts_with('"') && value.ends_with('"'))
}

fn representation_validator(headers: &HeaderMap) -> Option<String> {
    strong_etag(headers).map(ToString::to_string).or_else(|| {
        headers
            .get(reqwest::header::LAST_MODIFIED)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
    })
}

fn validate_representation_validator(
    headers: &HeaderMap,
    expected: &str,
    part_key: &str,
) -> std::result::Result<(), ImportAdvanceError> {
    let observed = if expected.starts_with('"') {
        strong_etag(headers)
    } else {
        headers
            .get(reqwest::header::LAST_MODIFIED)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    let Some(observed) = observed else {
        return Err(ImportAdvanceError::Fatal(format!(
            "range download for {part_key} omitted the probed representation validator"
        )));
    };
    if observed != expected {
        return Err(ImportAdvanceError::Fatal(format!(
            "range download for {part_key} changed representation validator from {expected} to {observed}"
        )));
    }
    Ok(())
}

fn persisted_representation_validator(validator: Option<&str>) -> Result<&str, String> {
    let validator = validator
        .map(str::trim)
        .filter(|validator| !validator.is_empty())
        .ok_or_else(|| {
            "cannot safely resume a map import without a representation validator".to_string()
        })?;
    HeaderValue::from_str(validator).map_err(|_| {
        "cannot safely resume a map import with a representation validator that is not a valid HTTP header value"
            .to_string()
    })?;
    Ok(validator)
}

fn probe_dataset_filename(
    final_url: &Url,
    configured_dataset_filename: Option<&str>,
) -> std::result::Result<String, ImportAdvanceError> {
    // A selected variant provides its canonical destination. Signed download
    // URLs commonly end in a token or an API route rather than `.mbtiles`, so
    // only the legacy, unconfigured import path derives a name from the URL.
    match configured_dataset_filename {
        Some(filename) => Ok(filename.to_string()),
        None => dataset_filename_from_url(final_url),
    }
}

fn dataset_filename_from_url(url: &Url) -> std::result::Result<String, ImportAdvanceError> {
    let Some(filename) = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
    else {
        return Err(ImportAdvanceError::Fatal(format!(
            "source URL {} does not contain a dataset filename",
            source_display_url(url.as_str())
        )));
    };
    if !filename.ends_with(".mbtiles") {
        return Err(ImportAdvanceError::Fatal(format!(
            "source dataset filename {filename} must end with .mbtiles"
        )));
    }
    Ok(filename.to_string())
}

fn extract_source_url(raw: &str) -> std::result::Result<String, String> {
    raw.split_whitespace()
        .map(|token| token.trim_matches(|ch| ch == '"' || ch == '\''))
        .find(|token| token.starts_with("https://") || token.starts_with("http://"))
        .map(ToString::to_string)
        .ok_or_else(|| "expected an http(s) URL or pasted wget command".to_string())
}

fn source_display_url(raw: &str) -> String {
    let Ok(url) = Url::parse(raw) else {
        return raw.to_string();
    };
    let host = url.host_str().unwrap_or("unknown-host");
    let file_name = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .filter(|segment| !segment.is_empty())
        .unwrap_or("download");
    format!("{}://{host}/.../{file_name}", url.scheme())
}

fn parse_content_range(value: &str) -> Option<(u64, u64, u64)> {
    let payload = value.strip_prefix("bytes ")?;
    let (range, total) = payload.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start = start.parse().ok()?;
    let end = end.parse().ok()?;
    let total = total.parse().ok()?;
    Some((start, end, total))
}

fn validate_content_range(
    headers: &HeaderMap,
    expected_start: u64,
    expected_end: u64,
    expected_total: u64,
    part_key: &str,
) -> std::result::Result<(), ImportAdvanceError> {
    let content_range = headers
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| {
            ImportAdvanceError::Fatal(format!(
                "range download for {part_key} was missing Content-Range"
            ))
        })?;
    let (start, end, total) = parse_content_range(content_range).ok_or_else(|| {
        ImportAdvanceError::Fatal(format!(
            "range download for {part_key} had invalid Content-Range: {content_range}"
        ))
    })?;
    if start != expected_start || end != expected_end || total != expected_total {
        return Err(ImportAdvanceError::Fatal(format!(
            "range download for {part_key} returned {content_range}, expected bytes {expected_start}-{expected_end}/{expected_total}"
        )));
    }
    Ok(())
}

fn classify_http_status(status: reqwest::StatusCode, message: String) -> ImportAdvanceError {
    if status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
    {
        ImportAdvanceError::Transient(message)
    } else {
        ImportAdvanceError::Fatal(message)
    }
}

fn import_error_status(error: ImportAdvanceError) -> StatusCode {
    match error {
        ImportAdvanceError::Fatal(_) => StatusCode::BAD_REQUEST,
        ImportAdvanceError::Transient(_) => StatusCode::BAD_GATEWAY,
    }
}

fn transient_retry_delay_secs(job: &MapDatasetImportJob) -> u64 {
    let exponent = job.retry_count.min(6);
    let base = 2u64.saturating_pow(exponent);
    base.clamp(2, MAP_IMPORT_RETRY_MAX_DELAY_SECS)
}

fn current_part_spec(
    job: &MapDatasetImportJob,
) -> std::result::Result<Option<MapDatasetPartSpec>, ImportAdvanceError> {
    if job.current_part_index >= job.total_parts {
        return Ok(None);
    }
    part_spec(
        &job.dataset_filename,
        &job.job_id,
        job.total_size_bytes,
        job.part_size_bytes,
        job.total_parts,
        job.current_part_index,
    )
    .map(Some)
    .map_err(|err| ImportAdvanceError::Fatal(err.to_string()))
}

fn part_spec(
    dataset_filename: &str,
    job_id: &str,
    total_size_bytes: u64,
    part_size_bytes: u64,
    total_parts: usize,
    index: usize,
) -> Result<MapDatasetPartSpec> {
    let width = split_suffix_width(total_parts);
    let offset_bytes = (index as u64)
        .checked_mul(part_size_bytes)
        .ok_or_else(|| anyhow!("part offset overflow"))?;
    let remaining = total_size_bytes
        .checked_sub(offset_bytes)
        .ok_or_else(|| anyhow!("part offset exceeds total size"))?;
    let size_bytes = remaining.min(part_size_bytes);
    Ok(MapDatasetPartSpec {
        index,
        part_id: split_suffix(index, width),
        key: format!(
            "{MAP_IMPORT_STORAGE_ROOT}/{dataset_filename}.import-{job_id}-part-{}",
            split_suffix(index, width)
        ),
        offset_bytes,
        size_bytes,
    })
}

fn build_split_manifest_bytes(job: &MapDatasetImportJob) -> Result<Vec<u8>> {
    let mut parts = Vec::with_capacity(job.total_parts);
    for index in 0..job.total_parts {
        let part = part_spec(
            &job.dataset_filename,
            &job.job_id,
            job.total_size_bytes,
            job.part_size_bytes,
            job.total_parts,
            index,
        )?;
        parts.push(SplitFileManifestPartDocument {
            part_id: part.part_id,
            key: part.key,
            offset_bytes: part.offset_bytes,
            size_bytes: part.size_bytes,
        });
    }
    let last_part_size_bytes = parts.last().map(|part| part.size_bytes).unwrap_or(0);
    serde_json::to_vec_pretty(&SplitFileManifestDocument {
        manifest_version: 1,
        manifest_type: "split_file_manifest".to_string(),
        logical_format: "mbtiles".to_string(),
        logical_key: job.logical_key.clone(),
        manifest_key: job.manifest_key.clone(),
        storage_root: MAP_IMPORT_STORAGE_ROOT.to_string(),
        logical_size_bytes: job.total_size_bytes,
        last_part_size_bytes,
        parts_count: parts.len(),
        parts,
    })
    .context("failed serializing split manifest JSON")
}

fn validate_manifest_checkpoint(
    job: &MapDatasetImportJob,
) -> std::result::Result<(), ImportAdvanceError> {
    if job.completed_parts != job.total_parts {
        return Err(ImportAdvanceError::Fatal(format!(
            "cannot publish map manifest after {} of {} parts",
            job.completed_parts, job.total_parts
        )));
    }
    if job.completed_bytes != job.total_size_bytes {
        return Err(ImportAdvanceError::Fatal(format!(
            "cannot publish map manifest after {} of {} bytes",
            job.completed_bytes, job.total_size_bytes
        )));
    }
    if job.current_part_index != job.total_parts {
        return Err(ImportAdvanceError::Fatal(format!(
            "cannot publish map manifest at part index {} after {} parts",
            job.current_part_index, job.total_parts
        )));
    }
    if job.current_part_downloaded_bytes != 0 || !job.current_part_chunk_refs.is_empty() {
        return Err(ImportAdvanceError::Fatal(
            "cannot publish map manifest with an unfinished part checkpoint".to_string(),
        ));
    }
    validate_persisted_job_checkpoint(job).map_err(ImportAdvanceError::Fatal)?;
    Ok(())
}

fn validate_persisted_job_checkpoint(job: &MapDatasetImportJob) -> std::result::Result<(), String> {
    persisted_representation_validator(job.representation_validator.as_deref())?;
    if job.total_size_bytes == 0 {
        return Err("map import total size must be greater than zero".into());
    }
    let expected_total_parts = part_count_for_size(job.total_size_bytes, job.part_size_bytes)
        .map_err(|error| format!("invalid map import part geometry: {error:#}"))?;
    if job.total_parts != expected_total_parts {
        return Err(format!(
            "map import declares {} parts but its size and part size require {expected_total_parts}",
            job.total_parts
        ));
    }
    if job.completed_parts > job.total_parts {
        return Err(format!(
            "map import completed {} of only {} parts",
            job.completed_parts, job.total_parts
        ));
    }
    let completed_parts = u64::try_from(job.completed_parts)
        .map_err(|_| "map import completed part count does not fit in u64".to_string())?;
    let expected_completed_bytes = if job.completed_parts == job.total_parts {
        job.total_size_bytes
    } else {
        completed_parts
            .checked_mul(job.part_size_bytes)
            .ok_or_else(|| "map import completed byte count overflowed".to_string())?
    };
    if job.completed_bytes != expected_completed_bytes {
        return Err(format!(
            "map import completed byte count is {}, expected {expected_completed_bytes}",
            job.completed_bytes
        ));
    }
    if job.current_part_index != job.completed_parts {
        return Err(format!(
            "map import current part index is {}, expected {}",
            job.current_part_index, job.completed_parts
        ));
    }
    let checkpoint_bytes = chunk_refs_size_bytes(&job.current_part_chunk_refs);
    if checkpoint_bytes != job.current_part_downloaded_bytes {
        return Err(format!(
            "map import checkpoint has {checkpoint_bytes} bytes in chunk refs but records {} downloaded bytes",
            job.current_part_downloaded_bytes
        ));
    }
    if job.completed_parts == job.total_parts {
        if job.current_part_downloaded_bytes != 0 || !job.current_part_chunk_refs.is_empty() {
            return Err("completed map import retains an unfinished part checkpoint".into());
        }
    } else {
        let current_part = current_part_spec(job)
            .map_err(|error| match error {
                ImportAdvanceError::Fatal(error) | ImportAdvanceError::Transient(error) => error,
            })?
            .ok_or_else(|| "running map import has no current part".to_string())?;
        if job.current_part_downloaded_bytes > current_part.size_bytes {
            return Err(format!(
                "map import checkpoint exceeds the {} byte current part",
                current_part.size_bytes
            ));
        }
    }
    if job.manifest_uploaded && job.completed_parts != job.total_parts {
        return Err("map import manifest was published before every part completed".into());
    }
    Ok(())
}

fn part_count_for_size(total_size_bytes: u64, part_size_bytes: u64) -> Result<usize> {
    if part_size_bytes == 0 {
        bail!("part size must be greater than zero");
    }
    let quotient = total_size_bytes / part_size_bytes;
    let remainder = total_size_bytes % part_size_bytes;
    let part_count = quotient + u64::from(remainder != 0);
    usize::try_from(part_count).context("part count overflow")
}

fn split_suffix_width(total_parts: usize) -> usize {
    let mut width = 2usize;
    let mut capacity = 26usize.saturating_mul(26);
    while total_parts > capacity {
        width = width.saturating_add(1);
        capacity = capacity.saturating_mul(26);
    }
    width
}

fn split_suffix(mut index: usize, width: usize) -> String {
    let mut chars = vec!['a'; width];
    for slot in (0..width).rev() {
        chars[slot] = char::from(b'a'.saturating_add((index % 26) as u8));
        index /= 26;
    }
    chars.into_iter().collect()
}

fn chunk_refs_size_bytes(chunk_refs: &[UploadChunkRef]) -> u64 {
    chunk_refs.iter().fold(0u64, |acc, chunk| {
        acc.saturating_add(chunk.size_bytes as u64)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::extract::State as AxumState;
    use axum::http::{HeaderMap as AxumHeaderMap, HeaderValue};
    use axum::routing::get;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    const TEST_STRONG_ETAG: &str = "\"dataset-v1\"";

    #[derive(Clone, Copy)]
    enum TestRangeBehavior {
        TruncateFirstDownload { bytes: usize },
        InvalidContentRange,
        OmitContentRange,
        OmitValidator,
        ChangedRepresentation,
        MismatchedValidator,
        OversizedBody,
        Status(StatusCode),
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct TestRangeRequest {
        range: String,
        if_range: Option<String>,
        accept_encoding: Option<String>,
    }

    #[derive(Clone)]
    struct TestRangeState {
        payload: Arc<Vec<u8>>,
        behavior: TestRangeBehavior,
        truncate_pending: Arc<AtomicBool>,
        requests: Arc<Mutex<Vec<TestRangeRequest>>>,
    }

    struct TestRangeServer {
        source_url: String,
        requests: Arc<Mutex<Vec<TestRangeRequest>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl TestRangeServer {
        fn requests(&self) -> Vec<TestRangeRequest> {
            self.requests
                .lock()
                .expect("range request log should not be poisoned")
                .clone()
        }
    }

    impl Drop for TestRangeServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    fn test_http_response(
        status: StatusCode,
        payload: Vec<u8>,
        content_range: Option<String>,
        etag: Option<&str>,
    ) -> axum::response::Response {
        let mut response = axum::response::Response::new(Body::from(payload));
        *response.status_mut() = status;
        if let Some(content_range) = content_range {
            response.headers_mut().insert(
                reqwest::header::CONTENT_RANGE,
                HeaderValue::from_str(&content_range)
                    .expect("test Content-Range should be a valid header"),
            );
        }
        if let Some(etag) = etag {
            response.headers_mut().insert(
                reqwest::header::ETAG,
                HeaderValue::from_str(etag).expect("test ETag should be a valid header"),
            );
        }
        response
    }

    fn parse_requested_test_range(headers: &AxumHeaderMap) -> Option<(usize, usize)> {
        let value = headers.get(reqwest::header::RANGE)?.to_str().ok()?;
        let (start, end) = value.strip_prefix("bytes=")?.split_once('-')?;
        Some((start.parse().ok()?, end.parse().ok()?))
    }

    fn expected_test_range_request(
        range: &str,
        representation_validator: Option<&str>,
    ) -> TestRangeRequest {
        TestRangeRequest {
            range: range.to_string(),
            if_range: representation_validator.map(ToString::to_string),
            accept_encoding: Some("identity".to_string()),
        }
    }

    async fn serve_test_range(
        AxumState(state): AxumState<TestRangeState>,
        headers: AxumHeaderMap,
    ) -> axum::response::Response {
        let request = TestRangeRequest {
            range: headers
                .get(reqwest::header::RANGE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("<missing>")
                .to_string(),
            if_range: headers
                .get(reqwest::header::IF_RANGE)
                .and_then(|value| value.to_str().ok())
                .map(ToString::to_string),
            accept_encoding: headers
                .get(reqwest::header::ACCEPT_ENCODING)
                .and_then(|value| value.to_str().ok())
                .map(ToString::to_string),
        };
        state
            .requests
            .lock()
            .expect("range request log should not be poisoned")
            .push(request);

        if let TestRangeBehavior::Status(status) = state.behavior {
            return test_http_response(status, Vec::new(), None, None);
        }
        if matches!(state.behavior, TestRangeBehavior::ChangedRepresentation) {
            return test_http_response(
                StatusCode::OK,
                b"changed".to_vec(),
                None,
                Some("\"dataset-v2\""),
            );
        }

        let Some((start, end)) = parse_requested_test_range(&headers) else {
            return test_http_response(StatusCode::BAD_REQUEST, Vec::new(), None, None);
        };
        if start > end || end >= state.payload.len() {
            return test_http_response(StatusCode::RANGE_NOT_SATISFIABLE, Vec::new(), None, None);
        }

        let mut payload = state.payload[start..=end].to_vec();
        if let TestRangeBehavior::TruncateFirstDownload { bytes } = state.behavior
            && !(start == 0 && end == 0)
            && state.truncate_pending.swap(false, Ordering::SeqCst)
        {
            payload.truncate(bytes.min(payload.len()));
        }
        if matches!(state.behavior, TestRangeBehavior::OversizedBody) {
            payload.push(b'!');
        }

        let content_range = match state.behavior {
            TestRangeBehavior::InvalidContentRange => Some(format!(
                "bytes {}-{end}/{}",
                start.saturating_add(1),
                state.payload.len()
            )),
            TestRangeBehavior::OmitContentRange => None,
            _ => Some(format!("bytes {start}-{end}/{}", state.payload.len())),
        };
        let etag = match state.behavior {
            TestRangeBehavior::OmitValidator => None,
            TestRangeBehavior::MismatchedValidator => Some("\"dataset-v2\""),
            _ => Some(TEST_STRONG_ETAG),
        };
        test_http_response(StatusCode::PARTIAL_CONTENT, payload, content_range, etag)
    }

    async fn spawn_test_range_server(
        payload: &[u8],
        behavior: TestRangeBehavior,
    ) -> TestRangeServer {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let server_state = TestRangeState {
            payload: Arc::new(payload.to_vec()),
            behavior,
            truncate_pending: Arc::new(AtomicBool::new(true)),
            requests: requests.clone(),
        };
        let app = axum::Router::new()
            .route("/dataset.mbtiles", get(serve_test_range))
            .with_state(server_state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback listener should bind");
        let address = listener
            .local_addr()
            .expect("loopback listener should have an address");
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("loopback range server should run");
        });
        TestRangeServer {
            source_url: format!("http://{address}/dataset.mbtiles"),
            requests,
            task,
        }
    }

    fn test_import_job(probe: MapDatasetProbe, part_size_bytes: u64) -> MapDatasetImportJob {
        let now = unix_ts();
        MapDatasetImportJob {
            job_id: "map-import-test-job".to_string(),
            state: AdminMapDatasetImportState::Running,
            source_url: probe.source_url,
            representation_validator: probe.representation_validator,
            variant_id: None,
            asset: None,
            dataset_filename: probe.dataset_filename.clone(),
            logical_key: format!("{MAP_IMPORT_STORAGE_ROOT}/{}", probe.dataset_filename),
            manifest_key: format!(
                "{MAP_IMPORT_STORAGE_ROOT}/{}.manifest.json",
                probe.dataset_filename
            ),
            part_size_bytes,
            total_size_bytes: probe.total_size_bytes,
            total_parts: part_count_for_size(probe.total_size_bytes, part_size_bytes)
                .expect("test part count should be valid"),
            completed_parts: 0,
            completed_bytes: 0,
            current_part_index: 0,
            current_part_downloaded_bytes: 0,
            current_part_chunk_refs: Vec::new(),
            manifest_uploaded: false,
            retry_count: 0,
            next_retry_at_unix: None,
            last_error: None,
            started_at_unix: now,
            updated_at_unix: now,
            finished_at_unix: None,
        }
    }

    async fn install_test_import_job(state: &ServerState, job: &MapDatasetImportJob) {
        {
            let mut runtime = state.storage.map_dataset_import.lock().await;
            runtime.job = Some(job.clone());
            runtime.worker_running = false;
        }
        persist_runtime(state)
            .await
            .expect("test import job should persist");
    }

    async fn wait_for_terminal_import(state: &ServerState) -> AdminMapDatasetImportJobView {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let terminal = {
                    let runtime = state.storage.map_dataset_import.lock().await;
                    match runtime.job.as_ref() {
                        Some(job)
                            if job.state != AdminMapDatasetImportState::Running
                                && !runtime.worker_running =>
                        {
                            Some(job.view())
                        }
                        _ => None,
                    }
                };
                if let Some(terminal) = terminal {
                    return terminal;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("map import should reach a terminal state")
    }

    #[tokio::test]
    async fn import_resumes_persisted_checkpoint_and_publishes_split_manifest() {
        let state = crate::tests::build_sqlite_test_state().await;
        let source_payload = b"abcdefghij";
        let server = spawn_test_range_server(
            source_payload,
            TestRangeBehavior::TruncateFirstDownload { bytes: 3 },
        )
        .await;
        let http = build_map_import_http_client().expect("HTTP client should build");
        let probe = probe_download_source(&http, &server.source_url, None)
            .await
            .expect("loopback source should pass its range probe");
        assert_eq!(probe.dataset_filename, "dataset.mbtiles");
        assert_eq!(probe.total_size_bytes, source_payload.len() as u64);
        assert_eq!(
            probe.representation_validator.as_deref(),
            Some(TEST_STRONG_ETAG)
        );

        let job = test_import_job(probe, 6);
        install_test_import_job(&state, &job).await;
        assert!(has_running_job(&state).await);
        let first_part = current_part_spec(&job)
            .expect("part specification should be valid")
            .expect("the first part should exist");
        let interrupted = download_part_bytes_with_limits(&state, &http, &job, &first_part, 2, 2)
            .await
            .expect_err("a truncated response should be retried");
        let retry_error = match interrupted {
            ImportAdvanceError::Transient(error) => error,
            ImportAdvanceError::Fatal(error) => panic!("truncated response was fatal: {error}"),
        };
        assert!(retry_error.contains("ended after 3 of 6 bytes"));
        // Keep the persisted retry deadline due immediately. Delayed restart
        // scheduling is a separate contract from checkpoint resumption.
        note_transient_failure(&state, retry_error.clone(), 0)
            .await
            .expect("retry state should persist");

        let checkpoint = load_runtime(state.managed_paths.data_dir())
            .await
            .expect("checkpoint should reload");
        let checkpoint_job = checkpoint
            .job
            .as_ref()
            .expect("checkpoint should contain a job");
        assert_eq!(checkpoint_job.current_part_downloaded_bytes, 3);
        assert_eq!(
            checkpoint_job.representation_validator.as_deref(),
            Some(TEST_STRONG_ETAG)
        );
        assert_eq!(
            checkpoint_job
                .current_part_chunk_refs
                .iter()
                .map(|chunk| chunk.size_bytes)
                .collect::<Vec<_>>(),
            [2, 1]
        );
        assert_eq!(checkpoint_job.retry_count, 1);
        assert_eq!(
            checkpoint_job.last_error.as_deref(),
            Some(retry_error.as_str())
        );
        assert!(checkpoint_job.next_retry_at_unix.is_some());

        // Replace the in-memory runtime with the disk checkpoint to model a
        // process restart before resuming the exact remaining byte range.
        *state.storage.map_dataset_import.lock().await = checkpoint;
        spawn_resume_if_needed(state.clone()).await;
        let completed = wait_for_terminal_import(&state).await;
        assert_eq!(completed.state, AdminMapDatasetImportState::Completed);
        assert_eq!(completed.completed_parts, 2);
        assert_eq!(completed.completed_bytes, source_payload.len() as u64);
        assert_eq!(completed.current_part_index, None);
        assert!(completed.manifest_uploaded);
        assert_eq!(completed.retry_count, 0);
        assert_eq!(completed.last_error, None);
        assert_eq!(completed.progress_percent, 100.0);
        assert!(completed.finished_at_unix.is_some());
        assert!(!has_running_job(&state).await);

        let persisted = load_runtime(state.managed_paths.data_dir())
            .await
            .expect("terminal state should reload");
        let persisted_job = persisted.job.expect("terminal job should remain visible");
        assert_eq!(persisted_job.state, AdminMapDatasetImportState::Completed);
        assert_eq!(persisted_job.completed_bytes, source_payload.len() as u64);
        assert!(persisted_job.manifest_uploaded);
        assert_eq!(
            persisted_job.representation_validator.as_deref(),
            Some(TEST_STRONG_ETAG)
        );

        let manifest_payload = {
            let store = read_store(&state, "test.map_import.read_manifest").await;
            store
                .get_object(
                    &completed.manifest_key,
                    None,
                    None,
                    storage::ObjectReadMode::Preferred,
                )
                .await
                .expect("published manifest should be readable")
        };
        let manifest: serde_json::Value =
            serde_json::from_slice(&manifest_payload).expect("manifest should be JSON");
        assert_eq!(manifest["type"], "split_file_manifest");
        assert_eq!(manifest["logical_size_bytes"], 10);
        assert_eq!(manifest["last_part_size_bytes"], 4);
        assert_eq!(manifest["parts_count"], 2);
        let parts = manifest["parts"]
            .as_array()
            .expect("manifest should list its parts");
        let mut reconstructed = Vec::new();
        for (index, part) in parts.iter().enumerate() {
            assert_eq!(part["part_id"], ["aa", "ab"][index]);
            assert_eq!(part["offset_bytes"], [0, 6][index]);
            assert_eq!(part["size_bytes"], [6, 4][index]);
            let key = part["key"].as_str().expect("part should have a key");
            let payload = {
                let store = read_store(&state, "test.map_import.read_part").await;
                store
                    .get_object(key, None, None, storage::ObjectReadMode::Preferred)
                    .await
                    .expect("published part should be readable")
            };
            reconstructed.extend_from_slice(&payload);
        }
        assert_eq!(reconstructed, source_payload);
        assert_eq!(
            server.requests(),
            [
                expected_test_range_request("bytes=0-0", None),
                expected_test_range_request("bytes=0-5", Some(TEST_STRONG_ETAG)),
                expected_test_range_request("bytes=3-5", Some(TEST_STRONG_ETAG)),
                expected_test_range_request("bytes=6-9", Some(TEST_STRONG_ETAG)),
            ]
        );

        crate::tests::cleanup_sqlite_test_state(&state).await;
    }

    #[tokio::test]
    async fn import_persists_fatal_content_range_mismatch() {
        let state = crate::tests::build_sqlite_test_state().await;
        let server = spawn_test_range_server(b"data", TestRangeBehavior::InvalidContentRange).await;
        let job = test_import_job(
            MapDatasetProbe {
                source_url: server.source_url.clone(),
                representation_validator: Some(TEST_STRONG_ETAG.to_string()),
                dataset_filename: "dataset.mbtiles".to_string(),
                total_size_bytes: 4,
            },
            4,
        );
        install_test_import_job(&state, &job).await;

        spawn_resume_if_needed(state.clone()).await;
        let failed = wait_for_terminal_import(&state).await;
        assert_eq!(failed.state, AdminMapDatasetImportState::Failed);
        assert_eq!(failed.completed_bytes, 0);
        assert!(!failed.manifest_uploaded);
        assert!(failed.finished_at_unix.is_some());
        assert!(
            failed
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("returned bytes 1-3/4, expected bytes 0-3/4"))
        );
        assert_eq!(
            server.requests(),
            [expected_test_range_request(
                "bytes=0-3",
                Some(TEST_STRONG_ETAG),
            )]
        );

        let persisted = load_runtime(state.managed_paths.data_dir())
            .await
            .expect("failed state should reload");
        assert_eq!(
            persisted.job.expect("failed job should persist").state,
            AdminMapDatasetImportState::Failed
        );

        crate::tests::cleanup_sqlite_test_state(&state).await;
    }

    #[tokio::test]
    async fn import_fails_if_the_probed_representation_changes() {
        let state = crate::tests::build_sqlite_test_state().await;
        let server =
            spawn_test_range_server(b"data", TestRangeBehavior::ChangedRepresentation).await;
        let job = test_import_job(
            MapDatasetProbe {
                source_url: server.source_url.clone(),
                representation_validator: Some(TEST_STRONG_ETAG.to_string()),
                dataset_filename: "dataset.mbtiles".to_string(),
                total_size_bytes: 4,
            },
            4,
        );
        install_test_import_job(&state, &job).await;

        spawn_resume_if_needed(state.clone()).await;
        let failed = wait_for_terminal_import(&state).await;
        assert_eq!(failed.state, AdminMapDatasetImportState::Failed);
        assert_eq!(failed.completed_bytes, 0);
        assert!(
            failed
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("unexpected status 200 OK"))
        );
        assert_eq!(
            server.requests(),
            [expected_test_range_request(
                "bytes=0-3",
                Some(TEST_STRONG_ETAG),
            )]
        );

        let unsatisfiable = spawn_test_range_server(
            b"data",
            TestRangeBehavior::Status(StatusCode::RANGE_NOT_SATISFIABLE),
        )
        .await;
        let unsatisfiable_job = test_import_job(
            MapDatasetProbe {
                source_url: unsatisfiable.source_url.clone(),
                representation_validator: Some(TEST_STRONG_ETAG.to_string()),
                dataset_filename: "dataset.mbtiles".to_string(),
                total_size_bytes: 4,
            },
            4,
        );
        let part = current_part_spec(&unsatisfiable_job)
            .expect("part specification should be valid")
            .expect("test job should have a part");
        let error = download_part_bytes(
            &state,
            &build_map_import_http_client().expect("HTTP client should build"),
            &unsatisfiable_job,
            &part,
        )
        .await
        .expect_err("416 during a range download must be fatal");
        assert!(matches!(error, ImportAdvanceError::Fatal(_)));
        assert_eq!(
            unsatisfiable.requests(),
            [expected_test_range_request(
                "bytes=0-3",
                Some(TEST_STRONG_ETAG),
            )]
        );

        crate::tests::cleanup_sqlite_test_state(&state).await;
    }

    #[tokio::test]
    async fn legacy_running_import_without_validator_fails_before_network_or_ingest() {
        let state = crate::tests::build_sqlite_test_state().await;
        let server = spawn_test_range_server(
            b"data",
            TestRangeBehavior::TruncateFirstDownload { bytes: 2 },
        )
        .await;
        let job = test_import_job(
            MapDatasetProbe {
                source_url: server.source_url.clone(),
                representation_validator: None,
                dataset_filename: "dataset.mbtiles".to_string(),
                total_size_bytes: 4,
            },
            4,
        );
        install_test_import_job(&state, &job).await;

        spawn_resume_if_needed(state.clone()).await;
        let failed = wait_for_terminal_import(&state).await;
        assert_eq!(failed.state, AdminMapDatasetImportState::Failed);
        assert_eq!(failed.completed_bytes, 0);
        assert!(
            failed
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("without a representation validator"))
        );
        assert!(server.requests().is_empty());
        let persisted = load_runtime(state.managed_paths.data_dir())
            .await
            .expect("failed legacy state should reload");
        let persisted = persisted.job.expect("failed legacy job should persist");
        assert_eq!(persisted.state, AdminMapDatasetImportState::Failed);
        assert!(persisted.current_part_chunk_refs.is_empty());

        crate::tests::cleanup_sqlite_test_state(&state).await;
    }

    #[tokio::test]
    async fn corrupted_persisted_checkpoints_fail_before_http_or_store_side_effects() {
        let state = crate::tests::build_sqlite_test_state().await;
        let server = spawn_test_range_server(
            b"abcdefghij",
            TestRangeBehavior::TruncateFirstDownload { bytes: 3 },
        )
        .await;
        let base = test_import_job(
            MapDatasetProbe {
                source_url: server.source_url.clone(),
                representation_validator: Some(TEST_STRONG_ETAG.to_string()),
                dataset_filename: "dataset.mbtiles".to_string(),
                total_size_bytes: 10,
            },
            6,
        );

        let mut wrong_part_geometry = base.clone();
        wrong_part_geometry.total_parts = 3;
        let mut wrong_completed_bytes = base.clone();
        wrong_completed_bytes.completed_parts = 1;
        wrong_completed_bytes.completed_bytes = 5;
        wrong_completed_bytes.current_part_index = 1;
        let mut wrong_checkpoint_sum = base.clone();
        wrong_checkpoint_sum.current_part_downloaded_bytes = 3;
        wrong_checkpoint_sum.current_part_chunk_refs = vec![UploadChunkRef {
            hash: "incomplete-checkpoint".to_string(),
            size_bytes: 2,
        }];
        let mut blank_validator = base.clone();
        blank_validator.representation_validator = Some("   ".to_string());
        let mut invalid_header_validator = base.clone();
        invalid_header_validator.representation_validator = Some("invalid\nvalidator".to_string());

        for (job, expected_error) in [
            (wrong_part_geometry, "size and part size require 2"),
            (wrong_completed_bytes, "byte count is 5, expected 6"),
            (
                wrong_checkpoint_sum,
                "2 bytes in chunk refs but records 3 downloaded bytes",
            ),
            (blank_validator, "without a representation validator"),
            (invalid_header_validator, "not a valid HTTP header value"),
        ] {
            install_test_import_job(&state, &job).await;
            let persisted = load_runtime(state.managed_paths.data_dir())
                .await
                .expect("corrupted checkpoint fixture should reload");
            *state.storage.map_dataset_import.lock().await = persisted;

            spawn_resume_if_needed(state.clone()).await;
            let failed = wait_for_terminal_import(&state).await;
            assert_eq!(failed.state, AdminMapDatasetImportState::Failed);
            assert!(
                failed
                    .last_error
                    .as_deref()
                    .is_some_and(|error| error.contains(expected_error)),
                "unexpected invariant error: {:?}",
                failed.last_error
            );
            assert!(server.requests().is_empty());
        }

        let manifest_read = {
            let store = read_store(&state, "test.map_import.corrupt_checkpoint_manifest").await;
            store
                .get_object(
                    &base.manifest_key,
                    None,
                    None,
                    storage::ObjectReadMode::Preferred,
                )
                .await
        };
        assert!(manifest_read.is_err());

        crate::tests::cleanup_sqlite_test_state(&state).await;
    }

    #[tokio::test]
    async fn range_download_rejects_mismatched_validator_and_oversized_body_before_ingest() {
        let state = crate::tests::build_sqlite_test_state().await;
        let http = build_map_import_http_client().expect("HTTP client should build");

        for (behavior, expected_error) in [
            (
                TestRangeBehavior::MismatchedValidator,
                "changed representation validator",
            ),
            (
                TestRangeBehavior::OversizedBody,
                "exceeded the declared remaining byte range",
            ),
        ] {
            let server = spawn_test_range_server(b"data", behavior).await;
            let job = test_import_job(
                MapDatasetProbe {
                    source_url: server.source_url.clone(),
                    representation_validator: Some(TEST_STRONG_ETAG.to_string()),
                    dataset_filename: "dataset.mbtiles".to_string(),
                    total_size_bytes: 4,
                },
                4,
            );
            install_test_import_job(&state, &job).await;
            let part = current_part_spec(&job)
                .expect("part specification should be valid")
                .expect("test job should have a part");

            let error = download_part_bytes(&state, &http, &job, &part)
                .await
                .expect_err("invalid representation response must be fatal");
            let ImportAdvanceError::Fatal(error) = error else {
                panic!("invalid representation response was retryable");
            };
            assert!(error.contains(expected_error), "unexpected error: {error}");
            {
                let runtime = state.storage.map_dataset_import.lock().await;
                let active = runtime.job.as_ref().expect("test job should remain active");
                assert_eq!(active.current_part_downloaded_bytes, 0);
                assert!(active.current_part_chunk_refs.is_empty());
            }
            assert_eq!(
                server.requests(),
                [expected_test_range_request(
                    "bytes=0-3",
                    Some(TEST_STRONG_ETAG),
                )]
            );
        }

        crate::tests::cleanup_sqlite_test_state(&state).await;
    }

    #[tokio::test]
    async fn range_probe_classifies_source_contract_failures() {
        let http = build_map_import_http_client().expect("HTTP client should build");
        for (status, expected_status, expected_fatal) in [
            (StatusCode::NO_CONTENT, StatusCode::BAD_REQUEST, true),
            (StatusCode::NOT_FOUND, StatusCode::BAD_REQUEST, true),
            (StatusCode::GONE, StatusCode::BAD_REQUEST, true),
            (
                StatusCode::PRECONDITION_FAILED,
                StatusCode::BAD_REQUEST,
                true,
            ),
            (
                StatusCode::RANGE_NOT_SATISFIABLE,
                StatusCode::BAD_REQUEST,
                true,
            ),
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                StatusCode::BAD_REQUEST,
                true,
            ),
            (StatusCode::REQUEST_TIMEOUT, StatusCode::BAD_GATEWAY, false),
            (
                StatusCode::TOO_MANY_REQUESTS,
                StatusCode::BAD_GATEWAY,
                false,
            ),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                StatusCode::BAD_GATEWAY,
                false,
            ),
        ] {
            let server = spawn_test_range_server(b"data", TestRangeBehavior::Status(status)).await;
            let error = probe_download_source(&http, &server.source_url, None)
                .await
                .expect_err("non-partial response should fail probing");
            assert_eq!(
                matches!(error, ImportAdvanceError::Fatal(_)),
                expected_fatal
            );
            assert_eq!(import_error_status(error), expected_status);
            assert_eq!(
                server.requests(),
                [expected_test_range_request("bytes=0-0", None)]
            );
        }

        let server = spawn_test_range_server(b"data", TestRangeBehavior::OmitContentRange).await;
        let error = probe_download_source(&http, &server.source_url, None)
            .await
            .expect_err("a range probe without Content-Range should fail");
        assert!(matches!(error, ImportAdvanceError::Fatal(_)));
        assert_eq!(import_error_status(error), StatusCode::BAD_REQUEST);
        assert_eq!(
            server.requests(),
            [expected_test_range_request("bytes=0-0", None)]
        );

        let server = spawn_test_range_server(b"data", TestRangeBehavior::OmitValidator).await;
        let error = probe_download_source(&http, &server.source_url, None)
            .await
            .expect_err("a range probe without a validator should fail");
        let ImportAdvanceError::Fatal(error) = error else {
            panic!("missing source validator was retryable");
        };
        assert!(error.contains("missing a strong ETag or Last-Modified"));
        assert_eq!(server.requests().len(), 1);
    }

    #[test]
    fn extracts_url_from_wget_command() {
        let url = extract_source_url(
            "wget -c https://data.maptiler.com/download/token/maptiler-satellite-2017-11-02-planet.mbtiles",
        )
        .expect("url should parse");
        assert_eq!(
            url,
            "https://data.maptiler.com/download/token/maptiler-satellite-2017-11-02-planet.mbtiles"
        );
    }

    #[test]
    fn split_suffix_grows_beyond_two_letters() {
        assert_eq!(split_suffix(0, split_suffix_width(10)), "aa");
        assert_eq!(split_suffix(25, split_suffix_width(26)), "az");
        assert_eq!(split_suffix(26, split_suffix_width(27)), "ba");
        assert_eq!(
            split_suffix(26 * 26, split_suffix_width(26 * 26 + 1)),
            "baa"
        );
    }

    #[test]
    fn part_count_handles_values_near_u64_max() {
        assert_eq!(part_count_for_size(u64::MAX, u64::MAX - 1).unwrap(), 2);
    }

    #[test]
    fn parses_content_range() {
        assert_eq!(
            parse_content_range("bytes 1024-2047/4096"),
            Some((1024, 2047, 4096))
        );
        assert_eq!(parse_content_range("bytes */4096"), None);
    }

    #[test]
    fn representation_validator_prefers_strong_etag_and_falls_back_to_last_modified() {
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::ETAG,
            HeaderValue::from_static("W/\"weak\""),
        );
        headers.insert(
            reqwest::header::LAST_MODIFIED,
            HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT"),
        );
        assert_eq!(
            representation_validator(&headers).as_deref(),
            Some("Wed, 21 Oct 2015 07:28:00 GMT")
        );

        headers.insert(reqwest::header::ETAG, HeaderValue::from_static("\"\""));
        assert_eq!(
            representation_validator(&headers).as_deref(),
            Some("Wed, 21 Oct 2015 07:28:00 GMT")
        );

        headers.insert(
            reqwest::header::ETAG,
            HeaderValue::from_static(TEST_STRONG_ETAG),
        );
        assert_eq!(
            representation_validator(&headers).as_deref(),
            Some(TEST_STRONG_ETAG)
        );
    }

    #[test]
    fn persisted_representation_validator_is_trimmed_and_header_safe() {
        assert_eq!(
            persisted_representation_validator(Some("  \"dataset-v1\"\t")).unwrap(),
            TEST_STRONG_ETAG
        );
        assert!(persisted_representation_validator(Some("dataset\n-v1")).is_err());
    }

    #[test]
    fn manifest_publication_requires_a_fully_finalized_checkpoint() {
        let mut complete = test_import_job(
            MapDatasetProbe {
                source_url: "https://example.invalid/dataset.mbtiles".to_string(),
                representation_validator: Some(TEST_STRONG_ETAG.to_string()),
                dataset_filename: "dataset.mbtiles".to_string(),
                total_size_bytes: 10,
            },
            6,
        );
        complete.completed_parts = 2;
        complete.completed_bytes = 10;
        complete.current_part_index = 2;
        assert!(validate_manifest_checkpoint(&complete).is_ok());

        let mut incomplete_parts = complete.clone();
        incomplete_parts.completed_parts = 1;
        assert!(matches!(
            validate_manifest_checkpoint(&incomplete_parts),
            Err(ImportAdvanceError::Fatal(error)) if error.contains("after 1 of 2 parts")
        ));

        let mut incomplete_bytes = complete.clone();
        incomplete_bytes.completed_bytes = 9;
        assert!(matches!(
            validate_manifest_checkpoint(&incomplete_bytes),
            Err(ImportAdvanceError::Fatal(error)) if error.contains("after 9 of 10 bytes")
        ));

        let mut wrong_index = complete.clone();
        wrong_index.current_part_index = 1;
        assert!(matches!(
            validate_manifest_checkpoint(&wrong_index),
            Err(ImportAdvanceError::Fatal(error)) if error.contains("at part index 1 after 2 parts")
        ));

        let mut partial_checkpoint = complete.clone();
        partial_checkpoint.current_part_downloaded_bytes = 1;
        assert!(matches!(
            validate_manifest_checkpoint(&partial_checkpoint),
            Err(ImportAdvanceError::Fatal(error)) if error.contains("unfinished part checkpoint")
        ));

        let mut retained_chunks = complete;
        retained_chunks
            .current_part_chunk_refs
            .push(UploadChunkRef {
                hash: "stale-chunk".to_string(),
                size_bytes: 1,
            });
        assert!(matches!(
            validate_manifest_checkpoint(&retained_chunks),
            Err(ImportAdvanceError::Fatal(error)) if error.contains("unfinished part checkpoint")
        ));
    }

    #[test]
    fn configured_import_does_not_require_mbtiles_in_download_url() {
        let signed_download =
            Url::parse("https://downloads.example.test/api/v1/artifacts/123?signature=abc")
                .expect("test URL should parse");

        let filename =
            probe_dataset_filename(&signed_download, Some("natural-earth-globe.mbtiles"))
                .expect("configured imports use their configured destination filename");

        assert_eq!(filename, "natural-earth-globe.mbtiles");
    }

    #[test]
    fn legacy_import_still_requires_mbtiles_in_download_url() {
        let signed_download =
            Url::parse("https://downloads.example.test/api/v1/artifacts/123?signature=abc")
                .expect("test URL should parse");

        assert!(probe_dataset_filename(&signed_download, None).is_err());
    }

    #[test]
    fn manifest_builder_uses_variable_part_size() {
        let job = MapDatasetImportJob {
            job_id: "job".to_string(),
            state: AdminMapDatasetImportState::Running,
            source_url: "https://example.invalid/file.mbtiles".to_string(),
            representation_validator: None,
            variant_id: None,
            asset: None,
            dataset_filename: "file.mbtiles".to_string(),
            logical_key: "sys/maps/file.mbtiles".to_string(),
            manifest_key: "sys/maps/file.mbtiles.manifest.json".to_string(),
            part_size_bytes: 1024,
            total_size_bytes: 2500,
            total_parts: 3,
            completed_parts: 0,
            completed_bytes: 0,
            current_part_index: 0,
            current_part_downloaded_bytes: 0,
            current_part_chunk_refs: Vec::new(),
            manifest_uploaded: false,
            retry_count: 0,
            next_retry_at_unix: None,
            last_error: None,
            started_at_unix: 1,
            updated_at_unix: 1,
            finished_at_unix: None,
        };

        let payload = build_split_manifest_bytes(&job).expect("manifest should build");
        let json: serde_json::Value =
            serde_json::from_slice(&payload).expect("manifest should decode");
        assert_eq!(json["parts_count"], 3);
        assert_eq!(json["last_part_size_bytes"], 452);
        assert_eq!(
            json["parts"][1]["key"],
            "sys/maps/file.mbtiles.import-job-part-ab"
        );
    }
}
