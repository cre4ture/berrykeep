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

pub(super) async fn updated_services(
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
    validate_services(state, &services, &editable_services(state)).await?;
    let urls = build_effective_rendezvous_urls(state, &service_urls(&services))?;
    resolve_rendezvous_services(&urls, &services, false)
}

pub(super) async fn validate_services(
    state: &ServerState,
    services: &[RendezvousServiceConfig],
    existing_services: &[RendezvousServiceConfig],
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
    // Admission checks apply when enabling a new global role. A retained
    // cluster-wide policy must remain editable on nodes with different local settings.
    let introduces_global = services.iter().any(|service| {
        service.global_registration_enabled
            && !existing_services
                .iter()
                .any(|existing| existing.url == service.url && existing.global_registration_enabled)
    });
    if !introduces_global {
        return Ok(());
    }
    if !state.network.rendezvous_mtls_required {
        bail!("global rendezvous registration requires rendezvous mTLS to be enabled");
    }
    let tls = state.network.internal_tls_runtime.clone().context(
        "global rendezvous registration requires a node client identity from internal TLS",
    )?;
    tokio::task::spawn_blocking(move || {
        validate_rendezvous_client_identity(&tls.cert_path, &tls.key_path)
    })
    .await
    .context("failed validating global rendezvous node identity")?
}

const ENROLLMENT_RECOVERY_GRACE_SECS: u64 = 60;

pub(super) struct GlobalEnrollmentState {
    pub(super) next_attempt_at: tokio::time::Instant,
    pub(super) consecutive_failures: u32,
}

impl GlobalEnrollmentState {
    fn healthy() -> Self {
        Self {
            next_attempt_at: tokio::time::Instant::now()
                + Duration::from_secs(ENROLLMENT_RECOVERY_GRACE_SECS),
            consecutive_failures: 0,
        }
    }

    fn failed(&mut self) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        let delay_secs = (5_u64 << self.consecutive_failures.saturating_sub(1).min(6)).min(300);
        self.next_attempt_at = tokio::time::Instant::now() + Duration::from_secs(delay_secs);
    }
}

async fn publish_presence(
    url: &str,
    client: &RendezvousControlClient,
    registration: &PresenceRegistration,
) -> Result<transport_sdk::RegisterPresenceResponse> {
    tokio::time::timeout(
        Duration::from_secs(RENDEZVOUS_REGISTRATION_REQUEST_TIMEOUT_SECS),
        client.register_presence(registration),
    )
    .await
    .with_context(|| {
        format!("timed out contacting rendezvous endpoint {url} while registering presence")
    })?
}

async fn enroll_cluster(state: &ServerState, url: &str, key: &str) -> Result<()> {
    let trust_state = state.clone();
    let trust = tokio::task::spawn_blocking(move || load_live_trust_material(&trust_state))
        .await
        .context("failed loading global rendezvous trust material")??;
    // Enrollment requests have their own timeout, separate from presence.
    register_cluster_with_global_rendezvous_services(
        state.cluster_id,
        &[url.to_string()],
        trust.cluster_ca_pem.as_deref(),
        Some(key),
        trust.rendezvous_ca_pem.as_deref(),
    )
    .await
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
    // Existing cluster identities continue working regardless of the node-local
    // legacy switch, and healthy registrations need no repeated ownership proof.
    let presence_error = match publish_presence(url, client, registration).await {
        Ok(response) => {
            if global {
                state
                    .network
                    .global_rendezvous_enrollments
                    .lock()
                    .await
                    .insert(url.to_string(), GlobalEnrollmentState::healthy());
            }
            return Ok(response);
        }
        Err(error) => error,
    };
    if !global || state.network.internal_tls_runtime.is_none() {
        return Err(presence_error);
    }
    // Only a signer can recover an unknown cluster; other nodes keep attempting
    // ordinary presence while a signer restores the shared registry entry.
    let Some(key) = state.network.internal_ca_key_pem.as_deref() else {
        return Err(presence_error);
    };
    {
        let mut enrollments = state.network.global_rendezvous_enrollments.lock().await;
        if enrollments
            .get(url)
            .is_some_and(|entry| entry.next_attempt_at > tokio::time::Instant::now())
        {
            return Err(presence_error);
        }
        // Reserve an attempt without holding the mutex across network requests.
        enrollments
            .entry(url.to_string())
            .or_insert_with(GlobalEnrollmentState::healthy)
            .next_attempt_at =
            tokio::time::Instant::now() + Duration::from_secs(ENROLLMENT_RECOVERY_GRACE_SECS);
    }
    let enrollment = enroll_cluster(state, url, key).await;
    {
        let mut enrollments = state.network.global_rendezvous_enrollments.lock().await;
        let entry = enrollments
            .entry(url.to_string())
            .or_insert_with(GlobalEnrollmentState::healthy);
        if enrollment.is_ok() {
            *entry = GlobalEnrollmentState::healthy();
        } else {
            entry.failed();
        }
    }
    enrollment?;
    publish_presence(url, client, registration).await
}
