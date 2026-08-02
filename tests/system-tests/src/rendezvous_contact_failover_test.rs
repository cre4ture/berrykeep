#![cfg(test)]

use crate::framework::{
    default_client_identity_path, fresh_data_dir, insecure_https_client, run_cli,
    run_latency_cli_with_retry, start_rendezvous_service, start_zero_touch_server, stop_server,
    wait_for_rendezvous_registered_endpoints,
};
use crate::managed_cluster_test_support::{
    admin_login_cookie, issue_bootstrap_bundle_with_cookie,
    issue_node_enrollment_from_join_request_with_cookie, setup_generate_join_request,
    setup_import_node_enrollment, setup_start_cluster,
    update_rendezvous_contact_configuration_with_cookie, wait_for_cluster_nodes_with_cookie,
    wait_for_runtime_admin_surface,
};
use anyhow::{Context, Result, bail};
use client_sdk::ConnectionBootstrap;
use std::fs;
use std::path::Path;
use std::time::Duration;
use tokio::time::sleep;

const NEW_RENDEZVOUS_BIND: &str = "127.0.0.1:19490";
const NODE_A_BIND: &str = "127.0.0.1:19491";
const NODE_B_BIND: &str = "127.0.0.1:19492";
const ADMIN_PASSWORD: &str = "rendezvous-contact-failover-password";

struct InitialClientBootstrap {
    bootstrap_path: std::path::PathBuf,
    bootstrap_arg: String,
    identity_arg: String,
    old_rendezvous_url: String,
}

#[tokio::test]
async fn replicated_rendezvous_contacts_register_nodes_and_survive_old_relay_shutdown() -> Result<()>
{
    let new_rendezvous_url = format!("http://{NEW_RENDEZVOUS_BIND}");
    let data_a = fresh_data_dir("rendezvous-contact-failover-node-a");
    let data_b = fresh_data_dir("rendezvous-contact-failover-node-b");
    let client_dir = fresh_data_dir("rendezvous-contact-failover-client");

    let insecure_http = insecure_https_client()?;
    let mut new_rendezvous = start_rendezvous_service(NEW_RENDEZVOUS_BIND).await?;
    let mut node_a = start_zero_touch_server(NODE_A_BIND, &data_a).await?;
    let mut node_b = start_zero_touch_server(NODE_B_BIND, &data_b).await?;

    let result = async {
        let start_cluster =
            setup_start_cluster(&insecure_http, NODE_A_BIND, ADMIN_PASSWORD).await?;
        let node_id_a = start_cluster
            .get("node_id")
            .and_then(|value| value.as_str())
            .context("start-cluster response missing node A id")?
            .to_string();
        wait_for_runtime_admin_surface(&insecure_http, NODE_A_BIND).await?;
        let admin_cookie_a = admin_login_cookie(&insecure_http, NODE_A_BIND, ADMIN_PASSWORD).await?;

        let join_request = setup_generate_join_request(&insecure_http, NODE_B_BIND).await?;
        let node_id_b = join_request
            .get("node_id")
            .and_then(|value| value.as_str())
            .context("join request missing node B id")?
            .to_string();
        let enrollment = issue_node_enrollment_from_join_request_with_cookie(
            &insecure_http,
            NODE_A_BIND,
            &admin_cookie_a,
            &join_request,
        )
        .await?;
        setup_import_node_enrollment(&insecure_http, NODE_B_BIND, ADMIN_PASSWORD, &enrollment)
            .await?;
        wait_for_runtime_admin_surface(&insecure_http, NODE_B_BIND).await?;
        let admin_cookie_b = admin_login_cookie(&insecure_http, NODE_B_BIND, ADMIN_PASSWORD).await?;
        wait_for_cluster_nodes_with_cookie(&insecure_http, NODE_A_BIND, &admin_cookie_a, 2, 240)
            .await?;

        let initial_client = issue_and_enroll_initial_client(
            &insecure_http,
            NODE_A_BIND,
            &admin_cookie_a,
            &client_dir,
            NEW_RENDEZVOUS_BIND,
        )
        .await?;

        let updated_contacts = update_rendezvous_contact_configuration_with_cookie(
            &insecure_http,
            NODE_A_BIND,
            &admin_cookie_a,
            &[new_rendezvous_url.as_str()],
        )
        .await?;
        let version_id = updated_contacts
            .get("version_id")
            .and_then(|value| value.as_str())
            .context("updated rendezvous contact list missing version id")?;
        assert!(
            updated_contacts
                .pointer("/configuration/rendezvous_urls")
                .and_then(|value| value.as_array())
                .is_some_and(|urls| {
                    urls.iter().any(|url| {
                        url.as_str()
                            .is_some_and(|value| value.contains(NEW_RENDEZVOUS_BIND))
                    })
                }),
            "admin update did not return the new rendezvous contact: {updated_contacts}"
        );

        // Node B must receive the normal replicated object before it can register itself at the
        // newly advertised service. This verifies the cluster-wide update, not merely node A's
        // in-process view.
        wait_for_replicated_contact_list(
            &insecure_http,
            NODE_B_BIND,
            &admin_cookie_b,
            NEW_RENDEZVOUS_BIND,
            version_id,
        )
        .await?;
        wait_for_rendezvous_registered_endpoints(&new_rendezvous_url, 2, 240).await?;

        // The old rendezvous service still lets the client make one authenticated connection.
        // That connection must save the newly downloaded cluster contact list in its bootstrap.
        let initial_relay_probe = run_latency_cli_with_retry(
            &initial_client.bootstrap_arg,
            &initial_client.identity_arg,
            &[
                "--path",
                "relay",
                "--relay-url",
                initial_client.old_rendezvous_url.trim_end_matches('/'),
                "--samples",
                "1",
                "--warmup",
                "0",
                "--pause-ms",
                "0",
                "--json",
            ],
            120,
        )
        .await?;
        assert_relay_probe_succeeds_via(
            &serde_json::from_str(&initial_relay_probe)
                .context("initial relay probe output should be JSON")?,
            initial_client.old_rendezvous_url.trim_end_matches('/'),
        );

        let persisted_bootstrap = ConnectionBootstrap::from_path(&initial_client.bootstrap_path)?;
        let persisted_contacts = persisted_bootstrap
            .rendezvous_contact_list
            .context("client did not persist the authenticated rendezvous contact list")?;
        assert_eq!(persisted_contacts.version_id, version_id);
        assert!(
            persisted_contacts
                .rendezvous_urls
                .iter()
                .any(|url| url.contains(NEW_RENDEZVOUS_BIND)),
            "persisted contact list is missing the new rendezvous URL: {persisted_contacts:?}"
        );

        // Node A hosts the old embedded service. Shutting it down proves that a fresh CLI
        // invocation validates and uses the persisted contact URL, while node B remains
        // reachable only through the replacement rendezvous service.
        stop_server(&mut node_a).await;
        let failover_probe = run_latency_cli_with_retry(
            &initial_client.bootstrap_arg,
            &initial_client.identity_arg,
            &[
                "--path",
                "relay",
                "--node-id",
                &node_id_b,
                "--relay-url",
                new_rendezvous_url.as_str(),
                "--samples",
                "1",
                "--warmup",
                "0",
                "--pause-ms",
                "0",
                "--json",
            ],
            120,
        )
        .await?;
        assert_relay_probe_succeeds_via(
            &serde_json::from_str(&failover_probe)
                .context("failover relay probe output should be JSON")?,
            new_rendezvous_url.as_str(),
        );
        assert!(
            failover_probe.contains(&node_id_b),
            "failover probe did not target node B: {failover_probe}"
        );
        assert!(
            !failover_probe.contains(&node_id_a),
            "failover probe unexpectedly selected shut-down node A: {failover_probe}"
        );

        Ok::<(), anyhow::Error>(())
    }
    .await;

    stop_server(&mut node_a).await;
    stop_server(&mut node_b).await;
    stop_server(&mut new_rendezvous).await;
    let _ = fs::remove_dir_all(&data_a);
    let _ = fs::remove_dir_all(&data_b);
    let _ = fs::remove_dir_all(&client_dir);

    result
}

async fn issue_and_enroll_initial_client(
    http: &reqwest::Client,
    bind: &str,
    session_cookie: &str,
    client_dir: &Path,
    replacement_rendezvous_bind: &str,
) -> Result<InitialClientBootstrap> {
    // Issue the bootstrap before the contact-list change. The client therefore has the old
    // embedded rendezvous service only as an immutable recovery anchor at first.
    let bootstrap = issue_bootstrap_bundle_with_cookie(
        http,
        bind,
        session_cookie,
        Some("rendezvous-contact-failover"),
        Some(3_600),
    )
    .await?;
    let old_rendezvous_url = bootstrap
        .rendezvous_urls
        .first()
        .cloned()
        .context("bootstrap missing the original embedded rendezvous URL")?;
    assert!(
        !bootstrap
            .rendezvous_urls
            .iter()
            .any(|url| url.contains(replacement_rendezvous_bind)),
        "initial bootstrap must not already include the later contact-list rendezvous URL"
    );

    let bootstrap_path = client_dir.join("rendezvous-contact-failover.bootstrap.json");
    bootstrap.write_to_path(&bootstrap_path)?;
    let bootstrap_arg = bootstrap_path.to_string_lossy().into_owned();
    let identity_arg = default_client_identity_path(&bootstrap_path)
        .to_string_lossy()
        .into_owned();

    let enroll_output = run_cli(&[
        "--bootstrap-file",
        bootstrap_arg.as_str(),
        "--client-identity-file",
        identity_arg.as_str(),
        "enroll",
        "--label",
        "rendezvous-contact-failover",
    ])
    .await?;
    assert!(
        enroll_output.contains("enrolled device"),
        "unexpected enrollment output: {enroll_output}"
    );

    Ok(InitialClientBootstrap {
        bootstrap_path,
        bootstrap_arg,
        identity_arg,
        old_rendezvous_url,
    })
}

async fn wait_for_replicated_contact_list(
    http: &reqwest::Client,
    bind: &str,
    session_cookie: &str,
    rendezvous_bind: &str,
    version_id: &str,
) -> Result<()> {
    for _ in 0..240 {
        if let Ok(response) = http
            .get(format!("https://{bind}/auth/cluster/rendezvous-contacts"))
            .header(reqwest::header::COOKIE, session_cookie)
            .send()
            .await
            && let Ok(response) = response.error_for_status()
            && let Ok(payload) = response.json::<serde_json::Value>().await
            && payload.get("version_id").and_then(|value| value.as_str()) == Some(version_id)
            && payload
                .pointer("/configuration/rendezvous_urls")
                .and_then(|value| value.as_array())
                .is_some_and(|urls| {
                    urls.iter().any(|url| {
                        url.as_str()
                            .is_some_and(|value| value.contains(rendezvous_bind))
                    })
                })
        {
            return Ok(());
        }
        sleep(Duration::from_millis(250)).await;
    }

    bail!(
        "node at https://{bind} did not receive rendezvous contact {rendezvous_bind} with version {version_id}"
    );
}

fn assert_relay_probe_succeeds_via(response: &serde_json::Value, rendezvous_url: &str) {
    let targets = response
        .get("targets")
        .and_then(|value| value.as_array())
        .unwrap_or_else(|| panic!("relay probe response should contain targets: {response}"));
    assert_eq!(
        targets.len(),
        1,
        "expected one relay target after explicit node and rendezvous selection: {response}"
    );
    let target = &targets[0];
    assert_eq!(
        target.get("transport_mode").and_then(|value| value.as_str()),
        Some("relay"),
        "expected relay transport: {response}"
    );
    assert!(
        target.get("error").is_none_or(serde_json::Value::is_null),
        "expected relay probe to succeed: {response}"
    );
    assert!(
        target
            .get("target")
            .and_then(|value| value.as_str())
            .is_some_and(|value| value.contains(rendezvous_url.trim_end_matches('/'))),
        "expected relay target to name {rendezvous_url}: {response}"
    );
}
