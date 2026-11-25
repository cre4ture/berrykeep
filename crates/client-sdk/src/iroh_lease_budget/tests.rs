use super::*;
use axum::{
    Json, Router,
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, post},
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;
use transport_sdk::{
    IrohRelayTicket, IrohRelayTicketReleaseRequest, IrohRelayTicketReleaseResponse,
    IrohRelayTicketRequest, RendezvousClientConfig,
};

#[derive(Default)]
struct ServerState {
    leases: Mutex<HashMap<String, u64>>,
    issue_count: AtomicUsize,
    release_count: AtomicUsize,
    release_mode: AtomicUsize, // 0: success, 1: hang, 2: fail, 3: first hangs
    issue_blocked: AtomicBool,
    issue_failed: AtomicBool,
    issue_backpressure: AtomicBool,
    issue_started: Notify,
    issue_continue: Notify,
    expiry: AtomicU64,
    peak: AtomicUsize,
}

struct Server {
    url: String,
    relay_url: String,
    state: Arc<ServerState>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

async fn server(limit: usize) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let url = format!("http://{address}");
    let relay_url = format!("https://relay-{}.example", address.port());
    let issued_relay_url = relay_url.clone();
    let state = Arc::new(ServerState::default());
    state.expiry.store(now() + 3600, Ordering::SeqCst);
    let issue_state = state.clone();
    let release_state = state.clone();
    let router = Router::new()
        .route(
            "/control/iroh-relay/ticket",
            post(move |Json(request): Json<IrohRelayTicketRequest>| {
                let state = issue_state.clone();
                let relay_url = issued_relay_url.clone();
                async move {
                    state.issue_count.fetch_add(1, Ordering::SeqCst);
                    state.issue_started.notify_one();
                    if state.issue_blocked.load(Ordering::SeqCst) {
                        state.issue_continue.notified().await;
                    }
                    if state.issue_backpressure.load(Ordering::SeqCst) {
                        return (
                            StatusCode::TOO_MANY_REQUESTS,
                            [("Retry-After", "1")],
                            "ticket limit",
                        )
                            .into_response();
                    }
                    let expiry = state.expiry.load(Ordering::SeqCst);
                    {
                        let mut leases = lock(&state.leases);
                        leases.retain(|_, expiry| *expiry > now());
                        if !leases.contains_key(&request.endpoint_id) && leases.len() >= limit {
                            return (StatusCode::TOO_MANY_REQUESTS, "server lease limit reached")
                                .into_response();
                        }
                        leases.insert(request.endpoint_id.clone(), expiry);
                        state.peak.fetch_max(leases.len(), Ordering::SeqCst);
                    }
                    if state.issue_failed.load(Ordering::SeqCst) {
                        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                    }
                    Json(IrohRelayTicket {
                        public_urls: vec![relay_url],
                        auth_token: request.endpoint_id,
                        expires_at_unix: expiry,
                        quic_port: None,
                    })
                    .into_response()
                }
            }),
        )
        .route(
            "/control/iroh-relay/ticket/release",
            delete(move |Json(request): Json<IrohRelayTicketReleaseRequest>| {
                let state = release_state.clone();
                async move {
                    let attempt = state.release_count.fetch_add(1, Ordering::SeqCst);
                    match state.release_mode.load(Ordering::SeqCst) {
                        1 => std::future::pending::<()>().await,
                        2 => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                        3 if attempt == 0 => std::future::pending::<()>().await,
                        _ => {}
                    }
                    let released = lock(&state.leases).remove(&request.endpoint_id).is_some();
                    Json(IrohRelayTicketReleaseResponse { released }).into_response()
                }
            }),
        );
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    Server {
        url,
        relay_url,
        state,
        task,
    }
}

fn rendezvous(servers: &[&Server]) -> RendezvousControlClient {
    RendezvousControlClient::new(
        RendezvousClientConfig {
            cluster_id: uuid::Uuid::now_v7(),
            rendezvous_urls: servers.iter().map(|server| server.url.clone()).collect(),
            heartbeat_interval_secs: 15,
        },
        None,
        None,
    )
    .unwrap()
}

async fn owner(
    budget: &IrohRelayLeaseBudget,
    client: &RendezvousControlClient,
) -> Arc<IrohRelayLeaseHandle> {
    budget
        .lease_for_endpoint(
            client.clone(),
            &iroh::SecretKey::generate().public().to_string(),
        )
        .unwrap()
}

async fn issue_all(owner: &Arc<IrohRelayLeaseHandle>) -> Vec<String> {
    let mut collection = owner.request_tickets().await.unwrap();
    let mut origins = vec![collection.first_ticket_rendezvous_url().to_string()];
    while let Some(result) = collection.next_ticket_with_origin().await {
        origins.push(result.unwrap().rendezvous_url);
    }
    origins
}

async fn eventually(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("condition should become true");
}

#[tokio::test]
async fn independent_endpoints_issue_concurrently_at_the_same_origin() {
    let public = server(8).await;
    public.state.issue_blocked.store(true, Ordering::SeqCst);
    let client = rendezvous(&[&public]);
    let budget = IrohRelayLeaseBudget::default();
    let mut requests = Vec::new();
    let mut owners = Vec::new();
    for _ in 0..3 {
        let owner = owner(&budget, &client).await;
        let issuing = owner.clone();
        requests.push(tokio::spawn(async move { issuing.request_tickets().await }));
        owners.push(owner);
    }
    tokio::time::timeout(Duration::from_secs(1), async {
        while public.state.issue_count.load(Ordering::SeqCst) < 3 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("free lease slots must allow the transport's three concurrent handshakes");
    public.state.issue_blocked.store(false, Ordering::SeqCst);
    public.state.issue_continue.notify_waiters();
    for request in requests {
        request.await.unwrap().unwrap();
    }
    assert_eq!(public.state.peak.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn eviction_waits_for_in_flight_issuance_before_releasing_its_slot() {
    let public = server(1).await;
    let client = rendezvous(&[&public]);
    let budget = IrohRelayLeaseBudget::with_limit(1);
    let first = owner(&budget, &client).await;
    public.state.issue_blocked.store(true, Ordering::SeqCst);
    let issuing = first.clone();
    let first_request = tokio::spawn(async move { issuing.request_tickets().await });
    public.state.issue_started.notified().await;
    let second = owner(&budget, &client).await;
    let issuing = second.clone();
    let second_request = tokio::spawn(async move { issuing.request_tickets().await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(public.state.release_count.load(Ordering::SeqCst), 0);
    public.state.issue_blocked.store(false, Ordering::SeqCst);
    public.state.issue_continue.notify_waiters();
    let _ = first_request.await.unwrap();
    second_request.await.unwrap().unwrap();
    assert_eq!(public.state.peak.load(Ordering::SeqCst), 1);
    assert_eq!(budget.active_per_origin(&public.url), 1);
    assert!(lock(&public.state.leases).contains_key(second.endpoint_id()));
}

#[tokio::test]
async fn healthy_origin_admission_does_not_wait_for_a_hanging_origin_release() {
    let public = server(1).await;
    let lan = server(1).await;
    let budget = IrohRelayLeaseBudget::with_limit(1);
    let client = rendezvous(&[&lan, &public]);
    let first = owner(&budget, &client).await;
    assert_eq!(issue_all(&first).await.len(), 2);
    lan.state.release_mode.store(1, Ordering::SeqCst);
    let second = owner(&budget, &client).await;

    let collection = tokio::time::timeout(Duration::from_millis(200), second.request_tickets())
        .await
        .expect("public admission must not wait for LAN cleanup")
        .unwrap();

    assert_eq!(collection.first_ticket_rendezvous_url(), public.url);
    eventually(|| lan.state.release_count.load(Ordering::SeqCst) > 0).await;
    assert_eq!(budget.active_per_origin(&public.url), 1);
    assert_eq!(budget.active_per_origin(&lan.url), 1);
    assert_eq!(lan.state.issue_count.load(Ordering::SeqCst), 1);
    assert_eq!(public.state.issue_count.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn cancelled_release_must_retry_before_freeing_capacity() {
    let server = server(1).await;
    let budget = IrohRelayLeaseBudget::with_limit(1);
    let lease = owner(&budget, &rendezvous(&[&server])).await;
    issue_all(&lease).await;
    server.state.release_mode.store(3, Ordering::SeqCst);

    assert!(
        tokio::time::timeout(Duration::from_millis(50), lease.release_now("cancelled"))
            .await
            .is_err()
    );
    // The cancelled request is unconfirmed; a cleanup worker or this retry must
    // actually send another release instead of accepting an in-flight flag.
    tokio::time::timeout(Duration::from_secs(1), lease.release_now("retry"))
        .await
        .unwrap()
        .unwrap();

    assert_eq!(server.state.release_count.load(Ordering::SeqCst), 2);
    assert_eq!(budget.active_per_origin(&server.url), 0);
    assert!(lock(&server.state.leases).is_empty());
}

#[tokio::test]
async fn ninth_ticket_releases_lru_and_preserves_the_per_origin_limit() {
    let server = server(MAX_ACTIVE_IROH_RELAY_LEASES_PER_ORIGIN).await;
    let client = rendezvous(&[&server]);
    let budget = IrohRelayLeaseBudget::default();
    let mut leases = Vec::new();
    for _ in 0..MAX_ACTIVE_IROH_RELAY_LEASES_PER_ORIGIN {
        let lease = owner(&budget, &client).await;
        issue_all(&lease).await;
        leases.push(lease);
    }
    leases[0].touch();
    let ninth = owner(&budget, &client).await;
    issue_all(&ninth).await;

    assert!(leases[0].is_active());
    assert!(!leases[1].is_active());
    assert!(ninth.is_active());
    assert!(!lock(&server.state.leases).contains_key(leases[1].endpoint_id()));
    assert_eq!(
        budget.active_per_origin(&server.url),
        MAX_ACTIVE_IROH_RELAY_LEASES_PER_ORIGIN
    );
    assert_eq!(
        server.state.peak.load(Ordering::SeqCst),
        MAX_ACTIVE_IROH_RELAY_LEASES_PER_ORIGIN
    );
}

#[tokio::test]
async fn eviction_at_one_origin_preserves_other_origins_and_filters_refreshes() {
    let a = server(1).await;
    let b = server(1).await;
    let budget = IrohRelayLeaseBudget::with_limit(1);
    let combined = owner(&budget, &rendezvous(&[&a, &b])).await;
    issue_all(&combined).await;
    assert_eq!(combined.ticket_set().relay_configs().len(), 2);
    let changes = combined.subscribe_changes();
    let b_only = owner(&budget, &rendezvous(&[&b])).await;
    issue_all(&b_only).await;

    assert!(combined.is_active());
    assert!(combined.authorizes_origin(&a.url));
    assert!(!combined.authorizes_origin(&b.url));
    assert!(changes.has_changed().unwrap());
    let remaining = combined.ticket_set().relay_configs();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].url.trim_end_matches('/'), a.relay_url);
    let mut refresh = combined.request_tickets().await.unwrap();
    assert_eq!(refresh.first_ticket_rendezvous_url(), a.url);
    while let Some(result) = refresh.next_ticket_with_origin().await {
        assert!(result.is_err());
    }
    assert_eq!(b.state.issue_count.load(Ordering::SeqCst), 2);
    assert_eq!(a.state.release_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn failed_release_retains_capacity_and_recovers_with_background_backoff() {
    let server = server(1).await;
    let budget = IrohRelayLeaseBudget::with_limit(1);
    let client = rendezvous(&[&server]);
    let first = owner(&budget, &client).await;
    issue_all(&first).await;
    server.state.release_mode.store(2, Ordering::SeqCst);
    assert!(first.release_now("test_failure").await.is_err());
    assert_eq!(budget.active_per_origin(&server.url), 1);
    let second = owner(&budget, &client).await;
    assert!(second.request_tickets().await.is_err());
    assert_eq!(server.state.issue_count.load(Ordering::SeqCst), 1);

    server.state.release_mode.store(0, Ordering::SeqCst);
    eventually(|| budget.active_per_origin(&server.url) == 0).await;
    issue_all(&second).await;
    assert_eq!(budget.active_per_origin(&server.url), 1);
}

#[tokio::test]
async fn concurrent_admissions_never_exceed_server_capacity() {
    let server = server(2).await;
    let budget = IrohRelayLeaseBudget::with_limit(2);
    let client = rendezvous(&[&server]);
    let mut owners = Vec::new();
    for _ in 0..12 {
        owners.push(owner(&budget, &client).await);
    }
    let mut jobs = tokio::task::JoinSet::new();
    for owner in &owners {
        let owner = owner.clone();
        jobs.spawn(async move { owner.request_tickets().await });
    }
    let mut completed = 0;
    while let Some(result) = jobs.join_next().await {
        match result.unwrap() {
            Ok(_) => completed += 1,
            Err(error) => assert!(error.to_string().contains("retired"), "{error:#}"),
        }
    }
    assert!(
        completed >= 2,
        "the last two admitted owners must receive tickets"
    );
    assert_eq!(budget.active_per_origin(&server.url), 2);
    assert_eq!(server.state.peak.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn dropped_owner_releases_capacity_without_a_strong_registry_cycle() {
    let server = server(1).await;
    let budget = IrohRelayLeaseBudget::with_limit(1);
    let lease = owner(&budget, &rendezvous(&[&server])).await;
    issue_all(&lease).await;
    drop(lease);
    eventually(|| budget.active_per_origin(&server.url) == 0).await;
    assert_eq!(server.state.release_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn refresh_reuses_the_same_slot_and_owner() {
    let server = server(1).await;
    let budget = IrohRelayLeaseBudget::with_limit(1);
    let client = rendezvous(&[&server]);
    let lease = owner(&budget, &client).await;
    issue_all(&lease).await;
    let same = budget
        .lease_for_endpoint(client, lease.endpoint_id())
        .unwrap();
    assert!(Arc::ptr_eq(&lease, &same));
    issue_all(&same).await;
    assert_eq!(budget.active_per_origin(&server.url), 1);
    assert_eq!(server.state.release_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn concurrent_requests_for_the_same_endpoint_share_one_ticket_worker() {
    let server = server(1).await;
    let budget = IrohRelayLeaseBudget::with_limit(1);
    let lease = owner(&budget, &rendezvous(&[&server])).await;
    server.state.issue_blocked.store(true, Ordering::SeqCst);
    let requests = async { tokio::join!(lease.request_tickets(), lease.request_tickets()) };
    let unblock = async {
        server.state.issue_started.notified().await;
        server.state.issue_blocked.store(false, Ordering::SeqCst);
        server.state.issue_continue.notify_one();
    };
    let ((first, second), ()) = tokio::join!(requests, unblock);
    first.unwrap();
    second.unwrap();
    assert_eq!(server.state.issue_count.load(Ordering::SeqCst), 1);
    assert_eq!(budget.active_per_origin(&server.url), 1);
}

#[tokio::test]
async fn cancelled_issuance_accounts_for_a_late_ticket_before_releasing_it() {
    let server = server(1).await;
    let budget = IrohRelayLeaseBudget::with_limit(1);
    let lease = owner(&budget, &rendezvous(&[&server])).await;
    server.state.issue_blocked.store(true, Ordering::SeqCst);
    let request = lease.request_tickets();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), request)
            .await
            .is_err()
    );
    assert_eq!(budget.active_per_origin(&server.url), 1);
    lease.retire("cancelled_route");
    server.state.issue_continue.notify_one();

    eventually(|| budget.active_per_origin(&server.url) == 0).await;
    assert_eq!(server.state.issue_count.load(Ordering::SeqCst), 1);
    assert_eq!(server.state.release_count.load(Ordering::SeqCst), 1);
    assert!(lock(&server.state.leases).is_empty());
}

#[tokio::test]
async fn uncertain_issuance_stays_counted_until_confirmed_release() {
    let server = server(1).await;
    let budget = IrohRelayLeaseBudget::with_limit(1);
    let lease = owner(&budget, &rendezvous(&[&server])).await;
    server.state.issue_failed.store(true, Ordering::SeqCst);
    assert!(lease.request_tickets().await.is_err());
    assert_eq!(budget.active_per_origin(&server.url), 1);
    assert_eq!(lock(&server.state.leases).len(), 1);
    lease.release_now("uncertain_issue").await.unwrap();
    assert_eq!(budget.active_per_origin(&server.url), 0);
}

#[tokio::test]
async fn shared_ticket_failures_preserve_typed_rendezvous_backpressure() {
    let server = server(1).await;
    let budget = IrohRelayLeaseBudget::with_limit(1);
    let lease = owner(&budget, &rendezvous(&[&server])).await;
    server
        .state
        .issue_backpressure
        .store(true, Ordering::SeqCst);
    let error = match lease.request_tickets().await {
        Ok(_) => panic!("ticket issuance should report backpressure"),
        Err(error) => error,
    };
    assert!(transport_sdk::is_rendezvous_backpressure(&error));
}

#[tokio::test]
async fn expired_known_ticket_frees_capacity_without_waiting_for_unreachable_release() {
    let server = server(1).await;
    let budget = IrohRelayLeaseBudget::with_limit(1);
    let client = rendezvous(&[&server]);
    let first = owner(&budget, &client).await;
    server.state.expiry.store(now() - 120, Ordering::SeqCst);
    issue_all(&first).await;
    server.state.release_mode.store(1, Ordering::SeqCst);
    server.state.expiry.store(now() + 3600, Ordering::SeqCst);
    let second = owner(&budget, &client).await;
    tokio::time::timeout(Duration::from_millis(200), second.request_tickets())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(server.state.release_count.load(Ordering::SeqCst), 0);
    assert_eq!(budget.active_per_origin(&server.url), 1);
}

#[tokio::test]
async fn release_is_idempotent_and_an_old_generation_cannot_release_its_replacement() {
    let server = server(1).await;
    let budget = IrohRelayLeaseBudget::with_limit(1);
    let client = rendezvous(&[&server]);
    let first = owner(&budget, &client).await;
    issue_all(&first).await;
    first.release_now("first").await.unwrap();
    let second = budget
        .lease_for_endpoint(client, first.endpoint_id())
        .unwrap();
    issue_all(&second).await;
    first.release_now("late_old_release").await.unwrap();
    assert_eq!(server.state.release_count.load(Ordering::SeqCst), 1);
    assert!(lock(&server.state.leases).contains_key(second.endpoint_id()));
    assert_eq!(budget.active_per_origin(&server.url), 1);
}
