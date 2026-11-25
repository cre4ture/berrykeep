use client_sdk::RemoteSnapshotUpdate;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use sync_core::SyncSnapshot;

/// A fetch may observe the gap between deleting an old kind and uploading its
/// replacement. Never apply that observation after accepting the local write.
#[derive(Clone, Default)]
pub(super) struct SnapshotFreshness(Arc<FreshnessState>);

#[derive(Default)]
struct FreshnessState {
    local_epoch: AtomicU64,
    fetch_epoch: AtomicU64,
    refresh_required: AtomicBool,
}

pub(super) struct RemoteObservation {
    pub snapshot: SyncSnapshot,
    pub changed_paths: Vec<String>,
    epoch: u64,
}

impl SnapshotFreshness {
    // The poller runs fetch and delivery sequentially on one worker.
    pub fn begin_fetch(&self) {
        self.0
            .fetch_epoch
            .store(self.0.local_epoch.load(Ordering::SeqCst), Ordering::SeqCst);
    }

    pub fn local_mutations_committed(&self) {
        self.0.local_epoch.fetch_add(1, Ordering::SeqCst);
        self.0.refresh_required.store(true, Ordering::SeqCst);
    }

    pub fn tag(&self, update: RemoteSnapshotUpdate) -> RemoteObservation {
        RemoteObservation {
            snapshot: update.snapshot,
            changed_paths: update.changed_paths,
            epoch: self.0.fetch_epoch.load(Ordering::SeqCst),
        }
    }

    pub fn needs_refresh(&self) -> bool {
        self.0.refresh_required.load(Ordering::SeqCst)
    }

    pub fn accept(
        &self,
        mut observation: RemoteObservation,
        deferred_paths: &mut BTreeSet<String>,
    ) -> Option<RemoteObservation> {
        if observation.epoch != self.0.local_epoch.load(Ordering::SeqCst) {
            // The poller has already advanced its comparison snapshot. Preserve
            // its delta so unrelated changes survive the forced fresh fetch.
            deferred_paths.extend(observation.changed_paths);
            self.0.refresh_required.store(true, Ordering::SeqCst);
            return None;
        }
        if !deferred_paths.is_empty() {
            deferred_paths.extend(observation.changed_paths);
            observation.changed_paths = std::mem::take(deferred_paths).into_iter().collect();
        }
        self.0.refresh_required.store(false, Ordering::SeqCst);
        Some(observation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use client_sdk::RemoteSnapshotUpdate;
    use std::collections::BTreeSet;
    use sync_core::{EntryKind, NamespaceEntry, SyncSnapshot};

    fn update(entries: Vec<NamespaceEntry>, paths: &[&str]) -> RemoteSnapshotUpdate {
        RemoteSnapshotUpdate {
            snapshot: SyncSnapshot {
                local: vec![],
                remote: entries,
            },
            changed_paths: paths.iter().map(|path| (*path).to_owned()).collect(),
            object_changes: vec![],
        }
    }

    #[test]
    fn stale_transition_snapshot_defers_until_fresh_even_when_refresh_is_unchanged() {
        let freshness = SnapshotFreshness::default();
        let mut deferred = BTreeSet::new();
        freshness.begin_fetch();
        // The fetch saw delete-old before create-new finished locally.
        freshness.local_mutations_committed();
        let old = freshness.tag(update(vec![], &["item", "unrelated"]));
        assert!(freshness.accept(old, &mut deferred).is_none());
        assert!(freshness.needs_refresh());
        freshness.begin_fetch();
        // A failed fetch produces no observation and must not consume the retry.
        assert!(freshness.needs_refresh());
        freshness.begin_fetch();
        let fresh = freshness.tag(update(vec![NamespaceEntry::directory("item")], &[]));
        let accepted = freshness.accept(fresh, &mut deferred).unwrap();
        assert_eq!(accepted.changed_paths, vec!["item", "unrelated"]);
        assert_eq!(accepted.snapshot.remote[0].kind, EntryKind::Directory);
        assert!(deferred.is_empty());
        assert!(!freshness.needs_refresh());
    }

    #[test]
    fn fetch_start_epoch_cannot_be_replaced_with_epoch_at_delivery() {
        let freshness = SnapshotFreshness::default();
        freshness.begin_fetch();
        freshness.local_mutations_committed();
        freshness.local_mutations_committed();
        let observation = freshness.tag(update(vec![], &["old"]));
        let mut deferred = BTreeSet::new();
        assert!(freshness.accept(observation, &mut deferred).is_none());
        freshness.begin_fetch();
        let observation = freshness.tag(update(vec![], &["new"]));
        assert_eq!(
            freshness
                .accept(observation, &mut deferred)
                .unwrap()
                .changed_paths,
            vec!["new", "old"]
        );
    }

    #[test]
    fn current_observation_needs_no_additional_refresh() {
        let freshness = SnapshotFreshness::default();
        freshness.begin_fetch();
        let observation = freshness.tag(update(vec![], &["item"]));
        assert!(
            freshness
                .accept(observation, &mut BTreeSet::new())
                .is_some()
        );
        assert!(!freshness.needs_refresh());
    }
}
