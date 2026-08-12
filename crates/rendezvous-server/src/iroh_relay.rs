use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, Response, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use common::ClusterId;
use hmac::{Hmac, Mac};
use hyper::body::Incoming;
use hyper::server::conn::{http1, http2};
use hyper::service::Service as HyperService;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::service::TowerToHyperService;
use iroh_relay::KeyCache;
use iroh_relay::server::clients::Clients;
use iroh_relay::server::http_server::{Handlers, RelayService, RelayServiceWithNotify};
use iroh_relay::server::streams::MaybeTlsStream;
use iroh_relay::server::{
    Access, AccessControl, ClientRateLimit, ClientRequest, ConnectionId, Metrics,
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tokio::sync::Notify;
use tower::Service;
use tracing::warn;
use transport_sdk::{IrohRelayTicket, PeerIdentity};

use crate::admission::{
    AcceptErrorBackoff, RendezvousAdmissionConfig, RendezvousAdmissionController,
    classify_accept_error, should_log_admission_event,
};
use crate::auth::{AuthenticatedPeer, WithAuthenticatedPeer, authenticated_peer_from_tls_stream};
use crate::{RendezvousServerTlsIdentity, auth::build_server_rustls_config};

const TICKET_FORMAT: &str = "imrt2";
const TICKET_VERSION: u8 = 2;
const TICKET_CLOCK_SKEW_SECS: u64 = 60;
const KEY_CACHE_CAPACITY: usize = 1024;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrohRelayServerConfig {
    pub public_urls: Vec<String>,
    pub ticket_ttl: Duration,
    pub client_rx_bytes_per_second: u32,
    pub client_rx_max_burst_bytes: u32,
    pub max_ticket_leases_per_client: u32,
    pub max_active_connections_per_client: u32,
    pub max_ticket_issues_per_minute: u32,
    pub quic: Option<IrohRelayQuicServerConfig>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct IrohRelayQuicServerConfig {
    pub bind_addr: SocketAddr,
    pub public_port: u16,
    pub server_identity: RendezvousServerTlsIdentity,
}

impl std::fmt::Debug for IrohRelayQuicServerConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IrohRelayQuicServerConfig")
            .field("bind_addr", &self.bind_addr)
            .field("public_port", &self.public_port)
            .field("server_identity", &"[REDACTED]")
            .finish()
    }
}

impl IrohRelayServerConfig {
    pub fn validate(&self) -> Result<()> {
        if self.public_urls.is_empty() {
            bail!("embedded iroh relay requires at least one public URL");
        }
        let mut seen_urls = std::collections::HashSet::new();
        for url in &self.public_urls {
            let uri = url
                .trim()
                .parse::<axum::http::Uri>()
                .with_context(|| format!("invalid embedded iroh relay public URL {url:?}"))?;
            if !uri.scheme_str().is_some_and(|scheme| {
                scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
            }) || uri.authority().is_none()
            {
                bail!("embedded iroh relay public URL must be an HTTP(S) origin: {url}");
            }
            if !matches!(uri.path(), "" | "/") || uri.query().is_some() {
                bail!("embedded iroh relay public URL must not contain a path or query: {url}");
            }
            let normalized = url.trim().trim_end_matches('/');
            if !seen_urls.insert(normalized.to_string()) {
                bail!("embedded iroh relay public URLs must not contain duplicates");
            }
        }
        if !(300..=86_400).contains(&self.ticket_ttl.as_secs()) {
            bail!("embedded iroh relay ticket TTL must be between 300 and 86400 seconds");
        }
        if self.client_rx_bytes_per_second == 0 {
            bail!("embedded iroh relay receive rate must be greater than zero");
        }
        if self.client_rx_max_burst_bytes == 0 {
            bail!("embedded iroh relay receive burst must be greater than zero");
        }
        if self.max_ticket_leases_per_client == 0 {
            bail!("embedded iroh relay ticket lease limit must be greater than zero");
        }
        if self.max_active_connections_per_client == 0 {
            bail!("embedded iroh relay active connection limit must be greater than zero");
        }
        if self.max_ticket_issues_per_minute == 0 {
            bail!("embedded iroh relay ticket issue rate must be greater than zero");
        }
        if let Some(quic) = self.quic.as_ref() {
            if quic.bind_addr.port() == 0 {
                bail!("embedded iroh relay QUIC bind port must be greater than zero");
            }
            if quic.public_port == 0 {
                bail!("embedded iroh relay QUIC public port must be greater than zero");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct TicketClaims {
    version: u8,
    cluster_id: ClusterId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    peer: Option<PeerIdentity>,
    endpoint_id: String,
    lease_id: String,
    issued_at_unix: u64,
    expires_at_unix: u64,
}

#[derive(Clone)]
struct TicketAuthority {
    key: Arc<[u8; 32]>,
    ttl_secs: u64,
}

impl std::fmt::Debug for TicketAuthority {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TicketAuthority")
            .field("key", &"[REDACTED]")
            .field("ttl_secs", &self.ttl_secs)
            .finish()
    }
}

impl TicketAuthority {
    fn generate(ttl: Duration) -> Self {
        let mut key = [0_u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut key);
        Self {
            key: Arc::new(key),
            ttl_secs: ttl.as_secs(),
        }
    }

    #[cfg(test)]
    fn from_key(key: [u8; 32], ttl: Duration) -> Self {
        Self {
            key: Arc::new(key),
            ttl_secs: ttl.as_secs(),
        }
    }

    fn issue(
        &self,
        public_urls: Vec<String>,
        quic_port: Option<u16>,
        claims: TicketClaims,
    ) -> Result<IrohRelayTicket> {
        claims
            .endpoint_id
            .parse::<iroh::EndpointId>()
            .context("invalid iroh endpoint id for relay ticket")?;
        let expires_at_unix = claims.expires_at_unix;
        let payload = serde_json::to_vec(&claims).context("failed encoding iroh relay ticket")?;
        let payload = URL_SAFE_NO_PAD.encode(payload);
        let signature = self.sign(payload.as_bytes());
        Ok(IrohRelayTicket {
            public_urls,
            auth_token: format!(
                "{TICKET_FORMAT}.{payload}.{}",
                URL_SAFE_NO_PAD.encode(signature)
            ),
            expires_at_unix,
            quic_port,
        })
    }

    fn verify(&self, token: &str, endpoint_id: &str, now_unix: u64) -> Result<TicketClaims> {
        let mut segments = token.split('.');
        let format = segments.next();
        let payload = segments.next();
        let signature = segments.next();
        if format != Some(TICKET_FORMAT)
            || payload.is_none()
            || signature.is_none()
            || segments.next().is_some()
        {
            bail!("invalid iroh relay ticket format");
        }
        let payload = payload.expect("checked relay ticket payload");
        let signature = URL_SAFE_NO_PAD
            .decode(signature.expect("checked relay ticket signature"))
            .context("invalid iroh relay ticket signature encoding")?;
        let mut mac =
            HmacSha256::new_from_slice(self.key.as_ref()).expect("HMAC accepts 32-byte keys");
        mac.update(payload.as_bytes());
        mac.verify_slice(&signature)
            .context("invalid iroh relay ticket signature")?;

        let decoded = URL_SAFE_NO_PAD
            .decode(payload)
            .context("invalid iroh relay ticket payload encoding")?;
        if URL_SAFE_NO_PAD.encode(&decoded) != payload {
            bail!("non-canonical iroh relay ticket payload encoding");
        }
        let claims: TicketClaims =
            serde_json::from_slice(&decoded).context("invalid iroh relay ticket payload")?;
        if claims.version != TICKET_VERSION {
            bail!("unsupported iroh relay ticket version");
        }
        if claims.endpoint_id != endpoint_id {
            bail!("iroh relay ticket endpoint binding does not match");
        }
        if claims.issued_at_unix > now_unix.saturating_add(TICKET_CLOCK_SKEW_SECS) {
            bail!("iroh relay ticket is not valid yet");
        }
        if claims.expires_at_unix <= now_unix {
            bail!("iroh relay ticket has expired");
        }
        if claims.expires_at_unix > claims.issued_at_unix.saturating_add(self.ttl_secs) {
            bail!("iroh relay ticket lifetime exceeds server policy");
        }
        Ok(claims)
    }

    fn sign(&self, payload: &[u8]) -> [u8; 32] {
        let mut mac =
            HmacSha256::new_from_slice(self.key.as_ref()).expect("HMAC accepts 32-byte keys");
        mac.update(payload);
        mac.finalize().into_bytes().into()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TicketClientKey {
    cluster_id: ClusterId,
    peer: Option<PeerIdentity>,
}

struct TicketLeaseRequest {
    public_urls: Vec<String>,
    quic_port: Option<u16>,
    client: TicketClientKey,
    endpoint_id: String,
    now_unix: u64,
}

impl TicketClientKey {
    fn new(cluster_id: ClusterId, peer: Option<PeerIdentity>) -> Self {
        Self { cluster_id, peer }
    }
}

#[derive(Clone)]
struct TicketLease {
    lease_id: String,
    expires_at_unix: u64,
    ticket: IrohRelayTicket,
}

#[derive(Default)]
struct TicketLeaseState {
    leases: HashMap<TicketClientKey, HashMap<String, TicketLease>>,
    issue_history: HashMap<TicketClientKey, VecDeque<u64>>,
    active_connections: HashMap<ConnectionId, TicketClientKey>,
}

#[derive(Debug)]
pub(crate) struct TicketLeaseBackpressure {
    pub(crate) message: String,
    pub(crate) retry_after_secs: u64,
}

#[derive(Clone)]
struct TicketLeaseRegistry {
    state: Arc<Mutex<TicketLeaseState>>,
    max_leases_per_client: usize,
    max_active_connections_per_client: usize,
    max_issues_per_minute: usize,
}

impl TicketLeaseRegistry {
    fn new(config: &IrohRelayServerConfig) -> Self {
        Self {
            state: Arc::new(Mutex::new(TicketLeaseState::default())),
            max_leases_per_client: config.max_ticket_leases_per_client as usize,
            max_active_connections_per_client: config.max_active_connections_per_client as usize,
            max_issues_per_minute: config.max_ticket_issues_per_minute as usize,
        }
    }

    fn issue_or_reuse(
        &self,
        authority: &TicketAuthority,
        request: TicketLeaseRequest,
    ) -> std::result::Result<IrohRelayTicket, TicketLeaseBackpressure> {
        let TicketLeaseRequest {
            public_urls,
            quic_port,
            client,
            endpoint_id,
            now_unix,
        } = request;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Self::prune_expired(&mut state, now_unix);

        if let Some(ticket) = state
            .leases
            .get(&client)
            .and_then(|leases| leases.get(&endpoint_id))
            .map(|lease| lease.ticket.clone())
        {
            return Ok(ticket);
        }

        let active_leases = state.leases.get(&client).map_or(0, HashMap::len);
        if active_leases >= self.max_leases_per_client {
            let retry_after_secs = state
                .leases
                .get(&client)
                .and_then(|leases| leases.values().map(|lease| lease.expires_at_unix).min())
                .unwrap_or(now_unix.saturating_add(1))
                .saturating_sub(now_unix)
                .max(1);
            return Err(TicketLeaseBackpressure {
                message: format!(
                    "iroh relay ticket lease limit reached ({}/{})",
                    active_leases, self.max_leases_per_client
                ),
                retry_after_secs,
            });
        }

        let history = state.issue_history.entry(client.clone()).or_default();
        while history
            .front()
            .is_some_and(|issued| issued.saturating_add(60) <= now_unix)
        {
            history.pop_front();
        }
        if history.len() >= self.max_issues_per_minute {
            let retry_after_secs = history
                .front()
                .copied()
                .unwrap_or(now_unix)
                .saturating_add(60)
                .saturating_sub(now_unix)
                .max(1);
            return Err(TicketLeaseBackpressure {
                message: format!(
                    "iroh relay ticket issue rate exceeded ({} per minute)",
                    self.max_issues_per_minute
                ),
                retry_after_secs,
            });
        }

        let mut lease_bytes = [0_u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut lease_bytes);
        let lease_id = URL_SAFE_NO_PAD.encode(lease_bytes);
        let expires_at_unix = now_unix.saturating_add(authority.ttl_secs);
        let ticket = authority
            .issue(
                public_urls,
                quic_port,
                TicketClaims {
                    version: TICKET_VERSION,
                    cluster_id: client.cluster_id,
                    peer: client.peer.clone(),
                    endpoint_id: endpoint_id.clone(),
                    lease_id: lease_id.clone(),
                    issued_at_unix: now_unix,
                    expires_at_unix,
                },
            )
            .map_err(|error| TicketLeaseBackpressure {
                message: error.to_string(),
                retry_after_secs: 0,
            })?;
        history.push_back(now_unix);
        state.leases.entry(client).or_default().insert(
            endpoint_id,
            TicketLease {
                lease_id,
                expires_at_unix,
                ticket: ticket.clone(),
            },
        );
        Ok(ticket)
    }

    fn authorize_connection(
        &self,
        claims: &TicketClaims,
        connection_id: ConnectionId,
        now_unix: u64,
    ) -> Result<()> {
        let client = TicketClientKey::new(claims.cluster_id, claims.peer.clone());
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Self::prune_expired(&mut state, now_unix);
        let lease_is_active = state
            .leases
            .get(&client)
            .and_then(|leases| leases.get(&claims.endpoint_id))
            .is_some_and(|lease| {
                lease.lease_id == claims.lease_id && lease.expires_at_unix > now_unix
            });
        if !lease_is_active {
            bail!("iroh relay ticket lease is expired or revoked");
        }
        let active_count = state
            .active_connections
            .values()
            .filter(|active_client| *active_client == &client)
            .count();
        if active_count >= self.max_active_connections_per_client {
            bail!(
                "iroh relay active connection limit reached ({}/{})",
                active_count,
                self.max_active_connections_per_client
            );
        }
        state.active_connections.insert(connection_id, client);
        Ok(())
    }

    fn disconnect(&self, connection_id: ConnectionId) {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .active_connections
            .remove(&connection_id);
    }

    fn release(
        &self,
        cluster_id: ClusterId,
        peer: Option<PeerIdentity>,
        endpoint_id: &str,
        now_unix: u64,
    ) -> bool {
        let client = TicketClientKey::new(cluster_id, peer);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Self::prune_expired(&mut state, now_unix);
        let released = state
            .leases
            .get_mut(&client)
            .and_then(|leases| leases.remove(endpoint_id))
            .is_some();
        if state.leases.get(&client).is_some_and(HashMap::is_empty) {
            state.leases.remove(&client);
        }
        released
    }

    fn prune_expired(state: &mut TicketLeaseState, now_unix: u64) {
        state.leases.retain(|_, leases| {
            leases.retain(|_, lease| lease.expires_at_unix > now_unix);
            !leases.is_empty()
        });
        state.issue_history.retain(|_, history| {
            while history
                .front()
                .is_some_and(|issued| issued.saturating_add(60) <= now_unix)
            {
                history.pop_front();
            }
            !history.is_empty()
        });
    }
}

#[derive(Clone)]
struct TicketAccess {
    authority: TicketAuthority,
    leases: TicketLeaseRegistry,
    clients: Arc<OnceLock<Clients>>,
    expiry_tasks: Arc<Mutex<HashMap<ConnectionId, tokio::task::AbortHandle>>>,
}

impl std::fmt::Debug for TicketAccess {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TicketAccess")
            .field("authority", &self.authority)
            .finish_non_exhaustive()
    }
}

impl TicketAccess {
    fn new(authority: TicketAuthority, leases: TicketLeaseRegistry) -> Self {
        Self {
            authority,
            leases,
            clients: Arc::new(OnceLock::new()),
            expiry_tasks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn set_clients(&self, clients: Clients) {
        let _ = self.clients.set(clients);
    }

    fn schedule_expiry(&self, request: &ClientRequest, expires_at_unix: u64) {
        let Some(clients) = self.clients.get().cloned() else {
            return;
        };
        let endpoint_id = request.endpoint_id();
        let connection_id = request.connection_id();
        let delay = Duration::from_secs(expires_at_unix.saturating_sub(unix_timestamp()));
        let task = tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            clients.disconnect(endpoint_id, Some(connection_id));
        });
        if let Some(previous) = self
            .expiry_tasks
            .lock()
            .expect("iroh relay expiry task mutex poisoned")
            .insert(connection_id, task.abort_handle())
        {
            previous.abort();
        }
    }
}

impl AccessControl for TicketAccess {
    async fn on_connect(&self, request: &ClientRequest) -> Access {
        let Some(token) = request.auth_token() else {
            return Access::Deny {
                reason: Some("missing IronMesh relay ticket".to_string()),
            };
        };
        match self
            .authority
            .verify(&token, &request.endpoint_id().to_string(), unix_timestamp())
        {
            Ok(claims) => {
                match self.leases.authorize_connection(
                    &claims,
                    request.connection_id(),
                    unix_timestamp(),
                ) {
                    Ok(()) => {
                        self.schedule_expiry(request, claims.expires_at_unix);
                        Access::Allow
                    }
                    Err(error) => Access::Deny {
                        reason: Some(error.to_string()),
                    },
                }
            }
            Err(error) => Access::Deny {
                reason: Some(error.to_string()),
            },
        }
    }

    fn on_disconnect(&self, _endpoint_id: iroh::EndpointId, connection_id: ConnectionId) {
        self.leases.disconnect(connection_id);
        if let Some(task) = self
            .expiry_tasks
            .lock()
            .expect("iroh relay expiry task mutex poisoned")
            .remove(&connection_id)
        {
            task.abort();
        }
    }
}

#[derive(Clone)]
pub(crate) struct IrohRelayRuntime {
    config: IrohRelayServerConfig,
    authority: TicketAuthority,
    access: Arc<TicketAccess>,
    service: RelayService,
}

impl std::fmt::Debug for IrohRelayRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IrohRelayRuntime")
            .field("config", &self.config)
            .field("authority", &self.authority)
            .finish_non_exhaustive()
    }
}

impl IrohRelayRuntime {
    pub(crate) fn new(config: IrohRelayServerConfig) -> Result<Self> {
        config.validate()?;
        let authority = TicketAuthority::generate(config.ticket_ttl);
        let leases = TicketLeaseRegistry::new(&config);
        let access = Arc::new(TicketAccess::new(authority.clone(), leases));
        let bytes_per_second = NonZeroU32::new(config.client_rx_bytes_per_second)
            .expect("validated iroh relay receive rate should be non-zero");
        let max_burst_bytes = NonZeroU32::new(config.client_rx_max_burst_bytes)
            .expect("validated iroh relay receive burst should be non-zero");
        let mut rate_limit = ClientRateLimit::new(bytes_per_second);
        rate_limit.max_burst_bytes = Some(max_burst_bytes);
        let service = RelayService::new(
            Handlers::default(),
            HeaderMap::new(),
            Some(rate_limit),
            KeyCache::new(KEY_CACHE_CAPACITY),
            access.clone(),
            Arc::new(Metrics::default()),
        );
        access.set_clients(service.clients().clone());
        Ok(Self {
            config,
            authority,
            access,
            service,
        })
    }

    pub(crate) fn public_urls(&self) -> &[String] {
        &self.config.public_urls
    }

    pub(crate) fn service(&self) -> RelayService {
        self.service.clone()
    }

    pub(crate) async fn spawn_quic_server(&self) -> Result<Option<::iroh_relay::server::Server>> {
        let Some(quic) = self.config.quic.as_ref() else {
            return Ok(None);
        };
        let server_config = build_server_rustls_config(&quic.server_identity)
            .context("failed building embedded iroh QAD TLS configuration")?;
        let mut quic_config = ::iroh_relay::server::QuicConfig::new(quic.bind_addr);
        quic_config.server_config = Some(server_config);
        let mut config = ::iroh_relay::server::ServerConfig::default();
        config.quic = Some(quic_config);
        let server = ::iroh_relay::server::Server::spawn(config)
            .await
            .context("failed starting embedded iroh QAD server")?;
        Ok(Some(server))
    }

    pub(crate) fn issue_ticket(
        &self,
        cluster_id: ClusterId,
        peer: Option<PeerIdentity>,
        endpoint_id: &str,
    ) -> std::result::Result<IrohRelayTicket, TicketLeaseBackpressure> {
        self.access.leases.issue_or_reuse(
            &self.authority,
            TicketLeaseRequest {
                public_urls: self.config.public_urls.clone(),
                quic_port: self.config.quic.as_ref().map(|quic| quic.public_port),
                client: TicketClientKey::new(cluster_id, peer),
                endpoint_id: endpoint_id.to_string(),
                now_unix: unix_timestamp(),
            },
        )
    }

    pub(crate) fn release_ticket(
        &self,
        cluster_id: ClusterId,
        peer: Option<PeerIdentity>,
        endpoint_id: &str,
    ) -> bool {
        let released = self
            .access
            .leases
            .release(cluster_id, peer, endpoint_id, unix_timestamp());
        if released && let Ok(endpoint_id) = endpoint_id.parse::<iroh::EndpointId>() {
            self.service.clients().disconnect(endpoint_id, None);
        }
        released
    }
}

pub(crate) async fn serve_listener(
    bind_addr: SocketAddr,
    app: Router,
    tls_config: Option<axum_server::tls_rustls::RustlsConfig>,
    relay: Option<RelayService>,
    admission_config: RendezvousAdmissionConfig,
) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    let tls_acceptor = tls_config.map(|config| tokio_rustls::TlsAcceptor::from(config.get_inner()));
    let admission = RendezvousAdmissionController::new(admission_config)?;
    let limits = admission.config();
    tracing::info!(
        event = "rendezvous_admission_started",
        max_connections = limits.max_connections,
        max_tls_handshakes = limits.max_tls_handshakes,
        tls_enabled = tls_acceptor.is_some(),
        "rendezvous listener admission protection enabled"
    );
    let mut accept_backoff = AcceptErrorBackoff::default();
    loop {
        let connection_permit = match admission.try_admit_connection() {
            Ok(permit) => permit,
            Err(saturation) => {
                if should_log_admission_event(saturation.events_total) {
                    tracing::warn!(
                        event = "rendezvous_connection_limit_saturated",
                        active_connections = saturation.active,
                        max_connections = saturation.limit,
                        saturation_events_total = saturation.events_total,
                        "rendezvous connection admission saturated; pausing accepts"
                    );
                }
                admission.admit_connection().await
            }
        };
        let (stream, peer_addr) = match listener.accept().await {
            Ok(accepted) => {
                accept_backoff.reset();
                accepted
            }
            Err(error) => {
                drop(connection_permit);
                let error_class = classify_accept_error(&error);
                let Some(retry_after) = accept_backoff.delay_after(&error) else {
                    tracing::debug!(
                        event = "rendezvous_accept_interrupted",
                        %error,
                        "rendezvous listener accept interrupted; retrying"
                    );
                    continue;
                };
                tracing::warn!(
                    event = "rendezvous_accept_failed",
                    %error,
                    ?error_class,
                    retry_after_ms = retry_after.as_millis(),
                    "rendezvous listener accept failed; retrying with bounded backoff"
                );
                tokio::time::sleep(retry_after).await;
                continue;
            }
        };
        let app = app.clone();
        let relay = relay.clone();
        let tls_acceptor = tls_acceptor.clone();
        let admission = admission.clone();
        tokio::spawn(async move {
            let _connection_permit = connection_permit;
            let result =
                serve_connection(stream, peer_addr, app, relay, tls_acceptor, admission).await;
            if let Err(error) = result {
                tracing::debug!(%error, %peer_addr, "rendezvous connection ended");
            }
        });
    }
}

async fn serve_connection(
    stream: tokio::net::TcpStream,
    peer_addr: SocketAddr,
    app: Router,
    relay: Option<RelayService>,
    tls_acceptor: Option<tokio_rustls::TlsAcceptor>,
    admission: RendezvousAdmissionController,
) -> Result<()> {
    if let Some(tls_acceptor) = tls_acceptor {
        let handshake_permit = match admission.try_admit_tls_handshake() {
            Ok(permit) => permit,
            Err(rejection) => {
                if should_log_admission_event(rejection.events_total) {
                    tracing::warn!(
                        event = "rendezvous_tls_handshake_rejected",
                        %peer_addr,
                        active_tls_handshakes = rejection.active,
                        max_tls_handshakes = rejection.limit,
                        rejected_total = rejection.events_total,
                        "rendezvous TLS handshake rejected by global admission limit"
                    );
                }
                return Ok(());
            }
        };
        let tls_stream = tokio::time::timeout(Duration::from_secs(10), tls_acceptor.accept(stream))
            .await
            .context("rendezvous TLS handshake timed out")?
            .context("rendezvous TLS handshake failed")?;
        drop(handshake_permit);
        let authenticated_peer = authenticated_peer_from_tls_stream(&tls_stream)?;
        let http2 = tls_stream.get_ref().1.alpn_protocol() == Some(b"h2");
        return serve_stream(
            MaybeTlsStream::Tls(tls_stream),
            peer_addr,
            authenticated_peer,
            app,
            relay,
            http2,
        )
        .await;
    }

    serve_stream(
        MaybeTlsStream::Plain(stream),
        peer_addr,
        None,
        app,
        relay,
        false,
    )
    .await
}

async fn serve_stream(
    stream: MaybeTlsStream,
    peer_addr: SocketAddr,
    authenticated_peer: Option<AuthenticatedPeer>,
    app: Router,
    relay: Option<RelayService>,
    http2: bool,
) -> Result<()> {
    let service = SamePortProtocolService::new(
        WithAuthenticatedPeer::new(app, authenticated_peer, Some(peer_addr)),
        relay,
    );
    let service = TowerToHyperService::new(service);
    if http2 {
        http2::Builder::new(TokioExecutor::new())
            .serve_connection(TokioIo::new(stream), service)
            .await
            .context("Rendezvous HTTP/2 connection failed")?;
    } else {
        http1::Builder::new()
            .serve_connection(TokioIo::new(stream), service)
            .with_upgrades()
            .await
            .context("Rendezvous HTTP/1.1 connection failed")?;
    }
    Ok(())
}

#[derive(Clone)]
pub(crate) struct SamePortProtocolService<S> {
    inner: S,
    relay: Option<RelayService>,
}

impl<S> SamePortProtocolService<S> {
    pub(crate) fn new(inner: S, relay: Option<RelayService>) -> Self {
        Self { inner, relay }
    }
}

impl<S> Service<Request<Incoming>> for SamePortProtocolService<S>
where
    S: Service<Request<Incoming>, Response = Response<Body>> + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
{
    type Response = Response<Body>;
    type Error = S::Error;
    type Future =
        Pin<Box<dyn Future<Output = std::result::Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(
        &mut self,
        context: &mut TaskContext<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        self.inner.poll_ready(context)
    }

    fn call(&mut self, request: Request<Incoming>) -> Self::Future {
        if request.method() == axum::http::Method::GET
            && request.version() == axum::http::Version::HTTP_11
            && request.uri().path() == iroh_relay::http::RELAY_PATH
            && let Some(relay) = self.relay.clone()
        {
            return Box::pin(async move {
                let service = RelayServiceWithNotify::new(relay, Arc::new(Notify::new()));
                let response = match HyperService::call(&service, request).await {
                    Ok(response) => {
                        let (parts, body) = response.into_parts();
                        Response::from_parts(parts, Body::new(body))
                    }
                    Err(error) => {
                        warn!(%error, "embedded iroh relay request handler failed");
                        Response::builder()
                            .status(StatusCode::INTERNAL_SERVER_ERROR)
                            .body(Body::empty())
                            .expect("fixed embedded relay error response")
                    }
                };
                Ok(response)
            });
        }

        Box::pin(self.inner.call(request))
    }
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{Request as HttpRequest, Version};
    use common::DeviceId;
    use iroh::SecretKey;
    use iroh_relay::http::ProtocolVersion;

    fn test_authority() -> TicketAuthority {
        TicketAuthority::from_key([7_u8; 32], Duration::from_secs(3600))
    }

    fn test_config() -> IrohRelayServerConfig {
        IrohRelayServerConfig {
            public_urls: vec!["https://rendezvous.example".to_string()],
            ticket_ttl: Duration::from_secs(3600),
            client_rx_bytes_per_second: 1024,
            client_rx_max_burst_bytes: 2048,
            max_ticket_leases_per_client: 10,
            max_active_connections_per_client: 10,
            max_ticket_issues_per_minute: 10,
            quic: None,
        }
    }

    fn ticket_lease_request(
        cluster_id: ClusterId,
        peer: Option<PeerIdentity>,
        endpoint_id: impl Into<String>,
        now_unix: u64,
    ) -> TicketLeaseRequest {
        TicketLeaseRequest {
            public_urls: vec!["https://relay.example".to_string()],
            quic_port: None,
            client: TicketClientKey::new(cluster_id, peer),
            endpoint_id: endpoint_id.into(),
            now_unix,
        }
    }

    #[test]
    fn relay_server_config_requires_origins_and_bounded_ticket_lifetime() {
        let mut config = test_config();
        config.validate().expect("valid config should pass");

        config.public_urls = vec!["https://rendezvous.example/nested".to_string()];
        assert!(config.validate().is_err());
        config.public_urls = vec!["https://rendezvous.example".to_string()];
        config.ticket_ttl = Duration::from_secs(299);
        assert!(config.validate().is_err());
        config.ticket_ttl = Duration::from_secs(3600);
        config.max_ticket_leases_per_client = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn tickets_are_endpoint_bound_tamper_evident_and_time_limited() {
        let authority = test_authority();
        let endpoint = SecretKey::generate().public().to_string();
        let other_endpoint = SecretKey::generate().public().to_string();
        let ticket = authority
            .issue(
                vec!["https://relay.example".to_string()],
                Some(7443),
                TicketClaims {
                    version: TICKET_VERSION,
                    cluster_id: ClusterId::now_v7(),
                    peer: None,
                    endpoint_id: endpoint.clone(),
                    lease_id: "lease-a".to_string(),
                    issued_at_unix: 7_200,
                    expires_at_unix: 10_800,
                },
            )
            .expect("ticket should issue");
        assert_eq!(ticket.quic_port, Some(7443));

        let claims = authority
            .verify(&ticket.auth_token, &endpoint, 7_201)
            .expect("ticket should verify");
        assert_eq!(claims.endpoint_id, endpoint);
        assert!(
            authority
                .verify(&ticket.auth_token, &other_endpoint, 7_201)
                .is_err()
        );
        assert!(
            authority
                .verify(&format!("{}x", ticket.auth_token), &endpoint, 7_201)
                .is_err()
        );
        assert!(
            authority
                .verify(&ticket.auth_token, &endpoint, ticket.expires_at_unix)
                .is_err()
        );
    }

    #[test]
    fn duplicate_endpoint_issuance_reuses_the_same_lease_without_consuming_capacity() {
        let authority = test_authority();
        let registry = TicketLeaseRegistry::new(&test_config());
        let endpoint = SecretKey::generate().public().to_string();
        let cluster_id = ClusterId::now_v7();
        let first = registry
            .issue_or_reuse(
                &authority,
                ticket_lease_request(cluster_id, None, endpoint.clone(), 7_200),
            )
            .expect("first ticket should issue");
        let second = registry
            .issue_or_reuse(
                &authority,
                ticket_lease_request(cluster_id, None, endpoint, 7_250),
            )
            .expect("second ticket should issue");
        assert_eq!(first, second);
    }

    #[test]
    fn ticket_lease_limit_and_issue_rate_apply_only_to_new_endpoints() {
        let authority = test_authority();
        let mut config = test_config();
        config.max_ticket_leases_per_client = 2;
        config.max_ticket_issues_per_minute = 2;
        let registry = TicketLeaseRegistry::new(&config);
        let cluster_id = ClusterId::now_v7();
        let peer = Some(PeerIdentity::Device(DeviceId::now_v7()));
        let endpoints = (0..3)
            .map(|_| SecretKey::generate().public().to_string())
            .collect::<Vec<_>>();

        let first = registry
            .issue_or_reuse(
                &authority,
                ticket_lease_request(cluster_id, peer.clone(), endpoints[0].clone(), 7_200),
            )
            .expect("first lease should issue");
        let duplicate = registry
            .issue_or_reuse(
                &authority,
                ticket_lease_request(cluster_id, peer.clone(), endpoints[0].clone(), 7_201),
            )
            .expect("duplicate endpoint should reuse its lease");
        assert_eq!(first, duplicate);
        registry
            .issue_or_reuse(
                &authority,
                ticket_lease_request(cluster_id, peer.clone(), endpoints[1].clone(), 7_201),
            )
            .expect("second lease should issue");
        let error = registry
            .issue_or_reuse(
                &authority,
                ticket_lease_request(cluster_id, peer.clone(), endpoints[2].clone(), 7_202),
            )
            .expect_err("third active lease must be rejected");
        assert!(error.message.contains("lease limit"));

        assert!(registry.release(cluster_id, peer.clone(), &endpoints[0], 7_203));
        let error = registry
            .issue_or_reuse(
                &authority,
                ticket_lease_request(cluster_id, peer, endpoints[2].clone(), 7_203),
            )
            .expect_err("release must not bypass the rolling issuance rate");
        assert!(error.message.contains("issue rate"));
        assert_eq!(error.retry_after_secs, 57);
    }

    #[tokio::test]
    async fn release_revokes_token_and_active_connection_limit_is_per_client() {
        let authority = test_authority();
        let mut config = test_config();
        config.max_active_connections_per_client = 1;
        let leases = TicketLeaseRegistry::new(&config);
        let cluster_id = ClusterId::now_v7();
        let peer = Some(PeerIdentity::Device(DeviceId::now_v7()));
        let first_endpoint = SecretKey::generate().public();
        let second_endpoint = SecretKey::generate().public();
        let issue = |endpoint_id: &iroh::EndpointId| {
            leases.issue_or_reuse(
                &authority,
                ticket_lease_request(
                    cluster_id,
                    peer.clone(),
                    endpoint_id.to_string(),
                    unix_timestamp(),
                ),
            )
        };
        let first_ticket = issue(&first_endpoint).expect("first ticket should issue");
        let second_ticket = issue(&second_endpoint).expect("second ticket should issue");
        let access = TicketAccess::new(authority, leases.clone());
        let first_request = client_request(first_endpoint, Some(&first_ticket.auth_token));
        let first_connection_id = first_request.connection_id();
        assert_eq!(access.on_connect(&first_request).await, Access::Allow);
        assert!(matches!(
            access
                .on_connect(&client_request(
                    second_endpoint,
                    Some(&second_ticket.auth_token)
                ))
                .await,
            Access::Deny { .. }
        ));

        access.on_disconnect(first_endpoint, first_connection_id);
        assert_eq!(
            access
                .on_connect(&client_request(
                    second_endpoint,
                    Some(&second_ticket.auth_token)
                ))
                .await,
            Access::Allow
        );
        assert!(leases.release(
            cluster_id,
            peer,
            &second_endpoint.to_string(),
            unix_timestamp()
        ));
        assert!(matches!(
            access
                .on_connect(&client_request(
                    second_endpoint,
                    Some(&second_ticket.auth_token)
                ))
                .await,
            Access::Deny { .. }
        ));
    }

    #[tokio::test]
    async fn relay_access_rejects_a_valid_ticket_from_another_endpoint() {
        let authority = test_authority();
        let leases = TicketLeaseRegistry::new(&test_config());
        let allowed_endpoint = SecretKey::generate().public();
        let other_endpoint = SecretKey::generate().public();
        let cluster_id = ClusterId::now_v7();
        let ticket = leases
            .issue_or_reuse(
                &authority,
                ticket_lease_request(
                    cluster_id,
                    None,
                    allowed_endpoint.to_string(),
                    unix_timestamp(),
                ),
            )
            .expect("ticket should issue");
        let access = TicketAccess::new(authority, leases.clone());

        assert_eq!(
            access
                .on_connect(&client_request(allowed_endpoint, Some(&ticket.auth_token)))
                .await,
            Access::Allow
        );
        assert!(matches!(
            access
                .on_connect(&client_request(other_endpoint, Some(&ticket.auth_token)))
                .await,
            Access::Deny { .. }
        ));
        assert!(matches!(
            access
                .on_connect(&client_request(other_endpoint, None))
                .await,
            Access::Deny { .. }
        ));
    }

    fn client_request(endpoint_id: iroh::EndpointId, token: Option<&str>) -> ClientRequest {
        let mut request = HttpRequest::builder()
            .uri(::iroh_relay::http::RELAY_PATH)
            .version(Version::HTTP_11);
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let (parts, ()) = request.body(()).expect("request should build").into_parts();
        ClientRequest::new(endpoint_id, ProtocolVersion::V1, parts)
    }
}
