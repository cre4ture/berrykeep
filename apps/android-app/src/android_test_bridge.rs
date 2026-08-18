use super::{runtime, throw_java_error};
use anyhow::{Context, Result};
use axum::{
    Router,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    routing::get,
};
use client_sdk::{
    BootstrapEndpoint, BootstrapEndpointUse, ClientIdentityMaterial, ConnectionBootstrap,
    RelayMode, StoreIndexEntry, StoreIndexMediaSummary, StoreIndexResponse,
};
use common::ClusterId;
use futures_util::{Sink, Stream};
use jni::{
    JNIEnv,
    objects::JClass,
    sys::{jint, jstring},
};
use serde::Serialize;
use std::{
    pin::Pin,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context as TaskContext, Poll},
};
use tokio::task::JoinHandle;
use transport_sdk::{
    BootstrapTrustRoots, BufferedTransportRequest, BufferedTransportResponse,
    DecodedWebSocketMessage, MultiplexConfig, MultiplexMode, MultiplexedSession,
    TRANSPORT_PROTOCOL_VERSION, TransportHeader, TransportSessionControlMessage,
    TransportSessionRole, WebSocketByteStream, WebSocketMessageCodec,
    perform_transport_server_handshake, read_buffered_transport_request,
    write_buffered_transport_response,
};

const TEST_CLUSTER_ID: &str = "019d02eb-ab39-7220-911a-c0eafcb38249";
const TEST_DOCUMENT_PATH: &str = "docs/readme.txt";
const TEST_DOCUMENT_REQUEST_PATH: &str = "/api/v1/store/docs%2Freadme.txt";
const TEST_DOCUMENT_SIZE_BYTES: usize = 1024 * 1024 + 17;
const EXPIRED_RENDEZVOUS_CLIENT_IDENTITY_PEM: &str = concat!(
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
    "-----END CERTIFICATE-----\n",
    "-----BEGIN PRIVATE KEY-----\n",
    "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgaxQmF3EgQxM8/nYg\n",
    "C4fi+hVjqma6xwFK4pwamjmotA+hRANCAASeG/ClE3s04e07hBjVXH8/IMPXIiGe\n",
    "wwOLPXEcJM4pU0ELoDcfpgZ0evvEiOKFC+R19CI3/dbbU02U0VnXMMXx\n",
    "-----END PRIVATE KEY-----\n"
);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AndroidRendezvousRenewalScenario {
    connection_bootstrap_json: String,
    expired_client_identity_json: String,
    renewed_rendezvous_client_identity_pem: String,
    remote_document_path: String,
    remote_document_size_bytes: usize,
}

#[derive(Clone)]
struct AndroidRendezvousRenewalState {
    public_url: String,
    renewed_rendezvous_client_identity_pem: String,
    captured_paths: Arc<Mutex<Vec<String>>>,
    paired_session_count: Arc<AtomicUsize>,
}

struct AndroidRendezvousRenewalServer {
    state: AndroidRendezvousRenewalState,
    handle: JoinHandle<()>,
}

fn android_rendezvous_renewal_server_state()
-> &'static Mutex<Option<AndroidRendezvousRenewalServer>> {
    static STATE: OnceLock<Mutex<Option<AndroidRendezvousRenewalServer>>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(None))
}

fn android_test_cluster_id() -> Result<ClusterId> {
    TEST_CLUSTER_ID
        .parse()
        .context("failed to parse android rendezvous renewal test cluster id")
}

fn renewed_rendezvous_client_identity_pem() -> String {
    EXPIRED_RENDEZVOUS_CLIENT_IDENTITY_PEM.replace('\n', "\r\n")
}

fn android_test_connection_bootstrap(public_url: &str) -> Result<ConnectionBootstrap> {
    Ok(ConnectionBootstrap {
        version: transport_sdk::CLIENT_BOOTSTRAP_VERSION,
        cluster_id: android_test_cluster_id()?,
        rendezvous_urls: Vec::new(),
        rendezvous_contact_list: None,
        rendezvous_mtls_required: false,
        direct_endpoints: vec![BootstrapEndpoint {
            url: public_url.to_string(),
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
        node_priority_overrides: Default::default(),
    })
}

fn android_test_client_identity() -> Result<ClientIdentityMaterial> {
    let mut identity = ClientIdentityMaterial::generate(
        android_test_cluster_id()?,
        None,
        Some("android-renewal-test-device".to_string()),
    )?;
    identity.credential_pem = Some("issued-credential".to_string());
    identity.rendezvous_client_identity_pem =
        Some(EXPIRED_RENDEZVOUS_CLIENT_IDENTITY_PEM.to_string());
    Ok(identity)
}

fn android_test_store_index_response(request_path: &str) -> StoreIndexResponse {
    let prefix = if android_test_query_matches(request_path, "prefix", "docs") {
        "docs"
    } else {
        ""
    };
    let entries = if prefix == "docs" {
        vec![android_test_document_entry()]
    } else if android_test_query_matches(request_path, "view", "tree") {
        vec![
            android_test_folder_entry("docs/", "prefix"),
            android_test_folder_entry("photos/", "prefix"),
        ]
    } else {
        vec![
            android_test_folder_entry("docs/", "prefix"),
            android_test_folder_entry("docs/", "key"),
            android_test_folder_entry("photos/", "prefix"),
            android_test_folder_entry("photos/", "key"),
        ]
    };
    StoreIndexResponse {
        prefix: prefix.to_string(),
        depth: 1,
        entry_count: entries.len(),
        total_entry_count: entries.len(),
        offset: 0,
        limit: None,
        has_more: false,
        next_cursor: None,
        sync_token: None,
        consistency_token: None,
        media_summary: StoreIndexMediaSummary::default(),
        entries,
    }
}

fn android_test_query_matches(request_path: &str, name: &str, expected_value: &str) -> bool {
    request_path
        .split_once('?')
        .map(|(_, query)| query)
        .into_iter()
        .flat_map(|query| query.split('&'))
        .filter_map(|pair| pair.split_once('='))
        .any(|(candidate_name, value)| candidate_name == name && value == expected_value)
}

fn android_test_document_entry() -> StoreIndexEntry {
    StoreIndexEntry {
        path: TEST_DOCUMENT_PATH.to_string(),
        entry_type: "key".to_string(),
        version: Some("v1".to_string()),
        content_hash: Some("hash-1".to_string()),
        size_bytes: Some(TEST_DOCUMENT_SIZE_BYTES as u64),
        modified_at_unix: None,
        content_fingerprint: None,
        media: None,
    }
}

fn android_test_folder_entry(path: &str, entry_type: &str) -> StoreIndexEntry {
    let is_object = entry_type == "key";
    StoreIndexEntry {
        path: path.to_string(),
        entry_type: entry_type.to_string(),
        version: None,
        content_hash: is_object.then(|| "folder-marker".to_string()),
        size_bytes: is_object.then_some(0),
        modified_at_unix: None,
        content_fingerprint: None,
        media: None,
    }
}

fn lock_server_state()
-> Result<std::sync::MutexGuard<'static, Option<AndroidRendezvousRenewalServer>>> {
    android_rendezvous_renewal_server_state()
        .lock()
        .map_err(|_| anyhow::anyhow!("android rendezvous renewal server state mutex poisoned"))
}

fn stop_android_rendezvous_renewal_server() -> Result<()> {
    let existing = lock_server_state()?.take();
    if let Some(server) = existing {
        server.handle.abort();
    }
    Ok(())
}

async fn start_android_rendezvous_renewal_server(
    renewed_rendezvous_client_identity_pem: String,
) -> Result<AndroidRendezvousRenewalServer> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .context("failed to bind android rendezvous renewal test server")?;
    let addr = listener
        .local_addr()
        .context("failed to resolve android rendezvous renewal test server address")?;
    let state = AndroidRendezvousRenewalState {
        public_url: format!("http://{addr}"),
        renewed_rendezvous_client_identity_pem,
        captured_paths: Arc::new(Mutex::new(Vec::new())),
        paired_session_count: Arc::new(AtomicUsize::new(0)),
    };
    let router = Router::new()
        .route("/transport/ws", get(android_direct_transport_ws))
        .with_state(state.clone());
    let handle = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, router).await {
            tracing::warn!(
                error = %err,
                "android rendezvous renewal test server terminated unexpectedly"
            );
        }
    });
    Ok(AndroidRendezvousRenewalServer { state, handle })
}

async fn android_direct_transport_ws(
    State(state): State<AndroidRendezvousRenewalState>,
    websocket: WebSocketUpgrade,
) -> impl axum::response::IntoResponse {
    websocket.on_upgrade(move |socket| async move {
        state.paired_session_count.fetch_add(1, Ordering::SeqCst);
        if let Err(err) = serve_android_multiplex_socket(state, socket).await {
            tracing::warn!(
                error = %err,
                "android rendezvous renewal test websocket session failed"
            );
        }
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AndroidTestWsMessage {
    Binary(Vec<u8>),
    Text(String),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close,
}

impl WebSocketMessageCodec for AndroidTestWsMessage {
    fn decode(self) -> std::io::Result<DecodedWebSocketMessage> {
        Ok(match self {
            Self::Binary(bytes) => DecodedWebSocketMessage::Binary(bytes),
            Self::Text(_) => DecodedWebSocketMessage::Ignore,
            Self::Ping(payload) => DecodedWebSocketMessage::Ping(payload),
            Self::Pong(_) => DecodedWebSocketMessage::Pong,
            Self::Close => DecodedWebSocketMessage::Close,
        })
    }

    fn binary(bytes: Vec<u8>) -> Self {
        Self::Binary(bytes)
    }

    fn pong(bytes: Vec<u8>) -> Self {
        Self::Pong(bytes)
    }
}

struct AndroidTestSocketAdapter {
    socket: WebSocket,
}

impl Stream for AndroidTestSocketAdapter {
    type Item = Result<AndroidTestWsMessage, axum::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match Pin::new(&mut this.socket).poll_next(cx) {
            Poll::Ready(Some(Ok(Message::Binary(bytes)))) => {
                Poll::Ready(Some(Ok(AndroidTestWsMessage::Binary(bytes.to_vec()))))
            }
            Poll::Ready(Some(Ok(Message::Text(text)))) => {
                Poll::Ready(Some(Ok(AndroidTestWsMessage::Text(text.to_string()))))
            }
            Poll::Ready(Some(Ok(Message::Ping(payload)))) => {
                Poll::Ready(Some(Ok(AndroidTestWsMessage::Ping(payload.to_vec()))))
            }
            Poll::Ready(Some(Ok(Message::Pong(payload)))) => {
                Poll::Ready(Some(Ok(AndroidTestWsMessage::Pong(payload.to_vec()))))
            }
            Poll::Ready(Some(Ok(Message::Close(_)))) => {
                Poll::Ready(Some(Ok(AndroidTestWsMessage::Close)))
            }
            Poll::Ready(Some(Err(err))) => Poll::Ready(Some(Err(err))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Sink<AndroidTestWsMessage> for AndroidTestSocketAdapter {
    type Error = axum::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.get_mut().socket).poll_ready(cx)
    }

    fn start_send(self: Pin<&mut Self>, item: AndroidTestWsMessage) -> Result<(), Self::Error> {
        let message = match item {
            AndroidTestWsMessage::Binary(bytes) => Message::Binary(bytes.into()),
            AndroidTestWsMessage::Text(text) => Message::Text(text.into()),
            AndroidTestWsMessage::Ping(payload) => Message::Ping(payload.into()),
            AndroidTestWsMessage::Pong(payload) => Message::Pong(payload.into()),
            AndroidTestWsMessage::Close => Message::Close(None),
        };
        Pin::new(&mut self.get_mut().socket).start_send(message)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.get_mut().socket).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.get_mut().socket).poll_close(cx)
    }
}

async fn serve_android_multiplex_socket(
    state: AndroidRendezvousRenewalState,
    socket: WebSocket,
) -> Result<()> {
    let transport = WebSocketByteStream::new(AndroidTestSocketAdapter { socket });
    let mut session =
        MultiplexedSession::spawn(transport, MultiplexMode::Server, MultiplexConfig::default())
            .context("failed to spawn android rendezvous renewal multiplex session")?;

    let hello = perform_transport_server_handshake(
        &mut session,
        TransportSessionControlMessage::Ready {
            protocol_version: TRANSPORT_PROTOCOL_VERSION,
            session_id: "android-rendezvous-renewal-test-session".to_string(),
            max_concurrent_streams: MultiplexConfig::default().max_num_streams,
        },
    )
    .await
    .context("failed to complete android rendezvous renewal transport handshake")?;
    if !matches!(
        hello,
        TransportSessionControlMessage::Hello {
            role: TransportSessionRole::Client,
            ..
        }
    ) {
        anyhow::bail!("android rendezvous renewal test received unexpected transport hello");
    }

    while let Some(mut stream) = session
        .accept_stream()
        .await
        .context("failed to accept android rendezvous renewal transport stream")?
    {
        let request = read_buffered_transport_request(&mut stream)
            .await
            .context("failed to decode android rendezvous renewal transport request")?;
        let path = request.path.clone();
        state
            .captured_paths
            .lock()
            .map_err(|_| anyhow::anyhow!("captured request path mutex poisoned"))?
            .push(path.clone());

        let (status, headers, body) = android_transport_response(&state, &request)?;
        write_buffered_transport_response(
            &mut stream,
            &BufferedTransportResponse {
                request_id: request.request_id,
                status,
                headers,
                body,
            },
        )
        .await
        .context("failed to write android rendezvous renewal transport response")?;
    }

    Ok(())
}

fn android_transport_response(
    state: &AndroidRendezvousRenewalState,
    request: &BufferedTransportRequest,
) -> Result<(u16, Vec<TransportHeader>, Vec<u8>)> {
    let method = request.method.as_str();
    let path = request.path.as_str();
    let resource_path = path
        .split_once('?')
        .map_or(path, |(resource_path, _)| resource_path);
    match (method, resource_path) {
        ("POST", "/api/v1/auth/device/renew-rendezvous-identity") => {
            let body = serde_json::to_vec(&serde_json::json!({
                "rendezvous_client_identity_pem": state.renewed_rendezvous_client_identity_pem,
            }))
            .context("failed to serialize android rendezvous renewal response")?;
            Ok((200, json_headers(body.len()), body))
        }
        ("GET", "/api/v1/store/index") => {
            let body = serde_json::to_vec(&android_test_store_index_response(path))
                .context("failed to serialize android store index response")?;
            Ok((200, json_headers(body.len()), body))
        }
        ("HEAD", TEST_DOCUMENT_REQUEST_PATH) => Ok((
            200,
            object_headers(TEST_DOCUMENT_SIZE_BYTES, None),
            Vec::new(),
        )),
        ("GET", TEST_DOCUMENT_REQUEST_PATH) => {
            let contents = test_document_contents();
            let Some((start, end_inclusive)) = requested_byte_range(&request.headers)? else {
                return Ok((200, object_headers(contents.len(), None), contents));
            };
            let body = contents[start..=end_inclusive].to_vec();
            let content_range =
                format!("bytes {start}-{end_inclusive}/{}", TEST_DOCUMENT_SIZE_BYTES,);
            Ok((
                206,
                object_headers(body.len(), Some(content_range.as_str())),
                body,
            ))
        }
        ("GET", "/api/v1/health") => Ok((200, Vec::new(), Vec::new())),
        _ => {
            let body = serde_json::to_vec(&serde_json::json!({
                "error": format!("unexpected test request {method} {path}"),
            }))
            .context("failed to serialize android unexpected request response")?;
            Ok((404, json_headers(body.len()), body))
        }
    }
}

fn json_headers(content_length: usize) -> Vec<TransportHeader> {
    vec![
        TransportHeader {
            name: "content-type".to_string(),
            value: "application/json".to_string(),
        },
        TransportHeader {
            name: "content-length".to_string(),
            value: content_length.to_string(),
        },
    ]
}

fn object_headers(content_length: usize, content_range: Option<&str>) -> Vec<TransportHeader> {
    let mut headers = vec![
        TransportHeader {
            name: "content-type".to_string(),
            value: "application/octet-stream".to_string(),
        },
        TransportHeader {
            name: "content-length".to_string(),
            value: content_length.to_string(),
        },
        TransportHeader {
            name: "accept-ranges".to_string(),
            value: "bytes".to_string(),
        },
        TransportHeader {
            name: "etag".to_string(),
            value: "\"android-saf-provider-test\"".to_string(),
        },
    ];
    if let Some(content_range) = content_range {
        headers.push(TransportHeader {
            name: "content-range".to_string(),
            value: content_range.to_string(),
        });
    }
    headers
}

fn requested_byte_range(headers: &[TransportHeader]) -> Result<Option<(usize, usize)>> {
    let Some(raw_range) = headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case("range"))
        .map(|header| header.value.as_str())
    else {
        return Ok(None);
    };
    let (start, end_inclusive) = raw_range
        .strip_prefix("bytes=")
        .and_then(|value| value.split_once('-'))
        .ok_or_else(|| anyhow::anyhow!("invalid Android test byte range {raw_range}"))?;
    let start = start
        .parse::<usize>()
        .with_context(|| format!("invalid Android test byte range start {raw_range}"))?;
    let end_inclusive = end_inclusive
        .parse::<usize>()
        .with_context(|| format!("invalid Android test byte range end {raw_range}"))?;
    if start > end_inclusive || end_inclusive >= TEST_DOCUMENT_SIZE_BYTES {
        anyhow::bail!("Android test byte range is out of bounds: {raw_range}");
    }
    Ok(Some((start, end_inclusive)))
}

fn test_document_contents() -> Vec<u8> {
    (0..TEST_DOCUMENT_SIZE_BYTES)
        .map(|index| (index % 251) as u8)
        .collect()
}

fn scenario_json() -> Result<String> {
    stop_android_rendezvous_renewal_server()?;
    let renewed_pem = renewed_rendezvous_client_identity_pem();
    let server =
        runtime()?.block_on(start_android_rendezvous_renewal_server(renewed_pem.clone()))?;
    let bootstrap_json = android_test_connection_bootstrap(&server.state.public_url)?
        .to_json_pretty()
        .context("failed to serialize android rendezvous renewal bootstrap")?;
    let expired_client_identity_json = android_test_client_identity()?
        .to_json_pretty()
        .context("failed to serialize android rendezvous renewal client identity")?;
    let scenario = AndroidRendezvousRenewalScenario {
        connection_bootstrap_json: bootstrap_json,
        expired_client_identity_json,
        renewed_rendezvous_client_identity_pem: renewed_pem,
        remote_document_path: TEST_DOCUMENT_PATH.to_string(),
        remote_document_size_bytes: TEST_DOCUMENT_SIZE_BYTES,
    };
    *lock_server_state()? = Some(server);
    serde_json::to_string(&scenario)
        .context("failed to serialize android rendezvous renewal scenario")
}

fn captured_request_paths_json() -> Result<String> {
    let paths = lock_server_state()?
        .as_ref()
        .map(|server| {
            server
                .state
                .captured_paths
                .lock()
                .map_err(|_| anyhow::anyhow!("captured request path mutex poisoned"))
                .map(|paths| paths.clone())
        })
        .transpose()?
        .unwrap_or_default();
    serde_json::to_string(&paths).context("failed to serialize captured request paths")
}

fn paired_session_count() -> Result<usize> {
    Ok(lock_server_state()?
        .as_ref()
        .map(|server| server.state.paired_session_count.load(Ordering::SeqCst))
        .unwrap_or(0))
}

#[cfg(target_os = "android")]
fn android_system_dns_server_count() -> Result<usize> {
    let (config, _) = hickory_resolver::system_conf::read_system_conf()
        .context("failed to read Android system DNS through the installed Iroh JNI context")?;
    Ok(config.name_servers().len())
}

/// # Safety
/// This function is intended to be called from Java via JNI during instrumentation tests.
#[cfg(target_os = "android")]
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_io_ironmesh_android_data_RustClientTestBridge_getAndroidSystemDnsServerCount(
    mut env: JNIEnv,
    _class: JClass,
) -> jint {
    match android_system_dns_server_count() {
        Ok(count) => count.try_into().unwrap_or(jint::MAX),
        Err(err) => {
            throw_java_error(
                &mut env,
                format!("rust getAndroidSystemDnsServerCount failed: {err:#}"),
            );
            0
        }
    }
}

/// # Safety
/// This function is intended to be called from Java via JNI during instrumentation tests.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_io_ironmesh_android_data_RustClientTestBridge_startRendezvousRenewalScenario(
    mut env: JNIEnv,
    _class: JClass,
) -> jstring {
    match scenario_json() {
        Ok(json) => match env.new_string(json) {
            Ok(value) => value.into_raw(),
            Err(err) => {
                throw_java_error(
                    &mut env,
                    format!(
                        "rust startRendezvousRenewalScenario failed to create java string: {err:#}"
                    ),
                );
                std::ptr::null_mut()
            }
        },
        Err(err) => {
            throw_java_error(
                &mut env,
                format!("rust startRendezvousRenewalScenario failed: {err:#}"),
            );
            std::ptr::null_mut()
        }
    }
}

/// # Safety
/// This function is intended to be called from Java via JNI during instrumentation tests.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_io_ironmesh_android_data_RustClientTestBridge_getCapturedRequestPaths(
    mut env: JNIEnv,
    _class: JClass,
) -> jstring {
    match captured_request_paths_json() {
        Ok(json) => match env.new_string(json) {
            Ok(value) => value.into_raw(),
            Err(err) => {
                throw_java_error(
                    &mut env,
                    format!("rust getCapturedRequestPaths failed to create java string: {err:#}"),
                );
                std::ptr::null_mut()
            }
        },
        Err(err) => {
            throw_java_error(
                &mut env,
                format!("rust getCapturedRequestPaths failed: {err:#}"),
            );
            std::ptr::null_mut()
        }
    }
}

/// # Safety
/// This function is intended to be called from Java via JNI during instrumentation tests.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_io_ironmesh_android_data_RustClientTestBridge_getPairedSessionCount(
    mut env: JNIEnv,
    _class: JClass,
) -> jint {
    match paired_session_count() {
        Ok(count) => count.try_into().unwrap_or(jint::MAX),
        Err(err) => {
            throw_java_error(
                &mut env,
                format!("rust getPairedSessionCount failed: {err:#}"),
            );
            0
        }
    }
}

/// # Safety
/// This function is intended to be called from Java via JNI during instrumentation tests.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_io_ironmesh_android_data_RustClientTestBridge_stopRendezvousRenewalScenario(
    mut env: JNIEnv,
    _class: JClass,
) {
    if let Err(err) = stop_android_rendezvous_renewal_server() {
        throw_java_error(
            &mut env,
            format!("rust stopRendezvousRenewalScenario failed: {err:#}"),
        );
    }
}
