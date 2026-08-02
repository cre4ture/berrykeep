use anyhow::{Context, Result, anyhow, bail};
use common::NodeId;
use futures_util::StreamExt;
use iroh::SecretKey;
use iroh::endpoint::{Connection, PathList};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Semaphore, watch};
use transport_sdk::{
    ClientIdentityMaterial, ConnectionCandidate, DEFAULT_DIRECT_QUIC_ALPN, DirectQuicEndpoint,
    DirectQuicEndpointConfig, DirectQuicSession, ExpectedNodeServerIdentity, IrohRelayTicket,
    IrohRelayTicketCollection, IrohRelayTicketRollover, IrohRelayTicketSet, MultiplexConfig,
    MultiplexMode, MultiplexedSession, PeerIdentity, RelayTicketRequest, RelayTunnelSession,
    RelayTunnelSessionKind, RelayTunnelSourceSecurityConfig, RendezvousControlClient,
    TRANSPORT_PROTOCOL_VERSION, TransportSessionControlMessage, TransportSessionRole,
    WebSocketByteStream, build_signed_request_headers,
    connect_websocket_with_expected_server_identity, endpoint_id_from_candidate,
    has_usable_peer_addresses, perform_transport_client_handshake, websocket_url,
};

const IROH_RELAY_TICKET_REFRESH_MAX_INTERVAL: Duration = Duration::from_secs(5 * 60);
const MOBILE_CONNECTION_LOG_TARGET: &str = "ironmesh_mobile_connection";

#[derive(Clone)]
pub(crate) struct TransportSessionPool {
    target: SessionPoolTarget,
    cached_session: Arc<Mutex<Option<CachedTransportSession>>>,
    // Serializes cold setup without retaining the cache mutex during network I/O.
    session_setup_permit: Arc<Semaphore>,
    // A cold QUIC setup is owned by the pool rather than a request or health
    // probe.  A short-lived consumer may stop waiting without cancelling the
    // ticket request, endpoint creation, or QUIC handshake for other users.
    direct_quic_setup: Arc<SharedSessionSetupCoordinator>,
    stats: Arc<TransportSessionPoolStats>,
    route_index: Arc<AtomicU64>,
}

#[derive(Clone)]
enum SessionPoolTarget {
    DirectHttps {
        server_base_url: String,
        server_ca_pem: Option<String>,
        expected_server_identity: Option<ExpectedNodeServerIdentity>,
    },
    DirectQuic {
        candidate: ConnectionCandidate,
        target_node_id: Option<NodeId>,
        endpoint: Arc<Mutex<Option<ManagedDirectQuicEndpoint>>>,
        rendezvous: Option<RendezvousControlClient>,
        relay_ca_pem: Option<String>,
    },
    Relay {
        rendezvous: RendezvousControlClient,
        target_node_id: NodeId,
        source_security: RelayTunnelSourceSecurityConfig,
    },
}

struct CachedTransportSession {
    session: Arc<MultiplexedSession>,
    _relay_session: Option<RelayTunnelSession>,
}

struct ManagedDirectQuicEndpoint {
    endpoint: DirectQuicEndpoint,
    relay_ticket_refresh: Option<tokio::task::AbortHandle>,
}

#[derive(Clone)]
struct DirectQuicEndpointLifecycle {
    endpoint_slot: Arc<Mutex<Option<ManagedDirectQuicEndpoint>>>,
    cached_session: Arc<Mutex<Option<CachedTransportSession>>>,
    stats: Arc<TransportSessionPoolStats>,
    endpoint_config: DirectQuicEndpointConfig,
    rendezvous: RendezvousControlClient,
    local_iroh_endpoint_id: String,
    remote_iroh_endpoint_id: String,
    target_node_id: NodeId,
}

type SharedSessionSetupResult = std::result::Result<Arc<MultiplexedSession>, Arc<str>>;

#[derive(Clone)]
struct SharedSessionSetup {
    attempt_id: u64,
    receiver: watch::Receiver<Option<SharedSessionSetupResult>>,
}

#[derive(Default)]
struct SharedSessionSetupCoordinator {
    active: Mutex<Option<SharedSessionSetup>>,
    next_attempt_id: AtomicU64,
    setup_active: std::sync::atomic::AtomicBool,
}

#[derive(Clone, Copy)]
pub(crate) enum DirectQuicSetupWaiter {
    SessionConsumer,
    BackgroundHealthProbe { probe_index: usize },
}

impl DirectQuicSetupWaiter {
    const fn label(self) -> &'static str {
        match self {
            Self::SessionConsumer => "session_consumer",
            Self::BackgroundHealthProbe { .. } => "background_health_probe",
        }
    }

    const fn probe_index(self) -> Option<usize> {
        match self {
            Self::SessionConsumer => None,
            Self::BackgroundHealthProbe { probe_index } => Some(probe_index),
        }
    }
}

struct DirectQuicSetupWaitGuard {
    setup_attempt_id: u64,
    candidate_index: Option<usize>,
    locator: String,
    target_node_id: Option<NodeId>,
    coordinator: Arc<SharedSessionSetupCoordinator>,
    waiter: DirectQuicSetupWaiter,
    completed: bool,
}

#[derive(Debug)]
struct ColdDirectQuicSessionSetupTimeout {
    target_label: String,
    stage: &'static str,
}

struct DirectQuicConnectContext<'a> {
    candidate: &'a ConnectionCandidate,
    target_node_id: NodeId,
    endpoint: &'a Arc<Mutex<Option<ManagedDirectQuicEndpoint>>>,
    cached_session: &'a Arc<Mutex<Option<CachedTransportSession>>>,
    stats: &'a Arc<TransportSessionPoolStats>,
    rendezvous: Option<&'a RendezvousControlClient>,
    relay_ca_pem: Option<&'a str>,
    setup_attempt_id: u64,
    setup_started: Instant,
}

impl std::fmt::Display for ColdDirectQuicSessionSetupTimeout {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "cold direct QUIC session setup to {} timed out after {:?} during {}",
            self.target_label, COLD_DIRECT_QUIC_SESSION_SETUP_TIMEOUT, self.stage
        )
    }
}

impl std::error::Error for ColdDirectQuicSessionSetupTimeout {}

impl DirectQuicSetupWaitGuard {
    fn complete(&mut self) {
        self.completed = true;
    }
}

impl Drop for DirectQuicSetupWaitGuard {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        tracing::info!(
            target: MOBILE_CONNECTION_LOG_TARGET,
            event = "iroh_session_setup_wait_cancelled",
            setup_attempt_id = self.setup_attempt_id,
            candidate_index = ?self.candidate_index,
            path_kind = "direct_quic",
            locator = %self.locator,
            target_node_id = ?self.target_node_id,
            cancelled_by = self.waiter.label(),
            probe_index = ?self.waiter.probe_index(),
            shared_session_setup_continues = self.coordinator.setup_active.load(Ordering::Relaxed),
            "iroh_session_setup_wait_cancelled"
        );
    }
}

impl Drop for ManagedDirectQuicEndpoint {
    fn drop(&mut self) {
        if let Some(refresh) = self.relay_ticket_refresh.take() {
            refresh.abort();
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransportSessionPoolSnapshot {
    pub connect_count: u64,
    pub reuse_count: u64,
    pub reset_count: u64,
    #[serde(default)]
    pub connect_duration_us: u64,
    #[serde(default)]
    pub relay_pairing_duration_us: u64,
}

#[derive(Default)]
struct TransportSessionPoolStats {
    connect_count: AtomicU64,
    reuse_count: AtomicU64,
    reset_count: AtomicU64,
    connect_duration_us: AtomicU64,
    relay_pairing_duration_us: AtomicU64,
    direct_connection_mode: AtomicU64,
}

const DIRECT_CONNECTION_MODE_UNKNOWN: u64 = 0;
const DIRECT_CONNECTION_MODE_DIRECT: u64 = 1;
const DIRECT_CONNECTION_MODE_RELAY: u64 = 2;
pub(crate) const RELAY_TICKET_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
const COLD_DIRECT_QUIC_SESSION_SETUP_TIMEOUT: Duration = Duration::from_secs(10);

impl TransportSessionPool {
    pub(crate) fn new_direct(
        server_base_url: impl Into<String>,
        server_ca_pem: Option<String>,
        expected_server_identity: Option<ExpectedNodeServerIdentity>,
    ) -> Self {
        Self {
            target: SessionPoolTarget::DirectHttps {
                server_base_url: server_base_url.into().trim_end_matches('/').to_string(),
                server_ca_pem,
                expected_server_identity,
            },
            cached_session: Arc::new(Mutex::new(None)),
            session_setup_permit: Arc::new(Semaphore::new(1)),
            direct_quic_setup: Arc::new(SharedSessionSetupCoordinator::default()),
            stats: Arc::new(TransportSessionPoolStats::default()),
            route_index: Arc::new(AtomicU64::new(u64::MAX)),
        }
    }

    pub(crate) fn new_direct_quic(
        candidate: ConnectionCandidate,
        target_node_id: Option<NodeId>,
        rendezvous: Option<RendezvousControlClient>,
        relay_ca_pem: Option<String>,
    ) -> Self {
        Self {
            target: SessionPoolTarget::DirectQuic {
                candidate,
                target_node_id,
                endpoint: Arc::new(Mutex::new(None)),
                rendezvous,
                relay_ca_pem,
            },
            cached_session: Arc::new(Mutex::new(None)),
            session_setup_permit: Arc::new(Semaphore::new(1)),
            direct_quic_setup: Arc::new(SharedSessionSetupCoordinator::default()),
            stats: Arc::new(TransportSessionPoolStats::default()),
            route_index: Arc::new(AtomicU64::new(u64::MAX)),
        }
    }

    pub(crate) fn new_relay(
        rendezvous: RendezvousControlClient,
        target_node_id: NodeId,
        source_security: RelayTunnelSourceSecurityConfig,
    ) -> Self {
        Self {
            target: SessionPoolTarget::Relay {
                rendezvous,
                target_node_id,
                source_security,
            },
            cached_session: Arc::new(Mutex::new(None)),
            session_setup_permit: Arc::new(Semaphore::new(1)),
            direct_quic_setup: Arc::new(SharedSessionSetupCoordinator::default()),
            stats: Arc::new(TransportSessionPoolStats::default()),
            route_index: Arc::new(AtomicU64::new(u64::MAX)),
        }
    }

    pub(crate) fn snapshot(&self) -> TransportSessionPoolSnapshot {
        TransportSessionPoolSnapshot {
            connect_count: self.stats.connect_count.load(Ordering::Relaxed),
            reuse_count: self.stats.reuse_count.load(Ordering::Relaxed),
            reset_count: self.stats.reset_count.load(Ordering::Relaxed),
            connect_duration_us: self.stats.connect_duration_us.load(Ordering::Relaxed),
            relay_pairing_duration_us: self.stats.relay_pairing_duration_us.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn set_route_index(&self, route_index: usize) {
        self.route_index.store(
            route_index.try_into().unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }

    fn route_index(&self) -> Option<usize> {
        let route_index = self.route_index.load(Ordering::Relaxed);
        (route_index != u64::MAX).then_some(route_index as usize)
    }

    pub(crate) fn hole_punching_mode(&self) -> Option<&'static str> {
        if !matches!(&self.target, SessionPoolTarget::DirectQuic { .. }) {
            return None;
        }

        Some(
            match self.stats.direct_connection_mode.load(Ordering::Relaxed) {
                DIRECT_CONNECTION_MODE_DIRECT => "direct",
                DIRECT_CONNECTION_MODE_RELAY => "relay",
                _ => "unknown",
            },
        )
    }

    pub(crate) async fn invalidate(&self) {
        let mut guard = self.cached_session.lock().await;
        if guard.take().is_some() {
            self.stats.reset_count.fetch_add(1, Ordering::Relaxed);
            self.stats
                .direct_connection_mode
                .store(DIRECT_CONNECTION_MODE_UNKNOWN, Ordering::Relaxed);
        }
    }

    pub(crate) async fn ensure_direct_session(
        &self,
        identity: &ClientIdentityMaterial,
        connection_name: Option<&str>,
        setup_waiter: DirectQuicSetupWaiter,
    ) -> Result<Arc<MultiplexedSession>> {
        if matches!(&self.target, SessionPoolTarget::DirectQuic { .. }) {
            if let Some(session) = self.cached_session().await {
                return Ok(session);
            }

            let setup_pool = self.clone();
            let setup_identity = identity.clone();
            let setup_connection_name = connection_name.map(ToString::to_string);
            return self
                .wait_for_shared_direct_quic_setup(
                    setup_waiter,
                    move |setup_attempt_id| async move {
                        let setup_started = Instant::now();
                        setup_pool.log_direct_quic_session_setup_started(setup_attempt_id);
                        let result = setup_pool
                            .establish_direct_session(
                                &setup_identity,
                                setup_connection_name.as_deref(),
                                Some(setup_attempt_id),
                            )
                            .await;
                        setup_pool.log_direct_quic_session_setup_finished(
                            setup_attempt_id,
                            setup_started.elapsed(),
                            &result,
                        );
                        result
                    },
                )
                .await;
        }

        self.establish_direct_session(identity, connection_name, None)
            .await
    }

    async fn establish_direct_session(
        &self,
        identity: &ClientIdentityMaterial,
        connection_name: Option<&str>,
        direct_quic_setup_attempt_id: Option<u64>,
    ) -> Result<Arc<MultiplexedSession>> {
        if let Some(session) = self.cached_session().await {
            return Ok(session);
        }
        // Direct HTTPS retains the existing serialization.  Direct QUIC is
        // serialized by its detached, shared setup task above, which must not
        // be owned by an individual request or health probe.
        let _setup_permit = if !matches!(&self.target, SessionPoolTarget::DirectQuic { .. }) {
            Some(self.acquire_session_setup_permit().await)
        } else {
            None
        };
        if _setup_permit.is_some()
            && let Some(session) = self.cached_session().await
        {
            return Ok(session);
        }

        let connect_started = Instant::now();
        let mut cold_quic_setup_started = None;

        let (multiplexed, handshake_context, target) = match &self.target {
            SessionPoolTarget::DirectHttps {
                server_base_url,
                server_ca_pem,
                expected_server_identity,
            } => {
                let ws_url = websocket_url(server_base_url, "transport/ws").with_context(|| {
                    format!("failed building direct transport websocket URL from {server_base_url}")
                })?;
                let ws_headers = websocket_auth_headers(identity, connection_name)?;
                let websocket = connect_websocket_with_expected_server_identity(
                    &ws_url,
                    server_ca_pem.as_deref(),
                    None,
                    *expected_server_identity,
                    &ws_headers,
                )
                .await
                .with_context(|| format!("failed opening direct transport websocket {}", ws_url))?;
                let transport = WebSocketByteStream::new(websocket);
                (
                    MultiplexedSession::spawn(
                        transport,
                        MultiplexMode::Client,
                        MultiplexConfig::default(),
                    )
                    .context("failed creating direct multiplexed transport session")?,
                    format!("failed performing direct transport handshake for {server_base_url}"),
                    expected_server_identity.map(|identity| PeerIdentity::Node(identity.node_id)),
                )
            }
            SessionPoolTarget::DirectQuic {
                candidate,
                target_node_id,
                endpoint,
                rendezvous,
                relay_ca_pem,
            } => {
                let target_node_id = target_node_id.ok_or_else(|| {
                    anyhow!("direct QUIC transport target is missing target node id")
                })?;
                let setup_attempt_id = direct_quic_setup_attempt_id.ok_or_else(|| {
                    anyhow!("direct QUIC session setup is missing its attempt id")
                })?;
                let target_label = candidate.endpoint.clone();
                let setup_started = Instant::now();
                cold_quic_setup_started = Some(setup_started);
                let direct_quic = self
                    .connect_direct_quic_session(DirectQuicConnectContext {
                        candidate,
                        target_node_id,
                        endpoint,
                        cached_session: &self.cached_session,
                        stats: &self.stats,
                        rendezvous: rendezvous.as_ref(),
                        relay_ca_pem: relay_ca_pem.as_deref(),
                        setup_attempt_id,
                        setup_started,
                    })
                    .await?;
                self.stats.direct_connection_mode.store(
                    direct_connection_mode_from_paths(&direct_quic.connection.paths()),
                    Ordering::Relaxed,
                );
                self.spawn_hole_punching_monitor(direct_quic.connection.clone());
                (
                    direct_quic.session,
                    format!("failed performing direct QUIC transport handshake for {target_label}"),
                    Some(PeerIdentity::Node(target_node_id)),
                )
            }
            SessionPoolTarget::Relay { .. } => {
                bail!("attempted to open a direct session from a relay transport session pool");
            }
        };

        let handshake = perform_transport_client_handshake(
            &multiplexed,
            TransportSessionControlMessage::Hello {
                protocol_version: TRANSPORT_PROTOCOL_VERSION,
                cluster_id: identity.cluster_id,
                role: TransportSessionRole::Client,
                peer: PeerIdentity::Device(identity.device_id),
                connection_name: connection_name.map(ToString::to_string),
                target,
            },
        );
        let handshake_result = match cold_quic_setup_started {
            Some(setup_started) => {
                let target_label = match &self.target {
                    SessionPoolTarget::DirectQuic { candidate, .. } => candidate.endpoint.clone(),
                    _ => "direct_quic".to_string(),
                };
                let remaining_budget =
                    remaining_cold_quic_setup_budget(setup_started).map_err(|_| {
                        ColdDirectQuicSessionSetupTimeout {
                            target_label: target_label.clone(),
                            stage: "transport_handshake",
                        }
                    })?;
                tokio::time::timeout(remaining_budget, handshake)
                    .await
                    .map_err(|_| ColdDirectQuicSessionSetupTimeout {
                        target_label,
                        stage: "transport_handshake",
                    })?
            }
            None => handshake.await,
        };
        handshake_result.with_context(|| handshake_context)?;

        let session = Arc::new(multiplexed);
        let (session, inserted) = self.cache_session(session, None).await;
        if inserted {
            self.stats.connect_count.fetch_add(1, Ordering::Relaxed);
            self.stats.connect_duration_us.fetch_add(
                duration_as_u64_micros(connect_started.elapsed()),
                Ordering::Relaxed,
            );
        }
        Ok(session)
    }

    async fn connect_direct_quic_session(
        &self,
        context: DirectQuicConnectContext<'_>,
    ) -> Result<DirectQuicSession> {
        let candidate = context.candidate;
        let target_node_id = context.target_node_id;
        let setup_attempt_id = context.setup_attempt_id;
        let target_label = context.candidate.endpoint.clone();
        let remote_iroh_endpoint_id = endpoint_id_from_candidate(candidate)?;
        let setup_started = context.setup_started;
        let mut iroh_connect_started = None;
        let mut local_iroh_endpoint_id = None;
        let direct_quic_result =
            tokio::time::timeout(remaining_cold_quic_setup_budget(setup_started)?, async {
                let endpoint = ensure_direct_quic_endpoint(&context, self.route_index()).await?;
                let endpoint_id = endpoint.endpoint_id();
                let remaining_setup_budget = remaining_cold_quic_setup_budget(setup_started)?;
                let connect_started = Instant::now();
                iroh_connect_started = Some(connect_started);
                local_iroh_endpoint_id = Some(endpoint_id.clone());
                tracing::info!(
                    target: MOBILE_CONNECTION_LOG_TARGET,
                    event = "iroh_connect_scheduled",
                    setup_attempt_id,
                    candidate_index = ?self.route_index(),
                    path_kind = "direct_quic",
                    locator = %candidate.endpoint,
                    local_iroh_endpoint_id = %endpoint_id,
                    remote_iroh_endpoint_id = %remote_iroh_endpoint_id,
                    target_node_id = %target_node_id,
                    setup_elapsed_us = duration_as_u64_micros(setup_started.elapsed()),
                    remaining_setup_budget_ms = remaining_setup_budget.as_millis(),
                    "iroh_connect_scheduled"
                );
                tracing::info!(
                    target: MOBILE_CONNECTION_LOG_TARGET,
                    event = "iroh_connect_started",
                    setup_attempt_id,
                    candidate_index = ?self.route_index(),
                    path_kind = "direct_quic",
                    locator = %candidate.endpoint,
                    local_iroh_endpoint_id = %endpoint_id,
                    remote_iroh_endpoint_id = %remote_iroh_endpoint_id,
                    target_node_id = %target_node_id,
                    setup_elapsed_us = duration_as_u64_micros(setup_started.elapsed()),
                    remaining_setup_budget_ms = remaining_setup_budget.as_millis(),
                    "iroh_connect_started"
                );
                match endpoint
                    .connect_session(candidate, MultiplexConfig::default())
                    .await
                {
                    Ok(session) => {
                        tracing::info!(
                            target: MOBILE_CONNECTION_LOG_TARGET,
                            event = "iroh_connect_completed",
                            setup_attempt_id,
                            candidate_index = ?self.route_index(),
                            path_kind = "direct_quic",
                            locator = %candidate.endpoint,
                            local_iroh_endpoint_id = %endpoint_id,
                            remote_iroh_endpoint_id = %remote_iroh_endpoint_id,
                            target_node_id = %target_node_id,
                            duration_us = duration_as_u64_micros(connect_started.elapsed()),
                            "iroh_connect_completed"
                        );
                        Ok(session)
                    }
                    Err(error) => {
                        tracing::warn!(
                            target: MOBILE_CONNECTION_LOG_TARGET,
                            event = "iroh_connect_failed",
                            setup_attempt_id,
                            candidate_index = ?self.route_index(),
                            path_kind = "direct_quic",
                            locator = %candidate.endpoint,
                            local_iroh_endpoint_id = %endpoint_id,
                            remote_iroh_endpoint_id = %remote_iroh_endpoint_id,
                            target_node_id = %target_node_id,
                            duration_us = duration_as_u64_micros(connect_started.elapsed()),
                            reason = "connect_error",
                            error = %error,
                            "iroh_connect_failed"
                        );
                        Err(error).with_context(|| {
                            format!(
                                "failed opening direct QUIC transport session to {target_label}"
                            )
                        })
                    }
                }
            })
            .await;

        match direct_quic_result {
            Ok(result) => result,
            Err(_) => {
                let failed_stage = if let (Some(connect_started), Some(endpoint_id)) =
                    (iroh_connect_started, local_iroh_endpoint_id.as_deref())
                {
                    tracing::warn!(
                        target: MOBILE_CONNECTION_LOG_TARGET,
                        event = "iroh_connect_failed",
                        setup_attempt_id,
                        candidate_index = ?self.route_index(),
                        path_kind = "direct_quic",
                        locator = %candidate.endpoint,
                        local_iroh_endpoint_id = endpoint_id,
                        remote_iroh_endpoint_id = %remote_iroh_endpoint_id,
                        target_node_id = %target_node_id,
                        duration_us = duration_as_u64_micros(connect_started.elapsed()),
                        reason = "cold_session_setup_timeout",
                        timeout_ms = COLD_DIRECT_QUIC_SESSION_SETUP_TIMEOUT.as_millis(),
                        "iroh_connect_failed"
                    );
                    "iroh_connect"
                } else {
                    "endpoint_setup"
                };
                Err(ColdDirectQuicSessionSetupTimeout {
                    target_label,
                    stage: failed_stage,
                }
                .into())
            }
        }
    }

    fn spawn_hole_punching_monitor(&self, connection: Connection) {
        let stats = Arc::clone(&self.stats);
        tokio::spawn(async move {
            let mut paths = connection.paths_stream();
            while let Some(paths) = paths.next().await {
                stats
                    .direct_connection_mode
                    .store(direct_connection_mode_from_paths(&paths), Ordering::Relaxed);
            }
        });
    }

    pub(crate) async fn ensure_relay_session(
        &self,
        source: PeerIdentity,
        connection_name: Option<&str>,
    ) -> Result<Arc<MultiplexedSession>> {
        let SessionPoolTarget::Relay {
            rendezvous,
            target_node_id,
            source_security,
        } = &self.target
        else {
            bail!("attempted to open a relay session from a direct transport session pool");
        };

        if let Some(session) = self.cached_session().await {
            return Ok(session);
        }
        let _setup_permit = self.acquire_session_setup_permit().await;
        if let Some(session) = self.cached_session().await {
            return Ok(session);
        }

        let connect_started = Instant::now();

        tracing::info!(
            target: MOBILE_CONNECTION_LOG_TARGET,
            event = "relay_pairing_started",
            candidate_index = ?self.route_index(),
            path_kind = "relay_tunnel",
            locator = %rendezvous.config().rendezvous_urls.first().map(String::as_str).unwrap_or("rendezvous"),
            target_node_id = %target_node_id,
            "relay_pairing_started"
        );

        let ticket_started = Instant::now();
        tracing::info!(
            target: MOBILE_CONNECTION_LOG_TARGET,
            event = "iroh_relay_ticket_started",
            candidate_index = ?self.route_index(),
            path_kind = "relay_tunnel",
            target_node_id = %target_node_id,
            timeout_ms = RELAY_TICKET_REQUEST_TIMEOUT.as_millis(),
            "iroh_relay_ticket_started"
        );
        let ticket = match tokio::time::timeout(
            RELAY_TICKET_REQUEST_TIMEOUT,
            rendezvous.issue_relay_ticket(&RelayTicketRequest {
                cluster_id: rendezvous.config().cluster_id,
                source: source.clone(),
                target: PeerIdentity::Node(*target_node_id),
                session_kind: RelayTunnelSessionKind::MultiplexTransport,
                security_mode: transport_sdk::RelayTunnelSecurityMode::InnerMtls,
                requested_expires_in_secs: Some(300),
            }),
        )
        .await
        {
            Ok(Ok(ticket)) => {
                tracing::info!(
                    target: MOBILE_CONNECTION_LOG_TARGET,
                    event = "iroh_relay_ticket_completed",
                    candidate_index = ?self.route_index(),
                    path_kind = "relay_tunnel",
                    target_node_id = %target_node_id,
                    duration_us = duration_as_u64_micros(ticket_started.elapsed()),
                    "iroh_relay_ticket_completed"
                );
                ticket
            }
            Ok(Err(error)) => {
                tracing::warn!(
                    target: MOBILE_CONNECTION_LOG_TARGET,
                    event = "iroh_relay_ticket_failed",
                    candidate_index = ?self.route_index(),
                    path_kind = "relay_tunnel",
                    target_node_id = %target_node_id,
                    duration_us = duration_as_u64_micros(ticket_started.elapsed()),
                    error = %error,
                    "iroh_relay_ticket_failed"
                );
                tracing::warn!(
                    target: MOBILE_CONNECTION_LOG_TARGET,
                    event = "relay_pairing_failed",
                    candidate_index = ?self.route_index(),
                    path_kind = "relay_tunnel",
                    locator = %rendezvous.config().rendezvous_urls.first().map(String::as_str).unwrap_or("rendezvous"),
                    target_node_id = %target_node_id,
                    error = %error,
                    "relay_pairing_failed"
                );
                return Err(error).with_context(|| {
                    format!(
                        "failed issuing multiplex relay ticket for client target node {}",
                        target_node_id
                    )
                });
            }
            Err(_) => {
                let error = anyhow!(
                    "issuing multiplex relay ticket for client target node {} timed out after {:?}",
                    target_node_id,
                    RELAY_TICKET_REQUEST_TIMEOUT
                );
                tracing::warn!(
                    target: MOBILE_CONNECTION_LOG_TARGET,
                    event = "iroh_relay_ticket_failed",
                    candidate_index = ?self.route_index(),
                    path_kind = "relay_tunnel",
                    target_node_id = %target_node_id,
                    duration_us = duration_as_u64_micros(ticket_started.elapsed()),
                    timeout_ms = RELAY_TICKET_REQUEST_TIMEOUT.as_millis(),
                    reason = "timeout",
                    error = %error,
                    "iroh_relay_ticket_failed"
                );
                return Err(error);
            }
        };
        let relay_tunnel = match rendezvous.connect_relay_tunnel_source(&ticket).await {
            Ok(tunnel) => tunnel,
            Err(error) => {
                tracing::warn!(
                    target: MOBILE_CONNECTION_LOG_TARGET,
                    event = "relay_pairing_failed",
                    candidate_index = ?self.route_index(),
                    path_kind = "relay_tunnel",
                    locator = %rendezvous.config().rendezvous_urls.first().map(String::as_str).unwrap_or("rendezvous"),
                    target_node_id = %target_node_id,
                    error = %error,
                    "relay_pairing_failed"
                );
                return Err(error).with_context(|| {
                    format!(
                        "failed opening relay tunnel source for client target node {}",
                        target_node_id
                    )
                });
            }
        };
        let relay_pairing_duration_us = relay_tunnel
            .pairing_timing()
            .map(|timing| timing.relay_pairing_duration_us)
            .unwrap_or_default();
        tracing::info!(
            target: MOBILE_CONNECTION_LOG_TARGET,
            event = "relay_pairing_completed",
            candidate_index = ?self.route_index(),
            path_kind = "relay_tunnel",
            locator = %rendezvous.config().rendezvous_urls.first().map(String::as_str).unwrap_or("rendezvous"),
            target_node_id = %target_node_id,
            pairing_duration_us = relay_pairing_duration_us,
            total_duration_us = duration_as_u64_micros(connect_started.elapsed()),
            "relay_pairing_completed"
        );
        let (relay_session, multiplexed) = relay_tunnel
            .into_secure_multiplexed_source_session(
                source_security.clone(),
                MultiplexConfig::default(),
            )
            .await
            .with_context(|| {
                format!(
                    "failed establishing inner mTLS relay session for client target node {}",
                    target_node_id
                )
            })?;

        perform_transport_client_handshake(
            &multiplexed,
            TransportSessionControlMessage::Hello {
                protocol_version: TRANSPORT_PROTOCOL_VERSION,
                cluster_id: rendezvous.config().cluster_id,
                role: relay_session_role_for_source(&source),
                peer: source,
                connection_name: connection_name.map(ToString::to_string),
                target: Some(PeerIdentity::Node(*target_node_id)),
            },
        )
        .await
        .with_context(|| {
            format!(
                "failed performing multiplex relay transport handshake for target node {}",
                target_node_id
            )
        })?;

        let session = Arc::new(multiplexed);
        let (session, inserted) = self.cache_session(session, Some(relay_session)).await;
        if inserted {
            self.stats.connect_count.fetch_add(1, Ordering::Relaxed);
            self.stats.connect_duration_us.fetch_add(
                duration_as_u64_micros(connect_started.elapsed()),
                Ordering::Relaxed,
            );
            self.stats
                .relay_pairing_duration_us
                .fetch_add(relay_pairing_duration_us, Ordering::Relaxed);
        }
        Ok(session)
    }

    pub(crate) async fn has_cached_session(&self) -> bool {
        self.cached_session.lock().await.is_some()
    }

    fn log_direct_quic_session_setup_started(&self, setup_attempt_id: u64) {
        let SessionPoolTarget::DirectQuic {
            candidate,
            target_node_id,
            ..
        } = &self.target
        else {
            return;
        };
        tracing::info!(
            target: MOBILE_CONNECTION_LOG_TARGET,
            event = "iroh_session_setup_started",
            setup_attempt_id,
            candidate_index = ?self.route_index(),
            path_kind = "direct_quic",
            locator = %candidate.endpoint,
            target_node_id = ?target_node_id,
            timeout_ms = COLD_DIRECT_QUIC_SESSION_SETUP_TIMEOUT.as_millis(),
            "iroh_session_setup_started"
        );
    }

    fn log_direct_quic_session_setup_finished(
        &self,
        setup_attempt_id: u64,
        duration: Duration,
        result: &Result<Arc<MultiplexedSession>>,
    ) {
        let SessionPoolTarget::DirectQuic {
            candidate,
            target_node_id,
            ..
        } = &self.target
        else {
            return;
        };
        match result {
            Ok(_) => tracing::info!(
                target: MOBILE_CONNECTION_LOG_TARGET,
                event = "iroh_session_setup_completed",
                setup_attempt_id,
                candidate_index = ?self.route_index(),
                path_kind = "direct_quic",
                locator = %candidate.endpoint,
                target_node_id = ?target_node_id,
                duration_us = duration_as_u64_micros(duration),
                "iroh_session_setup_completed"
            ),
            Err(error) => {
                let timeout = error
                    .chain()
                    .find_map(|cause| cause.downcast_ref::<ColdDirectQuicSessionSetupTimeout>());
                tracing::warn!(
                    target: MOBILE_CONNECTION_LOG_TARGET,
                    event = "iroh_session_setup_failed",
                    setup_attempt_id,
                    candidate_index = ?self.route_index(),
                    path_kind = "direct_quic",
                    locator = %candidate.endpoint,
                    target_node_id = ?target_node_id,
                    duration_us = duration_as_u64_micros(duration),
                    reason = if timeout.is_some() { "timeout" } else { "error" },
                    failed_stage = ?timeout.map(|timeout| timeout.stage),
                    error = %error,
                    "iroh_session_setup_failed"
                );
            }
        }
    }

    fn direct_quic_setup_wait_guard(
        &self,
        setup_attempt_id: u64,
        waiter: DirectQuicSetupWaiter,
    ) -> Option<DirectQuicSetupWaitGuard> {
        let SessionPoolTarget::DirectQuic {
            candidate,
            target_node_id,
            ..
        } = &self.target
        else {
            return None;
        };
        Some(DirectQuicSetupWaitGuard {
            setup_attempt_id,
            candidate_index: self.route_index(),
            locator: candidate.endpoint.clone(),
            target_node_id: *target_node_id,
            coordinator: Arc::clone(&self.direct_quic_setup),
            waiter,
            completed: false,
        })
    }

    async fn wait_for_shared_direct_quic_setup<F, Fut>(
        &self,
        waiter: DirectQuicSetupWaiter,
        setup: F,
    ) -> Result<Arc<MultiplexedSession>>
    where
        F: FnOnce(u64) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<Arc<MultiplexedSession>>> + Send + 'static,
    {
        let (setup_attempt_id, mut receiver) = {
            let mut active = self.direct_quic_setup.active.lock().await;
            if let Some(existing) = active.as_ref() {
                (existing.attempt_id, existing.receiver.clone())
            } else {
                let attempt_id = self
                    .direct_quic_setup
                    .next_attempt_id
                    .fetch_add(1, Ordering::Relaxed)
                    .saturating_add(1);
                let (sender, receiver) = watch::channel(None);
                *active = Some(SharedSessionSetup {
                    attempt_id,
                    receiver: receiver.clone(),
                });
                self.direct_quic_setup
                    .setup_active
                    .store(true, Ordering::Relaxed);

                let coordinator = Arc::clone(&self.direct_quic_setup);
                tokio::spawn(async move {
                    let result = setup(attempt_id)
                        .await
                        .map_err(|error| Arc::<str>::from(error.to_string()));
                    let _ = sender.send(Some(result));
                    let mut active = coordinator.active.lock().await;
                    if active
                        .as_ref()
                        .is_some_and(|current| current.attempt_id == attempt_id)
                    {
                        *active = None;
                        coordinator.setup_active.store(false, Ordering::Relaxed);
                    }
                });
                (attempt_id, receiver)
            }
        };

        let mut cancellation_guard = self.direct_quic_setup_wait_guard(setup_attempt_id, waiter);
        let result = loop {
            if let Some(result) = receiver.borrow().as_ref() {
                break result
                    .clone()
                    .map_err(|error| anyhow!(error.as_ref().to_string()));
            }
            if receiver.changed().await.is_err() {
                break Err(anyhow!(
                    "shared direct QUIC session setup task ended unexpectedly"
                ));
            }
        };
        if let Some(guard) = cancellation_guard.as_mut() {
            guard.complete();
        }
        result
    }

    async fn cached_session(&self) -> Option<Arc<MultiplexedSession>> {
        let guard = self.cached_session.lock().await;
        let session = guard.as_ref().map(|cached| Arc::clone(&cached.session));
        if session.is_some() {
            self.stats.reuse_count.fetch_add(1, Ordering::Relaxed);
        }
        session
    }

    async fn acquire_session_setup_permit(&self) -> tokio::sync::OwnedSemaphorePermit {
        Arc::clone(&self.session_setup_permit)
            .acquire_owned()
            .await
            .expect("transport session setup permit must not be closed")
    }

    async fn cache_session(
        &self,
        session: Arc<MultiplexedSession>,
        relay_session: Option<RelayTunnelSession>,
    ) -> (Arc<MultiplexedSession>, bool) {
        let mut guard = self.cached_session.lock().await;
        if let Some(existing) = guard.as_ref() {
            self.stats.reuse_count.fetch_add(1, Ordering::Relaxed);
            return (Arc::clone(&existing.session), false);
        }

        *guard = Some(CachedTransportSession {
            session: Arc::clone(&session),
            _relay_session: relay_session,
        });
        (session, true)
    }
}

fn duration_as_u64_micros(duration: std::time::Duration) -> u64 {
    duration.as_micros().try_into().unwrap_or(u64::MAX)
}

fn relay_session_role_for_source(source: &PeerIdentity) -> TransportSessionRole {
    match source {
        PeerIdentity::Node(_) => TransportSessionRole::Node,
        PeerIdentity::Device(_) => TransportSessionRole::Client,
    }
}

fn direct_connection_mode_from_paths(paths: &PathList<'_>) -> u64 {
    paths
        .iter()
        .find(|path| path.is_selected())
        .map(|path| {
            if path.is_ip() {
                DIRECT_CONNECTION_MODE_DIRECT
            } else if path.is_relay() {
                DIRECT_CONNECTION_MODE_RELAY
            } else {
                DIRECT_CONNECTION_MODE_UNKNOWN
            }
        })
        .unwrap_or(DIRECT_CONNECTION_MODE_UNKNOWN)
}

async fn ensure_direct_quic_endpoint(
    context: &DirectQuicConnectContext<'_>,
    route_index: Option<usize>,
) -> Result<DirectQuicEndpoint> {
    let candidate = context.candidate;
    let target_node_id = context.target_node_id;
    let endpoint = context.endpoint;
    let cached_session = context.cached_session;
    let stats = context.stats;
    let rendezvous = context.rendezvous;
    let relay_ca_pem = context.relay_ca_pem;
    let setup_attempt_id = context.setup_attempt_id;
    {
        let guard = endpoint.lock().await;
        if let Some(managed) = guard.as_ref() {
            return Ok(managed.endpoint.clone());
        }
    }

    let has_usable_peer_addresses = has_usable_peer_addresses(candidate)?;
    let secret_key = SecretKey::generate();
    let local_iroh_endpoint_id = secret_key.public().to_string();
    let remote_iroh_endpoint_id = endpoint_id_from_candidate(candidate)?;
    let relay_ticket_collection = match rendezvous {
        Some(rendezvous) => {
            let ticket_started = Instant::now();
            tracing::info!(
                target: MOBILE_CONNECTION_LOG_TARGET,
                event = "iroh_relay_ticket_started",
                setup_attempt_id,
                candidate_index = ?route_index,
                path_kind = "direct_quic",
                locator = %candidate.endpoint,
                local_iroh_endpoint_id = %local_iroh_endpoint_id,
                remote_iroh_endpoint_id = %remote_iroh_endpoint_id,
                target_node_id = %target_node_id,
                relay_scope = "endpoint_bound",
                rendezvous_urls = ?rendezvous.config().rendezvous_urls,
                timeout_ms = RELAY_TICKET_REQUEST_TIMEOUT.as_millis(),
                "iroh_relay_ticket_started"
            );
            match tokio::time::timeout(
                RELAY_TICKET_REQUEST_TIMEOUT,
                rendezvous.issue_iroh_relay_tickets_progressively(&local_iroh_endpoint_id),
            )
            .await
            {
                Ok(Ok(collection)) => {
                    tracing::info!(
                        target: MOBILE_CONNECTION_LOG_TARGET,
                        event = "iroh_relay_ticket_completed",
                        setup_attempt_id,
                        candidate_index = ?route_index,
                        path_kind = "direct_quic",
                        locator = %candidate.endpoint,
                        local_iroh_endpoint_id = %local_iroh_endpoint_id,
                        remote_iroh_endpoint_id = %remote_iroh_endpoint_id,
                        target_node_id = %target_node_id,
                        relay_scope = "endpoint_bound",
                        relay_urls = ?collection.first_ticket().public_urls,
                        duration_us = duration_as_u64_micros(ticket_started.elapsed()),
                        "iroh_relay_ticket_completed"
                    );
                    Some(collection)
                }
                Ok(Err(error)) => {
                    tracing::warn!(
                        target: MOBILE_CONNECTION_LOG_TARGET,
                        event = "iroh_relay_ticket_failed",
                        setup_attempt_id,
                        candidate_index = ?route_index,
                        path_kind = "direct_quic",
                        locator = %candidate.endpoint,
                        local_iroh_endpoint_id = %local_iroh_endpoint_id,
                        remote_iroh_endpoint_id = %remote_iroh_endpoint_id,
                        target_node_id = %target_node_id,
                        relay_scope = "endpoint_bound",
                        error_class = "request",
                        duration_us = duration_as_u64_micros(ticket_started.elapsed()),
                        error = %error,
                        "iroh_relay_ticket_failed"
                    );
                    None
                }
                Err(_) => {
                    tracing::warn!(
                        target: MOBILE_CONNECTION_LOG_TARGET,
                        event = "iroh_relay_ticket_failed",
                        setup_attempt_id,
                        candidate_index = ?route_index,
                        path_kind = "direct_quic",
                        locator = %candidate.endpoint,
                        local_iroh_endpoint_id = %local_iroh_endpoint_id,
                        remote_iroh_endpoint_id = %remote_iroh_endpoint_id,
                        target_node_id = %target_node_id,
                        relay_scope = "endpoint_bound",
                        error_class = "timeout",
                        duration_us = duration_as_u64_micros(ticket_started.elapsed()),
                        timeout_ms = RELAY_TICKET_REQUEST_TIMEOUT.as_millis(),
                        reason = "timeout",
                        "iroh_relay_ticket_failed"
                    );
                    None
                }
            }
        }
        None => None,
    };

    let first_relay_ticket = relay_ticket_collection
        .as_ref()
        .map(|collection| collection.first_ticket().clone());

    let static_relay_authorized = rendezvous.is_none()
        && candidate
            .transport_hints
            .as_ref()
            .and_then(|hints| hints.relay_url.as_deref())
            .is_some()
        && candidate
            .transport_hints
            .as_ref()
            .and_then(|hints| hints.relay_auth_token.as_deref())
            .is_some_and(|token| !token.trim().is_empty());
    let relay_enabled = first_relay_ticket.is_some() || static_relay_authorized;
    if !relay_enabled && !has_usable_peer_addresses {
        tracing::warn!(
            target: MOBILE_CONNECTION_LOG_TARGET,
            event = "iroh_endpoint_setup_failed",
            setup_attempt_id,
            candidate_index = ?route_index,
            path_kind = "direct_quic",
            locator = %candidate.endpoint,
            local_iroh_endpoint_id = %local_iroh_endpoint_id,
            remote_iroh_endpoint_id = %remote_iroh_endpoint_id,
            target_node_id = %target_node_id,
            reason = "no_relay_ticket_or_usable_peer_addresses",
            "iroh_endpoint_setup_failed"
        );
        bail!("direct QUIC unavailable: no relay ticket and no usable peer addresses");
    }
    if !relay_enabled {
        tracing::warn!(
            target: MOBILE_CONNECTION_LOG_TARGET,
            event = "iroh_direct_only_fallback",
            setup_attempt_id,
            candidate_index = ?route_index,
            path_kind = "direct_quic",
            locator = %candidate.endpoint,
            local_iroh_endpoint_id = %local_iroh_endpoint_id,
            remote_iroh_endpoint_id = %remote_iroh_endpoint_id,
            target_node_id = %target_node_id,
            relay_scope = "disabled",
            usable_peer_addresses = has_usable_peer_addresses,
            "continuing with direct-only QUIC because no relay ticket is available"
        );
    }

    let initial_relay_tickets = first_relay_ticket.as_ref().map(|ticket| {
        let mut tickets = IrohRelayTicketSet::default();
        tickets.insert(ticket);
        tickets
    });

    let mut config = DirectQuicEndpointConfig::new(secret_key);
    config.relay_enabled = relay_enabled;
    config.alpn = candidate
        .transport_hints
        .as_ref()
        .and_then(|hints| hints.alpn.clone())
        .unwrap_or_else(|| DEFAULT_DIRECT_QUIC_ALPN.to_string());
    if static_relay_authorized {
        if let Some(relay_url) = candidate
            .transport_hints
            .as_ref()
            .and_then(|hints| hints.relay_url.clone())
        {
            config.relay_urls.push(relay_url);
        }
        config.relay_auth_token = candidate
            .transport_hints
            .as_ref()
            .and_then(|hints| hints.relay_auth_token.clone());
    }
    config.relay_ca_pem = relay_ca_pem.map(ToString::to_string);
    config.initial_dynamic_relays = initial_relay_tickets
        .as_ref()
        .map(IrohRelayTicketSet::relay_configs)
        .unwrap_or_default();

    let bound_endpoint = DirectQuicEndpoint::bind(config.clone())
        .await
        .with_context(|| {
            format!(
                "failed binding local direct QUIC endpoint for remote candidate {}",
                candidate.endpoint
            )
        })?;
    let endpoint_snapshot = bound_endpoint.snapshot();
    if endpoint_snapshot.endpoint_id != local_iroh_endpoint_id {
        bail!("direct QUIC endpoint id changed while installing an endpoint-bound relay ticket");
    }
    tracing::info!(
        target: MOBILE_CONNECTION_LOG_TARGET,
        event = "iroh_endpoint_created",
        setup_attempt_id,
        candidate_index = ?route_index,
        path_kind = "direct_quic",
        locator = %candidate.endpoint,
        local_iroh_endpoint_id = %endpoint_snapshot.endpoint_id,
        remote_iroh_endpoint_id = %remote_iroh_endpoint_id,
        target_node_id = %target_node_id,
        relay_enabled = relay_enabled,
        relay_url = ?endpoint_snapshot.relay_url,
        direct_socket_addrs = ?endpoint_snapshot.direct_socket_addrs,
        "iroh_endpoint_created"
    );
    let relay_ticket_refresh = match (rendezvous, relay_ticket_collection, initial_relay_tickets) {
        (Some(rendezvous), Some(collection), Some(tickets)) => {
            let lifecycle = DirectQuicEndpointLifecycle {
                endpoint_slot: endpoint.clone(),
                cached_session: cached_session.clone(),
                stats: stats.clone(),
                endpoint_config: config,
                rendezvous: rendezvous.clone(),
                local_iroh_endpoint_id,
                remote_iroh_endpoint_id,
                target_node_id,
            };
            Some(spawn_iroh_relay_ticket_refresh(
                bound_endpoint.clone(),
                lifecycle,
                tickets,
                Some(collection),
            ))
        }
        _ => None,
    };
    let managed_endpoint = ManagedDirectQuicEndpoint {
        endpoint: bound_endpoint.clone(),
        relay_ticket_refresh,
    };
    let mut guard = endpoint.lock().await;
    if let Some(existing) = guard.as_ref() {
        return Ok(existing.endpoint.clone());
    }
    *guard = Some(managed_endpoint);
    Ok(bound_endpoint)
}

fn remaining_cold_quic_setup_budget(setup_started: Instant) -> Result<Duration> {
    COLD_DIRECT_QUIC_SESSION_SETUP_TIMEOUT
        .checked_sub(setup_started.elapsed())
        .ok_or_else(|| {
            anyhow!(
                "cold direct QUIC session setup exceeded its {:?} time budget",
                COLD_DIRECT_QUIC_SESSION_SETUP_TIMEOUT
            )
        })
}

fn spawn_iroh_relay_ticket_refresh(
    endpoint: DirectQuicEndpoint,
    lifecycle: DirectQuicEndpointLifecycle,
    mut relay_tickets: IrohRelayTicketSet,
    initial_collection: Option<IrohRelayTicketCollection>,
) -> tokio::task::AbortHandle {
    let task = tokio::spawn(async move {
        let mut pending_collection = initial_collection;
        let rollover = IrohRelayTicketRollover::from_ticket_set(&relay_tickets, unix_ts());
        loop {
            let refresh_delay = iroh_relay_ticket_rollover_delay(rollover, unix_ts());
            if let Some(collection) = pending_collection.as_mut() {
                tokio::select! {
                    ticket = collection.next_ticket() => match ticket {
                        Some(Ok(ticket)) => {
                            if let Err(error) = install_iroh_relay_ticket(
                                &endpoint,
                                &mut relay_tickets,
                                &ticket,
                            ).await {
                                tracing::warn!(
                                    %error,
                                    local_iroh_endpoint_id = %lifecycle.local_iroh_endpoint_id,
                                    remote_iroh_endpoint_id = %lifecycle.remote_iroh_endpoint_id,
                                    target_node_id = %lifecycle.target_node_id,
                                    "failed applying additional endpoint-bound iroh relay ticket"
                                );
                            } else {
                                tracing::info!(
                                    target: MOBILE_CONNECTION_LOG_TARGET,
                                    event = "iroh_relay_ticket_additional_completed",
                                    local_iroh_endpoint_id = %lifecycle.local_iroh_endpoint_id,
                                    remote_iroh_endpoint_id = %lifecycle.remote_iroh_endpoint_id,
                                    target_node_id = %lifecycle.target_node_id,
                                    relay_scope = "endpoint_bound",
                                    relay_urls = ?ticket.public_urls,
                                    "iroh_relay_ticket_additional_completed"
                                );
                            }
                            if rollover.is_some_and(|rollover| {
                                rollover.replacement_is_due(&relay_tickets, unix_ts())
                            }) {
                                match rollover_direct_quic_endpoint(
                                    &lifecycle,
                                    relay_tickets.clone(),
                                ).await {
                                    Ok(()) => return,
                                    Err(error) => tracing::warn!(
                                        %error,
                                        local_iroh_endpoint_id = %lifecycle.local_iroh_endpoint_id,
                                        remote_iroh_endpoint_id = %lifecycle.remote_iroh_endpoint_id,
                                        target_node_id = %lifecycle.target_node_id,
                                        "failed rotating endpoint-bound iroh relay ticket"
                                    ),
                                }
                            }
                        }
                        Some(Err(error)) => {
                            tracing::debug!(
                                %error,
                                local_iroh_endpoint_id = %lifecycle.local_iroh_endpoint_id,
                                remote_iroh_endpoint_id = %lifecycle.remote_iroh_endpoint_id,
                                target_node_id = %lifecycle.target_node_id,
                                "additional endpoint-bound iroh relay ticket request failed"
                            );
                        }
                        None => pending_collection = None,
                    },
                    _ = tokio::time::sleep(refresh_delay) => {
                        pending_collection = request_iroh_relay_ticket_collection(&lifecycle).await;
                        let first_ticket = pending_collection
                            .as_ref()
                            .map(|collection| collection.first_ticket().clone());
                        if let Some(ticket) = first_ticket {
                            if let Err(error) = install_iroh_relay_ticket(
                                &endpoint,
                                &mut relay_tickets,
                                &ticket,
                            ).await {
                                tracing::warn!(
                                    %error,
                                    local_iroh_endpoint_id = %lifecycle.local_iroh_endpoint_id,
                                    remote_iroh_endpoint_id = %lifecycle.remote_iroh_endpoint_id,
                                    target_node_id = %lifecycle.target_node_id,
                                    "failed applying refreshed endpoint-bound iroh relay ticket"
                                );
                                pending_collection = None;
                            } else if rollover.is_some_and(|rollover| {
                                rollover.replacement_is_due(&relay_tickets, unix_ts())
                            }) {
                                match rollover_direct_quic_endpoint(
                                    &lifecycle,
                                    relay_tickets.clone(),
                                ).await {
                                    Ok(()) => return,
                                    Err(error) => tracing::warn!(
                                        %error,
                                        local_iroh_endpoint_id = %lifecycle.local_iroh_endpoint_id,
                                        remote_iroh_endpoint_id = %lifecycle.remote_iroh_endpoint_id,
                                        target_node_id = %lifecycle.target_node_id,
                                        "failed rotating endpoint-bound iroh relay ticket"
                                    ),
                                }
                            }
                        } else {
                            tokio::time::sleep(Duration::from_secs(30)).await;
                        }
                    }
                }
            } else {
                tokio::time::sleep(refresh_delay).await;
                pending_collection = request_iroh_relay_ticket_collection(&lifecycle).await;
                if pending_collection.is_none() {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    continue;
                }
                let first_ticket = pending_collection
                    .as_ref()
                    .map(|collection| collection.first_ticket().clone());
                if let Some(ticket) = first_ticket {
                    if let Err(error) =
                        install_iroh_relay_ticket(&endpoint, &mut relay_tickets, &ticket).await
                    {
                        tracing::warn!(
                            %error,
                            local_iroh_endpoint_id = %lifecycle.local_iroh_endpoint_id,
                            remote_iroh_endpoint_id = %lifecycle.remote_iroh_endpoint_id,
                            target_node_id = %lifecycle.target_node_id,
                            "failed applying refreshed endpoint-bound iroh relay ticket"
                        );
                        pending_collection = None;
                    } else if rollover.is_some_and(|rollover| {
                        rollover.replacement_is_due(&relay_tickets, unix_ts())
                    }) {
                        match rollover_direct_quic_endpoint(&lifecycle, relay_tickets.clone()).await
                        {
                            Ok(()) => return,
                            Err(error) => tracing::warn!(
                                %error,
                                local_iroh_endpoint_id = %lifecycle.local_iroh_endpoint_id,
                                remote_iroh_endpoint_id = %lifecycle.remote_iroh_endpoint_id,
                                target_node_id = %lifecycle.target_node_id,
                                "failed rotating endpoint-bound iroh relay ticket"
                            ),
                        }
                    }
                }
            }
        }
    });
    task.abort_handle()
}

async fn request_iroh_relay_ticket_collection(
    lifecycle: &DirectQuicEndpointLifecycle,
) -> Option<IrohRelayTicketCollection> {
    match lifecycle
        .rendezvous
        .issue_iroh_relay_tickets_progressively(&lifecycle.local_iroh_endpoint_id)
        .await
    {
        Ok(collection) => Some(collection),
        Err(error) => {
            tracing::warn!(
                %error,
                local_iroh_endpoint_id = %lifecycle.local_iroh_endpoint_id,
                remote_iroh_endpoint_id = %lifecycle.remote_iroh_endpoint_id,
                target_node_id = %lifecycle.target_node_id,
                "failed refreshing endpoint-bound iroh relay ticket"
            );
            None
        }
    }
}

async fn install_iroh_relay_ticket(
    endpoint: &DirectQuicEndpoint,
    relay_tickets: &mut IrohRelayTicketSet,
    ticket: &IrohRelayTicket,
) -> Result<()> {
    let mut updated_tickets = relay_tickets.clone();
    updated_tickets.insert(ticket);
    endpoint
        .reconcile_dynamic_relays(&updated_tickets.relay_configs())
        .await?;
    *relay_tickets = updated_tickets;
    Ok(())
}

async fn rollover_direct_quic_endpoint(
    lifecycle: &DirectQuicEndpointLifecycle,
    relay_tickets: IrohRelayTicketSet,
) -> Result<()> {
    let mut next_config = lifecycle.endpoint_config.clone();
    next_config.initial_dynamic_relays = relay_tickets.relay_configs();
    let relay_count = next_config.initial_dynamic_relays.len();
    let next_endpoint = DirectQuicEndpoint::bind(next_config.clone())
        .await
        .context("failed binding replacement direct QUIC endpoint")?;
    if next_endpoint.endpoint_id() != lifecycle.local_iroh_endpoint_id {
        bail!("direct QUIC endpoint id changed while rotating relay tickets");
    }

    let next_lifecycle = DirectQuicEndpointLifecycle {
        endpoint_config: next_config,
        ..lifecycle.clone()
    };
    let refresh =
        spawn_iroh_relay_ticket_refresh(next_endpoint.clone(), next_lifecycle, relay_tickets, None);
    let previous = {
        let mut endpoint = lifecycle.endpoint_slot.lock().await;
        let Some(current) = endpoint.as_ref() else {
            refresh.abort();
            return Ok(());
        };
        if current.endpoint.endpoint_id() != lifecycle.local_iroh_endpoint_id {
            refresh.abort();
            return Ok(());
        }
        endpoint.replace(ManagedDirectQuicEndpoint {
            endpoint: next_endpoint.clone(),
            relay_ticket_refresh: Some(refresh),
        })
    };

    if lifecycle.cached_session.lock().await.take().is_some() {
        lifecycle.stats.reset_count.fetch_add(1, Ordering::Relaxed);
        lifecycle
            .stats
            .direct_connection_mode
            .store(DIRECT_CONNECTION_MODE_UNKNOWN, Ordering::Relaxed);
    }
    if let Some(previous) = previous {
        previous.endpoint.close().await;
    }
    tracing::info!(
        target: MOBILE_CONNECTION_LOG_TARGET,
        event = "iroh_endpoint_ticket_rollover_completed",
        local_iroh_endpoint_id = %lifecycle.local_iroh_endpoint_id,
        remote_iroh_endpoint_id = %lifecycle.remote_iroh_endpoint_id,
        target_node_id = %lifecycle.target_node_id,
        relay_count,
        "iroh_endpoint_ticket_rollover_completed"
    );
    Ok(())
}

fn iroh_relay_ticket_rollover_delay(
    rollover: Option<IrohRelayTicketRollover>,
    now_unix: u64,
) -> Duration {
    let Some(rollover) = rollover else {
        return Duration::from_secs(30);
    };
    Duration::from_secs(rollover.rollover_at_unix().saturating_sub(now_unix).max(1))
        .min(IROH_RELAY_TICKET_REFRESH_MAX_INTERVAL)
}

fn websocket_auth_headers(
    identity: &ClientIdentityMaterial,
    connection_name: Option<&str>,
) -> Result<Vec<(String, String)>> {
    let signed_headers =
        build_signed_request_headers(identity, "GET", "/transport/ws", unix_ts(), None)?;
    let mut headers = vec![
        (
            transport_sdk::HEADER_CLUSTER_ID.to_string(),
            signed_headers.cluster_id.to_string(),
        ),
        (
            transport_sdk::HEADER_DEVICE_ID.to_string(),
            signed_headers.device_id,
        ),
        (
            transport_sdk::HEADER_CREDENTIAL_FINGERPRINT.to_string(),
            signed_headers.credential_fingerprint,
        ),
        (
            transport_sdk::HEADER_AUTH_TIMESTAMP.to_string(),
            signed_headers.timestamp_unix.to_string(),
        ),
        (
            transport_sdk::HEADER_AUTH_NONCE.to_string(),
            signed_headers.nonce,
        ),
        (
            transport_sdk::HEADER_AUTH_SIGNATURE.to_string(),
            signed_headers.signature_base64,
        ),
    ];
    if let Some(connection_name) = connection_name {
        headers.push((
            transport_sdk::HEADER_CONNECTION_NAME.to_string(),
            connection_name.to_string(),
        ));
    }
    Ok(headers)
}

fn unix_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use transport_sdk::candidates::ConnectionCandidateTransportHints;

    #[tokio::test]
    async fn direct_quic_without_ticket_or_peer_addresses_fails_before_connecting() {
        let remote_endpoint_id = SecretKey::generate().public().to_string();
        let candidate = ConnectionCandidate {
            kind: transport_sdk::CandidateKind::DirectQuic,
            endpoint: format!("iroh://{remote_endpoint_id}"),
            rtt_ms: None,
            transport_hints: Some(ConnectionCandidateTransportHints {
                transport_id: Some(remote_endpoint_id),
                relay_url: Some("https://relay.example".to_string()),
                relay_auth_token: None,
                alpn: None,
                direct_socket_addrs: vec!["0.0.0.0:28080".to_string(), "[::]:28080".to_string()],
                observed_socket_addrs: Vec::new(),
            }),
        };
        let endpoint = Arc::new(Mutex::new(None));
        let cached_session = Arc::new(Mutex::new(None));
        let stats = Arc::new(TransportSessionPoolStats::default());
        let context = DirectQuicConnectContext {
            candidate: &candidate,
            target_node_id: NodeId::new_v4(),
            endpoint: &endpoint,
            cached_session: &cached_session,
            stats: &stats,
            rendezvous: None,
            relay_ca_pem: None,
            setup_attempt_id: 1,
            setup_started: Instant::now(),
        };

        let error = match ensure_direct_quic_endpoint(&context, None).await {
            Ok(_) => panic!("a direct-only fallback requires a usable remote address"),
            Err(error) => error,
        };

        assert!(
            error
                .to_string()
                .contains("direct QUIC unavailable: no relay ticket and no usable peer addresses")
        );
        assert!(endpoint.lock().await.is_none());
    }

    #[tokio::test]
    async fn ticket_timeout_without_peer_addresses_does_not_start_direct_quic() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("ticket listener should bind");
        let ticket_url = format!(
            "http://{}",
            listener
                .local_addr()
                .expect("ticket listener address should be available")
        );
        let request_started = Arc::new(AtomicU64::new(0));
        let request_started_for_server = Arc::clone(&request_started);
        let ticket_server = tokio::spawn(async move {
            let (_stream, _) = listener
                .accept()
                .await
                .expect("ticket listener should accept a connection");
            request_started_for_server.fetch_add(1, Ordering::SeqCst);
            std::future::pending::<()>().await;
        });

        let cluster_id = uuid::Uuid::now_v7();
        let rendezvous = RendezvousControlClient::new(
            transport_sdk::RendezvousClientConfig {
                cluster_id,
                rendezvous_urls: vec![ticket_url],
                heartbeat_interval_secs: 15,
            },
            None,
            None,
        )
        .expect("rendezvous client should build");
        let remote_endpoint_id = SecretKey::generate().public().to_string();
        let candidate = ConnectionCandidate {
            kind: transport_sdk::CandidateKind::DirectQuic,
            endpoint: format!("iroh://{remote_endpoint_id}"),
            rtt_ms: None,
            transport_hints: Some(ConnectionCandidateTransportHints {
                transport_id: Some(remote_endpoint_id),
                relay_url: None,
                relay_auth_token: None,
                alpn: None,
                direct_socket_addrs: vec!["0.0.0.0:28080".to_string(), "[::]:28080".to_string()],
                observed_socket_addrs: Vec::new(),
            }),
        };
        let endpoint = Arc::new(Mutex::new(None));
        let cached_session = Arc::new(Mutex::new(None));
        let stats = Arc::new(TransportSessionPoolStats::default());
        let context = DirectQuicConnectContext {
            candidate: &candidate,
            target_node_id: NodeId::new_v4(),
            endpoint: &endpoint,
            cached_session: &cached_session,
            stats: &stats,
            rendezvous: Some(&rendezvous),
            relay_ca_pem: None,
            setup_attempt_id: 1,
            setup_started: Instant::now(),
        };

        let error = match ensure_direct_quic_endpoint(&context, None).await {
            Ok(_) => panic!("ticket timeout without peers must not create an endpoint"),
            Err(error) => error,
        };

        ticket_server.abort();
        let _ = ticket_server.await;

        assert!(
            error
                .to_string()
                .contains("direct QUIC unavailable: no relay ticket and no usable peer addresses")
        );
        assert_eq!(request_started.load(Ordering::SeqCst), 1);
        assert!(endpoint.lock().await.is_none());
    }

    #[tokio::test]
    async fn cancelled_waiter_does_not_cancel_shared_direct_quic_setup() {
        let pool = TransportSessionPool::new_direct_quic(
            ConnectionCandidate {
                kind: transport_sdk::CandidateKind::DirectQuic,
                endpoint: format!("iroh://{}", SecretKey::generate().public()),
                rtt_ms: None,
                transport_hints: None,
            },
            Some(NodeId::new_v4()),
            None,
            None,
        );
        let starts = Arc::new(AtomicU64::new(0));
        let first_starts = Arc::clone(&starts);
        let first = tokio::time::timeout(
            Duration::from_millis(10),
            pool.wait_for_shared_direct_quic_setup(
                DirectQuicSetupWaiter::BackgroundHealthProbe { probe_index: 0 },
                move |setup_attempt_id| async move {
                    assert_eq!(setup_attempt_id, 1);
                    first_starts.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(60)).await;
                    Err(anyhow!("synthetic shared setup failure"))
                },
            ),
        )
        .await;
        assert!(first.is_err(), "the first consumer should stop waiting");

        let result = match pool
            .wait_for_shared_direct_quic_setup(DirectQuicSetupWaiter::SessionConsumer, |_| async {
                panic!("a second consumer must join, not start another setup")
            })
            .await
        {
            Ok(_) => panic!("the shared setup result should be delivered to later consumers"),
            Err(error) => error,
        };
        assert!(
            result
                .to_string()
                .contains("synthetic shared setup failure")
        );
        assert_eq!(starts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn direct_quic_setup_attempt_ids_increment_after_terminal_results() {
        let pool = TransportSessionPool::new_direct_quic(
            ConnectionCandidate {
                kind: transport_sdk::CandidateKind::DirectQuic,
                endpoint: format!("iroh://{}", SecretKey::generate().public()),
                rtt_ms: None,
                transport_hints: None,
            },
            Some(NodeId::new_v4()),
            None,
            None,
        );

        for expected_attempt_id in 1..=2 {
            let error = match pool
                .wait_for_shared_direct_quic_setup(
                    DirectQuicSetupWaiter::SessionConsumer,
                    move |setup_attempt_id| async move {
                        assert_eq!(setup_attempt_id, expected_attempt_id);
                        Err(anyhow!("synthetic terminal result"))
                    },
                )
                .await
            {
                Ok(_) => panic!("synthetic setup should fail"),
                Err(error) => error,
            };
            assert!(error.to_string().contains("synthetic terminal result"));

            tokio::time::timeout(Duration::from_secs(1), async {
                while pool.direct_quic_setup.setup_active.load(Ordering::Relaxed) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("terminal setup should clear the active attempt");
        }
    }

    #[test]
    fn direct_quic_pool_snapshot_starts_empty() {
        let pool = TransportSessionPool::new_direct_quic(
            ConnectionCandidate {
                kind: transport_sdk::CandidateKind::DirectQuic,
                endpoint: "iroh://peer-key-1".to_string(),
                rtt_ms: None,
                transport_hints: None,
            },
            Some(NodeId::new_v4()),
            None,
            None,
        );

        assert_eq!(pool.snapshot(), TransportSessionPoolSnapshot::default());
        assert_eq!(pool.route_index(), None);
        pool.set_route_index(4);
        assert_eq!(pool.route_index(), Some(4));
    }

    #[test]
    fn relay_ticket_refresh_is_early_and_bounded_for_restart_recovery() {
        let mut tickets = IrohRelayTicketSet::default();
        tickets.insert(&IrohRelayTicket {
            public_urls: vec!["https://creax.de:44043".to_string()],
            auth_token: "ticket".to_string(),
            expires_at_unix: 1_300,
            quic_port: Some(7842),
        });
        let rollover = IrohRelayTicketRollover::from_ticket_set(&tickets, 1_000)
            .expect("ticket set schedules a rollover");
        assert_eq!(
            iroh_relay_ticket_rollover_delay(Some(rollover), 1_000),
            Duration::from_secs(200)
        );
        assert_eq!(
            iroh_relay_ticket_rollover_delay(Some(rollover), 0),
            IROH_RELAY_TICKET_REFRESH_MAX_INTERVAL
        );
        assert_eq!(
            iroh_relay_ticket_rollover_delay(Some(rollover), 1_300),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn endpoint_bound_relay_tickets_keep_each_relay_configuration() {
        let mut tickets = IrohRelayTicketSet::default();
        tickets.insert(&IrohRelayTicket {
            public_urls: vec!["https://creax.de:44043/".to_string()],
            auth_token: "creax-ticket".to_string(),
            expires_at_unix: 1_000,
            quic_port: Some(7842),
        });
        tickets.insert(&IrohRelayTicket {
            public_urls: vec!["https://217.160.159.105:9443".to_string()],
            auth_token: "strato-ticket".to_string(),
            expires_at_unix: 2_000,
            quic_port: Some(7842),
        });

        assert_eq!(tickets.earliest_expiry(), Some(1_000));
        let relay_configs = tickets.relay_configs();
        assert_eq!(relay_configs.len(), 2);
        assert_eq!(relay_configs[0].url, "https://217.160.159.105:9443");
        assert_eq!(
            relay_configs[0].auth_token.as_deref(),
            Some("strato-ticket")
        );
        assert_eq!(relay_configs[1].url, "https://creax.de:44043");
        assert_eq!(relay_configs[1].auth_token.as_deref(), Some("creax-ticket"));
    }
}
