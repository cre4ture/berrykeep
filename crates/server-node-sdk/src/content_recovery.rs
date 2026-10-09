//! Recovery by immutable content identity. Availability is a source hint, never
//! proof that an unadvertised peer cannot contribute a missing chunk.
use super::*;
use futures_util::{StreamExt, stream};
use storage::content_recovery::{ContentRepairTask, validate_manifest};
use storage::retained_content::{MANIFEST_SUBJECT_PREFIX, RetainedContent, RetainedReference};

const CHUNK_FETCH_CONCURRENCY: usize = 4;
const PEER_FETCH_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const READ_THROUGH_RECOVERY_BUDGET: Duration = Duration::from_secs(30);
/// Bounds network transfer work in one durable repair pass. Its persisted task
/// and any verified chunks survive cancellation, so a later pass resumes with
/// a fresh peer snapshot. Full local verification is deliberately outside this
/// budget: a large, locally complete object must always be able to finish.
const DURABLE_CONTENT_REPAIR_BUDGET: Duration = Duration::from_secs(2 * 60);
/// A worker pass processes one durable manifest at a time. Each manifest has
/// its own transfer deadline, so this keeps notifications from being delayed
/// behind a history-sized batch while still allowing local verification to
/// finish.
const BACKGROUND_REPAIR_PASS_MAX_TRANSFERS: usize = 1;
/// Bounds expensive owned-replica checks during one periodic retained-history
/// audit. The cursor below rotates this budget across the whole history.
pub(crate) const RETAINED_AUDIT_PRESENCE_CHECK_BATCH_SIZE: usize = 64;
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

#[cfg(test)]
pub(crate) struct RecoveryPreparationBlocker {
    started: Arc<Notify>,
    release: Arc<Notify>,
    released: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
impl RecoveryPreparationBlocker {
    pub(crate) async fn wait_until_started(&self) {
        self.started.notified().await;
    }

    pub(crate) fn release(&self) {
        self.released.store(true, Ordering::Release);
        self.release.notify_waiters();
    }
}

#[cfg(test)]
fn recovery_preparation_blockers()
-> &'static StdMutex<HashMap<String, Arc<RecoveryPreparationBlocker>>> {
    static BLOCKERS: std::sync::OnceLock<
        StdMutex<HashMap<String, Arc<RecoveryPreparationBlocker>>>,
    > = std::sync::OnceLock::new();
    BLOCKERS.get_or_init(|| StdMutex::new(HashMap::new()))
}

#[cfg(test)]
pub(crate) fn block_recovery_preparation_for_test(
    manifest_hash: &str,
) -> Arc<RecoveryPreparationBlocker> {
    let blocker = Arc::new(RecoveryPreparationBlocker {
        started: Arc::new(Notify::new()),
        release: Arc::new(Notify::new()),
        released: std::sync::atomic::AtomicBool::new(false),
    });
    recovery_preparation_blockers()
        .lock()
        .expect("recovery preparation blocker lock should not be poisoned")
        .insert(manifest_hash.to_string(), blocker.clone());
    blocker
}

#[cfg(test)]
pub(crate) fn clear_recovery_preparation_blocker_for_test(manifest_hash: &str) {
    recovery_preparation_blockers()
        .lock()
        .expect("recovery preparation blocker lock should not be poisoned")
        .remove(manifest_hash);
}

#[cfg(test)]
async fn wait_for_recovery_preparation_test_blocker(manifest_hash: &str) {
    let blocker = recovery_preparation_blockers()
        .lock()
        .expect("recovery preparation blocker lock should not be poisoned")
        .get(manifest_hash)
        .cloned();
    if let Some(blocker) = blocker {
        blocker.started.notify_one();
        loop {
            let notified = blocker.release.notified();
            if blocker.released.load(Ordering::Acquire) {
                break;
            }
            notified.await;
        }
    }
}

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
    let (scrubber, retained_loader) = {
        let store = read_store(state, "content_recovery.scrub_snapshot").await;
        (
            store.data_scrubber().await?,
            store.retained_content_loader(),
        )
    };
    let retained = retained_loader.load().await?;
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

#[derive(Clone, Copy)]
enum ExistingChunkCheck {
    /// Read and hash any local chunk before deciding to reuse it. Foreground
    /// reads use this to avoid returning a corrupt cache entry.
    VerifyContent,
    /// A durable repair defers the full integrity scan to its completion step,
    /// so one bounded pass does not hash a large object twice.
    MatchMetadata,
    /// Completion identified a corrupt chunk; replace it from a verified peer
    /// even when the local file still has the expected size.
    Replace,
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
    retained
        .manifests
        .iter()
        .filter(|(_, references)| {
            references.values().any(|reference| {
                // Snapshot-only references use their manifest hash as an
                // opaque recovery subject, but still retain their logical
                // path. Placement must remain stable when a version index is
                // compacted into such a snapshot-only reference.
                let placement_key = reference.key.as_deref().unwrap_or(&reference.manifest_hash);
                *assigned_by_placement_key
                    .entry(placement_key.to_string())
                    .or_insert_with(|| {
                        placement
                            .placement_for_key(placement_key)
                            .selected_nodes
                            .contains(&state.node_id)
                    })
            })
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
    existing_chunk_check: ExistingChunkCheck,
) -> Result<bool> {
    match existing_chunk_check {
        ExistingChunkCheck::VerifyContent => {
            let store = read_store(state, "content_recovery.check_chunk").await;
            if let Ok(Some(bytes)) = store.read_chunk_payload(&chunk.hash).await
                && bytes.len() == chunk.size_bytes
            {
                return Ok(false);
            }
        }
        ExistingChunkCheck::MatchMetadata => {
            let store = read_store(state, "content_recovery.check_chunk").await;
            if matches!(
                store
                    .chunk_path_matches_size(&chunk.hash, chunk.size_bytes)
                    .await,
                Ok(true)
            ) {
                return Ok(false);
            }
        }
        ExistingChunkCheck::Replace => {}
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
    recover_chunks_with_progress(
        state,
        subject,
        chunks,
        preferred,
        cache,
        ExistingChunkCheck::VerifyContent,
        None,
    )
    .await
}

async fn recover_chunks_with_progress(
    state: &ServerState,
    subject: &str,
    chunks: &[ReplicationChunkInfo],
    preferred: Option<&NodeDescriptor>,
    cache: bool,
    existing_chunk_check: ExistingChunkCheck,
    recovered_progress: Option<Arc<AtomicUsize>>,
) -> ChunkRecoveryResult {
    let sources = Arc::new(source_nodes(state, subject, preferred).await);
    let unique: BTreeMap<_, _> = chunks.iter().map(|c| (c.hash.clone(), c.clone())).collect();
    let mut work = stream::iter(unique.into_values().map(|chunk| {
        let state = state.clone();
        let sources = sources.clone();
        let recovered_progress = recovered_progress.clone();
        async move {
            let outcome =
                recover_chunk(&state, &chunk, &sources, cache, existing_chunk_check).await;
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

pub(crate) async fn recover_manifest(
    state: &ServerState,
    reference: &RetainedReference,
) -> Result<Vec<u8>> {
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
    }
    // Mixed-version clusters may not have the manifest-by-hash endpoint yet.
    // Try the compatibility route only after every peer had a chance to serve
    // the immutable hash directly. A 404 from an early peer must not spend a
    // second timeout before a later new-style source is considered.
    if let Some(key) = &reference.key {
        for source in &sources {
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

pub(crate) fn repair_waits_for_retry(error: &anyhow::Error) -> bool {
    error.is::<NoContentSource>() || error.is::<DurableRepairBudgetExceeded>()
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
    let outcome =
        recover_task_with_transfer_budget(state, task, budget, recovered_progress.clone()).await;
    task.recovered_chunks = task
        .recovered_chunks
        .saturating_add(recovered_progress.load(Ordering::Relaxed));
    outcome
}

async fn bounded_durable_transfer<T>(
    remaining_budget: &mut Duration,
    transfer: impl Future<Output = Result<T>>,
) -> Result<T> {
    let started = Instant::now();
    let outcome = bounded_durable_recovery(*remaining_budget, transfer).await;
    *remaining_budget = remaining_budget.saturating_sub(started.elapsed());
    outcome
}

async fn recover_task_with_transfer_budget(
    state: &ServerState,
    task: &mut ContentRepairTask,
    mut remaining_budget: Duration,
    recovered_progress: Arc<AtomicUsize>,
) -> Result<usize> {
    let (subject, recovered) = bounded_durable_transfer(
        &mut remaining_budget,
        recover_task_initial_transfer(state, task, recovered_progress.clone()),
    )
    .await?;

    // Verification is local work with no resumable cursor. Keeping it outside
    // the transfer budget guarantees that a large object whose bytes are all
    // present can eventually complete instead of timing out at the same point
    // on every pass.
    let inspector = {
        let store = read_store(state, "content_recovery.verify_selection").await;
        store.content_recovery_inspector()
    };
    let invalid_chunks = inspector.invalid_recovered_chunks(task).await?;
    if invalid_chunks.is_empty() {
        let store = read_store(state, "content_recovery.finish_verified").await;
        store.finish_verified_content_repair(task).await?;
        return Ok(recovered);
    }
    let replacement = bounded_durable_transfer(&mut remaining_budget, async {
        Ok(recover_chunks_with_progress(
            state,
            &subject,
            &invalid_chunks,
            None,
            !task.repair_chunks,
            ExistingChunkCheck::Replace,
            Some(recovered_progress),
        )
        .await)
    })
    .await?;
    if !replacement.remaining.is_empty() {
        let detail = format!(
            "{} corrupt chunks replaced; {} still missing; {}",
            replacement.recovered,
            replacement.remaining.len(),
            replacement.errors.join("; ")
        );
        if replacement.has_local_error {
            bail!(detail);
        }
        return Err(NoContentSource(detail).into());
    }
    // `invalid_recovered_chunks` validated every retained byte, and replacement
    // responses were hash-checked before installation. Finalize under the GC
    // gate without re-reading the entire object a second time.
    let store = read_store(state, "content_recovery.finish_verified").await;
    store.finish_verified_content_repair(task).await?;
    Ok(recovered.saturating_add(replacement.recovered))
}

async fn recover_task_initial_transfer(
    state: &ServerState,
    task: &mut ContentRepairTask,
    recovered_progress: Arc<AtomicUsize>,
) -> Result<(String, usize)> {
    let bytes = recover_manifest(state, &task.reference).await?;
    let manifest = validate_manifest(&task.reference.manifest_hash, &bytes)?;
    // Inspect the storage-pool snapshot without retaining the global store
    // guard across one filesystem stat per manifest chunk.
    let inspector = {
        let store = read_store(state, "content_recovery.prepare").await;
        store.content_recovery_inspector()
    };
    let mut prepared_chunks = Vec::with_capacity(manifest.chunks.len());
    #[cfg(test)]
    wait_for_recovery_preparation_test_blocker(&task.reference.manifest_hash).await;
    for chunk in manifest.chunks {
        // Metadata-only nodes repair damaged cached bytes without hydrating
        // absent cache entries or acquiring replica ownership. Durable
        // completion performs the single full-byte validation pass.
        let cache_entry_exists = inspector
            .chunk_path_exists(&chunk.hash)
            .await
            .with_context(|| {
                format!(
                    "failed to inspect cached chunk {} before durable recovery",
                    chunk.hash
                )
            })?;
        if task.repair_chunks || cache_entry_exists {
            prepared_chunks.push(chunk);
        }
    }
    // A transfer deadline may cancel this future at any await above. Only
    // replace the durable GC pin after the complete manifest has been
    // inspected, so the outer failure path cannot persist a partial list.
    task.chunks = prepared_chunks;
    {
        let store = read_store(state, "content_recovery.persist_prepared").await;
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
        ExistingChunkCheck::MatchMetadata,
        Some(recovered_progress.clone()),
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
    Ok((subject, result.recovered))
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
        had_chunk_progress: false,
        waiting_for_source: false,
        unresolved: false,
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
    report.record_status_event(event, &context);
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
    let retained_loader = {
        let store = read_store(state, "content_recovery.catalog").await;
        store.retained_content_loader()
    };
    let retained = retained_loader.load().await?;
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
    let mut tasks = BTreeMap::new();
    for reference in references.into_values() {
        let owned = read_store(state, "content_recovery.ownership")
            .await
            .manifest_is_owned(&reference.manifest_hash)
            .await?;
        // An assigned repair remains a durability obligation during placement
        // changes. Normal replica handoff/cleanup releases it after verification.
        let task = ContentRepairTask::new(
            reference.clone(),
            owned || required.contains(&reference.manifest_hash),
        );
        tasks.insert(reference.manifest_hash.clone(), task);
    }
    let availability_may_have_changed = !tasks.is_empty();
    let transfer_limit = limit.unwrap_or(usize::MAX);
    let mut started_transfers = 0;
    for mut task in tasks.into_values() {
        let Some(_claim) = state
            .maintenance
            .content_repair_claims
            .try_claim(&task.reference.manifest_hash)
        else {
            report.skipped_items += 1;
            log_outcome(
                state,
                report,
                &task,
                "repair_deferred",
                "repair remains queued while another operation owns its manifest".to_string(),
                json!({"pending": true, "reason": "manifest_repair_in_progress"}),
            );
            continue;
        };
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
        task = existing.unwrap_or_else(|| {
            ContentRepairTask::new(requested_reference.clone(), requested_repair_chunks)
        });
        task.reference = requested_reference;
        task.repair_chunks |= requested_repair_chunks;
        // The execution budget bounds this pass, not the lifetime of its work.
        read_store(state, "content_recovery.enqueue")
            .await
            .prepare_and_persist_content_repair_task(&mut task)
            .await?;
        if started_transfers >= transfer_limit {
            report.skipped_items += 1;
            log_outcome(
                state,
                report,
                &task,
                "repair_deferred",
                "repair queued for a later execution batch".to_string(),
                json!({"pending": true}),
            );
            continue;
        }

        // Register the durable task before yielding to foreground traffic. A
        // scrub's enqueue-only pass uses a zero transfer limit and must still
        // publish its findings while a node is busy.
        drop(_claim);

        // Foreground load throttles byte transfer only. Do not retain the
        // manifest claim while waiting: a foreground pull may complete this
        // task in the meantime.
        await_repair_busy_threshold(state).await;
        let Some(_claim) = state
            .maintenance
            .content_repair_claims
            .try_claim(&task.reference.manifest_hash)
        else {
            report.skipped_items += 1;
            log_outcome(
                state,
                report,
                &task,
                "repair_deferred",
                "repair remains queued while another operation owns its manifest".to_string(),
                json!({"pending": true, "reason": "manifest_repair_in_progress"}),
            );
            continue;
        };
        let Some(mut task) = read_store(state, "content_recovery.reclaimed_task")
            .await
            .content_repair_tasks_for_manifests(std::slice::from_ref(&task.reference.manifest_hash))
            .await?
            .into_iter()
            .next()
        else {
            // A foreground or concurrent repair completed this manifest while
            // the background worker yielded its claim.
            continue;
        };
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
        task.last_attempt_unix = now;
        task.source_fingerprint = fingerprint.clone();
        started_transfers += 1;
        report.attempted_transfers += 1;
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
                task.waiting_for_source = repair_waits_for_retry(&error);
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
    // A single transfer can consume the full durable-recovery budget, so do
    // not let one worker tick monopolize repair tracking with a large batch.
    let hashes = read_store(state, "content_recovery.pending_schedule")
        .await
        .due_content_repair_task_hashes(
            unix_ts(),
            &fingerprint,
            BACKGROUND_REPAIR_PASS_MAX_TRANSFERS,
        )
        .await?;
    let subjects = hashes
        .into_iter()
        .filter(|hash| !state.maintenance.content_repair_claims.is_claimed(hash))
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
    let retained_loader = {
        let store = read_store(state, "content_recovery.audit").await;
        store.retained_content_loader()
    };
    let retained = retained_loader.load().await?;
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
    let cursor = state.maintenance.retained_audit_cursor.lock().await.clone();
    let mut candidates = Vec::<(String, RetainedReference)>::new();
    let pass_count = usize::from(cursor.is_some()) + 1;
    'candidate_scan: for pass in 0..pass_count {
        for (hash, references) in &retained.manifests {
            let in_this_pass = match (&cursor, pass) {
                (Some(cursor), 0) => hash > cursor,
                (Some(cursor), _) => hash <= cursor,
                (None, _) => true,
            };
            if !in_this_pass
                || hash == storage::TOMBSTONE_MANIFEST_HASH
                || !required.contains(hash)
                || pending.contains(hash)
                || references.keys().any(|subject| available.contains(subject))
            {
                continue;
            }
            let Some(reference) = references
                .values()
                .find(|reference| reference.version_id.is_some() || reference.snapshot_only)
                .or_else(|| references.values().next())
                .cloned()
            else {
                continue;
            };
            candidates.push((hash.clone(), reference));
            if candidates.len() == RETAINED_AUDIT_PRESENCE_CHECK_BATCH_SIZE {
                break 'candidate_scan;
            }
        }
    }
    if let Some((last_hash, _)) = candidates.last() {
        *state.maintenance.retained_audit_cursor.lock().await = Some(last_hash.clone());
    }

    // The durable-task hashes were read above and manifest claims serialize
    // local enqueue paths, so a single batched ownership lookup is sufficient
    // here. Avoid per-manifest task and ownership queries on deep history.
    let candidate_hashes = candidates
        .iter()
        .map(|(hash, _)| hash.clone())
        .collect::<Vec<_>>();
    let locally_owned = read_store(state, "content_recovery.audit_candidates")
        .await
        .filter_locally_owned_manifests(&candidate_hashes)
        .await?;
    let mut enqueued = false;
    for (hash, reference) in candidates {
        let Some(_claim) = state.maintenance.content_repair_claims.try_claim(&hash) else {
            // A repair already owns this manifest. Leave it to finish rather
            // than letting one slow source hold up the rest of this audit.
            continue;
        };
        // Availability can be empty during first-start convergence (or stale
        // after an out-of-band change). Do not turn a healthy owned replica
        // into pending repair work solely because that distributed view has
        // not caught up yet; a cache-only copy still needs ownership promotion.
        if locally_owned.contains(&hash) {
            match read_store(state, "content_recovery.audit_local_presence")
                .await
                .manifest_is_fully_local(&hash)
                .await
            {
                Ok(true) => continue,
                Ok(false) => {}
                Err(error) => {
                    warn!(
                        manifest_hash = %hash,
                        error = %error,
                        "skipping retained-content repair enqueue after local presence check failed"
                    );
                    continue;
                }
            }
        }
        let mut task = ContentRepairTask::new(reference, true);
        read_store(state, "content_recovery.audit_enqueue")
            .await
            .prepare_and_persist_content_repair_task(&mut task)
            .await?;
        enqueued = true;
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
