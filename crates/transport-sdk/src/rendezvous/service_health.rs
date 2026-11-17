use super::{RendezvousControlClient, rendezvous_http_client_builder};
use anyhow::{Context, Result, bail};
use futures_util::{StreamExt, stream};
use reqwest::{Client, Url, redirect::Policy};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SERVICE_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_CONCURRENT_SERVICE_PROBES: usize = 4;
const MAX_HEALTH_RESPONSE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RendezvousServiceHealthSnapshot {
    pub generated_at_unix_ms: u64,
    pub endpoints: Vec<RendezvousServiceHealth>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RendezvousServiceHealth {
    pub rendezvous: ServiceHealth,
    pub iroh_relays: Vec<ServiceHealth>,
    /// None with an empty relay list means the service explicitly advertised no relays.
    pub relay_discovery_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceHealth {
    pub url: String,
    /// `reachable`, `unhealthy` (HTTP response but failed health), or `unreachable`.
    pub status: String,
    pub checked_at_unix_ms: u64,
    pub latency_ms: u64,
    pub http_status: Option<u16>,
    pub reported_status: Option<String>,
    pub software_version: Option<String>,
    pub error: Option<String>,
}

#[derive(Deserialize)]
struct RendezvousHealthResponse {
    status: String,
    software_version: Option<String>,
    iroh_relay_public_urls: Option<Vec<String>>,
}

impl RendezvousControlClient {
    /// Check the infrastructure itself without discovery, ticket allocation, or route-state updates.
    pub async fn probe_service_health(&self) -> Result<RendezvousServiceHealthSnapshot> {
        self.probe_service_health_with_timeout(SERVICE_PROBE_TIMEOUT)
            .await
    }

    async fn probe_service_health_with_timeout(
        &self,
        timeout: Duration,
    ) -> Result<RendezvousServiceHealthSnapshot> {
        let rendezvous_http = rendezvous_http_client_builder(
            self.server_ca_pem.as_deref(),
            self.client_identity_pem.as_deref(),
        )?
        .redirect(Policy::none())
        .timeout(timeout)
        .build()?;
        // Advertised relays can have a different origin. Do not send them the Rendezvous identity.
        let relay_http = rendezvous_http_client_builder(self.server_ca_pem.as_deref(), None)?
            .redirect(Policy::none())
            .timeout(timeout)
            .build()?;
        let probes = self
            .config
            .rendezvous_urls
            .iter()
            .map(|url| probe_rendezvous(&rendezvous_http, &relay_http, url))
            .collect::<Vec<_>>();
        let endpoints = stream::iter(probes)
            .buffered(MAX_CONCURRENT_SERVICE_PROBES)
            .collect()
            .await;
        Ok(RendezvousServiceHealthSnapshot {
            generated_at_unix_ms: unix_ms(),
            endpoints,
        })
    }
}

async fn probe_rendezvous(
    http: &Client,
    relay_http: &Client,
    url: &str,
) -> RendezvousServiceHealth {
    let (mut rendezvous, body) = probe_http(http, url, "health").await;
    let advertised_relays = match body {
        Some(body) => match serde_json::from_slice::<RendezvousHealthResponse>(&body) {
            Ok(health) => {
                rendezvous.reported_status = Some(health.status.clone());
                rendezvous.software_version = health.software_version;
                if health.status != "ok" && rendezvous.status == "reachable" {
                    rendezvous.status = "unhealthy".to_string();
                    rendezvous.error =
                        Some(format!("Rendezvous reported status: {}", health.status));
                }
                health
                    .iroh_relay_public_urls
                    .context("Rendezvous did not report iroh relay addresses")
            }
            Err(error) => {
                if rendezvous.status == "reachable" {
                    rendezvous.status = "unhealthy".to_string();
                    rendezvous.error = Some(format!("Invalid Rendezvous health response: {error}"));
                }
                Err(anyhow::anyhow!(
                    "Cannot discover iroh relays: invalid Rendezvous health response"
                ))
            }
        },
        None => Err(anyhow::anyhow!(
            "Cannot discover iroh relays: {}",
            rendezvous
                .error
                .as_deref()
                .unwrap_or("Rendezvous health unavailable")
        )),
    };
    let (iroh_relays, relay_discovery_error) = match advertised_relays {
        Ok(urls) => {
            let mut unique = Vec::new();
            for url in urls {
                let url = url.trim().trim_end_matches('/').to_string();
                if !unique.contains(&url) {
                    unique.push(url);
                }
            }
            let pending = unique
                .iter()
                .map(|url| async move {
                    probe_http(relay_http, url, iroh_relay::http::RELAY_PROBE_PATH)
                        .await
                        .0
                })
                .collect::<Vec<_>>();
            let probes = stream::iter(pending)
                .buffered(MAX_CONCURRENT_SERVICE_PROBES)
                .collect()
                .await;
            (probes, None)
        }
        Err(error) => (Vec::new(), Some(error.to_string())),
    };
    RendezvousServiceHealth {
        rendezvous,
        iroh_relays,
        relay_discovery_error,
    }
}

async fn probe_http(http: &Client, base_url: &str, path: &str) -> (ServiceHealth, Option<Vec<u8>>) {
    let started = Instant::now();
    let mut health = ServiceHealth {
        url: base_url.to_string(),
        status: "unreachable".to_string(),
        checked_at_unix_ms: unix_ms(),
        latency_ms: 0,
        http_status: None,
        reported_status: None,
        software_version: None,
        error: None,
    };
    let result: Result<Vec<u8>> = async {
        let url = Url::parse(&format!("{}/", base_url.trim().trim_end_matches('/')))
            .context("Invalid service URL")?
            .join(path)
            .context("Invalid service probe URL")?;
        let mut response = http
            .get(url)
            .send()
            .await
            .context("Service health request failed")?;
        let status = response.status();
        health.http_status = Some(status.as_u16());
        health.status = if status.is_success() {
            "reachable"
        } else {
            "unhealthy"
        }
        .to_string();
        if !status.is_success() {
            health.error = Some(format!("HTTP {status}"));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .context("Reading service health response failed")?
        {
            if body.len() + chunk.len() > MAX_HEALTH_RESPONSE_BYTES {
                bail!("Service health response exceeded {MAX_HEALTH_RESPONSE_BYTES} bytes");
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
    .await;
    health.latency_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
    let body = match result {
        Ok(body) => Some(body),
        Err(error) => {
            if health.http_status.is_some() {
                health.status = "unhealthy".to_string();
            }
            health.error = Some(format!("{error:#}"));
            None
        }
    };
    (health, body)
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use crate::rendezvous::{RendezvousClientConfig, RendezvousControlClient};
    use axum::{Json, Router, routing::get};
    use reqwest::StatusCode;
    use serde_json::json;
    use std::time::Duration;

    async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, task)
    }

    fn client(urls: Vec<String>) -> RendezvousControlClient {
        RendezvousControlClient::new(
            RendezvousClientConfig {
                cluster_id: uuid::Uuid::new_v4(),
                rendezvous_urls: urls,
                heartbeat_interval_secs: 15,
            },
            None,
            None,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn service_health_separates_rendezvous_from_iroh_without_changing_routes() {
        let (relay_url, relay) =
            serve(Router::new().route("/ping", get(|| async { StatusCode::SERVICE_UNAVAILABLE })))
                .await;
        let advertised_url = relay_url.clone();
        let (url, server) = serve(Router::new().route(
            "/health",
            get(move || {
                let relay_url = advertised_url.clone();
                async move {
                    Json(json!({
                        "status": "ok", "software_version": "2.0.0",
                        "iroh_relay_public_urls": [relay_url.clone(), format!("{relay_url}/")]
                    }))
                }
            }),
        ))
        .await;
        let client = client(vec![url]);
        let before = client.runtime_state();

        let snapshot = client.probe_service_health().await.unwrap();

        assert_eq!(client.runtime_state(), before);
        assert_eq!(snapshot.endpoints.len(), 1);
        let endpoint = &snapshot.endpoints[0];
        assert_eq!(endpoint.rendezvous.status, "reachable");
        assert_eq!(
            endpoint.rendezvous.software_version.as_deref(),
            Some("2.0.0")
        );
        assert!(endpoint.rendezvous.checked_at_unix_ms > 0);
        assert_eq!(endpoint.iroh_relays.len(), 1);
        assert_eq!(endpoint.iroh_relays[0].url, relay_url);
        assert_eq!(endpoint.iroh_relays[0].status, "unhealthy");
        assert_eq!(endpoint.iroh_relays[0].http_status, Some(503));
        relay.abort();
        server.abort();
    }

    #[tokio::test]
    async fn service_health_reports_bad_payloads_and_connection_failures_per_endpoint() {
        let (invalid_url, invalid) =
            serve(Router::new().route("/health", get(|| async { "an unrelated HTTP service" })))
                .await;
        let (offline_url, offline) = serve(Router::new()).await;
        offline.abort();
        let _ = offline.await;
        let snapshot = client(vec![invalid_url, offline_url])
            .probe_service_health()
            .await
            .unwrap();

        assert_eq!(snapshot.endpoints[0].rendezvous.status, "unhealthy");
        assert_eq!(snapshot.endpoints[0].rendezvous.http_status, Some(200));
        assert!(snapshot.endpoints[0].relay_discovery_error.is_some());
        assert_eq!(snapshot.endpoints[1].rendezvous.status, "unreachable");
        assert!(snapshot.endpoints[1].rendezvous.error.is_some());
        assert!(snapshot.endpoints[1].relay_discovery_error.is_some());
        invalid.abort();
    }

    #[tokio::test]
    async fn service_health_distinguishes_no_advertised_relay_and_an_unhealthy_service() {
        let (url, server) = serve(Router::new().route(
            "/health",
            get(|| async { Json(json!({"status": "degraded", "iroh_relay_public_urls": []})) }),
        ))
        .await;
        let snapshot = client(vec![url]).probe_service_health().await.unwrap();
        let endpoint = &snapshot.endpoints[0];
        assert_eq!(endpoint.rendezvous.status, "unhealthy");
        assert_eq!(
            endpoint.rendezvous.reported_status.as_deref(),
            Some("degraded")
        );
        assert!(endpoint.iroh_relays.is_empty());
        assert!(endpoint.relay_discovery_error.is_none());
        server.abort();
    }

    #[tokio::test]
    async fn service_health_checks_relay_even_when_rendezvous_reports_failure() {
        let (relay_url, relay) = serve(Router::new().route("/ping", get(|| async { "" }))).await;
        let (url, server) = serve(Router::new().route(
            "/health",
            get(move || {
                let relay_url = relay_url.clone();
                async move {
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        Json(json!({
                            "status": "degraded", "iroh_relay_public_urls": [relay_url]
                        })),
                    )
                }
            }),
        ))
        .await;
        let snapshot = client(vec![url]).probe_service_health().await.unwrap();
        assert_eq!(snapshot.endpoints[0].rendezvous.status, "unhealthy");
        assert_eq!(snapshot.endpoints[0].iroh_relays[0].status, "reachable");
        assert_eq!(snapshot.endpoints[0].iroh_relays[0].http_status, Some(200));
        relay.abort();
        server.abort();
    }

    #[tokio::test]
    async fn service_health_reports_unknown_relays_for_older_health_responses() {
        let (url, server) =
            serve(Router::new().route("/health", get(|| async { Json(json!({"status": "ok"})) })))
                .await;
        let snapshot = client(vec![url]).probe_service_health().await.unwrap();
        assert_eq!(snapshot.endpoints[0].rendezvous.status, "reachable");
        assert!(snapshot.endpoints[0].relay_discovery_error.is_some());
        server.abort();
    }

    #[tokio::test]
    async fn service_health_bounds_response_bodies_and_preserves_http_status_on_body_timeout() {
        let (url, server) = serve(
            Router::new()
                .route("/large/health", get(|| async { "x".repeat(65 * 1024) }))
                .route(
                    "/slow-body/health",
                    get(|| async {
                        axum::body::Body::from_stream(futures_util::stream::once(async {
                            tokio::time::sleep(Duration::from_secs(10)).await;
                            Ok::<_, std::io::Error>("{}")
                        }))
                    }),
                ),
        )
        .await;
        let snapshot = tokio::time::timeout(
            Duration::from_secs(1),
            client(vec![format!("{url}/large"), format!("{url}/slow-body")])
                .probe_service_health_with_timeout(Duration::from_millis(50)),
        )
        .await
        .unwrap()
        .unwrap();
        for endpoint in &snapshot.endpoints {
            assert_eq!(endpoint.rendezvous.status, "unhealthy");
            assert_eq!(endpoint.rendezvous.http_status, Some(200));
            assert!(endpoint.relay_discovery_error.is_some());
        }
        assert!(
            snapshot.endpoints[0]
                .rendezvous
                .error
                .as_deref()
                .unwrap()
                .contains("exceeded")
        );
        server.abort();
    }

    #[tokio::test]
    async fn service_health_bounds_slow_probes_and_does_not_follow_redirects() {
        let (url, server) = serve(
            Router::new()
                .route(
                    "/slow/health",
                    get(|| async {
                        tokio::time::sleep(Duration::from_secs(10)).await;
                        Json(json!({"status": "ok"}))
                    }),
                )
                .route(
                    "/health",
                    get(|| async { axum::response::Redirect::temporary("/healthy") }),
                )
                .route("/healthy", get(|| async { Json(json!({"status": "ok"})) })),
        )
        .await;
        let snapshot = tokio::time::timeout(
            Duration::from_secs(1),
            client(vec![format!("{url}/slow/"), url])
                .probe_service_health_with_timeout(Duration::from_millis(50)),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(snapshot.endpoints[0].rendezvous.status, "unreachable");
        assert_eq!(snapshot.endpoints[1].rendezvous.status, "unhealthy");
        assert_eq!(snapshot.endpoints[1].rendezvous.http_status, Some(307));
        server.abort();
    }
}
