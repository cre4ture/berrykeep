use std::collections::HashSet;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::Json;
use axum::extract::{Extension, Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use cap_std::ambient_authority;
use cap_std::fs::Dir;
use futures_util::io::{AsyncRead, AsyncWrite};
use reqwest::Url;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{WebPkiSupportedAlgorithms, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{OtherError, RootCertStore, SignatureScheme};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead as TokioAsyncRead, AsyncWrite as TokioAsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::RwLock;
use tokio_rustls::TlsConnector;
use tokio_util::compat::FuturesAsyncReadCompatExt;
use transport_sdk::{TransportHeader, TransportRequestHead, TransportResponseHead};

use super::{
    AuthenticatedClientIdentity, ServerState, append_admin_audit, authorize_admin_request,
    ensure_non_traversing_path, normalize_non_traversing_path, transport_service,
    validate_client_auth_request, write_transport_response_head,
};

const WEB_SERVICES_STATE_VERSION: u32 = 1;
const WEB_SERVICES_STATE_DIRECTORY: &str = "state";
const WEB_SERVICES_STATE_FILE: &str = "web_services.json";
const WEB_SERVICE_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_WEB_SERVICES: usize = 256;
const MAX_UPSTREAM_URL_BYTES: usize = 2_048;
const MAX_TLS_CA_PEM_BYTES: usize = 256 * 1024;
const WEB_SERVICE_PATH_PREFIX: &str = "/api/v1/web-services/";
const WEB_SERVICE_CONNECT_SUFFIX: &str = "/connect";
pub(crate) const HEADER_UPSTREAM_AUTHORITY: &str = "x-ironmesh-web-service-authority";
pub(crate) const HEADER_UPSTREAM_BASE_PATH: &str = "x-ironmesh-web-service-base-path";
pub(crate) const HEADER_UPSTREAM_SCHEME: &str = "x-ironmesh-web-service-scheme";

#[derive(Clone)]
pub(crate) struct WebServiceRegistry {
    state_dir: Arc<Dir>,
    services: Arc<RwLock<Vec<WebServiceConfig>>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct WebServiceConfig {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub upstream_url: String,
    #[serde(default)]
    pub allowed_device_ids: Vec<String>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub tls_ca_pem: Option<String>,
    #[serde(default)]
    pub tls_certificate_sha256: Option<String>,
    #[serde(default)]
    pub tls_server_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WebServiceSummary {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub node_id: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WebServiceUpsertRequest {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub upstream_url: String,
    #[serde(default)]
    pub allowed_device_ids: Vec<String>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub tls_ca_pem: Option<String>,
    #[serde(default)]
    pub tls_certificate_sha256: Option<String>,
    #[serde(default)]
    pub tls_server_name: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct WebServicesFile {
    version: u32,
    services: Vec<WebServiceConfig>,
}

trait UpstreamIo: TokioAsyncRead + TokioAsyncWrite + Send + Unpin {}
impl<T> UpstreamIo for T where T: TokioAsyncRead + TokioAsyncWrite + Send + Unpin {}

fn default_enabled() -> bool {
    true
}

impl WebServiceRegistry {
    pub(crate) async fn load(data_dir: &Path) -> Result<Self> {
        let data_dir = data_dir.to_path_buf();
        let (state_dir, payload) = tokio::task::spawn_blocking(move || {
            let state_dir = Arc::new(open_state_directory(&data_dir)?);
            let payload = match state_dir.read(WEB_SERVICES_STATE_FILE) {
                Ok(payload) => Some(payload),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(error).context("failed reading web services state"),
            };
            Ok::<_, anyhow::Error>((state_dir, payload))
        })
        .await
        .context("web service state initialization task failed")??;
        let services = match payload {
            Some(payload) => {
                let file: WebServicesFile = serde_json::from_slice(&payload)
                    .context("failed parsing web services state")?;
                if file.version != WEB_SERVICES_STATE_VERSION {
                    bail!("unsupported web services state version {}", file.version);
                }
                if file.services.len() > MAX_WEB_SERVICES {
                    bail!("web services state contains more than {MAX_WEB_SERVICES} services");
                }
                let mut service_ids = HashSet::new();
                for service in &file.services {
                    validate_service(service)?;
                    if !service_ids.insert(service.id.as_str()) {
                        bail!("duplicate web service id {}", service.id);
                    }
                }
                file.services
            }
            None => Vec::new(),
        };
        Ok(Self {
            state_dir,
            services: Arc::new(RwLock::new(services)),
        })
    }

    #[cfg(test)]
    pub(crate) fn empty(data_dir: &Path) -> Self {
        Self {
            state_dir: Arc::new(
                open_state_directory(data_dir)
                    .expect("test web service state directory should open"),
            ),
            services: Arc::new(RwLock::new(Vec::new())),
        }
    }

    async fn all(&self) -> Vec<WebServiceConfig> {
        let mut services = self.services.read().await.clone();
        services.sort_by(|left, right| left.name.cmp(&right.name).then(left.id.cmp(&right.id)));
        services
    }

    async fn allowed(&self, service_id: &str, device_id: &str) -> Option<WebServiceConfig> {
        self.services
            .read()
            .await
            .iter()
            .find(|service| {
                service.enabled
                    && service.id == service_id
                    && service
                        .allowed_device_ids
                        .iter()
                        .any(|allowed| allowed == device_id)
            })
            .cloned()
    }

    async fn insert(&self, service: WebServiceConfig) -> Result<bool> {
        validate_service(&service)?;
        let mut services = self.services.write().await;
        if services.iter().any(|existing| existing.id == service.id) {
            return Ok(false);
        }
        if services.len() >= MAX_WEB_SERVICES {
            bail!("a node may configure at most {MAX_WEB_SERVICES} web services");
        }
        let mut updated = services.clone();
        updated.push(service);
        persist_services(&self.state_dir, &updated).await?;
        *services = updated;
        Ok(true)
    }

    async fn replace(&self, service: WebServiceConfig) -> Result<bool> {
        validate_service(&service)?;
        let mut services = self.services.write().await;
        let Some(index) = services
            .iter()
            .position(|existing| existing.id == service.id)
        else {
            return Ok(false);
        };
        let mut updated = services.clone();
        updated[index] = service;
        persist_services(&self.state_dir, &updated).await?;
        *services = updated;
        Ok(true)
    }

    #[cfg(test)]
    pub(crate) async fn upsert(&self, service: WebServiceConfig) -> Result<bool> {
        if self.replace(service.clone()).await? {
            Ok(false)
        } else {
            self.insert(service).await
        }
    }

    async fn delete(&self, service_id: &str) -> Result<bool> {
        let mut services = self.services.write().await;
        let previous_len = services.len();
        let mut updated = services.clone();
        updated.retain(|service| service.id != service_id);
        let deleted = updated.len() != previous_len;
        if deleted {
            persist_services(&self.state_dir, &updated).await?;
            *services = updated;
        }
        Ok(deleted)
    }
}

impl From<WebServiceUpsertRequest> for WebServiceConfig {
    fn from(request: WebServiceUpsertRequest) -> Self {
        Self {
            id: request.id.trim().to_string(),
            name: request.name.trim().to_string(),
            description: trimmed_option(request.description),
            upstream_url: request.upstream_url.trim().to_string(),
            allowed_device_ids: request
                .allowed_device_ids
                .into_iter()
                .map(|device_id| device_id.trim().to_string())
                .collect(),
            enabled: request.enabled,
            tls_ca_pem: trimmed_option(request.tls_ca_pem),
            tls_certificate_sha256: trimmed_option(request.tls_certificate_sha256),
            tls_server_name: trimmed_option(request.tls_server_name),
        }
    }
}

fn trimmed_option(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn open_state_directory(data_dir: &Path) -> Result<Dir> {
    ensure_non_traversing_path(data_dir, "data directory")?;
    let data_dir = normalize_non_traversing_path(data_dir);
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("failed creating data directory {}", data_dir.display()))?;
    let root = Dir::open_ambient_dir(&data_dir, ambient_authority())
        .with_context(|| format!("failed opening data directory {}", data_dir.display()))?;
    match root.symlink_metadata(WEB_SERVICES_STATE_DIRECTORY) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("web service state directory must not be a symbolic link");
        }
        Ok(metadata) if !metadata.is_dir() => {
            bail!("web service state path must be a directory");
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => root
            .create_dir(WEB_SERVICES_STATE_DIRECTORY)
            .context("failed creating web service state directory")?,
        Err(error) => {
            return Err(error).context("failed inspecting web service state directory");
        }
    }
    root.open_dir(WEB_SERVICES_STATE_DIRECTORY)
        .context("failed opening web service state directory")
}

async fn persist_services(state_dir: &Arc<Dir>, services: &[WebServiceConfig]) -> Result<()> {
    let payload = serde_json::to_vec_pretty(&WebServicesFile {
        version: WEB_SERVICES_STATE_VERSION,
        services: services.to_vec(),
    })
    .context("failed serializing web services state")?;
    let state_dir = Arc::clone(state_dir);
    tokio::task::spawn_blocking(move || write_state_file_atomic(&state_dir, &payload))
        .await
        .context("web service state persistence task failed")?
}

fn write_state_file_atomic(state_dir: &Dir, payload: &[u8]) -> Result<()> {
    let temporary_file = format!(
        ".{WEB_SERVICES_STATE_FILE}.tmp-{}",
        uuid::Uuid::new_v4().simple()
    );
    state_dir
        .write(&temporary_file, payload)
        .context("failed writing temporary web services state")?;
    if let Err(error) = state_dir.rename(&temporary_file, state_dir, WEB_SERVICES_STATE_FILE) {
        let _ = state_dir.remove_file(&temporary_file);
        return Err(error).context("failed replacing web services state");
    }
    Ok(())
}

fn validate_service(service: &WebServiceConfig) -> Result<()> {
    let id = service.id.trim();
    if id != service.id
        || id.is_empty()
        || id.len() > 63
        || !id
            .bytes()
            .all(|value| value.is_ascii_lowercase() || value.is_ascii_digit() || value == b'-')
        || id.starts_with('-')
        || id.ends_with('-')
    {
        bail!("service id must be a lowercase DNS label using letters, digits, and hyphens");
    }
    let name = service.name.trim();
    if name != service.name || name.is_empty() || name.len() > 120 {
        bail!("service name must contain 1 to 120 characters");
    }
    if service
        .description
        .as_deref()
        .is_some_and(|value| value.len() > 500)
    {
        bail!("service description must not exceed 500 characters");
    }
    if service.allowed_device_ids.len() > 256 {
        bail!("a service may allow at most 256 devices");
    }
    let mut device_ids = HashSet::new();
    for device_id in &service.allowed_device_ids {
        let parsed = uuid::Uuid::parse_str(device_id)
            .with_context(|| format!("invalid allowed device id {device_id}"))?;
        if parsed.to_string() != *device_id {
            bail!("allowed device id {device_id} must use canonical UUID notation");
        }
        if !device_ids.insert(device_id) {
            bail!("allowed device ids must not contain duplicates");
        }
    }

    if service.upstream_url != service.upstream_url.trim() {
        bail!("upstream_url must not contain surrounding whitespace");
    }
    if service.upstream_url.len() > MAX_UPSTREAM_URL_BYTES {
        bail!("upstream_url must not exceed {MAX_UPSTREAM_URL_BYTES} bytes");
    }
    let url = Url::parse(&service.upstream_url).context("invalid upstream_url")?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("upstream_url must use http or https");
    }
    if url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() {
        bail!("upstream_url must contain a host and must not contain credentials");
    }
    if url.query().is_some() || url.fragment().is_some() {
        bail!("upstream_url must not contain a query or fragment");
    }
    if url.scheme() != "https"
        && (service.tls_ca_pem.is_some()
            || service.tls_certificate_sha256.is_some()
            || service.tls_server_name.is_some())
    {
        bail!("TLS trust settings require an https upstream_url");
    }
    if service.tls_ca_pem.is_some() && service.tls_certificate_sha256.is_some() {
        bail!("configure either tls_ca_pem or tls_certificate_sha256, not both");
    }
    if let Some(ca_pem) = service.tls_ca_pem.as_deref() {
        if ca_pem.len() > MAX_TLS_CA_PEM_BYTES {
            bail!("tls_ca_pem must not exceed {MAX_TLS_CA_PEM_BYTES} bytes");
        }
        parse_ca_certificates(ca_pem)?;
    }
    if let Some(fingerprint) = service.tls_certificate_sha256.as_deref() {
        normalize_sha256_fingerprint(fingerprint)?;
    }
    if let Some(server_name) = service.tls_server_name.as_deref() {
        ServerName::try_from(server_name.trim().to_string())
            .context("tls_server_name is not a valid DNS name or IP address")?;
    }
    Ok(())
}

fn parse_ca_certificates(pem: &str) -> Result<Vec<CertificateDer<'static>>> {
    let certificates = CertificateDer::pem_reader_iter(&mut io::Cursor::new(pem.as_bytes()))
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("failed parsing tls_ca_pem")?;
    if certificates.is_empty() {
        bail!("tls_ca_pem does not contain a certificate");
    }
    Ok(certificates)
}

fn normalize_sha256_fingerprint(value: &str) -> Result<String> {
    let normalized = value
        .chars()
        .filter(|character| *character != ':' && !character.is_ascii_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    if normalized.len() != 64 || !normalized.bytes().all(|value| value.is_ascii_hexdigit()) {
        bail!("tls_certificate_sha256 must be a 32-byte SHA-256 fingerprint");
    }
    Ok(normalized)
}

pub(crate) async fn list_client_services(
    State(state): State<ServerState>,
    Extension(identity): Extension<AuthenticatedClientIdentity>,
) -> impl IntoResponse {
    let services = state
        .web_services
        .all()
        .await
        .into_iter()
        .filter(|service| {
            service.enabled
                && service
                    .allowed_device_ids
                    .iter()
                    .any(|device_id| device_id == &identity.device_id)
        })
        .map(|service| WebServiceSummary {
            id: service.id,
            name: service.name,
            description: service.description,
            node_id: state.node_id.to_string(),
        })
        .collect::<Vec<_>>();
    (StatusCode::OK, Json(services))
}

pub(crate) async fn list_admin_services(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> Response {
    let action = "auth/web-services/list";
    let authz = match authorize_admin_request(&state, &headers, action, true, true, json!({})).await
    {
        Ok(authz) => authz,
        Err(status) => return status.into_response(),
    };
    let services = state.web_services.all().await;
    append_admin_audit(
        &state,
        action,
        &authz,
        true,
        true,
        true,
        "success",
        json!({ "service_count": services.len() }),
    )
    .await;
    (StatusCode::OK, Json(services)).into_response()
}

pub(crate) async fn create_admin_service(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(request): Json<WebServiceUpsertRequest>,
) -> Response {
    upsert_admin_service(state, headers, None, request).await
}

pub(crate) async fn update_admin_service(
    State(state): State<ServerState>,
    headers: HeaderMap,
    AxumPath(service_id): AxumPath<String>,
    Json(request): Json<WebServiceUpsertRequest>,
) -> Response {
    upsert_admin_service(state, headers, Some(service_id), request).await
}

async fn upsert_admin_service(
    state: ServerState,
    headers: HeaderMap,
    path_service_id: Option<String>,
    request: WebServiceUpsertRequest,
) -> Response {
    let action = "auth/web-services/upsert";
    let service: WebServiceConfig = request.into();
    let creating = path_service_id.is_none();
    let authz = match authorize_admin_request(
        &state,
        &headers,
        action,
        false,
        true,
        json!({ "service_id": service.id }),
    )
    .await
    {
        Ok(authz) => authz,
        Err(status) => return status.into_response(),
    };
    if path_service_id
        .as_deref()
        .is_some_and(|path_id| path_id != service.id)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "path service id does not match request id" })),
        )
            .into_response();
    }
    if let Err(error) = validate_service(&service) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": error.to_string() })),
        )
            .into_response();
    }
    let service_id = service.id.clone();
    let saved = if creating {
        state.web_services.insert(service.clone()).await
    } else {
        state.web_services.replace(service.clone()).await
    };
    match saved {
        Ok(true) => {
            append_admin_audit(
                &state,
                action,
                &authz,
                true,
                false,
                true,
                "success",
                json!({ "service_id": service_id, "created": creating }),
            )
            .await;
            (
                if creating {
                    StatusCode::CREATED
                } else {
                    StatusCode::OK
                },
                Json(service),
            )
                .into_response()
        }
        Ok(false) => (
            if creating {
                StatusCode::CONFLICT
            } else {
                StatusCode::NOT_FOUND
            },
            Json(json!({
                "error": if creating {
                    "web service already exists"
                } else {
                    "web service does not exist"
                }
            })),
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": error.to_string() })),
        )
            .into_response(),
    }
}

pub(crate) async fn delete_admin_service(
    State(state): State<ServerState>,
    headers: HeaderMap,
    AxumPath(service_id): AxumPath<String>,
) -> Response {
    let action = "auth/web-services/delete";
    let authz = match authorize_admin_request(
        &state,
        &headers,
        action,
        false,
        true,
        json!({ "service_id": service_id }),
    )
    .await
    {
        Ok(authz) => authz,
        Err(status) => return status.into_response(),
    };
    match state.web_services.delete(&service_id).await {
        Ok(true) => {
            append_admin_audit(
                &state,
                action,
                &authz,
                true,
                false,
                true,
                "success",
                json!({ "service_id": service_id }),
            )
            .await;
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": error.to_string() })),
        )
            .into_response(),
    }
}

pub(crate) async fn handle_proxy_stream<S>(
    state: &ServerState,
    request: TransportRequestHead,
    stream: &mut S,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Send + Unpin,
{
    let service_id = match parse_connect_service_id(&request) {
        Ok(service_id) => service_id,
        Err(error) => {
            return write_proxy_error(stream, request.request_id, 400, error.to_string()).await;
        }
    };
    let headers = match transport_service::header_map_from_transport_headers(&request.headers) {
        Ok(headers) => headers,
        Err(error) => {
            return write_proxy_error(stream, request.request_id, 400, error.to_string()).await;
        }
    };
    let identity = match validate_client_auth_request(
        state,
        &headers,
        request.method.as_str(),
        request.path.as_str(),
    )
    .await
    {
        Ok(identity) => identity,
        Err(status) => {
            return write_proxy_error(
                stream,
                request.request_id,
                status.as_u16(),
                "web service authentication failed".to_string(),
            )
            .await;
        }
    };
    let Some(service) = state
        .web_services
        .allowed(&service_id, &identity.device_id)
        .await
    else {
        return write_proxy_error(
            stream,
            request.request_id,
            404,
            "web service is unavailable".to_string(),
        )
        .await;
    };
    let url = Url::parse(&service.upstream_url).context("stored upstream_url is invalid")?;
    let mut upstream = match connect_upstream(&service, &url).await {
        Ok(upstream) => upstream,
        Err(error) => {
            return write_proxy_error(
                stream,
                request.request_id,
                502,
                format!("failed connecting configured web service: {error:#}"),
            )
            .await;
        }
    };
    write_transport_response_head(
        stream,
        &TransportResponseHead {
            request_id: request.request_id,
            status: 200,
            headers: vec![
                TransportHeader {
                    name: HEADER_UPSTREAM_AUTHORITY.to_string(),
                    value: upstream_authority(&url)?,
                },
                TransportHeader {
                    name: HEADER_UPSTREAM_BASE_PATH.to_string(),
                    value: normalized_base_path(&url),
                },
                TransportHeader {
                    name: HEADER_UPSTREAM_SCHEME.to_string(),
                    value: url.scheme().to_string(),
                },
            ],
        },
    )
    .await
    .context("failed acknowledging web service proxy stream")?;

    let mut client = stream.compat();
    tokio::io::copy_bidirectional(&mut client, upstream.as_mut())
        .await
        .context("web service proxy byte stream failed")?;
    Ok(())
}

fn parse_connect_service_id(request: &TransportRequestHead) -> Result<String> {
    if request.method != "CONNECT" {
        bail!("web service proxy streams require CONNECT");
    }
    if request.end_of_stream {
        bail!("web service proxy CONNECT must keep the stream open");
    }
    let Some(service_id) = request
        .path
        .strip_prefix(WEB_SERVICE_PATH_PREFIX)
        .and_then(|tail| tail.strip_suffix(WEB_SERVICE_CONNECT_SUFFIX))
    else {
        bail!("invalid web service proxy path");
    };
    if service_id.is_empty() || service_id.contains('/') {
        bail!("invalid web service id");
    }
    Ok(service_id.to_string())
}

async fn write_proxy_error<S>(
    stream: &mut S,
    request_id: String,
    status: u16,
    message: String,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    write_transport_response_head(
        stream,
        &TransportResponseHead {
            request_id,
            status,
            headers: vec![TransportHeader {
                name: "x-ironmesh-error".to_string(),
                value: message,
            }],
        },
    )
    .await
    .context("failed writing web service proxy error")?;
    futures_util::io::AsyncWriteExt::close(stream)
        .await
        .context("failed closing rejected web service proxy stream")
}

async fn connect_upstream(service: &WebServiceConfig, url: &Url) -> Result<Box<dyn UpstreamIo>> {
    let host = url.host_str().context("upstream_url is missing a host")?;
    let port = url
        .port_or_known_default()
        .context("upstream_url is missing a port")?;
    let tcp = tokio::time::timeout(
        WEB_SERVICE_CONNECT_TIMEOUT,
        TcpStream::connect((host, port)),
    )
    .await
    .with_context(|| format!("connection to configured upstream {host} timed out"))?
    .with_context(|| format!("failed connecting configured upstream {host}"))?;
    tcp.set_nodelay(true)
        .context("failed configuring upstream TCP stream")?;
    if url.scheme() == "http" {
        return Ok(Box::new(tcp));
    }

    let mut tls_config = build_upstream_tls_config(service)?;
    tls_config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let server_name = service
        .tls_server_name
        .as_deref()
        .unwrap_or(host)
        .trim()
        .to_string();
    let server_name = ServerName::try_from(server_name).context("invalid TLS server name")?;
    let tls = tokio::time::timeout(
        WEB_SERVICE_CONNECT_TIMEOUT,
        TlsConnector::from(Arc::new(tls_config)).connect(server_name, tcp),
    )
    .await
    .context("upstream TLS handshake timed out")?
    .context("upstream TLS handshake failed")?;
    Ok(Box::new(tls))
}

fn build_upstream_tls_config(service: &WebServiceConfig) -> Result<rustls::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    if let Some(fingerprint) = service.tls_certificate_sha256.as_deref() {
        let fingerprint = normalize_sha256_fingerprint(fingerprint)?;
        let supported_algorithms = provider.signature_verification_algorithms;
        return Ok(rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .context("failed selecting upstream TLS protocol versions")?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedServerCertificateVerifier {
                fingerprint,
                supported_algorithms,
            }))
            .with_no_client_auth());
    }

    let mut roots = RootCertStore::empty();
    if let Some(ca_pem) = service.tls_ca_pem.as_deref() {
        for certificate in parse_ca_certificates(ca_pem)? {
            roots
                .add(certificate)
                .context("failed adding configured upstream CA certificate")?;
        }
    } else {
        let native = rustls_native_certs::load_native_certs();
        for certificate in native.certs {
            roots
                .add(certificate)
                .context("failed adding native root certificate")?;
        }
        if !native.errors.is_empty() && roots.is_empty() {
            bail!("failed loading native root certificates");
        }
    }

    Ok(rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("failed selecting upstream TLS protocol versions")?
        .with_root_certificates(roots)
        .with_no_client_auth())
}

#[derive(Debug)]
struct PinnedServerCertificateVerifier {
    fingerprint: String,
    supported_algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for PinnedServerCertificateVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        let actual = format!("{:x}", Sha256::digest(end_entity.as_ref()));
        if actual != self.fingerprint {
            return Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::Other(OtherError(Arc::new(io::Error::other(
                    "configured web service certificate fingerprint did not match",
                )))),
            ));
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.supported_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.supported_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.supported_algorithms.supported_schemes()
    }
}

fn upstream_authority(url: &Url) -> Result<String> {
    let host = url.host_str().context("upstream_url is missing a host")?;
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    Ok(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    })
}

fn normalized_base_path(url: &Url) -> String {
    let path = url.path().trim_end_matches('/');
    if path.is_empty() {
        "/".to_string()
    } else {
        path.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_service() -> WebServiceConfig {
        WebServiceConfig {
            id: "home-nas".to_string(),
            name: "Home NAS".to_string(),
            description: None,
            upstream_url: "https://nas.home.arpa:8443/ui".to_string(),
            allowed_device_ids: vec![uuid::Uuid::now_v7().to_string()],
            enabled: true,
            tls_ca_pem: None,
            tls_certificate_sha256: Some("11:".repeat(31) + "11"),
            tls_server_name: None,
        }
    }

    #[test]
    fn accepts_fixed_https_service_with_certificate_pin() {
        validate_service(&valid_service()).expect("valid service should be accepted");
    }

    #[test]
    fn client_service_summary_uses_camel_case_node_id() {
        let summary = WebServiceSummary {
            id: "home-nas".to_string(),
            name: "Home NAS".to_string(),
            description: None,
            node_id: uuid::Uuid::now_v7().to_string(),
        };
        let json = serde_json::to_value(summary).unwrap();
        assert!(json.get("nodeId").is_some());
        assert!(json.get("node_id").is_none());
    }

    #[test]
    fn rejects_non_http_targets_and_invalid_ids() {
        let mut service = valid_service();
        service.upstream_url = "ssh://nas.home.arpa".to_string();
        assert!(validate_service(&service).is_err());

        let mut service = valid_service();
        service.id = "Home NAS".to_string();
        assert!(validate_service(&service).is_err());

        let mut service = valid_service();
        service.tls_ca_pem = Some("not a certificate".to_string());
        assert!(validate_service(&service).is_err());
    }

    #[test]
    fn connect_path_never_accepts_a_caller_supplied_host() {
        let request = TransportRequestHead {
            request_id: "request-1".to_string(),
            kind: transport_sdk::TransportStreamKind::WebServiceProxy,
            method: "CONNECT".to_string(),
            path: "/api/v1/web-services/home-nas/connect".to_string(),
            headers: Vec::new(),
            end_of_stream: false,
        };
        assert_eq!(parse_connect_service_id(&request).unwrap(), "home-nas");

        let mut request = request;
        request.path = "/api/v1/web-services/home-nas/connect/10.0.0.1".to_string();
        assert!(parse_connect_service_id(&request).is_err());
    }

    #[tokio::test]
    async fn registry_persists_acl_and_defaults_to_deny() {
        let root = std::env::temp_dir().join(format!(
            "ironmesh-web-services-test-{}",
            uuid::Uuid::now_v7().simple()
        ));
        let registry = WebServiceRegistry::load(&root).await.unwrap();
        let service = valid_service();
        let allowed_device = service.allowed_device_ids[0].clone();
        assert!(registry.upsert(service.clone()).await.unwrap());
        assert!(
            registry
                .allowed(&service.id, &allowed_device)
                .await
                .is_some()
        );
        assert!(
            registry
                .allowed(&service.id, &uuid::Uuid::now_v7().to_string())
                .await
                .is_none()
        );

        let reloaded = WebServiceRegistry::load(&root).await.unwrap();
        assert_eq!(reloaded.all().await, vec![service.clone()]);
        assert!(reloaded.delete(&service.id).await.unwrap());
        assert!(reloaded.all().await.is_empty());
        tokio::fs::remove_dir_all(&root).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn registry_rejects_state_directory_symlink_escape() {
        let root = std::env::temp_dir().join(format!(
            "ironmesh-web-services-symlink-test-{}",
            uuid::Uuid::now_v7().simple()
        ));
        let outside = std::env::temp_dir().join(format!(
            "ironmesh-web-services-outside-test-{}",
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join(WEB_SERVICES_STATE_DIRECTORY)).unwrap();

        let error = WebServiceRegistry::load(&root).await.err().unwrap();
        assert!(error.to_string().contains("must not be a symbolic link"));

        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn certificate_pin_accepts_only_the_exact_leaf_der() {
        let certificate = CertificateDer::from(vec![1_u8, 2, 3, 4]);
        let fingerprint = format!("{:x}", Sha256::digest(certificate.as_ref()));
        let verifier = PinnedServerCertificateVerifier {
            fingerprint,
            supported_algorithms: rustls::crypto::ring::default_provider()
                .signature_verification_algorithms,
        };
        let server_name = ServerName::try_from("nas.home.arpa").unwrap();
        assert!(
            verifier
                .verify_server_cert(&certificate, &[], &server_name, &[], UnixTime::now())
                .is_ok()
        );
        let changed = CertificateDer::from(vec![1_u8, 2, 3, 5]);
        assert!(
            verifier
                .verify_server_cert(&changed, &[], &server_name, &[], UnixTime::now())
                .is_err()
        );
    }
}
