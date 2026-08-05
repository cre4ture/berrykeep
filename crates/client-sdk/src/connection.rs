use anyhow::{Context, Result, anyhow, bail};
use common::{ClusterId, NodeId};
use futures_util::stream::{FuturesUnordered, StreamExt};
use reqwest::Certificate;
use reqwest::Client;
use reqwest::ClientBuilder;
use reqwest::Url;
use reqwest::blocking::Client as BlockingClient;
use reqwest::blocking::ClientBuilder as BlockingClientBuilder;
use serde::Serialize;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::path::Path;
use std::time::Duration;
use std::time::Instant;
use transport_sdk::{
    CandidateKind, ClientIdentityMaterial, ConnectionCandidate, ExpectedNodeServerIdentity,
    RelayTunnelSourceSecurityConfig, RelayTunnelTlsIdentity, RendezvousClientConfig,
    RendezvousControlClient, TransportPathKind,
};

use crate::ironmesh_client::blocking_runtime;
use crate::latency_probe::LatencyProbeConfig;
use crate::{IronMeshClient, PlannedConnectionBootstrapTarget};

const RELAY_REQUEST_BASE_URL: &str = "https://relay.invalid/";
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const STARTUP_PROBE_RESPONSE_BYTES: usize = 64;
const STARTUP_PROBE_WARMUP_COUNT: usize = 1;
const STARTUP_PROBE_SAMPLE_COUNT: usize = 3;
const STARTUP_PROBE_FAILURE_PENALTY_MS: f64 = 500.0;
const STARTUP_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const STARTUP_PROBE_SUCCESS_GRACE: Duration = Duration::from_millis(250);

type StartupProbeResult = (usize, Result<f64>);
type StartupProbeWorkerResult = (Vec<IronMeshClient>, Vec<StartupProbeResult>);

#[derive(Serialize)]
#[serde(tag = "transport", rename_all = "snake_case")]
enum PlannedRouteKeyMaterial {
    DirectHttps {
        cluster_id: ClusterId,
        target_node_id: Option<NodeId>,
        server_base_url: String,
        server_ca_fingerprint: Option<String>,
        client_identity_fingerprint: Option<String>,
    },
    DirectQuic {
        cluster_id: ClusterId,
        target_node_id: Option<NodeId>,
        candidate: DirectQuicRouteKeyMaterial,
        rendezvous: Option<RendezvousRouteKeyMaterial>,
        relay_ca_fingerprint: Option<String>,
        client_identity_fingerprint: Option<String>,
    },
    RelayTunnel {
        cluster_id: ClusterId,
        target_node_id: Option<NodeId>,
        rendezvous: RendezvousRouteKeyMaterial,
        cluster_ca_fingerprint: Option<String>,
        client_identity_fingerprint: Option<String>,
    },
}

#[derive(Serialize)]
struct DirectQuicRouteKeyMaterial {
    kind: String,
    endpoint: String,
    transport_id: String,
    relay_url: Option<String>,
    relay_auth_fingerprint: Option<String>,
    alpn: String,
    direct_socket_addrs: Vec<String>,
    observed_socket_addrs: Vec<String>,
}

#[derive(Serialize)]
struct RendezvousRouteKeyMaterial {
    urls: Vec<String>,
    ca_fingerprint: Option<String>,
}

#[derive(Serialize)]
struct ClientIdentityRouteKeyMaterial<'a> {
    cluster_id: ClusterId,
    device_id: common::DeviceId,
    private_key_pem: &'a str,
    public_key_pem: &'a str,
    credential_pem: Option<&'a str>,
    rendezvous_client_identity_pem: Option<&'a str>,
}

pub fn load_root_certificate(path: &Path) -> Result<Certificate> {
    let pem = fs::read(path)
        .with_context(|| format!("failed to read server CA certificate {}", path.display()))?;
    Certificate::from_pem(&pem)
        .with_context(|| format!("failed to parse server CA certificate {}", path.display()))
}

pub fn load_root_certificate_pem(pem: &str) -> Result<Certificate> {
    Certificate::from_pem(pem.as_bytes()).context("failed to parse inline server CA certificate")
}

pub fn build_reqwest_client_from_pem(server_ca_pem: Option<&str>) -> Result<Client> {
    let builder = configure_reqwest_client_builder(Client::builder());
    let builder = if let Some(pem) = server_ca_pem {
        builder.add_root_certificate(load_root_certificate_pem(pem)?)
    } else {
        builder
    };
    builder.build().context("failed building HTTP client")
}

pub fn build_reqwest_client_from_pem_for_url(
    server_ca_pem: Option<&str>,
    url: &Url,
) -> Result<Client> {
    build_reqwest_client_from_pem_for_url_with_expected_server_identity(server_ca_pem, url, None)
}

fn build_reqwest_client_from_pem_for_url_with_expected_server_identity(
    server_ca_pem: Option<&str>,
    url: &Url,
    expected_server_identity: Option<ExpectedNodeServerIdentity>,
) -> Result<Client> {
    let builder = configure_reqwest_client_builder(Client::builder());
    let builder = if url.scheme() == "https"
        && let Some(expected_server_identity) = expected_server_identity
    {
        builder.use_preconfigured_tls(transport_sdk::build_tls_client_config(
            server_ca_pem,
            None,
            Some(expected_server_identity),
        )?)
    } else if let Some(pem) = server_ca_pem {
        builder.add_root_certificate(load_root_certificate_pem(pem)?)
    } else {
        builder
    };
    let builder = apply_best_effort_url_resolution(builder, url);
    builder.build().context("failed building HTTP client")
}

pub fn build_blocking_reqwest_client_from_pem(
    server_ca_pem: Option<&str>,
) -> Result<BlockingClient> {
    let builder = configure_blocking_reqwest_client_builder(BlockingClient::builder());
    let builder = if let Some(pem) = server_ca_pem {
        builder.add_root_certificate(load_root_certificate_pem(pem)?)
    } else {
        builder
    };
    builder
        .build()
        .context("failed building blocking HTTP client")
}

pub fn build_blocking_reqwest_client_from_pem_for_url(
    server_ca_pem: Option<&str>,
    url: &Url,
) -> Result<BlockingClient> {
    build_blocking_reqwest_client_from_pem_for_url_with_expected_server_identity(
        server_ca_pem,
        url,
        None,
    )
}

pub(crate) fn build_blocking_reqwest_client_from_pem_for_url_with_expected_server_identity(
    server_ca_pem: Option<&str>,
    url: &Url,
    expected_server_identity: Option<ExpectedNodeServerIdentity>,
) -> Result<BlockingClient> {
    let builder = configure_blocking_reqwest_client_builder(BlockingClient::builder());
    let builder = if url.scheme() == "https"
        && let Some(expected_server_identity) = expected_server_identity
    {
        builder.use_preconfigured_tls(transport_sdk::build_tls_client_config(
            server_ca_pem,
            None,
            Some(expected_server_identity),
        )?)
    } else if let Some(pem) = server_ca_pem {
        builder.add_root_certificate(load_root_certificate_pem(pem)?)
    } else {
        builder
    };
    let builder = apply_best_effort_url_resolution_blocking(builder, url);
    builder
        .build()
        .context("failed building blocking HTTP client")
}

pub fn build_http_client_from_pem(
    server_ca_pem: Option<&str>,
    base_url_str: &str,
) -> Result<IronMeshClient> {
    build_http_client_from_pem_with_target_node_id(server_ca_pem, base_url_str, None, None)
}

fn build_http_client_from_pem_with_target_node_id(
    server_ca_pem: Option<&str>,
    base_url_str: &str,
    target_node_id: Option<NodeId>,
    target_cluster_id: Option<ClusterId>,
) -> Result<IronMeshClient> {
    let base_url = Url::parse(base_url_str)
        .with_context(|| format!("failed to parse server base URL from {}", base_url_str))?;
    let expected_server_identity = expected_server_identity(target_node_id, target_cluster_id)?;
    let http = build_reqwest_client_from_pem_for_url_with_expected_server_identity(
        server_ca_pem,
        &base_url,
        expected_server_identity,
    )?;
    Ok(
        IronMeshClient::from_direct_http_client_with_target_node_id_and_ca_pem(
            base_url.as_str(),
            http,
            target_node_id,
            expected_server_identity,
            server_ca_pem.map(ToString::to_string),
        ),
    )
}

pub fn build_http_client_with_identity_from_pem(
    server_ca_pem: Option<&str>,
    base_url_str: &str,
    identity: &ClientIdentityMaterial,
) -> Result<IronMeshClient> {
    build_http_client_with_identity_from_pem_with_target_node_id(
        server_ca_pem,
        base_url_str,
        identity,
        None,
        None,
    )
}

fn build_http_client_with_identity_from_pem_with_target_node_id(
    server_ca_pem: Option<&str>,
    base_url_str: &str,
    identity: &ClientIdentityMaterial,
    target_node_id: Option<NodeId>,
    target_cluster_id: Option<ClusterId>,
) -> Result<IronMeshClient> {
    let base_url = Url::parse(base_url_str)
        .with_context(|| format!("failed to parse server base URL from {}", base_url_str))?;
    let expected_server_identity = expected_server_identity(target_node_id, target_cluster_id)?;
    let http = build_reqwest_client_from_pem_for_url_with_expected_server_identity(
        server_ca_pem,
        &base_url,
        expected_server_identity,
    )?;
    Ok(
        IronMeshClient::from_direct_http_client_with_target_node_id_and_ca_pem(
            base_url.as_str(),
            http,
            target_node_id,
            expected_server_identity,
            server_ca_pem.map(ToString::to_string),
        )
        .with_client_identity(identity.clone()),
    )
}

pub fn build_http_client_with_identity_from_planned_target(
    target: &PlannedConnectionBootstrapTarget,
    identity: &ClientIdentityMaterial,
) -> Result<IronMeshClient> {
    let client = build_http_client_with_identity_from_planned_target_unkeyed(target, identity)?;
    client.set_single_transport_route_key(planned_route_key(target, Some(identity))?)?;
    Ok(client)
}

fn build_http_client_with_identity_from_planned_target_unkeyed(
    target: &PlannedConnectionBootstrapTarget,
    identity: &ClientIdentityMaterial,
) -> Result<IronMeshClient> {
    match target.path_kind {
        TransportPathKind::DirectHttps => {
            let server_base_url =
                planned_target_direct_http_base_url(target)?.ok_or_else(|| {
                    anyhow!("direct HTTPS client transport target is missing server_base_url")
                })?;
            return build_http_client_with_identity_from_pem_with_target_node_id(
                target
                    .server_ca_pem
                    .as_deref()
                    .or(target.cluster_ca_pem.as_deref()),
                server_base_url,
                identity,
                target.target_node_id,
                Some(target.cluster_id),
            );
        }
        TransportPathKind::DirectQuic => {
            let candidate = planned_target_direct_quic_candidate(target)?;
            let rendezvous = if candidate_uses_rendezvous_relay(candidate, &target.rendezvous_urls)
            {
                Some(build_rendezvous_control_client_for_target(
                    target, identity,
                )?)
            } else {
                None
            };
            return Ok(IronMeshClient::from_direct_quic_candidate_with_rendezvous(
                candidate.clone(),
                target.target_node_id,
                rendezvous,
                target
                    .rendezvous_ca_pem
                    .clone()
                    .or_else(|| target.cluster_ca_pem.clone()),
            )
            .with_client_identity(identity.clone()));
        }
        TransportPathKind::RelayTunnel => {}
    }

    let target_node_id = target
        .target_node_id
        .ok_or_else(|| anyhow!("relay-backed client transport target is missing target_node_id"))?;
    let cluster_ca_pem = target
        .cluster_ca_pem
        .as_deref()
        .filter(|pem| !pem.trim().is_empty())
        .ok_or_else(|| {
            anyhow!(
                "relay-backed client transport requires target.cluster_ca_pem for inner relay mTLS"
            )
        })?;
    let rendezvous_client_identity_pem = identity
        .rendezvous_client_identity_pem
        .as_deref()
        .filter(|pem| !pem.trim().is_empty())
        .ok_or_else(|| {
            anyhow!(
                "relay-backed client transport requires rendezvous_client_identity_pem for inner relay mTLS"
            )
        })?;
    let relay_security = RelayTunnelSourceSecurityConfig {
        cluster_id: target.cluster_id,
        expected_target_node_id: target_node_id,
        cluster_ca_pem: cluster_ca_pem.as_bytes().to_vec(),
        identity: RelayTunnelTlsIdentity::from_combined_pem(rendezvous_client_identity_pem),
    };
    let rendezvous = build_rendezvous_control_client_for_target(target, identity)?;

    Ok(IronMeshClient::with_relay_transport(
        RELAY_REQUEST_BASE_URL,
        rendezvous,
        target_node_id,
        relay_security,
    )
    .with_client_identity(identity.clone()))
}

fn candidate_uses_rendezvous_relay(
    candidate: &ConnectionCandidate,
    rendezvous_urls: &[String],
) -> bool {
    let Some(relay_url) = candidate
        .transport_hints
        .as_ref()
        .and_then(|hints| hints.relay_url.as_deref())
        .and_then(|url| Url::parse(url).ok())
    else {
        return false;
    };
    rendezvous_urls.iter().any(|url| {
        Url::parse(url).ok().is_some_and(|rendezvous_url| {
            relay_url.scheme() == rendezvous_url.scheme()
                && relay_url.host_str() == rendezvous_url.host_str()
                && relay_url.port_or_known_default() == rendezvous_url.port_or_known_default()
        })
    })
}

fn build_rendezvous_control_client_for_target(
    target: &PlannedConnectionBootstrapTarget,
    identity: &ClientIdentityMaterial,
) -> Result<RendezvousControlClient> {
    let rendezvous_client_identity_pem = identity
        .rendezvous_client_identity_pem
        .as_deref()
        .filter(|pem| !pem.trim().is_empty())
        .ok_or_else(|| {
            anyhow!("rendezvous-backed transport requires rendezvous_client_identity_pem")
        })?;
    transport_sdk::RendezvousControlClient::new(
        RendezvousClientConfig {
            cluster_id: target.cluster_id,
            rendezvous_urls: target.rendezvous_urls.clone(),
            heartbeat_interval_secs: 15,
        },
        target
            .rendezvous_ca_pem
            .as_deref()
            .or(target.cluster_ca_pem.as_deref()),
        Some(rendezvous_client_identity_pem.as_bytes()),
    )
}

fn planned_route_key(
    target: &PlannedConnectionBootstrapTarget,
    identity: Option<&ClientIdentityMaterial>,
) -> Result<String> {
    let client_identity_fingerprint = identity
        .map(client_identity_route_fingerprint)
        .transpose()?;
    let effective_rendezvous_ca = target
        .rendezvous_ca_pem
        .as_deref()
        .or(target.cluster_ca_pem.as_deref());
    let rendezvous = || RendezvousRouteKeyMaterial {
        urls: target
            .rendezvous_urls
            .iter()
            .map(|url| normalize_route_url(url))
            .collect(),
        ca_fingerprint: optional_text_fingerprint(effective_rendezvous_ca),
    };

    let material = match target.path_kind {
        TransportPathKind::DirectHttps => PlannedRouteKeyMaterial::DirectHttps {
            cluster_id: target.cluster_id,
            target_node_id: target.target_node_id,
            server_base_url: normalize_route_url(
                planned_target_direct_http_base_url(target)?.ok_or_else(|| {
                    anyhow!("direct HTTPS client transport target is missing server_base_url")
                })?,
            ),
            server_ca_fingerprint: optional_text_fingerprint(
                target
                    .server_ca_pem
                    .as_deref()
                    .or(target.cluster_ca_pem.as_deref()),
            ),
            client_identity_fingerprint,
        },
        TransportPathKind::DirectQuic => {
            let candidate = planned_target_direct_quic_candidate(target)?;
            let hints = candidate.transport_hints.as_ref();
            PlannedRouteKeyMaterial::DirectQuic {
                cluster_id: target.cluster_id,
                target_node_id: target.target_node_id,
                candidate: DirectQuicRouteKeyMaterial {
                    kind: candidate_kind_label(candidate.kind).to_string(),
                    endpoint: normalize_route_url(&candidate.endpoint),
                    transport_id: hints
                        .and_then(|hints| hints.transport_id.as_deref())
                        .unwrap_or_default()
                        .trim()
                        .to_string(),
                    relay_url: hints
                        .and_then(|hints| hints.relay_url.as_deref())
                        .map(normalize_route_url),
                    relay_auth_fingerprint: hints
                        .and_then(|hints| hints.relay_auth_token.as_deref())
                        .and_then(nonempty_text_fingerprint),
                    alpn: hints
                        .and_then(|hints| hints.alpn.as_deref())
                        .unwrap_or(transport_sdk::DEFAULT_DIRECT_QUIC_ALPN)
                        .trim()
                        .to_string(),
                    direct_socket_addrs: hints
                        .map(|hints| normalize_socket_addr_list(&hints.direct_socket_addrs))
                        .unwrap_or_default(),
                    observed_socket_addrs: hints
                        .map(|hints| normalize_socket_addr_list(&hints.observed_socket_addrs))
                        .unwrap_or_default(),
                },
                rendezvous: candidate_uses_rendezvous_relay(candidate, &target.rendezvous_urls)
                    .then(rendezvous),
                relay_ca_fingerprint: optional_text_fingerprint(effective_rendezvous_ca),
                client_identity_fingerprint,
            }
        }
        TransportPathKind::RelayTunnel => PlannedRouteKeyMaterial::RelayTunnel {
            cluster_id: target.cluster_id,
            target_node_id: target.target_node_id,
            rendezvous: rendezvous(),
            cluster_ca_fingerprint: optional_text_fingerprint(target.cluster_ca_pem.as_deref()),
            client_identity_fingerprint,
        },
    };

    let encoded = serde_json::to_vec(&material).context("failed encoding planned route key")?;
    Ok(format!(
        "planned-route-v1:{}",
        blake3::hash(&encoded).to_hex()
    ))
}

pub(crate) fn planned_transport_route_key(
    target: &PlannedConnectionBootstrapTarget,
) -> Result<String> {
    planned_route_key(target, None)
}

fn client_identity_route_fingerprint(identity: &ClientIdentityMaterial) -> Result<String> {
    let material = ClientIdentityRouteKeyMaterial {
        cluster_id: identity.cluster_id,
        device_id: identity.device_id,
        private_key_pem: identity.private_key_pem.trim(),
        public_key_pem: identity.public_key_pem.trim(),
        credential_pem: identity.credential_pem.as_deref().map(str::trim),
        rendezvous_client_identity_pem: identity
            .rendezvous_client_identity_pem
            .as_deref()
            .map(str::trim),
    };
    let encoded = serde_json::to_vec(&material).context("failed encoding client route identity")?;
    Ok(blake3::hash(&encoded).to_hex().to_string())
}

fn optional_text_fingerprint(value: Option<&str>) -> Option<String> {
    value.and_then(nonempty_text_fingerprint)
}

fn nonempty_text_fingerprint(value: &str) -> Option<String> {
    let normalized = value.replace("\r\n", "\n");
    let normalized = normalized.trim();
    (!normalized.is_empty()).then(|| blake3::hash(normalized.as_bytes()).to_hex().to_string())
}

fn normalize_route_url(value: &str) -> String {
    let value = value.trim();
    Url::parse(value)
        .map(|url| url.to_string().trim_end_matches('/').to_string())
        .unwrap_or_else(|_| value.trim_end_matches('/').to_string())
}

fn normalize_socket_addr_list(values: &[String]) -> Vec<String> {
    values
        .iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

const fn candidate_kind_label(kind: CandidateKind) -> &'static str {
    match kind {
        CandidateKind::DirectHttps => "direct_https",
        CandidateKind::DirectQuic => "direct_quic",
        CandidateKind::ServerReflexive => "server_reflexive",
        CandidateKind::Relay => "relay",
    }
}

pub fn build_http_client_with_identity_from_planned_targets(
    targets: &[PlannedConnectionBootstrapTarget],
    identity: &ClientIdentityMaterial,
) -> Result<IronMeshClient> {
    let clients = collect_signed_planned_target_clients(targets, identity)?;
    if clients.len() == 1 {
        return IronMeshClient::combine(clients);
    }

    let ordered = order_clients_by_startup_probe(clients, true)?;
    IronMeshClient::combine(ordered)
}

fn collect_signed_planned_target_clients(
    targets: &[PlannedConnectionBootstrapTarget],
    identity: &ClientIdentityMaterial,
) -> Result<Vec<IronMeshClient>> {
    let mut clients = Vec::new();
    let mut build_errors = Vec::new();

    for target in targets {
        match build_http_client_with_identity_from_planned_target(target, identity) {
            Ok(client) => clients.push(client),
            Err(error) if target.relay_mode != transport_sdk::RelayMode::Required => {
                build_errors.push(error.to_string());
            }
            Err(error) => return Err(error),
        }
    }

    if clients.is_empty() {
        bail!(
            "bootstrap does not contain any buildable client transport targets{}",
            format_build_error_suffix(&build_errors)
        );
    }
    Ok(clients)
}

pub fn build_http_client_from_planned_targets(
    targets: &[PlannedConnectionBootstrapTarget],
) -> Result<IronMeshClient> {
    let clients = collect_anonymous_planned_target_clients(targets)?;
    if clients.len() == 1 {
        return IronMeshClient::combine(clients);
    }

    let ordered = order_clients_by_startup_probe(clients, false)?;
    IronMeshClient::combine(ordered)
}

fn collect_anonymous_planned_target_clients(
    targets: &[PlannedConnectionBootstrapTarget],
) -> Result<Vec<IronMeshClient>> {
    let mut clients = Vec::new();
    let mut build_errors = Vec::new();

    for target in targets {
        match build_client_with_optional_identity_from_planned_target(target, None) {
            Ok(client) => clients.push(client),
            Err(error) => build_errors.push(error.to_string()),
        }
    }

    if clients.is_empty() {
        bail!(
            "bootstrap does not contain any buildable direct client transport targets{}",
            format_build_error_suffix(&build_errors)
        );
    }
    Ok(clients)
}

pub fn build_client_with_optional_identity_from_planned_target(
    target: &PlannedConnectionBootstrapTarget,
    identity: Option<&ClientIdentityMaterial>,
) -> Result<IronMeshClient> {
    match identity {
        Some(identity) => build_http_client_with_identity_from_planned_target(target, identity),
        None => {
            if target.path_kind == TransportPathKind::DirectQuic {
                bail!(
                    "direct QUIC client transport target {} requires enrolled client identity material",
                    planned_target_label(target)
                );
            }
            if let Some(server_base_url) = planned_target_direct_http_base_url(target)? {
                let client = build_http_client_from_pem_with_target_node_id(
                    target
                        .server_ca_pem
                        .as_deref()
                        .or(target.cluster_ca_pem.as_deref()),
                    server_base_url,
                    target.target_node_id,
                    Some(target.cluster_id),
                )?;
                client.set_single_transport_route_key(planned_route_key(target, None)?)?;
                return Ok(client);
            }

            bail!("relay-backed client transport requires enrolled client identity material");
        }
    }
}

/// Builds all supported client transport routes for the supplied identity.
///
/// Unenrolled clients deliberately retain direct HTTPS-only behavior: relay and
/// Direct QUIC routes require a client identity for their authenticated inner
/// transport handshake. Keeping that policy here lets managed route refresh
/// callers share one route-construction path without duplicating platform
/// checks.
pub fn build_client_with_optional_identity_from_planned_targets(
    targets: &[PlannedConnectionBootstrapTarget],
    identity: Option<&ClientIdentityMaterial>,
) -> Result<IronMeshClient> {
    match identity {
        Some(identity) => build_http_client_with_identity_from_planned_targets(targets, identity),
        None => build_http_client_from_planned_targets(targets),
    }
}

/// Constructs the complete desired route set without performing startup probes.
/// Managed refresh callers reconcile these inert candidates with the live router
/// first, then let the router claim only routes whose retained health state says
/// they are due.
pub(crate) fn build_unprobed_client_with_optional_identity_from_planned_targets(
    targets: &[PlannedConnectionBootstrapTarget],
    identity: Option<&ClientIdentityMaterial>,
) -> Result<IronMeshClient> {
    let clients = match identity {
        Some(identity) => collect_signed_planned_target_clients(targets, identity)?,
        None => collect_anonymous_planned_target_clients(targets)?,
    };
    IronMeshClient::combine(clients)
}

pub fn build_http_client(
    server_ca_cert: Option<&Path>,
    base_url_str: &str,
) -> Result<IronMeshClient> {
    let server_ca_pem = server_ca_cert
        .map(|path| {
            fs::read_to_string(path)
                .with_context(|| format!("failed to read server CA certificate {}", path.display()))
        })
        .transpose()?;
    build_http_client_from_pem(server_ca_pem.as_deref(), base_url_str)
}

pub fn build_http_client_with_identity(
    server_ca_cert: Option<&Path>,
    base_url_str: &str,
    identity: &ClientIdentityMaterial,
) -> Result<IronMeshClient> {
    let server_ca_pem = server_ca_cert
        .map(|path| {
            fs::read_to_string(path)
                .with_context(|| format!("failed to read server CA certificate {}", path.display()))
        })
        .transpose()?;
    build_http_client_with_identity_from_pem(server_ca_pem.as_deref(), base_url_str, identity)
}

pub fn build_blocking_http_client(server_ca_cert: Option<&Path>) -> Result<BlockingClient> {
    let server_ca_pem = server_ca_cert
        .map(|path| {
            fs::read_to_string(path)
                .with_context(|| format!("failed to read server CA certificate {}", path.display()))
        })
        .transpose()?;
    build_blocking_reqwest_client_from_pem(server_ca_pem.as_deref())
}

fn expected_server_identity(
    target_node_id: Option<NodeId>,
    target_cluster_id: Option<ClusterId>,
) -> Result<Option<ExpectedNodeServerIdentity>> {
    match (target_node_id, target_cluster_id) {
        (Some(node_id), Some(cluster_id)) => Ok(Some(ExpectedNodeServerIdentity {
            node_id,
            cluster_id,
        })),
        // Older bootstrap payloads can describe direct endpoints without a
        // node ID. Keep those usable with the CA-only verifier; identity
        // binding is enabled as soon as the node ID is present.
        (None, _) => Ok(None),
        (Some(_), None) => {
            bail!("identity-bound direct HTTPS target is missing its expected cluster_id")
        }
    }
}

fn configure_reqwest_client_builder(builder: ClientBuilder) -> ClientBuilder {
    // Keep connection attempts short so clients can fail over quickly when a
    // hostname publishes an unreachable IPv6 address ahead of a working IPv4 one.
    builder.connect_timeout(HTTP_CONNECT_TIMEOUT)
}

fn configure_blocking_reqwest_client_builder(
    builder: BlockingClientBuilder,
) -> BlockingClientBuilder {
    builder.connect_timeout(HTTP_CONNECT_TIMEOUT)
}

fn apply_best_effort_url_resolution(builder: ClientBuilder, url: &Url) -> ClientBuilder {
    if let Some((host, addrs)) = preferred_socket_addrs_for_url(url) {
        builder.resolve_to_addrs(&host, &addrs)
    } else {
        builder
    }
}

fn apply_best_effort_url_resolution_blocking(
    builder: BlockingClientBuilder,
    url: &Url,
) -> BlockingClientBuilder {
    if let Some((host, addrs)) = preferred_socket_addrs_for_url(url) {
        builder.resolve_to_addrs(&host, &addrs)
    } else {
        builder
    }
}

fn order_clients_by_startup_probe(
    clients: Vec<IronMeshClient>,
    signed_probe: bool,
) -> Result<Vec<IronMeshClient>> {
    let worker = std::thread::Builder::new()
        .name("ironmesh-client-startup-probe".to_string())
        .spawn(move || -> Result<StartupProbeWorkerResult> {
            // Direct QUIC endpoints and multiplex sessions spawn transport
            // drivers on the runtime that performs their startup probe. Keep
            // that runtime alive for the process lifetime so the successfully
            // probed client remains usable after this worker returns.
            let runtime = blocking_runtime()?;

            Ok(runtime.block_on(async move {
                let mut probes = FuturesUnordered::new();
                for (index, client) in clients.iter().cloned().enumerate() {
                    probes.push(async move {
                        let probe = if signed_probe {
                            probe_signed_client_startup_quality(&client).await
                        } else {
                            probe_direct_client_startup_quality(&client).await
                        };
                        (index, probe)
                    });
                }

                let mut results = Vec::new();
                let mut success_grace_deadline = None;
                loop {
                    let next = if let Some(deadline) = success_grace_deadline {
                        tokio::select! {
                            result = probes.next() => result,
                            () = tokio::time::sleep_until(deadline) => None,
                        }
                    } else {
                        probes.next().await
                    };
                    let Some((index, probe)) = next else {
                        break;
                    };
                    if probe.is_ok() && success_grace_deadline.is_none() {
                        success_grace_deadline =
                            Some(tokio::time::Instant::now() + STARTUP_PROBE_SUCCESS_GRACE);
                    }
                    results.push((index, probe));
                }
                (clients, results)
            }))
        })
        .context("failed to spawn startup probe worker")?;

    let (clients, probed) = worker
        .join()
        .map_err(|_| anyhow!("startup probe worker panicked"))??;

    let mut success = Vec::new();
    let mut failed = Vec::new();
    let mut probe_errors = Vec::new();
    let mut scores = HashMap::new();

    for (index, probe) in probed {
        match probe {
            Ok(score) => {
                scores.insert(index, score);
            }
            Err(error) => {
                probe_errors.push(format!("{error:#}"));
            }
        }
    }

    if scores.is_empty() {
        bail!(
            "startup connection quality probe failed for all client transport targets{}",
            format_build_error_suffix(&probe_errors)
        );
    }

    for (index, client) in clients.into_iter().enumerate() {
        if let Some(score) = scores.remove(&index) {
            success.push((score, index, client));
        } else {
            failed.push((index, client));
        }
    }

    success.sort_by(|left, right| {
        left.0
            .partial_cmp(&right.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.1.cmp(&right.1))
    });
    failed.sort_by_key(|(index, _)| *index);

    let mut ordered = success
        .into_iter()
        .map(|(_, _, client)| client)
        .collect::<Vec<_>>();
    ordered.extend(failed.into_iter().map(|(_, client)| client));
    Ok(ordered)
}

async fn probe_signed_client_startup_quality(client: &IronMeshClient) -> Result<f64> {
    let target_label = startup_probe_target_label(client);
    if let Some(rendezvous) = client.rendezvous_client()
        && let Some(diagnostic) = rendezvous.client_identity_expiry_diagnostic()
    {
        bail!(diagnostic);
    }

    let probe_config = LatencyProbeConfig {
        sample_count: STARTUP_PROBE_SAMPLE_COUNT,
        warmup_count: STARTUP_PROBE_WARMUP_COUNT,
        response_bytes: STARTUP_PROBE_RESPONSE_BYTES,
        server_delay_ms: 0,
        pause_between_samples_ms: 0,
    };
    let result = tokio::time::timeout(STARTUP_PROBE_TIMEOUT, async {
        let mut latest_result = None;
        for _ in 0..2 {
            let result = client.run_latency_probe(probe_config.clone()).await?;
            let reached_target = result.summary.success_count > 0;
            latest_result = Some(result);
            if reached_target {
                break;
            }
        }
        latest_result.ok_or_else(|| anyhow!("startup signed latency probe did not run"))
    })
    .await
    .with_context(|| {
        format!(
            "startup signed latency probe timed out after {:?} for {target_label}",
            STARTUP_PROBE_TIMEOUT
        )
    })??;

    if result.summary.success_count == 0 {
        let detail = result
            .samples
            .iter()
            .find_map(|sample| sample.error.as_deref())
            .unwrap_or("all latency probe samples failed");
        bail!("startup signed latency probe failed for {target_label}: {detail}");
    }

    let latency_ms = result
        .summary
        .avg_total_duration_ms
        .or(result.cold_connect_duration_ms)
        .unwrap_or_default();
    Ok(latency_ms + result.summary.failure_count as f64 * STARTUP_PROBE_FAILURE_PENALTY_MS)
}

async fn probe_direct_client_startup_quality(client: &IronMeshClient) -> Result<f64> {
    let target_label = startup_probe_target_label(client);
    tokio::time::timeout(STARTUP_PROBE_TIMEOUT, async {
        let total_probe_count = STARTUP_PROBE_WARMUP_COUNT + STARTUP_PROBE_SAMPLE_COUNT;
        let mut latency_samples_ms = Vec::with_capacity(STARTUP_PROBE_SAMPLE_COUNT);
        for probe_index in 0..total_probe_count {
            let started_at = Instant::now();
            let response = client.get_relative_path("/health").await?;
            if !response.status.is_success() {
                bail!(
                    "health probe returned {} for {}",
                    response.status,
                    target_label
                );
            }
            if probe_index >= STARTUP_PROBE_WARMUP_COUNT {
                latency_samples_ms.push(started_at.elapsed().as_secs_f64() * 1000.0);
            }
        }
        Ok(latency_samples_ms.iter().sum::<f64>() / latency_samples_ms.len() as f64)
    })
    .await
    .with_context(|| {
        format!(
            "startup direct health probe timed out after {:?} for {target_label}",
            STARTUP_PROBE_TIMEOUT
        )
    })?
}

fn startup_probe_target_label(client: &IronMeshClient) -> String {
    client
        .connection_diagnostics()
        .endpoints
        .first()
        .map(|endpoint| endpoint.locator.clone())
        .unwrap_or_else(|| "<unknown-target>".to_string())
}

fn format_build_error_suffix(errors: &[String]) -> String {
    if errors.is_empty() {
        String::new()
    } else {
        format!(": {}", errors.join(" | "))
    }
}

fn planned_target_direct_http_base_url(
    target: &PlannedConnectionBootstrapTarget,
) -> Result<Option<&str>> {
    match target.path_kind {
        TransportPathKind::DirectHttps => {
            target.server_base_url.as_deref().map(Some).ok_or_else(|| {
                anyhow!("direct HTTPS client transport target is missing server_base_url")
            })
        }
        TransportPathKind::DirectQuic => bail!(
            "direct QUIC client transport target {} requires enrolled client identity material",
            planned_target_label(target)
        ),
        TransportPathKind::RelayTunnel => {
            if target.server_base_url.is_some() {
                bail!(
                    "relay-backed client transport target {} unexpectedly includes server_base_url",
                    planned_target_label(target)
                );
            }
            Ok(None)
        }
    }
}

fn planned_target_direct_quic_candidate(
    target: &PlannedConnectionBootstrapTarget,
) -> Result<&ConnectionCandidate> {
    if target.path_kind != TransportPathKind::DirectQuic {
        bail!(
            "direct QUIC candidate requested for non-QUIC target {}",
            planned_target_label(target)
        );
    }
    target.direct_candidate.as_ref().ok_or_else(|| {
        anyhow!(
            "direct QUIC client transport target {} is missing direct_candidate",
            planned_target_label(target)
        )
    })
}

fn planned_target_label(target: &PlannedConnectionBootstrapTarget) -> String {
    target
        .server_base_url
        .clone()
        .or_else(|| {
            target
                .direct_candidate
                .as_ref()
                .map(|candidate| candidate.endpoint.clone())
        })
        .or_else(|| {
            target
                .target_node_id
                .map(|node_id| format!("node {node_id}"))
        })
        .unwrap_or_else(|| format!("{:?}", target.path_kind))
}

fn preferred_socket_addrs_for_url(url: &Url) -> Option<(String, Vec<SocketAddr>)> {
    let host = url.host_str()?.trim();
    if host.is_empty() || host.parse::<IpAddr>().is_ok() {
        return None;
    }
    let port = url.port_or_known_default()?;
    let addrs = (host, port).to_socket_addrs().ok()?;
    let addrs = prioritize_socket_addrs(addrs);
    if addrs.is_empty() {
        None
    } else {
        Some((host.to_string(), addrs))
    }
}

fn prioritize_socket_addrs(addrs: impl IntoIterator<Item = SocketAddr>) -> Vec<SocketAddr> {
    let mut seen = HashSet::new();
    let mut ipv4 = Vec::new();
    let mut ipv6 = Vec::new();

    for addr in addrs {
        if !seen.insert(addr) {
            continue;
        }
        if addr.is_ipv4() {
            ipv4.push(addr);
        } else {
            ipv6.push(addr);
        }
    }

    ipv4.extend(ipv6);
    ipv4
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::NodeId;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::net::{Ipv4Addr, Ipv6Addr};
    use transport_sdk::candidates::ConnectionCandidateTransportHints;
    use transport_sdk::{CandidateKind, DEFAULT_DIRECT_QUIC_ALPN, RelayMode, TransportPathKind};
    use uuid::Uuid;

    fn sample_identity() -> ClientIdentityMaterial {
        let mut identity =
            ClientIdentityMaterial::generate(Uuid::now_v7(), None, Some("test-device".to_string()))
                .expect("identity should generate");
        identity.credential_pem = Some("issued-credential".to_string());
        identity
    }

    fn direct_quic_route_target(
        cluster_id: ClusterId,
        target_node_id: NodeId,
    ) -> PlannedConnectionBootstrapTarget {
        PlannedConnectionBootstrapTarget {
            cluster_id,
            rendezvous_urls: vec!["https://rendezvous.example".to_string()],
            rendezvous_mtls_required: true,
            relay_mode: RelayMode::Fallback,
            path_kind: TransportPathKind::DirectQuic,
            direct_candidate: Some(ConnectionCandidate {
                kind: CandidateKind::DirectQuic,
                endpoint: "iroh://peer-key-1".to_string(),
                rtt_ms: Some(8),
                transport_hints: Some(ConnectionCandidateTransportHints {
                    transport_id: Some("peer-key-1".to_string()),
                    relay_url: Some("https://rendezvous.example".to_string()),
                    relay_auth_token: Some("relay-token".to_string()),
                    alpn: Some(DEFAULT_DIRECT_QUIC_ALPN.to_string()),
                    direct_socket_addrs: vec!["127.0.0.1:7000".to_string()],
                    observed_socket_addrs: vec!["198.51.100.8:7000".to_string()],
                }),
            }),
            server_base_url: None,
            target_node_id: Some(target_node_id),
            server_ca_pem: None,
            cluster_ca_pem: Some("cluster-ca".to_string()),
            rendezvous_ca_pem: Some("rendezvous-ca".to_string()),
            pairing_token: Some("pairing-token".to_string()),
            device_label: Some("device label".to_string()),
            device_id: Some("device-id".to_string()),
        }
    }

    fn assert_route_key_changes(
        baseline: &PlannedConnectionBootstrapTarget,
        changed: PlannedConnectionBootstrapTarget,
        identity: &ClientIdentityMaterial,
        field: &str,
    ) {
        assert_ne!(
            planned_route_key(baseline, Some(identity)).expect("baseline key should build"),
            planned_route_key(&changed, Some(identity)).expect("changed key should build"),
            "changing {field} must replace the effective route"
        );
    }

    #[test]
    fn planned_direct_quic_route_key_covers_every_effective_transport_input() {
        let identity = sample_identity();
        let target_node_id = NodeId::new_v4();
        let baseline = direct_quic_route_target(identity.cluster_id, target_node_id);

        let mut changed = baseline.clone();
        changed.cluster_id = Uuid::now_v7();
        assert_route_key_changes(&baseline, changed, &identity, "cluster_id");

        let mut changed = baseline.clone();
        changed.target_node_id = Some(NodeId::new_v4());
        assert_route_key_changes(&baseline, changed, &identity, "target_node_id");

        let mut changed = baseline.clone();
        changed
            .rendezvous_urls
            .push("https://backup.example".to_string());
        assert_route_key_changes(&baseline, changed, &identity, "rendezvous_urls");

        let mut changed = baseline.clone();
        changed.rendezvous_ca_pem = Some("renewed-rendezvous-ca".to_string());
        assert_route_key_changes(&baseline, changed, &identity, "rendezvous_ca_pem");

        for (field, mutate) in [
            (
                "candidate endpoint",
                (|candidate: &mut ConnectionCandidate| {
                    candidate.endpoint = "iroh://peer-key-2".to_string();
                }) as fn(&mut ConnectionCandidate),
            ),
            ("transport_id", |candidate: &mut ConnectionCandidate| {
                candidate
                    .transport_hints
                    .as_mut()
                    .expect("hints should exist")
                    .transport_id = Some("peer-key-2".to_string());
            }),
            ("relay_url", |candidate: &mut ConnectionCandidate| {
                candidate
                    .transport_hints
                    .as_mut()
                    .expect("hints should exist")
                    .relay_url = Some("https://other-relay.example".to_string());
            }),
            ("relay_auth_token", |candidate: &mut ConnectionCandidate| {
                candidate
                    .transport_hints
                    .as_mut()
                    .expect("hints should exist")
                    .relay_auth_token = Some("renewed-relay-token".to_string());
            }),
            ("alpn", |candidate: &mut ConnectionCandidate| {
                candidate
                    .transport_hints
                    .as_mut()
                    .expect("hints should exist")
                    .alpn = Some("ironmesh/test/2".to_string());
            }),
            (
                "direct_socket_addrs",
                |candidate: &mut ConnectionCandidate| {
                    candidate
                        .transport_hints
                        .as_mut()
                        .expect("hints should exist")
                        .direct_socket_addrs = vec!["127.0.0.1:7001".to_string()];
                },
            ),
            (
                "observed_socket_addrs",
                |candidate: &mut ConnectionCandidate| {
                    candidate
                        .transport_hints
                        .as_mut()
                        .expect("hints should exist")
                        .observed_socket_addrs = vec!["198.51.100.9:7000".to_string()];
                },
            ),
        ] {
            let mut changed = baseline.clone();
            mutate(
                changed
                    .direct_candidate
                    .as_mut()
                    .expect("candidate should exist"),
            );
            assert_route_key_changes(&baseline, changed, &identity, field);
        }

        let mut renewed_identity = identity.clone();
        renewed_identity.credential_pem = Some("renewed-credential".to_string());
        assert_ne!(
            planned_route_key(&baseline, Some(&identity)).expect("old key should build"),
            planned_route_key(&baseline, Some(&renewed_identity))
                .expect("renewed key should build")
        );
    }

    #[test]
    fn planned_route_key_ignores_quality_and_enrollment_metadata() {
        let identity = sample_identity();
        let baseline = direct_quic_route_target(identity.cluster_id, NodeId::new_v4());
        let baseline_key = planned_route_key(&baseline, Some(&identity)).expect("key should build");
        let mut metadata_update = baseline.clone();
        metadata_update
            .direct_candidate
            .as_mut()
            .expect("candidate should exist")
            .rtt_ms = Some(999);
        metadata_update.pairing_token = Some("new-pairing-token".to_string());
        metadata_update.device_label = Some("renamed device".to_string());
        metadata_update.device_id = Some("new-bootstrap-device-id".to_string());

        assert_eq!(
            planned_route_key(&metadata_update, Some(&identity)).expect("key should build"),
            baseline_key
        );
    }

    #[test]
    fn planned_route_key_treats_socket_addresses_as_canonical_sets() {
        let identity = sample_identity();
        let mut baseline = direct_quic_route_target(identity.cluster_id, NodeId::new_v4());
        let hints = baseline
            .direct_candidate
            .as_mut()
            .and_then(|candidate| candidate.transport_hints.as_mut())
            .expect("candidate hints should exist");
        hints.direct_socket_addrs.push("127.0.0.1:7001".to_string());
        hints
            .observed_socket_addrs
            .push("198.51.100.9:7000".to_string());

        let mut reordered = baseline.clone();
        let hints = reordered
            .direct_candidate
            .as_mut()
            .and_then(|candidate| candidate.transport_hints.as_mut())
            .expect("candidate hints should exist");
        hints.direct_socket_addrs = vec![
            " 127.0.0.1:7001 ".to_string(),
            "127.0.0.1:7000".to_string(),
            "127.0.0.1:7001".to_string(),
            "".to_string(),
        ];
        hints.observed_socket_addrs = vec![
            "198.51.100.9:7000".to_string(),
            "198.51.100.8:7000".to_string(),
            " 198.51.100.8:7000 ".to_string(),
        ];

        assert_eq!(
            planned_route_key(&baseline, Some(&identity)).expect("baseline key should build"),
            planned_route_key(&reordered, Some(&identity)).expect("reordered key should build")
        );
    }

    #[test]
    fn planned_http_and_relay_route_keys_cover_their_effective_inputs() {
        let mut identity = sample_identity();
        identity.rendezvous_client_identity_pem = Some("rendezvous-identity".to_string());
        let http = PlannedConnectionBootstrapTarget {
            cluster_id: identity.cluster_id,
            rendezvous_urls: vec!["https://unused-rendezvous.example".to_string()],
            rendezvous_mtls_required: false,
            relay_mode: RelayMode::Fallback,
            path_kind: TransportPathKind::DirectHttps,
            direct_candidate: None,
            server_base_url: Some("https://node.example/".to_string()),
            target_node_id: Some(NodeId::new_v4()),
            server_ca_pem: Some("server-ca".to_string()),
            cluster_ca_pem: Some("unused-cluster-ca".to_string()),
            rendezvous_ca_pem: None,
            pairing_token: None,
            device_label: None,
            device_id: None,
        };
        let http_key = planned_route_key(&http, Some(&identity)).expect("HTTP key should build");
        let mut equivalent_http = http.clone();
        equivalent_http.server_base_url = Some("https://node.example".to_string());
        equivalent_http.rendezvous_urls = vec!["https://changed-but-unused.example".to_string()];
        equivalent_http.cluster_ca_pem = Some("changed-but-shadowed-ca".to_string());
        assert_eq!(
            planned_route_key(&equivalent_http, Some(&identity)).expect("HTTP key should build"),
            http_key
        );
        for (field, changed) in [
            ("HTTP URL", {
                let mut changed = http.clone();
                changed.server_base_url = Some("https://other-node.example".to_string());
                changed
            }),
            ("HTTP node identity", {
                let mut changed = http.clone();
                changed.target_node_id = Some(NodeId::new_v4());
                changed
            }),
            ("HTTP trust root", {
                let mut changed = http.clone();
                changed.server_ca_pem = Some("renewed-server-ca".to_string());
                changed
            }),
        ] {
            assert_route_key_changes(&http, changed, &identity, field);
        }

        let relay = PlannedConnectionBootstrapTarget {
            path_kind: TransportPathKind::RelayTunnel,
            server_base_url: None,
            server_ca_pem: None,
            rendezvous_urls: vec!["https://rendezvous.example".to_string()],
            rendezvous_ca_pem: Some("rendezvous-ca".to_string()),
            cluster_ca_pem: Some("cluster-ca".to_string()),
            relay_mode: RelayMode::Required,
            target_node_id: Some(NodeId::new_v4()),
            ..http.clone()
        };
        for (field, changed) in [
            ("relay rendezvous URL", {
                let mut changed = relay.clone();
                changed
                    .rendezvous_urls
                    .push("https://backup.example".to_string());
                changed
            }),
            ("relay rendezvous trust root", {
                let mut changed = relay.clone();
                changed.rendezvous_ca_pem = Some("renewed-rendezvous-ca".to_string());
                changed
            }),
            ("relay inner trust root", {
                let mut changed = relay.clone();
                changed.cluster_ca_pem = Some("renewed-cluster-ca".to_string());
                changed
            }),
        ] {
            assert_route_key_changes(&relay, changed, &identity, field);
        }
    }

    #[test]
    fn unprobed_planned_route_construction_does_not_require_reachable_targets() {
        let cluster_id = Uuid::now_v7();
        let targets = [9_u16, 10_u16].map(|port| PlannedConnectionBootstrapTarget {
            cluster_id,
            rendezvous_urls: Vec::new(),
            rendezvous_mtls_required: false,
            relay_mode: RelayMode::Disabled,
            path_kind: TransportPathKind::DirectHttps,
            direct_candidate: None,
            server_base_url: Some(format!("http://127.0.0.1:{port}")),
            target_node_id: None,
            server_ca_pem: None,
            cluster_ca_pem: None,
            rendezvous_ca_pem: None,
            pairing_token: None,
            device_label: None,
            device_id: None,
        });

        let client =
            build_unprobed_client_with_optional_identity_from_planned_targets(&targets, None)
                .expect("inert route construction must not connect to either target");
        assert_eq!(client.connection_route_snapshot().endpoints.len(), 2);
    }

    #[test]
    fn load_root_certificate_reports_missing_file() {
        let missing_path =
            std::env::temp_dir().join(format!("ironmesh-missing-{}.pem", Uuid::now_v7()));
        let error =
            load_root_certificate(&missing_path).expect_err("missing certificate file should fail");
        assert!(
            error
                .to_string()
                .contains("failed to read server CA certificate")
        );
    }

    #[test]
    fn build_http_client_from_pem_rejects_invalid_base_url() {
        let error = match build_http_client_from_pem(None, "://bad-url") {
            Ok(_) => panic!("invalid URL should fail"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("failed to parse server base URL")
        );
    }

    #[test]
    fn startup_probe_returns_after_first_success_without_discarding_slow_fallback() {
        let fast_listener =
            TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("fast test listener should bind");
        let fast_addr = fast_listener
            .local_addr()
            .expect("fast test listener address should resolve");
        let fast_server = std::thread::spawn(move || {
            let probe_count = STARTUP_PROBE_WARMUP_COUNT + STARTUP_PROBE_SAMPLE_COUNT;
            for _ in 0..probe_count {
                let (mut socket, _) = fast_listener.accept().expect("fast probe should connect");
                let mut request = [0_u8; 1024];
                let _ = socket.read(&mut request);
                socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                    )
                    .expect("fast probe response should write");
            }
        });

        let slow_listener =
            TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("slow test listener should bind");
        let slow_addr = slow_listener
            .local_addr()
            .expect("slow test listener address should resolve");
        let slow_server = std::thread::spawn(move || {
            let (_socket, _) = slow_listener.accept().expect("slow probe should connect");
            std::thread::sleep(Duration::from_secs(2));
        });

        let slow_url = format!("http://{slow_addr}");
        let fast_url = format!("http://{fast_addr}");
        let started = Instant::now();
        let ordered = order_clients_by_startup_probe(
            vec![
                IronMeshClient::from_direct_base_url(&slow_url),
                IronMeshClient::from_direct_base_url(&fast_url),
            ],
            false,
        )
        .expect("a successful startup path should be returned");

        assert!(
            started.elapsed() < Duration::from_secs(1),
            "slow unfinished candidates must not hold up startup"
        );
        assert_eq!(
            ordered[0].direct_server_base_url().as_deref(),
            Some(fast_url.as_str())
        );
        assert_eq!(
            ordered[1].direct_server_base_url().as_deref(),
            Some(slow_url.as_str())
        );
        fast_server.join().expect("fast server should finish");
        slow_server.join().expect("slow server should finish");
    }

    #[test]
    fn signed_startup_probe_rejects_report_without_successful_sample() {
        let listener =
            TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("probe test listener should bind");
        let address = listener
            .local_addr()
            .expect("probe test listener address should resolve");
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("probe should connect");
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request);
            socket
                .write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 11\r\n\r\nunavailable",
                )
                .expect("failed probe response should write");
        });

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("probe test runtime should build");
        let error = runtime
            .block_on(probe_signed_client_startup_quality(
                &IronMeshClient::from_direct_base_url(format!("http://{address}")),
            ))
            .expect_err("an all-failed diagnostic report must not mark the path reachable");

        assert!(
            error
                .to_string()
                .contains("startup signed latency probe failed")
        );
        server.join().expect("probe test server should finish");
    }

    #[test]
    fn prioritize_socket_addrs_prefers_ipv4_and_deduplicates() {
        let ipv6 = SocketAddr::from((Ipv6Addr::LOCALHOST, 443));
        let ipv4 = SocketAddr::from((Ipv4Addr::LOCALHOST, 443));
        let prioritized = prioritize_socket_addrs([ipv6, ipv4, ipv6, ipv4]);
        assert_eq!(prioritized, vec![ipv4, ipv6]);
    }

    #[test]
    fn build_http_client_with_identity_from_planned_target_uses_direct_transport() {
        let identity = sample_identity();
        let client = build_http_client_with_identity_from_planned_target(
            &PlannedConnectionBootstrapTarget {
                cluster_id: identity.cluster_id,
                rendezvous_urls: vec!["https://rendezvous.example".to_string()],
                rendezvous_mtls_required: false,
                relay_mode: RelayMode::Fallback,
                path_kind: TransportPathKind::DirectHttps,
                direct_candidate: None,
                server_base_url: Some("https://node-a.example".to_string()),
                target_node_id: Some(NodeId::new_v4()),
                server_ca_pem: None,
                cluster_ca_pem: None,
                rendezvous_ca_pem: None,
                pairing_token: None,
                device_label: None,
                device_id: None,
            },
            &identity,
        )
        .expect("direct client should build");

        assert!(!client.uses_relay_transport());
        assert!(client.rendezvous_client().is_none());
    }

    #[test]
    fn build_http_client_with_identity_from_planned_target_requires_relay_target_node_id() {
        let identity = sample_identity();
        let error = match build_http_client_with_identity_from_planned_target(
            &PlannedConnectionBootstrapTarget {
                cluster_id: identity.cluster_id,
                rendezvous_urls: vec!["https://rendezvous.example".to_string()],
                rendezvous_mtls_required: false,
                relay_mode: RelayMode::Required,
                path_kind: TransportPathKind::RelayTunnel,
                direct_candidate: None,
                server_base_url: None,
                target_node_id: None,
                server_ca_pem: None,
                cluster_ca_pem: None,
                rendezvous_ca_pem: None,
                pairing_token: None,
                device_label: None,
                device_id: None,
            },
            &identity,
        ) {
            Ok(_) => panic!("missing relay node id should fail"),
            Err(error) => error,
        };

        assert!(
            error
                .to_string()
                .contains("relay-backed client transport target is missing target_node_id")
        );
    }

    #[test]
    fn build_http_client_with_identity_from_planned_target_requires_cluster_ca_for_inner_relay_mtls()
     {
        let mut identity = sample_identity();
        identity.rendezvous_client_identity_pem = Some("rendezvous-identity".to_string());
        let error = match build_http_client_with_identity_from_planned_target(
            &PlannedConnectionBootstrapTarget {
                cluster_id: identity.cluster_id,
                rendezvous_urls: vec!["https://rendezvous.example".to_string()],
                rendezvous_mtls_required: false,
                relay_mode: RelayMode::Required,
                path_kind: TransportPathKind::RelayTunnel,
                direct_candidate: None,
                server_base_url: None,
                target_node_id: Some(NodeId::new_v4()),
                server_ca_pem: None,
                cluster_ca_pem: None,
                rendezvous_ca_pem: None,
                pairing_token: None,
                device_label: None,
                device_id: None,
            },
            &identity,
        ) {
            Ok(_) => panic!("missing cluster CA should fail"),
            Err(error) => error,
        };

        assert!(
            error
                .to_string()
                .contains("requires target.cluster_ca_pem for inner relay mTLS")
        );
    }

    #[test]
    fn build_http_client_with_identity_from_planned_target_requires_rendezvous_identity_for_inner_relay_mtls()
     {
        let identity = sample_identity();
        let error = match build_http_client_with_identity_from_planned_target(
            &PlannedConnectionBootstrapTarget {
                cluster_id: identity.cluster_id,
                rendezvous_urls: vec!["https://rendezvous.example".to_string()],
                rendezvous_mtls_required: false,
                relay_mode: RelayMode::Required,
                path_kind: TransportPathKind::RelayTunnel,
                direct_candidate: None,
                server_base_url: None,
                target_node_id: Some(NodeId::new_v4()),
                server_ca_pem: None,
                cluster_ca_pem: Some("cluster-ca".to_string()),
                rendezvous_ca_pem: None,
                pairing_token: None,
                device_label: None,
                device_id: None,
            },
            &identity,
        ) {
            Ok(_) => panic!("missing rendezvous identity should fail"),
            Err(error) => error,
        };

        assert!(
            error
                .to_string()
                .contains("requires rendezvous_client_identity_pem for inner relay mTLS")
        );
    }

    #[test]
    fn build_http_client_with_identity_from_planned_target_builds_direct_quic_transport() {
        let identity = sample_identity();
        let client = build_http_client_with_identity_from_planned_target(
            &PlannedConnectionBootstrapTarget {
                cluster_id: identity.cluster_id,
                rendezvous_urls: vec!["https://rendezvous.example".to_string()],
                rendezvous_mtls_required: false,
                relay_mode: RelayMode::Fallback,
                path_kind: TransportPathKind::DirectQuic,
                direct_candidate: Some(ConnectionCandidate {
                    kind: CandidateKind::DirectQuic,
                    endpoint: "iroh://peer-key-1".to_string(),
                    rtt_ms: Some(8),
                    transport_hints: Some(ConnectionCandidateTransportHints {
                        transport_id: Some("peer-key-1".to_string()),
                        relay_url: Some("https://relay.example".to_string()),
                        relay_auth_token: None,
                        alpn: Some(DEFAULT_DIRECT_QUIC_ALPN.to_string()),
                        direct_socket_addrs: vec!["127.0.0.1:7000".to_string()],
                        observed_socket_addrs: Vec::new(),
                    }),
                }),
                server_base_url: None,
                target_node_id: Some(NodeId::new_v4()),
                server_ca_pem: None,
                cluster_ca_pem: None,
                rendezvous_ca_pem: None,
                pairing_token: None,
                device_label: None,
                device_id: None,
            },
            &identity,
        )
        .expect("direct QUIC target should build with client identity");

        assert!(!client.uses_relay_transport());
        assert!(client.direct_server_base_url().is_none());
        assert_eq!(
            client.connection_diagnostics().endpoints[0].locator,
            "iroh://peer-key-1"
        );
    }

    #[test]
    fn only_same_origin_direct_quic_relays_use_rendezvous_tickets() {
        let candidate = ConnectionCandidate {
            kind: CandidateKind::DirectQuic,
            endpoint: "iroh://peer-key-1".to_string(),
            rtt_ms: None,
            transport_hints: Some(ConnectionCandidateTransportHints {
                relay_url: Some("https://rendezvous.example:443".to_string()),
                ..ConnectionCandidateTransportHints::default()
            }),
        };

        assert!(candidate_uses_rendezvous_relay(
            &candidate,
            &["https://rendezvous.example".to_string()]
        ));
        assert!(!candidate_uses_rendezvous_relay(
            &candidate,
            &["https://other.example".to_string()]
        ));
    }

    #[test]
    fn build_client_with_optional_identity_from_planned_target_rejects_direct_quic_without_identity()
     {
        let error = match build_client_with_optional_identity_from_planned_target(
            &PlannedConnectionBootstrapTarget {
                cluster_id: Uuid::now_v7(),
                rendezvous_urls: vec!["https://rendezvous.example".to_string()],
                rendezvous_mtls_required: false,
                relay_mode: RelayMode::Fallback,
                path_kind: TransportPathKind::DirectQuic,
                direct_candidate: Some(ConnectionCandidate {
                    kind: CandidateKind::DirectQuic,
                    endpoint: "iroh://peer-key-1".to_string(),
                    rtt_ms: None,
                    transport_hints: None,
                }),
                server_base_url: None,
                target_node_id: Some(NodeId::new_v4()),
                server_ca_pem: None,
                cluster_ca_pem: None,
                rendezvous_ca_pem: None,
                pairing_token: None,
                device_label: None,
                device_id: None,
            },
            None,
        ) {
            Ok(_) => panic!("direct QUIC target should fail closed without identity as well"),
            Err(error) => error,
        };

        assert!(error.to_string().contains(
            "direct QUIC client transport target iroh://peer-key-1 requires enrolled client identity material"
        ));
    }

    #[test]
    fn build_http_client_from_planned_targets_rejects_direct_quic_when_no_http_targets_exist() {
        let error =
            match build_http_client_from_planned_targets(&[PlannedConnectionBootstrapTarget {
                cluster_id: Uuid::now_v7(),
                rendezvous_urls: vec!["https://rendezvous.example".to_string()],
                rendezvous_mtls_required: false,
                relay_mode: RelayMode::Fallback,
                path_kind: TransportPathKind::DirectQuic,
                direct_candidate: Some(ConnectionCandidate {
                    kind: CandidateKind::DirectQuic,
                    endpoint: "iroh://peer-key-1".to_string(),
                    rtt_ms: None,
                    transport_hints: None,
                }),
                server_base_url: None,
                target_node_id: Some(NodeId::new_v4()),
                server_ca_pem: None,
                cluster_ca_pem: None,
                rendezvous_ca_pem: None,
                pairing_token: None,
                device_label: None,
                device_id: None,
            }]) {
                Ok(_) => panic!("direct QUIC target should not be built as HTTP"),
                Err(error) => error,
            };

        assert!(error.to_string().contains(
            "direct QUIC client transport target iroh://peer-key-1 requires enrolled client identity material"
        ));
    }
}
