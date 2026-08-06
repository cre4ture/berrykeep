#![cfg(target_os = "linux")]

#[path = "quic_network/fault_rendezvous.rs"]
mod fault_rendezvous;
#[path = "quic_network/process.rs"]
mod process;
#[path = "quic_network/runtime.rs"]
mod runtime;
#[path = "quic_network/tls.rs"]
mod tls;

use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use serde_json::Value;
use tokio::time::timeout;

use runtime::ScenarioRuntime;

const SCENARIO_TIMEOUT: Duration = Duration::from_secs(120);
const RECENT_ROUTE_USE_MS: u64 = 2_000;

#[ctor::ctor(unsafe)]
fn userns_ctor() {
    patchbay::init_userns().expect("failed to initialize Patchbay user namespace");
}

#[derive(Clone, Copy, Debug)]
enum ExpectedRoute {
    DirectQuic(&'static str),
    RelayTunnel,
}

#[derive(Clone, Copy, Debug)]
enum NetworkProfile {
    /// IPv4 EIM/APDF NAT without a second firewall layer, matching Iroh's
    /// upstream `Nat::Home` hole-punch test topology.
    HolePunchableHomeNat,
    /// Symmetric IPv4 NAT with UDP blocked.
    HotelBlockedUdp,
}

#[derive(Clone, Copy, Debug)]
struct Scenario {
    name: &'static str,
    network: NetworkProfile,
    iroh_relay_enabled: bool,
    ironmesh_relay_enabled: bool,
    stall_first_iroh_ticket: bool,
    expected: ExpectedRoute,
}

impl Scenario {
    const HOME_DIRECT: Self = Self {
        name: "home-direct",
        network: NetworkProfile::HolePunchableHomeNat,
        iroh_relay_enabled: true,
        ironmesh_relay_enabled: false,
        stall_first_iroh_ticket: false,
        expected: ExpectedRoute::DirectQuic("direct"),
    };

    const HOTEL_IROH_RELAY: Self = Self {
        name: "hotel-iroh-relay",
        network: NetworkProfile::HotelBlockedUdp,
        iroh_relay_enabled: true,
        ironmesh_relay_enabled: false,
        stall_first_iroh_ticket: false,
        expected: ExpectedRoute::DirectQuic("relay"),
    };

    const HOTEL_IRONMESH_RELAY: Self = Self {
        name: "hotel-ironmesh-relay",
        network: NetworkProfile::HotelBlockedUdp,
        iroh_relay_enabled: false,
        ironmesh_relay_enabled: true,
        stall_first_iroh_ticket: false,
        expected: ExpectedRoute::RelayTunnel,
    };

    const TICKET_RACE_HEALTHY_SECOND: Self = Self {
        name: "ticket-race-healthy-second",
        network: NetworkProfile::HotelBlockedUdp,
        iroh_relay_enabled: true,
        ironmesh_relay_enabled: false,
        stall_first_iroh_ticket: true,
        expected: ExpectedRoute::DirectQuic("relay"),
    };
}

#[tokio::test(flavor = "current_thread")]
async fn home_nat_hole_punches_direct_quic() -> Result<()> {
    run_with_timeout(Scenario::HOME_DIRECT).await
}

#[tokio::test(flavor = "current_thread")]
async fn udp_blocked_uses_iroh_relay_for_direct_quic() -> Result<()> {
    run_with_timeout(Scenario::HOTEL_IROH_RELAY).await
}

#[tokio::test(flavor = "current_thread")]
async fn udp_blocked_without_iroh_relay_falls_back_to_ironmesh_relay() -> Result<()> {
    run_with_timeout(Scenario::HOTEL_IRONMESH_RELAY).await
}

#[tokio::test(flavor = "current_thread")]
async fn stalled_first_iroh_ticket_endpoint_uses_healthy_second_endpoint() -> Result<()> {
    run_with_timeout(Scenario::TICKET_RACE_HEALTHY_SECOND).await
}

async fn run_with_timeout(scenario: Scenario) -> Result<()> {
    timeout(SCENARIO_TIMEOUT, run_scenario(scenario))
        .await
        .with_context(|| format!("{} exceeded {SCENARIO_TIMEOUT:?}", scenario.name))?
}

async fn run_scenario(scenario: Scenario) -> Result<()> {
    let mut runtime = ScenarioRuntime::setup(scenario).await?;
    let result = exercise_and_assert(&mut runtime, scenario).await;
    runtime.stop().await;
    result
}

async fn exercise_and_assert(runtime: &mut ScenarioRuntime, scenario: Scenario) -> Result<()> {
    runtime.wait_for_route(scenario.expected).await?;

    let started = Instant::now();
    let store_list = runtime
        .wait_for_store_list()
        .await
        .with_context(|| format!("store/list failed in {}", scenario.name))?;
    ensure!(
        store_list.is_array() || store_list.is_object(),
        "store/list returned unexpected JSON: {store_list}"
    );
    ensure!(
        started.elapsed() < Duration::from_secs(35),
        "{} fallback exceeded the bounded request budget: {:?}",
        scenario.name,
        started.elapsed()
    );
    let route_snapshot = runtime.connection_routes().await?;
    assert_expected_route(&route_snapshot, scenario.expected).with_context(|| {
        format!(
            "unexpected routes after exercising {}\n{route_snapshot:#}",
            scenario.name
        )
    })?;

    if scenario.stall_first_iroh_ticket {
        let ticket_requests_after = runtime.ticket_request_count()?;
        ensure!(
            ticket_requests_after > 0,
            "the CLI never reached the intentionally stalled Iroh ticket endpoint"
        );
    }
    Ok(())
}

fn candidate_matches(snapshot: &Value, expected: ExpectedRoute) -> bool {
    snapshot["endpoints"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|endpoint| {
            let successes = endpoint["total_successes"].as_u64().unwrap_or_default();
            match expected {
                ExpectedRoute::DirectQuic(mode) => {
                    endpoint["path_kind"] == "direct_quic"
                        && endpoint["hole_punching_mode"] == mode
                        && successes > 0
                }
                ExpectedRoute::RelayTunnel => {
                    endpoint["path_kind"] == "relay_tunnel"
                        && endpoint_was_recently_used(snapshot, endpoint)
                        && successes > 0
                }
            }
        })
}

fn endpoint_was_recently_used(snapshot: &Value, endpoint: &Value) -> bool {
    let Some(generated_at_unix_ms) = snapshot["generated_at_unix_ms"].as_u64() else {
        return false;
    };
    let Some(last_used_unix_ms) = endpoint["last_used_unix_ms"].as_u64() else {
        return false;
    };
    generated_at_unix_ms.saturating_sub(last_used_unix_ms) <= RECENT_ROUTE_USE_MS
}

fn assert_expected_route(snapshot: &Value, expected: ExpectedRoute) -> Result<()> {
    ensure!(
        candidate_matches(snapshot, expected),
        "expected route {expected:?} was not successful"
    );
    if let ExpectedRoute::DirectQuic(mode) = expected {
        let direct = snapshot["endpoints"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|endpoint| {
                endpoint["path_kind"] == "direct_quic"
                    && endpoint["hole_punching_mode"] == mode
                    && endpoint["total_successes"].as_u64().unwrap_or_default() > 0
            })
            .context("successful Direct QUIC endpoint disappeared")?;
        ensure!(
            endpoint_was_recently_used(snapshot, direct),
            "Direct QUIC route was not used within {RECENT_ROUTE_USE_MS} ms of the CLI request: {direct:#}"
        );
        ensure!(
            direct["transport_session_pool"]["connect_count"]
                .as_u64()
                .unwrap_or_default()
                >= 1,
            "Direct QUIC route did not establish a pooled session: {direct:#}"
        );
        ensure!(
            direct["transport_session_pool"]["reuse_count"]
                .as_u64()
                .unwrap_or_default()
                >= 1,
            "Direct QUIC route did not reuse its pooled session: {direct:#}"
        );
    }
    Ok(())
}
