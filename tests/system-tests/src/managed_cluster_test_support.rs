use anyhow::{Context, Result, bail};
use client_sdk::ConnectionBootstrap;
use std::time::Duration;
use tokio::time::sleep;

pub(crate) async fn setup_start_cluster(
    http: &reqwest::Client,
    bind: &str,
    admin_password: &str,
) -> Result<serde_json::Value> {
    http.post(format!("https://{bind}/setup/start-cluster"))
        .json(&serde_json::json!({
            "admin_password": admin_password,
            "public_origin": format!("https://{bind}"),
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
        .context("failed decoding setup start-cluster response")
}

pub(crate) async fn setup_generate_join_request(
    http: &reqwest::Client,
    bind: &str,
) -> Result<serde_json::Value> {
    http.post(format!("https://{bind}/setup/join/request"))
        .json(&serde_json::json!({
            "public_origin": format!("https://{bind}"),
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
        .context("failed decoding setup join request response")
}

pub(crate) async fn setup_import_node_enrollment(
    http: &reqwest::Client,
    bind: &str,
    admin_password: &str,
    package: &serde_json::Value,
) -> Result<serde_json::Value> {
    http.post(format!("https://{bind}/setup/join/import"))
        .json(&serde_json::json!({
            "admin_password": admin_password,
            "package_json": serde_json::to_string(package)?,
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
        .context("failed decoding setup join import response")
}

pub(crate) async fn wait_for_runtime_admin_surface(
    http: &reqwest::Client,
    bind: &str,
) -> Result<()> {
    for _ in 0..120 {
        if let Ok(response) = http
            .get(format!("https://{bind}/auth/admin/session"))
            .send()
            .await
            && response.status() == reqwest::StatusCode::OK
        {
            return Ok(());
        }
        sleep(Duration::from_millis(250)).await;
    }
    bail!("runtime admin surface did not become ready on https://{bind}");
}

fn parse_session_cookie(response: &reqwest::Response) -> Result<String> {
    let raw = response
        .headers()
        .get(reqwest::header::SET_COOKIE)
        .and_then(|value| value.to_str().ok())
        .context("admin login response missing Set-Cookie header")?;
    raw.split(';')
        .next()
        .map(ToString::to_string)
        .context("failed to parse admin session cookie")
}

pub(crate) async fn admin_login_cookie(
    http: &reqwest::Client,
    bind: &str,
    admin_password: &str,
) -> Result<String> {
    let response = http
        .post(format!("https://{bind}/auth/admin/login"))
        .json(&serde_json::json!({ "password": admin_password }))
        .send()
        .await?
        .error_for_status()?;
    parse_session_cookie(&response)
}

pub(crate) async fn issue_node_enrollment_from_join_request_with_cookie(
    http: &reqwest::Client,
    bind: &str,
    session_cookie: &str,
    join_request: &serde_json::Value,
) -> Result<serde_json::Value> {
    http.post(format!(
        "https://{bind}/auth/node-join-requests/issue-enrollment"
    ))
    .header(reqwest::header::COOKIE, session_cookie)
    .json(&serde_json::json!({
        "join_request": join_request,
        "tls_validity_secs": null,
        "tls_renewal_window_secs": null,
    }))
    .send()
    .await?
    .error_for_status()?
    .json()
    .await
    .context("failed decoding node enrollment from join request response")
}

pub(crate) async fn update_rendezvous_config_with_cookie(
    http: &reqwest::Client,
    bind: &str,
    session_cookie: &str,
    editable_urls: &[&str],
) -> Result<serde_json::Value> {
    http.put(format!("https://{bind}/auth/rendezvous-config"))
        .header(reqwest::header::COOKIE, session_cookie)
        .json(&serde_json::json!({ "editable_urls": editable_urls }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
        .context("failed decoding rendezvous config response")
}

pub(crate) async fn update_rendezvous_contact_configuration_with_cookie(
    http: &reqwest::Client,
    bind: &str,
    session_cookie: &str,
    rendezvous_urls: &[&str],
) -> Result<serde_json::Value> {
    http.put(format!("https://{bind}/auth/cluster/rendezvous-contacts"))
        .header(reqwest::header::COOKIE, session_cookie)
        .json(&serde_json::json!({
            "schema_version": 1,
            "rendezvous_urls": rendezvous_urls,
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
        .context("failed decoding rendezvous contact configuration response")
}

pub(crate) async fn issue_bootstrap_bundle_with_cookie(
    http: &reqwest::Client,
    bind: &str,
    session_cookie: &str,
    label: Option<&str>,
    expires_in_secs: Option<u64>,
) -> Result<ConnectionBootstrap> {
    http.post(format!("https://{bind}/auth/bootstrap-bundles/issue"))
        .header(reqwest::header::COOKIE, session_cookie)
        .json(&serde_json::json!({
            "label": label,
            "expires_in_secs": expires_in_secs,
        }))
        .send()
        .await?
        .error_for_status()?
        .json::<ConnectionBootstrap>()
        .await
        .context("failed decoding bootstrap bundle response")
}

pub(crate) async fn wait_for_cluster_nodes_with_cookie(
    http: &reqwest::Client,
    bind: &str,
    session_cookie: &str,
    expected_count: usize,
    retries: usize,
) -> Result<()> {
    for _ in 0..retries {
        if let Ok(response) = http
            .get(format!("https://{bind}/cluster/nodes"))
            .header(reqwest::header::COOKIE, session_cookie)
            .send()
            .await
            && let Ok(ok_response) = response.error_for_status()
            && let Ok(nodes) = ok_response.json::<serde_json::Value>().await
            && let Some(entries) = nodes.as_array()
            && entries.len() == expected_count
        {
            return Ok(());
        }
        sleep(Duration::from_millis(250)).await;
    }
    bail!(
        "cluster did not converge to {expected_count} known nodes at https://{bind}/cluster/nodes"
    );
}
