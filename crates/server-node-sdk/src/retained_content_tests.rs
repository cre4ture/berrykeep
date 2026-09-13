use super::*;

async fn retained_content_source_presence_is_not_a_second_scrub_impl(backend: StorageTestBackend) {
    let (root, mut store) = backend.init_store("source-presence-cost").await;
    let put = store
        .put_object_versioned(
            "owned.bin",
            Bytes::from_static(b"known bytes"),
            PutOptions::default(),
        )
        .await
        .unwrap();
    fs::write(
        store.chunk_path_for_test(&hash_hex(b"known bytes")),
        b"other bytes",
    )
    .await
    .unwrap();
    // An audit uses the same cheap presence contract as availability. Only scrub
    // and repair completion rehash the payload; repeating that work on every
    // under-replication pass would read the whole store indefinitely.
    assert!(
        store
            .check_owned_replica_presence(&put.manifest_hash)
            .await
            .is_ok(),
        "routine source checks must inspect sizes, not rehash owned chunk payloads"
    );
    assert!(store.run_data_scrub().await.unwrap().issue_count > 0);
    let reference = store
        .retained_content()
        .await
        .unwrap()
        .reference_for_subject("owned.bin")
        .unwrap()
        .clone();
    let task = content_recovery::ContentRepairTask::new(reference, true);
    store.persist_content_repair_task(&task).await.unwrap();
    assert!(
        store
            .check_owned_replica_presence(&put.manifest_hash)
            .await
            .is_err(),
        "a pending integrity finding overrides matching sizes"
    );
    assert!(
        store.finish_content_repair(&task).await.is_err(),
        "completion must still verify the full content hash"
    );
    store
        .ingest_chunk(&hash_hex(b"known bytes"), b"known bytes")
        .await
        .unwrap();
    store.finish_content_repair(&task).await.unwrap();
    assert!(
        store
            .check_owned_replica_presence(&put.manifest_hash)
            .await
            .is_ok()
    );
    drop(store);
    fs::remove_dir_all(root).await.unwrap();
}

run_on_all_metadata_backends!(
    retained_content_source_presence_is_not_a_second_scrub_impl,
    retained_content_source_presence_is_not_a_second_scrub,
    retained_content_source_presence_is_not_a_second_scrub_turso
);

#[test]
fn retained_content_identity_does_not_require_a_path() {
    let reference = retained_content::RetainedReference {
        key: None,
        object_id: Some("legacy-object".to_string()),
        version_id: Some("legacy-version".to_string()),
        manifest_hash: hash_hex(b"immutable metadata reference"),
        snapshot_only: false,
    };
    assert_eq!(
        reference.subject(),
        Some(format!("cas-manifest:{}", reference.manifest_hash))
    );
}

async fn retained_content_advertises_history_after_delete_impl(backend: StorageTestBackend) {
    let (root, mut store) = backend.init_store("retained-availability").await;
    let key = "history/old-name.bin";
    let old = store
        .put_object_versioned(
            key,
            Bytes::from_static(b"retained bytes"),
            PutOptions::default(),
        )
        .await
        .unwrap();
    store
        .tombstone_object(key, PutOptions::default())
        .await
        .unwrap();
    let inspector = store.replication_subject_inspector().await.unwrap();
    let available = inspector.list_replication_subjects().await.unwrap();
    assert!(
        available.contains(&format!("{key}@{}", old.version_id)),
        "{available:?}"
    );
    assert!(!available.contains(&key.to_string()));
    assert_eq!(available, store.list_replication_subjects().await.unwrap());
    drop(store);
    fs::remove_dir_all(root).await.unwrap();
}

run_on_all_metadata_backends!(
    retained_content_advertises_history_after_delete_impl,
    retained_content_advertises_history_after_delete,
    retained_content_advertises_history_after_delete_turso
);

async fn retained_content_metadata_only_is_not_a_broken_replica_impl(backend: StorageTestBackend) {
    let (source_root, mut source) = backend.init_store("retained-metadata-source").await;
    let (target_root, mut target) = backend.init_store("retained-metadata-target").await;
    source
        .put_object_versioned(
            "metadata.bin",
            Bytes::from_static(b"remote bytes"),
            PutOptions::default(),
        )
        .await
        .unwrap();
    let bundle = source
        .export_metadata_bundle("metadata.bin", None, ObjectReadMode::Preferred)
        .await
        .unwrap()
        .unwrap();
    target.import_metadata_bundle(&bundle).await.unwrap();
    let report = target.run_data_scrub().await.unwrap();
    assert_eq!(
        report.issue_count, 0,
        "metadata visibility must not imply local ownership: {report:?}"
    );
    assert!(
        target
            .list_locally_owned_manifests_for_test()
            .await
            .unwrap()
            .is_empty()
    );
    assert!(target.list_replication_subjects().await.unwrap().is_empty());
    // Even a complete read-through cache is not an owned durability replica.
    target
        .ingest_chunk(&hash_hex(b"remote bytes"), b"remote bytes")
        .await
        .unwrap();
    assert!(target.list_replication_subjects().await.unwrap().is_empty());
    let hash = target
        .retained_content()
        .await
        .unwrap()
        .reference_for_subject("metadata.bin")
        .unwrap()
        .manifest_hash
        .clone();
    assert!(
        target.check_owned_replica_presence(&hash).await.is_err(),
        "a cheap replica presence check must not accept unowned cache content"
    );
    drop(source);
    drop(target);
    fs::remove_dir_all(source_root).await.unwrap();
    fs::remove_dir_all(target_root).await.unwrap();
}

run_on_all_metadata_backends!(
    retained_content_metadata_only_is_not_a_broken_replica_impl,
    retained_content_metadata_only_is_not_a_broken_replica,
    retained_content_metadata_only_is_not_a_broken_replica_turso
);

async fn retained_content_snapshot_only_is_scrubbed_impl(backend: StorageTestBackend) {
    let (root, mut store) = backend.init_store("retained-snapshot-only").await;
    let put = store
        .put_object_versioned(
            "old.bin",
            Bytes::from_static(b"snapshot-only"),
            PutOptions::default(),
        )
        .await
        .unwrap();
    persist_snapshot_fixture(&store, "retained-snapshot", 1).await;
    store
        .metadata_store
        .remove_current_object("old.bin")
        .await
        .unwrap();
    store
        .metadata_store
        .delete_version_index_by_object_id(&put.object_id)
        .await
        .unwrap();
    let hash = hash_hex(b"snapshot-only");
    fs::remove_file(store.chunk_path_for_test(&hash))
        .await
        .unwrap();
    let report = store.run_data_scrub().await.unwrap();
    assert_eq!(report.issue_count, 1, "{report:?}");
    assert_eq!(
        report.issues[0].manifest_hash.as_deref(),
        Some(put.manifest_hash.as_str())
    );
    assert_eq!(report.issues[0].chunk_hash.as_deref(), Some(hash.as_str()));
    let output = store
        .data_scrubber()
        .await
        .unwrap()
        .run_with_repair_subjects()
        .await
        .unwrap();
    assert!(
        output
            .repair_subjects
            .contains(&format!("cas-manifest:{}", put.manifest_hash))
    );
    let catalog = store.retained_content().await.unwrap();
    let reference = catalog
        .reference_for_subject(&format!("cas-manifest:{}", put.manifest_hash))
        .unwrap()
        .clone();
    let mut task = content_recovery::ContentRepairTask::new(reference, true);
    let bytes = store
        .read_recovery_manifest(&put.manifest_hash)
        .await
        .unwrap()
        .unwrap();
    task.chunks = content_recovery::validate_manifest(&put.manifest_hash, &bytes)
        .unwrap()
        .chunks;
    store.persist_content_repair_task(&task).await.unwrap();
    store.ingest_chunk(&hash, b"snapshot-only").await.unwrap();
    assert!(
        store.list_replication_subjects().await.unwrap().is_empty(),
        "pending content cannot be re-advertised before verification"
    );
    store.finish_content_repair(&task).await.unwrap();
    assert_eq!(
        store.list_replication_subjects().await.unwrap(),
        vec![format!("cas-manifest:{}", put.manifest_hash)]
    );
    assert!(
        store
            .metadata_store
            .load_current_state()
            .await
            .unwrap()
            .objects
            .is_empty(),
        "snapshot recovery must not recreate a live binding"
    );
    drop(store);
    fs::remove_dir_all(root).await.unwrap();
}

run_on_all_metadata_backends!(
    retained_content_snapshot_only_is_scrubbed_impl,
    retained_content_snapshot_only_is_scrubbed,
    retained_content_snapshot_only_is_scrubbed_turso
);

async fn retained_content_repair_pins_survive_restart_and_gc_impl(backend: StorageTestBackend) {
    let (source_root, mut source) = backend.init_store("recovery-pin-source").await;
    let (target_root, mut target) = backend.init_store("recovery-pin-target").await;
    source
        .put_object_versioned(
            "pin.bin",
            Bytes::from_static(b"pinned bytes"),
            PutOptions::default(),
        )
        .await
        .unwrap();
    let bundle = source
        .export_metadata_bundle("pin.bin", None, ObjectReadMode::Preferred)
        .await
        .unwrap()
        .unwrap();
    target.import_metadata_bundle(&bundle).await.unwrap();
    let retained = target.retained_content().await.unwrap();
    let reference = retained.reference_for_subject("pin.bin").unwrap().clone();
    let mut task = content_recovery::ContentRepairTask::new(reference, true);
    let bytes = target
        .read_recovery_manifest(&task.reference.manifest_hash)
        .await
        .unwrap()
        .unwrap();
    task.chunks = content_recovery::validate_manifest(&task.reference.manifest_hash, &bytes)
        .unwrap()
        .chunks;
    task.defer("peer offline".to_string(), unix_ts(), 1);
    target.persist_content_repair_task(&task).await.unwrap();
    let chunk_hash = hash_hex(b"pinned bytes");
    let (cleanup, install) = tokio::join!(
        target.cleanup_unreferenced(0, false),
        target.ingest_chunk(&chunk_hash, b"pinned bytes")
    );
    cleanup.unwrap();
    install.unwrap();
    drop(target);
    let target = backend.open_store(target_root.clone()).await;
    let tasks = target.content_repair_tasks().await.unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].attempts, 1);
    assert_eq!(tasks[0].last_error.as_deref(), Some("peer offline"));
    target.cleanup_unreferenced(0, false).await.unwrap();
    assert!(
        target
            .read_chunk_payload(&task.chunks[0].hash)
            .await
            .unwrap()
            .is_some()
    );
    target.finish_content_repair(&task).await.unwrap();
    assert!(target.content_repair_tasks().await.unwrap().is_empty());
    assert!(
        target
            .manifest_is_owned(&task.reference.manifest_hash)
            .await
            .unwrap()
    );
    target.cleanup_unreferenced(0, false).await.unwrap();
    assert!(
        target
            .read_chunk_payload(&task.chunks[0].hash)
            .await
            .unwrap()
            .is_some()
    );
    drop(source);
    drop(target);
    fs::remove_dir_all(source_root).await.unwrap();
    fs::remove_dir_all(target_root).await.unwrap();
}

run_on_all_metadata_backends!(
    retained_content_repair_pins_survive_restart_and_gc_impl,
    retained_content_repair_pins_survive_restart_and_gc,
    retained_content_repair_pins_survive_restart_and_gc_turso
);
