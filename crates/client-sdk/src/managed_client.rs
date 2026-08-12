use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::{
    runtime::Runtime,
    sync::{Mutex as AsyncMutex, mpsc},
};

use crate::connection::{
    build_unprobed_client_with_optional_identity_from_planned_targets, planned_transport_route_key,
};
use crate::{
    ClientConnectionDiagnosticImpact, ClientConnectionRouteEndpointSnapshot,
    ClientConnectionRouteSnapshot, ClientIdentityMaterial, ClientRouteMaintenancePolicy,
    ConnectionBootstrap, IronMeshClient, PlannedConnectionBootstrapTarget,
};

const REFRESH_COALESCE_WINDOW: Duration = Duration::from_secs(1);
const MIN_PERIODIC_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

type PersistConnectionBootstrapFn = dyn Fn(&ConnectionBootstrap) -> Result<()> + Send + Sync;

/// Optional durable storage for contact-list updates learned by a managed
/// client. The callback is invoked only after an authenticated cluster API
/// response changes the cached `version_id`.
#[derive(Clone)]
pub struct ManagedBootstrapPersistence {
    source: &'static str,
    persist: Arc<PersistConnectionBootstrapFn>,
}

impl ManagedBootstrapPersistence {
    pub fn new<F>(source: &'static str, persist: F) -> Self
    where
        F: Fn(&ConnectionBootstrap) -> Result<()> + Send + Sync + 'static,
    {
        Self {
            source,
            persist: Arc::new(persist),
        }
    }

    fn persist(&self, bootstrap: &ConnectionBootstrap) -> Result<()> {
        (self.persist)(bootstrap)
    }

    fn source(&self) -> &'static str {
        self.source
    }
}

impl std::fmt::Debug for ManagedBootstrapPersistence {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedBootstrapPersistence")
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

/// Policy for shared, Rendezvous-managed client routes.
#[derive(Debug, Clone)]
pub struct ManagedClientOptions {
    /// Maximum foreground wait for the first advisory discovery pass.
    pub initial_discovery_timeout: Duration,
    /// Low-priority fallback cadence for discovery. Foreground, network-change,
    /// and relay-connected events refresh immediately; this interval only keeps
    /// dynamic routes fresh when none of those events arrive.
    pub discovery_ttl: Duration,
    /// Time an absent dynamic candidate remains selectable after a successful
    /// discovery response omits it. Static bootstrap targets are never retired.
    pub route_retirement_grace: Duration,
    /// Lets a caller forward an all-route transport failure without duplicating
    /// discovery or fallback policy in a platform wrapper.
    pub refresh_on_transport_failure: bool,
    /// Synthetic route-health maintenance performed by the shared transport
    /// router. Real requests and failover are not delayed by this policy.
    pub route_maintenance_policy: ClientRouteMaintenancePolicy,
    /// Optional durable storage for authenticated cluster-managed Rendezvous
    /// contact-list updates.
    pub connection_bootstrap_persistence: Option<ManagedBootstrapPersistence>,
}

impl Default for ManagedClientOptions {
    fn default() -> Self {
        Self {
            initial_discovery_timeout: Duration::from_secs(2),
            discovery_ttl: Duration::from_secs(2 * 60),
            route_retirement_grace: Duration::from_secs(10 * 60),
            refresh_on_transport_failure: true,
            route_maintenance_policy: ClientRouteMaintenancePolicy::default(),
            connection_bootstrap_persistence: None,
        }
    }
}

impl ManagedClientOptions {
    /// Conservative maintenance cadence for long-running mobile background work.
    pub fn mobile_background() -> Self {
        Self {
            discovery_ttl: Duration::from_secs(15 * 60),
            // Preserve the default policy's five-refresh tolerance even though
            // the low-priority background discovery cadence is much longer.
            route_retirement_grace: Duration::from_secs(75 * 60),
            route_maintenance_policy: ClientRouteMaintenancePolicy::mobile_background(),
            ..Self::default()
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
    RelayConnected,
    Foregrounded,
    ExplicitDiagnosticRequest,
}

impl RouteRefreshReason {
    const fn is_event_driven(self) -> bool {
        matches!(
            self,
            Self::NetworkChanged | Self::RelayConnected | Self::Foregrounded
        )
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Stale => "stale",
            Self::NetworkChanged => "network_changed",
            Self::TransportFailure => "transport_failure",
            Self::RelayConnected => "relay_connected",
            Self::Foregrounded => "foregrounded",
            Self::ExplicitDiagnosticRequest => "explicit_diagnostic_request",
        }
    }
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
    bootstrap: Mutex<ConnectionBootstrap>,
    identity: Mutex<Option<ClientIdentityMaterial>>,
    options: ManagedClientOptions,
    refresh_lock: AsyncMutex<()>,
    routes: Mutex<ManagedRouteState>,
    last_attempt: Mutex<Option<ManagedRouteRefreshAttempt>>,
    last_success: Mutex<Option<Instant>>,
    last_outcome: Mutex<RouteRefreshOutcome>,
    pending_identity_update: Mutex<Option<ClientIdentityMaterial>>,
    pending_connection_bootstrap_update: Mutex<Option<ConnectionBootstrap>>,
    periodic_refresh_started: AtomicBool,
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

#[derive(Clone, Copy)]
struct ManagedRouteRefreshAttempt {
    started_at: Instant,
    reason: RouteRefreshReason,
}

#[derive(Debug, Deserialize)]
struct RendezvousContactConfigurationResponse {
    configuration: RendezvousContactConfiguration,
    stored: bool,
    version_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RendezvousContactConfiguration {
    schema_version: u32,
    #[serde(default)]
    rendezvous_urls: Vec<String>,
}

#[derive(Clone, Copy)]
enum RouteSource {
    Bootstrap,
    Cache,
    Rendezvous,
}

impl RouteSource {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Bootstrap => "bootstrap",
            Self::Cache => "cache",
            Self::Rendezvous => "rendezvous",
        }
    }
}

#[derive(Clone, Copy)]
struct RouteRefreshTelemetry {
    reason: RouteRefreshReason,
    started_at: Instant,
    candidate_count_before: usize,
}

#[derive(Default)]
struct RouteMembershipChanges {
    added: usize,
    removed: usize,
    retained: usize,
}

impl RouteMembershipChanges {
    fn record(&mut self, (added, removed, retained): (usize, usize, usize)) {
        self.added = self.added.saturating_add(added);
        self.removed = self.removed.saturating_add(removed);
        self.retained = retained;
    }
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
        let client = self
            .build_client_with_optional_identity(identity.as_ref())?
            .with_route_maintenance_policy(options.route_maintenance_policy)
            .with_connection_diagnostic_impact(
                ClientConnectionDiagnosticImpact::BackgroundMaintenance,
            );
        let controller = Arc::new(ManagedRouteController {
            client: client.clone(),
            bootstrap: Mutex::new(self.clone()),
            identity: Mutex::new(identity),
            options: options.clone(),
            refresh_lock: AsyncMutex::new(()),
            routes: Mutex::new(ManagedRouteState::default()),
            last_attempt: Mutex::new(None),
            last_success: Mutex::new(None),
            last_outcome: Mutex::new(RouteRefreshOutcome::default()),
            pending_identity_update: Mutex::new(None),
            pending_connection_bootstrap_update: Mutex::new(None),
            periodic_refresh_started: AtomicBool::new(false),
            runtime_guard: Mutex::new(None),
        });
        let managed = ManagedIronMeshClient {
            client,
            controller: controller.clone(),
        };
        for endpoint in &managed.route_snapshot().endpoints {
            log_route_added(RouteSource::Bootstrap, endpoint);
        }
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
        let weak_controller = Arc::downgrade(&controller);
        managed
            .client
            .set_relay_connection_refresh_observer(Some(Arc::new(move |target_node_id| {
                let Some(controller) = weak_controller.upgrade() else {
                    return;
                };
                tracing::info!(
                    event = "dynamic_route_refresh_triggered",
                    refresh_reason = RouteRefreshReason::RelayConnected.as_str(),
                    target_node_id = %target_node_id,
                    "scheduling dynamic route refresh after first successful relay connection"
                );
                controller.schedule_refresh(RouteRefreshReason::RelayConnected);
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
        managed.start_periodic_refresh();

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
        // Keep the public async wrapper shallow for FFI and CLI callers. The
        // managed refresh path includes multiplexed direct and relay request
        // futures; boxing it here avoids requiring every consumer crate to
        // raise its compiler recursion limit.
        Box::pin(self.build_managed_client(Some(identity), options)).await
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
        self.client
            .clone()
            .with_connection_diagnostic_impact(ClientConnectionDiagnosticImpact::UserFacing)
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

    /// Returns and clears an authenticated cluster-managed Rendezvous contact
    /// list update. Platform owners without a Rust-side persistence callback
    /// can persist this bootstrap with their own secure/preferences storage.
    pub fn take_connection_bootstrap_update(&self) -> Option<ConnectionBootstrap> {
        self.controller
            .pending_connection_bootstrap_update
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    /// Reads a pending contact-list bootstrap update without acknowledging it.
    pub fn latest_connection_bootstrap_update(&self) -> Option<ConnectionBootstrap> {
        self.controller
            .pending_connection_bootstrap_update
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub async fn refresh_routes(&self, reason: RouteRefreshReason) -> RouteRefreshOutcome {
        if matches!(reason, RouteRefreshReason::Stale) && !self.discovery_is_stale() {
            return self.last_outcome();
        }

        let _refresh_guard = self.controller.refresh_lock.lock().await;
        if self.recently_attempted(reason) {
            return self.last_outcome();
        }
        *self
            .controller
            .last_attempt
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(ManagedRouteRefreshAttempt {
            started_at: Instant::now(),
            reason,
        });
        let telemetry = RouteRefreshTelemetry {
            reason,
            started_at: Instant::now(),
            candidate_count_before: self.client.connection_route_snapshot().endpoints.len(),
        };
        tracing::info!(
            event = "route_refresh_started",
            refresh_reason = reason.as_str(),
            candidate_count_before = telemetry.candidate_count_before,
            "refreshing dynamic client routes"
        );

        let mut identity = self
            .controller
            .identity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let identity_updated = if let Some(current_identity) = identity.clone() {
            let bootstrap = self
                .controller
                .bootstrap
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
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
                    return self.finish_route_refresh(
                        telemetry,
                        RouteRefreshOutcome {
                            discovery_used: false,
                            discovery_error: Some(RouteDiscoveryError {
                                message: format!("failed renewing Rendezvous identity: {error:#}"),
                            }),
                            identity_updated: false,
                            ..RouteRefreshOutcome::default()
                        },
                    );
                }
                Err(error) => {
                    return self.finish_route_refresh(
                        telemetry,
                        RouteRefreshOutcome {
                            discovery_used: false,
                            discovery_error: Some(RouteDiscoveryError {
                                message: format!(
                                    "Rendezvous identity renewal task failed: {error}"
                                ),
                            }),
                            identity_updated: false,
                            ..RouteRefreshOutcome::default()
                        },
                    );
                }
            }
        } else {
            false
        };

        if identity.is_some()
            && let Err(error) = self.refresh_cluster_rendezvous_contact_list().await
        {
            // This is intentionally advisory: a client with a valid immutable
            // bootstrap must remain usable while the replicated configuration
            // has not reached its current node yet.
            tracing::warn!(
                event = "rendezvous_contact_list_refresh_failed",
                refresh_reason = telemetry.reason.as_str(),
                error = %format!("{error:#}"),
                "failed refreshing authenticated cluster-managed rendezvous contacts"
            );
        }

        let (updates_sender, mut updates_receiver) = mpsc::unbounded_channel();
        let bootstrap = self
            .controller
            .bootstrap
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let discovery_identity = identity.clone();
        let discovery = bootstrap.refresh_dynamic_targets_with_updates(
            discovery_identity.as_ref(),
            move |targets| {
                updates_sender
                    .send(targets)
                    .map_err(|_| anyhow!("dynamic route update receiver closed"))
            },
        );
        tokio::pin!(discovery);

        let mut changes = RouteMembershipChanges::default();
        let refreshed_targets = loop {
            tokio::select! {
                Some(targets) = updates_receiver.recv() => {
                    let membership = match self
                        .adopt_discovered_targets(
                            targets,
                            identity.clone(),
                            telemetry.reason,
                            "partial",
                        )
                        .await
                    {
                        Ok(membership) => membership,
                        Err(error) => {
                            return self.finish_route_refresh(
                                telemetry,
                                RouteRefreshOutcome {
                                    discovery_used: true,
                                    discovery_error: Some(RouteDiscoveryError {
                                        message: format!("failed building discovered client routes: {error:#}"),
                                    }),
                                    routes_added: changes.added,
                                    routes_removed: changes.removed,
                                    routes_retained: changes.retained,
                                    identity_updated,
                                },
                            );
                        }
                    };
                    changes.record(membership);
                }
                result = &mut discovery => break result,
            }
        };

        let refreshed_targets = match refreshed_targets {
            Ok(targets) => targets,
            Err(error) => {
                return self.finish_route_refresh(
                    telemetry,
                    RouteRefreshOutcome {
                        discovery_used: changes.added > 0,
                        discovery_error: Some(RouteDiscoveryError {
                            message: format!("Rendezvous discovery failed: {error:#}"),
                        }),
                        routes_added: changes.added,
                        routes_removed: changes.removed,
                        routes_retained: changes.retained,
                        identity_updated,
                    },
                );
            }
        };

        // If discovery completed at the same instant as a queued update, process
        // the queue before the complete result. This keeps a session created from
        // the first usable candidate alive through final reconciliation.
        while let Ok(targets) = updates_receiver.try_recv() {
            let membership = match self
                .adopt_discovered_targets(targets, identity.clone(), telemetry.reason, "partial")
                .await
            {
                Ok(membership) => membership,
                Err(error) => {
                    return self.finish_route_refresh(
                        telemetry,
                        RouteRefreshOutcome {
                            discovery_used: true,
                            discovery_error: Some(RouteDiscoveryError {
                                message: format!(
                                    "failed building discovered client routes: {error:#}"
                                ),
                            }),
                            routes_added: changes.added,
                            routes_removed: changes.removed,
                            routes_retained: changes.retained,
                            identity_updated,
                        },
                    );
                }
            };
            changes.record(membership);
        }

        let membership = match self
            .adopt_discovered_targets(refreshed_targets, identity, telemetry.reason, "complete")
            .await
        {
            Ok(membership) => membership,
            Err(error) => {
                return self.finish_route_refresh(
                    telemetry,
                    RouteRefreshOutcome {
                        discovery_used: true,
                        discovery_error: Some(RouteDiscoveryError {
                            message: format!("failed building refreshed client routes: {error:#}"),
                        }),
                        routes_added: changes.added,
                        routes_removed: changes.removed,
                        routes_retained: changes.retained,
                        identity_updated,
                    },
                );
            }
        };
        changes.record(membership);
        *self
            .controller
            .last_success
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Instant::now());
        self.finish_route_refresh(
            telemetry,
            RouteRefreshOutcome {
                discovery_used: true,
                discovery_error: None,
                routes_added: changes.added,
                routes_removed: changes.removed,
                routes_retained: changes.retained,
                identity_updated,
            },
        )
    }

    async fn adopt_discovered_targets(
        &self,
        refreshed_targets: Vec<PlannedConnectionBootstrapTarget>,
        identity: Option<ClientIdentityMaterial>,
        refresh_reason: RouteRefreshReason,
        discovery_update: &'static str,
    ) -> Result<(usize, usize, usize)> {
        let desired_targets = self.reconcile_target_set(refreshed_targets);
        let refreshed_client = tokio::task::spawn_blocking(move || {
            build_unprobed_client_with_optional_identity_from_planned_targets(
                &desired_targets,
                identity.as_ref(),
            )
        })
        .await
        .map_err(|error| anyhow!("route construction task failed: {error}"))??;

        let routes_before = self.client.connection_route_snapshot();
        let membership = self
            .client
            .reconcile_transport_membership(&refreshed_client);
        let routes_after = self.client.connection_route_snapshot();
        log_dynamic_route_membership_changes(&routes_before, &routes_after);
        let routes_scheduled = self.client.spawn_due_connection_route_refresh();
        if routes_scheduled > 0 {
            tracing::info!(
                event = "dynamic_route_probe_scheduled",
                refresh_reason = refresh_reason.as_str(),
                discovery_update,
                routes_added = membership.0,
                routes_scheduled,
                "probing due discovered routes without blocking the active fallback"
            );
        }
        Ok(membership)
    }

    async fn refresh_cluster_rendezvous_contact_list(&self) -> Result<()> {
        let response = self
            .client
            .get_json_path("/cluster/rendezvous-contacts")
            .await
            .context("failed requesting cluster rendezvous contacts")?;
        let response = serde_json::from_value::<RendezvousContactConfigurationResponse>(response)
            .context("failed parsing cluster rendezvous contacts")?;

        self.apply_cluster_rendezvous_contact_response(response)
    }

    fn apply_cluster_rendezvous_contact_response(
        &self,
        response: RendezvousContactConfigurationResponse,
    ) -> Result<()> {
        // A missing config object is not an update. In particular, do not wipe
        // a previously persisted contact list merely because this node has not
        // received the object's metadata yet.
        if !response.stored {
            return Ok(());
        }
        let version_id = response
            .version_id
            .ok_or_else(|| anyhow!("stored cluster rendezvous contacts are missing version_id"))?;

        let updated_bootstrap = {
            let mut bootstrap = self
                .controller
                .bootstrap
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !bootstrap.apply_rendezvous_contact_list(
                response.configuration.schema_version,
                version_id,
                response.configuration.rendezvous_urls,
            )? {
                return Ok(());
            }
            bootstrap.clone()
        };

        *self
            .controller
            .pending_connection_bootstrap_update
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(updated_bootstrap.clone());

        if let Some(persistence) = self
            .controller
            .options
            .connection_bootstrap_persistence
            .as_ref()
        {
            if let Err(error) = persistence.persist(&updated_bootstrap) {
                tracing::warn!(
                    event = "rendezvous_contact_list_persistence_failed",
                    persistence_source = persistence.source(),
                    error = %error,
                    "failed persisting authenticated cluster-managed rendezvous contacts"
                );
            } else {
                tracing::info!(
                    event = "rendezvous_contact_list_persisted",
                    persistence_source = persistence.source(),
                    version_id = ?updated_bootstrap
                        .rendezvous_contact_list
                        .as_ref()
                        .map(|contacts| contacts.version_id.as_str()),
                    "persisted authenticated cluster-managed rendezvous contacts"
                );
            }
        }

        Ok(())
    }

    fn reconcile_target_set(
        &self,
        refreshed_targets: Vec<PlannedConnectionBootstrapTarget>,
    ) -> Vec<PlannedConnectionBootstrapTarget> {
        let static_targets = self
            .controller
            .bootstrap
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
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

    fn recently_attempted(&self, reason: RouteRefreshReason) -> bool {
        let last_attempt = self
            .controller
            .last_attempt
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .to_owned();
        refresh_attempt_is_coalesced(last_attempt, reason)
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

    fn finish_route_refresh(
        &self,
        telemetry: RouteRefreshTelemetry,
        outcome: RouteRefreshOutcome,
    ) -> RouteRefreshOutcome {
        let candidate_count_after = self.client.connection_route_snapshot().endpoints.len();
        tracing::info!(
            event = "route_refresh_completed",
            refresh_reason = telemetry.reason.as_str(),
            candidate_count_before = telemetry.candidate_count_before,
            candidate_count_after,
            duration_ms = telemetry.started_at.elapsed().as_millis() as u64,
            discovery_used = outcome.discovery_used,
            routes_added = outcome.routes_added,
            routes_removed = outcome.routes_removed,
            routes_retained = outcome.routes_retained,
            identity_updated = outcome.identity_updated,
            discovery_error = ?outcome.discovery_error.as_ref().map(|error| error.message.as_str()),
            "finished refreshing dynamic client routes"
        );
        self.record_outcome(outcome)
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

    fn start_periodic_refresh(&self) {
        if self
            .controller
            .bootstrap
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .effective_rendezvous_urls()
            .unwrap_or_default()
            .is_empty()
        {
            return;
        }
        self.controller.clone().start_periodic_refresh();
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

        let _ = self.spawn_refresh_task(task);
    }

    fn start_periodic_refresh(self: Arc<Self>) {
        if self
            .periodic_refresh_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }

        let interval = self
            .options
            .discovery_ttl
            .max(MIN_PERIODIC_REFRESH_INTERVAL);
        let weak_controller = Arc::downgrade(&self);
        let task = async move {
            loop {
                tokio::time::sleep(interval).await;
                let Some(controller) = weak_controller.upgrade() else {
                    return;
                };
                let managed = ManagedIronMeshClient {
                    client: controller.client.clone(),
                    controller,
                };
                let _ = managed.refresh_routes(RouteRefreshReason::Stale).await;
            }
        };

        if !self.spawn_refresh_task(task) {
            self.periodic_refresh_started
                .store(false, Ordering::Release);
        }
    }

    fn spawn_refresh_task<F>(&self, task: F) -> bool
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let runtime = self
            .runtime_guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(runtime) = runtime {
            runtime.spawn(task);
            true
        } else if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(task);
            true
        } else {
            false
        }
    }
}

fn refresh_attempt_is_coalesced(
    last_attempt: Option<ManagedRouteRefreshAttempt>,
    reason: RouteRefreshReason,
) -> bool {
    let Some(last_attempt) = last_attempt else {
        return false;
    };
    if last_attempt.started_at.elapsed() >= REFRESH_COALESCE_WINDOW {
        return false;
    }

    !(reason.is_event_driven() && last_attempt.reason != reason)
}

fn planned_target_key(target: &PlannedConnectionBootstrapTarget) -> String {
    planned_transport_route_key(target).unwrap_or_else(|_| {
        format!(
            "{:?}#{:?}#{:?}",
            target.path_kind, target.target_node_id, target.server_base_url
        )
    })
}

fn log_dynamic_route_membership_changes(
    routes_before: &ClientConnectionRouteSnapshot,
    routes_after: &ClientConnectionRouteSnapshot,
) {
    let previous = routes_before
        .endpoints
        .iter()
        .map(|endpoint| (route_snapshot_key(endpoint), endpoint))
        .collect::<BTreeMap<_, _>>();
    let current = routes_after
        .endpoints
        .iter()
        .map(|endpoint| (route_snapshot_key(endpoint), endpoint))
        .collect::<BTreeMap<_, _>>();

    for (route_key, endpoint) in &current {
        if !previous.contains_key(route_key) {
            log_route_added(RouteSource::Rendezvous, endpoint);
        }
    }
    for (route_key, endpoint) in &previous {
        if !current.contains_key(route_key) {
            log_route_removed(RouteSource::Cache, endpoint);
        }
    }
}

fn route_snapshot_key(endpoint: &ClientConnectionRouteEndpointSnapshot) -> String {
    format!(
        "{:?}#{}#{}#{}",
        endpoint.path_kind,
        endpoint.locator,
        endpoint
            .target_node_id
            .map(|node_id| node_id.to_string())
            .unwrap_or_default(),
        endpoint.hole_punching_mode.as_deref().unwrap_or_default(),
    )
}

fn log_route_added(source: RouteSource, endpoint: &ClientConnectionRouteEndpointSnapshot) {
    tracing::info!(
        event = "route_added",
        route_source = source.as_str(),
        path_kind = ?endpoint.path_kind,
        target_node_id = ?endpoint.target_node_id,
        route_locator = %endpoint.locator,
        "route candidate added to the client router"
    );
}

fn log_route_removed(source: RouteSource, endpoint: &ClientConnectionRouteEndpointSnapshot) {
    tracing::info!(
        event = "route_removed",
        route_source = source.as_str(),
        path_kind = ?endpoint.path_kind,
        target_node_id = ?endpoint.target_node_id,
        route_locator = %endpoint.locator,
        "route candidate removed from the client router"
    );
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
    use axum::{Json, Router, extract::Query, routing::get};
    use common::NodeId;
    use serde::Deserialize;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use transport_sdk::{
        BootstrapEndpoint, BootstrapEndpointUse, BootstrapTrustRoots, CandidateKind,
        ConnectionCandidate, DiscoveryResponse, RelayMode, TransportPathKind,
    };
    use uuid::Uuid;

    #[derive(Debug, Deserialize)]
    struct TestDiscoveryQuery {
        node_id: Option<String>,
    }

    #[test]
    fn default_discovery_fallback_runs_every_two_minutes() {
        assert_eq!(
            ManagedClientOptions::default().discovery_ttl,
            Duration::from_secs(2 * 60)
        );
    }

    #[test]
    fn mobile_background_options_bound_route_maintenance() {
        let options = ManagedClientOptions::mobile_background();

        assert_eq!(options.discovery_ttl, Duration::from_secs(15 * 60));
        assert_eq!(options.route_retirement_grace, options.discovery_ttl * 5);
        assert_eq!(
            options.route_maintenance_policy,
            ClientRouteMaintenancePolicy::mobile_background()
        );
    }

    #[test]
    fn event_driven_refresh_bypasses_a_different_recent_refresh_reason() {
        let just_started = ManagedRouteRefreshAttempt {
            started_at: Instant::now(),
            reason: RouteRefreshReason::Startup,
        };

        assert!(refresh_attempt_is_coalesced(
            Some(just_started),
            RouteRefreshReason::Startup,
        ));
        assert!(!refresh_attempt_is_coalesced(
            Some(just_started),
            RouteRefreshReason::RelayConnected,
        ));
        assert!(!refresh_attempt_is_coalesced(
            Some(just_started),
            RouteRefreshReason::Foregrounded,
        ));
        assert!(!refresh_attempt_is_coalesced(
            Some(just_started),
            RouteRefreshReason::NetworkChanged,
        ));
    }

    #[test]
    fn blocking_managed_client_keeps_anonymous_bootstrap_on_shared_routes() {
        let bootstrap = ConnectionBootstrap {
            version: 1,
            cluster_id: Uuid::now_v7(),
            rendezvous_urls: Vec::new(),
            rendezvous_contact_list: None,
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
        assert_eq!(
            managed.client.connection_diagnostic_impact(),
            ClientConnectionDiagnosticImpact::BackgroundMaintenance
        );
        assert_eq!(
            managed.client().connection_diagnostic_impact(),
            ClientConnectionDiagnosticImpact::UserFacing
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn managed_client_adopts_a_usable_candidate_before_other_nodes_finish_discovery() {
        let cluster_id = Uuid::now_v7();
        let fast_node_id = NodeId::new_v4();
        let stalled_node_id = NodeId::new_v4();
        let candidate_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("candidate listener should bind");
        let candidate_addr = candidate_listener
            .local_addr()
            .expect("candidate listener should expose its address");
        let candidate_probe_count = Arc::new(AtomicUsize::new(0));
        let candidate_route_probe_count = Arc::clone(&candidate_probe_count);
        let candidate_health_probe_count = Arc::clone(&candidate_probe_count);
        let candidate_server = tokio::spawn(async move {
            axum::serve(
                candidate_listener,
                Router::new()
                    .route(
                        "/api/v1/diagnostics/latency",
                        get(move || {
                            let candidate_route_probe_count =
                                Arc::clone(&candidate_route_probe_count);
                            async move {
                                candidate_route_probe_count.fetch_add(1, Ordering::SeqCst);
                                "ok"
                            }
                        }),
                    )
                    .route(
                        "/api/v1/health",
                        get(move || {
                            let candidate_health_probe_count =
                                Arc::clone(&candidate_health_probe_count);
                            async move {
                                candidate_health_probe_count.fetch_add(1, Ordering::SeqCst);
                                "ok"
                            }
                        }),
                    ),
            )
            .await
            .expect("candidate test server should run");
        });
        let candidate_url = format!("http://{candidate_addr}");
        let fast_node_label = fast_node_id.to_string();
        let discovery_candidate_url = candidate_url.clone();
        let discovery_router = Router::new()
            .route(
                "/control/discovery",
                get(move |Query(query): Query<TestDiscoveryQuery>| {
                    let fast_node_label = fast_node_label.clone();
                    let candidate_url = discovery_candidate_url.clone();
                    async move {
                        if query.node_id.as_deref() == Some(fast_node_label.as_str()) {
                            return Json(DiscoveryResponse {
                                rendezvous_peers: Vec::new(),
                                node_candidates: Some(vec![ConnectionCandidate {
                                    kind: CandidateKind::DirectHttps,
                                    endpoint: candidate_url,
                                    rtt_ms: Some(5),
                                    transport_hints: None,
                                }]),
                                node_relay_capable: false,
                            });
                        }
                        if query.node_id.is_some() {
                            tokio::time::sleep(Duration::from_secs(5)).await;
                        }
                        Json(DiscoveryResponse {
                            rendezvous_peers: Vec::new(),
                            node_candidates: None,
                            node_relay_capable: false,
                        })
                    }
                }),
            )
            .route("/api/v1/diagnostics/latency", get(|| async { "ok" }))
            .route("/api/v1/health", get(|| async { "ok" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("discovery listener should bind");
        let rendezvous_addr = listener
            .local_addr()
            .expect("discovery listener should expose its address");
        let discovery_server = tokio::spawn(async move {
            axum::serve(listener, discovery_router)
                .await
                .expect("discovery test server should run");
        });

        let route_url = format!("http://{rendezvous_addr}");
        let bootstrap = ConnectionBootstrap {
            version: transport_sdk::CLIENT_BOOTSTRAP_VERSION,
            cluster_id,
            rendezvous_urls: vec![route_url.clone()],
            rendezvous_contact_list: None,
            rendezvous_mtls_required: false,
            direct_endpoints: vec![
                BootstrapEndpoint {
                    url: route_url.clone(),
                    usage: Some(BootstrapEndpointUse::PublicApi),
                    node_id: Some(fast_node_id),
                },
                BootstrapEndpoint {
                    url: route_url,
                    usage: Some(BootstrapEndpointUse::PublicApi),
                    node_id: Some(stalled_node_id),
                },
            ],
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
        let managed = bootstrap
            .build_managed_client(
                None,
                ManagedClientOptions {
                    initial_discovery_timeout: Duration::ZERO,
                    discovery_ttl: Duration::from_secs(60),
                    ..ManagedClientOptions::default()
                },
            )
            .await
            .expect("managed client should start from its static routes");
        let refresh_client = managed.clone();
        let refresh_task =
            tokio::spawn(async move { refresh_client.notify_network_changed_async().await });

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let has_fast_candidate_route =
                    managed.route_snapshot().endpoints.iter().any(|endpoint| {
                        endpoint.path_kind == TransportPathKind::DirectHttps
                            && endpoint.target_node_id == Some(fast_node_id)
                            && endpoint.locator == candidate_url
                            && candidate_probe_count.load(Ordering::SeqCst) > 0
                    });
                if has_fast_candidate_route {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the usable candidate should be adopted before the stalled node completes");

        refresh_task.abort();
        let _ = refresh_task.await;
        discovery_server.abort();
        let _ = discovery_server.await;
        candidate_server.abort();
        let _ = candidate_server.await;
    }

    #[test]
    fn managed_client_persists_cluster_rendezvous_contacts() {
        let cluster_id = Uuid::now_v7();
        let persisted = Arc::new(Mutex::new(Vec::<ConnectionBootstrap>::new()));
        let persisted_updates = Arc::clone(&persisted);
        let bootstrap = ConnectionBootstrap {
            version: transport_sdk::CLIENT_BOOTSTRAP_VERSION,
            cluster_id,
            rendezvous_urls: vec!["https://bootstrap.example".to_string()],
            rendezvous_contact_list: None,
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
        let mut identity = ClientIdentityMaterial::generate(cluster_id, None, None)
            .expect("test identity should generate");
        identity.credential_pem = Some("issued-credential".to_string());
        let client = bootstrap
            .build_client_with_identity(&identity)
            .expect("test client should build");
        let managed = ManagedIronMeshClient {
            client: client.clone(),
            controller: Arc::new(ManagedRouteController {
                client,
                bootstrap: Mutex::new(bootstrap),
                identity: Mutex::new(Some(identity)),
                options: ManagedClientOptions {
                    connection_bootstrap_persistence: Some(ManagedBootstrapPersistence::new(
                        "test",
                        move |bootstrap| {
                            persisted_updates
                                .lock()
                                .expect("persisted updates lock should not be poisoned")
                                .push(bootstrap.clone());
                            Ok(())
                        },
                    )),
                    ..ManagedClientOptions::default()
                },
                refresh_lock: AsyncMutex::new(()),
                routes: Mutex::new(ManagedRouteState::default()),
                last_attempt: Mutex::new(None),
                last_success: Mutex::new(None),
                last_outcome: Mutex::new(RouteRefreshOutcome::default()),
                pending_identity_update: Mutex::new(None),
                pending_connection_bootstrap_update: Mutex::new(None),
                periodic_refresh_started: AtomicBool::new(false),
                runtime_guard: Mutex::new(None),
            }),
        };
        managed
            .apply_cluster_rendezvous_contact_response(RendezvousContactConfigurationResponse {
                configuration: RendezvousContactConfiguration {
                    schema_version: 1,
                    rendezvous_urls: vec!["https://home.example:19080".to_string()],
                },
                stored: true,
                version_id: Some("019d-contact-list-version".to_string()),
            })
            .expect("cluster contact response should be persisted");
        let updated = managed
            .take_connection_bootstrap_update()
            .expect("contact list update should be available to platform persistence");
        assert_eq!(
            updated
                .rendezvous_contact_list
                .as_ref()
                .expect("updated bootstrap should contain contacts")
                .version_id,
            "019d-contact-list-version"
        );
        assert_eq!(
            updated
                .effective_rendezvous_urls()
                .expect("updated contact list should be usable"),
            vec![
                "https://home.example:19080/".to_string(),
                "https://bootstrap.example/".to_string(),
            ]
        );
        assert_eq!(
            persisted
                .lock()
                .expect("persisted updates lock should not be poisoned")
                .len(),
            1
        );
    }
}
