use super::*;
use axum::{
    Json, Router,
    body::Body,
    extract::{
        Path as AxumPath, RawQuery, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{Response, header},
    response::IntoResponse,
    routing::{get, post, put},
};
use futures_util::{Sink, Stream, StreamExt};
use iroh::SecretKey;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, SanType,
};
use std::pin::Pin;
use std::sync::{
    Arc, Barrier,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
use std::task::{Context, Poll};
use tokio::sync::{Mutex, Notify};
use transport_sdk::{
    BufferedTransportResponse as MultiplexBufferedTransportResponse, CandidateKind,
    ConnectionCandidate, DecodedWebSocketMessage, DirectQuicEndpoint, DirectQuicEndpointConfig,
    MultiplexConfig, MultiplexMode, MultiplexedSession, PeerIdentity, RelayHttpHeader,
    RelayTunnelAcceptRequest, RelayTunnelSecurityMode, RelayTunnelSessionKind,
    RelayTunnelSourceSecurityConfig, RelayTunnelTargetSecurityConfig, RelayTunnelTlsIdentity,
    RendezvousClientConfig, RendezvousControlClient, TRANSPORT_PROTOCOL_VERSION, TransportHeader,
    TransportResponseHead, TransportSessionControlMessage, TransportSessionRole,
    TransportStreamKind, WebSocketByteStream, WebSocketMessageCodec,
    perform_transport_server_handshake, read_buffered_transport_request,
    write_buffered_transport_response, write_transport_response_head,
};

struct ConnectionDiagnosticsObserverReset;

static CONNECTION_DIAGNOSTICS_OBSERVER_TEST_LOCK: Mutex<()> = Mutex::const_new(());

impl Drop for ConnectionDiagnosticsObserverReset {
    fn drop(&mut self) {
        set_connection_diagnostics_observer(None);
    }
}

#[test]
fn direct_quic_route_identity_changes_on_relay_token_rotation_without_exposing_tokens() {
    let mut candidate = ConnectionCandidate {
        kind: CandidateKind::DirectQuic,
        endpoint: "iroh://dynamic-node-key".to_string(),
        rtt_ms: None,
        transport_hints: Some(
            transport_sdk::candidates::ConnectionCandidateTransportHints {
                relay_url: Some("https://relay.example".to_string()),
                relay_auth_token: Some("first-sensitive-token".to_string()),
                ..Default::default()
            },
        ),
    };
    let first = direct_quic_route_identity(&candidate);
    candidate
        .transport_hints
        .as_mut()
        .expect("candidate should include hints")
        .relay_auth_token = Some("second-sensitive-token".to_string());
    let second = direct_quic_route_identity(&candidate);

    assert_ne!(first, second);
    assert!(!first.contains("first-sensitive-token"));
    assert!(!second.contains("second-sensitive-token"));
}

#[test]
fn object_url_builder_escapes_segments() {
    let client = IronMeshClient::from_direct_base_url("http://127.0.0.1:18080/");
    let url = client
        .store_key_url("read me.txt")
        .expect("object url should build");
    assert_eq!(
        url.as_str(),
        "http://127.0.0.1:18080/api/v1/store/read%20me.txt"
    );
}

#[test]
fn gallery_map_zoom_request_validation_preserves_fractional_zoom() {
    assert_eq!(gallery_map_zoom_for_request(3.75).unwrap(), 3.75);
    assert_eq!(gallery_map_zoom_for_request(-2.0).unwrap(), 0.0);
    assert_eq!(gallery_map_zoom_for_request(25.0).unwrap(), 20.0);
    assert!(gallery_map_zoom_for_request(f64::NAN).is_err());
}

#[test]
fn client_clones_keep_the_same_connection_runtime_id() {
    let client = IronMeshClient::from_direct_base_url("http://127.0.0.1:18080/");
    let clone = client.clone();
    let rebuilt = IronMeshClient::from_direct_base_url("http://127.0.0.1:18080/");

    assert_eq!(
        client.connection_runtime_id(),
        clone.connection_runtime_id()
    );
    assert_ne!(
        client.connection_runtime_id(),
        rebuilt.connection_runtime_id()
    );
}

#[test]
fn connection_diagnostic_impact_is_explicit_and_clone_specific() {
    let client = IronMeshClient::from_direct_base_url("http://127.0.0.1:18080/");
    let maintenance_client = client
        .clone()
        .with_connection_diagnostic_impact(ClientConnectionDiagnosticImpact::BackgroundMaintenance);

    assert_eq!(
        client.connection_diagnostic_impact(),
        ClientConnectionDiagnosticImpact::UserFacing
    );
    assert_eq!(
        maintenance_client.connection_diagnostic_impact(),
        ClientConnectionDiagnosticImpact::BackgroundMaintenance
    );
    assert!(ClientConnectionDiagnosticImpact::UserFacing.affects_user_facing_connection_status());
    assert!(
        !ClientConnectionDiagnosticImpact::BackgroundMaintenance
            .affects_user_facing_connection_status()
    );
}

#[test]
fn shared_route_diagnostics_retain_each_attempts_impact() {
    let client = IronMeshClient::from_direct_base_url("http://127.0.0.1:18080/");
    let maintenance_client = client
        .clone()
        .with_connection_diagnostic_impact(ClientConnectionDiagnosticImpact::BackgroundMaintenance);
    let foreground_client = client
        .clone()
        .with_connection_diagnostic_impact(ClientConnectionDiagnosticImpact::UserFacing);
    let request_url = client
        .relative_url("/api/v1/cluster/status")
        .expect("relative URL should build");
    let endpoint = client
        .transport_router
        .endpoint(0)
        .expect("direct endpoint should exist");

    let _ = maintenance_client.record_request_failure(
        0,
        &endpoint,
        ClientRequestAttemptContext {
            method: &Method::GET,
            url: &request_url,
            timeout: None,
            started_unix_ms: 1_000,
            session_pool_before: TransportSessionPoolSnapshot::default(),
        },
        "background candidate timed out",
    );
    let _ = foreground_client.record_request_failure(
        0,
        &endpoint,
        ClientRequestAttemptContext {
            method: &Method::GET,
            url: &request_url,
            timeout: None,
            started_unix_ms: 2_000,
            session_pool_before: TransportSessionPoolSnapshot::default(),
        },
        "foreground request timed out",
    );

    let attempts = &foreground_client.connection_diagnostics().endpoints[0].recent_attempts;
    assert_eq!(attempts.len(), 2);
    assert_eq!(
        attempts[0].impact,
        ClientConnectionDiagnosticImpact::BackgroundMaintenance
    );
    assert_eq!(
        attempts[1].impact,
        ClientConnectionDiagnosticImpact::UserFacing
    );
}

#[test]
fn request_result_uses_captured_endpoint_after_route_membership_changes() {
    let client = IronMeshClient::from_direct_base_url("http://127.0.0.1:18080/");
    let request_url = client
        .relative_url("/api/v1/cluster/status")
        .expect("relative URL should build");
    let endpoint = client
        .transport_router
        .endpoint(0)
        .expect("direct endpoint should exist");

    let _ = client.transport_router.reconcile(Vec::new());
    let completed_operation = client.record_request_failure(
        0,
        &endpoint,
        ClientRequestAttemptContext {
            method: &Method::GET,
            url: &request_url,
            timeout: None,
            started_unix_ms: 1_000,
            session_pool_before: TransportSessionPoolSnapshot::default(),
        },
        "route was removed during the request",
    );

    assert_eq!(
        completed_operation.endpoint_locator,
        "http://127.0.0.1:18080"
    );
    assert_eq!(completed_operation.attempt.outcome, "failure");
    assert!(client.connection_diagnostics().endpoints.is_empty());
}

#[test]
fn background_probe_failures_are_scoped_as_maintenance_attempts() {
    let client = IronMeshClient::from_direct_base_url("http://127.0.0.1:18080/");

    client
        .transport_router
        .record_background_probe_failure(0, "candidate probe timed out");

    let attempts = &client.connection_diagnostics().endpoints[0].recent_attempts;
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].method, "PROBE");
    assert_eq!(attempts[0].outcome, "failure");
    assert_eq!(
        attempts[0].impact,
        ClientConnectionDiagnosticImpact::BackgroundMaintenance
    );
}

#[test]
fn connection_diagnostics_timestamp_does_not_precede_last_route_use() {
    let client = IronMeshClient::from_direct_base_url("http://127.0.0.1:18080/");
    let endpoint = client
        .transport_router
        .endpoint(0)
        .expect("the route should exist");
    client
        .transport_router
        .record_route_used(0, &endpoint, unix_ts_ms());

    let diagnostics = client.connection_diagnostics();
    let last_used_unix_ms = diagnostics.endpoints[0]
        .last_used_unix_ms
        .expect("route use should be recorded");

    assert!(diagnostics.generated_at_unix_ms >= last_used_unix_ms);
}

#[tokio::test]
async fn background_probe_refresh_publishes_maintenance_diagnostics() {
    let _observer_test_guard = CONNECTION_DIAGNOSTICS_OBSERVER_TEST_LOCK.lock().await;
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").expect("temporary listener should bind");
    let endpoint = format!(
        "http://{}",
        listener.local_addr().expect("listener should bind")
    );
    drop(listener);

    let connection_name = "background-probe-diagnostics-test";
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured_events = events.clone();
    set_connection_diagnostics_observer(Some(Arc::new(move |event| {
        if event.connection_name.as_deref() == Some(connection_name) {
            captured_events
                .lock()
                .expect("captured events lock should not be poisoned")
                .push(event);
        }
    })));
    let _observer_reset = ConnectionDiagnosticsObserverReset;

    let client = IronMeshClient::from_direct_base_url(endpoint)
        .with_connection_name(connection_name)
        .with_connection_diagnostic_impact(ClientConnectionDiagnosticImpact::UserFacing);
    client.refresh_connection_route_snapshot().await;

    let events = events
        .lock()
        .expect("captured events lock should not be poisoned");
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].impact,
        ClientConnectionDiagnosticImpact::BackgroundMaintenance
    );
    let attempts = &events[0].diagnostics.endpoints[0].recent_attempts;
    assert!(attempts.iter().any(|attempt| {
        attempt.method == "PROBE"
            && attempt.outcome == "failure"
            && attempt.impact == ClientConnectionDiagnosticImpact::BackgroundMaintenance
    }));
}

#[test]
fn route_reconciliation_preserves_static_state_and_retires_dynamic_routes() {
    let static_client = IronMeshClient::from_direct_base_url("http://127.0.0.1:18080/");
    let original_endpoint = static_client
        .transport_router
        .endpoint(0)
        .expect("static endpoint should exist");
    let dynamic_quic = IronMeshClient::from_direct_quic_candidate_with_target_node_id(
        ConnectionCandidate {
            kind: CandidateKind::DirectQuic,
            endpoint: "iroh://dynamic-node-key".to_string(),
            rtt_ms: None,
            transport_hints: None,
        },
        Some(NodeId::new_v4()),
    );
    let refreshed = IronMeshClient::combine(vec![
        IronMeshClient::from_direct_base_url("http://127.0.0.1:18080/"),
        dynamic_quic,
    ])
    .expect("refreshed routes should combine");

    let runtime_id = static_client.connection_runtime_id();
    let (added, removed, retained) = static_client.reconcile_transport_membership(&refreshed);
    assert_eq!((added, removed, retained), (1, 0, 1));
    assert_eq!(static_client.connection_runtime_id(), runtime_id);
    let retained_endpoint = static_client
        .transport_router
        .endpoint(0)
        .expect("static endpoint should remain");
    assert!(Arc::ptr_eq(
        &original_endpoint.state,
        &retained_endpoint.state
    ));
    assert!(
        static_client
            .connection_route_snapshot()
            .endpoints
            .iter()
            .any(|route| route.path_kind == TransportPathKind::DirectQuic)
    );

    let static_only = IronMeshClient::from_direct_base_url("http://127.0.0.1:18080/");
    let (added, removed, retained) = static_client.reconcile_transport_membership(&static_only);
    assert_eq!((added, removed, retained), (0, 1, 1));
    assert_eq!(static_client.connection_route_snapshot().endpoints.len(), 1);
}

#[test]
fn reintroduced_failed_route_retains_backoff_without_reusing_transport() {
    let target_node_id = NodeId::new_v4();
    let static_route = || IronMeshClient::from_direct_base_url("http://127.0.0.1:18080/");
    let dynamic_route = || {
        IronMeshClient::from_direct_quic_candidate_with_target_node_id(
            ConnectionCandidate {
                kind: CandidateKind::DirectQuic,
                endpoint: "iroh://retired-dynamic-node".to_string(),
                rtt_ms: None,
                transport_hints: None,
            },
            Some(target_node_id),
        )
    };
    let client = IronMeshClient::combine(vec![static_route(), dynamic_route()])
        .expect("initial routes should combine");
    let retired_endpoint = client
        .transport_router
        .endpoint(1)
        .expect("dynamic endpoint should exist");
    record_endpoint_failure_sample(
        &mut lock_endpoint_state(&retired_endpoint.state),
        "simulated route failure",
        true,
    );
    let failed_snapshot = client.connection_route_snapshot().endpoints[1].clone();

    client.reconcile_transport_membership(&static_route());
    let replacement = IronMeshClient::combine(vec![static_route(), dynamic_route()])
        .expect("replacement routes should combine");
    client.reconcile_transport_membership(&replacement);

    let reintroduced_endpoint = client
        .transport_router
        .endpoint(1)
        .expect("dynamic endpoint should be reintroduced");
    assert!(
        !Arc::ptr_eq(&retired_endpoint.state, &reintroduced_endpoint.state),
        "a tombstone must not retain the old endpoint or session pool"
    );
    let reintroduced_snapshot = &client.connection_route_snapshot().endpoints[1];
    assert_eq!(reintroduced_snapshot.consecutive_failures, 1);
    assert_eq!(
        reintroduced_snapshot.circuit_open_until_unix_ms,
        failed_snapshot.circuit_open_until_unix_ms
    );
    assert!(
        client
            .transport_router
            .claim_background_probe_candidates()
            .is_empty(),
        "the next route update must not immediately reclaim a cooling route"
    );
}

#[test]
fn retired_failure_tombstones_are_capped_by_oldest_expiry() {
    let mut retired_failure_states = HashMap::new();
    for index in 0..CLIENT_ROUTE_RETIRED_FAILURE_STATE_LIMIT + 5 {
        let state = ClientEndpointState {
            consecutive_failures: 1,
            total_failures: 1,
            ..ClientEndpointState::default()
        };
        let retired = RetiredRouteFailureState::capture(&state, index as u64)
            .expect("failed state should produce a tombstone");
        retired_failure_states.insert(RouteId::new(format!("route-{index}")), retired);
    }

    prune_retired_failure_states(&mut retired_failure_states);

    assert_eq!(
        retired_failure_states.len(),
        CLIENT_ROUTE_RETIRED_FAILURE_STATE_LIMIT
    );
    for index in 0..5 {
        assert!(!retired_failure_states.contains_key(&RouteId::new(format!("route-{index}"))));
    }
    assert!(retired_failure_states.contains_key(&RouteId::new(format!(
        "route-{}",
        CLIENT_ROUTE_RETIRED_FAILURE_STATE_LIMIT + 4
    ))));
}

#[test]
fn first_background_probe_gets_startup_budget_but_retries_keep_short_budget() {
    let mut state = ClientEndpointState::default();
    assert_eq!(
        background_probe_timeout(&state),
        CLIENT_ROUTE_INITIAL_BACKGROUND_PROBE_TIMEOUT
    );

    state.last_measurement_unix_ms = Some(unix_ts_ms());
    state.consecutive_failures = 1;
    assert_eq!(
        background_probe_timeout(&state),
        CLIENT_ROUTE_BACKGROUND_PROBE_TIMEOUT
    );
}

#[test]
fn route_failure_backoff_grows_exponentially_and_caps() {
    assert_eq!(endpoint_failure_backoff_ms(1), 1_500);
    assert_eq!(endpoint_failure_backoff_ms(2), 3_000);
    assert_eq!(endpoint_failure_backoff_ms(3), 6_000);
    assert_eq!(endpoint_failure_backoff_ms(4), 12_000);
    assert_eq!(endpoint_failure_backoff_ms(5), 24_000);
    assert_eq!(endpoint_failure_backoff_ms(6), 30_000);
    assert_eq!(endpoint_failure_backoff_ms(u32::MAX), 30_000);
}

#[test]
fn mobile_background_policy_waits_for_circuit_and_probe_interval_before_retrying() {
    let policy = ClientRouteMaintenancePolicy::mobile_background();
    let initial_probe_at_unix_ms = 10_000;
    let mut state = ClientEndpointState {
        consecutive_failures: 1,
        last_measurement_unix_ms: Some(initial_probe_at_unix_ms),
        circuit: RouteCircuitState::OpenUntil(initial_probe_at_unix_ms),
        ..ClientEndpointState::default()
    };

    assert!(background_probe_due(
        &state,
        initial_probe_at_unix_ms,
        policy
    ));

    state.last_background_probe_unix_ms = Some(initial_probe_at_unix_ms);
    let probe_interval_ms = duration_to_u64_ms(policy.background_probe_min_interval)
        .expect("mobile background probe interval should fit into milliseconds");
    assert!(!background_probe_due(
        &state,
        initial_probe_at_unix_ms + probe_interval_ms - 1,
        policy
    ));
    assert!(background_probe_due(
        &state,
        initial_probe_at_unix_ms + probe_interval_ms,
        policy
    ));
}

#[test]
fn mobile_background_policy_claims_one_candidate_per_batch() {
    let client = IronMeshClient::combine(vec![
        IronMeshClient::from_direct_base_url("http://127.0.0.1:18080/"),
        IronMeshClient::from_direct_base_url("http://127.0.0.1:18081/"),
        IronMeshClient::from_direct_base_url("http://127.0.0.1:18082/"),
    ])
    .expect("routes should combine")
    .with_route_maintenance_policy(ClientRouteMaintenancePolicy::mobile_background());

    let first_batch = client.transport_router.claim_background_probe_candidates();
    let second_batch = client.transport_router.claim_background_probe_candidates();

    assert_eq!(first_batch.len(), 1);
    assert_eq!(first_batch[0].sampling.warmup_count, 0);
    assert_eq!(first_batch[0].sampling.sample_count, 1);
    assert!(second_batch.is_empty());
}

#[test]
fn route_state_transitions_keep_validation_circuit_and_probe_orthogonal() {
    let mut state = ClientEndpointState::default();
    assert_eq!(state.validation, RouteValidationState::Probation);
    assert_eq!(state.circuit, RouteCircuitState::Closed);
    assert_eq!(state.probe, RouteProbeState::Idle);

    assert!(state.begin_probe(123));
    assert!(
        !state.begin_probe(124),
        "a probe claim must be single-flight"
    );
    state.record_success(8.0, 0, true);
    assert_eq!(state.validation, RouteValidationState::Validated);
    assert_eq!(state.circuit, RouteCircuitState::Closed);
    assert_eq!(state.probe, RouteProbeState::Idle);

    state.record_failure("transport failed", false);
    assert_eq!(state.validation, RouteValidationState::Validated);
    assert!(matches!(state.circuit, RouteCircuitState::OpenUntil(_)));
    assert_eq!(state.probe, RouteProbeState::Idle);
}

#[test]
fn stable_route_plan_survives_registry_reordering() {
    let route_a = || IronMeshClient::from_direct_base_url("http://127.0.0.1:18100/");
    let route_b = || IronMeshClient::from_direct_base_url("http://127.0.0.1:18101/");
    let client =
        IronMeshClient::combine(vec![route_a(), route_b()]).expect("routes should combine");
    let planned = client.transport_router.foreground_route_ids();
    let planned_primary = planned[0].clone();

    let reordered =
        IronMeshClient::combine(vec![route_b(), route_a()]).expect("routes should reorder");
    client.reconcile_transport_membership(&reordered);

    let mut executor = RequestExecutor::new(planned);
    let selected = executor
        .next_route(|route_id| client.transport_router.route_admission(route_id))
        .expect("the stable primary should remain selectable");
    assert_eq!(selected, planned_primary);
    let (current_index, endpoint) = client
        .transport_router
        .endpoint_by_id(&selected)
        .expect("the stable route should resolve after reconciliation");
    assert_eq!(current_index, 1);
    assert!(endpoint.descriptor.locator.contains(":18100"));
}

#[test]
fn route_affinity_keeps_exact_route_ahead_of_ranked_same_node_fallbacks() {
    let target_node_id = NodeId::new_v4();
    let client = IronMeshClient::combine(vec![
        IronMeshClient::from_direct_http_client_with_target_node_id_and_ca_pem(
            "http://127.0.0.1:18102/".to_string(),
            HttpClient::new(),
            Some(target_node_id),
            None,
            None,
        ),
        IronMeshClient::from_direct_http_client_with_target_node_id_and_ca_pem(
            "http://127.0.0.1:18103/".to_string(),
            HttpClient::new(),
            Some(target_node_id),
            None,
            None,
        ),
    ])
    .expect("routes should combine");
    let preferred = client
        .transport_router
        .endpoint(0)
        .expect("preferred route should exist");
    let fallback = client
        .transport_router
        .endpoint(1)
        .expect("fallback route should exist");
    {
        let mut preferred_state = lock_endpoint_state(&preferred.state);
        preferred_state.validation = RouteValidationState::Validated;
        preferred_state.ewma_latency_ms = Some(500.0);
    }
    {
        let mut fallback_state = lock_endpoint_state(&fallback.state);
        fallback_state.validation = RouteValidationState::Validated;
        fallback_state.ewma_latency_ms = Some(1.0);
    }

    let ranked = client.transport_router.foreground_route_ids();
    assert_eq!(ranked[0], fallback.descriptor.route_id);
    let affinity = NodeRouteAffinity::from_endpoint(&preferred);
    let with_affinity = client.route_ids_for_affinity(Some(&affinity));

    assert_eq!(with_affinity[0], preferred.descriptor.route_id);
    assert_eq!(with_affinity[1], fallback.descriptor.route_id);
}

#[test]
fn preflight_failure_is_recorded_against_captured_route_after_reordering() {
    let route_a = || IronMeshClient::from_direct_base_url("http://127.0.0.1:18105/");
    let route_b = || IronMeshClient::from_direct_base_url("http://127.0.0.1:18106/");
    let client =
        IronMeshClient::combine(vec![route_a(), route_b()]).expect("routes should combine");
    let mut execution =
        ForegroundRequestExecutor::new(&client, client.transport_router.foreground_route_ids());
    let (_original_index, captured_endpoint) = execution.next().expect("primary should resolve");

    let reordered =
        IronMeshClient::combine(vec![route_b(), route_a()]).expect("routes should reorder");
    client.reconcile_transport_membership(&reordered);
    execution.record_preflight_failure(&captured_endpoint, anyhow!("rewrite failed"));

    let endpoints = client.transport_router.endpoints_snapshot();
    let route_a_failures = endpoints
        .iter()
        .find(|endpoint| endpoint.descriptor.locator.contains(":18105"))
        .map(|endpoint| lock_endpoint_state(&endpoint.state).total_failures)
        .expect("route A should remain registered");
    let route_b_failures = endpoints
        .iter()
        .find(|endpoint| endpoint.descriptor.locator.contains(":18106"))
        .map(|endpoint| lock_endpoint_state(&endpoint.state).total_failures)
        .expect("route B should remain registered");
    assert_eq!(route_a_failures, 1);
    assert_eq!(route_b_failures, 0);
}

#[test]
fn route_use_is_recorded_against_captured_route_after_reordering() {
    let route_a = || IronMeshClient::from_direct_base_url("http://127.0.0.1:18107/");
    let route_b = || IronMeshClient::from_direct_base_url("http://127.0.0.1:18108/");
    let client =
        IronMeshClient::combine(vec![route_a(), route_b()]).expect("routes should combine");
    let mut execution =
        ForegroundRequestExecutor::new(&client, client.transport_router.foreground_route_ids());
    let (original_index, captured_endpoint) = execution.next().expect("primary should resolve");

    let reordered =
        IronMeshClient::combine(vec![route_b(), route_a()]).expect("routes should reorder");
    client.reconcile_transport_membership(&reordered);
    client
        .transport_router
        .record_route_used(original_index, &captured_endpoint, unix_ts_ms());

    let endpoints = client.transport_router.endpoints_snapshot();
    let route_a_last_used = endpoints
        .iter()
        .find(|endpoint| endpoint.descriptor.locator.contains(":18107"))
        .and_then(|endpoint| lock_endpoint_state(&endpoint.state).last_used_unix_ms);
    let route_b_last_used = endpoints
        .iter()
        .find(|endpoint| endpoint.descriptor.locator.contains(":18108"))
        .and_then(|endpoint| lock_endpoint_state(&endpoint.state).last_used_unix_ms);
    assert!(route_a_last_used.is_some());
    assert_eq!(route_b_last_used, None);
}

#[test]
fn concurrent_plan_avoids_route_after_another_request_reports_failure() {
    let client = IronMeshClient::combine(vec![
        IronMeshClient::from_direct_base_url("http://127.0.0.1:18110/"),
        IronMeshClient::from_direct_base_url("http://127.0.0.1:18111/"),
    ])
    .expect("routes should combine");
    let plan = client.transport_router.foreground_route_ids();
    let primary = plan[0].clone();
    let backup = plan[1].clone();
    let mut first_request = RequestExecutor::new(plan.clone());
    let mut concurrent_request = RequestExecutor::new(plan);

    assert_eq!(
        first_request.next_route(|route_id| client.transport_router.route_admission(route_id)),
        Some(primary.clone())
    );
    let (primary_index, _) = client
        .transport_router
        .endpoint_by_id(&primary)
        .expect("primary should exist");
    client
        .transport_router
        .record_failure(primary_index, "concurrent timeout");

    assert_eq!(
        concurrent_request.next_route(|route_id| client.transport_router.route_admission(route_id)),
        Some(backup),
        "admission is resolved lazily, so the fresh circuit state must win"
    );
}

#[test]
fn availability_target_stops_warming_additional_probation_routes() {
    let client = IronMeshClient::combine(vec![
        IronMeshClient::from_direct_base_url("http://127.0.0.1:18120/"),
        IronMeshClient::from_direct_base_url("http://127.0.0.1:18121/"),
        IronMeshClient::from_direct_base_url("http://127.0.0.1:18122/"),
        IronMeshClient::from_direct_base_url("http://127.0.0.1:18123/"),
    ])
    .expect("routes should combine")
    .with_route_maintenance_policy(ClientRouteMaintenancePolicy {
        background_probe_batch_min_interval: Duration::ZERO,
        ..ClientRouteMaintenancePolicy::mobile_background()
    });

    for _ in 0..2 {
        let candidates = client.transport_router.claim_background_probe_candidates();
        assert_eq!(candidates.len(), 1);
        client
            .transport_router
            .record_background_probe_candidate_successes(&candidates[0], &[5.0]);
    }

    assert!(
        client
            .transport_router
            .claim_background_probe_candidates()
            .is_empty(),
        "one primary plus one validated backup satisfies the mobile target"
    );
    assert_eq!(
        client
            .transport_router
            .endpoints_snapshot()
            .iter()
            .filter(|endpoint| {
                lock_endpoint_state(&endpoint.state).validation == RouteValidationState::Validated
            })
            .count(),
        2
    );
}

#[test]
fn completed_probe_does_not_update_replaced_endpoint_with_same_route_key() {
    let static_route = || IronMeshClient::from_direct_base_url("http://127.0.0.1:18080/");
    let dynamic_route = || IronMeshClient::from_direct_base_url("http://127.0.0.1:18081/");
    let client = IronMeshClient::combine(vec![static_route(), dynamic_route()])
        .expect("initial routes should combine");
    let candidate = client
        .transport_router
        .claim_background_probe_candidates()
        .into_iter()
        .find(|candidate| candidate.endpoint.descriptor.locator.contains(":18081"))
        .expect("dynamic route should be claimed");

    client.reconcile_transport_membership(&static_route());
    let replacement = IronMeshClient::combine(vec![static_route(), dynamic_route()])
        .expect("replacement routes should combine");
    client.reconcile_transport_membership(&replacement);
    let recorded = client
        .transport_router
        .record_background_probe_candidate_successes(&candidate, &[5.0]);
    assert!(
        !recorded,
        "a replaced endpoint must reject the stale result"
    );

    let snapshot = client.connection_route_snapshot();
    let replacement_snapshot = snapshot
        .endpoints
        .into_iter()
        .find(|endpoint| endpoint.locator.contains(":18081"))
        .expect("replacement route should exist");
    assert_eq!(replacement_snapshot.total_successes, 0);
    assert!(replacement_snapshot.last_measurement_unix_ms.is_none());
}

#[test]
fn route_reconciliation_updates_shared_request_identity() {
    let cluster_id = Uuid::now_v7();
    let original_identity = ClientIdentityMaterial::generate(cluster_id, None, None)
        .expect("original identity should generate");
    let renewed_identity = ClientIdentityMaterial::generate(cluster_id, None, None)
        .expect("renewed identity should generate");
    let client = IronMeshClient::from_direct_base_url("http://127.0.0.1:18080/")
        .with_client_identity(original_identity);
    let shared_clone = client.clone();
    let refreshed = IronMeshClient::from_direct_base_url("http://127.0.0.1:18080/")
        .with_client_identity(renewed_identity.clone());

    client.reconcile_transport_membership(&refreshed);

    let ClientRequestAuth::SignedIdentity(adopted_identity) = shared_clone.auth_snapshot() else {
        panic!("renewed signed identity should remain configured");
    };
    assert_eq!(adopted_identity.device_id, renewed_identity.device_id);
}

#[tokio::test]
async fn newly_discovered_direct_quic_is_ranked_first_and_probed_while_on_probation() {
    let static_client = IronMeshClient::from_direct_base_url("http://127.0.0.1:18080/");
    static_client
        .transport_router
        .endpoint(0)
        .expect("static endpoint should exist")
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .last_used_unix_ms = Some(unix_ts_ms());
    let dynamic_quic = IronMeshClient::from_direct_quic_candidate_with_target_node_id(
        ConnectionCandidate {
            kind: CandidateKind::DirectQuic,
            endpoint: "iroh://dynamic-node-key".to_string(),
            rtt_ms: None,
            transport_hints: None,
        },
        Some(NodeId::new_v4()),
    );
    let refreshed = IronMeshClient::combine(vec![
        dynamic_quic,
        IronMeshClient::from_direct_base_url("http://127.0.0.1:18080/"),
    ])
    .expect("refreshed routes should combine");

    static_client.reconcile_transport_membership(&refreshed);
    let before_probe = static_client.connection_route_snapshot();
    let direct_quic = before_probe
        .endpoints
        .iter()
        .find(|route| route.path_kind == TransportPathKind::DirectQuic)
        .expect("dynamic Direct QUIC route should exist");
    assert_eq!(
        before_probe.ranked_indices.first(),
        Some(&direct_quic.index),
        "new Direct QUIC should receive the first exploration attempt"
    );
    assert!(direct_quic.last_used_unix_ms.is_none());

    assert!(static_client.spawn_due_connection_route_refresh());
    let after_probe_scheduled = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let snapshot = static_client.connection_route_snapshot();
            if snapshot
                .endpoints
                .iter()
                .find(|route| route.path_kind == TransportPathKind::DirectQuic)
                .is_some_and(|route| route.last_background_probe_unix_ms.is_some())
            {
                break snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the shared supervisor should claim the due route promptly");
    let direct_quic = after_probe_scheduled
        .endpoints
        .iter()
        .find(|route| route.path_kind == TransportPathKind::DirectQuic)
        .expect("dynamic Direct QUIC route should remain");
    assert!(direct_quic.last_background_probe_unix_ms.is_some());
}

#[test]
fn exhausted_route_set_notifies_managed_refresh_once() {
    let client = IronMeshClient::combine(vec![
        IronMeshClient::from_direct_base_url("http://127.0.0.1:18080/"),
        IronMeshClient::from_direct_base_url("http://127.0.0.1:18081/"),
    ])
    .expect("combined routes should build");
    let refreshes = Arc::new(AtomicUsize::new(0));
    let observed_refreshes = refreshes.clone();
    client.set_transport_failure_refresh_observer(Some(Arc::new(move || {
        observed_refreshes.fetch_add(1, Ordering::SeqCst);
    })));

    client
        .transport_router
        .record_failure(0, "first route is unavailable");
    assert_eq!(refreshes.load(Ordering::SeqCst), 0);

    client
        .transport_router
        .record_failure(1, "last route is unavailable");
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);

    client
        .transport_router
        .record_failure(0, "already exhausted");
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
}

#[test]
fn normalize_client_api_path_prefixes_known_public_routes() {
    assert_eq!(
        normalize_client_api_path("/cluster/status").as_ref(),
        "/api/v1/cluster/status"
    );
    assert_eq!(
        normalize_client_api_path("/api/v1/cluster/status").as_ref(),
        "/api/v1/cluster/status"
    );
    assert_eq!(
        normalize_client_api_path("/media/thumbnail?key=gallery%2Fcat.png").as_ref(),
        "/api/v1/media/thumbnail?key=gallery%2Fcat.png"
    );
    assert_eq!(
        normalize_client_api_path("/maps/config").as_ref(),
        "/api/v1/maps/config"
    );
    assert_eq!(
        normalize_client_api_path("/web-services").as_ref(),
        "/api/v1/web-services"
    );
}

#[test]
fn web_service_summary_accepts_node_and_legacy_wire_formats() {
    let node_id = Uuid::now_v7();
    for node_key in ["nodeId", "node_id"] {
        let mut payload = serde_json::json!({
            "id": "home-nas",
            "name": "Home NAS",
            "description": "Private storage",
        });
        payload
            .as_object_mut()
            .unwrap()
            .insert(node_key.to_string(), serde_json::json!(node_id));
        let summary: WebServiceSummary = serde_json::from_value(payload).unwrap();
        assert_eq!(summary.node_id, node_id);
    }
}

#[test]
fn normalize_connection_name_preserves_readable_role_segments() {
    assert_eq!(
        normalize_connection_name(" Windows Cfapi / Upload Worker #1 ").as_deref(),
        Some("windows-cfapi-/-upload-worker-1")
    );
    assert_eq!(normalize_connection_name("   "), None);
}

#[test]
fn reset_timing_measurement_clears_attempts_and_uses_session_pool_baseline() {
    let mut state = ClientEndpointState {
        recent_attempts: vec![ClientConnectionAttempt {
            method: "GET".to_string(),
            ..ClientConnectionAttempt::default()
        }],
        total_successes: 11,
        total_failures: 3,
        ..ClientEndpointState::default()
    };
    let baseline = TransportSessionPoolSnapshot {
        connect_count: 4,
        reuse_count: 9,
        reset_count: 2,
        connect_duration_us: 700,
        relay_pairing_duration_us: 300,
    };

    reset_endpoint_timing_measurement(&mut state, baseline);

    assert!(state.recent_attempts.is_empty());
    assert_eq!(state.timing_session_pool_baseline, baseline);
    assert_eq!(state.total_successes, 11);
    assert_eq!(state.total_failures, 3);
    assert_eq!(
        transport_session_pool_delta(
            TransportSessionPoolSnapshot {
                connect_count: 5,
                reuse_count: 12,
                reset_count: 3,
                connect_duration_us: 900,
                relay_pairing_duration_us: 360,
            },
            state.timing_session_pool_baseline,
        ),
        TransportSessionPoolSnapshot {
            connect_count: 1,
            reuse_count: 3,
            reset_count: 1,
            connect_duration_us: 200,
            relay_pairing_duration_us: 60,
        },
    );
}

#[test]
fn route_score_strongly_prefers_direct_over_relay_without_last_used_credit() {
    let state = ClientEndpointState {
        ewma_latency_ms: Some(100.0),
        ..ClientEndpointState::default()
    };
    let direct = ClientEndpointDescriptor {
        route_id: RouteId::new("direct-route"),
        path_kind: ClientEndpointPathKind::Direct,
        transport_path_kind: TransportPathKind::DirectHttps,
        locator: "https://direct.example".to_string(),
        bootstrap_rank: 0,
        target_node_hostname: None,
        node_connection_priority: 0,
    };
    let relay = ClientEndpointDescriptor {
        route_id: RouteId::new("relay-route"),
        path_kind: ClientEndpointPathKind::Relay,
        transport_path_kind: TransportPathKind::RelayTunnel,
        locator: "relay://node@example".to_string(),
        bootstrap_rank: 0,
        target_node_hostname: None,
        node_connection_priority: 0,
    };
    let direct_quic = ClientEndpointDescriptor {
        route_id: RouteId::new("direct-quic-route"),
        path_kind: ClientEndpointPathKind::Direct,
        transport_path_kind: TransportPathKind::DirectQuic,
        locator: "iroh://direct-quic".to_string(),
        bootstrap_rank: 0,
        target_node_hostname: None,
        node_connection_priority: 0,
    };

    assert_eq!(endpoint_score(&direct, &state), 100.0);
    assert_eq!(endpoint_score(&relay, &state), 600.0);
    assert_eq!(endpoint_score(&direct_quic, &state), 0.0);
}

#[test]
fn route_score_prefers_higher_priority_server_nodes() {
    let state = ClientEndpointState {
        ewma_latency_ms: Some(100.0),
        ..ClientEndpointState::default()
    };
    let preferred = ClientEndpointDescriptor {
        route_id: RouteId::new("preferred-node"),
        path_kind: ClientEndpointPathKind::Direct,
        transport_path_kind: TransportPathKind::DirectHttps,
        locator: "https://preferred.example".to_string(),
        bootstrap_rank: 0,
        target_node_hostname: None,
        node_connection_priority: 5,
    };
    let neutral = ClientEndpointDescriptor {
        node_connection_priority: 0,
        ..preferred.clone()
    };

    assert!(endpoint_score(&preferred, &state) < endpoint_score(&neutral, &state));
    assert_eq!(endpoint_score(&preferred, &state), -25.0);
}

#[test]
fn route_latency_excludes_server_work_and_cold_session_setup() {
    assert_eq!(
        route_latency_duration_us(30_250_000, Some(50_000), 30_000_000),
        200_000
    );
    assert_eq!(
        route_latency_duration_us(30_200_000, None, 30_000_000),
        200_000
    );
}

#[test]
fn transport_stream_kind_classification_accepts_versioned_public_routes() {
    assert_eq!(
        transport_stream_kind_for_path("/api/v1/health"),
        TransportStreamKind::Diagnostics
    );
    assert_eq!(
        transport_stream_kind_for_path("/api/v1/diagnostics/latency"),
        TransportStreamKind::Diagnostics
    );
    assert_eq!(
        transport_stream_kind_for_path("/api/v1/cluster/status"),
        TransportStreamKind::Rpc
    );
}

#[test]
fn normalize_server_base_url_adds_scheme_and_trailing_slash() {
    let normalized = normalize_server_base_url("127.0.0.1:18080").expect("url should be valid");
    assert_eq!(normalized.as_str(), "http://127.0.0.1:18080/");
}

#[test]
fn snapshot_conversion_maps_prefix_and_keys() {
    let snapshot = snapshot_from_store_index_entries(vec![
        StoreIndexEntry {
            path: "docs/".to_string(),
            entry_type: "prefix".to_string(),
            labels: Vec::new(),
            labels_resolved: false,
            version: None,
            content_hash: None,
            size_bytes: None,
            modified_at_unix: None,
            content_fingerprint: None,
            media: None,
        },
        StoreIndexEntry {
            path: "docs/readme.txt".to_string(),
            entry_type: "key".to_string(),
            labels: Vec::new(),
            labels_resolved: false,
            version: None,
            content_hash: None,
            size_bytes: Some(42),
            modified_at_unix: Some(1_723_456_789),
            content_fingerprint: Some("cfp-readme".to_string()),
            media: Some(StoreIndexMedia {
                status: "ready".to_string(),
                content_fingerprint: "cfp-readme".to_string(),
                media_type: Some("image".to_string()),
                mime_type: Some("image/jpeg".to_string()),
                width: Some(4_032),
                height: Some(3_024),
                orientation: Some(6),
                taken_at_unix: Some(1_700_000_000),
                date_encoded_unix: None,
                duration_millis: None,
                frame_rate_millihertz: None,
                total_bitrate_bps: None,
                codec_name: None,
                codec_fourcc: None,
                gps: Some(StoreIndexGps {
                    latitude: 47.3769,
                    longitude: 8.5417,
                }),
                photo: Some(StoreIndexPhoto {
                    camera_manufacturer: Some("Contoso".to_string()),
                    camera_model: Some("Camera One".to_string()),
                    lens_manufacturer: None,
                    lens_model: Some("Prime 35".to_string()),
                    iso_speed: Some(200),
                    exposure_time_seconds: Some(0.008),
                    f_number: Some(2.8),
                    focal_length_mm: Some(35.0),
                    flash: Some(0),
                    white_balance: Some(1),
                }),
                thumbnail: None,
                error: None,
            }),
        },
    ]);

    assert_eq!(snapshot.local.len(), 0);
    assert_eq!(snapshot.remote.len(), 2);
    assert_eq!(snapshot.remote[0], NamespaceEntry::directory("docs"));
    assert_eq!(snapshot.remote[1].path, "docs/readme.txt");
    assert_eq!(snapshot.remote[1].version.as_deref(), Some("server-head"));
    assert_eq!(
        snapshot.remote[1].content_hash.as_deref(),
        Some("server-head:docs/readme.txt")
    );
    assert_eq!(
        snapshot.remote[1].content_fingerprint.as_deref(),
        Some("cfp-readme")
    );
    assert_eq!(snapshot.remote[1].size_bytes, Some(42));
    assert_eq!(snapshot.remote[1].modified_at_unix, Some(1_723_456_789));
    let media = snapshot.remote[1]
        .media
        .as_ref()
        .expect("media metadata should survive snapshot conversion");
    assert_eq!(media.mime_type.as_deref(), Some("image/jpeg"));
    assert_eq!(media.orientation, Some(6));
    assert_eq!(media.gps.as_ref().map(|gps| gps.latitude), Some(47.3769));
    assert_eq!(
        media
            .photo
            .as_ref()
            .and_then(|photo| photo.camera_model.as_deref()),
        Some("Camera One")
    );
}

#[test]
fn ensure_missing_folder_markers_adds_nested_parents() {
    let mut entries = vec![StoreIndexEntry {
        path: "a/b/c.txt".to_string(),
        entry_type: "key".to_string(),
        labels: Vec::new(),
        labels_resolved: false,
        version: None,
        content_hash: None,
        size_bytes: Some(7),
        modified_at_unix: None,
        content_fingerprint: None,
        media: None,
    }];

    ensure_missing_folder_markers(&mut entries, "");

    let paths = entries
        .into_iter()
        .map(|entry| entry.path)
        .collect::<Vec<_>>();
    assert_eq!(paths, vec!["a/", "a/b/", "a/b/c.txt"]);
}

#[test]
fn ensure_missing_folder_markers_keeps_existing_markers_unique() {
    let mut entries = vec![
        StoreIndexEntry {
            path: "docs/".to_string(),
            entry_type: "prefix".to_string(),
            labels: Vec::new(),
            labels_resolved: false,
            version: None,
            content_hash: None,
            size_bytes: None,
            modified_at_unix: None,
            content_fingerprint: None,
            media: None,
        },
        StoreIndexEntry {
            path: "docs/guides/readme.md".to_string(),
            entry_type: "key".to_string(),
            labels: Vec::new(),
            labels_resolved: false,
            version: None,
            content_hash: None,
            size_bytes: Some(11),
            modified_at_unix: None,
            content_fingerprint: None,
            media: None,
        },
    ];

    ensure_missing_folder_markers(&mut entries, "");

    let paths = entries
        .into_iter()
        .map(|entry| entry.path)
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        vec!["docs/", "docs/guides/", "docs/guides/readme.md"]
    );
}

#[test]
fn ensure_missing_folder_markers_stays_within_the_requested_prefix() {
    let mut entries = vec![StoreIndexEntry {
        path: "devices/Oppo-uli/Fotos/image.jpg".to_string(),
        entry_type: "key".to_string(),
        labels: Vec::new(),
        labels_resolved: false,
        version: None,
        content_hash: None,
        size_bytes: Some(7),
        modified_at_unix: None,
        content_fingerprint: None,
        media: None,
    }];

    ensure_missing_folder_markers(&mut entries, "devices/Oppo-uli/");

    let paths = entries
        .into_iter()
        .map(|entry| entry.path)
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        vec![
            "devices/Oppo-uli/Fotos/",
            "devices/Oppo-uli/Fotos/image.jpg",
        ]
    );
}

#[test]
fn folder_marker_synthesis_preserves_server_file_order_on_the_first_page() {
    let mut response = StoreIndexResponse {
        prefix: String::new(),
        depth: 64,
        entry_count: 3,
        total_entry_count: 64,
        offset: 0,
        limit: Some(32),
        has_more: true,
        next_cursor: None,
        sync_token: None,
        consistency_token: None,
        media_summary: StoreIndexMediaSummary::default(),
        entries: vec![
            store_index_test_entry("photos/2026/newest.jpg"),
            store_index_test_entry("archive/2025/middle.jpg"),
            store_index_test_entry("photos/2024/oldest.jpg"),
        ],
    };
    let options = StoreIndexRequestOptions {
        offset: Some(0),
        limit: Some(32),
        sort: Some(StoreIndexSortOrder::CapturedDesc),
        media_filter: Some(StoreIndexMediaFilter::Image),
        ..StoreIndexRequestOptions::default()
    };

    synthesize_missing_folder_markers_for_page(&mut response, &options);

    assert_eq!(
        response
            .entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>(),
        vec![
            "archive/",
            "archive/2025/",
            "photos/",
            "photos/2024/",
            "photos/2026/",
            "photos/2026/newest.jpg",
            "archive/2025/middle.jpg",
            "photos/2024/oldest.jpg",
        ]
    );
    assert_eq!(response.entry_count, 8);
    assert_eq!(response.total_entry_count, 64);
}

#[test]
fn folder_marker_synthesis_leaves_later_pages_unchanged() {
    let mut response = StoreIndexResponse {
        prefix: String::new(),
        depth: 64,
        entry_count: 2,
        total_entry_count: 64,
        offset: 32,
        limit: Some(32),
        has_more: false,
        next_cursor: None,
        sync_token: None,
        consistency_token: None,
        media_summary: StoreIndexMediaSummary::default(),
        entries: vec![
            store_index_test_entry("photos/2024/newer.jpg"),
            store_index_test_entry("archive/2023/older.jpg"),
        ],
    };
    let options = StoreIndexRequestOptions {
        offset: Some(32),
        limit: Some(32),
        sort: Some(StoreIndexSortOrder::CapturedDesc),
        media_filter: Some(StoreIndexMediaFilter::Image),
        ..StoreIndexRequestOptions::default()
    };

    synthesize_missing_folder_markers_for_page(&mut response, &options);

    assert_eq!(
        response
            .entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>(),
        vec!["photos/2024/newer.jpg", "archive/2023/older.jpg"]
    );
    assert_eq!(response.entry_count, 2);
    assert_eq!(response.total_entry_count, 64);
}

fn store_index_test_entry(path: &str) -> StoreIndexEntry {
    StoreIndexEntry {
        path: path.to_string(),
        entry_type: "key".to_string(),
        labels: Vec::new(),
        labels_resolved: false,
        version: None,
        content_hash: None,
        size_bytes: None,
        modified_at_unix: None,
        content_fingerprint: None,
        media: None,
    }
}

#[test]
fn place_downloaded_file_creates_missing_target_directory() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "ironmesh-place-downloaded-file-test-{}-{}",
        std::process::id(),
        nonce
    ));
    let source_dir = root.join("source");
    let target_dir = root.join("target").join("nested");
    fs::create_dir_all(&source_dir).unwrap();
    let temp_path = source_dir.join("download.part");
    let target_path = target_dir.join("download.bin");
    fs::write(&temp_path, b"hello").unwrap();

    place_downloaded_file(&temp_path, &target_path).unwrap();

    assert_eq!(fs::read(&target_path).unwrap(), b"hello");
    assert!(!temp_path.exists());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn delete_url_builder_builds_expected_path() {
    let client = IronMeshClient::from_direct_base_url("http://127.0.0.1:18080/");
    let url = client.store_delete_url().expect("delete url should build");
    assert_eq!(url.as_str(), "http://127.0.0.1:18080/api/v1/store/delete");
}

#[test]
fn versions_url_builder_builds_expected_path() {
    let client = IronMeshClient::from_direct_base_url("http://127.0.0.1:18080/");
    let url = client.store_versions_url("docs/readme.txt").unwrap();
    assert_eq!(
        url.as_str(),
        "http://127.0.0.1:18080/api/v1/versions/docs%2Freadme.txt"
    );
}

#[tokio::test]
async fn expected_revision_is_sent_separately_for_put_delete_and_recursive_delete() {
    async fn put(
        axum::extract::Query(query): axum::extract::Query<
            std::collections::HashMap<String, String>,
        >,
    ) -> axum::http::StatusCode {
        if query.get("expected_revision").map(String::as_str) == Some("version-7")
            && !query.contains_key("parent")
        {
            axum::http::StatusCode::CREATED
        } else {
            axum::http::StatusCode::BAD_REQUEST
        }
    }

    async fn delete(
        axum::extract::Query(query): axum::extract::Query<
            std::collections::HashMap<String, String>,
        >,
    ) -> axum::http::StatusCode {
        let is_recursive = query.get("recursive").map(String::as_str) == Some("true");
        let valid_revision = if is_recursive {
            !query.contains_key("expected_revision")
        } else {
            query.get("expected_revision").map(String::as_str) == Some("version-7")
        };
        if valid_revision && !query.contains_key("parent") {
            axum::http::StatusCode::CREATED
        } else {
            axum::http::StatusCode::BAD_REQUEST
        }
    }

    let app = axum::Router::new()
        .route("/api/v1/store/delete", axum::routing::post(delete))
        .route("/api/v1/store/{key}", axum::routing::put(put));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let addr = listener.local_addr().expect("listener should have addr");
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app.into_make_service()).await;
    });

    let client = IronMeshClient::from_direct_base_url(format!("http://{addr}"));
    client
        .put_with_expected_revision(
            "docs/readme.txt",
            bytes::Bytes::from_static(b"updated"),
            Some("version-7"),
        )
        .await
        .expect("expected revision PUT should be accepted");
    client
        .delete_path_with_expected_revision("docs/readme.txt", Some("version-7"))
        .await
        .expect("expected revision delete should be accepted");
    client
        .delete_path_with_expected_revision("docs/", None)
        .await
        .expect("recursive delete should not send a parent or expected revision");

    handle.abort();
}

#[tokio::test]
async fn list_versions_parses_version_graph_summary() {
    async fn versions(
        axum::extract::Path(key): axum::extract::Path<String>,
    ) -> axum::Json<VersionGraphSummary> {
        axum::Json(VersionGraphSummary {
            key,
            object_id: "obj-123".to_string(),
            preferred_head_version_id: Some("v2".to_string()),
            preferred_head_reason: Some(PreferredHeadReason::DeterministicTiebreakVersionId),
            head_version_ids: vec!["v2".to_string()],
            versions: vec![VersionRecordSummary {
                version_id: "v2".to_string(),
                logical_path: Some("docs/readme.txt".to_string()),
                parent_version_ids: vec!["v1".to_string()],
                state: VersionConsistencyState::Confirmed,
                created_at_unix: 123,
                copied_from_object_id: None,
                copied_from_version_id: None,
                copied_from_path: None,
            }],
        })
    }

    let app = axum::Router::new().route("/api/v1/versions/{key}", axum::routing::get(versions));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let addr = listener.local_addr().expect("listener should have addr");
    let server = axum::serve(listener, app.into_make_service());
    let handle = tokio::spawn(async move {
        let _ = server.await;
    });

    let client = IronMeshClient::from_direct_base_url(format!("http://{addr}"));
    let versions = client
        .list_versions("docs/readme.txt")
        .await
        .expect("versions should parse")
        .expect("versions should exist");

    assert_eq!(versions.object_id, "obj-123");
    assert_eq!(versions.preferred_head_version_id.as_deref(), Some("v2"));
    assert_eq!(versions.versions.len(), 1);
    assert_eq!(versions.versions[0].version_id, "v2");

    handle.abort();
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RelayTestCapturedRequest {
    kind: Option<TransportStreamKind>,
    method: String,
    path_and_query: String,
    headers: Vec<RelayHttpHeader>,
    body: Vec<u8>,
}

fn capture_transport_request(
    request: &transport_sdk::BufferedTransportRequest,
) -> RelayTestCapturedRequest {
    RelayTestCapturedRequest {
        kind: Some(request.kind),
        method: request.method.clone(),
        path_and_query: request.path.clone(),
        headers: request
            .headers
            .iter()
            .map(|header| RelayHttpHeader {
                name: header.name.clone(),
                value: header.value.clone(),
            })
            .collect(),
        body: request.body.clone(),
    }
}

#[derive(Debug, Clone)]
struct DirectHttpRouteState {
    cluster_status_hits: Arc<AtomicUsize>,
    health_hits: Arc<AtomicUsize>,
    response_delay_ms: u64,
    name: String,
}

async fn spawn_direct_http_route_server_at(
    bind_addr: std::net::SocketAddr,
    response_delay_ms: u64,
    name: &str,
) -> (String, DirectHttpRouteState, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .expect("listener should bind");
    let addr = listener.local_addr().expect("listener should have addr");
    let state = DirectHttpRouteState {
        cluster_status_hits: Arc::new(AtomicUsize::new(0)),
        health_hits: Arc::new(AtomicUsize::new(0)),
        response_delay_ms,
        name: name.to_string(),
    };
    let router = Router::new()
        .route(
            "/api/v1/cluster/status",
            get(|State(state): State<DirectHttpRouteState>| async move {
                state.cluster_status_hits.fetch_add(1, Ordering::SeqCst);
                if state.response_delay_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(state.response_delay_ms)).await;
                }
                Json(serde_json::json!({
                    "status": "ok",
                    "route": state.name,
                }))
            }),
        )
        .route(
            "/api/v1/health",
            get(|State(state): State<DirectHttpRouteState>| async move {
                state.health_hits.fetch_add(1, Ordering::SeqCst);
                if state.response_delay_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(state.response_delay_ms)).await;
                }
                StatusCode::OK
            }),
        )
        .with_state(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("direct http route server should run");
    });
    (format!("http://{addr}"), state, server)
}

async fn spawn_direct_http_route_server(
    response_delay_ms: u64,
    name: &str,
) -> (String, DirectHttpRouteState, tokio::task::JoinHandle<()>) {
    spawn_direct_http_route_server_at(
        "127.0.0.1:0".parse().expect("bind addr should parse"),
        response_delay_ms,
        name,
    )
    .await
}

#[tokio::test]
async fn untracked_relative_path_requests_keep_server_failures_out_of_route_diagnostics() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener
        .local_addr()
        .expect("test listener should have an address");
    let server = tokio::spawn(async move {
        let app = Router::new()
            .route(
                "/api/v1/maps/test-success",
                get(|| async { StatusCode::OK }),
            )
            .route(
                "/api/v1/maps/test-failure",
                get(|| async { StatusCode::BAD_GATEWAY }),
            );
        let _ = axum::serve(listener, app).await;
    });
    let client = IronMeshClient::from_direct_base_url(format!("http://{address}"));

    let success = client
        .request_relative_path_without_route_diagnostics(
            Method::GET,
            "/maps/test-success",
            Vec::new(),
            None,
        )
        .await
        .expect("untracked success request should complete");
    assert_eq!(success.status, StatusCode::OK);

    let failure = client
        .request_relative_path_without_route_diagnostics(
            Method::GET,
            "/maps/test-failure",
            Vec::new(),
            None,
        )
        .await
        .expect("server response should remain available to the caller");
    assert_eq!(failure.status, StatusCode::BAD_GATEWAY);

    let endpoint = &client.connection_diagnostics().endpoints[0];
    assert_eq!(endpoint.consecutive_failures, 0);
    assert_eq!(endpoint.total_failures, 0);
    assert_eq!(endpoint.total_successes, 0);
    assert!(endpoint.last_used_unix_ms.is_none());
    assert!(endpoint.recent_attempts.is_empty());

    server.abort();

    let unavailable_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("temporary listener should bind");
    let unavailable_address = unavailable_listener
        .local_addr()
        .expect("temporary listener should have an address");
    drop(unavailable_listener);
    let unavailable_client =
        IronMeshClient::from_direct_base_url(format!("http://{unavailable_address}"));
    let refreshes = Arc::new(AtomicUsize::new(0));
    let observed_refreshes = Arc::clone(&refreshes);
    unavailable_client.set_transport_failure_refresh_observer(Some(Arc::new(move || {
        observed_refreshes.fetch_add(1, Ordering::SeqCst);
    })));
    unavailable_client
        .request_relative_path_without_route_diagnostics(
            Method::GET,
            "/maps/test-unavailable",
            Vec::new(),
            None,
        )
        .await
        .expect_err("unreachable map endpoint should fail");
    let endpoint = &unavailable_client.connection_diagnostics().endpoints[0];
    assert_eq!(endpoint.consecutive_failures, 1);
    assert_eq!(endpoint.total_failures, 1);
    assert!(endpoint.recent_attempts.is_empty());
    assert!(
        unavailable_client.connection_route_snapshot().endpoints[0]
            .circuit_open_until_unix_ms
            .is_some()
    );
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
}

#[derive(Clone)]
struct SnapshotHttpRouteState {
    hits: Arc<AtomicUsize>,
    snapshot_list_hits: Arc<AtomicUsize>,
    restore_hits: Arc<AtomicUsize>,
    object_hits: Arc<AtomicUsize>,
    status: StatusCode,
    response_body: Vec<u8>,
    snapshot_list_body: Vec<u8>,
    last_index_query: Arc<Mutex<Option<String>>>,
    last_restore_request: Arc<Mutex<Option<serde_json::Value>>>,
    last_object_query: Arc<Mutex<Option<String>>>,
}

async fn spawn_snapshot_http_route_server(
    status: StatusCode,
    response_body: Vec<u8>,
    snapshot_list_body: Vec<u8>,
) -> (String, SnapshotHttpRouteState, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let addr = listener.local_addr().expect("listener should have addr");
    let state = SnapshotHttpRouteState {
        hits: Arc::new(AtomicUsize::new(0)),
        snapshot_list_hits: Arc::new(AtomicUsize::new(0)),
        restore_hits: Arc::new(AtomicUsize::new(0)),
        object_hits: Arc::new(AtomicUsize::new(0)),
        status,
        response_body,
        snapshot_list_body,
        last_index_query: Arc::new(Mutex::new(None)),
        last_restore_request: Arc::new(Mutex::new(None)),
        last_object_query: Arc::new(Mutex::new(None)),
    };
    let router =
        Router::new()
            .route(
                "/api/v1/store/index",
                get(
                    |State(state): State<SnapshotHttpRouteState>,
                     RawQuery(query): RawQuery| async move {
                        state.hits.fetch_add(1, Ordering::SeqCst);
                        *state.last_index_query.lock().await = query;
                        (
                            state.status,
                            [(header::CONTENT_TYPE, "application/json")],
                            state.response_body,
                        )
                            .into_response()
                    },
                ),
            )
            .route(
                "/api/v1/snapshots",
                get(
                    |State(state): State<SnapshotHttpRouteState>| async move {
                        state.snapshot_list_hits.fetch_add(1, Ordering::SeqCst);
                        (
                            StatusCode::OK,
                            [(header::CONTENT_TYPE, "application/json")],
                            state.snapshot_list_body,
                        )
                            .into_response()
                    },
                ),
            )
            .route(
                "/api/v1/store/restore",
                post(
                    |State(state): State<SnapshotHttpRouteState>,
                     Json(request): Json<serde_json::Value>| async move {
                        state.restore_hits.fetch_add(1, Ordering::SeqCst);
                        *state.last_restore_request.lock().await = Some(request.clone());
                        Json(SnapshotRestoreResponse {
                            snapshot_id: request["snapshot"]
                                .as_str()
                                .unwrap_or_default()
                                .to_string(),
                            source_path: request["from_path"]
                                .as_str()
                                .unwrap_or_default()
                                .to_string(),
                            target_path: request["to_path"]
                                .as_str()
                                .unwrap_or_default()
                                .to_string(),
                            recursive: request["recursive"].as_bool().unwrap_or(false),
                            restored_count: 1,
                        })
                    },
                ),
            )
            .route(
                "/api/v1/store/photo.jpg",
                get(
                    |State(state): State<SnapshotHttpRouteState>,
                     RawQuery(query): RawQuery| async move {
                        state.object_hits.fetch_add(1, Ordering::SeqCst);
                        *state.last_object_query.lock().await = query;
                        (
                            state.status,
                            [(header::CONTENT_TYPE, "application/octet-stream")],
                            b"snapshot-object".to_vec(),
                        )
                    },
                ),
            )
            .route("/api/v1/health", get(|| async { StatusCode::OK }))
            .with_state(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("snapshot index test server should run");
    });
    (format!("http://{addr}"), state, server)
}

fn snapshot_index_response_body(path: &str) -> Vec<u8> {
    serde_json::to_vec(&StoreIndexResponse {
        prefix: String::new(),
        depth: 1,
        entry_count: 1,
        total_entry_count: 1,
        offset: 0,
        limit: None,
        has_more: false,
        next_cursor: None,
        sync_token: None,
        consistency_token: None,
        media_summary: StoreIndexMediaSummary::default(),
        entries: vec![StoreIndexEntry {
            path: path.to_string(),
            entry_type: "key".to_string(),
            labels: Vec::new(),
            labels_resolved: false,
            version: Some("v1".to_string()),
            content_hash: Some("hash-1".to_string()),
            size_bytes: Some(42),
            modified_at_unix: None,
            content_fingerprint: None,
            media: None,
        }],
    })
    .expect("store index response should serialize")
}

fn direct_http_test_client_for_node(base_url: String, node_id: NodeId) -> IronMeshClient {
    IronMeshClient::from_direct_http_client_with_target_node_id_and_ca_pem(
        base_url,
        HttpClient::new(),
        Some(node_id),
        None,
        None,
    )
}

#[derive(Clone, Default)]
struct UploadSessionHttpSharedState {
    sessions: Arc<Mutex<std::collections::HashMap<String, UploadSessionView>>>,
    available_chunk_hashes: Arc<Mutex<std::collections::HashSet<String>>>,
}

#[derive(Clone)]
struct UploadSessionHttpServerState {
    shared: UploadSessionHttpSharedState,
    start_hits: Arc<AtomicUsize>,
    chunk_hits: Arc<AtomicUsize>,
    complete_hits: Arc<AtomicUsize>,
    start_gate: Option<UploadSessionHttpStartGate>,
}

#[derive(Clone)]
struct UploadSessionHttpStartGate {
    request_started: Arc<Notify>,
    release_response: Arc<Notify>,
}

async fn upload_session_http_start(
    State(state): State<UploadSessionHttpServerState>,
    Json(request): Json<UploadSessionStartRequest>,
) -> impl IntoResponse {
    state.start_hits.fetch_add(1, Ordering::SeqCst);
    let chunk_size_bytes = CHUNK_UPLOAD_SIZE_BYTES;
    let chunk_count = if request.total_size_bytes == 0 {
        1
    } else {
        ((request.total_size_bytes - 1) / chunk_size_bytes as u64 + 1) as usize
    };
    let available_chunk_hashes = state.shared.available_chunk_hashes.lock().await;
    let mut received_indexes = request
        .chunk_refs
        .iter()
        .enumerate()
        .filter_map(|(index, chunk_ref)| {
            available_chunk_hashes
                .contains(&chunk_ref.hash)
                .then_some(index)
        })
        .collect::<Vec<_>>();
    received_indexes.sort_unstable();
    drop(available_chunk_hashes);
    let view = UploadSessionView {
        upload_id: format!("upload-{}", uuid::Uuid::now_v7()),
        key: request.key,
        total_size_bytes: request.total_size_bytes,
        chunk_size_bytes,
        chunk_count,
        received_indexes,
        completed: false,
        completed_result: None,
        expires_at_unix: unix_ts().saturating_add(60),
    };
    state
        .shared
        .sessions
        .lock()
        .await
        .insert(view.upload_id.clone(), view.clone());
    if let Some(start_gate) = state.start_gate.as_ref() {
        start_gate.request_started.notify_one();
        start_gate.release_response.notified().await;
    }
    (StatusCode::CREATED, Json(view)).into_response()
}

async fn upload_session_http_get(
    State(state): State<UploadSessionHttpServerState>,
    AxumPath(upload_id): AxumPath<String>,
) -> impl IntoResponse {
    let sessions = state.shared.sessions.lock().await;
    let Some(session) = sessions.get(&upload_id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    Json(session.clone()).into_response()
}

async fn upload_session_http_chunk(
    State(state): State<UploadSessionHttpServerState>,
    AxumPath((upload_id, index)): AxumPath<(String, usize)>,
    _payload: Bytes,
) -> impl IntoResponse {
    let mut sessions = state.shared.sessions.lock().await;
    let Some(session) = sessions.get_mut(&upload_id) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    state.chunk_hits.fetch_add(1, Ordering::SeqCst);
    if !session.received_indexes.contains(&index) {
        session.received_indexes.push(index);
        session.received_indexes.sort_unstable();
    }

    (
        StatusCode::OK,
        Json(UploadSessionChunkResponse {
            stored: true,
            received_index: index,
        }),
    )
        .into_response()
}

async fn upload_session_http_complete(
    State(state): State<UploadSessionHttpServerState>,
    AxumPath(upload_id): AxumPath<String>,
) -> impl IntoResponse {
    let mut sessions = state.shared.sessions.lock().await;
    let Some(session) = sessions.get_mut(&upload_id) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    state.complete_hits.fetch_add(1, Ordering::SeqCst);
    session.completed = true;
    let response = UploadSessionCompleteResponse {
        snapshot_id: "snap-test".to_string(),
        version_id: "ver-test".to_string(),
        manifest_hash: "manifest-test".to_string(),
        state: "confirmed".to_string(),
        new_chunks: session.received_indexes.len(),
        dedup_reused_chunks: 0,
        created_new_version: true,
        total_size_bytes: session.total_size_bytes,
    };
    session.completed_result = Some(response.clone());
    (StatusCode::OK, Json(response)).into_response()
}

async fn spawn_upload_session_http_server(
    bind_addr: std::net::SocketAddr,
    shared: UploadSessionHttpSharedState,
) -> (
    String,
    UploadSessionHttpServerState,
    tokio::task::JoinHandle<()>,
) {
    spawn_upload_session_http_server_with_start_gate(bind_addr, shared, None).await
}

async fn spawn_upload_session_http_server_with_start_gate(
    bind_addr: std::net::SocketAddr,
    shared: UploadSessionHttpSharedState,
    start_gate: Option<UploadSessionHttpStartGate>,
) -> (
    String,
    UploadSessionHttpServerState,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .expect("listener should bind");
    let addr = listener.local_addr().expect("listener addr");
    let state = UploadSessionHttpServerState {
        shared,
        start_hits: Arc::new(AtomicUsize::new(0)),
        chunk_hits: Arc::new(AtomicUsize::new(0)),
        complete_hits: Arc::new(AtomicUsize::new(0)),
        start_gate,
    };
    let router = Router::new()
        .route(
            "/api/v1/store/uploads/start",
            post(upload_session_http_start),
        )
        .route(
            "/api/v1/store/uploads/{upload_id}",
            get(upload_session_http_get),
        )
        .route(
            "/api/v1/store/uploads/{upload_id}/chunk/{index}",
            put(upload_session_http_chunk),
        )
        .route(
            "/api/v1/store/uploads/{upload_id}/complete",
            post(upload_session_http_complete),
        )
        .with_state(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("upload session http server should run");
    });
    (format!("http://{addr}"), state, server)
}

#[derive(Clone)]
struct RelayTestSecurity {
    cluster_id: uuid::Uuid,
    target_node_id: NodeId,
    cluster_ca_pem: String,
    cluster_ca_key_pem: String,
    target_identity: RelayTunnelTlsIdentity,
}

impl RelayTestSecurity {
    fn new() -> Self {
        Self::for_cluster(uuid::Uuid::now_v7(), NodeId::new_v4())
    }

    fn for_cluster(cluster_id: uuid::Uuid, target_node_id: NodeId) -> Self {
        let ca_key = KeyPair::generate().expect("relay test CA key should generate");
        let mut ca_params = CertificateParams::default();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.distinguished_name = DistinguishedName::new();
        ca_params
            .distinguished_name
            .push(DnType::CommonName, "ironmesh-client-relay-test-ca");
        ca_params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let ca_cert = ca_params
            .self_signed(&ca_key)
            .expect("relay test CA certificate should issue");
        let cluster_ca_pem = ca_cert.pem();
        let cluster_ca_key_pem = ca_key.serialize_pem();
        let issuer = Issuer::new(ca_params, ca_key);

        let target_identity = Self::issue_target_identity(cluster_id, target_node_id, &issuer);

        Self {
            cluster_id,
            target_node_id,
            cluster_ca_pem,
            cluster_ca_key_pem,
            target_identity,
        }
    }

    fn issue_target_identity(
        cluster_id: uuid::Uuid,
        target_node_id: NodeId,
        issuer: &Issuer<'_, KeyPair>,
    ) -> RelayTunnelTlsIdentity {
        let target_key = KeyPair::generate().expect("relay test target key should generate");
        let mut target_params = CertificateParams::default();
        target_params.distinguished_name.push(
            DnType::CommonName,
            format!("ironmesh-node-{target_node_id}"),
        );
        target_params.subject_alt_names = vec![
            SanType::URI(
                format!("urn:ironmesh:node:{target_node_id}")
                    .try_into()
                    .expect("relay target node SAN should parse"),
            ),
            SanType::URI(
                format!("urn:ironmesh:cluster:{cluster_id}")
                    .try_into()
                    .expect("relay target cluster SAN should parse"),
            ),
        ];
        target_params.extended_key_usages = vec![
            ExtendedKeyUsagePurpose::ServerAuth,
            ExtendedKeyUsagePurpose::ClientAuth,
        ];
        let target_cert = target_params
            .signed_by(&target_key, issuer)
            .expect("relay test target certificate should issue");
        RelayTunnelTlsIdentity::new(target_cert.pem(), target_key.serialize_pem())
    }

    fn target_identity_for_node(&self, node_id: NodeId) -> RelayTunnelTlsIdentity {
        let issuer_key =
            KeyPair::from_pem(&self.cluster_ca_key_pem).expect("relay test CA key should parse");
        let issuer = Issuer::from_ca_cert_pem(&self.cluster_ca_pem, issuer_key)
            .expect("relay test CA issuer should build");
        Self::issue_target_identity(self.cluster_id, node_id, &issuer)
    }

    fn source_security(&self, device_id: uuid::Uuid) -> RelayTunnelSourceSecurityConfig {
        let issuer_key =
            KeyPair::from_pem(&self.cluster_ca_key_pem).expect("relay test CA key should parse");
        let issuer = Issuer::from_ca_cert_pem(&self.cluster_ca_pem, issuer_key)
            .expect("relay test CA issuer should build");
        let source_key = KeyPair::generate().expect("relay test source key should generate");
        let mut source_params = CertificateParams::default();
        source_params
            .distinguished_name
            .push(DnType::CommonName, format!("ironmesh-device-{device_id}"));
        source_params.subject_alt_names = vec![SanType::URI(
            format!("urn:ironmesh:device:{device_id}")
                .try_into()
                .expect("relay source device SAN should parse"),
        )];
        source_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let source_cert = source_params
            .signed_by(&source_key, &issuer)
            .expect("relay test source certificate should issue");

        RelayTunnelSourceSecurityConfig {
            cluster_id: self.cluster_id,
            expected_target_node_id: self.target_node_id,
            cluster_ca_pem: self.cluster_ca_pem.as_bytes().to_vec(),
            identity: RelayTunnelTlsIdentity::new(source_cert.pem(), source_key.serialize_pem()),
        }
    }

    fn target_security(&self, expected_source: PeerIdentity) -> RelayTunnelTargetSecurityConfig {
        RelayTunnelTargetSecurityConfig {
            expected_source,
            cluster_ca_pem: self.cluster_ca_pem.as_bytes().to_vec(),
            identity: self.target_identity.clone(),
        }
    }
}

#[derive(Clone)]
struct RelayTestState {
    public_url: String,
    security: RelayTestSecurity,
    captured_request: Arc<Mutex<Option<RelayTestCapturedRequest>>>,
    health_hits: Arc<AtomicUsize>,
    issued_ticket_count: Arc<AtomicUsize>,
    paired_session_count: Arc<AtomicUsize>,
    target_handshake_failure_count: Arc<AtomicUsize>,
    object_write_failures_remaining: Arc<AtomicUsize>,
    response_delay_ms: Arc<AtomicU64>,
    response_status: u16,
    response_headers: Vec<RelayHttpHeader>,
    response_body: Vec<u8>,
}

async fn direct_transport_ws(
    State(state): State<RelayTestState>,
    websocket: WebSocketUpgrade,
) -> impl axum::response::IntoResponse {
    websocket.on_upgrade(move |socket| async move {
        state.paired_session_count.fetch_add(1, Ordering::SeqCst);
        serve_test_multiplex_socket(
            state,
            socket,
            format!("direct-session-{}", uuid::Uuid::now_v7()),
        )
        .await;
    })
}

#[derive(Clone)]
struct RelayMixedWorkloadState {
    public_url: String,
    security: RelayTestSecurity,
    payload: Arc<Vec<u8>>,
    issued_ticket_count: Arc<AtomicUsize>,
    paired_session_count: Arc<AtomicUsize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RelayTestWsMessage {
    Binary(Vec<u8>),
    Text(String),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close,
}

impl WebSocketMessageCodec for RelayTestWsMessage {
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

struct RelayTestSocketAdapter {
    socket: WebSocket,
}

impl Stream for RelayTestSocketAdapter {
    type Item = Result<RelayTestWsMessage, axum::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match Pin::new(&mut this.socket).poll_next(cx) {
            Poll::Ready(Some(Ok(Message::Binary(bytes)))) => {
                Poll::Ready(Some(Ok(RelayTestWsMessage::Binary(bytes.to_vec()))))
            }
            Poll::Ready(Some(Ok(Message::Text(text)))) => {
                Poll::Ready(Some(Ok(RelayTestWsMessage::Text(text.to_string()))))
            }
            Poll::Ready(Some(Ok(Message::Ping(payload)))) => {
                Poll::Ready(Some(Ok(RelayTestWsMessage::Ping(payload.to_vec()))))
            }
            Poll::Ready(Some(Ok(Message::Pong(payload)))) => {
                Poll::Ready(Some(Ok(RelayTestWsMessage::Pong(payload.to_vec()))))
            }
            Poll::Ready(Some(Ok(Message::Close(_)))) => {
                Poll::Ready(Some(Ok(RelayTestWsMessage::Close)))
            }
            Poll::Ready(Some(Err(err))) => Poll::Ready(Some(Err(err))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Sink<RelayTestWsMessage> for RelayTestSocketAdapter {
    type Error = axum::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.get_mut().socket).poll_ready(cx)
    }

    fn start_send(self: Pin<&mut Self>, item: RelayTestWsMessage) -> Result<(), Self::Error> {
        let message = match item {
            RelayTestWsMessage::Binary(bytes) => Message::Binary(bytes.into()),
            RelayTestWsMessage::Text(text) => Message::Text(text.into()),
            RelayTestWsMessage::Ping(payload) => Message::Ping(payload.into()),
            RelayTestWsMessage::Pong(payload) => Message::Pong(payload.into()),
            RelayTestWsMessage::Close => Message::Close(None),
        };
        Pin::new(&mut self.get_mut().socket).start_send(message)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.get_mut().socket).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.get_mut().socket).poll_close(cx)
    }
}

async fn serve_test_multiplex_socket(state: RelayTestState, socket: WebSocket, session_id: String) {
    let transport = WebSocketByteStream::new(RelayTestSocketAdapter { socket });
    let session =
        MultiplexedSession::spawn(transport, MultiplexMode::Server, MultiplexConfig::default())
            .expect("multiplexed relay test session should spawn");

    serve_test_multiplex_session(state, session, session_id).await;
}

async fn serve_test_multiplex_session(
    state: RelayTestState,
    mut session: MultiplexedSession,
    session_id: String,
) {
    let hello = perform_transport_server_handshake(
        &mut session,
        TransportSessionControlMessage::Ready {
            protocol_version: TRANSPORT_PROTOCOL_VERSION,
            session_id,
            max_concurrent_streams: MultiplexConfig::default().max_num_streams,
        },
    )
    .await
    .expect("multiplexed relay test handshake should succeed");
    assert!(matches!(
        hello,
        TransportSessionControlMessage::Hello {
            role: TransportSessionRole::Client,
            ..
        }
    ));

    loop {
        let next_stream = match session.accept_stream().await {
            Ok(next_stream) => next_stream,
            Err(error) => {
                let message = format!("{error:#}");
                if message.contains("Connection reset")
                    || message.contains("without closing handshake")
                    || message.contains("peer closed connection without sending TLS close_notify")
                {
                    return;
                }
                panic!("multiplexed relay test stream accept should succeed: {error:#}");
            }
        };
        let Some(mut stream) = next_stream else {
            return;
        };
        let request = read_buffered_transport_request(&mut stream)
            .await
            .expect("multiplexed relay test request should decode");
        if request.path == "/api/v1/health" {
            state.health_hits.fetch_add(1, Ordering::SeqCst);
        } else {
            // Background transport probes must not overwrite the request under test.
            *state.captured_request.lock().await = Some(capture_transport_request(&request));
        }

        let fail_object_write = request.kind == TransportStreamKind::ObjectWrite
            && state
                .object_write_failures_remaining
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok();
        if fail_object_write {
            return;
        }

        let response_delay_ms = state.response_delay_ms.load(Ordering::SeqCst);
        if response_delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(response_delay_ms)).await;
        }

        write_buffered_transport_response(
            &mut stream,
            &MultiplexBufferedTransportResponse {
                request_id: request.request_id,
                status: state.response_status,
                headers: state
                    .response_headers
                    .iter()
                    .map(|header| TransportHeader {
                        name: header.name.clone(),
                        value: header.value.clone(),
                    })
                    .collect(),
                body: state.response_body.clone(),
            },
        )
        .await
        .expect("multiplexed relay test response should write");
    }
}

async fn serve_secure_relay_test_targets(state: RelayTestState) {
    let control = RendezvousControlClient::new(
        RendezvousClientConfig {
            cluster_id: state.security.cluster_id,
            rendezvous_urls: vec![state.public_url.clone()],
            heartbeat_interval_secs: 15,
        },
        None,
        None,
    )
    .expect("secure relay test target control should build");

    loop {
        let tunnel = match control
            .accept_relay_tunnel(&RelayTunnelAcceptRequest {
                cluster_id: state.security.cluster_id,
                target: PeerIdentity::Node(state.security.target_node_id),
                session_kind: RelayTunnelSessionKind::MultiplexTransport,
                wait_timeout_ms: Some(3_000),
            })
            .await
        {
            Ok(tunnel) => tunnel,
            Err(error)
                if transport_sdk::is_expected_idle_relay_tunnel_accept_timeout(
                    &error.to_string(),
                ) =>
            {
                continue;
            }
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            }
        };
        assert_eq!(
            tunnel.session().security_mode,
            RelayTunnelSecurityMode::InnerMtls
        );
        let target_security = state
            .security
            .target_security(tunnel.session().source.clone());
        let target_result = tunnel
            .into_secure_multiplexed_target_session(target_security, MultiplexConfig::default())
            .await;
        let (relay_session, multiplexed) = match target_result {
            Ok(secured) => secured,
            Err(_) => {
                state
                    .target_handshake_failure_count
                    .fetch_add(1, Ordering::SeqCst);
                continue;
            }
        };
        state.paired_session_count.fetch_add(1, Ordering::SeqCst);
        let session_state = state.clone();
        tokio::spawn(async move {
            serve_test_multiplex_session(session_state, multiplexed, relay_session.session_id)
                .await;
        });
    }
}

async fn spawn_relay_test_server(
    response_status: u16,
    response_headers: Vec<RelayHttpHeader>,
    response_body: Vec<u8>,
) -> (RelayTestState, tokio::task::JoinHandle<()>) {
    spawn_relay_test_server_with_object_write_failures(
        response_status,
        response_headers,
        response_body,
        0,
    )
    .await
}

async fn spawn_relay_test_server_with_object_write_failures(
    response_status: u16,
    response_headers: Vec<RelayHttpHeader>,
    response_body: Vec<u8>,
    object_write_failures_remaining: usize,
) -> (RelayTestState, tokio::task::JoinHandle<()>) {
    spawn_relay_test_server_with_delay_and_object_write_failures(
        response_status,
        response_headers,
        response_body,
        0,
        object_write_failures_remaining,
    )
    .await
}

async fn spawn_relay_test_server_with_delay(
    response_status: u16,
    response_headers: Vec<RelayHttpHeader>,
    response_body: Vec<u8>,
    response_delay_ms: u64,
) -> (RelayTestState, tokio::task::JoinHandle<()>) {
    spawn_relay_test_server_with_delay_and_object_write_failures(
        response_status,
        response_headers,
        response_body,
        response_delay_ms,
        0,
    )
    .await
}

async fn spawn_relay_test_server_with_delay_and_object_write_failures(
    response_status: u16,
    response_headers: Vec<RelayHttpHeader>,
    response_body: Vec<u8>,
    response_delay_ms: u64,
    object_write_failures_remaining: usize,
) -> (RelayTestState, tokio::task::JoinHandle<()>) {
    spawn_relay_test_server_at(
        "127.0.0.1:0".parse().expect("bind addr should parse"),
        response_status,
        response_headers,
        response_body,
        response_delay_ms,
        object_write_failures_remaining,
    )
    .await
}

async fn wait_for_relay_test_runtime(public_url: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if reqwest::get(format!("{public_url}/health"))
                .await
                .is_ok_and(|response| response.status() == StatusCode::OK)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("relay test runtime should become healthy");
}

async fn spawn_relay_test_server_at(
    bind_addr: std::net::SocketAddr,
    response_status: u16,
    response_headers: Vec<RelayHttpHeader>,
    response_body: Vec<u8>,
    response_delay_ms: u64,
    object_write_failures_remaining: usize,
) -> (RelayTestState, tokio::task::JoinHandle<()>) {
    spawn_relay_test_server_at_with_security(
        bind_addr,
        response_status,
        response_headers,
        response_body,
        response_delay_ms,
        object_write_failures_remaining,
        RelayTestSecurity::new(),
    )
    .await
}

async fn spawn_relay_test_server_at_with_security(
    bind_addr: std::net::SocketAddr,
    response_status: u16,
    response_headers: Vec<RelayHttpHeader>,
    response_body: Vec<u8>,
    response_delay_ms: u64,
    object_write_failures_remaining: usize,
    security: RelayTestSecurity,
) -> (RelayTestState, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .expect("listener should bind");
    spawn_relay_test_server_on_listener_with_security(
        listener,
        response_status,
        response_headers,
        response_body,
        response_delay_ms,
        object_write_failures_remaining,
        security,
    )
    .await
}

async fn spawn_relay_test_server_on_listener_with_security(
    listener: tokio::net::TcpListener,
    response_status: u16,
    response_headers: Vec<RelayHttpHeader>,
    response_body: Vec<u8>,
    response_delay_ms: u64,
    object_write_failures_remaining: usize,
    security: RelayTestSecurity,
) -> (RelayTestState, tokio::task::JoinHandle<()>) {
    let addr = listener.local_addr().expect("listener addr");
    let state = RelayTestState {
        public_url: format!("http://{addr}"),
        security,
        captured_request: Arc::new(Mutex::new(None)),
        health_hits: Arc::new(AtomicUsize::new(0)),
        issued_ticket_count: Arc::new(AtomicUsize::new(0)),
        paired_session_count: Arc::new(AtomicUsize::new(0)),
        target_handshake_failure_count: Arc::new(AtomicUsize::new(0)),
        object_write_failures_remaining: Arc::new(AtomicUsize::new(
            object_write_failures_remaining,
        )),
        response_delay_ms: Arc::new(AtomicU64::new(response_delay_ms)),
        response_status,
        response_headers,
        response_body,
    };
    let rendezvous =
        rendezvous_server::RendezvousAppState::new(rendezvous_server::RendezvousServerConfig {
            bind_addr: addr,
            public_url: format!("{}/", state.public_url),
            relay_public_urls: vec![format!("{}/", state.public_url)],
            iroh_relay: None,
            peer_rendezvous_urls: Vec::new(),
            mtls: None,
        })
        .expect("secure relay test rendezvous state should build");
    let router = rendezvous_server::build_router(rendezvous).layer(axum::middleware::from_fn({
        let issued_ticket_count = Arc::clone(&state.issued_ticket_count);
        move |request: axum::extract::Request, next: axum::middleware::Next| {
            let issued_ticket_count = Arc::clone(&issued_ticket_count);
            async move {
                if request.method() == axum::http::Method::POST
                    && request.uri().path() == "/control/relay/ticket"
                {
                    issued_ticket_count.fetch_add(1, Ordering::SeqCst);
                }
                next.run(request).await
            }
        }
    }));
    let target_state = state.clone();
    let server = tokio::spawn(async move {
        tokio::select! {
            result = axum::serve(listener, router) => {
                result.expect("relay test server should run");
            }
            _ = serve_secure_relay_test_targets(target_state) => {}
        }
    });
    wait_for_relay_test_runtime(&state.public_url).await;
    (state, server)
}

async fn spawn_direct_transport_test_server(
    response_status: u16,
    response_headers: Vec<RelayHttpHeader>,
    response_body: Vec<u8>,
) -> (RelayTestState, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let addr = listener.local_addr().expect("listener addr");
    let state = RelayTestState {
        public_url: format!("http://{addr}"),
        security: RelayTestSecurity::new(),
        captured_request: Arc::new(Mutex::new(None)),
        health_hits: Arc::new(AtomicUsize::new(0)),
        issued_ticket_count: Arc::new(AtomicUsize::new(0)),
        paired_session_count: Arc::new(AtomicUsize::new(0)),
        target_handshake_failure_count: Arc::new(AtomicUsize::new(0)),
        object_write_failures_remaining: Arc::new(AtomicUsize::new(0)),
        response_delay_ms: Arc::new(AtomicU64::new(0)),
        response_status,
        response_headers,
        response_body,
    };
    let router = Router::new()
        .route("/transport/ws", get(direct_transport_ws))
        .with_state(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("direct transport test server should run");
    });
    (state, server)
}

#[derive(Clone)]
struct DirectTransportHangAfterFirstSuccessState {
    public_url: String,
    cluster_status_hits: Arc<AtomicUsize>,
    stalled_request_count: Arc<AtomicUsize>,
    paired_session_count: Arc<AtomicUsize>,
    captured_stalled_request: Arc<Mutex<Option<RelayTestCapturedRequest>>>,
}

#[derive(Clone)]
struct DirectTransportStallsObjectWriteState {
    public_url: String,
    cluster_status_hits: Arc<AtomicUsize>,
    stalled_request_count: Arc<AtomicUsize>,
    paired_session_count: Arc<AtomicUsize>,
    captured_stalled_request: Arc<Mutex<Option<RelayTestCapturedRequest>>>,
}

async fn direct_transport_hangs_after_first_success_ws(
    State(state): State<DirectTransportHangAfterFirstSuccessState>,
    websocket: WebSocketUpgrade,
) -> impl IntoResponse {
    websocket.on_upgrade(move |socket| async move {
        state.paired_session_count.fetch_add(1, Ordering::SeqCst);
        serve_direct_transport_hangs_after_first_success_socket(state, socket).await;
    })
}

async fn serve_direct_transport_hangs_after_first_success_socket(
    state: DirectTransportHangAfterFirstSuccessState,
    socket: WebSocket,
) {
    let transport = WebSocketByteStream::new(RelayTestSocketAdapter { socket });
    let mut session =
        MultiplexedSession::spawn(transport, MultiplexMode::Server, MultiplexConfig::default())
            .expect("stalling direct transport test session should spawn");
    let hello = perform_transport_server_handshake(
        &mut session,
        TransportSessionControlMessage::Ready {
            protocol_version: TRANSPORT_PROTOCOL_VERSION,
            session_id: format!("stalling-direct-session-{}", uuid::Uuid::now_v7()),
            max_concurrent_streams: MultiplexConfig::default().max_num_streams,
        },
    )
    .await
    .expect("stalling direct transport handshake should succeed");
    assert!(matches!(
        hello,
        TransportSessionControlMessage::Hello {
            role: TransportSessionRole::Client,
            ..
        }
    ));

    while let Some(mut stream) = session
        .accept_stream()
        .await
        .expect("stalling direct transport stream accept should succeed")
    {
        let request = read_buffered_transport_request(&mut stream)
            .await
            .expect("stalling direct transport request should decode");

        if request.path == "/api/v1/cluster/status" {
            let prior_hits = state.cluster_status_hits.fetch_add(1, Ordering::SeqCst);
            if prior_hits >= 1 {
                *state.captured_stalled_request.lock().await =
                    Some(capture_transport_request(&request));
                state.stalled_request_count.fetch_add(1, Ordering::SeqCst);
                std::future::pending::<()>().await;
            }
        }

        let response_body = br#"{"status":"ok","route":"direct"}"#.to_vec();
        write_buffered_transport_response(
            &mut stream,
            &MultiplexBufferedTransportResponse {
                request_id: request.request_id,
                status: StatusCode::OK.as_u16(),
                headers: vec![
                    TransportHeader {
                        name: "content-type".to_string(),
                        value: "application/json".to_string(),
                    },
                    TransportHeader {
                        name: "content-length".to_string(),
                        value: response_body.len().to_string(),
                    },
                ],
                body: response_body,
            },
        )
        .await
        .expect("stalling direct transport response should write");
    }
}

async fn spawn_direct_transport_server_that_hangs_after_first_success() -> (
    DirectTransportHangAfterFirstSuccessState,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let addr = listener.local_addr().expect("listener addr");
    let state = DirectTransportHangAfterFirstSuccessState {
        public_url: format!("http://{addr}"),
        cluster_status_hits: Arc::new(AtomicUsize::new(0)),
        stalled_request_count: Arc::new(AtomicUsize::new(0)),
        paired_session_count: Arc::new(AtomicUsize::new(0)),
        captured_stalled_request: Arc::new(Mutex::new(None)),
    };
    let router = Router::new()
        .route(
            "/transport/ws",
            get(direct_transport_hangs_after_first_success_ws),
        )
        .with_state(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("stalling direct transport test server should run");
    });
    (state, server)
}

async fn direct_transport_stalls_object_write_ws(
    State(state): State<DirectTransportStallsObjectWriteState>,
    websocket: WebSocketUpgrade,
) -> impl IntoResponse {
    websocket.on_upgrade(move |socket| async move {
        state.paired_session_count.fetch_add(1, Ordering::SeqCst);
        serve_direct_transport_stalls_object_write_socket(state, socket).await;
    })
}

async fn serve_direct_transport_stalls_object_write_socket(
    state: DirectTransportStallsObjectWriteState,
    socket: WebSocket,
) {
    let transport = WebSocketByteStream::new(RelayTestSocketAdapter { socket });
    let mut session =
        MultiplexedSession::spawn(transport, MultiplexMode::Server, MultiplexConfig::default())
            .expect("stalling direct object-write session should spawn");

    let hello = perform_transport_server_handshake(
        &mut session,
        TransportSessionControlMessage::Ready {
            protocol_version: TRANSPORT_PROTOCOL_VERSION,
            session_id: format!("stalling-direct-write-session-{}", uuid::Uuid::now_v7()),
            max_concurrent_streams: MultiplexConfig::default().max_num_streams,
        },
    )
    .await
    .expect("stalling direct object-write handshake should succeed");
    assert!(matches!(
        hello,
        TransportSessionControlMessage::Hello {
            role: TransportSessionRole::Client,
            ..
        }
    ));

    loop {
        let next_stream = match session.accept_stream().await {
            Ok(next_stream) => next_stream,
            Err(error) => {
                let message = format!("{error:#}");
                if message.contains("Connection reset")
                    || message.contains("without closing handshake")
                {
                    return;
                }
                panic!("stalling direct object-write stream accept should succeed: {error:#}");
            }
        };
        let Some(mut stream) = next_stream else {
            return;
        };
        let request = read_buffered_transport_request(&mut stream)
            .await
            .expect("stalling direct object-write request should decode");

        if request.path == "/api/v1/cluster/status" {
            state.cluster_status_hits.fetch_add(1, Ordering::SeqCst);
        }

        if request.method.eq_ignore_ascii_case("POST") {
            *state.captured_stalled_request.lock().await =
                Some(capture_transport_request(&request));
            state.stalled_request_count.fetch_add(1, Ordering::SeqCst);
            std::future::pending::<()>().await;
        }

        let (status, body, headers) = if request.path == "/api/v1/cluster/status" {
            let response_body = br#"{"status":"ok","route":"direct"}"#.to_vec();
            (
                StatusCode::OK.as_u16(),
                response_body.clone(),
                vec![
                    TransportHeader {
                        name: "content-type".to_string(),
                        value: "application/json".to_string(),
                    },
                    TransportHeader {
                        name: "content-length".to_string(),
                        value: response_body.len().to_string(),
                    },
                ],
            )
        } else {
            (
                StatusCode::OK.as_u16(),
                Vec::new(),
                vec![TransportHeader {
                    name: "content-length".to_string(),
                    value: "0".to_string(),
                }],
            )
        };
        write_buffered_transport_response(
            &mut stream,
            &MultiplexBufferedTransportResponse {
                request_id: request.request_id,
                status,
                headers,
                body,
            },
        )
        .await
        .expect("stalling direct object-write response should write");
    }
}

async fn spawn_direct_transport_server_that_stalls_object_write() -> (
    DirectTransportStallsObjectWriteState,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let addr = listener.local_addr().expect("listener addr");
    let state = DirectTransportStallsObjectWriteState {
        public_url: format!("http://{addr}"),
        cluster_status_hits: Arc::new(AtomicUsize::new(0)),
        stalled_request_count: Arc::new(AtomicUsize::new(0)),
        paired_session_count: Arc::new(AtomicUsize::new(0)),
        captured_stalled_request: Arc::new(Mutex::new(None)),
    };
    let router = Router::new()
        .route(
            "/transport/ws",
            get(direct_transport_stalls_object_write_ws),
        )
        .with_state(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("stalling direct object-write test server should run");
    });
    (state, server)
}

#[derive(Clone)]
struct DirectTransportDelayedStoreIndexWaitState {
    public_url: String,
    paired_session_count: Arc<AtomicUsize>,
    captured_request: Arc<Mutex<Option<RelayTestCapturedRequest>>>,
}

async fn direct_transport_delays_store_index_wait_ws(
    State(state): State<DirectTransportDelayedStoreIndexWaitState>,
    websocket: WebSocketUpgrade,
) -> impl IntoResponse {
    websocket.on_upgrade(move |socket| async move {
        state.paired_session_count.fetch_add(1, Ordering::SeqCst);
        serve_direct_transport_delays_store_index_wait_socket(state, socket).await;
    })
}

async fn serve_direct_transport_delays_store_index_wait_socket(
    state: DirectTransportDelayedStoreIndexWaitState,
    socket: WebSocket,
) {
    let transport = WebSocketByteStream::new(RelayTestSocketAdapter { socket });
    let mut session =
        MultiplexedSession::spawn(transport, MultiplexMode::Server, MultiplexConfig::default())
            .expect("delayed store index wait session should spawn");

    let hello = perform_transport_server_handshake(
        &mut session,
        TransportSessionControlMessage::Ready {
            protocol_version: TRANSPORT_PROTOCOL_VERSION,
            session_id: format!("delayed-store-index-session-{}", uuid::Uuid::now_v7()),
            max_concurrent_streams: MultiplexConfig::default().max_num_streams,
        },
    )
    .await
    .expect("delayed store index wait handshake should succeed");
    assert!(matches!(
        hello,
        TransportSessionControlMessage::Hello {
            role: TransportSessionRole::Client,
            ..
        }
    ));

    while let Some(mut stream) = session
        .accept_stream()
        .await
        .expect("delayed store index wait stream accept should succeed")
    {
        let request = read_buffered_transport_request(&mut stream)
            .await
            .expect("delayed store index wait request should decode");
        *state.captured_request.lock().await = Some(capture_transport_request(&request));
        assert_eq!(
            request.path,
            "/api/v1/store/index/changes/wait?since=41&timeout_ms=2500"
        );
        tokio::time::sleep(Duration::from_millis(2_500)).await;

        let response_body = serde_json::to_vec(&serde_json::json!({
            "sequence": 41,
            "changed": false,
        }))
        .expect("store index wait response should serialize");
        write_buffered_transport_response(
            &mut stream,
            &MultiplexBufferedTransportResponse {
                request_id: request.request_id,
                status: StatusCode::OK.as_u16(),
                headers: vec![
                    TransportHeader {
                        name: "content-type".to_string(),
                        value: "application/json".to_string(),
                    },
                    TransportHeader {
                        name: "content-length".to_string(),
                        value: response_body.len().to_string(),
                    },
                ],
                body: response_body,
            },
        )
        .await
        .expect("store index wait response should write");
    }
}

async fn spawn_direct_transport_server_that_delays_store_index_wait() -> (
    DirectTransportDelayedStoreIndexWaitState,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let addr = listener.local_addr().expect("listener addr");
    let state = DirectTransportDelayedStoreIndexWaitState {
        public_url: format!("http://{addr}"),
        paired_session_count: Arc::new(AtomicUsize::new(0)),
        captured_request: Arc::new(Mutex::new(None)),
    };
    let router = Router::new()
        .route(
            "/transport/ws",
            get(direct_transport_delays_store_index_wait_ws),
        )
        .with_state(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("delayed store index wait test server should run");
    });
    (state, server)
}

fn relay_header_value(headers: &[RelayHttpHeader], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case(name))
        .map(|header| header.value.clone())
}

async fn serve_mixed_workload_transport_socket(
    socket: WebSocket,
    payload: Arc<Vec<u8>>,
    session_id: String,
) {
    let transport = WebSocketByteStream::new(RelayTestSocketAdapter { socket });
    let session =
        MultiplexedSession::spawn(transport, MultiplexMode::Server, MultiplexConfig::default())
            .expect("mixed workload session should spawn");
    serve_mixed_workload_transport_session(session, payload, session_id).await;
}

async fn serve_mixed_workload_transport_session(
    mut session: MultiplexedSession,
    payload: Arc<Vec<u8>>,
    session_id: String,
) {
    let hello = perform_transport_server_handshake(
        &mut session,
        TransportSessionControlMessage::Ready {
            protocol_version: TRANSPORT_PROTOCOL_VERSION,
            session_id,
            max_concurrent_streams: MultiplexConfig::default().max_num_streams,
        },
    )
    .await
    .expect("mixed workload handshake should succeed");
    assert!(matches!(
        hello,
        TransportSessionControlMessage::Hello {
            role: TransportSessionRole::Client,
            ..
        }
    ));

    loop {
        let mut stream = match session.accept_stream().await {
            Ok(Some(stream)) => stream,
            Ok(None) => return,
            Err(error) => {
                let message = format!("{error:#}");
                if message.contains("Connection reset")
                    || message.contains("without closing handshake")
                    || message.contains("peer closed connection without sending TLS close_notify")
                {
                    return;
                }
                panic!("mixed workload stream accept should succeed: {error:#}");
            }
        };
        let payload = Arc::clone(&payload);
        tokio::spawn(async move {
            let request = read_buffered_transport_request(&mut stream)
                .await
                .expect("mixed workload request should decode");

            match (request.kind, request.method.as_str(), request.path.as_str()) {
                (TransportStreamKind::Rpc, "HEAD", "/api/v1/store/large.bin") => {
                    write_buffered_transport_response(
                        &mut stream,
                        &MultiplexBufferedTransportResponse {
                            request_id: request.request_id,
                            status: StatusCode::OK.as_u16(),
                            headers: vec![
                                TransportHeader {
                                    name: ACCEPT_RANGES.as_str().to_string(),
                                    value: "bytes".to_string(),
                                },
                                TransportHeader {
                                    name: CONTENT_LENGTH.as_str().to_string(),
                                    value: payload.len().to_string(),
                                },
                                TransportHeader {
                                    name: ETAG.as_str().to_string(),
                                    value: "\"mixed-etag\"".to_string(),
                                },
                                TransportHeader {
                                    name: "x-ironmesh-object-size".to_string(),
                                    value: payload.len().to_string(),
                                },
                            ],
                            body: Vec::new(),
                        },
                    )
                    .await
                    .expect("mixed workload HEAD response should write");
                }
                (TransportStreamKind::ObjectRead, "GET", "/api/v1/store/large.bin") => {
                    let range = request
                        .headers
                        .iter()
                        .find(|header| header.name.eq_ignore_ascii_case("range"))
                        .map(|header| header.value.clone())
                        .expect("range header should be present");
                    let (start, end_inclusive) = parse_range_header(&range, payload.len());
                    let selected = &payload[start..=end_inclusive];
                    write_transport_response_head(
                        &mut stream,
                        &TransportResponseHead {
                            request_id: request.request_id,
                            status: StatusCode::PARTIAL_CONTENT.as_u16(),
                            headers: vec![
                                TransportHeader {
                                    name: ACCEPT_RANGES.as_str().to_string(),
                                    value: "bytes".to_string(),
                                },
                                TransportHeader {
                                    name: CONTENT_LENGTH.as_str().to_string(),
                                    value: selected.len().to_string(),
                                },
                                TransportHeader {
                                    name: CONTENT_RANGE.as_str().to_string(),
                                    value: format!(
                                        "bytes {start}-{end_inclusive}/{}",
                                        payload.len()
                                    ),
                                },
                                TransportHeader {
                                    name: ETAG.as_str().to_string(),
                                    value: "\"mixed-etag\"".to_string(),
                                },
                                TransportHeader {
                                    name: "x-ironmesh-object-size".to_string(),
                                    value: payload.len().to_string(),
                                },
                            ],
                        },
                    )
                    .await
                    .expect("mixed workload object-read head should write");

                    for chunk in selected.chunks(16 * 1024) {
                        stream
                            .write_all(chunk)
                            .await
                            .expect("mixed workload object-read body should write");
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                    stream
                        .close()
                        .await
                        .expect("mixed workload object-read stream should close");
                }
                (
                    TransportStreamKind::ObjectRead,
                    "GET",
                    "/s3/photos.example/docs/streamed.txt",
                ) => {
                    write_transport_response_head(
                        &mut stream,
                        &TransportResponseHead {
                            request_id: request.request_id,
                            status: StatusCode::OK.as_u16(),
                            headers: vec![
                                TransportHeader {
                                    name: CONTENT_LENGTH.as_str().to_string(),
                                    value: payload.len().to_string(),
                                },
                                TransportHeader {
                                    name: ETAG.as_str().to_string(),
                                    value: "\"s3-streamed-etag\"".to_string(),
                                },
                                TransportHeader {
                                    name: "content-type".to_string(),
                                    value: "application/octet-stream".to_string(),
                                },
                            ],
                        },
                    )
                    .await
                    .expect("mixed workload S3 object-read head should write");

                    for chunk in payload.chunks(16 * 1024) {
                        stream
                            .write_all(chunk)
                            .await
                            .expect("mixed workload S3 object-read body should write");
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                    stream
                        .close()
                        .await
                        .expect("mixed workload S3 object-read stream should close");
                }
                (TransportStreamKind::Rpc, "GET", "/api/v1/cluster/status") => {
                    write_buffered_transport_response(
                        &mut stream,
                        &MultiplexBufferedTransportResponse {
                            request_id: request.request_id,
                            status: StatusCode::OK.as_u16(),
                            headers: vec![
                                TransportHeader {
                                    name: "content-type".to_string(),
                                    value: "application/json".to_string(),
                                },
                                TransportHeader {
                                    name: "content-length".to_string(),
                                    value: br#"{"status":"ok"}"#.len().to_string(),
                                },
                            ],
                            body: br#"{"status":"ok"}"#.to_vec(),
                        },
                    )
                    .await
                    .expect("mixed workload RPC response should write");
                }
                _ => {
                    write_buffered_transport_response(
                        &mut stream,
                        &MultiplexBufferedTransportResponse {
                            request_id: request.request_id,
                            status: StatusCode::BAD_REQUEST.as_u16(),
                            headers: vec![
                                TransportHeader {
                                    name: "content-type".to_string(),
                                    value: "text/plain; charset=utf-8".to_string(),
                                },
                                TransportHeader {
                                    name: "content-length".to_string(),
                                    value: b"unsupported".len().to_string(),
                                },
                            ],
                            body: b"unsupported".to_vec(),
                        },
                    )
                    .await
                    .expect("mixed workload error response should write");
                }
            }
        });
    }
}

async fn direct_mixed_workload_ws(
    websocket: WebSocketUpgrade,
    State(payload): State<Arc<Vec<u8>>>,
) -> impl IntoResponse {
    websocket.on_upgrade(move |socket| async move {
        serve_mixed_workload_transport_socket(
            socket,
            payload,
            format!("mixed-session-{}", uuid::Uuid::now_v7()),
        )
        .await;
    })
}

async fn serve_secure_relay_mixed_workload_targets(state: RelayMixedWorkloadState) {
    let control = RendezvousControlClient::new(
        RendezvousClientConfig {
            cluster_id: state.security.cluster_id,
            rendezvous_urls: vec![state.public_url.clone()],
            heartbeat_interval_secs: 15,
        },
        None,
        None,
    )
    .expect("secure mixed relay target control should build");

    loop {
        let tunnel = match control
            .accept_relay_tunnel(&RelayTunnelAcceptRequest {
                cluster_id: state.security.cluster_id,
                target: PeerIdentity::Node(state.security.target_node_id),
                session_kind: RelayTunnelSessionKind::MultiplexTransport,
                wait_timeout_ms: Some(3_000),
            })
            .await
        {
            Ok(tunnel) => tunnel,
            Err(error)
                if transport_sdk::is_expected_idle_relay_tunnel_accept_timeout(
                    &error.to_string(),
                ) =>
            {
                continue;
            }
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            }
        };
        assert_eq!(
            tunnel.session().security_mode,
            RelayTunnelSecurityMode::InnerMtls
        );
        let target_security = state
            .security
            .target_security(tunnel.session().source.clone());
        let (relay_session, multiplexed) = tunnel
            .into_secure_multiplexed_target_session(target_security, MultiplexConfig::default())
            .await
            .expect("secure mixed relay target should establish inner mTLS");
        state.paired_session_count.fetch_add(1, Ordering::SeqCst);
        let payload = Arc::clone(&state.payload);
        tokio::spawn(async move {
            serve_mixed_workload_transport_session(multiplexed, payload, relay_session.session_id)
                .await;
        });
    }
}

async fn spawn_direct_mixed_workload_test_server(
    payload: Arc<Vec<u8>>,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let addr = listener.local_addr().expect("listener addr");
    let router = Router::new()
        .route("/transport/ws", get(direct_mixed_workload_ws))
        .with_state(payload);
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("mixed workload server should run");
    });
    (format!("http://{addr}"), server)
}

async fn spawn_relay_mixed_workload_test_server(
    payload: Arc<Vec<u8>>,
) -> (RelayMixedWorkloadState, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let addr = listener.local_addr().expect("listener addr");
    let state = RelayMixedWorkloadState {
        public_url: format!("http://{addr}"),
        security: RelayTestSecurity::new(),
        payload,
        issued_ticket_count: Arc::new(AtomicUsize::new(0)),
        paired_session_count: Arc::new(AtomicUsize::new(0)),
    };
    let rendezvous =
        rendezvous_server::RendezvousAppState::new(rendezvous_server::RendezvousServerConfig {
            bind_addr: addr,
            public_url: format!("{}/", state.public_url),
            relay_public_urls: vec![format!("{}/", state.public_url)],
            iroh_relay: None,
            peer_rendezvous_urls: Vec::new(),
            mtls: None,
        })
        .expect("mixed workload rendezvous state should build");
    let router = rendezvous_server::build_router(rendezvous).layer(axum::middleware::from_fn({
        let issued_ticket_count = Arc::clone(&state.issued_ticket_count);
        move |request: axum::extract::Request, next: axum::middleware::Next| {
            let issued_ticket_count = Arc::clone(&issued_ticket_count);
            async move {
                if request.method() == axum::http::Method::POST
                    && request.uri().path() == "/control/relay/ticket"
                {
                    issued_ticket_count.fetch_add(1, Ordering::SeqCst);
                }
                next.run(request).await
            }
        }
    }));
    let target_state = state.clone();
    let server = tokio::spawn(async move {
        tokio::select! {
            result = axum::serve(listener, router) => {
                result.expect("relay mixed workload server should run");
            }
            _ = serve_secure_relay_mixed_workload_targets(target_state) => {}
        }
    });
    wait_for_relay_test_runtime(&state.public_url).await;
    (state, server)
}

fn relay_test_client_for_public_url(
    public_url: impl Into<String>,
    security: &RelayTestSecurity,
    identity: ClientIdentityMaterial,
) -> IronMeshClient {
    assert_eq!(identity.cluster_id, security.cluster_id);
    let rendezvous = RendezvousControlClient::new(
        RendezvousClientConfig {
            cluster_id: security.cluster_id,
            rendezvous_urls: vec![public_url.into()],
            heartbeat_interval_secs: 15,
        },
        None,
        None,
    )
    .expect("rendezvous client should build");
    let source_security = security.source_security(identity.device_id);
    IronMeshClient::with_relay_transport(
        "https://relay.invalid/",
        rendezvous,
        security.target_node_id,
        source_security,
    )
    .with_client_identity(identity)
}

fn relay_test_identity(security: &RelayTestSecurity, label: &str) -> ClientIdentityMaterial {
    let mut identity =
        ClientIdentityMaterial::generate(security.cluster_id, None, Some(label.to_string()))
            .expect("relay test identity should generate");
    identity.credential_pem = Some("issued-credential".to_string());
    identity
}

fn direct_transport_test_client(
    state: &RelayTestState,
    identity: ClientIdentityMaterial,
) -> IronMeshClient {
    IronMeshClient::from_direct_base_url(state.public_url.clone()).with_client_identity(identity)
}

#[derive(Clone)]
struct DirectQuicTestState {
    candidate: ConnectionCandidate,
    captured_request: Arc<Mutex<Option<RelayTestCapturedRequest>>>,
    health_hits: Arc<AtomicUsize>,
    paired_session_count: Arc<AtomicUsize>,
    response_status: u16,
    response_headers: Vec<RelayHttpHeader>,
    response_body: Vec<u8>,
}

async fn spawn_direct_quic_transport_test_server(
    response_status: u16,
    response_headers: Vec<RelayHttpHeader>,
    response_body: Vec<u8>,
    expected_target_node_id: NodeId,
) -> (DirectQuicTestState, tokio::task::JoinHandle<()>) {
    let endpoint = DirectQuicEndpoint::bind(DirectQuicEndpointConfig::new(SecretKey::generate()))
        .await
        .expect("direct QUIC test endpoint should bind");
    let state = DirectQuicTestState {
        candidate: endpoint.candidate(),
        captured_request: Arc::new(Mutex::new(None)),
        health_hits: Arc::new(AtomicUsize::new(0)),
        paired_session_count: Arc::new(AtomicUsize::new(0)),
        response_status,
        response_headers,
        response_body,
    };
    let server_state = state.clone();
    let server = tokio::spawn(async move {
        let Some(mut accepted) = endpoint
            .accept_session(MultiplexConfig::default())
            .await
            .expect("direct QUIC test accept should succeed")
        else {
            return;
        };
        server_state
            .paired_session_count
            .fetch_add(1, Ordering::SeqCst);
        let hello = perform_transport_server_handshake(
            &mut accepted.session,
            TransportSessionControlMessage::Ready {
                protocol_version: TRANSPORT_PROTOCOL_VERSION,
                session_id: format!("direct-quic-session-{}", uuid::Uuid::now_v7()),
                max_concurrent_streams: MultiplexConfig::default().max_num_streams,
            },
        )
        .await
        .expect("direct QUIC handshake should succeed");
        assert!(matches!(
            hello,
            TransportSessionControlMessage::Hello {
                role: TransportSessionRole::Client,
                target: Some(PeerIdentity::Node(node_id)),
                ..
            } if node_id == expected_target_node_id
        ));

        while let Some(mut stream) = accepted
            .session
            .accept_stream()
            .await
            .expect("direct QUIC request stream should accept")
        {
            let request = read_buffered_transport_request(&mut stream)
                .await
                .expect("direct QUIC request should decode");
            if request.path == "/api/v1/health" {
                server_state.health_hits.fetch_add(1, Ordering::SeqCst);
            }
            *server_state.captured_request.lock().await = Some(capture_transport_request(&request));
            write_buffered_transport_response(
                &mut stream,
                &MultiplexBufferedTransportResponse {
                    request_id: request.request_id,
                    status: server_state.response_status,
                    headers: server_state
                        .response_headers
                        .iter()
                        .map(|header| TransportHeader {
                            name: header.name.clone(),
                            value: header.value.clone(),
                        })
                        .collect(),
                    body: server_state.response_body.clone(),
                },
            )
            .await
            .expect("direct QUIC response should write");
        }
    });
    (state, server)
}

fn direct_quic_transport_test_client(
    state: &DirectQuicTestState,
    identity: ClientIdentityMaterial,
    target_node_id: NodeId,
) -> IronMeshClient {
    IronMeshClient::from_direct_quic_candidate_with_target_node_id(
        state.candidate.clone(),
        Some(target_node_id),
    )
    .with_client_identity(identity)
}

fn relay_test_client(state: &RelayTestState, identity: ClientIdentityMaterial) -> IronMeshClient {
    relay_test_client_for_public_url(state.public_url.clone(), &state.security, identity)
}

fn parse_range_header(range: &str, total_len: usize) -> (usize, usize) {
    let trimmed = range
        .strip_prefix("bytes=")
        .expect("range header should have bytes= prefix");
    let (start, end) = trimmed
        .split_once('-')
        .expect("range header should contain dash");
    let start = start.parse::<usize>().expect("range start should parse");
    let end = end.parse::<usize>().expect("range end should parse");
    assert!(start <= end, "range start must not exceed end");
    assert!(end < total_len, "range end must stay within payload");
    (start, end)
}

fn gallery_map_clusters_request() -> GalleryMapClustersRequest {
    GalleryMapClustersRequest {
        prefix: None,
        depth: 64,
        media_filter: StoreIndexMediaFilter::All,
        viewport: StoreIndexViewport {
            south: -90.0,
            west: -180.0,
            north: 90.0,
            east: 180.0,
        },
        zoom: 1.0,
        require_labels: Vec::new(),
        exclude_labels: Vec::new(),
    }
}

#[test]
fn gallery_map_label_filters_use_one_comma_separated_query_value_each() {
    let client = IronMeshClient::from_direct_base_url("http://127.0.0.1:18080/");
    let mut request = gallery_map_clusters_request();
    request.require_labels = vec![" private ".to_string()];
    request.exclude_labels = vec!["nsfw ".to_string()];

    let url = client
        .gallery_map_clusters_url("/api/v1/gallery/map/clusters", &request, 1, 1.0)
        .expect("gallery map URL should build");
    let query = url
        .query_pairs()
        .collect::<std::collections::HashMap<_, _>>();
    assert_eq!(
        query.get("require_labels").map(|value| value.as_ref()),
        Some("private")
    );
    assert_eq!(
        query.get("exclude_labels").map(|value| value.as_ref()),
        Some("nsfw")
    );
}

#[test]
fn label_filter_wire_format_escapes_commas_and_backslashes() {
    let mut url = Url::parse("http://127.0.0.1:18080/store/list")
        .expect("label filter test URL should parse");
    append_comma_separated_labels(
        &mut url,
        "require_labels",
        &["family, close".to_string(), "travel\\journal".to_string()],
    );

    assert_eq!(
        url.query_pairs()
            .find_map(|(key, value)| (key == "require_labels").then_some(value.into_owned())),
        Some(r"family\, close,travel\\journal".to_string())
    );
}

fn gallery_map_clusters_response_body() -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "prefix": "",
        "depth": 64,
        "zoom": 1,
        "resolution": 8,
        "total_entry_count": 0,
        "visible_geotagged_count": 0,
        "query_token": "gallery-map-query-token",
        "clusters": [],
    }))
    .expect("gallery map response fixture should serialize")
}

fn gallery_map_clusters_response_headers(body: &[u8]) -> Vec<RelayHttpHeader> {
    vec![
        RelayHttpHeader {
            name: "content-type".to_string(),
            value: "application/json".to_string(),
        },
        RelayHttpHeader {
            name: "content-length".to_string(),
            value: body.len().to_string(),
        },
    ]
}

fn gallery_map_cluster_entries_response() -> serde_json::Value {
    serde_json::json!({
        "cluster_id": "0_0",
        "entry_count": 0,
        "total_entry_count": 0,
        "offset": 0,
        "limit": 100,
        "has_more": false,
        "query_token": "gallery-map-query-token",
        "entries": [],
    })
}

#[tokio::test]
async fn gallery_map_clusters_use_the_canonical_path_over_direct_http() {
    let app = Router::new()
        .route(
            "/api/v1/gallery/map/clusters",
            get(|| async {
                Json(serde_json::json!({
                    "prefix": "",
                    "depth": 64,
                    "zoom": 1,
                    "resolution": 8,
                    "total_entry_count": 0,
                    "visible_geotagged_count": 0,
                    "query_token": "gallery-map-query-token",
                    "clusters": [],
                }))
            }),
        )
        .route(
            "/api/v1/gallery/map/cluster-entries",
            get(|| async { Json(gallery_map_cluster_entries_response()) }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let address = listener
        .local_addr()
        .expect("listener address should be available");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let client = IronMeshClient::from_direct_base_url(format!("http://{address}"));
    let response = client
        .gallery_map_clusters(gallery_map_clusters_request())
        .await
        .expect("canonical gallery map request should succeed");
    assert_eq!(response.query_token, "gallery-map-query-token");
    let entries = client
        .gallery_map_cluster_entries("gallery-map-query-token", "0_0", 0, 100)
        .await
        .expect("canonical gallery map entries request should succeed");
    assert_eq!(entries.cluster_id, "0_0");

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn gallery_map_clients_fall_back_to_and_cache_legacy_paths() {
    let canonical_clusters_hits = Arc::new(AtomicUsize::new(0));
    let canonical_entries_hits = Arc::new(AtomicUsize::new(0));
    let legacy_clusters_hits = Arc::new(AtomicUsize::new(0));
    let legacy_entries_hits = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route(
            "/api/v1/gallery/map/clusters",
            get({
                let hits = Arc::clone(&canonical_clusters_hits);
                move || {
                    let hits = Arc::clone(&hits);
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        StatusCode::NOT_FOUND
                    }
                }
            }),
        )
        .route(
            "/api/v1/gallery/map/cluster-entries",
            get({
                let hits = Arc::clone(&canonical_entries_hits);
                move || {
                    let hits = Arc::clone(&hits);
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        StatusCode::NOT_FOUND
                    }
                }
            }),
        )
        .route(
            "/api/v1/store/map/clusters",
            get({
                let hits = Arc::clone(&legacy_clusters_hits);
                move || {
                    let hits = Arc::clone(&hits);
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        Json(serde_json::json!({
                            "prefix": "",
                            "depth": 64,
                            "zoom": 1,
                            "resolution": 8,
                            "total_entry_count": 0,
                            "visible_geotagged_count": 0,
                            "query_token": "gallery-map-query-token",
                            "clusters": [],
                        }))
                    }
                }
            }),
        )
        .route(
            "/api/v1/store/map/cluster-entries",
            get({
                let hits = Arc::clone(&legacy_entries_hits);
                move || {
                    let hits = Arc::clone(&hits);
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        Json(gallery_map_cluster_entries_response())
                    }
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let address = listener
        .local_addr()
        .expect("listener address should be available");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let client = IronMeshClient::from_direct_base_url(format!("http://{address}"));

    for _ in 0..2 {
        client
            .gallery_map_clusters(gallery_map_clusters_request())
            .await
            .expect("legacy gallery map clusters fallback should succeed");
        client
            .gallery_map_cluster_entries("gallery-map-query-token", "0_0", 0, 100)
            .await
            .expect("legacy gallery map cluster entries fallback should succeed");
    }

    assert_eq!(canonical_clusters_hits.load(Ordering::SeqCst), 1);
    assert_eq!(legacy_clusters_hits.load(Ordering::SeqCst), 2);
    assert_eq!(canonical_entries_hits.load(Ordering::SeqCst), 1);
    assert_eq!(legacy_entries_hits.load(Ordering::SeqCst), 2);

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn gallery_map_clusters_use_the_canonical_path_over_relay_transport() {
    let response_body = gallery_map_clusters_response_body();
    let (relay_state, server) = spawn_relay_test_server(
        200,
        gallery_map_clusters_response_headers(&response_body),
        response_body,
    )
    .await;
    let client = relay_test_client(
        &relay_state,
        relay_test_identity(&relay_state.security, "relay-gallery-map-device"),
    );

    let response = client
        .gallery_map_clusters(gallery_map_clusters_request())
        .await
        .expect("gallery map request over relay should succeed");
    assert_eq!(response.query_token, "gallery-map-query-token");
    let captured = relay_state
        .captured_request
        .lock()
        .await
        .clone()
        .expect("relay request should be captured");
    assert!(
        captured
            .path_and_query
            .starts_with("/api/v1/gallery/map/clusters?")
    );

    server.abort();
    let _ = server.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gallery_map_clusters_use_the_canonical_path_over_direct_quic() {
    let response_body = gallery_map_clusters_response_body();
    let target_node_id = NodeId::new_v4();
    let (direct_state, server) = spawn_direct_quic_transport_test_server(
        StatusCode::OK.as_u16(),
        gallery_map_clusters_response_headers(&response_body),
        response_body,
        target_node_id,
    )
    .await;
    let mut identity = ClientIdentityMaterial::generate(
        uuid::Uuid::now_v7(),
        None,
        Some("direct-quic-gallery-map-device".to_string()),
    )
    .expect("identity should generate");
    identity.credential_pem = Some("issued-credential".to_string());
    let client = direct_quic_transport_test_client(&direct_state, identity, target_node_id);

    let response = client
        .gallery_map_clusters(gallery_map_clusters_request())
        .await
        .expect("gallery map request over direct QUIC should succeed");
    assert_eq!(response.query_token, "gallery-map-query-token");
    let captured = direct_state
        .captured_request
        .lock()
        .await
        .clone()
        .expect("direct QUIC request should be captured");
    assert!(
        captured
            .path_and_query
            .starts_with("/api/v1/gallery/map/clusters?")
    );

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn relay_transport_executes_store_index_request_with_signed_device_identity() {
    let (relay_state, server) = spawn_relay_test_server(
        200,
        vec![
            RelayHttpHeader {
                name: "content-type".to_string(),
                value: "application/json".to_string(),
            },
            RelayHttpHeader {
                name: "content-length".to_string(),
                value: serde_json::to_vec(&StoreIndexResponse {
                    prefix: String::new(),
                    depth: 1,
                    entry_count: 1,
                    total_entry_count: 1,
                    offset: 0,
                    limit: None,
                    has_more: false,
                    next_cursor: None,
                    sync_token: None,
                    consistency_token: None,
                    media_summary: StoreIndexMediaSummary::default(),
                    entries: vec![StoreIndexEntry {
                        path: "docs/readme.txt".to_string(),
                        entry_type: "key".to_string(),
                        labels: Vec::new(),
                        labels_resolved: false,
                        version: Some("v1".to_string()),
                        content_hash: Some("hash-1".to_string()),
                        size_bytes: Some(42),
                        modified_at_unix: None,
                        content_fingerprint: None,
                        media: None,
                    }],
                })
                .expect("store index response should serialize")
                .len()
                .to_string(),
            },
        ],
        serde_json::to_vec(&StoreIndexResponse {
            prefix: String::new(),
            depth: 1,
            entry_count: 1,
            total_entry_count: 1,
            offset: 0,
            limit: None,
            has_more: false,
            next_cursor: None,
            sync_token: None,
            consistency_token: None,
            media_summary: StoreIndexMediaSummary::default(),
            entries: vec![StoreIndexEntry {
                path: "docs/readme.txt".to_string(),
                entry_type: "key".to_string(),
                labels: Vec::new(),
                labels_resolved: false,
                version: Some("v1".to_string()),
                content_hash: Some("hash-1".to_string()),
                size_bytes: Some(42),
                modified_at_unix: None,
                content_fingerprint: None,
                media: None,
            }],
        })
        .expect("store index response should serialize"),
    )
    .await;

    let identity = relay_test_identity(&relay_state.security, "relay-test-device");
    let client = relay_test_client(&relay_state, identity.clone());

    let response = client
        .store_index(None, 1, None)
        .await
        .expect("store index over relay should succeed");

    assert_eq!(response.entry_count, 2);
    assert_eq!(response.entries[0].path, "docs/");
    assert_eq!(response.entries[1].path, "docs/readme.txt");

    let captured = relay_state
        .captured_request
        .lock()
        .await
        .clone()
        .expect("relay request should be captured");
    assert_eq!(captured.method, "GET");
    assert_eq!(captured.path_and_query, "/api/v1/store/index?depth=1");
    assert!(
        captured
            .headers
            .iter()
            .any(|header| header.name == transport_sdk::HEADER_DEVICE_ID
                && header.value == identity.device_id.to_string())
    );

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn relay_transport_executes_generic_json_get_request() {
    let (relay_state, server) = spawn_relay_test_server(
        200,
        vec![
            RelayHttpHeader {
                name: "content-type".to_string(),
                value: "application/json".to_string(),
            },
            RelayHttpHeader {
                name: "content-length".to_string(),
                value: br#"{"status":"ok"}"#.len().to_string(),
            },
        ],
        br#"{"status":"ok"}"#.to_vec(),
    )
    .await;

    let identity = relay_test_identity(&relay_state.security, "relay-test-device");
    let client = relay_test_client(&relay_state, identity.clone());

    let response = client
        .get_json_path("/cluster/status")
        .await
        .expect("generic JSON GET over relay should succeed");

    assert_eq!(response["status"], "ok");
    let attempt = client
        .connection_diagnostics()
        .endpoints
        .into_iter()
        .flat_map(|endpoint| endpoint.recent_attempts)
        .find(|attempt| attempt.url.contains("/api/v1/cluster/status"))
        .expect("relay request diagnostics should retain the completed request");
    assert_eq!(
        attempt.timeout_ms,
        Some(duration_to_u64_ms(CLIENT_BUFFERED_REQUEST_ATTEMPT_TIMEOUT).unwrap())
    );

    let captured = relay_state
        .captured_request
        .lock()
        .await
        .clone()
        .expect("relay request should be captured");
    assert_eq!(captured.path_and_query, "/api/v1/cluster/status");
    assert!(
        captured
            .headers
            .iter()
            .any(|header| header.name == transport_sdk::HEADER_DEVICE_ID
                && header.value == identity.device_id.to_string())
    );

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn first_successful_relay_request_notifies_dynamic_route_refresh_once() {
    let (relay_state, server) = spawn_relay_test_server(
        200,
        vec![
            RelayHttpHeader {
                name: "content-type".to_string(),
                value: "application/json".to_string(),
            },
            RelayHttpHeader {
                name: "content-length".to_string(),
                value: br#"{"status":"ok"}"#.len().to_string(),
            },
        ],
        br#"{"status":"ok"}"#.to_vec(),
    )
    .await;
    let client = relay_test_client(
        &relay_state,
        relay_test_identity(&relay_state.security, "relay-refresh-observer-device"),
    );
    let refreshes = Arc::new(AtomicUsize::new(0));
    let observed_refreshes = Arc::clone(&refreshes);
    let expected_target_node_id = relay_state.security.target_node_id;
    client.set_relay_connection_refresh_observer(Some(Arc::new(move |target_node_id| {
        assert_eq!(target_node_id, expected_target_node_id);
        observed_refreshes.fetch_add(1, Ordering::SeqCst);
    })));

    client
        .get_json_path("/cluster/status")
        .await
        .expect("first relay request should succeed");
    client
        .get_json_path("/cluster/status")
        .await
        .expect("reused relay request should succeed");

    assert_eq!(refreshes.load(Ordering::SeqCst), 1);

    server.abort();
    let _ = server.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_buffered_request_enforces_total_deadline() {
    let (relay_state, server) = spawn_relay_test_server(
        200,
        vec![
            RelayHttpHeader {
                name: "content-type".to_string(),
                value: "application/json".to_string(),
            },
            RelayHttpHeader {
                name: "content-length".to_string(),
                value: br#"{"status":"ok"}"#.len().to_string(),
            },
        ],
        br#"{"status":"ok"}"#.to_vec(),
    )
    .await;

    let identity = relay_test_identity(&relay_state.security, "relay-timeout-test-device");
    let client = relay_test_client(&relay_state, identity);
    client
        .get_json_path("/cluster/status")
        .await
        .expect("initial relay request should warm the multiplexed session");
    relay_state.response_delay_ms.store(2_000, Ordering::SeqCst);

    let endpoint = client
        .transport_router
        .endpoint(0)
        .expect("relay endpoint should exist");
    let ClientTransport::Relay(relay) = &endpoint.transport else {
        panic!("test client should use relay transport");
    };
    let source = relay_source_identity_for_auth(&client.auth_snapshot())
        .expect("relay source identity should be available");
    let url = client
        .relative_url("/cluster/status")
        .expect("relay request URL should build");
    let timeout = Duration::from_millis(250);
    let started_at = std::time::Instant::now();
    let error = execute_relay_multiplex_buffered_request(
        RelayMultiplexSessionContext {
            relay,
            source,
            connection_name: client.connection_name.as_deref(),
        },
        &Method::GET,
        &url,
        &[],
        &[],
        Some(timeout),
    )
    .await
    .expect_err("stalled relay request should hit its total deadline");

    assert!(started_at.elapsed() >= Duration::from_millis(200));
    assert!(started_at.elapsed() < Duration::from_secs(2));
    assert!(
        error.chain().any(|cause| cause
            .downcast_ref::<RelayMultiplexRequestTimeout>()
            .is_some()),
        "timeout should retain its typed cause: {error:#}"
    );
    assert!(
        format!("{error:#}").contains("timed out after 250ms"),
        "timeout should include its applied deadline: {error:#}"
    );
    let captured = relay_state
        .captured_request
        .lock()
        .await
        .clone()
        .expect("stalled relay request should reach the target");
    assert_eq!(captured.path_and_query, "/api/v1/cluster/status");

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn relay_transport_fails_closed_when_source_certificate_identity_mismatches_ticket() {
    let (relay_state, server) = spawn_relay_test_server(
        200,
        vec![RelayHttpHeader {
            name: "content-type".to_string(),
            value: "application/json".to_string(),
        }],
        br#"{"status":"ok"}"#.to_vec(),
    )
    .await;

    let identity = relay_test_identity(&relay_state.security, "relay-test-device");
    let wrong_source_security = relay_state.security.source_security(uuid::Uuid::now_v7());
    let rendezvous = RendezvousControlClient::new(
        RendezvousClientConfig {
            cluster_id: relay_state.security.cluster_id,
            rendezvous_urls: vec![relay_state.public_url.clone()],
            heartbeat_interval_secs: 15,
        },
        None,
        None,
    )
    .expect("rendezvous client should build");
    let client = IronMeshClient::with_relay_transport(
        "https://relay.invalid/",
        rendezvous,
        relay_state.security.target_node_id,
        wrong_source_security,
    )
    .with_client_identity(identity);

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        client.get_json_path("/cluster/status"),
    )
    .await
    .expect("mismatched relay identity request should fail promptly");
    assert!(
        result.is_err(),
        "mismatched relay identity must fail closed"
    );

    tokio::time::timeout(Duration::from_secs(5), async {
        while relay_state
            .target_handshake_failure_count
            .load(Ordering::SeqCst)
            == 0
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("target should observe the rejected source identity");
    assert!(relay_state.issued_ticket_count.load(Ordering::SeqCst) >= 1);
    assert_eq!(relay_state.paired_session_count.load(Ordering::SeqCst), 0);

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn relay_transport_rejects_target_with_wrong_node_identity() {
    let mut security = RelayTestSecurity::new();
    let wrong_target_node_id = NodeId::new_v4();
    security.target_identity = security.target_identity_for_node(wrong_target_node_id);
    let (relay_state, server) = spawn_relay_test_server_at_with_security(
        "127.0.0.1:0".parse().expect("bind addr should parse"),
        200,
        vec![RelayHttpHeader {
            name: "content-type".to_string(),
            value: "application/json".to_string(),
        }],
        br#"{"status":"ok"}"#.to_vec(),
        0,
        0,
        security,
    )
    .await;
    let client = relay_test_client(
        &relay_state,
        relay_test_identity(&relay_state.security, "relay-wrong-target-identity-device"),
    );

    let error = tokio::time::timeout(
        Duration::from_secs(5),
        client.get_json_path("/cluster/status"),
    )
    .await
    .expect("wrong target identity request should fail promptly")
    .expect_err("relay source must reject a target certificate with the wrong node SAN");
    assert!(
        format!("{error:#}").contains("inner mTLS relay session"),
        "unexpected relay target identity error: {error:#}"
    );
    assert!(relay_state.captured_request.lock().await.is_none());
    assert_eq!(relay_state.paired_session_count.load(Ordering::SeqCst), 0);

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn relay_transport_executes_relative_path_get_request() {
    let (relay_state, server) = spawn_relay_test_server(
        200,
        vec![
            RelayHttpHeader {
                name: "content-type".to_string(),
                value: "image/jpeg".to_string(),
            },
            RelayHttpHeader {
                name: "content-length".to_string(),
                value: b"thumb-jpeg-bytes".len().to_string(),
            },
        ],
        b"thumb-jpeg-bytes".to_vec(),
    )
    .await;

    let identity = relay_test_identity(&relay_state.security, "relay-test-device");
    let client = relay_test_client(&relay_state, identity.clone());

    let response = client
        .get_relative_path("/media/thumbnail?key=gallery%2Fcat.png")
        .await
        .expect("relative GET over relay should succeed");

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body.as_ref(), b"thumb-jpeg-bytes");

    let captured = relay_state
        .captured_request
        .lock()
        .await
        .clone()
        .expect("relay request should be captured");
    assert_eq!(
        captured.path_and_query,
        "/api/v1/media/thumbnail?key=gallery%2Fcat.png"
    );
    assert!(
        captured
            .headers
            .iter()
            .any(|header| header.name == transport_sdk::HEADER_DEVICE_ID
                && header.value == identity.device_id.to_string())
    );

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn relay_transport_preserves_head_response_headers() {
    let payload = b"head-only-payload";
    let (relay_state, server) = spawn_relay_test_server(
        200,
        vec![
            RelayHttpHeader {
                name: ACCEPT_RANGES.as_str().to_string(),
                value: "bytes".to_string(),
            },
            RelayHttpHeader {
                name: CONTENT_LENGTH.as_str().to_string(),
                value: payload.len().to_string(),
            },
            RelayHttpHeader {
                name: ETAG.as_str().to_string(),
                value: "\"relay-head-etag\"".to_string(),
            },
        ],
        Vec::new(),
    )
    .await;

    let identity = relay_test_identity(&relay_state.security, "relay-test-device");
    let client = relay_test_client(&relay_state, identity.clone());

    let response = client
        .head_object("gallery/cat.png", None, None)
        .await
        .expect("HEAD over relay should succeed");

    assert_eq!(response.total_size_bytes, payload.len() as u64);
    assert!(response.accept_ranges);
    assert_eq!(response.etag.as_deref(), Some("\"relay-head-etag\""));

    let captured = relay_state
        .captured_request
        .lock()
        .await
        .clone()
        .expect("relay request should be captured");
    assert_eq!(captured.method, "HEAD");
    assert_eq!(captured.path_and_query, "/api/v1/store/gallery%2Fcat.png");

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn relay_transport_reuses_multiplexed_session_for_multiple_requests() {
    let (relay_state, server) = spawn_relay_test_server(
        200,
        vec![
            RelayHttpHeader {
                name: "content-type".to_string(),
                value: "application/json".to_string(),
            },
            RelayHttpHeader {
                name: "content-length".to_string(),
                value: br#"{"status":"ok"}"#.len().to_string(),
            },
        ],
        br#"{"status":"ok"}"#.to_vec(),
    )
    .await;

    let identity = relay_test_identity(&relay_state.security, "relay-test-device");
    let client = relay_test_client(&relay_state, identity);

    let first = client
        .get_json_path("/cluster/status")
        .await
        .expect("first multiplex relay request should succeed");
    let second = client
        .get_json_path("/cluster/status")
        .await
        .expect("second multiplex relay request should succeed");

    assert_eq!(first["status"], "ok");
    assert_eq!(second["status"], "ok");
    assert_eq!(relay_state.issued_ticket_count.load(Ordering::SeqCst), 1);
    assert_eq!(relay_state.paired_session_count.load(Ordering::SeqCst), 1);

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn relay_transport_streams_upload_session_chunks_over_object_write() {
    let response_body = serde_json::to_vec(&UploadSessionChunkResponse {
        stored: true,
        received_index: 2,
    })
    .expect("upload chunk response should serialize");
    let (relay_state, server) = spawn_relay_test_server(
        200,
        vec![
            RelayHttpHeader {
                name: "content-type".to_string(),
                value: "application/json".to_string(),
            },
            RelayHttpHeader {
                name: "content-length".to_string(),
                value: response_body.len().to_string(),
            },
        ],
        response_body,
    )
    .await;

    let identity = relay_test_identity(&relay_state.security, "relay-upload-test-device");
    let client = relay_test_client(&relay_state, identity);

    let response = client
        .upload_session_chunk_bytes("upload-123", 2, b"chunk-body".to_vec())
        .await
        .expect("relay upload chunk should succeed");

    assert!(response.stored);
    assert_eq!(response.received_index, 2);

    let captured = relay_state
        .captured_request
        .lock()
        .await
        .clone()
        .expect("relay request should be captured");
    assert_eq!(captured.kind, Some(TransportStreamKind::ObjectWrite));
    assert_eq!(captured.method, "PUT");
    assert_eq!(
        captured.path_and_query,
        "/api/v1/store/uploads/upload-123/chunk/2"
    );
    assert_eq!(captured.body, b"chunk-body".to_vec());
    let attempt = client
        .connection_diagnostics()
        .endpoints
        .into_iter()
        .flat_map(|endpoint| endpoint.recent_attempts)
        .find(|attempt| {
            attempt
                .url
                .contains("/api/v1/store/uploads/upload-123/chunk/2")
        })
        .expect("relay upload diagnostics should retain the completed request");
    assert_eq!(
        attempt.timeout_ms,
        Some(duration_to_u64_ms(CLIENT_BUFFERED_REQUEST_ATTEMPT_TIMEOUT).unwrap()),
        "diagnostics should report the relay response-head deadline"
    );

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn relay_streamed_upload_chunk_enforces_response_head_deadline() {
    let response_body = serde_json::to_vec(&UploadSessionChunkResponse {
        stored: true,
        received_index: 3,
    })
    .expect("upload chunk response should serialize");
    let (relay_state, server) = spawn_relay_test_server(200, Vec::new(), response_body).await;
    let identity = relay_test_identity(&relay_state.security, "relay-upload-timeout-device");
    let client = relay_test_client(&relay_state, identity);
    client
        .get_json_path("/cluster/status")
        .await
        .expect("initial relay request should warm the multiplexed session");
    relay_state.response_delay_ms.store(2_000, Ordering::SeqCst);

    let endpoint = client
        .transport_router
        .endpoint(0)
        .expect("relay endpoint should exist");
    let ClientTransport::Relay(relay) = &endpoint.transport else {
        panic!("test client should use relay transport");
    };
    let source = relay_source_identity_for_auth(&client.auth_snapshot())
        .expect("relay source identity should be available");
    let url = client
        .relative_url("/store/uploads/upload-timeout/chunk/3")
        .expect("relay upload URL should build");
    let timeout = Duration::from_millis(250);
    let started_at = std::time::Instant::now();
    let error = execute_relay_multiplex_streaming_object_write_request(
        RelayMultiplexSessionContext {
            relay,
            source,
            connection_name: client.connection_name.as_deref(),
        },
        &Method::PUT,
        &url,
        &[],
        b"chunk-body",
        Some(timeout),
    )
    .await
    .expect_err("stalled relay response head should time out");

    assert!(started_at.elapsed() >= Duration::from_millis(200));
    assert!(started_at.elapsed() < Duration::from_secs(2));
    assert!(
        error.chain().any(|cause| cause
            .downcast_ref::<RelayMultiplexRequestTimeout>()
            .is_some()),
        "timeout should retain its relay-specific typed cause: {error:#}"
    );
    assert!(
        format!("{error:#}").contains("relay multiplex"),
        "timeout should identify the relay transport: {error:#}"
    );

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn relay_transport_retries_streamed_upload_chunk_after_partial_session_failure() {
    let response_body = serde_json::to_vec(&UploadSessionChunkResponse {
        stored: true,
        received_index: 4,
    })
    .expect("upload chunk response should serialize");
    let (relay_state, server) = spawn_relay_test_server_with_object_write_failures(
        200,
        vec![
            RelayHttpHeader {
                name: "content-type".to_string(),
                value: "application/json".to_string(),
            },
            RelayHttpHeader {
                name: "content-length".to_string(),
                value: response_body.len().to_string(),
            },
        ],
        response_body,
        1,
    )
    .await;

    let identity = relay_test_identity(&relay_state.security, "relay-upload-retry-test-device");
    let client = relay_test_client(&relay_state, identity);

    let response = client
        .upload_session_chunk_bytes("upload-retry", 4, b"retry-body".to_vec())
        .await
        .expect("relay upload chunk retry should succeed");

    assert!(response.stored);
    assert_eq!(response.received_index, 4);
    assert_eq!(relay_state.issued_ticket_count.load(Ordering::SeqCst), 2);
    assert_eq!(relay_state.paired_session_count.load(Ordering::SeqCst), 2);

    let captured = relay_state
        .captured_request
        .lock()
        .await
        .clone()
        .expect("relay request should be captured");
    assert_eq!(captured.kind, Some(TransportStreamKind::ObjectWrite));
    assert_eq!(
        captured.path_and_query,
        "/api/v1/store/uploads/upload-retry/chunk/4"
    );
    assert_eq!(captured.body, b"retry-body".to_vec());

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn upload_session_affinity_uses_same_node_after_path_change() {
    let shared_node_a = UploadSessionHttpSharedState::default();
    let node_a = NodeId::new_v4();
    let node_b = NodeId::new_v4();

    let (node_a_primary_url, node_a_primary_state, node_a_primary_server) =
        spawn_upload_session_http_server(
            "127.0.0.1:0".parse().expect("bind addr should parse"),
            shared_node_a.clone(),
        )
        .await;
    let (node_b_url, node_b_state, node_b_server) = spawn_upload_session_http_server(
        "127.0.0.1:0".parse().expect("bind addr should parse"),
        UploadSessionHttpSharedState::default(),
    )
    .await;
    let (node_a_secondary_url, node_a_secondary_state, node_a_secondary_server) =
        spawn_upload_session_http_server(
            "127.0.0.1:0".parse().expect("bind addr should parse"),
            shared_node_a,
        )
        .await;

    let client = IronMeshClient::combine(vec![
        IronMeshClient::from_direct_http_client_with_target_node_id_and_ca_pem(
            node_a_primary_url,
            HttpClient::new(),
            Some(node_a),
            None,
            None,
        ),
        IronMeshClient::from_direct_http_client_with_target_node_id_and_ca_pem(
            node_b_url,
            HttpClient::new(),
            Some(node_b),
            None,
            None,
        ),
        IronMeshClient::from_direct_http_client_with_target_node_id_and_ca_pem(
            node_a_secondary_url,
            HttpClient::new(),
            Some(node_a),
            None,
            None,
        ),
    ])
    .expect("combined direct client should build");

    let session = client
        .begin_upload_session("photos/path-change.bin", 5)
        .await
        .expect("upload session should start");
    assert_eq!(node_a_primary_state.start_hits.load(Ordering::SeqCst), 1);
    assert_eq!(node_b_state.start_hits.load(Ordering::SeqCst), 0);
    assert_eq!(node_a_secondary_state.start_hits.load(Ordering::SeqCst), 0);

    node_a_primary_server.abort();
    let _ = node_a_primary_server.await;

    let chunk = client
        .upload_session_chunk_bytes(&session.upload_id, 0, b"hello".to_vec())
        .await
        .expect("chunk upload should switch to the second path on the same node");
    assert_eq!(chunk.received_index, 0);

    let completed = client
        .finalize_upload_session(&session.upload_id)
        .await
        .expect("upload session completion should use the same-node fallback path");
    assert_eq!(completed.total_size_bytes, 5);

    assert_eq!(node_b_state.chunk_hits.load(Ordering::SeqCst), 0);
    assert_eq!(node_b_state.complete_hits.load(Ordering::SeqCst), 0);
    assert_eq!(node_a_secondary_state.chunk_hits.load(Ordering::SeqCst), 1);
    assert_eq!(
        node_a_secondary_state.complete_hits.load(Ordering::SeqCst),
        1
    );

    node_b_server.abort();
    let _ = node_b_server.await;
    node_a_secondary_server.abort();
    let _ = node_a_secondary_server.await;
}

#[tokio::test]
async fn upload_session_affinity_survives_route_reconciliation_during_start() {
    let node_a = NodeId::new_v4();
    let node_b = NodeId::new_v4();
    let start_gate = UploadSessionHttpStartGate {
        request_started: Arc::new(Notify::new()),
        release_response: Arc::new(Notify::new()),
    };
    let (node_a_url, node_a_state, node_a_server) =
        spawn_upload_session_http_server_with_start_gate(
            "127.0.0.1:0".parse().expect("bind addr should parse"),
            UploadSessionHttpSharedState::default(),
            Some(start_gate.clone()),
        )
        .await;
    let (node_b_url, node_b_state, node_b_server) = spawn_upload_session_http_server(
        "127.0.0.1:0".parse().expect("bind addr should parse"),
        UploadSessionHttpSharedState::default(),
    )
    .await;

    let client = IronMeshClient::combine(vec![
        IronMeshClient::from_direct_http_client_with_target_node_id_and_ca_pem(
            node_a_url.clone(),
            HttpClient::new(),
            Some(node_a),
            None,
            None,
        ),
        IronMeshClient::from_direct_http_client_with_target_node_id_and_ca_pem(
            node_b_url.clone(),
            HttpClient::new(),
            Some(node_b),
            None,
            None,
        ),
    ])
    .expect("combined direct client should build");

    let start_client = client.clone();
    let start_task = tokio::spawn(async move {
        start_client
            .begin_upload_session("photos/reconciliation-race.bin", 5)
            .await
    });
    start_gate.request_started.notified().await;

    let reordered_routes = IronMeshClient::combine(vec![
        IronMeshClient::from_direct_http_client_with_target_node_id_and_ca_pem(
            node_b_url,
            HttpClient::new(),
            Some(node_b),
            None,
            None,
        ),
        IronMeshClient::from_direct_http_client_with_target_node_id_and_ca_pem(
            node_a_url,
            HttpClient::new(),
            Some(node_a),
            None,
            None,
        ),
    ])
    .expect("reordered direct client should build");
    client.reconcile_transport_membership(&reordered_routes);
    start_gate.release_response.notify_one();

    let session = start_task
        .await
        .expect("upload-session start task should not panic")
        .expect("upload session should start");
    let chunk_result = client
        .upload_session_chunk_bytes(&session.upload_id, 0, b"hello".to_vec())
        .await;

    node_a_server.abort();
    let _ = node_a_server.await;
    node_b_server.abort();
    let _ = node_b_server.await;

    let chunk = chunk_result
        .expect("route reconciliation must not move a started upload session to another node");
    assert_eq!(chunk.received_index, 0);
    assert_eq!(node_a_state.chunk_hits.load(Ordering::SeqCst), 1);
    assert_eq!(node_b_state.chunk_hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn put_large_aware_skips_chunks_already_available_on_server() {
    let shared = UploadSessionHttpSharedState::default();
    let chunk_size = CHUNK_UPLOAD_SIZE_BYTES;
    let mut payload = vec![b'A'; chunk_size];
    payload.extend(vec![b'B'; 257]);

    shared
        .available_chunk_hashes
        .lock()
        .await
        .insert(hash_hex(&payload[..chunk_size]));

    let (base_url, state, server) = spawn_upload_session_http_server(
        "127.0.0.1:0".parse().expect("bind addr should parse"),
        shared.clone(),
    )
    .await;
    let client = IronMeshClient::from_direct_http_client(base_url, HttpClient::new());

    let report = client
        .put_large_aware("photos/reused.bin", Bytes::from(payload))
        .await
        .expect("chunked upload should succeed");

    assert!(matches!(report.upload_mode, UploadMode::Chunked));
    assert_eq!(report.chunk_count, Some(2));
    assert_eq!(state.start_hits.load(Ordering::SeqCst), 1);
    assert_eq!(state.chunk_hits.load(Ordering::SeqCst), 1);
    assert_eq!(state.complete_hits.load(Ordering::SeqCst), 1);

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn direct_transport_executes_and_reuses_multiplexed_session() {
    let (direct_state, server) = spawn_direct_transport_test_server(
        200,
        vec![
            RelayHttpHeader {
                name: "content-type".to_string(),
                value: "application/json".to_string(),
            },
            RelayHttpHeader {
                name: "content-length".to_string(),
                value: br#"{"status":"ok"}"#.len().to_string(),
            },
        ],
        br#"{"status":"ok"}"#.to_vec(),
    )
    .await;

    let mut identity = ClientIdentityMaterial::generate(
        uuid::Uuid::now_v7(),
        None,
        Some("direct-test-device".to_string()),
    )
    .expect("identity should generate");
    identity.credential_pem = Some("issued-credential".to_string());
    let client = direct_transport_test_client(&direct_state, identity.clone());

    let first = client
        .get_json_path("/cluster/status")
        .await
        .expect("first direct multiplex request should succeed");
    let second = client
        .get_json_path("/cluster/status")
        .await
        .expect("second direct multiplex request should succeed");

    assert_eq!(first["status"], "ok");
    assert_eq!(second["status"], "ok");
    assert_eq!(direct_state.paired_session_count.load(Ordering::SeqCst), 1);

    let captured = direct_state
        .captured_request
        .lock()
        .await
        .clone()
        .expect("direct request should be captured");
    assert_eq!(captured.path_and_query, "/api/v1/cluster/status");
    assert!(
        captured
            .headers
            .iter()
            .any(|header| header.name == transport_sdk::HEADER_DEVICE_ID
                && header.value == identity.device_id.to_string())
    );

    server.abort();
    let _ = server.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_quic_transport_executes_request_and_reports_diagnostics() {
    let target_node_id = NodeId::new_v4();
    let (direct_state, server) = spawn_direct_quic_transport_test_server(
        200,
        vec![
            RelayHttpHeader {
                name: "content-type".to_string(),
                value: "application/json".to_string(),
            },
            RelayHttpHeader {
                name: "content-length".to_string(),
                value: br#"{"status":"ok","route":"direct-quic"}"#.len().to_string(),
            },
            RelayHttpHeader {
                name: transport_sdk::HEADER_SERVER_PROCESSING_DURATION_US.to_string(),
                value: "1200".to_string(),
            },
            RelayHttpHeader {
                name: transport_sdk::HEADER_SERVER_RECEIVED_UNIX_MS.to_string(),
                value: unix_ts_ms().to_string(),
            },
            RelayHttpHeader {
                name: transport_sdk::HEADER_SERVER_RESPONDED_UNIX_MS.to_string(),
                value: unix_ts_ms().to_string(),
            },
        ],
        br#"{"status":"ok","route":"direct-quic"}"#.to_vec(),
        target_node_id,
    )
    .await;

    let test_result = async {
        let mut identity = ClientIdentityMaterial::generate(
            uuid::Uuid::now_v7(),
            None,
            Some("direct-quic-device".to_string()),
        )
        .expect("identity should generate");
        identity.credential_pem = Some("issued-credential".to_string());

        let client = direct_quic_transport_test_client(&direct_state, identity, target_node_id)
            .with_target_node_hostname(Some("direct-quic-node".to_string()));
        let response = client
            .get_json_path("/cluster/status")
            .await
            .expect("direct QUIC request should succeed");
        assert_eq!(response["route"], "direct-quic");

        let diagnostics = client.connection_diagnostics();
        assert_eq!(diagnostics.endpoints.len(), 1);
        assert_eq!(diagnostics.endpoints[0].path_kind, "direct");
        assert_eq!(
            diagnostics.endpoints[0].target_node_id,
            Some(target_node_id)
        );
        assert_eq!(
            diagnostics.endpoints[0].target_node_hostname.as_deref(),
            Some("direct-quic-node")
        );
        assert_eq!(
            diagnostics.endpoints[0].transport_path_kind.as_deref(),
            Some("direct_quic")
        );
        assert_eq!(
            diagnostics.endpoints[0].locator,
            direct_state.candidate.endpoint
        );
        assert_eq!(
            diagnostics.endpoints[0].request_base_url,
            direct_state.candidate.endpoint
        );
        let route_snapshot = client.connection_route_snapshot();
        assert_eq!(route_snapshot.endpoints.len(), 1);
        assert_eq!(
            route_snapshot.endpoints[0].path_kind,
            transport_sdk::TransportPathKind::DirectQuic
        );
        assert_eq!(
            route_snapshot.endpoints[0].target_node_hostname.as_deref(),
            Some("direct-quic-node")
        );
        assert_eq!(
            route_snapshot.endpoints[0].hole_punching_mode.as_deref(),
            Some("direct")
        );
        assert_eq!(
            route_snapshot.endpoints[0].iroh_relay_urls,
            Some(Vec::new())
        );
        assert_eq!(
            route_snapshot.endpoints[0].last_successful_iroh_relay_url,
            None
        );
        assert!(
            route_snapshot.endpoints[0]
                .recent_attempts
                .iter()
                .any(|attempt| {
                    attempt.outcome == "success"
                        && attempt.method == "GET"
                        && attempt.url.contains("/api/v1/cluster/status")
                }),
            "route snapshot should retain the completed request details"
        );
        let timed_attempt = route_snapshot.endpoints[0]
            .recent_attempts
            .iter()
            .find(|attempt| attempt.outcome == "success")
            .expect("successful request timing should be retained");
        assert_eq!(timed_attempt.status_code, Some(200));
        assert_eq!(timed_attempt.server_processing_duration_us, Some(1_200));
        assert!(timed_attempt.total_duration_us.is_some());
        assert!(timed_attempt.transport_overhead_us.is_some());
        assert!(timed_attempt.session_setup_duration_us > 0);
        assert_eq!(timed_attempt.relay_pairing_duration_us, 0);
        assert!(timed_attempt.network_transfer_duration_us.is_some());
        assert!(!timed_attempt.session_reused);
        assert!(timed_attempt.response_body_complete);
        assert_eq!(
            timed_attempt.response_bytes,
            br#"{"status":"ok","route":"direct-quic"}"#.len() as u64
        );
        assert!(timed_attempt.clock_offset_us.is_some());
        assert!(timed_attempt.clock_uncertainty_us.is_some());
        assert_eq!(client.transport_session_pool_snapshot().connect_count, 1);
        assert_eq!(direct_state.paired_session_count.load(Ordering::SeqCst), 1);

        let captured = direct_state
            .captured_request
            .lock()
            .await
            .clone()
            .expect("direct QUIC request should be captured");
        assert_eq!(captured.path_and_query, "/api/v1/cluster/status");
        assert!(
            captured
                .headers
                .iter()
                .any(|header| header.name == transport_sdk::HEADER_DEVICE_ID)
        );
    };

    test_result.await;
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn direct_quic_continues_after_iroh_relay_ticket_timeout() {
    let ticket_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("ticket listener should bind");
    let ticket_url = format!(
        "http://{}",
        ticket_listener
            .local_addr()
            .expect("ticket listener address should be available")
    );
    let ticket_request_started = Arc::new(AtomicUsize::new(0));
    let ticket_request_started_for_server = Arc::clone(&ticket_request_started);
    let ticket_server = tokio::spawn(async move {
        let (_stream, _) = ticket_listener
            .accept()
            .await
            .expect("ticket listener should accept a connection");
        ticket_request_started_for_server.fetch_add(1, Ordering::SeqCst);
        std::future::pending::<()>().await;
    });

    let target_node_id = NodeId::new_v4();
    let response_body = br#"{"status":"ok","route":"direct-quic"}"#.to_vec();
    let (direct_state, direct_server) = spawn_direct_quic_transport_test_server(
        200,
        vec![
            RelayHttpHeader {
                name: "content-type".to_string(),
                value: "application/json".to_string(),
            },
            RelayHttpHeader {
                name: "content-length".to_string(),
                value: response_body.len().to_string(),
            },
        ],
        response_body,
        target_node_id,
    )
    .await;

    let test_result = async {
        let mut identity = ClientIdentityMaterial::generate(
            uuid::Uuid::now_v7(),
            None,
            Some("direct-quic-ticket-timeout-device".to_string()),
        )
        .expect("identity should generate");
        identity.credential_pem = Some("issued-credential".to_string());
        let rendezvous = RendezvousControlClient::new(
            RendezvousClientConfig {
                cluster_id: identity.cluster_id,
                rendezvous_urls: vec![ticket_url],
                heartbeat_interval_secs: 15,
            },
            None,
            None,
        )
        .expect("rendezvous client should build");
        let client = IronMeshClient::from_direct_quic_candidate_with_rendezvous(
            direct_state.candidate.clone(),
            Some(target_node_id),
            Some(rendezvous),
            None,
        )
        .with_client_identity(identity);

        let started = std::time::Instant::now();
        let response = tokio::time::timeout(
            crate::session_pool::RELAY_TICKET_REQUEST_TIMEOUT + Duration::from_secs(8),
            client.get_json_path("/cluster/status"),
        )
        .await
        .expect("relay ticket timeout should not prevent direct QUIC connection")
        .expect("direct QUIC should succeed without a relay ticket");

        assert_eq!(response["route"], "direct-quic");
        assert!(started.elapsed() >= crate::session_pool::RELAY_TICKET_REQUEST_TIMEOUT);
        assert_eq!(ticket_request_started.load(Ordering::SeqCst), 1);
        assert_eq!(direct_state.paired_session_count.load(Ordering::SeqCst), 1);
        Ok::<(), anyhow::Error>(())
    }
    .await;

    ticket_server.abort();
    let _ = ticket_server.await;
    direct_server.abort();
    let _ = direct_server.await;

    test_result.unwrap();
}

#[tokio::test]
async fn rendezvous_backpressure_stops_direct_quic_route_fanout() {
    let ticket_requests = Arc::new(AtomicUsize::new(0));
    let ticket_requests_for_handler = Arc::clone(&ticket_requests);
    let router = Router::new()
        .route(
            "/control/iroh-relay/ticket",
            post(move || {
                let ticket_requests = Arc::clone(&ticket_requests_for_handler);
                async move {
                    ticket_requests.fetch_add(1, Ordering::SeqCst);
                    (
                        StatusCode::TOO_MANY_REQUESTS,
                        [(header::RETRY_AFTER, "60")],
                        "ticket lease limit reached",
                    )
                }
            }),
        )
        .route(
            "/control/iroh-relay/ticket/release",
            axum::routing::delete(|| async {
                Json(transport_sdk::IrohRelayTicketReleaseResponse { released: false })
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("rendezvous listener should bind");
    let rendezvous_url = format!(
        "http://{}",
        listener.local_addr().expect("rendezvous listener address")
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("rendezvous server should run");
    });

    let cluster_id = uuid::Uuid::now_v7();
    let mut identity = ClientIdentityMaterial::generate(
        cluster_id,
        None,
        Some("rendezvous-backpressure-device".to_string()),
    )
    .expect("identity should generate");
    identity.credential_pem = Some("issued-credential".to_string());
    let rendezvous = RendezvousControlClient::new(
        RendezvousClientConfig {
            cluster_id,
            rendezvous_urls: vec![rendezvous_url],
            heartbeat_interval_secs: 15,
        },
        None,
        None,
    )
    .expect("rendezvous client should build");
    let direct_quic_route = |target_node_id| {
        IronMeshClient::from_direct_quic_candidate_with_rendezvous(
            ConnectionCandidate {
                kind: CandidateKind::DirectQuic,
                endpoint: format!("iroh://{}", SecretKey::generate().public()),
                rtt_ms: None,
                transport_hints: None,
            },
            Some(target_node_id),
            Some(rendezvous.clone()),
            None,
        )
        .with_client_identity(identity.clone())
    };
    let client = IronMeshClient::combine(vec![
        direct_quic_route(NodeId::new_v4()),
        direct_quic_route(NodeId::new_v4()),
    ])
    .expect("direct QUIC routes should combine")
    .with_route_maintenance_policy(ClientRouteMaintenancePolicy {
        max_background_probe_candidates: 0,
        ..ClientRouteMaintenancePolicy::default()
    });

    let error = client
        .get_json_path("/cluster/status")
        .await
        .expect_err("Rendezvous backpressure should reject the request");
    assert!(
        transport_sdk::is_rendezvous_backpressure(&error),
        "unexpected error chain: {error:#}"
    );
    assert_eq!(ticket_requests.load(Ordering::SeqCst), 1);
    assert_eq!(
        client
            .connection_diagnostics()
            .endpoints
            .into_iter()
            .flat_map(|endpoint| endpoint.recent_attempts)
            .filter(|attempt| attempt.impact == ClientConnectionDiagnosticImpact::UserFacing)
            .count(),
        1,
        "the second route must not be attempted after ticket backpressure"
    );

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn direct_transport_executes_store_index_request_with_signed_device_identity() {
    let (direct_state, server) = spawn_direct_transport_test_server(
        200,
        vec![
            RelayHttpHeader {
                name: "content-type".to_string(),
                value: "application/json".to_string(),
            },
            RelayHttpHeader {
                name: "content-length".to_string(),
                value: serde_json::to_vec(&StoreIndexResponse {
                    prefix: String::new(),
                    depth: 1,
                    entry_count: 1,
                    total_entry_count: 1,
                    offset: 0,
                    limit: None,
                    has_more: false,
                    next_cursor: None,
                    sync_token: None,
                    consistency_token: None,
                    media_summary: StoreIndexMediaSummary::default(),
                    entries: vec![StoreIndexEntry {
                        path: "docs/readme.txt".to_string(),
                        entry_type: "key".to_string(),
                        labels: Vec::new(),
                        labels_resolved: false,
                        version: Some("v1".to_string()),
                        content_hash: Some("hash-1".to_string()),
                        size_bytes: Some(42),
                        modified_at_unix: None,
                        content_fingerprint: None,
                        media: None,
                    }],
                })
                .expect("store index response should serialize")
                .len()
                .to_string(),
            },
        ],
        serde_json::to_vec(&StoreIndexResponse {
            prefix: String::new(),
            depth: 1,
            entry_count: 1,
            total_entry_count: 1,
            offset: 0,
            limit: None,
            has_more: false,
            next_cursor: None,
            sync_token: None,
            consistency_token: None,
            media_summary: StoreIndexMediaSummary::default(),
            entries: vec![StoreIndexEntry {
                path: "docs/readme.txt".to_string(),
                entry_type: "key".to_string(),
                labels: Vec::new(),
                labels_resolved: false,
                version: Some("v1".to_string()),
                content_hash: Some("hash-1".to_string()),
                size_bytes: Some(42),
                modified_at_unix: None,
                content_fingerprint: None,
                media: None,
            }],
        })
        .expect("store index response should serialize"),
    )
    .await;

    let mut identity = ClientIdentityMaterial::generate(
        uuid::Uuid::now_v7(),
        None,
        Some("direct-store-index-device".to_string()),
    )
    .expect("identity should generate");
    identity.credential_pem = Some("issued-credential".to_string());
    let client = direct_transport_test_client(&direct_state, identity.clone());

    let response = client
        .store_index(None, 1, None)
        .await
        .expect("store index over direct transport should succeed");

    assert_eq!(response.entry_count, 2);
    assert_eq!(response.entries[0].path, "docs/");
    assert_eq!(response.entries[1].path, "docs/readme.txt");

    let captured = direct_state
        .captured_request
        .lock()
        .await
        .clone()
        .expect("direct request should be captured");
    assert_eq!(captured.method, "GET");
    assert_eq!(captured.path_and_query, "/api/v1/store/index?depth=1");
    assert!(
        captured
            .headers
            .iter()
            .any(|header| header.name == transport_sdk::HEADER_DEVICE_ID
                && header.value == identity.device_id.to_string())
    );

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn combined_direct_transports_fail_over_to_second_endpoint() {
    let _observer_test_guard = CONNECTION_DIAGNOSTICS_OBSERVER_TEST_LOCK.lock().await;
    let (direct_state, server) = spawn_direct_transport_test_server(
        200,
        vec![
            RelayHttpHeader {
                name: "content-type".to_string(),
                value: "application/json".to_string(),
            },
            RelayHttpHeader {
                name: "content-length".to_string(),
                value: br#"{"status":"ok"}"#.len().to_string(),
            },
        ],
        br#"{"status":"ok"}"#.to_vec(),
    )
    .await;

    let mut identity = ClientIdentityMaterial::generate(
        uuid::Uuid::now_v7(),
        None,
        Some("direct-failover-test-device".to_string()),
    )
    .expect("identity should generate");
    identity.credential_pem = Some("issued-credential".to_string());

    let failing = IronMeshClient::from_direct_base_url("http://127.0.0.1:9")
        .with_client_identity(identity.clone());
    let healthy = direct_transport_test_client(&direct_state, identity);
    let connection_name = "terminal-failover-diagnostics-test";
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured_events = Arc::clone(&events);
    set_connection_diagnostics_observer(Some(Arc::new(move |event| {
        if event.connection_name.as_deref() == Some(connection_name)
            && event.impact == ClientConnectionDiagnosticImpact::UserFacing
        {
            captured_events
                .lock()
                .expect("captured events lock should not be poisoned")
                .push(event);
        }
    })));
    let _observer_reset = ConnectionDiagnosticsObserverReset;
    let client = IronMeshClient::combine(vec![failing, healthy])
        .expect("combined direct client should build")
        .with_connection_name(connection_name);

    let first = client
        .get_json_path("/cluster/status")
        .await
        .expect("first combined direct request should succeed via fallback");
    let second = client
        .get_json_path("/cluster/status")
        .await
        .expect("second combined direct request should keep using the healthy route");

    assert_eq!(first["status"], "ok");
    assert_eq!(second["status"], "ok");
    assert_eq!(
        client.direct_server_base_url().as_deref(),
        Some(direct_state.public_url.as_str())
    );
    assert_eq!(direct_state.paired_session_count.load(Ordering::SeqCst), 1);
    {
        let events = events
            .lock()
            .expect("captured events lock should not be poisoned");
        assert_eq!(
            events.len(),
            2,
            "each successful routed request should publish only its terminal outcome"
        );
        assert!(events.iter().all(|event| {
            event
                .completed_operation
                .as_ref()
                .is_some_and(|operation| operation.attempt.outcome == "success")
        }));
    }

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn failed_routed_operation_publishes_one_terminal_failure() {
    let _observer_test_guard = CONNECTION_DIAGNOSTICS_OBSERVER_TEST_LOCK.lock().await;
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").expect("temporary listener should bind");
    let endpoint = format!(
        "http://{}",
        listener.local_addr().expect("listener should have addr")
    );
    drop(listener);

    let connection_name = "terminal-failure-diagnostics-test";
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured_events = Arc::clone(&events);
    set_connection_diagnostics_observer(Some(Arc::new(move |event| {
        if event.connection_name.as_deref() == Some(connection_name)
            && event.impact == ClientConnectionDiagnosticImpact::UserFacing
        {
            captured_events
                .lock()
                .expect("captured events lock should not be poisoned")
                .push(event);
        }
    })));
    let _observer_reset = ConnectionDiagnosticsObserverReset;
    let client =
        IronMeshClient::from_direct_base_url(endpoint).with_connection_name(connection_name);

    client
        .get_json_path("/cluster/status")
        .await
        .expect_err("unreachable route should fail the operation");

    let events = events
        .lock()
        .expect("captured events lock should not be poisoned");
    assert_eq!(events.len(), 1);
    assert!(
        events[0]
            .completed_operation
            .as_ref()
            .is_some_and(|operation| operation.attempt.outcome == "failure")
    );
}

#[test]
fn client_snapshot_selector_round_trips_and_accepts_legacy_ids() {
    let owner_node_id = NodeId::new_v4();
    let qualified = client_snapshot_selector(owner_node_id, "snap:with:colons");
    let parsed = parse_client_snapshot_selector(&qualified).expect("selector should parse");

    assert_eq!(parsed.owner_node_id, Some(owner_node_id));
    assert_eq!(parsed.snapshot_id, "snap:with:colons");

    let legacy = parse_client_snapshot_selector("legacy-snapshot").expect("legacy ID should parse");
    assert_eq!(legacy.owner_node_id, None);
    assert_eq!(legacy.snapshot_id, "legacy-snapshot");

    assert!(parse_client_snapshot_selector("snapshot-v1:not-a-node:snapshot").is_err());
    assert!(parse_client_snapshot_selector(&format!("snapshot-v1:{owner_node_id}:")).is_err());
}

#[tokio::test]
async fn snapshot_list_qualifies_ids_with_the_serving_node() {
    let snapshot_list_body = serde_json::to_vec(&serde_json::json!([{
        "id": "snapshot-local-7",
        "created_at_unix": 1234,
        "object_count": 9
    }]))
    .expect("snapshot list should serialize");
    let (base_url, state, server) = spawn_snapshot_http_route_server(
        StatusCode::OK,
        snapshot_index_response_body("unused.jpg"),
        snapshot_list_body,
    )
    .await;
    let owner_node_id = NodeId::new_v4();
    let client = direct_http_test_client_for_node(base_url, owner_node_id);

    let snapshots = client
        .list_snapshots()
        .await
        .expect("snapshot list should load");

    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].snapshot_id, "snapshot-local-7");
    assert_eq!(snapshots[0].server_node_id, Some(owner_node_id));
    assert_eq!(
        snapshots[0].id,
        client_snapshot_selector(owner_node_id, "snapshot-local-7")
    );
    assert_eq!(state.snapshot_list_hits.load(Ordering::SeqCst), 1);

    server.abort();
}

#[tokio::test]
async fn qualified_snapshot_store_index_targets_owner_and_strips_qualifier() {
    let (other_url, other_state, other_server) = spawn_snapshot_http_route_server(
        StatusCode::OK,
        snapshot_index_response_body("wrong-node.jpg"),
        Vec::new(),
    )
    .await;
    let (owner_url, owner_state, owner_server) = spawn_snapshot_http_route_server(
        StatusCode::OK,
        snapshot_index_response_body("snapshot-owner.jpg"),
        Vec::new(),
    )
    .await;
    let other_node_id = NodeId::new_v4();
    let owner_node_id = NodeId::new_v4();
    let client = IronMeshClient::combine(vec![
        direct_http_test_client_for_node(other_url, other_node_id),
        direct_http_test_client_for_node(owner_url, owner_node_id),
    ])
    .expect("combined direct client should build");
    let selector = client_snapshot_selector(owner_node_id, "snapshot-local-7");

    let response = client
        .store_index(None, 1, Some(&selector))
        .await
        .expect("qualified snapshot should reach its owner");

    assert_eq!(response.entries[0].path, "snapshot-owner.jpg");
    assert_eq!(other_state.hits.load(Ordering::SeqCst), 0);
    assert_eq!(owner_state.hits.load(Ordering::SeqCst), 1);
    let query = owner_state
        .last_index_query
        .lock()
        .await
        .clone()
        .expect("owner should receive a query");
    assert!(query.contains("snapshot=snapshot-local-7"));
    assert!(!query.contains("snapshot-v1"));

    other_server.abort();
    owner_server.abort();
}

#[tokio::test]
async fn qualified_snapshot_stream_targets_owner_and_strips_qualifier() {
    let (other_url, other_state, other_server) = spawn_snapshot_http_route_server(
        StatusCode::OK,
        snapshot_index_response_body("wrong-node.jpg"),
        Vec::new(),
    )
    .await;
    let (owner_url, owner_state, owner_server) = spawn_snapshot_http_route_server(
        StatusCode::OK,
        snapshot_index_response_body("snapshot-owner.jpg"),
        Vec::new(),
    )
    .await;
    let owner_node_id = NodeId::new_v4();
    let client = IronMeshClient::combine(vec![
        direct_http_test_client_for_node(other_url, NodeId::new_v4()),
        direct_http_test_client_for_node(owner_url, owner_node_id),
    ])
    .expect("combined direct client should build");
    let selector = client_snapshot_selector(owner_node_id, "snapshot-local-7");
    let mut payload = Vec::new();

    let response = client
        .stream_object_request_to_writer(
            "photo.jpg",
            Some(&selector),
            None,
            None,
            None,
            &mut payload,
        )
        .await
        .expect("qualified snapshot stream should reach its owner");

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(payload, b"snapshot-object");
    assert_eq!(other_state.object_hits.load(Ordering::SeqCst), 0);
    assert_eq!(owner_state.object_hits.load(Ordering::SeqCst), 1);
    let query = owner_state
        .last_object_query
        .lock()
        .await
        .clone()
        .expect("owner should receive a query");
    assert!(query.contains("snapshot=snapshot-local-7"));
    assert!(!query.contains("snapshot-v1"));

    other_server.abort();
    owner_server.abort();
}

#[tokio::test]
async fn qualified_snapshot_restore_targets_owner_and_strips_qualifier() {
    let (other_url, other_state, other_server) = spawn_snapshot_http_route_server(
        StatusCode::OK,
        snapshot_index_response_body("wrong-node.jpg"),
        Vec::new(),
    )
    .await;
    let (owner_url, owner_state, owner_server) = spawn_snapshot_http_route_server(
        StatusCode::OK,
        snapshot_index_response_body("snapshot-owner.jpg"),
        Vec::new(),
    )
    .await;
    let owner_node_id = NodeId::new_v4();
    let client = IronMeshClient::combine(vec![
        direct_http_test_client_for_node(other_url, NodeId::new_v4()),
        direct_http_test_client_for_node(owner_url, owner_node_id),
    ])
    .expect("combined direct client should build");
    let selector = client_snapshot_selector(owner_node_id, "snapshot-local-7");

    let response = client
        .restore_path_from_snapshot(&selector, "gallery/source.jpg", "restored.jpg", false, true)
        .await
        .expect("qualified snapshot restore should reach its owner");

    assert_eq!(response.snapshot_id, "snapshot-local-7");
    assert_eq!(other_state.restore_hits.load(Ordering::SeqCst), 0);
    assert_eq!(owner_state.restore_hits.load(Ordering::SeqCst), 1);
    let request = owner_state
        .last_restore_request
        .lock()
        .await
        .clone()
        .expect("owner should receive restore request");
    assert_eq!(request["snapshot"], "snapshot-local-7");

    other_server.abort();
    owner_server.abort();
}

#[tokio::test]
async fn qualified_snapshot_with_unavailable_owner_does_not_probe_other_nodes() {
    let (base_url, state, server) = spawn_snapshot_http_route_server(
        StatusCode::OK,
        snapshot_index_response_body("wrong-node.jpg"),
        Vec::new(),
    )
    .await;
    let client = direct_http_test_client_for_node(base_url, NodeId::new_v4());
    let unavailable_owner = NodeId::new_v4();
    let selector = client_snapshot_selector(unavailable_owner, "snapshot-local-7");

    let error = client
        .store_index(None, 1, Some(&selector))
        .await
        .expect_err("a missing owner route should fail before sending a request");

    assert!(format!("{error:#}").contains("no client transport route"));
    assert_eq!(state.hits.load(Ordering::SeqCst), 0);

    server.abort();
}

#[tokio::test]
async fn legacy_snapshot_store_index_keeps_normal_route_selection() {
    let (first_url, first_state, first_server) = spawn_snapshot_http_route_server(
        StatusCode::OK,
        snapshot_index_response_body("legacy-owner.jpg"),
        Vec::new(),
    )
    .await;
    let (second_url, second_state, second_server) = spawn_snapshot_http_route_server(
        StatusCode::OK,
        snapshot_index_response_body("unexpected-route.jpg"),
        Vec::new(),
    )
    .await;
    let client = IronMeshClient::combine(vec![
        direct_http_test_client_for_node(first_url, NodeId::new_v4()),
        direct_http_test_client_for_node(second_url, NodeId::new_v4()),
    ])
    .expect("combined direct client should build");

    let response = client
        .store_index(None, 1, Some("legacy-snapshot"))
        .await
        .expect("legacy snapshot IDs should remain supported");

    assert_eq!(response.entries[0].path, "legacy-owner.jpg");
    assert_eq!(first_state.hits.load(Ordering::SeqCst), 1);
    assert_eq!(second_state.hits.load(Ordering::SeqCst), 0);

    first_server.abort();
    second_server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_direct_buffered_request_enforces_total_deadline() {
    let (direct_state, direct_server) =
        spawn_direct_transport_server_that_hangs_after_first_success().await;

    let test_result = async {
        let mut identity = ClientIdentityMaterial::generate(
            uuid::Uuid::now_v7(),
            None,
            Some("single-direct-timeout-device".to_string()),
        )
        .expect("identity should generate");
        identity.credential_pem = Some("issued-credential".to_string());
        let client = IronMeshClient::from_direct_base_url(direct_state.public_url.clone())
            .with_client_identity(identity);
        client
            .get_json_path("/cluster/status")
            .await
            .expect("initial direct request should warm the multiplexed session");

        let endpoint = client
            .transport_router
            .endpoint(0)
            .expect("direct endpoint should exist");
        let ClientTransport::DirectHttp {
            server_base_url,
            session_pool,
            ..
        } = &endpoint.transport
        else {
            panic!("test client should use direct HTTP multiplex transport");
        };
        let auth = client.auth_snapshot();
        let ClientRequestAuth::SignedIdentity(identity) = &auth else {
            panic!("test client should have signed identity");
        };
        let direct = DirectMultiplexSessionContext {
            transport_locator: server_base_url,
            session_pool,
            identity,
            connection_name: client.connection_name.as_deref(),
            direct_quic_setup_waiter: DirectQuicSetupWaiter::SessionConsumer,
        };
        let url = client
            .relative_url("/cluster/status")
            .expect("direct request URL should build");
        let timeout = Duration::from_millis(250);
        let started_at = std::time::Instant::now();
        let error = execute_direct_multiplex_buffered_request(
            direct,
            &Method::GET,
            &url,
            &[],
            &[],
            Some(timeout),
        )
        .await
        .expect_err("stalled direct request should hit its total deadline");

        assert!(started_at.elapsed() >= Duration::from_millis(200));
        assert!(started_at.elapsed() < Duration::from_secs(2));
        assert!(
            error.chain().any(|cause| cause
                .downcast_ref::<DirectMultiplexRequestTimeout>()
                .is_some()),
            "timeout should retain its typed cause: {error:#}"
        );
        assert!(
            format!("{error:#}").contains("timed out after 250ms"),
            "timeout should include its applied deadline: {error:#}"
        );
        assert_eq!(direct_state.stalled_request_count.load(Ordering::SeqCst), 1);

        Ok::<(), anyhow::Error>(())
    }
    .await;

    direct_server.abort();
    let _ = direct_server.await;

    test_result.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_route_stall_falls_back_to_relay_after_warm_session_timeout() {
    let (direct_state, direct_server) =
        spawn_direct_transport_server_that_hangs_after_first_success().await;
    let relay_body = br#"{"status":"ok","route":"relay"}"#.to_vec();
    let (relay_state, relay_server) = spawn_relay_test_server_with_delay(
        200,
        vec![
            RelayHttpHeader {
                name: "content-type".to_string(),
                value: "application/json".to_string(),
            },
            RelayHttpHeader {
                name: "content-length".to_string(),
                value: relay_body.len().to_string(),
            },
        ],
        relay_body,
        500,
    )
    .await;

    let test_result = async {
        let identity = relay_test_identity(&relay_state.security, "direct-stall-failover-device");
        let target_node_id = relay_state.security.target_node_id;

        let direct = IronMeshClient::from_direct_base_url(direct_state.public_url.clone())
            .with_client_identity(identity.clone());
        let relay = relay_test_client(&relay_state, identity);
        let client = IronMeshClient::combine(vec![direct, relay])
            .expect("combined direct+relay client should build");

        // Simulates a live direct session going half-open during a network change such as
        // Wi-Fi -> cellular. The first request succeeds directly, the next one must switch over.
        let first = client
            .get_json_path("/cluster/status")
            .await
            .expect("initial direct request should succeed");
        assert_eq!(first["route"], "direct");
        assert!(!client.uses_relay_transport());

        let started_at = std::time::Instant::now();
        let fallback = tokio::time::timeout(
            Duration::from_secs(5),
            client.get_json_path("/cluster/status"),
        )
        .await;
        let fallback = match fallback {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                return Err(anyhow::anyhow!(
                    "request returned an error before relay fallback completed: {error:#}"
                ));
            }
            Err(_) => {
                return Err(anyhow::anyhow!(
                    "request did not fall back to relay after the direct session stalled"
                ));
            }
        };

        assert!(started_at.elapsed() >= CLIENT_WARM_MULTIPLEX_REQUEST_TIMEOUT);
        assert!(started_at.elapsed() < Duration::from_secs(5));
        assert_eq!(fallback["route"], "relay");
        assert!(client.uses_relay_transport());
        assert_eq!(client.relay_target_node_id(), Some(target_node_id));
        let direct_request = direct_state
            .captured_stalled_request
            .lock()
            .await
            .clone()
            .expect("stalled direct request should be captured after reaching the server");
        let relay_request = relay_state
            .captured_request
            .lock()
            .await
            .clone()
            .expect("relay fallback request should be captured");
        let direct_nonce = relay_header_value(
            &direct_request.headers,
            transport_sdk::HEADER_AUTH_NONCE,
        )
        .expect("direct attempt should carry an auth nonce");
        let relay_nonce =
            relay_header_value(&relay_request.headers, transport_sdk::HEADER_AUTH_NONCE)
                .expect("relay attempt should carry an auth nonce");
        assert_ne!(
            direct_nonce, relay_nonce,
            "fallback must not replay the nonce consumed by the timed-out direct attempt"
        );
        let snapshot = client.connection_route_snapshot();
        assert!(
            snapshot
                .endpoints
                .iter()
                .all(|endpoint| endpoint.last_used_unix_ms.is_some()),
            "both the stalled direct attempt and successful relay fallback should be visible as recently used"
        );

        let title_status = client.run_title_latency_probe().await;
        assert_eq!(
            title_status.state,
            crate::TitleLatencyProbeState::Success,
            "title latency probe should succeed on the recovered relay route: {:?}",
            title_status.error,
        );
        assert_eq!(
            title_status.connection_type,
            crate::TitleLatencyConnectionType::Relay,
        );
        assert!(direct_state.cluster_status_hits.load(Ordering::SeqCst) >= 1);
        assert_eq!(direct_state.paired_session_count.load(Ordering::SeqCst), 1);
        assert!(relay_state.issued_ticket_count.load(Ordering::SeqCst) >= 1);
        assert_eq!(relay_state.paired_session_count.load(Ordering::SeqCst), 1);
        let diagnostics = client.connection_diagnostics();
        let direct_timeout = diagnostics
            .endpoints
            .iter()
            .find(|endpoint| endpoint.path_kind == "direct")
            .and_then(|endpoint| endpoint.recent_attempts.last())
            .and_then(|attempt| attempt.timeout_ms);
        let relay_timeout = diagnostics
            .endpoints
            .iter()
            .find(|endpoint| endpoint.path_kind == "relay")
            .and_then(|endpoint| endpoint.recent_attempts.last())
            .and_then(|attempt| attempt.timeout_ms);
        let expected_timeout_ms =
            duration_to_u64_ms(CLIENT_BUFFERED_REQUEST_ATTEMPT_TIMEOUT).unwrap();
        assert_eq!(direct_timeout, Some(expected_timeout_ms));
        assert_eq!(relay_timeout, Some(expected_timeout_ms));

        Ok::<(), anyhow::Error>(())
    }
    .await;

    direct_server.abort();
    let _ = direct_server.await;
    relay_server.abort();
    let _ = relay_server.await;

    test_result.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_store_index_wait_keeps_its_explicit_long_poll_semantics() {
    let (direct_state, direct_server) =
        spawn_direct_transport_server_that_delays_store_index_wait().await;

    let test_result = async {
        let mut identity = ClientIdentityMaterial::generate(
            uuid::Uuid::now_v7(),
            None,
            Some("direct-store-index-wait-device".to_string()),
        )
        .expect("identity should generate");
        identity.credential_pem = Some("issued-credential".to_string());
        let client = IronMeshClient::from_direct_base_url(direct_state.public_url.clone())
            .with_client_identity(identity);

        let started_at = std::time::Instant::now();
        let response = tokio::time::timeout(
            Duration::from_secs(5),
            client.wait_for_store_index_change(41, 2_500),
        )
        .await
        .expect("store index wait should not time out client-side")
        .expect("store index wait should succeed");

        assert!(started_at.elapsed() >= Duration::from_millis(2_400));
        assert_eq!(response.sequence, 41);
        assert!(!response.changed);

        let captured = direct_state
            .captured_request
            .lock()
            .await
            .clone()
            .expect("direct request should be captured");
        assert_eq!(captured.method, "GET");
        assert_eq!(
            captured.path_and_query,
            "/api/v1/store/index/changes/wait?since=41&timeout_ms=2500"
        );
        assert_eq!(direct_state.paired_session_count.load(Ordering::SeqCst), 1);

        Ok::<(), anyhow::Error>(())
    }
    .await;

    direct_server.abort();
    let _ = direct_server.await;

    test_result.unwrap();
}

#[test]
fn buffered_request_timeout_applies_to_normal_and_bounds_long_running_paths() {
    let normal = Url::parse("https://node.example/api/v1/store/index?depth=64")
        .expect("normal request URL should parse");
    let store_wait =
        Url::parse("https://node.example/api/v1/store/index/changes/wait?since=41&timeout_ms=2500")
            .expect("store wait URL should parse");
    let default_store_wait =
        Url::parse("https://node.example/api/v1/store/index/changes/wait?since=41")
            .expect("store wait URL without timeout should parse");
    let delayed_latency =
        Url::parse("https://node.example/api/v1/diagnostics/latency?server_delay_ms=12000")
            .expect("delayed latency URL should parse");
    let immediate_latency =
        Url::parse("https://node.example/api/v1/diagnostics/latency?server_delay_ms=0")
            .expect("immediate latency URL should parse");

    assert_eq!(
        buffered_request_timeout(&normal),
        Some(Duration::from_secs(10))
    );
    assert_eq!(
        buffered_request_timeout(&store_wait),
        Some(Duration::from_millis(12_500))
    );
    assert_eq!(
        buffered_request_timeout(&default_store_wait),
        Some(Duration::from_secs(35))
    );
    assert_eq!(
        buffered_request_timeout(&delayed_latency),
        Some(Duration::from_secs(22))
    );
    assert_eq!(
        buffered_request_timeout(&immediate_latency),
        Some(Duration::from_secs(10))
    );
}

#[test]
fn timeout_detection_is_case_insensitive() {
    assert!(is_timeout_error_message("operation TIMED OUT"));
    assert!(is_timeout_error_message("transport timeout"));
    assert!(!is_timeout_error_message("connection reset by peer"));
}

#[test]
fn ensure_operation_id_header_reuses_existing_value_for_mutating_methods() {
    let mut headers = Vec::<RelayHttpHeader>::new();

    ensure_operation_id_header(&Method::POST, &mut headers);
    let first_operation_id = relay_header_value(&headers, transport_sdk::HEADER_OPERATION_ID)
        .expect("mutating request should gain an operation id");

    ensure_operation_id_header(&Method::POST, &mut headers);
    let second_operation_id = relay_header_value(&headers, transport_sdk::HEADER_OPERATION_ID)
        .expect("operation id should remain present after retry preparation");

    assert!(!first_operation_id.trim().is_empty());
    assert_eq!(first_operation_id, second_operation_id);
}

#[test]
fn request_headers_for_attempt_refreshes_auth_nonce_and_preserves_operation_id() {
    let mut identity = ClientIdentityMaterial::generate(
        uuid::Uuid::now_v7(),
        None,
        Some("per-attempt-auth-test-device".to_string()),
    )
    .expect("identity should generate");
    identity.credential_pem = Some("issued-credential".to_string());
    let auth = ClientRequestAuth::SignedIdentity(identity);
    let url =
        Url::parse("https://node.example/api/v1/maps/config").expect("request URL should parse");
    let mut request_headers = Vec::new();
    ensure_operation_id_header(&Method::POST, &mut request_headers);

    let first = request_headers_for_attempt(
        &auth,
        &Method::POST,
        &url,
        Some("gallery-map"),
        &request_headers,
    )
    .expect("first attempt headers should build");
    let second = request_headers_for_attempt(
        &auth,
        &Method::POST,
        &url,
        Some("gallery-map"),
        &request_headers,
    )
    .expect("second attempt headers should build");

    let first_nonce = relay_header_value(&first, transport_sdk::HEADER_AUTH_NONCE)
        .expect("first attempt should carry an auth nonce");
    let second_nonce = relay_header_value(&second, transport_sdk::HEADER_AUTH_NONCE)
        .expect("second attempt should carry an auth nonce");
    let first_operation_id = relay_header_value(&first, transport_sdk::HEADER_OPERATION_ID)
        .expect("first attempt should carry an operation id");
    let second_operation_id = relay_header_value(&second, transport_sdk::HEADER_OPERATION_ID)
        .expect("second attempt should carry an operation id");

    assert_ne!(first_nonce, second_nonce);
    assert_eq!(first_operation_id, second_operation_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mutating_request_reuses_operation_id_across_direct_timeout_and_relay_fallback() {
    let (direct_state, direct_server) =
        spawn_direct_transport_server_that_stalls_object_write().await;
    let (relay_state, relay_server) =
        spawn_relay_test_server_with_delay(StatusCode::OK.as_u16(), Vec::new(), Vec::new(), 500)
            .await;

    let test_result = async {
        let identity =
            relay_test_identity(&relay_state.security, "direct-write-failover-device");
        let target_node_id = relay_state.security.target_node_id;

        let direct = IronMeshClient::from_direct_base_url(direct_state.public_url.clone())
            .with_client_identity(identity.clone());
        let relay = relay_test_client(&relay_state, identity);
        let client = IronMeshClient::combine(vec![direct, relay])
            .expect("combined direct+relay client should build");

        let first = client
            .get_json_path("/cluster/status")
            .await
            .expect("initial direct request should succeed");
        assert_eq!(first["route"], "direct");

        tokio::time::timeout(
            Duration::from_secs(12),
            client.post_relative_path("/cluster/status"),
        )
        .await
        .expect("mutating request should fall back after the 10-second direct deadline")
        .expect("mutating request should fall back to relay");

        let relay_request = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(request) = relay_state.captured_request.lock().await.clone() {
                    break request;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("relay fallback request should be captured");

        let relay_operation_id =
            relay_header_value(&relay_request.headers, transport_sdk::HEADER_OPERATION_ID)
                .expect("relay request should carry an operation id");

        assert!(!relay_operation_id.trim().is_empty());
        assert_eq!(relay_request.method, "POST");
        assert_eq!(relay_request.path_and_query, "/api/v1/cluster/status");
        if let Some(direct_request) = direct_state.captured_stalled_request.lock().await.clone() {
            let direct_operation_id =
                relay_header_value(&direct_request.headers, transport_sdk::HEADER_OPERATION_ID)
                    .expect("direct request should carry an operation id");
            let direct_nonce =
                relay_header_value(&direct_request.headers, transport_sdk::HEADER_AUTH_NONCE)
                    .expect("direct request should carry an auth nonce");
            let relay_nonce =
                relay_header_value(&relay_request.headers, transport_sdk::HEADER_AUTH_NONCE)
                    .expect("relay request should carry an auth nonce");
            assert_eq!(direct_request.method, "POST");
            assert_eq!(direct_request.path_and_query, "/api/v1/cluster/status");
            assert_eq!(direct_operation_id, relay_operation_id);
            assert_ne!(direct_nonce, relay_nonce);
        }
        assert!(
            matches!(
                direct_state.cluster_status_hits.load(Ordering::SeqCst),
                1 | 2
            ),
            "expected direct cluster status hits to reflect the initial GET and at most one stalled POST"
        );
        assert!(direct_state.stalled_request_count.load(Ordering::SeqCst) <= 1);
        assert!(client.uses_relay_transport());
        assert_eq!(client.relay_target_node_id(), Some(target_node_id));
        assert_eq!(relay_state.paired_session_count.load(Ordering::SeqCst), 1);

        Ok::<(), anyhow::Error>(())
    }
    .await;

    direct_server.abort();
    let _ = direct_server.await;
    relay_server.abort();
    let _ = relay_server.await;

    test_result.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn buffered_foreground_request_uses_best_scored_validated_route() {
    let (slower_url, slower_state, slower_server) =
        spawn_direct_http_route_server(0, "slower").await;
    let (challenger_url, challenger_state, challenger_server) =
        spawn_direct_http_route_server(0, "challenger").await;
    let client = IronMeshClient::combine(vec![
        IronMeshClient::from_direct_base_url(slower_url),
        IronMeshClient::from_direct_base_url(challenger_url),
    ])
    .expect("combined direct client should build");
    let slower_endpoint = client
        .transport_router
        .endpoint(0)
        .expect("slower endpoint should exist");
    let challenger_endpoint = client
        .transport_router
        .endpoint(1)
        .expect("challenger endpoint should exist");
    let previous_use = unix_ts_ms();
    {
        let mut state = lock_endpoint_state(&slower_endpoint.state);
        record_endpoint_success_sample(&mut state, 600.0, 0, false);
        state.last_used_unix_ms = Some(previous_use);
    }
    {
        let mut state = lock_endpoint_state(&challenger_endpoint.state);
        record_endpoint_success_sample(&mut state, 1.0, 0, false);
    }
    assert_eq!(client.transport_router.rank_indices()[0], 1);
    assert_eq!(
        client.transport_router.foreground_route_indices(),
        vec![1, 0]
    );
    let response = client
        .get_json_path("/cluster/status")
        .await
        .expect("foreground request should use the best-scored route");
    assert_eq!(response["route"], "challenger");
    assert_eq!(slower_state.cluster_status_hits.load(Ordering::SeqCst), 0);
    assert_eq!(
        challenger_state.cluster_status_hits.load(Ordering::SeqCst),
        1
    );
    let snapshot = client.connection_route_snapshot();
    assert_eq!(snapshot.endpoints[0].last_used_unix_ms, Some(previous_use));
    assert!(snapshot.endpoints[1].last_used_unix_ms.is_some());

    slower_server.abort();
    let _ = slower_server.await;
    challenger_server.abort();
    let _ = challenger_server.await;
}

#[test]
fn probation_is_transport_agnostic_and_clears_after_successful_probe() {
    let relay_security = RelayTestSecurity::new();
    let direct_identity = relay_test_identity(&relay_security, "direct-probation-test");

    // Direct routes do not get special treatment: an unvalidated direct route
    // cannot preempt a validated relay even though quality scoring puts
    // the direct route first.
    let relay = relay_test_client_for_public_url(
        "http://127.0.0.1:9",
        &relay_security,
        direct_identity.clone(),
    );
    let direct = IronMeshClient::from_direct_base_url("http://127.0.0.1:18080")
        .with_client_identity(direct_identity.clone());
    let relay_active = IronMeshClient::combine(vec![relay, direct])
        .expect("combined relay+direct client should build");
    let relay_endpoint = relay_active
        .transport_router
        .endpoint(0)
        .expect("relay endpoint should exist");
    let direct_endpoint = relay_active
        .transport_router
        .endpoint(1)
        .expect("direct endpoint should exist");
    assert_eq!(
        relay_active.transport_router.foreground_route_indices(),
        relay_active.transport_router.rank_indices(),
        "probationary routes must remain available while no route is validated"
    );
    {
        let mut state = lock_endpoint_state(&relay_endpoint.state);
        record_endpoint_success_sample(&mut state, 120.0, 0, false);
    }
    assert_eq!(
        lock_endpoint_state(&direct_endpoint.state).validation,
        RouteValidationState::Probation
    );
    assert_eq!(relay_active.transport_router.rank_indices()[0], 1);
    assert_eq!(
        relay_active.transport_router.foreground_route_indices(),
        vec![0, 1]
    );
    relay_active
        .transport_router
        .record_background_probe_successes(1, &[20.0]);
    assert_eq!(
        lock_endpoint_state(&direct_endpoint.state).validation,
        RouteValidationState::Validated
    );
    assert_eq!(
        relay_active.transport_router.foreground_route_indices(),
        vec![1, 0]
    );

    // The same probation rule applies to relay routes. Give the validated direct
    // route an intentionally poor score so the unvalidated relay would win the
    // old score-only foreground policy.
    let relay_identity = relay_test_identity(&relay_security, "relay-probation-test");
    let direct = IronMeshClient::from_direct_base_url("http://127.0.0.1:28080")
        .with_client_identity(relay_identity.clone());
    let relay = relay_test_client_for_public_url(
        "http://127.0.0.1:9",
        &relay_security,
        relay_identity.clone(),
    );
    let direct_active = IronMeshClient::combine(vec![direct, relay])
        .expect("combined direct+relay client should build");
    let direct_endpoint = direct_active
        .transport_router
        .endpoint(0)
        .expect("direct endpoint should exist");
    let relay_endpoint = direct_active
        .transport_router
        .endpoint(1)
        .expect("relay endpoint should exist");
    {
        let mut state = lock_endpoint_state(&direct_endpoint.state);
        record_endpoint_success_sample(&mut state, 1_000.0, 0, false);
    }
    assert_eq!(
        lock_endpoint_state(&relay_endpoint.state).validation,
        RouteValidationState::Probation
    );
    assert_eq!(direct_active.transport_router.rank_indices()[0], 1);
    assert_eq!(
        direct_active.transport_router.foreground_route_indices(),
        vec![0, 1]
    );
    direct_active
        .transport_router
        .record_background_probe_successes(1, &[10.0]);
    assert_eq!(
        lock_endpoint_state(&relay_endpoint.state).validation,
        RouteValidationState::Validated
    );
    assert_eq!(
        direct_active.transport_router.foreground_route_indices(),
        vec![1, 0]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_probe_reprioritizes_recovered_direct_endpoint() {
    let reserved_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let primary_addr = reserved_listener
        .local_addr()
        .expect("listener should have addr");
    drop(reserved_listener);

    let primary_url = format!("http://{primary_addr}");
    let (fallback_url, fallback_state, fallback_server) =
        spawn_direct_http_route_server(125, "fallback").await;

    let primary = IronMeshClient::from_direct_base_url(primary_url.clone());
    let fallback = IronMeshClient::from_direct_base_url(fallback_url.clone());
    let client = IronMeshClient::combine(vec![primary, fallback])
        .expect("combined direct client should build");

    let first = client
        .get_json_path("/cluster/status")
        .await
        .expect("first request should fall back to the healthy route");
    assert_eq!(first["route"], "fallback");
    assert_eq!(
        client.direct_server_base_url().as_deref(),
        Some(fallback_url.as_str())
    );

    let (_primary_url, primary_state, primary_server) =
        spawn_direct_http_route_server_at(primary_addr, 0, "primary").await;

    tokio::time::sleep(Duration::from_millis(
        CLIENT_ROUTE_CIRCUIT_BASE_BACKOFF_MS + 100,
    ))
    .await;

    let second = client
        .get_json_path("/cluster/status")
        .await
        .expect("second request should still use the current fallback route");
    assert_eq!(second["route"], "fallback");

    tokio::time::sleep(Duration::from_millis(200)).await;

    let third = client
        .get_json_path("/cluster/status")
        .await
        .expect("third request should attempt the reprobed route");
    assert!(matches!(
        third["route"].as_str(),
        Some("fallback" | "primary")
    ));
    assert!(primary_state.health_hits.load(Ordering::SeqCst) >= 1);
    assert!(fallback_state.cluster_status_hits.load(Ordering::SeqCst) >= 2);

    primary_server.abort();
    let _ = primary_server.await;
    fallback_server.abort();
    let _ = fallback_server.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_connection_route_snapshot_times_out_stalled_probe() {
    let stalled_delay_ms = duration_to_u64_ms(CLIENT_ROUTE_INITIAL_BACKGROUND_PROBE_TIMEOUT)
        .expect("probe timeout should fit into u64")
        + 250;
    let (stalled_url, stalled_state, stalled_server) =
        spawn_direct_http_route_server(stalled_delay_ms, "stalled").await;
    let (healthy_url, healthy_state, healthy_server) =
        spawn_direct_http_route_server(0, "healthy").await;

    let client = IronMeshClient::combine(vec![
        IronMeshClient::from_direct_base_url(stalled_url.clone()),
        IronMeshClient::from_direct_base_url(healthy_url.clone()),
    ])
    .expect("combined direct client should build");

    let snapshot = tokio::time::timeout(
        CLIENT_ROUTE_INITIAL_BACKGROUND_PROBE_TIMEOUT + Duration::from_secs(1),
        client.refresh_connection_route_snapshot(),
    )
    .await
    .expect("route snapshot refresh should not hang on a stalled health probe");

    let stalled_endpoint = snapshot
        .endpoints
        .iter()
        .find(|endpoint| endpoint.locator == stalled_url)
        .expect("stalled endpoint should appear in the snapshot");
    let healthy_endpoint = snapshot
        .endpoints
        .iter()
        .find(|endpoint| endpoint.locator == healthy_url)
        .expect("healthy endpoint should appear in the snapshot");

    assert_eq!(stalled_state.health_hits.load(Ordering::SeqCst), 1);
    assert_eq!(
        healthy_state.health_hits.load(Ordering::SeqCst),
        CLIENT_ROUTE_BACKGROUND_PROBE_WARMUP_COUNT + CLIENT_ROUTE_BACKGROUND_PROBE_SAMPLE_COUNT
    );
    assert_eq!(stalled_endpoint.total_failures, 1);
    assert!(!stalled_endpoint.background_probe_in_flight);
    assert!(
        stalled_endpoint
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("timed out")),
        "stalled endpoint should record a timeout error, got {:?}",
        stalled_endpoint.last_error
    );
    assert_eq!(
        healthy_endpoint.total_successes,
        CLIENT_ROUTE_BACKGROUND_PROBE_SAMPLE_COUNT as u64
    );
    assert!(healthy_endpoint.last_error.is_none());

    stalled_server.abort();
    let _ = stalled_server.await;
    healthy_server.abort();
    let _ = healthy_server.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn due_route_refresh_reprobes_only_stale_non_preferred_endpoint_with_warm_samples() {
    let (preferred_url, preferred_state, preferred_server) =
        spawn_direct_http_route_server(0, "preferred").await;
    let (inactive_url, inactive_state, inactive_server) =
        spawn_direct_http_route_server(0, "inactive").await;
    let client = IronMeshClient::combine(vec![
        IronMeshClient::from_direct_base_url(preferred_url),
        IronMeshClient::from_direct_base_url(inactive_url),
    ])
    .expect("combined direct client should build");

    let preferred_endpoint = client
        .transport_router
        .endpoint(0)
        .expect("preferred endpoint should exist");
    let inactive_endpoint = client
        .transport_router
        .endpoint(1)
        .expect("inactive endpoint should exist");
    {
        let mut state = lock_endpoint_state(&preferred_endpoint.state);
        record_endpoint_success_sample(&mut state, 1.0, 0, false);
    }
    {
        let mut state = lock_endpoint_state(&inactive_endpoint.state);
        record_endpoint_success_sample(&mut state, 12.0, 0, false);
    }

    client.refresh_due_connection_route_snapshot().await;
    assert_eq!(preferred_state.health_hits.load(Ordering::SeqCst), 0);
    assert_eq!(inactive_state.health_hits.load(Ordering::SeqCst), 0);

    {
        let mut state = lock_endpoint_state(&inactive_endpoint.state);
        state.last_measurement_unix_ms =
            Some(unix_ts_ms().saturating_sub(CLIENT_ROUTE_BACKGROUND_REFRESH_STALE_MS));
    }
    let snapshot = client.refresh_due_connection_route_snapshot().await;

    assert_eq!(preferred_state.health_hits.load(Ordering::SeqCst), 0);
    assert_eq!(
        inactive_state.health_hits.load(Ordering::SeqCst),
        CLIENT_ROUTE_BACKGROUND_PROBE_WARMUP_COUNT + CLIENT_ROUTE_BACKGROUND_PROBE_SAMPLE_COUNT
    );
    assert_eq!(
        snapshot.endpoints[1].total_successes,
        1 + CLIENT_ROUTE_BACKGROUND_PROBE_SAMPLE_COUNT as u64
    );

    preferred_server.abort();
    let _ = preferred_server.await;
    inactive_server.abort();
    let _ = inactive_server.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_probe_reprioritizes_recovered_relay_endpoint() {
    let primary_listener = Arc::new(
        tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind"),
    );
    let primary_addr = primary_listener
        .local_addr()
        .expect("listener should have addr");
    let placeholder_listener = Arc::clone(&primary_listener);
    let placeholder_server = tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = placeholder_listener.accept().await else {
                return;
            };
            drop(socket);
        }
    });

    let primary_url = format!("http://{primary_addr}");
    let fallback_body = serde_json::to_vec(&serde_json::json!({
        "status": "ok",
        "route": "fallback",
    }))
    .expect("fallback relay body should serialize");
    let (fallback_state, fallback_server) = spawn_relay_test_server_with_delay(
        200,
        vec![
            RelayHttpHeader {
                name: "content-type".to_string(),
                value: "application/json".to_string(),
            },
            RelayHttpHeader {
                name: "content-length".to_string(),
                value: fallback_body.len().to_string(),
            },
        ],
        fallback_body,
        750,
    )
    .await;

    let identity = relay_test_identity(&fallback_state.security, "relay-background-refresh-device");
    let primary_security =
        RelayTestSecurity::for_cluster(fallback_state.security.cluster_id, NodeId::new_v4());
    let _primary_target_node_id = primary_security.target_node_id;
    let fallback_target_node_id = fallback_state.security.target_node_id;
    let primary =
        relay_test_client_for_public_url(primary_url.clone(), &primary_security, identity.clone());
    let fallback = relay_test_client(&fallback_state, identity.clone());
    let client = IronMeshClient::combine(vec![primary, fallback])
        .expect("combined relay client should build");

    let first = client
        .get_json_path("/cluster/status")
        .await
        .expect("first request should fall back to the healthy relay route");
    assert_eq!(first["route"], "fallback");
    assert_eq!(client.relay_target_node_id(), Some(fallback_target_node_id));
    assert!(client.uses_relay_transport());

    let primary_body = serde_json::to_vec(&serde_json::json!({
        "status": "ok",
        "route": "primary",
    }))
    .expect("primary relay body should serialize");
    placeholder_server.abort();
    let _ = placeholder_server.await;
    let primary_listener = Arc::try_unwrap(primary_listener)
        .unwrap_or_else(|_| panic!("placeholder should release the primary listener"));
    let (primary_state, primary_server) = spawn_relay_test_server_on_listener_with_security(
        primary_listener,
        200,
        vec![
            RelayHttpHeader {
                name: "content-type".to_string(),
                value: "application/json".to_string(),
            },
            RelayHttpHeader {
                name: "content-length".to_string(),
                value: primary_body.len().to_string(),
            },
        ],
        primary_body,
        0,
        0,
        primary_security,
    )
    .await;

    tokio::time::sleep(Duration::from_millis(
        CLIENT_ROUTE_CIRCUIT_BASE_BACKOFF_MS + 100,
    ))
    .await;

    let second = client
        .get_json_path("/cluster/status")
        .await
        .expect("second request after backoff should succeed");
    assert!(matches!(
        second["route"].as_str(),
        Some("fallback" | "primary")
    ));

    // Allow up to 10s: the probe fires immediately on success, but if it fails
    // transiently the min-interval (5000ms) gates the retry, leaving only a
    // narrow window before the test would time out at 5s.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if primary_state.health_hits.load(Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("background probe should hit the recovered relay route");

    let third = client
        .get_json_path("/cluster/status")
        .await
        .expect("request after background probe should succeed");
    assert!(matches!(
        third["route"].as_str(),
        Some("fallback" | "primary")
    ));
    assert!(primary_state.health_hits.load(Ordering::SeqCst) >= 1);
    assert!(fallback_state.paired_session_count.load(Ordering::SeqCst) >= 1);

    primary_server.abort();
    let _ = primary_server.await;
    fallback_server.abort();
    let _ = fallback_server.await;
}

#[tokio::test]
async fn direct_transport_executes_relative_path_get_request() {
    let (direct_state, server) = spawn_direct_transport_test_server(
        200,
        vec![
            RelayHttpHeader {
                name: "content-type".to_string(),
                value: "image/jpeg".to_string(),
            },
            RelayHttpHeader {
                name: "content-length".to_string(),
                value: b"thumb-jpeg-bytes".len().to_string(),
            },
        ],
        b"thumb-jpeg-bytes".to_vec(),
    )
    .await;

    let mut identity = ClientIdentityMaterial::generate(
        uuid::Uuid::now_v7(),
        None,
        Some("direct-relative-path-device".to_string()),
    )
    .expect("identity should generate");
    identity.credential_pem = Some("issued-credential".to_string());
    let client = direct_transport_test_client(&direct_state, identity.clone());

    let response = client
        .get_relative_path("/media/thumbnail?key=gallery%2Fcat.png")
        .await
        .expect("relative GET over direct transport should succeed");

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body.as_ref(), b"thumb-jpeg-bytes");

    let captured = direct_state
        .captured_request
        .lock()
        .await
        .clone()
        .expect("direct request should be captured");
    assert_eq!(
        captured.path_and_query,
        "/api/v1/media/thumbnail?key=gallery%2Fcat.png"
    );
    assert!(
        captured
            .headers
            .iter()
            .any(|header| header.name == transport_sdk::HEADER_DEVICE_ID
                && header.value == identity.device_id.to_string())
    );

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn direct_transport_preserves_head_response_headers() {
    let payload = b"head-only-payload";
    let (direct_state, server) = spawn_direct_transport_test_server(
        200,
        vec![
            RelayHttpHeader {
                name: ACCEPT_RANGES.as_str().to_string(),
                value: "bytes".to_string(),
            },
            RelayHttpHeader {
                name: CONTENT_LENGTH.as_str().to_string(),
                value: payload.len().to_string(),
            },
            RelayHttpHeader {
                name: ETAG.as_str().to_string(),
                value: "\"direct-head-etag\"".to_string(),
            },
        ],
        Vec::new(),
    )
    .await;

    let mut identity = ClientIdentityMaterial::generate(
        uuid::Uuid::now_v7(),
        None,
        Some("direct-head-device".to_string()),
    )
    .expect("identity should generate");
    identity.credential_pem = Some("issued-credential".to_string());
    let client = direct_transport_test_client(&direct_state, identity);

    let response = client
        .head_object("gallery/cat.png", None, None)
        .await
        .expect("HEAD over direct transport should succeed");

    assert_eq!(response.total_size_bytes, payload.len() as u64);
    assert!(response.accept_ranges);
    assert_eq!(response.etag.as_deref(), Some("\"direct-head-etag\""));

    let captured = direct_state
        .captured_request
        .lock()
        .await
        .clone()
        .expect("direct request should be captured");
    assert_eq!(captured.method, "HEAD");
    assert_eq!(captured.path_and_query, "/api/v1/store/gallery%2Fcat.png");

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn direct_transport_head_error_includes_endpoint_and_target_node_id() {
    let (direct_state, server) =
        spawn_direct_transport_test_server(404, Vec::new(), Vec::new()).await;

    let mut identity = ClientIdentityMaterial::generate(
        uuid::Uuid::now_v7(),
        None,
        Some("direct-head-error-device".to_string()),
    )
    .expect("identity should generate");
    identity.credential_pem = Some("issued-credential".to_string());
    let target_node_id = NodeId::new_v4();
    let client = IronMeshClient::from_direct_http_client_with_target_node_id_and_ca_pem(
        direct_state.public_url.clone(),
        HttpClient::new(),
        Some(target_node_id),
        None,
        None,
    )
    .with_client_identity(identity);

    let error = client
        .head_object("gallery/missing photo.jpg", None, None)
        .await
        .expect_err("missing object should return an error");
    let message = error.to_string();
    assert!(
        message.contains("404 Not Found"),
        "unexpected error: {message}"
    );
    assert!(
        message.contains(&format!("endpoint_locator={}", direct_state.public_url)),
        "missing endpoint locator: {message}"
    );
    assert!(
        message.contains(&format!("target_node_id={target_node_id}")),
        "missing target node ID: {message}"
    );

    let captured = direct_state
        .captured_request
        .lock()
        .await
        .clone()
        .expect("direct request should be captured");
    assert_eq!(captured.method, "HEAD");
    assert_eq!(
        captured.path_and_query,
        "/api/v1/store/gallery%2Fmissing%20photo.jpg"
    );

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn direct_transport_retryable_head_error_includes_endpoint_and_target_node_id() {
    let (direct_state, server) =
        spawn_direct_transport_test_server(503, Vec::new(), Vec::new()).await;

    let mut identity = ClientIdentityMaterial::generate(
        uuid::Uuid::now_v7(),
        None,
        Some("direct-retryable-head-error-device".to_string()),
    )
    .expect("identity should generate");
    identity.credential_pem = Some("issued-credential".to_string());
    let target_node_id = NodeId::new_v4();
    let client = IronMeshClient::from_direct_http_client_with_target_node_id_and_ca_pem(
        direct_state.public_url.clone(),
        HttpClient::new(),
        Some(target_node_id),
        None,
        None,
    )
    .with_client_identity(identity);

    let error = client
        .head_object("gallery/retryable.jpg", None, None)
        .await
        .expect_err("retryable object response should return an error");
    let message = error.to_string();
    assert!(
        message.contains("503 Service Unavailable"),
        "unexpected error: {message}"
    );
    assert!(
        message.contains(&format!("endpoint_locator={}", direct_state.public_url)),
        "missing endpoint locator: {message}"
    );
    assert!(
        message.contains(&format!("target_node_id={target_node_id}")),
        "missing target node ID: {message}"
    );

    let attempt = client
        .connection_diagnostics()
        .endpoints
        .into_iter()
        .flat_map(|endpoint| endpoint.recent_attempts)
        .last()
        .expect("retryable response should retain a failed request attempt");
    assert_eq!(attempt.outcome, "failure");
    assert_eq!(attempt.status_code, Some(503));

    let captured = direct_state
        .captured_request
        .lock()
        .await
        .clone()
        .expect("direct request should be captured");
    assert_eq!(captured.method, "HEAD");
    assert_eq!(
        captured.path_and_query,
        "/api/v1/store/gallery%2Fretryable.jpg"
    );

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn direct_transport_diagnostics_preserve_contextualized_error_chain() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let unavailable_url = format!(
        "http://{}",
        listener
            .local_addr()
            .expect("listener should have an address")
    );
    drop(listener);

    let mut identity = ClientIdentityMaterial::generate(
        uuid::Uuid::now_v7(),
        None,
        Some("direct-diagnostics-device".to_string()),
    )
    .expect("identity should generate");
    identity.credential_pem = Some("issued-credential".to_string());
    let target_node_id = NodeId::new_v4();
    let client = IronMeshClient::from_direct_http_client_with_target_node_id_and_ca_pem(
        unavailable_url.clone(),
        HttpClient::new(),
        Some(target_node_id),
        None,
        None,
    )
    .with_client_identity(identity);

    client
        .head_object("gallery/unavailable.jpg", None, None)
        .await
        .expect_err("unavailable direct endpoint should fail");

    let diagnostics = client.connection_diagnostics();
    let endpoint = diagnostics
        .endpoints
        .first()
        .expect("direct endpoint diagnostics should exist");
    let last_error = endpoint
        .last_error
        .as_deref()
        .expect("direct endpoint should record its failure");
    assert!(last_error.contains(&format!("endpoint_locator={unavailable_url}")));
    assert!(last_error.contains("failed to execute multiplexed HEAD"));

    let attempt_error = endpoint
        .recent_attempts
        .last()
        .and_then(|attempt| attempt.error.as_deref())
        .expect("request attempt should retain its failure");
    assert!(attempt_error.contains(&format!("endpoint_locator={unavailable_url}")));
    assert!(attempt_error.contains("failed to execute multiplexed HEAD"));
}

#[tokio::test]
async fn direct_transport_streams_upload_session_chunks_over_object_write() {
    let response_body = serde_json::to_vec(&UploadSessionChunkResponse {
        stored: true,
        received_index: 3,
    })
    .expect("upload chunk response should serialize");
    let (direct_state, server) = spawn_direct_transport_test_server(
        200,
        vec![
            RelayHttpHeader {
                name: "content-type".to_string(),
                value: "application/json".to_string(),
            },
            RelayHttpHeader {
                name: "content-length".to_string(),
                value: response_body.len().to_string(),
            },
        ],
        response_body,
    )
    .await;

    let mut identity = ClientIdentityMaterial::generate(
        uuid::Uuid::now_v7(),
        None,
        Some("direct-upload-test-device".to_string()),
    )
    .expect("identity should generate");
    identity.credential_pem = Some("issued-credential".to_string());
    let client = direct_transport_test_client(&direct_state, identity);

    let response = client
        .upload_session_chunk_bytes("upload-abc", 3, b"direct-chunk".to_vec())
        .await
        .expect("direct upload chunk should succeed");

    assert!(response.stored);
    assert_eq!(response.received_index, 3);

    let captured = direct_state
        .captured_request
        .lock()
        .await
        .clone()
        .expect("direct request should be captured");
    assert_eq!(captured.kind, Some(TransportStreamKind::ObjectWrite));
    assert_eq!(captured.method, "PUT");
    assert_eq!(
        captured.path_and_query,
        "/api/v1/store/uploads/upload-abc/chunk/3"
    );
    assert_eq!(captured.body, b"direct-chunk".to_vec());

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn direct_transport_keeps_small_rpcs_responsive_during_streamed_downloads() {
    let payload = Arc::new(vec![0x5A; 1024 * 1024]);
    let payload_len = payload.len();
    let (base_url, server) = spawn_direct_mixed_workload_test_server(Arc::clone(&payload)).await;

    let mut identity = ClientIdentityMaterial::generate(
        uuid::Uuid::now_v7(),
        None,
        Some("direct-mixed-workload-device".to_string()),
    )
    .expect("identity should generate");
    identity.credential_pem = Some("issued-credential".to_string());
    let client = IronMeshClient::from_direct_base_url(base_url).with_client_identity(identity);

    let download_client = client.clone();
    let download_future = async move {
        let mut output = Vec::new();
        let mut progress = Vec::new();
        let mut on_progress = |update: DownloadProgress| {
            progress.push(update);
        };
        let result = download_client
            .download_range_to_writer_with_progress(
                DownloadRangeRequest {
                    key: "large.bin",
                    snapshot: None,
                    version: None,
                    range: RequestedRange {
                        offset: 0,
                        length: payload_len as u64,
                    },
                },
                &mut output,
                &mut on_progress,
                &|| false,
            )
            .await
            .expect("streamed download should succeed");
        (output, progress, result)
    };
    let rpc_future = async {
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        tokio::time::timeout(
            std::time::Duration::from_millis(1000),
            client.get_json_path("/cluster/status"),
        )
        .await
        .expect("small RPC should not be blocked behind streamed download")
        .expect("small RPC should succeed")
    };
    let ((output, progress, result), rpc_response) = tokio::join!(download_future, rpc_future);

    assert_eq!(rpc_response["status"], "ok");
    assert_eq!(output.len(), payload_len);
    assert_eq!(result.bytes_downloaded, payload_len as u64);
    assert!(
        progress
            .last()
            .is_some_and(|entry| entry.bytes_downloaded == payload_len as u64)
    );
    let snapshot = client.transport_session_pool_snapshot();
    assert_eq!(snapshot.connect_count, 1);
    assert!(snapshot.reuse_count >= 2);

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn direct_transport_streams_relative_s3_reads_without_blocking_small_rpcs() {
    let payload = Arc::new(vec![0x7B; 1024 * 1024]);
    let payload_len = payload.len();
    let (base_url, server) = spawn_direct_mixed_workload_test_server(Arc::clone(&payload)).await;

    let mut identity = ClientIdentityMaterial::generate(
        uuid::Uuid::now_v7(),
        None,
        Some("direct-stream-relative-s3-device".to_string()),
    )
    .expect("identity should generate");
    identity.credential_pem = Some("issued-credential".to_string());
    let client = IronMeshClient::from_direct_base_url(base_url).with_client_identity(identity);

    let download_client = client.clone();
    let download_future = async move {
        let mut response = download_client
            .request_relative_path_streaming_response(
                Method::GET,
                "/s3/photos.example/docs/streamed.txt",
                Vec::new(),
            )
            .await
            .expect("streamed relative S3 read should succeed");
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(
            response
                .headers
                .get(ETAG)
                .and_then(|value| value.to_str().ok()),
            Some("\"s3-streamed-etag\"")
        );

        let mut output = Vec::new();
        while let Some(chunk) = response.body.next().await {
            let chunk = chunk.expect("streamed relative S3 body chunk should succeed");
            output.extend_from_slice(chunk.as_ref());
        }
        output
    };
    let rpc_future = async {
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        tokio::time::timeout(
            std::time::Duration::from_millis(1000),
            client.get_json_path("/cluster/status"),
        )
        .await
        .expect("small RPC should not be blocked behind streamed relative S3 read")
        .expect("small RPC should succeed")
    };
    let (output, rpc_response) = tokio::join!(download_future, rpc_future);

    assert_eq!(rpc_response["status"], "ok");
    assert_eq!(output.len(), payload_len);
    assert_eq!(output, payload.as_ref().to_vec());
    let snapshot = client.transport_session_pool_snapshot();
    assert_eq!(snapshot.connect_count, 1);
    assert!(snapshot.reuse_count >= 1);

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn relay_transport_streams_relative_s3_reads_without_blocking_small_rpcs() {
    let payload = Arc::new(vec![0x7B; 1024 * 1024]);
    let payload_len = payload.len();
    let (relay_state, server) = spawn_relay_mixed_workload_test_server(Arc::clone(&payload)).await;

    let identity = relay_test_identity(&relay_state.security, "relay-stream-relative-s3-device");
    let client = relay_test_client_for_public_url(
        relay_state.public_url.clone(),
        &relay_state.security,
        identity,
    );

    let download_client = client.clone();
    let download_future = async move {
        let mut response = download_client
            .request_relative_path_streaming_response(
                Method::GET,
                "/s3/photos.example/docs/streamed.txt",
                Vec::new(),
            )
            .await
            .expect("relay streamed relative S3 read should succeed");
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(
            response
                .headers
                .get(ETAG)
                .and_then(|value| value.to_str().ok()),
            Some("\"s3-streamed-etag\"")
        );

        let mut output = Vec::new();
        while let Some(chunk) = response.body.next().await {
            let chunk = chunk.expect("relay streamed relative S3 body chunk should succeed");
            output.extend_from_slice(chunk.as_ref());
        }
        output
    };
    let rpc_future = async {
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        tokio::time::timeout(
            std::time::Duration::from_millis(1000),
            client.get_json_path("/cluster/status"),
        )
        .await
        .expect("small RPC should not be blocked behind relay streamed relative S3 read")
        .expect("small RPC should succeed")
    };
    let (output, rpc_response) = tokio::join!(download_future, rpc_future);

    assert_eq!(rpc_response["status"], "ok");
    assert_eq!(output.len(), payload_len);
    assert_eq!(output, payload.as_ref().to_vec());
    assert_eq!(relay_state.issued_ticket_count.load(Ordering::SeqCst), 1);
    assert_eq!(relay_state.paired_session_count.load(Ordering::SeqCst), 1);

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn direct_transport_cancels_streamed_download_promptly() {
    let payload = Arc::new(vec![0x3C; 1024 * 1024]);
    let payload_len = payload.len();
    let (base_url, server) = spawn_direct_mixed_workload_test_server(Arc::clone(&payload)).await;

    let mut identity = ClientIdentityMaterial::generate(
        uuid::Uuid::now_v7(),
        None,
        Some("direct-cancel-download-device".to_string()),
    )
    .expect("identity should generate");
    identity.credential_pem = Some("issued-credential".to_string());
    let client = IronMeshClient::from_direct_base_url(base_url).with_client_identity(identity);

    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cancel_for_task = Arc::clone(&cancel);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        cancel_for_task.store(true, Ordering::SeqCst);
    });

    let mut output = Vec::new();
    let result = client
        .download_range_to_writer_with_progress(
            DownloadRangeRequest {
                key: "large.bin",
                snapshot: None,
                version: None,
                range: RequestedRange {
                    offset: 0,
                    length: payload_len as u64,
                },
            },
            &mut output,
            &mut |_| {},
            &|| cancel.load(Ordering::SeqCst),
        )
        .await;

    let error = result.expect_err("streamed download should cancel");
    assert!(error.to_string().contains("download canceled"));
    assert!(output.len() < payload_len);

    server.abort();
    let _ = server.await;
}

#[test]
fn blocking_downloads_handle_concurrent_range_and_staged_requests() {
    fn build_range_response(
        payload: &[u8],
        status: StatusCode,
        start: usize,
        end_inclusive: usize,
    ) -> Response<Body> {
        Response::builder()
            .status(status)
            .header("x-ironmesh-object-size", payload.len().to_string())
            .header(ETAG.as_str(), "\"test-etag\"")
            .header(ACCEPT_RANGES.as_str(), "bytes")
            .header(
                CONTENT_LENGTH.as_str(),
                (end_inclusive - start + 1).to_string(),
            )
            .header(
                CONTENT_RANGE.as_str(),
                format!("bytes {start}-{end_inclusive}/{}", payload.len()),
            )
            .body(Body::from(payload[start..=end_inclusive].to_vec()))
            .expect("range response should build")
    }

    fn parse_range_header(range: &str, total_len: usize) -> (usize, usize) {
        let trimmed = range
            .strip_prefix("bytes=")
            .expect("range header should have bytes= prefix");
        let (start, end) = trimmed
            .split_once('-')
            .expect("range header should contain dash");
        let start = start.parse::<usize>().expect("range start should parse");
        let end = end.parse::<usize>().expect("range end should parse");
        assert!(start <= end, "range start must not exceed end");
        assert!(end < total_len, "range end must stay within payload");
        (start, end)
    }

    async fn head_store(
        State(payload): State<Arc<Vec<u8>>>,
        AxumPath(_key): AxumPath<String>,
    ) -> Response<Body> {
        Response::builder()
            .status(StatusCode::OK)
            .header("x-ironmesh-object-size", payload.len().to_string())
            .header(ETAG.as_str(), "\"test-etag\"")
            .header(ACCEPT_RANGES.as_str(), "bytes")
            .header(CONTENT_LENGTH.as_str(), payload.len().to_string())
            .body(Body::empty())
            .expect("head response should build")
    }

    async fn get_store(
        State(payload): State<Arc<Vec<u8>>>,
        AxumPath(_key): AxumPath<String>,
        headers: HeaderMap,
    ) -> Response<Body> {
        tokio::time::sleep(Duration::from_millis(20)).await;

        match headers.get(RANGE).and_then(|value| value.to_str().ok()) {
            Some(range) => {
                let (start, end_inclusive) = parse_range_header(range, payload.len());
                build_range_response(&payload, StatusCode::PARTIAL_CONTENT, start, end_inclusive)
            }
            None => Response::builder()
                .status(StatusCode::OK)
                .header("x-ironmesh-object-size", payload.len().to_string())
                .header(ETAG.as_str(), "\"test-etag\"")
                .header(ACCEPT_RANGES.as_str(), "bytes")
                .header(header::CONTENT_LENGTH, payload.len().to_string())
                .body(Body::from(payload.as_ref().clone()))
                .expect("full response should build"),
        }
    }

    let payload = Arc::new(
        (0..200_000)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>(),
    );

    let app = Router::new()
        .route("/api/v1/store/{*key}", get(get_store).head(head_store))
        .with_state(payload.clone());
    let (addr_tx, addr_rx) = std::sync::mpsc::sync_channel(1);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server_thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("server runtime should build");
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("listener should bind");
            addr_tx
                .send(listener.local_addr().expect("listener should have addr"))
                .expect("server addr should send");
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("range server should run");
        });
    });

    let addr = addr_rx.recv().expect("server addr should arrive");
    let client = IronMeshClient::from_direct_base_url(format!("http://{addr}"));
    let requests = [
        (0_u64, 65_536_u64),
        (65_536_u64, 4_096_u64),
        (69_632_u64, 61_440_u64),
        (131_072_u64, payload.len() as u64 - 131_072_u64),
    ];

    for _round in 0..8 {
        let barrier = Arc::new(Barrier::new(requests.len()));
        let mut handles = Vec::new();
        for (start, length) in requests {
            let client = client.clone();
            let barrier = barrier.clone();
            let expected = payload[start as usize..(start + length) as usize].to_vec();
            handles.push(std::thread::spawn(move || {
                let mut writer = Vec::new();
                let mut progress_updates = Vec::new();
                barrier.wait();
                let result = client
                    .download_range_to_writer_with_progress_blocking(
                        DownloadRangeRequest {
                            key: "photos/test.jpg",
                            snapshot: None,
                            version: None,
                            range: RequestedRange {
                                offset: start,
                                length,
                            },
                        },
                        &mut writer,
                        &mut |progress| progress_updates.push(progress),
                        &|| false,
                    )
                    .expect("blocking ranged download should succeed");
                assert_eq!(writer, expected);
                assert_eq!(result.range.offset, start);
                assert_eq!(result.range.length, length);
                assert_eq!(result.bytes_downloaded, length);
                assert!(
                    progress_updates
                        .last()
                        .is_some_and(|progress| progress.bytes_downloaded == length),
                    "final progress update should report the completed byte count",
                );
            }));
        }

        for handle in handles {
            handle.join().expect("download worker should complete");
        }
    }

    assert_concurrent_staged_downloads(&client, &payload);

    let _ = shutdown_tx.send(());
    server_thread.join().expect("server thread should stop");
}

fn assert_concurrent_staged_downloads(client: &IronMeshClient, payload: &Arc<Vec<u8>>) {
    let staging_root = std::env::temp_dir().join(format!(
        "ironmesh-concurrent-staged-download-{}",
        Uuid::new_v4()
    ));
    fs::create_dir_all(&staging_root).expect("staging root should be created");
    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();

    for _ in 0..2 {
        let client = client.clone();
        let staging_root = staging_root.clone();
        let barrier = barrier.clone();
        handles.push(std::thread::spawn(move || -> Result<Vec<u8>> {
            let mut writer = Vec::new();
            barrier.wait();
            client.download_to_writer_resumable_staged(
                "photos/test.jpg",
                None,
                None,
                &mut writer,
                &staging_root,
            )?;
            Ok(writer)
        }));
    }

    let results = handles
        .into_iter()
        .map(|handle| {
            handle
                .join()
                .expect("staged download worker should complete")
        })
        .collect::<Vec<_>>();
    fs::remove_dir_all(&staging_root).expect("staging root should be removed");

    for result in results {
        assert_eq!(
            result.expect("concurrent staged download should succeed"),
            payload.as_ref().clone()
        );
    }
}
