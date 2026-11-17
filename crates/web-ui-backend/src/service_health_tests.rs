use super::*;
use serde_json::{Value, json};

async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, task)
}

#[tokio::test]
async fn service_health_api_uses_managed_contacts_and_bootstrap_without_a_cluster_connection() {
    let (url, service) = serve(
        Router::new()
            .route(
                "/health",
                get(|| async { Json(json!({"status": "ok", "iroh_relay_public_urls": []})) }),
            )
            .route(
                "/managed/health",
                get(|| async { StatusCode::SERVICE_UNAVAILABLE }),
            ),
    )
    .await;
    let bootstrap: ConnectionBootstrap = serde_json::from_value(json!({
        "version": 1, "cluster_id": Uuid::new_v4(), "rendezvous_urls": [url],
        "rendezvous_contact_list": {
            "version_id": "contacts-v2", "rendezvous_urls": [format!("{url}/managed/")]
        },
        "trust_roots": {}
    }))
    .unwrap();
    let config =
        WebUiConfig::from_client(BerryKeepClient::from_direct_base_url("http://127.0.0.1:9"))
            .with_connection_bootstrap(bootstrap);
    let (web_url, web) = serve(router(config)).await;
    let response = reqwest::get(format!("{web_url}/api/v1/connection-services"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers().get(CACHE_CONTROL).unwrap(), "no-store");
    let payload: Value = response.json().await.unwrap();
    assert_eq!(payload["endpoints"].as_array().unwrap().len(), 2);
    assert_eq!(payload["endpoints"][0]["rendezvous"]["status"], "unhealthy");
    assert_eq!(payload["endpoints"][0]["rendezvous"]["http_status"], 503);
    assert_eq!(
        payload["endpoints"][1]["rendezvous"]["url"],
        format!("{url}/")
    );
    assert_eq!(payload["endpoints"][1]["rendezvous"]["status"], "reachable");
    service.abort();
    web.abort();
}

#[tokio::test]
async fn service_health_api_reports_no_configured_services() {
    let (url, server) = serve(router(WebUiConfig::from_client(
        BerryKeepClient::from_direct_base_url("http://127.0.0.1:9"),
    )))
    .await;
    let response = reqwest::get(format!("{url}/api/v1/connection-services"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let payload: Value = response.json().await.unwrap();
    assert_eq!(payload["endpoints"], json!([]));
    server.abort();
}
