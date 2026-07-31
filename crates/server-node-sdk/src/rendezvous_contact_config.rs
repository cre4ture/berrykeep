use super::*;

/// Cluster-owned contact list for clients looking up rendezvous services after
/// they have already established an authenticated cluster connection. Server
/// nodes also add the replicated contacts to their outbound registration set.
///
/// This is intentionally a normal, versioned object. The first implementation
/// therefore inherits normal object metadata synchronization and replication
/// behaviour; dedicated control-plane delivery policy is documented separately.
pub(crate) const RENDEZVOUS_CONTACT_CONFIGURATION_STORAGE_KEY: &str =
    "sys/cluster-config/rendezvous-contacts.json";
const RENDEZVOUS_CONTACT_CONFIGURATION_SCHEMA_VERSION: u32 = 1;
const MAX_RENDEZVOUS_CONTACT_URLS: usize = 32;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct RendezvousContactConfiguration {
    #[serde(default = "default_schema_version")]
    pub(crate) schema_version: u32,
    #[serde(default)]
    pub(crate) rendezvous_urls: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RendezvousContactConfigurationResponse {
    pub(crate) configuration: RendezvousContactConfiguration,
    pub(crate) stored: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) version_id: Option<String>,
}

#[derive(Debug, Clone)]
struct LoadedRendezvousContactConfiguration {
    configuration: RendezvousContactConfiguration,
    stored: bool,
    version_id: Option<String>,
}

fn default_schema_version() -> u32 {
    RENDEZVOUS_CONTACT_CONFIGURATION_SCHEMA_VERSION
}

fn default_configuration() -> RendezvousContactConfiguration {
    RendezvousContactConfiguration {
        schema_version: RENDEZVOUS_CONTACT_CONFIGURATION_SCHEMA_VERSION,
        rendezvous_urls: Vec::new(),
    }
}

pub(crate) async fn public_config(State(state): State<ServerState>) -> impl IntoResponse {
    match load_current_configuration(&state).await {
        Ok(loaded) => (
            StatusCode::OK,
            Json(RendezvousContactConfigurationResponse {
                configuration: loaded.configuration,
                stored: loaded.stored,
                version_id: loaded.version_id,
            }),
        )
            .into_response(),
        Err(err) => {
            warn!(error = %err, "failed loading rendezvous contact configuration");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub(crate) async fn admin_get_config(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let action = "auth/cluster/rendezvous-contacts/get";
    let authz = match authorize_admin_request(&state, &headers, action, true, true, json!({})).await
    {
        Ok(authz) => authz,
        Err(status) => return status.into_response(),
    };

    match load_current_configuration(&state).await {
        Ok(loaded) => {
            append_admin_audit(
                &state,
                action,
                &authz,
                true,
                true,
                true,
                "success",
                json!({
                    "stored": loaded.stored,
                    "version_id": loaded.version_id,
                    "rendezvous_url_count": loaded.configuration.rendezvous_urls.len(),
                }),
            )
            .await;
            (
                StatusCode::OK,
                Json(RendezvousContactConfigurationResponse {
                    configuration: loaded.configuration,
                    stored: loaded.stored,
                    version_id: loaded.version_id,
                }),
            )
                .into_response()
        }
        Err(err) => {
            warn!(error = %err, "failed loading rendezvous contact configuration for admin");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub(crate) async fn admin_put_config(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(configuration): Json<RendezvousContactConfiguration>,
) -> impl IntoResponse {
    let action = "auth/cluster/rendezvous-contacts/put";
    let authz = match authorize_admin_request(
        &state,
        &headers,
        action,
        true,
        true,
        json!({ "rendezvous_url_count": configuration.rendezvous_urls.len() }),
    )
    .await
    {
        Ok(authz) => authz,
        Err(status) => return status.into_response(),
    };

    let configuration = match normalize_configuration(configuration) {
        Ok(configuration) => configuration,
        Err(err) => {
            append_admin_audit(
                &state,
                action,
                &authz,
                true,
                true,
                true,
                "rejected",
                json!({ "error": err.to_string() }),
            )
            .await;
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": err.to_string() })),
            )
                .into_response();
        }
    };

    let payload = match serde_json::to_vec_pretty(&configuration) {
        Ok(payload) => payload,
        Err(err) => {
            warn!(error = %err, "failed encoding rendezvous contact configuration");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let outcome = {
        let mut store = lock_store(&state, "rendezvous_contacts.config.put").await;
        match store
            .put_object_versioned(
                RENDEZVOUS_CONTACT_CONFIGURATION_STORAGE_KEY,
                Bytes::from(payload),
                PutOptions {
                    parent_version_ids: Vec::new(),
                    state: VersionConsistencyState::Confirmed,
                    inherit_preferred_parent: true,
                    create_snapshot: true,
                    explicit_version_id: None,
                },
            )
            .await
        {
            Ok(outcome) => outcome,
            Err(err) => {
                warn!(error = %err, "failed storing rendezvous contact configuration");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        }
    };

    register_cluster_object_put_outcome(
        &state,
        RENDEZVOUS_CONTACT_CONFIGURATION_STORAGE_KEY,
        &outcome.version_id,
    )
    .await;

    if let Err(err) = synchronize_cluster_rendezvous_contact_urls(&state).await {
        warn!(
            error = %err,
            "failed applying updated rendezvous contact configuration to local server registration"
        );
    }

    append_admin_audit(
        &state,
        action,
        &authz,
        true,
        true,
        true,
        "updated",
        json!({
            "storage_key": RENDEZVOUS_CONTACT_CONFIGURATION_STORAGE_KEY,
            "version_id": outcome.version_id,
            "rendezvous_url_count": configuration.rendezvous_urls.len(),
        }),
    )
    .await;

    (
        StatusCode::OK,
        Json(RendezvousContactConfigurationResponse {
            configuration,
            stored: true,
            version_id: Some(outcome.version_id),
        }),
    )
        .into_response()
}

async fn load_current_configuration(
    state: &ServerState,
) -> Result<LoadedRendezvousContactConfiguration> {
    let (payload, version_id) = {
        let store = read_store(state, "rendezvous_contacts.config.get").await;
        let payload = match store
            .get_object(
                RENDEZVOUS_CONTACT_CONFIGURATION_STORAGE_KEY,
                None,
                None,
                ObjectReadMode::ConfirmedOnly,
            )
            .await
        {
            Ok(payload) => Some(payload),
            Err(StoreReadError::NotFound) => None,
            Err(StoreReadError::Corrupt(message)) => {
                bail!("rendezvous contact configuration is corrupt: {message}")
            }
            Err(StoreReadError::Internal(err)) => return Err(err),
        };
        let version_id = store
            .list_versions(RENDEZVOUS_CONTACT_CONFIGURATION_STORAGE_KEY)
            .await?
            .and_then(|graph| graph.preferred_head_version_id);
        (payload, version_id)
    };

    let Some(payload) = payload else {
        return Ok(LoadedRendezvousContactConfiguration {
            configuration: default_configuration(),
            stored: false,
            version_id: None,
        });
    };

    let configuration = serde_json::from_slice::<RendezvousContactConfiguration>(&payload)
        .context("failed parsing rendezvous contact configuration")?;
    Ok(LoadedRendezvousContactConfiguration {
        configuration: normalize_configuration(configuration)?,
        stored: true,
        version_id,
    })
}

pub(crate) async fn load_current_rendezvous_urls(state: &ServerState) -> Result<Vec<String>> {
    Ok(load_current_configuration(state)
        .await?
        .configuration
        .rendezvous_urls)
}

fn normalize_configuration(
    mut configuration: RendezvousContactConfiguration,
) -> Result<RendezvousContactConfiguration> {
    if configuration.schema_version != RENDEZVOUS_CONTACT_CONFIGURATION_SCHEMA_VERSION {
        bail!(
            "unsupported rendezvous contact configuration schema version {}; expected {}",
            configuration.schema_version,
            RENDEZVOUS_CONTACT_CONFIGURATION_SCHEMA_VERSION
        );
    }
    if configuration.rendezvous_urls.len() > MAX_RENDEZVOUS_CONTACT_URLS {
        bail!(
            "rendezvous contact configuration supports at most {MAX_RENDEZVOUS_CONTACT_URLS} URLs"
        );
    }

    configuration.rendezvous_urls = normalize_rendezvous_url_list(&configuration.rendezvous_urls)?;
    Ok(configuration)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_duplicate_urls_without_reordering_contacts() {
        let configuration = normalize_configuration(RendezvousContactConfiguration {
            schema_version: RENDEZVOUS_CONTACT_CONFIGURATION_SCHEMA_VERSION,
            rendezvous_urls: vec![
                "https://relay.example:19080".to_string(),
                "https://relay.example:19080".to_string(),
                "https://fallback.example:19080".to_string(),
            ],
        })
        .expect("configuration should be valid");

        assert_eq!(
            configuration.rendezvous_urls,
            vec![
                "https://relay.example:19080/".to_string(),
                "https://fallback.example:19080/".to_string(),
            ]
        );
    }

    #[test]
    fn rejects_unknown_schema_version() {
        let err = normalize_configuration(RendezvousContactConfiguration {
            schema_version: RENDEZVOUS_CONTACT_CONFIGURATION_SCHEMA_VERSION + 1,
            rendezvous_urls: Vec::new(),
        })
        .expect_err("unknown schema version must be rejected");

        assert!(err.to_string().contains("unsupported"));
    }
}
