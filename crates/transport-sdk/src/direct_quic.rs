use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use iroh::endpoint::{RecvStream, SendStream, presets};
use iroh::tls::CaTlsConfig;
use iroh::{Endpoint, EndpointAddr, RelayMode, RelayUrl, SecretKey, TransportAddr};
use iroh_relay::{RelayConfig, RelayMap};
use rustls_pki_types::CertificateDer;
use rustls_pki_types::pem::PemObject;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_util::compat::TokioAsyncReadCompatExt;

use crate::candidates::{CandidateKind, ConnectionCandidate, ConnectionCandidateTransportHints};
use crate::mux::{MultiplexConfig, MultiplexMode, MultiplexedSession};

const DIRECT_QUIC_ENDPOINT_SCHEME: &str = "iroh";
pub const DEFAULT_DIRECT_QUIC_ALPN: &str = "ironmesh/transport/1";

#[derive(Clone)]
pub struct DirectQuicEndpointConfig {
    pub secret_key: SecretKey,
    pub relay_urls: Vec<String>,
    pub relay_auth_token: Option<String>,
    pub relay_ca_pem: Option<String>,
    pub alpn: String,
}

impl std::fmt::Debug for DirectQuicEndpointConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DirectQuicEndpointConfig")
            .field("secret_key", &"[REDACTED]")
            .field("relay_urls", &self.relay_urls)
            .field(
                "relay_auth_token",
                &self.relay_auth_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("relay_ca_pem_configured", &self.relay_ca_pem.is_some())
            .field("alpn", &self.alpn)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct DirectQuicRelayConfig {
    pub url: String,
    pub auth_token: Option<String>,
}

impl std::fmt::Debug for DirectQuicRelayConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DirectQuicRelayConfig")
            .field("url", &self.url)
            .field(
                "auth_token",
                &self.auth_token.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectQuicEndpointSnapshot {
    pub endpoint_id: String,
    pub relay_url: Option<String>,
    pub direct_socket_addrs: Vec<String>,
    pub observed_socket_addrs: Vec<String>,
    pub alpn: String,
}

#[derive(Clone)]
pub struct DirectQuicEndpoint {
    endpoint: Endpoint,
    alpn: String,
    static_relays: Arc<BTreeMap<String, Option<String>>>,
    dynamic_relays: Arc<Mutex<BTreeMap<String, Option<String>>>>,
    configured_relays: Arc<Mutex<BTreeMap<String, Option<String>>>>,
}

pub struct DirectQuicAcceptedConnection {
    pub connection: iroh::endpoint::Connection,
    pub remote_endpoint_id: String,
}

pub struct DirectQuicSession {
    pub connection: iroh::endpoint::Connection,
    pub session: MultiplexedSession,
    pub remote_endpoint_id: String,
}

struct IrohBiStream {
    recv: RecvStream,
    send: SendStream,
}

impl DirectQuicEndpointConfig {
    pub fn new(secret_key: SecretKey) -> Self {
        Self {
            secret_key,
            relay_urls: Vec::new(),
            relay_auth_token: None,
            relay_ca_pem: None,
            alpn: DEFAULT_DIRECT_QUIC_ALPN.to_string(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.alpn.trim().is_empty() {
            bail!("direct QUIC ALPN must not be blank");
        }
        for relay_url in &self.relay_urls {
            relay_url
                .trim()
                .parse::<RelayUrl>()
                .with_context(|| format!("invalid direct QUIC relay URL {relay_url}"))?;
        }
        if self
            .relay_auth_token
            .as_deref()
            .is_some_and(|value| value.trim().is_empty())
        {
            bail!("direct QUIC relay auth token must not be blank when present");
        }
        if self.relay_urls.is_empty() && self.relay_auth_token.is_some() {
            bail!("direct QUIC relay auth token requires at least one relay URL");
        }
        if let Some(relay_ca_pem) = self.relay_ca_pem.as_deref() {
            relay_ca_config(relay_ca_pem)?;
        }
        Ok(())
    }
}

impl DirectQuicEndpoint {
    pub async fn bind(config: DirectQuicEndpointConfig) -> Result<Self> {
        config.validate()?;

        let configured_relays = config
            .relay_urls
            .iter()
            .map(|url| DirectQuicRelayConfig {
                url: url.trim().to_string(),
                auth_token: config.relay_auth_token.clone(),
            })
            .collect::<Vec<_>>();
        // Keep the relay transport present even when no static relay is known.
        // Iroh supports adding relays to an empty custom map at runtime, whereas
        // RelayMode::Disabled permanently omits the relay transport and later
        // insert_relay calls cannot bring the endpoint online.
        let relay_mode = RelayMode::Custom(relay_map(&configured_relays)?);

        let mut endpoint_builder = Endpoint::builder(presets::Minimal)
            .secret_key(config.secret_key)
            .relay_mode(relay_mode)
            .alpns(vec![config.alpn.as_bytes().to_vec()]);
        if let Some(relay_ca_pem) = config.relay_ca_pem.as_deref() {
            endpoint_builder = endpoint_builder.ca_tls_config(relay_ca_config(relay_ca_pem)?);
        }
        let endpoint = endpoint_builder
            .bind()
            .await
            .context("failed binding direct QUIC endpoint")?;

        let static_relays = configured_relays
            .into_iter()
            .map(|relay| {
                (
                    normalize_relay_url(&relay.url).to_string(),
                    relay.auth_token,
                )
            })
            .collect::<BTreeMap<_, _>>();
        Ok(Self {
            endpoint,
            alpn: config.alpn,
            static_relays: Arc::new(static_relays.clone()),
            dynamic_relays: Arc::new(Mutex::new(BTreeMap::new())),
            configured_relays: Arc::new(Mutex::new(static_relays)),
        })
    }

    /// Reconciles relay configurations learned from authenticated Rendezvous
    /// responses. Operator-provided static relays remain authoritative, while
    /// removed dynamic relays are withdrawn from the live iroh endpoint.
    pub async fn reconcile_dynamic_relays(
        &self,
        relays: &[DirectQuicRelayConfig],
    ) -> Result<usize> {
        let desired_dynamic = relays
            .iter()
            .map(|relay| {
                validate_relay_config(relay)?;
                Ok((
                    normalize_relay_url(&relay.url).to_string(),
                    relay.auth_token.clone(),
                ))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        let (removed, upserted) = {
            let mut dynamic = self
                .dynamic_relays
                .lock()
                .expect("direct QUIC dynamic relay config mutex poisoned");
            let mut configured = self
                .configured_relays
                .lock()
                .expect("direct QUIC relay config mutex poisoned");
            let mut desired_effective = desired_dynamic.clone();
            desired_effective.extend(
                self.static_relays
                    .iter()
                    .map(|(url, token)| (url.clone(), token.clone())),
            );

            let removed = configured
                .keys()
                .filter(|url| !desired_effective.contains_key(*url))
                .cloned()
                .collect::<Vec<_>>();
            let upserted = desired_effective
                .iter()
                .filter(|(url, token)| configured.get(*url) != Some(*token))
                .map(|(url, token)| DirectQuicRelayConfig {
                    url: url.clone(),
                    auth_token: token.clone(),
                })
                .collect::<Vec<_>>();
            *dynamic = desired_dynamic;
            *configured = desired_effective;
            (removed, upserted)
        };

        for url in &removed {
            let relay_url = url
                .parse::<RelayUrl>()
                .with_context(|| format!("invalid configured direct QUIC relay URL {url}"))?;
            self.endpoint.remove_relay(&relay_url).await;
        }
        for relay in &upserted {
            let (url, config) = iroh_relay_config(relay)?;
            self.endpoint.insert_relay(url, Arc::new(config)).await;
        }
        Ok(removed.len() + upserted.len())
    }

    pub async fn wait_until_online(&self) {
        self.endpoint.online().await;
    }

    pub fn endpoint_id(&self) -> String {
        self.endpoint.id().to_string()
    }

    pub fn snapshot(&self) -> DirectQuicEndpointSnapshot {
        let addr = self.endpoint.addr();
        let relay_url = addr.relay_urls().next().map(ToString::to_string);
        let observed_socket_addrs = addr.ip_addrs().map(ToString::to_string).collect::<Vec<_>>();
        let direct_socket_addrs = self
            .endpoint
            .bound_sockets()
            .into_iter()
            .map(|addr| addr.to_string())
            .collect::<Vec<_>>();

        DirectQuicEndpointSnapshot {
            endpoint_id: addr.id.to_string(),
            relay_url,
            direct_socket_addrs,
            observed_socket_addrs,
            alpn: self.alpn.clone(),
        }
    }

    pub fn candidate(&self) -> ConnectionCandidate {
        let mut candidate = self.snapshot().to_candidate();
        let relay_auth_token = candidate
            .transport_hints
            .as_ref()
            .and_then(|hints| hints.relay_url.as_deref())
            .and_then(|relay_url| {
                self.static_relays
                    .get(normalize_relay_url(relay_url))
                    .cloned()
                    .flatten()
            });
        if let Some(hints) = candidate.transport_hints.as_mut() {
            hints.relay_auth_token = relay_auth_token;
        }
        candidate
    }

    pub async fn connect_session(
        &self,
        candidate: &ConnectionCandidate,
        config: MultiplexConfig,
    ) -> Result<DirectQuicSession> {
        let endpoint_addr = endpoint_addr_from_candidate(candidate)?;
        let remote_endpoint_id = endpoint_addr.id.to_string();
        let alpn = candidate
            .transport_hints
            .as_ref()
            .and_then(|hints| hints.alpn.as_deref())
            .unwrap_or(self.alpn.as_str())
            .as_bytes()
            .to_vec();

        let connection = self
            .endpoint
            .connect(endpoint_addr, &alpn)
            .await
            .with_context(|| {
                format!("failed opening direct QUIC connection to {remote_endpoint_id}")
            })?;
        let (send, recv) = connection.open_bi().await.with_context(|| {
            format!("failed opening direct QUIC bi-stream to {remote_endpoint_id}")
        })?;
        let session = MultiplexedSession::spawn(
            IrohBiStream::new(recv, send).compat(),
            MultiplexMode::Client,
            config,
        )
        .with_context(|| {
            format!("failed creating direct QUIC multiplex session to {remote_endpoint_id}")
        })?;

        Ok(DirectQuicSession {
            connection,
            session,
            remote_endpoint_id,
        })
    }

    pub async fn accept_connection(&self) -> Result<Option<DirectQuicAcceptedConnection>> {
        let Some(incoming) = self.endpoint.accept().await else {
            return Ok(None);
        };
        let connection = incoming
            .accept()
            .context("failed accepting direct QUIC connection")?
            .await
            .context("direct QUIC connection handshake failed")?;
        let remote_endpoint_id = connection.remote_id().to_string();

        Ok(Some(DirectQuicAcceptedConnection {
            connection,
            remote_endpoint_id,
        }))
    }

    pub async fn accept_session(
        &self,
        config: MultiplexConfig,
    ) -> Result<Option<DirectQuicSession>> {
        let Some(accepted) = self.accept_connection().await? else {
            return Ok(None);
        };
        accepted.into_session(config).await.map(Some)
    }

    pub async fn close(&self) {
        self.endpoint.close().await;
    }
}

impl DirectQuicEndpointSnapshot {
    pub fn to_candidate(&self) -> ConnectionCandidate {
        ConnectionCandidate {
            kind: CandidateKind::DirectQuic,
            endpoint: direct_quic_endpoint_url(&self.endpoint_id),
            rtt_ms: None,
            transport_hints: Some(ConnectionCandidateTransportHints {
                transport_id: Some(self.endpoint_id.clone()),
                relay_url: self.relay_url.clone(),
                relay_auth_token: None,
                alpn: Some(self.alpn.clone()),
                direct_socket_addrs: self.direct_socket_addrs.clone(),
                observed_socket_addrs: self.observed_socket_addrs.clone(),
            }),
        }
    }
}

fn relay_map(relays: &[DirectQuicRelayConfig]) -> Result<RelayMap> {
    relays
        .iter()
        .map(iroh_relay_config)
        .map(|result| result.map(|(_, config)| config))
        .collect::<Result<Vec<_>>>()
        .map(RelayMap::from_iter)
}

fn iroh_relay_config(relay: &DirectQuicRelayConfig) -> Result<(RelayUrl, RelayConfig)> {
    validate_relay_config(relay)?;
    let url = relay
        .url
        .trim()
        .parse::<RelayUrl>()
        .with_context(|| format!("invalid direct QUIC relay URL {}", relay.url))?;
    let mut config = RelayConfig::new(url.clone(), None);
    if let Some(auth_token) = relay.auth_token.as_deref() {
        config = config.with_auth_token(auth_token);
    }
    Ok((url, config))
}

fn validate_relay_config(relay: &DirectQuicRelayConfig) -> Result<()> {
    relay
        .url
        .trim()
        .parse::<RelayUrl>()
        .with_context(|| format!("invalid direct QUIC relay URL {}", relay.url))?;
    if relay
        .auth_token
        .as_deref()
        .is_some_and(|value| value.trim().is_empty())
    {
        bail!("direct QUIC relay auth token must not be blank when present");
    }
    Ok(())
}

fn normalize_relay_url(url: &str) -> &str {
    url.trim().trim_end_matches('/')
}

fn relay_ca_config(pem: &str) -> Result<CaTlsConfig> {
    let mut reader = std::io::BufReader::new(pem.as_bytes());
    let roots = CertificateDer::pem_reader_iter(&mut reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("failed parsing direct QUIC relay CA certificate")?;
    if roots.is_empty() {
        bail!("direct QUIC relay CA PEM must contain at least one certificate");
    }
    Ok(CaTlsConfig::embedded().with_extra_roots(roots))
}

impl IrohBiStream {
    fn new(recv: RecvStream, send: SendStream) -> Self {
        Self { recv, send }
    }
}

impl DirectQuicAcceptedConnection {
    pub async fn into_session(self, config: MultiplexConfig) -> Result<DirectQuicSession> {
        let Self {
            connection,
            remote_endpoint_id,
        } = self;
        let (send, recv) = connection.accept_bi().await.with_context(|| {
            format!("failed accepting direct QUIC bi-stream from {remote_endpoint_id}")
        })?;
        let session = MultiplexedSession::spawn(
            IrohBiStream::new(recv, send).compat(),
            MultiplexMode::Server,
            config,
        )
        .with_context(|| {
            format!("failed creating direct QUIC multiplex session from {remote_endpoint_id}")
        })?;

        Ok(DirectQuicSession {
            connection,
            session,
            remote_endpoint_id,
        })
    }
}

impl AsyncRead for IrohBiStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.recv).poll_read(cx, buf)
    }
}

impl AsyncWrite for IrohBiStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.send)
            .poll_write(cx, buf)
            .map_err(write_error_to_io_error)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.send).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.send).poll_shutdown(cx)
    }
}

pub fn direct_quic_endpoint_url(endpoint_id: &str) -> String {
    format!("{DIRECT_QUIC_ENDPOINT_SCHEME}://{endpoint_id}")
}

pub fn endpoint_id_from_candidate(candidate: &ConnectionCandidate) -> Result<String> {
    if candidate.kind != CandidateKind::DirectQuic {
        bail!("endpoint id extraction requires a direct QUIC candidate");
    }

    let url = reqwest::Url::parse(candidate.endpoint.trim())
        .with_context(|| format!("invalid direct QUIC endpoint {}", candidate.endpoint))?;
    if url.scheme() != DIRECT_QUIC_ENDPOINT_SCHEME {
        bail!("direct QUIC candidate endpoint must use {DIRECT_QUIC_ENDPOINT_SCHEME}:// scheme");
    }
    let endpoint_id = url
        .host_str()
        .map(ToString::to_string)
        .or_else(|| {
            let value = candidate
                .endpoint
                .trim()
                .strip_prefix(&format!("{DIRECT_QUIC_ENDPOINT_SCHEME}://"))?
                .trim_matches('/');
            (!value.is_empty()).then_some(value.to_string())
        })
        .ok_or_else(|| anyhow!("direct QUIC candidate endpoint is missing endpoint id"))?;

    if let Some(transport_id) = candidate
        .transport_hints
        .as_ref()
        .and_then(|hints| hints.transport_id.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        && transport_id != endpoint_id
    {
        bail!(
            "direct QUIC candidate transport_id {transport_id} does not match endpoint id {endpoint_id}"
        );
    }

    Ok(endpoint_id)
}

pub fn endpoint_addr_from_candidate(candidate: &ConnectionCandidate) -> Result<EndpointAddr> {
    let endpoint_id = endpoint_id_from_candidate(candidate)?;
    let endpoint_id = endpoint_id
        .parse()
        .with_context(|| format!("invalid direct QUIC endpoint id {endpoint_id}"))?;

    let mut addrs = Vec::new();
    if let Some(hints) = candidate.transport_hints.as_ref() {
        if let Some(relay_url) = hints.relay_url.as_deref() {
            addrs.push(TransportAddr::Relay(
                relay_url
                    .parse::<RelayUrl>()
                    .with_context(|| format!("invalid direct QUIC relay URL {relay_url}"))?,
            ));
        }
        addrs.extend(socket_addrs_to_transport_addrs(&hints.direct_socket_addrs)?);
        addrs.extend(socket_addrs_to_transport_addrs(
            &hints.observed_socket_addrs,
        )?);
    }

    Ok(EndpointAddr::from_parts(endpoint_id, addrs))
}

pub fn read_secret_key_from_path(path: &Path) -> Result<SecretKey> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed reading direct QUIC secret key {}", path.display()))?;
    let bytes = base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(raw.trim())
        .with_context(|| format!("failed decoding direct QUIC secret key {}", path.display()))?;
    let bytes: [u8; 32] = bytes.try_into().map_err(|_| {
        anyhow!(
            "direct QUIC secret key {} must decode to exactly 32 bytes",
            path.display()
        )
    })?;
    Ok(SecretKey::from_bytes(&bytes))
}

pub fn write_secret_key_to_path(path: &Path, secret_key: &SecretKey) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed creating direct QUIC secret key directory {}",
                parent.display()
            )
        })?;
    }
    let payload = base64::engine::general_purpose::STANDARD_NO_PAD.encode(secret_key.to_bytes());
    fs::write(path, format!("{payload}\n"))
        .with_context(|| format!("failed writing direct QUIC secret key {}", path.display()))
}

pub fn load_or_create_secret_key(path: &Path) -> Result<SecretKey> {
    if path.exists() {
        return read_secret_key_from_path(path);
    }
    let secret_key = SecretKey::generate();
    write_secret_key_to_path(path, &secret_key)?;
    Ok(secret_key)
}

fn socket_addrs_to_transport_addrs(values: &[String]) -> Result<Vec<TransportAddr>> {
    let mut addrs = Vec::new();
    for value in values {
        let addr = value
            .trim()
            .parse::<SocketAddr>()
            .with_context(|| format!("invalid direct QUIC socket address {value}"))?;
        if !addrs.contains(&TransportAddr::Ip(addr)) {
            addrs.push(TransportAddr::Ip(addr));
        }
    }
    Ok(addrs)
}

fn write_error_to_io_error(error: iroh::endpoint::WriteError) -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_candidate_with_mismatched_authenticated_transport_id() {
        let candidate = ConnectionCandidate {
            kind: CandidateKind::DirectQuic,
            endpoint: "iroh://endpoint-key-a".to_string(),
            rtt_ms: None,
            transport_hints: Some(ConnectionCandidateTransportHints {
                transport_id: Some("endpoint-key-b".to_string()),
                ..ConnectionCandidateTransportHints::default()
            }),
        };

        let error = endpoint_id_from_candidate(&candidate)
            .expect_err("mismatched endpoint ids must be rejected");
        assert!(error.to_string().contains("does not match"));
    }

    #[test]
    fn snapshot_serializes_to_direct_quic_candidate() {
        let candidate = DirectQuicEndpointSnapshot {
            endpoint_id: "peer-key-1".to_string(),
            relay_url: Some("https://relay.example".to_string()),
            direct_socket_addrs: vec!["127.0.0.1:7000".to_string()],
            observed_socket_addrs: vec!["203.0.113.10:40000".to_string()],
            alpn: DEFAULT_DIRECT_QUIC_ALPN.to_string(),
        }
        .to_candidate();

        assert_eq!(candidate.kind, CandidateKind::DirectQuic);
        assert_eq!(candidate.endpoint, "iroh://peer-key-1");
        assert_eq!(
            candidate
                .transport_hints
                .as_ref()
                .and_then(|hints| hints.transport_id.as_deref()),
            Some("peer-key-1")
        );
    }

    #[test]
    fn secret_key_roundtrip_persists_exact_key() {
        let path =
            std::env::temp_dir().join(format!("ironmesh-iroh-key-{}.txt", uuid::Uuid::now_v7()));
        let secret_key = SecretKey::generate();

        write_secret_key_to_path(&path, &secret_key).expect("secret key should persist");
        let loaded = read_secret_key_from_path(&path).expect("secret key should load");
        std::fs::remove_file(&path).expect("temp secret key should be removed");

        assert_eq!(secret_key.to_bytes(), loaded.to_bytes());
    }

    #[tokio::test]
    async fn dynamic_relays_are_deduplicated_rotated_and_removed() {
        let endpoint =
            DirectQuicEndpoint::bind(DirectQuicEndpointConfig::new(SecretKey::generate()))
                .await
                .expect("direct QUIC endpoint should bind");
        let relay = DirectQuicRelayConfig {
            url: "http://127.0.0.1:9/".to_string(),
            auth_token: Some("first-token".to_string()),
        };

        assert_eq!(
            endpoint
                .reconcile_dynamic_relays(std::slice::from_ref(&relay))
                .await
                .expect("relay should be added"),
            1
        );
        assert_eq!(
            endpoint
                .candidate()
                .transport_hints
                .and_then(|hints| hints.relay_auth_token),
            None,
            "endpoint-bound dynamic tickets must never be advertised as candidate metadata"
        );
        assert_eq!(
            endpoint
                .reconcile_dynamic_relays(std::slice::from_ref(&relay))
                .await
                .expect("unchanged relay should be retained"),
            0
        );
        let rotated = DirectQuicRelayConfig {
            auth_token: Some("second-token".to_string()),
            ..relay
        };
        assert_eq!(
            endpoint
                .reconcile_dynamic_relays(&[rotated])
                .await
                .expect("relay token should rotate"),
            1
        );
        assert_eq!(
            endpoint
                .reconcile_dynamic_relays(&[])
                .await
                .expect("removed relay should be withdrawn"),
            1
        );
        assert!(
            endpoint
                .configured_relays
                .lock()
                .expect("relay config mutex should not be poisoned")
                .is_empty()
        );
        endpoint.close().await;
    }

    #[test]
    fn relay_configuration_debug_output_redacts_secrets() {
        let mut endpoint = DirectQuicEndpointConfig::new(SecretKey::generate());
        endpoint.relay_urls = vec!["https://relay.example".to_string()];
        endpoint.relay_auth_token = Some("sensitive-endpoint-token".to_string());
        let relay = DirectQuicRelayConfig {
            url: "https://relay.example".to_string(),
            auth_token: Some("sensitive-relay-token".to_string()),
        };

        let debug = format!("{endpoint:?} {relay:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("sensitive-endpoint-token"));
        assert!(!debug.contains("sensitive-relay-token"));
    }
}
