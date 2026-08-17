use super::{LogicalFileByteRange, WebUiConfig, parse_logical_file_range, router};
use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, ETAG, RANGE};
use axum::http::{HeaderMap, Response, StatusCode};
use axum::routing::get;
use bytes::Bytes;
use client_sdk::IronMeshClient;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::{io::AsyncReadExt, io::AsyncWriteExt};

#[derive(Clone)]
struct BinaryUpstreamState {
    payload: Arc<Vec<u8>>,
    head_fails: bool,
    accepts_ranges: bool,
    get_count: Arc<AtomicUsize>,
    release_second_chunk: Option<Arc<Notify>>,
    body_dropped: Arc<AtomicBool>,
}

struct UpstreamBodyDropGuard(Arc<AtomicBool>);

impl Drop for UpstreamBodyDropGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

fn binary_upstream_response_headers(
    payload_len: usize,
    selection: Option<LogicalFileByteRange>,
    accepts_ranges: bool,
) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if accepts_ranges {
        headers.insert(ACCEPT_RANGES, "bytes".parse().expect("valid header"));
    }
    headers.insert(ETAG, "\"binary-test-etag\"".parse().expect("valid header"));
    headers.insert(
        "x-ironmesh-object-size",
        payload_len.to_string().parse().expect("valid header"),
    );
    let content_length = selection
        .map(|range| range.end_inclusive - range.start + 1)
        .unwrap_or(payload_len as u64);
    headers.insert(
        CONTENT_LENGTH,
        content_length.to_string().parse().expect("valid header"),
    );
    if let Some(range) = selection {
        headers.insert(
            CONTENT_RANGE,
            format!(
                "bytes {}-{}/{}",
                range.start, range.end_inclusive, payload_len
            )
            .parse()
            .expect("valid header"),
        );
    }
    headers
}

async fn binary_upstream_head(
    State(state): State<BinaryUpstreamState>,
    AxumPath(_key): AxumPath<String>,
) -> Response<Body> {
    if state.head_fails {
        return Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .body(Body::empty())
            .expect("failure response should build");
    }
    let mut response = Response::new(Body::empty());
    *response.headers_mut() =
        binary_upstream_response_headers(state.payload.len(), None, state.accepts_ranges);
    response
}

async fn binary_upstream_get(
    State(state): State<BinaryUpstreamState>,
    AxumPath(_key): AxumPath<String>,
    headers: HeaderMap,
) -> Response<Body> {
    state.get_count.fetch_add(1, Ordering::SeqCst);
    let selection = state
        .accepts_ranges
        .then(|| {
            headers
                .get(RANGE)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| parse_logical_file_range(value, state.payload.len() as u64))
        })
        .flatten();
    let selected_bytes = selection
        .map(|range| {
            Bytes::copy_from_slice(
                &state.payload[range.start as usize..=range.end_inclusive as usize],
            )
        })
        .unwrap_or_else(|| Bytes::copy_from_slice(&state.payload));

    let body = if let Some(release_second_chunk) = state.release_second_chunk {
        let split_at = (selected_bytes.len() / 2).max(1).min(selected_bytes.len());
        let first = selected_bytes.slice(..split_at);
        let second = selected_bytes.slice(split_at..);
        let drop_guard = UpstreamBodyDropGuard(Arc::clone(&state.body_dropped));
        Body::from_stream(async_stream::stream! {
            let _drop_guard = drop_guard;
            yield Ok::<Bytes, io::Error>(first);
            release_second_chunk.notified().await;
            if !second.is_empty() {
                yield Ok::<Bytes, io::Error>(second);
            }
        })
    } else {
        Body::from(selected_bytes)
    };
    let mut response = Response::new(body);
    *response.status_mut() = selection
        .map(|_| StatusCode::PARTIAL_CONTENT)
        .unwrap_or(StatusCode::OK);
    *response.headers_mut() =
        binary_upstream_response_headers(state.payload.len(), selection, state.accepts_ranges);
    response
}

async fn start_binary_test_servers(
    upstream_state: BinaryUpstreamState,
) -> (String, JoinHandle<()>, JoinHandle<()>) {
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("upstream listener should bind");
    let upstream_address = upstream_listener
        .local_addr()
        .expect("upstream listener should have an address");
    let upstream_app = axum::Router::new()
        .route(
            "/api/v1/store/{*key}",
            get(binary_upstream_get).head(binary_upstream_head),
        )
        .with_state(upstream_state);
    let upstream = tokio::spawn(async move {
        let _ = axum::serve(upstream_listener, upstream_app).await;
    });

    let web_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("web UI listener should bind");
    let web_address = web_listener
        .local_addr()
        .expect("web UI listener should have an address");
    let app = router(WebUiConfig::from_client(
        IronMeshClient::from_direct_base_url(format!("http://{upstream_address}")),
    ));
    let web = tokio::spawn(async move {
        let _ = axum::serve(web_listener, app).await;
    });
    (format!("http://{web_address}"), web, upstream)
}

fn binary_test_state(payload: Vec<u8>) -> BinaryUpstreamState {
    BinaryUpstreamState {
        payload: Arc::new(payload),
        head_fails: false,
        accepts_ranges: true,
        get_count: Arc::new(AtomicUsize::new(0)),
        release_second_chunk: None,
        body_dropped: Arc::new(AtomicBool::new(false)),
    }
}

async fn wait_until(predicate: impl Fn() -> bool) {
    for _ in 0..200 {
        if predicate() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition did not become true");
}

#[tokio::test]
async fn binary_stream_preserves_head_range_and_attachment_contracts() {
    let payload = (0..64).map(|index| index as u8).collect::<Vec<_>>();
    let state = binary_test_state(payload.clone());
    let get_count = Arc::clone(&state.get_count);
    let (base_url, web, upstream) = start_binary_test_servers(state).await;
    let client = reqwest::Client::new();
    let inline_url = format!("{base_url}/api/v1/store/stream-binary?key=photos%2Fsample.jpg");

    let head = client
        .head(&inline_url)
        .header(RANGE, "bytes=4-11")
        .send()
        .await
        .expect("ranged HEAD should complete");
    assert_eq!(head.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(head.headers().get(CONTENT_LENGTH).unwrap(), "8");
    assert_eq!(head.headers().get(CONTENT_RANGE).unwrap(), "bytes 4-11/64");
    assert_eq!(head.headers().get(ACCEPT_RANGES).unwrap(), "bytes");
    assert_eq!(head.headers().get(ETAG).unwrap(), "\"binary-test-etag\"");
    assert_eq!(head.headers().get("content-type").unwrap(), "image/jpeg");
    assert_eq!(
        head.headers().get("content-disposition").unwrap(),
        "inline; filename=\"sample.jpg\""
    );
    assert_eq!(get_count.load(Ordering::SeqCst), 0);

    let unsatisfiable = client
        .get(&inline_url)
        .header(RANGE, "bytes=100-200")
        .send()
        .await
        .expect("unsatisfiable range should complete");
    assert_eq!(unsatisfiable.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(
        unsatisfiable.headers().get(CONTENT_RANGE).unwrap(),
        "bytes */64"
    );
    assert_eq!(get_count.load(Ordering::SeqCst), 0);

    let ranged = client
        .get(&inline_url)
        .header(RANGE, "bytes=4-11")
        .send()
        .await
        .expect("ranged GET should start");
    assert_eq!(ranged.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        ranged.bytes().await.expect("range body should complete"),
        payload[4..12]
    );

    let attachment = client
        .get(format!(
            "{base_url}/api/v1/store/get-binary?key=photos%2Fsample.jpg"
        ))
        .send()
        .await
        .expect("attachment GET should start");
    assert_eq!(attachment.status(), StatusCode::OK);
    assert_eq!(
        attachment.headers().get("content-type").unwrap(),
        "application/octet-stream"
    );
    assert_eq!(
        attachment.headers().get("content-disposition").unwrap(),
        "attachment; filename=\"sample.jpg\""
    );
    assert_eq!(
        attachment
            .bytes()
            .await
            .expect("attachment body should complete"),
        payload
    );

    web.abort();
    upstream.abort();
}

#[tokio::test]
async fn binary_stream_slices_non_range_upstreams_without_buffering_the_object() {
    let payload = (0..64).map(|index| index as u8).collect::<Vec<_>>();
    let mut state = binary_test_state(payload.clone());
    state.accepts_ranges = false;
    let (base_url, web, upstream) = start_binary_test_servers(state).await;

    let response = reqwest::Client::new()
        .get(format!(
            "{base_url}/api/v1/store/stream-binary?key=legacy.bin"
        ))
        .header(RANGE, "bytes=20-29")
        .send()
        .await
        .expect("fallback range request should start");
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        response
            .bytes()
            .await
            .expect("fallback range body should complete"),
        payload[20..30]
    );

    web.abort();
    upstream.abort();
}

#[tokio::test]
async fn binary_stream_delivers_the_first_chunk_before_upstream_finishes() {
    let release_second_chunk = Arc::new(Notify::new());
    let mut state = binary_test_state(vec![0x5a; 128 * 1024]);
    state.release_second_chunk = Some(Arc::clone(&release_second_chunk));
    let body_dropped = Arc::clone(&state.body_dropped);
    let (base_url, web, upstream) = start_binary_test_servers(state).await;

    let mut response = tokio::time::timeout(
        Duration::from_secs(2),
        reqwest::get(format!(
            "{base_url}/api/v1/store/stream-binary?key=video.mp4"
        )),
    )
    .await
    .expect("response headers should arrive before the body finishes")
    .expect("stream request should start");
    assert_eq!(response.status(), StatusCode::OK);
    let first = tokio::time::timeout(Duration::from_secs(2), response.chunk())
        .await
        .expect("first response chunk should arrive before upstream finishes")
        .expect("first chunk read should succeed")
        .expect("first chunk should be present");
    assert!(!first.is_empty());
    assert!(!body_dropped.load(Ordering::SeqCst));

    release_second_chunk.notify_one();
    let remaining = response
        .bytes()
        .await
        .expect("remaining response body should complete");
    assert_eq!(first.len() + remaining.len(), 128 * 1024);

    web.abort();
    upstream.abort();
}

#[tokio::test]
async fn dropping_binary_response_cancels_the_upstream_body() {
    let release_second_chunk = Arc::new(Notify::new());
    let mut state = binary_test_state(vec![0x3c; 128 * 1024]);
    state.release_second_chunk = Some(Arc::clone(&release_second_chunk));
    let body_dropped = Arc::clone(&state.body_dropped);
    let (base_url, web, upstream) = start_binary_test_servers(state).await;

    let address = base_url
        .strip_prefix("http://")
        .expect("test server URL should use HTTP");
    let mut socket = tokio::net::TcpStream::connect(address)
        .await
        .expect("test client should connect");
    socket
        .write_all(
            b"GET /api/v1/store/stream-binary?key=video.mp4 HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await
        .expect("test request should be written");

    let mut received = Vec::new();
    loop {
        let read = tokio::time::timeout(Duration::from_secs(2), socket.read_buf(&mut received))
            .await
            .expect("response should start")
            .expect("response bytes should be readable");
        assert!(read > 0, "response should include body bytes");
        if let Some(header_end) = received.windows(4).position(|part| part == b"\r\n\r\n")
            && received.len() > header_end + 4
        {
            break;
        }
    }
    drop(socket);
    release_second_chunk.notify_one();

    wait_until(|| body_dropped.load(Ordering::SeqCst)).await;

    web.abort();
    upstream.abort();
}

#[tokio::test]
async fn binary_stream_reports_head_failures_before_starting_a_body() {
    let mut state = binary_test_state(vec![0x7b; 32]);
    state.head_fails = true;
    let get_count = Arc::clone(&state.get_count);
    let (base_url, web, upstream) = start_binary_test_servers(state).await;

    let response = reqwest::get(format!(
        "{base_url}/api/v1/store/stream-binary?key=unavailable.bin"
    ))
    .await
    .expect("failed stream request should return an HTTP response");
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(get_count.load(Ordering::SeqCst), 0);

    web.abort();
    upstream.abort();
}
