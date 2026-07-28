use anyhow::{Context, Result, anyhow, bail};
use common::NodeId;
use futures_util::StreamExt;
use iroh::SecretKey;
use iroh::endpoint::Connection;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use std::time::Instant;
use tokio::sync::Mutex;
use transport_sdk::{
    ClientIdentityMaterial, ConnectionCandidate, DEFAULT_DIRECT_QUIC_ALPN, DirectQuicEndpoint,
    DirectQuicEndpointConfig, ExpectedNodeServerIdentity, MultiplexConfig, MultiplexMode,
    MultiplexedSession, PeerIdentity, RelayTicketRequest, RelayTunnelSession,
    RelayTunnelSessionKind, RelayTunnelSourceSecurityConfig, RendezvousControlClient,
    TRANSPORT_PROTOCOL_VERSION, TransportSessionControlMessage, TransportSessionRole,
    WebSocketByteStream, build_signed_request_headers,
    connect_websocket_with_expected_server_identity, perform_transport_client_handshake,
    websocket_url,
};

const IROH_RELAY_TICKET_REFRESH_MAX_INTERVAL: Duration = Duration::from_secs(5 * 60);

#[derive(Clone)]
pub(crate) struct TransportSessionPool {
    target: SessionPoolTarget,
    cached_session: Arc<Mutex<Option<CachedTransportSession>>>,
    stats: Arc<TransportSessionPoolStats>,
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
            stats: Arc::new(TransportSessionPoolStats::default()),
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
            stats: Arc::new(TransportSessionPoolStats::default()),
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
            stats: Arc::new(TransportSessionPoolStats::default()),
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
    ) -> Result<Arc<MultiplexedSession>> {
        let mut guard = self.cached_session.lock().await;
        if let Some(existing) = guard.as_ref() {
            self.stats.reuse_count.fetch_add(1, Ordering::Relaxed);
            return Ok(Arc::clone(&existing.session));
        }

        let connect_started = Instant::now();

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
                let target_label = candidate.endpoint.clone();
                let endpoint = ensure_direct_quic_endpoint(
                    endpoint,
                    candidate,
                    rendezvous.as_ref(),
                    relay_ca_pem.as_deref(),
                )
                .await?;
                let direct_quic = endpoint
                    .connect_session(candidate, MultiplexConfig::default())
                    .await
                    .with_context(|| {
                        format!("failed opening direct QUIC transport session to {target_label}")
                    })?;
                self.update_hole_punching_mode(&direct_quic.connection);
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

        perform_transport_client_handshake(
            &multiplexed,
            TransportSessionControlMessage::Hello {
                protocol_version: TRANSPORT_PROTOCOL_VERSION,
                cluster_id: identity.cluster_id,
                role: TransportSessionRole::Client,
                peer: PeerIdentity::Device(identity.device_id),
                connection_name: connection_name.map(ToString::to_string),
                target,
            },
        )
        .await
        .with_context(|| handshake_context)?;

        let session = Arc::new(multiplexed);
        *guard = Some(CachedTransportSession {
            session: Arc::clone(&session),
            _relay_session: None,
        });
        self.stats.connect_count.fetch_add(1, Ordering::Relaxed);
        self.stats.connect_duration_us.fetch_add(
            duration_as_u64_micros(connect_started.elapsed()),
            Ordering::Relaxed,
        );
        Ok(session)
    }

    fn update_hole_punching_mode(&self, connection: &Connection) {
        let mode = connection
            .paths()
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
            .unwrap_or(DIRECT_CONNECTION_MODE_UNKNOWN);
        self.stats
            .direct_connection_mode
            .store(mode, Ordering::Relaxed);
    }

    fn spawn_hole_punching_monitor(&self, connection: Connection) {
        let stats = Arc::clone(&self.stats);
        tokio::spawn(async move {
            let mut events = connection.path_events();
            while events.next().await.is_some() {
                let mode = connection
                    .paths()
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
                    .unwrap_or(DIRECT_CONNECTION_MODE_UNKNOWN);
                stats.direct_connection_mode.store(mode, Ordering::Relaxed);
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

        let mut guard = self.cached_session.lock().await;
        if let Some(existing) = guard.as_ref() {
            self.stats.reuse_count.fetch_add(1, Ordering::Relaxed);
            return Ok(Arc::clone(&existing.session));
        }

        let connect_started = Instant::now();

        let ticket = rendezvous
            .issue_relay_ticket(&RelayTicketRequest {
                cluster_id: rendezvous.config().cluster_id,
                source: source.clone(),
                target: PeerIdentity::Node(*target_node_id),
                session_kind: RelayTunnelSessionKind::MultiplexTransport,
                security_mode: transport_sdk::RelayTunnelSecurityMode::InnerMtls,
                requested_expires_in_secs: Some(300),
            })
            .await
            .with_context(|| {
                format!(
                    "failed issuing multiplex relay ticket for client target node {}",
                    target_node_id
                )
            })?;
        let relay_tunnel = rendezvous
            .connect_relay_tunnel_source(&ticket)
            .await
            .with_context(|| {
                format!(
                    "failed opening relay tunnel source for client target node {}",
                    target_node_id
                )
            })?;
        let relay_pairing_duration_us = relay_tunnel
            .pairing_timing()
            .map(|timing| timing.relay_pairing_duration_us)
            .unwrap_or_default();
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
        *guard = Some(CachedTransportSession {
            session: Arc::clone(&session),
            _relay_session: Some(relay_session),
        });
        self.stats.connect_count.fetch_add(1, Ordering::Relaxed);
        self.stats.connect_duration_us.fetch_add(
            duration_as_u64_micros(connect_started.elapsed()),
            Ordering::Relaxed,
        );
        self.stats
            .relay_pairing_duration_us
            .fetch_add(relay_pairing_duration_us, Ordering::Relaxed);
        Ok(session)
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

async fn ensure_direct_quic_endpoint(
    endpoint: &Arc<Mutex<Option<ManagedDirectQuicEndpoint>>>,
    candidate: &ConnectionCandidate,
    rendezvous: Option<&RendezvousControlClient>,
    relay_ca_pem: Option<&str>,
) -> Result<DirectQuicEndpoint> {
    let mut guard = endpoint.lock().await;
    if let Some(managed) = guard.as_ref() {
        return Ok(managed.endpoint.clone());
    }

    let secret_key = SecretKey::generate();
    let endpoint_id = secret_key.public().to_string();
    let relay_ticket = match rendezvous {
        Some(rendezvous) => match rendezvous.issue_iroh_relay_ticket(&endpoint_id).await {
            Ok(ticket) => Some(ticket),
            Err(error) => {
                tracing::warn!(
                    %error,
                    %endpoint_id,
                    "failed obtaining endpoint-bound iroh relay ticket; direct addresses remain available"
                );
                None
            }
        },
        None => None,
    };

    let mut config = DirectQuicEndpointConfig::new(secret_key);
    config.alpn = candidate
        .transport_hints
        .as_ref()
        .and_then(|hints| hints.alpn.clone())
        .unwrap_or_else(|| DEFAULT_DIRECT_QUIC_ALPN.to_string());
    if rendezvous.is_none() {
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

    let endpoint = DirectQuicEndpoint::bind(config).await.with_context(|| {
        format!(
            "failed binding local direct QUIC endpoint for remote candidate {}",
            candidate.endpoint
        )
    })?;
    if let Some(ticket) = relay_ticket.as_ref() {
        endpoint
            .reconcile_dynamic_relays(&iroh_relay_configs_from_ticket(ticket))
            .await
            .context("failed applying endpoint-bound iroh relay ticket")?;
    }
    let initial_expires_at_unix = relay_ticket
        .as_ref()
        .map(|ticket| ticket.expires_at_unix)
        .unwrap_or_else(unix_ts);
    let relay_ticket_refresh = rendezvous.map(|rendezvous| {
        spawn_iroh_relay_ticket_refresh(
            endpoint.clone(),
            rendezvous.clone(),
            endpoint_id,
            initial_expires_at_unix,
        )
    });
    *guard = Some(ManagedDirectQuicEndpoint {
        endpoint: endpoint.clone(),
        relay_ticket_refresh,
    });
    Ok(endpoint)
}

fn spawn_iroh_relay_ticket_refresh(
    endpoint: DirectQuicEndpoint,
    rendezvous: RendezvousControlClient,
    endpoint_id: String,
    initial_expires_at_unix: u64,
) -> tokio::task::AbortHandle {
    let task = tokio::spawn(async move {
        let mut expires_at_unix = initial_expires_at_unix;
        loop {
            tokio::time::sleep(iroh_relay_ticket_refresh_delay(expires_at_unix)).await;
            match rendezvous.issue_iroh_relay_ticket(&endpoint_id).await {
                Ok(ticket) => {
                    let relays = iroh_relay_configs_from_ticket(&ticket);
                    match endpoint.reconcile_dynamic_relays(&relays).await {
                        Ok(_) => expires_at_unix = ticket.expires_at_unix,
                        Err(error) => {
                            tracing::warn!(
                                %error,
                                %endpoint_id,
                                "failed applying refreshed endpoint-bound iroh relay ticket"
                            );
                            tokio::time::sleep(Duration::from_secs(30)).await;
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        %endpoint_id,
                        "failed refreshing endpoint-bound iroh relay ticket"
                    );
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
            }
        }
    });
    task.abort_handle()
}

fn iroh_relay_configs_from_ticket(
    ticket: &transport_sdk::IrohRelayTicket,
) -> Vec<transport_sdk::DirectQuicRelayConfig> {
    ticket
        .public_urls
        .iter()
        .map(|url| transport_sdk::DirectQuicRelayConfig {
            url: url.clone(),
            auth_token: Some(ticket.auth_token.clone()),
        })
        .collect()
}

fn iroh_relay_ticket_refresh_delay(expires_at_unix: u64) -> Duration {
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    iroh_relay_ticket_refresh_delay_at(expires_at_unix, now_unix)
}

fn iroh_relay_ticket_refresh_delay_at(expires_at_unix: u64, now_unix: u64) -> Duration {
    let remaining = expires_at_unix.saturating_sub(now_unix);
    Duration::from_secs((remaining.saturating_mul(2) / 3).max(1))
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
    }

    #[test]
    fn relay_ticket_refresh_is_early_and_bounded_for_restart_recovery() {
        assert_eq!(
            iroh_relay_ticket_refresh_delay_at(1_300, 1_000),
            Duration::from_secs(200)
        );
        assert_eq!(
            iroh_relay_ticket_refresh_delay_at(10_000, 1_000),
            IROH_RELAY_TICKET_REFRESH_MAX_INTERVAL
        );
        assert_eq!(
            iroh_relay_ticket_refresh_delay_at(1_000, 1_000),
            Duration::from_secs(1)
        );
    }
}
