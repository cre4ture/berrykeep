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
