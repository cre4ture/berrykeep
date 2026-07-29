use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::{runtime::Runtime, sync::Mutex as AsyncMutex};

use crate::{
    ClientConnectionRouteSnapshot, ClientIdentityMaterial, ConnectionBootstrap, IronMeshClient,
    PlannedConnectionBootstrapTarget, build_client_with_optional_identity_from_planned_targets,
};

const REFRESH_COALESCE_WINDOW: Duration = Duration::from_secs(1);

/// Policy for shared, Rendezvous-managed client routes.
#[derive(Debug, Clone)]
pub struct ManagedClientOptions {
    /// Maximum foreground wait for the first advisory discovery pass.
    pub initial_discovery_timeout: Duration,
    /// A stale lifecycle hint is ignored until this interval has elapsed after a
    /// successful discovery pass.
    pub discovery_ttl: Duration,
    /// Time an absent dynamic candidate remains selectable after a successful
    /// discovery response omits it. Static bootstrap targets are never retired.
    pub route_retirement_grace: Duration,
    /// Lets a caller forward an all-route transport failure without duplicating
    /// discovery or fallback policy in a platform wrapper.
    pub refresh_on_transport_failure: bool,
}

impl Default for ManagedClientOptions {
    fn default() -> Self {
        Self {
            initial_discovery_timeout: Duration::from_secs(2),
            discovery_ttl: Duration::from_secs(5 * 60),
            route_retirement_grace: Duration::from_secs(10 * 60),
            refresh_on_transport_failure: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RouteRefreshReason {
    Startup,
    Stale,
    NetworkChanged,
    TransportFailure,
    Foregrounded,
    ExplicitDiagnosticRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RouteDiscoveryError {
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct RouteRefreshOutcome {
    pub discovery_used: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery_error: Option<RouteDiscoveryError>,
    pub routes_added: usize,
    pub routes_removed: usize,
    pub routes_retained: usize,
    #[serde(default)]
    pub identity_updated: bool,
}

#[derive(Clone)]
pub struct ManagedIronMeshClient {
    client: IronMeshClient,
    controller: Arc<ManagedRouteController>,
}

struct ManagedRouteController {
    client: IronMeshClient,
    bootstrap: ConnectionBootstrap,
    identity: Mutex<Option<ClientIdentityMaterial>>,
    options: ManagedClientOptions,
    refresh_lock: AsyncMutex<()>,
    routes: Mutex<ManagedRouteState>,
    last_attempt: Mutex<Option<Instant>>,
    last_success: Mutex<Option<Instant>>,
    last_outcome: Mutex<RouteRefreshOutcome>,
    pending_identity_update: Mutex<Option<ClientIdentityMaterial>>,
    /// Present for blocking consumers such as desktop daemons. It keeps the
    /// executor used for initial discovery and later refreshes alive for the
    /// lifetime of the managed client.
    runtime_guard: Mutex<Option<Arc<Runtime>>>,
}

#[derive(Default)]
struct ManagedRouteState {
    dynamic_targets: BTreeMap<String, ManagedDynamicRoute>,
}

struct ManagedDynamicRoute {
    target: PlannedConnectionBootstrapTarget,
    last_seen: Instant,
}

impl ConnectionBootstrap {
    /// Builds a stable client handle from static bootstrap routes, then performs
    /// a bounded, fail-open Rendezvous refresh. Enrolled clients can adopt
    /// discovered Direct QUIC routes; anonymous clients continue to use only
    /// direct HTTPS routes.
    pub async fn build_managed_client(
        &self,
        identity: Option<ClientIdentityMaterial>,
        options: ManagedClientOptions,
    ) -> Result<ManagedIronMeshClient> {
        self.validate()?;
        if let Some(identity) = identity.as_ref() {
            identity.validate()?;
            if identity.cluster_id != self.cluster_id {
                anyhow::bail!(
                    "client identity cluster_id {} does not match bootstrap cluster_id {}",
                    identity.cluster_id,
                    self.cluster_id
                );
            }
        }
        let client = self.build_client_with_optional_identity(identity.as_ref())?;
        let controller = Arc::new(ManagedRouteController {
            client: client.clone(),
            bootstrap: self.clone(),
            identity: Mutex::new(identity),
            options: options.clone(),
            refresh_lock: AsyncMutex::new(()),
            routes: Mutex::new(ManagedRouteState::default()),
            last_attempt: Mutex::new(None),
            last_success: Mutex::new(None),
            last_outcome: Mutex::new(RouteRefreshOutcome::default()),
            pending_identity_update: Mutex::new(None),
            runtime_guard: Mutex::new(None),
        });
        let managed = ManagedIronMeshClient {
            client,
            controller: controller.clone(),
        };
        let weak_controller = Arc::downgrade(&controller);
        managed
            .client
            .set_transport_failure_refresh_observer(Some(Arc::new(move || {
                let Some(controller) = weak_controller.upgrade() else {
                    return;
                };
                if controller.options.refresh_on_transport_failure {
                    controller.schedule_refresh(RouteRefreshReason::TransportFailure);
                }
            })));

        let initial_refresh = managed.clone();
        if tokio::time::timeout(options.initial_discovery_timeout, async move {
            initial_refresh
                .refresh_routes(RouteRefreshReason::Startup)
                .await
        })
        .await
        .is_err()
        {
            // The caller already has a usable static client. Continue discovery
            // opportunistically instead of extending foreground startup.
            managed.schedule_refresh(RouteRefreshReason::Startup);
        }

        Ok(managed)
    }

    /// Builds a stable client handle from static bootstrap routes, then performs a
    /// bounded, fail-open Rendezvous refresh. A failed or slow discovery response
    /// never invalidates the static direct HTTPS or relay routes.
    pub async fn build_managed_client_with_identity(
        &self,
        identity: ClientIdentityMaterial,
        options: ManagedClientOptions,
    ) -> Result<ManagedIronMeshClient> {
        self.build_managed_client(Some(identity), options).await
    }

    /// Synchronous counterpart for desktop daemons and shell integrations that
    /// do not otherwise own a Tokio runtime. The returned managed client keeps
    /// its runtime alive, so transport-failure and network-change refreshes use
    /// the same shared route controller as mobile clients.
    pub fn build_managed_client_blocking(
        &self,
        identity: Option<ClientIdentityMaterial>,
        options: ManagedClientOptions,
    ) -> Result<ManagedIronMeshClient> {
        let runtime = blocking_managed_client_runtime();
        let managed = runtime.block_on(self.build_managed_client(identity, options))?;
        managed.attach_runtime(runtime);
        Ok(managed)
    }
}

impl ManagedIronMeshClient {
    pub fn client(&self) -> IronMeshClient {
        self.client.clone()
    }

    pub fn route_snapshot(&self) -> ClientConnectionRouteSnapshot {
        self.client.connection_route_snapshot()
    }

    /// Schedules a coalesced refresh from a platform connectivity callback. The
    /// platform passes only the hint; candidate parsing and route selection stay in
    /// the shared Rust controller.
    pub fn notify_network_changed(&self) {
        self.schedule_refresh(RouteRefreshReason::NetworkChanged);
    }

    pub async fn notify_network_changed_async(&self) -> RouteRefreshOutcome {
        self.refresh_routes(RouteRefreshReason::NetworkChanged)
            .await
    }

    pub async fn notify_foregrounded(&self) -> RouteRefreshOutcome {
        self.refresh_routes(RouteRefreshReason::Foregrounded).await
    }

    pub async fn notify_transport_failure(&self) -> RouteRefreshOutcome {
        if self.controller.options.refresh_on_transport_failure {
            self.refresh_routes(RouteRefreshReason::TransportFailure)
                .await
        } else {
            self.last_outcome()
        }
    }

    /// Returns and clears a renewed Rendezvous identity. FFI owners persist this
    /// value in their existing secure store, while the controller keeps using it in
    /// memory even when persistence fails.
    pub fn take_identity_update(&self) -> Option<ClientIdentityMaterial> {
        self.controller
            .pending_identity_update
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    /// Reads a renewed identity without acknowledging it. This is useful for an
    /// FFI handle that needs to expose the update to its platform owner later.
    pub fn latest_identity_update(&self) -> Option<ClientIdentityMaterial> {
        self.controller
            .pending_identity_update
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub async fn refresh_routes(&self, reason: RouteRefreshReason) -> RouteRefreshOutcome {
        if matches!(reason, RouteRefreshReason::Stale) && !self.discovery_is_stale() {
            return self.last_outcome();
        }

        let _refresh_guard = self.controller.refresh_lock.lock().await;
        if self.recently_attempted() {
            return self.last_outcome();
        }
        *self
            .controller
            .last_attempt
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Instant::now());

        let mut identity = self
            .controller
            .identity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let identity_updated = if let Some(current_identity) = identity.clone() {
            let bootstrap = self.controller.bootstrap.clone();
            let original_identity = current_identity.clone();
            let renewed = tokio::task::spawn_blocking(move || {
                let mut identity = current_identity;
                bootstrap
                    .renew_rendezvous_identity_if_needed(&mut identity)
                    .map(|updated| (identity, updated))
            })
            .await;

            match renewed {
                Ok(Ok((renewed_identity, updated))) => {
                    let identity_updated = updated && renewed_identity != original_identity;
                    identity = Some(renewed_identity);
                    if identity_updated {
                        *self
                            .controller
                            .identity
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) = identity.clone();
                        *self
                            .controller
                            .pending_identity_update
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) = identity.clone();
                    }
                    identity_updated
                }
                Ok(Err(error)) => {
                    // Renewal is advisory for route discovery. Preserve static routes and
                    // report the structured failure rather than discarding a valid client.
                    return self.record_outcome(RouteRefreshOutcome {
                        discovery_used: false,
                        discovery_error: Some(RouteDiscoveryError {
                            message: format!("failed renewing Rendezvous identity: {error:#}"),
                        }),
                        identity_updated: false,
                        ..RouteRefreshOutcome::default()
                    });
                }
                Err(error) => {
                    return self.record_outcome(RouteRefreshOutcome {
                        discovery_used: false,
                        discovery_error: Some(RouteDiscoveryError {
                            message: format!("Rendezvous identity renewal task failed: {error}"),
                        }),
                        identity_updated: false,
                        ..RouteRefreshOutcome::default()
                    });
                }
            }
        } else {
            false
        };

        let refreshed_targets = match self
            .controller
            .bootstrap
            .refresh_dynamic_targets(identity.as_ref())
            .await
        {
            Ok(targets) => targets,
            Err(error) => {
                return self.record_outcome(RouteRefreshOutcome {
                    discovery_used: false,
                    discovery_error: Some(RouteDiscoveryError {
                        message: format!("Rendezvous discovery failed: {error:#}"),
                    }),
                    identity_updated,
                    ..RouteRefreshOutcome::default()
                });
            }
        };

        let desired_targets = self.reconcile_target_set(refreshed_targets);
        let identity_for_build = identity.clone();
        let refreshed_client = match tokio::task::spawn_blocking(move || {
            build_client_with_optional_identity_from_planned_targets(
                &desired_targets,
                identity_for_build.as_ref(),
            )
        })
        .await
        {
            Ok(Ok(client)) => client,
            Ok(Err(error)) => {
                return self.record_outcome(RouteRefreshOutcome {
                    discovery_used: true,
                    discovery_error: Some(RouteDiscoveryError {
                        message: format!("failed building refreshed client routes: {error:#}"),
                    }),
                    identity_updated,
                    ..RouteRefreshOutcome::default()
                });
            }
            Err(error) => {
                return self.record_outcome(RouteRefreshOutcome {
                    discovery_used: true,
                    discovery_error: Some(RouteDiscoveryError {
                        message: format!("route construction task failed: {error}"),
                    }),
                    identity_updated,
                    ..RouteRefreshOutcome::default()
                });
            }
        };

        let (routes_added, routes_removed, routes_retained) = self
            .client
            .reconcile_transport_membership(&refreshed_client, !identity_updated);
        *self
            .controller
            .last_success
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Instant::now());
        self.record_outcome(RouteRefreshOutcome {
            discovery_used: true,
            discovery_error: None,
            routes_added,
            routes_removed,
            routes_retained,
            identity_updated,
        })
    }

    fn reconcile_target_set(
        &self,
        refreshed_targets: Vec<PlannedConnectionBootstrapTarget>,
    ) -> Vec<PlannedConnectionBootstrapTarget> {
        let static_targets = self
            .controller
            .bootstrap
            .planned_targets()
            // The managed constructor already validated these targets. Retaining an
            // empty list here still lets the existing client survive a malformed
            // later response without a panic.
            .unwrap_or_default();
        let static_keys = static_targets
            .iter()
            .map(planned_target_key)
            .collect::<BTreeSet<_>>();
        let now = Instant::now();
        let mut route_state = self
            .controller
            .routes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        for target in refreshed_targets {
            let key = planned_target_key(&target);
            if static_keys.contains(&key) {
                continue;
            }
            route_state.dynamic_targets.insert(
                key,
                ManagedDynamicRoute {
                    target,
                    last_seen: now,
                },
            );
        }
        route_state.dynamic_targets.retain(|_, route| {
            now.saturating_duration_since(route.last_seen)
                <= self.controller.options.route_retirement_grace
        });

        let mut targets = static_targets;
        targets.extend(
            route_state
                .dynamic_targets
                .values()
                .map(|route| route.target.clone()),
        );
        targets
    }

    fn discovery_is_stale(&self) -> bool {
        self.controller
            .last_success
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_none_or(|last_success| {
                last_success.elapsed() >= self.controller.options.discovery_ttl
            })
    }

    fn recently_attempted(&self) -> bool {
        self.controller
            .last_attempt
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some_and(|last_attempt| last_attempt.elapsed() < REFRESH_COALESCE_WINDOW)
    }

    fn last_outcome(&self) -> RouteRefreshOutcome {
        self.controller
            .last_outcome
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn record_outcome(&self, outcome: RouteRefreshOutcome) -> RouteRefreshOutcome {
        *self
            .controller
            .last_outcome
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = outcome.clone();
        outcome
    }

    fn attach_runtime(&self, runtime: Arc<Runtime>) {
        *self
            .controller
            .runtime_guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(runtime);
    }

    fn schedule_refresh(&self, reason: RouteRefreshReason) {
        self.controller.clone().schedule_refresh(reason);
    }
}

impl ManagedRouteController {
    fn schedule_refresh(self: Arc<Self>, reason: RouteRefreshReason) {
        let task_controller = self.clone();
        let task = async move {
            let managed = ManagedIronMeshClient {
                client: task_controller.client.clone(),
                controller: task_controller,
            };
            let _ = managed.refresh_routes(reason).await;
        };

        let runtime = self
            .runtime_guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(runtime) = runtime {
            runtime.spawn(task);
        } else if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(task);
        }
    }
}

fn planned_target_key(target: &PlannedConnectionBootstrapTarget) -> String {
    // The target is normalized and validated by ConnectionBootstrap before it is
    // passed here. JSON preserves all transport-relevant fields (including direct
    // QUIC endpoint ID, ALPN, and relay identity) without relying on vector indices.
    serde_json::to_string(target).unwrap_or_else(|_| {
        format!(
            "{:?}#{:?}#{:?}",
            target.path_kind, target.target_node_id, target.server_base_url
        )
    })
}

fn blocking_managed_client_runtime() -> Arc<Runtime> {
    static RUNTIME: OnceLock<Arc<Runtime>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            Arc::new(
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .enable_all()
                    .build()
                    .expect("failed to build shared managed client runtime"),
            )
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use transport_sdk::{BootstrapEndpoint, BootstrapEndpointUse, BootstrapTrustRoots, RelayMode};
    use uuid::Uuid;

    #[test]
    fn blocking_managed_client_keeps_anonymous_bootstrap_on_shared_routes() {
        let bootstrap = ConnectionBootstrap {
            version: 1,
            cluster_id: Uuid::now_v7(),
            rendezvous_urls: Vec::new(),
            rendezvous_mtls_required: false,
            direct_endpoints: vec![BootstrapEndpoint {
                url: "http://127.0.0.1:9".to_string(),
                usage: Some(BootstrapEndpointUse::PublicApi),
                node_id: None,
            }],
            relay_mode: RelayMode::Disabled,
            trust_roots: BootstrapTrustRoots {
                cluster_ca_pem: None,
                public_api_ca_pem: None,
                rendezvous_ca_pem: None,
            },
            pairing_token: None,
            device_label: None,
            device_id: None,
        };
        let options = ManagedClientOptions {
            initial_discovery_timeout: Duration::ZERO,
            ..ManagedClientOptions::default()
        };

        let managed = bootstrap
            .build_managed_client_blocking(None, options)
            .expect("anonymous bootstrap should retain its direct HTTPS fallback");

        assert_eq!(managed.route_snapshot().endpoints.len(), 1);
        assert!(
            managed.route_snapshot().endpoints[0]
                .locator
                .starts_with("http://127.0.0.1:9")
        );
    }
}
