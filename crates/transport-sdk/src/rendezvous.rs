use anyhow::{Context, Result, anyhow, bail};
use common::{ClusterId, NodeId};
use futures_util::stream::{FuturesUnordered, StreamExt};
use reqwest::{Certificate, Client, Url};
use rustls_pki_types::CertificateDer;
use rustls_pki_types::pem::PemObject;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::HashMap;
use std::io::Cursor;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use x509_parser::parse_x509_certificate;

use crate::bootstrap::RelayMode;
use crate::bootstrap_claim::{
    ClientBootstrapClaimPublishRequest, ClientBootstrapClaimPublishResponse,
};
use crate::candidates::ConnectionCandidate;
use crate::mux::{MultiplexConfig, MultiplexMode, MultiplexedSession};
use crate::peer::PeerIdentity;
use crate::relay::{RelayTicket, RelayTicketRequest};
use crate::relay_tunnel::{RelayTunnelAcceptRequest, RelayTunnelClient, RelayTunnelSession};
use crate::relay_wake::{RelayWakeClient, RelayWakeRegistration};

const MAX_RENDEZVOUS_ERROR_RESPONSE_BYTES: usize = 1024;

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IrohRelayAdvertisement {
    pub public_urls: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,
}

impl std::fmt::Debug for IrohRelayAdvertisement {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IrohRelayAdvertisement")
            .field("public_urls", &self.public_urls)
            .field(
                "auth_token",
                &self.auth_token.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IrohRelayTicketRequest {
    pub cluster_id: ClusterId,
    pub endpoint_id: String,
}

impl IrohRelayTicketRequest {
    pub fn validate(&self) -> Result<()> {
        if self.cluster_id.is_nil() {
            bail!("iroh relay ticket request must include a non-nil cluster_id");
        }
        self.endpoint_id
            .trim()
            .parse::<iroh::EndpointId>()
            .with_context(|| {
                format!("invalid iroh relay ticket endpoint_id {}", self.endpoint_id)
            })?;
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IrohRelayTicket {
    pub public_urls: Vec<String>,
    pub auth_token: String,
    pub expires_at_unix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quic_port: Option<u16>,
}

impl std::fmt::Debug for IrohRelayTicket {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IrohRelayTicket")
            .field("public_urls", &self.public_urls)
            .field("auth_token", &"[REDACTED]")
            .field("expires_at_unix", &self.expires_at_unix)
            .field("quic_port", &self.quic_port)
            .finish()
    }
}

impl IrohRelayTicket {
    pub fn validate(&self) -> Result<()> {
        if self.public_urls.is_empty() {
            bail!("iroh relay ticket must include at least one public URL");
        }
        validate_url_list("iroh relay ticket public_urls", &self.public_urls)?;
        if self.auth_token.trim().is_empty() {
            bail!("iroh relay ticket auth_token must not be blank");
        }
        if self.expires_at_unix == 0 {
            bail!("iroh relay ticket expires_at_unix must be greater than zero");
        }
        if self.quic_port == Some(0) {
            bail!("iroh relay ticket quic_port must be greater than zero when present");
        }
        Ok(())
    }
}

/// Endpoint-bound iroh relay tickets that arrive from independent Rendezvous
/// endpoints. The first ticket is available immediately; further valid tickets
/// can be consumed as their concurrent requests complete.
pub struct IrohRelayTicketCollection {
    first_ticket: IrohRelayTicket,
    additional_tickets: tokio::sync::mpsc::UnboundedReceiver<Result<IrohRelayTicket>>,
    _collection_task: AbortOnDrop,
}

impl IrohRelayTicketCollection {
    pub fn first_ticket(&self) -> &IrohRelayTicket {
        &self.first_ticket
    }

    /// Returns the next result from a request that was already started in
    /// parallel with the request that produced [`Self::first_ticket`].
    pub async fn next_ticket(&mut self) -> Option<Result<IrohRelayTicket>> {
        self.additional_tickets.recv().await
    }
}

struct AbortOnDrop {
    abort_handle: tokio::task::AbortHandle,
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.abort_handle.abort();
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TransportCapability {
    DirectHttps,
    DirectQuic,
    RelayTunnel,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RendezvousClientConfig {
    pub cluster_id: ClusterId,
    pub rendezvous_urls: Vec<String>,
    #[serde(default = "default_heartbeat_interval_secs")]
    pub heartbeat_interval_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PresenceRegistration {
    pub cluster_id: ClusterId,
    pub identity: PeerIdentity,
    #[serde(default)]
    pub public_api_url: Option<String>,
    #[serde(default)]
    pub public_direct_urls: Vec<String>,
    #[serde(default)]
    pub peer_api_url: Option<String>,
    #[serde(default)]
    pub direct_candidates: Vec<ConnectionCandidate>,
    #[serde(default)]
    pub labels: HashMap<String, String>,
    #[serde(default)]
    pub capacity_bytes: Option<u64>,
    #[serde(default)]
    pub free_bytes: Option<u64>,
    #[serde(default)]
    pub capabilities: Vec<TransportCapability>,
    #[serde(default)]
    pub relay_mode: RelayMode,
    pub connected_at_unix: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PresenceEntry {
    pub registration: PresenceRegistration,
    pub updated_at_unix: u64,
    #[serde(default)]
    pub observed_source_addr: Option<SocketAddr>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegisterPresenceResponse {
    pub accepted: bool,
    #[serde(default)]
    pub software_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iroh_relay: Option<IrohRelayAdvertisement>,
    pub updated_at_unix: u64,
    pub entry: PresenceEntry,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PresenceListResponse {
    pub registered_endpoints: usize,
    pub entries: Vec<PresenceEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscoveryResponse {
    #[serde(default)]
    pub rendezvous_peers: Vec<RendezvousEndpointStatus>,
    #[serde(default)]
    pub node_candidates: Option<Vec<ConnectionCandidate>>,
    #[serde(default)]
    pub node_relay_capable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RendezvousEndpointConnectionState {
    Unknown,
    Connected,
    Disconnected,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RendezvousEndpointStatus {
    pub url: String,
    pub status: RendezvousEndpointConnectionState,
    #[serde(default)]
    pub last_attempt_unix: Option<u64>,
    #[serde(default)]
    pub last_success_unix: Option<u64>,
    #[serde(default)]
    pub consecutive_failures: u64,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RendezvousRuntimeState {
    #[serde(default)]
    pub active_url: Option<String>,
    #[serde(default)]
    pub endpoint_statuses: Vec<RendezvousEndpointStatus>,
}

#[derive(Debug, Clone)]
pub struct RendezvousControlClient {
    config: RendezvousClientConfig,
    http: Client,
    server_ca_pem: Option<String>,
    client_identity_pem: Option<Vec<u8>>,
    runtime_state: Arc<Mutex<TrackedRendezvousRuntimeState>>,
}

#[derive(Debug, Default)]
struct TrackedRendezvousRuntimeState {
    active_url: Option<String>,
    endpoints: HashMap<String, RendezvousEndpointStatus>,
}

impl RendezvousClientConfig {
    pub fn validate(&self) -> Result<()> {
        if self.cluster_id.is_nil() {
            bail!("rendezvous client config must include a non-nil cluster_id");
        }
        if self.rendezvous_urls.is_empty() {
            bail!("rendezvous client config must include at least one rendezvous URL");
        }
        for url in &self.rendezvous_urls {
            if url.trim().is_empty() {
                bail!("rendezvous URLs must not contain empty values");
            }
            Url::parse(url.trim())?;
        }
        Ok(())
    }
}

impl PresenceRegistration {
    pub fn validate(&self) -> Result<()> {
        if self.cluster_id.is_nil() {
            bail!("presence registration must include a non-nil cluster_id");
        }
        validate_optional_url("public_api_url", self.public_api_url.as_deref())?;
        validate_url_list("public_direct_urls", &self.public_direct_urls)?;
        validate_optional_url("peer_api_url", self.peer_api_url.as_deref())?;
        for candidate in &self.direct_candidates {
            candidate.validate()?;
        }
        Ok(())
    }
}

impl RendezvousControlClient {
    pub fn new(
        config: RendezvousClientConfig,
        server_ca_pem: Option<&str>,
        client_identity_pem: Option<&[u8]>,
    ) -> Result<Self> {
        config.validate()?;

        let builder = Client::builder();
        let builder = if let Some(server_ca_pem) = server_ca_pem {
            builder.add_root_certificate(
                Certificate::from_pem(server_ca_pem.as_bytes())
                    .context("failed to parse rendezvous server CA certificate")?,
            )
        } else {
            builder
        };
        let builder = if let Some(client_identity_pem) = client_identity_pem {
            builder.identity(
                reqwest::Identity::from_pem(client_identity_pem)
                    .context("failed to parse rendezvous client identity PEM")?,
            )
        } else {
            builder
        };

        let http = builder
            .build()
            .context("failed building rendezvous control HTTP client")?;
        Ok(Self {
            runtime_state: Arc::new(Mutex::new(TrackedRendezvousRuntimeState::new(
                &config.rendezvous_urls,
            ))),
            server_ca_pem: server_ca_pem.map(ToString::to_string),
            client_identity_pem: client_identity_pem.map(|value| value.to_vec()),
            config,
            http,
        })
    }

    pub fn config(&self) -> &RendezvousClientConfig {
        &self.config
    }

    pub fn runtime_state(&self) -> RendezvousRuntimeState {
        self.runtime_state
            .lock()
            .expect("rendezvous runtime state lock poisoned")
            .snapshot(&self.config.rendezvous_urls)
    }

    pub fn client_identity_expiry_diagnostic(&self) -> Option<String> {
        self.client_identity_pem
            .as_deref()
            .and_then(|client_identity_pem| {
                rendezvous_client_identity_expiry_diagnostic_at(
                    client_identity_pem,
                    unix_timestamp(),
                )
            })
    }

    pub fn client_identity_needs_renewal(&self) -> bool {
        self.client_identity_pem
            .as_deref()
            .is_some_and(|pem| rendezvous_client_identity_needs_renewal_at(pem, unix_timestamp()))
    }

    pub async fn probe_endpoints(&self) -> Result<RendezvousRuntimeState> {
        self.probe_endpoints_with_path("/control/presence").await
    }

    pub async fn probe_health_endpoints(&self) -> Result<RendezvousRuntimeState> {
        self.probe_endpoints_with_path("/health").await
    }

    async fn probe_endpoints_with_path(&self, path: &str) -> Result<RendezvousRuntimeState> {
        for base_url in &self.config.rendezvous_urls {
            let url = control_url(base_url, path)?;
            let result = match self.http.get(url.clone()).send().await {
                Ok(response) if response.status().is_success() => Ok(()),
                Ok(response) => Err(rendezvous_response_error(&url, response).await),
                Err(err) => Err(self.decorate_transport_error(format!(
                    "failed contacting rendezvous endpoint {url}: {err}"
                ))),
            };
            self.record_endpoint_result(base_url, result, false);
        }

        Ok(self.runtime_state())
    }

    pub async fn register_presence(
        &self,
        registration: &PresenceRegistration,
    ) -> Result<RegisterPresenceResponse> {
        registration.validate()?;
        if registration.cluster_id != self.config.cluster_id {
            bail!(
                "presence registration cluster_id {} does not match rendezvous client cluster_id {}",
                registration.cluster_id,
                self.config.cluster_id
            );
        }
        self.post_json("/control/presence/register", registration)
            .await
    }

    pub async fn list_presence(&self) -> Result<PresenceListResponse> {
        self.get_json(&format!(
            "/control/presence?cluster_id={}",
            self.config.cluster_id
        ))
        .await
    }

    pub async fn fetch_mesh(&self) -> Result<RendezvousRuntimeState> {
        self.get_json("/control/mesh").await
    }

    pub async fn fetch_discovery(&self, node_id: Option<NodeId>) -> Result<DiscoveryResponse> {
        let mut last_error = None;
        for base_url in &self.config.rendezvous_urls {
            let mut url = control_url(base_url, "/control/discovery")?;
            url.query_pairs_mut()
                .append_pair("cluster_id", &self.config.cluster_id.to_string());
            if let Some(node_id) = node_id {
                url.query_pairs_mut()
                    .append_pair("node_id", &node_id.to_string());
            }

            match self.http.get(url.clone()).send().await {
                Ok(response) if response.status().is_success() => {
                    match response.json::<DiscoveryResponse>().await {
                        Ok(payload) => {
                            self.record_endpoint_result(base_url, Ok(()), true);
                            return Ok(payload);
                        }
                        Err(err) => {
                            let message =
                                format!("failed decoding rendezvous response from {url}: {err}");
                            self.record_endpoint_result(base_url, Err(message.clone()), true);
                            return Err(anyhow!(message));
                        }
                    }
                }
                Ok(response) => {
                    let message = rendezvous_response_error(&url, response).await;
                    self.record_endpoint_result(base_url, Err(message.clone()), true);
                    last_error = Some(anyhow!(message));
                }
                Err(err) => {
                    let message = self.decorate_transport_error(format!(
                        "failed contacting rendezvous endpoint {url}: {err}"
                    ));
                    self.record_endpoint_result(base_url, Err(message.clone()), true);
                    last_error = Some(anyhow!(message));
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow!("rendezvous client has no configured URLs")))
    }

    pub async fn issue_relay_ticket(&self, request: &RelayTicketRequest) -> Result<RelayTicket> {
        request.validate()?;
        if request.cluster_id != self.config.cluster_id {
            bail!(
                "relay ticket request cluster_id {} does not match rendezvous client cluster_id {}",
                request.cluster_id,
                self.config.cluster_id
            );
        }
        let ticket: RelayTicket = self.post_json("/control/relay/ticket", request).await?;
        if ticket.security_mode != request.security_mode {
            bail!(
                "rendezvous relay ticket security mode {:?} does not match requested mode {:?}",
                ticket.security_mode,
                request.security_mode
            );
        }
        Ok(ticket)
    }

    pub async fn issue_iroh_relay_ticket(&self, endpoint_id: &str) -> Result<IrohRelayTicket> {
        let request = IrohRelayTicketRequest {
            cluster_id: self.config.cluster_id,
            endpoint_id: endpoint_id.trim().to_string(),
        };
        request.validate()?;
        let ticket: IrohRelayTicket = self
            .post_json_first_valid(
                "/control/iroh-relay/ticket",
                &request,
                IrohRelayTicket::validate,
            )
            .await?;
        Ok(ticket)
    }

    /// Starts requests against every configured Rendezvous endpoint at once.
    ///
    /// The returned collection is ready as soon as the first validated ticket
    /// arrives. It deliberately does not wait for slower endpoints, whose
    /// valid endpoint-bound tickets remain available through
    /// [`IrohRelayTicketCollection::next_ticket`].
    pub async fn issue_iroh_relay_tickets_progressively(
        &self,
        endpoint_id: &str,
    ) -> Result<IrohRelayTicketCollection> {
        let request = IrohRelayTicketRequest {
            cluster_id: self.config.cluster_id,
            endpoint_id: endpoint_id.trim().to_string(),
        };
        request.validate()?;

        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let client = self.clone();
        let task = tokio::spawn(async move {
            let mut attempts = FuturesUnordered::new();
            for base_url in &client.config.rendezvous_urls {
                let client = client.clone();
                let base_url = base_url.clone();
                let request = request.clone();
                attempts.push(async move {
                    client
                        .issue_iroh_relay_ticket_from_endpoint(base_url, request)
                        .await
                });
            }

            while let Some(result) = attempts.next().await {
                if sender.send(result).is_err() {
                    break;
                }
            }
        });
        let collection_task = AbortOnDrop {
            abort_handle: task.abort_handle(),
        };
        let mut last_error = None;

        while let Some(result) = receiver.recv().await {
            match result {
                Ok(first_ticket) => {
                    return Ok(IrohRelayTicketCollection {
                        first_ticket,
                        additional_tickets: receiver,
                        _collection_task: collection_task,
                    });
                }
                Err(error) => last_error = Some(error),
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow!("rendezvous client has no configured URLs")))
    }

    pub async fn publish_bootstrap_claim(
        &self,
        request: &ClientBootstrapClaimPublishRequest,
    ) -> Result<ClientBootstrapClaimPublishResponse> {
        request.validate()?;
        if request.cluster_id != self.config.cluster_id {
            bail!(
                "bootstrap claim publish request cluster_id {} does not match rendezvous client cluster_id {}",
                request.cluster_id,
                self.config.cluster_id
            );
        }
        self.post_json("/control/bootstrap-claims/publish", request)
            .await
    }

    pub async fn connect_relay_tunnel_source(
        &self,
        ticket: &RelayTicket,
    ) -> Result<RelayTunnelClient> {
        ticket.validate()?;
        if ticket.cluster_id != self.config.cluster_id {
            bail!(
                "relay tunnel ticket cluster_id {} does not match rendezvous client cluster_id {}",
                ticket.cluster_id,
                self.config.cluster_id
            );
        }

        let relay_urls = if ticket.relay_urls.is_empty() {
            self.config.rendezvous_urls.as_slice()
        } else {
            ticket.relay_urls.as_slice()
        };
        let mut last_error = None;
        for base_url in relay_urls {
            match RelayTunnelClient::connect_source(
                base_url,
                self.server_ca_pem.as_deref(),
                self.client_identity_pem.as_deref(),
                ticket,
            )
            .await
            {
                Ok(client) => {
                    self.record_endpoint_result(base_url, Ok(()), true);
                    return Ok(client);
                }
                Err(err) => {
                    let message = self.decorate_transport_error(format!(
                        "failed establishing relay tunnel source at {base_url}: {err}"
                    ));
                    self.record_endpoint_result(base_url, Err(message.clone()), true);
                    last_error = Some(anyhow!(message));
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow!("rendezvous client has no configured URLs")))
    }

    pub async fn accept_relay_tunnel(
        &self,
        request: &RelayTunnelAcceptRequest,
    ) -> Result<RelayTunnelClient> {
        request.validate()?;
        if request.cluster_id != self.config.cluster_id {
            bail!(
                "relay tunnel accept request cluster_id {} does not match rendezvous client cluster_id {}",
                request.cluster_id,
                self.config.cluster_id
            );
        }

        let mut last_error = None;
        for base_url in &self.config.rendezvous_urls {
            match RelayTunnelClient::accept_target(
                base_url,
                self.server_ca_pem.as_deref(),
                self.client_identity_pem.as_deref(),
                request.clone(),
            )
            .await
            {
                Ok(client) => {
                    self.record_endpoint_result(base_url, Ok(()), true);
                    return Ok(client);
                }
                Err(err) => {
                    let message = self.decorate_transport_error(format!(
                        "failed accepting relay tunnel target at {base_url}: {err}"
                    ));
                    self.record_endpoint_result(base_url, Err(message.clone()), true);
                    last_error = Some(anyhow!(message));
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow!("rendezvous client has no configured URLs")))
    }

    /// Registers on the long-lived relay "wake" channel: instead of the caller having
    /// to repeatedly reopen a short-lived `accept_relay_tunnel` connection to poll for
    /// a peer, it holds this one connection open and gets pushed a wake the instant a
    /// peer is waiting, then reacts by dialing `accept_relay_tunnel` on demand.
    pub async fn connect_relay_wake(
        &self,
        registration: &RelayWakeRegistration,
    ) -> Result<RelayWakeClient> {
        registration.validate()?;
        if registration.cluster_id != self.config.cluster_id {
            bail!(
                "relay wake registration cluster_id {} does not match rendezvous client cluster_id {}",
                registration.cluster_id,
                self.config.cluster_id
            );
        }

        let mut last_error = None;
        for base_url in &self.config.rendezvous_urls {
            match RelayWakeClient::connect(
                base_url,
                self.server_ca_pem.as_deref(),
                self.client_identity_pem.as_deref(),
                registration.clone(),
            )
            .await
            {
                Ok(client) => {
                    self.record_endpoint_result(base_url, Ok(()), true);
                    return Ok(client);
                }
                Err(err) => {
                    let message = self.decorate_transport_error(format!(
                        "failed connecting relay wake channel at {base_url}: {err}"
                    ));
                    self.record_endpoint_result(base_url, Err(message.clone()), true);
                    last_error = Some(anyhow!(message));
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow!("rendezvous client has no configured URLs")))
    }

    pub async fn connect_relay_legacy_plaintext_multiplex_source(
        &self,
        ticket: &RelayTicket,
        config: MultiplexConfig,
    ) -> Result<(RelayTunnelSession, MultiplexedSession)> {
        let mut multiplex_ticket = ticket.clone();
        multiplex_ticket.session_kind = crate::relay::RelayTunnelSessionKind::MultiplexTransport;
        self.connect_relay_tunnel_source(&multiplex_ticket)
            .await?
            .into_legacy_plaintext_multiplexed_session(MultiplexMode::Client, config)
    }

    /// Deprecated compatibility alias for
    /// [`Self::connect_relay_legacy_plaintext_multiplex_source`].
    #[deprecated(
        since = "1.0.34",
        note = "use connect_relay_legacy_plaintext_multiplex_source for explicit legacy behavior"
    )]
    pub async fn connect_relay_multiplex_source(
        &self,
        ticket: &RelayTicket,
        config: MultiplexConfig,
    ) -> Result<(RelayTunnelSession, MultiplexedSession)> {
        self.connect_relay_legacy_plaintext_multiplex_source(ticket, config)
            .await
    }

    pub async fn accept_relay_legacy_plaintext_multiplex_target(
        &self,
        request: &RelayTunnelAcceptRequest,
        config: MultiplexConfig,
    ) -> Result<(RelayTunnelSession, MultiplexedSession)> {
        let mut multiplex_request = request.clone();
        multiplex_request.session_kind = crate::relay::RelayTunnelSessionKind::MultiplexTransport;
        self.accept_relay_tunnel(&multiplex_request)
            .await?
            .into_legacy_plaintext_multiplexed_session(MultiplexMode::Server, config)
    }

    /// Deprecated compatibility alias for
    /// [`Self::accept_relay_legacy_plaintext_multiplex_target`].
    #[deprecated(
        since = "1.0.34",
        note = "use accept_relay_legacy_plaintext_multiplex_target for explicit legacy behavior"
    )]
    pub async fn accept_relay_multiplex_target(
        &self,
        request: &RelayTunnelAcceptRequest,
        config: MultiplexConfig,
    ) -> Result<(RelayTunnelSession, MultiplexedSession)> {
        self.accept_relay_legacy_plaintext_multiplex_target(request, config)
            .await
    }

    async fn get_json<T>(&self, path: &str) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let mut last_error = None;
        for base_url in &self.config.rendezvous_urls {
            let url = control_url(base_url, path)?;
            match self.http.get(url.clone()).send().await {
                Ok(response) if response.status().is_success() => {
                    match response.json::<T>().await {
                        Ok(payload) => {
                            self.record_endpoint_result(base_url, Ok(()), true);
                            return Ok(payload);
                        }
                        Err(err) => {
                            let message =
                                format!("failed decoding rendezvous response from {url}: {err}");
                            self.record_endpoint_result(base_url, Err(message.clone()), true);
                            return Err(anyhow!(message));
                        }
                    }
                }
                Ok(response) => {
                    let message = rendezvous_response_error(&url, response).await;
                    self.record_endpoint_result(base_url, Err(message.clone()), true);
                    last_error = Some(anyhow!(message));
                }
                Err(err) => {
                    let message = self.decorate_transport_error(format!(
                        "failed contacting rendezvous endpoint {url}: {err}"
                    ));
                    self.record_endpoint_result(base_url, Err(message.clone()), true);
                    last_error = Some(anyhow!(message));
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow!("rendezvous client has no configured URLs")))
    }

    async fn post_json<Body, T>(&self, path: &str, body: &Body) -> Result<T>
    where
        Body: Serialize + ?Sized,
        T: DeserializeOwned,
    {
        let mut last_error = None;
        for base_url in &self.config.rendezvous_urls {
            let url = control_url(base_url, path)?;
            match self.http.post(url.clone()).json(body).send().await {
                Ok(response) if response.status().is_success() => {
                    match response.json::<T>().await {
                        Ok(payload) => {
                            self.record_endpoint_result(base_url, Ok(()), true);
                            return Ok(payload);
                        }
                        Err(err) => {
                            let message =
                                format!("failed decoding rendezvous response from {url}: {err}");
                            self.record_endpoint_result(base_url, Err(message.clone()), true);
                            return Err(anyhow!(message));
                        }
                    }
                }
                Ok(response) => {
                    let message = rendezvous_response_error(&url, response).await;
                    self.record_endpoint_result(base_url, Err(message.clone()), true);
                    last_error = Some(anyhow!(message));
                }
                Err(err) => {
                    let message = self.decorate_transport_error(format!(
                        "failed contacting rendezvous endpoint {url}: {err}"
                    ));
                    self.record_endpoint_result(base_url, Err(message.clone()), true);
                    last_error = Some(anyhow!(message));
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow!("rendezvous client has no configured URLs")))
    }

    async fn post_json_first_valid<Body, T, Validate>(
        &self,
        path: &str,
        body: &Body,
        validate: Validate,
    ) -> Result<T>
    where
        Body: Serialize + ?Sized,
        T: DeserializeOwned,
        Validate: Fn(&T) -> Result<()>,
    {
        let mut attempts = FuturesUnordered::new();
        let mut errors = std::iter::repeat_with(|| None)
            .take(self.config.rendezvous_urls.len())
            .collect::<Vec<Option<anyhow::Error>>>();

        for (index, base_url) in self.config.rendezvous_urls.iter().enumerate() {
            let url = control_url(base_url, path)?;
            let request = self.http.post(url.clone()).json(body);
            let base_url = base_url.clone();
            let client_identity_pem = self.client_identity_pem.as_deref();
            attempts.push(async move {
                let result = match request.send().await {
                    Ok(response) if response.status().is_success() => {
                        response.json::<T>().await.map_err(|error| {
                            format!("failed decoding rendezvous response from {url}: {error}")
                        })
                    }
                    Ok(response) => Err(rendezvous_response_error(&url, response).await),
                    Err(error) => Err(decorate_rendezvous_transport_error(
                        format!("failed contacting rendezvous endpoint {url}: {error}"),
                        client_identity_pem,
                        unix_timestamp(),
                    )),
                };
                (index, base_url, url, result)
            });
        }

        while let Some((index, base_url, url, result)) = attempts.next().await {
            match result {
                Ok(payload) => match validate(&payload) {
                    Ok(()) => {
                        self.record_endpoint_result(&base_url, Ok(()), true);
                        return Ok(payload);
                    }
                    Err(error) => {
                        let message = format!("invalid rendezvous response from {url}: {error}");
                        self.record_endpoint_result(&base_url, Err(message.clone()), true);
                        errors[index] = Some(anyhow!(message));
                    }
                },
                Err(message) => {
                    self.record_endpoint_result(&base_url, Err(message.clone()), true);
                    errors[index] = Some(anyhow!(message));
                }
            }
        }

        Err(errors
            .into_iter()
            .rev()
            .flatten()
            .next()
            .unwrap_or_else(|| anyhow!("rendezvous client has no configured URLs")))
    }

    async fn issue_iroh_relay_ticket_from_endpoint(
        &self,
        base_url: String,
        request: IrohRelayTicketRequest,
    ) -> Result<IrohRelayTicket> {
        let url = control_url(&base_url, "/control/iroh-relay/ticket")?;
        let result = match self.http.post(url.clone()).json(&request).send().await {
            Ok(response) if response.status().is_success() => {
                match response.json::<IrohRelayTicket>().await {
                    Ok(ticket) => ticket.validate().map(|()| ticket).map_err(|error| {
                        format!("invalid rendezvous response from {url}: {error}")
                    }),
                    Err(error) => Err(format!(
                        "failed decoding rendezvous response from {url}: {error}"
                    )),
                }
            }
            Ok(response) => Err(rendezvous_response_error(&url, response).await),
            Err(error) => Err(self.decorate_transport_error(format!(
                "failed contacting rendezvous endpoint {url}: {error}"
            ))),
        };

        match result {
            Ok(ticket) => {
                self.record_endpoint_result(&base_url, Ok(()), true);
                Ok(ticket)
            }
            Err(message) => {
                self.record_endpoint_result(&base_url, Err(message.clone()), true);
                Err(anyhow!(message))
            }
        }
    }

    fn record_endpoint_result(
        &self,
        base_url: &str,
        result: std::result::Result<(), String>,
        mark_active: bool,
    ) {
        self.runtime_state
            .lock()
            .expect("rendezvous runtime state lock poisoned")
            .record_result(base_url, result, mark_active);
    }

    fn decorate_transport_error(&self, message: String) -> String {
        decorate_rendezvous_transport_error(
            message,
            self.client_identity_pem.as_deref(),
            unix_timestamp(),
        )
    }
}

async fn rendezvous_response_error(url: &Url, response: reqwest::Response) -> String {
    let status = response.status();
    let status_kind = if status.is_client_error() {
        "client error"
    } else if status.is_server_error() {
        "server error"
    } else {
        "unexpected status"
    };
    let mut message =
        format!("rendezvous endpoint {url} returned HTTP status {status_kind} ({status})");
    if let Some(detail) = rendezvous_response_error_detail(response).await {
        message.push_str(": ");
        message.push_str(&detail);
    }
    message
}

async fn rendezvous_response_error_detail(mut response: reqwest::Response) -> Option<String> {
    let mut body = Vec::new();
    let mut truncated = false;

    while let Some(chunk) = response.chunk().await.ok()? {
        let remaining = MAX_RENDEZVOUS_ERROR_RESPONSE_BYTES.saturating_sub(body.len());
        if chunk.len() > remaining {
            body.extend_from_slice(&chunk[..remaining]);
            truncated = true;
            break;
        }
        body.extend_from_slice(&chunk);
    }

    let detail = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| String::from_utf8_lossy(&body).into_owned());
    let detail = detail
        .chars()
        .filter(|character| !character.is_control() || character.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if detail.is_empty() {
        return None;
    }

    if truncated {
        Some(format!("{detail} [response truncated]"))
    } else {
        Some(detail)
    }
}

pub fn is_expected_idle_relay_tunnel_accept_timeout(error: &str) -> bool {
    error.contains("timed out waiting for relay tunnel source")
}

impl TrackedRendezvousRuntimeState {
    fn new(urls: &[String]) -> Self {
        let mut state = Self::default();
        state.ensure_urls(urls);
        state
    }

    fn ensure_urls(&mut self, urls: &[String]) {
        for url in urls {
            let normalized = normalized_endpoint_url(url);
            self.endpoints
                .entry(normalized.clone())
                .or_insert_with(|| RendezvousEndpointStatus {
                    url: normalized,
                    status: RendezvousEndpointConnectionState::Unknown,
                    last_attempt_unix: None,
                    last_success_unix: None,
                    consecutive_failures: 0,
                    last_error: None,
                    active: false,
                });
        }
    }

    fn record_result(
        &mut self,
        base_url: &str,
        result: std::result::Result<(), String>,
        mark_active: bool,
    ) {
        let now = unix_timestamp();
        let normalized = normalized_endpoint_url(base_url);
        let endpoint =
            self.endpoints
                .entry(normalized.clone())
                .or_insert_with(|| RendezvousEndpointStatus {
                    url: normalized.clone(),
                    status: RendezvousEndpointConnectionState::Unknown,
                    last_attempt_unix: None,
                    last_success_unix: None,
                    consecutive_failures: 0,
                    last_error: None,
                    active: false,
                });
        endpoint.last_attempt_unix = Some(now);

        match result {
            Ok(()) => {
                endpoint.status = RendezvousEndpointConnectionState::Connected;
                endpoint.last_success_unix = Some(now);
                endpoint.consecutive_failures = 0;
                endpoint.last_error = None;
                if mark_active {
                    self.active_url = Some(endpoint.url.clone());
                }
            }
            Err(error) if is_expected_idle_relay_tunnel_accept_timeout(&error) => {}
            Err(error) => {
                endpoint.status = RendezvousEndpointConnectionState::Disconnected;
                endpoint.consecutive_failures = endpoint.consecutive_failures.saturating_add(1);
                endpoint.last_error = Some(error);
                if mark_active && self.active_url.as_deref() == Some(endpoint.url.as_str()) {
                    self.active_url = None;
                }
            }
        }
    }

    fn snapshot(&self, urls: &[String]) -> RendezvousRuntimeState {
        let mut endpoint_statuses = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for url in urls {
            let normalized = normalized_endpoint_url(url);
            if !seen.insert(normalized.clone()) {
                continue;
            }
            if let Some(endpoint) = self.endpoints.get(&normalized) {
                let mut endpoint = endpoint.clone();
                endpoint.active = self.active_url.as_deref() == Some(endpoint.url.as_str());
                endpoint_statuses.push(endpoint);
            } else {
                endpoint_statuses.push(RendezvousEndpointStatus {
                    url: normalized.clone(),
                    status: RendezvousEndpointConnectionState::Unknown,
                    last_attempt_unix: None,
                    last_success_unix: None,
                    consecutive_failures: 0,
                    last_error: None,
                    active: self.active_url.as_deref() == Some(normalized.as_str()),
                });
            }
        }

        RendezvousRuntimeState {
            active_url: self.active_url.clone(),
            endpoint_statuses,
        }
    }
}

fn default_heartbeat_interval_secs() -> u64 {
    15
}

fn normalized_endpoint_url(value: &str) -> String {
    value.trim().trim_end_matches('/').to_string()
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|value| value.as_secs())
        .unwrap_or(0)
}

fn control_url(base_url: &str, path: &str) -> Result<Url> {
    Url::parse(base_url.trim())
        .with_context(|| format!("invalid rendezvous base URL {}", base_url))?
        .join(path.trim_start_matches('/'))
        .with_context(|| {
            format!("failed to build rendezvous control URL from {base_url} and {path}")
        })
}

fn validate_optional_url(field_name: &str, value: Option<&str>) -> Result<()> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(());
    };
    Url::parse(value).with_context(|| format!("invalid {field_name} URL {value}"))?;
    Ok(())
}

fn validate_url_list(field_name: &str, values: &[String]) -> Result<()> {
    let mut seen = HashMap::new();
    for value in values {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            bail!("{field_name} must not contain empty URLs");
        }
        let parsed =
            Url::parse(trimmed).with_context(|| format!("invalid {field_name} URL {trimmed}"))?;
        let normalized = parsed.to_string();
        if seen.insert(normalized.clone(), ()).is_some() {
            bail!("{field_name} must not contain duplicate URLs");
        }
    }
    Ok(())
}

fn decorate_rendezvous_transport_error(
    message: String,
    client_identity_pem: Option<&[u8]>,
    now_unix: u64,
) -> String {
    let Some(client_identity_pem) = client_identity_pem else {
        return message;
    };
    let Some(diagnostic) =
        rendezvous_client_identity_expiry_diagnostic_at(client_identity_pem, now_unix)
    else {
        return message;
    };
    format!("{message}; {diagnostic}")
}

pub const RENDEZVOUS_IDENTITY_RENEWAL_WINDOW_SECS: u64 = 7 * 24 * 60 * 60;

pub fn rendezvous_client_identity_needs_renewal_at(pem: &[u8], now_unix: u64) -> bool {
    rendezvous_client_identity_not_after_unix(pem)
        .ok()
        .is_some_and(|not_after| {
            not_after <= now_unix.saturating_add(RENDEZVOUS_IDENTITY_RENEWAL_WINDOW_SECS)
        })
}

pub fn rendezvous_client_identity_is_expired_at(pem: &[u8], now_unix: u64) -> bool {
    rendezvous_client_identity_not_after_unix(pem)
        .ok()
        .is_some_and(|not_after| not_after <= now_unix)
}

/// Returns whether the end-entity certificate in a rendezvous client identity contains the
/// cluster URI SAN expected by the bootstrap that is about to use it.
///
/// Older client identities predate cluster-scoped rendezvous authentication and contain only a
/// device URI SAN.  Those identities are still sufficient to authenticate a direct API renewal
/// request, but cannot authenticate to a current rendezvous endpoint.  Keeping this check beside
/// the other identity parsing helpers lets callers migrate them before attempting relay or
/// discovery traffic.
pub fn rendezvous_client_identity_has_expected_cluster_uri_san(
    client_identity_pem: &[u8],
    expected_cluster_id: ClusterId,
) -> Result<bool> {
    let certificate_der = rendezvous_client_identity_certificate_der(client_identity_pem)?;
    let (_, certificate) = parse_x509_certificate(certificate_der.as_ref())
        .map_err(|error| anyhow!("failed parsing rendezvous client certificate: {error}"))?;
    let expected_cluster_uri = format!("urn:ironmesh:cluster:{expected_cluster_id}");

    Ok(certificate.extensions().iter().any(|extension| {
        matches!(
            extension.parsed_extension(),
            x509_parser::extensions::ParsedExtension::SubjectAlternativeName(san)
                if san.general_names.iter().any(|name| {
                    matches!(
                        name,
                        x509_parser::extensions::GeneralName::URI(uri)
                            if *uri == expected_cluster_uri
                    )
                })
        )
    }))
}

fn rendezvous_client_identity_expiry_diagnostic_at(
    client_identity_pem: &[u8],
    now_unix: u64,
) -> Option<String> {
    let not_after_unix = rendezvous_client_identity_not_after_unix(client_identity_pem).ok()?;
    if not_after_unix > now_unix {
        return None;
    }
    Some(format!(
        "local rendezvous client identity is expired at {}",
        format_diagnostic_timestamp(not_after_unix)
    ))
}

pub fn rendezvous_client_identity_not_after_unix(client_identity_pem: &[u8]) -> Result<u64> {
    let certificate_der = rendezvous_client_identity_certificate_der(client_identity_pem)?;
    let (_, certificate) = parse_x509_certificate(certificate_der.as_ref())
        .map_err(|error| anyhow!("failed parsing rendezvous client certificate: {error}"))?;
    let not_after_unix = certificate.validity().not_after.timestamp();
    if not_after_unix < 0 {
        bail!("rendezvous client certificate not_after is before the unix epoch");
    }
    Ok(not_after_unix as u64)
}

fn rendezvous_client_identity_certificate_der(
    client_identity_pem: &[u8],
) -> Result<CertificateDer<'static>> {
    let mut cert_reader = Cursor::new(client_identity_pem);
    let certificate = CertificateDer::pem_reader_iter(&mut cert_reader)
        .next()
        .transpose()
        .context("failed parsing rendezvous client certificate chain")?
        .ok_or_else(|| anyhow!("rendezvous client identity PEM is missing a certificate chain"))?;

    Ok(certificate)
}

fn format_diagnostic_timestamp(unix: u64) -> String {
    OffsetDateTime::from_unix_timestamp(unix as i64)
        .ok()
        .and_then(|value| value.format(&Rfc3339).ok())
        .map(|value| format!("{value} (unix {unix})"))
        .unwrap_or_else(|| format!("unix {unix}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::{RelayTunnelSecurityMode, RelayTunnelSessionKind};
    use axum::extract::Query;
    use axum::http::StatusCode;
    use axum::{
        Json, Router,
        routing::{get, post},
    };
    use common::NodeId;
    use rcgen::{CertificateParams, KeyPair, SanType};
    use std::collections::HashMap;
    use uuid::Uuid;

    const TEST_RENDEZVOUS_CLIENT_CERT_PEM: &str = concat!(
        "-----BEGIN CERTIFICATE-----\n",
        "MIIB3DCCAYKgAwIBAgITK3r0r5jwkdN+susWXewPKMOgPDAKBggqhkjOPQQDAjBA\n",
        "MT4wPAYDVQQDDDVpcm9ubWVzaC1jbHVzdGVyLTAxOWQwMmViLWFiMzktNzIyMC05\n",
        "MTFhLWMwZWFmY2IzODI0OTAeFw0yNjAzMjExMzA5MzRaFw0yNjA0MjAxMzA5MzRa\n",
        "MD8xPTA7BgNVBAMMNGlyb25tZXNoLWRldmljZS0wMTlkMTA4My1lYTIzLTdiZjEt\n",
        "YjVjYi0xZDVmY2ViNTBlOGEwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAASeG/Cl\n",
        "E3s04e07hBjVXH8/IMPXIiGewwOLPXEcJM4pU0ELoDcfpgZ0evvEiOKFC+R19CI3\n",
        "/dbbU02U0VnXMMXxo1wwWjBDBgNVHREEPDA6hjh1cm46aXJvbm1lc2g6ZGV2aWNl\n",
        "OjAxOWQxMDgzLWVhMjMtN2JmMS1iNWNiLTFkNWZjZWI1MGU4YTATBgNVHSUEDDAK\n",
        "BggrBgEFBQcDAjAKBggqhkjOPQQDAgNIADBFAiBPOa5XZSZLs8CqhQO9PscDS2Il\n",
        "jkjn2HXRB0g2pB2aeAIhALe+yYYMAqULo8WmhjcudAgQm/1vYSjowEWtUcMCY2J3\n",
        "-----END CERTIFICATE-----\n"
    );

    fn rendezvous_identity_pem_with_cluster_san(cluster_id: ClusterId) -> String {
        let key_pair = KeyPair::generate().expect("test identity key should generate");
        let mut params = CertificateParams::new(Vec::new()).expect("test certificate params");
        params.subject_alt_names = vec![SanType::URI(
            format!("urn:ironmesh:cluster:{cluster_id}")
                .try_into()
                .expect("test cluster SAN should parse"),
        )];
        let certificate = params
            .self_signed(&key_pair)
            .expect("test identity certificate should issue");
        format!("{}{}", certificate.pem(), key_pair.serialize_pem())
    }

    #[test]
    fn iroh_relay_ticket_types_validate_and_redact_credentials() {
        let endpoint_id = iroh::SecretKey::generate().public().to_string();
        IrohRelayTicketRequest {
            cluster_id: ClusterId::now_v7(),
            endpoint_id,
        }
        .validate()
        .expect("valid endpoint-bound request should pass");

        let ticket = IrohRelayTicket {
            public_urls: vec!["https://rendezvous.example".to_string()],
            auth_token: "sensitive-endpoint-ticket".to_string(),
            expires_at_unix: 10_000,
            quic_port: Some(7842),
        };
        ticket.validate().expect("valid relay ticket should pass");
        let debug = format!("{ticket:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("sensitive-endpoint-ticket"));
    }

    #[test]
    fn iroh_relay_ticket_request_rejects_invalid_endpoint_id() {
        let error = IrohRelayTicketRequest {
            cluster_id: ClusterId::now_v7(),
            endpoint_id: "not-an-iroh-endpoint".to_string(),
        }
        .validate()
        .expect_err("invalid endpoint ID should fail");
        assert!(error.to_string().contains("endpoint_id"));
    }

    #[tokio::test]
    async fn issue_iroh_relay_ticket_races_rendezvous_endpoints() {
        let cluster_id = ClusterId::now_v7();
        let endpoint_id = iroh::SecretKey::generate().public().to_string();
        let requests_started = Arc::new(tokio::sync::Barrier::new(2));

        let stalled_barrier = Arc::clone(&requests_started);
        let stalled_router = Router::new().route(
            "/control/iroh-relay/ticket",
            post(move || {
                let stalled_barrier = Arc::clone(&stalled_barrier);
                async move {
                    stalled_barrier.wait().await;
                    std::future::pending::<StatusCode>().await
                }
            }),
        );
        let stalled_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("stalled listener should bind");
        let stalled_addr = stalled_listener
            .local_addr()
            .expect("stalled listener address");
        let stalled_server = tokio::spawn(async move {
            axum::serve(stalled_listener, stalled_router)
                .await
                .expect("stalled rendezvous server should run");
        });

        let healthy_barrier = Arc::clone(&requests_started);
        let expected_endpoint_id = endpoint_id.clone();
        let healthy_router = Router::new().route(
            "/control/iroh-relay/ticket",
            post(move |Json(request): Json<IrohRelayTicketRequest>| {
                let healthy_barrier = Arc::clone(&healthy_barrier);
                let expected_endpoint_id = expected_endpoint_id.clone();
                async move {
                    assert_eq!(request.cluster_id, cluster_id);
                    assert_eq!(request.endpoint_id, expected_endpoint_id);
                    healthy_barrier.wait().await;
                    Json(IrohRelayTicket {
                        public_urls: vec!["https://relay.example".to_string()],
                        auth_token: "healthy-endpoint-ticket".to_string(),
                        expires_at_unix: unix_timestamp() + 60,
                        quic_port: Some(7842),
                    })
                }
            }),
        );
        let healthy_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("healthy listener should bind");
        let healthy_addr = healthy_listener
            .local_addr()
            .expect("healthy listener address");
        let healthy_server = tokio::spawn(async move {
            axum::serve(healthy_listener, healthy_router)
                .await
                .expect("healthy rendezvous server should run");
        });

        let healthy_url = format!("http://{healthy_addr}");
        let client = RendezvousControlClient::new(
            RendezvousClientConfig {
                cluster_id,
                rendezvous_urls: vec![format!("http://{stalled_addr}"), healthy_url.clone()],
                heartbeat_interval_secs: 15,
            },
            None,
            None,
        )
        .expect("rendezvous client should build");

        let ticket = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            client.issue_iroh_relay_ticket(&endpoint_id),
        )
        .await
        .expect("healthy second endpoint should beat the stalled first endpoint")
        .expect("healthy endpoint should issue a valid ticket");

        assert_eq!(ticket.auth_token, "healthy-endpoint-ticket");
        assert_eq!(
            client.runtime_state().active_url.as_deref(),
            Some(healthy_url.as_str())
        );

        stalled_server.abort();
        let _ = stalled_server.await;
        healthy_server.abort();
        let _ = healthy_server.await;
    }

    #[tokio::test]
    async fn issue_iroh_relay_tickets_progressively_keeps_slow_valid_tickets() {
        let cluster_id = ClusterId::now_v7();
        let endpoint_id = iroh::SecretKey::generate().public().to_string();
        let slow_request_started = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let release_slow_response = Arc::new(tokio::sync::Notify::new());

        let expected_endpoint_id = endpoint_id.clone();
        let slow_request_started_for_handler = Arc::clone(&slow_request_started);
        let release_slow_response_for_handler = Arc::clone(&release_slow_response);
        let slow_router = Router::new().route(
            "/control/iroh-relay/ticket",
            post(move |Json(request): Json<IrohRelayTicketRequest>| {
                let expected_endpoint_id = expected_endpoint_id.clone();
                let slow_request_started = Arc::clone(&slow_request_started_for_handler);
                let release_slow_response = Arc::clone(&release_slow_response_for_handler);
                async move {
                    assert_eq!(request.cluster_id, cluster_id);
                    assert_eq!(request.endpoint_id, expected_endpoint_id);
                    slow_request_started.store(1, std::sync::atomic::Ordering::SeqCst);
                    release_slow_response.notified().await;
                    Json(IrohRelayTicket {
                        public_urls: vec!["https://strato-relay.example".to_string()],
                        auth_token: "strato-endpoint-ticket".to_string(),
                        expires_at_unix: unix_timestamp() + 60,
                        quic_port: Some(7842),
                    })
                }
            }),
        );
        let slow_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("slow listener should bind");
        let slow_addr = slow_listener.local_addr().expect("slow listener address");
        let slow_server = tokio::spawn(async move {
            axum::serve(slow_listener, slow_router)
                .await
                .expect("slow rendezvous server should run");
        });

        let expected_endpoint_id = endpoint_id.clone();
        let fast_router = Router::new().route(
            "/control/iroh-relay/ticket",
            post(move |Json(request): Json<IrohRelayTicketRequest>| {
                let expected_endpoint_id = expected_endpoint_id.clone();
                async move {
                    assert_eq!(request.cluster_id, cluster_id);
                    assert_eq!(request.endpoint_id, expected_endpoint_id);
                    Json(IrohRelayTicket {
                        public_urls: vec!["https://relay.example".to_string()],
                        auth_token: "fast-endpoint-ticket".to_string(),
                        expires_at_unix: unix_timestamp() + 60,
                        quic_port: Some(7842),
                    })
                }
            }),
        );
        let fast_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("fast listener should bind");
        let fast_addr = fast_listener.local_addr().expect("fast listener address");
        let fast_server = tokio::spawn(async move {
            axum::serve(fast_listener, fast_router)
                .await
                .expect("fast rendezvous server should run");
        });

        let client = RendezvousControlClient::new(
            RendezvousClientConfig {
                cluster_id,
                rendezvous_urls: vec![format!("http://{slow_addr}"), format!("http://{fast_addr}")],
                heartbeat_interval_secs: 15,
            },
            None,
            None,
        )
        .expect("rendezvous client should build");

        let mut tickets = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            client.issue_iroh_relay_tickets_progressively(&endpoint_id),
        )
        .await
        .expect("first ticket should not wait for the slow rendezvous endpoint")
        .expect("fast rendezvous endpoint should issue a valid ticket");
        assert_eq!(tickets.first_ticket().auth_token, "fast-endpoint-ticket");

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while slow_request_started.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("slow request should have started alongside the fast request");
        release_slow_response.notify_one();

        let slow_ticket =
            tokio::time::timeout(std::time::Duration::from_secs(1), tickets.next_ticket())
                .await
                .expect("slow valid ticket should arrive after the first ticket")
                .expect("ticket collection should still be open")
                .expect("slow rendezvous endpoint should issue a valid ticket");
        assert_eq!(slow_ticket.auth_token, "strato-endpoint-ticket");

        slow_server.abort();
        let _ = slow_server.await;
        fast_server.abort();
        let _ = fast_server.await;
    }

    #[tokio::test]
    async fn issue_relay_ticket_rejects_inner_mtls_downgrade() {
        let cluster_id = Uuid::now_v7();
        let router = Router::new().route(
            "/control/relay/ticket",
            post(|Json(request): Json<RelayTicketRequest>| async move {
                Json(serde_json::json!({
                    "cluster_id": request.cluster_id,
                    "session_id": "legacy-response",
                    "source": request.source,
                    "target": request.target,
                    "session_kind": request.session_kind,
                    "relay_urls": ["https://relay.example"],
                    "issued_at_unix": 1,
                    "expires_at_unix": 61
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let addr = listener.local_addr().expect("listener addr");
        let server = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("test rendezvous server should run");
        });
        let client = RendezvousControlClient::new(
            RendezvousClientConfig {
                cluster_id,
                rendezvous_urls: vec![format!("http://{addr}")],
                heartbeat_interval_secs: 15,
            },
            None,
            None,
        )
        .expect("rendezvous client should build");

        let error = client
            .issue_relay_ticket(&RelayTicketRequest {
                cluster_id,
                source: PeerIdentity::Device(Uuid::now_v7()),
                target: PeerIdentity::Node(NodeId::now_v7()),
                session_kind: RelayTunnelSessionKind::MultiplexTransport,
                security_mode: RelayTunnelSecurityMode::InnerMtls,
                requested_expires_in_secs: Some(60),
            })
            .await
            .expect_err("a legacy response must not downgrade an inner-mTLS request");

        assert!(error.to_string().contains("does not match requested mode"));
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn issue_relay_ticket_includes_bounded_rendezvous_error_detail() {
        let cluster_id = Uuid::now_v7();
        let router = Router::new().route(
            "/control/relay/ticket",
            post(|| async {
                (
                    StatusCode::UNAUTHORIZED,
                    "relay ticket source device:requested does not match authenticated rendezvous client device:certificate",
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let addr = listener.local_addr().expect("listener addr");
        let server = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("test rendezvous server should run");
        });
        let client = RendezvousControlClient::new(
            RendezvousClientConfig {
                cluster_id,
                rendezvous_urls: vec![format!("http://{addr}")],
                heartbeat_interval_secs: 15,
            },
            None,
            None,
        )
        .expect("rendezvous client should build");

        let error = client
            .issue_relay_ticket(&RelayTicketRequest {
                cluster_id,
                source: PeerIdentity::Device(Uuid::now_v7()),
                target: PeerIdentity::Node(NodeId::now_v7()),
                session_kind: RelayTunnelSessionKind::MultiplexTransport,
                security_mode: RelayTunnelSecurityMode::InnerMtls,
                requested_expires_in_secs: Some(60),
            })
            .await
            .expect_err("an unauthorized ticket request must fail");

        let message = error.to_string();
        assert!(message.contains("HTTP status client error (401 Unauthorized)"));
        assert!(message.contains("relay ticket source device:requested"));
        assert!(message.contains("authenticated rendezvous client device:certificate"));
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn issue_relay_ticket_truncates_oversized_rendezvous_error_detail() {
        let cluster_id = Uuid::now_v7();
        let oversized_detail = format!(
            "relay ticket rejection: {}",
            "x".repeat(MAX_RENDEZVOUS_ERROR_RESPONSE_BYTES)
        );
        let router = Router::new().route(
            "/control/relay/ticket",
            post(move || {
                let oversized_detail = oversized_detail.clone();
                async move { (StatusCode::BAD_REQUEST, oversized_detail) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let addr = listener.local_addr().expect("listener addr");
        let server = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("test rendezvous server should run");
        });
        let client = RendezvousControlClient::new(
            RendezvousClientConfig {
                cluster_id,
                rendezvous_urls: vec![format!("http://{addr}")],
                heartbeat_interval_secs: 15,
            },
            None,
            None,
        )
        .expect("rendezvous client should build");

        let error = client
            .issue_relay_ticket(&RelayTicketRequest {
                cluster_id,
                source: PeerIdentity::Device(Uuid::now_v7()),
                target: PeerIdentity::Node(NodeId::now_v7()),
                session_kind: RelayTunnelSessionKind::MultiplexTransport,
                security_mode: RelayTunnelSecurityMode::InnerMtls,
                requested_expires_in_secs: Some(60),
            })
            .await
            .expect_err("an invalid ticket request must fail");

        let message = error.to_string();
        assert!(message.contains("HTTP status client error (400 Bad Request)"));
        assert!(message.contains("relay ticket rejection:"));
        assert!(message.contains("[response truncated]"));
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn runtime_state_tracks_failed_and_active_rendezvous_endpoints() {
        let cluster_id = Uuid::now_v7();
        let unused_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("unused listener should bind");
        let unused_addr = unused_listener
            .local_addr()
            .expect("unused listener should expose addr");

        let observed_presence_clusters = Arc::new(Mutex::new(Vec::new()));
        let observed_presence_clusters_for_handler = Arc::clone(&observed_presence_clusters);
        let router = Router::new().route(
            "/control/presence",
            get(
                move |Query(query): Query<HashMap<String, String>>| async move {
                    observed_presence_clusters_for_handler
                        .lock()
                        .expect("observed presence query lock poisoned")
                        .push(query.get("cluster_id").cloned());
                    Json(PresenceListResponse {
                        registered_endpoints: 0,
                        entries: Vec::new(),
                    })
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let addr = listener.local_addr().expect("listener addr");
        drop(unused_listener);
        let server = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("test rendezvous server should run");
        });

        let healthy_url = format!("http://{addr}");
        let client = RendezvousControlClient::new(
            RendezvousClientConfig {
                cluster_id,
                rendezvous_urls: vec![format!("http://{unused_addr}"), healthy_url.clone()],
                heartbeat_interval_secs: 15,
            },
            None,
            None,
        )
        .expect("rendezvous client should build");

        client
            .list_presence()
            .await
            .expect("list presence should succeed");
        assert_eq!(
            observed_presence_clusters
                .lock()
                .expect("observed presence query lock poisoned")
                .as_slice(),
            &[Some(cluster_id.to_string())]
        );

        let runtime_state = client.runtime_state();
        assert_eq!(
            runtime_state.active_url.as_deref(),
            Some(healthy_url.as_str())
        );
        assert_eq!(runtime_state.endpoint_statuses.len(), 2);
        assert_eq!(
            runtime_state.endpoint_statuses[0].status,
            RendezvousEndpointConnectionState::Disconnected
        );
        assert_eq!(
            runtime_state.endpoint_statuses[1].status,
            RendezvousEndpointConnectionState::Connected
        );
        assert!(runtime_state.endpoint_statuses[1].active);

        let probed_state = client
            .probe_endpoints()
            .await
            .expect("probing endpoints should succeed");
        assert_eq!(
            probed_state.active_url.as_deref(),
            Some(healthy_url.as_str())
        );
        assert!(probed_state.endpoint_statuses[1].active);

        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn fetch_discovery_returns_mesh_and_node_candidates() {
        let cluster_id = Uuid::now_v7();
        let node_id = NodeId::now_v7();
        let router = Router::new().route(
            "/control/discovery",
            get(
                move |Query(query): Query<HashMap<String, String>>| async move {
                    assert_eq!(query.get("node_id"), Some(&node_id.to_string()));
                    assert_eq!(query.get("cluster_id"), Some(&cluster_id.to_string()));
                    Json(DiscoveryResponse {
                        rendezvous_peers: vec![RendezvousEndpointStatus {
                            url: "https://peer-rendezvous.example".to_string(),
                            status: RendezvousEndpointConnectionState::Connected,
                            last_attempt_unix: Some(10),
                            last_success_unix: Some(10),
                            consecutive_failures: 0,
                            last_error: None,
                            active: false,
                        }],
                        node_candidates: Some(vec![ConnectionCandidate {
                            kind: crate::CandidateKind::ServerReflexive,
                            endpoint: "https://203.0.113.10:7443".to_string(),
                            rtt_ms: None,
                            transport_hints: None,
                        }]),
                        node_relay_capable: true,
                    })
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let addr = listener.local_addr().expect("listener addr");
        let server = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("test rendezvous server should run");
        });

        let client = RendezvousControlClient::new(
            RendezvousClientConfig {
                cluster_id,
                rendezvous_urls: vec![format!("http://{addr}")],
                heartbeat_interval_secs: 15,
            },
            None,
            None,
        )
        .expect("rendezvous client should build");

        let discovery = client
            .fetch_discovery(Some(node_id))
            .await
            .expect("discovery fetch should succeed");

        assert_eq!(discovery.rendezvous_peers.len(), 1);
        assert_eq!(
            discovery.rendezvous_peers[0].url,
            "https://peer-rendezvous.example"
        );
        assert_eq!(
            discovery.node_candidates,
            Some(vec![ConnectionCandidate {
                kind: crate::CandidateKind::ServerReflexive,
                endpoint: "https://203.0.113.10:7443".to_string(),
                rtt_ms: None,
                transport_hints: None,
            }])
        );
        assert!(discovery.node_relay_capable);

        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn fetch_discovery_without_node_id_includes_cluster_scope() {
        let cluster_id = Uuid::now_v7();
        let router = Router::new().route(
            "/control/discovery",
            get(
                move |Query(query): Query<HashMap<String, String>>| async move {
                    assert_eq!(query.get("cluster_id"), Some(&cluster_id.to_string()));
                    assert!(!query.contains_key("node_id"));
                    Json(DiscoveryResponse {
                        rendezvous_peers: Vec::new(),
                        node_candidates: None,
                        node_relay_capable: false,
                    })
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let addr = listener.local_addr().expect("listener addr");
        let server = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("test rendezvous server should run");
        });
        let client = RendezvousControlClient::new(
            RendezvousClientConfig {
                cluster_id,
                rendezvous_urls: vec![format!("http://{addr}")],
                heartbeat_interval_secs: 15,
            },
            None,
            None,
        )
        .expect("rendezvous client should build");

        client
            .fetch_discovery(None)
            .await
            .expect("mesh discovery should include its cluster scope");

        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn health_probe_succeeds_when_presence_endpoint_is_unauthorized() {
        let cluster_id = Uuid::now_v7();
        let router = Router::new()
            .route(
                "/health",
                get(|| async { Json(serde_json::json!({ "status": "ok" })) }),
            )
            .route(
                "/control/presence",
                get(|| async { (StatusCode::UNAUTHORIZED, "node certificate required") }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let addr = listener.local_addr().expect("listener addr");
        let server = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("test rendezvous server should run");
        });

        let healthy_url = format!("http://{addr}");
        let client = RendezvousControlClient::new(
            RendezvousClientConfig {
                cluster_id,
                rendezvous_urls: vec![healthy_url.clone()],
                heartbeat_interval_secs: 15,
            },
            None,
            None,
        )
        .expect("rendezvous client should build");

        let presence_probe = client
            .probe_endpoints()
            .await
            .expect("probe should complete");
        assert_eq!(
            presence_probe.endpoint_statuses[0].status,
            RendezvousEndpointConnectionState::Disconnected
        );
        assert!(
            presence_probe.endpoint_statuses[0]
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("401"))
        );

        let health_probe = client
            .probe_health_endpoints()
            .await
            .expect("health probe should succeed");
        assert_eq!(health_probe.active_url, None);
        assert_eq!(
            health_probe.endpoint_statuses[0].status,
            RendezvousEndpointConnectionState::Connected
        );
        assert!(!health_probe.endpoint_statuses[0].active);
        assert_eq!(health_probe.endpoint_statuses[0].url, healthy_url);

        server.abort();
        let _ = server.await;
    }

    #[test]
    fn rendezvous_identity_cluster_san_check_distinguishes_current_and_legacy_certificates() {
        let expected_cluster_id = ClusterId::now_v7();
        let correct_identity = rendezvous_identity_pem_with_cluster_san(expected_cluster_id);
        let wrong_identity = rendezvous_identity_pem_with_cluster_san(ClusterId::now_v7());

        assert!(
            rendezvous_client_identity_has_expected_cluster_uri_san(
                correct_identity.as_bytes(),
                expected_cluster_id,
            )
            .expect("current identity should parse")
        );
        assert!(
            !rendezvous_client_identity_has_expected_cluster_uri_san(
                wrong_identity.as_bytes(),
                expected_cluster_id,
            )
            .expect("identity with another cluster SAN should parse")
        );
        assert!(
            !rendezvous_client_identity_has_expected_cluster_uri_san(
                TEST_RENDEZVOUS_CLIENT_CERT_PEM.as_bytes(),
                expected_cluster_id,
            )
            .expect("legacy identity should parse")
        );
    }

    #[test]
    fn expired_rendezvous_identity_diagnostic_is_detected_at_boundary() {
        let not_after_unix =
            rendezvous_client_identity_not_after_unix(TEST_RENDEZVOUS_CLIENT_CERT_PEM.as_bytes())
                .expect("test certificate should parse");

        assert!(
            rendezvous_client_identity_expiry_diagnostic_at(
                TEST_RENDEZVOUS_CLIENT_CERT_PEM.as_bytes(),
                not_after_unix.saturating_sub(1)
            )
            .is_none()
        );

        let diagnostic = rendezvous_client_identity_expiry_diagnostic_at(
            TEST_RENDEZVOUS_CLIENT_CERT_PEM.as_bytes(),
            not_after_unix,
        )
        .expect("diagnostic should be emitted once the certificate expires");
        assert!(diagnostic.contains("local rendezvous client identity is expired at"));
        assert!(diagnostic.contains("unix"));
    }

    #[test]
    fn transport_error_decoration_appends_expired_identity_diagnostic() {
        let not_after_unix =
            rendezvous_client_identity_not_after_unix(TEST_RENDEZVOUS_CLIENT_CERT_PEM.as_bytes())
                .expect("test certificate should parse");

        let message = decorate_rendezvous_transport_error(
            "failed contacting rendezvous endpoint https://relay.example/health: synthetic failure"
                .to_string(),
            Some(TEST_RENDEZVOUS_CLIENT_CERT_PEM.as_bytes()),
            not_after_unix.saturating_add(1),
        );

        assert!(message.contains("failed contacting rendezvous endpoint"));
        assert!(message.contains("local rendezvous client identity is expired at"));
    }

    #[test]
    fn renewal_check_returns_false_when_cert_is_well_outside_window() {
        let not_after =
            rendezvous_client_identity_not_after_unix(TEST_RENDEZVOUS_CLIENT_CERT_PEM.as_bytes())
                .expect("test certificate should parse");
        // A full window + 1 second before the window opens — cert is healthy
        let before_window = not_after
            .saturating_sub(RENDEZVOUS_IDENTITY_RENEWAL_WINDOW_SECS)
            .saturating_sub(1);
        assert!(
            !rendezvous_client_identity_needs_renewal_at(
                TEST_RENDEZVOUS_CLIENT_CERT_PEM.as_bytes(),
                before_window
            ),
            "cert should not need renewal well before the renewal window"
        );
    }

    #[test]
    fn renewal_check_returns_true_at_window_boundary() {
        let not_after =
            rendezvous_client_identity_not_after_unix(TEST_RENDEZVOUS_CLIENT_CERT_PEM.as_bytes())
                .expect("test certificate should parse");
        // Exactly at the window boundary: now + RENEWAL_WINDOW_SECS == not_after
        let at_boundary = not_after.saturating_sub(RENDEZVOUS_IDENTITY_RENEWAL_WINDOW_SECS);
        assert!(
            rendezvous_client_identity_needs_renewal_at(
                TEST_RENDEZVOUS_CLIENT_CERT_PEM.as_bytes(),
                at_boundary
            ),
            "cert should need renewal exactly at the window boundary"
        );
    }

    #[test]
    fn renewal_check_returns_true_when_cert_is_expired() {
        let not_after =
            rendezvous_client_identity_not_after_unix(TEST_RENDEZVOUS_CLIENT_CERT_PEM.as_bytes())
                .expect("test certificate should parse");
        // Past expiry — cert is expired, still needs renewal
        let past_expiry = not_after.saturating_add(1);
        assert!(
            rendezvous_client_identity_needs_renewal_at(
                TEST_RENDEZVOUS_CLIENT_CERT_PEM.as_bytes(),
                past_expiry
            ),
            "expired cert should still be flagged as needing renewal"
        );
    }

    #[test]
    fn renewal_check_returns_false_on_invalid_pem() {
        assert!(
            !rendezvous_client_identity_needs_renewal_at(b"not-a-cert", 0),
            "invalid PEM should not trigger renewal (returns false rather than erroring)"
        );
    }

    #[test]
    fn expiry_check_returns_false_when_cert_is_valid() {
        let not_after =
            rendezvous_client_identity_not_after_unix(TEST_RENDEZVOUS_CLIENT_CERT_PEM.as_bytes())
                .expect("test certificate should parse");
        let before_expiry = not_after.saturating_sub(1);
        assert!(
            !rendezvous_client_identity_is_expired_at(
                TEST_RENDEZVOUS_CLIENT_CERT_PEM.as_bytes(),
                before_expiry
            ),
            "cert should not be expired before its not_after"
        );
    }

    #[test]
    fn expiry_check_returns_true_at_expiry_boundary() {
        let not_after =
            rendezvous_client_identity_not_after_unix(TEST_RENDEZVOUS_CLIENT_CERT_PEM.as_bytes())
                .expect("test certificate should parse");
        assert!(
            rendezvous_client_identity_is_expired_at(
                TEST_RENDEZVOUS_CLIENT_CERT_PEM.as_bytes(),
                not_after
            ),
            "cert should be considered expired exactly at not_after"
        );
    }

    #[test]
    fn expiry_check_returns_false_on_invalid_pem() {
        assert!(
            !rendezvous_client_identity_is_expired_at(b"not-a-cert", 0),
            "invalid PEM should not report as expired (returns false rather than erroring)"
        );
    }

    #[test]
    fn client_identity_needs_renewal_returns_false_with_no_identity() {
        let cluster_id = Uuid::now_v7();
        let client = RendezvousControlClient::new(
            RendezvousClientConfig {
                cluster_id,
                rendezvous_urls: vec!["https://rendezvous.example".to_string()],
                heartbeat_interval_secs: 15,
            },
            None,
            None,
        )
        .expect("rendezvous client should build");

        assert!(
            !client.client_identity_needs_renewal(),
            "client without a rendezvous identity should not report renewal needed"
        );
    }

    #[test]
    fn idle_relay_tunnel_accept_timeout_does_not_mark_endpoint_disconnected() {
        let cluster_id = Uuid::now_v7();
        let url = "https://rendezvous.example:9443";
        let client = RendezvousControlClient::new(
            RendezvousClientConfig {
                cluster_id,
                rendezvous_urls: vec![url.to_string()],
                heartbeat_interval_secs: 15,
            },
            None,
            None,
        )
        .expect("rendezvous client should build");

        client.record_endpoint_result(url, Ok(()), true);
        client.record_endpoint_result(
            url,
            Err(format!(
                "failed accepting relay tunnel target at {url}: relay tunnel establishment failed: timed out waiting for relay tunnel source"
            )),
            true,
        );

        let runtime_state = client.runtime_state();
        assert_eq!(
            runtime_state.endpoint_statuses[0].status,
            RendezvousEndpointConnectionState::Connected
        );
        assert_eq!(runtime_state.endpoint_statuses[0].consecutive_failures, 0);
        assert_eq!(runtime_state.endpoint_statuses[0].last_error, None);
        assert_eq!(runtime_state.active_url.as_deref(), Some(url));
    }
}
