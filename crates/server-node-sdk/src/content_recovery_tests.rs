use super::*;
use crate::storage::ReplicationExportBundle;

async fn recovery_scrub_persists_intent_when_execution_is_disabled_impl(backend: MainTestBackend) {
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
        "scrub findings must be durable before completion, even when execution is disabled"
    );
    crate::refresh_local_availability_view_once(&target).await;
    assert!(
        crate::cached_local_cluster_available_subjects(&target)
            .await
            .is_empty(),
        "a refresh cannot undo the persistent integrity finding"
    );
    assert!(
        repair_run_history(&target).await.is_empty(),
        "disabled repair must not transfer content"
    );
    cleanup_test_state(&target).await;
}

run_on_main_metadata_backends!(
    recovery_scrub_persists_intent_when_execution_is_disabled_impl,
    recovery_scrub_persists_intent_when_execution_is_disabled,
    recovery_scrub_persists_intent_when_execution_is_disabled_turso
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
        crate::RepairRunStatus::WaitingForSource
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
            .iter()
            .any(|n| n.node_id == source.node_id)
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
                .iter()
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
    target.maintenance.content_repair_lock = Arc::new(Mutex::new(()));
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

async fn recovery_cancelled_during_transfer_resumes_verified_chunks_impl(backend: MainTestBackend) {
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
    let running_target = target.clone();
    let repair = tokio::spawn(async move {
        crate::replication::execute_targeted_replication_repair_inner(
            &running_target,
            vec![format!("{key}@{version}")],
            None,
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if read_store(&target, "test.recovery.first_installed")
                .await
                .read_chunk_payload(&first_hash)
                .await
                .unwrap()
                .is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        !repair.is_finished(),
        "interrupt while the second chunk request is still in flight"
    );
    repair.abort();
    assert!(repair.await.unwrap_err().is_cancelled());
    unblock.notify_waiters();
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
    recovery_cancelled_during_transfer_resumes_verified_chunks_impl,
    recovery_cancelled_during_transfer_resumes_verified_chunks,
    recovery_cancelled_during_transfer_resumes_verified_chunks_turso
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
    crate::hydrate_missing_chunks_for_range(
        &target,
        "uncatalogued/history",
        std::slice::from_ref(&chunk),
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
