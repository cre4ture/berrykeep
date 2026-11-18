mod store;
pub use store::*;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn store() -> (std::path::PathBuf, WorkStore) {
        let root = std::env::temp_dir().join(format!(
            "berrykeep-work-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = WorkStore::open(root.join("work.sqlite")).unwrap();
        (root, store)
    }
    fn op(key: &str, value: u64) -> PlannedWork {
        PlannedWork::new(key, key, json!({"revision": value}))
    }

    #[test]
    #[cfg(windows)]
    fn long_windows_journal_paths_support_owner_reopen_and_wal() {
        let (root, short_store) = store();
        drop(short_store);
        let mut parent = root;
        while parent.as_os_str().len() < 310 {
            parent = parent.join("long-journal-directory-with-unicode-ü-and-nested-profile-state");
        }
        let path = parent.join("sync-work.sqlite");
        assert!(path.as_os_str().len() > 260);
        let owner = WorkStore::open_for_executor(&path).unwrap();
        owner.enqueue("intent", &[op("a", 1)]).unwrap();
        let control = WorkStore::open(&path).unwrap();
        assert_eq!(control.status("intent").unwrap().pending, 1);
        assert_eq!(control.path(), path);
        let old = owner
            .claim_next("intent", now_ms(), 120_000)
            .unwrap()
            .unwrap();
        drop(owner);
        let recovered = WorkStore::open_for_executor(&path).unwrap();
        let replay = recovered
            .claim_next("intent", now_ms(), 120_000)
            .unwrap()
            .unwrap();
        assert!(
            !recovered
                .finish(&old, WorkResult::Complete, now_ms())
                .unwrap()
        );
        assert!(
            recovered
                .finish(&replay, WorkResult::Complete, now_ms())
                .unwrap()
        );
        assert_eq!(control.status("intent").unwrap().completed, 1);
    }

    #[test]
    fn conflict_reason_survives_reopen_for_explicit_controls() {
        let (root, store) = store();
        store.enqueue("remote", &[op("a", 1)]).unwrap();
        let lease = store
            .claim_next("remote", now_ms(), 120_000)
            .unwrap()
            .unwrap();
        store
            .finish(
                &lease,
                WorkResult::Conflict("expected r1, found r2".into()),
                now_ms(),
            )
            .unwrap();
        let reopened = WorkStore::open(root.join("work.sqlite")).unwrap();
        let conflicts = reopened.conflict_details("remote").unwrap();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].work, op("a", 1));
        assert_eq!(conflicts[0].reason, "expected r1, found r2");
    }

    #[test]
    fn executor_ownership_retains_live_claims_and_recovers_only_after_owner_stops() {
        let (root, control) = store();
        let path = root.join("work.sqlite");
        let owner = WorkStore::open_for_executor(&path).unwrap();
        owner.enqueue("intent", &[op("a", 1)]).unwrap();
        let old = owner
            .claim_next("intent", now_ms(), 120_000)
            .unwrap()
            .unwrap();
        let retained = owner.clone();
        drop(owner);
        assert!(
            WorkStore::open_for_executor(&path).is_err(),
            "cloned executor must retain OS ownership"
        );
        assert!(
            control
                .claim_next("intent", now_ms(), 120_000)
                .unwrap()
                .is_none(),
            "ordinary opens must not steal live claims"
        );
        drop(retained);
        let recovered = WorkStore::open_for_executor(&path).unwrap();
        let replay = recovered
            .claim_next("intent", now_ms(), 120_000)
            .unwrap()
            .unwrap();
        assert!(
            !recovered
                .finish(&old, WorkResult::Complete, now_ms())
                .unwrap()
        );
        assert!(!recovered.renew(&old, now_ms(), 120_000).unwrap());
        assert!(
            recovered
                .finish(&replay, WorkResult::Complete, now_ms())
                .unwrap()
        );
    }

    #[test]
    fn executor_abrupt_exit_fixture() {
        let Some(path) = std::env::var_os("BERRYKEEP_TEST_WORK_EXECUTOR_CRASH") else {
            return;
        };
        let store = WorkStore::open_for_executor(path).unwrap();
        store.enqueue("intent", &[op("crash", 7)]).unwrap();
        assert!(
            store
                .claim_next("intent", now_ms(), 120_000)
                .unwrap()
                .is_some()
        );
        if std::env::var_os("BERRYKEEP_TEST_WORK_EXECUTOR_HOLD").is_some() {
            println!("executor-owner-ready");
            std::io::Write::flush(&mut std::io::stdout()).unwrap();
            let mut release = [0];
            std::io::Read::read_exact(&mut std::io::stdin(), &mut release).unwrap();
        }
        // Exit without running Rust destructors, as when the provider is killed.
        std::process::exit(17);
    }

    #[test]
    fn live_executor_in_another_process_excludes_owners_but_not_controls() {
        use std::io::BufRead;
        let (root, control) = store();
        let path = root.join("work.sqlite");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "sync_work::tests::executor_abrupt_exit_fixture",
                "--nocapture",
            ])
            .env("BERRYKEEP_TEST_WORK_EXECUTOR_CRASH", &path)
            .env("BERRYKEEP_TEST_WORK_EXECUTOR_HOLD", "1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let output = std::io::BufReader::new(child.stdout.take().unwrap());
        assert!(
            output
                .lines()
                .map(Result::unwrap)
                .any(|line| line.contains("executor-owner-ready"))
        );
        let second_owner = WorkStore::open_for_executor(&path);
        let status_while_owned = control.status("intent");
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(
            second_owner.is_err(),
            "another process must retain exclusive execution"
        );
        assert_eq!(status_while_owned.unwrap().running, 1);
        let recovered = WorkStore::open_for_executor(path).unwrap();
        assert!(
            recovered
                .claim_next("intent", now_ms(), 120_000)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn executor_recovers_immediately_after_process_exit_without_destructors() {
        let (root, _) = store();
        let path = root.join("work.sqlite");
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "sync_work::tests::executor_abrupt_exit_fixture"])
            .env("BERRYKEEP_TEST_WORK_EXECUTOR_CRASH", &path)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(17));
        let recovered = WorkStore::open_for_executor(path).unwrap();
        let lease = recovered
            .claim_next("intent", now_ms(), 120_000)
            .unwrap()
            .unwrap();
        assert_eq!(lease.work.payload, json!({"revision":7}));
    }

    #[test]
    fn interrupted_work_waits_for_lease_then_replays_and_rejects_old_ack() {
        let (root, store) = store();
        store.enqueue("intent", &[op("a", 1)]).unwrap();
        let first = store.claim_next("intent", 100, 50).unwrap().unwrap();
        let reopened = WorkStore::open(root.join("work.sqlite")).unwrap();
        assert!(reopened.claim_next("intent", 120, 50).unwrap().is_none());
        let second = reopened.claim_next("intent", 150, 50).unwrap().unwrap();
        assert!(!store.finish(&first, WorkResult::Complete, 160).unwrap());
        assert!(reopened.finish(&second, WorkResult::Complete, 160).unwrap());
        assert!(reopened.claim_next("intent", 200, 50).unwrap().is_none());
    }
    #[test]
    fn newer_desired_state_cancels_stale_jobs_and_late_completion() {
        let (_, store) = store();
        store
            .reconcile("remote", &[op("a", 1), op("b", 1)])
            .unwrap();
        let old = store.claim_next("remote", 100, 50).unwrap().unwrap();
        store.reconcile("remote", &[op("a", 2)]).unwrap();
        assert!(
            store.claim_next("remote", 101, 50).unwrap().is_none(),
            "superseding work must retain the resource lease until the old side effect stops"
        );
        assert!(!store.finish(&old, WorkResult::Complete, 101).unwrap());
        let next = store.claim_next("remote", 102, 50).unwrap().unwrap();
        assert_eq!(next.work.payload, json!({"revision": 2}));
        store.finish(&next, WorkResult::Complete, 103).unwrap();
        assert!(store.claim_next("remote", 200, 50).unwrap().is_none());
    }
    #[test]
    fn failed_parent_blocks_descendants_but_not_other_branches() {
        let (_, store) = store();
        let mut child = op("a/child", 1);
        child.depends_on.push("a".into());
        store
            .reconcile("remote", &[op("a", 1), child, op("b", 1)])
            .unwrap();
        let parent = store.claim_next("remote", 100, 50).unwrap().unwrap();
        assert_eq!(parent.work.key, "a");
        store
            .finish(&parent, WorkResult::Retry("locked".into()), 100)
            .unwrap();
        let other = store.claim_next("remote", 101, 50).unwrap().unwrap();
        assert_eq!(other.work.key, "b");
        store.finish(&other, WorkResult::Complete, 101).unwrap();
        assert!(store.claim_next("remote", 102, 50).unwrap().is_none());
        let parent = store.claim_next("remote", 10_000, 50).unwrap().unwrap();
        store.finish(&parent, WorkResult::Complete, 10_000).unwrap();
        assert_eq!(
            store
                .claim_next("remote", 10_001, 50)
                .unwrap()
                .unwrap()
                .work
                .key,
            "a/child"
        );
    }
    #[test]
    fn unchanged_reconciliation_preserves_completion_and_conflicts() {
        let (_, store) = store();
        store.reconcile("remote", &[op("a", 1)]).unwrap();
        let job = store.claim_next("remote", 100, 50).unwrap().unwrap();
        store
            .finish(&job, WorkResult::Conflict("local edit".into()), 101)
            .unwrap();
        store.reconcile("remote", &[op("a", 1)]).unwrap();
        assert!(store.claim_next("remote", 200, 50).unwrap().is_none());
        assert_eq!(store.status("remote").unwrap().conflicts, 1);
    }

    #[test]
    fn separate_connections_claim_a_resource_only_once() {
        let (root, store) = store();
        store.enqueue("intent", &[op("a", 1)]).unwrap();
        let second = WorkStore::open(root.join("work.sqlite")).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let other_barrier = barrier.clone();
        let thread = std::thread::spawn(move || {
            other_barrier.wait();
            second.claim_next("intent", 100, 1000).unwrap().is_some()
        });
        barrier.wait();
        let first = store.claim_next("intent", 100, 1000).unwrap().is_some();
        assert_ne!(first, thread.join().unwrap());
    }

    #[test]
    fn observations_survive_restart_and_keep_identity_separate_from_availability() {
        let (root, store) = store();
        let state = json!({"object_id":"object-a", "revision":"r1", "availability":ContentAvailability::MetadataOnly,"dirty":false});
        store.observe("local", &state).unwrap();
        drop(store);
        let reopened = WorkStore::open(root.join("work.sqlite")).unwrap();
        assert_eq!(
            reopened.observation::<serde_json::Value>("local").unwrap(),
            Some(state)
        );
    }

    #[test]
    fn claims_use_due_index_and_do_not_scan_blocked_descendants() {
        let (_, store) = store();
        let mut jobs = vec![op("blocked", 1)];
        for index in 0..2000 {
            let mut child = op(&format!("blocked/{index:04}"), 1);
            child.depends_on.push("blocked".into());
            jobs.push(child);
        }
        for index in 0..100 {
            jobs.push(op(&format!("independent/{index:04}"), 1));
        }
        store.enqueue("test", &jobs).unwrap();
        assert_eq!(store.ready_count("test").unwrap(), 101);
        let parent = store.claim_next("test", 0, 100).unwrap().unwrap();
        assert_eq!(parent.work.key, "blocked");
        store
            .finish(&parent, WorkResult::Retry("offline".into()), 0)
            .unwrap();
        let plan = store.claim_query_plan().unwrap();
        assert!(
            plan.iter().any(|line| line.contains("sync_work_due")),
            "{plan:?}"
        );
        assert!(
            !plan.iter().any(|line| line.contains("TEMP B-TREE")),
            "{plan:?}"
        );
        for _ in 0..100 {
            let lease = store.claim_next("test", 1, 100).unwrap().unwrap();
            assert!(lease.work.key.starts_with("independent/"));
            store.finish(&lease, WorkResult::Complete, 1).unwrap();
        }
        assert_eq!(
            store.ready_count("test").unwrap(),
            1,
            "blocked children never enter the claim index"
        );
    }

    #[test]
    fn terminal_history_pruning_preserves_current_desired_completions() {
        let (_, store) = store();
        store
            .reconcile("desired", &[op("current", 1), op("removed", 1)])
            .unwrap();
        while let Some(lease) = store.claim_next("desired", 0, 100).unwrap() {
            store.finish(&lease, WorkResult::Complete, 1).unwrap();
        }
        store.reconcile("desired", &[op("current", 1)]).unwrap();
        store.enqueue("intent", &[op("historical", 1)]).unwrap();
        let lease = store.claim_next("intent", 0, 100).unwrap().unwrap();
        store.finish(&lease, WorkResult::Complete, 1).unwrap();
        assert_eq!(store.prune(i64::MAX / 2, 100).unwrap(), 2);
        assert_eq!(store.status("desired").unwrap().completed, 1);
        assert!(store.claim_next("desired", 100, 100).unwrap().is_none());
    }

    #[test]
    fn normal_enqueue_runs_due_terminal_retention_maintenance() {
        let (_, store) = store();
        store.enqueue("intent", &[op("old", 1)]).unwrap();
        let lease = store.claim_next("intent", 0, 100).unwrap().unwrap();
        store.finish(&lease, WorkResult::Complete, 1).unwrap();
        store.observe("scheduler-maintenance", &0_i64).unwrap();
        store.enqueue("intent", &[op("new", 1)]).unwrap();
        assert_eq!(store.status("intent").unwrap().completed, 0);
        assert_eq!(store.status("intent").unwrap().pending, 1);
        assert!(
            store
                .observation::<i64>("scheduler-maintenance")
                .unwrap()
                .unwrap()
                > 0
        );
    }
}
