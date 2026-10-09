use super::*;
use crate::storage::ReplicationExportBundle;
use crate::storage::retained_content::MANIFEST_SUBJECT_PREFIX;

#[test]
fn content_repair_claims_are_manifest_scoped() {
    let claims = Arc::new(crate::ContentRepairClaims::default());
    let held = claims
        .try_claim("manifest-a")
        .expect("first claim for a manifest must succeed");
    assert!(
        claims.try_claim("manifest-a").is_none(),
        "audits must skip manifests an active repair already owns"
    );

    let different = claims.try_claim("manifest-b");
    assert!(
        different.is_some(),
        "a stalled manifest must not block unrelated replication"
    );
    drop(different);
    drop(held);
    assert!(
        claims.try_claim("manifest-a").is_some(),
        "releasing a claim must permit a later repair for the same manifest"
    );
}

#[tokio::test]
async fn repair_logs_expired_retained_reference_skips() {
    let state = build_test_state(1, false, MainTestBackend::Sqlite).await;
    let subject = format!("{MANIFEST_SUBJECT_PREFIX}expired-manifest");

    let report =
        crate::content_recovery::repair_subjects(&state, vec![subject.clone()], None).await;

    assert_eq!(report.skipped_items, 1, "{report:?}");
    assert!(
        report.detailed_log.iter().any(|entry| {
            entry.event == "subject_skipped"
                && entry.subject.as_deref() == Some(subject.as_str())
                && entry
                    .context
                    .as_ref()
                    .and_then(|context| context["reason"].as_str())
                    == Some("retained_reference_unavailable")
        }),
        "an unresolvable retained reference must remain visible in the repair audit log: {report:?}"
    );
    assert!(
        report.skipped_details.iter().any(|detail| {
            detail.subject == subject
                && detail.reason
                    == crate::replication::ReplicationRepairSkipReason::RetainedReferenceUnavailable
        }),
        "the structured repair result must retain the skip reason: {report:?}"
    );

    cleanup_test_state(&state).await;
}

async fn fully_local_recovery_does_not_require_the_store_write_lock_impl(backend: MainTestBackend) {
    let state = build_test_state(1, false, backend).await;
    let key = choose_locally_placed_key(&state, "shared-recovery-store").await;
    seed_subject_version(
        &state,
        &key,
        "v1",
        b"fully local repair bytes".to_vec(),
        vec![],
    )
    .await;
    let manifest = bundle(&state, &key, "v1").await;

    let reader = read_store(&state, "test.recovery.shared_store_reader").await;
    let report = tokio::time::timeout(
        Duration::from_secs(1),
        crate::content_recovery::repair_subjects(
            &state,
            vec![format!(
                "{MANIFEST_SUBJECT_PREFIX}{}",
                manifest.manifest_hash
            )],
            None,
        ),
    )
    .await
    .expect("fully local recovery must not wait for the store write lock");
    drop(reader);

    assert_eq!(report.successful_transfers, 1, "{report:?}");
    assert_eq!(report.failed_transfers, 0, "{report:?}");
    cleanup_test_state(&state).await;
}

run_on_main_metadata_backends!(
    fully_local_recovery_does_not_require_the_store_write_lock_impl,
    fully_local_recovery_does_not_require_the_store_write_lock,
    fully_local_recovery_does_not_require_the_store_write_lock_turso
);

async fn retained_catalog_loading_does_not_hold_the_store_lock_impl(backend: MainTestBackend) {
    let state = build_test_state(1, false, backend).await;
    let key = format!("retained-catalog-lock-{}", backend.suffix());
    seed_subject_version(
        &state,
        &key,
        "v1",
        b"retained catalog bytes".to_vec(),
        vec![],
    )
    .await;

    let loader = {
        let store = read_store(&state, "test.recovery.retained_catalog_loader").await;
        store.retained_content_loader()
    };
    let writer = lock_store(&state, "test.recovery.retained_catalog_writer").await;
    let retained = tokio::time::timeout(Duration::from_secs(1), loader.load())
        .await
        .expect("retained catalog loading must not wait for the global store lock")
        .unwrap();
    drop(writer);

    assert!(
        retained
            .reference_for_subject(&format!("{key}@v1"))
            .is_some(),
        "loading outside the global store lock must retain version history"
    );
    cleanup_test_state(&state).await;
}

run_on_main_metadata_backends!(
    retained_catalog_loading_does_not_hold_the_store_lock_impl,
    retained_catalog_loading_does_not_hold_the_store_lock,
    retained_catalog_loading_does_not_hold_the_store_lock_turso
);

async fn retained_catalog_snapshot_is_reused_until_invalidated_impl(backend: MainTestBackend) {
    let state = build_test_state(1, false, backend).await;
    let key = format!("retained-catalog-cache-{}", backend.suffix());
    seed_subject_version(
        &state,
        &key,
        "v1",
        b"retained catalog cache bytes".to_vec(),
        vec![],
    )
    .await;

    let first = crate::content_recovery::retained_content_snapshot(&state)
        .await
        .unwrap();
    let writer = lock_store(&state, "test.recovery.retained_catalog_cached_writer").await;
    let second = tokio::time::timeout(
        Duration::from_secs(1),
        crate::content_recovery::retained_content_snapshot(&state),
    )
    .await
    .expect("a cached catalog must not wait for the global store lock")
    .unwrap();
    drop(writer);

    assert!(
        Arc::ptr_eq(&first, &second),
        "repair passes in one cache generation must share the decoded catalog"
    );

    crate::publish_namespace_change(&state);
    let refreshed = crate::content_recovery::retained_content_snapshot(&state)
        .await
        .unwrap();
    assert!(
        !Arc::ptr_eq(&first, &refreshed),
        "namespace changes must invalidate the decoded retained catalog"
    );

    cleanup_test_state(&state).await;
}

run_on_main_metadata_backends!(
    retained_catalog_snapshot_is_reused_until_invalidated_impl,
    retained_catalog_snapshot_is_reused_until_invalidated,
    retained_catalog_snapshot_is_reused_until_invalidated_turso
);

async fn durable_recovery_replaces_same_size_corrupt_chunks_impl(backend: MainTestBackend) {
    let source = build_test_state(1, false, backend).await;
    let target = build_test_state(1, false, backend).await;
    let key = choose_locally_placed_key(
        &target,
        &format!("same-size-corrupt-recovery-{}", backend.suffix()),
    )
    .await;
    let payload = b"verified retained repair bytes".to_vec();
    for state in [&source, &target] {
        seed_subject_version(state, &key, "v1", payload.clone(), vec![]).await;
    }
    let manifest = bundle(&target, &key, "v1").await;
    fs::write(
        read_store(&target, "test.recovery.same_size_corruption")
            .await
            .chunk_path_for_test(&manifest.manifest.chunks[0].hash),
        vec![0; payload.len()],
    )
    .await
    .unwrap();
    let verification = crate::storage::content_recovery::block_recovery_verification_for_test(
        &manifest.manifest_hash,
    );
    verification.release();
    let (url, handle) = spawn_internal_peer_api_server(source.clone()).await;
    register_online_source_node(&target, &source, &url).await;

    let repair_run_id = "targeted-recovery-observability";
    let report = crate::replication::execute_targeted_replication_repair_inner_with_context(
        &target,
        vec![format!("{key}@v1")],
        None,
        Some(repair_run_id),
    )
    .await;

    assert_eq!(report.successful_transfers, 1, "{report:?}");
    assert_eq!(
        verification.calls(),
        1,
        "a corrupt-chunk repair must not re-read the full object during finalization"
    );
    for event in ["targeted_repair_started", "targeted_repair_finished"] {
        assert!(
            report.detailed_log.iter().any(|entry| {
                entry.event == event
                    && entry
                        .context
                        .as_ref()
                        .and_then(|context| context.get("repair_run_id"))
                        .and_then(serde_json::Value::as_str)
                        == Some(repair_run_id)
            }),
            "missing run-scoped lifecycle event {event}: {report:?}"
        );
    }
    assert_eq!(
        read_store(&target, "test.recovery.same_size_result")
            .await
            .get_object(&key, None, Some("v1"), ObjectReadMode::Preferred)
            .await
            .unwrap()
            .as_ref(),
        payload
    );
    crate::storage::content_recovery::clear_recovery_verification_blocker_for_test(
        &manifest.manifest_hash,
    );
    handle.abort();
    let _ = handle.await;
    cleanup_test_state(&source).await;
    cleanup_test_state(&target).await;
}

run_on_main_metadata_backends!(
    durable_recovery_replaces_same_size_corrupt_chunks_impl,
    durable_recovery_replaces_same_size_corrupt_chunks,
    durable_recovery_replaces_same_size_corrupt_chunks_turso
);

async fn local_availability_refresh_keeps_its_fresh_cache_impl(backend: MainTestBackend) {
    let state = build_test_state(1, false, backend).await;
    let key = "availability-cache-reconciliation.bin";
    seed_subject_version(
        &state,
        key,
        "v1",
        b"availability cache bytes".to_vec(),
        vec![],
    )
    .await;
    assert!(
        crate::cached_local_cluster_available_subjects(&state)
            .await
            .is_empty(),
        "the refresh must have a local availability change to persist"
    );

    let generation_before = state
        .maintenance
        .local_availability_generation
        .load(std::sync::atomic::Ordering::SeqCst);
    crate::refresh_local_availability_view_once(&state).await;
    let generation_after = state
        .maintenance
        .local_availability_generation
        .load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        generation_after, generation_before,
        "persisting the locally computed availability set must not invalidate it"
    );
    assert!(
        state
            .maintenance
            .local_availability_cache
            .lock()
            .await
            .as_ref()
            .is_some_and(|cache| cache.is_valid_for(generation_after)),
        "a changed local view should remain cacheable for the refresh TTL"
    );

    let scheduled_refresh = state
        .maintenance
        .local_availability_refresh_notify
        .notified();
    crate::request_ttl_bounded_local_availability_refresh(&state);
    tokio::time::timeout(Duration::from_secs(1), scheduled_refresh)
        .await
        .expect("the auditor refresh request was not queued");
    assert_eq!(
        state
            .maintenance
            .local_availability_generation
            .load(std::sync::atomic::Ordering::SeqCst),
        generation_after,
        "a periodic refresh request must retain a valid cache until its TTL expires"
    );
    state
        .maintenance
        .local_availability_cache
        .lock()
        .await
        .as_mut()
        .unwrap()
        .computed_at = std::time::Instant::now() - crate::LOCAL_AVAILABILITY_CACHE_TTL;
    crate::refresh_local_availability_view_once(&state).await;
    assert!(
        state
            .maintenance
            .local_availability_cache
            .lock()
            .await
            .as_ref()
            .is_some_and(|cache| cache.is_valid_for(generation_after)),
        "the queued refresh must recompute and replace an expired local view"
    );

    cleanup_test_state(&state).await;
}

run_on_main_metadata_backends!(
    local_availability_refresh_keeps_its_fresh_cache_impl,
    local_availability_refresh_keeps_its_fresh_cache,
    local_availability_refresh_keeps_its_fresh_cache_turso
);

async fn remote_availability_sync_keeps_local_availability_cache_impl(backend: MainTestBackend) {
    let source = build_test_state(1, false, backend).await;
    let target = build_test_state(1, false, backend).await;
    let target_key = choose_locally_placed_key(&target, "remote-sync-local-cache").await;
    seed_subject_version(
        &target,
        &target_key,
        "v1",
        b"local availability cache bytes".to_vec(),
        vec![],
    )
    .await;
    crate::refresh_local_availability_view_once(&target).await;
    let generation = target
        .maintenance
        .local_availability_generation
        .load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        target
            .maintenance
            .local_availability_cache
            .lock()
            .await
            .as_ref()
            .is_some_and(|cache| cache.is_valid_for(generation)),
        "the local refresh must populate a valid availability cache"
    );

    let source_key = choose_locally_placed_key(&source, "remote-availability-subject").await;
    seed_subject_version(
        &source,
        &source_key,
        "v1",
        b"remote availability bytes".to_vec(),
        vec![],
    )
    .await;
    crate::refresh_local_availability_view_once(&source).await;
    let (url, handle) = spawn_internal_peer_api_server(source.clone()).await;
    register_online_source_node(&target, &source, &url).await;

    crate::sync_remote_availability_views_once(&target).await;

    assert_eq!(
        target
            .maintenance
            .local_availability_generation
            .load(std::sync::atomic::Ordering::SeqCst),
        generation,
        "a remote peer's availability claim must not invalidate the local view"
    );
    assert!(
        target
            .maintenance
            .local_availability_cache
            .lock()
            .await
            .as_ref()
            .is_some_and(|cache| cache.is_valid_for(generation)),
        "a remote peer sync must retain the target's local availability cache"
    );
    assert!(
        target
            .cluster
            .lock()
            .await
            .available_nodes_for_subject(&source_key)
            .iter()
            .any(|node| node.node_id == source.node_id),
        "the remote availability claim must still be reconciled and persisted"
    );

    handle.abort();
    let _ = handle.await;
    cleanup_test_state(&source).await;
    cleanup_test_state(&target).await;
}

run_on_main_metadata_backends!(
    remote_availability_sync_keeps_local_availability_cache_impl,
    remote_availability_sync_keeps_local_availability_cache,
    remote_availability_sync_keeps_local_availability_cache_turso
);

async fn planning_subjects_keep_divergent_head_versions_impl(backend: MainTestBackend) {
    let state = build_test_state(1, false, backend).await;
    let key = choose_locally_placed_key(&state, "planning-divergent-heads").await;
    seed_subject_version(&state, &key, "ver-planning-a", b"first".to_vec(), vec![]).await;
    seed_subject_version(&state, &key, "ver-planning-b", b"second".to_vec(), vec![]).await;

    crate::refresh_local_availability_view_once(&state).await;
    let subjects = crate::planning_replication_subjects(&state).await;
    let matching = subjects
        .iter()
        .filter(|subject| crate::cluster::replication_placement_key(subject) == key)
        .collect::<Vec<_>>();
    assert_eq!(
        matching,
        vec![
            &key,
            &format!("{key}@ver-planning-a"),
            &format!("{key}@ver-planning-b"),
        ],
        "replication planning must not collapse concurrent heads behind the base subject"
    );
    cleanup_test_state(&state).await;
}

run_on_main_metadata_backends!(
    planning_subjects_keep_divergent_head_versions_impl,
    planning_subjects_keep_divergent_head_versions,
    planning_subjects_keep_divergent_head_versions_turso
);

async fn replication_plan_defers_presence_check_failures_impl(backend: MainTestBackend) {
    let source = build_test_state(1, false, backend).await;
    let target = build_test_state(1, false, backend).await;
    let key = "presence-check-failure.bin";
    let version = "ver-presence-check-failure";
    for state in [&source, &target] {
        seed_subject_version(
            state,
            key,
            version,
            b"local presence check bytes".to_vec(),
            vec![],
        )
        .await;
    }
    crate::refresh_local_availability_view_once(&target).await;

    let (url, handle) = spawn_internal_peer_api_server(source.clone()).await;
    register_online_source_node(&target, &source, &url).await;
    let source_node = target
        .cluster
        .lock()
        .await
        .list_nodes()
        .into_iter()
        .find(|node| node.node_id == source.node_id)
        .expect("registered source descriptor must be retained by the target");

    let manifest = bundle(&target, key, version).await;
    let chunk_path = read_store(&target, "test.recovery.presence_check_path")
        .await
        .chunk_path_for_test(&manifest.manifest.chunks[0].hash);
    let shard = chunk_path
        .parent()
        .expect("chunk must have a shard directory")
        .to_path_buf();
    fs::remove_file(&chunk_path).await.unwrap();
    fs::remove_dir(&shard).await.unwrap();
    fs::write(&shard, b"local storage obstruction")
        .await
        .unwrap();

    let plan = crate::cluster::ReplicationPlan {
        generated_at_unix: 0,
        under_replicated: 1,
        over_replicated: 0,
        cleanup_deferred_items: 0,
        cleanup_deferred_extra_nodes: 0,
        items: vec![crate::cluster::ReplicationPlanItem {
            key: format!("{key}@{version}"),
            desired_nodes: vec![source.node_id, target.node_id],
            current_nodes: vec![source.node_id],
            missing_nodes: vec![target.node_id],
            extra_nodes: Vec::new(),
            cleanup_option: crate::cluster::ReplicationCleanupOption::None,
            deferred_extra_nodes: 0,
        }],
    };
    let report = crate::replication::execute_replication_repair_plan(
        &target,
        &plan,
        vec![source_node],
        None,
        None,
    )
    .await;

    assert_eq!(report.attempted_transfers, 0, "{report:?}");
    assert_eq!(report.failed_transfers, 1, "{report:?}");
    assert_eq!(
        report.run_status(),
        crate::RepairRunStatus::Unresolved,
        "a transient local presence failure must be visible and retried: {report:?}"
    );
    assert!(
        report.detailed_log.iter().any(|entry| {
            entry.event == "local_replica_inspection_failed"
                && entry
                    .context
                    .as_ref()
                    .and_then(|context| context["retryable"].as_bool())
                    == Some(true)
        }),
        "the presence failure must remain observable and retryable: {report:?}"
    );
    assert!(
        !report
            .detailed_log
            .iter()
            .any(|entry| entry.event == "local_pull_started"),
        "a local presence check failure must not be treated as missing content: {report:?}"
    );

    handle.abort();
    let _ = handle.await;
    cleanup_test_state(&source).await;
    cleanup_test_state(&target).await;
}

run_on_main_metadata_backends!(
    replication_plan_defers_presence_check_failures_impl,
    replication_plan_defers_presence_check_failures,
    replication_plan_defers_presence_check_failures_turso
);

async fn snapshot_only_retention_keeps_logical_key_placement_impl(backend: MainTestBackend) {
    let state = build_test_state(1, false, backend).await;
    let peer_a = build_test_state(1, false, backend).await;
    let peer_b = build_test_state(1, false, backend).await;
    for peer in [&peer_a, &peer_b] {
        register_online_source_node(&state, peer, "http://127.0.0.1:9").await;
    }

    let mut selected = None;
    for attempt in 0..64 {
        let key = format!("snapshot-placement-{attempt}.bin");
        let version = format!("ver-snapshot-placement-{attempt}");
        seed_subject_version(
            &state,
            &key,
            &version,
            format!("snapshot placement bytes {attempt}").into_bytes(),
            vec![],
        )
        .await;
        let manifest = bundle(&state, &key, &version).await;
        let legacy_subject = format!("{MANIFEST_SUBJECT_PREFIX}{}", manifest.manifest_hash);
        let (path_assigned, legacy_subject_assigned) = {
            let cluster = state.cluster.lock().await;
            (
                cluster
                    .placement_for_key(&key)
                    .selected_nodes
                    .contains(&state.node_id),
                cluster
                    .placement_for_key(&legacy_subject)
                    .selected_nodes
                    .contains(&state.node_id),
            )
        };
        if path_assigned != legacy_subject_assigned {
            selected = Some((
                key,
                version,
                manifest.manifest_hash,
                path_assigned,
                legacy_subject_assigned,
            ));
            break;
        }
    }
    let (key, version, manifest_hash, path_assigned, legacy_subject_assigned) =
        selected.expect("three nodes must yield a key whose path and hash placements differ");

    {
        let mut store = lock_store(&state, "test.recovery.snapshot_placement_compact").await;
        store
            .tombstone_object(&key, PutOptions::default())
            .await
            .unwrap();
        store.compact_tombstone_indexes(0, false).await.unwrap();
        assert!(
            store
                .export_replication_bundle(&key, Some(&version), ObjectReadMode::Preferred)
                .await
                .unwrap()
                .is_none(),
            "the reference must be available only through snapshot retention"
        );
    }

    let retained = read_store(&state, "test.recovery.snapshot_placement_catalog")
        .await
        .retained_content()
        .await
        .unwrap();
    assert!(
        retained.manifests[&manifest_hash]
            .values()
            .all(|reference| reference.snapshot_only),
        "the compacted reference must be represented only by snapshot retention"
    );
    let required = crate::content_recovery::required_manifests(&state, &retained).await;
    assert_eq!(
        required.contains(&manifest_hash),
        path_assigned,
        "snapshot-only retention must keep the logical path's placement"
    );
    assert_ne!(
        path_assigned, legacy_subject_assigned,
        "the regression must distinguish logical-path placement from the legacy hash subject"
    );

    cleanup_test_state(&peer_a).await;
    cleanup_test_state(&peer_b).await;
    cleanup_test_state(&state).await;
}

run_on_main_metadata_backends!(
    snapshot_only_retention_keeps_logical_key_placement_impl,
    snapshot_only_retention_keeps_logical_key_placement,
    snapshot_only_retention_keeps_logical_key_placement_turso
);

async fn recovery_audit_pins_cached_chunks_before_first_worker_pass_impl(backend: MainTestBackend) {
    let source = build_test_state(1, false, backend).await;
    let target = build_test_state(1, false, backend).await;
    let key = "audit-pins-cache.bin";
    let version = "ver-audit-pins-cache";
    let payload = b"cached chunk retained before the first worker pass".to_vec();
    seed_subject_version(&source, key, version, payload.clone(), vec![]).await;
    let manifest = bundle(&source, key, version).await;
    let metadata = read_store(&source, "test.recovery.audit_pin_metadata")
        .await
        .export_metadata_bundle(key, None, ObjectReadMode::Preferred)
        .await
        .unwrap()
        .unwrap();
    lock_store(&target, "test.recovery.audit_pin_import")
        .await
        .import_metadata_bundle(&metadata)
        .await
        .unwrap();
    read_store(&target, "test.recovery.audit_pin_cached_chunk")
        .await
        .ingest_chunk(&manifest.manifest.chunks[0].hash, &payload)
        .await
        .unwrap();

    crate::content_recovery::audit_assigned(&target)
        .await
        .unwrap();
    let task = read_store(&target, "test.recovery.audit_pin_task")
        .await
        .content_repair_tasks()
        .await
        .unwrap()
        .into_iter()
        .next()
        .expect("the assigned but unowned replica must enqueue durable repair");
    assert_eq!(
        task.chunks
            .iter()
            .map(|chunk| (&chunk.hash, chunk.size_bytes))
            .collect::<Vec<_>>(),
        manifest
            .manifest
            .chunks
            .iter()
            .map(|chunk| (&chunk.hash, chunk.size_bytes))
            .collect::<Vec<_>>(),
        "{task:?}"
    );

    read_store(&target, "test.recovery.audit_pin_gc")
        .await
        .cleanup_unreferenced(0, false)
        .await
        .unwrap();
    assert!(
        read_store(&target, "test.recovery.audit_pin_verify")
            .await
            .read_chunk_payload(&manifest.manifest.chunks[0].hash)
            .await
            .unwrap()
            .is_some(),
        "the audit task must pin reusable cache bytes before a worker starts"
    );

    cleanup_test_state(&source).await;
    cleanup_test_state(&target).await;
}

run_on_main_metadata_backends!(
    recovery_audit_pins_cached_chunks_before_first_worker_pass_impl,
    recovery_audit_pins_cached_chunks_before_first_worker_pass,
    recovery_audit_pins_cached_chunks_before_first_worker_pass_turso
);

async fn cache_only_recovery_fails_closed_on_chunk_presence_error_impl(backend: MainTestBackend) {
    let source = build_test_state(1, false, backend).await;
    let target = build_test_state(1, false, backend).await;
    let key = "cache-only-presence-error.bin";
    let version = "ver-cache-only-presence-error";
    seed_subject_version(&source, key, version, b"cache-only bytes".to_vec(), vec![]).await;
    let manifest = bundle(&source, key, version).await;
    let metadata = read_store(&source, "test.recovery.cache_only_metadata")
        .await
        .export_metadata_bundle(key, None, ObjectReadMode::Preferred)
        .await
        .unwrap()
        .unwrap();
    lock_store(&target, "test.recovery.cache_only_import")
        .await
        .import_metadata_bundle(&metadata)
        .await
        .unwrap();
    let reference = read_store(&target, "test.recovery.cache_only_reference")
        .await
        .retained_content()
        .await
        .unwrap()
        .reference_for_subject(&format!("{key}@{version}"))
        .unwrap()
        .clone();
    let chunk_path = read_store(&target, "test.recovery.cache_only_chunk_path")
        .await
        .chunk_path_for_test(&manifest.manifest.chunks[0].hash);
    let shard = chunk_path
        .parent()
        .expect("chunk must have a shard directory")
        .to_path_buf();
    fs::create_dir_all(shard.parent().unwrap()).await.unwrap();
    fs::write(&shard, b"local storage obstruction")
        .await
        .unwrap();

    let mut task = crate::storage::content_recovery::ContentRepairTask::new(reference, false);
    let error = crate::content_recovery::recover_task_with_budget(
        &target,
        &mut task,
        Duration::from_secs(1),
    )
    .await
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("failed to inspect cached chunk"),
        "a presence check error must be surfaced instead of hydrating metadata-only content: {error:#}"
    );
    assert!(
        task.chunks.is_empty(),
        "an unreadable cache path must not be scheduled for a cache-only download"
    );

    let mut owned_task =
        crate::storage::content_recovery::ContentRepairTask::new(task.reference.clone(), true);
    let error = crate::content_recovery::recover_task_with_budget(
        &target,
        &mut owned_task,
        Duration::from_secs(1),
    )
    .await
    .unwrap_err();
    assert!(
        !format!("{error:#}").contains("failed to inspect cached chunk"),
        "an owner repair must leave presence checks to its transfer path: {error:#}"
    );
    assert_eq!(
        owned_task.chunks.len(),
        manifest.manifest.chunks.len(),
        "an owner repair must schedule every manifest chunk without a preliminary stat"
    );

    cleanup_test_state(&source).await;
    cleanup_test_state(&target).await;
}

run_on_main_metadata_backends!(
    cache_only_recovery_fails_closed_on_chunk_presence_error_impl,
    cache_only_recovery_fails_closed_on_chunk_presence_error,
    cache_only_recovery_fails_closed_on_chunk_presence_error_turso
);

async fn repair_deferral_keeps_valid_local_availability_cache_impl(backend: MainTestBackend) {
    let state = build_test_state(1, false, backend).await;
    let key = choose_locally_placed_key(&state, "repair-deferral-cache").await;
    seed_subject_version(
        &state,
        &key,
        "v1",
        b"repair deferral cache bytes".to_vec(),
        vec![],
    )
    .await;
    let reference = read_store(&state, "test.recovery.deferral_cache_reference")
        .await
        .retained_content()
        .await
        .unwrap()
        .reference_for_subject(&format!("{key}@v1"))
        .unwrap()
        .clone();
    let mut task = crate::storage::content_recovery::ContentRepairTask::new(reference, true);
    read_store(&state, "test.recovery.deferral_cache_task")
        .await
        .prepare_and_persist_content_repair_task(&mut task)
        .await
        .unwrap();
    crate::invalidate_local_availability_cache(&state);
    crate::refresh_local_availability_view_once(&state).await;
    let generation = state
        .maintenance
        .local_availability_generation
        .load(Ordering::SeqCst);
    let cached_subjects = state
        .maintenance
        .local_availability_cache
        .lock()
        .await
        .as_ref()
        .map(|cache| Arc::clone(&cache.subjects))
        .expect("the initial refresh must populate the availability cache");

    let report =
        crate::content_recovery::repair_subjects(&state, vec![format!("{key}@v1")], Some(0)).await;

    assert_eq!(report.attempted_transfers, 0, "{report:?}");
    assert_eq!(report.skipped_items, 1, "{report:?}");
    assert_eq!(
        state
            .maintenance
            .local_availability_generation
            .load(Ordering::SeqCst),
        generation,
        "an unchanged durable task must not invalidate local availability"
    );
    let cache = state.maintenance.local_availability_cache.lock().await;
    assert!(
        cache.as_ref().is_some_and(|cache| {
            cache.is_valid_for(generation) && Arc::ptr_eq(&cache.subjects, &cached_subjects)
        }),
        "a no-op deferral must retain the existing availability cache"
    );
    drop(cache);

    cleanup_test_state(&state).await;
}

run_on_main_metadata_backends!(
    repair_deferral_keeps_valid_local_availability_cache_impl,
    repair_deferral_keeps_valid_local_availability_cache,
    repair_deferral_keeps_valid_local_availability_cache_turso
);

async fn recovery_read_budget_bounds_slow_unadvertised_peers_impl(backend: MainTestBackend) {
    let target = build_test_state(1, false, backend).await;
    let source = build_test_state(1, false, backend).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let release = Arc::new(tokio::sync::Notify::new());
    let wait = release.clone();
    let app = axum::Router::new().route(
        "/cluster/v2/replication/chunk/{hash}",
        axum::routing::get(move || {
            let wait = wait.clone();
            async move {
                wait.notified().await;
                StatusCode::NOT_FOUND
            }
        }),
    );
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    register_online_source_node(&target, &source, &url).await;
    let chunk = crate::storage::ReplicationChunkInfo {
        hash: blake3::hash(b"absent").to_hex().to_string(),
        size_bytes: 6,
    };
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        crate::content_recovery::recover_chunks_for_read(
            &target,
            "unadvertised.bin",
            &[chunk],
            Duration::from_millis(50),
        ),
    )
    .await;
    release.notify_waiters();
    handle.abort();
    let _ = handle.await;
    cleanup_test_state(&source).await;
    cleanup_test_state(&target).await;
    assert!(
        result.is_ok(),
        "foreground recovery ignored its total time budget"
    );
    let error = match result.unwrap() {
        Err(error) => error,
        Ok(_) => panic!("expected a deadline error"),
    };
    assert!(
        error.to_string().contains("read-through recovery deadline"),
        "{error:#}"
    );
}

run_on_main_metadata_backends!(
    recovery_read_budget_bounds_slow_unadvertised_peers_impl,
    recovery_read_budget_bounds_slow_unadvertised_peers,
    recovery_read_budget_bounds_slow_unadvertised_peers_turso
);

async fn foreground_recovery_stops_after_first_unavailable_chunk_impl(backend: MainTestBackend) {
    let target = build_test_state(1, false, backend).await;
    let source = build_test_state(1, false, backend).await;
    let requests = Arc::new(AtomicUsize::new(0));
    let observed_requests = requests.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new().route(
        "/cluster/v2/replication/chunk/{hash}",
        axum::routing::get(move || {
            observed_requests.fetch_add(1, Ordering::SeqCst);
            async { StatusCode::NOT_FOUND }
        }),
    );
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    register_online_source_node(&target, &source, &url).await;
    let chunks = (0..32)
        .map(|index| {
            let bytes = format!("unavailable chunk {index}");
            crate::storage::ReplicationChunkInfo {
                hash: blake3::hash(bytes.as_bytes()).to_hex().to_string(),
                size_bytes: bytes.len(),
            }
        })
        .collect::<Vec<_>>();

    let result = crate::content_recovery::recover_chunks_for_read(
        &target,
        "unavailable-large-object.bin",
        &chunks,
        Duration::from_secs(5),
    )
    .await
    .unwrap();

    assert_eq!(result.remaining.len(), 1, "{result:?}");
    assert_eq!(result.errors.len(), 1, "{result:?}");
    assert!(requests.load(Ordering::SeqCst) > 0);
    assert!(
        requests.load(Ordering::SeqCst) <= 4,
        "foreground recovery must cancel the remaining chunk probes after the first failure"
    );
    handle.abort();
    let _ = handle.await;
    cleanup_test_state(&source).await;
    cleanup_test_state(&target).await;
}

run_on_main_metadata_backends!(
    foreground_recovery_stops_after_first_unavailable_chunk_impl,
    foreground_recovery_stops_after_first_unavailable_chunk,
    foreground_recovery_stops_after_first_unavailable_chunk_turso
);

#[test]
fn full_object_recovery_budget_scales_with_parallel_chunk_batches() {
    use crate::content_recovery::{
        FULL_OBJECT_RECOVERY_BUDGET_MAX, READ_THROUGH_RECOVERY_BUDGET, full_object_recovery_budget,
    };

    assert_eq!(full_object_recovery_budget(0), READ_THROUGH_RECOVERY_BUDGET);
    assert_eq!(full_object_recovery_budget(1), READ_THROUGH_RECOVERY_BUDGET);
    assert_eq!(full_object_recovery_budget(4), READ_THROUGH_RECOVERY_BUDGET);
    assert_eq!(
        full_object_recovery_budget(5),
        READ_THROUGH_RECOVERY_BUDGET.saturating_mul(2)
    );
    assert_eq!(
        full_object_recovery_budget(40),
        FULL_OBJECT_RECOVERY_BUDGET_MAX
    );
    assert_eq!(
        full_object_recovery_budget(usize::MAX),
        FULL_OBJECT_RECOVERY_BUDGET_MAX,
        "even pathological manifests must retain a bounded foreground deadline"
    );
}

#[test]
fn object_read_recovery_budget_scales_only_full_object_reads() {
    assert_eq!(
        crate::object_read_recovery_budget(true, 40),
        crate::content_recovery::READ_THROUGH_RECOVERY_BUDGET,
        "a byte range must retain its bounded foreground latency"
    );
    assert_eq!(
        crate::object_read_recovery_budget(false, 40),
        crate::content_recovery::FULL_OBJECT_RECOVERY_BUDGET_MAX,
        "a whole-object read may scale for useful progress but must remain bounded"
    );
}

async fn media_preview_budget_exhaustion_is_retryable_impl(backend: MainTestBackend) {
    let target = build_test_state(1, false, backend).await;
    let source = build_test_state(1, false, backend).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let release = Arc::new(tokio::sync::Notify::new());
    let wait = release.clone();
    let app = axum::Router::new().route(
        "/cluster/v2/replication/chunk/{hash}",
        axum::routing::get(move || {
            let wait = wait.clone();
            async move {
                wait.notified().await;
                StatusCode::NOT_FOUND
            }
        }),
    );
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    register_online_source_node(&target, &source, &url).await;
    let missing_chunk = crate::storage::ReplicationChunkInfo {
        hash: blake3::hash(b"missing preview bytes").to_hex().to_string(),
        size_bytes: 21,
    };

    let result = tokio::time::timeout(
        Duration::from_secs(1),
        crate::recover_missing_chunks_for_media_preview(
            &target,
            "preview.bin@v1",
            &[missing_chunk],
            Duration::from_millis(50),
        ),
    )
    .await
    .expect("preview recovery must honor its budget")
    .expect("budget exhaustion must not become an internal error");

    release.notify_waiters();
    handle.abort();
    let _ = handle.await;
    cleanup_test_state(&source).await;
    cleanup_test_state(&target).await;
    assert!(!result, "an incomplete preview must remain retryable");
}

run_on_main_metadata_backends!(
    media_preview_budget_exhaustion_is_retryable_impl,
    media_preview_budget_exhaustion_is_retryable,
    media_preview_budget_exhaustion_is_retryable_turso
);

#[tokio::test]
async fn recovery_manifest_checks_all_hash_sources_before_legacy_exports() {
    let source = build_test_state(1, false, MainTestBackend::Sqlite).await;
    let dummy = build_test_state(1, false, MainTestBackend::Sqlite).await;
    let target = build_test_state(1, false, MainTestBackend::Sqlite).await;
    let key = "hash-source-before-legacy.bin";
    let version = "ver-hash-source-before-legacy";
    seed_subject_version(
        &source,
        key,
        version,
        b"manifest source ordering".to_vec(),
        vec![],
    )
    .await;
    let export = bundle(&source, key, version).await;
    let reference = crate::storage::retained_content::RetainedReference {
        key: Some(key.to_string()),
        object_id: None,
        version_id: Some(version.to_string()),
        manifest_hash: export.manifest_hash.clone(),
        snapshot_only: false,
    };
    let manifest_path = format!("/cluster/v2/replication/manifest/{}", export.manifest_hash);

    let bad_requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let requests = bad_requests.clone();
    let bad_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bad_url = format!("http://{}", bad_listener.local_addr().unwrap());
    let bad_app = axum::Router::new().fallback(axum::routing::any(
        move |request: axum::extract::Request| {
            let requests = requests.clone();
            async move {
                requests.lock().await.push(request.uri().path().to_string());
                StatusCode::NOT_FOUND
            }
        },
    ));
    let bad_handle = tokio::spawn(async move {
        axum::serve(bad_listener, bad_app).await.unwrap();
    });

    let good_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let good_url = format!("http://{}", good_listener.local_addr().unwrap());
    let expected_manifest = export.manifest_bytes.clone();
    let good_app = axum::Router::new().route(
        &manifest_path,
        axum::routing::get(move || {
            let bytes = expected_manifest.clone();
            async move { axum::body::Body::from(bytes) }
        }),
    );
    let good_handle = tokio::spawn(async move {
        axum::serve(good_listener, good_app).await.unwrap();
    });

    let (first, second) = if source.node_id < dummy.node_id {
        (&source, &dummy)
    } else {
        (&dummy, &source)
    };
    register_online_source_node(&target, first, &bad_url).await;
    register_online_source_node(&target, second, &good_url).await;

    let recovered = crate::content_recovery::recover_manifest(&target, &reference)
        .await
        .unwrap();
    assert_eq!(recovered, export.manifest_bytes);
    assert_eq!(
        bad_requests.lock().await.as_slice(),
        [manifest_path.as_str()],
        "an early 404 must not trigger its legacy export before later hash sources are tried"
    );

    bad_handle.abort();
    good_handle.abort();
    let _ = bad_handle.await;
    let _ = good_handle.await;
    for state in [&source, &dummy, &target] {
        cleanup_test_state(state).await;
    }
}

async fn recovery_verification_can_outlive_transfer_budget_impl(backend: MainTestBackend) {
    let state = build_test_state(1, false, backend).await;
    let key = format!("verification-outlives-budget-{}.bin", backend.suffix());
    let version = "ver-verification-outlives-budget";
    seed_subject_version(
        &state,
        &key,
        version,
        b"already local verification bytes".to_vec(),
        vec![],
    )
    .await;
    let manifest = bundle(&state, &key, version).await;
    let reference = read_store(&state, "test.recovery.verification_budget_reference")
        .await
        .retained_content()
        .await
        .unwrap()
        .reference_for_subject(&format!("{key}@{version}"))
        .unwrap()
        .clone();
    let verification = crate::storage::content_recovery::block_recovery_verification_for_test(
        &manifest.manifest_hash,
    );
    let mut task = crate::storage::content_recovery::ContentRepairTask::new(reference, true);
    let recovery = crate::content_recovery::recover_task_with_budget(
        &state,
        &mut task,
        Duration::from_millis(500),
    );
    tokio::pin!(recovery);

    tokio::select! {
        () = verification.wait_until_started() => {}
        outcome = &mut recovery => panic!("recovery ended before local verification: {outcome:?}"),
        () = tokio::time::sleep(Duration::from_secs(5)) => {
            panic!("recovery did not reach local verification")
        }
    }
    let writer = tokio::time::timeout(
        Duration::from_secs(1),
        lock_store(&state, "test.recovery.verification_without_store_guard"),
    )
    .await
    .expect("full-object verification must not retain the global store read guard");
    drop(writer);
    assert!(
        tokio::time::timeout(Duration::from_millis(600), &mut recovery)
            .await
            .is_err(),
        "the transfer deadline cancelled a locally complete object's verification"
    );
    verification.release();
    let recovered = tokio::time::timeout(Duration::from_secs(5), &mut recovery)
        .await
        .expect("verification should finish after release")
        .expect("locally complete recovery should succeed");
    assert_eq!(recovered, 0);
    assert_eq!(verification.calls(), 1);
    crate::storage::content_recovery::clear_recovery_verification_blocker_for_test(
        &manifest.manifest_hash,
    );

    cleanup_test_state(&state).await;
}

run_on_main_metadata_backends!(
    recovery_verification_can_outlive_transfer_budget_impl,
    recovery_verification_can_outlive_transfer_budget,
    recovery_verification_can_outlive_transfer_budget_turso
);

async fn recovery_prepare_timeout_preserves_persisted_gc_pin_impl(backend: MainTestBackend) {
    let state = build_test_state(1, false, backend).await;
    let key = format!("prepare-timeout-gc-pin-{}.bin", backend.suffix());
    let version = "ver-prepare-timeout-gc-pin";
    seed_subject_version(
        &state,
        &key,
        version,
        format!("prepare timeout bytes for {}", backend.suffix()).into_bytes(),
        vec![],
    )
    .await;
    let manifest = bundle(&state, &key, version).await;
    let reference = read_store(&state, "test.recovery.prepare_timeout_reference")
        .await
        .retained_content()
        .await
        .unwrap()
        .reference_for_subject(&format!("{key}@{version}"))
        .unwrap()
        .clone();
    let expected_chunks = manifest.manifest.chunks.clone();
    let mut task = crate::storage::content_recovery::ContentRepairTask::new(reference, true);
    task.chunks = expected_chunks.clone();
    read_store(&state, "test.recovery.prepare_timeout_initial_pin")
        .await
        .persist_content_repair_task(&task)
        .await
        .unwrap();

    let preparation =
        crate::content_recovery::block_recovery_preparation_for_test(&manifest.manifest_hash);
    let error = {
        let recovery = crate::content_recovery::recover_task_with_budget(
            &state,
            &mut task,
            Duration::from_millis(500),
        );
        tokio::pin!(recovery);
        tokio::select! {
            () = preparation.wait_until_started() => {}
            outcome = &mut recovery => panic!("recovery ended before preparation blocked: {outcome:?}"),
            () = tokio::time::sleep(Duration::from_secs(5)) => {
                panic!("recovery did not reach task preparation")
            }
        }
        let writer = tokio::time::timeout(
            Duration::from_secs(1),
            lock_store(&state, "test.recovery.preparation_without_store_guard"),
        )
        .await
        .expect("manifest preparation must not retain the global store read guard");
        drop(writer);
        tokio::time::timeout(Duration::from_secs(2), &mut recovery)
            .await
            .expect("the internal transfer deadline should end preparation")
            .unwrap_err()
    };
    assert!(
        error.is::<crate::content_recovery::DurableRepairBudgetExceeded>(),
        "the artificial deadline must cancel task preparation: {error:#}"
    );
    preparation.release();
    crate::content_recovery::clear_recovery_preparation_blocker_for_test(&manifest.manifest_hash);

    task.defer("repair preparation timed out".to_string(), 100, 30, false);
    read_store(&state, "test.recovery.prepare_timeout_defer")
        .await
        .persist_content_repair_task(&task)
        .await
        .unwrap();
    let persisted = read_store(&state, "test.recovery.prepare_timeout_persisted")
        .await
        .content_repair_tasks()
        .await
        .unwrap();
    assert_eq!(persisted.len(), 1, "{persisted:?}");
    assert_eq!(persisted[0].chunks.len(), expected_chunks.len());
    assert_eq!(
        persisted[0]
            .chunks
            .iter()
            .map(|chunk| (&chunk.hash, chunk.size_bytes))
            .collect::<Vec<_>>(),
        expected_chunks
            .iter()
            .map(|chunk| (&chunk.hash, chunk.size_bytes))
            .collect::<Vec<_>>(),
        "a cancelled prepare pass must preserve the complete durable GC pin"
    );

    cleanup_test_state(&state).await;
}

run_on_main_metadata_backends!(
    recovery_prepare_timeout_preserves_persisted_gc_pin_impl,
    recovery_prepare_timeout_preserves_persisted_gc_pin,
    recovery_prepare_timeout_preserves_persisted_gc_pin_turso
);

#[tokio::test]
async fn durable_recovery_budget_keeps_stalled_work_retryable() {
    let error =
        crate::content_recovery::bounded_durable_recovery(Duration::from_millis(5), async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok::<(), anyhow::Error>(())
        })
        .await
        .unwrap_err();
    assert!(
        error.is::<crate::content_recovery::DurableRepairBudgetExceeded>(),
        "the bounded pass must report its deadline distinctly: {error:#}"
    );
    assert!(
        crate::content_recovery::repair_waits_for_retry(&error),
        "a durable repair deadline must remain a queued retry rather than an unresolved failure"
    );
}

async fn recovery_targeted_repair_respects_busy_throttle_impl(backend: MainTestBackend) {
    let source = build_test_state(1, false, backend).await;
    let mut target = build_test_state(1, false, backend).await;
    target.repair_config.busy_throttle_enabled = true;
    target.repair_config.busy_inflight_threshold = 1;
    target.repair_config.busy_wait_millis = 5;
    let key = "busy-targeted-recovery.bin";
    let version = "v1";
    for state in [&source, &target] {
        seed_subject_version(state, key, version, b"busy repair payload".to_vec(), vec![]).await;
    }
    let manifest = bundle(&target, key, version).await;
    remove_chunks(&target, &manifest, &[0]).await;
    let (url, handle) = spawn_internal_peer_api_server(source.clone()).await;
    register_online_source_node(&target, &source, &url).await;
    let inflight = Arc::clone(&target.maintenance.inflight_requests);
    inflight.store(2, std::sync::atomic::Ordering::Relaxed);
    let repair = crate::replication::execute_targeted_replication_repair_inner(
        &target,
        vec![format!("{key}@{version}")],
        None,
    );
    tokio::pin!(repair);
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut repair)
            .await
            .is_err(),
        "targeted retained-content recovery ignored the busy throttle"
    );
    let claim = target
        .maintenance
        .content_repair_claims
        .try_claim(&manifest.manifest_hash);
    assert!(
        claim.is_some(),
        "a busy-throttled repair must not block a foreground pull's manifest claim"
    );
    drop(claim);
    inflight.store(0, std::sync::atomic::Ordering::Relaxed);
    let report = repair.await;
    assert_eq!(report.successful_transfers, 1, "{report:?}");
    handle.abort();
    let _ = handle.await;
    cleanup_test_state(&source).await;
    cleanup_test_state(&target).await;
}

run_on_main_metadata_backends!(
    recovery_targeted_repair_respects_busy_throttle_impl,
    recovery_targeted_repair_respects_busy_throttle,
    recovery_targeted_repair_respects_busy_throttle_turso
);

async fn recovery_targeted_repair_defers_claimed_manifest_impl(backend: MainTestBackend) {
    let source = build_test_state(1, false, backend).await;
    let target = build_test_state(1, false, backend).await;
    let (url, handle) = spawn_internal_peer_api_server(source.clone()).await;
    register_online_source_node(&target, &source, &url).await;
    let key = "claimed-targeted-recovery.bin";
    let version = "v1";
    for state in [&source, &target] {
        seed_subject_version(
            state,
            key,
            version,
            b"claimed targeted repair payload".to_vec(),
            vec![],
        )
        .await;
    }
    let manifest = bundle(&target, key, version).await;
    remove_chunks(&target, &manifest, &[0]).await;
    let source_node = target
        .cluster
        .lock()
        .await
        .list_nodes()
        .into_iter()
        .find(|node| node.node_id == source.node_id)
        .expect("registered source descriptor must be retained by the target");
    let plan = crate::cluster::ReplicationPlan {
        generated_at_unix: 0,
        under_replicated: 1,
        over_replicated: 0,
        cleanup_deferred_items: 0,
        cleanup_deferred_extra_nodes: 0,
        items: vec![crate::cluster::ReplicationPlanItem {
            key: format!("{key}@{version}"),
            desired_nodes: vec![target.node_id],
            current_nodes: vec![source.node_id],
            missing_nodes: vec![target.node_id],
            extra_nodes: Vec::new(),
            cleanup_option: crate::cluster::ReplicationCleanupOption::None,
            deferred_extra_nodes: 0,
        }],
    };
    let claim = target
        .maintenance
        .content_repair_claims
        .try_claim(&manifest.manifest_hash)
        .expect("test must hold the manifest claim");

    let report = tokio::time::timeout(Duration::from_secs(1), async {
        crate::replication::execute_replication_repair_plan(
            &target,
            &plan,
            vec![source_node],
            None,
            None,
        )
        .await
    })
    .await
    .expect("a claimed manifest must not block the rest of the repair plan");

    assert_eq!(report.attempted_transfers, 0, "{report:?}");
    assert_eq!(report.successful_transfers, 0, "{report:?}");
    assert_eq!(report.failed_transfers, 0, "{report:?}");
    assert_eq!(report.skipped_items, 1, "{report:?}");
    assert!(
        report.detailed_log.iter().any(|entry| {
            entry.event == "repair_deferred"
                && entry
                    .context
                    .as_ref()
                    .and_then(|context| context["reason"].as_str())
                    == Some("manifest_repair_in_progress")
        }),
        "claim contention must remain observable without being marked as a failed transfer: {report:?}"
    );
    assert!(report.skipped_details.iter().any(|detail| {
        detail.reason == crate::replication::ReplicationRepairSkipReason::RepairInProgress
    }));

    drop(claim);
    handle.abort();
    let _ = handle.await;
    cleanup_test_state(&source).await;
    cleanup_test_state(&target).await;
}

run_on_main_metadata_backends!(
    recovery_targeted_repair_defers_claimed_manifest_impl,
    recovery_targeted_repair_defers_claimed_manifest,
    recovery_targeted_repair_defers_claimed_manifest_turso
);

async fn recovery_enqueue_only_pass_bypasses_busy_throttle_impl(backend: MainTestBackend) {
    let mut state = build_test_state(1, false, backend).await;
    state.repair_config.busy_throttle_enabled = true;
    state.repair_config.busy_inflight_threshold = 1;
    state.repair_config.busy_wait_millis = 5;
    let key = choose_locally_placed_key(&state, "busy-enqueue-only").await;
    seed_subject_version(
        &state,
        &key,
        "v1",
        b"durable finding bytes".to_vec(),
        vec![],
    )
    .await;
    let manifest = bundle(&state, &key, "v1").await;
    state
        .maintenance
        .inflight_requests
        .store(2, std::sync::atomic::Ordering::Relaxed);

    let report = tokio::time::timeout(
        Duration::from_millis(100),
        crate::content_recovery::repair_subjects(
            &state,
            vec![format!(
                "{MANIFEST_SUBJECT_PREFIX}{}",
                manifest.manifest_hash
            )],
            Some(0),
        ),
    )
    .await
    .expect("enqueue-only repair work must not wait for foreground load");

    assert_eq!(report.skipped_items, 1, "{report:?}");
    assert!(
        read_store(&state, "test.recovery.busy_enqueue_only")
            .await
            .content_repair_tasks()
            .await
            .unwrap()
            .iter()
            .any(|task| task.reference.manifest_hash == manifest.manifest_hash),
        "the enqueue-only pass must durably publish its repair finding"
    );
    cleanup_test_state(&state).await;
}

run_on_main_metadata_backends!(
    recovery_enqueue_only_pass_bypasses_busy_throttle_impl,
    recovery_enqueue_only_pass_bypasses_busy_throttle,
    recovery_enqueue_only_pass_bypasses_busy_throttle_turso
);

async fn recovery_backoff_starts_after_a_slow_transfer_impl(backend: MainTestBackend) {
    let source = build_test_state(1, false, backend).await;
    let mut target = build_test_state(1, false, backend).await;
    target.repair_config.backoff_secs = 1;
    let key = "slow-backoff.bin";
    let version = "v1";
    for state in [&source, &target] {
        seed_subject_version(state, key, version, b"slow retry payload".to_vec(), vec![]).await;
    }
    let manifest = bundle(&target, key, version).await;
    remove_chunks(&target, &manifest, &[0]).await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new().route(
        "/cluster/v2/replication/chunk/{hash}",
        axum::routing::get(|| async {
            tokio::time::sleep(Duration::from_secs(3)).await;
            StatusCode::NOT_FOUND
        }),
    );
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    register_online_source_node(&target, &source, &url).await;

    let report = crate::execute_tracked_targeted_local_replication_repair(
        &target,
        vec![format!("{key}@{version}")],
        crate::RepairRunTrigger::DataScrubAutoRepair,
    )
    .await;
    assert_eq!(report.failed_transfers, 1, "{report:?}");
    let store = read_store(&target, "test.recovery.slow_backoff").await;
    let pending = store.content_repair_tasks().await.unwrap();
    assert_eq!(pending.len(), 1);
    assert!(
        pending[0].next_attempt_unix > crate::unix_ts(),
        "backoff must begin after a slow failed transfer, not before it"
    );
    drop(store);
    handle.abort();
    let _ = handle.await;
    cleanup_test_state(&source).await;
    cleanup_test_state(&target).await;
}

run_on_main_metadata_backends!(
    recovery_backoff_starts_after_a_slow_transfer_impl,
    recovery_backoff_starts_after_a_slow_transfer,
    recovery_backoff_starts_after_a_slow_transfer_turso
);

async fn recovery_scrub_quarantines_intent_when_execution_is_disabled_impl(
    backend: MainTestBackend,
) {
    let mut target = build_test_state(1, false, backend).await;
    target.repair_config.enabled = false;
    let key = "disabled-recovery.bin";
    seed_subject_version(&target, key, "v1", b"damaged bytes".to_vec(), vec![]).await;
    let manifest = bundle(&target, key, "v1").await;
    fs::write(
        read_store(&target, "test.recovery.damage_disabled")
            .await
            .chunk_path_for_test(&manifest.manifest.chunks[0].hash),
        b"corrupt bytes",
    )
    .await
    .unwrap();
    crate::start_local_data_scrub(&target, crate::DataScrubRunTrigger::ManualRequest).await;
    wait_for_data_scrub_completion(&target).await;
    assert_eq!(
        read_store(&target, "test.recovery.disabled_queue")
            .await
            .content_repair_tasks()
            .await
            .unwrap()
            .len(),
        1,
        "a scrub-confirmed corrupt replica must remain quarantined until it can be repaired"
    );
    crate::refresh_local_availability_view_once(&target).await;
    assert!(
        crate::cached_local_cluster_available_subjects(&target)
            .await
            .is_empty(),
        "a refresh must not re-advertise content with a pending integrity finding"
    );
    assert!(
        repair_run_history(&target).await.is_empty(),
        "disabled repair must not transfer content"
    );
    cleanup_test_state(&target).await;
}

run_on_main_metadata_backends!(
    recovery_scrub_quarantines_intent_when_execution_is_disabled_impl,
    recovery_scrub_quarantines_intent_when_execution_is_disabled,
    recovery_scrub_quarantines_intent_when_execution_is_disabled_turso
);

async fn recovery_batch_limit_keeps_all_intent_durable_impl(backend: MainTestBackend) {
    let target = build_test_state(1, false, backend).await;
    let mut subjects = Vec::new();
    for key in ["batch/one", "batch/two"] {
        seed_subject_version(&target, key, "ver-batch", key.as_bytes().to_vec(), vec![]).await;
        let manifest = bundle(&target, key, "ver-batch").await;
        remove_chunks(&target, &manifest, &[0]).await;
        subjects.push(format!("{key}@ver-batch"));
    }
    let report =
        crate::replication::execute_targeted_replication_repair_inner(&target, subjects, Some(1))
            .await;
    assert_eq!(report.attempted_transfers, 1);
    assert_eq!(
        report.run_status(),
        crate::RepairRunStatus::WaitingForSource,
        "the first item has no source; the later batch-capacity deferral must not change that"
    );
    assert!(
        report
            .detailed_log
            .iter()
            .any(|entry| entry.event == "repair_deferred"),
        "a batch-capacity deferral must not be reported as a missing source: {report:?}"
    );
    assert_eq!(
        read_store(&target, "test.recovery.batch_queue")
            .await
            .content_repair_tasks()
            .await
            .unwrap()
            .len(),
        2,
        "the execution budget must not discard unattempted repair intent"
    );
    cleanup_test_state(&target).await;
}

run_on_main_metadata_backends!(
    recovery_batch_limit_keeps_all_intent_durable_impl,
    recovery_batch_limit_keeps_all_intent_durable,
    recovery_batch_limit_keeps_all_intent_durable_turso
);

async fn recovery_batch_limit_skips_contended_and_backoff_tasks_impl(backend: MainTestBackend) {
    let target = build_test_state(1, false, backend).await;
    let mut manifests = Vec::new();
    for prefix in ["batch-contention", "batch-backoff", "batch-ready"] {
        let key = choose_locally_placed_key(&target, prefix).await;
        seed_subject_version(
            &target,
            &key,
            "v1",
            format!("{prefix} bytes").into_bytes(),
            vec![],
        )
        .await;
        manifests.push((bundle(&target, &key, "v1").await.manifest_hash, key));
    }
    manifests.sort();
    let contended = manifests[0].clone();
    let backoff = manifests[1].clone();
    let ready = manifests[2].clone();
    let held_claim = target
        .maintenance
        .content_repair_claims
        .try_claim(&contended.0)
        .expect("test must hold the contended manifest claim");

    let reference = read_store(&target, "test.recovery.batch_backoff_reference")
        .await
        .retained_content()
        .await
        .unwrap()
        .reference_for_subject(&format!("{}@v1", backoff.1))
        .unwrap()
        .clone();
    let mut deferred = crate::storage::content_recovery::ContentRepairTask::new(reference, true);
    deferred.next_attempt_unix = crate::unix_ts() + 60;
    deferred.source_fingerprint = blake3::hash(b"[]").to_hex().to_string();
    read_store(&target, "test.recovery.batch_backoff_persist")
        .await
        .persist_content_repair_task(&deferred)
        .await
        .unwrap();

    let report = crate::content_recovery::repair_subjects(
        &target,
        manifests
            .iter()
            .map(|(_, key)| format!("{key}@v1"))
            .collect(),
        Some(1),
    )
    .await;

    assert_eq!(report.attempted_transfers, 1, "{report:?}");
    assert_eq!(report.successful_transfers, 1, "{report:?}");
    assert_eq!(report.skipped_backoff, 1, "{report:?}");
    assert_eq!(report.skipped_items, 2, "{report:?}");
    let ready_subject = format!("{}@v1", ready.1);
    assert!(
        report.detailed_log.iter().any(|entry| {
            entry.event == "repair_verified"
                && entry.subject.as_deref() == Some(ready_subject.as_str())
        }),
        "the ready tail task must use the transfer slot after contended and backoff tasks: {report:?}"
    );
    let pending = read_store(&target, "test.recovery.batch_backoff_pending")
        .await
        .content_repair_tasks()
        .await
        .unwrap();
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert_eq!(pending[0].reference.manifest_hash, backoff.0);

    drop(held_claim);
    cleanup_test_state(&target).await;
}

run_on_main_metadata_backends!(
    recovery_batch_limit_skips_contended_and_backoff_tasks_impl,
    recovery_batch_limit_skips_contended_and_backoff_tasks,
    recovery_batch_limit_skips_contended_and_backoff_tasks_turso
);

async fn recovery_worker_skips_already_claimed_tasks_impl(backend: MainTestBackend) {
    let target = build_test_state(1, false, backend).await;
    let key = choose_locally_placed_key(&target, "claimed-background-task").await;
    seed_subject_version(
        &target,
        &key,
        "v1",
        b"claimed repair bytes".to_vec(),
        vec![],
    )
    .await;
    let reference = read_store(&target, "test.recovery.claimed_task_reference")
        .await
        .retained_content()
        .await
        .unwrap()
        .reference_for_subject(&format!("{key}@v1"))
        .unwrap()
        .clone();
    let task = crate::storage::content_recovery::ContentRepairTask::new(reference, true);
    let manifest_hash = task.reference.manifest_hash.clone();
    read_store(&target, "test.recovery.persist_claimed_task")
        .await
        .persist_content_repair_task(&task)
        .await
        .unwrap();
    let claim = target
        .maintenance
        .content_repair_claims
        .try_claim(&manifest_hash)
        .expect("test must hold the pending manifest claim");

    crate::content_recovery::resume_pending(&target)
        .await
        .unwrap();

    assert!(
        repair_run_history(&target).await.is_empty(),
        "the background worker must not create a completed repair run while another repair owns the task"
    );
    assert_eq!(
        read_store(&target, "test.recovery.claimed_task_pending")
            .await
            .content_repair_task_hashes()
            .await
            .unwrap(),
        vec![manifest_hash],
        "a contended task must remain durable for its existing owner"
    );

    drop(claim);
    cleanup_test_state(&target).await;
}

run_on_main_metadata_backends!(
    recovery_worker_skips_already_claimed_tasks_impl,
    recovery_worker_skips_already_claimed_tasks,
    recovery_worker_skips_already_claimed_tasks_turso
);

async fn recovery_worker_skips_claimed_head_and_repairs_next_task_impl(backend: MainTestBackend) {
    let target = build_test_state(1, false, backend).await;
    let mut tasks = Vec::new();
    for key in ["claimed-queue-head/one", "claimed-queue-head/two"] {
        seed_subject_version(
            &target,
            key,
            "v1",
            format!("healthy bytes for {key}").into_bytes(),
            vec![],
        )
        .await;
        let reference = read_store(&target, "test.recovery.claimed_head_reference")
            .await
            .retained_content()
            .await
            .unwrap()
            .reference_for_subject(&format!("{key}@v1"))
            .unwrap()
            .clone();
        tasks.push(crate::storage::content_recovery::ContentRepairTask::new(
            reference, true,
        ));
    }
    tasks.sort_by(|left, right| {
        left.reference
            .manifest_hash
            .cmp(&right.reference.manifest_hash)
    });
    for task in &tasks {
        read_store(&target, "test.recovery.claimed_head_persist")
            .await
            .persist_content_repair_task(task)
            .await
            .unwrap();
    }
    let claim = target
        .maintenance
        .content_repair_claims
        .try_claim(&tasks[0].reference.manifest_hash)
        .expect("test must hold the first due task's manifest claim");

    crate::content_recovery::resume_pending(&target)
        .await
        .unwrap();

    assert_eq!(
        repair_run_history(&target).await.len(),
        1,
        "a claimed queue head must not stall an unrelated due repair"
    );
    assert_eq!(
        read_store(&target, "test.recovery.claimed_head_pending")
            .await
            .content_repair_task_hashes()
            .await
            .unwrap(),
        vec![tasks[0].reference.manifest_hash.clone()],
        "the worker must repair the next due task while preserving the claimed head"
    );

    drop(claim);
    cleanup_test_state(&target).await;
}

run_on_main_metadata_backends!(
    recovery_worker_skips_claimed_head_and_repairs_next_task_impl,
    recovery_worker_skips_claimed_head_and_repairs_next_task,
    recovery_worker_skips_claimed_head_and_repairs_next_task_turso
);

async fn recovery_worker_bounds_one_background_pass_impl(backend: MainTestBackend) {
    let mut target = build_test_state(1, false, backend).await;
    target.repair_config.batch_size = 32;
    let mut subjects = Vec::new();
    for key in ["background-pass/one", "background-pass/two"] {
        seed_subject_version(
            &target,
            key,
            "v1",
            format!("healthy bytes for {key}").into_bytes(),
            vec![],
        )
        .await;
        subjects.push(format!("{key}@v1"));
    }
    let retained = read_store(&target, "test.recovery.background_pass_references")
        .await
        .retained_content()
        .await
        .unwrap();
    for subject in &subjects {
        let task = crate::storage::content_recovery::ContentRepairTask::new(
            retained.reference_for_subject(subject).unwrap().clone(),
            true,
        );
        read_store(&target, "test.recovery.background_pass_persist")
            .await
            .persist_content_repair_task(&task)
            .await
            .unwrap();
    }

    crate::content_recovery::resume_pending(&target)
        .await
        .unwrap();

    assert_eq!(
        repair_run_history(&target).await.len(),
        1,
        "one bounded worker pass must publish one repair-run outcome"
    );
    assert_eq!(
        read_store(&target, "test.recovery.background_pass_pending")
            .await
            .content_repair_tasks()
            .await
            .unwrap()
            .len(),
        1,
        "a large configured batch must not run every durable task in one pass"
    );

    cleanup_test_state(&target).await;
}

run_on_main_metadata_backends!(
    recovery_worker_bounds_one_background_pass_impl,
    recovery_worker_bounds_one_background_pass,
    recovery_worker_bounds_one_background_pass_turso
);

async fn recovery_local_install_failure_is_unresolved_impl(backend: MainTestBackend) {
    let source = build_test_state(1, false, backend).await;
    let target = build_test_state(1, false, backend).await;
    let key = "local-error.bin";
    for node in [&source, &target] {
        seed_subject_version(node, key, "v1", b"healthy peer data".to_vec(), vec![]).await;
    }
    let manifest = bundle(&target, key, "v1").await;
    remove_chunks(&target, &manifest, &[0]).await;
    let shard = read_store(&target, "test.recovery.obstruct")
        .await
        .chunk_path_for_test(&manifest.manifest.chunks[0].hash)
        .parent()
        .unwrap()
        .to_path_buf();
    fs::remove_dir(&shard).await.unwrap();
    fs::write(&shard, b"local write obstruction").await.unwrap();
    let (url, handle) = spawn_internal_peer_api_server(source.clone()).await;
    register_online_source_node(&target, &source, &url).await;
    let report = crate::replication::execute_targeted_replication_repair_inner(
        &target,
        vec![format!("{key}@v1")],
        None,
    )
    .await;
    assert_eq!(
        report.run_status(),
        crate::RepairRunStatus::Unresolved,
        "a local disk error is not a missing source: {report:?}"
    );
    assert_eq!(
        read_store(&target, "test.recovery.error_queue")
            .await
            .content_repair_tasks()
            .await
            .unwrap()
            .len(),
        1
    );
    handle.abort();
    let _ = handle.await;
    cleanup_test_state(&source).await;
    cleanup_test_state(&target).await;
}

run_on_main_metadata_backends!(
    recovery_local_install_failure_is_unresolved_impl,
    recovery_local_install_failure_is_unresolved,
    recovery_local_install_failure_is_unresolved_turso
);

async fn bundle(state: &ServerState, key: &str, version: &str) -> ReplicationExportBundle {
    read_store(state, "test.recovery.bundle")
        .await
        .export_replication_bundle(key, Some(version), ObjectReadMode::Preferred)
        .await
        .unwrap()
        .unwrap()
}

async fn remove_chunks(state: &ServerState, bundle: &ReplicationExportBundle, indexes: &[usize]) {
    let store = read_store(state, "test.recovery.remove_chunks").await;
    for index in indexes {
        fs::remove_file(store.chunk_path_for_test(&bundle.manifest.chunks[*index].hash))
            .await
            .unwrap();
    }
}

async fn recovery_audit_finds_unadvertised_assigned_gap_impl(backend: MainTestBackend) {
    let source = build_test_state(1, false, backend).await;
    let target = build_test_state(1, false, backend).await;
    let key = choose_locally_placed_key(&target, "unadvertised-gap").await;
    let version = "ver-metadata-only";
    seed_subject_version(
        &source,
        &key,
        version,
        b"recover without inventory".to_vec(),
        vec![],
    )
    .await;
    let metadata = read_store(&source, "test.recovery.metadata_only")
        .await
        .export_metadata_bundle(&key, Some(version), ObjectReadMode::Preferred)
        .await
        .unwrap()
        .unwrap();
    lock_store(&target, "test.recovery.import_metadata_only")
        .await
        .import_metadata_bundle(&metadata)
        .await
        .unwrap();
    let report = crate::replication::execute_replication_repair_inner(&target, None).await;
    assert!(
        !report
            .detailed_log
            .iter()
            .any(|entry| entry.event == "local_replica_registered"),
        "exportable metadata must not be mistaken for a healthy replica: {report:?}"
    );
    crate::content_recovery::audit_assigned(&target)
        .await
        .unwrap();
    let (url, handle) = spawn_internal_peer_api_server(source.clone()).await;
    register_online_source_node(&target, &source, &url).await;
    crate::content_recovery::resume_pending(&target)
        .await
        .unwrap();
    assert_eq!(
        read_store(&target, "test.recovery.audit_result")
            .await
            .get_object(&key, None, Some(version), ObjectReadMode::Preferred)
            .await
            .unwrap()
            .as_ref(),
        b"recover without inventory"
    );
    handle.abort();
    let _ = handle.await;
    cleanup_test_state(&source).await;
    cleanup_test_state(&target).await;
}

run_on_main_metadata_backends!(
    recovery_audit_finds_unadvertised_assigned_gap_impl,
    recovery_audit_finds_unadvertised_assigned_gap,
    recovery_audit_finds_unadvertised_assigned_gap_turso
);

async fn recovery_audit_skips_complete_local_content_with_empty_availability_impl(
    backend: MainTestBackend,
) {
    let target = build_test_state(1, false, backend).await;
    let key = choose_locally_placed_key(&target, "empty-availability").await;
    seed_subject_version(
        &target,
        &key,
        "ver-healthy-local",
        b"healthy local content".to_vec(),
        vec![],
    )
    .await;

    // Model first-start convergence: the content exists locally, but the
    // cluster availability cache has not yet learned about it.
    assert!(
        target
            .cluster
            .lock()
            .await
            .available_subjects_for_node(target.node_id)
            .is_empty()
    );

    crate::content_recovery::audit_assigned(&target)
        .await
        .unwrap();

    assert!(
        read_store(&target, "test.recovery.empty_availability")
            .await
            .content_repair_tasks()
            .await
            .unwrap()
            .is_empty(),
        "a complete local replica must not be queued while availability converges"
    );
    cleanup_test_state(&target).await;
}

run_on_main_metadata_backends!(
    recovery_audit_skips_complete_local_content_with_empty_availability_impl,
    recovery_audit_skips_complete_local_content_with_empty_availability,
    recovery_audit_skips_complete_local_content_with_empty_availability_turso
);

async fn recovery_audit_rechecks_complete_retained_history_presence_impl(backend: MainTestBackend) {
    let state = build_test_state(1, false, backend).await;
    let key = choose_locally_placed_key(&state, "cached-retained-history").await;
    seed_subject_version(&state, &key, "v1", b"older retained bytes".to_vec(), vec![]).await;
    seed_subject_version(
        &state,
        &key,
        "v2",
        b"current retained bytes".to_vec(),
        vec!["v1".to_string()],
    )
    .await;
    let old = bundle(&state, &key, "v1").await;

    crate::content_recovery::audit_assigned(&state)
        .await
        .unwrap();
    assert!(
        read_store(&state, "test.recovery.complete_history")
            .await
            .content_repair_tasks()
            .await
            .unwrap()
            .is_empty(),
        "complete current and historical replicas must not be queued for repair"
    );

    // Model out-of-band loss after the first audit. Retained history is
    // rechecked on every audit, rather than hidden behind a short-lived cache.
    remove_chunks(&state, &old, &[0]).await;
    crate::content_recovery::audit_assigned(&state)
        .await
        .unwrap();
    assert!(
        read_store(&state, "test.recovery.rechecked_history")
            .await
            .content_repair_tasks()
            .await
            .unwrap()
            .iter()
            .any(|task| task.reference.manifest_hash == old.manifest_hash),
        "the next audit must detect out-of-band loss in retained history and queue repair"
    );
    cleanup_test_state(&state).await;
}

run_on_main_metadata_backends!(
    recovery_audit_rechecks_complete_retained_history_presence_impl,
    recovery_audit_rechecks_complete_retained_history_presence,
    recovery_audit_rechecks_complete_retained_history_presence_turso
);

async fn recovery_audit_bounds_and_rotates_retained_history_impl(backend: MainTestBackend) {
    let source = build_test_state(1, false, backend).await;
    let target = build_test_state(1, false, backend).await;
    for index in 0..=crate::content_recovery::RETAINED_AUDIT_PRESENCE_CHECK_BATCH_SIZE {
        let key = format!("bounded-audit-{index}.bin");
        let version = format!("ver-bounded-audit-{index}");
        seed_subject_version(
            &source,
            &key,
            &version,
            format!("bounded audit bytes {index}").into_bytes(),
            vec![],
        )
        .await;
        let metadata = read_store(&source, "test.recovery.bounded_audit_metadata")
            .await
            .export_metadata_bundle(&key, None, ObjectReadMode::Preferred)
            .await
            .unwrap()
            .unwrap();
        lock_store(&target, "test.recovery.bounded_audit_import")
            .await
            .import_metadata_bundle(&metadata)
            .await
            .unwrap();
    }

    crate::content_recovery::audit_assigned(&target)
        .await
        .unwrap();
    assert_eq!(
        read_store(&target, "test.recovery.bounded_audit_first_pass")
            .await
            .content_repair_tasks()
            .await
            .unwrap()
            .len(),
        crate::content_recovery::RETAINED_AUDIT_PRESENCE_CHECK_BATCH_SIZE,
        "one audit pass must not inspect and enqueue an unbounded retained history"
    );

    crate::content_recovery::audit_assigned(&target)
        .await
        .unwrap();
    assert_eq!(
        read_store(&target, "test.recovery.bounded_audit_second_pass")
            .await
            .content_repair_tasks()
            .await
            .unwrap()
            .len(),
        crate::content_recovery::RETAINED_AUDIT_PRESENCE_CHECK_BATCH_SIZE + 1,
        "the next audit pass must rotate to retained manifests beyond its first batch"
    );

    cleanup_test_state(&source).await;
    cleanup_test_state(&target).await;
}

run_on_main_metadata_backends!(
    recovery_audit_bounds_and_rotates_retained_history_impl,
    recovery_audit_bounds_and_rotates_retained_history,
    recovery_audit_bounds_and_rotates_retained_history_turso
);

async fn recovery_audit_claims_complete_cached_assigned_content_impl(backend: MainTestBackend) {
    let source = build_test_state(1, false, backend).await;
    let target = build_test_state(1, false, backend).await;
    let (url, handle) = spawn_internal_peer_api_server(source.clone()).await;
    register_online_source_node(&target, &source, &url).await;
    let key = choose_locally_placed_key(&target, "cached-assigned-content").await;
    let version = "ver-cached-only";
    seed_subject_version(
        &source,
        &key,
        version,
        b"verified cache bytes".to_vec(),
        vec![],
    )
    .await;
    let metadata = read_store(&source, "test.recovery.cached_metadata")
        .await
        .export_metadata_bundle(&key, Some(version), ObjectReadMode::Preferred)
        .await
        .unwrap()
        .unwrap();
    lock_store(&target, "test.recovery.import_cached_metadata")
        .await
        .import_metadata_bundle(&metadata)
        .await
        .unwrap();
    let manifest = bundle(&source, &key, version).await;
    let cached = crate::content_recovery::recover_chunks(
        &target,
        &key,
        &manifest.manifest.chunks,
        None,
        true,
    )
    .await;
    assert!(cached.remaining.is_empty(), "{:?}", cached.errors);
    let store = read_store(&target, "test.recovery.cached_presence").await;
    assert!(
        store
            .manifest_is_fully_local(&manifest.manifest_hash)
            .await
            .unwrap()
    );
    assert!(
        !store
            .manifest_is_owned(&manifest.manifest_hash)
            .await
            .unwrap(),
        "read-through cache bytes must not be mistaken for replica ownership"
    );
    drop(store);

    crate::content_recovery::audit_assigned(&target)
        .await
        .unwrap();
    assert_eq!(
        read_store(&target, "test.recovery.cached_task")
            .await
            .content_repair_tasks()
            .await
            .unwrap()
            .len(),
        1,
        "assigned cache-only content must be promoted through a durable repair task"
    );
    handle.abort();
    let _ = handle.await;
    cleanup_test_state(&source).await;
    cleanup_test_state(&target).await;
}

run_on_main_metadata_backends!(
    recovery_audit_claims_complete_cached_assigned_content_impl,
    recovery_audit_claims_complete_cached_assigned_content,
    recovery_audit_claims_complete_cached_assigned_content_turso
);

async fn recovery_combines_partial_peers_with_stale_inventory_impl(backend: MainTestBackend) {
    let source_a = build_test_state(1, false, backend).await;
    let source_b = build_test_state(1, false, backend).await;
    let target = build_test_state(1, false, backend).await;
    let key = "recover/partial.bin";
    let version = "ver-partial";
    let mut payload = vec![1; 1024 * 1024];
    payload.extend(vec![2; 1024 * 1024]);
    for node in [&source_a, &source_b, &target] {
        seed_subject_version(node, key, version, payload.clone(), vec![]).await;
    }
    let manifest = bundle(&target, key, version).await;
    remove_chunks(&source_a, &manifest, &[1]).await;
    remove_chunks(&source_b, &manifest, &[0]).await;
    remove_chunks(&target, &manifest, &[0, 1]).await;
    let (url_a, handle_a) = spawn_internal_peer_api_server(source_a.clone()).await;
    let (url_b, handle_b) = spawn_internal_peer_api_server(source_b.clone()).await;
    register_online_source_node(&target, &source_a, &url_a).await;
    register_online_source_node(&target, &source_b, &url_b).await;
    // Only A is advertised, and that claim is stale; B is never advertised.
    note_subject_replicas(&target, source_a.node_id, key, &[version.to_string()]).await;
    let report = crate::replication::execute_targeted_replication_repair_inner(
        &target,
        vec![format!("{key}@{version}")],
        None,
    )
    .await;
    assert_eq!(report.successful_transfers, 1, "{report:?}");
    let restored = read_store(&target, "test.recovery.verify")
        .await
        .get_object(key, None, Some(version), ObjectReadMode::Preferred)
        .await
        .unwrap();
    assert_eq!(restored.as_ref(), payload);
    handle_a.abort();
    handle_b.abort();
    let _ = handle_a.await;
    let _ = handle_b.await;
    for node in [&source_a, &source_b, &target] {
        cleanup_test_state(node).await;
    }
}

run_on_main_metadata_backends!(
    recovery_combines_partial_peers_with_stale_inventory_impl,
    recovery_combines_partial_peers_with_stale_inventory,
    recovery_combines_partial_peers_with_stale_inventory_turso
);

async fn recovery_deleted_history_uses_real_availability_impl(backend: MainTestBackend) {
    let source = build_test_state(1, false, backend).await;
    let target = build_test_state(1, false, backend).await;
    let (url, handle) = spawn_internal_peer_api_server(source.clone()).await;
    register_online_source_node(&target, &source, &url).await;
    let key = choose_locally_placed_key(&target, "deleted-history").await;
    let version = "ver-retained-before-delete";
    let payload = b"historical bytes must survive deletion".to_vec();
    seed_subject_version(&source, &key, version, payload.clone(), vec![]).await;
    let (history, deletion) = {
        let mut store = lock_store(&source, "test.recovery.delete").await;
        let history = store
            .export_metadata_bundle(&key, Some(version), ObjectReadMode::Preferred)
            .await
            .unwrap()
            .unwrap();
        store
            .tombstone_object(&key, PutOptions::default())
            .await
            .unwrap();
        let deletion = store
            .export_metadata_bundle(&key, None, ObjectReadMode::Preferred)
            .await
            .unwrap()
            .unwrap();
        (history, deletion)
    };
    {
        let mut store = lock_store(&target, "test.recovery.metadata").await;
        store.import_metadata_bundle(&history).await.unwrap();
        store.import_metadata_bundle(&deletion).await.unwrap();
    }
    crate::refresh_local_availability_view_once(&source).await;
    crate::sync_availability_views_once(&target).await;
    assert!(
        target
            .cluster
            .lock()
            .await
            .available_nodes_for_subject(&format!("{key}@{version}"))
            .is_empty(),
        "non-head retained history must not expand the cluster availability view"
    );
    let baseline = repair_run_history(&target).await.len();
    assert!(
        crate::start_local_data_scrub(&target, crate::DataScrubRunTrigger::ManualRequest)
            .await
            .started
    );
    wait_for_data_scrub_completion(&target).await;
    wait_for_repair_history_len(&target, baseline + 1).await;
    let store = read_store(&target, "test.recovery.verify_deleted").await;
    let restored = store
        .get_object(&key, None, Some(version), ObjectReadMode::Preferred)
        .await
        .unwrap();
    assert_eq!(restored.as_ref(), payload);
    assert!(
        store
            .get_object(&key, None, None, ObjectReadMode::Preferred)
            .await
            .is_err()
    );
    let after = store
        .export_metadata_bundle(&key, None, ObjectReadMode::Preferred)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(after).unwrap(),
        serde_json::to_value(deletion).unwrap(),
        "repair must not resurrect or rewrite the namespace"
    );
    assert!(store.content_repair_tasks().await.unwrap().is_empty());
    drop(store);
    handle.abort();
    let _ = handle.await;
    cleanup_test_state(&source).await;
    cleanup_test_state(&target).await;
}

run_on_main_metadata_backends!(
    recovery_deleted_history_uses_real_availability_impl,
    recovery_deleted_history_uses_real_availability,
    recovery_deleted_history_uses_real_availability_turso
);

async fn recovery_snapshot_only_uses_hash_without_version_export_impl(backend: MainTestBackend) {
    let source = build_test_state(1, false, backend).await;
    let target = build_test_state(1, false, backend).await;
    let key = "snapshot-only.bin";
    let version = "ver-before-compaction";
    for node in [&source, &target] {
        seed_subject_version(node, key, version, b"snapshot bytes".to_vec(), vec![]).await;
    }
    let manifest = bundle(&target, key, version).await;
    for node in [&source, &target] {
        let mut store = lock_store(node, "test.recovery.compact_history").await;
        store
            .tombstone_object(key, PutOptions::default())
            .await
            .unwrap();
        store.compact_tombstone_indexes(0, false).await.unwrap();
        assert!(
            store
                .export_replication_bundle(key, Some(version), ObjectReadMode::Preferred)
                .await
                .ok()
                .flatten()
                .is_none(),
            "the exact version export must really be unavailable"
        );
        assert!(
            store.retained_content().await.unwrap().manifests[&manifest.manifest_hash]
                .values()
                .all(|r| r.snapshot_only)
        );
    }
    remove_chunks(&target, &manifest, &[0]).await;
    fs::remove_file(
        read_store(&target, "test.recovery.remove_manifest")
            .await
            .manifest_path_for_test(&manifest.manifest_hash),
    )
    .await
    .unwrap();
    let (url, handle) = spawn_internal_peer_api_server(source.clone()).await;
    register_online_source_node(&target, &source, &url).await;
    crate::refresh_local_availability_view_once(&source).await;
    crate::sync_availability_views_once(&target).await;
    assert!(
        !crate::planning_replication_subjects(&target)
            .await
            .iter()
            .any(|subject| subject.starts_with("cas-manifest:")),
        "hash-only obligations must not enter the legacy object-key planner"
    );
    let output = crate::content_recovery::scrubber(&target)
        .await
        .unwrap()
        .run_with_repair_subjects()
        .await
        .unwrap();
    assert!(
        output
            .repair_subjects
            .contains(&format!("cas-manifest:{}", manifest.manifest_hash))
    );
    let report = crate::replication::execute_targeted_replication_repair_inner(
        &target,
        output.repair_subjects.into_iter().collect(),
        None,
    )
    .await;
    assert_eq!(report.successful_transfers, 1, "{report:?}");
    let store = read_store(&target, "test.recovery.snapshot_result").await;
    assert_eq!(
        store
            .read_chunk_payload(&manifest.manifest.chunks[0].hash)
            .await
            .unwrap()
            .unwrap()
            .as_ref(),
        b"snapshot bytes"
    );
    assert!(
        store
            .get_object(key, None, None, ObjectReadMode::Preferred)
            .await
            .is_err()
    );
    assert!(
        store
            .export_replication_bundle(key, Some(version), ObjectReadMode::Preferred)
            .await
            .ok()
            .flatten()
            .is_none(),
        "byte repair must not recreate compacted version metadata"
    );
    drop(store);
    handle.abort();
    let _ = handle.await;
    cleanup_test_state(&source).await;
    cleanup_test_state(&target).await;
}

run_on_main_metadata_backends!(
    recovery_snapshot_only_uses_hash_without_version_export_impl,
    recovery_snapshot_only_uses_hash_without_version_export,
    recovery_snapshot_only_uses_hash_without_version_export_turso
);

async fn recovery_resumes_partial_work_after_restart_and_peer_reconnect_impl(
    backend: MainTestBackend,
) {
    let source_a = build_test_state(1, false, backend).await;
    let source_b = build_test_state(1, false, backend).await;
    let mut target = build_test_state(1, false, backend).await;
    target.repair_config.backoff_secs = 3600;
    let (url_a, handle_a) = spawn_internal_peer_api_server(source_a.clone()).await;
    register_online_source_node(&target, &source_a, &url_a).await;
    let key = choose_locally_placed_key(&target, "restart-recovery").await;
    let version = "ver-before-restart";
    let mut payload = vec![31; 1024 * 1024];
    payload.extend(vec![32; 1024 * 1024]);
    for node in [&source_a, &source_b] {
        seed_subject_version(node, &key, version, payload.clone(), vec![]).await;
    }
    let manifest = bundle(&source_a, &key, version).await;
    let metadata = read_store(&source_a, "test.recovery.metadata")
        .await
        .export_metadata_bundle(&key, Some(version), ObjectReadMode::Preferred)
        .await
        .unwrap()
        .unwrap();
    lock_store(&target, "test.recovery.import")
        .await
        .import_metadata_bundle(&metadata)
        .await
        .unwrap();
    remove_chunks(&source_a, &manifest, &[1]).await;
    remove_chunks(&source_b, &manifest, &[0]).await;
    let first = crate::execute_tracked_targeted_local_replication_repair(
        &target,
        vec![format!("{key}@{version}")],
        crate::RepairRunTrigger::DataScrubAutoRepair,
    )
    .await;
    assert_eq!(first.successful_transfers, 0);
    assert_eq!(
        first.run_status(),
        crate::RepairRunStatus::PartiallyRepaired
    );
    let root = {
        let store = read_store(&target, "test.recovery.partial").await;
        let pending = store.content_repair_tasks().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].next_attempt_unix > crate::unix_ts());
        assert_eq!(pending[0].recovered_chunks, 1);
        let mut waiting = pending[0].clone();
        waiting.attempts = 100; // A retry budget must not permanently abandon retained bytes.
        // A new source wakes a deferred task only after the topology-retry
        // floor, so a flapping peer cannot collapse its backoff every tick.
        waiting.last_attempt_unix = crate::unix_ts()
            .saturating_sub(crate::storage::CONTENT_REPAIR_SOURCE_CHANGE_MIN_RETRY_INTERVAL_SECS);
        store.persist_content_repair_task(&waiting).await.unwrap();
        store.cleanup_unreferenced(0, false).await.unwrap();
        assert!(
            store
                .read_chunk_payload(&manifest.manifest.chunks[0].hash)
                .await
                .unwrap()
                .is_some()
        );
        store.root_dir().to_path_buf()
    };
    let reopened = PersistentStore::init_with_metadata_backend(root, backend.kind())
        .await
        .unwrap();
    target.store = new_store_rwlock(reopened);
    target.maintenance.content_repair_claims = Arc::new(crate::ContentRepairClaims::default());
    target.maintenance.repair_state = Arc::new(Mutex::new(RepairExecutorState::default()));
    // No second scrub or manual repair request: a new source breaks the old backoff.
    let (url_b, handle_b) = spawn_internal_peer_api_server(source_b.clone()).await;
    register_online_source_node(&target, &source_b, &url_b).await;
    crate::content_recovery::resume_pending(&target)
        .await
        .unwrap();
    let store = read_store(&target, "test.recovery.after_restart").await;
    assert!(store.content_repair_tasks().await.unwrap().is_empty());
    assert_eq!(
        store
            .get_object(&key, None, Some(version), ObjectReadMode::Preferred)
            .await
            .unwrap()
            .as_ref(),
        payload
    );
    drop(store);
    let history = repair_run_history(&target).await;
    assert_eq!(
        history.first().unwrap().status,
        crate::RepairRunStatus::Completed
    );
    let recovered = history.first().unwrap().report.as_ref().unwrap()["detailed_log"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| entry["context"]["chunks_recovered"].as_u64())
        .sum::<u64>();
    assert_eq!(
        recovered, 1,
        "the previously persisted chunk must not be downloaded again"
    );
    handle_a.abort();
    handle_b.abort();
    let _ = handle_a.await;
    let _ = handle_b.await;
    for node in [&source_a, &source_b, &target] {
        cleanup_test_state(node).await;
    }
}

run_on_main_metadata_backends!(
    recovery_resumes_partial_work_after_restart_and_peer_reconnect_impl,
    recovery_resumes_partial_work_after_restart_and_peer_reconnect,
    recovery_resumes_partial_work_after_restart_and_peer_reconnect_turso
);

async fn recovery_budget_timeout_records_completed_chunks_impl(backend: MainTestBackend) {
    let source = build_test_state(1, false, backend).await;
    let mut target = build_test_state(1, false, backend).await;
    let key = "interrupted.bin";
    let version = "ver-interrupted";
    let first = vec![71; 1024 * 1024];
    let mut payload = first.clone();
    payload.extend(vec![72; 1024 * 1024]);
    for node in [&source, &target] {
        seed_subject_version(node, key, version, payload.clone(), vec![]).await;
    }
    let manifest = bundle(&target, key, version).await;
    remove_chunks(&target, &manifest, &[0, 1]).await;
    let first_hash = manifest.manifest.chunks[0].hash.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let unblock = Arc::new(tokio::sync::Notify::new());
    let wait = unblock.clone();
    let first_for_server = first_hash.clone();
    let app = axum::Router::new().route(
        "/cluster/v2/replication/chunk/{hash}",
        axum::routing::get(
            move |axum::extract::Path(hash): axum::extract::Path<String>| {
                let wait = wait.clone();
                let bytes = first.clone();
                let is_first = hash == first_for_server;
                async move {
                    if !is_first {
                        wait.notified().await;
                    }
                    bytes
                }
            },
        ),
    );
    let stalled_peer = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    register_online_source_node(&target, &source, &url).await;
    let reference = read_store(&target, "test.recovery.timeout_reference")
        .await
        .retained_content()
        .await
        .unwrap()
        .reference_for_subject(&format!("{key}@{version}"))
        .unwrap()
        .clone();
    let mut task = crate::storage::content_recovery::ContentRepairTask::new(reference, true);
    let error = crate::content_recovery::recover_task_with_budget(
        &target,
        &mut task,
        Duration::from_millis(500),
    )
    .await
    .unwrap_err();
    assert!(
        error.is::<crate::content_recovery::DurableRepairBudgetExceeded>(),
        "the artificial deadline must interrupt the second chunk transfer: {error:#}"
    );
    assert_eq!(task.recovered_chunks, 1);
    assert!(
        read_store(&target, "test.recovery.first_installed")
            .await
            .read_chunk_payload(&first_hash)
            .await
            .unwrap()
            .is_some()
    );
    task.defer("repair pass timed out".to_string(), 100, 30, true);
    assert_eq!(task.attempts, 0);
    assert_eq!(task.next_attempt_unix, 130);
    read_store(&target, "test.recovery.persist_timeout_progress")
        .await
        .persist_content_repair_task(&task)
        .await
        .unwrap();
    stalled_peer.abort();
    let _ = stalled_peer.await;
    let root = read_store(&target, "test.recovery.restart_root")
        .await
        .root_dir()
        .to_path_buf();
    target.store = new_store_rwlock(
        PersistentStore::init_with_metadata_backend(root, backend.kind())
            .await
            .unwrap(),
    );
    read_store(&target, "test.recovery.restart_gc")
        .await
        .cleanup_unreferenced(0, false)
        .await
        .unwrap();
    let (url, handle) = spawn_internal_peer_api_server(source.clone()).await;
    register_online_source_node(&target, &source, &url).await;
    crate::content_recovery::resume_pending(&target)
        .await
        .unwrap();
    let history = repair_run_history(&target).await;
    let report = history[0].report.as_ref().unwrap();
    assert!(
        report["detailed_log"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["event"] == "repair_verified"
                && entry["context"]["chunks_recovered"] == 1)
    );
    assert_eq!(
        read_store(&target, "test.recovery.final")
            .await
            .get_object(key, None, Some(version), ObjectReadMode::Preferred)
            .await
            .unwrap()
            .as_ref(),
        payload
    );
    handle.abort();
    let _ = handle.await;
    cleanup_test_state(&source).await;
    cleanup_test_state(&target).await;
}

run_on_main_metadata_backends!(
    recovery_budget_timeout_records_completed_chunks_impl,
    recovery_budget_timeout_records_completed_chunks,
    recovery_budget_timeout_records_completed_chunks_turso
);

async fn recovery_audit_does_not_fill_metadata_only_nodes_impl(backend: MainTestBackend) {
    let source = build_test_state(1, false, backend).await;
    let target = build_test_state(1, false, backend).await;
    let (url, handle) = spawn_internal_peer_api_server(source.clone()).await;
    register_online_source_node(&target, &source, &url).await;
    let key = (0..10000)
        .map(|n| format!("metadata-only/{n}"))
        .find(|key| {
            target
                .cluster
                .try_lock()
                .unwrap()
                .placement_for_key(key)
                .selected_nodes
                .contains(&source.node_id)
        })
        .unwrap();
    seed_subject_version(
        &source,
        &key,
        "ver-metadata",
        b"remote-only".to_vec(),
        vec![],
    )
    .await;
    let metadata = read_store(&source, "test.recovery.export_metadata")
        .await
        .export_metadata_bundle(&key, None, ObjectReadMode::Preferred)
        .await
        .unwrap()
        .unwrap();
    lock_store(&target, "test.recovery.import_metadata")
        .await
        .import_metadata_bundle(&metadata)
        .await
        .unwrap();
    let report = crate::content_recovery::scrubber(&target)
        .await
        .unwrap()
        .run_with_repair_subjects()
        .await
        .unwrap();
    assert_eq!(report.report.issue_count, 0);
    assert!(report.repair_subjects.is_empty());
    crate::content_recovery::audit_assigned(&target)
        .await
        .unwrap();
    crate::content_recovery::resume_pending(&target)
        .await
        .unwrap();
    let store = read_store(&target, "test.recovery.no_hydration").await;
    assert!(store.content_repair_tasks().await.unwrap().is_empty());
    assert!(
        store
            .list_locally_owned_manifests_for_test()
            .await
            .unwrap()
            .is_empty()
    );
    let hash = blake3::hash(b"remote-only").to_hex().to_string();
    assert!(store.read_chunk_payload(&hash).await.unwrap().is_none());
    drop(store);
    handle.abort();
    let _ = handle.await;
    cleanup_test_state(&source).await;
    cleanup_test_state(&target).await;
}

run_on_main_metadata_backends!(
    recovery_audit_does_not_fill_metadata_only_nodes_impl,
    recovery_audit_does_not_fill_metadata_only_nodes,
    recovery_audit_does_not_fill_metadata_only_nodes_turso
);

async fn serve_chunk_bytes(
    bytes: Vec<u8>,
    requests: Arc<AtomicUsize>,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new().route(
        "/cluster/v2/replication/chunk/{hash}",
        axum::routing::get(move || {
            requests.fetch_add(1, Ordering::SeqCst);
            let bytes = bytes.clone();
            async move { bytes }
        }),
    );
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (url, handle)
}

async fn recovery_rejects_bad_peer_bytes_and_deduplicates_fetches_impl(backend: MainTestBackend) {
    let target = build_test_state(1, false, backend).await;
    let bad = build_test_state(1, false, backend).await;
    let good = build_test_state(1, false, backend).await;
    let bytes = b"valid verified bytes".to_vec();
    let bad_requests = Arc::new(AtomicUsize::new(0));
    let good_requests = Arc::new(AtomicUsize::new(0));
    // Same-sized corruption exercises the hash check, not just the length check.
    let (bad_url, bad_handle) = serve_chunk_bytes(vec![0; bytes.len()], bad_requests.clone()).await;
    let (good_url, good_handle) = serve_chunk_bytes(bytes.clone(), good_requests.clone()).await;
    register_online_source_node(&target, &bad, &bad_url).await;
    register_online_source_node(&target, &good, &good_url).await;
    let preferred = target
        .cluster
        .lock()
        .await
        .list_nodes()
        .into_iter()
        .find(|node| node.node_id == bad.node_id)
        .unwrap();
    let chunk = crate::storage::ReplicationChunkInfo {
        hash: blake3::hash(&bytes).to_hex().to_string(),
        size_bytes: bytes.len(),
    };
    let result = crate::content_recovery::recover_chunks(
        &target,
        "uncatalogued/history",
        &[chunk.clone(), chunk.clone()],
        Some(&preferred),
        true,
    )
    .await;
    assert!(result.remaining.is_empty(), "{:?}", result.errors);
    assert_eq!(result.recovered, 1);
    assert_eq!(bad_requests.load(Ordering::SeqCst), 1);
    assert_eq!(good_requests.load(Ordering::SeqCst), 1);
    crate::hydrate_missing_chunks_for_object_read(
        &target,
        "uncatalogued/history",
        std::slice::from_ref(&chunk),
        crate::content_recovery::READ_THROUGH_RECOVERY_BUDGET,
    )
    .await
    .unwrap();
    assert_eq!(
        good_requests.load(Ordering::SeqCst),
        1,
        "verified local chunks are reused by read-through"
    );
    let store = read_store(&target, "test.recovery.cache_only").await;
    assert!(
        store
            .list_locally_owned_manifests_for_test()
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .read_chunk_payload(&chunk.hash)
            .await
            .unwrap()
            .unwrap()
            .as_ref(),
        bytes
    );
    drop(store);
    bad_handle.abort();
    good_handle.abort();
    let _ = bad_handle.await;
    let _ = good_handle.await;
    for node in [&target, &bad, &good] {
        cleanup_test_state(node).await;
    }
}

run_on_main_metadata_backends!(
    recovery_rejects_bad_peer_bytes_and_deduplicates_fetches_impl,
    recovery_rejects_bad_peer_bytes_and_deduplicates_fetches,
    recovery_rejects_bad_peer_bytes_and_deduplicates_fetches_turso
);
