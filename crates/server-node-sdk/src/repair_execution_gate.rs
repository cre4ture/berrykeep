use std::collections::VecDeque;
use std::sync::{Arc, Mutex as StdMutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RepairExecutionPriority {
    Manual,
    Automatic,
}

#[derive(Debug, Default)]
struct RepairExecutionGateState {
    active: bool,
    next_waiter_id: u64,
    waiters: VecDeque<RepairExecutionWaiter>,
}

#[derive(Debug)]
struct RepairExecutionWaiter {
    id: u64,
    priority: RepairExecutionPriority,
    enqueued_at: Instant,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct RepairExecutionQueueSnapshot {
    pub(super) manual: usize,
    pub(super) automatic: usize,
}

const DEFAULT_AUTOMATIC_PRIORITY_AGING: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub(super) struct RepairExecutionGate {
    state: StdMutex<RepairExecutionGateState>,
    changed: Notify,
    automatic_priority_aging: Duration,
}

impl Default for RepairExecutionGate {
    fn default() -> Self {
        Self {
            state: StdMutex::new(RepairExecutionGateState::default()),
            changed: Notify::new(),
            automatic_priority_aging: DEFAULT_AUTOMATIC_PRIORITY_AGING,
        }
    }
}

impl RepairExecutionGate {
    pub(super) fn try_acquire(self: &Arc<Self>) -> Option<RepairExecutionPermit> {
        let mut state = self.lock_state();
        if state.active || !state.waiters.is_empty() {
            return None;
        }
        state.active = true;
        Some(RepairExecutionPermit {
            gate: Arc::clone(self),
        })
    }

    pub(super) async fn acquire(
        self: &Arc<Self>,
        priority: RepairExecutionPriority,
    ) -> RepairExecutionPermit {
        let mut waiter = RepairExecutionWaiterRegistration::register(Arc::clone(self), priority);

        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            // Register before inspecting the state. Otherwise a permit release
            // between the state check and the first poll could be lost because
            // `notify_waiters` does not retain a permit for future waiters.
            changed.as_mut().enable();
            {
                let mut state = self.lock_state();
                if !state.active
                    && state.selected_waiter_id(self.automatic_priority_aging) == Some(waiter.id)
                {
                    state.active = true;
                    waiter.unregister_locked(&mut state);
                    return RepairExecutionPermit {
                        gate: Arc::clone(self),
                    };
                }
            }
            changed.await;
        }
    }

    pub(super) fn queue_snapshot(&self) -> RepairExecutionQueueSnapshot {
        let state = self.lock_state();
        RepairExecutionQueueSnapshot {
            manual: state
                .waiters
                .iter()
                .filter(|waiter| waiter.priority == RepairExecutionPriority::Manual)
                .count(),
            automatic: state
                .waiters
                .iter()
                .filter(|waiter| waiter.priority == RepairExecutionPriority::Automatic)
                .count(),
        }
    }

    #[cfg(test)]
    fn with_automatic_priority_aging(automatic_priority_aging: Duration) -> Self {
        Self {
            automatic_priority_aging,
            ..Self::default()
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, RepairExecutionGateState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl RepairExecutionGateState {
    fn selected_waiter_id(&self, automatic_priority_aging: Duration) -> Option<u64> {
        self.waiters
            .iter()
            .find(|waiter| {
                waiter.priority == RepairExecutionPriority::Automatic
                    && waiter.enqueued_at.elapsed() >= automatic_priority_aging
            })
            .or_else(|| {
                self.waiters
                    .iter()
                    .find(|waiter| waiter.priority == RepairExecutionPriority::Manual)
            })
            .or_else(|| self.waiters.front())
            .map(|waiter| waiter.id)
    }
}

#[derive(Debug)]
struct RepairExecutionWaiterRegistration {
    gate: Arc<RepairExecutionGate>,
    id: u64,
    registered: bool,
}

impl RepairExecutionWaiterRegistration {
    fn register(gate: Arc<RepairExecutionGate>, priority: RepairExecutionPriority) -> Self {
        let id = {
            let mut state = gate.lock_state();
            let id = state.next_waiter_id;
            state.next_waiter_id = state.next_waiter_id.wrapping_add(1);
            state.waiters.push_back(RepairExecutionWaiter {
                id,
                priority,
                enqueued_at: Instant::now(),
            });
            id
        };
        Self {
            gate,
            id,
            registered: true,
        }
    }

    fn unregister_locked(&mut self, state: &mut RepairExecutionGateState) {
        if self.registered {
            state.waiters.retain(|waiter| waiter.id != self.id);
            self.registered = false;
        }
    }
}

impl Drop for RepairExecutionWaiterRegistration {
    fn drop(&mut self) {
        if !self.registered {
            return;
        }
        {
            let mut state = self.gate.lock_state();
            state.waiters.retain(|waiter| waiter.id != self.id);
        }
        self.registered = false;
        self.gate.changed.notify_waiters();
    }
}

#[derive(Debug)]
pub(super) struct RepairExecutionPermit {
    gate: Arc<RepairExecutionGate>,
}

impl Drop for RepairExecutionPermit {
    fn drop(&mut self) {
        {
            let mut state = self.gate.lock_state();
            debug_assert!(state.active, "repair execution permit released while idle");
            state.active = false;
        }
        self.gate.changed.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::{RepairExecutionGate, RepairExecutionPriority};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::Notify;

    #[tokio::test]
    async fn permits_exactly_one_active_execution() {
        let gate = Arc::new(RepairExecutionGate::default());
        let permit = gate.acquire(RepairExecutionPriority::Automatic).await;
        let second = tokio::spawn({
            let gate = Arc::clone(&gate);
            async move { gate.acquire(RepairExecutionPriority::Automatic).await }
        });

        tokio::task::yield_now().await;
        assert!(!second.is_finished());

        drop(permit);
        let second_permit = tokio::time::timeout(Duration::from_secs(1), second)
            .await
            .expect("second execution should acquire the released gate")
            .expect("second execution task should not panic");
        drop(second_permit);
    }

    #[tokio::test]
    async fn try_acquire_sheds_work_while_active_or_queued() {
        let gate = Arc::new(RepairExecutionGate::default());
        let active = gate
            .try_acquire()
            .expect("idle executor should admit immediate work");
        assert!(gate.try_acquire().is_none());

        let queued = tokio::spawn({
            let gate = Arc::clone(&gate);
            async move { gate.acquire(RepairExecutionPriority::Automatic).await }
        });
        tokio::task::yield_now().await;
        drop(active);

        assert!(
            gate.try_acquire().is_none(),
            "immediate work must not overtake an existing waiter"
        );
        drop(queued.await.expect("queued task should not panic"));
        assert!(gate.try_acquire().is_some());
    }

    #[tokio::test]
    async fn manual_waiter_runs_before_waiting_automatic_work() {
        let gate = Arc::new(RepairExecutionGate::default());
        let active = gate.acquire(RepairExecutionPriority::Automatic).await;
        let order = Arc::new(AtomicUsize::new(0));

        let automatic = tokio::spawn({
            let gate = Arc::clone(&gate);
            let order = Arc::clone(&order);
            async move {
                let permit = gate.acquire(RepairExecutionPriority::Automatic).await;
                let position = order.fetch_add(1, Ordering::SeqCst);
                drop(permit);
                position
            }
        });
        tokio::task::yield_now().await;
        let manual = tokio::spawn({
            let gate = Arc::clone(&gate);
            let order = Arc::clone(&order);
            async move {
                let permit = gate.acquire(RepairExecutionPriority::Manual).await;
                let position = order.fetch_add(1, Ordering::SeqCst);
                drop(permit);
                position
            }
        });
        tokio::task::yield_now().await;

        drop(active);
        assert_eq!(manual.await.expect("manual task should finish"), 0);
        assert_eq!(automatic.await.expect("automatic task should finish"), 1);
    }

    #[tokio::test]
    async fn cancelling_manual_waiter_unblocks_automatic_work() {
        let gate = Arc::new(RepairExecutionGate::default());
        let active = gate.acquire(RepairExecutionPriority::Automatic).await;
        let manual = tokio::spawn({
            let gate = Arc::clone(&gate);
            async move { gate.acquire(RepairExecutionPriority::Manual).await }
        });
        tokio::task::yield_now().await;
        let automatic = tokio::spawn({
            let gate = Arc::clone(&gate);
            async move { gate.acquire(RepairExecutionPriority::Automatic).await }
        });

        manual.abort();
        manual.await.expect_err("manual waiter should be cancelled");
        drop(active);

        let permit = tokio::time::timeout(Duration::from_secs(1), automatic)
            .await
            .expect("automatic waiter should no longer be priority-blocked")
            .expect("automatic task should not panic");
        drop(permit);
    }

    #[tokio::test]
    async fn queue_snapshot_counts_waiters_by_priority() {
        let gate = Arc::new(RepairExecutionGate::default());
        let active = gate.acquire(RepairExecutionPriority::Manual).await;
        let manual = tokio::spawn({
            let gate = Arc::clone(&gate);
            async move { gate.acquire(RepairExecutionPriority::Manual).await }
        });
        let automatic = tokio::spawn({
            let gate = Arc::clone(&gate);
            async move { gate.acquire(RepairExecutionPriority::Automatic).await }
        });
        tokio::task::yield_now().await;

        let snapshot = gate.queue_snapshot();
        assert_eq!(snapshot.manual, 1);
        assert_eq!(snapshot.automatic, 1);

        manual.abort();
        automatic.abort();
        drop(active);
    }

    #[tokio::test]
    async fn aged_automatic_waiter_runs_before_newer_manual_work() {
        let gate = Arc::new(RepairExecutionGate::with_automatic_priority_aging(
            Duration::ZERO,
        ));
        let active = gate.acquire(RepairExecutionPriority::Manual).await;
        let order = Arc::new(AtomicUsize::new(0));
        let automatic = tokio::spawn({
            let gate = Arc::clone(&gate);
            let order = Arc::clone(&order);
            async move {
                let permit = gate.acquire(RepairExecutionPriority::Automatic).await;
                let position = order.fetch_add(1, Ordering::SeqCst);
                drop(permit);
                position
            }
        });
        tokio::task::yield_now().await;
        let manual = tokio::spawn({
            let gate = Arc::clone(&gate);
            let order = Arc::clone(&order);
            async move {
                let permit = gate.acquire(RepairExecutionPriority::Manual).await;
                let position = order.fetch_add(1, Ordering::SeqCst);
                drop(permit);
                position
            }
        });
        tokio::task::yield_now().await;

        drop(active);
        assert_eq!(automatic.await.expect("automatic task should finish"), 0);
        assert_eq!(manual.await.expect("manual task should finish"), 1);
    }

    #[tokio::test]
    async fn dropping_permit_wakes_a_waiter() {
        let gate = Arc::new(RepairExecutionGate::default());
        let active = gate.acquire(RepairExecutionPriority::Manual).await;
        let acquired = Arc::new(Notify::new());
        let waiter = tokio::spawn({
            let gate = Arc::clone(&gate);
            let acquired = Arc::clone(&acquired);
            async move {
                let permit = gate.acquire(RepairExecutionPriority::Automatic).await;
                acquired.notify_one();
                permit
            }
        });

        drop(active);
        tokio::time::timeout(Duration::from_secs(1), acquired.notified())
            .await
            .expect("dropping a permit should wake a waiter");
        drop(waiter.await.expect("waiter should not panic"));
    }
}
