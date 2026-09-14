//! Recovery by immutable content identity. Availability is a source hint, never
//! proof that an unadvertised peer cannot contribute a missing chunk.
use super::*;
use futures_util::{StreamExt, stream};
use storage::content_recovery::{ContentRepairTask, validate_manifest};
use storage::retained_content::{MANIFEST_SUBJECT_PREFIX, RetainedContent, RetainedReference};

const CHUNK_FETCH_CONCURRENCY: usize = 4;
const PEER_FETCH_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const READ_THROUGH_RECOVERY_BUDGET: Duration = Duration::from_secs(30);
/// Bounds one durable repair pass. Its persisted task and any verified chunks
/// survive cancellation, so a later pass resumes with a fresh peer snapshot.
const DURABLE_CONTENT_REPAIR_BUDGET: Duration = Duration::from_secs(2 * 60);
const MAX_RECOVERY_ERRORS: usize = 16;

#[derive(Debug)]
struct NoContentSource(String);

impl std::fmt::Display for NoContentSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NoContentSource {}

#[derive(Debug)]
pub(crate) struct DurableRepairBudgetExceeded {
    budget: Duration,
}

impl std::fmt::Display for DurableRepairBudgetExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "durable content repair pass exceeded its {} second budget",
            self.budget.as_secs()
        )
    }
}

impl std::error::Error for DurableRepairBudgetExceeded {}

async fn source_fingerprint(state: &ServerState) -> String {
    let sources = source_nodes(state, "", None).await;
    let identities = sources
        .iter()
        .map(|node| (node.node_id, &node.reachability))
        .collect::<Vec<_>>();
    blake3::hash(&serde_json::to_vec(&identities).expect("source identities serialize"))
        .to_hex()
        .to_string()
}

pub(crate) async fn scrubber(state: &ServerState) -> Result<storage::DataScrubber> {
    let (scrubber, retained) = {
        let store = read_store(state, "content_recovery.scrub_snapshot").await;
        (
            store.data_scrubber().await?,
            store.retained_content().await?,
        )
    };
    let required = required_manifests(state, &retained).await;
    Ok(scrubber
        .with_required_manifests(required)
        .with_retained_content(retained))
}

#[derive(Default)]
pub(crate) struct ChunkRecoveryResult {
    pub recovered: usize,
    pub remaining: Vec<String>,
    pub errors: Vec<String>,
    has_local_error: bool,
}

pub(crate) async fn source_nodes(
    state: &ServerState,
    subject: &str,
    preferred: Option<&NodeDescriptor>,
) -> Vec<NodeDescriptor> {
    let mut cluster = state.cluster.lock().await;
    cluster.update_health_and_detect_offline_transition();
    let mut seen = HashSet::from([state.node_id]);
    let mut result = Vec::new();
    for node in preferred
        .into_iter()
        .cloned()
        .chain(cluster.available_nodes_for_subject(subject))
        .chain(cluster.replica_nodes_for_subject(subject))
        .chain(cluster.list_nodes())
    {
        if node.status == cluster::NodeStatus::Online && seen.insert(node.node_id) {
            result.push(node);
        }
    }
    result
}

pub(crate) async fn required_manifests(
    state: &ServerState,
    retained: &RetainedContent,
) -> HashSet<String> {
    // Placement is CPU-heavy for a deep retained history. Snapshot the small
    // mutable cluster view, then score distinct subjects without blocking
    // heartbeats, peer requests, or availability updates behind the mutex.
    let placement = state.cluster.lock().await.placement_snapshot();
    let mut assigned_by_placement_key = HashMap::<String, bool>::new();
    let assigned_subjects = retained
        .subjects()
        .into_iter()
        .filter(|subject| {
            let placement_key = cluster::replication_placement_key(subject);
            *assigned_by_placement_key
                .entry(placement_key.to_string())
                .or_insert_with(|| {
                    placement
                        .placement_for_key(placement_key)
                        .selected_nodes
                        .contains(&state.node_id)
                })
        })
        .collect::<HashSet<_>>();
    retained
        .manifests
        .iter()
        .filter(|(_, references)| {
            references
                .keys()
                .any(|subject| assigned_subjects.contains(subject))
        })
        .map(|(hash, _)| hash.clone())
        .collect()
}

async fn request_bytes(state: &ServerState, peer: &NodeDescriptor, path: &str) -> Result<Vec<u8>> {
    let response = tokio::time::timeout(
        PEER_FETCH_TIMEOUT,
        execute_peer_request(
            state,
            peer,
            reqwest::Method::GET,
            path,
            Vec::new(),
            Vec::new(),
        ),
    )
    .await
    .context("content source timed out")??;
    if !response.is_success() {
        bail!(
            "content source {} returned HTTP {}",
            peer.node_id,
            response.status
        );
    }
    Ok(response.body.to_vec())
}

async fn recover_chunk(
    state: &ServerState,
    chunk: &ReplicationChunkInfo,
    sources: &[NodeDescriptor],
    cache: bool,
) -> Result<bool> {
    {
        let store = read_store(state, "content_recovery.check_chunk").await;
        if let Ok(Some(bytes)) = store.read_chunk_payload(&chunk.hash).await
            && bytes.len() == chunk.size_bytes
        {
            return Ok(false);
        }
    }
    let path = format!("/cluster/v2/replication/chunk/{}", chunk.hash);
    let mut last_error = "no online content source".to_string();
    for source in sources {
        match request_bytes(state, source, &path).await {
            Ok(bytes)
                if bytes.len() == chunk.size_bytes
                    && blake3::hash(&bytes).to_hex().as_str() == chunk.hash =>
            {
                let store = lock_store(state, "content_recovery.install_chunk").await;
                // The peer response was size- and BLAKE3-validated above.
                // Avoid immediately hashing the same bytes again while storing.
                store.ingest_verified_chunk(&chunk.hash, &bytes).await?;
                if cache {
                    store
                        .note_cached_chunk_fetch(
                            &chunk.hash,
                            chunk.size_bytes,
                            Some(&source.node_id.to_string()),
                        )
                        .await?;
                }
                return Ok(true);
            }
            Ok(_) => {
                last_error = format!("source {} returned invalid size or hash", source.node_id)
            }
            Err(error) => last_error = format!("{error:#}"),
        }
    }
    Err(NoContentSource(format!("chunk {} unresolved: {last_error}", chunk.hash)).into())
}

pub(crate) async fn recover_chunks(
    state: &ServerState,
    subject: &str,
    chunks: &[ReplicationChunkInfo],
    preferred: Option<&NodeDescriptor>,
    cache: bool,
) -> ChunkRecoveryResult {
    recover_chunks_with_progress(state, subject, chunks, preferred, cache, None).await
}

async fn recover_chunks_with_progress(
    state: &ServerState,
    subject: &str,
    chunks: &[ReplicationChunkInfo],
    preferred: Option<&NodeDescriptor>,
    cache: bool,
    recovered_progress: Option<Arc<AtomicUsize>>,
) -> ChunkRecoveryResult {
    let sources = Arc::new(source_nodes(state, subject, preferred).await);
    let unique: BTreeMap<_, _> = chunks.iter().map(|c| (c.hash.clone(), c.clone())).collect();
    let mut work = stream::iter(unique.into_values().map(|chunk| {
        let state = state.clone();
        let sources = sources.clone();
        let recovered_progress = recovered_progress.clone();
        async move {
            let outcome = recover_chunk(&state, &chunk, &sources, cache).await;
            if matches!(&outcome, Ok(true))
                && let Some(recovered_progress) = recovered_progress
            {
                recovered_progress.fetch_add(1, Ordering::Relaxed);
            }
            (chunk.hash, outcome)
        }
    }))
    .buffer_unordered(CHUNK_FETCH_CONCURRENCY);
    let mut result = ChunkRecoveryResult::default();
    while let Some((hash, outcome)) = work.next().await {
        match outcome {
            Ok(true) => result.recovered += 1,
            Ok(false) => {}
            Err(error) => {
                result.remaining.push(hash.clone());
                result.has_local_error |= !error.is::<NoContentSource>();
                if result.errors.len() < MAX_RECOVERY_ERRORS {
                    result.errors.push(format!("{error:#}"));
                }
            }
        }
    }
    result
}

pub(crate) async fn recover_chunks_for_read(
    state: &ServerState,
    subject: &str,
    chunks: &[ReplicationChunkInfo],
    budget: Duration,
) -> Result<ChunkRecoveryResult> {
    // Bound the entire foreground operation, not just each peer request. A
    // larger cluster or missing range must not multiply request latency without
    // limit. Cancellation keeps already verified cache bytes reusable on retry.
    tokio::time::timeout(budget, recover_chunks(state, subject, chunks, None, true))
        .await
        .context("read-through recovery deadline exceeded")
}

async fn recover_manifest(state: &ServerState, reference: &RetainedReference) -> Result<Vec<u8>> {
    {
        let store = read_store(state, "content_recovery.check_manifest").await;
        if let Ok(Some(bytes)) = store.read_recovery_manifest(&reference.manifest_hash).await {
            return Ok(bytes);
        }
    }
    let subject = reference.subject().unwrap_or_default();
    let sources = source_nodes(state, &subject, None).await;
    let path = format!(
        "/cluster/v2/replication/manifest/{}",
        reference.manifest_hash
    );
    for source in &sources {
        if let Ok(bytes) = request_bytes(state, source, &path).await
            && validate_manifest(&reference.manifest_hash, &bytes).is_ok()
        {
            return Ok(bytes);
        }
        // Mixed-version clusters may not have the manifest-by-hash endpoint yet.
        if let Some(key) = &reference.key {
            let path =
                replication::build_replication_export_path(key, reference.version_id.as_deref());
            if let Ok(bytes) = request_bytes(state, source, &path).await
                && let Ok(bundle) =
                    serde_json::from_slice::<storage::ReplicationExportBundle>(&bytes)
                && bundle.manifest_hash == reference.manifest_hash
                && validate_manifest(&reference.manifest_hash, &bundle.manifest_bytes).is_ok()
            {
                return Ok(bundle.manifest_bytes);
            }
        }
    }
    Err(NoContentSource(format!(
        "no source supplied the expected manifest {}",
        reference.manifest_hash
    ))
    .into())
}

pub(crate) async fn bounded_durable_recovery<T>(
    budget: Duration,
    recovery: impl Future<Output = Result<T>>,
) -> Result<T> {
    tokio::time::timeout(budget, recovery)
        .await
        .map_err(|_| DurableRepairBudgetExceeded { budget })?
}

async fn recover_task(state: &ServerState, task: &mut ContentRepairTask) -> Result<usize> {
    recover_task_with_budget(state, task, DURABLE_CONTENT_REPAIR_BUDGET).await
}

pub(crate) async fn recover_task_with_budget(
    state: &ServerState,
    task: &mut ContentRepairTask,
    budget: Duration,
) -> Result<usize> {
    let recovered_progress = Arc::new(AtomicUsize::new(0));
    let outcome = bounded_durable_recovery(
        budget,
        recover_task_unbounded(state, task, recovered_progress.clone()),
    )
    .await;
    task.recovered_chunks = task
        .recovered_chunks
        .saturating_add(recovered_progress.load(Ordering::Relaxed));
    outcome
}

async fn recover_task_unbounded(
    state: &ServerState,
    task: &mut ContentRepairTask,
    recovered_progress: Arc<AtomicUsize>,
) -> Result<usize> {
    let bytes = recover_manifest(state, &task.reference).await?;
    let manifest = validate_manifest(&task.reference.manifest_hash, &bytes)?;
    {
        // This pass only reads chunk presence and persists metadata through
        // `PersistentStore`'s interior synchronization. Keeping the global
        // store lock shared lets ordinary reads proceed while large manifests
        // are prepared for recovery.
        let store = read_store(state, "content_recovery.prepare").await;
        task.chunks.clear();
        for chunk in manifest.chunks {
            // Metadata-only nodes repair damaged cached bytes without hydrating
            // absent cache entries or acquiring replica ownership. Do not hash
            // here: `recover_chunks` validates every included local entry once.
            if task.repair_chunks
                || !matches!(store.chunk_path_exists(&chunk.hash).await, Ok(false))
            {
                task.chunks.push(chunk);
            }
        }
        store.persist_content_repair_task(task).await?;
        store
            .install_recovery_manifest(&task.reference.manifest_hash, &bytes)
            .await?;
    }
    let subject = task.reference.subject().unwrap_or_default();
    let result = recover_chunks_with_progress(
        state,
        &subject,
        &task.chunks,
        None,
        !task.repair_chunks,
        Some(recovered_progress),
    )
    .await;
    if !result.remaining.is_empty() {
        let detail = format!(
            "{} chunks recovered; {} still missing; {}",
            result.recovered,
            result.remaining.len(),
            result.errors.join("; ")
        );
        if result.has_local_error {
            bail!(detail);
        }
        return Err(NoContentSource(detail).into());
    }
    // Completion re-hashes every recovered byte. It must retain the content
    // GC gate inside `finish_content_repair`, but does not mutate the store
    // object itself, so do not monopolize the global store lock for the scan.
    let store = read_store(state, "content_recovery.finish").await;
    store.finish_content_repair(task).await?;
    Ok(result.recovered)
}

fn empty_report() -> replication::ReplicationRepairReport {
    replication::ReplicationRepairReport {
        attempted_transfers: 0,
        successful_transfers: 0,
        failed_transfers: 0,
        skipped_items: 0,
        skipped_backoff: 0,
        skipped_max_retries: 0,
        skipped_details: Vec::new(),
        detailed_log: Vec::new(),
        last_error: None,
    }
}

fn log_outcome(
    state: &ServerState,
    report: &mut replication::ReplicationRepairReport,
    task: &ContentRepairTask,
    event: &str,
    detail: String,
    context: serde_json::Value,
) {
    replication::push_repair_log_entry(
        &mut report.detailed_log,
        state.node_id,
        event,
        detail,
        task.reference.subject(),
        task.reference.key.clone(),
        task.reference.version_id.clone(),
        None,
        Some(state.node_id),
        Some(context),
    );
}

pub(crate) async fn repair_subjects(
    state: &ServerState,
    subjects: Vec<String>,
    limit: Option<usize>,
) -> replication::ReplicationRepairReport {
    let mut report = empty_report();
    if let Err(error) = repair_subjects_inner(state, subjects, limit, &mut report).await {
        report.failed_transfers += 1;
        report.last_error = Some(format!("{error:#}"));
    }
    report
}

async fn repair_subjects_inner(
    state: &ServerState,
    subjects: Vec<String>,
    limit: Option<usize>,
    report: &mut replication::ReplicationRepairReport,
) -> Result<()> {
    let retained = {
        let store = read_store(state, "content_recovery.catalog").await;
        store.retained_content().await?
    };
    let required = required_manifests(state, &retained).await;
    let fingerprint = source_fingerprint(state).await;
    let mut references = BTreeMap::new();
    for subject in subjects {
        let reference = retained.reference_for_subject(&subject).or_else(|| {
            subject
                .strip_prefix(MANIFEST_SUBJECT_PREFIX)
                .and_then(|hash| retained.manifests.get(hash))
                .and_then(|references| references.values().next())
        });
        let Some(reference) =
            reference.filter(|r| r.manifest_hash != storage::TOMBSTONE_MANIFEST_HASH)
        else {
            report.skipped_items += 1;
            let detail =
                "retained content reference is no longer present; discarding stale repair task";
            replication::push_repair_log_entry(
                &mut report.detailed_log,
                state.node_id,
                "subject_skipped",
                detail,
                Some(subject.clone()),
                None,
                None,
                None,
                Some(state.node_id),
                Some(json!({"reason": "retained_reference_unavailable"})),
            );
            replication::push_repair_skipped_detail(
                &mut report.skipped_details,
                state.node_id,
                subject.clone(),
                None,
                None,
                None,
                Some(state.node_id),
                replication::ReplicationRepairSkipReason::RetainedReferenceUnavailable,
                detail,
            );
            if let Some(hash) = subject.strip_prefix(MANIFEST_SUBJECT_PREFIX) {
                read_store(state, "content_recovery.expired")
                    .await
                    .discard_content_repair_task(hash)
                    .await?;
            }
            continue;
        };
        references.insert(reference.manifest_hash.clone(), reference);
    }
    let requested_hashes = references.keys().cloned().collect::<Vec<_>>();
    let pending: HashMap<_, _> = read_store(state, "content_recovery.selected_pending")
        .await
        .content_repair_tasks_for_manifests(&requested_hashes)
        .await?
        .into_iter()
        .map(|task| (task.reference.manifest_hash.clone(), task))
        .collect();
    let mut tasks = BTreeMap::new();
    for reference in references.into_values() {
        let owned = read_store(state, "content_recovery.ownership")
            .await
            .manifest_is_owned(&reference.manifest_hash)
            .await?;
        let mut task = pending
            .get(&reference.manifest_hash)
            .cloned()
            .unwrap_or_else(|| {
                ContentRepairTask::new(
                    reference.clone(),
                    owned || required.contains(&reference.manifest_hash),
                )
            });
        task.reference = reference.clone();
        // An assigned repair remains a durability obligation during placement
        // changes. Normal replica handoff/cleanup releases it after verification.
        task.repair_chunks |= owned || required.contains(&reference.manifest_hash);
        tasks.insert(reference.manifest_hash.clone(), task);
    }
    let availability_may_have_changed = !tasks.is_empty();
    for (index, mut task) in tasks.into_values().enumerate() {
        let _claim = state
            .maintenance
            .content_repair_claims
            .claim(&task.reference.manifest_hash)
            .await;
        let requested_reference = task.reference.clone();
        let requested_repair_chunks = task.repair_chunks;
        let existing = read_store(state, "content_recovery.claimed_task")
            .await
            .content_repair_tasks_for_manifests(std::slice::from_ref(
                &requested_reference.manifest_hash,
            ))
            .await?
            .into_iter()
            .next();
        if let Some(mut existing) = existing {
            existing.reference = requested_reference.clone();
            existing.repair_chunks |= requested_repair_chunks;
            task = existing;
        }
        // The execution budget bounds this pass, not the lifetime of its work.
        read_store(state, "content_recovery.enqueue")
            .await
            .persist_content_repair_task(&task)
            .await?;
        if index >= limit.unwrap_or(usize::MAX) {
            report.skipped_items += 1;
            log_outcome(
                state,
                report,
                &task,
                "repair_waiting",
                "repair queued for a later execution batch".to_string(),
                json!({"pending": true}),
            );
            continue;
        }
        let now = unix_ts();
        if task.next_attempt_unix > now && task.source_fingerprint == fingerprint {
            report.skipped_items += 1;
            report.skipped_backoff += 1;
            log_outcome(
                state,
                report,
                &task,
                if task.waiting_for_source {
                    "repair_waiting"
                } else {
                    "repair_unresolved"
                },
                "repair remains queued during backoff".to_string(),
                json!({"next_attempt_unix": task.next_attempt_unix}),
            );
            continue;
        }
        task.source_fingerprint = fingerprint.clone();
        report.attempted_transfers += 1;
        await_repair_busy_threshold(state).await;
        let recovered_before_attempt = task.recovered_chunks;
        match recover_task(state, &mut task).await {
            Ok(recovered) => {
                report.successful_transfers += 1;
                log_outcome(
                    state,
                    report,
                    &task,
                    "repair_verified",
                    "retained content repaired and verified".to_string(),
                    json!({"chunks_recovered": recovered, "total_chunks_recovered": task.recovered_chunks, "verified_at_unix": unix_ts(), "manifest_hash": task.reference.manifest_hash}),
                );
            }
            Err(error) => {
                task.waiting_for_source = error.is::<NoContentSource>();
                let event = if task.waiting_for_source {
                    "repair_waiting"
                } else {
                    "repair_unresolved"
                };
                let detail = format!("{error:#}");
                task.defer(
                    detail.clone(),
                    unix_ts(),
                    state.repair_config.backoff_secs,
                    task.recovered_chunks > recovered_before_attempt,
                );
                read_store(state, "content_recovery.defer")
                    .await
                    .persist_content_repair_task(&task)
                    .await?;
                report.failed_transfers += 1;
                report.last_error = Some(detail.clone());
                log_outcome(
                    state,
                    report,
                    &task,
                    event,
                    detail,
                    json!({"pending": true, "chunks_recovered": task.recovered_chunks.saturating_sub(recovered_before_attempt), "total_chunks_recovered": task.recovered_chunks, "next_attempt_unix": task.next_attempt_unix, "manifest_hash": task.reference.manifest_hash}),
                );
            }
        }
    }
    if availability_may_have_changed {
        invalidate_local_availability_cache(state);
    }
    refresh_local_availability_view_once(state).await;
    Ok(())
}

pub(crate) fn spawn_worker(state: ServerState) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        loop {
            tokio::select! {
                _ = interval.tick() => {},
                _ = state.maintenance.content_repair_notify.notified() => {},
            }
            if !state.repair_config.enabled {
                continue;
            }
            if let Err(error) = resume_pending(&state).await {
                warn!(error = %error, "failed resuming durable content repairs");
            }
        }
    });
}

pub(crate) async fn resume_pending(state: &ServerState) -> Result<()> {
    let fingerprint = source_fingerprint(state).await;
    let hashes = read_store(state, "content_recovery.pending_schedule")
        .await
        .due_content_repair_task_hashes(
            unix_ts(),
            &fingerprint,
            state.repair_config.batch_size.max(1),
        )
        .await?;
    let subjects = hashes
        .into_iter()
        .map(|hash| format!("{MANIFEST_SUBJECT_PREFIX}{hash}"))
        .collect::<Vec<_>>();
    if !subjects.is_empty() {
        execute_tracked_targeted_local_replication_repair(
            state,
            subjects,
            RepairRunTrigger::BackgroundAudit,
        )
        .await;
    }
    Ok(())
}

/// Placement obligations exist even when no node currently advertises a replica.
/// Queue them independently of the legacy replica map; the worker discovers bytes.
#[cfg(test)]
pub(crate) async fn audit_assigned(state: &ServerState) -> Result<()> {
    let retained = {
        let store = read_store(state, "content_recovery.audit").await;
        store.retained_content().await?
    };
    audit_assigned_from_retained(state, &retained).await
}

/// Audits a caller-owned retained-content snapshot so one background pass does
/// not repeatedly decode the full version and snapshot history.
pub(crate) async fn audit_assigned_from_retained(
    state: &ServerState,
    retained: &RetainedContent,
) -> Result<()> {
    let pending = read_store(state, "content_recovery.audit_pending")
        .await
        .content_repair_task_hashes()
        .await?;
    let required = required_manifests(state, retained).await;
    let available = cached_local_cluster_available_subjects(state)
        .await
        .into_iter()
        .collect::<HashSet<_>>();
    let pending = pending.into_iter().collect::<HashSet<_>>();
    let mut enqueued = false;
    for (hash, references) in &retained.manifests {
        if hash == storage::TOMBSTONE_MANIFEST_HASH
            || !required.contains(hash)
            || pending.contains(hash)
            || references.keys().any(|subject| available.contains(subject))
        {
            continue;
        }
        let Some(_claim) = state.maintenance.content_repair_claims.try_claim(hash) else {
            // A repair already owns this manifest. Leave it to finish rather
            // than letting one slow source hold up the rest of this audit.
            continue;
        };
        if !read_store(state, "content_recovery.audit_claimed_task")
            .await
            .content_repair_tasks_for_manifests(std::slice::from_ref(hash))
            .await?
            .is_empty()
        {
            continue;
        }
        // Availability can be empty during first-start convergence (or stale
        // after an out-of-band change). Do not turn a healthy owned replica
        // into pending repair work solely because that distributed view has
        // not caught up yet; a cache-only copy still needs ownership promotion.
        if read_store(state, "content_recovery.audit_local_replica")
            .await
            .check_owned_replica_presence(hash)
            .await
            .is_ok()
        {
            continue;
        }
        if let Some(reference) = references
            .values()
            .find(|reference| reference.version_id.is_some() || reference.snapshot_only)
            .or_else(|| references.values().next())
            .cloned()
        {
            let task = ContentRepairTask::new(reference, true);
            read_store(state, "content_recovery.audit_enqueue")
                .await
                .persist_content_repair_task(&task)
                .await?;
            enqueued = true;
        }
    }
    if enqueued {
        invalidate_local_availability_cache(state);
    }
    state.maintenance.content_repair_notify.notify_one();
    Ok(())
}

pub(crate) async fn get_manifest(
    State(state): State<ServerState>,
    Path(hash): Path<String>,
) -> impl IntoResponse {
    let store = read_store(&state, "content_recovery.manifest_export").await;
    match store.read_recovery_manifest(&hash).await {
        Ok(Some(bytes)) => (StatusCode::OK, bytes).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => StatusCode::BAD_REQUEST.into_response(),
    }
}
