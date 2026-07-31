use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use client_sdk::{ConnectionBootstrap, RelayMode};
use patchbay::{Device, Lab, Nat, OutDir, Router, RouterPreset};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use tokio::time::{sleep, timeout};
use uuid::Uuid;

use super::{
    ExpectedRoute, NetworkProfile, Scenario, candidate_matches,
    fault_rendezvous::TicketTimeoutServer,
    process::{
        ADMIN_TOKEN, CLI_WEB_PORT, NODE_PUBLIC_PORT, PROCESS_READY_TIMEOUT, ProcessGuard,
        RENDEZVOUS_PORT, artifact_dir, enroll_cli, spawn_cli_web, spawn_node, spawn_rendezvous,
    },
    tls::write_node_tls,
};

const ROUTE_READY_TIMEOUT: Duration = Duration::from_secs(50);
const STORE_READY_TIMEOUT: Duration = Duration::from_secs(35);
const DEVICE_REQUEST_TIMEOUT: Duration = Duration::from_secs(45);
pub(super) struct ScenarioRuntime {
    client: Device,
    web_base_url: String,
    cli: ProcessGuard,
    node: ProcessGuard,
    rendezvous: ProcessGuard,
    ticket_timeout: Option<TicketTimeoutServer>,
    _lab: Lab,
}

impl ScenarioRuntime {
    pub(super) async fn setup(scenario: Scenario) -> Result<Self> {
        let artifacts = artifact_dir(scenario.name)?;
        let patchbay_out = artifacts.join("patchbay");
        fs::create_dir_all(&patchbay_out)?;
        let lab = Lab::builder()
            .label(scenario.name)
            .outdir(OutDir::Exact(patchbay_out))
            .build()
            .await
            .context("failed building Patchbay lab")?;

        let public_router = lab
            .add_router("public")
            .preset(RouterPreset::PublicV4)
            .build()
            .await?;
        let node_router = add_network_router(&lab, "node-nat", scenario.network).await?;
        let client_router = add_network_router(&lab, "client-nat", scenario.network).await?;
        let rendezvous_device = lab
            .add_device("rendezvous")
            .uplink(public_router.id())
            .build()
            .await?;
        let node_device = lab
            .add_device("node")
            .uplink(node_router.id())
            .build()
            .await?;
        let client_device = lab
            .add_device("client")
            .uplink(client_router.id())
            .build()
            .await?;

        let rendezvous_ip = rendezvous_device
            .ip()
            .context("rendezvous device has no IPv4 address")?;
        let node_ip = node_device
            .ip()
            .context("node device has no IPv4 address")?;
        let cluster_id = Uuid::new_v4();
        let node_id = Uuid::new_v4();
        let tls = write_node_tls(
            &artifacts.join("node-tls"),
            cluster_id,
            node_id,
            node_ip,
            rendezvous_ip,
        )?;
        let rendezvous_url = format!("http://{rendezvous_ip}:{RENDEZVOUS_PORT}");
        let mut rendezvous = spawn_rendezvous(
            &rendezvous_device,
            &artifacts,
            &rendezvous_url,
            scenario.iroh_relay_enabled,
            &tls,
        )?;
        wait_for_status(
            &rendezvous_device,
            format!("{rendezvous_url}/health"),
            StatusCode::OK,
        )
        .await?;
        rendezvous.ensure_running()?;

        let mut node = spawn_node(
            &node_device,
            &artifacts,
            &rendezvous_url,
            cluster_id,
            node_id,
            node_ip,
            &tls,
        )?;
        let node_url = format!("http://{node_ip}:{NODE_PUBLIC_PORT}");
        wait_for_status(&node_device, format!("{node_url}/health"), StatusCode::OK).await?;
        node.ensure_running()?;
        wait_for_registered_endpoint(&rendezvous_device, &rendezvous_url).await?;

        let client_dir = artifacts.join("client");
        fs::create_dir_all(&client_dir)?;
        let bootstrap_path = client_dir.join("connection.bootstrap.json");
        issue_bootstrap(&node_device, &node_url, &bootstrap_path).await?;
        configure_ironmesh_relay(&bootstrap_path, scenario.ironmesh_relay_enabled)?;
        let identity_path = client_dir.join("connection.bootstrap.client-identity.json");
        enroll_cli(&node_device, &bootstrap_path, &identity_path).await?;

        let ticket_timeout = if scenario.stall_first_iroh_ticket {
            let server =
                TicketTimeoutServer::spawn(&rendezvous_device, "0.0.0.0:0".parse()?).await?;
            prepend_rendezvous_url(&bootstrap_path, &server.url())?;
            Some(server)
        } else {
            None
        };
        let mut cli = spawn_cli_web(&client_device, &artifacts, &bootstrap_path, &identity_path)?;
        let web_base_url = format!("http://127.0.0.1:{CLI_WEB_PORT}");
        wait_for_status(
            &client_device,
            format!("{web_base_url}/api/ping"),
            StatusCode::OK,
        )
        .await
        .with_context(|| format!("cli logs:\n{}", cli.stderr_tail()))?;
        cli.ensure_running()?;

        Ok(Self {
            client: client_device,
            web_base_url,
            cli,
            node,
            rendezvous,
            ticket_timeout,
            _lab: lab,
        })
    }

    pub(super) async fn wait_for_route(&self, expected: ExpectedRoute) -> Result<Value> {
        let deadline = Instant::now() + ROUTE_READY_TIMEOUT;
        let mut last_snapshot: Value;
        loop {
            last_snapshot = match self.connection_routes().await {
                Ok(snapshot) => {
                    if candidate_matches(&snapshot, expected) {
                        return Ok(snapshot);
                    }
                    snapshot
                }
                Err(error) => json!({ "refresh_error": format!("{error:#}") }),
            };
            if Instant::now() >= deadline {
                bail!(
                    "route candidate {expected:?} did not become usable within {ROUTE_READY_TIMEOUT:?}; last snapshot: {last_snapshot:#}"
                );
            }
            sleep(Duration::from_millis(500)).await;
        }
    }

    pub(super) async fn store_list(&self) -> Result<Value> {
        device_json_request(
            &self.client,
            reqwest::Method::GET,
            format!("{}/api/store/list", self.web_base_url),
            None,
        )
        .await
    }

    pub(super) async fn wait_for_store_list(&self) -> Result<Value> {
        let deadline = Instant::now() + STORE_READY_TIMEOUT;
        let mut last_error = String::from("no request attempted");
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!(
                    "store/list did not become usable within {STORE_READY_TIMEOUT:?}: {last_error}"
                );
            }
            match timeout(remaining, self.store_list()).await {
                Ok(Ok(store_list)) => return Ok(store_list),
                Ok(Err(error)) => last_error = format!("{error:#}"),
                Err(_) => {
                    bail!(
                        "store/list did not become usable within {STORE_READY_TIMEOUT:?}: {last_error}"
                    );
                }
            }
            sleep(Duration::from_millis(500)).await;
        }
    }

    pub(super) async fn connection_routes(&self) -> Result<Value> {
        device_json_request(
            &self.client,
            reqwest::Method::GET,
            format!("{}/api/connection-routes", self.web_base_url),
            None,
        )
        .await
    }

    pub(super) fn ticket_request_count(&self) -> Result<u64> {
        self.ticket_timeout
            .as_ref()
            .map(TicketTimeoutServer::ticket_request_count)
            .context("scenario has no Iroh ticket fault server")
    }

    pub(super) async fn stop(mut self) {
        self.cli.stop().await;
        self.node.stop().await;
        self.rendezvous.stop().await;
        if let Some(ticket_timeout) = self.ticket_timeout.take() {
            let _ = ticket_timeout.shutdown().await;
        }
    }
}

async fn add_network_router(lab: &Lab, name: &str, profile: NetworkProfile) -> Result<Router> {
    match profile {
        NetworkProfile::HolePunchableHomeNat => {
            // Match Iroh's own IPv4 NAT traversal tests exactly: EIM/APDF NAT
            // with no additional firewall. RouterBuilder defaults to IPv4-only.
            Ok(lab.add_router(name).nat(Nat::Home).build().await?)
        }
        NetworkProfile::HotelBlockedUdp => Ok(lab
            .add_router(name)
            .preset(RouterPreset::Hotel)
            .build()
            .await?),
    }
}

async fn issue_bootstrap(device: &Device, node_url: &str, output: &Path) -> Result<()> {
    let node_url = node_url.to_string();
    let output = output.to_path_buf();
    device
        .spawn(move |_device| async move {
            let bootstrap = isolated_http_client()?
                .post(format!("{node_url}/auth/bootstrap-bundles/issue"))
                .header("x-ironmesh-admin-token", ADMIN_TOKEN)
                .json(&json!({
                    "label": "quic-network-cli",
                    "expires_in_secs": 3600
                }))
                .send()
                .await?
                .error_for_status()?
                .json::<ConnectionBootstrap>()
                .await
                .context("failed decoding issued bootstrap")?;
            bootstrap.write_to_path(&output)
        })?
        .await
        .context("bootstrap issue task panicked")?
}

fn configure_ironmesh_relay(path: &Path, enabled: bool) -> Result<()> {
    let mut bootstrap = ConnectionBootstrap::from_path(path)?;
    bootstrap.relay_mode = if enabled {
        RelayMode::Fallback
    } else {
        RelayMode::Disabled
    };
    bootstrap.write_to_path(path)
}

fn prepend_rendezvous_url(path: &Path, first_url: &str) -> Result<()> {
    let mut bootstrap = ConnectionBootstrap::from_path(path)?;
    bootstrap.rendezvous_urls.retain(|url| url != first_url);
    bootstrap.rendezvous_urls.insert(0, first_url.to_string());
    bootstrap.write_to_path(path)
}

async fn wait_for_status(device: &Device, url: String, expected: StatusCode) -> Result<()> {
    let deadline = Instant::now() + PROCESS_READY_TIMEOUT;
    let mut last_error: anyhow::Error;
    loop {
        let request_url = url.clone();
        match device
            .spawn(move |_device| async move {
                isolated_http_client()?
                    .get(request_url)
                    .send()
                    .await
                    .map(|response| response.status())
                    .map_err(anyhow::Error::from)
            })?
            .await
            .context("health request task panicked")?
        {
            Ok(status) if status == expected => return Ok(()),
            Ok(status) => last_error = anyhow!("received status {status}"),
            Err(error) => last_error = error,
        }
        if Instant::now() >= deadline {
            bail!(
                "{url} did not return {expected} within {PROCESS_READY_TIMEOUT:?}: {last_error:#}"
            );
        }
        sleep(Duration::from_millis(250)).await;
    }
}

async fn wait_for_registered_endpoint(device: &Device, rendezvous_url: &str) -> Result<()> {
    let deadline = Instant::now() + PROCESS_READY_TIMEOUT;
    let url = format!("{rendezvous_url}/control/presence");
    let mut last_payload = Value::Null;
    loop {
        if let Ok(payload) =
            device_json_request(device, reqwest::Method::GET, url.clone(), None).await
        {
            let registered = payload["registered_endpoints"].as_u64().unwrap_or_default();
            if registered == 1 {
                return Ok(());
            }
            last_payload = payload;
        }
        if Instant::now() >= deadline {
            bail!(
                "rendezvous did not report one registered endpoint within {PROCESS_READY_TIMEOUT:?}: {last_payload:#}"
            );
        }
        sleep(Duration::from_millis(250)).await;
    }
}

async fn device_json_request(
    device: &Device,
    method: reqwest::Method,
    url: String,
    body: Option<Value>,
) -> Result<Value> {
    timeout(DEVICE_REQUEST_TIMEOUT, {
        let device = device.clone();
        async move {
            device
                .spawn(move |_device| async move {
                    let client = isolated_http_client()?;
                    let mut request = client.request(method, url);
                    if let Some(body) = body {
                        request = request.json(&body);
                    }
                    request
                        .send()
                        .await?
                        .error_for_status()?
                        .json::<Value>()
                        .await
                        .context("failed decoding JSON response")
                })?
                .await
                .context("device HTTP task panicked")?
        }
    })
    .await
    .context("device JSON request timed out")?
}

fn isolated_http_client() -> Result<Client> {
    Client::builder()
        .no_proxy()
        .build()
        .context("failed building namespace-local HTTP client")
}
