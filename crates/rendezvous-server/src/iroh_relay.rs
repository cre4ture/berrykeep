use std::collections::HashMap;
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

use crate::auth::{AuthenticatedPeer, WithAuthenticatedPeer, authenticated_peer_from_tls_stream};
use crate::{RendezvousServerTlsIdentity, auth::build_server_rustls_config};

const TICKET_FORMAT: &str = "imrt1";
const TICKET_VERSION: u8 = 1;
const TICKET_CLOCK_SKEW_SECS: u64 = 60;
const KEY_CACHE_CAPACITY: usize = 1024;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrohRelayServerConfig {
    pub public_urls: Vec<String>,
    pub ticket_ttl: Duration,
    pub client_rx_bytes_per_second: u32,
    pub client_rx_max_burst_bytes: u32,
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
        cluster_id: ClusterId,
        peer: Option<PeerIdentity>,
        endpoint_id: &str,
        now_unix: u64,
    ) -> Result<IrohRelayTicket> {
        endpoint_id
            .parse::<iroh::EndpointId>()
            .context("invalid iroh endpoint id for relay ticket")?;
        let rotation_secs = (self.ttl_secs / 4).max(60);
        let issued_at_unix = now_unix - (now_unix % rotation_secs);
        let expires_at_unix = issued_at_unix.saturating_add(self.ttl_secs);
        let claims = TicketClaims {
            version: TICKET_VERSION,
            cluster_id,
            peer,
            endpoint_id: endpoint_id.to_string(),
            issued_at_unix,
            expires_at_unix,
        };
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

#[derive(Clone)]
struct TicketAccess {
    authority: TicketAuthority,
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
    fn new(authority: TicketAuthority) -> Self {
        Self {
            authority,
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
                self.schedule_expiry(request, claims.expires_at_unix);
                Access::Allow
            }
            Err(error) => Access::Deny {
                reason: Some(error.to_string()),
            },
        }
    }

    fn on_disconnect(&self, _endpoint_id: iroh::EndpointId, connection_id: ConnectionId) {
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
        let access = Arc::new(TicketAccess::new(authority.clone()));
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
    ) -> Result<IrohRelayTicket> {
        self.authority.issue(
            self.config.public_urls.clone(),
            self.config.quic.as_ref().map(|quic| quic.public_port),
            cluster_id,
            peer,
            endpoint_id,
            unix_timestamp(),
        )
    }
}

pub(crate) async fn serve_same_port(
    bind_addr: SocketAddr,
    app: Router,
    tls_config: Option<axum_server::tls_rustls::RustlsConfig>,
    relay: RelayService,
) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    let tls_acceptor = tls_config.map(|config| tokio_rustls::TlsAcceptor::from(config.get_inner()));
    loop {
        let (stream, peer_addr) = listener.accept().await?;
        let app = app.clone();
        let relay = relay.clone();
        let tls_acceptor = tls_acceptor.clone();
        tokio::spawn(async move {
            let result =
                serve_same_port_connection(stream, peer_addr, app, relay, tls_acceptor).await;
            if let Err(error) = result {
                tracing::debug!(%error, %peer_addr, "same-port rendezvous connection ended");
            }
        });
    }
}

async fn serve_same_port_connection(
    stream: tokio::net::TcpStream,
    peer_addr: SocketAddr,
    app: Router,
    relay: RelayService,
    tls_acceptor: Option<tokio_rustls::TlsAcceptor>,
) -> Result<()> {
    if let Some(tls_acceptor) = tls_acceptor {
        let tls_stream = tokio::time::timeout(Duration::from_secs(10), tls_acceptor.accept(stream))
            .await
            .context("rendezvous TLS handshake timed out")?
            .context("rendezvous TLS handshake failed")?;
        let authenticated_peer = authenticated_peer_from_tls_stream(&tls_stream)?;
        let http2 = tls_stream.get_ref().1.alpn_protocol() == Some(b"h2");
        return serve_same_port_stream(
            MaybeTlsStream::Tls(tls_stream),
            peer_addr,
            authenticated_peer,
            app,
            relay,
            http2,
        )
        .await;
    }

    serve_same_port_stream(
        MaybeTlsStream::Plain(stream),
        peer_addr,
        None,
        app,
        relay,
        false,
    )
    .await
}

async fn serve_same_port_stream(
    stream: MaybeTlsStream,
    peer_addr: SocketAddr,
    authenticated_peer: Option<AuthenticatedPeer>,
    app: Router,
    relay: RelayService,
    http2: bool,
) -> Result<()> {
    let service = SamePortProtocolService::new(
        WithAuthenticatedPeer::new(app, authenticated_peer, Some(peer_addr)),
        Some(relay),
    );
    let service = TowerToHyperService::new(service);
    if http2 {
        http2::Builder::new(TokioExecutor::new())
            .serve_connection(TokioIo::new(stream), service)
            .await
            .context("same-port Rendezvous HTTP/2 connection failed")?;
    } else {
        http1::Builder::new()
            .serve_connection(TokioIo::new(stream), service)
            .with_upgrades()
            .await
            .context("same-port Rendezvous HTTP/1.1 connection failed")?;
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
    use iroh::SecretKey;
    use iroh_relay::http::ProtocolVersion;

    fn test_authority() -> TicketAuthority {
        TicketAuthority::from_key([7_u8; 32], Duration::from_secs(3600))
    }

    #[test]
    fn relay_server_config_requires_origins_and_bounded_ticket_lifetime() {
        let mut config = IrohRelayServerConfig {
            public_urls: vec!["https://rendezvous.example".to_string()],
            ticket_ttl: Duration::from_secs(3600),
            client_rx_bytes_per_second: 1024,
            client_rx_max_burst_bytes: 2048,
            quic: None,
        };
        config.validate().expect("valid config should pass");

        config.public_urls = vec!["https://rendezvous.example/nested".to_string()];
        assert!(config.validate().is_err());
        config.public_urls = vec!["https://rendezvous.example".to_string()];
        config.ticket_ttl = Duration::from_secs(299);
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
                ClusterId::now_v7(),
                None,
                &endpoint,
                7_200,
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
    fn tickets_are_stable_within_a_rotation_window() {
        let authority = test_authority();
        let endpoint = SecretKey::generate().public().to_string();
        let cluster_id = ClusterId::now_v7();
        let first = authority
            .issue(
                vec!["https://relay.example".to_string()],
                None,
                cluster_id,
                None,
                &endpoint,
                7_200,
            )
            .expect("first ticket should issue");
        let second = authority
            .issue(
                vec!["https://relay.example".to_string()],
                None,
                cluster_id,
                None,
                &endpoint,
                7_250,
            )
            .expect("second ticket should issue");
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn relay_access_rejects_a_valid_ticket_from_another_endpoint() {
        let authority = test_authority();
        let allowed_endpoint = SecretKey::generate().public();
        let other_endpoint = SecretKey::generate().public();
        let ticket = authority
            .issue(
                vec!["https://relay.example".to_string()],
                None,
                ClusterId::now_v7(),
                None,
                &allowed_endpoint.to_string(),
                unix_timestamp(),
            )
            .expect("ticket should issue");
        let access = TicketAccess::new(authority);

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
