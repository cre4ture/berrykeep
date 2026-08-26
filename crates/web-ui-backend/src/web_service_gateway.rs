use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::header::{
    CONNECTION, CONTENT_TYPE, COOKIE, HOST, LOCATION, ORIGIN, REFERER, SET_COOKIE, UPGRADE,
};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use common::NodeId;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::{Connect, Connected, Connection};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{Mutex, RwLock, Semaphore};
use tokio_util::compat::FuturesAsyncReadCompatExt;
use tower_service::Service;
use uuid::Uuid;

use super::{WebRuntime, WebState, cookie_values, current_sdk, error_response};

const SERVICE_HOST_SUFFIX: &str = ".localhost";
const OPEN_PATH: &str = "/_ironmesh/open";
const GATEWAY_SESSION_COOKIE: &str = "ironmesh_service_gateway_session";
const LAUNCH_TOKEN_TTL: Duration = Duration::from_secs(60);
const SERVICE_SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);
const MAX_PENDING_LAUNCHES: usize = 1_024;
const MAX_SERVICE_SESSIONS: usize = 2_048;
const MAX_POOLED_SERVICE_CLIENTS: usize = 256;
const MAX_IDLE_CONNECTIONS_PER_SERVICE: usize = 4;
const MAX_CONCURRENT_CONNECTS_PER_SERVICE: usize = 4;
const SERVICE_CLIENT_TTL: Duration = Duration::from_secs(60);
const SERVICE_CONNECTION_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const LAUNCH_TRANSITION_HTML: &str = concat!(
    "<!doctype html><html><head><meta charset=\"utf-8\">",
    "<meta http-equiv=\"refresh\" content=\"0;url=/\">",
    "<title>Opening private service</title></head>",
    "<body>Opening private service. <a href=\"/\">Continue</a>.</body></html>",
);

#[derive(Clone, Default)]
pub(super) struct WebServiceGateway {
    state: std::sync::Arc<Mutex<GatewayState>>,
}

#[derive(Default)]
struct GatewayState {
    launches: HashMap<String, PendingLaunch>,
    sessions: HashMap<String, ServiceSession>,
    clients: HashMap<(NodeId, String), PooledServiceClient>,
}

struct PendingLaunch {
    alias: String,
    node_id: NodeId,
    service_id: String,
    expires_at: Instant,
}

#[derive(Clone)]
struct ServiceSession {
    alias: String,
    node_id: NodeId,
    service_id: String,
    expires_at: Instant,
}

type ServiceHttpClient = Client<WebServiceConnector, Body>;

#[derive(Clone)]
struct PooledServiceClient {
    client: ServiceHttpClient,
    metadata: ServiceConnectionMetadata,
    expires_at: Instant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ServiceConnectionMetadata {
    authority: String,
    base_path: String,
    upstream_scheme: String,
}

#[derive(Clone)]
struct WebServiceConnector {
    runtime: Arc<RwLock<WebRuntime>>,
    node_id: NodeId,
    service_id: String,
    metadata: ServiceConnectionMetadata,
    initial_connection: Arc<Mutex<Option<client_sdk::ironmesh_client::WebServiceProxyConnection>>>,
    connect_permits: Arc<Semaphore>,
}

trait ServiceStream: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T> ServiceStream for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

struct PooledServiceStream {
    inner: Box<dyn ServiceStream>,
}

impl ServiceConnectionMetadata {
    fn from_connection(
        connection: &client_sdk::ironmesh_client::WebServiceProxyConnection,
    ) -> Self {
        Self {
            authority: connection.authority.clone(),
            base_path: connection.base_path.clone(),
            upstream_scheme: connection.upstream_scheme.clone(),
        }
    }

    fn absolute_uri(&self, path: &Uri) -> Result<Uri> {
        let path = path
            .path_and_query()
            .map(|value| value.as_str())
            .unwrap_or("/");
        format!("{}://{}{}", self.upstream_scheme, self.authority, path)
            .parse()
            .context("failed constructing pooled service request URI")
    }
}

impl PooledServiceClient {
    fn new(
        runtime: Arc<RwLock<WebRuntime>>,
        connection: client_sdk::ironmesh_client::WebServiceProxyConnection,
    ) -> Self {
        let metadata = ServiceConnectionMetadata::from_connection(&connection);
        let connector = WebServiceConnector {
            runtime,
            node_id: connection.node_id,
            service_id: connection.service_id.clone(),
            metadata: metadata.clone(),
            initial_connection: Arc::new(Mutex::new(Some(connection))),
            connect_permits: Arc::new(Semaphore::new(MAX_CONCURRENT_CONNECTS_PER_SERVICE)),
        };
        Self {
            client: build_service_http_client(connector),
            metadata,
            expires_at: Instant::now() + SERVICE_CLIENT_TTL,
        }
    }
}

fn build_service_http_client<C>(connector: C) -> Client<C, Body>
where
    C: Connect + Clone,
{
    let mut builder = Client::builder(TokioExecutor::new());
    builder
        .pool_timer(TokioTimer::new())
        .pool_idle_timeout(SERVICE_CONNECTION_IDLE_TIMEOUT)
        .pool_max_idle_per_host(MAX_IDLE_CONNECTIONS_PER_SERVICE);
    builder.build(connector)
}

impl WebServiceConnector {
    async fn open_connection(
        &self,
    ) -> io::Result<client_sdk::ironmesh_client::WebServiceProxyConnection> {
        if let Some(connection) = self.initial_connection.lock().await.take() {
            return Ok(connection);
        }
        let sdk = self.runtime.read().await.sdk.clone();
        sdk.open_web_service_proxy(self.node_id, &self.service_id)
            .await
            .map_err(|error| io::Error::other(format!("{error:#}")))
    }
}

impl Service<Uri> for WebServiceConnector {
    type Response = TokioIo<PooledServiceStream>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<Self::Response>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, destination: Uri) -> Self::Future {
        let connector = self.clone();
        Box::pin(async move {
            if destination.scheme_str() != Some(connector.metadata.upstream_scheme.as_str())
                || destination.authority().map(|value| value.as_str())
                    != Some(connector.metadata.authority.as_str())
            {
                return Err(io::Error::other(
                    "pooled service connector received an unexpected destination",
                ));
            }
            let permit = Arc::clone(&connector.connect_permits)
                .acquire_owned()
                .await
                .map_err(|_| io::Error::other("pooled service connector was closed"))?;
            let connection = connector.open_connection().await?;
            if connection.node_id != connector.node_id
                || connection.service_id != connector.service_id
                || ServiceConnectionMetadata::from_connection(&connection) != connector.metadata
            {
                return Err(io::Error::other(
                    "web service configuration changed while opening a pooled connection",
                ));
            }
            drop(permit);
            Ok(TokioIo::new(PooledServiceStream {
                inner: Box::new(connection.stream.compat()),
            }))
        })
    }
}

impl Connection for PooledServiceStream {
    fn connected(&self) -> Connected {
        Connected::new()
    }
}

impl AsyncRead for PooledServiceStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.inner).poll_read(cx, buffer)
    }
}

impl AsyncWrite for PooledServiceStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut *self.inner).poll_write(cx, buffer)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.inner).poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buffers: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut *self.inner).poll_write_vectored(cx, buffers)
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LaunchResponse {
    url: String,
    expires_in_seconds: u64,
}

impl WebServiceGateway {
    async fn issue_launch(
        &self,
        node_id: NodeId,
        service_id: &str,
        listener_port: u16,
    ) -> LaunchResponse {
        let alias = service_alias(node_id, service_id);
        let token = Uuid::new_v4().simple().to_string();
        let mut state = self.state.lock().await;
        state.prune();
        state.evict_earliest_launch_if_full();
        state.launches.insert(
            token.clone(),
            PendingLaunch {
                alias: alias.clone(),
                node_id,
                service_id: service_id.to_string(),
                expires_at: Instant::now() + LAUNCH_TOKEN_TTL,
            },
        );
        LaunchResponse {
            url: format!(
                "http://{alias}{SERVICE_HOST_SUFFIX}:{listener_port}{OPEN_PATH}?token={token}"
            ),
            expires_in_seconds: LAUNCH_TOKEN_TTL.as_secs(),
        }
    }

    async fn redeem_launch(&self, alias: &str, token: &str) -> Option<String> {
        let mut state = self.state.lock().await;
        state.prune();
        let launch = state.launches.remove(token)?;
        if launch.alias != alias || Instant::now() >= launch.expires_at {
            return None;
        }
        let session_token = Uuid::new_v4().simple().to_string();
        state.evict_earliest_session_if_full();
        state.sessions.insert(
            session_token.clone(),
            ServiceSession {
                alias: launch.alias,
                node_id: launch.node_id,
                service_id: launch.service_id,
                expires_at: Instant::now() + SERVICE_SESSION_TTL,
            },
        );
        Some(session_token)
    }

    async fn session(&self, alias: &str, tokens: &[&str]) -> Option<ServiceSession> {
        let mut state = self.state.lock().await;
        state.prune();
        tokens.iter().find_map(|token| {
            state
                .sessions
                .get(*token)
                .filter(|session| session.alias == alias)
                .cloned()
        })
    }

    async fn service_client(
        &self,
        runtime: &Arc<RwLock<WebRuntime>>,
        node_id: NodeId,
        service_id: &str,
    ) -> Result<PooledServiceClient> {
        let key = (node_id, service_id.to_string());
        {
            let mut state = self.state.lock().await;
            state.prune();
            if let Some(client) = state.clients.get(&key) {
                return Ok(client.clone());
            }
        }

        let sdk = runtime.read().await.sdk.clone();
        let connection = sdk.open_web_service_proxy(node_id, service_id).await?;
        let candidate = PooledServiceClient::new(Arc::clone(runtime), connection);
        let mut state = self.state.lock().await;
        state.prune();
        if let Some(client) = state.clients.get(&key) {
            return Ok(client.clone());
        }
        state.evict_earliest_client_if_full();
        state.clients.insert(key, candidate.clone());
        Ok(candidate)
    }

    pub(super) async fn clear_service_clients(&self) {
        self.state.lock().await.clients.clear();
    }
}

impl GatewayState {
    fn prune(&mut self) {
        let now = Instant::now();
        self.launches.retain(|_, launch| launch.expires_at > now);
        self.sessions.retain(|_, session| session.expires_at > now);
        self.clients.retain(|_, client| client.expires_at > now);
    }

    fn evict_earliest_launch_if_full(&mut self) {
        if self.launches.len() < MAX_PENDING_LAUNCHES {
            return;
        }
        if let Some(token) = self
            .launches
            .iter()
            .min_by_key(|(_, launch)| launch.expires_at)
            .map(|(token, _)| token.clone())
        {
            self.launches.remove(&token);
        }
    }

    fn evict_earliest_session_if_full(&mut self) {
        if self.sessions.len() < MAX_SERVICE_SESSIONS {
            return;
        }
        if let Some(token) = self
            .sessions
            .iter()
            .min_by_key(|(_, session)| session.expires_at)
            .map(|(token, _)| token.clone())
        {
            self.sessions.remove(&token);
        }
    }

    fn evict_earliest_client_if_full(&mut self) {
        if self.clients.len() < MAX_POOLED_SERVICE_CLIENTS {
            return;
        }
        if let Some(key) = self
            .clients
            .iter()
            .min_by_key(|(_, client)| client.expires_at)
            .map(|(key, _)| key.clone())
        {
            self.clients.remove(&key);
        }
    }
}

pub(super) async fn list_services(State(state): State<WebState>) -> Response {
    let sdk = current_sdk(&state).await;
    match sdk.list_web_services().await {
        Ok(services) => (StatusCode::OK, Json(services)).into_response(),
        Err(error) => error_response(
            StatusCode::BAD_GATEWAY,
            format!("failed listing web services: {error:#}"),
        ),
    }
}

pub(super) async fn launch_service(
    State(state): State<WebState>,
    Path((node_id, service_id)): Path<(NodeId, String)>,
    headers: HeaderMap,
) -> Response {
    let listener_port = match listener_port(&headers) {
        Ok(port) => port,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, error.to_string()),
    };
    let sdk = current_sdk(&state).await;
    let service_exists = match sdk.list_web_services_on_node(node_id).await {
        Ok(services) => services
            .iter()
            .any(|service| service.id == service_id && service.node_id == node_id),
        Err(error) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                format!("failed checking web service access: {error:#}"),
            );
        }
    };
    if !service_exists {
        return StatusCode::NOT_FOUND.into_response();
    }
    let launch = state
        .web_service_gateway
        .issue_launch(node_id, &service_id, listener_port)
        .await;
    (StatusCode::CREATED, Json(launch)).into_response()
}

pub(super) async fn dispatch_service_origin(
    State(state): State<WebState>,
    mut request: Request,
    next: Next,
) -> Response {
    let Some((alias, local_authority)) = service_origin(&request) else {
        return next.run(request).await;
    };
    if request.uri().path() == OPEN_PATH {
        return redeem_launch_request(&state, &alias, request.uri()).await;
    }
    let local_origin = format!("http://{local_authority}");
    if !service_request_origin_allowed(request.headers(), &local_origin) {
        return service_error(
            StatusCode::FORBIDDEN,
            "cross-origin private service requests are not allowed",
        );
    }
    let session_tokens =
        cookie_values(request.headers(), GATEWAY_SESSION_COOKIE).collect::<Vec<_>>();
    let session = state
        .web_service_gateway
        .session(&alias, &session_tokens)
        .await;
    let Some(session) = session else {
        return service_error(
            StatusCode::UNAUTHORIZED,
            "service session is missing or expired",
        );
    };
    match proxy_request(&state, &session, &local_authority, &mut request).await {
        Ok(response) => response,
        Err(error) => service_error(
            StatusCode::BAD_GATEWAY,
            &format!("web service proxy failed: {error:#}"),
        ),
    }
}

fn service_origin(request: &Request) -> Option<(String, String)> {
    let authority = request.headers().get(HOST)?.to_str().ok()?.trim();
    let hostname = authority
        .rsplit_once(':')
        .filter(|(_, port)| port.parse::<u16>().is_ok())
        .map(|(host, _)| host)
        .unwrap_or(authority)
        .to_ascii_lowercase();
    let alias = hostname.strip_suffix(SERVICE_HOST_SUFFIX)?;
    if alias.is_empty() || alias.contains('.') {
        return None;
    }
    Some((alias.to_string(), authority.to_string()))
}

fn listener_port(headers: &HeaderMap) -> Result<u16> {
    let authority = headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .context("local web UI request omitted Host")?;
    authority
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse::<u16>().ok())
        .or((!authority.contains(':')).then_some(80))
        .context("local web UI Host does not contain a valid listener port")
}

fn service_alias(node_id: NodeId, service_id: &str) -> String {
    let service = service_id
        .chars()
        .filter(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || *character == '-'
        })
        .take(36)
        .collect::<String>();
    let service_hash = blake3::hash(service_id.as_bytes()).to_hex().to_string();
    let node = node_id.simple().to_string();
    format!(
        "{}-{}-{}",
        service.trim_matches('-'),
        &service_hash[..12],
        &node[..12]
    )
}

fn service_request_origin_allowed(headers: &HeaderMap, local_origin: &str) -> bool {
    let Ok(local_origin) = reqwest::Url::parse(local_origin) else {
        return false;
    };
    if headers.get_all(ORIGIN).iter().any(|value| {
        value
            .to_str()
            .ok()
            .and_then(|value| reqwest::Url::parse(value).ok())
            .map(|origin| origin.origin() != local_origin.origin())
            .unwrap_or(true)
    }) {
        return false;
    }
    headers.get_all("sec-fetch-site").iter().all(|value| {
        value.to_str().is_ok_and(|value| {
            value.eq_ignore_ascii_case("same-origin") || value.eq_ignore_ascii_case("none")
        })
    })
}

async fn redeem_launch_request(state: &WebState, alias: &str, uri: &Uri) -> Response {
    let token = reqwest::Url::parse(&format!("http://local{uri}"))
        .ok()
        .and_then(|url| {
            url.query_pairs()
                .find(|(name, _)| name == "token")
                .map(|(_, value)| value.into_owned())
        });
    let Some(session_token) = state
        .web_service_gateway
        .redeem_launch(alias, token.as_deref().unwrap_or_default())
        .await
    else {
        return service_error(
            StatusCode::UNAUTHORIZED,
            "launch link is invalid or expired",
        );
    };
    launch_transition_response(&session_token)
}

fn launch_transition_response(session_token: &str) -> Response {
    let cookie = format!(
        "{GATEWAY_SESSION_COOKIE}={session_token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}",
        SERVICE_SESSION_TTL.as_secs()
    );
    let mut response = (
        StatusCode::OK,
        [(CONTENT_TYPE, "text/html; charset=utf-8")],
        LAUNCH_TRANSITION_HTML,
    )
        .into_response();
    if let Ok(cookie) = HeaderValue::from_str(&cookie) {
        response.headers_mut().append(SET_COOKIE, cookie);
    }
    response.headers_mut().insert(
        "content-security-policy",
        HeaderValue::from_static(
            "default-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
        ),
    );
    apply_service_security_headers(&mut response);
    response
}

async fn proxy_request(
    state: &WebState,
    session: &ServiceSession,
    local_authority: &str,
    request: &mut Request,
) -> Result<Response> {
    let service_client = state
        .web_service_gateway
        .service_client(&state.runtime, session.node_id, &session.service_id)
        .await?;
    let metadata = &service_client.metadata;
    let upstream_origin = format!("{}://{}", metadata.upstream_scheme, metadata.authority);
    let local_origin = format!("http://{local_authority}");
    let upstream_path = map_local_path(request.uri(), &metadata.base_path)?;
    let upstream_request_uri = upstream_path.clone();
    let websocket = is_websocket_upgrade(request.headers());
    let browser_upgrade = websocket.then(|| hyper::upgrade::on(&mut *request));

    let (mut parts, body) = std::mem::replace(request, Request::new(Body::empty())).into_parts();
    parts.uri = metadata.absolute_uri(&upstream_path)?;
    parts.version = axum::http::Version::HTTP_11;
    rewrite_request_headers(
        &mut parts.headers,
        &metadata.authority,
        &upstream_origin,
        &local_origin,
        websocket,
    )?;
    let upstream_request = http_request_from_parts(parts, body);
    let mut upstream_response = service_client
        .client
        .request(upstream_request)
        .await
        .context("configured upstream rejected the HTTP connection")?;
    let status = upstream_response.status();
    let upgraded = websocket && status == StatusCode::SWITCHING_PROTOCOLS;
    let upstream_upgrade = upgraded.then(|| hyper::upgrade::on(&mut upstream_response));
    let (parts, body) = upstream_response.into_parts();
    let mut response = if let (Some(browser_upgrade), Some(upstream_upgrade)) =
        (browser_upgrade, upstream_upgrade)
    {
        tokio::spawn(async move {
            let result = async {
                let browser = browser_upgrade.await.context("browser upgrade failed")?;
                let upstream = upstream_upgrade.await.context("upstream upgrade failed")?;
                let mut browser = TokioIo::new(browser);
                let mut upstream = TokioIo::new(upstream);
                tokio::io::copy_bidirectional(&mut browser, &mut upstream)
                    .await
                    .context("WebSocket byte bridge failed")?;
                Result::<()>::Ok(())
            }
            .await;
            if let Err(error) = result {
                tracing::debug!(%error, "web service WebSocket bridge ended with an error");
            }
        });
        Response::from_parts(parts, Body::empty())
    } else {
        Response::from_parts(parts, Body::new(body))
    };
    rewrite_response_headers(
        response.headers_mut(),
        &upstream_origin,
        &local_origin,
        &metadata.base_path,
        &upstream_request_uri,
        upgraded,
    )?;
    Ok(response)
}

fn http_request_from_parts(parts: axum::http::request::Parts, body: Body) -> hyper::Request<Body> {
    hyper::Request::from_parts(parts, body)
}

fn map_local_path(uri: &Uri, base_path: &str) -> Result<Uri> {
    reject_dot_segments(uri.path())?;
    let local = uri
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or("/");
    let (path, query) = local
        .split_once('?')
        .map(|(path, query)| (path, Some(query)))
        .unwrap_or((local, None));
    let base = base_path.trim_end_matches('/');
    let mapped = if base.is_empty() || base == "/" {
        path.to_string()
    } else if path == "/" {
        format!("{base}/")
    } else {
        format!("{base}{path}")
    };
    let mapped = match query {
        Some(query) => format!("{mapped}?{query}"),
        None => mapped,
    };
    mapped.parse().context("failed mapping local service URI")
}

fn reject_dot_segments(path: &str) -> Result<()> {
    let mut decoded = path.as_bytes().to_vec();
    loop {
        if decoded
            .split(|byte| matches!(*byte, b'/' | b'\\'))
            .any(|segment| segment == b"." || segment == b"..")
        {
            anyhow::bail!("local service path must not contain dot segments");
        }
        let next = percent_decode_once(&decoded);
        if next == decoded {
            return Ok(());
        }
        decoded = next;
    }
}

fn percent_decode_once(value: &[u8]) -> Vec<u8> {
    let mut decoded = Vec::with_capacity(value.len());
    let mut index = 0;
    while index < value.len() {
        if value[index] == b'%'
            && let Some(high) = value.get(index + 1).copied().and_then(hex_value)
            && let Some(low) = value.get(index + 2).copied().and_then(hex_value)
        {
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(value[index]);
            index += 1;
        }
    }
    decoded
}

const fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn rewrite_request_headers(
    headers: &mut HeaderMap,
    authority: &str,
    upstream_origin: &str,
    local_origin: &str,
    websocket: bool,
) -> Result<()> {
    remove_hop_by_hop_headers(headers, websocket);
    let private_headers = headers
        .keys()
        .filter(|name| name.as_str().starts_with("x-ironmesh-"))
        .cloned()
        .collect::<Vec<_>>();
    for name in private_headers {
        headers.remove(name);
    }
    headers.insert(HOST, HeaderValue::from_str(authority)?);
    if let Some(origin) = headers.get(ORIGIN).and_then(|value| value.to_str().ok())
        && origin.eq_ignore_ascii_case(local_origin)
    {
        headers.insert(ORIGIN, HeaderValue::from_str(upstream_origin)?);
    }
    if let Some(referer) = headers
        .get(REFERER)
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string)
        && let Some(suffix) = referer.strip_prefix(local_origin)
    {
        headers.insert(
            REFERER,
            HeaderValue::from_str(&format!("{upstream_origin}{suffix}"))?,
        );
    }
    if let Some(cookie) = headers
        .get(COOKIE)
        .and_then(|value| value.to_str().ok())
        .map(remove_gateway_cookie)
    {
        if cookie.is_empty() {
            headers.remove(COOKIE);
        } else {
            headers.insert(COOKIE, HeaderValue::from_str(&cookie)?);
        }
    }
    Ok(())
}

fn remove_gateway_cookie(value: &str) -> String {
    value
        .split(';')
        .map(str::trim)
        .filter(|part| {
            part.split_once('=')
                .map(|(name, _)| name != GATEWAY_SESSION_COOKIE)
                .unwrap_or(false)
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn rewrite_response_headers(
    headers: &mut HeaderMap,
    upstream_origin: &str,
    local_origin: &str,
    base_path: &str,
    upstream_request_uri: &Uri,
    websocket: bool,
) -> Result<()> {
    remove_hop_by_hop_headers(headers, websocket);
    for name in [
        "strict-transport-security",
        "public-key-pins",
        "alt-svc",
        "report-to",
        "nel",
        "content-security-policy-report-only",
    ] {
        headers.remove(name);
    }
    if let Some(location) = headers
        .get(LOCATION)
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string)
        && let Some(rewritten) = rewrite_location(
            &location,
            upstream_origin,
            local_origin,
            base_path,
            upstream_request_uri,
        )?
    {
        let value = HeaderValue::from_str(&rewritten)
            .context("rewritten upstream redirect is not a valid header value")?;
        headers.insert(LOCATION, value);
    }

    let cookies = headers.get_all(SET_COOKIE).iter().filter_map(|value| {
        value
            .to_str()
            .ok()
            .and_then(|value| rewrite_set_cookie(value, base_path))
            .and_then(|value| HeaderValue::from_str(&value).ok())
    });
    let cookies = cookies.collect::<Vec<_>>();
    headers.remove(SET_COOKIE);
    for cookie in cookies {
        headers.append(SET_COOKIE, cookie);
    }

    let policies = headers
        .get_all("content-security-policy")
        .iter()
        .map(|value| {
            value
                .to_str()
                .ok()
                .map(|policy| {
                    rewrite_content_security_policy(policy, upstream_origin, local_origin)
                })
                .and_then(|policy| HeaderValue::from_str(&policy).ok())
                .unwrap_or_else(|| value.clone())
        })
        .collect::<Vec<_>>();
    if !policies.is_empty() {
        headers.remove("content-security-policy");
        for policy in policies {
            headers.append("content-security-policy", policy);
        }
    }
    Ok(())
}

fn rewrite_content_security_policy(
    policy: &str,
    upstream_origin: &str,
    local_origin: &str,
) -> String {
    let upstream_websocket_origin = upstream_origin
        .strip_prefix("https://")
        .map(|authority| format!("wss://{authority}"))
        .or_else(|| {
            upstream_origin
                .strip_prefix("http://")
                .map(|authority| format!("ws://{authority}"))
        });
    let local_websocket_origin = local_origin
        .strip_prefix("http://")
        .map(|authority| format!("ws://{authority}"));
    let mut policy = policy.replace(upstream_origin, local_origin);
    if let (Some(upstream), Some(local)) = (upstream_websocket_origin, local_websocket_origin) {
        policy = policy.replace(&upstream, &local);
    }
    policy
        .split(';')
        .map(str::trim)
        .filter(|directive| {
            let lowercase = directive.to_ascii_lowercase();
            lowercase != "upgrade-insecure-requests"
                && lowercase != "block-all-mixed-content"
                && !lowercase.starts_with("report-uri ")
                && !lowercase.starts_with("report-to ")
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn rewrite_location(
    location: &str,
    upstream_origin: &str,
    local_origin: &str,
    base_path: &str,
    upstream_request_uri: &Uri,
) -> Result<Option<String>> {
    let upstream =
        reqwest::Url::parse(upstream_origin).context("upstream origin is not a valid URL")?;
    let current = reqwest::Url::parse(&format!(
        "{upstream_origin}{}",
        upstream_request_uri
            .path_and_query()
            .map(|value| value.as_str())
            .unwrap_or("/")
    ))
    .context("upstream request URI could not be resolved against its origin")?;
    let Ok(resolved) = current.join(location) else {
        return Ok(None);
    };
    if resolved.origin() != upstream.origin() {
        return Ok(None);
    }
    let local_path = strip_redirect_base_path(resolved.path(), base_path).with_context(|| {
        format!(
            "upstream redirect path {} escapes configured base path {base_path}",
            resolved.path()
        )
    })?;
    let mut rewritten = format!("{local_origin}{local_path}");
    if let Some(query) = resolved.query() {
        rewritten.push('?');
        rewritten.push_str(query);
    }
    if let Some(fragment) = resolved.fragment() {
        rewritten.push('#');
        rewritten.push_str(fragment);
    }
    Ok(Some(rewritten))
}

fn strip_redirect_base_path<'a>(path: &'a str, base_path: &str) -> Option<&'a str> {
    let base = base_path.trim_end_matches('/');
    if base.is_empty() || base == "/" {
        return Some(path);
    }
    if path == base {
        return Some("/");
    }
    path.strip_prefix(base)
        .filter(|suffix| suffix.starts_with('/'))
}

fn strip_base_path<'a>(path: &'a str, base_path: &str) -> &'a str {
    let base = base_path.trim_end_matches('/');
    if base.is_empty() || base == "/" {
        return path;
    }
    if path == base {
        return "/";
    }
    path.strip_prefix(base)
        .filter(|suffix| suffix.starts_with('/'))
        .unwrap_or(path)
}

fn rewrite_set_cookie(value: &str, base_path: &str) -> Option<String> {
    let mut parts = value.split(';');
    let pair = parts.next()?.trim();
    let (cookie_name, _) = pair.split_once('=')?;
    if cookie_name.trim() == GATEWAY_SESSION_COOKIE {
        return None;
    }
    let mut attributes = Vec::new();
    for attribute in parts.map(str::trim) {
        let name = attribute
            .split_once('=')
            .map(|(name, _)| name)
            .unwrap_or(attribute);
        if name.eq_ignore_ascii_case("domain") {
            continue;
        }
        if name.eq_ignore_ascii_case("path") {
            let path = attribute
                .split_once('=')
                .map(|(_, value)| value)
                .unwrap_or("/");
            let local_path = strip_base_path(path, base_path);
            attributes.push(format!(
                "Path={}",
                if local_path.is_empty() {
                    "/"
                } else {
                    local_path
                }
            ));
        } else {
            attributes.push(attribute.to_string());
        }
    }
    Some(
        std::iter::once(pair.to_string())
            .chain(attributes)
            .collect::<Vec<_>>()
            .join("; "),
    )
}

fn remove_hop_by_hop_headers(headers: &mut HeaderMap, preserve_upgrade: bool) {
    let connection_tokens = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|token| !token.is_empty() && !token.eq_ignore_ascii_case("upgrade"))
        .filter_map(|token| HeaderName::from_bytes(token.as_bytes()).ok())
        .collect::<Vec<_>>();
    for name in connection_tokens {
        headers.remove(name);
    }
    for name in [
        "proxy-connection",
        "proxy-authenticate",
        "proxy-authorization",
        "keep-alive",
        "transfer-encoding",
        "te",
        "trailer",
    ] {
        headers.remove(name);
    }
    if !preserve_upgrade {
        headers.remove(CONNECTION);
        headers.remove(UPGRADE);
    } else {
        headers.insert(CONNECTION, HeaderValue::from_static("Upgrade"));
    }
}

fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    headers
        .get(UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
        && headers
            .get(CONNECTION)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(',')
                    .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
            })
}

fn service_error(status: StatusCode, message: &str) -> Response {
    let mut response = (status, message.to_string()).into_response();
    apply_service_security_headers(&mut response);
    response
}

fn apply_service_security_headers(response: &mut Response) {
    response.headers_mut().insert(
        "cache-control",
        HeaderValue::from_static("no-store, private"),
    );
    response
        .headers_mut()
        .insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    response.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::{BodyExt as _, Full};
    use std::convert::Infallible;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone)]
    struct CountingConnector {
        opens: Arc<AtomicUsize>,
    }

    impl Service<Uri> for CountingConnector {
        type Response = TokioIo<PooledServiceStream>;
        type Error = io::Error;
        type Future = std::future::Ready<io::Result<Self::Response>>;

        fn poll_ready(&mut self, _cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _destination: Uri) -> Self::Future {
            self.opens.fetch_add(1, Ordering::SeqCst);
            let (client, server) = tokio::io::duplex(64 * 1024);
            tokio::spawn(async move {
                let service = hyper::service::service_fn(|_request| async {
                    Ok::<_, Infallible>(hyper::Response::new(Full::new(
                        hyper::body::Bytes::from_static(b"pooled"),
                    )))
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(server), service)
                    .await;
            });
            std::future::ready(Ok(TokioIo::new(PooledServiceStream {
                inner: Box::new(client),
            })))
        }
    }

    #[tokio::test]
    async fn service_http_client_reuses_an_idle_connection() {
        let opens = Arc::new(AtomicUsize::new(0));
        let client = build_service_http_client(CountingConnector {
            opens: Arc::clone(&opens),
        });

        for path in ["one", "two"] {
            let request = hyper::Request::builder()
                .uri(format!("http://service.localhost/{path}"))
                .body(Body::empty())
                .unwrap();
            let response = client.request(request).await.unwrap();
            assert_eq!(
                response.into_body().collect().await.unwrap().to_bytes(),
                "pooled"
            );
        }

        assert_eq!(opens.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn gateway_cookie_is_never_forwarded_upstream() {
        assert_eq!(
            remove_gateway_cookie("nas=one; ironmesh_service_gateway_session=secret; theme=dark"),
            "nas=one; theme=dark"
        );
    }

    #[test]
    fn upstream_paths_and_redirects_are_mapped_beneath_the_configured_base() {
        let uri: Uri = "/login?next=%2F".parse().unwrap();
        assert_eq!(map_local_path(&uri, "/ui").unwrap(), "/ui/login?next=%2F");
        let upstream_request: Uri = "/ui/login?next=%2F".parse().unwrap();
        assert_eq!(
            rewrite_location(
                "https://nas.home/ui/dashboard",
                "https://nas.home",
                "http://home-nas.localhost:4100",
                "/ui",
                &upstream_request,
            )
            .unwrap()
            .unwrap(),
            "http://home-nas.localhost:4100/dashboard"
        );
        assert_eq!(
            rewrite_location(
                "next?tab=storage#quota",
                "https://nas.home",
                "http://home-nas.localhost:4100",
                "/ui",
                &upstream_request,
            )
            .unwrap()
            .unwrap(),
            "http://home-nas.localhost:4100/next?tab=storage#quota"
        );
        assert!(
            rewrite_location(
                "https://other.example/",
                "https://nas.home",
                "http://home-nas.localhost:4100",
                "/ui",
                &upstream_request,
            )
            .unwrap()
            .is_none()
        );
        let error = rewrite_location(
            "https://nas.home/login",
            "https://nas.home",
            "http://home-nas.localhost:4100",
            "/ui",
            &upstream_request,
        )
        .unwrap_err();
        assert!(error.to_string().contains("escapes configured base path"));
    }

    #[test]
    fn service_origin_rejects_sibling_browser_requests() {
        let local_origin = "http://home-nas.localhost:4100";
        let mut headers = HeaderMap::new();
        assert!(service_request_origin_allowed(&headers, local_origin));

        headers.insert(ORIGIN, HeaderValue::from_static(local_origin));
        headers.insert("sec-fetch-site", HeaderValue::from_static("same-origin"));
        assert!(service_request_origin_allowed(&headers, local_origin));

        headers.insert(
            ORIGIN,
            HeaderValue::from_static("http://other-service.localhost:4100"),
        );
        assert!(!service_request_origin_allowed(&headers, local_origin));

        headers.remove(ORIGIN);
        headers.insert("sec-fetch-site", HeaderValue::from_static("cross-site"));
        assert!(!service_request_origin_allowed(&headers, local_origin));
        headers.insert("sec-fetch-site", HeaderValue::from_static("same-site"));
        assert!(!service_request_origin_allowed(&headers, local_origin));
    }

    #[test]
    fn every_upstream_content_security_policy_is_preserved() {
        let mut headers = HeaderMap::new();
        headers.append(
            "content-security-policy",
            HeaderValue::from_static("default-src https://nas.home"),
        );
        headers.append(
            "content-security-policy",
            HeaderValue::from_static("script-src https://nas.home; upgrade-insecure-requests"),
        );
        rewrite_response_headers(
            &mut headers,
            "https://nas.home",
            "http://home-nas.localhost:4100",
            "/ui",
            &"/ui/".parse().unwrap(),
            false,
        )
        .unwrap();

        let policies = headers
            .get_all("content-security-policy")
            .iter()
            .map(|value| value.to_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            policies,
            vec![
                "default-src http://home-nas.localhost:4100",
                "script-src http://home-nas.localhost:4100",
            ]
        );
    }

    #[test]
    fn upstream_path_mapping_rejects_encoded_and_nested_dot_segments() {
        for path in [
            "/../admin",
            "/./admin",
            "/%2e%2e/admin",
            "/..%2fadmin",
            "/%2E%2e%5cadmin",
            "/%252e%252e%252fadmin",
        ] {
            let uri: Uri = path.parse().unwrap();
            assert!(map_local_path(&uri, "/ui").is_err(), "accepted {path}");
        }

        let safe: Uri = "/files/report%2Epdf?next=../admin".parse().unwrap();
        assert_eq!(
            map_local_path(&safe, "/ui").unwrap(),
            "/ui/files/report%2Epdf?next=../admin"
        );
    }

    #[test]
    fn upstream_cookie_is_scoped_to_the_isolated_local_origin() {
        assert_eq!(
            rewrite_set_cookie(
                "sid=abc; Domain=nas.home; Path=/ui; Secure; HttpOnly; SameSite=Lax",
                "/ui",
            )
            .unwrap(),
            "sid=abc; Path=/; Secure; HttpOnly; SameSite=Lax"
        );
        assert!(
            rewrite_set_cookie(
                "ironmesh_service_gateway_session=overwritten; Path=/; HttpOnly",
                "/ui",
            )
            .is_none()
        );
    }

    #[tokio::test]
    async fn launch_tokens_are_single_use_and_bound_to_the_service_origin() {
        let gateway = WebServiceGateway::default();
        let node_id = Uuid::now_v7();
        let launch = gateway.issue_launch(node_id, "home-nas", 4100).await;
        let launch_url = reqwest::Url::parse(&launch.url).unwrap();
        assert_eq!(launch_url.host_str().unwrap().split('.').count(), 2);
        let token = launch_url
            .query_pairs()
            .find(|(name, _)| name == "token")
            .unwrap()
            .1
            .into_owned();
        let alias = launch_url
            .host_str()
            .unwrap()
            .strip_suffix(SERVICE_HOST_SUFFIX)
            .unwrap();
        assert!(
            gateway
                .redeem_launch("wrong-origin", &token)
                .await
                .is_none()
        );

        let second = gateway.issue_launch(node_id, "home-nas", 4100).await;
        let second_url = reqwest::Url::parse(&second.url).unwrap();
        let second_token = second_url
            .query_pairs()
            .find(|(name, _)| name == "token")
            .unwrap()
            .1
            .into_owned();
        let session = gateway.redeem_launch(alias, &second_token).await.unwrap();
        assert!(gateway.redeem_launch(alias, &second_token).await.is_none());
        assert!(gateway.session(alias, &[&session]).await.is_some());
        assert!(gateway.session("wrong-origin", &[&session]).await.is_none());
    }

    #[tokio::test]
    async fn a_shadow_cookie_cannot_hide_the_valid_service_session() {
        let gateway = WebServiceGateway::default();
        let node_id = Uuid::now_v7();
        let launch = gateway.issue_launch(node_id, "home-nas", 4100).await;
        let launch_url = reqwest::Url::parse(&launch.url).unwrap();
        let alias = launch_url
            .host_str()
            .unwrap()
            .strip_suffix(SERVICE_HOST_SUFFIX)
            .unwrap();
        let token = launch_url
            .query_pairs()
            .find(|(name, _)| name == "token")
            .unwrap()
            .1
            .into_owned();
        let session = gateway.redeem_launch(alias, &token).await.unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            COOKIE,
            HeaderValue::from_str(&format!(
                "{GATEWAY_SESSION_COOKIE}=sibling-service-shadow; \
                 {GATEWAY_SESSION_COOKIE}={session}"
            ))
            .unwrap(),
        );
        let tokens = cookie_values(&headers, GATEWAY_SESSION_COOKIE).collect::<Vec<_>>();

        assert!(gateway.session(alias, &tokens).await.is_some());
    }

    #[tokio::test]
    async fn launch_transition_keeps_strict_cookie_on_a_same_origin_page() {
        let response = launch_transition_response("session-secret");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );
        let cookie = response
            .headers()
            .get(SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Strict"));
        assert!(!response.headers().contains_key(LOCATION));
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(body.contains("http-equiv=\"refresh\""));
        assert!(body.contains("content=\"0;url=/\""));
    }
}
