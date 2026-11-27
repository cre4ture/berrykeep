use super::*;

#[test]
fn queued_materialization_waits_for_later_local_intent_then_resumes_without_snapshot() {
    for changed_local_path in [false, true] {
        let (sync_root, provider) =
            super::super::tests::registered_monitor_test_sync_root("deferred-protected-work");
        let work = CfapiWork::new(
            sync_root.root_path.clone(),
            provider,
            WorkStore::open(":memory:").unwrap(),
            RemoteAppliedTracker::default(),
        )
        .unwrap();
        work.schedule(
            &CfapiActionPlan {
                actions: vec![file("a.txt"), file("other.txt")],
            },
            &BTreeMap::new(),
        )
        .unwrap();
        let planned = work.store.all_work(MATERIALIZATION_SCOPE).unwrap()[0]
            .payload
            .clone();
        work.enqueue_intent(&LocalMutation::Delete {
            path: if changed_local_path {
                "locally-renamed.txt"
            } else {
                "a.txt"
            }
            .into(),
            renamed_from: None,
            object_id: "object-a.txt".into(),
            revision: "r1".into(),
            is_dir: false,
        })
        .unwrap();
        work.drain_materialization().unwrap();
        assert_eq!(
            work.store.status(MATERIALIZATION_SCOPE).unwrap().pending,
            1,
            "a temporary local protection cannot acknowledge unexecuted desired work"
        );
        assert!(!sync_root.root_path.join("a.txt").exists());
        assert!(
            sync_root.root_path.join("other.txt").exists(),
            "protected sibling must not starve independent files"
        );
        work.store.cancel(LOCAL_SCOPE, "object-a.txt").unwrap();
        // Bring the backed-off batch forward without supplying another remote
        // plan; this is the same persisted desired operation after resolution.
        let retry = work
            .store
            .claim_next(MATERIALIZATION_SCOPE, now_ms() + 10_000, 120_000)
            .unwrap()
            .unwrap();
        assert_eq!(retry.work.payload, planned);
        work.store
            .finish(&retry, WorkResult::Retry("ready".into()), 0)
            .unwrap();
        work.drain_materialization().unwrap();
        assert_eq!(work.store.status(MATERIALIZATION_SCOPE).unwrap().pending, 0);
        let entry = snapshot_entry(
            &sync_root.root_path,
            "a.txt",
            &sync_root.root_path.join("a.txt"),
            false,
        );
        assert_eq!(entry.placeholder_object_id.as_deref(), Some("object-a.txt"));
        assert_eq!(entry.placeholder_state.unwrap().on_disk_data_size, 0);
    }
}

#[test]
fn terminal_upload_conflict_retains_typed_reason_without_rearming_on_local_edits() {
    struct ConflictingUploader(std::sync::atomic::AtomicUsize);
    impl Uploader for ConflictingUploader {
        fn upload_reader_for_object(
            &self,
            path: &str,
            _: Option<&str>,
            _: Option<&str>,
            reader: &mut dyn std::io::Read,
            length: u64,
        ) -> Result<crate::runtime::UploadReceipt> {
            self.upload_reader(path, reader, length)
        }
        fn upload_reader(
            &self,
            _: &str,
            _: &mut dyn std::io::Read,
            _: u64,
        ) -> Result<crate::runtime::UploadReceipt> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(client_sdk::ObjectMutationConflict::new("upload", "expected r1, remote r2").into())
        }
    }
    let work = CfapiWork::new(
        root(),
        uuid::Uuid::nil(),
        WorkStore::open(":memory:").unwrap(),
        RemoteAppliedTracker::default(),
    )
    .unwrap();
    let inner = Arc::new(ConflictingUploader(std::sync::atomic::AtomicUsize::new(0)));
    let uploader = JournaledUploader {
        work: work.clone(),
        inner: inner.clone(),
    };
    for length in [1, 2] {
        let error = uploader
            .upload_reader_for_object(
                "a.txt",
                Some("object"),
                Some("r1"),
                &mut std::io::Cursor::new(vec![0; length as usize]),
                length,
            )
            .unwrap_err();
        assert!(
            is_remote_mutation_conflict(&error),
            "terminal conflict must remain typed: {error:#}"
        );
        assert!(format!("{error:#}").contains("remote r2"));
    }
    assert_eq!(
        inner.0.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "editing local bytes does not authorize another stale CAS attempt"
    );
    let entries = CfapiWork::conflicts_in(&work.store).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].path, "a.txt");
    assert_eq!(entries[0].operation, "upload");
    assert!(entries[0].reason.contains("remote r2"));
}

#[test]
fn materialization_conflict_list_names_actual_failed_paths_in_a_partial_batch() {
    let store = WorkStore::open(":memory:").unwrap();
    let job = PlannedWork::new(
        "batch",
        "group",
        serde_json::to_value(Materialization {
            actions: vec![file("good.txt"), file("bad.txt")],
            availability: ContentAvailability::MetadataOnly,
        })
        .unwrap(),
    );
    store.enqueue(MATERIALIZATION_SCOPE, &[job]).unwrap();
    let lease = store
        .claim_next(MATERIALIZATION_SCOPE, now_ms(), 120_000)
        .unwrap()
        .unwrap();
    store.finish(&lease, WorkResult::Conflict(serde_json::json!([{"path":"bad.txt","operation":"materialize","reason":"local bytes changed"}]).to_string()), now_ms()).unwrap();
    let entries = CfapiWork::conflicts_in(&store).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].path, "bad.txt");
    assert_eq!(entries[0].reason, "local bytes changed");
}
#[derive(Default)]
struct RecordingUploader {
    deletes: Mutex<Vec<(String, String)>>,
    uploads: Mutex<usize>,
}
impl Uploader for RecordingUploader {
    fn upload_reader(
        &self,
        _path: &str,
        _reader: &mut dyn std::io::Read,
        _length: u64,
    ) -> Result<crate::runtime::UploadReceipt> {
        anyhow::bail!("unexpected path-only upload")
    }
    fn upload_reader_for_object(
        &self,
        _path: &str,
        _object_id: Option<&str>,
        _revision: Option<&str>,
        _reader: &mut dyn std::io::Read,
        _length: u64,
    ) -> Result<crate::runtime::UploadReceipt> {
        *self.uploads.lock().unwrap() += 1;
        Ok(crate::runtime::UploadReceipt {
            object_id: Some("test-object".into()),
            remote_version: Some("accepted-next".into()),
            in_sync_content_fingerprint: None,
        })
    }
    fn delete_object(&self, object_id: &str, revision: &str) -> Result<()> {
        self.deletes
            .lock()
            .unwrap()
            .push((object_id.into(), revision.into()));
        Ok(())
    }
}
fn root() -> PathBuf {
    let path = std::env::temp_dir().join(format!("berrykeep-cfapi-work-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&path).unwrap();
    path
}
fn file(path: &str) -> CfapiAction {
    CfapiAction::EnsurePlaceholder {
        object_id: Some(format!("object-{path}")),
        path: path.into(),
        remote_version: "r1".into(),
        remote_content_hash: "hash".into(),
        remote_size: Some(1),
        remote_content_fingerprint: None,
        remote_modified_at_unix: None,
        remote_media: None,
    }
}
#[test]
fn accepted_old_kind_receipt_preserves_replacement_and_accepted_delete_identity() {
    for old_directory in [false, true] {
        let (sync_root, provider) =
            super::super::tests::registered_monitor_test_sync_root("receipt-kind-replacement");
        if old_directory {
            std::fs::write(sync_root.root_path.join("entry"), b"new local file").unwrap();
        } else {
            std::fs::create_dir(sync_root.root_path.join("entry")).unwrap();
        }
        let work = CfapiWork::new(
            sync_root.root_path.clone(),
            provider,
            WorkStore::open(":memory:").unwrap(),
            RemoteAppliedTracker::default(),
        )
        .unwrap();
        work.record_accepted_receipt(
            if old_directory { "entry/" } else { "entry" },
            Some("old-object"),
            Some("r1"),
            &crate::runtime::UploadReceipt {
                object_id: Some("old-object".into()),
                remote_version: Some("accepted-r2".into()),
                in_sync_content_fingerprint: None,
            },
        )
        .unwrap();
        work.drain_upload_receipts().unwrap();
        let current = snapshot_entry(
            &sync_root.root_path,
            "entry",
            &sync_root.root_path.join("entry"),
            !old_directory,
        );
        assert!(
            current.placeholder_object_id.is_none(),
            "an old accepted receipt must not bind another kind: {}",
            current.to_log_string()
        );
        assert_eq!(
            work.prior()["entry"].revision.as_deref(),
            Some("accepted-r2"),
            "accepted remote identity remains available for guarded removal"
        );
        if old_directory {
            assert_eq!(
                std::fs::read(sync_root.root_path.join("entry")).unwrap(),
                b"new local file"
            );
        }
    }
}

#[test]
fn production_uploader_guards_kind_replacement_before_creating_new_object() {
    struct KindUploader {
        live: Mutex<BTreeSet<String>>,
        deleted: Mutex<Vec<String>>,
        uploaded: Mutex<Vec<String>>,
    }
    impl Uploader for KindUploader {
        fn upload_reader(
            &self,
            _: &str,
            _: &mut dyn std::io::Read,
            _: u64,
        ) -> Result<crate::runtime::UploadReceipt> {
            anyhow::bail!("unexpected unguarded upload")
        }
        fn upload_reader_for_object(
            &self,
            path: &str,
            id: Option<&str>,
            rev: Option<&str>,
            reader: &mut dyn std::io::Read,
            _: u64,
        ) -> Result<crate::runtime::UploadReceipt> {
            assert!(
                self.live.lock().unwrap().is_empty(),
                "new kind uploaded before old namespace was removed"
            );
            assert!(
                id.is_none() && rev.is_none(),
                "replacement must create its own identity"
            );
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes)?;
            self.uploaded.lock().unwrap().push(path.to_owned());
            Ok(crate::runtime::UploadReceipt {
                object_id: Some("replacement-object".into()),
                remote_version: Some("replacement-r1".into()),
                in_sync_content_fingerprint: None,
            })
        }
        fn delete_object(&self, id: &str, rev: &str) -> Result<()> {
            assert_eq!(rev, "r1");
            let mut live = self.live.lock().unwrap();
            if id == "old-directory" && live.iter().any(|id| id.ends_with("child")) {
                return Err(client_sdk::ObjectMutationConflict::new(
                    "delete",
                    "directory has a live child",
                )
                .into());
            }
            assert!(live.remove(id));
            self.deleted.lock().unwrap().push(id.to_owned());
            Ok(())
        }
    }
    for (old_directory, unseen_child) in [(false, false), (true, false), (true, true)] {
        let (sync_root, provider) =
            super::super::tests::registered_monitor_test_sync_root("wrapped-kind-replacement");
        let old_id = if old_directory {
            "old-directory"
        } else {
            "old-file"
        };
        let actions = if old_directory {
            vec![
                CfapiAction::EnsureDirectory {
                    object_id: Some(old_id.into()),
                    path: "entry".into(),
                    remote_version: Some("r1".into()),
                },
                CfapiAction::EnsurePlaceholder {
                    object_id: Some("known-child".into()),
                    path: "entry/child".into(),
                    remote_version: "r1".into(),
                    remote_content_hash: "hash".into(),
                    remote_size: Some(1),
                    remote_content_fingerprint: None,
                    remote_modified_at_unix: None,
                    remote_media: None,
                },
            ]
        } else {
            vec![CfapiAction::EnsurePlaceholder {
                object_id: Some(old_id.into()),
                path: "entry".into(),
                remote_version: "r1".into(),
                remote_content_hash: "hash".into(),
                remote_size: Some(1),
                remote_content_fingerprint: None,
                remote_modified_at_unix: None,
                remote_media: None,
            }]
        };
        crate::runtime::apply_action_plan(
            &sync_root.root_path,
            &CfapiActionPlan { actions },
            provider,
            true,
        )
        .unwrap();
        let mut live = BTreeSet::from([old_id.to_owned()]);
        if old_directory {
            live.insert("known-child".into());
        }
        if unseen_child {
            live.insert("unseen-child".into());
        }
        let inner = Arc::new(KindUploader {
            live: Mutex::new(live),
            deleted: Mutex::default(),
            uploaded: Mutex::default(),
        });
        let journal = WorkStore::open(root().join("work.sqlite")).unwrap();
        let mut monitor = SyncRootMonitor::from_store(
            "test",
            sync_root.root_path.clone(),
            provider,
            inner.clone(),
            journal.clone(),
        )
        .unwrap();
        monitor.uploader = Arc::new(JournaledUploader {
            inner: inner.clone(),
            work: monitor.work.clone(),
        });
        monitor.prepare_startup().unwrap();
        if old_directory {
            std::fs::remove_dir_all(sync_root.root_path.join("entry")).unwrap();
            std::fs::write(sync_root.root_path.join("entry"), b"replacement bytes").unwrap();
        } else {
            std::fs::remove_file(sync_root.root_path.join("entry")).unwrap();
            std::fs::create_dir(sync_root.root_path.join("entry")).unwrap();
        }
        let upload_path = if old_directory { "entry" } else { "entry/" };
        let payload: &[u8] = if old_directory {
            b"replacement bytes"
        } else {
            b"<DIR>"
        };
        assert!(
            monitor
                .uploader
                .upload_reader_for_object(
                    upload_path,
                    None,
                    None,
                    &mut std::io::Cursor::new(payload),
                    payload.len() as u64
                )
                .is_err()
        );
        assert!(
            monitor.work.pending_uploads().unwrap().contains("entry"),
            "callback before scan must retain replacement upload across restart"
        );
        drop(monitor);
        let mut reopened = SyncRootMonitor::from_store(
            "test",
            sync_root.root_path.clone(),
            provider,
            inner.clone(),
            journal,
        )
        .unwrap();
        reopened.uploader = Arc::new(JournaledUploader {
            inner: inner.clone(),
            work: reopened.work.clone(),
        });
        reopened.prepare_startup().unwrap();
        reopened.try_walk().unwrap();
        reopened.try_walk().unwrap();
        if unseen_child {
            assert!(inner.uploaded.lock().unwrap().is_empty());
            assert_eq!(reopened.work.status().unwrap().conflicts, 1);
            assert_eq!(
                std::fs::read(sync_root.root_path.join("entry")).unwrap(),
                payload
            );
            assert!(inner.live.lock().unwrap().contains("unseen-child"));
        } else {
            assert_eq!(*inner.uploaded.lock().unwrap(), vec![upload_path]);
            assert!(inner.live.lock().unwrap().is_empty());
            if old_directory {
                assert_eq!(
                    *inner.deleted.lock().unwrap(),
                    vec!["known-child", "old-directory"]
                );
            }
        }
    }
}

#[test]
fn deleting_pending_directory_rename_gates_marker_on_children_and_protects_original_ids() {
    let work = CfapiWork::new(
        root(),
        uuid::Uuid::nil(),
        WorkStore::open(":memory:").unwrap(),
        RemoteAppliedTracker::default(),
    )
    .unwrap();
    work.enqueue_intent(&LocalMutation::Rename {
        from: "a".into(),
        to: "b".into(),
        object_id: "first-parent".into(),
        revision: "r1".into(),
        is_dir: true,
    })
    .unwrap();
    work.reconcile_pending_renames(&HashMap::new()).unwrap();
    assert!(
        work.protected_paths().unwrap().contains("a"),
        "converted directory deletion retains original source-subtree protection"
    );
    // A child may already have its own post-rename observation. It must be
    // enqueued before replaying the parent's now-deleted destination.
    work.enqueue_deletes(&[LocalMutation::Delete {
        path: "b/child".into(),
        renamed_from: None,
        object_id: "z-child".into(),
        revision: "r2".into(),
        is_dir: false,
    }])
    .unwrap();
    let next = work
        .store
        .claim_next(LOCAL_SCOPE, now_ms(), 120_000)
        .unwrap()
        .unwrap();
    assert_eq!(next.work.key, "z-child");
    work.schedule(
        &CfapiActionPlan {
            actions: vec![CfapiAction::EnsureDirectory {
                object_id: Some("first-parent".into()),
                path: "a".into(),
                remote_version: Some("r1".into()),
            }],
        },
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(
        work.store.status(MATERIALIZATION_SCOPE).unwrap().pending,
        0,
        "pending deletion by ID protects the original source even after local target changed"
    );
}

#[test]
fn implicit_parents_exist_only_for_eligible_children() {
    let plan = CfapiActionPlan {
        actions: vec![
            CfapiAction::EnsureDirectory {
                object_id: None,
                path: "deleted-parent".into(),
                remote_version: None,
            },
            file("deleted-parent/a"),
            CfapiAction::EnsureDirectory {
                object_id: Some("empty-marker".into()),
                path: "explicit-empty".into(),
                remote_version: Some("r1".into()),
            },
            file("other/a"),
        ],
    };
    let jobs = materialization_plan(&plan, &BTreeSet::from(["deleted-parent/a".into()])).unwrap();
    assert!(
        !jobs.iter().any(|job| job.key == "directory:deleted-parent"),
        "pending child deletion must not recreate its empty implicit parent"
    );
    assert!(jobs.iter().any(|job| job.key == "directory:explicit-empty"));
    assert!(jobs.iter().any(|job| job.key == "directory:other"));
}

#[test]
fn idle_work_wait_has_no_deadline_and_scheduling_cannot_lose_wakeup() {
    let work = CfapiWork::new(
        root(),
        uuid::Uuid::nil(),
        WorkStore::open(":memory:").unwrap(),
        RemoteAppliedTracker::default(),
    )
    .unwrap();
    assert_eq!(work.next_execution_at().unwrap(), None);
    let epoch = work.wake_epoch();
    work.schedule(
        &CfapiActionPlan {
            actions: vec![file("wake.txt")],
        },
        &BTreeMap::new(),
    )
    .unwrap();
    let before = std::time::Instant::now();
    work.wait_for_work(epoch, std::time::Duration::from_secs(5));
    assert!(
        before.elapsed() < std::time::Duration::from_secs(1),
        "enqueue before wait must wake immediately"
    );
    assert!(work.next_execution_at().unwrap().is_some());
}

#[test]
fn large_tree_plan_is_bounded_and_parents_gate_children() {
    let mut actions = vec![CfapiAction::EnsureDirectory {
        object_id: None,
        path: "docs".into(),
        remote_version: None,
    }];
    for index in 0..1000 {
        actions.push(file(&format!("docs/file-{index:04}")));
    }
    let jobs = materialization_plan(
        &CfapiActionPlan { actions },
        &std::collections::BTreeSet::new(),
    )
    .unwrap();
    assert!(jobs.iter().all(|job| {
        serde_json::from_value::<Materialization>(job.payload.clone())
            .unwrap()
            .actions
            .len()
            <= 64
    }));
    let root = jobs.iter().find(|job| job.key == "directory:docs").unwrap();
    assert!(
        jobs.iter()
            .filter(|job| job.key != root.key)
            .all(|job| job.depends_on.contains(&root.key))
    );
}
#[test]
fn pending_local_directory_intent_suppresses_remote_descendants() {
    let jobs = materialization_plan(
        &CfapiActionPlan {
            actions: vec![file("docs/a"), file("other/b")],
        },
        &std::collections::BTreeSet::from(["docs".into()]),
    )
    .unwrap();
    let paths = jobs
        .iter()
        .flat_map(|job| {
            serde_json::from_value::<Materialization>(job.payload.clone())
                .unwrap()
                .actions
        })
        .map(|action| action_path(&action).to_string())
        .collect::<Vec<_>>();
    assert!(!paths.iter().any(|path| path.starts_with("docs")));
    assert!(paths.iter().any(|path| path == "other/b"));
}

#[test]
fn completed_delete_fence_survives_restart_and_rejects_pre_replay_snapshot() {
    let root = root();
    let journal = root.join("state/work.sqlite");
    let work = CfapiWork::new(
        root.clone(),
        uuid::Uuid::nil(),
        WorkStore::open(&journal).unwrap(),
        RemoteAppliedTracker::default(),
    )
    .unwrap();
    let intent = LocalMutation::Delete {
        path: "a.txt".into(),
        renamed_from: None,
        object_id: "object-a.txt".into(),
        revision: "r1".into(),
        is_dir: false,
    };
    work.enqueue_intent(&intent).unwrap();
    let uploader = RecordingUploader::default();
    work.drain_mutations(&uploader).unwrap();
    assert!(work.local_intents().unwrap().is_empty());
    let reopened = CfapiWork::new(
        root,
        uuid::Uuid::nil(),
        WorkStore::open(journal).unwrap(),
        RemoteAppliedTracker::default(),
    )
    .unwrap();
    reopened
        .schedule(
            &CfapiActionPlan {
                actions: vec![file("a.txt")],
            },
            &BTreeMap::new(),
        )
        .unwrap();
    assert_eq!(
        reopened.status().unwrap().pending,
        0,
        "a snapshot fetched before local delete replay must not recreate the old revision"
    );
    let mut restored = file("a.txt");
    if let CfapiAction::EnsurePlaceholder { remote_version, .. } = &mut restored {
        *remote_version = "restored-r2".into();
    }
    reopened
        .schedule(
            &CfapiActionPlan {
                actions: vec![restored],
            },
            &BTreeMap::new(),
        )
        .unwrap();
    assert_eq!(
        reopened.status().unwrap().pending,
        1,
        "a deliberate newer restore remains eligible"
    );
}

#[test]
fn accepted_receipt_is_not_erased_by_an_older_scan_and_drives_immediate_delete() {
    let root = root();
    let journal = root.join("state/work.sqlite");
    let work = CfapiWork::new(
        root.clone(),
        uuid::Uuid::nil(),
        WorkStore::open(&journal).unwrap(),
        RemoteAppliedTracker::default(),
    )
    .unwrap();
    work.record_receipt(
        "gone.txt",
        &crate::runtime::UploadReceipt {
            object_id: Some("uploaded-object".into()),
            remote_version: Some("uploaded-r1".into()),
            in_sync_content_fingerprint: None,
        },
    )
    .unwrap();
    work.persist_prior(&HashMap::new(), &HashMap::new())
        .unwrap();
    assert_eq!(
        work.prior()["gone.txt"].object_id.as_deref(),
        Some("uploaded-object")
    );
    let uploader = Arc::new(RecordingUploader::default());
    let mut monitor = SyncRootMonitor::from_store(
        "test",
        root,
        uuid::Uuid::nil(),
        uploader.clone(),
        WorkStore::open(journal).unwrap(),
    )
    .unwrap();
    monitor.prepare_startup().unwrap();
    assert_eq!(
        *uploader.deletes.lock().unwrap(),
        vec![("uploaded-object".into(), "uploaded-r1".into())]
    );
}

#[test]
fn accepted_upload_before_identity_repair_recovers_own_revision_after_restart() {
    let (sync_root, provider) =
        crate::monitor::tests::registered_monitor_test_sync_root("accepted-upload-recovery");
    let path = "a.txt";
    crate::monitor::tests::create_clean_provider_placeholder(&sync_root.root_path, provider, path);
    let initial = snapshot_entry_with_placeholder_state(
        &sync_root.root_path,
        path,
        &sync_root.root_path.join(path),
        false,
        Ok(CF_PLACEHOLDER_STATE_NO_STATES),
    );
    assert!(
        initial.is_placeholder,
        "initial={}",
        initial.to_log_string()
    );
    assert!(initial.placeholder_state.unwrap().is_partial);
    assert!(initial.materialized_file.is_none());
    let state = root().join("work.sqlite");
    let work = CfapiWork::new(
        sync_root.root_path.clone(),
        provider,
        WorkStore::open(&state).unwrap(),
        RemoteAppliedTracker::default(),
    )
    .unwrap();
    work.record_accepted_receipt(
        path,
        Some("test-object"),
        Some("newer-revision"),
        &crate::runtime::UploadReceipt {
            object_id: Some("test-object".into()),
            remote_version: Some("accepted-r2".into()),
            in_sync_content_fingerprint: None,
        },
    )
    .unwrap();
    let reopened = CfapiWork::new(
        sync_root.root_path.clone(),
        provider,
        WorkStore::open(state).unwrap(),
        RemoteAppliedTracker::default(),
    )
    .unwrap();
    reopened.recover_upload_observations().unwrap();
    reopened.drain_upload_receipts().unwrap();
    let entry = snapshot_entry(
        &sync_root.root_path,
        path,
        &sync_root.root_path.join(path),
        false,
    );
    assert_eq!(
        entry.placeholder_revision.as_deref(),
        Some("accepted-r2"),
        "state={} status={:?}",
        entry.to_log_string(),
        reopened.status().unwrap()
    );
    assert_eq!(
        entry.placeholder_state.unwrap().on_disk_data_size,
        0,
        "receipt repair must not hydrate a placeholder"
    );
    let inner = Arc::new(RecordingUploader::default());
    let uploader = JournaledUploader {
        inner: inner.clone(),
        work: reopened,
    };
    uploader
        .upload_reader_for_object(
            path,
            Some("test-object"),
            Some("accepted-r2"),
            &mut std::io::Cursor::new(Vec::<u8>::new()),
            0,
        )
        .unwrap();
    assert_eq!(
        *inner.uploads.lock().unwrap(),
        1,
        "own accepted revision must not permanently reject later local uploads"
    );
}

#[test]
fn accepted_receipt_repair_waits_through_close_upload_finalization() {
    use crate::close_upload::{
        UploadConcurrencyGate, UploadWorkerContext, process_close_upload_for_test,
    };
    use std::sync::mpsc;
    use std::time::Duration;

    struct UploadWithConcurrentRepair {
        work: CfapiWork,
        start: mpsc::Sender<()>,
        finished: Mutex<mpsc::Receiver<()>>,
    }
    impl Uploader for UploadWithConcurrentRepair {
        fn upload_reader(
            &self,
            path: &str,
            _reader: &mut dyn std::io::Read,
            _length: u64,
        ) -> Result<crate::runtime::UploadReceipt> {
            let receipt = crate::runtime::UploadReceipt {
                object_id: Some("test-object".into()),
                remote_version: Some("accepted-r2".into()),
                in_sync_content_fingerprint: None,
            };
            // Simulate the accepted receipt boundary while the close callback
            // still owns its pre-finalization snapshot and USN.
            self.work
                .record_accepted_receipt(path, None, None, &receipt)?;
            self.start.send(()).unwrap();
            assert!(
                matches!(
                    self.finished
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_millis(100)),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ),
                "receipt repair ran inside the close-upload finalization window"
            );
            Ok(receipt)
        }
    }

    let (sync_root, provider) =
        super::super::tests::registered_monitor_test_sync_root("serialized-receipt");
    std::fs::write(sync_root.root_path.join("a.txt"), b"local upload").unwrap();
    let work = CfapiWork::new(
        sync_root.root_path.clone(),
        provider,
        WorkStore::open(root().join("work.sqlite")).unwrap(),
        RemoteAppliedTracker::default(),
    )
    .unwrap();
    let (start, started) = mpsc::channel();
    let (done, finished) = mpsc::channel();
    let inner = Arc::new(UploadWithConcurrentRepair {
        work: work.clone(),
        start,
        finished: Mutex::new(finished),
    });
    let repair_work = work.clone();
    let repair = std::thread::spawn(move || {
        started.recv().unwrap();
        repair_work.drain_upload_receipts().unwrap();
        done.send(()).unwrap();
    });
    let worker = UploadWorkerContext {
        sync_root: sync_root.root_path.clone(),
        provider_instance_id: provider,
        runtime: Arc::new(crate::runtime::CfapiRuntime::default()),
        uploader: Arc::new(JournaledUploader {
            inner: inner.clone(),
            work,
        }),
        upload_gate: Arc::new(UploadConcurrencyGate::new(1)),
    };
    assert!(process_close_upload_for_test(&worker, "a.txt"));
    inner
        .finished
        .lock()
        .unwrap()
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    repair.join().unwrap();
    let info = crate::cfapi::cf_get_placeholder_standard_info(
        &std::fs::File::open(sync_root.root_path.join("a.txt")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        info.InSyncState,
        windows::Win32::Storage::CloudFilters::CF_IN_SYNC_STATE_IN_SYNC.0
    );
}

#[test]
fn interrupted_partial_placeholder_batch_resumes_without_another_snapshot() {
    let (sync_root, provider) =
        crate::monitor::tests::registered_monitor_test_sync_root("partial-batch-resume");
    let state = root().join("work.sqlite");
    let work = CfapiWork::new(
        sync_root.root_path.clone(),
        provider,
        WorkStore::open(&state).unwrap(),
        RemoteAppliedTracker::default(),
    )
    .unwrap();
    let actions = (0..100)
        .map(|index| file(&format!("docs/file-{index:03}")))
        .collect();
    work.schedule(&CfapiActionPlan { actions }, &BTreeMap::new())
        .unwrap();
    let directory = work
        .store
        .claim_next(MATERIALIZATION_SCOPE, 0, 1)
        .unwrap()
        .unwrap();
    let payload: Materialization = serde_json::from_value(directory.work.payload.clone()).unwrap();
    assert!(matches!(
        work.apply_materialization(&payload).unwrap(),
        WorkResult::Complete
    ));
    work.store
        .finish(&directory, WorkResult::Complete, 0)
        .unwrap();
    let batch = work
        .store
        .claim_next(MATERIALIZATION_SCOPE, 0, 1)
        .unwrap()
        .unwrap();
    let mut payload: Materialization = serde_json::from_value(batch.work.payload.clone()).unwrap();
    payload.actions.truncate(32);
    assert!(matches!(
        work.apply_materialization(&payload).unwrap(),
        WorkResult::Complete
    ));
    // No acknowledgement: a new process sees the partially applied running batch.
    let reopened = CfapiWork::new(
        sync_root.root_path.clone(),
        provider,
        WorkStore::open(state).unwrap(),
        RemoteAppliedTracker::default(),
    )
    .unwrap();
    reopened.drain_materialization().unwrap();
    assert_eq!(reopened.status().unwrap().pending, 0);
    for index in 0..100 {
        let path = format!("docs/file-{index:03}");
        let entry = snapshot_entry(
            &sync_root.root_path,
            &path,
            &sync_root.root_path.join(&path),
            false,
        );
        assert_eq!(
            entry.placeholder_object_id.as_deref(),
            Some(format!("object-{path}").as_str())
        );
        assert_eq!(entry.placeholder_state.unwrap().on_disk_data_size, 0);
    }
}
