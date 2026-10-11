use std::sync::{Arc, Mutex as StdMutex, MutexGuard};

use tokio::sync::Notify;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RepairExecutionPriority {
    Manual,
    Automatic,
}

#[derive(Debug, Default)]
struct RepairExecutionGateState {
    active: bool,
    manual_waiters: usize,
}

#[derive(Debug, Default)]
pub(super) struct RepairExecutionGate {
    state: StdMutex<RepairExecutionGateState>,
    changed: Notify,
}

impl RepairExecutionGate {
    pub(super) async fn acquire(
        self: &Arc<Self>,
        priority: RepairExecutionPriority,
    ) -> RepairExecutionPermit {
        let mut manual_waiter = (priority == RepairExecutionPriority::Manual)
            .then(|| ManualWaiter::register(Arc::clone(self)));

        loop {
            let changed = self.changed.notified();
            {
                let mut state = self.lock_state();
                let manual_has_priority = priority == RepairExecutionPriority::Manual;
                if !state.active && (manual_has_priority || state.manual_waiters == 0) {
                    state.active = true;
                    if let Some(waiter) = manual_waiter.as_mut() {
                        waiter.unregister_locked(&mut state);
                    }
                    return RepairExecutionPermit {
                        gate: Arc::clone(self),
                    };
                }
            }
            changed.await;
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, RepairExecutionGateState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[derive(Debug)]
struct ManualWaiter {
    gate: Arc<RepairExecutionGate>,
    registered: bool,
}

impl ManualWaiter {
    fn register(gate: Arc<RepairExecutionGate>) -> Self {
        gate.lock_state().manual_waiters += 1;
        Self {
            gate,
            registered: true,
        }
    }

    fn unregister_locked(&mut self, state: &mut RepairExecutionGateState) {
        if self.registered {
            state.manual_waiters = state.manual_waiters.saturating_sub(1);
            self.registered = false;
        }
    }
}

impl Drop for ManualWaiter {
    fn drop(&mut self) {
        if !self.registered {
            return;
        }
        {
            let mut state = self.gate.lock_state();
            state.manual_waiters = state.manual_waiters.saturating_sub(1);
            self.registered = false;
        }
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
