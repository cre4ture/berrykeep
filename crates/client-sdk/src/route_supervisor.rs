//! Stable route planning and the orthogonal state used by the client route supervisor.
//!
//! This module intentionally contains no transport I/O. The router owns route membership and
//! measurements, while [`RequestExecutor`] owns only a stable-ID request plan. Keeping those two
//! concerns separate lets Rendezvous reconciliation replace/reorder the registry without changing
//! the route represented by an in-flight plan.

use std::fmt;
use std::sync::{Arc, Mutex};

use crate::ironmesh_client::IronMeshClient;

/// Stable identity of a client transport route.
///
/// Numeric route indices are presentation-only and may change whenever discovery reconciles the
/// route registry. `RouteId` is backed by the existing canonical transport route key and is the
/// only identity used by foreground plans and route affinity.
#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct RouteId(String);

impl RouteId {
    pub(crate) fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

impl fmt::Display for RouteId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Whether a route has ever completed a real request or a health probe successfully.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum RouteValidationState {
    #[default]
    Probation,
    Validated,
}

/// Circuit-breaker admission is independent from route validation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum RouteCircuitState {
    #[default]
    Closed,
    OpenUntil(u64),
}

impl RouteCircuitState {
    pub(crate) fn open_until(self) -> Option<u64> {
        match self {
            Self::Closed => None,
            Self::OpenUntil(until_unix_ms) => Some(until_unix_ms),
        }
    }

    pub(crate) fn is_selectable_at(self, now_unix_ms: u64) -> bool {
        self.open_until()
            .is_none_or(|until_unix_ms| until_unix_ms <= now_unix_ms)
    }
}

/// Probe ownership is independent from validation and circuit state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum RouteProbeState {
    #[default]
    Idle,
    InFlight {
        started_unix_ms: u64,
    },
}

impl RouteProbeState {
    pub(crate) fn is_in_flight(self) -> bool {
        matches!(self, Self::InFlight { .. })
    }

    pub(crate) fn started_unix_ms(self) -> Option<u64> {
        match self {
            Self::Idle => None,
            Self::InFlight { started_unix_ms } => Some(started_unix_ms),
        }
    }
}

/// Admission observed while lazily resolving a request plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RouteAdmission {
    Available,
    CoolingUntil(u64),
    Missing,
}

/// Shared failover decision for every foreground transport operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttemptDisposition {
    Accept,
    RetryNextRoute,
    Stop,
}

/// Coalescing single-worker scheduler for synthetic route maintenance.
///
/// The channel carries only wake-ups, while the latest client and all explicit-refresh waiters
/// live in one shared pending slot. At most one wake-up can be queued while maintenance is running,
/// so foreground request bursts cannot grow an unbounded queue of client clones.
#[derive(Clone, Default)]
pub(crate) struct RouteSupervisor {
    sender: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<()>>>>,
    pending: Arc<Mutex<RouteSupervisorPending>>,
}

#[derive(Default)]
struct RouteSupervisorPending {
    client: Option<IronMeshClient>,
    completions: Vec<tokio::sync::oneshot::Sender<()>>,
    wake_queued: bool,
}

impl RouteSupervisor {
    pub(crate) fn signal(&self, client: IronMeshClient) -> bool {
        self.send(client, None)
    }

    pub(crate) async fn refresh_due(&self, client: IronMeshClient) {
        let (completion, receiver) = tokio::sync::oneshot::channel();
        if !self.send(client, Some(completion)) {
            return;
        }
        let _ = receiver.await;
    }

    fn send(
        &self,
        client: IronMeshClient,
        completion: Option<tokio::sync::oneshot::Sender<()>>,
    ) -> bool {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return false;
        };
        let mut sender = self
            .sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if sender
            .as_ref()
            .is_none_or(tokio::sync::mpsc::UnboundedSender::is_closed)
        {
            // A wake queued for a previous worker is no longer deliverable after that worker's
            // receiver closes. Preserve pending work, but allow the replacement worker to receive
            // a fresh wake-up.
            self.pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .wake_queued = false;
            let (new_sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<()>();
            let pending = Arc::clone(&self.pending);
            runtime.spawn(async move {
                while receiver.recv().await.is_some() {
                    let (worker_client, completions) = {
                        let mut pending = pending
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        pending.wake_queued = false;
                        (
                            pending.client.take(),
                            std::mem::take(&mut pending.completions),
                        )
                    };
                    if let Some(worker_client) = worker_client {
                        worker_client.run_due_route_maintenance().await;
                    }
                    for completion in completions {
                        let _ = completion.send(());
                    }
                }
            });
            *sender = Some(new_sender);
        }
        let wake_queued = {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            pending.client = Some(client);
            pending.completions.extend(completion);
            if pending.wake_queued {
                true
            } else {
                pending.wake_queued = true;
                false
            }
        };
        if wake_queued {
            return true;
        }
        if sender
            .as_ref()
            .is_some_and(|sender| sender.send(()).is_ok())
        {
            return true;
        }

        // A worker can disappear only after an unexpected task failure. Leave the supervisor
        // restartable and release explicit refresh waiters instead of stranding them forever.
        *sender = None;
        let completions = {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            pending.wake_queued = false;
            pending.client = None;
            std::mem::take(&mut pending.completions)
        };
        for completion in completions {
            let _ = completion.send(());
        }
        false
    }
}

/// Per-request failover cursor over stable route identities.
///
/// The ordering is captured at planning time, but admission is evaluated each time a candidate is
/// taken. A failure reported by another concurrent request therefore prevents a newly selected
/// attempt from entering the route's fresh circuit-breaker window. Already-running attempts are
/// deliberately unaffected.
#[derive(Clone, Debug)]
pub(crate) struct RequestExecutor {
    pending: Vec<RouteId>,
}

impl RequestExecutor {
    pub(crate) fn new(pending: Vec<RouteId>) -> Self {
        Self { pending }
    }

    pub(crate) fn classify_http_status(retryable_transport_status: bool) -> AttemptDisposition {
        if retryable_transport_status {
            AttemptDisposition::RetryNextRoute
        } else {
            AttemptDisposition::Accept
        }
    }

    pub(crate) fn classify_transport_error(
        rendezvous_backpressure: bool,
        request_body_reusable: bool,
    ) -> AttemptDisposition {
        if rendezvous_backpressure || !request_body_reusable {
            AttemptDisposition::Stop
        } else {
            AttemptDisposition::RetryNextRoute
        }
    }

    pub(crate) fn next_route(
        &mut self,
        mut admission: impl FnMut(&RouteId) -> RouteAdmission,
    ) -> Option<RouteId> {
        // Resolve admission exactly once per route. Apart from avoiding repeated registry
        // lookups, this prevents one selection pass from observing mutually inconsistent circuit
        // states while concurrent requests report outcomes.
        let mut candidates = std::mem::take(&mut self.pending)
            .into_iter()
            .filter_map(|route_id| {
                let admission = admission(&route_id);
                (admission != RouteAdmission::Missing).then_some((route_id, admission))
            })
            .collect::<Vec<_>>();
        let selected = candidates
            .iter()
            .position(|(_, admission)| *admission == RouteAdmission::Available)
            .or_else(|| {
                candidates
                    .iter()
                    .enumerate()
                    .filter_map(|(index, (_, admission))| match admission {
                        RouteAdmission::CoolingUntil(until_unix_ms) => Some((index, until_unix_ms)),
                        RouteAdmission::Available | RouteAdmission::Missing => None,
                    })
                    .min_by_key(|(_, until_unix_ms)| **until_unix_ms)
                    .map(|(index, _)| index)
            });
        let selected = selected.map(|index| candidates.remove(index).0);
        self.pending = candidates
            .into_iter()
            .map(|(route_id, _)| route_id)
            .collect();
        selected
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn executor_observes_admission_changes_after_plan_creation() {
        let primary = RouteId::new("primary");
        let backup = RouteId::new("backup");
        let mut executor = RequestExecutor::new(vec![primary.clone(), backup.clone()]);
        let admission = HashMap::from([
            (primary.clone(), RouteAdmission::CoolingUntil(42)),
            (backup.clone(), RouteAdmission::Available),
        ]);

        assert_eq!(
            executor.next_route(|route_id| admission[route_id]),
            Some(backup)
        );
        assert_eq!(
            executor.next_route(|route_id| admission[route_id]),
            Some(primary)
        );
    }

    #[test]
    fn executor_ignores_routes_removed_during_reconciliation() {
        let removed = RouteId::new("removed");
        let retained = RouteId::new("retained");
        let mut executor = RequestExecutor::new(vec![removed, retained.clone()]);

        assert_eq!(
            executor.next_route(|route_id| {
                if route_id == &retained {
                    RouteAdmission::Available
                } else {
                    RouteAdmission::Missing
                }
            }),
            Some(retained)
        );
    }

    #[test]
    fn executor_resolves_each_candidates_admission_once_per_selection() {
        let primary = RouteId::new("primary");
        let backup = RouteId::new("backup");
        let mut executor = RequestExecutor::new(vec![primary.clone(), backup.clone()]);
        let mut calls = HashMap::<RouteId, usize>::new();

        assert_eq!(
            executor.next_route(|route_id| {
                *calls.entry(route_id.clone()).or_default() += 1;
                if route_id == &primary {
                    RouteAdmission::CoolingUntil(42)
                } else {
                    RouteAdmission::Available
                }
            }),
            Some(backup.clone())
        );
        assert_eq!(calls, HashMap::from([(primary, 1), (backup, 1)]));
    }

    #[test]
    fn backpressure_and_consumed_streams_stop_route_fanout() {
        assert_eq!(
            RequestExecutor::classify_transport_error(true, true),
            AttemptDisposition::Stop
        );
        assert_eq!(
            RequestExecutor::classify_transport_error(false, false),
            AttemptDisposition::Stop
        );
        assert_eq!(
            RequestExecutor::classify_transport_error(false, true),
            AttemptDisposition::RetryNextRoute
        );
    }
}
