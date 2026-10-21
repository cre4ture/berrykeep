use super::*;
use transport_sdk::{RendezvousServiceConfig, resolve_rendezvous_services};

pub(super) fn service_urls(services: &[RendezvousServiceConfig]) -> Vec<String> {
    services.iter().map(|service| service.url.clone()).collect()
}

pub(super) fn configured_services(
    config: &ServerNodeConfig,
) -> Result<Vec<RendezvousServiceConfig>> {
    let mut explicit = config.rendezvous_services.clone();
    if let Some(managed) = &config.managed_rendezvous {
        let managed_url = canonicalize_rendezvous_url(&managed.public_url)?;
        explicit.retain(|service| {
            canonicalize_rendezvous_url(&service.url).ok().as_ref() != Some(&managed_url)
        });
        explicit.push(RendezvousServiceConfig {
            url: managed_url,
            global_registration_enabled: false,
        });
    }
    resolve_rendezvous_services(
        &config.rendezvous_urls,
        &explicit,
        config.global_rendezvous_registration_enabled,
    )
}

pub(super) fn current_configured_services(state: &ServerState) -> Vec<RendezvousServiceConfig> {
    state
        .network
        .configured_rendezvous_services
        .lock()
        .expect("rendezvous services mutex poisoned")
        .clone()
}

pub(super) fn current_services(state: &ServerState) -> Vec<RendezvousServiceConfig> {
    let mut services = current_configured_services(state);
    let contacts = state
        .network
        .cluster_rendezvous_services
        .lock()
        .expect("rendezvous contacts mutex poisoned")
        .clone();
    let mut seen: HashSet<_> = services.iter().map(|service| service.url.clone()).collect();
    // Node-local configuration takes precedence over a replicated contact for the same URL.
    services.extend(
        contacts
            .into_iter()
            .filter(|service| seen.insert(service.url.clone())),
    );
    services
}

pub(super) fn editable_services(state: &ServerState) -> Vec<RendezvousServiceConfig> {
    let editable_urls = current_editable_rendezvous_urls(state);
    current_configured_services(state)
        .into_iter()
        .filter(|service| editable_urls.contains(&service.url))
        .collect()
}

pub(super) fn updated_services(
    state: &ServerState,
    request: &UpdateRendezvousConfigRequest,
) -> Result<Vec<RendezvousServiceConfig>> {
    let services = match &request.editable_services {
        Some(services) => resolve_rendezvous_services(&[], services, false)?,
        None => {
            // URL-only API clients preserve the policy of entries they retain.
            let urls = normalize_rendezvous_url_list(request.editable_urls.as_deref().context(
                "rendezvous configuration requires editable_urls or editable_services",
            )?)?;
            let retained: Vec<_> = editable_services(state)
                .into_iter()
                .filter(|service| urls.contains(&service.url))
                .collect();
            resolve_rendezvous_services(&urls, &retained, false)?
        }
    };
    validate_services(state, &services)?;
    let urls = build_effective_rendezvous_urls(state, &service_urls(&services))?;
    resolve_rendezvous_services(&urls, &services, false)
}

pub(super) fn validate_services(
    state: &ServerState,
    services: &[RendezvousServiceConfig],
) -> Result<()> {
    let managed_url = state
        .network
        .managed_rendezvous_public_url
        .as_deref()
        .map(canonicalize_rendezvous_url)
        .transpose()?;
    if services.iter().any(|service| {
        service.global_registration_enabled && Some(&service.url) == managed_url.as_ref()
    }) {
        bail!("the embedded rendezvous service cannot use global registration");
    }
    if services
        .iter()
        .any(|service| service.global_registration_enabled)
        && !state.network.rendezvous_mtls_required
    {
        bail!("global rendezvous registration requires rendezvous mTLS to be enabled");
    }
    Ok(())
}

pub(super) async fn register_endpoint_presence(
    state: &ServerState,
    url: &str,
    client: &RendezvousControlClient,
    registration: &PresenceRegistration,
) -> Result<transport_sdk::RegisterPresenceResponse> {
    let global = current_services(state)
        .iter()
        .any(|service| service.url == url && service.global_registration_enabled);
    if global {
        if !state.network.rendezvous_mtls_required {
            bail!("global rendezvous registration requires rendezvous mTLS to be enabled");
        }
        // Only a node holding the cluster signer can prove CA ownership.
        if let Some(key) = state.network.internal_ca_key_pem.as_deref() {
            let registered = state
                .network
                .globally_registered_rendezvous_urls
                .lock()
                .await
                .contains(url);
            if !registered {
                let trust_state = state.clone();
                let trust =
                    tokio::task::spawn_blocking(move || load_live_trust_material(&trust_state))
                        .await
                        .context("failed loading global rendezvous trust material")??;
                // Each enrollment request has its own dedicated timeout. It is
                // separate from the shorter presence request budget below.
                register_cluster_with_global_rendezvous_services(
                    state.cluster_id,
                    &[url.to_string()],
                    trust.cluster_ca_pem.as_deref(),
                    Some(key),
                    trust.rendezvous_ca_pem.as_deref(),
                )
                .await?;
                state
                    .network
                    .globally_registered_rendezvous_urls
                    .lock()
                    .await
                    .insert(url.to_string());
            }
        }
    }
    // A transient presence error must not discard successful cluster enrollment.
    tokio::time::timeout(
        Duration::from_secs(RENDEZVOUS_REGISTRATION_REQUEST_TIMEOUT_SECS),
        client.register_presence(registration),
    )
    .await
    .with_context(|| {
        format!("timed out contacting rendezvous endpoint {url} while registering presence")
    })?
}
